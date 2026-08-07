pub(crate) mod atomicio;
pub mod blob;
mod discovery;
pub mod entry_meta;
pub mod gc;
pub mod paths;
#[cfg(unix)]
pub mod rclone;
#[cfg(unix)]
mod remote;
pub mod scope;
pub mod snapshot;

pub(crate) use atomicio::atomic_write;
pub use blob::{
    restore_blob, restore_blob_with_meta, restore_outputs_staged, write_blob_with_meta,
    write_outputs_blob, BlobReadResult, BlobReadResultWithMeta, BlobWriteResult, MetaFiles,
    StagedRestore,
};
pub use discovery::{
    bucket_key, bucket_keys_for, current_session_shard_key, new_session_shard_key,
    rank_shard_candidates, write_bucket_key, ShardCandidate, DEFAULT_SHARD_BYTE_BUDGET,
    DEFAULT_SHARD_MAX_AGE_MS, DEFAULT_SHARED_CACHE_DAY_WINDOW, SHARED_CACHE_SHARD_COUNT,
};
pub use entry_meta::{
    encode_entry_meta, entry_meta_path, read_entry_meta, write_entry_meta, EntryMeta,
    EntryMetaWriteResult, EntryReport, ENTRY_META_SCHEMA_VERSION,
};
pub use gc::{maybe_run_gc, run_gc, GcStats, DEFAULT_GC_RETENTION, DEFAULT_GC_THROTTLE};
pub use paths::{
    open_shared_paths, resolve_shared_cache_dir, SharedCachePaths, BLOBS_DIR_NAME,
    ENTRIES_DIR_NAME, SHARED_CACHE_DIR_ENV, SNAPSHOTS_DIR_NAME,
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
    Snapshot, SnapshotEntry, SnapshotStore, SnapshotUpload, SNAPSHOT_SCHEMA_VERSION,
};
use std::collections::HashSet;
#[cfg(unix)]
use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

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
    /// A candidate with no output files. Commits to an empty path list.
    pub fn empty_outputs(
        outputs_hash: [u8; 32],
        record: TaskRunRecord,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        reports: Vec<crate::store::ReportInput>,
        package_dir: &Path,
    ) -> std::io::Result<Self> {
        Ok(Self {
            outputs_hash,
            record,
            stdout,
            stderr,
            reports,
            staged: blob::StagedRestore::empty(package_dir)?,
        })
    }

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
///
/// Just a presence set: `try_restore_candidates` only asks "did *any* candidate
/// ever record this input_key as cacheable" (`contains`). The actual record,
/// outputs_hash, and streams for a hit are resolved afterward from the single
/// `entries/<input_key>` object (see `stage_entry`), not from anything carried
/// here — so there is nothing to arbitrate between shards that both mention the
/// same input_key. An earlier version stored `(SnapshotEntry, String)` per key
/// and picked a "winner" on conflict; that payload had no reader outside tests
/// and its comparator (`cached_at_unix_ms`, a wall clock from whichever machine
/// wrote the entry) was unsound across machines with skewed clocks. Removed
/// rather than kept and documented, since Task 9 would otherwise inherit it
/// silently when merging remote listings into this same index.
#[derive(Debug, Clone)]
pub struct MergedIndex {
    /// Set of input_key_hex values recorded as cacheable by some candidate shard.
    entries: HashSet<String>,
}

impl MergedIndex {
    fn new() -> Self {
        Self {
            entries: HashSet::new(),
        }
    }

    fn insert_entry(&mut self, input_key_hex: String) {
        self.entries.insert(input_key_hex);
    }
}

/// Facade for the shared cache, composing blobs and snapshots.
#[derive(Debug)]
pub struct SharedCache {
    /// Resolved paths for the cache.
    paths: Arc<SharedCachePaths>,
    /// Bucket key this process writes its entries under: today's UTC date and
    /// a nonce-selected shard (see `discovery::write_bucket_key`).
    write_bucket_key: Option<String>,
    /// Number of days of history `candidate_keys()` reads, newest first. Each
    /// day contributes `SHARED_CACHE_SHARD_COUNT` bucket keys, so the read set
    /// has exactly `day_window * SHARED_CACHE_SHARD_COUNT` keys — computed,
    /// not discovered.
    day_window: usize,
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
            // Flush queued synchronous remote pushes before stopping the rclone
            // daemon. The queue worker is a plain OS thread, so joining it here
            // is safe even if Drop runs inside build runtime async context.
            remote.flush_push_queue();
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
    /// Returns `None` if the shared cache directory cannot be created.
    pub fn open(repo_root: &Path, size_cap_bytes: u64, day_window: usize) -> Option<Self> {
        Self::open_with_remote(repo_root, size_cap_bytes, day_window, OpenExtras::default())
    }

