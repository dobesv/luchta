---
title: "Process-based proxy worker chain with engine-side dependsOn injection"
date: 2026-06-14
category: integration-issues
problem_type: integration_issue
component: luchta-worker, luchta-engine
root_case: "Workers needing environment setup cannot answer resolve at startup; needed composable filter/lazy chain architecture"
resolution_type: code_fix
severity: high
tags:
  - rust
  - process-proxy
  - worker-chain
  - oneshot-race
  - dependency-injection
  - timeout-discipline
  - argv-parsing
  - waiter-cleanup
  - drop-safety
plan_ref: luchta-issue-48-worker-prereqs
last_updated: 2026-06-14
---

## Problem

Workers requiring environment setup (yarn install, PnP, dependency builds) could not answer `resolve` at startup because real worker processes had to be live before graph-build. Needed architecture for: (1) lazy workers that defer delegate startup, (2) composable filters that prune during resolve, (3) native worker-level `dependsOn` config injected engine-side.

## Solution

### 1. Process-Based Proxy Primitive

Added `luchta-worker::proxy` module with `DelegateHandle` — a thin process-based proxy that spawns a delegate worker as a child and forwards JSONL. Enables worker chain composition via `--` split in argv.

**Key structural insight:** Wrapper workers link ONLY `luchta-worker`, never `luchta-engine`. This preserves worker↔engine dependency boundary and keeps worker binaries lightweight.

```rust
// luchta-worker/src/proxy.rs — Core API
pub struct DelegateArgvSplit {
    pub stage_args: Vec<String>,
    pub delegate_command: Vec<String>,
}
pub fn split_current_process_argv() -> DelegateArgvSplit;

pub struct DelegateHandle {
    delegate_command: Vec<String>,
    state: Mutex<Option<DelegateState>>,
    stdout_writer: SharedWriter,  // Arc<Mutex<Box<dyn AsyncWrite>>>
    stderr_writer: SharedWriter,
}

impl DelegateHandle {
    pub fn new(delegate_command: Vec<String>) -> Self;
    pub async fn send(&self, message: WorkerMessage) -> Result<WorkerResponse, ProxyError>;
    pub async fn send_with_timeout(&self, message: WorkerMessage, timeout: Duration) -> ...;
    pub async fn shutdown(&self) -> Result<(), ProxyError>;
}
```

**Execution model:**
- Lazy spawn on first `send()`
- `send()` forwards one `WorkerMessage` JSONL line to delegate stdin
- Responses routed by `response.id()` back to callers
- `shutdown()` closes stdin, waits/escalates (SIGTERM→SIGKILL after 5s)

### 2. Oneshot vs Notify: Lost-Wakeup Race Fix

Initial impl used `Notify+Mutex<Option<WorkerResponse>>`. Race: fast delegate responds before caller parks → Notify fires with no waiter → send hangs forever.

**Fix:** Per-request oneshot channels:

```rust
type ResponseWaiters = Arc<Mutex<HashMap<String, oneshot::Sender<ResponseResult>>>>;

pub async fn send_with_timeout(...) -> Result<WorkerResponse, ProxyError> {
    let (response_tx, response_rx) = oneshot::channel();
    waiters.lock().await.insert(id.clone(), response_tx);
    // ... send message ...
    response_rx.await.expect("sender not dropped")
}
```

Oneshot buffers the single value; response delivered before caller park is never lost.

### 3. Shared Stdout Writer + Double-Write Avoidance

Each wrapper process has ONE stdout for delegate responses. `DelegateHandle` retains `stdout_writer: SharedWriter` (Arc). All instances in same process share same writer.

**Double-write avoidance:** Proxy reader task writes delegate responses directly. Caller `send()` returns received value — never writes response itself.

### 4. Argv[0] Includes Binary Name Gotcha

`split_current_process_argv()` uses `std::env::args()`, so `stage_args[0]` is the wrapper binary name, NOT the first stage argument.

```rust
// WRONG: predicate[0] = "luchta-command-filter" → re-exec loop
let predicate = argv.stage_args;

// CORRECT: skip argv[0]
let predicate = argv.stage_args.into_iter().skip(1).collect::<Vec<_>>();
```

Caught in command-filter (resolve pruned ALL tasks), file-exists-filter (benign bogus glob added).

