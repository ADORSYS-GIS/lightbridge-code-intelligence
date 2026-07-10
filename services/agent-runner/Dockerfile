# Lightbridge agent runner. The per-task Job image the dispatcher launches.
#
# This Dockerfile has TWO final targets (runner-differentiation, RFC-0001) that share one Rust
# `builder` stage so the runner binary is compiled once:
#
#   • `indexer` (DEFAULT) — the full image. Bundles (ADR-0019):
#       1. the Rust runner binary (orchestration: clone, embed→pgvector, report)
#       2. Python + Graphify (`graphifyy`) — the 36-language structural-graph extractor it spawns
#       3. git + ca-certificates — clone the target repo, TLS to the control plane
#       4. opengrep + a vendored ruleset — the deterministic SAST pass (ADR-0061)
#   • `review` — a leaner image WITHOUT the Python/Graphify venv (debian-slim + git + ca-certs +
#       the runner binary + opengrep). The review path never invokes Graphify, so it needs no Python
#       runtime, but it IS where the SAST pass runs, so it carries opengrep + the rules.
#
# Build context = repo root (the Cargo workspace). Pick a target with `--target`:
#   docker build -f services/agent-runner/Dockerfile --target indexer -t lightbridge-agent-runner .
#   docker build -f services/agent-runner/Dockerfile --target review  -t lightbridge-agent-review .

