//! The agent runner's JSON file config (ADR-0021/0018/0062/0066): the `Deserialize` shape mounted
//! from the ConfigMap, plus loading it off disk. Resolving these into the runner's effective configs
//! (env fallback, defaults, tier fan-out) lives in the sibling `embeddings`/`review`/`sast` modules —
//! this module owns only the *shape* and its validation at parse time.

use std::path::Path;

use regex::Regex;
use serde::Deserialize;

/// Where the runner looks for its JSON config file (mounted from a ConfigMap). Overridable via
/// `AGENT_CONFIG`. When the file is absent the runner falls back to legacy env vars, so a Job keeps
/// working before the chart mounts the ConfigMap.
const DEFAULT_AGENT_CONFIG_PATH: &str = "/etc/lightbridge/agent.json";

/// The agent runner's file config (ADR-0021/0018). Every field is optional: a partial file overrides
/// only what it sets, and an absent file means "use env + defaults everywhere". String values support
/// `{env:VAR:-default}` (resolved by `lci-config`), so secrets stay in env while models,
/// URLs, and template paths live declaratively in the ConfigMap.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
    pub embeddings: Option<EmbeddingsFile>,
    pub review: Option<ReviewFile>,
    /// Deterministic SAST pass (ADR-0061). Absent or `enabled: false` ⇒ no SAST.
    pub sast: Option<SastFile>,
}

/// File config for the deterministic SAST pass (ADR-0061). Every field is optional; an absent block (or
/// `enabled: false`) disables SAST entirely. Bool/numeric-string tolerant so `{env:…}`-substituted
/// values still deserialize.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SastFile {
    #[serde(default, deserialize_with = "lci_config::de::opt_bool")]
    pub enabled: Option<bool>,
    /// opengrep binary name/path; defaults to `opengrep` on PATH.
    pub bin: Option<String>,
    /// `--config` value: a local rules dir (default: the vendored set) or a registry ruleset.
    pub rules: Option<String>,
    /// Minimum SARIF level to surface (`error`|`warning`|`note`).
    pub min_severity: Option<String>,
    #[serde(default, deserialize_with = "lci_config::de::opt_usize")]
    pub max_findings: Option<usize>,
    #[serde(default, deserialize_with = "lci_config::de::opt_u64")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmbeddingsFile {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// Per-model tuning block (ADR-0051). Embeddings have few knobs today; `config` keeps the shape
    /// uniform with the review models and is where future ones land. Unset = defaults.
    #[serde(default)]
    pub config: Option<EmbeddingsTuningFile>,
}