### 5. Test Delegate: Portable sh JSONL Loopback

Do NOT use Python in raw strings or `${var%%pattern}` shell expansion. Use:

```sh
id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
case "$line" in *'"type":"run"'*|*'"type":"resolve"'*)
  printf '{"type":"done","id":"%s","exitCode":0}\n' "$id"
esac
```

Portable POSIX, no raw-string backslash issues, no shell syntax variations.

### 6. Duplex EOF + drop(handle) Test Discipline

`DelegateHandle` retains writer Arcs. Duplex reader only sees EOF when ALL Arcs drop.

```rust
handle.shutdown().await.expect("shutdown ok");
// MUST drop handle else reader blocks forever
drop(handle);
let responses = read_json_lines(reader).await;
```

### 7. Timeout Discipline: Bounded Resolve, Unbounded Run

Graph-build must not hang on alive-but-silent delegate. Run can stream Logs for long builds.

```rust
// Resolve FORWARD: bounded (30s timeout, prune on silence)
handle.send_with_timeout(WorkerMessage::ResolveTask(...), Duration::from_secs(30));

// Run: unbounded (delegate death via EOF signals completion)
handle.send(WorkerMessage::Run(...));
```

Predicate timeout (command-filter): spawn child + `tokio::time::timeout(30s, child.wait())`.

### 8. Engine-Side dependsOn Injection: Ordering Critical

Inject AFTER resolve, BEFORE apply_prunes:

```rust
pub async fn build_resolved(...) {
    let mut resolved_pipeline = ResolvedPipeline::build(...);
    let pruned = resolved_pipeline.resolve(...).await?;
    resolved_pipeline.inject_worker_dependencies(worker_definitions);  // AFTER resolve
    resolved_pipeline.apply_prunes(&pruned);  // BEFORE apply_prunes
    ...
}

fn inject_worker_dependencies(&mut self, worker_defs: &HashMap<String, WorkerDefinition>) {
    for definition in self.tasks_by_id.values_mut() {
        let Some(worker_name) = &definition.worker else { continue };
        let Some(worker_def) = worker_defs.get(worker_name) else { continue };
        let mut seen: HashSet<DependsOn> = definition.depends_on.iter().cloned().collect();
        for dep in &worker_def.depends_on {
            if seen.insert(dep.clone()) {
                definition.depends_on.push(dep.clone());
            }
        }
    }
}
```

**Ordering rationale:**
- AFTER resolve: Worker `Modify(depends_on=...)` cannot erase injected deps
- BEFORE apply_prunes: Injected deps subject to normal pruning rules
- Dedupe: Both task's existing deps AND duplicates within worker's own list

## Why This Works

1. **Process proxy preserves protocol boundary:** Workers never depend on engine internals; chain composition happens via argv `--` split.

2. **Oneshot race-free:** Single-value buffer guarantees delivery; no lost-wakeup.

3. **Shared writer scalable:** Multiple delegates in same process share one stdout; no interleaving.

4. **Engine-side injection preserves worker autonomy:** Workers don't know about dependsOn; engine injects at graph-build time.

5. **Pruning silent:** Filters prune without logging; resolves cleanly.

---

## Proxy Reliability Patterns (PR #62 Review)

### Drain-Loop Waiter Cleanup

JSONL proxy reader routes responses to per-request `oneshot` waiters. Early `?` on read/parse/write errors bypasses end-of-loop `fail_all_waiters` cleanup → in-flight `send()` callers hang forever on non-EOF failures.

**Pattern:** Guarantee waiter-failure cleanup on EVERY exit path (clean EOF AND every error), not just the EOF path.

```rust
// BAD: early ? skips fail_all_waiters
async fn read_delegate_stdout(...) {
    while let Some(line) = lines.next_line().await? {  // EOF ok, parse err bypasses cleanup
        let response: WorkerResponse = serde_json::from_str(&line)?;  // malformed JSON bypasses
        waiters.lock().await.remove(&response.id()).map(|tx| tx.send(Ok(response)));
    }
    fail_all_waiters(&waiters).await;  // only reached on clean EOF
}

// GOOD: wrap loop, cleanup on any exit
async fn read_delegate_stdout(...) -> Result<(), ProxyError> {
    let result = async {
        while let Some(line) = lines.next_line().await? {
            let response: WorkerResponse = serde_json::from_str(&line)?;
            waiters.lock().await.remove(&response.id()).map(|tx| tx.send(Ok(response)));
        }
        Ok(())
    }.await;
    fail_all_waiters(&waiters).await;  // runs on EOF AND errors
    result
}
```

