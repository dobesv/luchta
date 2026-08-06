# Shared Cache: Entry Meta Split + Recency Discovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix two shared-cache addressing bugs — per-task meta stored inside an outputs-addressed blob (#278), and cache discovery that requires git ancestry (#277).

**Architecture:** The shared cache currently names two things by something that isn't their identity. Blobs are named by `outputs_hash` but also carry per-task meta (record, stdout, stderr, reports), so every task with no outputs collides on one blob. Snapshot index shards are named by git commit id and discovered by walking first-parent ancestry, so builds on ephemeral merge commits can never find each other's entries. Phase 1 moves meta into its own object addressed by `input_key`. Phase 2 renames shards to opaque session ids and discovers them by listing + recency instead of git history.

**Tech Stack:** Rust, `blake3`, `bincode` (v2 serde API), `zstd`, `tar`, `gix`, rclone rcd daemon over unix socket, `cargo nextest`.

## Global Constraints

- Run tests with `cargo nextest run --workspace`. Plain `cargo test` produces flaky e2e failures in this repo.
- Crate under change is `luchta-cache`; the CLI wiring lives in `luchta-cli`.
- All new on-disk formats use zstd-compressed bincode via `crate::serialization::bincode_config()` (fixed int encoding) for records, and `snapshot_bincode_config()` (standard) for snapshots. Do not mix them.
- All file writes into the shared cache go through `super::atomicio::atomic_write` or `streaming_atomic_write`. Never write in place.
- Remote failures must never fail a build. Every remote path logs to stderr with a `warn:` or `debug:` prefix and degrades to a cache miss. This is existing behavior — preserve it.
- `#[cfg(unix)]` gates all remote-sync code. Follow the existing gating exactly when touching `shared/mod.rs` and `shared/remote.rs`.
- Commit after every task with an imperative subject line. No AI attribution footers.

## Amendments during execution

Recorded here because the plan is the briefing source for each task.

- **Task 4 (during execution):** `EntryMeta` gains `pub has_outputs: bool`, set in `store` from
  `!matches!(blob_result, BlobWriteResult::NoOutputs)`. Task 4's `stage_entry` branches on
  `!meta.has_outputs`, NOT on `record.outputs.is_empty()` as originally written. Reason: the write side
  decides "no blob" from `rel_output_paths` (absent entries filtered out, then missing-on-disk files
  skipped), so a task with declared-but-absent outputs stored no blob yet had a non-empty
  `record.outputs` — restore then chased a blob that could never exist, missing permanently and firing a
  futile remote pull each attempt. `ENTRY_META_SCHEMA_VERSION` stays at 1; nothing has shipped.
- **Task 4 (during execution):** the multi-candidate blob-miss fallback in `try_restore_candidates` is
  deleted and `stage_entry` narrowed to take `input_key: &[u8; 32]`. It became inert once meta moved to
  `entries/<input_key>`: every candidate for one input_key resolves identically. Accepted consequence —
  a GC'd blob is now a rebuild rather than a fallback to another commit's outputs.

- **Task 7 (during execution):** removing git-commit shard naming orphaned two e2e tests that assert on
  `snapshots/<commit>` and `snapshots/<commit>-dirty` directory layout. Human ruling: rewrite both in Task 8
  to assert the surviving property rather than the removed mechanism. `dirty_key_isolation` becomes
  `dirty_tree_entry_is_not_reused_by_clean_build`; `accumulation_single_snapshot_multiple_entries` becomes
  `entries_from_separate_runs_are_both_discoverable`. Also deleted in Task 7: `cross_commit_key_hierarchy`,
  which called the now-deleted `git::candidate_commit_keys` directly and could not compile.

- **Task 7 review (during execution):** the zero-padding property of `new_session_shard_key` shipped
  untested — every Task 7 test used an already-13-digit timestamp, so `{now_unix_ms:013}` never actually
  padded. The padding is the ordering fallback for remotes that don't report ModTime, so it is load-bearing.
  A short-timestamp case was added to Task 8's test block. Additive, so no human ruling needed.

- **Task 8 review (during execution):** `MergedIndex` is reduced to `HashSet<String>` of input_key hexes.
  Human ruling. Task 4 made `stage_entry` resolve everything from `entries/<input_key>`, so the index
  payload was never read on the restore path — only `contains_key`. The `cached_at_unix_ms` conflict
  comparator added in Task 8 therefore arbitrated a value nothing consumed, using a cross-machine wall
  clock. Deleting the payload removes the trap before Task 9 can inherit it. `newest_wins_on_conflict` is
  deleted with it: it asserted on structure no production code reads.
- **Task 8 review (during execution):** discovery's read window will be bounded by entry count and age
  rather than shard count — see Task 10. Twenty per-invocation local shards would otherwise evict every CI
  shard from a 20-slot window.

- **Task 9 (rewritten during execution):** Step 5 originally described a `merged_candidate_keys` helper,
  a `pull_candidate_commits_for` rename, and `merged.snapshots.reverse()`. All three were obsolete by the
  time Task 9 came up: Task 4 deleted `MergedIndex.snapshots`, Task 8 already added the `history_len`
  field, made `candidate_keys()` a discovering method, and gave `pull_candidate_commits` a keys parameter.
  Step 5 was rewritten against the real code and a remote-only-shard test added as Step 6.

## Scope Note

This plan covers two subsystems. **Phase 1 (Tasks 1–6) is independently shippable** and fixes #278 on its own. **Phase 2 (Tasks 7–11)** fixes #277 and does not depend on Phase 1 landing first, but the ordering here is deliberate: Phase 1 makes restore two-phase (cheap meta fetch, then blob), which is what makes Phase 2's wider candidate set affordable. If you need to stop early, stop at the end of Task 6.

---

## File Structure

**New files:**

- `crates/luchta-cache/src/shared/entry_meta.rs` — the `EntryMeta` object: definition, bincode+zstd serialization, read/write against `entries/<input_key>.bin`. One responsibility: per-entry meta persistence. No knowledge of blobs, snapshots, or remotes.
- `crates/luchta-cache/src/shared/discovery.rs` — shard discovery: generate a session shard id, list local shard dirs by mtime, merge with a remote listing, apply the recency window. One responsibility: deciding *which* shards to read. No knowledge of snapshot contents.
- `crates/luchta-cli/tests/shared_cache_no_output_e2e.rs` — e2e regression coverage for #278.
- `crates/luchta-cli/tests/shared_cache_discovery_e2e.rs` — e2e regression coverage for #277.

**Modified files:**

- `crates/luchta-cache/src/shared/paths.rs` — add `entries_dir` to `SharedCachePaths` and create it in `open_shared_paths`.
- `crates/luchta-cache/src/shared/blob.rs` — add `restore_outputs_staged`; stop writing meta into new blobs. Keep the existing meta-reading code so blobs written by older clients still restore their outputs.
- `crates/luchta-cache/src/shared/mod.rs` — `store()` writes meta + outputs separately; `stage_entry()` becomes two-phase; construction switches from commit keys to discovered shard keys.
- `crates/luchta-cache/src/shared/remote.rs` — push/pull for `entries/`; snapshot dir listing with ModTime.
- `crates/luchta-cache/src/shared/rclone/mod.rs` — add `ModTime` to `Entry`.
- `crates/luchta-cache/src/shared/gc.rs` — age out `entries/`.
- `crates/luchta-cache/src/shared/git.rs` — `resolve_commit_key`/`candidate_commit_keys` are deleted; a session id generator replaces them.
- `crates/luchta-cache/src/shared/snapshot.rs` — rename the `commit_key` parameter to `shard_key` throughout. Behavior unchanged.

**Deleted:** nothing is deleted outright. `git.rs` shrinks to nothing useful and is removed in Task 7.

---

## Phase 1 — Split entry meta out of the outputs blob (#278)

### Task 1: `EntryMeta` type and storage

**Files:**
- Create: `crates/luchta-cache/src/shared/entry_meta.rs`
- Modify: `crates/luchta-cache/src/shared/paths.rs:21-38` (add `ENTRIES_DIR_NAME` and `entries_dir`), `crates/luchta-cache/src/shared/paths.rs:105-119` (`open_shared_paths`)
- Modify: `crates/luchta-cache/src/shared/mod.rs` (add `mod entry_meta;` and re-exports next to the existing `mod blob;` declarations)
- Test: inline `#[cfg(test)] mod tests` in `entry_meta.rs`; extend `paths.rs` tests

**Interfaces:**
- Consumes: `SharedCachePaths` from `shared/paths.rs`, `atomic_write` from `shared/atomicio.rs`, `crate::store::ReportInput`.
- Produces:
  - `pub const ENTRY_META_SCHEMA_VERSION: u32 = 1;`
  - `pub struct EntryReport { pub filename: String, pub mime_type: String, pub content: String }`
  - `pub struct EntryMeta { pub schema_version: u32, pub outputs_hash: [u8; 32], pub record: Vec<u8>, pub stdout: Vec<u8>, pub stderr: Vec<u8>, pub reports: Vec<EntryReport> }`
  - `pub enum EntryMetaWriteResult { Written, AlreadyExists }`
  - `pub fn entry_meta_path(paths: &SharedCachePaths, input_key: &[u8; 32]) -> PathBuf`
  - `pub fn write_entry_meta(paths: &SharedCachePaths, input_key: &[u8; 32], meta: &EntryMeta) -> io::Result<EntryMetaWriteResult>`
  - `pub fn read_entry_meta(paths: &SharedCachePaths, input_key: &[u8; 32]) -> Option<EntryMeta>`
  - `pub fn encode_entry_meta(meta: &EntryMeta) -> io::Result<Vec<u8>>`
  - `impl From<&ReportInput> for EntryReport` and `impl From<EntryReport> for ReportInput`
  - `pub const ENTRIES_DIR_NAME: &str = "entries";` and `SharedCachePaths::entries_dir` field

**Why a bincode struct instead of a tar:** meta has no directory structure and no path-escape surface, so tar buys nothing here. It also incidentally fixes report `mime_type`, which the tar path drops (`blob.rs:962` sets it to `String::new()` on readback).

- [ ] **Step 1: Add the entries dir to `SharedCachePaths`**

In `crates/luchta-cache/src/shared/paths.rs`, add the constant next to `SNAPSHOTS_DIR_NAME`:

```rust
/// Subdirectory name for per-entry meta objects.
pub const ENTRIES_DIR_NAME: &str = "entries";
```

Add the field to the struct:

```rust
pub struct SharedCachePaths {
    /// Root directory of the shared cache.
    pub root: PathBuf,
    /// Directory for storing blobs (content-addressed).
    pub blobs_dir: PathBuf,
    /// Directory for storing snapshots.
    pub snapshots_dir: PathBuf,
    /// Directory for storing per-entry meta objects (keyed by input_key).
    pub entries_dir: PathBuf,
}
```

And in `open_shared_paths`:

```rust
pub fn open_shared_paths(root: &Path) -> io::Result<SharedCachePaths> {
    let blobs_dir = root.join(BLOBS_DIR_NAME);
    let snapshots_dir = root.join(SNAPSHOTS_DIR_NAME);
    let entries_dir = root.join(ENTRIES_DIR_NAME);

    fs::create_dir_all(root)?;
    fs::create_dir_all(&blobs_dir)?;
    fs::create_dir_all(&snapshots_dir)?;
    fs::create_dir_all(&entries_dir)?;

    Ok(SharedCachePaths {
        root: root.to_path_buf(),
        blobs_dir,
        snapshots_dir,
        entries_dir,
    })
}
```

Extend the existing `open_shared_paths_creates_directories` test in the same file with:

```rust
        assert_eq!(paths.entries_dir, root.join(ENTRIES_DIR_NAME));
        assert!(paths.entries_dir.exists());
```

- [ ] **Step 2: Run the paths tests to verify they pass**

Run: `cargo nextest run -p luchta-cache paths::`
Expected: PASS. If it fails to compile, other constructors of `SharedCachePaths` need the new field — search with `rg -n "SharedCachePaths \{" crates/` and add `entries_dir` to each.

- [ ] **Step 3: Write the failing round-trip test**

Create `crates/luchta-cache/src/shared/entry_meta.rs` containing only this test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::paths::open_shared_paths;
    use tempfile::TempDir;

    fn sample_meta() -> EntryMeta {
        EntryMeta {
            schema_version: ENTRY_META_SCHEMA_VERSION,
            outputs_hash: [7; 32],
            record: vec![1, 2, 3, 4],
            stdout: b"out".to_vec(),
            stderr: b"err".to_vec(),
            reports: vec![EntryReport {
                filename: "lint.json".to_string(),
                mime_type: "application/json".to_string(),
                content: "{}".to_string(),
            }],
        }
    }

    #[test]
    fn write_then_read_round_trips() {
        let temp = TempDir::new().unwrap();
        let paths = open_shared_paths(temp.path()).unwrap();
        let input_key = [3; 32];

        let result = write_entry_meta(&paths, &input_key, &sample_meta()).unwrap();
        assert_eq!(result, EntryMetaWriteResult::Written);

        let read_back = read_entry_meta(&paths, &input_key).expect("meta should be readable");
        assert_eq!(read_back, sample_meta());
    }

    #[test]
    fn read_returns_none_when_absent() {
        let temp = TempDir::new().unwrap();
        let paths = open_shared_paths(temp.path()).unwrap();
        assert!(read_entry_meta(&paths, &[9; 32]).is_none());
    }

    #[test]
    fn read_returns_none_when_corrupt() {
        let temp = TempDir::new().unwrap();
        let paths = open_shared_paths(temp.path()).unwrap();
        let input_key = [4; 32];
        std::fs::write(entry_meta_path(&paths, &input_key), b"not bincode").unwrap();
        assert!(read_entry_meta(&paths, &input_key).is_none());
    }

    #[test]
    fn second_write_is_idempotent_and_keeps_first() {
        let temp = TempDir::new().unwrap();
        let paths = open_shared_paths(temp.path()).unwrap();
        let input_key = [5; 32];

        write_entry_meta(&paths, &input_key, &sample_meta()).unwrap();

        let mut second = sample_meta();
        second.stdout = b"different".to_vec();
        let result = write_entry_meta(&paths, &input_key, &second).unwrap();

        assert_eq!(result, EntryMetaWriteResult::AlreadyExists);
        assert_eq!(read_entry_meta(&paths, &input_key).unwrap().stdout, b"out");
    }

    #[test]
    fn distinct_input_keys_do_not_collide() {
        let temp = TempDir::new().unwrap();
        let paths = open_shared_paths(temp.path()).unwrap();

        let mut a = sample_meta();
        a.stdout = b"task-a".to_vec();
        let mut b = sample_meta();
        b.stdout = b"task-b".to_vec();

        // Same outputs_hash — this is the #278 scenario.
        assert_eq!(a.outputs_hash, b.outputs_hash);

        write_entry_meta(&paths, &[1; 32], &a).unwrap();
        write_entry_meta(&paths, &[2; 32], &b).unwrap();

        assert_eq!(read_entry_meta(&paths, &[1; 32]).unwrap().stdout, b"task-a");
        assert_eq!(read_entry_meta(&paths, &[2; 32]).unwrap().stdout, b"task-b");
    }
}
```

Register the module in `crates/luchta-cache/src/shared/mod.rs` next to the other `mod` declarations:

```rust
mod entry_meta;
pub use entry_meta::{
    encode_entry_meta, entry_meta_path, read_entry_meta, write_entry_meta, EntryMeta,
    EntryMetaWriteResult, EntryReport, ENTRY_META_SCHEMA_VERSION,
};
```

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo nextest run -p luchta-cache entry_meta::`
Expected: compile error — `EntryMeta`, `write_entry_meta`, `read_entry_meta`, `entry_meta_path`, `EntryMetaWriteResult`, `EntryReport`, `ENTRY_META_SCHEMA_VERSION` not found.

- [ ] **Step 5: Write the implementation**

Prepend to `crates/luchta-cache/src/shared/entry_meta.rs`, above the test module:

```rust
//! Per-entry meta objects for the shared cache.
//!
//! Meta (the run record, captured stdout/stderr, and reports) is keyed by
//! `input_key`, not by `outputs_hash`. Bundling it into the outputs blob made
//! every task with no outputs collide on a single object, because
//! `combined_outputs_hash(&[])` is a constant. See issue #278.

use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::atomicio::atomic_write;
use super::SharedCachePaths;
use crate::serialization::bincode_config;
use crate::store::ReportInput;

/// Schema version for the on-disk entry meta object.
pub const ENTRY_META_SCHEMA_VERSION: u32 = 1;

const ENTRY_META_ZSTD_LEVEL: i32 = 3;
const ENTRY_META_FILE_EXTENSION: &str = "bin";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryReport {
    pub filename: String,
    pub mime_type: String,
    pub content: String,
}

/// Everything about a cached run except the output files themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryMeta {
    pub schema_version: u32,
    /// Points at `blobs/<outputs_hash>.tar.zst`. All-zero-length output sets
    /// share one hash, which is why meta cannot live inside that blob.
    pub outputs_hash: [u8; 32],
    /// Bincode-encoded `TaskRunRecord`.
    pub record: Vec<u8>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub reports: Vec<EntryReport>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryMetaWriteResult {
    Written,
    AlreadyExists,
}

impl From<&ReportInput> for EntryReport {
    fn from(value: &ReportInput) -> Self {
        Self {
            filename: value.filename.clone(),
            mime_type: value.mime_type.clone(),
            content: value.content.clone(),
        }
    }
}

impl From<EntryReport> for ReportInput {
    fn from(value: EntryReport) -> Self {
        Self {
            filename: value.filename,
            mime_type: value.mime_type,
            content: value.content,
        }
    }
}

#[must_use]
pub fn entry_meta_path(paths: &SharedCachePaths, input_key: &[u8; 32]) -> PathBuf {
    paths.entries_dir.join(format!(
        "{}.{ENTRY_META_FILE_EXTENSION}",
        blake3::Hash::from(*input_key).to_hex()
    ))
}

/// Encode meta to its on-disk representation: bincode, then zstd.
pub fn encode_entry_meta(meta: &EntryMeta) -> io::Result<Vec<u8>> {
    let raw = bincode::serde::encode_to_vec(meta, bincode_config()).map_err(io::Error::other)?;
    zstd::encode_all(raw.as_slice(), ENTRY_META_ZSTD_LEVEL)
}

/// Write meta for `input_key`, keeping any existing object.
///
/// First writer wins, matching `SnapshotStore::merge_entry`'s idempotent-noop
/// semantics. Re-running the same task produces a record that differs only in
/// timings, so rewriting would churn the remote for no gain.
pub fn write_entry_meta(
    paths: &SharedCachePaths,
    input_key: &[u8; 32],
    meta: &EntryMeta,
) -> io::Result<EntryMetaWriteResult> {
    let path = entry_meta_path(paths, input_key);
    if path.exists() {
        return Ok(EntryMetaWriteResult::AlreadyExists);
    }
    let encoded = encode_entry_meta(meta)?;
    atomic_write(&path, &encoded).map_err(io::Error::other)?;
    Ok(EntryMetaWriteResult::Written)
}

/// Read meta for `input_key`. Returns `None` when absent, unreadable, corrupt,
/// or written by a future schema version — all of which degrade to a cache miss.
pub fn read_entry_meta(paths: &SharedCachePaths, input_key: &[u8; 32]) -> Option<EntryMeta> {
    let bytes = fs::read(entry_meta_path(paths, input_key)).ok()?;
    let raw = zstd::decode_all(bytes.as_slice()).ok()?;
    let (meta, _) = bincode::serde::decode_from_slice::<EntryMeta, _>(&raw, bincode_config()).ok()?;
    if meta.schema_version != ENTRY_META_SCHEMA_VERSION {
        return None;
    }
    Some(meta)
}
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo nextest run -p luchta-cache entry_meta:: paths::`
Expected: PASS, 5 entry_meta tests plus the paths tests.

