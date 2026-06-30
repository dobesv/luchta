---
title: "Watch mode session/run split and no-lost-changes coalescing latch"
date: 2026-06-30
category: logic-errors
problem_type: logic_error
component: luchta-cli/watch
root_cause: "WorkerManager.is_shutdown is a permanent AtomicBool preventing reuse after shutdown; stale Notify permit consumed by wrong select! branch caused lost follow-up cycles"
resolution_type: code_fix
severity: high
tags:
  - watch-mode
  - session-lifecycle
  - cancellation
  - coalescing
  - race-condition
  - tokio-Notify
  - inotify
  - resident-workers
plan_ref: luchta-watch-mode
issue: "#5"
---

## Problem

Implement `luchta watch`: re-run tasks automatically when files change, with resident workers across rebuild cycles. Critical constraint: `WorkerManager.is_shutdown` is a PERMANENT `AtomicBool` — once set, the manager can never be reused. A change detected DURING a build must NOT be lost; it must trigger a follow-up rebuild.

Initial implementation had a subtle race: the same `tokio::sync::Notify` was used for both (a) cancelling the in-flight cycle and (b) waking the outer loop. A stale Notify permit could cancel the NEXT cycle before it dispatched, causing the change to be lost.

## Symptoms

- Change injected during first build produced cycle with `✔ 0` tasks when second cycle should have run
- `.run-marker` file showed count of 1 instead of 2 after change-during-build
- Second cycle returned `CycleOutcome::Cancelled` spuriously with no new change arrival
- Tests proving marker count `== 2` failed intermittently under stress

## Investigation Steps

1. **Arc identity verification**: Confirmed `Arc::ptr_eq` on WorkerManager held across cycles. Manager was NOT being respawned — the lifecycle split was correct.

2. **Notify permit tracing**: Deterministic E2E harness blocked first worker job on sentinel file, injected change mid-cycle, released sentinel. Observed: cycle 1 correctly cancelled, cycle 2 immediately returned `Cancelled` with 0 tasks. The outer loop had consumed the Notify permit intended for a different purpose.

3. **Select branch analysis**: `run_cycle_with_status` originally had a `select!` branch on `wake.notified()` to cancel in-flight work. After cycle 1 drained pending and started cycle 2, a stale permit from the drain task's `notify_one()` was still available — cycle 2's select consumed it immediately.

4. **Pending as source of truth**: Post-cycle backstop checking `pending.is_empty()` existed but the loop still relied on Notify for wake timing. Race: Notify consumed between drain and cycle start, but pending already empty → no cycle.

## Root Cause

### 1. Notify multiplexed for two signals

The same `Notify` was used for:
- **Signal A**: Cancel the in-flight cycle (drain task calls `wake.notify_one()` after `cancel_if_active()`)
- **Signal B**: Wake the outer loop after pending changes arrive

When cycle 2 started, the Notify permit from the cancel path was still unconsumed. The `select!` branch treated it as a cancel request, returning `Cancelled` before dispatch.

### 2. Notify is edge-triggered, not durable

`tokio::sync::Notify` is a signal, not state. If nobody is waiting when `notify_one()` is called, a single permit is stored. If TWO cycles run, ONE permit can cause ONE of them to spuriously wake/cancel. Correctness cannot depend on Notify permit semantics.

### 3. WorkerManager shutdown permanence

`is_shutdown: AtomicBool` is set once and never cleared. A manager that has been shut down cannot be reused; subsequent `run_job` calls will fail or panic. This constraint requires:
- **SIGINT (terminal)**: `shutdown_immediate()` is correct — process is exiting anyway
- **Watch cycle-cancel (non-terminal)**: MUST NOT call any `shutdown*()` on the manager

## Solution

### 1. Session/run split (Path B, not Path A)

```
WatchSession (long-lived):
  - owns Arc<WorkerManager>
  - owns PackageGraph, TaskGraph (static config)
  - owns workspace_root for path normalization
  - shutdown() / shutdown_immediate() ONLY on Ctrl-C

per-cycle RunContext + CancellationToken:
  - fresh Walker, output_hashes, dispatch resources
  - cancelled via dedicated ActiveCycle token
  - NO manager shutdown on cancel
```

Key files: `watch/session.rs` (WatchSession), `run.rs` (RunContext, run_cycle), `run/setup.rs` (per-cycle resources).

### 2. PendingChanges as sole source of truth

