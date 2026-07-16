//! The OpenCode review host (RFC-0009 / ADR-0094 review cutover, slice 3): the thin transport shell
//! over the proven `lci_review_agent::opencode` core.
//!
//! All the review *logic* — reconstructing a turn from the recorder, the reused coverage/refute gates,
//! the drive loop, the transcript — lives in `lci-review-agent` and is unit-tested there. This host
//! supplies only the I/O the loop needs: render the per-task config, write it + the env, spawn
//! `opencode acp`, and implement [`ReviewSession`] by driving one `session/prompt` cycle and tailing
//! the recorder file for the events it produced. It returns the same [`ReviewOutcome`] the native host
//! does, so `finalize_review_outcome` is untouched at the cutover.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use uuid::Uuid;

use lci_acp_host::{AcpClient, PermissionPolicy};
use lci_agent_clients::{ControlPlaneClient, TranscriptEntry};
use lci_review_agent::opencode::{
    RecorderEvent, ReviewDriver, ReviewGates, ReviewResolution, ReviewSession, parse_recorder,
    render_review_config, run_review_loop, transcript_from_recorder,
};
use lci_review_agent::prompt::{self, PrDiffRef, PromptConfig};

use super::ReviewOutcome;
use crate::bootstrap::config::ReviewConfig;
use crate::clone::PrDiff;

/// The supervisor re-prompt budget: how many `session/prompt` cycles the host will drive before
/// declaring exhaustion. A cycle is a whole OpenCode agent run (many model turns), so this is small —
/// it bounds gate bounces + keep-going nudges, not model turns (those are OpenCode's own budget).
const MAX_REVIEW_CYCLES: usize = 8;

/// Ceiling on how much of the recorder file to read into memory (gemini #447). The recorder logs tool
/// RESULTS — including `read_file` output of arbitrary repo files — so a pathological repo could bloat
/// it; cap the read so it can't OOM the runner. 32 MiB is far above any real review's recorder.
const RECORDER_READ_CAP: u64 = 32 * 1024 * 1024;

/// Read at most [`RECORDER_READ_CAP`] bytes of the recorder file, lossily decoded (a truncated tail /
/// invalid UTF-8 just yields fewer parseable lines, never a panic). Missing file → empty.
async fn read_recorder_capped(path: &Path) -> String {
    use tokio::io::AsyncReadExt;
    let Ok(file) = tokio::fs::File::open(path).await else {
        return String::new();
    };
    let mut buffer = Vec::new();
    let _ = file.take(RECORDER_READ_CAP).read_to_end(&mut buffer).await;
    String::from_utf8_lossy(&buffer).into_owned()
}

/// A live OpenCode review session: one `ReviewSession::prompt` drives a single `session/prompt` cycle
/// over ACP and returns the recorder events (ADR-0095) that cycle appended — the completeness
/// authority the gates read (subagent-internal tool calls included).
pub struct OpencodeReviewSession {
    client: AcpClient,
    session_id: String,
    recorder_path: PathBuf,
    /// How many recorder events have already been returned, so each cycle yields only its own delta.
    consumed: usize,
}

impl OpencodeReviewSession {
    #[must_use]
    pub fn new(client: AcpClient, session_id: String, recorder_path: PathBuf) -> Self {
        Self {
            client,
            session_id,
            recorder_path,
            consumed: 0,
        }
    }

    /// Terminate the opencode child.
    pub async fn shutdown(self) -> Result<()> {
        self.client.shutdown().await
    }
}

impl ReviewSession for OpencodeReviewSession {
    async fn prompt(&mut self, text: &str) -> Result<Vec<RecorderEvent>> {
        self.client
            .prompt(&self.session_id, text)
            .await
            .context("opencode session/prompt")?;
        // The recorder appends over the whole run; return only the events new since the last cycle.
        let content = read_recorder_capped(&self.recorder_path).await;
        let mut all = parse_recorder(&content);
        let delta = if self.consumed < all.len() {
            all.split_off(self.consumed)
        } else {
            Vec::new()
        };
        self.consumed += delta.len();
        Ok(delta)
    }
}

