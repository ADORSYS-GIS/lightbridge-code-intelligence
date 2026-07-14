//! Shared review-signal classification for a changed file's repo-relative path (#306).
//!
//! Two independent call sites need to answer the same question — "does this path carry real,
//! hand-authored review signal?" — and used to answer it two different ways: [`crate::prompt::diff`]
//! deprioritised lockfiles/minified bundles out of the rendered diff, while
//! [`crate::policies::coverage`] counted *every* changed path (lockfiles included) toward the coverage
//! gate's denominator. A production run (`ADORSYS-GIS/webank-mobile#145`) disclosed "examined 26 of 40
//! changed files" when only ~29 of those 40 were reviewable product source — 11 were generated l10n
//! output, tests, or config/docs that no amount of review attention would improve. This module gives
//! both call sites one shared answer, classified on path/extension/directory convention alone (neither
//! call site has the diff hunk content available).
//!
//! Deliberately conservative: only well-known generator/test/config conventions are matched, never a
//! bare extension alone (a risk the ticket calls out explicitly — over-aggressive exclusion could hide
//! a hand-edited "generated" file, or swallow a real CI/deploy config change).

/// Coarse review-signal bucket for one changed file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileSignal {
    /// Hand-written product source: counts toward review coverage.
    Source,
    /// Lockfiles, minified/bundled build output, or snapshot dumps.
    LockOrVendor,
    /// Generator output (codegen, gen-l10n, protobuf, …) — no hand-authored logic to review.
    Generated,
    /// Test code — exercised by CI, not the review's coverage target.
    Test,
    /// Docs or deployment/environment config with no application logic.
    Config,
}

impl FileSignal {
    /// Whether this bucket should be excluded from a "real source coverage" ratio.
    #[must_use]
    pub fn is_low_signal(self) -> bool {
        !matches!(self, FileSignal::Source)
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            FileSignal::Source => "source",
            FileSignal::LockOrVendor => "lockfile/vendored",
            FileSignal::Generated => "generated",
            FileSignal::Test => "test",
            FileSignal::Config => "config/docs",
        }
    }
}

/// Classify a changed file's path into the bucket that best explains why it does (or doesn't) carry
/// hand-authored review signal. Matched on the normalised (forward-slash) path so
/// `a\b\Cargo.lock` classifies the same as `a/b/Cargo.lock`.
#[must_use]
pub fn classify_path(path: &str) -> FileSignal {
    let normalized;
    let p = if path.contains('\\') {
        normalized = path.replace('\\', "/");
        normalized.as_str()
    } else {
        path
    };
    let name = p.rsplit('/').next().unwrap_or(p);

    if is_lock_or_vendor(name) {
        FileSignal::LockOrVendor
    } else if is_generated(name) {
        FileSignal::Generated
    } else if is_test(p, name) {
        FileSignal::Test
    } else if is_config_or_docs(p, name) {
        FileSignal::Config
    } else {
        FileSignal::Source
    }
}

