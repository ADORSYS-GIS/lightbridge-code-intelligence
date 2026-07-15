# lci-config

The shared **configuration loader** for the Rust services. Both the
[control plane](../control-plane/README.md) and the [agent runner](../agent-runner/README.md) read a
single JSON config file (mounted from a Helm ConfigMap) instead of a sprawl of individual env vars.

## What it does

- **One JSON file, not many env vars.** A service deserializes its typed config from one document, so
  the Helm chart has a single declarative source of truth per environment.
- **`{env:VAR:-default}` substitution.** Every string in the parsed tree — and the contents of any
  template files the config points at — supports `{env:NAME}` / `{env:NAME:-fallback}` expansion
  *before* typed deserialization. Secrets and per-environment values stay in env (secret-injected),
  while the config and templates stay declarative.
- **Best-effort by design.** Loading is best-effort *at the call site*: a missing config path means
  "use built-in defaults / legacy env", so a service keeps running until the ConfigMap is mounted.

## Why JSON (not YAML)

JSON keeps the dependency surface at zero beyond `serde` — and templates are *separate mounted files*,
so the config itself stays scalars and paths only.

The design rationale lives in [`src/lib.rs`](src/lib.rs) (crate-level `//!` docs); the
implementation is split by responsibility into [`src/substitute.rs`](src/substitute.rs) (the
`{env:…}` grammar), [`src/loader.rs`](src/loader.rs) (reading + deserializing config/template
files), and [`src/de.rs`](src/de.rs) (the numeric/bool serde coercions).

## Tests

`cargo nextest run -p lci-config` — substitution edge cases are covered with **tempfile**
(no external state needed).
