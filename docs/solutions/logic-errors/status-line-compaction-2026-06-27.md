---
title: "Status-line task-list compaction with consistent separators and prefix handling"
date: 2026-06-27
category: logic-errors
problem_type: logic_error
component: luchta-cli
root_cause: inconsistent rendering logic and unsafe string operations
resolution_type: code_fix
severity: medium
tags:
  - string-rendering
  - prefix-compaction
  - utf8
  - status-line
  - grouping
plan_ref: status-line-compaction
---

## Problem

Running-tasks status line in `progress_task_list.rs` had inconsistent rendering: grouped tasks used `:` to join package(set)→task while individual tasks used `#`. Package prefix compaction stopped at npm scope boundaries, missing deeper common prefixes. Root sentinel `//root` could leak in edge cases. Overflow truncation (`+N`) hid running tasks.

## Symptoms

- `{a,b}:lint` vs `pkg#build` — different separators for same logical join
- `@formative/{server-answers,server-changes,...}` instead of more compact `@formative/server-{answers,changes,...}`
- Potential malformed output like `server-{,api}` when prefix equals a package name
- `trim_start_matches` over-stripping: `server-server-a` → `a` (should be `server-server-a`)

## Investigation Steps

Reviewed `render_running_task_groups` and `format_package_set`. Found:
1. Separator `:` hardcoded in grouped path, `#` in individual path
2. Prefix compaction only stripped npm scope, not word-boundary prefixes
3. `trim_start_matches` used instead of `strip_prefix` — over-strips repeated occurrences
4. No guard against empty suffixes after prefix removal
5. UTF-8 byte-slicing without `char_indices()` could panic on multi-byte names

## Root Cause

1. **Separator inconsistency**: Two code paths used different join characters without shared constant
2. **Incomplete compaction**: Only npm scope (`@scope/`) was stripped, not common word-boundary prefixes
3. **Unsafe prefix removal**: `trim_start_matches` treats prefix as character set, not literal — strips all leading occurrences
4. **Missing empty-suffix guard**: If one package equals the prefix, suffix would be empty → malformed `{,suffix}`
5. **Byte-slicing on UTF-8**: Slicing at byte offset without checking char boundaries

## Solution

### Separator Consistency
Changed all package(set)→task joins to use `#`:
```rust
// Before (grouped)
format!("{}:{}", format_package_set(&packages), task_name)
// After
format!("{}#{}", format_package_set(&packages), shared_scope), task_name)
```

### Two-Level Prefix Compaction
Compute shared scope ONCE over entire shown set, thread through all rendering paths:
```rust
pub(crate) fn render_running_task_groups(shown: &[&TaskId]) -> String {
    let shared_scope = shared_scope_for_tasks(shown);
    let (mut rendered, consumed) = group_by_shared_task_name(shown, shared_scope);
    rendered.extend(group_remaining_by_package(shown, &consumed, shared_scope));
    rendered.join(", ")
}

fn shared_scope_for_tasks<'a>(shown: &[&'a TaskId]) -> Option<&'a str> {
    let packages = shown
        .iter()
        .filter(|task| !task.package.is_root())
        .map(|task| task.package.as_str())
        .collect::<BTreeSet<_>>();
    common_scope(&packages)
}
```

Then compact longest shared prefix at word boundary (`-`, `/`, `.`):
```rust
fn longest_shared_boundary_prefix<'a>(packages: &[&'a str]) -> Option<&'a str> {
    let first = *packages.first()?;
    let max_len = shared_prefix_len(packages);
    separator_boundaries(first, max_len)
        .rev()  // longest → shortest
        .find_map(|index| {
            let prefix = &first[..index];
            all_suffixes_non_empty(packages, prefix).then_some(prefix)
        })
}
```

