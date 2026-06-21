use jiff::{fmt::temporal::DateTimePrinter, tz::TimeZone, Timestamp};
use owo_colors::{OwoColorize, Stream};

pub struct LogBlockMeta<'a> {
    pub package: &'a str,
    pub task: &'a str,
    pub start: Option<u64>,
    pub duration_ms: Option<u64>,
    pub exit_status: Option<i32>,
    pub cache_hash: Option<&'a str>,
}

const TIMESTAMP_PRINTER: DateTimePrinter = DateTimePrinter::new().precision(Some(0));
const HEADER_MARKER: &str = "──▶";
const FOOTER_MARKER: &str = "──◀";

/// Header (package, task, start time) + body lines verbatim + footer (duration, exit status, cache hash).
pub fn format_task_log_block(meta: &LogBlockMeta, body: &str) -> String {
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
        "{} {} start={}\n",
        HEADER_MARKER.blue(),
        task_label.bold(),
        start.dimmed()
    ));
    if !body.is_empty() {
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
    }
    out.push_str(&format!(
        "{} duration={} exit={} cache={}\n",
        FOOTER_MARKER.blue(),
        duration.dimmed(),
        exit.dimmed(),
        cache.dimmed()
    ));
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

pub fn format_sarif_pretty(sarif: &serde_sarif::sarif::Sarif) -> String {
    use std::fmt::Write;
    let mut out = String::new();

    for run in &sarif.runs {
        if let Some(results) = &run.results {
            for result in results {
                let level_str = match &result.level {
                    Some(level) => format!("{}", level),
                    None => "warning".to_string(),
                };

                let level_colored = match level_str.as_str() {
                    "error" => format!(
                        "[{}]",
                        "error".if_supports_color(Stream::Stdout, |t| t.red())
                    ),
                    "warning" => format!(
                        "[{}]",
                        "warning".if_supports_color(Stream::Stdout, |t| t.yellow())
                    ),
                    "note" => format!(
                        "[{}]",
                        "note".if_supports_color(Stream::Stdout, |t| t.green())
                    ),
                    other => format!("[{}]", other),
                };

                let message = result.message.text.as_deref().unwrap_or("No message");

                let mut path = "-".to_string();
                let mut line = "0".to_string();
                let mut col = "0".to_string();

                if let Some(locations) = &result.locations {
                    if let Some(loc) = locations.first() {
                        if let Some(pl) = &loc.physical_location {
                            if let Some(al) = &pl.artifact_location {
                                if let Some(uri) = &al.uri {
                                    path = uri.clone();
                                }
                            }
                            if let Some(reg) = &pl.region {
                                if let Some(sl) = reg.start_line {
                                    line = sl.to_string();
                                }
                                if let Some(sc) = reg.start_column {
                                    col = sc.to_string();
                                }
                            }
                        }
                    }
                }

                let location_clickable = format!("{}:{}:{}", path, line, col)
                    .if_supports_color(Stream::Stdout, |t| t.cyan())
                    .to_string();
                let _ = writeln!(
                    out,
                    "{} {} --> {}",
                    level_colored, message, location_clickable
                );
            }
        }
    }

    out
}

