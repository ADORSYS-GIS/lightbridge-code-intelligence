//! Language-scoping the opengrep ruleset to the languages actually present in a PR's changed files —
//! the perf lever documented on [`super::scan`]: `opengrep scan --config <dir>` loads every rule under
//! the path before it matches anything, so pointing at the whole multi-language tree cost ~4 min/scan
//! even for one file (observed live).

use std::collections::BTreeSet;

/// The opengrep-rules language subdirectories to scan for a set of changed files. Maps each file's
/// extension / name to its rule dir, and ALWAYS adds `generic` (language-agnostic secret/keyword rules
/// that apply to any changed file, including docs/config) whenever there is at least one target. Pure
/// (no fs) so it's unit-tested; the caller filters the names to dirs that actually exist on disk. A
/// `BTreeSet` dedups + orders the result for deterministic scans.
pub(crate) fn rule_dir_names_for_targets(targets: &[String]) -> BTreeSet<&'static str> {
    let mut names = BTreeSet::new();
    for t in targets {
        for dir in rule_dirs_for_file(t) {
            names.insert(*dir);
        }
    }
    if !targets.is_empty() {
        names.insert("generic");
    }
    names
}

/// The opengrep-rules dir(s) that apply to one file, by filename then extension (lower-cased). Empty when
/// the file isn't a recognized code language — it then gets only `generic` (added by the caller). Maps to
/// dir names that exist in the vendored opengrep-rules tree; a name that's absent on disk is filtered by
/// the caller, so an over-broad entry here is harmless.
fn rule_dirs_for_file(path: &str) -> &'static [&'static str] {
    let name = path.rsplit('/').next().unwrap_or(path);
    if name == "Dockerfile" || name.starts_with("Dockerfile.") || name.ends_with(".dockerfile") {
        return &["dockerfile"];
    }
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "py" | "pyi" => &["python", "trusted_python"],
        "js" | "jsx" | "mjs" | "cjs" => &["javascript"],
        "ts" | "tsx" | "mts" | "cts" => &["typescript"],
        "go" => &["go"],
        "rs" => &["rust"],
        "rb" | "rake" => &["ruby"],
        "php" | "phtml" => &["php"],
        "java" => &["java"],
        "kt" | "kts" => &["kotlin"],
        "scala" | "sc" => &["scala"],
        "cs" => &["csharp"],
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hxx" => &["c"],
        "swift" => &["swift"],
        "clj" | "cljs" | "cljc" | "edn" => &["clojure"],
        "ex" | "exs" => &["elixir"],
        "ml" | "mli" => &["ocaml"],
        "sol" => &["solidity"],
        "sh" | "bash" => &["bash"],
        "html" | "htm" => &["html"],
        "json" => &["json"],
        "tf" | "tfvars" => &["terraform"],
        "yaml" | "yml" => &["yaml"],
        "cls" | "trigger" => &["apex"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_dirs_map_extensions_and_filenames() {
        assert_eq!(
            rule_dirs_for_file("src/app.py"),
            &["python", "trusted_python"]
        );
        assert_eq!(rule_dirs_for_file("web/main.ts"), &["typescript"]);
        assert_eq!(rule_dirs_for_file("a/b/x.rs"), &["rust"]);
        assert_eq!(rule_dirs_for_file("Dockerfile"), &["dockerfile"]);
        assert_eq!(
            rule_dirs_for_file("deploy/Dockerfile.prod"),
            &["dockerfile"]
        );
        // Unknown / non-code → no language dir (the caller still adds `generic`).
        assert_eq!(rule_dirs_for_file("README.md"), &[] as &[&str]);
        assert_eq!(rule_dirs_for_file("Cargo.lock"), &[] as &[&str]);
    }

    #[test]
    fn rule_dir_names_scope_to_present_languages_plus_generic() {
        // A mixed code PR: python + typescript rule dirs, plus generic, deduped + ordered.
        let names = rule_dir_names_for_targets(&[
            "src/a.py".to_string(),
            "src/b.py".to_string(),
            "web/c.tsx".to_string(),
        ]);
        let got: Vec<&str> = names.into_iter().collect();
        assert_eq!(
            got,
            vec!["generic", "python", "trusted_python", "typescript"]
        );

        // A docs-only PR scopes to `generic` ONLY (a cheap secrets pass, not a full-tree load).
        let docs: Vec<&str> = rule_dir_names_for_targets(&["docs/x.md".to_string()])
            .into_iter()
            .collect();
        assert_eq!(docs, vec!["generic"]);

        // No targets → no rule sets at all (the caller skips opengrep entirely).
        assert!(rule_dir_names_for_targets(&[]).is_empty());
    }
}
