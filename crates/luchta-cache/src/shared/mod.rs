pub(crate) mod atomicio;
pub mod blob;
pub mod gc;
pub mod git;
pub mod paths;
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
pub use scope::{classify_outputs, OutputScope, ScopeError};
pub use snapshot::{
    combined_dep_outputs_hash, derive_input_key, input_key_hex, MergeResult, Snapshot,
    SnapshotEntry, SnapshotStore, SNAPSHOT_SCHEMA_VERSION,
};

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::OnceLock;

use crate::record::TaskRunRecord;

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
    staged: blob::StagedRestore,
}

impl StagedCandidate {
    /// Commit this restore by moving staged files into the package directory.
    pub fn commit(self) -> std::io::Result<RestoredHit> {
        self.staged.commit()?;
        Ok(RestoredHit {
            outputs_hash: self.outputs_hash,
            record: self.record,
            stdout: self.stdout,
            stderr: self.stderr,
        })
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
    /// Lazily-built merged index.
    index: OnceLock<MergedIndex>,
    /// Size cap for individual blobs.
    size_cap_bytes: u64,
}

impl SharedCache {
    /// Opens the shared cache for a repo.
    ///
    /// Returns `None` if:
    /// - The shared cache directory cannot be created
    /// - No commit key is available (not in a git repo)
    pub fn open(repo_root: &Path, size_cap_bytes: u64, history_len: usize) -> Option<Self> {
        Self::open_with_cache_dir(repo_root, size_cap_bytes, history_len, None)
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
        let cache_path = cache_dir
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

        Some(Self {
            paths,
            write_commit_key,
            candidate_keys,
            snapshot_store,
            index: OnceLock::new(),
            size_cap_bytes,
        })
    }

    #[must_use]
    pub fn paths(&self) -> &SharedCachePaths {
        &self.paths
    }

    /// Test-only constructor that accepts a pre-configured SnapshotStore with a load counter.
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
        candidates
            .into_iter()
            .filter_map(move |entry| Self::stage_entry(&entry, &paths, &package_dir))
    }

