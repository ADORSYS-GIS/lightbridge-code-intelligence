//! Audit-trail redaction for a resolved [`ReviewConfig`] (ADR-0034/0062/0066): everything the runner
//! persists about a run's exact configuration, with every credential-bearing field scrubbed. Split out
//! from `review.rs` because it is a security-sensitive concern in its own right — a future field added
//! to `ReviewConfig` should force a conscious decision here, not silently ride along with a `Serialize`
//! derive.

use super::file::ReviewToolSelector;
use super::review::ReviewConfig;

/// The placeholder a redacted secret is replaced with in the run-config audit trail. A distinctive
/// sentinel so the redaction test can assert the encoded payload carries it (and not the real value).
pub const REDACTED: &str = "[REDACTED]";

/// Secret values shorter than this are excluded from the value-based scrub (see
/// [`ReviewConfig::redacted_config_b64`]) — too short to be a real credential, and substring-replacing
/// them would mangle innocent text.
const MIN_SCRUB_SECRET_LEN: usize = 8;

impl ReviewConfig {
    /// The resolved config as a JSON object with **every credential-bearing field redacted** — for the
    /// run-level telemetry (ADR-0034/0062/0066), which base64-encodes this and persists it so a run's
    /// exact configuration is auditable later.
    ///
    /// SECURITY INVARIANT — redact BEFORE encode. Base64 is encoding, not encryption, so the api_key
    /// must never reach the encoder. `ReviewConfig` deliberately does NOT derive `Serialize`; this is the
    /// single, explicit serialization path so a future field can't silently leak a secret into the
    /// audit trail. Redacted here: `api_key` (the only true secret today); any `extra` field — at ANY
    /// nesting depth — whose key is credential-shaped (see [`secret_shaped_key`]; `{env:VAR}` substitution
    /// runs at every depth of the file config, so a nested `extra.azure.api_key` carries the real secret);
    /// and the userinfo/query of `base_url` (see [`sanitized_base_url`]). Everything else is included in
    /// full and IS the point of the audit: model, all budget/generation knobs, the per-tier tools
    /// allowlist, `stream`, and the ~23KB `system_prompt` (prompt churn is exactly what the owner wants
    /// auditable). [`redacted_config_b64`](Self::redacted_config_b64) adds a final VALUE-based scrub on
    /// top of this key-based pass.
    pub fn redacted_json(&self) -> serde_json::Value {
        use serde_json::json;

        let tools = self.tools.as_ref().map(|sel| {
            sel.iter()
                .map(|s| match s {
                    ReviewToolSelector::Builtin(b) => b.as_str().to_string(),
                    ReviewToolSelector::Mcp(p) => p.as_str().to_string(),
                })
                .collect::<Vec<_>>()
        });

        json!({
            "base_url": sanitized_base_url(&self.base_url),
            "api_key": REDACTED,
            "model": self.model,
            "system_prompt": self.system_prompt,
            "max_diff_chars": self.max_diff_chars,
            "max_turns": self.max_turns,
            "max_batch_size": self.max_batch_size,
            "max_files_read": self.max_files_read,
            "max_searches": self.max_searches,
            "max_batches": self.max_batches,
            "max_coverage_bounces": self.max_coverage_bounces,
            "context_window": self.context_window,
            "temperature": self.temperature,
            "top_p": self.top_p,
            "max_tokens": self.max_tokens,
            "extra": redact_value(&serde_json::Value::Object(self.extra.clone())),
            "stream": self.stream,
            "resilience": {
                "request_timeout_secs": self.resilience.request_timeout_secs,
                "max_retries": self.resilience.max_retries,
                "circuit_breaker_threshold": self.resilience.circuit_breaker_threshold,
            },
            "fast": self.fast,
            "tier": if self.fast { "fast" } else { "deep" },
            "tools": tools,
            // The operator OpenCode overlay (ADR-0099) is auditable config — WHICH custom agents/models
            // were active is exactly the churn the owner wants recorded — but it's raw operator JSON that
            // could carry an inlined credential (e.g. a `headers.authorization`), so it rides the SAME
            // recursive key-based scrub as `extra` (secrets should be `{env:}`, but defend in depth).
            "opencode_overlay": self.opencode_overlay.as_ref().map(redact_value),
        })
    }

