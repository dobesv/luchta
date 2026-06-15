---
title: "stdout pipe deadlock from wait-before-read ordering"
date: 2026-06-14
category: logic-errors
problem_type: logic_error
component: "luchta-cli config loader"
root_cause: "wait-then-read ordering on piped stdout without concurrent drain"
resolution_type: code_fix
severity: high
tags:
  - child-process
  - pipe-buffer
  - deadlock
  - stdout
  - tokio
plan_ref: "luchta-issue-48-worker-prereqs"
---

## Problem

Child process stdout deadlocks when parent waits for exit before draining a `Stdio::piped()` stream. The child blocks on write once the pipe buffer fills (~64KB on Linux), never exits, and the wait times out with a misleading "timed out" error.

## Symptoms

- `luchta check` reported `config script timed out after 30s` even though the script ran in <100ms standalone
- `node luchta-config.mts | head` worked because `head` drained the pipe
- "running a worker" / "loading config" framing was a red herring — the loader itself deadlocked
- Deterministic repro: child emitting >64KB to piped stdout with wait-before-read ordering

## Investigation Steps

1. Observed fast standalone run vs. timeout when captured — classic pipe-buffer symptom
2. Confirmed config script output ~300KB (6000 workers)
3. Found `execute_config_script` called `child.wait()` before reading `child.stdout`; stderr was already drained concurrently via `spawn_stderr_forwarder`, but stdout was not
4. Reproduced with test generating >200KB output — immediate hang
5. Recognized pattern: `| head` works, full capture hangs = pipe buffer exhaustion

## Root Cause

```rust
// BEFORE (broken)
let mut child = spawn_config_script(...);
let stderr_task = spawn_stderr_forwarder(child.stderr.take(), ...);

let wait_result = timeout(.., child.wait()).await;  // BLOCKS HERE
let stdout = child.stdout.unwrap().read_to_end().await;  // NEVER REACHED
```

`child.stdout` is `Stdio::piped()`. Parent calls `wait()` first. Child writes >64KB to stdout, fills OS pipe buffer, blocks on write, never exits. `wait()` blocks until timeout. False "timed out" error.

Key insight: only stderr was drained concurrently. stdout was wait-then-read — exactly the pattern that deadlocks for large output.

## Solution

Spawn stdout reader task before `child.wait()`, mirroring the existing stderr forwarder:

```rust
// AFTER (fixed)
let mut child = spawn_config_script(...);
let stderr_task = spawn_stderr_forwarder(child.stderr.take(), ...);
let stdout_task = spawn_stdout_reader(child.stdout.take());  // DRAIN CONCURRENTLY

let wait_result = timeout(.., child.wait()).await;
match wait_result {
    Ok(status) => {
        let stdout = finish_stdout_reader(stdout_task, ...).await?;
        // ... success path
    }
    Err(_) => {
        abort_stdout_reader(stdout_task).await;  // CLEANUP ON TIMEOUT
        terminate_config_script(&mut child).await;
        // ... timeout path with stderr tail
    }
}
```

Helper functions:

```rust
fn spawn_stdout_reader(stdout: Option<ChildStdout>) -> Option<JoinHandle<io::Result<Vec<u8>>>> {
    stdout.map(|stdout| tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;
        Ok(bytes)
    }))
}

async fn abort_stdout_reader(stdout_task: Option<JoinHandle<io::Result<Vec<u8>>>>) {
    if let Some(handle) = stdout_task {
        handle.abort();
        let _ = handle.await;
    }
}
```

## Why This Works

The stdout reader task drains the pipe concurrently with `child.wait()`. The pipe never fills, so the child never blocks on write. On success, join the reader and return bytes. On timeout, abort the reader and kill the child — clean termination without hanging.

## Prevention Strategies

**Diagnostic heuristic:** A process that runs fast under `| head` but hangs when its full output is captured is a strong tell for pipe-buffer deadlock. Test deterministically by emitting >64KB to the piped stream.

**General rule:** When capturing a child's piped stdout AND stderr, always drain BOTH concurrently with the wait — never wait-then-read on a piped stream that can exceed the OS buffer (~64KB).

**Test coverage:** Added regression test `loads_large_config_stdout_without_timeout` generating ~300KB JSON config via Python script with 6000 workers, asserting parse succeeds in <500ms with 2s loader timeout.

```rust
#[tokio::test]
async fn loads_large_config_stdout_without_timeout() {
    // Generates ~300KB JSON config with 6000 workers
    let config = load_config_with_timeout(temp.path(), Duration::from_secs(2))
        .await
        .expect("large config should load");
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(config.workers.len(), 6000);
}
```

**Code review checklist:**
- [ ] When spawning a child with `Stdio::piped()`, is the pipe drained concurrently with `wait()`?
- [ ] Are ALL piped streams (stdout, stderr) drained before or during the wait?
- [ ] Timeout path aborts drain tasks and terminates child cleanly?

## Related Issues

- Commit: `56448767` — Fix config loader stdout pipe deadlock misreported as timeout
- File: `crates/luchta-cli/src/config.rs`
- Test: `config::tests::loads_large_config_stdout_without_timeout`
