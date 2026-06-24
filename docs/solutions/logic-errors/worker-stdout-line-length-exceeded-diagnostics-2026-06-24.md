---
title: "Worker stdout line length exceeded crashes with misleading diagnostics"
date: 2026-06-24
category: logic-errors
problem_type: logic_error
component: luchta-engine/worker
root_cause: "LinesCodec error discarded in reader loop; MaxLineLengthExceeded matched as generic Err(_) before breaking"
resolution_type: code_fix
severity: high
tags:
  - tokio
  - LinesCodec
  - error-handling
  - diagnostics
  - worker-ipc
  - jsonl
plan_ref: worker-max-line-length
---

## Problem

Worker stdout lines larger than `MAX_LINE_LENGTH` (1 MiB) silently crashed the build job with only a misleading "delegate closed / Broken pipe" symptom. No diagnostic indicated the real cause: a `LinesCodecError::MaxLineLengthExceeded` was discarded in the reader's error handler before stopping the read loop.

## Symptoms

- Error: `delegate failed before done: io error: Broken pipe` (or `delegate closed`)
- No indication of line length violation or which line triggered failure
- Real-world trigger: SARIF `report` protocol message (~2.5 MB) for a large package
- Frequency: Deterministic on any worker output line exceeding 1 MiB

## Investigation Steps

1. Identified `io_tasks.rs` uses `tokio_util::codec::LinesCodec::new_with_max_length(MAX_LINE_LENGTH)` with `MAX_LINE_LENGTH = 1 << 20` (1 MiB)
2. Found `handle_reader_frame` matches `Err(_error) => { crash_reader_jobs(); Stop }` — discarding the error entirely
3. Traced downstream: worker proxy's next write to closed pipe fails with Broken pipe, which surfaces as the misleading symptom
4. Recognized `LinesCodecError` has two variants: `MaxLineLengthExceeded` and `Io(e)` — only the former was the real issue

## Root Cause

The reader loop's error handler matched `Err(_)` generically, discarding the `LinesCodecError` before calling `crash_reader_jobs()`. This converted a clear, local failure (line too long) into a confusing distant symptom (broken pipe in a different component). The operator had no way to distinguish codec overflow from genuine I/O failure.

```rust
// BEFORE — discards error
Err(_error) => {
    crash_reader_jobs();
    Stop
}
```

## Solution

Three-part fix with error surfacing and capacity increase:

1. **Raised `MAX_LINE_LENGTH` to `1 << 26` (64 MiB)** — report payloads legitimately large

2. **Surfaced error into existing diagnostic channel**: Plumbed `crash_state: Arc<Mutex<WorkerCrashState>>` into `ReaderContext`, and in the Err branch recorded a descriptive line via `record_stderr_line` before crashing jobs

3. **Distinguished error variants**:

```rust
// AFTER — surfaces specific error
Err(LinesCodecError::MaxLineLengthExceeded) => {
    record_stderr_line(
        &crash_state,
        format!(
            "worker output line exceeded MAX_LINE_LENGTH ({} bytes)",
            MAX_LINE_LENGTH
        ),
    );
    crash_reader_jobs();
    Stop
}
Err(LinesCodecError::Io(e)) => {
    record_stderr_line(
        &crash_state,
        format!("failed to read worker output: {e}"),
    );
    crash_reader_jobs();
    Stop
}
```

Added unit tests for both error paths. Added knope changeset.

## Why This Works

1. **Error surfaces where diagnostics already flow**: `record_stderr_line` writes to the same channel that feeds worker crash diagnostics, ensuring the message appears in the operator-visible error output

2. **Variant-specific messages name the true cause**: Operator can now distinguish codec overflow from generic I/O failure, avoiding misdiagnosis

3. **Capacity matches real payloads**: 64 MiB cap accommodates SARIF reports for large packages without triggering overflow

4. **Preserves existing error flow**: No change to crash detection/reaper pathway; only enriches the diagnostic content

## Prevention Strategies

### Test Cases

Added in `io_tasks` tests:
- Line exactly at `MAX_LINE_LENGTH` succeeds
- Line exceeding `MAX_LINE_LENGTH` by 1 byte surfaces "exceeded MAX_LINE_LENGTH" message
- Generic I/O error surfaces "failed to read worker output" message

### Best Practices

- **Never discard an error variant in a stream/reader loop**: `Err(_) =>` converts a clear, local failure into a confusing distant symptom. Always surface the error into whatever diagnostic channel the component already has
- **Match enum variants individually**: When a codec/parser error enum has multiple variants, match each separately so diagnostics name the true cause
- **Protocol stream caps must account for legitimate payloads**: If a message type (reports, logs) can legitimately be large, set caps accordingly or use alternative transport (file paths instead of inline content)

### Code Review Checklist

- [ ] Reader/stream error handlers surface the error, not discard it?
- [ ] Error enum variants matched individually with specific messages?
- [ ] Stream caps documented with rationale and tested against real payload sizes?
- [ ] Diagnostic channel (stderr, crash state) used for all failure paths?

### Future Mitigation

Issue #127 notes: file-based report transport (path instead of inline content) would decouple report size from stream cap entirely.

## Related Issues

- **GitHub:** [#127](https://github.com/dobesv/luchta/issues/127) — Worker stdout line length exceeded crashes without diagnostic
- **Prior Solution:** [worker-crash-diagnostics-ownership-boundary-2026-06-20.md](../integration-issues/worker-crash-diagnostics-ownership-boundary-2026-06-20.md) — Crash diagnostic plumbing via `WorkerCrashState`
- **Prior Solution:** [multiplexed-worker-backpressure-2026-06-14.md](../logic-errors/multiplexed-worker-backpressure-2026-06-14.md) — Same stdout reader architecture
