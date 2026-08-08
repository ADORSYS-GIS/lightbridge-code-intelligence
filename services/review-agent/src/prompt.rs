//! Review prompt assembly (ADR-0037 + ADR-0070): the system message (operator persona + the machine
//! tool-protocol) and the user message (request + file-boundary-packed diff + the static context
//! blocks). The exact text/structure is a byte-frozen contract — the golden traces lock it — so this
//! module owns it verbatim, decoupled from the runner's `ReviewConfig`/`PrDiff` via the small
//! [`PromptConfig`]/[`PrDiffRef`] param structs the host maps in at the call boundary.

use lci_agent_loop::ChatMessage;

mod diff;

/// The review-agent-owned slice of the runner's `ReviewConfig` that prompt assembly needs. The host
/// maps its config onto this at the call boundary, so `review-agent` depends on nothing in the runner.
#[derive(Debug, Clone)]
pub struct PromptConfig {
    /// The operator's reviewer *guidance* (persona + focus), from the ai-helm `config.reviewSystemPrompt`
    /// (ADR-0037 — required, no built-in default). Used verbatim; the tool-protocol is appended after it.
    pub system_prompt: String,
    /// Ceiling on the diff pasted into the prompt (the diff block's char budget; ADR-0070 may shrink it
    /// window-proportionally).
    pub max_diff_chars: usize,
    /// Model context window in tokens (ADR-0045). `Some(n)` activates the window-proportional block caps
    /// (ADR-0070); `None` = the absolute ceilings apply unchanged (legacy behaviour).
    pub context_window: Option<usize>,
    /// The resolved review preset name (`"fast"`/`"deep"`/`"ultra"`, or an operator-defined one,
    /// ADR-0103). Carries NO prompt-shaping behaviour of its own — every preset already reaches this
    /// struct pre-resolved into `max_diff_chars`/`context_window`/etc. Its only job is as the `preset`
    /// field on the prompt-budget log event ([`log_prompt_metrics`]): without it, the per-run byte
    /// accounting can't distinguish the 120k `fast` tier from the 300k `deep` tier, which is the whole
    /// point of collecting this data (deciding what to raise the cap TO, per tier).
    pub preset: String,
}

/// The PR change set as prompt assembly sees it: the unified diff + the changed-file list. Borrowed
/// (the host owns the underlying `PrDiff`), so no allocation crosses the boundary.
#[derive(Debug, Clone, Copy)]
pub struct PrDiffRef<'a> {
    /// `git diff <merge-base>..<head>` output (unified, no color).
    pub diff: &'a str,
    /// Repo-root-relative paths the PR touches — the only files a finding may land on.
    pub files: &'a [String],
}

/// The machine **tool-protocol** appended after the operator's system prompt (ADR-0037). This is the
/// only behaviour-shaping text that lives in code — it is factual and coupled to the tool API (names,
/// when to call them), NOT persona/guidance, which is operator-owned config (`review.system_prompt`,
/// from the ai-helm `config.reviewSystemPrompt`). It goes last so it's the final instruction.
const TOOL_PROTOCOL: &str = "\
# How to act\n\
Investigate with the search/graph tools before making any claim — never speculate about code you \
have not looked up. If `run_sast` is available and this is a real review (not just answering a \
question), call it EARLY — before you finish investigating — so its findings are recorded up front; do \
not re-report what it returns as your own finding, but you may investigate one of its leads further \
(confirm exploitability, trace a tainted input, note a false positive) if it's worth your budget. As \
you find issues, record each one with `add_review_comment` (one call per finding, on a line this diff \
adds or changes). Only set `start_line` when the finding's evidence genuinely spans multiple contiguous \
lines — a multi-line problem, not a multi-line explanation of a one-line problem — and if the finding \
also carries a `suggestion`, make it cover the full start_line..line range. Use `add_comment` for a \
plain reply that isn't pinned to a diff line (e.g. answering a question). Nothing you record is posted \
until you call `finish` with your overall verdict — call `finish` exactly once when you are done, even \
if you found nothing. If you genuinely cannot produce anything useful, call `abort` with a reason — \
that reason is posted verbatim as the public review note on the PR, so write one honest sentence for \
the PR author, never an internal note or scratch reasoning. You may not edit files or run commands.";

