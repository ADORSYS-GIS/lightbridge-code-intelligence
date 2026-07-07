# ADR-0071: Multi-line (range) inline review comments

- **Status:** Proposed
- **Date:** 2026-07-07
- **Deciders:** @stephane-segning

## Context and Problem Statement

[ADR-0022](0022-review-writeback-control-plane.md) anchors every inline finding to exactly one
`(file, line)` pair: the mediated `add_review_comment` tool takes a single `line`
(`services/agent-runner/src/review/native/tools.rs`), the control plane's `commentable_lines` /
`validate` compute and check a single RIGHT-side line per file (`services/control-plane/src/review.rs`),
and `ReviewComment` posts `path` + `line` + `side: RIGHT` to GitHub's create-review endpoint
(`services/control-plane/src/integrations/github.rs`). ADR-0022 named this explicitly as a follow-up
gap: *"Multi-line suggestions (the current \`\`\`suggestion block replaces a single anchored line)."*

The gap shows up whenever a finding's evidence genuinely spans several lines — a multi-statement bug, a
loop whose problem is the whole body, a proposed replacement that adds or removes lines. Today the model
must either pick one representative line and describe the rest in prose, or emit a `suggestion` block
that visually "replaces" only the one anchored line while the body talks about a wider span — both read
as imprecise to the human reviewer.

