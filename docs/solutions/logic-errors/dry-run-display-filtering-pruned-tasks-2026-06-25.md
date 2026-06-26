---
title: "Suppress pruned/no-command tasks from dry-run output while preserving test-selection semantics"
date: 2026-06-25
category: logic-errors
problem_type: logic_error
component: luchta-cli/run.rs
root_cause: "Execution topology mixed with display rendering; dry-run repurposed as task-selection inspector in tests"
resolution_type: code_fix
severity: medium
tags:
  - dry-run
  - display-filtering
  - topology-preservation
  - test-selection-inspector
  - worker-protocol
  - ordering-connectors
plan_ref: dry-run-hide-pruned
issue: "#133"
---

## Problem

`luchta run --dry-run` displayed "pruned" no-command tasks (ordering-only connectors, tasks with `dependsOn` but no `worker`) alongside runnable tasks, cluttering output with rows that have no actual execution semantics. Users wanted these hidden, but the change broke tests that repurposed `--dry-run` as a task-selection inspector.

## Symptoms

- Dry-run output showed rows like `lib#build: (no script / no worker)` for dependency-only tasks
- A significant test suite used `--dry-run` to assert "which tasks were selected" rather than "what would run"
- After hiding no-command tasks, ~10-15 tests in `since_integration.rs` and `integration.rs` failed with `0 task(s) across 0 wave(s)`
- New test using `command: "/bin/sh"` deadlocked during dry-run (nextest SLOW >360s)

## Investigation Steps

1. Verified user request: issue #133 explicitly wants no-script/no-worker rows removed from dry-run
2. Checked prior work: issue #46 / PR #132 already removed "N task(s) pruned during resolution" note from `luchta check`
3. Tried hiding no-command tasks; integration tests failed — they define fixtures with `dependsOn` only (no worker)
4. Traced deadlock: fixture used `command: "/bin/sh"` which doesn't speak worker protocol → resident-worker resolution hung forever
5. Evaluated three options with user: (A) give fixtures real workers, (B) new `--inspect` flag, (C) show but don't hide
6. User chose Option A: update fixtures so selected tasks resolve to commands

## Root Cause

**Dual-purpose design conflict**: dry-run serves both (1) "show what would execute" and (2) "show what was selected". Fixtures in category (2) defined tasks without workers, so hiding no-command tasks collapsed their output to 0 waves.

**Worker fixture misunderstanding**: dry-run still performs worker resolution. Any config pointing at a worker must use a protocol-speaking script, not a bare shell.

**Topology/display coupling**: Original code mixed display rendering with execution semantics. `report_pruned_tasks` was a separate post-hoc filter rather than an integrated display-layer decision.

## Solution

### 1. Display-only filter layer

Added display filtering that runs AFTER topology computation:

```rust
// run.rs — compute_displayed_dry_run_waves
fn compute_displayed_dry_run_waves(
    task_graph: &TaskGraph,
    tasks_to_run: &TaskSet,
    commands: &HashMap<TaskId, CommandSpec>,
    invalid: &HashMap<TaskId, TaskConfigError>,
    selection: &Selection,
) -> Vec<Vec<(TaskId, String)>> {
    compute_execution_waves(task_graph, tasks_to_run)
        .into_iter()
        .filter_map(|wave| {
            let displayed: Vec<(TaskId, String)> = wave
                .into_iter()
                .filter_map(|task_id| {
                    describe_planned_action(&task_id, commands, invalid, selection)
                        .map(|desc| (task_id, desc))
                })
                .collect();
            if displayed.is_empty() { None } else { Some(displayed) }
        })
        .collect()
}
```

Single lookup per task: `describe_planned_action` returns `Option<String>`. `filter_map` + `is_some()` derives both visibility and description. Tasks hidden: no command and no config error (pruned/no-script/ordering connectors). Kept visible: runnable tasks (command+worker), config-error tasks, top-level (`-T`) root ordering tasks.

### 2. Root ordering-task labeling

