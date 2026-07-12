# Lightbridge agent runner. The per-task Job image the dispatcher launches.
#
# Assembly only — the Rust runner binary is compiled on the CI runner (native static-musl, sccache)
# and delivered in the build CONTEXT (ADR-0080); this file does NOT compile Rust. Because the binary
# is fully static it runs unchanged on the glibc base below (it only *spawns* glibc subprocesses —
# git, opengrep — which use the image's own libc).
#
# ONE lean runtime, TWO targets (runner-differentiation, RFC-0001):
#   • `indexer` — `index` Jobs (clone, tree-sitter chunk → pgvector, in-house lci-codegraph structural
#       graph → Neo4j, ADR-0086). The structural graph is now built IN-PROCESS by the Rust runner
#       (the `lci-codegraph` crate); there is no Python/Graphify subprocess to bundle anymore, so this
#       image is the same lean debian-slim runtime the review path uses.
#   • `review` — the native agent loop (ADR-0026/0037), LLM/network-bound, reuses the latest indexed
#       snapshot (ADR-0050). It is where the deterministic SAST pass runs.
# Both targets alias one shared runtime stage (`runtime`): git + ca-certificates + the static runner
# binary + opengrep (SAST, ADR-0061). Retiring the Python + Graphify venv (ADR-0086, supersedes
# ADR-0019/ADR-0080's split) is what slims the indexer image and collapses the two into one runtime.
#
# Assemble (daemonless) with a context that contains the `agent-runner` binary at its root:
#   buildah bud -f services/agent-runner/Dockerfile --target indexer -t lightbridge-agent-runner <ctx>
#   buildah bud -f services/agent-runner/Dockerfile --target review  -t lightbridge-agent-review  <ctx>

# ── Stage 1: opengrep (SAST engine + vendored rules, ADR-0061) ────────────────────────────────────
# opengrep (the LGPL Semgrep fork) has NO official image, so we fetch the release binary by arch and
# vendor a pinned ruleset, so scans are hermetic — no registry fetch at runtime. The Linux asset arch
# token is `x86`/`aarch64` (NOT Docker's `amd64`/`arm64`), so we map `TARGETARCH`; `manylinux` = glibc,
# matching the debian-slim runtime below. The single-file binary needs no Python runtime.
FROM debian:bookworm-slim AS opengrep
ARG OPENGREP_VERSION=v1.23.0
ARG TARGETARCH
RUN apt-get update \
  && apt-get install -y --no-install-recommends curl ca-certificates git \
  && rm -rf /var/lib/apt/lists/* \
  && arch="$([ "$TARGETARCH" = "arm64" ] && echo aarch64 || echo x86)" \
  && curl -fsSL -o /usr/local/bin/opengrep \
       "https://github.com/opengrep/opengrep/releases/download/${OPENGREP_VERSION}/opengrep_manylinux_${arch}" \
  && chmod +x /usr/local/bin/opengrep \
  && opengrep --version \
  # Vendor the ruleset (upstream is archived/frozen, so a shallow clone of the default branch is a
  # stable pin). Then PRUNE the repo's non-rule YAML — this is LOAD-BEARING, not cleanup: opengrep
  # (semgrep semantics) treats a SINGLE invalid config under `--config <dir>` as fatal to the WHOLE
  # ruleset — it loads ZERO rules and every scan returns no findings (verified: the repo's
  # `.pre-commit-config.yaml` alone zeroed the ruleset → `rules:[]`). Rules live ONLY in the language
  # subdirs, so drop every top-level FILE (docs / packaging / `.pre-commit-config.yaml` / ignore files)
  # plus the non-rule dirs that ship YAML (`.git`, `.github`, `stats`, `scripts`). Verified in-image:
  # after this prune opengrep loads the rules and reports findings (exit 0, no config errors). The
  # `test -d` guard fails the build loudly if a future upstream layout change removes the rule dirs.
  && git clone --depth 1 https://github.com/opengrep/opengrep-rules /opt/opengrep-rules \
  && find /opt/opengrep-rules -maxdepth 1 -type f -delete \
  && rm -rf /opt/opengrep-rules/.git /opt/opengrep-rules/.github \
            /opt/opengrep-rules/stats /opt/opengrep-rules/scripts \
  && test -d /opt/opengrep-rules/python && test -d /opt/opengrep-rules/rust

# ── Stage 2: shared lean runtime ──────────────────────────────────────────────────────────────────
# One debian-slim base + git + ca-certificates + the static runner binary + opengrep (SAST). Since the
# structural graph is now built in-process by `lci-codegraph` (ADR-0086) there is no Python/Graphify
# venv, so `index` and `review` Jobs run identical images — this stage IS both. git: clone the target
# repo (src/clone.rs). ca-certificates: TLS to control plane + GitHub. The native review agent
# (ADR-0026/0037) runs in-process in the Rust runner — no subprocess to install.
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates git \
  && rm -rf /var/lib/apt/lists/* \
  && useradd --uid 1000 --create-home --user-group app \
  && mkdir -p /workspace && chown 1000:1000 /workspace
COPY agent-runner /usr/local/bin/agent-runner
# opengrep + vendored rules for the deterministic SAST pass (ADR-0061).
COPY --from=opengrep /usr/local/bin/opengrep /usr/local/bin/opengrep
COPY --from=opengrep /opt/opengrep-rules /opt/opengrep-rules
USER 1000
ENV WORKDIR=/workspace \
    RUST_LOG=info
ENTRYPOINT ["/usr/local/bin/agent-runner"]

# ── Targets: `indexer` and `review` both alias the one lean runtime ───────────────────────────────
# Kept as distinct targets so the image-pipeline matrix (and any `--target`) still resolves both
# names and publishes both tags; they build to the same runtime now that Graphify is gone.
FROM runtime AS indexer
FROM runtime AS review
