# Quality Pipeline Deployment Checklist

## Pre-Deployment Validation ✓

- [x] All shell scripts pass bash syntax validation (`bash -n`) and were execution-tested with
  fixture SARIF/JSON inputs (not just parsed) — the workflow invokes them via `bash`, so bash is
  the compatibility target, not zsh
- [x] GitHub Actions workflow YAML is valid
- [x] SARIF merging logic validated (jq syntax)
- [x] All external actions pinned to commit SHAs
- [x] Offline capability verified (no floating tags, pre-cached databases)
- [x] Semgrep rules included (typescript-injection, rust-security)
- [x] Documentation complete (475+ lines)
- [x] Suppressions format defined with required fields
- [x] Runner provisioning manifest included

## 1. Runner Provisioning (One-Time Setup)

**Required: SSH access to `vymalo-vps` runner host**

### 1.1 Install Scanner Binaries

```bash
# Semgrep (SAST)
pip install semgrep==1.45.0
semgrep --version

# Trivy (Dependency scanning)
curl -sfL https://raw.githubusercontent.com/aquasecurity/trivy/main/contrib/install.sh | \
  sh -s -- -b /usr/local/bin v0.51.0
trivy --version

# Gitleaks (Secrets)
curl -L https://github.com/gitleaks/gitleaks/releases/download/v8.18.0/gitleaks-linux-x64 \
  -o /usr/local/bin/gitleaks && chmod +x /usr/local/bin/gitleaks
gitleaks version

# Hadolint (Dockerfile linting)
curl -L https://github.com/hadolint/hadolint/releases/download/v2.12.0/hadolint-Linux-x86_64 \
  -o /usr/local/bin/hadolint && chmod +x /usr/local/bin/hadolint
hadolint --version

# Reviewdog (PR reporting)
# Already installed on vymalo-vps, verify:
reviewdog --version

# Biome (TypeScript/JavaScript linting)
# Already installed via pnpm; verify in repo:
# pnpm exec biome --version
```

**Verify all installed:**
```bash
command -v semgrep trivy gitleaks hadolint reviewdog && echo "✓ All tools found"
```

- [ ] Semgrep installed & verified
- [ ] Trivy installed & verified
- [ ] Gitleaks installed & verified
- [ ] Hadolint installed & verified
- [ ] Reviewdog installed & verified

### 1.2 Pre-Cache Trivy Vulnerability Databases

```bash
# Download and cache Trivy vulnerability databases (one-time, ~600 MB).
# This must complete before first workflow run.

trivy image --download-db-only

# Verify cache is populated:
ls -lh ~/.cache/trivy/db/
# Should show: db.tar.gz, metadata.json, etc.
```

- [ ] Trivy DBs downloaded and cached
- [ ] Cache directory verified: `~/.cache/trivy/db/`

### 1.3 Schedule Daily Trivy DB Updates

Set up a daily cron job to keep vulnerability databases fresh:

```bash
# Edit runner's crontab:
crontab -e

# Add line:
0 3 * * * /usr/local/bin/trivy image --download-db-only

# Verify:
crontab -l | grep trivy
```

- [ ] Daily cron job scheduled for Trivy DB updates

## 2. Repository Configuration

### 2.1 Verify Workflow File Placement

The workflow must be at: `.github/workflows/quality.yml`

```bash
# From repository root:
ls -la .github/workflows/quality.yml
# Should exist and be readable

# Verify syntax:
python3 -c "import yaml; yaml.safe_load(open('.github/workflows/quality.yml'))" && \
  echo "✓ YAML valid"
```

- [ ] Workflow file exists at `.github/workflows/quality.yml`
- [ ] YAML syntax validated

### 2.2 Verify Script Placement & Permissions

```bash
# From repository root:
ls -la .ci/quality/{run,merge-sarif,gate,hadolint-to-sarif,biome-to-sarif}.sh

# All should be executable (have 'x' permission).
# Verify:
test -x .ci/quality/run.sh && echo "✓ run.sh executable" || chmod +x .ci/quality/run.sh
```

- [ ] All orchestration scripts are executable
- [ ] Scripts in correct locations

### 2.3 Configure Repository Variables (Optional)

