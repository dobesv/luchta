---
title: "Speeding up no-op cached builds — profile before parallelizing"
date: 2026-06-30
category: "performance-issues"
problem_type: performance_issue
component: "luchta-cache/change-detection"
root_cause: "repeated directory traversal per task without stat-based fast-path"
resolution_type: code_fix
severity: medium
tags:
  - rust
  - caching
  - performance
  - profiling
  - directory-traversal
  - blake3
  - gix
  - worktree-discovery
plan_ref: "luchta-perf-154"
last_updated: 2026-07-01
---

## Problem

A fully-cached build (`luchta run build`, 928 tasks all cache-hits) took ~37s, essentially all single-threaded. GitHub issue #154 guessed the fix was "more parallelism in file hashing" — but profiling showed hashing was negligible. The real cost was repeated directory traversal: 2305 walks of the same ~26 package dirs across input and output resolution.

## Symptoms

```
luchta run build (all cache hits)
Before: ~37s wall time
Impact: Developer wait time on no-op builds
```

Profiling showed:
- File hashing: negligible
- RESOLVE_INPUTS: ~18.6s (majority of runtime)
- RESOLVE_OUTPUTS: not measured separately (rolled into resolve phase)

## Investigation Steps

1. **Assumption testing**: The issue suggested parallel hashing. Profiled actual runtime — hashing was negligible.
2. **Poor-man's profiler**: System `perf record` blocked by `perf_event_paranoid=4`, so used env-gated `Instant` timers around suspected phases.
3. **Directory walk counting**: Added counters to track how many times each package dir was walked.
4. **Discovery**: 2305 directory walks across ~26 packages. Each task walked its package dir for inputs and again for outputs.

## Root Cause

Two sources of redundant work:

### (a) Eager hashing after stat

The cache-hit fast-path fetched prior file entries but then called `blake3_file()` unconditionally due to `unwrap_or()` eager argument evaluation, discarding the prior hash.

```rust
// BEFORE (eager evaluation bug — hashes unconditionally):
let hash = prior_entry
    .filter(|e| e.size == metadata.len() && e.mtime_ns == current_mtime)
    .map(|e| e.hash)
    .unwrap_or(blake3_file(path)?);  // BUG: blake3_file runs even when prior_entry matches

// AFTER (lazy evaluation):
let hash = match prior_entry {
    Some(e) if e.size == metadata.len() && e.mtime_ns == current_mtime => e.hash,
    _ => blake3_file(path)?,
};
```

The `unwrap_or` version hashes unconditionally because Rust evaluates the argument eagerly. Must use `match` or `unwrap_or_else` for lazy evaluation.

Also collapsed a double `stat` (`exists()` + `metadata()`) into one `fs::metadata()` with `NotFound` → absent.

### (b) Run-scoped directory listing cache

Walk each package dir once per `luchta run` and share across tasks:

- **Scope**: Owned in run setup, threaded through via `DispatchContext` or similar. MUST be scoped to one run.
- **NOT a process-lifetime `static`**: A static cache breaks visibility of files created/removed between runs and breaks `watch` cycles. An untracked-file test failed with a static cache because new files weren't visible.

Implementation sketch:
```rust
// In run setup:
let listing_cache = Rc<RefCell<HashMap<PathBuf, Arc<Vec<FileEntry>>>>>;

// Thread through DispatchContext, pass to resolver functions.
// On first access for a dir, walk + cache. Subsequent tasks hit the cache.
```

## Why This Works

1. **Stat fast-path**: `metadata()` syscalls are orders of magnitude cheaper than reading + hashing file contents. When (size, mtime) match, the prior blake3 hash is still valid. Blake3 is fast but I/O is slower.

2. **Listing cache**: `gix::discover` + `WalkDir` traversal has non-trivial overhead (git repo discovery, directory entry iteration, gitignore matching). Doing this once per package instead of once per task removes 2300+ redundant traversals.

