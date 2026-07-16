---
title: "Delegate process exit-status capture without double-wait or shutdown deadlock"
date: 2026-07-16
category: integration-issues
problem_type: integration_issue
component: luchta-worker/proxy
root_cause: "Tokio Child supports only one wait() caller; naive reaper + shutdown paths cause double-wait, deadlock, or mislabeled graceful-shutdown exits"
resolution_type: code_fix
severity: high
tags:
  - process-management
  - concurrency
  - shutdown
  - exit-status
  - reaper
  - tokio
  - unix
  - middleware
plan_ref: improve-worker-error-output
---

## Problem

Adding delegate exit-status capture to proxy workers (`DelegateHandle`, `RawDelegate`) for diagnostics requires a reaper task that calls `child.wait()`. However, `tokio::process::Child` permits exactly **one** `wait()` caller. The shutdown path also needs to signal/kill the child. Naive designs cause four distinct bugs:

1. **Double-wait**: Both reaper and shutdown call `child.wait()` → panic or "Invalid argument" error.
2. **Shutdown deadlock (mutex contention)**: Reaper holds `child` mutex across `wait().await`; shutdown's kill path blocks forever waiting for that mutex.
3. **Shutdown hanging (false "exited" signal)**: Shutdown uses `child.is_none()` in mutex as proxy for "process exited", skips SIGKILL escalation, and hangs when the process ignores SIGTERM.
4. **Mislabeling graceful shutdown as failure**: Clean exit 0 or our-own-SIGTERM during intentional shutdown logged as "delegate process failed".

## Symptoms

- `shutdown should complete via kill path: Elapsed(())` — shutdown times out on stuck child that ignores SIGTERM.
- `clean exit should not log delegate failure: ["delegate: delegate process failed: ..., exit=signal: 15 (SIGTERM)"]` — graceful shutdown incorrectly logs failure.
- `ConcurrentModificationException` equivalent: double-wait on tokio `Child`.
- Process leaked: Drop aborts reaper without wait, leaving zombie.

## Investigation Steps

Started from test failure `raw_delegate_shutdown_kills_stuck_child` timing out. Initial attempt moved `Child` into `Arc<Mutex<Option<Child>>>` with a reaper that `take()`s before `wait()`. This solved double-wait but introduced race: reaper takes child → shutdown can't kill → hangs.

Next attempt: store PID at spawn, use `nix::killpg(-pgid, SIG...)` to signal process group independently of `Child` handle. This fixed the kill path but still raced: `wait_for_delegate_exit` returned early when child mutex was empty, skipping SIGKILL escalation.

Final fix: replace child-presence check with actual exit monitoring via `watch` channel. An `Arc<AtomicBool> shutting_down` flag distinguishes intentional shutdown from crashes. Non-Unix path rewritten to use `try_wait()` poll loop that releases mutex between polls.

Stress-tested stuck-child tests 50+ iterations with 0 failures across 5 rounds of race fixes.

## Root Cause

Tokio `Child` enforces single-wait semantics. The reaper must be the sole caller. Previous designs conflated two concerns:
1. **Ownership** of the `Child` handle (who can `wait()`)
2. **Signalability** of the process (who can send TERM/KILL)

On Unix, signalability decouples from `Child` via `kill(-pgid, signal)`. On non-Unix (Windows), no process-group signaling exists; kill requires the `Child` handle, so reaper must release mutex between polls.

The shutdown path used child-presence-in-mutex as a proxy for "process alive" — invalid when reaper owns the child. This caused shutdown to skip escalation when it mistakenly thought the process had exited.

## Solution

### Unix Design

