use std::collections::{hash_map::Entry, HashMap, HashSet};
use std::path::{Path, PathBuf};

use gix::bstr::BStr;
use gix::status::index_worktree::BuiltinSubmoduleStatus;
use gix::status::plumbing::{self, index_as_worktree::traits::SubmoduleStatus};
use luchta_cache::PathRequestMatcher;
use luchta_engine::{expand_input_patterns, TaskGraph};
use luchta_types::{PackageName, TaskDefinition, TaskId, TaskName};
use luchta_workspace::{PackageGraph, PackageNode, WorkspaceError};
use miette::Diagnostic;
use thiserror::Error;

use crate::cache_ctx::{load_lockfile_state, parse_lockfile_content, LockfileState};
use crate::dep_closure_diff::packages_with_changed_dep_closures;

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
    #[error("Failed to locate workspace lockfile for --since: {0}")]
    LockfilePath(String),
    #[error("Failed to expand task inputs for --since: {0}")]
    InputExpansion(String),
    #[error("Failed to match changed paths against task inputs: {0}")]
    InputMatch(String),
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

/// Maps changed paths to directly changed packages, then includes their transitive
/// dependents.
///
/// Accepts repo-relative paths from git and absolute paths from file watchers.
/// `repo_root.join(path)` is used for normalization; Rust preserves absolute
/// `path` unchanged when joined onto another path.
///
/// The root package is deliberately excluded: it would swallow every
/// repo-root file (`README.md`, etc.) via `strip_prefix` and break the clean
/// no-op on doc-only changes (issue #15, PR #91). Files outside workspace
/// packages are instead matched against declared task inputs in
/// [`declared_input_matches`], which runs as part of [`since_affected`].
pub fn deepest_package_for_path<'a>(
    changed_path: &Path,
    repo_root: &Path,
    package_graph: &'a PackageGraph,
) -> Option<&'a PackageNode> {
    let absolute_changed_path = repo_root.join(changed_path);
    let root_package = package_graph.root_package();

    package_graph
        .as_graph()
        .node_weights()
        .filter(|node| Some(&node.name) != root_package)
        .filter(|node| absolute_changed_path.strip_prefix(&node.path).is_ok())
        .max_by_key(|node| node.path.components().count())
}

pub fn affected_packages_from_paths(
    changed_paths: &HashSet<PathBuf>,
    repo_root: &Path,
    package_graph: &PackageGraph,
) -> Result<HashSet<PackageName>, SinceError> {
    let mut changed_packages = HashSet::new();

    for changed_path in changed_paths {
        if let Some(package) = deepest_package_for_path(changed_path, repo_root, package_graph) {
            changed_packages.insert(package.name.clone());
        }
    }

    package_graph
        .transitive_dependents_of(changed_packages)
        .map_err(SinceError::from)
}

/// Result of the `--since` change analysis.
///
/// `packages` holds every affected package including transitive dependents.
/// The root-task sets classify top-level (`#task`) tasks: only tasks whose
/// declared inputs expand to at least one positive include are subject to
/// `--since` filtering — a changed file that matches one of their declared
/// inputs puts the task in `affected_root_tasks`. Root tasks without declared
/// inputs, or whose inputs expand to nothing positive, keep the historic bypass
/// and always pass.
#[derive(Debug, Default)]
pub struct SinceAffected {
    packages: HashSet<PackageName>,
    affected_root_tasks: HashSet<TaskName>,
    root_tasks_with_inputs: HashSet<TaskName>,
}

impl SinceAffected {
    /// Full construction, as produced by [`since_affected`].
    pub(crate) fn new(
        packages: HashSet<PackageName>,
        affected_root_tasks: HashSet<TaskName>,
        root_tasks_with_inputs: HashSet<TaskName>,
    ) -> Self {
        Self {
            packages,
            affected_root_tasks,
            root_tasks_with_inputs,
        }
    }

    /// Package-only view, e.g. for the watch driver's per-event affected set,
    /// which is computed from task-input dirty tracking alone and carries no
    /// root-task analysis.
    pub fn from_packages(packages: HashSet<PackageName>) -> Self {
        Self {
            packages,
            ..Self::default()
        }
    }

    pub fn packages(&self) -> &HashSet<PackageName> {
        &self.packages
    }

    pub fn contains_package(&self, package: &PackageName) -> bool {
        self.packages.contains(package)
    }

    pub fn contains_affected_root_task(&self, task: &TaskName) -> bool {
        self.affected_root_tasks.contains(task)
    }

    /// Whether a top-level (`#task`) task passes the `--since` filter.
    ///
    /// Tasks without declared inputs — or whose inputs expand to no positive
    /// include, so they can never match a change — keep the historic bypass and
    /// are never filtered here. Tasks whose inputs do include something pass
    /// only when a changed file matched one of them.
    pub fn root_task_passes(&self, task: &TaskName) -> bool {
        !self.root_tasks_with_inputs.contains(task) || self.contains_affected_root_task(task)
    }
}

/// Full `--since` impact analysis: affected packages plus root-task matches.
///
/// Package affinity comes from changed files inside package directories (as
/// [`affected_packages_from_paths`]) and additionally from changed files that
/// match any task's declared inputs — this is what makes root-level files like
/// `#tsconfig.base.json` or `scripts/**` affect their declaring tasks instead
/// of being dropped on the floor.
pub fn since_affected(
    workspace_root: &Path,
    repo_root: &Path,
    since_ref: &str,
    package_graph: &PackageGraph,
    task_graph: &TaskGraph,
) -> Result<SinceAffected, SinceError> {
    let changed_paths = changed_paths_since(workspace_root, since_ref)?;
    let mut changed_packages = packages_changed_by_paths_and_lockfile(
        workspace_root,
        repo_root,
        since_ref,
        package_graph,
        &changed_paths,
    )?;

    let declared = declared_input_matches(
        &changed_paths,
        repo_root,
        workspace_root,
        package_graph,
        task_graph,
        &changed_packages,
    )?;
    changed_packages.extend(declared.input_matched_packages);

    // Path-based changes already include dependents; expand again for the
    // closure- and input-change seeds.
    let packages = package_graph
        .transitive_dependents_of(changed_packages)
        .map_err(SinceError::from)?;
    Ok(SinceAffected::new(
        packages,
        declared.affected_root_tasks,
        declared.root_tasks_with_inputs,
    ))
}

