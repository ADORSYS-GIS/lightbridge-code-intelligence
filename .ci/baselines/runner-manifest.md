# Self-Hosted Runner Provisioning Manifest

This document specifies all required tools, databases, and configurations for offline-capable quality scanning on the self-hosted `vymalo-vps` runner pool.

## Current Runner Configuration

**Label(s)**: `[self-hosted, vymalo-vps]`

**Base image**: `ghcr.io/vymalo/arc-runners:jdk21-node22` (already includes rustup, cargo, cargo-llvm-cov, pnpm)

**Provisioning**: Manual pre-cache on the runner; GitHub Actions workflows pass offline flags to prevent runtime downloads.

---

## Tool Inventory

### 1. Semgrep (SAST)

- **Version**: 1.45.0+
- **Installation**: `pip install semgrep` (or from package manager)
- **Offline mode**: No external downloads. Rules are provided locally in `.ci/rules/semgrep/`.
- **Validation**:
  ```bash
  semgrep --version
  ```
- **Rule pack updates**: Add new `.yaml` or `.json` rule files to `.ci/rules/semgrep/`, commit, and run full scan.

### 2. Trivy (Dependency & Container Scanning)

- **Version**: 0.51.0+
- **Installation**: Download from official releases (no auto-update in workflow)
- **Binary location**: `/usr/local/bin/trivy`
- **Offline flags used in workflow**:
  - `--skip-db-update` — Do not check for newer DB
  - `--skip-java-db-update` — Do not fetch Java vulnerability DB
  - `--skip-check-update` — Do not fetch policy updates
  - `--skip-version-check` — Do not check for newer Trivy version
- **Database cache**:
  - Location: `$HOME/.cache/trivy/db/` (on the runner)
  - Update frequency: Daily (manual maintenance, not during workflow runs)
  - Size: ~600 MB (varies)
  - Initialize with: `trivy image --download-db-only` (run once during runner setup)

#### Trivy Database Update Process

On the self-hosted runner, run the following **outside workflow execution** (e.g., via a daily scheduled job or manual intervention):

```bash
# Update Trivy's vulnerability databases (run once per day).
trivy image --download-db-only

# Verify the DB is fresh.
ls -lh ~/.cache/trivy/db/
```

This is **not** part of the GitHub Actions workflow; databases are expected to be pre-cached.

### 3. Gitleaks (Secret Scanning)

- **Version**: 8.18.0+
- **Installation**: `brew install gitleaks` (macOS) or download binary
- **Binary location**: `/usr/local/bin/gitleaks`
- **Offline mode**: Uses built-in pattern library; no external rule downloads.
- **Validation**:
  ```bash
  gitleaks version
  ```

### 4. Hadolint (Dockerfile Linting)

- **Version**: 2.12.0+
- **Installation**: `brew install hadolint` (macOS) or download binary
- **Binary location**: `/usr/local/bin/hadolint`
- **Offline mode**: Uses built-in rules; no external downloads.
- **Validation**:
  ```bash
  hadolint --version
  ```

### 5. Reviewdog (PR Reporting)

- **Version**: 0.17.0+
- **Installation**: `go install github.com/reviewdog/reviewdog/cmd/reviewdog@latest`
- **Binary location**: `~/go/bin/reviewdog` (or in PATH)
- **Requires**:
  - `GITHUB_TOKEN` environment variable (provided by GitHub Actions automatically)
  - GitHub check/review write permissions on the repository
- **Offline mode**: Does not download rules or databases. Requires GitHub API access (acceptable per requirements).
- **Validation**:
  ```bash
  reviewdog --version
  ```

### 6. Biome (JavaScript/TypeScript Linting)

- **Version**: 2.0.0+ (from `package.json` devDependencies)
- **Installation**: Managed by pnpm. Run `pnpm install` in the repository root.
- **Offline mode**: Uses workspace-local configuration (biome.json).
- **Invoked via**: `pnpm exec biome check .`
- **Validation**:
  ```bash
  pnpm exec biome --version
  ```

---

## External Dependencies

### GitHub Actions

Used in `.github/workflows/quality.yml`:

1. **actions/checkout@v4**
   - SHA: `11bd71901afe44af187055612f316e63f77d3e91`
   - Release: v4.2.0
   - Purpose: Check out repository code

2. **actions/upload-artifact@v4**
   - SHA: `97a0fdd30d330287a78c2239ea01e15720c0e5a3`
   - Release: v4.7.0
   - Purpose: Upload quality reports as CI artifacts

3. **github/codeql-action/upload-sarif@v3** (conditional, only if `vars.ENABLE_CODE_SCANNING == 'true'`)
   - SHA: `9fa7e86e37b0d1da3b80c8a1b9a2f0ede7e8d9e0`
   - Release: v3.25.0
   - Purpose: Upload merged SARIF to GitHub Code Scanning

All actions are pinned to full commit SHAs to ensure reproducibility and offline operation. Checksum verification is not strictly necessary for GitHub-hosted actions, but commit-level pinning prevents supply-chain surprises.

---

## Runner Setup Checklist

### Initial Provisioning

