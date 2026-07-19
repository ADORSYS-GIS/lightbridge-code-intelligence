//! Render the per-task OpenCode config for a review run (ADR-0099).
//!
//! The config is built in three layers, last-writer-wins, merged host-side:
//!
//! 1. **base** — the checked-in, human-readable [`integrations/opencode/config/review.jsonc`], baked in
//!    via `include_str!`. It holds the invariants + defaults: the disabled built-ins (top-level
//!    `tools`), the read-only `permission`, the mediated `lightbridge` MCP, the recorder/gate/logger
//!    plugins, and the `eaig/reviewer` model wiring via `{env:*}`. Secrets ride `{env:*}` placeholders
//!    (opencode resolves them at load); the reviewer prompt rides a `{file:*}` placeholder pointing at
//!    a per-task file the host writes beside the config.
//! 2. **runtime injection** — patched onto the parsed base per task: the attribution/billing headers
//!    (#89, dynamic keys), the tier's `reasoning` flag (deep=true/fast=false, ADR-0069), the tier's
//!    `review.extra` passthrough params (`reasoning_effort`, `top_p`, …) merged into the reviewer
//!    model's `options`, and `temperature` when set. These are the values that can't be a static
//!    `{env:*}`/`{file:*}`.
//! 3. **operator overlay** — the trusted `review.opencode` object from ai-helm-values, deep-merged LAST
//!    with **full override** (objects merge recursively; arrays and scalars from the overlay replace
//!    ours). The merge is host-side so the untrusted checkout is never a config source (ADR-0097 #6).
//!
//! Two invariants the base enforces for **coverage parity** with the native loop:
//! - **All file access is mediated.** OpenCode's built-in `read`/`grep`/`glob`/`list`/`edit`/`bash`
//!   (and `task`, i.e. subagents) are disabled, so every read goes through `lightbridge_read_file` and
//!   the retrieval tools — the single mediated path that makes the recorder-driven coverage accounting
//!   exact (a built-in `read` would be invisible to it).
//! - **Read-only.** `edit`/`bash`/`webfetch` are denied; a review never mutates the tree or egresses.
//!
//! Because the overlay wins on every key (full override, the owner's chosen policy), it can WEAKEN
//! those invariants. That is permitted by design; the system makes it *visible*, not impossible:
//! [`render_review_config`] diffs the merged config against the base floor and returns a
//! [`FloorBreach`] for each relaxation (a built-in re-enabled — globally OR for one agent, a
//! permission opened — globally OR for one agent, the `lightbridge` MCP or a plugin dropped/replaced)
//! so the host can WARN and note it on the review's coverage
//! disclosure.

use serde_json::{Map, Value};

/// The checked-in base config (ADR-0099), baked in at compile time. It is `.jsonc` (real `//`
/// comments, documented there) — [`strip_jsonc_comments`] removes them before parsing. The path
/// reaches the repo-root `integrations/` tree, which is present at compile time (the release build
/// compiles the whole checked-out workspace); the runtime image needs no copy of it.
const BASE_REVIEW_JSONC: &str =
    include_str!("../../../../integrations/opencode/config/review.jsonc");

/// The relative `{file:*}` reference the base uses for the reviewer prompt (opencode resolves it
/// against the config file's directory at load). The host writes the per-task prompt to a file of this
/// name **beside** the written config so the reference resolves — see the agent-runner review host.
pub const REVIEW_PROMPT_FILE: &str = "review-prompt.md";

/// One way the operator overlay relaxed a base review invariant (ADR-0099 §4). Full override is
/// intentional — this is surfaced (logged + noted on the coverage disclosure), never prevented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloorBreach {
    /// A human-readable one-liner naming what was relaxed — used both for the `tracing::warn!` and,
    /// joined, for the coverage-disclosure note.
    pub message: String,
}

/// The rendered per-task config plus any floor relaxations the overlay introduced.
#[derive(Debug, Clone)]
pub struct RenderedReviewConfig {
    /// The final, merged config to write as `OPENCODE_CONFIG`.
    pub config: Value,
    /// Empty unless the operator overlay relaxed the base floor (built-in re-enabled, permission
    /// opened, or the `lightbridge` MCP / a required plugin dropped or replaced). Never blocks the run.
    pub floor_breaches: Vec<FloorBreach>,
}