/// Exact lock / dependency-manifest files across ecosystems, plus minified/bundled/map/snapshot noise.
fn is_lock_or_vendor(name: &str) -> bool {
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
    const NOISE_SUFFIXES: &[&str] = &[".min.js", ".min.css", ".map", ".snap", ".bundle.js"];
    NOISE_SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// Generator output: codegen conventions whose bytes are machine-produced, never a bare extension alone
/// (a `.dart`/`.go` file is not generated just because of its extension).
fn is_generated(name: &str) -> bool {
    const GENERATED_SUFFIXES: &[&str] = &[
        ".g.dart",
        ".freezed.dart",
        ".gr.dart",
        ".config.dart", // Dart build_runner family
        ".pb.go",
        ".pb.gw.go", // protobuf/grpc-gateway (Go)
        "_pb2.py",
        "_pb2_grpc.py", // protobuf (Python)
        ".generated.ts",
        ".generated.go",
    ];
    if GENERATED_SUFFIXES.iter().any(|s| name.ends_with(s)) {
        return true;
    }
    // `flutter gen-l10n`'s default output class is `AppLocalizations`, emitted as
    // `app_localizations.dart` + one `app_localizations_<locale>.dart` per locale.
    if name.starts_with("app_localizations") && name.ends_with(".dart") {
        return true;
    }
    // `.arb` (Application Resource Bundle) is gen-l10n's *input* translation data, not source.
    name.ends_with(".arb")
}

/// Test code, by filename convention or a `__tests__`/`__snapshots__` directory segment.
fn is_test(path: &str, name: &str) -> bool {
    const TEST_SUFFIXES: &[&str] = &[
        "_test.go",
        "_test.dart",
        "_test.py",
        ".test.ts",
        ".test.tsx",
        ".test.js",
        ".test.jsx",
        ".spec.ts",
        ".spec.tsx",
        ".spec.js",
        "_spec.rb",
    ];
    if TEST_SUFFIXES.iter().any(|s| name.ends_with(s)) {
        return true;
    }
    const TEST_DIR_SEGMENTS: &[&str] = &["__tests__", "__snapshots__"];
    path.split('/').any(|seg| TEST_DIR_SEGMENTS.contains(&seg))
}

/// Docs (prose, no logic) and deployment/environment config nested under a directory whose name marks
/// it as configuration — narrow on purpose: a `.yml`/`.json` file only counts as config when a path
/// segment is literally `config`/`configs` or ends in `-config`/`_config`, so CI workflows
/// (`.github/workflows/*.yml`), Kubernetes manifests, etc. still gate review coverage.
fn is_config_or_docs(path: &str, name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    const DOC_SUFFIXES: &[&str] = &[".md", ".mdx", ".rst"];
    if DOC_SUFFIXES.iter().any(|s| lower.ends_with(s)) {
        return true;
    }
    const DATA_SUFFIXES: &[&str] = &[
        ".yml", ".yaml", ".json", ".toml", ".ini", ".conf", ".cfg", ".env",
    ];
    if !DATA_SUFFIXES.iter().any(|s| lower.ends_with(s)) {
        return false;
    }
    path.split('/').any(|seg| {
        let seg = seg.to_ascii_lowercase();
        seg == "config" || seg == "configs" || seg.ends_with("-config") || seg.ends_with("_config")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_lock_and_vendor_noise() {
        for p in [
            "Cargo.lock",
            "clients/app/pnpm-lock.yaml",
            "web\\yarn.lock",
            "dist/app.min.js",
            "bundle.js.map",
            "components/__snapshots__/a.snap",
        ] {
            assert_eq!(classify_path(p), FileSignal::LockOrVendor, "{p}");
        }
    }

    #[test]
    fn classifies_generated_output() {
        for p in [
            "lib/l10n/app_localizations.dart",
            "lib/l10n/app_localizations_en.dart",
            "lib/l10n/app_localizations_fr.dart",
            "lib/l10n/app_en.arb",
            "lib/l10n/app_fr.arb",
            "lib/models/user.g.dart",
            "lib/models/user.freezed.dart",
            "proto/gen/service.pb.go",
            "proto/gen/service_pb2.py",
        ] {
            assert_eq!(classify_path(p), FileSignal::Generated, "{p}");
        }
    }

    #[test]
    fn classifies_tests() {
        for p in [
            "payments/service_test.go",
            "pendingp2p/service_test.go",
            "lib/models/pending_transfer_model_test.dart",
            "lib/features/referral_capture_test.dart",
            "src/foo.test.ts",
            "src/bar.spec.tsx",
            "__tests__/baz.js",
        ] {
            assert_eq!(classify_path(p), FileSignal::Test, "{p}");
        }
    }

    #[test]
    fn classifies_config_and_docs() {
        for p in [
            "docker/fineract-config/base-config.yml",
            "pendingp2p/README.md",
            "docs/adr/0063.md",
        ] {
            assert_eq!(classify_path(p), FileSignal::Config, "{p}");
        }
    }

    #[test]
    fn keeps_hand_written_source_and_real_config_as_source() {
        for p in [
            "src/auth/store.rs",
            "Cargo.toml",
            "clients/lci/src/main.rs",
            "app.js",
            "lib/screens/send_screen.dart",
            "lib/screens/recipient_confirm_screen.dart",
            "lib/repositories/pending_p2p_repository.dart",
            // A real deploy manifest, not a "*-config" directory: must NOT be swallowed.
            ".github/workflows/ci.yml",
            "k8s/deployment.yaml",
        ] {
            assert_eq!(classify_path(p), FileSignal::Source, "{p}");
        }
    }

    #[test]
    fn normalizes_windows_separators_before_classifying() {
        assert_eq!(
            classify_path("docker\\fineract-config\\base-config.yml"),
            FileSignal::Config
        );
    }
}
