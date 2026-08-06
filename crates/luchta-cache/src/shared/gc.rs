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
/// Marker file for the GC throttle.
const GC_MARKER_NAME: &str = ".gc-marker";
/// Marker file for the shard rollup throttle.
///
/// A separate marker from `GC_MARKER_NAME` so a GC run and a rollup run don't
/// throttle each other: `mod.rs::build_index` runs its rollup on its own
/// schedule, independent of the CLI's `maybe_run_gc` call.
pub const ROLLUP_MARKER_NAME: &str = ".rollup-marker";
const SNAPSHOT_SUFFIX: &str = ".bincode";
const BLOB_SUFFIX: &str = ".tar.zst";
const ENTRY_META_SUFFIX: &str = ".bin";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcStats {
    pub snapshots_deleted: u64,
    pub blobs_deleted: u64,
    pub entries_deleted: usize,
    pub bytes_freed: u64,
}

#[must_use]
pub fn run_gc(paths: &SharedCachePaths, retention: Duration) -> GcStats {
    let now = SystemTime::now();
    let mut stats = GcStats::default();

    gc_snapshot_dir(paths, retention, now, &mut stats);
    gc_blob_dir(paths, retention, now, &mut stats);
    gc_entries_dir(paths, retention, now, &mut stats);

    stats
}

pub fn maybe_run_gc(
    paths: &SharedCachePaths,
    retention: Duration,
    throttle: Duration,
) -> Option<GcStats> {
    if !should_run_marked(paths, GC_MARKER_NAME, throttle, SystemTime::now()) {
        return None;
    }

    let stats = run_gc(paths, retention);
    let _ = write_marker(paths, GC_MARKER_NAME, SystemTime::now());
    Some(stats)
}

/// True if `throttle` has elapsed since the last shard rollup. Stamps the
/// rollup marker so a subsequent call within the throttle window returns
/// `false`.
///
/// Throttled independently of `maybe_run_gc`: rollups run inside
/// `build_index` (see `mod.rs::maybe_write_rollup`), not from the CLI's GC
/// hook, and re-serialize the whole discovered index, so they need their own
/// cadence rather than piggybacking on the GC throttle.
#[must_use]
pub fn should_run_rollup(paths: &SharedCachePaths, throttle: Duration) -> bool {
    if !should_run_marked(paths, ROLLUP_MARKER_NAME, throttle, SystemTime::now()) {
        return false;
    }
    let _ = write_marker(paths, ROLLUP_MARKER_NAME, SystemTime::now());
    true
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

fn gc_entries_dir(
    paths: &SharedCachePaths,
    retention: Duration,
    now: SystemTime,
    stats: &mut GcStats,
) {
    let entries = match fs::read_dir(&paths.entries_dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !has_file_name_suffix(&path, ENTRY_META_SUFFIX) {
            continue;
        }
        if !is_older_than(&path, retention, now) {
            continue;
        }

        // Age-based, like blobs. A reader that loses the race treats the
        // missing meta as a cache miss and reruns the task.
        let meta_bytes = file_len(&path);
        if remove_file_if_exists(&path) {
            stats.entries_deleted += 1;
            stats.bytes_freed = stats.bytes_freed.saturating_add(meta_bytes);
        }
    }
}

/// True if `throttle` has elapsed since the marker named `marker_name` was
/// last stamped (or if it has never been stamped). Shared by the GC throttle
/// and the rollup throttle, each with its own marker file, so neither run
/// throttles the other.
fn should_run_marked(
    paths: &SharedCachePaths,
    marker_name: &str,
    throttle: Duration,
    now: SystemTime,
) -> bool {
    let path = marker_path(paths, marker_name);
    match fs::metadata(path).and_then(|metadata| metadata.modified()) {
        Ok(modified) => elapsed_at_least(modified, throttle, now),
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(_) => true,
    }
}

fn write_marker(paths: &SharedCachePaths, marker_name: &str, now: SystemTime) -> io::Result<()> {
    let timestamp = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string();
    atomic_write(&marker_path(paths, marker_name), timestamp.as_bytes()).map_err(io::Error::other)
}

fn marker_path(paths: &SharedCachePaths, marker_name: &str) -> PathBuf {
    paths.root.join(marker_name)
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
    use crate::shared::{open_shared_paths, write_outputs_blob};
    use filetime::FileTime;
    use std::sync::Arc;
    use std::thread;
    use tempfile::{tempdir, TempDir};

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
        let result = write_outputs_blob(
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
        assert!(marker_path(&paths, GC_MARKER_NAME).exists());
    }

    #[test]
    fn should_run_rollup_throttles_back_to_back_calls() {
        let temp = TempDir::new().unwrap();
        let paths = crate::shared::paths::open_shared_paths(temp.path()).unwrap();

        assert!(should_run_rollup(&paths, Duration::from_secs(3600)));
        assert!(!should_run_rollup(&paths, Duration::from_secs(3600)));
    }

    #[test]
    fn rollup_throttle_is_independent_of_the_gc_throttle() {
        let temp = TempDir::new().unwrap();
        let paths = crate::shared::paths::open_shared_paths(temp.path()).unwrap();

        assert!(should_run_rollup(&paths, Duration::from_secs(3600)));
        // GC has its own marker, so it is still eligible.
        assert!(maybe_run_gc(&paths, Duration::from_secs(60), Duration::from_secs(3600)).is_some());
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

    #[test]
    fn run_gc_deletes_old_entry_meta() {
        let temp = TempDir::new().unwrap();
        let paths = crate::shared::paths::open_shared_paths(temp.path()).unwrap();
        let input_key = [12_u8; 32];

        let meta = crate::shared::EntryMeta {
            schema_version: crate::shared::ENTRY_META_SCHEMA_VERSION,
            outputs_hash: [1; 32],
            has_outputs: false,
            record: vec![0],
            stdout: Vec::new(),
            stderr: Vec::new(),
            reports: Vec::new(),
        };
        crate::shared::write_entry_meta(&paths, &input_key, &meta).unwrap();

        let path = crate::shared::entry_meta_path(&paths, &input_key);
        set_mtime(
            &path,
            SystemTime::now() - Duration::from_secs(60 * 60 * 24 * 30),
        );

        let stats = run_gc(&paths, Duration::from_secs(60 * 60 * 24 * 7));

        assert_eq!(stats.entries_deleted, 1);
        assert!(!path.exists());
    }

    #[test]
    fn run_gc_keeps_recent_entry_meta() {
        let temp = TempDir::new().unwrap();
        let paths = crate::shared::paths::open_shared_paths(temp.path()).unwrap();
        let input_key = [13_u8; 32];

        let meta = crate::shared::EntryMeta {
            schema_version: crate::shared::ENTRY_META_SCHEMA_VERSION,
            outputs_hash: [1; 32],
            has_outputs: false,
            record: vec![0],
            stdout: Vec::new(),
            stderr: Vec::new(),
            reports: Vec::new(),
        };
        crate::shared::write_entry_meta(&paths, &input_key, &meta).unwrap();

        let stats = run_gc(&paths, Duration::from_secs(60 * 60 * 24 * 7));

        assert_eq!(stats.entries_deleted, 0);
        assert!(crate::shared::entry_meta_path(&paths, &input_key).exists());
    }

    fn set_mtime(path: &Path, modified: SystemTime) {
        let seconds = modified.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        filetime::set_file_mtime(path, FileTime::from_unix_time(seconds, 0)).unwrap();
    }
}
