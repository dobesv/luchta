---
title: "Cache sharing: per-task tier gating without touching local skip/metadata"
date: 2026-07-22
category: logic-errors
problem_type: logic_error
component: luchta-cli/dispatch
root_cause: "Adding per-task cache-tier control requires precise gate placement in existing four-gate model to avoid affecting local skip/metadata gates"
resolution_type: code_fix
severity: medium
tags:
  - cache-tier-gating
  - dispatch-gating
  - shared-cache
  - four-gate-model
  - task_spec_hash
  - verification-traps
plan_ref: luchta-cache-sharing
---

## Problem

Users needed per-task control over shared/remote cache usage without affecting local caching — some tasks should use only local cache (for security, bandwidth, or CI isolation reasons). The existing four-gate cache model (local skip, shared read, shared write, local write) already separated these concerns, but lacked a configuration surface for selectively disabling shared cache tiers.

## Symptoms

- Shared cache uploads occurred even for tasks that should remain local-only (bandwidth cost, CI pollution)
- Cross-repo cache contamination when unrelated projects share a remote cache namespace
- No config-level control to disable shared cache without also disabling local cache

## Investigation Steps

1. Reviewed existing four-gate model from `no-cache-flag-four-gate-model-2026-07-13.md`:
   - Gate 1: `skip_enabled` in `try_cache_skip()` — local skip check
   - Gate 2: `maybe_mark_shared_cache_hit()` — shared-read check/restoration
   - Gate 3: `shared_store_enabled` in `run_task_and_persist_cache()` — shared-write upload
   - Gate 4: `build_cache_decision_context` forced `Decision::Run` — local metadata persistence

2. Identified exact gate points for shared-tier control:
   - **Gate 2 (shared-read)**: `maybe_mark_shared_cache_hit` at lines 1068-1098 in `dispatch.rs`
   - **Gate 3 (shared-write)**: `shared_store_enabled` guard at line 511 in `dispatch.rs`

3. Confirmed Gate 1 and Gate 4 have no dependency on shared cache. Local skip uses `.luchta/cache/<hash>/run-record.json`; local metadata persistence uses same path — both independent of `LUCHTA_SHARED_CACHE`.

4. Verified `TaskSpecHashInput` structure: nonce already included as cache-control field. Followed same pattern for `sharing`.

5. Added `CacheSharing` enum (`None`, `Local`, `Remote`) with `#[serde(rename_all = "kebab-case")]` and `#[default] Remote`.

## Root Cause

Lack of per-task configuration surface for gate 2/3 control. The four-gate model was already structurally correct — each gate is independently controlled. Adding a new control knob required:
1. Enum in `luchta-types` with serde defaults for backward compatibility
2. Gate 2 early-return guard before `try_shared_cache_skip` call
3. Gate 3 compound guard amendment: `shared_store_enabled = cache_enabled && !no_cache && sharing.allows_shared_write()`
4. Hash inclusion for policy changes (cache invalidation)

Critical: Gate 1 (`skip_enabled`) and Gate 4 (unconditional `CacheWriteContext`) must NOT reference `sharing` — local cache behavior must remain unchanged.

## Solution

### 1. CacheSharing Enum (luchta-types/src/lib.rs)

```rust
/// Controls which cache tiers task may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum CacheSharing {
    None,
    Local,
    #[default]
    Remote,
}

impl CacheSharing {
    pub fn allows_shared_read(self) -> bool {
        matches!(self, CacheSharing::Remote)
    }

    pub fn allows_shared_write(self) -> bool {
        matches!(self, CacheSharing::Remote)
    }
}
```

Serde behavior: `#[serde(default)]` on `CacheConfig.sharing` — omitted field deserializes to `Remote`.

### 2. Gate 2 Shared-Read Guard (dispatch.rs:1068-1098)

```rust
fn maybe_mark_shared_cache_hit(
    ctx: &DecisionContext,
    no_cache: bool,
    cache_ctx: &mut CacheWriteContext,
    input: SharedCacheSkipInput<'_>,
    dep_outputs: &BTreeMap<String, [u8; 32]>,
) {
    let sharing = input
        .task_def
        .cache
        .as_ref()
        .map(|cache| cache.sharing)
        .unwrap_or_default();
    if no_cache || !sharing.allows_shared_read() || !matches!(input.decision.action, Decision::Run)
    {
        return;
    }
    // ... try_shared_cache_skip call
}
```

Early return before `try_shared_cache_skip` prevents shared cache lookup entirely.

### 3. Gate 3 Shared-Write Guard (dispatch.rs:488-511)

```rust
let sharing = cache_write
    .as_ref()
    .map(|cache_ctx| {
        cache_ctx
            .task_def
            .cache
            .as_ref()
            .map(|cache| cache.sharing)
            .unwrap_or_default()
    })
    .unwrap_or_default();
// ...
shared_store_enabled: cache_enabled && !no_cache && sharing.allows_shared_write(),
```

Extraction happens BEFORE `cache_write` is moved into `CachePersistInputs`. Guard added to compound condition.

### 4. Hash Boundary: Sharing in TaskSpecHashInput (hashing.rs:100-108)

```rust
struct TaskSpecHashInput<'a> {
    // ... other fields
    // Cache sharing policy belongs in task_spec_hash for the same reason as nonce:
    // it is cache-control that affects which cache tiers (local vs remote) a task
    // may use, and changing the policy must invalidate task-spec identity.
    sharing: CacheSharing,
}
```

