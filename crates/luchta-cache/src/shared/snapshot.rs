use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::shared::{atomic_write, EntryMeta, SharedCachePaths};

pub const SNAPSHOT_SCHEMA_VERSION: u32 = 3;
const SNAPSHOT_SCHEMA_VERSION_V2: u32 = 2;
const SNAPSHOT_ZSTD_LEVEL: i32 = 3;
const ZSTD_FRAME_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
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
    /// Complete restore metadata when its compressed entry representation fits
    /// the shared-cache inline budget. `None` uses `entries/<input_key>.bin`.
    pub inline_meta: Option<EntryMeta>,
    /// Whether `duration_ms` measures execution after semaphore admission.
    /// Schema-v2 durations included queueing time and are converted as untrusted.
    pub duration_trusted: bool,
}

/// Exact schema-v2 wire representation. Keep this separate from the current
/// structs so compatibility does not depend on serde field evolution.
#[derive(Debug, Serialize, Deserialize)]
struct SnapshotV2 {
    schema_version: u32,
    entries: BTreeMap<String, SnapshotEntryV2>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SnapshotEntryV2 {
    task_id: String,
    input_key: [u8; 32],
    outputs_hash: [u8; 32],
    task_spec_hash: [u8; 32],
    env_hash: [u8; 32],
    pkg_dep_hash: [u8; 32],
    duration_ms: u64,
    output_bytes: u64,
    cached_at_unix_ms: u64,
    tool_version: Option<String>,
}

impl From<SnapshotV2> for Snapshot {
    fn from(snapshot: SnapshotV2) -> Self {
        debug_assert_eq!(snapshot.schema_version, SNAPSHOT_SCHEMA_VERSION_V2);
        Self {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            entries: snapshot
                .entries
                .into_iter()
                .map(|(key, entry)| {
                    (
                        key,
                        SnapshotEntry {
                            task_id: entry.task_id,
                            input_key: entry.input_key,
                            outputs_hash: entry.outputs_hash,
                            task_spec_hash: entry.task_spec_hash,
                            env_hash: entry.env_hash,
                            pkg_dep_hash: entry.pkg_dep_hash,
                            duration_ms: entry.duration_ms,
                            output_bytes: entry.output_bytes,
                            cached_at_unix_ms: entry.cached_at_unix_ms,
                            tool_version: entry.tool_version,
                            inline_meta: None,
                            duration_trusted: false,
                        },
                    )
                })
                .collect(),
        }
    }
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeEntryOutcome {
    pub result: MergeResult,
    pub new_snapshot_upload: Option<SnapshotUpload>,
    pub subsumed_shard_ids: Vec<String>,
}

/// What a bucket's shard files merged into, plus the ids of the shards that
/// actually contributed.
///
/// The id list is what makes deleting subsumed shards safe. A shard that
/// failed to decode — corrupt, or written by a client running a newer
/// `SNAPSHOT_SCHEMA_VERSION` — is skipped by the merge, so it holds entries
/// the consolidated shard does not. Subsuming it anyway destroys cache
/// entries this client merely couldn't read, and since the same id list
/// drives `push_index_merge`'s remote deletes, an older client would wipe a
/// newer client's shard off the object store too.
#[derive(Debug)]
struct MergedShards {
    snapshot: Option<Snapshot>,
    merged_shard_ids: HashSet<String>,
}

#[derive(Debug)]
struct ConsolidatedShardWrite {
    shard_id: String,
    shard_on_disk: Vec<u8>,
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

    pub fn load(&self, shard_key: &str) -> Option<Snapshot> {
        let shards = self.list_snapshot_shards(shard_key);
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

        self.load_merged_snapshot_from_shards(shard_key, shards)
            .snapshot
    }

    /// Single-entry convenience wrapper over `merge_entries_with_outcome`,
    /// kept for tests. No production caller left since the store-side merge
    /// moved into `SharedCache::flush_pending_entries`, which always batches.
    pub fn merge_entry(&self, shard_key: &str, entry: SnapshotEntry) -> MergeResult {
        self.merge_entry_with_outcome(shard_key, entry).result
    }

    /// Single-entry convenience wrapper over `merge_entries_with_outcome`,
    /// kept for tests — see `merge_entry`.
    pub fn merge_entry_with_outcome(
        &self,
        shard_key: &str,
        entry: SnapshotEntry,
    ) -> MergeEntryOutcome {
        self.merge_entries_with_outcome(shard_key, vec![entry])
    }

    /// Merges any number of entries into `shard_key` in a single
    /// load-modify-write cycle: one shard load, and (only if something
    /// actually changed) one consolidated write producing at most one
    /// `new_snapshot_upload` — regardless of how many entries are in
    /// `entries`.
    ///
    /// `merge_entry_with_outcome` is the `entries.len() == 1` case of this,
    /// not a separate code path.
    ///
    /// Used by `SharedCache::flush_pending_entries` to collapse a run's worth
    /// of stores and cache-hit refreshes into a single remote push instead of
    /// one push per store or hit: pushing on every store/hit saturates the
    /// rclone daemon on exactly the runs where the cache is doing the most
    /// work, tripping the timeout-disable circuit breaker and defeating the
    /// feature under its own success.
    pub fn merge_entries_with_outcome(
        &self,
        shard_key: &str,
        entries: Vec<SnapshotEntry>,
    ) -> MergeEntryOutcome {
        if entries.is_empty() {
            return MergeEntryOutcome::from_result(MergeResult::IdempotentNoop);
        }

        let shard_dir = self.shard_dir_path(shard_key);
        if let Err(err) = crate::shared::ensure_cache_dir(&shard_dir) {
            eprintln!(
                "warning: failed to create snapshot shard dir {}: {err}; skipping shared snapshot write",
                shard_dir.display()
            );
            return MergeEntryOutcome::from_result(MergeResult::SkippedLockUnavailable);
        }

        let visible_shards = self.list_snapshot_shards(shard_key);
        let MergedShards {
            snapshot,
            merged_shard_ids,
        } = self.load_merged_snapshot_from_shards(shard_key, visible_shards.clone());
        let mut consolidated = snapshot.unwrap_or_default();

        let MergeChanges {
            changed,
            saw_conflict,
        } = merge_snapshot_entries(&mut consolidated, entries);

        if !changed {
            // Preserve the single-entry distinction exactly: a lone
            // conflicting entry must still report `ConflictKeptExisting`
            // (see `snapshot_store_conflict_keeps_existing_entry`), not
            // collapse into the more generic "nothing changed" outcome.
            let result = if saw_conflict {
                MergeResult::ConflictKeptExisting
            } else {
                MergeResult::IdempotentNoop
            };
            return MergeEntryOutcome::from_result(result);
        }

        self.write_consolidated_shard(shard_key, &consolidated, &visible_shards, &merged_shard_ids)
    }

