---
title: "Exclude no-op ordering-only connector tasks from progress stats without collapsing wave topology"
date: 2026-06-22
category: logic-errors
problem_type: logic_error
component: luchta-cli/progress-reporter
root_cause: "No-op connector tasks counted in progress totals; early filtering collapsed wave topology"
resolution_type: code_fix
severity: medium
tags:
  - progress-reporting
  - wave-scheduling
  - topology-preservation
  - dry-run
  - connector-tasks
plan_ref: issue-98-noop-stats
---

## Problem

`luchta run` status line counted no-op connector tasks (TaskDefinition with no `worker` and no command) in grand total, per-wave totals, done, and pending counters. Fast all-skipped builds still showed thousands of "tasks" (e.g., `2199/2199`). Ordering-only tasks exist purely to wire dependencies together and should not pollute work-item counts.

## Symptoms

- Status line inflated: `✓ 2199/2199 done · 0 skipped` for builds where most "tasks" were ordering connectors
- Wave progress (`🌊 X / Y`) showed fewer waves during dry-run vs live run after initial fix attempt
- Sequential tasks like `build -> noop_connector -> test` collapsed into same wave, losing stage granularity

## Investigation Steps

1. Traced status counters through `ProgressReporter::render_progress()` — total derived from `wave_of` HashMap size
2. Found `compute_wave_indices()` built wave map over ALL selected tasks regardless of whether they represent work
3. Dispatch path routed no-command nodes through `task_ran()` (Done counter) instead of skipping them
4. First fix attempt: filter no-op tasks OUT of graph BEFORE computing longest-path wave depth
5. Observed dry-run showing 4 waves (correct topology) while live run showed 1 wave (collapsed)
6. Root cause: filtering before depth calculation severed dependency chains through noop connectors

## Root Cause

Two interrelated bugs:

1. **Counting bug**: `compute_wave_indices` included all selected tasks in `wave_of`, and dispatch's no-command branch marked nodes as `task_ran` (done). Pure ordering connectors contributed to totals.

2. **Topology collapse bug** (first-fix pitfall): Filtering `tasks_to_run` to "counted" subset before calling `compute_longest_path_waves()` removed connector nodes from depth calculation. A chain `build -> noop_connector -> test` had noop removed, causing `build` and `test` to share the same wave (Wave 0). Dry-run used unfiltered full graph, showing 4 waves; live run used filtered graph, showing 1 wave.

## Solution

### 1. Single source of truth for "counts toward stats"

Added `TaskDefinition::counts_in_progress()` in `luchta-types`:

```rust
/// Counts toward runtime progress/stat totals when task represents runnable
/// work (`worker`) or selected misconfiguration (`command` without worker).
/// Pure ordering connectors — no worker and no non-blank command — stay out
/// of counted stats even though they still participate in wave topology.
#[must_use]
pub fn counts_in_progress(&self) -> bool {
    self.worker.is_some()
        || self
            .command
            .as_deref()
            .map(str::trim)
            .is_some_and(|command| !command.is_empty())
}
```

A task counts in stats if:
- Has a `worker` (normal runnable task), OR
- Has a non-blank `command` without a `worker` (config error that must be tracked/reported)

Pure connectors (no worker, no command) do NOT count.

### 2. Compute waves on FULL graph, THEN filter stats

```rust
/// Compute runtime progress wave indices from full selected topology, then drop
/// uncounted tasks from returned stats map.
///
/// Longest-path depth is resolved over whole selected subgraph so ordering-only
/// connectors preserve real stage boundaries. Returned `wave_of` only includes
/// tasks that count toward runtime stats/progress via
/// `TaskDefinition::counts_in_progress()`. `total_waves` keeps full-topology wave
/// count, including waves that become empty after filtering, so runtime `🌊 X / Y`
/// stays aligned with dry-run numbering and can still reach `Y / Y` at completion.
fn compute_wave_indices(
    task_graph: &TaskGraph,
    tasks_to_run: &HashSet<TaskId>,
) -> (HashMap<TaskId, usize>, usize) {
    // Step 1: Compute depth over FULL selected subgraph (preserves topology)
    let (full_wave_of, total_waves) = compute_longest_path_waves(task_graph, tasks_to_run);
    
    // Step 2: Filter to counted tasks only (for stats map)
    let counted_wave_of = full_wave_of
        .into_iter()
        .filter(|(task_id, _)| {
            task_graph
                .task_definition(task_id)
                .is_some_and(TaskDefinition::counts_in_progress)
        })
        .collect();

    (counted_wave_of, total_waves)
}
```

