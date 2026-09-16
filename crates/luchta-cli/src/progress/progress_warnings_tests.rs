use super::super::*;
use std::collections::HashMap;

use crate::memory_pressure::PressureDetail;

#[test]
fn render_progress_shows_no_suffix_without_pressure() {
    let reporter = ProgressReporter::new(OutputMode::Default, HashMap::new(), 0);
    let out = reporter.render_progress("10 MB", None, owo_colors::Stream::Stdout);
    assert!(!out.contains("memory pressure"));
}

#[test]
fn render_progress_shows_linux_stall_percentage() {
    let reporter = ProgressReporter::new(OutputMode::Default, HashMap::new(), 0);
    let out = reporter.render_progress(
        "10 MB",
        Some(PressureDetail::Stalled(23.0)),
        owo_colors::Stream::Stdout,
    );
    assert!(out.ends_with("❗ memory pressure (stalled 23%)"));
}

#[test]
fn render_progress_shows_platform_pressure_levels() {
    let reporter = ProgressReporter::new(OutputMode::Default, HashMap::new(), 0);
    for (detail, expected) in [
        (PressureDetail::Warning, "warning"),
        (PressureDetail::Critical, "critical"),
        (PressureDetail::LowMemory, "low memory"),
    ] {
        let out = reporter.render_progress("10 MB", Some(detail), owo_colors::Stream::Stdout);
        assert!(
            out.ends_with(&format!("❗ memory pressure ({expected})")),
            "detail {detail:?} rendered as: {out}"
        );
    }
}