    /// The redacted config as a base64-encoded JSON string (ADR-0034/0062/0066). Encodes the output of
    /// [`redacted_json`](Self::redacted_json) after a final **VALUE-based scrub**: any occurrence of a
    /// known live secret VALUE (the resolved api_key + the credential env vars this process runs with)
    /// in ANY string of the payload is replaced with the sentinel. The system prompt template is fully
    /// `{env:}`-substituted before it reaches this struct, so an operator template could inline
    /// `LLM_API_KEY` etc. into prose the key-based pass can't see — this backstops that, and anything
    /// else the key-based redaction ever misses.
    pub fn redacted_config_b64(&self) -> String {
        use base64::Engine as _;
        let mut value = self.redacted_json();
        let mut secrets: Vec<String> = vec![self.api_key.clone()];
        for var in ["AGENT_RUNNER_TOKEN", "EMBEDDINGS_API_KEY", "LLM_API_KEY"] {
            if let Ok(v) = std::env::var(var) {
                secrets.push(v);
            }
        }
        // A trivially-short "secret" would shred innocent prose (e.g. a 2-char api_key in a dev env
        // matching every occurrence of that bigram) — only scrub plausibly-real credentials.
        secrets.retain(|s| s.len() >= MIN_SCRUB_SECRET_LEN);
        scrub_secret_values(&mut value, &secrets);
        let json = serde_json::to_vec(&value).unwrap_or_default();
        base64::engine::general_purpose::STANDARD.encode(json)
    }
}

/// Whether a JSON key is credential-shaped, tested on a NORMALIZED form (lowercased, `-`/`_` stripped)
/// so `api_key`, `api-key`, `x-api-key`, and `apiKey` all match the one `apikey` needle.
fn secret_shaped_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|c| *c != '-' && *c != '_')
        .map(|c| c.to_ascii_lowercase())
        .collect();
    // "token" needs care: the token-COUNTING forms are ubiquitous generation knobs (`max_tokens`,
    // `max_completion_tokens`, `tokenizer`), and redacting those would destroy exactly the audit data
    // this trail exists for. A credential is the singular form (`token`, `refresh_token`, `authtoken`).
    let token_credential = normalized.contains("token")
        && !normalized.contains("tokens")
        && !normalized.contains("tokenizer");
    token_credential
        || [
            "apikey",
            "authorization",
            "secret",
            "password",
            "bearer",
            "credential",
        ]
        .iter()
        .any(|needle| normalized.contains(needle))
}

/// Recursively redact every secret-shaped key at ANY depth (objects nested in arrays included).
/// Depth matters: `lci_config::substitute_value` expands `{env:VAR}` at every level of the
/// file config, so `review.extra: {"azure": {"api_key": "{env:LLM_API_KEY}"}}` puts the REAL secret
/// in a nested object a top-level-only pass would clone verbatim.
fn redact_value(v: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, val)| {
                    let redacted = if secret_shaped_key(k) {
                        Value::String(REDACTED.to_string())
                    } else {
                        redact_value(val)
                    };
                    (k.clone(), redacted)
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(redact_value).collect()),
        other => other.clone(),
    }
}

/// Replace any occurrence of a known secret VALUE in any string of the payload with the sentinel —
/// the value-based backstop behind the key-based [`redact_value`] pass.
fn scrub_secret_values(v: &mut serde_json::Value, secrets: &[String]) {
    use serde_json::Value;
    match v {
        Value::String(s) => {
            for secret in secrets {
                if s.contains(secret.as_str()) {
                    *s = s.replace(secret.as_str(), REDACTED);
                }
            }
        }
        Value::Object(m) => m
            .values_mut()
            .for_each(|val| scrub_secret_values(val, secrets)),
        Value::Array(a) => a
            .iter_mut()
            .for_each(|val| scrub_secret_values(val, secrets)),
        _ => {}
    }
}

