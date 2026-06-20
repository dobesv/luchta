//! Logs command implementation.
//!
//! Reads cached logs and metadata for configured tasks from the task graph,
//! printing each task's output as a wrapped header/body/footer block.

use luchta_cache::{resolve_cache_dir, task_cache_key, Cache, FileEntry};
use luchta_types::TaskId;
use miette::Result;

use crate::format::{
    format_task_log_block, format_unix_ms_local, package_and_task_display, LogBlockMeta,
};
use crate::run::{
    build_globset, collect_matched_package_names, collect_requested_subgraph, prepare_workspace,
    CollectSubgraphRequest, TaskSelection,
};
use luchta_engine::ResolveMode;

pub(crate) struct LogsOptions<'a> {
    pub tasks: &'a [String],
    pub packages: &'a [String],
    pub top_level: bool,
    pub time_taken: Option<u64>,
    pub failed: bool,
    pub show_inputs: bool,
    pub show_outputs: bool,
}

/// Execute the `luchta logs` command.
pub async fn execute_logs(
    workspace_root: &std::path::Path,
    options: &LogsOptions<'_>,
) -> Result<()> {
    let prepared = prepare_workspace(workspace_root, ResolveMode::Run, None).await?;
    prepared.worker_manager.shutdown().await;

    let selection = TaskSelection {
        requested_tasks: options.tasks,
        packages: options.packages,
        top_level: options.top_level,
        since: None,
    };
    let selected_ids = select_task_ids(&prepared.task_graph, &selection, &prepared.pruned)?;

    let cache_dir = resolve_cache_dir(workspace_root);
    let cache = Cache::open(&cache_dir).map_err(|e| miette::miette!("cache open failed: {}", e))?;

    let mut sorted_ids: Vec<TaskId> = selected_ids.into_iter().collect();
    sorted_ids.sort_by_key(|id| id.to_string());

    for task_id in sorted_ids {
        print_task_logs(&cache, &task_id, options);
    }

    Ok(())
}

fn select_task_ids(
    task_graph: &luchta_engine::TaskGraph,
    selection: &TaskSelection<'_>,
    pruned: &[luchta_engine::PrunedTask],
) -> Result<std::collections::HashSet<TaskId>> {
    if selection_uses_default_scope(selection) {
        return Ok(task_graph.nodes().map(|node| node.id.clone()).collect());
    }

    if selection_requests_only_top_level(selection) {
        return Ok(task_graph
            .nodes()
            .filter(|node| node.id.is_root())
            .map(|node| node.id.clone())
            .collect());
    }

    if selection_requests_packages_without_tasks(selection) {
        let package_globs = build_globset(selection.packages)?;
        let available_nodes: Vec<_> = task_graph.nodes().collect();
        let matched_package_names =
            collect_matched_package_names(&available_nodes, selection.packages, &package_globs);

        if matched_package_names.is_empty() {
            return Err(miette::miette!(
                "No packages matched: [{}]. -p matches package names, not paths.",
                selection.packages.join(", ")
            ));
        }

        return Ok(available_nodes
            .into_iter()
            .filter(|node| !node.id.is_root())
            .filter(|node| matched_package_names.contains(&node.id.package))
            .map(|node| node.id.clone())
            .collect());
    }

    collect_requested_subgraph(CollectSubgraphRequest {
        task_graph,
        selection,
        pruned,
        since_affected: None,
        expand_dependencies: false,
    })
}

fn selection_uses_default_scope(selection: &TaskSelection<'_>) -> bool {
    selection.requested_tasks.is_empty() && selection.packages.is_empty() && !selection.top_level
}

fn selection_requests_only_top_level(selection: &TaskSelection<'_>) -> bool {
    selection.top_level && selection.requested_tasks.is_empty() && selection.packages.is_empty()
}

fn selection_requests_packages_without_tasks(selection: &TaskSelection<'_>) -> bool {
    selection.requested_tasks.is_empty() && !selection.packages.is_empty() && !selection.top_level
}

