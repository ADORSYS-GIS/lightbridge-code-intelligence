# ADR-0084: Customer handbook as modular AsciiDoc (tiny files, composed deliverables)

- **Status:** Proposed (design-only)
- **Date:** 2026-07-11
- **Deciders:** @stephane-segning

## Context and Problem Statement

The repo's documentation (`docs/` — ADRs, RFCs, runbooks, architecture notes) is written for
**contributors**: it narrates decisions and internals, assumes the codebase, and changes with it.
There is nothing shareable with a **customer** — someone deploying Lightbridge (soon incl. the
one-process `shared` role, [ADR-0083](0083-platform-crate-architecture-and-cratestack-data-layer.md)),
operating it, and integrating their GitHub/GitLab. The owner's requirements: a customer-facing
handbook, authored as **many tiny AsciiDoc files** so that upgrades touch small, reviewable,
re-translatable units — not a monolithic manual that drifts wholesale.

## Decision Drivers

- **Tiny files = cheap upgrades.** A release that changes one config knob should change one
  ~50-line `.adoc`, and the customer-visible diff should say exactly that.
- **Composed deliverables.** Customers receive one polished artifact (HTML and/or PDF), not a file
  tree; the modularity is an *authoring* property.
- AsciiDoc over Markdown: real `include::`, attributes for product/version substitution,
  cross-references with stable anchors, PDF output — the features modular docs actually need.
- No new heavyweight toolchain: this repo already runs Rust + Node tooling; docs must build in CI
  and locally with one command.
- Contributor docs (`docs/`) stay as they are — this is a **second audience**, not a migration.

## Considered Options

- **A — plain Asciidoctor + an `include::` tree** (this ADR): tiny topic files composed by a
  master doc per deliverable; built by `asciidoctor`/`asciidoctor-pdf` via a `just` target + CI.
- **B — Antora.** The full multi-repo, multi-version AsciiDoc site generator. Right shape for a
  docs *site* with several versions live; a Node toolchain + component-descriptor structure that
  is overkill for "one handbook per release" today. Deferred, deliberately — the file layout below
  is Antora-compatible (`modules/…/pages` maps 1:1) so moving later is a re-arrangement, not a
  rewrite.
- **C — keep Markdown + a docs generator.** Loses includes/attributes/PDF; tiny-file composition
  in Markdown means preprocessor glue we would own forever.

## Decision Outcome

Chosen option: **A — modular Asciidoctor**, structured as follows.

### Layout (`docs/handbook/`)

```
docs/handbook/
  handbook.adoc              # master: the ONLY file that composes the full handbook (includes only)
  attributes.adoc            # product name, {version}, URLs, image tags — substituted everywhere
  nav.adoc                   # ordered include list (the one file that knows the reading order)
  overview/
    what-is-lightbridge.adoc # one concept per file, target ≤ ~80 lines each
    architecture-at-a-glance.adoc
  install/
    prerequisites.adoc
    helm-install.adoc
    single-node-shared-role.adoc     # the ADR-0083 `shared` role story
    forge-app-github.adoc
    forge-app-gitlab.adoc
  operate/
    roles-and-scaling.adoc           # the role matrix (serve/dispatcher/… /shared)
    configuration-reference.adoc     # generated section — see below
    upgrades.adoc
    backup-restore.adoc
    troubleshooting/
      reviews-not-posting.adoc       # one symptom per file
      webhook-not-received.adoc
  integrate/
    review-workflow.adoc             # @mention tiers, reactions, labels
    a2a-api.adoc                     # the A2A surface for programmatic callers
    push-notifications.adoc
  reference/
    glossary.adoc
    support-matrix.adoc
```

Rules that keep it healthy:

- **One concept per file, ≤ ~80 lines**; a file that grows past that splits. Every file starts
  with a stable anchor id (`[[install-helm]]`) so cross-refs survive moves.
- **Only `handbook.adoc` (via `nav.adoc`) composes**; topic files never `include::` each other —
  composition stays one-level and the reading order lives in exactly one place.
- **`attributes.adoc` owns every proper noun and version string** (`{lightbridge-version}`,
  `{chart-repo}`, image tags). A release bump is a one-line attribute change; no topic file ever
  hardcodes a version.
- **Customer voice only**: no ADR references, no internals, no "we/our" of the dev team. Where a
  contributor doc explains *why*, the handbook says *do this*; the two link in one direction only
  (contributor docs may cite the handbook, never the reverse).
- **`configuration-reference.adoc` is generated** (an `xtask` emits it from the config structs /
  Helm values schema) so the reference cannot drift from the code — the same
  docs-as-code doctrine as the Grafana dashboards (ADR-0046).

### Build & delivery

- `just handbook` → `asciidoctor` (single-file HTML with embedded assets) +
  `asciidoctor-pdf` (PDF), both stamped from `attributes.adoc`. Toolchain runs from the standard
  `asciidoctor/docker-asciidoctor` image locally and in CI — no host Ruby.
- CI builds the handbook on every PR touching `docs/handbook/` (a broken include/xref fails the
  build) and **attaches the HTML+PDF to each GitHub release** — the shareable, versioned artifact
  customers receive. Release notes link it.
- Because files are tiny and composition is central, a customer-specific variant (different
  attributes, a subset nav) is a second master doc, not a fork.

## Consequences

- **Good:** upgrades touch leaf files; diffs are small and reviewable; version strings change in
  one place; releases carry a polished, versioned handbook automatically.
- **Good:** Antora-compatible layout keeps the multi-version docs-site door open without paying
  for it now.
- **Bad:** a second documentation audience is a standing editorial cost — every user-visible
  change now owes a handbook touch. Mitigated by the PR template gaining a "handbook impact"
  line (like Verification), and by tiny files making the touch cheap.
- **Bad:** AsciiDoc is a second markup in a Markdown repo; contributors need the (small) syntax
  delta. Contained: only `docs/handbook/` uses it.
- **Neutral:** the contributor docs are unaffected; nothing migrates.

## More Information

- [ADR-0083](0083-platform-crate-architecture-and-cratestack-data-layer.md) — the `shared` role and
  role matrix the handbook's install/operate sections document.
- [ADR-0046](0046-observability-dashboard-deployment.md) — the docs-as-code/generated-artifact
  doctrine the configuration reference follows.
- Asciidoctor: <https://asciidoctor.org> · `asciidoctor-pdf` · the `docker-asciidoctor` image.
