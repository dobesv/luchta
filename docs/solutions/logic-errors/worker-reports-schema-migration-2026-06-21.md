---
title: "Worker reports feature with bincode schema migration and shared-cache parity"
date: 2026-06-21
category: logic-errors
problem_type: logic_error
component: luchta-cache/luchta-engine
root_cause: "bincode positional schema fragility; incomplete shared-cache artifact threading"
resolution_type: code_fix
severity: high
tags:
  - bincode
  - schema-migration
  - shared-cache
  - worker-protocol
  - reports
  - jsonl
  - owocolors
plan_ref: luchta-27-normalized-error-reporting
---
## Problem

Adding a `reports` field to `TaskRunRecord` broke backward compatibility with existing cache files due to bincode's positional (non-self-describing) encoding. Additionally, new report artifacts were only persisted to the local cache, silently vanishing on shared-cache hits because the shared blob format was not updated to include them.

## Symptoms

**Bincode schema mismatch:**
- `UnexpectedEof` when reading old `meta.bincode` files after adding `reports: Vec<ReportMeta>`
- `#[serde(default)]` on the new field did NOT help — bincode has no schema metadata
- Legacy cache directories caused cache-layer errors or forced manual `.luchta/cache` deletion

**Shared-cache artifact loss:**
- Reports written to local cache correctly on first run
- After shared-cache restore on a fresh machine/branch: reports missing from local cache
- `luchta logs --file <report>` could not find files that existed on the original machine

**CLI ANSI pollution:**
- `luchta logs` output included ANSI escape codes when piped or with `NO_COLOR` set
- IDE-clickable `path:line:col` links broken by color codes
- Machine consumers (`reviewdog`) failed to parse colored output

## Investigation Steps

1. **Bincode migration:** Added `reports: Vec<ReportMeta>` to `TaskRunRecord`. Ran tests against existing cache dirs: `UnexpectedEof`. Confirmed `#[serde(default)]` ineffective for bincode (positional, no field names). Reviewed `TaskRunRecord` struct — `schema_version` already first field.

2. **Schema design:** Adopted defensive read pattern: `bincode::serde::decode_from_slice`. On ANY error OR `schema_version != SCHEMA_VERSION_V2`, return `None`. Caller treats as cache miss and regenerates. Cache is disposable.

3. **Shared-cache audit:** Traced `SharedCache::store` and `restore_blob_with_meta`. Found blob format packs `.luchta-meta/{stdout.log, stderr.log, meta.bincode}` only. Reports not included.

4. **Shared-cache fix:** Extended `write_blob_with_meta` to pack report content and metadata into `.luchta-meta/reports/<filename>` entries. Extended `restore_blob_with_meta` to hydrate these into the local cache `task_dir`.

5. **owo-colors audit:** Found `format.rs` using unconditional `.red()/.green()/.cyan()`. These emit ANSI regardless of TTY/NO_COLOR. Replaced with `.if_supports_color(Stream::Stdout, |t| t.color())`.

## Root Cause

**Bincode positional encoding:** Bincode encodes structs as contiguous field values with no self-describing metadata. Adding/removing/reordering fields breaks existing files. `schema_version` as first field provides an opt-in version check, but readers must handle old versions gracefully (treat as cache miss).

**Incomplete shared-cache threading:** Shared-cache blob format was defined once (`stdout.log`, `stderr.log`, `meta.bincode`) and not revisited when adding reports. Local-cache write path added reports, but shared-store/restore paths were omitted.

## Solution

### 1. Bincode schema migration with version check

```rust
// record.rs
pub const SCHEMA_VERSION_V1: u32 = 1;
pub const SCHEMA_VERSION_V2: u32 = 2;

#[derive(Serialize, Deserialize)]
pub struct TaskRunRecord {
    pub schema_version: u32,  // FIRST field
    // ... other fields ...
    pub reports: Vec<ReportMeta>,  // NEW field
}

// store.rs
pub fn read(&self, task_id: &str) -> Option<TaskRunRecord> {
    let bytes = fs::read(self.task_dir(task_id).join("meta.bincode")).ok()?;
    let (record, _): (TaskRunRecord, usize) =
        bincode::serde::decode_from_slice(&bytes, bincode_config()).ok()?;
    // Version mismatch -> None (cache miss, regenerate)
    (record.schema_version == SCHEMA_VERSION_V2).then_some(record)
}

pub fn write(&self, task_id: &str, artifacts: &RunArtifacts) -> Result<()> {
    let record = artifacts.record;
    record.schema_version = SCHEMA_VERSION_V2;  // Always write current version
    let encoded = bincode::serde::encode_to_vec(record, bincode_config())?;
    // ... write atomically ...
}
```

**Key insight:** ANY decode error OR version mismatch returns `None`. Cache is disposable; regenerate on mismatch.

### 2. Shared-cache artifact parity

