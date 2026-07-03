use super::super::*;
use super::helpers::*;
use std::collections::HashMap;

#[test]
fn render_summary_includes_failed_suffix_when_present() {
    let task = task_id("pkg-a", "build");
    let reporter =
        ProgressReporter::new(OutputMode::Summary, HashMap::from([(task.clone(), 0)]), 1);

    reporter.task_failed(&task);

    let summary = reporter.render_summary("10 MB", false, owo_colors::Stream::Stdout);

    assert!(summary.contains("× 1 ("), "summary was: {summary}");
    assert!(summary.contains("pkg-a#build"), "summary was: {summary}");
    assert!(summary.contains("🐏 10 MB"), "summary was: {summary}");
    assert!(summary.ends_with("🌊 1 / 1"), "summary was: {summary}");
}
#[test]
fn render_summary_omits_skipped_suffix_when_zero() {
    let task = task_id("pkg-a", "build");
    let reporter =
        ProgressReporter::new(OutputMode::Summary, HashMap::from([(task.clone(), 0)]), 1);

    reporter.task_ran(&task);

    let summary = reporter.render_summary("10 MB", false, owo_colors::Stream::Stdout);

    DoneSummaryExpectation {
        done: 1,
        skipped: 0,
        waves: 1,
    }
    .assert_in(&summary);
    assert!(summary.contains("🐏 10 MB"));
}

#[test]
fn render_summary_includes_skipped_suffix_when_present() {
    let task_a = task_id("pkg-a", "build");
    let task_b = task_id("pkg-b", "build");
    let reporter = ProgressReporter::new(
        OutputMode::Summary,
        HashMap::from([(task_a.clone(), 0), (task_b.clone(), 1)]),
        2,
    );

    reporter.task_ran(&task_a);
    reporter.task_skipped_cache_hit(&task_b);

    let summary = reporter.render_summary("10 MB", false, owo_colors::Stream::Stdout);

    DoneSummaryExpectation {
        done: 2,
        skipped: 1,
        waves: 2,
    }
    .assert_in(&summary);
    assert!(summary.contains("🐏 10 MB"));
}

#[test]
fn render_summary_includes_shared_hits_suffix_when_present() {
    let task_a = task_id("pkg-a", "build");
    let task_b = task_id("pkg-b", "build");
    let reporter = ProgressReporter::new(
        OutputMode::Default,
        HashMap::from([(task_a.clone(), 0), (task_b.clone(), 0)]),
        1,
    );

    reporter.task_ran(&task_a);
    reporter.task_skipped_shared_cache(&task_b);

    let summary = reporter.render_summary("10 MB", false, owo_colors::Stream::Stdout);

    assert!(summary.contains("✔ 2 ⏩ 1 📥 1"), "summary was: {summary}");
}

#[test]
fn render_summary_includes_cancelled_suffix_when_true() {
    let reporter = ProgressReporter::new(OutputMode::Default, HashMap::new(), 1);
    let summary = reporter.render_summary("10 MB", true, owo_colors::Stream::Stdout);
    assert!(
        summary.contains("❗ new changes detected"),
        "summary was: {summary}"
    );
}
