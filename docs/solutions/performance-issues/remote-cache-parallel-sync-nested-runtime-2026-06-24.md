---
title: "Remote cache parallel sync (30-60s startup) and nested tokio runtime panic"
date: 2026-06-24
category: performance-issues
problem_type: performance_issue
component: luchta-cache/shared
root_cause: triple-serialized remote ops + nested tokio block_on panic
resolution_type: code_fix
severity: high
tags:
  - remote-cache
  - rclone
  - concurrency
  - tokio
  - nested-runtime-panic
plan_ref: remote-cache-perf
---
# Remote cache parallel sync and nested tokio runtime panic

## Problem

Remote cache enabled → 30-60s startup latency. Root cause: three levels of serialization in `crates/luchta-cache/src/shared/`:
1. `SharedCache::build_index` (mod.rs) looped over candidate commit keys sequentially.
2. `RemoteSync::pull_snapshot_commit` (remote.rs) pulled each shard via sequential `operations/copyfile` loop (one rclone RC round-trip per shard).
3. `RcloneRcd::call` (rclone.rs) held `Mutex<State>` for entire request, ran on single-thread `current_thread` tokio runtime, spawning scoped OS thread per request.

Net: N commits × M shards × S3 RTT, fully serial → 30-60s.

Separate issue: initial concurrent-pull fix called `tokio::runtime::Runtime::block_on` on a thread already inside a tokio runtime → panic in production: *"Cannot start a runtime from within a runtime."*

## Symptoms

```
- Startup: 30-60s delay with remote cache enabled
- Panic (on buggy impl): "Cannot start a runtime from within a runtime. This happens because a function (like `block_on`) attempted to block the current thread while the thread is being used to drive asynchronous tasks."
- Unit tests passed; panic only in production async context
```

## Investigation Steps

1. Profiled startup: `build_index` dominated. Traced to sequential candidate commit loop.
2. Inspected `pull_snapshot_commit`: per-shard `copyfile` RC calls in `for` loop.
3. Inspected `RcloneRcd::call`: global `Mutex<State>` + `Builder::new_current_thread()` runtime.
4. Realized rclone rcd daemon handles concurrent requests fine — bottleneck was our serialization.
5. Implemented concurrent pull with fresh multi-thread runtime + `block_on` inside `build_index` → unit tests passed.
6. Production panic revealed call path: `dispatch_loop` (async) → `dispatch_ready_task` (direct call, not `spawn_blocking`) → `try_shared_cache_skip` → `SharedCache::try_restore_candidates` → `get_or_build_index` → `build_index`.
7. Discovered existing `RcloneRcd` already avoided nested runtime by running `block_on` on dedicated scoped OS thread.

## Root Cause

**Serialization:** Triple serialization at commit loop, shard loop, and global mutex + current-thread runtime meant no two S3 requests could be in flight simultaneously.

**Nested runtime panic:** The async dispatch path calls sync cache methods directly (not via `spawn_blocking`). Calling `block_on` on a thread with an ambient tokio runtime panics. Unit tests had no ambient runtime, so they didn't catch it.

## Solution

### 1. De-serialize rclone RC calls

**Before (rclone.rs):**
```rust
let rt = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()?;
// State mutex held for entire call
let state = self.state.lock().unwrap();
// ... single-threaded, one request at a time
```

**After:**
```rust
let rt = tokio::runtime::Builder::new_multi_thread()
    .enable_all()
    .build()?;
// Hold mutex ONLY to spawn daemon once + read socket path
let socket_path = {
    let state = self.state.lock().unwrap();
    state.ensure_daemon_spawned()?;
    state.socket_path.clone()
};
// RELEASE mutex BEFORE request
let client = Client::unix();
// Each request runs concurrently; rclone rcd handles parallelism
```

### 2. Replace per-shard copyfile with directory sync

**Before (remote.rs):**
```rust
for shard in shards {
    copy_remote_file_down(remote, shard).await?; // N×M RC calls
}
```

