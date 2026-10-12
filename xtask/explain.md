# Xtask — `xtask/`

## What is it?

A **cargo-xtask** automation crate. It provides a single binary (`cargo xtask <task>`) that
implements all the local Rust development gates — invoked from the project's `justfile` so the
same commands work in CI and on a developer's machine (shift-left).

The pattern: rather than hiding CI logic in YAML where it can't be run locally, `xtask` wraps
it in Rust code that runs identically everywhere.

---

## Task list

| Task | What it does | CI equivalent |
|------|-------------|---------------|
| `cargo xtask ci` | Full gate: validate-schema → fmt → clippy → test | `rust-lint.yml` + local |
| `cargo xtask fmt` | `cargo fmt --all` | `cargo fmt --all -- --check` |
| `cargo xtask lint` | `cargo clippy --all-targets -- -D warnings` | `clippy` step in `rust-lint.yml` |
| `cargo xtask build` | `cargo build --workspace --all-targets` | — |
| `cargo xtask test` | `cargo nextest run` (fallback: `cargo test`) | `control-plane-tests.yml` |
| `cargo xtask validate-schema` | Lint `control-plane.cstack` against the 0.4.x grammar | — |

---

## CI task (`cargo xtask ci`)

Runs these in order:

1. **validate-schema** — lints the cratestack schema (`services/control-plane/schema/control-plane.cstack`)
   against the 0.4.x grammar so the schema-first source of truth can't silently drift from
   `src/types.rs`. Best-effort: skips with a hint when `cratestack-cli` is not installed.
2. **cargo fmt --all -- --check** — formatting gate
3. **cargo clippy --all-targets -- -D warnings** — lint gate
4. **test** — prefer `cargo nextest run`, fall back to `cargo test`

---

## How shell-outs work

Every command goes through the `run()` function, which transparently wraps the command in
[`chronic`](https://joeyh.name/code/moreutils/) when it is available on `PATH`:

- On **success**: output is swallowed (a passing gate stays quiet)
- On **failure**: output is printed in full
- Without `chronic`: runs the command directly

This keeps CI logs clean while preserving debuggability.

---

## Files

| File | Contents |
|------|----------|
| `Cargo.toml` | Package manifest: `xtask` (publish=false), depends only on `anyhow` |
| `src/main.rs` | Task dispatch + `ci`, `fmt`, `lint`, `build`, `test`, `validate-schema` implementations |

---

## How it's invoked

From the project `justfile`:

```just
ci:
    cargo xtask ci

fmt:
    cargo xtask fmt

lint:
    cargo xtask lint

build:
    cargo xtask build

test:
    cargo xtask test
```
