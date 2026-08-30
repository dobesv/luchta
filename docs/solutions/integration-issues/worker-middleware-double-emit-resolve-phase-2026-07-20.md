---
title: "Preventing double-emits and interleaved output in resolve middleware"
date: 2026-07-20
last_verified: 2026-08-30
category: integration-issues
problem_type: integration_issue
component: luchta-worker, luchta-extra-resolve-worker
root_cause: "DelegateHandle auto-forwards every response line; resolve middleware must suppress internal responses by id and serialize all remaining records through one shared writer"
resolution_type: code_fix
severity: high
tags:
  - rust
  - worker-middleware
  - DelegateHandle
  - stdout-forwarding
  - resolve-phase
  - ResponseFilteringWriter
  - SharedWriter
  - terminal-response
plan_ref: luchta-extra-resolve-worker
---

## Problem

`luchta-extra-resolve-worker` wraps two delegates: a resolve worker (resolve-phase only) and a run delegate (run phase, plus fallback when resolve returns Accept/Modify). `DelegateHandle::with_writers` auto-forwards EVERY delegate response line to its configured stdout writer. Middleware that also writes its own merged response causes duplicate/conflicting `resolved` lines unless stdout forwarding is selectively suppressed.

The middleware handles independent requests concurrently. Its filtered delegate records and synthetic `resolved` records therefore also have to use the same outer `SharedWriter` lock. Separate writers around separate `stdout()` handles do not provide one record-level serialization boundary and can interleave JSON bodies and newlines.

Additionally, the original `read_delegate_stdout` waiter contract in `luchta-worker/src/proxy.rs` removed+satisfied the in-flight waiter on the FIRST id-matching response — regardless of variant. `send_with_timeout` could return an intermediate `Log`/`Report` instead of the terminal `Resolved`, and a middleware matching on `Resolved` would fall through to a spurious prune.

## Symptoms

- Two `resolved` lines emitted for a single resolve request when forwarding to run delegate during resolve phase
- `send_with_timeout` returning non-terminal `Log`/`Report` responses instead of waiting for `Resolved`
- Spurious prune paths in middleware when delegate logs before resolving
- Integration tests asserting `responses.len() == 1` failing with 2 responses
- Invalid JSONL when a delegated run response and synthetic resolve response are written concurrently

## Investigation Steps

1. First implementation gave resolve worker a `sink()` stdout writer (correct) but run delegate used real stdout (wrong). During resolve-phase forwards, run delegate's auto-forward emitted one `resolved` line, wrapper emitted merged response — two lines.

2. Tried unconditional `.send()` without capturing response for resolve-phase — still double-emit because auto-forward happens regardless of whether caller reads the response.

3. Analyzed `read_delegate_stdout` in `proxy.rs` (~L500-560). Found waiter removed on first id-match, not terminal check. Log before Resolve caused early return.

4. Reviewed `luchta-command-filter` pattern — single delegate with real stdout for streaming. That pattern doesn't compose for two-phase resolve/run decisions.

5. The first suppression mechanism used one global `SwitchableStdoutWriter` mode. Once middleware dispatch became concurrent, one resolve request could sink an unrelated run response. Per-correlation filtering fixed that isolation problem, but initially left filtered delegate output and synthetic output behind different `SharedWriter` locks. Issue #325 consolidated both paths behind one lock.

## Root Cause

**Double-emit**: `DelegateHandle::with_writers` routes the stdout reader task through the configured `AsyncWrite`. Every parsed response line is written before the waiter is satisfied. A middleware that also writes its own final response writes twice.

**Non-terminal waiter bug**: `read_delegate_stdout` removed waiter on first id-match regardless of `WorkerResponse` variant. `Log`/`Report` satisfied the oneshot, returning intermediate response. Middleware expecting `Resolved` fell through `if let Resolved { .. }` branches.

**Interleaved output**: `write_worker_response` is atomic only relative to calls using the same `SharedWriter`. A separate filtered writer and synthetic writer can each lock successfully while writing to the same process stdout, so the JSONL protocol has no shared record boundary.

## Solution

### 1. Per-ID Filtering Inside One Shared Output Writer

Track suppressed resolve IDs in `ResponseFilter`. `ResponseFilteringWriter` buffers complete JSONL records, drops responses for suppressed IDs, and removes an ID when its terminal response arrives. Unrelated run output continues streaming while multiple resolve requests are in flight.

Place that filter inside the one `SharedWriter`, then give the run delegate and the middleware's synthetic response path clones of the same `Arc`:

```rust
let filter = ResponseFilter::default();
let stdout_writer = shared_response_writer(stdout(), filter.clone());
let run_delegate = DelegateHandle::with_writers(
    delegate_command,
    Arc::clone(&stdout_writer),
    stderr_writer,
    Some("delegate stderr: ".to_owned()),
);
```

