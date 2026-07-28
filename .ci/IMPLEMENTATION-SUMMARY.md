# Quality Pipeline Implementation Summary

This document summarizes the SonarQube CE replacement implementation: an offline-capable, GitHub Actions-native quality scanning system.

## What Was Implemented

### 1. Directory Structure

```
.ci/
├── quality/                       # Orchestration scripts
│   ├── README.md
│   ├── run.sh                     # Master orchestrator (all scanners)
│   ├── merge-sarif.sh             # Deduplicate & merge SARIF reports
│   ├── gate.sh                    # Quality gate enforcement
│   ├── hadolint-to-sarif.sh       # Hadolint JSON → SARIF converter
│   ├── biome-to-sarif.sh          # Biome JSON → SARIF converter
│   └── reports/                   # Scanner outputs (generated at runtime)
│       ├── semgrep.sarif
│       ├── trivy-fs.sarif
│       ├── gitleaks.sarif
│       ├── hadolint*.json
│       ├── biome.json
│       ├── hadolint.sarif
│       ├── biome.sarif
│       └── quality.sarif          # Merged canonical report
├── baselines/
│   ├── README.md                  # Suppression format & procedures
│   ├── runner-manifest.md         # Tool versions, DBs, provisioning
│   └── suppressions.json          # Explicit finding suppressions
└── rules/
    └── semgrep/
        ├── README.md              # Rule management guide
        └── security/
            ├── typescript-injection.yaml
            └── rust-security.yaml

.github/workflows/
└── quality.yml                    # GitHub Actions workflow

docs/
└── quality-pipeline.md            # User-facing documentation (475 lines)
```

### 2. Enabled Scanners

| Scanner | Purpose | Output | Offline Mode |
|---------|---------|--------|--------------|
| **Semgrep** | SAST | SARIF | ✓ (rules in `.ci/rules/semgrep/`) |
| **Trivy** | Dependencies, containers | SARIF | ✓ (DBs pre-cached on runner) |
| **Gitleaks** | Secrets | SARIF | ✓ (built-in patterns) |
| **Hadolint** | Dockerfiles | JSON → SARIF | ✓ (built-in rules) |
| **Biome** | TypeScript/JavaScript linting | JSON → SARIF | ✓ (workspace config) |

**Not implemented (future work):**
- CodeQL (would require pinned query packs)
- Code duplication (jscpd, Lizard)
- Complexity metrics

### 3. GitHub Actions Workflow (.github/workflows/quality.yml)

**Triggers:**
- Pull requests (on open, synchronize, reopen)
- Pushes to `main` branch
- Scheduled weekly (Mondays 02:00 UTC)
- Manual dispatch (`workflow_dispatch`)

**Features:**
- Full history fetch for accurate PR diffs (`fetch-depth: 0`)
- Concurrency control (cancel older runs for same ref)
- Least-privilege permissions (read content, write checks/PRs)
- Artifact retention (30 days)
- Optional GitHub Code Scanning upload (gated by `vars.ENABLE_CODE_SCANNING`)

**Actions (pinned to commit SHA):**
- `actions/checkout@v4` (11bd..., 4.2.0)
- `actions/upload-artifact@v4` (97a0f..., 4.7.0)
- `github/codeql-action/upload-sarif@v3` (9fa7e..., 3.25.0) [conditional]

### 4. Orchestration Scripts

#### run.sh (Master Orchestrator)
- Runs all applicable scanners sequentially
- Captures exit codes (0 = no findings, 1 = findings found, 2+ = error)
- Generates individual SARIF reports
- Calls merge and gate scripts
- Exit codes: 0 (pass), 1 (config error), 2 (gate failed)

#### merge-sarif.sh (SARIF Merger)
- Merges all SARIF files into single deduplicated report
- Deduplication key: (ruleId, message, path, startLine)
- Output: `.ci/quality/reports/quality.sarif`
- Handles empty scanner sets gracefully

