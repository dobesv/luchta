use std::collections::HashSet;
use std::path::{Path, PathBuf};

use gix::bstr::BStr;
use luchta_types::PackageName;
use luchta_workspace::{PackageGraph, WorkspaceError};
use miette::Diagnostic;
use thiserror::Error;

#[derive(Debug, Error, Diagnostic)]
pub enum SinceError {
    #[error("Not a git repository; --since requires a git repository.")]
    NotGitRepository,
    #[error("Git repository at {0} has no worktree.")]
    NoWorktree(PathBuf),
    #[error("Could not resolve git ref '{reference}': {reason}.")]
    InvalidRef { reference: String, reason: String },
    #[error("Repository has no HEAD commit; --since requires at least one commit.")]
    EmptyRepository,
    #[error("Failed to diff git trees for --since: {0}")]
    Diff(String),
    #[error("Failed to read git status for --since: {0}")]
    Status(String),
    #[error("Failed to query package graph for --since: {0}")]
    Graph(String),
}

impl From<WorkspaceError> for SinceError {
    fn from(error: WorkspaceError) -> Self {
        SinceError::Graph(error.to_string())
    }
}

pub fn discover_repo_root(workspace_root: &Path) -> Result<PathBuf, SinceError> {
    let repo = discover_repo(workspace_root)?;
    let worktree = repo
        .worktree()
        .ok_or_else(|| SinceError::NoWorktree(workspace_root.to_path_buf()))?;
    Ok(worktree.base().to_path_buf())
}

pub fn changed_paths_since(
    workspace_root: &Path,
    since_ref: &str,
) -> Result<HashSet<PathBuf>, SinceError> {
    let repo = discover_repo(workspace_root)?;
    let mut changed_paths = collect_committed_changes(&repo, since_ref)?;
    collect_worktree_changes(&repo, &mut changed_paths)?;
    Ok(changed_paths)
}

pub fn affected_packages(
    workspace_root: &Path,
    repo_root: &Path,
    since_ref: &str,
    package_graph: &PackageGraph,
) -> Result<HashSet<PackageName>, SinceError> {
    let changed_paths = changed_paths_since(workspace_root, since_ref)?;
    let mut changed_packages = HashSet::new();
    let root_package = package_graph.root_package();

    for changed_path in changed_paths {
        let absolute_changed_path = repo_root.join(changed_path);
        let matched_package = package_graph
            .as_graph()
            .node_weights()
            .filter(|node| Some(&node.name) != root_package)
            .filter(|node| absolute_changed_path.strip_prefix(&node.path).is_ok())
            .max_by_key(|node| node.path.components().count());

        if let Some(package) = matched_package {
            changed_packages.insert(package.name.clone());
        }
    }

    package_graph
        .transitive_dependents_of(changed_packages)
        .map_err(SinceError::from)
}

fn discover_repo(workspace_root: &Path) -> Result<gix::Repository, SinceError> {
    gix::discover(workspace_root).map_err(|_| SinceError::NotGitRepository)
}

