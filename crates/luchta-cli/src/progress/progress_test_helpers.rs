use std::collections::HashMap;

use luchta_types::TaskId;

use crate::{cli::OutputMode, memory_pressure::PressureSnapshot, progress::ProgressReporter};

pub(super) struct DoneSummaryExpectation {
    pub(super) done: usize,
    pub(super) skipped: usize,
    pub(super) waves: usize,
}

impl DoneSummaryExpectation {
    pub(super) fn assert_in(&self, out: &str) {
        assert!(
            out.contains(&format!("✔ {} ⏩ {}", self.done, self.skipped)),
            "expected done summary '✔ {} ⏩ {}', got: {out}",
            self.done,
            self.skipped
        );
        assert!(
            out.contains(&format!("🌊 {} / {}", self.waves, self.waves)),
            "expected wave summary '🌊 {} / {}', got: {out}",
            self.waves,
            self.waves
        );
        assert!(
            !out.contains("Done:"),
            "should not contain old 'Done:', got: {out}"
        );
    }
}

pub(super) fn pressure_snapshot(
    sample: Option<MemorySample>,
    usage_threshold: u64,
    free_threshold: u64,
) -> PressureSnapshot {
    PressureSnapshot {
        reasons: Vec::new(),
        sample,
        usage_threshold,
        free_threshold,
    }
}

pub(super) fn assert_progress_line_shape(
    out: &str,
    expected_prefix: &str,
    rss: SegmentLabel,
    wave_progress: SegmentLabel,
) {
    let stripped = strip_ansi(out);
    let actual = (
        progress_prefix(&stripped),
        elapsed_label(&stripped),
        rss_segment(out),
        wave_segment(out),
        out.contains("done ·"),
    );
    let expected = (
        expected_prefix.to_owned(),
        "0s".to_owned(),
        rss.to_segment(),
        wave_progress.to_segment(),
        false,
    );
    assert_eq!(actual, expected, "unexpected progress line: {out}");
}

#[derive(Clone, Copy)]
pub(super) struct SegmentLabel {
    icon: &'static str,
    value: &'static str,
}

impl SegmentLabel {
    pub(super) const fn new(icon: &'static str, value: &'static str) -> Self {
        Self { icon, value }
    }

    pub(super) fn to_segment(self) -> String {
        format!("{} {}", self.icon, self.value)
    }
}

pub(super) fn progress_prefix(stripped: &str) -> String {
    stripped
        .split_once('⌚')
        .map(|(prefix, _)| format!("{prefix}⌚ "))
        .expect("progress line should contain elapsed segment")
}

pub(super) fn elapsed_label(stripped: &str) -> String {
    stripped
        .split_once('⌚')
        .map(|(_, rest)| rest.trim())
        .and_then(|rest| rest.split_whitespace().next())
        .map(str::to_owned)
        .expect("progress line should contain elapsed value")
}

pub(super) fn rss_segment(out: &str) -> String {
    segment_between(
        out,
        SegmentLabel::new("🐏", "🐏"),
        SegmentLabel::new("🌊", "🌊"),
    )
}

pub(super) fn wave_segment(out: &str) -> String {
    segment_from(out, SegmentLabel::new("🌊", "🌊"))
}

pub(super) fn segment_between(
    out: &str,
    marker: SegmentLabel,
    next_marker: SegmentLabel,
) -> String {
    out.split(marker.icon)
        .nth(1)
        .and_then(|suffix| suffix.split(next_marker.icon).next())
        .map(|suffix| format!("{} {}", marker.icon, suffix.trim()))
        .expect("progress line should contain segment")
}

pub(super) fn segment_from(out: &str, marker: SegmentLabel) -> String {
    out.split(marker.icon)
        .nth(1)
        .map(|suffix| format!("{} {}", marker.icon, suffix.trim()))
        .expect("progress line should contain segment")
}

pub(super) fn strip_ansi(text: &str) -> String {
    let mut stripped = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            stripped.push(ch);
            continue;
        }

        consume_ansi_sequence(&mut chars);
    }
    stripped
}

pub(super) fn consume_ansi_sequence(chars: &mut std::str::Chars<'_>) {
    for next in chars.by_ref() {
        if next.is_ascii_alphabetic() {
            return;
        }
    }
}

pub(super) fn running_tasks(tasks: &[(&str, &str)]) -> Vec<&'static TaskId> {
    let leaked = Box::leak(
        tasks
            .iter()
            .map(|(package, task)| task_id(package, task))
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    leaked.iter().collect()
}

pub(super) fn task_id(package: &str, task: &str) -> TaskId {
    TaskId::new(package, task)
}

pub(super) fn reporter_with_completed_tasks(
    wave_of: HashMap<TaskId, usize>,
    total_waves: usize,
    completed_tasks: &[&TaskId],
) -> ProgressReporter {
    let reporter = ProgressReporter::new(OutputMode::Default, wave_of, total_waves);
    for task in completed_tasks {
        reporter.task_ran(task);
    }
    reporter
}

pub(super) use crate::memory_pressure::MemorySample;
pub(super) use crate::progress_task_list::render_running_task_groups;
pub(super) use crate::rss;