impl RenderedReviewConfig {
    /// A single coverage-disclosure sentence when the floor was breached, else `None`. Appended to the
    /// review's coverage disclosure so a finding set produced under a custom operator config isn't
    /// mistaken for the default (ADR-0099 §4).
    #[must_use]
    pub fn disclosure_note(&self) -> Option<String> {
        if self.floor_breaches.is_empty() {
            return None;
        }
        let relaxations = self
            .floor_breaches
            .iter()
            .map(|b| b.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        Some(format!(
            "A custom operator OpenCode config (review.opencode) was active and relaxed the review \
             floor: {relaxations}. Coverage/read-only guarantees may differ from the default."
        ))
    }
}

/// Render the OpenCode config for one review run (ADR-0099): parse the checked-in base, apply the
/// runtime injection, deep-merge the trusted operator `overlay` (full override), and diff the result
/// against the base floor.
///
/// - `fast` selects the tier: deep enables the reasoning model (ADR-0069), fast does not.
/// - `temperature` is patched into the provider model options when set (best-effort; fine sampling
///   params don't all map 1:1 once OpenCode owns the loop).
/// - `extra` is the tier's provider-passthrough params (`review.extra` — `reasoning_effort`, `top_p`,
///   `max_tokens`, …), merged into the reviewer model's `options` so the openai-compatible provider
///   forwards them on the request body. This is the SAME map, keyed the same way, the native request
///   body carries — native-path parity (ADR-0069 `reasoning_effort:"high"` reaches eaig on BOTH engines
///   now; the OpenCode path previously dropped it, silently running deep reviews at the gateway's
///   default low reasoning effort).
/// - `attribution` is the per-project billing header set (#89), forwarded on every provider request
///   via the openai-compatible provider's `headers` option so OpenCode-hosted review bills as native.
/// - `overlay` is the operator's `review.opencode` object (trusted); `None` (or an empty object) is a
///   no-op, leaving the config byte-identical to the base+injection.
///
/// The reviewer prompt is NOT a parameter: the base references it via `{file:REVIEW_PROMPT_FILE}` and
/// the host writes that file per task. Panics only on a corrupt *checked-in* base (a compile-time
/// asset, guarded by [`tests`]); operator input can't reach the panic.
#[must_use]
pub fn render_review_config(
    fast: bool,
    temperature: Option<f64>,
    extra: &Map<String, Value>,
    attribution: &[(String, String)],
    overlay: Option<&Value>,
) -> RenderedReviewConfig {
    let mut config = parse_base();
    inject_runtime(&mut config, fast, temperature, extra, attribution);

    // Snapshot the floor from the injected base BEFORE the overlay — the floor items (tools /
    // permission / mcp.lightbridge / plugin) are all base-owned, untouched by injection.
    let floor = Floor::capture(&config);

    // Apply the overlay ONLY when it is a non-empty object. An absent/empty overlay is a no-op (leaving
    // the config byte-identical to base+injection); a NON-object overlay (a misconfigured
    // `review.opencode: "…"`/`[…]`) is ignored rather than allowed to clobber our whole config into a
    // scalar — the deep merge's replace arm is for nested values, not the top-level config root.
    if let Some(overlay) = overlay.filter(|v| v.as_object().is_some_and(|o| !o.is_empty())) {
        deep_merge(&mut config, overlay);
    }

    let floor_breaches = floor.diff(&config);
    RenderedReviewConfig {
        config,
        floor_breaches,
    }
}

/// Parse the checked-in base jsonc into a JSON value. The base is a compile-time constant we author, so
/// a parse failure is a build-broke-the-asset bug, not a runtime condition — hence the expect (a
/// dedicated test parses it so the failure surfaces at `cargo test`, not in prod).
fn parse_base() -> Value {
    let stripped = strip_jsonc_comments(BASE_REVIEW_JSONC);
    serde_json::from_str(&stripped)
        .expect("the checked-in review.jsonc base is valid JSON once comments are stripped")
}

/// Patch the per-task runtime values onto the parsed base (ADR-0099 layer 2): the attribution headers
/// (dynamic keys), the tier `reasoning` flag, the `review.extra` passthrough params, and `temperature`
/// when set. Everything else the base already carries as `{env:*}`/`{file:*}` placeholders opencode
/// resolves at load.
fn inject_runtime(
    config: &mut Value,
    fast: bool,
    temperature: Option<f64>,
    extra: &Map<String, Value>,
    attribution: &[(String, String)],
) {
    let reviewer = &mut config["provider"]["eaig"]["models"]["reviewer"];
    // Deep tier runs a reasoning-capable model (ADR-0069 floor); fast does not. This is a capability
    // BOOLEAN only — the reasoning EFFORT level rides `review.extra` (`reasoning_effort`) below.
    reviewer["reasoning"] = Value::Bool(!fast);

    // Provider-passthrough generation params (`review.extra`) → the reviewer model's `options`, where
    // the `@ai-sdk/openai-compatible` eaig provider forwards them on the request body. This is where the
    // tier's configured `reasoning_effort` ("high" for deep, ADR-0069) actually reaches eaig; without
    // it, qwen3p7-plus defaults to its lowest reasoning variant. The WHOLE map is threaded (parity with
    // the native path, which merges the same `extra` into the chat body) so `top_p`/`max_tokens`/etc.
    // an operator sets there flow too. Merged BEFORE `temperature` so an explicit tier `temperature`
    // wins over any `temperature` an operator put in `extra`.
    if !extra.is_empty() {
        let options = reviewer["options"]
            .as_object_mut()
            .expect("the checked-in base ships reviewer.options as an object");
        for (key, value) in extra {
            options.insert(key.clone(), value.clone());
        }
    }

    if let Some(temperature) = temperature {
        reviewer["options"]["temperature"] = json_number(temperature);
    }

    // Per-project billing attribution (#89) — dynamic header keys, so patched in rather than `{env:*}`.
    let headers: Map<String, Value> = attribution
        .iter()
        .map(|(key, value)| (key.clone(), Value::String(value.clone())))
        .collect();
    config["provider"]["eaig"]["options"]["headers"] = Value::Object(headers);
}

/// A finite `f64` as a JSON number, falling back to null on the (unreachable for a real temperature)
/// non-finite case — `serde_json::Number::from_f64` returns `None` for NaN/inf.
fn json_number(value: f64) -> Value {
    serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
}

/// Deep-merge `overlay` INTO `base` with the ADR-0099 policy: two objects merge recursively; for any
/// other pair (scalar-vs-anything, array-vs-anything, type mismatch) the overlay value REPLACES the
/// base value. This is the operator's full-override semantics.
fn deep_merge(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            for (key, overlay_value) in overlay_map {
                match base_map.get_mut(key) {
                    Some(base_value) => deep_merge(base_value, overlay_value),
                    None => {
                        base_map.insert(key.clone(), overlay_value.clone());
                    }
                }
            }
        }
        (base, overlay) => *base = overlay.clone(),
    }
}

