# Quality Gates and Opengrep SAST

This note briefly explains how Lightbridge checks review quality and how Opengrep SAST is installed and used in the review path.

## 1. Quality Gates

Quality Gates are review-loop policies that run around the agent's normal tool use. They do not replace the AI reviewer; they constrain when a review can finish and when a finding is allowed to survive.

The review flow registers the policies in a fixed order:

```rust
// services/review-agent/src/flows.rs
let policies: Vec<Box<dyn TurnPolicy>> = vec![
    Box::new(coverage),
    Box::new(RefuteGate::new(params.fast)),
    Box::new(SastAnchorGate::new(params.sast_leads, params.fast)),
    Box::new(FindingFinishNudge::new(params.fast)),
];
```

The main gates are:

- **Coverage Gate:** If the agent calls `finish` before reading or commenting on every changed file, the gate bounces the finish request and tells the agent what files are still missing.
- **Refute Pass:** High-priority findings are challenged before posting. The agent must re-check its own evidence and can call `retract_finding` if the claim does not hold.
- **SAST Anchor Gate:** If the agent comments on an Opengrep finding, it must anchor the verdict to the exact `file:line` Opengrep flagged.

The coverage bounce is configurable in `agent.json`:

```json
{
  "review": {
    "deep": {
      "max_coverage_bounces": 3
    }
  }
}
```

## 2. Opengrep Installation

The review runner image vendors Opengrep and its rules. The native runner image downloads the Opengrep binary and clones the rules into `/opt/opengrep-rules`:

```dockerfile
# services/agent-runner/Dockerfile
FROM debian:bookworm-slim AS opengrep

RUN curl -fsSL -o /usr/local/bin/opengrep \
      "https://github.com/opengrep/opengrep/releases/download/${OPENGREP_VERSION}/opengrep_manylinux_${arch}" \
  && chmod +x /usr/local/bin/opengrep \
  && opengrep --version \
  && git clone --depth 1 https://github.com/opengrep/opengrep-rules /opt/opengrep-rules

COPY --from=opengrep /usr/local/bin/opengrep /usr/local/bin/opengrep
COPY --from=opengrep /opt/opengrep-rules /opt/opengrep-rules
```

The OpenCode review image follows the same idea, but uses the musl-native Opengrep release and verifies the SHA256 before copying the binary into the final image.

## 3. Opengrep Configuration

SAST is opt-in. The runner has defaults, but the scan is only enabled when config turns it on and the tier offers `run_sast`.

```rust
// services/agent-runner/src/bootstrap/config/defaults.rs
pub const DEFAULT_SAST_BIN: &str = "opengrep";
pub const DEFAULT_SAST_RULES: &str = "/opt/opengrep-rules";
pub const DEFAULT_SAST_MIN_SEVERITY: &str = "error";
pub const DEFAULT_SAST_MAX_FINDINGS: usize = 25;
pub const DEFAULT_SAST_TIMEOUT_SECS: u64 = 300;
```

Example config:

```json
{
  "sast": {
    "enabled": true,
    "bin": "opengrep",
    "rules": "/opt/opengrep-rules",
    "min_severity": "error",
    "max_findings": 25,
    "timeout_secs": 300
  },
  "review": {
    "fast": {
      "tools": ["run_sast", "add_review_comment", "finish", "abort"]
    },
    "deep": {
      "tools": ["run_sast", "read_file", "add_review_comment", "finish", "abort"]
    }
  }
}
```

## 4. How `run_sast` Is Used

Opengrep is exposed as the `run_sast` tool. It is not a separate poster and it is not an unconditional pre-agent scan. The agent is instructed to call it early when it is available:

```rust
// services/review-agent/src/prompt.rs
if `run_sast` is available and this is a real review ... call it EARLY
```

When called, the SAST crate scans only changed files that still exist in the checkout:

```rust
// services/agent-sast/src/lib.rs
let sarif = process::run_opengrep(config, checkout, &targets, &config_paths).await?;
```

The subprocess writes SARIF to a private temp file and scans the changed files from the checkout:

```rust
// services/agent-sast/src/process.rs
cmd.arg("scan");
for path in config_paths {
    cmd.arg("--config").arg(path);
}
cmd.arg(format!("--sarif-output={}", sarif_path.display()))
    .arg("--quiet")
    .current_dir(&checkout_abs)
    .args(targets);
```

Results are normalized, buffered into the same review channel as AI findings, and returned to the agent as a digest so it does not duplicate Opengrep's findings.