/// Conservative chars-per-token constant for the window-proportional block budgets (ADR-0070).
const PROMPT_CHARS_PER_TOKEN: usize = 4;

/// Floor for a window-derived block budget (ADR-0070): even on a tiny window a block keeps its header,
/// a few lines, and the truncation marker — a nuked-to-nothing block would silently drop the *framing*
/// ("untrusted", "don't re-report") along with the content.
const MIN_BLOCK_CHARS: usize = 1_000;

/// Absolute ceilings for the injected static context blocks (ADR-0070). Each mirrors the bound its
/// assembly side already enforces — the control plane caps the prior-reviews block at 8k
/// (`PRIOR_BLOCK_CHAR_CAP`), repo memory is a `LIMIT 30` of one-liners, and the AGENTS.md ingest is 32
/// KiB (`instructions::TOTAL_CAP`) — so on today's large-window models nothing changes. The
/// window-proportional share below can only SHRINK them, never grow them. (The SAST digest no longer has
/// a ceiling here — ADR-0073 made it a `run_sast` tool result, not a static prompt block.)
const PRIORS_BLOCK_CHAR_CEIL: usize = 8_000;
const MEMORY_BLOCK_CHAR_CEIL: usize = 4_000;
const INSTRUCTIONS_BLOCK_CHAR_CEIL: usize = 32 * 1024;
/// The repo's OWN explicit review config (`.lightbridge-code-review.jsonc`, ADR-0030) — small by
/// design (conventions/architecture/instructions prose), so a modest ceiling suffices.
const REPO_CONFIG_BLOCK_CHAR_CEIL: usize = 8_000;

/// Char budgets for the static context blocks of one run (ADR-0070). The per-block constants (and the
/// operator's `max_diff_chars`) were tuned for the current ~1M-window models; pointed at a small-window
/// model they would silently eat the whole window before the review starts (the 60k-char diff cap alone
/// is ~15k tokens). When `context_window` is set (ADR-0045 — the same knob that already drives
/// wind-down/trim), each block budget becomes `min(absolute ceiling, share-of-window)`, floored at
/// [`MIN_BLOCK_CHARS`]; with no window configured the ceilings apply unchanged (legacy behaviour).
struct PromptBudgets {
    diff: usize,
    priors: usize,
    memory: usize,
    instructions: usize,
    repo_config: usize,
}

impl PromptBudgets {
    /// Shares of the window: diff 25%, instructions 2%, priors 2%, memory 1% — together ≤ ~30% of the
    /// window for static context, leaving the rest for the system prompt, the conversation, and the
    /// ADR-0045 wind-down headroom.
    fn for_config(config: &PromptConfig) -> Self {
        let share = |frac: f64, ceil: usize| -> usize {
            match config.context_window {
                Some(window) => {
                    let chars = (window as f64 * frac) as usize * PROMPT_CHARS_PER_TOKEN;
                    ceil.min(chars.max(MIN_BLOCK_CHARS))
                }
                None => ceil,
            }
        };
        Self {
            diff: share(0.25, config.max_diff_chars),
            priors: share(0.02, PRIORS_BLOCK_CHAR_CEIL),
            memory: share(0.01, MEMORY_BLOCK_CHAR_CEIL),
            instructions: share(0.02, INSTRUCTIONS_BLOCK_CHAR_CEIL),
            repo_config: share(0.02, REPO_CONFIG_BLOCK_CHAR_CEIL),
        }
    }
}

