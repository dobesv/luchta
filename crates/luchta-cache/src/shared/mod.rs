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
    bucket_key, bucket_keys_for, write_bucket_key, DEFAULT_SHARED_CACHE_DAY_WINDOW,
    SHARED_CACHE_SHARD_COUNT,
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
#[cfg(unix)]
use std::collections::VecDeque;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

#[cfg(unix)]
use tokio::task::JoinSet;

use crate::record::TaskRunRecord;
use crate::serialization::bincode_config;

/// Reserved prefix for metadata files inside blobs.
/// Tasks faster than this aren't worth a shared-cache round trip: the store,
/// and the restore it would enable, cost more than just running the task.
pub const DEFAULT_MIN_STORE_DURATION_MS: u64 = 100;

/// Minimum task duration that qualifies for a shared-cache store, overridable
/// with `LUCHTA_SHARED_CACHE_MIN_DURATION_MS`.
///
/// The override exists because the default makes "this task is too fast to
/// cache" untestable by wall clock: a test that needs a task to finish inside
/// 100ms is really asserting the machine isn't busy, and fails under CPU
/// oversubscription while looking exactly like a cache regression (#290).
/// Setting the threshold instead of racing it makes those tests deterministic.
///
/// Read once — the value can't change within a run, and `store()` consults it
/// per task.
fn min_store_duration_ms() -> u64 {
    static MIN_DURATION_MS: OnceLock<u64> = OnceLock::new();
    *MIN_DURATION_MS.get_or_init(|| {
        std::env::var("LUCHTA_SHARED_CACHE_MIN_DURATION_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_MIN_STORE_DURATION_MS)
    })
}

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
    /// Skipped: shared cache disabled (no write bucket key).
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
    ///
    /// Clamped to at least `discovery::MIN_SHARED_CACHE_DAY_WINDOW` (2) at
    /// construction, not left to the caller: `write_bucket_key` is computed
    /// once, at `open()`, but the read set is recomputed fresh on every
    /// `candidate_keys()` call (including the first `build_index()`, which
    /// can run arbitrarily later). If a UTC midnight falls in between, the
    /// write key is *yesterday's* date — still inside the read set only
    /// because the window covers at least two days. A `day_window` of 1
    /// (or, without this clamp, 0 — reachable through this library's own
    /// `open()`/`open_with_cache_dir()`/`open_with_remote()`, which take a
    /// bare `usize` with no floor of their own; only the CLI's env parsing
    /// guards against it) would make that midnight race silently drop the
    /// process's own just-stored entries from its own later restores.
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
    /// Entries accumulated this run for the single end-of-run index merge,
    /// keyed by `input_key` so repeat writes of the same key collapse to one
    /// entry. Two callers feed this same map:
    ///
    /// - `finish_store`, right after a fresh store (a miss). This run's own
    ///   artifacts were just pushed immediately by `enqueue_entry_artifacts`,
    ///   so this entry needs no remote catch-up at flush time.
    /// - `refresh_entry`, right after a cache hit. This entry's artifacts may
    ///   have been pushed by some earlier run, possibly on another machine,
    ///   so it may need a catch-up push — see `PendingState::catchup_representative`.
    ///
    /// A given `input_key` can only ever be stored (following a miss) or
    /// refreshed (following a hit) in one run, never both, so a repeat
    /// `HashMap::insert` for the same key is last-write-wins over two entries
    /// that describe the same content-addressed result — never an actual
    /// conflict.
    ///
    /// Merged and pushed exactly once per run by `flush_pending_entries`,
    /// called once after all tasks complete — see `flush_pending_entries`'s
    /// doc comment for why a per-store or per-hit push would be
    /// self-defeating.
    ///
    /// Deferring the merge is invisible within a run, with one caveat about
    /// two tasks sharing an `input_key` — see `flush_pending_entries`'s doc
    /// comment.
    ///
    /// It is *not* invisible across a run that never reaches its flush: a
    /// process killed after `finish_store`/`refresh_entry` but before
    /// `flush_pending_entries` leaves this map's entries un-merged forever
    /// — their blobs and `entries/*.bin` are already on disk (and, for
    /// stores, already enqueued for the remote), but no shard ever points at
    /// them, so a later run treats them as a miss and redoes the work. This
    /// is an accepted tradeoff, not an oversight: it already applied to
    /// refreshes before this change, and a killed build losing its last few
    /// stores' worth of index entries is far cheaper than the per-store
    /// remote traffic this batching removes.
    pending: Mutex<PendingState>,
}

/// The run's pending index state, behind one lock.
///
/// These two were separate `Mutex`es. The coupling between them —  a
/// representative only ever exists alongside the entries it was recorded
/// with — was maintained by convention, and a path that cleared one without
/// the other would leave a stale representative for a later flush to push a
/// catch-up for. One lock makes the pair inseparable, and lets
/// `refresh_entry` record both in a single acquisition.
#[derive(Debug, Default)]
struct PendingState {
    /// Keyed by `input_key` so repeat writes of the same key collapse.
    entries: HashMap<[u8; 32], SnapshotEntry>,
    /// The first refreshed entry recorded this run, if any — used by
    /// `flush_pending_entries` for a best-effort blob/entry-meta catch-up
    /// push. `None` for a run that only stores: see `SharedCache::pending`'s
    /// doc comment for why stores need no catch-up.
    ///
    /// One entry, not N. This is a token push for a single representative,
    /// not general coverage of the run's refreshed artifacts: if a run
    /// refreshes 40 entries, 39 of them still get no artifact push, and if
    /// their blobs really are missing from this remote the next reader
    /// degrades to a cache miss and re-stores them. A missing blob is a miss,
    /// never an error, which is why one representative is enough and pushing
    /// all N would be the per-hit remote traffic this batching removes.
    /// Which refreshed entry ends up "first" doesn't matter, since the
    /// snapshot-shard push itself is driven by the merge outcome, not by
    /// this entry.
    #[cfg(unix)]
    catchup_representative: Option<SnapshotEntry>,
}

