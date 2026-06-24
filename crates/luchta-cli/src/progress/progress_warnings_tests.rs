use super::super::*;
use super::helpers::*;
use std::collections::HashMap;

#[test]
fn render_progress_warnings_usage_only() {
    let reporter = ProgressReporter::new(OutputMode::Default, HashMap::new(), 0);
    let sample = MemorySample {
        tree_rss: 32 * 1024 * 1024,
        system_available: 99 * 1024 * 1024,
    };
    let threshold = 30 * 1024 * 1024;
    let out = reporter.render_progress(
        "10 MB",
        &[crate::memory_pressure::PressureReason::UsageHigh],
        &pressure_snapshot(Some(sample), threshold, 0),
        owo_colors::Stream::Stdout,
    );
    assert!(out.contains("mem usage high ("));
    assert!(out.contains(&rss::format_rss(Some(sample.tree_rss))));
    assert!(out.contains(&rss::format_rss(Some(threshold))));
    assert!(!out.contains("system free memory low"));
}

#[test]
fn render_progress_warnings_free_only() {
    let reporter = ProgressReporter::new(OutputMode::Default, HashMap::new(), 0);
    let sample = MemorySample {
        tree_rss: 32 * 1024 * 1024,
        system_available: 8 * 1024 * 1024,
    };
    let threshold = 16 * 1024 * 1024;
    let out = reporter.render_progress(
        "10 MB",
        &[crate::memory_pressure::PressureReason::FreeLow],
        &pressure_snapshot(Some(sample), 0, threshold),
        owo_colors::Stream::Stdout,
    );
    assert!(out.contains("system free memory low ("));
    assert!(out.contains(&rss::format_rss(Some(sample.system_available))));
    assert!(out.contains(&rss::format_rss(Some(threshold))));
    assert!(!out.contains("mem usage high"));
}

#[test]
fn render_progress_warnings_both() {
    let reporter = ProgressReporter::new(OutputMode::Default, HashMap::new(), 0);
    let warnings = vec![
        crate::memory_pressure::PressureReason::UsageHigh,
        crate::memory_pressure::PressureReason::FreeLow,
    ];
    let sample = MemorySample {
        tree_rss: 32 * 1024 * 1024,
        system_available: 8 * 1024 * 1024,
    };
    let out = reporter.render_progress(
        "10 MB",
        &warnings,
        &pressure_snapshot(Some(sample), 30 * 1024 * 1024, 16 * 1024 * 1024),
        owo_colors::Stream::Stdout,
    );
    assert!(out.contains("❗ mem usage high ("));
    assert!(out.contains("❗ system free memory low ("));
    assert!(out.ends_with(&format!(
        "❗ system free memory low ({} / {})",
        rss::format_rss(Some(sample.system_available)),
        rss::format_rss(Some(16 * 1024 * 1024))
    )));
}

#[test]
fn render_progress_warnings_none() {
    let reporter = ProgressReporter::new(OutputMode::Default, HashMap::new(), 0);
    let out = reporter.render_progress(
        "10 MB",
        &[],
        &pressure_snapshot(None, 0, 0),
        owo_colors::Stream::Stdout,
    );
    assert!(!out.contains("❗"));
}
