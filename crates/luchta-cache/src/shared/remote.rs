//! Remote (S3-via-rclone) sync layer for the shared cache.
//!
//! Owns `RemoteSync` — the opt-in remote pull/push transport built on the
//! rclone rcd sidecar — and its run-wide disable-and-warn state. Kept separate
//! from `mod.rs` so the local cache and the remote sync concerns stay cohesive.

use std::fs;
use std::io;
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Default threshold for consecutive timeouts before disabling remote sync.
/// A 0 threshold would disable remote on the first queued timeout, defeating
/// the backpressure policy's purpose.
pub const DEFAULT_TIMEOUT_DISABLE_THRESHOLD: usize = 8;
pub const DEFAULT_PUSH_QUEUE_CAPACITY: usize = 256;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use super::snapshot::{SnapshotUpload, SNAPSHOT_FILE_EXTENSION, SNAPSHOT_MERGED_EXTENSION};
use super::{
    blob_path, entry_meta_path, hex_hash, rclone, MergeEntryOutcome, RcloneRcd, SharedCachePaths,
    SnapshotStore, BLOBS_DIR_NAME, ENTRIES_DIR_NAME, SNAPSHOTS_DIR_NAME,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConfig {
    pub fs_base: String,
    pub sync_timeout: Duration,
    pub timeout_disable_threshold: usize,
    pub rclone_concurrency: usize,
}

/// Run-wide remote state shared across all `RemoteSync` clones.
///
/// `RemoteSync` is cloned during restore iteration; the `Arc` wrapping this
/// state guarantees every clone observes the same disable flag and warns at
/// most once per run.
#[derive(Debug)]
struct RemoteState {
    /// Once set, all remote operations are skipped for the rest of the run.
    disabled: AtomicBool,
    /// Ensures the "remote cache disabled" warning is emitted only once.
    warned: AtomicBool,
    /// Consecutive timeout streak; queued rcd backpressure can time out transiently.
    consecutive_timeouts: AtomicUsize,
    timeout_disable_threshold: usize,
}

impl RemoteState {
    fn new(timeout_disable_threshold: usize) -> Self {
        Self {
            disabled: AtomicBool::new(false),
            warned: AtomicBool::new(false),
            consecutive_timeouts: AtomicUsize::new(0),
            // A threshold of 0 would disable the remote on the first queued
            // timeout, defeating the point of the backpressure policy. Clamp to
            // at least 1 so the invariant holds regardless of the caller.
            timeout_disable_threshold: timeout_disable_threshold.max(1),
        }
    }

    fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::Acquire)
    }

    fn disable_with_warning(&self, reason: &str) {
        self.disabled.store(true, Ordering::Release);
        self.consecutive_timeouts.store(0, Ordering::Release);
        if !self.warned.swap(true, Ordering::AcqRel) {
            eprintln!("warning: remote cache disabled: {reason}");
        }
    }

    fn record_timeout(&self, reason: &str) {
        let streak = self.consecutive_timeouts.fetch_add(1, Ordering::AcqRel) + 1;
        if streak >= self.timeout_disable_threshold {
            self.disable_with_warning(reason);
        }
    }

    fn record_success(&self) {
        self.consecutive_timeouts.store(0, Ordering::Release);
    }
}

fn is_missing_local_source_copy_error(err: &rclone::RcloneError) -> bool {
    let rclone::RcloneError::HttpStatus { status, body } = err else {
        return false;
    };
    if *status != 500 {
        return false;
    }

    let body = body.to_ascii_lowercase();
    (body.contains("failed to open source object") || body.contains("object not found"))
        && (body.contains("lstat") || body.contains("srcremote"))
        && body.contains("no such file")
}

#[derive(Debug, Clone)]
pub struct RemoteSync {
    pub(crate) rclone: Arc<RcloneRcd>,
    pub(crate) remote_base_fs: String,
    state: Arc<RemoteState>,
    push_queue: Arc<PushQueue>,
}

#[derive(Debug)]
struct PushQueue {
    tx: Mutex<Option<SyncSender<PushMsg>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Debug)]
enum PushMsg {
    Push(OwnedPushArtifacts),
    #[cfg(any(test, doctest))]
    Flush(std::sync::mpsc::Sender<()>),
}

#[derive(Debug)]
pub(crate) struct OwnedPushArtifacts {
    pub(crate) paths: Arc<SharedCachePaths>,
    pub(crate) commit_key: String,
    pub(crate) outputs_hash: [u8; 32],
    pub(crate) input_key: [u8; 32],
    pub(crate) has_outputs: bool,
    pub(crate) merge: MergeEntryOutcome,
}

/// Inputs for [`RemoteSync::push_store_artifacts`].
pub(crate) struct PushArtifacts<'a> {
    pub(crate) paths: &'a SharedCachePaths,
    pub(crate) commit_key: &'a str,
    pub(crate) outputs_hash: &'a [u8; 32],
    pub(crate) input_key: &'a [u8; 32],
    pub(crate) has_outputs: bool,
    pub(crate) merge: MergeEntryOutcome,
}

impl RemoteSync {
    #[must_use]
    pub(crate) fn new(
        rclone: Arc<RcloneRcd>,
        remote_base_fs: impl Into<String>,
        timeout_disable_threshold: usize,
    ) -> Self {
        let state = Arc::new(RemoteState::new(timeout_disable_threshold));
        let mut remote = Self {
            rclone,
            remote_base_fs: remote_base_fs.into(),
            state,
            push_queue: Arc::new(PushQueue {
                tx: Mutex::new(None),
                worker: Mutex::new(None),
            }),
        };
        remote.start_push_queue();
        remote
    }

    pub(crate) fn from_config(config: RemoteConfig) -> Result<Self, rclone::RcloneError> {
        let rclone = Arc::new(RcloneRcd::with_concurrency_limit(
            config.sync_timeout,
            config.rclone_concurrency,
        )?);
        Ok(Self::new(
            rclone,
            config.fs_base,
            config.timeout_disable_threshold,
        ))
    }

    pub(crate) fn is_disabled(&self) -> bool {
        self.state.is_disabled()
    }

    fn record_remote_success(&self) {
        self.state.record_success();
    }

    /// Flip the run-wide disable flag from a typed rclone error and warn once.
    ///
    /// A `404` (object/directory not found) is a normal cache MISS — the
    /// bucket simply has no remote shards/blob yet — and must NOT disable
    /// the remote.
    /// Only genuine health failures (timeout, unavailable, process/request
    /// errors, other HTTP statuses) trip the run-wide disable flag.
    fn record_remote_error(&self, err: &rclone::RcloneError) {
        if matches!(err, rclone::RcloneError::HttpStatus { status: 404, .. })
            || is_missing_local_source_copy_error(err)
        {
            return;
        }
        match err {
            rclone::RcloneError::Timeout { .. } => {
                self.state.record_timeout(&remote_disable_reason(err));
            }
            rclone::RcloneError::RemoteUnavailable { .. }
            | rclone::RcloneError::Process { .. }
            | rclone::RcloneError::HttpStatus { .. }
            | rclone::RcloneError::Rc { .. }
            | rclone::RcloneError::Request { .. }
            | rclone::RcloneError::Decode(_)
            | rclone::RcloneError::Io(_) => {
                self.state.disable_with_warning(&remote_disable_reason(err));
            }
        }
    }

    /// Shut the rclone daemon down at run end (best-effort).
    pub(crate) fn shutdown(&self) {
        let _ = self.rclone.shutdown(self.rclone.default_timeout());
    }

    /// Test-only: whether the run-wide remote-disable flag has been tripped.
    #[cfg(test)]
    pub(crate) fn is_disabled_for_test(&self) -> bool {
        self.is_disabled()
    }

    fn snapshots_fs(&self, commit_key: &str) -> String {
        format!(
            "{}/{SNAPSHOTS_DIR_NAME}/{commit_key}",
            self.remote_base_fs.trim_end_matches('/')
        )
    }

    fn blobs_fs(&self) -> String {
        format!(
            "{}/{BLOBS_DIR_NAME}",
            self.remote_base_fs.trim_end_matches('/')
        )
    }

    fn entries_fs(&self) -> String {
        format!(
            "{}/{ENTRIES_DIR_NAME}",
            self.remote_base_fs.trim_end_matches('/')
        )
    }
}

/// Turns an rclone error into a short, human-readable reason string, used by
/// `record_remote_error` when it records a timeout or disables the remote
/// cache for the rest of the run.
fn remote_disable_reason(err: &rclone::RcloneError) -> String {
    match err {
        rclone::RcloneError::Timeout { timeout } => {
            format!("sync timed out after {}s", timeout.as_secs())
        }
        rclone::RcloneError::RemoteUnavailable { reason } => reason.clone(),
        rclone::RcloneError::HttpStatus { status, .. } => {
            format!("remote operation failed with HTTP {status}")
        }
        rclone::RcloneError::Rc { message } => format!("remote operation failed: {message}"),
        rclone::RcloneError::Request { reason } => format!("remote request failed: {reason}"),
        rclone::RcloneError::Process { reason } => format!("remote process failed: {reason}"),
        rclone::RcloneError::Decode(err) => format!("remote response decode failed: {err}"),
        rclone::RcloneError::Io(err) => format!("remote I/O failed: {err}"),
    }
}

