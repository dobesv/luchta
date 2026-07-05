//! Input-stability checking for the cache-write path.
//!
//! Detects the concurrent-change race described in issue #157: a user (or
//! external process) edits a task's input file *after* the task has read it but
//! *before* the run finishes. Recording the post-run hash of such an input would
//! bake an input state the task never actually consumed into the cache metadata,
//! which could later cause a wrongly-skipped rebuild or a stale shared-cache
//! restore.
//!
//! The guarantee is enforced by capturing a pre-execution snapshot of the
//! *declared* inputs and re-checking them after the run (see
//! [`check_input_stability`]). Worker-detected inputs — files a worker discovers
//! only during the run — have no pre-execution baseline and are handled
//! best-effort.
//!
//! This module also owns the input/output resolution helpers used by the
//! cache-write path ([`resolve_cache_inputs`], [`resolve_cache_outputs`]).

use super::*;
use luchta_cache::files_diff;

/// Resolve a pre-execution input snapshot for stability checking.
///
/// Captures the declared input patterns (with content hashes) BEFORE task
/// execution so a concurrent edit during the run can be detected afterwards.
pub(crate) fn resolve_pre_execution_inputs(
    input_patterns: &[String],
    source_pkg: &PackageName,
    package_graph: &PackageGraph,
    repo_root: &Path,
) -> Vec<FileEntry> {
    let requests = match expand_input_patterns(input_patterns, source_pkg, package_graph, repo_root)
    {
        Ok(reqs) => reqs,
        Err(_) => return Vec::new(),
    };

    resolve_inputs_with_semantics(&requests).unwrap_or_default()
}

/// Check input stability across task execution and build the record's input list.
///
/// `uses_worker_detected_patterns` indicates whether the post-run resolution
/// (`post_inputs`) was produced from patterns the WORKER reported during the run
/// (rather than the task's statically declared `inputs`). This determines which
/// files fall under the strict concurrent-change guarantee:
///
/// - When `uses_worker_detected_patterns` is `false`, the whole run resolved the DECLARED
///   input patterns, so every entry in `post_inputs` is a declared input. The
///   full `post_inputs` set is compared against `pre_snapshot` by CONTENT HASH
///   (mtime/size are unsafe due to granularity). ANY delta — a changed hash, a
///   deleted file, OR a file that appeared mid-run and now matches a declared
///   pattern (present in `post_inputs` but absent from `pre_snapshot`) — is a
///   concurrent change and returns `Err` (the caller skips the cache write).
///
/// - When `uses_worker_detected_patterns` is `true`, only the files that were ALSO present in
///   `pre_snapshot` (the declared pre-execution baseline) get the strict check;
///   the remaining post-run files are worker-detected inputs with no
///   pre-execution baseline and are recorded best-effort with their post-run
///   hash. (A change to a detected input between runs is still caught by the
///   normal cache decision on the next run.)
///
/// On stability, returns `Ok` with the record's input list: the verified-stable
/// PRE-SNAPSHOT entries for declared files (authoritative — preserves the H1
/// state the task actually consumed and closes the post-resolve->write gap),
/// followed by the post-run entries for any worker-detected files.
pub(crate) fn check_input_stability(
    pre_snapshot: &[FileEntry],
    post_inputs: &[FileEntry],
    uses_worker_detected_patterns: bool,
    task_id: &TaskId,
) -> Result<Vec<FileEntry>, String> {
    use std::collections::BTreeSet;

    // Partition the post-run resolution into the files subject to the strict
    // stability check (`post_declared`) and the best-effort worker-detected files
    // (`post_detected`).
    //
    // When the run resolved the DECLARED patterns (`!uses_worker_detected_patterns`), the entire
    // post-run set is declared — including any file that appeared mid-run and now
    // matches a declared pattern. Such a new file is NOT in the pre-snapshot, so
    // comparing the full `post_inputs` against `pre_snapshot` flags it as a
    // concurrent change (a "new file"), which is the desired strict behavior.
    //
    // When the worker reported the patterns (`uses_worker_detected_patterns`), only files that
    // were already in the pre-snapshot have a pre-execution baseline; the rest are
    // best-effort.
    let (post_declared, post_detected): (Vec<FileEntry>, Vec<FileEntry>) =
        if uses_worker_detected_patterns {
            let declared_paths: BTreeSet<&str> =
                pre_snapshot.iter().map(|e| e.path.as_str()).collect();
            post_inputs
                .iter()
                .cloned()
                .partition(|e| declared_paths.contains(e.path.as_str()))
        } else {
            (post_inputs.to_vec(), Vec::new())
        };

    // Strict content-hash comparison of declared inputs.
    let (deltas, _, change_count) = files_diff(pre_snapshot, &post_declared, 10);

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

    // Declared inputs stable: use the pre-snapshot as authoritative, then append
    // the post-run worker-detected entries.
    let mut record_inputs = pre_snapshot.to_vec();
    record_inputs.extend(post_detected);
    Ok(record_inputs)
}

