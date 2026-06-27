use super::super::*;
use super::helpers::*;
use std::collections::HashMap;

#[test]
fn render_progress_omits_zero_skipped_and_shows_pending_when_work_remains() {
    let wave_of = HashMap::from([(task_id("pkg-a", "build"), 0)]);
    let reporter = ProgressReporter::new(OutputMode::Default, wave_of, 1);

    let out = reporter.render_progress(
        "10 MB",
        &[],
        &pressure_snapshot(None, 0, 0),
        owo_colors::Stream::Stdout,
    );
    assert_progress_line_shape(
        &out,
        "✔ 0 ⌛ 1 ⌚ ",
        SegmentLabel::new("🐏", "10 MB"),
        SegmentLabel::new("🌊", "0 / 1"),
    );
    assert!(!out.contains("⏩"));
    assert!(!out.contains("🏃"));
}

#[test]
fn render_progress_numerator_includes_skipped_and_pending_omits_at_zero() {
    let task_a = task_id("pkg-a", "build");
    let task_b = task_id("pkg-b", "build");
    let task_c = task_id("pkg-c", "build");
    let reporter = ProgressReporter::new(
        OutputMode::Default,
        HashMap::from([
            (task_a.clone(), 0),
            (task_b.clone(), 0),
            (task_c.clone(), 0),
        ]),
        1,
    );

    reporter.task_ran(&task_a);
    reporter.task_skipped_cache_hit(&task_b);
    reporter.task_started(&task_c);

    let out = reporter.render_progress(
        "10 MB",
        &[],
        &pressure_snapshot(None, 0, 0),
        owo_colors::Stream::Stdout,
    );
    assert_progress_line_shape(
        &out,
        "✔ 2 ⏩ 1 🏃 1 (pkg-c#build) ⌚ ",
        SegmentLabel::new("🐏", "10 MB"),
        SegmentLabel::new("🌊", "0 / 1"),
    );
    assert!(!out.contains("⌛"));
}

#[test]
fn render_progress_includes_shared_hits_segment_when_present() {
    let task_a = task_id("pkg-a", "build");
    let task_b = task_id("pkg-b", "build");
    let reporter = ProgressReporter::new(
        OutputMode::Default,
        HashMap::from([(task_a.clone(), 0), (task_b.clone(), 0)]),
        1,
    );

    reporter.task_ran(&task_a);
    reporter.task_skipped_shared_cache(&task_b);

    let out = reporter.render_progress(
        "10 MB",
        &[],
        &pressure_snapshot(None, 0, 0),
        owo_colors::Stream::Stdout,
    );

    assert!(out.contains("📥 1"), "output was: {out}");
    assert!(out.contains("⏩ 1"), "output was: {out}");
}

#[test]
fn render_progress_running_segment_uses_grouped_list() {
    let wave_of = HashMap::from([
        (task_id("a", "lint"), 0),
        (task_id("b", "lint"), 0),
        (task_id("c", "lint"), 0),
        (task_id("d", "test"), 0),
        (task_id("d", "tsc"), 0),
        (task_id("e", "babel"), 0),
    ]);
    let reporter = ProgressReporter::new(OutputMode::Default, wave_of.clone(), 1);
    for task in wave_of.keys() {
        reporter.task_started(task);
    }

    let out = reporter.render_progress(
        "42 MB",
        &[],
        &PressureSnapshot {
            reasons: Vec::new(),
            sample: None,
            usage_threshold: 0,
            free_threshold: 0,
        },
        owo_colors::Stream::Stdout,
    );
    assert_progress_line_shape(
        &out,
        "✔ 0 🏃 6 ({a,b,c}#lint, d#{test,tsc}, e#babel) ⌚ ",
        SegmentLabel::new("🐏", "42 MB"),
        SegmentLabel::new("🌊", "0 / 1"),
    );
    assert!(!out.contains("running:"));
}

#[test]
fn render_progress_failed_segment_uses_grouped_list_and_appears_after_running() {
    let wave_of = HashMap::from([
        (task_id("a", "lint"), 0),
        (task_id("b", "lint"), 0),
        (task_id("c", "lint"), 0),
    ]);
    let reporter = ProgressReporter::new(OutputMode::Default, wave_of.clone(), 1);

    for task in wave_of.keys() {
        reporter.task_started(task);
    }

    reporter.task_failed(&task_id("a", "lint"));
    reporter.task_failed(&task_id("b", "lint"));

    let out = reporter.render_progress(
        "42 MB",
        &[],
        &PressureSnapshot {
            reasons: Vec::new(),
            sample: None,
            usage_threshold: 0,
            free_threshold: 0,
        },
        owo_colors::Stream::Stdout,
    );

    assert!(out.contains("🏃 1 (c#lint)"), "output was: {out}");
    assert!(out.contains("× 2 ({a,b}#lint)"), "output was: {out}");
    assert!(out.find("🏃").unwrap() < out.find("×").unwrap());
}
#[test]
fn render_progress_counts_completed_waves_from_done_skipped_and_failed() {
    let wave_of = HashMap::from([
        (task_id("pkg-a", "build"), 0),
        (task_id("pkg-b", "build"), 0),
        (task_id("pkg-c", "build"), 1),
        (task_id("pkg-d", "build"), 1),
        (task_id("pkg-e", "build"), 2),
    ]);
    let reporter = ProgressReporter::new(OutputMode::Default, wave_of, 3);

    reporter.task_ran(&task_id("pkg-a", "build"));
    reporter.task_skipped_cache_hit(&task_id("pkg-b", "build"));
    reporter.task_ran(&task_id("pkg-c", "build"));
    reporter.task_started(&task_id("pkg-d", "build"));

    let out = reporter.render_progress(
        "24 MB",
        &[],
        &PressureSnapshot {
            reasons: Vec::new(),
            sample: None,
            usage_threshold: 0,
            free_threshold: 0,
        },
        owo_colors::Stream::Stdout,
    );
    assert_progress_line_shape(
        &out,
        "✔ 3 ⏩ 1 ⌛ 1 🏃 1 (pkg-d#build) ⌚ ",
        SegmentLabel::new("🐏", "24 MB"),
        SegmentLabel::new("🌊", "1 / 3"),
    );
    assert!(out.contains("🌊 1 / 3"));
    assert!(!out.contains("W1 "));
    assert!(!out.contains("done ·"));
}

