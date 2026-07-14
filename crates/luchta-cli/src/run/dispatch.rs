//! Per-task execution machinery: dispatching a ready task, running it, and
//! persisting its cache state. Extracted from `run.rs` to keep that module
//! cohesive (one responsibility per submodule).
//!
//! These helpers operate on the shared, read-only `DispatchContext` (defined in
//! the parent module). `use super::*` pulls in the parent's imports and private
//! items so the relocated code compiles unchanged.

use super::*;

use crate::cache_ctx::gather_pkg_dep_pairs_filtered;

use luchta_cache::shared::{
    combined_dep_outputs_hash, derive_input_key, RestoredHit, StoreOutcome,
};
use luchta_cache::{
    decide_shared_restore, task_cache_key, CurrentState, FileEntry, ReportInput, RunArtifacts,
    RunReason, SCHEMA_VERSION_V4,
};
use luchta_types::EnvSpec;
use luchta_worker::BUILTIN_PASSTHROUGH_ENV;

use crate::env_merge::merge_env;
use luchta_workspace::PackageGraph;

use std::sync::OnceLock;

use crate::watch::registry::{register_task_watch_state, register_task_watch_state_from_packages};

/// Shared empty env map used as a stable fallback when a task has no entry in
/// `task_envs`. Mirrors the original `unwrap_or(&empty)` semantics (hash an
/// empty env rather than panic) while providing a `'static` reference that
/// outlives the caller. Uses `OnceLock` (stable since 1.70) to stay within the
/// crate's 1.78 MSRV.
fn empty_task_env() -> &'static BTreeMap<String, EnvSpec> {
    static EMPTY_TASK_ENV: OnceLock<BTreeMap<String, EnvSpec>> = OnceLock::new();
    EMPTY_TASK_ENV.get_or_init(BTreeMap::new)
}

fn split_captured_logs(sink: &ExecutionLogSink) -> (Vec<u8>, Vec<u8>) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    for line in sink.lines() {
        let buf = match line.stream {
            LogStream::Stdout => &mut out,
            LogStream::Stderr => &mut err,
        };
        buf.extend_from_slice(line.line.as_bytes());
        buf.push(b'\n');
    }
    (out, err)
}

fn collected_reports_for_cache(sink: &ExecutionLogSink) -> Vec<ReportInput> {
    sink.reports()
        .into_iter()
        .map(|report| ReportInput {
            filename: report.filename,
            mime_type: report.mime_type,
            content: report.content,
        })
        .collect()
}

struct FailureLogContext {
    task_id: TaskId,
    start_unix_ms: u64,
    end_unix_ms: u64,
    exit_status: Option<i32>,
    fallback_detail: Option<String>,
}

fn format_captured_failure_logs(context: FailureLogContext, sink: &ExecutionLogSink) -> String {
    let FailureLogContext {
        task_id,
        start_unix_ms,
        end_unix_ms,
        exit_status,
        fallback_detail,
    } = context;
    let (stdout, stderr) = split_captured_logs(sink);
    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);

    let mut body = stdout.into_owned();
    if !stderr.is_empty() {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&stderr);
    }
    if let Some(detail) = fallback_detail {
        if body.trim().is_empty() {
            body = detail;
        } else {
            if !body.ends_with('\n') {
                body.push('\n');
            }
            body.push_str(&detail);
        }
    }

    let lines: Vec<&str> = body.lines().collect();
    let reports_raw = sink.reports();
    let reports = crate::format::render_reports_pretty(
        reports_raw
            .iter()
            .map(|report| crate::format::ReportRenderInput {
                mime_type: &report.mime_type,
                bytes: report.content.as_bytes(),
            }),
        Stream::Stderr,
    );

    let cache_hash_full = task_cache_key(&task_id.to_string());
    let cache_hash_12 = &cache_hash_full[..12];
    let (package_display, task_display) = crate::format::package_and_task_display(&task_id);

    let (shown_lines, _) = crate::format::truncate_output(&lines, package_display, task_display);
    let body = shown_lines.join("\n");

    crate::format::format_task_log_block(
        &crate::format::LogBlockMeta {
            package: package_display,
            task: task_display,
            start: Some(start_unix_ms),
            duration_ms: Some(end_unix_ms.saturating_sub(start_unix_ms)),
            exit_status,
            cache_hash: Some(cache_hash_12),
            show_cache_nonce: false,
            cache_nonce: None,
            run_reason: None,
        },
        &body,
        &reports,
        Stream::Stderr,
    )
}

pub(super) fn dispatch_ready_task(task: ReadyTask, ctx: &DispatchContext<'_>) {
    spawn_task_runner(task, ctx);
}

/// Begins handling a ready task. Fast-path terminal states (outside selection,
/// fast-stop, invalid, ordering connector) are finalized immediately and return
/// `None`. Otherwise the cache-skip decision is computed:
/// - a pure LOCAL decision is offloaded to a bounded blocking pool and delivered
///   later via `decision_result_tx` (returns `None`);
/// - a SHARED-cache or non-cache-enabled decision is computed synchronously here
///   (kept serialized) and returned as `Some(..)` so the caller can feed it
///   straight into the sequential completion path.
pub(super) fn dispatch_ready_task_async(
    task_node: TaskNode,
    done_tx: CompletionSignal,
    ctx: &DispatchContext<'_>,
    decision_semaphore: Arc<tokio::sync::Semaphore>,
    decision_result_tx: tokio::sync::mpsc::Sender<DecisionTaskResult>,
) -> Option<DecisionTaskResult> {
    let task_id = task_node.id.clone();

    if !ctx.tasks_to_run.contains(&task_id) {
        mark_task_outside_selection(ctx.reporter, &task_id, done_tx);
        return None;
    }

    // In default mode, once a failure has occurred no further work is dispatched
    // — including tasks that turn out to be invalid/config errors. Check the
    // fast-stop gate before invalid/connector handling so a late invalid task is
    // suppressed (uncounted) rather than reported as an additional failure.
    if should_skip_for_fast_stop(ctx) {
        skip_task_after_prior_failure(ctx.reporter, &task_id, done_tx);
        return None;
    }

    if let Some(message) = ctx.invalid.get(&task_id) {
        fail_invalid_task(&task_id, message, done_tx, ctx);
        return None;
    }

    let Some(request) = ctx.commands.get(&task_id).cloned() else {
        mark_ordering_connector(ctx.reporter, &task_id, done_tx);
        return None;
    };

    let cache_enabled = ctx
        .task_graph
        .task_definition(&task_id)
        .is_some_and(TaskDefinition::cache_enabled);
    let skip_enabled = cache_enabled && !ctx.no_cache;

    // Only the LOCAL cache-skip decision is safe to run concurrently: it is a
    // pure filesystem read (glob + stat) with no side effects. The SHARED-cache
    // decision path (`try_shared_cache_skip`) restores outputs into the package
    // directory and mutates shared state, so it must stay serialized. Tasks that
    // are not cache-enabled need no decision at all. For those non-parallelizable
    // cases we compute synchronously here (on the dispatch loop) and return the
    // result so the caller feeds it straight into the sequential completion path
    // — we deliberately do NOT `tokio::spawn` blocking work onto the async
    // executor or bypass the decision semaphore.
    if !skip_enabled || ctx.decision_ctx.shared_cache.is_some() {
        let decision = if skip_enabled {
            try_cache_skip(&task_id, &ctx.decision_ctx)
        } else {
            None
        };
        return Some(DecisionTaskResult {
            task_id,
            request,
            done_tx,
            cache_enabled,
            outcome: DecisionOutcome::Direct { decision },
        });
    }

    // Parallelizable local-cache decision: offload the blocking FS work to a
    // bounded blocking pool and deliver the result via the channel.
    let decision_ctx = ctx.decision_ctx.clone();
    tokio::spawn(async move {
        let task_id_for_decision = task_id.clone();
        let decision_ctx_for_decision = decision_ctx.clone();
        let decision_result = super::pause::run_decision_task(decision_semaphore, move || {
            try_cache_skip(&task_id_for_decision, &decision_ctx_for_decision)
        })
        .await;

        let _ = decision_result_tx
            .send(DecisionTaskResult {
                task_id,
                request,
                done_tx,
                cache_enabled,
                outcome: DecisionOutcome::Parallelizable { decision_result },
            })
            .await;
    });

    None
}

fn mark_task_outside_selection(
    reporter: &ProgressReporter,
    task_id: &TaskId,
    done_tx: CompletionSignal,
) {
    // Task not in requested subgraph — not counted.
    reporter.task_finished_uncounted(task_id);
    let _ = done_tx.send(true);
}

fn fail_invalid_task(
    task_id: &TaskId,
    message: &str,
    done_tx: CompletionSignal,
    ctx: &DispatchContext<'_>,
) {
    // A misconfigured task (e.g. command without worker) only fails when it is
    // actually selected to run — it must not abort unrelated tasks.
    trigger_fast_stop_on_first_failure(
        ctx.any_failed,
        ctx.interrupted,
        ctx.continue_on_failure,
        ctx.owns_worker_manager,
        ctx.worker_manager,
    );
    eprintln!(
        "{} {}",
        "✖".if_supports_color(Stream::Stderr, |text| text.red()),
        message.if_supports_color(Stream::Stderr, |text| text.red())
    );
    // Invalid/config-error — counted in totals via wave map, but completion is
    // recorded as failed because it is neither done nor cache-skipped.
    ctx.reporter.task_failed(task_id);
    let _ = done_tx.send(false);
}

fn mark_ordering_connector(
    reporter: &ProgressReporter,
    task_id: &TaskId,
    done_tx: CompletionSignal,
) {
    // No worker/no command ordering node — uncounted connector, not runnable work.
    reporter.task_finished_uncounted(task_id);
    let _ = done_tx.send(true);
}

fn should_skip_for_fast_stop(ctx: &DispatchContext<'_>) -> bool {
    !ctx.continue_on_failure && ctx.any_failed.load(Ordering::SeqCst)
}

fn skip_task_after_prior_failure(
    reporter: &ProgressReporter,
    task_id: &TaskId,
    done_tx: CompletionSignal,
) {
    // Skipped due to previous failure — not counted.
    reporter.task_finished_uncounted(task_id);
    let _ = done_tx.send(false);
}

fn handle_cache_skip(
    task_id: &TaskId,
    decision: Decision,
    done_tx: CompletionSignal,
    ctx: &DispatchContext<'_>,
) {
    match decision {
        Decision::Skip => {
            ctx.reporter.task_skipped_cache_hit(task_id);
            if let Some(prior) = ctx.cache.read(&task_id.to_string()) {
                record_output_hash(ctx.output_hashes, task_id, prior.outputs_hash);
                register_task_watch_state_from_packages(
                    &ctx.decision_ctx.task_watch_registry,
                    task_id,
                    ctx.packages,
                    &prior,
                )
                .expect("cache skip task watch registration should compile globs");
            }
            let _ = done_tx.send(true);
        }
        Decision::SharedHit => {
            ctx.reporter.task_skipped_shared_cache(task_id);
            let _ = done_tx.send(true);
        }
        Decision::Run => {
            let _ = done_tx.send(false);
        }
    }
}

#[derive(Debug)]
pub(super) enum DecisionOutcome {
    Parallelizable {
        decision_result: Result<Option<Decision>, miette::Report>,
    },
    Direct {
        decision: Option<Decision>,
    },
}

#[derive(Debug)]
pub(super) struct DecisionTaskResult {
    pub(super) task_id: TaskId,
    pub(super) request: ExecutionRequest,
    pub(super) done_tx: CompletionSignal,
    pub(super) cache_enabled: bool,
    pub(super) outcome: DecisionOutcome,
}

