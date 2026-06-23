use jiff::{fmt::temporal::DateTimePrinter, tz::TimeZone, Timestamp};
use owo_colors::{OwoColorize, Stream};

use crate::reports::{parse_ctrf, parse_sarif, printer_for, ReportKind};

pub struct LogBlockMeta<'a> {
    pub package: &'a str,
    pub task: &'a str,
    pub start: Option<u64>,
    pub duration_ms: Option<u64>,
    pub exit_status: Option<i32>,
    pub cache_hash: Option<&'a str>,
}

const TIMESTAMP_PRINTER: DateTimePrinter = DateTimePrinter::new().precision(Some(0));
const HEADER_MARKER: &str = "╭─";
const FOOTER_MARKER: &str = "╰─";

/// Header (package, task, start time) + body lines verbatim + reports + footer (duration, exit status, cache hash).
pub fn format_task_log_block(
    meta: &LogBlockMeta,
    body: &str,
    reports: &str,
    stream: Stream,
) -> String {
    // Root tasks have package "#" — render as "#task", not "##task"
    let task_label = if meta.package == "#" {
        format!("#{}", meta.task)
    } else {
        format!("{}#{}", meta.package, meta.task)
    };
    let start = meta
        .start
        .map(format_unix_ms_local)
        .unwrap_or_else(|| "-".to_string());
    let duration = meta
        .duration_ms
        .map(format_duration_ms)
        .unwrap_or_else(|| "unknown".to_string());
    let exit = meta
        .exit_status
        .map(|code| code.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let cache = meta.cache_hash.unwrap_or("unknown");

    let mut out = String::new();
    out.push_str(&format!(
        "{} {} · {}\n",
        HEADER_MARKER.if_supports_color(stream, |t| t.blue()),
        task_label.if_supports_color(stream, |t| t.bold()),
        start.if_supports_color(stream, |t| t.dimmed())
    ));
    if !body.is_empty() {
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
    }
    if !reports.is_empty() {
        out.push_str(reports);
        if !reports.ends_with('\n') {
            out.push('\n');
        }
    }
    out.push_str(&format!(
        "{} {} · {} {} · {} {}\n",
        FOOTER_MARKER.if_supports_color(stream, |t| t.blue()),
        duration.if_supports_color(stream, |t| t.dimmed()),
        "exit".if_supports_color(stream, |t| t.dimmed()),
        exit.if_supports_color(stream, |t| t.dimmed()),
        "cache".if_supports_color(stream, |t| t.dimmed()),
        cache.if_supports_color(stream, |t| t.dimmed())
    ));
    out
}

pub struct ReportRenderInput<'a> {
    pub mime_type: &'a str,
    pub bytes: &'a [u8],
}

pub fn render_reports_pretty<'a>(
    reports: impl IntoIterator<Item = ReportRenderInput<'a>>,
    stream: Stream,
) -> String {
    use std::fmt::Write;

    let mut out = String::new();

    for report in reports {
        match printer_for(report.mime_type) {
            Some(ReportKind::Sarif) => match parse_sarif(report.bytes) {
                Ok(sarif) => out.push_str(&format_sarif_pretty(&sarif, stream)),
                Err(error) => {
                    let _ = writeln!(out, "Failed to parse SARIF: {error}");
                }
            },
            Some(ReportKind::Ctrf) => match parse_ctrf(report.bytes) {
                Ok(ctrf) => out.push_str(&format_ctrf_pretty(&ctrf, stream)),
                Err(error) => {
                    let _ = writeln!(out, "Failed to parse CTRF: {error}");
                }
            },
            None => out.push_str(&String::from_utf8_lossy(report.bytes)),
        }

        if !out.ends_with('\n') {
            out.push('\n');
        }
    }

    out
}

