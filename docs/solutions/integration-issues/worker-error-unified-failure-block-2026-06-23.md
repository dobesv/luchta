---
title: "Unify worker error/crash output with task-failure block rendering"
date: 2026-06-23
category: integration-issues
problem_type: integration_issue
component: luchta-cli/output-formatting
root_cause: "Worker crash errors rendered via bare eprintln outside styled block; two separate output paths created inconsistent UX"
resolution_type: code_fix
severity: medium
tags:
  - output-formatting
  - worker-crash
  - ux-consistency
  - failure-rendering
  - fallback-detail
plan_ref: worker-error-output-box
---

## Problem

Worker crash/exec errors (`Err(ExecutorError)` wrapping `WorkerError::Crashed`) printed as a bare `eprintln!` line outside the styled rounded-corner failure block. Failed task output rendered inside the `╭─`/`╰─` box via `format_task_log_block`, but worker errors appeared as uncolored text after the box. For worker crashes, captured stdout/stderr is typically empty, so the only diagnostic sat raw and unstyled — inconsistent and hard to read.

## Symptoms

- Worker crash message: bare `eprintln!("task '{task_id}' {detail}")` without box styling
- Task failure message: rendered inside colored `╭─`/`╰─` block
- Two different visual treatments for failure scenarios
- Worker crash details easily missed when captured output was empty
- Duplicate diagnostic lines when both paths triggered

## Investigation Steps

Reviewed `finalize_task_run` and found two output paths:
1. `format_captured_failure_logs` → `format_task_log_block` → styled box with task logs
2. `report_task_failure` → bare `eprintln!` → unstyed summary line

Worker crashes flow through path 2, but captured logs are empty, so the box (path 1) renders empty while the only useful diagnostic (path 2) sits outside it.

Attempted simple fix: only use fallback detail when body empty. Review caught edge case: worker printing output *before* crashing needs both visible — crash cause would be lost.

## Root Cause

Two separate rendering paths for failure output:
- `format_captured_failure_logs` builds a styled block with captured stdout/stderr
- `report_task_failure` prints a bare line with error detail

No mechanism to combine them. When captured output empty (common for crashes), block is useless; detail lives outside and unstyled.

## Solution

Added `fallback_detail: Option<String>` to `FailureLogContext`. In `finalize_task_run`, pass worker crash error text as `fallback_detail`. In `format_captured_failure_logs`, after assembling captured body, always surface `fallback_detail` inside the same block:

```rust
if let Some(detail) = fallback_detail {
    if body.trim().is_empty() {
        body = detail;
    } else {
        if !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&detail);
    }
}
```

If body empty → fallback becomes body; if non-empty → append as trailing line. Guarantees crash cause visible even when worker emitted output before crashing.

Removed redundant `report_task_failure` function; deleted bare `eprintln!` line. Simplified `report_task_outcome` to only set `any_failed` flag. Kept `format_task_error` for building `fallback_detail`.

All failure output now flows through `format_task_log_block(..., Stream::Stderr)`, using `owo-colors` `if_supports_color` (respects `NO_COLOR`).

### Tests Added

- Unit test: `format_captured_failure_logs_uses_fallback_detail_when_output_empty` — fallback becomes body
- Unit test: `format_captured_failure_logs_appends_fallback_detail_after_output` — captured output precedes appended detail
- Integration test: `worker_crash_renders_single_wrapped_failure_block` — asserts exactly one `╭─`/`╰─` block with crash detail inside; detail appears exactly once

## Why This Works

Centralizing failure rendering removes scattered `eprintln!` styling and duplicate output. The `fallback_detail` inside the block ensures crash diagnostics are visible in all cases:
- Empty captured output: crash detail is the body
- Partial output: crash detail appended after captured logs

One styled block contains all failure information. No more "outside the box" diagnostics.

## Prevention Strategies

**Code Review Checklist:**
- [ ] When merging output paths, consider partial data case (output + error)
- [ ] Fallback/error detail should flow through same rendering path as primary content
- [ ] Test assertions should verify single rendering of diagnostic text

**Testing Patterns:**
- Assert exactly one occurrence of diagnostic text in output
- Test both empty-body and body-with-fallback scenarios
- Integration tests for end-to-end rendering consistency

## Related: #115 — Color in status lines

Run status/progress/summary lines (and the interrupt line) were plain, no color. Colorized via owo-colors `if_supports_color(stream, |t| t.<style>())`, which auto-respects `NO_COLOR` and tty detection, degrading to byte-identical plain text otherwise.

**Implementation:** Threaded an `owo_colors::Stream` parameter through `ProgressReporter::render_progress` / `render_summary` and `pressure_suffix` in `crates/luchta-cli/src/progress.rs`. Call sites pass the correct stream: progress + interrupt lines → `Stream::Stderr` (printed via eprintln in `run/pause.rs`), summary → `Stream::Stdout` (println in `run/setup.rs`).

**Palette:**
- done `✔` → green; skipped `⏩` / shared `📥` → cyan
- pending `⌛`, elapsed `⌚`, rss `🐏`, waves `🌊` → dimmed
- running `🏃` → yellow
- pressure/interrupt warnings → red

**Gotcha:** Two interrupt code paths in `run/pause.rs` (main `dispatch_loop` and `interrupted_during_pause`); both must be styled identically — the second was initially missed. When adding styling to a behavior, grep for ALL emit sites.

**Testing:** Assert plain path emits NO ANSI (`with_override(false)`) AND forced path emits ANSI (`with_override(true)`) for both progress and summary; nextest uses process isolation so global override is safe.

**Known pre-existing follow-ups (out of scope):** Config/expansion error lines in `run/dispatch.rs` still use bare `.red()`/`eprintln!` rather than stream-aware path; `✔` count combines done+skipped which can read ambiguously alongside separate `⏩` skipped count.

## Related Issues

- **GitHub:** [#121](https://github.com/dobesv/luchta/issues/121) — Improve worker error output
- **GitHub:** [#115](https://github.com/dobesv/luchta/issues/115) — Color in status lines
- **Prior Solution:** [worker-crash-diagnostics-ownership-boundary-2026-06-20.md](./worker-crash-diagnostics-ownership-boundary-2026-06-20.md) — Established crash error capture; this solution unifies rendering
