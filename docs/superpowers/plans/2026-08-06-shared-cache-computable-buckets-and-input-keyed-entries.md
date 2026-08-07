# Shared Cache: Computable Buckets and Input-Keyed Entries — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace recency-based shard discovery with computable date+shard bucket keys, fold the resolved input state into the cache key so multiple variants of a task can coexist, and keep hot entries alive by refreshing them on a cache hit.

**Architecture:** This supersedes Phase 2 of `2026-08-05-shared-cache-entry-meta-and-recency-discovery.md`. That plan replaced git-commit shard keys with per-invocation session keys discovered by listing and recency ranking. It works, but it gave up the one good property of the commit-key design — keys you can compute without asking the object store — and bought a listing on every build plus a rollup/ranking/budget apparatus to manage the resulting shard sprawl. Here, shard keys are derived from the UTC date and a shard number, so the read set is a fixed, computable list of `DAYS × SHARDS` keys fetched directly. That makes ranking, remote listing, rollup packs, and the byte budget all unnecessary, and they are deleted. Separately, the entry key gains a hash of the resolved inputs, turning fetch-then-validate into exact match and letting different source states of the same task coexist instead of fighting over one slot.

**Tech Stack:** Rust, `blake3`, `bincode` (v2 serde API), `zstd`, `tar`, rclone rcd daemon over unix socket, `cargo nextest`.

## Why this shape

Three observations drive it:

1. **Snapshot selection precision is nearly worthless.** Builds only run tasks whose inputs changed, so a package that changed cannot hit regardless of which snapshots you pull. Hits come from stable-but-widely-used packages, and those appear in essentially every snapshot. "Which shards do I pick?" — the question rollup packs, the byte budget, and the pressure trigger all exist to answer — barely affects hit rate. What matters is having *enough recent index*, cheaply.

2. **Computable keys beat discovered keys.** The commit-key design could fetch specific keys with no listing. Its fatal flaw was that the keys never matched across pull requests, not that they were computed. Date+shard keys keep the computation and fix the matching.

3. **The current key is incomplete, so lookup can't be exact.** `derive_input_key` covers the task definition, env, package deps, and dependency outputs — but not the package's own source. Two branches that change a package differently compute the same key, and `record.inputs` is compared only *after* fetching the meta. Worse, both write paths are first-writer-wins, so once a slot is taken the new state can never be stored until GC ages it out: a permanent miss plus a wasted fetch on every build.

## Global Constraints

- Run **both** suites, always:
  - `cargo nextest run --workspace --no-fail-fast` — baseline **1432 run, 1431 passed, 3 skipped** (plus the known flake below)
  - `LUCHTA_TEST_RCLONE=1 cargo nextest run -p luchta-cache --no-fail-fast` — baseline **276 passed, 0 failed, 0 skipped**
  The gated suite is env-only and CI never sets it. On the previous branch, 18 gated tests rotted undetected across three tasks because the default run reports them as "3 skipped". Do not repeat that.
- **Known flaky test — not yours, do not chase it:** `luchta-cli::run_continue_and_fail_fast_integration failure_shows_summary_without_old_message`. Investigated across ~150 runs; reproduces on byte-identical pre-branch code, correlates with the working directory rather than the code, proximate cause is `main.rs` resetting SIGPIPE to `SIG_DFL`. Filed separately. Note it and move on.
- `git stash` is not a baseline. To establish whether something is pre-existing, compare against a named earlier commit in a separate worktree.
- On-disk record formats use zstd-compressed bincode via `crate::serialization::bincode_config()` (fixed int encoding). `snapshot_bincode_config()` (standard) is for snapshots only. Do not mix them.
- All shared-cache file writes go through `atomic_write` or `streaming_atomic_write`.
- `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets` must be clean.
- **Windows is a live target** — `ci.yml` builds `windows-latest` and runs clippy with `-D warnings` (production targets, so `--lib` is in scope); `release.yaml` ships three `*-pc-windows-msvc` triples. Remote code is `#[cfg(unix)]` gated. To check the non-unix build locally without a Windows toolchain, copy the crate to a scratch worktree, substitute the cfg predicates (including the `cfg_attr` forms — see Task 1 Step 1 for the exact sed) and run `cargo check -p luchta-cache --lib`. This technique found a real CI-breaking warning on the previous branch in about 90 seconds.
- Commit with a **lowercase imperative** subject. **No AI attribution footers** — no "Co-Authored-By", no "Generated with". This repo forbids them.