pub fn format_unix_ms_local(unix_ms: u64) -> String {
    let millis = i64::try_from(unix_ms).expect("unix ms must fit in i64 for jiff::Timestamp");
    let timestamp =
        Timestamp::from_millisecond(millis).expect("unix ms must be in range for jiff::Timestamp");
    let local_offset = TimeZone::system().to_offset(timestamp);
    TIMESTAMP_PRINTER.timestamp_with_offset_to_string(&timestamp, local_offset)
}

pub fn format_duration_ms(ms: u64) -> String {
    if ms >= 60_000 {
        let minutes = ms / 60_000;
        let seconds = (ms % 60_000) / 1_000;
        return format!("{minutes}m {seconds}s");
    }

    let whole_seconds = ms / 1_000;
    let tenths = (ms % 1_000) / 100;
    format!("{whole_seconds}.{tenths}s")
}

pub(crate) fn package_and_task_display(task_id: &luchta_types::TaskId) -> (&str, &str) {
    if task_id.is_root() {
        ("#", task_id.task.as_str())
    } else {
        (task_id.package.as_str(), task_id.task.as_str())
    }
}

/// Truncate long task output for live failure replay.
///
/// Rules:
/// - If <= 100 lines, show all lines unchanged.
/// - If > 100 lines, show first 30 lines, then a single placeholder line, then
///   last 70 lines. The placeholder is:
///   `… N lines hidden — run `luchta logs ...` for full output`
/// - For package tasks use: `luchta logs -p <pkg> <task>`
/// - For root tasks use: `luchta logs <task>`
///
/// Returns `(shown_lines, truncated)`.
pub fn truncate_output<'a>(
    lines: &'a [&'a str],
    package_display: &str,
    task_display: &str,
) -> (Vec<&'a str>, bool) {
    const MAX_LINES: usize = 100;
    const HEAD_LINES: usize = 30;
    const TAIL_LINES: usize = 70;

    if lines.len() <= MAX_LINES {
        return (lines.to_vec(), false);
    }

    let hidden = lines.len() - HEAD_LINES - TAIL_LINES;
    let command = if package_display == "#" {
        format!("luchta logs {}", task_display)
    } else {
        format!("luchta logs -p {} {}", package_display, task_display)
    };
    let placeholder = format!(
        "… {} lines hidden — run `{}` for full output",
        hidden, command
    );

    let mut shown = Vec::with_capacity(MAX_LINES + 1);
    shown.extend_from_slice(&lines[..HEAD_LINES]);
    let leaked: &'static str = Box::leak(placeholder.into_boxed_str());
    shown.push(leaked);
    shown.extend_from_slice(&lines[lines.len() - TAIL_LINES..]);
    (shown, true)
}

use crate::reports::Ctrf;

struct SarifLocationSummary {
    path: Option<String>,
    start_line: Option<i64>,
    start_column: Option<i64>,
    end_line: Option<i64>,
    end_column: Option<i64>,
    snippet: Option<String>,
}

fn summarize_sarif_location(location: &serde_sarif::sarif::Location) -> SarifLocationSummary {
    let physical_location = location.physical_location.as_ref();
    let artifact_location = physical_location.and_then(|pl| pl.artifact_location.as_ref());
    let region = physical_location.and_then(|pl| pl.region.as_ref());
    let snippet = region
        .and_then(|region| region.snippet.as_ref())
        .and_then(|snippet| snippet.text.clone());

    SarifLocationSummary {
        path: artifact_location.and_then(|al| al.uri.clone()),
        start_line: region.and_then(|region| region.start_line),
        start_column: region.and_then(|region| region.start_column),
        end_line: region.and_then(|region| region.end_line),
        end_column: region.and_then(|region| region.end_column),
        snippet,
    }
}

fn sarif_level_text(level: Option<&serde_sarif::sarif::ResultLevel>) -> &'static str {
    match level {
        Some(serde_sarif::sarif::ResultLevel::Error) => "error",
        Some(serde_sarif::sarif::ResultLevel::Warning) => "warning",
        _ => "note",
    }
}

