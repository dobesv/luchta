//! Logs command implementation.
//!
//! Reads cached logs and metadata for configured tasks from the task graph,
//! printing each task's output as a wrapped header/body/footer block.

use luchta_cache::{resolve_cache_dir, task_cache_key, Cache, FileEntry};
use luchta_types::TaskId;
use miette::Result;
use owo_colors::Stream;

use crate::format::{
    format_task_log_block, format_unix_ms_local, package_and_task_display, render_reports_pretty,
    LogBlockMeta, ReportRenderInput,
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
    pub show_cache_nonce: bool,
    pub files: &'a [String],
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

    if options.files.is_empty() {
        for task_id in sorted_ids {
            print_task_logs(&cache, &task_id, options);
        }
    } else {
        let mut records = Vec::new();
        for task_id in &sorted_ids {
            let task_id_str = task_id.to_string();
            if let Some(record) = cache.read(&task_id_str) {
                if record_passes_filters(&record, options) {
                    records.push((task_id.clone(), record));
                }
            }
        }

        let matched =
            tasks_with_requested_files(records.iter().map(|(id, r)| (id, r)), options.files)?;

        for (task_id, matched_files) in matched {
            let task_id_str = task_id.to_string();
            for file_name in matched_files {
                if let Some(bytes) = cache.read_report(&task_id_str, file_name) {
                    use std::io::Write;
                    let _ = std::io::stdout().write_all(&bytes);
                }
            }
        }
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
        if !options.failed {
            println!("no cached output for {}", task_id_str);
        }
        return;
    };

    if !record_passes_filters(&record, options) {
        return;
    }

    let body = build_log_body(cache, &task_id_str);
    let report_bytes: Vec<_> = record
        .reports
        .iter()
        .filter_map(|report| {
            cache
                .read_report(&task_id_str, &report.filename)
                .map(|bytes| (report, bytes))
        })
        .collect();
    let reports = render_reports_pretty(
        report_bytes
            .iter()
            .map(|(report, bytes)| ReportRenderInput {
                mime_type: &report.mime_type,
                bytes,
            }),
        Stream::Stdout,
    );
    let cache_hash_full = task_cache_key(&task_id_str);
    let meta = build_log_block_meta(task_id, &record, &cache_hash_full, options.show_cache_nonce);
    print!(
        "{}",
        format_task_log_block(&meta, &body, &reports, Stream::Stdout)
    );

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
    let stdout = std::fs::read(cache.stdout_path(task_id_str))
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let stderr = std::fs::read(cache.stderr_path(task_id_str))
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
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
    show_cache_nonce: bool,
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
        show_cache_nonce,
        cache_nonce: record.cache_nonce.as_deref(),
        run_reason: record.run_reason.as_ref().map(|r| r.summary()),
    }
}

fn render_file_sections(record: &luchta_cache::TaskRunRecord, options: &LogsOptions<'_>) {
    if options.show_inputs {
        print_patterns(
            "input patterns",
            &record.input_patterns,
            record.detected_input_patterns,
        );
        print_file_entries("inputs", &record.inputs);
    }
    if options.show_outputs {
        print_patterns(
            "output patterns",
            &record.output_patterns,
            record.detected_output_patterns,
        );
        print_file_entries("outputs", &record.outputs);
    }
}

/// Print the effective input/output patterns (globs) stored in the cache
/// metadata, noting whether they were worker-detected or declared.
fn print_patterns(label: &str, patterns: &[String], detected: bool) {
    print!("{}", format_patterns_section(label, patterns, detected));
}

