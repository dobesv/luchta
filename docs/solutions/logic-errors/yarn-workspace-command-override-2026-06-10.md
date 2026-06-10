---
title: "Yarn workspace command override for explicit task commands"
date: 2026-06-10
last_updated: 2026-06-10
category: logic-errors
problem_type: logic_error
component: luchta-yarn-worker
root_cause: "No mechanism to override default package.json script resolution for yarn worker tasks"
resolution_type: code_fix
severity: medium
tags:
  - yarn
  - worker-protocol
  - monorepo
  - shell-escaping
  - serde-compatibility
  - cross-crate-protocol
  - environment-injection
plan_ref: issue-19-yarn-worker-command
---

## Problem

Tasks using the yarn worker could only run package.json scripts matching the task name. No way existed to pass explicit commands (e.g., `yarn exec`, `yarn install`, or commands with args) without adding a script entry to package.json.

Additionally, the worker must **always** run scripts through `yarn` — never execute a resolved package.json script body directly. Yarn injects required environment variables (PATH, NODE_OPTIONS, etc.) when running a command; bypassing yarn would run scripts with the wrong environment.

## Symptoms

1. **No way to run yarn subcommands**: Tasks couldn't invoke `yarn install`, `yarn exec`, or other subcommands not present in package.json scripts.
2. **No argument passing**: Tasks couldn't pass flags or args to scripts without duplicating the full command in package.json.
3. **Root vs workspace ambiguity**: Once explicit commands were allowed, needed to distinguish root-level `yarn <cmd>` from workspace-scoped `yarn workspace <name> <cmd>`.

## Investigation Steps

1. **Architecture review**: Identified that CLI → Engine → Worker flow uses a cross-crate JSONL protocol. Adding yarn-specific logic to CLI/engine would violate separation of concerns — the yarn worker binary is the right place for yarn-prefix composition.

2. **Protocol extension**: Added optional `workspace` field to `WorkerRequest`. Considered three-state semantics: `None` (raw command), `Some("")` (root), `Some(name)` (workspace).

3. **Serde compatibility test failure**: Initial implementation broke `worker_request_json_uses_camel_case_fields` test because the new field appeared in JSON output. Solved with `#[serde(default, skip_serializing_if = "Option::is_none")]`.

4. **Shell-escaping pitfall**: POSIX shell requires single-quote escaping for workspace names interpolated into `sh -c`. Implemented `shell_single_quote` with `'\''` escaping.

5. **Raw-string literal gotcha**: Unit test assertions failed with normal string literals (`"'a'\''b'"` collapses `\'` to `'`). Fixed by using raw strings: `r"'a'\''b'"`.

6. **Concurrent cargo flake**: During parallel agent work, `cargo build` spuriously reported `can't find crate for 'thiserror'/'tokio'`. Clean re-run succeeded — concurrent cargo invocations against same target dir can race.

## Root Cause

The yarn worker had no protocol field to receive workspace context, and no logic to compose yarn prefixes. The CLI lacked the ability to signal "use yarn workspace" semantics vs raw shell execution.

## Solution

**Worker tasks: always run through yarn**

The worker must **always** invoke `yarn` — never resolve and execute a package.json script body directly. Yarn injects environment variables (PATH, NODE_OPTIONS, etc.) when running commands; bypassing yarn would run scripts with incorrect environment.

In `build_command_map` (run.rs), logic branches on worker vs non-worker:
- **WORKER tasks**: effective command = explicit non-blank `TaskDefinition.command`, else the **task name** (default). The `workspace` hint is **always set** (`Some("")` for root package, `Some(package.name)` otherwise). CLI does NOT read package.json script bodies for worker tasks — yarn resolves the script by name. Worker always runs `yarn workspace <ws> <command>` or `yarn <command>`.
- **NON-worker tasks**: unchanged (explicit command or package.json script body resolved by CLI; no workspace hint).

**1. Added three-state `workspace` field to `WorkerRequest`** (protocol.rs):