fn collect_committed_changes(
    repo: &gix::Repository,
    since_ref: &str,
) -> Result<HashSet<PathBuf>, SinceError> {
    let base_tree = repo
        .rev_parse_single(since_ref)
        .map_err(|error| SinceError::InvalidRef {
            reference: since_ref.to_owned(),
            reason: error.to_string(),
        })?
        .object()
        .map_err(|error| SinceError::InvalidRef {
            reference: since_ref.to_owned(),
            reason: error.to_string(),
        })?
        .peel_to_tree()
        .map_err(|error| SinceError::InvalidRef {
            reference: since_ref.to_owned(),
            reason: error.to_string(),
        })?;

    let head_tree = repo
        .rev_parse_single("HEAD")
        .map_err(map_head_error)?
        .object()
        .map_err(map_head_error)?
        .peel_to_tree()
        .map_err(map_head_error)?;

    let mut changed_paths = HashSet::new();
    let mut changes = base_tree
        .changes()
        .map_err(|error| SinceError::Diff(error.to_string()))?;
    changes.options(|options| {
        options.track_rewrites(None);
    });
    changes
        .for_each_to_obtain_tree(&head_tree, |change| {
            use gix::object::tree::diff::{Action, Change};

            match change {
                Change::Addition { location, .. }
                | Change::Deletion { location, .. }
                | Change::Modification { location, .. } => {
                    changed_paths.insert(bstr_to_path(location));
                }
                Change::Rewrite {
                    source_location,
                    location,
                    ..
                } => {
                    changed_paths.insert(bstr_to_path(source_location));
                    changed_paths.insert(bstr_to_path(location));
                }
            }
            Ok::<Action, std::convert::Infallible>(Action::Continue)
        })
        .map_err(|error| SinceError::Diff(error.to_string()))?;

    Ok(changed_paths)
}

fn collect_worktree_changes(
    repo: &gix::Repository,
    changed_paths: &mut HashSet<PathBuf>,
) -> Result<(), SinceError> {
    let status = repo
        .status(gix::progress::Discard)
        .map_err(|error| SinceError::Status(error.to_string()))?
        .untracked_files(gix::status::UntrackedFiles::Files);

    let items = status
        .into_iter(std::iter::empty::<gix::bstr::BString>())
        .map_err(|error| SinceError::Status(error.to_string()))?;

    for item in items {
        let item = item.map_err(|error| SinceError::Status(error.to_string()))?;
        changed_paths.insert(bstr_to_path(item.location()));
    }

    Ok(())
}

fn bstr_to_path(location: &BStr) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(location.as_ref()).into_owned())
}

fn map_head_error(_err: impl std::fmt::Display) -> SinceError {
    SinceError::EmptyRepository
}

#[cfg(test)]
mod tests {
    use super::{affected_packages, changed_paths_since, SinceError};
    use std::collections::HashSet;
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use luchta_types::PackageName;
    use luchta_workspace::{PackageGraph, PackageNode};
    use tempfile::TempDir;

    const PKG_A_JSON: &str = "{\n  \"name\": \"@repo/a\"\n}\n";
    const ROOT_PKG_JSON: &str = "{\n  \"name\": \"root\"\n}\n";

    #[test]
    fn unions_committed_staged_unstaged_and_untracked_changes() {
        let repo = TestRepo::new();

        repo.write_file("a.txt", "base\n");
        repo.write_file("b.txt", "base\n");
        repo.git_add_and_commit_all();
        let base_ref = repo.head_commit();

        repo.write_file("b.txt", "committed change\n");
        repo.git_add_and_commit_all();

        repo.write_file("staged.txt", "staged\n");
        git(repo.path(), ["add", "staged.txt"]);

        repo.write_file("unstaged.txt", "unstaged\n");
        repo.write_file("untracked.txt", "untracked\n");
        repo.write_file(".gitignore", "ignored.txt\n");
        repo.write_file("ignored.txt", "ignored\n");

        let changed = changed_paths_since(repo.path(), &base_ref).unwrap();

        assert!(changed.contains(Path::new("b.txt")));
        assert!(changed.contains(Path::new("staged.txt")));
        assert!(changed.contains(Path::new("unstaged.txt")));
        assert!(changed.contains(Path::new("untracked.txt")));
        assert!(!changed.contains(Path::new("ignored.txt")));
    }

    #[test]
    fn invalid_ref_reports_reference_name() {
        let repo = TestRepo::new();
        repo.write_file("a.txt", "base\n");
        repo.git_add_and_commit_all();

        let err = changed_paths_since(repo.path(), "missing-ref").unwrap_err();
        assert!(matches!(err, SinceError::InvalidRef { .. }));
        assert!(err.to_string().contains("missing-ref"));
    }

