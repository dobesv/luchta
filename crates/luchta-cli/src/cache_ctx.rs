use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
};

use luchta_cache::{
    env_hash, pkg_dep_hash, resolve_inputs, resolve_outputs, task_spec_hash, CurrentState,
    FileEntry, FileStateResolver,
};
use luchta_lockfiles::parse_lockfile;
use luchta_types::TaskDefinition;
use luchta_workspace::{PackageGraph, PackageNode};
use miette::{IntoDiagnostic, Result};
use serde::Deserialize;

pub struct PackageDirResolver {
    package_dir: PathBuf,
}

impl PackageDirResolver {
    #[must_use]
    pub fn new(package_dir: PathBuf) -> Self {
        Self { package_dir }
    }
}

impl FileStateResolver for PackageDirResolver {
    fn resolve_inputs(&self, patterns: &[String]) -> luchta_cache::Result<Vec<FileEntry>> {
        resolve_inputs(&self.package_dir, patterns)
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
        let bytes = fs::read(&full_path)?;
        Ok(*blake3::hash(&bytes).as_bytes())
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

pub fn gather_pkg_dep_pairs(
    workspace_root: &Path,
    package: &PackageNode,
    package_graph: Option<&PackageGraph>,
) -> Result<Vec<(String, String)>> {
    let lockfile_path = workspace_root.join("yarn.lock");
    let lockfile_contents = match fs::read_to_string(&lockfile_path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).into_diagnostic(),
    };
    if lockfile_contents.trim().is_empty() {
        return Ok(Vec::new());
    }
    let lockfile = parse_lockfile(&lockfile_contents).into_diagnostic()?;
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
            lockfile.as_ref(),
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
    dep_outputs: BTreeMap<String, [u8; 32]>,
    pkg_dep_pairs: &'a [(String, String)],
    resolver: &'a dyn FileStateResolver,
) -> CurrentState<'a> {
    CurrentState {
        task_spec_hash: task_spec_hash(task_def),
        env_hash: env_hash(&task_def.env, |name| std::env::var(name).ok()),
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