```rust
pub struct WorkerRequest {
    pub id: String,
    pub command: String,
    pub cwd: Option<String>,
    /// `None` => run `command` as raw shell command (generic behavior).
    /// `Some("")` => run `yarn <command>` at workspace root.
    /// `Some(name)` => run `yarn workspace <name> <command>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}
```

**2. Yarn worker binary composes prefix** (main.rs):

```rust
fn build_shell_command(workspace: Option<&str>, command: &str) -> String {
    match workspace {
        None => command.to_owned(),
        Some("") => format!("yarn {command}"),
        Some(workspace) => format!("yarn workspace {} {command}", shell_single_quote(workspace)),
    }
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
```

**3. CLI provides workspace hint** (run.rs):

For worker tasks, workspace is always set:

```rust
let workspace = if worker.is_some() {
    Some(package
        .filter(|pkg| pkg.path != workspace_root)
        .map(|pkg| pkg.name.to_string())
        .unwrap_or_default())
} else {
    None  // non-worker: CLI resolves script body; no workspace hint
};
```

**4. CLI never reads package.json script bodies for worker tasks**:

```rust
let effective_command = if worker.is_some() {
    // Worker: explicit command or task name (yarn resolves by name)
    task_def.and_then(|def| def.command.as_deref())
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(|c| c.to_string())
        .unwrap_or_else(|| task_name.to_string())
} else {
    // Non-worker: resolve package.json script or use explicit command
    // ... existing logic unchanged
};
```

## Why This Works

- **Environment injection**: Yarn injects PATH, NODE_OPTIONS, and other environment variables when running scripts. The worker must go through yarn to get correct environment.
- **Separation of concerns**: Yarn-specific logic stays in yarn-worker binary. CLI only provides a hint; generic worker model unchanged.
- **Backward compatibility**: `skip_serializing_if` ensures `None` produces identical JSON to pre-change output for non-worker tasks.
- **Root detection**: `pkg.path == workspace_root` reliably identifies root packages; emits `Some("")` for root, `Some(name)` for packages.
- **Blank command hardening**: Trim-and-check on `command` prevents whitespace-only commands from producing bare `yarn` invocations.
- **POSIX correctness**: Single-quote escaping prevents workspace names with spaces, quotes, or special chars from breaking shell parsing.

## Prevention Strategies

**Test Cases:**
- Add test for whitespace-only commands falling back to default behavior
- Add test for workspace names containing single quotes
- Add test asserting `None` workspace omits field from JSON
- **Worker e2e tests**: Use hermetic fake `yarn` shim in temp `bin/` dir, prepended to PATH. Shim echoes recognizable line (`yarn-ran workspace=<ws> script=<script> ...`) for assertions. Worker inherits luchta process env (including PATH) and runs via `sh -c`, so shim is found. Verifies full CLI→worker→shell path without real yarn install.

**Best Practices:**
- When adding optional fields to cross-crate serialized protocols, use `skip_serializing_if` to preserve exact-shape tests
- Use raw-string literals (`r"..."`) for test assertions containing backslash-escaped quotes
- Shell-escape any user-provided strings interpolated into `sh -c`
- For e2e tests invoking system commands, inject fake shims via PATH to avoid external dependencies

**Code Review Checklist:**
- [ ] Does new protocol field preserve backward compatibility?
- [ ] Are optional fields omitted from JSON when unset?
- [ ] Are interpolated strings shell-safe?
- [ ] Are test assertions using raw strings for escape sequences?

## Related Issues

- **GitHub:** [Issue #19](https://github.com/dobesv/luchta/issues/19) — Yarn worker command field
- **Related Solution:** [integration-issues/resident-worker-process-management-2026-06-09.md](../integration-issues/resident-worker-process-management-2026-06-09.md) — Worker protocol and lifecycle management
- **Related Solution:** [integration-issues/yarn-berry-lockfile-parser-2026-06-09.md](../integration-issues/yarn-berry-lockfile-parser-2026-06-09.md) — Yarn workspace protocol handling
