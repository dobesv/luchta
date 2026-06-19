use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::shared::snapshot::{SNAPSHOT_FILE_EXTENSION, SNAPSHOT_MERGED_EXTENSION};
use crate::shared::{atomic_write, SharedCachePaths};

#[cfg(test)]
use crate::shared::{derive_input_key, restore_blob, SnapshotEntry, SnapshotStore};

/// Default shared-cache retention window. P6.1 makes this env-configurable.
pub const DEFAULT_GC_RETENTION: Duration = Duration::from_secs(14 * 24 * 60 * 60);
/// Default throttle window for opportunistic GC runs.
pub const DEFAULT_GC_THROTTLE: Duration = Duration::from_secs(24 * 60 * 60);
const LAST_GC_MARKER: &str = ".last-gc";
const SNAPSHOT_SUFFIX: &str = ".bincode";
const BLOB_SUFFIX: &str = ".tar.zst";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcStats {
    pub snapshots_deleted: u64,
    pub blobs_deleted: u64,
    pub bytes_freed: u64,
}

#[must_use]
pub fn run_gc(paths: &SharedCachePaths, retention: Duration) -> GcStats {
    let now = SystemTime::now();
    let mut stats = GcStats::default();

    gc_snapshot_dir(paths, retention, now, &mut stats);
    gc_blob_dir(paths, retention, now, &mut stats);

    stats
}

pub fn maybe_run_gc(
    paths: &SharedCachePaths,
    retention: Duration,
    throttle: Duration,
) -> Option<GcStats> {
    if !should_run_gc(paths, throttle, SystemTime::now()) {
        return None;
    }

    let stats = run_gc(paths, retention);
    let _ = write_gc_marker(paths, SystemTime::now());
    Some(stats)
}

fn gc_snapshot_dir(
    paths: &SharedCachePaths,
    retention: Duration,
    now: SystemTime,
    stats: &mut GcStats,
) {
    let entries = match fs::read_dir(&paths.snapshots_dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();

        if path.is_dir() {
            gc_snapshot_commit_dir(&path, retention, now, stats);
            continue;
        }

        if !has_file_name_suffix(&path, SNAPSHOT_SUFFIX) {
            continue;
        }
        if !is_older_than(&path, retention, now) {
            continue;
        }

        delete_snapshot_file(&path, stats);
    }
}

fn gc_snapshot_commit_dir(path: &Path, retention: Duration, now: SystemTime, stats: &mut GcStats) {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let shard_path = entry.path();

        if !shard_path.is_file() {
            continue;
        }
        if shard_path.extension().and_then(|ext| ext.to_str()) != Some(SNAPSHOT_FILE_EXTENSION) {
            continue;
        }
        if !is_older_than(&shard_path, retention, now) {
            continue;
        }

        delete_snapshot_shard(&shard_path, stats);
    }

    prune_empty_dir(path);
}

fn delete_snapshot_shard(path: &Path, stats: &mut GcStats) {
    let snapshot_bytes = file_len(path);
    if remove_file_if_exists(path) {
        stats.snapshots_deleted += 1;
        stats.bytes_freed = stats.bytes_freed.saturating_add(snapshot_bytes);
    }

    let sidecar_path = path.with_extension(SNAPSHOT_MERGED_EXTENSION);
    let _ = remove_file_if_exists(&sidecar_path);
}

fn delete_snapshot_file(path: &Path, stats: &mut GcStats) {
    let snapshot_bytes = file_len(path);
    if remove_file_if_exists(path) {
        stats.snapshots_deleted += 1;
        stats.bytes_freed = stats.bytes_freed.saturating_add(snapshot_bytes);
    }
}

fn gc_blob_dir(
    paths: &SharedCachePaths,
    retention: Duration,
    now: SystemTime,
    stats: &mut GcStats,
) {
    let entries = match fs::read_dir(&paths.blobs_dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        if !has_file_name_suffix(&path, BLOB_SUFFIX) {
            continue;
        }
        if !is_older_than(&path, retention, now) {
            continue;
        }

        // Age-based MVP, not reachability-based. Safe because shared cache readers
        // already treat missing blobs as cache misses and rerun task.
        let snapshot_bytes = file_len(&path);
        if remove_file_if_exists(&path) {
            stats.blobs_deleted += 1;
            stats.bytes_freed = stats.bytes_freed.saturating_add(snapshot_bytes);
        }
    }
}

fn should_run_gc(paths: &SharedCachePaths, throttle: Duration, now: SystemTime) -> bool {
    let marker_path = gc_marker_path(paths);
    match fs::metadata(marker_path).and_then(|metadata| metadata.modified()) {
        Ok(modified) => elapsed_at_least(modified, throttle, now),
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(_) => true,
    }
}

fn write_gc_marker(paths: &SharedCachePaths, now: SystemTime) -> io::Result<()> {
    let timestamp = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string();
    atomic_write(&gc_marker_path(paths), timestamp.as_bytes()).map_err(io::Error::other)
}

fn gc_marker_path(paths: &SharedCachePaths) -> PathBuf {
    paths.root.join(LAST_GC_MARKER)
}

