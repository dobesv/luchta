//! Per-entry meta objects for the shared cache.
//!
//! Meta (the run record, captured stdout/stderr, and reports) is keyed by
//! `input_key`, not by `outputs_hash`. Bundling it into the outputs blob made
//! every task with no outputs collide on a single object, because
//! `combined_outputs_hash(&[])` is a constant. See issue #278.

use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::atomicio::atomic_write;
use super::SharedCachePaths;
use crate::serialization::bincode_config;
use crate::store::ReportInput;

/// Schema version for the on-disk entry meta object.
pub const ENTRY_META_SCHEMA_VERSION: u32 = 1;
/// Maximum compressed entry metadata size embedded directly in a schema-v3
/// snapshot. Larger entries retain the fallback `entries/` object.
pub const INLINE_ENTRY_META_MAX_BYTES: usize = 16 * 1024;
/// Maximum resident size of metadata retained inline in the merged snapshot
/// index. This guards highly-compressible metadata from expanding into a large
/// long-lived allocation while preserving the 16 KiB compressed wire cutoff.
pub(crate) const INLINE_ENTRY_META_MAX_RESIDENT_BYTES: usize = 1024 * 1024;

const ENTRY_META_ZSTD_LEVEL: i32 = 3;
const ENTRY_META_FILE_EXTENSION: &str = "bin";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryReport {
    pub filename: String,
    pub mime_type: String,
    pub content: String,
}

/// Everything about a cached run except the output files themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryMeta {
    pub schema_version: u32,
    /// Points at `blobs/<outputs_hash>.tar.zst`. All-zero-length output sets
    /// share one hash, which is why meta cannot live inside that blob.
    pub outputs_hash: [u8; 32],
    /// Whether an outputs blob was actually written for this entry.
    ///
    /// Authoritative bit set by `store` from `write_outputs_blob`'s result.
    /// Do not re-derive this from `record.outputs.is_empty()`: a task can
    /// declare an output path and still produce nothing (all `FileEntry`s
    /// `absent: true`), in which case `record.outputs` is non-empty but no
    /// blob was ever written. Restore must not stat/pull a blob that will
    /// never exist.
    pub has_outputs: bool,
    /// Bincode-encoded `TaskRunRecord`.
    pub record: Vec<u8>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub reports: Vec<EntryReport>,
}

