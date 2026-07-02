use std::{
    collections::HashMap,
    collections::HashSet,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
    time::Instant,
};

use luchta_types::TaskId;
use owo_colors::{OwoColorize, Stream};

use crate::{
    cli::OutputMode,
    memory_pressure::{PressureReason, PressureSnapshot},
    progress_task_list::render_task_id_list,
};

/// Outcome of a task as recorded by the progress reporter.
///
/// A successful run increments the wave's `done` bucket; a cache hit increments
/// `skipped`. Shared-cache hits also increment a dedicated `shared_hits`
/// counter. Everything else — ordering-only no-worker nodes, previous-failure
/// skips, config errors, tasks outside the requested subgraph, and execution
/// failures — is `Uncounted`: removed from running set but not added to done or
/// skipped totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskOutcome {
    /// Task executed successfully (increments the wave's done count).
    Ran,
    /// Task skipped due to a local cache hit.
    SkippedLocalCache,
    /// Task skipped due to a shared cache hit.
    SkippedSharedCache,
    /// Outcome that contributes to neither the done nor the skipped totals.
    Uncounted,
}

#[derive(Debug)]
pub struct ProgressReporter {
    pub wave_of: HashMap<TaskId, usize>,
    pub wave_done: Vec<AtomicUsize>,
    pub wave_skipped: Vec<AtomicUsize>,
    pub wave_failed: Vec<AtomicUsize>,
    pub running: Mutex<HashMap<TaskId, Instant>>,
    pub failed_tasks: Mutex<HashSet<TaskId>>,
    done: AtomicUsize,
    skipped: AtomicUsize,
    failed: AtomicUsize,
    shared_hits: AtomicUsize,
    pub mode: OutputMode,
    pub total_waves: usize,
    pub wave_total: Vec<usize>,
    pub start: Instant,
}

impl ProgressReporter {
    pub fn new(mode: OutputMode, wave_of: HashMap<TaskId, usize>, total_waves: usize) -> Self {
        let mut wave_total = vec![0; total_waves];
        for &wave_index in wave_of.values() {
            if let Some(total) = wave_total.get_mut(wave_index) {
                *total += 1;
            }
        }

        Self {
            mode,
            wave_of,
            total_waves,
            wave_done: (0..total_waves).map(|_| AtomicUsize::new(0)).collect(),
            wave_skipped: (0..total_waves).map(|_| AtomicUsize::new(0)).collect(),
            wave_failed: (0..total_waves).map(|_| AtomicUsize::new(0)).collect(),
            wave_total,
            running: Mutex::new(HashMap::new()),
            failed_tasks: Mutex::new(HashSet::new()),
            start: Instant::now(),
            done: AtomicUsize::new(0),
            skipped: AtomicUsize::new(0),
            failed: AtomicUsize::new(0),
            shared_hits: AtomicUsize::new(0),
        }
    }

    pub fn task_started(&self, id: &TaskId) {
        if !self.wave_of.contains_key(id) {
            return;
        }

        let mut running = self
            .running
            .lock()
            .expect("progress reporter running mutex poisoned");
        running.insert(id.clone(), Instant::now());
    }

    pub fn task_ran(&self, id: &TaskId) {
        self.finish_task(id, TaskOutcome::Ran);
    }

    pub fn task_skipped_cache_hit(&self, id: &TaskId) {
        self.finish_task(id, TaskOutcome::SkippedLocalCache);
    }

    pub fn task_skipped_shared_cache(&self, id: &TaskId) {
        self.finish_task(id, TaskOutcome::SkippedSharedCache);
    }

    pub fn task_finished_uncounted(&self, id: &TaskId) {
        self.finish_task(id, TaskOutcome::Uncounted);
    }

