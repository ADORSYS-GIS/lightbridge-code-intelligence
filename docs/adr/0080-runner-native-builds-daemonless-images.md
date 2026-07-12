# ADR-0080: Compile on the runner, assemble images daemonless (static-musl + buildah)

- **Status:** Proposed
- **Date:** 2026-07-10
- **Deciders:** Stephane Segning Lambou (owner/eng)

## Context and Problem Statement

Our four images (`control-plane`, `agent-runner` indexer/review, `web`) were built with multi-stage
Dockerfiles that **compiled inside the image build**: a `rust:1-slim-bookworm` stage ran `cargo build`,
and a `node` stage ran `next build`. That coupling forced compiler caching *into* BuildKit — the
`--mount=type=cache` target dir is not persisted by GHA layer cache (`type=gha` stores layers, not
cache mounts), so every CI run recompiled from cold. ADR-0080's predecessor work (sccache-in-Docker
via secret mounts, #327) papered over this, but the plumbing (BuildKit secrets, `RUSTC_WRAPPER`
guards) only exists because compilation happens where caching is hard. Should the container build keep
doing the compiling — or should we compile natively on the runner and let the image build be a thin
packaging step?

## Decision Drivers

- Compiler/build caching should be trivial (native sccache + Swatinem + Turbo remote cache), not
  something we fight BuildKit for.
- Remove the Docker **daemon** from CI (daemonless posture; also lets images be assembled anywhere).
- Smaller, simpler runtime images; delete the sccache-secret plumbing.
- Keep the deploy contract intact: same `sha-<gitsha>` tags + keyless cosign signature that
  argocd-image-updater verifies (ADR-0055/0057).

## Considered Options

- **A — Runner-native builds + daemonless buildah assembly.** `cargo build` (static-musl) and
  `pnpm/turbo build` run on the runner; images are `COPY`-only Dockerfiles assembled by `buildah`.
- **B — Keep compiling in-Docker, just swap the builder** (buildctl/kaniko) for the daemon.
- **C — Status quo** (compile in Docker, sccache via BuildKit secrets).

## Decision Outcome

Chosen option: **A**. Compilation moves to the runner where caching is native and free; the image
build becomes a `COPY` over a prebuilt artifact, so the builder choice stops mattering and we assemble
daemonless with buildah. The Rust services ship as **fully static `x86_64-unknown-linux-musl`** binaries
(verified: `aws-lc-sys`/`aws-lc-rs` + the whole workspace build static under musl on both arches), so
`control-plane` runs on `distroless/static` and `agent-runner` on its existing python/debian bases
(the static binary only *spawns* glibc subprocesses — git, graphify, opengrep). `web` ships the
portable Next.js `standalone` bundle on `node:22-slim`. amd64 only for now; arm64 is future work.

Because musl's built-in allocator regresses under multithreaded load, both service binaries install
**mimalloc** as the `#[global_allocator]` to restore glibc-class throughput. TLS stays **rustls**
(no OpenSSL), which is what makes static-musl painless.

### Consequences

- Good, because caching is native: sccache (compiler), Swatinem (crate downloads), Turbo (JS) — no
  BuildKit secret mounts; the #327 sccache-in-Docker plumbing is deleted.
- Good, because no Docker daemon in the image path (buildah, daemonless), and `control-plane` shrinks
  to a distroless/static image.
- Good, because the workspace compiles once natively instead of 2–3× across image builds.
- Bad, because we adopt musl at runtime for the prod services — mitigated with mimalloc; musl's
  DNS/NSS resolver semantics differ from glibc and are a thing to watch in k8s.
- Neutral, because `web` is on the epic #241 sunset path; it rides this restructure but isn't the
  reason for it. arm64 support is deferred.

## Pros and Cons of the Options

### A — runner-native + buildah

- Good, because caching is trivial, images are minimal, daemon is gone, plumbing deleted.
- Bad, because musl runtime semantics + a slightly more involved CI (artifacts between jobs).

### B — swap the in-Docker builder

- Good, because daemonless without changing the build model.
- Bad, because it keeps compilation where caching is hard — the actual problem — unsolved.

### C — status quo

- Good, because no change.
- Bad, because every run recompiles cold unless we keep the fragile BuildKit-secret sccache path.

## More Information

- Supersedes the sccache-in-Docker approach from #327 (that plumbing is removed here).
- Related: ADR-0055/0057 (GitOps delivery + signed-image promotion), ADR-0019 (agent-runner image),
  ADR-0061/0073 (opengrep/SAST in the runner image).
- Verification: static-musl + mimalloc build spike (both arches, fully static) recorded on the PR.
