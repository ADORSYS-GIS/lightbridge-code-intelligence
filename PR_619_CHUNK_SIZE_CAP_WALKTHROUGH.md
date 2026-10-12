# Walkthrough: capping indexer chunk size (PR #619, issue #618)

## The problem

Two independent repos failed to index, permanently and deterministically, because a single
oversized "chunk" reached the embeddings API and got rejected outright:

- `adorsys/xs2a/qwac-assessor` — failed twice, byte-identically, at "embedding batch 27". HTTP `400`
  from `api.fireworks.ai`, request body 244,907 bytes (~2x any successful batch in the same run).
- `ADORSYS-GIS/webank-models` — failed twice, byte-identically:
  `"The prompt is too long: 42774, model maximum context length: 40959"`. Reproduced locally against
  a clone of the repo: the largest chunk was a 100-line window of `ROADMAP.md` — 160,653 bytes,
  because that file's changelog entries are single unwrapped paragraphs up to ~2,500 characters per
  line. `160,653 bytes ÷ 42,774 tokens ≈ 3.76 bytes/token`, matching the reported failure closely.

Both are `services/agent-runner/src/indexer/chunker.rs` producing a chunk with no upper bound on
byte size — full evidence and log excerpts are in issue #618.

Why "permanently, deterministically" matters: chunking is pure function of file content. Every
retry, every pod restart, every future push re-derives the identical chunk set and fails at the
identical spot. This isn't a flaky infra issue — it's a standing outage for any repo that trips it.

---

## Change 1 — a new operator-tunable knob: `max_chunk_bytes`

`services/agent-runner/src/indexer/mod.rs` already had four tunable knobs on `IndexTuning`
(`embed_batch_size`, `max_chunk_lines`, `window_size`, `window_step`), each backed by an env var and
clamped to ≥1. All four are **line-count** based. None of them bound byte size.

```rust
pub struct IndexTuning {
    pub embed_batch_size: usize,
    pub max_chunk_lines: usize,
    pub window_size: usize,
    pub window_step: usize,
    /// Ceiling on a single chunk's content length, in bytes, applied after chunking regardless of
    /// strategy. `INDEX_MAX_CHUNK_BYTES` (default 16_000).
    pub max_chunk_bytes: usize,
}
```

```rust
impl Default for IndexTuning {
    fn default() -> Self {
        Self {
            embed_batch_size: 32,
            max_chunk_lines: 150,
            window_size: 100,
            window_step: 50,
            max_chunk_bytes: 16_000,
        }
    }
}

impl IndexTuning {
    pub fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            embed_batch_size: env_usize("INDEX_EMBED_BATCH_SIZE", defaults.embed_batch_size),
            max_chunk_lines: env_usize("INDEX_MAX_CHUNK_LINES", defaults.max_chunk_lines),
            window_size: env_usize("INDEX_WINDOW_SIZE", defaults.window_size),
            window_step: env_usize("INDEX_WINDOW_STEP", defaults.window_step),
            max_chunk_bytes: env_usize("INDEX_MAX_CHUNK_BYTES", defaults.max_chunk_bytes),
        }
    }
}
```

**Why a new field instead of repurposing an existing one:** none of the four existing knobs are
byte-based, and a line-count knob can never give a byte guarantee — a "100-line window" can be 4KB
or 160KB depending on how long the lines are (this is exactly what happened with `ROADMAP.md`).
Shrinking `window_size` to be "safe" for one file's line lengths would over-fragment every other
repo's normal-length-line files, with no actual guarantee against the next repo having even longer
lines. A byte cap is the only thing that can make a real promise.

**Why 16,000 bytes as the default:** conservative — roughly 4-5k tokens at typical density — well
under any current model's real limit, and small enough that a chunk stays a coherent, useful
semantic unit for review/search rather than maxing out whatever a specific model happens to allow.

---

## Change 2 — the actual cap, applied once at the single funnel point

Both of `chunker.rs`'s chunk-producing strategies (tree-sitter and the line-window fallback) already
converge on one entry point, `chunk_file()`. Rather than adding a size check into each strategy
separately, the cap is a **single post-processing pass** there:

