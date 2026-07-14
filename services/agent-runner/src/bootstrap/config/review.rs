//! [`ReviewConfig`] — the native review agent's LLM config (ADR-0018/0026/0037/0039/0042/0045/0062/
//!0066/0069): resolution from the file config or env, two-tier (fast/deep) fan-out, and the
//! resilience/budget knobs. The audit-trail redaction of a resolved config lives in
//! [`super::redact`] — a separate concern kept out of this already-large module.

use std::path::Path;

use anyhow::Context;

use super::defaults::{
    DEFAULT_CIRCUIT_BREAKER_THRESHOLD, DEFAULT_MAX_BATCH_SIZE, DEFAULT_MAX_BATCHES,
    DEFAULT_MAX_COVERAGE_BOUNCES, DEFAULT_MAX_DIFF_CHARS, DEFAULT_MAX_FILES_READ,
    DEFAULT_MAX_RETRIES, DEFAULT_MAX_SEARCHES, DEFAULT_MAX_TURNS, DEFAULT_REQUEST_TIMEOUT_SECS,
};
use super::env::{parse_env_u64, require, require_field};
use super::file::{FileConfig, ReviewFile, ReviewTool, ReviewToolSelector};

/// Configuration for the native review agent's LLM — an OpenAI-compatible Chat Completions endpoint
/// (the eaig gateway in prod; ADR-0018/0026). Like embeddings, **no default model** so a misconfigured
/// Job fails loudly. Optional as a whole: absent `LLM_MODEL`, the runner skips the review step
/// (indexing-only).
#[derive(Debug, Clone)]
pub struct ReviewConfig {
    /// Base URL of the OpenAI-compatible Chat Completions endpoint.
    pub base_url: String,
    /// API key for the gateway.
    pub api_key: String,
    /// Chat model id.
    pub model: String,
    /// The reviewer's *guidance* (persona + what to focus on), from the `review.system_prompt_file`
    /// template (file config) or `REVIEW_SYSTEM_PROMPT` (env). **Required — there is no built-in
    /// default** (ADR-0037): a prompt this central is operational config, owned in the ai-helm chart
    /// (`config.reviewSystemPrompt`), not a stale code constant. If review is enabled but no prompt is
    /// configured, [`resolve`]/[`from_env`] error (fail closed) rather than running a weak fallback.
    pub system_prompt: String,
    /// Ceiling on the diff pasted into the prompt; from `review.max_diff_chars` or the default.
    pub max_diff_chars: usize,
    /// Ceiling on model turns before the run is cut off; from `review.max_turns` (or `LLM_MAX_TURNS`
    /// env) or [`DEFAULT_MAX_TURNS`]. Operator-tunable so a large PR isn't truncated by a tight budget.
    pub max_turns: usize,
    /// Max read-only tool calls executed concurrently within one turn (ADR-0042); from
    /// `review.max_batch_size` (or `LLM_MAX_BATCH_SIZE`) or [`DEFAULT_MAX_BATCH_SIZE`]. Clamped to ≥1.
    pub max_batch_size: usize,
    /// Cumulative read budgets (ADR-0042): once spent, the matching tool is dropped and the model is
    /// nudged to converge. From `review.max_*` (or `LLM_MAX_*`) or the `DEFAULT_MAX_*` constants.
    pub max_files_read: usize,
    pub max_searches: usize,
    pub max_batches: usize,
    /// Coverage-gate bounce cap (ADR-0069): how many times an early `finish` with un-engaged changed
    /// files is bounced before it goes through (with a coverage disclosure). From
    /// `review.max_coverage_bounces` (or `LLM_MAX_COVERAGE_BOUNCES`) or
    /// [`DEFAULT_MAX_COVERAGE_BOUNCES`]. `0` disables the bounce; NOT clamped — zero is meaningful.
    pub max_coverage_bounces: usize,
    /// Model context window in tokens (ADR-0045). `Some(n)` enables conversation budgeting: the loop
    /// winds down + trims old tool output as the estimate nears `n`, and finalizes (never discards
    /// findings) on an overflow error. `None` = no budgeting. From `review.context_window` /
    /// `LLM_CONTEXT_WINDOW`; a 0 is treated as unset.
    pub context_window: Option<usize>,
    /// Generation params for the review model. `None` = provider/model default.
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_tokens: Option<i64>,
    /// Provider-specific passthrough request fields (reasoning budget etc.), from `review.extra`,
    /// merged verbatim into the chat body. Empty = nothing extra.
    pub extra: serde_json::Map<String, serde_json::Value>,
    /// Stream the chat response (SSE) and reassemble client-side (ADR-0039 / #206). From
    /// `review.stream`, else the `LLM_STREAM` env, else false.
    pub stream: bool,
    /// Resilience policy for the LLM transport: timeout, retry/backoff, circuit breaker (ADR-0039).
    /// Always present (defaults applied at resolve time). These are the **primary** model's per-request
    /// knobs + the loop-level breaker.
    pub resilience: ResilienceConfig,
    /// FAST tier (ADR-0062): when `true` the agent runs a single diff-only turn with **no retrieval
    /// tools** and no investigation loop (SAST still posts independently). This is a **per-task** flag —
    /// NOT from file config — set by `main.rs` from the task context's `tier` (`fast` → `true`); a Job
    /// runs one task, so mutating it per-run on the resolved config is sound. Defaults to `false` (deep).
    pub fast: bool,
    /// Per-tier tool allowlist (ADR-0062 + ADR-0066): when `Some`, the authoritative set of tools this
    /// tier offers the model (a non-empty list of [`ReviewToolSelector`] — built-in names + `mcp__`
    /// regex selectors). `None` = the built-in default for the tier. From `review.<tier>.tools`.
    pub tools: Option<Vec<ReviewToolSelector>>,
}

