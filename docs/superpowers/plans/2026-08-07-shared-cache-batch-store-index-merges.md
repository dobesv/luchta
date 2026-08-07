# Shared Cache: Batch Store-Side Index Merges — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop rewriting and re-uploading the whole day's shared-cache index shard once per stored task. Defer the index merge to a single flush per run, matching what refreshes already do.

**Architecture:** `SharedCache::store()` currently ends in `finish_store`, which calls `merge_entry_with_outcome` and then `enqueue_remote_push` — so a build with N cacheable tasks does N load-modify-write cycles over the day's shard and N remote pushes of the whole thing. Refreshes already solved this: `refresh_entry` records into a `Mutex<HashMap<[u8; 32], SnapshotEntry>>` and `flush_refreshes` does one `merge_entries_with_outcome` and one `enqueue_remote_push` at end of run. This plan routes stores through the same map. Because a given `input_key` cannot be both stored (after a miss) and refreshed (after a hit) in one run, both mechanisms share one map and one flush — one merge and one index push per run in total.

**Tech Stack:** Rust, `blake3`, `bincode` (v2 serde API), `zstd`, rclone rcd daemon over unix socket, `cargo nextest`.

## Why this is needed

GitHub issue #281. Under the old git-commit shard keys the write bucket held only the current build's entries, so the load-modify-write in `merge_entry_with_outcome` was quadratic but bounded by one build. Date-bucketed keys changed that: `build_index` → `pull_candidate_commits` copies all 18 candidate buckets down from the remote first, including today's, so the write bucket now holds the fleet's activity for that day in that shard.

A `SnapshotEntry` plus its hex key is roughly 240 bytes raw, ~100 compressed. At 25k entries/day fleet-wide that is ~4k entries per bucket and ~400 KB per shard write, so a 500-task build uploads ~200 MB. At 400k entries/day it is ~6.7 MB per write and tens of GB per build. The cost grows with adoption, which is exactly when the cache matters most, and it will trip the `timeout_disable` circuit breaker — the same self-defeating dynamic that made per-hit refresh pushes unworkable.

## The one structural change

`RemoteSync::push_store_artifacts` currently does three jobs in one call:

1. `push_blob_if_missing` — per entry, gated on `has_outputs`
2. `push_entry_meta_if_missing` — per entry
3. `push_snapshot_upload` plus deletion of subsumed shards — per merge

Only (3) can be deferred. (1) and (2) are per-entry and must stay immediate: the blob and `entries/<input_key>.bin` are content-addressed, independently useful, and a restore on another machine needs them whether or not this run's index push has happened yet. So the push path splits in two.

## What is deliberately NOT in scope