/// Path-to-package mapping plus the `yarn.lock` dependency-closure diff.
/// The result still needs transitive-dependent expansion by the caller.
fn packages_changed_by_paths_and_lockfile(
    workspace_root: &Path,
    repo_root: &Path,
    since_ref: &str,
    package_graph: &PackageGraph,
    changed_paths: &HashSet<PathBuf>,
) -> Result<HashSet<PackageName>, SinceError> {
    let mut changed_packages =
        affected_packages_from_paths(changed_paths, repo_root, package_graph)?;

    let lockfile_path = repo_relative_lockfile_path(workspace_root, repo_root)?;
    let repo = discover_repo(workspace_root)?;
    let base_tree = resolve_ref_tree(&repo, since_ref)?;
    let base = lockfile_state_from_tree(&base_tree, &lockfile_path);
    let head = load_lockfile_state(workspace_root);
    let candidates = package_graph
        .as_graph()
        .node_weights()
        .filter(|node| Some(&node.name) != package_graph.root_package());
    // Note: no byte-equality fast path (e.g., base.blob == head.bytes) because
    // identical malformed lockfiles at both base and head must select all
    // candidates. The Failed → all rule in packages_with_changed_dep_closures
    // runs first.
    changed_packages.extend(packages_with_changed_dep_closures(
        candidates,
        package_graph,
        &base,
        &head,
    ));

    Ok(changed_packages)
}

/// Declared-input matching results: packages of non-root tasks whose inputs
/// matched, root tasks whose inputs matched, and root tasks whose declared
/// inputs subject them to `--since` filtering.
#[derive(Default)]
struct DeclaredInputMatches {
    input_matched_packages: HashSet<PackageName>,
    affected_root_tasks: HashSet<TaskName>,
    root_tasks_with_inputs: HashSet<TaskName>,
}

/// Matches changed files against declared (expanded) inputs of tasks whose
/// package is not already affected, plus all root tasks.
///
/// A non-root task whose inputs match a changed file seeds its package into
/// the affected set (transitive dependents are expanded by the caller); a
/// root task's match lands in `affected_root_tasks`.
///
/// Tasks without declared inputs never match anything here; a changed file
/// that maps to no package and matches no declared input affects nothing —
/// this is what keeps undeclared root edits like `README.md` a no-op. With no
/// changed paths, package tasks are skipped outright because nothing can
/// match them; root tasks still expand, because whether their inputs resolve
/// to any positive include decides whether they are filtered or keep the
/// historic bypass.
///
/// `already_affected` includes path-mapped packages, their transitive dependents,
/// and lockfile dependency-closure seeds. Package tasks are also skipped after
/// an earlier task matches an input in the same package. Expansion and compile
/// errors from package tasks are deferred until all tasks have been checked:
/// they only fail the call if their package remains unaffected, regardless of
/// task iteration order. Root-task errors still fail immediately.
///
/// Inputs are expanded with the engine's shared grammar (`#`, `pkg#`, `^`,
/// `^^`, plain) and matched with the cache's [`PathRequestMatcher`], so
/// negated patterns (`!...`) exclude files exactly as they do for cache
/// hashing. A root task whose inputs expand to no positive include (only
/// negations, or `^`/`^^` when the root has no upstream packages) can never
/// match a change and keeps the historic bypass instead of being filtered out
/// of every `--since` run.
fn declared_input_matches(
    changed_paths: &HashSet<PathBuf>,
    repo_root: &Path,
    workspace_root: &Path,
    package_graph: &PackageGraph,
    task_graph: &TaskGraph,
    already_affected: &HashSet<PackageName>,
) -> Result<DeclaredInputMatches, SinceError> {
    let tasks = task_graph.nodes().filter_map(|node| {
        task_graph
            .task_definition(&node.id)
            .map(|definition| (&node.id, definition))
    });
    declared_input_matches_for_tasks(
        changed_paths,
        repo_root,
        workspace_root,
        package_graph,
        tasks,
        already_affected,
    )
}

fn declared_input_matches_for_tasks<'a>(
    changed_paths: &HashSet<PathBuf>,
    repo_root: &Path,
    workspace_root: &Path,
    package_graph: &PackageGraph,
    tasks: impl IntoIterator<Item = (&'a TaskId, &'a TaskDefinition)>,
    already_affected: &HashSet<PackageName>,
) -> Result<DeclaredInputMatches, SinceError> {
    let mut matches = DeclaredInputMatches::default();
    let mut matchers = HashMap::new();
    let mut input_errors = Vec::new();

    // Git hands us repo-relative paths; joining the repo root yields the
    // absolute paths the input requests (based in workspace/package dirs)
    // strip their prefixes from. Hoisted so each path is joined once.
    let absolute_paths: Vec<PathBuf> = changed_paths
        .iter()
        .map(|changed_path| repo_root.join(changed_path))
        .collect();

    for (task, definition) in tasks {
        if definition.inputs.is_empty() {
            continue;
        }

        // A package task can only add its own package, so checking an already
        // affected package cannot change the result. Root tasks still need
        // expansion for the bypass classification below, even with no changes.
        if !task.is_root()
            && (absolute_paths.is_empty()
                || already_affected.contains(&task.package)
                || matches.input_matched_packages.contains(&task.package))
        {
            continue;
        }

        let matcher = match matchers.entry((&task.package, definition.inputs.as_slice())) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                match compile_input_matcher(task, &definition.inputs, package_graph, workspace_root)
                {
                    Ok(matcher) => entry.insert(matcher),
                    Err(error) if task.is_root() => return Err(error),
                    Err(error) => {
                        input_errors.push((task, error));
                        continue;
                    }
                }
            }
        };

        if task.is_root() {
            // Classification comes from the expanded requests, not the raw
            // pattern strings: `^` inputs on a root task do resolve to
            // includes when the root package has upstreams. Only a root task
            // with at least one positive include can ever match a change, so
            // only those are filtered by `--since`; the rest keep the
            // historic bypass.
            if !matcher.has_includes() {
                continue;
            }
            matches.root_tasks_with_inputs.insert(task.task.clone());
        }

        for absolute_path in &absolute_paths {
            if !matcher.matches_path(absolute_path) {
                continue;
            }
            if task.is_root() {
                matches.affected_root_tasks.insert(task.task.clone());
            } else {
                matches.input_matched_packages.insert(task.package.clone());
            }
            break;
        }
    }

    // A later input match makes earlier errors in that package irrelevant.
    // Choose by task id so errors in still-unaffected packages are stable too.
    if let Some((_, error)) = input_errors
        .into_iter()
        .filter(|(task, _)| {
            !already_affected.contains(&task.package)
                && !matches.input_matched_packages.contains(&task.package)
        })
        .min_by_key(|(task, _)| task.to_string())
    {
        return Err(error);
    }

    Ok(matches)
}

