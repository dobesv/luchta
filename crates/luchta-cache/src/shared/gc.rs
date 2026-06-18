use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::shared::{atomic_write, SharedCachePaths};

#[cfg(test)]
use crate::shared::{derive_input_key, restore_blob, SnapshotEntry, SnapshotStore};

/// Default shared-cache retention window. P6.1 makes this env-configurable.
pub const DEFAULT_GC_RETENTION: Duration = Duration::from_secs(14 * 24 * 60 * 60);
/// Default throttle window for opportunistic GC runs.
pub const DEFAULT_GC_THROTTLE: Duration = Duration::from_secs(24 * 60 * 60);
const LAST_GC_MARKER: &str = ".last-gc";
const SNAPSHOT_SUFFIX: &str = ".bincode";
const LOCK_SUFFIX: &str = ".bincode.lock";
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
        if !has_file_name_suffix(&path, SNAPSHOT_SUFFIX) || has_file_name_suffix(&path, LOCK_SUFFIX)
        {
            continue;
        }
        if !is_older_than(&path, retention, now) {
            continue;
        }

        let snapshot_bytes = file_len(&path);
        if remove_file_if_exists(&path) {
            stats.snapshots_deleted += 1;
            stats.bytes_freed = stats.bytes_freed.saturating_add(snapshot_bytes);
        }
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

#[cfg(test)]
fn snapshot_lock_path(snapshot_path: &Path) -> PathBuf {
    let Some(file_name) = snapshot_path.file_name().and_then(|name| name.to_str()) else {
        return snapshot_path.with_extension("lock");
    };
    snapshot_path.with_file_name(format!("{file_name}.lock"))
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
        paths.snapshots_dir.join(format!("{commit_key}.bincode"))
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
    fn run_gc_deletes_old_snapshots_and_blobs_but_leaves_lock_files() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let package_dir = temp_dir.path().join("pkg");
        fs::create_dir_all(&package_dir).unwrap();

        let old_snapshot = write_snapshot(&paths, "old-commit", [9; 32]);
        let old_lock = snapshot_lock_path(&old_snapshot);
        fs::write(&old_lock, b"lock").unwrap();
        let old_blob = write_blob_fixture(&paths, &package_dir, [8; 32]);

        let recent_snapshot = paths.snapshots_dir.join("recent-commit.bincode");
        fs::write(&recent_snapshot, b"recent").unwrap();
        let recent_lock = snapshot_lock_path(&recent_snapshot);
        fs::write(&recent_lock, b"lock").unwrap();
        let recent_blob = paths.blobs_dir.join("recent.tar.zst");
        fs::write(&recent_blob, b"recent blob").unwrap();

        let stale = SystemTime::now() - Duration::from_secs(3 * 24 * 60 * 60);
        set_mtime(&old_snapshot, stale);
        set_mtime(&old_lock, stale);
        set_mtime(&old_blob, stale);

        let stats = run_gc(&paths, Duration::from_secs(24 * 60 * 60));

        assert_eq!(stats.snapshots_deleted, 1);
        assert_eq!(stats.blobs_deleted, 1);
        assert!(stats.bytes_freed > 0);
        assert!(!old_snapshot.exists());
        assert!(old_lock.exists());
        assert!(!old_blob.exists());
        assert!(recent_snapshot.exists());
        assert!(recent_lock.exists());
        assert!(recent_blob.exists());
    }

    #[test]
    fn run_gc_leaves_orphan_snapshot_lock_in_place() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let snapshot_path = paths.snapshots_dir.join("commit-orphan.bincode");
        let lock_path = snapshot_lock_path(&snapshot_path);
        fs::write(&lock_path, b"lock").unwrap();
        let stale = SystemTime::now() - Duration::from_secs(3 * 24 * 60 * 60);
        set_mtime(&lock_path, stale);

        let stats = run_gc(&paths, Duration::from_secs(24 * 60 * 60));

        assert_eq!(stats.snapshots_deleted, 0);
        assert_eq!(stats.blobs_deleted, 0);
        assert_eq!(stats.bytes_freed, 0);
        assert!(lock_path.exists());
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