/// Render the patterns section as a string (one line per output line,
/// trailing newline included). Split out from `print_patterns` so the
/// formatting — including the `(detected)`/`(declared)` marker and the
/// empty `(none)` case — is unit-testable without capturing stdout.
fn format_patterns_section(label: &str, patterns: &[String], detected: bool) -> String {
    let source = if detected { "detected" } else { "declared" };
    if patterns.is_empty() {
        return format!("  {label} ({source}): (none)\n");
    }
    let mut out = format!("  {label} ({source}):\n");
    for pattern in patterns {
        out.push_str(&format!("    {pattern}\n"));
    }
    out
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

pub(crate) fn tasks_with_requested_files<'a>(
    records_with_ids: impl Iterator<Item = (&'a TaskId, &'a luchta_cache::TaskRunRecord)>,
    requested_files: &'a [String],
) -> Result<Vec<(&'a TaskId, Vec<&'a String>)>> {
    let mut matched = Vec::new();
    let mut file_to_tasks: std::collections::BTreeMap<&String, Vec<&TaskId>> =
        std::collections::BTreeMap::new();

    let records: Vec<_> = records_with_ids.collect();

    for (task_id, record) in &records {
        let mut matched_files = Vec::new();
        for file_name in requested_files {
            if record.reports.iter().any(|r| r.filename == **file_name) {
                matched_files.push(file_name);
                file_to_tasks.entry(file_name).or_default().push(task_id);
            }
        }
        if !matched_files.is_empty() {
            matched.push((*task_id, matched_files));
        }
    }

    for (file_name, tasks) in file_to_tasks {
        if tasks.len() > 1 {
            let mut task_names: Vec<String> = tasks.iter().map(|id| id.to_string()).collect();
            task_names.sort();
            miette::bail!(
                "Requested file '{}' is ambiguous: it was found on multiple tasks ({}). Please narrow your selection (e.g. by task name or -p <package>).",
                file_name,
                task_names.join(", ")
            );
        }
    }

    if matched.is_empty() && !requested_files.is_empty() {
        miette::bail!("No task matching the selection contained any of the requested files.");
    }

    Ok(matched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use luchta_cache::{ReportMeta, TaskRunRecord};
    use luchta_types::TaskId;
    use std::collections::BTreeMap;

    #[test]
    fn format_patterns_section_marks_declared() {
        let patterns = vec!["src/**/*.ts".to_string(), "package.json".to_string()];
        let out = format_patterns_section("input patterns", &patterns, false);
        assert_eq!(
            out,
            "  input patterns (declared):\n    src/**/*.ts\n    package.json\n"
        );
    }

    #[test]
    fn format_patterns_section_marks_detected() {
        let patterns = vec!["dist/**".to_string()];
        let out = format_patterns_section("output patterns", &patterns, true);
        assert_eq!(out, "  output patterns (detected):\n    dist/**\n");
    }

    #[test]
    fn format_patterns_section_empty_renders_none() {
        let out = format_patterns_section("input patterns", &[], false);
        assert_eq!(out, "  input patterns (declared): (none)\n");

        let out_detected = format_patterns_section("output patterns", &[], true);
        assert_eq!(out_detected, "  output patterns (detected): (none)\n");
    }

    fn dummy_record(reports: Vec<&str>) -> TaskRunRecord {
        TaskRunRecord {
            schema_version: luchta_cache::SCHEMA_VERSION_V4,
            task_spec_hash: [0; 32],
            input_patterns: vec![],
            inputs: vec![],
            output_patterns: vec![],
            outputs: vec![],
            detected_input_patterns: false,
            detected_output_patterns: false,
            outputs_hash: [0; 32],
            env_hash: [0; 32],
            pkg_dep_hash: [0; 32],
            dep_outputs: BTreeMap::new(),
            exit_status: 0,
            succeeded: true,
            start_unix_ms: 0,
            end_unix_ms: 0,
            reports: reports
                .into_iter()
                .map(|f| ReportMeta {
                    filename: f.to_string(),
                    mime_type: "text/plain".to_string(),
                })
                .collect(),
            cache_nonce: None,
            run_reason: None,
        }
    }

    #[test]
    fn test_tasks_with_requested_files_union() {
        let task1 = TaskId::new("pkg1", "build");
        let task2 = TaskId::new("pkg2", "test");

        let rec1 = dummy_record(vec!["report.json", "coverage.xml"]);
        let rec2 = dummy_record(vec!["coverage.xml", "lint.json"]);

        let records = vec![(&task1, &rec1), (&task2, &rec2)];

        let requested = vec!["report.json".to_string(), "lint.json".to_string()];

        let matched = tasks_with_requested_files(records.into_iter(), &requested).unwrap();

        assert_eq!(matched.len(), 2);
        assert_eq!(matched[0].0, &task1);
        assert_eq!(matched[0].1, vec![&"report.json".to_string()]);

        assert_eq!(matched[1].0, &task2);
        assert_eq!(matched[1].1, vec![&"lint.json".to_string()]);
    }

    #[test]
    fn test_tasks_with_requested_files_no_match() {
        let task1 = TaskId::new("pkg1", "build");
        let rec1 = dummy_record(vec!["report.json"]);

        let records = vec![(&task1, &rec1)];
        let requested = vec!["missing.json".to_string()];

        let err = tasks_with_requested_files(records.into_iter(), &requested).unwrap_err();
        assert_eq!(
            err.to_string(),
            "No task matching the selection contained any of the requested files."
        );
    }

    #[test]
    fn format_task_log_block_keeps_reports_before_footer() {
        let meta = LogBlockMeta {
            package: "pkg",
            task: "lint",
            start: None,
            duration_ms: None,
            exit_status: Some(1),
            cache_hash: Some("abcdef123456"),
            show_cache_nonce: false,
            cache_nonce: None,
            run_reason: None,
        };
        let body = join_output_streams("stdout line".to_string(), "stderr line".to_string());
        let reports = render_reports_pretty(
            [ReportRenderInput {
                mime_type: "text/plain",
                bytes: b"report body",
            }],
            Stream::Stdout,
        );
        let block = format_task_log_block(&meta, &body, &reports, Stream::Stdout);
        let report_index = block.find("report body").unwrap();
        let footer_index = block.find("╰─").unwrap();

        assert!(block.contains("stdout line\nstderr line"));
        assert!(
            report_index < footer_index,
            "report must render before footer: {block}"
        );
    }
}
