//! Workspace automation (cargo-xtask pattern). Invoked via the justfile, e.g. `cargo xtask ci`.
//! Keeping CI logic here (rather than only in YAML) lets the same gate run locally — shift-left.
//!
//! Every shell-out goes through [`run`], which transparently wraps the command in
//! [`chronic`](https://joeyh.name/code/moreutils/) when it is on `PATH`: output is swallowed on
//! success and printed in full only on failure, so a green gate stays quiet. Without `chronic`
//! installed it falls back to running the command directly.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use serde::Deserialize;

const SCHEMA: &str = "services/control-plane/schema/control-plane.cstack";

fn main() -> anyhow::Result<()> {
    let task = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "help".to_string());
    match task.as_str() {
        "ci" => ci(),
        "fmt" => run("cargo", &["fmt", "--all"]),
        "lint" => run(
            "cargo",
            &["clippy", "--all-targets", "--", "-D", "warnings"],
        ),
        "build" => run("cargo", &["build", "--workspace", "--all-targets"]),
        "test" => test(),
        "validate-schema" => validate_schema(),
        "dependency-hygiene" => dependency_hygiene(),
        _ => {
            eprintln!(
                "usage: cargo xtask <ci|fmt|lint|build|test|validate-schema|dependency-hygiene>"
            );
            Ok(())
        }
    }
}

/// The full local Rust gate: schema check, format check, clippy, then tests.
fn ci() -> anyhow::Result<()> {
    validate_schema()?;
    dependency_hygiene()?;
    run("cargo", &["fmt", "--all", "--", "--check"])?;
    run(
        "cargo",
        &["clippy", "--all-targets", "--", "-D", "warnings"],
    )?;
    test()
}

#[derive(Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
    workspace_members: Vec<String>,
}

#[derive(Deserialize)]
struct CargoPackage {
    id: String,
    manifest_path: PathBuf,
    dependencies: Vec<CargoDependency>,
}

#[derive(Deserialize)]
struct CargoDependency {
    // Cargo metadata reports the resolved package name here even when a manifest uses `package =`
    // to alias it. It also expands workspace-inherited dependencies and includes every target and
    // dependency kind, so none of those manifest forms can bypass the ownership check.
    name: String,
    kind: Option<String>,
}

struct DependencyOwner {
    dependency: &'static str,
    manifest: &'static str,
}

// Transitional R1 ownership. `agent-worker` is deliberately an R2 crate; until it exists, the
// control plane remains the one Restate SDK host for the already-live egress worker (ADR-0074).
const SINGLE_MANIFEST_DEPENDENCIES: &[DependencyOwner] = &[
    DependencyOwner {
        dependency: "restate-sdk",
        manifest: "services/control-plane/Cargo.toml",
    },
    DependencyOwner {
        dependency: "kube",
        manifest: "services/control-plane/Cargo.toml",
    },
    DependencyOwner {
        dependency: "sqlx",
        manifest: "services/control-plane/Cargo.toml",
    },
];

/// Enforce architecture-owned heavyweight dependencies from resolved Cargo metadata.
fn dependency_hygiene() -> anyhow::Result<()> {
    let workspace_root = workspace_root();
    let metadata = load_cargo_metadata(&workspace_root)?;
    validate_dependency_hygiene(&metadata, &workspace_root)
}

