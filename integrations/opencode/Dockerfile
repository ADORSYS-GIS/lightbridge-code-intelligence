# Lightbridge `open`-mode agent image (RFC-0009 / ADR-0094).
#
# Assembly only (ADR-0080). Nothing is compiled here — we vendor the prebuilt OpenCode release binary
# onto our OWN pinned base, the same way services/agent-runner vendors the opengrep release binary.
# We do NOT inherit anomalyco's whole container image: pinning the RELEASE (v1.18.2) instead of that
# image's independent build tag keeps the runtime version aligned with the npm `@opencode-ai/plugin`
# version the plugins compile against (both v1.18.2), and leaves the base image OURS to control.
#
# BASE = Alpine (deliberate divergence from the repo's debian-slim house base). The OpenCode release
# binary is musl-DYNAMIC — `file` reports `interpreter /lib/ld-musl-x86_64.so.1` for BOTH the `-musl`
# and `-baseline-musl` variants (verified 2026-07-16; the only difference is the CPU baseline, not the
# linkage). It needs the musl loader + libgcc/libstdc++, so it cannot run on glibc/debian; Alpine is
# the natural host. The ADR-0088 sandbox posture (non-root, seccomp, read-only-root, egress-allowlist)
# is enforced at the POD level, so the base distro costs nothing on the security invariants.
#
# Build (context = integrations/opencode/). Prod target linux/amd64 (CI runners); build native locally:
#   docker build -f integrations/opencode/Dockerfile -t lightbridge-agent-open integrations/opencode

# ── Stage 1: fetch + verify + extract the pinned OpenCode release binary ───────────────────────────
FROM alpine:3.24 AS fetch
ARG OPENCODE_VERSION=v1.18.2
# sha256 of each release tarball, pinned for a hermetic, tamper-evident fetch (values verified
# 2026-07-16 against github.com/anomalyco/opencode releases). Non-baseline `-musl` = needs AVX2 on
# x64; the `-baseline-musl` asset is the fallback for older CPUs if a runtime ever lacks it.
ARG OPENCODE_SHA256_AMD64=36cb91ee3b6493f38bc50aa49bb241f3dbc620fcea938eb0776069213d0bb5ee
ARG OPENCODE_SHA256_ARM64=1defc1e42ffb6cb0fd2fc345e714f516398162ca49e1f7b165e99eceae32742f
ARG TARGETARCH
RUN set -eux; \
  apk add --no-cache curl; \
  case "$TARGETARCH" in \
    amd64) asset="opencode-linux-x64-musl.tar.gz";   sha="$OPENCODE_SHA256_AMD64" ;; \
    arm64) asset="opencode-linux-arm64-musl.tar.gz"; sha="$OPENCODE_SHA256_ARM64" ;; \
    *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
  esac; \
  curl -fsSL -o /tmp/opencode.tar.gz \
    "https://github.com/anomalyco/opencode/releases/download/${OPENCODE_VERSION}/${asset}"; \
  echo "${sha}  /tmp/opencode.tar.gz" | sha256sum -c -; \
  tar xzf /tmp/opencode.tar.gz -C /usr/local/bin opencode; \
  chmod +x /usr/local/bin/opencode

# ── Stage 2: the open-mode runtime ────────────────────────────────────────────────────────────────
FROM alpine:3.24 AS runtime
# Runtime deps: libgcc/libstdc++ (the binary links them), ripgrep (opencode shells out to it for
# search), git + ca-certificates (the sandbox clones the target repo over HTTPS and commits locally).
RUN set -eux; \
  apk add --no-cache libgcc libstdc++ ripgrep git ca-certificates; \
  adduser -D -u 1000 app; \
  mkdir -p /workspace; chown 1000:1000 /workspace

# The pinned OpenCode binary. `--version` at build time is a smoke test: a broken fetch or a missing
# lib fails the BUILD loudly rather than at task time.
COPY --from=fetch /usr/local/bin/opencode /usr/local/bin/opencode
RUN opencode --version

# Lightbridge's first-party integration: the recorder + gate-interlock plugins (ADR-0095) and the
# per-mode config (agents, subagents, permissions, mediated MCP). Plugins import ONLY
# `import type { Plugin }` (erased at runtime), so they need no node_modules — OpenCode's bundled Bun
# transpiles them in place. The exact load path (config `plugin` entry vs a .opencode/plugin dir) is
# an RFC-0009 probe finding; both source trees are staged so either wiring works. .dockerignore keeps
# the host node_modules out so the image stays hermetic.
COPY config /opt/lightbridge/opencode/config
COPY plugins /opt/lightbridge/opencode/plugins
RUN chown -R 1000:1000 /opt/lightbridge

# NOTE (next slice): the agent-plane supervisor binary (ACP client; ADR-0094) is not built yet. When
# it lands it is COPY'd in here from the build context (fully-static musl, like agent-runner) and
# becomes the ENTRYPOINT, spawning `opencode acp` as a subprocess. Until then the entrypoint is
# opencode itself, so the image is directly runnable for the probe and manual ACP sessions.

USER 1000
ENV WORKDIR=/workspace \
    LCI_OPENCODE_CONFIG_DIR=/opt/lightbridge/opencode/config \
    LCI_OPENCODE_PLUGINS_DIR=/opt/lightbridge/opencode/plugins \
    BUN_RUNTIME_TRANSPILER_CACHE_PATH=0
WORKDIR /workspace
ENTRYPOINT ["opencode"]