3. **Run scope**: Ensures fresh listings for each `luchta run` invocation and each `watch` cycle. Files created/removed between runs are correctly observed.

## Result

```
Round 1 Before: ~37s for fully-cached build
Round 1 After:  ~19s for fully-cached build
Round 1 Speedup: ~2x
```

Also collapsed double `stat` into single `fs::metadata()` call.

---

## Round 2: Memoizing Git Worktree Discovery

After round 1 eliminated hashing and directory-walk redundancy, re-profiling revealed a new hotspot: `gix::discover` called ~3039 times despite all ~26 workspace packages living in ONE git repo. Each call walks UP the filesystem to find `.git` — cheap per-call but expensive at scale.

### Problem

`gix::discover(base_dir)` walks up the directory tree to find the git worktree root. Called per-task for input resolution, this accumulated to ~10s of the remaining ~18.6s runtime.

### Solution

Memoize the discovered worktree root per `base_dir` in the run-scoped `ListingCache`:

```rust
struct ListingCache {
    dir_listings: HashMap<PathBuf, Arc<Vec<FileEntry>>>,
    worktree_roots: HashMap<PathBuf, PathBuf>,  // base_dir → worktree root
}
```

Discovery runs ~once per distinct `base_dir` instead of per task.

### Pitfalls

1. **Over-eager "optimization" regressed performance**: A first attempt pre-collected ALL gitignored paths by doing a full `WalkDir` of the ENTIRE worktree root for every package's listing (343×). Result: RESOLVE_INPUTS 6.2s→17s, wall 30s — slower than before. Lesson: an "optimization" that adds a whole-repo walk per package is worse than the per-package walk it replaced. The correct fix kept the per-package subtree walk and only memoized repo discovery.

2. **Prefix-matching is WRONG for nested repos/submodules**: Keying by prefix (`base_dir.starts_with(root)`) means a directory inside a nested repo under a parent worktree gets served the PARENT's root. Fix: key by EXACT `base_dir` — `HashMap<base_dir, root>`. Each distinct `base_dir` keeps its own `gix::discover` result. A nested-repo test locked this in.

3. **`gix::Repository`/`AttributeStack` are not `Send`/`'static`**: They can't live in an `Arc<Mutex<...>>` run cache. Fix: cache the cheap, ownable derived data (worktree root `PathBuf`), and open the repo + build the ignore stack locally per listing call. Since discovery is memoized, opening the repo by path is cheap.

### Round 2 Result

```
Before: ~18.6s (after round 1)
After:  ~7s internal time for RESOLVE_INPUTS phase
Wall:  Reduced further from ~18.6s baseline
```

### Round 2 Prevention Strategies

**Re-profile after each win:**
- The bottleneck moves. After eliminating the top hotspot, the next-largest becomes visible.
- Round 1's fix (listing cache) made directory walks cheap, exposing `gix::discover`.

**Always measure "optimizations":**
- An optimization that adds work can regress. The whole-repo-walk-per-package attempt made things worse.
- Only profiling caught the regression before merge.

**Cache ownable data, not live handles:**
- `gix` types often aren't `Send`/`'static`. Cache derived data (`PathBuf`), re-open repo locally.

**Code Review Checklist (Round 2):**
- [ ] After a perf fix, was the workload re-profiled to find the next hotspot?
- [ ] Does the cache key correctly handle nested repos (exact match, not prefix)?
- [ ] Are cached values `Send` + `'static`, or is derived data cached instead?
- [ ] Was the "optimized" path measured against the baseline?

---

## Round 3: Output Candidate Walk Scoping (prefix_union)

After round 2, ~11s no-op time remained. Prior assumption: "build preparation" overhead. Profile-first overturned this.

### Problem

Env-gated `Instant` timers revealed the real split: prep ~3.6s vs **decide/skip loop ~7s**. Within decide, output candidate directory listing walked **1.44M files vs 27k for inputs**.

