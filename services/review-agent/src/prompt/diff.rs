//! File-boundary–aware packing of a PR's unified diff into the review prompt (ADR-0062).
//!
//! The review prompt has a byte budget (`max_diff_chars`). The naive approach — take the whole
//! `git diff` string and cut it at the byte cap — slices mid-file and mid-hunk with no idea where a
//! file's changes start or end. A real failure (PR #274): a 132 KB diff was cut at byte 60 000, exactly
//! 278 bytes *before* a function definition whose call site the model *could* see, so the fast pass
//! honestly reported it "cannot see the implementation" and filed a P1 — while 55 % of the PR (every
//! file after the cut) was silently never in the prompt at all. The lockfile (`Cargo.lock`) had also
//! burned the first 12.6 KB of budget before a single source file was rendered.
//!
//! This module fixes both:
//!
//! 1. **Truncate on file boundaries, not bytes.** The diff is split into per-file sections (each a whole
//!    `diff --git …` block) and packed *whole* until the next one wouldn't fit. A file is either shown
//!    completely or listed as not-shown — never cut mid-hunk.
//! 2. **Deprioritise generated / lock-file noise.** Lockfiles, `*.min.js`, snapshots, etc. carry almost
//!    no review signal per byte, so they're set aside and listed rather than rendered — freeing the whole
//!    budget for source. (If a PR changes *only* such files we still render them, so the diff isn't blank.)
//!
//! The caller ([`super::build_messages`]) turns [`RenderedDiff::omitted_for_budget`] and
//! [`RenderedDiff::low_signal`] into an explicit "these files were NOT shown" block in the prompt, so the
//! model can state honest coverage and never raises a defect about code it was never given.

/// One file's slice of a unified diff: its repo-relative path and the raw `diff --git …` block (header +
/// hunks) exactly as git emitted it — both borrowed from the original diff (no per-file allocation).
struct FileSection<'a> {
    path: &'a str,
    text: &'a str,
    low_signal: bool,
}

/// The outcome of packing a PR diff into the prompt budget, file-boundary aware. Paths borrow from the
/// diff (`'a`) — the caller stringifies them into the prompt immediately, so no ownership is needed.
pub struct RenderedDiff<'a> {
    /// The diff text to paste into the prompt: whole per-file sections, source first, within budget.
    pub text: String,
    /// Source files whose diff didn't fit the budget and were dropped. The important coverage signal —
    /// the model is told these changes exist but were not shown, so it can't be confidently wrong about
    /// them. Repo-relative paths, in diff order.
    pub omitted_for_budget: Vec<&'a str>,
    /// Files set aside as low-signal generated/lock noise and listed rather than rendered (unless the PR
    /// changed nothing else). Repo-relative paths, in diff order.
    pub low_signal: Vec<&'a str>,
}

