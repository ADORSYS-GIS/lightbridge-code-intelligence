// Unix-only: stubs `opengrep` via a `#!/bin/sh` script + `PermissionsExt` (the review runner images are
// all Linux/musl anyway). Gate the whole file so a non-Unix `cargo test` still compiles.
#![cfg(unix)]

//! Wire-boundary proof for the SAST parity port (ADR-0073 / ADR-0097): spawn the REAL `lci-review-mcp`
//! binary, speak JSON-RPC over its stdio, and prove `run_sast`
//!   1. is advertised in `tools/list` ONLY when the supervisor set the SAST env group (opt-in surface),
//!   2. actually executes over the wire — dispatching through the real `Tools::with_sast` registry into
//!      `lci_agent_sast::scan` (a stub opengrep), buffering the finding to the (mock) control plane, and
//!      returning the digest whose coordinates the supervisor's `SastAnchorGate` anchors to.
//!
//! This is the "drive it, don't just unit-test it" evidence the #246 lesson demands for a cross-process
//! boundary: unit tests can't see the real binary's stdio framing + env-gated registration.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use lci_agent_sast::SastConfig;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use uuid::Uuid;
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

/// A stub `opengrep` that writes the fixed SARIF to its `--sarif-output=PATH` and exits 0 — the same
/// technique `lci-review-agent`'s `run_sast` unit tests use (no real opengrep in this dev/CI env).
fn write_stub_opengrep(dir: &Path, sarif_body: &str) -> PathBuf {
    let bin = dir.join("opengrep-stub.sh");
    let script = format!(
        "#!/bin/sh\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    --sarif-output=*)\n      out=\"${{arg#--sarif-output=}}\"\n      cat > \"$out\" <<'SARIF'\n{sarif_body}\nSARIF\n      ;;\n  esac\ndone\nexit 0\n",
    );
    let mut file = std::fs::File::create(&bin).unwrap();
    file.write_all(script.as_bytes()).unwrap();
    let mut perms = std::fs::metadata(&bin).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&bin, perms).unwrap();
    bin
}

/// Spawn the real MCP binary with the given extra env; base env (`LCI_MCP_*`) is always set.
fn spawn_mcp(
    cp_uri: &str,
    task_id: Uuid,
    checkout: &Path,
    extra_env: &[(String, String)],
) -> Child {
    let bin = env!("CARGO_BIN_EXE_lci-review-mcp");
    let mut cmd = Command::new(bin);
    cmd.env("LCI_MCP_CP_URL", cp_uri)
        .env("LCI_MCP_RUNNER_TOKEN", "tok")
        .env("LCI_MCP_TASK_ID", task_id.to_string())
        .env("LCI_MCP_CHECKOUT", checkout.display().to_string())
        .env("LCI_MCP_EMBED_URL", "http://unused")
        .env("LCI_MCP_EMBED_KEY", "key")
        .env("LCI_MCP_EMBED_MODEL", "model")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    cmd.spawn().expect("spawn lci-review-mcp")
}

/// Send one JSON-RPC request and read line-delimited responses until the one with `id` arrives
/// (requests run in their own tasks server-side, so responses may interleave — correlate by id).
async fn rpc(
    stdin: &mut tokio::process::ChildStdin,
    reader: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    request: Value,
) -> Value {
    let id = request["id"].clone();
    let line = format!("{}\n", serde_json::to_string(&request).unwrap());
    stdin.write_all(line.as_bytes()).await.unwrap();
    stdin.flush().await.unwrap();
    loop {
        let next = tokio::time::timeout(Duration::from_secs(20), reader.next_line())
            .await
            .expect("MCP response timed out")
            .expect("reading MCP stdout")
            .expect("MCP closed stdout early");
        let value: Value = match serde_json::from_str(&next) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if value.get("id") == Some(&id) {
            return value;
        }
    }
}