impl RemoteSync {
    fn start_push_queue(&mut self) {
        let capacity = std::env::var("LUCHTA_SHARED_CACHE_PUSH_QUEUE_CAPACITY")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .map(|value| value.max(1))
            .unwrap_or(DEFAULT_PUSH_QUEUE_CAPACITY);
        let (tx, rx) = mpsc::sync_channel(capacity);
        let worker_remote = self.clone();
        let worker = std::thread::spawn(move || {
            for msg in rx {
                match msg {
                    PushMsg::Push(push) => worker_remote.push_store_artifacts_owned(push),
                    #[cfg(any(test, doctest))]
                    PushMsg::Flush(ack) => {
                        let _ = ack.send(());
                    }
                }
            }
        });
        *self
            .push_queue
            .tx
            .lock()
            .expect("push queue tx mutex poisoned") = Some(tx);
        *self
            .push_queue
            .worker
            .lock()
            .expect("push queue worker mutex poisoned") = Some(worker);
    }

    pub(crate) fn enqueue_push_store_artifacts(&self, push: OwnedPushArtifacts) {
        let Some(tx) = self
            .push_queue
            .tx
            .lock()
            .expect("push queue tx mutex poisoned")
            .as_ref()
            .cloned()
        else {
            self.push_store_artifacts_owned(push);
            return;
        };
        if tx.send(PushMsg::Push(push)).is_err() {
            eprintln!("debug: remote push queue closed before enqueue completed");
        }
    }

    #[cfg(any(test, doctest))]
    pub(crate) fn drain_push_queue(&self) {
        let Some(tx) = self
            .push_queue
            .tx
            .lock()
            .expect("push queue tx mutex poisoned")
            .as_ref()
            .cloned()
        else {
            return;
        };
        let (ack_tx, ack_rx) = mpsc::channel();
        if tx.send(PushMsg::Flush(ack_tx)).is_err() {
            return;
        }
        let _ = ack_rx.recv();
    }

    pub(crate) fn flush_push_queue(&self) {
        self.push_queue
            .tx
            .lock()
            .expect("push queue tx mutex poisoned")
            .take();
        if let Some(worker) = self
            .push_queue
            .worker
            .lock()
            .expect("push queue worker mutex poisoned")
            .take()
        {
            let _ = worker.join();
        }
    }

    fn push_store_artifacts_owned(&self, push: OwnedPushArtifacts) {
        self.push_store_artifacts(PushArtifacts {
            paths: &push.paths,
            commit_key: &push.commit_key,
            outputs_hash: &push.outputs_hash,
            input_key: &push.input_key,
            has_outputs: push.has_outputs,
            merge: push.merge,
        });
    }

    pub(crate) fn pull_snapshot_commit(&self, snapshot_store: &SnapshotStore, commit_key: &str) {
        if self.is_disabled() {
            return;
        }
        let remote_fs = self.snapshots_fs(commit_key);
        let local_dir = snapshot_store.paths().snapshots_dir.join(commit_key);
        if let Err(err) = fs::create_dir_all(&local_dir) {
            eprintln!("debug: local snapshot dir prep failed for bucket={commit_key}: {err}");
            return;
        }
        let local_fs = format!(":local:{}", local_dir.display());
        if let Err(err) = self
            .rclone
            .copy_dir(&remote_fs, &local_fs, self.rclone.default_timeout())
        {
            self.record_remote_error(&err);
            eprintln!("debug: remote snapshot copy failed for bucket={commit_key}: {err}");
        } else {
            self.record_remote_success();
        }
    }

    pub(crate) fn pull_entry_meta(
        &self,
        paths: &SharedCachePaths,
        input_key: &[u8; 32],
    ) -> Result<(), rclone::RcloneError> {
        if self.is_disabled() {
            return Ok(());
        }
        let local_path = entry_meta_path(paths, input_key);
        if local_path.exists() {
            return Ok(());
        }
        let file_name = format!("{}.bin", hex_hash(*input_key));
        self.copy_remote_file_down(&self.entries_fs(), &file_name, &local_path)
            .inspect(|_| self.record_remote_success())
            .inspect_err(|err| self.record_remote_error(err))
    }

    pub(crate) fn pull_blob(
        &self,
        paths: &SharedCachePaths,
        outputs_hash: &[u8; 32],
    ) -> Result<(), rclone::RcloneError> {
        if self.is_disabled() {
            return Ok(());
        }
        let file_name = format!("{}.tar.zst", hex_hash(*outputs_hash));
        let local_path = paths.blobs_dir.join(&file_name);
        if local_path.exists() {
            return Ok(());
        }
        self.copy_remote_file_down(&self.blobs_fs(), &file_name, &local_path)
            .inspect(|_| self.record_remote_success())
            .inspect_err(|err| self.record_remote_error(err))
    }

