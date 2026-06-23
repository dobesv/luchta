---
title: "Error reports display in run output with IDE-clickable SARIF formatting"
date: 2026-06-22
category: logic-errors
problem_type: logic_error
component: luchta-cli/format-rs
root_cause: "divergent display paths; color-stream mismatch; non-clickable SARIF format"
resolution_type: code_fix
severity: medium
tags:
  - reports
  - sarif
  - ctrf
  - display
  - clickability
  - owo-colors
  - stream-mismatch
  - terminal
plan_ref: issue-113-error-reports
---

## Problem

PR #110 added structured worker error reports (SARIF/CTRF), but three display bugs: (1) reports only appeared in `luchta logs`, not during `luchta run` failures; (2) when rendered, they appeared AFTER the task's footer end-marker instead of inside the task block; (3) SARIF output wasn't IDE-clickable.

## Symptoms

- `luchta run` failures showed task footer, then reports — reports outside the task block
- `luchta logs` showed reports inside block correctly
- SARIF lines like `[error] message --> src/foo.ts:15:10` not clickable in terminals/IDEs
- ANSI escape codes leaked to redirected stderr on run failure path

## Investigation Steps

Traced `luchta logs` (crates/luchta-cli/src/logs.rs) vs `luchta run` failure path (crates/luchta-cli/src/run/dispatch.rs `format_captured_failure_logs`/`finalize_task_run`). Both rendered task output but independently. Found `render_reports_pretty` needed in both. Identified color-stream mismatch: run failure uses `eprint!` (stderr) but formatters hardcoded `Stream::Stdout`. Tested IDE clickability with SARIF output: needed `path:line:col:` at column 0.

## Root Cause

**Divergent display paths:** `logs.rs` and `dispatch.rs` had independent report-rendering logic. Only logs.rs rendered reports inside the block.

**Color-stream mismatch:** `owo_colors::if_supports_color(Stream::Stdout, ...)` hardcoded Stdout, but `eprint!` writes to stderr. ANSI leaked or was suppressed incorrectly.

**Non-clickable format:** SARIF `[level] message --> path:line:col` placed `path:line:col` mid-line. IDEs/terminals require `path:line:col:` at column 0 for Cmd+click.

**Multiline caret bug:** For multiline SARIF snippets, caret underline emitted after the whole block, landing under the wrong line.

## Solution

### 1. Shared helper (`render_reports_pretty`)

Created `render_reports_pretty` in `format.rs` consumed by both `logs.rs` and `dispatch.rs`:

```rust
// format.rs
pub fn render_reports_pretty(
    reports: impl IntoIterator<Item = ReportRenderInput>,
    stream: Stream,
) -> String {
    // MIME dispatch to sarif/ctrf/raw printers
}
```

Call sites pass the stream matching the actual sink:
- `logs.rs: Stream::Stdout`
- `dispatch.rs` failure path: `Stream::Stderr`

### 2. Stream-threaded color

All color in formatters uses `if_supports_color(stream, ...)`:

```rust
pub fn format_task_log_block(
    meta: &str,
    body: &str,
    reports: &str,
    stream: Stream,  // <-- added param
) -> String {
    // All markers/meta route through stream param
    label.if_supports_color(stream, |s| s.bold())
}
```

Regression test: `format_task_log_block_does_not_emit_ansi_for_captured_stream` asserts no `\u{1b}`.

### 3. IDE-clickable SARIF format

Changed to leading-location:

```text
src/foo.ts:15:10: error: Cannot find name 'x'. [TS2304]
```

Enriched rustc-style (when data available):

```text
src/foo.ts:15:12: error: Cannot find name 'stringss'. Did you mean 'string'? [TS2552]
    |
  15 |   const x: stringss = getVariant();
     |            ^^^^^^^^^
    = help: replace `stringss` with `string`
    = note: 'string' is declared here (lib.es5.d.ts:3:1)
```

Fields from `serde_sarif` 0.8 (generated types):
- `region.snippet.text` → snippet lines
- `region.start/end_line/column` → caret span
- `result.fixes[].description.text` → `= help:`
- `result.related_locations` → `= note:` lines

### 4. Block frame design (clickability constraint)

```text
╭─ @pkg#task · 12:34:56
<body and report lines, NOT prefixed — paths at column 0>
╰─ 0.1s · exit 1 · cache 191d16638e53
```

Interior body/report lines NOT prefixed. Prefixing would shift `path:line:col` off column 0, breaking click detection.

### 5. Multiline caret fix

Caret printed under FIRST snippet line:

```rust
// format_sarif_snippet_block
// Single-line span: start_column..=end_column
// Multiline span: start_column to end of first line
for (i, line) in snippet_lines.iter().enumerate() {
    if i == 0 {
        // print caret line immediately after first line
    }
}
```

## Why This Works

**Single renderer:** Both paths call `render_reports_pretty`. Behavior cannot diverge.

**Correct stream:** Each call site passes the stream matching its actual output sink. `if_supports_color` queries terminal support for the right stream.

**Clickable paths:** Leading-location format (`path:line:col:`) plus no-prefix interior lines keeps location at column 0.

**Accurate carets:** Caret printed immediately after first snippet line, using first-line column values.

## Prevention Strategies

**Test Cases:**
- Run failure path: capture stderr, assert reports inside block (before footer)
- Color-stream: assert no ESC byte in captured output
- SARIF clickability: grep output for `^src/.*\.ts:[0-9]+:[0-9]+:` pattern
- Multiline carets: verify caret under first line, not last

**Code Review Checklist:**
- [ ] Does new display logic use shared helper?
- [ ] Does color call pass correct `Stream` param?
- [ ] Do interior report lines stay at column 0 (no prefix)?
- [ ] Does SARIF snippet caret render under first line?

**Design Constraint Record:**
- Clickability requires `path:line:col:` at column 0 — any log framing change must not prefix interior lines

## Related Issues

- **Prior work:** [logic-errors/worker-reports-schema-migration-2026-06-21.md](./worker-reports-schema-migration-2026-06-21.md) — PR #110: bincode schema, shared-cache parity, filename safety
- **GitHub:** Issue #113 — display/UX follow-ups