    /// Writes the consolidated shard + `.merged` sidecar and deletes the shards
    /// it subsumes. Returns the merge outcome (new shard id + subsumed ids).
    fn write_consolidated_shard(
        &self,
        shard_key: &str,
        consolidated: &Snapshot,
        visible_shards: &[SnapshotShard],
        merged_shard_ids: &HashSet<String>,
    ) -> MergeEntryOutcome {
        let shard_dir = self.shard_dir_path(shard_key);
        let encoded = bincode::serde::encode_to_vec(consolidated, snapshot_bincode_config())
            .expect("snapshot serialization should succeed");
        let shard_id = blake3::hash(&encoded).to_hex().to_string();
        let shard_path = shard_dir.join(format!("{shard_id}.{SNAPSHOT_FILE_EXTENSION}"));
        let write = ConsolidatedShardWrite {
            shard_id,
            shard_on_disk: Vec::new(),
        };

        if shard_path.exists() {
            return MergeEntryOutcome::from_result(MergeResult::IdempotentNoop);
        }

        let on_disk = match compress_snapshot_bytes(&encoded) {
            Ok(on_disk) => on_disk,
            Err(err) => {
                eprintln!(
                    "warning: failed to compress snapshot shard {}: {err}; skipping shared snapshot write",
                    shard_path.display()
                );
                return MergeEntryOutcome::from_result(MergeResult::SkippedLockUnavailable);
            }
        };
        let write = ConsolidatedShardWrite {
            shard_on_disk: on_disk,
            ..write
        };

        if let Err(err) = atomic_write(&shard_path, &write.shard_on_disk) {
            eprintln!(
                "warning: failed to write snapshot shard {}: {err}; skipping shared snapshot write",
                shard_path.display()
            );
            return MergeEntryOutcome::from_result(MergeResult::SkippedLockUnavailable);
        }

        self.finalize_sidecar_and_subsumed(shard_key, write, visible_shards, merged_shard_ids)
    }

    fn finalize_sidecar_and_subsumed(
        &self,
        shard_key: &str,
        write: ConsolidatedShardWrite,
        visible_shards: &[SnapshotShard],
        merged_shard_ids: &HashSet<String>,
    ) -> MergeEntryOutcome {
        let ConsolidatedShardWrite {
            shard_id,
            shard_on_disk,
        } = write;
        // Subsume only the shards that actually merged. One this client
        // couldn't decode still holds entries the consolidated shard is
        // missing, so deleting it would drop cache entries -- and for a
        // future-schema shard, they'd be a newer client's entries, deleted
        // both locally and (via `push_index_merge`) on the shared remote.
        // Leaving it costs a warning per load until GC ages it out.
        let subsumed_shard_ids = visible_shards
            .iter()
            .filter(|shard| merged_shard_ids.contains(&shard.shard_id))
            .filter_map(SnapshotShard::deletable_shard_id)
            .collect::<Vec<_>>();
        // The consolidated shard is already on disk at this point, so the
        // shards it subsumes are redundant and safe to drop. This used to be
        // gated on first writing a `.merged` sidecar listing them -- a
        // journal for a recovery pass that was never written (#284).
        for subsumed_shard_id in &subsumed_shard_ids {
            self.delete_shard_files_by_id(shard_key, subsumed_shard_id);
        }

        MergeEntryOutcome {
            result: MergeResult::Inserted,
            new_snapshot_upload: Some(SnapshotUpload {
                shard_id,
                shard_bytes: shard_on_disk,
            }),
            subsumed_shard_ids,
        }
    }

    pub fn lookup(&self, shard_key: &str, input_key: &[u8; 32]) -> Option<SnapshotEntry> {
        let snapshot = self.load(shard_key)?;
        snapshot.entries.get(&input_key_hex(*input_key)).cloned()
    }

    fn shard_dir_path(&self, shard_key: &str) -> PathBuf {
        self.paths.snapshots_dir.join(shard_key)
    }

    #[cfg(test)]
    fn legacy_snapshot_path(&self, shard_key: &str) -> PathBuf {
        self.paths
            .snapshots_dir
            .join(format!("{shard_key}.{SNAPSHOT_FILE_EXTENSION}"))
    }

    #[cfg(test)]
    fn merged_sidecar_path(&self, shard_key: &str, shard_id: &str) -> PathBuf {
        self.shard_dir_path(shard_key)
            .join(format!("{shard_id}.{SNAPSHOT_MERGED_EXTENSION}"))
    }

    fn load_merged_snapshot_from_shards(
        &self,
        shard_key: &str,
        shards: Vec<SnapshotShard>,
    ) -> MergedShards {
        let mut merged = Snapshot::new();
        let mut saw_any = false;
        let mut merged_shard_ids = HashSet::new();

        for shard in shards {
            let bytes = match fs::read(shard.path()) {
                Ok(bytes) => bytes,
                // A missing shard is an expected, benign condition (e.g. the
                // shard was pruned or never synced from a remote cache), so
                // skip it silently instead of emitting a warning.
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => {
                    eprintln!(
                        "warning: failed to read snapshot shard {}: {err}; skipping shard",
                        shard.path().display()
                    );
                    continue;
                }
            };

            let snapshot = match decode_snapshot(&bytes, shard_key) {
                Ok(snapshot) => snapshot,
                Err(err) => {
                    eprintln!(
                        "warning: failed to decode snapshot shard {} for bucket {shard_key}: {err}; skipping shard",
                        shard.path().display()
                    );
                    continue;
                }
            };

            saw_any = true;
            merged_shard_ids.insert(shard.shard_id.clone());
            merge_shard_entries(&mut merged, snapshot);
        }

        MergedShards {
            snapshot: saw_any.then_some(merged),
            merged_shard_ids,
        }
    }

