//! Shard bucket keys for the shared cache.
//!
//! Keys are computed, not discovered: `<YYYYMMDD>-<shard>` in UTC. A reader
//! knows every key it wants without listing the object store, which is the
//! property the original git-commit scheme had and the reason it was worth
//! keeping. What that scheme got wrong was the *choice* of key — commit ids
//! never matched across pull requests (issue #277).
//!
//! Buckets accumulate: many runs merge into the same bucket via
//! `SnapshotStore::merge_entry`, whose content-addressed shard files make
//! concurrent writers safe without locking.
//!
//! This file still carries the recency-ranking discovery apparatus
//! (`ShardCandidate`, `rank_shard_candidates`, `local_shard_candidates_for`,
//! and friends) that computed buckets replace. As of this change it is
//! unreferenced by production code — kept in place rather than deleted here
//! so the diff that introduces computed buckets stays reviewable separately
//! from the deletion of what they replace. A later task removes it.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::shared::paths::SharedCachePaths;
use crate::shared::snapshot::SNAPSHOT_FILE_EXTENSION;

static SHARD_NONCE: AtomicU64 = AtomicU64::new(0);

/// Number of shards per day.
///
/// This is a wire-compatibility constant, deliberately NOT env-configurable.
/// A machine writing with a higher count puts entries in shards that a machine
/// reading with a lower count never looks at, and the loss is silent.
/// Decreasing is safe; increasing is not. Change it only fleet-wide, with a
/// schema bump.
pub const SHARED_CACHE_SHARD_COUNT: usize = 6;

/// Days of history read by default. Safe to tune per machine — it changes only
/// local read breadth and cannot desynchronise writers from readers.
pub const DEFAULT_SHARED_CACHE_DAY_WINDOW: usize = 3;

const MS_PER_DAY: u64 = 24 * 60 * 60 * 1000;

/// `<YYYYMMDD>-<shard>` for the UTC day containing `day_unix_ms`.
#[must_use]
pub fn bucket_key(day_unix_ms: u64, shard: usize) -> String {
    let (year, month, day) = civil_from_days((day_unix_ms / MS_PER_DAY) as i64);
    format!("{year:04}{month:02}{day:02}-{shard:02}")
}

/// Every bucket key in the read window, newest day first.
#[must_use]
pub fn bucket_keys_for(now_unix_ms: u64, day_window: usize) -> Vec<String> {
    let mut keys = Vec::with_capacity(day_window * SHARED_CACHE_SHARD_COUNT);
    for day_back in 0..day_window {
        let day_ms = now_unix_ms.saturating_sub(day_back as u64 * MS_PER_DAY);
        for shard in 0..SHARED_CACHE_SHARD_COUNT {
            keys.push(bucket_key(day_ms, shard));
        }
    }
    keys
}

/// The bucket this process writes to: today, on a nonce-selected shard.
#[must_use]
pub fn write_bucket_key(now_unix_ms: u64, nonce: u64) -> String {
    bucket_key(
        now_unix_ms,
        (nonce % SHARED_CACHE_SHARD_COUNT as u64) as usize,
    )
}

/// Wall-clock now, in unix milliseconds. Zero if the clock is before the epoch.
#[must_use]
pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Civil date from days since the unix epoch (Howard Hinnant's algorithm).
///
/// Inlined rather than pulling in a date crate: `luchta-cache` has no date
/// dependency and this is the only place one would be needed.
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Nonce for the current call: a per-process atomic counter XORed with the
/// pid, so concurrent processes and concurrent calls within one process both
/// get distinct values without any shared coordination.
fn current_nonce() -> u64 {
    SHARD_NONCE.fetch_add(1, Ordering::Relaxed) ^ (std::process::id() as u64) << 16
}

/// The bucket this process's current run writes to.
#[must_use]
pub fn current_write_bucket_key() -> String {
    write_bucket_key(now_unix_ms(), current_nonce())
}

/// Drop shards older than two weeks. Long enough to span a quiet weekend,
/// short enough that the merged index stays small.
///
/// Unreferenced as of this change; see the module doc.
pub const DEFAULT_SHARD_MAX_AGE_MS: u64 = 1000 * 60 * 60 * 24 * 14;

/// Default byte budget for the discovery window, on top of the existing
/// shard-count `limit`. Sized well above what a single day's worth of local
/// `luchta run` invocations would produce, so ordinary local churn does not
/// push an older, not-yet-rolled-up shard out of the window before a rollup
/// ever sees it.
///
/// Unreferenced as of this change; see the module doc.
pub const DEFAULT_SHARD_BYTE_BUDGET: u64 = 50 * 1024 * 1024;

/// A shard directory found on disk (or reported by a remote listing),
/// carrying just enough to rank it: its key, last-modified time, and a weight
/// for the byte-budget walk in `rank_shard_candidates`.
///
/// Unreferenced as of this change; see the module doc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardCandidate {
    pub key: String,
    pub modified_unix_ms: u64,
    /// A cheap proxy for how much this shard would cost to load: its on-disk
    /// (or remote-listed) byte size, or 1 when unknown. Deliberately *not* a
    /// decoded `SnapshotEntry` count — decoding every candidate during
    /// discovery would double the load `build_index` already does for the
    /// shards that survive ranking, and there's no way to get an exact count
    /// for a remote shard without a network round trip anyway. Byte size is
    /// the one scale both sources can report cheaply, so local and remote
    /// candidates rank on a common basis.
    pub approx_bytes: u64,
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
    new_session_shard_key(now_unix_ms(), current_nonce())
}