    /// Stage a single entry, returning a StagedCandidate for validation.
    fn stage_entry(
        entry: &SnapshotEntry,
        paths: &SharedCachePaths,
        package_dir: &Path,
    ) -> Option<StagedCandidate> {
        let staged = match restore_blob_with_meta(paths, &entry.outputs_hash, package_dir) {
            Ok(BlobReadResultWithMeta::Restored(staged)) => staged,
            Ok(BlobReadResultWithMeta::Missing) | Ok(BlobReadResultWithMeta::Corrupt) => {
                return None
            }
            Err(_) => return None,
        };

        // Decode record.
        let record: TaskRunRecord = match bincode::serde::decode_from_slice(
            &staged.meta.record,
            bincode::config::standard().with_fixed_int_encoding(),
        ) {
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
            let mut merged = MergedIndex::new();

            // Iterate in reverse order (oldest first) so that newest overwrites
            for commit_key in self.candidate_keys.iter().rev() {
                if let Some(snapshot) = self.snapshot_store.load(commit_key) {
                    for (input_key_hex, entry) in &snapshot.entries {
                        merged.insert_entry(
                            input_key_hex.clone(),
                            entry.clone(),
                            commit_key.clone(),
                        );
                    }
                    merged.snapshots.push((commit_key.clone(), snapshot));
                }
            }

            // Reverse snapshots so newest is first
            merged.snapshots.reverse();

            merged
        })
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
            Ok(OutputScope::Escape) => {
                // This branch shouldn't be reachable; Escape is signaled via Err.
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "output path escapes repository root",
                ));
            }
        }

        // Prepare meta files.
        let meta_record = bincode::serde::encode_to_vec(
            record,
            bincode::config::standard().with_fixed_int_encoding(),
        )
        .map_err(io::Error::other)?;

        let meta = MetaFiles {
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
            record: meta_record,
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

        match blob_result {
            BlobWriteResult::Written | BlobWriteResult::AlreadyExists => {
                // Merge entry into snapshot.
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

                let merge_result = self.snapshot_store.merge_entry(&write_key, entry);
                if matches!(merge_result, MergeResult::SkippedLockUnavailable) {
                    return Ok(StoreOutcome::SkippedLockUnavailable);
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

    fn bincode_config() -> impl bincode::config::Config {
        bincode::config::standard().with_fixed_int_encoding()
    }

    fn setup_git_repo(repo_root: &Path) {
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

    fn create_commit(repo_root: &Path) -> String {
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

    fn sample_record(succeeded: bool, duration_ms: u64) -> TaskRunRecord {
        let start = 1_000_000_000_000_u64;
        TaskRunRecord {
            schema_version: crate::record::SCHEMA_VERSION_V1,
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
                temp_repo.path(),
            )
            .unwrap();
        assert_eq!(result, StoreOutcome::Stored);

        // Restore into a fresh directory.
        let restore_dir = temp_repo.path().join("restore");
        fs::create_dir_all(&restore_dir).unwrap();

        // Use new try_restore_candidates API - commit first valid candidate
        let hit = cache
            .try_restore_candidates("pkg#build", &input_key, &restore_dir)
            .next()
            .expect("expected at least one candidate")
            .commit()
            .expect("commit should succeed");
        assert_eq!(hit.outputs_hash, [7; 32]);
        assert_eq!(hit.stdout, b"stdout output");
        assert_eq!(hit.stderr, b"stderr output");
        assert!(hit.record.succeeded);

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
        let hit = candidates
            .remove(0)
            .commit()
            .expect("commit should succeed");
        assert_eq!(hit.stdout, b"stdout v1");
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

    #[test]
    fn load_once_proven_via_counter() {
        use std::sync::atomic::Ordering;

        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        let commit1 = create_commit(temp_repo.path());
        let commit2 = create_commit(temp_repo.path());

        let temp_cache = TempDir::new().unwrap();

        // Manually create snapshot files BEFORE opening cache
        // This way merge_entry doesn't trigger load during test setup
        let input_key1 = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);
        let input_key2 = derive_input_key([5; 32], [6; 32], [7; 32], [8; 32]);

        // Create minimal snapshot files for both commits
        let snapshot_dir = temp_cache.path().join("snapshots");
        fs::create_dir_all(&snapshot_dir).unwrap();

        let mut snapshot1 = Snapshot::new();
        snapshot1.entries.insert(
            input_key_hex(input_key1),
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

        let mut snapshot2 = Snapshot::new();
        snapshot2.entries.insert(
            input_key_hex(input_key2),
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

        // Write snapshot files directly (no load during merge_entry)
        let encoded1 = bincode::serde::encode_to_vec(&snapshot1, bincode_config()).unwrap();
        let encoded2 = bincode::serde::encode_to_vec(&snapshot2, bincode_config()).unwrap();
        fs::write(snapshot_dir.join(format!("{commit1}.bincode")), &encoded1).unwrap();
        fs::write(snapshot_dir.join(format!("{commit2}.bincode")), &encoded2).unwrap();

        // Open cache with per-instance load counter
        let paths = open_shared_paths(temp_cache.path()).unwrap();
        let (snapshot_store, load_counter) = SnapshotStore::new_with_counter(paths);

        let cache =
            SharedCache::from_parts_for_test(temp_repo.path(), 1_000_000, 10, snapshot_store)
                .unwrap();

        // Call try_restore 50 times across different input_keys
        let restore_dir = temp_repo.path().join("restore");
        for i in 0..50 {
            fs::create_dir_all(&restore_dir).unwrap();
            // Alternate between input keys
            let input_key = if i % 2 == 0 { &input_key1 } else { &input_key2 };
            // Commit first candidate (if any) - just testing load-once behavior
            if let Some(candidate) = cache
                .try_restore_candidates("pkg#build", input_key, &restore_dir)
                .next()
            {
                let _ = candidate.commit();
            }
            // Clean restore dir for next iteration
            fs::remove_dir_all(&restore_dir).ok();
        }

        // Count snapshot files that exist in cache
        let snapshot_file_count = 2; // We know we wrote exactly 2

        // CRITICAL ASSERTION: Each snapshot file is loaded EXACTLY ONCE during index build.
        // If try_restore re-reads from disk, count would be 50 * snapshot_file_count.
        // With proper O(1) in-memory lookup, count must equal snapshot_file_count.
        assert_eq!(
            load_counter.load(Ordering::SeqCst),
            snapshot_file_count,
            "FAIL: Snapshot files reloaded from disk! Expected {} loads (once per file), got {}. \
             If try_restore re-reads on each call, count would be 100+.",
            snapshot_file_count,
            load_counter.load(Ordering::SeqCst)
        );

        // Index was initialized exactly once.
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
