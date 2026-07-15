//! Tunable-default constants for the resolved configs (ADR-0021/0018/0039/0042/0061/0069). Each is
//! the fallback used when neither the file config nor the legacy env var sets a value — moved here
//! verbatim from the former single-file `config.rs`, with no change to any value.

/// Default ceiling on the diff pasted into the review prompt (chars).
pub const DEFAULT_MAX_DIFF_CHARS: usize = 60_000;

/// Default per-request timeout for the chat HTTP client (seconds). Deliberately **generous**: eaig can
/// legitimately take up to ~2 minutes to answer a turn, so an aggressive timeout would kill a
/// slow-but-valid response (ADR-0039). Overridable via `LLM_REQUEST_TIMEOUT_SECS` /
/// `review.request_timeout_secs`.
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 180;

/// Default number of **retries** (so total attempts = retries + 1) on a transient turn failure
/// (connect/timeout, HTTP 429, HTTP 5xx). 4xx other than 429 is deterministic and never retried.
/// Overridable via `LLM_MAX_RETRIES` / `review.max_retries`.
pub const DEFAULT_MAX_RETRIES: u32 = 2;

/// Default per-run circuit-breaker threshold: after this many *consecutive* turn failures the run
/// fails fast rather than burning the whole turn budget (ADR-0039). Overridable via
/// `LLM_CIRCUIT_BREAKER_THRESHOLD` / `review.circuit_breaker_threshold`.
pub const DEFAULT_CIRCUIT_BREAKER_THRESHOLD: u32 = 3;

/// Default ceiling on model turns before the run is cut off (each turn is one chat round-trip). On the
/// deepseek model a turn is ~6s and the agent records roughly one finding per turn, so the old ceiling
/// of 16 was far too tight for a real PR — a run could exhaust its budget with findings still buffered.
/// Operator-tunable via `review.max_turns` / `LLM_MAX_TURNS`. Overridable but defaults generously.
pub const DEFAULT_MAX_TURNS: usize = 40;

/// Default ceiling on how many read-only tool calls (search / graph / `read_file`) the runner executes
/// **concurrently** within one turn — the batch size for risk-first review (ADR-0042). The model can
/// emit many tool calls in a turn; we run up to this many in parallel so a batch of reads costs one
/// round-trip's latency instead of N. Read-only only: write/finish/abort keep their ordered, sequential
/// handling. Operator-tunable via `review.max_batch_size` / `LLM_MAX_BATCH_SIZE`.
pub const DEFAULT_MAX_BATCH_SIZE: usize = 8;

/// Default cumulative read budgets for risk-first review (ADR-0042): once a budget is spent the runner
/// drops the matching tool from the offered set and nudges the model to converge — so a review reads
/// *enough* to be confident, then stops, instead of grinding through the whole repo. `max_files_read`
/// caps `read_file` calls, `max_searches` caps retrieval (vector + graph) calls, and `max_batches` caps
/// investigation rounds (turns that issue ≥1 read-only call) before forcing the wind-down. Generous
/// defaults; operator-tunable via `review.*` / `LLM_MAX_*`.
pub const DEFAULT_MAX_FILES_READ: usize = 30;
pub const DEFAULT_MAX_SEARCHES: usize = 15;
pub const DEFAULT_MAX_BATCHES: usize = 6;

/// Default ceiling on coverage-gate bounces (ADR-0069, run bac4b5d8). The gate re-bounces an early
/// `finish` while changed files remain un-engaged, up to this cap — bounded so a model that will never
/// comply can't burn the budget arguing with the gate; whatever gets through incomplete carries a
/// coverage disclosure on the posted summary. Semantics of the knob (`review.<tier>.max_coverage_bounces`
/// / `LLM_MAX_COVERAGE_BOUNCES`): **`0` disables the bounce entirely** (the disclosure still applies),
/// `1` restores the pre-ADR-0069 bounce-once behaviour, higher values push a lazy model harder.
/// Deliberately NOT clamped to ≥1 — unlike the read budgets, zero is a meaningful setting here.
pub const DEFAULT_MAX_COVERAGE_BOUNCES: usize = 3;

/// SAST (opengrep) defaults (ADR-0061). The whole feature is **opt-in** (default disabled) so the
/// rollout is image-then-config: an existing deploy without the opengrep-bearing image is unaffected.
pub const DEFAULT_SAST_BIN: &str = "opengrep";
/// Vendored, pinned ruleset baked into the runner image (see the Dockerfile). A local dir, so scans are
/// hermetic — no registry fetch at runtime. Operator-overridable to narrow the rule set to their stack.
pub const DEFAULT_SAST_RULES: &str = "/opt/opengrep-rules";
/// Minimum SARIF level to surface. `error` (high-signal, mostly real security/correctness) by default so
/// the first rollout doesn't flood PRs; lower to `warning`/`note` to widen.
pub const DEFAULT_SAST_MIN_SEVERITY: &str = "error";
/// Cap on findings posted per review, so a pathological file can't bury the PR. Excess is logged, not
/// silently dropped (ADR-0033).
pub const DEFAULT_SAST_MAX_FINDINGS: usize = 25;
/// Wall-clock ceiling on one opengrep scan; on timeout the pass is abandoned (non-fatal).
pub const DEFAULT_SAST_TIMEOUT_SECS: u64 = 300;
