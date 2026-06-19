# S3 remote build cache via an rclone rcd sidecar (no S3 SDK)

**Date:** 2026-06-19
**Area:** `crates/luchta-cache/src/shared/` (`snapshot.rs`, `remote.rs`, `rclone.rs`, `gc.rs`, `mod.rs`), `crates/luchta-cli/src/run/setup.rs`
**Issue:** dobesv/luchta#22

## Problem

We wanted an opt-in S3 remote layer on top of the local shared build cache
without compiling an S3/AWS SDK into luchta (maintainer constraint: the cache
tool should be its own process). The hard part: the existing per-commit snapshot
was a single mutable file, and updating a mutable object on S3 safely needs a
compare-and-swap (conditional PUT). rclone's RC API can't do a clean CAS
(opaque `412`s, multipart-ETag blindness), so a mutable snapshot on S3 is a
dead end.

## Solution

### 1. Append-only content-addressed shards dissolve the CAS requirement
Replace the mutable `snapshots/<commit>.bincode` with an immutable directory of
shards: `snapshots/<commit>/<blake3-of-bytes>.bincode`. Same content => same
name => idempotent write. Restore = list all `*.bincode` shards for the commit,
read each, merge in memory. Cross-shard conflicts (same key, different
`outputs_hash`) resolve deterministically by **first shard-id (blake3 hex)
sort** — reproducible across machines because the id is a content hash, and
non-corrupting because outputs are content-addressed + validated on restore (a
wrong pick only causes a recompute, never wrong outputs).

This is local+remote-**symmetric**, so the remote transport only ever does
immutable **copy / list / delete** — no CAS anywhere.

### 2. Compaction via a `.merged` sidecar ledger
On store, the writer reads the visible shard set `S`, writes ONE consolidated
shard, writes `<new-id>.merged` listing the ids in `S`, then deletes **only**
those exact ids. A racing writer's brand-new shard (not in `S`) is never deleted
=> no lost entries. Steady state ≈ 1 shard/commit.

**`.merged` is NEVER consulted during restore** — it is purely a delete/GC
ledger. Restore reads only physically-present `.bincode` shards. This keeps
restore "miss-only" under list/delete visibility races: a subsumed-but-not-yet-
deleted shard merely re-contributes known entries; a deleted shard merely drops
to a miss.

Legacy single-file `snapshots/<commit>.bincode` is read as one extra shard for
back-compat and **never mutated**.

### 3. Transport = a luchta-owned `rclone rcd` sidecar
`rclone rcd --rc-addr unix://<per-run-socket> --rc-no-auth`, spawned lazily on
the first remote op, RC HTTP API over the unix socket (hyper + hyperlocal — no
S3 SDK). Lifecycle mirrors the resident-worker pattern: lazy spawn, readiness
via `rc/noop` poll, graceful `core/quit` + kill-on-`Drop`. The socket lives in a
**per-run unique temp dir** so a SIGKILL-orphaned daemon is harmless (bound to a
stale path, never reused). Every RC call is **deadline-bounded** (`tokio::time::
timeout`) so no remote op can hang a build wave.

## Gotchas (each cost real debugging time)

- **rclone fs/remote contract.** `operations/{copyfile,list,stat,deletefile}`
  treat `fs` as the **container** and `remote` as a path **relative to that
  fs**. `fs=":local:"` + `remote="/abs/path"` returns `404 object not found`.
  Correct: `fs=":local:/abs/dir"` + `remote="file.txt"` (caller owns the
  fs/root split). The blob/shard keys are built as `<base>/blobs` + `<hash>.tar.zst`
  and `<base>/snapshots/<commit>` + `<shard>.bincode`.
- **Test rclone LIVE.** A delegate's gated lifecycle test was silently SKIPPED
  in its sandbox (no rclone on PATH), so the fs/remote bug above only surfaced
  when finally run against a real `rclone rcd`. Install rclone and actually run
  the gated tests before trusting them.
- **404 is a normal MISS, not a remote-health failure.** The run-wide
  remote-disable flag (warn-once → degrade to local-only) trips on
  timeout/unavailable/process/request/non-404 HTTP. A `404` ("directory not
  found" for a commit that simply has no remote shards yet — e.g. the
  `<commit>-dirty` candidate) must be treated as a cache miss. Disabling on 404
  wrongly killed the remote and dropped real hits for the clean commit.
- **Never fail a build for cache reasons.** Every rclone/S3 error degrades
  silently to local-only. Opt-in is strict: `LUCHTA_SHARED_CACHE=rclone:<spec>`
  enables remote; `local`/`1`/`true` = local-only (unchanged); unset = off.
  `LUCHTA_SHARED_CACHE_SYNC_TIMEOUT` (default 30s) bounds the initial sync.
- **Never drop the rclone runtime from inside an async context.** `RcloneRcd`
  owns a `tokio::runtime::Runtime`. The `SharedCache` Arc is released *inside*
  the build's tokio runtime (the `run_tasks` async task), so its `Drop` chain
  reaches `RcloneRcd::Drop` while still in an async context. Calling
  `runtime.block_on(...)` there — or simply letting the owned `Runtime` drop
  there — panics: *"Cannot drop a runtime in a context where blocking is not
  allowed."* This only surfaces at runtime (the unit tests dropped the cache
  outside async, so they missed it). Fix: in `Drop`, **move** the runtime out
  (`Option<Runtime>` + `take()`) onto a fresh `std::thread`, run the
  quit/wait/kill there, and let the runtime drop on that thread. `shutdown()`
  (called from `SharedCache::Drop`) does the same via `std::thread::scope` so
  `block_on` never runs on the async thread. Add a regression test that drops a
  remote-configured `SharedCache` inside a `block_on` — it reproduces the panic
  on the unfixed code and passes after.
- **Remote GC is out of scope** — use S3 bucket lifecycle rules. Local GC was
  extended to the shard-dir layout (age out shards, remove `.merged` sidecars
  with their shard, prune empty commit dirs, still age out legacy files) and
  stays GC-race tolerant.

## CodeScene discipline after a large feature
A big feature can tank module code-health (Low Cohesion from too many new
functions, Large/Complex methods, 5+ arg functions). The fix that worked:
extract the new subsystem into its own module (`shared/remote.rs`), bundle
related args into small request/context structs (`OpenExtras`, `PushArtifacts`,
`PullCommit`, `CopyFile`, `ShardMergeContext`), and split large/complex methods.
Run `cs delta origin/HEAD` and iterate until zero new/degraded issues. Do it as a
dedicated final pass, compiling after each small change (full-file rewrites from
memory drift and break the build).

## Prior art this built on
- `docs/solutions/logic-errors/shared-build-cache-validate-on-restore-2026-06-17.md`
  (validate-on-restore + GC-race/corrupt-snapshot tolerance — applied to remote shards).
- `docs/solutions/integration-issues/resident-worker-process-management-2026-06-09.md`
  (sidecar spawn/readiness/shutdown/orphan handling — reused for rclone rcd).
