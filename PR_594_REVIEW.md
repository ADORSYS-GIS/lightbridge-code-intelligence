# Review — PR #594, per-repo Grafana analytics embed (story #516)

| | |
|---|---|
| PR | [#594 — feat(web): embed per-repo cost/token Grafana panels on the detail page](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/pull/594) |
| Story | [#516 — Per-repo review analytics embedded via Grafana](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/516) (epic #493) |
| Author | @leghadjeu-christian · opened 2026‑08‑06 · 1 commit · +101 / −1 across 6 files |
| Verdict | **Approve with two changes required** — one silent-wrong-data risk, one CI gap. The design is sound and the honesty is exemplary. |

---

## The issue, in context

### The problem #516 exists to solve

Lightbridge already collects rich per-review telemetry — what each review cost, how many tokens it
burned, how many findings it produced — and it is already visualised, in three Grafana dashboards
that ship from this repo: `review-cost`, `review-quality`, `review-runs`.

But that data was reachable **only through standalone Grafana**. A repo owner looking at their
repo's page in `apps/web` had no way to see how their own repo's reviews were performing without
leaving the product, knowing Grafana exists, having an account on it, and then filtering by hand to
their repo. In practice that means nobody looked.

The story states it plainly:

> As a repo owner, I want per-repo review analytics (cost, quality, run history) embedded on the
> repo's detail page, so that I don't have to leave apps/web or hunt through standalone Grafana to
> see how my repo's reviews are performing.

### Why this is a small story, not a big one

Nothing needs building. The dashboards exist, the data pipeline exists, and — critically — the
**embedding mechanism already exists and is proven**. `apps/web/components/runs/run-logs-embed.tsx`
embeds a Grafana panel into the run-detail page today using Grafana's `d-solo` endpoint inside an
iframe, a pattern established by ADR-0102.

So the work is: point that same mechanism at dashboards that already exist, and scope them to one
repo via a Grafana template variable. The story is explicit that building anything new is **out of
scope**:

> Out of Scope: Building new dashboards — this story only scopes and embeds existing ones.

That framing is what makes the PR's central judgement call (§3) interesting: two of the three
dashboards turned out not to *be* repo-scopable without a query change, which would have crossed
that line.

### What "done" means

Two acceptance criteria:

1. > Given the repo detail page, when it loads, then it embeds `review-cost`/`review-quality`/
   > `review-runs` panels filtered to that repo via a Grafana template variable.
2. > Given `NEXT_PUBLIC_GRAFANA_URL` is unset, when the page loads, then it falls back gracefully
   > (same posture as the existing run-logs embed), not a broken iframe.

Plus a test expectation that turns out to be the crux of this review:

> Component test for the embed with/without `NEXT_PUBLIC_GRAFANA_URL` set; **manual verification
> that the repo-scoped filter actually filters (not just decorative)**.

That emphasis is the story author anticipating the exact failure mode of this kind of work: an
iframe that renders beautifully and shows numbers for *every* repo, which looks correct and is
wrong. §4 and §5 of this review are largely about that risk.

### Where it sits in the epic

#516 is one of six stories under epic [#493 — apps/web investment](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/493),
which reversed an earlier direction of retiring `apps/web` and instead invests in it. Its two stated
dependencies are now both satisfied:

| Dependency | Status |
|---|---|
| A per-repo detail page to mount on (#514) | ✅ closed — the page exists |
| "Benefits from #483 being fixed first so the page family isn't inheriting a known-broken panel" | ✅ #483's CRI-unwrap bug fixed and verified in prod on 2026‑08‑11 |

So nothing blocks this story on the epic side. The remaining obstacle is environmental, not
sequential — see §11.

---

## 1. Verdict up front

The code is clean, follows the established pattern closely, and the PR description is unusually
honest about what it did *not* do. Two things must be settled before merge:

| # | Issue | Severity |
|---|---|---|
| 1 | `$model` is never passed in the embed URL and has no `current` in the dashboard JSON. If Grafana does not default it to "All", the cost panel silently shows **$2.19 instead of $72.04** on a real repo — a 97% understatement with no error. | **Blocker until confirmed** |
| 2 | The CI jobs that gate the generated dashboards (`generate-and-verify`, `helm-lint`) were **cancelled**, not run. The dashboards are regenerated artifacts. | **Blocker** |
| 3 | AC 1 is partially met — 2 of 3 dashboards embedded — with a documented, correct reason. | Accept as-is |
| 4 | No component test, which the story explicitly asked for. | Discuss — see §6 |
| 5 | Panel title mismatch: the card says "(30d)", the panel inside the iframe says "(range)". | Nit |

I verified the parts the author could not, against production. Results in §4.

---

## 2. What the PR does

Four coordinated changes.

**A new embed component**, `apps/web/components/repos/repo-analytics-embed.tsx`, mirroring the
existing `run-logs-embed.tsx` with `var-repo` instead of `var-task_id`:

```tsx
export function RepoAnalyticsEmbed({ repo, panel }: { repo: string; panel: RepoAnalyticsPanel }) {
  const base = process.env.NEXT_PUBLIC_GRAFANA_URL?.trim().replace(/\/+$/, "");
  if (!base) {
    return (
      <StatusLine>
        Set <code className="font-mono">NEXT_PUBLIC_GRAFANA_URL</code> to embed {panel.title} from
        Grafana.
      </StatusLine>
    );
  }

  const src =
    `${base}/d-solo/${panel.dashboardUid}/${panel.dashboardSlug}` +
    `?orgId=1&panelId=${panel.panelId}` +
    `&var-repo=${encodeURIComponent(repo)}&theme=${GRAFANA_THEME}&kiosk`;

  return <iframe src={src} title={panel.title} /* … */ />;
}
```

**A declarative panel list**, which is a better structure than `run-logs-embed.tsx`'s two loose
constants — it makes the page a simple `.map()`:

```tsx
export const REPO_ANALYTICS_PANELS: RepoAnalyticsPanel[] = [
  { dashboardUid: "lci-review-cost",    dashboardSlug: "review-cost",    panelId: 100, title: "Billed cost (30d)" },
  { dashboardUid: "lci-review-quality", dashboardSlug: "review-quality", panelId: 100, title: "Tokens used (30d)" },
];
```

**Mounting on the repo detail page** (`app/dashboard/repositories/[id]/page.tsx`):

```tsx
<Card>
  <CardHeader><CardTitle>Review analytics</CardTitle></CardHeader>
  <CardBody className="flex flex-col gap-3 sm:flex-row">
    {REPO_ANALYTICS_PANELS.map((panel) => (
      <div key={panel.dashboardUid} className="flex-1">
        <RepoAnalyticsEmbed repo={repoSlug(repo)} panel={panel} />
      </div>
    ))}
  </CardBody>
</Card>
```

**Pinned panel ids** in the two generators, so the embed URL survives panel reordering — the same
convention `task_runs.py` established:

```python
tokens_total = (
    stat.Panel()
    # explicit id pinned so the apps/web d-solo embed URL is deterministic
    # (repo-analytics-embed.tsx), mirroring task_runs.py's run_logs panel. …
    .id(100)
    .title("Tokens (range)")
    …
)
```

---

## 3. Does it satisfy story #516?

### AC 2 — graceful fallback: **met**

> Given `NEXT_PUBLIC_GRAFANA_URL` is unset, when the page loads, then it falls back gracefully
> (same posture as the existing run-logs embed), not a broken iframe.

The early return above does exactly this. No code path can render an iframe with an empty base.

In production the variable *is* set — baked into the bundle at `next build` time, see §11 — so the
fallback is not what users will see. It still earns its place: it is what makes local dev and CI
render a note rather than a dead iframe, which is exactly the posture the AC asks for.

### AC 1 — three dashboards embedded: **partially met, correctly**

> …embeds `review-cost`/`review-quality`/`review-runs` panels filtered to that repo via a Grafana
> template variable.

The PR embeds two panels, not three, and says so plainly rather than quietly narrowing scope. The
reason is sound and I confirmed it in the source:

- `review-cost` "Billed cost (range)" — Loki-sourced, genuinely `$repo`-filtered. ✅
- `review-quality` "Tokens (range)" — same. ✅
- `review-quality`'s **findings/reactions panels** — Postgres-sourced and *not* `$repo`-scoped.
- `review-runs` — has **no `$repo` variable at all**.

Embedding either of the latter would have shown every repo's data on one repo's page. Declining to
guess at a query change is the right call, and it is what this repo's conventions ask for. The PR
puts the trade-off in "Reviewer Focus" rather than hiding it.

**Recommendation:** accept the narrower scope, and file a follow-up for `review-runs` +
`review-quality`'s findings panels rather than leaving #516 half-closed by implication.

---

## 4. Verification I ran that the PR could not

The PR states plainly: *"Nothing was exercised against a running Grafana/control-plane in this
environment."* The story's test expectation is *"manual verification that the repo-scoped filter
actually filters (not just decorative)."* I ran that against the production Loki.

### The filter genuinely filters ✅

Billed cost over 30d, using the **embedded panel's own query** read from the generated JSON:

```text
$repo = .+  (All)                  -> $326.63
$repo = ADORSYS-GIS/ai-helm        ->  $72.04
$repo = ADORSYS-GIS/ai-helm-values ->  $11.44
$repo = vymalo/vsms                ->   $8.65
```

Distinct per repo. Not decorative.

### The label values line up ✅

`repoSlug()` returns `` `${repo.owner}/${repo.name}` `` (`apps/web/lib/domain/repos.ts:29`), and the
real `account_id` values in the gateway stream are exactly that shape:

```text
'ADORSYS-GIS/ai-helm': 760 lines
'vymalo/vsms': 356
'ADORSYS-GIS/lightbridge-code-intelligence': 293
```

### The regex is anchored — no sibling-repo leakage ✅

`account_id=~"$repo"` is a *pipeline* label filter, not a stream selector, so anchoring was worth
confirming: an unanchored `ADORSYS-GIS/ai-helm` would silently absorb `ADORSYS-GIS/ai-helm-values`.

```text
exact   = "ADORSYS-GIS/ai-helm" -> 72.0373
regex  =~ "ADORSYS-GIS/ai-helm" -> 72.0373
=> ANCHORED (safe)
```

### Panel ids do not collide ✅

```text
review-cost    panel ids: [100, None ×8]
review-quality panel ids: [None ×3, 100, None ×7]
```

100 is unique on both; every other panel stays id-less, so Grafana's auto-assignment (which starts
at 1) cannot reach it.

### It still regenerates cleanly on today's `main` ✅

The branch is based on `e90806b` (Aug 6), before #607 merged. I rebased it in a scratch worktree and
re-ran the generator:

```text
$ git rebase origin/main            -> Successfully rebased
$ python generate.py
$ git status --porcelain -- deploy/observability/dashboards
<<< empty >>>                        # CI's generate-and-verify gate would pass
```

No conflict with #607: that PR touched `common.CRI_UNWRAP`, which only `task_runs.py` and
`prompt_budget.py` import. These two dashboards read the OTLP gateway stream, which was never
CRI-wrapped.

---

## 5. Issue 1 — `$model` is unbound in the embed (**must confirm before merge**)

This is the one finding that could put wrong numbers in front of a user with no visible error.

Both embedded panels filter on **two** variables:

```logql
sum(sum_over_time({service_name="envoy-ai-gateway"} | json
  | oidc_jti=~"runid:.+"
  | account_id=~"$repo"
  | gen_ai_request_model=~"$model"      ← never set by the embed
  | unwrap gen_ai_usage_custom_total_cost | __error__="" [$__range])) / 1e6
```

The embed URL passes `var-repo` only. And the generated dashboard JSON carries **no `current`** for
either variable:

```text
review-cost  var 'repo':  current=None
review-cost  var 'model': current=None
```

`model_var` is declared with `include_all(True).all_value(".+")`, so *if* Grafana selects "All" when
no `current` is saved and no URL value is given, `$model` becomes `.+` and everything is correct.
That is the likely behaviour — but it is unverified, and the cost of being wrong is not small.

Measured blast radius on a real repo:

```text
ADORSYS-GIS/ai-helm, 30d
  $model = .+                       -> $72.04     (correct)
  $model = deepseek-v4-flash-0731   ->  $2.19     (first model alphabetically)
```

A **97% understatement**, rendered as a normal-looking stat panel. Two models serve this repo's
traffic (`deepseek-v4-flash-0731`, `qwen3-embedding-8b`), so any single-model default is badly wrong.

**Fix — belt and braces, one line:**

```tsx
const src =
  `${base}/d-solo/${panel.dashboardUid}/${panel.dashboardSlug}` +
  `?orgId=1&panelId=${panel.panelId}` +
  `&var-repo=${encodeURIComponent(repo)}&var-model=${encodeURIComponent(".+")}` +
  `&theme=${GRAFANA_THEME}&kiosk`;
```

Pinning `var-model=.+` explicitly makes the embed independent of Grafana's default-selection
behaviour *and* of anyone later saving a different model selection onto the dashboard. Given the
panel is embedded outside its dashboard's UI — where nobody can see or correct the variable — every
variable the query reads should be pinned in the URL.

The same argument applies to the time range: the embed relies on the dashboard's saved
`now-30d`/`now`. That is what makes the "(30d)" labels true today, but a future edit to the
dashboard's saved range would silently change what the card claims. Consider `&from=now-30d&to=now`.

---

## 6. Issue 2 — CI never actually ran (**blocker**)

```text
generate-and-verify      cancelled
helm-lint                cancelled
pipeline / web-build     cancelled
pipeline / rust-build    cancelled
governance-check         cancelled
quality                  success
Quality Gate             success
Lightbridge Review       success
```

`mergeable_state: unstable`. The two jobs that specifically gate this change — the dashboard
regeneration diff and the Helm render — **never completed**. Since `deploy/observability/dashboards/*.json`
are generated artifacts, an un-run `generate-and-verify` means the committed JSON is unverified by CI.

I reproduced both locally after rebasing (§4) and they pass, but that is not a substitute. **Push the
rebase and let CI run green before merging.** The rebase is needed anyway — the branch is five days
and several merges behind `main`.

---

## 7. Issue 3 — missing component test

The story asks for:

> Component test for the embed with/without `NEXT_PUBLIC_GRAFANA_URL` set.

The PR has none, and its Verification section leaves every box unticked except "Checking logs".

In fairness to the author: **`apps/web` has no test harness at all.**

```text
$ ls apps/web/vitest.config.* apps/web/jest.config.*  -> no matches
$ grep '"test"' apps/web/package.json                 -> no test script
$ find apps/web -name "*.test.tsx"                    -> nothing
```

The existing `run-logs-embed.tsx` — the pattern this PR mirrors — is also untested. So the ask is
not "add a test alongside the others", it is "stand up component testing for `apps/web`", which is
a materially larger piece of work and arguably its own story.

**Recommendation:** don't block on this. Either amend #516's test expectation to match reality, or
file a separate story for the `apps/web` test harness. What should *not* happen is closing #516 with
its stated test expectation silently unmet — that is the kind of quiet gap this repo's governance
exists to prevent.

---

## 8. Nits

**Title mismatch.** The card labels say `"Billed cost (30d)"` / `"Tokens used (30d)"`, but the
panels rendered inside the iframes are titled `"Billed cost (range)"` / `"Tokens (range)"`. With
`kiosk` + `d-solo`, the panel's own title is visible, so the user sees "(range)" — which is
meaningless outside the dashboard where a range picker exists. Either rename the panels, or keep the
component titles purely for the `title` attribute and hide the panel chrome.

**`dashboardSlug` is decorative.** Grafana ignores the slug segment in `/d-solo/<uid>/<slug>` and
resolves by UID. Harmless and consistent with `run-logs-embed.tsx`, but it is a third string to keep
in sync for no functional gain.

**Duplicated fallback copy.** `RepoAnalyticsEmbed` and `RunLogsEmbed` now carry near-identical
"set `NEXT_PUBLIC_GRAFANA_URL`" branches. Two instances is not yet a problem; a third would be the
moment to extract a shared `<GrafanaEmbed>`.

---

## 9. What is good here

Worth saying explicitly, because it is the harder half of the job:

- **The scope reduction is documented, not silent.** The PR says which panel it dropped and why,
  and puts the decision in "Reviewer Focus" for a human to overrule.
- **It refuses to guess.** Rather than invent a `$repo` `WHERE` clause for `review-runs` it could
  not verify, it left it out and explained. That is the correct instinct.
- **The AI Usage Declaration is honest** — accountability boxes deliberately unticked, and
  "Screenshot: not captured" stated rather than glossed.
- **It follows the existing pattern closely** and improves on it slightly with the declarative
  panel list.

---

## 10. Requested changes

1. **Pin `var-model=.+`** in the embed URL (§5). Or confirm by click-through that Grafana defaults
   `$model` to All, and record that in the PR. Consider pinning `from`/`to` too.
2. **Rebase onto `main` and get CI green** — `generate-and-verify` and `helm-lint` must actually run
   (§6).
3. **Click-through** — doable now, nothing blocks it (§11): open two different repos' detail pages
   and confirm the numbers differ and match Grafana with `$model` set to All. Attach a screenshot —
   the PR itself asks for this. This also settles item 1.
4. **File the follow-up** for `review-runs` + `review-quality` findings panels, so #516's dropped
   third of AC 1 is tracked rather than lost.
5. **Decide on the test expectation** (§7) — amend the story or file the harness work.

Items 1 and 2 are blocking. 3–5 can be commitments rather than code in this PR.

---

## 11. The embed is live in production — retraction

An earlier draft of this review claimed `NEXT_PUBLIC_GRAFANA_URL` was unset and that no Grafana
embed rendered in production. **That was wrong.** The variable is set in the CI build rather than
the Deployment (`.github/workflows/image-pipeline.yml:107`):

```yaml
- name: turbo build (remote cache)
  env:
    # NEXT_PUBLIC_* vars are inlined into the client bundle at `next build` time (ADR-0102) —
    # setting this at deploy/runtime instead would have no effect …
    NEXT_PUBLIC_GRAFANA_URL: https://grafana.ai.camer.digital
  run: pnpm turbo run build --filter=@lightbridge/web
```

`next build` inlines `NEXT_PUBLIC_*` into the client bundle, so the URL is a literal string in the
compiled JS and nothing reads it from the environment at runtime. Its absence from the Deployment
env is the expected state. The error came from checking the runtime env — the one place such a
variable provably cannot appear — rather than the build pipeline.

**What this changes for this PR, in its favour:**

- The existing run-logs embed already renders for users today (iframe → `grafana.ai.camer.digital`
  → Keycloak SSO if the browser has no session → panel).
- These new panels will render as soon as this merges and the image is rebuilt. There is no
  environmental blocker.
- **Requested change #3 is therefore doable immediately**, not deferred — and it is the cheapest way
  to settle the `$model` question in §5, since the rendered panel's own value can be compared
  against Grafana with `$model` explicitly set to All.

---

*Reviewed by Claude (Claude Code). Live figures measured against the production Loki via a
read-only `kubectl` port-forward on 2026‑08‑11; the rebase and regeneration were run in a scratch
worktree. The `$model` default-selection behaviour in §5 is the one claim I could not verify — it is
stated as a question for the click-through, not a defect.*