/// Resilience policy for the review LLM transport (ADR-0039). eaig can legitimately take ~2 minutes
/// per turn, so the timeout is deliberately generous; retries are bounded and only fire on transient
/// failures; and a per-run circuit breaker fails fast before the turn budget is exhausted.
#[derive(Debug, Clone)]
pub struct ResilienceConfig {
    /// Per-request timeout (seconds) for one chat round-trip.
    pub request_timeout_secs: u64,
    /// Retries on a transient turn failure (total attempts = `max_retries + 1`).
    pub max_retries: u32,
    /// Consecutive turn-failures before the per-run circuit breaker trips and the run fails fast.
    pub circuit_breaker_threshold: u32,
}

impl Default for ResilienceConfig {
    fn default() -> Self {
        Self {
            request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
            max_retries: DEFAULT_MAX_RETRIES,
            circuit_breaker_threshold: DEFAULT_CIRCUIT_BREAKER_THRESHOLD,
        }
    }
}

impl ResilienceConfig {
    /// From env vars, each falling back to the safe default when unset/unparseable. Used by the
    /// env-config path; the file-config path builds this from the `review.*` fields.
    fn from_env() -> Self {
        Self {
            request_timeout_secs: parse_env_u64("LLM_REQUEST_TIMEOUT_SECS")
                .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS),
            max_retries: parse_env_u64("LLM_MAX_RETRIES")
                .map(|n| n as u32)
                .unwrap_or(DEFAULT_MAX_RETRIES),
            circuit_breaker_threshold: parse_env_u64("LLM_CIRCUIT_BREAKER_THRESHOLD")
                .map(|n| n as u32)
                .unwrap_or(DEFAULT_CIRCUIT_BREAKER_THRESHOLD),
        }
    }
}

/// Whether streaming is enabled via the legacy/local `LLM_STREAM` env toggle (`"1"` = on). The
/// file-config `review.stream` takes precedence over this; it exists so a local run or a stale
/// deploy can still opt in without an `agent.json` change.
fn stream_from_env() -> bool {
    std::env::var("LLM_STREAM").ok().as_deref() == Some("1")
}

