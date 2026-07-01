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
last_updated: 2026-06-30
---

## Problem

A fully-cached build (`luchta run build`, 928 tasks all cache-hits) took ~37s, essentially all single-threaded. GitHub issue #154 guessed the fix was "more parallelism in file hashing" — but profiling showed hashing was negligible. The real cost was repeated directory traversal: 2305 walks of the same ~26 package dirs across input and output resolution.

## Symptoms

```
- Behavior: ~37s wall time for fully-cached build (all 928 tasks cache-hits)
- CPU: Nearly all single-threaded during cache-skip path
- Scale: 928 tasks × ~2.5 walks/task = 2305 directory traversals
- Each walk: gix::discover + full WalkDir + gitignore filtering for inputs, separate walk for outputs
```

The issue report suggested parallelizing hashing with rayon. This was the wrong hypothesis.

## Investigation Steps

1. **Hypothesis**: Hashing is slow → parallelize with rayon.
2. **Profiling approach**: Added temporary atomic counters gated by an env var, dumped at process exit. This cheap approach worked where `perf`/`gdb` were blocked by sandbox `perf_event_paranoid`/`ptrace_scope`.
3. **Finding**: Blake3 hashing was a tiny fraction of total time. Real hotspot: repeated `gix::discover` + `WalkDir` traversals per-task.
4. **Root cause confirmed**: Each of 928 tasks independently walked its package dir (inputs) and often a second walk (outputs) — 2305 total walks over the same ~26 package directories.

## Root Cause

Two distinct problems:

1. **No stat fast-path**: Resolver always re-read + hash file contents, even when (size, mtime_ns) unchanged and prior hash available in cache record. `FileEntry` already persisted size+mtime+hash; resolver just wasn't given prior entries.

2. **No directory listing cache**: Each task traversed its package dir independently with `gix::discover` + `WalkDir` + gitignore filtering. No sharing across tasks in the same run.

## Solution

### (a) Stat-based content-hash fast-path

When a file's (size, mtime_ns) match the prior `FileEntry` from cache, reuse the stored hash instead of re-reading + hashing:

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

## Prevention Strategies

**Profiling before optimizing:**
- Profile the real workload before assuming the bottleneck. The issue reporter's suggested fix and the "obvious" hotspot (hashing) were both wrong.
- Cheap poor-man's profiling (env-gated atomic counters around suspected hot functions) beats guessing when system profilers are unavailable.

**Code patterns:**
- Use `match` or `unwrap_or_else` for lazy evaluation, not `unwrap_or(expensive_call())`.
- Prefer run-scoped caches over process-lifetime statics for data that may change between invocations.

**Repo constraints:**
- `AGENTS.md` explicitly forbids `rayon` ("Rayon is explicitly excluded"). The stat fast-path made hashing negligible, so rayon addition was unnecessary. Respect architectural constraints; profile to confirm a dependency actually pays for itself before adding it.

**Code Review Checklist:**
- [ ] Does `unwrap_or` contain an expensive call that should be lazy?
- [ ] Is the cache scope correct (run vs process lifetime)?
- [ ] Was profiling done before proposing parallelism?
- [ ] Does the fix respect repo-wide dependency constraints (e.g., no rayon)?

## Related Issues

- **GitHub:** [#154](https://github.com/dobesv/luchta/issues/154) — Speed up no-op build (change detection)
- **Related Solution:** [performance-issues/yarn-lock-once-per-run-enum-state-2026-06-15.md](yarn-lock-once-per-run-enum-state-2026-06-15.md) — Similar "parse once per run" pattern with run-scoped state
- **Related Solution:** [performance-issues/remote-cache-parallel-sync-nested-runtime-2026-06-24.md](remote-cache-parallel-sync-nested-runtime-2026-06-24.md) — Another profiling-driven perf fix in cache layer
