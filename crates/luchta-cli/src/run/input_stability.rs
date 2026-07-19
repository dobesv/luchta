//! Input-stability checking for the cache-write path.
//!
//! Detects the concurrent-change race described in issue #157: a user (or
//! external process) edits a task's input file *after* the task has read it but
//! *before* the run finishes. Recording the post-run hash of such an input would
//! bake an input state the task never actually consumed into the cache metadata,
//! which could later cause a wrongly-skipped rebuild or a stale shared-cache
//! restore.
//!
//! The guarantee is enforced by capturing a pre-execution snapshot of resolved
//! task inputs and re-checking them after the run (see
//! [`check_input_stability`]). Resolve-time worker input overrides are already
//! applied before this snapshot, so every post-run input must match that
//! baseline strictly.
//!
//! This module also owns the input/output resolution helpers used by the
//! cache-write path ([`resolve_cache_inputs`], [`resolve_cache_outputs`]).

use super::*;
use luchta_cache::files_diff;

/// Borrowed inputs for resolving a task's pre-execution snapshot.
///
/// Encapsulates the resolution context so the diagnostics can name the task and
/// its input provenance without exposing the internal `//root` package sentinel.
pub(crate) struct PreExecutionSnapshotRequest<'a> {
    pub input_patterns: &'a [String],
    pub source_pkg: &'a PackageName,
    pub package_graph: &'a PackageGraph,
    pub repo_root: &'a Path,
    pub task_id: &'a TaskId,
    pub inputs_from_worker: bool,
}

/// Resolve a pre-execution input snapshot for stability checking.
///
/// Captures the declared input patterns (with content hashes) BEFORE task
/// execution so a concurrent edit during the run can be detected afterwards.
pub(crate) fn resolve_pre_execution_inputs(
    request: PreExecutionSnapshotRequest<'_>,
) -> Vec<FileEntry> {
    let PreExecutionSnapshotRequest {
        input_patterns,
        source_pkg,
        package_graph,
        repo_root,
        task_id,
        inputs_from_worker,
    } = request;

    // Identify the task by its user-facing `task_id` (root tasks render as
    // `#task`, never the internal `//root` sentinel) and state where its inputs
    // originated, matching the fatal cache-write path. We deliberately do NOT
    // print `source_pkg` — that would leak the `//root` sentinel for root tasks.
    let origin = input_origin_clause(inputs_from_worker);
    let requests = match expand_input_patterns(input_patterns, source_pkg, package_graph, repo_root)
    {
        Ok(reqs) => reqs,
        Err(error) => {
            // A pre-run resolution failure yields an empty baseline. Log it so it
            // is not silently mistaken for "task has no inputs": if the post-run
            // resolution then succeeds, the diff would otherwise look like inputs
            // appeared mid-run and spuriously skip the cache write.
            eprintln!(
                "warning: failed to expand input patterns for pre-execution snapshot of task '{task_id}' ({origin}): {error} — skipping concurrent-change detection for this run"
            );
            return Vec::new();
        }
    };

    match resolve_inputs_with_semantics(&requests) {
        Ok(entries) => entries,
        Err(error) => {
            eprintln!(
                "warning: failed to resolve inputs for pre-execution snapshot of task '{task_id}' ({origin}): {error} — skipping concurrent-change detection for this run"
            );
            Vec::new()
        }
    }
}

/// Check input stability across task execution and build record's input list.
///
/// Every post-run input must match pre-execution snapshot exactly. Any delta —
/// changed hash, deleted file, or file that appears mid-run and matches task
/// inputs — is concurrent change and returns `Err` so caller skips cache write.
///
/// On stability, returns authoritative PRE-SNAPSHOT entries so record preserves
/// input state task actually consumed.
pub(crate) fn check_input_stability(
    pre_snapshot: &[FileEntry],
    post_inputs: &[FileEntry],
    task_id: &TaskId,
) -> Result<Vec<FileEntry>, String> {
    let (deltas, _, change_count) = files_diff(pre_snapshot, post_inputs, 10);

    if change_count != 0 {
        let delta_summary: Vec<String> = deltas
            .iter()
            .take(5)
            .map(|d| {
                if d.prior_absent && !d.current_absent {
                    format!("{} (new file)", d.path)
                } else if !d.prior_absent && d.current_absent {
                    format!("{} (deleted)", d.path)
                } else {
                    format!("{} (content changed)", d.path)
                }
            })
            .collect();

        let suffix = if change_count > 5 {
            format!(" and {} more", change_count - 5)
        } else {
            String::new()
        };

        return Err(format!(
            "task '{}' declared inputs changed during execution: {}{} — skipping cache write",
            task_id,
            delta_summary.join(", "),
            suffix
        ));
    }

    Ok(pre_snapshot.to_vec())
}

/// Result of resolving a task's inputs for the cache-write path.
/// Distinguishes expansion errors (fatal) from IO errors (warn + skip).
pub(crate) enum CacheInputResult {
    Ok(Vec<FileEntry>),
    ExpansionError(String),
    IoError,
}