/// Result of resolving a task's inputs for the cache-write path.
/// Distinguishes expansion errors (fatal) from IO errors (warn + skip).
pub(crate) enum CacheInputResult {
    Ok(Vec<FileEntry>),
    ExpansionError(String),
    IoError,
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
            return CacheInputResult::ExpansionError(format!(
                "input \"{}\" in package \"{}\": {}",
                error.pattern(),
                cache_ctx.source_pkg,
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
        let result = check_input_stability(&pre, &post, false, &task_id);
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
        let result = check_input_stability(&pre, &post, false, &task_id);
        assert!(result.is_ok(), "should accept unchanged content");
    }

    #[test]
    fn check_input_stability_records_new_worker_detected_file() {
        // A file present post-run but absent from the pre-execution snapshot is a
        // worker-detected input. It has no pre-execution baseline, so the strict
        // check does NOT apply to it: the run stays cacheable and the file is
        // recorded best-effort with its post-run hash.
        let pre: Vec<FileEntry> = vec![];
        let post = vec![FileEntry {
            path: "src/new.txt".to_string(),
            hash: [1; 32],
            size: 10,
            mtime_ns: 1000,
            absent: false,
        }];
        let task_id = TaskId::new("pkg", "build");
        let result = check_input_stability(&pre, &post, true, &task_id);
        assert!(
            result.is_ok(),
            "new worker-detected file should not be a stability mismatch"
        );
        let recorded = result.unwrap();
        assert_eq!(recorded.len(), 1, "worker-detected file should be recorded");
        assert_eq!(recorded[0].path, "src/new.txt");
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
        let result = check_input_stability(&pre, &post, false, &task_id);
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
        let result = check_input_stability(&pre, &post, false, &task_id);
        assert!(result.is_ok(), "same content hash should be stable");
        // Result should be the pre-snapshot (authoritative)
        let stable = result.unwrap();
        assert_eq!(stable[0].mtime_ns, 1000, "should use pre-snapshot metadata");
        assert_eq!(stable[0].size, 10, "should use pre-snapshot size");
    }

    #[test]
    fn check_input_stability_new_detected_input_kept_with_stable_declared() {
        // A worker discovers a NEW input file (`detected.txt`) not present in the
        // pre-execution snapshot, while the declared input (`declared.txt`) stays
        // stable across the run. The strict concurrent-change guarantee only
        // covers DECLARED inputs, so this is NOT a stability mismatch: the run
        // stays cacheable. The record keeps the verified-stable declared entry
        // (from the pre-snapshot) and the post-run detected entry (best-effort).
        let declared = FileEntry {
            path: "declared.txt".to_string(),
            hash: [1; 32],
            size: 4,
            mtime_ns: 1000,
            absent: false,
        };
        let pre = vec![declared.clone()];

        let post = vec![
            // Declared input, unchanged content (post-run metadata differs, which
            // is fine — the pre-snapshot version is used in the record).
            FileEntry {
                path: "declared.txt".to_string(),
                hash: [1; 32],
                size: 4,
                mtime_ns: 2000,
                absent: false,
            },
            // New worker-detected input, absent from the pre-snapshot.
            FileEntry {
                path: "detected.txt".to_string(),
                hash: [9; 32],
                size: 6,
                mtime_ns: 3000,
                absent: false,
            },
        ];

        let task_id = TaskId::new("pkg", "build");
        let result = check_input_stability(&pre, &post, true, &task_id)
            .expect("stable declared input + new detected input should be Ok");

        // Declared entry is recorded from the pre-snapshot (authoritative).
        let declared_entry = result
            .iter()
            .find(|e| e.path == "declared.txt")
            .expect("declared input recorded");
        assert_eq!(
            declared_entry.mtime_ns, 1000,
            "declared input should use pre-snapshot metadata"
        );

        // Worker-detected entry is recorded best-effort from the post-run hash.
        let detected_entry = result
            .iter()
            .find(|e| e.path == "detected.txt")
            .expect("worker-detected input recorded");
        assert_eq!(
            detected_entry.hash, [9; 32],
            "detected input uses post-run hash"
        );
    }

    #[test]
    fn check_input_stability_new_declared_match_file_is_a_mismatch() {
        // A DECLARED-pattern run (uses_worker_detected_patterns = false) where a file matching a
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
        let result = check_input_stability(&pre, &post, false, &task_id);
        assert!(
            result.is_err(),
            "a new declared-match file appearing mid-run must be a stability mismatch"
        );
        assert!(
            result.unwrap_err().contains("new file"),
            "mismatch reason should mention the new file"
        );
    }
}
