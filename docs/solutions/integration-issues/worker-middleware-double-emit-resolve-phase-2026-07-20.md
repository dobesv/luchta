---
title: "Preventing double-emit in worker middleware during resolve-phase forwarding"
date: 2026-07-20
category: integration-issues
problem_type: integration_issue
component: luchta-worker, luchta-extra-resolve-worker
root_cause: "DelegateHandle auto-forwards every response line to stdout; middleware that also writes merged responses emits duplicate protocol lines"
resolution_type: code_fix
severity: high
tags:
  - rust
  - worker-middleware
  - DelegateHandle
  - stdout-forwarding
  - resolve-phase
  - SwitchableStdoutWriter
  - terminal-response
plan_ref: luchta-extra-resolve-worker
---

## Problem

`luchta-extra-resolve-worker` wraps two delegates: a resolve worker (resolve-phase only) and a run delegate (run phase, plus fallback when resolve returns Accept/Modify). `DelegateHandle::with_writers` auto-forwards EVERY delegate response line to its configured stdout writer. Middleware that also writes its own merged response causes duplicate/conflicting `resolved` lines unless stdout forwarding is selectively suppressed.

Additionally, the original `read_delegate_stdout` waiter contract in `luchta-worker/src/proxy.rs` removed+satisfied the in-flight waiter on the FIRST id-matching response — regardless of variant. `send_with_timeout` could return an intermediate `Log`/`Report` instead of the terminal `Resolved`, and a middleware matching on `Resolved` would fall through to a spurious prune.

## Symptoms

- Two `resolved` lines emitted for a single resolve request when forwarding to run delegate during resolve phase
- `send_with_timeout` returning non-terminal `Log`/`Report` responses instead of waiting for `Resolved`
- Spurious prune paths in middleware when delegate logs before resolving
- Integration tests asserting `responses.len() == 1` failing with 2 responses

## Investigation Steps

1. First implementation gave resolve worker a `sink()` stdout writer (correct) but run delegate used real stdout (wrong). During resolve-phase forwards, run delegate's auto-forward emitted one `resolved` line, wrapper emitted merged response — two lines.

2. Tried unconditional `.send()` without capturing response for resolve-phase — still double-emit because auto-forward happens regardless of whether caller reads the response.

3. Analyzed `read_delegate_stdout` in `proxy.rs` (~L500-560). Found waiter removed on first id-match, not terminal check. Log before Resolve caused early return.

4. Reviewed `luchta-command-filter` pattern — single delegate with real stdout for streaming. That pattern doesn't compose for two-phase resolve/run decisions.

## Root Cause

**Double-emit**: `DelegateHandle::with_writers` routes the stdout reader task through the configured `AsyncWrite`. Every parsed response line is written before the waiter is satisfied. A middleware that also writes its own final response writes twice.

**Non-terminal waiter bug**: `read_delegate_stdout` removed waiter on first id-match regardless of `WorkerResponse` variant. `Log`/`Report` satisfied the oneshot, returning intermediate response. Middleware expecting `Resolved` fell through `if let Resolved { .. }` branches.

## Solution

### 1. SwitchableStdoutWriter for Run Delegate

Give run delegate a wrapper-local `SwitchableStdoutWriter` — a custom `AsyncWrite` over an `Arc<AtomicU8>` mode. Set `FORWARD_SINK` (discard) before each resolve-phase `send_with_timeout` call, restore `FORWARD_REAL` after. Wrapper emits single merged response.

```rust
const FORWARD_REAL: u8 = 0;
const FORWARD_SINK: u8 = 1;

struct SwitchableStdoutWriter {
    mode: Arc<AtomicU8>,
    real: Stdout,
    sink: Sink,
}

impl AsyncWrite for SwitchableStdoutWriter {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.active().with(|w| w.poll_write(cx, buf))
    }
    // poll_flush, poll_shutdown similar
}
```

Usage in resolve-phase forward:
```rust
async fn resolve_via_run_delegate(&self, task: ResolveTask) -> WorkerResponse {
    self.run_forward_mode.store(FORWARD_SINK, Ordering::SeqCst);
    let result = self.run_delegate.send_with_timeout(
        WorkerMessage::ResolveTask(task), RESOLVE_TIMEOUT
    ).await;
    self.run_forward_mode.store(FORWARD_REAL, Ordering::SeqCst);
    // wrapper writes merged response, sink discarded delegate's auto-forward
    result
}
```

Resolve-only worker (not consulted for run phase) uses `tokio::io::sink()` directly as stdout writer.

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
    write_response(&ctx.writer, &response).await?;
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

**SwitchableStdoutWriter**: Safe because worker stdin loop is single-threaded (`current_thread` tokio runtime) and processes messages strictly sequentially. No concurrent resolve-phase forwards; mode flip is atomic with respect to the write.

**Terminal response contract**: Backward-compatible. Well-behaved workers emitting single terminal line are unchanged. Run path still streams `Log`/`Report` and satisfies waiter on `Done`. Timeout fires if only non-terminal responses arrive.

**Struct-update pattern**: Cleanly handles partial modification without inventing faux-fields on `ResolveTask`.

## Prevention Strategies

### Test Cases
- Every middleware integration test must assert exactly ONE response per request
- Test delegate logging before resolve: mock worker emits `Log` then `Resolved`, assert `send_with_timeout` returns `Resolved`
- Test workspace-level build after workspace-member edits: `cargo nextest run --workspace` catches dropped members

### Code Review Checklist
- [ ] Does middleware write its own response while delegate auto-forwards?
- [ ] Is `send_with_timeout` caller prepared for non-terminal responses?
- [ ] After editing root `Cargo.toml` workspace members, diff against base to confirm no existing member was displaced
- [ ] Does `SwitchableStdoutWriter` mode restore on ALL paths (success, error, panic)?

### Process Notes
- `cargo build`/`clippy`/`metadata` still pass if a workspace member is dropped. Only escargot-based integration tests (`cargo build --package X`) catch the regression. Full `cargo nextest run --workspace` is required.
- CodeScene `cs delta` attributes pre-existing file-level smells as "new" the first time a file is touched in the baseline window. Interpret AGENTS.md gate as "don't introduce/worsen" and avoid out-of-scope refactors of shared lifecycle code.

## Related Issues

- **Issue:** [#253](https://github.com/dobesv/luchta/issues/253) — luchta-extra-resolve-worker for resolve+run phase separation
- **Related Solution:** [process-proxy-worker-chain-2026-06-14.md](process-proxy-worker-chain-2026-06-14.md) — DelegateHandle primitive and oneshot waiter pattern
- **Related Solution:** [delegate-exit-status-capture-2026-07-16.md](delegate-exit-status-capture-2026-07-16.md) — In-flight waiter management during shutdown
