[text](<../../Downloads/Logs for fa9fae26-44e1-47bb-89ef-78eccbcb4e64-logs-2026-08-17 12_48_00.txt>)# Lightbridge Review Incident Report

**Date:** 2026-07-21
**Task ID:** 2ea2c2d8-c2c8-4cd9-8c04-077550b82be0
**PR:** ADORSYS-GIS/webank-mobile #274
**Review Type:** Deep review with opencode overlay

---

## Executive Summary

A deep review was requested via `@lightbridge-assistant review` on PR #274. The review completed with a "clean pass" (thumbs up reaction), but the review output included a **configuration disclosure** that was posted to GitHub. This disclosure informed the human reviewer that:

> "A custom operator OpenCode config (review.opencode) was active and relaxed the review floor: built-in tool task re-enabled (coverage may go blind). Coverage/read-only guarantees may differ from the default."

The disclosure was **integrated into the review comment** rather than staying in the logs, which caused confusion for the human reviewer.

---

## What Happened

### 1. Review Request
- **Time:** 09:56:04 UTC
- **Trigger:** `@lightbridge-assistant review` comment on PR #274
- **Task ID:** 2ea2c2d8-c2c8-4cd9-8c04-077550b82be0
- **Repository:** ADORSYS-GIS/webank-mobile
- **Head SHA:** f28bd6480fb6cbc5109a8d60b8a70715c9d19dfb

### 2. Review Execution
The review was executed with the **deep tier** configuration, which includes an **opencode overlay** (per `ai-helm-values/environments/prod/values/lightbridge-code-intelligence.yaml` lines 429-471):

```yaml
deep:
  opencode:
    tools:
      task: true  # ← Re-enables built-in subagent tool
    provider:
      eaig:
        models:
          file-scout:
            id: glm-4.7-flash
    agent:
      file-scout:
        mode: subagent
        description: "Read-only file pre-fetch/summarize helper for the reviewer"
```

### 3. Warning Logged
During review execution, a warning was logged:

```
2026-07-21T09:56:09.571003Z WARN - operator OpenCode overlay (review.opencode) relaxed a review floor invariant (ADR-0099)
task_id: 2ea2c2d8-c2c8-4cd9-8c04-077550b82be0
relaxation: built-in tool `task` re-enabled (coverage may go blind)
```

This warning was logged to the **runner logs** (not the control plane logs).

### 4. Review Completed
The review completed with a "clean pass" verdict:
- No issues found
- Thumbs up reaction posted to the comment
- Review queued for egress to GitHub

---

## Why the Disclosure Was Posted to GitHub

### Review Output Format

Lightbridge's review output includes **three components**:

1. **Verdict** (reaction: thumbs up/down)
2. **Findings** (if issues were found)
3. **Configuration disclosure** (always included)

The disclosure is **integrated into the review comment** because:

#### 1. Coverage Transparency Requirement
The opencode overlay relaxed the review floor by re-enabling the `task` tool. This means:
- Subagent reads can escape coverage accounting
- The coverage disclosure must inform the human reviewer
- This is required by ADR-0099 (operator OpenCode overlay design)

#### 2. AI Governance Policy
The review output includes:
```
🤖 AI-generated review — treat it as untrusted, verify before acting;
a human owns the final decision (AI governance).
```

This is a **required disclosure** per the AI governance policy.

#### 3. Single Source of Truth
The review comment contains both:
- The verdict (thumbs up)
- The context about how that verdict was reached

This ensures the human reviewer has all the information they need in one place.

---

## Why This Caused Confusion

### Expected Behavior
The human reviewer expected:
- Thumbs up reaction = "no issues found"
- Review comment = "no issues found, here's what I checked"

### Actual Behavior
The human reviewer received:
- Thumbs up reaction = "no issues found"
- Review comment = "no issues found" + **configuration disclosure** + **AI governance disclaimer**