- [ ] **Step 7: Commit**

```bash
git add crates/luchta-cache/src/shared/entry_meta.rs crates/luchta-cache/src/shared/paths.rs crates/luchta-cache/src/shared/mod.rs
git commit -m "add per-input_key entry meta object to shared cache"
```

---

### Task 2: Outputs-only blob write

**Files:**
- Modify: `crates/luchta-cache/src/shared/blob.rs:227-292` (`write_blob` — already outputs-only, needs the size-cap parameter split out)
- Test: inline tests in `blob.rs`

**Interfaces:**
- Consumes: `BlobWriteResult` from `blob.rs:20-26`, `SharedCachePaths`.
- Produces: `pub fn write_outputs_blob(paths: &SharedCachePaths, outputs_hash: &[u8; 32], package_dir: &Path, rel_output_paths: &[PathBuf], size_cap_bytes: u64) -> io::Result<BlobWriteResult>` — identical semantics to the existing `write_blob`, but returns `BlobWriteResult::NoOutputs` when there is nothing to archive **and writes no file**.

`write_blob` already does exactly this. This task is a rename plus making `NoOutputs` reachable from the caller, which it currently is not (`shared/mod.rs:703` is dead).

- [ ] **Step 1: Write the failing test**

Add to the existing `#[cfg(test)] mod tests` in `blob.rs` (the one containing `write_blob_returns_no_outputs_when_list_empty_or_missing`):

```rust
    #[test]
    fn write_outputs_blob_creates_no_file_when_there_are_no_outputs() {
        let temp = TempDir::new().unwrap();
        let paths = crate::shared::paths::open_shared_paths(temp.path()).unwrap();
        let package_dir = temp.path().join("pkg");
        std::fs::create_dir_all(&package_dir).unwrap();

        let outputs_hash = crate::resolve::combined_outputs_hash(&[]);
        let result =
            write_outputs_blob(&paths, &outputs_hash, &package_dir, &[], 1_024).unwrap();

        assert_eq!(result, BlobWriteResult::NoOutputs);
        assert!(
            !blob_path(&paths, &outputs_hash).exists(),
            "no blob file should be created for an empty output set"
        );
    }

    #[test]
    fn write_outputs_blob_omits_meta_dir_from_archive() {
        let temp = TempDir::new().unwrap();
        let paths = crate::shared::paths::open_shared_paths(temp.path()).unwrap();
        let package_dir = temp.path().join("pkg");
        std::fs::create_dir_all(package_dir.join("dist")).unwrap();
        std::fs::write(package_dir.join("dist/main.js"), "console.log(1);").unwrap();

        let outputs_hash = [21_u8; 32];
        let result = write_outputs_blob(
            &paths,
            &outputs_hash,
            &package_dir,
            &[PathBuf::from("dist/main.js")],
            1_000_000,
        )
        .unwrap();
        assert_eq!(result, BlobWriteResult::Written);

        let entries = list_entries(&blob_path(&paths, &outputs_hash)).unwrap();
        assert_eq!(entries, vec![PathBuf::from("dist/main.js")]);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p luchta-cache blob::tests::write_outputs_blob`
Expected: compile error — `write_outputs_blob` not found.

- [ ] **Step 3: Rename `write_blob` to `write_outputs_blob`**

In `crates/luchta-cache/src/shared/blob.rs`, change the signature at line 227:

```rust
/// Write an outputs-only blob.
///
/// The archive contains nothing but the task's output files. Per-task meta
/// lives in `entries/<input_key>` — see `shared/entry_meta.rs` and issue #278.
pub fn write_outputs_blob(
    paths: &SharedCachePaths,
    outputs_hash: &[u8; 32],
    package_dir: &Path,
    rel_output_paths: &[PathBuf],
    size_cap_bytes: u64,
) -> io::Result<BlobWriteResult> {
```

The body is unchanged. Update the existing call sites and tests that reference `write_blob`:

```bash
rg -n "write_blob\b" crates/
```

Rename each to `write_outputs_blob`. Leave `write_blob_with_meta` alone for now — Task 3 removes its use.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p luchta-cache blob::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/luchta-cache/src/shared/blob.rs
git commit -m "rename write_blob to write_outputs_blob"
```

---

### Task 3: Store meta and outputs as separate objects

**Files:**
- Modify: `crates/luchta-cache/src/shared/mod.rs:600-706` (`store` and `finish_store`)
- Test: inline tests in `shared/mod.rs`

**Interfaces:**
- Consumes: `write_outputs_blob` (Task 2), `write_entry_meta` / `EntryMeta` / `EntryReport` (Task 1).
- Produces: `SharedCache::store` keeps its existing signature and `StoreOutcome` values. `finish_store` gains an `input_key` parameter: `fn finish_store(&self, blob_result: BlobWriteResult, write_key: &str, input_key: &[u8; 32], entry: SnapshotEntry) -> io::Result<StoreOutcome>`.

**Size cap:** today meta bytes count toward `size_cap_bytes` alongside outputs (`blob.rs:748-755`). Preserve that — sum the encoded meta length with the output bytes and return `StoreOutcome::SkippedTooLarge` if the total exceeds the cap.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` in `crates/luchta-cache/src/shared/mod.rs`:

```rust
    #[test]
    fn two_no_output_tasks_keep_separate_meta() {
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());

        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            10,
            Some(temp_cache.path()),
        )
        .unwrap();

        let empty_hash = crate::resolve::combined_outputs_hash(&[]);

        let pkg_a = temp_repo.path().join("pkg-a");
        let pkg_b = temp_repo.path().join("pkg-b");
        fs::create_dir_all(&pkg_a).unwrap();
        fs::create_dir_all(&pkg_b).unwrap();

        let mut record_a = sample_record(true, 200);
        record_a.output_patterns = vec![];
        record_a.outputs = vec![];
        record_a.outputs_hash = empty_hash;
        record_a.task_spec_hash = [11; 32];
        let key_a = derive_input_key([11; 32], [2; 32], [3; 32], [4; 32]);

        let mut record_b = record_a.clone();
        record_b.task_spec_hash = [22; 32];
        let key_b = derive_input_key([22; 32], [2; 32], [3; 32], [4; 32]);

        cache
            .store("pkg-a#lint", &key_a, &empty_hash, &pkg_a, &[], &record_a,
                   b"A stdout", b"", &[], temp_repo.path())
            .unwrap();
        cache
            .store("pkg-b#lint", &key_b, &empty_hash, &pkg_b, &[], &record_b,
                   b"B stdout", b"", &[], temp_repo.path())
            .unwrap();

        let meta_a = read_entry_meta(cache.paths(), &key_a).expect("meta for A");
        let meta_b = read_entry_meta(cache.paths(), &key_b).expect("meta for B");

        assert_eq!(meta_a.stdout, b"A stdout");
        assert_eq!(meta_b.stdout, b"B stdout");

        assert!(
            !blob_path(cache.paths(), &empty_hash).exists(),
            "no outputs means no blob file"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p luchta-cache shared::tests::two_no_output_tasks_keep_separate_meta`
Expected: FAIL — `read_entry_meta` returns `None` because `store` still writes meta into the blob.

- [ ] **Step 3: Rewrite the store tail**

In `crates/luchta-cache/src/shared/mod.rs`, replace the block from `// Prepare meta files.` through the `self.finish_store(...)` call at the end of `store` with:

```rust
        // Prepare per-entry meta. Keyed by input_key, never by outputs_hash.
        let meta_record =
            bincode::serde::encode_to_vec(record, bincode_config()).map_err(io::Error::other)?;

        let meta = EntryMeta {
            schema_version: ENTRY_META_SCHEMA_VERSION,
            outputs_hash: *outputs_hash,
            record: meta_record,
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
            reports: reports.iter().map(EntryReport::from).collect(),
        };

        // Meta counts toward the size cap, same as when it lived in the blob.
        let meta_bytes = encode_entry_meta(&meta)?.len() as u64;
        let output_bytes: u64 = record.outputs.iter().map(|file| file.size).sum();
        let total_bytes = output_bytes.saturating_add(meta_bytes);
        if total_bytes > self.size_cap_bytes {
            return Ok(StoreOutcome::SkippedTooLarge { bytes: total_bytes });
        }

        let blob_result = write_outputs_blob(
            &self.paths,
            outputs_hash,
            package_dir,
            rel_output_paths,
            self.size_cap_bytes,
        )?;

        // Don't leave meta pointing at a blob that was never written.
        if let BlobWriteResult::SkippedTooLarge { bytes } = blob_result {
            return Ok(StoreOutcome::SkippedTooLarge { bytes });
        }

        write_entry_meta(&self.paths, input_key, &meta)?;

        let entry = SnapshotEntry {
            task_id: task_id.to_string(),
            input_key: *input_key,
            outputs_hash: *outputs_hash,
            task_spec_hash: record.task_spec_hash,
            env_hash: record.env_hash,
            pkg_dep_hash: record.pkg_dep_hash,
            duration_ms,
            output_bytes,
            cached_at_unix_ms: record.end_unix_ms,
            tool_version: None,
        };
        self.finish_store(blob_result, &write_key, input_key, entry)
    }
```

Then update `finish_store` so `NoOutputs` records a snapshot entry like the other success cases, and so the remote push carries the input key:

```rust
    fn finish_store(
        &self,
        blob_result: BlobWriteResult,
        write_key: &str,
        input_key: &[u8; 32],
        entry: SnapshotEntry,
    ) -> io::Result<StoreOutcome> {
        match blob_result {
            // NoOutputs is a success: the entry meta is what makes it restorable.
            BlobWriteResult::Written
            | BlobWriteResult::AlreadyExists
            | BlobWriteResult::NoOutputs => {
                #[cfg(unix)]
                let outputs_hash = entry.outputs_hash;
                #[cfg(unix)]
                let has_outputs = !matches!(blob_result, BlobWriteResult::NoOutputs);
                let merge = self
                    .snapshot_store
                    .merge_entry_with_outcome(write_key, entry);
                if matches!(merge.result, MergeResult::SkippedLockUnavailable) {
                    return Ok(StoreOutcome::SkippedLockUnavailable);
                }
                #[cfg(unix)]
                self.enqueue_remote_push(write_key, outputs_hash, *input_key, has_outputs, merge);
                Ok(StoreOutcome::Stored)
            }
            BlobWriteResult::SkippedTooLarge { bytes } => {
                Ok(StoreOutcome::SkippedTooLarge { bytes })
            }
        }
    }
```

Add the imports at the top of `shared/mod.rs`:

```rust
use blob::{write_outputs_blob, BlobWriteResult};
use entry_meta::{encode_entry_meta, read_entry_meta, write_entry_meta, EntryMeta, EntryReport, ENTRY_META_SCHEMA_VERSION};
```

`enqueue_remote_push` and `OwnedPushArtifacts` do not yet take `input_key` or `has_outputs` — Task 5 adds them. For now, add the two parameters to `enqueue_remote_push` and ignore them with `let _ = (input_key, has_outputs);` so this task compiles and its test passes in isolation.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p luchta-cache shared::`
Expected: PASS for `two_no_output_tasks_keep_separate_meta`. Several existing restore tests will now FAIL because `stage_entry` still reads meta from the blob — that is expected and Task 4 fixes it. Note which ones fail so you can confirm they recover.

- [ ] **Step 5: Commit**

```bash
git add crates/luchta-cache/src/shared/mod.rs
git commit -m "store shared cache meta per input_key instead of inside the blob"
```

---

### Task 4: Two-phase restore — meta first, outputs second

**Files:**
- Modify: `crates/luchta-cache/src/shared/blob.rs` (add `restore_outputs_staged`)
- Modify: `crates/luchta-cache/src/shared/mod.rs:392-437` (`stage_entry`)
- Test: inline tests in `shared/mod.rs`

**Interfaces:**
- Consumes: `read_entry_meta` (Task 1), `StagedRestore` / `BlobReadResultWithMeta` from `blob.rs:880-918`.
- Produces:
  - `pub fn restore_outputs_staged(paths: &SharedCachePaths, outputs_hash: &[u8; 32], package_dir: &Path) -> io::Result<BlobReadResultWithMeta<StagedRestore>>` — same as `restore_blob_with_meta` but the caller ignores `StagedRestore::meta`.
  - `StagedCandidate` gains `pub fn empty_outputs(outputs_hash: [u8; 32], record: TaskRunRecord, stdout: Vec<u8>, stderr: Vec<u8>, reports: Vec<ReportInput>, package_dir: &Path) -> io::Result<Self>` — a candidate with an empty staging dir, so `commit()` writes nothing and returns an empty path list.

**Back-compat:** blobs written by older clients still have `.luchta-meta/` inside. `restore_outputs_staged` reuses `extract_blob_with_meta_to_staging`, and `move_non_meta_files` already filters `META_DIR_NAME` out on commit, so those blobs restore their outputs correctly and their embedded meta is simply ignored. Snapshot entries with no `entries/` object become unrestorable and degrade to a miss until they age out. Say so in the commit message.

- [ ] **Step 1: Write the failing tests**

Add to `#[cfg(test)] mod tests` in `crates/luchta-cache/src/shared/mod.rs`:

```rust
    #[test]
    fn restore_reads_meta_from_entries_not_from_blob() {
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());

        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            10,
            Some(temp_cache.path()),
        )
        .unwrap();

        let package_dir = temp_repo.path().join("pkg");
        fs::create_dir_all(package_dir.join("dist")).unwrap();
        fs::write(package_dir.join("dist/main.js"), "console.log('hi');").unwrap();

        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);
        let record = sample_record(true, 200);

        cache
            .store("pkg#build", &input_key, &[7; 32], &package_dir,
                   &[PathBuf::from("dist/main.js")], &record,
                   b"stdout output", b"stderr output", &[], temp_repo.path())
            .unwrap();

        let restore_dir = temp_repo.path().join("restore");
        fs::create_dir_all(&restore_dir).unwrap();

        let (hit, written_paths) = cache
            .try_restore_candidates("pkg#build", &input_key, &restore_dir)
            .next()
            .expect("expected at least one candidate")
            .commit()
            .expect("commit should succeed");

        assert_eq!(hit.outputs_hash, [7; 32]);
        assert_eq!(hit.stdout, b"stdout output");
        assert_eq!(hit.stderr, b"stderr output");
        assert_eq!(written_paths, vec![restore_dir.join("dist/main.js")]);
        assert_eq!(
            fs::read(restore_dir.join("dist/main.js")).unwrap(),
            b"console.log('hi');"
        );
        assert!(!restore_dir.join(".luchta-meta").exists());
    }

    #[test]
    fn no_output_task_restores_without_touching_a_blob() {
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());

        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            10,
            Some(temp_cache.path()),
        )
        .unwrap();

        let empty_hash = crate::resolve::combined_outputs_hash(&[]);
        let package_dir = temp_repo.path().join("pkg");
        fs::create_dir_all(&package_dir).unwrap();

        let mut record = sample_record(true, 200);
        record.output_patterns = vec![];
        record.outputs = vec![];
        record.outputs_hash = empty_hash;
        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);

        cache
            .store("pkg#lint", &input_key, &empty_hash, &package_dir, &[], &record,
                   b"lint output", b"", &[], temp_repo.path())
            .unwrap();

        let restore_dir = temp_repo.path().join("restore");
        fs::create_dir_all(&restore_dir).unwrap();

        let (hit, written_paths) = cache
            .try_restore_candidates("pkg#lint", &input_key, &restore_dir)
            .next()
            .expect("expected a candidate for a no-output task")
            .commit()
            .expect("commit should succeed");

        assert_eq!(hit.stdout, b"lint output");
        assert!(written_paths.is_empty(), "nothing to write for a no-output task");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p luchta-cache shared::tests::restore_reads_meta shared::tests::no_output_task_restores`
Expected: FAIL — `stage_entry` still reads the record out of the blob, so the first test panics on `expected at least one candidate` and the second finds no blob at all.

- [ ] **Step 3: Add `restore_outputs_staged`**

In `crates/luchta-cache/src/shared/blob.rs`, next to `restore_blob_with_meta`:

```rust
/// Restore an outputs-only blob into a staging directory.
///
/// Blobs written by older clients still carry a `.luchta-meta/` directory.
/// Its contents are ignored here — `entries/<input_key>` is authoritative —
/// and `move_non_meta_files` filters it out on commit.
pub fn restore_outputs_staged(
    paths: &SharedCachePaths,
    outputs_hash: &[u8; 32],
    package_dir: &Path,
) -> io::Result<BlobReadResultWithMeta<StagedRestore>> {
    restore_blob_with_meta(paths, outputs_hash, package_dir)
}
```

- [ ] **Step 4: Add `StagedCandidate::empty_outputs` and rewrite `stage_entry`**

In `crates/luchta-cache/src/shared/mod.rs`, add to `impl StagedCandidate`:

```rust
    /// A candidate with no output files. Commits to an empty path list.
    pub fn empty_outputs(
        outputs_hash: [u8; 32],
        record: TaskRunRecord,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        reports: Vec<crate::store::ReportInput>,
        package_dir: &Path,
    ) -> std::io::Result<Self> {
        Ok(Self {
            outputs_hash,
            record,
            stdout,
            stderr,
            reports,
            staged: blob::StagedRestore::empty(package_dir)?,
        })
    }
```

And in `blob.rs`, add the matching constructor to `impl StagedRestore`:

```rust
    /// A staged restore holding no files. Commits to an empty path list.
    pub fn empty(package_dir: &Path) -> io::Result<Self> {
        let staging_dir = tempfile::Builder::new()
            .prefix("blob-restore-empty-")
            .tempdir_in(package_dir)?;
        Ok(Self {
            meta: MetaFiles {
                stdout: Vec::new(),
                stderr: Vec::new(),
                record: Vec::new(),
                reports: Vec::new(),
            },
            staging_dir,
            package_dir: package_dir.to_path_buf(),
        })
    }
```

Replace `stage_entry` in `shared/mod.rs` with:

```rust
    /// Stage a single entry, returning a StagedCandidate for validation.
    ///
    /// Two-phase: fetch the small `entries/<input_key>` object first and decode
    /// the record from it. Only pull the outputs blob if the entry actually has
    /// outputs. A candidate rejected by `decide_shared_restore` therefore never
    /// costs an outputs download.
    fn stage_entry(
        entry: &SnapshotEntry,
        paths: &SharedCachePaths,
        package_dir: &Path,
        #[cfg(unix)] remote: Option<&RemoteSync>,
    ) -> Option<StagedCandidate> {
        #[cfg(unix)]
        if read_entry_meta(paths, &entry.input_key).is_none() {
            if let Some(remote) = remote {
                if let Err(err) = remote.pull_entry_meta(paths, &entry.input_key) {
                    eprintln!(
                        "debug: remote entry meta pull failed for input_key={}: {err}",
                        hex_hash(entry.input_key)
                    );
                }
            }
        }

        let meta = read_entry_meta(paths, &entry.input_key)?;

        let record: TaskRunRecord =
            match bincode::serde::decode_from_slice(&meta.record, bincode_config()) {
                Ok((record, _)) => record,
                Err(_) => return None,
            };

        let reports: Vec<crate::store::ReportInput> = meta
            .reports
            .into_iter()
            .map(crate::store::ReportInput::from)
            .collect();

        if record.outputs.is_empty() {
            return StagedCandidate::empty_outputs(
                meta.outputs_hash,
                record,
                meta.stdout,
                meta.stderr,
                reports,
                package_dir,
            )
            .ok();
        }

        if !blob_path(paths, &meta.outputs_hash).is_file() {
            #[cfg(unix)]
            if let Some(remote) = remote {
                if let Err(err) = remote.pull_blob(paths, &meta.outputs_hash) {
                    eprintln!(
                        "debug: remote blob pull failed for outputs_hash={}: {err}",
                        hex_hash(meta.outputs_hash)
                    );
                }
            }
        }

        let staged = match restore_outputs_staged(paths, &meta.outputs_hash, package_dir) {
            Ok(BlobReadResultWithMeta::Restored(staged)) => staged,
            Ok(BlobReadResultWithMeta::Missing) | Ok(BlobReadResultWithMeta::Corrupt) => {
                return None
            }
            Err(_) => return None,
        };

        Some(StagedCandidate {
            outputs_hash: meta.outputs_hash,
            record,
            stdout: meta.stdout,
            stderr: meta.stderr,
            reports,
            staged,
        })
    }
```

`RemoteSync::pull_entry_meta` does not exist until Task 5. To keep this task compiling and testable on its own, add a stub in `remote.rs`:

```rust
    pub(crate) fn pull_entry_meta(
        &self,
        _paths: &SharedCachePaths,
        _input_key: &[u8; 32],
    ) -> Result<(), rclone::RcloneError> {
        Ok(())
    }
```

Update the `restore_outputs_staged` and `read_entry_meta` imports at the top of `shared/mod.rs`.

- [ ] **Step 5: Run the full crate tests**

Run: `cargo nextest run -p luchta-cache`
Expected: PASS. The restore tests that broke in Task 3 recover here. If `store_and_restore_round_trip_byte_identical` still fails, check that `store` writes the entry meta before `finish_store` returns.

- [ ] **Step 6: Commit**

```bash
git add crates/luchta-cache/src/shared/blob.rs crates/luchta-cache/src/shared/mod.rs crates/luchta-cache/src/shared/remote.rs
git commit -m "restore shared cache entries from per-entry meta, then outputs

Meta is read from entries/<input_key> and the outputs blob is only pulled
when the entry has outputs. Blobs written by older clients still restore
their outputs; their embedded .luchta-meta is ignored. Snapshot entries
with no entries/ object degrade to a miss until they age out."
```

---

### Task 5: Remote push and pull for entry meta

**Files:**
- Modify: `crates/luchta-cache/src/shared/remote.rs:220-232` (`entries_fs`), `:339-368` (`push_store_artifacts_owned`), `:370-385` (replace the `pull_entry_meta` stub), `:430-495` (`push_store_artifacts`, `push_blob_if_missing`)
- Modify: `crates/luchta-cache/src/shared/mod.rs` (`enqueue_remote_push` — pass the parameters added in Task 3 for real)
- Test: inline tests in `remote.rs`, following `remote_store_skips_blob_reupload_when_remote_blob_exists` at `remote.rs:1453`

**Interfaces:**
- Consumes: `EntryMeta` encoding via `entry_meta_path` (Task 1).
- Produces:
  - `RemoteSync::pull_entry_meta(&self, paths: &SharedCachePaths, input_key: &[u8; 32]) -> Result<(), rclone::RcloneError>`
  - `OwnedPushArtifacts` and `PushArtifacts` gain `input_key: [u8; 32]` and `has_outputs: bool`
  - `SharedCache::enqueue_remote_push(&self, write_key: &str, outputs_hash: [u8; 32], input_key: [u8; 32], has_outputs: bool, merge: MergeEntryOutcome)`

- [ ] **Step 1: Write the failing test**

Add to `#[cfg(test)] mod tests` in `crates/luchta-cache/src/shared/remote.rs`, modelled on the existing `remote_store_skips_blob_reupload_when_remote_blob_exists`:

```rust
    #[test]
    fn remote_store_uploads_entry_meta_and_second_machine_restores_no_output_task() {
        let remote_root = tempfile::tempdir().unwrap();
        let machine_a_cache = tempfile::tempdir().unwrap();
        let machine_b_cache = tempfile::tempdir().unwrap();

        let repo = tempfile::tempdir().unwrap();
        crate::shared::tests::setup_git_repo(repo.path());
        crate::shared::tests::create_commit(repo.path());

        let empty_hash = crate::resolve::combined_outputs_hash(&[]);
        let package_dir = repo.path().join("pkg");
        std::fs::create_dir_all(&package_dir).unwrap();

        let mut record = crate::shared::tests::sample_record(true, 200);
        record.output_patterns = vec![];
        record.outputs = vec![];
        record.outputs_hash = empty_hash;
        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);

        // `open_cache_with_remote` lives at remote.rs:677 and takes a &RemoteSync.
        // Build one against a :local: path the same way remote.rs:903 does.
        let seed_remote = RemoteSync::new(
            Arc::new(RcloneRcd::with_default_timeout().unwrap()),
            format!(":local:{}", remote_root.path().display()),
            8,
        );

        let cache_a = open_cache_with_remote(repo.path(), machine_a_cache.path(), &seed_remote);
        cache_a
            .store("pkg#lint", &input_key, &empty_hash, &package_dir, &[], &record,
                   b"lint output", b"", &[], repo.path())
            .unwrap();
        cache_a.flush_push_queue();

        assert!(
            remote_root.path().join("entries").read_dir().unwrap().count() > 0,
            "entry meta should be uploaded"
        );

        let cache_b = open_cache_with_remote(repo.path(), machine_b_cache.path(), remote_root.path());
        let restore_dir = repo.path().join("restore");
        std::fs::create_dir_all(&restore_dir).unwrap();

        let candidate = cache_b
            .try_restore_candidates("pkg#lint", &input_key, &restore_dir)
            .next()
            .expect("machine B should find the entry");
        assert_eq!(candidate.stdout, b"lint output");
    }
```

Reuse whatever helper the neighbouring tests use to build a `SharedCache` with a `:local:` remote; if there isn't one, extract the setup from `remote_store_skips_blob_reupload_when_remote_blob_exists` into `fn open_cache_with_remote(repo_root: &Path, cache_dir: &Path, remote_root: &Path) -> SharedCache` in the test module and use it from both.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p luchta-cache remote::tests::remote_store_uploads_entry_meta`
Expected: FAIL — nothing is uploaded to `entries/`, so the `read_dir` count assertion trips.

- [ ] **Step 3: Implement the remote entry-meta paths**

In `remote.rs`, add next to `blobs_fs`:

```rust
    fn entries_fs(&self) -> String {
        format!(
            "{}/{ENTRIES_DIR_NAME}",
            self.remote_base_fs.trim_end_matches('/')
        )
    }
```

Import `ENTRIES_DIR_NAME` alongside the existing `BLOBS_DIR_NAME` / `SNAPSHOTS_DIR_NAME` imports.

Replace the Task 4 stub with the real pull:

```rust
    pub(crate) fn pull_entry_meta(
        &self,
        paths: &SharedCachePaths,
        input_key: &[u8; 32],
    ) -> Result<(), rclone::RcloneError> {
        if self.is_disabled() {
            return Ok(());
        }
        let local_path = entry_meta_path(paths, input_key);
        if local_path.exists() {
            return Ok(());
        }
        let file_name = format!("{}.bin", hex_hash(*input_key));
        self.copy_remote_file_down(&self.entries_fs(), &file_name, &local_path)
            .inspect(|_| self.record_remote_success())
            .inspect_err(|err| self.record_remote_error(err))
    }
```

Add the push, mirroring `push_blob_if_missing`:

```rust
    fn push_entry_meta_if_missing(&self, paths: &SharedCachePaths, input_key: &[u8; 32]) {
        let remote_fs = self.entries_fs();
        let file_name = format!("{}.bin", hex_hash(*input_key));
        match self
            .rclone
            .stat(&remote_fs, &file_name, self.rclone.default_timeout())
        {
            Ok(Some(_)) => {
                self.record_remote_success();
                return;
            }
            Ok(None) => {
                self.record_remote_success();
            }
            Err(err) => {
                self.record_remote_error(&err);
                eprintln!("warn: shared cache remote entry meta stat failed for {file_name}: {err}");
                return;
            }
        }

        let local_path = entry_meta_path(paths, input_key);
        if let Err(err) = self.copy_local_file_up(&local_path, &remote_fs, &file_name) {
            self.record_remote_error(&err);
            eprintln!("warn: shared cache remote entry meta upload failed for {file_name}: {err}");
        } else {
            self.record_remote_success();
        }
    }
```

Add `input_key: [u8; 32]` and `has_outputs: bool` to both `OwnedPushArtifacts` and `PushArtifacts`, thread them through `push_store_artifacts_owned`, and change the head of `push_store_artifacts` to:

```rust
        let PushArtifacts {
            paths,
            commit_key,
            outputs_hash,
            input_key,
            has_outputs,
            merge,
        } = push;

        if has_outputs {
            self.push_blob_if_missing(paths, outputs_hash);
        }
        self.push_entry_meta_if_missing(paths, input_key);
```

In `shared/mod.rs`, make `enqueue_remote_push` pass them through instead of discarding them:

```rust
    #[cfg(unix)]
    fn enqueue_remote_push(
        &self,
        write_key: &str,
        outputs_hash: [u8; 32],
        input_key: [u8; 32],
        has_outputs: bool,
        merge: MergeEntryOutcome,
    ) {
        let Some(remote) = &self.remote else {
            return;
        };
        if remote.is_disabled() {
            return;
        }
        remote.enqueue_push_store_artifacts(remote::OwnedPushArtifacts {
            paths: Arc::clone(&self.paths),
            commit_key: write_key.to_string(),
            outputs_hash,
            input_key,
            has_outputs,
            merge,
        });
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p luchta-cache`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/luchta-cache/src/shared/remote.rs crates/luchta-cache/src/shared/mod.rs
git commit -m "sync entry meta objects to and from the remote shared cache"
```

---

### Task 6: GC the entries dir, plus the #278 e2e regression test

**Files:**
- Modify: `crates/luchta-cache/src/shared/gc.rs:28-36` (`run_gc`), add `gc_entries_dir`
- Create: `crates/luchta-cli/tests/shared_cache_no_output_e2e.rs`
- Test: inline tests in `gc.rs`; the new e2e file

**Interfaces:**
- Consumes: `SharedCachePaths::entries_dir` (Task 1), the full store/restore path (Tasks 3–5).
- Produces: `GcStats` gains `pub entries_deleted: usize`.

- [ ] **Step 1: Write the failing GC test**

Add to `#[cfg(test)] mod tests` in `gc.rs`:

```rust
    #[test]
    fn run_gc_deletes_old_entry_meta() {
        let temp = TempDir::new().unwrap();
        let paths = crate::shared::paths::open_shared_paths(temp.path()).unwrap();
        let input_key = [12_u8; 32];

        let meta = crate::shared::EntryMeta {
            schema_version: crate::shared::ENTRY_META_SCHEMA_VERSION,
            outputs_hash: [1; 32],
            has_outputs: false,
            record: vec![0],
            stdout: Vec::new(),
            stderr: Vec::new(),
            reports: Vec::new(),
        };
        crate::shared::write_entry_meta(&paths, &input_key, &meta).unwrap();

        let path = crate::shared::entry_meta_path(&paths, &input_key);
        set_mtime(&path, SystemTime::now() - Duration::from_secs(60 * 60 * 24 * 30));

        let stats = run_gc(&paths, Duration::from_secs(60 * 60 * 24 * 7));

        assert_eq!(stats.entries_deleted, 1);
        assert!(!path.exists());
    }

    #[test]
    fn run_gc_keeps_recent_entry_meta() {
        let temp = TempDir::new().unwrap();
        let paths = crate::shared::paths::open_shared_paths(temp.path()).unwrap();
        let input_key = [13_u8; 32];

        let meta = crate::shared::EntryMeta {
            schema_version: crate::shared::ENTRY_META_SCHEMA_VERSION,
            outputs_hash: [1; 32],
            has_outputs: false,
            record: vec![0],
            stdout: Vec::new(),
            stderr: Vec::new(),
            reports: Vec::new(),
        };
        crate::shared::write_entry_meta(&paths, &input_key, &meta).unwrap();

        let stats = run_gc(&paths, Duration::from_secs(60 * 60 * 24 * 7));

        assert_eq!(stats.entries_deleted, 0);
        assert!(crate::shared::entry_meta_path(&paths, &input_key).exists());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p luchta-cache gc::tests::run_gc_deletes_old_entry_meta gc::tests::run_gc_keeps_recent_entry_meta`
Expected: compile error — `GcStats` has no field `entries_deleted`.

- [ ] **Step 3: Implement `gc_entries_dir`**

Add `pub entries_deleted: usize` to `GcStats`, call the new function from `run_gc`:

```rust
pub fn run_gc(paths: &SharedCachePaths, retention: Duration) -> GcStats {
    let now = SystemTime::now();
    let mut stats = GcStats::default();

    gc_snapshot_dir(paths, retention, now, &mut stats);
    gc_blob_dir(paths, retention, now, &mut stats);
    gc_entries_dir(paths, retention, now, &mut stats);

    stats
}
```

And add, modelled on `gc_blob_dir`:

```rust
const ENTRY_META_SUFFIX: &str = ".bin";

fn gc_entries_dir(
    paths: &SharedCachePaths,
    retention: Duration,
    now: SystemTime,
    stats: &mut GcStats,
) {
    let entries = match fs::read_dir(&paths.entries_dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !has_file_name_suffix(&path, ENTRY_META_SUFFIX) {
            continue;
        }
        if !is_older_than(&path, retention, now) {
            continue;
        }

        // Age-based, like blobs. A reader that loses the race treats the
        // missing meta as a cache miss and reruns the task.
        let meta_bytes = file_len(&path);
        if remove_file_if_exists(&path) {
            stats.entries_deleted += 1;
            stats.bytes_freed = stats.bytes_freed.saturating_add(meta_bytes);
        }
    }
}
```

- [ ] **Step 4: Run the GC tests to verify they pass**

Run: `cargo nextest run -p luchta-cache gc::`
Expected: PASS.

- [ ] **Step 5: Write the e2e regression test for #278**

Create `crates/luchta-cli/tests/shared_cache_no_output_e2e.rs`:

```rust
mod common;

use assert_cmd::Command;
use assert_fs::prelude::*;
use common::{init_git, write_counter_task_config, write_root_workspace};

fn run(temp: &assert_fs::TempDir, cache_dir: &std::path::Path, task: &str) -> String {
    let out = Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg(task)
        .arg("--workspace-root")
        .arg(temp.path())
        .env("LUCHTA_SHARED_CACHE", "1")
        .env("LUCHTA_SHARED_CACHE_DIR", cache_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out).unwrap()
}

/// Regression for #278: every task with no declared outputs used to map to the
/// single blob named by `combined_outputs_hash(&[])`, so only the first one to
/// store could ever be restored.
#[test]
fn two_no_output_tasks_both_hit_the_shared_cache() {
    let shared_cache_dir = tempfile::tempdir().unwrap();
    let temp = assert_fs::TempDir::new().unwrap();
    write_root_workspace(&temp);

    write_counter_task_config(
        &temp,
        r#""app#lint":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":[],"command":"sleep 0.15 && count=$(cat ../../lint-counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > ../../lint-counter.txt"},"app#test":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":[],"command":"sleep 0.15 && count=$(cat ../../test-counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > ../../test-counter.txt"}}"#,
    );

    temp.child("packages/app/src.txt").write_str("source\n").unwrap();
    temp.child("packages/app/package.json")
        .write_str(
            r#"{
  "name": "app",
  "scripts": {
    "lint": "echo ignored",
    "test": "echo ignored"
  }
}"#,
        )
        .unwrap();
    init_git(&temp);

    run(&temp, shared_cache_dir.path(), "lint");
    run(&temp, shared_cache_dir.path(), "test");

    // Drop the local cache so the second pass has to come from the shared cache.
    std::fs::remove_dir_all(temp.path().join(".luchta/cache")).unwrap();

    let second_lint = run(&temp, shared_cache_dir.path(), "lint");
    let second_test = run(&temp, shared_cache_dir.path(), "test");

    assert!(second_lint.contains("📥 1"), "lint should be a shared hit, got:\n{second_lint}");
    assert!(second_test.contains("📥 1"), "test should be a shared hit, got:\n{second_test}");

    // Neither command body ran a second time.
    assert_eq!(
        std::fs::read_to_string(temp.path().join("lint-counter.txt")).unwrap(),
        "1\n"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("test-counter.txt")).unwrap(),
        "1\n"
    );
}
```

- [ ] **Step 6: Run the e2e test to verify it passes**

Run: `cargo nextest run -p luchta-cli --test shared_cache_no_output_e2e`
Expected: PASS. Before Phase 1 this test fails with `test should be a shared hit` and `test-counter.txt` reading `2\n`.

- [ ] **Step 7: Run the full workspace suite**

Run: `cargo nextest run --workspace`
Expected: PASS. Fix any shared-cache test that asserted meta lives inside the blob — those assertions are now wrong and should assert against `entries/` instead.

- [ ] **Step 8: Commit**

```bash
git add crates/luchta-cache/src/shared/gc.rs crates/luchta-cli/tests/shared_cache_no_output_e2e.rs
git commit -m "gc entry meta objects and cover the no-output collision e2e

Closes #278."
```

**Phase 1 is shippable here.**

---

## Phase 2 — Recency-based shard discovery (#277)

### Task 7: Session shard ids replace commit keys

**Files:**
- Create: `crates/luchta-cache/src/shared/discovery.rs`
- Delete: `crates/luchta-cache/src/shared/git.rs`
- Modify: `crates/luchta-cache/src/shared/mod.rs:19` (re-exports), `:257-263` and `:303-309` (construction)
- Modify: `crates/luchta-cache/src/shared/snapshot.rs` — rename the `commit_key` parameter to `shard_key` throughout. Behavior unchanged.
- Test: inline tests in `discovery.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub fn new_session_shard_key(now_unix_ms: u64, nonce: u64) -> String` — returns `"{now_unix_ms:013}-{nonce:08x}"`. Zero-padded so lexical order matches chronological order.
  - `pub fn current_session_shard_key() -> String` — wraps the above using the wall clock and a process-random nonce.

The zero-padded millisecond prefix means shard dirs sort chronologically by name, which gives a working fallback when a remote does not report ModTime.

`gix` may now be unused in `luchta-cache`. Check with `cargo build -p luchta-cache` and drop the dependency from `crates/luchta-cache/Cargo.toml` if the compiler says it is unused.

- [ ] **Step 1: Write the failing test**

