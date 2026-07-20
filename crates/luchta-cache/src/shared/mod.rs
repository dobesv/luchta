pub(crate) mod atomicio;
pub mod blob;
pub mod gc;
pub mod git;
pub mod paths;
#[cfg(unix)]
pub mod rclone;
#[cfg(unix)]
mod remote;
pub mod scope;
pub mod snapshot;

pub(crate) use atomicio::atomic_write;
pub use blob::{
    restore_blob, restore_blob_with_meta, write_blob, write_blob_with_meta, BlobReadResult,
    BlobReadResultWithMeta, BlobWriteResult, MetaFiles, StagedRestore,
};
pub use gc::{maybe_run_gc, run_gc, GcStats, DEFAULT_GC_RETENTION, DEFAULT_GC_THROTTLE};
pub use git::{candidate_commit_keys, resolve_commit_key, CommitKey};
pub use paths::{
    open_shared_paths, resolve_shared_cache_dir, SharedCachePaths, BLOBS_DIR_NAME,
    SHARED_CACHE_DIR_ENV, SNAPSHOTS_DIR_NAME,
};
#[cfg(unix)]
pub use rclone::RcloneRcd;
#[cfg(unix)]
pub use rclone::DEFAULT_RCLONE_CONCURRENCY;
#[cfg(unix)]
pub use remote::RemoteConfig;
#[cfg(unix)]
pub(crate) use remote::RemoteSync;
#[cfg(unix)]
pub use remote::DEFAULT_TIMEOUT_DISABLE_THRESHOLD;
pub use scope::{classify_outputs, OutputScope, ScopeError};
pub use snapshot::{
    combined_dep_outputs_hash, derive_input_key, input_key_hex, MergeEntryOutcome, MergeResult,
    Snapshot, SnapshotEntry, SnapshotStore, SNAPSHOT_SCHEMA_VERSION,
};
use std::collections::HashMap;
#[cfg(unix)]
use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[cfg(unix)]
use tokio::task::JoinSet;

use crate::record::TaskRunRecord;
use crate::serialization::bincode_config;

/// Reserved prefix for metadata files inside blobs.
pub const META_DIR_NAME: &str = ".luchta-meta";
pub const META_STDOUT_FILE_NAME: &str = "stdout.log";
pub const META_STDERR_FILE_NAME: &str = "stderr.log";
pub const META_RECORD_FILE_NAME: &str = "meta.bincode";

/// Result of a successful cache restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoredHit {
    pub outputs_hash: [u8; 32],
    pub record: TaskRunRecord,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub reports: Vec<crate::store::ReportInput>,
}

/// A staged candidate from the shared cache, waiting for validation.
///
/// Contains the extracted blob in a staging directory, along with the
/// TaskRunRecord needed for validation. Call `commit()` to move files into
/// the package directory, or drop to discard without modification.
#[derive(Debug)]
pub struct StagedCandidate {
    pub outputs_hash: [u8; 32],
    pub record: TaskRunRecord,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub reports: Vec<crate::store::ReportInput>,
    staged: blob::StagedRestore,
}

impl StagedCandidate {
    /// Commit this restore by moving staged files into the package directory.
    pub fn commit(self) -> std::io::Result<(RestoredHit, Vec<std::path::PathBuf>)> {
        let written_paths = self.staged.commit()?;
        Ok((
            RestoredHit {
                outputs_hash: self.outputs_hash,
                record: self.record,
                stdout: self.stdout,
                stderr: self.stderr,
                reports: self.reports,
            },
            written_paths,
        ))
    }

    /// Discard this restore without modifying the package directory.
    pub fn discard(self) -> std::io::Result<()> {
        self.staged.discard()
    }
}

/// Result of a cache store operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreOutcome {
    /// Entry was stored successfully.
    Stored,
    /// Skipped: task did not succeed.
    SkippedNotSucceeded,
    /// Skipped: task duration below threshold.
    SkippedTooFast { duration_ms: u64 },
    /// Skipped: output size exceeds cap.
    SkippedTooLarge { bytes: u64 },
    /// Skipped: outputs cross package boundary.
    SkippedCrossPackage,
    /// Skipped: shared snapshot merge could not take lock or write snapshot metadata.
    SkippedLockUnavailable,
    /// Skipped: shared cache disabled (no write commit key).
    Disabled,
}

/// Merged index from all candidate snapshots, built lazily on first access.
#[derive(Debug, Clone)]
pub struct MergedIndex {
    /// input_key_hex -> (SnapshotEntry, commit_key) with newest-wins semantics.
    entries: HashMap<String, (SnapshotEntry, String)>,
    /// Loaded snapshots retained in memory for blob-miss fallback (newest-first order).
    snapshots: Vec<(String, Snapshot)>,
}

impl MergedIndex {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            snapshots: Vec::new(),
        }
    }

    fn insert_entry(&mut self, input_key_hex: String, entry: SnapshotEntry, commit_key: String) {
        // Newest-wins: later entries in the iteration overwrite earlier.
        self.entries.insert(input_key_hex, (entry, commit_key));
    }
}

/// Facade for the shared cache, composing blobs and snapshots.
#[derive(Debug)]
pub struct SharedCache {
    /// Resolved paths for the cache.
    paths: SharedCachePaths,
    /// Write commit key for the current repo state (None if dirty/unavailable).
    write_commit_key: Option<String>,
    /// Candidate commit keys for lookup (newest-first).
    candidate_keys: Vec<String>,
    /// Snapshot store for merge_entry.
    snapshot_store: SnapshotStore,
    /// Optional remote sync for on-demand restore pull.
    #[cfg(unix)]
    remote: Option<RemoteSync>,
    /// Lazily-built merged index.
    index: OnceLock<MergedIndex>,
    /// Size cap for individual blobs.
    size_cap_bytes: u64,
}

