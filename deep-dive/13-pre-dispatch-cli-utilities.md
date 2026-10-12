# 13. Pre-dispatch CLI utilities — the one path that runs before `AppState` even exists

File `01` §1.2-1.4 covers how `main.rs` resolves a *role* string and dispatches to one of seven
long-running `async fn(AppState) -> anyhow::Result<()>` functions. There is exactly one exception to
that uniform shape: `mint-runner-token`, which runs — and returns — **before `AppState::from_env()`
is ever called.** This file is about that exception: the general pattern it is, why it exists here,
and the exact code.

## 13.1 The general concept: not every "mode" of a program deserves the same setup cost

A long-running service and a one-shot CLI utility are different enough in *kind* that forcing them
through identical startup machinery is often a mistake, not a nicety — it's effort spent
constructing resources (a database pool, a secrets client, a full async executor's worth of
background tasks) that the one-shot path will never touch before exiting. The general answer,
independent of Rust, is: **check for the cheap, narrow-scoped command first, and only pay for the
expensive, broad-scoped setup once you know you actually need it.** This is the same shape as a CLI
tool handling `--help` or `--version` before touching its config file or network clients — those
don't need a database connection either, and forcing one to exist first would make the fast path
slow and the failure mode wrong (a missing `DATABASE_URL` shouldn't block `--version`).

## 13.2 Why this specific case exists: a credential bootstrap problem

```rust
// services/control-plane/src/main.rs:655-659 (comment immediately above the check)
// `mint-runner-token` is a local-dev/ops utility (ADR-0092), not a long-running role: it prints one
// token to stdout and exits. It needs only `RUNNER_TOKEN_SIGNING_KEY` from env, so it's handled
// before `AppState::from_env()` (which would otherwise require a database). Without it, running the
// agent-runner manually (docs/local-setup.md) has no way to obtain a token now that the internal API
// rejects anything but a validly-signed, task-scoped one — there is no shared dev bearer anymore.
```

The backstory is [ADR-0092](../docs/adr/0092-per-task-runner-tokens.md): the control plane used to
hand every agent Job the *same* long-lived bearer token (`AGENT_RUNNER_TOKEN`) to talk to its
internal API — simple, but a single leaked Job env var meant a credential valid against every task,
past and future, forever. ADR-0092's fix is per-task, short-lived, signed tokens instead — which
closes that hole, but creates a new practical problem: if you're running the agent-runner **by hand**
(debugging a stuck task, exercising the internal API locally — `docs/local-setup.md`), there's no
longer a shared dev bearer you can just paste in. `mint-runner-token` exists purely to let a human
mint one real, validly-scoped token on demand, from the same signing key the control plane itself
uses, without standing up the whole service.

## 13.3 The real code

```rust
// main.rs:660-664 — the interception point, inside main(), before role dispatch
if role == "mint-runner-token" {
    let result = mint_runner_token_cli();
    otel_guard.shutdown().await;
    return result;
}

let state = AppState::from_env().await?;   // <- only reached by the seven real roles
```

```rust
// main.rs:697-716 — the whole implementation
fn mint_runner_token_cli() -> anyhow::Result<()> {
    use anyhow::Context;

    let signer = runner_token::RunnerTokenSigner::from_env()
        .ok_or_else(|| anyhow::anyhow!("RUNNER_TOKEN_SIGNING_KEY is not set"))?;
    let task_id: uuid::Uuid = std::env::args()
        .nth(2)
        .or_else(|| std::env::var("TASK_ID").ok())
        .ok_or_else(|| anyhow::anyhow!("usage: control-plane mint-runner-token <task-id>"))?
        .parse()
        .context("task id is not a valid UUID")?;
    let deadline_seconds = std::env::var("AGENT_JOB_DEADLINE_SECONDS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|&secs| secs > 0)
        .unwrap_or(3600);
    let token = signer.mint(task_id, deadline_seconds)?;
    println!("{token}");
    Ok(())
}
```

Worth noting precisely: this is a **plain, synchronous-looking `fn`**, not `async fn` — it does no
I/O beyond reading env vars and parsing a UUID (`RunnerTokenSigner::mint` is a pure cryptographic
signing operation, no network, no database), so there's nothing here that needs `.await` at all. It
runs synchronously inside `main`'s async body, finishes in effectively zero time, and returns.

## 13.4 What this buys, mechanically

1. **No database dependency for this path.** `AppState::from_env()` — which this check sits in
   front of — is what builds the Postgres pool, the Neo4j handle, the GitHub App client, and
   everything else in the struct file `10` documents. `mint-runner-token` needs none of it; checking
   `role == "mint-runner-token"` *before* calling `AppState::from_env()` means a developer can run
   `control-plane mint-runner-token <task-id>` with nothing but `RUNNER_TOKEN_SIGNING_KEY` set —
   no `DATABASE_URL`, no live Postgres, no Neo4j — and it still works. Had the check come *after*
   `AppState::from_env()`, the `?` on that call would fail the whole command the instant any one of
   those other resources was unavailable, for a command that was never going to touch them anyway.
2. **The same `otel_guard.shutdown().await` runs on both paths.** Even the one-shot utility flushes
   its (trivial) telemetry before exiting — the shutdown call isn't duplicated by accident, it's
   simply called from both the early-return branch and the normal end-of-`main` path, so neither
   one skips cleanup.
3. **`return result;` — an early return out of `main` itself.** This is ordinary Rust control flow,
   nothing role-dispatch-specific: `main`'s body is just a sequence of statements like any other
   function, and `return` exits it immediately with whatever `anyhow::Result<()>` the utility
   produced, which `#[tokio::main]`'s generated wrapper turns into the process's exit code exactly
   as described for the seven real roles in file `01` §1.4.7 — a one-shot utility's failure is a
   non-zero exit code via the identical mechanism a role's failure would use, even though it never
   went anywhere near the `match`.

## 13.5 Why this isn't just an eighth arm of the `match`

It would be possible to make `mint-runner-token` an eighth branch of the role `match` in file `01`
§1.3 instead of a check beforehand. The reason it isn't is exactly the type-unification point from
file `01` §1.4.4: every arm of that `match` has the identical signature
`async fn(AppState) -> anyhow::Result<()>` — *because* the match requires one shared type across all
arms, and `AppState` is already built by the time the match runs. Folding `mint-runner-token` into
that match would force it to accept an `AppState` it doesn't want and can't always obtain (no DB,
remember), or would force every other role to special-case "maybe I don't actually have a real
`AppState`" — either way, it corrupts the one clean invariant the real roles share. Keeping it as a
plain `if` *before* that match, operating on nothing but the raw `role` string, keeps both
assumptions intact: the match's arms can all assume a fully-built `AppState`, and the utility can
assume nothing more than `std::env`.