pub fn format_ctrf_pretty(ctrf: &Ctrf) -> String {
    use std::fmt::Write;
    let mut out = String::new();

    let summary = &ctrf.results.summary;
    let failed_str = if summary.failed > 0 {
        format!(
            "{} failed",
            summary
                .failed
                .if_supports_color(Stream::Stdout, |t| t.red())
        )
    } else {
        format!("{} failed", summary.failed)
    };

    let passed_str = format!(
        "{} passed",
        summary
            .passed
            .if_supports_color(Stream::Stdout, |t| t.green())
    );
    let skipped_str = format!("{} skipped", summary.skipped);

    let _ = writeln!(out, "{}, {}, {}", passed_str, failed_str, skipped_str);

    for test in &ctrf.results.tests {
        if test.status == "failed" {
            let msg = test.message.as_deref().unwrap_or("No message");
            let _ = writeln!(
                out,
                "  {} {}",
                "✗".if_supports_color(Stream::Stdout, |t| t.red()),
                test.name.if_supports_color(Stream::Stdout, |t| t.red())
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
            assert_eq!(shown[*index], *expected);
        }
        for needle in expectation.absent_substrings {
            assert!(shown.iter().all(|line| !line.contains(needle)));
        }
    }

    #[test]
    fn truncate_output_keeps_zero_lines() {
        assert_truncation(TruncationExpectation {
            line_count: 0,
            package: "app",
            task: "build",
            truncated: false,
            expected_len: 0,
            first_line: None,
            last_line: None,
            checks: &[],
            absent_substrings: &[],
        });
    }

    #[test]
    fn truncate_output_keeps_expected_non_truncated_shapes() {
        let cases = [
            TruncationExpectation {
                line_count: 30,
                package: "app",
                task: "build",
                truncated: false,
                expected_len: 30,
                first_line: Some("line 1"),
                last_line: Some("line 30"),
                checks: &[],
                absent_substrings: &[],
            },
            TruncationExpectation {
                line_count: 100,
                package: "#",
                task: "test",
                truncated: false,
                expected_len: 100,
                first_line: None,
                last_line: None,
                checks: &[(29, "line 30"), (99, "line 100")],
                absent_substrings: &["lines hidden"],
            },
        ];

        for case in cases {
            assert_truncation(case);
        }
    }

    #[test]
    fn truncate_output_keeps_expected_truncated_shapes() {
        let cases = [
            TruncationExpectation {
                line_count: 101,
                package: "app",
                task: "build",
                truncated: true,
                expected_len: 101,
                first_line: Some("line 1"),
                last_line: Some("line 101"),
                checks: &[
                    (29, "line 30"),
                    (
                        30,
                        "… 1 lines hidden — run `luchta logs -p app build` for full output",
                    ),
                    (31, "line 32"),
                ],
                absent_substrings: &[],
            },
            TruncationExpectation {
                line_count: 150,
                package: "#",
                task: "test",
                truncated: true,
                expected_len: 101,
                first_line: None,
                last_line: Some("line 150"),
                checks: &[
                    (29, "line 30"),
                    (
                        30,
                        "… 50 lines hidden — run `luchta logs test` for full output",
                    ),
                    (31, "line 81"),
                ],
                absent_substrings: &[],
            },
            TruncationExpectation {
                line_count: 200,
                package: "pkg",
                task: "lint",
                truncated: true,
                expected_len: 101,
                first_line: None,
                last_line: Some("line 200"),
                checks: &[
                    (29, "line 30"),
                    (
                        30,
                        "… 100 lines hidden — run `luchta logs -p pkg lint` for full output",
                    ),
                    (31, "line 131"),
                ],
                absent_substrings: &[],
            },
        ];

        for case in cases {
            assert_truncation(case);
        }
    }

    #[test]
    fn format_task_log_block_root_task_label_single_hash() {
        // Root task: package is "#" → label should be "#build", NOT "##build"
        let meta = LogBlockMeta {
            package: "#",
            task: "build",
            start: None,
            duration_ms: None,
            exit_status: None,
            cache_hash: None,
        };
        let output = format_task_log_block(&meta, "body");
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
        let output = format_task_log_block(&meta, "body");
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
        let formatted = format_sarif_pretty(&sarif);
        assert!(formatted.contains("Something is wrong"));
        assert!(formatted.contains("src/main.rs:10:5"));
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
        let formatted = format_ctrf_pretty(&ctrf);
        assert!(formatted.contains("passed"));
        assert!(formatted.contains("failed"));
        assert!(formatted.contains("test 2"));
        assert!(formatted.contains("Expected 1 to be 2"));
        assert!(formatted.contains("Trace: Error at line 1"));
    }
}
