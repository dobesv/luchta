//! Per-task execution machinery: dispatching a ready task, running it, and
//! persisting its cache state. Extracted from `run.rs` to keep that module
//! cohesive (one responsibility per submodule).
//!
//! These helpers operate on the shared, read-only `DispatchContext` (defined in
//! the parent module). `use super::*` pulls in the parent's imports and private
//! items so the relocated code compiles unchanged.

use super::*;

use luchta_cache::shared::{
    combined_dep_outputs_hash, derive_input_key, RestoredHit, StoreOutcome,
};
use luchta_cache::{
    decide_shared_restore, task_cache_key, FileEntry, ReportInput, RunArtifacts, SCHEMA_VERSION_V2,
};
use luchta_types::EnvSpec;
use luchta_worker::BUILTIN_PASSTHROUGH_ENV;

use crate::env_merge::merge_env;
use luchta_workspace::PackageGraph;

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

struct FailureLogContext<'a> {
    task_id: &'a TaskId,
    start_unix_ms: u64,
    end_unix_ms: u64,
    exit_status: Option<i32>,
}

fn format_captured_failure_logs(context: FailureLogContext<'_>, sink: &ExecutionLogSink) -> String {
    let FailureLogContext {
        task_id,
        start_unix_ms,
        end_unix_ms,
        exit_status,
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

    let lines: Vec<&str> = body.lines().collect();

    let cache_hash_full = task_cache_key(&task_id.to_string());
    let cache_hash_12 = &cache_hash_full[..12];
    let (package_display, task_display) = crate::format::package_and_task_display(task_id);

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
        },
        &body,
    )
}

pub(super) fn dispatch_ready_task(
    task_node: TaskNode,
    done_tx: CompletionSignal,
    ctx: &DispatchContext<'_>,
) {
    let task_id = task_node.id.clone();

    if !ctx.tasks_to_run.contains(&task_id) {
        // Task not in requested subgraph — not counted.
        ctx.reporter.task_finished_uncounted(&task_id);
        let _ = done_tx.send(true);
        return;
    }

    // A misconfigured task (e.g. command without worker) only fails when it is
    // actually selected to run — it must not abort unrelated tasks.
    if let Some(message) = ctx.invalid.get(&task_id) {
        ctx.any_failed.store(true, Ordering::SeqCst);
        eprintln!("{} {}", "✖".red(), message.red());
        // Invalid/config-error — counted in totals via wave map, but completion is
        // still recorded as uncounted because it is neither done nor cache-skipped.
        ctx.reporter.task_finished_uncounted(&task_id);
        let _ = done_tx.send(false);
        return;
    }

    let Some(request) = ctx.commands.get(&task_id).cloned() else {
        // No worker/no command ordering node — uncounted connector, not runnable work.
        ctx.reporter.task_finished_uncounted(&task_id);
        let _ = done_tx.send(true);
        return;
    };

    if ctx.any_failed.load(Ordering::SeqCst) {
        // Skipped due to previous failure — not counted.
        ctx.reporter.task_finished_uncounted(&task_id);
        let _ = done_tx.send(false);
        return;
    }

    let cache_enabled = ctx
        .task_graph
        .task_definition(&task_id)
        .is_some_and(TaskDefinition::cache_enabled);
    if cache_enabled {
        if let Some(decision) = try_cache_skip(&task_id, ctx) {
            match decision {
                Decision::Skip => {
                    // Local cache hit — this IS the legacy "skipped" count.
                    ctx.reporter.task_skipped_cache_hit(&task_id);
                    let _ = done_tx.send(true);
                    return;
                }
                Decision::SharedHit => {
                    ctx.reporter.task_skipped_shared_cache(&task_id);
                    let _ = done_tx.send(true);
                    return;
                }
                Decision::Run => {}
            }
        }
    }

    spawn_task_runner(
        ReadyTask {
            task_id,
            request,
            done_tx,
            cache_enabled,
        },
        ctx,
    );
}

