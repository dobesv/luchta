---
title: "Executable config loader with process-group timeout and ETXTBSY retry"
date: 2026-06-08
category: integration-issues
problem_type: integration_issue
component: luchta-cli/config-loader
root_cause: "Process lifecycle gaps and kernel race condition in config script execution"
resolution_type: code_fix
severity: high
tags:
  - process-management
  - timeout
  - process-group
  - etxtbsy
  - executable-config
  - serde-compatibility
plan_ref: luchta
---

## Problem

Luchta's static `luchta.toml` config loader was replaced with an executable config-script loader (`luchta-config.*`). The initial implementation had three critical reliability gaps: (1) no timeout on config script execution allowing indefinite hangs at startup, (2) orphaned child processes persisting after timeout, and (3) a Linux-specific `ETXTBSY` race causing flaky tests under parallel load.

## Symptoms

1. **Indefinite hang**: A config script containing `sleep 60` would block `luchta check` and `luchta run` indefinitely with no user feedback.
2. **Flaky tests**: Under parallel `#[tokio::test]` load (20+ runs), config-loader tests intermittently failed (~3/20) with different tests each time. Error: `assertion failed: message.contains("failed to parse config script output")` — spawn failed before reaching JSON parse.
3. **Test hangs**: After adding timeout, a unit test took 60 seconds instead of ~1 second because killing the direct child (`sh`) left orphaned grandchildren (`sleep 30`) holding inherited stdout pipe open.
4. **camelCase/snake_case compatibility**: Existing TOML tests used snake_case (`depends_on`, `max_weight`) while new JSON config uses camelCase (`dependsOn`, `maxWeight`).

## Investigation Steps

1. **ETXTBSY diagnosis**: Examined `crates/luchta-cli/src/config.rs` and found `fs::set_permissions(0o755)` immediately before `Command::spawn()`. Under parallel test load, Linux returns `ETXTBSY` (errno 26) when exec races with recent file write/chmod. Each test uses its own tempdir, ruling out shared-path collision.

2. **Timeout hang diagnosis**: Initial timeout implementation called `child.start_kill()` on timeout. The shell config script's grandchild (`sleep 30`) was orphaned and kept the inherited stdout pipe open, so the parent's `wait_with_output()` hung until grandchild exited naturally.

3. **Process-group kill**: Verified that `tokio::process::Command::process_group(0)` creates a new PGID matching child PID. Negative PID in `libc::kill(-pgid, SIGKILL)` sends signal to entire process group including grandchildren.

4. **Serde compatibility**: Confirmed `#[serde(rename = "dependsOn", alias = "depends_on")]` pattern correctly handles both JSON camelCase and legacy TOML snake_case.

## Root Cause

1. **Timeout hang**: `wait_with_output()` has no timeout by default. Grandchild processes inherit stdout pipe and survive parent termination, blocking read side.

2. **Orphan processes**: `start_kill()` sends SIGKILL to direct child only. Shell scripts may spawn grandchildren that survive as orphans.

3. **ETXTBSY race**: Linux kernel marks file "text busy" when another process has write handle open. Parallel tests writing/chmodding temp scripts can race with exec.

4. **Testing gap**: ETXTBSY retry path needed deterministic tests, not reliance on kernel race reproduction.

## Solution

### 1. Config execution timeout with process-group kill

```rust
// Constants
const DEFAULT_CONFIG_TIMEOUT: Duration = Duration::from_secs(30);
const CONFIG_TIMEOUT_ENV_VAR: &str = "LUCHTA_CONFIG_TIMEOUT_SECS";
const ETXTBSY_ERRNO: i32 = 26;
const EXECUTE_CONFIG_ETXTBSY_RETRIES: usize = 10;

// Spawn in process group on Unix
#[cfg(unix)]
let mut command = Command::new(config_path);
#[cfg(unix)]
command.process_group(0);  // New PGID = child PID

command.current_dir(workspace_root);
command.stdout(Stdio::piped());
command.stderr(Stdio::piped());

// Timeout wrapper
let timeout_duration = config_timeout();
let result = tokio::time::timeout(timeout_duration, async {
    let child = spawn_with_retry(|| spawn_config_script(workspace_root, config_path)).await?;
    child.wait_with_output().await
}).await;

// On timeout, kill process group then reap
match result {
    Err(_) => {
        terminate_config_script(&mut child).await;  // Process-group kill
        bail!("config script `{}` timed out after {}s", path, timeout_duration.as_secs());
    }
    Ok(Ok(output)) => { /* handle output */ }
    Ok(Err(e)) => { /* handle spawn/wait error */ }
}

// Unix termination: kill PGID, then reap direct child
#[cfg(unix)]
async fn terminate_config_script(child: &mut Child) {
    let pgid = child.id().unwrap_or(0) as i32;
    if pgid > 0 {
        let _ = unsafe { libc::kill(-pgid, libc::SIGKILL) };  // Negative = process group
    }
    let _ = child.start_kill();
    let _ = child.wait().await;  // Reap to prevent zombie
}
```

### 2. ETXTBSY bounded retry

```rust
fn spawn_with_retry<F, T, E>(mut spawn_fn: F) -> io::Result<T>
where
    F: FnMut() -> io::Result<T>,
{
    let mut retries = 0;
    loop {
        match spawn_fn() {
            Ok(result) => return Ok(result),
            Err(error) if is_etxtbsy(&error) && retries < EXECUTE_CONFIG_ETXTBSY_RETRIES => {
                retries += 1;
                std::thread::sleep(EXECUTE_CONFIG_ETXTBSY_BACKOFF);
            }
            Err(error) if is_etxtbsy(&error) => {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("file still busy after {} retries", retries)
                ));
            }
            Err(other) => return Err(other),
        }
    }
}

fn is_etxtbsy(error: &io::Error) -> bool {
    error.raw_os_error() == Some(ETXTBSY_ERRNO)  // Linux-specific
}
```

