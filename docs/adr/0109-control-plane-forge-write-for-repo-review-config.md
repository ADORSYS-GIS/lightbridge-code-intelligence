# ADR-0109: Control-plane forge-write capability, scoped to `.lightbridge-code-review.jsonc`

- **Status:** Accepted
- **Date:** 2026-07-29
- **Deciders:** @stephane-segning

## Context and Problem Statement

Story #500 (epic #491) wants a repo owner or admin to change a repo's review preset from the `lci` TUI
or apps/web, not just by hand-editing `.lightbridge-code-review.jsonc` and committing it themselves.
That file is deliberately author-owned, in-repo, git-committed config
([ADR-0030](0030-repo-review-config.md)) — nothing in this codebase writes to it today.
`CodePlatform` (`services/control-plane/src/integrations/platform.rs`) exposes only reads
(`get_repo_file`, `list_changed_files`, `default_branch`, `pr_shas`, `clone_url`) plus a narrow set of
PR/issue-scoped writes (`post_review`, `post_comment`, `add_reaction`, `add_labels`) — never a
repo-content commit. The GitHub App's private key, the only credential capable of a file write, is kept
control-plane-side by design ([ADR-0002](0002-rust-control-plane-trust-boundary.md),
[ADR-0096](0096-mediated-forge-read-tools.md)) and nothing today asks it to write.

Should the control plane acquire the capability to commit a file into a user's repo, and if so, how
narrowly?

## Decision Drivers

- **Bounded blast radius.** Any new forge-write capability is a genuine trust-boundary expansion; it
  must not become a general "write any file" primitive just because one field needs updating.
