---
title: "oxlint worker: avoid leaving empty suppressions files"
date: 2026-07-15
category: integration-issues
problem_type: integration_issue
component: luchta-oxlint-worker
root_cause: "finalize writes empty {} for no-suppression runs; no cleanup path"
resolution_type: code_fix
severity: low
tags:
  - oxc
  - suppressions
  - file-hygiene
  - log-consistency
  - serde_json
plan_ref: oxlint-empty-suppressions-231
---

## Problem

The `luchta-oxlint-worker` wrote an empty `{}` suppressions file via oxc's `SuppressionManager::finalize` when `--suppress-all` or prune operations produced no remaining suppressions. The empty file served no purpose but remained on disk, creating noise and dirty working trees.

## Symptoms

- Empty `oxlint-suppressions.json` file left in project root after clean lint runs
- Worker logged "wrote/updated oxlint-suppressions.json" for files immediately deleted
- Pre-existing empty suppressions files from prior runs persisted indefinitely

## Investigation Steps

1. Traced `SuppressionManager::finalize` in oxc_linter — writes file unconditionally, returns `OxlintSuppressionFileAction` enum indicating write status.
2. Identified caller site in `lint_files_blocking` (`crates/luchta-oxlint-worker/src/lint.rs`) — action passed to `FinalizeResult` flows into `suppression_log_lines` which emits "wrote/updated" text.
3. Noted two-phase problem: (a) empty file written, (b) log claims write happened even if we add deletion.
4. Designed helper to run post-finalize: read file, check for emptiness, delete if empty, return corrected action.

## Root Cause

`SuppressionManager::finalize` writes the suppressions file regardless of content. When suppression tracking produces no entries (all counts zero, suppress-all on clean codebase), the file contains only `{}` or nested-empty structures like `{"file.js": {}}`. No cleanup existed, and the log path lacked awareness of deletion.

## Solution

Added `remove_empty_suppressions_file` helper in `suppressions.rs`:

```rust
fn is_empty_suppressions_value(value: &Value) -> bool {
    match value {
        Value::Object(entries) => entries.values().all(is_empty_suppressions_value),
        _ => false,
    }
}

pub fn remove_empty_suppressions_file(
    path: &Path,
    action: OxlintSuppressionFileAction,
) -> Result<OxlintSuppressionFileAction, String> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(action),
        Err(error) => return Err(format!("failed to read suppressions file {}: {error}", path.display())),
    };

    let parsed: Value = serde_json::from_str(&contents)
        .map_err(|error| format!("failed to parse suppressions file {}: {error}", path.display()))?;

    if !is_empty_suppressions_value(&parsed) {
        return Ok(action);
    }

    match std::fs::remove_file(path) {
        Ok(()) => Ok(OxlintSuppressionFileAction::None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(OxlintSuppressionFileAction::None),
        Err(error) => Err(format!("failed to remove empty suppressions file {}: {error}", path.display())),
    }
}
```

Call site in `lint_files_blocking`:

```rust
manager.finalize(diff, &tx_error, &cwd)
    .map_err(|error| error.to_string())?;
let suppressions_path = cwd.join(SUPPRESSIONS_FILENAME);
let file_action = remove_empty_suppressions_file(&suppressions_path, manager.file_action.clone())?;
// ... FinalizeResult uses file_action
```

## Why This Works

1. **Post-finalize timing**: Helper runs after oxc writes the file, ensuring cleanup targets actual disk state.
2. **Action reset**: Returns `OxlintSuppressionFileAction::None` on successful deletion. `suppression_log_lines` treats `None` as no-op, preventing false "wrote/updated" messages.
3. **Recursive emptiness**: `is_empty_suppressions_value` treats `{"a": {}}` and deeper nested-empty as empty. For this schema, empty nested objects mean "no suppressions" — semantic match for issue intent.
4. **Vacuous truth simplification**: `Iterator::all()` returns `true` for empty iterators. No separate `is_empty()` guard needed — empty object `{}` matches correctly.
5. **Fail-loud on malformed**: Parse errors propagate as `Err`, failing the run. Intentional: alerts user to corruption rather than silently skipping.

## Prevention Strategies

**Test Coverage:**
- Unit tests: empty `{}`, nested-empty, non-empty retention, missing file (no-op), malformed JSON (error)
- Integration tests: `--suppress-all` clean run leaves no file, pre-existing empty file removed

**Code Review Checklist:**
- [ ] Does cleanup run after finalize completes?
- [ ] Does returned action reflect deletion (None) for log accuracy?
- [ ] Does emptiness predicate match schema semantics?
- [ ] Is `serde_json` gated behind `oxc` feature?

**Future Notes:**
- Helper assumes suppression schema shape (nested objects, leaf nodes have `count`). If oxc adds top-level metadata keys, predicate may need adjustment.
- Size-gate optimization (skip parsing for files >10KB) deferred — overhead negligible in practice.

## Related Issues

- **GitHub:** [#231](https://github.com/dobesv/luchta/issues/231) — oxlint: avoid empty suppressions files
- **Related Solution:** [oxc-worker-in-process-integration-2026-07-08.md](./oxc-worker-in-process-integration-2026-07-08.md) — SuppressionManager lifecycle and private-type workaround