pub(super) fn dispatch_decision_result(
    result: DecisionTaskResult,
    ctx: &DispatchContext<'_>,
) -> Result<Option<ReadyTask>> {
    let DecisionTaskResult {
        task_id,
        request,
        done_tx,
        cache_enabled,
        outcome,
    } = result;

    let decision = match outcome {
        DecisionOutcome::Parallelizable { decision_result } => decision_result?,
        DecisionOutcome::Direct { decision } => decision,
    };

    if let Some(decision) = decision {
        match decision {
            Decision::Skip | Decision::SharedHit => {
                handle_cache_skip(&task_id, decision, done_tx, ctx);
                return Ok(None);
            }
            Decision::Run => {}
        }
    }

    Ok(Some(ReadyTask {
        task_id,
        request,
        done_tx,
        cache_enabled,
    }))
}

fn build_task_run_context(
    task_id: &TaskId,
    cache_enabled: bool,
    no_cache: bool,
    ctx: &DispatchContext<'_>,
) -> TaskRunContext {
    let output_hash_record =
        build_output_hash_record_context(task_id, ctx.task_graph, ctx.packages, ctx.workspace_root);
    let cache_write = match build_cache_write_context(task_id, &ctx.decision_ctx) {
        CacheInputState::Ready(mut cache_ctx) => {
            if cache_enabled {
                let decision = build_cache_decision_context(
                    task_id,
                    &ctx.decision_ctx,
                    no_cache,
                    &mut cache_ctx,
                );
                match decision.action {
                    Decision::Run => {
                        cache_ctx.capture_pre_execution_snapshot();
                        Some(*cache_ctx)
                    }
                    Decision::Skip => None,
                    Decision::SharedHit => None,
                }
            } else {
                cache_ctx.capture_pre_execution_snapshot();
                Some(*cache_ctx)
            }
        }
        CacheInputState::Disabled => None,
    };

    TaskRunContext {
        executor: Arc::clone(ctx.executor),
        any_failed: Arc::clone(ctx.any_failed),
        interrupted: Arc::clone(ctx.interrupted),
        cache: Arc::clone(ctx.cache),
        output_hashes: Arc::clone(ctx.output_hashes),
        cache_write,
        output_hash_record,
        shared_cache: ctx.shared_cache.clone(),
    }
}

struct SpawnedTaskOutcome {
    outcome_res: Result<TaskRunOutcome, luchta_engine::ExecutorError>,
    succeeded: bool,
    start_unix_ms: u64,
    end_unix_ms: u64,
}

struct SpawnedTaskRun<F> {
    executor: Arc<WeightedExecutor>,
    request: ExecutionRequest,
    on_start: F,
    log_sink: ExecutionLogSink,
    cache_enabled: bool,
    no_cache: bool,
    repo_root: PathBuf,
    task_ctx: TaskRunContext,
    task_start_unix_ms: u64,
}

fn prepare_task_log_sink(request: &mut ExecutionRequest) -> ExecutionLogSink {
    let log_sink = ExecutionLogSink::new();
    request.log_sink = Some(log_sink.clone());
    log_sink
}

async fn run_task_and_persist_cache<F>(run: SpawnedTaskRun<F>) -> SpawnedTaskOutcome
where
    F: FnOnce() + Send + 'static,
{
    let SpawnedTaskRun {
        executor,
        request,
        on_start,
        log_sink,
        cache_enabled,
        no_cache,
        repo_root,
        task_ctx,
        task_start_unix_ms,
    } = run;
    let TaskRunContext {
        executor: _,
        any_failed: _,
        interrupted,
        cache,
        output_hashes,
        cache_write,
        output_hash_record,
        shared_cache,
    } = task_ctx;
    let outcome_res = executor.run_with_on_start(&request, on_start).await;
    let end_unix_ms = now_unix_ms();
    let succeeded = matches!(&outcome_res, Ok(result) if result.status.success());
    let output_hash_record =
        output_hash_record.map(|record| record.with_effective_patterns(outcome_res.as_ref().ok()));
    let start_unix_ms = cache_write
        .as_ref()
        .map(|cache_ctx| cache_ctx.start_unix_ms)
        .unwrap_or(task_start_unix_ms);
    let persist_failure_record = succeeded || !interrupted.load(Ordering::SeqCst);
    let expansion_error = persist_cache_state(CachePersistInputs {
        cache,
        cache_write,
        output_hashes: &output_hashes,
        output_hash_record: output_hash_record.as_ref(),
        log_sink: Some(&log_sink),
        outcome: outcome_res.as_ref().ok(),
        succeeded,
        persist_failure_record,
        end_unix_ms,
        shared_cache: cache_enabled.then_some(shared_cache).flatten(),
        shared_store_enabled: cache_enabled && !no_cache,
        repo_root,
    })
    .await;

    if let Some(expansion_error) = expansion_error {
        if !interrupted.load(Ordering::SeqCst) {
            eprintln!(
                "{} {}",
                "✖".if_supports_color(Stream::Stderr, |text| text.red()),
                expansion_error.if_supports_color(Stream::Stderr, |text| text.red())
            );
        }
        return SpawnedTaskOutcome {
            outcome_res,
            succeeded: false,
            start_unix_ms,
            end_unix_ms,
        };
    }

    SpawnedTaskOutcome {
        outcome_res,
        succeeded,
        start_unix_ms,
        end_unix_ms,
    }
}

fn record_resolved_output_hash(
    output_hashes: &Arc<Mutex<HashMap<TaskId, [u8; 32]>>>,
    output_hash_record: &OutputHashRecordContext,
) {
    match resolve_outputs(&output_hash_record.package_path, &output_hash_record.output_patterns) {
        Ok(outputs) => {
            let outputs_hash = combined_outputs_hash(&outputs);
            record_output_hash(output_hashes, &output_hash_record.task_id, outputs_hash);
        }
        Err(error) => eprintln!(
            "warning: skipping dependency output hash record for task '{}': failed to resolve cache outputs: {error}",
            output_hash_record.task_id
        ),
    }
}

fn build_output_hash_record_context(
    task_id: &TaskId,
    task_graph: &TaskGraph,
    packages: &[PackageNode],
    workspace_root: &Path,
) -> Option<OutputHashRecordContext> {
    let task_def = task_graph.task_definition(task_id)?;
    let cache_package = cache_package_context_for(packages, workspace_root, task_id)?;
    Some(OutputHashRecordContext {
        task_id: task_id.clone(),
        package_path: cache_package.package_path,
        output_patterns: task_def.outputs.clone(),
    })
}

fn cache_run_decision() -> CacheDecisionContext {
    CacheDecisionContext {
        action: Decision::Run,
        run_reason: RunReason::NoPriorRecord,
    }
}
fn build_cache_write_context(task_id: &TaskId, ctx: &DecisionContext) -> CacheInputState {
    let Some(task_def) = ctx.task_graph.task_definition(task_id).cloned() else {
        return CacheInputState::Disabled;
    };

    // Resolve nonce using the same helper as the read path.
    let nonce = ctx.resolve_task_nonce(&task_def);
    let Some(cache_context) = cache_state_context(task_id, ctx) else {
        return CacheInputState::Disabled;
    };

    let merged_env = match ctx.task_envs.get(task_id) {
        Some(env) => env,
        None => empty_task_env(),
    };
    let current = build_cache_current_state(CacheCurrentStateInput {
        task_def: &task_def,
        merged_env,
        nonce: nonce.as_deref(),
        cache_context: &cache_context,
    });
    let task_spec_hash = current.task_spec_hash;
    let env_hash = current.env_hash;
    let pkg_dep_hash = current.pkg_dep_hash;

    CacheInputState::Ready(Box::new(CacheWriteContext {
        task_id: task_id.clone(),
        task_def,
        package_path: cache_context.cache_package.package_path.clone(),
        dep_outputs: cache_context.dep_outputs,
        task_spec_hash,
        env_hash,
        pkg_dep_hash,
        start_unix_ms: now_unix_ms(),
        repo_root: ctx.workspace_root.clone(),
        source_pkg: cache_context.cache_package.package_name.clone(),
        package_graph: Arc::clone(&ctx.package_graph),
        cache_nonce: nonce,
        decision: cache_run_decision(),
        task_watch_registry: Arc::clone(&ctx.task_watch_registry),
        pre_snapshot: None,
    }))
}

fn cache_state_context(task_id: &TaskId, ctx: &DecisionContext) -> Option<CacheStateContext> {
    let cache_package = cache_package_context_for(&ctx.packages, &ctx.workspace_root, task_id)?;
    let dep_outputs = dependency_output_hashes(task_id, &ctx.task_graph, &ctx.output_hashes);
    let pkg_dep_pairs = cache_pkg_dep_pairs(task_id, ctx, &cache_package)?;
    let resolver = PackageDirResolver::new(
        cache_package.package_path.clone(),
        ctx.workspace_root.clone(),
        cache_package.package_name.clone(),
        Arc::clone(&ctx.package_graph),
        Arc::clone(&ctx.listing_cache),
    );

    Some(CacheStateContext {
        cache_package: CachePackageContextOwned {
            package_path: cache_package.package_path.clone(),
            package_name: cache_package.package_name.clone(),
        },
        dep_outputs,
        pkg_dep_pairs,
        resolver,
    })
}

fn cache_pkg_dep_pairs(
    task_id: &TaskId,
    ctx: &DecisionContext,
    cache_package: &CachePackageContext<'_>,
) -> Option<Vec<(String, String)>> {
    let task_def = ctx.task_graph.task_definition(task_id)?;
    let synthetic_package;
    let package = if let Some(package) = cache_package.package {
        package
    } else {
        synthetic_package = PackageNode::new(
            cache_package.package_name.clone(),
            cache_package.package_path.clone(),
        );
        &synthetic_package
    };

    match gather_pkg_dep_pairs_filtered(
        package,
        cache_package.package.map(|_| ctx.package_graph.as_ref()),
        &ctx.workspace_root,
        ctx.lockfile.as_ref(),
        &task_def.dependencies,
    ) {
        Ok(pkg_dep_pairs) => Some(pkg_dep_pairs),
        Err(error) => {
            eprintln!(
                "warning: skipping cache write for task '{task_id}': failed to gather package dependencies: {error}"
            );
            None
        }
    }
}