    /// Opens the shared cache with an optional explicit cache directory.
    ///
    /// If `cache_dir` is provided, uses it directly instead of resolving
    /// from environment/platform defaults. This is useful for testing.
    pub fn open_with_cache_dir(
        repo_root: &Path,
        size_cap_bytes: u64,
        day_window: usize,
        cache_dir: Option<&Path>,
    ) -> Option<Self> {
        Self::open_with_remote(
            repo_root,
            size_cap_bytes,
            day_window,
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
        day_window: usize,
        extras: OpenExtras<'_>,
    ) -> Option<Self> {
        let _ = repo_root;
        let cache_path = extras
            .cache_dir
            .map(|p| p.to_path_buf())
            .unwrap_or_else(resolve_shared_cache_dir);
        let paths = Arc::new(open_shared_paths(&cache_path).ok()?);

        let write_bucket_key = Some(discovery::current_write_bucket_key());

        let snapshot_store = SnapshotStore::new((*paths).clone());
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
            write_bucket_key,
            day_window,
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
        day_window: usize,
        snapshot_store: SnapshotStore,
    ) -> Option<Self> {
        let _ = repo_root;
        let paths = snapshot_store.paths().clone();

        let write_bucket_key = Some(discovery::current_write_bucket_key());

        Some(Self {
            paths: Arc::new(paths),
            write_bucket_key,
            day_window,
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
    /// 2. O(1) lookup by input_key in merged index — no disk read.
    /// 3. If found, stage from `entries/<input_key>` (and its outputs blob, if
    ///    any) and return a StagedCandidate for validation.
    /// 4. Caller validates candidate by calling `validate()` with a FileStateResolver.
    /// 5. If valid, caller calls `commit()` to move files into package_dir.
    ///
    /// There is at most one candidate per input_key: `stage_entry` resolves
    /// everything (record, outputs_hash, stdout/stderr) from the single
    /// `entries/<input_key>` object, so trying a different `SnapshotEntry` for
    /// the same input_key across commits can never produce a different
    /// result. A blob that's been GC'd is a rebuild, not a fallback.
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

        // O(1) lookup in the merged index — NO disk read. Only gates whether
        // any snapshot ever recorded this input_key as cacheable.
        let candidate = index.entries.contains(&input_key_hex).then_some(*input_key);

        let paths = self.paths.clone();
        let package_dir = package_dir.to_path_buf();
        #[cfg(unix)]
        let remote = self.remote.clone();
        candidate.into_iter().filter_map(move |input_key| {
            Self::stage_entry(
                &input_key,
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
    ///
    /// Two-phase: fetch the small `entries/<input_key>` object first and decode
    /// the record from it. Only pull the outputs blob if the entry actually has
    /// outputs. A candidate rejected by `decide_shared_restore` therefore never
    /// costs an outputs download.
    fn stage_entry(
        input_key: &[u8; 32],
        paths: &SharedCachePaths,
        package_dir: &Path,
        #[cfg(unix)] remote: Option<&RemoteSync>,
    ) -> Option<StagedCandidate> {
        #[cfg(unix)]
        if read_entry_meta(paths, input_key).is_none() {
            if let Some(remote) = remote {
                if let Err(err) = remote.pull_entry_meta(paths, input_key) {
                    eprintln!(
                        "debug: remote entry meta pull failed for input_key={}: {err}",
                        hex_hash(*input_key)
                    );
                }
            }
        }

        let meta = read_entry_meta(paths, input_key)?;

        let record: TaskRunRecord =
            match bincode::serde::decode_from_slice(&meta.record, bincode_config()) {
                Ok((record, _)) => record,
                Err(_) => return None,
            };

        let reports: Vec<crate::store::ReportInput> = meta
            .reports
            .into_iter()
            .map(crate::store::ReportInput::from)
            .collect();

        if !meta.has_outputs {
            return StagedCandidate::empty_outputs(
                meta.outputs_hash,
                record,
                meta.stdout,
                meta.stderr,
                reports,
                package_dir,
            )
            .ok();
        }

        if !blob_path(paths, &meta.outputs_hash).is_file() {
            #[cfg(unix)]
            if let Some(remote) = remote {
                if let Err(err) = remote.pull_blob(paths, &meta.outputs_hash) {
                    eprintln!(
                        "debug: remote blob pull failed for outputs_hash={}: {err}",
                        hex_hash(meta.outputs_hash)
                    );
                }
            }
        }

        let staged = match restore_outputs_staged(paths, &meta.outputs_hash, package_dir) {
            Ok(BlobReadResultWithMeta::Restored(staged)) => staged,
            Ok(BlobReadResultWithMeta::Missing) | Ok(BlobReadResultWithMeta::Corrupt) => {
                return None
            }
            Err(_) => return None,
        };

        Some(StagedCandidate {
            outputs_hash: meta.outputs_hash,
            record,
            stdout: meta.stdout,
            stderr: meta.stderr,
            reports,
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
        // Computed, not discovered: entries this same process just wrote (its
        // own write bucket, or a bucket a test wrote directly) always fall
        // inside `candidate_keys()` by construction (see
        // `discovery::write_bucket_key`), even before their directory exists
        // on disk. So unlike the old discovery-based scheme, there's no
        // ordering requirement between `open()` and this first build.
        let candidate_keys = self.candidate_keys();

        #[cfg(unix)]
        self.pull_candidate_commits(remote, &candidate_keys);

        let mut merged = MergedIndex::new();
        for commit_key in &candidate_keys {
            self.load_commit_into_index(
                &mut merged,
                commit_key,
                #[cfg(unix)]
                remote,
            );
        }

        merged
    }

    /// Roll the currently-discoverable shards into one merged shard.
    ///
    /// Unreferenced as of the switch to computed bucket keys: with a fixed,
    /// small read set (`day_window * SHARED_CACHE_SHARD_COUNT` keys) there's
    /// no unbounded shard sprawl for a rollup to bound. Kept in place rather
    /// than deleted here so the diff that introduces computed buckets stays
    /// reviewable separately from the deletion of what they replace (see the
    /// module doc on `discovery`). A later task removes it.
    #[allow(dead_code)]
    fn maybe_write_rollup(&self, keys: &[String], candidates_since_last_rollup: usize) {
        if keys.len() < 2
            || !gc::should_run_rollup(
                &self.paths,
                gc::DEFAULT_GC_THROTTLE,
                self.day_window,
                candidates_since_last_rollup,
            )
        {
            return;
        }
        let rollup_key = current_session_shard_key();
        let Some(upload) = self.snapshot_store.write_rollup_shard(keys, &rollup_key) else {
            return;
        };
        self.maybe_push_rollup_upload(rollup_key, upload);
    }

    #[cfg(unix)]
    #[allow(dead_code)]
    fn maybe_push_rollup_upload(&self, rollup_key: String, upload: SnapshotUpload) {
        let Some(remote) = &self.remote else {
            return;
        };
        if remote.is_disabled() {
            return;
        }
        remote.enqueue_push_snapshot_upload(rollup_key, upload);
    }

    #[cfg(not(unix))]
    #[allow(dead_code)]
    fn maybe_push_rollup_upload(&self, _rollup_key: String, _upload: SnapshotUpload) {}

    #[cfg(unix)]
    fn pull_candidate_commits(&self, remote: Option<&RemoteSync>, candidate_keys: &[String]) {
        let Some(remote) = remote.cloned() else {
            return;
        };
        Self::run_candidate_pulls_on_dedicated_thread(
            remote,
            self.snapshot_store.clone(),
            candidate_keys.to_vec(),
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
        for input_key_hex in snapshot.entries.keys() {
            merged.insert_entry(input_key_hex.clone());
        }
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
    /// - Snapshot entry via merge_entry to write_bucket_key
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
        let write_key = match &self.write_bucket_key {
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

        // Prepare per-entry meta. Keyed by input_key, never by outputs_hash.
        let meta_record =
            bincode::serde::encode_to_vec(record, bincode_config()).map_err(io::Error::other)?;

        // `has_outputs` is provisional here — the size-cap estimate below
        // doesn't depend on its value (encoding a bool costs the same byte
        // either way) — and is corrected from `blob_result` once we know
        // whether `write_outputs_blob` actually wrote a blob.
        let mut meta = EntryMeta {
            schema_version: ENTRY_META_SCHEMA_VERSION,
            outputs_hash: *outputs_hash,
            has_outputs: !rel_output_paths.is_empty(),
            record: meta_record,
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
            reports: reports.iter().map(EntryReport::from).collect(),
        };

        // Meta counts toward the size cap, same as when it lived in the blob.
        let meta_bytes = encode_entry_meta(&meta)?.len() as u64;
        let output_bytes: u64 = record.outputs.iter().map(|file| file.size).sum();
        let total_bytes = output_bytes.saturating_add(meta_bytes);
        if total_bytes > self.size_cap_bytes {
            return Ok(StoreOutcome::SkippedTooLarge { bytes: total_bytes });
        }

        let blob_result = write_outputs_blob(
            &self.paths,
            outputs_hash,
            package_dir,
            rel_output_paths,
            self.size_cap_bytes,
        )?;

        // Don't leave meta pointing at a blob that was never written.
        if let BlobWriteResult::SkippedTooLarge { bytes } = blob_result {
            return Ok(StoreOutcome::SkippedTooLarge { bytes });
        }

        // Authoritative: a declared output can still be absent on disk, in
        // which case `write_outputs_blob` returns `NoOutputs` even though
        // `record.outputs` is non-empty. Restore must trust this bit, not
        // re-derive it from the record.
        meta.has_outputs = !matches!(blob_result, BlobWriteResult::NoOutputs);

        write_entry_meta(&self.paths, input_key, &meta)?;

        let entry = SnapshotEntry {
            task_id: task_id.to_string(),
            input_key: *input_key,
            outputs_hash: *outputs_hash,
            task_spec_hash: record.task_spec_hash,
            env_hash: record.env_hash,
            pkg_dep_hash: record.pkg_dep_hash,
            duration_ms,
            output_bytes,
            cached_at_unix_ms: record.end_unix_ms,
            tool_version: None,
        };
        self.finish_store(
            blob_result,
            &write_key,
            #[cfg(unix)]
            input_key,
            #[cfg(unix)]
            meta.has_outputs,
            entry,
        )
    }

    /// Records the snapshot entry and pushes to the remote after a blob write.
    fn finish_store(
        &self,
        blob_result: BlobWriteResult,
        write_key: &str,
        #[cfg(unix)] input_key: &[u8; 32],
        #[cfg(unix)] has_outputs: bool,
        entry: SnapshotEntry,
    ) -> io::Result<StoreOutcome> {
        match blob_result {
            // NoOutputs is a success: the entry meta is what makes it restorable.
            BlobWriteResult::Written
            | BlobWriteResult::AlreadyExists
            | BlobWriteResult::NoOutputs => {
                #[cfg(unix)]
                let outputs_hash = entry.outputs_hash;
                let merge = self
                    .snapshot_store
                    .merge_entry_with_outcome(write_key, entry);
                if matches!(merge.result, MergeResult::SkippedLockUnavailable) {
                    return Ok(StoreOutcome::SkippedLockUnavailable);
                }
                #[cfg(unix)]
                self.enqueue_remote_push(write_key, outputs_hash, *input_key, has_outputs, merge);
                Ok(StoreOutcome::Stored)
            }
            BlobWriteResult::SkippedTooLarge { bytes } => {
                Ok(StoreOutcome::SkippedTooLarge { bytes })
            }
        }
    }

    #[cfg(unix)]
    fn enqueue_remote_push(
        &self,
        write_key: &str,
        outputs_hash: [u8; 32],
        input_key: [u8; 32],
        has_outputs: bool,
        merge: MergeEntryOutcome,
    ) {
        let Some(remote) = &self.remote else {
            return;
        };
        if remote.is_disabled() {
            return;
        }
        remote.enqueue_push_store_artifacts(remote::OwnedPushArtifacts {
            paths: Arc::clone(&self.paths),
            commit_key: write_key.to_string(),
            outputs_hash,
            input_key,
            has_outputs,
            merge,
        });
    }

    #[cfg(any(test, doctest))]
    pub(crate) fn flush_push_queue(&self) {
        if let Some(remote) = &self.remote {
            remote.drain_push_queue();
        }
    }

    /// Returns the write bucket key for this cache.
    #[must_use]
    pub fn write_bucket_key(&self) -> Option<&str> {
        self.write_bucket_key.as_deref()
    }

    /// Computes the candidate bucket keys for this cache, newest day first.
    ///
    /// Computed, not discovered: no directory listing, no remote listing,
    /// no ranking. The read set is exactly `bucket_keys_for(now, day_window)`
    /// — `day_window * SHARED_CACHE_SHARD_COUNT` keys, always. `write_bucket_key`
    /// is always inside this set by construction (it's today's date, and
    /// today's shards are always in the window), so unlike the old
    /// discovery-based scheme there's no need to special-case injecting the
    /// write key: see `discovery::tests::write_bucket_is_always_inside_the_read_set`.
    #[must_use]
    pub fn candidate_keys(&self) -> Vec<String> {
        discovery::bucket_keys_for(discovery::now_unix_ms(), self.day_window)
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
            schema_version: crate::record::SCHEMA_VERSION_V5,
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
    fn legacy_blob_with_embedded_meta_restores_outputs_only() {
        // Blobs written by pre-Task-4 clients still embed a `.luchta-meta/`
        // directory (see `write_blob_with_meta`). Restore must still extract
        // their output files correctly, and must not leak that embedded meta
        // into the restored package directory: `entries/<input_key>` is
        // authoritative for the record/stdout/stderr, and
        // `move_non_meta_files` filters `.luchta-meta` out on commit.
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());

        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            5,
            Some(temp_cache.path()),
        )
        .unwrap();

        let package_dir = temp_repo.path().join("pkg");
        fs::create_dir_all(package_dir.join("dist")).unwrap();
        fs::write(package_dir.join("dist/main.js"), "v1").unwrap();

        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);
        let record = sample_record(true, 200);

        let entry = SnapshotEntry {
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
        // Must land in a computed bucket key, not an arbitrary string (e.g. a
        // git commit hash): `candidate_keys()` only ever asks for keys
        // `bucket_keys_for` computes, so anything else is never read back.
        // `write_bucket_key()` is guaranteed to be one of them by construction.
        let write_key = cache.write_bucket_key().unwrap().to_string();
        cache.snapshot_store.merge_entry(&write_key, entry);

        // Legacy blob: meta embedded via write_blob_with_meta (pre-Task-2 format).
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

        // entries/<input_key> is what the two-phase restore path actually
        // reads; the meta embedded in the blob above is ignored.
        write_entry_meta(
            &cache.paths,
            &input_key,
            &EntryMeta {
                schema_version: ENTRY_META_SCHEMA_VERSION,
                outputs_hash: [7; 32],
                has_outputs: true,
                record: meta.record.clone(),
                stdout: meta.stdout.clone(),
                stderr: meta.stderr.clone(),
                reports: vec![],
            },
        )
        .unwrap();

        let restore_dir = temp_repo.path().join("restore");
        fs::create_dir_all(&restore_dir).unwrap();

        let (hit, written_paths) = cache
            .try_restore_candidates("pkg#build", &input_key, &restore_dir)
            .next()
            .expect("expected a candidate")
            .commit()
            .expect("commit should succeed");

        assert_eq!(hit.stdout, b"stdout v1");
        assert_eq!(written_paths, vec![restore_dir.join("dist/main.js")]);
        assert_eq!(fs::read(restore_dir.join("dist/main.js")).unwrap(), b"v1");
        assert!(
            !restore_dir.join(".luchta-meta").exists(),
            "embedded legacy meta must be filtered out on commit"
        );
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
        create_commit(temp_repo.path());
        let temp_cache = TempDir::new().unwrap();
        let snapshot_dir = temp_cache.path().join("snapshots");
        let input_key1 = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);
        let input_key2 = derive_input_key([5; 32], [6; 32], [7; 32], [8; 32]);

        // Keys must be valid computed bucket keys (`<YYYYMMDD>-<shard>`), not
        // arbitrary strings: `candidate_keys()` no longer discovers whatever
        // directories happen to exist under `snapshots/`, it only ever asks
        // for the keys `bucket_keys_for` computes. Two different shards of
        // "today" both fall inside the default read window.
        let now = discovery::now_unix_ms();
        let bucket1 = discovery::bucket_key(now, 0);
        let bucket2 = discovery::bucket_key(now, 1);

        write_snapshot_fixture(
            &snapshot_dir,
            &bucket1,
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
            &bucket2,
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
        // No rollup pre-stamp needed here: `build_index` no longer runs a
        // rollup pass at all under computed bucket keys (see
        // `maybe_write_rollup`'s doc), so there's nothing that would re-read
        // either fixture a second time on top of the load this test counts.
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

    fn shard_dir_count(cache_dir: &Path) -> usize {
        fs::read_dir(cache_dir.join("snapshots"))
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.path().is_dir())
                    .count()
            })
            .unwrap_or(0)
    }

    #[test]
    fn shard_count_pressure_rollup_does_not_retrigger_without_fresh_churn() {
        // Directly seed enough distinct local shards to exceed the pressure
        // threshold in one discovery pass, bypassing `store()` so this stays
        // focused on discovery/rollup behavior rather than the store path.
        // `history_len` here (100, passed to `open_with_cache_dir` below) is
        // deliberately non-default so the pressure threshold has to be
        // derived from it, not the hardcoded value for the default cap of 20.
        let temp_cache = TempDir::new().unwrap();
        let history_len = 100;
        let seed_count = gc::rollup_pressure_threshold(history_len) + 1;
        for i in 0..seed_count {
            let paths = open_shared_paths(temp_cache.path()).unwrap();
            let store = SnapshotStore::new(paths);
            let seed = i as u8;
            store.merge_entry(
                &format!("{i:013}-seed"),
                SnapshotEntry {
                    task_id: format!("pkg#seed-{i}"),
                    input_key: derive_input_key([seed; 32], [9; 32], [9; 32], [9; 32]),
                    outputs_hash: [0; 32],
                    task_spec_hash: [seed; 32],
                    env_hash: [9; 32],
                    pkg_dep_hash: [9; 32],
                    duration_ms: 200,
                    output_bytes: 0,
                    cached_at_unix_ms: 1,
                    tool_version: None,
                },
            );
        }

        let before_first_run = shard_dir_count(temp_cache.path());
        assert_eq!(before_first_run, seed_count, "sanity: all seeds landed");

        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());

        // First "run": discovery should see shard-count pressure and fire a
        // rollup, adding one new shard directory.
        {
            let cache = SharedCache::open_with_cache_dir(
                temp_repo.path(),
                1_000_000,
                history_len,
                Some(temp_cache.path()),
            )
            .unwrap();
            let restore_dir = temp_repo.path().join("restore-first");
            fs::create_dir_all(&restore_dir).unwrap();
            let probe_key = derive_input_key([254; 32], [8; 32], [8; 32], [8; 32]);
            let _ = cache
                .try_restore_candidates("pkg#probe", &probe_key, &restore_dir)
                .next();
        }

        let after_first_run = shard_dir_count(temp_cache.path());
        assert_eq!(
            after_first_run,
            before_first_run + 1,
            "a rollup should have added exactly one new shard directory"
        );

        // Second "run": no new shards since the rollup. Discovery must not
        // fire a second one -- without the reset, a naive
        // discovered-count-since-forever check would retrigger on every
        // single call once churn crossed the threshold, since rollups never
        // delete their sources.
        {
            let cache = SharedCache::open_with_cache_dir(
                temp_repo.path(),
                1_000_000,
                history_len,
                Some(temp_cache.path()),
            )
            .unwrap();
            let restore_dir = temp_repo.path().join("restore-second");
            fs::create_dir_all(&restore_dir).unwrap();
            let probe_key = derive_input_key([253; 32], [8; 32], [8; 32], [8; 32]);
            let _ = cache
                .try_restore_candidates("pkg#probe", &probe_key, &restore_dir)
                .next();
        }

        let after_second_run = shard_dir_count(temp_cache.path());
        assert_eq!(
            after_second_run, after_first_run,
            "no new rollup should fire without fresh churn since the last one"
        );
    }

    #[test]
    fn shard_count_pressure_rollup_keeps_an_old_pack_reachable_through_heavy_local_churn() {
        // The motivating #277 scenario: an older shard ("the pack") plus
        // enough newer, tiny local shards to exceed the shard-count `limit`
        // on their own. Without the pressure trigger, the count cap alone
        // would evict the pack before any rollup got a chance to fold it in.
        //
        // Still passes under computed bucket keys, but no longer for the
        // reason described above: `build_index` no longer calls
        // `maybe_write_rollup` at all (see its doc), so no rollup ever fires
        // here. The assertion holds instead because every write in this test
        // happens "today", and today's shards are always in the computed
        // read window regardless of how many local shard directories pile
        // up — the shard-count cap this test was probing doesn't exist
        // anymore. Left in place because it still exercises a real
        // production path (`store()` / `try_restore_candidates()`) under
        // heavy churn; the doc comment above is kept for history, not as a
        // claim about current behavior.
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());
        let temp_cache = TempDir::new().unwrap();

        let package_dir = temp_repo.path().join("pkg");
        fs::create_dir_all(&package_dir).unwrap();
        let empty_hash = crate::resolve::combined_outputs_hash(&[]);
        let mut record = sample_record(true, 200);
        record.output_patterns = vec![];
        record.outputs = vec![];
        record.outputs_hash = empty_hash;

        const HISTORY_LEN: usize = 20;

        let old_input_key = derive_input_key([88; 32], [1; 32], [1; 32], [1; 32]);
        {
            let cache = SharedCache::open_with_cache_dir(
                temp_repo.path(),
                1_000_000,
                HISTORY_LEN,
                Some(temp_cache.path()),
            )
            .unwrap();
            cache
                .store(
                    "pkg#old",
                    &old_input_key,
                    &empty_hash,
                    &package_dir,
                    &[],
                    &record,
                    b"",
                    b"",
                    &[],
                    temp_repo.path(),
                )
                .unwrap();
        }

        // 21 more "runs": each writes one fresh shard, then triggers
        // discovery, simulating local churn past both the pressure
        // threshold (15) and the shard-count cap (20). Kept to the minimum
        // that clears both bars rather than a rounder, larger number: each
        // iteration opens a fresh `SharedCache` and does real disk I/O, and
        // this test doesn't need more churn than that to prove the property.
        for i in 0..21u8 {
            let cache = SharedCache::open_with_cache_dir(
                temp_repo.path(),
                1_000_000,
                HISTORY_LEN,
                Some(temp_cache.path()),
            )
            .unwrap();
            let churn_input_key = derive_input_key([i; 32], [2; 32], [2; 32], [2; 32]);
            cache
                .store(
                    "pkg#churn",
                    &churn_input_key,
                    &empty_hash,
                    &package_dir,
                    &[],
                    &record,
                    b"",
                    b"",
                    &[],
                    temp_repo.path(),
                )
                .unwrap();
            let restore_dir = temp_repo.path().join(format!("restore-{i}"));
            fs::create_dir_all(&restore_dir).unwrap();
            let _ = cache
                .try_restore_candidates("pkg#churn", &churn_input_key, &restore_dir)
                .next();
        }

        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            HISTORY_LEN,
            Some(temp_cache.path()),
        )
        .unwrap();
        let restore_dir = temp_repo.path().join("restore-final");
        fs::create_dir_all(&restore_dir).unwrap();
        assert!(
            cache
                .try_restore_candidates("pkg#old", &old_input_key, &restore_dir)
                .next()
                .is_some(),
            "the old pack's entry should survive heavy local churn once shard-count pressure rolls it up"
        );
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

    #[test]
    fn two_no_output_tasks_keep_separate_meta() {
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

        let empty_hash = crate::resolve::combined_outputs_hash(&[]);

        let pkg_a = temp_repo.path().join("pkg-a");
        let pkg_b = temp_repo.path().join("pkg-b");
        fs::create_dir_all(&pkg_a).unwrap();
        fs::create_dir_all(&pkg_b).unwrap();

        let mut record_a = sample_record(true, 200);
        record_a.output_patterns = vec![];
        record_a.outputs = vec![];
        record_a.outputs_hash = empty_hash;
        record_a.task_spec_hash = [11; 32];
        let key_a = derive_input_key([11; 32], [2; 32], [3; 32], [4; 32]);

        let mut record_b = record_a.clone();
        record_b.task_spec_hash = [22; 32];
        let key_b = derive_input_key([22; 32], [2; 32], [3; 32], [4; 32]);

        cache
            .store(
                "pkg-a#lint",
                &key_a,
                &empty_hash,
                &pkg_a,
                &[],
                &record_a,
                b"A stdout",
                b"",
                &[],
                temp_repo.path(),
            )
            .unwrap();
        cache
            .store(
                "pkg-b#lint",
                &key_b,
                &empty_hash,
                &pkg_b,
                &[],
                &record_b,
                b"B stdout",
                b"",
                &[],
                temp_repo.path(),
            )
            .unwrap();

        let meta_a = read_entry_meta(cache.paths(), &key_a).expect("meta for A");
        let meta_b = read_entry_meta(cache.paths(), &key_b).expect("meta for B");

        assert_eq!(meta_a.stdout, b"A stdout");
        assert_eq!(meta_b.stdout, b"B stdout");

        assert!(
            !blob_path(cache.paths(), &empty_hash).exists(),
            "no outputs means no blob file"
        );

        // NoOutputs is a success path: the entry must still be indexed.
        let write_key = cache.write_bucket_key().expect("write key");
        let snapshot = cache
            .snapshot_store
            .load(write_key)
            .expect("snapshot should exist for no-output entries");
        assert_eq!(snapshot.entries.len(), 2, "both no-output entries indexed");
        assert!(
            snapshot.entries.contains_key(&input_key_hex(key_a)),
            "entry A should be recorded in the snapshot"
        );
        assert!(
            snapshot.entries.contains_key(&input_key_hex(key_b)),
            "entry B should be recorded in the snapshot"
        );
    }

    #[test]
    fn restore_reads_meta_from_entries_not_from_blob() {
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
        fs::write(package_dir.join("dist/main.js"), "console.log('hi');").unwrap();

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
                b"stdout output",
                b"stderr output",
                &[],
                temp_repo.path(),
            )
            .unwrap();

        let restore_dir = temp_repo.path().join("restore");
        fs::create_dir_all(&restore_dir).unwrap();

        let (hit, written_paths) = cache
            .try_restore_candidates("pkg#build", &input_key, &restore_dir)
            .next()
            .expect("expected at least one candidate")
            .commit()
            .expect("commit should succeed");

        assert_eq!(hit.outputs_hash, [7; 32]);
        assert_eq!(hit.stdout, b"stdout output");
        assert_eq!(hit.stderr, b"stderr output");
        assert_eq!(written_paths, vec![restore_dir.join("dist/main.js")]);
        assert_eq!(
            fs::read(restore_dir.join("dist/main.js")).unwrap(),
            b"console.log('hi');"
        );
        assert!(!restore_dir.join(".luchta-meta").exists());
    }

    #[test]
    fn no_output_task_restores_without_touching_a_blob() {
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

        let empty_hash = crate::resolve::combined_outputs_hash(&[]);
        let package_dir = temp_repo.path().join("pkg");
        fs::create_dir_all(&package_dir).unwrap();

        let mut record = sample_record(true, 200);
        record.output_patterns = vec![];
        record.outputs = vec![];
        record.outputs_hash = empty_hash;
        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);

        cache
            .store(
                "pkg#lint",
                &input_key,
                &empty_hash,
                &package_dir,
                &[],
                &record,
                b"lint output",
                b"",
                &[],
                temp_repo.path(),
            )
            .unwrap();

        let restore_dir = temp_repo.path().join("restore");
        fs::create_dir_all(&restore_dir).unwrap();

        let (hit, written_paths) = cache
            .try_restore_candidates("pkg#lint", &input_key, &restore_dir)
            .next()
            .expect("expected a candidate for a no-output task")
            .commit()
            .expect("commit should succeed");

        assert_eq!(hit.stdout, b"lint output");
        assert!(
            written_paths.is_empty(),
            "nothing to write for a no-output task"
        );
    }

    #[test]
    fn absent_declared_output_restores_from_meta_without_blob() {
        // A task can declare an output path and still not produce it: its
        // FileEntry has `absent: true`, so `record.outputs` is non-empty even
        // though nothing was written to disk. The caller filters absent
        // entries out of `rel_output_paths` before calling `store`, so
        // `write_outputs_blob` sees no existing files and returns `NoOutputs`
        // — no blob is written. Restore must trust `EntryMeta::has_outputs`
        // (set from that `NoOutputs` result), not re-derive "has outputs"
        // from `record.outputs.is_empty()`, which is false here.
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

        let mut record = sample_record(true, 200);
        record.outputs = vec![FileEntry {
            path: "dist/missing.js".to_string(),
            size: 0,
            mtime_ns: 0,
            hash: [0; 32],
            absent: true,
        }];
        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);

        let result = cache
            .store(
                "pkg#build",
                &input_key,
                &[7; 32],
                &package_dir,
                &[], // the declared output was never produced: no path to archive
                &record,
                b"stdout output",
                b"",
                &[],
                temp_repo.path(),
            )
            .unwrap();
        assert_eq!(result, StoreOutcome::Stored);
        assert!(
            !blob_path(cache.paths(), &[7; 32]).exists(),
            "no blob should be written when the only declared output is absent"
        );

        let restore_dir = temp_repo.path().join("restore");
        fs::create_dir_all(&restore_dir).unwrap();

        let (hit, written_paths) = cache
            .try_restore_candidates("pkg#build", &input_key, &restore_dir)
            .next()
            .expect("a non-empty record.outputs must not block restore when has_outputs is false")
            .commit()
            .expect("commit should succeed");

        assert_eq!(hit.stdout, b"stdout output");
        assert!(
            written_paths.is_empty(),
            "nothing to write when the declared output was never produced"
        );
    }

    #[test]
    fn entries_written_by_separate_runs_are_both_found_by_a_later_run() {
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());
        let temp_cache = TempDir::new().unwrap();

        let package_dir = temp_repo.path().join("pkg");
        fs::create_dir_all(&package_dir).unwrap();

        // Two SharedCache instances over one cache dir, as two separate
        // `luchta run` invocations would be. Each picks its own write shard.
        let key_a = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);
        let key_b = derive_input_key([5; 32], [6; 32], [7; 32], [8; 32]);

        for (task, key, spec) in [("pkg#a", key_a, [1u8; 32]), ("pkg#b", key_b, [5u8; 32])] {
            let cache = SharedCache::open_with_cache_dir(
                temp_repo.path(),
                1_000_000,
                3,
                Some(temp_cache.path()),
            )
            .unwrap();
            let mut record = sample_record(true, 200);
            record.output_patterns = vec![];
            record.outputs = vec![];
            record.outputs_hash = crate::resolve::combined_outputs_hash(&[]);
            record.task_spec_hash = spec;
            cache
                .store(
                    task,
                    &key,
                    &record.outputs_hash,
                    &package_dir,
                    &[],
                    &record,
                    b"out",
                    b"",
                    &[],
                    temp_repo.path(),
                )
                .unwrap();
        }

        // A third instance must see both, regardless of which shards they landed in.
        let reader = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            3,
            Some(temp_cache.path()),
        )
        .unwrap();
        let restore_dir = temp_repo.path().join("restore");
        fs::create_dir_all(&restore_dir).unwrap();

        assert!(
            reader
                .try_restore_candidates("pkg#a", &key_a, &restore_dir)
                .next()
                .is_some(),
            "entry from the first run must be discoverable"
        );
        assert!(
            reader
                .try_restore_candidates("pkg#b", &key_b, &restore_dir)
                .next()
                .is_some(),
            "entry from the second run must be discoverable"
        );
    }
}