Root cause: the output lister did a raw `WalkDir` over the ENTIRE package dir just to glob-match patterns like `dist/**` — walking `node_modules`, `.git`, build artifacts, everything.

### Solution

Scope the output-candidate walk to each output pattern's **static (metachar-free) leading path prefix** — e.g., for `dist/**/*.js`, only walk `dist/`.

Key the run-scoped `ListingCache` by `(base_dir, prefix_union)` instead of `base_dir` alone, where `prefix_union` is the union of all output patterns' static prefixes for that task.

**Implementation anchors:**
- `crates/luchta-cache/src/resolve.rs` — `static_prefix()`, `prefix_union()`, `FilesystemLister`

### Edge Cases

1. **Leading `**`/brace/glob patterns**: No extractable prefix → fallback to full walk.
2. **`..` segment in pattern**: Return `None` → full walk. Do NOT "stop at prior prefix" — that under-walks and misses outputs outside the package dir.
3. **Multiple patterns**: Compute union of all prefixes. Precise narrow set is the optimization.

### Result

```text
Candidates walked: 1.44M → 59k
Warm no-op: ~11s → ~9s
Peak memory: 307MB → 167MB
Cache skips preserved: 928/928 ✓
```

### Counterintuitive Pitfall

A reviewer-suggested "optimization" to prune descendant prefixes in the union (collapse `[dist/types, dist/schema.json]` to `[dist]`) **regressed perf by ~3-4s**. Why? Broader walk to shared ancestor.

**Lesson:** The precise narrow prefix set IS the optimization. Broader roots walk more. Always A/B benchmark perf "cleanups" interleaved under identical machine load — load average matters (observed 9s vs 12s purely from system load variation).

### Round 3 Prevention Strategies

**Prefix union specifics:**
- Static prefix extraction MUST be metachar-free. `*`, `?`, `[` break the guarantee.
- `..` segments cannot be safely resolved without canonicalization, which may not exist yet — fallback to full walk.
- Pruning to shared ancestor is an **anti-pattern** — it broadens the walk.

**Benchmark hygiene:**
- Interleave A/B runs to cancel load noise.
- Same machine, same thermal state, same background processes.
- Report min/median of multiple runs.

**Code Review Checklist (Round 3):**
- [ ] Does the prefix extraction handle all glob metacharacters correctly?
- [ ] Does `..` in pattern fall back to full walk rather than incorrect partial?
- [ ] Was any "cleanup" measured against baseline?
- [ ] Are A/B runs interleaved to cancel load noise?

---

## Round 4: Parallel Decide Loop — Deferred

### Why Not Parallelized Yet

The decision path reads `current.dep_outputs` from a per-cycle mutable `output_hashes` map populated topologically **during** the loop. Naive parallel fan-out causes **false cache hits**: a downstream task reads a stale/absent upstream hash and wrongly skips.

### Safe Design (Deferred)

**Wave-based (topological-layer) parallelism with barrier per wave:**

1. Precompute decision map without reporting completion.
2. Have `dispatch_ready_task` consume precomputed decisions.
3. Each wave waits at barrier before next layer proceeds.

**Key constraint:** The decision stage is entangled with Walker's completion-signaling protocol (each task expects exactly one `done_tx` completion). Wave-preapply must NOT report/complete early.

### Architectural Blocker

Implementation hit deeper blocker: `dispatch_ready_task` expects to call decide and immediately emit completions. Wave-based approach requires separating decision from dispatch — a larger engine refactor.

### What Was Landed

**Owned `DecisionContext` (prerequisite for `spawn_blocking`):**

- Refactored decision context to be `Arc`-backed and `'static`.
- Enables `tokio::spawn_blocking` for CPU-bound cache decision work.
- Preserves AGENTS.md constraint: no Rayon, all concurrency via tokio.

**File anchors:**
- `crates/luchta-cli/src/run/dispatch.rs` — decide path, `dependency_output_hashes`, `output_hashes`
- `crates/luchta-engine/src/...` — `compute_execution_waves` (future wave-based dispatch)

