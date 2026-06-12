---
title: "Hash boundary: task spec hash excludes env/inputs/outputs patterns"
date: 2026-06-12
category: logic-errors
problem_type: logic_error
component: luchta-cache/hashing
root_cause: "task_spec_hash hashed entire TaskDefinition, defeating env opt-out and missing invalidation paths"
resolution_type: code_fix
severity: high
tags:
  - cache-invalidation
  - hashing
  - spec-hash
  - env-opt-out
  - declared-patterns
plan_ref: luchta-build-cache
---

## Problem

`task_spec_hash` was hashing the entire `TaskDefinition` struct, including `env`, `inputs`, and `outputs` fields. This defeated the documented `env: { input: false }` opt-out (env changes would invalidate via the spec hash anyway) and created missing invalidation paths for declared pattern changes.

## Symptoms

```
- Feature: env: { API_TOKEN: { value: "secret", input: false } } should NOT invalidate cache on change
- Actual: Changing API_TOKEN value caused rerun (env in spec_hash defeated opt-out)
- CodeRabbit #711: task_spec_hash includes env, defeating input:false
```

Additionally, declared input/output pattern changes were not causing reruns because they lacked a dedicated invalidation path.

## Investigation Steps

1. Traced `task_spec_hash` implementation: `bincode::encode_to_vec(task_def, ...)` — hashes whole struct.
2. Traced `env_hash`: correctly excludes entries where `input: false`.
3. Identified conflict: `task_spec_hash` re-includes env, negating `env_hash` opt-out.
4. Analyzed what SHOULD be in spec hash:
   - Things with no other change-detection mechanism.
   - Declared config that defines execution identity.
5. Analyzed what should NOT be in spec hash:
   - Things tracked by separate hashes with opt-out semantics (env).
   - Things tracked by separate hash mechanisms (inputs, outputs have resolved file hashes).
6. Realized declared pattern changes (glob syntax) need to invalidate even if resolved file set unchanged.

## Root Cause

The original plan defined `task_spec_hash` as "hash of execution spec" but implementation used whole-struct serialization:

```rust
// hashing.rs (before)
pub fn task_spec_hash(task_def: &TaskDefinition) -> [u8; 32] {
    let bytes = bincode::encode_to_vec(task_def, bincode_config())?;
    blake3::hash(&bytes)
}
```

This included:
- `task_def.env` — defeats `env_hash` opt-out.
- `task_def.inputs` — but resolved inputs have separate hash, so pattern-only changes were ignored.
- `task_def.outputs` — same issue.

The boundary was wrong: spec hash should capture declared config that OTHER hashes don't track.

## Solution

Curate an explicit struct containing only spec-fields:

```rust
// hashing.rs
#[derive(Serialize)]
struct TaskSpecHashInput<'a> {
    command: Option<&'a str>,
    worker: Option<&'a str>,
    weight: u8,
    depends_on: &'a [DependsOn],
    cache_enabled: bool,
    inputs: &'a [String],   // declared patterns (glob changes must invalidate)
    outputs: &'a [String],  // declared patterns
}

pub fn task_spec_hash(task_def: &TaskDefinition) -> [u8; 32] {
    let spec = TaskSpecHashInput {
        command: task_def.command.as_deref(),
        worker: task_def.worker.as_deref(),
        weight: task_def.weight,
        depends_on: &task_def.depends_on,
        cache_enabled: task_def.cache_enabled(),
        inputs: &task_def.inputs,
        outputs: &task_def.outputs,
    };
    let bytes = bincode::encode_to_vec(spec, bincode_config())?;
    *blake3::hash(&bytes).as_bytes()
}
```

Key decisions:
- **Exclude `env`**: `env_hash` handles env with opt-out. Including here would double-hash and defeat opt-out.
- **Include `inputs`/`outputs`**: Declared pattern changes (e.g., `"src/**"` → `"src/**/*.ts"`) must invalidate even if currently resolved file set is same. Glob syntax is part of spec.
- **Include `cache_enabled`**: Cache presence affects execution semantics (cached vs uncached).

## Why This Works

**Separation of concerns:**

| Field | Hash | Why |
|-------|------|-----|
| `command`, `worker`, `weight`, `depends_on` | `task_spec_hash` | Execution identity, no other tracking |
| `cache_enabled` | `task_spec_hash` | Affects execution path (skip/run) |
| `inputs` (patterns) | `task_spec_hash` | Glob syntax changes must invalidate |
| `outputs` (patterns) | `task_spec_hash` | Glob syntax changes must invalidate |
| `env` | `env_hash` | Has opt-out semantics via `input: false` |
| Resolved inputs | `inputs` field in record | File content hashes |
| Resolved outputs | `outputs_hash` | Combined output hash |

**Why inputs/outputs patterns belong in spec hash:**

A pattern `"*.ts"` and a pattern `"*.js"` may resolve to the same file set (if no `.js` files exist). But the semantic intent changed — future `.js` files will be included. This declared intent change must cause invalidation.

**Why env belongs in env_hash only:**

`env: { API_TOKEN: { value: "...", input: false } }` explicitly opts out of invalidation. If `task_spec_hash` includes env, changing the token value changes spec_hash → invalidation → opt-out defeated.

## Prevention Strategies

### Test Cases

- Test: `env: { VAR: { input: false } }` changes don't invalidate cache.
- Test: `env: { VAR: { input: true } }` changes DO invalidate (via env_hash).
- Test: Changing `inputs` pattern (e.g., `"src/**/*.ts"` → `"src/**/*.tsx"`) invalidates.
- Test: Changing `outputs` pattern invalidates.
- Test: Changing `command` invalidates.
- Test: Changing `weight` invalidates.

### Best Practices

- **Explicit hash boundaries**: When multiple hashes compose into a cache key, document which fields belong to which hash and WHY.
- **Audit opt-out paths**: If a field has opt-out semantics (like `env.input: false`), verify it's NOT included in other hashes.
- **Declared vs resolved distinction**: Declared patterns belong in spec hash; resolved file sets belong in separate tracking.

### Code Review Checklist

- [ ] Does each field have exactly one invalidation path?
- [ ] Are opt-out semantics honored (no double-hashing)?
- [ ] Do declared pattern changes cause invalidation?
- [ ] Is the hash boundary documented?

## Related Issues

- **CodeRabbit:** #711 — Original report of env in spec_hash defeating opt-out
- **Plan Note:** `4e4ef727` — Triage identifying the bug
- **Plan Note:** `be7b6184` — Follow-up fix adding inputs/outputs to spec hash