fn build_task_run_context(
    task_id: &TaskId,
    _cache_enabled: bool,
    ctx: &DispatchContext<'_>,
) -> TaskRunContext {
    let output_hash_record =
        build_output_hash_record_context(task_id, ctx.task_graph, ctx.packages, ctx.workspace_root);
    let cache_write = match build_cache_write_context(task_id, ctx) {
        CacheInputState::Ready(cache_ctx) => Some(*cache_ctx),
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
        repo_root,
        task_ctx,
        task_start_unix_ms,
    } = run;
    let TaskRunContext {
        executor: _,
        any_failed,
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
    let expansion_error = persist_cache_state(CachePersistInputs {
        cache,
        cache_write,
        output_hashes: &output_hashes,
        output_hash_record: output_hash_record.as_ref(),
        log_sink: Some(&log_sink),
        outcome: outcome_res.as_ref().ok(),
        succeeded,
        end_unix_ms,
        shared_cache: cache_enabled.then_some(shared_cache).flatten(),
        shared_store_enabled: cache_enabled,
        repo_root,
    })
    .await;

    if let Some(expansion_error) = expansion_error {
        any_failed.store(true, Ordering::SeqCst);
        if !interrupted.load(Ordering::SeqCst) {
            eprintln!("{} {}", "✖".red(), expansion_error.red());
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
fn build_cache_write_context(task_id: &TaskId, ctx: &DispatchContext<'_>) -> CacheInputState {
    let Some(task_def) = ctx.task_graph.task_definition(task_id).cloned() else {
        return CacheInputState::Disabled;
    };
    let Some(cache_package) = cache_package_context_for(ctx.packages, ctx.workspace_root, task_id)
    else {
        return CacheInputState::Disabled;
    };
    let dep_outputs = dependency_output_hashes(task_id, ctx.task_graph, ctx.output_hashes);
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
    let pkg_dep_pairs = match gather_pkg_dep_pairs(
        package,
        cache_package.package.map(|_| ctx.package_graph),
        ctx.lockfile,
    ) {
        Ok(pkg_dep_pairs) => pkg_dep_pairs,
        Err(error) => {
            eprintln!(
                "warning: skipping cache write for task '{task_id}': failed to gather package dependencies: {error}"
            );
            return CacheInputState::Disabled;
        }
    };
    let resolver = PackageDirResolver::new(
        cache_package.package_path.clone(),
        ctx.workspace_root.to_path_buf(),
        cache_package.package_name.clone(),
        ctx.package_graph.clone(),
    );

    let empty = BTreeMap::new();
    let merged_env = ctx.task_envs.get(task_id).unwrap_or(&empty);
    let current = build_current_state(
        &task_def,
        merged_env,
        dep_outputs.clone(),
        &pkg_dep_pairs,
        &resolver,
    );
    let task_spec_hash = current.task_spec_hash;
    let env_hash = current.env_hash;
    let pkg_dep_hash = current.pkg_dep_hash;

    CacheInputState::Ready(Box::new(CacheWriteContext {
        task_id: task_id.clone(),
        task_def,
        package_path: cache_package.package_path,
        dep_outputs,
        task_spec_hash,
        env_hash,
        pkg_dep_hash,
        start_unix_ms: now_unix_ms(),
        repo_root: ctx.workspace_root.to_path_buf(),
        source_pkg: cache_package.package_name.clone(),
        package_graph: ctx.package_graph.clone(),
    }))
}

/// Result of building a run record for cache write.
/// Distinguishes between success, expansion errors (fatal), and IO/other errors (skip).
enum BuildRecordResult {
    Ok(Box<TaskRunRecord>),
    ExpansionError(String),
}

fn build_run_record(
    cache_ctx: &CacheWriteContext,
    outcome: Option<&TaskRunOutcome>,
    succeeded: bool,
    end_unix_ms: u64,
) -> BuildRecordResult {
    let (output_patterns, detected_output_patterns) =
        effective_output_patterns(&cache_ctx.task_def, outcome);
    let (input_patterns, detected_input_patterns) =
        effective_input_patterns(&cache_ctx.task_def, outcome);
    let inputs = match resolve_cache_inputs(cache_ctx, &input_patterns) {
        CacheInputResult::Ok(entries) => entries,
        CacheInputResult::ExpansionError(msg) => return BuildRecordResult::ExpansionError(msg),
        CacheInputResult::IoError => Vec::new(),
    };
    let outputs = resolve_cache_outputs(cache_ctx, &output_patterns).unwrap_or_default();
    let outputs_hash = combined_outputs_hash(&outputs);
    let exit_status = outcome
        .map(|result| result.status.code().unwrap_or(1))
        .unwrap_or(1);

    BuildRecordResult::Ok(Box::new(TaskRunRecord {
        schema_version: SCHEMA_VERSION_V2,
        task_spec_hash: cache_ctx.task_spec_hash,
        input_patterns,
        inputs,
        output_patterns,
        outputs,
        detected_input_patterns,
        detected_output_patterns,
        outputs_hash,
        env_hash: cache_ctx.env_hash,
        pkg_dep_hash: cache_ctx.pkg_dep_hash,
        dep_outputs: cache_ctx.dep_outputs.clone(),
        exit_status,
        succeeded,
        start_unix_ms: cache_ctx.start_unix_ms,
        end_unix_ms,
        reports: vec![],
    }))
}

/// Result of cache input resolution for the write path.
/// Distinguishes between expansion errors (fatal) and IO errors (warn + skip).
enum CacheInputResult {
    Ok(Vec<FileEntry>),
    ExpansionError(String),
    IoError,
}

fn resolve_cache_inputs(
    cache_ctx: &CacheWriteContext,
    input_patterns: &[String],
) -> CacheInputResult {
    let requests = match expand_input_patterns(
        input_patterns,
        &cache_ctx.source_pkg,
        &cache_ctx.package_graph,
        &cache_ctx.repo_root,
    ) {
        Ok(reqs) => reqs,
        Err(error) => {
            return CacheInputResult::ExpansionError(format!(
                "input \"{}\" in package \"{}\": {}",
                error.pattern(),
                cache_ctx.source_pkg,
                error
            ));
        }
    };

    match resolve_inputs_with_semantics(&requests) {
        Ok(inputs) => CacheInputResult::Ok(inputs),
        Err(error) => {
            eprintln!(
                "warning: failed to resolve cache inputs for task '{}': {error} — recording run with empty inputs",
                cache_ctx.task_id
            );
            CacheInputResult::IoError
        }
    }
}

fn resolve_cache_outputs(
    cache_ctx: &CacheWriteContext,
    output_patterns: &[String],
) -> Option<Vec<FileEntry>> {
    match resolve_outputs(&cache_ctx.package_path, output_patterns) {
        Ok(outputs) => Some(outputs),
        Err(error) => {
            eprintln!(
                "warning: failed to resolve cache outputs for task '{}': {error} — recording run with empty outputs",
                cache_ctx.task_id
            );
            None
        }
    }
}

/// Result of writing a run record to cache.
/// ExpansionError signals a fatal security error that must fail the task.
enum WriteRecordResult {
    Ok,
    ExpansionError(String),
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
    shared_cache: Option<Arc<SharedCache>>,
    shared_store_enabled: bool,
    repo_root: PathBuf,
) -> WriteRecordResult {
    let record = match build_run_record(&cache_ctx, outcome, succeeded, end_unix_ms) {
        BuildRecordResult::Ok(record) => record,
        BuildRecordResult::ExpansionError(msg) => return WriteRecordResult::ExpansionError(msg),
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

fn report_task_outcome(
    task_id: &TaskId,
    outcome: &Result<TaskRunOutcome, luchta_engine::ExecutorError>,
    any_failed: &Arc<AtomicBool>,
    interrupted: &Arc<AtomicBool>,
) {
    match outcome {
        Ok(result) if result.status.success() => {}
        Ok(result) => {
            let detail = match result.status.code() {
                Some(code) => format!("failed with status {code}"),
                None => "failed".to_string(),
            };
            report_task_failure(task_id, &detail, any_failed, interrupted)
        }
        Err(error) => {
            report_task_failure(task_id, &format_task_error(error), any_failed, interrupted)
        }
    }
}

fn format_task_error(error: &luchta_engine::ExecutorError) -> String {
    format!("failed: {error}")
}

fn try_cache_skip(task_id: &TaskId, ctx: &DispatchContext<'_>) -> Option<Decision> {
    let task_def = ctx.task_graph.task_definition(task_id)?;
    let cache_package = cache_package_context_for(ctx.packages, ctx.workspace_root, task_id)?;
    let resolver = PackageDirResolver::new(
        cache_package.package_path.clone(),
        ctx.workspace_root.to_path_buf(),
        cache_package.package_name.clone(),
        ctx.package_graph.clone(),
    );
    let dep_outputs = dependency_output_hashes(task_id, ctx.task_graph, ctx.output_hashes);
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
    let pkg_dep_pairs = match gather_pkg_dep_pairs(
        package,
        cache_package.package.map(|_| ctx.package_graph),
        ctx.lockfile,
    ) {
        Ok(pkg_dep_pairs) => pkg_dep_pairs,
        Err(error) => {
            eprintln!(
                "warning: skipping cache read for task '{task_id}': failed to gather package dependencies: {error}; task will run"
            );
            return Some(Decision::Run);
        }
    };
    let empty = BTreeMap::new();
    let merged_env = ctx.task_envs.get(task_id).unwrap_or(&empty);
    let current = build_current_state(
        task_def,
        merged_env,
        dep_outputs.clone(),
        &pkg_dep_pairs,
        &resolver,
    );
    let prior = ctx.cache.read(&task_id.to_string());
    let decision = decide(prior.as_ref(), &current);
    if matches!(decision, Decision::Skip) {
        if let Some(p) = prior {
            record_output_hash(ctx.output_hashes, task_id, p.outputs_hash);
        }
        return Some(decision);
    }

    // Local cache miss -> try shared cache if available.
    // Read-time scope gate: skip shared cache for tasks with outputs that lexically
    // escape the package directory (e.g. absolute paths or patterns starting with ../).
    if let Some(ref shared_cache) = ctx.shared_cache {
        if !outputs_lexically_in_package(&task_def.outputs) {
            // Outputs may escape package dir -> not read-eligible for shared cache.
            // Falls through to run normally (write-time scope check in P4.3).
            return Some(Decision::Run);
        }

        // Compute input_key from the SAME hashes used for local cache.
        let dep_outputs_hash = combined_dep_outputs_hash(&dep_outputs);
        let input_key = derive_input_key(
            current.task_spec_hash,
            current.env_hash,
            current.pkg_dep_hash,
            dep_outputs_hash,
        );

        // Try restore from shared cache with validation.
        // Iterate candidates newest-first; validate each before committing.
        for candidate in shared_cache.try_restore_candidates(
            &task_id.to_string(),
            &input_key,
            &cache_package.package_path,
        ) {
            // VALIDATE: Use decide_shared_restore to check if this candidate matches current tree state.
            // Unlike full decide(), this does NOT require outputs to exist in the tree —
            // we're ABOUT to restore outputs from the blob.
            if decide_shared_restore(&candidate.record, &current) {
                // Candidate is VALID - inputs match current tree.
                // Commit the staged restore.
                match candidate.commit() {
                    Ok(hit) => {
                        // Shared cache HIT (validated):
                        // (a) Outputs now restored to package dir.
                        // (b) Hydrate local cache for next build.
                        hydrate_local_cache(ctx.cache.clone(), task_id.clone(), &hit);
                        // (c) Replay logs via reporter (so output appears as if task ran).
                        replay_logs(&hit, ctx.reporter);
                        // (d) Record output hash for downstream invalidation.
                        record_output_hash(ctx.output_hashes, task_id, hit.outputs_hash);
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
    }

    Some(decision)
}

/// A ready task to spawn: what to run, where to report completion, and whether
/// caching applies. Groups the per-task parameters so `spawn_task_runner` stays
/// within a sane argument count.
struct ReadyTask {
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
    let task_ctx = build_task_run_context(&task_id, cache_enabled, ctx);
    let reporter = Arc::clone(ctx.reporter);
    let started_task_id = task_id.clone();
    let repo_root = ctx.workspace_root.to_path_buf();

    let executor = Arc::clone(&task_ctx.executor);
    let any_failed = Arc::clone(&task_ctx.any_failed);
    let interrupted = Arc::clone(&task_ctx.interrupted);
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
        log_sink,
        outcome_res,
        succeeded,
        start_unix_ms,
        end_unix_ms,
    } = finalization;

    let interrupted_run = interrupted.load(Ordering::SeqCst);
    if !succeeded && !interrupted_run {
        let failure_logs = format_captured_failure_logs(
            FailureLogContext {
                task_id,
                start_unix_ms,
                end_unix_ms,
                exit_status: outcome_res
                    .as_ref()
                    .ok()
                    .and_then(|result| result.status.code()),
            },
            log_sink,
        );
        eprint!("{}", failure_logs);
    }

    report_task_outcome(task_id, outcome_res, any_failed, interrupted);

    if succeeded {
        reporter.task_ran(task_id);
    } else {
        reporter.task_finished_uncounted(task_id);
    }

    let _ = done_tx.send(succeeded);
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
        end_unix_ms,
        shared_cache,
        shared_store_enabled,
        repo_root,
    } = inputs;

    if let Some(cache_ctx) = cache_write {
        let result = write_run_record(
            cache,
            cache_ctx,
            Arc::clone(output_hashes),
            log_sink,
            outcome,
            succeeded,
            end_unix_ms,
            shared_cache,
            shared_store_enabled,
            repo_root,
        )
        .await;
        return cache_write_error(result);
    }

    if succeeded {
        if let Some(record) = output_hash_record {
            record_resolved_output_hash(output_hashes, record);
        }
    }
    None
}

fn cache_write_error(result: WriteRecordResult) -> Option<String> {
    match result {
        WriteRecordResult::Ok => None,
        WriteRecordResult::ExpansionError(msg) => Some(msg),
    }
}

/// Marks the run as failed and prints a concise message, unless the run is
/// being interrupted (where killed jobs are expected and must stay quiet).
fn report_task_failure(
    task_id: &TaskId,
    detail: &str,
    any_failed: &Arc<AtomicBool>,
    interrupted: &Arc<AtomicBool>,
) {
    any_failed.store(true, Ordering::SeqCst);
    if !interrupted.load(Ordering::SeqCst) {
        eprintln!("task '{task_id}' {detail}");
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

struct CachePackageContext<'a> {
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

fn effective_input_patterns(
    task_def: &TaskDefinition,
    outcome: Option<&TaskRunOutcome>,
) -> (Vec<String>, bool) {
    match outcome.and_then(|o| o.detected_inputs.clone()) {
        Some(patterns) => (patterns, true),
        None => (task_def.inputs.clone(), false),
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
            record: &hit.record,
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
fn replay_logs(hit: &RestoredHit, _reporter: &Arc<ProgressReporter>) {
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
    use std::sync::atomic::AtomicBool;

    /// Test that report_task_outcome sets any_failed and formats messages correctly.

    #[test]
    fn report_task_outcome_sets_any_failed_on_failure() {
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;
        use std::process::ExitStatus;

        let any_failed = Arc::new(AtomicBool::new(false));
        let interrupted = Arc::new(AtomicBool::new(true)); // suppressed output
        let task_id: TaskId = TaskId::new("test-pkg", "test-task");

        // Exit status with code 1
        #[cfg(unix)]
        let status_with_code = ExitStatus::from_raw(256); // 1 << 8
        #[cfg(windows)]
        let status_with_code = ExitStatus::from_raw(1);

        let outcome_with_code: Result<TaskRunOutcome, luchta_engine::ExecutorError> =
            Ok(TaskRunOutcome {
                status: status_with_code,
                detected_inputs: None,
                detected_outputs: None,
            });

        report_task_outcome(&task_id, &outcome_with_code, &any_failed, &interrupted);

        // any_failed should be set
        assert!(any_failed.load(Ordering::SeqCst));
    }

    // Test the exact formatting logic used in report_task_outcome.
    // Verifies: (1) output contains "failed with status 1" for code Some(1)
    //           (2) output does NOT contain "Some("
    //           (3) output for signal-terminated (None) is "failed"
    #[test]
    fn status_code_format_omits_some_wrapper() {
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;
        use std::process::ExitStatus;

        // Simulate code() == Some(1)
        #[cfg(unix)]
        let status_with_code = ExitStatus::from_raw(256); // 1 << 8
        #[cfg(windows)]
        let status_with_code = ExitStatus::from_raw(1);

        let detail = match status_with_code.code() {
            Some(code) => format!("failed with status {code}"),
            None => "failed".to_string(),
        };

        assert!(
            !detail.contains("Some("),
            "detail must not contain 'Some(': got: {:?}",
            detail
        );
        assert_eq!(detail, "failed with status 1");

        // Simulate code() == None (signal termination on Unix)
        #[cfg(unix)]
        {
            let status_killed = ExitStatus::from_raw(9); // SIGKILL
            let detail_none = match status_killed.code() {
                Some(code) => format!("failed with status {code}"),
                None => "failed".to_string(),
            };

            assert!(
                !detail_none.contains("Some("),
                "detail must not contain 'Some(': got: {:?}",
                detail_none
            );
            assert_eq!(detail_none, "failed");
        }
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
    fn empty_outputs_are_eligible() {
        // Empty output list has no escapes
        let outputs: Vec<String> = vec![];
        assert!(
            outputs_lexically_in_package(&outputs),
            "empty outputs should be eligible (no escapes)"
        );
    }
}
