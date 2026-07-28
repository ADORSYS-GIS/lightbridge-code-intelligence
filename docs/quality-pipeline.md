# Quality Pipeline: Offline-Capable SAST & Dependency Scanning

## Overview

The Lightbridge quality pipeline replaces SonarQube CE with a modular, offline-capable scanning system built on GitHub Actions, local scanners, and reviewdog PR reporting.

**Design goals:**
- Run all analysis **offline** on self-hosted runners (no external rule/database downloads during CI)
- Produce clean, deduplicated **PR checks** via reviewdog (SARIF-based)
- Keep full reports as **artifacts** for dashboarding and compliance
- Preserve existing CI behavior (Rust tests, lint, coverage remain in their own workflows)
- Enable optional GitHub Code Scanning upload for centralized tracking
- No changes to application source code; no automatic fixes

## Architecture

```
GitHub Actions Workflow (quality.yml)
├─ actions/checkout (fetch-depth: 0 for PR diffs)
├─ .ci/quality/run.sh (master orchestrator)
│  ├─ Semgrep (SAST) → semgrep.sarif
│  ├─ Trivy (dependencies/containers) → trivy-fs.sarif
│  ├─ Gitleaks (secrets) → gitleaks.sarif
│  ├─ Hadolint (Dockerfile) → hadolint-*.json
│  └─ Biome (TypeScript/JavaScript linting) → biome.sarif (native --reporter=sarif)
├─ .ci/quality/merge-sarif.sh (deduplicate + merge all SARIF)
│  └─ .ci/quality/reports/quality.sarif (single canonical report)
├─ .ci/quality/gate.sh (evaluate findings; fail/warn based on severity)
├─ reviewdog (report merged findings to PR as GitHub checks)
└─ github/codeql-action (optional Code Scanning upload)
```

## Enabled Scanners

### SAST (Semgrep)

**Purpose:** Find security vulnerabilities, injection flaws, and code-quality issues.

**Why Semgrep:** Lightweight, language-agnostic, rules are version-controlled locally (no external fetches).

