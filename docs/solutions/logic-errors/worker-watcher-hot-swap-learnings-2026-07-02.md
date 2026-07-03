---
title: "Worker-watcher hot-swap: current-thread starvation, absolute/relative glob matching, and terminal-response synthesis"
date: 2026-07-02
category: logic-errors
problem_type: logic_error
component: luchta-worker-watcher
root_cause: "Multiple subtle concurrency and protocol contract issues: blocking recv on current-thread runtime causing starvation, notify delivering absolute paths against relative globs, and missing terminal responses on delegate failure"
resolution_type: code_fix
severity: high
tags:
  - tokio
  - current-thread-runtime
  - file-watching
  - glob-matching
  - notify
  - worker-protocol
  - terminal-response
  - hot-swap
plan_ref: luchta-worker-watcher
issue: "#170"
---

## Problem

`luchta-worker-watcher` — a JSONL-over-stdin/stdout worker-protocol middleware that wraps a delegate worker, watches file globs, and hot-swaps the delegate on matching changes — exhibited multiple subtle bugs: (1) total hang under a current-thread tokio runtime when bridging the sync notify callback into async via `std::sync::mpsc::recv()`, (2) file changes never triggering hot-swap because `GlobSet::is_match` silently returned false for relative globs against notify's absolute paths, and (3) upstream callers hanging indefinitely when the delegate crashed or stdin send failed, because no terminal `Done` was emitted for in-flight message IDs.

## Symptoms

- **Current-thread starvation**: Wrapper process hung with zero output. Stdin reader, router actor, and child I/O tasks never made progress. Runtime appeared deadlocked.
- **Glob mismatch silence**: Watched files under relative globs like `src/**/*.rs` never triggered a hot-swap. Adding debug logging showed `path_matches` always returned false despite matching changes.
- **Terminal-response hang**: The e2e test harness (`Harness`) blocked waiting for terminal responses that never arrived after delegate crashes or send failures. Upstream caller timeout or indefinite hang.
- **Test flake**: e2e tests that touched a watched file immediately after wrapper start missed the event; tests failed spuriously on timing.

## Investigation Steps

1. **Runtime starvation**: Enabled tokio-console, observed all tasks parked. Traced the notify event callback: used `std::sync::mpsc::channel` to bridge file events, calling `rx.recv()` inside `tokio::spawn`. On `new_current_thread()` runtime, this blocked the sole thread, starving all other tasks.

2. **Glob mismatch**: Added debug output to `path_matches`. notify emitted `/home/user/project/src/main.rs` but glob was `src/**/*.rs`. `GlobSet::is_match` returned false silently. Verified notify always delivers absolute paths.

3. **Terminal-response**: Injected delegate crash in e2e test; observed outstanding message IDs never received synthesized `Done`. Router actor only handled clean shutdown, not mid-stream failures.

4. **Single-actor stdout ownership**: Initially distributed stdout writes across generation tasks. Observed interleaved/multiline corruption under load.

## Root Cause

1. **Current-thread starvation**: `std::sync::mpsc::Receiver::recv()` is a blocking call. Inside a `tokio::spawn`ed task on a `new_current_thread()` runtime, it parks the single OS thread. No other task can run — the runtime is starved. The general rule: **never call blocking operations (blocking_recv, blocking_lock, blocking_send, std::sync::mpsc::recv) from within a task on a current-thread runtime**.

2. **Absolute vs. relative path mismatch**: `notify` delivers absolute paths. `GlobSet::is_match` performs literal matching. A glob like `src/**/*.rs` won't match `/home/user/project/src/main.rs` without path transformation.

3. **Terminal-response contract violation**: The worker protocol requires every accepted message to reach a terminal response (`Done` for `Run`, `Resolved` for `ResolveTask`). When delegate stdin send fails or delegate stdout EOFs with outstanding IDs, the router must synthesize `Done{id, exitCode:1}` through the stdout writer.

4. **Concurrent stdout writes**: Without a single serialization point, multiple generations could write `Done` responses simultaneously, corrupting the JSONL protocol.

5. **Test race condition**: notify may not be fully registered when the wrapper process starts. Tests touching files immediately after spawn race against watcher arming.

