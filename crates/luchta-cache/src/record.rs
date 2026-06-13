use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// First schema version for filesystem-backed run-record metadata.
pub const SCHEMA_VERSION_V1: u32 = 1;

/// Serialized cache metadata for one task execution.
///
/// Invariant: `output_patterns` preserves declared or detected output patterns
/// even when `outputs` is empty. This keeps "no outputs declared" distinct from
/// "outputs declared, but zero current matches".
///
/// Bootstrap rule: later cache checks may trust stored pattern lists only when
/// matching detected flag is `true` for that side. `detected_input_patterns`
/// guards `input_patterns`; `detected_output_patterns` guards
/// `output_patterns`. When flag is `false`, callers must fall back to current
/// declared task patterns for that side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRunRecord {
    pub schema_version: u32,
    pub task_spec_hash: [u8; 32],
    pub input_patterns: Vec<String>,
    pub inputs: Vec<FileEntry>,
    pub output_patterns: Vec<String>,
    pub outputs: Vec<FileEntry>,
    /// `true` when `input_patterns` came from worker-detected inputs instead of
    /// declared task inputs.
    pub detected_input_patterns: bool,
    /// `true` when `output_patterns` came from worker-detected outputs instead
    /// of declared task outputs.
    pub detected_output_patterns: bool,
    pub outputs_hash: [u8; 32],
    pub env_hash: [u8; 32],
    pub pkg_dep_hash: [u8; 32],
    pub dep_outputs: BTreeMap<String, [u8; 32]>,
    pub exit_status: i32,
    pub succeeded: bool,
    pub start_unix_ms: u64,
    pub end_unix_ms: u64,
}

/// Snapshot of one resolved input or output path.
///
/// `absent = true` marks a declared literal path that did not exist at scan
/// time. Missing files stay distinct from present zero-byte files, and later
/// hashing/decision code must preserve that distinction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub mtime_ns: i128,
    pub hash: [u8; 32],
    pub absent: bool,
}

impl FileEntry {
    /// Builds sentinel entry for declared literal path that is currently absent.
    #[must_use]
    pub fn absent(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            size: 0,
            mtime_ns: 0,
            hash: [0; 32],
            absent: true,
        }
    }
}
