use std::collections::HashMap;

use luchta_types::TaskId;

use crate::{cli::OutputMode, progress::ProgressReporter};

pub(super) struct DoneSummaryExpectation {
    pub(super) done: usize,
    pub(super) skipped: usize,
    pub(super) waves: usize,
}

impl DoneSummaryExpectation {
    pub(super) fn assert_in(&self, out: &str) {
        let done_segment = format!("✔ {}", self.done);
        assert!(
            out.contains(&done_segment),
            "expected done summary '{done_segment}', got: {out}"
        );
        if self.skipped > 0 {
            let skipped_segment = format!("⏩ {}", self.skipped);
            assert!(
                out.contains(&skipped_segment),
                "expected skipped summary '{skipped_segment}', got: {out}"
            );
        } else {
            assert!(
                !out.contains("⏩"),
                "expected zero skipped tasks to be omitted, got: {out}"
            );
        }
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

/// The shape a rendered progress line is expected to have. Grouped into a
/// struct, rather than passed as bare positional arguments, because
/// `expected_prefix` was a `&str` sitting right next to the rendered line
/// under test — also a `&str` — making the two easy to transpose at a call
/// site with no compiler error.
pub(super) struct ExpectedProgressShape<'a> {
    pub(super) prefix: &'a str,
    pub(super) rss: SegmentLabel,
    pub(super) wave_progress: SegmentLabel,
}

/// A progress line as `ProgressReporter` rendered it, plus the parsers that
/// pull structured segments back out of it for assertions.
///
/// `progress_prefix`, `elapsed_label`, `rss_segment`, `wave_segment`, and
/// `stripped` used to be five free functions that each took the same
/// rendered line as a bare `&str` argument. Grouping them as `&self` methods
/// on the line they parse means none of them carries a string parameter of
/// its own, and a call site holds one `RenderedLine` instead of threading the
/// same string through five separate calls.
pub(super) struct RenderedLine<'a>(&'a str);

impl<'a> RenderedLine<'a> {
    pub(super) fn new(out: &'a str) -> Self {
        Self(out)
    }

    pub(super) fn raw(&self) -> &'a str {
        self.0
    }

    /// The line with ANSI escape sequences removed, for parsing segments that
    /// a live-redraw line and its plain-mode twin should share.
    pub(super) fn stripped(&self) -> String {
        let mut stripped = String::with_capacity(self.0.len());
        let mut chars = self.0.chars();
        while let Some(ch) = chars.next() {
            if ch != '\u{1b}' {
                stripped.push(ch);
                continue;
            }

            consume_ansi_sequence(&mut chars);
        }
        stripped
    }

    pub(super) fn prefix(&self) -> String {
        let stripped = self.stripped();
        stripped
            .split_once('⌚')
            .map(|(prefix, _)| format!("{prefix}⌚ "))
            .expect("progress line should contain elapsed segment")
    }

    pub(super) fn elapsed_label(&self) -> String {
        let stripped = self.stripped();
        stripped
            .split_once('⌚')
            .map(|(_, rest)| rest.trim())
            .and_then(|rest| rest.split_whitespace().next())
            .map(str::to_owned)
            .expect("progress line should contain elapsed value")
    }

    pub(super) fn rss_segment(&self) -> String {
        segment_between(
            self.0,
            SegmentLabel::new("🐏", "🐏"),
            SegmentLabel::new("🌊", "🌊"),
        )
    }

    pub(super) fn wave_segment(&self) -> String {
        if self.0.contains('🏃') {
            segment_between(
                self.0,
                SegmentLabel::new("🌊", "🌊"),
                SegmentLabel::new("🏃", "🏃"),
            )
        } else {
            segment_from(self.0, SegmentLabel::new("🌊", "🌊"))
        }
    }
}

pub(super) fn assert_progress_line_shape(
    line: RenderedLine<'_>,
    expected: ExpectedProgressShape<'_>,
) {
    let actual = (
        line.prefix(),
        line.elapsed_label(),
        line.rss_segment(),
        line.wave_segment(),
        line.raw().contains("done ·"),
    );
    let expected = (
        expected.prefix.to_owned(),
        "0s".to_owned(),
        expected.rss.to_segment(),
        expected.wave_progress.to_segment(),
        false,
    );
    assert_eq!(actual, expected, "unexpected progress line: {}", line.raw());
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

pub(super) use crate::progress_task_list::render_running_task_groups;