/// Per-model tuning for the embeddings model (ADR-0051). Numeric-string tolerant for `{env:}` values.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmbeddingsTuningFile {
    #[serde(default, deserialize_with = "lci_config::de::opt_u64")]
    pub request_timeout_secs: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReviewFile {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// Path to the reviewer's system-prompt template (a mounted file); its contents are env-subst'd.
    pub system_prompt_file: Option<String>,
    #[serde(default, deserialize_with = "lci_config::de::opt_usize")]
    pub max_diff_chars: Option<usize>,
    /// Generation params for the review model. All optional — unset means the model/provider default.
    /// Numeric-string tolerant so `{env:…}`-substituted values (always strings) still deserialize.
    #[serde(default, deserialize_with = "lci_config::de::opt_f64")]
    pub temperature: Option<f64>,
    #[serde(default, deserialize_with = "lci_config::de::opt_f64")]
    pub top_p: Option<f64>,
    #[serde(default, deserialize_with = "lci_config::de::opt_i64")]
    pub max_tokens: Option<i64>,
    /// Provider-specific passthrough generation params, merged verbatim into the chat request body —
    /// for knobs the typed fields don't cover, notably a **reasoning budget** (e.g. `thinking`,
    /// `reasoning_effort`) to stop a reasoning model over-thinking. A JSON object; `None` = nothing
    /// extra. The operator owns correctness; unknown fields the gateway/model ignores are harmless.
    #[serde(default)]
    pub extra: Option<serde_json::Value>,
    /// Resilience knobs (ADR-0039). All optional — unset falls back to the safe defaults above so a
    /// deploy works without an ai-helm values change. Numeric-string tolerant for `{env:…}` values.
    #[serde(default, deserialize_with = "lci_config::de::opt_u64")]
    pub request_timeout_secs: Option<u64>,
    #[serde(default, deserialize_with = "lci_config::de::opt_u64")]
    pub max_retries: Option<u64>,
    #[serde(default, deserialize_with = "lci_config::de::opt_u64")]
    pub circuit_breaker_threshold: Option<u64>,
    /// Ceiling on model turns before the run is cut off (operator-tunable). Unset = [`super::DEFAULT_MAX_TURNS`].
    /// Numeric-string tolerant for `{env:…}`-substituted values.
    #[serde(default, deserialize_with = "lci_config::de::opt_usize")]
    pub max_turns: Option<usize>,
    /// Max read-only tool calls run concurrently per turn (ADR-0042). Unset = [`super::DEFAULT_MAX_BATCH_SIZE`].
    #[serde(default, deserialize_with = "lci_config::de::opt_usize")]
    pub max_batch_size: Option<usize>,
    /// Cumulative read budgets (ADR-0042). Unset = the `DEFAULT_MAX_*` constants.
    #[serde(default, deserialize_with = "lci_config::de::opt_usize")]
    pub max_files_read: Option<usize>,
    #[serde(default, deserialize_with = "lci_config::de::opt_usize")]
    pub max_searches: Option<usize>,
    #[serde(default, deserialize_with = "lci_config::de::opt_usize")]
    pub max_batches: Option<usize>,
    /// Coverage-gate bounce cap (ADR-0069). Unset = [`super::DEFAULT_MAX_COVERAGE_BOUNCES`]; `0` disables the
    /// bounce (the coverage disclosure still applies), `1` = legacy bounce-once.
    #[serde(default, deserialize_with = "lci_config::de::opt_usize")]
    pub max_coverage_bounces: Option<usize>,
    /// OpenCode-path re-prompt ceiling (fast-tier-parity plan): how many whole `session/prompt` cycles
    /// the supervisor will re-drive before finalizing as exhausted. Unset = [`super::DEFAULT_MAX_CYCLES`].
    /// Kept as a real Rust-side enforcement mechanism because opencode's own `maxSteps` was found NOT to
    /// cap anything over ACP (see `services/agent-runner/src/review/opencode.rs`'s `e2e` module) — this
    /// is what actually stops a stuck/adversarial model, tier-configurable so fast can run a
    /// genuinely smaller budget than deep.
    #[serde(default, deserialize_with = "lci_config::de::opt_usize")]
    pub max_cycles: Option<usize>,
    /// Model context window in tokens (ADR-0045). When set, the agent budgets its conversation against
    /// it — winding down before overflow and trimming old tool output — instead of failing a 400 when
    /// the history grows too large. Unset = no budgeting (unchanged behaviour).
    #[serde(default, deserialize_with = "lci_config::de::opt_usize")]
    pub context_window: Option<usize>,
    /// Stream the chat response (SSE) and reassemble it client-side instead of awaiting the whole
    /// completion (ADR-0039 / #206). `Some(true)` enables it; unset falls back to the `LLM_STREAM` env
    /// (legacy/local toggle), else off. Streaming bounds a long-but-progressing turn by a per-chunk idle
    /// timeout rather than one whole-request timeout — useful for a heavy-reasoning model (e.g. GLM).
    /// Bool-tolerant like the numeric knobs above, so a `{env:…}`-substituted string (e.g.
    /// `"{env:LLM_STREAM:-true}"`) still deserializes instead of failing the config.
    #[serde(default, deserialize_with = "lci_config::de::opt_bool")]
    pub stream: Option<bool>,
    /// Two-tier review (ADR-0062): a fully-independent config for the FAST tier (automatic
    /// `pull_request opened`). When present it is a COMPLETE review block (its own model, gateway, prompt,
    /// reasoning budget, timeout, …) — NOT an overlay on the flat fields. When absent, the FAST tier
    /// falls back to the flat `review.*` block (back-compat: an older values file with no tier blocks).
    /// A nested block's own `fast`/`deep` are ignored.
    #[serde(default)]
    pub fast: Option<Box<ReviewFile>>,
    /// Two-tier review (ADR-0062): a fully-independent config for the DEEP tier (`@mention`). Same shape
    /// and fallback as `fast`. This is where the strong model + 2h timeout live; the FAST block carries
    /// the cheap model + short timeout.
    #[serde(default)]
    pub deep: Option<Box<ReviewFile>>,
    /// Per-tier tool allowlist (ADR-0062 + ADR-0066): the exact set of tools this tier offers, e.g.
    /// `["add_review_comment", "finish", "abort"]` for a diff-only FAST pass with no retrieval. Each
    /// entry is a [`ReviewToolSelector`] — either an exact built-in name (validated at deserialize
    /// against the closed [`ReviewTool`] enum) OR an `mcp__`-prefixed **regex** matched against the
    /// dynamically-discovered `mcp__<server>__<tool>` names, so an operator can pick a SUBSET of a
    /// server's tools (a busy server like brave-search exposes many) instead of all-or-nothing — e.g.
    /// `"mcp__brave-search__brave_web_search"` (exact) or `"mcp__context7__.*"` (all of context7's).
    /// An unknown built-in / malformed regex **fails at deserialize** rather than silently offering
    /// fewer tools. When unset, the tier uses the built-in default (the full surface — every built-in
    /// plus all discovered MCP tools — for DEEP; the wind-down write/finish/abort set for FAST).
    #[serde(default)]
    pub tools: Option<Vec<ReviewToolSelector>>,
    /// Operator-supplied OpenCode config overlay for the review run (ADR-0099). A raw OpenCode config
    /// object, deep-merged LAST over our base+injection with **full override** (objects merge
    /// recursively; arrays/scalars from the overlay replace ours), so a SysAdmin can add custom
    /// sub-agents, extra models/providers, extra MCP servers, or change `permission`/`tools` with no
    /// code change. TRUSTED (owner-managed via ai-helm-values), unlike the untrusted checkout which is
    /// still never a config source (ADR-0097 #6). Relaxing a review invariant is permitted but WARNED
    /// (and noted on the coverage disclosure), never blocked. When a per-tier block sets its own
    /// `opencode`, that wins for the tier; else the flat `review.opencode` applies to both tiers.
    #[serde(default)]
    pub opencode: Option<serde_json::Value>,
}

