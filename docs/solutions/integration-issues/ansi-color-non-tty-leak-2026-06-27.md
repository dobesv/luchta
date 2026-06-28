---
title: "ANSI color leaking onto non-TTY output in luchta CLI"
date: 2026-06-27
category: integration-issues
problem_type: integration_issue
component: luchta-cli
root_cause: "Raw owo-colors method calls bypassed TTY detection, emitting ANSI unconditionally"
resolution_type: code_fix
severity: medium
tags:
  - ansi-color
  - tty-detection
  - owo-colors
  - dry-run
  - no-color
  - cli-output
plan_ref: no-color-non-tty
issue: "#134, #43"
---

## Problem

Piped/redirected CLI output showed raw ANSI escape codes because color calls bypassed TTY detection. `NO_COLOR` and `CI` env vars were also ignored in affected code paths.

## Symptoms

- `luchta run build --dry-run | less` showed raw ANSI escape sequences
- Any piped or redirected output contained ANSI codes
- `NO_COLOR` env var did not suppress color in dry-run wave plan
- `CI` env var did not suppress color
- Error/notice messages in some paths showed ANSI when piped

## Investigation Steps

1. Checked dependency: `owo-colors` v4 with `supports-colors` feature — TTY-aware API available
2. Found TTY-aware pattern: `value.if_supports_color(Stream::Stdout, |t| t.color()).to_string()`
3. Grepped for raw color method calls: `rg -nE '\.(bold|red|yellow|cyan|dimmed|green|blue|magenta)\(' | rg -v if_supports_color`
4. Located offenders: dry-run wave plan formatting, error paths in `run/dispatch.rs`, notice messages
5. Reviewed reference pattern in `format.rs`: `colorize_sarif_level`/`colorize_sarif_marker` use stream-aware wrappers

## Root Cause

CLI uses `owo-colors` v4 with `supports-colors` feature. The TTY-aware API `if_supports_color(Stream, ...)` honors:
- TTY detection (`isatty`)
- `NO_COLOR` env var
- `CLICOLOR_FORCE` env var

But several code paths called raw color methods directly (`.bold()`, `.cyan()`, `.red()`, `.yellow()`, `.dimmed()`), which emit ANSI unconditionally regardless of stream type.

**Affected locations:**
- `compute_displayed_dry_run_waves` in `crates/luchta-cli/src/run.rs`
- `describe_planned_action` in `crates/luchta-cli/src/run.rs`
- Error/notice paths in `crates/luchta-cli/src/run/dispatch.rs`

## Solution

Wrap every color call in `if_supports_color(stream, ...)`:

1. **For stdout sites:** Use `Stream::Stdout`
2. **For stderr sites:** Use `Stream::Stderr`
3. **For helper functions:** Thread `Stream` parameter through the call chain

**Before:**
```rust
println!("{}", style("message").cyan().bold());
```

**After:**
```rust
use owo_colors::{OwoColorize, Stream};
println!("{}", "message".if_supports_color(Stream::Stdout, |t| t.cyan().bold().to_string()));
```

**Helper function pattern:**
```rust
fn describe_planned_action(action: &Action, stream: Stream) -> String {
    // Now can use if_supports_color(stream, ...) internally
}
```

**Grep command to find remaining raw calls:**
```bash
rg -nE '\.(bold|red|yellow|cyan|dimmed|green|blue|magenta)\(' crates/luchta-cli/src/ \
  | rg -v if_supports_color \
  | rg -v '_tests\.rs'
```

## Why This Works

`if_supports_color(Stream, closure)` checks:
1. Whether the stream is a TTY (`isatty` on file descriptor)
2. `NO_COLOR` env var (disables color)
3. `CLICOLOR_FORCE` env var (forces color)

When conditions indicate color should be suppressed, the closure is not called; the value is returned as plain text. This matches the `supports-color` crate semantics used by `owo-colors`.

## Prevention Strategies

**Test Cases:**
- Add `dry_run_emits_no_ansi_on_non_tty` test that runs dry-run WITHOUT `NO_COLOR` and asserts zero ESC bytes (`\x1b`) in captured stdout/stderr
- Integration tests under pipe (non-TTY) already strip color, so regression tests must explicitly check for ANSI bytes

**Code Review Checklist:**
- [ ] Is every `.bold()`, `.red()`, `.cyan()`, etc. wrapped in `if_supports_color`?
- [ ] Does the chosen stream match the output destination (`stdout` vs `stderr`)?
- [ ] Do helper functions that build colored strings accept a `Stream` parameter?

**Grep Audit:**
```bash
# Regular audit for raw color method calls
rg -nE '\.(bold|red|yellow|cyan|dimmed|green|blue|magenta)\(' \
  crates/luchta-cli/src/ \
  | rg -v if_supports_color \
  | rg -v '_tests\.rs'
```

## Related Issues

- **GitHub:** [#134](https://github.com/dobesv/luchta/issues/134) — ANSI color leaking onto non-tty output
- **GitHub:** [#43](https://github.com/dobesv/luchta/issues/43) — Original NO_COLOR support
- **Related Solution:** [worker-error-unified-failure-block-2026-06-23.md](worker-error-unified-failure-block-2026-06-23.md) — Also uses `if_supports_color(Stream::Stderr, ...)` for failure block rendering