    fn copy_remote_file_down(
        &self,
        src_fs: &str,
        src_remote: &str,
        local_path: &Path,
    ) -> Result<(), rclone::RcloneError> {
        let parent = local_path.parent().ok_or_else(|| {
            rclone::RcloneError::Io(io::Error::other("local cache target missing parent"))
        })?;
        fs::create_dir_all(parent)?;
        let temp_dir = tempfile::Builder::new()
            .prefix("remote-pull-")
            .tempdir_in(parent)?;
        let temp_path = temp_dir
            .path()
            .join(local_path.file_name().unwrap_or_default());
        let dst_fs = format!(":local:{}", temp_dir.path().display());
        let dst_remote = temp_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                rclone::RcloneError::Io(io::Error::other("local cache target not valid utf-8"))
            })?;
        self.rclone.copyfile(
            rclone::CopyFile {
                src_fs,
                src_remote,
                dst_fs: &dst_fs,
                dst_remote,
            },
            self.rclone.default_timeout(),
        )?;
        std::fs::rename(&temp_path, local_path).or_else(|err| {
            if err.kind() == io::ErrorKind::AlreadyExists {
                Ok(())
            } else {
                Err(err)
            }
        })?;
        Ok(())
    }

    pub(crate) fn push_store_artifacts(&self, push: PushArtifacts<'_>) {
        if self.is_disabled() {
            return;
        }
        let PushArtifacts {
            paths,
            commit_key,
            outputs_hash,
            input_key,
            has_outputs,
            merge,
        } = push;

        if has_outputs {
            self.push_blob_if_missing(paths, outputs_hash);
        }
        self.push_entry_meta_if_missing(paths, input_key);

        if self.is_disabled() {
            return;
        }

        let uploaded_new_shard = match merge.new_snapshot_upload.as_ref() {
            Some(upload) => self.push_snapshot_upload(commit_key, upload),
            None => false,
        };

        if !uploaded_new_shard || self.is_disabled() {
            return;
        }

        for shard_id in &merge.subsumed_shard_ids {
            if self.is_disabled() {
                break;
            }
            self.delete_remote_snapshot_file(commit_key, shard_id, SNAPSHOT_FILE_EXTENSION);
            self.delete_remote_snapshot_file(commit_key, shard_id, SNAPSHOT_MERGED_EXTENSION);
        }
    }

    fn push_blob_if_missing(&self, paths: &SharedCachePaths, outputs_hash: &[u8; 32]) {
        let remote_fs = self.blobs_fs();
        let blob_name = format!("{}.tar.zst", hex_hash(*outputs_hash));
        match self
            .rclone
            .stat(&remote_fs, &blob_name, self.rclone.default_timeout())
        {
            Ok(Some(_)) => {
                self.record_remote_success();
                return;
            }
            Ok(None) => {
                self.record_remote_success();
            }
            Err(err) => {
                self.record_remote_error(&err);
                eprintln!("warn: shared cache remote blob stat failed for {blob_name}: {err}");
                return;
            }
        }

        let local_path = blob_path(paths, outputs_hash);
        if let Err(err) = self.copy_local_file_up(&local_path, &remote_fs, &blob_name) {
            self.record_remote_error(&err);
            eprintln!("warn: shared cache remote blob upload failed for {blob_name}: {err}");
        } else {
            self.record_remote_success();
        }
    }

    fn push_entry_meta_if_missing(&self, paths: &SharedCachePaths, input_key: &[u8; 32]) {
        let remote_fs = self.entries_fs();
        let file_name = format!("{}.bin", hex_hash(*input_key));
        match self
            .rclone
            .stat(&remote_fs, &file_name, self.rclone.default_timeout())
        {
            Ok(Some(_)) => {
                self.record_remote_success();
                return;
            }
            Ok(None) => {
                self.record_remote_success();
            }
            Err(err) => {
                self.record_remote_error(&err);
                eprintln!(
                    "warn: shared cache remote entry meta stat failed for {file_name}: {err}"
                );
                return;
            }
        }

        let local_path = entry_meta_path(paths, input_key);
        if let Err(err) = self.copy_local_file_up(&local_path, &remote_fs, &file_name) {
            self.record_remote_error(&err);
            eprintln!("warn: shared cache remote entry meta upload failed for {file_name}: {err}");
        } else {
            self.record_remote_success();
        }
    }

    fn push_snapshot_upload(&self, commit_key: &str, upload: &SnapshotUpload) -> bool {
        let remote_fs = self.snapshots_fs(commit_key);
        let shard_name = format!("{}.{SNAPSHOT_FILE_EXTENSION}", upload.shard_id);
        if let Err(err) = self.copy_bytes_up(&upload.shard_bytes, &remote_fs, &shard_name) {
            self.record_remote_error(&err);
            eprintln!(
                "warn: shared cache remote snapshot upload failed for bucket={commit_key} file={shard_name}: {err}"
            );
            return false;
        }
        self.record_remote_success();

        let merged_name = format!("{}.{SNAPSHOT_MERGED_EXTENSION}", upload.shard_id);
        if let Err(err) = self.copy_bytes_up(&upload.merged_bytes, &remote_fs, &merged_name) {
            self.record_remote_error(&err);
            eprintln!(
                "warn: shared cache remote snapshot upload failed for bucket={commit_key} file={merged_name}: {err}"
            );
            return false;
        }
        self.record_remote_success();

        true
    }

    fn delete_remote_snapshot_file(&self, commit_key: &str, shard_id: &str, extension: &str) {
        if self.is_disabled() {
            return;
        }
        let remote_fs = self.snapshots_fs(commit_key);
        let remote_name = format!("{shard_id}.{extension}");
        if let Err(err) =
            self.rclone
                .deletefile(&remote_fs, &remote_name, self.rclone.default_timeout())
        {
            if matches!(err, rclone::RcloneError::HttpStatus { status: 404, .. }) {
                self.record_remote_success();
                return;
            }
            self.record_remote_error(&err);
            eprintln!(
                "warn: shared cache remote snapshot delete failed for bucket={commit_key} file={remote_name}: {err}"
            );
        } else {
            self.record_remote_success();
        }
    }

    fn copy_bytes_up(
        &self,
        bytes: &[u8],
        dst_fs: &str,
        dst_remote: &str,
    ) -> Result<(), rclone::RcloneError> {
        self.rclone.upload_bytes(
            rclone::UploadFile {
                fs: dst_fs,
                remote_dir: "",
                file_name: dst_remote,
                bytes,
            },
            self.rclone.default_timeout(),
        )
    }

    fn copy_local_file_up(
        &self,
        local_path: &Path,
        dst_fs: &str,
        dst_remote: &str,
    ) -> Result<(), rclone::RcloneError> {
        let parent = local_path.parent().ok_or_else(|| {
            rclone::RcloneError::Io(io::Error::other("local cache source missing parent"))
        })?;
        let src_fs = format!(":local:{}", parent.display());
        let src_remote = local_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                rclone::RcloneError::Io(io::Error::other("local cache source not valid utf-8"))
            })?;
        self.rclone.copyfile(
            rclone::CopyFile {
                src_fs: &src_fs,
                src_remote,
                dst_fs,
                dst_remote,
            },
            self.rclone.default_timeout(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::snapshot::snapshot_bincode_config;
    use crate::shared::tests::{create_commit, sample_record, setup_git_repo};
    use crate::shared::{
        derive_input_key, input_key_hex, MergeResult, OpenExtras, SharedCache, Snapshot,
        SnapshotEntry, StoreOutcome, SNAPSHOT_SCHEMA_VERSION,
    };
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    // Regression: a remote-configured `SharedCache` (which owns an `RcloneRcd`
    // with its own tokio runtime) used to PANIC when its `Arc` was dropped from
    // inside the build's tokio runtime ("Cannot drop a runtime in a context
    // where blocking is not allowed"). The real `luchta run` drops the cache
    // inside an async task, so this must not panic. Not rclone-gated: the bug is
    // dropping the owned runtime in an async context, independent of whether the
    // daemon ever spawned.
    #[test]
    fn remote_cache_drops_cleanly_inside_async_context() {
        use std::sync::Arc;

        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());
        let cache_dir = TempDir::new().unwrap();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async move {
            let cache = SharedCache::open_with_remote(
                temp_repo.path(),
                1_000_000,
                10,
                OpenExtras {
                    cache_dir: Some(cache_dir.path()),
                    remote: Some(RemoteConfig {
                        fs_base: ":local:/tmp/luchta-async-drop-test".to_string(),
                        sync_timeout: std::time::Duration::from_secs(1),
                        timeout_disable_threshold: 8,
                        rclone_concurrency: 16,
                    }),
                },
            )
            .unwrap();
            let cache = Arc::new(cache);
            // Drop the last Arc reference inside the async context — must not panic.
            drop(cache);
        });
    }

    fn should_run_rclone_test() -> bool {
        match std::env::var("LUCHTA_TEST_RCLONE") {
            Ok(value) if value == "1" => std::process::Command::new("rclone")
                .arg("version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false),
            _ => false,
        }
    }

    fn remote_snapshot_files(remote_root: &Path, shard_key: &str) -> Vec<String> {
        let shard_dir = remote_root.join("snapshots").join(shard_key);
        let Ok(read_dir) = fs::read_dir(shard_dir) else {
            return Vec::new();
        };
        let mut entries = read_dir
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }

    fn remote_blob_path(remote_root: &Path, outputs_hash: &[u8; 32]) -> PathBuf {
        remote_root
            .join("blobs")
            .join(format!("{}.tar.zst", hex_hash(*outputs_hash)))
    }

    fn open_cache_with_remote(
        repo_root: &Path,
        cache_dir: &Path,
        remote: &RemoteSync,
    ) -> SharedCache {
        SharedCache::open_with_remote(
            repo_root,
            1_000_000,
            10,
            OpenExtras {
                cache_dir: Some(cache_dir),
                remote: Some(RemoteConfig {
                    fs_base: remote.remote_base_fs.clone(),
                    sync_timeout: remote.rclone.default_timeout(),
                    timeout_disable_threshold: 8,
                    rclone_concurrency: 16,
                }),
            },
        )
        .unwrap()
    }

    fn write_dist_file(package_dir: &Path, body: &str) {
        fs::create_dir_all(package_dir.join("dist")).unwrap();
        fs::write(package_dir.join("dist/main.js"), body).unwrap();
    }

    struct RemoteHarness {
        temp_repo: TempDir,
        remote_root: TempDir,
        local_cache: TempDir,
        package_dir: PathBuf,
        remote: RemoteSync,
    }

    impl RemoteHarness {
        fn new(file_body: &str) -> Self {
            let temp_repo = TempDir::new().unwrap();
            setup_git_repo(temp_repo.path());
            let remote_root = TempDir::new().unwrap();
            let local_cache = TempDir::new().unwrap();
            let package_dir = temp_repo.path().join("pkg");
            write_dist_file(&package_dir, file_body);
            let remote = RemoteSync::new(
                Arc::new(RcloneRcd::new(Duration::from_secs(10)).unwrap()),
                format!(":local:{}", remote_root.path().display()),
                8,
            );
            Self {
                temp_repo,
                remote_root,
                local_cache,
                package_dir,
                remote,
            }
        }

        /// Builds a `SharedCache` pointed at this harness's remote and local
        /// cache dir. The cache writes to a computed `<YYYYMMDD>-<shard>`
        /// bucket key (today's date, a nonce-selected shard) — there is no
        /// commit-derived key a test can predict up front, so callers that
        /// need to know where a `store()` on this cache landed must read it
        /// back via `write_bucket_key()` (see `StoredRemoteCase::shard_key`).
        fn cache(&self) -> SharedCache {
            open_cache_with_remote(self.temp_repo.path(), self.local_cache.path(), &self.remote)
        }
    }

    struct StoredRemoteCase {
        harness: RemoteHarness,
        cache: SharedCache,
        input_key: [u8; 32],
        outputs_hash: [u8; 32],
    }

    impl StoredRemoteCase {
        /// The shard key `cache` actually wrote its entry to.
        fn shard_key(&self) -> &str {
            self.cache.write_bucket_key().expect("write key")
        }
    }

    fn seed_remote_store(
        file_body: &str,
        outputs_hash: [u8; 32],
        duration_ms: u64,
        streams: (&'static [u8], &'static [u8]),
    ) -> StoredRemoteCase {
        let (stdout, stderr) = streams;
        let harness = RemoteHarness::new(file_body);
        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
        let cache = harness.cache();
        let outcome = cache
            .store(
                "pkg#build",
                &input_key,
                &outputs_hash,
                &harness.package_dir,
                &[PathBuf::from("dist/main.js")],
                &sample_record(true, duration_ms),
                stdout,
                stderr,
                &[],
                harness.temp_repo.path(),
            )
            .unwrap();
        cache.flush_push_queue();
        assert!(matches!(outcome, StoreOutcome::Stored));
        StoredRemoteCase {
            harness,
            cache,
            input_key,
            outputs_hash,
        }
    }

    fn assert_remote_has_blob(remote_root: &Path, outputs_hash: &[u8; 32]) {
        assert!(remote_blob_path(remote_root, outputs_hash).exists());
    }

    fn assert_snapshot_shard_count(files: &[String], bincode_count: usize, merged_count: usize) {
        assert_eq!(
            files
                .iter()
                .filter(|name| name.ends_with(".bincode"))
                .count(),
            bincode_count
        );
        assert_eq!(
            files
                .iter()
                .filter(|name| name.ends_with(".merged"))
                .count(),
            merged_count
        );
    }

    fn assert_remote_store_layout(remote_root: &Path, shard_key: &str, outputs_hash: &[u8; 32]) {
        let files = remote_snapshot_files(remote_root, shard_key);
        assert_eq!(files.len(), 2);
        assert_snapshot_shard_count(&files, 1, 1);
        assert_remote_has_blob(remote_root, outputs_hash);
    }

    fn assert_remote_restore_result(
        restore_dir: &Path,
        hit: &crate::shared::RestoredHit,
        expected_streams: (&[u8], &[u8]),
        expected_body: &str,
    ) {
        let (expected_stdout, expected_stderr) = expected_streams;
        assert_eq!(hit.stdout, expected_stdout);
        assert_eq!(hit.stderr, expected_stderr);
        assert_eq!(
            fs::read_to_string(restore_dir.join("dist/main.js")).unwrap(),
            expected_body
        );
    }

    fn seed_snapshot_entry(
        seed_cache: &SharedCache,
        remote_seed: &RemoteSync,
        shard_key: &str,
        entry: SnapshotEntry,
    ) -> String {
        let outputs_hash = entry.outputs_hash;
        let input_key = entry.input_key;
        let merge = seed_cache
            .snapshot_store
            .merge_entry_with_outcome(shard_key, entry);
        let shard_id = merge.new_snapshot_upload.as_ref().unwrap().shard_id.clone();
        remote_seed.push_store_artifacts(PushArtifacts {
            paths: seed_cache.paths(),
            commit_key: shard_key,
            outputs_hash: &outputs_hash,
            input_key: &input_key,
            has_outputs: true,
            merge,
        });
        shard_id
    }

    fn seed_remote_snapshot_entries(
        repo_root: &Path,
        shard_key: &str,
        remote_root: &Path,
    ) -> (SharedCache, String, String) {
        let remote_seed_root = TempDir::new().unwrap();
        let seed_cache = SharedCache::open_with_cache_dir(
            repo_root,
            1_000_000,
            10,
            Some(remote_seed_root.path()),
        )
        .unwrap();
        let remote_seed = RemoteSync::new(
            Arc::new(RcloneRcd::new(Duration::from_secs(10)).unwrap()),
            format!(":local:{}", remote_root.display()),
            8,
        );
        let merge1_id = seed_snapshot_entry(
            &seed_cache,
            &remote_seed,
            shard_key,
            SnapshotEntry {
                task_id: "pkg#a".to_string(),
                input_key: derive_input_key([11; 32], [12; 32], [13; 32], [14; 32], [5; 32]),
                outputs_hash: [21; 32],
                task_spec_hash: [31; 32],
                env_hash: [41; 32],
                pkg_dep_hash: [51; 32],
                duration_ms: 100,
                output_bytes: 10,
                cached_at_unix_ms: 1,
                tool_version: None,
            },
        );
        let merge2_id = seed_snapshot_entry(
            &seed_cache,
            &remote_seed,
            shard_key,
            SnapshotEntry {
                task_id: "pkg#b".to_string(),
                input_key: derive_input_key([15; 32], [16; 32], [17; 32], [18; 32], [5; 32]),
                outputs_hash: [22; 32],
                task_spec_hash: [32; 32],
                env_hash: [42; 32],
                pkg_dep_hash: [52; 32],
                duration_ms: 110,
                output_bytes: 11,
                cached_at_unix_ms: 2,
                tool_version: None,
            },
        );
        (seed_cache, merge1_id, merge2_id)
    }

    #[test]
    fn missing_local_source_copy_error_does_not_disable_remote() {
        let remote = RemoteSync::new(
            Arc::new(RcloneRcd::new(Duration::from_secs(1)).unwrap()),
            ":local:/tmp/nonexistent-remote".to_string(),
            8,
        );
        let err = rclone::RcloneError::HttpStatus {
            status: 500,
            body: "failed to open source object: lstat /tmp/cache/snapshots/abc/123.merged: no such file or directory".to_string(),
        };

        remote.record_remote_error(&err);

        assert!(!remote.is_disabled_for_test());
    }

    #[test]
    fn timeout_below_threshold_does_not_disable() {
        let remote = RemoteSync::new(
            Arc::new(RcloneRcd::new(Duration::from_secs(1)).unwrap()),
            ":local:/tmp/nonexistent-remote".to_string(),
            3,
        );
        let timeout = rclone::RcloneError::Timeout {
            timeout: Duration::from_secs(30),
        };

        remote.record_remote_error(&timeout);
        remote.record_remote_error(&timeout);

        assert!(!remote.is_disabled_for_test());
    }

    #[test]
    fn timeout_reaching_threshold_disables() {
        let remote = RemoteSync::new(
            Arc::new(RcloneRcd::new(Duration::from_secs(1)).unwrap()),
            ":local:/tmp/nonexistent-remote".to_string(),
            3,
        );
        let timeout = rclone::RcloneError::Timeout {
            timeout: Duration::from_secs(30),
        };

        remote.record_remote_error(&timeout);
        remote.record_remote_error(&timeout);
        assert!(!remote.is_disabled_for_test());
        remote.record_remote_error(&timeout);

        assert!(remote.is_disabled_for_test());
    }

    #[test]
    fn success_resets_timeout_counter() {
        let remote = RemoteSync::new(
            Arc::new(RcloneRcd::new(Duration::from_secs(1)).unwrap()),
            ":local:/tmp/nonexistent-remote".to_string(),
            3,
        );
        let timeout = rclone::RcloneError::Timeout {
            timeout: Duration::from_secs(30),
        };

        remote.record_remote_error(&timeout);
        remote.record_remote_error(&timeout);
        remote.record_remote_success();
        remote.record_remote_error(&timeout);
        remote.record_remote_error(&timeout);

        assert!(!remote.is_disabled_for_test());
        remote.record_remote_error(&timeout);
        assert!(remote.is_disabled_for_test());
    }

    #[test]
    fn unavailable_and_process_disable_immediately() {
        let unavailable = RemoteSync::new(
            Arc::new(RcloneRcd::new(Duration::from_secs(1)).unwrap()),
            ":local:/tmp/nonexistent-remote".to_string(),
            3,
        );
        unavailable.record_remote_error(&rclone::RcloneError::RemoteUnavailable {
            reason: "down".to_string(),
        });
        assert!(unavailable.is_disabled_for_test());

        let process = RemoteSync::new(
            Arc::new(RcloneRcd::new(Duration::from_secs(1)).unwrap()),
            ":local:/tmp/nonexistent-remote".to_string(),
            3,
        );
        process.record_remote_error(&rclone::RcloneError::Process {
            reason: "dead".to_string(),
        });
        assert!(process.is_disabled_for_test());
    }

    #[test]
    fn not_found_does_not_disable_remote() {
        let remote = RemoteSync::new(
            Arc::new(RcloneRcd::new(Duration::from_secs(1)).unwrap()),
            ":local:/tmp/nonexistent-remote".to_string(),
            3,
        );
        remote.record_remote_error(&rclone::RcloneError::HttpStatus {
            status: 404,
            body: "missing".to_string(),
        });
        assert!(!remote.is_disabled_for_test());
    }

    #[test]
    fn flush_push_queue_drains_enqueued_pushes() {
        if !should_run_rclone_test() {
            eprintln!("skipping rclone queue flush test; rclone not on PATH or LUCHTA_TEST_RCLONE disabled");
            return;
        }

        let harness = RemoteHarness::new("queue-body");
        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
        let outputs_hash = [0x31; 32];
        let cache = harness.cache();
        cache
            .store(
                "pkg#build",
                &input_key,
                &outputs_hash,
                &harness.package_dir,
                &[PathBuf::from("dist/main.js")],
                &sample_record(true, 240),
                b"stdout-queue",
                b"stderr-queue",
                &[],
                harness.temp_repo.path(),
            )
            .unwrap();

        let remote_blob = harness
            .remote_root
            .path()
            .join("blobs")
            .join(format!("{}.tar.zst", hex_hash(outputs_hash)));
        assert!(!remote_blob.exists());

        cache.flush_push_queue();
        assert!(remote_blob.exists());
    }

    #[test]
    fn push_queue_full_blocks_instead_of_dropping() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::mpsc::channel;
        use std::thread;

        let (tx, rx) = mpsc::sync_channel::<PushMsg>(1);
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let (processed_tx, processed_rx) = channel();
        let processed = Arc::new(AtomicUsize::new(0));
        let processed_in_worker = Arc::clone(&processed);
        let worker = thread::spawn(move || {
            for msg in rx {
                match msg {
                    PushMsg::Push(_) => {
                        let count = processed_in_worker.fetch_add(1, Ordering::SeqCst);
                        started_tx.send(count).unwrap();
                        release_rx.recv().unwrap();
                        processed_tx.send(()).unwrap();
                    }
                    PushMsg::Flush(ack) => {
                        let _ = ack.send(());
                    }
                }
            }
        });

        let make_push = |n| OwnedPushArtifacts {
            paths: Arc::new(SharedCachePaths {
                root: PathBuf::from(format!("/tmp/luchta-test-{n}")),
                blobs_dir: PathBuf::from(format!("/tmp/luchta-test-{n}/blobs")),
                snapshots_dir: PathBuf::from(format!("/tmp/luchta-test-{n}/snapshots")),
                entries_dir: PathBuf::from(format!("/tmp/luchta-test-{n}/entries")),
            }),
            commit_key: format!("commit-{n}"),
            outputs_hash: [n as u8; 32],
            input_key: [n as u8; 32],
            has_outputs: true,
            merge: MergeEntryOutcome {
                result: MergeResult::Inserted,
                new_snapshot_upload: None,
                subsumed_shard_ids: Vec::new(),
            },
        };

        tx.send(PushMsg::Push(make_push(1))).unwrap();
        started_rx.recv().unwrap();
        tx.send(PushMsg::Push(make_push(2))).unwrap();

        let (send_result_tx, send_result_rx) = channel();
        let send_third = {
            let tx = tx.clone();
            thread::spawn(move || {
                let sent = tx.send(PushMsg::Push(make_push(3))).is_ok();
                send_result_tx.send(sent).unwrap();
            })
        };

        assert!(processed_rx
            .recv_timeout(Duration::from_millis(50))
            .is_err());
        release_tx.send(()).unwrap();
        processed_rx.recv().unwrap();
        started_rx.recv().unwrap();
        send_third.join().unwrap();
        assert!(send_result_rx.recv().unwrap());

        release_tx.send(()).unwrap();
        processed_rx.recv().unwrap();
        started_rx.recv().unwrap();
        release_tx.send(()).unwrap();
        processed_rx.recv().unwrap();

        drop(tx);
        worker.join().unwrap();
        assert_eq!(processed.load(Ordering::SeqCst), 3);
    }
    fn seed_guard_blob(local_cache: &TempDir, outputs_hash: [u8; 32], body: &[u8]) {
        let local_blob_dir = local_cache.path().join("blobs");
        fs::create_dir_all(&local_blob_dir).unwrap();
        fs::write(
            local_blob_dir.join(format!("{}.tar.zst", hex_hash(outputs_hash))),
            body,
        )
        .unwrap();
    }

    fn assert_snapshot_upload_failure_preserves_remote(
        remote_root: &Path,
        shard_key: &str,
        before: &[String],
        expected_present_ids: &[&str],
    ) {
        let remote_after = remote_snapshot_files(remote_root, shard_key);
        assert_eq!(remote_after, before);
        for shard_id in expected_present_ids {
            assert!(remote_after.iter().any(|name| name.starts_with(shard_id)));
        }
        assert!(!remote_after
            .iter()
            .any(|name| name.starts_with("subsuming-shard")));
    }

    #[test]
    fn remote_store_skips_remote_delete_when_snapshot_upload_fails() {
        if !should_run_rclone_test() {
            eprintln!("skipping rclone-gated shared-cache upload-failure guard test; rclone not on PATH or LUCHTA_TEST_RCLONE disabled");
            return;
        }

        let harness = RemoteHarness::new("console.log('guard');\n");
        let cache = harness.cache();
        let shard_key = cache.write_bucket_key().expect("write key").to_string();
        let (seed_cache, _merge1_id, merge2_id) = seed_remote_snapshot_entries(
            harness.temp_repo.path(),
            &shard_key,
            harness.remote_root.path(),
        );
        let remote_before = remote_snapshot_files(harness.remote_root.path(), &shard_key);
        assert_eq!(remote_before.len(), 2);
        seed_guard_blob(&harness.local_cache, [23; 32], b"blob-23");
        let mut merge3 = cache.snapshot_store.merge_entry_with_outcome(
            &shard_key,
            SnapshotEntry {
                task_id: "pkg#c".to_string(),
                input_key: derive_input_key([19; 32], [20; 32], [21; 32], [22; 32], [5; 32]),
                outputs_hash: [23; 32],
                task_spec_hash: [33; 32],
                env_hash: [43; 32],
                pkg_dep_hash: [53; 32],
                duration_ms: 120,
                output_bytes: 12,
                cached_at_unix_ms: 3,
                tool_version: None,
            },
        );
        let upload = merge3.new_snapshot_upload.as_mut().unwrap();
        let expected_subsumed = merge3.subsumed_shard_ids.clone();
        upload.shard_id = "subsuming-shard".to_string();

        // Force the new shard's `operations/uploadfile` to fail ON THE REAL
        // REMOTE that we then verify: pre-create the upload's destination path
        // as a DIRECTORY, so rclone returns HTTP 500 ("is a directory") when it
        // tries to write the file. Crucially the failure is on the same remote
        // root (`harness.remote_root`) whose snapshot dir we assert against, and
        // deletes of the existing shards on that root would still succeed — so a
        // regression that deleted the subsumed shards after a failed upload WOULD
        // be observable here. The push must instead SKIP those deletes.
        let blocking_path = harness
            .remote_root
            .path()
            .join("snapshots")
            .join(&shard_key)
            .join(format!("subsuming-shard.{SNAPSHOT_FILE_EXTENSION}"));
        fs::create_dir_all(&blocking_path).unwrap();
        let input_key = derive_input_key([19; 32], [20; 32], [21; 32], [22; 32], [5; 32]);
        harness.remote.push_store_artifacts(PushArtifacts {
            paths: cache.paths(),
            commit_key: &shard_key,
            outputs_hash: &[23; 32],
            input_key: &input_key,
            has_outputs: true,
            merge: merge3,
        });
        // The failed upload must not have disabled the remote permanently in a
        // way that hides a delete — but it must have skipped the subsumed-shard
        // deletes. Remove the blocking dir so the snapshot listing below only
        // sees the original shard files.
        fs::remove_dir(&blocking_path).unwrap();
        drop(seed_cache);
        let expected_present_ids: Vec<&str> = std::iter::once(merge2_id.as_str())
            .chain(expected_subsumed.iter().map(String::as_str))
            .collect();
        assert_snapshot_upload_failure_preserves_remote(
            harness.remote_root.path(),
            &shard_key,
            &remote_before,
            &expected_present_ids,
        );
    }

    #[test]
    fn remote_restore_bad_remote_degrades_to_miss() {
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        let commit = create_commit(temp_repo.path());
        let cache_dir = TempDir::new().unwrap();
        let local_cache_dir = cache_dir.path().join("local");
        let cache = SharedCache::open_with_remote(
            temp_repo.path(),
            1_000_000,
            10,
            crate::shared::OpenExtras {
                cache_dir: Some(&local_cache_dir),
                remote: Some(RemoteConfig {
                    fs_base: ":local:/definitely/missing/luchta-remote".to_string(),
                    sync_timeout: Duration::from_secs(2),
                    timeout_disable_threshold: 8,
                    rclone_concurrency: 16,
                }),
            },
        )
        .unwrap();

        let input_key = [9; 32];
        let snapshot = Snapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            entries: std::iter::once((
                input_key_hex(input_key),
                SnapshotEntry {
                    task_id: "pkg#build".to_string(),
                    input_key,
                    outputs_hash: [3; 32],
                    task_spec_hash: [1; 32],
                    env_hash: [2; 32],
                    pkg_dep_hash: [4; 32],
                    duration_ms: 100,
                    output_bytes: 10,
                    cached_at_unix_ms: 1,
                    tool_version: None,
                },
            ))
            .collect::<BTreeMap<_, _>>(),
        };
        let remote_commit_dir = cache_dir.path().join("remote/snapshots").join(&commit);
        fs::create_dir_all(&remote_commit_dir).unwrap();
        fs::write(
            remote_commit_dir.join(format!("missing.{SNAPSHOT_FILE_EXTENSION}")),
            bincode::serde::encode_to_vec(&snapshot, snapshot_bincode_config()).unwrap(),
        )
        .unwrap();

        let restore_dir = temp_repo.path().join("restore-miss");
        fs::create_dir_all(&restore_dir).unwrap();
        let candidates: Vec<_> = cache
            .try_restore_candidates("pkg#build", &input_key, &restore_dir)
            .collect();
        assert!(candidates.is_empty());
    }

    #[test]
    fn remote_unreachable_trips_disable_flag_and_build_continues() {
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());
        let cache_dir = TempDir::new().unwrap();
        let local_cache_dir = cache_dir.path().join("local");
        let package_dir = temp_repo.path().join("pkg");
        write_dist_file(&package_dir, "console.log('x');");

        let cache = SharedCache::open_with_remote(
            temp_repo.path(),
            1_000_000,
            10,
            crate::shared::OpenExtras {
                cache_dir: Some(&local_cache_dir),
                remote: Some(RemoteConfig {
                    fs_base: "nonexistent-luchta-remote-xyz:".to_string(),
                    sync_timeout: Duration::from_secs(2),
                    timeout_disable_threshold: 8,
                    rclone_concurrency: 16,
                }),
            },
        )
        .unwrap();
        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
        let restore_dir = temp_repo.path().join("restore-degrade");
        fs::create_dir_all(&restore_dir).unwrap();

        let first: Vec<_> = cache
            .try_restore_candidates("pkg#build", &input_key, &restore_dir)
            .collect();
        assert!(first.is_empty());
        assert!(cache.remote.as_ref().unwrap().is_disabled_for_test());

        let outcome = cache
            .store(
                "pkg#build",
                &input_key,
                &[7; 32],
                &package_dir,
                &[PathBuf::from("dist/main.js")],
                &sample_record(true, 200),
                b"stdout",
                b"stderr",
                &[],
                temp_repo.path(),
            )
            .unwrap();
        cache.flush_push_queue();
        assert!(matches!(outcome, StoreOutcome::Stored));
    }

    #[test]
    fn remote_restore_pulls_snapshots_and_blob_when_rclone_enabled() {
        if !should_run_rclone_test() {
            eprintln!("skipping rclone-gated shared-cache pull test; rclone not on PATH or LUCHTA_TEST_RCLONE disabled");
            return;
        }

        let seed = seed_remote_store(
            "console.log('forge');\n",
            [8; 32],
            500,
            (b"stdout-remote", b"stderr-remote"),
        );
        let local_cache = TempDir::new().unwrap();
        let pull_cache = open_cache_with_remote(
            seed.harness.temp_repo.path(),
            local_cache.path(),
            &seed.harness.remote,
        );
        let restore_dir = seed.harness.temp_repo.path().join("restore-remote");
        fs::create_dir_all(&restore_dir).unwrap();

        let mut candidates: Vec<_> = pull_cache
            .try_restore_candidates("pkg#build", &seed.input_key, &restore_dir)
            .collect();
        assert_eq!(candidates.len(), 1);
        let (hit, written_paths) = candidates.remove(0).commit().unwrap();
        assert_eq!(written_paths, vec![restore_dir.join("dist/main.js")]);
        assert_remote_restore_result(
            &restore_dir,
            &hit,
            (b"stdout-remote", b"stderr-remote"),
            "console.log('forge');\n",
        );
        assert!(local_cache
            .path()
            .join("snapshots")
            .join(seed.shard_key())
            .exists());
        assert!(local_cache
            .path()
            .join("blobs")
            .read_dir()
            .unwrap()
            .next()
            .is_some());
    }

    #[test]
    fn remote_store_pushes_blob_and_compacted_snapshot_when_rclone_enabled() {
        if !should_run_rclone_test() {
            eprintln!("skipping rclone-gated shared-cache push test; rclone not on PATH or LUCHTA_TEST_RCLONE disabled");
            return;
        }

        let seed = seed_remote_store(
            "console.log('push');\n",
            [0x44; 32],
            300,
            (b"stdout-push", b"stderr-push"),
        );
        assert_remote_store_layout(
            seed.harness.remote_root.path(),
            seed.shard_key(),
            &seed.outputs_hash,
        );
    }

    #[test]
    fn remote_store_streams_snapshot_bytes_to_expected_remote_files() {
        if !should_run_rclone_test() {
            eprintln!("skipping rclone-gated shared-cache uploadfile path test; rclone not on PATH or LUCHTA_TEST_RCLONE disabled");
            return;
        }

        let seed = seed_remote_store(
            "console.log('uploadfile');\n",
            [0x45; 32],
            310,
            (b"stdout-uploadfile", b"stderr-uploadfile"),
        );

        let snapshot_dir = seed
            .harness
            .remote_root
            .path()
            .join("snapshots")
            .join(seed.shard_key());
        let mut bincode_files = Vec::new();
        let mut merged_files = Vec::new();
        for entry in fs::read_dir(&snapshot_dir).unwrap() {
            let entry = entry.unwrap();
            let file_name = entry.file_name().into_string().unwrap();
            if file_name.ends_with(".bincode") {
                bincode_files.push(file_name);
            } else if file_name.ends_with(".merged") {
                merged_files.push(file_name);
            }
        }
        assert_eq!(bincode_files.len(), 1);
        assert_eq!(merged_files.len(), 1);

        let local_snapshot_dir = seed.cache.paths().snapshots_dir.join(seed.shard_key());
        let local_shard_name = bincode_files.pop().unwrap();
        let local_merged_name = merged_files.pop().unwrap();
        assert_eq!(
            fs::read(snapshot_dir.join(&local_shard_name)).unwrap(),
            fs::read(local_snapshot_dir.join(&local_shard_name)).unwrap()
        );
        assert_eq!(
            fs::read(snapshot_dir.join(&local_merged_name)).unwrap(),
            fs::read(local_snapshot_dir.join(&local_merged_name)).unwrap()
        );
    }

    #[test]
    fn remote_store_skips_blob_reupload_when_remote_blob_exists() {
        if !should_run_rclone_test() {
            eprintln!("skipping rclone-gated shared-cache blob dedup test; rclone not on PATH or LUCHTA_TEST_RCLONE disabled");
            return;
        }

        let harness = RemoteHarness::new("console.log('dedup');\n");
        let outputs_hash = [0x66; 32];
        let blob_path = remote_blob_path(harness.remote_root.path(), &outputs_hash);
        fs::create_dir_all(blob_path.parent().unwrap()).unwrap();
        fs::write(&blob_path, b"preseeded-blob").unwrap();
        let before_mtime = fs::metadata(&blob_path).unwrap().modified().unwrap();

        let cache = harness.cache();
        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
        let outcome = cache
            .store(
                "pkg#build",
                &input_key,
                &outputs_hash,
                &harness.package_dir,
                &[PathBuf::from("dist/main.js")],
                &sample_record(true, 340),
                b"stdout-dedup",
                b"stderr-dedup",
                &[],
                harness.temp_repo.path(),
            )
            .unwrap();
        cache.flush_push_queue();
        assert!(matches!(outcome, StoreOutcome::Stored));

        let after_mtime = fs::metadata(&blob_path).unwrap().modified().unwrap();
        assert_eq!(before_mtime, after_mtime);
        assert_eq!(fs::read(&blob_path).unwrap(), b"preseeded-blob");
    }

    #[test]
    fn remote_store_uploads_entry_meta_and_second_machine_restores_no_output_task() {
        if !should_run_rclone_test() {
            eprintln!("skipping rclone-gated shared-cache entry-meta sync test; rclone not on PATH or LUCHTA_TEST_RCLONE disabled");
            return;
        }

        let remote_root = TempDir::new().unwrap();
        let machine_a_cache = TempDir::new().unwrap();
        let machine_b_cache = TempDir::new().unwrap();

        let repo = TempDir::new().unwrap();
        setup_git_repo(repo.path());
        create_commit(repo.path());

        let empty_hash = crate::resolve::combined_outputs_hash(&[]);
        let package_dir = repo.path().join("pkg");
        fs::create_dir_all(&package_dir).unwrap();

        let mut record = sample_record(true, 200);
        record.output_patterns = vec![];
        record.outputs = vec![];
        record.outputs_hash = empty_hash;
        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);

        let remote = RemoteSync::new(
            Arc::new(RcloneRcd::with_default_timeout().unwrap()),
            format!(":local:{}", remote_root.path().display()),
            8,
        );

        let cache_a = open_cache_with_remote(repo.path(), machine_a_cache.path(), &remote);
        cache_a
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
                repo.path(),
            )
            .unwrap();
        cache_a.flush_push_queue();

        assert!(
            remote_root
                .path()
                .join("entries")
                .read_dir()
                .unwrap()
                .count()
                > 0,
            "entry meta should be uploaded"
        );

        // Same remote, fresh local cache: stands in for a second machine.
        let cache_b = open_cache_with_remote(repo.path(), machine_b_cache.path(), &remote);
        let restore_dir = repo.path().join("restore");
        fs::create_dir_all(&restore_dir).unwrap();

        let candidate = cache_b
            .try_restore_candidates("pkg#lint", &input_key, &restore_dir)
            .next()
            .expect("machine B should find the entry");
        assert_eq!(candidate.stdout, b"lint output");
    }

    #[test]
    fn remote_cross_machine_store_on_a_restore_on_fresh_b_when_rclone_enabled() {
        if !should_run_rclone_test() {
            eprintln!("skipping rclone-gated cross-machine shared-cache test; rclone not on PATH or LUCHTA_TEST_RCLONE disabled");
            return;
        }

        let seed = seed_remote_store(
            "console.log('machine-a');\n",
            [0x77; 32],
            275,
            (b"stdout-a", b"stderr-a"),
        );
        assert_remote_store_layout(
            seed.harness.remote_root.path(),
            seed.shard_key(),
            &seed.outputs_hash,
        );

        fs::remove_dir_all(seed.harness.local_cache.path()).unwrap();
        let machine_b_cache = TempDir::new().unwrap();
        let cache_b = open_cache_with_remote(
            seed.harness.temp_repo.path(),
            machine_b_cache.path(),
            &seed.harness.remote,
        );
        let restore_dir = seed.harness.temp_repo.path().join("restore-machine-b");
        fs::create_dir_all(&restore_dir).unwrap();

        let (hit, written_paths) = cache_b
            .try_restore_candidates("pkg#build", &seed.input_key, &restore_dir)
            .next()
            .expect("fresh machine should pull from remote")
            .commit()
            .expect("remote restore should succeed");
        assert_eq!(written_paths, vec![restore_dir.join("dist/main.js")]);
        assert_remote_restore_result(
            &restore_dir,
            &hit,
            (b"stdout-a", b"stderr-a"),
            "console.log('machine-a');\n",
        );
        assert!(machine_b_cache
            .path()
            .join("snapshots")
            .join(seed.shard_key())
            .exists());
        assert!(machine_b_cache.path().join("blobs").exists());
    }

    #[test]
    fn remote_pull_deleted_shard_between_list_and_copy_is_graceful_miss_when_rclone_enabled() {
        if !should_run_rclone_test() {
            eprintln!("skipping rclone-gated shared-cache delete-race test; rclone not on PATH or LUCHTA_TEST_RCLONE disabled");
            return;
        }

        let seed = seed_remote_store(
            "console.log('race');\n",
            [0x78; 32],
            280,
            (b"stdout-race", b"stderr-race"),
        );
        let shard_name = remote_snapshot_files(seed.harness.remote_root.path(), seed.shard_key())
            .into_iter()
            .find(|name| name.ends_with(".bincode"))
            .expect("expected remote shard");
        fs::remove_file(
            seed.harness
                .remote_root
                .path()
                .join("snapshots")
                .join(seed.shard_key())
                .join(&shard_name),
        )
        .unwrap();
        fs::remove_dir_all(seed.harness.local_cache.path().join("snapshots")).ok();
        fs::remove_dir_all(seed.harness.local_cache.path().join("blobs")).ok();

        let restore_dir = seed.harness.temp_repo.path().join("restore-race");
        fs::create_dir_all(&restore_dir).unwrap();
        let candidates: Vec<_> = seed
            .cache
            .try_restore_candidates("pkg#build", &seed.input_key, &restore_dir)
            .collect();
        assert!(candidates.is_empty());
        assert!(!seed.cache.remote.as_ref().unwrap().is_disabled_for_test());
        assert!(!restore_dir.join("dist/main.js").exists());
        assert_remote_has_blob(seed.harness.remote_root.path(), &seed.outputs_hash);
        assert!(!seed
            .harness
            .local_cache
            .path()
            .join("snapshots")
            .join(seed.shard_key())
            .join(&shard_name)
            .exists());
    }

    #[test]
    fn remote_store_stops_deleting_subsumed_shards_once_remote_disables_mid_push() {
        if !should_run_rclone_test() {
            eprintln!("skipping rclone-gated shared-cache mid-push disable delete guard test; rclone not on PATH or LUCHTA_TEST_RCLONE disabled");
            return;
        }

        let harness = RemoteHarness::new("console.log('compact-mid-push');\n");
        // No `cache.store()` runs in this test, so there's no write key to
        // match against — just an arbitrary shard key to seed and assert
        // against consistently.
        let shard_key = "0000000000001-mid-push".to_string();
        let (seed_cache, _merge1_id, surviving_shard_id) = seed_remote_snapshot_entries(
            harness.temp_repo.path(),
            &shard_key,
            harness.remote_root.path(),
        );
        let remote_before = remote_snapshot_files(harness.remote_root.path(), &shard_key);
        assert_eq!(remote_before.len(), 2);
        seed_guard_blob(&harness.local_cache, [0x66; 32], b"blob-66");

        let upload_shard_id = "subsuming-shard-mid-push".to_string();
        let upload_shard_bytes = b"synthetic-shard".to_vec();
        let upload_merged_bytes = b"synthetic-merged".to_vec();
        let merge3 = MergeEntryOutcome {
            result: MergeResult::Inserted,
            new_snapshot_upload: Some(SnapshotUpload {
                shard_id: upload_shard_id.clone(),
                shard_bytes: upload_shard_bytes.clone(),
                merged_bytes: upload_merged_bytes.clone(),
            }),
            subsumed_shard_ids: vec![
                "disabling-shard-mid-push".to_string(),
                surviving_shard_id.clone(),
            ],
        };
        let remote_shard_dir = harness
            .remote_root
            .path()
            .join("snapshots")
            .join(&shard_key);
        fs::write(
            remote_shard_dir.join(format!("{}.{}", upload_shard_id, SNAPSHOT_FILE_EXTENSION)),
            &upload_shard_bytes,
        )
        .unwrap();
        fs::write(
            remote_shard_dir.join(format!("{}.{}", upload_shard_id, SNAPSHOT_MERGED_EXTENSION)),
            &upload_merged_bytes,
        )
        .unwrap();

        let disabling_shard_id = "disabling-shard-mid-push";
        let poisoned_file =
            remote_shard_dir.join(format!("{disabling_shard_id}.{SNAPSHOT_FILE_EXTENSION}"));
        fs::create_dir_all(&poisoned_file).unwrap();
        fs::write(
            remote_shard_dir.join(format!("{disabling_shard_id}.{SNAPSHOT_MERGED_EXTENSION}")),
            b"disabling-merged",
        )
        .unwrap();

        let input_key = derive_input_key([71; 32], [72; 32], [73; 32], [74; 32], [5; 32]);
        harness.remote.push_store_artifacts(PushArtifacts {
            paths: seed_cache.paths(),
            commit_key: &shard_key,
            outputs_hash: &[0x66; 32],
            input_key: &input_key,
            has_outputs: true,
            merge: merge3,
        });
        assert!(harness.remote.is_disabled_for_test());
        fs::remove_dir(&poisoned_file).unwrap();
        drop(seed_cache);

        let snapshot_files = remote_snapshot_files(harness.remote_root.path(), &shard_key);
        assert_eq!(snapshot_files.len(), 5);
        assert!(snapshot_files
            .iter()
            .any(|name| name == &format!("{disabling_shard_id}.{SNAPSHOT_MERGED_EXTENSION}")));
        assert!(snapshot_files
            .iter()
            .any(|name| name == &format!("{surviving_shard_id}.{SNAPSHOT_FILE_EXTENSION}")));
        assert!(snapshot_files
            .iter()
            .any(|name| name == &format!("{surviving_shard_id}.{SNAPSHOT_MERGED_EXTENSION}")));
        assert!(!snapshot_files
            .iter()
            .any(|name| name == &format!("{disabling_shard_id}.{SNAPSHOT_FILE_EXTENSION}")));
        assert!(snapshot_files
            .iter()
            .any(|name| name == &format!("{}.{}", upload_shard_id, SNAPSHOT_FILE_EXTENSION)));
        assert!(snapshot_files
            .iter()
            .any(|name| name == &format!("{}.{}", upload_shard_id, SNAPSHOT_MERGED_EXTENSION)));
    }

    #[test]
    fn remote_store_deletes_subsumed_remote_shards_when_rclone_enabled() {
        if !should_run_rclone_test() {
            eprintln!("skipping rclone-gated shared-cache shard delete test; rclone not on PATH or LUCHTA_TEST_RCLONE disabled");
            return;
        }

        let harness = RemoteHarness::new("console.log('compact');\n");
        // Build the cache first so the pre-seeded shards below land in the
        // exact shard `cache.store()` will write (and compact) into: the
        // cache picks its own write key at construction, so there is no
        // predictable key to seed ahead of time otherwise.
        let cache = harness.cache();
        let shard_key = cache.write_bucket_key().expect("write key").to_string();
        let (seed_cache, merge1_id, merge2_id) = seed_remote_snapshot_entries(
            harness.temp_repo.path(),
            &shard_key,
            harness.remote_root.path(),
        );
        let seeded_files = remote_snapshot_files(harness.remote_root.path(), &shard_key);
        assert_eq!(seeded_files.len(), 2);
        assert!(harness
            .remote_root
            .path()
            .join("snapshots")
            .join(&shard_key)
            .join(format!("{merge2_id}.bincode"))
            .exists());

        fs::remove_dir_all(harness.local_cache.path().join("snapshots")).ok();
        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
        let outcome = cache
            .store(
                "pkg#build",
                &input_key,
                &[0x55; 32],
                &harness.package_dir,
                &[PathBuf::from("dist/main.js")],
                &sample_record(true, 320),
                b"stdout-compact",
                b"stderr-compact",
                &[],
                harness.temp_repo.path(),
            )
            .unwrap();
        cache.flush_push_queue();
        assert!(matches!(outcome, StoreOutcome::Stored));
        drop(seed_cache);

        let snapshot_files = remote_snapshot_files(harness.remote_root.path(), &shard_key);
        assert_eq!(snapshot_files.len(), 4);
        assert!(!snapshot_files
            .iter()
            .any(|name| name.starts_with(&merge1_id)));
        // merge2's shard must survive the unrelated `cache.store()` above
        // (its entries a+b are still valid, so it's not subsumed by
        // anything) alongside the brand-new shard the store just created.
        // Check set membership, not position: `remote_snapshot_files` sorts
        // lexicographically by shard_id, an opaque hash, so which shard
        // happens to sort first is not a meaningful thing to assert on.
        assert!(
            snapshot_files
                .iter()
                .filter_map(|name| name.strip_suffix(".bincode"))
                .any(|id| id == merge2_id),
            "merge2's shard must still be present, not subsumed: {snapshot_files:?}"
        );
        assert_snapshot_shard_count(&snapshot_files, 2, 2);
    }

    #[test]
    fn remote_restore_from_async_runtime_does_not_nested_panic_when_rclone_enabled() {
        if !should_run_rclone_test() {
            eprintln!("skipping rclone-gated async shared-cache pull test; rclone not on PATH or LUCHTA_TEST_RCLONE disabled");
            return;
        }

        let seed = seed_remote_store(
            "console.log('async-restore');\n",
            [0x81; 32],
            420,
            (b"stdout-async", b"stderr-async"),
        );
        let local_cache = TempDir::new().unwrap();
        let pull_cache = open_cache_with_remote(
            seed.harness.temp_repo.path(),
            local_cache.path(),
            &seed.harness.remote,
        );
        let restore_dir = seed.harness.temp_repo.path().join("restore-remote-async");
        fs::create_dir_all(&restore_dir).unwrap();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let restore_dir_for_async = restore_dir.clone();
        let (hit, written_paths) = runtime.block_on(async move {
            pull_cache
                .try_restore_candidates("pkg#build", &seed.input_key, &restore_dir_for_async)
                .next()
                .expect("async runtime should still restore from remote")
                .commit()
                .expect("async remote restore should succeed")
        });
        assert_eq!(written_paths, vec![restore_dir.join("dist/main.js")]);

        assert_remote_restore_result(
            &restore_dir,
            &hit,
            (b"stdout-async", b"stderr-async"),
            "console.log('async-restore');\n",
        );
        assert!(local_cache
            .path()
            .join("snapshots")
            .join(seed.shard_key())
            .exists());
        assert!(local_cache
            .path()
            .join("blobs")
            .read_dir()
            .unwrap()
            .next()
            .is_some());
    }

    #[test]
    fn candidate_keys_include_remote_only_shards() {
        if !should_run_rclone_test() {
            eprintln!("skipping rclone-gated remote-only shard discovery test; rclone not on PATH or LUCHTA_TEST_RCLONE disabled");
            return;
        }

        // The #277 guarantee, restated for computed buckets: a bucket that
        // exists only on the remote must still be reachable. There's no
        // listing step left to prove that through -- `candidate_keys()` is
        // pure arithmetic over `now`/`day_window` and never touches the
        // remote -- so the proof has to be that `pull_candidate_commits`
        // actually copies a remote-only bucket down and a fresh local cache
        // restores from it. The seeded key is yesterday's date on a shard
        // this test never writes to directly, so nothing here accidentally
        // makes it reachable except the computed read window.
        let yesterday_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            - 24 * 60 * 60 * 1000;
        let remote_only_key = crate::shared::bucket_key(yesterday_ms, 5);

        let repo = tempfile::tempdir().unwrap();
        setup_git_repo(repo.path());
        create_commit(repo.path());
        let remote_root = tempfile::tempdir().unwrap();

        // Seed the entry on its own local cache dir, standing in for a
        // different machine: the fresh cache asserted against below never
        // shares a cache dir with this one, only the remote.
        let seed_cache_dir = tempfile::tempdir().unwrap();
        let seed_paths = crate::shared::open_shared_paths(seed_cache_dir.path()).unwrap();
        let input_key = derive_input_key([61; 32], [62; 32], [63; 32], [64; 32], [5; 32]);
        let record_bytes = bincode::serde::encode_to_vec(
            sample_record(true, 150),
            crate::serialization::bincode_config(),
        )
        .unwrap();
        crate::shared::write_entry_meta(
            &seed_paths,
            &input_key,
            &crate::shared::EntryMeta {
                schema_version: crate::shared::ENTRY_META_SCHEMA_VERSION,
                outputs_hash: [0; 32],
                has_outputs: false,
                record: record_bytes,
                stdout: b"remote-only-stdout".to_vec(),
                stderr: Vec::new(),
                reports: Vec::new(),
            },
        )
        .unwrap();
        let merge = SnapshotStore::new(seed_paths.clone()).merge_entry_with_outcome(
            &remote_only_key,
            SnapshotEntry {
                task_id: "pkg#remote-only".to_string(),
                input_key,
                outputs_hash: [0; 32],
                task_spec_hash: [61; 32],
                env_hash: [62; 32],
                pkg_dep_hash: [63; 32],
                duration_ms: 150,
                output_bytes: 0,
                cached_at_unix_ms: 1,
                tool_version: None,
            },
        );

        let remote_seed = RemoteSync::new(
            Arc::new(RcloneRcd::with_default_timeout().unwrap()),
            format!(":local:{}", remote_root.path().display()),
            8,
        );
        remote_seed.push_store_artifacts(PushArtifacts {
            paths: &seed_paths,
            commit_key: &remote_only_key,
            outputs_hash: &[0; 32],
            input_key: &input_key,
            has_outputs: false,
            merge,
        });

        // Fresh local cache: it never wrote to `remote_only_key`, and that
        // key isn't its own write bucket either. The only way it can see
        // this entry is by pulling every computed candidate key from the
        // remote.
        let fresh_cache_dir = tempfile::tempdir().unwrap();
        let cache = open_cache_with_remote(repo.path(), fresh_cache_dir.path(), &remote_seed);
        let restore_dir = repo.path().join("restore-remote-only");
        fs::create_dir_all(&restore_dir).unwrap();

        let candidate = cache
            .try_restore_candidates("pkg#remote-only", &input_key, &restore_dir)
            .next()
            .expect("a shard that exists only on the remote must still be reachable");
        assert_eq!(candidate.stdout, b"remote-only-stdout");
    }

    #[test]
    fn flush_refreshes_on_remote_backed_cache_pushes_the_merge_and_entry_meta() {
        if !should_run_rclone_test() {
            eprintln!("skipping rclone-gated shared-cache refresh push test; rclone not on PATH or LUCHTA_TEST_RCLONE disabled");
            return;
        }

        // A refresh must reach the remote, not just this machine's local
        // index: `enqueue_push_store_artifacts` is the only path in this
        // crate that syncs a snapshot shard outward. A refresh that only
        // updated the local `SnapshotStore` would leave the day-window leak
        // open for every OTHER machine pulling from the same remote --
        // exactly the multi-machine deployment this task exists to fix.
        let harness = RemoteHarness::new("console.log('refresh-push');\n");
        let cache = harness.cache();

        let input_key = derive_input_key([21; 32], [22; 32], [23; 32], [24; 32], [25; 32]);
        let outputs_hash = [0x66; 32];

        // A real hit always has a locally-readable meta object before
        // `refresh_entry` is ever called -- `try_restore_candidates` requires
        // `read_entry_meta` to succeed to produce a candidate at all -- so
        // seed one here for `flush_refreshes` to read `has_outputs` from.
        let record_bytes = bincode::serde::encode_to_vec(
            sample_record(true, 200),
            crate::serialization::bincode_config(),
        )
        .unwrap();
        crate::shared::write_entry_meta(
            cache.paths(),
            &input_key,
            &crate::shared::EntryMeta {
                schema_version: crate::shared::ENTRY_META_SCHEMA_VERSION,
                outputs_hash,
                has_outputs: false,
                record: record_bytes,
                stdout: Vec::new(),
                stderr: Vec::new(),
                reports: Vec::new(),
            },
        )
        .unwrap();

        let entry = SnapshotEntry {
            task_id: "pkg#refresh".to_string(),
            input_key,
            outputs_hash,
            task_spec_hash: [21; 32],
            env_hash: [22; 32],
            pkg_dep_hash: [23; 32],
            duration_ms: 200,
            output_bytes: 0,
            cached_at_unix_ms: 1_000_000_000_000,
            tool_version: None,
        };

        // `refresh_entry` only records the entry and touches its local
        // mtime -- nothing reaches the remote (or even the local index)
        // until `flush_refreshes` runs.
        cache.refresh_entry(&input_key, &entry);
        cache.flush_push_queue();
        let write_key = cache.write_bucket_key().unwrap().to_string();
        assert!(
            remote_snapshot_files(harness.remote_root.path(), &write_key).is_empty(),
            "refresh_entry alone must not reach the remote"
        );

        // Nothing has been merged into today's write bucket yet, so this
        // flush is the day's first hit that adds the key: the merge outcome
        // is `Inserted`, which is exactly the case that must trigger a real
        // remote push (see `flush_refreshes`'s doc comment for why
        // `IdempotentNoop`/`ConflictKeptExisting` push at most the entry
        // meta/blob, not a fresh snapshot shard).
        cache.flush_refreshes();
        cache.flush_push_queue();

        assert!(
            !remote_snapshot_files(harness.remote_root.path(), &write_key).is_empty(),
            "flush_refreshes must push the merged snapshot shard to the remote, \
             not just merge it into the local index"
        );
        assert!(
            harness
                .remote_root
                .path()
                .join("entries")
                .join(format!("{}.bin", hex_hash(input_key)))
                .exists(),
            "flush_refreshes must push the entry meta object to the remote too"
        );
    }

    #[test]
    fn flush_refreshes_collapses_two_hits_into_a_single_push() {
        if !should_run_rclone_test() {
            eprintln!("skipping rclone-gated shared-cache refresh-batching test; rclone not on PATH or LUCHTA_TEST_RCLONE disabled");
            return;
        }

        // Round 2 of this feature pushed a merge to the remote on every
        // single hit: a build with N hits enqueued up to N pushes, worst on
        // exactly the runs where the cache works best, saturating the rclone
        // daemon and tripping its timeout-disable circuit breaker. Batching
        // must collapse any number of hits in one run into exactly one
        // push -- proven here with two hits for two distinct keys.
        let harness = RemoteHarness::new("console.log('flush-collapse');\n");
        let cache = harness.cache();

        let hits = [
            (
                derive_input_key([31; 32], [32; 32], [33; 32], [34; 32], [35; 32]),
                [0x71; 32],
                31u8,
            ),
            (
                derive_input_key([41; 32], [42; 32], [43; 32], [44; 32], [45; 32]),
                [0x72; 32],
                41u8,
            ),
        ];

        for (input_key, outputs_hash, seed) in hits {
            let record_bytes = bincode::serde::encode_to_vec(
                sample_record(true, 200),
                crate::serialization::bincode_config(),
            )
            .unwrap();
            crate::shared::write_entry_meta(
                cache.paths(),
                &input_key,
                &crate::shared::EntryMeta {
                    schema_version: crate::shared::ENTRY_META_SCHEMA_VERSION,
                    outputs_hash,
                    has_outputs: false,
                    record: record_bytes,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    reports: Vec::new(),
                },
            )
            .unwrap();

            let entry = SnapshotEntry {
                task_id: format!("pkg-{seed}#refresh"),
                input_key,
                outputs_hash,
                task_spec_hash: [seed; 32],
                env_hash: [seed.wrapping_add(1); 32],
                pkg_dep_hash: [seed.wrapping_add(2); 32],
                duration_ms: 200,
                output_bytes: 0,
                cached_at_unix_ms: 1_000_000_000_000,
                tool_version: None,
            };
            cache.refresh_entry(&input_key, &entry);
        }

        // Neither hit has reached the remote yet: two calls to
        // `refresh_entry` with no flush in between must produce zero remote
        // traffic.
        cache.flush_push_queue();
        let write_key = cache.write_bucket_key().unwrap().to_string();
        assert!(
            remote_snapshot_files(harness.remote_root.path(), &write_key).is_empty(),
            "two hits with no flush yet must not have reached the remote"
        );

        cache.flush_refreshes();
        cache.flush_push_queue();

        let files = remote_snapshot_files(harness.remote_root.path(), &write_key);
        assert_snapshot_shard_count(&files, 1, 1);

        // And that single shard must carry BOTH entries -- proving the flush
        // merged them together in one pass rather than only the last one
        // surviving.
        let remote_paths = crate::shared::open_shared_paths(harness.remote_root.path()).unwrap();
        let remote_snapshot = SnapshotStore::new(remote_paths)
            .load(&write_key)
            .expect("the single pushed shard must be loadable");
        assert_eq!(
            remote_snapshot.entries.len(),
            2,
            "the single push must carry both refreshed entries"
        );
        for (input_key, _, _) in hits {
            assert!(
                remote_snapshot
                    .entries
                    .contains_key(&input_key_hex(input_key)),
                "both hits' entries must be present in the one pushed shard"
            );
        }
    }
}
