---
title: "Single detected_patterns flag conflation caused input detection regression"
date: 2026-06-12
category: logic-errors
problem_type: logic_error
component: luchta-cache/record
root_cause: "Single boolean flag used for both input and output detection decisions, while write side only set it from detected outputs"
resolution_type: code_fix
severity: high
tags:
  - cache-invalidation
  - detected-patterns
  - worker-protocol
  - flag-conflation
  - bisect-debugging
plan_ref: luchta-build-cache
---

## Problem

A single `detected_patterns: bool` field in `TaskRunRecord` was used to gate both input and output pattern decisions. The write side only set this flag from detected outputs (`outcome.detected_outputs.is_some()`), ignoring detected inputs. A naive one-line fix to the input-pattern selection logic silently regressed any task that relied on worker-detected inputs (e.g., yarn-worker's `package.json` detection).

## Symptoms

```
- Test: cache_yarn_worker_detected_package_json_input_reruns_on_package_edit FAILED
- Expected: Second run skips (package.json unchanged)
- Actual: Second run always reruns (declared inputs checked instead of detected)
```

Frequency: Occurred when fixing CodeRabbit #699 (declared-input changes should rerun). The "fix" passed its own test but broke the previously-passing yarn-worker test.

## Investigation Steps

1. Applied one-line fix to `effective_input_patterns` in decide.rs to use declared inputs when `detected_patterns == false`.
2. Ran full test suite: cache_yarn_worker test FAILED.
3. Ran `git bisect` but couldn't identify which file caused regression (bisect operates on commits).
4. Used manual per-file `git stash` approach:
   - Stashed each changed file individually
   - Ran failing test after each stash
   - Found that stashing decide.rs made test pass → regression in decide logic.
5. Traced `detected_patterns` usage:
   - Write side (run.rs): `detected_patterns = outcome.detected_outputs.is_some()` — only outputs.
   - Read side (decide.rs): `effective_input_patterns` used `prior.detected_patterns` to choose patterns.
   - Asymmetry: input decisions keyed off output-derived flag.
6. Realized single flag governs two independent dimensions.

## Root Cause

`TaskRunRecord.detected_patterns` was documented as "true when output patterns came from worker-detected outputs" but was used for *both* input and output decisions:

```rust
// decide.rs (read side)
fn effective_input_patterns(prior: &TaskRunRecord, current: &CurrentState) -> Vec<String> {
    if prior.detected_patterns {
        prior.input_patterns.clone()  // Uses OUTPUT-derived flag for INPUT decision
    } else {
        current.declared_input_patterns.to_vec()
    }
}
```

```rust
// run.rs (write side)
let detected_patterns = outcome.detected_outputs.is_some();  // Ignores detected_inputs
```

Consequence chain:
- yarn-worker returns `detected_inputs = ["package.json"]`, `detected_outputs = None`.
- Stored: `detected_patterns = false`, `input_patterns = ["package.json"]`.
- OLD buggy code: `effective_input_patterns` returned `prior.input_patterns` unconditionally → package.json checked → skip worked (accidentally correct).
- NEW fix: when `detected_patterns == false`, use declared inputs → package.json dropped → file-set mismatch → false rerun.

## Solution

Split the single flag into two independent flags:

```rust
// record.rs
pub struct TaskRunRecord {
    // ...
    /// true when input_patterns came from worker-detected inputs.
    pub detected_input_patterns: bool,
    /// true when output_patterns came from worker-detected outputs.
    pub detected_output_patterns: bool,
}
```

Update all call sites:

**Write side (run.rs):**
```rust
let detected_input_patterns = outcome.detected_inputs.is_some();
let detected_output_patterns = outcome.detected_outputs.is_some();
```

**Read side (decide.rs):**
```rust
fn effective_input_patterns(prior: &TaskRunRecord, current: &CurrentState) -> Vec<String> {
    if prior.detected_input_patterns {
        prior.input_patterns.clone()
    } else {
        current.declared_input_patterns.to_vec()
    }
}

fn effective_output_patterns(prior: &TaskRunRecord, current: &CurrentState) -> Vec<String> {
    if prior.detected_output_patterns {
        prior.output_patterns.clone()
    } else {
        current.declared_output_patterns.to_vec()
    }
}
```

## Why This Works

Input and output detection are independent events:
- A worker may detect inputs but not outputs (yarn-worker: `package.json`).
- A worker may detect outputs but not inputs (conceivable future worker).
- A worker may detect both or neither.

Each dimension needs its own flag. The fix makes:
- `detected_input_patterns` gate input pattern selection.
- `detected_output_patterns` gate output pattern selection.

Now both CodeRabbit #699 (declared-input change reruns) and yarn-worker tests pass.

## Prevention Strategies

### Test Cases

- Add tests where worker detects inputs only (not outputs).
- Add tests where worker detects outputs only (not inputs).
- Add tests where worker detects both.
- Add tests where worker detects neither.
- Verify declared-input changes cause reruns even when prior detected inputs exist.

### Best Practices

- **One flag per dimension**: When a flag governs N independent decisions, use N flags. A single flag creates implicit coupling that breaks when fixing one side.
- **Bisect-debugging technique**: When git bisect can't isolate files, use `git stash push -- <file>` to test each changed file individually.
- **Read/Write symmetry audit**: When a flag is written on one side and read on another, verify all read usages match the write semantics.

### Code Review Checklist

- [ ] Does any single flag control multiple independent decisions?
- [ ] Are read-side and write-side flag semantics symmetric?
- [ ] Does the fix for one dimension break the other? Run full test suite.
- [ ] For detection flags: are input and output independently detectable?

## Related Issues

- **Related Solution:** [uncached-task-detected-output-coupling-2026-06-12.md](./uncached-task-detected-output-coupling-2026-06-12.md) — Another detected-patterns related bug in uncached task output hashing
- **Plan Note:** `8c71ad32` — Root cause analysis and fix specification
- **CodeRabbit:** #699 — Original input-pattern bug report that exposed the conflation