/// Whether a path is generated / lock-file noise that carries little review signal per byte, so it's
/// deprioritised out of the rendered diff (still disclosed in the file list). Matched on the normalised
/// (forward-slash) path so `a\b\Cargo.lock` classifies the same as `a/b/Cargo.lock`.
fn is_low_signal_path(path: &str) -> bool {
    // Only allocate to normalise when a backslash is actually present (Windows-style paths are rare).
    let normalized;
    let p = if path.contains('\\') {
        normalized = path.replace('\\', "/");
        normalized.as_str()
    } else {
        path
    };
    let name = p.rsplit('/').next().unwrap_or(p);

    // Exact lock / dependency-manifest files across ecosystems.
    const LOCK_FILES: &[&str] = &[
        "Cargo.lock",
        "package-lock.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "npm-shrinkwrap.json",
        "bun.lockb",
        "composer.lock",
        "Gemfile.lock",
        "poetry.lock",
        "Pipfile.lock",
        "go.sum",
        "flake.lock",
    ];
    if LOCK_FILES.contains(&name) {
        return true;
    }

    // Minified / bundled / map artefacts and common snapshot dumps — high-byte, low-signal.
    const NOISE_SUFFIXES: &[&str] = &[".min.js", ".min.css", ".map", ".snap", ".bundle.js"];
    NOISE_SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// The repo-relative path a `diff --git …` section is about. Prefers the `+++ b/…` header (present for
/// adds/modifies), falls back to `--- a/…` (deletions have `+++ /dev/null`), and finally to the
/// `diff --git a/… b/…` line for binary / pure-rename / mode-only sections that carry no `+++`/`---`.
///
/// Git leaves ASCII paths unquoted — including ones with **spaces** (it just appends a tab, which
/// `.trim()` drops) — but C-quotes names with non-ASCII/control bytes under `core.quotePath=true`
/// (default): `+++ "b/caf\303\251.rs"`. We strip the surrounding quotes for a readable label; the octal
/// escapes remain (unescaping needs allocation and this string is display/coverage-only, never used to
/// match against the real file list). Returns `None` only for a malformed section, which the caller
/// renders under a `(diff)` placeholder rather than dropping.
fn section_path(section: &str) -> Option<&str> {
    for line in section.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            if rest != "dev/null" {
                return Some(rest.trim());
            }
        } else if let Some(rest) = line.strip_prefix("--- a/") {
            if rest != "dev/null" {
                return Some(rest.trim());
            }
        } else if let Some(rest) = line.strip_prefix("+++ \"b/") {
            return Some(rest.trim().trim_end_matches('"'));
        } else if let Some(rest) = line.strip_prefix("--- \"a/") {
            return Some(rest.trim().trim_end_matches('"'));
        }
    }
    // No +++/--- lines (binary / pure rename / mode-only): parse the git header. Take the ` b/` tail
    // (paths equal except on a rename; the new name is the one under review), quoted variant first.
    let header = section.lines().next()?;
    let rest = header.strip_prefix("diff --git ")?;
    if let Some(i) = rest.find(" \"b/") {
        return Some(rest[i + 4..].trim().trim_end_matches('"'));
    }
    let i = rest.find(" b/")?;
    Some(rest[i + 3..].trim())
}

/// Split a unified diff into per-file sections at `diff --git` boundaries. Any preamble before the first
/// header (git never emits one, but a hand-crafted patch might) is attached to the first section so no
/// bytes are lost.
fn split_sections(diff: &str) -> Vec<FileSection<'_>> {
    // Byte offsets of every line that begins a new file section.
    let mut starts: Vec<usize> = Vec::new();
    let mut offset = 0usize;
    for line in diff.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            starts.push(offset);
        }
        offset += line.len();
    }
    if starts.is_empty() {
        // No recognisable headers: treat the whole diff as one anonymous section (still packed/capped).
        return vec![FileSection {
            path: section_path(diff).unwrap_or("(diff)"),
            text: diff,
            low_signal: false,
        }];
    }
    // A leading preamble (bytes before the first header) folds into the first section via `begin = 0`.
    let mut sections = Vec::with_capacity(starts.len());
    for (i, &start) in starts.iter().enumerate() {
        let begin = if i == 0 { 0 } else { start };
        let end = starts.get(i + 1).copied().unwrap_or(diff.len());
        let text = &diff[begin..end];
        let path = section_path(&diff[start..end]).unwrap_or("(diff)");
        let low_signal = is_low_signal_path(path);
        sections.push(FileSection {
            path,
            text,
            low_signal,
        });
    }
    sections
}

/// `s` truncated to at most `max` bytes without slicing through a multi-byte char.
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

