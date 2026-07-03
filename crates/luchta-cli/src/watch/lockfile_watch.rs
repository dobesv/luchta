use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use luchta_types::PackageName;
use luchta_workspace::{PackageGraph, PackageNode};

use crate::cache_ctx::{gather_pkg_dep_pairs, load_lockfile_state, LockfileState};

pub(crate) struct LockfileWatchState {
    lockfile_path: PathBuf,
    baseline: HashMap<PackageName, Vec<(String, String)>>,
}

impl LockfileWatchState {
    pub(crate) fn new(workspace_root: &Path) -> Self {
        let canonical_root = workspace_root
            .canonicalize()
            .unwrap_or_else(|_| workspace_root.to_path_buf());
        Self {
            lockfile_path: canonical_root.join("yarn.lock"),
            baseline: HashMap::new(),
        }
    }

    pub(crate) fn rebuild_baseline(
        &mut self,
        packages: &[PackageNode],
        package_graph: Option<&PackageGraph>,
        workspace_root: &Path,
    ) {
        let lockfile_state = load_lockfile_state(workspace_root);
        match &lockfile_state {
            LockfileState::Parsed(_) => {
                self.baseline.clear();
                for package in packages {
                    if let Ok(dep_pairs) =
                        gather_pkg_dep_pairs(package, package_graph, &lockfile_state)
                    {
                        self.baseline.insert(package.name.clone(), dep_pairs);
                    }
                }
            }
            LockfileState::Failed(_) | LockfileState::Absent => {
                self.baseline.clear();
            }
        }
    }

    pub(crate) fn affected_packages(
        &self,
        packages: &[PackageNode],
        package_graph: Option<&PackageGraph>,
        workspace_root: &Path,
    ) -> HashSet<PackageName> {
        let lockfile_state = load_lockfile_state(workspace_root);
        match &lockfile_state {
            LockfileState::Parsed(_) => packages
                .iter()
                .filter_map(|package| {
                    let package_name = package.name.clone();
                    match gather_pkg_dep_pairs(package, package_graph, &lockfile_state) {
                        Ok(dep_pairs) if self.baseline.get(&package_name) == Some(&dep_pairs) => {
                            None
                        }
                        Ok(_) | Err(_) => Some(package_name),
                    }
                })
                .collect(),
            LockfileState::Failed(_) | LockfileState::Absent => packages
                .iter()
                .map(|package| package.name.clone())
                .collect(),
        }
    }