/// A tool the review agent can be configured to offer (ADR-0062). A **closed enum** so a per-tier
/// `review.<tier>.tools` allowlist is validated when the config is parsed — an unknown name fails the
/// config with serde listing the valid variants — instead of a free-form string the runner has to
/// re-validate by hand. Each serde name is the EXACT tool name the agent dispatches (see
/// [`lci_review_agent::tools`]); a sync test asserts the enum can't drift from that surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum ReviewTool {
    #[serde(rename = "lightbridge_vector_semantic_search")]
    VectorSemanticSearch,
    #[serde(rename = "lightbridge_graph_find_symbol")]
    GraphFindSymbol,
    #[serde(rename = "lightbridge_graph_get_callers")]
    GraphGetCallers,
    #[serde(rename = "read_file")]
    ReadFile,
    #[serde(rename = "add_review_comment")]
    AddReviewComment,
    #[serde(rename = "retract_finding")]
    RetractFinding,
    #[serde(rename = "add_comment")]
    AddComment,
    #[serde(rename = "finish")]
    Finish,
    #[serde(rename = "report_progress")]
    ReportProgress,
    #[serde(rename = "abort")]
    Abort,
    /// The deterministic opengrep pass, now a tool the agent calls on demand instead of an automatic
    /// pre-agent scan (ADR-0073) — an operator must list it explicitly for either tier to offer it.
    #[serde(rename = "run_sast")]
    RunSast,
}

impl ReviewTool {
    /// Every variant, in the canonical tool order — the operator-facing list of valid built-in names.
    pub const ALL: [ReviewTool; 11] = [
        ReviewTool::VectorSemanticSearch,
        ReviewTool::GraphFindSymbol,
        ReviewTool::GraphGetCallers,
        ReviewTool::ReadFile,
        ReviewTool::AddReviewComment,
        ReviewTool::RetractFinding,
        ReviewTool::AddComment,
        ReviewTool::Finish,
        ReviewTool::ReportProgress,
        ReviewTool::Abort,
        ReviewTool::RunSast,
    ];

    /// The canonical tool name the agent dispatches — the exact string in [`lci_review_agent::tools`].
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewTool::VectorSemanticSearch => "lightbridge_vector_semantic_search",
            ReviewTool::GraphFindSymbol => "lightbridge_graph_find_symbol",
            ReviewTool::GraphGetCallers => "lightbridge_graph_get_callers",
            ReviewTool::ReadFile => "read_file",
            ReviewTool::AddReviewComment => "add_review_comment",
            ReviewTool::RetractFinding => "retract_finding",
            ReviewTool::AddComment => "add_comment",
            ReviewTool::Finish => "finish",
            ReviewTool::ReportProgress => "report_progress",
            ReviewTool::Abort => "abort",
            ReviewTool::RunSast => "run_sast",
        }
    }

    /// The built-in matching `name`, if any (the inverse of [`as_str`](Self::as_str)).
    fn from_name(name: &str) -> Option<ReviewTool> {
        ReviewTool::ALL.into_iter().find(|t| t.as_str() == name)
    }
}