impl ReviewConfig {
    /// Returns `None` when `LLM_MODEL` is unset (review disabled), `Err` when it's set but the
    /// base URL / key are missing (a misconfiguration we want to surface, not silently skip).
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        if std::env::var("LLM_MODEL")
            .ok()
            .filter(|v| !v.is_empty())
            .is_none()
        {
            return Ok(None);
        }
        Ok(Some(Self {
            base_url: require("LLM_BASE_URL")?,
            api_key: require("LLM_API_KEY")?,
            model: require("LLM_MODEL")?,
            system_prompt: require_system_prompt(None)?,
            max_diff_chars: DEFAULT_MAX_DIFF_CHARS,
            max_turns: parse_env_u64("LLM_MAX_TURNS")
                .map(|n| n as usize)
                .unwrap_or(DEFAULT_MAX_TURNS)
                .max(1),
            max_batch_size: parse_env_u64("LLM_MAX_BATCH_SIZE")
                .map(|n| n as usize)
                .unwrap_or(DEFAULT_MAX_BATCH_SIZE)
                .max(1),
            max_files_read: parse_env_u64("LLM_MAX_FILES_READ")
                .map(|n| n as usize)
                .unwrap_or(DEFAULT_MAX_FILES_READ)
                .max(1),
            max_searches: parse_env_u64("LLM_MAX_SEARCHES")
                .map(|n| n as usize)
                .unwrap_or(DEFAULT_MAX_SEARCHES)
                .max(1),
            max_batches: parse_env_u64("LLM_MAX_BATCHES")
                .map(|n| n as usize)
                .unwrap_or(DEFAULT_MAX_BATCHES)
                .max(1),
            // Deliberately unclamped: 0 = bounce disabled (ADR-0069).
            max_coverage_bounces: parse_env_u64("LLM_MAX_COVERAGE_BOUNCES")
                .map(|n| n as usize)
                .unwrap_or(DEFAULT_MAX_COVERAGE_BOUNCES),
            context_window: parse_env_u64("LLM_CONTEXT_WINDOW")
                .map(|n| n as usize)
                .filter(|&n| n > 0),
            temperature: None,
            top_p: None,
            max_tokens: None,
            // No env var for arbitrary passthrough params — the file path (`review.extra`) is where a
            // reasoning budget is set.
            extra: serde_json::Map::new(),
            stream: stream_from_env(),
            resilience: ResilienceConfig::from_env(),
            fast: false, // per-task; main.rs sets it from the task tier (ADR-0062)
            // No env knob for a per-tier tool allowlist — the file path (`review.<tier>.tools`) is where
            // an operator declares it; env config uses the built-in per-tier defaults.
            tools: None,
        }))
    }

    /// Resolve from the file config when it carries a `review` block (with a non-empty model), else
    /// from env. The system prompt comes from the `system_prompt_file` template (env-subst'd) when
    /// set. A `review` block whose model is empty disables review, same as an unset `LLM_MODEL`.
    pub fn resolve(file: Option<&FileConfig>) -> anyhow::Result<Option<Self>> {
        Self::resolve_with_env(file, &|name| std::env::var(name).ok())
    }

    fn resolve_with_env(
        file: Option<&FileConfig>,
        env: &impl Fn(&str) -> Option<String>,
    ) -> anyhow::Result<Option<Self>> {
        let Some(r) = file.and_then(|f| f.review.as_ref()) else {
            return Self::from_env();
        };
        Self::from_review_file_with_env(r, env)
    }

    /// Resolve ONE review block — the flat `review.*`, or a per-tier `review.fast`/`review.deep` block,
    /// each a *complete* config (ADR-0062) — into a [`ReviewConfig`]. `Ok(None)` when the model is empty
    /// (review disabled). Any nested `fast`/`deep` on `r` is ignored (tiers don't nest).
    fn from_review_file(r: &ReviewFile) -> anyhow::Result<Option<Self>> {
        Self::from_review_file_with_env(r, &|name| std::env::var(name).ok())
    }

    fn from_review_file_with_env(
        r: &ReviewFile,
        env: &impl Fn(&str) -> Option<String>,
    ) -> anyhow::Result<Option<Self>> {
        if r.model.trim().is_empty() {
            return Ok(None); // review explicitly disabled
        }
        // Primary effective values (file wins, else env, else default).
        let context_window = r
            .context_window
            .or_else(|| parse_env_u64("LLM_CONTEXT_WINDOW").map(|n| n as usize))
            .filter(|&n| n > 0);
        let temperature = r.temperature;
        let top_p = r.top_p;
        let max_tokens = r.max_tokens;
        // `review.extra`: a free-form object of provider-specific params (reasoning budget etc.). Only
        // a JSON object is meaningful as request fields; a non-object is a misconfiguration — warn and
        // ignore it rather than silently dropping it (review still runs; the tuning just doesn't apply).
        let extra = match r.extra.as_ref() {
            Some(serde_json::Value::Object(o)) => o.clone(),
            None => serde_json::Map::new(),
            Some(_) => {
                tracing::warn!(
                    "review.extra is not a JSON object; ignoring it (expected a map of request fields)"
                );
                serde_json::Map::new()
            }
        };
        let request_timeout_secs = r
            .request_timeout_secs
            .or_else(|| parse_env_u64("LLM_REQUEST_TIMEOUT_SECS"))
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS);
        // Per-tier tool allowlist (ADR-0062 + ADR-0066): entries are already validated at deserialize
        // (built-in name or a compiled `mcp__` regex). Only an EMPTY list needs rejecting here — a
        // tier with no tools can't act.
        let tools = match &r.tools {
            Some(t) if t.is_empty() => anyhow::bail!(
                "review.tools is set but empty — a tier with no tools can't act. Remove the key (use \
                 the built-in default) or list at least one built-in ({}) or `mcp__<server>__<tool>` \
                 regex selector.",
                ReviewTool::ALL
                    .iter()
                    .map(|t| t.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Some(t) => Some(t.clone()),
            None => None,
        };
        let max_retries = r
            .max_retries
            .or_else(|| parse_env_u64("LLM_MAX_RETRIES"))
            .map(|n| n as u32)
            .unwrap_or(DEFAULT_MAX_RETRIES);
        Ok(Some(Self {
            base_url: require_field("review", "base_url", &r.base_url)?,
            api_key: require_field("review", "api_key", &r.api_key)?,
            model: require_field("review", "model", &r.model)?,
            system_prompt: require_system_prompt_with_env(
                r.system_prompt_file.as_deref(),
                env("REVIEW_SYSTEM_PROMPT"),
            )?,
            max_diff_chars: r.max_diff_chars.unwrap_or(DEFAULT_MAX_DIFF_CHARS),
            // File config wins when set, else env (an operator can still tune via env with a
            // ConfigMap mounted), else the generous default.
            max_turns: r
                .max_turns
                .or_else(|| parse_env_u64("LLM_MAX_TURNS").map(|n| n as usize))
                .unwrap_or(DEFAULT_MAX_TURNS)
                .max(1),
            max_batch_size: r
                .max_batch_size
                .or_else(|| parse_env_u64("LLM_MAX_BATCH_SIZE").map(|n| n as usize))
                .unwrap_or(DEFAULT_MAX_BATCH_SIZE)
                .max(1),
            max_files_read: r
                .max_files_read
                .or_else(|| parse_env_u64("LLM_MAX_FILES_READ").map(|n| n as usize))
                .unwrap_or(DEFAULT_MAX_FILES_READ)
                .max(1),
            max_searches: r
                .max_searches
                .or_else(|| parse_env_u64("LLM_MAX_SEARCHES").map(|n| n as usize))
                .unwrap_or(DEFAULT_MAX_SEARCHES)
                .max(1),
            max_batches: r
                .max_batches
                .or_else(|| parse_env_u64("LLM_MAX_BATCHES").map(|n| n as usize))
                .unwrap_or(DEFAULT_MAX_BATCHES)
                .max(1),
            // Deliberately unclamped: 0 = bounce disabled (ADR-0069).
            max_coverage_bounces: r
                .max_coverage_bounces
                .or_else(|| parse_env_u64("LLM_MAX_COVERAGE_BOUNCES").map(|n| n as usize))
                .unwrap_or(DEFAULT_MAX_COVERAGE_BOUNCES),
            context_window,
            temperature,
            top_p,
            max_tokens,
            extra,
            // File wins; else the `LLM_STREAM` env (legacy/local), else off.
            stream: r.stream.unwrap_or_else(stream_from_env),
            resilience: ResilienceConfig {
                request_timeout_secs,
                max_retries,
                circuit_breaker_threshold: r
                    .circuit_breaker_threshold
                    .or_else(|| parse_env_u64("LLM_CIRCUIT_BREAKER_THRESHOLD"))
                    .map(|n| n as u32)
                    .unwrap_or(DEFAULT_CIRCUIT_BREAKER_THRESHOLD),
            },
            fast: false, // set by resolve_tiers / main per the task tier (ADR-0062)
            tools,
        }))
    }

    /// Resolve BOTH review tiers (ADR-0062). Each tier uses its own complete block when present
    /// (`review.fast` / `review.deep`), else falls back to the flat `review.*` block — so a values file
    /// with no tier blocks (the legacy shape) keeps working, with both tiers on the flat config. The
    /// returned FAST config carries the structural `fast` flag (single diff-only turn, no retrieval); the
    /// DEEP config does not. Transition-safe: this runner accepts BOTH the flat and the nested shapes, so
    /// it can deploy before the values are restructured.
    pub fn resolve_tiers(file: Option<&FileConfig>) -> anyhow::Result<ReviewConfigs> {
        let Some(r) = file.and_then(|f| f.review.as_ref()) else {
            // Local/dev env path: one config from env, used for both tiers.
            let deep = Self::from_env()?;
            let mut fast = deep.clone();
            if let Some(c) = fast.as_mut() {
                c.fast = true;
            }
            return Ok(ReviewConfigs { fast, deep });
        };
        let deep = match r.deep.as_deref() {
            Some(d) => Self::from_review_file(d)?,
            None => Self::from_review_file(r)?,
        };
        let mut fast = match r.fast.as_deref() {
            Some(f) => Self::from_review_file(f)?,
            None => Self::from_review_file(r)?,
        };
        if let Some(c) = fast.as_mut() {
            c.fast = true;
        }
        Ok(ReviewConfigs { fast, deep })
    }
}