/// Human-readable clause describing where a task's input patterns originated.
pub(crate) fn input_origin_clause(inputs_from_worker: bool) -> &'static str {
    if inputs_from_worker {
        "returned by the worker"
    } else {
        "declared in the task spec"
    }
}

/// Resolve the (post-execution) input entries for a task's cache record.
pub(crate) fn resolve_cache_inputs(
    cache_ctx: &CacheWriteContext,
    input_patterns: &[String],
) -> CacheInputResult {
    let requests = match expand_input_patterns(
        input_patterns,
        &cache_ctx.source_pkg,
        &cache_ctx.package_graph,
        &cache_ctx.repo_root,
    ) {
        Ok(reqs) => reqs,
        Err(error) => {
            // NOTE: the offending package is already conveyed by `task_id`
            // (root tasks render as `#task`, never the internal `//root`
            // sentinel) and by the wrapped `error`'s own Display, so we do not
            // print `source_pkg` separately — that would both duplicate the
            // package and leak the `//root` sentinel for root tasks.
            return CacheInputResult::ExpansionError(format!(
                "input \"{}\" for task \"{}\" ({}): {}",
                error.pattern(),
                cache_ctx.task_id,
                input_origin_clause(cache_ctx.inputs_from_worker),
                error
            ));
        }
    };

    match resolve_inputs_with_semantics(&requests) {
        Ok(inputs) => CacheInputResult::Ok(inputs),
        Err(error) => {
            eprintln!(
                "warning: failed to resolve cache inputs for task '{}': {error} — recording run with empty inputs",
                cache_ctx.task_id
            );
            CacheInputResult::IoError
        }
    }
}

