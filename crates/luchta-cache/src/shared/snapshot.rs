use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::shared::{atomic_write, SharedCachePaths};

pub const SNAPSHOT_SCHEMA_VERSION: u32 = 2;
const DEP_OUTPUTS_HASH_DOMAIN: &[u8] = b"luchta:dep-outputs:v1";
const DEP_OUTPUTS_HASH_SEPARATOR: u8 = 0;
pub(crate) const SNAPSHOT_FILE_EXTENSION: &str = "bincode";
pub(crate) const SNAPSHOT_MERGED_EXTENSION: &str = "merged";

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotUpload {
    pub shard_id: String,
    pub shard_bytes: Vec<u8>,
    pub merged_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeEntryOutcome {
    pub result: MergeResult,
    pub new_snapshot_upload: Option<SnapshotUpload>,
    pub subsumed_shard_ids: Vec<String>,
}

impl MergeEntryOutcome {
    fn from_result(result: MergeResult) -> Self {
        Self {
            result,
            new_snapshot_upload: None,
            subsumed_shard_ids: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotStore {
    paths: SharedCachePaths,
    /// Optional per-instance load counter for testing. None in production.
    #[cfg(test)]
    load_count: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SnapshotShard {
    shard_id: String,
    source: SnapshotShardSource,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum SnapshotShardSource {
    LegacyFile(PathBuf),
    ShardFile(PathBuf),
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

impl SnapshotEntry {
    #[must_use]
    pub fn dep_outputs_hash(&self) -> [u8; 32] {
        derive_input_key(
            self.task_spec_hash,
            self.env_hash,
            self.pkg_dep_hash,
            [0; 32],
        )
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

    /// Get paths for this store.
    pub fn paths(&self) -> &SharedCachePaths {
        &self.paths
    }

    pub fn load(&self, commit_key: &str) -> Option<Snapshot> {
        let shards = self.list_snapshot_shards(commit_key);
        if shards.is_empty() {
            return None;
        }

        #[cfg(test)]
        {
            use std::sync::atomic::Ordering;
            if let Some(counter) = &self.load_count {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        }

        self.load_merged_snapshot_from_shards(commit_key, shards)
    }

    pub fn merge_entry(&self, commit_key: &str, entry: SnapshotEntry) -> MergeResult {
        self.merge_entry_with_outcome(commit_key, entry).result
    }

    pub fn merge_entry_with_outcome(
        &self,
        commit_key: &str,
        entry: SnapshotEntry,
    ) -> MergeEntryOutcome {
        let shard_dir = self.shard_dir_path(commit_key);
        if let Err(err) = fs::create_dir_all(&shard_dir) {
            eprintln!(
                "warning: failed to create snapshot shard dir {}: {err}; skipping shared snapshot write",
                shard_dir.display()
            );
            return MergeEntryOutcome::from_result(MergeResult::SkippedLockUnavailable);
        }

        let visible_shards = self.list_snapshot_shards(commit_key);
        let merged_snapshot = self
            .load_merged_snapshot_from_shards(commit_key, visible_shards.clone())
            .unwrap_or_default();

        if let Some(existing) = merged_snapshot.entries.get(&input_key_hex(entry.input_key)) {
            if existing.outputs_hash == entry.outputs_hash {
                return MergeEntryOutcome::from_result(MergeResult::IdempotentNoop);
            }

            return MergeEntryOutcome::from_result(MergeResult::ConflictKeptExisting);
        }

        let mut consolidated = merged_snapshot;
        let entry_key = input_key_hex(entry.input_key);
        consolidated.entries.insert(entry_key, entry);

        self.write_consolidated_shard(commit_key, &consolidated, &visible_shards)
    }

    /// Writes the consolidated shard + `.merged` sidecar and deletes the shards
    /// it subsumes. Returns the merge outcome (new shard id + subsumed ids).
    fn write_consolidated_shard(
        &self,
        commit_key: &str,
        consolidated: &Snapshot,
        visible_shards: &[SnapshotShard],
    ) -> MergeEntryOutcome {
        let shard_dir = self.shard_dir_path(commit_key);
        let encoded = bincode::serde::encode_to_vec(consolidated, bincode_config())
            .expect("snapshot serialization should succeed");
        let shard_id = blake3::hash(&encoded).to_hex().to_string();
        let shard_path = shard_dir.join(format!("{shard_id}.{SNAPSHOT_FILE_EXTENSION}"));
        let merged_sidecar_path = shard_dir.join(format!("{shard_id}.{SNAPSHOT_MERGED_EXTENSION}"));

        if shard_path.exists() {
            return MergeEntryOutcome::from_result(MergeResult::IdempotentNoop);
        }

        if let Err(err) = atomic_write(&shard_path, &encoded) {
            eprintln!(
                "warning: failed to write snapshot shard {}: {err}; skipping shared snapshot write",
                shard_path.display()
            );
            return MergeEntryOutcome::from_result(MergeResult::SkippedLockUnavailable);
        }

        let subsumed_shard_ids = visible_shards
            .iter()
            .filter_map(SnapshotShard::deletable_shard_id)
            .collect::<Vec<_>>();

        let merged_bytes = encode_merged_sidecar(&subsumed_shard_ids).into_bytes();
        if let Err(err) = atomic_write(&merged_sidecar_path, &merged_bytes) {
            eprintln!(
                "warning: failed to write snapshot merged sidecar {}: {err}; skipping compaction cleanup",
                merged_sidecar_path.display()
            );
            return MergeEntryOutcome {
                result: MergeResult::Inserted,
                new_snapshot_upload: Some(SnapshotUpload {
                    shard_id,
                    shard_bytes: encoded,
                    merged_bytes: Vec::new(),
                }),
                subsumed_shard_ids: Vec::new(),
            };
        }

        for subsumed_shard_id in &subsumed_shard_ids {
            self.delete_shard_files_by_id(commit_key, subsumed_shard_id);
        }

        MergeEntryOutcome {
            result: MergeResult::Inserted,
            new_snapshot_upload: Some(SnapshotUpload {
                shard_id,
                shard_bytes: encoded,
                merged_bytes,
            }),
            subsumed_shard_ids,
        }
    }

    pub fn lookup(&self, commit_key: &str, input_key: &[u8; 32]) -> Option<SnapshotEntry> {
        let snapshot = self.load(commit_key)?;
        snapshot.entries.get(&input_key_hex(*input_key)).cloned()
    }

    fn shard_dir_path(&self, commit_key: &str) -> PathBuf {
        self.paths.snapshots_dir.join(commit_key)
    }

    #[cfg(test)]
    fn legacy_snapshot_path(&self, commit_key: &str) -> PathBuf {
        self.paths
            .snapshots_dir
            .join(format!("{commit_key}.{SNAPSHOT_FILE_EXTENSION}"))
    }

    #[cfg(test)]
    fn merged_sidecar_path(&self, commit_key: &str, shard_id: &str) -> PathBuf {
        self.shard_dir_path(commit_key)
            .join(format!("{shard_id}.{SNAPSHOT_MERGED_EXTENSION}"))
    }

    fn load_merged_snapshot_from_shards(
        &self,
        commit_key: &str,
        shards: Vec<SnapshotShard>,
    ) -> Option<Snapshot> {
        let mut merged = Snapshot::new();
        let mut saw_any = false;

        for shard in shards {
            let bytes = match fs::read(shard.path()) {
                Ok(bytes) => bytes,
                Err(err) => {
                    eprintln!(
                        "warning: failed to read snapshot shard {}: {err}; skipping shard",
                        shard.path().display()
                    );
                    continue;
                }
            };

            let snapshot = match decode_snapshot(&bytes, commit_key) {
                Ok(snapshot) => snapshot,
                Err(err) => {
                    eprintln!(
                        "warning: failed to decode snapshot shard {} for commit {commit_key}: {err}; skipping shard",
                        shard.path().display()
                    );
                    continue;
                }
            };

            saw_any = true;
            merge_shard_entries(&mut merged, snapshot);
        }

        saw_any.then_some(merged)
    }

    fn list_snapshot_shards(&self, commit_key: &str) -> Vec<SnapshotShard> {
        let mut shards = Vec::new();
        let legacy_path = self.snapshot_path(commit_key);
        if legacy_path.is_file() {
            shards.push(SnapshotShard {
                shard_id: format!("legacy-{commit_key}"),
                source: SnapshotShardSource::LegacyFile(legacy_path),
            });
        }

        let shard_dir = self.shard_dir_path(commit_key);
        if let Ok(entries) = fs::read_dir(&shard_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                if path.extension().and_then(|ext| ext.to_str()) != Some(SNAPSHOT_FILE_EXTENSION) {
                    continue;
                }
                let Some(file_stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                    continue;
                };
                shards.push(SnapshotShard {
                    shard_id: file_stem.to_owned(),
                    source: SnapshotShardSource::ShardFile(path),
                });
            }
        }

        shards.sort_unstable_by(|left, right| left.shard_id.cmp(&right.shard_id));
        shards
    }

    fn snapshot_path(&self, commit_key: &str) -> PathBuf {
        self.paths
            .snapshots_dir
            .join(format!("{commit_key}.{SNAPSHOT_FILE_EXTENSION}"))
    }

    fn delete_shard_files_by_id(&self, commit_key: &str, shard_id: &str) {
        for path in [
            self.shard_dir_path(commit_key)
                .join(format!("{shard_id}.{SNAPSHOT_FILE_EXTENSION}")),
            self.shard_dir_path(commit_key)
                .join(format!("{shard_id}.{SNAPSHOT_MERGED_EXTENSION}")),
        ] {
            if let Err(err) = remove_file_if_exists(&path) {
                eprintln!(
                    "warning: failed to delete snapshot compaction file {}: {err}",
                    path.display()
                );
            }
        }
    }
}

impl SnapshotShard {
    fn path(&self) -> &Path {
        match &self.source {
            SnapshotShardSource::LegacyFile(path) | SnapshotShardSource::ShardFile(path) => path,
        }
    }

    fn deletable_shard_id(&self) -> Option<String> {
        match self.source {
            SnapshotShardSource::LegacyFile(_) => None,
            SnapshotShardSource::ShardFile(_) => Some(self.shard_id.clone()),
        }
    }
}

fn merge_shard_entries(merged: &mut Snapshot, shard: Snapshot) {
    for (entry_key, entry) in shard.entries {
        match merged.entries.get(&entry_key) {
            None => {
                merged.entries.insert(entry_key, entry);
            }
            Some(existing) if existing.outputs_hash == entry.outputs_hash => {}
            Some(_) => {}
        }
    }
}

fn encode_merged_sidecar(shard_ids: &[String]) -> String {
    if shard_ids.is_empty() {
        String::new()
    } else {
        format!("{}\n", shard_ids.join("\n"))
    }
}

fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
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
    fn combined_dep_outputs_hash_is_order_stable() {
        let mut map = BTreeMap::new();
        map.insert("pkg-a#build".to_owned(), [1; 32]);
        map.insert("pkg-b#build".to_owned(), [2; 32]);

        let first = combined_dep_outputs_hash(&map);
        let second = combined_dep_outputs_hash(&map);
        assert_eq!(first, second);

        let mut reversed = BTreeMap::new();
        reversed.insert("pkg-b#build".to_owned(), [2; 32]);
        reversed.insert("pkg-a#build".to_owned(), [1; 32]);
        assert_eq!(first, combined_dep_outputs_hash(&reversed));
    }

    #[test]
    fn combined_dep_outputs_hash_changes_when_any_dependency_changes() {
        let mut base = BTreeMap::new();
        base.insert("pkg-a#build".to_owned(), [1; 32]);

        let mut changed_hash = base.clone();
        changed_hash.insert("pkg-a#build".to_owned(), [9; 32]);

        let mut changed_task = BTreeMap::new();
        changed_task.insert("pkg-b#build".to_owned(), [1; 32]);

        assert_ne!(
            combined_dep_outputs_hash(&base),
            combined_dep_outputs_hash(&changed_hash)
        );
        assert_ne!(
            combined_dep_outputs_hash(&base),
            combined_dep_outputs_hash(&changed_task)
        );
    }

    #[test]
    fn snapshot_round_trip_serialization() {
        let snapshot = sample_snapshot();
        let encoded = bincode::serde::encode_to_vec(&snapshot, bincode_config()).unwrap();
        let decoded = decode_snapshot(&encoded, "commit").unwrap();
        assert_eq!(decoded, snapshot);
    }

    #[test]
    fn snapshot_decode_rejects_schema_mismatch() {
        let mut snapshot = sample_snapshot();
        snapshot.schema_version = SNAPSHOT_SCHEMA_VERSION + 1;
        let encoded = bincode::serde::encode_to_vec(&snapshot, bincode_config()).unwrap();
        let err = decode_snapshot(&encoded, "commit").unwrap_err();
        assert!(err
            .to_string()
            .contains("unsupported snapshot schema version"));
    }

    #[test]
    fn snapshot_store_writes_and_reads_entry() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let store = SnapshotStore::new(paths);
        let entry = sample_entry_with_seed(1, [5; 32]);

        assert_eq!(
            store.merge_entry("commit-a", entry.clone()),
            MergeResult::Inserted
        );
        assert_eq!(store.lookup("commit-a", &entry.input_key), Some(entry));
        assert_eq!(store.lookup("commit-a", &[99; 32]), None);
    }

    #[test]
    fn snapshot_store_idempotent_when_outputs_match() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let store = SnapshotStore::new(paths);
        let entry = sample_entry_with_seed(2, [6; 32]);

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
    }

    #[test]
    fn snapshot_store_conflict_keeps_existing_entry() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let store = SnapshotStore::new(paths);
        let original = sample_entry_with_seed(3, [7; 32]);
        let mut conflicting = original.clone();
        conflicting.outputs_hash = [8; 32];

        assert_eq!(
            store.merge_entry("commit-c", original.clone()),
            MergeResult::Inserted
        );
        assert_eq!(
            store.merge_entry("commit-c", conflicting),
            MergeResult::ConflictKeptExisting
        );

        let snapshot = store.load("commit-c").unwrap();
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(
            snapshot.entries.get(&input_key_hex(original.input_key)),
            Some(&original)
        );
    }

    #[test]
    fn snapshot_store_handles_concurrent_appends_without_losing_entries() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path());
        let store = Arc::new(SnapshotStore::new(paths.unwrap()));
        let commit_key = "commit-concurrent";

        let mut handles = Vec::new();
        for seed in 4..12 {
            let store = Arc::clone(&store);
            let commit_key = commit_key.to_owned();
            handles.push(thread::spawn(move || {
                let entry = sample_entry_with_seed(seed, [seed; 32]);
                assert_eq!(store.merge_entry(&commit_key, entry), MergeResult::Inserted);
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let snapshot = store.load(commit_key).unwrap();
        assert_eq!(snapshot.entries.len(), 8);
        for seed in 4..12 {
            let entry = sample_entry_with_seed(seed, [seed; 32]);
            assert_eq!(
                snapshot.entries.get(&input_key_hex(entry.input_key)),
                Some(&entry)
            );
        }
    }

    #[test]
    fn snapshot_store_shard_name_matches_content_hash() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let store = SnapshotStore::new(paths.clone());
        let commit_key = "commit-hash";
        let entry = sample_entry_with_seed(9, [9; 32]);

        assert_eq!(
            store.merge_entry(commit_key, entry.clone()),
            MergeResult::Inserted
        );

        let shard_dir = store.shard_dir_path(commit_key);
        let shard_paths = collect_bincode_files(&shard_dir);
        assert_eq!(shard_paths.len(), 1);

        let bytes = fs::read(&shard_paths[0]).unwrap();
        let expected_name = format!(
            "{}.{}",
            blake3::hash(&bytes).to_hex(),
            SNAPSHOT_FILE_EXTENSION
        );
        assert_eq!(
            shard_paths[0].file_name().unwrap().to_string_lossy(),
            expected_name
        );

        let snapshot = decode_snapshot(&bytes, commit_key).unwrap();
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(
            snapshot.entries.get(&input_key_hex(entry.input_key)),
            Some(&entry)
        );
    }

    #[test]
    fn snapshot_store_compacts_seeded_shards_and_records_subsumed_ids() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let store = SnapshotStore::new(paths.clone());
        let commit_key = "commit-compact";
        let seeded = [
            sample_entry_with_seed(12, [1; 32]),
            sample_entry_with_seed(13, [2; 32]),
            sample_entry_with_seed(14, [3; 32]),
        ];

        for entry in &seeded {
            let snapshot = snapshot_with_entries([entry.clone()]);
            let bytes = bincode::serde::encode_to_vec(&snapshot, bincode_config()).unwrap();
            let shard_id = blake3::hash(&bytes).to_hex().to_string();
            write_snapshot_file(
                &store
                    .shard_dir_path(commit_key)
                    .join(format!("{shard_id}.{SNAPSHOT_FILE_EXTENSION}")),
                snapshot,
            );
        }

        let shard_ids_before = collect_shard_ids(&store.shard_dir_path(commit_key));
        assert_eq!(shard_ids_before.len(), 3);

        let new_entry = sample_entry_with_seed(15, [4; 32]);
        assert_eq!(
            store.merge_entry(commit_key, new_entry.clone()),
            MergeResult::Inserted
        );

        let shard_ids_after = collect_shard_ids(&store.shard_dir_path(commit_key));
        assert_eq!(shard_ids_after.len(), 1);
        let merged_sidecar =
            fs::read_to_string(store.merged_sidecar_path(commit_key, &shard_ids_after[0])).unwrap();
        assert_eq!(
            merged_sidecar.lines().collect::<Vec<_>>(),
            shard_ids_before
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
        );

        let snapshot = store.load(commit_key).unwrap();
        assert_eq!(snapshot.entries.len(), 4);
        for entry in seeded.into_iter().chain(std::iter::once(new_entry)) {
            assert_eq!(
                snapshot.entries.get(&input_key_hex(entry.input_key)),
                Some(&entry)
            );
        }
    }

    #[test]
    fn snapshot_store_keeps_unseen_shard_added_after_capture() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let store = SnapshotStore::new(paths.clone());
        let commit_key = "commit-unseen";
        let seen_entry = sample_entry_with_seed(16, [5; 32]);
        let unseen_entry = sample_entry_with_seed(17, [6; 32]);
        let new_entry = sample_entry_with_seed(18, [7; 32]);

        let seen_snapshot = snapshot_with_entries([seen_entry.clone()]);
        let seen_bytes = bincode::serde::encode_to_vec(&seen_snapshot, bincode_config()).unwrap();
        let seen_shard_id = blake3::hash(&seen_bytes).to_hex().to_string();
        write_snapshot_file(
            &store
                .shard_dir_path(commit_key)
                .join(format!("{seen_shard_id}.{SNAPSHOT_FILE_EXTENSION}")),
            seen_snapshot,
        );

        let visible_shards = store.list_snapshot_shards(commit_key);
        let mut consolidated = store
            .load_merged_snapshot_from_shards(commit_key, visible_shards.clone())
            .unwrap();
        consolidated
            .entries
            .insert(input_key_hex(new_entry.input_key), new_entry.clone());
        let consolidated_bytes =
            bincode::serde::encode_to_vec(&consolidated, bincode_config()).unwrap();
        let consolidated_id = blake3::hash(&consolidated_bytes).to_hex().to_string();
        let consolidated_path = store
            .shard_dir_path(commit_key)
            .join(format!("{consolidated_id}.{SNAPSHOT_FILE_EXTENSION}"));
        let consolidated_sidecar = store.merged_sidecar_path(commit_key, &consolidated_id);

        atomic_write(&consolidated_path, &consolidated_bytes).unwrap();

        let unseen_snapshot = snapshot_with_entries([unseen_entry.clone()]);
        let unseen_bytes =
            bincode::serde::encode_to_vec(&unseen_snapshot, bincode_config()).unwrap();
        let unseen_shard_id = blake3::hash(&unseen_bytes).to_hex().to_string();
        write_snapshot_file(
            &store
                .shard_dir_path(commit_key)
                .join(format!("{unseen_shard_id}.{SNAPSHOT_FILE_EXTENSION}")),
            unseen_snapshot,
        );

        let subsumed_shard_ids = visible_shards
            .iter()
            .filter_map(SnapshotShard::deletable_shard_id)
            .collect::<Vec<_>>();
        atomic_write(
            &consolidated_sidecar,
            encode_merged_sidecar(&subsumed_shard_ids).as_bytes(),
        )
        .unwrap();
        for shard_id in subsumed_shard_ids {
            store.delete_shard_files_by_id(commit_key, &shard_id);
        }

        let shard_ids_after = collect_shard_ids(&store.shard_dir_path(commit_key));
        let mut expected_shard_ids = vec![consolidated_id.clone(), unseen_shard_id.clone()];
        expected_shard_ids.sort();
        assert_eq!(shard_ids_after, expected_shard_ids);

        let restored = store.load(commit_key).unwrap();
        assert_eq!(restored.entries.len(), 3);
        for entry in [seen_entry, unseen_entry, new_entry] {
            assert_eq!(
                restored.entries.get(&input_key_hex(entry.input_key)),
                Some(&entry)
            );
        }
    }

    #[test]
    fn snapshot_store_delete_missing_subsumed_shard_is_noop() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let store = SnapshotStore::new(paths.clone());
        let commit_key = "commit-delete-noop";
        let entry = sample_entry_with_seed(19, [8; 32]);
        let snapshot = snapshot_with_entries([entry.clone()]);
        let bytes = bincode::serde::encode_to_vec(&snapshot, bincode_config()).unwrap();
        let shard_id = blake3::hash(&bytes).to_hex().to_string();
        let shard_path = store
            .shard_dir_path(commit_key)
            .join(format!("{shard_id}.{SNAPSHOT_FILE_EXTENSION}"));
        write_snapshot_file(&shard_path, snapshot);
        atomic_write(
            &store.merged_sidecar_path(commit_key, &shard_id),
            b"some-old-sidecar\n",
        )
        .unwrap();

        fs::remove_file(&shard_path).unwrap();
        store.delete_shard_files_by_id(commit_key, &shard_id);
        store.delete_shard_files_by_id(commit_key, &shard_id);

        assert!(!store.merged_sidecar_path(commit_key, &shard_id).exists());
    }

    #[test]
    fn snapshot_store_load_merges_legacy_file_alongside_shards() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let store = SnapshotStore::new(paths.clone());
        let commit_key = "commit-legacy";
        let legacy_entry = sample_entry_with_seed(14, [3; 32]);
        let shard_entry = sample_entry_with_seed(15, [4; 32]);

        write_snapshot_file(
            &store.legacy_snapshot_path(commit_key),
            snapshot_with_entries([legacy_entry.clone()]),
        );
        assert_eq!(
            store.merge_entry(commit_key, shard_entry.clone()),
            MergeResult::Inserted
        );

        let snapshot = store.load(commit_key).unwrap();
        assert_eq!(snapshot.entries.len(), 2);
        assert_eq!(
            snapshot.entries.get(&input_key_hex(legacy_entry.input_key)),
            Some(&legacy_entry)
        );
        assert_eq!(
            snapshot.entries.get(&input_key_hex(shard_entry.input_key)),
            Some(&shard_entry)
        );
        assert!(store.legacy_snapshot_path(commit_key).exists());
    }

    #[test]
    fn snapshot_store_load_skips_corrupt_shards() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let store = SnapshotStore::new(paths.clone());
        let commit_key = "commit-corrupt";
        let valid_entry = sample_entry_with_seed(20, [5; 32]);

        assert_eq!(
            store.merge_entry(commit_key, valid_entry.clone()),
            MergeResult::Inserted
        );

        let corrupt_path = store
            .shard_dir_path(commit_key)
            .join(format!("{}.{}", "0000badshard", SNAPSHOT_FILE_EXTENSION));
        fs::write(&corrupt_path, b"not-bincode").unwrap();

        let snapshot = store.load(commit_key).unwrap();
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(
            snapshot.entries.get(&input_key_hex(valid_entry.input_key)),
            Some(&valid_entry)
        );
    }

    #[test]
    fn snapshot_store_load_conflicting_legacy_file_and_shard_dir_prefers_first_shard_id() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let store = SnapshotStore::new(paths.clone());
        let commit_key = "commit-legacy-conflict";
        let original = sample_entry_with_seed(22, [9; 32]);
        let mut conflicting = original.clone();
        conflicting.outputs_hash = [10; 32];
        conflicting.cached_at_unix_ms += 1;

        write_snapshot_file(
            &store.legacy_snapshot_path(commit_key),
            snapshot_with_entries([conflicting]),
        );
        let shard_path = store
            .shard_dir_path(commit_key)
            .join(format!("{}.{}", "000-first", SNAPSHOT_FILE_EXTENSION));
        write_snapshot_file(&shard_path, snapshot_with_entries([original.clone()]));

        let snapshot = store.load(commit_key).unwrap();
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(
            snapshot.entries.get(&input_key_hex(original.input_key)),
            Some(&original)
        );
    }

    #[test]
    fn snapshot_store_load_conflict_resolution_first_shard_id_wins() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let store = SnapshotStore::new(paths.clone());
        let commit_key = "commit-conflict";
        let original = sample_entry_with_seed(21, [6; 32]);
        let mut conflicting = original.clone();
        conflicting.outputs_hash = [7; 32];
        conflicting.cached_at_unix_ms += 10;

        let low_id_path = store
            .shard_dir_path(commit_key)
            .join(format!("{}.{}", "000-first", SNAPSHOT_FILE_EXTENSION));
        let high_id_path = store
            .shard_dir_path(commit_key)
            .join(format!("{}.{}", "zzz-last", SNAPSHOT_FILE_EXTENSION));
        write_snapshot_file(&high_id_path, snapshot_with_entries([conflicting]));
        write_snapshot_file(&low_id_path, snapshot_with_entries([original.clone()]));

        let snapshot = store.load(commit_key).unwrap();
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(
            snapshot.entries.get(&input_key_hex(original.input_key)),
            Some(&original)
        );
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

    fn snapshot_with_entries(entries: impl IntoIterator<Item = SnapshotEntry>) -> Snapshot {
        let mut snapshot = Snapshot::new();
        for entry in entries {
            snapshot
                .entries
                .insert(input_key_hex(entry.input_key), entry);
        }
        snapshot
    }

    fn write_snapshot_file(path: &Path, snapshot: Snapshot) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bytes = bincode::serde::encode_to_vec(&snapshot, bincode_config()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn collect_bincode_files(dir: &Path) -> Vec<PathBuf> {
        let mut files = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension().and_then(|ext| ext.to_str()) == Some(SNAPSHOT_FILE_EXTENSION)
            })
            .collect::<Vec<_>>();
        files.sort();
        files
    }

    fn collect_shard_ids(dir: &Path) -> Vec<String> {
        let mut shard_ids = collect_bincode_files(dir)
            .into_iter()
            .filter_map(|path| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(ToOwned::to_owned)
            })
            .collect::<Vec<_>>();
        shard_ids.sort();
        shard_ids
    }
}
