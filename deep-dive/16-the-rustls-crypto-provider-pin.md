# 16. The crypto-provider pin — one line that prevents a guaranteed startup panic

A single line runs before anything else in both long-running Rust binaries:

```rust
// services/control-plane/src/main.rs:636 (first real statement in main(), before role resolution)
lci_observability::install_default_crypto_provider();
```

```rust
// services/agent-runner/src/run.rs:52 (first statement in run_once(), before tracing init)
lci_observability::install_default_crypto_provider();
```

This file is about what that one call actually does, why it has to run *first* — before even
OTel/tracing init — and what happens if it's skipped.

## 16.1 The concept: rustls doesn't ship its own cryptography

`rustls` (the TLS library every HTTPS client in this workspace uses — no OpenSSL in the runtime
images) implements the TLS *protocol state machine*, but deliberately contains no cryptographic
primitives of its own. Since rustls 0.22, the actual AES/ChaCha20/ECDSA/key-exchange/RNG
implementations come from a separate, pluggable `CryptoProvider`. Two are popular in the Rust
ecosystem and both are relevant here:

- **`ring`** — wraps a long-audited C/assembly crypto core (originally BoringSSL's).
- **`aws-lc-rs`** — wraps AWS-LC, a BoringSSL fork AWS maintains and audits.

A process can have **multiple** `CryptoProvider` implementations *linked* (different crates in the
dependency graph can each depend on a different one) — that's normal and harmless by itself. The
problem is narrower: rustls needs exactly **one *process-default*** provider to hand to any TLS
config that doesn't name one explicitly, and if more than one implementation is linked, rustls
**will not guess** which one you meant.

## 16.2 Why this workspace actually hits the problem

This isn't a hypothetical — this workspace genuinely links both providers, confirmed directly in
the root `Cargo.toml`:

```toml
# Cargo.toml:86-93
# rustls everywhere (no OpenSSL in the runtime images).
reqwest = { version = "0.13", default-features = false, features = ["json", "rustls", "stream", "form"] }
# Explicit rustls handle so the control-plane binary can pin ONE crypto provider at startup. Two are
# linked in this workspace — `ring` (via sqlx's `tls-rustls-ring-webpki`) and `aws-lc-rs` (pulled
# transitively by `rmcp` → reqwest 0.13 → rustls-platform-verifier). With both present, rustls 0.23
# can't auto-select a process-default provider and panics on the first TLS handshake; we install
# `ring` up front in `main`. Only control-plane pulls both, so only it needs the handle.
rustls = { version = "0.23.41", default-features = false, features = ["ring"] }
```

Two independent, perfectly reasonable dependency choices collide:

- `sqlx`'s Postgres driver is configured with the `tls-rustls-ring-webpki` feature (`Cargo.toml:116`)
  — sqlx's rustls integration, backed by `ring`.
- `rmcp` (the MCP client/server crate) pulls `reqwest` 0.13, which pulls `rustls-platform-verifier`,
  which is backed by `aws-lc-rs` (`Cargo.toml:142-149`, comment at `Cargo.toml:90`).

Neither crate did anything wrong — they each picked a reasonable rustls backend independently.
The *workspace*, by depending on both, ends up with both providers linked into the same binary.
A third rustls consumer, `kube` (`Cargo.toml:124`, `features = ["client", "rustls-tls"]` — the
dispatcher's Kubernetes Job-launching client), rides whichever provider its own TLS stack resolves
to, adding a third place this ambiguity could surface.

Confirmed as a closed, audited non-issue for one more consumer: the A2A SDK's reqwest client also
rides rustls 0.23 on `aws-lc-rs` — "the SAME generation `rmcp` already pulls, so this adds NO third
crypto provider beyond the `ring` process-default `main` installs" (`Cargo.toml:156-158`). That
line matters: it's evidence someone *checked* the provider lineage for each new TLS-using
dependency added after this fix landed, rather than assuming the pin would keep working.

## 16.3 The fix itself

```rust
// services/observability/src/lib.rs:19-32
/// Pin the process-default rustls crypto provider before anything builds a TLS client. Moved here
/// (was inline in `control-plane/src/main.rs`) so both binaries share one audited call site —
/// `agent-runner` pulls both `ring` and `aws-lc-rs` in its production binary too (via the
/// workspace's `rustls` feature pin and `reqwest`'s `rustls-platform-verifier` respectively) and
/// had no pin at all before this crate existed. Idempotent: a benign `Err` means a provider is
/// already installed (e.g. a re-entrant test), which is left in place.
pub fn install_default_crypto_provider() {
    if rustls::crypto::ring::default_provider()
        .install_default()
        .is_err()
    {
        tracing::warn!("a rustls CryptoProvider was already installed; keeping the existing one");
    }
}
```

Three lines of actual logic:

1. `rustls::crypto::ring::default_provider()` builds rustls's own `ring`-backed `CryptoProvider`
   value — just a struct in memory, nothing global yet.
2. `.install_default()` tries to install it as *the* process-wide default, exactly once per
   process (it's backed by an `OnceLock` internally in rustls).
3. If that fails — meaning something else already won the race and installed a provider first —
   the function doesn't panic or error out; it logs a warning and leaves the existing provider in
   place. Calling this function twice (a second role in a test harness, a re-entrant call) is
   therefore always safe.

Every TLS consumer in the process — sqlx's Postgres connection, `kube`'s client, reqwest's client
(used by `rmcp`, the OTel OTLP exporter, the A2A SDK, the embeddings client) — from this point on
resolves "which crypto backend" the same way: whatever `ring::default_provider()` installed here,
regardless of which provider feature *that particular crate* happened to compile with. The pin
doesn't change what any crate does; it answers the one question rustls refuses to answer on its
own.

## 16.4 Why it has to run *first*, specifically

Both call sites put this before anything else — before role resolution in `main.rs`, before even
`init_tracing()` (which builds the OTel OTLP exporter's own reqwest-based HTTP client):

```rust
// services/control-plane/src/main.rs:626-636 (trimmed to the ordering)
async fn main() -> anyhow::Result<()> {
    // Pin the process-default rustls crypto provider before anything builds a TLS client — including
    // the OTel OTLP exporter's own client, set up below. ...
    lci_observability::install_default_crypto_provider();

    // One binary, several roles (RFC-0001): `serve` (HTTP) and `dispatcher` (queue consumer), ...
    let role = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("CONTROL_PLANE_ROLE").ok())
        .unwrap_or_else(|| "serve".to_string());

    let otel_guard = lci_observability::init_tracing(&format!("control-plane-{role}"));
    ...
```

`install_default()` only succeeds if it's the *first* thing to try to pick a process default. If
some other code path built a TLS client and triggered rustls's internal auto-detection first, two
things could happen: either that auto-detection already panicked (because, as established in
§16.2, more than one provider is linked and auto-detection can't choose), or — if only one provider
happened to get exercised first — some *other* provider could win the race and this call would
hit its own `Err` branch, silently keeping a provider the rest of the comment didn't intend. Either
way, "first" is load-bearing, not cosmetic.

## 16.5 What actually happens without it

This isn't a theoretical risk the comment is hedging against — it's a **documented incident**,
named directly in the source:

> "with both present rustls 0.23 can't auto-select a default and panics on the first handshake
> (`"Could not automatically determine the process-level CryptoProvider"`). **That bit the
> `dispatcher` role at startup when it builds its kube client.**"

Concretely, without this call: the control-plane binary starts, resolves its role, and at some
point — for the `dispatcher` role, the moment it constructs its `kube::Client` to be able to launch
Kubernetes Jobs — rustls attempts its first real TLS handshake. Finding two linked
`CryptoProvider`s and no installed default, it panics immediately with that exact message. Not a
graceful error, not a retry, not a degraded mode: the process crashes on startup, every time,
deterministically, for that role. In a Kubernetes Deployment, that's an instant crash-loop — the
dispatcher Pod would never reach a healthy state at all.

Which role hits it first isn't fixed by the bug itself; it depends on which rustls-backed client
the role's own startup path constructs first (`kube` for `dispatcher`, potentially sqlx or reqwest
for another role). The pin makes the question moot — there is no "first handshake wins" race left
to depend on, because the default is already settled before any handshake is attempted.

## 16.6 Why `ring`, specifically — not `aws-lc-rs`

The choice isn't arbitrary; `ring` was already the "more pinned-in" dependency on two other counts
in the same workspace:

**`jsonwebtoken`'s own crypto backend** needs the same kind of explicit choice, for a related but
distinct reason — not two *linked* providers colliding, but a crate with *no default backend at
all*:

```toml
# Cargo.toml:101-106
# `rust_crypto` (ring) is jsonwebtoken's crypto backend — NOT a default feature. It used to be
# switched on transitively by restate-sdk; with the Restate egress path removed (ADR-0093) the sole
# jsonwebtoken user (control-plane) must enable it explicitly, else it panics with "exactly one of
# 'rust_crypto' and 'aws_lc_rs'". Still true on v11: the crypto backend remains an explicit choice
# and neither backend is in `default`. ring matches the process-level provider `main` installs.
jsonwebtoken = { version = "11", features = ["rust_crypto"] }
```

Worth noting as its own small lesson: this one was a **latent** bug for a while — `restate-sdk`
used to pull in a crypto backend for `jsonwebtoken` as a side effect, so the workspace "worked" by
accident. Removing Restate (ADR-0093) silently removed that transitive feature too, which would
have reintroduced the exact same class of panic (`jsonwebtoken` verifying a JWT is also a crypto
operation requiring a chosen backend) had nobody caught it. `features = ["rust_crypto"]` makes the
dependency explicit instead of borrowed.

**`ring::aead` is reused directly**, not just through rustls's abstraction, for encrypting the A2A
push-notification webhook token at rest:

```toml
# Cargo.toml:186-190
# AEAD (ChaCha20-Poly1305) for encrypting the A2A webhook auth token at rest (ADR-0079 §3). `ring` is
# already this workspace's single pinned rustls crypto provider (installed process-wide in
# control-plane's `main`), so `ring::aead` reuses an audited, already-compiled primitive — it adds no
# new crypto crate and no second provider. Version-matched to the transitive `ring` sqlx already pulls.
ring = "0.17.14"
```

So the real decision wasn't "which provider is faster" — it's "which provider is *already*
load-bearing elsewhere in this binary for an unrelated reason, so pinning it avoids compiling and
auditing a second cryptographic implementation at all." `aws-lc-rs` stays linked (because `rmcp`
and the A2A SDK need it to build), it simply never gets *used* at the process-default level.

## 16.7 The shape of the fix: one audited call site, not two

The function itself carries its own migration history in its doc comment: it "was inline in
`control-plane/src/main.rs`" originally, then moved into the shared `lci-observability` crate
specifically because `agent-runner` turned out to need the identical fix — it also links both
providers in its production binary (the same workspace `rustls` feature pin, plus `reqwest`'s
`rustls-platform-verifier`) — and "had no pin at all before this crate existed." Consolidating onto
one function means a future change to *how* the pin works (a different provider, a different
install strategy) happens in one place both binaries pick up, instead of two copies drifting apart
the way the `jsonwebtoken` backend nearly did by accident (§16.6).

## 16.8 What to read next

- File [`01`](01-control-plane-roles.md) §1.4 for the general multi-call-binary pattern
  `install_default_crypto_provider()` sits at the very front of — it runs before role resolution,
  so every one of the seven roles gets the same guarantee regardless of which one is selected.
- File [`09`](09-observability.md) for the rest of what `lci-observability`/`init_tracing` sets
  up immediately after this call (the OTel OTLP exporter, the JSON tracing layer) — the exporter's
  own reqwest client is one of the TLS clients this pin exists to protect.