fn compile_input_matcher(
    task: &TaskId,
    inputs: &[String],
    package_graph: &PackageGraph,
    workspace_root: &Path,
) -> Result<PathRequestMatcher, SinceError> {
    let requests = expand_input_patterns(inputs, &task.package, package_graph, workspace_root)
        .map_err(|error| SinceError::InputExpansion(format!("task '{task}': {error}")))?;
    PathRequestMatcher::compile(&requests)
        .map_err(|error| SinceError::InputMatch(format!("task '{task}': {error}")))
}

fn repo_relative_lockfile_path(
    workspace_root: &Path,
    repo_root: &Path,
) -> Result<PathBuf, SinceError> {
    let workspace_root = dunce::canonicalize(workspace_root)
        .map_err(|error| SinceError::LockfilePath(error.to_string()))?;
    let repo_root = dunce::canonicalize(repo_root)
        .map_err(|error| SinceError::LockfilePath(error.to_string()))?;
    workspace_root
        .strip_prefix(&repo_root)
        .map(|relative| relative.join("yarn.lock"))
        .map_err(|error| {
            SinceError::LockfilePath(format!(
                "workspace '{}' is outside repository '{}': {error}",
                workspace_root.display(),
                repo_root.display()
            ))
        })
}

fn lockfile_state_from_tree(tree: &gix::Tree<'_>, path: &Path) -> LockfileState {
    let entry = match tree.lookup_entry_by_path(path) {
        Ok(Some(entry)) => entry,
        Ok(None) => return LockfileState::Absent,
        Err(error) => return LockfileState::Failed(error.to_string()),
    };
    let object = match entry.object() {
        Ok(object) => object,
        Err(error) => return LockfileState::Failed(error.to_string()),
    };
    let blob = match object.try_into_blob() {
        Ok(blob) => blob,
        Err(error) => return LockfileState::Failed(error.to_string()),
    };
    match std::str::from_utf8(&blob.data) {
        Ok(content) => parse_lockfile_content(content),
        Err(error) => LockfileState::Failed(error.to_string()),
    }
}

fn discover_repo(workspace_root: &Path) -> Result<gix::Repository, SinceError> {
    gix::discover(workspace_root).map_err(|_| SinceError::NotGitRepository)
}

fn resolve_ref_tree<'repo>(
    repo: &'repo gix::Repository,
    since_ref: &str,
) -> Result<gix::Tree<'repo>, SinceError> {
    repo.rev_parse_single(since_ref)
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
        })
}

fn collect_committed_changes(
    repo: &gix::Repository,
    since_ref: &str,
) -> Result<HashSet<PathBuf>, SinceError> {
    let base_tree = resolve_ref_tree(repo, since_ref)?;

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
            Ok::<Action, std::convert::Infallible>(Action::Continue(()))
        })
        .map_err(|error| SinceError::Diff(error.to_string()))?;

    Ok(changed_paths)
}

fn collect_worktree_changes(
    repo: &gix::Repository,
    changed_paths: &mut HashSet<PathBuf>,
) -> Result<(), SinceError> {
    let index = repo.index_or_empty().map_err(map_status_error)?;
    collect_unstaged_changes(repo, &index, changed_paths)?;
    collect_staged_changes(repo, &index, changed_paths)
}

fn collect_unstaged_changes(
    repo: &gix::Repository,
    index: &gix::index::State,
    changed_paths: &mut HashSet<PathBuf>,
) -> Result<(), SinceError> {
    let mut records = plumbing::index_as_worktree_with_renames::Recorder::default();
    let options = gix::status::index_worktree::Options {
        sorting: None,
        rewrites: None,
        thread_limit: None,
        dirwalk_options: Some(
            repo.dirwalk_options()
                .map_err(map_status_error)?
                .emit_untracked(gix::dir::walk::EmissionMode::Matching),
        ),
    };
    // The high-level status iterator reads .gitmodules even without gitlinks.
    repo.index_worktree_status(
        index,
        std::iter::empty::<&BStr>(),
        &mut records,
        plumbing::index_as_worktree::traits::FastEq,
        WorktreeSubmodules::new(repo, index),
        &mut gix::progress::Discard,
        &std::sync::atomic::AtomicBool::new(false),
        options,
    )
    .map_err(map_status_error)?;

    for record in records.records {
        // Stat-only index refreshes and ignored dirwalk entries aren't changes.
        if record.summary().is_some() {
            changed_paths.insert(bstr_to_path(record.destination_rela_path()));
        }
    }
    Ok(())
}

fn collect_staged_changes(
    repo: &gix::Repository,
    index: &gix::index::State,
    changed_paths: &mut HashSet<PathBuf>,
) -> Result<(), SinceError> {
    repo.tree_index_status(
        &repo.head_tree_id_or_empty().map_err(map_status_error)?,
        index,
        None,
        // Report both paths of a rename as removal and addition.
        gix::status::tree_index::TrackRenames::Disabled,
        |change, _, _| {
            changed_paths.insert(bstr_to_path(change.location()));
            Ok::<_, std::convert::Infallible>(gix::diff::index::Action::Continue(()))
        },
    )
    .map_err(map_status_error)?;
    Ok(())
}

/// Submodule status provider that only loads `.gitmodules` when the index has
/// gitlinks, and treats a config load failure as "no submodules configured".
///
/// gix's high-level `status().into_iter()` reads `.gitmodules` unconditionally
/// and fails when it can't, while git ignores the file unless submodules exist
/// (#941).
#[derive(Clone)]
struct WorktreeSubmodules(Option<BuiltinSubmoduleStatus>);

impl WorktreeSubmodules {
    fn new(repo: &gix::Repository, index: &gix::index::State) -> Self {
        if !index
            .entries()
            .iter()
            .any(|entry| entry.mode == gix::index::entry::Mode::COMMIT)
        {
            return Self(None);
        }
        let status = BuiltinSubmoduleStatus::new(repo.clone().into_sync(), Default::default())
            .inspect_err(|error| {
                eprintln!(
                    "warning: unable to read submodule configuration for --since: {error}; \
                     continuing with submodules unconfigured"
                );
            })
            .ok();
        Self(status)
    }
}

impl SubmoduleStatus for WorktreeSubmodules {
    type Output = <BuiltinSubmoduleStatus as SubmoduleStatus>::Output;
    type Error = <BuiltinSubmoduleStatus as SubmoduleStatus>::Error;