# ── Stage 1: build the Rust runner ──────────────────────────────────────────────────────────────
FROM rust:1-slim-bookworm AS builder
WORKDIR /build
# sccache: shared S3 compiler cache (see services/control-plane/Dockerfile for the rationale — GHA
# layer cache does not persist BuildKit cache mounts, so CI otherwise recompiles from cold). Enabled
# only when CI passes SCCACHE_BUCKET + creds; a bare local build stays uncached.
ARG SCCACHE_VERSION=v0.8.2
RUN set -eux; \
    apt-get update && apt-get install -y --no-install-recommends curl ca-certificates; \
    case "$(dpkg --print-architecture)" in \
      amd64) t=x86_64-unknown-linux-musl;; \
      arm64) t=aarch64-unknown-linux-musl;; \
      *) echo "unsupported arch" >&2; exit 1;; \
    esac; \
    curl -fsSL "https://github.com/mozilla/sccache/releases/download/${SCCACHE_VERSION}/sccache-${SCCACHE_VERSION}-${t}.tar.gz" \
      | tar -xz -C /tmp; \
    install -m0755 "/tmp/sccache-${SCCACHE_VERSION}-${t}/sccache" /usr/local/bin/sccache; \
    rm -rf /var/lib/apt/lists/* /tmp/sccache-*
# Every workspace member's manifest must be present for cargo to load the workspace, even when
# building a single package — hence `clients` (the `lci` TUI) is copied too though it isn't built here.
COPY Cargo.toml Cargo.lock ./
COPY services services
COPY clients clients
COPY xtask xtask
# CI passes these; empty on a bare local build (sccache then stays off).
ARG SCCACHE_BUCKET=""
ARG SCCACHE_ENDPOINT=""
# Copy the runner out before the cached target dir vanishes. S3 creds arrive as BuildKit secrets
# (not build-args) so they never land in a layer or `docker history`.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    --mount=type=secret,id=sccache_s3_access_key_id \
    --mount=type=secret,id=sccache_s3_secret_access_key \
    if [ -n "$SCCACHE_BUCKET" ] && [ -s /run/secrets/sccache_s3_access_key_id ]; then \
      export RUSTC_WRAPPER=sccache CARGO_INCREMENTAL=0 SCCACHE_S3_USE_SSL=true \
             SCCACHE_ENDPOINT="$SCCACHE_ENDPOINT" SCCACHE_REGION=us-east-1 \
             AWS_ACCESS_KEY_ID="$(cat /run/secrets/sccache_s3_access_key_id)" \
             AWS_SECRET_ACCESS_KEY="$(cat /run/secrets/sccache_s3_secret_access_key)"; \
    fi; \
    cargo build --release --locked -p agent-runner \
    && cp target/release/agent-runner /usr/local/bin/ \
    && { command -v sccache >/dev/null && sccache --show-stats || true; }

# ── Stage 2: install Graphify into a self-contained venv ────────────────────────────────────────
# Built with compilers present (some tree-sitter grammars may ship sdists) but the toolchain is
# left behind — only the venv is copied into the runtime image.
FROM python:3.12-slim-bookworm AS pybuild
RUN apt-get update \
  && apt-get install -y --no-install-recommends build-essential \
  && rm -rf /var/lib/apt/lists/*
RUN python -m venv /opt/venv
ENV PATH="/opt/venv/bin:$PATH"
# Pin the version the runner's graph.json parser was validated against.
RUN pip install --no-cache-dir "graphifyy==0.8.44" \
  && graphify --help >/dev/null

# ── Stage 2b: opengrep (SAST engine + vendored rules, ADR-0061) ───────────────────────────────────
# opengrep (the LGPL Semgrep fork) has NO official image, so we fetch the release binary by arch and
# vendor a pinned ruleset, so scans are hermetic — no registry fetch at runtime. The Linux asset arch
# token is `x86`/`aarch64` (NOT Docker's `amd64`/`arm64`), so we map `TARGETARCH`; `manylinux` = glibc,
# matching both debian/python-slim runtimes below. The single-file binary needs no Python runtime.
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

# ── Stage 3a: indexer runtime (DEFAULT) ──────────────────────────────────────────────────────────
# The full image: python-slim + Graphify venv + the runner. Used for `index` Jobs, which run the
# heavy structural-graph extraction. This is the default target so a bare `docker build` (and any
# tooling that doesn't pass `--target`) keeps producing today's image.
FROM python:3.12-slim-bookworm AS indexer
# git: clone the target repo (src/clone.rs). ca-certificates: TLS to control plane + GitHub.
# The native review agent (ADR-0026/0037) runs in-process in the Rust runner — no subprocess to install.
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates git \
  && rm -rf /var/lib/apt/lists/* \
  && useradd --uid 1000 --create-home --user-group app \
  && mkdir -p /workspace && chown 1000:1000 /workspace
# Graphify (venv) + the Rust runner.
COPY --from=pybuild /opt/venv /opt/venv
COPY --from=builder /usr/local/bin/agent-runner /usr/local/bin/agent-runner
# opengrep + vendored rules for the deterministic SAST pass (ADR-0061).
COPY --from=opengrep /usr/local/bin/opengrep /usr/local/bin/opengrep
COPY --from=opengrep /opt/opengrep-rules /opt/opengrep-rules
USER 1000
ENV PATH="/opt/venv/bin:$PATH" \
    WORKDIR=/workspace \
    RUST_LOG=info
ENTRYPOINT ["/usr/local/bin/agent-runner"]

# ── Stage 3b: review runtime (leaner — no Python/Graphify) ────────────────────────────────────────
# The review path is LLM/network-bound and reuses the latest indexed snapshot (ADR-0050); it never
# spawns Graphify, so this image drops the entire Python venv (~hundreds of MB) and ships only the
# runner binary on a plain debian-slim base with git + ca-certificates.
#
# Cold-repo caveat: a *cold-repo* review still self-indexes today (ADR-0050 only makes *warm* reviews
# reuse the snapshot). On this image the structural-graph step is simply skipped — `index_graph`
# (services/agent-runner/src/indexer/graph.rs) is best-effort/non-fatal: it logs and continues if
# `graphify` is absent. So the first cold review still builds the semantic (pgvector) index and just
# defers the graph until the next `index` task runs on the indexer image. (Proper fix — review never
# self-indexes — is a separate runner-differentiation slice.)
FROM debian:bookworm-slim AS review
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates git \
  && rm -rf /var/lib/apt/lists/* \
  && useradd --uid 1000 --create-home --user-group app \
  && mkdir -p /workspace && chown 1000:1000 /workspace
COPY --from=builder /usr/local/bin/agent-runner /usr/local/bin/agent-runner
# opengrep + vendored rules for the deterministic SAST pass (ADR-0061). The review path is where SAST
# runs, so this leaner image carries it too.
COPY --from=opengrep /usr/local/bin/opengrep /usr/local/bin/opengrep
COPY --from=opengrep /opt/opengrep-rules /opt/opengrep-rules
USER 1000
ENV WORKDIR=/workspace \
    RUST_LOG=info
ENTRYPOINT ["/usr/local/bin/agent-runner"]
