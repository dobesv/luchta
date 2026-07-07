---
title: "Watch mode ignoring edits after failed build cycle (registry + WorkerManager poisoning)"
date: 2026-07-06
category: logic-errors
problem_type: logic_error
component: luchta-cli/watch
root_cause: "Failed runs skipped watch-state registration leaving registry empty; fast-stop shutdown_immediate poisoned the shared cross-cycle WorkerManager"
resolution_type: code_fix
severity: high
tags:
  - watch-mode
  - WorkerManager
  - shared-resource-lifecycle
  - fast-stop
  - TaskWatchRegistry
  - state-poisoning
plan_ref: watch-failed-cycle-registry
---

## Problem

Watch mode stopped detecting file edits after any build cycle failed, requiring a
restart to recover. User saw `[watch] up to date` after editing inputs that should
have triggered a rebuild.

Two independent root causes combined to produce this symptom.

## Symptoms

```text
[watch] build failed (1 failed)
[watch] up to date          <- edit made, but no rebuild triggered
[watch] up to date          <- subsequent edits also ignored
```

- After a failed cycle, ALL edits reported "up to date"
- No rebuild attempted even for legitimate input changes
- Restarting watch mode temporarily "fixed" it until the next failure

## Root Cause Analysis

### Bug 1: TaskWatchRegistry empty after failed cycle

`build_run_record` in `dispatch.rs` registered watch state ONLY for successful
runs. Failed runs skipped `register_task_watch_state`, leaving `TaskWatchRegistry`
empty for that task.

Consequence: `dirty_packages_for_changes` returned nothing → edits dropped as
"up to date". Worse, `PendingChanges::drain_non_empty` consumed the event,
preventing retry even if registry was later populated.

**Fix:** Register watch state for failed runs too (except `StabilityMismatch`),
without making failed cache records skippable — `decide.rs` PriorFailed still
forces rerun.

### Bug 2: WorkerManager poisoning (cross-cycle shared resource)

Default `continue_on_failure: false` (fast-stop) called `shutdown_immediate()` on
first failure. Watch mode reuses ONE long-lived `WorkerManager` across cycles
(by design — `session.rs`). Once `is_shutdown=true`, every later cycle returned
`WorkerError::Crashed` immediately.

The first bug surfaced this as "up to date"; without the registry bug, the
poisoning would have manifested as "worker crashed during job".

**Fix (design principle):** Fast-stop is a per-cycle concern (stop dispatching
more tasks THIS cycle via the `interrupted` flag); it must NOT tear down a
shared cross-cycle resource.

## Solution

### For Bug 1 (Registry)

Register watch state unconditionally at the end of the run record build, gated
by run outcome:

```rust
// dispatch.rs::build_run_record - simplified logic
if !matches!(outcome, Outcome::StabilityMismatch) {
    register_task_watch_state(&task_id, &watch_state);
}
```

### For Bug 2 (WorkerManager)

Added `owns_worker_manager: bool` to `RunContext`:
- One-shot runs: `true` (default, existing behavior preserved)
- Watch cycles: `false` (cannot call `shutdown_immediate`)

Gate ALL `shutdown_immediate()` call sites on this flag:

```rust
// dispatch.rs fast-stop path
if first_failure && !continue_on_failure && ctx.owns_worker_manager {
    spawn(worker_manager.shutdown_immediate());
}

// run.rs post-cycle cleanup
if was_interrupted && ctx.owns_worker_manager {
    worker_manager.shutdown_immediate();
}
```

Watch's Ctrl-C handler in `driver.rs` still calls `session.shutdown_immediate()`
directly — that's the legitimate user-initiated shutdown.

## Why This Works

**Registry fix:** Failed runs now populate `TaskWatchRegistry`, enabling
`dirty_packages_for_changes` to identify dirty packages from input edits.
The PriorFailed decide rule ensures failed tasks always rerun regardless of
cache state.

**WorkerManager fix:** Separating per-cycle concerns (don't dispatch more tasks)
from cross-cycle resource lifecycle (keep the worker pool alive) ensures a
failed cycle doesn't poison the watch session. The ownership flag makes the
intention explicit at construction time rather than depending on call-site
discipline.

## Gotcha: The Hard-Coded Literal Trap

The initial fix added `owns_worker_manager` and plumbed it through `RunContext`,
`RunCycleParams`, and the call chain. But a HARD-CODED `owns_worker_manager: true`
literal in `spawn_task_runner`'s `TaskRunFinalization` construction bypassed the
propagated value.

The poisoning persisted even though the flag "looked fully plumbed."

**Lesson:** When adding a flag that must flow everywhere:
1. Grep for EVERY struct-literal construction of the carrying types
2. Grep for the target method's call sites
3. Don't trust that "the field exists" means "the field is propagated"

## Diagnostic Technique: Backtrace Capture at State Transitions

After two rounds of "still failing," an instrumented probe cracked it:

```rust
// Temporary diagnostic at cycle boundaries
eprintln!("[DEBUG] is_shutdown = {}", worker_manager.is_shutdown());

// Inside shutdown_all
let bt = Backtrace::force_capture();
eprintln!("[DEBUG] shutdown_all called from:\n{}", bt);
```

This pinpointed the exact call site flipping the flag — revealing the
hard-coded literal bypass.

**Reusable approach for "shared state mysteriously poisoned" bugs:**
Capture the state transition with a backtrace at the mutation point.

## E2E Test Requirements

Reproducing the true user scenario required:

1. Worker script that fails job 1 then succeeds on retry
2. `git init` + `git add -A` in test workspace — input hashing walks up to a
   git repo boundary (resolves empty otherwise)
3. Marker file append BEFORE the `done` message so markers advance even on
   failed jobs

The E2E must FAIL if either fix is reverted (verified via revert-checks).

## Prevention Strategies

**Code Review Checklist:**
- [ ] Does this resource span multiple cycles (watch, server mode)?
- [ ] Are shutdown/cleanup paths gated by ownership semantics?
- [ ] Is the flag propagated through EVERY construction site, not just "added to the struct"?

**Test Coverage:**
- Watch E2E with failed cycle → edit → rebuild MUST run
- Assert `worker_manager.is_shutdown() == false` after a failed watch cycle

**Diagnostic Pattern:**
When debugging shared state corruption, capture backtraces at mutation points
using `Backtrace::force_capture()` rather than relying on log inspection.

## Related Issues

- **watch-input-aware-rebuild-registry** — Earlier registry work that introduced
  the `TaskWatchRegistry` this bug affected
- **watch-mode-session-run-split** — Original separation of watch session from
  run cycle that established the shared WorkerManager pattern