    fn status(
        &mut self,
        entry: &gix::index::Entry,
        rela_path: &BStr,
    ) -> Result<Option<Self::Output>, Self::Error> {
        match &mut self.0 {
            Some(status) => status.status(entry, rela_path),
            None => Ok(None),
        }
    }
}

fn map_status_error(error: impl std::fmt::Display) -> SinceError {
    SinceError::Status(error.to_string())
}

fn bstr_to_path(location: &BStr) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(location.as_ref()).into_owned())
}

fn map_head_error(_err: impl std::fmt::Display) -> SinceError {
    SinceError::EmptyRepository
}

#[cfg(test)]
mod tests {
    use super::{
        affected_packages_from_paths, changed_paths_since, declared_input_matches,
        declared_input_matches_for_tasks, since_affected, SinceError,
    };
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use luchta_engine::TaskGraph;
    use luchta_types::{PackageName, TaskDefinition, TaskId, TaskName};
    use luchta_workspace::{PackageGraph, PackageNode};
    use tempfile::TempDir;

    const PKG_A_JSON: &str = "{\n  \"name\": \"@repo/a\"\n}\n";
    const ROOT_PKG_JSON: &str = "{\n  \"name\": \"root\"\n}\n";

    /// Package-level view of [`since_affected`] over an empty task graph: no
    /// task declares inputs, so the result is purely the package analysis and
    /// the production `since_affected` path is what the tests exercise.
    fn affected_packages(
        workspace_root: &Path,
        repo_root: &Path,
        since_ref: &str,
        package_graph: &PackageGraph,
    ) -> Result<HashSet<PackageName>, SinceError> {
        let task_graph =
            TaskGraph::build(package_graph, &HashMap::new()).expect("empty task graph");
        Ok(since_affected(
            workspace_root,
            repo_root,
            since_ref,
            package_graph,
            &task_graph,
        )?
        .packages()
        .clone())
    }

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
    fn gitmodules_directory_without_gitlinks_does_not_break_status() {
        let repo = TestRepo::with_status_changes();
        fs::create_dir(repo.path().join(".gitmodules")).unwrap();
        repo.assert_status_changes(&[]);
    }

