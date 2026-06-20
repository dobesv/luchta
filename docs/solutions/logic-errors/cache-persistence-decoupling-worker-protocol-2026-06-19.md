---
title: "Cache-persistence decoupling and worker-protocol log-message fixtures"
date: 2026-06-19
category: logic-errors
problem_type: logic_error
component: luchta-cli/cache-layer
root_cause: "Incorrect coupling between cacheability gates and local persistence; test fixture workers emitting raw stdout instead of JSON protocol messages"
resolution_type: code_fix
severity: high
tags:
  - cache-persistence
  - skip-gate
  - worker-protocol
  - jsonl
  - test-fixtures
  - integration-testing
plan_ref: luchta-logs-output-ux
---

## Problem

Two distinct issues emerged during implementation of `luchta logs` command and improved failure-output display:

1. **Cache-persistence coupling**: Attempting to persist run records only for cacheable tasks caused logs to be missing for non-cacheable task failures. Naive changes to "unconditionally persist" risked breaking cache-skip semantics.

2. **Worker-protocol test fixtures**: Integration tests for output truncation needed workers that emit captured stdout/stderr, but shell-script fixtures printing directly to stdout corrupted the JSONL protocol stream, causing worker crashes and empty captured logs.

## Symptoms

**Cache-persistence issue:**
- Non-cacheable tasks had no `.luchta/cache/<hash>/` directory
- `luchta logs` could not find logs for non-cacheable tasks
- Naive "move persistence outside cache_enabled gate" risked making non-cacheable tasks skip on subsequent runs

**Worker-protocol issue:**
```
Error: Worker crashed with no output
Captured logs: empty
```
- Shell workers using `echo "line 1"` directly to stdout caused engine JSONL parser failures
- Test failures: `run_failure_output_truncates_live_replay` showed empty captured output
- Debugging revealed worker process exit before emitting expected 150 lines

## Investigation Steps

**Cache-persistence:**
1. Reviewed `dispatch.rs` skip path: `if cache_enabled { try_cache_skip() }` — the skip logic relies on `decide()` which has no `cache_enabled` field
2. Traced `CacheWriteContext` construction — it was only built inside the `cache_enabled` branch
3. Identified that `decide()` in `luchta-cache/src/decide.rs` depends solely on prior `TaskRunRecord` + current file hashes
4. Recognized that persistence must be unconditional BUT the skip gate must remain untouched

**Worker-protocol:**
1. Examined shell fixture: `echo "line $i"` emitted raw stdout
2. Traced engine's `LinesCodec` JSONL framing — expects `{"type":"log",...}` messages
3. Raw stdout (just `line 1\n`) is not valid JSON → parser rejects line → worker continues but engine ignores output
4. Discovered correct pattern from `run_failure_output_integration.rs`: `printf '{"type":"log","id":"%s","stream":"stdout","line":"line %d"}\n'`

## Root Cause

**Cache-persistence:** The `CacheWriteContext` was tied to cacheability because shared-cache upload needed conditional gating. However, local persistence (meta.bincode, stdout.log, stderr.log) was incorrectly bundled with shared-cache persistence. The skip gate `if cache_enabled { try_cache_skip() }` must remain because `decide()` has no knowledge of cache_enabled — it purely compares prior record hashes against current file state. Making persistence unconditional is only safe if the skip gate stays intact.

**Worker-protocol:** The luchta engine communicates with workers via JSONL (JSON Lines) protocol. Each line must be a valid JSON object with a `type` field. Workers emit `{"type":"log","id":"<job_id>","stream":"stdout","line":"..."}` for captured output. Raw text to stdout bypasses the protocol and corrupts the stream. Tests using shell-script fixtures must emit EACH output line as a JSON `log` message, not raw stdout.

## Solution

### Cache-Persistence Decoupling

Changed `build_task_run_context` to unconditionally build `CacheWriteContext` for every executed task:

```rust
fn build_task_run_context(
    task_id: &TaskId,
    _cache_enabled: bool,  // Used for gating, not for context building
    ctx: &DispatchContext<'_>,
) -> TaskRunContext {
    let cache_write = match build_cache_write_context(task_id, ctx) {
        CacheInputState::Ready(cache_ctx) => Some(*cache_ctx),
        CacheInputState::Disabled => None,
    };
    // ... rest of context
}
```

The skip gate remains unchanged:

```rust
let cache_enabled = ctx.task_graph.task_definition(&task_id)
    .is_some_and(TaskDefinition::cache_enabled);
if cache_enabled {
    if let Some(decision) = try_cache_skip(&task_id, ctx) {
        match decision {
            Decision::Skip => { /* ... skip path */ }
            Decision::SharedHit => { /* ... shared cache hit */ }
            Decision::Run => {}
        }
    }
}
// After this point, ALL tasks run through spawn_task_runner with CacheWriteContext
```

