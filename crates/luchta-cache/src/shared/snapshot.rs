use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::path::PathBuf;

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::shared::{atomic_write, SharedCachePaths};

pub const SNAPSHOT_SCHEMA_VERSION: u32 = 2;
const DEP_OUTPUTS_HASH_DOMAIN: &[u8] = b"luchta:dep-outputs:v1";
const DEP_OUTPUTS_HASH_SEPARATOR: u8 = 0;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotEntry {
    pub task_id: String,
    pub input_key: [u8; 32],
    pub outputs_hash: [u8; 32],
    pub task_spec_hash: [u8; 32],
    pub env_hash: [u8; 32],
    pub pkg_dep_hash: [u8; 32],
    pub duration_ms: u64,
    pub output_bytes: u64,
    pub cached_at_unix_ms: u64,
    pub tool_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema_version: u32,
    pub entries: BTreeMap<String, SnapshotEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeResult {
    Inserted,
    IdempotentNoop,
    ConflictKeptExisting,
    SkippedLockUnavailable,
}

#[derive(Debug, Clone)]
pub struct SnapshotStore {
    paths: SharedCachePaths,
    /// Optional per-instance load counter for testing. None in production.
    #[cfg(test)]
    load_count: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
}

impl Snapshot {
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            entries: BTreeMap::new(),
        }
    }
}

impl Default for Snapshot {
    fn default() -> Self {
        Self::new()
    }
}

impl SnapshotStore {
    #[must_use]
    pub fn new(paths: SharedCachePaths) -> Self {
        Self {
            paths,
            #[cfg(test)]
            load_count: None,
        }
    }

    /// Creates a new SnapshotStore with a per-instance load counter for testing.
    /// Each call to `load` increments this counter, allowing tests to verify
    /// that snapshots are loaded exactly once per instance (not per-call).
    #[cfg(test)]
    pub fn new_with_counter(
        paths: SharedCachePaths,
    ) -> (Self, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;

        let counter = Arc::new(AtomicUsize::new(0));
        (
            Self {
                paths,
                load_count: Some(Arc::clone(&counter)),
            },
            counter,
        )
    }

    /// Get the paths for this store (needed for test constructor).
    #[cfg(test)]
    pub fn paths(&self) -> &SharedCachePaths {
        &self.paths
    }

    pub fn load(&self, commit_key: &str) -> Option<Snapshot> {
        let path = self.snapshot_path(commit_key);
        let bytes = fs::read(path).ok()?;
        #[cfg(test)]
        {
            use std::sync::atomic::Ordering;
            if let Some(counter) = &self.load_count {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        }
        decode_snapshot(&bytes, commit_key).ok()
    }

    pub fn merge_entry(&self, commit_key: &str, entry: SnapshotEntry) -> MergeResult {
        let lock_path = self.lock_path(commit_key);
        let _ = fs::create_dir_all(&self.paths.snapshots_dir);

        // Lock sidecar file, not snapshot data file itself. atomic_write() does temp+rename,
        // which swaps inode for `<commit>.bincode`; sidecar keeps stable flock handle across RMW.
        let lock_file = match OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
        {
            Ok(lock_file) => lock_file,
            Err(err) => {
                eprintln!(
                    "warning: failed to open snapshot lock file {}: {err}; skipping shared snapshot write",
                    lock_path.display()
                );
                return MergeResult::SkippedLockUnavailable;
            }
        };
        if let Err(err) = lock_file.lock_exclusive() {
            eprintln!(
                "warning: failed to lock snapshot lock file {}: {err}; skipping shared snapshot write",
                lock_path.display()
            );
            return MergeResult::SkippedLockUnavailable;
        }

        let snapshot_path = self.snapshot_path(commit_key);
        let bytes = fs::read(&snapshot_path).ok();
        let mut snapshot = match bytes {
            Some(bytes) => decode_snapshot(&bytes, commit_key).unwrap_or_else(|_| Snapshot::new()),
            None => Snapshot::new(),
        };

        let entry_key = input_key_hex(entry.input_key);
        let result = match snapshot.entries.get(&entry_key) {
            None => {
                snapshot.entries.insert(entry_key, entry);
                MergeResult::Inserted
            }
            Some(existing) if existing.outputs_hash == entry.outputs_hash => {
                MergeResult::IdempotentNoop
            }
            Some(existing) => {
                eprintln!(
                    "warn: snapshot conflict kept existing for task_id={} existing_outputs_hash={} new_outputs_hash={}",
                    existing.task_id,
                    hex_hash(existing.outputs_hash),
                    hex_hash(entry.outputs_hash)
                );
                MergeResult::ConflictKeptExisting
            }
        };

        if matches!(result, MergeResult::Inserted) {
            let encoded = bincode::serde::encode_to_vec(&snapshot, bincode_config())
                .expect("snapshot serialization should succeed");
            if let Err(err) = atomic_write(&snapshot_path, &encoded) {
                eprintln!(
                    "warning: failed to write snapshot {}: {err}; skipping shared snapshot write",
                    snapshot_path.display()
                );
                return MergeResult::SkippedLockUnavailable;
            }
        }

        result
    }

    pub fn lookup(&self, commit_key: &str, input_key: &[u8; 32]) -> Option<SnapshotEntry> {
        let snapshot = self.load(commit_key)?;
        snapshot.entries.get(&input_key_hex(*input_key)).cloned()
    }

    fn snapshot_path(&self, commit_key: &str) -> PathBuf {
        self.paths
            .snapshots_dir
            .join(format!("{commit_key}.bincode"))
    }

    fn lock_path(&self, commit_key: &str) -> PathBuf {
        self.paths
            .snapshots_dir
            .join(format!("{commit_key}.bincode.lock"))
    }
}

pub fn input_key_hex(input_key: [u8; 32]) -> String {
    blake3::Hash::from(input_key).to_hex().to_string()
}

#[must_use]
pub fn derive_input_key(
    task_spec_hash: [u8; 32],
    env_hash: [u8; 32],
    pkg_dep_hash: [u8; 32],
    dep_outputs_hash: [u8; 32],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&task_spec_hash);
    hasher.update(&env_hash);
    hasher.update(&pkg_dep_hash);
    hasher.update(&dep_outputs_hash);
    *hasher.finalize().as_bytes()
}

