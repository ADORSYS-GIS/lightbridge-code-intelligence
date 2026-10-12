# 15. How a review comment gets its line — from the model's tool call to the forge's API

File [`12`](12-opencode-integration.md) covers OpenCode as a runtime, and the presentation's "What
it's actually fed" slide covers the two prompt channels. This file answers a narrower, more
mechanical question: when the model decides "this line is wrong," what actually carries that
`(file, line)` pair from the model's tool call all the way to a real inline comment on a GitHub
review or a GitLab merge-request discussion — and where, if anywhere, is it told how to pick that
line in the first place?

Short answer: the model's line is **never trusted as-is**. It crosses a mediated write boundary
immediately, sits unvalidated in a buffer until the run finishes, and is only cross-checked against
the *real, independently-fetched* unified diff at finalize time — the control plane re-derives
which lines are commentable, not the model.

## 15.1 Step 1 — the model is told how to pick a line, twice

There are two independent places the model is told the rule, not one:

**In the system prompt** — `TOOL_PROTOCOL`, the code-owned text appended after the operator's own
persona (already quoted in the presentation; verbatim here for this file's purposes):

```rust
// services/review-agent/src/prompt.rs:47-62
const TOOL_PROTOCOL: &str = "\
# How to act\n\
Investigate with the search/graph tools before making any claim — never speculate about code you \
have not looked up. ... As \
you find issues, record each one with `add_review_comment` (one call per finding, on a line this diff \
adds or changes). Only set `start_line` when the finding's evidence genuinely spans multiple contiguous \
lines — a multi-line problem, not a multi-line explanation of a one-line problem ...";
```

**In the tool's own JSON-schema parameter descriptions** — which a function-calling model reads as
part of its instructions just as much as the system prompt, and which are considerably more
precise than the prose above:

```rust
// services/review-agent/src/tools/record.rs:64 (the add_review_comment ToolSpec, formatted here)
"line": {
  "type": "integer",
  "description": "The line this finding anchors to — a line the diff adds or changes. \
                   When `start_line` is also given, this is the LAST line of the multi-line range."
},
"start_line": {
  "type": "integer",
  "description": "Optional. The FIRST line of a multi-line range; the range ends at `line`. \
                   Omit for a single-line finding. Leave unset unless the finding's evidence \
                   genuinely spans multiple contiguous lines."
},
```

So the "what line" contract is **code-owned, not operator-owned** — it ships identically on every
deployment regardless of what an operator puts in `config.reviewSystemPrompt` (ai-helm-values). The
operator's prompt supplies persona and focus; the line-number contract is a fixed part of the tool
surface itself.

## 15.2 Step 2 — the tool call crosses the mediated boundary

The model never touches GitHub, GitLab, or even the control plane's real database directly (file
[`06`](06-the-review-loop.md)/[`12`](12-opencode-integration.md)'s mediated-tools framing). Calling
`add_review_comment` runs this handler inside the Job:

```rust
// services/review-agent/src/tools/record.rs:145-159 (trimmed)
match self.services.client.add_review_comment(
    cx.task_id, &args.file, args.line, args.start_line,
    Some(&args.title), Some(&args.priority), Some(&args.category),
    args.suggestion.as_deref(), &body,
).await {
    Ok(()) => ToolOutcome::Continue(format!("recorded finding at {}:{}", args.file, args.line)),
    ...
}
```

`self.services.client` is `ControlPlaneClient`, which turns that into one POST:

```rust
// services/agent-clients/src/control_plane/review.rs:38 (the request body)
"file": file, "line": line, "start_line": start_line, "title": title, ...
```

`POST /internal/tasks/{id}/review/inline` — the **internal** API (bearer-token auth, not OIDC; file
[`01`](01-control-plane-roles.md)'s trust-boundary framing), confirmed directly in the real router:

```rust
// services/control-plane/src/main.rs:464-467
// ADR-0037 mediated write actions: the native agent buffers findings/replies/summary, then
// flushes them as one grouped review on finalize.
.route("/internal/tasks/{id}/review/inline", post(internal::add_review_comment))
```

## 15.3 Step 3 — buffered, not yet trusted

The control plane does **not** validate the line here. It just appends a row:

```rust
// services/control-plane/src/db/pending_actions.rs:13-23
/// One buffered inline finding (the `add_review_comment` payload), read back at flush time.
#[derive(Debug, sqlx::FromRow)]
pub struct PendingInline {
    pub file: String,
    pub line: i32,
    pub title: Option<String>,
    pub priority: Option<String>,
    pub category: Option<String>,
    pub suggestion: Option<String>,
    pub body: String,
}
```

Worth noticing what's **missing** from that struct: `start_line`. The tool accepts it, the system
prompt and tool schema both explain it, `ControlPlaneClient::add_review_comment` sends it over the
wire — and the receiving buffer silently has nowhere to put it. Confirmed at the one place it
matters, finalize time:

```rust
// services/control-plane/src/http/internal.rs:1670-1680 (trimmed)
let findings: Vec<crate::review::Finding> = pending.inline.iter().map(|pi| crate::review::Finding {
    file: pi.file.clone(),
    line: pi.line.max(0) as u32,
    // The buffered `add_review_comment` tool call (`InlineActionBody`) doesn't yet carry a
    // `start_line` — extending that mediated-tool contract is ADR-0071's companion ticket (#287).
    start_line: None,
    ...
}).collect();
```

So today, every finding posts as a single-line comment regardless of what the model actually set —
not a silent bug exactly (the comment above names the gap and its tracking ticket explicitly), but
a real, currently-true gap between what the tool *advertises* and what the pipeline *delivers*. The
rest of the range machinery below (§15.4) is fully built and tested; this one connection is the
missing link.

## 15.4 Step 4 — validated against the *real* diff at finalize

This is the actual answer to "how does it know the line is legitimate": not by trusting the model,
but by **re-deriving the diff independently** and checking membership. `finalize_review` fetches the
PR's real diff itself (the same one the review Job read, fetched fresh with a freshly-minted
installation token — file [`01`](01-control-plane-roles.md)'s trust boundary again) and runs every
buffered finding through a validator:

```rust
// services/control-plane/src/review.rs:656-696 (trimmed — the core anchor-resolution logic)
let anchor = if added.is_some_and(|s| s.contains(&finding.line)) {
    // Directly on an added line — always valid for both GitHub and GitLab.
    Some(finding.line)
} else if all.is_some_and(|s| s.contains(&finding.line)) {
    // Context line: valid for GitHub, rejected by GitLab. Clamp to nearest added line.
    added.and_then(|s| nearest_added_line(finding.line, s, CLAMP_MAX_DELTA))
} else {
    None // line not visible in the diff at all — defer
};
```

Three real outcomes, each independently tested:

| The model's line is... | Result |
|---|---|
| An **added** line (the diff actually changed it) | Posted inline, exactly where the model put it |
| A **context** line (visible in the diff, unchanged) | Clamped to the nearest added line — GitHub allows context-line comments, GitLab's API 400s on them, so the control plane quietly normalizes to the lowest common denominator |
| **Not in the diff at all** (a line the model never actually saw change) | **Deferred** — folded into the review's summary body as prose, never posted as a dangling inline comment |
| On a **file the PR doesn't touch** | Dropped outright (`out_of_scope`) |

A clamped finding also loses its `suggestion` and any range, since a suggested replacement computed
for line 42 is meaningless pasted onto line 40 — the code enforces that by constructing a fresh
`Finding` with both cleared rather than trusting the caller to remember.

`start_line` (when it eventually reaches `Finding`, §15.3) goes through its own validator:

```rust
// services/control-plane/src/review.rs:702-719 (trimmed)
/// - `start_line` is present, and
/// - `start_line <= line` (GitHub always treats `line` as the range's *last* line), and
/// - every line from `start_line` to `line` (inclusive) is in the file's `commentable` set.
fn validated_range_start(finding: &Finding, file_lines: Option<&BTreeSet<u32>>) -> Option<u32> {
    let start = finding.start_line?;
    if start == 0 || start > finding.line { return None; }
    ...
}
```

— which is also how a range can never cross a diff hunk boundary: hunks are strictly increasing and
non-overlapping, so a *contiguous* run of commentable line numbers can only ever come from one hunk.

## 15.5 Step 5 — what the outbox actually stores

The validated list — now `InlineComment { path, line, start_line, body }`, the platform-neutral
shape — is serialized straight into the `outbox` row's JSON `payload` column at enqueue time (file
[`02`](02-postgres-schema-and-queue.md)'s outbox pattern). This is the literal answer to "how does
the outbox know what line to include": it isn't computed at delivery time at all — it was already
fully resolved and validated back at finalize, and the outbox row is just that decision, frozen as
JSON, waiting for the reconciler.

## 15.6 Step 6 — the reconciler speaks each platform's dialect

The reconciler (file [`01`](01-control-plane-roles.md) — the sole forge-write role) drains the row,
deserializes the payload, and rebuilds the same platform-neutral shape:

```rust
// services/control-plane/src/queue/reconciler.rs:480-489 (trimmed)
let comments: Vec<crate::integrations::platform::InlineComment> = p.comments.iter().map(|c| {
    crate::integrations::platform::InlineComment {
        path: c.path.clone(), line: c.line, side: "RIGHT",
        start_line: c.start_line, start_side: c.start_line.map(|_| "RIGHT"),
        body: c.body.clone(),
    }
}).collect();
```

Then calls `platform.post_review(...)` — and this is where the one platform-neutral shape forks into
two genuinely different wire formats, because GitHub and GitLab don't agree on how a line comment is
addressed at all:

**GitHub** — the struct is a byte-for-byte mirror of GitHub's own review-comment JSON:

```rust
// services/control-plane/src/integrations/github.rs:849-864
pub struct ReviewComment {
    pub path: String,
    pub line: u32,        // GitHub treats this as the range's LAST line when start_line is set
    pub side: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_side: Option<&'static str>,
    pub body: String,
}
```

(`start_line`/`start_side` are *omitted*, not sent as `null`, on a single-line comment — tested
explicitly as `review_comment_without_start_line_serializes_unchanged`, so adding range support
could never change the wire shape of an existing single-line comment.)

**GitLab** — no `line`/`side` at all. A GitLab inline comment is a *discussion* anchored to a
`position` object that names the diff itself, not just a line in it:

```rust
// services/control-plane/src/integrations/gitlab.rs:744-756
let refs = self.fetch_mr_diff_refs(&project, mr_iid).await?;   // base_sha, head_sha, start_sha
let position = serde_json::json!({
    "base_sha": refs.base_sha, "head_sha": refs.head_sha, "start_sha": refs.start_sha,
    "position_type": "text",
    "new_path": c.path, "old_path": c.path,
    "new_line": c.line,
});
```

Same validated `(path, line)`, two structurally different API calls — exactly what the
`CodePlatform` trait (file [`01`](01-control-plane-roles.md) §1.5, ADR-0072) exists to absorb, so
every upstream step (the tool, the buffer, the validator, the outbox payload) stays platform-neutral
and only the reconciler's last mile knows the difference.

## 15.7 The whole path, end to end

```
model calls add_review_comment{file, line, start_line?}
  │  (told the rule by TOOL_PROTOCOL *and* the tool's own JSON-schema descriptions — §15.1)
  ▼
review-agent/src/tools/record.rs → ControlPlaneClient → POST /internal/tasks/{id}/review/inline
  ▼
pending_review_actions (buffered, UNvalidated — start_line is dropped here today, §15.3)
  ▼
finalize_review: fetch the PR's real diff fresh, validate every (file, line) against it (§15.4)
  │
  ├─ added line           → InlineComment, line unchanged
  ├─ context line         → InlineComment, clamped to nearest added line
  ├─ not in the diff      → deferred into the review's summary body (no dangling inline comment)
  └─ file not in the PR   → dropped (out_of_scope)
  ▼
outbox.payload (JSON) — the validated InlineComment list, frozen, waiting for delivery (§15.5)
  ▼
reconciler drains the row → platform.post_review(...)
  │
  ├─ GitHub: ReviewComment{path, line, side, start_line?, start_side?, body}
  └─ GitLab: a discussion with a position{base_sha, head_sha, start_sha, new_path, new_line}
```

## 15.8 What to read next

- File [`02`](02-postgres-schema-and-queue.md) for the `outbox` table/pattern this file's §15.5–15.6
  builds on, and the reconciler's "sole writer" role.
- File [`06`](06-the-review-loop.md) for the broader mediated-write framing (`add_review_comment` is
  one of several mediated tools; this file is a deep, single-tool trace through it).
- File [`12`](12-opencode-integration.md) for how the tool call in §15.2 reaches OpenCode's actual
  process boundary in the first place (the ACP transport), and the presentation's "How it's actually
  spawned"/"What it's actually fed" slides for the prompt-assembly side of §15.1.
- [`docs/adr/0071-multi-line-review-comments.md`](../docs/adr/0071-multi-line-review-comments.md)
  (the range-comment design §15.4's `validated_range_start` implements) and the tracking ticket
  referenced at `internal.rs:1678` (#287) for the `start_line` gap named in §15.3 — the one place
  this file found the wiring genuinely incomplete, not just undocumented.
