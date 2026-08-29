//! Run-cycle aggregate observability for shared-cache remote work.

#[cfg(unix)]
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

#[cfg(unix)]
pub(crate) fn directory_file_bytes(path: &Path) -> u64 {
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
        .sum()
}

#[derive(Debug, Default)]
pub(crate) struct SharedCacheStats {
    pub(crate) snapshot_syncs: AtomicU64,
    pub(crate) snapshot_bytes: AtomicU64,
    pub(crate) inline_hits: AtomicU64,
    pub(crate) fallback_meta_gets: AtomicU64,
    pub(crate) blob_gets: AtomicU64,
    pub(crate) download_bytes: AtomicU64,
    pub(crate) download_latency_ms: AtomicU64,
    pub(crate) queue_depth: AtomicUsize,
    pub(crate) queue_max_depth: AtomicUsize,
    pub(crate) queue_drops: AtomicU64,
    pub(crate) uploads: AtomicU64,
    pub(crate) upload_bytes: AtomicU64,
    pub(crate) upload_latency_ms: AtomicU64,
    pub(crate) disable_reason: Mutex<Option<String>>,
}

impl SharedCacheStats {
    #[cfg(any(unix, test))]
    pub(crate) fn observe_queue_depth(&self, depth: usize) {
        self.queue_depth.store(depth, Ordering::Release);
        self.queue_max_depth.fetch_max(depth, Ordering::AcqRel);
    }

    #[cfg(any(unix, test))]
    pub(crate) fn set_disable_reason(&self, reason: &str) {
        *self
            .disable_reason
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(reason.to_owned());
    }

    pub(crate) fn take_diagnostic_line(&self) -> String {
        let current_depth = self.queue_depth.load(Ordering::Acquire);
        let max_depth = self.queue_max_depth.swap(current_depth, Ordering::AcqRel);
        let disable_reason = self
            .disable_reason
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .unwrap_or_else(|| "none".to_owned());
        format!(
            "shared cache stats: snapshot_syncs={} snapshot_bytes={} inline_hits={} \
             fallback_meta_gets={} blob_gets={} download_bytes={} download_ms={} \
             queue_depth={} queue_max={} queue_drops={} uploads={} upload_bytes={} \
             upload_ms={} disabled={}",
            self.snapshot_syncs.swap(0, Ordering::AcqRel),
            self.snapshot_bytes.swap(0, Ordering::AcqRel),
            self.inline_hits.swap(0, Ordering::AcqRel),
            self.fallback_meta_gets.swap(0, Ordering::AcqRel),
            self.blob_gets.swap(0, Ordering::AcqRel),
            self.download_bytes.swap(0, Ordering::AcqRel),
            self.download_latency_ms.swap(0, Ordering::AcqRel),
            current_depth,
            max_depth,
            self.queue_drops.swap(0, Ordering::AcqRel),
            self.uploads.swap(0, Ordering::AcqRel),
            self.upload_bytes.swap(0, Ordering::AcqRel),
            self.upload_latency_ms.swap(0, Ordering::AcqRel),
            disable_reason,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_line_reports_and_resets_cycle_counters() {
        let stats = SharedCacheStats::default();
        stats.snapshot_syncs.store(2, Ordering::Release);
        stats.inline_hits.store(7, Ordering::Release);
        stats.queue_drops.store(3, Ordering::Release);
        stats.observe_queue_depth(4);
        stats.observe_queue_depth(1);
        stats.set_disable_reason("artifact upload timeout");

        let first = stats.take_diagnostic_line();
        assert!(first.starts_with("shared cache stats:"));
        assert!(first.contains("snapshot_syncs=2"));
        assert!(first.contains("inline_hits=7"));
        assert!(first.contains("queue_depth=1 queue_max=4 queue_drops=3"));
        assert!(first.contains("disabled=artifact upload timeout"));

        let second = stats.take_diagnostic_line();
        assert!(second.contains("snapshot_syncs=0"));
        assert!(second.contains("inline_hits=0"));
        assert!(second.contains("queue_depth=1 queue_max=1 queue_drops=0"));
    }
}