/// Approximate heap and inline storage retained by an [`EntryMeta`].
///
/// The calculation includes every variable-length payload and the fixed
/// report descriptors. Allocator bookkeeping is deliberately excluded; the
/// result is used as a conservative content budget, not a memory profiler.
#[must_use]
pub(crate) fn entry_meta_resident_bytes(meta: &EntryMeta) -> usize {
    meta.record
        .len()
        .saturating_add(meta.stdout.len())
        .saturating_add(meta.stderr.len())
        .saturating_add(std::mem::size_of_val(meta))
        .saturating_add(
            meta.reports
                .len()
                .saturating_mul(std::mem::size_of::<EntryReport>()),
        )
        .saturating_add(meta.reports.iter().fold(0_usize, |total, report| {
            total
                .saturating_add(report.filename.len())
                .saturating_add(report.mime_type.len())
                .saturating_add(report.content.len())
        }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryMetaWriteResult {
    Written,
    AlreadyExists,
}

impl From<&ReportInput> for EntryReport {
    fn from(value: &ReportInput) -> Self {
        Self {
            filename: value.filename.clone(),
            mime_type: value.mime_type.clone(),
            content: value.content.clone(),
        }
    }
}

impl From<EntryReport> for ReportInput {
    fn from(value: EntryReport) -> Self {
        Self {
            filename: value.filename,
            mime_type: value.mime_type,
            content: value.content,
        }
    }
}

#[must_use]
pub fn entry_meta_path(paths: &SharedCachePaths, input_key: &[u8; 32]) -> PathBuf {
    paths.entries_dir.join(format!(
        "{}.{ENTRY_META_FILE_EXTENSION}",
        blake3::Hash::from(*input_key).to_hex()
    ))
}

/// Encode meta to its on-disk representation: bincode, then zstd.
pub fn encode_entry_meta(meta: &EntryMeta) -> io::Result<Vec<u8>> {
    let raw = bincode::serde::encode_to_vec(meta, bincode_config()).map_err(io::Error::other)?;
    zstd::encode_all(raw.as_slice(), ENTRY_META_ZSTD_LEVEL)
}

/// Write meta for `input_key`, keeping any existing object.
///
/// First writer wins, matching `SnapshotStore::merge_entry`'s idempotent-noop
/// semantics. Re-running the same task produces a record that differs only in
/// timings, so rewriting would churn the remote for no gain.
pub fn write_entry_meta(
    paths: &SharedCachePaths,
    input_key: &[u8; 32],
    meta: &EntryMeta,
) -> io::Result<EntryMetaWriteResult> {
    let path = entry_meta_path(paths, input_key);
    if path.exists() {
        // TOCTOU is tolerated: concurrent writers for the same input_key are idempotent
        // because identical input_key means equivalent content anyway.
        return Ok(EntryMetaWriteResult::AlreadyExists);
    }
    let encoded = encode_entry_meta(meta)?;
    atomic_write(&path, &encoded).map_err(io::Error::other)?;
    Ok(EntryMetaWriteResult::Written)
}

/// Read meta for `input_key`. Returns `None` when absent, unreadable, corrupt,
/// or written by a future schema version — all of which degrade to a cache miss.
pub fn read_entry_meta(paths: &SharedCachePaths, input_key: &[u8; 32]) -> Option<EntryMeta> {
    let bytes = fs::read(entry_meta_path(paths, input_key)).ok()?;
    let raw = zstd::decode_all(bytes.as_slice()).ok()?;
    let (meta, _) =
        bincode::serde::decode_from_slice::<EntryMeta, _>(&raw, bincode_config()).ok()?;
    if meta.schema_version != ENTRY_META_SCHEMA_VERSION {
        return None;
    }
    Some(meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::paths::open_shared_paths;
    use tempfile::TempDir;

    fn sample_meta() -> EntryMeta {
        EntryMeta {
            schema_version: ENTRY_META_SCHEMA_VERSION,
            outputs_hash: [7; 32],
            has_outputs: true,
            record: vec![1, 2, 3, 4],
            stdout: b"out".to_vec(),
            stderr: b"err".to_vec(),
            reports: vec![EntryReport {
                filename: "lint.json".to_string(),
                mime_type: "application/json".to_string(),
                content: "{}".to_string(),
            }],
        }
    }

    #[test]
    fn write_then_read_round_trips() {
        let temp = TempDir::new().unwrap();
        let paths = open_shared_paths(temp.path()).unwrap();
        let input_key = [3; 32];

        let result = write_entry_meta(&paths, &input_key, &sample_meta()).unwrap();
        assert_eq!(result, EntryMetaWriteResult::Written);

        let read_back = read_entry_meta(&paths, &input_key).expect("meta should be readable");
        assert_eq!(read_back, sample_meta());
    }

    #[test]
    fn read_returns_none_when_absent() {
        let temp = TempDir::new().unwrap();
        let paths = open_shared_paths(temp.path()).unwrap();
        assert!(read_entry_meta(&paths, &[9; 32]).is_none());
    }

    #[test]
    fn read_returns_none_when_corrupt() {
        let temp = TempDir::new().unwrap();
        let paths = open_shared_paths(temp.path()).unwrap();
        let input_key = [4; 32];
        std::fs::write(entry_meta_path(&paths, &input_key), b"not bincode").unwrap();
        assert!(read_entry_meta(&paths, &input_key).is_none());
    }

    #[test]
    fn second_write_is_idempotent_and_keeps_first() {
        let temp = TempDir::new().unwrap();
        let paths = open_shared_paths(temp.path()).unwrap();
        let input_key = [5; 32];

        write_entry_meta(&paths, &input_key, &sample_meta()).unwrap();

        let mut second = sample_meta();
        second.stdout = b"different".to_vec();
        let result = write_entry_meta(&paths, &input_key, &second).unwrap();

        assert_eq!(result, EntryMetaWriteResult::AlreadyExists);
        assert_eq!(read_entry_meta(&paths, &input_key).unwrap().stdout, b"out");
    }

    #[test]
    fn distinct_input_keys_do_not_collide() {
        let temp = TempDir::new().unwrap();
        let paths = open_shared_paths(temp.path()).unwrap();

        let mut a = sample_meta();
        a.stdout = b"task-a".to_vec();
        let mut b = sample_meta();
        b.stdout = b"task-b".to_vec();

        // Same outputs_hash — this is the #278 scenario.
        assert_eq!(a.outputs_hash, b.outputs_hash);

        write_entry_meta(&paths, &[1; 32], &a).unwrap();
        write_entry_meta(&paths, &[2; 32], &b).unwrap();

        assert_eq!(read_entry_meta(&paths, &[1; 32]).unwrap().stdout, b"task-a");
        assert_eq!(read_entry_meta(&paths, &[2; 32]).unwrap().stdout, b"task-b");
    }
}
