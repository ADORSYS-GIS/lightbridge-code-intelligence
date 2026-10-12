# Finding Verification & The Refute Pass

This report details how Lightbridge Code Intelligence ensures the reliability of its findings. To mitigate AI hallucinations and prevent the agent from confidently blocking pull requests with incorrect information, the system forces a "Refute Pass" for high-severity findings.

## 1. The Core Problem: LLM Overconfidence

Large Language Models (LLMs) are prone to hallucinations, particularly when performing static analysis tasks. They can sound extremely confident when claiming that a variable is uninitialized, a boundary condition is missed, or a security vulnerability exists, even when the code is perfectly safe.

If an AI posts a blocking finding (e.g., Priority 0 or Priority 1) that turns out to be false, developers quickly lose trust in the system. The cost of a false positive is much higher than the cost of a missed "nit."

## 2. The Solution: The Refute Pass

To combat this, Lightbridge implements a **Finding Verification Mechanism** (documented internally as ADR-0043: Review finding verification). 

Instead of trusting the agent's first conclusion, the control plane forces the agent to play the role of a skeptic against its own findings.

### How It Works

1. **The First Review (The Proposal)**
   During the normal review loop, the agent inspects the code and buffers findings using the `add_review_comment` tool. If the agent records a high-severity finding (P0/P1), the system notes this internally.

2. **The "Bounce" (The Challenge)**
   When the agent calls `finish` to submit its review, the control plane intervenes. If high-severity findings are present and haven't been re-verified, the control plane **rejects the finish call**. 
   
   It bounces the request back to the agent with a prompt along the lines of:
   > "You have proposed a high-severity finding on line X. Before I accept this, you must explicitly re-verify it. Cite the exact code that proves this vulnerability exists. If you cannot find concrete proof, you must use the `retract_finding` tool to remove it."

3. **The Confirm Review (The Refute Pass)**
   The agent is forced into a new turn. It must re-read the specific file and line it commented on. 
   - If the vulnerability is real, the agent reinforces its argument.
   - If the agent realizes it made an assumption (e.g., it thought a variable was unchecked, but a few lines up there is a check), it must use a `retract_finding` tool to quietly delete the false positive from the buffer.

4. **Final Submission**
   Once the agent has performed the refute pass, it can call `finish` again. The control plane accepts the final buffer, and only the surviving, verified findings are sent to the GitHub egress.

## 3. Why This Works

This two-step "propose then confirm" mechanism effectively leverages the LLM's capacity for self-correction. 
- In the first pass, the LLM is acting creatively to find edge cases.
- In the second pass, the prompt shifts the LLM's persona to be highly skeptical and evidence-based. 

By separating the "brainstorming" phase from the "verification" phase, Lightbridge significantly reduces the false-positive rate of automated code reviews.

## 4. Integration with the Coverage Gate

The Refute Pass works alongside the **Coverage Gate**. While the Refute Pass ensures that *what is said is correct*, the Coverage Gate ensures that *everything important was looked at*.

- **Coverage Gate:** If the agent tries to `finish` without reading all changed files, the control plane bounces it back: *"You missed files A and B."*
- **Refute Pass:** If the agent tries to `finish` with unverified P0/P1 findings, the control plane bounces it back: *"Prove finding X."*

Both mechanisms demonstrate Lightbridge's core architectural principle: **The agent proposes, but the control plane dictates the rules of engagement.**