- **Dropping the `.merged` sidecar** (issue #284). It has no production reader, but it is a remote-layout change and belongs in its own diff. Batching already removes most of its aggregate cost by reducing merges from N per run to 1.
- **Not subsuming remote-pulled shards.** Considered and rejected: it caps write *size* but leaves the push *count* at N, and combined with batching it would grow one shard per bucket per run, so every reader would download and merge hundreds of shards. Subsuming is what keeps read cost bounded.

## Global Constraints

- Run **both** suites, always:
  - `cargo nextest run --workspace --no-fail-fast` — baseline **1433 run, 1433 passed, 3 skipped**
  - `LUCHTA_TEST_RCLONE=1 cargo nextest run -p luchta-cache --no-fail-fast` — baseline **272 passed, 0 failed, 0 skipped**
- **Kill stray rclone daemons before measuring the gated suite:** `pkill -f 'rclone rcd --rc-addr unix:///tmp/\.tmp'`. The suite leaks ~6 daemons per run (issue #283); accumulated daemons cause contention that looks exactly like a code regression. That confound produced two incorrect causal claims during earlier work.
- **Known flaky test, not yours:** `luchta-cli::run_continue_and_fail_fast_integration failure_shows_summary_without_old_message` (issue #282) — SIGPIPE kills the child before it prints its summary. Environment-correlated.
- `git stash` is not a baseline. To establish whether something is pre-existing, compare against a named earlier commit in a separate worktree.
- All shared-cache file writes go through `atomic_write` or `streaming_atomic_write`.
- `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets` must be clean.
- **Windows is a live target.** Remote code is `#[cfg(unix)]` gated and `ci.yml` runs clippy with `-D warnings` on `windows-latest`. Verify the non-unix build in a scratch worktree:

```bash
WT=$(mktemp -d); git worktree add -q --detach "$WT" HEAD; cd "$WT"
grep -rl 'unix' crates/luchta-cache/src | xargs sed -i \
  -e 's/cfg_attr(not(unix),/cfg_attr(all(),/g' -e 's/cfg(not(unix))/cfg(all())/g' \
  -e 's/cfg_attr(unix,/cfg_attr(any(),/g' -e 's/cfg(unix)/cfg(any())/g'
cargo check -p luchta-cache --lib
```

Expect zero warnings. Remove the worktree afterwards; never move HEAD on the main checkout.
- Commit with a **lowercase imperative** subject. **No AI attribution footers.** This repo forbids them.

## Invariants that must survive

- **Blob and entry-meta writes stay immediate and per-entry.** Only the index merge defers.
- **Remote failures never fail a build.** Every remote path logs at `debug:`/`warn:` and degrades to a miss, preserving the `record_remote_success` / `record_remote_error` bookkeeping that drives the timeout-disable breaker.
- **The flush is best-effort and infallible.** It runs after all tasks complete; nothing downstream depends on it.
- **Push-then-delete ordering.** A subsumed shard is deleted from the remote only after the replacement shard uploads successfully. Do not reorder.
- **`MergedIndex` stays a `HashSet<String>`.** No payload, comparator, or provenance.

---

## File Structure

**Modified:**

- `crates/luchta-cache/src/shared/mod.rs` — `store`/`finish_store` record instead of merging; `pending_refreshes` becomes a shared pending map; `flush_refreshes` becomes the one flush for both mechanisms; `enqueue_remote_push` splits into an artifact push and an index push.
- `crates/luchta-cache/src/shared/remote.rs` — `push_store_artifacts` splits; `OwnedPushArtifacts` / `PushArtifacts` split to match; a new `PushMsg` variant for the index push.
- `crates/luchta-cli/src/run.rs` — the flush call site's comment now covers stores as well as refreshes.
- `crates/luchta-cli/src/run/dispatch.rs` — only if `StoreOutcome` changes shape.

**New:** none.

---

## Task 1: Split the remote push into artifacts and index

Pure refactor, no behaviour change. Doing it first means Task 2 has the two halves it needs and can be judged on its own merits.

**Files:**
- Modify: `crates/luchta-cache/src/shared/remote.rs` — `push_store_artifacts`, `OwnedPushArtifacts`, `PushArtifacts`, `PushMsg`, `enqueue_push_store_artifacts`
- Modify: `crates/luchta-cache/src/shared/mod.rs` — `enqueue_remote_push`
- Test: inline tests in `remote.rs`

**Interfaces:**
- Consumes: `MergeEntryOutcome { result, new_snapshot_upload, subsumed_shard_ids }` from `snapshot.rs:54`.
- Produces:
  - `pub(crate) struct OwnedEntryArtifacts { paths: Arc<SharedCachePaths>, outputs_hash: [u8; 32], input_key: [u8; 32], has_outputs: bool }`
  - `pub(crate) struct OwnedIndexPush { paths: Arc<SharedCachePaths>, shard_key: String, merge: MergeEntryOutcome }`
  - `RemoteSync::push_entry_artifacts(&self, …)` — blob (when `has_outputs`) then entry meta
  - `RemoteSync::push_index_merge(&self, …)` — snapshot upload then subsumed deletes
  - `RemoteSync::enqueue_entry_artifacts(&self, OwnedEntryArtifacts)` and `RemoteSync::enqueue_index_push(&self, OwnedIndexPush)`
  - `PushMsg` gains variants for both; the existing `Flush` test variant is unchanged.

**Note on `paths`:** both halves need it — the artifact push reads `blob_path` / `entry_meta_path`, the index push reads nothing from disk but `push_snapshot_upload` takes the shard bytes from `merge.new_snapshot_upload`. Check whether the index half actually needs `paths` before including it; drop the field if not.

- [ ] **Step 1: Write the failing test**

Add to `remote.rs`'s test module, gated with `should_run_rclone_test()` like its neighbours:

```rust
    #[test]
    fn entry_artifacts_and_index_push_are_independently_dispatchable() {
        if !should_run_rclone_test() {
            return;
        }
        let remote_root = tempfile::tempdir().unwrap();
        let remote = RemoteSync::new(
            Arc::new(RcloneRcd::with_default_timeout().unwrap()),
            format!(":local:{}", remote_root.path().display()),
            8,
        );

        let harness = RemoteHarness::new("console.log('split');\n");
        let cache = harness.cache();
        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32], [5; 32]);

        // Write the artifacts locally so there is something to push.
        let package_dir = harness.temp_repo.path().join("pkg");
        fs::create_dir_all(&package_dir).unwrap();
        let empty_outputs = crate::resolve::combined_outputs_hash(&[]);
        let mut record = sample_record(true, 200);
        record.output_patterns = vec![];
        record.outputs = vec![];
        record.outputs_hash = empty_outputs;
        cache
            .store("pkg#build", &input_key, &empty_outputs, &package_dir, &[], &record,
                   b"out", b"", &[], harness.temp_repo.path())
            .unwrap();
        cache.flush_push_queue();

        // Entry artifacts reached the remote...
        assert!(
            remote_root.path().join("entries").read_dir().unwrap().count() > 0,
            "entry meta must be pushed by the artifact half"
        );
        // ...while the index shard did not, because no index push was enqueued.
        let snapshots = remote_root.path().join("snapshots");
        assert!(
            !snapshots.exists() || snapshots.read_dir().unwrap().count() == 0,
            "the index half must not run when only artifacts were enqueued"
        );
    }
```

Check `RemoteHarness`'s field visibility (`temp_repo` may be private), `open_cache_with_remote`'s signature, and the queue-drain helper before writing — these shapes have drifted across recent work. Adapt and say so in your report rather than forcing the snippet.

- [ ] **Step 2: Run the test to verify it fails**

Run: `pkill -f 'rclone rcd --rc-addr unix:///tmp/\.tmp'; LUCHTA_TEST_RCLONE=1 cargo nextest run -p luchta-cache remote::tests::entry_artifacts_and_index_push --no-fail-fast`
Expected: compile error — `enqueue_entry_artifacts` not found.

- [ ] **Step 3: Split the structs and the push function**

Replace `OwnedPushArtifacts` / `PushArtifacts` with the two structs above, and split `push_store_artifacts` at the seam that already exists in its body — the blob and entry-meta calls form the first half, the `new_snapshot_upload` handling and the subsumed-delete loop the second. Preserve the existing `is_disabled()` re-check between them inside `push_index_merge`, and keep the push-then-delete ordering exactly as it is.

Add the matching `PushMsg` variants and route them in the queue worker.

- [ ] **Step 4: Update the single caller**

`SharedCache::enqueue_remote_push` (`mod.rs:819`) becomes two methods: `enqueue_entry_artifacts` and `enqueue_index_push`. `finish_store` calls both, back to back, so this task changes no behaviour — Task 2 is what moves the second call to the flush.

- [ ] **Step 5: Run both suites and commit**

Both suites must match baseline exactly; this task is behaviour-preserving. Run the non-unix check — you are editing `#[cfg(unix)]` code.

```bash
git commit -m "split remote push into entry artifacts and index merge"
```

---

## Task 2: Route stores through the pending map

**Files:**
- Modify: `crates/luchta-cache/src/shared/mod.rs` — `finish_store`, the pending map, `flush_refreshes`
- Modify: `crates/luchta-cli/src/run.rs` — the flush call site comment
- Test: inline tests in `mod.rs`

**Interfaces:**
- Consumes: Task 1's `enqueue_entry_artifacts` / `enqueue_index_push`.
- Produces:
  - `pending_refreshes` renamed to `pending_entries` (it now serves both mechanisms)
  - `flush_refreshes` renamed to `flush_pending_entries`, with `flush_refreshes` kept as a deprecated alias only if an external caller needs it — check; if `run.rs` is the sole caller, rename outright.
  - `SharedCache::record_pending_entry(&self, input_key: &[u8; 32], entry: SnapshotEntry)` — the shared insert used by both `finish_store` and `refresh_entry`.

**Design notes:**

- **One map for both.** A given `input_key` cannot be both stored and refreshed in one run: a store follows a miss, a refresh follows a hit. If the same key somehow arrives twice, last-write-wins via `HashMap::insert` is correct — both entries describe the same content-addressed result.
- **`StoreOutcome::SkippedLockUnavailable` becomes unreachable from `store()`**, because the merge no longer happens there. Its only consumer is an empty match arm at `dispatch.rs:957` and one test assertion at `mod.rs:1506`. Decide deliberately: either remove the variant (and its arm and test) or keep it documented as flush-only. Removing is cleaner; state which you chose and why in your report.
- **The flush's representative-entry logic goes away for stores.** `flush_refreshes` currently picks one entry to size a best-effort blob/meta catch-up push, because a refreshed entry's artifacts were pushed by an earlier run. Stores push their own artifacts immediately in `finish_store`, so they need no catch-up. Keep the representative logic for the refresh case, and make sure a store-only flush does not do a spurious catch-up push for an unrelated entry.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn n_stores_produce_one_index_merge_not_n() {
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
        let write_bucket = cache.write_bucket_key().expect("write bucket").to_string();

        for seed in 0u8..5 {
            let mut record = sample_record(true, 200);
            record.output_patterns = vec![];
            record.outputs = vec![];
            record.outputs_hash = empty_outputs;
            record.task_spec_hash = [seed; 32];
            let key = derive_input_key([seed; 32], [2; 32], [3; 32], [4; 32], [5; 32]);
            cache
                .store("pkg#build", &key, &empty_outputs, &package_dir, &[], &record,
                       b"out", b"", &[], temp_repo.path())
                .unwrap();
        }

        // Nothing merged yet: the bucket has no shard until the flush.
        assert!(
            cache.snapshot_store().load(&write_bucket).is_none(),
            "stores must not merge into the index before the flush"
        );
        assert_eq!(cache.pending_entry_count(), 5, "all five entries pending");

        cache.flush_pending_entries();

        let snapshot = cache
            .snapshot_store()
            .load(&write_bucket)
            .expect("flush must write the bucket");
        assert_eq!(snapshot.entries.len(), 5, "one merge carrying all five entries");
    }
```

The `#[cfg(test)]` accessor is `pending_refresh_count` today; rename it to `pending_entry_count` alongside the field so the name still describes what it counts. Keep it `#[cfg(test)]` — do not widen it.

Also add a test that a store's **blob and entry meta are written immediately**, before any flush — that is the invariant most at risk from this change.

- [ ] **Step 2: Run to verify it fails**

Expected: the pre-flush assertion fails, because `finish_store` still merges eagerly.

- [ ] **Step 3: Make `finish_store` record instead of merge**

Replace the `merge_entry_with_outcome` + `enqueue_remote_push` pair with: `enqueue_entry_artifacts` (immediate, per entry) followed by `record_pending_entry`. Return `StoreOutcome::Stored`.

- [ ] **Step 4: Rename and extend the flush**

`pending_refreshes` → `pending_entries`, `flush_refreshes` → `flush_pending_entries`. Update the doc comment: it now explains that both stores and refreshes accumulate here, why one merge per run is the point, and why deferral is safe (the merged index is built once behind a `OnceLock`, so a mid-run merge was never visible to a later lookup in the same process).

- [ ] **Step 5: Update the call site comment in `run.rs`**

`run.rs:2366-2378` describes the flush as being about refreshes. It now covers stores too, and the reasoning for the call site (not `Drop`, exactly once per cycle regardless of outcome) still holds. Update the wording; do not move the call.

- [ ] **Step 6: Run both suites and commit**

Expect failures in tests that assert a shard exists immediately after `store()`. Update them to flush first — but do not weaken an assertion to make it pass. If a test's *intent* no longer applies, say so in your report and leave it failing rather than deleting it.

```bash
git commit -m "batch store-side index merges into the end-of-run flush"
```

---

## Task 3: Prove the amplification is gone, and document it

A test that counts merges is what stops this regressing. Without it, someone reinstates an eager merge and every suite still passes.

**Files:**
- Modify: `crates/luchta-cache/src/shared/mod.rs` — tests
- Modify: `README.md` — the shared-cache section's description of when writes happen
- Test: inline tests in `mod.rs`, plus a gated remote test in `remote.rs`

- [ ] **Step 1: Add a remote-side count test**

Assert that N stores against a remote-backed cache produce exactly **one** snapshot-shard upload, and N entry-meta uploads. Gate with `should_run_rclone_test()`.

The discriminating property is the shard-upload count. Verify it by reverting `finish_store` to an eager merge temporarily and confirming the count becomes N; report that output.

- [ ] **Step 2: Add a local-side ordering test**

Assert that after `store()` but *before* the flush: the blob exists, `entries/<input_key>.bin` exists, and the write bucket has no shard. That pins all three invariants at once — artifacts immediate, index deferred.

- [ ] **Step 3: Update the README**

The shared-cache section describes what happens on a store. It should now say the index shard is written once per run at the end rather than per task, and that blobs and entry metadata are written as each task completes. Check the existing wording against the code rather than assuming; the section was rewritten recently and may already be vague enough to need only a sentence.

- [ ] **Step 4: Run both suites, the non-unix check, and commit**

```bash
git commit -m "cover batched index merges and document write timing"
```

---

## Verification

```bash
pkill -f 'rclone rcd --rc-addr unix:///tmp/\.tmp'
cargo nextest run --workspace --no-fail-fast
LUCHTA_TEST_RCLONE=1 cargo nextest run -p luchta-cache --no-fail-fast
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Plus the non-unix cfg check from Global Constraints.

Then by hand, against a repo with `LUCHTA_SHARED_CACHE=rclone:<remote>` pointed at a scratch bucket or a `:local:` path:

1. Run a build with several cacheable tasks that all miss. Confirm exactly one object appears under `snapshots/<today>-<shard>/` rather than one per task, and that `blobs/` and `entries/` gain one object per task.
2. Re-run. Confirm the hits still restore, and that the second run's flush produces one more shard object at most.
