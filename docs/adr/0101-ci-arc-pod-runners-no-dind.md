# ADR-0101: CI moves to ARC pod runners; dind is rejected as a Docker-in-CI fix

- **Status:** Accepted
- **Date:** 2026-07-18
- **Deciders:** Stephane Segning Lambou (owner/eng)
- **Source of truth:** #476

## Context and Problem Statement

The `[self-hosted, vymalo-vps]` label previously resolved to a single Docker-capable VM. It had
recurring `docker.sock: permission denied` and disk-space flakes, and every Rust job that needed a C
toolchain had to route around the VM's missing `cc` by wrapping itself in a `container:
ghcr.io/vymalo/arc-runners:latest` step with `--user root` (see `rust-lint.yml` /
`image-pipeline.yml` pre-#476 history) — a per-job workaround, not a runner-level fix.

The label now resolves to **ephemeral ARC (Actions Runner Controller) pod runners**, provisioned via
a separate infra repo, **home-os** (`gha-runner-scale-set`, ArgoCD app `vymalo-runners`, destination
cluster `netcup-k8s` / kubectl context `admin@netcup`, namespace `gh-runners-vymalo`). Each ephemeral
pod runs the image `ghcr.io/vymalo/arc-runners:jdk21-node22` — a single fat image (built and owned in
the `vymalo/arc-runners` repo) baking in Rust (rustup stable + clippy/rustfmt/rust-src,
cargo-llvm-cov/nextest/deny, sccache), Node + pnpm, JDK 21, Python, and **rootless Buildah + Podman +
BuildKit**, whose own Dockerfile states the intent explicitly: "daemonless builds and compose smoke
tests WITHOUT a privileged dind sidecar." The runner pod's own `securityContext.privileged: true` is
needed only so nested rootless podman/buildah can map subuids on this host — there is still no Docker
daemon, dind sidecar, or `docker.sock` anywhere in the pod.

The whole point of ARC — a pod runner already carrying every tool the fleet needs — is defeated the
moment a job still has to wrap itself in a redundant `container:` image or reach for a Docker feature.
Concretely: ARC pods have **no Docker daemon**, so any job step or workflow construct that shells out
to Docker fails at the `docker version` check — including `container:` (job-level or step-level) and
`services:` (both are launched via the Docker socket by the GitHub Actions runner).

## Decision

Adapt this repo's workflows to run **directly on the ARC pod's own tools**, and treat **dind
(docker-in-docker) as explicitly rejected** as the fix for any future Docker-needing CI job on this
fleet — rootless buildah/podman (already baked into the runner image, and already this repo's
strategy for daemonless image assembly, [ADR-0080](0080-runner-native-builds-daemonless-images.md))
is the one sanctioned path. If a future job needs a container tool, adapt it the same way — drop
`container:`, use podman — do not reach for a privileged dind sidecar.

Applied in #476:

- **`.github/workflows/rust-lint.yml`** and **`.github/workflows/image-pipeline.yml`'s `rust-build`
  job** — dropped the `container: { image: ghcr.io/vymalo/arc-runners:latest, options: --user root }`
  wrapper. The runner pod already **is** that image (tag `jdk21-node22`); the wrapper was a
  workaround for the old VM's missing C toolchain and is now redundant. `image-pipeline.yml`'s
  musl/C-toolchain install step switched from a root-in-container `apt-get` to `sudo apt-get` (the
  ARC runner user has password-less sudo; the VM-workaround container ran as root instead).
- **`.github/workflows/dashboards.yml`** — this one did **not** use the arc-runners image; it wrapped
  in `container: python:3.12-bookworm` (worked around the VM's Debian 13/trixie having no prebuilt
  `actions/setup-python` interpreter). Converted to the runner's system `python3` (3.12, Ubuntu 24.04
  noble base) + a throwaway venv (noble's system Python is PEP-668 externally-managed, so a bare `pip
  install` is rejected).
- **`.github/workflows/control-plane-tests.yml`** — dropped `container:` and converted its `services:
  postgres` (pgvector) block into an explicit `podman run -d --name pg docker.io/pgvector/pgvector:pg17`
  step, a `pg_isready` readiness-wait loop, and an always-run `podman rm -f pg` cleanup.
  `DATABASE_URL`'s host moved from the GHA-service hostname `postgres` to `127.0.0.1` (podman
  publishes the port to localhost; there is no service-network DNS without the Docker-backed
  `services:` machinery).
- Image assembly (`image-pipeline.yml`'s `images` job, "Build (podman)" step) already used rootless
  `podman build` / `podman push` per ADR-0080 and needed no change.

## Consequences

- **Positive.** No more VM-workaround `container:` wrappers; ARC gives ephemeral, auto-scaled pod
  runners with no shared-host state to leak between jobs (`docker.sock` permission drift, disk
  exhaustion from a prior job's layers). The rootless-podman posture is now uniform across both "run
  the job" and "assemble the image" — one container story, not two.
- **Positive.** A clear, written rule (this ADR) for the next Docker-needing job: adapt to podman, do
  not add dind. Prevents the fleet from silently regaining a privileged Docker daemon one workaround
  at a time.
- **Negative / operational.** The ARC pods depend on two node-level prerequisites on the
  `netcup-k8s` Talos cluster that are invisible from this repo and easy to forget when the fleet
  grows (see below) — a contributor debugging a stuck queue needs to know to look at the cluster, not
  just the workflow YAML.
- **Neutral.** The old debian-container workaround in `dashboards.yml` and the `--user root`
  container option in `rust-lint.yml`/`image-pipeline.yml` are gone entirely (hard cutover, not kept
  as a fallback path).

## Operational prerequisites (home-os side, verified against the live cluster 2026-07-19)

These are not in this repo, but a contributor debugging CI needs them to make sense of a stuck queue:

1. **Namespace PodSecurity level.** The `gh-runners-vymalo` namespace carries an explicit
   `pod-security.kubernetes.io/{enforce,audit,warn}: privileged` label, set via the `vymalo-runners`
   ArgoCD Application's `syncPolicy.managedNamespaceMetadata` (home-os `charts/cd/values.yaml`). This
   is required: the `netcup-k8s` cluster's unlabeled-namespace default is **`baseline`** (verified live
   — a `securityContext.privileged: true` pod in a fresh, unlabeled namespace is rejected with
   `violates PodSecurity "baseline:latest": privileged`), which would reject the runner pod's required
   `privileged: true` securityContext.
2. **Talos worker sysctl.** The Talos worker machine config sets `machine.sysctls.user.max_user_namespaces:
   "15000"` (home-os `netcup-k8s/patch-worker.yaml` / `cluster/worker.yaml`; Talos's own default is
   `0`). Verified live on the node backing a running runner pod
   (`/proc/sys/user/max_user_namespaces` reads `15000`). Without it, rootless podman/buildah inside the
   pod fails to create user namespaces (`cannot clone: No space left on device` /
   "user namespaces are not enabled"). Talos sysctl patches apply live via `talosctl patch mc`, no
   reboot required.

**Diagnosis tip:** a CI queue with jobs stuck `queued` and nothing ever reaching `in_progress` for an
extended time is this infra, not a workflow-YAML problem — check
`kubectl --context admin@netcup -n gh-runners-vymalo get ephemeralrunner` for entries in `Failed`
status and read `.status.message` for which of the two prerequisites above is missing.

## More Information

- [ADR-0080](0080-runner-native-builds-daemonless-images.md) — the predecessor decision that put
  rootless podman/buildah on the image-assembly path; this ADR extends the same daemonless posture to
  the job-runner layer.
- Runner image source: `vymalo/arc-runners` (home-os-adjacent infra repo, not this repo). Its
  Dockerfile documents the buildah/podman/rootless-BuildKit stack as an explicit replacement for a
  privileged `docker:dind` sidecar.
- Runner deployment: `home-os` repo, `charts/cd/values.yaml` (ArgoCD Application `vymalo-runners`,
  `gha-runner-scale-set` chart) and `netcup-k8s/patch-worker.yaml` / `cluster/worker.yaml` (Talos
  sysctl). Not part of this repo; documented here because it directly gates whether this repo's CI
  runs at all.
