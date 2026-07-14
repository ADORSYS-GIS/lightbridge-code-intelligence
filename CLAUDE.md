This is **Lightbridge Code Intelligence**, a pnpm + Turborepo monorepo: `apps/web` (Next.js + better-auth), `packages/*` (shared TypeScript), and `services/control-plane` (a standalone Rust backend built on Axum and bound to cratestack), plus a Cargo `xtask`. See `docs/INDEX.md` and `docs/adr/` for architecture and decisions, and `CONTRIBUTING.md` for the contribution workflow.

Note: your global user instructions still apply on top of this file.

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

## Working methodology in this repo

**Clarity over speed, always.** When a choice is between the quick way and the clear way, take the
clear way — even if it costs more turns, more tool calls, or a slower PR. This governs every
trade-off below: split the god-file properly rather than patch around it; write the extra
regression test rather than trust that a change is safe; read a bot's full comment rather than
skim the summary. Autonomy and momentum (below) are about *not stalling on process* — they are not
license to cut a corner on the code itself.

### Code quality conventions (established across the 2026-07 repo-wide refactor)

- **SOLID/SRP by file, not by ceremony.** A file mixing unrelated responsibilities (e.g. one
  `db.rs` doing task queries, review persistence, and durable-step journaling) gets split by
  domain into sibling files behind a `mod.rs` that re-exports the same public paths — callers
  never change. Don't split a file that's already single-purpose just because it's long; length
  alone isn't the smell, mixed responsibility is.
- **`match` over `if`/`else if` chains** wherever the case set is closed and known (subcommand
  dispatch, per-variant handling, state-machine transitions). Leave a genuine binary `if`/`else`
  alone — converting a two-way branch to `match` for its own sake adds noise, not clarity.
- **Traits/interfaces only at a real seam** — where more than one concrete implementation
  genuinely exists or is coming (e.g. `StepRuntime`, `Tool`, `LanguageSupport`). Do not add a
  trait pre-emptively for a single implementation; that's ceremony, not abstraction. Likewise, a
  shared error type across unrelated domains is usually a worse idea than several small
  domain-specific ones — `thiserror`/`anyhow` per-crate is the default, not a shared error crate.
- **Reuse what the workspace already provides before hand-rolling it.** Before writing a new
  env-var parser, CLI arg parser, or similar plumbing, check whether an existing crate
  (`lci-config` for env/config coercion, `clap` for CLI — already a workspace dependency) already
  solves it. If a crate already depends on the shared solution and still reinvented it locally,
  that's a bug, not a style choice.
- **MVP / Model-Update-View separation for UI code** (the TUI client, and `apps/web` by the same
  logic): state (Model) and its transitions (Update/Presenter) belong in different files from
  rendering (View). A screen's `state.rs`/`update.rs`/`ui/<screen>.rs` split should make it obvious
  at a glance which file owns which concern.

### Refactor discipline — behavior-neutral changes must be *provably* behavior-neutral

- **Zero behavior change, zero public-API change**, unless the task is explicitly a behavior
  change. A structural refactor that also changes what the code does is not a refactor — split it
  into two changes.
- **Verify with the full existing test suite passing *unchanged*** (assertions relocated, never
  weakened or deleted to make the diff easier) **plus a full `cargo build --workspace`** (or the
  monorepo-wide equivalent) to catch any interaction a single crate's own tests can't see. For a
  crate other crates depend on, that workspace build is the real proof — not optional.
- **If you find a real bug while refactoring, flag it — do not fix it in the same PR.** A
  structural PR and a behavior-fixing PR should never be the same commit; it makes the structural
  PR harder to trust and the bug fix harder to review on its own merits. Open a tracking ticket
  (or note it clearly for one) and move on.
- **A committed golden/fixture file changing is a signal you broke something, not that the golden
  is stale.** Regenerating a golden to make a refactor pass is very rarely correct — prove the
  refactor byte-for-byte reproduces the existing golden instead.

### PR and review workflow

- **Every PR needs the governance template filled honestly**: numbered sections, a real
  source-of-truth (`#123` or a full URL — a doc name like "ADR-0086" in prose does *not* satisfy
  the parser; a boilerplate governance link does not either), and verification evidence as a
  fenced code block or checked box (inline-backtick bullets don't count, and a bare `#`-prefixed
  line *inside* a fenced block gets parsed as a heading and truncates the section — use `$`
  prompts). Read `.github/PULL_REQUEST_TEMPLATE.md` for the current exact shape rather than
  assuming a remembered one — it has drifted before.
- **Pull the PR's own bot comments (gemini-code-assist / lightbridge-assistant / codex) before
  every merge — not just a sub-agent's adversarial review.** A sub-agent reviewer is not a
  substitute for what the PR's own bots actually found; both should be checked, and a bot's
  legitimate finding (even round two, even on a PR fixing a bot's *first* finding) gets addressed
  before merging, not waved off.
- **Adversarial review before merging anything non-trivial**: an implementer produces the diff,
  one or more independent reviewers try to break its claims (find a concrete input → wrong output,
  not a style nit), then merge. This is especially load-bearing for anything claiming "zero
  behavior change" — that claim needs to be *proven*, not asserted.
- **Admin-merge only under explicit authorization** — a general "drive this" mandate does not
  imply `--admin`; bypassing branch protection needs its own explicit go-ahead, though that
  authorization can cover a whole batch when given that way.
<!-- END: Working methodology -->