Create `crates/luchta-cache/src/shared/discovery.rs` with just this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_shard_keys_sort_chronologically_by_name() {
        let earlier = new_session_shard_key(1_754_431_200_123, 0x0000_00ff);
        let later = new_session_shard_key(1_754_431_200_456, 0x0000_0001);
        assert!(earlier < later, "{earlier} should sort before {later}");
    }

    #[test]
    fn session_shard_key_has_stable_shape() {
        assert_eq!(
            new_session_shard_key(1_754_431_200_123, 0xdead_beef),
            "1754431200123-deadbeef"
        );
    }

    #[test]
    fn session_shard_keys_differ_for_the_same_millisecond() {
        let first = new_session_shard_key(1_754_431_200_123, 1);
        let second = new_session_shard_key(1_754_431_200_123, 2);
        assert_ne!(first, second);
    }

    #[test]
    fn current_session_shard_key_is_unique_across_calls() {
        let first = current_session_shard_key();
        let second = current_session_shard_key();
        assert_ne!(first, second);
    }
}
```

Register it in `shared/mod.rs`:

```rust
mod discovery;
pub use discovery::{current_session_shard_key, new_session_shard_key};
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p luchta-cache discovery::`
Expected: compile error — `new_session_shard_key` and `current_session_shard_key` not found.

- [ ] **Step 3: Implement the shard key generator**

Prepend to `discovery.rs`:

```rust
//! Shard discovery for the shared cache.
//!
//! Shards used to be named by git commit id and discovered by walking
//! first-parent ancestry from HEAD. That fails whenever builds run on commits
//! no other build will ever see — feature branches, and especially Prow's
//! temporary merged-with-master commits. See issue #277.
//!
//! Shards are now named `<unix_ms>-<nonce>` and discovered by recency.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static SHARD_NONCE: AtomicU64 = AtomicU64::new(0);

/// Build a shard key from an explicit timestamp and nonce.
///
/// The millisecond field is zero-padded to 13 digits so lexical ordering
/// matches chronological ordering — the fallback when a remote listing does
/// not report modification times.
#[must_use]
pub fn new_session_shard_key(now_unix_ms: u64, nonce: u64) -> String {
    format!("{now_unix_ms:013}-{:08x}", nonce & 0xffff_ffff)
}

/// Build a shard key for this process's current run.
#[must_use]
pub fn current_session_shard_key() -> String {
    let now_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0);
    let nonce = SHARD_NONCE.fetch_add(1, Ordering::Relaxed) ^ (std::process::id() as u64) << 16;
    new_session_shard_key(now_unix_ms, nonce)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p luchta-cache discovery::`
Expected: PASS.

- [ ] **Step 5: Switch `SharedCache` construction off commit keys**

In `shared/mod.rs`, in both `open_with_remote` and `from_parts_for_test`, replace the commit-key block with:

```rust
        let write_commit_key = Some(current_session_shard_key());
        let candidate_keys = discover_recent_shard_keys(&paths, history_len);
```

`discover_recent_shard_keys` arrives in Task 8. For this task, stub it in `discovery.rs` so the crate compiles:

```rust
/// Placeholder until Task 8 implements local recency discovery.
pub fn discover_recent_shard_keys(_paths: &SharedCachePaths, _limit: usize) -> Vec<String> {
    Vec::new()
}
```

Both functions take `repo_root: &Path` and no longer use it. Keep the parameter — the CLI passes it and Task 9 does not need it back — and silence the warning with `let _ = repo_root;`.

Delete `crates/luchta-cache/src/shared/git.rs` and remove `mod git;` plus the `pub use git::{...}` line from `shared/mod.rs`. Fix the resulting test failures: `shared/mod.rs:1313` asserts `cache.candidate_keys().contains(&commit1)` — delete that assertion, it is testing the behavior being removed.

- [ ] **Step 6: Run the crate tests**

Run: `cargo nextest run -p luchta-cache`
Expected: PASS for unit tests. Shared-cache restore tests that relied on commit-key discovery will fail because `discover_recent_shard_keys` returns nothing — that is expected and Task 8 fixes it. Record which ones fail.

- [ ] **Step 7: Commit**

```bash
git add crates/luchta-cache/src/shared/discovery.rs crates/luchta-cache/src/shared/mod.rs crates/luchta-cache/src/shared/snapshot.rs
git rm crates/luchta-cache/src/shared/git.rs
git commit -m "name shared cache shards by session id instead of git commit"
```

---

### Task 8: Local recency discovery

**Files:**
- Modify: `crates/luchta-cache/src/shared/discovery.rs` (replace the stub)
- Modify: `crates/luchta-cli/tests/shared_cache_e2e.rs` (rewrite two obsolete commit-key tests — see Step 5)
- Test: inline tests in `discovery.rs`; rewritten e2e tests in `shared_cache_e2e.rs`

**Interfaces:**
- Consumes: `SharedCachePaths::snapshots_dir`, `new_session_shard_key` (Task 7).
- Produces:
  - `pub struct ShardCandidate { pub key: String, pub modified_unix_ms: u64 }`
  - `pub fn rank_shard_candidates(candidates: Vec<ShardCandidate>, limit: usize, max_age_ms: Option<u64>, now_unix_ms: u64) -> Vec<String>` — pure, newest-first, at most `limit`, dropping anything older than `max_age_ms`.
  - `pub fn discover_recent_shard_keys(paths: &SharedCachePaths, limit: usize) -> Vec<String>` — lists `snapshots_dir`, builds `ShardCandidate`s from directory mtimes, and ranks them.
  - `pub const DEFAULT_SHARD_MAX_AGE_MS: u64 = 1000 * 60 * 60 * 24 * 14;` (14 days)

Keeping the ranking pure makes it testable without a filesystem and lets Task 9 feed it remote listings through the same function.

- [ ] **Step 1: Write the failing tests**

Add to the test module in `discovery.rs`:

```rust
    fn candidate(key: &str, modified_unix_ms: u64) -> ShardCandidate {
        ShardCandidate {
            key: key.to_string(),
            modified_unix_ms,
        }
    }

    const NOW: u64 = 1_754_431_200_000;

    #[test]
    fn shard_key_zero_pads_short_timestamps_so_lexical_order_stays_chronological() {
        // The 13-digit pad is the ordering fallback for remotes that don't report
        // ModTime. Task 7's tests all used already-13-digit values, so padding never
        // actually fired — this covers the case where it does.
        let short = new_session_shard_key(5, 0);
        let full = new_session_shard_key(1_754_431_200_123, 0);
        assert_eq!(short, "0000000000005-00000000");
        assert!(short < full, "{short} must sort before {full}");
    }

    #[test]
    fn rank_returns_newest_first() {
        let ranked = rank_shard_candidates(
            vec![
                candidate("a", NOW - 3_000),
                candidate("b", NOW - 1_000),
                candidate("c", NOW - 2_000),
            ],
            10,
            None,
            NOW,
        );
        assert_eq!(ranked, vec!["b", "c", "a"]);
    }

    #[test]
    fn rank_applies_the_limit() {
        let ranked = rank_shard_candidates(
            vec![
                candidate("a", NOW - 3_000),
                candidate("b", NOW - 1_000),
                candidate("c", NOW - 2_000),
            ],
            2,
            None,
            NOW,
        );
        assert_eq!(ranked, vec!["b", "c"]);
    }

    #[test]
    fn rank_drops_shards_older_than_max_age() {
        let ranked = rank_shard_candidates(
            vec![
                candidate("fresh", NOW - 1_000),
                candidate("stale", NOW - 100_000),
            ],
            10,
            Some(10_000),
            NOW,
        );
        assert_eq!(ranked, vec!["fresh"]);
    }

    #[test]
    fn rank_breaks_mtime_ties_by_key_descending() {
        let ranked = rank_shard_candidates(
            vec![candidate("0000000000001-aaaa", NOW), candidate("0000000000001-bbbb", NOW)],
            10,
            None,
            NOW,
        );
        assert_eq!(ranked, vec!["0000000000001-bbbb", "0000000000001-aaaa"]);
    }

    #[test]
    fn discover_finds_local_shard_dirs_newest_first() {
        use std::time::{Duration, SystemTime};
        let temp = tempfile::TempDir::new().unwrap();
        let paths = crate::shared::paths::open_shared_paths(temp.path()).unwrap();

        for key in ["0000000000001-aaaa", "0000000000002-bbbb", "0000000000003-cccc"] {
            std::fs::create_dir_all(paths.snapshots_dir.join(key)).unwrap();
        }

        // Make "0000000000001-aaaa" the newest by mtime to prove mtime wins over name.
        let newest = paths.snapshots_dir.join("0000000000001-aaaa");
        filetime::set_file_mtime(
            &newest,
            filetime::FileTime::from_system_time(SystemTime::now() + Duration::from_secs(60)),
        )
        .unwrap();

        let discovered = discover_recent_shard_keys(&paths, 10);
        assert_eq!(discovered.first().map(String::as_str), Some("0000000000001-aaaa"));
        assert_eq!(discovered.len(), 3);
    }

    #[test]
    fn discover_returns_empty_when_snapshots_dir_is_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        let paths = SharedCachePaths {
            root: temp.path().to_path_buf(),
            blobs_dir: temp.path().join("blobs"),
            snapshots_dir: temp.path().join("does-not-exist"),
            entries_dir: temp.path().join("entries"),
        };
        assert!(discover_recent_shard_keys(&paths, 10).is_empty());
    }
```

`filetime` is already a dev-dependency of this crate (`gc.rs` tests use `set_mtime`). If `cargo nextest` reports it missing, add `filetime` to `[dev-dependencies]` in `crates/luchta-cache/Cargo.toml`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p luchta-cache discovery::`
Expected: compile error — `ShardCandidate` and `rank_shard_candidates` not found; `discover_recent_shard_keys` returns an empty vec so the discovery tests fail too.

- [ ] **Step 3: Implement discovery**

Replace the stub in `discovery.rs`:

```rust
use std::fs;
use std::path::Path;

use super::SharedCachePaths;

/// Drop shards older than two weeks. Long enough to span a quiet weekend,
/// short enough that the merged index stays small.
pub const DEFAULT_SHARD_MAX_AGE_MS: u64 = 1000 * 60 * 60 * 24 * 14;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardCandidate {
    pub key: String,
    pub modified_unix_ms: u64,
}

/// Rank candidates newest-first, capped at `limit` and filtered by `max_age_ms`.
///
/// Ties on modification time fall back to key order descending, which is
/// chronological because keys are zero-padded millisecond timestamps.
#[must_use]
pub fn rank_shard_candidates(
    candidates: Vec<ShardCandidate>,
    limit: usize,
    max_age_ms: Option<u64>,
    now_unix_ms: u64,
) -> Vec<String> {
    let mut kept: Vec<ShardCandidate> = candidates
        .into_iter()
        .filter(|candidate| match max_age_ms {
            Some(max_age_ms) => {
                now_unix_ms.saturating_sub(candidate.modified_unix_ms) <= max_age_ms
            }
            None => true,
        })
        .collect();

    kept.sort_unstable_by(|left, right| {
        right
            .modified_unix_ms
            .cmp(&left.modified_unix_ms)
            .then_with(|| right.key.cmp(&left.key))
    });
    kept.truncate(limit);
    kept.into_iter().map(|candidate| candidate.key).collect()
}

/// Discover shard keys present in the local cache, newest-first.
#[must_use]
pub fn discover_recent_shard_keys(paths: &SharedCachePaths, limit: usize) -> Vec<String> {
    let candidates = local_shard_candidates(&paths.snapshots_dir);
    let now_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0);
    rank_shard_candidates(
        candidates,
        limit,
        Some(DEFAULT_SHARD_MAX_AGE_MS),
        now_unix_ms,
    )
}

fn local_shard_candidates(snapshots_dir: &Path) -> Vec<ShardCandidate> {
    let Ok(entries) = fs::read_dir(snapshots_dir) else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(key) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let modified_unix_ms = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0);
        candidates.push(ShardCandidate {
            key: key.to_string(),
            modified_unix_ms,
        });
    }
    candidates
}
```

Export `ShardCandidate`, `rank_shard_candidates`, `discover_recent_shard_keys`, and `DEFAULT_SHARD_MAX_AGE_MS` from `shared/mod.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p luchta-cache`
Expected: PASS, including the restore tests that broke in Task 7 — they write and read within one process, so the local shard dir is discoverable.

- [ ] **Step 5: Rewrite the two obsolete commit-key tests**

Task 7 removed git-commit shard naming, which left two e2e tests in
`crates/luchta-cli/tests/shared_cache_e2e.rs` asserting on a directory layout that no longer
exists. Both protect properties that DO survive — rewrite them to assert the property instead
of the mechanism. Do not delete them.

`dirty_key_isolation` currently asserts `snapshots/<commit>-dirty` exists and `snapshots/<commit>`
does not. The surviving property is that a clean build must not consume a dirty build's entry —
now enforced by `decide_shared_restore` comparing `record.inputs` against the working tree, not by
key namespacing. Rewrite it as: build with a dirty tree (counter reaches 1), commit the change,
build again clean, and assert the second build did NOT report a shared hit and the counter advanced
to 2. Drop every assertion about snapshot directory names. Rename it to
`dirty_tree_entry_is_not_reused_by_clean_build` and update the doc comment to describe the
content-validation mechanism.

`accumulation_single_snapshot_multiple_entries` currently builds the path
`snapshots/<commit>` and asserts one shard dir holds both entries. Under session shard keys two
separate `luchta run` invocations produce two shard dirs, so that premise is gone; what survives is
that both entries remain discoverable. Rewrite it as: run `lint`, run `test`, then delete the local
cache and re-run each, asserting both report a shared hit and neither counter advances. Rename it to
`entries_from_separate_runs_are_both_discoverable`.

Both rewrites must be discriminating. For the dirty test, confirm it fails if you make
`decide_shared_restore` return `true` unconditionally. Report that evidence.

- [ ] **Step 6: Run the full suite**

Run: `cargo nextest run --workspace --no-fail-fast`
Expected: all tests passing, 0 failed. Task 7 left 17 failures; this task must clear every one.

- [ ] **Step 7: Commit**

```bash
git add crates/luchta-cache/src/shared/discovery.rs crates/luchta-cache/src/shared/mod.rs crates/luchta-cli/tests/shared_cache_e2e.rs
git commit -m "discover shared cache shards by recency instead of git ancestry"
```

---

### Task 9: Remote shard listing

**Files:**
- Modify: `crates/luchta-cache/src/shared/rclone/mod.rs:31-42` (`Entry`)
- Modify: `crates/luchta-cache/src/shared/remote.rs` (add `list_shard_candidates`; `snapshots_fs` gains a no-arg root variant)
- Modify: `crates/luchta-cache/src/shared/mod.rs:481-495` (`pull_candidate_commits`)
- Test: inline tests in `remote.rs`

**Interfaces:**
- Consumes: `ShardCandidate` / `rank_shard_candidates` (Task 8).
- Produces:
  - `rclone::Entry` gains `pub mod_time: String` (deserialized from `ModTime`, RFC3339).
  - `pub(crate) fn RemoteSync::list_shard_candidates(&self) -> Vec<ShardCandidate>`
  - `pub(crate) fn RemoteSync::snapshots_root_fs(&self) -> String`

rclone's `operations/list` returns `ModTime` as an RFC3339 string. Parse it with `time::OffsetDateTime::parse` (already a dependency — confirm with `rg -n '^time' crates/luchta-cache/Cargo.toml`; if absent, sort by key name instead and skip the parse, since keys are chronological).

- [ ] **Step 1: Write the failing test**

Add to the test module in `remote.rs`:

```rust
    #[test]
    fn list_shard_candidates_returns_remote_shard_dirs() {
        let remote_root = tempfile::tempdir().unwrap();
        let snapshots = remote_root.path().join("snapshots");
        std::fs::create_dir_all(snapshots.join("0000000000001-aaaa")).unwrap();
        std::fs::create_dir_all(snapshots.join("0000000000002-bbbb")).unwrap();
        // A stray file at the shard-dir level must be ignored.
        std::fs::write(snapshots.join("stray.txt"), b"x").unwrap();

        let remote = RemoteSync::new(
            Arc::new(RcloneRcd::with_default_timeout().unwrap()),
            format!(":local:{}", remote_root.path().display()),
            8,
        );

        let mut keys: Vec<String> = remote
            .list_shard_candidates()
            .into_iter()
            .map(|candidate| candidate.key)
            .collect();
        keys.sort();

        assert_eq!(keys, vec!["0000000000001-aaaa", "0000000000002-bbbb"]);
    }
```

`RemoteSync::new(rclone, remote_base_fs, timeout_disable_threshold)` is at `remote.rs:142`; `remote.rs:903` shows the same `:local:` construction pattern.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p luchta-cache remote::tests::list_shard_candidates`
Expected: compile error — no method `list_shard_candidates`.

- [ ] **Step 3: Add `ModTime` to `rclone::Entry`**

```rust
pub struct Entry {
    #[serde(rename = "Path")]
    pub path: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "IsDir")]
    pub is_dir: bool,
    #[serde(rename = "Size")]
    pub size: i64,
    /// RFC3339. Absent or unparseable on some backends — callers fall back to
    /// key ordering, which is chronological because keys are timestamps.
    #[serde(rename = "ModTime", default)]
    pub mod_time: String,
}
```

Fix any `Entry { .. }` literal in tests that now misses the field.

- [ ] **Step 4: Implement `list_shard_candidates`**

In `remote.rs`:

```rust
    pub(crate) fn snapshots_root_fs(&self) -> String {
        format!(
            "{}/{SNAPSHOTS_DIR_NAME}",
            self.remote_base_fs.trim_end_matches('/')
        )
    }

    /// List shard directories on the remote with their modification times.
    ///
    /// Returns an empty list on any error — discovery then falls back to
    /// whatever is already in the local cache.
    pub(crate) fn list_shard_candidates(&self) -> Vec<ShardCandidate> {
        if self.is_disabled() {
            return Vec::new();
        }
        let entries = match self
            .rclone
            .list(&self.snapshots_root_fs(), "", self.rclone.default_timeout())
        {
            Ok(entries) => {
                self.record_remote_success();
                entries
            }
            Err(err) => {
                self.record_remote_error(&err);
                eprintln!("debug: remote snapshot listing failed: {err}");
                return Vec::new();
            }
        };

        entries
            .into_iter()
            .filter(|entry| entry.is_dir)
            .map(|entry| ShardCandidate {
                modified_unix_ms: parse_mod_time_unix_ms(&entry.mod_time)
                    .unwrap_or_else(|| shard_key_unix_ms(&entry.name).unwrap_or(0)),
                key: entry.name,
            })
            .collect()
    }
```

And the two helpers, as free functions in `remote.rs`:

```rust
fn parse_mod_time_unix_ms(mod_time: &str) -> Option<u64> {
    if mod_time.is_empty() {
        return None;
    }
    time::OffsetDateTime::parse(mod_time, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|parsed| (parsed.unix_timestamp_nanos() / 1_000_000) as u64)
}