```rust
// shared/blob.rs
pub fn write_blob_with_meta(
    paths: &SharedCachePaths,
    outputs_hash: &[u8; 32],
    package_dir: &Path,
    rel_output_paths: &[PathBuf],
    size_cap_bytes: u64,
    meta: &MetaFiles,
    reports: &[ReportInput],  // NEW parameter
) -> io::Result<BlobWriteResult> {
    // Build tar with:
    //   - Output files from package_dir
    //   - .luchta-meta/stdout.log, stderr.log, meta.bincode
    //   - .luchta-meta/reports/<filename>  (NEW)
}

pub fn restore_blob_with_meta(
    paths: &SharedCachePaths,
    outputs_hash: &[u8; 32],
    package_dir: &Path,
) -> io::Result<BlobReadResultWithMeta> {
    // Extract all entries to staging dir
    // Hydrate reports into Cache::write() call  (NEW)
}

pub struct MetaFiles {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub meta: TaskRunRecord,
    pub reports: Vec<ReportInput>,  // NEW field
}
```

**Key constraint:** Every artifact added to local cache must flow through both store AND restore paths. Missing either breaks cross-machine correctness.

### 3. TTY-aware color output

```rust
// format.rs
use owo_colors::{OwoColorize, Stream};

// BEFORE: unconditional ANSI
format!("{} {} start={}\n", HEADER_MARKER.blue(), task_label.bold(), ...)

// AFTER: respects TTY and NO_COLOR
format!(
    "{} {} start={}\n",
    HEADER_MARKER.if_supports_color(Stream::Stdout, |t| t.blue()),
    task_label.if_supports_color(Stream::Stdout, |t| t.bold()),
    ...
)
```

Requires `owo-colors` feature `supports-colors` enabled.

### 4. Machine-safe raw passthrough

```rust
// logs.rs
pub(crate) fn tasks_with_requested_files<'a>(
    records: impl Iterator<Item = (&'a TaskId, &'a TaskRunRecord)>,
    requested_files: &[String],
) -> Result<Vec<(TaskId, Vec<String>)>> {
    // Build union of tasks that have ANY requested file
    // But: if same filename appears on multiple tasks -> ERROR
    // Do NOT concatenate; that would produce invalid combined output
}
```

**Key rule:** `--file` passthrough must emit exactly one file's bytes to stdout. Ambiguous match errors instead of silent concatenation.

### 5. Defense-in-depth filename safety

```rust
// store.rs
pub(crate) fn is_valid_report_filename(filename: &str) -> bool {
    !filename.is_empty()
        && !Path::new(filename).is_absolute()
        && !filename.contains('/')
        && !filename.contains('\\')
        && !filename.contains("..")
        && filename != META_FILE_NAME
        && filename != STDOUT_FILE_NAME
        && filename != STDERR_FILE_NAME
}

// worker_protocol.rs (engine)
fn is_valid_report_filename(filename: &str) -> bool { same logic }

// On invalid filename: warn and DROP; never crash the worker
```

Applied at every boundary: worker protocol ingest, cache write, cache read, shared-cache restore.

## Why This Works

**Bincode version check:** By checking `schema_version` after decode, readers reject old files without crashing. Cache miss triggers re-execution, producing fresh v2 records. One-time cost per migration.

**Shared-cache parity:** Reports flow through both store (local → shared blob) and restore (shared blob → local cache). Cross-machine hits hydrate the same artifacts that were stored locally.

**TTY-aware coloring:** `if_supports_color(Stream::Stdout, ...)` queries terminal support and respects `NO_COLOR`. Unconditional color blindly emits ANSI bytes.

**Defense-in-depth:** Each layer validates independently. Compromised input at one layer cannot bypass checks at another.

## Prevention Strategies

**Test Cases:**
- Bincode migration: write v1 record, upgrade binary, verify read returns None, verify re-run produces v2
- Shared-cache round-trip: store with reports, restore on fresh machine, verify reports present
- Color output: run `luchta logs` with `NO_COLOR=1`, grep for ANSI escape codes (should be none)
- Ambiguous file: run `--file sarif.json` when two tasks have same filename, verify error (not concatenation)
- Path traversal: send `report` with `filename="../escape.txt"`, verify warning logged, task completes, no file escaped

**Code Review Checklist:**
- [ ] Does new `TaskRunRecord` field bump `SCHEMA_VERSION`?
- [ ] Does read path handle decode errors / version mismatch as None?
- [ ] Does new local-cache artifact have shared-cache store AND restore?
- [ ] Do color methods use `if_supports_color` instead of unconditional?
- [ ] Does `--file` passthrough error on ambiguous multi-task match?

**Related:**
- [logic-errors/uncached-task-detected-output-coupling-2026-06-12.md](./uncached-task-detected-output-coupling-2026-06-12.md) — prior art on JSONL stdout pollution
- [logic-errors/cache-persistence-decoupling-worker-protocol-2026-06-19.md](./cache-persistence-decoupling-worker-protocol-2026-06-19.md) — worker protocol fixture best practices
