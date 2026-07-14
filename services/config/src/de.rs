//! `serde` field deserializers that accept **a number or a numeric string**. Substitution always
//! yields strings, so a numeric config field written as `"{env:DEADLINE:-3600}"` arrives as the
//! string `"3600"`; these let it deserialize anyway (while a literal JSON number still works). Empty
//! / null → `None`. Annotate numeric `Option` fields with
//! `#[serde(default, deserialize_with = "lci_config::de::opt_u64")]`.

use serde::de::Error;
use serde::{Deserialize, Deserializer};
use std::fmt::Display;
use std::str::FromStr;

/// A JSON number of type `T`, or a numeric string that parses as `T` (substitution always yields
/// strings). Deserializing the `Num` arm directly into `T` — rather than through a fixed-width
/// intermediate like `i64` — is what makes this correct for every target width: a literal JSON `-5`
/// or a substituted `"-5"` against `T = u64` now goes through `u64`'s own (rejecting) parse/
/// deserialize, instead of silently clamping through an `i64` detour; a value in `i64::MAX..=u64::MAX`
/// deserializes straight into `u64`/`usize` instead of failing to fit in the old `i64` intermediate;
/// and the target width (`u64`/`usize`) is respected on every platform, so there's no `as usize`
/// truncation step on 32-bit targets.
#[derive(Deserialize)]
#[serde(untagged)]
enum NumOrStr<T> {
    Num(T),
    Str(String),
}

fn parse_opt<'de, T, D>(d: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de> + FromStr,
    T::Err: Display,
    D: Deserializer<'de>,
{
    match Option::<NumOrStr<T>>::deserialize(d)? {
        None => Ok(None),
        Some(NumOrStr::Num(n)) => Ok(Some(n)),
        Some(NumOrStr::Str(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                trimmed.parse::<T>().map(Some).map_err(D::Error::custom)
            }
        }
    }
}

pub fn opt_i64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    parse_opt(d)
}

pub fn opt_u64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    parse_opt(d)
}

pub fn opt_usize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<usize>, D::Error> {
    parse_opt(d)
}

/// Like [`opt_i64`] but for fractional fields (e.g. `temperature`, `top_p`): accepts a JSON number
/// or a numeric string (substitution always yields strings). Empty / null → `None`.
pub fn opt_f64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    parse_opt(d)
}

#[derive(Deserialize)]
#[serde(untagged)]
enum BoolOrStr {
    Bool(bool),
    Str(String),
}