/// Recover the timestamp from a `<unix_ms>-<nonce>` shard key.
fn shard_key_unix_ms(key: &str) -> Option<u64> {
    key.split_once('-')?.0.parse().ok()
}
```

If `time` is not a dependency of `luchta-cache`, drop `parse_mod_time_unix_ms` and use `shard_key_unix_ms(&entry.name).unwrap_or(0)` alone. Adjust the test accordingly and note it in the commit message.

- [ ] **Step 5: Merge remote candidates into discovery**

Task 8 already reshaped this area, so the code below reflects what is actually on disk now — not the
original plan sketch. `SharedCache` has a `history_len` field; `candidate_keys()` is a method that
discovers local shards and always injects the write key; `build_index` calls `self.candidate_keys()`
and passes the result to `pull_candidate_commits(remote, &keys)`; `MergedIndex` is a `HashSet<String>`
and has no `snapshots` field. Do not reintroduce any of the removed structure.

Give `candidate_keys` an optional remote and union the remote listing into the local one before ranking:

```rust
    /// Discovers the candidate shard keys for this cache, newest-first.
    ///
    /// (keep the existing doc comment about OnceLock-once-per-process and the
    /// load-bearing write-key injection — extend it with the remote behavior)
    #[must_use]
    pub fn candidate_keys(&self) -> Vec<String> {
        self.candidate_keys_with_remote(
            #[cfg(unix)]
            self.remote.as_ref(),
        )
    }

    fn candidate_keys_with_remote(
        &self,
        #[cfg(unix)] remote: Option<&RemoteSync>,
    ) -> Vec<String> {
        let mut candidates = local_shard_candidates_for(&self.paths);

        // Remote-only shards are the whole point of #277: a shard written by
        // another machine has no local directory to discover.
        #[cfg(unix)]
        if let Some(remote) = remote {
            let known: std::collections::HashSet<&str> =
                candidates.iter().map(|c| c.key.as_str()).collect();
            let extra: Vec<ShardCandidate> = remote
                .list_shard_candidates()
                .into_iter()
                .filter(|c| !known.contains(c.key.as_str()))
                .collect();
            candidates.extend(extra);
        }

        let now_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0);
        let mut keys = rank_shard_candidates(
            candidates,
            self.history_len,
            Some(DEFAULT_SHARD_MAX_AGE_MS),
            now_unix_ms,
        );

        if let Some(write_key) = &self.write_commit_key {
            if !keys.iter().any(|key| key == write_key) {
                keys.insert(0, write_key.clone());
            }
        }
        keys
    }
```

Make `local_shard_candidates` public in `discovery.rs` as
`pub fn local_shard_candidates_for(paths: &SharedCachePaths) -> Vec<ShardCandidate>`, and export
`ShardCandidate` and `DEFAULT_SHARD_MAX_AGE_MS` from `shared/mod.rs` if they are not already.

`build_index` needs no change: it already calls `self.candidate_keys()`. Confirm that before editing it.

The write-key injection must stay after ranking, exactly as it is today — the fresh session key would
otherwise be filtered or truncated, and it is load-bearing for
`remote_unreachable_trips_disable_flag_and_build_continues`.

- [ ] **Step 6: Add a test that a remote-only shard is discovered**

A shard present on the remote but absent locally must appear in `candidate_keys`. This is the behavior
#277 exists to deliver and nothing else in the suite covers it.

```rust
    #[test]
    fn candidate_keys_include_remote_only_shards() {
        let remote_root = tempfile::tempdir().unwrap();
        let snapshots = remote_root.path().join("snapshots");
        std::fs::create_dir_all(snapshots.join("0000000000001-aaaa")).unwrap();

        let repo = tempfile::tempdir().unwrap();
        crate::shared::tests::setup_git_repo(repo.path());
        crate::shared::tests::create_commit(repo.path());
        let local_cache = tempfile::tempdir().unwrap();

        let remote = RemoteSync::new(
            Arc::new(RcloneRcd::with_default_timeout().unwrap()),
            format!(":local:{}", remote_root.path().display()),
            8,
        );
        let cache = open_cache_with_remote(repo.path(), local_cache.path(), &remote);

        assert!(
            cache
                .candidate_keys()
                .iter()
                .any(|key| key == "0000000000001-aaaa"),
            "a shard that exists only on the remote must still be a candidate"
        );
    }
```

Gate it with `should_run_rclone_test()` the same way the neighbouring real-rclone tests do.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo nextest run -p luchta-cache entry_meta:: paths::`
Expected: PASS, 5 entry_meta tests plus the paths tests.

- [ ] **Step 7: Commit**

```bash
git add crates/luchta-cache/src/shared/entry_meta.rs crates/luchta-cache/src/shared/paths.rs crates/luchta-cache/src/shared/mod.rs
git commit -m "add per-input_key entry meta object to shared cache"
```

---

### Task 2: Outputs-only blob write

**Files:**
- Modify: `crates/luchta-cache/src/shared/blob.rs:227-292` (`write_blob` — already outputs-only, needs the size-cap parameter split out)
- Test: inline tests in `blob.rs`

**Interfaces:**
- Consumes: `BlobWriteResult` from `blob.rs:20-26`, `SharedCachePaths`.
- Produces: `pub fn write_outputs_blob(paths: &SharedCachePaths, outputs_hash: &[u8; 32], package_dir: &Path, rel_output_paths: &[PathBuf], size_cap_bytes: u64) -> io::Result<BlobWriteResult>` — identical semantics to the existing `write_blob`, but returns `BlobWriteResult::NoOutputs` when there is nothing to archive **and writes no file**.

`write_blob` already does exactly this. This task is a rename plus making `NoOutputs` reachable from the caller, which it currently is not (`shared/mod.rs:703` is dead).

- [ ] **Step 1: Write the failing test**

Add to the existing `#[cfg(test)] mod tests` in `blob.rs` (the one containing `write_blob_returns_no_outputs_when_list_empty_or_missing`):

```rust
    #[test]
    fn write_outputs_blob_creates_no_file_when_there_are_no_outputs() {
        let temp = TempDir::new().unwrap();
        let paths = crate::shared::paths::open_shared_paths(temp.path()).unwrap();
        let package_dir = temp.path().join("pkg");
        std::fs::create_dir_all(&package_dir).unwrap();

        let outputs_hash = crate::resolve::combined_outputs_hash(&[]);
        let result =
            write_outputs_blob(&paths, &outputs_hash, &package_dir, &[], 1_024).unwrap();

        assert_eq!(result, BlobWriteResult::NoOutputs);
        assert!(
            !blob_path(&paths, &outputs_hash).exists(),
            "no blob file should be created for an empty output set"
        );
    }

    #[test]
    fn write_outputs_blob_omits_meta_dir_from_archive() {
        let temp = TempDir::new().unwrap();
        let paths = crate::shared::paths::open_shared_paths(temp.path()).unwrap();
        let package_dir = temp.path().join("pkg");
        std::fs::create_dir_all(package_dir.join("dist")).unwrap();
        std::fs::write(package_dir.join("dist/main.js"), "console.log(1);").unwrap();

        let outputs_hash = [21_u8; 32];
        let result = write_outputs_blob(
            &paths,
            &outputs_hash,
            &package_dir,
            &[PathBuf::from("dist/main.js")],
            1_000_000,
        )
        .unwrap();
        assert_eq!(result, BlobWriteResult::Written);

        let entries = list_entries(&blob_path(&paths, &outputs_hash)).unwrap();
        assert_eq!(entries, vec![PathBuf::from("dist/main.js")]);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p luchta-cache blob::tests::write_outputs_blob`
Expected: compile error — `write_outputs_blob` not found.

- [ ] **Step 3: Rename `write_blob` to `write_outputs_blob`**

In `crates/luchta-cache/src/shared/blob.rs`, change the signature at line 227:

```rust
/// Write an outputs-only blob.
///
/// The archive contains nothing but the task's output files. Per-task meta
/// lives in `entries/<input_key>` — see `shared/entry_meta.rs` and issue #278.
pub fn write_outputs_blob(
    paths: &SharedCachePaths,
    outputs_hash: &[u8; 32],
    package_dir: &Path,
    rel_output_paths: &[PathBuf],
    size_cap_bytes: u64,
) -> io::Result<BlobWriteResult> {
```

The body is unchanged. Update the existing call sites and tests that reference `write_blob`:

```bash
rg -n "write_blob\b" crates/
```

Rename each to `write_outputs_blob`. Leave `write_blob_with_meta` alone for now — Task 3 removes its use.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p luchta-cache blob::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/luchta-cache/src/shared/blob.rs
git commit -m "rename write_blob to write_outputs_blob"
```

---

### Task 3: Store meta and outputs as separate objects

**Files:**
- Modify: `crates/luchta-cache/src/shared/mod.rs:600-706` (`store` and `finish_store`)
- Test: inline tests in `shared/mod.rs`

**Interfaces:**
- Consumes: `write_outputs_blob` (Task 2), `write_entry_meta` / `EntryMeta` / `EntryReport` (Task 1).
- Produces: `SharedCache::store` keeps its existing signature and `StoreOutcome` values. `finish_store` gains an `input_key` parameter: `fn finish_store(&self, blob_result: BlobWriteResult, write_key: &str, input_key: &[u8; 32], entry: SnapshotEntry) -> io::Result<StoreOutcome>`.

**Size cap:** today meta bytes count toward `size_cap_bytes` alongside outputs (`blob.rs:748-755`). Preserve that — sum the encoded meta length with the output bytes and return `StoreOutcome::SkippedTooLarge` if the total exceeds the cap.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` in `crates/luchta-cache/src/shared/mod.rs`:

```rust
    #[test]
    fn two_no_output_tasks_keep_separate_meta() {
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());

        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            10,
            Some(temp_cache.path()),
        )
        .unwrap();

        let empty_hash = crate::resolve::combined_outputs_hash(&[]);

        let pkg_a = temp_repo.path().join("pkg-a");
        let pkg_b = temp_repo.path().join("pkg-b");
        fs::create_dir_all(&pkg_a).unwrap();
        fs::create_dir_all(&pkg_b).unwrap();

        let mut record_a = sample_record(true, 200);
        record_a.output_patterns = vec![];
        record_a.outputs = vec![];
        record_a.outputs_hash = empty_hash;
        record_a.task_spec_hash = [11; 32];
        let key_a = derive_input_key([11; 32], [2; 32], [3; 32], [4; 32]);

        let mut record_b = record_a.clone();
        record_b.task_spec_hash = [22; 32];
        let key_b = derive_input_key([22; 32], [2; 32], [3; 32], [4; 32]);

        cache
            .store("pkg-a#lint", &key_a, &empty_hash, &pkg_a, &[], &record_a,
                   b"A stdout", b"", &[], temp_repo.path())
            .unwrap();
        cache
            .store("pkg-b#lint", &key_b, &empty_hash, &pkg_b, &[], &record_b,
                   b"B stdout", b"", &[], temp_repo.path())
            .unwrap();

        let meta_a = read_entry_meta(cache.paths(), &key_a).expect("meta for A");
        let meta_b = read_entry_meta(cache.paths(), &key_b).expect("meta for B");

        assert_eq!(meta_a.stdout, b"A stdout");
        assert_eq!(meta_b.stdout, b"B stdout");

        assert!(
            !blob_path(cache.paths(), &empty_hash).exists(),
            "no outputs means no blob file"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p luchta-cache shared::tests::two_no_output_tasks_keep_separate_meta`
Expected: FAIL — `read_entry_meta` returns `None` because `store` still writes meta into the blob.

- [ ] **Step 3: Rewrite the store tail**

In `crates/luchta-cache/src/shared/mod.rs`, replace the block from `// Prepare meta files.` through the `self.finish_store(...)` call at the end of `store` with:

```rust
        // Prepare per-entry meta. Keyed by input_key, never by outputs_hash.
        let meta_record =
            bincode::serde::encode_to_vec(record, bincode_config()).map_err(io::Error::other)?;

        let meta = EntryMeta {
            schema_version: ENTRY_META_SCHEMA_VERSION,
            outputs_hash: *outputs_hash,
            record: meta_record,
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
            reports: reports.iter().map(EntryReport::from).collect(),
        };

        // Meta counts toward the size cap, same as when it lived in the blob.
        let meta_bytes = encode_entry_meta(&meta)?.len() as u64;
        let output_bytes: u64 = record.outputs.iter().map(|file| file.size).sum();
        let total_bytes = output_bytes.saturating_add(meta_bytes);
        if total_bytes > self.size_cap_bytes {
            return Ok(StoreOutcome::SkippedTooLarge { bytes: total_bytes });
        }

        let blob_result = write_outputs_blob(
            &self.paths,
            outputs_hash,
            package_dir,
            rel_output_paths,
            self.size_cap_bytes,
        )?;

        // Don't leave meta pointing at a blob that was never written.
        if let BlobWriteResult::SkippedTooLarge { bytes } = blob_result {
            return Ok(StoreOutcome::SkippedTooLarge { bytes });
        }

        write_entry_meta(&self.paths, input_key, &meta)?;

        let entry = SnapshotEntry {
            task_id: task_id.to_string(),
            input_key: *input_key,
            outputs_hash: *outputs_hash,
            task_spec_hash: record.task_spec_hash,
            env_hash: record.env_hash,
            pkg_dep_hash: record.pkg_dep_hash,
            duration_ms,
            output_bytes,
            cached_at_unix_ms: record.end_unix_ms,
            tool_version: None,
        };
        self.finish_store(blob_result, &write_key, input_key, entry)
    }
```

Then update `finish_store` so `NoOutputs` records a snapshot entry like the other success cases, and so the remote push carries the input key:

```rust
    fn finish_store(
        &self,
        blob_result: BlobWriteResult,
        write_key: &str,
        input_key: &[u8; 32],
        entry: SnapshotEntry,
    ) -> io::Result<StoreOutcome> {
        match blob_result {
            // NoOutputs is a success: the entry meta is what makes it restorable.
            BlobWriteResult::Written
            | BlobWriteResult::AlreadyExists
            | BlobWriteResult::NoOutputs => {
                #[cfg(unix)]
                let outputs_hash = entry.outputs_hash;
                #[cfg(unix)]
                let has_outputs = !matches!(blob_result, BlobWriteResult::NoOutputs);
                let merge = self
                    .snapshot_store
                    .merge_entry_with_outcome(write_key, entry);
                if matches!(merge.result, MergeResult::SkippedLockUnavailable) {
                    return Ok(StoreOutcome::SkippedLockUnavailable);
                }
                #[cfg(unix)]
                self.enqueue_remote_push(write_key, outputs_hash, *input_key, has_outputs, merge);
                Ok(StoreOutcome::Stored)
            }
            BlobWriteResult::SkippedTooLarge { bytes } => {
                Ok(StoreOutcome::SkippedTooLarge { bytes })
            }
        }
    }
```

Add the imports at the top of `shared/mod.rs`:

```rust
use blob::{write_outputs_blob, BlobWriteResult};
use entry_meta::{encode_entry_meta, read_entry_meta, write_entry_meta, EntryMeta, EntryReport, ENTRY_META_SCHEMA_VERSION};
```

`enqueue_remote_push` and `OwnedPushArtifacts` do not yet take `input_key` or `has_outputs` — Task 5 adds them. For now, add the two parameters to `enqueue_remote_push` and ignore them with `let _ = (input_key, has_outputs);` so this task compiles and its test passes in isolation.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p luchta-cache shared::`
Expected: PASS for `two_no_output_tasks_keep_separate_meta`. Several existing restore tests will now FAIL because `stage_entry` still reads meta from the blob — that is expected and Task 4 fixes it. Note which ones fail so you can confirm they recover.

- [ ] **Step 5: Commit**

```bash
git add crates/luchta-cache/src/shared/mod.rs
git commit -m "store shared cache meta per input_key instead of inside the blob"
```

---

### Task 4: Two-phase restore — meta first, outputs second

**Files:**
- Modify: `crates/luchta-cache/src/shared/blob.rs` (add `restore_outputs_staged`)
- Modify: `crates/luchta-cache/src/shared/mod.rs:392-437` (`stage_entry`)
- Test: inline tests in `shared/mod.rs`

**Interfaces:**
- Consumes: `read_entry_meta` (Task 1), `StagedRestore` / `BlobReadResultWithMeta` from `blob.rs:880-918`.
- Produces:
  - `pub fn restore_outputs_staged(paths: &SharedCachePaths, outputs_hash: &[u8; 32], package_dir: &Path) -> io::Result<BlobReadResultWithMeta<StagedRestore>>` — same as `restore_blob_with_meta` but the caller ignores `StagedRestore::meta`.
  - `StagedCandidate` gains `pub fn empty_outputs(outputs_hash: [u8; 32], record: TaskRunRecord, stdout: Vec<u8>, stderr: Vec<u8>, reports: Vec<ReportInput>, package_dir: &Path) -> io::Result<Self>` — a candidate with an empty staging dir, so `commit()` writes nothing and returns an empty path list.

**Back-compat:** blobs written by older clients still have `.luchta-meta/` inside. `restore_outputs_staged` reuses `extract_blob_with_meta_to_staging`, and `move_non_meta_files` already filters `META_DIR_NAME` out on commit, so those blobs restore their outputs correctly and their embedded meta is simply ignored. Snapshot entries with no `entries/` object become unrestorable and degrade to a miss until they age out. Say so in the commit message.

- [ ] **Step 1: Write the failing tests**

Add to `#[cfg(test)] mod tests` in `crates/luchta-cache/src/shared/mod.rs`:

```rust
    #[test]
    fn restore_reads_meta_from_entries_not_from_blob() {
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());

        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            10,
            Some(temp_cache.path()),
        )
        .unwrap();

        let package_dir = temp_repo.path().join("pkg");
        fs::create_dir_all(package_dir.join("dist")).unwrap();
        fs::write(package_dir.join("dist/main.js"), "console.log('hi');").unwrap();

        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);
        let record = sample_record(true, 200);

        cache
            .store("pkg#build", &input_key, &[7; 32], &package_dir,
                   &[PathBuf::from("dist/main.js")], &record,
                   b"stdout output", b"stderr output", &[], temp_repo.path())
            .unwrap();

        let restore_dir = temp_repo.path().join("restore");
        fs::create_dir_all(&restore_dir).unwrap();

        let (hit, written_paths) = cache
            .try_restore_candidates("pkg#build", &input_key, &restore_dir)
            .next()
            .expect("expected at least one candidate")
            .commit()
            .expect("commit should succeed");

        assert_eq!(hit.outputs_hash, [7; 32]);
        assert_eq!(hit.stdout, b"stdout output");
        assert_eq!(hit.stderr, b"stderr output");
        assert_eq!(written_paths, vec![restore_dir.join("dist/main.js")]);
        assert_eq!(
            fs::read(restore_dir.join("dist/main.js")).unwrap(),
            b"console.log('hi');"
        );
        assert!(!restore_dir.join(".luchta-meta").exists());
    }

    #[test]
    fn no_output_task_restores_without_touching_a_blob() {
        let temp_repo = TempDir::new().unwrap();
        setup_git_repo(temp_repo.path());
        create_commit(temp_repo.path());

        let temp_cache = TempDir::new().unwrap();
        let cache = SharedCache::open_with_cache_dir(
            temp_repo.path(),
            1_000_000,
            10,
            Some(temp_cache.path()),
        )
        .unwrap();

        let empty_hash = crate::resolve::combined_outputs_hash(&[]);
        let package_dir = temp_repo.path().join("pkg");
        fs::create_dir_all(&package_dir).unwrap();

        let mut record = sample_record(true, 200);
        record.output_patterns = vec![];
        record.outputs = vec![];
        record.outputs_hash = empty_hash;
        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);

        cache
            .store("pkg#lint", &input_key, &empty_hash, &package_dir, &[], &record,
                   b"lint output", b"", &[], temp_repo.path())
            .unwrap();

        let restore_dir = temp_repo.path().join("restore");
        fs::create_dir_all(&restore_dir).unwrap();

        let (hit, written_paths) = cache
            .try_restore_candidates("pkg#lint", &input_key, &restore_dir)
            .next()
            .expect("expected a candidate for a no-output task")
            .commit()
            .expect("commit should succeed");

        assert_eq!(hit.stdout, b"lint output");
        assert!(written_paths.is_empty(), "nothing to write for a no-output task");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p luchta-cache shared::tests::restore_reads_meta shared::tests::no_output_task_restores`
