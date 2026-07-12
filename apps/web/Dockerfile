# Lightbridge web console (Next.js standalone) — assembly only.
#
# `next build` runs on the CI runner (native pnpm + Turbo remote cache), NOT here (ADR-0080). The
# build CONTEXT supplies the standalone bundle; this file just lays it out on node:22-slim. The
# output is portable JS (the sole native dep, sharp, is disabled — pnpm-workspace allowBuilds), so a
# runner-built bundle runs unchanged here.
#
# Assemble (daemonless) with a context containing standalone/, static/, public/ at its root:
#   buildah bud -f apps/web/Dockerfile -t lightbridge-web <context>
FROM node:22-slim AS runner
WORKDIR /app
ENV NODE_ENV=production
# node:22-slim ships a non-root user at uid 1000; use the NUMERIC uid so Kubernetes
# runAsNonRoot validation passes without resolving the username inside the container.
# `standalone` mirrors the monorepo (apps/web/server.js + traced node_modules/packages), so it lands
# at the app root; static + public sit at the paths Next resolves them from at runtime.
COPY --chown=1000:1000 standalone ./
COPY --chown=1000:1000 static ./apps/web/.next/static
COPY --chown=1000:1000 public ./apps/web/public
USER 1000
ENV PORT=3000 \
    HOSTNAME=0.0.0.0
EXPOSE 3000
CMD ["node", "apps/web/server.js"]