    #[test]
    fn gitmodules_directory_with_gitlinks_does_not_break_status() {
        let repo = TestRepo::with_status_changes();
        repo.add_gitlink();
        fs::create_dir(repo.path().join(".gitmodules")).unwrap();
        repo.assert_status_changes(&["sub"]);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_gitmodules_without_gitlinks_does_not_break_status() {
        let repo = TestRepo::with_status_changes();
        let Some(_permissions) = repo.unreadable_gitmodules() else {
            return;
        };
        repo.assert_status_changes(&[".gitmodules"]);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_gitmodules_with_gitlinks_does_not_break_status() {
        let repo = TestRepo::with_status_changes();
        repo.add_gitlink();
        let Some(_permissions) = repo.unreadable_gitmodules() else {
            return;
        };
        repo.assert_status_changes(&[".gitmodules", "sub"]);
    }

    #[test]
    fn readable_gitmodules_preserves_configured_submodule_status() {
        let repo = TestRepo::new();
        repo.write_file("tracked.txt", "base\n");
        repo.git_add_and_commit_all();
        let sub = TestRepo::new();
        sub.write_file("tracked.txt", "base\n");
        sub.git_add_and_commit_all();
        git(
            repo.path(),
            [
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub.path().to_str().unwrap(),
                "sub",
            ],
        );
        repo.git_add_and_commit_all();
        repo.write_file("sub/tracked.txt", "dirty\n");

        let changed = changed_paths_since(repo.path(), "HEAD").unwrap();
        assert_eq!(changed, HashSet::from(["sub".into()]));

        git(repo.path(), ["config", "submodule.sub.ignore", "all"]);
        assert!(changed_paths_since(repo.path(), "HEAD").unwrap().is_empty());
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

    /// Committed two-package workspace (`@repo/b` depends on `@repo/a`) with a
    /// `tsconfig.base.json` at the repo root, returning the repo, its base ref,
    /// and the package graph. Root files changed after this helper returns are
    /// invisible to plain path-to-package mapping.
    fn repo_with_root_file_workspace() -> (TestRepo, String, PackageGraph) {
        let repo = TestRepo::new();
        repo.write_file("packages/a/package.json", &package_json("@repo/a", &[]));
        repo.write_file(
            "packages/b/package.json",
            &package_json("@repo/b", &["@repo/a"]),
        );
        repo.write_file("tsconfig.base.json", "{}\n");
        repo.git_add_and_commit_all();
        let base_ref = repo.head_commit();

        let graph = package_graph(
            repo.path(),
            [("@repo/a", "packages/a"), ("@repo/b", "packages/b")],
        );
        (repo, base_ref, graph)
    }

    #[test]
    fn since_affected_selects_package_with_declared_root_input_and_its_dependents() {
        let (repo, base_ref, graph) = repo_with_root_file_workspace();
        let pipeline = HashMap::from([(
            TaskName::from("@repo/a#build"),
            TaskDefinition {
                inputs: vec!["#tsconfig.base.json".to_string()],
                ..TaskDefinition::default()
            },
        )]);
        let task_graph = TaskGraph::build(&graph, &pipeline).unwrap();

        repo.write_file("tsconfig.base.json", "{\n  \"changed\": true\n}\n");

        let affected =
            since_affected(repo.path(), repo.path(), &base_ref, &graph, &task_graph).unwrap();

        // `@repo/a` declares the root input; `@repo/b` depends on it and rides
        // along through transitive-dependent expansion.
        assert_eq!(
            affected.packages(),
            &package_name_set(["@repo/a", "@repo/b"])
        );
    }

    #[test]
    fn since_affected_selects_root_task_when_declared_inputs_match() {
        let (repo, base_ref, graph) = repo_with_root_file_workspace();
        let pipeline = HashMap::from([(
            TaskName::from("#deploy"),
            TaskDefinition {
                inputs: vec!["scripts/**".to_string()],
                ..TaskDefinition::default()
            },
        )]);
        let task_graph = TaskGraph::build(&graph, &pipeline).unwrap();

        repo.write_file("scripts/run.sh", "echo changed\n");

        let affected =
            since_affected(repo.path(), repo.path(), &base_ref, &graph, &task_graph).unwrap();

        assert!(affected.packages().is_empty());
        assert!(affected.contains_affected_root_task(&TaskName::from("deploy")));
        assert!(affected.root_task_passes(&TaskName::from("deploy")));
    }

    #[test]
    fn since_affected_excludes_root_task_when_declared_inputs_do_not_match() {
        let (repo, base_ref, graph) = repo_with_root_file_workspace();
        let pipeline = HashMap::from([(
            TaskName::from("#deploy"),
            TaskDefinition {
                inputs: vec!["scripts/**".to_string()],
                ..TaskDefinition::default()
            },
        )]);
        let task_graph = TaskGraph::build(&graph, &pipeline).unwrap();

        // Undeclared root file: matches no declared input anywhere.
        repo.write_file("README.md", "root only change\n");

        let affected =
            since_affected(repo.path(), repo.path(), &base_ref, &graph, &task_graph).unwrap();

        assert!(affected.packages().is_empty());
        assert!(!affected.contains_affected_root_task(&TaskName::from("deploy")));
        // The task declares inputs, so it is filtered instead of bypassing.
        assert!(!affected.root_task_passes(&TaskName::from("deploy")));
    }

    #[test]
    fn since_affected_respects_negated_root_task_inputs() {
        let (repo, base_ref, graph) = repo_with_root_file_workspace();
        let pipeline = HashMap::from([(
            TaskName::from("#deploy"),
            TaskDefinition {
                inputs: vec!["scripts/**".to_string(), "!scripts/ignored/**".to_string()],
                ..TaskDefinition::default()
            },
        )]);
        let task_graph = TaskGraph::build(&graph, &pipeline).unwrap();

        repo.write_file("scripts/ignored/tool.sh", "echo ignored\n");
        let affected =
            since_affected(repo.path(), repo.path(), &base_ref, &graph, &task_graph).unwrap();
        assert!(!affected.contains_affected_root_task(&TaskName::from("deploy")));

        repo.write_file("scripts/kept.sh", "echo kept\n");
        let affected =
            since_affected(repo.path(), repo.path(), &base_ref, &graph, &task_graph).unwrap();
        assert!(affected.contains_affected_root_task(&TaskName::from("deploy")));
    }

    #[test]
    fn since_affected_respects_negated_root_inputs_on_package_tasks() {
        let (repo, base_ref, graph) = repo_with_root_file_workspace();
        let pipeline = HashMap::from([(
            TaskName::from("@repo/a#build"),
            TaskDefinition {
                inputs: vec!["#scripts/**".to_string(), "!scripts/ignored/**".to_string()],
                ..TaskDefinition::default()
            },
        )]);
        let task_graph = TaskGraph::build(&graph, &pipeline).unwrap();

        repo.write_file("scripts/ignored/tool.sh", "echo ignored\n");
        let affected =
            since_affected(repo.path(), repo.path(), &base_ref, &graph, &task_graph).unwrap();
        assert_eq!(affected.packages(), &HashSet::new());

        repo.write_file("scripts/kept.sh", "echo kept\n");
        let affected =
            since_affected(repo.path(), repo.path(), &base_ref, &graph, &task_graph).unwrap();
        assert_eq!(
            affected.packages(),
            &package_name_set(["@repo/a", "@repo/b"])
        );
    }

    #[test]
    fn root_task_with_negation_only_inputs_keeps_since_bypass() {
        let (repo, base_ref, graph) = repo_with_root_file_workspace();
        let pipeline = HashMap::from([(
            TaskName::from("#deploy"),
            TaskDefinition {
                inputs: vec!["!scripts/**".to_string()],
                ..TaskDefinition::default()
            },
        )]);
        let task_graph = TaskGraph::build(&graph, &pipeline).unwrap();

        repo.write_file("scripts/run.sh", "echo run\n");
        let affected =
            since_affected(repo.path(), repo.path(), &base_ref, &graph, &task_graph).unwrap();

        // Excludes with nothing to exclude from can never match; the task
        // keeps the historic bypass instead of never running under -T.
        assert!(!affected.contains_affected_root_task(&TaskName::from("deploy")));
        assert!(affected.root_task_passes(&TaskName::from("deploy")));
    }

    #[test]
    fn root_task_with_upstream_only_inputs_keeps_since_bypass() {
        let (repo, base_ref, graph) = repo_with_root_file_workspace();
        let pipeline = HashMap::from([(
            TaskName::from("#deploy"),
            TaskDefinition {
                inputs: vec!["^src/**".to_string()],
                ..TaskDefinition::default()
            },
        )]);
        let task_graph = TaskGraph::build(&graph, &pipeline).unwrap();

        repo.write_file("README.md", "root only change\n");
        let affected =
            since_affected(repo.path(), repo.path(), &base_ref, &graph, &task_graph).unwrap();

        // This fixture has no root package, so the root task's `^` inputs
        // resolve to nothing and can never match; the task keeps the bypass.
        // (A root package with workspace dependencies is covered by
        // `root_task_upstream_inputs_resolve_when_root_package_has_workspace_deps`.)
        assert!(!affected.contains_affected_root_task(&TaskName::from("deploy")));
        assert!(affected.root_task_passes(&TaskName::from("deploy")));
    }

    #[test]
    fn root_task_upstream_inputs_resolve_when_root_package_has_workspace_deps() {
        let repo = TestRepo::new();
        repo.write_file("package.json", &package_json("root", &["@repo/a"]));
        repo.write_file("packages/a/package.json", &package_json("@repo/a", &[]));
        repo.write_file(
            "packages/b/package.json",
            &package_json("@repo/b", &["@repo/a"]),
        );
        repo.git_add_and_commit_all();
        let base_ref = repo.head_commit();

        let graph = package_graph_with_root(
            repo.path(),
            "root",
            [
                ("root", "."),
                ("@repo/a", "packages/a"),
                ("@repo/b", "packages/b"),
            ],
        );
        let pipeline = HashMap::from([(
            TaskName::from("#deploy"),
            TaskDefinition {
                inputs: vec!["^src/**".to_string()],
                ..TaskDefinition::default()
            },
        )]);
        let task_graph = TaskGraph::build(&graph, &pipeline).unwrap();

        repo.write_file("packages/a/src/index.ts", "export const changed = true;\n");
        let affected =
            since_affected(repo.path(), repo.path(), &base_ref, &graph, &task_graph).unwrap();
        assert!(affected.contains_affected_root_task(&TaskName::from("deploy")));

        repo.git_add_and_commit_all();
        let base_ref = repo.head_commit();
        repo.write_file("README.md", "root only change\n");
        let affected =
            since_affected(repo.path(), repo.path(), &base_ref, &graph, &task_graph).unwrap();
        assert!(!affected.contains_affected_root_task(&TaskName::from("deploy")));
        // The root package has @repo/a as an upstream, so `^` resolved to a
        // positive include and the task is filtered like any declared-input
        // root task instead of keeping the bypass.
        assert!(!affected.root_task_passes(&TaskName::from("deploy")));
    }

    #[test]
    fn root_task_with_specific_package_inputs_is_filtered_by_since() {
        let (repo, base_ref, graph) = repo_with_root_file_workspace();
        let pipeline = HashMap::from([(
            TaskName::from("#deploy"),
            TaskDefinition {
                inputs: vec!["@repo/a#src/**".to_string()],
                ..TaskDefinition::default()
            },
        )]);
        let task_graph = TaskGraph::build(&graph, &pipeline).unwrap();

        repo.write_file("packages/a/src/index.ts", "export const changed = true;\n");
        let affected =
            since_affected(repo.path(), repo.path(), &base_ref, &graph, &task_graph).unwrap();
        assert!(affected.contains_affected_root_task(&TaskName::from("deploy")));

        // Commit the change so the second scenario starts from a clean tree.
        repo.git_add_and_commit_all();
        let base_ref = repo.head_commit();
        repo.write_file("README.md", "root only change\n");
        let affected =
            since_affected(repo.path(), repo.path(), &base_ref, &graph, &task_graph).unwrap();
        assert!(!affected.contains_affected_root_task(&TaskName::from("deploy")));
        // A positive `pkg#` input subjects the task to filtering: with no
        // matching change it does NOT run under -T.
        assert!(!affected.root_task_passes(&TaskName::from("deploy")));
    }

    #[test]
    fn invalid_input_pattern_on_any_task_fails_since_with_task_id() {
        let (repo, base_ref, graph) = repo_with_root_file_workspace();
        // The invalid pattern sits on an otherwise unrelated package task.
        let pipeline = HashMap::from([(
            TaskName::from("@repo/b#build"),
            TaskDefinition {
                inputs: vec!["#".to_string()],
                ..TaskDefinition::default()
            },
        )]);
        let task_graph = TaskGraph::build(&graph, &pipeline).unwrap();

        // No package is path-affected, so @repo/b's inputs must be checked.
        repo.write_file("README.md", "root only change\n");

        let error =
            since_affected(repo.path(), repo.path(), &base_ref, &graph, &task_graph).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("task '@repo/b#build'"),
            "expected task id in error, got: {message}"
        );
        assert!(
            message.contains("root input must include path"),
            "expected pattern error in message, got: {message}"
        );
    }

    #[test]
    fn invalid_input_pattern_in_already_affected_package_is_skipped() {
        for changed_package in ["a", "b"] {
            let (repo, base_ref, graph) = repo_with_root_file_workspace();
            let pipeline = HashMap::from([(
                TaskName::from("@repo/b#build"),
                TaskDefinition {
                    inputs: vec!["#".to_string()],
                    ..TaskDefinition::default()
                },
            )]);
            let task_graph = TaskGraph::build(&graph, &pipeline).unwrap();

            // @repo/b is affected either directly or as @repo/a's dependent.
            repo.write_file(
                &format!("packages/{changed_package}/src/index.ts"),
                "export const changed = true;\n",
            );
            let expected = if changed_package == "a" {
                package_name_set(["@repo/a", "@repo/b"])
            } else {
                package_name_set(["@repo/b"])
            };
            let affected =
                since_affected(repo.path(), repo.path(), &base_ref, &graph, &task_graph).unwrap();
            assert_eq!(
                affected.packages(),
                &expected,
                "changed package: {changed_package}"
            );
        }
    }

    #[test]
    fn matching_task_suppresses_invalid_sibling_inputs_in_either_order() {
        let (repo, _, graph) = repo_with_root_file_workspace();
        let matching_task = TaskId::new("@repo/b", "build");
        let invalid_task = TaskId::new("@repo/b", "invalid");
        let matching_definition = TaskDefinition {
            inputs: vec!["#tsconfig.base.json".to_string()],
            ..TaskDefinition::default()
        };
        let changed_paths = HashSet::from([PathBuf::from("tsconfig.base.json")]);
        let already_affected =
            affected_packages_from_paths(&changed_paths, repo.path(), &graph).unwrap();
        assert!(already_affected.is_empty());

        // Exercise expansion and compilation failures, not harmless non-matches.
        for invalid_input in ["#", "["] {
            let invalid_definition = TaskDefinition {
                inputs: vec![invalid_input.to_string()],
                ..TaskDefinition::default()
            };
            let error = declared_input_matches_for_tasks(
                &changed_paths,
                repo.path(),
                repo.path(),
                &graph,
                [(&invalid_task, &invalid_definition)],
                &already_affected,
            )
            .err()
            .expect("an unaffected package with only an invalid task must fail");
            assert!(matches!(
                (invalid_input, error),
                ("#", SinceError::InputExpansion(_)) | ("[", SinceError::InputMatch(_))
            ));

            let invalid = (&invalid_task, &invalid_definition);
            let matching = (&matching_task, &matching_definition);
            // Explicit order avoids relying on TaskGraph's randomized HashMap.
            for tasks in [[invalid, matching], [matching, invalid]] {
                let matches = declared_input_matches_for_tasks(
                    &changed_paths,
                    repo.path(),
                    repo.path(),
                    &graph,
                    tasks,
                    &already_affected,
                )
                .unwrap();
                assert_eq!(
                    matches.input_matched_packages,
                    package_name_set(["@repo/b"]),
                    "input {invalid_input:?}, first task {}",
                    tasks[0].0
                );
            }
        }
    }

    #[test]
    fn unaffected_package_input_errors_are_reported_by_task_id_in_any_order() {
        let (repo, _, graph) = repo_with_root_file_workspace();
        let ids = [
            TaskId::new("@repo/a", "build"),
            TaskId::new("@repo/a", "lint"),
            TaskId::new("@repo/b", "build"),
        ];
        let definition = TaskDefinition {
            inputs: vec!["#".to_string()],
            ..TaskDefinition::default()
        };
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let error = declared_input_matches_for_tasks(
                &HashSet::from([PathBuf::from("README.md")]),
                repo.path(),
                repo.path(),
                &graph,
                order.map(|index| (&ids[index], &definition)),
                &HashSet::new(),
            )
            .err()
            .expect("unaffected packages must still report invalid inputs");
            assert!(
                error.to_string().contains("task '@repo/a#build'"),
                "order {order:?}: {error}"
            );
        }
    }

    #[test]
    fn identical_inputs_are_cached_per_package_without_changing_matches() {
        let (repo, _, graph) = repo_with_root_file_workspace();
        let pipeline = ["build", "lint"]
            .into_iter()
            .map(|task| {
                (
                    TaskName::from(task),
                    TaskDefinition {
                        inputs: vec!["src/**".to_string()],
                        ..TaskDefinition::default()
                    },
                )
            })
            .collect();
        let task_graph = TaskGraph::build(&graph, &pipeline).unwrap();

        // Unmatched tasks exercise cache hits rather than the affected-package
        // shortcut. Matching tasks also exercise skipping a later same-package task.
        for (path, expected) in [
            ("README.md", HashSet::new()),
            ("packages/a/src/index.ts", package_name_set(["@repo/a"])),
            ("packages/b/src/index.ts", package_name_set(["@repo/b"])),
        ] {
            let matches = declared_input_matches(
                &HashSet::from([PathBuf::from(path)]),
                repo.path(),
                repo.path(),
                &graph,
                &task_graph,
                &HashSet::new(),
            )
            .unwrap();
            assert_eq!(matches.input_matched_packages, expected, "{path}");
        }
    }

    #[test]
    fn cached_root_inputs_still_classify_each_task_and_distinct_input_list() {
        let (repo, base_ref, graph) = repo_with_root_file_workspace();
        let pipeline = [
            ("#deploy", "tsconfig.base.json"),
            ("#check", "tsconfig.base.json"),
            ("#lint", "scripts/**"),
        ]
        .into_iter()
        .map(|(task, input)| {
            (
                TaskName::from(task),
                TaskDefinition {
                    inputs: vec![input.to_string()],
                    ..TaskDefinition::default()
                },
            )
        })
        .collect();
        let task_graph = TaskGraph::build(&graph, &pipeline).unwrap();
        repo.write_file("tsconfig.base.json", "{\"changed\": true}\n");
        repo.write_file("packages/a/src/index.ts", "export const changed = true;\n");

        let affected =
            since_affected(repo.path(), repo.path(), &base_ref, &graph, &task_graph).unwrap();
        assert_eq!(
            affected.packages(),
            &package_name_set(["@repo/a", "@repo/b"])
        );
        assert_eq!(
            affected.root_tasks_with_inputs,
            ["deploy", "check", "lint"].map(TaskName::from).into()
        );
        assert_eq!(
            affected.affected_root_tasks,
            ["deploy", "check"].map(TaskName::from).into()
        );
        assert!(!affected.root_task_passes(&TaskName::from("lint")));
    }

    #[test]
    fn invalid_root_task_input_fails_since_even_when_nothing_changed() {
        let (repo, base_ref, graph) = repo_with_root_file_workspace();
        let pipeline = HashMap::from([(
            TaskName::from("#deploy"),
            TaskDefinition {
                inputs: vec!["#".to_string()],
                ..TaskDefinition::default()
            },
        )]);
        let task_graph = TaskGraph::build(&graph, &pipeline).unwrap();

        // Clean tree, no changed paths: root tasks still expand for the
        // bypass-vs-filter classification, so the invalid pattern fails
        // loudly instead of being silently skipped.
        let error =
            since_affected(repo.path(), repo.path(), &base_ref, &graph, &task_graph).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("task '#deploy'"),
            "expected task id in error, got: {message}"
        );
        assert!(
            message.contains("root input must include path"),
            "expected pattern error in message, got: {message}"
        );
    }