```rust
pub struct PendingChanges {
    inner: Mutex<HashSet<PathBuf>>,
}

impl PendingChanges {
    pub fn add(&self, batch: HashSet<PathBuf>) -> bool {
        let mut inner = self.inner.lock().expect("pending mutex poisoned");
        let was_empty = inner.is_empty();
        inner.extend(batch);
        was_empty && !inner.is_empty()  // signal: transition empty→non-empty
    }

    pub fn drain_non_empty(&self) -> Option<HashSet<PathBuf>> {
        let mut inner = self.inner.lock().expect("pending mutex poisoned");
        if inner.is_empty() { None } else { Some(std::mem::take(&mut *inner)) }
    }

    pub fn has_changes(&self) -> bool {
        !self.inner.lock().expect("pending mutex poisoned").is_empty()
    }
}
```

**Invariant**: Every path added to pending is either (a) in the current drained set, or (b) remaining pending for a follow-up cycle. Provable WITHOUT relying on Notify.

### 3. ActiveCycle for direct cancellation

```rust
pub struct ActiveCycle {
    token: Mutex<Option<CancellationToken>>,
}

impl ActiveCycle {
    pub fn set(&self, token: CancellationToken) -> bool {
        let mut guard = self.token.lock().expect("active cycle mutex poisoned");
        if guard.is_some() { return false; }
        *guard = Some(token);
        true
    }

    pub fn cancel_if_active(&self) -> bool {
        if let Some(token) = self.token.lock().expect("active cycle mutex poisoned").take() {
            token.cancel();
            true
        } else {
            false
        }
    }

    pub fn clear(&self) {
        self.token.lock().expect("active cycle mutex poisoned").take();
    }
}
```

The drain task calls `active_cycle.cancel_if_active()` directly on the stored token. The Notify is ONLY a hint for the outer `notified().await` wait.

### 4. Watch loop with durable pending check

```rust
loop {
    // Initial cycle
    let cancel = CancellationToken::new();
    active_cycle.set(cancel.clone());
    let outcome = session.run_cycle(..., cancel.clone()).await;
    active_cycle.clear();

    // Backstop: check pending AFTER cycle returns
    if pending.has_changes() {
        continue;  // immediate follow-up, no await
    }

    // Wait for new changes
    tokio::select! {
        biased;
        _ = shutdown.clone() => return shutdown_watch(...),
        _ = wake.notified() => {
            // Wake hint only — pending is the authority
            if !pending.has_changes() {
                // Spurious wakeup, back to top of loop
                continue;
            }
        }
    }
}
```

The loop does NOT rely on `wake.notified()` for correctness. It's an optimization to avoid busy-waiting.

### 5. Distinct SIGINT vs cycle-cancel

| Operation | Terminal? | WorkerManager | Mechanism |
|-----------|-----------|----------------|-----------|
| SIGINT (Ctrl-C) | YES | `shutdown_immediate()` | `interrupted` AtomicBool → kill workers, exit |
| Cycle cancel (new change) | NO | Keep alive | `CancellationToken.cancel()` → stop dispatch, drain in-flight |

File: `run.rs` `run_cycle` — `select!` dispatch vs `cancel_token.cancelled()`.

### 6. Inotify-frugal file watching

**CRITICAL**: Do NOT use `RecursiveMode::Recursive` on workspace root. On Linux, notify registers an inotify watch per subdirectory at setup time — including `node_modules/` and `target/` — exhausting system limits (ENOSPC).

```rust
// watcher.rs
fn discover_watch_dirs(workspace_root: &Path) -> Vec<PathBuf> {
    let mut builder = ignore::WalkBuilder::new(workspace_root);
    builder.git_ignore(true);
    builder.git_global(true);
    builder.hidden(false);  // include hidden for .gitignore check

    // Hardcoded ignore by name
    builder.filter_entry(|entry| {
        !IGNORED_DIR_NAMES.contains(&entry.file_name().to_string_lossy().as_ref())
    });

    builder.build()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.into_path())
        .collect()
}

// Non-recursive watch per directory
for dir in watch_dirs {
    debouncer.watcher().watch(&dir, RecursiveMode::NonRecursive)?;
}
```

New directories detected at runtime: bridge task locks debouncer, registers `Watch(new_dir, NonRecursive)`.

File: `watch/watcher.rs`.

### 7. Package-level change detection reuse

`since.rs` already has path→package→transitive-dependents logic. Refactored:

```rust
pub fn affected_packages_from_paths(
    changed_paths: &HashSet<PathBuf>,
    repo_root: &Path,
    package_graph: &PackageGraph,
) -> Result<HashSet<PackageName>, SinceError>
```

Accepts absolute paths (Rust `Path::join` keeps absolute RHS unchanged). Root package is explicitly excluded from matching — repo-root files map to no package (consistent with `--since` behavior).