## Solution

### 1. Bridge sync callback with tokio::sync::mpsc::unbounded_channel

**Before:**
```rust
// notify callback (sync)
let (tx, rx) = std::sync::mpsc::channel();
let tx_clone = tx.clone();
// ... in callback: tx_clone.send(event).unwrap();

// async consumer (blocks!)
tokio::spawn(async move {
    while let Some(event) = rx.recv() {  // BLOCKS THE THREAD
        // process event
    }
});
```

**After:**
```rust
use tokio::sync::mpsc;

let (tx, mut rx) = mpsc::unbounded_channel();
let tx_clone = tx.clone();
// ... in callback: tx_clone.send(event).unwrap();  // sync, non-blocking

tokio::spawn(async move {
    while let Some(event) = rx.recv().await {  // async await
        // process event
    }
});
```

`UnboundedSender::send` is sync and callable from the notify callback. The async side awaits without blocking the runtime.

### 2. Match event paths in multiple forms

```rust
fn path_matches(globset: &GlobSet, cwd: &Option<PathBuf>, canonical_cwd: &Option<PathBuf>, path: &Path) -> bool {
    // Try raw path first (for absolute globs)
    if globset.is_match(path) {
        return true;
    }

    // Try stripping cwd prefix (for relative globs)
    if let (Some(cwd), Ok(relative)) = (cwd, path.strip_prefix(cwd)) {
        if globset.is_match(relative) {
            return true;
        }
    }

    // Try canonicalized-relative (for symlinked tempdirs like macOS /private/var vs /var)
    if let (Some(canonical_cwd), Ok(canonical_path)) = (canonical_cwd, path.canonicalize()) {
        if let Ok(relative) = canonical_path.strip_prefix(canonical_cwd) {
            if globset.is_match(relative) {
                return true;
            }
        }
    }

    false
}
```

This handles: relative globs, absolute globs, and symlinked paths (macOS tempdirs).

### 3. Synthesize terminal responses on failure

```rust
// In router actor, on delegate crash or send failure:
async fn handle_generation_failure(&mut self, gen_id: u64, outstanding_ids: Vec<String>) {
    let writer = &mut self.stdout;
    for id in outstanding_ids {
        let done = WorkerResponse::done(&id, 1);  // exitCode: 1
        let line = serde_json::to_string(&done).expect("serialize Done");
        writer.write_all(line.as_bytes()).await.ok();
        writer.write_all(b"\n").await.ok();
    }
}
```

Every accepted message ID gets a synthesized `Done{id, exitCode:1}` through the single stdout writer.

### 4. Single-actor router owns all stdout writes

All state mutations and stdout writes serialize through one actor task:

```rust
pub enum RouterEvent {
    Inbound(WorkerMessage),
    Response(u64, WorkerResponse),
    StdoutClosed(u64),
    FileChanged,
    ShutdownAll,
}

impl MessageRouter {
    async fn run(&mut self, mut events: mpsc::Receiver<RouterEvent>) {
        while let Some(event) = events.recv().await {
            match event {
                RouterEvent::Inbound(msg) => self.handle_inbound(msg).await,
                RouterEvent::Response(gen_id, resp) => self.handle_response(gen_id, resp).await,
                RouterEvent::StdoutClosed(gen_id) => self.handle_stdout_closed(gen_id).await,
                RouterEvent::FileChanged => self.handle_file_changed().await,
                RouterEvent::ShutdownAll => self.handle_shutdown().await,
            }
        }
    }
}
```

No concurrent stdout access across coexisting generations.

### 5. send_line must be non-blocking and async-safe

`RawDelegate::send_line` used by the async router must not use `blocking_lock` or `blocking_send` (panics inside a runtime):

```rust
impl RawDelegate {
    pub fn send_line(&self, line: String) -> Result<(), ProxyError> {
        let tx = self.stdin_tx.lock().expect("stdin mutex").clone();
        tx.send(line).map_err(|_| ProxyError::StdinClosed)
    }
}
```

Lock a std Mutex only to clone the `UnboundedSender`, then sync `send`. No `blocking_lock` on tokio Mutex.

