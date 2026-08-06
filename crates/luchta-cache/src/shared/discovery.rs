//! Shard discovery for the shared cache.
//!
//! Shards used to be named by git commit id and discovered by walking
//! first-parent ancestry from HEAD. That fails whenever builds run on commits
//! no other build will ever see — feature branches, and especially Prow's
//! temporary merged-with-master commits. See issue #277.
//!
//! Shards are now named `<unix_ms>-<nonce>` and discovered by recency.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::shared::paths::SharedCachePaths;
use crate::shared::snapshot::SNAPSHOT_FILE_EXTENSION;

static SHARD_NONCE: AtomicU64 = AtomicU64::new(0);

/// Drop shards older than two weeks. Long enough to span a quiet weekend,
/// short enough that the merged index stays small.
pub const DEFAULT_SHARD_MAX_AGE_MS: u64 = 1000 * 60 * 60 * 24 * 14;

/// Default entry budget (in on-disk shard bytes — see `ShardCandidate::
/// entry_count`) for the discovery window, on top of the existing
/// shard-count `limit`. Sized well above what a single day's worth of local
/// `luchta run` invocations would produce, so ordinary local churn does not
/// push an older, not-yet-rolled-up shard out of the window before a rollup
/// ever sees it.
pub const DEFAULT_SHARED_CACHE_ENTRY_BUDGET: usize = 50 * 1024 * 1024;

/// A shard directory found on disk (or reported by a remote listing),
/// carrying just enough to rank it: its key, last-modified time, and a weight
/// for the entry-budget walk in `rank_shard_candidates`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardCandidate {
    pub key: String,
    pub modified_unix_ms: u64,
    /// A cheap proxy for how much this shard would cost to load: its on-disk
    /// (or remote-listed) byte size, or 1 when unknown. Not a literal
    /// `SnapshotEntry` count — an exact count would mean decoding every
    /// candidate during discovery, doubling the load `build_index` already
    /// does for the shards that survive ranking. Byte size is roughly linear
    /// in entry count for this format (each `SnapshotEntry` is dominated by a
    /// handful of fixed-size hashes), and it's the same unit remote listings
    /// already report, so local and remote candidates rank on a common scale.
    pub entry_count: usize,
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

/// Rank candidates newest-first, filtered by `max_age_ms`, then walk that
/// list accumulating `entry_count` until either `entry_budget` is reached or
/// `limit` shards have been kept — whichever comes first.
///
/// `limit` is a hard upper bound on shard *count*, independent of
/// `entry_budget`: it exists so a run of shards that individually under-report
/// (or a remote listing with unknown sizes, which fall back to an
/// `entry_count` of 1) can't force the walk through an unbounded number of
/// shards while still short of the budget. `entry_budget` is what actually
/// keeps the window scoped to a bounded amount of history: a handful of tiny
/// local shards contribute only a handful of entries, so they don't burn
/// through the budget the way they would a shard-count-only limit, leaving
/// room for an older, larger shard (e.g. a rollup pack) to still make the cut.
///
/// Ties on modification time fall back to key order descending, which is
/// chronological because keys are zero-padded millisecond timestamps.
#[must_use]
pub fn rank_shard_candidates(
    candidates: Vec<ShardCandidate>,
    limit: usize,
    entry_budget: usize,
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

    let mut selected = Vec::new();
    let mut entries_so_far: usize = 0;
    for candidate in kept {
        if selected.len() >= limit || entries_so_far >= entry_budget {
            break;
        }
        entries_so_far = entries_so_far.saturating_add(candidate.entry_count);
        selected.push(candidate.key);
    }
    selected
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
            entry_count: shard_dir_byte_size(&path),
        });
    }
    candidates
}