**After:**
```rust
// Single sync/copy of whole snapshot directory
rclone.sync_copy(remote_snapshot_dir, local_snapshot_dir).await?;
// sync/copy is COPY-ONLY (no delete), pulls .merged sidecars for free
// rclone parallelizes internally via --transfers
```

Collapses N×M calls to ~N (one per commit).

### 3. Pull commits concurrently with bounded JoinSet

```rust
let mut join_set = tokio::task::JoinSet::new();
for commit in candidate_keys {
    join_set.spawn(async move { pull_commit(commit).await });
    // Clamp concurrency 1..=4
}
```

### 4. Avoid nested runtime panic — drive block_on on dedicated OS thread

**Buggy (panics in production):**
```rust
fn build_index(&self) {
    let rt = tokio::runtime::Runtime::new()?; // fresh runtime
    rt.block_on(async { pull_candidates().await }); // PANIC: ambient runtime exists
}
```

**Fix:**
```rust
fn build_index(&self) {
    std::thread::scope(|s| {
        s.spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async { pull_candidates().await });
        }).join().unwrap();
    });
}
```

Same pattern as existing `RcloneRcd::shutdown()`: `block_on` never runs on a thread with ambient runtime.

### 5. Regression test for nested runtime panic

```rust
#[test]
fn remote_restore_from_async_runtime_does_not_nested_panic_when_rclone_enabled() {
    // try_restore_candidates is a SYNC API, so we drive it from a sync test that
    // builds its own runtime and calls block_on — this reproduces the production
    // ambient-runtime condition (dispatch_loop is async). A plain sync #[test]
    // with no runtime would NOT trigger the panic.
    let cache = SharedCache::with_remote(/* ... */);
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let hit = runtime.block_on(async move {
        // This path goes through build_index -> pull_candidate_commits internally.
        cache.try_restore_candidates(/* ... */).next().unwrap().commit().unwrap()
    });
    // Would panic ("Cannot start a runtime from within a runtime") on buggy impl.
    assert!(/* hit is valid */ true);
}
```

Test fails (panics) on buggy current-thread `block_on` and passes after moving to dedicated OS thread.

## Why This Works

1. `sync/copy` lets rclone parallelize internally with its own `--transfers` flag, avoiding per-shard RC overhead.
2. Multi-thread runtime + mutex released before request allows concurrent RC calls against rclone rcd.
3. Bounded `JoinSet` parallelizes commit pulls without overwhelming S3.
4. Dedicated OS thread for `block_on` avoids nested runtime panic. Runtime drop on dedicated thread avoids "cannot drop a runtime in a context where blocking is not allowed" panic.

## Prevention Strategies

**Test Cases:**
- Add async-context regression test for any sync code path reachable from tokio runtime (e.g., `remote_restore_from_async_runtime_does_not_nested_panic_when_rclone_enabled`).
- Existing sync `#[test]` functions do NOT catch nested-runtime issues because they have no ambient runtime.

**Best Practices:**
- When calling `block_on` from sync code that may be invoked from an async context, ALWAYS drive it on a dedicated OS thread via `std::thread::scope`.
- Hold global mutexes only for the minimum scope needed (e.g., spawn daemon, read socket path), then release before I/O.
- Use rclone batch operations (`sync/copy`, `sync/sync`) instead of per-file RC calls.

**Code Review Checklist:**
- [ ] Does this sync function call `block_on` on the current thread?
- [ ] Is the caller reachable from an async context without `spawn_blocking`?
- [ ] Is the runtime dropped on a dedicated thread, not inside async context?
- [ ] Are global locks held for minimal scope during concurrent I/O?

**Monitoring:**
- Track startup latency metric when remote cache enabled.
- Alert if startup exceeds 5s threshold.

## Related Issues

- **Prior doc:** [integration-issues/s3-remote-cache-via-rclone-rcd-2026-06-19.md](../integration-issues/s3-remote-cache-via-rclone-rcd-2026-06-19.md) — initial rclone integration, mentions runtime-drop panic but not nested block_on issue.
- **GitHub Issue:** dobesv/luchta#99
- **Plan:** `remote-cache-perf`