Key insight: `total_waves` stays at full-topology count. Waves that contain only uncounted connectors are handled in `ProgressReporter::render_progress()` by treating `wave_total == 0` as already-complete.

### 3. Dispatch routes no-command through Uncounted path

No-command nodes now route through `task_finished_uncounted()` instead of `task_ran()`:

```rust
// dispatch.rs — no-command branch
ctx.reporter.task_finished_uncounted(&task_id);
```

`TaskOutcome::Uncounted` does NOT increment done/skipped/failed counters.

## Why This Works

1. **Topology preserved**: `compute_longest_path_waves()` receives full `tasks_to_run` set, so connectors participate in depth calculation. `build -> noop -> test` correctly yields 4 waves, not 1.

2. **Stats reflect work, not structure**: Filtering happens AFTER wave depth calculation. The `counted_wave_of` map excludes pure connectors from runtime counters.

3. **Dry-run and live-run alignment**: Both use `compute_longest_path_waves()` on full graph. Dry-run lists everything selected (connectors included) via `describe_planned_action()`; stats count only real work.

4. **Wave completion still works**: Empty waves (all tasks uncounted) have `wave_total == 0`. Progress renderer treats these as complete, so `🌊` can still reach `Y / Y`.

## General Principle

**Keep "what to DISPLAY/count" separate from "what defines the structure/topology."**

Filter the derived view, not the structural input. Wave topology derives from the full dependency graph. Progress stats derive from a filtered projection of that topology.

Premature filtering severs dependency edges and collapses the structure, causing:
- Wave count mismatch between dry-run and live execution
- Lost stage granularity in progress reporting
- User confusion when planned waves ≠ executed waves

## Prevention Strategies

**Test Cases:**
- `compute_wave_indices_excludes_no_worker_tasks_and_empty_waves`: Verify noop connectors excluded from `wave_of` but `total_waves` reflects full topology
- `compute_wave_indices_counts_command_without_worker_config_error`: Verify config-error tasks (command without worker) ARE counted
- `wave_progress_preserves_topology_across_noop_connectors`: `build -> noop -> test` shows 4 waves, not 1
- `dry_run_and_live_run_show_same_wave_count`: Alignment between `--dry-run` output and runtime `🌊 X / Y`
- `all_uncounted_selection_shows_zero_counters`: Selection containing only ordering connectors shows `0 done, 0 skipped`

**Best Practices:**
- Centralize "counts toward progress" logic in one place (`TaskDefinition::counts_in_progress()`)
- Compute topology on full graph, then filter for display/counting
- Treat topology computation and stats filtering as separate concerns
- Ensure dry-run and live-run use same structural algorithms

**Code Review Checklist:**
- [ ] Wave topology computed on full selected subgraph, not filtered subset?
- [ ] No-op connectors excluded from progress counters?
- [ ] Config-error tasks (command without worker) still counted?
- [ ] Dry-run wave structure matches live-run wave indicator?
- [ ] `total_waves` reflects full topology, not counted subset?

## Related Issues

- **GitHub:** [#98](https://github.com/dobesv/luchta/issues/98) — Make sure status outputs not counting no-op tasks with no worker
- **Related Solution:** [workflow-issues/wave-bucketed-progress-reporter-2026-06-13.md](../workflow-issues/wave-bucketed-progress-reporter-2026-06-13.md) — Original ProgressReporter design
- **Related Solution:** [logic-errors/cache-persistence-decoupling-worker-protocol-2026-06-19.md](cache-persistence-decoupling-worker-protocol-2026-06-19.md) — Sink installation for all tasks
