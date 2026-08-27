# Chunker fix: a single line longer than the cap must be split, not emitted whole

## 1. The issue, restated

Indexing `ADORSYS-GIS/CoopData` fails outright, deterministically, on every push to its default
branch. From the task logs:

```
ERROR task failed  task_id=0786217c-3511-4da1-b6a7-79c7a688d8e5
error: "embedding batch 62: embeddings API returned 422 Unprocessable Entity:
{\"error\":{\"message\":\"Value error, The input sequence should have less than
131072 characters. Input length: 191026\", ...}}"
```

It failed the same way on a second attempt (`embedding batch 717`), with the exact same
`Input length: 191026` — not a fluke, the same chunk both times.

**The concrete example.** The repo contains `frontend/openapi.json`, a machine-generated OpenAPI
spec serialized as a single line:

```bash
$ awk 'length > 100000 {print FILENAME": line "FNR" length "length}' frontend/openapi.json
frontend/openapi.json: line 1 length 191026
```

That one line is 191,026 bytes. The chunker's configured cap (`INDEX_MAX_CHUNK_BYTES`, default
`16,000`) is supposed to guarantee no chunk ever gets close to that — and the embeddings model's
own hard ceiling is `131,072` characters. This one line blows through both.

### Why the existing cap doesn't catch it

`cap_chunk_bytes` in
[`services/agent-runner/src/indexer/chunker.rs`](services/agent-runner/src/indexer/chunker.rs)
splits an oversized chunk by grouping whole lines up to the byte cap:

```rust
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
            let with_next = size + 1 + lines[end].len();
            if with_next > tuning.max_chunk_bytes {
                break;
            }
            size = with_next;
            end += 1;
        }
        out.push(Chunk { /* ... */ content: lines[start..end].join("\n") });
        start = end;
    }
    out
}
```

**Worked example at a small scale** — cap = `10` bytes, content = one line of 100 `a`s:

| Step | `start` | `end` | `size` | What happens |
|---|---|---|---|---|
| 1 | `0` | `0 + 1 = 1` | `lines[0].len() = 100` | `size` (100) is already over the cap (10) before the inner loop even runs |
| 2 | inner loop checks `end < lines.len()` | — | — | `lines.len() == 1`, so the inner loop's condition is false immediately — it never runs at all |
| 3 | — | — | — | Loop exits with `end` still `1`; the whole 100-byte line is emitted as one chunk |

The comment on line 66 names this on purpose: *"Always take at least one line, so a single line
longer than the cap still terminates."* That's a real invariant (the loop must make progress and
must terminate), but it was never paired with actually shrinking that one line — so the function's
own documented contract (asserted by its existing tests: *"no output chunk may exceed
max_chunk_bytes"*) is silently broken for exactly this input shape. There's even a test that
currently locks in the broken behavior as correct:

```rust
#[test]
fn a_single_line_longer_than_the_cap_still_terminates_as_its_own_chunk() {
    // cap = 10
    let out = cap_chunk_bytes(chunk /* content: "a".repeat(100) */, tuning);
    assert_eq!(out.len(), 1, "a single oversized line is still one chunk, not split mid-line");
    assert_eq!(out[0].content.len(), 100);   // 100 > cap(10) — the bug, written down as a spec
}
```

`openapi.json`'s single 191,026-byte line is the same shape, just at production scale (cap
`16,000` instead of `10`). It sails through `chunk_file` → `index_checkout`'s embedding loop
([`indexer/mod.rs:117`](services/agent-runner/src/indexer/mod.rs:117)) untouched, and the whole
task fails when the embeddings API rejects it.

---

## 2. The fix, with an example

Detect the single-oversized-line case explicitly, and when it happens, split *that line* into
`max_chunk_bytes`-sized pieces on a valid UTF-8 char boundary, instead of emitting it whole. The
"walk back to a valid char boundary" idea already exists independently in ~9 other files in this
codebase (e.g.
[`review/transcript.rs:118-127`](services/agent-runner/src/review/transcript.rs:118)) for
single-point truncation; this reuses the same idea, repeated until the whole line is consumed:

```rust
/// Split one line into `<= max_bytes`-sized pieces, each cut on a valid UTF-8 char boundary.
fn split_line_by_bytes(line: &str, max_bytes: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = line;
    while rest.len() > max_bytes {
        let mut end = max_bytes;
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        pieces.push(&rest[..end]);
        rest = &rest[end..];
    }
    pieces.push(rest);
    pieces
}
```

`cap_chunk_bytes` branches on the single-line case before trying to merge lines together (the
merge loop can never itself produce an oversized window — see the walkthrough above — so this
branch is the only place that needs to change):