## Compatibility rules that bind the whole plan

- **`SHARED_CACHE_SHARD_COUNT` is a wire-compatibility constant, not a tunable.** If one machine writes with 12 shards and another reads 6, the reader silently misses everything in shards 6–11. Decreasing is safe; increasing is not. It must not be env-configurable. Changing it is a coordinated fleet-wide change tied to a schema bump.
- **The day window may be env-configurable.** Reading more or fewer days changes only local breadth and cannot desynchronise machines.
- **Dates are UTC.** Machines in different zones would otherwise compute different keys around midnight.
- **Input patterns must come from the task definition, not the record.** An inputs hash is undefined if you need the record to know which patterns to hash — that is circular. `dispatch.rs` hardcodes `detected_input_patterns: false` today, so this holds; Task 4 makes the dependency explicit rather than accidental.

## Migration

Changing both the shard key format and the entry key derivation makes every existing shared-cache object unreachable: old `<commit>` and `<unix_ms>-<nonce>` shard directories are simply never in the computed read set, and old `entries/<key>.bin` objects sit at keys nothing will ask for. Both age out through the existing GC. This is a clean, one-time cache reset with no dual-read path, and it is acceptable because nothing has shipped. Task 6 documents it.

---

## File Structure

**Modified:**

- `crates/luchta-cache/src/shared/discovery.rs` — loses recency ranking entirely; becomes bucket-key computation (`bucket_keys_for`, `write_bucket_key`). One responsibility: deciding which shard keys exist, arithmetically.
- `crates/luchta-cache/src/shared/mod.rs` — `candidate_keys` computes instead of discovers; `stage_entry` and `try_shared_cache_skip` gain the inputs hash; new refresh-on-hit entry point.
- `crates/luchta-cache/src/shared/snapshot.rs` — `derive_input_key` gains an inputs-hash parameter; `write_rollup_shard` deleted.
- `crates/luchta-cache/src/shared/remote.rs` — `list_shard_candidates`, `snapshots_root_fs`, and the rollup push path deleted.
- `crates/luchta-cache/src/shared/rclone/mod.rs` — `Entry::mod_time` deleted (added for ranking, never read).
- `crates/luchta-cache/src/shared/gc.rs` — rollup marker and `should_run_rollup` deleted.
- `crates/luchta-cache/src/resolve.rs` — add `combined_inputs_hash`.
- `crates/luchta-cli/src/run/dispatch.rs` — resolve inputs before the shared lookup; refresh on hit.
- `crates/luchta-cli/src/run/setup.rs` — `LUCHTA_SHARED_CACHE_HISTORY` → `LUCHTA_SHARED_CACHE_DAYS`, via `non_zero_env_u64_or`.
- `crates/luchta-cli/tests/shared_cache_discovery_e2e.rs` — branch-name portability.
- `README.md` — the shared-cache section currently documents the deleted commit-keyed design.
- `.github/workflows/ci.yml` — add an rclone job.

**New:** none. This plan is net deletion plus two focused additions.

---

## Task 1: Unblock CI and close the standalone review findings

These are small, independent, and survive the rework. Doing them first keeps the branch from sitting red while the larger changes land.

**Files:**
- Modify: `crates/luchta-cache/src/shared/mod.rs` (two cfg warnings)
- Modify: `crates/luchta-cli/src/run/setup.rs:295-301`
- Modify: `crates/luchta-cli/tests/shared_cache_discovery_e2e.rs:76`

**Interfaces:**
- Consumes: nothing.
- Produces: no API change.

- [ ] **Step 1: Reproduce the Windows failure locally**

Copy the crate into a scratch worktree, substitute the cfg predicates, and compile:

```bash
WT=$(mktemp -d)
git worktree add -q --detach "$WT" HEAD
cd "$WT"
grep -rl 'unix' crates/luchta-cache/src | xargs sed -i \
  -e 's/cfg_attr(not(unix),/cfg_attr(all(),/g' \
  -e 's/cfg(not(unix))/cfg(all())/g' \
  -e 's/cfg_attr(unix,/cfg_attr(any(),/g' \
  -e 's/cfg(unix)/cfg(any())/g'
cargo check -p luchta-cache --lib
```