    #[test]
    fn non_git_dir_reports_actionable_error() {
        let dir = TempDir::new().unwrap();

        let err = changed_paths_since(dir.path(), "HEAD").unwrap_err();
        assert!(matches!(err, SinceError::NotGitRepository));
        assert_eq!(
            err.to_string(),
            "Not a git repository; --since requires a git repository."
        );
    }

    #[test]
    fn affected_packages_maps_changes_to_packages() {
        // Each case: (initial files, changed file, graph builder, expected set).
        // A direct package change is attributed to that package; changes to
        // repo-root files (including the root package's own directory) map to no
        // package and yield an empty affected set.
        struct Case {
            initial: &'static [(&'static str, &'static str)],
            changed: (&'static str, &'static str),
            graph: fn(&Path) -> PackageGraph,
            expected: &'static [&'static str],
        }

        let cases = [
            Case {
                initial: &[
                    ("packages/a/package.json", PKG_A_JSON),
                    ("packages/a/src/index.ts", "export const value = 1;\n"),
                ],
                changed: ("packages/a/src/index.ts", "export const value = 2;\n"),
                graph: |root| package_graph(root, [("@repo/a", "packages/a")]),
                expected: &["@repo/a"],
            },
            Case {
                initial: &[
                    ("packages/a/package.json", PKG_A_JSON),
                    ("packages/a/src/index.ts", "export const value = 1;\n"),
                    ("README.md", "base\n"),
                ],
                changed: ("README.md", "changed\n"),
                graph: |root| package_graph(root, [("@repo/a", "packages/a")]),
                expected: &[],
            },
            Case {
                initial: &[
                    ("package.json", ROOT_PKG_JSON),
                    ("packages/a/package.json", PKG_A_JSON),
                    ("packages/a/src/index.ts", "export const value = 1;\n"),
                    ("README.md", "base\n"),
                ],
                changed: ("README.md", "changed\n"),
                graph: |root| {
                    package_graph_with_root(
                        root,
                        "root",
                        [("root", "."), ("@repo/a", "packages/a")],
                    )
                },
                expected: &[],
            },
        ];