- **Right owner, again** ([ADR-0038](0038-per-repo-review-model.md)'s same reasoning): an admin changing
  an operator-cost-relevant setting (which preset — and by extension which model/budget — a repo runs
  under) shouldn't need the repo author's separate sign-off, the way ADR-0038 already argued for model
  selection living outside the author-owned file.
- **No silent action.** This repo's governance posture (every AI/admin-authored change discloses itself)
  extends naturally to an admin-triggered commit: it should say who did it and why, in the commit itself
  — not land as an anonymous diff in the repo's history.
- **Preserve the author's own edit path.** A human can still edit and commit the file directly at any
  time; the new capability adds a second path to the same file, it doesn't take the first one away.
- **Reuse what already exists.** `get_repo_file` (story #494/#495) already reads this exact file, at this
  exact path, per platform — the write path should be the direct sibling of that, not a new general
  mechanism.

## Considered Options

- **Option A — No write capability; read-only display only.** The UI shows the resolved preset; changing
  it still requires a human commit.
- **Option B — General-purpose repo-file-write tool.** A `CodePlatform::write_repo_file(path, content)`
  usable for any path.
- **Option C — Single-purpose, hard-restricted write, PR-and-wait.** A method that only ever writes
  `.lightbridge-code-review.jsonc`, but proposes the change as a PR the repo owner must merge.
- **Option D — Single-purpose, hard-restricted write, direct commit.** Same restriction as C, but commits
  straight to the default branch, discovered by the caller (not the repo owner) needing merge approval.

## Decision Outcome

Chosen option: **Option D.**

- **One new `CodePlatform` trait method**, deliberately narrow in shape:
  ```rust
  async fn update_repo_file(
      &self,
      repo: &RepoRef,
      path: &str,
      mutate: impl FnOnce(Option<String>) -> String + Send,
      message: &str,
  ) -> anyhow::Result<()>;
  ```
  implemented per platform: GitHub Contents API `PUT` (reads the current `sha` first for the API's
  optimistic-concurrency check), GitLab Repository Files API `PUT`, Bitbucket Source API `POST`. `mutate`
  receives the current file content (`None` if the file doesn't exist yet) and returns the new content —
  a read-modify-write, not a blind overwrite.
- **The only caller passes `path = ".lightbridge-code-review.jsonc"`.** This is not exposed as a
  general-purpose write tool anywhere in the mediated-tools surface reviewed agents use
  ([ADR-0037](0037-agent-acts-via-mediated-tools.md)) — it is reachable only from the new admin HTTP
  endpoint (story #500), never from a review/A2A/OpenCode tool call. Nothing in this decision expands
  what an agent can do; it only expands what the *admin console, acting as itself*, can do.
- **Direct commit to the default branch, not a PR.** An admin already has authority over this setting
  (same reasoning ADR-0038 used for model selection); requiring a PR-and-merge round-trip for a settings
  toggle would defeat the point of a direct admin action and add a confusing extra step. The commit
  message discloses the actor and action, e.g.
  `"chore: set review preset to \"ultra\" via Lightbridge admin console (requested by <admin login>)"`.
- **Read-modify-write, scoped to the `preset`/`entry_points` keys only.** The endpoint parses the
  existing file with `jsonc-parser` (already a workspace dependency, story #494), updates only those two
  fields, and re-serializes. Comment/formatting preservation is not guaranteed by the chosen JSONC
  parsing approach — a file with no other author-authored content round-trips cleanly; a heavily
  hand-commented file may lose comments on a write. This is disclosed to the admin in the endpoint's
  response, not silently accepted.
- **New RBAC permission `repo:configure`** ([ADR-0023](0023-db-backed-rbac.md)'s existing flat-claim
  model), required in addition to the repo already being approved. Checked at the new
  `POST /admin/repositories/{id}/preset` endpoint (story #500).
- **Conflict handling.** A concurrent edit (the fetched `sha`/revision is stale by the time the write
  lands) surfaces as a clear 409 to the caller — never a silent overwrite, never a retry-and-clobber.

### Consequences

- **Good:** the admin console can finally change a setting it already has a legitimate, disclosed reason
  to change, without needing the repo owner in the loop for every toggle; the capability is narrow enough
  that a reviewer can audit its entire blast radius by reading one function; the commit trail is
  self-documenting.
- **Bad / accepted trade-off:** this is a genuine, permanent expansion of what the control plane's forge
  credential can do — mitigated, but not eliminated, by the single-path restriction and RBAC gate. A
  future careless change that widens `path` beyond the one hard-coded value would silently turn this into
  Option B; a reviewer of any future PR touching `update_repo_file`'s call site should treat widening it
  as a change requiring its own sign-off, not a routine extension.
- **Neutral / to watch:** the file now has two legitimate writers (the repo author, directly; the admin
  console, via this path) — a repo owner who dislikes an admin-set preset can still just edit the file
  back, same as any other git history; there's no lock-out mechanism and none is proposed here.

## Pros and Cons of the Options

### Option A — read-only display only
- Good: zero new trust-boundary surface.
- Bad: doesn't fulfil story #500's actual AC ("...and can be changed"); an admin still has to ask the
  repo owner to make every change.

### Option B — general-purpose write tool
- Good: reusable for whatever the next file-write need turns out to be.
- Bad: unbounded blast radius for a capability motivated by exactly one field on exactly one file;
  invites exactly the scope creep this ADR's Decision Drivers reject.

### Option C — PR-and-wait
- Good: preserves the repo owner's final say even on an admin-triggered change; matches this repo's
  general "a human merges the change" governance posture used everywhere reviews are concerned.
- Bad: wrong workflow for a settings toggle an admin already has standing authority to make — a PR sits
  open until someone merges it, so "changed" in the UI doesn't mean "took effect," which is a confusing
  admin UX and doesn't match ADR-0038's established reasoning that admin-owned settings shouldn't need
  author sign-off.

### Option D — direct commit (chosen)
- Good: matches the admin's actual authority; the change takes effect the moment it's made, which is
  what "change the preset" should mean from an admin console.
- Bad: no author-side review window before the change lands — mitigated by the disclosed commit message
  and the narrow, auditable capability.

## More Information

- Sibling read path: `get_repo_file` ([ADR-0030](0030-repo-review-config.md), stories #494/#495) — this
  ADR is its write-side counterpart, at the same restricted single-path scope.
- Admin gating: [ADR-0023](0023-db-backed-rbac.md) (permission-based authz), same pattern
  [ADR-0038](0038-per-repo-review-model.md) used for the model-selection admin surface.
- Trust boundary the App private key stays behind: [ADR-0002](0002-rust-control-plane-trust-boundary.md),
  [ADR-0096](0096-mediated-forge-read-tools.md) — this ADR does not relax that boundary for any
  agent-facing surface, only for the admin console acting through control-plane-owned HTTP endpoints.
- Consuming story: #500 (epic #491).
