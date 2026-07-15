//! Small `serde` deserialize helpers shared by the stream ([`super::stream`]) and non-stream
//! ([`super::wire`]) response DTOs.

use serde::{Deserialize, Deserializer};

/// Deserialize a value that may arrive as an explicit JSON `null`, substituting `T::default()`.
///
/// `#[serde(default)]` only fills a default when a key is **absent**; an explicit `"field": null`
/// still routes into `T`'s own `Deserialize`, which for a `Vec`/collection fails the *whole* object
/// with `invalid type: null, expected a sequence`. Several eaig gateway backends stream reasoning /
/// content deltas carrying an explicit `"tool_calls": null` (GLM-5.2, MiMo); without this the entire
/// chunk fails to parse and the delta's `reasoning_content` / `content` is silently dropped, so a
/// deep-tier review logs `reasoning_chars: 0` on every turn (issue #411).
///
/// Pair with `#[serde(default, deserialize_with = "null_as_default")]` so both an absent key and an
/// explicit `null` collapse to the default.
pub(super) fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}