#### gate.sh (Quality Gate)
- **PR mode:** Fails if NEW error-level findings exist
- **Main branch:** Reports findings but does not fail (informational)
- **Scheduled scans:** Full scan, no fail
- Respects `GITHUB_EVENT_NAME` and `GITHUB_BASE_REF` for context detection

#### Converter Scripts
- `hadolint-to-sarif.sh` — Converts Hadolint JSON to SARIF 2.1.0
- `biome-to-sarif.sh` — Converts Biome JSON to SARIF 2.1.0

### 5. Semgrep Rules

Located in `.ci/rules/semgrep/security/`:

**typescript-injection.yaml:**
- `typescript-no-eval` — Detects eval() and Function() constructor misuse
- `typescript-sql-injection-template` — Detects potential SQL injection in template strings
- `typescript-no-hardcoded-secrets` — Detects hardcoded secrets (passwords, tokens, API keys)

**rust-security.yaml:**
- `rust-unsafe-block-undocumented` — Flags unsafe blocks without documentation
- `rust-unwrap-unchecked` — Warns on unwrap()/expect() (can panic)
- `rust-no-panic` — Detects panic!() calls (terminates program)

All rules are configured with severity levels, CWE/OWASP mappings, and references.

### 6. Runner Provisioning Manifest

`.ci/baselines/runner-manifest.md` documents:
- **Tool inventory** with versions and offline flags (Semgrep, Trivy, Gitleaks, Hadolint, Reviewdog, Biome)
- **Database provisioning** (Trivy DB pre-cache location and update procedure)
- **GitHub Actions versions** (all pinned to commit SHAs with release notes)
- **Setup checklist** for initial runner provisioning
- **Daily/monthly maintenance** procedures
- **Troubleshooting** guide

### 7. Suppressions & Baselines

`.ci/baselines/suppressions.json` — Format for explicit finding suppression:
```json
{
  "suppressions": [
    {
      "ruleId": "semgrep.security.eval",
      "path": "apps/web/src/dynamic-loader.ts",
      "reason": "Legitimate eval() in trusted admin context only",
      "expiresAt": "2026-12-31T23:59:59Z",
      "approved_by": "security@company.com",
      "ticket": "#TICKET-123"
    }
  ]
}
```

**Important:** Every suppression requires:
- Detailed reason (not just "false positive")
- Expiration date (forces periodic review)
- Approval contact
- Tracking ticket

### 8. Documentation

**docs/quality-pipeline.md (475 lines):**
- Architecture & data flow
- Enabled scanners & why each was selected
- SonarQube CE capability mapping
- Local execution instructions
- PR gate behavior (fail conditions, visualization)
- Triaging & suppressing findings
- Tool/rule/DB update procedures
- Code Scanning integration
- Troubleshooting guide
- Future enhancement ideas

## Validation Results

### Syntax Checks
- ✓ `.github/workflows/quality.yml` — Valid YAML (Python YAML parser)
- ✓ `.ci/quality/run.sh` — Valid bash syntax, execution-tested with fixture data
- ✓ `.ci/quality/merge-sarif.sh` — Valid bash syntax, execution-tested with fixture data
- ✓ `.ci/quality/gate.sh` — Valid bash syntax, execution-tested (all 3 policy branches: PR-fail,
  PR-pass, push-informational)
- ✓ `.ci/quality/hadolint-to-sarif.sh` — Valid bash syntax, execution-tested with fixture data
- ✓ `.ci/quality/biome-to-sarif.sh` — Valid bash syntax, execution-tested with fixture data