```rust
pub fn chunk_file(
    file_path: &str,
    source: &str,
    language: &str,
    tuning: super::IndexTuning,
) -> Vec<Chunk> {
    if source.len() > MAX_FILE_BYTES {
        return Vec::new();
    }
    if source.as_bytes().iter().take(512).any(|&b| b == 0) {
        return Vec::new();
    }

    let chunks = if super::language::has_grammar(language)
        && let Some(chunks) = try_treesitter(file_path, source, language, tuning)
        && !chunks.is_empty()
    {
        chunks
    } else {
        window_chunks(file_path, source, language, tuning)
    };

    // Neither strategy above bounds a chunk's byte size on its own (a tree-sitter symbol with no
    // splittable children, or a windowed slice of long unwrapped lines, can both come out huge) —
    // this pass is the single place that guarantees every chunk fits an embedding model's input.
    chunks
        .into_iter()
        .flat_map(|c| cap_chunk_bytes(c, tuning))
        .collect()
}
```

```rust
/// Split a chunk whose content exceeds `tuning.max_chunk_bytes` into smaller line-windowed pieces.
fn cap_chunk_bytes(chunk: Chunk, tuning: super::IndexTuning) -> Vec<Chunk> {
    if chunk.content.len() <= tuning.max_chunk_bytes {
        return vec![chunk];
    }
    let lines: Vec<&str> = chunk.content.lines().collect();
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < lines.len() {
        // Always take at least one line, so a single line longer than the cap still terminates.
        let mut end = start + 1;
        let mut size = lines[start].len();
        while end < lines.len() {
            // +1 accounts for the "\n" the join() below inserts between lines — omitting it
            // undercounts by one byte per line, so the emitted content can exceed the cap.
            let with_next = size + 1 + lines[end].len();
            if with_next > tuning.max_chunk_bytes {
                break;
            }
            size = with_next;
            end += 1;
        }
        out.push(Chunk {
            file_path: chunk.file_path.clone(),
            language: chunk.language.clone(),
            chunk_type: chunk.chunk_type.clone(),
            symbol_name: chunk.symbol_name.clone(),
            start_line: chunk.start_line + start as i32,
            end_line: chunk.start_line + end as i32 - 1,
            content: lines[start..end].join("\n"),
        });
        start = end;
    }
    out
}
```

**Why one pass instead of two size checks:** avoids duplicating the same logic into
`window_chunks` and `collect_items` separately — one function to reason about, one place a future
reader checks for "is a chunk's size actually bounded."

**Why reuse line-windowing instead of a byte-slice:** slicing raw bytes could cut a chunk mid-line
(or mid-UTF-8-character), producing garbage content. Splitting on line boundaries keeps every
sub-chunk's content well-formed.

**Why "always take at least one line" (`end = start + 1` unconditionally):** guarantees termination.
Without it, a single line longer than the cap (e.g. one giant minified JSON line) would never let
`end` advance, looping forever. With it, that line becomes its own one-line chunk — still possibly
over the cap, but bounded (one line, not unbounded), and the loop always makes forward progress.

---

## Change 3 — retiring a false assumption in the tree-sitter path

`collect_items()` has a branch for a tree-sitter symbol (function/struct/enum/...) that's too big to
keep whole (`span > tuning.max_chunk_lines`) but has no smaller sub-nodes to split into instead. The
old comment here was the root of the "why did this ever ship without a cap" question:

```rust
// Before:
// No interesting sub-nodes (e.g. a 200-line function with no nested fns).
// Emit it as a single chunk rather than silently dropping it; the embedding
// API will truncate if the content exceeds the model's context window.
```

That assumption is false — both production incidents prove the API **rejects** oversized input with
an HTTP 400, it does not silently truncate it. Updated to describe what actually happens now that
Change 2 exists:

```rust
// After:
// No interesting sub-nodes (e.g. a 200-line function with no nested fns).
// Emit it as one chunk rather than silently dropping the symbol —
// `chunk_file`'s cap_chunk_bytes pass splits it further if it's oversized.
```

This is a comment-only change — the code path is untouched; it now relies on Change 2's pass to
actually bound the output, which is true, where the old comment's claim never was.

---