    #[test]
    fn affected_packages_from_paths_accepts_absolute_paths() {
        let repo = TestRepo::new();
        repo.write_file("packages/a/package.json", &package_json("@repo/a", &[]));
        repo.write_file("packages/a/src/index.ts", "export const value = 1;\n");
        repo.write_file(
            "packages/b/package.json",
            &package_json("@repo/b", &["@repo/a"]),
        );
        repo.write_file("packages/b/src/index.ts", "export const value = 1;\n");

        let graph = package_graph(
            repo.path(),
            [("@repo/a", "packages/a"), ("@repo/b", "packages/b")],
        );
        let changed_paths = HashSet::from([repo.path().join("packages/a/src/index.ts")]);

        let affected = affected_packages_from_paths(&changed_paths, repo.path(), &graph).unwrap();

        assert_eq!(affected, package_name_set(["@repo/a", "@repo/b"]));
    }

    const BERRY_LOCKFILE: &str = r#"__metadata:
  version: 8
  cacheKey: 10

"foo@npm:^2.0.0":
  version: 2.0.1
  resolution: "foo@npm:2.0.1"
  dependencies:
    bar: "npm:^3.0.0"

"bar@npm:^3.0.0":
  version: 3.0.1
  resolution: "bar@npm:3.0.1"
  checksum: 10/bar

"left-pad@npm:^1.0.0":
  version: 1.0.0
  resolution: "left-pad@npm:1.0.0"
"#;