### Why Pre-seeding output_hashes is Unsafe

Persisted records diverge in mixed hit/run builds. A task with a cache hit reuses the persisted output hash, but a task that runs produces a new output hash. Downstream decisions must see the NEW hash, not the persisted one. Only topological populate-then-read within the same cycle is correct.

### Round 4 Status

**LANDED:** Owned `DecisionContext` refactor — reusable for future wave-parallel decide.

**DEFERRED:** Wave-based parallel decide loop. Requires dispatch-core refactor to separate decision from completion reporting.

### Round 4 Prevention Strategies

**Concurrency safety:**
- Topological dependents read from map populated during the SAME cycle.
- Parallel reads require wave barriers or snapshot-and-seal approach.
- Pre-seeding from persisted state breaks mixed hit/run builds.

**Process hygiene:**
- Land independently-valuable pieces as separate commits (perf win, refactor prerequisite).
- Don't block verified wins on riskier changes.
- Respect repo constraints: AGENTS.md forbids Rayon.

**Code Review Checklist (Round 4):**
- [ ] Does parallelizing a loop preserve topological dependency order?
- [ ] Are reads fenced behind writes from topologically-prior tasks?
- [ ] Was a prerequisite refactor landed separately?
- [ ] Does the design respect AGENTS.md constraints (no Rayon)?

---

## Prevention Strategies

**Profiling before optimizing:**
- Profile the real workload before assuming the bottleneck. The issue reporter's suggested fix and the "obvious" hotspot (hashing) were both wrong.
- Cheap poor-man's profiling (env-gated atomic counters around suspected hot functions) beats guessing when system profilers are unavailable.
- **PROFILE-FIRST OVERTURNS ASSUMPTIONS (repeatedly):** Round 1 (hashing negligible), Round 3 ("build prep" was actually decide loop). Each round's assumption was wrong.

**Code patterns:**
- Use `match` or `unwrap_or_else` for lazy evaluation, not `unwrap_or(expensive_call())`.
- Prefer run-scoped caches over process-lifetime statics for data that may change between invocations.
- Scope directory walks to the minimal prefix set. Broader roots walk more.

**Repo constraints:**
- `AGENTS.md` explicitly forbids `rayon` ("Rayon is explicitly excluded"). The stat fast-path made hashing negligible, so rayon addition was unnecessary. Respect architectural constraints; profile to confirm a dependency actually pays for itself before adding it.
- All concurrency via tokio. Use `spawn_blocking` for CPU-bound work with `'static` contexts.

**Benchmark discipline:**
- Interleave A/B runs under identical load. System load variation can obscure 3s differences.
- Measure "cleanups" — they may be regressions.

**Process:**
- Land independently-valuable, verified pieces as separate commits.
- Don't block wins on riskier refactors.

**Code Review Checklist:**
- [ ] Does `unwrap_or` contain an expensive call that should be lazy?
- [ ] Is the cache scope correct (run vs process lifetime)?
- [ ] Was profiling done before proposing parallelism?
- [ ] Does the fix respect repo-wide dependency constraints (e.g., no rayon)?
- [ ] Was any "optimization" measured against the baseline?
- [ ] Are A/B benchmarks interleaved to cancel load noise?

## Related Issues

- **GitHub:** [#154](https://github.com/dobesv/luchta/issues/154) — Speed up no-op build (change detection)
- **Related Solution:** [performance-issues/yarn-lock-once-per-run-enum-state-2026-06-15.md](yarn-lock-once-per-run-enum-state-2026-06-15.md) — Similar "parse once per run" pattern with run-scoped state
- **Related Solution:** [performance-issues/remote-cache-parallel-sync-nested-runtime-2026-06-24.md](remote-cache-parallel-sync-nested-runtime-2026-06-24.md) — Another profiling-driven perf fix in cache layer