/// Like [`opt_i64`] but for boolean flags (e.g. `review.stream`): accepts a JSON bool or a string
/// (`{env:…}` substitution always yields strings), so `"true"`/`"1"`/`"yes"`/`"on"` → `true` and
/// `"false"`/`"0"`/`"no"`/`"off"` → `false` (case-insensitive). Empty / null → `None`; any other
/// string is an error rather than a silent default.
pub fn opt_bool<'de, D: Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    match Option::<BoolOrStr>::deserialize(d)? {
        None => Ok(None),
        Some(BoolOrStr::Bool(b)) => Ok(Some(b)),
        Some(BoolOrStr::Str(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            match trimmed.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => Ok(Some(true)),
                "false" | "0" | "no" | "off" => Ok(Some(false)),
                other => Err(D::Error::custom(format!(
                    "invalid boolean {other:?} (expected true/false/1/0/yes/no/on/off)"
                ))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::load_with;
    use crate::test_support::env_of;

    #[test]
    fn numeric_fields_accept_env_substituted_strings() {
        #[derive(Deserialize)]
        struct Cfg {
            #[serde(default, deserialize_with = "opt_u64")]
            timeout: Option<u64>,
            #[serde(default, deserialize_with = "opt_usize")]
            size: Option<usize>,
        }
        // A `{env:…}`-substituted numeric field arrives as a string; it must still deserialize.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, r#"{ "timeout": "{env:T:-45}", "size": 1000 }"#).unwrap();

        let cfg: Cfg = load_with(&path, &env_of(&[])).unwrap();
        assert_eq!(cfg.timeout, Some(45), "numeric string from default coerces");
        assert_eq!(cfg.size, Some(1000), "literal number still works");
    }

    #[test]
    fn float_fields_accept_env_substituted_strings() {
        #[derive(Deserialize)]
        struct Cfg {
            #[serde(default, deserialize_with = "opt_f64")]
            temperature: Option<f64>,
            #[serde(default, deserialize_with = "opt_f64")]
            top_p: Option<f64>,
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        // temperature is a substituted string ("0.2"); top_p is a literal JSON number.
        std::fs::write(
            &path,
            r#"{ "temperature": "{env:TEMP:-0.2}", "top_p": 0.9 }"#,
        )
        .unwrap();

        let cfg: Cfg = load_with(&path, &env_of(&[])).unwrap();
        assert_eq!(cfg.temperature, Some(0.2), "numeric string coerces to f64");
        assert_eq!(cfg.top_p, Some(0.9), "literal float still works");
    }

    #[test]
    fn bool_fields_accept_env_substituted_strings() {
        #[derive(Deserialize)]
        struct Cfg {
            #[serde(default, deserialize_with = "opt_bool")]
            stream: Option<bool>,
            #[serde(default, deserialize_with = "opt_bool")]
            literal: Option<bool>,
            #[serde(default, deserialize_with = "opt_bool")]
            unset: Option<bool>,
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        // `stream` is an env-substituted string ("true"); `literal` is a JSON bool; `unset` resolves to
        // an empty string and must become None (not an error) — the same shape as an omitted `{env:}`.
        std::fs::write(
            &path,
            r#"{ "stream": "{env:LLM_STREAM:-true}", "literal": false, "unset": "{env:NOPE}" }"#,
        )
        .unwrap();

        let cfg: Cfg = load_with(&path, &env_of(&[])).unwrap();
        assert_eq!(cfg.stream, Some(true), "bool string from default coerces");
        assert_eq!(cfg.literal, Some(false), "literal JSON bool still works");
        assert_eq!(cfg.unset, None, "empty substitution → None, not an error");
    }

    #[test]
    fn negative_value_is_rejected_not_silently_clamped_to_zero() {
        #[derive(Deserialize)]
        struct Cfg {
            #[serde(default, deserialize_with = "opt_u64")]
            #[allow(dead_code)]
            timeout: Option<u64>,
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        // A negative substituted value must surface as an error, not clamp to 0 and mask the bug.
        std::fs::write(&path, r#"{ "timeout": "{env:T:--5}" }"#).unwrap();
        assert!(load_with::<Cfg>(&path, &env_of(&[])).is_err());
    }

    #[test]
    fn value_above_i64_max_deserializes_into_u64() {
        #[derive(Deserialize)]
        struct Cfg {
            #[serde(default, deserialize_with = "opt_u64")]
            big: Option<u64>,
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        // 18446744073709551615 = u64::MAX, well past i64::MAX (9223372036854775807) — the old i64
        // intermediate would fail to parse this even though it fits in the target u64.
        std::fs::write(&path, r#"{ "big": "18446744073709551615" }"#).unwrap();
        let cfg: Cfg = load_with(&path, &env_of(&[])).unwrap();
        assert_eq!(cfg.big, Some(u64::MAX));
    }

    #[test]
    fn bool_field_rejects_a_non_boolean_string() {
        #[derive(Deserialize)]
        struct Cfg {
            // Deserialization is expected to fail, so the field is never read — the test asserts the
            // error, not the value.
            #[serde(default, deserialize_with = "opt_bool")]
            #[allow(dead_code)]
            stream: Option<bool>,
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, r#"{ "stream": "maybe" }"#).unwrap();
        // A garbage value is surfaced as an error, not silently defaulted.
        assert!(load_with::<Cfg>(&path, &env_of(&[])).is_err());
    }
}