/// The base URL as persisted in the audit trail: URL userinfo (`https://user:pass@host`) cleared and
/// the query dropped (an `?api_key=...` query is a real gateway auth pattern). The host/path — the
/// part worth auditing — survives. An unparseable value is redacted whole rather than persisted as-is.
fn sanitized_base_url(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(mut u) => {
            // Setter failures (cannot-be-a-base URLs) leave the component unset-able; falling through
            // to the sanitized-anyway serialization is still safe because such URLs carry no userinfo.
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.set_query(None);
            u.to_string()
        }
        Err(_) => REDACTED.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::config::defaults::DEFAULT_MAX_COVERAGE_BOUNCES;
    use crate::bootstrap::config::file::{ReviewTool, ReviewToolSelector};
    use crate::bootstrap::config::review::ResilienceConfig;

    /// A fully-populated `ReviewConfig` for the redaction tests — a real-looking api_key, secret-shaped
    /// `extra` entries at TOP LEVEL and NESTED (with a hyphenated key), credentials in the base_url,
    /// the api_key value inlined in the system prompt (an `{env:}`-substituted template can do that),
    /// and both selector kinds in the tools allowlist.
    fn sample_config_with_secrets() -> ReviewConfig {
        let mut extra = serde_json::Map::new();
        extra.insert("reasoning_effort".to_string(), serde_json::json!("high"));
        // A credential smuggled through the passthrough map: must also be redacted (defence in depth).
        extra.insert(
            "Authorization".to_string(),
            serde_json::json!("Bearer super-secret-extra-token"),
        );
        // `{env:VAR}` substitution runs at EVERY depth of the file config, so a nested provider block
        // can carry the real secret — and gateways use hyphenated header names like `x-api-key`.
        extra.insert(
            "azure".to_string(),
            serde_json::json!({
                "api_key": "nested-azure-secret-value",
                "deployment": "gpt-x",
                "headers": [{ "x-api-key": "hyphenated-header-secret" }],
            }),
        );
        let tools = vec![
            ReviewToolSelector::Builtin(ReviewTool::AddReviewComment),
            serde_json::from_value::<ReviewToolSelector>(serde_json::json!("mcp__context7__.*"))
                .expect("valid mcp selector"),
        ];
        ReviewConfig {
            base_url: "https://user:url-pass-secret@gateway.internal/v1?api_key=query-secret"
                .to_string(),
            api_key: "sk-live-DEADBEEF-do-not-leak".to_string(),
            model: "adorsys-reviewer".to_string(),
            // The template is fully `{env:}`-substituted before it reaches ReviewConfig — simulate an
            // operator template that inlined the api_key into prose (the value-based scrub's target).
            system_prompt: "You are a careful reviewer. Only claim what the diff proves. \
                            (misconfigured template inlined: sk-live-DEADBEEF-do-not-leak)"
                .to_string(),
            max_diff_chars: 60_000,
            max_turns: 40,
            max_batch_size: 8,
            max_files_read: 50,
            max_searches: 30,
            max_batches: 12,
            max_coverage_bounces: DEFAULT_MAX_COVERAGE_BOUNCES,
            context_window: Some(128_000),
            temperature: Some(0.2),
            top_p: None,
            max_tokens: Some(4096),
            extra,
            stream: true,
            resilience: ResilienceConfig::default(),
            fast: false,
            tools: Some(tools),
            // An operator overlay (ADR-0099) carrying a misconfigured INLINE credential (secrets are
            // meant to ride `{env:}`, but defend in depth) plus a non-secret custom sub-agent — the
            // former must be redacted, the latter must survive as audit data.
            opencode_overlay: Some(serde_json::json!({
                "provider": { "eaig": { "options": { "headers": {
                    "authorization": "Bearer overlay-inlined-secret-token"
                }}}},
                "agent": { "explore": { "mode": "subagent", "description": "read-only helper" } }
            })),
        }
    }

    /// SECURITY: the base64 run-config audit payload must NEVER contain the api_key value (base64 is
    /// encoding, not encryption — redaction happens before the encode). It must also redact a
    /// credential smuggled through `review.extra`, and it must round-trip decode to a JSON object that
    /// carries the redaction sentinel plus the non-secret knobs (model, prompt, budgets, tools).
    #[test]
    fn redacted_config_never_leaks_the_api_key() {
        use base64::Engine as _;
        let cfg = sample_config_with_secrets();
        let b64 = cfg.redacted_config_b64();

        // The encoded payload must not carry the secret in any form.
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&b64)
            .expect("valid base64");
        let decoded_str = String::from_utf8(decoded).expect("utf8");
        assert!(
            !b64.contains(
                &base64::engine::general_purpose::STANDARD.encode(cfg.api_key.as_bytes())
            ),
            "the api_key must not survive base64 encoding"
        );
        assert!(
            !decoded_str.contains(&cfg.api_key),
            "the decoded payload must not contain the api_key value (incl. inlined in the prompt)"
        );
        assert!(
            !decoded_str.contains("super-secret-extra-token"),
            "a credential smuggled through review.extra must also be redacted"
        );
        assert!(
            !decoded_str.contains("nested-azure-secret-value"),
            "redaction must recurse: a secret in a NESTED extra object must not survive"
        );
        assert!(
            !decoded_str.contains("hyphenated-header-secret"),
            "hyphenated keys (x-api-key) inside nested arrays must be redacted too"
        );
        assert!(
            !decoded_str.contains("url-pass-secret") && !decoded_str.contains("query-secret"),
            "base_url userinfo and query credentials must not be persisted"
        );
        assert!(
            !decoded_str.contains("overlay-inlined-secret-token"),
            "an inlined credential in the operator OpenCode overlay (ADR-0099) must be redacted"
        );

        // Round-trip decode: the audit payload is the resolved config with secrets redacted.
        let value: serde_json::Value = serde_json::from_str(&decoded_str).expect("valid json");
        assert_eq!(value["api_key"], serde_json::json!(REDACTED));
        assert_eq!(value["extra"]["Authorization"], serde_json::json!(REDACTED));
        assert_eq!(
            value["extra"]["azure"]["api_key"],
            serde_json::json!(REDACTED)
        );
        assert_eq!(
            value["extra"]["azure"]["headers"][0]["x-api-key"],
            serde_json::json!(REDACTED)
        );
        // The non-secret knobs ARE recorded — this is the point of the audit trail.
        assert_eq!(value["model"], serde_json::json!("adorsys-reviewer"));
        assert_eq!(
            value["extra"]["reasoning_effort"],
            serde_json::json!("high")
        );
        assert_eq!(
            value["extra"]["azure"]["deployment"],
            serde_json::json!("gpt-x"),
            "non-secret NESTED fields survive the recursive pass"
        );
        // Host + path survive; userinfo and query do not.
        assert_eq!(
            value["base_url"],
            serde_json::json!("https://gateway.internal/v1")
        );
        assert_eq!(value["stream"], serde_json::json!(true));
        assert_eq!(value["tier"], serde_json::json!("deep"));
        let prompt = value["system_prompt"].as_str().unwrap();
        assert!(
            prompt.contains("careful reviewer"),
            "the system prompt is captured verbatim (prompt churn is auditable)"
        );
        assert!(
            prompt.contains(REDACTED),
            "an api_key value inlined into the prompt is value-scrubbed to the sentinel"
        );
        // The per-tier tools allowlist is recorded by selector string (builtin name + mcp regex).
        let tools = value["tools"].as_array().expect("tools array");
        assert_eq!(tools[0], serde_json::json!("add_review_comment"));
        assert_eq!(tools[1], serde_json::json!("mcp__context7__.*"));
        // The operator overlay (ADR-0099): its inlined credential is redacted, its non-secret custom
        // sub-agent survives as audit data.
        assert_eq!(
            value["opencode_overlay"]["provider"]["eaig"]["options"]["headers"]["authorization"],
            serde_json::json!(REDACTED)
        );
        assert_eq!(
            value["opencode_overlay"]["agent"]["explore"]["mode"],
            serde_json::json!("subagent")
        );
    }

    /// The value-based scrub also catches the runner's own credential env vars when a template inlines
    /// them (`{env:}` substitution runs before the prompt reaches `ReviewConfig`), and the key
    /// normalization matches hyphenated/camelCase credential shapes.
    #[test]
    fn secret_shaped_key_normalizes_separators_and_case() {
        for key in [
            "api_key",
            "api-key",
            "x-api-key",
            "apiKey",
            "X-Api-Key",
            "AUTHORIZATION",
            "refresh_token",
            "client-secret",
            "PASSWORD",
            "bearerToken",
            "aws_credentials",
        ] {
            assert!(secret_shaped_key(key), "{key} should be secret-shaped");
        }
        for key in [
            "model",
            "reasoning_effort",
            "max_tokens",
            "max_completion_tokens",
            "tokenizer",
            "deployment",
        ] {
            assert!(!secret_shaped_key(key), "{key} should NOT be secret-shaped");
        }
    }

    /// `sanitized_base_url` keeps the auditable part (scheme/host/path), drops userinfo + query, and
    /// redacts an unparseable value whole rather than persisting it as-is.
    #[test]
    fn sanitized_base_url_strips_credentials() {
        assert_eq!(
            sanitized_base_url("https://u:p@gw.example/v1?api_key=s&x=1"),
            "https://gw.example/v1"
        );
        assert_eq!(
            sanitized_base_url("https://gw.example/v1"),
            "https://gw.example/v1"
        );
        assert_eq!(sanitized_base_url("not a url"), REDACTED);
    }
}