```rust
struct DelegateState {
    child: Arc<Mutex<Option<Child>>>,
    child_pid: Option<u32>,  // Captured at spawn
    reaper_task: JoinHandle<()>,
}

struct DelegateLifecycle {
    exit_status: Arc<Mutex<Option<ExitStatus>>>,
    shutting_down: Arc<AtomicBool>,
}

// Reaper: SOLE wait() caller
#[cfg(unix)]
async fn reap_delegate_child(
    child: Arc<Mutex<Option<Child>>>,
    exit_status: Arc<Mutex<Option<ExitStatus>>>,
) {
    let child = child.lock().await.take();  // Take before wait
    let Some(mut child) = child else { return };
    if let Ok(status) = child.wait().await {
        *exit_status.lock().await = Some(status);
    }
}

// Shutdown: signals via stored PID, watches ACTUAL exit
async fn shutdown_delegate_process(...) -> Result<(), ProxyError> {
    let (exit_status_tx, exit_status_rx) = watch::channel(None);
    let monitor_task = tokio::spawn(monitor_delegate_exit(
        Arc::clone(&exit_status), exit_status_tx, reaper_task,
    ));

    signal_delegate(child, child_pid, terminate_child).await?;  // SIGTERM via killpg
    let timed_out = wait_for_delegate_exit(exit_status, exit_status_rx, SHUTDOWN_TIMEOUT)
        .await.is_err();
    if timed_out {
        signal_delegate(child, child_pid, kill_child).await?;  // SIGKILL escalation
    }
    await_delegate_exit_monitor(monitor_task).await;
    Ok(())
}

// Signal via process group - works without Child handle
fn signal_delegate(child: &Arc<Mutex<Option<Child>>>, child_pid: Option<u32>, signaler: ChildSignaler) {
    // On Unix: ignore child mutex, use stored PID
    let _ = child;
    signaler(child_pid).await
}

fn terminate_child(child_pid: Option<u32>) -> BoxFuture<Result<(), ProxyError>> {
    let id = child_pid.ok_or(ProxyError::MissingChildId)? as i32;
    nix_killpg(id, libc::SIGTERM)?;
    Ok(())
}

// Treat ESRCH as success (process already dead)
fn nix_killpg(pgid: i32, signal: i32) -> Result<(), ProxyError> {
    let result = unsafe { libc::kill(-pgid, signal) };
    if result == 0 { return Ok(()); }
    let error = std::io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(libc::ESRCH)) {
        Ok(())  // Process already terminated
    } else {
        Err(ProxyError::Io(error))
    }
}
```

### Non-Unix Design

```rust
// Reaper: bounded poll loop, releases mutex between polls
#[cfg(not(unix))]
async fn reap_delegate_child(
    child: Arc<Mutex<Option<Child>>>,
    exit_status: Arc<Mutex<Option<ExitStatus>>>,
) {
    loop {
        let status = {
            let mut child_guard = child.lock().await;
            let Some(child) = child_guard.as_mut() else { return };
            match child.try_wait() {
                Ok(Some(status)) => Some(status),
                Ok(None) => None,  // Still running
                Err(_) => None,
            }
        };

        if let Some(status) = status {
            *exit_status.lock().await = Some(status);
            return;
        }

        tokio::time::sleep(REAPER_POLL_WAIT).await;  // 25ms
    }
}

// Signal: needs mutex, but reaper releases between polls
async fn signal_delegate(child: &Arc<Mutex<Option<Child>>>, ...) {
    let mut child_guard = child.lock().await;
    let Some(child) = child_guard.as_mut() else { return Ok(()) };
    child.start_kill()?;
}
```

### Clean vs Crash Classification

```rust
// EOF branch: log failure only when dirty
let has_in_flight_waiters = !waiters.lock().await.is_empty();
let shutting_down = lifecycle.shutting_down.load(Ordering::SeqCst);
let dirty = has_in_flight_waiters
    || (!shutting_down && status.is_none_or(|s| !s.success()));
if dirty {
    log_delegate_failure(&stderr_writer, stderr_prefix, &delegate_command, status).await?;
}
```

Log failure when:
- In-flight waiters remain, OR
- Not shutting down AND (exit unknown OR exit non-success)

This suppresses logging for:
- Clean exit 0 with no in-flight waiters
- Our-own-SIGTERM during graceful shutdown with no in-flight waiters

