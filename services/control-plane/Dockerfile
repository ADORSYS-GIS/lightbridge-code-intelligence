# Lightbridge control plane (Rust/Axum). Build context = repo root (the Cargo workspace).
#   docker build -f services/control-plane/Dockerfile -t lightbridge-control-plane .
FROM rust:1-slim-bookworm AS builder
WORKDIR /build
# Workspace root manifests + the members needed to resolve and build the control plane. Every
# workspace member's manifest must be present for cargo to load the workspace, even when building a
# single package — hence `clients` (the `lci` TUI) is copied too though it isn't compiled here.
COPY Cargo.toml Cargo.lock ./
COPY services services
COPY clients clients
COPY xtask xtask
# --locked: build only from the reviewed Cargo.lock (fail on drift).
# Cache mounts persist the cargo registry + target across builds; the target dir is a
# cache mount (not in the layer), so copy the binary out to a real path before the stage ends.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked -p control-plane \
    && cp target/release/control-plane /usr/local/bin/control-plane

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
