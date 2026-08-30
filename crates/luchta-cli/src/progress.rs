use std::{
    collections::HashMap,
    collections::HashSet,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

use luchta_engine::{ExecutionLogSink, TaskProgress};
use luchta_types::TaskId;
use owo_colors::{OwoColorize, Stream};

use crate::{
    cli::OutputMode,
    memory_pressure::{PressureReason, PressureSnapshot},
    progress_task_list::render_task_id_list,
};

mod console_output;
mod status_line;

pub(crate) use console_output::ProgressOutput;
use status_line::{render_status_line, StatusLineCounts, StatusLineInput};

#[cfg(test)]
use console_output::{truncate_ansi, visible_width, InteractiveStatusState};

/// Outcome of a task as recorded by the progress reporter.
///
/// A successful run increments the wave's `done` bucket, a local-cache hit
/// increments `skipped`, and a shared-cache hit increments `shared_hits`.
/// Everything else — ordering-only no-worker nodes, previous-failure skips,
/// config errors, tasks outside the requested subgraph, and execution failures
/// — is `Uncounted`: removed from the running set but not added to a successful
/// completion bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskOutcome {
    /// Task executed successfully (increments the wave's done count).
    Ran,
    /// Task skipped due to a local cache hit.
    SkippedLocalCache,
    /// Task satisfied by a shared cache hit.
    SharedCacheHit,
    /// Outcome that contributes to neither the done nor the skipped totals.
    Uncounted,
}

#[derive(Debug)]
pub struct ProgressReporter {
    pub wave_of: HashMap<TaskId, usize>,
    pub wave_done: Vec<AtomicUsize>,
    pub wave_skipped: Vec<AtomicUsize>,
    pub wave_shared_hits: Vec<AtomicUsize>,
    pub wave_failed: Vec<AtomicUsize>,
    pub running: Mutex<HashMap<TaskId, Instant>>,
    progress_sinks: Mutex<HashMap<TaskId, ExecutionLogSink>>,
    pub failed_tasks: Mutex<HashSet<TaskId>>,
    done: AtomicUsize,
    skipped: AtomicUsize,
    failed: AtomicUsize,
    shared_hits: AtomicUsize,
    pub mode: OutputMode,
    pub total_waves: usize,
    pub wave_total: Vec<usize>,
    pub start: Instant,
    output: ProgressOutput,
}