        for case in cases {
            let repo = TestRepo::new();
            let affected = affected_after_change(&repo, case.initial, case.changed, case.graph);
            assert_eq!(
                affected,
                package_name_set(case.expected.iter().copied()),
                "unexpected affected set for changed file {:?}",
                case.changed.0
            );
        }
    }

    #[test]
    fn affected_packages_include_transitive_dependents() {
        let repo = TestRepo::new();
        repo.write_file("packages/a/package.json", &package_json("@repo/a", &[]));
        repo.write_file(
            "packages/b/package.json",
            &package_json("@repo/b", &["@repo/a"]),
        );
        repo.write_file("packages/a/src/index.ts", "export const value = 1;\n");
        repo.write_file("packages/b/src/index.ts", "export const value = 1;\n");
        repo.git_add_and_commit_all();
        let base_ref = repo.head_commit();

        repo.write_file("packages/a/src/index.ts", "export const value = 2;\n");

        let graph = package_graph(
            repo.path(),
            [("@repo/a", "packages/a"), ("@repo/b", "packages/b")],
        );
        let affected = affected_packages(repo.path(), repo.path(), &base_ref, &graph).unwrap();

        assert_eq!(affected, package_name_set(["@repo/a", "@repo/b"]));
    }

    #[test]
    fn affected_packages_choose_deepest_matching_package() {
        let repo = TestRepo::new();
        repo.write_file("packages/a/package.json", &package_json("@repo/a", &[]));
        repo.write_file(
            "packages/a/nested/package.json",
            &package_json("@repo/nested", &[]),
        );
        repo.write_file(
            "packages/a/nested/src/index.ts",
            "export const value = 1;\n",
        );
        repo.git_add_and_commit_all();
        let base_ref = repo.head_commit();

        repo.write_file(
            "packages/a/nested/src/index.ts",
            "export const value = 2;\n",
        );

        let graph = package_graph(
            repo.path(),
            [
                ("@repo/a", "packages/a"),
                ("@repo/nested", "packages/a/nested"),
            ],
        );
        let affected = affected_packages(repo.path(), repo.path(), &base_ref, &graph).unwrap();

        assert_eq!(affected, package_name_set(["@repo/nested"]));
    }

    fn package_graph<const N: usize>(
        repo_root: &Path,
        packages: [(&str, &str); N],
    ) -> PackageGraph {
        let package_nodes = packages.into_iter().map(|(name, relative_path)| {
            PackageNode::new(PackageName::from(name), repo_root.join(relative_path))
        });

        PackageGraph::build(package_nodes.collect()).unwrap()
    }

    fn package_graph_with_root<const N: usize>(
        repo_root: &Path,
        root_package: &str,
        packages: [(&str, &str); N],
    ) -> PackageGraph {
        package_graph(repo_root, packages).with_root_package(PackageName::from(root_package))
    }

    fn package_json(name: &str, dependencies: &[&str]) -> String {
        let mut json = format!("{{\n  \"name\": \"{name}\"");
        if !dependencies.is_empty() {
            json.push_str(",\n  \"dependencies\": {");
            for (index, dependency) in dependencies.iter().enumerate() {
                let separator = if index + 1 == dependencies.len() {
                    "\n"
                } else {
                    ",\n"
                };
                json.push_str(&format!(
                    "\n    \"{dependency}\": \"workspace:*\"{separator}"
                ));
            }
            json.push_str("  }");
        }
        json.push_str("\n}\n");
        json
    }

    fn package_name_set(names: impl IntoIterator<Item = &'static str>) -> HashSet<PackageName> {
        names.into_iter().map(PackageName::from).collect()
    }

    /// Arrange-act helper for the common affected-packages scenario: write the
    /// `initial` files and commit them as the base, build the package graph
    /// (after the files exist, since `PackageGraph::build` reads each
    /// `package.json`), then overwrite `changed` (path, contents) and compute
    /// the affected set. `build_graph` receives the repo root and returns the
    /// graph so callers can opt into `with_root_package`.
    fn affected_after_change(
        repo: &TestRepo,
        initial: &[(&str, &str)],
        changed: (&str, &str),
        build_graph: impl Fn(&Path) -> PackageGraph,
    ) -> HashSet<PackageName> {
        for (path, contents) in initial {
            repo.write_file(path, contents);
        }
        repo.git_add_and_commit_all();
        let base_ref = repo.head_commit();

        let graph = build_graph(repo.path());

        repo.write_file(changed.0, changed.1);

        affected_packages(repo.path(), repo.path(), &base_ref, &graph).unwrap()
    }

    struct TestRepo {
        root: TempDir,
    }

    impl TestRepo {
        fn new() -> Self {
            let root = TempDir::new().unwrap();
            git(root.path(), ["init"]);
            git(root.path(), ["config", "user.name", "Luchta Tests"]);
            git(root.path(), ["config", "user.email", "luchta@example.com"]);
            Self { root }
        }

        fn path(&self) -> &Path {
            self.root.path()
        }

        fn write_file(&self, relative: &str, contents: &str) {
            let path = self.path().join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, contents).unwrap();
        }

        fn git_add_and_commit_all(&self) {
            static COUNTER: AtomicU64 = AtomicU64::new(1);
            git(self.path(), ["add", "."]);
            let message = format!(
                "commit-{}-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            );
            git(self.path(), ["commit", "-m", &message]);
        }

        fn head_commit(&self) -> String {
            git_output(self.path(), ["rev-parse", "HEAD"])
                .trim()
                .to_owned()
        }
    }

    fn git(repo: &Path, args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>) {
        let status = Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn git_output(
        repo: &Path,
        args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
    ) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap()
    }
}
