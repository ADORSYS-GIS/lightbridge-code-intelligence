# Lightbridge Code Intelligence — Review Quality & Performance Improvement Report

*Prepared 2026-08-21. Based on a full read of the ADR corpus (114 ADRs), the review pipeline docs, and
external research on comparable AI code-review systems and academic literature on LLM verification.*

## 1. What Lightbridge is actually trying to do

Lightbridge is a GitHub App that turns a PR into a **repo-aware, trust-bounded review task**: a Rust
control plane (the only thing holding the GitHub App key and the only writer to GitHub) dispatches
disposable Kubernetes Jobs that clone the repo, build two indexes from one tree-sitter parse (a pgvector
semantic index and a Neo4j structural graph), and run an LLM review agent over mediated tools. The agent
never touches GitHub directly and never trusts its own confidence — findings flow through a validated
buffer, and the product's whole differentiator, stated over and over in the ADR history, is **"a wrong
blocker costs more trust than a missed nit."** Everything in the last three months of ADRs (0040 through
0114) is downstream of that one sentence: evidence citation, refute passes, coverage gates, grounding
rules, capability floors, feedback memory. This is already a team that has been fighting false positives
methodically, one incident at a time, for a while. The improvements below build on that discipline rather
than replace it.

## 2. Current state — what's already built (don't re-invent these)

| Mechanism | ADR | What it does |
|---|---|---|
| Evidence citation on every finding | 0043 | `add_review_comment` requires an `evidence` field; rendered to the human, no schema change |
| Refute pass | 0043 | One bounce before `finish` on any P0/P1: re-verify against cited evidence or retract |
| Outward disconfirmation nudge | 0091 | For absence-claims ("X is never sent"), the refute bounce is told to search *outside* already-engaged files, not re-read the same two files |
| Grounding contract | 0047 | "0 hits ≠ absence" — an empty retrieval must never be read as proof something doesn't exist; explicit permission to say "could not verify" |
| Prompt structure rewrite | 0048 | Top+bottom anchored prime directives, worked good/bad examples, induced planning — GLM/MiniMax-portable prompting technique |
| Full-diff coverage gate | 0041, 0069 | Tracks which changed files were `read_file`'d or commented on; bounces (capped, not infinite) a `finish` that skips files; discloses uncovered files on the posted review rather than trusting the model's claim |
| SAST (opengrep) as a tool | 0061, 0073 | Deterministic, token-free scan rides the same mediated finding buffer; anchor-gated so a "false positive" verdict must cite the real flagged line (0061 amendment) |
| Two-index hybrid retrieval | 0086, 0089, 0090, 0114 | pgvector chunk search + Neo4j graph (`find_symbol`/`get_callers`) + a new WRRF-fused semantic+exact **symbol** search |
| Feedback memory M1 | 0044 | Read-only: findings a human 👎'd are injected as "don't re-raise this" context |
| Re-review reconciliation | 0040, 0065 | Prior findings are fed back so successive runs don't contradict or duplicate themselves |
| Context-window budgeting | 0045, 0070 | Converge-before-overflow wind-down; every injected static block is capped as a share of the *actual* model window, not a fixed constant |
| Model-capability floor | 0069, restated by 0103 | Documents (not yet code-enforces) that the "thorough" preset needs a frontier-class reasoning model — a flash/lite model games the honor-system gates instead of doing the work |
| Eval-driven prompt iteration | 0049 | Tier-1: deterministic scripted-model regression tests in CI. Tier-2 (real model on curated golden diffs): **proposed, still not built** |
| Repo-configurable presets | 0103 | fast/deep/ultra are now named `ReviewConfig` presets a repo picks per entry-point, not hardcoded tiers |
| OpenCode-hosted loop | 0097 | The agent loop itself is now OpenCode-over-ACP; the tuned gates (coverage/refute) run supervisor-side, reused verbatim |

This is a strong foundation. The rest of this report is about what to layer on top.

## 3. Where the system still lags — and how to fix it

### 3.1 The refute pass is one-shot, same-model, and only truly adversarial for one failure shape

**The gap.** ADR-0043's refute pass asks the *same model, same context* to "re-verify" its own finding.
ADR-0091 fixed this for exactly one failure family (absence claims: "X is never sent/set") by pointing the
re-verify at specific unexplored files. Its own "What this deliberately defers" section admits the fix
does **not** generalize to other confidently-wrong shapes — mis-stated invariants, wrong type claims,
incorrect control-flow assumptions — and that a model determined to be wrong can still "satisfy the letter
of the search and re-affirm."

