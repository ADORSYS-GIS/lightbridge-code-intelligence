# Lightbridge web console (Next.js standalone, pnpm monorepo).
# Build context = repo root.
#   docker build -f apps/web/Dockerfile -t lightbridge-web .
FROM node:22-slim AS base
ENV PNPM_HOME=/pnpm
ENV PATH=$PNPM_HOME:$PATH
RUN corepack enable

FROM base AS builder
WORKDIR /repo
COPY . .
# Cache the pnpm store across builds so deps aren't re-downloaded on layer invalidation.
RUN --mount=type=cache,id=pnpm,target=/pnpm/store \
    pnpm install --frozen-lockfile --store-dir /pnpm/store
# Next is configured with output: "standalone" + outputFileTracingRoot at the repo root,
# so the bundle mirrors the monorepo (apps/web/server.js + traced node_modules/packages).
#
# Build through `turbo run` so the task hits the self-hosted Turbo remote cache: a `build` whose
# inputs are unchanged is restored from the cache instead of re-running `next build`. Enabled only
# when CI passes TURBO_API + token (below); a bare local build runs turbo without a remote and just
# builds normally. TURBO_TOKEN arrives as a BuildKit secret so it never lands in a layer.
ARG TURBO_API=""
ARG TURBO_TEAM=""
RUN --mount=type=secret,id=turbo_token \
    if [ -n "$TURBO_API" ] && [ -s /run/secrets/turbo_token ]; then \
      export TURBO_API="$TURBO_API" TURBO_TEAM="$TURBO_TEAM" \
             TURBO_TOKEN="$(cat /run/secrets/turbo_token)"; \
    fi; \
    pnpm turbo run build --filter=@lightbridge/web

FROM base AS runner
WORKDIR /app
ENV NODE_ENV=production
# node:22-slim ships a non-root user at uid 1000; use the NUMERIC uid so Kubernetes
# runAsNonRoot validation passes without resolving the username inside the container.
COPY --from=builder --chown=1000:1000 /repo/apps/web/.next/standalone ./
COPY --from=builder --chown=1000:1000 /repo/apps/web/.next/static ./apps/web/.next/static
COPY --from=builder --chown=1000:1000 /repo/apps/web/public ./apps/web/public
USER 1000
ENV PORT=3000 \
    HOSTNAME=0.0.0.0
EXPOSE 3000
CMD ["node", "apps/web/server.js"]
