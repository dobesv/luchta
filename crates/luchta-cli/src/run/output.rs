//! Console output emitted while a run may have an interactive status line.

use std::sync::Arc;

use luchta_cache::shared::RestoredHit;
use luchta_cache::{Cache, ReportInput, RunArtifacts, RunReason, SCHEMA_VERSION_V5};
use luchta_types::TaskId;

use crate::progress::{ProgressOutput, ProgressReporter};

/// Hydrate local cache from a shared-cache hit.
///
/// Writes the restored record and logs so the next build in the same
/// worktree gets a normal local skip with correct downstream invalidation.
pub(super) fn hydrate_local_cache(
    cache: Arc<Cache>,
    task_id: TaskId,
    hit: &RestoredHit,
    output: &ProgressOutput,
) {
    let cache_key = task_id.to_string();
    let mut record = hit.record.clone();
    record.schema_version = SCHEMA_VERSION_V5;
    record.run_reason = Some(RunReason::SharedCacheHit);
    let reports: Vec<ReportInput> = hit
        .record
        .reports
        .iter()
        .filter_map(|report| {
            hit.reports
                .iter()
                .find(|stored| stored.filename == report.filename)
                .map(|stored| ReportInput {
                    filename: report.filename.clone(),
                    mime_type: report.mime_type.clone(),
                    content: stored.content.clone(),
                })
        })
        .collect();
    if let Err(error) = cache.write(
        &cache_key,
        RunArtifacts {
            record: &record,
            stdout: &hit.stdout,
            stderr: &hit.stderr,
            reports: &reports,
        },
    ) {
        output.stderr_line(&format!(
            "warning: failed to hydrate local cache for task '{task_id}': {error}"
        ));
    }
}

/// Replay restored logs to the progress reporter.
///
/// This mirrors how the normal run path emits logs so output appears
/// as if the task actually ran.
pub(super) fn replay_logs(hit: &RestoredHit, reporter: &Arc<ProgressReporter>) {
    let output = reporter.output();
    replay_utf8_lines(&hit.stdout, |line| output.stdout_line(line));
    replay_utf8_lines(&hit.stderr, |line| output.stderr_line(line));
}

fn replay_utf8_lines(bytes: &[u8], mut write_line: impl FnMut(&str)) {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return;
    };
    for line in text.lines() {
        write_line(line);
    }
}
