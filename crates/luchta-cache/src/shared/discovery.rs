//! Shard discovery for the shared cache.
//!
//! Shards used to be named by git commit id and discovered by walking
//! first-parent ancestry from HEAD. That fails whenever builds run on commits
//! no other build will ever see — feature branches, and especially Prow's
//! temporary merged-with-master commits. See issue #277.
//!
//! Shards are now named `<unix_ms>-<nonce>` and discovered by recency.

use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::shared::paths::SharedCachePaths;

static SHARD_NONCE: AtomicU64 = AtomicU64::new(0);

/// Drop shards older than two weeks. Long enough to span a quiet weekend,
/// short enough that the merged index stays small.
pub const DEFAULT_SHARD_MAX_AGE_MS: u64 = 1000 * 60 * 60 * 24 * 14;

/// A shard directory found on disk (or reported by a remote listing),
/// carrying just enough to rank it: its key and last-modified time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardCandidate {
    pub key: String,
    pub modified_unix_ms: u64,
}

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

/// Rank candidates newest-first, capped at `limit` and filtered by `max_age_ms`.
///
/// Ties on modification time fall back to key order descending, which is
/// chronological because keys are zero-padded millisecond timestamps.
#[must_use]
pub fn rank_shard_candidates(
    candidates: Vec<ShardCandidate>,
    limit: usize,
    max_age_ms: Option<u64>,
    now_unix_ms: u64,
) -> Vec<String> {
    let mut kept: Vec<ShardCandidate> = candidates
        .into_iter()
        .filter(|candidate| match max_age_ms {
            Some(max_age_ms) => {
                now_unix_ms.saturating_sub(candidate.modified_unix_ms) <= max_age_ms
            }
            None => true,
        })
        .collect();

    kept.sort_unstable_by(|left, right| {
        right
            .modified_unix_ms
            .cmp(&left.modified_unix_ms)
            .then_with(|| right.key.cmp(&left.key))
    });
    kept.truncate(limit);
    kept.into_iter().map(|candidate| candidate.key).collect()
}

/// Discover shard directories present in the local cache, unranked.
///
/// Exposed so callers that also have a remote listing (see `RemoteSync::
/// list_shard_candidates`) can union the two candidate sets before calling
/// `rank_shard_candidates` once over the combined set.
#[must_use]
pub fn local_shard_candidates_for(paths: &SharedCachePaths) -> Vec<ShardCandidate> {
    let Ok(entries) = fs::read_dir(&paths.snapshots_dir) else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(key) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let modified_unix_ms = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0);
        candidates.push(ShardCandidate {
            key: key.to_string(),
            modified_unix_ms,
        });
    }
    candidates
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

    fn candidate(key: &str, modified_unix_ms: u64) -> ShardCandidate {
        ShardCandidate {
            key: key.to_string(),
            modified_unix_ms,
        }
    }

    const NOW: u64 = 1_754_431_200_000;

    #[test]
    fn shard_key_zero_pads_short_timestamps_so_lexical_order_stays_chronological() {
        // The 13-digit pad is the ordering fallback for remotes that don't report
        // ModTime. Task 7's tests all used already-13-digit values, so padding never
        // actually fired — this covers the case where it does.
        let short = new_session_shard_key(5, 0);
        let full = new_session_shard_key(1_754_431_200_123, 0);
        assert_eq!(short, "0000000000005-00000000");
        assert!(short < full, "{short} must sort before {full}");
    }

    #[test]
    fn rank_returns_newest_first() {
        let ranked = rank_shard_candidates(
            vec![
                candidate("a", NOW - 3_000),
                candidate("b", NOW - 1_000),
                candidate("c", NOW - 2_000),
            ],
            10,
            None,
            NOW,
        );
        assert_eq!(ranked, vec!["b", "c", "a"]);
    }

    #[test]
    fn rank_applies_the_limit() {
        let ranked = rank_shard_candidates(
            vec![
                candidate("a", NOW - 3_000),
                candidate("b", NOW - 1_000),
                candidate("c", NOW - 2_000),
            ],
            2,
            None,
            NOW,
        );
        assert_eq!(ranked, vec!["b", "c"]);
    }

    #[test]
    fn rank_drops_shards_older_than_max_age() {
        let ranked = rank_shard_candidates(
            vec![
                candidate("fresh", NOW - 1_000),
                candidate("stale", NOW - 100_000),
            ],
            10,
            Some(10_000),
            NOW,
        );
        assert_eq!(ranked, vec!["fresh"]);
    }

    #[test]
    fn rank_breaks_mtime_ties_by_key_descending() {
        let ranked = rank_shard_candidates(
            vec![
                candidate("0000000000001-aaaa", NOW),
                candidate("0000000000001-bbbb", NOW),
            ],
            10,
            None,
            NOW,
        );
        assert_eq!(ranked, vec!["0000000000001-bbbb", "0000000000001-aaaa"]);
    }

    #[test]
    fn discover_finds_local_shard_dirs_newest_first() {
        use std::time::{Duration, SystemTime};
        let temp = tempfile::TempDir::new().unwrap();
        let paths = crate::shared::paths::open_shared_paths(temp.path()).unwrap();

        for key in [
            "0000000000001-aaaa",
            "0000000000002-bbbb",
            "0000000000003-cccc",
        ] {
            std::fs::create_dir_all(paths.snapshots_dir.join(key)).unwrap();
        }

        // Make "0000000000001-aaaa" the newest by mtime to prove mtime wins over name.
        let newest = paths.snapshots_dir.join("0000000000001-aaaa");
        filetime::set_file_mtime(
            &newest,
            filetime::FileTime::from_system_time(SystemTime::now() + Duration::from_secs(60)),
        )
        .unwrap();

        let now_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let discovered = rank_shard_candidates(
            local_shard_candidates_for(&paths),
            10,
            Some(DEFAULT_SHARD_MAX_AGE_MS),
            now_unix_ms,
        );
        assert_eq!(
            discovered.first().map(String::as_str),
            Some("0000000000001-aaaa")
        );
        assert_eq!(discovered.len(), 3);
    }

    #[test]
    fn local_shard_candidates_for_returns_empty_when_snapshots_dir_is_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        let paths = SharedCachePaths {
            root: temp.path().to_path_buf(),
            blobs_dir: temp.path().join("blobs"),
            snapshots_dir: temp.path().join("does-not-exist"),
            entries_dir: temp.path().join("entries"),
        };
        assert!(local_shard_candidates_for(&paths).is_empty());
    }
}