fn build_cache_current_state(input: CacheCurrentStateInput<'_>) -> CurrentState<'_> {
    build_current_state(
        input.task_def,
        input.merged_env,
        input.cache_context.dep_outputs.clone(),
        &input.cache_context.pkg_dep_pairs,
        &input.cache_context.resolver,
        input.nonce,
    )
}

/// Result of building a run record for cache write.
/// Distinguishes between success, expansion errors (fatal), and IO/other errors (skip).
enum BuildRecordResult {
    Ok(Box<TaskRunRecord>),
    ExpansionError(String),
    /// Inputs changed during execution - cache Skipped, but run was successful.
    StabilityMismatch(String),
}

/// Effective input/output patterns for a run, plus whether the worker reported
/// additional (detected) patterns beyond the declared ones.
struct RunRecordPatterns {
    input_patterns: Vec<String>,
    output_patterns: Vec<String>,
    detected_output_patterns: bool,
}

/// Assemble a [`TaskRunRecord`] from the cache context, effective patterns, and the
/// resolved input entries to store. Resolves and hashes outputs here.
fn assemble_run_record(
    cache_ctx: &CacheWriteContext,
    args: &BuildRunRecordArgs<'_>,
    patterns: &RunRecordPatterns,
    inputs: Vec<FileEntry>,
) -> Box<TaskRunRecord> {
    let outputs = resolve_cache_outputs(cache_ctx, &patterns.output_patterns).unwrap_or_default();
    let outputs_hash = combined_outputs_hash(&outputs);
    let exit_status = args
        .outcome
        .map(|result| result.status.code().unwrap_or(1))
        .unwrap_or(1);

    Box::new(TaskRunRecord {
        schema_version: SCHEMA_VERSION_V4,
        task_spec_hash: cache_ctx.task_spec_hash,
        input_patterns: patterns.input_patterns.clone(),
        inputs,
        output_patterns: patterns.output_patterns.clone(),
        outputs,
        detected_input_patterns: false,
        detected_output_patterns: patterns.detected_output_patterns,
        outputs_hash,
        env_hash: cache_ctx.env_hash,
        pkg_dep_hash: cache_ctx.pkg_dep_hash,
        dep_outputs: cache_ctx.dep_outputs.clone(),
        exit_status,
        succeeded: args.succeeded,
        start_unix_ms: cache_ctx.start_unix_ms,
        end_unix_ms: args.end_unix_ms,
        reports: vec![],
        cache_nonce: cache_ctx.cache_nonce.clone(),
        run_reason: args.run_reason.clone(),
    })
}

/// Build the run record for a SUCCESSFUL run.
///
/// The strict concurrent-change guarantee applies only to DECLARED inputs (those
/// captured in `cache_ctx.pre_snapshot` before the task ran). `check_input_stability`
/// compares the pre-snapshot against the declared subset of `post_inputs`:
/// - A declared input changed/deleted mid-run -> `StabilityMismatch` (no cache
///   write, no watch-state registration; the task stays dirty so watch mode reruns).
/// - Otherwise record inputs come from verified-stable pre-snapshot captured
///   before execution. This authoritative snapshot already reflects any
///   resolve-time worker input override and closes post-resolve->write gap.
fn build_successful_run_record(
    cache_ctx: &CacheWriteContext,
    args: &BuildRunRecordArgs<'_>,
    patterns: &RunRecordPatterns,
    post_inputs: &[FileEntry],
) -> BuildRecordResult {
    let stable_inputs =
        match check_input_stability(cache_ctx.pre_snapshot(), post_inputs, &cache_ctx.task_id) {
            Ok(inputs) => inputs,
            Err(reason) => return BuildRecordResult::StabilityMismatch(reason),
        };

    let record = assemble_run_record(cache_ctx, args, patterns, stable_inputs);

    register_task_watch_state(
        &cache_ctx.task_watch_registry,
        &cache_ctx.task_id,
        cache_ctx.source_pkg.clone(),
        cache_ctx.package_path.clone(),
        &record,
    )
    .expect("run task watch registration should compile globs");

    BuildRecordResult::Ok(record)
}

fn build_run_record(
    cache_ctx: &CacheWriteContext,
    args: BuildRunRecordArgs<'_>,
) -> BuildRecordResult {
    let (output_patterns, detected_output_patterns) =
        effective_output_patterns(&cache_ctx.task_def, args.outcome);
    let input_patterns = cache_ctx.task_def.inputs.clone();

    // Resolve inputs after execution for the stability comparison.
    let post_inputs = match resolve_cache_inputs(cache_ctx, &input_patterns) {
        CacheInputResult::Ok(entries) => entries,
        CacheInputResult::ExpansionError(msg) => return BuildRecordResult::ExpansionError(msg),
        CacheInputResult::IoError => Vec::new(),
    };

    let patterns = RunRecordPatterns {
        input_patterns,
        output_patterns,
        detected_output_patterns,
    };

    if !args.succeeded {
        // Failed run: record with post-run inputs and register watch state so
        // subsequent edits to declared inputs still trigger watch rebuilds.
        let record = assemble_run_record(cache_ctx, &args, &patterns, post_inputs);
        register_task_watch_state(
            &cache_ctx.task_watch_registry,
            &cache_ctx.task_id,
            cache_ctx.source_pkg.clone(),
            cache_ctx.package_path.clone(),
            &record,
        )
        .expect("failed to register task watch state");
        return BuildRecordResult::Ok(record);
    }

    build_successful_run_record(cache_ctx, &args, &patterns, &post_inputs)
}

/// Result of writing a run record to cache.
/// ExpansionError signals a fatal security error that must fail the task.
/// StabilityMismatch indicates inputs changed during execution - cache skipped.
enum WriteRecordResult {
    Ok,
    ExpansionError(String),
    StabilityMismatch(#[allow(dead_code)] String),
}

#[allow(clippy::too_many_arguments)]
async fn write_run_record(
    cache: Arc<Cache>,
    cache_ctx: CacheWriteContext,
    output_hashes: Arc<Mutex<HashMap<TaskId, [u8; 32]>>>,
    log_sink: Option<&ExecutionLogSink>,
    outcome: Option<&TaskRunOutcome>,
    succeeded: bool,
    end_unix_ms: u64,
    run_reason: Option<RunReason>,
    shared_cache: Option<Arc<SharedCache>>,
    shared_store_enabled: bool,
    repo_root: PathBuf,
) -> WriteRecordResult {
    let record = match build_run_record(
        &cache_ctx,
        BuildRunRecordArgs {
            outcome,
            succeeded,
            end_unix_ms,
            run_reason,
        },
    ) {
        BuildRecordResult::Ok(record) => record,
        BuildRecordResult::ExpansionError(msg) => return WriteRecordResult::ExpansionError(msg),
        BuildRecordResult::StabilityMismatch(reason) => {
            // Emit warning but don't fail the task - just skip cache write
            eprintln!(
                "{}",
                reason.if_supports_color(Stream::Stderr, |text| text.yellow())
            );
            return WriteRecordResult::StabilityMismatch(reason);
        }
    };
    record_output_hash(&output_hashes, &cache_ctx.task_id, record.outputs_hash);
    let (stdout, stderr) = log_sink.map(split_captured_logs).unwrap_or_default();
    let reports = log_sink
        .map(collected_reports_for_cache)
        .unwrap_or_default();
    let mut record = record;
    record.reports = reports
        .iter()
        .map(|report| luchta_cache::ReportMeta {
            filename: report.filename.clone(),
            mime_type: report.mime_type.clone(),
        })
        .collect();
    let cache_key = cache_ctx.task_id.to_string();

    // Clone values needed for shared cache store before moving into spawn_blocking
    let task_id_str = cache_ctx.task_id.to_string();
    let package_dir = cache_ctx.package_path.clone();
    let task_spec_hash = cache_ctx.task_spec_hash;
    let env_hash = cache_ctx.env_hash;
    let pkg_dep_hash = cache_ctx.pkg_dep_hash;
    let dep_outputs = cache_ctx.dep_outputs.clone();
    let start_unix_ms = cache_ctx.start_unix_ms;
    let outputs_hash = record.outputs_hash;
    let record_for_local = (*record).clone();
    let record_for_shared = record_for_local.clone();
    let task_id_for_error = cache_ctx.task_id.clone();

    match tokio::task::spawn_blocking(move || {
        // Local cache write (unchanged)
        if let Err(error) = cache.write(
            &cache_key,
            RunArtifacts {
                record: &record_for_local,
                stdout: &stdout,
                stderr: &stderr,
                reports: &reports,
            },
        ) {
            eprintln!(
                "warning: failed to write cache record for task '{}': {error}",
                task_id_for_error
            );
        }

        // Shared cache store (after local write, only if enabled)
        // Path-escape at this point is FATAL and propagates as expansion error.
        if shared_store_enabled {
            if let Some(shared) = shared_cache {
                let _duration_ms = end_unix_ms.saturating_sub(start_unix_ms);
                let input_key = derive_input_key(
                    task_spec_hash,
                    env_hash,
                    pkg_dep_hash,
                    combined_dep_outputs_hash(&dep_outputs),
                );

                // Gather package-relative output paths from record.outputs (skip absent entries)
                let rel_output_paths: Vec<std::path::PathBuf> = record_for_shared
                    .outputs
                    .iter()
                    .filter(|f| !f.absent)
                    .map(|f| std::path::PathBuf::from(&f.path))
                    .collect();

                match shared.store(
                    &task_id_str,
                    &input_key,
                    &outputs_hash,
                    &package_dir,
                    &rel_output_paths,
                    &record_for_shared,
                    &stdout,
                    &stderr,
                    &reports,
                    &repo_root,
                ) {
                    Ok(StoreOutcome::Stored) => {}
                    Ok(StoreOutcome::SkippedNotSucceeded) => {}
                    Ok(StoreOutcome::SkippedTooFast { duration_ms: _ }) => {}
                    Ok(StoreOutcome::SkippedTooLarge { bytes: _ }) => {}
                    Ok(StoreOutcome::SkippedCrossPackage) => {}
                    Ok(StoreOutcome::SkippedLockUnavailable) => {}
                    Ok(StoreOutcome::Disabled) => {}
                    Err(e) => {
                        if e.kind() == std::io::ErrorKind::InvalidData {
                            // Path-escape is a security hard-fail.
                            return Some(format!(
                                "shared cache store failed for task '{}': {}",
                                task_id_str, e
                            ));
                        }

                        eprintln!(
                            "warning: shared cache store failed for task '{}': {}; continuing with local cache",
                            task_id_str, e
                        );
                    }
                }
            }
        }

        None
    })
    .await
    {
        Ok(Some(expansion_error)) => return WriteRecordResult::ExpansionError(expansion_error),
        Ok(None) => {}
        Err(error) => eprintln!(
            "warning: cache write task panicked for task '{}': {error}",
            cache_ctx.task_id
        ),
    }
    WriteRecordResult::Ok
}

fn trigger_fast_stop_on_first_failure(
    any_failed: &Arc<AtomicBool>,
    interrupted: &Arc<AtomicBool>,
    continue_on_failure: bool,
    owns_worker_manager: bool,
    worker_manager: &Arc<WorkerManager>,
) -> bool {
    let first_failure = any_failed
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok();

    if first_failure && !continue_on_failure {
        interrupted.store(true, Ordering::SeqCst);
        if owns_worker_manager {
            let worker_manager = Arc::clone(worker_manager);
            tokio::spawn(async move {
                worker_manager.shutdown_immediate().await;
            });
        }
    }

    first_failure
}

fn format_task_error(error: &luchta_engine::ExecutorError) -> String {
    format!("failed: {error}")
}

fn build_cache_decision_context(
    task_id: &TaskId,
    ctx: &DecisionContext,
    no_cache: bool,
    cache_ctx: &mut CacheWriteContext,
) -> CacheDecisionContext {
    let task_def = cache_ctx.task_def.clone();
    let Some(cache_context) = cache_read_state_context(task_id, ctx, cache_ctx) else {
        return cache_ctx.decision.clone();
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
    let decision = decide(ctx.cache.read(&task_id.to_string()).as_ref(), &current);
    cache_ctx.decision = cache_decision_from_result(&decision);
    maybe_mark_shared_cache_hit(
        ctx,
        no_cache,
        cache_ctx,
        SharedCacheSkipInput {
            task_id,
            task_def: &task_def,
            current: &current,
            decision: &decision,
        },
        &cache_context.dep_outputs,
    );
    if no_cache {
        cache_ctx.decision = cache_run_decision();
    }
    cache_ctx.decision.clone()
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

fn maybe_mark_shared_cache_hit(
    ctx: &DecisionContext,
    no_cache: bool,
    cache_ctx: &mut CacheWriteContext,
    input: SharedCacheSkipInput<'_>,
    dep_outputs: &BTreeMap<String, [u8; 32]>,
) {
    if no_cache || !matches!(input.decision.action, Decision::Run) {
        return;
    }

    if let Some(shared_decision) = try_shared_cache_skip(
        input.task_id,
        ctx,
        input.task_def,
        &cache_ctx.package_path,
        input.current,
        dep_outputs,
    ) {
        if matches!(shared_decision, Decision::SharedHit) {
            cache_ctx.decision.action = Decision::SharedHit;
        }
    }
}

pub(super) fn try_cache_skip(task_id: &TaskId, ctx: &DecisionContext) -> Option<Decision> {
    let task_def = ctx.task_graph.task_definition(task_id)?;

    // Resolve nonce using the same helper as the write path.
    let nonce = ctx.resolve_task_nonce(task_def);

    let mut cache_ctx = match build_cache_write_context(task_id, ctx) {
        CacheInputState::Ready(cache_ctx) => *cache_ctx,
        CacheInputState::Disabled => return Some(Decision::Run),
    };
    cache_ctx.cache_nonce = nonce;

    Some(build_cache_decision_context(task_id, ctx, false, &mut cache_ctx).action)
}

fn try_shared_cache_skip(
    task_id: &TaskId,
    ctx: &DecisionContext,
    task_def: &TaskDefinition,
    package_path: &Path,
    current: &CurrentState<'_>,
    dep_outputs: &BTreeMap<String, [u8; 32]>,
) -> Option<Decision> {
    let shared_cache = ctx.shared_cache.as_ref()?;

    // Outputs may escape package dir -> not read-eligible for shared cache.
    // Falls through to run normally (write-time scope check in P4.3).
    if !outputs_lexically_in_package(&task_def.outputs) {
        return Some(Decision::Run);
    }

    // Compute input_key from the SAME hashes used for local cache.
    let dep_outputs_hash = combined_dep_outputs_hash(dep_outputs);
    let input_key = derive_input_key(
        current.task_spec_hash,
        current.env_hash,
        current.pkg_dep_hash,
        dep_outputs_hash,
    );

    // Try restore from shared cache with validation.
    // Iterate candidates newest-first; validate each before committing.
    for candidate in
        shared_cache.try_restore_candidates(&task_id.to_string(), &input_key, package_path)
    {
        // VALIDATE: Use decide_shared_restore to check if this candidate matches current tree state.
        // Unlike full decide(), this does NOT require outputs to exist in the tree —
        // we're ABOUT to restore outputs from the blob.
        if decide_shared_restore(&candidate.record, current) {
            // Candidate is VALID - inputs match current tree.
            // Commit the staged restore.
            match candidate.commit() {
                Ok((hit, _written_paths)) => {
                    register_task_watch_state(
                        &ctx.task_watch_registry,
                        task_id,
                        task_id.package.clone(),
                        package_path.to_path_buf(),
                        &hit.record,
                    )
                    .expect("shared hit task watch registration should compile globs");
                    // Shared cache HIT (validated):
                    // (a) Outputs now restored to package dir.
                    // (b) Hydrate local cache for next build.
                    hydrate_local_cache(ctx.cache.clone(), task_id.clone(), &hit);
                    // (c) Replay the restored task's captured stdout/stderr so a
                    // shared-cache hit produces the same visible output as on main.
                    replay_logs(&hit, &ctx.reporter);
                    // (d) Record output hash for downstream invalidation.
                    record_output_hash(&ctx.output_hashes, task_id, hit.outputs_hash);
                    // (e) Return dedicated shared-hit decision so dispatcher can count it.
                    return Some(Decision::SharedHit);
                }
                Err(e) => {
                    // Commit failed - log and continue to next candidate
                    eprintln!("warning: shared cache restore commit failed: {e}");
                    continue;
                }
            }
        } else {
            // Candidate is STALE - inputs do not match current tree.
            // Discard staging and try next candidate.
            if let Err(e) = candidate.discard() {
                eprintln!("warning: shared cache discard failed: {e}");
            }
            continue;
        }
    }

    None
}

/// A ready task to spawn: what to run, where to report completion, and whether
/// caching applies. Groups the per-task parameters so `spawn_task_runner` stays
/// within a sane argument count.
pub(super) struct ReadyTask {
    task_id: TaskId,
    request: ExecutionRequest,
    done_tx: CompletionSignal,
    cache_enabled: bool,
}

/// Spawns the async runner that executes the task and reports completion back
/// through its `done_tx`. Records failures in `any_failed`; errors/non-zero
/// exits are reported unless the run was interrupted (in which case killed jobs
/// are expected and their noise is suppressed).
fn spawn_task_runner(ready: ReadyTask, ctx: &DispatchContext<'_>) {
    let ReadyTask {
        task_id,
        mut request,
        done_tx,
        cache_enabled,
    } = ready;
    let task_start_unix_ms = now_unix_ms();
    let task_ctx = build_task_run_context(&task_id, cache_enabled, ctx.no_cache, ctx);
    let reporter = Arc::clone(ctx.reporter);
    let started_task_id = task_id.clone();
    let repo_root = ctx.workspace_root.to_path_buf();

    let executor = Arc::clone(&task_ctx.executor);
    let any_failed = Arc::clone(&task_ctx.any_failed);
    let interrupted = Arc::clone(&task_ctx.interrupted);
    let worker_manager = Arc::clone(ctx.worker_manager);
    let owns_worker_manager = ctx.owns_worker_manager;
    let continue_on_failure = ctx.continue_on_failure;
    let no_cache = ctx.no_cache;
    let log_sink = prepare_task_log_sink(&mut request);

    tokio::spawn(async move {
        let on_start = {
            let reporter = Arc::clone(&reporter);
            move || reporter.task_started(&started_task_id)
        };
        let SpawnedTaskOutcome {
            outcome_res,
            succeeded,
            start_unix_ms,
            end_unix_ms,
        } = run_task_and_persist_cache(SpawnedTaskRun {
            executor,
            request,
            on_start,
            log_sink: log_sink.clone(),
            cache_enabled,
            no_cache,
            repo_root,
            task_ctx,
            task_start_unix_ms,
        })
        .await;

        finalize_task_run(TaskRunFinalization {
            task_id: &task_id,
            done_tx,
            reporter: &reporter,
            any_failed: &any_failed,
            interrupted: &interrupted,
            worker_manager: &worker_manager,
            owns_worker_manager,
            continue_on_failure,
            log_sink: &log_sink,
            outcome_res: &outcome_res,
            succeeded,
            start_unix_ms,
            end_unix_ms,
        });
    });
}

struct TaskRunFinalization<'a> {
    task_id: &'a TaskId,
    done_tx: CompletionSignal,
    reporter: &'a Arc<ProgressReporter>,
    any_failed: &'a Arc<AtomicBool>,
    interrupted: &'a Arc<AtomicBool>,
    worker_manager: &'a Arc<WorkerManager>,
    owns_worker_manager: bool,
    continue_on_failure: bool,
    log_sink: &'a ExecutionLogSink,
    outcome_res: &'a Result<TaskRunOutcome, luchta_engine::ExecutorError>,
    succeeded: bool,
    start_unix_ms: u64,
    end_unix_ms: u64,
}

fn finalize_task_run(finalization: TaskRunFinalization<'_>) {
    let TaskRunFinalization {
        task_id,
        done_tx,
        reporter,
        any_failed,
        interrupted,
        worker_manager,
        owns_worker_manager,
        continue_on_failure,
        log_sink,
        outcome_res,
        succeeded,
        start_unix_ms,
        end_unix_ms,
    } = finalization;

    let interrupted_run = interrupted.load(Ordering::SeqCst);
    let failure_kind = classify_task_failure(TaskFailureContext {
        succeeded,
        any_failed,
        interrupted,
        continue_on_failure,
        owns_worker_manager,
        worker_manager,
    });

    if should_print_failure_logs(failure_kind, interrupted_run) {
        let failure_logs = format_captured_failure_logs(
            FailureLogContext {
                task_id: task_id.clone(),
                start_unix_ms,
                end_unix_ms,
                exit_status: outcome_res
                    .as_ref()
                    .ok()
                    .and_then(|result| result.status.code()),
                fallback_detail: outcome_res.as_ref().err().map(format_task_error),
            },
            log_sink,
        );
        eprint!("{}", failure_logs);
    }

    record_task_outcome(reporter, task_id, failure_kind);
    let _ = done_tx.send(succeeded);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TaskFailureKind {
    Succeeded,
    Failed,
    CollateralFastStop,
}

struct TaskFailureContext<'a> {
    succeeded: bool,
    any_failed: &'a Arc<AtomicBool>,
    interrupted: &'a Arc<AtomicBool>,
    continue_on_failure: bool,
    owns_worker_manager: bool,
    worker_manager: &'a Arc<WorkerManager>,
}

fn classify_task_failure(context: TaskFailureContext<'_>) -> TaskFailureKind {
    let TaskFailureContext {
        succeeded,
        any_failed,
        interrupted,
        continue_on_failure,
        owns_worker_manager,
        worker_manager,
    } = context;
    if succeeded {
        return TaskFailureKind::Succeeded;
    }

    let first_failure = trigger_fast_stop_on_first_failure(
        any_failed,
        interrupted,
        continue_on_failure,
        owns_worker_manager,
        worker_manager,
    );
    let collateral_fast_stop =
        !continue_on_failure && interrupted.load(Ordering::SeqCst) && !first_failure;

    if collateral_fast_stop {
        TaskFailureKind::CollateralFastStop
    } else {
        TaskFailureKind::Failed
    }
}

fn should_print_failure_logs(failure_kind: TaskFailureKind, interrupted_run: bool) -> bool {
    matches!(failure_kind, TaskFailureKind::Failed) && !interrupted_run
}

fn record_task_outcome(
    reporter: &ProgressReporter,
    task_id: &TaskId,
    failure_kind: TaskFailureKind,
) {
    match failure_kind {
        TaskFailureKind::Succeeded => reporter.task_ran(task_id),
        TaskFailureKind::Failed => reporter.task_failed(task_id),
        TaskFailureKind::CollateralFastStop => reporter.task_finished_uncounted(task_id),
    }
}

/// Inputs for persisting a finished task's cache state.
struct CachePersistInputs<'a> {
    cache: Arc<Cache>,
    cache_write: Option<CacheWriteContext>,
    output_hashes: &'a Arc<Mutex<HashMap<TaskId, [u8; 32]>>>,
    output_hash_record: Option<&'a OutputHashRecordContext>,
    log_sink: Option<&'a ExecutionLogSink>,
    outcome: Option<&'a TaskRunOutcome>,
    succeeded: bool,
    persist_failure_record: bool,
    end_unix_ms: u64,
    /// Shared cache for storing successful task results.
    shared_cache: Option<Arc<SharedCache>>,
    /// Whether shared-cache store is enabled for this task.
    shared_store_enabled: bool,
    /// Repo root for scope classification during shared cache write.
    repo_root: PathBuf,
}

/// Records the run record (cached tasks) or just the resolved output hash
/// (uncached tasks) so downstream dependency coupling stays correct.
/// Returns an expansion error message if one occurred (for caller to handle).
///
/// Shared-cache store happens AFTER local cache write. The store runs
/// synchronously within `spawn_blocking` (shared with the local write) because:
/// - Correctness first: no races between local write and shared store.
/// - Simplicity: avoids complex async dance with compression overhead.
/// - The local write already uses spawn_blocking, so we piggy-back.
/// - Path-escape at shared-store time is FATAL (propagated as expansion error).
async fn persist_cache_state(inputs: CachePersistInputs<'_>) -> Option<String> {
    let CachePersistInputs {
        cache,
        cache_write,
        output_hashes,
        output_hash_record,
        log_sink,
        outcome,
        succeeded,
        persist_failure_record,
        end_unix_ms,
        shared_cache,
        shared_store_enabled,
        repo_root,
    } = inputs;

    if let Some(cache_ctx) = cache_write {
        return persist_cache_write(CacheWriteInputs {
            cache,
            cache_ctx,
            output_hashes: Arc::clone(output_hashes),
            log_sink,
            outcome,
            succeeded,
            persist_failure_record,
            end_unix_ms,
            shared_cache,
            shared_store_enabled,
            repo_root,
        })
        .await;
    }

    record_successful_output_hash(output_hashes, output_hash_record, succeeded);
    None
}

struct CacheWriteInputs<'a> {
    cache: Arc<Cache>,
    cache_ctx: CacheWriteContext,
    output_hashes: Arc<Mutex<HashMap<TaskId, [u8; 32]>>>,
    log_sink: Option<&'a ExecutionLogSink>,
    outcome: Option<&'a TaskRunOutcome>,
    succeeded: bool,
    persist_failure_record: bool,
    end_unix_ms: u64,
    shared_cache: Option<Arc<SharedCache>>,
    shared_store_enabled: bool,
    repo_root: PathBuf,
}

async fn persist_cache_write(inputs: CacheWriteInputs<'_>) -> Option<String> {
    let CacheWriteInputs {
        cache,
        cache_ctx,
        output_hashes,
        log_sink,
        outcome,
        succeeded,
        persist_failure_record,
        end_unix_ms,
        shared_cache,
        shared_store_enabled,
        repo_root,
    } = inputs;

    if !succeeded && !persist_failure_record {
        return None;
    }

    let run_reason = matches!(cache_ctx.decision.action, Decision::Run)
        .then(|| cache_ctx.decision.run_reason.clone());
    let result = write_run_record(
        cache,
        cache_ctx,
        output_hashes,
        log_sink,
        outcome,
        succeeded,
        end_unix_ms,
        run_reason,
        shared_cache,
        shared_store_enabled,
        repo_root,
    )
    .await;
    cache_write_error(result)
}

fn record_successful_output_hash(
    output_hashes: &Arc<Mutex<HashMap<TaskId, [u8; 32]>>>,
    output_hash_record: Option<&OutputHashRecordContext>,
    succeeded: bool,
) {
    if !succeeded {
        return;
    }

    if let Some(record) = output_hash_record {
        record_resolved_output_hash(output_hashes, record);
    }
}

fn cache_write_error(result: WriteRecordResult) -> Option<String> {
    match result {
        WriteRecordResult::Ok => None,
        WriteRecordResult::StabilityMismatch(_) => None, // Non-fatal - cache skipped
        WriteRecordResult::ExpansionError(msg) => Some(msg),
    }
}

// ---- Cache-input/output helpers (used only by the per-task execution path) ----

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn package_node_for<'a>(
    packages: &'a [PackageNode],
    workspace_root: &Path,
    id: &TaskId,
) -> Option<&'a PackageNode> {
    if is_root_task(id) {
        packages
            .iter()
            .find(|package| package.path == workspace_root)
    } else {
        packages.iter().find(|package| package.name == id.package)
    }
}

