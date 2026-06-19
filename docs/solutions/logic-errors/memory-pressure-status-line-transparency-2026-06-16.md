---
title: "Memory-pressure status-line transparency fix"
date: 2026-06-16
category: logic-errors
problem_type: logic_error
component: luchta-cli/memory-pressure
root_cause: "Control-decision value and displayed value came from separate RSS measurement paths; sysinfo retained dead processes and per-thread /proc children undercounted"
resolution_type: code_fix
severity: high
tags:
  - memory-pressure
  - rss
  - status-line
  - transparency
  - control-display-mismatch
  - sysinfo
  - procfs
  - process-tree
plan_ref: luchta-mem-pressure-rss
last_updated: 2026-06-16
---

## Problem

Memory-pressure warnings showed an RSS value that didn't match the value triggering the warning, and status lines stopped printing during pause, making the build appear hung.

## Symptoms

- `luchta run build` showed "❗ mem usage high" with displayed 🐏 RSS far below the 50%-of-RAM threshold (4.1 GB displayed on a 64 GB host)
- Status lines stopped printing during memory-pressure pause; build appeared to hang indefinitely
- User couldn't tell whether memory usage was actually high or what threshold triggered pause

## Investigation Steps

1. Traced warning trigger: `MemoryMonitor::check()` uses `compute_tree_rss` via sysinfo's `process.parent()` adjacency, stores in `MemorySample.tree_rss`, compares to `usage_threshold`
2. Traced displayed 🐏: `crate::rss::process_tree_rss_bytes()` uses `/proc/<pid>/task/<pid>/children` walk — different mechanism
3. Built standalone Rust probe comparing sysinfo parent-walk vs /proc children-walk:
   - Both methods agreed (1.00x) for simple subprocess tree spawned from worker thread
   - Proves methods CAN agree but aren't guaranteed to for arbitrary topologies
4. Confirmed sysinfo 0.36 does NOT fold threads into process table unless `ProcessRefreshKind::tasks()` enabled (not used here) — ruled out thread-RSS multiplication
5. Found `render_status_line` only printed when `running_count() > 0` — paused state holds next task, so running_count eventually hits 0

## Root Cause

**Cause 1 (transparency)**: Two separate RSS measurements. Pause decision used `MemorySample.tree_rss` from sysinfo's parent-adjacency walk. Displayed 🐏 used `/proc/<pid>/task/<tid>/children` walk. Different process-tree discovery mechanisms can diverge, so displayed value didn't match the value that triggered the warning.