/// Pack a PR's unified `diff` into `max` bytes on **file boundaries**: whole per-file sections, source
/// files first and lock/generated noise deprioritised, each shown completely or listed as not-shown.
///
/// Ordering: source sections (in diff order) come first so the budget is spent on reviewable code;
/// low-signal sections are set aside and listed, *unless* the PR changed nothing but low-signal files —
/// then they're the diff (packed within budget) so the prompt isn't empty. A single section larger than
/// the whole budget is boundary-truncated (only when it would otherwise be the first thing shown) so one
/// huge file can't blank the diff; it's still flagged as omitted so the model knows it's partial.
pub fn render_diff_for_prompt(diff: &str, max: usize) -> RenderedDiff<'_> {
    let sections = split_sections(diff);

    let (source, low_signal): (Vec<&FileSection>, Vec<&FileSection>) =
        sections.iter().partition(|s| !s.low_signal);

    // Low-signal files are listed, not rendered — unless they're all the PR has, in which case render
    // them (still boundary-packed) so a lockfile-only PR isn't a blank diff.
    let render_order: Vec<&FileSection> = if source.is_empty() {
        low_signal.clone()
    } else {
        source.clone()
    };
    let mut low_signal_listed: Vec<&str> = if source.is_empty() {
        Vec::new()
    } else {
        low_signal.iter().map(|s| s.path).collect()
    };

    let mut text = String::new();
    let mut omitted_for_budget: Vec<&str> = Vec::new();
    for sec in &render_order {
        // `<=`: a section exactly filling the remaining budget still fits.
        if text.len() + sec.text.len() <= max {
            text.push_str(sec.text);
        } else if text.is_empty() {
            // Nothing rendered yet and even this first section overflows: show as much of it as the
            // budget allows (boundary-safe) rather than emitting an empty diff, and flag it as partial.
            text.push_str(truncate_on_boundary(sec.text, max));
            omitted_for_budget.push(sec.path);
        } else {
            omitted_for_budget.push(sec.path);
        }
    }

    // Stable, de-duplicated disclosure lists in diff order.
    low_signal_listed.dedup();
    omitted_for_budget.dedup();

    RenderedDiff {
        text,
        omitted_for_budget,
        low_signal: low_signal_listed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal but realistic `git diff` section for `path` padded to roughly `body_lines` `+` lines,
    /// so a test can dial a section's byte size.
    fn section(path: &str, body_lines: usize) -> String {
        let mut s = format!(
            "diff --git a/{path} b/{path}\nnew file mode 100644\nindex 0000000..1111111\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{body_lines} @@\n",
        );
        for i in 0..body_lines {
            s.push_str(&format!("+line {i} in {path}\n"));
        }
        s
    }

    #[test]
    fn classifies_lockfiles_and_noise_as_low_signal() {
        for p in [
            "Cargo.lock",
            "clients/app/pnpm-lock.yaml",
            "package-lock.json",
            "web\\yarn.lock",
            "dist/app.min.js",
            "styles/x.min.css",
            "bundle.js.map",
            "components/__snapshots__/a.snap",
        ] {
            assert!(is_low_signal_path(p), "{p} should be low-signal");
        }
        for p in [
            "src/auth/store.rs",
            "Cargo.toml",
            "clients/lci/src/main.rs",
            "docs/adr/0063.md",
            "app.js", // not minified
        ] {
            assert!(!is_low_signal_path(p), "{p} should be source");
        }
    }

    #[test]
    fn extracts_path_for_add_delete_and_binary_sections() {
        let add = "diff --git a/src/new.rs b/src/new.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/new.rs\n@@ -0,0 +1,1 @@\n+x\n";
        assert_eq!(section_path(add), Some("src/new.rs"));

        let del = "diff --git a/src/old.rs b/src/old.rs\ndeleted file mode 100644\n--- a/src/old.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n";
        assert_eq!(section_path(del), Some("src/old.rs"));

        let bin = "diff --git a/assets/logo.png b/assets/logo.png\nBinary files a/assets/logo.png and b/assets/logo.png differ\n";
        assert_eq!(section_path(bin), Some("assets/logo.png"));

        // A path with a space is left UNQUOTED by git (it just appends a tab, which `.trim()` drops) —
        // so the plain `+++ b/` branch already handles it.
        let spaced = "diff --git a/src/spa ce.rs b/src/spa ce.rs\nindex 1..2 100644\n--- a/src/spa ce.rs\t\n+++ b/src/spa ce.rs\t\n@@ -1 +1 @@\n-x\n+y\n";
        assert_eq!(section_path(spaced), Some("src/spa ce.rs"));
    }

    // Quoted paths (git C-quotes non-ASCII/control bytes under core.quotePath=true, the default) must not
    // collapse to the `(diff)` placeholder — that mislabels the coverage disclosure AND would classify a
    // quoted lockfile as source, re-triggering the very budget-waste this module prevents (adversarial +
    // gemini review of PR #275). The octal escapes are left in (display-only); the surrounding quotes go.
    #[test]
    fn extracts_quoted_non_ascii_paths_from_header_and_marker() {
        // Add: quoted `+++ "b/…"` marker present.
        let add = "diff --git \"a/src/caf\\303\\251.rs\" \"b/src/caf\\303\\251.rs\"\nnew file mode 100644\n--- /dev/null\n+++ \"b/src/caf\\303\\251.rs\"\n@@ -0,0 +1 @@\n+x\n";
        assert_eq!(section_path(add), Some("src/caf\\303\\251.rs"));

        // Binary quoted: no `+++`/`---`, parsed from the `diff --git … "b/…"` header.
        let bin = "diff --git \"a/img/na\\303\\257ve.png\" \"b/img/na\\303\\257ve.png\"\nBinary files a/x and b/y differ\n";
        assert_eq!(section_path(bin), Some("img/na\\303\\257ve.png"));

        // A quoted lockfile (unicode-named parent dir) still classifies as low-signal via its ASCII base.
        assert!(is_low_signal_path("src/caf\\303\\251/Cargo.lock"));
    }

    #[test]
    fn splits_multi_file_diff_on_boundaries_without_losing_bytes() {
        let a = section("src/a.rs", 2);
        let b = section("src/b.rs", 2);
        let diff = format!("{a}{b}");
        let secs = split_sections(&diff);
        assert_eq!(secs.len(), 2);
        assert_eq!(secs[0].path, "src/a.rs");
        assert_eq!(secs[1].path, "src/b.rs");
        // Byte-preserving: concatenated sections reconstruct the original diff exactly.
        let joined: String = secs.iter().map(|s| s.text).collect();
        assert_eq!(joined, diff);
    }

    #[test]
    fn packs_whole_files_and_omits_the_ones_that_dont_fit() {
        let a = section("src/a.rs", 3);
        let b = section("src/b.rs", 3);
        let c = section("src/c.rs", 3);
        let diff = format!("{a}{b}{c}");
        // Budget for two whole sections but not the third.
        let max = a.len() + b.len() + 1;
        let out = render_diff_for_prompt(&diff, max);
        // Whole files only — never a partial hunk.
        assert!(out.text.contains("src/a.rs") && out.text.contains("src/b.rs"));
        assert!(!out.text.contains("+line 0 in src/c.rs"));
        assert_eq!(out.omitted_for_budget, vec!["src/c.rs"]);
        assert!(out.low_signal.is_empty());
    }

    #[test]
    fn renders_source_first_and_lists_lockfile_noise_unrendered() {
        // The PR #274 shape: a big lockfile ahead of source in diff order. Source must render; the
        // lockfile must be listed, not rendered — even though it comes first in the diff.
        let lock = section("Cargo.lock", 200);
        let src = section("src/auth/store.rs", 5);
        let diff = format!("{lock}{src}");
        // Budget fits the source but NOT the lock — the old byte-cut would spend it all on the lock.
        let max = src.len() + 50;
        let out = render_diff_for_prompt(&diff, max);
        assert!(
            out.text.contains("src/auth/store.rs"),
            "source file must be rendered"
        );
        assert!(
            !out.text.contains("Cargo.lock"),
            "lockfile must not be rendered"
        );
        assert_eq!(out.low_signal, vec!["Cargo.lock"]);
        assert!(out.omitted_for_budget.is_empty());
    }

    #[test]
    fn renders_lockfile_only_pr_so_the_diff_is_not_blank() {
        let lock = section("Cargo.lock", 3);
        let out = render_diff_for_prompt(&lock, lock.len() + 10);
        assert!(out.text.contains("Cargo.lock"));
        assert!(out.low_signal.is_empty());
        assert!(out.omitted_for_budget.is_empty());
    }

    #[test]
    fn a_single_oversized_file_is_boundary_truncated_not_blanked() {
        let big = section("src/huge.rs", 100);
        let max = big.len() / 2;
        let out = render_diff_for_prompt(&big, max);
        assert!(
            !out.text.is_empty(),
            "must show a partial rather than nothing"
        );
        assert!(out.text.len() <= max);
        assert_eq!(out.omitted_for_budget, vec!["src/huge.rs"]);
    }

    #[test]
    fn whole_diff_under_budget_renders_verbatim() {
        let a = section("src/a.rs", 2);
        let out = render_diff_for_prompt(&a, a.len() + 1000);
        assert_eq!(out.text, a);
        assert!(out.omitted_for_budget.is_empty());
        assert!(out.low_signal.is_empty());
    }
}
