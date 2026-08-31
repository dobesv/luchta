use std::collections::HashMap;
#[cfg(unix)]
use std::sync::Arc;

#[cfg(unix)]
use super::{read_entry_meta, remote};
use super::{CacheFileObservation, MergeEntryOutcome, MergeResult, SharedCache, SnapshotEntry};

/// Index observations accumulated for the single end-of-run snapshot merge.
/// Entries are keyed by input key so repeated stores or refreshes collapse.
#[derive(Debug, Default)]
pub(super) struct PendingState {
    pub(super) entries: HashMap<[u8; 32], SnapshotEntry>,
    cache_file_observations: Vec<CacheFileObservation>,
    /// One refreshed entry used for a best-effort remote artifact catch-up.
    #[cfg(unix)]
    catchup_representative: Option<SnapshotEntry>,
}

struct PendingSnapshotUpdates {
    entries: Vec<SnapshotEntry>,
    cache_file_observations: Vec<CacheFileObservation>,
}

impl PendingSnapshotUpdates {
    fn len(&self) -> usize {
        self.entries.len() + self.cache_file_observations.len()
    }
}

impl PendingState {
    pub(super) fn record_entry(&mut self, entry: SnapshotEntry) {
        self.entries.insert(entry.input_key, entry);
    }

    pub(super) fn record_refresh(&mut self, entry: SnapshotEntry) {
        #[cfg(unix)]
        if self.catchup_representative.is_none() {
            self.catchup_representative = Some(entry.clone());
        }
        self.record_entry(entry);
    }

    pub(super) fn record_cache_file(&mut self, observation: CacheFileObservation) {
        self.cache_file_observations.push(observation);
    }

    fn drain(&mut self) -> Option<PendingSnapshotUpdates> {
        if self.entries.is_empty() && self.cache_file_observations.is_empty() {
            #[cfg(unix)]
            {
                self.catchup_representative = None;
            }
            return None;
        }
        Some(PendingSnapshotUpdates {
            entries: std::mem::take(&mut self.entries).into_values().collect(),
            cache_file_observations: std::mem::take(&mut self.cache_file_observations),
        })
    }

    #[cfg(unix)]
    fn take_catchup_representative(&mut self) -> Option<SnapshotEntry> {
        self.catchup_representative.take()
    }
}

pub(super) fn flush(cache: &SharedCache) {
    // Resolve the key before the destructive drain so a disabled writer can
    // never discard observations with nowhere to persist them.
    let Some(write_key) = cache.write_bucket_key.as_deref() else {
        return;
    };
    let updates = {
        let mut pending = cache
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending.drain()
    };
    let Some(updates) = updates else {
        return;
    };
    let entry_count = updates.len();
    let merge = cache.snapshot_store.merge_updates_with_outcome(
        write_key,
        updates.entries,
        updates.cache_file_observations,
    );
    if merge.result == MergeResult::SkippedLockUnavailable {
        warn_dropped_entries(entry_count);
        return;
    }
    finish_merge(cache, write_key, merge);
}

fn warn_dropped_entries(entry_count: usize) {
    eprintln!(
        "warn: shared cache could not lock its index shard; dropped {entry_count} index \
         entries for this run, so those tasks will be rebuilt next time"
    );
}

fn finish_merge(cache: &SharedCache, write_key: &str, merge: MergeEntryOutcome) {
    #[cfg(unix)]
    {
        enqueue_catchup(cache);
        cache.enqueue_index_push(write_key, merge);
    }
    #[cfg(not(unix))]
    {
        let _ = (cache, write_key, merge);
    }
}

#[cfg(unix)]
fn enqueue_catchup(cache: &SharedCache) {
    let representative = cache
        .pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take_catchup_representative();
    let Some(representative) = representative else {
        return;
    };
    let has_outputs = representative
        .inline_meta
        .as_ref()
        .map(|meta| meta.has_outputs)
        .or_else(|| {
            read_entry_meta(&cache.paths, &representative.input_key).map(|meta| meta.has_outputs)
        })
        .unwrap_or(false);
    cache.enqueue_entry_artifacts(remote::OwnedEntryArtifacts {
        paths: Arc::clone(&cache.paths),
        outputs_hash: representative.outputs_hash,
        input_key: representative.input_key,
        presence: remote::ArtifactPresence {
            outputs: has_outputs,
            entry_meta: representative.inline_meta.is_none(),
        },
    });
}