/// The env the review MCP server (`lci-review-mcp`) resolves its `LCI_MCP_*` from — control-plane URL +
/// runner token + task + checkout + embeddings. Grouped so the host's long spawn call reads cleanly.
pub struct McpEnv<'a> {
    pub control_plane_url: &'a str,
    pub runner_token: &'a str,
    pub checkout_root: &'a Path,
    pub embed_url: &'a str,
    pub embed_key: &'a str,
    pub embed_model: &'a str,
}

/// Run one review on OpenCode. Renders the per-task config, spawns `opencode acp`, drives the review
/// to resolution over the reused gates, reconstructs the ADR-0034 transcript from the recorder, and
/// maps the result onto a [`ReviewOutcome`] (posting any coverage disclosure). Only a transport/loop
/// failure returns `Err`; the caller finalizes on all three outcomes exactly as for the native host.
#[allow(clippy::too_many_arguments)]
pub async fn run_opencode_agent(
    review: &ReviewConfig,
    command: &str,
    diff: Option<&PrDiff>,
    repo_instructions: Option<&str>,
    prior_reviews: Option<&str>,
    repo_memory: Option<&str>,
    // Per-project billing attribution headers (epic #89) — forwarded to the eaig provider so
    // OpenCode-hosted review bills the same as native.
    attribution: &[(String, String)],
    mcp_env: &McpEnv<'_>,
    task_id: Uuid,
    client: &ControlPlaneClient,
    transcript: &mut Vec<TranscriptEntry>,
) -> Result<ReviewOutcome> {
    // ── Prompt (reuse the native builder) ───────────────────────────────────────────────────────
    let prompt_config = PromptConfig {
        system_prompt: review.system_prompt.clone(),
        max_diff_chars: review.max_diff_chars,
        context_window: review.context_window,
    };
    let diff_ref = diff.map(|pr| PrDiffRef {
        diff: &pr.diff,
        files: &pr.files,
    });
    let messages = prompt::build_messages(
        &prompt_config,
        command,
        diff_ref,
        repo_instructions,
        prior_reviews,
        repo_memory,
    );
    // The system message is OpenCode's agent `prompt`; the user message(s) are the `session/prompt`.
    let system_prompt = messages
        .iter()
        .find(|message| message.role == "system")
        .and_then(|message| message.content.clone())
        .unwrap_or_default();
    let user_prompt = messages
        .iter()
        .filter(|message| message.role == "user")
        .filter_map(|message| message.content.clone())
        .collect::<Vec<_>>()
        .join("\n\n");

    // ── Render + write the config, and pick the recorder path ───────────────────────────────────
    let config = render_review_config(&system_prompt, review.fast, review.temperature, attribution);
    let workdir = std::env::temp_dir().join(format!("lci-opencode-review-{task_id}"));
    tokio::fs::create_dir_all(&workdir)
        .await
        .context("creating the opencode review workdir")?;
    let config_path = workdir.join("opencode.review.json");
    tokio::fs::write(
        &config_path,
        serde_json::to_vec_pretty(&config).context("serializing opencode config")?,
    )
    .await
    .context("writing opencode config")?;
    let recorder_path = workdir.join("recording.jsonl");

    // ── Env for the opencode child (config placeholders + recorder + the review MCP server) ──────
    let env: Vec<(String, String)> = vec![
        ("OPENCODE_CONFIG".into(), config_path.display().to_string()),
        ("OPENCODE_DISABLE_AUTOUPDATE".into(), "1".into()),
        ("OPENCODE_DISABLE_MODELS_FETCH".into(), "1".into()),
        (
            "LCI_RECORDER_PATH".into(),
            recorder_path.display().to_string(),
        ),
        // Provider (`{env:LCI_EAIG_*}` in the config).
        ("LCI_EAIG_BASE_URL".into(), review.base_url.clone()),
        ("LCI_EAIG_API_KEY".into(), review.api_key.clone()),
        ("LCI_EAIG_MODEL".into(), review.model.clone()),
        // The mediated review MCP server (`lci-review-mcp`) resolves these.
        (
            "LCI_MCP_CP_URL".into(),
            mcp_env.control_plane_url.to_string(),
        ),
        (
            "LCI_MCP_RUNNER_TOKEN".into(),
            mcp_env.runner_token.to_string(),
        ),
        ("LCI_MCP_TASK_ID".into(), task_id.to_string()),
        (
            "LCI_MCP_CHECKOUT".into(),
            mcp_env.checkout_root.display().to_string(),
        ),
        ("LCI_MCP_EMBED_URL".into(), mcp_env.embed_url.to_string()),
        ("LCI_MCP_EMBED_KEY".into(), mcp_env.embed_key.to_string()),
        (
            "LCI_MCP_EMBED_MODEL".into(),
            mcp_env.embed_model.to_string(),
        ),
        // Gate-interlock backstop: the terminal tool is `finish`; the supervisor-side gates are
        // authoritative, so no hard required tool is imposed in-process.
        ("LCI_GATE_TERMINAL_TOOL".into(), "lightbridge_finish".into()),
        ("LCI_GATE_REQUIRED_TOOLS".into(), String::new()),
    ];

    // ── Spawn + handshake ───────────────────────────────────────────────────────────────────────
    let bin = std::env::var("OPENCODE_BIN").unwrap_or_else(|_| "opencode".to_string());
    // Deny every permission request: review is read-only, so edit/bash/webfetch are already denied in
    // the config and should never be asked — a cancel is the safe answer if one somehow arrives.
    let client_acp = AcpClient::spawn(&bin, mcp_env.checkout_root, PermissionPolicy::Cancel, &env)
        .await
        .context("spawning opencode acp")?;
    client_acp
        .initialize()
        .await
        .context("opencode initialize")?;
    // `mcpServers` here must be a JSON ARRAY (opencode rejects an object); the stdio review MCP is
    // wired via the config `mcp` block, not here, so this is empty — caught by the real-opencode e2e.
    let session_id = client_acp
        .new_session(
            &mcp_env.checkout_root.to_string_lossy(),
            serde_json::json!([]),
        )
        .await
        .context("opencode session/new")?;

    // ── Drive the review to resolution ──────────────────────────────────────────────────────────
    let diff_files = diff.map(|pr| pr.files.clone()).unwrap_or_default();
    let gates = ReviewGates::new(
        diff_files,
        review.max_coverage_bounces,
        review.max_turns,
        review.fast,
    );
    let mut driver = ReviewDriver::new(gates, MAX_REVIEW_CYCLES);
    let mut session = OpencodeReviewSession::new(client_acp, session_id, recorder_path.clone());
    let resolution = run_review_loop(&mut session, &mut driver, &user_prompt).await;

    // ── Transcript from the full recorder file (ADR-0034), regardless of how the loop ended ──────
    let recorder_content = read_recorder_capped(&recorder_path).await;
    transcript.extend(transcript_from_recorder(
        &parse_recorder(&recorder_content),
        &review.model,
    ));

    // Reap opencode before returning either way.
    if let Err(error) = session.shutdown().await {
        tracing::warn!(%error, "shutting down opencode acp failed (non-fatal)");
    }

    // ── Map the resolution onto a ReviewOutcome (+ post any coverage disclosure) ─────────────────
    Ok(
        match resolution.context("driving the opencode review loop")? {
            ReviewResolution::Finished { disclosure } => {
                post_disclosure(client, task_id, disclosure).await;
                ReviewOutcome::Finished
            }
            ReviewResolution::Exhausted { disclosure } => {
                post_disclosure(client, task_id, disclosure).await;
                ReviewOutcome::Exhausted
            }
            ReviewResolution::Aborted(reason) => ReviewOutcome::Aborted(reason),
        },
    )
}

