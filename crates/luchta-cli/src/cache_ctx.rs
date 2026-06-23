use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use luchta_cache::{
    env_hash, pkg_dep_hash, resolve_inputs_with_semantics, resolve_outputs, task_spec_hash,
    CurrentState, FileEntry, FileStateResolver,
};
use luchta_engine::{expand_input_patterns, InputExpansionError};
use luchta_lockfiles::{parse_lockfile, Lockfile};
use luchta_types::{EnvSpec, PackageName, TaskDefinition};
use luchta_workspace::{PackageGraph, PackageNode};
use miette::{IntoDiagnostic, Result};
use serde::Deserialize;

pub(crate) enum LockfileState {
    Absent,
    Parsed(Arc<dyn Lockfile>),
    Failed(String),
}

/// Reads and parses `workspace_root/yarn.lock` once, returning the outcome.
///
/// Maps the four current per-task outcomes so call sites behave identically:
/// missing/empty -> `Absent`, parses -> `Parsed`, parse error or non-NotFound
/// I/O error -> `Failed` (which `gather_pkg_dep_pairs` surfaces as `Err`, so the
/// caller disables caching for that task).
pub(crate) fn load_lockfile_state(workspace_root: &Path) -> LockfileState {
    match fs::read_to_string(workspace_root.join("yarn.lock")) {
        Ok(contents) if contents.trim().is_empty() => LockfileState::Absent,
        Ok(contents) => match parse_lockfile(&contents) {
            Ok(parsed) => LockfileState::Parsed(Arc::<dyn Lockfile>::from(parsed)),
            Err(e) => LockfileState::Failed(e.to_string()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => LockfileState::Absent,
        Err(e) => LockfileState::Failed(format!("failed to read yarn.lock: {e}")),
    }
}

pub struct PackageDirResolver {
    package_dir: PathBuf,
    repo_root: PathBuf,
    source_pkg: PackageName,
    package_graph: PackageGraph,
}

impl PackageDirResolver {
    #[must_use]
    pub fn new(
        package_dir: PathBuf,
        repo_root: PathBuf,
        source_pkg: PackageName,
        package_graph: PackageGraph,
    ) -> Self {
        Self {
            package_dir,
            repo_root,
            source_pkg,
            package_graph,
        }
    }
}

fn format_expansion_error(error: &InputExpansionError) -> String {
    error.to_string()
}

impl FileStateResolver for PackageDirResolver {
    fn resolve_inputs(&self, patterns: &[String]) -> luchta_cache::Result<Vec<FileEntry>> {
        let requests = expand_input_patterns(
            patterns,
            &self.source_pkg,
            &self.package_graph,
            &self.repo_root,
        )
        .map_err(|e| luchta_cache::CacheError::InputExpansion(format_expansion_error(&e)))?;
        resolve_inputs_with_semantics(&requests)
    }

    fn resolve_outputs(&self, patterns: &[String]) -> luchta_cache::Result<Vec<FileEntry>> {
        resolve_outputs(&self.package_dir, patterns)
    }

    fn blake3_file(&self, path: &Path) -> luchta_cache::Result<[u8; 32]> {
        let full_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.package_dir.join(path)
        };
        luchta_cache::blake3_file(&full_path)
    }
}

#[derive(Debug, Deserialize)]
struct PackageJsonExternalDeps {
    #[serde(default)]
    dependencies: HashMap<String, String>,
    #[serde(default, rename = "devDependencies")]
    dev_dependencies: HashMap<String, String>,
    #[serde(default, rename = "optionalDependencies")]
    optional_dependencies: HashMap<String, String>,
}

pub(crate) fn gather_pkg_dep_pairs(
    package: &PackageNode,
    package_graph: Option<&PackageGraph>,
    lockfile: &LockfileState,
) -> Result<Vec<(String, String)>> {
    let lockfile = match lockfile {
        LockfileState::Absent => return Ok(Vec::new()),
        LockfileState::Failed(msg) => return Err(miette::miette!("{msg}")),
        LockfileState::Parsed(lf) => lf.as_ref(),
    };
    let package_json = fs::read_to_string(package.path.join("package.json")).into_diagnostic()?;
    let package_json: PackageJsonExternalDeps =
        serde_json::from_str(&package_json).into_diagnostic()?;

    let mut closure = package_graph
        .map(|graph| graph.dependencies_of(&package.name).into_diagnostic())
        .transpose()?
        .unwrap_or_default();
    closure.push(package);

    let workspace_names: BTreeSet<_> = closure
        .iter()
        .map(|node| node.name.as_str().to_owned())
        .collect();
    let workspace_paths: BTreeSet<_> = closure
        .iter()
        .map(|node| node.path.to_string_lossy().into_owned())
        .collect();
    let mut pairs = BTreeSet::new();

    for (name, version) in package_json
        .dependencies
        .into_iter()
        .chain(package_json.dev_dependencies)
        .chain(package_json.optional_dependencies)
    {
        collect_dep_pairs_for_package(
            lockfile,
            &package.path,
            &workspace_names,
            &workspace_paths,
            &name,
            &version,
            &mut pairs,
        )?;
    }

    Ok(pairs.into_iter().collect())
}

fn collect_dep_pairs_for_package(
    lockfile: &dyn luchta_lockfiles::Lockfile,
    package_path: &Path,
    workspace_names: &BTreeSet<String>,
    workspace_paths: &BTreeSet<String>,
    name: &str,
    version: &str,
    pairs: &mut BTreeSet<(String, String)>,
) -> Result<()> {
    if should_skip_dependency_spec(name, version, workspace_names, workspace_paths) {
        return Ok(());
    }

    let Some(resolved) = lockfile
        .resolve_package(&package_path.to_string_lossy(), name, version)
        .into_diagnostic()?
    else {
        return Ok(());
    };

    pairs.insert((name.to_owned(), resolved.version.clone()));
    for (dep_name, dep_version) in lockfile.all_dependencies(&resolved.key).into_diagnostic()? {
        pairs.insert((dep_name, dep_version));
    }

    Ok(())
}

pub fn build_current_state<'a>(
    task_def: &'a TaskDefinition,
    merged_env: &'a BTreeMap<String, EnvSpec>,
    dep_outputs: BTreeMap<String, [u8; 32]>,
    pkg_dep_pairs: &'a [(String, String)],
    resolver: &'a dyn FileStateResolver,
    nonce: Option<&str>,
) -> CurrentState<'a> {
    CurrentState {
        task_spec_hash: task_spec_hash(task_def, nonce),
        // Hash declared merged EnvSpec only. Built-in passthrough whitelist vars are
        // injected later into ExecutionRequest.env, so whitelist-only ambient changes
        // never enter env_hash.
        env_hash: env_hash(merged_env, |name| std::env::var(name).ok()),
        pkg_dep_hash: pkg_dep_hash(pkg_dep_pairs),
        dep_outputs,
        declared_input_patterns: &task_def.inputs,
        declared_output_patterns: &task_def.outputs,
        resolver,
    }
}