Expected: FAIL — `stage_entry` still reads the record out of the blob, so the first test panics on `expected at least one candidate` and the second finds no blob at all.

- [ ] **Step 3: Add `restore_outputs_staged`**

In `crates/luchta-cache/src/shared/blob.rs`, next to `restore_blob_with_meta`:

```rust
/// Restore an outputs-only blob into a staging directory.
///
/// Blobs written by older clients still carry a `.luchta-meta/` directory.
/// Its contents are ignored here — `entries/<input_key>` is authoritative —
/// and `move_non_meta_files` filters it out on commit.
pub fn restore_outputs_staged(
    paths: &SharedCachePaths,
    outputs_hash: &[u8; 32],
    package_dir: &Path,
) -> io::Result<BlobReadResultWithMeta<StagedRestore>> {
    restore_blob_with_meta(paths, outputs_hash, package_dir)
}
```

- [ ] **Step 4: Add `StagedCandidate::empty_outputs` and rewrite `stage_entry`**

In `crates/luchta-cache/src/shared/mod.rs`, add to `impl StagedCandidate`:

```rust
    /// A candidate with no output files. Commits to an empty path list.
    pub fn empty_outputs(
        outputs_hash: [u8; 32],
        record: TaskRunRecord,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        reports: Vec<crate::store::ReportInput>,
        package_dir: &Path,
    ) -> std::io::Result<Self> {
        Ok(Self {
            outputs_hash,
            record,
            stdout,
            stderr,
            reports,
            staged: blob::StagedRestore::empty(package_dir)?,
        })
    }
```

And in `blob.rs`, add the matching constructor to `impl StagedRestore`:

```rust
    /// A staged restore holding no files. Commits to an empty path list.
    pub fn empty(package_dir: &Path) -> io::Result<Self> {
        let staging_dir = tempfile::Builder::new()
            .prefix("blob-restore-empty-")
            .tempdir_in(package_dir)?;
        Ok(Self {
            meta: MetaFiles {
                stdout: Vec::new(),
                stderr: Vec::new(),
                record: Vec::new(),
                reports: Vec::new(),
            },
            staging_dir,
            package_dir: package_dir.to_path_buf(),
        })
    }
```

Replace `stage_entry` in `shared/mod.rs` with:

```rust
    /// Stage a single entry, returning a StagedCandidate for validation.
    ///
    /// Two-phase: fetch the small `entries/<input_key>` object first and decode
    /// the record from it. Only pull the outputs blob if the entry actually has
    /// outputs. A candidate rejected by `decide_shared_restore` therefore never
    /// costs an outputs download.
    fn stage_entry(
        entry: &SnapshotEntry,
        paths: &SharedCachePaths,
        package_dir: &Path,
        #[cfg(unix)] remote: Option<&RemoteSync>,
    ) -> Option<StagedCandidate> {
        #[cfg(unix)]
        if read_entry_meta(paths, &entry.input_key).is_none() {
            if let Some(remote) = remote {
                if let Err(err) = remote.pull_entry_meta(paths, &entry.input_key) {
                    eprintln!(
                        "debug: remote entry meta pull failed for input_key={}: {err}",
                        hex_hash(entry.input_key)
                    );
                }
            }
        }

        let meta = read_entry_meta(paths, &entry.input_key)?;

        let record: TaskRunRecord =
            match bincode::serde::decode_from_slice(&meta.record, bincode_config()) {
                Ok((record, _)) => record,
                Err(_) => return None,
            };

        let reports: Vec<crate::store::ReportInput> = meta
            .reports
            .into_iter()
            .map(crate::store::ReportInput::from)
            .collect();

        if record.outputs.is_empty() {
            return StagedCandidate::empty_outputs(
                meta.outputs_hash,
                record,
                meta.stdout,
                meta.stderr,
                reports,
                package_dir,
            )
            .ok();
        }

        if !blob_path(paths, &meta.outputs_hash).is_file() {
            #[cfg(unix)]
            if let Some(remote) = remote {
                if let Err(err) = remote.pull_blob(paths, &meta.outputs_hash) {
                    eprintln!(
                        "debug: remote blob pull failed for outputs_hash={}: {err}",
                        hex_hash(meta.outputs_hash)
                    );
                }
            }
        }

        let staged = match restore_outputs_staged(paths, &meta.outputs_hash, package_dir) {
            Ok(BlobReadResultWithMeta::Restored(staged)) => staged,
            Ok(BlobReadResultWithMeta::Missing) | Ok(BlobReadResultWithMeta::Corrupt) => {
                return None
            }
            Err(_) => return None,
        };

        Some(StagedCandidate {
            outputs_hash: meta.outputs_hash,
            record,
            stdout: meta.stdout,
            stderr: meta.stderr,
            reports,
            staged,
        })
    }
```

`RemoteSync::pull_entry_meta` does not exist until Task 5. To keep this task compiling and testable on its own, add a stub in `remote.rs`:

```rust
    pub(crate) fn pull_entry_meta(
        &self,
        _paths: &SharedCachePaths,
        _input_key: &[u8; 32],
    ) -> Result<(), rclone::RcloneError> {
        Ok(())
    }
```

Update the `restore_outputs_staged` and `read_entry_meta` imports at the top of `shared/mod.rs`.

- [ ] **Step 5: Run the full crate tests**

Run: `cargo nextest run -p luchta-cache`
Expected: PASS. The restore tests that broke in Task 3 recover here. If `store_and_restore_round_trip_byte_identical` still fails, check that `store` writes the entry meta before `finish_store` returns.

- [ ] **Step 6: Commit**

```bash
git add crates/luchta-cache/src/shared/blob.rs crates/luchta-cache/src/shared/mod.rs crates/luchta-cache/src/shared/remote.rs
git commit -m "restore shared cache entries from per-entry meta, then outputs

Meta is read from entries/<input_key> and the outputs blob is only pulled
when the entry has outputs. Blobs written by older clients still restore
their outputs; their embedded .luchta-meta is ignored. Snapshot entries
with no entries/ object degrade to a miss until they age out."
```

---

### Task 5: Remote push and pull for entry meta

**Files:**
- Modify: `crates/luchta-cache/src/shared/remote.rs:220-232` (`entries_fs`), `:339-368` (`push_store_artifacts_owned`), `:370-385` (replace the `pull_entry_meta` stub), `:430-495` (`push_store_artifacts`, `push_blob_if_missing`)
- Modify: `crates/luchta-cache/src/shared/mod.rs` (`enqueue_remote_push` — pass the parameters added in Task 3 for real)
- Test: inline tests in `remote.rs`, following `remote_store_skips_blob_reupload_when_remote_blob_exists` at `remote.rs:1453`

**Interfaces:**
- Consumes: `EntryMeta` encoding via `entry_meta_path` (Task 1).
- Produces:
  - `RemoteSync::pull_entry_meta(&self, paths: &SharedCachePaths, input_key: &[u8; 32]) -> Result<(), rclone::RcloneError>`
  - `OwnedPushArtifacts` and `PushArtifacts` gain `input_key: [u8; 32]` and `has_outputs: bool`
  - `SharedCache::enqueue_remote_push(&self, write_key: &str, outputs_hash: [u8; 32], input_key: [u8; 32], has_outputs: bool, merge: MergeEntryOutcome)`

- [ ] **Step 1: Write the failing test**

Add to `#[cfg(test)] mod tests` in `crates/luchta-cache/src/shared/remote.rs`, modelled on the existing `remote_store_skips_blob_reupload_when_remote_blob_exists`:

```rust
    #[test]
    fn remote_store_uploads_entry_meta_and_second_machine_restores_no_output_task() {
        let remote_root = tempfile::tempdir().unwrap();
        let machine_a_cache = tempfile::tempdir().unwrap();
        let machine_b_cache = tempfile::tempdir().unwrap();

        let repo = tempfile::tempdir().unwrap();
        crate::shared::tests::setup_git_repo(repo.path());
        crate::shared::tests::create_commit(repo.path());

        let empty_hash = crate::resolve::combined_outputs_hash(&[]);
        let package_dir = repo.path().join("pkg");
        std::fs::create_dir_all(&package_dir).unwrap();

        let mut record = crate::shared::tests::sample_record(true, 200);
        record.output_patterns = vec![];
        record.outputs = vec![];
        record.outputs_hash = empty_hash;
        let input_key = derive_input_key([1; 32], [2; 32], [3; 32], [4; 32]);

        // `open_cache_with_remote` lives at remote.rs:677 and takes a &RemoteSync.
        // Build one against a :local: path the same way remote.rs:903 does.
        let seed_remote = RemoteSync::new(
            Arc::new(RcloneRcd::with_default_timeout().unwrap()),
            format!(":local:{}", remote_root.path().display()),
            8,
        );

        let cache_a = open_cache_with_remote(repo.path(), machine_a_cache.path(), &seed_remote);
        cache_a
            .store("pkg#lint", &input_key, &empty_hash, &package_dir, &[], &record,
                   b"lint output", b"", &[], repo.path())
            .unwrap();
        cache_a.flush_push_queue();

        assert!(
            remote_root.path().join("entries").read_dir().unwrap().count() > 0,
            "entry meta should be uploaded"
        );

        let cache_b = open_cache_with_remote(repo.path(), machine_b_cache.path(), remote_root.path());
        let restore_dir = repo.path().join("restore");
        std::fs::create_dir_all(&restore_dir).unwrap();

        let candidate = cache_b
            .try_restore_candidates("pkg#lint", &input_key, &restore_dir)
            .next()
            .expect("machine B should find the entry");
        assert_eq!(candidate.stdout, b"lint output");
    }
```

Reuse whatever helper the neighbouring tests use to build a `SharedCache` with a `:local:` remote; if there isn't one, extract the setup from `remote_store_skips_blob_reupload_when_remote_blob_exists` into `fn open_cache_with_remote(repo_root: &Path, cache_dir: &Path, remote_root: &Path) -> SharedCache` in the test module and use it from both.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p luchta-cache remote::tests::remote_store_uploads_entry_meta`
Expected: FAIL — nothing is uploaded to `entries/`, so the `read_dir` count assertion trips.

- [ ] **Step 3: Implement the remote entry-meta paths**

In `remote.rs`, add next to `blobs_fs`:

```rust
    fn entries_fs(&self) -> String {
        format!(
            "{}/{ENTRIES_DIR_NAME}",
            self.remote_base_fs.trim_end_matches('/')
        )
    }
```

Import `ENTRIES_DIR_NAME` alongside the existing `BLOBS_DIR_NAME` / `SNAPSHOTS_DIR_NAME` imports.

Replace the Task 4 stub with the real pull:

```rust
    pub(crate) fn pull_entry_meta(
        &self,
        paths: &SharedCachePaths,
        input_key: &[u8; 32],
    ) -> Result<(), rclone::RcloneError> {
        if self.is_disabled() {
            return Ok(());
        }
        let local_path = entry_meta_path(paths, input_key);
        if local_path.exists() {
            return Ok(());
        }
        let file_name = format!("{}.bin", hex_hash(*input_key));
        self.copy_remote_file_down(&self.entries_fs(), &file_name, &local_path)
            .inspect(|_| self.record_remote_success())
            .inspect_err(|err| self.record_remote_error(err))
    }
```

Add the push, mirroring `push_blob_if_missing`:

```rust
    fn push_entry_meta_if_missing(&self, paths: &SharedCachePaths, input_key: &[u8; 32]) {
        let remote_fs = self.entries_fs();
        let file_name = format!("{}.bin", hex_hash(*input_key));
        match self
            .rclone
            .stat(&remote_fs, &file_name, self.rclone.default_timeout())
        {
            Ok(Some(_)) => {
                self.record_remote_success();
                return;
            }
            Ok(None) => {
                self.record_remote_success();
            }
            Err(err) => {
                self.record_remote_error(&err);
                eprintln!("warn: shared cache remote entry meta stat failed for {file_name}: {err}");
                return;
            }
        }

        let local_path = entry_meta_path(paths, input_key);
        if let Err(err) = self.copy_local_file_up(&local_path, &remote_fs, &file_name) {
            self.record_remote_error(&err);
            eprintln!("warn: shared cache remote entry meta upload failed for {file_name}: {err}");
        } else {
            self.record_remote_success();
        }
    }
```

Add `input_key: [u8; 32]` and `has_outputs: bool` to both `OwnedPushArtifacts` and `PushArtifacts`, thread them through `push_store_artifacts_owned`, and change the head of `push_store_artifacts` to:

```rust
        let PushArtifacts {
            paths,
            commit_key,
            outputs_hash,
            input_key,
            has_outputs,
            merge,
        } = push;

        if has_outputs {
            self.push_blob_if_missing(paths, outputs_hash);
        }
        self.push_entry_meta_if_missing(paths, input_key);
```

In `shared/mod.rs`, make `enqueue_remote_push` pass them through instead of discarding them:

```rust
    #[cfg(unix)]
    fn enqueue_remote_push(
        &self,
        write_key: &str,
        outputs_hash: [u8; 32],
        input_key: [u8; 32],
        has_outputs: bool,
        merge: MergeEntryOutcome,
    ) {
        let Some(remote) = &self.remote else {
            return;
        };
        if remote.is_disabled() {
            return;
        }
        remote.enqueue_push_store_artifacts(remote::OwnedPushArtifacts {
            paths: Arc::clone(&self.paths),
            commit_key: write_key.to_string(),
            outputs_hash,
            input_key,
            has_outputs,
            merge,
        });
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p luchta-cache`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/luchta-cache/src/shared/remote.rs crates/luchta-cache/src/shared/mod.rs
git commit -m "sync entry meta objects to and from the remote shared cache"
```

---

### Task 6: GC the entries dir, plus the #278 e2e regression test

**Files:**
- Modify: `crates/luchta-cache/src/shared/gc.rs:28-36` (`run_gc`), add `gc_entries_dir`
- Create: `crates/luchta-cli/tests/shared_cache_no_output_e2e.rs`
- Test: inline tests in `gc.rs`; the new e2e file

**Interfaces:**
- Consumes: `SharedCachePaths::entries_dir` (Task 1), the full store/restore path (Tasks 3–5).
- Produces: `GcStats` gains `pub entries_deleted: usize`.

- [ ] **Step 1: Write the failing GC test**

Add to `#[cfg(test)] mod tests` in `gc.rs`:

```rust
    #[test]
    fn run_gc_deletes_old_entry_meta() {
        let temp = TempDir::new().unwrap();
        let paths = crate::shared::paths::open_shared_paths(temp.path()).unwrap();
        let input_key = [12_u8; 32];

        let meta = crate::shared::EntryMeta {
            schema_version: crate::shared::ENTRY_META_SCHEMA_VERSION,
            outputs_hash: [1; 32],
            has_outputs: false,
            record: vec![0],
            stdout: Vec::new(),
            stderr: Vec::new(),
            reports: Vec::new(),
        };
        crate::shared::write_entry_meta(&paths, &input_key, &meta).unwrap();

        let path = crate::shared::entry_meta_path(&paths, &input_key);
        set_mtime(&path, SystemTime::now() - Duration::from_secs(60 * 60 * 24 * 30));

        let stats = run_gc(&paths, Duration::from_secs(60 * 60 * 24 * 7));

        assert_eq!(stats.entries_deleted, 1);
        assert!(!path.exists());
    }

    #[test]
    fn run_gc_keeps_recent_entry_meta() {
        let temp = TempDir::new().unwrap();
        let paths = crate::shared::paths::open_shared_paths(temp.path()).unwrap();
        let input_key = [13_u8; 32];

        let meta = crate::shared::EntryMeta {
            schema_version: crate::shared::ENTRY_META_SCHEMA_VERSION,
            outputs_hash: [1; 32],
            has_outputs: false,
            record: vec![0],
            stdout: Vec::new(),
            stderr: Vec::new(),
            reports: Vec::new(),
        };
        crate::shared::write_entry_meta(&paths, &input_key, &meta).unwrap();

        let stats = run_gc(&paths, Duration::from_secs(60 * 60 * 24 * 7));

        assert_eq!(stats.entries_deleted, 0);
        assert!(crate::shared::entry_meta_path(&paths, &input_key).exists());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p luchta-cache gc::tests::run_gc_deletes_old_entry_meta gc::tests::run_gc_keeps_recent_entry_meta`
Expected: compile error — `GcStats` has no field `entries_deleted`.

- [ ] **Step 3: Implement `gc_entries_dir`**

Add `pub entries_deleted: usize` to `GcStats`, call the new function from `run_gc`:

```rust
pub fn run_gc(paths: &SharedCachePaths, retention: Duration) -> GcStats {
    let now = SystemTime::now();
    let mut stats = GcStats::default();

    gc_snapshot_dir(paths, retention, now, &mut stats);
    gc_blob_dir(paths, retention, now, &mut stats);
    gc_entries_dir(paths, retention, now, &mut stats);

    stats
}
```

And add, modelled on `gc_blob_dir`:

```rust
const ENTRY_META_SUFFIX: &str = ".bin";

fn gc_entries_dir(
    paths: &SharedCachePaths,
    retention: Duration,
    now: SystemTime,
    stats: &mut GcStats,
) {
    let entries = match fs::read_dir(&paths.entries_dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !has_file_name_suffix(&path, ENTRY_META_SUFFIX) {
            continue;
        }
        if !is_older_than(&path, retention, now) {
            continue;
        }

        // Age-based, like blobs. A reader that loses the race treats the
        // missing meta as a cache miss and reruns the task.
        let meta_bytes = file_len(&path);
        if remove_file_if_exists(&path) {
            stats.entries_deleted += 1;
            stats.bytes_freed = stats.bytes_freed.saturating_add(meta_bytes);
        }
    }
}
```

- [ ] **Step 4: Run the GC tests to verify they pass**

Run: `cargo nextest run -p luchta-cache gc::`
Expected: PASS.

- [ ] **Step 5: Write the e2e regression test for #278**

Create `crates/luchta-cli/tests/shared_cache_no_output_e2e.rs`:

```rust
mod common;

use assert_cmd::Command;
use assert_fs::prelude::*;
use common::{init_git, write_counter_task_config, write_root_workspace};

fn run(temp: &assert_fs::TempDir, cache_dir: &std::path::Path, task: &str) -> String {
    let out = Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg(task)
        .arg("--workspace-root")
        .arg(temp.path())
        .env("LUCHTA_SHARED_CACHE", "1")
        .env("LUCHTA_SHARED_CACHE_DIR", cache_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out).unwrap()
}

/// Regression for #278: every task with no declared outputs used to map to the
/// single blob named by `combined_outputs_hash(&[])`, so only the first one to
/// store could ever be restored.
#[test]
fn two_no_output_tasks_both_hit_the_shared_cache() {
    let shared_cache_dir = tempfile::tempdir().unwrap();
    let temp = assert_fs::TempDir::new().unwrap();
    write_root_workspace(&temp);

    write_counter_task_config(
        &temp,
        r#""app#lint":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":[],"command":"sleep 0.15 && count=$(cat ../../lint-counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > ../../lint-counter.txt"},"app#test":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":[],"command":"sleep 0.15 && count=$(cat ../../test-counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > ../../test-counter.txt"}}"#,
    );

    temp.child("packages/app/src.txt").write_str("source\n").unwrap();
    temp.child("packages/app/package.json")
        .write_str(
            r#"{
  "name": "app",
  "scripts": {
    "lint": "echo ignored",
    "test": "echo ignored"
  }
}"#,
        )
        .unwrap();
    init_git(&temp);

    run(&temp, shared_cache_dir.path(), "lint");
    run(&temp, shared_cache_dir.path(), "test");

    // Drop the local cache so the second pass has to come from the shared cache.
    std::fs::remove_dir_all(temp.path().join(".luchta/cache")).unwrap();

    let second_lint = run(&temp, shared_cache_dir.path(), "lint");
    let second_test = run(&temp, shared_cache_dir.path(), "test");

    assert!(second_lint.contains("📥 1"), "lint should be a shared hit, got:\n{second_lint}");
    assert!(second_test.contains("📥 1"), "test should be a shared hit, got:\n{second_test}");

    // Neither command body ran a second time.
    assert_eq!(
        std::fs::read_to_string(temp.path().join("lint-counter.txt")).unwrap(),
        "1\n"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("test-counter.txt")).unwrap(),
        "1\n"
    );
}
```

- [ ] **Step 6: Run the e2e test to verify it passes**

Run: `cargo nextest run -p luchta-cli --test shared_cache_no_output_e2e`
Expected: PASS. Before Phase 1 this test fails with `test should be a shared hit` and `test-counter.txt` reading `2\n`.

- [ ] **Step 7: Run the full workspace suite**

Run: `cargo nextest run --workspace`
Expected: PASS. Fix any shared-cache test that asserted meta lives inside the blob — those assertions are now wrong and should assert against `entries/` instead.

- [ ] **Step 8: Commit**

```bash
git add crates/luchta-cache/src/shared/gc.rs crates/luchta-cli/tests/shared_cache_no_output_e2e.rs
git commit -m "gc entry meta objects and cover the no-output collision e2e

Closes #278."
```

**Phase 1 is shippable here.**

---

## Phase 2 — Recency-based shard discovery (#277)

### Task 7: Session shard ids replace commit keys

**Files:**
- Create: `crates/luchta-cache/src/shared/discovery.rs`
- Delete: `crates/luchta-cache/src/shared/git.rs`
- Modify: `crates/luchta-cache/src/shared/mod.rs:19` (re-exports), `:257-263` and `:303-309` (construction)
- Modify: `crates/luchta-cache/src/shared/snapshot.rs` — rename the `commit_key` parameter to `shard_key` throughout. Behavior unchanged.
- Test: inline tests in `discovery.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub fn new_session_shard_key(now_unix_ms: u64, nonce: u64) -> String` — returns `"{now_unix_ms:013}-{nonce:08x}"`. Zero-padded so lexical order matches chronological order.
  - `pub fn current_session_shard_key() -> String` — wraps the above using the wall clock and a process-random nonce.

The zero-padded millisecond prefix means shard dirs sort chronologically by name, which gives a working fallback when a remote does not report ModTime.

`gix` may now be unused in `luchta-cache`. Check with `cargo build -p luchta-cache` and drop the dependency from `crates/luchta-cache/Cargo.toml` if the compiler says it is unused.

- [ ] **Step 1: Write the failing test**

Create `crates/luchta-cache/src/shared/discovery.rs` with just this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_shard_keys_sort_chronologically_by_name() {
        let earlier = new_session_shard_key(1_754_431_200_123, 0x0000_00ff);
        let later = new_session_shard_key(1_754_431_200_456, 0x0000_0001);
        assert!(earlier < later, "{earlier} should sort before {later}");
    }

    #[test]
    fn session_shard_key_has_stable_shape() {
        assert_eq!(
            new_session_shard_key(1_754_431_200_123, 0xdead_beef),
            "1754431200123-deadbeef"
        );
    }

    #[test]
    fn session_shard_keys_differ_for_the_same_millisecond() {
        let first = new_session_shard_key(1_754_431_200_123, 1);
        let second = new_session_shard_key(1_754_431_200_123, 2);
        assert_ne!(first, second);
    }

    #[test]
    fn current_session_shard_key_is_unique_across_calls() {
        let first = current_session_shard_key();
        let second = current_session_shard_key();
        assert_ne!(first, second);
    }
}
```

Register it in `shared/mod.rs`:

```rust
mod discovery;
pub use discovery::{current_session_shard_key, new_session_shard_key};
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p luchta-cache discovery::`
Expected: compile error — `new_session_shard_key` and `current_session_shard_key` not found.

- [ ] **Step 3: Implement the shard key generator**

Prepend to `discovery.rs`:

```rust
//! Shard discovery for the shared cache.
//!
//! Shards used to be named by git commit id and discovered by walking
//! first-parent ancestry from HEAD. That fails whenever builds run on commits
//! no other build will ever see — feature branches, and especially Prow's
//! temporary merged-with-master commits. See issue #277.
//!
//! Shards are now named `<unix_ms>-<nonce>` and discovered by recency.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static SHARD_NONCE: AtomicU64 = AtomicU64::new(0);

/// Build a shard key from an explicit timestamp and nonce.
///
/// The millisecond field is zero-padded to 13 digits so lexical ordering
/// matches chronological ordering — the fallback when a remote listing does
/// not report modification times.
#[must_use]
pub fn new_session_shard_key(now_unix_ms: u64, nonce: u64) -> String {
    format!("{now_unix_ms:013}-{:08x}", nonce & 0xffff_ffff)
}

/// Build a shard key for this process's current run.
#[must_use]
pub fn current_session_shard_key() -> String {
    let now_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0);
    let nonce = SHARD_NONCE.fetch_add(1, Ordering::Relaxed) ^ (std::process::id() as u64) << 16;
    new_session_shard_key(now_unix_ms, nonce)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p luchta-cache discovery::`
Expected: PASS.

- [ ] **Step 5: Switch `SharedCache` construction off commit keys**

In `shared/mod.rs`, in both `open_with_remote` and `from_parts_for_test`, replace the commit-key block with:

```rust
        let write_commit_key = Some(current_session_shard_key());
        let candidate_keys = discover_recent_shard_keys(&paths, history_len);
```

`discover_recent_shard_keys` arrives in Task 8. For this task, stub it in `discovery.rs` so the crate compiles:

```rust
/// Placeholder until Task 8 implements local recency discovery.
pub fn discover_recent_shard_keys(_paths: &SharedCachePaths, _limit: usize) -> Vec<String> {
    Vec::new()
}
```

Both functions take `repo_root: &Path` and no longer use it. Keep the parameter — the CLI passes it and Task 9 does not need it back — and silence the warning with `let _ = repo_root;`.

Delete `crates/luchta-cache/src/shared/git.rs` and remove `mod git;` plus the `pub use git::{...}` line from `shared/mod.rs`. Fix the resulting test failures: `shared/mod.rs:1313` asserts `cache.candidate_keys().contains(&commit1)` — delete that assertion, it is testing the behavior being removed.

- [ ] **Step 6: Run the crate tests**

Run: `cargo nextest run -p luchta-cache`
Expected: PASS for unit tests. Shared-cache restore tests that relied on commit-key discovery will fail because `discover_recent_shard_keys` returns nothing — that is expected and Task 8 fixes it. Record which ones fail.

- [ ] **Step 7: Commit**

```bash
git add crates/luchta-cache/src/shared/discovery.rs crates/luchta-cache/src/shared/mod.rs crates/luchta-cache/src/shared/snapshot.rs
git rm crates/luchta-cache/src/shared/git.rs
git commit -m "name shared cache shards by session id instead of git commit"
```

---

### Task 8: Local recency discovery

**Files:**
- Modify: `crates/luchta-cache/src/shared/discovery.rs` (replace the stub)
- Modify: `crates/luchta-cli/tests/shared_cache_e2e.rs` (rewrite two obsolete commit-key tests — see Step 5)
- Test: inline tests in `discovery.rs`; rewritten e2e tests in `shared_cache_e2e.rs`

**Interfaces:**
- Consumes: `SharedCachePaths::snapshots_dir`, `new_session_shard_key` (Task 7).
- Produces:
  - `pub struct ShardCandidate { pub key: String, pub modified_unix_ms: u64 }`
  - `pub fn rank_shard_candidates(candidates: Vec<ShardCandidate>, limit: usize, max_age_ms: Option<u64>, now_unix_ms: u64) -> Vec<String>` — pure, newest-first, at most `limit`, dropping anything older than `max_age_ms`.
  - `pub fn discover_recent_shard_keys(paths: &SharedCachePaths, limit: usize) -> Vec<String>` — lists `snapshots_dir`, builds `ShardCandidate`s from directory mtimes, and ranks them.
  - `pub const DEFAULT_SHARD_MAX_AGE_MS: u64 = 1000 * 60 * 60 * 24 * 14;` (14 days)

Keeping the ranking pure makes it testable without a filesystem and lets Task 9 feed it remote listings through the same function.

- [ ] **Step 1: Write the failing tests**

Add to the test module in `discovery.rs`:

```rust
    fn candidate(key: &str, modified_unix_ms: u64) -> ShardCandidate {
        ShardCandidate {
            key: key.to_string(),
            modified_unix_ms,
        }
    }

    const NOW: u64 = 1_754_431_200_000;

    #[test]
    fn shard_key_zero_pads_short_timestamps_so_lexical_order_stays_chronological() {
        // The 13-digit pad is the ordering fallback for remotes that don't report
        // ModTime. Task 7's tests all used already-13-digit values, so padding never
        // actually fired — this covers the case where it does.
        let short = new_session_shard_key(5, 0);
        let full = new_session_shard_key(1_754_431_200_123, 0);
        assert_eq!(short, "0000000000005-00000000");
        assert!(short < full, "{short} must sort before {full}");
    }

    #[test]
    fn rank_returns_newest_first() {
        let ranked = rank_shard_candidates(
            vec![
                candidate("a", NOW - 3_000),
                candidate("b", NOW - 1_000),
                candidate("c", NOW - 2_000),
            ],
            10,
            None,
            NOW,
        );
        assert_eq!(ranked, vec!["b", "c", "a"]);
    }

    #[test]
    fn rank_applies_the_limit() {
        let ranked = rank_shard_candidates(
            vec![
                candidate("a", NOW - 3_000),
                candidate("b", NOW - 1_000),
                candidate("c", NOW - 2_000),
            ],
            2,
            None,
            NOW,
        );
        assert_eq!(ranked, vec!["b", "c"]);
    }

    #[test]
    fn rank_drops_shards_older_than_max_age() {
        let ranked = rank_shard_candidates(
            vec![
                candidate("fresh", NOW - 1_000),
                candidate("stale", NOW - 100_000),
            ],
            10,
            Some(10_000),
            NOW,
        );
        assert_eq!(ranked, vec!["fresh"]);
    }

    #[test]
    fn rank_breaks_mtime_ties_by_key_descending() {
        let ranked = rank_shard_candidates(
            vec![candidate("0000000000001-aaaa", NOW), candidate("0000000000001-bbbb", NOW)],
            10,
            None,
            NOW,
        );
        assert_eq!(ranked, vec!["0000000000001-bbbb", "0000000000001-aaaa"]);
    }

    #[test]
    fn discover_finds_local_shard_dirs_newest_first() {
        use std::time::{Duration, SystemTime};
        let temp = tempfile::TempDir::new().unwrap();
        let paths = crate::shared::paths::open_shared_paths(temp.path()).unwrap();

        for key in ["0000000000001-aaaa", "0000000000002-bbbb", "0000000000003-cccc"] {
            std::fs::create_dir_all(paths.snapshots_dir.join(key)).unwrap();
        }

        // Make "0000000000001-aaaa" the newest by mtime to prove mtime wins over name.
        let newest = paths.snapshots_dir.join("0000000000001-aaaa");
        filetime::set_file_mtime(
            &newest,
            filetime::FileTime::from_system_time(SystemTime::now() + Duration::from_secs(60)),
        )
        .unwrap();

        let discovered = discover_recent_shard_keys(&paths, 10);
        assert_eq!(discovered.first().map(String::as_str), Some("0000000000001-aaaa"));
        assert_eq!(discovered.len(), 3);
    }

    #[test]
    fn discover_returns_empty_when_snapshots_dir_is_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        let paths = SharedCachePaths {
            root: temp.path().to_path_buf(),
            blobs_dir: temp.path().join("blobs"),
            snapshots_dir: temp.path().join("does-not-exist"),
            entries_dir: temp.path().join("entries"),
        };
        assert!(discover_recent_shard_keys(&paths, 10).is_empty());
    }
```

`filetime` is already a dev-dependency of this crate (`gc.rs` tests use `set_mtime`). If `cargo nextest` reports it missing, add `filetime` to `[dev-dependencies]` in `crates/luchta-cache/Cargo.toml`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p luchta-cache discovery::`
Expected: compile error — `ShardCandidate` and `rank_shard_candidates` not found; `discover_recent_shard_keys` returns an empty vec so the discovery tests fail too.

- [ ] **Step 3: Implement discovery**

Replace the stub in `discovery.rs`:

```rust
use std::fs;
use std::path::Path;

use super::SharedCachePaths;

/// Drop shards older than two weeks. Long enough to span a quiet weekend,
/// short enough that the merged index stays small.
pub const DEFAULT_SHARD_MAX_AGE_MS: u64 = 1000 * 60 * 60 * 24 * 14;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardCandidate {
    pub key: String,
    pub modified_unix_ms: u64,
}

/// Rank candidates newest-first, capped at `limit` and filtered by `max_age_ms`.
///
/// Ties on modification time fall back to key order descending, which is
/// chronological because keys are zero-padded millisecond timestamps.
#[must_use]
pub fn rank_shard_candidates(
    candidates: Vec<ShardCandidate>,
    limit: usize,
    max_age_ms: Option<u64>,
    now_unix_ms: u64,
) -> Vec<String> {
    let mut kept: Vec<ShardCandidate> = candidates
        .into_iter()
        .filter(|candidate| match max_age_ms {
            Some(max_age_ms) => {
                now_unix_ms.saturating_sub(candidate.modified_unix_ms) <= max_age_ms
            }
            None => true,
        })
        .collect();

    kept.sort_unstable_by(|left, right| {
        right
            .modified_unix_ms
            .cmp(&left.modified_unix_ms)
            .then_with(|| right.key.cmp(&left.key))
    });
    kept.truncate(limit);
    kept.into_iter().map(|candidate| candidate.key).collect()
}

/// Discover shard keys present in the local cache, newest-first.
#[must_use]
pub fn discover_recent_shard_keys(paths: &SharedCachePaths, limit: usize) -> Vec<String> {
    let candidates = local_shard_candidates(&paths.snapshots_dir);
    let now_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0);
    rank_shard_candidates(
        candidates,
        limit,
        Some(DEFAULT_SHARD_MAX_AGE_MS),
        now_unix_ms,
    )
}

fn local_shard_candidates(snapshots_dir: &Path) -> Vec<ShardCandidate> {
    let Ok(entries) = fs::read_dir(snapshots_dir) else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(key) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let modified_unix_ms = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0);
        candidates.push(ShardCandidate {
            key: key.to_string(),
            modified_unix_ms,
        });
    }
    candidates
}
```

Export `ShardCandidate`, `rank_shard_candidates`, `discover_recent_shard_keys`, and `DEFAULT_SHARD_MAX_AGE_MS` from `shared/mod.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p luchta-cache`
Expected: PASS, including the restore tests that broke in Task 7 — they write and read within one process, so the local shard dir is discoverable.

- [ ] **Step 5: Rewrite the two obsolete commit-key tests**

Task 7 removed git-commit shard naming, which left two e2e tests in
`crates/luchta-cli/tests/shared_cache_e2e.rs` asserting on a directory layout that no longer
exists. Both protect properties that DO survive — rewrite them to assert the property instead
of the mechanism. Do not delete them.

`dirty_key_isolation` currently asserts `snapshots/<commit>-dirty` exists and `snapshots/<commit>`
does not. The surviving property is that a clean build must not consume a dirty build's entry —
now enforced by `decide_shared_restore` comparing `record.inputs` against the working tree, not by
key namespacing. Rewrite it as: build with a dirty tree (counter reaches 1), commit the change,
build again clean, and assert the second build did NOT report a shared hit and the counter advanced
to 2. Drop every assertion about snapshot directory names. Rename it to
`dirty_tree_entry_is_not_reused_by_clean_build` and update the doc comment to describe the
content-validation mechanism.

`accumulation_single_snapshot_multiple_entries` currently builds the path
`snapshots/<commit>` and asserts one shard dir holds both entries. Under session shard keys two
separate `luchta run` invocations produce two shard dirs, so that premise is gone; what survives is
that both entries remain discoverable. Rewrite it as: run `lint`, run `test`, then delete the local
cache and re-run each, asserting both report a shared hit and neither counter advances. Rename it to
`entries_from_separate_runs_are_both_discoverable`.

Both rewrites must be discriminating. For the dirty test, confirm it fails if you make
`decide_shared_restore` return `true` unconditionally. Report that evidence.

- [ ] **Step 6: Run the full suite**

Run: `cargo nextest run --workspace --no-fail-fast`
Expected: all tests passing, 0 failed. Task 7 left 17 failures; this task must clear every one.

- [ ] **Step 7: Commit**

```bash
git add crates/luchta-cache/src/shared/discovery.rs crates/luchta-cache/src/shared/mod.rs crates/luchta-cli/tests/shared_cache_e2e.rs
git commit -m "discover shared cache shards by recency instead of git ancestry"
```

---

### Task 9: Remote shard listing

**Files:**
- Modify: `crates/luchta-cache/src/shared/rclone/mod.rs:31-42` (`Entry`)
- Modify: `crates/luchta-cache/src/shared/remote.rs` (add `list_shard_candidates`; `snapshots_fs` gains a no-arg root variant)
- Modify: `crates/luchta-cache/src/shared/mod.rs:481-495` (`pull_candidate_commits`)
- Test: inline tests in `remote.rs`

**Interfaces:**
- Consumes: `ShardCandidate` / `rank_shard_candidates` (Task 8).
- Produces:
  - `rclone::Entry` gains `pub mod_time: String` (deserialized from `ModTime`, RFC3339).
  - `pub(crate) fn RemoteSync::list_shard_candidates(&self) -> Vec<ShardCandidate>`
  - `pub(crate) fn RemoteSync::snapshots_root_fs(&self) -> String`

rclone's `operations/list` returns `ModTime` as an RFC3339 string. Parse it with `time::OffsetDateTime::parse` (already a dependency — confirm with `rg -n '^time' crates/luchta-cache/Cargo.toml`; if absent, sort by key name instead and skip the parse, since keys are chronological).

- [ ] **Step 1: Write the failing test**

Add to the test module in `remote.rs`:

```rust
    #[test]
    fn list_shard_candidates_returns_remote_shard_dirs() {
        let remote_root = tempfile::tempdir().unwrap();
        let snapshots = remote_root.path().join("snapshots");
        std::fs::create_dir_all(snapshots.join("0000000000001-aaaa")).unwrap();
        std::fs::create_dir_all(snapshots.join("0000000000002-bbbb")).unwrap();
        // A stray file at the shard-dir level must be ignored.
        std::fs::write(snapshots.join("stray.txt"), b"x").unwrap();

        let remote = RemoteSync::new(
            Arc::new(RcloneRcd::with_default_timeout().unwrap()),
            format!(":local:{}", remote_root.path().display()),
            8,
        );

        let mut keys: Vec<String> = remote
            .list_shard_candidates()
            .into_iter()
            .map(|candidate| candidate.key)
            .collect();
        keys.sort();

        assert_eq!(keys, vec!["0000000000001-aaaa", "0000000000002-bbbb"]);
    }
```