GitHub's create-review API already supports this: each entry in `comments[]` may carry `start_line` +
`start_side` in addition to `line` + `side`, turning a single-line comment into a range comment
anchored from `start_line` to `line` (GitHub always treats `line` as the range's last line). No new
endpoint or GitHub App permission is needed — this is an additive field on the call ADR-0022 already
makes.

## Decision Drivers

- GitHub already supports ranged comments on the same endpoint ADR-0022 uses — no new integration
  surface, just an optional field.
- The overwhelming majority of findings are genuinely single-line; whatever we build must not add
  friction or risk to that path.
- Everything downstream of a posted comment — `retract_finding`, the feedback poller's reaction
  correlation (ADR-0035), re-review's prior-findings context (ADR-0040) — keys off `(file, line)`. That
  keying should not have to change for this to ship.
- GitHub itself constrains ranges to a single diff hunk (a range that crosses a hunk boundary, or whose
  start isn't on a commentable line, is rejected by GitHub, not silently accepted) — validation has to
  enforce that before we ever call the API, consistent with ADR-0022's "validate before posting so one
  bad line can't sink the whole review."

## Considered Options

- **A. Additive `start_line` on the existing tool/pipeline.** `add_review_comment` gains an optional
  `start_line`; when present and the range validates, post `start_line` + `start_side: RIGHT` alongside
  the existing `line` + `side: RIGHT`. Absent `start_line` → today's single-line path, byte-for-byte.
- **B. Range only inside the `suggestion` block.** Keep the model citing one anchor `line`; let
  `suggestion` describe a multi-line replacement by widening the rendered `\`\`\`suggestion` fence
  itself (using a fixed lookback from `line`), without touching the GitHub `comments[]` payload.
- **C. Re-key everything on `(file, start_line, end_line)`.** Change `Finding`/`InlineComment` identity
  repo-wide — `retract_finding`, dedup, the feedback poller's comment correlation, and the ADR-0040
  prior-review context all move from a single `line` to a range.

## Decision Outcome

Chosen option: **"A — additive `start_line`"**, because it is the smallest change that fixes the actual
gap (an inaccurate anchor, not just an inaccurate suggestion fence — Option B) without touching every
downstream consumer's identity model (Option C's blast radius). `line` keeps its existing meaning
everywhere else in the system: it is the range's *end*, so `retract_finding`, feedback-reaction
correlation, and prior-review context keep matching on it unchanged.

Shape of the change:

- **Tool schema** (`add_review_comment`): add optional `start_line` ("first line of the range, when this
  finding spans more than one line; omit for a single-line finding"). `line` remains required and is
  documented as the range's last line when `start_line` is given.
- **Validation** (`services/control-plane/src/review.rs`): extend `validate` so that when `start_line` is
  present, the finding anchors inline only if **every** line from `start_line` to `line` (inclusive) is
  in that file's `commentable` set — i.e. the whole range is added/context lines inside one hunk. GitHub
  itself rejects a range that isn't contiguous within one hunk, so this must be checked control-plane
  side before posting, the same "validate before posting" contract ADR-0022 established for single
  lines. If the range doesn't fully validate (crosses a hunk boundary, `start_line` isn't commentable, or
  `start_line > line`), **fall back to a single-line comment anchored at `line`** — never drop the
  finding outright, and never send GitHub a payload it would reject wholesale.
- **GitHub payload** (`ReviewComment` in `integrations/github.rs`): add `start_line: Option<u32>` and
  `start_side: Option<&'static str>`, both serialized only `if let Some`, so a single-line `ReviewComment`
  serializes to the exact same JSON as today (`serde(skip_serializing_if = "Option::is_none")`).
- **Suggestion blocks**: when a finding both anchors a range and carries a `suggestion`, the rendered
  \`\`\`suggestion\`\`\` fence now correctly replaces the whole `start_line..=line` span instead of just
  `line` — closing the exact gap ADR-0022's follow-up named.
- **Everything else is unchanged**: `retract_finding`, the feedback poller (ADR-0035), and the ADR-0040
  prior-review context all continue to identify a comment by its single `line` (the range's end) — no
  schema or keying change required there.

### Consequences

- Good, because a finding whose evidence spans several lines gets an accurate anchor and, when it
  proposes a fix, an accurate multi-line `suggestion` — closing the gap ADR-0022 flagged.
- Good, because the change is fully additive and backward compatible: `start_line` absent (the default,
  and the only path the current prompt/model produces until this ships) behaves identically to today,
  byte-for-byte on the wire.
- Good, because no downstream consumer's identity model changes — `retract_finding`, feedback
  correlation, and prior-review context keep keying on the end `line`.
- Bad, because validation gets strictly more work per finding (checking a contiguous run of commentable
  lines, not membership of one line) — bounded and cheap (the commentable set per file is already a
  `BTreeSet<u32>`, so a range check is a small number of set lookups), but it is more code to reason
  about and test.
- Neutral, because ranges that cross a hunk boundary or start on a non-commentable line silently degrade
  to a single-line comment at `line` rather than failing — consistent with ADR-0022's "never drop a
  finding outright" posture, but it means a model-cited range isn't always honored exactly as asked.
- Neutral, because this only extends the RIGHT (new-file) side, matching the existing ADR-0022
  limitation that comments on deleted (LEFT-side) lines aren't supported.

## Pros and Cons of the Options

### Option A — additive `start_line`

- Good, because it fixes the anchor, not just the suggestion fence.
- Good, because it's backward compatible and low blast radius.
- Bad, because it still needs range-validity checking (contiguous, single-hunk) that Option B avoids.

### Option B — range only inside `suggestion`

- Good, because it needs no new validation logic — the anchor stays a single already-validated line.
- Bad, because the *comment itself* (not just its suggestion) is still pinned to one line, so the visible
  finding still misrepresents a multi-line problem as single-line — doesn't actually close the ADR-0022
  gap, only half of it.

### Option C — re-key on a range everywhere

- Good, because it's the most "complete" model of a ranged finding.
- Bad, because it touches `retract_finding`, the feedback poller's reaction correlation (ADR-0035), and
  the ADR-0040 prior-review context — a much larger, riskier change for no behavior the reviewer-facing
  side actually needs (nothing downstream cares about the *start* of a range, only its anchor).

## More Information

- [ADR-0022](0022-review-writeback-control-plane.md) — establishes single-line inline comments and the
  "validate against the diff before posting" contract this ADR extends to ranges; names multi-line
  suggestions as an explicit follow-up.
- [ADR-0035](0035-review-feedback-signal.md) — feedback/reaction correlation, unaffected: keys on the
  posted comment's `line`.
- [ADR-0040](0040-re-review-reads-prior-findings.md) — prior-review context, unaffected: keys on
  `(file, line)`.
- [ADR-0043](0043-review-finding-verification.md) — evidence citation; a ranged finding's `evidence`
  should already name the span, which is what motivated giving the anchor itself the same precision.
- GitHub REST API — `POST /repos/{owner}/{repo}/pulls/{pull_number}/reviews`, `comments[].start_line` /
  `comments[].start_side` (the existing `comments[].line` / `comments[].side` fields ADR-0022 already
  posts remain required; `start_line`/`start_side` are the additive range fields, and GitHub requires the
  range to fall within a single diff hunk).