fn colorize_sarif_level(level_text: &str, stream: Stream) -> String {
    match level_text {
        "error" => level_text
            .if_supports_color(stream, |t| t.red())
            .to_string(),
        "warning" => level_text
            .if_supports_color(stream, |t| t.yellow())
            .to_string(),
        _ => level_text
            .if_supports_color(stream, |t| t.blue())
            .to_string(),
    }
}

fn colorize_sarif_marker(text: &str, level_text: &str, stream: Stream) -> String {
    match level_text {
        "error" => text.if_supports_color(stream, |t| t.red()).to_string(),
        "warning" => text.if_supports_color(stream, |t| t.yellow()).to_string(),
        _ => text.if_supports_color(stream, |t| t.blue()).to_string(),
    }
}

fn format_sarif_location_display(path: &str, line: &str, col: &str, stream: Stream) -> String {
    format!("{path}:{line}:{col}")
        .if_supports_color(stream, |t| t.cyan())
        .to_string()
}

fn format_related_location_suffix(location: &SarifLocationSummary) -> Option<String> {
    let path = location.path.as_deref()?;
    let mut rendered = path.to_string();

    if let Some(line) = location.start_line {
        rendered.push(':');
        rendered.push_str(&line.to_string());
        if let Some(col) = location.start_column {
            rendered.push(':');
            rendered.push_str(&col.to_string());
        }
    }

    Some(rendered)
}

fn format_sarif_snippet_block(
    out: &mut String,
    location: &SarifLocationSummary,
    level_text: &str,
    stream: Stream,
) {
    use std::fmt::Write;

    let Some(snippet) = location.snippet.as_deref() else {
        return;
    };

    let mut snippet_lines = snippet.lines();
    let Some(first_line) = snippet_lines.next() else {
        return;
    };

    let line_number = location
        .start_line
        .map(|line| line.to_string())
        .unwrap_or_default();
    let gutter_width = line_number.len().max(1);
    let pipe = "|".if_supports_color(stream, |t| t.blue()).to_string();

    let _ = writeln!(out, "  {:>gutter_width$} {pipe}", "");
    let _ = writeln!(out, "  {line_number:>gutter_width$} {pipe} {first_line}");

    if let Some(start_column) = location.start_column {
        let caret_offset = usize::try_from(start_column.saturating_sub(1)).unwrap_or_default();
        let single_line_width = match location.end_column {
            Some(end_column) if end_column >= start_column => {
                usize::try_from((end_column - start_column + 1).max(1)).unwrap_or(1)
            }
            _ => 1,
        };
        let is_multiline = matches!(
            (location.start_line, location.end_line),
            (Some(start_line), Some(end_line)) if end_line > start_line
        );
        let multiline_width = first_line
            .chars()
            .count()
            .saturating_sub(caret_offset)
            .max(1);
        let caret_width = if is_multiline {
            multiline_width
        } else {
            single_line_width
        };
        let spaces = " ".repeat(caret_offset);
        let carets = "^".repeat(caret_width);
        let marker = colorize_sarif_marker(&format!("{spaces}{carets}"), level_text, stream);
        let _ = writeln!(out, "  {:>gutter_width$} {pipe} {marker}", "");
    }

    for extra_line in snippet_lines {
        let _ = writeln!(out, "  {:>gutter_width$} {pipe} {extra_line}", "");
    }
}

