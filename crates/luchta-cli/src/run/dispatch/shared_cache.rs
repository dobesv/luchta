use super::*;
use luchta_cache::CurrentState;

struct SharedCacheSkipInput<'a> {
    task_def: &'a TaskDefinition,
    current: &'a CurrentState<'a>,
    decision: &'a DecisionResult,
    local_record: Option<&'a TaskRunRecord>,
}

pub(in crate::run) struct PreparedSharedCacheHit {
    input_key: [u8; 32],
    package_path: PathBuf,
    restore: luchta_cache::shared::PreparedRestore,
}

pub(in crate::run) struct PreparedCacheDecision {
    pub(in crate::run) decision: Decision,
    pub(in crate::run) cache_write: Option<CacheWriteContext>,
    pub(in crate::run) shared_hit: Option<PreparedSharedCacheHit>,
}

impl PreparedCacheDecision {
    pub(super) fn run_without_context() -> Self {
        Self {
            decision: Decision::Run,
            cache_write: None,
            shared_hit: None,
        }
    }
}

fn prepare_cache_decision_context(
    task_id: &TaskId,
    ctx: &DecisionContext,
    no_cache: bool,
    cache_ctx: &mut CacheWriteContext,
) -> Option<PreparedSharedCacheHit> {
    let task_def = cache_ctx.task_def.clone();
    let cache_context = cache_read_state_context(task_id, ctx, cache_ctx)?;
    let cache_nonce = cache_ctx.cache_nonce.clone();
    let merged_env = match ctx.task_envs.get(task_id) {
        Some(env) => env,
        None => empty_task_env(),
    };
    let current = build_cache_current_state(CacheCurrentStateInput {
        task_def: &task_def,
        merged_env,
        nonce: cache_nonce.as_deref(),
        cache_context: &cache_context,
    });
    let local_record = ctx.cache.read(&task_id.to_string());
    let decision = decide(local_record.as_ref(), &current);
    cache_ctx.decision = cache_decision_from_result(&decision);
    let shared_hit = maybe_prepare_shared_cache_hit(
        ctx,
        no_cache,
        &cache_ctx.package_path,
        SharedCacheSkipInput {
            task_def: &task_def,
            current: &current,
            decision: &decision,
            local_record: local_record.as_ref(),
        },
        &cache_context.dep_outputs,
    );
    if no_cache {
        cache_ctx.decision = cache_run_decision();
    }
    shared_hit
}

fn cache_read_state_context(
    task_id: &TaskId,
    ctx: &DecisionContext,
    cache_ctx: &mut CacheWriteContext,
) -> Option<CacheStateContext> {
    let Some(cache_context) = cache_state_context(task_id, ctx) else {
        cache_ctx.decision = cache_run_decision();
        return None;
    };
    cache_ctx.dep_outputs = cache_context.dep_outputs.clone();
    Some(cache_context)
}

fn cache_decision_from_result(decision: &DecisionResult) -> CacheDecisionContext {
    CacheDecisionContext {
        action: decision.action,
        run_reason: decision.reason.clone(),
    }
}

fn maybe_prepare_shared_cache_hit(
    ctx: &DecisionContext,
    no_cache: bool,
    package_path: &Path,
    input: SharedCacheSkipInput<'_>,
    dep_outputs: &BTreeMap<String, [u8; 32]>,
) -> Option<PreparedSharedCacheHit> {
    if no_cache || !matches!(input.decision.action, Decision::Run) {
        return None;
    }

    try_shared_cache_prepare(
        ctx,
        input.task_def,
        package_path,
        input.current,
        dep_outputs,
        input.local_record,
    )
}

pub(super) fn prepare_cache_decision(
    task_id: &TaskId,
    ctx: &DecisionContext,
) -> PreparedCacheDecision {
    let Some(task_def) = ctx.task_graph.task_definition(task_id) else {
        return PreparedCacheDecision::run_without_context();
    };
    let nonce = ctx.resolve_task_nonce(task_def);
    let mut cache_ctx = match build_cache_write_context(task_id, ctx) {
        CacheInputState::Ready(cache_ctx) => *cache_ctx,
        CacheInputState::Disabled => return PreparedCacheDecision::run_without_context(),
    };
    cache_ctx.cache_nonce = nonce;

    let shared_hit = prepare_cache_decision_context(task_id, ctx, false, &mut cache_ctx);
    let decision = cache_ctx.decision.action;
    PreparedCacheDecision {
        decision,
        cache_write: matches!(decision, Decision::Run).then_some(cache_ctx),
        shared_hit,
    }
}

fn try_shared_cache_prepare(
    ctx: &DecisionContext,
    task_def: &TaskDefinition,
    package_path: &Path,
    current: &CurrentState<'_>,
    dep_outputs: &BTreeMap<String, [u8; 32]>,
    local_record: Option<&TaskRunRecord>,
) -> Option<PreparedSharedCacheHit> {
    let shared_cache = ctx.shared_cache.as_ref()?;
    if !outputs_lexically_in_package(&task_def.outputs) {
        return None;
    }

    // Resolve exactly once before network I/O. The lookup key must use the
    // task definition's declared patterns because record-carried detected
    // patterns are unavailable until after a candidate has been selected.
    // Validation reuses the same hash instead of walking the tree again.
    let prior_inputs: &[FileEntry] = local_record.map_or(&[], |record| record.inputs.as_slice());
    let resolved_inputs = current
        .resolver
        .resolve_inputs(current.declared_input_patterns, prior_inputs)
        .ok()?;
    let inputs_hash = combined_inputs_hash(&resolved_inputs);
    let input_key = derive_input_key(
        current.task_spec_hash,
        current.env_hash,
        current.pkg_dep_hash,
        combined_dep_outputs_hash(dep_outputs),
        inputs_hash,
    );

    let restore = shared_cache.prepare_restore(&input_key, package_path)?;
    if decide_shared_restore(restore.record(), current, inputs_hash) {
        return Some(PreparedSharedCacheHit {
            input_key,
            package_path: package_path.to_path_buf(),
            restore,
        });
    }

    let (candidate, _) = restore.into_parts();
    if let Err(error) = candidate.discard() {
        ctx.reporter
            .output()
            .stderr_line(&format!("warning: shared cache discard failed: {error}"));
    }
    None
}

pub(super) fn finalize_shared_cache_hit(
    task_id: &TaskId,
    prepared: PreparedSharedCacheHit,
    ctx: &DispatchContext<'_>,
) -> bool {
    let Some(shared_cache) = ctx.shared_cache.as_ref() else {
        return false;
    };
    let (candidate, snapshot_entry) = prepared.restore.into_parts();
    let hit = match candidate.commit() {
        Ok((hit, _)) => hit,
        Err(error) => {
            ctx.reporter.output().stderr_line(&format!(
                "warning: shared cache restore commit failed: {error}"
            ));
            return false;
        }
    };
    register_task_watch_state(
        &ctx.decision_ctx.task_watch_registry,
        task_id,
        task_id.package.clone(),
        prepared.package_path,
        &hit.record,
    )
    .expect("shared hit task watch registration should compile globs");
    hydrate_local_cache(
        ctx.cache.clone(),
        task_id.clone(),
        &hit,
        &ctx.reporter.output(),
    );
    if snapshot_entry.duration_trusted {
        shared_cache.refresh_entry(&prepared.input_key, &snapshot_entry);
    }
    record_output_hash(ctx.output_hashes, task_id, hit.outputs_hash);
    true
}