Still logs for:
- Genuine crashes (nonzero exit with in-flight work)
- External kills (SIGTERM from outside our shutdown)
- Unknown exit status with in-flight work

### Shutdown Flag Timing

```rust
async fn shutdown_raw_delegate(state, exit_status, shutting_down) {
    state.stdin_task.abort();
    shutting_down.store(true, Ordering::SeqCst);  // BEFORE sending SIGTERM
    shutdown_delegate_process(...).await?;
    ...
}
```

Set `shutting_down` flag before signaling, so EOF branch observes correct state.

## Why This Works

1. **Single-wait guarantee**: Reaper `take()`s child from mutex; only reaper calls `wait()`. Shutdown uses monitor task + watch channel to observe exit without calling `wait()`.

2. **Kill path independent of Child handle (Unix)**: Stored `child_pid` + `kill(-pgid, SIG...)` allows signaling even after reaper owns the `Child`. Process-group signaling reaches all descendants.

3. **Mutex-free kill path (Unix)**: `signal_delegate` ignores child mutex entirely on Unix, using stored PID.

4. **Non-deadlock polling (non-Unix)**: Reaper holds mutex only for `try_wait()` (non-blocking), releases before `sleep()`. Shutdown `start_kill()` can acquire mutex between polls.

5. **Real exit monitoring**: `wait_for_delegate_exit` uses watch channel, not child-presence-in-mutex. Shutdown escalates to SIGKILL based on actual timeout, not false "already exited" signal.

6. **Correct failure classification**: `shutting_down` flag distinguishes intentional termination from crashes. Dirty check suppresses logging for clean graceful shutdown while catching real failures.

7. **ESRCH handling**: When signaling an already-dead process, `libc::ESRCH` is treated as success rather than error.

## Prevention Strategies

### Test Cases

- Stress test stuck-child shutdown 50+ iterations: `raw_delegate_shutdown_kills_stuck_child`, `delegate_shutdown_kills_stuck_child`
- Test SIGKILL escalation: child that traps SIGTERM should be force-killed after timeout
- Test clean exit: exit 0 with no in-flight waiters → no failure log
- Test intentional shutdown: SIGTERM from our shutdown → no failure log
- Test crash: nonzero exit with in-flight work → failure logged
- Test external kill: SIGTERM from outside (not our shutdown) with in-flight work → failure logged

### Best Practices

- **Capture PID at spawn**: Store `child.id()` before any ownership transfers
- **Reaper is sole waiter**: Only one code path calls `child.wait()`
- **Signal via process group on Unix**: Use `kill(-pgid, SIG...)` for group-wide signaling
- **Don't use child presence as exit signal**: Child can be moved/taken; monitor actual exit via channel/flag
- **Release mutex before blocking await**: On non-Unix, long holds block other paths
- **Distinguish intentional shutdown**: Use atomic flag set before signaling
- **Handle ESRCH**: Process already dead is success, not error

### Code Review Checklist

- [ ] Exactly one `child.wait()` caller in the codebase?
- [ ] PID captured at spawn before any ownership transfer?
- [ ] Shutdown kills via stored PID, not requiring Child handle?
- [ ] Non-Unix reaper releases mutex between polls?
- [ ] Exit detection uses watch/notify, not child-presence check?
- [ ] `shutting_down` flag set before sending SIGTERM?
- [ ] ESRCH handled as success?
- [ ] Clean-vs-crash classification includes `shutting_down` check?
- [ ] Stress-tested stuck-child shutdown under race conditions?

## Related Issues

- **GitHub:** [#126](https://github.com/dobesv/luchta/issues/126) — Log command line and exit code when worker exits unexpectedly
- **Prior Solution:** [worker-crash-diagnostics-ownership-boundary-2026-06-20.md](./worker-crash-diagnostics-ownership-boundary-2026-06-20.md) — Established engine-owned failure reporting, signal naming
- **Prior Solution:** [worker-error-unified-failure-block-2026-06-23.md](./worker-error-unified-failure-block-2026-06-23.md) — Unified CLI output for crash details inside failure box