pub(super) struct CachePackageContext<'a> {
    package: Option<&'a PackageNode>,
    package_path: PathBuf,
    package_name: PackageName,
}

fn cache_package_context_for<'a>(
    packages: &'a [PackageNode],
    workspace_root: &Path,
    id: &TaskId,
) -> Option<CachePackageContext<'a>> {
    if is_root_task(id) {
        Some(CachePackageContext {
            package: package_node_for(packages, workspace_root, id),
            package_path: workspace_root.to_path_buf(),
            package_name: id.package.clone(),
        })
    } else {
        package_node_for(packages, workspace_root, id).map(|package| CachePackageContext {
            package: Some(package),
            package_path: package.path.clone(),
            package_name: package.name.clone(),
        })
    }
}

fn dependency_output_hashes(
    task_id: &TaskId,
    task_graph: &TaskGraph,
    output_hashes: &Arc<Mutex<HashMap<TaskId, [u8; 32]>>>,
) -> BTreeMap<String, [u8; 32]> {
    let map = output_hashes.lock().expect("output_hashes poisoned");
    task_graph
        .dependencies_of(task_id)
        .into_iter()
        .filter_map(|d| map.get(&d.id).copied().map(|h| (d.id.to_string(), h)))
        .collect()
}

fn record_output_hash(
    output_hashes: &Arc<Mutex<HashMap<TaskId, [u8; 32]>>>,
    task_id: &TaskId,
    hash: [u8; 32],
) {
    output_hashes
        .lock()
        .expect("output_hashes poisoned")
        .insert(task_id.clone(), hash);
}