### Safe Prefix Stripping
Use `strip_prefix`, never `trim_start_matches`:
```rust
// WRONG: trim_start_matches treats prefix as char set
package.trim_start_matches(prefix)  // "server-server-a" with prefix "server-" → "a"

// CORRECT: strip_prefix treats prefix as literal
package.strip_prefix(prefix).unwrap_or(package)  // "server-server-a" → "server-a"
```

### Empty-Suffix Guard
Only apply prefix if ALL suffixes are non-empty:
```rust
fn all_suffixes_non_empty(packages: &[&str], prefix: &str) -> bool {
    packages.iter().all(|package| {
        package
            .strip_prefix(prefix)
            .is_some_and(|suffix| !suffix.is_empty())
    })
}
```

### UTF-8 Safe Boundaries
Build separator indices from `char_indices()` + `len_utf8()`:
```rust
fn separator_boundaries(
    package: &str,
    max_len: usize,
) -> impl DoubleEndedIterator<Item = usize> + '_ {
    package
        .char_indices()
        .filter_map(move |(index, ch)| is_word_separator(ch).then_some(index + ch.len_utf8()))
        .filter(move |index| *index <= max_len)
}
```

### Root Sentinel Protection
Filter root packages before grouping, render as `#{...}`:
```rust
pub(crate) fn shared_task_name_packages<'a>(tasks: &'a [(usize, &'a TaskId)]) -> BTreeSet<&'a str> {
    tasks
        .iter()
        .filter(|(_, task)| !task.package.is_root())
        .map(|(_, task)| task.package.as_str())
        .collect()
}
```

## Why This Works

- **Consistency**: Single separator `#` used across all paths matches user expectation and config syntax
- **Scope-once**: Computing shared scope once ensures consistent output whether packages group by shared task or render separately
- **Word-boundary compaction**: Compacting at `-`, `/`, `.` produces readable `{server-answers,server-changes}` → `server-{answers,changes}`
- **Empty-suffix guard**: Prevents malformed output when prefix would consume entire package name
- **UTF-8 safety**: `char_indices()` ensures byte-slicing stays on character boundaries
- **Root masking**: Filtering `is_root()` before grouping prevents sentinel `//root` from appearing in output

## Prevention Strategies

**Test Cases:**
```rust
#[test]
fn format_package_set_repeated_prefix_keeps_literal_prefix_once() {
    assert_eq!(
        format_packages(&["@scope/server-server-a", "@scope/server-server-b"]),
        "server-server-{a,b}"
    );
}

#[test]
fn format_package_set_rejects_prefix_that_would_leave_empty_suffix() {
    assert_eq!(format_packages(&["pkga-", "pkga-api"]), "{pkga-,pkga-api}");
}

#[test]
fn format_package_set_compacts_utf8_prefix_safely() {
    assert_eq!(
        format_packages(&["@scope/café-a", "@scope/café-b"]),
        "café-{a,b}"
    );
}
```

**Best Practices:**
- Always use `strip_prefix(prefix).unwrap_or(s)` for literal prefix removal — never `trim_start_matches`
- Guard against empty suffixes when compacting; iterate longest→shortest and pick first where all suffixes non-empty
- When computing display transformations over a set, compute once and thread through all paths
- Use `char_indices()` + `len_utf8()` for UTF-8-safe byte slicing
- Table-driven tests avoid Code-Duplication in static analysis (CodeScene `cs delta`)

**Code Review Checklist:**
- [ ] Are all package(set)→task joins using `#`?
- [ ] Is prefix stripping using `strip_prefix`, not `trim_start_matches`?
- [ ] Is there an empty-suffix guard before applying compacted prefix?
- [ ] Are string slices computed from `char_indices()` for UTF-8 safety?
- [ ] Is shared scope computed once over the whole set and passed to all renderers?

## Related Issues

- **GitHub:** [#145](https://github.com/dobesv/luchta/issues/145) — Consistent `#` join separator
- **GitHub:** [#146](https://github.com/dobesv/luchta/issues/146) — Compact prefixes at word boundaries
- **Plan:** `status-line-compaction`