/// Resolved review configs for both tiers (ADR-0062). The runner picks one per task by its tier; each
/// is a complete, independent config (own model/gateway/prompt/budget). Either side is `None` when that
/// tier's model is empty (review disabled).
#[derive(Debug, Clone)]
pub struct ReviewConfigs {
    pub fast: Option<ReviewConfig>,
    pub deep: Option<ReviewConfig>,
}

impl ReviewConfigs {
    /// The config to run a task of this tier: `fast` → the fast config, anything else → deep. Returns
    /// `None` when that tier is disabled (no model). The selected fast config already carries the
    /// structural `fast` flag set by [`ReviewConfig::resolve_tiers`].
    pub fn for_tier(&self, tier: &str) -> Option<&ReviewConfig> {
        match tier {
            "fast" => self.fast.as_ref(),
            _ => self.deep.as_ref(),
        }
    }
}

/// Load the **required** reviewer system prompt (ADR-0037): the `system_prompt_file` template when a
/// path is given, else the `REVIEW_SYSTEM_PROMPT` env. Errors if neither yields a non-empty prompt —
/// there is deliberately no built-in default, so a misconfigured deploy fails the review closed
/// instead of silently running a weak fallback. The prompt is owned in ai-helm
/// (`config.reviewSystemPrompt`) and mounted into the Job.
fn require_system_prompt(system_prompt_file: Option<&str>) -> anyhow::Result<String> {
    require_system_prompt_with_env(
        system_prompt_file,
        std::env::var("REVIEW_SYSTEM_PROMPT").ok(),
    )
}

