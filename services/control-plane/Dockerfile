# Lightbridge control plane (Rust/Axum). Build context = repo root (the Cargo workspace).
#   docker build -f services/control-plane/Dockerfile -t lightbridge-control-plane .
FROM rust:1-slim-bookworm AS builder
WORKDIR /build
# sccache: shared S3 compiler cache. BuildKit `--mount=type=cache` dirs are NOT persisted by the CI's
# `cache-from/to: type=gha` (that stores image layers, not cache mounts), so every CI build otherwise
# recompiles the workspace from cold. sccache's S3 backend survives across runs; enabled only when
# CI passes SCCACHE_BUCKET + creds (below), so a plain `docker build` still works uncached.
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
# Workspace root manifests + the members needed to resolve and build the control plane. Every
# workspace member's manifest must be present for cargo to load the workspace, even when building a
# single package — hence `clients` (the `lci` TUI) is copied too though it isn't compiled here.
COPY Cargo.toml Cargo.lock ./
COPY services services
COPY clients clients
COPY xtask xtask
# CI passes these; empty on a bare local build (sccache then stays off).
ARG SCCACHE_BUCKET=""
ARG SCCACHE_ENDPOINT=""
# --locked: build only from the reviewed Cargo.lock (fail on drift).
# Cache mounts persist the cargo registry + target across builds; the target dir is a
# cache mount (not in the layer), so copy the binary out to a real path before the stage ends.
# S3 creds arrive as BuildKit secrets (not build-args) so they never land in a layer or `docker history`.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    --mount=type=secret,id=sccache_s3_access_key_id \
    --mount=type=secret,id=sccache_s3_secret_access_key \
    if [ -n "$SCCACHE_BUCKET" ] && [ -s /run/secrets/sccache_s3_access_key_id ]; then \
      export RUSTC_WRAPPER=sccache CARGO_INCREMENTAL=0 SCCACHE_S3_USE_SSL=true \
             SCCACHE_ENDPOINT="$SCCACHE_ENDPOINT" \
             AWS_ACCESS_KEY_ID="$(cat /run/secrets/sccache_s3_access_key_id)" \
             AWS_SECRET_ACCESS_KEY="$(cat /run/secrets/sccache_s3_secret_access_key)"; \
    fi; \
    cargo build --release --locked -p control-plane \
    && cp target/release/control-plane /usr/local/bin/control-plane \
    && { command -v sccache >/dev/null && sccache --show-stats || true; }

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates \
  && rm -rf /var/lib/apt/lists/* \
  && useradd --uid 1000 --create-home --user-group app
COPY --from=builder /usr/local/bin/control-plane /usr/local/bin/control-plane
USER 1000
ENV BIND_ADDR=0.0.0.0:8080 \
    RUST_LOG=info
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/control-plane"]
