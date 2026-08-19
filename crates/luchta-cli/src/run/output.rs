//! Console output emitted while a run may have an interactive status line.

use std::sync::Arc;

use luchta_cache::shared::RestoredHit;
use luchta_cache::{Cache, ReportInput, RunArtifacts, RunReason, SCHEMA_VERSION_V5};
use luchta_types::TaskId;

use crate::progress::ProgressOutput;

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