pub fn format_sarif_pretty(sarif: &serde_sarif::sarif::Sarif, stream: Stream) -> String {
    use std::fmt::Write;
    let mut out = String::new();

    for run in &sarif.runs {
        if let Some(results) = &run.results {
            for result in results {
                let level = result.level.as_ref();
                let level_text = sarif_level_text(level);
                let level_colored = colorize_sarif_level(level_text, stream);
                let message = result.message.text.as_deref().unwrap_or("No message");

                let primary_location = result
                    .locations
                    .as_ref()
                    .and_then(|locations| locations.first())
                    .map(summarize_sarif_location);

                let path = primary_location
                    .as_ref()
                    .and_then(|location| location.path.as_deref())
                    .unwrap_or("-");
                let line = primary_location
                    .as_ref()
                    .and_then(|location| location.start_line)
                    .unwrap_or_default()
                    .to_string();
                let col = primary_location
                    .as_ref()
                    .and_then(|location| location.start_column)
                    .unwrap_or_default()
                    .to_string();

                let location_clickable = format_sarif_location_display(path, &line, &col, stream);
                let rule_suffix = result
                    .rule_id
                    .as_deref()
                    .map(|rule_id| format!(" [{rule_id}]"))
                    .unwrap_or_default();
                let _ = writeln!(
                    out,
                    "{}: {}: {}{}",
                    location_clickable, level_colored, message, rule_suffix
                );

                if let Some(location) = &primary_location {
                    format_sarif_snippet_block(&mut out, location, level_text, stream);
                }

                if let Some(fixes) = &result.fixes {
                    for fix in fixes {
                        if let Some(text) = fix
                            .description
                            .as_ref()
                            .and_then(|description| description.text.as_deref())
                        {
                            let help = "help".if_supports_color(stream, |t| t.cyan()).to_string();
                            let _ = writeln!(out, "    = {help}: {text}");
                        }
                    }
                }

                if let Some(related_locations) = &result.related_locations {
                    for related in related_locations {
                        let summary = summarize_sarif_location(related);
                        let message = related
                            .message
                            .as_ref()
                            .and_then(|message| message.text.as_deref());
                        let location_suffix = format_related_location_suffix(&summary);

                        if message.is_none() && location_suffix.is_none() {
                            continue;
                        }

                        let note = "note".if_supports_color(stream, |t| t.blue()).to_string();
                        match (message, location_suffix) {
                            (Some(message), Some(location_suffix)) => {
                                let _ =
                                    writeln!(out, "    = {note}: {message} ({location_suffix})");
                            }
                            (Some(message), None) => {
                                let _ = writeln!(out, "    = {note}: {message}");
                            }
                            (None, Some(location_suffix)) => {
                                let _ = writeln!(out, "    = {note}: {location_suffix}");
                            }
                            (None, None) => {}
                        }
                    }
                }
            }
        }
    }

    out
}