```rust
fn cap_chunk_bytes(chunk: Chunk, tuning: super::IndexTuning) -> Vec<Chunk> {
    if chunk.content.len() <= tuning.max_chunk_bytes {
        return vec![chunk];
    }
    let lines: Vec<&str> = chunk.content.lines().collect();
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < lines.len() {
        // A line longer than the cap on its own can't be merged with anything — split the line
        // itself instead of emitting it whole (a minified/generated single-line file otherwise
        // sails straight past an embedding model's own input limit, unsplit).
        if lines[start].len() > tuning.max_chunk_bytes {
            let line_no = chunk.start_line + start as i32;
            for piece in split_line_by_bytes(lines[start], tuning.max_chunk_bytes) {
                out.push(Chunk {
                    file_path: chunk.file_path.clone(),
                    language: chunk.language.clone(),
                    chunk_type: chunk.chunk_type.clone(),
                    symbol_name: chunk.symbol_name.clone(),
                    start_line: line_no,
                    end_line: line_no,
                    content: piece.to_string(),
                });
            }
            start += 1;
            continue;
        }

        let mut end = start + 1;
        let mut size = lines[start].len();
        while end < lines.len() {
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

No signature change, no new config, no `Chunk` field added. Every existing caller (`chunk_file`,
`index_checkout`, `index_graph`) is unaffected.

**The same worked example, after the fix** — cap = `10` bytes, content = one line of 100 `a`s:

| Step | What happens |
|---|---|
| 1 | `lines[0].len() = 100 > 10` → take the new branch instead of the merge loop |
| 2 | `split_line_by_bytes("a"*100, 10)` cuts off `10` bytes at a time (all single-byte ASCII here, so every cut lands on a char boundary immediately) |
| 3 | Result: `10` pieces, each exactly `"aaaaaaaaaa"` (10 bytes), `start_line == end_line == 0` for all of them |
| 4 | Every emitted chunk now satisfies `content.len() <= max_chunk_bytes` — the invariant holds |

**At production scale** — `openapi.json`'s 191,026-byte line, cap `16,000`:
`191026 / 16000 = 11.94` → **12 chunks**, the first 11 at exactly 16,000 bytes and the 12th
holding the 3,026-byte remainder. Each one is well under both `max_chunk_bytes` and the embedding
model's 131,072-character limit, so the `422` never happens.

**Multi-byte safety** — if the oversized line contains multi-byte UTF-8 (e.g. emoji or non-ASCII
text baked into a minified blob), `split_line_by_bytes` walks `end` backwards from the raw byte
cap until `is_char_boundary(end)` is true, so a cut is never made through the middle of a
character — the same guarantee `truncate_on_boundary` already gives call sites elsewhere in this
codebase, just applied repeatedly instead of once.

---

## 3. Test changes

**Update the test that currently encodes the bug** (its old assertion — one unsplit oversized
chunk — is exactly the behavior being fixed):

```rust
#[test]
fn a_single_line_longer_than_the_cap_is_split_into_capped_pieces() {
    let tuning = IndexTuning { max_chunk_bytes: 10, ..IndexTuning::default() };
    let chunk = Chunk {
        file_path: "f.txt".to_string(),
        language: "text".to_string(),
        chunk_type: "window".to_string(),
        symbol_name: None,
        start_line: 0,
        end_line: 0,
        content: "a".repeat(100),
    };
    let out = cap_chunk_bytes(chunk, tuning);
    assert!(out.len() > 1, "a 100-byte line at a 10-byte cap must split into multiple pieces");
    assert!(
        out.iter().all(|c| c.content.len() <= tuning.max_chunk_bytes),
        "no output chunk may exceed max_chunk_bytes"
    );
    assert_eq!(
        out.iter().map(|c| c.content.len()).sum::<usize>(),
        100,
        "the pieces must reconstruct the full line with nothing dropped"
    );
    assert!(out.iter().all(|c| c.start_line == 0 && c.end_line == 0));
}
```

**Add the production regression** — a single-line JSON file at realistic scale, the same shape as
`openapi.json`:

```rust
// Regression: ADORSYS-GIS/CoopData's frontend/openapi.json is one 191,026-byte line — a
// minified/generated file — which the embeddings API rejects outright past its own 131,072-char
// input limit. The splitter must never emit a chunk that large regardless of file shape.
#[test]
fn a_giant_single_line_json_file_is_split_under_the_embedding_models_limit() {
    let tuning = IndexTuning::default(); // max_chunk_bytes: 16_000
    let src = "x".repeat(191_026);
    let chunks = chunk_file("openapi.json", &src, "json", tuning);
    assert!(!chunks.is_empty());
    assert!(
        chunks.iter().all(|c| c.content.len() <= tuning.max_chunk_bytes),
        "no chunk may exceed max_chunk_bytes, even from a single giant line"
    );
}
```

**UTF-8 safety** — the one thing that can panic if the boundary walk is wrong:

```rust
#[test]
fn split_line_by_bytes_never_slices_through_a_multibyte_char() {
    let line = "€".repeat(20); // each € is 3 bytes — max_bytes not a multiple of 3
    let pieces = split_line_by_bytes(&line, 10);
    assert!(pieces.iter().all(|p| p.len() <= 10));
    assert_eq!(pieces.concat(), line, "no bytes lost or corrupted");
}
```

---

## 4. Explicitly out of scope

`frontend/playwright-report/index.html` — a second, separate oversized-single-line file found in
the same repo (133,539 bytes, an auto-generated Playwright test report) — is **not** addressed by
this fix. Whether generated test-report HTML belongs in the index at all is a policy question (an
addition to `collect_chunks`'s directory skip-list — today it skips `node_modules`, `dist`,
`build`, `.next`, `.venv`, `venv`, `__pycache__`, `.git`, but not report-output directories), not a
chunker correctness bug. Keeping it out of this PR keeps the fix minimal and independently
reviewable; worth its own follow-up if wanted.

---

## 5. Verification plan

```bash
cargo test -p agent-runner --lib chunker
cargo build --workspace
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
```
