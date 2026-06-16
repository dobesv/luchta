//! Per-task execution machinery: dispatching a ready task, running it, and
//! persisting its cache state. Extracted from `run.rs` to keep that module
//! cohesive (one responsibility per submodule).
//!
//! These helpers operate on the shared, read-only `DispatchContext` (defined in
//! the parent module). `use super::*` pulls in the parent's imports and private
//! items so the relocated code compiles unchanged.

use super::*;

use luchta_types::EnvSpec;
use luchta_worker::BUILTIN_PASSTHROUGH_ENV;

use crate::env_merge::merge_env;

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

fn print_captured_logs(sink: &ExecutionLogSink) {
    for line in sink.lines() {
        match line.stream {
            LogStream::Stdout => println!("{}", line.line),
            LogStream::Stderr => eprintln!("{}", line.line),
        }
    }
}

pub(super) fn dispatch_ready_task(
    task_node: TaskNode,
    done_tx: CompletionSignal,
    ctx: &DispatchContext<'_>,
) {
    let task_id = task_node.id.clone();

    if !ctx.tasks_to_run.contains(&task_id) {
        // Task not in requested subgraph — not counted.
        ctx.reporter.task_finished_other(&task_id);
        let _ = done_tx.send(true);
        return;
    }

    // A misconfigured task (e.g. command without worker) only fails when it is
    // actually selected to run — it must not abort unrelated tasks.
    if let Some(message) = ctx.invalid.get(&task_id) {
        ctx.any_failed.store(true, Ordering::SeqCst);
        eprintln!("{} {}", "✖".red(), message.red());
        // Invalid/config-error — NOT counted (failure path handles it).
        ctx.reporter.task_finished_other(&task_id);
        let _ = done_tx.send(false);
        return;
    }

    let Some(request) = ctx.commands.get(&task_id).cloned() else {
        // No command — treat ordering-only node as completed, not skipped.
        ctx.reporter.task_ran(&task_id);
        let _ = done_tx.send(true);
        return;
    };

    if ctx.any_failed.load(Ordering::SeqCst) {
        // Skipped due to previous failure — not counted.
        ctx.reporter.task_finished_other(&task_id);
        let _ = done_tx.send(false);
        return;
    }

    let cache_enabled = ctx
        .task_graph
        .task_definition(&task_id)
        .is_some_and(TaskDefinition::cache_enabled);
    if cache_enabled {
        if let Some(decision) = try_cache_skip(&task_id, ctx) {
            if matches!(decision, Decision::Skip) {
                // Cache hit — this IS the "skipped" count.
                ctx.reporter.task_skipped_cache_hit(&task_id);
                let _ = done_tx.send(true);
                return;
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
    cache_enabled: bool,
    ctx: &DispatchContext<'_>,
) -> TaskRunContext {
    let output_hash_record =
        build_output_hash_record_context(task_id, ctx.task_graph, ctx.packages, ctx.workspace_root);
    let cache_write = if cache_enabled {
        match build_cache_write_context(task_id, ctx) {
            CacheInputState::Ready(cache_ctx) => Some(*cache_ctx),
            CacheInputState::Disabled => None,
        }
    } else {
        None
    };

    TaskRunContext {
        executor: Arc::clone(ctx.executor),
        any_failed: Arc::clone(ctx.any_failed),
        interrupted: Arc::clone(ctx.interrupted),
        cache: Arc::clone(ctx.cache),
        output_hashes: Arc::clone(ctx.output_hashes),
        cache_write,
        output_hash_record,
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
    let resolver = PackageDirResolver::new(cache_package.package_path.clone());

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
    }))
}

fn build_run_record(
    cache_ctx: &CacheWriteContext,
    outcome: Option<&TaskRunOutcome>,
    succeeded: bool,
    end_unix_ms: u64,
) -> Option<TaskRunRecord> {
    let (output_patterns, detected_output_patterns) =
        effective_output_patterns(&cache_ctx.task_def, outcome);
    let (input_patterns, detected_input_patterns) =
        effective_input_patterns(&cache_ctx.task_def, outcome);
    let inputs = match resolve_inputs(&cache_ctx.package_path, &input_patterns) {
        Ok(inputs) => inputs,
        Err(error) => {
            eprintln!(
                "warning: skipping cache write for task '{}': failed to resolve cache inputs: {error}",
                cache_ctx.task_id
            );
            return None;
        }
    };
    let outputs = match resolve_outputs(&cache_ctx.package_path, &output_patterns) {
        Ok(outputs) => outputs,
        Err(error) => {
            eprintln!(
                "warning: skipping cache write for task '{}': failed to resolve cache outputs: {error}",
                cache_ctx.task_id
            );
            return None;
        }
    };
    let outputs_hash = combined_outputs_hash(&outputs);
    let exit_status = outcome
        .map(|result| result.status.code().unwrap_or(1))
        .unwrap_or(1);

    Some(TaskRunRecord {
        schema_version: SCHEMA_VERSION_V1,
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
    })
}

async fn write_run_record(
    cache: Arc<Cache>,
    cache_ctx: CacheWriteContext,
    output_hashes: Arc<Mutex<HashMap<TaskId, [u8; 32]>>>,
    log_sink: Option<&ExecutionLogSink>,
    outcome: Option<&TaskRunOutcome>,
    succeeded: bool,
    end_unix_ms: u64,
) {
    let Some(record) = build_run_record(&cache_ctx, outcome, succeeded, end_unix_ms) else {
        return;
    };
    record_output_hash(&output_hashes, &cache_ctx.task_id, record.outputs_hash);
    let (stdout, stderr) = log_sink.map(split_captured_logs).unwrap_or_default();
    let cache_key = cache_ctx.task_id.to_string();
    match tokio::task::spawn_blocking(move || {
        cache.write(
            &cache_key,
            RunArtifacts {
                record: &record,
                stdout: &stdout,
                stderr: &stderr,
            },
        )
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => eprintln!(
            "warning: failed to write cache record for task '{}': {error}",
            cache_ctx.task_id
        ),
        Err(error) => eprintln!(
            "warning: cache write task panicked for task '{}': {error}",
            cache_ctx.task_id
        ),
    }
}

fn report_task_outcome(
    task_id: &TaskId,
    outcome: &Result<TaskRunOutcome, luchta_engine::ExecutorError>,
    any_failed: &Arc<AtomicBool>,
    interrupted: &Arc<AtomicBool>,
) {
    match outcome {
        Ok(result) if result.status.success() => {}
        Ok(result) => report_task_failure(
            task_id,
            &format!("failed with status {:?}", result.status.code()),
            any_failed,
            interrupted,
        ),
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
    let resolver = PackageDirResolver::new(cache_package.package_path.clone());
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
    let current = build_current_state(task_def, merged_env, dep_outputs, &pkg_dep_pairs, &resolver);
    let prior = ctx.cache.read(&task_id.to_string());
    let decision = decide(prior.as_ref(), &current);
    if matches!(decision, Decision::Skip) {
        if let Some(p) = prior {
            record_output_hash(ctx.output_hashes, task_id, p.outputs_hash);
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
    let TaskRunContext {
        executor,
        any_failed,
        interrupted,
        cache,
        output_hashes,
        cache_write,
        output_hash_record,
    } = build_task_run_context(&task_id, cache_enabled, ctx);

    let log_sink = ExecutionLogSink::new();
    request.log_sink = Some(log_sink.clone());

    // Clone reporter Arc to move into the spawned future.
    let reporter = Arc::clone(ctx.reporter);

    let started_task_id = task_id.clone();

    tokio::spawn(async move {
        let outcome_res = executor
            .run_with_on_start(&request, {
                let reporter = Arc::clone(&reporter);
                move || reporter.task_started(&started_task_id)
            })
            .await;
        let end_unix_ms = now_unix_ms();
        let succeeded = matches!(&outcome_res, Ok(result) if result.status.success());
        // Override the declared output patterns with worker-detected outputs
        // (when emitted) so uncached-dependency coupling matches the cache-write
        // path's `effective_output_patterns` precedence.
        let output_hash_record = output_hash_record
            .map(|record| record.with_effective_patterns(outcome_res.as_ref().ok()));

        persist_cache_state(CachePersistInputs {
            cache,
            cache_write,
            output_hashes: &output_hashes,
            output_hash_record: output_hash_record.as_ref(),
            log_sink: Some(&log_sink),
            outcome: outcome_res.as_ref().ok(),
            succeeded,
            end_unix_ms,
        })
        .await;

        let interrupted_run = interrupted.load(Ordering::SeqCst);
        let failed = !succeeded;
        if failed && !interrupted_run {
            print_captured_logs(&log_sink);
        }

        report_task_outcome(&task_id, &outcome_res, &any_failed, &interrupted);

        // Report task completion to the progress reporter.
        if succeeded {
            reporter.task_ran(&task_id);
        } else {
            // Failed tasks are NOT counted in done/skipped.
            reporter.task_finished_other(&task_id);
        }

        let _ = done_tx.send(succeeded);
    });
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
}

/// Records the run record (cached tasks) or just the resolved output hash
/// (uncached tasks) so downstream dependency coupling stays correct.
async fn persist_cache_state(inputs: CachePersistInputs<'_>) {
    let CachePersistInputs {
        cache,
        cache_write,
        output_hashes,
        output_hash_record,
        log_sink,
        outcome,
        succeeded,
        end_unix_ms,
    } = inputs;

    if let Some(cache_ctx) = cache_write {
        write_run_record(
            cache,
            cache_ctx,
            Arc::clone(output_hashes),
            log_sink,
            outcome,
            succeeded,
            end_unix_ms,
        )
        .await;
        return;
    }

    if succeeded {
        if let Some(record) = output_hash_record {
            record_resolved_output_hash(output_hashes, record);
        }
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
            match resolve_non_worker_command(task_def) {
                NonWorkerCommand::NoOp => continue,
                NonWorkerCommand::CommandWithoutWorker => {
                    invalid.insert(
                        task_id.clone(),
                        format!(
                            "task '{task_id}' defines a command but no worker; specify a worker to execute it"
                        ),
                    );
                    continue;
                }
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
