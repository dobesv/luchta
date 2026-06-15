---
title: "Back-pressure for multiplexed worker stdout reader prevents log loss"
date: 2026-06-14
category: logic-errors
problem_type: logic_error
component: luchta-engine/worker
root_cause: "try_send on bounded channel drops messages when consumer falls behind"
resolution_type: code_fix
severity: high
tags:
  - tokio
  - back-pressure
  - mpsc-channel
  - multiplexing
  - log-integrity
  - concurrency
plan_ref: issue-64-worker-queue-full
---

## Problem

Under high log volume (e.g., `luchta run clean`), the worker stdout reader dropped log lines with message: "worker response queue full for job '...' ; dropping Log {...}". Build tools must never lose output.

## Symptoms

- Log lines silently dropped during high-volume worker output
- Error message visible in engine logs: `worker response queue full for job '...' ; dropping Log {...}`
- Occurred when `luchta run clean` emitted many logs faster than parent could print
- Per-job channel capacity: 64 slots; overflow triggered drops

## Investigation Steps

1. Traced message route: worker stdout → shared reader task → newline-delimited JSON `WorkerResponse` → per-job `mpsc::channel(64)` → `round_trip` consumer prints to `on_log`
2. Found `route_worker_response` used `sender.try_send(response)` — non-blocking, returns `Err` when channel full
3. Original comment rationale: "Shared stdout reader must keep draining worker output even if one job stops consuming" (fear: blocking send stalls ALL jobs on that worker)
4. Identified producer/consumer rate mismatch: reader parsed lines faster than consumer printed them
5. Recognized trade-off: drop logs vs. stall worker — build tool integrity demands stall

## Root Cause

```rust
// BEFORE (broken) — drops on full channel
if let Err(_) = sender.try_send(response) {
    eprintln!("worker response queue full for job {:?} ; dropping {:?}", ...);
}
```

Single shared stdout reader per worker process multiplexes `WorkerResponse` objects (tagged by job id) to per-job bounded channels. Consumer (`round_trip` in `manager.rs`) prints each `Log` line synchronously. When producer outpaces consumer, 64-slot buffer fills. `try_send` drops messages to avoid blocking the shared reader.

**Mistaken rationale**: Blocking `send().await` would stall the shared reader, potentially affecting all jobs multiplexed on that worker. Fear was head-of-line blocking from one slow consumer.

## Solution

```rust
// AFTER (correct) — back-pressure via await
let sender = {
    let jobs = jobs.lock().await;
    jobs.get(response.id()).cloned()  // lookup by ref, not clone first
};
if let Some(sender) = sender {
    // Back-pressure rather than drop: a build tool must not lose output.
    // Awaiting the bounded send pauses the shared reader when a job's
    // consumer falls behind, which stalls the worker's stdout until
    // the consumer catches up. SendError means job finished — benign.
    let _ = sender.send(response).await;
}
```

Replace `try_send` with `let _ = sender.send(response).await;`. Await blocks shared reader when channel full, propagating back-pressure through OS pipe buffer to worker process.

**Why deadlock fear is unfounded**:
- Every dispatched job has a dedicated active consumer (`round_trip`) draining its channel until terminal response
- Jobs map lock released before `await` (lines 193-196 release before 206)
- `SendError` (closed channel) simply means job already finished — benign, ignored via `let _`
- No circular wait: consumers don't depend on each other, only on their own channel
- Head-of-line blocking is the accepted trade-off: log completeness > per-job latency

## Why This Works

Bounded `mpsc::channel` in Tokio applies back-pressure naturally. When consumer is slow:
1. Channel fills to capacity (64)
2. `send().await` blocks sender until slot available
3. Shared reader pauses, stops reading worker stdout
4. OS pipe buffer (typically 64KB) fills
5. Worker process blocks on write, slows output production
6. Consumer catches up, channel drains, flow resumes

No logs lost. Worker naturally throttled. System reaches equilibrium.

## Prevention Strategies

**Regression test** (verified fix works, catches regression):
```rust
// emits 1000 log lines >> 64-slot channel
const LINE_COUNT: usize = 1000;
// worker script echoes 1000 Log responses before Done
// assert sink captures exactly 1000 lines in order

// Verify fix: reverted code dropped 456/1000 (captured 544)
// With fix: captures all 1000
```

**Code review checklist**:
- [ ] Multiplexed channels use `send().await` (not `try_send`) when message loss is unacceptable
- [ ] Lock scope ends before `await` to avoid blocking other tasks
- [ ] Closed channel handling documented if `SendError` is benign
- [ ] Comments explain back-pressure propagation and stall semantics

**Best practices**:
- Use `try_send` only when dropping is acceptable (metrics, best-effort hints)
- For integrity-critical data (logs, results), always apply back-pressure
- Document when head-of-line blocking is intentional trade-off

## Related Issues

- **GitHub**: [#64](https://github.com/dobesv/luchta/issues/64) — worker response queue full for job
- **Related**: Resident worker process management (`integration-issues/resident-worker-process-management-2026-06-09.md`) — same worker/manager architecture
- **Related**: stdout pipe deadlock (`logic-errors/stdout-pipe-deadlock-wait-before-read-2026-06-14.md`) — another pipe buffer constraint in worker lifecycle
