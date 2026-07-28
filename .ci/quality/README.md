## Quality Pipeline Scripts

This directory contains the scanner orchestration scripts for the offline-capable quality pipeline.

### Files

- **run.sh** — Master orchestrator: runs all applicable scanners, captures their exit codes, and reports results
- **merge-sarif.sh** — SARIF 2.1.0 merger: combines individual scanner reports into a single deduplicated run
- **gate.sh** — Quality gate enforcer: fails on scanner errors, new findings (by severity), and PR quality metrics

### Running Locally

```bash
cd .ci/quality
bash run.sh
```

Environment variables:
- `CI=true` — Set automatically in GitHub Actions; enables stricter error handling
- `GITHUB_EVENT_PATH` — Path to GitHub Actions event payload (PR detection)
- `GITHUB_REF` — Ref being tested; inferred from git if not set
- `GITHUB_WORKSPACE` — Repository root; defaults to git root

### Scanner Dependencies

Each scanner must be pre-provisioned on the self-hosted runner:
- `semgrep` v1.45.0+ (CLI only, no rules download)
- `trivy` v0.51.0+ (with offline-mode databases)
- `gitleaks` v8.18.0+
- `hadolint` v2.12.0+
- `reviewdog` v0.17.0+ (for PR reporting only)
- `pnpm` v11.5.2+ (for biome; already present via toolchain)

See `.ci/baselines/runner-manifest.md` for full versioning and provisioning steps.
