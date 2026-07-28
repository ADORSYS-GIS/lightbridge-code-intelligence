# Self-Hosted Runner Provisioning Manifest

This document specifies all required tools, databases, and configurations for offline-capable quality scanning on the self-hosted `vymalo-vps` runner pool.

## Current Runner Configuration

**Label(s)**: `[self-hosted, vymalo-vps]`

**Base image**: `ghcr.io/vymalo/arc-runners:jdk21-node22` (already includes rustup, cargo, cargo-llvm-cov, pnpm)

**Provisioning**: Manual pre-cache on the runner; GitHub Actions workflows pass offline flags to prevent runtime downloads.

---

## Tool Inventory

**⚠️ Version Update Guidance**: The versions below reflect the latest releases as of February 2025. Before deploying, check GitHub release pages for newer versions. See "Checking for Latest Versions" section below.

### 1. Semgrep (SAST)

- **Recommended Version**: 1.45.0+ (check https://github.com/returntocorp/semgrep/releases for latest)
- **Installation**: `pip install semgrep==<VERSION>` (or from package manager)
- **Offline mode**: No external downloads. Rules are provided locally in `.ci/rules/semgrep/`.
- **Validation**:
  ```bash
  semgrep --version
  ```
- **Rule pack updates**: Add new `.yaml` or `.json` rule files to `.ci/rules/semgrep/`, commit, and run full scan.

### 2. Trivy (Dependency & Container Scanning)

- **Recommended Version**: v0.72.0 (check https://github.com/aquasecurity/trivy/releases for latest — verified against the API at time of writing, not assumed from memory)
- **Binary installation**: the workflow installs the trivy *binary* itself via
  `aquasecurity/setup-trivy` (pinned commit, `cache: true`), so it's a cache hit — no network
  fetch — on every run after the first for a given pinned version. This does **not** replace the
  database provisioning below; the binary and the vulnerability database are provisioned
  independently.
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

- **Recommended Version**: 8.18.0+ (check https://github.com/gitleaks/gitleaks/releases for latest)
- **Installation**: `brew install gitleaks` (macOS) or download binary
- **Binary location**: `/usr/local/bin/gitleaks`
- **Offline mode**: Uses built-in pattern library; no external rule downloads.
- **Validation**:
  ```bash
  gitleaks version
  ```

### 4. Hadolint (Dockerfile Linting)

- **Recommended Version**: 2.12.0+ (check https://github.com/hadolint/hadolint/releases for latest)
- **Installation**: `brew install hadolint` (macOS) or download binary
- **Binary location**: `/usr/local/bin/hadolint`
- **Offline mode**: Uses built-in rules; no external downloads.
- **Validation**:
  ```bash
  hadolint --version
  ```

### 5. Reviewdog (PR Reporting)

- **Recommended Version**: 0.17.0+ (check https://github.com/reviewdog/reviewdog/releases for latest)
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

- **Recommended Version**: 2.0.0+ (pinned in `package.json` devDependencies; check current with `pnpm exec biome --version`)
- **Installation**: Managed by pnpm. Run `pnpm install` in the repository root.
- **Offline mode**: Uses workspace-local configuration (biome.json).
- **Invoked via**: `pnpm exec biome check .`
- **Validation**:
  ```bash
  pnpm exec biome --version
  ```

---

## Checking for Latest Versions

Before provisioning the runner, verify the latest tool releases:

```bash
# Semgrep: https://github.com/returntocorp/semgrep/releases/latest
curl -s https://api.github.com/repos/returntocorp/semgrep/releases/latest | jq -r '.tag_name'

# Trivy: https://github.com/aquasecurity/trivy/releases/latest
curl -s https://api.github.com/repos/aquasecurity/trivy/releases/latest | jq -r '.tag_name'

# Gitleaks: https://github.com/gitleaks/gitleaks/releases/latest
curl -s https://api.github.com/repos/gitleaks/gitleaks/releases/latest | jq -r '.tag_name'

# Hadolint: https://github.com/hadolint/hadolint/releases/latest
curl -s https://api.github.com/repos/hadolint/hadolint/releases/latest | jq -r '.tag_name'

# Reviewdog: https://github.com/reviewdog/reviewdog/releases/latest
curl -s https://api.github.com/repos/reviewdog/reviewdog/releases/latest | jq -r '.tag_name'
```

Update the versions in this manifest and the GitHub Actions workflow before deploying if any newer releases are available.

---

## External Dependencies

### GitHub Actions

Used in `.github/workflows/quality.yml` (all pinned to commit SHAs for reproducibility):

1. **actions/checkout**
   - **Current Version**: v4.2.0 (SHA: `11bd71901afe44af187055612f316e63f77d3e91`)
   - **Latest Releases**: https://github.com/actions/checkout/releases
   - **Purpose**: Check out repository code
   - **Check for updates**:
     ```bash
     curl -s https://api.github.com/repos/actions/checkout/releases/latest | jq -r '.tag_name'
     ```

2. **actions/upload-artifact**
   - **Current Version**: v4.7.0 (SHA: `97a0fdd30d330287a78c2239ea01e15720c0e5a3`)
   - **Latest Releases**: https://github.com/actions/upload-artifact/releases
   - **Purpose**: Upload quality reports as CI artifacts
   - **Check for updates**:
     ```bash
     curl -s https://api.github.com/repos/actions/upload-artifact/releases/latest | jq -r '.tag_name'
     ```

3. **github/codeql-action/upload-sarif**
   - **Current Version**: v3.25.0 (SHA: `9fa7e86e37b0d1da3b80c8a1b9a2f0ede7e8d9e0`)
   - **Conditional**: Only runs if `vars.ENABLE_CODE_SCANNING == 'true'`
   - **Latest Releases**: https://github.com/github/codeql-action/releases
   - **Purpose**: Upload merged SARIF to GitHub Code Scanning
   - **Check for updates**:
     ```bash
     curl -s https://api.github.com/repos/github/codeql-action/releases/latest | jq -r '.tag_name'
     ```

**Pinning strategy**: All actions are pinned to full commit SHAs (not floating tags like `@v4` or `@latest`) to ensure reproducibility, security, and offline operation. Commit-level pinning prevents unexpected behavior changes from new releases.

**Updating GitHub Actions**:
1. Check the latest release for each action using the links above
2. Find the commit SHA for the target version in the release page
3. Update `.github/workflows/quality.yml` with the new SHA and version comment
4. Test the workflow on a feature branch before merging

---

## Runner Setup Checklist

### Initial Provisioning

- [ ] Ensure runner has `vymalo-vps` label applied
- [ ] Verify base image includes: rustup (stable), cargo-llvm-cov, pnpm, Node.js 22+
- [ ] Install Semgrep: `pip install semgrep==1.45.0` (or latest stable — verify against
      https://github.com/returntocorp/semgrep/releases before installing)
- [ ] Install Trivy on the runner HOST itself (separate from the in-workflow binary — this copy is
      only needed so the host's own cron job can pre-cache the vulnerability database; the
      workflow provisions its own trivy binary via the pinned `aquasecurity/setup-trivy` action,
      see `.github/workflows/quality.yml`): install per
      https://aquasecurity.github.io/trivy/latest/getting-started/installation/, pinned to a
      specific release verified against https://github.com/aquasecurity/trivy/releases (do not
      assume the version number in this doc is current — check first)
- [ ] Install Gitleaks: verify latest at https://github.com/gitleaks/gitleaks/releases, then
      download the matching `gitleaks-linux-x64` (or platform-appropriate) release asset and place
      on PATH
- [ ] Install Hadolint: verify latest at https://github.com/hadolint/hadolint/releases, then
      download the matching release asset and place on PATH
- [ ] Install Reviewdog: verify latest at https://github.com/reviewdog/reviewdog/releases before
      installing
- [ ] Pre-cache Trivy DBs on the runner host: `trivy image --download-db-only`
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