/// Cap an injected prompt block to its budget (ADR-0070): cut char-safely on a line boundary and
/// append an explicit marker naming what was cut — the same never-truncate-silently rule as the diff
/// packing (#275) and the prior-reviews block (ADR-0065). Under budget, the block passes through
/// unchanged (borrowed).
fn cap_prompt_block<'a>(block: &'a str, budget: usize, label: &str) -> std::borrow::Cow<'a, str> {
    if block.len() <= budget {
        return std::borrow::Cow::Borrowed(block);
    }
    // `truncate_on_boundary` already walks back to a valid UTF-8 char boundary, so the byte slice below
    // never splits a multi-byte char (and `\n` is single-byte ASCII, so `[..=i]` stays on a boundary).
    let cut = truncate_on_boundary(block, budget);
    // Prefer a line boundary so the cut never leaves half a finding/sentence dangling — but only when
    // it keeps most of the budget. A block with sparse newlines (e.g. one giant line) could otherwise
    // have its last `\n` near the start, throwing away nearly the whole budget; below the halfway mark
    // we keep the full char-safe cut instead (gemini review on #280).
    let cut = match cut.rfind('\n') {
        Some(i) if i >= budget / 2 => &cut[..=i],
        _ => cut,
    };
    std::borrow::Cow::Owned(format!(
        "{cut}\n… [{label} truncated to fit the model's context window — {} of {} chars shown]\n",
        cut.len(),
        block.len(),
    ))
}

/// `s` truncated to at most `max` bytes, never slicing through a multi-byte char.
fn truncate_on_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ─── Prompt-budget accounting ───────────────────────────────────────────────────────────────────
//
// The operator wants to raise `review.max_diff_chars` (120k on `fast`, 300k on `deep` in prod) but
// needs evidence of how many bytes are actually being thrown away, not just how often the cap binds
// — "raise it to what?" needs a distribution, not a boolean. [`log_prompt_metrics`] below is that
// evidence: it emits one structured log event per run with the full byte/file breakdown, and the
// runner's stdout already reaches Loki, so querying and percentile-bucketing across runs is a log
// query away. (An earlier version of this also recorded OTel histograms; that path was removed once
// it was confirmed the production Grafana Alloy collector drops delta-temporality metrics and the
// fix — `otelcol.processor.deltatocumulative` — is an experimental component the cluster's sole
// collector can't be lowered to run. The log event was always the primary sink for the high-
// cardinality per-run detail, so nothing here changed shape, only the now-dead second sink went away.)

/// Per-run diff byte/file accounting handed to [`log_prompt_metrics`] — a straight relabelling of
/// [`diff::RenderedDiff`]'s fields plus the budget it was packed against, computed once in
/// [`build_messages`] where the raw diff and [`PromptBudgets`] are both in scope.
struct DiffMetrics {
    source_bytes: usize,
    rendered_bytes: usize,
    budget_bytes: usize,
    omitted_bytes: usize,
    total_files: usize,
    rendered_files: usize,
    omitted_files: usize,
    low_signal_files: usize,
}

/// Per-run byte accounting for one of the four static context blocks (priors/memory/instructions/
/// repo_config), recorded only when that block was actually present on this run (`None` blocks record
/// nothing — an absent block isn't a zero-byte data point, it's not a data point).
struct BlockMetrics {
    /// The generic-block field value: `"priors"`, `"memory"`, `"instructions"`, or `"repo_config"`.
    block: &'static str,
    input_bytes: usize,
    rendered_bytes: usize,
    budget_bytes: usize,
}