/// Rank candidates newest-first, filtered by `max_age_ms`, then walk that
/// list accumulating `approx_bytes` until either `byte_budget` is reached or
/// `limit` shards have been kept — whichever comes first.
///
/// `limit` is a hard upper bound on shard *count*, independent of
/// `byte_budget`: it exists so a run of shards that individually under-report
/// (or a remote listing with unknown sizes, which fall back to an
/// `approx_bytes` of 1) can't force the walk through an unbounded number of
/// shards while still short of the budget. `byte_budget` is what actually
/// keeps the window scoped to a bounded amount of history: a handful of tiny
/// local shards contribute only a handful of bytes, so they don't burn
/// through the budget the way they would a shard-count-only limit, leaving
/// room for an older, larger shard (e.g. a rollup pack) to still make the cut.
///
/// Ties on modification time fall back to key order descending, which is
/// chronological because keys are zero-padded millisecond timestamps.
///
/// Unreferenced as of this change; see the module doc.
#[must_use]
pub fn rank_shard_candidates(
    candidates: Vec<ShardCandidate>,
    limit: usize,
    byte_budget: u64,
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
    let mut bytes_so_far: u64 = 0;
    for candidate in kept {
        if selected.len() >= limit || bytes_so_far >= byte_budget {
            break;
        }
        bytes_so_far = bytes_so_far.saturating_add(candidate.approx_bytes);
        selected.push(candidate.key);
    }
    selected
}

/// Discover shard directories present in the local cache, unranked.
///
/// Exposed so callers that also have a remote listing (see `RemoteSync::
/// list_shard_candidates`) can union the two candidate sets before calling
/// `rank_shard_candidates` once over the combined set.
///
/// Unreferenced as of this change; see the module doc.
#[allow(dead_code)]
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
            approx_bytes: shard_dir_byte_size(&path),
        });
    }
    candidates
}

