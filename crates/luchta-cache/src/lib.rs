mod decide;
mod hashing;
mod record;
mod resolve;
mod store;

pub use decide::{decide, CurrentState, Decision, FileStateResolver};
pub use hashing::{env_hash, pkg_dep_hash, task_spec_hash};
pub use record::{FileEntry, TaskRunRecord, SCHEMA_VERSION_V1};
pub use resolve::{
    classify_pattern, combined_outputs_hash, resolve_inputs, resolve_outputs, PatternKind,
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
}

pub type Result<T> = std::result::Result<T, CacheError>;
