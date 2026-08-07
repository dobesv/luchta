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

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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

/// Hard ceiling on `day_window`, applied by `bucket_keys_for` regardless of
/// caller input. `day_window` reaches here straight from an env var
/// (`LUCHTA_SHARED_CACHE_DAYS`) with no upper-bound check of its own, and
/// unlike the old discovery-based scheme — where an oversized value was inert,
/// just a higher cap on top of a byte budget that did the real limiting — it
/// now directly sizes a `Vec::with_capacity` allocation and multiplies into
/// `day_back as u64 * MS_PER_DAY`. 365 is far more than any real read window
/// needs; it exists to bound the blast radius of a bad input, not as a
/// realistic setting.
pub const MAX_SHARED_CACHE_DAY_WINDOW: usize = 365;

/// Floor on `day_window` applied at `SharedCache` construction (not here):
/// see `SharedCache`'s `day_window` field doc for why `write_bucket_key` needs
/// at least 2 days of read window to always be inside the read set.
pub const MIN_SHARED_CACHE_DAY_WINDOW: usize = 2;

const MS_PER_DAY: u64 = 24 * 60 * 60 * 1000;

/// `<YYYYMMDD>-<shard>` for the UTC day containing `day_unix_ms`.
#[must_use]
pub fn bucket_key(day_unix_ms: u64, shard: usize) -> String {
    let (year, month, day) = civil_from_days((day_unix_ms / MS_PER_DAY) as i64);
    format!("{year:04}{month:02}{day:02}-{shard:02}")
}