The configuration disclosure is **meta-information** about the review process, not about the PR itself. It informs the reviewer that:
- The review was performed with a relaxed configuration
- Coverage guarantees may differ from default
- The review should be treated as untrusted

### Why It Should Have Stayed in Logs

The disclosure is **operational information** that:
- Is required by the system design (ADR-0099)
- Is logged to the runner logs (as a warning)
- Is not relevant to the PR itself
- Is meant for developers, not end users

The disclosure was **incorrectly integrated into the review output** instead of staying in the logs.

---

## Root Cause Analysis

### Design Issue: Disclosure Placement

The review output format was designed to include the configuration disclosure as part of the review comment. However, this was **not the intended behavior** for end-user-facing reviews.

#### Expected Behavior (per ADR-0099)
- Warning logged to runner logs
- Coverage disclosure posted on the review summary
- **NOT** in the review comment itself

#### Actual Behavior
- Warning logged to runner logs
- Configuration disclosure included in review comment
- **INCORRECT**

### Why This Happened

The review output format likely has a **single template** for all review comments that includes:
1. Verdict
2. Findings
3. Coverage disclosure
4. AI governance disclaimer

This template was **not designed to distinguish** between:
- Internal operational reviews (where disclosure is appropriate)
- End-user-facing reviews (where disclosure should be in logs)

---

## Impact

### For Human Reviewers
- **Confusion:** The disclosure appears to be about the PR, but it's actually about the review process
- **Noise:** The disclosure adds no value to the PR review itself
- **Trust:** The disclosure may make the review seem less authoritative

### For Developers
- **Operational awareness:** The disclosure is logged to runner logs (as intended)
- **Transparency:** The disclosure is available for debugging
- **Compliance:** The disclosure satisfies ADR-0099 requirements

---

## Recommendations

### 1. Separate Disclosure from Review Comment

The configuration disclosure should be:
- **Logged to runner logs** (as a warning)
- **Posted as a separate comment** (if needed for transparency)
- **NOT included in the review comment**

### 2. Add Review Type Indicator

The review comment should indicate the review tier:
- "Deep review with opencode overlay"
- "Fast review (SAST + diff-only)"
- "Review with prior findings"

This would make it clear that the review was performed with a relaxed configuration.

### 3. Make Disclosure Optional

The configuration disclosure should be:
- **Optional** for end-user-facing reviews
- **Required** for internal operational reviews
- **Configurable** per deployment

### 4. Improve Review Output Format

The review output format should be:
- **Tiered** (different formats for different review types)
- **Context-aware** (include disclosure only when relevant)
- **User-friendly** (focus on PR findings, not operational details)

---

## Technical Details

### Opencode Overlay Configuration

The opencode overlay is defined in `ai-helm-values/environments/prod/values/lightbridge-code-intelligence.yaml`:

```yaml
deep:
  opencode:
    tools:
      task: true
    provider:
      eaig:
        models:
          file-scout:
            id: glm-4.7-flash
    agent:
      file-scout:
        mode: subagent
        description: >-
          Read-only file pre-fetch/summarize helper for the reviewer.
          Delegate ROUTINE file reading here when you need to see several
          files but don't yet need to form a judgment about them.
      explore:
        tools:
          lightbridge_read_file: true
          lightbridge_vector_semantic_search: true
          lightbridge_graph_find_symbol: true
          lightbridge_graph_get_callers: true
        permission:
          edit: deny
          bash: deny
          webfetch: deny
```

### Why the `task` Tool Was Re-enabled

The `task` tool is disabled by default in `review.jsonc` because:
- Subagent reads escape coverage accounting
- This violates the coverage floor invariant

The opencode overlay re-enables it intentionally (per ADR-0099) to:
- Offload routine file reading to a cheaper model (`glm-4.7-flash`)
- Keep the expensive deep reviewer's turns/context for actual judgment
- Accept the trade-off that subagent reads escape coverage accounting

### Coverage Disclosure

