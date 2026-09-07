---
title: "Literal task + --since filter: validate existence before applying affected-set intersection"
date: 2026-09-07
last_verified: 2026-09-07
component: luchta-cli
problem_type: logic_error
status: current
anchors:
  - crates/luchta-cli/src/list.rs:select_task_ids
  - crates/luchta-cli/src/list.rs:passes_since
  - crates/luchta-cli/src/run.rs:collect_requested_subgraph
  - crates/luchta-cli/src/run.rs:validate_literal_task_requests
tags:
  - task-selection
  - since-filter
  - literal-task
  - affected-packages
plan_ref: luchta-list-since-packages
---

## Problem

When a literal task request (e.g., `luchta list build --since HEAD`) combines with an
affected set that excludes all matching task ids, the command errors with
"task '<x>' not found in task graph" instead of returning an empty result.

This happens when the since filter is passed *into* `collect_requested_subgraph`,
which calls `validate_literal_task_requests` after filtering. A valid literal task
that happens to be outside the affected set appears unmatched.

## When this is relevant

- Adding `--since` support to a new command that accepts literal task names
- Modifying `collect_requested_subgraph` or `validate_literal_task_requests`
- Debugging "task not found" errors that only occur with `--since`

## Durable lesson

The since filter must be applied **after** existence validation, not interleaved:

1. Validate literal tasks against the full task graph (pass `since_affected: None`
   to `collect_requested_subgraph`)
2. Apply the since intersection as a post-filter on the returned ids

This preserves typo-guarding (genuinely missing tasks still error) while keeping
the affected set as a pure intersection: existing literal tasks outside the set
return empty, not an error.

## Evidence and current anchors

The fix in `list.rs:select_task_ids` (branch 4, lines 209-219):

```rust
let selected = collect_requested_subgraph(CollectSubgraphRequest {
    task_graph,
    selection,
    pruned,
    since_affected: None,  // validate against full graph
    expand_dependencies: false,
})?;
Ok(selected
    .into_iter()
    .filter(|task_id| passes_since(task_id, since_affected))  // post-filter
    .collect())
```

The `passes_since` helper (line 222-224) encodes the rule:

```rust
fn passes_since(task_id: &TaskId, since_affected: Option<&HashSet<PackageName>>) -> bool {
    task_id.is_root() || since_affected.is_none_or(|affected| affected.contains(&task_id.package))
}
```

Root tasks always bypass the since filter (matches `run.rs:package_matches`).

## Latent issue in run.rs

The same quirk exists in `run.rs:collect_requested_subgraph`, which passes
`since_affected` through to `validate_literal_task_requests`. The `run`
subcommand has not yet been fixed, so `luchta run <task> --since <ref>` may
error on an empty affected set when a literal task is specified. Fixing `run`
is a separate task (not in scope for the list commands work).

## Prevention

When wiring `--since` into any code path that validates literal task requests:

- Apply `passes_since` as a post-filter, not as input to `collect_requested_subgraph`
- Test: `cmd <literal-task> --since <empty-affected-ref>` should return empty, exit 0
- Test: `cmd nonexistent --since <ref>` should still error (typo guard preserved)