pub(crate) struct ProgressRenderContext<'a> {
    pub(crate) rss_formatted: &'a str,
    pub(crate) warnings: &'a [PressureReason],
    pub(crate) pressure: &'a PressureSnapshot,
    pub(crate) stream: Stream,
    pub(crate) max_width: Option<usize>,
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
            wave_shared_hits: (0..total_waves).map(|_| AtomicUsize::new(0)).collect(),
            wave_failed: (0..total_waves).map(|_| AtomicUsize::new(0)).collect(),
            wave_total,
            running: Mutex::new(HashMap::new()),
            progress_sinks: Mutex::new(HashMap::new()),
            failed_tasks: Mutex::new(HashSet::new()),
            start: Instant::now(),
            done: AtomicUsize::new(0),
            skipped: AtomicUsize::new(0),
            failed: AtomicUsize::new(0),
            shared_hits: AtomicUsize::new(0),
            output: ProgressOutput::detect(mode),
        }
    }

    pub(crate) fn output(&self) -> ProgressOutput {
        self.output.clone()
    }

    pub(crate) fn uses_live_status(&self) -> bool {
        self.output.is_live()
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

    pub fn task_started_with_progress(&self, id: &TaskId, sink: ExecutionLogSink) {
        self.task_started(id);
        if !self.wave_of.contains_key(id) {
            return;
        }
        self.progress_sinks
            .lock()
            .expect("progress reporter sink mutex poisoned")
            .insert(id.clone(), sink);
    }

    pub(crate) fn start_callback(
        self: &Arc<Self>,
        id: TaskId,
        sink: ExecutionLogSink,
    ) -> impl FnOnce() + Send + 'static {
        let reporter = Arc::clone(self);
        move || reporter.task_started_with_progress(&id, sink)
    }

    pub fn task_ran(&self, id: &TaskId) {
        self.finish_task(id, TaskOutcome::Ran);
    }

    pub fn task_skipped_cache_hit(&self, id: &TaskId) {
        self.finish_task(id, TaskOutcome::SkippedLocalCache);
    }

    pub fn task_shared_cache_hit(&self, id: &TaskId) {
        self.finish_task(id, TaskOutcome::SharedCacheHit);
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
        self.remove_progress_sink(id);

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

    #[cfg(test)]
    pub fn render_progress(
        &self,
        rss_formatted: &str,
        warnings: &[PressureReason],
        pressure: &PressureSnapshot,
        stream: Stream,
    ) -> String {
        self.render_progress_for_width(ProgressRenderContext {
            rss_formatted,
            warnings,
            pressure,
            stream,
            max_width: None,
        })
    }

    pub(crate) fn render_progress_for_width(&self, context: ProgressRenderContext<'_>) -> String {
        let (counts, running_tasks) = {
            let running = self
                .running
                .lock()
                .expect("progress reporter running mutex poisoned");
            (
                self.progress_counts(&running),
                running.keys().cloned().collect::<Vec<_>>(),
            )
        };
        let task_progress = self.worker_progress();
        let segment_counts = StatusLineCounts {
            completed: counts.completed,
            skipped: counts.skipped,
            shared_hits: counts.shared_hits,
            pending: counts.pending,
            running: counts.running_count,
            elapsed_total: counts.elapsed_total,
            waves_done: counts.waves_done,
            total_waves: self.total_waves,
        };
        let failed_segment = self.failed_segment(context.stream);
        let warning_suffix = pressure_suffix(context.warnings, context.pressure, context.stream);
        let running_task_refs = running_tasks.iter().collect();
        render_status_line(StatusLineInput {
            stream: context.stream,
            running_tasks: running_task_refs,
            task_progress: &task_progress,
            failed_segment: failed_segment.as_deref(),
            counts: segment_counts,
            rss_formatted: context.rss_formatted,
            warning_suffix: &warning_suffix,
            max_width: context.max_width,
        })
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
        let completed = done + skipped + shared_hits;
        let done_str = format!("✔ {completed}")
            .if_supports_color(stream, |t| t.green())
            .to_string();

        let skipped_segment = if skipped > 0 {
            format!(" ⏩ {skipped}")
                .if_supports_color(stream, |t| t.cyan())
                .to_string()
        } else {
            String::new()
        };

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

        format!("{done_str}{skipped_segment}{failed_segment}{shared_segment} {elapsed_str} {rss_str} {waves_str}{cancelled_segment}")
    }

    fn finish_task(&self, id: &TaskId, kind: TaskOutcome) {
        let mut running = self
            .running
            .lock()
            .expect("progress reporter running mutex poisoned");
        running.remove(id);
        drop(running);
        self.remove_progress_sink(id);

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
            TaskOutcome::SharedCacheHit => {
                self.wave_shared_hits[wave_index].fetch_add(1, Ordering::SeqCst);
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

    fn remove_progress_sink(&self, id: &TaskId) {
        if let Some(sink) = self
            .progress_sinks
            .lock()
            .expect("progress reporter sink mutex poisoned")
            .remove(id)
        {
            sink.clear_progress();
        }
    }

    fn worker_progress(&self) -> HashMap<TaskId, TaskProgress> {
        self.progress_sinks
            .lock()
            .expect("progress reporter sink mutex poisoned")
            .iter()
            .filter_map(|(id, sink)| sink.progress().map(|progress| (id.clone(), progress)))
            .collect()
    }

    fn progress_counts(&self, running: &HashMap<TaskId, Instant>) -> ProgressCounts {
        let total_tasks: usize = self.wave_total.iter().sum();
        let done = self.done.load(Ordering::SeqCst);
        let skipped = self.skipped.load(Ordering::SeqCst);
        let failed = self.failed.load(Ordering::SeqCst);
        let shared_hits = self.shared_hits.load(Ordering::SeqCst);
        let completed = done + skipped + shared_hits;
        let running_count = running.len();
        let pending = total_tasks.saturating_sub(completed + running_count + failed);

        ProgressCounts {
            completed,
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
                        + self.wave_shared_hits[*wave_index].load(Ordering::SeqCst)
                        + self.wave_failed[*wave_index].load(Ordering::SeqCst)
                        == **wave_total
            })
            .count()
    }
}

struct ProgressCounts {
    completed: usize,
    skipped: usize,
    shared_hits: usize,
    running_count: usize,
    pending: usize,
    elapsed_total: u64,
    waves_done: usize,
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