### 6. Test poll/retry for file watcher arming

```rust
// e2e test: poll until restart observed
let start = std::time::Instant::now();
while start.elapsed() < Duration::from_secs(10) {
    touch_file(&watched_file);
    if saw_restart(&output) {
        break;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
}
assert!(saw_restart(&output), "restart should be observed");
```

Bounded retry until the change is observed, or settle before a negative assertion.

## Why This Works

1. **Unbounded channel bridging**: `tokio::sync::mpsc::UnboundedSender::send` is a non-blocking sync call safe from any context. The async receiver can await without blocking the runtime.

2. **Multiple path forms**: Covering raw (absolute globs), cwd-stripped (relative globs), and canonicalized (symlink handling) ensures all valid matches succeed.

3. **Terminal synthesis**: The protocol contract is upheld — every accepted message reaches a terminal response even on delegate failure. Upstream callers never hang.

4. **Single-actor stdout**: Serialization avoids interleaved JSONL output and races between Notify and check-then-wait patterns.

5. **Non-blocking send_line**: A std Mutex held only to clone a sender, then sync send, avoids panics and blocking inside async contexts.

6. **Poll/retry test pattern**: Bounded retry handles the inherent race between process start and watcher registration.

## Prevention Strategies

### Test Cases

- **Starvation test**: Current-thread runtime with concurrent notify events + stdin I/O. Assert no hang, all events processed.
- **Glob form coverage**: Unit tests for path_matches with relative globs, absolute globs, symlinked paths (macOS `/private/var`).
- **Terminal synthesis**: Delegate crash with outstanding IDs, assert synthesized `Done{id, exitCode:1}` for each.
- **Concurrent generations**: File change during drain, assert in-flight ops complete on old generation, new ops on new.
- **Send failure**: Inject stdin send failure, assert synthesized terminal response.
- **Test race**: e2e with immediate file touch, poll/retry until restart observed.

### Best Practices

- **Never call blocking operations on current-thread runtime**: No `blocking_recv`, `blocking_lock`, `blocking_send`, or `std::sync::mpsc::recv` inside tokio tasks on `new_current_thread()`.
- **Bridge sync to async with tokio::sync::mpsc**: `UnboundedSender::send` is sync and non-blocking; async side awaits.
- **Handle multiple path forms in file watchers**: Absolute/relative/canonicalized for glob matching against notify events.
- **Synthesize terminal responses on all failure paths**: Delegate crash, send failure, stdout EOF with outstanding IDs.
- **Single-actor stdout ownership**: Serialize all writes through one task; no concurrent access across generations.
- **Non-blocking send_line**: Clone sender from std Mutex, sync send — no tokio Mutex blocking.

### Code Review Checklist

- [ ] Sync-to-async bridge uses tokio::sync::mpsc, not std::sync::mpsc?
- [ ] No blocking operations inside tokio::spawn on current-thread runtime?
- [ ] File watcher path matching handles absolute, relative, and canonicalized forms?
- [ ] Every accepted message ID gets a terminal response on all failure paths?
- [ ] Single actor owns stdout writes (no concurrent access)?
- [ ] send_line uses std Mutex + clone, not tokio Mutex blocking_lock?
- [ ] e2e tests poll/retry for file watcher arming races?

## Related Issues

- **Prior Art:** [integration-issues/resident-worker-process-management-2026-06-09.md](../integration-issues/resident-worker-process-management-2026-06-09.md) — Foundational worker lifecycle, JSONL IPC, shutdown patterns
- **Prior Art:** [logic-errors/async-shutdown-worker-pool-notify-race-2026-06-10.md](./async-shutdown-worker-pool-notify-race-2026-06-10.md) — Async shutdown, Notify patterns, single-actor serialization
- **Prior Art:** [logic-errors/watch-input-aware-rebuild-registry-2026-07-01.md](./watch-input-aware-rebuild-registry-2026-07-01.md) — Watch mode, file change detection, glob matching
- **GitHub:** [dobesv/luchta#170](https://github.com/dobesv/luchta/issues/170) — Worker-watcher hot-swap feature
- **Plan:** `luchta-worker-watcher`