/// The base floor captured before the overlay merge: exactly the invariants the ADR-0099 diff warns
/// about when relaxed. Derived from the (injected) base so it can't drift from what the base declares.
struct Floor {
    /// Built-in tool names the base disables (`tools.<name> == false`).
    disabled_builtins: Vec<String>,
    /// Permission keys the base denies (`permission.<key> == "deny"`).
    denied_permissions: Vec<String>,
    /// The base's `mcp.lightbridge` object, if present (dropped/replaced ⇒ breach).
    lightbridge_mcp: Option<Value>,
    /// The base's `plugin` entries (any missing from the final ⇒ breach).
    plugins: Vec<Value>,
}

impl Floor {
    /// Extract the floor from a config value (the injected base).
    fn capture(config: &Value) -> Self {
        let disabled_builtins = config
            .get("tools")
            .and_then(Value::as_object)
            .map(|tools| {
                tools
                    .iter()
                    .filter(|(_, v)| *v == &Value::Bool(false))
                    .map(|(k, _)| k.clone())
                    .collect()
            })
            .unwrap_or_default();
        let denied_permissions = config
            .get("permission")
            .and_then(Value::as_object)
            .map(|perm| {
                perm.iter()
                    .filter(|(_, v)| v.as_str() == Some("deny"))
                    .map(|(k, _)| k.clone())
                    .collect()
            })
            .unwrap_or_default();
        let lightbridge_mcp = config
            .get("mcp")
            .and_then(|mcp| mcp.get("lightbridge"))
            .cloned();
        let plugins = config
            .get("plugin")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Self {
            disabled_builtins,
            denied_permissions,
            lightbridge_mcp,
            plugins,
        }
    }