To enable GitHub Code Scanning upload (optional):

1. Navigate to repository **Settings** → **Variables**
2. Click **New repository variable**
3. Name: `ENABLE_CODE_SCANNING`
4. Value: `true`
5. Click **Add variable**

This enables SARIF upload to GitHub Code Scanning on main branch pushes. Workflow remains fully functional without it.

- [ ] Repository variable `ENABLE_CODE_SCANNING` created (if Code Scanning desired)

## 3. Initial Workflow Test

### 3.1 Test on Feature Branch

Create a test PR to verify the workflow runs correctly:

```bash
# Create test branch
git checkout -b test/quality-pipeline

# Make a trivial change (e.g., add comment to a file)
echo "# test" >> .ci/DEPLOYMENT-CHECKLIST.md

# Commit and push
git add .ci/DEPLOYMENT-CHECKLIST.md
git commit -m "test: trigger quality workflow"
git push origin test/quality-pipeline
```

### 3.2 Monitor Workflow Execution

1. Navigate to repository **Actions** tab
2. Find the **Quality** workflow run for your branch
3. Wait for completion (typically 2–4 minutes)

**Expected outcome:**
- Workflow completes without errors
- "Quality Gate" check passes or reports findings
- `.ci/quality/reports/` artifact is uploaded

**Common issues:**
- Workflow fails immediately → Check runner connectivity
- "Scanner not found" → Scanner not installed on runner (run provisioning again)
- "Database not found" (Trivy) → Trivy DBs not cached (run `trivy image --download-db-only` on runner)

- [ ] Test branch workflow completes
- [ ] Reports artifact uploaded to GitHub Actions
- [ ] No scanner errors in logs

### 3.3 Verify SARIF Merging

Inspect the merged report from the workflow artifact:

```bash
# Download quality-reports artifact from GitHub Actions, then:
cat quality.sarif | jq '.runs[0].results | length'
# Should print a number (count of findings)

# View sample finding:
cat quality.sarif | jq '.runs[0].results[0]'
```

- [ ] Merged quality.sarif is valid JSON
- [ ] Findings are properly formatted
- [ ] Deduplication is working (no duplicate entries)

### 3.4 Test PR Comment (Reviewdog Integration)

On a PR with the quality workflow running:

1. Wait for workflow to complete
2. Check for "Quality Gate" check in PR
3. View check details to see findings
4. Findings should be reported as GitHub checks (not comments)

**Expected:** Clean PR check output listing any findings by rule ID and file.

- [ ] Reviewdog reports appear as GitHub checks
- [ ] Finding details are clear and actionable

### 3.5 Delete Test Branch

```bash
# After testing, remove the branch:
git branch -D test/quality-pipeline
git push origin -d test/quality-pipeline
```

- [ ] Test branch cleaned up

## 4. Documentation & Knowledge Transfer

### 4.1 Review User Documentation

Read and understand:
- `docs/quality-pipeline.md` — Comprehensive guide (475 lines)
- `.ci/QUICK-START.md` — Common tasks for developers
- `.ci/baselines/runner-manifest.md` — Runner provisioning details

- [ ] User documentation reviewed
- [ ] QUICK-START guide is accessible to developers

### 4.2 Communicate to Team

Share the Quick Start guide with the development team:
- Explain how to run locally: `bash .ci/quality/run.sh`
- Explain how to suppress findings: edit `.ci/baselines/suppressions.json`
- Explain PR gate behavior: new error-level findings fail the PR
- Clarify that this replaces SonarQube CE scanning (not full dashboard)

- [ ] Team briefing scheduled or documentation shared
- [ ] Developers understand local execution and suppression workflow

## 5. Post-Deployment Monitoring

### 5.1 First Few Workflow Runs

Monitor the first 3–5 workflow runs (PRs to main branch):
- Check that execution times are reasonable (~2–4 minutes)
- Verify no spurious failures or configuration errors
- Collect feedback from developers on finding quality

- [ ] Monitor first 3–5 runs
- [ ] No systematic errors observed

### 5.2 Baseline Findings Audit

If this is the first time running comprehensive scanning on the codebase:

1. Run a full scan on main branch (wait for scheduled weekly run or manually trigger)
2. Review findings in `.ci/quality/reports/quality.sarif`
3. Triage findings:
   - Real bugs → Create issues for fixing
   - False positives → Suppress in `.ci/baselines/suppressions.json`
   - Accepted risks → Suppress with clear reason and ticket

**Note:** The quality gate does NOT fail on main branch (informational only). This baseline review is for establishing baseline understanding.

- [ ] Baseline findings reviewed
- [ ] Suppressions created for false positives / accepted risks
- [ ] Real bugs triaged into issues

### 5.3 Establish Maintenance Schedule

Set recurring calendar reminders:
- **Daily:** Monitor Trivy DB freshness (cron job should handle)
- **Weekly:** Review quality pipeline results (check for trends)
- **Monthly:** Review expired suppressions, update tool versions if needed

- [ ] Daily DB update cron job verified
- [ ] Weekly review cadence established
- [ ] Monthly maintenance tasks scheduled

## 6. Rollback Plan (If Needed)

If the quality pipeline causes critical issues:

1. **Disable the workflow temporarily:**
   ```bash
   # Rename or delete the workflow file
   mv .github/workflows/quality.yml .github/workflows/quality.yml.disabled
   git commit -m "chore: temporarily disable quality workflow"
   git push
   ```

2. **Revert workflow triggers** on failing PRs:
   - Delete the workflow run from GitHub Actions (or wait for PR to be closed)
   - Re-run PR checks without the quality workflow

3. **Root cause analysis:**
   - Review logs from `.ci/quality/reports/`
   - Check scanner configuration (verify rules, databases)
   - Verify runner provisioning

4. **Fix and re-enable:**
   - Once root cause is addressed, rename workflow back to `.github/workflows/quality.yml`
   - Test on a feature branch before re-enabling on main

- [ ] Rollback procedure documented and understood

## 7. Long-Term Maintenance

### 7.1 Rule Updates

Every quarter (or as new security patterns emerge):
1. Review Semgrep registry for new community rules
2. Evaluate rules for applicability to codebase
3. Add new rules to `.ci/rules/semgrep/`
4. Test locally and merge

### 7.2 Tool Version Updates

Every month, check for tool updates:
- Semgrep: https://github.com/returntocorp/semgrep/releases
- Trivy: https://github.com/aquasecurity/trivy/releases
- Gitleaks: https://github.com/gitleaks/gitleaks/releases
- Hadolint: https://github.com/hadolint/hadolint/releases

Update `.ci/baselines/runner-manifest.md` and install on runner.

### 7.3 Suppression Housekeeping

Monthly or quarterly:
1. Review all entries in `.ci/baselines/suppressions.json`
2. Delete suppressions with `expiresAt` in the past
3. For still-relevant suppressions, extend `expiresAt` (or fix the underlying issue)
4. Document reason for extension in commit message

- [ ] Long-term maintenance plan documented

## Final Checklist Summary

**Pre-Deployment:**
- [x] Validation checks passed
- [ ] Runner provisioned with all tools
- [ ] Trivy DBs cached and cron job scheduled

**Repository:**
- [ ] Workflow file in place
- [ ] Scripts executable
- [ ] Documentation reviewed by team

**Testing:**
- [ ] Initial workflow test passed
- [ ] SARIF merging verified
- [ ] Reviewdog integration working
- [ ] Baseline findings triaged

**Operations:**
- [ ] Team briefed
- [ ] Monitoring established
- [ ] Maintenance schedule set
- [ ] Rollback procedure understood

---

## Deployment Sign-Off

- [ ] All checklist items completed
- [ ] Ready for production deployment
- [ ] Team briefing completed
- [ ] Documentation is accessible

**Deployed by:** ___________________
**Date:** ___________________
**Notes:** ___________________

---

## Support & Questions

For issues or questions during deployment:

1. Check `.ci/QUICK-START.md` for common tasks
2. Review `.ci/baselines/runner-manifest.md` for runner troubleshooting
3. Read `docs/quality-pipeline.md` for comprehensive documentation
4. Check workflow run logs in GitHub Actions for detailed error messages

**Key contacts:**
- Maintainer: (see repository settings)
- Security team: (for security-related findings)
- DevOps: (for runner infrastructure)