**Note:** an earlier revision of these scripts used a `#!/bin/zsh` shebang while the workflow
invokes them with `bash <script>.sh`. Because the GitHub Actions `run:` step ignores a script's
shebang and always uses the interpreter named in the step, `local` outside a function and
top-level `return` (both zsh-tolerant, bash-fatal) crashed the pipeline immediately, and a
malformed `jq` filter in the gate's error-count query threw on every invocation. All of this was
caught only by actually *executing* the scripts under `bash` with fixture SARIF/JSON — `zsh -n`
syntax checks are silent on this class of bug because they only parse, never run, the script.
Fixed by switching every script's shebang to `#!/usr/bin/env bash`, removing the misplaced
`local`/`return`, and correcting the `jq` filter.

### SARIF Compliance
- All scanners configured to output SARIF 2.1.0 (or converted to SARIF via helper scripts)
- Merged SARIF uses stable deduplication keys
- Tool driver metadata includes name, version, infoUri

### Offline Capability
- ✓ Semgrep: Rules in `.ci/rules/semgrep/` (no external downloads)
- ✓ Trivy: Uses `--skip-db-update --skip-java-db-update` (requires pre-cached DBs)
- ✓ Gitleaks: Built-in patterns (no external downloads)
- ✓ Hadolint: Built-in rules (no external downloads)
- ✓ Biome: Uses workspace config (no external downloads)
- ✓ Reviewdog: Only needs GITHUB_TOKEN (for PR reporting; external I/O permitted)

### GitHub Actions Pinning
All external actions pinned to commit SHAs (not floating tags):
- `actions/checkout@11bd71...` (v4.2.0)
- `actions/upload-artifact@97a0f...` (v4.7.0)
- `github/codeql-action/upload-sarif@9fa7e...` (v3.25.0)

## Runtime Prerequisites (Self-Hosted Runner)

### Tools Required (Pre-Provisioned)
- `semgrep` v1.45.0+
- `trivy` v0.51.0+
- `gitleaks` v8.18.0+
- `hadolint` v2.12.0+
- `reviewdog` v0.17.0+
- `pnpm` v11.5.2+ (already on vymalo-vps)
- `jq` (for SARIF merging)
- `bash` (standard on the Linux runner image; all orchestration scripts target bash explicitly)

### Databases & Caches (Pre-Provisioned)
- Trivy vulnerability DB: `$HOME/.cache/trivy/db/` (~600 MB, daily updates)
- Semgrep rules: `.ci/rules/semgrep/` (version-controlled)
- Gitleaks patterns: built-in (updated when binary upgraded)

See `.ci/baselines/runner-manifest.md` for detailed provisioning steps.

## Known Limitations

1. **Baseline comparisons:** Gate does not track "known findings" baseline. All error-level findings fail PRs. Use `suppressions.json` for accepted risks.

2. **Trivy DB staleness:** If DBs are not updated for >7 days, newly disclosed vulnerabilities may not be detected. Daily updates recommended.

3. **Hadolint SARIF:** Hadolint outputs JSON natively; uses helper script for SARIF conversion (not supported by hadolint directly).

4. **Biome SARIF:** Biome outputs JSON; uses helper script for SARIF conversion.

5. **No automatic fixes:** Scanners report findings only; no auto-repair (per requirements).

6. **CodeQL not yet enabled:** Would require pinned query pack provisioning (future enhancement).

## Integration with Existing CI

The quality pipeline coexists with existing CI:
- **rust-lint.yml** — Remains unchanged (cargo fmt, clippy, coverage)
- **control-plane-tests.yml** — Remains unchanged (integration tests)
- **quality.yml** — New (runs independently on PRs, main, scheduled)

No modifications to application source code or existing workflows.

## File Inventory

### Created Files (18 total)

#### Orchestration Scripts (6)
- `.ci/quality/run.sh`
- `.ci/quality/merge-sarif.sh`
- `.ci/quality/gate.sh`
- `.ci/quality/hadolint-to-sarif.sh`
- `.ci/quality/biome-to-sarif.sh`
- `.ci/quality/README.md`

#### Baselines & Suppressions (3)
- `.ci/baselines/runner-manifest.md`
- `.ci/baselines/suppressions.json`
- `.ci/baselines/README.md`

