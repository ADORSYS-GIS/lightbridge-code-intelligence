//! Shared test-only helpers, used by the unit tests in [`crate::substitute`], [`crate::loader`],
//! and [`crate::de`].

use std::collections::HashMap;

/// Build an env `lookup` closure from a fixed set of `(name, value)` pairs.
pub fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |name: &str| map.get(name).cloned()
}
