mod decide;
mod hashing;
mod record;
mod resolve;
pub mod shared;
mod store;

pub use decide::{decide, decide_shared_restore, CurrentState, Decision, FileStateResolver};
pub use hashing::{blake3_file, env_hash, pkg_dep_hash, task_spec_hash};
pub use luchta_types::{classify_pattern, InputSemantics};
pub use record::{FileEntry, TaskRunRecord, SCHEMA_VERSION_V1};
pub use resolve::{
    combined_outputs_hash, resolve_inputs, resolve_inputs_with_semantics, resolve_outputs,
    ResolveRequest,
};
pub use store::{
    resolve_cache_dir, Cache, RunArtifacts, CACHE_DIR_ENV, CACHE_DIR_NAME, GITIGNORE_CONTENTS,
    GITIGNORE_FILE_NAME, LUCHTA_DIR_NAME,
};

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("failed to access cache filesystem: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to serialize task run record: {0}")]
    SerializeRecord(bincode::error::EncodeError),
    #[error("invalid glob pattern: {0}")]
    InvalidGlob(globset::Error),
    #[error("failed to build glob matcher: {0}")]
    BuildGlobSet(globset::Error),
    #[error("git repository lookup failed: {0}")]
    Git(String),
    #[error("walkdir scan failed: {0}")]
    WalkDir(#[from] walkdir::Error),
    #[error("failed to strip base dir prefix: {0}")]
    StripBaseDir(String),
    #[error("mtime predates unix epoch: {0}")]
    InvalidMtime(String),
    #[error("input pattern expansion failed: {0}")]
    InputExpansion(String),
}

pub type Result<T> = std::result::Result<T, CacheError>;