/// One entry in a per-tier `review.<tier>.tools` allowlist (ADR-0062 + ADR-0066): either an exact
/// built-in tool, or a regex selector for the dynamically-discovered external-knowledge MCP tools.
/// Deserialized from a single string — a built-in name binds [`Self::Builtin`]; a string starting
/// with the `mcp__` prefix is compiled as an anchored regex into [`Self::Mcp`]; anything else fails
/// the config (a typo'd built-in can't silently become a never-matching pattern).
#[derive(Debug, Clone)]
pub enum ReviewToolSelector {
    Builtin(ReviewTool),
    Mcp(McpToolPattern),
}

/// A compiled, fully-anchored regex over discovered `mcp__<server>__<tool>` names (ADR-0066). Carries
/// the raw pattern for logging/diagnostics alongside the compiled matcher.
#[derive(Debug, Clone)]
pub struct McpToolPattern {
    raw: String,
    regex: Regex,
}

impl McpToolPattern {
    /// Whether this selector matches a discovered tool's (prefixed) name.
    pub fn is_match(&self, discovered_tool_name: &str) -> bool {
        self.regex.is_match(discovered_tool_name)
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

impl<'de> Deserialize<'de> for ReviewToolSelector {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let s = String::deserialize(deserializer)?;
        if let Some(builtin) = ReviewTool::from_name(&s) {
            return Ok(ReviewToolSelector::Builtin(builtin));
        }
        // Not a built-in. It MUST be an `mcp__` selector, else it's a typo we fail loud on rather
        // than treating a mistyped built-in as a regex that silently matches nothing.
        if let Some(pattern) = s.strip_prefix(lci_review_agent::tools::MCP_TOOL_PREFIX) {
            // Anchor to a FULL match and keep the `mcp__` prefix in the compiled regex, so
            // `mcp__brave-search__brave_web_search` matches exactly that discovered name and
            // `mcp__context7__.*` matches all of context7's — never a partial/substring hit. The
            // user's remainder is wrapped in a NON-CAPTURING GROUP so a top-level alternation binds
            // INSIDE the anchors: without it, `mcp__brave__foo|bar` would parse as
            // `(^mcp__brave__foo)|(bar$)` and match any tool ending in `bar` on ANY server.
            let anchored = format!(
                "^{}(?:{})$",
                regex::escape(lci_review_agent::tools::MCP_TOOL_PREFIX),
                pattern
            );
            let regex = Regex::new(&anchored)
                .map_err(|e| D::Error::custom(format!("invalid mcp tool regex {s:?}: {e}")))?;
            return Ok(ReviewToolSelector::Mcp(McpToolPattern { raw: s, regex }));
        }
        Err(D::Error::custom(format!(
            "unknown review tool {s:?}: expected a built-in ({}) or an \"mcp__<server>__<tool>\" \
             regex selector",
            ReviewTool::ALL
                .iter()
                .map(|t| t.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }
}

/// Load the agent config file if it exists. `Ok(None)` when the path is absent (use env); `Err` when
/// it exists but is malformed — a misconfiguration we want surfaced, not silently ignored.
pub fn load_file_config() -> anyhow::Result<Option<FileConfig>> {
    let path =
        std::env::var("AGENT_CONFIG").unwrap_or_else(|_| DEFAULT_AGENT_CONFIG_PATH.to_string());
    let path = Path::new(&path);
    if !path.exists() {
        return Ok(None);
    }
    lci_config::load::<FileConfig>(path).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ADR-0066: the mcp selector is a FULL-match regex, so `mcp__context7__.*` picks every context7
    // tool but no other server's, and a bare exact name matches only itself.
    #[test]
    fn mcp_tool_selector_regex_matches_are_anchored_per_server() {
        let sel: Vec<ReviewToolSelector> = serde_json::from_value(serde_json::json!([
            "mcp__context7__.*",
            "mcp__brave-search__brave_web_search",
        ]))
        .unwrap();
        let ReviewToolSelector::Mcp(all_context7) = &sel[0] else {
            panic!("expected mcp selector");
        };
        assert!(all_context7.is_match("mcp__context7__resolve-library-id"));
        assert!(all_context7.is_match("mcp__context7__query-docs"));
        assert!(!all_context7.is_match("mcp__brave-search__brave_web_search"));
        // A partial/substring must NOT match (anchoring): the pattern is the whole name.
        assert!(!all_context7.is_match("x-mcp__context7__query-docs"));

        let ReviewToolSelector::Mcp(exact) = &sel[1] else {
            panic!("expected mcp selector");
        };
        assert!(exact.is_match("mcp__brave-search__brave_web_search"));
        assert!(!exact.is_match("mcp__brave-search__brave_web_search_extra"));
    }

    // A top-level alternation must bind INSIDE the anchors (the non-capturing-group wrap): without
    // it, `mcp__brave-search__foo|bar` would parse as `(^mcp__brave-search__foo)|(bar$)` and match
    // ANY server's tool ending in `bar`, plus the bare string `bar`. With the wrap the alternation
    // stays contained — it can never leak past the `^mcp__…$` anchors to another server.
    #[test]
    fn mcp_tool_selector_alternation_stays_within_anchors() {
        // Raw (unparenthesized) alternation: `^mcp__(?:brave-search__foo|bar)$` — so the two
        // alternatives are `mcp__brave-search__foo` and `mcp__bar`, NEVER `…evil…bar` or bare `bar`.
        let raw: Vec<ReviewToolSelector> =
            serde_json::from_value(serde_json::json!(["mcp__brave-search__foo|bar"])).unwrap();
        let ReviewToolSelector::Mcp(p) = &raw[0] else {
            panic!("expected mcp selector");
        };
        assert!(p.is_match("mcp__brave-search__foo"));
        assert!(!p.is_match("mcp__evil-server__grabbar")); // the bug this wrap fixes
        assert!(!p.is_match("bar")); // ditto

        // The form an operator actually writes to pick two tools on ONE server — parenthesized —
        // matches both and still can't escape the server prefix.
        let grouped: Vec<ReviewToolSelector> =
            serde_json::from_value(serde_json::json!(["mcp__brave-search__(foo|bar)"])).unwrap();
        let ReviewToolSelector::Mcp(g) = &grouped[0] else {
            panic!("expected mcp selector");
        };
        assert!(g.is_match("mcp__brave-search__foo"));
        assert!(g.is_match("mcp__brave-search__bar"));
        assert!(!g.is_match("mcp__evil-server__grabbar"));
    }

    // A malformed regex in an `mcp__` selector fails the config at parse time (fail-closed).
    #[test]
    fn invalid_mcp_regex_fails_at_deserialize() {
        let err = serde_json::from_str::<FileConfig>(
            r#"{"review":{"base_url":"u","api_key":"k","model":"m","tools":["mcp__brave__(oops"]}}"#,
        )
        .expect_err("an invalid regex must fail parsing");
        assert!(
            err.to_string().contains("invalid mcp tool regex"),
            "error names the problem: {err}"
        );
    }

    // An unknown tool name that is neither a built-in nor an `mcp__` selector fails at parse time —
    // the custom `ReviewToolSelector` deserializer names the offending value, so a typo in
    // `review.<tier>.tools` fails the config loudly instead of silently offering fewer tools (and a
    // mistyped built-in is NOT quietly reinterpreted as a never-matching regex).
    #[test]
    fn unknown_tool_name_fails_at_deserialize() {
        let json = r#"{"review":{"base_url":"u","api_key":"k","model":"m",
                        "tools":["add_review_comment","nope_tool"]}}"#;
        let err =
            serde_json::from_str::<FileConfig>(json).expect_err("unknown tool must fail parsing");
        assert!(
            err.to_string().contains("nope_tool"),
            "the error names the bad tool: {err}"
        );
    }

    // Drift guard: the operator-facing `ReviewTool` enum must stay in lockstep with the tool surface the
    // agent actually dispatches (`tools::known_tool_names`). Add/remove a tool without updating the enum
    // and an allowlist would filter against a stale set — this fails the build instead.
    #[test]
    fn review_tool_enum_matches_the_dispatch_surface() {
        use std::collections::BTreeSet;
        let enum_names: BTreeSet<&str> = ReviewTool::ALL.iter().map(|t| t.as_str()).collect();
        let known: BTreeSet<&str> = lci_review_agent::tools::known_tool_names()
            .into_iter()
            .collect();
        assert_eq!(
            enum_names, known,
            "ReviewTool variants must match tools::known_tool_names() exactly"
        );
    }
}