The resolve-only worker still uses `tokio::io::sink()` because the middleware always merges and emits that decision itself. Before forwarding a resolve request to the run delegate, suppress only that request ID. The proxy writes the terminal delegate response through the filter before it satisfies the response waiter, so the filter clears the ID before the middleware emits the synthetic `resolved` response with the same ID.

### 2. Terminal Response Contract in proxy.rs

Add `is_terminal_response` function:
```rust
fn is_terminal_response(response: &WorkerResponse) -> bool {
    matches!(response, WorkerResponse::Resolved { .. } | WorkerResponse::Done { .. })
}
```

In `read_delegate_stdout` path, auto-forward EVERY response to writer (unchanged), but only remove+satisfy waiter for TERMINAL responses:
```rust
async fn process_delegate_line(line: String, ctx: &DelegateStdoutCtx) -> Result<(), ProxyError> {
    let response = parse_delegate_response(&line, ctx).await?;
    write_worker_response(&ctx.writer, &response).await?;
    deliver_terminal_response(&ctx.waiters, response).await;
    Ok(())
}

async fn deliver_terminal_response(waiters: &ResponseWaiters, response: WorkerResponse) {
    if is_terminal_response(&response) {
        if let Some(tx) = waiters.lock().await.remove(response.id()) {
            let _ = tx.send(Ok(response));
        }
    }
}
```

Non-terminal `Log`/`Report` auto-forward and leave waiter installed until terminal response.

### 3. Applying TaskModification to ResolveTask

`TaskModification::apply_to` targets `TaskDefinition`, not `ResolveTask`. Only `command` and `inputs` overlap. Reconstruct via struct-update:
```rust
let modified_resolve = ResolveTask {
    command: modification.command.unwrap_or(resolve.command.clone()),
    inputs: modification.inputs.unwrap_or(resolve.inputs.clone()),
    ..resolve.clone()
};
```

`depends_on`, `weight`, `dependencies` have no `ResolveTask` equivalent and are ignored (but present in the Modify result returned to caller).

## Why This Works

**Per-ID filtering**: Concurrent resolve requests cannot change a process-global forwarding mode or suppress unrelated run output. Terminal responses end suppression for exactly their own correlation ID.

**One shared output writer**: `write_worker_response` holds the `SharedWriter` lock across the serialized JSON body, newline, and flush. Sharing that exact outer lock between the delegate and synthetic paths makes a complete JSONL record the serialization boundary.

**Proxy ordering**: A delegated terminal response is filtered before its waiter is satisfied. The subsequent synthetic response therefore sees its ID unsuppressed and is emitted exactly once.

**Terminal response contract**: Backward-compatible. Well-behaved workers emitting single terminal line are unchanged. Run path still streams `Log`/`Report` and satisfies waiter on `Done`. Timeout fires if only non-terminal responses arrive.

**Struct-update pattern**: Cleanly handles partial modification without inventing faux-fields on `ResolveTask`.

## Prevention Strategies

### Test Cases
- Every middleware integration test must assert exactly ONE response per request
- Test delegate logging before resolve: mock worker emits `Log` then `Resolved`, assert `send_with_timeout` returns `Resolved`
- Force partial, yielding writes from concurrent synthetic and delegated responses, then parse every combined JSONL record
- Test workspace-level build after workspace-member edits: `cargo nextest run --workspace` catches dropped members

### Code Review Checklist
- [ ] Does middleware write its own response while delegate auto-forwards?
- [ ] Does every path writing to one protocol stream clone the same outer `SharedWriter`?
- [ ] Is suppression scoped by correlation ID rather than a global forwarding mode?
- [ ] Is `send_with_timeout` caller prepared for non-terminal responses?
- [ ] After editing root `Cargo.toml` workspace members, diff against base to confirm no existing member was displaced

### Process Notes
- `cargo build`/`clippy`/`metadata` still pass if a workspace member is dropped. Only escargot-based integration tests (`cargo build --package X`) catch the regression. Full `cargo nextest run --workspace` is required.
- CodeScene `cs delta` attributes pre-existing file-level smells as "new" the first time a file is touched in the baseline window. Interpret AGENTS.md gate as "don't introduce/worsen" and avoid out-of-scope refactors of shared lifecycle code.

## Related Issues

- **Issue:** [#253](https://github.com/dobesv/luchta/issues/253) — luchta-extra-resolve-worker for resolve+run phase separation
- **Issue:** [#325](https://github.com/dobesv/luchta/issues/325) — serialize filtered and synthetic responses through one output boundary
- **Related Solution:** [process-proxy-worker-chain-2026-06-14.md](process-proxy-worker-chain-2026-06-14.md) — DelegateHandle primitive and oneshot waiter pattern
- **Related Solution:** [delegate-exit-status-capture-2026-07-16.md](delegate-exit-status-capture-2026-07-16.md) — In-flight waiter management during shutdown
