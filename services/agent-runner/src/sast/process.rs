//! Spawning the `opengrep` subprocess and validating the paths handed to it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;

use super::finding::truncate;
use crate::bootstrap::config::SastConfig;

/// Whether a changed-file path is safe to hand to `checkout.join()` for scanning: relative and not
/// climbing out of the tree with `..`. Guards the `Path::join` footgun where an absolute path silently
/// discards the base, which could redirect the scan to an arbitrary file on the runner.
pub(crate) fn is_safe_relative(path: &str) -> bool {
    let p = Path::new(path);
    !p.is_absolute()
        && !p
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
}

/// Spawn `opengrep scan` over the targets and return the SARIF it writes. Output goes to a private file
/// in the **system temp dir** (outside the checkout, so a repo can't plant or clobber it). We use
/// temp_dir rather than the checkout's parent: opengrep takes an *absolute* `--sarif-output`, so there's
/// no reason to depend on the workdir layout, and `/tmp` is always writable by the non-root runner user.
/// A non-zero exit is NOT by itself an error — opengrep exits non-zero when it finds matches; we treat
/// "the SARIF file exists and parses" as success and only bail when the file never appeared.
pub(crate) async fn run_opengrep(
    config: &SastConfig,
    checkout: &Path,
    targets: &[String],
    config_paths: &[PathBuf],
) -> anyhow::Result<String> {
    let checkout_abs = tokio::fs::canonicalize(checkout)
        .await
        .with_context(|| format!("canonicalizing {}", checkout.display()))?;
    let out_dir = std::env::temp_dir().join("sast-run");
    tokio::fs::create_dir_all(&out_dir)
        .await
        .with_context(|| format!("creating {}", out_dir.display()))?;
    let sarif_path = out_dir.join("opengrep.sarif");
    // Remove a stale artifact from a prior attempt so a failed run can't be read as a stale success.
    let _ = tokio::fs::remove_file(&sarif_path).await;

    let mut cmd = tokio::process::Command::new(&config.bin);
    cmd.arg("scan");
    // One `--config` per resolved rule dir (language-scoped, ADR-0061 perf). `--config` is repeatable;
    // opengrep unions the rules across them.
    for path in config_paths {
        cmd.arg("--config").arg(path);
    }
    cmd.arg(format!("--sarif-output={}", sarif_path.display()))
        // Quiet stdout (we read the SARIF file). We deliberately do NOT pass `--error`, so a scan that
        // *finds* something still exits 0 — we judge success by "did the SARIF file get written", not by
        // the exit code (opengrep exits non-zero on findings when `--error` is set).
        .arg("--quiet")
        // Best-effort hermeticity: suppress the upstream version ping + metrics so a locked-down pod
        // makes no outbound call for the scan. These are env vars (silently ignored if opengrep doesn't
        // recognize them) rather than CLI flags — an unknown *flag* would be a fatal arg error, an
        // unknown env var is harmless. Both the semgrep- and opengrep-prefixed names are set since
        // opengrep inherits semgrep's CLI surface.
        .env("SEMGREP_ENABLE_VERSION_CHECK", "0")
        .env("OPENGREP_ENABLE_VERSION_CHECK", "0")
        .env("SEMGREP_SEND_METRICS", "off")
        .env("OPENGREP_SEND_METRICS", "off")
        // Force UTF-8. opengrep is frozen-CPython and reads its rule files with Python's locale-default
        // codec; the slim runner image has NO locale set, so that default is **ASCII** and any rule file
        // with a non-ASCII byte (an em-dash / smart-quote in a rule message — opengrep-rules has many)
        // crashes the config load with `UnicodeDecodeError`, exit 2, no SARIF (every scan silently fails,
        // observed live). `PYTHONUTF8=1` is the locale-independent fix (CPython UTF-8 Mode); LANG/LC_ALL
        // are belt-and-suspenders for any path that consults the locale rather than the interpreter flag.
        .env("PYTHONUTF8", "1")
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .current_dir(&checkout_abs)
        // Scan only the changed files (relative to the checkout cwd).
        .args(targets);

    let run = tokio::time::timeout(Duration::from_secs(config.timeout_secs), cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("opengrep scan timed out after {}s", config.timeout_secs))?
        .context("spawning opengrep (is it on PATH in the image?)")?;

    if !sarif_path.exists() {
        // No SARIF written → opengrep didn't run to completion (bad rules path, crash, etc.). Surface
        // BOTH streams (bounded): under `--quiet` a crash traceback can land on stdout, so stderr alone
        // was empty and undiagnosable on the first live failure — include stdout too.
        let stderr = String::from_utf8_lossy(&run.stderr);
        let stdout = String::from_utf8_lossy(&run.stdout);
        let detail = format!("{stderr}{stdout}");
        anyhow::bail!(
            "opengrep produced no SARIF (exit {}): {}",
            run.status,
            truncate(detail.trim(), 600)
        );
    }
    // Bounded read: the scan is scoped to the PR's changed files, so the SARIF is small in practice, but
    // cap it defensively so a pathological diff can't OOM the runner pod reading untrusted output into
    // memory. A truncated read just fails to parse → zero findings (logged, non-fatal), never a crash.
    use tokio::io::AsyncReadExt;
    const MAX_SARIF_BYTES: u64 = 16 * 1024 * 1024;
    let file = tokio::fs::File::open(&sarif_path)
        .await
        .with_context(|| format!("opening {}", sarif_path.display()))?;
    let mut buf = Vec::new();
    file.take(MAX_SARIF_BYTES)
        .read_to_end(&mut buf)
        .await
        .with_context(|| format!("reading {}", sarif_path.display()))?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_safe_relative_rejects_absolute_and_parent_escapes() {
        assert!(is_safe_relative("src/exec.rs"));
        assert!(is_safe_relative("a/b/c.ts"));
        assert!(!is_safe_relative("/etc/passwd"), "absolute path rejected");
        assert!(
            !is_safe_relative("../../etc/passwd"),
            "parent escape rejected"
        );
        assert!(
            !is_safe_relative("src/../../etc/passwd"),
            "mid-path .. rejected"
        );
    }
}
