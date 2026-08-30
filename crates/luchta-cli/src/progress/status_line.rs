use std::{collections::HashMap, time::Duration};

use luchta_engine::TaskProgress;
use luchta_types::TaskId;
use owo_colors::{OwoColorize, Stream};

use super::console_output::visible_width;
use crate::progress_task_list::{
    render_running_task_list, render_task_id_with_status, shared_package_prefix_for_tasks,
    shared_scope_for_tasks,
};

const HIDDEN_TASK_MARKER: &str = "…";

#[derive(Clone, Copy)]
pub(super) struct StatusLineCounts {
    pub(super) completed: usize,
    pub(super) skipped: usize,
    pub(super) shared_hits: usize,
    pub(super) pending: usize,
    pub(super) running: usize,
    pub(super) elapsed_total: u64,
    pub(super) waves_done: usize,
    pub(super) total_waves: usize,
}

pub(super) struct StatusLineInput<'a> {
    pub(super) stream: Stream,
    pub(super) running_tasks: Vec<&'a TaskId>,
    pub(super) task_progress: &'a HashMap<TaskId, TaskProgress>,
    pub(super) task_elapsed: &'a HashMap<TaskId, Duration>,
    pub(super) failed_segment: Option<&'a str>,
    pub(super) counts: StatusLineCounts,
    pub(super) rss_formatted: &'a str,
    pub(super) warning_suffix: &'a str,
    pub(super) max_width: Option<usize>,
}

pub(super) fn render_status_line(input: StatusLineInput<'_>) -> String {
    let full_task_list = render_running_task_list(
        &input.running_tasks,
        input.task_progress,
        input.task_elapsed,
    );
    let parts = StatusLineParts {
        stream: input.stream,
        failed_segment: input.failed_segment,
        counts: input.counts,
        rss_formatted: input.rss_formatted,
        warning_suffix: input.warning_suffix,
    };
    let line = parts.render(&full_task_list);

    let Some(max_width) = input
        .max_width
        .filter(|width| visible_width(&line) > *width)
    else {
        return line;
    };
    let fixed_width = visible_width(&line).saturating_sub(visible_width(&full_task_list));
    let compact_task_list = compact_running_task_list(
        input.running_tasks,
        input.task_progress,
        input.task_elapsed,
        max_width.saturating_sub(fixed_width),
    );
    parts.render(&compact_task_list)
}

#[derive(Clone, Copy)]
struct StatusLineParts<'a> {
    stream: Stream,
    failed_segment: Option<&'a str>,
    counts: StatusLineCounts,
    rss_formatted: &'a str,
    warning_suffix: &'a str,
}

impl StatusLineParts<'_> {
    fn render(&self, running_task_list: &str) -> String {
        let mut segments = vec![format!("✔ {}", self.counts.completed)
            .if_supports_color(self.stream, |value| value.green())
            .to_string()];
        self.append_fixed_segments(&mut segments);
        let mut line = segments.join(" ");
        line.push_str(self.warning_suffix);
        if self.counts.running > 0 {
            line.push(' ');
            line.push_str(&self.running_segment(running_task_list));
        }
        line
    }

    fn append_fixed_segments(&self, segments: &mut Vec<String>) {
        push_optional_segment(segments, self.counts.skipped > 0, || {
            format!("⏩ {}", self.counts.skipped)
                .if_supports_color(self.stream, |value| value.cyan())
                .to_string()
        });
        push_optional_segment(segments, self.counts.shared_hits > 0, || {
            format!("📥 {}", self.counts.shared_hits)
                .if_supports_color(self.stream, |value| value.cyan())
                .to_string()
        });
        push_optional_segment(segments, self.counts.pending > 0, || {
            format!("⌛ {}", self.counts.pending)
                .if_supports_color(self.stream, |value| value.dimmed())
                .to_string()
        });
        if let Some(segment) = self.failed_segment {
            segments.push(segment.to_owned());
        }
        segments.extend([
            format!("⌚ {}s", self.counts.elapsed_total)
                .if_supports_color(self.stream, |value| value.dimmed())
                .to_string(),
            format!("🐏 {}", self.rss_formatted)
                .if_supports_color(self.stream, |value| value.dimmed())
                .to_string(),
            format!(
                "🌊 {} / {}",
                self.counts.waves_done, self.counts.total_waves
            )
            .if_supports_color(self.stream, |value| value.dimmed())
            .to_string(),
        ]);
    }

    fn running_segment(&self, task_list: &str) -> String {
        let text = if task_list.is_empty() {
            format!("🏃 {}", self.counts.running)
        } else {
            format!("🏃 {} {task_list}", self.counts.running)
        };
        text.if_supports_color(self.stream, |value| value.bright_black())
            .to_string()
    }
}