    fn list_snapshot_shards(&self, shard_key: &str) -> Vec<SnapshotShard> {
        let mut shards = Vec::new();
        let legacy_path = self.snapshot_path(shard_key);
        if legacy_path.is_file() {
            shards.push(SnapshotShard {
                shard_id: format!("legacy-{shard_key}"),
                source: SnapshotShardSource::LegacyFile(legacy_path),
            });
        }

        let shard_dir = self.shard_dir_path(shard_key);
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

    fn snapshot_path(&self, shard_key: &str) -> PathBuf {
        self.paths
            .snapshots_dir
            .join(format!("{shard_key}.{SNAPSHOT_FILE_EXTENSION}"))
    }

    fn delete_shard_files_by_id(&self, shard_key: &str, shard_id: &str) {
        for path in [
            self.shard_dir_path(shard_key)
                .join(format!("{shard_id}.{SNAPSHOT_FILE_EXTENSION}")),
            self.shard_dir_path(shard_key)
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
        match merged.entries.get_mut(&entry_key) {
            None => {
                merged.entries.insert(entry_key, entry);
            }
            Some(existing) if existing.outputs_hash == entry.outputs_hash => {
                enrich_same_output_entry(existing, entry);
            }
            Some(_) => {}
        }
    }
}

struct MergeChanges {
    changed: bool,
    saw_conflict: bool,
}

fn merge_snapshot_entries(snapshot: &mut Snapshot, entries: Vec<SnapshotEntry>) -> MergeChanges {
    let mut result = MergeChanges {
        changed: false,
        saw_conflict: false,
    };
    for entry in entries {
        let entry_key = input_key_hex(entry.input_key);
        match snapshot.entries.get_mut(&entry_key) {
            Some(existing) if existing.outputs_hash == entry.outputs_hash => {
                result.changed |= enrich_same_output_entry(existing, entry);
            }
            // The input key includes resolved source content. A differing
            // output therefore indicates a nondeterministic build, for which
            // retaining the first writer is the safe result.
            Some(_) => result.saw_conflict = true,
            None => {
                snapshot.entries.insert(entry_key, entry);
                result.changed = true;
            }
        }
    }
    result
}

/// Same-output schema-v3 data may enrich a schema-v2 observation without
/// changing first-writer-wins conflict semantics.
pub(crate) fn enrich_same_output_entry(
    existing: &mut SnapshotEntry,
    mut incoming: SnapshotEntry,
) -> bool {
    let mut changed = false;
    if !existing.duration_trusted && incoming.duration_trusted {
        let existing_inline_meta = existing.inline_meta.take();
        *existing = incoming;
        if existing.inline_meta.is_none() {
            existing.inline_meta = existing_inline_meta;
        }
        changed = true;
    } else if existing.inline_meta.is_none() && incoming.inline_meta.is_some() {
        existing.inline_meta = incoming.inline_meta.take();
        changed = true;
    }
    changed
}

fn compress_snapshot_bytes(raw: &[u8]) -> io::Result<Vec<u8>> {
    zstd::encode_all(raw, SNAPSHOT_ZSTD_LEVEL)
}

fn decompress_snapshot_bytes(bytes: &[u8]) -> io::Result<Vec<u8>> {
    if bytes.starts_with(&ZSTD_FRAME_MAGIC) {
        zstd::decode_all(bytes)
    } else {
        Ok(bytes.to_vec())
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

/// Derives the shared-cache entry key.
///
/// `inputs_hash` covers the package's own resolved source content
/// (`combined_inputs_hash` over the same `FileEntry` list `files_changed`
/// compares in `decide.rs`). Folding it into the key means two branches that
/// change a package differently land in distinct slots instead of racing for
/// one first-writer-wins slot keyed only by the task's *definition* — see
/// `decide_shared_restore`'s doc comment for why the separate inputs
/// comparison there is now a safety net rather than the discriminator.
///
/// An inputs hash is only meaningful when the pattern set it was computed
/// over came from the task definition, not from a previously stored record —
/// otherwise the record would need to be fetched to know which patterns to
/// hash, which is circular. Every caller must resolve inputs against
/// `detected_input_patterns: false` (see `TaskRunRecord::detected_input_patterns`
/// and `assemble_run_record` in `luchta-cli`); enabling worker-detected input
/// patterns requires reworking this key derivation first.
///
/// This key assumes the task is a pure function of `task_spec_hash`,
/// `env_hash`, `pkg_dep_hash`, `dep_outputs_hash`, and `inputs_hash`. Neither
/// the task id nor any package identity is folded in, and `inputs_hash`
/// (`combined_inputs_hash`) hashes package-relative paths, not absolute
/// ones. So two different packages running the same task with identical
/// declared deps and byte-identical declared inputs compute the same key and
/// share one shared-cache entry. That's correct for a task whose output
/// depends only on its declared inputs and spec. It's wrong for a task whose
/// behaviour also depends on something outside its declared inputs — the
/// package name or directory, an unpattern-matched `package.json` field, a
/// generated artifact that embeds its own path — and such a task will read
/// back another package's cached result. If you're adding a task type,
/// checking that its declared inputs fully determine its output is the
/// caller's job; this function has no way to catch the omission.
#[must_use]
pub fn derive_input_key(
    task_spec_hash: [u8; 32],
    env_hash: [u8; 32],
    pkg_dep_hash: [u8; 32],
    dep_outputs_hash: [u8; 32],
    inputs_hash: [u8; 32],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&task_spec_hash);
    hasher.update(&env_hash);
    hasher.update(&pkg_dep_hash);
    hasher.update(&dep_outputs_hash);
    hasher.update(&inputs_hash);
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

/// Bincode configuration for on-disk snapshot shards.
///
/// Snapshots intentionally use the default variable-length integer encoding —
/// distinct from the fixed-int cache-record config in `crate::serialization` —
/// to preserve the existing on-disk snapshot format. Exposed to sibling test
/// modules so snapshot fixtures serialize with the same encoding
/// `Snapshot::load` expects.
pub(crate) fn snapshot_bincode_config() -> impl bincode::config::Config {
    bincode::config::standard()
}

fn decode_snapshot(
    bytes: &[u8],
    _shard_key: &str,
) -> Result<Snapshot, bincode::error::DecodeError> {
    let raw = decompress_snapshot_bytes(bytes)
        .map_err(|err| bincode::error::DecodeError::OtherString(err.to_string()))?;
    let (schema_version, _): (u32, usize) =
        bincode::serde::decode_from_slice(&raw, snapshot_bincode_config())?;
    match schema_version {
        SNAPSHOT_SCHEMA_VERSION_V2 => {
            let (snapshot, _): (SnapshotV2, usize) =
                bincode::serde::decode_from_slice(&raw, snapshot_bincode_config())?;
            Ok(snapshot.into())
        }
        SNAPSHOT_SCHEMA_VERSION => {
            let (snapshot, _): (Snapshot, usize) =
                bincode::serde::decode_from_slice(&raw, snapshot_bincode_config())?;
            Ok(snapshot)
        }
        _ => Err(bincode::error::DecodeError::OtherString(
            "unsupported snapshot schema version".to_owned(),
        )),
    }
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
        let base = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
        let changed_components = [
            derive_input_key([9; 32], [2; 32], [3; 32], [4; 32], [5; 32]),
            derive_input_key([1; 32], [9; 32], [3; 32], [4; 32], [5; 32]),
            derive_input_key([1; 32], [2; 32], [9; 32], [4; 32], [5; 32]),
            derive_input_key([1; 32], [2; 32], [3; 32], [9; 32], [5; 32]),
            derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [9; 32]),
        ];

        for changed in changed_components {
            assert_ne!(base, changed, "every component must be folded into the key");
        }
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
    fn snapshot_compression_round_trip_and_raw_passthrough() {
        let raw = b"snapshot bytes without zstd header";
        let compressed = compress_snapshot_bytes(raw).unwrap();

        assert!(compressed.starts_with(&ZSTD_FRAME_MAGIC));
        assert_eq!(decompress_snapshot_bytes(&compressed).unwrap(), raw);
        assert_eq!(decompress_snapshot_bytes(raw).unwrap(), raw);
    }

    #[test]
    fn snapshot_round_trip_serialization() {
        let snapshot = sample_snapshot();
        let encoded = bincode::serde::encode_to_vec(&snapshot, snapshot_bincode_config()).unwrap();
        let decoded = decode_snapshot(&encoded, "commit").unwrap();
        assert_eq!(decoded, snapshot);
    }

    #[test]
    fn snapshot_v2_decodes_without_inline_meta_and_with_untrusted_duration() {
        let current = sample_entry_with_seed(31, [7; 32]);
        let legacy_entry = SnapshotEntryV2 {
            task_id: current.task_id.clone(),
            input_key: current.input_key,
            outputs_hash: current.outputs_hash,
            task_spec_hash: current.task_spec_hash,
            env_hash: current.env_hash,
            pkg_dep_hash: current.pkg_dep_hash,
            duration_ms: 9_999,
            output_bytes: current.output_bytes,
            cached_at_unix_ms: current.cached_at_unix_ms,
            tool_version: current.tool_version.clone(),
        };
        let legacy = SnapshotV2 {
            schema_version: SNAPSHOT_SCHEMA_VERSION_V2,
            entries: BTreeMap::from([(input_key_hex(current.input_key), legacy_entry)]),
        };
        let encoded = bincode::serde::encode_to_vec(&legacy, snapshot_bincode_config()).unwrap();

        let decoded = decode_snapshot(&encoded, "legacy-commit").unwrap();
        let entry = decoded
            .entries
            .get(&input_key_hex(current.input_key))
            .unwrap();
        assert_eq!(decoded.schema_version, SNAPSHOT_SCHEMA_VERSION);
        assert_eq!(entry.duration_ms, 9_999);
        assert!(!entry.duration_trusted);
        assert!(entry.inline_meta.is_none());
    }

    #[test]
    fn same_output_shards_accumulate_inline_meta_and_trusted_duration() {
        let mut legacy = sample_entry_with_seed(32, [8; 32]);
        legacy.duration_ms = 50_000;
        legacy.duration_trusted = false;

        let mut inline_only = legacy.clone();
        inline_only.inline_meta = Some(sample_inline_meta(inline_only.outputs_hash));

        let mut trusted_only = legacy.clone();
        trusted_only.duration_ms = 250;
        trusted_only.duration_trusted = true;

        let key = input_key_hex(legacy.input_key);
        let mut merged = snapshot_with_entries([legacy]);
        merge_shard_entries(&mut merged, snapshot_with_entries([inline_only]));
        merge_shard_entries(&mut merged, snapshot_with_entries([trusted_only]));

        let enriched = merged.entries.get(&key).unwrap();
        assert!(enriched.inline_meta.is_some());
        assert!(enriched.duration_trusted);
        assert_eq!(enriched.duration_ms, 250);
    }

    #[test]
    fn snapshot_decode_rejects_schema_mismatch() {
        let mut snapshot = sample_snapshot();
        snapshot.schema_version = SNAPSHOT_SCHEMA_VERSION + 1;
        let encoded = bincode::serde::encode_to_vec(&snapshot, snapshot_bincode_config()).unwrap();
        let err = decode_snapshot(&encoded, "commit").unwrap_err();
        assert!(err
            .to_string()
            .contains("unsupported snapshot schema version"));
    }

    #[test]
    fn decode_snapshot_zstd_magic_with_corrupt_payload_is_cache_miss() {
        let mut bytes = ZSTD_FRAME_MAGIC.to_vec();
        bytes.extend_from_slice(b"not-a-valid-zstd-payload");

        assert!(decode_snapshot(&bytes, "some-commit").is_err());
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

        let shard_files = collect_bincode_files(&store.shard_dir_path("commit-a"));
        assert_eq!(shard_files.len(), 1);
        let shard_bytes = fs::read(&shard_files[0]).unwrap();
        assert!(shard_bytes.starts_with(&ZSTD_FRAME_MAGIC));
        let merged_path = store.merged_sidecar_path(
            "commit-a",
            shard_files[0]
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap(),
        );
        assert!(
            !merged_path.exists(),
            "the .merged sidecar had no reader and is no longer written (#284)"
        );

        assert_eq!(store.lookup("commit-a", &entry.input_key), Some(entry));
        assert_eq!(store.lookup("commit-a", &[99; 32]), None);
    }

    #[test]
    fn snapshot_store_loads_raw_bincode_shard_via_passthrough() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let store = SnapshotStore::new(paths);
        let shard_key = "commit-raw";
        let entry = sample_entry_with_seed(21, [11; 32]);
        let snapshot = snapshot_with_entries([entry.clone()]);
        let raw_bytes =
            bincode::serde::encode_to_vec(&snapshot, snapshot_bincode_config()).unwrap();
        let shard_id = blake3::hash(&raw_bytes).to_hex().to_string();
        let shard_path = store
            .shard_dir_path(shard_key)
            .join(format!("{shard_id}.{SNAPSHOT_FILE_EXTENSION}"));
        fs::create_dir_all(shard_path.parent().unwrap()).unwrap();
        fs::write(&shard_path, &raw_bytes).unwrap();

        let loaded = store.load(shard_key).unwrap();
        assert_eq!(
            loaded.entries.get(&input_key_hex(entry.input_key)),
            Some(&entry)
        );
    }

    #[test]
    fn compressed_snapshot_survives_remote_round_trip() {
        let temp_dir_a = tempdir().unwrap();
        let paths_a = open_shared_paths(temp_dir_a.path()).unwrap();
        let store_a = SnapshotStore::new(paths_a);
        let shard_key = "commit-remote-round-trip";
        let entries = [
            sample_entry_with_seed(2, [6; 32]),
            sample_entry_with_seed(3, [7; 32]),
        ];
        let mut last_upload = None;

        for entry in &entries {
            assert_eq!(
                store_a.merge_entry(shard_key, entry.clone()),
                MergeResult::Inserted
            );
            let shard_ids = collect_shard_ids(&store_a.shard_dir_path(shard_key));
            let shard_id = shard_ids.into_iter().next().unwrap();
            let shard_bytes = fs::read(
                store_a
                    .shard_dir_path(shard_key)
                    .join(format!("{shard_id}.{SNAPSHOT_FILE_EXTENSION}")),
            )
            .unwrap();
            last_upload = Some(SnapshotUpload {
                shard_id,
                shard_bytes,
            });
        }

        let upload = last_upload.unwrap();
        assert!(upload.shard_bytes.starts_with(&ZSTD_FRAME_MAGIC));

        let temp_dir_b = tempdir().unwrap();
        let paths_b = open_shared_paths(temp_dir_b.path()).unwrap();
        let store_b = SnapshotStore::new(paths_b);
        let shard_path = store_b
            .shard_dir_path(shard_key)
            .join(format!("{}.{SNAPSHOT_FILE_EXTENSION}", upload.shard_id));
        fs::create_dir_all(shard_path.parent().unwrap()).unwrap();
        fs::write(&shard_path, &upload.shard_bytes).unwrap();

        let snapshot = store_b.load(shard_key).unwrap();
        assert_eq!(snapshot.entries.len(), entries.len());
        for entry in entries {
            assert_eq!(
                snapshot.entries.get(&input_key_hex(entry.input_key)),
                Some(&entry)
            );
        }
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
    fn two_writers_to_the_same_input_key_merge_as_idempotent_not_conflict() {
        // With resolved input content folded into the key, two
        // writers that land on the same `input_key` have, by construction,
        // observed the same source state and therefore the same
        // deterministic build output. Storing the same key twice must merge
        // as a benign no-op, never `ConflictKeptExisting` -- that outcome is
        // now reserved for the (non-deterministic-build) case where the same
        // key somehow produced two different `outputs_hash`.
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let store = SnapshotStore::new(paths);

        let inputs_hash = crate::resolve::combined_inputs_hash(&[crate::record::FileEntry {
            path: "src/main.ts".to_string(),
            size: 10,
            mtime_ns: 0,
            hash: [0xAA; 32],
            absent: false,
        }]);
        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], inputs_hash);

        let mut first_writer = sample_entry_with_seed(20, [42; 32]);
        first_writer.input_key = input_key;
        let mut second_writer = first_writer.clone();
        second_writer.task_id = "pkg-b#build".to_owned();

        assert_eq!(
            store.merge_entry("commit-idempotent", first_writer.clone()),
            MergeResult::Inserted
        );
        assert_eq!(
            store.merge_entry("commit-idempotent", second_writer),
            MergeResult::IdempotentNoop,
            "two writers landing on one input_key must merge as idempotent, not a conflict"
        );

        let snapshot = store.load("commit-idempotent").unwrap();
        assert_eq!(snapshot.entries.len(), 1);
    }

    #[test]
    fn snapshot_store_handles_concurrent_appends_without_losing_entries() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path());
        let store = Arc::new(SnapshotStore::new(paths.unwrap()));
        let shard_key = "commit-concurrent";

