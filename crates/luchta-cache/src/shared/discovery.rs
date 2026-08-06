//! Shard discovery for the shared cache.
//!
//! Shards used to be named by git commit id and discovered by walking
//! first-parent ancestry from HEAD. That fails whenever builds run on commits
//! no other build will ever see — feature branches, and especially Prow's
//! temporary merged-with-master commits. See issue #277.
//!
//! Shards are now named `<unix_ms>-<nonce>` and discovered by recency.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::shared::paths::SharedCachePaths;

static SHARD_NONCE: AtomicU64 = AtomicU64::new(0);

/// Build a shard key from an explicit timestamp and nonce.
///
/// The millisecond field is zero-padded to 13 digits so lexical ordering
/// matches chronological ordering — the fallback when a remote listing does
/// not report modification times.
#[must_use]
pub fn new_session_shard_key(now_unix_ms: u64, nonce: u64) -> String {
    format!("{now_unix_ms:013}-{:08x}", nonce & 0xffff_ffff)
}

/// Build a shard key for this process's current run.
#[must_use]
pub fn current_session_shard_key() -> String {
    let now_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0);
    let nonce = SHARD_NONCE.fetch_add(1, Ordering::Relaxed) ^ (std::process::id() as u64) << 16;
    new_session_shard_key(now_unix_ms, nonce)
}

/// Placeholder until Task 8 implements local recency discovery.
pub fn discover_recent_shard_keys(_paths: &SharedCachePaths, _limit: usize) -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_shard_keys_sort_chronologically_by_name() {
        let earlier = new_session_shard_key(1_754_431_200_123, 0x0000_00ff);
        let later = new_session_shard_key(1_754_431_200_456, 0x0000_0001);
        assert!(earlier < later, "{earlier} should sort before {later}");
    }

    #[test]
    fn session_shard_key_has_stable_shape() {
        assert_eq!(
            new_session_shard_key(1_754_431_200_123, 0xdead_beef),
            "1754431200123-deadbeef"
        );
    }

    #[test]
    fn session_shard_keys_differ_for_the_same_millisecond() {
        let first = new_session_shard_key(1_754_431_200_123, 1);
        let second = new_session_shard_key(1_754_431_200_123, 2);
        assert_ne!(first, second);
    }

    #[test]
    fn current_session_shard_key_is_unique_across_calls() {
        let first = current_session_shard_key();
        let second = current_session_shard_key();
        assert_ne!(first, second);
    }
}