**What it scans:**
- TypeScript/JavaScript (apps/web, packages/*)
- Rust (services/control-plane, services/agent-runner, xtask)
- Dockerfiles
- YAML/JSON configuration

**Rules location:** `.ci/rules/semgrep/` (categorized by security, reliability, best-practices)

**Typical findings:**
- Unsafe eval(), Function() constructors
- SQL injection (template literals without parameterization)
- Hardcoded secrets
- Unsafe Rust blocks without documentation
- Unhandled panics or unwrap()

**Example PR output:**
```
semgrep.security.eval: Unsafe eval() detected
  └─ apps/web/src/components/Dynamic.tsx:42
```

### Dependency & Container Scanning (Trivy)

**Purpose:** Detect vulnerable dependencies and container images.

**Why Trivy:** Fast, minimal dependencies, supports offline mode with pre-cached DBs.

**What it scans:**
- `package.json` / `pnpm-lock.yaml` (JavaScript dependencies)
- `Cargo.lock` (Rust crates)
- Dockerfiles and built images (if image references are detected)
- System packages in base images

**Offline mode:**
- Uses `--skip-db-update --skip-java-db-update --skip-check-update --skip-version-check`
- Requires pre-cached vulnerability databases on runner (`$HOME/.cache/trivy/db/`)
- Database is updated **outside** the workflow (daily maintenance job or manual runner setup)

**Typical findings:**
```
trivy.cve-2024-12345: CVE-2024-12345 (HIGH)
  └─ Cargo.lock: dependency 'serde' 1.0.0 → update to 1.0.1
```

### Secret Scanning (Gitleaks)

**Purpose:** Detect leaked credentials, API keys, and passwords.

**What it scans:**
- Git commit history (full history on default branch; PR range on PRs)
- Patterns for: GitHub tokens, AWS keys, private keys, database passwords, etc.

**Behavior:**
- **On PRs:** Scans only new commits (`merge-base..HEAD`) to find secrets introduced by this PR
- **On main branch:** Scans full history (for auditing)
- **On scheduled runs:** Performs thorough historical scans

**Typical findings:**
```
gitleaks.secrets: GitHub Token detected
  └─ services/control-plane/config.rs:10 (CONFIRMED)
```

### Infrastructure/Configuration Scanning (Hadolint)

**Purpose:** Lint Dockerfiles for best practices and portability issues.

**What it scans:**
- All `Dockerfile*` files in the repository
- Detects: missing HEALTHCHECK, non-root users, shell pitfalls, etc.

**Typical findings:**
```
hadolint.DL4006: Set the SHELL option -o pipefail before RUN with a pipe
  └─ apps/web/Dockerfile:15
```

### JavaScript/TypeScript Linting (Biome)

**Purpose:** Enforce code style, catch common mistakes, and ensure formatting consistency.

**Configuration:** Defined in `biome.json` (workspace root)

**What it checks:**
- ESLint-compatible linting rules (recommended rules enabled)
- Unused variables and imports
- Dead code
- Type compatibility (via TypeScript integration)

**Typical findings:**
```
biome.js.correctness.noUnusedVariables: Variable 'x' is declared but never used
  └─ apps/web/src/lib/utils.ts:42
```

## Running Locally

To run quality checks on your local machine:

```bash
cd /path/to/lightbridge-code-intelligence
bash .ci/quality/run.sh
```

**Requirements:**
- All scanners installed and in PATH (see `.ci/baselines/runner-manifest.md`)
- Trivy DBs cached in `$HOME/.cache/trivy/db/`
- Biome installed via `pnpm install`

**Output:**
```
$ bash .ci/quality/run.sh

=== Verifying scanner availability ===
=== Running scanners ===
Running: semgrep-sast
✓ semgrep reported to .ci/quality/reports/semgrep.sarif
Running: trivy-fs
✓ trivy reported to .ci/quality/reports/trivy-fs.sarif
...
=== Merging SARIF reports ===
✓ Merged 5 SARIF file(s) into quality.sarif (23 total findings)
=== Quality gate evaluation ===
✓ All quality checks passed.
```

**Interpreting output:**
- Exit code 0 = no actionable findings
- Exit code 2 = new error-level findings on PR (gate failed)
- Exit code 1 = scanner configuration or environment error

## PR Quality Gate Behavior

### On Pull Requests

**Policy:**
- Report **all** findings to the PR via GitHub checks (reviewdog)
- **Fail the PR** only for **NEW error-level findings** (not the repository backlog)
- Warn (do not fail) for findings with level `warning` or `note`
- Always treat as errors:
  - Confirmed secret leaks
  - Critical/High security findings (CVE/CWE)

**Reviewer workflow:**
1. Author opens PR
2. Quality workflow runs; results appear as a "Quality Gate" check
3. If the check fails (red ✗):
   - Reviewer sees the specific error-level findings in the check details
   - Author fixes them and pushes
   - Workflow re-runs automatically
4. If the check passes (green ✓):
   - PR can be merged (no quality blocker)

**Visualization:**
```
✓ Quality Gate
  └─ Quality Gate · quality-pipeline (passed)
     Merged 4 SARIF file(s)...
     Errors: 0, Warnings: 2, Notes: 1
     ✓ No new error-level findings.
```

### On Default Branch (main)

**Policy:**
- Perform **full scans** (no PR range restrictions for secrets)
- Report all findings as **artifacts** (for dashboarding, compliance auditing)
- **Do not fail** the workflow (gate is informational only)
- Optionally upload to GitHub Code Scanning for centralized UI

**Use case:** Maintainers and security teams review historical scans and track remediation over time.

### Scheduled Scans (Weekly)

**Schedule:** Monday 02:00 UTC (weekly)

**Behavior:**
- Full codebase scan (same as default branch push)
- Reports saved as artifacts for trending analysis
- Useful for detecting stale findings and monitoring repository health

## Triaging & Suppressing Findings

### Quick Suppression (Explicit Baseline)

For findings that are **not actionable** (false positives, accepted risks), add a suppression:

1. Edit `.ci/baselines/suppressions.json`:
   ```json
   {
     "suppressions": [
       {
         "ruleId": "semgrep.security.eval",
         "path": "apps/web/src/dynamic-loader.ts",
         "reason": "Legitimate eval() in trusted admin context only; no untrusted input.",
         "expiresAt": "2026-12-31T23:59:59Z",
         "approved_by": "security@company.com",
         "ticket": "#TICKET-123"
       }
     ]
   }
   ```

2. Commit the suppression in your PR.

3. The gate will exclude this finding from the pass/fail decision.

**Important:** Every suppression must include:
- **reason**: Why this finding is not actionable (required for audit trail)
- **expiresAt**: ISO 8601 timestamp (forces periodic review; ~90 days out recommended)
- **ticket**: Reference to a GitHub issue, discussion, or decision record

### Updating Semgrep Rules

To reduce false positives, refine Semgrep rules:

1. Identify the rule ID from the finding (e.g., `semgrep.security.eval`)
2. Edit the corresponding rule file in `.ci/rules/semgrep/`
3. Add patterns, exclusions, or filters to increase precision
4. Test locally: `semgrep --config=.ci/rules/semgrep apps/web`
5. Commit and re-run the workflow

**Example:** Refining the eval() rule to exclude trusted loaders:
```yaml
  - id: typescript-no-eval
    pattern-either:
      - pattern: eval(...)
      - pattern: Function(...)
    pattern-not: |
      eval($X) where $X is from AdminLoader
    message: "Unsafe eval() detected"
    languages: [typescript]
    severity: ERROR
```

### Disabling a Scanner (Rare)

To skip a scanner entirely (e.g., if a tool keeps failing):

**Option 1: Disable in run.sh**
```bash
# Comment out the scanner block in .ci/quality/run.sh
# if command -v semgrep &>/dev/null; then
#   run_scanner "semgrep-sast" ...
# fi
```

**Option 2: Skip via environment variable** (if .ci/quality/run.sh is extended to support it)
```bash
SKIP_SEMGREP=true bash .ci/quality/run.sh
```

**Important:** Do not disable a scanner to hide findings. Instead:
- Suppress specific findings in `.ci/baselines/suppressions.json`
- File an issue if the scanner is misconfigured
- Update the runner provisioning if the scanner is missing

## Updating Tools & Databases

### Semgrep Rules

**Add new rules:**
1. Create `.yaml` file in `.ci/rules/semgrep/<category>/`
2. Test locally
3. Commit to repository
4. Rules are picked up automatically on next workflow run

**Update existing rules:**
1. Edit the `.yaml` file
2. Test locally: `semgrep --config=.ci/rules/semgrep <path>`
3. Commit

**Remove rules:**
1. Delete the `.yaml` file (or comment out rules within it)
2. Commit

### Trivy Vulnerability Databases

**Update frequency:** Daily (outside workflow runs)

**Manual update:**
```bash
# SSH into the self-hosted runner and run:
trivy image --download-db-only

# Verify:
ls -lh ~/.cache/trivy/db/
```

**Automation:** Set up a daily cron job or scheduled GitHub Actions workflow on the runner host to keep DBs fresh.

**Scheduled maintenance workflow** (example, runs daily on runner):
```yaml
name: Update Trivy DBs
on:
  schedule:
    - cron: "0 3 * * *"  # 03:00 UTC daily
jobs:
  update:
    runs-on: [self-hosted, vymalo-vps]
    steps:
      - name: Update Trivy vulnerability databases
        run: trivy image --download-db-only
```

### Scanner Binary Versions

To update a scanner (e.g., Semgrep, Trivy):

1. **Update the manifest:** Edit `.ci/baselines/runner-manifest.md` with new version + checksum
2. **SSH into runner:** Install new version (commands in manifest)
3. **Verify:** Run `semgrep --version && trivy --version` etc.
4. **Test locally:** Run `.ci/quality/run.sh` to ensure compatibility
5. **Commit the manifest update** to document the change

### GitHub Actions Versions

To update a GitHub Action (e.g., actions/checkout):

1. Check the latest release on the action's repository
2. Identify the commit SHA for the desired version
3. Update `.github/workflows/quality.yml` (replace the pinned SHA + add a comment with the release version)
4. Test in a PR (CI will run the workflow)
5. Merge

**Example:**
```yaml
# Before:
- uses: actions/checkout@11bd71901afe44af187055612f316e63f77d3e91  # v4.2.0

# After:
- uses: actions/checkout@44c6b3ce3cc3e1c1fb1890f87666b007a1c4d475  # v4.2.1
```

## Code Scanning Integration (Optional)

GitHub Code Scanning provides a centralized UI for tracking security findings across the repository and over time.

### Enabling Code Scanning Upload

1. Set repository variable `ENABLE_CODE_SCANNING=true` (via Settings > Variables)
2. Quality workflow will automatically upload merged SARIF to GitHub Code Scanning on main branch pushes

### Accessing Code Scanning Results

1. Navigate to repository **Security** tab → **Code Scanning**
2. View findings by severity, file, rule
3. Dismiss findings (marks as resolved; does not suppress in this workflow)
4. Track remediation progress over time

### Limitations

- GitHub Code Scanning shows **all** findings (not just new ones on PRs)
- Dismissals in GitHub UI are independent of `.ci/baselines/suppressions.json`
- This replaces SonarQube's centralized dashboard, but workflow-level suppressions remain the source of truth

## Limitations & Known Issues

1. **Baseline comparisons:** The current gate does not compare against a baseline of "known" findings. All error-level findings fail the PR. To suppress, use `.ci/baselines/suppressions.json`.

2. **Trivy DB staleness:** If Trivy DBs are not updated for >7 days, newly disclosed vulnerabilities may not be detected. Set up daily DB updates on the runner.

3. **Semgrep rule coverage:** Rules are curated manually. Not all security patterns are covered. Complement with security reviews and penetration testing.

4. **Hadolint JSON output:** Hadolint does not natively output SARIF. JSON results are stored but not merged into the main quality.sarif. To include in PR checks, a JSON-to-SARIF converter would be needed.

5. **Multi-language limitations:** Some rules may not work across all code paths (e.g., TypeScript rules only apply to `.ts/.tsx` files).

## Architecture: SonarQube CE Replacement Mapping

| SonarQube CE Capability | Replacement |
|---|---|
| **SAST scanning** | Semgrep (local rules, `.ci/rules/semgrep/`) |
| **Dependency scanning** | Trivy (filesystem scanning, offline DBs) |
| **Secret detection** | Gitleaks (built-in patterns, local history) |
| **Code metrics (complexity, duplication)** | *(Not yet implemented; can be added with jscpd or Lizard)* |
| **Test coverage tracking** | Rust: `cargo llvm-cov` (existing `rust-lint.yml`) |
| **Quality gates** | Custom gate in `.ci/quality/gate.sh` |
| **PR integration** | reviewdog (GitHub PR checks) |
| **Centralized dashboard** | GitHub Code Scanning (optional, via codeql-action) |
| **Historical trending** | GitHub Code Scanning or artifact retention |

## Troubleshooting

### Workflow fails: "Scanner X not found"

**Cause:** Tool is not installed on the runner.

**Fix:** SSH into runner and install per `.ci/baselines/runner-manifest.md`. Restart the runner or re-run the workflow.

### Trivy reports "Database file does not exist"

**Cause:** Trivy DB was never cached.

**Fix:** Run `trivy image --download-db-only` on the runner once, then re-run the workflow.

### Reviewdog reports "No findings" even though quality.sarif has results

**Cause:** SARIF parsing error or filter-mode exclusion.

**Fix:** Verify SARIF structure: `jq '.runs[0].results | length' .ci/quality/reports/quality.sarif`

### Suppression not applied; finding still appears on PR

**Cause:** Suppression syntax error or mismatched ruleId/path.

**Fix:** 
1. Verify JSON syntax in `suppressions.json`: `jq . .ci/baselines/suppressions.json`
2. Check that ruleId and path match exactly (case-sensitive)
3. Commit the update and re-run the workflow

## Future Enhancements

1. **Baseline DB:** Track known findings over time; only fail on net-new issues
2. **Code duplication:** Integrate jscpd or Lizard for duplicate code detection
3. **Complexity metrics:** Add McCabe complexity thresholds
4. **Container image scanning:** Add Trivy image scanning (requires runner to build images)
5. **Incident response:** Automated rollback/notification on critical findings

## References

- [Semgrep Documentation](https://semgrep.dev/docs/)
- [Trivy Scanning Documentation](https://aquasecurity.github.io/trivy/)
- [Gitleaks Documentation](https://gitleaks.io/)
- [Hadolint Rules](https://github.com/hadolint/hadolint/wiki/Rules)
- [Reviewdog Documentation](https://github.com/reviewdog/reviewdog)
- [SARIF Specification](https://sarifweb.azurewebsites.net/)
- [GitHub Code Scanning](https://docs.github.com/en/code-security/code-scanning)

## Support & Questions

For questions or issues with the quality pipeline:
1. Check this document and `.ci/quality/README.md`
2. Review runner-manifest.md for provisioning steps
3. File an issue with logs from `.ci/quality/reports/`