fn effective_output_patterns(
    task_def: &TaskDefinition,
    outcome: Option<&TaskRunOutcome>,
) -> (Vec<String>, bool) {
    match outcome.and_then(|o| o.detected_outputs.clone()) {
        Some(p) => (p, true),
        None => (task_def.outputs.clone(), false),
    }
}

/// Result of building per-task execution plan.
///
/// `invalid` holds tasks that are misconfigured. `task_envs` stores each task
/// merged env spec for cache hashing.
pub(super) struct CommandMap {
    pub(super) commands: HashMap<TaskId, ExecutionRequest>,
    pub(super) invalid: HashMap<TaskId, String>,
    pub(super) task_envs: HashMap<TaskId, BTreeMap<String, EnvSpec>>,
}

pub(super) fn build_command_map(
    task_graph: &TaskGraph,
    packages: &[PackageNode],
    workspace_root: &Path,
    global_env: &BTreeMap<String, EnvSpec>,
    workers: &HashMap<String, WorkerDefinition>,
    package_graph: Option<&PackageGraph>,
) -> CommandMap {
    let package_by_name: HashMap<_, _> = packages.iter().map(|pkg| (&pkg.name, pkg)).collect();
    let mut commands = HashMap::new();
    let mut invalid = HashMap::new();
    let mut task_envs = HashMap::new();

    for node in task_graph.nodes() {
        let task_id = &node.id;
        let task_def = task_graph.task_definition(task_id);
        let package = package_by_name.get(&task_id.package).copied();
        let cwd = if is_root_task(task_id) {
            workspace_root.to_path_buf()
        } else {
            package
                .map(|pkg| pkg.path.clone())
                .unwrap_or_else(|| workspace_root.to_path_buf())
        };

        let worker = task_def.and_then(|def| def.worker.clone());
        let worker_env = worker
            .as_ref()
            .and_then(|worker_name| workers.get(worker_name))
            .map(|worker| &worker.env);
        let empty_task_env = BTreeMap::new();
        let task_env = task_def.map(|def| &def.env).unwrap_or(&empty_task_env);
        let merged_env = merge_env(global_env, worker_env, task_env);

        // Validate declared input patterns eagerly when we have the package graph
        if let (Some(def), Some(graph)) = (task_def, package_graph) {
            if !def.inputs.is_empty() {
                let source_pkg = task_id.package.clone();
                if let Err(error) =
                    expand_input_patterns(&def.inputs, &source_pkg, graph, workspace_root)
                {
                    invalid.insert(
                        task_id.clone(),
                        format!(
                            "input \"{}\" in package \"{}\": {}",
                            error.pattern(),
                            source_pkg,
                            error
                        ),
                    );
                    continue;
                }
            }
        }

        let (command, workspace) = if let Some(worker_name) = &worker {
            if !workers.contains_key(worker_name) {
                invalid.insert(
                    task_id.clone(),
                    format!("task '{task_id}' references unknown worker '{worker_name}'"),
                );
                continue;
            }
            let command = luchta_types::resolve_script_name(
                task_def.and_then(|def| def.command.as_deref()),
                task_id.task.as_str(),
            )
            .to_owned();
            let workspace = package
                .filter(|pkg| pkg.path != workspace_root)
                .map(|pkg| pkg.name.to_string())
                .unwrap_or_default();
            (command, Some(workspace))
        } else {
            match task_def {
                Some(definition) if !definition.counts_in_progress() => continue,
                Some(_) => {
                    invalid.insert(
                        task_id.clone(),
                        format!(
                            "task '{task_id}' defines a command but no worker; specify a worker to execute it"
                        ),
                    );
                    continue;
                }
                None => continue,
            }
        };

        let request = ExecutionRequest {
            task: node.clone(),
            command,
            cwd: Some(cwd),
            env: build_execution_env(&merged_env),
            log_sink: None,
            worker,
            workspace,
            inputs: task_def.map(|definition| definition.inputs.clone()),
            outputs: task_def.map(|definition| definition.outputs.clone()),
        };
        task_envs.insert(task_id.clone(), merged_env);
        commands.insert(task_id.clone(), request);
    }

    CommandMap {
        commands,
        invalid,
        task_envs,
    }
}

pub(super) fn resolve_task_env(env: &BTreeMap<String, EnvSpec>) -> HashMap<String, String> {
    env.iter()
        .filter_map(|(name, spec)| {
            spec.resolve_env_value(name, || std::env::var(name).ok())
                .map(|v| (name.clone(), v))
        })
        .collect()
}

fn collect_builtin_passthrough_env() -> HashMap<String, String> {
    BUILTIN_PASSTHROUGH_ENV
        .iter()
        .filter_map(|&name| std::env::var(name).ok().map(|v| (name.to_owned(), v)))
        .collect()
}

fn build_execution_env(merged_env: &BTreeMap<String, EnvSpec>) -> HashMap<String, String> {
    let mut env = collect_builtin_passthrough_env();
    env.extend(resolve_task_env(merged_env));
    env
}

/// Check if output patterns lexically stay inside package directory.
///
/// Read-time scope gate (Momus B2): at READ time outputs don't exist yet,
/// so we gate on the DECLARED output patterns. If any pattern is absolute
/// (starts with /) or lexically escapes the package (starts with ../ or
/// contains /../), the task is read-INELIGIBLE.
///
/// This is a conservative guard; the full resolved-path scope check is
/// WRITE-time (P4.3). Correctness rests on write-time (only InPackage
/// tasks are ever stored), so this read gate is an optimization.
fn outputs_lexically_in_package(output_patterns: &[String]) -> bool {
    for pattern in output_patterns {
        // Absolute path
        if pattern.starts_with('/') {
            return false;
        }
        // Explicit parent traversal
        if pattern.starts_with("../") || pattern.contains("/../") {
            return false;
        }
        // Pattern ends with parent reference
        if pattern == ".." || pattern.ends_with("/..") {
            return false;
        }
    }
    true
}

/// Hydrate local cache from a shared-cache hit.
///
/// Writes the restored record and logs so the next build in the same
/// worktree gets a normal local skip with correct downstream invalidation.
fn hydrate_local_cache(cache: Arc<Cache>, task_id: TaskId, hit: &RestoredHit) {
    let cache_key = task_id.to_string();
    let mut record = hit.record.clone();
    record.schema_version = SCHEMA_VERSION_V4;
    record.run_reason = Some(RunReason::SharedCacheHit);
    let reports: Vec<ReportInput> = hit
        .record
        .reports
        .iter()
        .filter_map(|report| {
            hit.reports
                .iter()
                .find(|stored| stored.filename == report.filename)
                .map(|stored| ReportInput {
                    filename: report.filename.clone(),
                    mime_type: report.mime_type.clone(),
                    content: stored.content.clone(),
                })
        })
        .collect();
    if let Err(e) = cache.write(
        &cache_key,
        RunArtifacts {
            record: &record,
            stdout: &hit.stdout,
            stderr: &hit.stderr,
            reports: &reports,
        },
    ) {
        eprintln!(
            "warning: failed to hydrate local cache for task '{}': {e}",
            task_id
        );
    }
}