**Why this matters.** Academic work on exactly this problem (arXiv 2604.19049, "Refute-or-Promote: An
Adversarial Stage-Gated Multi-Agent Review Methodology") formalizes what ADR-0091 discovered by hand: a
**three-role split — Discoverer, Refuter, Promoter** — where the Refuter is a distinct agent instance whose
only job is to try to disprove the claim, and a Promoter judges the contest. The self-confirmation problem
ADR-0091 names ("re-reading the lines a finding already cites can only ever reproduce the same read") is
structural to a single model re-reading its own work; it goes away when the refuter role gets a genuinely
adversarial framing and, ideally, no visibility into the discoverer's own reasoning — only the claim and
the evidence.

**Recommendation.** Generalize the refute pass from a one-shot "re-verify" nudge to a real second turn
framed as adversarial disconfirmation for *every* P0/P1, not just absence claims:
- Give the refute turn a framing that says "your job now is to try to prove this finding wrong — find the
  strongest counter-evidence you can, don't just re-read what you already cited."
- This costs nothing new in infrastructure — it reuses the exact bounce/`retract_finding` machinery ADR-0043
  already built. It's a prompt change plus removing the absence-claim-only gate that ADR-0091 scoped in.
- Track it with a Tier-1 golden case the same way every other prompt lever is gated (ADR-0049's own
  discipline).

### 3.2 The refute pass is still an honor-system gate — pair it with something that doesn't rely on the model's good faith

**The gap.** Every quality gate in this pipeline (coverage, refute, SAST-anchor) is a *behavioral
contract*: a corrective message that assumes the model responds to it in good faith. ADR-0069 documented
this precisely after a flash-class model satisfied a bounce's letter ("call `finish` again") while
violating its intent, and ADR-0091 restates the same limitation for the refute pass. ADR-0091 itself names
the stronger fix and explicitly defers it: **a mediated `search_for_disconfirming_evidence` tool that
structurally fails a P0/P1 unless the model actually queries outside its already-engaged file set** — not
a nudge, a hard requirement enforced in code.

**Recommendation.** Build that deferred tool. It's a small, well-scoped addition given the machinery
already exists (`engaged_files` tracking is already computed for the coverage gate and the absence-claim
nudge in ADR-0091's `RefuteGate`). Concretely: before a `finish` carrying a P0/P1, require at least one
`read_file`/`graph_*`/`vector_search` call this turn whose target is outside `engaged_files` — if none
occurred, bounce again (still capped, per the ADR-0069 precedent) rather than accepting the `finish`. This
converts "the model is supposed to look elsewhere" from a prompt hope into a mechanically checkable
precondition, the same way the coverage gate converted "the model is supposed to read every file" into one.

### 3.3 A wrong-but-confident model is still just one model — add cheap, independent cross-checking for the findings that matter most

**The gap.** The whole refute-pass design (0043) exists because "a model that is wrong is also confident,"
so self-reported confidence is worthless. That's correct. But the fix chosen — asking the *same* model to
re-check itself — inherits a version of the same problem: a model's own re-reading is correlated with its
own first read, not independent of it.

**What the literature and the field are converging on** is cheap, independent resampling, not a bigger
prompt: research on multi-pass LLM review aggregation ("Multi-Review," described in recent literature
surveys of agentic PR review) shows that running the same reviewer model multiple independent times on
the same target and aggregating (majority vote / an aggregator pass) measurably improves F1 and recall
versus a single pass — because independent samples decorrelate the failure, where re-reading the same
context does not. Separately, Cubic.dev's production system — a **Planner + Security + Duplication +
Editorial** multi-agent split, each agent independently proposing findings that get reconciled — is
reported to cut false positives ~51% *without* losing recall, precisely because independent, narrower-scoped
agents don't share each other's blind spots the way one sequential loop with one context does.

**Recommendation, scoped to this project's actual cost model.** Don't run N full reviews (expensive, and
ADR-0062/0060 already fought hard to keep cost bounded). Instead, apply independent resampling **only** to
the already-narrow set of contested claims — i.e., extend the refute pass so that, for a P0/P1 finding, a
*second independent sample* (fresh context, same model or a cheaper one, given only {the claim, the cited
evidence} with no framing from the original run) is asked "does this hold?" This is structurally identical
to what QASecClaw does for SAST triage (a dedicated filter agent re-judges each candidate finding with only
the finding + code context, not the original scanner's reasoning) — and QASecClaw's measured result on
OWASP Benchmark was an **88.6% false-positive reduction at only 3.1% recall loss**. The same shape — a
narrow, independent, second-opinion pass on a small number of already-flagged items — is cheap because it
never re-runs the whole investigation, only re-judges the claims that survived to P0/P1.

This is the single highest-leverage change available: it directly targets "confidently-wrong P0/P1s" (the
project's own stated top failure mode, ADR-0043) with a technique that has published false-positive
numbers an order of magnitude better than prompt-only re-verification.

### 3.4 The eval harness's real-model tier has been "proposed, not built" since 2026-06-25

**The gap.** ADR-0049 designed a two-tier eval system: Tier 1 (deterministic, scripted-model, in CI — this
exists and works) and Tier 2 (the *real* model against curated golden diffs, scored against a rubric — this
is the tier that would actually prove a prompt change doesn't regress quality). Every subsequent ADR that
touches review quality (0091, 0097, 0114) still points back at epic #252 as open. In practice this means
**the only way a prompt or model regression is caught today is a live PR in production** — which is exactly
how the #187 hallucination, the #304 absence-claim false positive, and the #305 SAST-anchor mismatch were
all discovered. Three concrete production incidents, each becoming its own ADR, are the direct, admitted
cost of not having Tier 2.

**Recommendation.** Build a minimal Tier 2 now rather than continuing to defer it — even 15-20 curated
golden diffs (each of the incidents above is already a perfect seed case, since they're documented in
detail with expected outcomes) run against the live model before any prompt or model-config change ships.
Score with a simple rubric first (did it find the planted issue? did it stay in scope? did it hallucinate
on an empty/hollow index?) — an LLM-judge automation layer is a fine follow-on, but per OpenAI's own
evaluation guidance (cited approvingly in ADR-0049 itself), a human rubric on a small curated set beats no
signal at all, and is cheap to start. This is the report's second-highest-leverage recommendation because
it's the only lever here that catches *future*, not-yet-observed regressions, rather than one already-named
failure mode.

### 3.5 The SAST tool call is itself gated on model good faith — with a documented, un-built fix

**The gap.** ADR-0073 moved SAST from an always-on deterministic pre-pass to an agent-called tool
(`run_sast`), which fixed the "SAST runs on pure conversational asks" waste — but its own Consequences
section flags the trade explicitly: *"A model that never calls `run_sast` gets zero deterministic coverage,
silently... a finish-time safeguard... is the natural follow-up — noted, not built."*

**Recommendation.** Build it. This is the cheapest item in this report: on a review-kind run, if `finish`
is reached and `run_sast` was never called, either force one call before accepting `finish`, or (lower risk,
faster to ship) append the same style of machine-authored disclosure the coverage gate already uses
("⚠️ this run did not execute a security scan") so the gap is visible rather than silent. Given the
coverage-disclosure pattern is already built and battle-tested (ADR-0069), this is largely wiring, not new
design.

### 3.6 Prompt caching is a named, deferred latency lever — and latency is the tier-differentiator that's already caused one regression

**The gap.** ADR-0048 explicitly defers "splitting the system prompt into a cached static prefix + per-run
suffix... a cost/latency optimization, not a quality one." Meanwhile ADR-0060's own measurement is stark:
the dominant cost driver is **`reasoning_effort` + turn count + retrieval depth, not the model or the
gateway** — and prod already drifted once from the fast/cheap model (MiniMax-M2.7, ~47 tok/s) to a slow one
(GLM-5.2, ~11-15 tok/s, folding all its thinking into `completion_tokens`) with a 3-4x wall-clock and cost
regression that went unnoticed until this ADR's own investigation caught it.

**Recommendations:**
- **Ship prompt caching.** The system prompt (~6.5KB+ after the 0048 rewrite) plus tool schemas is resent,
  unchanged, on every single turn of every multi-turn review. Most OpenAI-compatible gateways/providers
  support prefix caching (cached-prefix pricing/latency discounts); splitting the static prefix (system
  prompt + tool defs + AGENTS.md block) from the per-run suffix (diff + conversation) is close to free
  engineering-wise given the prompt assembly is already centralized (`build_messages`), and directly cuts
  TTFT and cost on every deep-tier multi-turn run — which is exactly the workload ADR-0062 and ADR-0060 spent
  the most effort trying to make cheap by other means.
- **Add a standing alert on `review.model`/`reasoning_effort` drift** from the ADR-0054 recorded decision.
  The GLM-5.2 regression was caught by a manual investigation, not monitoring — given ADR-0060 already
  logs the active `review.extra` at agent start, a simple check that flags when the deployed config diverges
  from the last operator-approved model/effort pair would have caught the 4x latency regression the same
  day it happened instead of during a later ad hoc investigation.
- **De-prioritize speculative decoding.** It's a reasonable-sounding lever, but current research on
  search/tool-heavy agents (arXiv 2511.20048) finds its benefit is limited here specifically because
  end-to-end latency in an agent loop is dominated by tool-execution and retrieval round-trips, not
  decode-bound spans — the parallel-batching work already done in ADR-0042 is the more relevant lever for
  this shape of workload, and is worth extending (below) before reaching for decode-level tricks.

### 3.7 Consider fanning the single sequential loop into a few narrow, parallel specialized passes

**The gap.** The review agent is architecturally one loop with a growing toolbox (retrieval, SAST,
external-knowledge MCP, hybrid graph search) reasoning sequentially through a risk-first plan (ADR-0042).
Every industry system reviewed for this report that reports strong false-positive numbers uses the
opposite shape: **multiple narrower agents run in parallel, each responsible for one dimension, reconciled
at the end** — Cubic.dev's Planner/Security/Duplication/Editorial split (-51% FPs, same recall), Qodo's
multi-agent architecture with auto-learning rules, CodeRabbit's parallel Review/Verification/Chat/Pre-Merge
agents.

**Why this is plausible here specifically.** ADR-0032 already broadened the review's mission to five
categories (security, correctness, quality, style, performance) inside *one* loop, relying on priority
(P0-P2) to keep the output triage-able rather than splitting by dimension. A narrower agent per dimension
would (a) give each pass a smaller, more focused context — which composes well with the context-budget
pressure ADR-0045/0070 are already fighting — and (b) let a security-focused pass and a correctness-focused
pass cross-validate structurally, the same way the refute pass does today but for free, as a side effect of
running independently rather than as a deliberate extra turn.

**Recommendation.** This is a bigger architectural lift than the others in this report, so treat it as a
longer-horizon item, not a next-sprint one — but worth prototyping behind the existing preset mechanism
(ADR-0103 already makes "a named review configuration" a first-class, swappable concept): an `ultra` preset
could fan out into 2-3 narrow sub-passes (e.g., security-and-correctness vs. quality-and-performance) whose
findings are reconciled the same way SAST findings already reconcile into the shared buffer today (last-write-wins
by `(file, line)`, per ADR-0061). Given the OpenCode-hosted loop (ADR-0097) already runs subagents and the
recorder plugin already sees subagent-internal tool calls for coverage purposes, the transport-level
plumbing for this may already partly exist.

### 3.8 Two smaller, cheap wins worth closing while in this area

- **Reasoning persistence (ADR-0060's own follow-up, not yet done).** Chain-of-thought is currently
  logs-only, capped, and never reaches the DB/UI. Persisting it (even truncated) directly feeds a future
  Tier-2 eval harness's rubric-scoring *and* gives a human reviewer the "why" behind a finding, which is
  exactly the kind of transparency that reduces the trust cost of an eventual false positive.
- **Neo4j vector-index scoping (ADR-0114's own disclosed limitation).** The hybrid symbol search currently
  ranks across *every* repo/commit in the index before filtering to the current one, mitigated only by a
  large `sourceK=500`. This is a real, disclosed correctness risk that grows quietly as the number of
  indexed repos grows — worth scheduling the "per-repo/snapshot-scoped index" follow-up (or upgrading to
  Neo4j's filtered vector search once available) before it silently degrades retrieval grounding on a larger
  deployment, since degraded retrieval is a direct input to more hallucinated/ungrounded findings.

## 4. How Lightbridge already compares to the market

| Dimension | Lightbridge today | Where the market leans |
|---|---|---|
| Grounding | Repo-only (graph+vector) + Context7/web-search via mediated MCP (ADR-0066) — good parity | CodeRabbit also greps the last few thousand commits and runs the actual test suite against the change — Lightbridge has no execution-grounded verification yet (§3.7 territory, longer-term) |
| False-positive control | Evidence citation + one-shot refute + coverage gate — a genuinely disciplined, incident-driven design | Best-in-class systems (Cubic, QASecClaw) get their biggest wins from *independent* second-opinion passes, not same-model re-verification — this is exactly §3.1/§3.3 above |
| Determinism | opengrep (SAST) rides the same buffer as LLM findings, deterministic and cheap | CodeRabbit combines 50+ static analyzers/linters this way — Lightbridge's SAST-tool pattern is generic enough to add more deterministic sources cheaply if useful (a second scanner, a CVE/dependency check) |
| Memory | M1: read-only, reject-only, keyed on 👎 | CodeRabbit's "Learnings" also captures positive reinforcement and issue/PR-linked context, not just rejected findings — ADR-0044's own M2 (agent-writable memory) was deferred specifically because tool-calling was unreliable at the time; worth revisiting now that MiniMax-M2.7 tool-calling is confirmed solid |
| Eval discipline | Tier 1 (CI, scripted) shipped; Tier 2 (real model) still proposed-not-built | This is the gap that most directly explains why several failure modes reached production before being caught — see §3.4 |

## 5. Priority order (highest leverage first)

1. **§3.3 — independent second-opinion pass on P0/P1 claims.** Biggest, best-evidenced false-positive lever
   (QASecClaw: -88.6% FP at -3.1% recall on a comparable filtering task); reuses existing refute-pass
   machinery; bounded, cheap.
2. **§3.4 — stand up Tier 2 eval harness, even minimally.** The only lever that catches regressions before
   production instead of after; three past incidents are ready-made seed cases.
3. **§3.2 — tool-enforced disconfirmation search.** Closes the honor-system gap ADR-0091 explicitly named
   and deferred; small, well-scoped build on `engaged_files` tracking that already exists.
4. **§3.5 — SAST-not-called finish-time disclosure.** Smallest possible fix; reuses the exact disclosure
   pattern already shipped for coverage.
5. **§3.1 — generalize the refute pass beyond absence claims.** Prompt-only change plus removing a scope
   gate; pairs naturally with #1.
6. **§3.6 — prompt caching + model/effort drift alerting.** Pure latency/cost win with no quality
   trade-off; the drift alert would have caught an already-happened regression same-day.
7. **§3.8 — reasoning persistence + Neo4j index scoping.** Cheap, already-disclosed follow-ups worth
   scheduling before they compound.
8. **§3.7 — fan the loop into narrow specialized sub-passes.** Highest long-run ceiling on both quality and
   latency (parallel + narrower context each), but the largest engineering lift — treat as a longer-horizon
   architecture bet, prototyped behind the existing preset mechanism.

## Sources consulted

- Project ADRs (primary source): 0032, 0040–0049, 0053–0054, 0060–0062, 0065–0066, 0069–0070, 0073, 0086,
  0089–0091, 0097, 0103, 0114, plus `CLAUDE.md` and `README.md`.
- [How CodeRabbit brings AI to code reviews — InfoWorld](https://www.infoworld.com/article/4025088/how-coderabbit-brings-ai-to-code-reviews.html)
- [CodeRabbit Architecture docs](https://docs.coderabbit.ai/overview/architecture)
- [Explainable AI Code Reviews: Inside CodeRabbit's Context Engine](https://www.coderabbit.ai/blog/explainable-reviews-coderabbit-review-context-engine)
- [cubic blog: Bugbot alternatives — the Planner/Security/Duplication/Editorial split, -51% false positives](https://www.cubic.dev/blog/the-3-best-bugbot-alternatives-for-ai-code-review-in-2025)
- [Best AI Code Review Tools 2026 Compared (Greptile vs CodeRabbit catch-rate/FP-rate numbers)](https://levelop.dev/blog/best-ai-code-review-tools-2026-coderabbit-greptile-qodo-compared)
- QASecClaw: A Multi-Agent LLM Approach for False Positive Reduction in Static Application Security Testing — [arXiv:2605.01885](https://arxiv.org/html/2605.01885)
- Refute-or-Promote: An Adversarial Stage-Gated Multi-Agent Review Methodology — [arXiv:2604.19049](https://arxiv.org/pdf/2604.19049)
- Multi-Agent Code Verification via Information Theory — [arXiv:2511.16708](https://arxiv.org/html/2511.16708)
- Reducing False Positives in Static Bug Detection with LLMs: An Empirical Study in Industry — [arXiv:2601.18844](https://arxiv.org/pdf/2601.18844)
- Reducing Latency of LLM Search Agent via Speculation-based Algorithm-System Co-Design — [arXiv:2511.20048](https://arxiv.org/pdf/2511.20048)
- Chain-of-Verification Reduces Hallucination in Large Language Models — [arXiv:2309.11495](https://arxiv.org/pdf/2309.11495)
- SWE-PRBench: Benchmarking AI Code Review Quality Against Pull Request Feedback — [ResearchGate](https://www.researchgate.net/publication/403262187_SWE-PRBench_Benchmarking_AI_Code_Review_Quality_Against_Pull_Request_Feedback)
- Code Review Bench — [withmartian.com](https://withmartian.com/post/code-review-bench-v0)