fn has_file_name_suffix(path: &Path, suffix: &str) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(suffix))
}

fn is_older_than(path: &Path, retention: Duration, now: SystemTime) -> bool {
    let modified = match fs::metadata(path).and_then(|metadata| metadata.modified()) {
        Ok(modified) => modified,
        Err(_) => return false,
    };
    elapsed_at_least(modified, retention, now)
}

fn elapsed_at_least(earlier: SystemTime, threshold: Duration, now: SystemTime) -> bool {
    now.duration_since(earlier)
        .map(|elapsed| elapsed >= threshold)
        .unwrap_or(false)
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn prune_empty_dir(path: &Path) {
    if !dir_is_empty(path) {
        return;
    }
    let _ = fs::remove_dir(path);
}

fn dir_is_empty(path: &Path) -> bool {
    match fs::read_dir(path) {
        Ok(mut entries) => entries.next().is_none(),
        Err(_) => false,
    }
}

fn remove_file_if_exists(path: &Path) -> bool {
    match fs::remove_file(path) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::{open_shared_paths, write_blob};
    use filetime::FileTime;
    use std::sync::Arc;
    use std::thread;
    use tempfile::tempdir;

    fn entry_for(task_id: &str, outputs_hash: [u8; 32]) -> SnapshotEntry {
        let task_spec_hash = [1; 32];
        let env_hash = [2; 32];
        let pkg_dep_hash = [3; 32];
        SnapshotEntry {
            task_id: task_id.to_owned(),
            input_key: derive_input_key(task_spec_hash, env_hash, pkg_dep_hash, [0; 32]),
            outputs_hash,
            task_spec_hash,
            env_hash,
            pkg_dep_hash,
            duration_ms: 199,
            output_bytes: 15,
            cached_at_unix_ms: 1,
            tool_version: None,
        }
    }

    fn write_snapshot(
        paths: &SharedCachePaths,
        commit_key: &str,
        outputs_hash: [u8; 32],
    ) -> PathBuf {
        let store = SnapshotStore::new(paths.clone());
        let result = store.merge_entry(commit_key, entry_for("pkg#build", outputs_hash));
        assert!(matches!(result, crate::shared::MergeResult::Inserted));
        let shard_dir = paths.snapshots_dir.join(commit_key);
        let mut entries = fs::read_dir(&shard_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("bincode"))
            .collect::<Vec<_>>();
        entries.sort();
        entries.into_iter().next().unwrap()
    }

    fn write_blob_fixture(
        paths: &SharedCachePaths,
        package_dir: &Path,
        outputs_hash: [u8; 32],
    ) -> PathBuf {
        let output_rel = PathBuf::from("dist/out.txt");
        let output_path = package_dir.join(&output_rel);
        fs::create_dir_all(output_path.parent().unwrap()).unwrap();
        fs::write(&output_path, b"shared blob data").unwrap();
        let result = write_blob(
            paths,
            &outputs_hash,
            package_dir,
            &[output_rel],
            1024 * 1024,
        )
        .unwrap();
        assert!(matches!(result, crate::shared::BlobWriteResult::Written));
        paths.blobs_dir.join(format!(
            "{}.tar.zst",
            blake3::Hash::from(outputs_hash).to_hex()
        ))
    }

    #[test]
    fn run_gc_deletes_old_shard_and_merged_sidecar() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let old_snapshot = write_snapshot(&paths, "old-commit", [9; 32]);
        let merged_sidecar = old_snapshot.with_extension(SNAPSHOT_MERGED_EXTENSION);
        fs::write(&merged_sidecar, b"older-shard\n").unwrap();
        let snapshot_bytes = file_len(&old_snapshot);

        let stale = SystemTime::now() - Duration::from_secs(3 * 24 * 60 * 60);
        set_mtime(&old_snapshot, stale);
        set_mtime(&merged_sidecar, stale);

        let stats = run_gc(&paths, Duration::from_secs(24 * 60 * 60));

        assert_eq!(stats.snapshots_deleted, 1);
        assert_eq!(stats.blobs_deleted, 0);
        assert_eq!(stats.bytes_freed, snapshot_bytes);
        assert!(!old_snapshot.exists());
        assert!(!merged_sidecar.exists());
        assert!(!old_snapshot.parent().unwrap().exists());
    }

    #[test]
    fn run_gc_prunes_empty_commit_dir_after_shard_deletion() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let old_snapshot = write_snapshot(&paths, "old-commit", [9; 32]);
        let commit_dir = old_snapshot.parent().unwrap().to_path_buf();
        let stale = SystemTime::now() - Duration::from_secs(3 * 24 * 60 * 60);
        set_mtime(&old_snapshot, stale);

        let stats = run_gc(&paths, Duration::from_secs(24 * 60 * 60));

        assert_eq!(stats.snapshots_deleted, 1);
        assert!(!commit_dir.exists());
    }

    #[test]
    fn run_gc_deletes_old_legacy_snapshot_file() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let legacy_snapshot = paths.snapshots_dir.join("legacy-commit.bincode");
        fs::write(&legacy_snapshot, b"legacy snapshot").unwrap();
        let expected_bytes = file_len(&legacy_snapshot);
        let stale = SystemTime::now() - Duration::from_secs(3 * 24 * 60 * 60);
        set_mtime(&legacy_snapshot, stale);

        let stats = run_gc(&paths, Duration::from_secs(24 * 60 * 60));

        assert_eq!(stats.snapshots_deleted, 1);
        assert_eq!(stats.blobs_deleted, 0);
        assert_eq!(stats.bytes_freed, expected_bytes);
        assert!(!legacy_snapshot.exists());
    }

    #[test]
    fn run_gc_deletes_old_snapshots_and_blobs() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let package_dir = temp_dir.path().join("pkg");
        fs::create_dir_all(&package_dir).unwrap();

        let old_snapshot = write_snapshot(&paths, "old-commit", [9; 32]);
        let old_blob = write_blob_fixture(&paths, &package_dir, [8; 32]);

        let recent_snapshot = paths.snapshots_dir.join("recent-commit.bincode");
        fs::write(&recent_snapshot, b"recent").unwrap();
        let recent_blob = paths.blobs_dir.join("recent.tar.zst");
        fs::write(&recent_blob, b"recent blob").unwrap();

        let stale = SystemTime::now() - Duration::from_secs(3 * 24 * 60 * 60);
        set_mtime(&old_snapshot, stale);
        set_mtime(&old_blob, stale);

        let stats = run_gc(&paths, Duration::from_secs(24 * 60 * 60));

        assert_eq!(stats.snapshots_deleted, 1);
        assert_eq!(stats.blobs_deleted, 1);
        assert!(stats.bytes_freed > 0);
        assert!(!old_snapshot.exists());
        assert!(!old_snapshot.parent().unwrap().exists());
        assert!(!old_blob.exists());
        assert!(recent_snapshot.exists());
        assert!(recent_blob.exists());
    }

    #[test]
    fn maybe_run_gc_throttles_back_to_back_runs() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let package_dir = temp_dir.path().join("pkg");
        fs::create_dir_all(&package_dir).unwrap();
        let old_blob = write_blob_fixture(&paths, &package_dir, [7; 32]);
        let stale = SystemTime::now() - Duration::from_secs(3 * 24 * 60 * 60);
        set_mtime(&old_blob, stale);

        let first = maybe_run_gc(
            &paths,
            Duration::from_secs(24 * 60 * 60),
            Duration::from_secs(24 * 60 * 60),
        );
        let second = maybe_run_gc(
            &paths,
            Duration::from_secs(24 * 60 * 60),
            Duration::from_secs(24 * 60 * 60),
        );

        assert!(first.is_some());
        assert!(second.is_none());
        assert!(gc_marker_path(&paths).exists());
    }

    #[test]
    fn reader_hitting_gcd_shard_degrades_to_miss_not_error() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let commit_key = "commit-race";
        let outputs_hash = [6; 32];
        let snapshot_path = write_snapshot(&paths, commit_key, outputs_hash);

        assert!(SnapshotStore::new(paths.clone()).load(commit_key).is_some());
        assert!(remove_file_if_exists(&snapshot_path));
        prune_empty_dir(snapshot_path.parent().unwrap());

        let loaded = SnapshotStore::new(paths.clone()).load(commit_key);
        assert!(loaded.is_none());
    }

    #[test]
    fn concurrent_reader_racing_gc_degrades_to_miss_not_panic() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let package_dir = temp_dir.path().join("pkg");
        fs::create_dir_all(&package_dir).unwrap();
        let commit_key = "commit-race";
        let outputs_hash = [6; 32];
        let snapshot_path = write_snapshot(&paths, commit_key, outputs_hash);
        let blob_path = write_blob_fixture(&paths, &package_dir, outputs_hash);
        let stale = SystemTime::now() - Duration::from_secs(3 * 24 * 60 * 60);
        set_mtime(&snapshot_path, stale);
        set_mtime(&blob_path, stale);

        let reader_paths = Arc::new(paths.clone());
        let reader_package_dir = package_dir.clone();
        let reader = thread::spawn(move || {
            let store = SnapshotStore::new((*reader_paths).clone());
            for _ in 0..200 {
                let _ = store.load(commit_key);
                let result = restore_blob(&reader_paths, &outputs_hash, &reader_package_dir)
                    .expect("blob reader should tolerate missing blobs");
                assert!(matches!(
                    result,
                    crate::shared::BlobReadResult::Restored
                        | crate::shared::BlobReadResult::Missing
                        | crate::shared::BlobReadResult::Corrupt
                ));
            }
        });

        let stats = run_gc(&paths, Duration::from_secs(24 * 60 * 60));
        reader.join().unwrap();

        assert!(stats.snapshots_deleted <= 1);
        assert!(stats.blobs_deleted <= 1);
    }

    fn set_mtime(path: &Path, modified: SystemTime) {
        let seconds = modified.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        filetime::set_file_mtime(path, FileTime::from_unix_time(seconds, 0)).unwrap();
    }
}
