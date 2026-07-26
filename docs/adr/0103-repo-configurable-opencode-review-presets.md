# ADR-0103: Repo-configurable OpenCode review presets replace the fast/deep tier model

- **Status:** Accepted
- **Date:** 2026-07-26
- **Deciders:** @stephane-segning
- **Supersedes:** [ADR-0062](0062-two-tier-review-fast-auto-deep-on-demand.md), [ADR-0069](0069-review-tier-minimum-model-capability.md)
- **Extends:** [ADR-0099](0099-operator-opencode-config-overlay.md), [ADR-0097](0097-review-runs-on-opencode.md)

## Context and Problem Statement

[ADR-0062](0062-two-tier-review-fast-auto-deep-on-demand.md) fixed review to exactly two named
tiers — `fast` (auto, every PR) and `deep` (on `@mention`) — each backed by one operator-configured
model block (`review.fast`/`review.deep`), with tier chosen by **hardcoded webhook-event-type
logic** (`services/control-plane/src/http/webhook.rs`, `a2a/handler/lifecycle.rs`,
`queue/reaper.rs`). [ADR-0069](0069-review-tier-minimum-model-capability.md) then bolted a
documented (not code-enforced) capability floor onto `deep` after a flash-class model produced a
bad review. Two problems follow from the fixed-tier shape: (1) a repo cannot choose its own
review posture — every repo gets the same two operator-wide tiers — and (2) "fast" and "deep" are
now, post-#488, identical in tools/gates/prompt shape and differ only by model + `max_cycles` +
system-prompt text, i.e. they were already becoming instances of one underlying concept
(a named OpenCode agent configuration) rather than two structurally different loops. Separately,
[ADR-0099](0099-operator-opencode-config-overlay.md) already lets an operator supply a full
OpenCode config overlay for review — most of the mechanism this ADR needs already exists; what's
missing is a **repo-scoped selection** across a small named set of presets, not just one global
overlay.

## Decision Drivers

- A repo owner, not just the platform operator, should be able to pick how aggressively/expensively
  their repo gets reviewed.
- "Fast" and "deep" as fixed, code-known tier names block adding a third posture (e.g. an
  `ultra` frontier-model pass) without another round of hardcoded branching.
- Every preset must present the **same tools and the same system prompt** to the model — the
  only per-preset knobs are model, budgets (`max_cycles`, `max_batch_size`, etc.), and
  `reasoning_effort`. Divergent tool/gate surfaces between presets is exactly the class of bug
  PR #488 just closed for fast/deep; the new design must make that bug class structurally
  impossible, not just fixed once.
- Reuse [ADR-0099](0099-operator-opencode-config-overlay.md)'s overlay mechanism rather than
  inventing a second config-merge path.

## Considered Options

- **A — Keep two hardcoded tiers, add per-repo model override only.** Rejected: doesn't solve the
  fixed-name problem, and `ultra` would still need new hardcoded branches everywhere `"fast"`/`"deep"`
  string-matches today (`ReviewConfigs::for_tier`, the tier column CHECK, webhook dispatch).
- **B — Repo-configurable named presets, resolved from repo config, uniform tools/prompt.** A repo's
  `.lightbridge-code-review.jsonc` ([ADR-0030](0030-repo-review-config.md)) names which preset an
  entry point uses; the control plane resolves `(repo, entry-point) → preset name → OpenCode agent
  config`, with `fast`/`deep`/`ultra` shipped as the default preset set an operator can extend.
  Chosen.
- **C — One preset per repo, no per-entry-point mapping.** Rejected: loses the existing
  auto-PR-open-vs-@mention distinction the operator already relies on for cost control; entry-point
  → preset mapping is cheap to keep and the user explicitly asked for it.

## Decision Outcome

Chosen option: **B**. `tier: String` (`"fast"`/`"deep"`) is replaced end-to-end by `preset: String`,
resolved per `(repo, entry-point)` from repo config, falling back to a platform-default mapping when
a repo hasn't configured one. Every preset renders through the **same** OpenCode agent config
(tools, MCP wiring, system prompt) that [ADR-0099](0099-operator-opencode-config-overlay.md)/[ADR-0097](0097-review-runs-on-opencode.md)
already build — presets differ only in the `ReviewConfig` fields that were always meant to vary
(`model`, `base_url`, `api_key`, `extra.reasoning_effort`, `max_cycles`, `max_batch_size`,
`max_files_read`, `max_searches`, `max_batches`, `max_coverage_bounces`). The `fast: bool`
structural flag and any remaining gate short-circuits tied to it are removed — PR #488 already
proved gate parity works; this ADR makes divergence impossible by construction instead of by
discipline. `ultra` ships as a third default preset pointed at the platform's best available model,
with a wider budget than `deep`.

### Consequences

- Good, because a repo can move between `fast`/`deep`/`ultra` (or an operator-defined fourth preset)
  through its own config file, without a control-plane code change or redeploy.
- Good, because there is exactly one code path that renders an OpenCode agent config from a
  `ReviewConfig` — no per-preset branch can silently drop a tool or gate again.
- Bad, because the `tasks.tier` column and every `tier: "fast"|"deep"` call site
  (webhook.rs, a2a/handler/lifecycle.rs, queue/reaper.rs, db/tasks.rs) needs a rename/migration to
  `preset`; this is a schema + call-site sweep, tracked as its own story, not a decision risk.
- Neutral, because the model-capability-floor idea from ADR-0069 doesn't disappear — it's restated
  as guidance per-preset ("a preset named for heavy/on-demand review should carry a frontier-class
  model") rather than pinned to a fixed tier name; still not code-enforced, for the same reason
  ADR-0069 gave (no reliable way to classify capability from a gateway alias).

## More Information

Per-identity (org/user/repo) model selection + ACL (the ADR-0038 upgrade carried forward from
Epic #241) is a natural extension of the same resolution path and is tracked as a follow-on story
under the epic implementing this ADR, not a separate mechanism.
