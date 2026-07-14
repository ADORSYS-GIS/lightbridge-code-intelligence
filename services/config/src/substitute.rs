//! `{env:VAR}` / `{env:VAR:-default}` substitution grammar.
//!
//! Scans a string — or every string in a parsed JSON tree — for env references and resolves each
//! one through an injectable `lookup` function, so the grammar itself is fully unit-testable
//! without touching the real process environment. [`substitute_env`] is the process-env-bound
//! convenience wrapper used at the call sites.

/// Marker for an env reference: `{env:NAME}` or `{env:NAME:-default}`.
const OPEN: &str = "{env:";
/// Default separator inside an env reference, e.g. `{env:NAME:-fallback}`.
const DEFAULT_SEP: &str = ":-";

/// Substitute every `{env:VAR}` / `{env:VAR:-default}` in `input` using `lookup`.
///
/// Resolution order per reference: `lookup(var)` → the inline `:-default` → empty string. An
/// unterminated `{env:` (no closing `}`) is left verbatim. `lookup` is injected (not hard-wired to
/// the process env) so the behaviour is fully unit-testable.
pub fn substitute_with(input: &str, lookup: &impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        let after = &rest[start + OPEN.len()..];
        let Some(end) = after.find('}') else {
            // No closing brace — not a real reference; keep the rest literally.
            out.push_str(&rest[start..]);
            return out;
        };
        let body = &after[..end];
        let (name, default) = match body.split_once(DEFAULT_SEP) {
            Some((name, default)) => (name.trim(), Some(default)),
            None => (body.trim(), None),
        };
        let value = lookup(name)
            .or_else(|| default.map(str::to_string))
            .unwrap_or_default();
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// [`substitute_with`] bound to the process environment.
pub fn substitute_env(input: &str) -> String {
    substitute_with(input, &|name| std::env::var(name).ok())
}

/// Recursively substitute `{env:…}` in every string within a parsed JSON value.
pub fn substitute_value(value: &mut serde_json::Value, lookup: &impl Fn(&str) -> Option<String>) {
    match value {
        serde_json::Value::String(s) => *s = substitute_with(s, lookup),
        serde_json::Value::Array(items) => {
            for item in items {
                substitute_value(item, lookup);
            }
        }
        serde_json::Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                substitute_value(v, lookup);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::env_of;

    #[test]
    fn substitutes_present_var() {
        let look = env_of(&[("FOO", "bar")]);
        assert_eq!(substitute_with("a-{env:FOO}-b", &look), "a-bar-b");
    }

    #[test]
    fn falls_back_to_inline_default_then_empty() {
        let look = env_of(&[]);
        assert_eq!(substitute_with("{env:MISSING:-def}", &look), "def");
        assert_eq!(substitute_with("x{env:MISSING}y", &look), "xy");
    }

    #[test]
    fn present_var_wins_over_default() {
        let look = env_of(&[("MODEL", "qwen")]);
        assert_eq!(substitute_with("{env:MODEL:-fallback}", &look), "qwen");
    }

    #[test]
    fn handles_multiple_and_literals_and_unterminated() {
        let look = env_of(&[("A", "1"), ("B", "2")]);
        assert_eq!(substitute_with("{env:A}/{env:B}!", &look), "1/2!");
        assert_eq!(substitute_with("no refs here", &look), "no refs here");
        // An unterminated reference is left exactly as-is.
        assert_eq!(substitute_with("oops {env:A", &look), "oops {env:A");
    }

    #[test]
    fn default_may_contain_colons_and_urls() {
        let look = env_of(&[]);
        assert_eq!(
            substitute_with("{env:URL:-https://gw:443/v1}", &look),
            "https://gw:443/v1"
        );
    }

    #[test]
    fn substitutes_through_a_json_tree() {
        let look = env_of(&[("KEY", "secret"), ("N", "5")]);
        let mut v = serde_json::json!({
            "review": { "api_key": "{env:KEY}", "model": "{env:MODEL:-default-model}" },
            "list": ["{env:N}", "plain"]
        });
        substitute_value(&mut v, &look);
        assert_eq!(v["review"]["api_key"], "secret");
        assert_eq!(v["review"]["model"], "default-model");
        assert_eq!(v["list"][0], "5");
        assert_eq!(v["list"][1], "plain");
    }
}
