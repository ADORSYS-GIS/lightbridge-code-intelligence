# Quality Pipeline Quick Start

## For Developers

### Run Locally
```bash
bash .ci/quality/run.sh
```
**Output:** `.ci/quality/reports/quality.sarif` (merged findings)

### Suppress a Finding
1. Edit `.ci/baselines/suppressions.json`
2. Add entry with ruleId, path, reason, expiration date
3. Commit and push
4. Finding will be excluded from next scan

**Example:**
```json
{
  "ruleId": "semgrep.security.eval",
  "path": "apps/web/src/utils.ts",
  "reason": "Legitimately trusted input in admin-only module",
  "expiresAt": "2026-12-31T23:59:59Z",
  "approved_by": "security@company.com",
  "ticket": "#123"
}
```

### Understanding PR Check Result
- **Green ✓:** No new error-level findings
- **Red ✗:** New error-level findings detected
- **Details:** Click "Quality Gate" check to see findings

### Fix a Finding
1. Read the finding message and rule ID
2. Navigate to file and line number
3. Fix the issue (don't just suppress it unless truly false positive)
4. Push fix and check will pass

---

## For Maintainers

### Update Semgrep Rules
1. Edit or add `.yaml` files in `.ci/rules/semgrep/security/`
2. Test locally: `semgrep --config=.ci/rules/semgrep path/to/code`
3. Commit and merge
4. Rules automatically picked up on next workflow run

### Update Tool Versions
1. Update version in `.ci/baselines/runner-manifest.md`
2. SSH into runner and install new version (see manifest for commands)
3. Test: Run workflow on a branch
4. Merge changes

### Update Trivy Databases (Runner Maintenance)
```bash
# SSH into vymalo-vps runner and run:
trivy image --download-db-only

# Verify:
ls -lh ~/.cache/trivy/db/
```
**Frequency:** Daily (set up cron job on runner)

### Enable GitHub Code Scanning Upload
1. Go to repository **Settings** → **Variables**
2. Create new variable: `ENABLE_CODE_SCANNING=true`
3. Next workflow run will upload merged SARIF to GitHub Code Scanning
4. View results in **Security** → **Code Scanning** tab

### Review Expired Suppressions
Suppressions with `expiresAt` in the past should be reviewed:
1. Audit the underlying finding (is it still relevant?)
2. Either delete the suppression or extend `expiresAt`
3. Commit update

### Troubleshoot Workflow Failure

**Error: "Scanner 'X' not found"**
→ SSH into runner and install per `.ci/baselines/runner-manifest.md`

**Error: "Database file does not exist" (Trivy)**
→ Run `trivy image --download-db-only` on runner

**Error: "SARIF merge failed"**
→ Check `.ci/quality/reports/` for malformed individual SARIF files

**Error: "No SARIF files found"**
→ Likely configuration error in one of the scanners (check logs)

---

## Documentation

- **Full Guide:** [docs/quality-pipeline.md](../docs/quality-pipeline.md) (475 lines, comprehensive)
- **Runner Setup:** [.ci/baselines/runner-manifest.md](.ci/baselines/runner-manifest.md)
- **Implementation Details:** [.ci/IMPLEMENTATION-SUMMARY.md](.ci/IMPLEMENTATION-SUMMARY.md)
- **This File:** [.ci/QUICK-START.md](.ci/QUICK-START.md) (you are here)

---

## File Locations

```
.ci/quality/          # Orchestration scripts
.ci/baselines/        # Suppressions and runner manifest
.ci/rules/semgrep/    # Semgrep rules
.github/workflows/quality.yml  # GitHub Actions workflow
docs/quality-pipeline.md  # Full documentation
```

---

## Common Tasks

| Task | Command/File |
|------|---|
| Run local scan | `bash .ci/quality/run.sh` |
| View findings | `cat .ci/quality/reports/quality.sarif \| jq` |
| Suppress finding | Edit `.ci/baselines/suppressions.json` |
| Add Semgrep rule | Create `.ci/rules/semgrep/security/new-rule.yaml` |
| Update Trivy DBs | SSH to runner, run `trivy image --download-db-only` |
| Check tool versions | See `.ci/baselines/runner-manifest.md` |
| Debug workflow | Check `.ci/quality/reports/` artifacts in GitHub Actions |
| Enable Code Scanning | Set repo variable `ENABLE_CODE_SCANNING=true` |

---

## Key Concepts

### Offline Operation
All scanners run locally without external rule/database downloads:
- **Semgrep:** Rules in `.ci/rules/semgrep/` (version-controlled)
- **Trivy:** DBs pre-cached in runner's `~/.cache/trivy/db/` (daily updates)
- **Gitleaks:** Built-in patterns (updated with binary upgrades)

### SARIF (Standardized Analysis Results Format)
All findings consolidated into single `.ci/quality/reports/quality.sarif` file for:
- Consistent PR reporting via reviewdog
- Machine-readable audit trail
- GitHub Code Scanning integration

### Quality Gate Policy
- **PRs:** Fail only on NEW error-level findings
- **Main branch:** Report all findings (no fail)
- **Suppressions:** Explicit, documented, time-limited

### Deduplication
Individual scanner SARIF files are merged and deduplicated by:
- Rule ID
- Finding message
- File path
- Start line number

Keeps reports clean (single entry per finding, even if multiple scanners detect it).