fn compact_running_task_list(
    tasks: Vec<&TaskId>,
    progress: &HashMap<TaskId, TaskProgress>,
    elapsed: &HashMap<TaskId, Duration>,
    max_width: usize,
) -> String {
    let total = tasks.len();
    let mut shown = Vec::new();

    for task in tasks {
        shown.push(task);
        let candidate = render_compact_entries(&shown, progress, elapsed, total > shown.len());
        if visible_width(&candidate) > max_width {
            shown.pop();
        }
    }

    let compact = render_compact_entries(&shown, progress, elapsed, total > shown.len());
    if visible_width(&compact) <= max_width {
        compact
    } else {
        String::new()
    }
}

fn render_compact_entries(
    tasks: &[&TaskId],
    progress: &HashMap<TaskId, TaskProgress>,
    elapsed: &HashMap<TaskId, Duration>,
    hidden: bool,
) -> String {
    let shared_scope = if tasks.iter().any(|task| task.package.is_root()) {
        None
    } else {
        shared_scope_for_tasks(tasks)
    };
    let package_prefix = shared_package_prefix_for_tasks(tasks, shared_scope)
        .filter(|prefix| visible_width(prefix).saturating_mul(tasks.len().saturating_sub(1)) >= 2);
    let mut entries = tasks
        .iter()
        .map(|task| {
            render_task_id_with_status(task, shared_scope, package_prefix, progress, elapsed)
        })
        .collect::<Vec<_>>();
    if hidden {
        entries.push(HIDDEN_TASK_MARKER.to_owned());
    }

    let contents = entries.join(", ");
    if let Some(prefix) = package_prefix {
        format!("{prefix}{{{contents}}}")
    } else {
        contents
    }
}

fn push_optional_segment<F>(segments: &mut Vec<String>, include: bool, build: F)
where
    F: FnOnce() -> String,
{
    if include {
        segments.push(build());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use luchta_types::{PackageName, TaskName};

    fn task_id(package: &str, task: &str) -> TaskId {
        TaskId::new(PackageName::from(package), TaskName::from(task))
    }

    #[test]
    fn compact_list_keeps_marker_when_an_earlier_task_does_not_fit() {
        let tasks = [
            task_id("aaa-package-name-too-long-to-fit", "build"),
            task_id("b", "x"),
            task_id("c", "x"),
        ];
        let task_refs = tasks.iter().collect();

        let compact = compact_running_task_list(task_refs, &HashMap::new(), &HashMap::new(), 8);

        assert_eq!(compact, "b#x, …");
    }

    #[test]
    fn compact_list_omits_scope_shared_by_visible_age_ordered_tasks() {
        let tasks = [
            task_id("@formative/react-main", "lint:styles"),
            task_id("@formative/test-data", "build:browser"),
            task_id(
                "@formative/package-name-too-long-for-the-remaining-width",
                "test",
            ),
        ];
        let elapsed = HashMap::from([
            (tasks[0].clone(), Duration::from_secs(45)),
            (tasks[1].clone(), Duration::from_secs(10)),
            (tasks[2].clone(), Duration::from_secs(6)),
        ]);
        let expected = "react-main#lint:styles(⌚ 45s), test-data#build:browser(⌚ 10s), …";

        let compact = compact_running_task_list(
            tasks.iter().collect(),
            &HashMap::new(),
            &elapsed,
            visible_width(expected),
        );

        assert_eq!(compact, expected);
    }

    #[test]
    fn compact_list_factors_package_prefix_after_omitting_shared_scope() {
        let tasks = [
            task_id("@formative/react-components", "lint:styles"),
            task_id("@formative/react-item-bank-item-generation", "lint:styles"),
            task_id(
                "@formative/react-package-name-too-long-for-the-remaining-width",
                "test",
            ),
        ];
        let elapsed = HashMap::from([
            (tasks[0].clone(), Duration::from_secs(32)),
            (tasks[1].clone(), Duration::from_secs(24)),
            (tasks[2].clone(), Duration::from_secs(6)),
        ]);
        let expected = "react-{components#lint:styles(⌚ 32s), item-bank-item-generation#lint:styles(⌚ 24s), …}";

        let compact = compact_running_task_list(
            tasks.iter().collect(),
            &HashMap::new(),
            &elapsed,
            visible_width(expected),
        );

        assert_eq!(compact, expected);
    }

    #[test]
    fn running_tasks_follow_fixed_status_and_warnings_without_outer_parentheses() {
        let task = task_id("pkg", "build");
        let line = render_status_line(StatusLineInput {
            stream: Stream::Stdout,
            running_tasks: vec![&task],
            task_progress: &HashMap::new(),
            task_elapsed: &HashMap::new(),
            failed_segment: Some("× 1 failed#test"),
            counts: StatusLineCounts {
                completed: 1,
                skipped: 0,
                shared_hits: 0,
                pending: 0,
                running: 1,
                elapsed_total: 9,
                waves_done: 1,
                total_waves: 2,
            },
            rss_formatted: "10 MB",
            warning_suffix: " ❗ warning",
            max_width: None,
        });

        assert_eq!(
            line,
            "✔ 1 × 1 failed#test ⌚ 9s 🐏 10 MB 🌊 1 / 2 ❗ warning 🏃 1 pkg#build"
        );
    }
}
