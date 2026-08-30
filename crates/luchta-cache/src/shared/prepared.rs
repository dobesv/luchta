use super::{SnapshotEntry, StagedCandidate};
use crate::record::TaskRunRecord;

/// Restore work prepared without mutating package outputs. The dispatch loop
/// serializes `commit`, local hydration, refresh, and output-hash publication.
#[derive(Debug)]
pub struct PreparedRestore {
    pub(super) candidate: StagedCandidate,
    pub(super) snapshot_entry: SnapshotEntry,
}

impl PreparedRestore {
    #[must_use]
    pub fn record(&self) -> &TaskRunRecord {
        &self.candidate.record
    }

    #[must_use]
    pub fn into_parts(self) -> (StagedCandidate, SnapshotEntry) {
        (self.candidate, self.snapshot_entry)
    }
}
