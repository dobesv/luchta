---
title: "Cross-package input expansion: untrusted worker patterns require hard-fail at both call sites"
date: 2026-06-16
category: security-issues
problem_type: security_issue
component: luchta-engine/input-expansion
root_cause: "Expansion errors from untrusted detected_inputs silently skipped cache write instead of failing task; validation needed at both decide and write paths"
resolution_type: code_fix
severity: critical
tags:
  - security
  - input-validation
  - path-escape
  - detected-inputs
  - cache-invalidation
  - cross-package
plan_ref: luchta-80-cross-package-inputs
---

## Problem

Untrusted worker-reported `detected_inputs` may carry cross-package input prefixes (`@pkg#path`, `#path`, `^glob`). Malicious patterns could escape the repository root (e.g., `@pkg#../../etc/passwd`, `#../x`). The initial implementation detected path escapes but silently skipped the cache write, allowing the task to succeed. Security-critical expansion errors must hard-fail at both decide-time (declared inputs) and write-time (worker-reported inputs).

## Symptoms

```
# Before fix: worker-reported path escape silently skipped cache, task succeeded
Task: my-task
Worker reported: detected_inputs: ["#../escape.txt"]
Result: task succeeded, cache write skipped (no error)

# Expected: task must HARD-FAIL
✖ input "#../escape.txt" in package "frontend": path escapes repository root
```

Security review (Aristarchus) flagged this as a blocker: path escape from untrusted input must not be silently tolerated.

## Investigation Steps

1. Traced expansion flow: `expand_input_patterns` in `luchta-engine` validates packages and paths, returning `InputExpansionError::PathEscape` or `UnknownPackage`.

2. Found two call sites with different timing:
   - **Decide path** (`cache_ctx.rs`): validates patterns before task runs
   - **Write path** (`dispatch.rs`): validates worker-reported `detected_inputs` after task runs

3. Discovered write-path bug: `resolve_cache_inputs` returned `None` on `InputExpansionError`, which caller treated as "skip cache write" — task still succeeded.

4. Reviewed distinguishability: ordinary IO resolve errors (file read failure, git-tracked lookup) should REMAIN non-fatal (warn + skip). Only expansion errors (path escape, unknown package) must hard-fail.

5. Verified lexical path validation: `lexical_normalize` collapses `.` and `..` without filesystem access. `canonicalize()` cannot be used because the path may not exist. Validation must check:
   - Referenced package exists in `PackageGraph`
   - Resolved base_dir is within repo_root
   - Final path is within base_dir (for non-upstream patterns)

## Root Cause

The security boundary between trusted declared inputs and untrusted worker-detected inputs was unclear:

1. **Write-path error destruction**: The cache-write pipeline had `build_run_record` → `write_run_record` → `persist_cache_state`. `InputExpansionError` was mapped to `None`, losing the error signal. The caller interpreted `None` as "skip cache write", not as "fatal expansion failure".

2. **Conflation with IO errors**: Both expansion errors (`PathEscape`, `UnknownPackage`) and ordinary IO errors (file not found, git error) flowed through the same result type, making it impossible to distinguish security-critical failures from tolerable resolve problems.

3. **Missing decide-time validation**: Declared inputs weren't eagerly validated before building the command map, so path escapes would only be caught post-execution.

## Solution

### 1. Add Dedicated Error Types

**luchta-engine (input_expansion.rs):**
```rust
pub enum InputExpansionError {
    UnknownPackage { package: PackageName, pattern: String },
    PathEscape { source_pkg: PackageName, pattern: String },
    InvalidPattern { pattern: String },  // parse failures
}
```

**luchta-cache (error.rs):**
```rust
pub enum CacheError {
    // ... existing variants
    InputExpansion(String),  // NEW: security-critical expansion failures
}
```

### 2. Declared Inputs: Eager Validation at Decide-Time

**luchta-cli/src/run/dispatch.rs (build_command_map):**
```rust
// Validate declared input patterns BEFORE building command map
let expanded = match expand_input_patterns(
    &task_def.inputs,
    &source_pkg,
    &graph,
    &repo_root,
) {
    Ok(requests) => requests,
    Err(e) => {
        // HARD-FAIL: insert into invalid map, task fails before running
        invalid.insert(task_id.clone(), e.to_string());
        continue;
    }
};
```

The dispatch loop checks the `invalid` map and fails the task immediately (`any_failed = true`, `✖` message, exit 1).

### 3. Detected Inputs: Thread Expansion Error Through Write Path