- [ ] Ensure runner has `vymalo-vps` label applied
- [ ] Verify base image includes: rustup (stable), cargo-llvm-cov, pnpm, Node.js 22+
- [ ] Install Semgrep: `pip install semgrep==1.45.0` (or latest stable)
- [ ] Install Trivy: `curl -sfL https://raw.githubusercontent.com/aquasecurity/trivy/main/contrib/install.sh | sh -s -- -b /usr/local/bin v0.51.0`
- [ ] Install Gitleaks: `curl -L https://github.com/gitleaks/gitleaks/releases/download/v8.18.0/gitleaks-linux-x64 -o /usr/local/bin/gitleaks && chmod +x /usr/local/bin/gitleaks`
- [ ] Install Hadolint: `curl -L https://github.com/hadolint/hadolint/releases/download/v2.12.0/hadolint-Linux-x86_64 -o /usr/local/bin/hadolint && chmod +x /usr/local/bin/hadolint`
- [ ] Install Reviewdog: `go install github.com/reviewdog/reviewdog/cmd/reviewdog@v0.17.0`
- [ ] Pre-cache Trivy DBs: `trivy image --download-db-only`
- [ ] Verify all tools: `semgrep --version && trivy --version && gitleaks version && hadolint --version && reviewdog --version`

### Daily Maintenance

- [ ] Update Trivy DBs (run on a schedule outside of workflow execution):
  ```bash
  trivy image --download-db-only
  ```

### Monthly Review

- [ ] Check for tool version updates (subscribe to release feeds if possible)
- [ ] Audit suppressions in `.ci/baselines/suppressions.json` for expired entries
- [ ] Review quality pipeline documentation if policies change

---

## Known Limitations & Workarounds

1. **Trivy database staleness**: If scans are run without updating Trivy DBs for >7 days, findings may lag behind newly disclosed vulnerabilities. Schedule daily DB updates via a separate job or trigger.

2. **Semgrep rules**: Repository-local rules in `.ci/rules/semgrep/` are version-controlled and static. To update community rules, manually fetch new `.yaml` files from the Semgrep registry and commit them.

3. **Gitleaks pattern library**: Built-in patterns are frozen at gitleaks binary release time. To use newer patterns, upgrade the binary.

4. **CodeQL**: Not yet enabled. If enabled in future, a CodeQL query pack must be pre-cached on the runner (similar to Trivy DBs). This is a manual setup step.

---

## Troubleshooting

### "Scanner X not found" Error in Workflow

**Symptom**: Workflow fails with `Scanner 'semgrep' not found: 'semgrep' not in PATH`.

**Cause**: Tool is not installed or not in PATH on the runner.

**Fix**:
1. SSH into the runner (if accessible).
2. Verify tool is installed: `which semgrep`
3. If missing, install using the checklist above.
4. Verify PATH: `echo $PATH | grep -q /usr/local/bin && echo OK || echo FAIL`
5. Restart the runner or re-run the workflow.

### Trivy Reports "Database file does not exist"

**Symptom**: Trivy exits with "database file does not exist" even though `--skip-db-update` is set.

**Cause**: Trivy DB was never initialized, or cache directory was purged.

**Fix**:
1. SSH into the runner.
2. Run: `trivy image --download-db-only`
3. Verify: `ls ~/.cache/trivy/db/ | grep db.tar.gz`
4. Re-run the workflow.

### Gitleaks or Hadolint Not Installed

**Symptom**: Workflow warns "Gitleaks not available" and skips secret scanning.

**Cause**: Binary not in PATH or not installed.

**Fix**:
1. Use the installation commands in the inventory above.
2. Verify: `which gitleaks && gitleaks version`
3. Re-run the workflow.

---

## Version Pinning & Update Policy

- **Workflow actions**: Pinned to commit SHAs. Update once per quarter or when a critical fix is released.
- **Semgrep**: Pinned in `.github/workflows/quality.yml`. Update on release of new major/minor versions.
- **Trivy**: Pinned in workflow script. Update monthly or on critical fixes.
- **Gitleaks**: Pinned in workflow script. Update quarterly or on critical fixes.
- **Biome**: Pinned in `package.json` (via pnpm-lock.yaml). Update via `pnpm upgrade`.

**Update procedure**:
1. Test new version locally or in a feature branch.
2. Update the version in the manifest and workflow.
3. Create a PR with governance template (source of truth: RFC or release notes).
4. Merge and deploy.

---

## Cost & Performance

- **Scan time**: ~2–4 minutes per PR (varies by codebase size).
- **Trivy DB size**: ~600 MB (one-time download, then cached).
- **Biome**: Already cached via pnpm-lock.yaml; negligible additional cost.
- **Semgrep**: Fast; no external I/O during scan.
- **Gitleaks**: Fast on PR diffs; slower on full history scans (scheduled jobs).

---

## References

- [Trivy Offline Documentation](https://aquasecurity.github.io/trivy/latest/advanced/offline-scanning/)
- [Semgrep CLI Reference](https://semgrep.dev/docs/cli-reference/)
- [Gitleaks Documentation](https://gitleaks.io/)
- [Hadolint Rules](https://github.com/hadolint/hadolint/wiki/Rules)
- [Reviewdog Documentation](https://github.com/reviewdog/reviewdog)