fn require_system_prompt_with_env(
    system_prompt_file: Option<&str>,
    env_prompt: Option<String>,
) -> anyhow::Result<String> {
    let from_file = match system_prompt_file {
        Some(path) if !path.trim().is_empty() => Some(
            lci_config::load_template(Path::new(path))
                .with_context(|| format!("loading review.system_prompt_file {path}"))?,
        ),
        _ => None,
    };
    let prompt = from_file
        .or(env_prompt)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    prompt.context(
        "no review system prompt configured: set review.system_prompt_file (mounted from the \
         ai-helm `config.reviewSystemPrompt` ConfigMap) or REVIEW_SYSTEM_PROMPT. There is no \
         built-in default (ADR-0037) — review fails closed without one.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_config_requires_a_system_prompt() {
        // ADR-0037: no built-in default. With no prompt source set, resolving review fails closed.
        let file = FileConfig {
            review: Some(ReviewFile {
                base_url: "https://gw/v1".to_string(),
                api_key: "key".to_string(),
                model: "model".to_string(),
                ..ReviewFile::default()
            }),
            ..FileConfig::default()
        };
        let err = ReviewConfig::resolve_with_env(Some(&file), &|_| None)
            .expect_err("must fail closed without a prompt");
        assert!(
            format!("{err:#}").contains("system prompt"),
            "error names the missing prompt: {err:#}"
        );
    }

    // A zero turn/budget (explicitly set in config or env) must clamp to ≥1, not silently no-op the
    // run (`for turn in 0..0`) or pre-disable a tool (`count >= 0`). #180 dogfood-review catch.
    #[test]
    fn turn_and_read_budgets_clamp_to_at_least_one() {
        // A temp prompt file so resolve() succeeds without touching env (parallel-safe).
        let prompt = std::env::temp_dir().join(format!("lci-clamp-{}.md", std::process::id()));
        std::fs::write(&prompt, "You are a reviewer.").unwrap();
        let file = FileConfig {
            embeddings: None,
            sast: None,
            review: Some(ReviewFile {
                base_url: "https://gw/v1".to_string(),
                api_key: "k".to_string(),
                model: "m".to_string(),
                system_prompt_file: Some(prompt.to_string_lossy().into_owned()),
                max_diff_chars: None,
                temperature: None,
                top_p: None,
                max_tokens: None,
                extra: None,
                request_timeout_secs: None,
                max_retries: None,
                circuit_breaker_threshold: None,
                // Every count knob explicitly 0 — `Some(0)` short-circuits the env fallback, so this is
                // deterministic regardless of the test environment.
                max_turns: Some(0),
                max_batch_size: Some(0),
                max_files_read: Some(0),
                max_searches: Some(0),
                max_batches: Some(0),
                // NOT clamped, unlike the budgets above: 0 = coverage bounce disabled (ADR-0069).
                max_coverage_bounces: Some(0),
                // A 0 context window is meaningless — it must resolve to "disabled" (None), not a
                // window of zero that would force wind-down on turn 0 (ADR-0045).
                context_window: Some(0),
                stream: None,
                fast: None,
                deep: None,
                tools: None,
            }),
        };
        let cfg = ReviewConfig::resolve(Some(&file))
            .expect("resolves with a prompt file")
            .expect("review enabled");
        assert_eq!(cfg.max_turns, 1, "max_turns clamped");
        assert_eq!(cfg.max_batch_size, 1, "max_batch_size clamped");
        assert_eq!(
            cfg.context_window, None,
            "a 0 context window resolves to disabled"
        );
        assert_eq!(cfg.max_files_read, 1, "max_files_read clamped");
        assert_eq!(cfg.max_searches, 1, "max_searches clamped");
        assert_eq!(cfg.max_batches, 1, "max_batches clamped");
        assert_eq!(
            cfg.max_coverage_bounces, 0,
            "max_coverage_bounces is NOT clamped — 0 disables the coverage bounce (ADR-0069)"
        );
        std::fs::remove_file(&prompt).ok();
    }

    /// A minimal review block with the given model (and a shared prompt file), every other knob unset.
    #[cfg(test)]
    fn review_block(model: &str, prompt_file: &str) -> ReviewFile {
        ReviewFile {
            base_url: "https://gw/v1".to_string(),
            api_key: "k".to_string(),
            model: model.to_string(),
            system_prompt_file: Some(prompt_file.to_string()),
            max_diff_chars: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            extra: None,
            request_timeout_secs: None,
            max_retries: None,
            circuit_breaker_threshold: None,
            max_turns: None,
            max_batch_size: None,
            max_files_read: None,
            max_searches: None,
            max_batches: None,
            max_coverage_bounces: None,
            context_window: None,
            stream: None,
            fast: None,
            deep: None,
            tools: None,
        }
    }

    // Two-tier review (ADR-0062): with `review.fast`/`review.deep` present, each tier resolves to its own
    // complete config (own model); the fast config carries the structural `fast` flag, deep does not.
    #[test]
    fn resolve_tiers_uses_independent_per_tier_blocks() {
        let prompt = std::env::temp_dir().join(format!("lci-tiers-{}.md", std::process::id()));
        std::fs::write(&prompt, "You are a reviewer.").unwrap();
        let p = prompt.to_string_lossy().into_owned();
        let mut flat = review_block("flat-model", &p);
        flat.fast = Some(Box::new(review_block("fast-model", &p)));
        flat.deep = Some(Box::new(review_block("deep-model", &p)));
        let file = FileConfig {
            embeddings: None,
            sast: None,
            review: Some(flat),
        };
        let tiers = ReviewConfig::resolve_tiers(Some(&file)).expect("resolves");
        let fast = tiers.for_tier("fast").expect("fast enabled");
        let deep = tiers.for_tier("deep").expect("deep enabled");
        assert_eq!(fast.model, "fast-model");
        assert!(fast.fast, "fast tier carries the structural fast flag");
        assert_eq!(deep.model, "deep-model");
        assert!(!deep.fast, "deep tier is the full run");
        std::fs::remove_file(&prompt).ok();
    }

    // Back-compat: a flat `review.*` block with NO tier sub-blocks resolves both tiers to the flat config
    // (the fast one still flagged). This is the transition shape — the runner deploys before the values
    // are restructured.
    #[test]
    fn resolve_tiers_falls_back_to_flat_block() {
        let prompt = std::env::temp_dir().join(format!("lci-flat-{}.md", std::process::id()));
        std::fs::write(&prompt, "You are a reviewer.").unwrap();
        let p = prompt.to_string_lossy().into_owned();
        let file = FileConfig {
            embeddings: None,
            sast: None,
            review: Some(review_block("only-model", &p)),
        };
        let tiers = ReviewConfig::resolve_tiers(Some(&file)).expect("resolves");
        let fast = tiers.for_tier("fast").expect("fast enabled");
        let deep = tiers.for_tier("deep").expect("deep enabled");
        assert_eq!(
            fast.model, "only-model",
            "fast falls back to the flat block"
        );
        assert_eq!(
            deep.model, "only-model",
            "deep falls back to the flat block"
        );
        assert!(fast.fast && !deep.fast, "flags still set per tier");
        std::fs::remove_file(&prompt).ok();
    }

    // Per-tier tool allowlist (ADR-0062): a valid list resolves through to the tier config; an EMPTY
    // list fails closed (a tier with no tools can't act). Unknown names are rejected earlier, at
    // deserialize, by the closed `ReviewTool` enum (see `unknown_tool_name_fails_at_deserialize` in
    // `super::file`).
    #[test]
    fn resolve_tools_allowlist_carries_through_and_rejects_empty() {
        let prompt = std::env::temp_dir().join(format!("lci-tools-{}.md", std::process::id()));
        std::fs::write(&prompt, "You are a reviewer.").unwrap();
        let p = prompt.to_string_lossy().into_owned();

        // A good allowlist (built-ins + an mcp selector) resolves and is carried onto the config.
        let mut good = review_block("m", &p);
        good.tools = Some(
            serde_json::from_value(serde_json::json!([
                "add_review_comment",
                "finish",
                "abort",
                "mcp__brave-search__brave_web_search",
            ]))
            .unwrap(),
        );
        let cfg = ReviewConfig::from_review_file(&good)
            .expect("resolves")
            .expect("enabled");
        let selectors = cfg.tools.as_deref().expect("carried through");
        assert_eq!(selectors.len(), 4);
        assert!(matches!(
            &selectors[0],
            ReviewToolSelector::Builtin(ReviewTool::AddReviewComment)
        ));
        // The mcp selector matches the exact discovered name and nothing else.
        let ReviewToolSelector::Mcp(pat) = &selectors[3] else {
            panic!("expected an mcp selector: {:?}", selectors[3]);
        };
        assert!(pat.is_match("mcp__brave-search__brave_web_search"));
        assert!(!pat.is_match("mcp__brave-search__brave_local_search"));

        // An empty list is rejected — a tier with no tools can't act.
        let mut empty = review_block("m", &p);
        empty.tools = Some(vec![]);
        ReviewConfig::from_review_file(&empty).expect_err("empty allowlist must fail");

        std::fs::remove_file(&prompt).ok();
    }

    // `review.stream` (file config) takes precedence; when unset it falls back to the `LLM_STREAM`
    // env. Here the file sets it explicitly so the result is deterministic regardless of the ambient
    // env (#206 streaming toggle, promoted from env-only to a config knob).
    #[test]
    fn review_stream_from_file_wins() {
        let prompt = std::env::temp_dir().join(format!("lci-stream-{}.md", std::process::id()));
        std::fs::write(&prompt, "You are a reviewer.").unwrap();
        let file = FileConfig {
            embeddings: None,
            sast: None,
            review: Some(ReviewFile {
                base_url: "https://gw/v1".into(),
                api_key: "k".into(),
                model: "m".into(),
                system_prompt_file: Some(prompt.to_string_lossy().into_owned()),
                stream: Some(true),
                ..Default::default()
            }),
        };
        let cfg = ReviewConfig::resolve(Some(&file))
            .expect("resolves with a prompt file")
            .expect("review enabled");
        assert!(cfg.stream, "review.stream=true is honoured over the env");
        std::fs::remove_file(&prompt).ok();
    }
}