pub fn format_ctrf_pretty(ctrf: &Ctrf, stream: Stream) -> String {
    use std::fmt::Write;
    let mut out = String::new();

    let summary = &ctrf.results.summary;
    let failed_str = if summary.failed > 0 {
        format!(
            "{} failed",
            summary.failed.if_supports_color(stream, |t| t.red())
        )
    } else {
        format!("{} failed", summary.failed)
    };

    let passed_str = format!(
        "{} passed",
        summary.passed.if_supports_color(stream, |t| t.green())
    );
    let skipped_str = format!("{} skipped", summary.skipped);

    let _ = writeln!(out, "{}, {}, {}", passed_str, failed_str, skipped_str);

    for test in &ctrf.results.tests {
        if test.status == "failed" {
            let msg = test.message.as_deref().unwrap_or("No message");
            let _ = writeln!(
                out,
                "  {} {}",
                "✗".if_supports_color(stream, |t| t.red()),
                test.name.if_supports_color(stream, |t| t.red())
            );
            let _ = writeln!(out, "    {}", msg);
            if let Some(trace) = &test.trace {
                let _ = writeln!(out, "    Trace: {}", trace);
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered_lines(count: usize) -> Vec<String> {
        (1..=count).map(|n| format!("line {n}")).collect()
    }

    struct TruncationExpectation<'a> {
        line_count: usize,
        package: &'a str,
        task: &'a str,
        truncated: bool,
        expected_len: usize,
        first_line: Option<&'a str>,
        last_line: Option<&'a str>,
        checks: &'a [(usize, &'a str)],
        absent_substrings: &'a [&'a str],
    }

    fn assert_truncation(expectation: TruncationExpectation<'_>) {
        let owned = numbered_lines(expectation.line_count);
        let input: Vec<&str> = owned.iter().map(String::as_str).collect();
        let (shown, truncated) = truncate_output(&input, expectation.package, expectation.task);

        assert_eq!(truncated, expectation.truncated);
        assert_eq!(shown.len(), expectation.expected_len);

        if let Some(first_line) = expectation.first_line {
            assert_eq!(shown.first().copied(), Some(first_line));
        }
        if let Some(last_line) = expectation.last_line {
            assert_eq!(shown.last().copied(), Some(last_line));
        }
        for (index, expected) in expectation.checks {
            assert_eq!(shown.get(*index).copied(), Some(*expected));
        }
        for forbidden in expectation.absent_substrings {
            assert!(
                !shown.iter().any(|line| line.contains(forbidden)),
                "unexpected substring {forbidden:?} in {:?}",
                shown
            );
        }
    }

    #[test]
    fn format_duration_ms_seconds_and_subseconds() {
        assert_eq!(format_duration_ms(0), "0.0s");
        assert_eq!(format_duration_ms(999), "0.9s");
        assert_eq!(format_duration_ms(1_234), "1.2s");
        assert_eq!(format_duration_ms(59_999), "59.9s");
    }

    #[test]
    fn format_duration_ms_minutes() {
        assert_eq!(format_duration_ms(60_000), "1m 0s");
        assert_eq!(format_duration_ms(61_000), "1m 1s");
        assert_eq!(format_duration_ms(125_000), "2m 5s");
    }

    #[test]
    fn truncate_output_keeps_short_output_unchanged() {
        assert_truncation(TruncationExpectation {
            line_count: 100,
            package: "pkg",
            task: "build",
            truncated: false,
            expected_len: 100,
            first_line: Some("line 1"),
            last_line: Some("line 100"),
            checks: &[(29, "line 30"), (99, "line 100")],
            absent_substrings: &["lines hidden", "luchta logs"],
        });
    }

    #[test]
    fn truncate_output_replaces_middle_for_package_task() {
        assert_truncation(TruncationExpectation {
            line_count: 101,
            package: "pkg",
            task: "build",
            truncated: true,
            expected_len: 101,
            first_line: Some("line 1"),
            last_line: Some("line 101"),
            checks: &[
                (29, "line 30"),
                (
                    30,
                    "… 1 lines hidden — run `luchta logs -p pkg build` for full output",
                ),
                (31, "line 32"),
                (100, "line 101"),
            ],
            absent_substrings: &["line 31"],
        });
    }

    #[test]
    fn truncate_output_replaces_middle_for_root_task() {
        assert_truncation(TruncationExpectation {
            line_count: 150,
            package: "#",
            task: "lint",
            truncated: true,
            expected_len: 101,
            first_line: Some("line 1"),
            last_line: Some("line 150"),
            checks: &[
                (29, "line 30"),
                (
                    30,
                    "… 50 lines hidden — run `luchta logs lint` for full output",
                ),
                (31, "line 81"),
                (100, "line 150"),
            ],
            absent_substrings: &["line 31", "line 80"],
        });
    }

    #[test]
    fn package_and_task_display_root_task_uses_hash_package() {
        let task_id = luchta_types::TaskId::new(luchta_types::ROOT_PACKAGE_NAME, "build");
        assert_eq!(package_and_task_display(&task_id), ("#", "build"));
    }

    #[test]
    fn package_and_task_display_package_task_uses_package_name() {
        let task_id = luchta_types::TaskId::new("app", "test");
        assert_eq!(package_and_task_display(&task_id), ("app", "test"));
    }

    #[test]
    fn format_task_log_block_root_task_label_single_hash() {
        // Root task: package is "#" → label should be "#build", not "##build"
        let meta = LogBlockMeta {
            package: "#",
            task: "build",
            start: None,
            duration_ms: None,
            exit_status: None,
            cache_hash: None,
        };
        let output = format_task_log_block(&meta, "body", "", Stream::Stdout);
        assert!(
            output.contains("#build"),
            "expected label '#build' for root task, got: {output}"
        );
        assert!(
            !output.contains("##build"),
            "label should not contain '##build' for root task, got: {output}"
        );
    }

    #[test]
    fn format_task_log_block_non_root_task_label_package_hash() {
        // Non-root task: package is "app" → label should be "app#build"
        let meta = LogBlockMeta {
            package: "app",
            task: "build",
            start: None,
            duration_ms: None,
            exit_status: None,
            cache_hash: None,
        };
        let output = format_task_log_block(&meta, "body", "", Stream::Stdout);
        assert!(
            output.contains("app#build"),
            "expected label 'app#build' for non-root task, got: {output}"
        );
    }

    #[test]
    fn format_sarif_pretty_test() {
        let sarif_json = r#"{
            "version": "2.1.0",
            "runs": [{
                "tool": { "driver": { "name": "test" } },
                "results": [{
                    "ruleId": "E123",
                    "level": "error",
                    "message": { "text": "Something is wrong" },
                    "locations": [{
                        "physicalLocation": {
                            "artifactLocation": { "uri": "src/main.rs" },
                            "region": { "startLine": 10, "startColumn": 5 }
                        }
                    }]
                }]
            }]
        }"#;
        let sarif: serde_sarif::sarif::Sarif = serde_json::from_str(sarif_json).unwrap();
        let formatted = format_sarif_pretty(&sarif, Stream::Stdout);
        assert_eq!(
            formatted,
            "src/main.rs:10:5: error: Something is wrong [E123]\n"
        );
        assert!(!formatted.contains(" --> "));
    }

    #[test]
    fn format_sarif_pretty_renders_snippet_and_carets_without_ansi_for_captured_stream() {
        let sarif_json = r#"{
            "version": "2.1.0",
            "runs": [{
                "tool": { "driver": { "name": "test" } },
                "results": [{
                    "level": "error",
                    "message": { "text": "Cannot find name 'stringss'" },
                    "locations": [{
                        "physicalLocation": {
                            "artifactLocation": { "uri": "src/foo.rs" },
                            "region": {
                                "startLine": 15,
                                "startColumn": 12,
                                "endColumn": 19,
                                "snippet": { "text": "  const x: stringss = value;" }
                            }
                        }
                    }]
                }]
            }]
        }"#;
        let sarif: serde_sarif::sarif::Sarif = serde_json::from_str(sarif_json).unwrap();

        let formatted = format_sarif_pretty(&sarif, Stream::Stdout);

        assert!(formatted.contains(" 15 |   const x: stringss = value;"));
        assert!(formatted.contains("|            ^^^^^^^^"));
        assert!(!formatted.contains('\u{1b}'));
    }

    #[test]
    fn format_sarif_pretty_renders_fix_descriptions_as_help() {
        let sarif_json = r#"{
            "version": "2.1.0",
            "runs": [{
                "tool": { "driver": { "name": "test" } },
                "results": [{
                    "level": "warning",
                    "message": { "text": "Misspelled identifier" },
                    "fixes": [{
                        "artifactChanges": [],
                        "description": { "text": "replace `stringss` with `string`" }
                    }]
                }]
            }]
        }"#;
        let sarif: serde_sarif::sarif::Sarif = serde_json::from_str(sarif_json).unwrap();

        let formatted = format_sarif_pretty(&sarif, Stream::Stdout);

        assert!(formatted.contains("help:"));
        assert!(formatted.contains("replace `stringss` with `string`"));
    }

    #[test]
    fn format_sarif_pretty_renders_related_locations_as_notes() {
        let sarif_json = r#"{
            "version": "2.1.0",
            "runs": [{
                "tool": { "driver": { "name": "test" } },
                "results": [{
                    "level": "warning",
                    "message": { "text": "See definition" },
                    "relatedLocations": [{
                        "id": 1,
                        "message": { "text": "defined here" },
                        "physicalLocation": {
                            "artifactLocation": { "uri": "src/types.rs" },
                            "region": { "startLine": 3, "startColumn": 1 }
                        }
                    }]
                }]
            }]
        }"#;
        let sarif: serde_sarif::sarif::Sarif = serde_json::from_str(sarif_json).unwrap();

        let formatted = format_sarif_pretty(&sarif, Stream::Stdout);

        assert!(formatted.contains("note:"));
        assert!(formatted.contains("defined here (src/types.rs:3:1)"));
    }

    #[test]
    fn format_sarif_snippet_block_places_multiline_caret_under_first_line() {
        let sarif_json = r#"{
            "version": "2.1.0",
            "runs": [{
                "tool": { "driver": { "name": "test" } },
                "results": [{
                    "level": "error",
                    "message": { "text": "Broken call" },
                    "locations": [{
                        "physicalLocation": {
                            "artifactLocation": { "uri": "src/lib.rs" },
                            "region": {
                                "startLine": 12,
                                "startColumn": 5,
                                "endLine": 13,
                                "endColumn": 3,
                                "snippet": { "text": "call(foo,\nbar);" }
                            }
                        }
                    }]
                }]
            }]
        }"#;
        let sarif: serde_sarif::sarif::Sarif = serde_json::from_str(sarif_json).unwrap();

        let formatted = format_sarif_pretty(&sarif, Stream::Stdout);
        let expected = "  12 | call(foo,
     |     ^^^^^
     | bar);";

        assert!(
            formatted.contains(expected),
            "unexpected snippet block: {formatted}"
        );
    }

    #[test]
    fn format_sarif_snippet_block_aligns_gutter_pipes_for_multi_digit_line() {
        let sarif_json = r#"{
            "version": "2.1.0",
            "runs": [{
                "tool": { "driver": { "name": "test" } },
                "results": [{
                    "level": "error",
                    "message": { "text": "bad" },
                    "locations": [{
                        "physicalLocation": {
                            "artifactLocation": { "uri": "a.ts" },
                            "region": {
                                "startLine": 150,
                                "startColumn": 5,
                                "endColumn": 9,
                                "snippet": { "text": "    let xyz = 1;" }
                            }
                        }
                    }]
                }]
            }]
        }"#;
        let sarif: serde_sarif::sarif::Sarif = serde_json::from_str(sarif_json).unwrap();

        let formatted = format_sarif_pretty(&sarif, Stream::Stdout);
        // The empty-gutter, numbered, and caret lines must all place the pipe
        // at the same column even when the line number has multiple digits.
        let expected = "      |
  150 |     let xyz = 1;
      |     ^^^^^";

        assert!(
            formatted.contains(expected),
            "gutter pipes must align for multi-digit line numbers: {formatted}"
        );
    }

    #[test]
    fn format_task_log_block_places_reports_before_footer() {
        let meta = LogBlockMeta {
            package: "app",
            task: "build",
            start: None,
            duration_ms: None,
            exit_status: None,
            cache_hash: None,
        };

        let output = format_task_log_block(&meta, "body line", "report line\n", Stream::Stdout);
        let report_index = output.find("report line").unwrap();
        let footer_index = output.find("╰─").unwrap();

        assert!(
            report_index < footer_index,
            "reports must be inside block: {output}"
        );
        assert!(output.contains("body line\nreport line"));
    }

    #[test]
    fn render_reports_pretty_formats_sarif_and_raw_reports() {
        let sarif = br#"{
            "version": "2.1.0",
            "runs": [{
                "tool": { "driver": { "name": "test" } },
                "results": [{
                    "level": "warning",
                    "message": { "text": "Clickable warning" },
                    "locations": [{
                        "physicalLocation": {
                            "artifactLocation": { "uri": "src/lib.rs" },
                            "region": { "startLine": 3, "startColumn": 9 }
                        }
                    }]
                }]
            }]
        }"#;
        let rendered = render_reports_pretty(
            [
                ReportRenderInput {
                    mime_type: "application/sarif+json",
                    bytes: sarif,
                },
                ReportRenderInput {
                    mime_type: "text/plain",
                    bytes: b"raw payload",
                },
            ],
            Stream::Stdout,
        );

        assert!(rendered.contains("src/lib.rs:3:9: warning: Clickable warning"));
        assert!(rendered.contains("raw payload"));
        assert!(!rendered.contains("report.sarif"));
        assert!(!rendered.contains("application/sarif+json"));
        assert!(!rendered.contains("report.txt"));
    }

    #[test]
    fn format_sarif_pretty_handles_missing_message_location_fields() {
        let sarif_json = r#"{
            "version": "2.1.0",
            "runs": [{
                "tool": { "driver": { "name": "test" } },
                "results": [{
                    "level": "warning",
                    "message": {},
                    "locations": [{
                        "physicalLocation": {
                            "artifactLocation": {},
                            "region": {}
                        }
                    }]
                }]
            }]
        }"#;
        let sarif: serde_sarif::sarif::Sarif = serde_json::from_str(sarif_json).unwrap();

        let formatted = format_sarif_pretty(&sarif, Stream::Stdout);

        assert!(formatted.contains("-:0:0: warning: No message"));
    }

    #[test]
    fn render_reports_pretty_falls_back_to_lossy_utf8_for_unknown_reports() {
        let rendered = render_reports_pretty(
            [ReportRenderInput {
                mime_type: "application/x-unknown",
                bytes: b"prefix\xFFsuffix",
            }],
            Stream::Stdout,
        );

        assert!(!rendered.contains("raw.bin"));
        assert!(rendered.contains("prefix�suffix"));
    }

    #[test]
    fn format_task_log_block_does_not_emit_ansi_for_captured_stream() {
        let meta = LogBlockMeta {
            package: "app",
            task: "build",
            start: Some(0),
            duration_ms: Some(1_234),
            exit_status: Some(1),
            cache_hash: Some("deadbeefcafe"),
        };
        let reports = render_reports_pretty(
            [ReportRenderInput {
                mime_type: "application/sarif+json",
                bytes: br#"{
                    "version": "2.1.0",
                    "runs": [{
                        "tool": { "driver": { "name": "test" } },
                        "results": [{
                            "level": "warning",
                            "message": { "text": "No ANSI" },
                            "locations": [{
                                "physicalLocation": {
                                    "artifactLocation": { "uri": "src/lib.rs" },
                                    "region": { "startLine": 3, "startColumn": 9 }
                                }
                            }]
                        }]
                    }]
                }"#,
            }],
            Stream::Stdout,
        );

        let output = format_task_log_block(&meta, "body", &reports, Stream::Stdout);

        assert!(
            !output.contains('\u{1b}'),
            "unexpected ANSI escape in: {output:?}"
        );
    }

    #[test]
    fn format_ctrf_pretty_test() {
        let ctrf_json = r#"{
            "results": {
                "tool": { "name": "jest" },
                "summary": { "tests": 2, "passed": 1, "failed": 1, "pending": 0, "skipped": 0, "start": 0, "stop": 0 },
                "tests": [
                    { "name": "test 1", "status": "passed", "duration": 10 },
                    { "name": "test 2", "status": "failed", "message": "Expected 1 to be 2", "trace": "Error at line 1" }
                ]
            }
        }"#;
        use crate::reports::parse_ctrf;
        let ctrf = parse_ctrf(ctrf_json.as_bytes()).unwrap();
        let formatted = format_ctrf_pretty(&ctrf, Stream::Stdout);
        assert!(formatted.contains("passed"));
        assert!(formatted.contains("failed"));
        assert!(formatted.contains("test 2"));
        assert!(formatted.contains("Expected 1 to be 2"));
        assert!(formatted.contains("Trace: Error at line 1"));
    }
}