fn load_cargo_metadata(workspace_root: &Path) -> anyhow::Result<CargoMetadata> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
        .current_dir(workspace_root)
        .output()
        .context("failed to run `cargo metadata` for dependency hygiene")?;

    if !output.status.success() {
        anyhow::bail!(
            "`cargo metadata --format-version 1 --no-deps --locked` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let metadata: CargoMetadata =
        serde_json::from_slice(&output.stdout).context("failed to decode cargo metadata")?;
    Ok(metadata)
}

fn validate_dependency_hygiene(metadata: &CargoMetadata, root: &Path) -> anyhow::Result<()> {
    let workspace_members: HashSet<&str> = metadata
        .workspace_members
        .iter()
        .map(String::as_str)
        .collect();

    for rule in SINGLE_MANIFEST_DEPENDENCIES {
        let manifests: BTreeSet<String> = metadata
            .packages
            .iter()
            .filter(|package| workspace_members.contains(package.id.as_str()))
            .filter(|package| {
                package
                    .dependencies
                    .iter()
                    .any(|dependency| dependency.name == rule.dependency)
            })
            .map(|package| relative_manifest(&package.manifest_path, root))
            .collect::<anyhow::Result<_>>()?;

        let expected = BTreeSet::from([rule.manifest.to_string()]);
        if manifests != expected {
            anyhow::bail!(
                "dependency hygiene violation: `{}` must appear in exactly `{}`; found [{}]",
                rule.dependency,
                rule.manifest,
                manifests.into_iter().collect::<Vec<_>>().join(", ")
            );
        }
    }

    for package in metadata
        .packages
        .iter()
        .filter(|package| workspace_members.contains(package.id.as_str()))
    {
        for dependency in &package.dependencies {
            if dependency.name == "lci-agent-testkit" && dependency.kind.as_deref() != Some("dev") {
                anyhow::bail!(
                    "dependency hygiene violation: `lci-agent-testkit` must be a dev-dependency; found a {} dependency in `{}`",
                    dependency.kind.as_deref().unwrap_or("normal"),
                    relative_manifest(&package.manifest_path, root)?
                );
            }
        }
    }

    Ok(())
}

fn relative_manifest(manifest: &Path, root: &Path) -> anyhow::Result<String> {
    manifest
        .strip_prefix(root)
        .with_context(|| {
            format!(
                "workspace manifest `{}` is outside workspace root `{}`",
                manifest.display(),
                root.display()
            )
        })
        .map(|path| path.to_string_lossy().replace('\\', "/"))
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask must live directly under the workspace root")
        .to_path_buf()
}

/// Prefer cargo-nextest; fall back to `cargo test` if it is not installed.
fn test() -> anyhow::Result<()> {
    run("cargo", &["nextest", "run"]).or_else(|_| run("cargo", &["test"]))
}

/// Lint the cratestack schema against the documented 0.4.x grammar so the schema-first source of
/// truth cannot silently drift from `src/types.rs` (codegen stays deferred, ADR-0005). Best-effort:
/// skips with a hint when `cratestack-cli` is absent, so CI never hard-requires a young external crate.
fn validate_schema() -> anyhow::Result<()> {
    if on_path("cratestack-cli") {
        run("cratestack-cli", &["validate", SCHEMA])
    } else {
        eprintln!("cratestack-cli not installed — skipping schema validation.");
        eprintln!("Install to enforce: cargo install cratestack-cli --version 0.4.9");
        Ok(())
    }
}

/// Run `cmd args`, wrapped in `chronic` when available (quiet on success, full output on failure).
fn run(cmd: &str, args: &[&str]) -> anyhow::Result<()> {
    let status = if on_path("chronic") {
        Command::new("chronic").arg(cmd).args(args).status()?
    } else {
        Command::new(cmd).args(args).status()?
    };
    if !status.success() {
        anyhow::bail!("`{cmd} {}` failed: {status}", args.join(" "));
    }
    Ok(())
}

/// Whether an executable named `bin` exists on `PATH` (a dependency-free `which`).
fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            let candidate = dir.join(bin);
            candidate.is_file() || Path::new(&candidate).with_extension("exe").is_file()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ScratchWorkspace {
        root: tempfile::TempDir,
    }

    impl ScratchWorkspace {
        fn new(agent_loop_dependencies: &str) -> Self {
            let root = tempfile::tempdir().unwrap();
            write(
                root.path(),
                "Cargo.toml",
                r#"
[workspace]
resolver = "3"
members = [
    "services/control-plane",
    "services/agent-loop",
    "services/review-agent",
]
exclude = [
    "deps/restate-sdk",
    "deps/kube",
    "deps/sqlx",
    "deps/lci-agent-testkit",
]

[workspace.dependencies]
database = { package = "sqlx", path = "deps/sqlx" }
test-support = { package = "lci-agent-testkit", path = "deps/lci-agent-testkit" }
"#,
            );

            write_package(root.path(), "deps/restate-sdk", "restate-sdk", "");
            write_package(root.path(), "deps/kube", "kube", "");
            write_package(root.path(), "deps/sqlx", "sqlx", "");
            write_package(
                root.path(),
                "deps/lci-agent-testkit",
                "lci-agent-testkit",
                "",
            );

            write_package(
                root.path(),
                "services/control-plane",
                "control-plane",
                r#"
[dependencies]
durable-host = { package = "restate-sdk", path = "../../deps/restate-sdk" }

[target.'cfg(unix)'.dev-dependencies]
database.workspace = true

[target.'cfg(unix)'.build-dependencies]
cluster-client = { package = "kube", path = "../../deps/kube" }
"#,
            );
            write_package(
                root.path(),
                "services/agent-loop",
                "lci-agent-loop",
                agent_loop_dependencies,
            );
            write_package(
                root.path(),
                "services/review-agent",
                "lci-review-agent",
                r#"
[target.'cfg(unix)'.dev-dependencies]
test-support.workspace = true
"#,
            );

            let output = Command::new("cargo")
                .args(["generate-lockfile", "--manifest-path"])
                .arg(root.path().join("Cargo.toml"))
                .env("CARGO_NET_OFFLINE", "true")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "fixture lockfile generation failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );

            Self { root }
        }

        fn metadata(&self) -> CargoMetadata {
            load_cargo_metadata(&self.root_path()).unwrap()
        }

        fn validate(&self) -> anyhow::Result<()> {
            let metadata = self.metadata();
            validate_dependency_hygiene(&metadata, &self.root_path())
        }

        fn root_path(&self) -> PathBuf {
            self.root.path().canonicalize().unwrap()
        }
    }

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents.trim_start()).unwrap();
    }

    fn write_package(root: &Path, relative: &str, name: &str, dependencies: &str) {
        write(
            root,
            &format!("{relative}/Cargo.toml"),
            &format!(
                r#"
[package]
name = "{name}"
version = "0.0.0"
edition = "2024"
publish = false
{dependencies}
"#
            ),
        );
        write(root, &format!("{relative}/src/lib.rs"), "");
    }

    fn dependency(name: &str) -> CargoDependency {
        CargoDependency {
            name: name.to_string(),
            kind: None,
        }
    }

    fn dev_dependency(name: &str) -> CargoDependency {
        CargoDependency {
            name: name.to_string(),
            kind: Some("dev".to_string()),
        }
    }

    fn package(id: &str, manifest: &str, dependencies: &[&str]) -> CargoPackage {
        CargoPackage {
            id: id.to_string(),
            manifest_path: PathBuf::from("/workspace").join(manifest),
            dependencies: dependencies.iter().map(|name| dependency(name)).collect(),
        }
    }

    fn valid_metadata() -> CargoMetadata {
        CargoMetadata {
            packages: vec![
                package(
                    "control-plane",
                    "services/control-plane/Cargo.toml",
                    &["restate-sdk", "kube", "sqlx"],
                ),
                package(
                    "lci-agent-loop",
                    "services/agent-loop/Cargo.toml",
                    &["lci-agent-step"],
                ),
            ],
            workspace_members: vec!["control-plane".to_string(), "lci-agent-loop".to_string()],
        }
    }

    #[test]
    fn approved_dependency_owners_pass() {
        validate_dependency_hygiene(&valid_metadata(), Path::new("/workspace")).unwrap();
    }

    #[test]
    fn heavyweight_dependency_in_agent_crate_fails() {
        let mut metadata = valid_metadata();
        metadata.packages[1].dependencies.push(dependency("sqlx"));

        let error = validate_dependency_hygiene(&metadata, Path::new("/workspace")).unwrap_err();
        assert!(error.to_string().contains("services/agent-loop"));
    }

    #[test]
    fn heavyweight_dev_or_target_dependency_cannot_bypass_ownership() {
        let mut metadata = valid_metadata();
        metadata.packages[1].dependencies.push(CargoDependency {
            name: "restate-sdk".to_string(),
            kind: Some("dev".to_string()),
        });

        let error = validate_dependency_hygiene(&metadata, Path::new("/workspace")).unwrap_err();
        assert!(error.to_string().contains("services/agent-loop"));
    }

    #[test]
    fn missing_approved_dependency_owner_fails() {
        let mut metadata = valid_metadata();
        metadata.packages[0]
            .dependencies
            .retain(|dependency| dependency.name != "kube");

        let error = validate_dependency_hygiene(&metadata, Path::new("/workspace")).unwrap_err();
        assert!(error.to_string().contains("found []"));
    }

    #[test]
    fn non_workspace_packages_do_not_affect_ownership() {
        let mut metadata = valid_metadata();
        metadata.packages.push(package(
            "third-party",
            "vendor/third-party/Cargo.toml",
            &["sqlx"],
        ));

        validate_dependency_hygiene(&metadata, Path::new("/workspace")).unwrap();
    }

    #[test]
    fn agent_testkit_is_allowed_as_a_dev_dependency() {
        let mut metadata = valid_metadata();
        metadata.packages[1]
            .dependencies
            .push(dev_dependency("lci-agent-testkit"));

        validate_dependency_hygiene(&metadata, Path::new("/workspace")).unwrap();
    }

    #[test]
    fn agent_testkit_as_a_normal_dependency_fails() {
        let mut metadata = valid_metadata();
        metadata.packages[1]
            .dependencies
            .push(dependency("lci-agent-testkit"));

        let error = validate_dependency_hygiene(&metadata, Path::new("/workspace")).unwrap_err();
        assert!(error.to_string().contains("must be a dev-dependency"));
    }

    #[test]
    fn agent_testkit_as_a_build_dependency_fails() {
        let mut metadata = valid_metadata();
        metadata.packages[1].dependencies.push(CargoDependency {
            name: "lci-agent-testkit".to_string(),
            kind: Some("build".to_string()),
        });

        let error = validate_dependency_hygiene(&metadata, Path::new("/workspace")).unwrap_err();
        assert!(error.to_string().contains("build dependency"));
    }

    #[test]
    fn scratch_workspace_resolves_alias_workspace_target_and_dependency_kinds() {
        let scratch = ScratchWorkspace::new("");
        let metadata = scratch.metadata();
        scratch.validate().unwrap();

        let control_plane = metadata
            .packages
            .iter()
            .find(|package| {
                package
                    .manifest_path
                    .ends_with("services/control-plane/Cargo.toml")
            })
            .unwrap();
        assert!(
            control_plane.dependencies.iter().any(|dependency| {
                dependency.name == "restate-sdk" && dependency.kind.is_none()
            })
        );
        assert!(control_plane.dependencies.iter().any(|dependency| {
            dependency.name == "sqlx" && dependency.kind.as_deref() == Some("dev")
        }));
        assert!(control_plane.dependencies.iter().any(|dependency| {
            dependency.name == "kube" && dependency.kind.as_deref() == Some("build")
        }));

        let review_agent = metadata
            .packages
            .iter()
            .find(|package| {
                package
                    .manifest_path
                    .ends_with("services/review-agent/Cargo.toml")
            })
            .unwrap();
        assert!(review_agent.dependencies.iter().any(|dependency| {
            dependency.name == "lci-agent-testkit" && dependency.kind.as_deref() == Some("dev")
        }));
    }

    #[test]
    fn scratch_workspace_rejects_heavyweight_dependencies_in_every_manifest_form() {
        let cases = [
            (
                "normal package alias",
                r#"
[dependencies]
database-client = { package = "sqlx", path = "../../deps/sqlx" }
"#,
            ),
            (
                "workspace-inherited dev dependency",
                r#"
[dev-dependencies]
database.workspace = true
"#,
            ),
            (
                "build dependency",
                r#"
[build-dependencies]
cluster-client = { package = "kube", path = "../../deps/kube" }
"#,
            ),
            (
                "inactive target dependency",
                r#"
[target.'cfg(windows)'.dependencies]
durable-host = { package = "restate-sdk", path = "../../deps/restate-sdk" }
"#,
            ),
        ];

        for (case, dependencies) in cases {
            let error = ScratchWorkspace::new(dependencies).validate().unwrap_err();
            assert!(
                error.to_string().contains("services/agent-loop/Cargo.toml"),
                "{case} escaped the check: {error}"
            );
        }
    }

    #[test]
    fn scratch_workspace_enforces_agent_testkit_as_dev_only() {
        for (case, dependencies) in [
            (
                "normal",
                r#"
[dependencies]
test-support.workspace = true
"#,
            ),
            (
                "build",
                r#"
[build-dependencies]
test-support.workspace = true
"#,
            ),
        ] {
            let error = ScratchWorkspace::new(dependencies).validate().unwrap_err();
            assert!(
                error.to_string().contains("must be a dev-dependency"),
                "{case} testkit dependency escaped the check: {error}"
            );
        }
    }
}
