# ADR-0005: Adopt cratestack (schema-first Rust) for the control plane

- **Status:** Accepted
- **Date:** 2026-06-18
- **Updated:** 2026-06-19 — cratestack 0.4.x grammar confirmed; `.cstack` rewritten to it; codegen still deferred (see Update below).

## Context and Problem Statement

The Rust control plane exposes an HTTP API over Postgres, with typed clients (consumed by the web
app) and policy enforcement on procedures. Hand-writing the server, the SQLx layer, the client
types, and the policy checks separately invites drift between them. We want one source of truth.

## Decision Drivers

- Single source of truth for schema, routes, clients, and policies
- Reduced drift between server, database, and consumers
- Avoiding premature lock-in to an unstable codegen grammar

## Considered Options

- **cratestack (schema-first)** — define models, procedures, and policies in
  `services/control-plane/schema/control-plane.cstack`; generate Axum + SQLx server, typed clients,
  and policy enforcement.
- **Hand-written Axum + SQLx** — full control, but every layer maintained and synchronized by hand.

## Decision Outcome

Chosen option: **cratestack**, with **codegen wiring deferred** until the cratestack **0.4.x**
grammar is pinned. Until then, hand-written types in `services/control-plane/src/types.rs` mirror
the `.cstack` schema so the modelling work is captured and reviewable now. The `.cstack` file is
the intended source of truth; the hand-written types must be kept in sync with it until codegen is
enabled.

### Consequences

- Good, because the schema is captured and reviewable today without betting on an unstable grammar.
- Good, because once pinned, codegen removes the manual server/client/policy synchronization burden.
- Bad, because until codegen is wired, `.cstack` and `types.rs` must be kept in sync by hand.
- Neutral, because banking-grade primitives (idempotency keys, optimistic locking, audit logging)
  are a follow-up once the grammar is fixed.

## Update (2026-06-19)

cratestack reached **0.4.9** (published 2026-06-17). The grammar is now confirmed against its docs
to be **Prisma-like**: `datasource`/`auth`/`type` blocks, models with `@relation`, `@@allow` policy
expressions, and proc-macro codegen (`include_server_schema!`) rather than a CLI/xtask codegen step.

The earlier `.cstack` draft was written in an invented grammar (`Bool`, `@auto`, `@ref(...)`,
`policy: public`, `-> Type`) that does **not** parse under 0.4.x. It has been rewritten to the real
grammar, and `just validate-schema` lints it (best-effort; skips when `cratestack-cli` is absent).

**Codegen remains deferred.** At ~two days old and a few hundred downloads, 0.4.x is too young to
bind a "banking-grade" control plane to; `src/types.rs` stays the hand-written, compiled source. Two
constructs the public docs do not specify — procedure-level policy syntax and the `autoincrement`
default — are flagged `# TODO(grammar)` in the schema and must be confirmed with the validator before
codegen is enabled. Revisit adoption once cratestack matures.

## Update (2026-07-11)

Revisited: [ADR-0083](0083-platform-crate-architecture-and-cratestack-data-layer.md) **executes
this ADR's intent and supersedes the deferral clause** — cratestack (`cratestack-pg`, still 0.4.9)
is adopted as the **data layer only** (generated SQLx-backed delegates behind repository traits in
a dedicated `lci-data` crate; generated routes not mounted), with the `.cstack` regenerated
from the live 27-migration schema and a CI drift gate. The "whole-server codegen" bet this ADR
declined stays declined; the containment that makes ORM-only adoption acceptable is argued there.