Expected: two warnings — an unused `input_key` in `finish_store`, and an unnecessary `mut` on `let mut candidates`. `ci.yml` runs clippy with `-D warnings` on `windows-latest`, so these fail the build.

- [ ] **Step 2: Fix both**

`finish_store`'s `input_key` is only read inside the `#[cfg(unix)] self.enqueue_remote_push(...)` call. Gate the parameter the way its sibling already is — the same function already threads `#[cfg(unix)] meta.has_outputs` correctly, so follow that pattern:

```rust
    fn finish_store(
        &self,
        blob_result: BlobWriteResult,
        write_key: &str,
        #[cfg(unix)] input_key: &[u8; 32],
        #[cfg(unix)] has_outputs: bool,
        entry: SnapshotEntry,
    ) -> io::Result<StoreOutcome> {
```

and drop the argument from the non-unix call site.

For `let mut candidates`, whose only mutation is inside a `#[cfg(unix)]` block:

```rust
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut candidates = ...;
```

Note: Task 3 deletes `candidate_keys_with_remote` entirely, so that second fix is temporary. Do it anyway — the branch should not be red in the interim.

- [ ] **Step 3: Verify the non-unix build is clean**

Re-run the Step 1 substitution and `cargo check`. Expected: no warnings. Then remove the scratch worktree.

- [ ] **Step 4: Guard `LUCHTA_SHARED_CACHE_HISTORY` against zero**

`setup.rs:295-301` uses `parse_env_u64_or`. With the value `0`, `rank_shard_candidates` selects nothing and shared-cache reads are silently disabled. `non_zero_env_u64_or` exists 25 lines above at `setup.rs:268` for exactly this class of knob and is already used by the other numeric settings. Swap it in.

Add a test asserting `0` falls back to the default.

- [ ] **Step 5: Remove the hardcoded branch name**

`crates/luchta-cli/tests/shared_cache_discovery_e2e.rs:76` does `git checkout -b feature-two master`. `common::init_git` runs bare `git init`, so the branch name comes from `init.defaultBranch` — this fails anywhere that is configured to `main`. It is the only branch-name assumption in the CLI test suite.

Capture the base ref before diverging instead:

```rust
    let base = String::from_utf8(
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(temp.path())
            .output()
            .expect("rev-parse")
            .stdout,
    )
    .expect("utf8")
    .trim()
    .to_string();
```

then branch from `&base` instead of `"master"`.

- [ ] **Step 6: Run both suites and commit**

```bash
cargo nextest run --workspace --no-fail-fast
LUCHTA_TEST_RCLONE=1 cargo nextest run -p luchta-cache --no-fail-fast
git add -A && git commit -m "fix non-unix build warnings and test portability"
```

---

## Task 2: Computable date+shard bucket keys

**Files:**
- Modify: `crates/luchta-cache/src/shared/discovery.rs`
- Modify: `crates/luchta-cache/src/shared/mod.rs` — `candidate_keys`, construction
- Test: inline tests in `discovery.rs`; integration tests in `mod.rs`

**Interfaces:**
- Consumes: `SharedCachePaths`.
- Produces:
  - `pub const SHARED_CACHE_SHARD_COUNT: usize = 6;`
  - `pub const DEFAULT_SHARED_CACHE_DAY_WINDOW: usize = 3;`
  - `pub fn bucket_key(day_unix_ms: u64, shard: usize) -> String` — `"{YYYYMMDD}-{shard:02}"`, UTC
  - `pub fn bucket_keys_for(now_unix_ms: u64, day_window: usize) -> Vec<String>` — newest day first, all shards per day, length `day_window * SHARED_CACHE_SHARD_COUNT`
  - `pub fn write_bucket_key(now_unix_ms: u64, nonce: u64) -> String` — today's date, shard `nonce % SHARED_CACHE_SHARD_COUNT`

**Design notes for the implementer:**

- Compute the UTC date from `now_unix_ms` arithmetically (days since epoch → civil date). Do not add a date library for this; `time` is not a dependency of `luchta-cache` and adding one for a `YYYYMMDD` string is not worth it. The civil-from-days algorithm is short and testable.
- The write bucket must be one of the read buckets — it is today's date, and all of today's shards are in the read set, so this holds by construction. Add a test asserting it, because a future change to the window could silently break it.
- Shard selection for writing should be uniform. Reuse the existing session nonce (process id XOR an atomic counter) rather than inventing new entropy.

