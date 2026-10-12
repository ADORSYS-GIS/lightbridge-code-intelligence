# The two PRs, change by change

Every change in both PRs, what it does, and whether it is strictly necessary or a defensive choice.

| PR | Repo | Scope |
|---|---|---|
| [#607](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/pull/607) | lightbridge-code-intelligence | Makes the panel work. 3 commits. |
| [#222](https://github.com/ADORSYS-GIS/ai-helm-values/pull/222) | ai-helm-values | Fixes the underlying cause. 1 line. **Draft.** |

**Merge order: #607 → #222.** Never the reverse. §5 explains what breaks.

---

## 0. The one-paragraph problem

The kubelet writes pod logs in CRI format — `<ts> <stdout|stderr> <F|P> <line>` — and Alloy tails
those files verbatim with no `stage.cri`, so the envelope is stored in Loki as part of every line.
Any LogQL `| json` therefore dies with `JSONParserErr`. That is why the `task-runs` panel could
never show the control plane's `task_id`-tagged lines, and why it showed only the agent pod's half
of a task. #607 works around it in the query; #222 removes it at the source.

---

## 1. PR #607, commit 1 — `fix(dashboards): add control-plane logs to the task-runs panel`

```
deploy/observability/dashboards/task-runs.json  |  9 ++++++
tools/dashboard-gen/lci_dashboards/task_runs.py | 43 +++++++++++++++++++++----
```

This is the fix for #517. Everything else in both PRs exists to support or protect it.

### 1.1 The panel gains a second target

```python
run_logs = (
    logs.Panel()
    .id(100)
    .title("Logs for $task_id")
    .datasource(LOKI)
    .with_target(logql('{namespace="lightbridge-agents", pod=~"lightbridge-agent-${task_id}-.*"}'))
    .with_target(logql(_CONTROL_PLANE_LOGS, ref_id="B"))   # ← added
    .grid_pos(layout.place(24, 11))
)
```

**Necessary.** Target A (the agent pod) is untouched and byte-identical. A task's lifecycle spans
two pod families, and only one was represented. Adding a target rather than replacing one means the
panel cannot regress below what it already shows.

`.id(100)` stays pinned because `apps/web` builds its `d-solo` embed URL from it
(`run-logs-embed.tsx`); changing it would silently break the iframe.

### 1.2 The stream selector

```python
_CONTROL_PLANE_SELECTOR = '{namespace="converse", service_name="lightbridge-ci", container="main"}'
```

| Part | Necessary? | Why |
|---|---|---|
| `namespace="converse"` | **Yes** | Where the long-lived control-plane pods run. |
| `service_name="lightbridge-ci"` | **Yes** | Alloy maps `app.kubernetes.io/name` → `service_name`. This is the only label that spans all the pods a task touches. |
| `container="main"` | **No — defensive** | All eleven `lightbridge-ci-*` pods have exactly one container, named `main`. Insurance against a future sidecar injecting logs into the same stream. Droppable. |

**Why `service_name` and not `pod=~"lightbridge-ci-control-plane-.*"`?** Because a task's lifecycle
is genuinely spread across pods. Measured on one real task:

```text
pods surfaced: {'lightbridge-ci-control-plane-…-n9mbw': 102,
                'lightbridge-ci-dispatcher-…-8sxrv':      1}
emitting targets: {'control_plane::http::internal':   101,
                   'control_plane::queue::dispatcher':  1,
                   'control_plane::http::webhook':      1}
```

Narrowing to the control-plane pod would silently drop the dispatch record — the single line that
says the task was handed to Kubernetes.

### 1.3 The query, stage by stage

```python
_CONTROL_PLANE_LOGS = (
    f"{_CONTROL_PLANE_SELECTOR} "
    '|= "${task_id}" '
    f"{CRI_UNWRAP} "                    # | regexp … | line_format …
    '| json | __error__="" '
    '| fields_task_id != "" '
    '| fields_task_id = "${task_id}"'
)
```

#### `|= "${task_id}"` — **necessary**, and doing three jobs

1. Scopes the panel to one task.
2. Runs **before any parser**, so it is the cheap narrowing pass over a stream that carries ~11,000
   control-plane lines per day.
3. It is the safety guard for `service_name="lightbridge-ci"` including `lightbridge-ci-neo4j`,
   whose logs are not JSON. Those lines never reach `| json` because they do not contain the id.

Order matters here. Put the parser first and you pay to parse every line, and neo4j's lines produce
errors.

#### `| regexp … | line_format …` — **necessary**. This is the fix.

```logql
| regexp `^(?:\S+ std(?:out|err) [FP] )?(?P<content>.*)$`
| line_format `{{.content}}`
```

Strips the CRI envelope so the following `| json` sees clean input. Without it, every line errors:

```text
$ query '{namespace="converse", service_name="lightbridge-ci", container="main"} | json'
-> ERROR=JSONParserErr (Value looks like object, but can't find closing '}' symbol)
```

`content` captures **everything after** the three CRI fields, not up to the next space — which is
what makes it safe for JSON payloads full of spaces.

The optional `(?:…)?` group is *not* needed today — see §3, it exists for the #222 transition.

> **Why not `| cri`?** It does not exist in LogQL. `cri` is an Alloy/Promtail *ingest-pipeline*
> stage. LogQL's parsers are `json`, `logfmt`, `pattern`, `regexp`, `unpack`. #483 tried `| cri`
> and got `parse error … unexpected |`; that is a category error, not a version gap.

#### `| json` — **necessary**

Produces the `fields_*` labels the next two filters need.

#### `| __error__=""` — **not strictly necessary; defensive**

Measured 0 error lines across every query run. Kept because `prompt_budget.py` does the same and it
costs nothing, and because it backstops a non-JSON line that happened to contain a task id.

#### `| fields_task_id != ""` and `| fields_task_id = "${task_id}"` — **both necessary**

These look redundant. They are not, and this is the least obvious part of the change.

First, the field name. The tracing subscriber is built at
[`services/observability/src/lib.rs:75`](services/observability/src/lib.rs) as:

```rust
let fmt_layer = tracing_subscriber::fmt::layer().json();
```

with **no `.flatten_event(true)`**. So `tracing::info!(task_id = %task.id, "…")` serializes with the
field nested:

```json
{"timestamp":"…","level":"INFO",
 "fields":{"message":"dispatched task to a Job","task_id":"640563a4-…"},
 "target":"control_plane::queue::dispatcher"}
```

LogQL's `| json` flattens nested objects with an underscore, so the label is **`fields_task_id`**,
never `task_id`. #483's original `| json | task_id = …` would have returned an empty panel even
after the CRI problem was solved. This second cause was never diagnosed there.

Now why *both* filters. LogQL has an asymmetry: **an absent label compares equal to `""` under `=`,
but unequal under `!=`.** `$task_id` is a text-box variable that defaults to empty — the state when
an operator opens the dashboard directly instead of arriving via the run-detail embed. In that state:

- `|= ""` matches **every** line, and
- `| fields_task_id = ""` **also** matches every line whose `fields_task_id` was never extracted.

So the equality alone firehoses the whole control-plane stream under the title "Logs for ". The
presence guard alone would return every task-bearing line. Together, `!= ""` keeps only lines that
carry a task id, then `= ""` asks which of those have an empty one — none. Measured:

```text
                          real id      EMPTY $task_id
equality only          -> 103 lines -> 500 lines   ← limit-capped firehose
!= "" then equality    -> 103 lines ->   0 lines
```

Outside the empty case, the equality drops nothing: on 24h of real logs, all 335 lines containing a
given task id also carried it in `fields.task_id`. Its load-bearing job is the one above.

This is the same presence-guard shape `prompt_budget.py`'s `_unwrap` already uses.

### 1.4 The generated JSON

```json
{
  "datasource": { "type": "loki", "uid": "${DS_LOKI}" },
  "expr": "{namespace=\"converse\", service_name=\"lightbridge-ci\", container=\"main\"} |= \"${task_id}\" | regexp `^(?:\\S+ std(?:out|err) [FP] )?(?P<content>.*)$` | line_format `{{.content}}` | json | __error__=\"\" | fields_task_id != \"\" | fields_task_id = \"${task_id}\"",
  "queryType": "range",
  "refId": "B"
}
```

**Necessary, and not hand-written.** `deploy/observability/dashboards/*.json` are build artifacts;
CI regenerates them and fails on a diff. You edit the Python, run
`python tools/dashboard-gen/generate.py`, and commit the result.

Two details that matter:

- `\S` is a **single** backslash. LogQL backtick strings are raw. A first attempt used `\\S`, which
  RE2 read as a literal backslash and returned **0 lines**.
- `{{.content}}` is Go-template syntax surviving inside a Helm chart. That works because the chart
  embeds dashboards with `.Files.Glob` under a literal block scalar, which does not re-template
  file contents:

  ```yaml
  {{- range $path, $bytes := .Files.Glob "dashboards/*.json" }}
  spec:
    json: |
  {{ $bytes | toString | indent 4 }}
  ```

  Verified against the rendered manifest.

---

## 2. PR #607, commit 2 — `refactor(dashboards): unify the CRI unwrap into one common helper`

```
common.py        | 15 +++++++++++++++
prompt_budget.py |  5 ++---
task_runs.py     |  5 ++---
```

Commit 1 introduced a second copy of the unwrap chain — `prompt_budget.py` already had one. Commit 3
has to change that chain. Changing it in two places invites them to drift, so this commit makes it
one definition:

```python
# common.py
CRI_UNWRAP = "| pattern `<_> <_> <_> <content>` | line_format `{{.content}}`"
```

and both call sites become:

```python
f"{CRI_UNWRAP} "
```

**Necessary? Strictly, no — the PR would work with two copies.** It is here because commit 3 must
apply identically to both dashboards, and because this repo requires a structural change and a
behavioural change to be separate commits. Splitting them makes commit 3's diff show only what
actually changes semantically.

**Proof it changes nothing**, which is the standard this repo asks for rather than regenerating a
golden to make a diff pass:

```text
$ git checkout <commit 2> && python generate.py
$ git status --porcelain -- deploy/observability/dashboards
<<< empty >>>
```

All eleven dashboard JSONs reproduce byte-for-byte.

---

## 3. PR #607, commit 3 — `fix(dashboards): make the CRI unwrap tolerate already-unwrapped lines`

```
common.py        | 22 +++++++++++++-----
task-runs.json   |  2 +-
prompt-budget.json | 26 +++++++++++-----------
```

One constant changes:

```python
CRI_UNWRAP = (
    r"| regexp `^(?:\S+ std(?:out|err) [FP] )?(?P<content>.*)$` "
    "| line_format `{{.content}}`"
)
```

`| pattern` → `| regexp`, with the CRI prefix wrapped in an **optional** group.

**Necessary — but for #222, not for today.** On current data both forms are identical. Its job is
to survive the window after #222 merges.

### Why that window exists

`stage.cri` affects only newly-ingested lines. Everything already in Loki keeps its envelope for
the full retention period — `retention_period: 90d`
(`ai-helm-values`, `environments/prod/values/loki.yaml:70`). So for three months **both line shapes
are live in Loki at once**, and one query has to serve both.

### Why `| pattern` cannot survive it

Applied to an already-unwrapped line, `<_> <_> <_> <content>` captures garbage, the following
`| json` errors, and the `| __error__=""` every call site pairs it with **drops the line silently**.
Zero rows, no error surfaced anywhere.

Measured against an OTLP-ingested stream, which is already envelope-free — a live preview of what
every line looks like post-`stage.cri`:

```text
UNWRAPPED (OTLP) stream | `| pattern` unwrap        ->   0 line(s)
UNWRAPPED (OTLP) stream | tolerant `| regexp`       -> 200 line(s)
CRI-wrapped stream      | `| pattern` unwrap        -> 103 line(s)
CRI-wrapped stream      | tolerant `| regexp`       -> 103 line(s)
```

`prompt_budget.py` drives a `max_diff_chars` sizing decision off those aggregates. A silent zero
there is worse than an error.

### Cost

`| regexp` (RE2) is more expensive per line than `| pattern`. Both call sites put a `|=` substring
filter before the parser, so RE2 only runs on the small surviving subset. All 13 prompt-budget
queries returned within the normal timeout against 24h of prod data.

---

## 4. PR #222 — `fix(alloy): unwrap CRI-formatted pod logs before parsing`

One functional line in `environments/prod/values/alloy.yaml`, purely additive:

```river
loki.process "kubernetes_labels" {
  stage.cri {}                                    // ← added
  stage.json {
    expressions = { level = "level", trace_id = "trace_id" }
  }
  stage.labels {
    values = { level = "level" }
  }
  forward_to = [loki.write.default.receiver]
}
```

**Necessary, and this is the actual root cause.** #607 is a workaround at the query layer; this is
the fix at the source. It repairs three things:

1. **Every dashboard's `| json`** — the reason #483 exists.
2. **This pipeline's own `stage.json`**, three lines below. It parses the same CRI-wrapped line,
   fails, and so `stage.labels` never promotes `level`. Verified live — the pod-log streams carry no
   `level` label at all:

   ```text
   labels on {namespace="converse", service_name="lightbridge-ci", container="main"}:
   ['cluster', 'container', 'detected_level', 'filename', 'job', 'namespace', 'pod', 'service_name']
   ```

   (`detected_level` is Loki 3.x's own heuristic, not this pipeline's.) The `trace_id` extraction
   feeding the Loki→Tempo derived field fails identically.
3. **Lines the kubelet splits at 16 KB** (`P` flag) are stored as fragments with the JSON broken
   across them. `stage.cri` rejoins them; no query-side workaround can. Currently theoretical here
   — 0 partial lines over 14d, agent-pod lines topping out at 4,726 bytes — but real the moment any
   service logs a large payload.

### Two constraints this file imposes

The file's own header warns that the alloy chart runs `configMap.content` through Helm `tpl`, so
the River config must contain **zero** `{{` sequences. `stage.cri {}` is single braces. Verified:
0 opening sequences on both `main` and this branch.

Validated with the exact Alloy version prod runs:

```text
$ docker run --rm grafana/alloy:v1.16.1 fmt config.alloy   -> exit 0
```

`alloy fmt` also reports whole-file formatting drift (it wants tabs, the file uses spaces), but
that is pre-existing — **753 drift lines on `main`** — and not reformatted here.

### Not verified

The pipeline has not run with this stage applied. `alloy fmt` proves it parses, not that the
resulting stream is correct. After deploy, watch: new lines arriving without the prefix; the `level`
label appearing; both dashboards still returning rows; and **ingest cost**, since `stage.cri` adds a
per-line parse across every pod on every node.

---

## 5. Merge order

**#607 first, then #222.** #222 is a draft specifically to prevent the reverse.

Merging #222 first means new lines arrive unwrapped while the dashboards still run a `| pattern`
unwrap that returns zero rows on them — silently, per §3. The panel would look empty rather than
broken, which is the hardest failure mode to notice. Commit 3 of #607 is the thing that makes #222
safe.

---

## 6. Deliberately not in either PR

| Item | Why not |
|---|---|
| Simplifying to a bare `\| json` | Correct only once the 90d window has fully rolled past the #222 deploy. Needs a dated ticket, not a guess. |
| ~~`NEXT_PUBLIC_GRAFANA_URL` on `lightbridge-ci-web`~~ | **Retracted — this was wrong.** The variable is set in the CI build (`.github/workflows/image-pipeline.yml:107`), not the Deployment, because `NEXT_PUBLIC_*` values are inlined into the bundle by `next build`. Its absence from the Deployment env is expected, not a gap. The embed renders in production today, behind Grafana's SSO. The error came from checking the runtime env instead of the build pipeline. |
| Reformatting `alloy.yaml` to `alloy fmt` style | 753-line unrelated diff. |
| Any other panel issue | Outside #483/#517. |

---

## 7. Necessity at a glance

| Change | Verdict |
|---|---|
| Second panel target | **Required** — this is the fix |
| `namespace` + `service_name` selector | **Required** |
| `container="main"` | Defensive, droppable |
| `\|= "${task_id}"` | **Required** — scoping, cost, and the non-JSON guard |
| `\| regexp` + `\| line_format` | **Required** — the CRI fix |
| ↳ optional `(?:…)?` group | Required for #222 only; a no-op today |
| `\| json` | **Required** |
| `\| __error__=""` | Defensive, droppable |
| `\| fields_task_id != ""` | **Required** — without it, empty `$task_id` firehoses |
| `\| fields_task_id = "${task_id}"` | **Required** — with `!= ""`, gives 0 on empty; precision guard otherwise |
| `common.CRI_UNWRAP` extraction | Not required; keeps commit 3 honest and reviewable |
| `stage.cri {}` | **Required** — the root cause, and the only fix for `level`, `trace_id`, and 16 KB splits |

---

*Every quantitative claim above was measured against the `hetzner-prod` cluster and its Loki
instance on 2026‑08‑11, not inferred. The two unverified items are stated as such: #222's runtime
behaviour (§4) and the ingest-cost impact.*