### 3. Deterministic ETXTBSY tests via injectable spawner

```rust
#[test]
fn retries_etxtbsy_then_succeeds() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_clone = attempts.clone();
    
    let spawn_fn = move || {
        let n = attempts_clone.fetch_add(1, Ordering::SeqCst);
        if n < 3 {
            Err(io::Error::from_raw_os_error(ETXTBSY_ERRNO))
        } else {
            Ok(mock_successful_spawn())
        }
    };
    
    let result = spawn_with_retry(spawn_fn);
    assert!(result.is_ok());
    assert_eq!(attempts.load(Ordering::SeqCst), 4);  // 3 failures + 1 success
}
```

### 4. Serde dual-case support

```rust
// crates/luchta-types/src/lib.rs
#[derive(Serialize, Deserialize)]
pub struct TaskDefinition {
    #[serde(default, rename = "dependsOn", alias = "depends_on")]
    pub depends_on: Vec<DependsOn>,
    // ...
}

// crates/luchta-types/src/config.rs
#[derive(Serialize, Deserialize)]
pub struct ConcurrencyConfig {
    #[serde(default = "default_max_weight", rename = "maxWeight", alias = "max_weight")]
    pub max_weight: u32,
}
```

### 5. Improved failure diagnostics

```rust
fn format_exit_status(status: &ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exited with code {}", code),
        None => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(sig) = status.signal() {
                    return format!("terminated by signal {}", sig);
                }
            }
            "exited with unknown status".to_string()
        }
    }
}

// Include bounded stderr tail in error message
const STDERR_TAIL_MAX_LINES: usize = 20;
const STDERR_TAIL_MAX_BYTES: usize = 2048;
```

## Why This Works

1. **Process-group kill**: `process_group(0)` creates a new PGID. `kill(-pgid, SIGKILL)` sends signal to all PGID members including orphaned grandchildren. The negative-PID convention (`-pgid`) targets the process group, not just the direct child.

2. **Bounded retry**: ETXTBSY is transient. Ten retries with 5ms backoff (50ms total budget) handles the race without infinite loops. Retry scope is narrow: only `spawn()`, not entire execution.

3. **Injectable spawner**: Generic `spawn_with_retry<F>` accepts a closure, allowing tests to inject mock failures deterministically. Tests cover retry-then-success, exhaustion, and non-ETXTBSY immediate-fail paths.

4. **Dependency injection for timeout**: `load_config_with_timeout(root, Duration)` internal helper lets tests pass short timeout (1s) directly, avoiding env-var races across parallel tests.

5. **Serde alias**: `rename` sets new primary (camelCase JSON), `alias` preserves legacy (snake_case TOML). Existing TOML tests pass unchanged.

## Prevention Strategies

**Test Cases:**
- Timeout behavior test: spawn script with `sleep 30`, assert timeout error within 2s
- Process-group cleanup test: run script spawning grandchildren, verify no orphan processes after timeout
- ETXTBSY retry: inject spawner returning error 26 N times, verify retry count
- ETXTBSY exhaustion: inject always-failing spawner, verify exhaustion message
- Non-ETXTBSY: inject other error, verify immediate failure (no retry)
- Stderr tail: script outputting many lines, verify bounded capture in error

**Best Practices:**
- Always use `process_group(0)` when spawning processes that may create children
- Kill process group via negative PGID, then `wait()` to reap direct child
- Handle `ESRCH` (process already gone) gracefully
- Define errno constants (Linux-specific): `const ETXTBSY_ERRNO: i32 = 26;`
- Use `tokio::time::timeout()` around any potentially-hanging async operation
- Capture stderr with bounds for diagnostics while still teeing live to parent

**Code Review Checklist:**
- [ ] Config script execution wrapped in `tokio::time::timeout`?
- [ ] Process spawned in its own process group (`process_group(0)`)?
- [ ] Timeout path kills process group (negative PGID), not just direct child?
- [ ] Direct child reaped via `wait()` after kill?
- [ ] Retry logic scoped to specific error (ETXTBSY), not blanket?
- [ ] Serde aliases preserve backward compatibility?
- [ ] Test coverage for retry paths via injectable seams?
- [ ] Tests don't rely on env vars (process-global) for parallel-safety?

## Gotchas

1. **ETXTBSY is Linux-specific**: errno 26 is Linux's `ETXTBSY`. BSD/macOS don't have this error. On those platforms, the retry logic never triggers (harmless).

2. **Negative PID convention**: `kill(-pgid, signal)` targets process group. Positive PID targets single process. Documentation often omits this Unix quirk.

3. **Stdout pipe inheritance**: Grandchildren inherit stdout pipe. Orphan holding pipe keeps read side blocked. Process-group kill solves this.

4. **Zombie race window**: After `kill(-pgid)` and `wait()`, grandchildren become zombies briefly until init reaps. Transient, no practical impact.

5. **Env vars are process-global**: Don't use `LUCHTA_CONFIG_TIMEOUT_SECS` in parallel tests. Inject timeout via function parameter instead.

6. **Ignore ESRCH after kill**: Process may already be gone. Treat `ESRCH` as success.

## Related Issues

- Plan note `d8ac8a0d`: Config format amendment specifying executable config design
- Plan note `5a46159d`: ETXTBSY diagnosis and root cause analysis
- Plan note `da9ee38c`: Timeout/kill diagnosis for 60-second test hang
- Plan note `92c00f5f`: Executable config loader verification
- Plan note `344b2361`: Verification summary and remaining debt