- [ ] **Step 1: Write the failing tests**

Replace the contents of `discovery.rs`'s test module with:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // 2026-08-06T10:00:00Z
    const NOW: u64 = 1_785_060_000_000;

    #[test]
    fn bucket_key_is_utc_date_plus_zero_padded_shard() {
        assert_eq!(bucket_key(NOW, 3), "20260806-03");
        assert_eq!(bucket_key(NOW, 0), "20260806-00");
    }

    #[test]
    fn bucket_key_rolls_over_at_utc_midnight_not_local() {
        // 2026-08-06T23:59:59Z and 2026-08-07T00:00:00Z
        let before = 1_785_110_399_000;
        let after = 1_785_110_400_000;
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
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p luchta-cache discovery::`
Expected: compile error — `bucket_key`, `bucket_keys_for`, `write_bucket_key`, `SHARED_CACHE_SHARD_COUNT` not found.

- [ ] **Step 3: Implement bucket-key computation**

Replace the body of `discovery.rs` (keep the module doc, rewritten) with:

```rust
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

use std::time::{SystemTime, UNIX_EPOCH};

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
    bucket_key(now_unix_ms, (nonce % SHARED_CACHE_SHARD_COUNT as u64) as usize)
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
```

Keep `current_session_shard_key`'s nonce generation if it is still needed for the write shard; otherwise fold it in here.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p luchta-cache discovery::`
Expected: PASS, 6 tests.

- [ ] **Step 5: Wire `candidate_keys` to compute instead of discover**

In `shared/mod.rs`, replace `candidate_keys`'s body so it returns `bucket_keys_for(now_unix_ms(), self.day_window)`, and set `write_commit_key` from `write_bucket_key`. Rename the field to `write_bucket_key` and `history_len` to `day_window` while you are here — the old names now describe nothing.

Delete the write-key injection: with computed buckets the write bucket is already in the read set by construction, so the special case is dead. The Task 2 test `write_bucket_is_always_inside_the_read_set` is what guards that property now.

Leave `rank_shard_candidates` and the rest of the old apparatus in place but unreferenced — Task 3 deletes them, and separating the two keeps this diff about the key scheme.

- [ ] **Step 6: Add an integration test for bucket accumulation**

In `shared/mod.rs`'s test module, prove two separate `SharedCache` instances (simulating two runs) writing the same bucket both end up discoverable:

```rust
    #[test]
    fn entries_written_by_separate_runs_are_both_found_by_a_later_run() {
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());
        let temp_cache = TempDir::new().unwrap();

        let package_dir = temp_repo.path().join("pkg");
        fs::create_dir_all(&package_dir).unwrap();

        // Two SharedCache instances over one cache dir, as two separate
        // `luchta run` invocations would be. Each picks its own write shard.
        let key_a = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);
        let key_b = derive_input_key([5; 32], [6; 32], [7; 32], [8; 32]);

        for (task, key, spec) in [("pkg#a", key_a, [1u8; 32]), ("pkg#b", key_b, [5u8; 32])] {
            let cache = SharedCache::open_with_cache_dir(
                temp_repo.path(), 1_000_000, 3, Some(temp_cache.path()),
            )
            .unwrap();
            let mut record = sample_record(true, 200);
            record.output_patterns = vec![];
            record.outputs = vec![];
            record.outputs_hash = crate::resolve::combined_outputs_hash(&[]);
            record.task_spec_hash = spec;
            cache
                .store(task, &key, &record.outputs_hash, &package_dir, &[], &record,
                       b"out", b"", &[], temp_repo.path())
                .unwrap();
        }

        // A third instance must see both, regardless of which shards they landed in.
        let reader = SharedCache::open_with_cache_dir(
            temp_repo.path(), 1_000_000, 3, Some(temp_cache.path()),
        )
        .unwrap();
        let restore_dir = temp_repo.path().join("restore");
        fs::create_dir_all(&restore_dir).unwrap();

        assert!(
            reader.try_restore_candidates("pkg#a", &key_a, &restore_dir).next().is_some(),
            "entry from the first run must be discoverable"
        );
        assert!(
            reader.try_restore_candidates("pkg#b", &key_b, &restore_dir).next().is_some(),
            "entry from the second run must be discoverable"
        );
    }
```

Adjust the constructor and `store` argument shapes to whatever they actually are at this point in the branch — check before writing rather than trusting these signatures.

- [ ] **Step 7: Run both suites and commit**

Expect failures in tests that assert on session-key shapes; update those tests to the new key format, but do not weaken their assertions. Report any test whose *intent* no longer applies rather than deleting it.

```bash
git commit -m "compute shard bucket keys from the utc date instead of discovering them"
```

---

## Task 3: Delete the recency, listing, and rollup apparatus

Pure deletion. Everything here exists to answer "which shards?", which Task 2 made arithmetic.

**Files:**
- Modify: `crates/luchta-cache/src/shared/discovery.rs` — remove `ShardCandidate`, `rank_shard_candidates`, `local_shard_candidates_for`, `discover_recent_shard_keys`, `DEFAULT_SHARD_MAX_AGE_MS`, `DEFAULT_SHARD_BYTE_BUDGET`, `rollup_pressure_threshold`
- Modify: `crates/luchta-cache/src/shared/remote.rs` — remove `list_shard_candidates`, `snapshots_root_fs`, `remote_approx_bytes`, `enqueue_push_snapshot_upload` and its `PushMsg` variant
- Modify: `crates/luchta-cache/src/shared/snapshot.rs` — remove `write_rollup_shard`
- Modify: `crates/luchta-cache/src/shared/gc.rs` — remove `should_run_rollup`, `rollup_marker_modified_unix_ms`, `ROLLUP_MARKER_NAME`, `rollup_pressure_threshold`
- Modify: `crates/luchta-cache/src/shared/mod.rs` — remove `maybe_write_rollup`, `maybe_push_rollup_upload`, `candidate_keys_with_remote`
- Modify: `crates/luchta-cache/src/shared/rclone/mod.rs` — remove `Entry::mod_time`

- [ ] **Step 1: Delete, compiler-guided**

Remove the items above and follow the compiler. Delete tests that exercised deleted behavior — ranking order, byte budget, rollup firing, pressure reset, remote listing. Those test real behavior that no longer exists; keeping them adapted would be testing nothing.

**Do not delete** `entries_from_separate_runs_are_both_discoverable` or `dirty_tree_entry_is_not_reused_by_clean_build` — they assert properties that survive. If they reference removed helpers, adapt them.

- [ ] **Step 2: Confirm nothing remains**

```bash
rg -n "rank_shard_candidates|ShardCandidate|approx_bytes|byte_budget|rollup|ROLLUP|mod_time|list_shard_candidates" crates/
```
Expected: no hits outside comments describing history.

- [ ] **Step 3: Verify the non-unix build**

Use the cfg-substitution technique from Task 1 Step 1. Deleting `#[cfg(unix)]` code is exactly where imbalance appears.

- [ ] **Step 4: Run both suites and commit**

```bash
git commit -m "delete shard ranking, remote listing, and rollup packs"
```

---

## Task 4: Fold the resolved inputs into the cache key

**Files:**
- Modify: `crates/luchta-cache/src/resolve.rs` — add `combined_inputs_hash`
- Modify: `crates/luchta-cache/src/shared/snapshot.rs:545` — `derive_input_key` gains a fifth hash
- Modify: `crates/luchta-cli/src/run/dispatch.rs` — resolve inputs before the shared lookup
- Modify: `crates/luchta-cache/src/decide.rs` — `decide_shared_restore`'s inputs check becomes a safety net

**Interfaces:**
- Produces: `pub fn combined_inputs_hash(entries: &[FileEntry]) -> [u8; 32]` and
  `derive_input_key(task_spec_hash, env_hash, pkg_dep_hash, dep_outputs_hash, inputs_hash) -> [u8; 32]`

**The decision-path change, in detail.** `build_cache_decision_context` (`dispatch.rs:1017`) calls `decide(local_record, current)` and only reaches the shared lookup when that returns `Run`. `decide()` short-circuits before touching the filesystem in two common cases: no local record (`decide.rs:57`) and a changed dependency (`decide.rs:85`, checked before `check_patterns_unchanged`). So inputs are frequently unresolved when the shared lookup begins.

Today the shared path resolves them anyway, inside `decide_shared_restore` → `patterns_unchanged`, using the *fetched* record as the mtime prior. On a fresh CI checkout every `mtime_ns` differs, so the fast path at `resolve.rs:119-122` never fires and every input is hashed regardless — after a network round trip. Moving the hash before the lookup costs the same work and skips the fetch entirely on a miss, which is the common case on CI.

The one place this could regress is a warm developer machine where the fetched record's mtimes *would* have matched. Avoid it by using the **local** record as the mtime prior for the up-front resolve: `ctx.cache.read(&task_id.to_string())` is already called at `dispatch.rs:1017` and is keyed by task id, so it is available even when the shared lookup is the one that matters.

- [ ] **Step 1: Write the failing tests**

In `resolve.rs`:

```rust
    #[test]
    fn combined_inputs_hash_changes_when_any_input_content_changes() {
        let base = vec![entry("src/a.ts", [1; 32]), entry("src/b.ts", [2; 32])];
        let changed = vec![entry("src/a.ts", [1; 32]), entry("src/b.ts", [9; 32])];
        assert_ne!(combined_inputs_hash(&base), combined_inputs_hash(&changed));
    }

    #[test]
    fn combined_inputs_hash_is_order_stable() {
        let a = vec![entry("src/a.ts", [1; 32]), entry("src/b.ts", [2; 32])];
        let b = vec![entry("src/b.ts", [2; 32]), entry("src/a.ts", [1; 32])];
        assert_eq!(combined_inputs_hash(&a), combined_inputs_hash(&b));
    }
```

Write the `entry` helper against the real `FileEntry` shape.

In `shared/mod.rs`, the test that actually matters — the one this whole task exists for:

```rust
    #[test]
    fn two_source_states_of_one_task_each_get_their_own_entry() {
        // Before inputs were part of the key both states computed the SAME
        // input_key, the first writer took the slot, and the second could
        // never be stored (ConflictKeptExisting) — a permanent miss plus a
        // wasted meta fetch on every build until GC aged the entry out.
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());
        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(), 1_000_000, 3, Some(temp_cache.path()),
        )
        .unwrap();

        let package_dir = temp_repo.path().join("pkg");
        fs::create_dir_all(&package_dir).unwrap();
        let empty_outputs = crate::resolve::combined_outputs_hash(&[]);

        // Same task, same env, same deps — only the input CONTENT differs.
        let inputs_a = crate::resolve::combined_inputs_hash(&[FileEntry {
            path: "src/main.ts".to_string(), size: 10, mtime_ns: 0, hash: [0xAA; 32], absent: false,
        }]);
        let inputs_b = crate::resolve::combined_inputs_hash(&[FileEntry {
            path: "src/main.ts".to_string(), size: 10, mtime_ns: 0, hash: [0xBB; 32], absent: false,
        }]);
        assert_ne!(inputs_a, inputs_b, "fixture must model two distinct source states");

        let key_a = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], inputs_a);
        let key_b = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], inputs_b);
        assert_ne!(key_a, key_b, "distinct inputs must yield distinct cache keys");

        for (key, marker) in [(key_a, &b"variant-a"[..]), (key_b, &b"variant-b"[..])] {
            let mut record = sample_record(true, 200);
            record.output_patterns = vec![];
            record.outputs = vec![];
            record.outputs_hash = empty_outputs;
            let outcome = cache
                .store("pkg#build", &key, &empty_outputs, &package_dir, &[], &record,
                       marker, b"", &[], temp_repo.path())
                .unwrap();
            assert_eq!(outcome, StoreOutcome::Stored, "both variants must store");
        }

        // Each key resolves to its own meta — neither evicted the other.
        assert_eq!(read_entry_meta(cache.paths(), &key_a).unwrap().stdout, b"variant-a");
        assert_eq!(read_entry_meta(cache.paths(), &key_b).unwrap().stdout, b"variant-b");
    }
```

Adjust the `store` argument shape and `FileEntry` field list to what they actually are — verify before writing.

- [ ] **Step 2: Run to verify failure**

Expected: compile error for `combined_inputs_hash`; the two-source-state test fails on the second store being rejected.

- [ ] **Step 3: Implement**

`combined_inputs_hash` mirrors `combined_outputs_hash` (`resolve.rs:565`) exactly — same sort, same length prefix, same per-entry framing — with its own domain string (`b"luchta-cache:combined-inputs:v1"`). Do not reuse the outputs domain; distinct domains for distinct meanings.

Extend `derive_input_key` with a fifth `inputs_hash: [u8; 32]` fed into the same hasher.

In `try_shared_cache_skip`, resolve inputs before computing the key, passing the local record's `inputs` as the mtime prior. On resolve failure, return `Decision::Run` — never a restore.

- [ ] **Step 4: Reduce `decide_shared_restore`'s inputs check to a safety net**

An exact key match now implies the inputs matched, because the hash covers the same `FileEntry` list `files_changed` compares. Keep the check — cheap, and it still catches a resolve error — but update its doc comment to say it is a defence-in-depth assertion rather than the discriminator. Do not delete it: the "resolve failed ⇒ do not restore" path is load-bearing.

- [ ] **Step 5: Verify the first-writer-wins conflict is now benign**

With inputs in the key, two writers to one key have identical inputs, so `ConflictKeptExisting` (`snapshot.rs:205`) can no longer pin a stale variant. Add a test asserting that storing the same key twice with the same inputs is an idempotent noop rather than a conflict, and update the doc comment on that match arm.

- [ ] **Step 6: Run both suites and commit**

```bash
git commit -m "fold resolved input state into the shared cache key"
```

---

## Task 5: Refresh entries on a cache hit

Without this the day window is a trap. A stable package's entry is written once; three days later it falls out of the window, every build misses it, everyone rebuilds, and it is rewritten — a rebuild sawtooth on precisely the packages the cache exists to serve, worst for the ones that change least.

**Files:**
- Modify: `crates/luchta-cache/src/shared/mod.rs` — add `refresh_entry`
- Modify: `crates/luchta-cli/src/run/dispatch.rs` — call it on `Decision::SharedHit`

**Interfaces:**
- Produces: `pub fn SharedCache::refresh_entry(&self, input_key: &[u8; 32], entry: &SnapshotEntry)`

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn a_shared_hit_refreshes_the_entry_into_the_current_write_bucket() {
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());
        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(), 1_000_000, 3, Some(temp_cache.path()),
        )
        .unwrap();

        // Seed the entry into an OLDER bucket only, as a build two days ago would have.
        let stale_bucket = bucket_key(now_unix_ms() - 2 * 24 * 60 * 60 * 1000, 0);
        let write_bucket = cache.write_bucket_key().expect("write bucket").to_string();
        assert_ne!(stale_bucket, write_bucket, "fixture must seed a different bucket");

        let entry = sample_entry_with_seed(1, [7; 32]);
        let input_key = entry.input_key;
        cache.snapshot_store().merge_entry(&stale_bucket, entry.clone());
        assert!(
            cache.snapshot_store().load(&write_bucket).is_none(),
            "write bucket must start empty"
        );

        cache.refresh_entry(&input_key, &entry);

        let refreshed = cache
            .snapshot_store()
            .load(&write_bucket)
            .expect("refresh must write into the current bucket");
        assert!(
            refreshed.entries.contains_key(&input_key_hex(input_key)),
            "the refreshed entry must be present in today's bucket, so it survives \
             the day window moving past the bucket it was originally written to"
        );
    }

    #[test]
    fn a_shared_hit_advances_the_entry_meta_mtime() {
        // gc_entries_dir ages out entries/*.bin by mtime and a hit does not
        // rewrite the file, so without this an entry in active use has its
        // meta deleted while its index key stays live.
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());
        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(), 1_000_000, 3, Some(temp_cache.path()),
        )
        .unwrap();

        let input_key = [3u8; 32];
        let meta = EntryMeta {
            schema_version: ENTRY_META_SCHEMA_VERSION,
            outputs_hash: [7; 32],
            has_outputs: false,
            record: vec![1, 2, 3],
            stdout: Vec::new(),
            stderr: Vec::new(),
            reports: Vec::new(),
        };
        write_entry_meta(cache.paths(), &input_key, &meta).unwrap();

        let path = entry_meta_path(cache.paths(), &input_key);
        let backdated = SystemTime::now() - Duration::from_secs(60 * 60 * 24 * 10);
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(backdated)).unwrap();

        cache.refresh_entry(&input_key, &sample_entry_with_seed(1, [7; 32]));

        let after = fs::metadata(&path).unwrap().modified().unwrap();
        assert!(
            after > backdated,
            "a hit must advance the meta mtime so GC does not expire an entry in active use"
        );
    }