    /// Diff the final (post-overlay) config against this floor, returning a breach per relaxation.
    fn diff(&self, final_config: &Value) -> Vec<FloorBreach> {
        let mut breaches = Vec::new();

        // A disabled built-in that is no longer `false` (re-enabled, or the whole `tools` object was
        // replaced by a non-object / dropped) escapes the mediated path and blinds coverage.
        let final_tools = final_config.get("tools");
        for name in &self.disabled_builtins {
            let still_disabled = final_tools
                .and_then(|t| t.get(name))
                .is_some_and(|v| v == &Value::Bool(false));
            if !still_disabled {
                breaches.push(FloorBreach {
                    message: format!("built-in tool `{name}` re-enabled (coverage may go blind)"),
                });
            }
        }

        // A permission no longer `"deny"` lets the review mutate the tree or egress.
        let final_perm = final_config.get("permission");
        for key in &self.denied_permissions {
            let still_denied =
                final_perm.and_then(|p| p.get(key)).and_then(Value::as_str) == Some("deny");
            if !still_denied {
                breaches.push(FloorBreach {
                    message: format!("permission `{key}` opened (no longer \"deny\")"),
                });
            }
        }

        // The mediated `lightbridge` MCP dropped or replaced breaks the finalize/coverage path.
        if let Some(base_mcp) = &self.lightbridge_mcp {
            let final_mcp = final_config
                .get("mcp")
                .and_then(|mcp| mcp.get("lightbridge"));
            match final_mcp {
                None => breaches.push(FloorBreach {
                    message: "the `lightbridge` MCP was dropped (finish/coverage path broken)"
                        .to_string(),
                }),
                Some(mcp) if mcp != base_mcp => breaches.push(FloorBreach {
                    message: "the `lightbridge` MCP was replaced (finish/coverage path may break)"
                        .to_string(),
                }),
                Some(_) => {}
            }
        }

        // A per-agent `tools`/`permission` override can ALSO re-enable a disabled built-in or open a
        // denied permission, scoped to just that one agent — so it never shows up in the top-level
        // checks above (the top-level `tools.bash`/`permission.bash` can stay closed while an operator
        // opens them just for a sub-agent, e.g. ADR-0099's `explore`/`file-scout` delegation). That is
        // exactly as real a floor relaxation as a global re-enable, just narrower in blast radius.
        if let Some(agents) = final_config.get("agent").and_then(Value::as_object) {
            for (agent_name, agent_config) in agents {
                let agent_tools = agent_config.get("tools").and_then(Value::as_object);
                for name in &self.disabled_builtins {
                    if agent_tools
                        .and_then(|t| t.get(name))
                        .and_then(Value::as_bool)
                        == Some(true)
                    {
                        breaches.push(FloorBreach {
                            message: format!(
                                "agent `{agent_name}` re-enabled built-in tool `{name}` (coverage \
                                 may go blind for that agent)"
                            ),
                        });
                    }
                }
                let agent_perm = agent_config.get("permission").and_then(Value::as_object);
                for key in &self.denied_permissions {
                    if agent_perm
                        .and_then(|p| p.get(key))
                        .and_then(Value::as_str)
                        .is_some_and(|v| v != "deny")
                    {
                        breaches.push(FloorBreach {
                            message: format!(
                                "agent `{agent_name}` opened permission `{key}` (no longer \"deny\")"
                            ),
                        });
                    }
                }
            }
        }

        // A required plugin missing from the final `plugin` list (arrays are replaced wholesale under
        // full override, so an overlay that sets `plugin` can drop the recorder/gate/logger).
        let final_plugins = final_config
            .get("plugin")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for plugin in &self.plugins {
            if !final_plugins.contains(plugin) {
                let name = plugin.as_str().unwrap_or("<plugin>");
                breaches.push(FloorBreach {
                    message: format!(
                        "required plugin `{name}` dropped or replaced (recorder/gate/logger)"
                    ),
                });
            }
        }

        breaches
    }
}