    pub fn task_failed(&self, id: &TaskId) {
        let mut running = self
            .running
            .lock()
            .expect("progress reporter running mutex poisoned");
        running.remove(id);
        drop(running);

        let mut failed_tasks = self
            .failed_tasks
            .lock()
            .expect("progress reporter failed tasks mutex poisoned");
        if failed_tasks.insert(id.clone()) {
            self.failed.fetch_add(1, Ordering::SeqCst);
            if let Some(&wave_index) = self.wave_of.get(id) {
                self.wave_failed[wave_index].fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn failed_count(&self) -> usize {
        self.failed.load(Ordering::SeqCst)
    }

    pub fn running_count(&self) -> usize {
        let running = self
            .running
            .lock()
            .expect("progress reporter running mutex poisoned");
        running.len()
    }

    pub fn render_progress(
        &self,
        rss_formatted: &str,
        warnings: &[PressureReason],
        pressure: &PressureSnapshot,
        stream: Stream,
    ) -> String {
        let running = self
            .running
            .lock()
            .expect("progress reporter running mutex poisoned");
        let counts = self.progress_counts(&running);

        let mut segments = vec![format!("✔ {}", counts.done_or_skipped)
            .if_supports_color(stream, |t| t.green())
            .to_string()];
        extend_progress_segments(
            &mut segments,
            stream,
            &running,
            self.failed_segment(stream),
            ProgressSegmentCounts {
                skipped: counts.skipped,
                shared_hits: counts.shared_hits,
                pending: counts.pending,
                running_count: counts.running_count,
                elapsed_total: counts.elapsed_total,
                waves_done: counts.waves_done,
                total_waves: self.total_waves,
            },
            rss_formatted,
        );

        let mut line = segments.join(" ");
        line.push_str(&pressure_suffix(warnings, pressure, stream));
        line
    }

    pub fn render_summary(
        &self,
        rss_formatted: &str,
        was_cancelled: bool,
        stream: Stream,
    ) -> String {
        let elapsed_total = self.start.elapsed().as_secs();
        let done = self.done.load(Ordering::SeqCst);
        let skipped = self.skipped.load(Ordering::SeqCst);
        let shared_hits = self.shared_hits.load(Ordering::SeqCst);
        let done_or_skipped = done + skipped;
        let done_str = format!("✔ {done_or_skipped}")
            .if_supports_color(stream, |t| t.green())
            .to_string();

        let skipped_str = format!("⏩ {skipped}")
            .if_supports_color(stream, |t| t.cyan())
            .to_string();

        let failed_segment = self
            .failed_segment(stream)
            .map(|segment| format!(" {segment}"))
            .unwrap_or_default();

        let shared_segment = if shared_hits > 0 {
            format!(" 📥 {shared_hits}")
                .if_supports_color(stream, |t| t.cyan())
                .to_string()
        } else {
            String::new()
        };

        let elapsed_str = format!("⌚ {elapsed_total}s")
            .if_supports_color(stream, |t| t.dimmed())
            .to_string();
        let rss_str = format!("🐏 {rss_formatted}")
            .if_supports_color(stream, |t| t.dimmed())
            .to_string();
        let waves_str = format!("🌊 {} / {}", self.total_waves, self.total_waves)
            .if_supports_color(stream, |t| t.dimmed())
            .to_string();

        let cancelled_segment = if was_cancelled {
            " ❗ new changes detected"
                .if_supports_color(stream, |t| t.yellow())
                .to_string()
        } else {
            String::new()
        };

        format!("{done_str} {skipped_str}{failed_segment}{shared_segment} {elapsed_str} {rss_str} {waves_str}{cancelled_segment}")
    }

    fn finish_task(&self, id: &TaskId, kind: TaskOutcome) {
        let mut running = self
            .running
            .lock()
            .expect("progress reporter running mutex poisoned");
        running.remove(id);
        drop(running);

        let Some(&wave_index) = self.wave_of.get(id) else {
            return;
        };

        match kind {
            TaskOutcome::Ran => {
                self.wave_done[wave_index].fetch_add(1, Ordering::SeqCst);
                self.done.fetch_add(1, Ordering::SeqCst);
            }
            TaskOutcome::SkippedLocalCache => {
                self.wave_skipped[wave_index].fetch_add(1, Ordering::SeqCst);
                self.skipped.fetch_add(1, Ordering::SeqCst);
            }
            TaskOutcome::SkippedSharedCache => {
                self.wave_skipped[wave_index].fetch_add(1, Ordering::SeqCst);
                self.skipped.fetch_add(1, Ordering::SeqCst);
                self.shared_hits.fetch_add(1, Ordering::SeqCst);
            }
            TaskOutcome::Uncounted => {}
        }
    }

    fn failed_segment(&self, stream: Stream) -> Option<String> {
        let failed_tasks = self
            .failed_tasks
            .lock()
            .expect("progress reporter failed tasks mutex poisoned");
        if failed_tasks.is_empty() {
            return None;
        }

        Some(
            format!(
                "× {} ({})",
                self.failed.load(Ordering::SeqCst),
                render_task_id_list(failed_tasks.iter().collect())
            )
            .if_supports_color(stream, |t| t.red())
            .to_string(),
        )
    }

    fn progress_counts(&self, running: &HashMap<TaskId, Instant>) -> ProgressCounts {
        let total_tasks: usize = self.wave_total.iter().sum();
        let done = self.done.load(Ordering::SeqCst);
        let skipped = self.skipped.load(Ordering::SeqCst);
        let failed = self.failed.load(Ordering::SeqCst);
        let shared_hits = self.shared_hits.load(Ordering::SeqCst);
        let done_or_skipped = done + skipped;
        let running_count = running.len();
        let pending = total_tasks.saturating_sub(done_or_skipped + running_count + failed);

        ProgressCounts {
            done_or_skipped,
            skipped,
            shared_hits,
            running_count,
            pending,
            elapsed_total: self.start.elapsed().as_secs(),
            waves_done: self.completed_waves(),
        }
    }

    fn completed_waves(&self) -> usize {
        self.wave_total
            .iter()
            .enumerate()
            .filter(|(wave_index, wave_total)| {
                **wave_total == 0
                    || self.wave_done[*wave_index].load(Ordering::SeqCst)
                        + self.wave_skipped[*wave_index].load(Ordering::SeqCst)
                        + self.wave_failed[*wave_index].load(Ordering::SeqCst)
                        == **wave_total
            })
            .count()
    }
}

struct ProgressCounts {
    done_or_skipped: usize,
    skipped: usize,
    shared_hits: usize,
    running_count: usize,
    pending: usize,
    elapsed_total: u64,
    waves_done: usize,
}

struct ProgressSegmentCounts {
    skipped: usize,
    shared_hits: usize,
    pending: usize,
    running_count: usize,
    elapsed_total: u64,
    waves_done: usize,
    total_waves: usize,
}

fn extend_progress_segments(
    segments: &mut Vec<String>,
    stream: Stream,
    running: &HashMap<TaskId, Instant>,
    failed_segment: Option<String>,
    counts: ProgressSegmentCounts,
    rss_formatted: &str,
) {
    push_optional_segment(segments, counts.skipped > 0, || {
        format!("⏩ {}", counts.skipped)
            .if_supports_color(stream, |value| value.cyan())
            .to_string()
    });
    push_optional_segment(segments, counts.shared_hits > 0, || {
        format!("📥 {}", counts.shared_hits)
            .if_supports_color(stream, |value| value.cyan())
            .to_string()
    });
    push_optional_segment(segments, counts.pending > 0, || {
        format!("⌛ {}", counts.pending)
            .if_supports_color(stream, |value| value.dimmed())
            .to_string()
    });
    push_optional_segment(segments, counts.running_count > 0, || {
        format!(
            "🏃 {} ({})",
            counts.running_count,
            render_task_id_list(running.keys().collect())
        )
        .if_supports_color(stream, |value| value.yellow())
        .to_string()
    });
    if let Some(segment) = failed_segment {
        segments.push(segment);
    }
    segments.push(
        format!("⌚ {}s", counts.elapsed_total)
            .if_supports_color(stream, |value| value.dimmed())
            .to_string(),
    );
    segments.push(
        format!("🐏 {rss_formatted}")
            .if_supports_color(stream, |value| value.dimmed())
            .to_string(),
    );
    segments.push(
        format!("🌊 {} / {}", counts.waves_done, counts.total_waves)
            .if_supports_color(stream, |value| value.dimmed())
            .to_string(),
    );
}

fn push_optional_segment<F>(segments: &mut Vec<String>, include: bool, build: F)
where
    F: FnOnce() -> String,
{
    if include {
        segments.push(build());
    }
}
fn pressure_suffix(
    warnings: &[PressureReason],
    pressure: &PressureSnapshot,
    stream: Stream,
) -> String {
    let mut suffix = String::new();
    let sample = pressure.sample;
    for warning in warnings {
        match warning {
            PressureReason::UsageHigh => {
                let measured = crate::rss::format_rss(sample.map(|sample| sample.tree_rss));
                let threshold = crate::rss::format_rss(Some(pressure.usage_threshold));
                suffix.push_str(
                    &format!(" ❗ mem usage high ({measured} / {threshold})")
                        .if_supports_color(stream, |t| t.red())
                        .to_string(),
                );
            }
            PressureReason::FreeLow => {
                let measured = crate::rss::format_rss(sample.map(|sample| sample.system_available));
                let threshold = crate::rss::format_rss(Some(pressure.free_threshold));
                suffix.push_str(
                    &format!(" ❗ system free memory low ({measured} / {threshold})")
                        .if_supports_color(stream, |t| t.red())
                        .to_string(),
                );
            }
        }
    }
    suffix
}

#[cfg(test)]
#[path = "progress"]
mod tests {
    #[path = "progress_ansi_and_state_tests.rs"]
    mod ansi_and_state;
    #[path = "progress_test_helpers.rs"]
    mod helpers;
    #[path = "progress_render_progress_tests.rs"]
    mod render_progress;
    #[path = "progress_summary_tests.rs"]
    mod summary;
    #[path = "progress_task_group_tests.rs"]
    mod task_groups;
    #[path = "progress_warnings_tests.rs"]
    mod warnings;
}
