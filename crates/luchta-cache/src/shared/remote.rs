//! Remote (S3-via-rclone) sync layer for the shared cache.
//!
//! Owns `RemoteSync` — the opt-in remote pull/push transport built on the
//! rclone rcd sidecar — and its run-wide disable-and-warn state. Kept separate
//! from `mod.rs` so the local cache and the remote sync concerns stay cohesive.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::snapshot::{SnapshotUpload, SNAPSHOT_FILE_EXTENSION, SNAPSHOT_MERGED_EXTENSION};
use super::{
    blob_path, hex_hash, rclone, MergeEntryOutcome, RcloneRcd, SharedCachePaths, SnapshotStore,
    BLOBS_DIR_NAME, SNAPSHOTS_DIR_NAME,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConfig {
    pub fs_base: String,
    pub sync_timeout: Duration,
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
}

impl RemoteState {
    fn new() -> Self {
        Self {
            disabled: AtomicBool::new(false),
            warned: AtomicBool::new(false),
        }
    }

    fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::Acquire)
    }

    fn disable_with_warning(&self, reason: &str) {
        self.disabled.store(true, Ordering::Release);
        if !self.warned.swap(true, Ordering::AcqRel) {
            eprintln!("warning: remote cache disabled: {reason}");
        }
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
}

/// Inputs for [`RemoteSync::push_store_artifacts`].
pub(crate) struct PushArtifacts<'a> {
    pub(crate) paths: &'a SharedCachePaths,
    pub(crate) commit_key: &'a str,
    pub(crate) outputs_hash: &'a [u8; 32],
    pub(crate) merge: MergeEntryOutcome,
}

impl RemoteSync {
    #[must_use]
    pub(crate) fn new(rclone: Arc<RcloneRcd>, remote_base_fs: impl Into<String>) -> Self {
        Self {
            rclone,
            remote_base_fs: remote_base_fs.into(),
            state: Arc::new(RemoteState::new()),
        }
    }

    pub(crate) fn from_config(config: RemoteConfig) -> Result<Self, rclone::RcloneError> {
        let rclone = Arc::new(RcloneRcd::new(config.sync_timeout)?);
        Ok(Self::new(rclone, config.fs_base))
    }

    fn is_disabled(&self) -> bool {
        self.state.is_disabled()
    }

    /// Flip the run-wide disable flag from a typed rclone error and warn once.
    ///
    /// A `404` (object/directory not found) is a normal cache MISS — the commit
    /// simply has no remote shards/blob yet — and must NOT disable the remote.
    /// Only genuine health failures (timeout, unavailable, process/request
    /// errors, other HTTP statuses) trip the run-wide disable flag.
    fn record_remote_error(&self, err: &rclone::RcloneError) {
        if matches!(err, rclone::RcloneError::HttpStatus { status: 404, .. })
            || is_missing_local_source_copy_error(err)
        {
            return;
        }
        self.state.disable_with_warning(&remote_disable_reason(err));
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
}

/// Splits a remote snapshot-dir listing into shard file names and the set of
/// `.merged` sidecar names present, ignoring directories and other entries.
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
    pub(crate) fn pull_snapshot_commit(&self, snapshot_store: &SnapshotStore, commit_key: &str) {
        if self.is_disabled() {
            return;
        }
        let remote_fs = self.snapshots_fs(commit_key);
        let local_dir = snapshot_store.paths().snapshots_dir.join(commit_key);
        if let Err(err) = fs::create_dir_all(&local_dir) {
            eprintln!("debug: local snapshot dir prep failed for commit={commit_key}: {err}");
            return;
        }
        let local_fs = format!(":local:{}", local_dir.display());
        if let Err(err) = self
            .rclone
            .copy_dir(&remote_fs, &local_fs, self.rclone.default_timeout())
        {
            self.record_remote_error(&err);
            eprintln!("debug: remote snapshot copy failed for commit={commit_key}: {err}");
        }
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
            merge,
        } = push;

        self.push_blob_if_missing(paths, outputs_hash);

        let uploaded_new_shard = match merge.new_snapshot_upload.as_ref() {
            Some(upload) => self.push_snapshot_upload(commit_key, upload),
            None => false,
        };

        if !uploaded_new_shard {
            return;
        }