/// Every bucket key in the read window, newest day first.
///
/// `day_window` is clamped to `MAX_SHARED_CACHE_DAY_WINDOW` before it sizes
/// any allocation or multiplication. The loop also stops early if a day
/// computation repeats a previous one: `now_unix_ms.saturating_sub(...)`
/// floors at zero, so a `day_window` reaching back past the unix epoch (or a
/// pre-epoch `now_unix_ms`, which `now_unix_ms()` can report via its
/// `unwrap_or(0)`) would otherwise emit the same `19700101-*` keys
/// `SHARED_CACHE_SHARD_COUNT` times over for every day past the epoch.
#[must_use]
pub fn bucket_keys_for(now_unix_ms: u64, day_window: usize) -> Vec<String> {
    let day_window = day_window.min(MAX_SHARED_CACHE_DAY_WINDOW);
    let mut keys = Vec::with_capacity(day_window * SHARED_CACHE_SHARD_COUNT);
    let mut last_day_ms = None;
    for day_back in 0..day_window {
        let day_ms = now_unix_ms.saturating_sub(day_back as u64 * MS_PER_DAY);
        if last_day_ms == Some(day_ms) {
            break;
        }
        last_day_ms = Some(day_ms);
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
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Nonce for the current call: a per-process atomic counter mixed with the
/// pid, so concurrent processes and concurrent calls within one process both
/// get distinct values without any shared coordination.
fn current_nonce() -> u64 {
    mix_nonce(
        std::process::id() as u64,
        SHARD_NONCE.fetch_add(1, Ordering::Relaxed),
    )
}

/// Mix a pid and a counter into a nonce, pulled out as a pure function of its
/// two inputs so the shard distribution it produces can be tested directly
/// across a realistic pid range — the previous `counter ^ (pid << 16)` looked
/// fine under a "vary the counter" test, but every process calls
/// `current_nonce()` exactly once at `open()`, so `counter` is always `0` in
/// production and the nonce is always `pid << 16`. Since
/// `(1 << 16) % SHARED_CACHE_SHARD_COUNT == 4` and `gcd(4, 6) == 2`, that left
/// only 3 of the 6 shards ever reachable, for every pid, forever. A splitmix64
/// finalizer avoids this class of bug for any modulus, not just today's 6:
/// there's no bit range of the output that stays aligned with a small
/// modulus's factors the way a bare shift does.
fn mix_nonce(pid: u64, counter: u64) -> u64 {
    let mut z = pid
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(counter);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The bucket this process's current run writes to.
#[must_use]
pub fn current_write_bucket_key() -> String {
    write_bucket_key(now_unix_ms(), current_nonce())
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
    fn bucket_keys_for_near_the_epoch_never_duplicates_the_saturated_day() {
        // `now_unix_ms.saturating_sub(day_back * MS_PER_DAY)` floors at zero,
        // so a `day_window` reaching back past 1970-01-01 (or a pre-epoch
        // `now_unix_ms`, which `now_unix_ms()` can report via its
        // `unwrap_or(0)`) would otherwise repeat "19700101-*" once per day
        // past the epoch instead of stopping there.
        let one_day_after_epoch = MS_PER_DAY;
        let keys = bucket_keys_for(one_day_after_epoch, 5);

        // Only two distinct days are possible here (epoch day and the day
        // after), so the loop must stop after those instead of emitting
        // duplicates for the other three requested days.
        assert_eq!(keys.len(), 2 * SHARED_CACHE_SHARD_COUNT);
        let unique: std::collections::HashSet<&String> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len(), "no duplicate keys near the epoch");

        // A `now_unix_ms` of exactly zero (pre-epoch clock, or the epoch
        // itself) must not emit `SHARED_CACHE_SHARD_COUNT * day_window` copies
        // of the same day.
        let keys_at_epoch = bucket_keys_for(0, 5);
        assert_eq!(keys_at_epoch.len(), SHARED_CACHE_SHARD_COUNT);
    }

    #[test]
    fn bucket_keys_for_clamps_an_oversized_day_window() {
        // An env-supplied `day_window` has no upper bound of its own before
        // reaching here; `bucket_keys_for` is what has to stop an enormous
        // value from sizing an allocation or overflowing `day_back * MS_PER_DAY`.
        let keys = bucket_keys_for(NOW, usize::MAX);
        assert_eq!(
            keys.len(),
            MAX_SHARED_CACHE_DAY_WINDOW * SHARED_CACHE_SHARD_COUNT
        );
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
        // Characterizes `write_bucket_key`'s own modulo mapping in isolation:
        // varying the nonce input directly must reach every shard. This does
        // NOT exercise `current_nonce`/`mix_nonce` — see
        // `write_bucket_shards_spread_across_realistic_pids` below for that.
        let seen: std::collections::HashSet<String> = (0..(SHARED_CACHE_SHARD_COUNT as u64))
            .map(|nonce| write_bucket_key(NOW, nonce))
            .collect();
        assert_eq!(seen.len(), SHARED_CACHE_SHARD_COUNT);
    }

    #[test]
    fn write_bucket_shards_spread_across_realistic_pids() {
        // The production shape: one call per process, so `counter` is always
        // `0` — every process's shard is decided by `mix_nonce(pid, 0)`
        // alone. A prior version used `counter ^ (pid << 16)`; that passed
        // `write_bucket_shards_spread_across_all_shards` above (which varies
        // the nonce/counter directly) and would *also* have passed a test
        // that varied the counter with the pid fixed, since
        // `{(pid << 16) ^ c) % 6 for c in 0..6}` is the full `0..6` range.
        // What it never covered is many *pids* at a fixed counter of `0`:
        // `(pid << 16) % 6` is one of `{0, 2, 4}` for every pid, because
        // `65536 % 6 == 4` shares a factor of 2 with 6. This test varies pid
        // with counter pinned at zero, which is the only shape that
        // discriminates for that bug.
        let seen: std::collections::HashSet<u64> = (1u64..40_000)
            .map(|pid| mix_nonce(pid, 0) % SHARED_CACHE_SHARD_COUNT as u64)
            .collect();
        assert_eq!(
            seen.len(),
            SHARED_CACHE_SHARD_COUNT,
            "all {SHARED_CACHE_SHARD_COUNT} shards must be reachable across realistic pids, got {seen:?}"
        );
    }
}