        let mut handles = Vec::new();
        for seed in 4..12 {
            let store = Arc::clone(&store);
            let shard_key = shard_key.to_owned();
            handles.push(thread::spawn(move || {
                let entry = sample_entry_with_seed(seed, [seed; 32]);
                assert_eq!(store.merge_entry(&shard_key, entry), MergeResult::Inserted);
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let snapshot = store.load(shard_key).unwrap();
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
        let shard_key = "commit-hash";
        let entry = sample_entry_with_seed(9, [9; 32]);

        assert_eq!(
            store.merge_entry(shard_key, entry.clone()),
            MergeResult::Inserted
        );

        let shard_dir = store.shard_dir_path(shard_key);
        let shard_paths = collect_bincode_files(&shard_dir);
        assert_eq!(shard_paths.len(), 1);

        let bytes = fs::read(&shard_paths[0]).unwrap();
        let raw = decompress_snapshot_bytes(&bytes).unwrap();
        let expected_name = format!(
            "{}.{}",
            blake3::hash(&raw).to_hex(),
            SNAPSHOT_FILE_EXTENSION
        );
        assert_eq!(
            shard_paths[0].file_name().unwrap().to_string_lossy(),
            expected_name
        );

        let snapshot = decode_snapshot(&bytes, shard_key).unwrap();
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(
            snapshot.entries.get(&input_key_hex(entry.input_key)),
            Some(&entry)
        );
    }

    #[test]
    fn snapshot_store_compacts_seeded_shards_and_reports_subsumed_ids() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let store = SnapshotStore::new(paths.clone());
        let shard_key = "commit-compact";
        let seeded = [
            sample_entry_with_seed(12, [1; 32]),
            sample_entry_with_seed(13, [2; 32]),
            sample_entry_with_seed(14, [3; 32]),
        ];

        for entry in &seeded {
            let snapshot = snapshot_with_entries([entry.clone()]);
            let bytes =
                bincode::serde::encode_to_vec(&snapshot, snapshot_bincode_config()).unwrap();
            let shard_id = blake3::hash(&bytes).to_hex().to_string();
            write_snapshot_file(
                &store
                    .shard_dir_path(shard_key)
                    .join(format!("{shard_id}.{SNAPSHOT_FILE_EXTENSION}")),
                snapshot,
            );
        }

        let shard_ids_before = collect_shard_ids(&store.shard_dir_path(shard_key));
        assert_eq!(shard_ids_before.len(), 3);

        let new_entry = sample_entry_with_seed(15, [4; 32]);
        let outcome = store.merge_entry_with_outcome(shard_key, new_entry.clone());
        assert_eq!(outcome.result, MergeResult::Inserted);

        let shard_ids_after = collect_shard_ids(&store.shard_dir_path(shard_key));
        assert_eq!(shard_ids_after.len(), 1);
        // The subsumed ids used to be written to a `.merged` sidecar nobody
        // read (#284). They still exist where they are actually used: the
        // merge outcome, which drives the remote deletes.
        let mut subsumed = outcome.subsumed_shard_ids.clone();
        subsumed.sort();
        let mut expected = shard_ids_before.clone();
        expected.sort();
        assert_eq!(subsumed, expected);
        assert!(
            !store
                .merged_sidecar_path(shard_key, &shard_ids_after[0])
                .exists(),
            "no sidecar should be written for the consolidated shard"
        );

        let snapshot = store.load(shard_key).unwrap();
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
        let shard_key = "commit-unseen";
        let seen_entry = sample_entry_with_seed(16, [5; 32]);
        let unseen_entry = sample_entry_with_seed(17, [6; 32]);
        let new_entry = sample_entry_with_seed(18, [7; 32]);

        let seen_snapshot = snapshot_with_entries([seen_entry.clone()]);
        let seen_bytes =
            bincode::serde::encode_to_vec(&seen_snapshot, snapshot_bincode_config()).unwrap();
        let seen_shard_id = blake3::hash(&seen_bytes).to_hex().to_string();
        write_snapshot_file(
            &store
                .shard_dir_path(shard_key)
                .join(format!("{seen_shard_id}.{SNAPSHOT_FILE_EXTENSION}")),
            seen_snapshot,
        );

        let visible_shards = store.list_snapshot_shards(shard_key);
        let MergedShards {
            snapshot,
            merged_shard_ids: _,
        } = store.load_merged_snapshot_from_shards(shard_key, visible_shards.clone());
        let mut consolidated = snapshot.unwrap();
        consolidated
            .entries
            .insert(input_key_hex(new_entry.input_key), new_entry.clone());
        let consolidated_bytes =
            bincode::serde::encode_to_vec(&consolidated, snapshot_bincode_config()).unwrap();
        let consolidated_id = blake3::hash(&consolidated_bytes).to_hex().to_string();
        let consolidated_path = store
            .shard_dir_path(shard_key)
            .join(format!("{consolidated_id}.{SNAPSHOT_FILE_EXTENSION}"));

        let consolidated_on_disk = compress_snapshot_bytes(&consolidated_bytes).unwrap();
        atomic_write(&consolidated_path, &consolidated_on_disk).unwrap();

        let unseen_snapshot = snapshot_with_entries([unseen_entry.clone()]);
        let unseen_bytes =
            bincode::serde::encode_to_vec(&unseen_snapshot, snapshot_bincode_config()).unwrap();
        let unseen_shard_id = blake3::hash(&unseen_bytes).to_hex().to_string();
        write_snapshot_file(
            &store
                .shard_dir_path(shard_key)
                .join(format!("{unseen_shard_id}.{SNAPSHOT_FILE_EXTENSION}")),
            unseen_snapshot,
        );

        let subsumed_shard_ids = visible_shards
            .iter()
            .filter_map(SnapshotShard::deletable_shard_id)
            .collect::<Vec<_>>();
        for shard_id in subsumed_shard_ids {
            store.delete_shard_files_by_id(shard_key, &shard_id);
        }

        let shard_ids_after = collect_shard_ids(&store.shard_dir_path(shard_key));
        let mut expected_shard_ids = vec![consolidated_id.clone(), unseen_shard_id.clone()];
        expected_shard_ids.sort();
        assert_eq!(shard_ids_after, expected_shard_ids);

        let restored = store.load(shard_key).unwrap();
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
        let shard_key = "commit-delete-noop";
        let entry = sample_entry_with_seed(19, [8; 32]);
        let snapshot = snapshot_with_entries([entry.clone()]);
        let bytes = bincode::serde::encode_to_vec(&snapshot, snapshot_bincode_config()).unwrap();
        let shard_id = blake3::hash(&bytes).to_hex().to_string();
        let shard_path = store
            .shard_dir_path(shard_key)
            .join(format!("{shard_id}.{SNAPSHOT_FILE_EXTENSION}"));
        write_snapshot_file(&shard_path, snapshot);
        atomic_write(
            &store.merged_sidecar_path(shard_key, &shard_id),
            b"some-old-sidecar\n",
        )
        .unwrap();

        fs::remove_file(&shard_path).unwrap();
        store.delete_shard_files_by_id(shard_key, &shard_id);
        store.delete_shard_files_by_id(shard_key, &shard_id);

        assert!(!store.merged_sidecar_path(shard_key, &shard_id).exists());
    }

    #[test]
    fn snapshot_store_load_merges_legacy_file_alongside_shards() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let store = SnapshotStore::new(paths.clone());
        let shard_key = "commit-legacy";
        let legacy_entry = sample_entry_with_seed(14, [3; 32]);
        let shard_entry = sample_entry_with_seed(15, [4; 32]);

        write_snapshot_file(
            &store.legacy_snapshot_path(shard_key),
            snapshot_with_entries([legacy_entry.clone()]),
        );
        assert_eq!(
            store.merge_entry(shard_key, shard_entry.clone()),
            MergeResult::Inserted
        );

        let snapshot = store.load(shard_key).unwrap();
        assert_eq!(snapshot.entries.len(), 2);
        assert_eq!(
            snapshot.entries.get(&input_key_hex(legacy_entry.input_key)),
            Some(&legacy_entry)
        );
        assert_eq!(
            snapshot.entries.get(&input_key_hex(shard_entry.input_key)),
            Some(&shard_entry)
        );
        assert!(store.legacy_snapshot_path(shard_key).exists());
    }

    #[test]
    fn snapshot_store_load_skips_corrupt_shards() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let store = SnapshotStore::new(paths.clone());
        let shard_key = "commit-corrupt";
        let valid_entry = sample_entry_with_seed(20, [5; 32]);

        assert_eq!(
            store.merge_entry(shard_key, valid_entry.clone()),
            MergeResult::Inserted
        );

        let corrupt_path = store
            .shard_dir_path(shard_key)
            .join(format!("{}.{}", "0000badshard", SNAPSHOT_FILE_EXTENSION));
        fs::write(&corrupt_path, b"not-bincode").unwrap();

        let snapshot = store.load(shard_key).unwrap();
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
        let shard_key = "commit-legacy-conflict";
        let original = sample_entry_with_seed(22, [9; 32]);
        let mut conflicting = original.clone();
        conflicting.outputs_hash = [10; 32];
        conflicting.cached_at_unix_ms += 1;

        write_snapshot_file(
            &store.legacy_snapshot_path(shard_key),
            snapshot_with_entries([conflicting]),
        );
        let shard_path = store
            .shard_dir_path(shard_key)
            .join(format!("{}.{}", "000-first", SNAPSHOT_FILE_EXTENSION));
        write_snapshot_file(&shard_path, snapshot_with_entries([original.clone()]));

        let snapshot = store.load(shard_key).unwrap();
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
        let shard_key = "commit-conflict";
        let original = sample_entry_with_seed(21, [6; 32]);
        let mut conflicting = original.clone();
        conflicting.outputs_hash = [7; 32];
        conflicting.cached_at_unix_ms += 10;

        let low_id_path = store
            .shard_dir_path(shard_key)
            .join(format!("{}.{}", "000-first", SNAPSHOT_FILE_EXTENSION));
        let high_id_path = store
            .shard_dir_path(shard_key)
            .join(format!("{}.{}", "zzz-last", SNAPSHOT_FILE_EXTENSION));
        write_snapshot_file(&high_id_path, snapshot_with_entries([conflicting]));
        write_snapshot_file(&low_id_path, snapshot_with_entries([original.clone()]));

        let snapshot = store.load(shard_key).unwrap();
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(
            snapshot.entries.get(&input_key_hex(original.input_key)),
            Some(&original)
        );
    }