        for shard_id in &merge.subsumed_shard_ids {
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
            Ok(Some(_)) => return,
            Ok(None) => {}
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
        }
    }

    fn push_snapshot_upload(&self, commit_key: &str, upload: &SnapshotUpload) -> bool {
        let remote_fs = self.snapshots_fs(commit_key);
        let shard_name = format!("{}.{SNAPSHOT_FILE_EXTENSION}", upload.shard_id);
        if let Err(err) = self.copy_bytes_up(&upload.shard_bytes, &remote_fs, &shard_name) {
            self.record_remote_error(&err);
            eprintln!(
                "warn: shared cache remote snapshot upload failed for commit={commit_key} file={shard_name}: {err}"
            );
            return false;
        }

        let merged_name = format!("{}.{SNAPSHOT_MERGED_EXTENSION}", upload.shard_id);
        if let Err(err) = self.copy_bytes_up(&upload.merged_bytes, &remote_fs, &merged_name) {
            self.record_remote_error(&err);
            eprintln!(
                "warn: shared cache remote snapshot upload failed for commit={commit_key} file={merged_name}: {err}"
            );
            return false;
        }

        true
    }

    fn delete_remote_snapshot_file(&self, commit_key: &str, shard_id: &str, extension: &str) {
        let remote_fs = self.snapshots_fs(commit_key);
        let remote_name = format!("{shard_id}.{extension}");
        if let Err(err) =
            self.rclone
                .deletefile(&remote_fs, &remote_name, self.rclone.default_timeout())
        {
            if matches!(err, rclone::RcloneError::HttpStatus { status: 404, .. }) {
                return;
            }
            self.record_remote_error(&err);
            eprintln!(
                "warn: shared cache remote snapshot delete failed for commit={commit_key} file={remote_name}: {err}"
            );
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
    use crate::shared::tests::{bincode_config, create_commit, sample_record, setup_git_repo};
    use crate::shared::{
        derive_input_key, input_key_hex, OpenExtras, SharedCache, Snapshot, SnapshotEntry,
        StoreOutcome, SNAPSHOT_SCHEMA_VERSION,
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

    fn remote_snapshot_files(remote_root: &Path, commit: &str) -> Vec<String> {
        let commit_dir = remote_root.join("snapshots").join(commit);
        let Ok(read_dir) = fs::read_dir(commit_dir) else {
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
        commit: String,
        remote: RemoteSync,
    }

    impl RemoteHarness {
        fn new(file_body: &str) -> Self {
            let temp_repo = TempDir::new().unwrap();
            setup_git_repo(temp_repo.path());
            let commit = create_commit(temp_repo.path());
            let remote_root = TempDir::new().unwrap();
            let local_cache = TempDir::new().unwrap();
            let package_dir = temp_repo.path().join("pkg");
            write_dist_file(&package_dir, file_body);
            let remote = RemoteSync::new(
                Arc::new(RcloneRcd::new(Duration::from_secs(10)).unwrap()),
                format!(":local:{}", remote_root.path().display()),
            );
            Self {
                temp_repo,
                remote_root,
                local_cache,
                package_dir,
                commit,
                remote,
            }
        }

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

    fn seed_remote_store(
        file_body: &str,
        outputs_hash: [u8; 32],
        duration_ms: u64,
        streams: (&'static [u8], &'static [u8]),
    ) -> StoredRemoteCase {
        let (stdout, stderr) = streams;
        let harness = RemoteHarness::new(file_body);
        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);
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

    fn assert_remote_store_layout(remote_root: &Path, commit: &str, outputs_hash: &[u8; 32]) {
        let files = remote_snapshot_files(remote_root, commit);
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
        commit: &str,
        entry: SnapshotEntry,
    ) -> String {
        let outputs_hash = entry.outputs_hash;
        let merge = seed_cache
            .snapshot_store
            .merge_entry_with_outcome(commit, entry);
        let shard_id = merge.new_snapshot_upload.as_ref().unwrap().shard_id.clone();
        remote_seed.push_store_artifacts(PushArtifacts {
            paths: seed_cache.paths(),
            commit_key: commit,
            outputs_hash: &outputs_hash,
            merge,
        });
        shard_id
    }

    fn seed_remote_snapshot_entries(
        repo_root: &Path,
        commit: &str,
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
        );
        let merge1_id = seed_snapshot_entry(
            &seed_cache,
            &remote_seed,
            commit,
            SnapshotEntry {
                task_id: "pkg#a".to_string(),
                input_key: derive_input_key([11; 32], [12; 32], [13; 32], [14; 32]),
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
            commit,
            SnapshotEntry {
                task_id: "pkg#b".to_string(),
                input_key: derive_input_key([15; 32], [16; 32], [17; 32], [18; 32]),
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
        );
        let err = rclone::RcloneError::HttpStatus {
            status: 500,
            body: "failed to open source object: lstat /tmp/cache/snapshots/abc/123.merged: no such file or directory".to_string(),
        };

        remote.record_remote_error(&err);

        assert!(!remote.is_disabled_for_test());
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
        commit: &str,
        before: &[String],
        expected_present_ids: &[&str],
    ) {
        let remote_after = remote_snapshot_files(remote_root, commit);
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
        let (seed_cache, _merge1_id, merge2_id) = seed_remote_snapshot_entries(
            harness.temp_repo.path(),
            &harness.commit,
            harness.remote_root.path(),
        );
        let remote_before = remote_snapshot_files(harness.remote_root.path(), &harness.commit);
        assert_eq!(remote_before.len(), 2);
        seed_guard_blob(&harness.local_cache, [23; 32], b"blob-23");
        let cache = harness.cache();
        let mut merge3 = cache.snapshot_store.merge_entry_with_outcome(
            &harness.commit,
            SnapshotEntry {
                task_id: "pkg#c".to_string(),
                input_key: derive_input_key([19; 32], [20; 32], [21; 32], [22; 32]),
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
            .join(&harness.commit)
            .join(format!("subsuming-shard.{SNAPSHOT_FILE_EXTENSION}"));
        fs::create_dir_all(&blocking_path).unwrap();
        harness.remote.push_store_artifacts(PushArtifacts {
            paths: cache.paths(),
            commit_key: &harness.commit,
            outputs_hash: &[23; 32],
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
            &harness.commit,
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
            bincode::serde::encode_to_vec(&snapshot, bincode_config()).unwrap(),
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
                }),
            },
        )
        .unwrap();
        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);
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
        let hit = candidates.remove(0).commit().unwrap();
        assert_remote_restore_result(
            &restore_dir,
            &hit,
            (b"stdout-remote", b"stderr-remote"),
            "console.log('forge');\n",
        );
        assert!(local_cache
            .path()
            .join("snapshots")
            .join(&seed.harness.commit)
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
            &seed.harness.commit,
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
            .join(&seed.harness.commit);
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

        let local_snapshot_dir = seed.cache.paths().snapshots_dir.join(&seed.harness.commit);
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
        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);
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
        assert!(matches!(outcome, StoreOutcome::Stored));

        let after_mtime = fs::metadata(&blob_path).unwrap().modified().unwrap();
        assert_eq!(before_mtime, after_mtime);
        assert_eq!(fs::read(&blob_path).unwrap(), b"preseeded-blob");
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
            &seed.harness.commit,
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

        let hit = cache_b
            .try_restore_candidates("pkg#build", &seed.input_key, &restore_dir)
            .next()
            .expect("fresh machine should pull from remote")
            .commit()
            .expect("remote restore should succeed");
        assert_remote_restore_result(
            &restore_dir,
            &hit,
            (b"stdout-a", b"stderr-a"),
            "console.log('machine-a');\n",
        );
        assert!(machine_b_cache
            .path()
            .join("snapshots")
            .join(&seed.harness.commit)
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
        let shard_name =
            remote_snapshot_files(seed.harness.remote_root.path(), &seed.harness.commit)
                .into_iter()
                .find(|name| name.ends_with(".bincode"))
                .expect("expected remote shard");
        fs::remove_file(
            seed.harness
                .remote_root
                .path()
                .join("snapshots")
                .join(&seed.harness.commit)
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
            .join(&seed.harness.commit)
            .join(&shard_name)
            .exists());
    }

    #[test]
    fn remote_store_deletes_subsumed_remote_shards_when_rclone_enabled() {
        if !should_run_rclone_test() {
            eprintln!("skipping rclone-gated shared-cache shard delete test; rclone not on PATH or LUCHTA_TEST_RCLONE disabled");
            return;
        }

        let harness = RemoteHarness::new("console.log('compact');\n");
        let (seed_cache, merge1_id, merge2_id) = seed_remote_snapshot_entries(
            harness.temp_repo.path(),
            &harness.commit,
            harness.remote_root.path(),
        );
        let seeded_files = remote_snapshot_files(harness.remote_root.path(), &harness.commit);
        assert_eq!(seeded_files.len(), 2);
        assert!(harness
            .remote_root
            .path()
            .join("snapshots")
            .join(&harness.commit)
            .join(format!("{merge2_id}.bincode"))
            .exists());

        fs::remove_dir_all(harness.local_cache.path().join("snapshots")).ok();
        let cache = harness.cache();
        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);
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
        assert!(matches!(outcome, StoreOutcome::Stored));
        drop(seed_cache);

        let snapshot_files = remote_snapshot_files(harness.remote_root.path(), &harness.commit);
        assert_eq!(snapshot_files.len(), 4);
        assert!(!snapshot_files
            .iter()
            .any(|name| name.starts_with(&merge1_id)));
        assert_eq!(
            snapshot_files
                .iter()
                .find_map(|name| name.strip_suffix(".bincode"))
                .unwrap(),
            merge2_id
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
        let hit = runtime.block_on(async move {
            pull_cache
                .try_restore_candidates("pkg#build", &seed.input_key, &restore_dir_for_async)
                .next()
                .expect("async runtime should still restore from remote")
                .commit()
                .expect("async remote restore should succeed")
        });

        assert_remote_restore_result(
            &restore_dir,
            &hit,
            (b"stdout-async", b"stderr-async"),
            "console.log('async-restore');\n",
        );
        assert!(local_cache
            .path()
            .join("snapshots")
            .join(&seed.harness.commit)
            .exists());
        assert!(local_cache
            .path()
            .join("blobs")
            .read_dir()
            .unwrap()
            .next()
            .is_some());
    }
}
