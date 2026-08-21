# @lightbridge/web

The Next.js (App Router) **web console** for Lightbridge — an operator dashboard over the control plane.

## Auth

OIDC against Keycloak (Authorization Code + PKCE,
[ADR-0014](../../docs/adr/0014-keycloak-oidc-resource-server.md)); the app stores no credentials.
Authorization is **permission-based** ([ADR-0023](../../docs/adr/0023-db-backed-rbac.md)): the token
carries a `permissions` list under a configurable claim, and both the control plane and the UI gate on
capabilities (`task:read/cancel`, `repo:read/approve/deny`, `review:read`, …). `proxy.ts` guards
`/dashboard/*`.

## Surfaces

- **Overview** — insights (KPIs, runs-over-time, breakdowns; client-aggregated).
- **Runs** — timeline + sortable/paginated table with filters and a ⌘K command palette; URL-synced state
  via `nuqs`.
- **Run detail** — persisted review output, an **embedded Grafana/Loki log panel** for the run (the
  generated `task-runs` dashboard's "Logs for $task_id" panel via a `d-solo` iframe; Epic #459), and a
  Cancel action (gated on `task:cancel`).
- **Repositories** — connected repos + approval/run activity.
- **Approvals** (admin) — the repo approval gate (epic #75): a repo is indexed/reviewed only once
  approved; decisions are reversible.
- **Settings** — account, GitHub-App link, effective permissions.

## Layout

Components and lib modules are grouped by *what they are*, so the shape is self-evident:

```
app/                 App Router routes (one folder per surface) + api/ route handlers
components/
  runs/              run list, table, timeline, row, logs, review output
  repos/             repository list
  overview/          insights / KPIs
  shell/             dashboard chrome — console shell, nav links, ⌘K palette
  ui/                design-system primitives (button, card, pill, states, …)
lib/
  domain/            domain types + presentation logic (tasks, repos, insights)
  auth/              OIDC client + session/claims
  server/            server-side API clients (control-plane calls, admin)
  utils/             cross-cutting helpers (cn, config)
  hooks/             shared client hooks
proxy.ts             permission gate for /dashboard/*
```

## Design system

daisyUI v5 with the **dracula** theme, **dark-only**, on Tailwind v4
([ADR-0027](../../docs/adr/0027-daisyui-design-system.md)). Variants are composed with
`class-variance-authority` + `cn` (tailwind-merge + clsx); shared UI primitives live in
`components/ui/`, shared client logic in `lib/hooks/`.

## Develop

```bash
pnpm --filter @lightbridge/web dev     # http://localhost:3000
pnpm --filter @lightbridge/web build
pnpm exec biome check .                 # lint/format (run in apps/web)
```

Env: `.env` for the OIDC issuer/client + the control-plane URL + `AGENT_NAMESPACE` (used for the
`kubectl logs` snippet on the run detail page) + `NEXT_PUBLIC_GRAFANA_URL` (client-visible base URL for
the embedded run-logs panel; unset falls back to the `kubectl` snippet). See
[docs/local-setup.md](../../docs/local-setup.md).
