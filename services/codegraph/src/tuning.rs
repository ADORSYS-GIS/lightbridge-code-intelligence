//! Operator-tunable indexer knobs (ADR-0010 / epic #5), absorbed verbatim from
//! `agent-runner/src/indexer/mod.rs` so the embedding-path behaviour is byte-for-byte unchanged
//! (ADR-0086: "batching/round-trip tuning carries over verbatim"). Read from the environment once
//! per run and clamped to ≥1 so a misconfiguration can't wedge the pipeline.

/// Operator-tunable indexer knobs. These were hardcoded, which made a downstream limit (e.g. an
/// AI-gateway capping the batched-embedding *response* size) impossible to work around without a
/// rebuild — the reason this struct exists.
#[derive(Clone, Copy, Debug)]
pub struct IndexTuning {
    /// Chunks embedded + submitted per round-trip. Larger = fewer requests (kinder to per-minute
    /// rate limits) but a bigger embeddings response body, which some gateways cap.
    /// `INDEX_EMBED_BATCH_SIZE` (default 32).
    pub embed_batch_size: usize,
    /// Max lines a structured (tree-sitter) chunk may span before it is split into windows.
    /// `INDEX_MAX_CHUNK_LINES` (default 150).
    pub max_chunk_lines: usize,
    /// Windowed-fallback window size, in lines. `INDEX_WINDOW_SIZE` (default 100).
    pub window_size: usize,
    /// Windowed-fallback step, in lines (overlap = `window_size - window_step`). `INDEX_WINDOW_STEP`
    /// (default 50).
    pub window_step: usize,
}

impl Default for IndexTuning {
    fn default() -> Self {
        Self {
            embed_batch_size: 32,
            max_chunk_lines: 150,
            window_size: 100,
            window_step: 50,
        }
    }
}

impl IndexTuning {
    /// Read the knobs from the environment, falling back to [`Default`] and clamping each to ≥1.
    #[must_use]
    pub fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            embed_batch_size: env_usize("INDEX_EMBED_BATCH_SIZE", defaults.embed_batch_size),
            max_chunk_lines: env_usize("INDEX_MAX_CHUNK_LINES", defaults.max_chunk_lines),
            window_size: env_usize("INDEX_WINDOW_SIZE", defaults.window_size),
            window_step: env_usize("INDEX_WINDOW_STEP", defaults.window_step),
        }
    }
}

/// Parse a `usize` env var, clamping to ≥1; falls back to `default` when unset or unparseable.
fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .map(|n| n.max(1))
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::{IndexTuning, env_usize};

    #[test]
    fn defaults_match_the_historical_hardcoded_values() {
        let tuning = IndexTuning::default();
        assert_eq!(tuning.embed_batch_size, 32);
        assert_eq!(tuning.max_chunk_lines, 150);
        assert_eq!(tuning.window_size, 100);
        assert_eq!(tuning.window_step, 50);
    }

    #[test]
    fn env_usize_parses_clamps_to_one_and_falls_back() {
        // A test-unique key so this never races another test's environment.
        let key = "LCI_CODEGRAPH_TEST_ENV_USIZE";
        unsafe { std::env::remove_var(key) };
        assert_eq!(env_usize(key, 7), 7, "unset → default");
        unsafe { std::env::set_var(key, "12") };
        assert_eq!(env_usize(key, 7), 12, "parses a value");
        unsafe { std::env::set_var(key, "0") };
        assert_eq!(env_usize(key, 7), 1, "zero clamps to 1");
        unsafe { std::env::set_var(key, "not-a-number") };
        assert_eq!(env_usize(key, 7), 7, "unparseable → default");
        unsafe { std::env::remove_var(key) };
    }
}
