use std::collections::HashMap;

use luchta_engine::TaskProgress;
use luchta_types::TaskId;
use owo_colors::{OwoColorize, Stream};

use super::console_output::visible_width;
use crate::progress_task_list::{render_task_id_list_with_progress, render_task_id_with_progress};

const HIDDEN_TASK_MARKER: &str = "…";

#[derive(Clone, Copy)]
pub(super) struct StatusLineCounts {
    pub(super) done_or_skipped: usize,
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
    pub(super) failed_segment: Option<&'a str>,
    pub(super) counts: StatusLineCounts,
    pub(super) rss_formatted: &'a str,
    pub(super) warning_suffix: &'a str,
    pub(super) max_width: Option<usize>,
}

pub(super) fn render_status_line(input: StatusLineInput<'_>) -> String {
    let full_task_list =
        render_task_id_list_with_progress(input.running_tasks.clone(), input.task_progress);
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
        let mut segments = vec![format!("✔ {}", self.counts.done_or_skipped)
            .if_supports_color(self.stream, |value| value.green())
            .to_string()];
        self.append_segments(&mut segments, running_task_list);
        let mut line = segments.join(" ");
        line.push_str(self.warning_suffix);
        line
    }

    fn append_segments(&self, segments: &mut Vec<String>, running_task_list: &str) {
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
        push_optional_segment(segments, self.counts.running > 0, || {
            self.running_segment(running_task_list)
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
            format!("🏃 {} ({task_list})", self.counts.running)
        };
        text.if_supports_color(self.stream, |value| value.bright_black())
            .to_string()
    }
}

fn compact_running_task_list(
    mut tasks: Vec<&TaskId>,
    progress: &HashMap<TaskId, TaskProgress>,
    max_width: usize,
) -> String {
    tasks.sort_by_key(|task| task.to_string());
    let total = tasks.len();
    let mut shown = Vec::new();
    let mut shown_width = 0;

    for task in tasks {
        let entry = render_task_id_with_progress(task, progress);
        let separator_width = usize::from(!shown.is_empty()) * 2;
        let candidate_width = shown_width + separator_width + visible_width(&entry);
        let hidden_after = total.saturating_sub(shown.len() + 1);
        let marker_width = if hidden_after > 0 {
            2 + visible_width(HIDDEN_TASK_MARKER)
        } else {
            0
        };
        if candidate_width + marker_width <= max_width {
            shown_width = candidate_width;
            shown.push(entry);
        }
    }

    append_hidden_marker(&mut shown, shown_width, total, max_width);
    shown.join(", ")
}

fn append_hidden_marker(
    shown: &mut Vec<String>,
    shown_width: usize,
    total: usize,
    max_width: usize,
) {
    let hidden = total.saturating_sub(shown.len());
    if hidden == 0 {
        return;
    }

    let marker_width = visible_width(HIDDEN_TASK_MARKER);
    let marker_fits = if shown.is_empty() {
        marker_width <= max_width
    } else {
        shown_width + 2 + marker_width <= max_width
    };
    if marker_fits {
        shown.push(HIDDEN_TASK_MARKER.to_owned());
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

        let compact = compact_running_task_list(task_refs, &HashMap::new(), 8);

        assert_eq!(compact, "b#x, …");
    }
}