pub(crate) fn blob_path(paths: &SharedCachePaths, outputs_hash: &[u8; 32]) -> PathBuf {
    paths
        .blobs_dir
        .join(format!("{}.tar.zst", hex_hash(*outputs_hash)))
}

pub(crate) fn hex_hash(hash: [u8; 32]) -> String {
    blake3::Hash::from(hash).to_hex().to_string()
}

impl Drop for SharedCache {
    fn drop(&mut self) {
        // Own the rclone rcd daemon lifecycle: shut it down at run end so no
        // process is orphaned. SIGKILL skips Drop, but that is mitigated by the
        // per-run unique temp socket — any orphaned daemon is bound to a stale
        // socket path that is never reused by a later run.
        #[cfg(unix)]
        if let Some(remote) = &self.remote {
            remote.shutdown();
        }
    }
}

/// Optional inputs for [`SharedCache::open_with_remote`].
///
/// Bundles the rarely-set knobs (explicit cache directory, remote sync config)
/// so the opener keeps a small, fixed argument list.
#[derive(Debug, Default)]
pub struct OpenExtras<'a> {
    /// Explicit cache directory; `None` resolves from env/platform defaults.
    pub cache_dir: Option<&'a Path>,
    /// Remote sync config; `None` keeps the cache local-only.
    #[cfg(unix)]
    pub remote: Option<RemoteConfig>,
}

impl SharedCache {
    /// Opens the shared cache for a repo.
    ///
    /// Returns `None` if:
    /// - The shared cache directory cannot be created
    /// - No commit key is available (not in a git repo)
    pub fn open(repo_root: &Path, size_cap_bytes: u64, history_len: usize) -> Option<Self> {
        Self::open_with_remote(
            repo_root,
            size_cap_bytes,
            history_len,
            OpenExtras::default(),
        )
    }

    /// Opens the shared cache with an optional explicit cache directory.
    ///
    /// If `cache_dir` is provided, uses it directly instead of resolving
    /// from environment/platform defaults. This is useful for testing.
    pub fn open_with_cache_dir(
        repo_root: &Path,
        size_cap_bytes: u64,
        history_len: usize,
        cache_dir: Option<&Path>,
    ) -> Option<Self> {
        Self::open_with_remote(
            repo_root,
            size_cap_bytes,
            history_len,
            OpenExtras {
                cache_dir,
                #[cfg(unix)]
                remote: None,
            },
        )
    }

    /// Opens shared cache with optional cache directory and optional remote sync.
    pub fn open_with_remote(
        repo_root: &Path,
        size_cap_bytes: u64,
        history_len: usize,
        extras: OpenExtras<'_>,
    ) -> Option<Self> {
        let cache_path = extras
            .cache_dir
            .map(|p| p.to_path_buf())
            .unwrap_or_else(resolve_shared_cache_dir);
        let paths = open_shared_paths(&cache_path).ok()?;

        let write_commit_key = match resolve_commit_key(repo_root) {
            CommitKey::Clean(key) => Some(key),
            CommitKey::Dirty(key) => Some(key),
            CommitKey::Unavailable => None,
        };

        let candidate_keys = candidate_commit_keys(repo_root, history_len);

        let snapshot_store = SnapshotStore::new(paths.clone());
        #[cfg(unix)]
        let remote = match extras.remote {
            Some(config) => match RemoteSync::from_config(config) {
                Ok(remote) => Some(remote),
                Err(err) => {
                    eprintln!("warn: shared cache remote disabled: {err}");
                    None
                }
            },
            None => None,
        };

        Some(Self {
            paths,
            write_commit_key,
            candidate_keys,
            snapshot_store,
            #[cfg(unix)]
            remote,
            index: OnceLock::new(),
            size_cap_bytes,
        })
    }

    #[must_use]
    pub fn paths(&self) -> &SharedCachePaths {
        &self.paths
    }
    #[cfg(test)]
    pub fn from_parts_for_test(
        repo_root: &Path,
        size_cap_bytes: u64,
        history_len: usize,
        snapshot_store: SnapshotStore,
    ) -> Option<Self> {
        let paths = snapshot_store.paths().clone();

        let write_commit_key = match resolve_commit_key(repo_root) {
            CommitKey::Clean(key) => Some(key),
            CommitKey::Dirty(key) => Some(key),
            CommitKey::Unavailable => None,
        };

        let candidate_keys = candidate_commit_keys(repo_root, history_len);

        Some(Self {
            paths,
            write_commit_key,
            candidate_keys,
            snapshot_store,
            #[cfg(unix)]
            remote: None,
            index: OnceLock::new(),
            size_cap_bytes,
        })
    }

    /// Attempts to restore cached artifacts for a task.
    ///
    /// Lookup proceeds as follows:
    /// 1. Build merged index on first access (lazy, ONCE).
    /// 2. O(1) lookup by input_key in merged index.
    /// 3. If found, extract blob to staging and return a StagedCandidate for validation.
    /// 4. Caller validates candidate by calling `validate()` with a FileStateResolver.
    /// 5. If valid, caller calls `commit()` to move files into package_dir.
    /// 6. If blob missing, fall back to older candidates (blob-miss) from in-memory snapshots.
    pub fn try_restore_candidates(
        &self,
        _task_id: &str,
        input_key: &[u8; 32],
        package_dir: &Path,
    ) -> impl Iterator<Item = StagedCandidate> + '_ {
        #[cfg(unix)]
        self.pull_remote_snapshots_for_restore();
        let index = self.get_or_build_index();
        let input_key_hex = input_key_hex(*input_key);