**Regression test:** Feed malformed (non-JSON) line while request in-flight, assert caller gets `Err` under timeout.

### Drop Without a Runtime

`Drop` impl that calls `tokio::spawn` panics if value dropped outside active Tokio runtime (e.g., after `_runtime.drop()`). Pattern: guard with `Handle::try_current()`.

```rust
impl Drop for DelegateHandle {
    fn drop(&mut self) {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async { shutdown_delegate(state).await });
        } else {
            // No runtime: synchronous best-effort cleanup
            let _ = state.child.start_kill();  // cross-platform, no Unix-only dep
            state.stdout_task.abort();
            state.stderr_task.abort();
        }
    }
}
```

**Why `start_kill()`:** Cross-platform (works on Windows), avoids adding Unix-only deps.

---

### Inject-Before-Validate Consistency

Feature injecting synthetic graph edges (worker-level `dependsOn`) must run SAME injection in validation/check path as in resolve path, otherwise typo'd references silently escape `check` diagnostics.

**Pattern:** Thread `worker_definitions` through validation; apply injection before building `DependencyContext`.

## Prevention Strategies

### Test Cases

- **send_with_timeout_surfaces_clean_error_when_delegate_stays_silent**: Delegate reads stdin forever, never responds → ResponseTimeout, no hang.
- **concurrent_requests_route_by_id**: Multiple in-flight ids, responses routed correctly.
- **argv_skip_one**: Verify wrapper interprets stage_args without argv[0].
- **worker_dep_injection_survives_worker_modify**: Injected deps survive worker `Modify(depends_on=...)`.
- **worker_dep_injection_dedupes**: Identical dep in task and worker → single edge.
- **malformed_delegate_stdout_fails_inflight_waiter_instead_of_hanging**: Non-JSON line → waiter gets `DelegateClosed("invalid JSON")`, no hang.
- **dropping_delegate_handle_without_runtime_does_not_panic**: Drop outside runtime → sync cleanup, no panic.

### Best Practices

- **Workers link luchta-worker only**: Never import from luchta-engine.
- **Oneshot per request**: Never use Notify+Option for single-response correlation.
- **Test delegates use portable sh**: No Python, no shell-specific expansion.
- **drop(handle) before duplex read**: Otherwise blocks on EOF.
- **Bound resolve, unbound run**: Graph-build hung on silent delegate = deadlock.
- **Wrap drain loops**: Ensure cleanup runs on all exit paths (EOF AND errors).
- **Guard Drop spawns**: Check `Handle::try_current()` before `tokio::spawn` in `Drop`.

### Code Review Checklist

- [ ] Wrapper workers use `.skip(1)` on stage_args?
- [ ] Proxy tests drop handle before duplex read?
- [ ] Resolve forward has timeout?
- [ ] Inject runs after resolve, before apply_prunes?
- [ ] Worker dependsOn deduped against task's existing deps?
- [ ] Drain loops: cleanup runs on ALL exit paths (EOF + errors)?
- [ ] Drop impls using `tokio::spawn` guarded with `Handle::try_current()`?
- [ ] Synthetic edge injection: same transform in check and resolve paths?

## Related Issues

- **Prior Solution:** [worker-trait-harness-extraction-2026-06-11.md](./worker-trait-harness-extraction-2026-06-11.md) — Worker trait, lightweight deps
- **Prior Solution:** [worker-crash-handle-cache-dead-reuse-2026-06-13.md](../logic-errors/worker-crash-handle-cache-dead-reuse-2026-06-13.md) — EAGAIN/Stdio::null rationale
- **GitHub:** [#48](https://github.com/dobesv/luchta/issues/48) — Worker prerequisites
- **GitHub:** [#38](https://github.com/dobesv/luchta/issues/38) — Related dependsOn work
- **PR:** [#62](https://github.com/dobesv/luchta/pull/62) — Proxy reliability patterns (drain-loop cleanup, Drop safety, inject-before-validate)