**Limitation**: Nested `.gitignore` files may not be fully honored in event-time filtering. Initial directory discovery uses `ignore::WalkBuilder` (full gitignore support), but `IgnoreFilter` for event filtering loads only root `.gitignore`. Impact: unnecessary rebuilds for files ignored by nested rules.

File: `since.rs`.

## Why This Works

1. **PendingChanges is durable**: `std::mem::take` under a mutex atomically swaps the set. Any path added during the swap is either returned or remains for next drain — never lost.

2. **ActiveCycle direct cancellation**: Token is stored in Mutex<Option<CancellationToken>>. Drain task extracts and cancels directly — no multiplexed Notify.

3. **Notify is wake-hint only**: The outer loop checks `pending.has_changes()` before AND after awaiting `wake.notified()`. A stale permit causes at most one spurious wake, then the empty check suppresses the cycle.

4. **Manager never shut down on cancel**: Only SIGINT path calls `shutdown_immediate()`. Cancel path returns `CycleOutcome::Cancelled` with manager still usable.

5. **Inotify limits avoided**: Per-dir non-recursive watches never descend into `node_modules/`, `target/`, `.git/`. The event filter would be too late — watches already consumed.

## Prevention Strategies

### Test Cases

- **No-lost-changes E2E**: Block first cycle on sentinel file, inject change, release sentinel, assert `.run-marker` count == 2. File: `driver.rs` `no_lost_changes_change_during_build_triggers_second_cycle`.

- **Worker reuse after cancel**: Cancel cycle mid-flight, assert `Arc::ptr_eq` on manager before and after, assert `!is_shutdown()`. File: `run.rs` `watch_session_cancellation_drains_in_flight_job_and_keeps_workers_alive`.

- **No rebuild for outside-package changes**: Inject change to workspace root (non-package file), assert marker stays at 1. File: `driver.rs` `change_outside_package_does_not_trigger_rebuild`.

- **Stress test**: `cargo nextest run --stress-count=5` to catch Notify permit races.

- **Inotify exhaustion**: Run watch on repo with large `node_modules/`, verify no ENOSPC.

### Best Practices

- **Never multiplex Notify for two independent signals**: Use separate channels or a durable state (like PendingChanges) as the source of truth.

- **Make pending state authoritative, notifications opportunistic**: Correctness should be provable without Notify semantics. Notify only avoids busy-waiting.

- **Cancel via CancellationToken stored in shared state**: Direct `token.cancel()` is unambiguous; select! on `cancel_token.cancelled()` is the cancellation point.

- **Distinguish terminal vs non-terminal cancellation**: SIGINT is terminal (process exiting); cycle-cancel is non-terminal (manager must survive).

- **Per-dir non-recursive watches on Linux**: Never recursive-watch large trees. Use gitignore-aware directory discovery.

- **Drain-based cancel when protocol lacks per-job abort**: If workers have no abort message, stop dispatch and let in-flight jobs complete.

- **Avoid process-group in worker spawn**: Manager already kills process group on shutdown. Watch cancel doesn't kill workers, so no orphan risk.

### Code Review Checklist

- [ ] Notify used for only ONE purpose (cancel OR wake, never both)?
- [ ] `pending.has_changes()` checked before and after `notified().await`?
- [ ] Cancel branch does NOT call `worker_manager.shutdown*()`?
- [ ] SIGINT path correctly calls `shutdown_immediate()`?
- [ ] File watcher uses `RecursiveMode::NonRecursive` per directory?
- [ ] Directory discovery uses `ignore::WalkBuilder` with gitignore?
- [ ] Nested `.gitignore` limitation documented if applicable?

## Related Issues

- **Prior Art:** [logic-errors/since-filter-selection-and-gix-change-detection-2026-06-18.md](./since-filter-selection-and-gix-change-detection-2026-06-18.md) — `affected_packages_from_paths` reused for watch
- **Prior Art:** [integration-issues/resident-worker-process-management-2026-06-09.md](../integration-issues/resident-worker-process-management-2026-06-09.md) — WorkerManager lifecycle, Arc identity, process groups
- **Prior Art:** [logic-errors/async-shutdown-worker-pool-notify-race-2026-06-10.md](./async-shutdown-worker-pool-notify-race-2026-06-10.md) — Notify race patterns, `Notified::enable()` usage
- **GitHub:** [dobesv/luchta#5](https://github.com/dobesv/luchta/issues/5) — Watch mode feature request
- **Plan:** `luchta-watch-mode` — Full implementation history in plan notes (ab5e4563, 2dcd0ec7, efa5f9b9, c69b882f, 98443070, d343febb, a8d94335)
