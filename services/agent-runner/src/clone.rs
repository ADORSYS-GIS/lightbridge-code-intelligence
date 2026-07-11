//! Shallow repository checkout for a task. We shell out to `git` (the runtime image bundles it)
//! rather than linking libgit2 — simpler to build, and partial/SHA fetches are exactly what the CLI
//! is good at. The installation token rides in the remote URL, so every captured error is scrubbed
//! of it before it can reach a log line.

use std::path::{Path, PathBuf};
use std::process::Output;

use crate::bootstrap::client::TaskContext;

/// Clone the task's repo at the relevant commit into `{workdir}/repo` and return that path.
///
/// We `init` + `fetch --depth 1 <ref>` rather than a full clone: a PR review only needs the head
/// tree (and, best-effort, the base commit for later diffing), not the whole history. The fetched
/// ref is the head SHA when known, else the repo's default branch. GitHub permits fetching a commit
/// by SHA, so head/base fetches work even though the commit isn't a branch tip.
pub async fn checkout(ctx: &TaskContext, workdir: &str) -> anyhow::Result<PathBuf> {
    let dir = Path::new(workdir).join("repo");
    tokio::fs::create_dir_all(&dir).await?;
    let url = ctx.authenticated_clone_url();

    git(&dir, &["init", "-q"], &ctx.token).await?;
    git(&dir, &["remote", "add", "origin", &url], &ctx.token).await?;

    // Primary ref: the head SHA we were asked to review, falling back to the default branch.
    let head_ref = ctx.head_sha.as_deref().unwrap_or(&ctx.default_branch);
    git(
        &dir,
        &["fetch", "--depth", "1", "origin", head_ref],
        &ctx.token,
    )
    .await?;
    git(&dir, &["checkout", "-q", "FETCH_HEAD"], &ctx.token).await?;

    // Best-effort: bring in the base commit too (for PR diffing / overlay indexing in a later
    // slice). A failure here is non-fatal — the head checkout is what this slice needs.
    if let Some(base_sha) = &ctx.base_sha
        && Some(base_sha) != ctx.head_sha.as_ref()
        && let Err(error) = git(
            &dir,
            &["fetch", "--depth", "1", "origin", base_sha],
            &ctx.token,
        )
        .await
    {
        tracing::warn!(%error, base_sha, "could not fetch base sha (non-fatal)");
    }

    Ok(dir)
}

/// The PR's change set: the unified diff (merge-base→head) and the list of changed file paths. Used to
/// scope the review to *what the PR actually changed* rather than auditing the whole repository.
pub struct PrDiff {
    /// `git diff <merge-base>..<head>` output (unified, no color).
    pub diff: String,
    /// Paths (repo-root-relative) that the PR touches — the only files a finding may land on.
    pub files: Vec<String>,
}

/// Compute the PR diff for the task in `checkout`, scoped to what the PR itself changed.
///
/// We diff `head` against the **merge-base** of base and head (three-dot semantics — exactly what
/// GitHub's "Files changed" tab shows), NOT against the base branch tip. `base_sha` is the base
/// *branch tip* (from the webhook `pull_request.base.sha` / the `pulls/{n}` API), which drifts forward
/// as other PRs merge into the base branch. A plain two-dot `base..head` then renders every commit that
/// landed on base *after this PR forked* as a spurious deletion — the review then describes a change the
/// PR never made (vymalo#275: #274 merged into `main` between fork and review, so the two-dot diff
/// "deleted" all of #274's files and the fast pass reviewed the inverse of the wrong PR).
///
/// Best-effort: returns `None` when we lack both SHAs, they're equal, the commits aren't present (their
/// fetch is itself best-effort in [`checkout`]), or git produces an empty diff — in every such case the
/// caller falls back to an unscoped review rather than failing the task.
pub async fn pr_diff(checkout: &Path, ctx: &TaskContext) -> Option<PrDiff> {
    let base = ctx.base_sha.as_deref()?;
    let head = ctx.head_sha.as_deref()?;
    if base == head {
        return None;
    }
    let diff_from = merge_base_or_deepen(checkout, base, head, &ctx.token).await;
    let range = format!("{diff_from}..{head}");

    let patch = match git(checkout, &["diff", "--no-color", &range], &ctx.token).await {
        Ok(out) => out,
        Err(error) => {
            tracing::warn!(%error, "could not compute PR diff (non-fatal; review unscoped)");
            return None;
        }
    };
    let diff = String::from_utf8_lossy(&patch.stdout).trim().to_string();
    if diff.is_empty() {
        return None;
    }

    // `-z`: NUL-separated, and crucially *unquoted* — without it git quotes/escapes paths with
    // spaces or non-ASCII bytes, which would corrupt the changed-file set used to scope the review.
    let names = git(checkout, &["diff", "--name-only", "-z", &range], &ctx.token)
        .await
        .ok()?;
    let files = String::from_utf8_lossy(&names.stdout)
        .split('\0')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();

    Some(PrDiff { diff, files })
}