fn tool_names(list_result: &Value) -> Vec<String> {
    list_result["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect()
}

/// The SAST env the supervisor sets when `run_sast` is offered: the `SastConfig` round-trip plus the
/// changed-file list path. Returns the (env pairs, tempdirs-to-keep-alive).
fn sast_env(stub: &Path, scratch: &Path) -> Vec<(String, String)> {
    let config = SastConfig {
        bin: stub.display().to_string(),
        rules: "unused-by-the-stub".to_string(),
        min_severity: "warning".to_string(),
        max_findings: 50,
        timeout_secs: 30,
    };
    let list_path = scratch.join("changed-files.txt");
    std::fs::write(&list_path, "src/exec.rs\n").unwrap();
    let mut env = config.to_env_pairs();
    env.push((
        lci_agent_sast::ENV_CHANGED_FILES.to_string(),
        list_path.display().to_string(),
    ));
    env
}

/// Opt-in: with NO SAST env, the real binary must not advertise `run_sast` (the surface rule).
#[tokio::test]
async fn run_sast_absent_from_tools_list_without_sast_env() {
    let cp = MockServer::start().await;
    let checkout = tempfile::tempdir().unwrap();
    let task_id = Uuid::new_v4();

    let mut child = spawn_mcp(&cp.uri(), task_id, checkout.path(), &[]);
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap()).lines();

    let _ = rpc(
        &mut stdin,
        &mut reader,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
    )
    .await;
    let list = rpc(
        &mut stdin,
        &mut reader,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    )
    .await;
    let names = tool_names(&list);
    assert!(
        !names.contains(&"run_sast".to_string()),
        "run_sast must NOT be offered without the SAST env: {names:?}"
    );
    // Sanity: the core review tools ARE there (so the assertion above isn't a "no tools at all" bug).
    assert!(
        names.contains(&"add_review_comment".to_string()),
        "{names:?}"
    );

    let _ = child.kill().await;
}

/// The real thing: with the SAST env set, `run_sast` is advertised AND actually runs over the wire —
/// opengrep (stub) scans the changed file, the finding is buffered to the (mock) control plane, and the
/// digest naming `src/exec.rs:42` comes back as the tool result (the coordinate the anchor gate uses).
#[tokio::test]
async fn run_sast_lists_and_executes_over_the_real_stdio_boundary() {
    let task_id = Uuid::new_v4();
    let cp = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wm_path(format!(
            "/api/v2/internal/tasks/{task_id}/review/inline"
        )))
        .respond_with(ResponseTemplate::new(204))
        .mount(&cp)
        .await;

    let checkout = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(checkout.path().join("src")).unwrap();
    std::fs::write(checkout.path().join("src/exec.rs"), "fn main() {}\n").unwrap();

    let scratch = tempfile::tempdir().unwrap();
    let stub = write_stub_opengrep(scratch.path(), ONE_FINDING_SARIF);
    let env = sast_env(&stub, scratch.path());

    let mut child = spawn_mcp(&cp.uri(), task_id, checkout.path(), &env);
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap()).lines();

    let _ = rpc(
        &mut stdin,
        &mut reader,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
    )
    .await;

    // 1. run_sast is advertised (opt-in surface satisfied).
    let list = rpc(
        &mut stdin,
        &mut reader,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    )
    .await;
    let names = tool_names(&list);
    assert!(
        names.contains(&"run_sast".to_string()),
        "run_sast must be offered with the SAST env set: {names:?}"
    );

    // 2. run_sast executes over the wire and returns the digest for the flagged coordinate.
    let call = rpc(
        &mut stdin,
        &mut reader,
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
               "params":{"name":"run_sast","arguments":{}}}),
    )
    .await;
    let text = call["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains("src/exec.rs:42"),
        "the run_sast result digest names the flagged coordinate: {text}"
    );
    assert!(
        text.contains("rust.security.exec"),
        "the digest carries the rule id the anchor gate keys on: {text}"
    );

    // 3. The finding was buffered to the control plane over the real mediated channel (same as native).
    let requests = cp.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .any(|r| r.url.path().ends_with("/review/inline")),
        "run_sast buffered the finding via add_review_comment"
    );

    let _ = child.kill().await;
}
