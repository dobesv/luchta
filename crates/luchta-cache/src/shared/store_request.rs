use std::path::{Path, PathBuf};

use crate::record::TaskRunRecord;

/// Borrowed data needed to persist one shared-cache entry.
///
/// Keeping these related values together prevents the explicit monotonic
/// duration API from growing another long positional parameter list.
#[derive(Clone, Copy)]
pub struct SharedCacheStoreRequest<'a> {
    pub task_id: &'a str,
    pub input_key: &'a [u8; 32],
    pub outputs_hash: &'a [u8; 32],
    pub package_dir: &'a Path,
    pub rel_output_paths: &'a [PathBuf],
    pub record: &'a TaskRunRecord,
    pub stdout: &'a [u8],
    pub stderr: &'a [u8],
    pub reports: &'a [crate::store::ReportInput],
    pub repo_root: &'a Path,
}

#[derive(Clone, Copy)]
pub(super) struct StoreTiming {
    pub(super) duration_ms: u64,
    pub(super) trusted: bool,
}