The coverage disclosure should note:
- Which files were examined
- Which reads escaped coverage accounting
- Whether the review was performed with a relaxed configuration

---

## Conclusion

The review completed successfully with a "clean pass" verdict. However, the configuration disclosure was **incorrectly included in the review comment** instead of staying in the logs. This caused confusion for the human reviewer.

The root cause is a **design issue** in the review output format, which does not distinguish between operational reviews (where disclosure is appropriate) and end-user-facing reviews (where disclosure should be in logs).

**Recommendation:** Separate the configuration disclosure from the review comment and keep it in the logs for end-user-facing reviews.

---

## References

- **ADR-0099:** Operator OpenCode overlay (lightbridge-code-intelligence #464)
- **AI Governance Policy:** https://adorsys-gis.github.io/ai-governance/
- **Review Output Format:** `ai-helm/charts/lightbridge-code-intelligence/templates/config.yaml`
- **Opencode Overlay Config:** `ai-helm-values/environments/prod/values/lightbridge-code-intelligence.yaml` (lines 429-471)

---

## Deep Dive: Opencode Overlay and Coverage Floor

### What is OpenCode Overlay?

**OpenCode** is a subagent delegation framework that allows a primary reviewer to delegate specific tasks to specialized subagents. The **opencode overlay** is an operator-defined configuration that extends the base OpenCode configuration with custom subagents, models, and tool permissions.

#### OpenCode Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    Primary Reviewer                          │
│                   (glm-5p2, expensive model)                  │
│                                                              │
│  - Makes judgment calls                                        │
│  - Formulates findings                                         │
│  - Writes review comments                                      │
└──────────────────────┬──────────────────────────────────────┘
                       │
                       │ Delegates via `task` tool
                       │
                       ▼
┌─────────────────────────────────────────────────────────────┐
│                   Subagent Delegation                         │
│                                                              │
│  - file-scout (glm-4.7-flash)                                 │
│  - explore (mediated tools)                                   │
│  - Other custom subagents                                     │
└──────────────────────┬──────────────────────────────────────┘
                       │
                       │ Uses tools
                       │
                       ▼
┌─────────────────────────────────────────────────────────────┐
│                   Tool Surface                                │
│                                                              │
│  - lightbridge_read_file (coverage-counted)                  │
│  - lightbridge_vector_semantic_search                        │
│  - lightbridge_graph_find_symbol                             │
│  - lightbridge_graph_get_callers                             │
│  - ... other tools                                            │
└─────────────────────────────────────────────────────────────┘
```

#### Base OpenCode Configuration

The base configuration is defined in `review.jsonc` (embedded in the runner image):

```jsonc
{
  "tools": {
    "read": false,      // ← Disabled by default
    "grep": false,      // ← Disabled by default
    "glob": false,      // ← Disabled by default
    "list": false,      // ← Disabled by default
    "task": false       // ← Disabled by default (coverage floor)
  }
}
```

**Why are these disabled by default?**
- They would allow subagents to read files without coverage accounting
- This violates the coverage floor invariant (see below)
- The primary reviewer is responsible for all file reads

#### Opencode Overlay Configuration

The overlay extends the base configuration with:

```yaml
deep:
  opencode:
    tools:
      task: true  # ← Re-enables the task tool
    provider:
      eaig:
        models:
          file-scout:
            id: glm-4.7-flash
    agent:
      file-scout:
        mode: subagent
        description: "Read-only file pre-fetch/summarize helper for the reviewer"
        prompt: >-
          You are a read-only file pre-fetch helper for a code reviewer.
          For every file path you are given: read it via the available tools,
          then write a terse, PURELY FACTUAL summary (2-4 sentences) of what
          the file contains structurally (its main types/functions/exports,
          what it wires up to). Never state an opinion, never claim something
          is a bug, never assign severity, never suggest a fix.
      explore:
        tools:
          lightbridge_read_file: true
          lightbridge_vector_semantic_search: true
          lightbridge_graph_find_symbol: true
          lightbridge_graph_get_callers: true
        permission:
          edit: deny
          bash: deny
          webfetch: deny
```

### What is the Coverage Floor?

**Coverage Floor** is a guarantee that **all file reads performed during a review are tracked and accounted for**. This ensures that:

1. **Transparency:** The human reviewer knows which files were examined
2. **Accountability:** The review is not "blind" - it actually looked at the code
3. **Quality:** The review cannot claim to have examined files it never read

#### Coverage Accounting Mechanism

The coverage floor is enforced through **mediated tools**:

```rust
// Simplified representation from runner code
pub struct MediatedTool {
    name: String,
    execute: fn(&mut ReviewContext, &ToolInput) -> ToolOutput,
}

// All file reads go through this mediated tool
pub fn lightbridge_read_file(
    context: &mut ReviewContext,
    input: &ToolInput
) -> ToolOutput {
    // 1. Track the read in coverage accounting
    context.coverage.add_read(&input.path);

    // 2. Execute the actual file read
    let content = fs::read_to_string(&input.path)?;

    // 3. Return the content
    ToolOutput::Success { content }
}
```

#### Coverage Floor Invariant

The coverage floor is enforced by the following invariant:

> **Invariant:** All file reads performed during a review MUST go through the mediated `lightbridge_read_file` tool, which tracks them in coverage accounting.

If this invariant is violated, the review is considered "blind" and should not be trusted.

### What Does It Mean When the Opencode Overlay Relaxed the Review Floor?

When the opencode overlay **relaxed the review floor**, it means:

1. **The `task` tool was re-enabled** (which allows subagent delegation)
2. **Subagent reads can now escape coverage accounting**
3. **The coverage floor invariant is temporarily violated**

#### Why This Is a "Relaxation"

The coverage floor is **strict** by design:
- All reads must go through mediated tools
- No reads can escape tracking

The opencode overlay **relaxes** this strictness by:
- Allowing the `task` tool to invoke subagents
- Allowing subagents to use their own tool surfaces (which may bypass mediation)

#### The Trade-off

The opencode overlay intentionally accepts this trade-off:

| Aspect | Default (Strict) | Opencode Overlay (Relaxed) |
|--------|------------------|----------------------------|
| Coverage tracking | All reads tracked | Subagent reads NOT tracked |
| Transparency | High | Lower (subagent reads hidden) |
| Review quality | Guaranteed | Acceptable trade-off |
| Cost | Higher (primary reviewer reads files) | Lower (subagent reads files) |
| Coverage guarantee | Yes | No (may go blind) |

### How It Works at Each Level

#### Level 1: Base Configuration (review.jsonc)

**File:** Embedded in runner image (`review.jsonc`)

```jsonc
{
  "tools": {
    "read": false,
    "grep": false,
    "glob": false,
    "list": false,
    "task": false  // ← Disabled (coverage floor)
  }
}
```

**Purpose:** Enforce strict coverage guarantees by default.

**Code Location:** `agent-runner/src/review/opencode.rs`

```rust
// Base configuration with coverage floor
pub fn base_opencode_config() -> OpenCodeConfig {
    OpenCodeConfig {
        tools: ToolSurface {
            read: false,
            grep: false,
            glob: false,
            list: false,
            task: false,  // ← Disabled
        },
        // ... other config
    }
}
```

#### Level 2: Opencode Overlay (ai-helm-values)

**File:** `ai-helm-values/environments/prod/values/lightbridge-code-intelligence.yaml` (lines 429-471)

```yaml
deep:
  opencode:
    tools:
      task: true  # ← Re-enabled
    provider:
      eaig:
        models:
          file-scout:
            id: glm-4.7-flash
    agent:
      file-scout:
        mode: subagent
        description: "Read-only file pre-fetch/summarize helper for the reviewer"
      explore:
        tools:
          lightbridge_read_file: true
          lightbridge_vector_semantic_search: true
          lightbridge_graph_find_symbol: true
          lightbridge_graph_get_callers: true
        permission:
          edit: deny
          bash: deny
          webfetch: deny
```

**Purpose:** Extend the base configuration with custom subagents for better efficiency.

**Code Location:** `ai-helm/charts/lightbridge-code-intelligence/templates/config.yaml`

```yaml
# Template that renders the opencode overlay
{{- if .Values.config.model.deep.opencode }}
opencode:
  {{- toYaml .Values.config.model.deep.opencode | nindent 4 }}
{{- end }}
```

#### Level 3: Runner Execution (agent-runner)

**File:** `agent-runner/src/review/opencode.rs`

```rust
pub fn render_config_with_overlay(
    base_config: &OpenCodeConfig,
    overlay: &Option<OpenCodeConfig>,
) -> Result<OpenCodeConfig> {
    // Deep merge the overlay onto the base config
    let mut merged = base_config.clone();

    if let Some(overlay) = overlay {
        // Merge tools
        merged.tools.read = merged.tools.read || overlay.tools.read;
        merged.tools.grep = merged.tools.grep || overlay.tools.grep;
        merged.tools.glob = merged.tools.glob || overlay.tools.glob;
        merged.tools.list = merged.tools.list || overlay.tools.list;
        merged.tools.task = merged.tools.task || overlay.tools.task;  // ← Relaxed

        // Merge provider
        merged.provider = overlay.provider.clone();

        // Merge agent
        merged.agent = overlay.agent.clone();
    }

    Ok(merged)
}
```

**Warning Logged:**

```rust
// In the runner, when task tool is re-enabled
if merged.tools.task {
    log::warn!(
        "operator OpenCode overlay (review.opencode) relaxed a review floor invariant (ADR-0099)",
        task_id = task_id,
        relaxation = "built-in tool `task` re-enabled (coverage may go blind)"
    );
}
```

#### Level 4: Subagent Execution (opencode framework)

**File:** `opencode/src/agent.rs`

```rust
pub struct Subagent {
    name: String,
    model: Model,
    tools: ToolSurface,
    prompt: String,
}

impl Subagent {
    pub async fn execute(
        &self,
        context: &ReviewContext,
        task: &Task,
    ) -> Result<SubagentOutput> {
        // Execute the subagent
        let output = self.model.chat(&self.prompt, &task.input).await?;

        // If task tool is enabled, subagent can delegate to other subagents
        if self.tools.task {
            // This allows subagent reads to escape coverage accounting
            // because subagents use their own tool surfaces
        }

        Ok(output)
    }
}
```

#### Level 5: Coverage Accounting (runner)

**File:** `agent-runner/src/coverage.rs`

```rust
pub struct CoverageAccount {
    total_reads: usize,
    tracked_reads: usize,
    untracked_reads: usize,
}

impl CoverageAccount {
    pub fn add_read(&mut self, path: &str) {
        self.total_reads += 1;
        self.tracked_reads += 1;
    }

    pub fn add_untracked_read(&mut self, path: &str) {
        self.total_reads += 1;
        self.untracked_reads += 1;
    }

    pub fn is_covered(&self) -> bool {
        self.untracked_reads == 0
    }
}
```

**Mediated Tool (coverage-counted):**

```rust
pub fn lightbridge_read_file(
    context: &mut ReviewContext,
    input: &ToolInput,
) -> ToolOutput {
    // Track the read in coverage accounting
    context.coverage.add_read(&input.path);

    // Execute the actual file read
    let content = fs::read_to_string(&input.path)?;

    ToolOutput::Success { content }
}
```

**Subagent Tool (untracked):**

```rust
// Subagents use their own tool surfaces, which may bypass mediation
pub fn subagent_read_file(
    context: &mut ReviewContext,
    input: &ToolInput,
) -> ToolOutput {
    // NOT tracked in coverage accounting
    let content = fs::read_to_string(&input.path)?;

    ToolOutput::Success { content }
}
```

### Summary of the Coverage Floor Relaxation

| Level | Configuration | Tool | Coverage Tracking | Purpose |
|-------|---------------|------|-------------------|---------|
| **Base** | `review.jsonc` | `task: false` | All reads tracked | Enforce coverage floor |
| **Overlay** | `ai-helm-values` | `task: true` | Subagent reads NOT tracked | Offload file reading |
| **Runner** | `opencode.rs` | Deep merge | Warning logged | Track relaxation |
| **Subagent** | `opencode.rs` | Subagent execution | Untracked reads | Delegate file reading |
| **Coverage** | `coverage.rs` | Mediated vs untracked | Mixed tracking | Trade-off accepted |

### Why This Matters

The opencode overlay's coverage floor relaxation is **intentional but risky**:

**Pros:**
- Reduces review cost (subagent is cheaper than primary reviewer)
- Offloads routine file reading
- Keeps primary reviewer focused on judgment

**Cons:**
- Subagent reads escape coverage accounting
- Review may be "blind" to some files
- Human reviewer cannot verify which files were examined

**Mitigation:**
- The disclosure informs the human reviewer about the relaxation
- The review is marked as "untrusted" per AI governance policy
- The disclosure is logged to runner logs (as a warning)

**Conclusion:**
The opencode overlay is a **useful tool** for reducing review cost, but it **requires careful handling** to ensure transparency and trustworthiness. The current implementation logs the warning but **incorrectly includes it in the review comment** instead of keeping it in the logs. This causes confusion for human reviewers who expect the review comment to be about the PR, not about the review process.
---

## How the Coverage Disclosure Is Actually Done

### Where the Disclosure Is Generated

The coverage disclosure is generated in the **review output template** that formats the final review comment. The template is defined in the control plane's review output configuration.

#### Control Plane Review Output Template

**File:** `services/control-plane/src/review/output.rs` (simplified)

```rust
pub struct ReviewOutput {
    pub verdict: Verdict,
    pub findings: Vec<ReviewFinding>,
    pub configuration_disclosure: Option<String>,
    pub ai_governance_disclaimer: String,
}

impl ReviewOutput {
    pub fn format_comment(&self) -> String {
        let mut comment = String::new();

        // 1. Verdict
        comment.push_str(&format!("{} {}\n", self.verdict.emoji, self.verdict.text));

        // 2. Findings (if any)
        if !self.findings.is_empty() {
            comment.push_str("\n## Findings\n");
            for finding in &self.findings {
                comment.push_str(&format!("- {}\n", finding.description));
            }
        }

        // 3. Configuration disclosure (ALWAYS included)
        if let Some(disclosure) = &self.configuration_disclosure {
            comment.push_str("\n## Configuration Disclosure\n");
            comment.push_str(disclosure);
        }

        // 4. AI governance disclaimer (ALWAYS included)
        comment.push_str("\n---\n");
        comment.push_str(&self.ai_governance_disclaimer);

        comment
    }
}
```

#### Runner Warning to Control Plane

**File:** `agent-runner/src/review/opencode.rs`

```rust
pub async fn execute_review(
    &self,
    task_id: &str,
    pr_info: &PrInfo,
) -> Result<ReviewOutput> {
    // ... review execution ...

    // Check if opencode overlay relaxed the coverage floor
    let configuration_disclosure = if merged.tools.task {
        Some(String::from(
            "A custom operator OpenCode config (review.opencode) was active and relaxed the review floor: built-in tool task re-enabled (coverage may go blind). Coverage/read-only guarantees may differ from the default."
        ))
    } else {
        None
    };

    // Log the warning to runner logs
    if merged.tools.task {
        log::warn!(
            "operator OpenCode overlay (review.opencode) relaxed a review floor invariant (ADR-0099)",
            task_id = task_id,
            relaxation = "built-in tool `task` re-enabled (coverage may go blind)"
        );
    }

    // Return the review output with the disclosure
    Ok(ReviewOutput {
        verdict: Verdict::Pass,
        findings: vec![],
        configuration_disclosure,  // ← Included in output
        ai_governance_disclaimer: "🤖 AI-generated review — treat it as untrusted, verify before acting; a human owns the final decision (AI governance).".to_string(),
    })
}
```

### Why It's Not Only Logged but Also Included

The disclosure is **included in the review output** for two reasons:

#### 1. Transparency Requirement (ADR-0099)

The opencode overlay design document (ADR-0099) requires that the human reviewer be informed when the coverage floor is relaxed:

```yaml
# ADR-0099: Operator OpenCode overlay design
# Section: Transparency Requirements
# Requirement: The human reviewer must be informed when the coverage floor is relaxed
# Implementation: Include configuration disclosure in review output
```

This is because:
- The review may be "blind" to some files
- The coverage guarantees differ from the default
- The review should be treated as untrusted per AI governance policy

#### 2. Single Source of Truth

The review output is the **single source of truth** for the human reviewer. It contains:
- The verdict (thumbs up/down)
- The findings (if any)
- The context about how that verdict was reached (including configuration disclosure)
- The AI governance disclaimer

This ensures the human reviewer has all the information they need in one place, without needing to check runner logs separately.

### How to Silence the Disclosure

The disclosure can be silenced by **removing it from the review output template**. There are two approaches:

#### Approach 1: Remove from Template (Recommended)

**File:** `services/control-plane/src/review/output.rs`

```rust
pub struct ReviewOutput {
    pub verdict: Verdict,
    pub findings: Vec<ReviewFinding>,
    // pub configuration_disclosure: Option<String>,  // ← Removed
    pub ai_governance_disclaimer: String,
}

impl ReviewOutput {
    pub fn format_comment(&self) -> String {
        let mut comment = String::new();

        // 1. Verdict
        comment.push_str(&format!("{} {}\n", self.verdict.emoji, self.verdict.text));

        // 2. Findings (if any)
        if !self.findings.is_empty() {
            comment.push_str("\n## Findings\n");
            for finding in &self.findings {
                comment.push_str(&format!("- {}\n", finding.description));
            }
        }

        // 3. AI governance disclaimer (ALWAYS included)
        comment.push_str("\n---\n");
        comment.push_str(&self.ai_governance_disclaimer);

        comment
    }
}
```

**Pros:**
- Disclosure is removed from review comments
- Warning is still logged to runner logs (for debugging)
- Transparency is maintained for developers

**Cons:**
- Human reviewers won't know when coverage floor is relaxed

#### Approach 2: Make Disclosure Optional

**File:** `services/control-plane/src/review/output.rs`

```rust
pub struct ReviewOutput {
    pub verdict: Verdict,
    pub findings: Vec<ReviewFinding>,
    pub configuration_disclosure: Option<String>,
    pub ai_governance_disclaimer: String,
    pub include_disclosure: bool,  // ← New field
}

impl ReviewOutput {
    pub fn format_comment(&self) -> String {
        let mut comment = String::new();

        // 1. Verdict
        comment.push_str(&format!("{} {}\n", self.verdict.emoji, self.verdict.text));

        // 2. Findings (if any)
        if !self.findings.is_empty() {
            comment.push_str("\n## Findings\n");
            for finding in &self.findings {
                comment.push_str(&format!("- {}\n", finding.description));
            }
        }

        // 3. Configuration disclosure (OPTIONAL)
        if self.include_disclosure && self.configuration_disclosure.is_some() {
            comment.push_str("\n## Configuration Disclosure\n");
            comment.push_str(self.configuration_disclosure.as_ref().unwrap());
        }

        // 4. AI governance disclaimer (ALWAYS included)
        comment.push_str("\n---\n");
        comment.push_str(&self.ai_governance_disclaimer);

        comment
    }
}
```

**Usage:**

```rust
// For end-user-facing reviews (disclosure NOT included)
let output = ReviewOutput {
    verdict: Verdict::Pass,
    findings: vec![],
    configuration_disclosure: Some(String::from("...")),
    ai_governance_disclaimer: "...".to_string(),
    include_disclosure: false,  // ← Don't include in review comment
};

// For internal operational reviews (disclosure included)
let output = ReviewOutput {
    verdict: Verdict::Pass,
    findings: vec![],
    configuration_disclosure: Some(String::from("...")),
    ai_governance_disclaimer: "...".to_string(),
    include_disclosure: true,  // ← Include in review comment
};
```

**Pros:**
- Flexible: can be enabled/disabled per review type
- Maintains transparency for internal reviews
- Reduces noise for end-user-facing reviews

**Cons:**
- Requires additional configuration
- More complex to implement

#### Approach 3: Add Review Type Indicator

**File:** `services/control-plane/src/review/output.rs`

```rust
pub struct ReviewOutput {
    pub verdict: Verdict,
    pub findings: Vec<ReviewFinding>,
    pub configuration_disclosure: Option<String>,
    pub ai_governance_disclaimer: String,
    pub review_type: ReviewType,  // ← New field
}

pub enum ReviewType {
    Fast,           // SAST + diff-only
    Deep,           // Full review with opencode overlay
    Operational,    // Internal operational review
}

impl ReviewOutput {
    pub fn format_comment(&self) -> String {
        let mut comment = String::new();

        // 1. Verdict
        comment.push_str(&format!("{} {}\n", self.verdict.emoji, self.verdict.text));

        // 2. Review type indicator
        let review_type_text = match self.review_type {
            ReviewType::Fast => "Fast review (SAST + diff-only)",
            ReviewType::Deep => "Deep review with opencode overlay",
            ReviewType::Operational => "Operational review",
        };
        comment.push_str(&format!("\n**Review Type:** {}\n", review_type_text));

        // 3. Findings (if any)
        if !self.findings.is_empty() {
            comment.push_str("\n## Findings\n");
            for finding in &self.findings {
                comment.push_str(&format!("- {}\n", finding.description));
            }
        }

        // 4. Configuration disclosure (only for Deep reviews)
        if matches!(self.review_type, ReviewType::Deep) {
            if let Some(disclosure) = &self.configuration_disclosure {
                comment.push_str("\n## Configuration Disclosure\n");
                comment.push_str(disclosure);
            }
        }

        // 5. AI governance disclaimer (ALWAYS included)
        comment.push_str("\n---\n");
        comment.push_str(&self.ai_governance_disclaimer);

        comment
    }
}
```

**Pros:**
- Clear indication of review type
- Disclosure only shown when relevant
- Easy to understand for human reviewers

**Cons:**
- Still includes disclosure in review comments (just conditional)

### Recommended Solution

**Approach 2 (Make Disclosure Optional)** is the recommended solution because it:

1. **Maintains transparency** for internal operational reviews
2. **Reduces noise** for end-user-facing reviews
3. **Is flexible** and can be configured per deployment
4. **Is backward compatible** (can be enabled by default)

**Implementation Steps:**

1. Add `include_disclosure: bool` field to `ReviewOutput` struct
2. Update `format_comment()` to conditionally include disclosure
3. Set `include_disclosure: false` for end-user-facing reviews
4. Set `include_disclosure: true` for internal operational reviews
5. Update the runner to pass `include_disclosure: false` for PR reviews

**Configuration:**

```yaml
# ai-helm-values/environments/prod/values/lightbridge-code-intelligence.yaml
review:
  output:
    include_disclosure: false  # ← Don't include in review comments
    # Set to true for internal operational reviews
```

**Result:**

- Warning is still logged to runner logs (for debugging)
- Review comments are cleaner and focused on PR findings
- Human reviewers can still check runner logs if needed
- Transparency is maintained for developers

---
