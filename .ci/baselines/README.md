## Quality Pipeline Baselines & Suppressions

This directory contains runner provisioning manifests and finding suppressions.

### Files

- **runner-manifest.md** — Complete inventory of required tools, versions, checksums, and offline databases. Used to provision self-hosted runners.
- **suppressions.json** — Explicit suppression rules for specific findings (rule IDs, paths, reasons, expiration dates).

### Suppressions Format

```json
{
  "suppressions": [
    {
      "ruleId": "semgrep.security-scanner-id",
      "path": "path/to/file.ts",
      "reason": "False positive: legitimate use of eval in trusted context (only on admin panel)",
      "expiresAt": "2026-12-31T23:59:59Z",
      "approved_by": "security@company.com",
      "ticket": "#TICKET-123"
    }
  ]
}
```

**Fields:**
- `ruleId` — Scanner rule identifier (e.g., `semgrep.python.security.improper-input-validation`).
- `path` — Repository-relative path (POSIX, not Windows). Wildcards (`*`, `**`) supported.
- `reason` — Justification for the suppression (required; must explain why the finding is not actionable).
- `expiresAt` — ISO 8601 timestamp after which this suppression is considered stale. Gate will fail with a reminder to review.
- `approved_by` — Contact for follow-up (name or email).
- `ticket` — Reference to a tracking issue or decision record.

**Application:**
Suppressions are loaded by the quality gate and used to filter findings before determining pass/fail. A suppression applied to a finding removes it from the report visible to reviewdog.

### Runner Manifest

The manifest documents the exact tools, versions, and databases required for offline operation.

See `runner-manifest.md` for:
1. Tool versions and checksums
2. Trivy database cache locations and update frequency
3. CodeQL query pack locations (if enabled)
4. Semgrep rule pack sync procedure
5. GitHub Action pinned versions (with release notes)

### Baseline Management

**Adding a new suppression:**
1. Edit `suppressions.json` and add a new entry with all required fields.
2. Use a near-future expiration date (e.g., 90 days out) to force periodic review.
3. Create a tracking issue or link to an existing one in the `ticket` field.
4. Commit and include the suppression in your PR.

**Expiring or removing suppressions:**
- On scheduled reviews (monthly recommended), grep for expiring suppressions in CI logs.
- Delete expired entries or extend their `expiresAt` if the underlying issue persists.

**Baseline backfill (rare):**
If adding the quality gate to a repository with a large existing backlog of findings:
1. Run the full scan and capture the merged SARIF.
2. Manually audit each finding and create suppressions for true positives/accepted risks.
3. Commit `suppressions.json` in the same PR as the quality workflow.
4. The PR will be green; subsequent PRs will only fail on NEW findings introduced by the author.

### Offline Operation

All required databases and rule packs are pre-cached on the self-hosted runner:
- Trivy vulnerability DBs: `/opt/trivy-db/` (daily updates, not during workflow runs)
- Semgrep rules: `.ci/rules/semgrep/` (version-controlled, no external fetch)
- Gitleaks patterns: built-in (no external fetch)

The workflow passes offline flags to all scanners; see `.ci/quality/run.sh` for exact flags used.