fn should_skip_dependency_spec(
    name: &str,
    version: &str,
    workspace_names: &BTreeSet<String>,
    workspace_paths: &BTreeSet<String>,
) -> bool {
    version.starts_with("workspace:")
        || version.starts_with("link:")
        || version.starts_with("portal:")
        || version.starts_with("file:")
        || workspace_names.contains(name)
        || workspace_paths.iter().any(|path| version.contains(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use luchta_types::PackageName;
    use luchta_workspace::PackageNode;
    use tempfile::TempDir;

    #[test]
    fn load_lockfile_state_missing_file_is_absent() {
        // No yarn.lock at all -> Absent (caching proceeds with empty pairs).
        let dir = TempDir::new().unwrap();
        assert!(matches!(
            load_lockfile_state(dir.path()),
            LockfileState::Absent
        ));
    }

    #[test]
    fn load_lockfile_state_empty_file_is_absent() {
        // Present-but-empty (whitespace-only) yarn.lock -> Absent.
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("yarn.lock"), "   \n\t").unwrap();
        assert!(matches!(
            load_lockfile_state(dir.path()),
            LockfileState::Absent
        ));
    }

    #[test]
    fn load_lockfile_state_unparseable_file_is_failed() {
        // Non-empty but unparseable yarn.lock -> Failed (cache disabled per task).
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("yarn.lock"), "this is not a lockfile {{{").unwrap();
        assert!(matches!(
            load_lockfile_state(dir.path()),
            LockfileState::Failed(_)
        ));
    }

    #[test]
    fn gather_pkg_dep_pairs_absent_lockfile_yields_no_pairs() {
        // `LockfileState::Absent` (missing/empty yarn.lock) must short-circuit to
        // an empty pair list before any per-package read, so caching proceeds.
        let package = PackageNode::new(PackageName::new("pkg"), "/nonexistent");
        let pairs = gather_pkg_dep_pairs(&package, None, &LockfileState::Absent)
            .expect("Absent lockfile should yield Ok");
        assert!(pairs.is_empty());
    }

    #[test]
    fn gather_pkg_dep_pairs_failed_lockfile_errors() {
        // `LockfileState::Failed` (parse error or non-NotFound I/O error) must
        // surface as `Err` so both call sites disable caching for the task.
        let package = PackageNode::new(PackageName::new("pkg"), "/nonexistent");
        let err = gather_pkg_dep_pairs(&package, None, &LockfileState::Failed("boom".into()))
            .expect_err("Failed lockfile should yield Err");
        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn should_skip_dependency_spec_returns_true_for_workspace_prefix() {
        let workspace_names: BTreeSet<String> = BTreeSet::new();
        let workspace_paths: BTreeSet<String> = BTreeSet::new();

        assert!(should_skip_dependency_spec(
            "some-dep",
            "workspace:*",
            &workspace_names,
            &workspace_paths
        ));
        assert!(should_skip_dependency_spec(
            "some-dep",
            "workspace:^1.0.0",
            &workspace_names,
            &workspace_paths
        ));
    }

    #[test]
    fn should_skip_dependency_spec_returns_true_for_workspace_name_match() {
        let mut workspace_names: BTreeSet<String> = BTreeSet::new();
        workspace_names.insert("internal-pkg".to_string());
        let workspace_paths: BTreeSet<String> = BTreeSet::new();

        // Workspace name in workspace_names should be skipped
        assert!(should_skip_dependency_spec(
            "internal-pkg",
            "1.0.0",
            &workspace_names,
            &workspace_paths
        ));

        // Non-workspace name should not be skipped
        assert!(!should_skip_dependency_spec(
            "external-pkg",
            "1.0.0",
            &workspace_names,
            &workspace_paths
        ));
    }
}
