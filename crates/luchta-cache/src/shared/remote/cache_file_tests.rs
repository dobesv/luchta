use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tempfile::TempDir;

use super::{RemoteConfig, RemoteSync};
use crate::shared::tests::{create_commit, sample_record, setup_git_repo};
use crate::shared::{
    derive_cache_file_scope, hex_hash, CacheFileRestoreRequest, CacheFileStoreRequest, OpenExtras,
    RcloneRcd, SharedCache, StoreOutcome,
};

struct RemoteCacheFilesFixture {
    repo: TempDir,
    remote_root: TempDir,
    machine_a_cache: TempDir,
    machine_b_cache: TempDir,
    package_dir: PathBuf,
    remote: RemoteSync,
    patterns: Vec<String>,
    scope_hash: [u8; 32],
    upstream_hash: [u8; 32],
}

impl RemoteCacheFilesFixture {
    fn new() -> Self {
        let repo = TempDir::new().unwrap();
        setup_git_repo(repo.path());
        create_commit(repo.path());
        let remote_root = TempDir::new().unwrap();
        let package_dir = repo.path().join("pkg");
        fs::create_dir_all(package_dir.join("tool-cache")).unwrap();
        fs::write(package_dir.join("tool-cache/a.bin"), b"alpha").unwrap();
        fs::write(package_dir.join("tool-cache/b.bin"), b"beta").unwrap();
        let remote = RemoteSync::new(
            Arc::new(RcloneRcd::with_default_timeout().unwrap()),
            format!(":local:{}", remote_root.path().display()),
            8,
        );
        Self {
            repo,
            remote_root,
            machine_a_cache: TempDir::new().unwrap(),
            machine_b_cache: TempDir::new().unwrap(),
            package_dir,
            remote,
            patterns: vec!["tool-cache/**".to_owned()],
            scope_hash: derive_cache_file_scope("pkg#lint", [1; 32], [4; 32], [5; 32]),
            upstream_hash: [7; 32],
        }
    }

    fn publish(&self) -> [u8; 32] {
        let entries = crate::resolve_outputs(&self.package_dir, &self.patterns).unwrap();
        let relative_paths = entries
            .iter()
            .map(|entry| PathBuf::from(&entry.path))
            .collect::<Vec<_>>();
        let state_hash = crate::combined_cache_files_hash(&entries);
        let cache =
            open_cache_with_remote(self.repo.path(), self.machine_a_cache.path(), &self.remote);
        let outcome = cache
            .store_cache_files_with_execution_duration(
                CacheFileStoreRequest {
                    scope_hash: self.scope_hash,
                    upstream_outputs_hash: self.upstream_hash,
                    package_dir: &self.package_dir,
                    relative_paths: &relative_paths,
                    state_hash: Some(state_hash),
                    state_bytes: 9,
                    record: &sample_record(true, 200),
                    repo_root: self.repo.path(),
                },
                200,
            )
            .unwrap();
        assert_eq!(outcome, StoreOutcome::Stored);
        cache.flush_pending_entries();
        cache.flush_push_queue();
        assert!(
            remote_snapshot_dir(self.remote_root.path(), cache.write_bucket_key().unwrap())
                .read_dir()
                .unwrap()
                .next()
                .is_some()
        );
        state_hash
    }
}

#[test]
fn state_uploads_and_restores_on_a_fresh_machine() {
    if !should_run_rclone_test() {
        eprintln!(
            "skipping rclone-gated cache-file state test; rclone not on PATH or LUCHTA_TEST_RCLONE disabled"
        );
        return;
    }

    let fixture = RemoteCacheFilesFixture::new();
    let state_hash = fixture.publish();
    assert!(fixture
        .remote_root
        .path()
        .join("cache-files")
        .join(format!("{}.tar.zst", hex_hash(state_hash)))
        .is_file());

    fs::remove_dir_all(fixture.package_dir.join("tool-cache")).unwrap();
    let cache = open_cache_with_remote(
        fixture.repo.path(),
        fixture.machine_b_cache.path(),
        &fixture.remote,
    );
    let staged = cache
        .prepare_cache_files(CacheFileRestoreRequest {
            scope_hash: &fixture.scope_hash,
            upstream_outputs_hash: fixture.upstream_hash,
            package_dir: &fixture.package_dir,
            patterns: &fixture.patterns,
        })
        .expect("fresh machine should fetch and stage advisory state");
    assert_eq!(
        staged.relative_paths(),
        [
            PathBuf::from("tool-cache/a.bin"),
            PathBuf::from("tool-cache/b.bin")
        ]
    );
    staged.commit().unwrap();
    assert_eq!(
        fs::read(fixture.package_dir.join("tool-cache/a.bin")).unwrap(),
        b"alpha"
    );
    assert_eq!(
        fs::read(fixture.package_dir.join("tool-cache/b.bin")).unwrap(),
        b"beta"
    );
}

fn open_cache_with_remote(repo_root: &Path, cache_dir: &Path, remote: &RemoteSync) -> SharedCache {
    SharedCache::open_with_remote(
        repo_root,
        1_000_000,
        3,
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

fn remote_snapshot_dir(remote_root: &Path, shard_key: &str) -> PathBuf {
    remote_root.join("snapshots").join(shard_key)
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