/// Best-effort post of the coverage disclosure note (ADR-0069 / #306) as the review summary — a failed
/// re-post keeps the model's own summary rather than failing a finished run.
async fn post_disclosure(client: &ControlPlaneClient, task_id: Uuid, disclosure: Option<String>) {
    if let Some(note) = disclosure
        && let Err(error) = client.set_review_summary(task_id, &note).await
    {
        tracing::warn!(%error, task_id = %task_id, "coverage disclosure re-post failed (non-fatal)");
    }
}

/// End-to-end proof that the host drives a REAL `opencode acp` (RFC-0009 slice 3 / slice 4).
///
/// This exercises the actual wire boundary the unit tests can't: one `session/prompt` runs opencode's
/// whole internal cycle, the recorder captures the mediated tool calls, `OpencodeReviewSession` tails
/// the file for that cycle's events, and `run_review_loop` drives the reused coverage gate to a clean
/// `Finished`. It uses the node mock provider + mock review MCP (no eaig, no control plane) under
/// `integrations/opencode/sim`. Skipped (not failed) when `opencode`/`node` aren't on PATH, so CI —
/// which has neither — stays green; run it locally to prove the host.
#[cfg(test)]
mod e2e {
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::time::Duration;

    use lci_acp_host::{AcpClient, PermissionPolicy};
    use lci_review_agent::opencode::{
        ReviewDriver, ReviewGates, ReviewResolution, run_review_loop,
    };