    pub(crate) fn lockfile_path(&self) -> &Path {
        &self.lockfile_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::session::discover_packages_for_paths;
    use luchta_workspace::{WorkspaceDiscovery, YarnWorkspace};
    use std::collections::{BTreeSet, HashSet};
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn parsed_lockfile_bump_affects_only_changed_package() {
        let workspace = TestWorkspace::new();
        let (packages, package_graph) = workspace.discover();
        let mut state = LockfileWatchState::new(workspace.root());
        state.rebuild_baseline(&packages, Some(&package_graph), workspace.root());

        workspace.write_lockfile(&workspace.lockfile_with_versions("1.1.0", "4.0.0", "3.0.0"));

        let affected = state.affected_packages(&packages, Some(&package_graph), workspace.root());

        assert_eq!(affected, package_set(["@scope/a"]));
    }

    #[test]
    fn new_canonicalizes_non_canonical_workspace_root_for_lockfile_path() {
        let workspace = TestWorkspace::new();
        let non_canonical_root = workspace.root().join("packages").join("..");
        let state = LockfileWatchState::new(&non_canonical_root);

        assert_eq!(
            state.lockfile_path(),
            workspace.root().canonicalize().unwrap().join("yarn.lock")
        );
    }

    #[test]
    fn unchanged_lockfile_produces_empty_affected_set() {
        let workspace = TestWorkspace::new();
        let (packages, package_graph) = workspace.discover();
        let mut state = LockfileWatchState::new(workspace.root());
        state.rebuild_baseline(&packages, Some(&package_graph), workspace.root());

        let affected = state.affected_packages(&packages, Some(&package_graph), workspace.root());

        assert!(
            affected.is_empty(),
            "expected no affected packages, got {affected:?}"
        );
    }

    #[test]
    fn failed_lockfile_parse_marks_all_packages_affected() {
        let workspace = TestWorkspace::new();
        let (packages, package_graph) = workspace.discover();
        let mut state = LockfileWatchState::new(workspace.root());
        state.rebuild_baseline(&packages, Some(&package_graph), workspace.root());

        workspace.write_lockfile(
            "not: [valid
yarn: lockfile",
        );

        let affected = state.affected_packages(&packages, Some(&package_graph), workspace.root());

        assert_eq!(affected, package_set(["@scope/a", "b"]));
    }

    #[test]
    fn absent_lockfile_marks_all_packages_affected() {
        let workspace = TestWorkspace::new();
        let (packages, package_graph) = workspace.discover();
        let mut state = LockfileWatchState::new(workspace.root());
        state.rebuild_baseline(&packages, Some(&package_graph), workspace.root());
        fs::remove_file(workspace.root().join("yarn.lock")).expect("remove lockfile");

        let affected = state.affected_packages(&packages, Some(&package_graph), workspace.root());

        assert_eq!(affected, package_set(["@scope/a", "b"]));
    }

    #[test]
    fn absent_to_parsed_transition_marks_packages_with_dependencies_affected() {
        let workspace = TestWorkspace::new_without_lockfile();
        let (packages, package_graph) = workspace.discover();
        let mut state = LockfileWatchState::new(workspace.root());
        state.rebuild_baseline(&packages, Some(&package_graph), workspace.root());
        workspace.write_default_lockfile();

        let affected = state.affected_packages(&packages, Some(&package_graph), workspace.root());

        assert_eq!(affected, package_set(["@scope/a", "b"]));
    }

    /// A DEEP transitive resolved-version change IS detected because
    /// `gather_pkg_dep_pairs` now tracks the FULL transitive closure.
    /// When `repeat-string`'s resolved version changes (3.0.0 -> 3.0.1),
    /// package `@scope/a` (which declares `left-pad` as a dependency) is marked
    /// affected. This conservative invalidation ensures cache consistency — any
    /// transitive resolved-version change invalidates the dependent package.
    #[test]
    fn deep_transitive_resolved_version_change_busts_cache() {
        let workspace = TestWorkspace::new();
        let (packages, package_graph) = workspace.discover();
        let mut state = LockfileWatchState::new(workspace.root());
        state.rebuild_baseline(&packages, Some(&package_graph), workspace.root());

        workspace.write_lockfile(&workspace.lockfile_with_versions("1.0.0", "4.0.0", "3.0.1"));

        let affected = state.affected_packages(&packages, Some(&package_graph), workspace.root());

        // @scope/a declares left-pad; left-pad depends on repeat-string transitively.
        // When repeat-string's resolved version changes, @scope/a must be marked affected.
        assert_eq!(affected, package_set(["@scope/a"]));
    }

    struct TestWorkspace {
        temp_dir: TempDir,
    }

    impl TestWorkspace {
        fn new() -> Self {
            let workspace = Self::new_without_lockfile();
            workspace.write_default_lockfile();
            workspace
        }

        fn new_without_lockfile() -> Self {
            let temp_dir = tempfile::tempdir().expect("create temp dir");
            let root = temp_dir.path();
            fs::create_dir_all(root.join("packages/a")).expect("create package a dir");
            fs::create_dir_all(root.join("packages/b")).expect("create package b dir");
            fs::write(
                root.join("package.json"),
                r#"{"name":"root","private":true,"workspaces":["packages/*"]}"#,
            )
            .expect("write root package json");
            fs::write(
                root.join("packages/a/package.json"),
                r#"{"name":"@scope/a","version":"1.0.0","dependencies":{"left-pad":"^1.0.0"}}"#,
            )
            .expect("write package a json");
            fs::write(
                root.join("packages/b/package.json"),
                r#"{"name":"b","version":"1.0.0","dependencies":{"lodash":"^4.17.0"}}"#,
            )
            .expect("write package b json");
            Self { temp_dir }
        }

        fn root(&self) -> &Path {
            self.temp_dir.path()
        }

        fn discover(&self) -> (Vec<PackageNode>, PackageGraph) {
            let workspace_root = self.root().canonicalize().expect("canonicalize workspace");
            let discovered_paths = self.discover_package_paths(&workspace_root);
            let packages = discover_packages_for_paths(&workspace_root, &discovered_paths)
                .expect("discover packages");
            let package_graph = PackageGraph::build(packages.clone()).expect("build package graph");
            (packages, package_graph)
        }

        fn discover_package_paths(&self, workspace_root: &Path) -> BTreeSet<PathBuf> {
            YarnWorkspace::new(workspace_root)
                .discover()
                .expect("discover workspace")
                .into_iter()
                .filter(|package| package.path != workspace_root)
                .map(|package| package.path.canonicalize().expect("canonical package path"))
                .collect()
        }

        fn write_default_lockfile(&self) {
            self.write_lockfile(&self.lockfile_with_versions("1.0.0", "4.0.0", "3.0.0"));
        }

        fn write_lockfile(&self, contents: &str) {
            fs::write(self.root().join("yarn.lock"), contents).expect("write yarn.lock");
        }

        fn lockfile_with_versions(
            &self,
            left_pad_version: &str,
            lodash_version: &str,
            repeat_version: &str,
        ) -> String {
            [
                "# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.".to_string(),
                "# yarn lockfile v1".to_string(),
                String::new(),
                "left-pad@^1.0.0:".to_string(),
                format!("  version \"{left_pad_version}\""),
                format!(
                    "  resolved \"https://registry.yarnpkg.com/left-pad/-/left-pad-{left_pad_version}.tgz#leftpad\""
                ),
                "  integrity sha512-leftpad".to_string(),
                "  dependencies:".to_string(),
                "    repeat-string \"^3.0.0\"".to_string(),
                "    transitive-helper \"^1.0.0\"".to_string(),
                String::new(),
                "repeat-string@^3.0.0:".to_string(),
                format!("  version \"{repeat_version}\""),
                format!(
                    "  resolved \"https://registry.yarnpkg.com/repeat-string/-/repeat-string-{repeat_version}.tgz#repeat\""
                ),
                "  integrity sha512-repeat".to_string(),
                String::new(),
                "transitive-helper@^1.0.0:".to_string(),
                "  version \"1.0.0\"".to_string(),
                "  resolved \"https://registry.yarnpkg.com/transitive-helper/-/transitive-helper-1.0.0.tgz#helper\"".to_string(),
                "  integrity sha512-helper".to_string(),
                "  dependencies:".to_string(),
                "    repeat-string \"^3.0.0\"".to_string(),
                String::new(),
                "lodash@^4.0.0, lodash@^4.17.0:".to_string(),
                format!("  version \"{lodash_version}\""),
                format!(
                    "  resolved \"https://registry.yarnpkg.com/lodash/-/lodash-{lodash_version}.tgz#lodash\""
                ),
                "  integrity sha512-lodash".to_string(),
                String::new(),
            ]
            .join("\n")
        }
    }

    fn package_set<const N: usize>(names: [&str; N]) -> HashSet<PackageName> {
        names.into_iter().map(PackageName::from).collect()
    }
}