/// The commit to diff `head` against: the merge-base of `base` and `head` when we can find it, else
/// `base` itself (the old two-dot behaviour — correct whenever `base` is already an ancestor of `head`,
/// wrong only when it isn't; the fallback is therefore never worse than before).
///
/// The checkout is shallow ([`checkout`] fetches each ref at `--depth 1`), so the common ancestor is
/// usually absent and `git merge-base` fails outright. We deepen both tips (bounded, exponentially) and
/// retry until it resolves. A fork point deeper than the cap — or a network hiccup mid-deepen — falls
/// back to `base`. The common case (a PR whose branch forked recently) resolves on the first deepen.
async fn merge_base_or_deepen(dir: &Path, base: &str, head: &str, token: &str) -> String {
    if let Some(mb) = merge_base(dir, base, head, token).await {
        return mb;
    }
    // Absolute depths (not `--deepen` increments): re-fetching the same SHAs at a larger depth extends
    // the shallow history in place. Both tips are deepened together so the ancestor can appear on either.
    // Start small — a PR's fork point is almost always a handful of commits back — and grow only if the
    // first steps don't reach it, so the common case pays one cheap fetch, not a 1024-deep one.
    for depth in ["16", "64", "256", "1024"] {
        if git(
            dir,
            &["fetch", "--depth", depth, "origin", base, head],
            token,
        )
        .await
        .is_err()
        {
            // A fetch failure (network / expired creds / unreachable ref) is persistent — a deeper
            // fetch would fail the same way, each burning another timeout. Stop and take the fallback.
            break;
        }
        if let Some(mb) = merge_base(dir, base, head, token).await {
            return mb;
        }
    }
    tracing::warn!(
        base,
        head,
        "no merge-base found after deepening; diffing against the base tip (the diff may include \
         base-branch commits not from this PR)"
    );
    base.to_string()
}

/// `git merge-base base head`, or `None` when git can't compute one — no common ancestor in the shallow
/// history yet (exit 1), or any other git error. A blank result (shouldn't happen on success) is `None`.
async fn merge_base(dir: &Path, base: &str, head: &str, token: &str) -> Option<String> {
    let out = git(dir, &["merge-base", base, head], token).await.ok()?;
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

/// Run a `git` subcommand in `dir`, returning an error whose message has `token` redacted.
async fn git(dir: &Path, args: &[&str], token: &str) -> anyhow::Result<Output> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .map_err(|error| {
            anyhow::anyhow!("failed to spawn git {:?}: {error}", redact(args, token))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "git {:?} failed ({}): {}",
            redact(args, token),
            output.status,
            scrub(&stderr, token)
        );
    }
    Ok(output)
}

/// Replace any occurrence of the token (e.g. inside a remote URL git echoed back) with `***`.
fn scrub(text: &str, token: &str) -> String {
    if token.is_empty() {
        return text.to_string();
    }
    text.replace(token, "***")
}

/// Render the arg list for error messages with any embedded token redacted (the `remote add` URL).
fn redact(args: &[&str], token: &str) -> Vec<String> {
    args.iter().map(|arg| scrub(arg, token)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_removes_the_token() {
        assert_eq!(
            scrub(
                "https://x-access-token:test-secret@github.com/o/r.git",
                "test-secret"
            ),
            "https://x-access-token:***@github.com/o/r.git"
        );
    }

    #[test]
    fn scrub_is_a_noop_for_empty_token() {
        assert_eq!(scrub("nothing to hide", ""), "nothing to hide");
    }

    // Reproduces the vymalo#275 shape in a real local repo: the base branch advances past the PR's fork
    // point (another PR merged into base after this one branched). The fix must diff `head` against the
    // MERGE-BASE (showing only the PR's own change), never the base tip (which two-dots the base-only
    // commit into a spurious deletion). Local repo has full history, so merge-base resolves on the fast
    // path — this pins the correctness, not the deepening glue (which needs a remote).
    #[tokio::test]
    async fn diffs_against_merge_base_not_the_drifted_base_tip() {
        use std::process::Command;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let git_run = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.email=t@t.co", "-c", "user.name=t"])
                .args(args)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?} failed");
        };
        let rev = |r: &str| {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["rev-parse", r])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        git_run(&["init", "-q", "-b", "main"]);
        // The fork point — the true merge-base.
        std::fs::write(dir.join("root.txt"), "root\n").unwrap();
        git_run(&["add", "-A"]);
        git_run(&["commit", "-qm", "root"]);
        let root = rev("HEAD");
        // Base branch advances after the fork: another PR (à la #274) lands `landed.txt`.
        std::fs::write(dir.join("landed.txt"), "landed on base\n").unwrap();
        git_run(&["add", "-A"]);
        git_run(&["commit", "-qm", "base advanced"]);
        let base = rev("HEAD");
        // This PR forks from `root` and adds its own file.
        git_run(&["checkout", "-q", "-b", "pr", &root]);
        std::fs::write(dir.join("feature.rs"), "the PR\n").unwrap();
        git_run(&["add", "-A"]);
        git_run(&["commit", "-qm", "the PR"]);
        let head = rev("HEAD");

        // Merge-base resolves to the fork point, and that's what we diff from.
        assert_eq!(
            merge_base(dir, &base, &head, "").await.as_deref(),
            Some(root.as_str())
        );
        assert_eq!(merge_base_or_deepen(dir, &base, &head, "").await, root);

        let names = |range: String| async move {
            let out = git(dir, &["diff", "--name-only", "-z", &range], "")
                .await
                .unwrap();
            String::from_utf8_lossy(&out.stdout).replace('\0', " ")
        };

        // The fix: only the PR's own file, never the base-only file.
        let fixed = names(format!("{root}..{head}")).await;
        assert!(fixed.contains("feature.rs"), "PR file present: {fixed:?}");
        assert!(
            !fixed.contains("landed.txt"),
            "base-only file must NOT appear in the PR diff: {fixed:?}"
        );

        // Sanity: the old two-dot `base..head` DID surface the base-only file (as a deletion) — the bug.
        let buggy = names(format!("{base}..{head}")).await;
        assert!(
            buggy.contains("landed.txt"),
            "two-dot base..head reproduces the bug (base-only file leaks in): {buggy:?}"
        );
    }
}
