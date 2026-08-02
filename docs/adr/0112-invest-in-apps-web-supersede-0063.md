# ADR-0112: Invest in `apps/web` as a permanent admin surface (supersedes ADR-0063's retirement plan)

- **Status:** Accepted
- **Date:** 2026-08-02
- **Deciders:** @stephane-segning

## Context and Problem Statement

[ADR-0063](0063-cli-only-repository-approval.md) (2026-06-28, amended 2026-07-03) concluded that
`apps/web`'s **last unique function** was the repository approval gate, and chose a CLI/TUI
(`clients/lci`) as the path to eventually retire the whole Next.js app once Grafana absorbed
observability. `ROADMAP.md` still carries "Retire `apps/web` (Epic #241)" as an open item.

That premise no longer holds. Since ADR-0063 was written, `apps/web` gained substantial *new* unique
surface, not less: the full ADR-0024 v2 information architecture (sortable data table, timeline view,
disclosure-row findings, Insights KPIs + hand-rolled sparkline, the `cmdk` command palette), the
ADR-0027 daisyUI migration, and — critically — **[ADR-0109](0109-control-plane-forge-write-for-repo-review-config.md)**
(2026-07-29, *after* ADR-0063's own amendment) explicitly designs its repo-preset admin capability to be
settable from "the `lci` TUI **or** `apps/web`." A later, accepted ADR already assumes `apps/web` keeps
working indefinitely — silently contradicting an earlier ADR's retirement conclusion. That is exactly
the kind of drift this project's own ADR process exists to catch and resolve, not leave unreconciled.

The concrete forcing function: [ADR-0111](0111-per-repo-review-settings-and-review-on-push.md) just
shipped a six-field per-repo settings resolver (check-run reporting, review-on-open, review-on-push,
push-storm strategy, dedup scope) with **zero UI on any surface**. It needs a home. `apps/web`'s
`app/dashboard/repositories/[id]/page.tsx` is explicitly commented in the code as a deliberately narrow
"preset only" stub — i.e. it was already earmarked to grow into the fuller per-repo detail page. Building
that page properly requires deciding, formally, whether `apps/web` has a future.

## Decision Drivers

- **Don't let an accepted ADR silently contradict an earlier one.** ADR-0109 already treats `apps/web`
  as permanent; that assumption should be decided explicitly, not left as an unstated contradiction for
  a future reader to notice and untangle.
- **Match the tool to the job.** Six provenance-tracked settings, a model-override picker, and a preset
  selector are naturally tabular/form-based structured admin UX. daisyUI's `SettingsSection`/`SettingsRow`
  with badges, toggles, and selects is a strictly better fit for that than a ratatui form — rebuilding
  equivalent fidelity in the TUI would cost more than continuing to invest in the Next.js app that
  already has it.
- **`clients/lci` is not being second-guessed.** It is genuinely good at what it was built for — fast,
  terminal-native approve/deny and run-watching for an operator who lives in a shell. Nothing here argues
  it was the wrong call; it argues *retiring the web app instead of also keeping it* was premature.
- **Don't re-litigate what's still genuinely unresolved.** ADR-0063's own Consequences flagged an
  imperative-CLI-vs-GitOps-declarative tension. That tension is orthogonal to "should `apps/web` exist"
  — it applies equally to `lci` and to `apps/web` (both are imperative admin actions over the same
  authz), and stays exactly as unresolved as ADR-0063 left it.

## Considered Options

- **Option A — Retire `apps/web` as ADR-0063 planned**, porting the settings/model-override UI into
  `clients/lci` instead of building it in Next.js.
- **Option B — Keep `apps/web` alive only for the new settings/model slice**, with everything else still
  headed for retirement per ADR-0063.
- **Option C — Formally reverse ADR-0063: invest in `apps/web` as the primary browsing/discovery and
  structured-admin surface, permanently. `clients/lci` remains the fast terminal-native ops tool.** Both
  surfaces coexist against the same admin API, indefinitely.

## Decision Outcome

Chosen option: **C**.

Rich per-repo settings (six fields, each independently provenance-tracked across default/file/DB
layers) plus a model-override picker are exactly the kind of structured, discoverable, form-heavy admin
surface a browser does well and a terminal does awkwardly. `apps/web` already has the IA investment
(ADR-0024) and the design system (ADR-0027) to do this properly; `clients/lci` would need to grow
equivalent form/table/badge primitives from scratch to match, for a worse result (terminal UIs are
better at fast single-actions and live-watching than at rich multi-field configuration surfaces).

This is not a reversal of ADR-0063's *judgment* — `lci` remains a good, maintained tool for what it does.
It is a reversal of ADR-0063's *conclusion that `apps/web` should therefore be deleted*. Both surfaces
now permanently coexist against the same control-plane admin API; capability parity between them is a
per-feature judgment call (see the Reviewer Focus note in the PR that upgrades the repo-detail page's
preset control to a dropdown, which the TUI does not get — a first deliberate, documented divergence
under this new posture), not a hard requirement.

### Consequences

- **Good** — no more UI/authz surface debt from an app that keeps gaining new endpoints pointed at it
  while nominally "soon to be deleted." The settings/model-override UI gets a real home instead of being
  awkwardly retrofitted into a TUI never designed for six-field provenance-tracked forms.
- **Good** — `clients/lci` stays exactly as valid and maintained as it already is; nothing about this ADR
  touches it or asks anyone to migrate away from it for terminal-native ops.
- **Bad / accepted trade-off** — two admin surfaces now need feature-parity *awareness* going forward,
  not lockstep parity. A capability added to one (e.g. a richer control the other's platform can't
  cheaply match) should be flagged explicitly per-PR, not silently diverged — same discipline this
  project already applies to config-schema changes needing both readers updated.
- **Neutral / unresolved, unchanged** — the imperative-CLI-vs-GitOps-declarative tension ADR-0063 flagged
  in its own Consequences remains genuinely open, applies equally to both surfaces, and is out of scope
  for this ADR to resolve.

### Required side effects (landed in the same PR as this ADR)

- `docs/adr/0063-cli-only-repository-approval.md`: status line changes from `Accepted` to
  `Superseded by [ADR-0112](0112-invest-in-apps-web-supersede-0063.md)` — body untouched, per this
  project's own ADR immutability rule (`docs/adr/README.md`).
- `docs/adr/README.md`: update ADR-0063's index row status; add this ADR's row.
- `ROADMAP.md`: remove the "Retire `apps/web`" bullet; replace with an active item describing the
  `apps/web` revamp, pointing at this ADR.

## Pros and Cons of the Options

### Option A — retire as planned, port settings into `lci`
- Good — follows through on ADR-0063 as originally decided; one fewer stack to maintain long-term.
- Bad — would require building form/table/provenance-badge primitives in ratatui from scratch, for a
  worse result than the Next.js app already provides; throws away the ADR-0024/0027 investment.

### Option B — keep only the new slice, still retire the rest
- Good — smallest possible new-code footprint.
- Bad — leaves the app in permanent limbo (half "keep," half "retire"), which is worse for maintainers
  than a clear decision either way; doesn't resolve the ADR-0109 contradiction, just narrows it.

### Option C — formally reverse, both surfaces coexist (chosen)
- Good — resolves the ADR-0109 contradiction explicitly; matches tool to job for structured admin UX;
  `lci` keeps its own lane.
- Bad — two surfaces to keep aware of each other going forward; documented as an accepted, ongoing cost
  rather than swept aside.

## More Information

- Superseded: [ADR-0063](0063-cli-only-repository-approval.md) (CLI-only repository approval).
- The contradiction this resolves: [ADR-0109](0109-control-plane-forge-write-for-repo-review-config.md)
  (repo preset config settable from `lci` **or** `apps/web`).
- The forcing function: [ADR-0111](0111-per-repo-review-settings-and-review-on-push.md) (per-repo
  settings store, needing a UI home).
- Design lineage this ADR continues investing in: [ADR-0006](0006-nextjs-app-router-web-ui.md)
  (Next.js), [ADR-0015](0015-web-console-design-language.md) (design philosophy),
  [ADR-0016](0016-dashboard-information-architecture.md) (IA), [ADR-0024](0024-web-console-redesign-v2.md)
  (v2 patterns), [ADR-0027](0027-daisyui-design-system.md) (daisyUI mechanism, still live).
- `clients/lci`, explicitly unaffected by this ADR: `clients/lci/README.md`.
- Epic #241 (the retirement epic ADR-0063 fed): superseded by this ADR's decision; not carried forward.
