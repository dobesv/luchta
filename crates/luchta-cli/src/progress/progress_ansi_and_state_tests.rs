use super::super::*;
use super::helpers::*;
use std::collections::{HashMap, HashSet};

#[test]
fn render_progress_emits_no_ansi_when_color_unsupported() {
    // Captured (non-tty) output must degrade to plain text identical to the
    // pre-color behavior: no ANSI escape sequences.
    let task = task_id("pkg", "build");
    let reporter =
        ProgressReporter::new(OutputMode::Default, HashMap::from([(task.clone(), 0)]), 1);
    reporter.task_ran(&task);

    let out = owo_colors::with_override(false, || {
        reporter.render_progress(
            "10 MB",
            &[],
            &pressure_snapshot(None, 0, 0),
            owo_colors::Stream::Stdout,
        )
    });

    assert!(
        !out.contains('\u{1b}'),
        "plain status line must not contain ANSI escapes: {out:?}"
    );
    assert!(out.contains("✔ 1"), "output was: {out}");
}

#[test]
fn render_summary_emits_no_ansi_when_color_unsupported() {
    let task = task_id("pkg", "build");
    let reporter =
        ProgressReporter::new(OutputMode::Summary, HashMap::from([(task.clone(), 0)]), 1);
    reporter.task_ran(&task);

    let summary = owo_colors::with_override(false, || {
        reporter.render_summary("10 MB", false, owo_colors::Stream::Stdout)
    });

    assert!(
        !summary.contains('\u{1b}'),
        "plain summary must not contain ANSI escapes: {summary:?}"
    );
}

#[test]
fn render_progress_emits_ansi_when_color_forced() {
    // When color is force-enabled, the status line carries ANSI escapes,
    // while the underlying text (counts/emoji) is preserved. Exercise the
    // shared-cache (cyan) and pressure-warning (red) coloring paths too, not
    // just the done (green) segment.
    let task = task_id("pkg", "build");
    let shared = task_id("pkg", "shared");
    let reporter = ProgressReporter::new(
        OutputMode::Default,
        HashMap::from([(task.clone(), 0), (shared.clone(), 0)]),
        1,
    );
    reporter.task_ran(&task);
    reporter.task_skipped_shared_cache(&shared);

    let sample = MemorySample {
        tree_rss: 32 * 1024 * 1024,
        system_available: 99 * 1024 * 1024,
    };
    let out = owo_colors::with_override(true, || {
        reporter.render_progress(
            "10 MB",
            &[crate::memory_pressure::PressureReason::UsageHigh],
            &pressure_snapshot(Some(sample), 30 * 1024 * 1024, 0),
            owo_colors::Stream::Stdout,
        )
    });

    assert!(
        out.contains('\u{1b}'),
        "forced-color status line must contain ANSI escapes: {out:?}"
    );
    // Underlying text preserved across the colored segments.
    assert!(
        out.contains("✔ 2"),
        "colored output still carries text: {out}"
    );
    assert!(out.contains("📥 1"), "shared-cache segment present: {out}");
    assert!(
        out.contains("❗ mem usage high ("),
        "pressure warning segment present: {out}"
    );
}

#[test]
fn render_summary_emits_ansi_when_color_forced() {
    let task = task_id("pkg", "build");
    let reporter =
        ProgressReporter::new(OutputMode::Summary, HashMap::from([(task.clone(), 0)]), 1);
    reporter.task_ran(&task);

    let summary = owo_colors::with_override(true, || {
        reporter.render_summary("10 MB", false, owo_colors::Stream::Stdout)
    });

    assert!(
        summary.contains('\u{1b}'),
        "forced-color summary must contain ANSI escapes: {summary:?}"
    );
    assert!(
        summary.contains("✔ 1"),
        "colored summary still carries text: {summary}"
    );
}

#[test]
fn task_failed_moves_task_from_running_to_failed_once() {
    let task = task_id("pkg-a", "build");
    let reporter =
        ProgressReporter::new(OutputMode::Default, HashMap::from([(task.clone(), 0)]), 1);

    reporter.task_started(&task);
    reporter.task_failed(&task);
    reporter.task_failed(&task);

    let running_contains_task = reporter
        .running
        .lock()
        .expect("progress reporter running mutex poisoned")
        .contains_key(&task);
    let failed_tasks = reporter
        .failed_tasks
        .lock()
        .expect("progress reporter failed tasks mutex poisoned")
        .iter()
        .cloned()
        .collect::<HashSet<_>>();

    assert_eq!(
        (
            reporter.failed_count(),
            reporter.running_count(),
            running_contains_task,
            failed_tasks,
        ),
        (1, 0, false, HashSet::from([task.clone()])),
        "failed task state mismatch for {task}"
    );
}