#### Semgrep Rules (3)
- `.ci/rules/semgrep/README.md`
- `.ci/rules/semgrep/security/typescript-injection.yaml`
- `.ci/rules/semgrep/security/rust-security.yaml`

#### Workflow (1)
- `.github/workflows/quality.yml`

#### Documentation (1)
- `docs/quality-pipeline.md`

#### Summary (1)
- `.ci/IMPLEMENTATION-SUMMARY.md` (this file)

## Next Steps: Runner Provisioning

**Before running the workflow, provision the self-hosted runner:**

1. **SSH into the runner** (label: `vymalo-vps`)

2. **Install scanner binaries:**
   ```bash
   # Semgrep
   pip install semgrep==1.45.0
   
   # Trivy
   curl -sfL https://raw.githubusercontent.com/aquasecurity/trivy/main/contrib/install.sh | \
     sh -s -- -b /usr/local/bin v0.51.0
   
   # Gitleaks
   curl -L https://github.com/gitleaks/gitleaks/releases/download/v8.18.0/gitleaks-linux-x64 \
     -o /usr/local/bin/gitleaks && chmod +x /usr/local/bin/gitleaks
   
   # Hadolint
   curl -L https://github.com/hadolint/hadolint/releases/download/v2.12.0/hadolint-Linux-x86_64 \
     -o /usr/local/bin/hadolint && chmod +x /usr/local/bin/hadolint
   
   # Reviewdog
   go install github.com/reviewdog/reviewdog/cmd/reviewdog@v0.17.0
   ```

3. **Pre-cache Trivy databases:**
   ```bash
   trivy image --download-db-only
   ```

4. **Verify installation:**
   ```bash
   semgrep --version && trivy --version && gitleaks version && hadolint --version && reviewdog --version
   ```

5. **Set up daily Trivy DB updates** (via cron or scheduled job):
   ```bash
   # Add to runner's crontab:
   0 3 * * * trivy image --download-db-update-only
   ```

See `.ci/baselines/runner-manifest.md` for detailed instructions and troubleshooting.

## Testing Locally

To test the pipeline on your local machine (requires all tools installed):

```bash
# Run full quality scan
bash .ci/quality/run.sh

# Inspect merged SARIF
cat .ci/quality/reports/quality.sarif | jq '.runs[0].results | length'

# Test Semgrep rules specifically
semgrep --config=.ci/rules/semgrep apps/web services/control-plane

# Test Trivy (requires pre-cached DBs)
trivy fs --skip-db-update --quiet .

# Test Gitleaks (requires Git history)
gitleaks detect --verbose
```

## Integration with Governance Template

This implementation respects ADORSYS-GIS AI Governance requirements:
- ✓ All external actions pinned to commit SHAs (reproducible)
- ✓ No auto-fixes applied to source code
- ✓ Quality gate and findings are human-reviewed before merge
- ✓ Full reports retained as artifacts (audit trail)
- ✓ Suppressions require documented justification and expiration dates

PRs that introduce this quality pipeline should include:
- **AI Usage Declaration:** Document which AI tools were used (e.g., Claude Code)
- **Source of Truth:** Link to this implementation summary or the quality-pipeline.md doc
- **Verification:** List of validation checks passed (YAML syntax, offline capability, etc.)

## References

- [Semgrep Docs](https://semgrep.dev/docs/)
- [Trivy Docs](https://aquasecurity.github.io/trivy/)
- [Gitleaks Docs](https://gitleaks.io/)
- [Hadolint Rules](https://github.com/hadolint/hadolint/wiki/Rules)
- [Reviewdog Docs](https://github.com/reviewdog/reviewdog)
- [SARIF Spec](https://sarifweb.azurewebsites.net/)
- [GitHub Code Scanning](https://docs.github.com/en/code-security/code-scanning)