pub(crate) fn blob_path(paths: &SharedCachePaths, outputs_hash: &[u8; 32]) -> PathBuf {
    paths
        .blobs_dir
        .join(format!("{}.tar.zst", hex_hash(*outputs_hash)))
}

pub(crate) fn hex_hash(hash: [u8; 32]) -> String {
    blake3::Hash::from(hash).to_hex().to_string()
}

/// Creates `dir`, first clearing a non-directory sitting where it belongs.
///
/// The snapshot key scheme has changed twice — `<commit>`, then
/// `<unix_ms>-<nonce>`, now `<YYYYMMDD>-<shard>` — and older layouts wrote a
/// plain file where a shard directory now goes. That file makes its bucket
/// permanently unusable: every pull fails to prepare the directory and every
/// merge fails to create it. Both report at `debug:`, so the only symptom a
/// user sees is a shared cache that never hits, which is what #276 was.
///
/// Removing it is safe. Everything under the cache dir is disposable and
/// content-addressed; the worst case is one bucket's worth of entries being
/// rebuilt. Only a non-directory blocking the path is treated this way —
/// permission errors and read-only filesystems still surface.
pub(crate) fn ensure_cache_dir(dir: &Path) -> io::Result<()> {
    let Err(err) = fs::create_dir_all(dir) else {
        return Ok(());
    };

    let blocked_by_file = fs::symlink_metadata(dir).is_ok_and(|meta| !meta.is_dir());
    if !blocked_by_file {
        return Err(err);
    }

    fs::remove_file(dir)?;
    eprintln!(
        "warning: removed a stale file at {} where the shared cache needs a directory; \
         it is left over from an older cache layout and its entries will be rebuilt",
        dir.display()
    );
    fs::create_dir_all(dir)
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

        // Not unix-gated: a missed flush loses index entries on every
        // platform. Nothing here fixes it — the merge needs the run's own
        // call site (`run.rs`), not Drop, whose ordering relative to
        // unwinding and cancellation is not something to depend on. But a
        // call site that forgets `flush_pending_entries` otherwise fails
        // completely silently: every store still reports
        // `StoreOutcome::Stored`, the blobs and `entries/*.bin` are all
        // there, and no shard points at any of them, so the next run misses
        // on all of it. One line makes that diagnosable.
        let pending = self
            .pending
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !pending.entries.is_empty() {
            eprintln!(
                "debug: shared cache dropped with {} unflushed pending index entries",
                pending.entries.len()
            );
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
            day_window: day_window.max(discovery::MIN_SHARED_CACHE_DAY_WINDOW),
            snapshot_store,
            #[cfg(unix)]
            remote,
            index: OnceLock::new(),
            size_cap_bytes,
            pending: Mutex::new(PendingState::default()),
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
            day_window: day_window.max(discovery::MIN_SHARED_CACHE_DAY_WINDOW),
            snapshot_store,
            #[cfg(unix)]
            remote: None,
            index: OnceLock::new(),
            size_cap_bytes,
            pending: Mutex::new(PendingState::default()),
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
    /// the same input_key across buckets can never produce a different
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
        if let Some(remote) = remote {
            if !entry_meta_path(paths, input_key).exists() {
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
            return match StagedCandidate::empty_outputs(
                meta.outputs_hash,
                record,
                meta.stdout,
                meta.stderr,
                reports,
                package_dir,
            ) {
                Ok(candidate) => Some(candidate),
                Err(err) => {
                    eprintln!(
                        "debug: shared cache no-output restore failed for input_key={}: {err}",
                        hex_hash(*input_key)
                    );
                    None
                }
            };
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

    /// Pull (if remote-enabled) and merge a single bucket's snapshot into the index.
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
        if self.write_bucket_key.is_none() {
            return Ok(StoreOutcome::Disabled);
        }

        // Check if task succeeded.
        if !record.succeeded {
            return Ok(StoreOutcome::SkippedNotSucceeded);
        }

        // Check duration threshold.
        let duration_ms = record.end_unix_ms.saturating_sub(record.start_unix_ms);
        if duration_ms < min_store_duration_ms() {
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
            #[cfg(unix)]
            input_key,
            #[cfg(unix)]
            meta.has_outputs,
            entry,
        )
    }

    /// Pushes this store's own artifacts immediately, then records the entry
    /// for the once-per-run index merge.
    ///
    /// Only the artifact half (blob + entry meta) is immediate here; the
    /// index half is deferred to `flush_pending_entries`, called once after
    /// all tasks complete, the same way `refresh_entry` already defers it for
    /// cache hits — see `PendingState::entries`'s doc comment for why one map and
    /// one flush serve both, and `flush_pending_entries`'s doc comment for
    /// why batching the merge is the point. Pushing the artifacts immediately
    /// (rather than also deferring them) matters because a restore on
    /// another machine must be able to find them whether or not this run's
    /// index push has happened yet — see `enqueue_entry_artifacts`'s doc
    /// comment.
    fn finish_store(
        &self,
        blob_result: BlobWriteResult,
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
                self.enqueue_entry_artifacts(entry.outputs_hash, *input_key, has_outputs);
                self.record_pending_entry(entry);
                Ok(StoreOutcome::Stored)
            }
            BlobWriteResult::SkippedTooLarge { bytes } => {
                Ok(StoreOutcome::SkippedTooLarge { bytes })
            }
        }
    }

    /// Enqueues the content-addressed blob (when `has_outputs`) and the
    /// entry meta object for background push.
    ///
    /// Independent of [`enqueue_index_push`](Self::enqueue_index_push): a
    /// restore on another machine needs these regardless of whether this
    /// run's index push has happened, so callers may dispatch this half
    /// without the other (see `RemoteSync::push_entry_artifacts`).
    #[cfg(unix)]
    fn enqueue_entry_artifacts(
        &self,
        outputs_hash: [u8; 32],
        input_key: [u8; 32],
        has_outputs: bool,
    ) {
        let Some(remote) = &self.remote else {
            return;
        };
        if remote.is_disabled() {
            return;
        }
        remote.enqueue_entry_artifacts(remote::OwnedEntryArtifacts {
            paths: Arc::clone(&self.paths),
            outputs_hash,
            input_key,
            has_outputs,
        });
    }

    /// Enqueues the merged index shard (and its subsumed-shard deletes) for
    /// background push. See [`enqueue_entry_artifacts`](Self::enqueue_entry_artifacts)
    /// for why this is a separate dispatch from the blob/entry-meta push.
    #[cfg(unix)]
    fn enqueue_index_push(&self, write_key: &str, merge: MergeEntryOutcome) {
        let Some(remote) = &self.remote else {
            return;
        };
        if remote.is_disabled() {
            return;
        }
        remote.enqueue_index_push(remote::OwnedIndexPush {
            shard_key: write_key.to_string(),
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
    /// today's shards are always in the window — see the `day_window` field
    /// doc for why that needs a floor of 2, not 1), so unlike the old
    /// discovery-based scheme there's no need to special-case injecting the
    /// write key: see `discovery::tests::write_bucket_is_always_inside_the_read_set`.
    #[must_use]
    pub fn candidate_keys(&self) -> Vec<String> {
        discovery::bucket_keys_for(discovery::now_unix_ms(), self.day_window)
    }

    /// Test-only accessor for the snapshot store, so tests can seed and read
    /// back buckets directly without a public production API for it.
    #[cfg(test)]
    fn snapshot_store(&self) -> &SnapshotStore {
        &self.snapshot_store
    }

    /// Test-only accessor for how many entries are pending a flush.
    ///
    /// Exists so a test can pin "repeat `refresh_entry` calls for the same
    /// `input_key` collapse to one pending entry" at the collection itself,
    /// before `flush_pending_entries` runs. Asserting only on the post-flush
    /// shard doesn't discriminate this: `merge_entries_with_outcome` inserts
    /// into a `BTreeMap` keyed by `input_key`, so it would absorb a
    /// duplicate-preserving (e.g. `Vec`-based) pending collection into the
    /// same one-entry shard anyway.
    #[cfg(test)]
    fn pending_entry_count(&self) -> usize {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entries
            .len()
    }

    /// Shared insert behind `PendingState::entries`, used by both `finish_store`
    /// (after a miss) and `refresh_entry` (after a hit). See
    /// `SharedCache::pending`'s doc comment for why one map serves both.
    ///
    /// Keys on `entry.input_key`, the same field `merge_entries_with_outcome`
    /// re-keys on at flush time, so the map's dedup key and the shard's key
    /// cannot drift apart.
    fn record_pending_entry(&self, entry: SnapshotEntry) {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entries
            .insert(entry.input_key, entry);
    }

    /// `record_pending_entry` for the cache-hit path, which additionally
    /// nominates the first refreshed entry as the catch-up representative.
    /// One acquisition rather than two, so the entry and the representative
    /// it belongs to can never be recorded across a gap.
    fn record_pending_refresh(&self, entry: SnapshotEntry) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        #[cfg(unix)]
        if pending.catchup_representative.is_none() {
            pending.catchup_representative = Some(entry.clone());
        }
        pending.entries.insert(entry.input_key, entry);
    }

    /// Records an entry on a shared-cache hit for a later batched merge, and
    /// immediately advances `entries/<input_key>.bin`'s mtime.
    ///
    /// Without this, a stable entry written once falls out of the day window
    /// a few days later (every build then misses it, rebuilds, and rewrites
    /// it — a sawtooth on exactly the packages the cache exists to serve),
    /// and separately, `gc_entries_dir` ages its meta out by mtime alone
    /// since a hit doesn't otherwise touch the file.
    ///
    /// Only the mtime touch happens here, per hit. The merge into today's
    /// write bucket (and the remote push it enables) is deferred to
    /// `flush_pending_entries`, called once after all tasks complete: merging
    /// and pushing per-hit self-defeats the feature -- see
    /// `flush_pending_entries`'s doc comment. The mtime touch itself stays
    /// immediate and per-entry: it's a local file operation with no rclone
    /// involvement, and every refreshed entry's meta file has to be touched
    /// regardless of how the index-merge is batched.
    ///
    /// Also records this entry as the catch-up representative if none is
    /// queued yet -- unlike a store, a refreshed entry's artifacts may have
    /// been pushed by an earlier run, possibly on another machine, so they
    /// may be missing from this remote. See
    /// `PendingState::catchup_representative`'s doc comment.
    ///
    /// Best-effort and infallible: the hit has already succeeded by the time
    /// this runs, so a refresh failure must never turn it into a miss or fail
    /// the build. The mtime-touch failure path just logs at `debug:` and
    /// returns. Never re-stores the blob or the meta object — both are
    /// content-addressed and already correct.
    pub fn refresh_entry(&self, input_key: &[u8; 32], entry: &SnapshotEntry) {
        self.record_pending_refresh(entry.clone());

        let path = entry_meta_path(&self.paths, input_key);
        match OpenOptions::new().write(true).open(&path) {
            Ok(file) => {
                let times = std::fs::FileTimes::new().set_modified(SystemTime::now());
                if let Err(err) = file.set_times(times) {
                    eprintln!(
                        "debug: shared cache refresh could not advance meta mtime for input_key={}: {err}",
                        hex_hash(*input_key)
                    );
                }
            }
            Err(err) => {
                eprintln!(
                    "debug: shared cache refresh could not advance meta mtime for input_key={}: {err}",
                    hex_hash(*input_key)
                );
            }
        }
    }

    /// Flushes every entry `finish_store` or `refresh_entry` recorded this
    /// run: exactly one `merge_entries_with_outcome` call (one shard load, at
    /// most one consolidated write), and on unix at most one
    /// `enqueue_entry_artifacts` catch-up call plus exactly one
    /// `enqueue_index_push` call, regardless of how many stores or hits fed
    /// into it.
    ///
    /// Round 2 of this feature pushed a merge to the remote on every single
    /// hit. That reached the remote correctly, but a build with N cache hits
    /// enqueues up to N pushes on the first run of a day -- and N is largest
    /// exactly when the cache is working best. The same shape of problem hit
    /// stores once the write bucket became date-keyed rather than
    /// commit-keyed: `build_index` now pulls the whole day's shards, including
    /// the write bucket, so a per-store merge-and-push reloads and re-uploads
    /// the fleet's entire day of activity on every single store. Either way,
    /// saturating the rclone daemon trips the `timeout_disable` circuit
    /// breaker, disabling the remote for the rest of the build: the feature
    /// defeats itself under its own success. Batching collapses both cases to
    /// one push per run, independent of store or hit count, and also removes
    /// the per-call shard reload `merge_entry_with_outcome` (now
    /// `merge_entries_with_outcome`) pays to even report `IdempotentNoop` --
    /// one flush, one load.
    ///
    /// The catch-up push is representative-driven and only fires when a
    /// refresh nominated one (see `PendingState::catchup_representative`'s
    /// doc comment): a flush containing only stores has nothing to catch up --
    /// `finish_store` already pushed each store's own artifacts immediately
    /// -- so it does exactly one `enqueue_index_push` and no
    /// `enqueue_entry_artifacts` call at all. When it does fire it covers one
    /// entry, not every refreshed one: a token push for a single
    /// representative, with the other N-1 left uncovered because a blob
    /// missing from the remote degrades a later reader to a cache miss rather
    /// than an error.
    ///
    /// Batching is invisible within a run, with one caveat: `get_or_build_index`
    /// builds the merged index once behind a `OnceLock`, so a mid-run merge
    /// was never visible to a later *index* lookup in the same process
    /// anyway. The exception is a task that reaches `Decision::Run` before
    /// the index is built at all (`dispatch.rs`): under the old eager merge
    /// its entry was already in the shard, so a later lookup for the same
    /// `input_key` in the same run could see it. Now it can't, and two tasks
    /// sharing an `input_key` in one run each rebuild. That costs one
    /// redundant rebuild of an identical result, never a wrong one.
    ///
    /// Best-effort and infallible, same as `refresh_entry`: called after all
    /// tasks complete, so nothing downstream depends on it succeeding.
    pub fn flush_pending_entries(&self) {
        // Resolve the write key BEFORE draining: draining first and then
        // bailing on a `None` key would discard the whole run's entries with
        // nowhere for them to have gone. Unreachable today (a cache with no
        // write key records nothing to flush), but the drain is destructive,
        // so it happens only once the flush can actually proceed.
        let Some(write_key) = self.write_bucket_key.as_deref() else {
            return;
        };

        let entries: Vec<SnapshotEntry> = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if pending.entries.is_empty() {
                // A representative is only ever recorded alongside an entry,
                // so there should be nothing here. Clear it anyway rather
                // than trust that: a representative outliving its entries
                // would have a later flush push a catch-up for an entry this
                // run never merged.
                #[cfg(unix)]
                {
                    pending.catchup_representative = None;
                }
                return;
            }
            std::mem::take(&mut pending.entries).into_values().collect()
        };

        let entry_count = entries.len();

        let merge = self
            .snapshot_store
            .merge_entries_with_outcome(write_key, entries);
        match merge.result {
            MergeResult::SkippedLockUnavailable => {
                // One batched merge means one lock failure now costs the
                // whole run's index entries, not one entry's — worth a
                // `warn:`, unlike the per-entry version this replaced.
                eprintln!(
                    "warn: shared cache could not lock its index shard; dropped {entry_count} index \
                     entries for this run, so those tasks will be rebuilt next time"
                );
            }
            MergeResult::Inserted
            | MergeResult::IdempotentNoop
            | MergeResult::ConflictKeptExisting => {
                #[cfg(unix)]
                {
                    // Taken here rather than before the merge so a lock
                    // failure doesn't consume it: the index entries are gone
                    // either way, but the catch-up push is independent of
                    // them and there's no reason to lose both.
                    //
                    // Only refreshes nominate a representative (see
                    // `PendingState::catchup_representative`): a store-only
                    // flush must not manufacture a catch-up push for an
                    // arbitrary entry, since `finish_store` already pushed
                    // that entry's own artifacts immediately -- that's
                    // exactly the remote traffic this batching removes.
                    let representative = self
                        .pending
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .catchup_representative
                        .take();
                    if let Some(representative) = representative {
                        let has_outputs = read_entry_meta(&self.paths, &representative.input_key)
                            .map(|meta| meta.has_outputs)
                            .unwrap_or(false);
                        self.enqueue_entry_artifacts(
                            representative.outputs_hash,
                            representative.input_key,
                            has_outputs,
                        );
                    }
                    self.enqueue_index_push(write_key, merge);
                }
            }
        }
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
    use std::time::{Duration, SystemTime};
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

        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
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
        // The index merge is deferred to the end-of-run flush; a restore
        // needs the merged index built, so simulate the run finishing here.
        cache.flush_pending_entries();

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

        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
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
        cache.flush_pending_entries();

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

        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
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

        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
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

        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
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
        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
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

        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
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
    fn flush_pending_entries_is_best_effort_when_snapshot_lock_unavailable() {
        // `store()` itself can no longer observe a snapshot-lock failure: the
        // merge moved out of `finish_store` into `flush_pending_entries`, so
        // `StoreOutcome::SkippedLockUnavailable` was removed as dead
        // (unreachable from `store()`, its only consumers were an empty
        // match arm and this test's old assertion). The underlying failure
        // mode -- the snapshot shard dir being unwritable -- still has to be
        // handled somewhere, and now it's here: `flush_pending_entries` must
        // stay best-effort and infallible (no panic, no error propagated)
        // when the merge it performs hits `MergeResult::SkippedLockUnavailable`.
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

        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
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
        assert_eq!(result, StoreOutcome::Stored);
        assert_eq!(cache.pending_entry_count(), 1);

        let write_bucket = cache.write_bucket_key().expect("write bucket").to_string();

        fs::set_permissions(
            &cache.paths.snapshots_dir,
            fs::Permissions::from_mode(0o500),
        )
        .unwrap();

        // Must not panic.
        cache.flush_pending_entries();

        // "Didn't panic" alone is equally true of the success path, so pin
        // that the merge really did fail. Running as root (routine in CI
        // containers) or on a filesystem that ignores mode bits makes the
        // `chmod` above a no-op, `create_dir_all` succeeds, and the merge
        // lands -- in which case this test never exercised the failure
        // handling it exists for and must say so instead of passing.
        // Read before restoring permissions, but assert after: a panic here
        // would otherwise skip the restore and leave `snapshots_dir`
        // unwritable, which `TempDir`'s drop can't clean up and silently
        // swallows. `load` only needs `r-x`, so it works inside the window.
        let merged = cache.snapshot_store().load(&write_bucket);

        fs::set_permissions(
            &cache.paths.snapshots_dir,
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();

        assert!(
            merged.is_none(),
            "the merge must have failed; the permission trick was ineffective (running as root?)"
        );
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

        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
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
        let input_key1 = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
        let input_key2 = derive_input_key([5; 32], [6; 32], [7; 32], [8; 32], [5; 32]);

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
        // `build_index` reads each candidate key's shard exactly once and
        // never re-reads it under computed bucket keys, so nothing here
        // re-reads either fixture a second time on top of the load this
        // test counts.
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
    fn old_bucket_within_the_day_window_survives_heavy_same_day_churn() {
        // The #277 property, restated for computed buckets: a bucket near
        // the back of the day window must stay reachable no matter how much
        // local churn piles up on other buckets. Under the old
        // discovery-plus-rollup scheme this needed a rollup to protect an
        // old shard from a shard-count cap; computed buckets have no cap to
        // evict it from in the first place -- the read set is exactly
        // `bucket_keys_for(now, day_window)`, independent of how many local
        // shard directories exist.
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());
        let temp_cache = TempDir::new().unwrap();

        const DAY_WINDOW: usize = 3;

        // A bucket for the oldest day still inside the window, on a shard
        // this test's churn never touches directly -- seeded straight into
        // the snapshot store, bypassing `store()` (which only ever writes
        // to *today's* bucket).
        let now = discovery::now_unix_ms();
        let old_day_ms = now.saturating_sub((DAY_WINDOW as u64 - 1) * 24 * 60 * 60 * 1000);
        let old_bucket = discovery::bucket_key(old_day_ms, 0);
        let old_input_key = derive_input_key([88; 32], [1; 32], [1; 32], [1; 32], [5; 32]);
        {
            let paths = open_shared_paths(temp_cache.path()).unwrap();
            SnapshotStore::new(paths.clone()).merge_entry(
                &old_bucket,
                SnapshotEntry {
                    task_id: "pkg#old".to_string(),
                    input_key: old_input_key,
                    outputs_hash: [0; 32],
                    task_spec_hash: [1; 32],
                    env_hash: [1; 32],
                    pkg_dep_hash: [1; 32],
                    duration_ms: 200,
                    output_bytes: 0,
                    cached_at_unix_ms: 1,
                    tool_version: None,
                },
            );
            write_entry_meta(
                &paths,
                &old_input_key,
                &EntryMeta {
                    schema_version: ENTRY_META_SCHEMA_VERSION,
                    outputs_hash: [0; 32],
                    has_outputs: false,
                    record: bincode::serde::encode_to_vec(
                        sample_record(true, 200),
                        bincode_config(),
                    )
                    .unwrap(),
                    stdout: b"old-stdout".to_vec(),
                    stderr: Vec::new(),
                    reports: Vec::new(),
                },
            )
            .unwrap();
        }

        let package_dir = temp_repo.path().join("pkg");
        fs::create_dir_all(&package_dir).unwrap();
        let empty_hash = crate::resolve::combined_outputs_hash(&[]);
        let mut record = sample_record(true, 200);
        record.output_patterns = vec![];
        record.outputs = vec![];
        record.outputs_hash = empty_hash;

        // Heavy same-day churn: 30 fresh stores, each its own `luchta run`
        // (its own `SharedCache` instance, flushed at the end like a real
        // run) landing in one of today's `SHARED_CACHE_SHARD_COUNT` shards.
        for i in 0..30u8 {
            let cache = SharedCache::open_with_cache_dir(
                temp_repo.path(),
                1_000_000,
                DAY_WINDOW,
                Some(temp_cache.path()),
            )
            .unwrap();
            let churn_input_key = derive_input_key([i; 32], [2; 32], [2; 32], [2; 32], [2; 32]);
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
            cache.flush_pending_entries();
        }

        // The churn must actually have landed on disk, not just been
        // recorded and silently dropped when each per-iteration `cache` was
        // dropped unflushed -- otherwise the restore below would prove
        // nothing about surviving churn.
        let churn_snapshot_dirs = fs::read_dir(temp_cache.path().join("snapshots"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .count();
        assert!(
            churn_snapshot_dirs > 1,
            "expected churn to produce shard directories beyond the seeded old bucket, got {churn_snapshot_dirs}"
        );

        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            DAY_WINDOW,
            Some(temp_cache.path()),
        )
        .unwrap();
        let restore_dir = temp_repo.path().join("restore-old");
        fs::create_dir_all(&restore_dir).unwrap();
        let candidate = cache
            .try_restore_candidates("pkg#old", &old_input_key, &restore_dir)
            .next()
            .expect(
                "an old bucket inside the day window must stay reachable \
                 regardless of how many newer buckets exist",
            );
        assert_eq!(candidate.stdout, b"old-stdout");
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

        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
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
        cache.flush_pending_entries();

        // Concurrent restore threads.
        let initialized = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();

        for i in 0..4 {
            let cache = Arc::clone(&cache);
            let initialized = Arc::clone(&initialized);
            let restore_dir = temp_repo.path().join(format!("restore-{}", i));
            fs::create_dir_all(&restore_dir).unwrap();

            handles.push(thread::spawn(move || {
                let found = cache
                    .try_restore_candidates("pkg#build", &input_key, &restore_dir)
                    .next()
                    .is_some();
                // Mark that we initialized the index.
                initialized.store(cache.index.get().is_some(), Ordering::SeqCst);
                found
            }));
        }

        // All threads complete. Each must actually have found the stored
        // entry -- with the merge left unflushed, the index would be empty
        // and every thread's candidate iteration would silently yield
        // `None`, leaving this test unable to exercise the concurrent
        // restore path (`stage_entry` et al.) it's named for.
        for handle in handles {
            assert!(
                handle.join().unwrap(),
                "each concurrent restore thread must find the stored candidate"
            );
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

        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
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
        cache.flush_pending_entries();

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
        let key_a = derive_input_key([11; 32], [2; 32], [3; 32], [4; 32], [5; 32]);

        let mut record_b = record_a.clone();
        record_b.task_spec_hash = [22; 32];
        let key_b = derive_input_key([22; 32], [2; 32], [3; 32], [4; 32], [5; 32]);

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

        // NoOutputs is a success path: the entry must still be indexed, once
        // the deferred index merge is flushed.
        cache.flush_pending_entries();
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

        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
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
        cache.flush_pending_entries();

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
        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);

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
        cache.flush_pending_entries();

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
        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);

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
        cache.flush_pending_entries();

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
        let key_a = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
        let key_b = derive_input_key([5; 32], [6; 32], [7; 32], [8; 32], [5; 32]);

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
            // Each iteration stands in for a whole separate `luchta run`
            // invocation, which ends with exactly this flush.
            cache.flush_pending_entries();
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

    #[test]
    fn day_window_is_clamped_to_a_floor_of_two_at_construction() {
        // A `day_window` of 0 or 1 would let a UTC-midnight race between
        // `write_bucket_key`'s computation (at `open()`) and the first
        // `candidate_keys()` call put the write key outside the read
        // window. `open()`/`open_with_cache_dir()`/`open_with_remote()` take
        // a bare `usize` with no floor of their own -- only the CLI's env
        // parsing guards against 0 -- so the clamp has to live here.
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());
        let temp_cache = TempDir::new().unwrap();

        for requested in [0, 1] {
            let cache = SharedCache::open_with_cache_dir(
                temp_repo.path(),
                1_000_000,
                requested,
                Some(temp_cache.path()),
            )
            .unwrap();
            assert_eq!(
                cache.candidate_keys().len(),
                discovery::MIN_SHARED_CACHE_DAY_WINDOW * SHARED_CACHE_SHARD_COUNT,
                "day_window={requested} should be clamped up to the floor of {}",
                discovery::MIN_SHARED_CACHE_DAY_WINDOW
            );
        }
    }

    #[test]
    fn two_source_states_of_one_task_each_get_their_own_entry() {
        // Before inputs were part of the key both states computed the SAME
        // input_key, the first writer took the slot, and the second could
        // never be stored (ConflictKeptExisting) — a permanent miss plus a
        // wasted meta fetch on every build until GC aged the entry out.
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());
        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            3,
            Some(temp_cache.path()),
        )
        .unwrap();

        let package_dir = temp_repo.path().join("pkg");
        fs::create_dir_all(&package_dir).unwrap();
        let empty_outputs = crate::resolve::combined_outputs_hash(&[]);

        // Same task, same env, same deps — only the input CONTENT differs.
        let inputs_a = crate::resolve::combined_inputs_hash(&[FileEntry {
            path: "src/main.ts".to_string(),
            size: 10,
            mtime_ns: 0,
            hash: [0xAA; 32],
            absent: false,
        }]);
        let inputs_b = crate::resolve::combined_inputs_hash(&[FileEntry {
            path: "src/main.ts".to_string(),
            size: 10,
            mtime_ns: 0,
            hash: [0xBB; 32],
            absent: false,
        }]);
        assert_ne!(
            inputs_a, inputs_b,
            "fixture must model two distinct source states"
        );

        let key_a = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], inputs_a);
        let key_b = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], inputs_b);
        assert_ne!(
            key_a, key_b,
            "distinct inputs must yield distinct cache keys"
        );

        for (key, marker) in [(key_a, &b"variant-a"[..]), (key_b, &b"variant-b"[..])] {
            let mut record = sample_record(true, 200);
            record.output_patterns = vec![];
            record.outputs = vec![];
            record.outputs_hash = empty_outputs;
            let outcome = cache
                .store(
                    "pkg#build",
                    &key,
                    &empty_outputs,
                    &package_dir,
                    &[],
                    &record,
                    marker,
                    b"",
                    &[],
                    temp_repo.path(),
                )
                .unwrap();
            assert_eq!(outcome, StoreOutcome::Stored, "both variants must store");
        }

        // Each key resolves to its own meta — neither evicted the other.
        assert_eq!(
            read_entry_meta(cache.paths(), &key_a).unwrap().stdout,
            b"variant-a"
        );
        assert_eq!(
            read_entry_meta(cache.paths(), &key_b).unwrap().stdout,
            b"variant-b"
        );
    }

    fn sample_entry_with_seed(seed: u8, outputs_hash: [u8; 32]) -> SnapshotEntry {
        let task_spec_hash = [seed; 32];
        let env_hash = [seed.wrapping_add(1); 32];
        let pkg_dep_hash = [seed.wrapping_add(2); 32];
        let dep_outputs_hash = [seed.wrapping_add(3); 32];
        let inputs_hash = [seed.wrapping_add(4); 32];
        SnapshotEntry {
            task_id: format!("pkg-{seed}#build"),
            input_key: derive_input_key(
                task_spec_hash,
                env_hash,
                pkg_dep_hash,
                dep_outputs_hash,
                inputs_hash,
            ),
            outputs_hash,
            task_spec_hash,
            env_hash,
            pkg_dep_hash,
            duration_ms: 100 + u64::from(seed),
            output_bytes: 1_000 + u64::from(seed),
            cached_at_unix_ms: 1_700_000_000_000 + u64::from(seed),
            tool_version: Some("0.1.0".to_owned()),
        }
    }

    #[test]
    fn a_shared_hit_refreshes_the_entry_into_the_current_write_bucket() {
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());
        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            3,
            Some(temp_cache.path()),
        )
        .unwrap();

        // Seed the entry into an OLDER bucket only, as a build two days ago would have.
        let stale_bucket = bucket_key(discovery::now_unix_ms() - 2 * 24 * 60 * 60 * 1000, 0);
        let write_bucket = cache.write_bucket_key().expect("write bucket").to_string();
        assert_ne!(
            stale_bucket, write_bucket,
            "fixture must seed a different bucket"
        );

        let entry = sample_entry_with_seed(1, [7; 32]);
        let input_key = entry.input_key;
        cache
            .snapshot_store()
            .merge_entry(&stale_bucket, entry.clone());
        assert!(
            cache.snapshot_store().load(&write_bucket).is_none(),
            "write bucket must start empty"
        );

        cache.refresh_entry(&input_key, &entry);
        assert!(
            cache.snapshot_store().load(&write_bucket).is_none(),
            "refresh_entry only records the entry; nothing is merged until flush_pending_entries runs"
        );
        cache.flush_pending_entries();

        let refreshed = cache
            .snapshot_store()
            .load(&write_bucket)
            .expect("flush_pending_entries must write the recorded entry into the current bucket");
        assert!(
            refreshed.entries.contains_key(&input_key_hex(input_key)),
            "the refreshed entry must be present in today's bucket, so it survives \
             the day window moving past the bucket it was originally written to"
        );
    }

    #[test]
    fn a_shared_hit_advances_the_entry_meta_mtime() {
        // gc_entries_dir ages out entries/*.bin by mtime and a hit does not
        // rewrite the file, so without this an entry in active use has its
        // meta deleted while its index key stays live.
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());
        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            3,
            Some(temp_cache.path()),
        )
        .unwrap();

        let input_key = [3u8; 32];
        let meta = EntryMeta {
            schema_version: ENTRY_META_SCHEMA_VERSION,
            outputs_hash: [7; 32],
            has_outputs: false,
            record: vec![1, 2, 3],
            stdout: Vec::new(),
            stderr: Vec::new(),
            reports: Vec::new(),
        };
        write_entry_meta(cache.paths(), &input_key, &meta).unwrap();

        let path = entry_meta_path(cache.paths(), &input_key);
        let backdated = SystemTime::now() - Duration::from_secs(60 * 60 * 24 * 10);
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(backdated)).unwrap();

        // The entry's own `input_key` must match the one whose meta we
        // backdated: `refresh_entry` touches the meta by its parameter, but
        // `record_pending_entry` keys the pending map on `entry.input_key`.
        // Leaving them different would record the refresh under an unrelated
        // key and still pass, since this test only checks the mtime.
        let mut refreshed = sample_entry_with_seed(1, [7; 32]);
        refreshed.input_key = input_key;
        cache.refresh_entry(&input_key, &refreshed);

        let after = fs::metadata(&path).unwrap().modified().unwrap();
        assert!(
            after > backdated,
            "a hit must advance the meta mtime so GC does not expire an entry in active use"
        );
    }

    #[test]
    fn repeat_hits_of_the_same_key_collapse_to_one_entry_before_flush() {
        // `PendingState::entries` is keyed by `input_key` precisely so repeat
        // hits of the same key collapse to a single entry before a flush.
        // `flush_pending_entries_collapses_two_hits_into_a_single_push` (in
        // `remote.rs`) proves N DISTINCT keys collapse into one push, but
        // that alone doesn't pin dedup-by-key: a Vec-based (or otherwise
        // duplicate-preserving) pending collection would also pass it, AND
        // would also produce a one-entry final shard here, since
        // `merge_entries_with_outcome` inserts into a `BTreeMap` keyed by
        // `input_key` and so absorbs same-key duplicates during the merge
        // itself. So the property has to be pinned at the pending
        // collection, before any merge happens -- via `pending_entry_count`.
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());
        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            3,
            Some(temp_cache.path()),
        )
        .unwrap();

        let entry = sample_entry_with_seed(1, [7; 32]);
        let input_key = entry.input_key;

        cache.refresh_entry(&input_key, &entry);
        cache.refresh_entry(&input_key, &entry);
        assert_eq!(
            cache.pending_entry_count(),
            1,
            "two refresh_entry calls for the SAME input_key must collapse to one \
             pending entry before any flush/merge ever runs"
        );

        cache.flush_pending_entries();

        let write_bucket = cache.write_bucket_key().unwrap().to_string();
        let snapshot = cache
            .snapshot_store()
            .load(&write_bucket)
            .expect("flush_pending_entries must write the recorded entry into the current bucket");
        assert_eq!(
            snapshot.entries.len(),
            1,
            "the flushed shard must contain exactly one entry for the repeated key"
        );
    }

    #[test]
    fn stores_do_not_merge_into_the_index_until_flush() {
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());
        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            3,
            Some(temp_cache.path()),
        )
        .unwrap();

        let package_dir = temp_repo.path().join("pkg");
        fs::create_dir_all(&package_dir).unwrap();
        let empty_outputs = crate::resolve::combined_outputs_hash(&[]);
        let write_bucket = cache.write_bucket_key().expect("write bucket").to_string();

        for seed in 0u8..5 {
            let mut record = sample_record(true, 200);
            record.output_patterns = vec![];
            record.outputs = vec![];
            record.outputs_hash = empty_outputs;
            record.task_spec_hash = [seed; 32];
            let key = derive_input_key([seed; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
            let outcome = cache
                .store(
                    "pkg#build",
                    &key,
                    &empty_outputs,
                    &package_dir,
                    &[],
                    &record,
                    b"out",
                    b"",
                    &[],
                    temp_repo.path(),
                )
                .unwrap();
            assert_eq!(outcome, StoreOutcome::Stored);
        }

        // Nothing merged yet: the bucket has no shard until the flush.
        assert!(
            cache.snapshot_store().load(&write_bucket).is_none(),
            "stores must not merge into the index before the flush"
        );
        assert_eq!(cache.pending_entry_count(), 5, "all five entries pending");

        cache.flush_pending_entries();

        let snapshot = cache
            .snapshot_store()
            .load(&write_bucket)
            .expect("flush must write the bucket");
        assert_eq!(
            snapshot.entries.len(),
            5,
            "one merge carrying all five entries"
        );

        // A post-flush shard-file count was tried here and removed: it can't
        // distinguish one batched merge from five eager ones, because
        // `merge_entries_with_outcome` compacts (deletes) every shard it
        // subsumes on each call -- five sequential single-entry merges and
        // one five-entry merge both converge to exactly one file at rest.
        // Nothing here counts merges, which is why the name says timing
        // rather than counts: the pre-flush assertion above catches an eager
        // per-store merge because no shard may exist yet, and the five
        // post-flush entries show the batch carried all of them. Its remote
        // counterpart is the pre-flush `remote_snapshot_files(...).is_empty()`
        // check in `remote.rs`. The remote is not a second, count-based
        // proof -- `push_index_merge` deletes each subsumed shard from the
        // remote too, so the remote resting state converges to one shard
        // under eager merges exactly as the local one does. Do not re-add a
        // post-flush shard-file-count assertion on either side; it will pass
        // under both behaviours and imply a proof it can't provide.
    }

    #[test]
    fn ensure_cache_dir_leaves_an_existing_directory_and_its_contents_alone() {
        // The healing path deletes whatever is in the way, so the guard that
        // it only ever fires for a non-directory matters: firing on a real
        // shard directory would delete a whole bucket's entries every run.
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("20260808-01");
        fs::create_dir_all(&dir).unwrap();
        let shard = dir.join("existing.bincode");
        fs::write(&shard, b"real shard").unwrap();

        ensure_cache_dir(&dir).expect("an existing directory is not an error");

        assert!(dir.is_dir());
        assert_eq!(
            fs::read(&shard).unwrap(),
            b"real shard",
            "an existing shard directory must survive untouched"
        );
    }

    #[test]
    fn ensure_cache_dir_creates_missing_parents() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("snapshots").join("20260808-02");

        ensure_cache_dir(&dir).expect("missing parents should be created");

        assert!(dir.is_dir());
    }

    #[test]
    fn a_second_flush_merges_entries_recorded_after_the_first() {
        // `run.rs` flushes twice per cycle: once when the dispatch loop
        // returns, then again after the walker drain and worker kill. The
        // second call exists for tasks that were still in flight during the
        // first one (#287), which happens on the SIGINT/SIGTERM and
        // watch-cancel paths. That only helps if a flush after a flush
        // actually merges the late arrival instead of being a no-op, which
        // is what this pins.
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());
        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            3,
            Some(temp_cache.path()),
        )
        .unwrap();

        let package_dir = temp_repo.path().join("pkg");
        fs::create_dir_all(&package_dir).unwrap();
        let empty_outputs = crate::resolve::combined_outputs_hash(&[]);
        let write_bucket = cache.write_bucket_key().expect("write bucket").to_string();

        let store_seed = |seed: u8| {
            let mut record = sample_record(true, 200);
            record.output_patterns = vec![];
            record.outputs = vec![];
            record.outputs_hash = empty_outputs;
            record.task_spec_hash = [seed; 32];
            let key = derive_input_key([seed; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
            let outcome = cache
                .store(
                    "pkg#build",
                    &key,
                    &empty_outputs,
                    &package_dir,
                    &[],
                    &record,
                    b"out",
                    b"",
                    &[],
                    temp_repo.path(),
                )
                .unwrap();
            assert_eq!(outcome, StoreOutcome::Stored);
        };

        // The tasks that finished before the dispatch loop returned.
        store_seed(0);
        store_seed(1);
        cache.flush_pending_entries();
        assert_eq!(
            cache
                .snapshot_store()
                .load(&write_bucket)
                .expect("first flush must write the bucket")
                .entries
                .len(),
            2,
            "first flush carries the tasks that had already completed"
        );

        // The straggler: still running when the first flush happened, lands
        // while `finalize_and_report` is draining.
        store_seed(2);
        assert_eq!(
            cache.pending_entry_count(),
            1,
            "the late store must be pending again after the first flush drained the map"
        );

        cache.flush_pending_entries();

        let snapshot = cache
            .snapshot_store()
            .load(&write_bucket)
            .expect("bucket must still be readable after the second flush");
        assert_eq!(
            snapshot.entries.len(),
            3,
            "the second flush must add the straggler without dropping the first flush's entries"
        );
        assert_eq!(
            cache.pending_entry_count(),
            0,
            "the second flush must drain the map too"
        );
    }

    #[test]
    fn store_writes_blob_and_entry_meta_immediately_before_any_flush() {
        // The invariant most at risk from batching the index merge: a
        // restore on another machine has to find a store's artifacts
        // whether or not this run's index push (now deferred to
        // `flush_pending_entries`) has happened. Pinned here at the local
        // filesystem level -- no flush call anywhere in this test.
        //
        // A guard for that invariant only. It is not evidence of batching:
        // an eager per-store merge writes the blob and entry meta at the same
        // point, so every assertion below passes identically under it. The
        // batching itself is pinned by
        // `stores_do_not_merge_into_the_index_until_flush`.
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());
        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            3,
            Some(temp_cache.path()),
        )
        .unwrap();

        let package_dir = temp_repo.path().join("pkg");
        fs::create_dir_all(package_dir.join("dist")).unwrap();
        fs::write(package_dir.join("dist/main.js"), "console.log('hi');").unwrap();

        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
        let outputs_hash = [7; 32];
        let record = sample_record(true, 200);

        let outcome = cache
            .store(
                "pkg#build",
                &input_key,
                &outputs_hash,
                &package_dir,
                &[PathBuf::from("dist/main.js")],
                &record,
                b"stdout",
                b"stderr",
                &[],
                temp_repo.path(),
            )
            .unwrap();
        assert_eq!(outcome, StoreOutcome::Stored);

        // No flush anywhere above: the entry meta and blob must already be
        // on disk, since only the index merge is deferred.
        assert!(
            read_entry_meta(cache.paths(), &input_key).is_some(),
            "entry meta must be written immediately by store(), not deferred to flush"
        );
        assert!(
            blob_path(cache.paths(), &outputs_hash).exists(),
            "the blob must be written immediately by store(), not deferred to flush"
        );
    }
}