/// Replay restored logs to the progress reporter.
///
/// This mirrors how the normal run path emits logs so output appears
/// as if the task actually ran.
pub(super) fn replay_logs(hit: &RestoredHit, _reporter: &Arc<ProgressReporter>) {
    // Replay stdout
    if !hit.stdout.is_empty() {
        if let Ok(stdout_str) = std::str::from_utf8(&hit.stdout) {
            for line in stdout_str.lines() {
                println!("{line}");
            }
        }
    }
    // Replay stderr
    if !hit.stderr.is_empty() {
        if let Ok(stderr_str) = std::str::from_utf8(&hit.stderr) {
            for line in stderr_str.lines() {
                eprintln!("{line}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::cli::OutputMode;
    use crate::progress::ProgressReporter;
    use luchta_cache::{decide, FileDelta, ReportInput, RunReason, SCHEMA_VERSION_V4};
    use luchta_engine::CollectedReport;
    use std::sync::atomic::AtomicBool;

    /// Fast-stop latch: first failure in default mode sets both any_failed and interrupted.
    #[tokio::test]
    async fn fast_stop_latch_default_mode_sets_any_failed_and_interrupted() {
        let (first_call, any_failed, interrupted) = run_fast_stop_latch_case(false).await;

        assert!(first_call);
        assert!(any_failed, "any_failed should be set");
        assert!(interrupted, "default mode should set interrupted");
    }

    async fn run_fast_stop_latch_case(continue_on_failure: bool) -> (bool, bool, bool) {
        let any_failed = Arc::new(AtomicBool::new(false));
        let interrupted = Arc::new(AtomicBool::new(false));
        let worker_manager = Arc::new(WorkerManager::new(HashMap::new()));

        let first_call = trigger_fast_stop_on_first_failure(
            &any_failed,
            &interrupted,
            continue_on_failure,
            true,
            &worker_manager,
        );
        tokio::task::yield_now().await;

        (
            first_call,
            any_failed.load(Ordering::SeqCst),
            interrupted.load(Ordering::SeqCst),
        )
    }

    struct FastStopInvalidTaskFixture {
        task_id: TaskId,
        ctx: DispatchContext<'static>,
    }

    impl FastStopInvalidTaskFixture {
        fn new() -> Self {
            let temp_dir = Box::leak(Box::new(tempfile::tempdir().expect("create temp dir")));
            let package = make_test_package(temp_dir.path());
            let package_graph = Box::leak(Box::new(build_test_package_graph(&package)));
            let task_graph = Box::leak(Box::new(build_test_task_graph(package_graph)));
            let task_id = TaskId::new("pkg", "invalid");
            let reporter = Box::leak(Box::new(Arc::new(ProgressReporter::new(
                OutputMode::Default,
                HashMap::from([(task_id.clone(), 0)]),
                1,
            ))));

            let cache = Box::leak(Box::new(open_test_cache(temp_dir.path())));
            let output_hashes = Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
            let ctx = DispatchContext {
                tasks_to_run: Box::leak(Box::new(HashSet::from([task_id.clone()]))),
                commands: Box::leak(Box::new(HashMap::new())),
                invalid: Box::leak(Box::new(HashMap::from([(
                    task_id.clone(),
                    "task missing worker".to_string(),
                )]))),
                executor: Box::leak(Box::new(Arc::new(WeightedExecutor::new(1)))),
                any_failed: Box::leak(Box::new(Arc::new(AtomicBool::new(true)))),
                interrupted: Box::leak(Box::new(Arc::new(AtomicBool::new(false)))),
                continue_on_failure: false,
                no_cache: false,
                owns_worker_manager: true,
                worker_manager: Box::leak(Box::new(Arc::new(WorkerManager::new(HashMap::new())))),
                workspace_root: temp_dir.path(),
                packages: Box::leak(Box::new(vec![package.clone()])),
                task_graph,
                cache,
                output_hashes,
                reporter,
                shared_cache: None,
                decision_ctx: DecisionContext {
                    task_envs: Arc::new(HashMap::new()),
                    workspace_root: temp_dir.path().to_path_buf(),
                    package_graph: Arc::new(package_graph.clone()),
                    packages: Arc::new(vec![package.clone()]),
                    task_graph: Arc::new(task_graph.clone()),
                    cache: Arc::clone(cache),
                    output_hashes: Arc::clone(output_hashes),
                    lockfile: Arc::new(LockfileState::Absent),
                    shared_cache: None,
                    listing_cache: Arc::new(ListingCache::default()),
                    workers: Arc::new(HashMap::new()),
                    global_cache_nonce: None,
                    env_cache_nonce: None,
                    reporter: Arc::clone(reporter),
                    task_watch_registry: crate::watch::registry::empty_task_watch_registry(),
                },
            };

            Self { task_id, ctx }
        }

        fn task_node(&self) -> TaskNode {
            TaskNode {
                id: self.task_id.clone(),
                weight: 1,
            }
        }
    }

    fn assert_skip_progress_without_failure_marker(reporter: &ProgressReporter) {
        let progress = reporter.render_progress(
            "0 MB",
            &[],
            &crate::memory_pressure::PressureSnapshot {
                reasons: Vec::new(),
                sample: None,
                usage_threshold: 0,
                free_threshold: 0,
            },
            owo_colors::Stream::Stdout,
        );
        assert!(
            progress.contains("⌛ 1"),
            "skip path should leave task uncounted in pending bucket: {progress}"
        );
        assert!(
            !progress.contains("× 1") && !progress.contains('✖'),
            "skip path must not render failed marker: {progress}"
        );
    }
    /// Fast-stop gate ordering: prior failure suppresses later invalid ready task.
    fn make_test_package(workspace_root: &Path) -> PackageNode {
        let package_dir = workspace_root.join("packages/pkg");
        std::fs::create_dir_all(&package_dir).expect("create package dir");
        std::fs::write(
            package_dir.join("package.json"),
            serde_json::json!({
                "name": "pkg",
                "version": "1.0.0",
            })
            .to_string(),
        )
        .expect("write package manifest");
        PackageNode::new(PackageName::from("pkg"), &package_dir)
    }

    fn build_test_package_graph(package: &PackageNode) -> PackageGraph {
        PackageGraph::build(vec![package.clone()]).expect("build package graph")
    }

    /// Initialize a git repo at `root`. Input resolution walks up to a git repo
    /// boundary, so tests that resolve real inputs need one.
    fn init_git_repo(root: &Path) {
        use std::process::Command;
        let ok = Command::new("git")
            .arg("init")
            .current_dir(root)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git init failed for {}", root.display());
    }

    fn build_test_task_graph(package_graph: &PackageGraph) -> TaskGraph {
        let pipeline = HashMap::from([(
            TaskName::from("invalid"),
            TaskDefinition {
                worker: Some("missing-worker".to_string()),
                ..TaskDefinition::default()
            },
        )]);
        TaskGraph::build(package_graph, &pipeline).expect("build task graph")
    }

    fn open_test_cache(workspace_root: &Path) -> Arc<Cache> {
        Arc::new(Cache::open(&workspace_root.join(".luchta/cache")).expect("open cache"))
    }

    fn sample_cache_write_context(task_id: TaskId) -> CacheWriteContext {
        let root = tempfile::tempdir().expect("tempdir").keep();
        let package = make_test_package(&root);
        let package_graph = Arc::new(build_test_package_graph(&package));
        CacheWriteContext {
            task_id,
            task_def: TaskDefinition::default(),
            package_path: package.path.to_path_buf(),
            dep_outputs: BTreeMap::new(),
            task_spec_hash: [1; 32],
            env_hash: [2; 32],
            pkg_dep_hash: [3; 32],
            start_unix_ms: 10,
            repo_root: root,
            source_pkg: PackageName::from("pkg"),
            package_graph,
            cache_nonce: None,
            decision: CacheDecisionContext {
                action: Decision::Run,
                run_reason: RunReason::NoPriorRecord,
            },
            task_watch_registry: crate::watch::registry::empty_task_watch_registry(),
            pre_snapshot: Some(Vec::new()),
        }
    }

    #[test]
    fn capture_pre_execution_snapshot_is_lazy_and_populates_on_demand() {
        // Regression guard for the no-op-build perf fix: the pre-execution input
        // snapshot must NOT be computed until a task is committed to run. A fresh
        // CacheWriteContext (as produced on the cache-skip decision path) must have
        // `pre_snapshot == None`; only an explicit `capture_pre_execution_snapshot`
        // populates it (before the task is dispatched, preserving the #157 TOCTOU
        // guarantee for tasks that actually run).
        let task_id = TaskId::new("pkg", "build");
        let mut cache_ctx = sample_cache_write_context(task_id);
        // Simulate the state right after build_cache_write_context: not captured.
        cache_ctx.pre_snapshot = None;
        cache_ctx.task_def.inputs = vec!["src.txt".to_string()];
        init_git_repo(&cache_ctx.repo_root);
        std::fs::write(cache_ctx.package_path.join("src.txt"), "H1\n").expect("write input");

        // Skip path never captures: accessor yields an empty slice, no hashing done.
        assert!(
            cache_ctx.pre_snapshot.is_none(),
            "pre_snapshot must remain None until a task is committed to run"
        );
        assert!(cache_ctx.pre_snapshot().is_empty());

        // Run path captures the baseline before dispatch.
        cache_ctx.capture_pre_execution_snapshot();
        assert!(
            cache_ctx
                .pre_snapshot
                .as_ref()
                .is_some_and(|snapshot| !snapshot.is_empty()),
            "capture_pre_execution_snapshot should populate the declared input baseline"
        );

        // Idempotent: a second call must not re-hash or clobber the captured snapshot.
        let captured = cache_ctx.pre_snapshot.clone();
        cache_ctx.capture_pre_execution_snapshot();
        assert_eq!(
            cache_ctx.pre_snapshot, captured,
            "capture_pre_execution_snapshot must be idempotent"
        );
    }

    #[test]
    fn build_run_record_persists_supplied_run_reason() {
        let temp = tempfile::tempdir().expect("tempdir");
        let task_id = TaskId::new("pkg", "build");
        let mut cache_ctx = sample_cache_write_context(task_id);
        cache_ctx.repo_root = temp.path().to_path_buf();
        cache_ctx.package_path = temp.path().to_path_buf();
        std::fs::write(temp.path().join("src.txt"), "hello\n").expect("write input");
        cache_ctx.task_def.inputs = vec!["src.txt".to_string()];
        let run_reason = RunReason::InputChanged {
            changed: vec![FileDelta {
                path: "src.txt".to_string(),
                prior_hash: [0; 32],
                current_hash: [1; 32],
                prior_absent: false,
                current_absent: false,
            }],
            truncated: false,
            change_count: 1,
        };

        let record = match build_run_record(
            &cache_ctx,
            BuildRunRecordArgs {
                outcome: None,
                succeeded: true,
                end_unix_ms: 20,
                run_reason: Some(run_reason.clone()),
            },
        ) {
            BuildRecordResult::Ok(record) => record,
            BuildRecordResult::ExpansionError(msg) => panic!("unexpected expansion error: {msg}"),
            BuildRecordResult::StabilityMismatch(msg) => {
                panic!("unexpected stability mismatch: {msg}")
            }
        };

        assert_eq!(record.schema_version, SCHEMA_VERSION_V4);
        assert_eq!(record.run_reason, Some(run_reason));
    }

    #[test]
    fn build_run_record_declared_input_changed_mid_run_is_stability_mismatch() {
        // A declared input is edited DURING the run: the pre-snapshot captured
        // its pre-run state, but the post-run resolution sees the new content.
        // build_run_record must return StabilityMismatch and must NOT register
        // watch state (so the task stays dirty for the next watch cycle).
        let task_id = TaskId::new("pkg", "build");
        let mut cache_ctx = sample_cache_write_context(task_id.clone());
        cache_ctx.task_def.inputs = vec!["src.txt".to_string()];
        init_git_repo(&cache_ctx.repo_root);
        let input_path = cache_ctx.package_path.join("src.txt");

        // Pre-run state H1, captured into the pre-snapshot.
        std::fs::write(&input_path, "H1\n").expect("write input");
        cache_ctx.pre_snapshot = Some(resolve_pre_execution_inputs(
            &cache_ctx.task_def.inputs,
            &cache_ctx.source_pkg,
            &cache_ctx.package_graph,
            &cache_ctx.repo_root,
        ));
        assert!(
            cache_ctx
                .pre_snapshot
                .as_ref()
                .is_some_and(|snapshot| !snapshot.is_empty()),
            "pre-snapshot should capture the declared input"
        );

        // Concurrent edit to H2 before the record is built (post-run resolution).
        std::fs::write(&input_path, "H2-changed\n").expect("edit input");

        let result = build_run_record(
            &cache_ctx,
            BuildRunRecordArgs {
                outcome: None,
                succeeded: true,
                end_unix_ms: 20,
                run_reason: Some(RunReason::NoPriorRecord),
            },
        );

        match result {
            BuildRecordResult::StabilityMismatch(reason) => {
                assert!(
                    reason.contains("changed during execution"),
                    "reason should describe the concurrent change: {reason}"
                );
            }
            BuildRecordResult::Ok(_) => {
                panic!("expected StabilityMismatch when a declared input changed mid-run")
            }
            BuildRecordResult::ExpansionError(msg) => panic!("unexpected expansion error: {msg}"),
        }

        // Watch state must NOT be registered for an unstable run.
        assert!(
            cache_ctx
                .task_watch_registry
                .lock()
                .expect("lock watch registry")
                .is_empty(),
            "stability mismatch must not register watch state"
        );
    }

    #[tokio::test]
    async fn write_run_record_skips_cache_on_stability_mismatch() {
        // End-to-end of the write path: a declared input changes mid-run, so
        // write_run_record must return StabilityMismatch and leave the local
        // cache untouched (no record persisted).
        let cache_dir = tempfile::tempdir().expect("cache tempdir");
        let cache =
            Arc::new(Cache::open(&cache_dir.path().join(".luchta/cache")).expect("open cache"));
        let task_id = TaskId::new("pkg", "build");
        let mut cache_ctx = sample_cache_write_context(task_id.clone());
        cache_ctx.task_def.inputs = vec!["src.txt".to_string()];
        init_git_repo(&cache_ctx.repo_root);
        let repo_root = cache_ctx.repo_root.clone();
        let input_path = cache_ctx.package_path.join("src.txt");

        std::fs::write(&input_path, "H1\n").expect("write input");
        cache_ctx.pre_snapshot = Some(resolve_pre_execution_inputs(
            &cache_ctx.task_def.inputs,
            &cache_ctx.source_pkg,
            &cache_ctx.package_graph,
            &cache_ctx.repo_root,
        ));
        // Concurrent edit before the record is written.
        std::fs::write(&input_path, "H2-changed\n").expect("edit input");

        let output_hashes = Arc::new(Mutex::new(HashMap::new()));
        let result = write_run_record(
            Arc::clone(&cache),
            cache_ctx,
            Arc::clone(&output_hashes),
            None,
            None,
            true,
            20,
            Some(RunReason::NoPriorRecord),
            None,
            false,
            repo_root,
        )
        .await;

        assert!(
            matches!(result, WriteRecordResult::StabilityMismatch(_)),
            "write should report a stability mismatch"
        );
        assert!(
            cache.read(&task_id.to_string()).is_none(),
            "no cache record should be written on a stability mismatch"
        );
        assert!(
            output_hashes.lock().expect("lock output hashes").is_empty(),
            "no output hash should be recorded on a stability mismatch"
        );
    }

    #[test]
    fn build_run_record_skip_context_does_not_persist_reason_without_param() {
        let temp = tempfile::tempdir().expect("tempdir");
        let task_id = TaskId::new("pkg", "build");
        let mut cache_ctx = sample_cache_write_context(task_id);
        cache_ctx.repo_root = temp.path().to_path_buf();
        cache_ctx.package_path = temp.path().to_path_buf();
        std::fs::write(temp.path().join("src.txt"), "hello\n").expect("write input");
        cache_ctx.task_def.inputs = vec!["src.txt".to_string()];
        cache_ctx.decision = CacheDecisionContext {
            action: Decision::Skip,
            run_reason: RunReason::SharedCacheHit,
        };

        let record = match build_run_record(
            &cache_ctx,
            BuildRunRecordArgs {
                outcome: None,
                succeeded: true,
                end_unix_ms: 20,
                run_reason: None,
            },
        ) {
            BuildRecordResult::Ok(record) => record,
            BuildRecordResult::ExpansionError(msg) => panic!("unexpected expansion error: {msg}"),
            BuildRecordResult::StabilityMismatch(msg) => {
                panic!("unexpected stability mismatch: {msg}")
            }
        };

        assert_eq!(record.run_reason, None);
    }

    #[test]
    fn replay_logs_accepts_restored_hit_output() {
        let reporter = Arc::new(ProgressReporter::new(
            OutputMode::Default,
            HashMap::new(),
            0,
        ));
        let task_id = TaskId::new("pkg", "build");
        let record = match build_run_record(
            &sample_cache_write_context(task_id),
            BuildRunRecordArgs {
                outcome: None,
                succeeded: true,
                end_unix_ms: 20,
                run_reason: Some(RunReason::NoPriorRecord),
            },
        ) {
            BuildRecordResult::Ok(record) => *record,
            BuildRecordResult::ExpansionError(msg) => panic!("unexpected expansion error: {msg}"),
            BuildRecordResult::StabilityMismatch(msg) => {
                panic!("unexpected stability mismatch: {msg}")
            }
        };
        let hit = RestoredHit {
            record,
            outputs_hash: [9; 32],
            stdout: b"restored stdout\n".to_vec(),
            stderr: b"restored stderr\n".to_vec(),
            reports: Vec::new(),
        };

        // Restored stdout/stderr replay for a shared-cache hit must not panic and
        // is wired back into the shared-cache-hit path (regression guard: this
        // call was dropped during the owned-decision-context refactor).
        replay_logs(&hit, &reporter);
    }

    #[test]
    fn hydrate_local_cache_marks_shared_cache_hit_reason() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cache = Arc::new(Cache::open(&temp.path().join(".luchta/cache")).expect("open cache"));
        let task_id = TaskId::new("pkg", "build");
        let mut record = match build_run_record(
            &sample_cache_write_context(task_id.clone()),
            BuildRunRecordArgs {
                outcome: None,
                succeeded: true,
                end_unix_ms: 20,
                run_reason: Some(RunReason::NoPriorRecord),
            },
        ) {
            BuildRecordResult::Ok(record) => *record,
            BuildRecordResult::ExpansionError(msg) => panic!("unexpected expansion error: {msg}"),
            BuildRecordResult::StabilityMismatch(msg) => {
                panic!("unexpected stability mismatch: {msg}")
            }
        };
        record.reports = vec![luchta_cache::ReportMeta {
            filename: "report.txt".to_string(),
            mime_type: "text/plain".to_string(),
        }];
        let hit = RestoredHit {
            record,
            outputs_hash: [9; 32],
            stdout: b"stdout".to_vec(),
            stderr: b"stderr".to_vec(),
            reports: vec![ReportInput {
                filename: "report.txt".to_string(),
                mime_type: "text/plain".to_string(),
                content: "report body".to_string(),
            }],
        };

        hydrate_local_cache(Arc::clone(&cache), task_id.clone(), &hit);

        let hydrated = cache
            .read(&task_id.to_string())
            .expect("hydrated record should exist");
        assert_eq!(hydrated.schema_version, SCHEMA_VERSION_V4);
        assert_eq!(hydrated.run_reason, Some(RunReason::SharedCacheHit));
    }

    fn build_skip_reason_fixture() -> (tempfile::TempDir, Arc<Cache>, TaskId, CacheWriteContext) {
        let temp = tempfile::tempdir().expect("tempdir");
        let cache = Arc::new(Cache::open(&temp.path().join(".luchta/cache")).expect("open cache"));
        let task_id = TaskId::new("pkg", "build");
        let mut cache_ctx = sample_cache_write_context(task_id.clone());
        let package = make_test_package(temp.path());
        cache_ctx.repo_root = temp.path().to_path_buf();
        cache_ctx.package_path = package.path.to_path_buf();
        cache_ctx.package_graph = Arc::new(build_test_package_graph(&package));
        std::fs::write(package.path.join("src.txt"), "hello\n").expect("write input");
        cache_ctx.task_def.inputs = vec!["src.txt".to_string()];
        (temp, cache, task_id, cache_ctx)
    }

    fn resolver_for_cache_ctx(cache_ctx: &CacheWriteContext) -> PackageDirResolver {
        PackageDirResolver::new(
            cache_ctx.package_path.clone(),
            cache_ctx.repo_root.clone(),
            cache_ctx.source_pkg.clone(),
            cache_ctx.package_graph.clone(),
            std::sync::Arc::new(luchta_cache::ListingCache::default()),
        )
    }

    #[test]
    fn decide_skip_leaves_prior_record_reason_untouched() {
        let (_temp, cache, task_id, cache_ctx) = build_skip_reason_fixture();
        let prior_reason = RunReason::InputChanged {
            changed: vec![FileDelta {
                path: "src.txt".to_string(),
                prior_hash: [0; 32],
                current_hash: [1; 32],
                prior_absent: false,
                current_absent: false,
            }],
            truncated: false,
            change_count: 1,
        };
        let prior_record = match build_run_record(
            &cache_ctx,
            BuildRunRecordArgs {
                outcome: None,
                succeeded: true,
                end_unix_ms: 20,
                run_reason: Some(prior_reason.clone()),
            },
        ) {
            BuildRecordResult::Ok(record) => record,
            BuildRecordResult::ExpansionError(msg) => {
                panic!("unexpected expansion error: {msg}")
            }
            BuildRecordResult::StabilityMismatch(msg) => {
                panic!("unexpected stability mismatch: {msg}")
            }
        };
        cache
            .write(
                &task_id.to_string(),
                RunArtifacts {
                    record: &prior_record,
                    stdout: b"",
                    stderr: b"",
                    reports: &[],
                },
            )
            .expect("write prior record");

        let env = BTreeMap::new();
        let resolver = resolver_for_cache_ctx(&cache_ctx);
        let current = build_current_state(
            &cache_ctx.task_def,
            &env,
            cache_ctx.dep_outputs.clone(),
            &[],
            &resolver,
            cache_ctx.cache_nonce.as_deref(),
        );
        let decision = decide(cache.read(&task_id.to_string()).as_ref(), &current);

        assert!(
            matches!(decision.action, Decision::Skip | Decision::Run),
            "skip semantics under test: record must remain untouched regardless of current action"
        );
        let persisted = cache
            .read(&task_id.to_string())
            .expect("record should remain");
        assert_eq!(persisted.run_reason, Some(prior_reason));
    }

    #[test]
    fn dispatch_ready_task_skips_invalid_task_after_prior_failure_in_default_mode() {
        let fixture = FastStopInvalidTaskFixture::new();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();

        dispatch_ready_task_async(
            fixture.task_node(),
            done_tx,
            &fixture.ctx,
            Arc::new(tokio::sync::Semaphore::new(1)),
            tokio::sync::mpsc::channel(1).0,
        );

        assert_eq!(
            done_rx.blocking_recv(),
            Ok(false),
            "fast-stop skip should report incomplete downstream signal"
        );
        assert_eq!(
            fixture.ctx.reporter.failed_count(),
            0,
            "invalid task after prior failure must not be counted as failed"
        );
        assert_skip_progress_without_failure_marker(fixture.ctx.reporter.as_ref());
    }

    fn decision_result_execution_request(task_id: &TaskId) -> ExecutionRequest {
        ExecutionRequest {
            task: TaskNode {
                id: task_id.clone(),
                weight: 1,
            },
            command: "true".to_string(),
            cwd: None,
            env: HashMap::new(),
            worker: None,
            workspace: None,
            inputs: None,
            outputs: None,
            log_sink: None,
        }
    }

    // A parallel cache-skip decision that resolves to Skip must be finalized
    // synchronously on the result-handling path: it records completion via
    // done_tx(true) and yields no ReadyTask (nothing to dispatch to the executor).
    #[test]
    fn dispatch_decision_result_skip_completes_and_yields_no_ready_task() {
        let fixture = FastStopInvalidTaskFixture::new();
        let task_id = fixture.task_id.clone();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();

        let result = DecisionTaskResult {
            task_id: task_id.clone(),
            request: decision_result_execution_request(&task_id),
            done_tx,
            cache_enabled: true,
            outcome: DecisionOutcome::Parallelizable {
                decision_result: Ok(Some(Decision::Skip)),
            },
        };

        let ready = dispatch_decision_result(result, &fixture.ctx).expect("decision result ok");
        assert!(
            ready.is_none(),
            "a cache-skip decision must not produce a ReadyTask to dispatch"
        );
        assert_eq!(
            done_rx.blocking_recv(),
            Ok(true),
            "cache skip must signal successful completion via done_tx"
        );
    }

    // A decision that resolves to Run must be deferred to the pressure-gated
    // dispatch path: dispatch_decision_result returns the ReadyTask (still
    // owning done_tx) rather than completing it here.
    #[test]
    fn dispatch_decision_result_run_defers_ready_task_without_completing() {
        let fixture = FastStopInvalidTaskFixture::new();
        let task_id = fixture.task_id.clone();
        let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();

        let result = DecisionTaskResult {
            task_id: task_id.clone(),
            request: decision_result_execution_request(&task_id),
            done_tx,
            cache_enabled: true,
            outcome: DecisionOutcome::Direct {
                decision: Some(Decision::Run),
            },
        };

        let ready = dispatch_decision_result(result, &fixture.ctx).expect("decision result ok");
        let ready = ready.expect("Run decision must yield a ReadyTask for dispatch");
        assert_eq!(ready.task_id, task_id);
        assert!(
            ready.cache_enabled,
            "cache_enabled flag must be propagated to the ReadyTask"
        );
        // done_tx must still be owned by the ReadyTask (not yet fired): the
        // receiver sees the channel as still open with no value.
        assert!(
            matches!(
                done_rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ),
            "Run path must not complete done_tx during decision handling"
        );
    }

    // A cache-disabled task (or a shared-cache fallback miss) yields a `Direct`
    // outcome with `None` decision. It must be treated as Run: no completion is
    // signalled here; the task is returned for pressure-gated dispatch.
    #[test]
    fn dispatch_decision_result_direct_none_defers_as_run() {
        let fixture = FastStopInvalidTaskFixture::new();
        let task_id = fixture.task_id.clone();
        let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();

        let result = DecisionTaskResult {
            task_id: task_id.clone(),
            request: decision_result_execution_request(&task_id),
            done_tx,
            cache_enabled: false,
            outcome: DecisionOutcome::Direct { decision: None },
        };

        let ready = dispatch_decision_result(result, &fixture.ctx).expect("decision result ok");
        let ready = ready.expect("a task with no cache decision must be dispatched to run");
        assert_eq!(ready.task_id, task_id);
        assert!(!ready.cache_enabled);
        assert!(
            matches!(
                done_rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ),
            "run path must not complete done_tx during decision handling"
        );
    }

    // The bounded decision-parallelism used for the semaphore must never be zero;
    // a zero-permit semaphore would deadlock every cache-skip decision.
    #[test]
    fn decision_parallelism_is_at_least_one() {
        assert!(super::super::pause::decision_parallelism_for_test() >= 1);
    }

    /// Fast-stop latch: continue mode sets any_failed but leaves interrupted false.
    #[tokio::test]
    async fn fast_stop_latch_continue_mode_leaves_interrupted_false() {
        let (first_call, any_failed, interrupted) = run_fast_stop_latch_case(true).await;

        assert!(first_call);
        assert!(any_failed, "any_failed should be set");
        assert!(!interrupted, "continue mode should NOT set interrupted");
    }

    /// Fast-stop latch: second call returns false and does not re-trigger state transitions.
    #[tokio::test]
    async fn fast_stop_latch_second_call_returns_false_without_retrigger() {
        let any_failed = Arc::new(AtomicBool::new(false));
        let interrupted = Arc::new(AtomicBool::new(false));
        let worker_manager = Arc::new(WorkerManager::new(HashMap::new()));

        let first_call = trigger_fast_stop_on_first_failure(
            &any_failed,
            &interrupted,
            false,
            true,
            &worker_manager,
        );
        tokio::task::yield_now().await;
        assert!(first_call);
        assert!(interrupted.load(Ordering::SeqCst));

        interrupted.store(false, Ordering::SeqCst);
        let second_call = trigger_fast_stop_on_first_failure(
            &any_failed,
            &interrupted,
            false,
            true,
            &worker_manager,
        );
        tokio::task::yield_now().await;

        assert!(
            !second_call,
            "second call should return false after first failure latched"
        );
        assert!(
            any_failed.load(Ordering::SeqCst),
            "any_failed should stay set"
        );
        assert!(
            !interrupted.load(Ordering::SeqCst),
            "second call should not re-trigger interrupted once caller clears it"
        );
    }

    // Tests for outputs_lexically_in_package read-time scope gate.
    // This gate determines shared-cache eligibility before outputs exist.

    #[test]
    fn in_package_outputs_are_eligible() {
        // Simple relative paths within package are eligible
        let outputs = vec![
            "out.txt".to_string(),
            "dist/bundle.js".to_string(),
            "build/output.wasm".to_string(),
        ];
        assert!(
            outputs_lexically_in_package(&outputs),
            "simple relative paths should be eligible"
        );
    }

    #[test]
    fn absolute_path_output_is_ineligible() {
        // Absolute paths escape package boundary
        let outputs = vec!["/tmp/output.txt".to_string()];
        assert!(
            !outputs_lexically_in_package(&outputs),
            "absolute path should be ineligible"
        );
    }

    #[test]
    fn parent_traversal_output_is_ineligible() {
        // Starting with ../ escapes package
        let outputs = vec!["../escape.txt".to_string()];
        assert!(
            !outputs_lexically_in_package(&outputs),
            "path starting with ../ should be ineligible"
        );
    }

    #[test]
    fn embedded_parent_traversal_is_ineligible() {
        // Embedded /../ in middle of path also escapes
        let outputs = vec!["subdir/../escape.txt".to_string()];
        assert!(
            !outputs_lexically_in_package(&outputs),
            "path containing /../ should be ineligible"
        );
    }

    #[test]
    fn trailing_parent_is_ineligible() {
        // Path ending in /.. or being ".." is escape
        let outputs1 = vec!["subdir/..".to_string()];
        assert!(
            !outputs_lexically_in_package(&outputs1),
            "path ending in /.. should be ineligible"
        );

        let outputs2 = vec!["..".to_string()];
        assert!(
            !outputs_lexically_in_package(&outputs2),
            "bare '..' should be ineligible"
        );
    }

    #[test]
    fn mixed_outputs_one_escape_makes_ineligible() {
        // Even if one output is safe, any escape makes task ineligible
        let outputs = vec!["safe.txt".to_string(), "../escape.txt".to_string()];
        assert!(
            !outputs_lexically_in_package(&outputs),
            "any escaping pattern makes task ineligible"
        );
    }

    #[test]
    fn format_captured_failure_logs_includes_reports_inside_block() {
        let task_id = TaskId::new("pkg", "build");
        let sink = ExecutionLogSink::new();
        sink.push(LogStream::Stdout, "stdout line");
        sink.push_report(CollectedReport {
            filename: "report.sarif".to_string(),
            mime_type: "application/sarif+json".to_string(),
            content: r#"{
                "version": "2.1.0",
                "runs": [{
                    "tool": { "driver": { "name": "test" } },
                    "results": [{
                        "level": "error",
                        "message": { "text": "Failure details" },
                        "locations": [{
                            "physicalLocation": {
                                "artifactLocation": { "uri": "src/main.rs" },
                                "region": { "startLine": 7, "startColumn": 2 }
                            }
                        }]
                    }]
                }]
            }"#
            .to_string(),
        });

        let rendered = format_captured_failure_logs(
            FailureLogContext {
                task_id: task_id.clone(),
                start_unix_ms: 10,
                end_unix_ms: 20,
                exit_status: Some(1),
                fallback_detail: None,
            },
            &sink,
        );
        let report_index = rendered
            .find("src/main.rs:7:2: error: Failure details")
            .unwrap();
        let footer_index = rendered.find("╰─").unwrap();

        assert!(rendered.contains("stdout line"));
        assert!(rendered.contains("src/main.rs:7:2: error: Failure details"));
        assert!(
            report_index < footer_index,
            "report must render before footer: {rendered}"
        );
    }

    #[test]
    fn format_captured_failure_logs_appends_fallback_detail_after_output() {
        let task_id = TaskId::new("app", "build");
        let sink = ExecutionLogSink::new();
        sink.push(LogStream::Stdout, "stdout line");
        sink.push(LogStream::Stderr, "stderr line");
        let detail =
            "failed: worker 'crash-worker' crashed during job 'app#build': exited with code 1";

        let rendered = format_captured_failure_logs(
            FailureLogContext {
                task_id: task_id.clone(),
                start_unix_ms: 10,
                end_unix_ms: 20,
                exit_status: None,
                fallback_detail: Some(detail.to_string()),
            },
            &sink,
        );
        let stdout_index = rendered.find("stdout line").unwrap();
        let stderr_index = rendered.find("stderr line").unwrap();
        let detail_index = rendered.find(detail).unwrap();
        let footer_index = rendered.find("╰─").unwrap();

        assert!(
            stdout_index < stderr_index && stderr_index < detail_index,
            "captured output should appear before fallback detail: {rendered}"
        );
        assert!(
            detail_index < footer_index,
            "fallback detail must render inside block before footer: {rendered}"
        );
        assert_eq!(
            rendered.matches(detail).count(),
            1,
            "fallback detail should appear exactly once: {rendered}"
        );
        assert!(
            rendered.contains("exit unknown") && rendered.contains("cache 71d474512380"),
            "missing failure footer: {rendered}"
        );
    }

    #[test]
    fn format_captured_failure_logs_uses_fallback_detail_when_output_empty() {
        let task_id = TaskId::new("app", "build");
        let sink = ExecutionLogSink::new();
        let detail =
            "failed: worker 'crash-worker' crashed during job 'app#build': exited with code 1";

        let rendered = format_captured_failure_logs(
            FailureLogContext {
                task_id: task_id.clone(),
                start_unix_ms: 10,
                end_unix_ms: 20,
                exit_status: None,
                fallback_detail: Some(detail.to_string()),
            },
            &sink,
        );

        assert!(
            rendered.contains("╭─"),
            "missing failure header: {rendered}"
        );
        assert!(
            rendered.contains(detail),
            "fallback detail should render inside block: {rendered}"
        );
        assert!(
            rendered.contains("╰─")
                && rendered.contains("exit unknown")
                && rendered.contains("cache 71d474512380"),
            "missing failure footer: {rendered}"
        );
    }
    #[test]
    fn empty_outputs_are_eligible() {
        // Empty output list has no escapes
        let outputs: Vec<String> = vec![];
        assert!(
            outputs_lexically_in_package(&outputs),
            "empty outputs should be eligible (no escapes)"
        );
    }
}
