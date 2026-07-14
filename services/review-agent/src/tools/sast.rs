//! `run_sast` (ADR-0073): the deterministic opengrep pass (ADR-0061), now a mediated tool the agent calls
//! on demand instead of an automatic pre-agent scan. The scan/buffer/digest machinery itself is reused
//! verbatim from `lci-agent-sast`; this module only owns *when* it runs (a tool call) and *how* its
//! findings reach [`SastAnchorGate`](crate::policies::SastAnchorGate) (the shared [`SastLeadSink`]).

use std::sync::Arc;

use lci_agent_sast as sast;
use lci_agent_sast::SastConfig;
use lci_agent_tools::{
    BoxFuture, RegistryError, ReplaySafety, RuntimeCaps, Tool, ToolCx, ToolKind, ToolRegistry,
};
use lci_agent_types::{ToolCallReq, ToolOutcome, ToolSpec};
use serde::Deserialize;

use super::{ReviewServices, parse};
use crate::policies::{SastLead, SastLeadSink, normalize_repo_path};

pub const RUN_SAST: &str = "run_sast";

/// What [`register`] needs beyond the shared [`ReviewServices`]: the resolved scan config, the PR's
/// changed-file set (the default scan scope when `files` is omitted), and the sink
/// [`SastAnchorGate`](crate::policies::SastAnchorGate) drains. The host passes `None` to
/// [`super::tool_registry`] when SAST is off or there's no diff to scope a scan to — the tool then simply
/// isn't registered, mirroring how a discovery failure leaves an MCP tool unregistered.
pub struct SastToolConfig {
    pub config: SastConfig,
    pub changed_files: Vec<String>,
    pub leads: SastLeadSink,
}

#[derive(Deserialize)]
struct Args {
    #[serde(default)]
    files: Option<Vec<String>>,
}

pub fn spec() -> ToolSpec {
    ToolSpec::function(
        RUN_SAST,
        "Run a deterministic static-analysis (opengrep) pass over the PR's changed files and record any findings into this review. Call this EARLY in a real review, before `finish` — a purely conversational answer that never calls it triggers no scan. Findings are buffered and WILL be posted regardless of what you do with them afterward; do not re-report what this tool's result already lists. Idempotent: re-calling upserts by (file, line), so calling it more than once (e.g. after investigating further) is safe.",
        serde_json::json!({"type":"object","properties":{"files":{"type":"array","items":{"type":"string"},"description":"Optional subset of changed files to scope the scan to. Omit to scan every changed file."}}}),
    )
}

struct RunSastTool {
    spec: ToolSpec,
    services: ReviewServices,
    config: SastConfig,
    changed_files: Vec<String>,
    leads: SastLeadSink,
}

pub(crate) fn register(
    registry: &mut ToolRegistry,
    services: &ReviewServices,
    tool_config: SastToolConfig,
    caps: RuntimeCaps,
) -> Result<(), RegistryError> {
    registry.register(
        Arc::new(RunSastTool {
            spec: spec(),
            services: services.clone(),
            config: tool_config.config,
            changed_files: tool_config.changed_files,
            leads: tool_config.leads,
        }),
        caps,
    )
}

