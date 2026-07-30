---
title: "Filter/proxy workers swallowing crashing resolve delegates as silent prune (issue #273)"
date: 2026-07-30
category: logic-errors
problem_type: logic_error
component: luchta-extra-resolve-worker, luchta-command-filter, luchta-file-exists-filter, luchta-yarn-filter
root_cause: "On delegate error during resolve-forward, converting to prune instead of failing — prune silently removes task from graph"
resolution_type: code_fix
severity: high
tags:
  - delegate
  - crash-handling
  - resolve-phase
  - worker-middleware
  - protocol-correctness
plan_ref: fix-delegate-crash-swallowing
---

## Problem

Filter/proxy workers swallowed a crashing/timed-out resolve delegate by emitting a synthetic `WorkerResponse::resolved(id, ResolveResult::prune(None))` on the resolve-forward error path. Prune silently removes the task from the build graph, so the engine never surfaces the crash — user sees "success with no inputs" instead of failure.

## Symptoms

- Task accepted with zero inputs when delegate crashes during resolve
- `luchta-extra-resolve-worker` emits `resolved(..., prune(None))` on delegate crash/timeout
- No error surfaced to user; build proceeds with incomplete graph
- GitHub issue #273: "luchta-extra-resolve-worker swallows a crashing resolve delegate and accepts with no inputs"

## Antipattern (Before)

Four workers had identical bug in resolve-forward delegate error path:

```rust
// WRONG: Convert delegate crash to silent prune
match delegate.send_with_timeout(...).await {
    Err(e) => {
        eprintln!("delegate error: {e}");
        emit_prune(id);  // Silently removes task!
    }
    Ok(response) => { ... }
}
```

Affected sites:
- `luchta-extra-resolve-worker`: `handle_resolve_task`, `handle_accept`, `handle_modify` (3 sites)
- `luchta-command-filter`: resolve-forward Err → prune
- `luchta-file-exists-filter`: resolve-forward Err → prune
- `luchta-yarn-filter`: resolve-forward Err → prune

## Correct Contract

On delegate crash/timeout during resolve forward, FAIL the worker:
- Log delegate command + exit status + error to stderr
- Exit non-zero (or return `Err` from `handle_*` fns — main loop converts to stderr + exit_code=1 + break)
- Do NOT emit synthetic `resolved` message

This matches the workers' existing Run-path behavior. Engine's `WorkerManager::resolve` (`round_trip_retry_once_on_crash`) then surfaces the failure with one retry.

## Fix (After)

For `handle_*` functions returning `Result<(), String>`:

```rust
match delegate.send_with_timeout(...).await {
    Err(e) => {
        return Err(format!(
            "delegate {:?} failed: {e}",
            delegate_command
        ));
    }
    Ok(response) => { ... }
}
```

Main loop already converts returned `Err` to stderr + exit 1 + break.

For filter main loops:

```rust
match delegate.send_with_timeout(...).await {
    Err(e) => {
        eprintln!("delegate crash: {e}");
        exit_code = 1;
        break;
    }
    Ok(response) => { ... }
}
```

## Critical Distinction

Two errors must be carefully separated:

| Scenario | Correct Action |
|----------|----------------|
| Delegate crash/timeout during resolve-forward | **FAIL** — exit 1, no `resolved` message |
| Predicate command execution error (filter) | **FAIL** — crashed predicate is not "does not match" |
| Worker returns `ResolveDecision::Prune` | **PRUNE** — legitimate prune decision |
| Filter's `Ok(false)` (does not match) | **PRUNE** — legitimate non-match |

Changes target ONLY delegate-error and predicate-evaluation-error arms. Legitimate prune paths preserved.

## Testing Pattern

Spawn worker binary as subprocess, test crash handling:

```rust
#[test]
fn resolve_delegate_crash_fails_without_pruning() {
    let mut child = Command::new(worker_binary())
        .args(["--delegate", "sh", "-c", "read -r _line; exit 42"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Send resolve request, then close stdin so the worker sees EOF.
    {
        let mut stdin = child.stdin.take().unwrap();
        writeln!(stdin, "{{\"ResolveTask\":{{...}}}}").unwrap();
    } // stdin dropped here → closed

    // Drain stdout/stderr while waiting to avoid a pipe-full deadlock.
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());  // Worker exits non-zero

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains("\"resolved\""));  // No synthetic prune
}
```

For predicate-evaluation-error path (cwd access failure):

```rust
#[test]
fn pattern_evaluation_error_fails_without_pruning() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(worker_binary())
        .current_dir(temp_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    // Remove cwd while worker running — safe, only child's cwd
    drop(temp_dir);

    writeln!(child.stdin.as_mut().unwrap(), "{{\"ResolveTask\":{{...}}}}").unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success());  // Fails, doesn't prune
}
```

The `fs::remove_dir` of child's cwd is safe — only affects the spawned child's working directory, not the test process. No `require_nextest` guard needed.

## Prevention

- **Code review**: When a worker converts delegate error to prune/accept, verify the contract. Delegate crash ≠ legitimate prune.
- **Protocol tests**: Each proxy/filter worker should have crash-delegate tests asserting (a) non-zero exit AND (b) no `resolved` message.
- **Pattern matching**: Error paths that synthesize a protocol response deserve scrutiny — why hide the failure?

## Related

- **Issue**: #273
- **Related**: [integration-issues/worker-middleware-double-emit-resolve-phase-2026-07-20.md](../integration-issues/worker-middleware-double-emit-resolve-phase-2026-07-20.md) — Same workers, different issue (double-emit vs crash-swallowing)
- **Related**: [logic-errors/worker-crash-retry-once-dispatch-2026-07-07.md](../logic-errors/worker-crash-retry-once-dispatch-2026-07-07.md) — Engine retry logic at dispatch layer
