use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use globset::{Glob, GlobSetBuilder};
use luchta_cache::{
    env_hash, pkg_dep_hash, resolve_inputs_with_semantics_and_options,
    resolve_outputs_with_options, task_spec_hash, CurrentState, FileEntry, FileStateResolver,
    ListingCache, ResolveOptions,
};
use luchta_engine::{expand_input_patterns, InputExpansionError};
use luchta_lockfiles::{parse_lockfile, Lockfile};
use luchta_types::{EnvSpec, InputPattern, PackageName, TaskDefinition};
use luchta_workspace::{PackageGraph, PackageNode};
use miette::{IntoDiagnostic, Result};
use serde::Deserialize;

#[derive(Clone, Debug)]
pub(crate) enum LockfileState {
    Absent,
    Parsed(Arc<dyn Lockfile>),
    Failed(String),
}

/// Attempts to load workspace lockfile once and classify outcome:
/// missing/empty -> `Absent`, parses -> `Parsed`, parse error or non-NotFound
/// I/O error -> `Failed` (which `gather_pkg_dep_pairs` surfaces as `Err`, so the
/// caller disables caching for that task).
pub(crate) fn load_lockfile_state(workspace_root: &Path) -> LockfileState {
    match fs::read_to_string(workspace_root.join("yarn.lock")) {
        Ok(text) => {
            if text.trim().is_empty() {
                LockfileState::Absent
            } else {
                match parse_lockfile(&text) {
                    Ok(lockfile) => LockfileState::Parsed(lockfile.into()),
                    Err(error) => LockfileState::Failed(error.to_string()),
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => LockfileState::Absent,
        Err(error) => LockfileState::Failed(error.to_string()),
    }
}

pub(crate) struct PackageDirResolver {
    package_dir: PathBuf,
    pub(crate) repo_root: PathBuf,
    source_pkg: PackageName,
    pub(crate) package_graph: Arc<PackageGraph>,
    listing_cache: Arc<ListingCache>,
}

impl PackageDirResolver {
    pub(crate) fn new(
        package_dir: PathBuf,
        repo_root: PathBuf,
        source_pkg: PackageName,
        package_graph: impl Into<Arc<PackageGraph>>,
        listing_cache: Arc<ListingCache>,
    ) -> Self {
        Self {
            package_dir,
            repo_root,
            source_pkg,
            package_graph: package_graph.into(),
            listing_cache,
        }
    }
}

fn format_expansion_error(error: &InputExpansionError) -> String {
    // The wrapped `error`'s Display already names the offending pattern and a
    // user-safe package label (root renders as "the workspace root", never the
    // internal `//root` sentinel), so we only add the `input` prefix here. This
    // read/skip path has no task context to include.
    format!("input \"{}\": {}", error.pattern(), error)
}

impl FileStateResolver for PackageDirResolver {
    fn resolve_inputs(
        &self,
        patterns: &[String],
        prior_entries: &[FileEntry],
    ) -> luchta_cache::Result<Vec<FileEntry>> {
        let requests = expand_input_patterns(
            patterns,
            &self.source_pkg,
            &self.package_graph,
            &self.repo_root,
        )
        .map_err(|e| luchta_cache::CacheError::InputExpansion(format_expansion_error(&e)))?;
        resolve_inputs_with_semantics_and_options(
            &requests,
            ResolveOptions {
                prior_entries,
                listing_cache: Some(self.listing_cache.as_ref()),
            },
        )
    }

    fn resolve_outputs(
        &self,
        patterns: &[String],
        prior_entries: &[FileEntry],
    ) -> luchta_cache::Result<Vec<FileEntry>> {
        resolve_outputs_with_options(
            &self.package_dir,
            patterns,
            ResolveOptions {
                prior_entries,
                listing_cache: Some(self.listing_cache.as_ref()),
            },
        )
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SelectedDependencyRoot {
    name: String,
    version: String,
    origin_package_path: PathBuf,
}

pub(crate) fn gather_pkg_dep_pairs(
    package: &PackageNode,
    package_graph: Option<&PackageGraph>,
    lockfile: &LockfileState,
) -> Result<Vec<(String, String)>> {
    gather_pkg_dep_pairs_filtered(
        package,
        package_graph,
        &package.path,
        lockfile,
        &["**/*".to_string()],
    )
}

pub(crate) fn gather_pkg_dep_pairs_filtered(
    package: &PackageNode,
    package_graph: Option<&PackageGraph>,
    repo_root: &Path,
    lockfile: &LockfileState,
    dependency_patterns: &[String],
) -> Result<Vec<(String, String)>> {
    let lockfile = match lockfile {
        LockfileState::Absent => return Ok(Vec::new()),
        LockfileState::Failed(msg) => return Err(miette::miette!("{msg}")),
        LockfileState::Parsed(lf) => lf.as_ref(),
    };
    let package_json = read_package_json_external_deps(&package.path)?;
    let external_dependencies = collect_external_dependencies(package_json);

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
    let selected_roots = select_dependency_roots(
        package,
        package_graph,
        repo_root,
        dependency_patterns,
        &external_dependencies,
    )?;
    let mut pairs = BTreeSet::new();

    for selected_root in selected_roots {
        collect_dep_pairs_for_package(
            lockfile,
            &selected_root.origin_package_path,
            &workspace_names,
            &workspace_paths,
            &selected_root.name,
            &selected_root.version,
            &mut pairs,
        )?;
    }

    Ok(pairs.into_iter().collect())
}

fn read_package_json_external_deps(package_path: &Path) -> Result<PackageJsonExternalDeps> {
    let package_json = fs::read_to_string(package_path.join("package.json")).into_diagnostic()?;
    serde_json::from_str(&package_json).into_diagnostic()
}

fn collect_external_dependencies(package_json: PackageJsonExternalDeps) -> Vec<(String, String)> {
    package_json
        .dependencies
        .into_iter()
        .chain(package_json.dev_dependencies)
        .chain(package_json.optional_dependencies)
        .collect()
}

fn select_dependency_roots(
    package: &PackageNode,
    package_graph: Option<&PackageGraph>,
    repo_root: &Path,
    dependency_patterns: &[String],
    external_dependencies: &[(String, String)],
) -> Result<BTreeSet<SelectedDependencyRoot>> {
    let source_candidates = dependency_roots_from_pairs(external_dependencies, &package.path);
    let mut selected = BTreeSet::new();

    for raw_pattern in dependency_patterns {
        let pattern = raw_pattern.parse::<InputPattern>().into_diagnostic()?;
        match pattern {
            InputPattern::SamePackage(glob) => {
                insert_matching_dependency_roots(&mut selected, &source_candidates, &glob)?;
            }
            InputPattern::DirectUpstream(glob) => {
                let graph = package_graph.ok_or_else(|| {
                    miette::miette!("dependency pattern '^{}' requires package graph", glob)
                })?;
                let mut candidates = BTreeSet::new();
                for upstream in graph.dependencies_of(&package.name).into_diagnostic()? {
                    candidates.extend(read_external_dependency_roots(&upstream.path)?);
                }
                insert_matching_dependency_roots(&mut selected, &candidates, &glob)?;
            }
            InputPattern::TransitiveUpstream(glob) => {
                let graph = package_graph.ok_or_else(|| {
                    miette::miette!("dependency pattern '^^{}' requires package graph", glob)
                })?;
                let mut candidates = BTreeSet::new();
                for upstream_name in graph
                    .transitive_dependencies_of(&package.name)
                    .into_diagnostic()?
                    .into_iter()
                    .filter(|upstream_name| upstream_name != &package.name)
                {
                    let upstream = graph.node(&upstream_name).into_diagnostic()?;
                    candidates.extend(read_external_dependency_roots(&upstream.path)?);
                }
                insert_matching_dependency_roots(&mut selected, &candidates, &glob)?;
            }
            InputPattern::Specific(package_name, glob) => {
                let target_path =
                    specific_package_path(package_graph, repo_root, &package_name, raw_pattern)?;
                let candidates = read_external_dependency_roots(&target_path)?;
                insert_matching_dependency_roots(&mut selected, &candidates, &glob)?;
            }
            InputPattern::Root(glob) => {
                let candidates = read_external_dependency_roots(repo_root)?;
                insert_matching_dependency_roots(&mut selected, &candidates, &glob)?;
            }
        }
    }

    Ok(selected)
}

fn insert_matching_dependency_roots(
    selected: &mut BTreeSet<SelectedDependencyRoot>,
    candidates: &BTreeSet<SelectedDependencyRoot>,
    glob: &str,
) -> Result<()> {
    let mut builder = GlobSetBuilder::new();
    builder.add(Glob::new(glob).into_diagnostic()?);
    let globset = builder.build().into_diagnostic()?;
    selected.extend(
        candidates
            .iter()
            .filter(|candidate| globset.is_match(candidate.name.as_str()))
            .cloned(),
    );
    Ok(())
}

fn read_external_dependency_roots(package_path: &Path) -> Result<BTreeSet<SelectedDependencyRoot>> {
    Ok(dependency_roots_from_pairs(
        &collect_external_dependencies(read_package_json_external_deps(package_path)?),
        package_path,
    ))
}

fn dependency_roots_from_pairs(
    dependencies: &[(String, String)],
    package_path: &Path,
) -> BTreeSet<SelectedDependencyRoot> {
    dependencies
        .iter()
        .map(|(name, version)| SelectedDependencyRoot {
            name: name.clone(),
            version: version.clone(),
            origin_package_path: package_path.to_path_buf(),
        })
        .collect()
}

fn specific_package_path(
    package_graph: Option<&PackageGraph>,
    repo_root: &Path,
    package_name: &PackageName,
    pattern: &str,
) -> Result<std::path::PathBuf> {
    if package_name.is_root() {
        return Ok(repo_root.to_path_buf());
    }

    let graph = package_graph.ok_or_else(|| {
        miette::miette!(
            "unknown package '{}' in dependency pattern '{}'",
            package_name,
            pattern
        )
    })?;
    graph
        .node(package_name)
        .map(|node| node.path.clone())
        .map_err(|_| {
            miette::miette!(
                "unknown package '{}' in dependency pattern '{}'",
                package_name,
                pattern
            )
        })
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
    pairs.extend(
        lockfile
            .transitive_dependencies(&resolved.key)
            .into_diagnostic()?,
    );

    Ok(())
}

pub fn build_current_state<'a>(
    task_def: &'a TaskDefinition,
    merged_env: &'a BTreeMap<String, EnvSpec>,
    dep_outputs: BTreeMap<String, [u8; 32]>,
    pkg_dep_pairs: &'a [(String, String)],
    resolver: &'a dyn FileStateResolver,
    nonce: Option<&'a str>,
) -> CurrentState<'a> {
    CurrentState {
        task_spec_hash: task_spec_hash(task_def, nonce),
        // Hash declared merged EnvSpec only. Built-in passthrough whitelist vars are
        // injected later into ExecutionRequest.env, so whitelist-only ambient changes
        // never enter env_hash.
        env_hash: env_hash(merged_env, |name| std::env::var(name).ok()),
        pkg_dep_hash: pkg_dep_hash(pkg_dep_pairs),
        dep_outputs,
        cache_nonce: nonce,
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
        match load_lockfile_state(dir.path()) {
            LockfileState::Absent => {}
            other => panic!("expected Absent, got {other:?}"),
        }
    }

    #[test]
    fn load_lockfile_state_empty_file_is_absent() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("yarn.lock"), "   \n").unwrap();
        match load_lockfile_state(dir.path()) {
            LockfileState::Absent => {}
            other => panic!("expected Absent, got {other:?}"),
        }
    }

    #[test]
    fn load_lockfile_state_unparseable_file_is_failed() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("yarn.lock"), "not-a-real-lockfile").unwrap();
        match load_lockfile_state(dir.path()) {
            LockfileState::Failed(msg) => assert!(!msg.is_empty()),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn gather_pkg_dep_pairs_absent_lockfile_yields_no_pairs() {
        let dir = TempDir::new().unwrap();
        let package = make_package(dir.path());
        write_package_json(dir.path(), "{}\n");
        let pairs = gather_pkg_dep_pairs(&package, None, &LockfileState::Absent)
            .expect("absent lockfile should not error");
        assert!(pairs.is_empty());
    }

    #[test]
    fn gather_pkg_dep_pairs_failed_lockfile_errors() {
        let dir = TempDir::new().unwrap();
        let package = make_package(dir.path());
        write_package_json(dir.path(), "{}\n");
        let err = gather_pkg_dep_pairs(&package, None, &LockfileState::Failed("boom".into()))
            .expect_err("failed lockfile should error");
        assert!(format!("{err}").contains("boom"));
    }

    #[test]
    fn select_dependency_roots_preserves_upstream_origin_and_version() {
        let source_dir = TempDir::new().unwrap();
        write_package_json(
            source_dir.path(),
            "{\"name\":\"@repo/app\",\"dependencies\":{\"@repo/lib\":\"workspace:*\",\"left-pad\":\"^1.0.0\"}}\n",
        );
        let source = PackageNode::new(
            PackageName::from("@repo/app"),
            source_dir.path().to_path_buf(),
        );

        let upstream_dir = TempDir::new().unwrap();
        write_package_json(
            upstream_dir.path(),
            "{\"name\":\"@repo/lib\",\"dependencies\":{\"chalk\":\"^5.0.0\",\"kleur\":\"^4.0.0\"}}\n",
        );
        let upstream = PackageNode::new(
            PackageName::from("@repo/lib"),
            upstream_dir.path().to_path_buf(),
        );

        let graph = PackageGraph::build(vec![upstream.clone(), source.clone()])
            .expect("graph")
            .with_root_package(PackageName::from("//root"));

        let selected = select_dependency_roots(
            &source,
            Some(&graph),
            source_dir.path(),
            &["^chalk".to_string()],
            &[("left-pad".to_string(), "^1.0.0".to_string())],
        )
        .expect("select roots");

        assert_eq!(selected.len(), 1);
        let root = selected.iter().next().expect("root");
        assert_eq!(root.name, "chalk");
        assert_eq!(root.version, "^5.0.0");
        assert_eq!(root.origin_package_path, upstream.path);
    }

    #[test]
    fn select_dependency_roots_default_parity_matches_source_dependencies() {
        let dir = TempDir::new().unwrap();
        let package = make_package(dir.path());
        let external_dependencies = vec![
            ("left-pad".to_string(), "^1.0.0".to_string()),
            ("chalk".to_string(), "^5.0.0".to_string()),
        ];

        let selected = select_dependency_roots(
            &package,
            None,
            dir.path(),
            &["**/*".to_string()],
            &external_dependencies,
        )
        .expect("select roots");

        let expected = dependency_roots_from_pairs(&external_dependencies, dir.path());
        assert_eq!(selected, expected);
    }

    #[test]
    fn select_dependency_roots_transitive_upstream_excludes_source_package() {
        let transitive_dir = TempDir::new().unwrap();
        write_package_json(
            transitive_dir.path(),
            "{\"name\":\"@repo/core\",\"dependencies\":{\"lodash\":\"^4.17.0\"}}\n",
        );
        let transitive = PackageNode::new(
            PackageName::from("@repo/core"),
            transitive_dir.path().to_path_buf(),
        );

        let upstream_dir = TempDir::new().unwrap();
        write_package_json(
            upstream_dir.path(),
            "{\"name\":\"@repo/lib\",\"dependencies\":{\"@repo/core\":\"workspace:*\",\"chalk\":\"^5.0.0\"}}\n",
        );
        let upstream = PackageNode::new(
            PackageName::from("@repo/lib"),
            upstream_dir.path().to_path_buf(),
        );

        let source_dir = TempDir::new().unwrap();
        write_package_json(
            source_dir.path(),
            "{\"name\":\"@repo/app\",\"dependencies\":{\"@repo/lib\":\"workspace:*\",\"left-pad\":\"^1.0.0\"}}\n",
        );
        let source = PackageNode::new(
            PackageName::from("@repo/app"),
            source_dir.path().to_path_buf(),
        );

        let graph = PackageGraph::build(vec![transitive.clone(), upstream.clone(), source.clone()])
            .expect("graph")
            .with_root_package(PackageName::from("//root"));

        let selected = select_dependency_roots(
            &source,
            Some(&graph),
            source_dir.path(),
            &["^^*".to_string()],
            &[("left-pad".to_string(), "^1.0.0".to_string())],
        )
        .expect("select roots");

        let names = selected
            .into_iter()
            .map(|root| root.name)
            .collect::<BTreeSet<_>>();
        assert!(names.contains("chalk"));
        assert!(names.contains("lodash"));
        assert!(!names.contains("left-pad"));
    }

    #[test]
    fn should_skip_dependency_spec_returns_true_for_workspace_prefix() {
        let names = BTreeSet::new();
        let paths = BTreeSet::new();
        assert!(should_skip_dependency_spec(
            "foo",
            "workspace:*",
            &names,
            &paths,
        ));
    }

    #[test]
    fn should_skip_dependency_spec_returns_true_for_workspace_name_match() {
        let names = BTreeSet::from(["foo".to_owned()]);
        let paths = BTreeSet::new();
        assert!(should_skip_dependency_spec("foo", "1.0.0", &names, &paths));
    }

    fn make_package(path: &Path) -> PackageNode {
        PackageNode::new(PackageName::from("pkg"), path.to_path_buf())
    }

    fn write_package_json(dir: &Path, contents: &str) {
        fs::write(dir.join("package.json"), contents).unwrap();
    }

    // =============================================================================
    // Phase 2: dependencies filter tests
    // =============================================================================

    /// Test 1: default_dependencies_includes_all — a task WITHOUT an explicit
    /// `dependencies` filter (default `["**/*"]`) produces the SAME dep-pair set /
    /// same pkg_dep_hash as the full Phase-1 closure.
    #[test]
    fn default_dependencies_includes_all_yarn1() {
        let dir = TempDir::new().unwrap();
        let package = make_package(dir.path());
        // Source package depends on left-pad and chalk
        write_package_json(
            dir.path(),
            r#"{"name":"app","dependencies":{"left-pad":"^1.0.0","chalk":"^5.0.0"}}"#,
        );
        // Yarn v1 lockfile with resolved versions
        let lockfile_content = r#"# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
# yarn lockfile v1

left-pad@^1.0.0:
  version "1.0.0"
  resolved "https://registry.yarnpkg.com/left-pad/-/left-pad-1.0.0.tgz#c84e2417581bbb8eaf2b9e3d7a122e572ab1af37"
  integrity sha512-XI5MPzVNApjAyhQzZWn7Q4oJ4kBzP4Qzy6wKzH8w0v7lidh+NROm4tW7x1YgnPZXoqBYwygJyI072QtdgQXl3g==

chalk@^5.0.0:
  version "5.0.0"
  resolved "https://registry.yarnpkg.com/chalk/-/chalk-5.0.0.tgz#ae417bf7adye0"
  integrity sha512-chalk0
"#;
        fs::write(dir.path().join("yarn.lock"), lockfile_content).unwrap();

        let lockfile_state = load_lockfile_state(dir.path());
        let pairs_unfiltered = gather_pkg_dep_pairs(&package, None, &lockfile_state)
            .expect("gather unfiltered should succeed");
        let pairs_default_filtered = gather_pkg_dep_pairs_filtered(
            &package,
            None,
            dir.path(),
            &lockfile_state,
            &["**/*".to_string()],
        )
        .expect("gather filtered default should succeed");

        // Default filter produces same pairs as unfiltered
        assert_eq!(
            pairs_unfiltered, pairs_default_filtered,
            "default **/* filter should match unfiltered closure"
        );
    }

    /// Test 1 (Berry variant): default filter produces same pairs.
    #[test]
    fn default_dependencies_includes_all_yarn_berry() {
        let dir = TempDir::new().unwrap();
        let package = make_package(dir.path());
        write_package_json(
            dir.path(),
            r#"{"name":"app","dependencies":{"left-pad":"^1.0.0","chalk":"^5.0.0"}}"#,
        );
        let lockfile_content = r#"__metadata:
  version: 8
  cacheKey: 10

"left-pad@npm:^1.0.0":
  version: 1.0.0
  resolution: "left-pad@npm:1.0.0"
  checksum: 10/c84e2417581bbb8eaf2b9e3d7a122e572ab1af37
  languageName: node
  linkType: hard

"chalk@npm:^5.0.0":
  version: 5.0.0
  resolution: "chalk@npm:5.0.0"
  checksum: 10/abc123def456
  languageName: node
  linkType: hard

"app@workspace:.":
  version: 0.0.0-use.local
  resolution: "app@workspace:."
  dependencies:
    left-pad: "npm:^1.0.0"
    chalk: "npm:^5.0.0"
  languageName: node
  linkType: soft
"#;
        fs::write(dir.path().join("yarn.lock"), lockfile_content).unwrap();

        let lockfile_state = load_lockfile_state(dir.path());
        let pairs_unfiltered = gather_pkg_dep_pairs(&package, None, &lockfile_state)
            .expect("gather unfiltered should succeed");
        let pairs_default_filtered = gather_pkg_dep_pairs_filtered(
            &package,
            None,
            dir.path(),
            &lockfile_state,
            &["**/*".to_string()],
        )
        .expect("gather filtered default should succeed");

        assert_eq!(
            pairs_unfiltered, pairs_default_filtered,
            "default **/* filter should match unfiltered closure (Berry)"
        );
    }

    /// Test 2: filter_narrows_roots — with a filter selecting only one dep,
    /// changing a NON-matched external dep does NOT change the resulting pairs/hash,
    /// while changing the matched dep DOES.
    #[test]
    fn filter_narrows_roots_yarn1() {
        let dir = TempDir::new().unwrap();
        let package = make_package(dir.path());
        // Source depends on left-pad and chalk; filter selects only left-pad
        write_package_json(
            dir.path(),
            r#"{"name":"app","dependencies":{"left-pad":"^1.0.0","chalk":"^5.0.0"}}"#,
        );
        // Base lockfile with left-pad@1.0.0 and chalk@5.0.0
        let lockfile_v1 = r#"# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
# yarn lockfile v1

left-pad@^1.0.0:
  version "1.0.0"
  resolved "https://registry.yarnpkg.com/left-pad/-/left-pad-1.0.0.tgz#c84e2417581bbb8eaf2b9e3d7a122e572ab1af37"
  integrity sha512-leftpad1

chalk@^5.0.0:
  version "5.0.0"
  resolved "https://registry.yarnpkg.com/chalk/-/chalk-5.0.0.tgz#ae417bf7adye0"
  integrity sha512-chalk5
"#;
        // Lockfile where left-pad version changed to 1.1.0 (matched by filter)
        let lockfile_leftpad_bump = r#"# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
# yarn lockfile v1

left-pad@^1.0.0:
  version "1.1.0"
  resolved "https://registry.yarnpkg.com/left-pad/-/left-pad-1.1.0.tgz#different"
  integrity sha512-leftpad11

chalk@^5.0.0:
  version "5.0.0"
  resolved "https://registry.yarnpkg.com/chalk/-/chalk-5.0.0.tgz#ae417bf7adye0"
  integrity sha512-chalk5
"#;
        // Lockfile where chalk version changed to 5.1.0 (NOT matched by filter)
        let lockfile_chalk_bump = r#"# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
# yarn lockfile v1

left-pad@^1.0.0:
  version "1.0.0"
  resolved "https://registry.yarnpkg.com/left-pad/-/left-pad-1.0.0.tgz#c84e2417581bbb8eaf2b9e3d7a122e572ab1af37"
  integrity sha512-leftpad1

chalk@^5.0.0:
  version "5.1.0"
  resolved "https://registry.yarnpkg.com/chalk/-/chalk-5.1.0.tgz#different"
  integrity sha512-chalk51
"#;

        fs::write(dir.path().join("yarn.lock"), lockfile_v1).unwrap();
        let lockfile_state = load_lockfile_state(dir.path());
        let pairs_base = gather_pkg_dep_pairs_filtered(
            &package,
            None,
            dir.path(),
            &lockfile_state,
            &["left-pad".to_string()],
        )
        .expect("gather filtered should succeed");

        // left-pad bump changes pairs (matched)
        fs::write(dir.path().join("yarn.lock"), lockfile_leftpad_bump).unwrap();
        let lockfile_state_matched = load_lockfile_state(dir.path());
        let pairs_leftpad_bump = gather_pkg_dep_pairs_filtered(
            &package,
            None,
            dir.path(),
            &lockfile_state_matched,
            &["left-pad".to_string()],
        )
        .expect("gather filtered should succeed");
        assert_ne!(
            pairs_base, pairs_leftpad_bump,
            "matched dep version change should affect pairs"
        );

        // chalk bump does NOT change pairs (not matched)
        fs::write(dir.path().join("yarn.lock"), lockfile_chalk_bump).unwrap();
        let lockfile_state_unmatched = load_lockfile_state(dir.path());
        let pairs_chalk_bump = gather_pkg_dep_pairs_filtered(
            &package,
            None,
            dir.path(),
            &lockfile_state_unmatched,
            &["left-pad".to_string()],
        )
        .expect("gather filtered should succeed");
        assert_eq!(
            pairs_base, pairs_chalk_bump,
            "non-matched dep version change should not affect filtered pairs"
        );
    }

    /// Test 3: filter_root_pulls_full_closure — filter matches a root;
    /// a TRANSITIVE dep of that root changing version DOES change the pairs/hash.
    /// (Interpretation A: matched root pulls its whole closure)
    #[test]
    fn filter_root_pulls_full_closure_yarn1() {
        let dir = TempDir::new().unwrap();
        let package = make_package(dir.path());
        // Source depends on left-pad; left-pad depends on repeat-string (transitive)
        write_package_json(
            dir.path(),
            r#"{"name":"app","dependencies":{"left-pad":"^1.0.0"}}"#,
        );
        // lockfile with left-pad@1.0.0 → repeat-string@3.0.0
        let lockfile_v300 = r#"# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
# yarn lockfile v1

left-pad@^1.0.0:
  version "1.0.0"
  resolved "https://registry.yarnpkg.com/left-pad/-/left-pad-1.0.0.tgz#c84e2417581bbb8eaf2b9e3d7a122e572ab1af37"
  integrity sha512-XI5MPzVNApjAyhQzZWn7Q4oJ4kBzP4Qzy6wKzH8w0v7lidh+NROm4tW7x1YgnPZXoqBYwygJyI072QtdgQXl3g==
  dependencies:
    repeat-string "^3.0.0"

repeat-string@^3.0.0:
  version "3.0.0"
  resolved "https://registry.yarnpkg.com/repeat-string/-/repeat-string-3.0.0.tgz#abc123def456"
  integrity sha512-repeat0
"#;
        // lockfile with left-pad@1.0.0 → repeat-string@3.0.1
        let lockfile_v301 = r#"# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
# yarn lockfile v1

left-pad@^1.0.0:
  version "1.0.0"
  resolved "https://registry.yarnpkg.com/left-pad/-/left-pad-1.0.0.tgz#c84e2417581bbb8eaf2b9e3d7a122e572ab1af37"
  integrity sha512-XI5MPzVNApjAyhQzZWn7Q4oJ4kBzP4Qzy6wKzH8w0v7lidh+NROm4tW7x1YgnPZXoqBYwygJyI072QtdgQXl3g==
  dependencies:
    repeat-string "^3.0.0"

repeat-string@^3.0.0:
  version "3.0.1"
  resolved "https://registry.yarnpkg.com/repeat-string/-/repeat-string-3.0.1.tgz#abc123def789"
  integrity sha512-repeat1
"#;

        fs::write(dir.path().join("yarn.lock"), lockfile_v300).unwrap();
        let lockfile_state = load_lockfile_state(dir.path());
        let pairs_v300 = gather_pkg_dep_pairs_filtered(
            &package,
            None,
            dir.path(),
            &lockfile_state,
            &["left-pad".to_string()],
        )
        .expect("gather filtered should succeed");

        // Transitive dep change affects pairs (closure semantics)
        fs::write(dir.path().join("yarn.lock"), lockfile_v301).unwrap();
        let lockfile_state_v301 = load_lockfile_state(dir.path());
        let pairs_v301 = gather_pkg_dep_pairs_filtered(
            &package,
            None,
            dir.path(),
            &lockfile_state_v301,
            &["left-pad".to_string()],
        )
        .expect("gather filtered should succeed");

        assert_ne!(
            pairs_v300, pairs_v301,
            "transitive dep version change should affect pairs (closure semantics)"
        );
        // Both should contain left-pad and repeat-string (closure pulled)
        assert!(
            pairs_v300.iter().any(|(n, _)| n == "repeat-string"),
            "pairs should include transitive dep repeat-string"
        );
    }

    /// Test 3 Berry variant: transitive closure pulled for matched root.
    #[test]
    fn filter_root_pulls_full_closure_yarn_berry() {
        let dir = TempDir::new().unwrap();
        let package = make_package(dir.path());
        write_package_json(
            dir.path(),
            r#"{"name":"app","dependencies":{"left-pad":"^1.0.0"}}"#,
        );
        let lockfile_v300 = r#"__metadata:
  version: 8
  cacheKey: 10

"left-pad@npm:^1.0.0":
  version: 1.0.0
  resolution: "left-pad@npm:1.0.0"
  checksum: 10/c84e2417581bbb8eaf2b9e3d7a122e572ab1af37
  languageName: node
  linkType: hard
  dependencies:
    repeat-string: "npm:^3.0.0"

"repeat-string@npm:^3.0.0":
  version: 3.0.0
  resolution: "repeat-string@npm:3.0.0"
  checksum: 10/abc123def456
  languageName: node
  linkType: hard

"app@workspace:.":
  version: 0.0.0-use.local
  resolution: "app@workspace:."
  dependencies:
    left-pad: "npm:^1.0.0"
  languageName: node
  linkType: soft
"#;
        let lockfile_v301 = r#"__metadata:
  version: 8
  cacheKey: 10

"left-pad@npm:^1.0.0":
  version: 1.0.0
  resolution: "left-pad@npm:1.0.0"
  checksum: 10/c84e2417581bbb8eaf2b9e3d7a122e572ab1af37
  languageName: node
  linkType: hard
  dependencies:
    repeat-string: "npm:^3.0.0"

"repeat-string@npm:^3.0.0":
  version: 3.0.1
  resolution: "repeat-string@npm:3.0.1"
  checksum: 10/abc123def789
  languageName: node
  linkType: hard

"app@workspace:.":
  version: 0.0.0-use.local
  resolution: "app@workspace:."
  dependencies:
    left-pad: "npm:^1.0.0"
  languageName: node
  linkType: soft
"#;

        fs::write(dir.path().join("yarn.lock"), lockfile_v300).unwrap();
        let lockfile_state = load_lockfile_state(dir.path());
        let pairs_v300 = gather_pkg_dep_pairs_filtered(
            &package,
            None,
            dir.path(),
            &lockfile_state,
            &["left-pad".to_string()],
        )
        .expect("gather filtered should succeed");

        fs::write(dir.path().join("yarn.lock"), lockfile_v301).unwrap();
        let lockfile_state_v301 = load_lockfile_state(dir.path());
        let pairs_v301 = gather_pkg_dep_pairs_filtered(
            &package,
            None,
            dir.path(),
            &lockfile_state_v301,
            &["left-pad".to_string()],
        )
        .expect("gather filtered should succeed");

        assert_ne!(
            pairs_v300, pairs_v301,
            "transitive dep version change should affect pairs (Berry)"
        );
        assert!(
            pairs_v300.iter().any(|(n, _)| n == "repeat-string"),
            "pairs should include transitive dep repeat-string (Berry)"
        );
    }

    /// Test 4: upstream_prefix_inherits_dep_specs — `^glob` selects a dep declared by
    /// a DIRECT upstream workspace package (NOT by the source package) and it
    /// contributes to the pairs. Also covers `^^` from a transitive upstream.
    #[test]
    fn upstream_prefix_inherits_dep_specs_direct() {
        let source_dir = TempDir::new().unwrap();
        write_package_json(
            source_dir.path(),
            r#"{"name":"@repo/app","dependencies":{"@repo/lib":"workspace:*","left-pad":"^1.0.0"}}"#,
        );
        let source = PackageNode::new(
            PackageName::from("@repo/app"),
            source_dir.path().to_path_buf(),
        );

        let upstream_dir = TempDir::new().unwrap();
        write_package_json(
            upstream_dir.path(),
            r#"{"name":"@repo/lib","dependencies":{"chalk":"^5.0.0"}}"#,
        );
        let upstream = PackageNode::new(
            PackageName::from("@repo/lib"),
            upstream_dir.path().to_path_buf(),
        );

        let graph = PackageGraph::build(vec![upstream.clone(), source.clone()])
            .expect("graph")
            .with_root_package(PackageName::from("//root"));

        // Lockfile containing source's left-pad AND upstream's chalk
        let lockfile_content = r#"# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
# yarn lockfile v1

left-pad@^1.0.0:
  version "1.0.0"
  resolved "https://registry.yarnpkg.com/left-pad/-/left-pad-1.0.0.tgz#c84e2417581bbb8eaf2b9e3d7a122e572ab1af37"
  integrity sha512-leftpad1

chalk@^5.0.0:
  version "5.0.0"
  resolved "https://registry.yarnpkg.com/chalk/-/chalk-5.0.0.tgz#ae417bf7adye0"
  integrity sha512-chalk5
"#;
        fs::write(source_dir.path().join("yarn.lock"), lockfile_content).unwrap();

        let lockfile_state = load_lockfile_state(source_dir.path());

        // ^chalk selects chalk from upstream's package.json
        let pairs = gather_pkg_dep_pairs_filtered(
            &source,
            Some(&graph),
            source_dir.path(),
            &lockfile_state,
            &["^chalk".to_string()],
        )
        .expect("gather filtered should succeed");

        // Should include chalk from upstream's dependencies
        assert!(
            pairs.iter().any(|(n, v)| n == "chalk" && v == "5.0.0"),
            "^chalk should select upstream's chalk dependency"
        );
        // Should NOT include source's own left-pad
        assert!(
            !pairs.iter().any(|(n, _)| n == "left-pad"),
            "^chalk should not include source's own left-pad"
        );
    }

    /// Test 4 (transitive upstream variant): `^^` selects from transitive upstream.
    /// Uses Yarn Berry lockfile to exercise Berry parser.
    #[test]
    fn upstream_prefix_inherits_dep_specs_transitive_yarn_berry() {
        // Build chain: source → upstream → upstream2
        let source_dir = TempDir::new().unwrap();
        write_package_json(
            source_dir.path(),
            r#"{"name":"@repo/app","dependencies":{"@repo/lib":"workspace:*"}}"#,
        );
        let source = PackageNode::new(
            PackageName::from("@repo/app"),
            source_dir.path().to_path_buf(),
        );

        let upstream_dir = TempDir::new().unwrap();
        write_package_json(
            upstream_dir.path(),
            r#"{"name":"@repo/lib","dependencies":{"@repo/core":"workspace:*","chalk":"^5.0.0"}}"#,
        );
        let upstream = PackageNode::new(
            PackageName::from("@repo/lib"),
            upstream_dir.path().to_path_buf(),
        );

        let upstream2_dir = TempDir::new().unwrap();
        write_package_json(
            upstream2_dir.path(),
            r#"{"name":"@repo/core","dependencies":{"glob":"^10.0.0"}}"#,
        );
        let upstream2 = PackageNode::new(
            PackageName::from("@repo/core"),
            upstream2_dir.path().to_path_buf(),
        );

        let graph = PackageGraph::build(vec![upstream2.clone(), upstream.clone(), source.clone()])
            .expect("graph")
            .with_root_package(PackageName::from("//root"));

        // Yarn Berry lockfile
        let lockfile_content = r#"__metadata:
  version: 8
  cacheKey: 10

"chalk@npm:^5.0.0":
  version: 5.0.0
  resolution: "chalk@npm:5.0.0"
  checksum: 10/chalk5
  languageName: node
  linkType: hard

"glob@npm:^10.0.0":
  version: 10.0.0
  resolution: "glob@npm:10.0.0"
  checksum: 10/glob10
  languageName: node
  linkType: hard
"#;
        fs::write(source_dir.path().join("yarn.lock"), lockfile_content).unwrap();

        let lockfile_state = load_lockfile_state(source_dir.path());

        // ^^glob selects glob from transitive upstream (upstream2/@repo/core)
        let pairs = gather_pkg_dep_pairs_filtered(
            &source,
            Some(&graph),
            source_dir.path(),
            &lockfile_state,
            &["^^glob".to_string()],
        )
        .expect("gather filtered should succeed");

        assert!(
            pairs.iter().any(|(n, v)| n == "glob" && v == "10.0.0"),
            "^^glob should select transitive upstream's glob dependency (Berry)"
        );
    }

    /// Test 5: worker_override_replaces_dependency_set — apply a TaskModification
    /// with dependencies: Some(...) to a TaskDefinition, then resolve pairs;
    /// assert a dep dropped by the worker's narrowed set no longer contributes.
    #[test]
    fn worker_override_replaces_dependency_set() {
        use luchta_worker::TaskModification;

        let dir = TempDir::new().unwrap();
        let package = make_package(dir.path());
        write_package_json(
            dir.path(),
            r#"{"name":"app","dependencies":{"left-pad":"^1.0.0","chalk":"^5.0.0"}}"#,
        );
        let lockfile_content = r#"# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
# yarn lockfile v1

left-pad@^1.0.0:
  version "1.0.0"
  resolved "https://registry.yarnpkg.com/left-pad/-/left-pad-1.0.0.tgz#c84e2417581bbb8eaf2b9e3d7a122e572ab1af37"
  integrity sha512-leftpad1

chalk@^5.0.0:
  version "5.0.0"
  resolved "https://registry.yarnpkg.com/chalk/-/chalk-5.0.0.tgz#ae417bf7adye0"
  integrity sha512-chalk5
"#;
        fs::write(dir.path().join("yarn.lock"), lockfile_content).unwrap();
        let lockfile_state = load_lockfile_state(dir.path());

        // Worker narrows to just left-pad
        let mut definition = TaskDefinition::default();
        let modification = TaskModification {
            command: None,
            depends_on: None,
            weight: None,
            dependencies: Some(vec!["left-pad".to_string()]),
            inputs: None,
        };
        modification.apply_to(&mut definition);

        let pairs = gather_pkg_dep_pairs_filtered(
            &package,
            None,
            dir.path(),
            &lockfile_state,
            &definition.dependencies,
        )
        .expect("gather filtered should succeed");

        // Should include left-pad
        assert!(
            pairs.iter().any(|(n, _)| n == "left-pad"),
            "worker-narrowed filter should include left-pad"
        );
        // Should NOT include chalk (dropped by worker)
        assert!(
            !pairs.iter().any(|(n, _)| n == "chalk"),
            "worker-narrowed filter should NOT include chalk (dropped)"
        );
    }

    /// Test 6: worker_override_none_keeps_static — TaskModification with
    /// dependencies: None leaves the static filter untouched.
    #[test]
    fn worker_override_none_keeps_static() {
        use luchta_worker::TaskModification;

        let dir = TempDir::new().unwrap();
        let package = make_package(dir.path());
        write_package_json(
            dir.path(),
            r#"{"name":"app","dependencies":{"left-pad":"^1.0.0","chalk":"^5.0.0"}}"#,
        );
        let lockfile_content = r#"# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
# yarn lockfile v1

left-pad@^1.0.0:
  version "1.0.0"
  resolved "https://registry.yarnpkg.com/left-pad/-/left-pad-1.0.0.tgz#c84e2417581bbb8eaf2b9e3d7a122e572ab1af37"
  integrity sha512-leftpad1

chalk@^5.0.0:
  version "5.0.0"
  resolved "https://registry.yarnpkg.com/chalk/-/chalk-5.0.0.tgz#ae417bf7adye0"
  integrity sha512-chalk5
"#;
        fs::write(dir.path().join("yarn.lock"), lockfile_content).unwrap();
        let lockfile_state = load_lockfile_state(dir.path());

        // Start with static filter selecting only left-pad
        let mut definition = TaskDefinition {
            dependencies: vec!["left-pad".to_string()],
            ..TaskDefinition::default()
        };

        // Apply modification with None for dependencies
        let modification = TaskModification {
            command: None,
            depends_on: None,
            weight: None,
            dependencies: None,
            inputs: None,
        };
        modification.apply_to(&mut definition);

        // Static filter should remain ["left-pad"]
        assert_eq!(
            definition.dependencies,
            vec!["left-pad"],
            "dependencies should remain unchanged when modification is None"
        );

        let pairs = gather_pkg_dep_pairs_filtered(
            &package,
            None,
            dir.path(),
            &lockfile_state,
            &definition.dependencies,
        )
        .expect("gather filtered should succeed");

        // Only left-pad should contribute
        assert!(
            pairs.iter().any(|(n, _)| n == "left-pad"),
            "should include left-pad"
        );
        assert!(
            !pairs.iter().any(|(n, _)| n == "chalk"),
            "should NOT include chalk (static filter remains)"
        );
    }
}