/// Resolve the produced output entries for a task's cache record.
pub(crate) fn resolve_cache_outputs(
    cache_ctx: &CacheWriteContext,
    output_patterns: &[String],
) -> Option<Vec<FileEntry>> {
    match resolve_outputs(&cache_ctx.package_path, output_patterns) {
        Ok(outputs) => Some(outputs),
        Err(error) => {
            eprintln!(
                "warning: failed to resolve cache outputs for task '{}': {error} — recording run with empty outputs",
                cache_ctx.task_id
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeMap, sync::Arc};

    use crate::{
        run::{CacheDecisionContext, CacheWriteContext},
        watch::registry::empty_task_watch_registry,
    };
    use luchta_cache::{Decision, RunReason};
    use luchta_workspace::PackageNode;

    fn sample_cache_write_context(task_id: TaskId) -> CacheWriteContext {
        let root = tempfile::tempdir().expect("tempdir").keep();
        let package_path = root.join("packages/pkg");
        std::fs::create_dir_all(&package_path).expect("create package path");
        std::fs::write(
            package_path.join("package.json"),
            r#"{"name":"pkg","version":"1.0.0"}"#,
        )
        .expect("write package manifest");
        let package_graph = Arc::new(
            PackageGraph::build(vec![PackageNode::new(
                PackageName::from("pkg"),
                &package_path,
            )])
            .expect("build package graph"),
        );

        CacheWriteContext {
            task_id,
            task_def: TaskDefinition::default(),
            inputs_from_worker: false,
            package_path,
            dep_outputs: BTreeMap::new(),
            task_spec_hash: [1; 32],
            env_hash: [2; 32],
            pkg_dep_hash: [3; 32],
            start_unix_ms: 10,
            repo_root: root,
            source_pkg: PackageName::from("pkg"),
            package_graph,
            cache_nonce: None,
            decision: CacheDecisionContext {
                action: Decision::Run,
                run_reason: RunReason::NoPriorRecord,
            },
            task_watch_registry: empty_task_watch_registry(),
            pre_snapshot: Some(Vec::new()),
        }
    }

    #[test]
    fn check_input_stability_detects_change() {
        let pre = vec![FileEntry {
            path: "src/a.txt".to_string(),
            hash: [1; 32],
            size: 10,
            mtime_ns: 1000,
            absent: false,
        }];
        let post = vec![FileEntry {
            path: "src/a.txt".to_string(),
            hash: [2; 32], // Changed hash
            size: 10,
            mtime_ns: 1000,
            absent: false,
        }];
        let task_id = TaskId::new("pkg", "build");
        let result = check_input_stability(&pre, &post, &task_id);
        assert!(result.is_err(), "should detect changed content");
    }

    #[test]
    fn check_input_stability_accepts_unchanged() {
        let entry = FileEntry {
            path: "src/a.txt".to_string(),
            hash: [1; 32],
            size: 10,
            mtime_ns: 1000,
            absent: false,
        };
        let pre = vec![entry.clone()];
        let post = vec![entry];
        let task_id = TaskId::new("pkg", "build");
        let result = check_input_stability(&pre, &post, &task_id);
        assert!(result.is_ok(), "should accept unchanged content");
    }

    #[test]
    fn check_input_stability_detects_deleted_file() {
        let pre = vec![FileEntry {
            path: "src/old.txt".to_string(),
            hash: [1; 32],
            size: 10,
            mtime_ns: 1000,
            absent: false,
        }];
        let post: Vec<FileEntry> = vec![];
        let task_id = TaskId::new("pkg", "build");
        let result = check_input_stability(&pre, &post, &task_id);
        assert!(result.is_err(), "should detect deleted file");
        let err = result.unwrap_err();
        assert!(
            err.contains("deleted"),
            "error message should mention deleted"
        );
    }

    #[test]
    fn check_input_stability_uses_pre_snapshot_on_match() {
        // Pre-snapshot has the authoritative state
        let pre = vec![FileEntry {
            path: "src/a.txt".to_string(),
            hash: [1; 32],
            size: 10,
            mtime_ns: 1000,
            absent: false,
        }];
        // Post has same content hash but different metadata (mtime)
        let post = vec![FileEntry {
            path: "src/a.txt".to_string(),
            hash: [1; 32],
            size: 20,       // Different size
            mtime_ns: 2000, // Different mtime
            absent: false,
        }];
        let task_id = TaskId::new("pkg", "build");
        let result = check_input_stability(&pre, &post, &task_id);
        assert!(result.is_ok(), "same content hash should be stable");
        // Result should be the pre-snapshot (authoritative)
        let stable = result.unwrap();
        assert_eq!(stable[0].mtime_ns, 1000, "should use pre-snapshot metadata");
        assert_eq!(stable[0].size, 10, "should use pre-snapshot size");
    }

    #[test]
    fn check_input_stability_new_declared_match_file_is_a_mismatch() {
        // A run where a file matching task inputs
        // declared glob appeared mid-run: it is present post-run but absent from
        // the pre-execution snapshot. Because the whole post-run set is declared,
        // this new file is a concurrent change and MUST trigger a stability
        // mismatch — it must NOT be silently recorded as a worker-detected input.
        let pre = vec![FileEntry {
            path: "src/a.txt".to_string(),
            hash: [1; 32],
            size: 10,
            mtime_ns: 1000,
            absent: false,
        }];
        let post = vec![
            FileEntry {
                path: "src/a.txt".to_string(),
                hash: [1; 32],
                size: 10,
                mtime_ns: 1000,
                absent: false,
            },
            // Appeared mid-run, matches the declared glob.
            FileEntry {
                path: "src/b.txt".to_string(),
                hash: [7; 32],
                size: 3,
                mtime_ns: 2000,
                absent: false,
            },
        ];
        let task_id = TaskId::new("pkg", "build");
        let result = check_input_stability(&pre, &post, &task_id);
        assert!(
            result.is_err(),
            "a new declared-match file appearing mid-run must be a stability mismatch"
        );
        assert!(
            result.unwrap_err().contains("new file"),
            "mismatch reason should mention the new file"
        );
    }

    fn assert_expansion_error_origin(inputs_from_worker: bool, origin: &str) {
        let mut cache_ctx = sample_cache_write_context(TaskId::new("pkg", "build"));
        cache_ctx.inputs_from_worker = inputs_from_worker;

        let result = resolve_cache_inputs(&cache_ctx, &["/".to_owned()]);
        let CacheInputResult::ExpansionError(message) = result else {
            panic!("expected expansion error");
        };

        assert!(message.contains(&format!("input \"/\" for task \"pkg#build\" ({origin})")));
        assert!(message.contains("resolved path escapes base directory"));
    }

    #[test]
    fn resolve_cache_inputs_expansion_error_names_task_and_task_spec_origin() {
        assert_expansion_error_origin(false, "declared in the task spec");
    }

    #[test]
    fn resolve_cache_inputs_expansion_error_names_worker_origin() {
        assert_expansion_error_origin(true, "returned by the worker");
    }

    #[test]
    fn resolve_cache_inputs_expansion_error_never_leaks_root_sentinel() {
        // Regression for issue #83 review: a root-task input escape must render
        // the task as `#task` and MUST NOT expose the internal `//root`
        // package sentinel to the user.
        let mut cache_ctx =
            sample_cache_write_context(TaskId::new(luchta_types::ROOT_PACKAGE_NAME, "build"));
        cache_ctx.source_pkg = PackageName::from(luchta_types::ROOT_PACKAGE_NAME);

        let result = resolve_cache_inputs(&cache_ctx, &["/".to_owned()]);
        let CacheInputResult::ExpansionError(message) = result else {
            panic!("expected expansion error");
        };

        assert!(
            !message.contains(luchta_types::ROOT_PACKAGE_NAME),
            "root sentinel must never appear in user-facing message: {message}"
        );
        assert!(
            message.contains("for task \"#build\""),
            "root task should render as #build: {message}"
        );
        assert!(
            message.contains("the workspace root"),
            "root package should render with the friendly label: {message}"
        );
    }
}