        // O(1) lookup in the merged index — NO disk read
        let primary_entry = index.entries.get(&input_key_hex);

        // Collect all candidate entries (primary + alternates) with their outputs_hash
        let mut candidates: Vec<SnapshotEntry> = Vec::new();

        if let Some((entry, _commit_key)) = primary_entry {
            candidates.push(entry.clone());
        }

        // Blob-miss fallback: collect entries from older in-memory snapshots with different outputs_hash
        for (_commit_key, snapshot) in &index.snapshots {
            if let Some(alt_entry) = snapshot.entries.get(&input_key_hex) {
                // Skip if same outputs_hash (already have this candidate)
                if candidates
                    .iter()
                    .any(|c| c.outputs_hash == alt_entry.outputs_hash)
                {
                    continue;
                }
                candidates.push(alt_entry.clone());
            }
        }

        // Stage each candidate (returns None for missing/corrupt blobs)
        let paths = self.paths.clone();
        let package_dir = package_dir.to_path_buf();
        #[cfg(unix)]
        let remote = self.remote.clone();
        candidates.into_iter().filter_map(move |entry| {
            Self::stage_entry(
                &entry,
                &paths,
                &package_dir,
                #[cfg(unix)]
                remote.as_ref(),
            )
        })
    }

    #[cfg(unix)]
    fn pull_remote_snapshots_for_restore(&self) {
        let Some(remote) = self.remote.as_ref() else {
            return;
        };
        self.index.get_or_init(|| self.build_index(Some(remote)));
    }

    /// Stage a single entry, returning a StagedCandidate for validation.
    fn stage_entry(
        entry: &SnapshotEntry,
        paths: &SharedCachePaths,
        package_dir: &Path,
        #[cfg(unix)] remote: Option<&RemoteSync>,
    ) -> Option<StagedCandidate> {
        if !blob_path(paths, &entry.outputs_hash).is_file() {
            #[cfg(unix)]
            if let Some(remote) = remote {
                if let Err(err) = remote.pull_blob(paths, &entry.outputs_hash) {
                    eprintln!(
                        "debug: remote blob pull failed for outputs_hash={}: {err}",
                        hex_hash(entry.outputs_hash)
                    );
                }
            }
        }
        let staged = match restore_blob_with_meta(paths, &entry.outputs_hash, package_dir) {
            Ok(BlobReadResultWithMeta::Restored(staged)) => staged,
            Ok(BlobReadResultWithMeta::Missing) | Ok(BlobReadResultWithMeta::Corrupt) => {
                return None
            }
            Err(_) => return None,
        };

        // Decode record.
        let record: TaskRunRecord =
            match bincode::serde::decode_from_slice(&staged.meta.record, bincode_config()) {
                Ok((record, _)) => record,
                Err(_) => {
                    // Discard staging on decode failure
                    let _ = staged.discard();
                    return None;
                }
            };

        let meta = staged.meta.clone();
        Some(StagedCandidate {
            outputs_hash: entry.outputs_hash,
            record,
            stdout: meta.stdout,
            stderr: meta.stderr,
            reports: meta.reports,
            staged,
        })
    }

    /// Legacy method for backward compatibility.
    /// Attempts to restore and validate in one step (immediate commit).
    #[deprecated(note = "Use try_restore_candidates with validate callback instead")]
    pub fn try_restore(
        &self,
        _task_id: &str,
        _input_key: &[u8; 32],
        _package_dir: &Path,
    ) -> Option<RestoredHit> {
        // This is no longer used by the CLI - kept for backward compat
        None
    }

    /// Builds the merged index on first access.
    fn get_or_build_index(&self) -> &MergedIndex {
        self.index.get_or_init(|| {
            self.build_index(
                #[cfg(unix)]
                self.remote.as_ref(),
            )
        })
    }

    fn build_index(&self, #[cfg(unix)] remote: Option<&RemoteSync>) -> MergedIndex {
        #[cfg(unix)]
        self.pull_candidate_commits(remote);

        let mut merged = MergedIndex::new();

        // Iterate in reverse order (oldest first) so that newest overwrites.
        for commit_key in self.candidate_keys.iter().rev() {
            self.load_commit_into_index(
                &mut merged,
                commit_key,
                #[cfg(unix)]
                remote,
            );
        }

        // Reverse snapshots so newest is first.
        merged.snapshots.reverse();

        merged
    }

    #[cfg(unix)]
    fn pull_candidate_commits(&self, remote: Option<&RemoteSync>) {
        let Some(remote) = remote.cloned() else {
            return;
        };
        Self::run_candidate_pulls_on_dedicated_thread(
            remote,
            self.snapshot_store.clone(),
            self.candidate_keys.clone(),
        );
    }

    #[cfg(unix)]
    fn run_candidate_pulls_on_dedicated_thread(
        remote: RemoteSync,
        snapshot_store: SnapshotStore,
        candidate_keys: Vec<String>,
    ) {
        let concurrency = candidate_keys.len().clamp(1, 4);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(concurrency)
            .enable_all()
            .build()
            .expect("candidate pull runtime");
        std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    runtime.block_on(async move {
                        Self::pull_candidate_commits_with_runtime(
                            remote,
                            snapshot_store,
                            candidate_keys,
                            concurrency,
                        )
                        .await;
                    })
                })
                .join()
                .expect("candidate snapshot pull thread panicked");
        });
    }

    #[cfg(unix)]
    async fn pull_candidate_commits_with_runtime(
        remote: RemoteSync,
        snapshot_store: SnapshotStore,
        candidate_keys: Vec<String>,
        concurrency: usize,
    ) {
        let mut pending: VecDeque<_> = candidate_keys.into();
        let mut in_flight = JoinSet::new();
        while in_flight.len() < concurrency {
            let Some(commit_key) = pending.pop_front() else {
                break;
            };
            Self::spawn_candidate_pull(
                &mut in_flight,
                remote.clone(),
                snapshot_store.clone(),
                commit_key,
            );
        }
        while let Some(result) = in_flight.join_next().await {
            result.expect("candidate snapshot pull task panicked");
            if let Some(commit_key) = pending.pop_front() {
                Self::spawn_candidate_pull(
                    &mut in_flight,
                    remote.clone(),
                    snapshot_store.clone(),
                    commit_key,
                );
            }
        }
    }

    #[cfg(unix)]
    fn spawn_candidate_pull(
        in_flight: &mut JoinSet<()>,
        remote: RemoteSync,
        snapshot_store: SnapshotStore,
        commit_key: String,
    ) {
        in_flight.spawn_blocking(move || {
            remote.pull_snapshot_commit(&snapshot_store, &commit_key);
        });
    }

    /// Pull (if remote-enabled) and merge a single commit's snapshot into the index.
    fn load_commit_into_index(
        &self,
        merged: &mut MergedIndex,
        commit_key: &str,
        #[cfg(unix)] remote: Option<&RemoteSync>,
    ) {
        #[cfg(unix)]
        let _ = remote;
        let Some(snapshot) = self.snapshot_store.load(commit_key) else {
            return;
        };
        for (input_key_hex, entry) in &snapshot.entries {
            merged.insert_entry(input_key_hex.clone(), entry.clone(), commit_key.to_string());
        }
        merged.snapshots.push((commit_key.to_string(), snapshot));
    }

    /// Store task outputs in the shared cache.
    ///
    /// Requirements for cacheable:
    /// - Task succeeded
    /// - Duration >= 100ms
    /// - OutputScope::InPackage
    /// - Total size <= size_cap_bytes
    ///
    /// Stores:
    /// - Blob with meta files (.luchta-meta/{stdout.log,stderr.log,meta.bincode})
    /// - Snapshot entry via merge_entry to write_commit_key
    #[allow(clippy::too_many_arguments)]
    pub fn store(
        &self,
        task_id: &str,
        input_key: &[u8; 32],
        outputs_hash: &[u8; 32],
        package_dir: &Path,
        rel_output_paths: &[std::path::PathBuf],
        record: &TaskRunRecord,
        stdout: &[u8],
        stderr: &[u8],
        reports: &[crate::store::ReportInput],
        repo_root: &Path,
    ) -> io::Result<StoreOutcome> {
        // Check if cache is disabled (no write key).
        let write_key = match &self.write_commit_key {
            Some(key) => key.clone(),
            None => return Ok(StoreOutcome::Disabled),
        };

        // Check if task succeeded.
        if !record.succeeded {
            return Ok(StoreOutcome::SkippedNotSucceeded);
        }

        // Check duration threshold.
        let duration_ms = record.end_unix_ms.saturating_sub(record.start_unix_ms);
        if duration_ms < 100 {
            return Ok(StoreOutcome::SkippedTooFast { duration_ms });
        }

        // Check output scope.
        match classify_outputs(repo_root, package_dir, rel_output_paths) {
            Ok(OutputScope::InPackage) => {}
            Ok(OutputScope::CrossPackage) => return Ok(StoreOutcome::SkippedCrossPackage),
            Err(ScopeError::PathEscape { .. }) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "output path escapes repository root",
                ));
            }
        }

        // Prepare meta files.
        let meta_record =
            bincode::serde::encode_to_vec(record, bincode_config()).map_err(io::Error::other)?;

        let meta = MetaFiles {
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
            record: meta_record,
            reports: reports.to_vec(),
        };

        // Write blob with meta.
        let blob_result = write_blob_with_meta(
            &self.paths,
            outputs_hash,
            package_dir,
            rel_output_paths,
            self.size_cap_bytes,
            &meta,
        )?;

        let entry = SnapshotEntry {
            task_id: task_id.to_string(),
            input_key: *input_key,
            outputs_hash: *outputs_hash,
            task_spec_hash: record.task_spec_hash,
            env_hash: record.env_hash,
            pkg_dep_hash: record.pkg_dep_hash,
            duration_ms,
            output_bytes: record.outputs.iter().map(|f| f.size).sum(),
            cached_at_unix_ms: record.end_unix_ms,
            tool_version: None,
        };
        self.finish_store(blob_result, &write_key, entry)
    }

    /// Records the snapshot entry and pushes to the remote after a blob write.
    fn finish_store(
        &self,
        blob_result: BlobWriteResult,
        write_key: &str,
        entry: SnapshotEntry,
    ) -> io::Result<StoreOutcome> {
        match blob_result {
            BlobWriteResult::Written | BlobWriteResult::AlreadyExists => {
                #[cfg(unix)]
                let outputs_hash = entry.outputs_hash;
                let merge = self
                    .snapshot_store
                    .merge_entry_with_outcome(write_key, entry);
                if matches!(merge.result, MergeResult::SkippedLockUnavailable) {
                    return Ok(StoreOutcome::SkippedLockUnavailable);
                }
                #[cfg(unix)]
                if let Some(remote) = &self.remote {
                    remote.push_store_artifacts(remote::PushArtifacts {
                        paths: &self.paths,
                        commit_key: write_key,
                        outputs_hash: &outputs_hash,
                        merge,
                    });
                }
                Ok(StoreOutcome::Stored)
            }
            BlobWriteResult::SkippedTooLarge { bytes } => {
                Ok(StoreOutcome::SkippedTooLarge { bytes })
            }
            BlobWriteResult::NoOutputs => Ok(StoreOutcome::Stored), // Empty outputs are cacheable.
        }
    }

    /// Returns the write commit key for this cache.
    #[must_use]
    pub fn write_commit_key(&self) -> Option<&str> {
        self.write_commit_key.as_deref()
    }

    /// Returns the candidate keys for this cache.
    #[must_use]
    pub fn candidate_keys(&self) -> &[String] {
        &self.candidate_keys
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::FileEntry;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use tempfile::TempDir;

    pub(crate) fn setup_git_repo(repo_root: &Path) {
        use std::process::Command;
        Command::new("git")
            .args(["init"])
            .current_dir(repo_root)
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(repo_root)
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(repo_root)
            .status()
            .unwrap();
    }

    pub(crate) fn create_commit(repo_root: &Path) -> String {
        use std::process::Command;
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        fs::write(repo_root.join(format!("file-{unique}.txt")), "content").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(repo_root)
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", &format!("commit-{unique}")])
            .current_dir(repo_root)
            .status()
            .unwrap();
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo_root)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    pub(crate) fn sample_record(succeeded: bool, duration_ms: u64) -> TaskRunRecord {
        let start = 1_000_000_000_000_u64;
        TaskRunRecord {
            schema_version: crate::record::SCHEMA_VERSION_V4,
            task_spec_hash: [1; 32],
            input_patterns: vec!["src/**/*.ts".to_string()],
            inputs: vec![],
            output_patterns: vec!["dist/**/*.js".to_string()],
            outputs: vec![FileEntry {
                path: "dist/main.js".to_string(),
                size: 100,
                mtime_ns: 0,
                hash: [2; 32],
                absent: false,
            }],
            detected_input_patterns: true,
            detected_output_patterns: true,
            outputs_hash: [3; 32],
            env_hash: [4; 32],
            pkg_dep_hash: [5; 32],
            dep_outputs: BTreeMap::new(),
            exit_status: if succeeded { 0 } else { 1 },
            succeeded,
            start_unix_ms: start,
            end_unix_ms: start + duration_ms,
            reports: vec![],
            cache_nonce: None,
            run_reason: None,
        }
    }

    #[test]
    fn store_and_restore_round_trip_byte_identical() {
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        let _commit = create_commit(temp_repo.path());

        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            10,
            Some(temp_cache.path()),
        )
        .unwrap();

        // Create outputs.
        let package_dir = temp_repo.path().join("pkg");
        fs::create_dir_all(package_dir.join("dist")).unwrap();
        fs::write(package_dir.join("dist/main.js"), "console.log('hi');").unwrap();

        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);
        let record = sample_record(true, 200);

        let result = cache
            .store(
                "pkg#build",
                &input_key,
                &[7; 32],
                &package_dir,
                &[PathBuf::from("dist/main.js")],
                &record,
                b"stdout output",
                b"stderr output",
                &[],
                temp_repo.path(),
            )
            .unwrap();
        assert_eq!(result, StoreOutcome::Stored);

        // Restore into a fresh directory.
        let restore_dir = temp_repo.path().join("restore");
        fs::create_dir_all(&restore_dir).unwrap();

        // Use new try_restore_candidates API - commit first valid candidate
        let (hit, written_paths) = cache
            .try_restore_candidates("pkg#build", &input_key, &restore_dir)
            .next()
            .expect("expected at least one candidate")
            .commit()
            .expect("commit should succeed");
        assert_eq!(hit.outputs_hash, [7; 32]);
        assert_eq!(hit.stdout, b"stdout output");
        assert_eq!(hit.stderr, b"stderr output");
        assert!(hit.record.succeeded);
        assert_eq!(written_paths, vec![restore_dir.join("dist/main.js")]);

        // Check file content.
        let restored_content = fs::read(restore_dir.join("dist/main.js")).unwrap();
        assert_eq!(restored_content, b"console.log('hi');");

        // Check no .luchta-meta litter.
        assert!(!restore_dir.join(".luchta-meta").exists());

        // Verify Cache::write works with the record.
        let local_cache =
            crate::store::Cache::open(&temp_cache.path().join(".luchta").join("cache")).unwrap();
        local_cache
            .write(
                "pkg#build",
                crate::store::RunArtifacts {
                    record: &hit.record,
                    stdout: &hit.stdout,
                    stderr: &hit.stderr,
                    reports: &[],
                },
            )
            .unwrap();

        let read_back = local_cache.read("pkg#build").unwrap();
        assert_eq!(read_back, hit.record);
    }

    #[test]
    #[cfg(unix)]
    fn store_preserves_exec_bit() {
        use std::os::unix::fs::PermissionsExt;

        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());

        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            10,
            Some(temp_cache.path()),
        )
        .unwrap();

        let package_dir = temp_repo.path().join("pkg");
        fs::create_dir_all(package_dir.join("dist")).unwrap();
        let script_path = package_dir.join("dist/script.sh");
        fs::write(&script_path, "#!/bin/bash\necho hi").unwrap();
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();

        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);
        let record = sample_record(true, 200);

        cache
            .store(
                "pkg#build",
                &input_key,
                &[7; 32],
                &package_dir,
                &[PathBuf::from("dist/script.sh")],
                &record,
                b"",
                b"",
                &[],
                temp_repo.path(),
            )
            .unwrap();

        let restore_dir = temp_repo.path().join("restore");
        fs::create_dir_all(&restore_dir).unwrap();
        cache
            .try_restore_candidates("pkg#build", &input_key, &restore_dir)
            .next()
            .expect("expected at least one candidate")
            .commit()
            .expect("commit should succeed");

        let restored_path = restore_dir.join("dist/script.sh");
        assert!(restored_path.exists(), "restored file should exist");
        let mode = fs::metadata(&restored_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "exec bit should be preserved");
    }

    #[test]
    fn store_excludes_failed_tasks() {
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());

        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            10,
            Some(temp_cache.path()),
        )
        .unwrap();

        let package_dir = temp_repo.path().join("pkg");
        fs::create_dir_all(package_dir.join("dist")).unwrap();
        fs::write(package_dir.join("dist/main.js"), "content").unwrap();

        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);
        let record = sample_record(false, 200);

        let result = cache
            .store(
                "pkg#build",
                &input_key,
                &[7; 32],
                &package_dir,
                &[PathBuf::from("dist/main.js")],
                &record,
                b"",
                b"",
                &[],
                temp_repo.path(),
            )
            .unwrap();
        assert_eq!(result, StoreOutcome::SkippedNotSucceeded);
    }

    #[test]
    fn store_excludes_fast_tasks() {
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());

        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            10,
            Some(temp_cache.path()),
        )
        .unwrap();

        let package_dir = temp_repo.path().join("pkg");
        fs::create_dir_all(package_dir.join("dist")).unwrap();
        fs::write(package_dir.join("dist/main.js"), "content").unwrap();

        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);
        let record = sample_record(true, 50); // 50ms < 100ms

        let result = cache
            .store(
                "pkg#build",
                &input_key,
                &[7; 32],
                &package_dir,
                &[PathBuf::from("dist/main.js")],
                &record,
                b"",
                b"",
                &[],
                temp_repo.path(),
            )
            .unwrap();
        assert_eq!(result, StoreOutcome::SkippedTooFast { duration_ms: 50 });
    }

    #[test]
    fn store_excludes_over_cap() {
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());

        // Cap: 1 byte - output file will definitely exceed this.
        let temp_cache = TempDir::new().unwrap();
        let cache =
            SharedCache::open_with_cache_dir(temp_repo.path(), 1, 10, Some(temp_cache.path()))
                .unwrap();

        let package_dir = temp_repo.path().join("pkg");
        fs::create_dir_all(package_dir.join("dist")).unwrap();
        // Write content that exceeds cap (and also exceeds meta size)
        fs::write(package_dir.join("dist/main.js"), "x").unwrap();

        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);
        let record = sample_record(true, 200);

        let result = cache
            .store(
                "pkg#build",
                &input_key,
                &[7; 32],
                &package_dir,
                &[PathBuf::from("dist/main.js")],
                &record,
                b"",
                b"",
                &[],
                temp_repo.path(),
            )
            .unwrap();

        // Should be SkippedTooLarge because output file size (1) + meta sizes > cap (1)
        assert!(
            matches!(result, StoreOutcome::SkippedTooLarge { .. }),
            "expected SkippedTooLarge, got {:?}",
            result
        );
    }

    #[test]
    fn store_excludes_cross_package() {
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());

        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            10,
            Some(temp_cache.path()),
        )
        .unwrap();

        // Package dir is pkg-a, output goes to ../pkg-b.
        let package_dir = temp_repo.path().join("packages/pkg-a");
        let other_package = temp_repo.path().join("packages/pkg-b");
        fs::create_dir_all(&package_dir).unwrap();
        fs::create_dir_all(&other_package).unwrap();
        fs::write(other_package.join("output.txt"), "content").unwrap();

        // Point rel_output_paths to sibling package.
        // First, create the actual file in package_dir to avoid NotFound.
        fs::create_dir_all(package_dir.join("dist")).unwrap();
        fs::write(package_dir.join("dist/local.txt"), "local").unwrap();

        // Create a sibling file that triggers cross-package when classified.
        // We need outputs that resolve to outside package_dir.
        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);
        let mut record = sample_record(true, 200);
        record.outputs = vec![FileEntry {
            path: "../pkg-b/output.txt".to_string(),
            size: 7,
            mtime_ns: 0,
            hash: [1; 32],
            absent: false,
        }];

        let result = cache
            .store(
                "pkg#build",
                &input_key,
                &[7; 32],
                &package_dir,
                &[PathBuf::from("../pkg-b/output.txt")],
                &record,
                b"",
                b"",
                &[],
                temp_repo.path(),
            )
            .unwrap();
        assert_eq!(result, StoreOutcome::SkippedCrossPackage);
    }

    #[test]
    fn store_rejects_escape() {
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());

        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            10,
            Some(temp_cache.path()),
        )
        .unwrap();

        let package_dir = temp_repo.path().join("pkg");
        fs::create_dir_all(&package_dir).unwrap();

        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);
        let record = sample_record(true, 200);

        let result = cache.store(
            "pkg#build",
            &input_key,
            &[7; 32],
            &package_dir,
            &[PathBuf::from("../../../etc/passwd")],
            &record,
            b"",
            b"",
            &[],
            temp_repo.path(),
        );
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[cfg(unix)]
    #[test]
    fn store_returns_skipped_when_snapshot_lock_unavailable() {
        use std::os::unix::fs::PermissionsExt;

        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());

        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            10,
            Some(temp_cache.path()),
        )
        .unwrap();

        let package_dir = temp_repo.path().join("pkg");
        fs::create_dir_all(package_dir.join("dist")).unwrap();
        fs::write(package_dir.join("dist/main.js"), "content").unwrap();
        fs::set_permissions(
            &cache.paths.snapshots_dir,
            fs::Permissions::from_mode(0o500),
        )
        .unwrap();

        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);
        let record = sample_record(true, 200);

        let result = cache
            .store(
                "pkg#build",
                &input_key,
                &[7; 32],
                &package_dir,
                &[PathBuf::from("dist/main.js")],
                &record,
                b"",
                b"",
                &[],
                temp_repo.path(),
            )
            .unwrap();

        assert_eq!(result, StoreOutcome::SkippedLockUnavailable);
    }

    #[test]
    fn multi_candidate_hit_in_older_snapshot() {
        // Create repo with 2 commits.
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        let commit1 = create_commit(temp_repo.path());
        let commit2 = create_commit(temp_repo.path());

        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            5,
            Some(temp_cache.path()),
        )
        .unwrap();

        // Store under commit1.
        let package_dir = temp_repo.path().join("pkg");
        fs::create_dir_all(package_dir.join("dist")).unwrap();
        fs::write(package_dir.join("dist/main.js"), "v1").unwrap();

        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);
        let record = sample_record(true, 200);

        // Manually create snapshot for commit1 (older).
        let entry_commit1 = SnapshotEntry {
            task_id: "pkg#build".to_string(),
            input_key,
            outputs_hash: [7; 32],
            task_spec_hash: [1; 32],
            env_hash: [2; 32],
            pkg_dep_hash: [3; 32],
            duration_ms: 200,
            output_bytes: 100,
            cached_at_unix_ms: 1_000_000_000_000,
            tool_version: None,
        };
        cache
            .snapshot_store
            .merge_entry(&commit1, entry_commit1.clone());

        // Create blob with the right outputs_hash.
        let meta = MetaFiles {
            stdout: b"stdout v1".to_vec(),
            stderr: b"stderr v1".to_vec(),
            record: bincode::serde::encode_to_vec(&record, bincode_config()).unwrap(),
            reports: vec![],
        };
        write_blob_with_meta(
            &cache.paths,
            &[7; 32],
            &package_dir,
            &[PathBuf::from("dist/main.js")],
            1_000_000,
            &meta,
        )
        .unwrap();

        // Now also add a snapshot entry for commit2 (newer) with a DIFFERENT outputs_hash
        // This simulates the case where commit2's blob is missing but commit1's exists.
        let entry_commit2 = SnapshotEntry {
            task_id: "pkg#build".to_string(),
            input_key,
            outputs_hash: [8; 32], // Different hash - blob won't exist for this.
            task_spec_hash: [1; 32],
            env_hash: [2; 32],
            pkg_dep_hash: [3; 32],
            duration_ms: 200,
            output_bytes: 100,
            cached_at_unix_ms: 2_000_000_000_000,
            tool_version: None,
        };
        cache.snapshot_store.merge_entry(&commit2, entry_commit2);

        // Restore should find entry from commit1 via blob-miss fallback.
        let restore_dir = temp_repo.path().join("restore");
        fs::create_dir_all(&restore_dir).unwrap();

        // Verify candidate_keys includes both commits.
        assert!(cache.candidate_keys().contains(&commit1));

        // Use new try_restore_candidates API
        let mut candidates: Vec<_> = cache
            .try_restore_candidates("pkg#build", &input_key, &restore_dir)
            .collect();

        // Should have at least one candidate
        assert!(!candidates.is_empty(), "expected at least one candidate");

        // The first valid candidate should be from commit1 (commit2's blob is missing)
        let (hit, written_paths) = candidates
            .remove(0)
            .commit()
            .expect("commit should succeed");
        assert_eq!(hit.stdout, b"stdout v1");
        assert_eq!(written_paths, vec![restore_dir.join("dist/main.js")]);
    }

    #[test]
    fn newest_wins_on_conflict() {
        // Create repo with 2 commits.
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        let commit1 = create_commit(temp_repo.path());
        let commit2 = create_commit(temp_repo.path());

        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            5,
            Some(temp_cache.path()),
        )
        .unwrap();

        // Same input_key, different outputs_hash.
        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);

        let entry1 = SnapshotEntry {
            task_id: "pkg#build".to_string(),
            input_key,
            outputs_hash: [1; 32],
            task_spec_hash: [1; 32],
            env_hash: [2; 32],
            pkg_dep_hash: [3; 32],
            duration_ms: 100,
            output_bytes: 50,
            cached_at_unix_ms: 1_000_000_000_000,
            tool_version: None,
        };

        let mut entry2 = entry1.clone();
        entry2.outputs_hash = [2; 32];
        entry2.cached_at_unix_ms = 2_000_000_000_000;

        // Insert both entries (order doesn't matter, newest wins).
        cache.snapshot_store.merge_entry(&commit2, entry2);
        cache.snapshot_store.merge_entry(&commit1, entry1);

        // Build index.
        let index = cache.get_or_build_index();
        let input_hex = input_key_hex(input_key);
        let (found, _key) = index.entries.get(&input_hex).unwrap();

        // Newest (commit2) should win.
        assert_eq!(found.outputs_hash, [2; 32]);
    }

    fn write_snapshot_fixture(snapshot_dir: &Path, commit: &str, entry: SnapshotEntry) {
        let mut snapshot = Snapshot::new();
        snapshot
            .entries
            .insert(input_key_hex(entry.input_key), entry);
        let encoded = bincode::serde::encode_to_vec(
            &snapshot,
            crate::shared::snapshot::snapshot_bincode_config(),
        )
        .unwrap();
        fs::create_dir_all(snapshot_dir.join(commit)).unwrap();
        fs::write(snapshot_dir.join(commit).join("a.bincode"), encoded).unwrap();
    }

    #[test]
    fn load_once_proven_via_counter() {
        use std::sync::atomic::Ordering;

        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        let commit1 = create_commit(temp_repo.path());
        let commit2 = create_commit(temp_repo.path());
        let temp_cache = TempDir::new().unwrap();
        let snapshot_dir = temp_cache.path().join("snapshots");
        let input_key1 = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);
        let input_key2 = derive_input_key([5; 32], [6; 32], [7; 32], [8; 32]);

        write_snapshot_fixture(
            &snapshot_dir,
            &commit1,
            SnapshotEntry {
                task_id: "pkg#build".to_string(),
                input_key: input_key1,
                outputs_hash: [7; 32],
                task_spec_hash: [1; 32],
                env_hash: [2; 32],
                pkg_dep_hash: [3; 32],
                duration_ms: 200,
                output_bytes: 100,
                cached_at_unix_ms: 1_000_000_000_000,
                tool_version: None,
            },
        );
        write_snapshot_fixture(
            &snapshot_dir,
            &commit2,
            SnapshotEntry {
                task_id: "pkg#build".to_string(),
                input_key: input_key2,
                outputs_hash: [8; 32],
                task_spec_hash: [5; 32],
                env_hash: [6; 32],
                pkg_dep_hash: [7; 32],
                duration_ms: 100,
                output_bytes: 50,
                cached_at_unix_ms: 2_000_000_000_000,
                tool_version: None,
            },
        );

        let paths = open_shared_paths(temp_cache.path()).unwrap();
        let (snapshot_store, load_counter) = SnapshotStore::new_with_counter(paths);
        let cache =
            SharedCache::from_parts_for_test(temp_repo.path(), 1_000_000, 10, snapshot_store)
                .unwrap();
        let restore_dir = temp_repo.path().join("restore");

        for i in 0..50 {
            fs::create_dir_all(&restore_dir).unwrap();
            let input_key = if i % 2 == 0 { &input_key1 } else { &input_key2 };
            if let Some(candidate) = cache
                .try_restore_candidates("pkg#build", input_key, &restore_dir)
                .next()
            {
                let _ = candidate.commit();
            }
            fs::remove_dir_all(&restore_dir).ok();
        }

        let snapshot_file_count = 2;
        assert_eq!(
            load_counter.load(Ordering::SeqCst),
            snapshot_file_count,
            "FAIL: Snapshot files reloaded from disk! Expected {} loads (once per file), got {}. \
             If try_restore re-reads on each call, count would be 100+.",
            snapshot_file_count,
            load_counter.load(Ordering::SeqCst)
        );
        assert!(cache.index.get().is_some());
    }

    #[test]
    fn concurrent_try_restore_once_lock_init() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());

        let temp_cache = TempDir::new().unwrap();
        let cache = Arc::new(
            SharedCache::open_with_cache_dir(
                temp_repo.path(),
                1_000_000,
                10,
                Some(temp_cache.path()),
            )
            .unwrap(),
        );

        // Create minimal cacheable state.
        let package_dir = temp_repo.path().join("pkg");
        fs::create_dir_all(package_dir.join("dist")).unwrap();
        fs::write(package_dir.join("dist/main.js"), "content").unwrap();

        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);
        let record = sample_record(true, 200);

        cache
            .store(
                "pkg#build",
                &input_key,
                &[7; 32],
                &package_dir,
                &[PathBuf::from("dist/main.js")],
                &record,
                b"",
                b"",
                &[],
                temp_repo.path(),
            )
            .unwrap();

        // Concurrent restore threads.
        let initialized = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();

        for i in 0..4 {
            let cache = Arc::clone(&cache);
            let initialized = Arc::clone(&initialized);
            let restore_dir = temp_repo.path().join(format!("restore-{}", i));
            fs::create_dir_all(&restore_dir).unwrap();

            handles.push(thread::spawn(move || {
                let result = cache
                    .try_restore_candidates("pkg#build", &input_key, &restore_dir)
                    .next();
                // Mark that we initialized the index.
                initialized.store(cache.index.get().is_some(), Ordering::SeqCst);
                result
            }));
        }

        // All threads complete.
        for handle in handles {
            handle.join().unwrap();
        }

        // Index was initialized exactly once (OnceLock guarantee).
        assert!(initialized.load(Ordering::SeqCst));
    }

    #[test]
    fn no_luchta_meta_litter_after_restore() {
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());

        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            10,
            Some(temp_cache.path()),
        )
        .unwrap();

        let package_dir = temp_repo.path().join("pkg");
        fs::create_dir_all(package_dir.join("dist")).unwrap();
        fs::write(package_dir.join("dist/main.js"), "content").unwrap();

        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);
        let record = sample_record(true, 200);

        cache
            .store(
                "pkg#build",
                &input_key,
                &[7; 32],
                &package_dir,
                &[PathBuf::from("dist/main.js")],
                &record,
                b"stdout content",
                b"stderr content",
                &[],
                temp_repo.path(),
            )
            .unwrap();

        let restore_dir = temp_repo.path().join("restore");
        fs::create_dir_all(&restore_dir).unwrap();
        cache
            .try_restore_candidates("pkg#build", &input_key, &restore_dir)
            .next()
            .expect("expected at least one candidate")
            .commit()
            .expect("commit should succeed");

        // Verify .luchta-meta does NOT exist in restore_dir.
        assert!(!restore_dir.join(".luchta-meta").exists());
        assert!(!restore_dir.join(".luchta-meta/stdout.log").exists());
        assert!(!restore_dir.join(".luchta-meta/stderr.log").exists());
        assert!(!restore_dir.join(".luchta-meta/meta.bincode").exists());
    }
}