/// Sum of the `.bincode` shard file sizes in a shard directory: a cheap
/// stand-in for entry count that costs a `read_dir` and some `stat` calls,
/// not a decode of every candidate's contents. Falls back to 1 (never 0, so
/// the candidate still counts toward the entry budget) if the directory
/// can't be read or is empty.
fn shard_dir_byte_size(shard_dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(shard_dir) else {
        return 1;
    };
    let total: u64 = entries
        .flatten()
        .filter(|entry| {
            entry.path().extension().and_then(|ext| ext.to_str()) == Some(SNAPSHOT_FILE_EXTENSION)
        })
        .filter_map(|entry| entry.metadata().ok())
        .map(|metadata| metadata.len())
        .sum();
    usize::try_from(total).unwrap_or(usize::MAX).max(1)
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
        candidate_with_entries(key, modified_unix_ms, 1)
    }

    fn candidate_with_entries(
        key: &str,
        modified_unix_ms: u64,
        entry_count: usize,
    ) -> ShardCandidate {
        ShardCandidate {
            key: key.to_string(),
            modified_unix_ms,
            entry_count,
        }
    }

    const NOW: u64 = 1_754_431_200_000;
    /// Large enough that no test below accidentally exercises the entry
    /// budget when it means to be testing something else (age, count limit,
    /// tie-breaking).
    const AMPLE_BUDGET: usize = 1_000_000;

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
            AMPLE_BUDGET,
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
            AMPLE_BUDGET,
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
            AMPLE_BUDGET,
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
            AMPLE_BUDGET,
            None,
            NOW,
        );
        assert_eq!(ranked, vec!["0000000000001-bbbb", "0000000000001-aaaa"]);
    }

    #[test]
    fn rank_applies_the_entry_budget_as_a_hard_stop() {
        let ranked = rank_shard_candidates(
            vec![
                candidate_with_entries("a", NOW - 2_000, 5),
                candidate_with_entries("b", NOW - 1_000, 5),
                candidate_with_entries("c", NOW - 3_000, 5),
            ],
            10,
            8,
            None,
            NOW,
        );
        // "b" (5) then "a" (5) crosses the budget of 8 at 10; "c" never runs.
        assert_eq!(ranked, vec!["b", "a"]);
    }

    #[test]
    fn rank_bounds_by_entry_count_not_shard_count_so_local_churn_does_not_evict_an_old_pack() {
        // 20 fresh, one-entry local shards plus one much older 500-entry
        // rollup pack. A shard-count-only cap of 20 would fill up on the
        // fresh shards alone and never even look at the pack — this proves
        // ranking now walks by accumulated entry count, so the pack survives
        // as long as the entry budget and shard-count limit both have room
        // for it.
        let mut candidates: Vec<ShardCandidate> = (0..20)
            .map(|i| candidate_with_entries(&format!("fresh-{i:02}"), NOW - i as u64, 1))
            .collect();
        candidates.push(candidate_with_entries("old-pack", NOW - 1_000_000, 500));

        let ranked = rank_shard_candidates(candidates, 25, 100, None, NOW);

        assert_eq!(ranked.len(), 21);
        assert!(
            ranked.contains(&"old-pack".to_string()),
            "the older, larger pack must not be evicted by 20 tiny local shards"
        );
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
            AMPLE_BUDGET,
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

    #[test]
    fn local_shard_candidates_for_weighs_entry_count_by_on_disk_shard_bytes() {
        let temp = tempfile::TempDir::new().unwrap();
        let paths = crate::shared::paths::open_shared_paths(temp.path()).unwrap();

        let small_dir = paths.snapshots_dir.join("0000000000001-small");
        let big_dir = paths.snapshots_dir.join("0000000000002-big");
        fs::create_dir_all(&small_dir).unwrap();
        fs::create_dir_all(&big_dir).unwrap();
        fs::write(small_dir.join("aaaa.bincode"), vec![0u8; 10]).unwrap();
        fs::write(big_dir.join("bbbb.bincode"), vec![0u8; 1000]).unwrap();
        // A `.merged` sidecar isn't a shard and must not count toward the size.
        fs::write(big_dir.join("bbbb.merged"), vec![0u8; 1000]).unwrap();

        let candidates = local_shard_candidates_for(&paths);
        let small = candidates
            .iter()
            .find(|c| c.key == "0000000000001-small")
            .unwrap();
        let big = candidates
            .iter()
            .find(|c| c.key == "0000000000002-big")
            .unwrap();
        assert_eq!(small.entry_count, 10);
        assert_eq!(big.entry_count, 1000);
    }
}