/// Emit one run's prompt-budget byte/file accounting as a structured log event. `preset` distinguishes
/// tiers; the runner's stdout already reaches Loki, so this event is queryable per-run (sliceable by
/// repo/PR via the surrounding span) immediately — the sole sink for this data (see the "Prompt-budget
/// accounting" section above). Matches the style of the existing ADR-0070 "window-proportional caps
/// active" log below.
fn log_prompt_metrics(preset: &str, diff: Option<&DiffMetrics>, blocks: &[BlockMetrics]) {
    let block = |name: &str| blocks.iter().find(|b| b.block == name);
    let priors = block("priors");
    let memory = block("memory");
    let instructions = block("instructions");
    let repo_config = block("repo_config");

    tracing::info!(
        preset,
        diff_source_bytes = diff.map(|d| d.source_bytes),
        diff_rendered_bytes = diff.map(|d| d.rendered_bytes),
        diff_budget_bytes = diff.map(|d| d.budget_bytes),
        diff_omitted_bytes = diff.map(|d| d.omitted_bytes),
        diff_files_total = diff.map(|d| d.total_files),
        diff_files_rendered = diff.map(|d| d.rendered_files),
        diff_files_omitted = diff.map(|d| d.omitted_files),
        diff_files_low_signal = diff.map(|d| d.low_signal_files),
        priors_input_bytes = priors.map(|b| b.input_bytes),
        priors_rendered_bytes = priors.map(|b| b.rendered_bytes),
        priors_budget_bytes = priors.map(|b| b.budget_bytes),
        memory_input_bytes = memory.map(|b| b.input_bytes),
        memory_rendered_bytes = memory.map(|b| b.rendered_bytes),
        memory_budget_bytes = memory.map(|b| b.budget_bytes),
        instructions_input_bytes = instructions.map(|b| b.input_bytes),
        instructions_rendered_bytes = instructions.map(|b| b.rendered_bytes),
        instructions_budget_bytes = instructions.map(|b| b.budget_bytes),
        repo_config_input_bytes = repo_config.map(|b| b.input_bytes),
        repo_config_rendered_bytes = repo_config.map(|b| b.rendered_bytes),
        repo_config_budget_bytes = repo_config.map(|b| b.budget_bytes),
        "prompt budget usage: per-block byte accounting (informs max_diff_chars sizing)"
    );
}