/// Strip `//` line comments and `/* … */` block comments from a jsonc string, leaving valid JSON. The
/// input is our own checked-in base (not arbitrary input), but this is string-literal-aware so a `//`
/// INSIDE a string (e.g. the `$schema` URL `https://…`) is preserved. Escapes are respected so a `\"`
/// inside a string doesn't end it. No trailing-comma handling: the base is authored comma-clean.
fn strip_jsonc_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                // Line comment: consume to (but keep) the newline so line numbers/layout survive.
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                // Block comment: consume through the closing `*/`.
                chars.next(); // the '*'
                let mut prev = '\0';
                for next in chars.by_ref() {
                    if prev == '*' && next == '/' {
                        break;
                    }
                    prev = next;
                }
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn render(fast: bool, temperature: Option<f64>, attribution: &[(String, String)]) -> Value {
        render_review_config(fast, temperature, &Map::new(), attribution, None).config
    }

    /// Render with an explicit `review.extra` passthrough map (the reasoning-effort fix path).
    fn render_with_extra(fast: bool, extra: &Map<String, Value>) -> Value {
        render_review_config(fast, None, extra, &[], None).config
    }

    #[test]
    fn base_jsonc_parses_and_carries_the_invariants() {
        let config = render(false, None, &[]);
        // Model wiring + secret placeholders (never inlined).
        assert_eq!(config["model"], "eaig/reviewer");
        assert_eq!(
            config["provider"]["eaig"]["options"]["baseURL"],
            "{env:LCI_EAIG_BASE_URL}"
        );
        assert_eq!(
            config["provider"]["eaig"]["options"]["apiKey"],
            "{env:LCI_EAIG_API_KEY}"
        );
        // The reviewer prompt is a {file:*} reference the host fills per task (ADR-0099).
        assert_eq!(
            config["agent"]["review"]["prompt"],
            format!("{{file:./{REVIEW_PROMPT_FILE}}}")
        );
        // The mediated stdio review MCP.
        assert_eq!(config["mcp"]["lightbridge"]["type"], "local");
        assert_eq!(config["mcp"]["lightbridge"]["command"][0], "lci-review-mcp");
        // The three first-party plugins by absolute path.
        assert_eq!(config["plugin"].as_array().map(Vec::len), Some(3));
    }

    #[test]
    fn disables_builtin_file_and_exec_tools_at_top_level_for_mediated_coverage() {
        let config = render(false, None, &[]);
        // TOP-LEVEL, not per-agent (opencode-over-ACP runs its default `build` agent, so a per-agent
        // block is ignored). Every built-in that could read the tree off the mediated path is off, so
        // all reads flow through lightbridge_read_file (exact coverage accounting).
        let tools = &config["tools"];
        for builtin in [
            "read",
            "grep",
            "glob",
            "list",
            "edit",
            "write",
            "patch",
            "bash",
            "webfetch",
            "websearch",
            "task",
            "skill",
            "todowrite",
        ] {
            assert_eq!(
                tools[builtin], false,
                "{builtin} must be disabled top-level"
            );
        }
        // Read-only posture.
        assert_eq!(config["permission"]["edit"], "deny");
        assert_eq!(config["permission"]["bash"], "deny");
        assert_eq!(config["permission"]["webfetch"], "deny");
    }

    #[test]
    fn deep_tier_enables_reasoning_fast_does_not() {
        let deep = render(false, None, &[]);
        let fast = render(true, None, &[]);
        assert_eq!(
            deep["provider"]["eaig"]["models"]["reviewer"]["reasoning"],
            true
        );
        assert_eq!(
            fast["provider"]["eaig"]["models"]["reviewer"]["reasoning"],
            false
        );
    }

    #[test]
    fn extra_reasoning_effort_reaches_reviewer_options_on_both_tiers() {
        // The bug this fixes: the OpenCode path dropped `review.extra`, so the deep tier's configured
        // `reasoning_effort:"high"` (ADR-0069) never reached eaig — deep reviews silently ran at the
        // gateway's default low reasoning variant. It must now land in the reviewer model's `options`,
        // where the `@ai-sdk/openai-compatible` provider forwards it on the request body — the SAME key
        // the native request body carries.
        let deep_extra = Map::from_iter([("reasoning_effort".to_string(), json!("high"))]);
        let deep = render_with_extra(false, &deep_extra);
        assert_eq!(
            deep["provider"]["eaig"]["models"]["reviewer"]["options"]["reasoning_effort"],
            "high"
        );
        // The capability boolean is UNCHANGED — the effort level is additive, not a replacement.
        assert_eq!(
            deep["provider"]["eaig"]["models"]["reviewer"]["reasoning"],
            true
        );

        // Fast tier carries its own configured effort ("low") the same way; reasoning bool stays false.
        let fast_extra = Map::from_iter([("reasoning_effort".to_string(), json!("low"))]);
        let fast = render_with_extra(true, &fast_extra);
        assert_eq!(
            fast["provider"]["eaig"]["models"]["reviewer"]["options"]["reasoning_effort"],
            "low"
        );
        assert_eq!(
            fast["provider"]["eaig"]["models"]["reviewer"]["reasoning"],
            false
        );

        // Empty `extra` adds nothing to options, and the reasoning bool behaves exactly as before.
        let empty = render_with_extra(false, &Map::new());
        assert!(
            empty["provider"]["eaig"]["models"]["reviewer"]["options"]
                .get("reasoning_effort")
                .is_none(),
            "empty extra must not inject any option: {}",
            empty["provider"]["eaig"]["models"]["reviewer"]["options"]
        );
        assert_eq!(
            empty["provider"]["eaig"]["models"]["reviewer"]["reasoning"],
            true
        );
    }

    #[test]
    fn extra_threads_the_whole_map_not_just_reasoning_effort() {
        // The whole `extra` map flows (parity with the native chat-body merge), so an operator's
        // `top_p`/`max_tokens` reach eaig too — the fix is not special-cased to `reasoning_effort`.
        let extra = Map::from_iter([
            ("reasoning_effort".to_string(), json!("high")),
            ("top_p".to_string(), json!(0.9)),
            ("max_tokens".to_string(), json!(4096)),
        ]);
        let options =
            &render_with_extra(false, &extra)["provider"]["eaig"]["models"]["reviewer"]["options"];
        assert_eq!(options["reasoning_effort"], "high");
        assert_eq!(options["top_p"], 0.9);
        assert_eq!(options["max_tokens"], 4096);
    }

    #[test]
    fn explicit_temperature_wins_over_a_temperature_in_extra() {
        // `temperature` is a dedicated tier field; if an operator also puts one in `extra`, the explicit
        // tier value takes precedence (extra is merged first, the tier temperature patched last).
        let extra = Map::from_iter([("temperature".to_string(), json!(0.99))]);
        let config = render_review_config(false, Some(0.1), &extra, &[], None).config;
        assert_eq!(
            config["provider"]["eaig"]["models"]["reviewer"]["options"]["temperature"],
            0.1
        );
    }

    #[test]
    fn forwards_attribution_as_provider_headers() {
        let config = render(
            false,
            None,
            &[("x-project".to_string(), "acme".to_string())],
        );
        assert_eq!(
            config["provider"]["eaig"]["options"]["headers"]["x-project"],
            "acme"
        );
        // Empty attribution → empty headers object, never missing.
        let none = render(false, None, &[]);
        assert!(none["provider"]["eaig"]["options"]["headers"].is_object());
        assert_eq!(
            none["provider"]["eaig"]["options"]["headers"]
                .as_object()
                .map(Map::len),
            Some(0)
        );
    }

    #[test]
    fn threads_temperature_only_when_set() {
        let with = render(false, Some(0.2), &[]);
        let without = render(false, None, &[]);
        assert_eq!(
            with["provider"]["eaig"]["models"]["reviewer"]["options"]["temperature"],
            0.2
        );
        assert!(
            without["provider"]["eaig"]["models"]["reviewer"]["options"]
                .get("temperature")
                .is_none()
        );
    }

    #[test]
    fn absent_and_empty_overlay_are_no_ops_and_leave_no_breaches() {
        let base_plus_injection = render_review_config(false, None, &Map::new(), &[], None);
        let empty_overlay = render_review_config(false, None, &Map::new(), &[], Some(&json!({})));
        // Byte-identical (no behaviour change) and no floor breach for absent/empty overlay.
        assert_eq!(base_plus_injection.config, empty_overlay.config);
        assert!(base_plus_injection.floor_breaches.is_empty());
        assert!(empty_overlay.floor_breaches.is_empty());
    }

    #[test]
    fn overlay_adds_a_subagent_without_breaching_the_floor() {
        let overlay = json!({
            "agent": {
                "explore": {
                    "mode": "subagent",
                    "description": "read-only investigation helper",
                    "prompt": "explore the code"
                }
            }
        });
        let rendered = render_review_config(false, None, &Map::new(), &[], Some(&overlay));
        // The custom sub-agent is present…
        assert_eq!(rendered.config["agent"]["explore"]["mode"], "subagent");
        // …and the base `review` agent is untouched (recursive object merge).
        assert_eq!(rendered.config["agent"]["review"]["mode"], "primary");
        // Adding an agent relaxes nothing.
        assert!(rendered.floor_breaches.is_empty());
        assert!(rendered.disclosure_note().is_none());
    }

    #[test]
    fn overlay_can_point_a_tier_at_a_different_model_without_a_breach() {
        // A model/provider swap is a legitimate operator override (ADR-0099 §3) and is NOT a floor
        // relaxation — no warning.
        let overlay = json!({
            "provider": { "openrouter": { "npm": "@openrouter/ai-sdk-provider", "name": "OpenRouter" } },
            "model": "openrouter/some-model"
        });
        let rendered = render_review_config(false, None, &Map::new(), &[], Some(&overlay));
        assert_eq!(rendered.config["model"], "openrouter/some-model");
        assert_eq!(
            rendered.config["provider"]["openrouter"]["name"],
            "OpenRouter"
        );
        // The base eaig provider still present (recursive merge, not replaced).
        assert_eq!(rendered.config["provider"]["eaig"]["name"], "eaig");
        assert!(rendered.floor_breaches.is_empty());
    }

    #[test]
    fn overlay_opening_permission_bash_fires_the_floor_warning() {
        let overlay = json!({ "permission": { "bash": "allow" } });
        let rendered = render_review_config(false, None, &Map::new(), &[], Some(&overlay));
        // Full override took effect…
        assert_eq!(rendered.config["permission"]["bash"], "allow");
        // …AND the floor warning fired for it (only it — edit/webfetch still deny).
        assert!(
            rendered
                .floor_breaches
                .iter()
                .any(|b| b.message.contains("permission `bash` opened")),
            "expected a bash-permission breach, got {:?}",
            rendered.floor_breaches
        );
        assert!(rendered.disclosure_note().is_some());
    }

    #[test]
    fn overlay_re_enabling_a_builtin_fires_the_floor_warning() {
        let overlay = json!({ "tools": { "read": true } });
        let rendered = render_review_config(false, None, &Map::new(), &[], Some(&overlay));
        assert_eq!(rendered.config["tools"]["read"], true);
        assert!(
            rendered
                .floor_breaches
                .iter()
                .any(|b| b.message.contains("built-in tool `read` re-enabled")),
            "expected a read re-enable breach, got {:?}",
            rendered.floor_breaches
        );
    }

    #[test]
    fn overlay_reopening_a_builtin_or_permission_for_one_agent_fires_the_floor_warning() {
        // A per-agent grant (ADR-0099's explore/file-scout shape) closes NOTHING at the top level —
        // `tools.bash`/`permission.bash` both stay at their floor default — so only the per-agent check
        // can catch it. Regression guard: this used to slip past `Floor::diff` entirely.
        let overlay = json!({
            "agent": {
                "explore": {
                    "tools": { "bash": true, "webfetch": true, "lightbridge_read_file": true },
                    "permission": { "bash": "allow", "webfetch": "allow" }
                }
            }
        });
        let rendered = render_review_config(false, None, &Map::new(), &[], Some(&overlay));
        // The top-level floor is untouched (still closed for the primary).
        assert_eq!(rendered.config["tools"]["bash"], false);
        assert_eq!(rendered.config["permission"]["bash"], "deny");
        // …but the per-agent relaxation is still caught.
        assert!(
            rendered.floor_breaches.iter().any(|b| b
                .message
                .contains("agent `explore` re-enabled built-in tool `bash`")),
            "expected an agent-scoped bash re-enable breach, got {:?}",
            rendered.floor_breaches
        );
        assert!(
            rendered.floor_breaches.iter().any(|b| b
                .message
                .contains("agent `explore` re-enabled built-in tool `webfetch`")),
            "expected an agent-scoped webfetch re-enable breach, got {:?}",
            rendered.floor_breaches
        );
        assert!(
            rendered.floor_breaches.iter().any(|b| b
                .message
                .contains("agent `explore` opened permission `bash`")),
            "expected an agent-scoped bash-permission breach, got {:?}",
            rendered.floor_breaches
        );
        assert!(
            rendered.floor_breaches.iter().any(|b| b
                .message
                .contains("agent `explore` opened permission `webfetch`")),
            "expected an agent-scoped webfetch-permission breach, got {:?}",
            rendered.floor_breaches
        );
        // Granting a MEDIATED tool (not a disabled built-in name) is NOT itself a breach.
        assert!(
            !rendered
                .floor_breaches
                .iter()
                .any(|b| b.message.contains("lightbridge_read_file")),
            "granting a mediated tool must not be reported as a floor breach: {:?}",
            rendered.floor_breaches
        );
        assert!(rendered.disclosure_note().is_some());
    }

    #[test]
    fn overlay_replacing_plugins_or_dropping_the_mcp_fires_the_floor_warning() {
        // Replacing the `plugin` ARRAY drops all three first-party plugins (full override on arrays).
        let overlay = json!({ "plugin": ["/some/custom/plugin.ts"] });
        let rendered = render_review_config(false, None, &Map::new(), &[], Some(&overlay));
        assert_eq!(
            rendered
                .floor_breaches
                .iter()
                .filter(|b| b.message.contains("required plugin"))
                .count(),
            3,
            "all three base plugins should be reported dropped: {:?}",
            rendered.floor_breaches
        );

        // Replacing the lightbridge MCP command is a breach.
        let overlay = json!({ "mcp": { "lightbridge": { "type": "local", "command": ["evil"], "enabled": true } } });
        let rendered = render_review_config(false, None, &Map::new(), &[], Some(&overlay));
        assert!(
            rendered
                .floor_breaches
                .iter()
                .any(|b| b.message.contains("`lightbridge` MCP was replaced")),
            "expected a lightbridge-replaced breach, got {:?}",
            rendered.floor_breaches
        );
    }

    #[test]
    fn strip_jsonc_comments_preserves_slashes_inside_strings() {
        let input = r#"{
            // a line comment
            "url": "https://opencode.ai/config.json", /* block */
            "path": "/opt/x", "q": "a \"//b\" c"
        }"#;
        let stripped = strip_jsonc_comments(input);
        let value: Value = serde_json::from_str(&stripped).expect("valid JSON after strip");
        assert_eq!(value["url"], "https://opencode.ai/config.json");
        assert_eq!(value["path"], "/opt/x");
        assert_eq!(value["q"], r#"a "//b" c"#);
    }

    #[test]
    fn deep_merge_recurses_objects_and_replaces_arrays_and_scalars() {
        let mut base = json!({ "a": { "b": 1, "c": [1, 2] }, "d": "x" });
        deep_merge(&mut base, &json!({ "a": { "c": [9], "e": 2 }, "d": "y" }));
        // Objects merge recursively; the untouched key survives.
        assert_eq!(base["a"]["b"], 1);
        assert_eq!(base["a"]["e"], 2);
        // Arrays and scalars are replaced wholesale.
        assert_eq!(base["a"]["c"], json!([9]));
        assert_eq!(base["d"], "y");
    }
}