**Cause 2 (silent hang)**: `render_status_line` gated on `running_count() > 0`. While paused for memory pressure, the next task is HELD (not dispatched). Once the last in-flight task finished, running_count hit 0 and no periodic status line emitted — appeared to hang though waiting forever by design (issue #37).

Lesson: A value used for a control decision and the value shown to the user must come from a single source of truth.

## Solution

**Unified RSS source**: `PressureState` now stores an atomic snapshot (`PressureSnapshot`) containing:
- Latest `MemorySample` from `MemoryMonitor::check()`
- Reasons (`UsageHigh`, `FreeLow`)
- Resolved `usage_threshold` and `free_threshold` byte values

All from one `RwLock` read.

**Transparency in warning suffix**: Status line renders 🐏 from `snapshot.sample.tree_rss` (same value compared to threshold). Warning shows measured vs threshold:
- `❗ mem usage high (<rss> / <usage_threshold>)` e.g., `(32.1 / 30.97 GB)`
- `❗ system free memory low (<available> / <free_threshold>)`

**Paused progress rendering**: `render_status_line` prints while paused even with zero running tasks. Gated via pure helper:
```rust
fn should_render(paused: bool, running_count: usize, mode: RenderMode) -> bool {
    paused || running_count > 0 || mode != RenderMode::Normal
}
```

**RSS fallback**: `crate::rss` /proc walk retained only for pre-first-sample fallback in terminal summary and interrupt paths.

## Why This Works

- Single source of truth for control and display eliminates user confusion about threshold mismatch
- Paused status lines give visibility into indefinite wait per issue #37 design intent
- Thresholds in warning let users verify threshold configuration matches expectations
- Atomic snapshot under one lock prevents telemetry-control race conditions

## Prevention Strategies

- [ ] Control decision values and displayed values must use same data source
- [ ] Status-line rendering must account for paused/waiting states, not just running count
- [ ] Periodic output during indefinite waits is user-facing contract, not optional
- [ ] Test matrix: `should_render(paused, running_count, mode)` covers all combos
- [ ] When multiple measurement paths exist, unify or document divergence risk

## Why the RSS was wrong: sysinfo corpse retention + per-thread /proc children

After unification made display==decision, the build's reported tree RSS was still wildly wrong — **~12x inflated** (44–49 GB vs true ~3.6 GB) and **frozen** for 100+ seconds even with zero build tasks running. A frozen, inflated RSS exceeding the host's entire memory footprint is the tell-tale sign of counting stale/dead processes.

### Root Cause A: sysinfo retained dead processes

`MemoryMonitor` reused one long-lived `sysinfo::System` and called `refresh_processes_specifics(ProcessesToUpdate::All, true, with_memory())` each tick, then summed parent→children adjacency. sysinfo 0.36's `switch_updated()` + `retain` dead-process eviction does NOT reliably remove thousands of short-lived build workers, so the process table accumulates "corpses" whose stale RSS keeps being summed.

**Direct evidence**: a retained `System` reported 171 MB for a single dead 150 MB child, while a fresh `/proc` read reported correct 2.9 MB.

### Fix A: stateless /proc descendant walk

Replace sysinfo process-table RSS with a **stateless /proc descendant walk** each call:
- Read `/proc/<pid>/status` VmRSS for the process's own RSS
- Discover children via `/proc/<pid>/task/<tid>/children`
- No retained process table = no corpses

Keep sysinfo **only** for `total_memory()` / `available_memory()` (those were correct).

### Root Cause B: /proc children is per-thread

`/proc/<pid>/task/<TID>/children` lists only children spawned by thread TID (proc(5) documents this). Reading just `/proc/<pid>/task/<pid>/children` (main thread) **misses children spawned from other threads**. luchta spawns workers from tokio worker threads, so this under-counted.

### Fix B: union all threads' children

```rust
fn discover_children(pid: u32) -> Vec<u32> {
    let mut children = std::collections::HashSet::new();
    let task_dir = format!("/proc/{}/task", pid);
    for entry in std::fs::read_dir(&task_dir).ok()?.flatten() {
        let tid = entry.file_name().to_string_lossy().parse::<u32>().ok()?;
        let children_path = format!("/proc/{}/task/{}/children", pid, tid);
        if let Ok(contents) = std::fs::read_to_string(&children_path) {
            for line in contents.lines() {
                if let Ok(child) = line.trim().parse::<u32>() {
                    children.insert(child);
                }
            }
        }
    }
    children.into_iter().collect()
}
```

### Diagnosis tactic

A host-shell script (`luchta-memtrace.sh`, not committed) ran the build under timeout, capturing every 5s:
1. Strict `/proc` descendant walk of real luchta PID
2. System-wide node RSS
3. `/proc/meminfo` MemAvailable

This proved luchta's number (44 GB) matched neither the true tree (3.6 GB) nor the host total (~20 GB) → straight at stale-corpse accumulation.

**Note**: Sandboxed/PID-namespaced probes can't reproduce this — must trace on the real host.

### Testing tactic

Regression test must:
1. **Spawn child from non-main thread** to exercise root cause B:
   ```rust
   std::thread::spawn(|| Command::new("sleep").arg("30").spawn()).join()
   ```
   Main-thread spawn would pass even with buggy main-TID-only walk.
2. Assert **PID membership** in descendant set (present while alive, absent after reap) rather than strict byte-delta — parent's own RSS moves between samples making byte-delta assertions flaky.

### Gotchas ruled out

- **NOT a unit bug**: sysinfo 0.36 `Process::memory()` returns RSS in **bytes** (statm field[1] × page_size), not VSZ and not KiB. Verified.
- **NOT threshold math**: 50% usage threshold and 1/16 free threshold resolved correctly.
- Neither math nor units at fault — only the process-set being summed.

## Related Issues

- **GitHub:** [#37](https://github.com/dobesv/luchta/issues/37) — Memory pressure handling (parent feature)
- **Files changed:** `crates/luchta-cli/src/memory_pressure.rs`, `progress.rs`, `run.rs`, `run/pause.rs`, `run/setup.rs`, `rss.rs`
- **Related Solution:** [wave-bucketed-progress-reporter-2026-06-13.md](../workflow-issues/wave-bucketed-progress-reporter-2026-06-13.md) — Original ProgressReporter design
- **Related Solution:** [task-name-grouped-progress-rendering-2026-06-16.md](../workflow-issues/task-name-grouped-progress-rendering-2026-06-16.md) — Progress rendering refactoring