/// Print logs for a single task.
///
/// Prints a NOTICE if no cached output exists.
fn print_task_logs(cache: &Cache, task_id: &TaskId, options: &LogsOptions<'_>) {
    let task_id_str = task_id.to_string();
    let Some(record) = cache.read(&task_id_str) else {
        println!("no cached output for {}", task_id_str);
        return;
    };

    if !record_passes_filters(&record, options) {
        return;
    }

    let body = build_log_body(cache, &task_id_str);
    let cache_hash_full = task_cache_key(&task_id_str);
    let meta = build_log_block_meta(task_id, &record, &cache_hash_full);
    print!("{}", format_task_log_block(&meta, &body));
    render_file_sections(&record, options);
}

fn record_passes_filters(record: &luchta_cache::TaskRunRecord, options: &LogsOptions<'_>) -> bool {
    if options.failed && record.succeeded {
        return false;
    }

    if let Some(min_ms) = options.time_taken {
        let duration_ms = record.end_unix_ms.saturating_sub(record.start_unix_ms);
        if duration_ms < min_ms {
            return false;
        }
    }

    true
}

fn build_log_body(cache: &Cache, task_id_str: &str) -> String {
    let stdout = std::fs::read_to_string(cache.stdout_path(task_id_str)).unwrap_or_default();
    let stderr = std::fs::read_to_string(cache.stderr_path(task_id_str)).unwrap_or_default();
    join_output_streams(stdout, stderr)
}

fn join_output_streams(mut stdout: String, stderr: String) -> String {
    if !stderr.is_empty() {
        if !stdout.is_empty() && !stdout.ends_with('\n') {
            stdout.push('\n');
        }
        stdout.push_str(&stderr);
    }
    stdout
}

fn build_log_block_meta<'a>(
    task_id: &'a TaskId,
    record: &'a luchta_cache::TaskRunRecord,
    cache_hash_full: &'a str,
) -> LogBlockMeta<'a> {
    let cache_hash_12 = &cache_hash_full[..12];
    let (package_display, task_display) = package_and_task_display(task_id);
    let duration_ms = record.end_unix_ms.saturating_sub(record.start_unix_ms);

    LogBlockMeta {
        package: package_display,
        task: task_display,
        start: Some(record.start_unix_ms),
        duration_ms: Some(duration_ms),
        exit_status: Some(record.exit_status),
        cache_hash: Some(cache_hash_12),
    }
}

fn render_file_sections(record: &luchta_cache::TaskRunRecord, options: &LogsOptions<'_>) {
    if options.show_inputs {
        print_file_entries("inputs", &record.inputs);
    }
    if options.show_outputs {
        print_file_entries("outputs", &record.outputs);
    }
}

/// Print file entries (inputs or outputs) with metadata.
fn print_file_entries(label: &str, entries: &[FileEntry]) {
    if entries.is_empty() {
        return;
    }
    println!("  {}:", label);
    for entry in entries {
        let mtime_ms = (entry.mtime_ns / 1_000_000) as u64;
        let mtime_display = if entry.absent || entry.mtime_ns == 0 {
            "-".to_string()
        } else {
            format_unix_ms_local(mtime_ms)
        };
        let hash_12 = format_hash_12(&entry.hash);
        let absent_marker = if entry.absent { " (absent)" } else { "" };

        println!(
            "    {} (size={}, mtime={}, hash={}){}",
            entry.path, entry.size, mtime_display, hash_12, absent_marker
        );
    }
}

/// Format a 32-byte hash as 12 hex characters (truncated).
fn format_hash_12(hash: &[u8; 32]) -> String {
    const HEX_CHARS: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(12);
    for byte in hash.iter().take(6) {
        out.push(HEX_CHARS[(byte >> 4) as usize] as char);
        out.push(HEX_CHARS[(byte & 0x0f) as usize] as char);
    }
    out
}