Shared-cache upload gated by separate flag:

```rust
persist_cache_state(CachePersistInputs {
    // ... other fields
    shared_cache: cache_enabled.then_some(shared_cache).flatten(),
    shared_store_enabled: cache_enabled,  // Non-cacheable tasks: false
    // ...
}).await;
```

### Worker-Protocol Log-Message Fixtures

Correct shell-worker pattern for emitting captured output:

```sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"type":"resolveTask"'*)
      printf '{"type":"resolved","id":"%s","result":{"decision":"accept"}}\n' "$id"
      ;;
    *'"type":"run"'*)
      WORKER_ID="$id" python3 - <<'PY'
import json
import os

worker_id = os.environ["WORKER_ID"]
for i in range(1, 151):
    print(json.dumps({"type": "log", "id": worker_id, "stream": "stdout", "line": f"line {i}"}))
print(json.dumps({"type": "done", "id": worker_id, "exitCode": 7}))
PY
      ;;
  esac
done
```

Key requirements:
- Each output line must be `{"type":"log","id":"<job_id>","stream":"stdout|stderr","line":"<content>"}`
- Terminal message must be `{"type":"done","id":"<job_id>","exitCode":<int>}`
- Never print raw text to stdout — it corrupts JSONL stream

## Why This Works

**Cache-persistence:**
1. `decide()` operates on prior `TaskRunRecord` + current file state — no `cache_enabled` field
2. The skip gate `if cache_enabled { try_cache_skip() }` remains the ONLY path that gates cache hits
3. Unconditional `CacheWriteContext` ensures all executed tasks get local persistence
4. Separate `shared_store_enabled: cache_enabled` flag gates shared-cache upload while allowing local persistence

This maintains correct semantics: cacheable tasks can skip on subsequent runs; non-cacheable tasks always persist locally but never to shared cache.

**Worker-protocol:**
- Engine's `LinesCodec` parses each line as JSON
- Valid `log` messages get captured into `ExecutionLogSink`
- Invalid lines (raw stdout) get dropped or cause parse errors
- Using JSON `log` messages ensures captured output flows through the same path as real worker output

## Prevention Strategies

### Test Cases

**Cache-persistence:**
- `non_cacheable_task_persists_local_run_record_but_still_reruns`: Verify local `.luchta/cache/<hash>/` exists after run, but second run executes task again (no skip)
- `non_cacheable_task_stays_out_of_shared_cache_but_writes_local_record`: Verify shared cache receives no upload for non-cacheable task

**Worker-protocol:**
- Verify test worker emits valid JSONL before debugging empty captured output
- Use `python3 -c 'import json; print(json.dumps({...}))'` for reliable JSON generation
- Assert captured line count matches expected count in integration tests

### Best Practices

**Cache design:**
- Separate "should I persist?" from "should I skip?"
- Cache-hit logic should operate on record content, not configuration flags
- Thread separate flags for different persistence targets (local vs shared)
- Gate shared-cache operations separately from local persistence

**Worker fixtures:**
- Always use JSONL protocol messages for test workers
- Each stdout/stderr line requires `{"type":"log","id":"...","stream":"...","line":"..."}`
- Terminal message requires `{"type":"done","id":"...","exitCode":...}`
- Never emit raw text to stdout in worker processes

### Code Review Checklist

- [ ] Cache skip gate touches `if cache_enabled { try_cache_skip() }` ONLY?
- [ ] `CacheWriteContext` built unconditionally for all executed tasks?
- [ ] Shared-cache upload uses separate `shared_store_enabled` flag?
- [ ] `decide()` has no `cache_enabled` dependency?
- [ ] Worker fixture emits JSON `log` messages, not raw stdout?
- [ ] Worker fixture terminal message uses `exitCode` (not `success: true`)?

## Related Issues

- **GitHub:** [#50](https://github.com/dobesv/luchta/issues/50) — `luchta logs` command
- **GitHub:** [#94](https://github.com/dobesv/luchta/issues/94) — Truncate large failed-task output
- **GitHub:** [#102](https://github.com/dobesv/luchta/issues/102) — Wrap failed-task output in header/footer block
- **Related Solution:** [integration-issues/resident-worker-process-management-2026-06-09.md](../integration-issues/resident-worker-process-management-2026-06-09.md) — Worker process lifecycle and JSONL IPC fundamentals
- **Related Solution:** [workflow-issues/wave-bucketed-progress-reporter-2026-06-13.md](../workflow-issues/wave-bucketed-progress-reporter-2026-06-13.md) — Sink installation for all tasks regardless of cache configuration
