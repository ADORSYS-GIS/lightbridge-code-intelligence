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
// use lci_agent_clients::ControlPlaneClient;  // ← Not used (coverage disclosure no longer posted)
use lci_agent_sast::{ENV_CHANGED_FILES, SastConfig};
use lci_review_agent::opencode::{
    REVIEW_PROMPT_FILE, RecorderEvent, ReviewDriver, ReviewGates, ReviewResolution, ReviewSession,
    parse_recorder, render_review_config, run_review_loop,
};
use lci_review_agent::prompt::{self, PrDiffRef, PromptConfig};

use super::ReviewOutcome;
use crate::bootstrap::config::ReviewConfig;
use crate::clone::PrDiff;

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
    /// The repo's `severity.min` (ADR-0030), when declared — the `add_review_comment` tool inside
    /// `lci-review-mcp` skips recording a finding below this priority. `None` = no filter (record
    /// everything). Threaded as an env var (`LCI_MCP_MIN_PRIORITY`) since the MCP server is a separate
    /// process, not a function call.
    pub min_priority: Option<&'a str>,
    /// The task's GitHub App installation token (ADR-0072), reused verbatim from the same token
    /// `TaskContext` already carries for the authenticated clone URL — story #498 mints nothing new.
    /// `None` for a non-GitHub platform (GitLab/Bitbucket embed credentials directly in `clone_url`
    /// instead, per `authenticated_clone_url`'s own platform detection) or when the task context
    /// carries no token. GitHub MCP is only ever spawned when this is `Some` AND the preset's
    /// `review.tools` explicitly lists a `mcp__github__…` selector (ADR-0105) — see
    /// `tool_surface::github_mcp_explicitly_listed`.
    pub github_token: Option<&'a str>,
}

