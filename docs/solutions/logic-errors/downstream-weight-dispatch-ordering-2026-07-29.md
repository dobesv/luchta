---
title: "Downstream-weight dispatch ordering for task walker"
date: 2026-07-29
category: logic-errors
problem_type: logic_error
component: luchta-engine/walker
root_cause: "Ready tasks emitted in nondeterministic HashMap iteration order instead of priority order"
resolution_type: code_fix
severity: low
tags:
  - dispatch-priority
  - task-graph
  - topological-order
  - scheduling-hint
plan_ref: weight-priority-dispatch
---

## Problem

Ready-to-dispatch tasks were emitted from the walker in nondeterministic `HashMap` key order rather than prioritized order. Tasks that unlock the most downstream work should dispatch first to maximize parallelism, but all ready tasks were treated equally.

## Symptoms

- Deterministic builds could emit tasks in different orders across runs (HashMap nondeterminism)
- No consideration of "unlock potential" when multiple tasks become ready simultaneously
- Difficult to reason about dispatch order in tests or logs

## Root Cause

`WalkerState::take_ready_nodes` collected ready nodes from `self.nodes.keys()` — a `HashMap<NodeIndex, TaskNode>`. HashMap iteration order is unspecified, so dispatch order was effectively random among concurrently-ready tasks.

## Solution

Implemented downstream-weight priority sorting for ready tasks:

### 1. Precompute downstream weights once at Walker construction

Added pure helper function that computes cumulative downstream weight (own weight + sum of all transitive dependents' weights) in a single O(V+E) pass:

```rust
fn compute_downstream_weights(
    nodes: &HashMap<NodeIndex, TaskNode>,
    dependents: &HashMap<NodeIndex, Vec<NodeIndex>>,
    order: &[NodeIndex],
) -> HashMap<NodeIndex, u64> {
    let mut downstream = HashMap::with_capacity(nodes.len());
    for node_index in order {
        let weight = nodes
            .get(node_index)
            .map(|n| n.weight as u64)
            .unwrap_or(0);
        let dependent_sum: u64 = dependents
            .get(node_index)
            .into_iter()
            .flatten()
            .filter_map(|d| downstream.get(d).copied())
            .sum();
        downstream.insert(*node_index, weight + dependent_sum);
    }
    downstream
}
```

Key: `order` must iterate **dependents before dependencies** so each node's dependents are already computed. `TaskGraph::topological_order()` returns dependencies-first, so reverse it.

### 2. Sort ready nodes by priority key

In `take_ready_nodes`, after collecting ready nodes:

```rust
ready.sort_by(|left, right| {
    let left_node = self.nodes.get(left).expect("ready node missing");
    let right_node = self.nodes.get(right).expect("ready node missing");
    (
        Reverse(self.downstream_weights.get(left).copied().unwrap_or_default()),
        Reverse(left_node.weight),
        &left_node.id.package.0,
        &left_node.id.task.0,
    )
        .cmp(&(
            Reverse(self.downstream_weights.get(right).copied().unwrap_or_default()),
            Reverse(right_node.weight),
            &right_node.id.package.0,
            &right_node.id.task.0,
        ))
});
```

Sort key (descending priority):
1. `downstream_weight` desc (tasks unlocking more work first)
2. `own weight` desc (heavier tasks first when downstream tied)
3. `id.package.0` asc (deterministic tie-break)
4. `id.task.0` asc (final tie-break)

## Why This Works

**Reverse topological order ensures availability:** When computing `downstream[n]`, all dependents `d` have already been visited because we iterate from sinks toward roots. Each `downstream[d]` is complete when we need it.

**Diamond double-counting is acceptable:** In a diamond (A depends on B and C, both depend on D), D's weight gets counted twice in A's downstream weight. This is an intentional approximation — a scheduling hint, not an exact unlock metric. Simpler and cheaper than set-union, and tie-breaking handles the rest.

**Inline closure avoids blast radius:** Sorting uses an inline `sort_by` closure with `std::cmp::Reverse` for descending keys. **Do NOT derive `Ord` on TaskId/PackageName/TaskName** — that would leak lexical ordering everywhere and risk changing `BTreeMap`/`BTreeSet` behavior.

## Learnings

1. **`TaskGraph::topological_order()` returns dependencies-first:** To iterate dependents-first (sinks toward roots), reverse the result. The weight recurrence requires dependents already computed.

2. **Diamond double-counting is a feature, not a bug:** Shared ancestors get counted multiple times. This is simpler than set-union and acceptable because downstream-weight is a priority hint, not an exact metric.

3. **No Ord derive on identifier types:** Deriving `Ord` on `TaskId`/`PackageName`/`TaskName` would affect all `BTreeMap`/`BTreeSet` usage. Inline `sort_by` closures limit impact to exactly where ordering is needed.

4. **Pure helper for unit testing:** `compute_downstream_weights` takes plain maps (`HashMap<NodeIndex, TaskNode>`, `HashMap<NodeIndex, Vec<NodeIndex>>`, `&[NodeIndex]`), not `&TaskGraph`. Makes the algorithm unit-testable in isolation without async or graph construction.

5. **Weight set via TaskDefinition:** No direct `TaskNode` constructor exists — all construction goes through `TaskGraph::build`. Weighted fixtures set `TaskDefinition { weight, ..default() }` per package.

6. **Dispatch priority ≠ semaphore acquisition:** Sorting affects dispatch order only. Executor's `semaphore.acquire_many_owned(weight)` is unchanged. A prioritized heavy task at channel head can still block lighter ready tasks behind it (head-of-line blocking). This is a known limitation deferred to separate resource-aware-scheduling work.

## Prevention Strategies

**Test Cases:**
- Direct unit test of `compute_downstream_weights` on plain maps with diamond graph, asserting expected values including double-counting
- Integration test: equal-weight ready tasks with differing downstream weights → higher-downstream dispatches first
- Integration test: equal downstream weights → deterministic package/task name tie-break
- Integration test: task with heavier transitive dependents prioritized over equal-own-weight task

**Code Review Checklist:**
- [ ] Does topological iteration go in the correct direction (dependents-first vs dependencies-first)?
- [ ] Is the weight recurrence correct: `own + Σ downstream[dependent]`?
- [ ] Is double-counting documented as intentional approximation?
- [ ] Is tie-breaking deterministic (all keys fully specified, no random/HashMap order)?
- [ ] Did you avoid deriving `Ord` on identifier types?

**Related Issues:**
- GitHub: [#271](https://github.com/dobesv/luchta/issues/271) — Prefer higher-weight tasks first (downstream-weight dispatch ordering)
- Related: [weight-clamp-config-validation-paths-2026-06-27.md](weight-clamp-config-validation-paths-2026-06-27.md) — Weight clamping and semaphore (context only; executor unchanged here)