#[must_use]
pub fn combined_dep_outputs_hash(dep_outputs_hashes: &BTreeMap<String, [u8; 32]>) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(DEP_OUTPUTS_HASH_DOMAIN);

    for (task_id, outputs_hash) in dep_outputs_hashes {
        hasher.update(task_id.as_bytes());
        hasher.update(&[DEP_OUTPUTS_HASH_SEPARATOR]);
        hasher.update(outputs_hash);
        hasher.update(&[DEP_OUTPUTS_HASH_SEPARATOR]);
    }

    *hasher.finalize().as_bytes()
}

fn bincode_config() -> impl bincode::config::Config {
    bincode::config::standard()
}

fn decode_snapshot(
    bytes: &[u8],
    _commit_key: &str,
) -> Result<Snapshot, bincode::error::DecodeError> {
    let (snapshot, _): (Snapshot, usize) =
        bincode::serde::decode_from_slice(bytes, bincode_config())?;
    if snapshot.schema_version != SNAPSHOT_SCHEMA_VERSION {
        return Err(bincode::error::DecodeError::OtherString(
            "unsupported snapshot schema version".to_owned(),
        ));
    }
    Ok(snapshot)
}

fn hex_hash(hash: [u8; 32]) -> String {
    blake3::Hash::from(hash).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::thread;

    use tempfile::tempdir;

    use super::*;
    use crate::shared::open_shared_paths;

    #[test]
    fn derive_input_key_changes_when_any_component_changes() {
        let base = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);

        assert_ne!(base, derive_input_key([9; 32], [2; 32], [3; 32], [4; 32]));
        assert_ne!(base, derive_input_key([1; 32], [9; 32], [3; 32], [4; 32]));
        assert_ne!(base, derive_input_key([1; 32], [2; 32], [9; 32], [4; 32]));
        assert_ne!(base, derive_input_key([1; 32], [2; 32], [3; 32], [9; 32]));
    }

    #[test]
    fn input_key_hex_encodes_full_hash() {
        let key = [0xAB; 32];
        assert_eq!(
            input_key_hex(key),
            "abababababababababababababababababababababababababababababababab"
        );
    }

    #[test]
    fn combined_dep_outputs_hash_empty_is_stable() {
        let first = combined_dep_outputs_hash(&BTreeMap::new());
        let second = combined_dep_outputs_hash(&BTreeMap::new());
        assert_eq!(first, second);
    }

    #[test]
    fn combined_dep_outputs_hash_changes_when_dependency_hash_changes() {
        let first = combined_dep_outputs_hash(&sample_dep_outputs_one());
        let second = combined_dep_outputs_hash(&sample_dep_outputs_two());
        assert_ne!(first, second);
    }

    #[test]
    fn combined_dep_outputs_hash_order_independent_for_same_map() {
        let mut ordered = BTreeMap::new();
        ordered.insert("pkg-a#lint".to_owned(), [7; 32]);
        ordered.insert("pkg-b#build".to_owned(), [8; 32]);

        let mut same_entries = BTreeMap::new();
        same_entries.insert("pkg-b#build".to_owned(), [8; 32]);
        same_entries.insert("pkg-a#lint".to_owned(), [7; 32]);

        assert_eq!(
            combined_dep_outputs_hash(&ordered),
            combined_dep_outputs_hash(&same_entries)
        );
    }

    #[test]
    fn snapshot_serialization_round_trip_preserves_entry_map() {
        let snapshot = sample_snapshot();
        let bytes = bincode::serde::encode_to_vec(&snapshot, bincode_config())
            .expect("snapshot serialization should succeed");
        let (decoded, _): (Snapshot, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode_config())
                .expect("snapshot deserialization should succeed");

        assert_eq!(decoded, snapshot);
        assert_eq!(decoded.schema_version, SNAPSHOT_SCHEMA_VERSION);
    }

    #[test]
    fn snapshot_schema_version_initializes_to_current() {
        let snapshot = Snapshot::new();
        assert_eq!(snapshot.schema_version, SNAPSHOT_SCHEMA_VERSION);
        assert!(snapshot.entries.is_empty());
    }

    #[test]
    fn snapshot_supports_multiple_variants_for_same_task_id() {
        let task_id = "pkg-a#build".to_owned();
        let first_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);
        let second_key = derive_input_key([1; 32], [9; 32], [3; 32], [4; 32]);

        let first_entry = SnapshotEntry {
            task_id: task_id.clone(),
            input_key: first_key,
            outputs_hash: [5; 32],
            task_spec_hash: [1; 32],
            env_hash: [2; 32],
            pkg_dep_hash: [3; 32],
            duration_ms: 10,
            output_bytes: 20,
            cached_at_unix_ms: 30,
            tool_version: Some("1.0.0".to_owned()),
        };
        let second_entry = SnapshotEntry {
            task_id: task_id.clone(),
            input_key: second_key,
            outputs_hash: [5; 32],
            task_spec_hash: [1; 32],
            env_hash: [9; 32],
            pkg_dep_hash: [3; 32],
            duration_ms: 11,
            output_bytes: 21,
            cached_at_unix_ms: 31,
            tool_version: Some("1.0.1".to_owned()),
        };

        let mut snapshot = Snapshot::new();
        snapshot
            .entries
            .insert(input_key_hex(first_key), first_entry.clone());
        snapshot
            .entries
            .insert(input_key_hex(second_key), second_entry.clone());

        let bytes = bincode::serde::encode_to_vec(&snapshot, bincode_config())
            .expect("snapshot serialization should succeed");
        let (decoded, _): (Snapshot, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode_config())
                .expect("snapshot deserialization should succeed");

        assert_eq!(decoded.schema_version, SNAPSHOT_SCHEMA_VERSION);
        assert_eq!(decoded.entries.len(), 2);
        assert_eq!(
            decoded.entries.get(&input_key_hex(first_key)),
            Some(&first_entry)
        );
        assert_eq!(
            decoded.entries.get(&input_key_hex(second_key)),
            Some(&second_entry)
        );
    }

    #[test]
    fn snapshot_store_insert_then_lookup_returns_entry() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let store = SnapshotStore::new(paths);
        let entry = sample_entry_with_seed(1, [41; 32]);

        assert_eq!(
            store.merge_entry("commit-a", entry.clone()),
            MergeResult::Inserted
        );
        assert_eq!(
            store.lookup("commit-a", &entry.input_key),
            Some(entry.clone())
        );
        assert_eq!(store.lookup("commit-a", &[99; 32]), None);
    }

    #[test]
    fn snapshot_store_idempotent_merge_keeps_single_entry() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let store = SnapshotStore::new(paths);
        let entry = sample_entry_with_seed(2, [42; 32]);

        assert_eq!(
            store.merge_entry("commit-b", entry.clone()),
            MergeResult::Inserted
        );
        assert_eq!(
            store.merge_entry("commit-b", entry.clone()),
            MergeResult::IdempotentNoop
        );

        let snapshot = store.load("commit-b").unwrap();
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(
            snapshot.entries.get(&input_key_hex(entry.input_key)),
            Some(&entry)
        );
    }

    #[test]
    fn snapshot_store_conflict_keeps_original_entry() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let store = SnapshotStore::new(paths);
        let original = sample_entry_with_seed(3, [43; 32]);
        let mut conflicting = original.clone();
        conflicting.outputs_hash = [99; 32];

        assert_eq!(
            store.merge_entry("commit-c", original.clone()),
            MergeResult::Inserted
        );
        assert_eq!(
            store.merge_entry("commit-c", conflicting),
            MergeResult::ConflictKeptExisting
        );
        assert_eq!(
            store.lookup("commit-c", &original.input_key),
            Some(original)
        );
    }

    #[test]
    fn snapshot_store_load_corrupt_file_returns_none() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let store = SnapshotStore::new(paths.clone());
        let snapshot_path = paths.snapshots_dir.join("commit-d.bincode");
        fs::write(snapshot_path, b"not-bincode").unwrap();

        assert_eq!(store.load("commit-d"), None);
    }

    #[test]
    fn snapshot_store_concurrent_merges_preserve_all_entries() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let store = Arc::new(SnapshotStore::new(paths));
        let commit_key = "commit-e";
        let threads = 16usize;

        let handles = (0..threads)
            .map(|i| {
                let store = Arc::clone(&store);
                thread::spawn(move || {
                    let entry = sample_entry_with_seed(i as u8 + 10, [i as u8 + 1; 32]);
                    assert_eq!(store.merge_entry(commit_key, entry), MergeResult::Inserted);
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            handle.join().unwrap();
        }

        let snapshot = store.load(commit_key).unwrap();
        assert_eq!(snapshot.entries.len(), threads);
        for i in 0..threads {
            let entry = sample_entry_with_seed(i as u8 + 10, [i as u8 + 1; 32]);
            assert_eq!(
                snapshot.entries.get(&input_key_hex(entry.input_key)),
                Some(&entry)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_store_lock_failure_skips_instead_of_panicking() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        fs::set_permissions(&paths.snapshots_dir, fs::Permissions::from_mode(0o500)).unwrap();
        let store = SnapshotStore::new(paths.clone());

        let result = store.merge_entry("commit-lock-fail", sample_entry_with_seed(42, [9; 32]));

        assert_eq!(result, MergeResult::SkippedLockUnavailable);
        assert_eq!(store.load("commit-lock-fail"), None);
    }

    fn sample_snapshot() -> Snapshot {
        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);
        let entry = SnapshotEntry {
            task_id: "pkg-a#build".to_owned(),
            input_key,
            outputs_hash: [5; 32],
            task_spec_hash: [1; 32],
            env_hash: [2; 32],
            pkg_dep_hash: [3; 32],
            duration_ms: 42,
            output_bytes: 128,
            cached_at_unix_ms: 1_700_000_000_000,
            tool_version: Some("0.1.0".to_owned()),
        };

        let mut snapshot = Snapshot::new();
        snapshot.entries.insert(input_key_hex(input_key), entry);
        snapshot
    }

    fn sample_entry_with_seed(seed: u8, outputs_hash: [u8; 32]) -> SnapshotEntry {
        let task_spec_hash = [seed; 32];
        let env_hash = [seed.wrapping_add(1); 32];
        let pkg_dep_hash = [seed.wrapping_add(2); 32];
        let dep_outputs_hash = [seed.wrapping_add(3); 32];
        SnapshotEntry {
            task_id: format!("pkg-{seed}#build"),
            input_key: derive_input_key(task_spec_hash, env_hash, pkg_dep_hash, dep_outputs_hash),
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

    fn sample_dep_outputs_one() -> BTreeMap<String, [u8; 32]> {
        BTreeMap::from([
            ("pkg-a#lint".to_owned(), [7; 32]),
            ("pkg-b#build".to_owned(), [8; 32]),
        ])
    }

    fn sample_dep_outputs_two() -> BTreeMap<String, [u8; 32]> {
        BTreeMap::from([
            ("pkg-a#lint".to_owned(), [7; 32]),
            ("pkg-b#build".to_owned(), [9; 32]),
        ])
    }
}
