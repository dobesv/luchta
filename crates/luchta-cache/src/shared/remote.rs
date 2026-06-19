//! Remote (S3-via-rclone) sync layer for the shared cache.
//!
//! Owns `RemoteSync` — the opt-in remote pull/push transport built on the
//! rclone rcd sidecar — and its run-wide disable-and-warn state. Kept separate
//! from `mod.rs` so the local cache and the remote sync concerns stay cohesive.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::snapshot::{SNAPSHOT_FILE_EXTENSION, SNAPSHOT_MERGED_EXTENSION};
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
    pub(crate) merge: &'a MergeEntryOutcome,
}

/// Per-commit context shared across the shard pulls of one snapshot directory.
struct PullCommit<'a> {
    snapshots_dir: PathBuf,
    commit_key: &'a str,
    remote_fs: &'a str,
    merged_sidecars: &'a HashSet<String>,
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
        if matches!(err, rclone::RcloneError::HttpStatus { status: 404, .. }) {
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
fn classify_remote_listing(entries: Vec<rclone::Entry>) -> (Vec<String>, HashSet<String>) {
    let mut shard_names = Vec::new();
    let mut merged_sidecars = HashSet::new();
    for entry in entries {
        if entry.is_dir {
            continue;
        }
        let path = entry.path;
        if path.ends_with(&format!(".{SNAPSHOT_MERGED_EXTENSION}")) {
            merged_sidecars.insert(path);
        } else if path.ends_with(&format!(".{SNAPSHOT_FILE_EXTENSION}")) {
            shard_names.push(path);
        }
    }
    (shard_names, merged_sidecars)
}

/// Maps a typed rclone error to a short human-readable disable reason.
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
        // First remote interaction of the run; bounded by the configured
        // sync_timeout (RcloneRcd default). A failure here disables remote for
        // the rest of the run and the build continues local-only.
        let entries = match self
            .rclone
            .list(&remote_fs, "", self.rclone.default_timeout())
        {
            Ok(entries) => entries,
            Err(err) => {
                self.record_remote_error(&err);
                eprintln!("debug: remote snapshot list failed for commit={commit_key}: {err}");
                return;
            }
        };

        let (shard_names, merged_sidecars) = classify_remote_listing(entries);
        let ctx = PullCommit {
            snapshots_dir: snapshot_store.paths().snapshots_dir.join(commit_key),
            commit_key,
            remote_fs: &remote_fs,
            merged_sidecars: &merged_sidecars,
        };
        for shard_name in shard_names {
            // Pull each shard (and its `.merged` sidecar if present). Any failure
            // disables the remote and stops further pulls for the rest of the run.
            if self.pull_one_shard(&ctx, &shard_name).is_err() {
                return;
            }
        }
    }

    /// Pulls a single shard and its `.merged` sidecar (if listed) into the local
    /// cache. Returns `Err(())` if a genuine remote failure occurred (the caller
    /// stops pulling); a normal miss/already-present case returns `Ok(())`.
    fn pull_one_shard(&self, ctx: &PullCommit<'_>, shard_name: &str) -> Result<(), ()> {
        let commit_key = ctx.commit_key;
        let local_path = ctx.snapshots_dir.join(shard_name);
        if !local_path.exists() {
            if let Err(err) = self.copy_remote_file_down(ctx.remote_fs, shard_name, &local_path) {
                self.record_remote_error(&err);
                eprintln!(
                    "debug: remote snapshot pull failed for commit={commit_key} shard={shard_name}: {err}"
                );
                return Err(());
            }
        }

        let shard_id = shard_name.trim_end_matches(&format!(".{SNAPSHOT_FILE_EXTENSION}"));
        let merged_name = format!("{shard_id}.{SNAPSHOT_MERGED_EXTENSION}");
        if !ctx.merged_sidecars.contains(&merged_name) {
            return Ok(());
        }
        let merged_local_path = ctx.snapshots_dir.join(&merged_name);
        if merged_local_path.exists() {
            return Ok(());
        }
        if let Err(err) =
            self.copy_remote_file_down(ctx.remote_fs, &merged_name, &merged_local_path)
        {
            self.record_remote_error(&err);
            eprintln!(
                "debug: remote merged sidecar pull failed for commit={commit_key} shard={merged_name}: {err}"
            );
            return Err(());
        }
        Ok(())
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

        if let Some(new_shard_id) = &merge.new_shard_id {
            self.push_snapshot_file(
                paths,
                commit_key,
                &format!("{new_shard_id}.{SNAPSHOT_FILE_EXTENSION}"),
            );
            self.push_snapshot_file(
                paths,
                commit_key,
                &format!("{new_shard_id}.{SNAPSHOT_MERGED_EXTENSION}"),
            );
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

    fn push_snapshot_file(&self, paths: &SharedCachePaths, commit_key: &str, file_name: &str) {
        let local_path = paths.snapshots_dir.join(commit_key).join(file_name);
        if !local_path.exists() {
            return;
        }

        let remote_fs = self.snapshots_fs(commit_key);
        if let Err(err) = self.copy_local_file_up(&local_path, &remote_fs, file_name) {
            self.record_remote_error(&err);
            eprintln!(
                "warn: shared cache remote snapshot upload failed for commit={commit_key} file={file_name}: {err}"
            );
        }
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

        let merge1 = seed_cache.snapshot_store.merge_entry_with_outcome(
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
        remote_seed.push_store_artifacts(PushArtifacts {
            paths: seed_cache.paths(),
            commit_key: commit,
            outputs_hash: &[21; 32],
            merge: &merge1,
        });
        let merge1_id = merge1.new_shard_id.unwrap();

        let merge2 = seed_cache.snapshot_store.merge_entry_with_outcome(
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
        remote_seed.push_store_artifacts(PushArtifacts {
            paths: seed_cache.paths(),
            commit_key: commit,
            outputs_hash: &[22; 32],
            merge: &merge2,
        });
        (seed_cache, merge1_id, merge2.new_shard_id.unwrap())
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
}
