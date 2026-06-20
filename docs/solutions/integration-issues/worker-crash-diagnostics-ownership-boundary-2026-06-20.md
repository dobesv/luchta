---
title: "Worker crash diagnostics with worker identity, exit reason, and stderr block"
date: 2026-06-20
category: integration-issues
problem_type: integration_issue
component: luchta-engine/worker
root_cause: "Crash diagnostics lacked worker identity and readable stderr; middleware incorrectly plumbed failures to JSONL stdout"
resolution_type: code_fix
severity: high
tags:
  - diagnostics
  - process-management
  - stderr-capture
  - jsonl-ipc
  - ownership-boundary
  - middleware
  - exit-status
plan_ref: worker-error-diagnostics
---

## Problem

Worker process crashes reported only vague `delegate failed before done: io error: Broken pipe` without identifying which worker or task failed, what caused the exit (code vs signal), or any worker stderr context. Additionally, middleware crates (yarn-filter, lazy-worker, command-filter, file-exists-filter) incorrectly plumbed delegate failures as JSONL `log`/`done` responses to stdout, conflating failure-reporting ownership between middleware and engine.

## Symptoms

- Error messages: `delegate failed before done: io error: Broken pipe` (no worker name, no task id, no exit reason)
- `WorkerError::Protocol` lacked worker identity: `protocol error for job 'pkg#task': unexpected 'done' response`
- Worker stderr printed directly to terminal without context (which worker, which task)
- Middleware wrote failure diagnostics as JSONL to stdout, risking protocol corruption if delegate failed mid-message
- Operators could not diagnose OOM kills, segfaults, or other worker exits from crash output

## Investigation Steps

1. Reviewed existing crash infrastructure: `WorkerCrashState::stderr_tail` (VecDeque, limit 32), `format_exit_status` (code vs signal), reaper→crash flow already correct. Much was built already.

2. Identified gaps:
   - `WorkerError::Protocol` missing `worker` field (construction site in `round_trip` had `worker_name` in scope)
   - `crash_info()` rendered stderr inline (`stderr: l1 | l2`) — hard to read, not a block
   - Exit reason not prominent (buried in detail string)
   - Middleware emitted `delegate failed before done` as JSONL instead of letting engine own failure reporting

3. Traced ownership boundary: middleware should print to own stderr, exit non-zero; engine reaper detects worker exit and reports `WorkerError::Crashed`. Middleware must NOT write failure text to stdout (corrupts JSONL protocol).

4. Testing insight: triggering deterministic `WorkerError::Protocol` required driving `resolve` round-trip with unexpected response while keeping worker alive — `run_job` path raced worker-exit and produced `Crashed` instead.

## Root Cause

1. **Missing worker identity in Protocol error**: `WorkerError::Protocol { id, detail }` lacked `worker` field. Worker name was available at construction but not threaded through.

2. **Inline stderr rendering**: `crash_info()` joined stderr lines with ` | ` separator, producing unreadable single-line output for multi-line stderr.

3. **Ownership boundary violation**: Middleware crates caught delegate failures and emitted JSONL `log stderr` + `done` responses. This dual ownership of failure reporting muddied responsibility and risked stdout pollution.

4. **Exit reason not prominent**: `format_exit_status` existed but output was not clearly positioned in crash message.

## Solution

### 1. Add worker field to WorkerError::Protocol

```rust
// manager.rs
#[error("worker '{worker}' protocol error for job '{id}': {detail}")]
Protocol {
    worker: String,
    id: String,
    detail: String,
},
```

Construction in `round_trip` now passes `worker_name.to_owned()`.

### 2. Render stderr as delimited block

```rust
// handle.rs
fn format_stderr_block(worker_name: &str, lines: &VecDeque<String>) -> String {
    let count = lines.len();
    let header = format!("--- worker '{worker_name}' stderr (last {count} lines) ---");
    let footer = format!("--- end worker '{worker_name}' stderr ---");
    format!("{header}\n{}\n{footer}", lines.iter().join("\n"))
}

pub(crate) fn crash_info(&self, worker_name: &str) -> Option<WorkerCrashInfo> {
    let mut detail = Vec::new();
    if let Some(status) = self.status {
        detail.push(format_exit_status(status));
    }
    if let Some(wait_error) = &self.wait_error {
        detail.push(format!("wait error: {wait_error}"));
    }
    let mut detail = detail.join("; ");

    if !self.stderr_tail.is_empty() {
        let stderr_block = format_stderr_block(worker_name, &self.stderr_tail);
        if !detail.is_empty() {
            detail.push('\n');
        }
        detail.push_str(&stderr_block);
    }
    // ...
}
```

Output:
```
exited with code 1
--- worker 'builder' stderr (last 2 lines) ---
error: out of memory
at allocate (core.rs:42)
--- end worker 'builder' stderr ---
```

### 3. Middleware exits non-zero to own stderr