    fn sample_snapshot() -> Snapshot {
        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
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
            inline_meta: None,
            duration_trusted: true,
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
            inline_meta: None,
            duration_trusted: true,
        }
    }

    fn sample_inline_meta(outputs_hash: [u8; 32]) -> EntryMeta {
        EntryMeta {
            schema_version: crate::shared::ENTRY_META_SCHEMA_VERSION,
            outputs_hash,
            has_outputs: false,
            record: vec![1, 2, 3],
            stdout: b"inline stdout".to_vec(),
            stderr: b"inline stderr".to_vec(),
            reports: Vec::new(),
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

    #[test]
    fn snapshot_store_skips_missing_shard_without_error() {
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let store = SnapshotStore::new(paths);
        let shard_key = "commit-missing";
        let present_entry = sample_entry_with_seed(20, [9; 32]);
        let missing_entry = sample_entry_with_seed(21, [10; 32]);

        for snapshot in [
            snapshot_with_entries([present_entry.clone()]),
            snapshot_with_entries([missing_entry.clone()]),
        ] {
            let bytes =
                bincode::serde::encode_to_vec(&snapshot, snapshot_bincode_config()).unwrap();
            let shard_id = blake3::hash(&bytes).to_hex().to_string();
            write_snapshot_file(
                &store
                    .shard_dir_path(shard_key)
                    .join(format!("{shard_id}.{SNAPSHOT_FILE_EXTENSION}")),
                snapshot,
            );
        }

        // Capture the shard list, then delete one shard file out from under it
        // to simulate a shard that was pruned or never synced from a remote
        // cache. The stale entry must be skipped silently.
        let shards = store.list_snapshot_shards(shard_key);
        assert_eq!(shards.len(), 2);
        let missing_bytes = bincode::serde::encode_to_vec(
            snapshot_with_entries([missing_entry.clone()]),
            snapshot_bincode_config(),
        )
        .unwrap();
        let missing_shard_id = blake3::hash(&missing_bytes).to_hex().to_string();
        fs::remove_file(
            store
                .shard_dir_path(shard_key)
                .join(format!("{missing_shard_id}.{SNAPSHOT_FILE_EXTENSION}")),
        )
        .unwrap();

        let merged = store
            .load_merged_snapshot_from_shards(shard_key, shards)
            .snapshot
            .expect("surviving shard should still load");
        assert_eq!(merged.entries.len(), 1);
        assert_eq!(
            merged.entries.get(&input_key_hex(present_entry.input_key)),
            Some(&present_entry)
        );
        assert!(!merged
            .entries
            .contains_key(&input_key_hex(missing_entry.input_key)));
    }

    fn write_snapshot_file(path: &Path, snapshot: Snapshot) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bytes = bincode::serde::encode_to_vec(&snapshot, snapshot_bincode_config()).unwrap();
        let on_disk = compress_snapshot_bytes(&bytes).unwrap();
        fs::write(path, on_disk).unwrap();
    }

    /// Writes a shard this client cannot decode, and returns its id and path.
    /// `schema_version` above `SNAPSHOT_SCHEMA_VERSION` is what a client
    /// running a newer luchta writes into a shared bucket.
    fn write_future_schema_shard(
        store: &SnapshotStore,
        shard_key: &str,
        entry: SnapshotEntry,
    ) -> (String, PathBuf) {
        let mut snapshot = snapshot_with_entries([entry]);
        snapshot.schema_version = SNAPSHOT_SCHEMA_VERSION + 1;
        let encoded = bincode::serde::encode_to_vec(&snapshot, snapshot_bincode_config()).unwrap();
        let shard_id = blake3::hash(&encoded).to_hex().to_string();
        let path = store
            .shard_dir_path(shard_key)
            .join(format!("{shard_id}.{SNAPSHOT_FILE_EXTENSION}"));
        atomic_write(&path, &compress_snapshot_bytes(&encoded).unwrap()).unwrap();
        (shard_id, path)
    }

    #[test]
    fn merge_heals_a_stale_file_where_the_shard_dir_belongs() {
        // What a cache written by an older luchta looks like after the key
        // scheme changed: a plain file at `snapshots/<key>`, where the shard
        // directory now goes. Before, `create_dir_all` failed on it, the
        // merge bailed with a `debug:` line, and that bucket never worked
        // again -- a shared cache that silently never hits (#276).
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let store = SnapshotStore::new(paths);
        let shard_key = "20260808-03";

        let blocking = store.shard_dir_path(shard_key);
        fs::create_dir_all(blocking.parent().unwrap()).unwrap();
        fs::write(&blocking, b"snapshot bytes from an older layout").unwrap();
        assert!(blocking.is_file(), "the stale file must be in place");

        let entry = sample_entry_with_seed(1, [5; 32]);
        assert_eq!(
            store.merge_entry(shard_key, entry.clone()),
            MergeResult::Inserted,
            "the merge must clear the stale file rather than fail forever"
        );

        assert!(
            blocking.is_dir(),
            "the shard path should now be a directory"
        );
        let loaded = store.load(shard_key).expect("the bucket must be readable");
        assert_eq!(
            loaded.entries.get(&input_key_hex(entry.input_key)),
            Some(&entry),
            "the entry written after healing must be readable back"
        );
    }

    #[test]
    fn merge_does_not_subsume_a_shard_it_could_not_decode() {
        // A shard written by a client running a newer schema version is
        // skipped by the merge, so the consolidated shard does not contain
        // its entries. Deleting it would destroy cache entries this client
        // merely couldn't read -- and because `subsumed_shard_ids` also
        // drives `push_index_merge`'s remote deletes, an older client would
        // wipe a newer client's shard off the shared object store.
        let temp_dir = tempdir().unwrap();
        let paths = open_shared_paths(temp_dir.path()).unwrap();
        let store = SnapshotStore::new(paths);
        let shard_key = "20260807-00";

        // Seed a readable shard so the merge has something to subsume, which
        // keeps this test honest: if it deleted nothing at all, the
        // assertions below would pass for the wrong reason.
        let readable = sample_entry_with_seed(1, [5; 32]);
        assert_eq!(
            store.merge_entry(shard_key, readable.clone()),
            MergeResult::Inserted
        );
        let readable_shard_id = collect_bincode_files(&store.shard_dir_path(shard_key))
            .first()
            .and_then(|path| path.file_stem())
            .and_then(|stem| stem.to_str())
            .map(str::to_owned)
            .expect("the readable shard should be on disk");

        let future_entry = sample_entry_with_seed(2, [6; 32]);
        let (future_shard_id, future_path) =
            write_future_schema_shard(&store, shard_key, future_entry);

        let outcome = store.merge_entry_with_outcome(shard_key, sample_entry_with_seed(3, [7; 32]));
        assert_eq!(outcome.result, MergeResult::Inserted);

        assert!(
            !outcome.subsumed_shard_ids.contains(&future_shard_id),
            "an undecodable shard must not be reported as subsumed, or the remote copy gets deleted too"
        );
        assert!(
            future_path.exists(),
            "an undecodable shard must survive a merge by a client that cannot read it"
        );
        assert!(
            outcome.subsumed_shard_ids.contains(&readable_shard_id),
            "shards that did merge must still be subsumed, or consolidation stops reclaiming anything"
        );
        assert!(
            !store
                .shard_dir_path(shard_key)
                .join(format!("{readable_shard_id}.{SNAPSHOT_FILE_EXTENSION}"))
                .exists(),
            "the subsumed readable shard should be gone from disk"
        );
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
