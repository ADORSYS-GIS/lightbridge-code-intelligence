# GitHub Workflows — `.github/workflows/`

## Summary

Six CI/CD workflows drive the Lightbridge monorepo. They are designed to be:

- **Parallel** — independent workflows run concurrently
- **Efficient** — Rust caching (`Swatinem/rust-cache`), Docker layer caching (`type=gha`),
  path-trigger filtering
- **Secure** — minimal `permissions`, immutable SHA pinning for cross-repo workflows,
  keyless cosign signing

---

## Workflow reference

### 1. `build-images.yml` — Build and publish container images

**Triggers:** PR (build only), push to `main`, tags `v*` (build + publish + sign)

**What it builds (4 images):**

| Image | Dockerfile | Target | Contents |
|-------|-----------|--------|----------|
| `lightbridge-control-plane` | `services/control-plane/Dockerfile` | (default) | The Axum HTTP server |
| `lightbridge-web` | `apps/web/Dockerfile` | (default) | Next.js web console |
| `lightbridge-agent-runner` | `services/agent-runner/Dockerfile` | `indexer` | Full image with Python + Graphify |
| `lightbridge-agent-review` | `services/agent-runner/Dockerfile` | `review` | Lean image (no Python/Graphify) |

**PR build:** Validates all images build, reports uncompressed size in the run summary.
No push (no registry credentials).

**Publish (main/tags):**
- Pushes all 4 images to `ghcr.io/<owner>/`
- Tags: branch (`main`), semver (`v0.1.0`), commit SHA (`sha-<gitsha>`), `latest`
- Signs each digest with **keyless cosign** (Fulcio + Rekor, GitHub OIDC)
- Reports compressed (registry pull) size in the run summary
- argocd-image-updater promotes the newest signed `sha-<gitsha>` digest by committing it to
  `adorsys-gis/ai-helm-values` (NOT this repo)

**Path exclusions:** `deploy/**`, `docs/**`, `**/*.md` — no image rebuild for doc-only changes.

### 2. `control-plane-tests.yml` — Control plane Rust test suite

**Triggers:** PR + push to `main`, path-filtered to `services/control-plane/**`,
`services/config/**`, `Cargo.toml`, `Cargo.lock`, `self`

**What it does:**
- Spins up `pgvector/pgvector:pg17` as a service container (mirrors `compose.yaml`)
- Runs `cargo test -p control-plane --locked` against it
- This runs `#[sqlx::test]` integration tests that provision throwaway databases with
  `CREATE EXTENSION vector` and run migrations — catching schema drift that unit tests miss

**Why it exists:** Migration 0005 widened the embedding column to `vector(4096)`, but existing
tests only ran against a live DB locally — so a test inserting a 1536-dim vector passed CI and
only failed in production (#132 / #134). This job closes that gap.

**Concurrency:** A newer push to the same ref cancels the in-flight run.

### 3. `rust-lint.yml` — Workspace-wide Rust lint gate

**Triggers:** PR + push to `main`, path-filtered to `services/**`, `xtask/**`,
`Cargo.toml`, `Cargo.lock`, `self`

**Steps:**
1. `cargo fmt --all -- --check` — formatting check
2. `cargo clippy --workspace --all-targets --locked -- -D warnings` — lint with deny

**Why it exists:** The other CI jobs compile and test but never checked formatting or lints,
so drift landed on `main` unnoticed (e.g. an over-width assert merged in #169 — it only
surfaced when a later branch ran `fmt --check` locally).

### 4. `dashboards.yml` — Grafana dashboard verification

**Triggers:** PR + push to `main`, path-filtered to `tools/dashboard-gen/**`,
`deploy/observability/**`, `self`

**What it does:**
1. Installs the dashboard generator (Python, `tools/dashboard-gen/`)
2. Regenerates all dashboards from source
3. Fails if `git diff` shows any changes — dashboards are **generated artifacts** that must
   be regenerated and committed before merging

Plus: `helm lint` + `helm template` for the observability chart.

### 5. `governance.yml` — AI Governance PR compliance

**Triggers:** PR opened, edited, synchronize, reopened

**What it does:**
Calls the **reusable workflow** from `ADORSYS-GIS/ai-governance` (pinned to an immutable
commit SHA, v1.0.0). It checks that the PR body contains:
1. **AI Usage Declaration** — what AI was used for, what was verified
2. **Source-of-truth link** — a URL or `#123` reference
3. **Verification evidence** — commands, logs, checked boxes

Fails with a sticky comment listing what's missing.

### 6. `opencode.yml` — opencode review for this repo

**Triggers:**
- PR: `opened`, `synchronize`
- Issue comment: `created`
- PR review comment: `created`

**What it does:**
Calls the **reusable workflow** from `ADORSYS-GIS/ai-governance` (immutable SHA). It runs an
opencode PR review (auto on PRs, or `/oc` in a comment) and issue tasks (`/oc` on an issue).

**Security:**
- **No-op** if `vars.OPENCODE_GATEWAY_AUDIENCE` is not set (so a fresh repo doesn't red-fail)
- Pinned to immutable SHA (not `@main`) — `id-token: write` + contents/pull-requests/issues
  `write` make a mutable ref a supply-chain risk
- Authenticates to the AI gateway via GitHub OIDC (no shared key) using the
  `camer-digital-ai` GitHub App

---

## Security patterns across all workflows

| Pattern | Where |
|---------|-------|
| Minimal `permissions` | Every workflow lists only the permissions it needs |
| Immutable SHA pinning | `governance.yml` and `opencode.yml` pin to specific commits, not movable tags |
| Keyless signing | `build-images.yml` uses cosign with GitHub OIDC (Fulcio + Rekor) |
| Path filtering | `control-plane-tests.yml`, `rust-lint.yml`, `dashboards.yml` only run on relevant changes |
| Concurrency groups | `control-plane-tests.yml` and `opencode.yml` cancel in-flight runs for the same ref |
