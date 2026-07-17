---
title: "Deduplicating bincode configs exposed silent-corruption trap: two serialization domains must stay distinct"
date: 2026-07-16
category: logic-errors
problem_type: logic_error
component: luchta-cache
root_cause: "two bincode configs with different int-encoding coexist; conflation causes silent decode failures"
resolution_type: code_fix
severity: high
tags:
  - bincode
  - serialization
  - fixed-int-encoding
  - varint
  - content-addressing
  - testing
  - false-green
plan_ref: issue-54
---
## Problem

`luchta-cache` has **two distinct bincode configurations** that must NOT be conflated. When deduplicating "duplicate" serialization helpers, a test fixture was mistakenly updated to use the wrong config, causing silent data corruption that passed all tests.

## Symptoms

**Snapshot test fixtures produced empty entries after decode:**

- Test serialized `Snapshot` fixture with `bincode_config()` (fixed-int encoding from `serialization.rs`)
- `Snapshot::load` decodes with `snapshot_bincode_config()` (varint/variable-length encoding)
- `bincode::serde::decode_from_slice` ignores trailing bytes
- Fixed-int header bytes happen to parse as a valid (but wrong/empty) snapshot under varint
- Decoding **succeeds silently** → empty `entries` map
- Tests asserting only control-flow (`index.is_some()`, load counts) passed **false-green**

No error message. No panic. Just silently wrong data.

## Investigation Steps

1. Noticed two `bincode_config()` helpers in codebase during deduplication refactor:
   - `crate::serialization::bincode_config()` → `standard().with_fixed_int_encoding()`
   - `shared/snapshot.rs::snapshot_bincode_config()` → `standard()` (varint)

2. Traced call sites:
   - `bincode_config()` (fixed-int): `hashing.rs`, `store.rs`, `shared/mod.rs` — used for `TaskRunRecord`, BLAKE3 hashes must be stable
   - `snapshot_bincode_config()` (varint): `snapshot.rs` — used for on-disk snapshot shards, must preserve existing format

3. Found test fixture in `shared/mod.rs` used `bincode_config()` (fixed-int) to serialize snapshot, while `decode_snapshot` uses varint.

4. Reproduced the silent-corruption: encode with fixed-int, decode with varint → `decode_from_slice` returns empty struct, no error.

5. Read bincode docs: `decode_from_slice` returns `(decoded, consumed_bytes)`. Trailing bytes are ignored. Positional encoding means the varint decoder reads the fixed-int length prefix as the first field value and stops early.

6. Confirmed tests only checked `Option::is_some()` not content → passed despite wrong answer.

## Root Cause

Bincode configs differ in **integer encoding mode**:

| Config | Encoding | Use Case | Why It Matters |
|--------|----------|----------|----------------|
| `bincode_config()` | Fixed-int | `TaskRunRecord` serialization, cache hashing, shared-cache prod paths | BLAKE3 hashes must be byte-stable across runs. Fixed-int encoding ensures deterministic bytes for same logical value. |
| `snapshot_bincode_config()` | Varint | On-disk snapshot shards | Preserves existing on-disk format. Changing encoding would break all existing snapshots. |

These look similar (`bincode::config::standard()` plus one `.with_*` call) but encode integers differently. Conflating them:

- Breaks content-addressing (hashes become unstable)
- Causes silent decode corruption (varint reads fixed-int bytes wrong, ignores trailing bytes)

The test suite passed because it asserted control-flow, not decoded content.

## Solution

### 1. Keep both configs with explicit doc comments

```rust
// serialization.rs
/// Canonical bincode configuration for cache hashing and record storage.
/// Uses fixed-int encoding so BLAKE3 hashes are byte-stable.
pub(crate) fn bincode_config() -> impl bincode::config::Config {
    bincode::config::standard().with_fixed_int_encoding()
}

// shared/snapshot.rs
/// Bincode configuration for on-disk snapshot shards.
/// Uses variable-length (varint) encoding to preserve existing snapshot format.
/// MUST NOT be conflated with `crate::serialization::bincode_config()`.
pub(crate) fn snapshot_bincode_config() -> impl bincode::config::Config {
    bincode::config::standard()
}
```

### 2. Fix test fixture call sites

Verified `write_snapshot_fixture` in `shared/mod.rs` now uses `snapshot_bincode_config()` to match `decode_snapshot`.

### 3. Tests must assert decoded content

```rust
// BEFORE (false-green)
let snapshot = Snapshot::load(&dir);
assert!(snapshot.is_some()); // passes even if entries empty

// AFTER (correct)
let snapshot = Snapshot::load(&dir).expect("snapshot should load");
assert!(!snapshot.entries.is_empty(), "entries should not be empty");
assert_eq!(snapshot.entries.get("expected_key").unwrap().task_id, "pkg#build");
```

## Why This Works

1. **Named domain-specific configs prevent import mistakes** — `bincode_config` vs `snapshot_bincode_config` makes intent explicit at each call site.

2. **Doc comments warn future maintainers** — explains WHY they differ, not just WHAT they do.

3. **Content assertions catch silent corruption** — `decode_from_slice` ignores trailing bytes; only asserting decoded values catches mismatched configs.

4. **Fixed-int ensures stable hashes** — content-addressing requires deterministic bytes; varint would change ID for same logical content.

## Prevention Strategies

**Test cases:**
- Snapshot round-trip: encode with `snapshot_bincode_config` → decode → assert entries match exactly
- Cross-config detection: encode with fixed-int, decode with varint, assert failure OR assert decoded content is wrong (not just `is_some()`)
- Hash stability: encode same record twice with `bincode_config`, assert BLAKE3 hashes equal

**Code review checklist:**
- [ ] Serialization config matches the data type being encoded/decoded?
- [ ] Test asserts decoded CONTENT, not just that decode returned `Some`?
- [ ] When deduplicating configs, all call sites truly share the SAME encoding requirements?
- [ ] Content-addressed data uses fixed-int for stable hashes?

**Best practices:**
- Give each serialization domain a uniquely-named config helper with doc comment explaining purpose
- When sharing a helper across domains, verify each call site's encoding requirements match
- Bincode `decode_from_slice` ignores trailing bytes — always assert decoded content, not just success
- Identical-looking `standard()`-based builders can differ in int-encoding/endianness — check the chain

## Related Issues

- **GitHub Issue:** [#54](https://github.com/dobesv/luchta/issues/54) — Deduplicate bincode_config in luchta-cache
- **Related Solution:** [compress-content-addressed-chokepoint-2026-06-29.md](./compress-content-addressed-chokepoint-2026-06-29.md) — Hash-before-compress requires fixed-int for stable IDs
- **Related Solution:** [yarn-berry-lockfile-parser-2026-06-09.md](../integration-issues/yarn-berry-lockfile-parser-2026-06-09.md) — Tolerant deserialization patterns