    fn lockfile_workspace(
        repo: &TestRepo,
        prefix: &Path,
        lockfile: impl AsRef<[u8]>,
    ) -> PackageGraph {
        for (path, contents) in [
            ("package.json", ROOT_PKG_JSON.to_owned()),
            (
                "packages/a/package.json",
                r#"{"name":"@repo/a","dependencies":{"foo":"^2.0.0"}}"#.to_owned(),
            ),
            (
                "packages/b/package.json",
                package_json("@repo/b", &["@repo/a"]),
            ),
            (
                "packages/c/package.json",
                r#"{"name":"@repo/c","dependencies":{"left-pad":"^1.0.0"}}"#.to_owned(),
            ),
        ] {
            let path = repo.path().join(prefix).join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        fs::write(repo.path().join(prefix).join("yarn.lock"), lockfile).unwrap();
        repo.git_add_and_commit_all();
        package_graph_with_root(
            &repo.path().join(prefix),
            "root",
            [
                ("root", "."),
                ("@repo/a", "packages/a"),
                ("@repo/b", "packages/b"),
                ("@repo/c", "packages/c"),
            ],
        )
    }

    fn affected_after_lockfile_change(
        base: impl AsRef<[u8]>,
        head: impl AsRef<[u8]>,
    ) -> HashSet<PackageName> {
        let repo = TestRepo::new();
        let graph = lockfile_workspace(&repo, Path::new("."), base);
        let base_ref = repo.head_commit();
        fs::write(repo.path().join("yarn.lock"), head).unwrap();

        affected_packages(repo.path(), repo.path(), &base_ref, &graph).unwrap()
    }

    #[test]
    fn lockfile_only_transitive_bump_selects_package_and_dependents() {
        let head = BERRY_LOCKFILE.replace("3.0.1", "3.0.2");
        let affected = affected_after_lockfile_change(BERRY_LOCKFILE, &head);

        assert_eq!(affected, package_name_set(["@repo/a", "@repo/b"]));
    }

    #[test]
    fn unchanged_lockfile_selects_no_packages() {
        assert!(affected_after_lockfile_change(BERRY_LOCKFILE, BERRY_LOCKFILE).is_empty());
    }

    #[test]
    fn malformed_base_selects_all_non_root_packages() {
        for head in [BERRY_LOCKFILE, "not-a-lockfile"] {
            let affected = affected_after_lockfile_change("not-a-lockfile", head);
            assert_eq!(
                affected,
                package_name_set(["@repo/a", "@repo/b", "@repo/c"])
            );
        }
    }

    #[test]
    fn malformed_head_selects_all_non_root_packages() {
        let affected = affected_after_lockfile_change(BERRY_LOCKFILE, "not-a-lockfile");

        assert_eq!(
            affected,
            package_name_set(["@repo/a", "@repo/b", "@repo/c"])
        );
    }

    #[test]
    fn missing_base_lockfile_selects_packages_with_external_dependencies() {
        let repo = TestRepo::new();
        let graph = lockfile_workspace(&repo, Path::new("."), BERRY_LOCKFILE);
        fs::remove_file(repo.path().join("yarn.lock")).unwrap();
        repo.git_add_and_commit_all();
        let base_ref = repo.head_commit();
        repo.write_file("yarn.lock", BERRY_LOCKFILE);

        let affected = affected_packages(repo.path(), repo.path(), &base_ref, &graph).unwrap();
        assert_eq!(
            affected,
            package_name_set(["@repo/a", "@repo/b", "@repo/c"])
        );
    }

    #[test]
    fn non_utf8_base_lockfile_selects_all_non_root_packages() {
        let affected = affected_after_lockfile_change([0xff, 0xfe], BERRY_LOCKFILE);
        assert_eq!(
            affected,
            package_name_set(["@repo/a", "@repo/b", "@repo/c"])
        );
    }

    #[test]
    fn unreadable_head_lockfile_selects_all_non_root_packages() {
        let repo = TestRepo::new();
        let graph = lockfile_workspace(&repo, Path::new("."), BERRY_LOCKFILE);
        let base_ref = repo.head_commit();
        fs::remove_file(repo.path().join("yarn.lock")).unwrap();
        fs::create_dir(repo.path().join("yarn.lock")).unwrap();

        let affected = affected_packages(repo.path(), repo.path(), &base_ref, &graph).unwrap();
        assert_eq!(
            affected,
            package_name_set(["@repo/a", "@repo/b", "@repo/c"])
        );
    }

    #[test]
    fn relative_nested_workspace_uses_its_lockfile_not_repo_root_decoy() {
        luchta_test_support::require_nextest();
        let repo = TestRepo::new();
        let head = BERRY_LOCKFILE.replace("3.0.1", "3.0.2");
        repo.write_file("yarn.lock", &head);
        let graph = lockfile_workspace(&repo, Path::new("nested"), BERRY_LOCKFILE);
        let base_ref = repo.head_commit();
        repo.write_file("nested/yarn.lock", &head);

        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(repo.path()).unwrap();
        let result = affected_packages(
            Path::new("nested/../nested"),
            Path::new("."),
            &base_ref,
            &graph,
        );
        std::env::set_current_dir(original_dir).unwrap();

        assert_eq!(result.unwrap(), package_name_set(["@repo/a", "@repo/b"]));
    }

    #[test]
    fn workspace_outside_repo_does_not_fall_back_to_repo_root_lockfile() {
        let repo = TestRepo::new();
        let other_workspace = TempDir::new().unwrap();
        let result = super::repo_relative_lockfile_path(other_workspace.path(), repo.path());

        assert!(matches!(result, Err(SinceError::LockfilePath(_))));
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

        fn with_status_changes() -> Self {
            let repo = Self::new();
            for path in [
                "modified",
                "deleted",
                "staged-deleted",
                "rename-old",
                "clean",
            ] {
                repo.write_file(path, "base\n");
            }
            repo.write_file(".gitignore", "ignored/\n");
            repo.git_add_and_commit_all();
            repo.write_file("modified", "changed\n");
            fs::remove_file(repo.path().join("deleted")).unwrap();
            git(repo.path(), ["rm", "staged-deleted"]);
            git(repo.path(), ["mv", "rename-old", "rename-new"]);
            repo.write_file("staged", "staged\n");
            git(repo.path(), ["add", "staged"]);
            repo.write_file("untracked", "new\n");
            repo.write_file("nested/deep/one", "new\n");
            repo.write_file("nested/deep/two", "new\n");
            repo.write_file("ignored/file", "ignored\n");
            repo
        }

        fn add_gitlink(&self) {
            git(
                self.path(),
                [
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("160000,{},sub", self.head_commit()),
                ],
            );
            fs::create_dir(self.path().join("sub")).unwrap();
        }

        fn assert_status_changes(&self, extra: &[&str]) {
            let expected = [
                "modified",
                "deleted",
                "staged-deleted",
                "rename-old",
                "rename-new",
                "staged",
                "untracked",
                "nested/deep/one",
                "nested/deep/two",
            ]
            .into_iter()
            .chain(extra.iter().copied())
            .map(Into::into)
            .collect();
            assert_eq!(changed_paths_since(self.path(), "HEAD").unwrap(), expected);
        }

        #[cfg(unix)]
        fn unreadable_gitmodules(&self) -> Option<RestorePermissions> {
            use std::os::unix::fs::PermissionsExt;

            self.write_file(".gitmodules", "");
            let path = self.path().join(".gitmodules");
            let permissions = fs::metadata(&path).unwrap().permissions();
            let restore = RestorePermissions(path.clone(), permissions);
            fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
            if fs::read(&path).is_ok() {
                eprintln!("skipping: chmod 000 does not prevent reading .gitmodules");
                return None;
            }
            Some(restore)
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

    #[cfg(unix)]
    struct RestorePermissions(std::path::PathBuf, fs::Permissions);

    #[cfg(unix)]
    impl Drop for RestorePermissions {
        fn drop(&mut self) {
            // Ignore failures so a panic during unwind can't mask the real assertion.
            let _ = fs::set_permissions(&self.0, self.1.clone());
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