```rust
// lazy-worker/src/main.rs (same pattern in yarn-filter, command-filter, file-exists-filter)
WorkerMessage::Run(request) => {
    if let Err(error) = delegate.send(WorkerMessage::Run(request)).await {
        eprintln!("delegate failed: {error}");
        exit_code = 1;
        break;
    }
}
```

Middleware writes to own stderr (never stdout — would corrupt JSONL), exits non-zero. Engine detects worker exit, collects crash detail via reaper, reports `WorkerError::Crashed` with full context.

### 4. Unix signal name lookup

```rust
fn signal_name(signal: i32) -> Option<&'static str> {
    match signal {
        6 => Some("SIGABRT"),
        8 => Some("SIGFPE"),
        9 => Some("SIGKILL"),
        11 => Some("SIGSEGV"),
        15 => Some("SIGTERM"),
        _ => None,
    }
}

fn format_exit_status(status: ExitStatus) -> String {
    #[cfg(unix)]
    {
        if let Some(signal) = status.signal() {
            return match signal_name(signal) {
                Some(name) => format!("killed by signal {name} ({signal})"),
                None => format!("killed by signal {signal}"),
            };
        }
    }
    // ...
}
```

## Why This Works

1. **Worker identity in all error paths**: Both `WorkerError::Protocol` and `WorkerError::Crashed` carry worker name and job id. Operator can identify which worker and which task(s) failed.

2. **Clear exit reason**: Exit code vs signal prominently displayed. Common signals named (SIGKILL, SIGSEGV, etc.) for operator recognition.

3. **Readable stderr block**: Delimited header/footer with line count; multi-line stderr preserved as lines, not escaped inline. Block omitted when stderr empty.

4. **Ownership boundary clear**: Middleware does not emit JSONL failure messages. Engine owns failure reporting via reaper-detected worker exit. This preserves JSONL protocol integrity — middleware stdout remains clean JSONL stream.

5. **No race in crash detection**: Existing reaper→crash flow already correct. Reaper sets `crash_state.status`, signals `exit_notify`, drops job senders. `round_trip` sees channel close, calls `wait_for_crash_detail` (250ms), then `crashed_error_for`.

## Prevention Strategies

### Test Cases

Added in `crates/luchta-engine/src/worker/manager/tests.rs`:
- `unexpected_done_response_to_resolve_returns_protocol_error_with_worker_and_id` — Protocol error includes worker name and job id
- `crashed_worker_is_evicted_and_respawned` — Crash error includes worker name, task id, exit reason, stderr block delimiter

Updated in `crates/luchta-lazy-worker/tests/protocol.rs`:
- `delegate_failure_on_first_run_exits_nonzero_and_reports_stderr` — Asserts non-zero exit + stderr text (not JSONL)

### Best Practices

- **Middleware must not write failure diagnostics to stdout**: Use `eprintln!` + non-zero exit; let engine own failure reporting via reaper.
- **Thread worker name through error construction sites**: Worker identity at point of error construction, not just in error variant definition.
- **Render stderr as delimited block**: Multi-line output readable; inline `stderr: l1 | l2` pattern hard to scan.
- **Limit signal name lookup to common signals**: Do not build exhaustive table — common cases (SIGKILL, SIGSEGV, SIGABRT, SIGTERM, SIGFPE) sufficient.

### Code Review Checklist

- [ ] Error variants carry worker name + job id?
- [ ] Middleware failure path writes to stderr (not stdout)?
- [ ] Middleware exits non-zero on delegate failure?
- [ ] Crash diagnostic includes exit reason (code or signal)?
- [ ] Stderr rendered as delimited block (not inline)?
- [ ] Signal name lookup limited to common signals?

### Testing Insight for Future Authors

Triggering `WorkerError::Protocol` deterministically requires driving `resolve` round-trip with unexpected `done` response while keeping worker alive. The `run_job` path races worker-exit and produces `Crashed` instead. Use separate test paths to validate each error variant.

## Related Issues

- **GitHub:** [#106](https://github.com/dobesv/luchta/issues/106) — Worker communication error messages do not indicate which worker or task
- **GitHub:** [#65](https://github.com/dobesv/luchta/issues/65) — Display errors from workers
- **Prior Solution:** [worker-crash-handle-cache-dead-reuse-2026-06-13.md](../logic-errors/worker-crash-handle-cache-dead-reuse-2026-06-13.md) — Initial crash diagnostics (exit status, stderr tail buffer)
- **Prior Solution:** [resident-worker-process-management-2026-06-09.md](../integration-issues/resident-worker-process-management-2026-06-09.md) — Reaper/shutdown architecture
- **Prior Solution:** [multiplexed-worker-backpressure-2026-06-14.md](../logic-errors/multiplexed-worker-backpressure-2026-06-14.md) — Per-job JSONL multiplexing
- **Prior Solution:** [uncached-task-detected-output-coupling-2026-06-12.md](../logic-errors/uncached-task-detected-output-coupling-2026-06-12.md) — Stdout pollution corrupts JSONL
