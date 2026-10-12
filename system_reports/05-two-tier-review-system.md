# Two-Tier Review System

This report provides a detailed breakdown of the Two-Tier Review architecture in the Lightbridge Code Intelligence platform. The system operates in two distinct modes—Fast and Deep—to balance speed, cost, and analytical depth based on the trigger context.

## 1. Why Two Tiers?

Running a full, context-aware, multi-turn LLM loop on every single pull request update is computationally expensive and slow. Most PRs need a quick sanity check (e.g., "Did I break anything obvious? Are there security issues?"). However, complex refactoring or architectural changes require deep, repository-aware investigation.

To address this, Lightbridge splits its review into two tiers:
- **Fast Tier:** Optimized for speed and cost.
- **Deep Tier:** Optimized for comprehensive, structural understanding.

## 2. Triggering Mechanism

The tier is selected automatically by the control plane (`services/control-plane/src/http/webhook.rs`) when a webhook arrives from GitHub or GitLab.

- **Fast Tier Trigger:** `pull_request` `opened`
  - *Behavior:* Automatically runs when a new PR is opened.
- **Deep Tier Trigger:** `issue_comment` `created` leading with `@<handle>`
  - *Behavior:* Runs on-demand when a maintainer requests it via a comment mention.

## 3. Tier Configuration & Capabilities

The agent runner resolves the configuration for the selected tier and restricts or grants capabilities accordingly.

### The Fast Tier (`tier: "fast"`)
- **Focus:** Quick diff analysis and deterministic security scanning.
- **Model:** Uses a cheaper, faster LLM.
- **Retrieval:** **None**. The agent is restricted to viewing only the PR diff. It cannot search the semantic index or traverse the knowledge graph.
- **Turn Budget:** Clamped to a small maximum (e.g., 5 turns).
- **Quality Gates:** Bypasses complex gates like the full-diff coverage gate and refute pass, as its scope is naturally limited by the diff.
- **SAST:** Runs deterministic static analysis (`opengrep`) over changed files.
- **Output:** Posts a consolidated review quickly (≲ 2 minutes). To set expectations, the control plane injects a specific banner: `> 🅵 **Fast automated pass**`.

### The Deep Tier (`tier: "deep"`)
- **Focus:** Deep architectural investigation and cross-file impact analysis.
- **Model:** Uses a frontier/stronger LLM.
- **Retrieval:** **Full Access**. The agent can use semantic search (`lightbridge_vector_semantic_search`), structural graph queries (`lightbridge_graph_find_symbol`, `lightbridge_graph_get_callers`), and read raw files (`read_file`).
- **Turn Budget:** High maximum (e.g., 40 turns), allowing for extensive multi-step reasoning.
- **Quality Gates:** 
  - **Coverage Gate:** Must comment on or explicitly acknowledge all changed files. If it attempts to finish prematurely, the control plane bounces the request back (up to 3 times).
  - **Refute Pass:** High-severity findings (P0/P1) trigger a forced re-verification pass where the agent must substantiate its claims against the code or retract them.
- **SAST:** Available as a tool to execute.
- **Output:** Posts a comprehensive, repository-aware review. Due to the deep retrieval and multi-turn nature, this can run asynchronously for a longer duration.

## 4. Architectural Implementation

The tier configuration is enforced rigidly:
- **Tool Allowlist:** Defined via a closed enum `ReviewTool`. Unknown tools in the `review.<tier>.tools` config block will fail deserialization.
- **System Prompt:** Distinct prompts exist for each tier (`review-system-fast.md` vs. `review-system.md`), ensuring the agent's persona aligns with its available capabilities.
- **State Management:** The internal API strictly enforces the capabilities based on the task's tier, ensuring the fast tier cannot accidentally execute an expensive graph query.

## 5. Summary

The Two-Tier system ensures Lightbridge remains responsive and cost-effective for everyday PRs while maintaining the capability to perform expert-level, repository-wide architectural reviews when explicitly requested by human maintainers.
