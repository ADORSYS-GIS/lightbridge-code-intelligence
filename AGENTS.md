This is **Lightbridge Code Intelligence**, a pnpm + Turborepo monorepo: `apps/web` (Next.js + better-auth), `packages/*` (shared TypeScript), and `services/control-plane` (a standalone Rust backend built on Axum and bound to cratestack), plus a Cargo `xtask`. See `docs/INDEX.md` and `docs/adr/` for architecture and decisions, `ROADMAP.md` for the at-a-glance status of shipped/in-progress/planned work, and `CONTRIBUTING.md` for the contribution workflow.

<!-- BEGIN: AI Governance stanza (managed by ADORSYS-GIS/ai-governance) -->
## AI Governance

AI may accelerate the work, but humans own intent, verification, and consequences.
AI output is not truth: review AI-generated code as untrusted, and never submit work you cannot explain.

When opening issues or pull requests in this repo:

- Use the provided **issue forms** (Epic, User Story, Dev Ticket) and the **pull request template** — do not open blank issues/PRs.
- Fill in the **AI Usage Declaration** honestly (what AI was used for, what you verified).
- Include a **source-of-truth link** (a URL or `#123` reference). No source of truth means the work is not ready.
- Provide **verification evidence** (commands, logs, links, or checked verification boxes). No evidence means it is not done.

Source of truth and full doctrine: https://adorsys-gis.github.io/ai-governance/
This stanza is intentionally thin — read the site; do not duplicate the doctrine here.
<!-- END: AI Governance stanza -->

## Keeping the roadmap current

`ROADMAP.md` is the maintainer-facing status of the project. **Treat updating it as part of "done."**
When a change meaningfully ships, unblocks, or retires a roadmap item — a feature going live, an epic
advancing, a blocker cleared — move that item to its new section (or add it) in `ROADMAP.md` **in the same
PR**, with the merge/issue reference and date. It is not the source of truth (decisions live in
`docs/adr/`, tracked work in GitHub issues), so keep entries short and link out rather than duplicating
detail. If a change touches nothing on the roadmap, leave it alone — don't churn the file for its own sake.