## Change 4 — a bot review caught a real bug in Change 2

`lightbridge-assistant`'s automated review on this PR flagged a genuine P2 correctness issue in
`cap_chunk_bytes`, traced and confirmed by hand before fixing:

**The bug:** the original `size` accumulator summed only `lines[i].len()` for each line included in
the window, but the emitted content is `lines[start..end].join("\n")` — which inserts one `\n` byte
*between* every pair of joined lines. That separator byte was never counted.

```rust
// Before (buggy):
while end < lines.len() && size + lines[end].len() < tuning.max_chunk_bytes {
    size += lines[end].len();
    end += 1;
}
```

**Concretely, with `max_chunk_bytes = 100` and 99 one-byte lines** (`"x"` × 99): the real joined
content is `99 + 98 = 197` bytes — but the buggy accumulator only ever reaches `size = 99` (never
counting the 98 separators), so the loop's `size + lines[end].len() < 100` condition stays true long
enough to pack **all 99 lines into a single chunk** — a chunk whose real length (197 bytes) is
**~2x the cap it was supposed to respect**. At the production default (16,000-byte cap, ≤100-line
windows) the overshoot is bounded to at most ~99 bytes — small in absolute terms, but a real
violation of the function's own stated guarantee, and provably worse for any caller that sets a
tighter cap.

**The fix:** fold the separator into the running total *before* deciding whether to extend the
window, so `size` always equals the real `join("\n")`'d length:

```rust
// After (fixed):
while end < lines.len() {
    // +1 accounts for the "\n" the join() below inserts between lines — omitting it
    // undercounts by one byte per line, so the emitted content can exceed the cap.
    let with_next = size + 1 + lines[end].len();
    if with_next > tuning.max_chunk_bytes {
        break;
    }
    size = with_next;
    end += 1;
}
```

**Why this form and not `size + lines[end].len() + 1 < tuning.max_chunk_bytes`:** computing
`with_next` first and comparing with `>` (equivalently `<=` to continue) makes the invariant
explicit and easy to verify by inspection: `size` after the loop is *exactly*
`lines[start..end].join("\n").len()`, not an approximation of it.

A regression test reproduces the bot's exact repro shape and would have failed against the old code:

```rust
#[test]
fn cap_accounts_for_join_separator_bytes_not_just_line_lengths() {
    let tuning = IndexTuning { max_chunk_bytes: 100, ..IndexTuning::default() };
    let chunk = Chunk {
        // ...
        content: vec!["x"; 99].join("\n"), // 99 + 98 separators = 197 bytes
    };
    let out = cap_chunk_bytes(chunk, tuning);
    assert!(
        out.iter().all(|c| c.content.len() <= tuning.max_chunk_bytes),
        "every split chunk must respect the cap once join() separators are counted"
    );
}
```

---

## Tests added (4 total)

| Test | What it proves |
|---|---|
| `chunk_under_the_cap_passes_through_unchanged` | No unnecessary fragmentation — a chunk already under the cap is untouched. |
| `oversized_windowed_chunk_from_long_unwrapped_lines_is_split_under_the_cap` | Mirrors the real `ROADMAP.md` failure: long unwrapped lines still produce output chunks all under the cap. |
| `a_single_line_longer_than_the_cap_still_terminates_as_its_own_chunk` | The pathological single-giant-line case terminates (no infinite loop) rather than being sliced mid-line. |
| `cap_accounts_for_join_separator_bytes_not_just_line_lengths` | Regression for the bot-found separator-byte bug — many short lines don't overshoot the cap. |

## Verification

```bash
cargo build --workspace       # clean
cargo test -p agent-runner --lib   # 85 passed, 0 failed (10 in indexer::)
cargo clippy -p agent-runner --lib -- -D warnings   # clean
cargo fmt -p agent-runner -- --check   # clean
```

## Links

- Issue: [#618](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/618)
- PR: [#619](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/pull/619)
- Prior, related fix: [#617](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/pull/617) —
  fixed the swallowed-error bug that made the `webank-models` failure's real cause (the Fireworks
  400 + prompt-too-long message) visible in the first place; without it, this investigation would
  only have had `"embedding batch 1"` to go on.