```

`write_bucket_key()` and `snapshot_store()` accessors may not exist yet — add them, or reach the same state through whatever the current API exposes. Do not widen visibility further than the tests need.

- [ ] **Step 2: Run to verify failure**

- [ ] **Step 3: Implement `refresh_entry`**

Merge the entry into the current write bucket via the existing `merge_entry_with_outcome`, and touch `entries/<key>.bin`'s mtime. Both operations must be best-effort: log at `debug:` and continue on failure. A refresh failure must never fail a build or turn a hit into a miss.

Only the index entry and an mtime are written — the blob and meta are content-addressed and unchanged, so this is cheap.

- [ ] **Step 4: Call it on the hit path**

In `try_shared_cache_skip`'s `Decision::SharedHit` arm (`dispatch.rs:~1140`), alongside `hydrate_local_cache` and `replay_logs`.

- [ ] **Step 5: Run both suites and commit**

```bash
git commit -m "refresh shared cache entries on a hit so hot entries survive the window"
```

---

## Task 6: Documentation, CI coverage, and the migration note

**Files:**
- Modify: `README.md` — shared-cache section
- Modify: `.github/workflows/ci.yml` — rclone job
- Modify: `crates/luchta-cli/src/run/setup.rs` — env var rename

- [ ] **Step 1: Rename the env var**

`LUCHTA_SHARED_CACHE_HISTORY` counted commits, which no longer exist. Rename to `LUCHTA_SHARED_CACHE_DAYS` with `DEFAULT_SHARED_CACHE_DAY_WINDOW`. Keep reading the old name for one release, warning to stderr when it is set.

- [ ] **Step 2: Rewrite the README shared-cache section**

The current text documents a design that no longer exists. At minimum, these are wrong today:

- `README.md:1030` "**Commit-Keyed:** Results are indexed by git commit hash."
- `README.md:1032` "**Read Window:** ... consults the last 20 commits (configurable)"
- `README.md:1038` `snapshots/<commit>/<shard_id>.bincode`
- `README.md:1051` "`LUCHTA_SHARED_CACHE_HISTORY` — Number of recent commits to check"
- `README.md:1070` "appends `blobs/` and `snapshots/` beneath this base"

That last one matters most operationally: `README.md:1082` tells operators "Remote GC is not managed by Luchta. Use S3 bucket lifecycle rules", and the prefix list they write those rules against omits `entries/`. An operator following the current README expires blobs and snapshots and lets `entries/` grow forever.

Document: the three prefixes (`blobs/`, `snapshots/`, `entries/`); the `<YYYYMMDD>-<shard>` bucket format and that date-prefixed keys make lifecycle rules straightforward; that shard count is fixed and why; that the day window is tunable; and the one-time cache reset this change causes.

- [ ] **Step 3: Add an rclone CI job**

The remote paths have real coverage — 276 gated tests — but CI never runs them, and on the previous branch that let 18 of them rot across three tasks before a manual bisect found it. The gating helper and the tests already exist; only the workflow step is missing.

Add a job installing rclone and running `LUCHTA_TEST_RCLONE=1 cargo nextest run -p luchta-cache`.

- [ ] **Step 4: Run both suites and commit**

```bash
git commit -m "document computable bucket keys and add rclone ci coverage"
```

---

## Verification

```bash
cargo nextest run --workspace --no-fail-fast
LUCHTA_TEST_RCLONE=1 cargo nextest run -p luchta-cache --no-fail-fast
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Plus the non-unix check from Task 1 Step 1.

Then, by hand against a real repo with `LUCHTA_SHARED_CACHE=1`:

1. Build on a branch, create a sibling branch off the trunk, wipe `.luchta/cache`, build again — expect `📥`.
2. Change a package's source, build, revert, build — expect a hit on the reverted state, proving both variants are cached rather than one having evicted the other.
3. Build, wait past the day boundary (or fake the clock), build again — expect the entry refreshed into the new day's bucket rather than a rebuild.