/// Assemble the system (operator prompt + tool-protocol) and user (request + diff + static context)
/// messages for one review run. The system prompt is the **required** operator-owned guidance
/// (ADR-0037 — no built-in default); the tool-protocol is appended last so it's the final instruction
/// the model sees. Returns the engine's [`ChatMessage`] type directly.
///
/// The exact text/structure here is a byte-frozen contract (the golden traces lock it); do not reword.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn build_messages(
    config: &PromptConfig,
    command: &str,
    diff: Option<PrDiffRef<'_>>,
    repo_instructions: Option<&str>,
    prior_reviews: Option<&str>,
    repo_memory: Option<&str>,
    repo_config_context: Option<&str>,
) -> Vec<ChatMessage> {
    let system = format!("{}\n\n{TOOL_PROTOCOL}", config.system_prompt);

    // Window-proportional budgets for the static blocks (ADR-0070). Log once when the window actually
    // shrank something below its ceiling, so a small-window deploy is legible from the run log.
    let budgets = PromptBudgets::for_config(config);
    if config.context_window.is_some()
        && (budgets.diff < config.max_diff_chars
            || budgets.priors < PRIORS_BLOCK_CHAR_CEIL
            || budgets.memory < MEMORY_BLOCK_CHAR_CEIL
            || budgets.instructions < INSTRUCTIONS_BLOCK_CHAR_CEIL
            || budgets.repo_config < REPO_CONFIG_BLOCK_CHAR_CEIL)
    {
        tracing::info!(
            context_window = config.context_window,
            diff_chars = budgets.diff,
            priors_chars = budgets.priors,
            memory_chars = budgets.memory,
            instructions_chars = budgets.instructions,
            repo_config_chars = budgets.repo_config,
            "prompt budgets: window-proportional caps active (ADR-0070)"
        );
    }

    // Byte accounting for the prompt-budget log event emitted at the end of this function (below), once
    // every number is in scope. `diff_metrics` stays `None` on a diff-less (`ask`) run — nothing to
    // measure. `block_metrics` collects one entry per STATIC block actually present on this run; an
    // absent block records nothing (it isn't a zero-byte data point, it's not a data point).
    let mut diff_metrics: Option<DiffMetrics> = None;
    let mut block_metrics: Vec<BlockMetrics> = Vec::new();

    let mut user = format!("The maintainer's request: {command}");
    match diff {
        Some(pr) => {
            user.push_str(&format!(
                "\n\nThis pull request changes {} file(s):\n{}",
                pr.files.len(),
                pr.files
                    .iter()
                    .map(|f| format!("- {f}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ));
            // File-boundary packing (ADR-0062): whole per-file sections, source first, lock/generated
            // noise deprioritised — never a mid-hunk byte cut. A byte cut on PR #274 hid a function
            // definition whose call site *was* visible, so the pass filed a P1 on code it never saw, while
            // 55% of the PR (every file past the cut) was silently absent. Anything not shown is disclosed
            // below so the model states honest coverage and can't fault unseen code.
            let rendered = diff::render_diff_for_prompt(pr.diff, budgets.diff);
            user.push_str("\n\nUnified diff (review ONLY lines this diff changes):\n```diff\n");
            user.push_str(&rendered.text);
            user.push_str("\n```");
            if !rendered.low_signal.is_empty() {
                user.push_str(&format!(
                    "\n\nAlso changed but NOT shown above (generated/lock files — low review signal): {}.",
                    rendered.low_signal.join(", ")
                ));
            }
            if !rendered.omitted_for_budget.is_empty() {
                user.push_str(&format!(
                    "\n\n⚠️ {} changed file(s) did NOT fit the prompt budget and are NOT shown above: {}. \
                     You have not seen these changes — do NOT raise a finding about their contents, and do \
                     NOT assume they are correct or incorrect. If a line you *can* see depends on one of \
                     them, treat that as an unverifiable question (at most P2), not a defect. State plainly \
                     in your verdict which files you could not review.",
                    rendered.omitted_for_budget.len(),
                    rendered.omitted_for_budget.join(", ")
                ));
            }
            // "Source diff bytes" — the real demand — is the raw diff minus whatever was set aside as
            // low-signal (`rendered.low_signal_bytes`, which is `0` in the lockfile-only-PR edge case,
            // since then the low-signal files ARE the source). This is the number that answers "raise
            // max_diff_chars to what": everything a bigger cap could plausibly let through.
            diff_metrics = Some(DiffMetrics {
                source_bytes: pr.diff.len() - rendered.low_signal_bytes,
                rendered_bytes: rendered.text.len(),
                budget_bytes: budgets.diff,
                omitted_bytes: rendered.omitted_for_budget_bytes,
                total_files: rendered.total_files,
                rendered_files: rendered.rendered_files,
                omitted_files: rendered.omitted_for_budget_files,
                low_signal_files: rendered.low_signal_files,
            });
        }
        None => user.push_str(
            "\n\nNo diff is available for this run; answer or review against the working tree and \
             keep every claim grounded in the tools.",
        ),
    }

    // Prior-review context (ADR-0040 + ADR-0065): the agent's own prior reviews of this target,
    // pre-formatted control-plane-side as explicitly-UNTRUSTED context. The block's framing is
    // re-derive-then-reconcile: review the diff independently first, then retract any prior finding that
    // can't be re-derived (Option C) — it does NOT tell the agent to restate priors. All reconcile
    // wording lives in that string; the runner injects it verbatim. Placed after the diff (the thing
    // under review) and before the repo's own instructions; the tool-protocol in the system message stays
    // authoritative. `None` on a first review, so a fresh PR reads exactly as before.
    if let Some(prior) = prior_reviews {
        user.push_str("\n\n");
        let rendered = cap_prompt_block(prior, budgets.priors, "prior-reviews context");
        block_metrics.push(BlockMetrics {
            block: "priors",
            input_bytes: prior.len(),
            rendered_bytes: rendered.len(),
            budget_bytes: budgets.priors,
        });
        user.push_str(&rendered);
    }

    // Per-repo feedback memory (M1, ADR-0044): findings rejected (👎) here before — untrusted context,
    // same as the prior review; the tool-protocol stays authoritative. `None` keeps a clean-repo run
    // reading exactly as before.
    if let Some(memory) = repo_memory {
        user.push_str("\n\n");
        let rendered = cap_prompt_block(memory, budgets.memory, "repo feedback memory");
        block_metrics.push(BlockMetrics {
            block: "memory",
            input_bytes: memory.len(),
            rendered_bytes: rendered.len(),
            budget_bytes: budgets.memory,
        });
        user.push_str(&rendered);
    }

    // The repo's OWN explicit review config (`.lightbridge-code-review.jsonc`, ADR-0030): conventions/
    // architecture/instructions the repo owner deliberately declared for review. TRUSTED, explicit
    // config — unlike `repo_instructions` below (AGENTS.md-style prose folded in with an untrusted-
    // content guardrail), this is data the repo owner opted into for exactly this purpose, so it carries
    // no such header. Placed before `repo_instructions` (config first, ambient convention prose second).
    if let Some(repo_config) = repo_config_context {
        user.push_str("\n\n");
        let rendered = cap_prompt_block(
            repo_config,
            budgets.repo_config,
            "repository review configuration",
        );
        block_metrics.push(BlockMetrics {
            block: "repo_config",
            input_bytes: repo_config.len(),
            rendered_bytes: rendered.len(),
            budget_bytes: budgets.repo_config,
        });
        user.push_str(&rendered);
    }

    // Repo-native agent instructions (ADR-0036), kept in the user message as untrusted context (it is
    // already labelled and the tool-protocol/mission in the system message stays authoritative).
    if let Some(instructions) = repo_instructions {
        user.push_str("\n\n");
        let rendered = cap_prompt_block(
            instructions,
            budgets.instructions,
            "repository agent instructions",
        );
        block_metrics.push(BlockMetrics {
            block: "instructions",
            input_bytes: instructions.len(),
            rendered_bytes: rendered.len(),
            budget_bytes: budgets.instructions,
        });
        user.push_str(&rendered);
    }

    // Prompt-budget structured log (see the "Prompt-budget accounting" section above): every number is
    // in scope here, at the end of assembly, after every block has made its capping decision.
    log_prompt_metrics(&config.preset, diff_metrics.as_ref(), &block_metrics);

    vec![ChatMessage::system(system), ChatMessage::user(user)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt_config() -> PromptConfig {
        PromptConfig {
            system_prompt: "You are a reviewer.".to_string(),
            max_diff_chars: 60_000,
            context_window: None,
            preset: "fast".to_string(),
        }
    }

    // The maintainer's request reaches the user prompt; the operator system prompt is used verbatim.
    #[test]
    fn build_messages_carries_request_and_uses_operator_prompt() {
        let config = prompt_config();
        let msgs = build_messages(
            &config,
            "propose a better implementation",
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "system");
        let system = msgs[0].content.as_deref().expect("system content");
        assert!(
            system.starts_with("You are a reviewer."),
            "operator prompt first"
        );
        assert!(system.contains("How to act"), "tool-protocol appended");
        let user = msgs[1].content.as_deref().expect("user content");
        assert!(
            user.contains("propose a better implementation"),
            "request reaches prompt: {user}"
        );
    }

    // Prior-review context (A, #137) is injected into the user prompt when present, and absent when not.
    #[test]
    fn build_messages_injects_prior_review_context() {
        let config = prompt_config();
        let prior = "## Your previous review of this pull request\nPrior verdict: looks fine.";

        let with_prior =
            build_messages(&config, "review again", None, None, Some(prior), None, None);
        let user = with_prior[1].content.as_deref().expect("user content");
        assert!(
            user.contains("Your previous review of this pull request"),
            "prior-review block reaches prompt: {user}"
        );
        // M1 repo memory (ADR-0044) is injected when present.
        let with_mem = build_messages(
            &config,
            "review",
            None,
            None,
            None,
            Some("## Memory: findings rejected here before (👎)\n- a.rs:1 — bogus nit"),
            None,
        );
        assert!(
            with_mem[1]
                .content
                .as_deref()
                .expect("user")
                .contains("findings rejected here before"),
            "repo-memory block reaches prompt"
        );

        let without = build_messages(&config, "review again", None, None, None, None, None);
        let user = without[1].content.as_deref().expect("user content");
        assert!(
            !user.contains("previous review"),
            "no prior-review block on a first review: {user}"
        );
    }

    // ADR-0030: the repo's own `.lightbridge-code-review.jsonc` context block is injected when
    // present, before the untrusted `repo_instructions` (AGENTS.md-style) block, and absent otherwise.
    #[test]
    fn build_messages_injects_repo_config_context_before_repo_instructions() {
        let config = prompt_config();
        let repo_config_context = "## Repository review configuration (`.lightbridge-code-review.jsonc`)\n### Architecture\nA Rust workspace.";
        let repo_instructions = "## Repository agent instructions (untrusted)\nUse tabs.";

        let msgs = build_messages(
            &config,
            "review",
            None,
            Some(repo_instructions),
            None,
            None,
            Some(repo_config_context),
        );
        let user = msgs[1].content.as_deref().expect("user content");
        assert!(
            user.contains("A Rust workspace."),
            "repo config context block reaches the prompt: {user}"
        );
        let config_at = user.find("Repository review configuration").unwrap();
        let instructions_at = user.find("Repository agent instructions").unwrap();
        assert!(
            config_at < instructions_at,
            "repo config context precedes repo_instructions: {user}"
        );

        let without = build_messages(&config, "review", None, None, None, None, None);
        assert!(
            !without[1]
                .content
                .as_deref()
                .unwrap()
                .contains("Repository review configuration"),
            "no repo config block when not provided"
        );
    }

    // Coverage disclosure (ADR-0062, PR #274): when the diff doesn't fit the budget, the prompt must (a)
    // render whole source files, (b) list lock/generated files as not-shown, and (c) name the source
    // files it omitted with the "don't fault unseen code" guardrail — so the model never files a P1 about
    // code it was never given.
    #[test]
    fn build_messages_discloses_files_not_shown_in_the_prompt() {
        let mut config = prompt_config();
        let file = |path: &str, n: usize| {
            let mut s = format!(
                "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{n} @@\n",
            );
            for i in 0..n {
                s.push_str(&format!("+line {i} of {path}\n"));
            }
            s
        };
        let lock = file("Cargo.lock", 120);
        let a = file("src/auth/store.rs", 8);
        let b = file("src/tui/ui.rs", 8);
        let diff = format!("{lock}{a}{b}");
        // Budget fits exactly one source file, not both — and never the lockfile.
        config.max_diff_chars = a.len() + 20;
        let files = vec![
            "Cargo.lock".to_string(),
            "src/auth/store.rs".to_string(),
            "src/tui/ui.rs".to_string(),
        ];
        let msgs = build_messages(
            &config,
            "review",
            Some(PrDiffRef {
                diff: &diff,
                files: &files,
            }),
            None,
            None,
            None,
            None,
        );
        let user = msgs[1].content.as_deref().expect("user content");

        assert!(
            user.contains("+line 0 of src/auth/store.rs"),
            "the first source file renders whole: {user}"
        );
        assert!(
            !user.contains("+line 0 of Cargo.lock"),
            "the lockfile is never rendered into the diff"
        );
        assert!(
            user.contains("generated/lock files") && user.contains("Cargo.lock"),
            "lockfile is disclosed as not-shown: {user}"
        );
        assert!(
            user.contains("did NOT fit the prompt budget")
                && user.contains("src/tui/ui.rs")
                && user.contains("at most P2"),
            "budget-omitted source file is disclosed with the P2 guardrail: {user}"
        );
    }

    // Window-proportional block caps (ADR-0070): a large static block passes through verbatim with no
    // window, but is cut + disclosed once a small `context_window` shrinks its share below the ceiling.
    #[test]
    fn build_messages_caps_static_blocks_to_the_window() {
        let mut config = prompt_config();
        // ~5k chars: under the 8k absolute ceiling (so the no-window case passes it through verbatim),
        // over a small window's 2% share (so the windowed case cuts it).
        let long_prior = format!(
            "## Prior automated reviews\n{}",
            "- [P2] some/file.rs:10 — an earlier finding title\n".repeat(100)
        );

        config.context_window = None;
        let msgs = build_messages(&config, "review", None, None, Some(&long_prior), None, None);
        let user = msgs[1].content.as_deref().unwrap();
        assert!(
            user.contains(&long_prior),
            "no window → the block is injected verbatim"
        );

        config.context_window = Some(8_192);
        let msgs = build_messages(&config, "review", None, None, Some(&long_prior), None, None);
        let user = msgs[1].content.as_deref().unwrap();
        assert!(
            !user.contains(&long_prior),
            "small window → the block was cut"
        );
        assert!(
            user.contains("prior-reviews context truncated to fit the model's context window"),
            "…and the cut is disclosed: {}",
            &user[user.len().saturating_sub(400)..]
        );
    }
}