impl Tool for RunSastTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Write
    }
    fn replay(&self) -> ReplaySafety {
        ReplaySafety::Idempotent
    }
    fn call<'a>(&'a self, cx: &'a ToolCx<'a>, call: &'a ToolCallReq) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let args = match parse::<Args>(&call.function.arguments) {
                Ok(args) => args,
                Err(error) => return ToolOutcome::Continue(error),
            };
            let root = match cx.workspace.root().await {
                Ok(root) => root,
                Err(error) => {
                    return ToolOutcome::Continue(format!(
                        "error: could not materialize the repository checkout: {error}"
                    ));
                }
            };
            // A `files` arg only ever SCOPES DOWN the scan — it must not be able to widen it past the
            // PR's changed-file set (bug caught in PR review, gemini-code-assist): match against
            // `self.changed_files` normalized (backslashes / leading `./` or `/`), but scan using the
            // CANONICAL spelling from `changed_files` rather than the model's raw string, so a
            // differently-spelled-but-equal request still resolves to the exact path the diff carries.
            let targets = match args.files {
                Some(requested) => {
                    let canonical: std::collections::HashMap<String, &String> = self
                        .changed_files
                        .iter()
                        .map(|f| (normalize_repo_path(f), f))
                        .collect();
                    requested
                        .iter()
                        .filter_map(|f| {
                            canonical.get(&normalize_repo_path(f)).map(|f| (*f).clone())
                        })
                        .collect()
                }
                None => self.changed_files.clone(),
            };
            if targets.is_empty() {
                return ToolOutcome::Continue(
                    "SAST scan complete: none of the requested files are in this PR's changed-file \
                     set (run_sast only scans the diff — nothing to record)."
                        .to_string(),
                );
            }
            let findings = match sast::scan(&self.config, root, &targets).await {
                Ok(findings) => findings,
                Err(error) => {
                    tracing::warn!(%error, "sast: opengrep scan failed (non-fatal)");
                    return ToolOutcome::Continue(format!(
                        "error: SAST scan failed (non-fatal, nothing recorded): {error:#}"
                    ));
                }
            };
            if findings.is_empty() {
                return ToolOutcome::Continue(
                    "SAST scan complete: opengrep found no findings in the scanned files."
                        .to_string(),
                );
            }
            // Same mediated `add_review_comment` channel the pre-agent pass used (ADR-0061) — a scanned
            // finding is buffered the moment the tool runs, before the tool result even reaches the
            // model, so it's recorded even if the run is exhausted/aborted right after this call.
            sast::buffer(self.services.client.as_ref(), cx.task_id, &findings).await;
            {
                let mut leads = self
                    .leads
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                leads.extend(findings.iter().map(|f| SastLead {
                    file: f.file.clone(),
                    line: f.line,
                    rule_id: f.rule_id.clone(),
                }));
            }
            // The digest IS the tool result now (ADR-0073) — previously a static prompt block injected
            // before the loop even started.
            ToolOutcome::Continue(sast::digest(&findings).unwrap_or_default())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use lci_agent_clients::{ControlPlaneClient, EmbeddingsClient};
    use lci_agent_types::FunctionCallReq;
    use uuid::Uuid;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::tools::EagerWorkspace;

    fn call(arguments: &str) -> ToolCallReq {
        ToolCallReq {
            id: "t".into(),
            kind: "function".into(),
            function: FunctionCallReq {
                name: RUN_SAST.into(),
                arguments: arguments.into(),
            },
            extra_content: None,
        }
    }

    const ONE_FINDING_SARIF: &str = r#"{
      "runs": [{
        "tool": {"driver": {"name": "opengrep", "rules": [
          {"id": "rust.security.exec", "defaultConfiguration": {"level": "error"}}
        ]}},
        "results": [
          {"ruleId": "rust.security.exec",
           "message": {"text": "Command injection via untrusted input."},
           "locations": [{"physicalLocation": {
             "artifactLocation": {"uri": "src/exec.rs"}, "region": {"startLine": 42}}}]}
        ]
      }]
    }"#;

    const EMPTY_SARIF: &str = r#"{"runs": [{"tool": {"driver": {"rules": []}}, "results": []}]}"#;

    /// Write a stub `opengrep` binary: it touches `marker` (so a test can assert whether it ran at all),
    /// dumps its argv to `{marker}.argv` (so a test can assert what targets it was invoked with), writes
    /// `sarif_body` to whatever `--sarif-output=PATH` it was given, and exits 0. This exercises the real
    /// `lci_agent_sast::scan` path (subprocess spawn → SARIF parse) without needing a real opengrep
    /// binary — none is installed in this dev/CI environment, and no existing test in the repo spawns the
    /// real one either.
    fn write_stub_opengrep(dir: &Path, marker: &Path, sarif_body: &str) -> PathBuf {
        let bin = dir.join("opengrep-stub.sh");
        let script = format!(
            "#!/bin/sh\ntouch '{marker}'\necho \"$@\" > '{marker}.argv'\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    --sarif-output=*)\n      out=\"${{arg#--sarif-output=}}\"\n      cat > \"$out\" <<'SARIF'\n{sarif_body}\nSARIF\n      ;;\n  esac\ndone\nexit 0\n",
            marker = marker.display(),
        );
        std::fs::write(&bin, script).unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();
        bin
    }

    fn config(bin: PathBuf) -> SastConfig {
        SastConfig {
            bin: bin.display().to_string(),
            rules: "unused-in-these-tests".to_string(),
            min_severity: "warning".to_string(),
            max_findings: 50,
            timeout_secs: 5,
        }
    }

    fn tool(
        cp_uri: &str,
        config: SastConfig,
        changed_files: Vec<String>,
        leads: SastLeadSink,
    ) -> RunSastTool {
        RunSastTool {
            spec: spec(),
            services: ReviewServices {
                client: Arc::new(ControlPlaneClient::new(cp_uri, "tok")),
                embedder: Arc::new(EmbeddingsClient::new("http://unused", "key", "model")),
            },
            config,
            changed_files,
            leads,
        }
    }

    #[tokio::test]
    async fn finds_a_finding_buffers_it_and_feeds_the_lead_sink() {
        let cp = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!(
                "/internal/tasks/{}/review/inline",
                Uuid::nil()
            )))
            .respond_with(ResponseTemplate::new(204))
            .mount(&cp)
            .await;

        let stub_dir = tempfile::tempdir().unwrap();
        let marker = stub_dir.path().join("ran");
        let bin = write_stub_opengrep(stub_dir.path(), &marker, ONE_FINDING_SARIF);

        let checkout = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(checkout.path().join("src"))
            .await
            .unwrap();
        tokio::fs::write(checkout.path().join("src/exec.rs"), "fn main() {}\n")
            .await
            .unwrap();

        let leads: SastLeadSink = Arc::new(Mutex::new(Vec::new()));
        let tool = tool(
            &cp.uri(),
            config(bin),
            vec!["src/exec.rs".to_string()],
            Arc::clone(&leads),
        );
        let workspace = EagerWorkspace::new(checkout.path().to_path_buf());
        let cx = ToolCx {
            task_id: Uuid::nil(),
            workspace: &workspace,
        };

        let outcome = tool.call(&cx, &call("{}")).await;

        assert!(marker.exists(), "the opengrep stub actually ran");
        let ToolOutcome::Continue(text) = outcome else {
            panic!("expected Continue");
        };
        assert!(
            text.contains("src/exec.rs:42"),
            "the digest names the finding: {text}"
        );

        {
            let recorded = leads.lock().unwrap();
            assert_eq!(recorded.len(), 1);
            assert_eq!(recorded[0].file, "src/exec.rs");
            assert_eq!(recorded[0].line, 42);
        }

        let requests = cp.received_requests().await.unwrap();
        assert!(
            requests
                .iter()
                .any(|r| r.url.path().ends_with("/review/inline")),
            "the finding was buffered via the same mediated add_review_comment channel"
        );
    }

    #[tokio::test]
    async fn no_findings_means_no_buffered_write_and_an_empty_sink() {
        // No mocks mounted on the control plane at all — a write here would fail loudly (panic on the
        // unmatched request), proving nothing gets buffered when opengrep finds nothing.
        let cp = MockServer::start().await;

        let stub_dir = tempfile::tempdir().unwrap();
        let marker = stub_dir.path().join("ran");
        let bin = write_stub_opengrep(stub_dir.path(), &marker, EMPTY_SARIF);

        let checkout = tempfile::tempdir().unwrap();
        tokio::fs::write(checkout.path().join("a.rs"), "one\n")
            .await
            .unwrap();

        let leads: SastLeadSink = Arc::new(Mutex::new(Vec::new()));
        let tool = tool(
            &cp.uri(),
            config(bin),
            vec!["a.rs".to_string()],
            Arc::clone(&leads),
        );
        let workspace = EagerWorkspace::new(checkout.path().to_path_buf());
        let cx = ToolCx {
            task_id: Uuid::nil(),
            workspace: &workspace,
        };

        let outcome = tool.call(&cx, &call("{}")).await;

        assert!(marker.exists(), "the opengrep stub still ran");
        assert_eq!(
            outcome,
            ToolOutcome::Continue(
                "SAST scan complete: opengrep found no findings in the scanned files.".to_string()
            )
        );
        assert!(leads.lock().unwrap().is_empty());
        assert!(cp.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn files_arg_scopes_the_scan_to_a_subset_of_changed_files() {
        let cp = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!(
                "/internal/tasks/{}/review/inline",
                Uuid::nil()
            )))
            .respond_with(ResponseTemplate::new(204))
            .mount(&cp)
            .await;

        let stub_dir = tempfile::tempdir().unwrap();
        let marker = stub_dir.path().join("ran");
        let bin = write_stub_opengrep(stub_dir.path(), &marker, EMPTY_SARIF);

        let checkout = tempfile::tempdir().unwrap();
        tokio::fs::write(checkout.path().join("only.rs"), "one\n")
            .await
            .unwrap();
        tokio::fs::write(checkout.path().join("other.rs"), "two\n")
            .await
            .unwrap();

        let leads: SastLeadSink = Arc::new(Mutex::new(Vec::new()));
        let tool = tool(
            &cp.uri(),
            config(bin),
            vec!["only.rs".to_string(), "other.rs".to_string()],
            Arc::clone(&leads),
        );
        let workspace = EagerWorkspace::new(checkout.path().to_path_buf());
        let cx = ToolCx {
            task_id: Uuid::nil(),
            workspace: &workspace,
        };

        tool.call(&cx, &call(r#"{"files":["only.rs"]}"#)).await;

        let argv = std::fs::read_to_string(format!("{}.argv", marker.display())).unwrap();
        assert!(
            argv.contains("only.rs"),
            "scoped to the requested file: {argv}"
        );
        assert!(
            !argv.contains("other.rs"),
            "the un-requested changed file is NOT scanned: {argv}"
        );
    }

    // PR review (gemini-code-assist): `files` must only ever SCOPE DOWN the scan, never widen it past
    // the PR's changed-file set — a model asking for a file outside the diff must not get it scanned.
    #[tokio::test]
    async fn files_arg_cannot_widen_the_scan_past_changed_files() {
        let stub_dir = tempfile::tempdir().unwrap();
        let marker = stub_dir.path().join("ran");
        let bin = write_stub_opengrep(stub_dir.path(), &marker, ONE_FINDING_SARIF);

        let checkout = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(checkout.path().join("src"))
            .await
            .unwrap();
        tokio::fs::write(checkout.path().join("src/exec.rs"), "fn main() {}\n")
            .await
            .unwrap();
        tokio::fs::write(checkout.path().join(".env"), "SECRET=shh\n")
            .await
            .unwrap();

        let cp = MockServer::start().await;
        let leads: SastLeadSink = Arc::new(Mutex::new(Vec::new()));
        let tool = tool(
            &cp.uri(),
            config(bin),
            vec!["src/exec.rs".to_string()],
            Arc::clone(&leads),
        );
        let workspace = EagerWorkspace::new(checkout.path().to_path_buf());
        let cx = ToolCx {
            task_id: Uuid::nil(),
            workspace: &workspace,
        };

        // Requests a file that exists in the checkout but is NOT in this PR's changed-file set.
        let outcome = tool.call(&cx, &call(r#"{"files":[".env"]}"#)).await;

        assert!(
            !marker.exists(),
            "opengrep must never run when every requested file is out of scope"
        );
        assert_eq!(
            outcome,
            ToolOutcome::Continue(
                "SAST scan complete: none of the requested files are in this PR's changed-file \
                 set (run_sast only scans the diff — nothing to record)."
                    .to_string()
            )
        );

        // A mix of in-scope and out-of-scope requests keeps only the in-scope one.
        tool.call(&cx, &call(r#"{"files":[".env","src/exec.rs"]}"#))
            .await;
        let argv = std::fs::read_to_string(format!("{}.argv", marker.display())).unwrap();
        assert!(
            argv.contains("src/exec.rs"),
            "the in-scope file is scanned: {argv}"
        );
        assert!(
            !argv.contains(".env"),
            "the out-of-scope file is dropped: {argv}"
        );
    }
}
