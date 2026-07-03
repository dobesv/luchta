---
title: "Watch output single-package compaction pitfall when reusing package-name helpers"
date: 2026-07-01
category: logic-errors
problem_type: logic_error
component: luchta-cli/watch
root_cause: shared-scope compaction helpers assumed scope displayed elsewhere, single-element case lost information
resolution_type: code_fix
severity: medium
tags:
  - watch-mode
  - output-formatting
  - compaction
  - edge-case
  - single-element
plan_ref: watch-output-164
---

## Problem

Reusing `progress_task_list::common_scope` + `format_package_set` for the watch-mode change-detected line produced wrong output for single affected package: `📝 {react-reporting}` instead of `📝 @formative/react-reporting`. The compaction helpers stripped the npm scope and wrapped in braces, dropping information that had nowhere else to be displayed.

## Symptoms

- Single-package change-detected line showed `📝 {react-reporting}` (braces, no scope)
- Expected `📝 @formative/react-reporting` (full name, no braces)
- Multi-package case worked correctly: `📝 pkg-{a,b}`

## Investigation Steps

1. Traced `format_change_detected_line` in `watch/driver.rs` — called `format_package_set` directly
2. `format_package_set` (progress_task_list.rs ~line 86) always wraps in `{...}`, scope stripping via `packages_for_display`
3. `common_scope` returns the package's own scope for single package
4. For 2+ packages: shared scope shown in running-task header, braces compact list
5. For 1 package: header doesn't exist, scope stripped and braces added → information loss
6. Root cause: helpers assume shared context (header) displays scope; standalone usage breaks this

## Root Cause

`format_package_set` designed for running-task status line where:
- 2+ packages share scope displayed in group header
- Scope-stripping + braces compact the set under that header
- Consumer sees header + compacted set → full information

Change-detected line is standalone — no header shows the scope. Reusing these helpers for single package:
1. `common_scope` returns `@formative` (package's own scope)
2. `packages_for_display` strips it → `react-reporting`
3. `format!` wraps in braces → `{react-reporting}`
4. Result: `📝 {react-reporting}` — lost scope, spurious braces

Helper invariants violated: single-element case has no "shared" context.

## Solution

Special-case `affected.len() == 1` in `format_change_detected_line`:

```rust
fn format_change_detected_line(affected: &HashSet<PackageName>) -> String {
    if affected.len() == 1 {
        let name = affected.iter().next().expect("len checked").to_string();
        return format!("📝 {name}")
            .if_supports_color(Stream::Stdout, |text| text.cyan())
            .to_string();
    }

    // 2+ packages: use compaction helpers
    let packages_set: BTreeSet<&str> = affected.iter().map(|p| p.as_str()).collect();
    let shared_scope = crate::progress_task_list::common_scope(&packages_set);
    let compacted = crate::progress_task_list::format_package_set(&packages_set, shared_scope);

    format!("📝 {}", compacted)
        .if_supports_color(Stream::Stdout, |text| text.cyan())
        .to_string()
}
```

Print full package name verbatim for single package. Keep compaction only for 2+.

## Why This Works

Single-package path bypasses helpers entirely, avoiding scope-stripping and brace-wrapping. Full package name (including npm scope) displays correctly: `📝 @formative/react-reporting`.

Multi-package path continues using compaction helpers as intended — the shared scope appears in the compacted output when meaningful, and brace expansion reduces visual noise.

## Prevention Strategies

**Test Cases:**
- Always test single-element case with exact-equality assertions (not `contains`)
- Test single scoped package: `@scope/pkg` — verify full name preserved, no braces
- Test multi-package compaction: `pkg-{a,b}` — verify works as before
- Test empty set separately if reachable (guards exist in driver)

**Code Review Checklist:**
- [ ] When reusing list/formatting helpers for standalone output, check single-element behavior
- [ ] Verify helper invariants: what context does helper assume is displayed elsewhere?
- [ ] If helper designed for grouped display, either: (a) special-case single element, or (b) recreate needed context

**Pattern Recognition:**
- Compaction/helpers that strip "shared" prefixes assume shared context visible elsewhere
- Standalone usage of grouped-display helpers often fails for n=1
- Test boundary conditions: empty, single, pair, many

## Related Issues

- **Issue:** [#164](https://github.com/dobesv/luchta/issues/164) — Improve watch mode output
- **Related Solution:** [status-line-compaction-2026-06-27.md](../logic-errors/status-line-compaction-2026-06-27.md) — Original compaction helpers design
