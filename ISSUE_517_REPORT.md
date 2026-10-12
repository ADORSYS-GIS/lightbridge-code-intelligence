# Issue #517 — the task-runs Logs panel: full mechanism, root cause, and fix

**Status:** fixed in [PR #607](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/pull/607), verified live against the production Loki on 2026‑08‑11.

| | |
|---|---|
| Story | [#517 — Fix #483 broken task-runs LogQL panel](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/517) |
| Parent epic | [#493 — apps/web investment](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/493) |
| Original bug | [#483 — control-plane task_id logs still broken (CRI-wrapped `\| json` fails)](https://github.com/ADORSYS-GIS/lightbridge-code-intelligence/issues/483) |
| Cluster | `hetzner-prod` — Loki 3.6.7, Alloy v1.16.1, Grafana 12.3.1 |
| Repos involved | `lightbridge-code-intelligence` (the fix), `ai-helm-values` (the root cause), `ai-helm` (stack deployment) |

---

## 1. The situation in one paragraph

The run-detail page has a "Logs for $task_id" panel that is supposed to show **everything that
happened to one review task**. A task's life spans two very different places: the control plane
decides to create it and dispatches it, then a throwaway Kubernetes Job actually runs the review.
The panel only ever showed the second half. The first half — "created review task", "dispatched
task to a Job", "graph ingested" — was silently missing, because the LogQL that was supposed to
fetch it could not parse the log lines at all. The reason turns out to have nothing to do with the
dashboard: it is a single missing stage in the cluster's log-collection pipeline, three repos away.

---

## 2. The mechanism, end to end

Five hops. The bug lives in hop 2, and shows up in hop 4.

```
┌─ 1 ─────────────┐   ┌─ 2 ──────────┐   ┌─ 3 ─────┐   ┌─ 4 ─────────┐   ┌─ 5 ────────┐
│ Rust services   │   │ kubelet      │   │  Loki   │   │  Grafana    │   │  apps/web  │
│ tracing → JSON  │──▶│ writes files │──▶│ stores  │──▶│ LogQL panel │──▶│  d-solo    │
│ on stdout       │   │ Alloy tails  │   │ verbatim│   │             │   │  iframe    │
└─────────────────┘   └──────────────┘   └─────────┘   └─────────────┘   └────────────┘
                       ▲ the bug is here                ▲ where it hurts
```

### Hop 1 — where the logs come from

Every Rust service installs one process-wide `tracing` subscriber, in
[`services/observability/src/lib.rs:75`](services/observability/src/lib.rs):

```rust
let fmt_layer = tracing_subscriber::fmt::layer().json();
```

Two consequences matter here, and both bite later:

1. **Output is JSON on stdout**, one object per line.
2. **There is no `.flatten_event(true)`.** This is the important one. Without it, the fields you
   pass to a `tracing` macro are nested under a `fields` object rather than hoisted to the top
   level. So this call:

   ```rust
   tracing::info!(task_id = %task.id, job_name = %name, "dispatched task to a Job");
   ```

   serializes as this — note `task_id` is **inside** `fields`:

   ```json
   {"timestamp":"2026-08-10T20:52:37.377304Z","level":"INFO",
    "fields":{"message":"dispatched task to a Job",
              "task_id":"640563a4-c120-4ed7-97b5-d6608be1bf81",
              "job_name":"lightbridge-agent-640563a4-c120-4ed7-97b5-d6608be1bf81"},
    "target":"control_plane::queue::dispatcher"}
   ```

The task's two halves are emitted by two different kinds of pod:

- **Long-lived pods** — `lightbridge-ci-control-plane`, `-dispatcher`, `-reconciler`, `-notifier`,
  … in namespace `converse`. These carry the admin/lifecycle lines.
- **One ephemeral Job pod per task**, named from the task id in
  [`services/control-plane/src/integrations/k8s.rs:343`](services/control-plane/src/integrations/k8s.rs):

  ```rust
  /// Deterministic, collision-free Job name for a task (the task id is unique).
  fn job_name(task: &ClaimedTask) -> String {
      format!("lightbridge-agent-{}", task.id)
  }
  ```

  Kubernetes appends its own suffix, so the pod is `lightbridge-agent-<task-id>-<random>` in
  namespace `lightbridge-agents`. That naming is what lets a dashboard select one run's pod by
  task id with no JSON parsing at all.

### Hop 2 — how logs get out of the pod (this is where the bug is)

The kubelet does **not** store a container's stdout verbatim. It writes
`/var/log/pods/<ns>_<pod>_<uid>/<container>/0.log` in the **CRI log format**, which prefixes every
line with three space-separated fields:

```
<RFC3339Nano timestamp> <stdout|stderr> <F|P> <the original line>
```

`F` means a full line, `P` a partial one. So the JSON above is stored on disk — and later in Loki —
looking like this:

```
2026-08-10T20:52:37.377304123Z stdout F {"timestamp":"2026-08-10T20:52:37.377304Z","level":"INFO","fields":{...}}
```

Grafana Alloy runs as a DaemonSet and ships those files to Loki. Its pipeline lives in
`ai-helm-values`, `environments/prod/values/alloy.yaml`:

```river
loki.source.file "pod_logs" {
  targets    = local.file_match.pod_logs.targets
  forward_to = [loki.process.kubernetes_labels.receiver]
}

loki.process "kubernetes_labels" {
  // Labels already set by discovery.relabel; here we only enrich
  // from structured log bodies (level for filtering, trace_id for
  // the Loki→Tempo derived field).
  stage.json {
    expressions = { level = "level", trace_id = "trace_id" }
  }
  stage.labels {
    values = { level = "level" }
  }
  forward_to = [loki.write.default.receiver]
}
```

**There is no `stage.cri {}` anywhere in this pipeline.** `loki.source.file` reads raw file lines;
it does not know or care about the CRI envelope. Nothing strips it. So the CRI prefix travels all
the way into Loki storage.

Stream labels do get attached, but from Kubernetes service discovery rather than from the line — a
deliberate earlier fix, per the config's own comment:

```river
rule { source_labels = ["__meta_kubernetes_namespace"];        target_label = "namespace" }
rule { source_labels = ["__meta_kubernetes_pod_name"];         target_label = "pod" }
rule { source_labels = ["__meta_kubernetes_pod_container_name"]; target_label = "container" }
rule { source_labels = ["__meta_kubernetes_pod_label_app_kubernetes_io_name"]; target_label = "service_name" }
```

That mapping is why the control plane's Loki labels are
`namespace="converse", service_name="lightbridge-ci", container="main"` — `service_name` comes from
the pod's `app.kubernetes.io/name`, which every `lightbridge-ci-*` pod shares.

### Hop 3 — storage

Loki stores the line **exactly as received**. It does no format detection and no normalisation.
Whatever Alloy sent is what a query gets back. Confirmed live:

```text
$ query '{namespace="converse", service_name="lightbridge-ci", container="main"}'
-> 2026-08-11T07:25:51.844484734Z stdout F {"timestamp":"2026-08-11T07:25:51.844251Z","level":"INFO",...}
```

### Hop 4 — how it is meant to be displayed

Dashboards are **generated**, not hand-written. `tools/dashboard-gen/lci_dashboards/<name>.py`
builds them with the Grafana Foundation SDK; `generate.py` renders deterministic JSON into
`deploy/observability/dashboards/`. A CI job regenerates and diffs, so the JSON is a build artifact
— you edit the Python, never the JSON.

That JSON ships in a Helm chart which wraps each file in a `GrafanaDashboard` CR
(`deploy/observability/templates/grafana-dashboards.yaml`):

```yaml
{{- range $path, $bytes := .Files.Glob "dashboards/*.json" }}
kind: GrafanaDashboard
metadata:
  name: lci-{{ $base }}
spec:
  json: |
{{ $bytes | toString | indent 4 }}
```

`.Files.Glob` + a literal block scalar means the JSON is **embedded, not templated** — which is why
a LogQL `{{.content}}` inside it survives Helm untouched. The Grafana Operator then pushes the
dashboard into Grafana and re-asserts it every `resyncPeriod`.

### Hop 5 — the web embed

`apps/web` renders the panel chromeless via `d-solo`
([`apps/web/components/runs/run-logs-embed.tsx`](apps/web/components/runs/run-logs-embed.tsx)):

```tsx
const GRAFANA_DASHBOARD_UID = "lci-task-runs";
const GRAFANA_LOGS_PANEL_ID = 100;

const src =
  `${base}/d-solo/${GRAFANA_DASHBOARD_UID}/task-runs` +
  `?orgId=1&panelId=${GRAFANA_LOGS_PANEL_ID}` +
  `&var-task_id=${encodeURIComponent(taskId)}&theme=${GRAFANA_THEME}&kiosk`;
```

The panel id is pinned to `100` in the generator precisely so this URL stays stable.

---

## 3. The issue

### What was broken

The panel had exactly one query — the ephemeral Job pod's logs:

```python
.with_target(logql('{namespace="lightbridge-agents", pod=~"lightbridge-agent-${task_id}-.*"}'))
```

That works, and needs no parsing: the pod name contains the task id, so the stream selector alone
is the filter. But it only covers what the **agent** did. The control plane's own task-scoped
lines live in different pods, in a different namespace, where the only thing identifying the task
is a field **inside the JSON**. Getting at those requires parsing the line — and that is where it
fell over.

### Why it failed — two independent reasons stacked

**Reason 1 — the CRI envelope.** The obvious query is `| json | task_id = "..."`. But `| json`
receives `2026-08-10T20:52:37Z stdout F {"timestamp":...}` and dies on the first character:

```text
$ query '{namespace="converse", service_name="lightbridge-ci", container="main"} | json'
-> ERROR=JSONParserErr (Value looks like object, but can't find closing '}' symbol)
```

The natural instinct is `| cri`, and that is what #483 tried:

```text
-> parse error at line 1, col 61: syntax error: unexpected |
```

That error is not a version problem. **`cri` is a Promtail/Alloy *pipeline* stage, not a LogQL
*parser* stage.** It exists on the ingest side only. LogQL's parsers are `json`, `logfmt`,
`pattern`, `regexp`, and `unpack` — there is no `cri` among them and never has been. So the CRI
envelope can only be dealt with in one of two places: at ingest (add `stage.cri`), or in the query
using a general-purpose parser to strip it.

**Reason 2 — the field is nested.** Even with the envelope gone, `| json | task_id = "..."` would
still have matched nothing. Because of the missing `.flatten_event(true)` in hop 1, `task_id` sits
under `fields`, and LogQL's `| json` flattens nested objects by joining keys with `_`. The label is
therefore `fields_task_id`, never `task_id`. Verified live — the extracted label set for one line:

```text
['fields_delivery_id','fields_message','fields_pr','fields_run_epoch',
 'fields_task_id','level','target','timestamp']
```

This second reason was never diagnosed in #483; it would have produced a silent empty panel even
after fixing the first.

### The blast radius is wider than this panel

The same missing `stage.cri` breaks Alloy's **own** enrichment, three lines below it in the config.
`stage.json { expressions = { level = "level", ... } }` parses the same CRI-wrapped line and fails
the same way, so `stage.labels { values = { level = "level" } }` never fires. Proven live — the
Alloy-set stream labels are:

```text
['cluster', 'container', 'detected_level', 'filename', 'job', 'namespace', 'pod', 'service_name']
```

No `level`. (`detected_level` is Loki 3.x's own heuristic, not Alloy's — and being differently
named, it does not collide with the `level` that a query-side `| json` extracts.) The `trace_id`
extraction feeding the Loki→Tempo derived field is broken by the same mechanism.

---

## 4. What the fix does

### The key discovery: this repo had already solved it

`tools/dashboard-gen/lci_dashboards/prompt_budget.py` queries the *agent* pods' JSON logs and hit
this exact wall first. Its `_CRI_JSON` is the established in-repo answer:

```python
_CRI_JSON = (
    f"{_SELECTOR} "
    '|= "prompt budget usage" '
    "| pattern `<_> <_> <_> <content>` "
    "| line_format `{{.content}}` "
    '| json | __error__=""'
)
```

Its module docstring records verification against real Loki 3.1.1 and 3.5.7 on genuine
`tracing_subscriber` output. So #517 is not a research problem — it is applying a proven mechanism
to a second panel. PR #607 deliberately reuses that exact chain rather than inventing a `| regexp`
variant.

### How the three stages work

```
| pattern `<_> <_> <_> <content>`
```

`pattern` matches using the literal text between captures as delimiters — here, single spaces. It
consumes three fields (timestamp, stream, flag), discards them via the unnamed `<_>` capture, and
binds everything remaining to `content`. Crucially the final capture takes **the rest of the line**,
not up to the next space — which is what makes it safe for JSON full of spaces.

```
| line_format `{{.content}}`
```

Rewrites the log line to be just that capture. Downstream stages now see bare JSON. (This is Go
template syntax, which is why it must survive Helm — it does, per hop 4.)

```
| json | __error__=""
```

Parses it, then drops any line that still failed — a defensive backstop, not decoration.

### The change

[`tools/dashboard-gen/lci_dashboards/task_runs.py`](tools/dashboard-gen/lci_dashboards/task_runs.py):

```python
_CONTROL_PLANE_SELECTOR = '{namespace="converse", service_name="lightbridge-ci", container="main"}'

_CONTROL_PLANE_LOGS = (
    f"{_CONTROL_PLANE_SELECTOR} "
    '|= "${task_id}" '
    "| pattern `<_> <_> <_> <content>` "
    "| line_format `{{.content}}` "
    '| json | __error__="" '
    '| fields_task_id != "" '
    '| fields_task_id = "${task_id}"'
)
```

added as a second target on the existing panel — the first is untouched:

```python
run_logs = (
    logs.Panel()
    .id(100)
    .title("Logs for $task_id")
    .datasource(LOKI)
    .show_time(True)
    .wrap_log_message(True)
    .with_target(logql('{namespace="lightbridge-agents", pod=~"lightbridge-agent-${task_id}-.*"}'))
    .with_target(logql(_CONTROL_PLANE_LOGS, ref_id="B"))
    .grid_pos(layout.place(24, 11))
)
```

Design notes, each load-bearing:

- **`service_name="lightbridge-ci"`, not `pod=~"lightbridge-ci-control-plane-.*"`.** A task's
  lifecycle genuinely spans several pods — the live run below returned 102 lines from the control
  plane *and* 1 from the dispatcher. Narrowing to one pod would drop the dispatch record.
- **`|= "${task_id}"` comes before every parser.** It is the cheap narrowing pass over a large
  mixed stream, and it is also the safety guard: `service_name="lightbridge-ci"` includes
  `lightbridge-ci-neo4j`, whose logs are not JSON. Those lines never reach `| json` because they
  do not contain the task id.
- **`fields_task_id`, not `task_id`** — reason 2 above.
- **`| fields_task_id != ""` before the equality** — see §5.

The generated diff is nine added lines, nothing removed:

```json
{
  "datasource": { "type": "loki", "uid": "${DS_LOKI}" },
  "expr": "{namespace=\"converse\", service_name=\"lightbridge-ci\", container=\"main\"} |= \"${task_id}\" | pattern `<_> <_> <_> <content>` | line_format `{{.content}}` | json | __error__=\"\" | fields_task_id != \"\" | fields_task_id = \"${task_id}\"",
  "queryType": "range",
  "refId": "B"
}
```

---

## 5. A bug the live verification found in the fix itself

Worth recording, because it was invisible to every form of static checking and to CI.

`$task_id` is a `TextBoxVariable` defaulting to empty — the state when an operator opens the
dashboard directly rather than arriving through the run-detail embed. On an empty value:

- `|= ""` matches **every** line, and
- `fields_task_id = ""` **also** matches every line whose `fields_task_id` was never extracted,
  because LogQL treats an absent label as equal to the empty string under `=`.

So the first version of the fix firehosed the entire control-plane stream into a panel titled
"Logs for ". Measured against prod rather than reasoned about:

```text
current  (= only)          | real id -> 103 line(s)
current  (= only)          | EMPTY   -> 500 line(s)   ← limit-capped firehose
guarded  (!= '' then =)    | real id -> 103 line(s)
guarded  (!= '' then =)    | EMPTY   ->   0 line(s)
```

The fix is a presence guard, `| fields_task_id != ""`, before the equality. An absent label
compares *unequal* to `""` under `!=` — the asymmetry that makes this work — so empty-variable
lines are dropped before the equality can wave them through. It is the same shape
`prompt_budget.py`'s `_unwrap` already uses, for a closely related reason.

Note that the original target A was always correct here: with an empty task id its selector becomes
`pod=~"lightbridge-agent--.*"`, which matches nothing.

---

## 6. Verification

All expressions were read out of the **generated** `task-runs.json`, not retyped from the
generator source.

**The bug, reproduced:**

```text
$ query '{namespace="converse", service_name="lightbridge-ci", container="main"}'
-> 2026-08-11T07:25:51.844484734Z stdout F {"timestamp":"...","level":"INFO",...}

$ query '… | json'
-> ERROR=JSONParserErr (Value looks like object, but can't find closing '}' symbol)
```

**The fix, working** — task `a2daa4c6-d157-4cfe-9c11-a17792d1f078`:

```text
target A | task a2daa4c6…           -> 107 lines, 0 parse-errors
                                       pods: {lightbridge-agent-a2daa4c6-…-lm6d2: 107}
target A | empty $task_id (default) ->   0 lines, 0 parse-errors
target B | task a2daa4c6…           -> 103 lines, 0 parse-errors
                                       pods: {lightbridge-ci-control-plane-…: 102,
                                              lightbridge-ci-dispatcher-…:      1}
target B | empty $task_id (default) ->   0 lines, 0 parse-errors
```

A target-B line, unwrapped and parsed:

```json
{"timestamp":"2026-08-11T06:09:07.652873Z","level":"INFO",
 "fields":{"message":"graph ingested","task_id":"a2daa4c6-d157-4cfe-9c11-a17792d1f078",
           "nodes":1791,"edges":3597},
 "target":"control_plane::http::internal"}
```

Emitting targets across those 103 lines: `control_plane::http::internal` ×101,
`control_plane::queue::dispatcher` ×1, `control_plane::http::webhook` ×1. The control-plane count
matches `kubectl logs` for the same task exactly (102).

**CI**, reproduced locally on the CI interpreter (python 3.12, pinned SDK `1769699998!10.1.0`):
regeneration touched exactly one file — the other ten dashboards came back byte-identical, which is
what proves the toolchain matches CI — and `helm lint` / `helm template` both pass.

### How to reproduce any of this

```bash
kubectl -n observability port-forward svc/loki-gateway 3100:80
```

`loki-gateway` is ClusterIP-only with no ingress, and the `observability` namespace runs
`default-deny-ingress` + `allow-same-namespace`, so it is reachable only from inside that namespace
— hence the port-forward, which tunnels via the kubelet and bypasses the NetworkPolicy entirely.
This needs `create` on `pods/portforward`; check with
`kubectl auth can-i create pods --subresource=portforward -n observability` (the `pods/portforward`
spelling silently answers "no" and is a trap).

Without that permission, Grafana Explore at `https://grafana.ai.camer.digital` runs the identical
LogQL against the identical datasource and needs no extra RBAC at all.

---

## 7. Known remaining gaps

Each is deliberately outside PR #607, and each needs its own ticket.

1. **Add `stage.cri {}` to the Alloy pipeline** (`ai-helm-values`,
   `environments/prod/values/alloy.yaml`). This is the true upstream fix and also repairs the
   `level` label and the `trace_id`/Tempo derived field, cluster-wide:

   ```river
   loki.process "kubernetes_labels" {
     stage.cri {}                    // ← unwrap `<ts> stdout F ` before anything parses
     stage.json {
       expressions = { level = "level", trace_id = "trace_id" }
     }
     ...
   }
   ```

   It must land **after** #607, not instead of it, and it cannot be a drop-in. Three measurements
   constrain the migration:

   - **Retention is 90d** (`loki.yaml:70`), so both line shapes coexist for 90 days after rollout.
   - **The existing queries break silently on unwrapped lines.** Tested by running the CRI-unwrap
     chain against an OTLP-ingested stream, which is already envelope-free — i.e. a preview of what
     every line looks like post-`stage.cri`:

     ```text
     bare | json                                        -> 50 lines, errors=none
     CRI-unwrap chain applied to an UNWRAPPED line      -> 50 lines, errors={JSONParserErr}
       ...with __error__="" as the panel actually has it ->  0 lines, errors=none
     ```

     Zero lines, no error surfaced. `prompt_budget.py` drives a `max_diff_chars` sizing decision off
     those numbers and would silently read empty. So `task_runs.py` and `prompt_budget.py` must be
     made shape-tolerant *before* `stage.cri` lands — e.g.
     `| regexp "^(?:\\S+ std(?:out|err) [FP] )?(?P<content>.*)$"`, whose optional prefix group
     matches both — or all three changes must ship in one coordinated window.
   - **Partial-line reassembly is not an argument for it here.** `stage.cri` also rejoins lines the
     kubelet splits at 16 KB (`P` flag), which no query-side unwrap can do. But prod has **0
     partial lines in 14d** across both namespaces, and agent-pod lines top out at 4,726 bytes
     (p99 4,350; median 288) — nowhere near the threshold. Worth re-checking if log volume per line
     ever grows.

2. ~~**`NEXT_PUBLIC_GRAFANA_URL` is unset on the deployed `lightbridge-ci-web` pod.**~~
   **Retracted — this was wrong.** The variable *is* set, in the CI build rather than the
   Deployment (`.github/workflows/image-pipeline.yml:107`):

   ```yaml
   - name: turbo build (remote cache)
     env:
       NEXT_PUBLIC_GRAFANA_URL: https://grafana.ai.camer.digital
     run: pnpm turbo run build --filter=@lightbridge/web
   ```

   `NEXT_PUBLIC_*` values are inlined into the client bundle by `next build`, so by image-build
   time the URL is a literal string in the compiled JS and there is nothing left to read from the
   environment at runtime. Its absence from the Deployment is the expected state, not a gap. The
   embed works in production today; the iframe loads `grafana.ai.camer.digital`, Grafana redirects
   to Keycloak SSO if the browser has no session, and the panel renders after sign-in.

   The original claim came from checking the Deployment's runtime env — the one place a
   `NEXT_PUBLIC_` var provably cannot appear — and not checking the build pipeline. No follow-up
   work is needed here.

3. **The CRI-unwrap chain is now duplicated** in `task_runs.py` and `prompt_budget.py`. Extracting
   it to `common.py` is the obvious cleanup, but it touches a heavily-verified module and this repo
   requires a refactor and a behaviour fix to be separate commits.

---

## 8. Summary

| Question | Answer |
|---|---|
| What's broken? | The run-detail Logs panel showed only the agent pod's logs, never the control plane's task lifecycle. |
| Why? | Its `\| json` could not parse the line — Loki stores it CRI-wrapped. |
| Why is it CRI-wrapped? | The kubelet writes that format, and Alloy's pipeline has no `stage.cri` to strip it. |
| Why didn't `\| cri` work? | `cri` is an ingest-pipeline stage, not a LogQL parser. It has never existed in LogQL. |
| Second, undiagnosed reason | `task_id` nests under `fields` (no `.flatten_event(true)`), so the label is `fields_task_id`. |
| How is it fixed? | A second panel target that strips the envelope with `\| pattern` + `\| line_format` before `\| json` — the chain `prompt_budget.py` already proved. |
| Verified? | Yes — live against prod Loki. 103 control-plane lines, 0 parse errors, both empty- and real-variable cases. |
| Anything still open? | Alloy's missing `stage.cri` (§7). The panel itself is live — the embed renders in production behind Grafana's SSO. |

---

*Prepared by Claude (Claude Code) from source inspection across all three repos and live read-only
queries against the `hetzner-prod` cluster and its Loki instance, 2026‑08‑11. Every quantitative
claim above was measured, not inferred; the two open items in §7 are stated as gaps rather than
folded into the PR.*
