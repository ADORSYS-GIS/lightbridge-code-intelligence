# OpenCode review — shadow parity gate (RFC-0009 slice 4)

The go/no-go gate before cutting the live review path to OpenCode (slice 5) is **measured findings
parity**: on the same PR, does the OpenCode review catch the issues the native review catches?

This can't be a hermetic unit test — it needs a real model (eaig), the control plane, and a real PR,
and model output isn't deterministic. So it's an **operator procedure** that produces two findings
sets and diffs them with `cargo xtask shadow diff`. The analysis half (the diff + verdict) is code and
unit-tested; the run half is here.

## What "parity" means

| bucket | meaning | gate |
|---|---|---|
| **matched** | both engines flagged the same issue (same file, line within `--line-tolerance`) | good |
| **only-native** | native caught it, **OpenCode missed it** | **BLOCKS cutover** — this is the regression |
| only-opencode | OpenCode flagged something native didn't | reported, human call (signal or noise) |
| severity-diverged | matched issue, different P0/P1/P2 | reported — calibration drift, judge case-by-case |

`xtask shadow diff` **exits non-zero if `only-native` is non-empty.**

## Procedure

Pick **several representative PRs** (a security-relevant one, a large-diff one, a docs-only one, a
fast-tier one). For each:

1. **Native run** — the current live path. Run the review (or read an already-posted one) and export
   its inline findings to `native.json` — either a bare array or `{ "findings": [ … ] }`, each with
   `file`/`path`, `line`, `priority`/`severity`, `title`. (Source: the control plane's buffered inline
   findings for the task, or the posted review comments.)

2. **OpenCode run** — drive `run_opencode_agent` against the **same** PR/checkout with the same
   `ReviewConfig` (same model, same tier). It buffers findings control-plane-side exactly like native
   (mediated `lightbridge_add_review_comment`). Export them to `opencode.json`.

   > The host isolates nothing about the model — point `LCI_EAIG_*` at the same gateway/model the
   > native run used, so a divergence is the *loop/harness*, not the model.

3. **Diff**:
   ```
   $ cargo xtask shadow diff --native native.json --opencode opencode.json
   ```
   Read the report. **`only-native` must be empty** for that PR to pass.

## Verdict

Cut over (slice 5) only when, across the representative PRs:

- `only-native` is **consistently empty** (no missed findings), and
- `severity-diverged` is understood (OpenCode isn't systematically down-grading real issues), and
- `only-opencode` is reviewed (new findings are signal, not noise).

A single PR where OpenCode misses a real native finding is a **no-go** — investigate (prompt? tool
budget? the loop stopping early?) before proceeding.

## Notes

- Model output varies run-to-run; run each PR a couple of times and treat a finding as "caught" if the
  engine finds it in *any* run (the native reviewer has the same variance — issue #420).
- `--line-tolerance` (default 3) absorbs the two engines anchoring the same issue a few lines apart.
- This procedure is the substitute for a real eval harness (#252), which doesn't exist yet.
