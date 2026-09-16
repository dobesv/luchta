//! Cache records for ordering-only connector tasks.
//!
//! A connector has no worker and no non-blank command, so it never runs and has
//! nothing to cache. The record written here exists purely so `luchta why` can
//! explain it; see [`record_connector_state`].

use super::*;

/// Persist a connector's dependency state so `luchta why` can explain it.
///
/// A connector produces nothing, so this record exists only to answer "what
/// changed behind this task?" — walking down a chain of connectors names the
/// task that actually rebuilt. Nothing reads it back for a cache decision:
/// `resolve_dependency_outputs` always re-derives a connector's hash from the
/// tasks behind it, so a stale record here cannot cause a stale skip.
///
/// Written inline rather than on the blocking pool: a connector declares no
/// inputs or outputs to resolve, so the work is a dependency walk and one small
/// file. Downstream tasks read the in-memory hashes, never this record, so the
/// write order against `done_tx` does not matter.
pub(super) fn record_connector_state(task_id: &TaskId, ctx: &DispatchContext<'_>) {
    let Some(record) = build_connector_record(task_id, &ctx.decision_ctx) else {
        return;
    };
    if let Err(error) = ctx.cache.write(
        &task_id.to_string(),
        RunArtifacts {
            record: &record,
            stdout: &[],
            stderr: &[],
            reports: &[],
        },
    ) {
        ctx.reporter.output().stderr_line(&format!(
            "warning: failed to record state for ordering task '{task_id}': {error}"
        ));
    }
}

/// `None` when the connector's state is unchanged (the prior record still
/// describes it) or when there is not enough context to describe it at all.
fn build_connector_record(task_id: &TaskId, ctx: &DecisionContext) -> Option<TaskRunRecord> {
    let task_def = ctx.task_graph.task_definition(task_id)?.clone();
    let cache_nonce = ctx.resolve_task_nonce(&task_def);
    let cache_context = cache_state_context(task_id, ctx)?;
    let merged_env = ctx
        .task_envs
        .get(task_id)
        .unwrap_or_else(|| empty_task_env());
    let current = build_cache_current_state(CacheCurrentStateInput {
        task_def: &task_def,
        merged_env,
        nonce: cache_nonce.as_deref(),
        cache_context: &cache_context,
    });

    let prior = ctx.cache.read(&task_id.to_string());
    let decision = decide(prior.as_ref(), &current);
    if decision.action != Decision::Run {
        return None;
    }

    // Declared patterns are meaningless on a task that never runs, but resolving
    // them keeps `decide` from reporting a phantom input change on the next run
    // if a config does declare them.
    let inputs = current
        .resolver
        .resolve_inputs(&task_def.inputs, &[])
        .unwrap_or_default();
    let outputs = current
        .resolver
        .resolve_outputs(&task_def.outputs, &[])
        .unwrap_or_default();
    let now_unix_ms = now_unix_ms();

    Some(TaskRunRecord {
        schema_version: SCHEMA_VERSION_V5,
        task_spec_hash: current.task_spec_hash,
        input_patterns: task_def.inputs.clone(),
        inputs,
        output_patterns: task_def.outputs.clone(),
        outputs_hash: combined_outputs_hash(&outputs),
        outputs,
        detected_input_patterns: false,
        detected_output_patterns: false,
        env_hash: current.env_hash,
        pkg_dep_hash: current.pkg_dep_hash,
        dep_outputs: cache_context.dep_outputs.clone(),
        exit_status: 0,
        succeeded: true,
        start_unix_ms: now_unix_ms,
        end_unix_ms: now_unix_ms,
        reports: Vec::new(),
        cache_nonce,
        run_reason: Some(decision.reason),
    })
}