**luchta-cli/src/run/dispatch.rs (persist_cache_state):**
```rust
pub enum BuildRecordResult {
    Success(TaskRunRecord),
    IoError(String),           // non-fatal: warn + skip
    ExpansionError(String),    // FATAL: hard-fail task
}

pub enum WriteRecordResult {
    Success,
    IoError(String),
    ExpansionError(String),
}

pub fn persist_cache_state(inputs: CachePersistInputs) -> Option<String> {
    match write_run_record(&inputs) {
        WriteRecordResult::Success => None,
        WriteRecordResult::IoError(msg) => {
            eprintln!("warning: {}", msg);
            None  // non-fatal
        }
        WriteRecordResult::ExpansionError(msg) => Some(msg),  // FATAL signal
    }
}
```

**Caller (spawn_task_runner):**
```rust
let expansion_error = persist_cache_state(cache_inputs);
if let Some(msg) = expansion_error {
    any_failed.store(true, Ordering::SeqCst);
    eprintln!("{} {}", "✖".red(), msg.red());
    reporter.task_finished_other(&task_id, "failed");
    done_tx.send(false).ok();
    return;
}
```

### 4. Lexical Path Normalization (No Filesystem Access)

**luchta-engine/src/input_expansion.rs:**
```rust
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !normalized.pop() {
                    // Attempted to escape root: /a/../..
                    // Return as-is; caller should check if within base
                }
            }
            Component::CurDir => { /* skip */ }
            c => normalized.push(c),
        }
    }
    normalized
}

fn validate_path_within_base(base: &Path, path: &Path) -> Result<(), InputExpansionError> {
    let resolved = lexical_normalize(&base.join(path));
    if !resolved.starts_with(base) {
        return Err(InputExpansionError::PathEscape { ... });
    }
    Ok(())
}
```

## Why This Works

### Defense in Depth

| Call Site | Timing | Input Source | Failure Mode |
|-----------|--------|--------------|--------------|
| `build_command_map` | Pre-execution | Declared inputs | Task fails immediately, never runs |
| `persist_cache_state` | Post-execution | Worker-detected inputs | Task fails after run, exit 1 |

Both paths call `expand_input_patterns` with SAME context (`repo_root`, `source_pkg`, `package_graph`), ensuring parity.

### Error Distinguishability

- `InputExpansionError` → FATAL (security boundary violation)
- `IO error / git error / file not found` → NON-FATAL (warn + skip cache)

The `ExpansionError` variant in result enums makes this explicit.

### Why `lexical_normalize` Instead of `canonicalize`

- `canonicalize()` requires the path to exist on disk
- Worker-reported paths may not exist yet (e.g., `detected_inputs` for generated files)
- Lexical normalization handles `.` and `..` safely without filesystem access
- Validation checks that normalized path starts with allowed base directory

## Prevention Strategies

### Test Cases

- **Declared inputs with path escape**: `inputs: ["#../escape.txt"]` → task fails before run
- **Declared inputs with unknown package**: `inputs: ["nonexistent#file.txt"]` → task fails before run
- **Worker-detected inputs with path escape**: worker reports `["#../escape.txt"]` → task fails after run, exit 1
- **Ordinary IO errors remain non-fatal**: file read error during resolve → warning, cache write skipped, task succeeds

### Best Practices

- **HARD-FAIL on trust boundary violations**: User-provided pattern strings (especially from workers) can reference paths outside their scope. Validation errors must propagate as task failures, not silent skips.

- **Dedicated error types for security vs operational failures**: If an error represents a security boundary violation, it needs its own type variant. Mixing with IO errors invites mis-handling.

- **Validate at BOTH call sites**: Declared inputs validated before execution, detected inputs validated after. Same `expand_input_patterns` function, same validation logic, different timing.

- **Lexical normalization for untrusted paths**: `canonicalize()` is not available for non-existent paths. Lexical collapse of `.`/`..` is sufficient for path-escape detection.

### Code Review Checklist

- [ ] Does untrusted input pattern validation hard-fail (not silent skip)?
- [ ] Are both decide-time and write-time expansion paths covered?
- [ ] Is there a dedicated error type distinguishing security failures from IO errors?
- [ ] Does path validation use lexical normalization (no filesystem access needed)?
- [ ] Are expansion errors propagated through ALL intermediate layers without loss?
- [ ] Do ordinary IO resolve errors remain non-fatal (warn + skip)?

## Related Issues

- **Related Solution:** [logic-errors/detected-patterns-flag-conflation-2026-06-12.md](../logic-errors/detected-patterns-flag-conflation-2026-06-12.md) — Parity between decide/write paths for input pattern selection
- **Related Solution:** [logic-errors/hash-boundary-task-spec-vs-separate-2026-06-12.md](../logic-errors/hash-boundary-task-spec-vs-separate-2026-06-12.md) — Raw pattern strings in spec hash, resolved files in run record
- **GitHub Issue:** #80 — Cross-package input dependency support
- **Plan Note:** `b245a9db` — Security requirement for hard-fail on path escape
- **Plan Note:** `f66db9af` — Aristarchus REQUEST_CHANGES on security blocker
- **Plan Note:** `fcf6f875` — Implemented hard-fail mechanism at both call sites