/// Sum of the `.bincode` shard file sizes in a shard directory: a cheap
/// stand-in for entry count that costs a `read_dir` and some `stat` calls,
/// not a decode of every candidate's contents. Falls back to 1 (never 0, so
/// the candidate still counts toward the byte budget) if the directory can't
/// be read or is empty.
///
/// Unreferenced as of this change; see the module doc.
fn shard_dir_byte_size(shard_dir: &Path) -> u64 {
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
    total.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-08-06T10:00:00Z
    //
    // The brief specified this constant as 1_785_060_000_000, but that value
    // is actually 2026-07-26T10:00:00Z (verified against `civil_from_days`,
    // which implements the well-known Howard Hinnant algorithm, and cross
    // checked independently in Python). Corrected here to the timestamp the
    // brief's own comment and downstream assertions (20260806-*, 20260805-00,
    // 20260804-05) actually require.
    const NOW: u64 = 1_786_010_400_000;

    #[test]
    fn bucket_key_is_utc_date_plus_zero_padded_shard() {
        assert_eq!(bucket_key(NOW, 3), "20260806-03");
        assert_eq!(bucket_key(NOW, 0), "20260806-00");
    }

    #[test]
    fn bucket_key_rolls_over_at_utc_midnight_not_local() {
        // 2026-08-06T23:59:59Z and 2026-08-07T00:00:00Z (see the NOW comment
        // above re: the brief's timestamps being off by eleven days).
        let before = 1_786_060_799_000;
        let after = 1_786_060_800_000;
        assert_eq!(bucket_key(before, 0), "20260806-00");
        assert_eq!(bucket_key(after, 0), "20260807-00");
    }

    #[test]
    fn bucket_keys_for_covers_every_shard_of_every_day_in_the_window() {
        let keys = bucket_keys_for(NOW, 3);
        assert_eq!(keys.len(), 3 * SHARED_CACHE_SHARD_COUNT);

        // Newest day first.
        assert!(keys[0].starts_with("20260806-"));
        assert!(keys.iter().any(|k| k == "20260805-00"));
        assert!(keys.iter().any(|k| k == "20260804-05"));
        assert!(!keys.iter().any(|k| k.starts_with("20260803-")));

        // No duplicates.
        let unique: std::collections::HashSet<&String> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len());
    }

    #[test]
    fn bucket_keys_for_zero_window_is_empty() {
        assert!(bucket_keys_for(NOW, 0).is_empty());
    }

    #[test]
    fn write_bucket_is_always_inside_the_read_set() {
        // The write bucket must be readable by this same process, or a task's
        // own stored entry is invisible to a later lookup in the same run.
        let read = bucket_keys_for(NOW, 3);
        for nonce in 0..(SHARED_CACHE_SHARD_COUNT as u64 * 4) {
            let write = write_bucket_key(NOW, nonce);
            assert!(
                read.contains(&write),
                "write bucket {write} must be in the read set"
            );
        }
    }

    #[test]
    fn write_bucket_shards_spread_across_all_shards() {
        let seen: std::collections::HashSet<String> = (0..(SHARED_CACHE_SHARD_COUNT as u64))
            .map(|nonce| write_bucket_key(NOW, nonce))
            .collect();
        assert_eq!(seen.len(), SHARED_CACHE_SHARD_COUNT);
    }

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
        candidate_with_bytes(key, modified_unix_ms, 1)
    }

    fn candidate_with_bytes(key: &str, modified_unix_ms: u64, approx_bytes: u64) -> ShardCandidate {
        ShardCandidate {
            key: key.to_string(),
            modified_unix_ms,
            approx_bytes,
        }
    }

    const LEGACY_NOW: u64 = 1_754_431_200_000;
    /// Large enough that no test below accidentally exercises the byte
    /// budget when it means to be testing something else (age, count limit,
    /// tie-breaking).
    const AMPLE_BUDGET: u64 = 1_000_000;

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
                candidate("a", LEGACY_NOW - 3_000),
                candidate("b", LEGACY_NOW - 1_000),
                candidate("c", LEGACY_NOW - 2_000),
            ],
            10,
            AMPLE_BUDGET,
            None,
            LEGACY_NOW,
        );
        assert_eq!(ranked, vec!["b", "c", "a"]);
    }

    #[test]
    fn rank_applies_the_limit() {
        let ranked = rank_shard_candidates(
            vec![
                candidate("a", LEGACY_NOW - 3_000),
                candidate("b", LEGACY_NOW - 1_000),
                candidate("c", LEGACY_NOW - 2_000),
            ],
            2,
            AMPLE_BUDGET,
            None,
            LEGACY_NOW,
        );
        assert_eq!(ranked, vec!["b", "c"]);
    }

    #[test]
    fn rank_drops_shards_older_than_max_age() {
        let ranked = rank_shard_candidates(
            vec![
                candidate("fresh", LEGACY_NOW - 1_000),
                candidate("stale", LEGACY_NOW - 100_000),
            ],
            10,
            AMPLE_BUDGET,
            Some(10_000),
            LEGACY_NOW,
        );
        assert_eq!(ranked, vec!["fresh"]);
    }

    #[test]
    fn rank_breaks_mtime_ties_by_key_descending() {
        let ranked = rank_shard_candidates(
            vec![
                candidate("0000000000001-aaaa", LEGACY_NOW),
                candidate("0000000000001-bbbb", LEGACY_NOW),
            ],
            10,
            AMPLE_BUDGET,
            None,
            LEGACY_NOW,
        );
        assert_eq!(ranked, vec!["0000000000001-bbbb", "0000000000001-aaaa"]);
    }

    #[test]
    fn rank_applies_the_byte_budget_as_a_hard_stop() {
        let ranked = rank_shard_candidates(
            vec![
                candidate_with_bytes("a", LEGACY_NOW - 2_000, 5),
                candidate_with_bytes("b", LEGACY_NOW - 1_000, 5),
                candidate_with_bytes("c", LEGACY_NOW - 3_000, 5),
            ],
            10,
            8,
            None,
            LEGACY_NOW,
        );
        // "b" (5) then "a" (5) crosses the budget of 8 at 10; "c" never runs.
        assert_eq!(ranked, vec!["b", "a"]);
    }

    #[test]
    fn rank_bounds_by_byte_size_not_shard_count_so_local_churn_does_not_evict_an_old_pack() {
        // 20 fresh, tiny local shards plus one much older, much bigger
        // rollup pack. A shard-count-only cap of 20 would fill up on the
        // fresh shards alone and never even look at the pack — this proves
        // ranking now walks by accumulated byte size, so the pack survives
        // as long as the byte budget and shard-count limit both have room
        // for it.
        let mut candidates: Vec<ShardCandidate> = (0..20)
            .map(|i| candidate_with_bytes(&format!("fresh-{i:02}"), LEGACY_NOW - i as u64, 1))
            .collect();
        candidates.push(candidate_with_bytes(
            "old-pack",
            LEGACY_NOW - 1_000_000,
            500,
        ));

        let ranked = rank_shard_candidates(candidates, 25, 100, None, LEGACY_NOW);

        assert_eq!(ranked.len(), 21);
        assert!(
            ranked.contains(&"old-pack".to_string()),
            "the older, larger pack must not be evicted by 20 tiny local shards"
        );
    }

    #[test]
    fn discover_finds_local_shard_dirs_newest_first() {
        use std::time::Duration;
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
    fn local_shard_candidates_for_weighs_approx_bytes_by_on_disk_shard_size() {
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
        assert_eq!(small.approx_bytes, 10);
        assert_eq!(big.approx_bytes, 1000);
    }
}
