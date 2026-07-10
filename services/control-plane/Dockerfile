# Lightbridge control plane (Rust/Axum) — assembly only.
#
# The binary is compiled on the CI runner (native static-musl, sccache) and delivered in the build
# CONTEXT — this file does NOT compile anything (ADR-0080). Because the binary is fully static
# (x86_64-unknown-linux-musl, no libc dependency), the runtime image is distroless/static: it carries
# only ca-certificates + tzdata + a nonroot user, nothing else. rustls (no OpenSSL) means no shared
# libs to link.
#
# Assemble (daemonless) with a context that contains the `control-plane` binary at its root:
#   buildah bud -f services/control-plane/Dockerfile -t lightbridge-control-plane <context>
FROM gcr.io/distroless/static-debian12:nonroot
COPY control-plane /usr/local/bin/control-plane
ENV BIND_ADDR=0.0.0.0:8080 \
    RUST_LOG=info
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/control-plane"]