/// Run one review on OpenCode. Renders the per-task config, spawns `opencode acp`, drives the review
/// to resolution over the reused gates, and maps the result onto a [`ReviewOutcome`] (posting any
/// coverage disclosure). Run observability is Loki-only (epic #459) — the logger plugin emits the
/// model's parts/tool calls to stderr → Loki; there is no DB transcript. Only a transport/loop
/// failure returns `Err`; the caller finalizes on all three outcomes exactly as for the native host.
#[allow(clippy::too_many_arguments)]
pub async fn run_opencode_agent(
    review: &ReviewConfig,
    command: &str,
    diff: Option<&PrDiff>,
    repo_instructions: Option<&str>,
    prior_reviews: Option<&str>,
    repo_memory: Option<&str>,
    repo_config_context: Option<&str>,
    // The resolved SAST config (ADR-0073), forwarded to the `run_sast` tool that runs inside
    // `lci-review-mcp`. `None` when SAST is off; even when set, the tool is only offered if it also
    // clears the diff-present + per-tier-allowlist gate (see below) — same rule as the native surface.
    sast_config: Option<&SastConfig>,
    // Per-project billing attribution headers (epic #89) — forwarded to the eaig provider so
    // OpenCode-hosted review bills the same as native.
    attribution: &[(String, String)],
    mcp_env: &McpEnv<'_>,
    task_id: Uuid,
    // client: &ControlPlaneClient,  // ← Not used (coverage disclosure no longer posted)
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
        repo_config_context,
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

    // ── Render the config (base + injection + operator overlay), and pick the workdir ────────────
    // ADR-0099/0103: the base is the checked-in `review.jsonc`; the supervisor injects the
    // reasoning/headers and deep-merges the trusted operator overlay (`review.opencode`) with full
    // override. There is no structural tier flag any more — the reasoning-capable model turns on purely
    // from `review.extra` carrying `reasoning_effort`. A relaxation of the coverage/read-only floor is
    // WARNED (and disclosed below), never blocked.
    let rendered = render_review_config(
        review.temperature,
        // The preset's provider-passthrough params (`review.extra` — `reasoning_effort:"high"` for a
        // reasoning preset, ADR-0069). The native path merges this same map into the chat body; the
        // OpenCode path merges it into the reviewer model's `options` so eaig receives the SAME keys
        // (native-path parity).
        &review.extra,
        attribution,
        review.opencode_overlay.as_ref(),
    );
    for breach in &rendered.floor_breaches {
        tracing::warn!(
            task_id = %task_id,
            relaxation = %breach.message,
            "operator OpenCode overlay (review.opencode) relaxed a review floor invariant (ADR-0099)"
        );
    }
    let floor_disclosure = rendered.disclosure_note();
    let mut config = rendered.config;

    // ── GitHub MCP (ADR-0105, story #498) ───────────────────────────────────────────────────────
    // Opt-in per preset (`review.tools` must explicitly list a `mcp__github__…` selector) AND only for
    // a GitHub-platformed task (`github_token: Some`, the same installation token already minted for
    // the clone URL — no new credential). Registered as a SECOND `local` (stdio) MCP server alongside
    // `lightbridge`, not proxied through it — unlike the ADR-0066 knowledge-tools registry (shared,
    // pre-deployed, no per-task credential), this genuinely needs a fresh per-task secret in its own
    // subprocess env, which the shared `lci-review-mcp` proxy shape can't carry.
    let github_mcp_offered =
        super::tool_surface::github_mcp_explicitly_listed(review) && mcp_env.github_token.is_some();
    if github_mcp_offered {
        config["mcp"]["github"] = serde_json::json!({
            "type": "local",
            // `--read-only` is the upstream github-mcp-server flag restricting it to non-mutating
            // operations — review must never get a write-capable GitHub MCP. Pin the exact binary
            // version in the runner image build (verify the `stdio --read-only` invocation shape
            // against that pinned release before rollout).
            "command": ["github-mcp-server", "stdio", "--read-only"],
            "enabled": true,
        });
    }

    let workdir = std::env::temp_dir().join(format!("lci-opencode-review-{task_id}"));
    tokio::fs::create_dir_all(&workdir)
        .await
        .context("creating the opencode review workdir")?;
    // The reviewer prompt rides a `{file:REVIEW_PROMPT_FILE}` reference in the config (ADR-0099); write
    // its per-task content BESIDE the config so opencode resolves the reference (config-dir-relative).
    tokio::fs::write(workdir.join(REVIEW_PROMPT_FILE), &system_prompt)
        .await
        .context("writing the opencode review prompt file")?;
    // ⚠️ CONFIG ISOLATION (security): opencode MERGES config from its cwd's `opencode.json` and from
    // the global HOME/XDG config over our `OPENCODE_CONFIG`. The checkout is UNTRUSTED (a PR from a
    // fork could ship an `opencode.json` that re-enables built-in tools / bash, injects an MCP server
    // that runs commands, or swaps the model — verified via `opencode debug config`). So opencode runs
    // with a NEUTRAL cwd (this workdir, never the checkout) and an EMPTY HOME/XDG — its ONLY config is
    // ours. File reads still reach the checkout via `lci-review-mcp` (LCI_MCP_CHECKOUT), so opencode
    // never needs it as cwd. The config file is `opencode.review.json` (via OPENCODE_CONFIG), NOT
    // `opencode.json`, so it isn't itself auto-loaded as a project config.
    let fake_home = workdir.join("home");
    let xdg_dir = workdir.join("xdg");
    tokio::fs::create_dir_all(&fake_home)
        .await
        .context("creating the isolated opencode HOME")?;
    tokio::fs::create_dir_all(&xdg_dir)
        .await
        .context("creating the isolated opencode XDG dir")?;
    let config_path = workdir.join("opencode.review.json");
    tokio::fs::write(
        &config_path,
        serde_json::to_vec_pretty(&config).context("serializing opencode config")?,
    )
    .await
    .context("writing opencode config")?;
    let recorder_path = workdir.join("recording.jsonl");
    let sentinel_marker_path = workdir.join("sentinel.marker.json");

    // ── Env for the opencode child (config placeholders + recorder + the review MCP server) ──────
    let mut env: Vec<(String, String)> = vec![
        ("OPENCODE_CONFIG".into(), config_path.display().to_string()),
        ("OPENCODE_DISABLE_AUTOUPDATE".into(), "1".into()),
        ("OPENCODE_DISABLE_MODELS_FETCH".into(), "1".into()),
        // Config isolation (see above): empty HOME/XDG so no global opencode config merges in.
        ("HOME".into(), fake_home.display().to_string()),
        ("XDG_CONFIG_HOME".into(), xdg_dir.display().to_string()),
        ("XDG_DATA_HOME".into(), xdg_dir.display().to_string()),
        ("XDG_CACHE_HOME".into(), xdg_dir.display().to_string()),
        (
            "LCI_RECORDER_PATH".into(),
            recorder_path.display().to_string(),
        ),
        // The sentinel plugin's marker file (ADR-0106, story #499) — read after shutdown, below.
        (
            "LCI_SENTINEL_MARKER_PATH".into(),
            sentinel_marker_path.display().to_string(),
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
    // Repo `severity.min` (ADR-0030): tells `lci-review-mcp`'s `add_review_comment` tool to skip
    // recording a finding below this priority. Absent when the repo declared no severity filter.
    if let Some(min_priority) = mcp_env.min_priority {
        env.push(("LCI_MCP_MIN_PRIORITY".into(), min_priority.to_string()));
    }
    // ⚠️ Internal-CA trust for opencode's HTTPS to the eaig gateway. The native Rust clients
    // `add_root_certificate` the mounted CA (via EMBEDDINGS_CA_CERT, ADR-0018); opencode (bun) does
    // NOT read that — without NODE_EXTRA_CA_CERTS its session/prompt fails "unable to verify the first
    // certificate" and the whole review errors (observed in prod on the cutover). Bun honors
    // NODE_EXTRA_CA_CERTS; point it at the same mounted CA. (lci-review-mcp is Rust and inherits
    // EMBEDDINGS_CA_CERT from this same env, so its embeddings/CP TLS is already covered.)
    if let Ok(ca_path) = std::env::var("EMBEDDINGS_CA_CERT") {
        env.push(("NODE_EXTRA_CA_CERTS".into(), ca_path));
    }
    // GitHub MCP credential (ADR-0105, story #498): set on the WHOLE opencode process env, the same way
    // `lci-review-mcp` gets its `LCI_MCP_*` vars — a `type: "local"` MCP server is a child of opencode
    // and inherits its parent's env, and this codebase's own `review.jsonc` comment confirms that's the
    // verified mechanism opencode-over-ACP actually honors for stdio MCP servers (no per-server
    // `environment` config key is used anywhere else in this repo's OpenCode config). `lci-review-mcp`
    // itself never reads this var, so its presence in the shared env is harmless there.
    if let (true, Some(token)) = (github_mcp_offered, mcp_env.github_token) {
        env.push(("GITHUB_PERSONAL_ACCESS_TOKEN".into(), token.to_string()));
    }

    // ── SAST opt-in wiring for the review MCP (ADR-0073 / ADR-0097) ───────────────────────────────
    // `run_sast` runs INSIDE `lci-review-mcp` (a separate process). Offer it there iff it clears the
    // exact same gate the native surface applies (`tool_surface::resolve_offered_tools`): SAST enabled
    // (a resolved config) + a diff to scope a scan to + an explicit `run_sast` allowlist entry for this
    // tier. When offered, hand the MCP the resolved config (the `SastConfig` env round-trip) and the
    // changed-file scan scope (written to a file — the same `SastToolConfig::changed_files` native
    // passes, and the widen-guard). The presence of these env vars is the MCP's "offer run_sast" signal;
    // their absence leaves the tool unregistered. The supervisor-side `SastAnchorGate` is already
    // composed in `ReviewGates` and recovers its leads from the tool's result digest.
    let diff_present = diff.is_some();
    let sast_offered = sast_config.is_some()
        && diff_present
        && super::tool_surface::sast_explicitly_listed(review);
    if let (true, Some(sast_config)) = (sast_offered, sast_config) {
        let changed_files = diff.map(|pr| pr.files.clone()).unwrap_or_default();
        let list_path = workdir.join("sast-changed-files.txt");
        tokio::fs::write(&list_path, changed_files.join("\n"))
            .await
            .context("writing the SAST changed-file list for lci-review-mcp")?;
        env.extend(sast_config.to_env_pairs());
        env.push((
            ENV_CHANGED_FILES.to_string(),
            list_path.display().to_string(),
        ));
    }

    // ── Spawn + handshake ───────────────────────────────────────────────────────────────────────
    let bin = std::env::var("OPENCODE_BIN").unwrap_or_else(|_| "opencode".to_string());
    // Deny every permission request: review is read-only, so edit/bash/webfetch are already denied in
    // the config and should never be asked — a cancel is the safe answer if one somehow arrives.
    // cwd = the neutral workdir, NOT the untrusted checkout (config-isolation, see above).
    // Startup-cost measurement (ADR-0105's explicit NFR, story #498): timed + logged with whether
    // GitHub MCP was offered this run, so an operator can compare the two populations in Loki before
    // deciding to enable it broadly — this codebase has no `metrics` crate dependency in agent-runner
    // (only control-plane does), so a `tracing` span is the measurement, not a histogram.
    let spawn_started = std::time::Instant::now();
    let client_acp = AcpClient::spawn(&bin, &workdir, PermissionPolicy::Cancel, &env)
        .await
        .context("spawning opencode acp")?;
    client_acp
        .initialize()
        .await
        .context("opencode initialize")?;
    tracing::info!(
        task_id = %task_id,
        github_mcp_offered,
        elapsed_ms = spawn_started.elapsed().as_millis() as u64,
        "opencode spawn+initialize complete"
    );
    // `mcpServers` here must be a JSON ARRAY (opencode rejects an object); the stdio review MCP is
    // wired via the config `mcp` block, not here, so this is empty — caught by the real-opencode e2e.
    let session_id = client_acp
        .new_session(
            // Session cwd = the neutral workdir too, NOT the untrusted checkout (config-isolation).
            &workdir.to_string_lossy(),
            serde_json::json!([]),
        )
        .await
        .context("opencode session/new")?;

    // ── Drive the review to resolution ──────────────────────────────────────────────────────────
    let diff_files = diff.map(|pr| pr.files.clone()).unwrap_or_default();
    let gates = ReviewGates::new(diff_files, review.max_coverage_bounces, review.max_turns);
    // The Rust-side re-prompt ceiling (fast-tier-parity plan): opencode's own `maxSteps` doesn't cap
    // anything over ACP (see the `e2e` module below), so `review.max_cycles` — tier-configurable, fast
    // smaller than deep — is what actually stops a stuck/adversarial model.
    let mut driver = ReviewDriver::new(gates, review.max_cycles);
    let mut session = OpencodeReviewSession::new(client_acp, session_id, recorder_path.clone());
    let resolution = run_review_loop(&mut session, &mut driver, &user_prompt).await;

    // Reap opencode before returning either way.
    if let Err(error) = session.shutdown().await {
        tracing::warn!(%error, "shutting down opencode acp failed (non-fatal)");
    }

    // ── Sentinel marker (ADR-0106, story #499) ──────────────────────────────────────────────────
    // Read AFTER shutdown so the sentinel's `process.on("exit", …)` handler has already run (it fires
    // on the child process's own exit, which `session.shutdown()` triggers). `provider_error`/
    // `uncaught_exception` are a genuine anomaly regardless of how the loop resolved — logged as a
    // warning even on a clean Finished/Exhausted/Aborted outcome. `exit_without_terminal` is NOT
    // inherently alarming (a normal budget-exhausted review also never calls finish/abort — see the
    // plugin's own doc comment) — it's only folded into the reported error below, on the `Err` branch,
    // where it adds real diagnostic value (WHY did the loop never reach a resolution at all).
    let sentinel = read_sentinel_marker(&sentinel_marker_path).await;
    if let Some(event) = &sentinel
        && event.fatal_kind != SentinelFatalKind::ExitWithoutTerminal
    {
        tracing::warn!(
            task_id = %task_id,
            fatal_kind = ?event.fatal_kind,
            message = %event.message,
            last_tool_call = ?event.last_tool_call,
            "opencode sentinel observed a fatal-shaped event (ADR-0106)"
        );
    }
    let resolution = resolution.map_err(|error| match &sentinel {
        Some(event) => error.context(format!(
            "opencode sentinel: {:?} — {} (last tool call: {})",
            event.fatal_kind,
            event.message,
            event.last_tool_call.as_deref().unwrap_or("none")
        )),
        None => error,
    });

    // ── Map the resolution onto a ReviewOutcome (+ post any coverage disclosure) ─────────────────
    // The coverage disclosure (ADR-0069) is augmented with the ADR-0099 floor note when the operator
    // overlay relaxed an invariant, so findings produced under a custom config aren't read as default.
    // NOTE: The disclosure is now logged to runner logs (via tracing::warn) but NOT posted to the
    // control plane to avoid cluttering review comments. This maintains transparency for developers
    // while keeping end-user-facing reviews clean.
    Ok(
        match resolution.context("driving the opencode review loop")? {
            ReviewResolution::Finished { disclosure } => {
                let disclosure = merge_disclosure(disclosure, floor_disclosure);
                // Log the disclosure for transparency (not posted to control plane)
                if let Some(note) = &disclosure {
                    tracing::warn!(%task_id, "coverage disclosure (not posted): {}", note);
                }
                ReviewOutcome::Finished
            }
            ReviewResolution::Exhausted { disclosure } => {
                let disclosure = merge_disclosure(disclosure, floor_disclosure);
                // Log the disclosure for transparency (not posted to control plane)
                if let Some(note) = &disclosure {
                    tracing::warn!(%task_id, "coverage disclosure (not posted): {}", note);
                }
                ReviewOutcome::Exhausted
            }
            ReviewResolution::Aborted(reason) => ReviewOutcome::Aborted(reason),
        },
    )
}

/// The sentinel plugin's own three kinds (ADR-0106, `integrations/opencode/plugins/sentinel`) —
/// `serde(rename_all = "snake_case")` matches the plugin's TS union (`"provider_error"` etc.) verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum SentinelFatalKind {
    ProviderError,
    UncaughtException,
    ExitWithoutTerminal,
}

/// One fatal-shaped event the sentinel plugin observed, deserialized from its marker file
/// (`LCI_SENTINEL_MARKER_PATH`). Field names match the plugin's `FatalEvent` JSON shape (camelCase) —
/// see `integrations/opencode/plugins/sentinel/src/index.ts`.
#[derive(Debug, Clone, serde::Deserialize)]
struct SentinelEvent {
    #[serde(rename = "fatalKind")]
    fatal_kind: SentinelFatalKind,
    message: String,
    #[serde(rename = "lastToolCall")]
    last_tool_call: Option<String>,
}

/// Best-effort read of the sentinel marker file, written by the sentinel plugin (ADR-0106) if it ever
/// observed a fatal-shaped event this run. `None` on any failure (file absent — the overwhelmingly
/// common case, nothing fatal happened — or malformed content) — never lets a marker-read problem mask
/// or replace the actual review outcome.
async fn read_sentinel_marker(marker_path: &Path) -> Option<SentinelEvent> {
    let content = tokio::fs::read_to_string(marker_path).await.ok()?;
    match serde_json::from_str(&content) {
        Ok(event) => Some(event),
        Err(error) => {
            tracing::warn!(%error, path = %marker_path.display(), "sentinel marker file is malformed; ignoring");
            None
        }
    }
}

/// Combine the coverage disclosure (ADR-0069) with the ADR-0099 operator-overlay floor note. Either may
/// be absent; when both are present the floor note follows the coverage note as its own paragraph.
fn merge_disclosure(coverage: Option<String>, floor: Option<String>) -> Option<String> {
    match (coverage, floor) {
        (Some(coverage), Some(floor)) => Some(format!("{coverage}\n\n{floor}")),
        (Some(coverage), None) => Some(coverage),
        (None, floor) => floor,
    }
}

// Best-effort post of the coverage disclosure note (ADR-0069 / #306) as the review summary — a failed
// re-post keeps the model's own summary rather than failing a finished run.
// NOTE: This function is no longer used (coverage disclosure no longer posted to control plane).
// async fn post_disclosure(client: &ControlPlaneClient, task_id: Uuid, disclosure: Option<String>) {
//     if let Some(note) = disclosure
//         && let Err(error) = client.set_review_summary(task_id, &note).await
//     {
//         tracing::warn!(%error, task_id = %task_id, "coverage disclosure re-post failed (non-fatal)");
//     }
// }

#[cfg(test)]
mod sentinel_marker_tests {
    use super::*;

    // Exact shape the sentinel plugin's `writeFileSync` produces
    // (`integrations/opencode/plugins/sentinel/src/index.ts`'s `FatalEvent`) — a cross-language
    // contract test: if either side's field names/casing drift, this test (not a silent runtime
    // parse failure) is what should catch it.
    #[tokio::test]
    async fn parses_the_exact_shape_the_sentinel_plugin_writes() {
        let dir = tempfile::tempdir().unwrap();
        let marker_path = dir.path().join("sentinel.marker.json");
        tokio::fs::write(
            &marker_path,
            r#"{"kind":"fatal_event","fatalKind":"provider_error","message":"provider unreachable","lastToolCall":"lightbridge_read_file","sessionID":"s1"}"#,
        )
        .await
        .unwrap();
        let event = read_sentinel_marker(&marker_path).await.expect("parses");
        assert_eq!(event.fatal_kind, SentinelFatalKind::ProviderError);
        assert_eq!(event.message, "provider unreachable");
        assert_eq!(
            event.last_tool_call.as_deref(),
            Some("lightbridge_read_file")
        );
    }

    #[tokio::test]
    async fn parses_exit_without_terminal_with_a_null_last_tool_call() {
        let dir = tempfile::tempdir().unwrap();
        let marker_path = dir.path().join("sentinel.marker.json");
        tokio::fs::write(
            &marker_path,
            r#"{"kind":"fatal_event","fatalKind":"exit_without_terminal","message":"process exited","lastToolCall":null,"sessionID":null}"#,
        )
        .await
        .unwrap();
        let event = read_sentinel_marker(&marker_path).await.expect("parses");
        assert_eq!(event.fatal_kind, SentinelFatalKind::ExitWithoutTerminal);
        assert_eq!(event.last_tool_call, None);
    }

    // The overwhelmingly common case: nothing fatal happened, so the plugin never wrote the file.
    #[tokio::test]
    async fn absent_marker_file_is_none_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let marker_path = dir.path().join("sentinel.marker.json");
        assert!(read_sentinel_marker(&marker_path).await.is_none());
    }

    #[tokio::test]
    async fn malformed_marker_content_degrades_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let marker_path = dir.path().join("sentinel.marker.json");
        tokio::fs::write(&marker_path, "not json").await.unwrap();
        assert!(read_sentinel_marker(&marker_path).await.is_none());
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
///
/// F4 (#463) also rides this same real run: at `LCI_LOG_LEVEL=debug` the mock provider emits an
/// OpenAI-style `reasoning_content` delta (ADR-0060's captured real-provider shape), so real opencode
/// surfaces a `message.part.updated` with `part.type:"reasoning"`, and the logger plugin — whose
/// stderr we capture here — must turn that into an `agent.reasoning` line. This is the real-wire proof
/// that reasoning reaches the logs, which no synthetic-event unit test can give (it can't catch a
/// wire-shape drift — the #411 silent-drop failure shape).
#[cfg(test)]
mod e2e {
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::time::Duration;

    use lci_acp_host::{AcpClient, PermissionPolicy};
    use lci_review_agent::opencode::{
        REVIEW_PROMPT_FILE, ReviewDriver, ReviewGates, ReviewResolution, ReviewSession,
        render_review_config, run_review_loop,
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
            // TOP-LEVEL tool disables (agent-independent) — a per-agent block is ignored because
            // opencode runs its default `build` agent, not a custom one (proven below and by
            // `agent_build_prompt_reaches_the_real_wire`). Targeting `agent.build` here for the same
            // reason, though this test's own instruction reaches the model via `run_review_loop`'s
            // prompt argument, not this config field.
            "tools": { "read": false, "grep": false, "glob": false, "list": false,
                       "edit": false, "write": false, "patch": false, "bash": false,
                       "webfetch": false, "websearch": false, "task": false, "todowrite": false,
                       "skill": false },
            "agent": { "build": {
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
            // Debug level so the logger emits `agent.reasoning` (chain-of-thought is debug-only). The
            // mock provider streams a `reasoning_content` delta, so a real reasoning part flows here.
            ("LCI_LOG_LEVEL".to_string(), "debug".to_string()),
        ];

        // Capture the opencode child's stderr (where the logger plugin writes) so the F4 assertion can
        // prove `agent.reasoning` reaches the logs on the real wire. Production uses the plain `spawn`
        // (stderr inherited → Loki); only this e2e pipes it.
        let (acp, logger_stderr) = AcpClient::spawn_with_captured_stderr(
            "opencode",
            &workdir,
            PermissionPolicy::Cancel,
            &env,
        )
        .await
        .expect("spawn opencode acp");
        acp.initialize().await.expect("initialize");
        let session_id = acp
            .new_session(&workdir.to_string_lossy(), serde_json::json!([]))
            .await
            .expect("session/new");

        let gates = ReviewGates::new(vec!["a.rs".to_string()], 3, 40);
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

        // F4 (#463): the real run must have surfaced the model's reasoning to the LOGS. The mock
        // streamed a `reasoning_content` delta, real opencode mapped it to a `part.type:"reasoning"`
        // `message.part.updated`, and the logger plugin (stderr captured above) must have emitted an
        // `agent.reasoning` line. Asserting on the real captured stderr — not a synthetic event — is
        // what makes this a wire-shape-drift guard.
        let logger_lines = logger_stderr.lock().await.clone();
        let reasoning_lines: Vec<&String> = logger_lines
            .iter()
            .filter(|l| l.contains("\"message\":\"agent.reasoning\""))
            .collect();
        assert!(
            !reasoning_lines.is_empty(),
            "no `agent.reasoning` line reached the logger's stderr — reasoning did NOT round-trip \
             from the real wire to the logs (F4). Captured logger stderr:\n{}",
            logger_lines.join("\n")
        );
    }

    /// ADR-0099 proof against REAL opencode: the rendered config (base + injection + operator overlay)
    /// is a config opencode's strict schema ACCEPTS, and the overlay actually takes effect — a custom
    /// sub-agent appears and a permission the overlay opened is honoured — while a floor invariant the
    /// overlay did NOT touch (built-in `read` disabled) stays intact. Uses `opencode debug config`
    /// (opencode's own resolver, incl. `{env:*}`/`{file:*}` substitution) so this is opencode's verdict,
    /// not ours. Skipped when `opencode` isn't on PATH so CI stays green.
    #[test]
    fn rendered_config_with_overlay_is_accepted_by_real_opencode() {
        if !on_path("opencode") {
            eprintln!("SKIP: opencode not on PATH — this ADR-0099 proof needs it");
            return;
        }
        // The overlay a SysAdmin might ship: add a read-only `explore` sub-agent and open `bash`.
        let overlay = serde_json::json!({
            "agent": { "explore": { "mode": "subagent", "description": "read-only helper" } },
            "permission": { "bash": "allow" }
        });
        let rendered = render_review_config(None, &serde_json::Map::new(), &[], Some(&overlay));
        // Our floor diff must have flagged the bash relaxation (surfaced, not blocked).
        assert!(
            rendered
                .floor_breaches
                .iter()
                .any(|b| b.message.contains("permission `bash` opened")),
            "expected the render to flag the bash relaxation: {:?}",
            rendered.floor_breaches
        );

        let workdir = std::env::temp_dir().join("lci-adr0099-overlay-proof");
        let _ = std::fs::remove_dir_all(&workdir);
        let home = workdir.join("home");
        let xdg = workdir.join("xdg");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&xdg).unwrap();
        let config_path = workdir.join("opencode.review.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec_pretty(&rendered.config).unwrap(),
        )
        .unwrap();
        // The `{file:REVIEW_PROMPT_FILE}` reference resolves relative to the config dir — write it there.
        std::fs::write(workdir.join(REVIEW_PROMPT_FILE), "REVIEWER PROMPT BODY").unwrap();

        // opencode's OWN resolver: it parses the config (strict schema — an unknown key would fail the
        // whole thing) and applies substitution. Isolated HOME/XDG so only OUR config is a source.
        let out = std::process::Command::new("opencode")
            .args(["debug", "config"])
            .env("OPENCODE_CONFIG", &config_path)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &xdg)
            .env("XDG_DATA_HOME", &xdg)
            .env("XDG_CACHE_HOME", &xdg)
            .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
            .env("OPENCODE_DISABLE_MODELS_FETCH", "1")
            .env("LCI_EAIG_BASE_URL", "https://gw.internal/v1")
            .env("LCI_EAIG_API_KEY", "test-key")
            .env("LCI_EAIG_MODEL", "test-model")
            .output()
            .expect("run opencode debug config");
        assert!(
            out.status.success(),
            "opencode REJECTED the rendered config (strict schema):\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let resolved: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("opencode debug config emits JSON");

        // The overlay took effect: the custom sub-agent is present…
        assert_eq!(
            resolved["agent"]["explore"]["mode"], "subagent",
            "overlay sub-agent missing from opencode's resolved config: {resolved}"
        );
        // …and the permission the overlay opened is honoured.
        assert_eq!(
            resolved["permission"]["bash"], "allow",
            "overlay permission not honoured: {resolved}"
        );
        // A floor invariant the overlay did NOT touch stays intact (mediated coverage preserved).
        assert_eq!(
            resolved["tools"]["read"], false,
            "untouched floor invariant (read disabled) was lost: {resolved}"
        );
        // The reviewer prompt resolved from the {file:*} reference to the per-task file content, on
        // `agent.build` — the agent ACP actually runs (see `agent_build_prompt_reaches_the_real_wire`).
        assert_eq!(
            resolved["agent"]["build"]["prompt"], "REVIEWER PROMPT BODY",
            "the {{file:*}} reviewer prompt did not resolve: {resolved}"
        );
        // Secrets stayed as resolved-from-env (never inlined into the checked-in base).
        assert_eq!(
            resolved["provider"]["eaig"]["options"]["baseURL"],
            "https://gw.internal/v1"
        );
    }

    /// Fast-tier-parity plan, Step 0 (spike): `integrations/opencode/config/review.jsonc`'s own comment
    /// and ADR-0097 already proved, for the `tools` key, that the live ACP session runs opencode's
    /// default `build` agent, not the checked-in `agent.review` block. This proves the SAME is true for
    /// `prompt` by checking the real wire (not `opencode debug config`, which only proves schema
    /// resolution, never functional effect) — and proves the fix: targeting `agent.build.prompt` instead
    /// actually reaches the model. `agent.review.prompt` is presumed inert by the same mechanism already
    /// proven for `tools`; re-deriving that negative here would need a slow/flaky non-arrival timeout for
    /// no extra signal, so this test asserts the positive fix only.
    #[tokio::test]
    async fn agent_build_prompt_reaches_the_real_wire() {
        if !on_path("opencode") || !on_path("node") {
            eprintln!("SKIP: opencode/node not on PATH — this e2e proof needs both");
            return;
        }
        let root = repo_root();
        let sim = root.join("integrations/opencode/sim");
        let plugins = root.join("integrations/opencode/plugins");
        let port = 8918u16;

        let workdir = std::env::temp_dir().join("lci-opencode-review-e2e-build-prompt");
        let _ = std::fs::remove_dir_all(&workdir);
        std::fs::create_dir_all(&workdir).unwrap();
        let fake_home = workdir.join("home");
        let xdg = workdir.join("xdg");
        std::fs::create_dir_all(&fake_home).unwrap();
        std::fs::create_dir_all(&xdg).unwrap();
        let recorder_path = workdir.join("recording.jsonl");
        let config_path = workdir.join("opencode.json");
        let msg_log = workdir.join("messages.log");

        const MARKER: &str = "MARKER_AGENT_BUILD_PROMPT_9f2c1a";

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
            "tools": { "read": false, "grep": false, "glob": false, "list": false,
                       "edit": false, "write": false, "patch": false, "bash": false,
                       "webfetch": false, "websearch": false, "task": false, "todowrite": false,
                       "skill": false },
            // Target `agent.build` (the agent ACP actually runs) instead of the checked-in
            // `agent.review` block — this is the injection-point fix this spike verifies.
            "agent": { "build": { "prompt": MARKER } }
        });
        std::fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();

        let mut provider = tokio::process::Command::new("node")
            .arg(sim.join("review-mock-provider.mjs"))
            .env("LCI_SIM_PROVIDER_PORT", port.to_string())
            .env("LCI_SIM_MSG_LOG", msg_log.display().to_string())
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

        let (acp, _logger_stderr) = AcpClient::spawn_with_captured_stderr(
            "opencode",
            &workdir,
            PermissionPolicy::Cancel,
            &env,
        )
        .await
        .expect("spawn opencode acp");
        acp.initialize().await.expect("initialize");
        let session_id = acp
            .new_session(&workdir.to_string_lossy(), serde_json::json!([]))
            .await
            .expect("session/new");

        let mut session = OpencodeReviewSession::new(acp, session_id, recorder_path.clone());
        let result = session
            .prompt("Distinct user turn: please review a.rs now.")
            .await;
        let _ = session.shutdown().await;
        let _ = provider.kill().await;

        let logged = std::fs::read_to_string(&msg_log).unwrap_or_default();
        result.unwrap_or_else(|error| {
            panic!("review loop errored: {error}\n--- logged requests ---\n{logged}")
        });
        assert!(
            logged.contains(MARKER),
            "agent.build.prompt content never reached the model on the real wire — the injection \
             point is wrong.\n--- logged requests ---\n{logged}"
        );
    }

    /// Fast-tier-parity plan, Step 0 (spike), second half — **documents a known opencode limitation,
    /// does not prove a working feature**. `agent.build.maxSteps` is schema-accepted by the pinned
    /// binary (`opencode debug config` resolves it fine) but was found, empirically, NOT to bound the
    /// model's in-session turns over ACP: driven against a provider that never finishes
    /// (`LCI_SIM_NEVER_FINISH=1`) with `maxSteps: 3`, the session made 600+ round-trips before a test
    /// timeout had to cut it off. Because of that finding, `review.max_cycles` (the Rust-side re-prompt
    /// ceiling) was kept and made tier-configurable rather than retired — see the fast-tier-parity
    /// plan's Context section. This test asserts the CURRENT (broken) behavior with a short bounded
    /// timeout so it stays a fast, green CI canary: if opencode ever ships a fix, this test starts
    /// FAILING — that failure is a signal to revisit whether `maxSteps` can now replace/shrink
    /// `review.max_cycles`, not a regression to silently paper over.
    #[tokio::test]
    async fn agent_build_max_steps_does_not_cap_a_never_finishing_model() {
        if !on_path("opencode") || !on_path("node") {
            eprintln!("SKIP: opencode/node not on PATH — this e2e proof needs both");
            return;
        }
        let root = repo_root();
        let sim = root.join("integrations/opencode/sim");
        let plugins = root.join("integrations/opencode/plugins");
        let port = 8919u16;

        let workdir = std::env::temp_dir().join("lci-opencode-review-e2e-max-steps");
        let _ = std::fs::remove_dir_all(&workdir);
        std::fs::create_dir_all(&workdir).unwrap();
        let fake_home = workdir.join("home");
        let xdg = workdir.join("xdg");
        std::fs::create_dir_all(&fake_home).unwrap();
        std::fs::create_dir_all(&xdg).unwrap();
        let recorder_path = workdir.join("recording.jsonl");
        let config_path = workdir.join("opencode.json");
        let tools_log = workdir.join("tools.log");

        const MAX_STEPS: u64 = 3;

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
            "tools": { "read": false, "grep": false, "glob": false, "list": false,
                       "edit": false, "write": false, "patch": false, "bash": false,
                       "webfetch": false, "websearch": false, "task": false, "todowrite": false,
                       "skill": false },
            "agent": { "build": { "maxSteps": MAX_STEPS } }
        });
        std::fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();

        let mut provider = tokio::process::Command::new("node")
            .arg(sim.join("review-mock-provider.mjs"))
            .env("LCI_SIM_PROVIDER_PORT", port.to_string())
            .env("LCI_SIM_TOOLS_LOG", tools_log.display().to_string())
            .env("LCI_SIM_NEVER_FINISH", "1")
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

        let (acp, _logger_stderr) = AcpClient::spawn_with_captured_stderr(
            "opencode",
            &workdir,
            PermissionPolicy::Cancel,
            &env,
        )
        .await
        .expect("spawn opencode acp");
        acp.initialize().await.expect("initialize");
        let session_id = acp
            .new_session(&workdir.to_string_lossy(), serde_json::json!([]))
            .await
            .expect("session/new");

        let mut session = OpencodeReviewSession::new(acp, session_id, recorder_path.clone());
        // Short bound (not 30s): we EXPECT this to time out — see the doc comment. Long enough to be
        // confident it's not just slow to start (the provider answers in milliseconds), short enough
        // to keep this a fast CI canary rather than a slow one.
        let outcome = tokio::time::timeout(
            Duration::from_secs(8),
            session.prompt("Review the change; keep going until you're told to stop."),
        )
        .await;

        let _ = session.shutdown().await;
        let _ = provider.kill().await;

        let advertised_requests = std::fs::read_to_string(&tools_log)
            .unwrap_or_default()
            .lines()
            .count();

        assert!(
            outcome.is_err(),
            "agent.build.maxSteps={MAX_STEPS} unexpectedly capped the session after only \
             {advertised_requests} requests — opencode may have fixed step-limit enforcement over ACP \
             upstream. If so, revisit the fast-tier-parity plan's decision to keep review.max_cycles \
             as the real ceiling instead of retiring it in favor of maxSteps."
        );
        assert!(
            advertised_requests > (MAX_STEPS as usize) + 2,
            "expected far more than {MAX_STEPS} provider round-trips in 8s (maxSteps is known not to \
             cap ACP sessions at the pinned opencode version), got only {advertised_requests} — \
             investigate before assuming this is still the same known-broken behavior"
        );
    }
}