#[test]
fn progress_counts_treat_failed_tasks_as_terminal_and_wave_complete() {
    let tasks = [task_id("pkg-a", "build"), task_id("pkg-b", "build")];
    let reporter = ProgressReporter::new(
        OutputMode::Default,
        HashMap::from([(tasks[0].clone(), 0), (tasks[1].clone(), 0)]),
        1,
    );

    reporter.task_ran(&tasks[0]);
    reporter.task_started(&tasks[1]);
    reporter.task_failed(&tasks[1]);

    let running = reporter
        .running
        .lock()
        .expect("progress reporter running mutex poisoned");
    let counts = reporter.progress_counts(&running);
    drop(running);

    let actual = (
        counts.pending,
        reporter.completed_waves(),
        reporter.failed_count(),
        reporter.wave_failed[0].load(Ordering::SeqCst),
    );
    assert_eq!(actual, (0, 1, 1, 1));
}

#[test]
fn render_progress_ignores_uncounted_tasks_for_running_done_pending_and_waves() {
    let counted = task_id("pkg-a", "build");
    let uncounted = task_id("pkg-b", "build");
    let reporter = ProgressReporter::new(
        OutputMode::Default,
        HashMap::from([(counted.clone(), 0)]),
        1,
    );

    reporter.task_started(&uncounted);
    reporter.task_finished_uncounted(&uncounted);

    let initial = reporter.render_progress(
        "24 MB",
        &[],
        &pressure_snapshot(None, 0, 0),
        owo_colors::Stream::Stdout,
    );
    assert_progress_line_shape(
        &initial,
        "✔ 0 ⌛ 1 ⌚ ",
        SegmentLabel::new("🐏", "24 MB"),
        SegmentLabel::new("🌊", "0 / 1"),
    );
    assert!(!initial.contains("🏃"), "output was: {initial}");

    reporter.task_ran(&counted);
    let finished = reporter.render_progress(
        "24 MB",
        &[],
        &pressure_snapshot(None, 0, 0),
        owo_colors::Stream::Stdout,
    );
    assert_progress_line_shape(
        &finished,
        "✔ 1 ⌚ ",
        SegmentLabel::new("🐏", "24 MB"),
        SegmentLabel::new("🌊", "1 / 1"),
    );
    assert!(!finished.contains("⌛"), "output was: {finished}");
    assert!(!finished.contains("🏃"), "output was: {finished}");
}

#[test]
fn render_progress_counts_zero_task_waves_as_complete_for_denominator() {
    let task_a = task_id("pkg-a", "build");
    let task_b = task_id("pkg-b", "build");
    let reporter = reporter_with_completed_tasks(
        HashMap::from([(task_a.clone(), 0), (task_b.clone(), 2)]),
        3,
        &[&task_a, &task_b],
    );

    let out = reporter.render_progress(
        "24 MB",
        &[],
        &pressure_snapshot(None, 0, 0),
        owo_colors::Stream::Stdout,
    );
    assert_progress_line_shape(
        &out,
        "✔ 2 ⌚ ",
        SegmentLabel::new("🐏", "24 MB"),
        SegmentLabel::new("🌊", "3 / 3"),
    );
}

#[test]
fn render_progress_all_uncounted_selection_keeps_zero_counters_and_reaches_wave_parity() {
    let connector_a = task_id("pkg-a", "noop-a");
    let connector_b = task_id("pkg-a", "noop-b");
    let reporter = ProgressReporter::new(OutputMode::Default, HashMap::new(), 2);

    reporter.task_started(&connector_a);
    reporter.task_finished_uncounted(&connector_a);
    reporter.task_started(&connector_b);
    reporter.task_finished_uncounted(&connector_b);

    let out = reporter.render_progress(
        "24 MB",
        &[],
        &pressure_snapshot(None, 0, 0),
        owo_colors::Stream::Stdout,
    );
    assert_progress_line_shape(
        &out,
        "✔ 0 ⌚ ",
        SegmentLabel::new("🐏", "24 MB"),
        SegmentLabel::new("🌊", "2 / 2"),
    );
    assert!(!out.contains("⌛"), "output was: {out}");
    assert!(!out.contains("🏃"), "output was: {out}");
}