Top-level root ordering tasks (created by `-T` without explicit worker) display as `(ordering task)` instead of being hidden:

```rust
// run.rs — describe_planned_action
if selection.top_level.contains(&task_id) && task_graph.is_root_task(&task_id) {
    return Some("(ordering task)".to_string());
}
```

### 3. Fixtures with real workers

Updated selection-inspector test fixtures to use protocol-speaking workers:

```yaml
# Before: task with no worker (hidden after fix)
tasks:
  lib#build:
    dependsOn: [setup]

# After: task resolves to a command (stays visible)
tasks:
  lib#build:
    dependsOn: [setup]
    worker: "sh"
workers:
  sh:
    command: "/bin/sh"  # WRONG — doesn't speak protocol
```

Fixed worker using helper:

```rust
// tests/common/mod.rs — write_task_config_with_named_worker
write_task_config_with_named_worker(dir, "sh", &shell_worker_script);
```

The shell worker answers `resolveTask` with `{"decision":"accept"}` and `run` with `done`.

### 4. Removed `report_pruned_tasks`

Deleted the post-hoc "pruned tasks" note entirely from dry-run path (aligned with issue #46 / PR #132 removal in `luchta check`).

## Why This Works

1. **Topology unchanged**: `compute_execution_waves` receives full `tasks_to_run`; dependency edges preserved. Waves renumbered only for display, not for scheduling.

2. **Single source of truth**: `describe_planned_action` returns `Option<String>` — visibility and description unified. No redundant map lookups, no bare-id inconsistency.

3. **Test semantics preserved**: Fixtures with real workers mean selected tasks have commands and stay visible. Selection assertions unchanged.

4. **Worker resolution works**: Protocol-speaking workers respond to `resolveTask`, so dry-run completes instead of hanging.

## Deliberate Divergence

Dry-run now drops empty waves and renumbers; live-run progress (`🌊 X/Y`) still numbers over full topology. Diverges only when an entire wave is no-command connectors. Left as a separately-tracked product decision (out of #133 scope).

## Prevention Strategies

**Test Cases:**
- `describe_planned_action_hides_non_root_no_command_task`: Returns `None` for hidden tasks
- `describe_planned_action_labels_top_level_root_ordering_task`: Returns `"(ordering task)"`
- `describe_planned_action_keeps_config_error_visible`: Config errors stay visible
- Selection-inspector fixtures: tasks have real workers/commands
- Dry-run worker fixture tests: use `shell_worker_body`/`make_worker_script` or `common::shell_worker`

**Best Practices:**
- Compute topology on full graph; filter for display only
- Single lookup for visibility+description (return `Option`)
- Audit tests that repurpose dry-run as selection inspector BEFORE changing display semantics
- Any worker reference must point at protocol-speaking script, even for dry-run
- Use existing worker fixture helpers (`shell_worker_body`, `make_worker_script`, `common::shell_worker`)

**Code Review Checklist:**
- [ ] Display filter runs AFTER topology computation?
- [ ] `describe_planned_action` returns `Option<String>` (single lookup)?
- [ ] Top-level root ordering tasks handled (labeled, not hidden)?
- [ ] New worker fixtures use protocol-speaking scripts?
- [ ] Tests using `--dry-run` for selection assertions have updated fixtures?

## Related Issues

- **GitHub:** [#133](https://github.com/dobesv/luchta/issues/133) — Hide pruned tasks from dry-run
- **GitHub:** [#46](https://github.com/dobesv/luchta/issues/46) — Remove pruned note from `luchta check`
- **PR:** [#132](https://github.com/dobesv/luchta/pull/132) — Pruned-note removal in check
- **Related Solution:** [noop-connector-task-exclusion-from-progress-stats-2026-06-22.md](noop-connector-task-exclusion-from-progress-stats-2026-06-22.md) — Live-run progress stats (topology/display separation)
- **Related Solution:** [since-filter-selection-and-gix-change-detection-2026-06-18.md](since-filter-selection-and-gix-change-detection-2026-06-18.md) — Dry-run as selection inspector
