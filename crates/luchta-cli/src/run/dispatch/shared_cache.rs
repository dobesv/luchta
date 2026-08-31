use super::*;
use luchta_cache::{path_matches_resolve_requests, CurrentState, ResolveRequest};

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

pub(in crate::run) struct PreparedAdvisoryCacheFiles {
    package_path: PathBuf,
    patterns: Vec<String>,
    restore: luchta_cache::shared::PreparedCacheFiles,
}

struct StagedCacheFileValidation<'a> {
    ctx: &'a DecisionContext,
    task_id: &'a TaskId,
    package_path: &'a Path,
    task_def: &'a TaskDefinition,
    current: &'a CurrentState<'a>,
    local_record: Option<&'a TaskRunRecord>,
    restore: &'a luchta_cache::shared::PreparedCacheFiles,
}

pub(in crate::run) struct PreparedCacheDecision {
    pub(in crate::run) decision: Decision,
    pub(in crate::run) cache_write: Option<CacheWriteContext>,
    pub(in crate::run) shared_hit: Option<PreparedSharedCacheHit>,
    pub(in crate::run) cache_files: Option<PreparedAdvisoryCacheFiles>,
}

impl PreparedCacheDecision {
    pub(super) fn run_without_context() -> Self {
        Self {
            decision: Decision::Run,
            cache_write: None,
            shared_hit: None,
            cache_files: None,
        }
    }
}

fn prepare_cache_decision_context(
    task_id: &TaskId,
    ctx: &DecisionContext,
    no_cache: bool,
    cache_ctx: &mut CacheWriteContext,
) -> (
    Option<PreparedSharedCacheHit>,
    Option<PreparedAdvisoryCacheFiles>,
) {
    let task_def = cache_ctx.task_def.clone();
    let Some(cache_context) = cache_read_state_context(task_id, ctx, cache_ctx) else {
        return (None, None);
    };
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
    let cache_files = if shared_hit.is_none() && matches!(decision.action, Decision::Run) {
        maybe_prepare_advisory_cache_files(
            ctx,
            no_cache,
            task_id,
            &task_def,
            &cache_ctx.package_path,
            &current,
            &cache_context.dep_outputs,
            local_record.as_ref(),
        )
    } else {
        None
    };
    (shared_hit, cache_files)
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

    let (shared_hit, cache_files) =
        prepare_cache_decision_context(task_id, ctx, false, &mut cache_ctx);
    let decision = cache_ctx.decision.action;
    PreparedCacheDecision {
        decision,
        cache_write: matches!(decision, Decision::Run).then_some(cache_ctx),
        shared_hit,
        cache_files,
    }
}

#[allow(clippy::too_many_arguments)]
fn maybe_prepare_advisory_cache_files(
    ctx: &DecisionContext,
    no_cache: bool,
    task_id: &TaskId,
    task_def: &TaskDefinition,
    package_path: &Path,
    current: &CurrentState<'_>,
    dep_outputs: &BTreeMap<String, [u8; 32]>,
    local_record: Option<&TaskRunRecord>,
) -> Option<PreparedAdvisoryCacheFiles> {
    if no_cache || task_def.cache_files.is_empty() {
        return None;
    }
    let shared_cache = ctx.shared_cache.as_ref()?;

    // Local state is authoritative as one coherent set: one matching file is
    // enough to suppress all shared restoration.
    let local_cache_files = resolve_outputs(package_path, &task_def.cache_files).ok()?;
    if local_cache_files.iter().any(|entry| !entry.absent) {
        return None;
    }

    let scope = derive_cache_file_scope(
        &task_id.to_string(),
        current.task_spec_hash,
        current.env_hash,
        current.pkg_dep_hash,
    );
    let restore =
        shared_cache.prepare_cache_files(luchta_cache::shared::CacheFileRestoreRequest {
            scope_hash: &scope,
            upstream_outputs_hash: combined_dep_outputs_hash(dep_outputs),
            package_dir: package_path,
            patterns: &task_def.cache_files,
        })?;

    if !staged_cache_files_are_disjoint(StagedCacheFileValidation {
        ctx,
        task_id,
        package_path,
        task_def,
        current,
        local_record,
        restore: &restore,
    }) {
        let _ = restore.discard();
        ctx.reporter.output().stderr_line(&format!(
            "warning: shared cache-file restore for task '{task_id}' overlaps resolved inputs or outputs; running cold"
        ));
        return None;
    }

    Some(PreparedAdvisoryCacheFiles {
        package_path: package_path.to_path_buf(),
        patterns: task_def.cache_files.clone(),
        restore,
    })
}

fn staged_cache_files_are_disjoint(validation: StagedCacheFileValidation<'_>) -> bool {
    let input_requests = match expand_input_patterns(
        &validation.task_def.inputs,
        &validation.task_id.package,
        &validation.ctx.package_graph,
        &validation.ctx.workspace_root,
    ) {
        Ok(requests) => requests,
        Err(_) => return false,
    };
    let output_requests = validation
        .task_def
        .outputs
        .iter()
        .map(|pattern| ResolveRequest::new(validation.package_path.to_path_buf(), pattern))
        .collect::<Vec<_>>();
    let prior_inputs = validation
        .local_record
        .map_or(&[][..], |record| record.inputs.as_slice());
    let inputs = match validation
        .current
        .resolver
        .resolve_inputs(validation.current.declared_input_patterns, prior_inputs)
    {
        Ok(inputs) => inputs,
        Err(_) => return false,
    };
    let outputs = match resolve_outputs(validation.package_path, &validation.task_def.outputs) {
        Ok(outputs) => outputs,
        Err(_) => return false,
    };
    let occupied = inputs
        .iter()
        .map(|entry| validation.ctx.workspace_root.join(&entry.path))
        .chain(
            outputs
                .iter()
                .map(|entry| validation.package_path.join(&entry.path)),
        )
        .collect::<HashSet<_>>();
    validation.restore.relative_paths().iter().all(|path| {
        let absolute = validation.package_path.join(path);
        !occupied.contains(&absolute)
            && path_matches_resolve_requests(&absolute, &input_requests)
                .is_ok_and(|matched| !matched)
            && path_matches_resolve_requests(&absolute, &output_requests)
                .is_ok_and(|matched| !matched)
    })
}

pub(super) fn finalize_advisory_cache_files(
    task_id: &TaskId,
    prepared: PreparedAdvisoryCacheFiles,
    ctx: &DispatchContext<'_>,
) {
    // Recheck local precedence at the commit boundary. A concurrent process or
    // an upstream task may have produced local warm state after preparation.
    if resolve_outputs(&prepared.package_path, &prepared.patterns).is_ok_and(|entries| {
        entries
            .iter()
            .any(|entry| !entry.absent && !prepared.restore.is_staged_path(Path::new(&entry.path)))
    }) {
        let _ = prepared.restore.discard();
        return;
    }
    if let Err(error) = prepared.restore.commit() {
        ctx.reporter.output().stderr_line(&format!(
            "warning: shared cache-file restore commit failed for task '{task_id}': {error}; running cold"
        ));
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
