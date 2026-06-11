---
title: "Worker trait harness extraction through copy-first development"
date: 2026-06-11
category: integration-issues
problem_type: integration_issue
component: luchta-worker
root_cause: "Excessive code duplication across worker binaries with ~93% identical plumbing"
resolution_type: code_fix
severity: medium
tags:
  - rust
  - worker-pattern
  - trait-abstraction
  - refactoring
  - copy-first-pattern
  - code-duplication
  - protocol-compatibility
plan_ref: luchta-bash-worker
---

## Problem

The bash-worker implementation (GitHub #42) needed to add a second worker type, but the existing `luchta-yarn-worker` binary had ~93% duplicated plumbing code. Designing a shared abstraction up-front risked YAGNI and getting the interface wrong without two real consumers to validate it.

## Symptoms

- **Near-duplicate worker binaries**: Yarn worker's `main.rs` contained ~360 lines of generic stdin/stdout protocol handling, child spawning, and error handling, with only ~18 lines of actual yarn-specific logic
- **Risk of designing wrong abstraction**: Creating a shared trait without two real implementations risks missing actual requirements
- **Heavy dependency bloat**: Worker binaries linked heavyweight dependencies (`petgraph`, `luchta-workspace`) through `luchta-engine`, unnecessarily bloating binary size

## Investigation Steps

1. **Phase 1: Copy-first approach** — Built `luchta-bash-worker` as a near-duplicate of `luchta-yarn-worker`, copying all plumbing code first and only modifying the worker-specific logic (blank-command validation via `ResolveTask.mode`, verbatim `sh -c` execution)

2. **Phase 2: Validate duplication** — Confirmed tests/protocol.rs for both workers were similar (but not identical) and passing. The bash worker's blank-command semantics differed from yarn workspace resolution, proving real differences exist.

3. **Phase 3: Extract common harness** — With two real implementations in hand, extracted `Worker` trait + `run_worker` harness to `crates/luchta-worker`. The trait signature crystallized naturally from actual usage:
   - `fn resolve_task(&self, req: &ResolveTask) -> ResolveResult` — worker-owned validation
   - `fn build_command(&self, req: &WorkerRequest) -> String` — covers yarn (workspace quoting) and bash (verbatim) equally

4. **Phase 4: Behavior guard** — Both workers' `tests/protocol.rs` files stayed **byte-unchanged** and continued passing, proving exact behavior preservation through the refactor.

## Root Cause

Shared plumber code lived inside each worker binary because:
1. No prior abstraction existed — protocol.rs was inside `luchta-engine` with heavy deps
2. Only one worker existed (yarn), so duplication wasn't visible
3. No trait interface existed for workers to implement

The "copy-first, extract-second" pattern avoided premature abstraction by forcing real duplication to emerge before designing the shared interface.

## Solution

### 1. Extract `luchta-worker` crate with protocol types

Moved `protocol.rs` from `luchta-engine` to new `luchta-worker` crate. Kept it lightweight:
- **NO** `petgraph` dependency
- **NO** `luchta-workspace` dependency
- Only `tokio`, `serde_json`, and minimal deps

Engine preserved API compatibility via re-export facade:
```rust
// crates/luchta-engine/src/worker/mod.rs
pub use luchta_worker as protocol;
```

### 2. Define `Worker` trait based on actual implementations

```rust
// crates/luchta-worker/src/runtime.rs
pub trait Worker: Send + Sync + 'static {
    fn resolve_task(&self, req: &ResolveTask) -> ResolveResult;
    fn build_command(&self, req: &WorkerRequest) -> String;
}
```

This minimal interface covers both workers:
- **BashWorker**: `resolve_task` checks blank commands (Reject in Check, Prune in Run); `build_command` returns `req.command.clone()`
- **YarnWorker**: `resolve_task` checks script existence in package; `build_command` adds `yarn workspace` prefix when needed

### 3. Generic harness handles all plumbing

```rust
pub async fn run_worker<W: Worker>(worker: W) -> Result<(), WorkerError> {
    let worker = Arc::new(worker);
    let writer: SharedWriter = Arc::new(Mutex::new(Box::new(stdout())));
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut requests = BufReader::new(stdin()).lines();
    let mut jobs = JoinSet::new();

    loop {
        match requests.next_line().await? {
            Some(line) => {
                let message = serde_json::from_str(&line)?;
                spawn_request(message, Arc::clone(&worker), &writer, &shutdown, &mut jobs);
            }
            None => break,
        }
    }

    drain_jobs(&mut jobs).await;
    Ok(())
}
```

### 4. Worker implementations collapse to trait impls

**Before** (yarn): ~360 lines with inline protocol handling  
**After**: ~139 lines total, with most being unit tests. Actual impl:

```rust
struct YarnWorker;

impl Worker for YarnWorker {
    fn resolve_task(&self, req: &ResolveTask) -> ResolveResult {
        let script = req.resolved_script_name();
        if req.scripts.iter().any(|candidate| candidate == script) {
            ResolveResult::accept()
        } else {
            ResolveResult::prune(Some(format!(
                "script `{script}` not found in package `{}`",
                req.package
            )))
        }
    }

    fn build_command(&self, req: &WorkerRequest) -> String {
        match req.workspace.as_deref() {
            None => req.command.clone(),
            Some("") => format!("yarn {}", req.command),
            Some(workspace) => format!(
                "yarn workspace {} {}",
                shell_single_quote(workspace),
                req.command
            ),
        }
    }
}

#[tokio::main]
async fn main() {
    run_worker_main(YarnWorker).await;
}
```

## Why This Works

1. **Copy-first validated real differences**: Building bash-worker first revealed that `resolve_task` semantics genuinely differ (blank-command vs script-lookup). The shared trait design absorbs this cleanly through polymorphism.

2. **Single `build_command` signature covers all cases**: The `&WorkerRequest -> String` return type handles yarn's workspace quoting and bash's verbatim execution equally without special cases.

3. **Worker-owned validation via ResolveTask.mode**: Workers participate in check-time resolution naturally. `ResolveMode::Check -> Reject`, `ResolveMode::Run -> Prune` pattern lets workers emit hard errors in `luchta check` while silently skipping in `luchta run`.

4. **Byte-unchanged tests/protocol.rs as behavior guard**: The protocol tests from Phase 1 were kept identical through Phase 2 refactor. Any behavior change would break these tests.

5. **Lightweight worker crate**: By excluding heavy deps (`petgraph`, `luchta-workspace`), worker binaries stay small. Workers only need protocol types and harness — not the full engine's dep tree.

6. **Re-export facade preserves downstream imports**: `pub use luchta_worker as protocol;` lets `luchta-engine` keep using `crate::worker::protocol::*` paths unchanged. Downstream crates importing `luchta_engine::{WorkerRequest, ...}` work without modification.

## Prevention Strategies

**Test Cases:**
- Stress test both workers with `--stress-count=5` after migration
- Verify `cargo tree -p luchta-worker` shows no `petgraph` or `luchta-workspace` deps
- Keep `tests/protocol.rs` byte-unchanged through refactors — diff must show exit 0

**Best Practices:**
- **Copy-first, extract-second**: When adding a variant of existing code, build it as a near-duplicate first. Get it green. THEN extract the shared abstraction with two real consumers in hand.
- **Behavior guard tests**: Lock protocol tests from Phase 1 as immutable witnesses. Any refactor that touches them is suspect.
- **Dependency isolation**: Shared harness crates should link minimal dependencies. Heavy deps belong in the engine, not in worker runtimes.
- **Re-export facades**: When moving types between crates, use `pub use` aliases at the old location to preserve API compatibility.

**Code Review Checklist:**
- [ ] Do both workers' `tests/protocol.rs` files match pre-refactor state (git diff shows no changes)?
- [ ] Does `luchta-worker` link zero heavy deps (petgraph, luchta-workspace)?
- [ ] Does the Worker trait signature match actual usage (no speculative methods)?
- [ ] Do worker binaries consume shared harness via thin trait impls (no inline protocol handling)?

## Related Issues

- **GitHub:** [dobesv/luchta#42](https://github.com/dobesv/luchta/issues/42) — Add bash-worker crate
- **Related Solution:** [resident-worker-process-management-2026-06-09.md](./resident-worker-process-management-2026-06-09.md) — Worker protocol liveness invariant (terminal `Done` emission)
- **Plan:** `luchta-bash-worker` — Full implementation history in plan notes including Phase 1 copy and Phase 2 extract decisions