`RemoteSync::new(rclone, remote_base_fs, timeout_disable_threshold)` is at `remote.rs:142`; `remote.rs:903` shows the same `:local:` construction pattern.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p luchta-cache remote::tests::list_shard_candidates`
Expected: compile error — no method `list_shard_candidates`.

- [ ] **Step 3: Add `ModTime` to `rclone::Entry`**

```rust
pub struct Entry {
    #[serde(rename = "Path")]
    pub path: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "IsDir")]
    pub is_dir: bool,
    #[serde(rename = "Size")]
    pub size: i64,
    /// RFC3339. Absent or unparseable on some backends — callers fall back to
    /// key ordering, which is chronological because keys are timestamps.
    #[serde(rename = "ModTime", default)]
    pub mod_time: String,
}
```

Fix any `Entry { .. }` literal in tests that now misses the field.

- [ ] **Step 4: Implement `list_shard_candidates`**

In `remote.rs`:

```rust
    pub(crate) fn snapshots_root_fs(&self) -> String {
        format!(
            "{}/{SNAPSHOTS_DIR_NAME}",
            self.remote_base_fs.trim_end_matches('/')
        )
    }

    /// List shard directories on the remote with their modification times.
    ///
    /// Returns an empty list on any error — discovery then falls back to
    /// whatever is already in the local cache.
    pub(crate) fn list_shard_candidates(&self) -> Vec<ShardCandidate> {
        if self.is_disabled() {
            return Vec::new();
        }
        let entries = match self
            .rclone
            .list(&self.snapshots_root_fs(), "", self.rclone.default_timeout())
        {
            Ok(entries) => {
                self.record_remote_success();
                entries
            }
            Err(err) => {
                self.record_remote_error(&err);
                eprintln!("debug: remote snapshot listing failed: {err}");
                return Vec::new();
            }
        };

        entries
            .into_iter()
            .filter(|entry| entry.is_dir)
            .map(|entry| ShardCandidate {
                modified_unix_ms: parse_mod_time_unix_ms(&entry.mod_time)
                    .unwrap_or_else(|| shard_key_unix_ms(&entry.name).unwrap_or(0)),
                key: entry.name,
            })
            .collect()
    }
```

And the two helpers, as free functions in `remote.rs`:

```rust
fn parse_mod_time_unix_ms(mod_time: &str) -> Option<u64> {
    if mod_time.is_empty() {
        return None;
    }
    time::OffsetDateTime::parse(mod_time, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|parsed| (parsed.unix_timestamp_nanos() / 1_000_000) as u64)
}

/// Recover the timestamp from a `<unix_ms>-<nonce>` shard key.
fn shard_key_unix_ms(key: &str) -> Option<u64> {
    key.split_once('-')?.0.parse().ok()
}
```

If `time` is not a dependency of `luchta-cache`, drop `parse_mod_time_unix_ms` and use `shard_key_unix_ms(&entry.name).unwrap_or(0)` alone. Adjust the test accordingly and note it in the commit message.

- [ ] **Step 5: Merge remote candidates into discovery**

In `shared/mod.rs`, change `pull_candidate_commits` so it lists the remote, ranks local plus remote candidates together, and pulls the winners:

```rust
    #[cfg(unix)]
    fn pull_candidate_commits(&self, remote: Option<&RemoteSync>) {
        let Some(remote) = remote.cloned() else {
            return;
        };
        Self::run_candidate_pulls_on_dedicated_thread(
            remote,
            self.snapshot_store.clone(),
            self.candidate_keys.clone(),
        );
    }
```

The candidate list itself now has to include remote-only shards, which means it can no longer be computed at construction time from local state alone. Move the merge into `build_index`, replacing the `self.candidate_keys.iter().rev()` loop's input:

```rust
    fn build_index(&self, #[cfg(unix)] remote: Option<&RemoteSync>) -> MergedIndex {
        #[cfg(unix)]
        let keys = self.merged_candidate_keys(remote);
        #[cfg(not(unix))]
        let keys = self.candidate_keys.clone();

        #[cfg(unix)]
        self.pull_candidate_commits_for(remote, &keys);

        let mut merged = MergedIndex::new();
        for shard_key in keys.iter().rev() {
            self.load_commit_into_index(
                &mut merged,
                shard_key,
                #[cfg(unix)]
                remote,
            );
        }
        merged.snapshots.reverse();
        merged
    }

    /// Union the local shard dirs with the remote listing, then rank by recency.
    #[cfg(unix)]
    fn merged_candidate_keys(&self, remote: Option<&RemoteSync>) -> Vec<String> {
        let mut candidates = discovery::local_shard_candidates_for(&self.paths);
        if let Some(remote) = remote {
            let known: std::collections::HashSet<String> =
                candidates.iter().map(|c| c.key.clone()).collect();
            for candidate in remote.list_shard_candidates() {
                if !known.contains(&candidate.key) {
                    candidates.push(candidate);
                }
            }
        }
        let now_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0);
        discovery::rank_shard_candidates(
            candidates,
            self.history_len,
            Some(discovery::DEFAULT_SHARD_MAX_AGE_MS),
            now_unix_ms,
        )
    }
```

Make `local_shard_candidates` public as `pub fn local_shard_candidates_for(paths: &SharedCachePaths) -> Vec<ShardCandidate>` in `discovery.rs`, and add a `history_len: usize` field to `SharedCache`, populated from the constructor argument that is currently passed straight to `candidate_commit_keys`. Rename `pull_candidate_commits` to `pull_candidate_commits_for(&self, remote: Option<&RemoteSync>, keys: &[String])` and have it pull exactly the keys it is given.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo nextest run --workspace --no-fail-fast`
Expected: 0 failures.

- [ ] **Step 8: Commit**

```bash
git add crates/luchta-cache/src/shared/rclone/mod.rs crates/luchta-cache/src/shared/remote.rs crates/luchta-cache/src/shared/mod.rs crates/luchta-cache/src/shared/discovery.rs
git commit -m "discover remote shared cache shards by listing and recency"
```

---

### Task 10: Rollup packs

**Files:**
- Modify: `crates/luchta-cache/src/shared/snapshot.rs` (add `write_rollup_shard`)
- Modify: `crates/luchta-cache/src/shared/gc.rs:168-189` (generalise the throttle marker, add `should_run_rollup`)
- Modify: `crates/luchta-cache/src/shared/mod.rs` (call the rollup at the end of `build_index`)
- Modify: `crates/luchta-cache/src/shared/remote.rs` (add `enqueue_push_snapshot_upload`)
- Test: inline tests in `snapshot.rs` and `gc.rs`

**Interfaces:**
- Consumes: `Snapshot`, `SnapshotStore`, `SnapshotUpload` from `snapshot.rs:32-51`.
- Produces: `pub fn SnapshotStore::write_rollup_shard(&self, shard_keys: &[String], rollup_key: &str) -> Option<SnapshotUpload>` — merges every entry from `shard_keys` into a single new shard under `rollup_key`, and **does not delete the sources**.

**Why this task exists:** one shard per run plus a recency window of `history_len` means a busy repo's window covers only the last few hours. A rollup shard is a full merge of everything currently discoverable, so one recent pull covers weeks of history. Sources are left alone because another machine may be mid-read — deleting them across sessions would lose entries. GC ages them out instead.

**Also in this task — bound the read window by entry count, not shard count.**

Task 7 made shard keys per-`luchta run` invocation, and discovery takes the newest
`DEFAULT_SHARED_CACHE_HISTORY_LEN` (20) shards *by count*. Twenty local runs therefore evict every CI
shard from the candidate window — precisely the cross-machine reuse #277 exists to enable. Rollup packs
alone don't fix it: a pack older than 20 local shards is evicted too.

Change `rank_shard_candidates` to accept an entry budget alongside the count limit, and have
`ShardCandidate` carry an `entry_count: usize` (cheap for local shards; for remote shards use the
listing's size as a proxy, or 1 when unknown). Walk newest-first, accumulating until either the entry
budget is reached or the age cutoff is passed. Twenty tiny local shards then contribute few entries and a
large CI pack still makes the window. Keep the existing count limit as a hard upper bound so a single
enormous pack can't pull in unbounded shards. Add a test proving 20 one-entry local shards do not evict an
older 500-entry pack.

**Cadence:** the rollup needs the discovered shard key list, which only exists inside `build_index`. The existing `maybe_run_gc` call is in the CLI (`crates/luchta-cli/src/run/setup.rs:361-367`), before the index is built, so it is the wrong hook. Put the rollup at the end of `build_index` behind its own throttle marker, mirroring `gc.rs`'s `should_run_gc` / `write_gc_marker` pair. Do not roll up on every store — it re-serializes the whole index.

- [ ] **Step 1: Write the failing test**

Add to the test module in `snapshot.rs`:

```rust
    #[test]
    fn write_rollup_shard_merges_all_source_shards() {
        let temp = TempDir::new().unwrap();
        let paths = crate::shared::paths::open_shared_paths(temp.path()).unwrap();
        let store = SnapshotStore::new(paths);

        store.merge_entry("0000000000001-aaaa", sample_entry_with_seed(1, [1; 32]));
        store.merge_entry("0000000000002-bbbb", sample_entry_with_seed(2, [2; 32]));

        let upload = store
            .write_rollup_shard(
                &["0000000000001-aaaa".to_string(), "0000000000002-bbbb".to_string()],
                "0000000000003-rollup",
            )
            .expect("rollup should be written");

        assert!(!upload.shard_id.is_empty());

        let rolled = store.load("0000000000003-rollup").expect("rollup snapshot loads");
        assert_eq!(rolled.entries.len(), 2);

        // Sources survive — another machine may still be reading them.
        assert!(store.load("0000000000001-aaaa").is_some());
        assert!(store.load("0000000000002-bbbb").is_some());
    }

    #[test]
    fn write_rollup_shard_returns_none_when_there_is_nothing_to_roll_up() {
        let temp = TempDir::new().unwrap();
        let paths = crate::shared::paths::open_shared_paths(temp.path()).unwrap();
        let store = SnapshotStore::new(paths);

        assert!(store.write_rollup_shard(&[], "0000000000003-rollup").is_none());
    }
```

`sample_entry_with_seed` already exists at `snapshot.rs:1188`.

Add the throttle test to `gc.rs`'s test module:

```rust
    #[test]
    fn should_run_rollup_throttles_back_to_back_calls() {
        let temp = TempDir::new().unwrap();
        let paths = crate::shared::paths::open_shared_paths(temp.path()).unwrap();

        assert!(should_run_rollup(&paths, Duration::from_secs(3600)));
        assert!(!should_run_rollup(&paths, Duration::from_secs(3600)));
    }

    #[test]
    fn rollup_throttle_is_independent_of_the_gc_throttle() {
        let temp = TempDir::new().unwrap();
        let paths = crate::shared::paths::open_shared_paths(temp.path()).unwrap();

        assert!(should_run_rollup(&paths, Duration::from_secs(3600)));
        // GC has its own marker, so it is still eligible.
        assert!(maybe_run_gc(&paths, Duration::from_secs(60), Duration::from_secs(3600)).is_some());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p luchta-cache snapshot::tests::write_rollup_shard`
Expected: compile error — no method `write_rollup_shard`.

- [ ] **Step 3: Implement the rollup**

In `impl SnapshotStore`:

```rust
    /// Merge every entry from `shard_keys` into one new shard at `rollup_key`.
    ///
    /// Source shards are deliberately left in place: another machine may be
    /// reading them, and cross-session deletion would lose entries. GC ages
    /// them out on its own schedule.
    pub fn write_rollup_shard(
        &self,
        shard_keys: &[String],
        rollup_key: &str,
    ) -> Option<SnapshotUpload> {
        let mut rolled = Snapshot::new();
        for shard_key in shard_keys {
            let Some(snapshot) = self.load(shard_key) else {
                continue;
            };
            merge_shard_entries(&mut rolled, snapshot);
        }

        if rolled.entries.is_empty() {
            return None;
        }

        let shard_dir = self.shard_dir_path(rollup_key);
        if let Err(err) = fs::create_dir_all(&shard_dir) {
            eprintln!(
                "warning: failed to create rollup shard dir {}: {err}",
                shard_dir.display()
            );
            return None;
        }

        // No visible shards to subsume: the rollup adds, it never deletes.
        match self.write_consolidated_shard(rollup_key, &rolled, &[]) {
            MergeEntryOutcome {
                new_snapshot_upload: Some(upload),
                ..
            } => Some(upload),
            _ => None,
        }
    }
```

In `gc.rs`, generalise the throttle marker so the rollup can have its own. Change `should_run_gc` and `write_gc_marker` to take a marker file name, keep `gc_marker_path` as `paths.root.join(name)`, and pass `".gc-marker"` from `maybe_run_gc`. Export:

```rust
/// Marker file for the shard rollup throttle.
pub const ROLLUP_MARKER_NAME: &str = ".rollup-marker";

/// True if `throttle` has elapsed since the last rollup. Stamps the marker.
pub fn should_run_rollup(paths: &SharedCachePaths, throttle: Duration) -> bool {
    if !should_run_marked(paths, ROLLUP_MARKER_NAME, throttle, SystemTime::now()) {
        return false;
    }
    let _ = write_marker(paths, ROLLUP_MARKER_NAME, SystemTime::now());
    true
}
```

In `shared/mod.rs`, add the rollup to `SharedCache` and call it at the very end of `build_index`, after `merged.snapshots.reverse()`, passing the `keys` that built the index:

```rust
    /// Roll the currently-discoverable shards into one merged shard.
    ///
    /// Throttled independently of GC. Sources are never deleted here — another
    /// machine may be reading them.
    #[cfg(unix)]
    fn maybe_write_rollup(&self, keys: &[String]) {
        if keys.len() < 2 || !gc::should_run_rollup(&self.paths, gc::DEFAULT_GC_THROTTLE) {
            return;
        }
        let rollup_key = current_session_shard_key();
        let Some(upload) = self.snapshot_store.write_rollup_shard(keys, &rollup_key) else {
            return;
        };
        let Some(remote) = &self.remote else {
            return;
        };
        if remote.is_disabled() {
            return;
        }
        remote.enqueue_push_snapshot_upload(rollup_key, upload);
    }
```

Add `enqueue_push_snapshot_upload(&self, commit_key: String, upload: SnapshotUpload)` to `RemoteSync`: `push_snapshot_upload` at `remote.rs:495` already does the transfer, so the new method only enqueues a job on the existing push queue that calls it.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p luchta-cache`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/luchta-cache/src/shared/snapshot.rs crates/luchta-cache/src/shared/gc.rs crates/luchta-cache/src/shared/mod.rs crates/luchta-cache/src/shared/remote.rs
git commit -m "roll shared cache shards into a merged pack on a throttle"
```

---

### Task 11: The #277 e2e regression test

**Files:**
- Create: `crates/luchta-cli/tests/shared_cache_discovery_e2e.rs`

**Interfaces:**
- Consumes: everything from Tasks 7–10.
- Produces: nothing consumed by later tasks.

- [ ] **Step 1: Write the failing test**

Create `crates/luchta-cli/tests/shared_cache_discovery_e2e.rs`:

```rust
mod common;

use assert_cmd::Command;
use assert_fs::prelude::*;
use common::{init_git, write_counter_task_config, write_root_workspace};

fn git(temp: &assert_fs::TempDir, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(temp.path())
        .status()
        .expect("git command");
    assert!(status.success(), "git {args:?} failed");
}

fn run(temp: &assert_fs::TempDir, cache_dir: &std::path::Path) -> String {
    let out = Command::cargo_bin("luchta")
        .unwrap()
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("LUCHTA_SHARED_CACHE", "1")
        .env("LUCHTA_SHARED_CACHE_DIR", cache_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out).unwrap()
}

/// Regression for #277: cache entries written on one branch must be findable
/// from an unrelated branch. Discovery used to walk first-parent ancestry from
/// HEAD, so a build on an ephemeral merge commit — the Prow case — could never
/// see, or be seen by, any other build.
#[test]
fn cache_written_on_one_branch_is_found_from_an_unrelated_branch() {
    let shared_cache_dir = tempfile::tempdir().unwrap();
    let temp = assert_fs::TempDir::new().unwrap();
    write_root_workspace(&temp);

    write_counter_task_config(
        &temp,
        r#""app#build":{"cache":{},"worker":"shell","inputs":["src.txt"],"outputs":["out.txt"],"command":"sleep 0.15 && count=$(cat ../../run-count.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > ../../run-count.txt; echo built > out.txt"}}"#,
    );

    temp.child("packages/app/src.txt").write_str("source\n").unwrap();
    temp.child("packages/app/package.json")
        .write_str(r#"{"name":"app","scripts":{"build":"echo ignored"}}"#)
        .unwrap();
    init_git(&temp);

    // Build on a feature branch.
    git(&temp, &["checkout", "-b", "feature-one"]);
    run(&temp, shared_cache_dir.path());
    assert_eq!(
        std::fs::read_to_string(temp.path().join("run-count.txt")).unwrap(),
        "1\n"
    );

    // Move to a sibling branch whose history does NOT contain feature-one's commit,
    // and drop the local cache so only the shared cache can serve the task.
    git(&temp, &["checkout", "-b", "feature-two", "master"]);
    std::fs::remove_dir_all(temp.path().join(".luchta/cache")).unwrap();
    std::fs::remove_file(temp.path().join("packages/app/out.txt")).ok();

    let second = run(&temp, shared_cache_dir.path());

    assert!(second.contains("📥 1"), "expected a shared hit, got:\n{second}");
    assert_eq!(
        std::fs::read_to_string(temp.path().join("run-count.txt")).unwrap(),
        "1\n",
        "the task body should not have run a second time"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("packages/app/out.txt")).unwrap(),
        "built\n"
    );
}
```

`init_git` commits on whatever the default branch is; if `git checkout -b feature-two master` fails because the default branch is named something else, read the name with `git rev-parse --abbrev-ref HEAD` right after `init_git` and branch from that instead.

- [ ] **Step 2: Run the test to verify it passes**

Run: `cargo nextest run -p luchta-cli --test shared_cache_discovery_e2e`
Expected: PASS. To confirm the test is meaningful, `git stash` the Phase 2 commits and re-run — it should fail with `expected a shared hit` and `run-count.txt` reading `2\n`.

- [ ] **Step 3: Run the full workspace suite**

Run: `cargo nextest run --workspace`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/luchta-cli/tests/shared_cache_discovery_e2e.rs
git commit -m "cover cross-branch shared cache discovery e2e

Closes #277."
```

---

## Verification

After Task 11, confirm the whole thing end to end:

```bash
cargo nextest run --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Then check the two issue scenarios by hand against a real repo with `LUCHTA_SHARED_CACHE=1`:

1. Run a no-output task (lint or typecheck) in two different packages, wipe `.luchta/cache`, re-run. Both should report `📥`.
2. Build on a branch, create a sibling branch off the trunk, wipe `.luchta/cache`, build again. Should report `📥`.