    use super::OpencodeReviewSession;

    fn on_path(bin: &str) -> bool {
        std::process::Command::new(bin)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    }

    fn repo_root() -> PathBuf {
        // services/agent-runner -> services -> <repo root>
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("repo root above services/agent-runner")
            .to_path_buf()
    }

    #[tokio::test]
    async fn drives_a_real_opencode_review_to_finished() {
        if !on_path("opencode") || !on_path("node") {
            eprintln!("SKIP: opencode/node not on PATH — this e2e proof needs both");
            return;
        }
        let root = repo_root();
        let sim = root.join("integrations/opencode/sim");
        let plugins = root.join("integrations/opencode/plugins");
        let port = 8917u16;

        // Isolated workdir for the config + recorder.
        let workdir = std::env::temp_dir().join("lci-opencode-review-e2e");
        let _ = std::fs::remove_dir_all(&workdir);
        std::fs::create_dir_all(&workdir).unwrap();
        // Isolate opencode from the machine's GLOBAL config (~/.config/opencode) so the test uses ONLY
        // our OPENCODE_CONFIG (the sim provider), not the user's real eaig gateway — otherwise the run
        // silently bills a real model. A fresh empty HOME/XDG makes opencode find no global config.
        let fake_home = workdir.join("home");
        let xdg = workdir.join("xdg");
        std::fs::create_dir_all(&fake_home).unwrap();
        std::fs::create_dir_all(&xdg).unwrap();
        let recorder_path = workdir.join("recording.jsonl");
        let config_path = workdir.join("opencode.json");
        let tools_log = workdir.join("tools.log");

        // Config: sim provider -> node mock; stdio MCP -> node review mock; the real plugins.
        let plugin = |name: &str| {
            plugins
                .join(name)
                .join("src/index.ts")
                .display()
                .to_string()
        };
        let config = serde_json::json!({
            "$schema": "https://opencode.ai/config.json",
            "model": "sim/sim-model",
            "provider": { "sim": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "Sim",
                "options": { "baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "sim" },
                "models": { "sim-model": { "name": "Sim" } }
            }},
            "plugin": [plugin("recorder"), plugin("gate-interlock"), plugin("logger")],
            "mcp": { "lightbridge": {
                "type": "local",
                "command": ["node", sim.join("review-mock-mcp.mjs").display().to_string()],
                "enabled": true
            }},
            // TOP-LEVEL tool disables (agent-independent) — the per-agent block was ignored because
            // opencode runs its default `build` agent, not a custom one.
            "tools": { "read": false, "grep": false, "glob": false, "list": false,
                       "edit": false, "write": false, "patch": false, "bash": false,
                       "webfetch": false, "websearch": false, "task": false, "todowrite": false,
                       "skill": false },
            "agent": { "review": {
                "mode": "primary",
                "description": "Review the change via the mediated tools; read-only.",
                "prompt": "Review the changed file a.rs. Read it, record findings with add_review_comment, then call finish."
            }}
        });
        std::fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();

        // Start the mock provider.
        let mut provider = tokio::process::Command::new("node")
            .arg(sim.join("review-mock-provider.mjs"))
            .env("LCI_SIM_PROVIDER_PORT", port.to_string())
            .env("LCI_SIM_TOOLS_LOG", tools_log.display().to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn mock provider");
        tokio::time::sleep(Duration::from_millis(1200)).await;

        let env = vec![
            (
                "OPENCODE_CONFIG".to_string(),
                config_path.display().to_string(),
            ),
            ("OPENCODE_DISABLE_AUTOUPDATE".to_string(), "1".to_string()),
            ("OPENCODE_DISABLE_MODELS_FETCH".to_string(), "1".to_string()),
            // Isolate from any global opencode config so the sim provider is the only one.
            ("HOME".to_string(), fake_home.display().to_string()),
            ("XDG_CONFIG_HOME".to_string(), xdg.display().to_string()),
            ("XDG_DATA_HOME".to_string(), xdg.display().to_string()),
            ("XDG_CACHE_HOME".to_string(), xdg.display().to_string()),
            (
                "LCI_RECORDER_PATH".to_string(),
                recorder_path.display().to_string(),
            ),
            (
                "LCI_GATE_TERMINAL_TOOL".to_string(),
                "lightbridge_finish".to_string(),
            ),
            ("LCI_GATE_REQUIRED_TOOLS".to_string(), String::new()),
        ];

        let acp = AcpClient::spawn("opencode", &workdir, PermissionPolicy::Cancel, &env)
            .await
            .expect("spawn opencode acp");
        acp.initialize().await.expect("initialize");
        let session_id = acp
            .new_session(&workdir.to_string_lossy(), serde_json::json!([]))
            .await
            .expect("session/new");

        let gates = ReviewGates::new(vec!["a.rs".to_string()], 3, 40, false);
        let mut driver = ReviewDriver::new(gates, 8);
        let mut session = OpencodeReviewSession::new(acp, session_id, recorder_path.clone());

        let resolution = run_review_loop(
            &mut session,
            &mut driver,
            "Review the change to a.rs across correctness, security, and quality.",
        )
        .await;

        let _ = session.shutdown().await;
        let _ = provider.kill().await;

        let recorder = std::fs::read_to_string(&recorder_path).unwrap_or_default();
        let resolution = resolution.unwrap_or_else(|error| {
            panic!("review loop errored: {error}\n--- recorder ---\n{recorder}")
        });

        // Coverage-parity guard (agent-selection fix): opencode must advertise ONLY the mediated tools
        // to the model — no built-in read/grep/glob/bash/edit that could let it investigate off the
        // mediated path and escape the recorder-driven coverage accounting. This asserts the top-level
        // `tools` disable actually took effect against the real binary (a per-agent block did NOT).
        let advertised = std::fs::read_to_string(&tools_log).unwrap_or_default();
        let mut all_tools: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for line in advertised.lines() {
            if let Ok(names) = serde_json::from_str::<Vec<String>>(line) {
                all_tools.extend(names);
            }
        }
        for builtin in ["read", "grep", "glob", "bash", "edit", "write", "task"] {
            assert!(
                !all_tools.contains(builtin),
                "built-in `{builtin}` was advertised to the model — coverage guard broken. \
                 Advertised: {all_tools:?}"
            );
        }
        assert!(
            all_tools.contains("lightbridge_read_file"),
            "the mediated read_file was not advertised: {all_tools:?}"
        );

        // The reused coverage gate accepted the finish because a.rs was read; the real opencode drove
        // read_file -> add_review_comment -> finish through the mediated MCP, and the recorder captured
        // it (which is how the gate saw the read at all).
        assert!(
            matches!(resolution, ReviewResolution::Finished { .. }),
            "expected Finished, got {resolution:?}\n--- recorder ---\n{recorder}"
        );
        assert!(
            recorder.contains("read_file"),
            "recorder missing read_file:\n{recorder}"
        );
        assert!(
            recorder.contains("recorded finding"),
            "recorder missing the add_review_comment result:\n{recorder}"
        );
        assert!(
            recorder.contains("finish"),
            "recorder missing finish:\n{recorder}"
        );
    }
}