Policy change invalidates cache key — accepted one-time rebuild on upgrade.

## Why This Works

1. **Gate isolation**: Each gate is an independent boolean guard. Adding `sharing` conditions to gates 2 and 3 does not affect gates 1 and 4 because those paths never reference the field.

2. **Extraction pattern**: `task_def.cache.as_ref().map(|c| c.sharing).unwrap_or_default()` cascades correctly:
   - No `cache` field → `CacheSharing::Remote` (default behavior preserved)
   - `cache: {}` with no `sharing` → `CacheSharing::Remote` (serde default)
   - `cache: { sharing: "none" }` → blocks shared read/write

3. **Hash inclusion justification**: `sharing` is cache-control policy. Changing `Remote` → `None` means task should not use shared cache entries from prior remote-enabled runs. Including in `task_spec_hash` provides one-time invalidation on policy change.

4. **Backward compatibility**: `#[serde(default)]` + `#[default] Remote` ensure existing configs without `sharing` field behave identically to current behavior (shared cache enabled).

5. **Semantic equivalence of `None` and `Local`**: Both disable gates 2 and 3 identically. The separation exists for future observability/diagnostics (e.g., `"local"` might indicate intentional CI isolation vs `"none"` meaning "no caching whatsoever" if local-only mode is later extended).

## Prevention Strategies

### Test Cases

- **Gate 2 bypass**: E2E test wipes local cache then re-runs task with `sharing: none` — must re-execute (proves shared-read disabled)
- **Gate 3 bypass**: E2E test checks shared cache directory after run with `sharing: none` — must be empty
- **Gate 1/4 preservation**: E2E test runs task with `sharing: none` twice in same workspace — second run skips locally (proves Gate 1 still works), local metadata persists after run
- **Default regression**: E2E test with omitted `sharing` proves shared cache hit/write still works

### Verification Traps

1. **Per-crate build incomplete**: `cargo build -p <crate>` or `cargo build --workspace` (default profile) does NOT compile test code. A missing-field struct literal in `#[cfg(test)]` module (e.g., `list.rs:285`) will not surface until:
   - `cargo check --workspace --tests`
   - `cargo nextest run --workspace`

2. **JSON snapshot regressions**: Adding an additive serde field to a type that appears in snapshot assertions (e.g., `list_integration.rs` JSON output) requires updating expected JSON. grep all assertions referencing modified types:
   ```bash
   rg 'CacheConfig|TaskDefinition' crates/luchta-cli/tests/
   ```

3. **Full verification before merge**: When adding a field to a serialized/`Hash`/`PartialEq` type:
   ```bash
   cargo nextest run --workspace
   cargo fmt --check
   cargo clippy --workspace --all-targets
   ```
   And grep for ALL struct literals of the modified type across the workspace.

4. **Duplicated extraction pattern**: The `.cache.as_ref().map(|c| c.sharing).unwrap_or_default()` idiom appears in 3 places (hashing.rs, dispatch.rs×2). Consider future refactoring into `TaskDefinition::cache_sharing()` helper to reduce drift risk.

### Code Review Checklist

- [ ] `allows_shared_read()` / `allows_shared_write()` return `true` only for `Remote`
- [ ] Gate 1 (`skip_enabled`) unchanged — no `sharing` reference
- [ ] Gate 4 (`build_cache_decision_context`) unchanged — no `sharing` reference
- [ ] Gate 2 guard: early return BEFORE `try_shared_cache_skip`
- [ ] Gate 3 guard: `sharing.allows_shared_write()` added to compound condition
- [ ] `sharing` in `TaskSpecHashInput` with explanatory comment
- [ ] All `CacheConfig` struct literals updated (grep workspace)
- [ ] JSON snapshot tests updated for additive field
- [ ] `#[serde(default)]` on `CacheConfig.sharing`
- [ ] `#[default] Remote` on enum variant
- [ ] `cargo nextest run --workspace` passes
- [ ] `cargo check --workspace --tests` passes

## Design Decision: Hash Inclusion

**Decision**: Include `sharing` in `task_spec_hash`.

**Rationale**: 
- `sharing` is cache-control policy, not transient runtime config
- Policy change from `Remote` → `None` means task should NOT reuse cache entries from prior remote-enabled executions
- One-time cache invalidation on upgrade is acceptable (documented in plan)

**Alternative considered**: Exclude `sharing` from hash, only gate at runtime.
- **Downside**: Changing `Remote` → `None` would silently use existing shared cache entries, defeating user intent of disabling remote cache.

## Related Issues

- **GitHub**: [#103](https://github.com/dobesv/luchta/issues/103) — Ability to disable shared/remote cache for some tasks
- **Related Solution**: [no-cache-flag-four-gate-model-2026-07-13.md](./no-cache-flag-four-gate-model-2026-07-13.md) — Four-gate model fundamentals
- **Related Solution**: [cache-nonce-invalidation-control-2026-06-23.md](./cache-nonce-invalidation-control-2026-06-23.md) — Nonce pattern for manual invalidation
- **Related Solution**: [hash-boundary-task-spec-vs-separate-2026-06-12.md](./hash-boundary-task-spec-vs-separate-2026-06-12.md) — Task spec hash boundary decisions
