use std::{
    collections::BTreeMap,
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
    time::Instant,
};

use luchta_types::TaskId;

use crate::cli::OutputMode;

/// Outcome of a task as recorded by the progress reporter.
///
/// A successful run (including ordering-only no-command nodes) increments the
/// wave's `done` bucket; a cache hit increments `skipped` (the only thing
/// counted as "skipped"). Everything else — previous-failure skips,
/// config errors, tasks outside the requested subgraph, and execution
/// failures — is `Uncounted`: removed from the running set but not added to the
/// done or skipped totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskOutcome {
    /// Task executed successfully (increments the wave's done count).
    Ran,
    /// Task skipped due to a cache hit (the ONLY "skipped" counted in progress).
    SkippedCacheHit,
    /// Outcome that contributes to neither the done nor the skipped totals.
    Uncounted,
}

#[derive(Debug)]
pub struct ProgressReporter {
    pub wave_of: HashMap<TaskId, usize>,
    pub wave_done: Vec<AtomicUsize>,
    pub wave_skipped: Vec<AtomicUsize>,
    pub running: Mutex<HashMap<TaskId, Instant>>,
    done: AtomicUsize,
    skipped: AtomicUsize,
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
            wave_total,
            running: Mutex::new(HashMap::new()),
            start: Instant::now(),
            done: AtomicUsize::new(0),
            skipped: AtomicUsize::new(0),
        }
    }

    pub fn task_started(&self, id: &TaskId) {
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
        self.finish_task(id, TaskOutcome::SkippedCacheHit);
    }

    pub fn task_finished_other(&self, id: &TaskId) {
        self.finish_task(id, TaskOutcome::Uncounted);
    }

    pub fn running_count(&self) -> usize {
        let running = self
            .running
            .lock()
            .expect("progress reporter running mutex poisoned");
        running.len()
    }

    pub fn render_progress(&self, rss_formatted: &str) -> String {
        let elapsed_total = self.start.elapsed().as_secs();
        let running = self
            .running
            .lock()
            .expect("progress reporter running mutex poisoned");

        let total_tasks: usize = self.wave_total.iter().sum();
        let done = self.done.load(Ordering::SeqCst);
        let skipped = self.skipped.load(Ordering::SeqCst);
        let running_count = running.len();
        let pending = total_tasks.saturating_sub(done + skipped + running_count);

        let mut agg_parts = format!("{}/{} done", done, total_tasks);
        if skipped > 0 {
            agg_parts.push_str(&format!(" · {} skipped", skipped));
        }
        agg_parts.push_str(&format!(" · {} running", running_count));
        if pending > 0 {
            agg_parts.push_str(&format!(" · {} pending", pending));
        }

        let mut running_by_wave: Vec<Vec<&TaskId>> = vec![Vec::new(); self.total_waves];
        for task_id in running.keys() {
            if let Some(&wave_index) = self.wave_of.get(task_id) {
                if let Some(tasks) = running_by_wave.get_mut(wave_index) {
                    tasks.push(task_id);
                }
            }
        }

        let mut active_waves = Vec::new();
        for (wave_index, wave_running) in running_by_wave.iter().enumerate() {
            if !wave_running.is_empty() {
                let wave_done = self.wave_done[wave_index].load(Ordering::SeqCst);
                let wave_total = self.wave_total[wave_index];
                active_waves.push(format!("W{} {}/{}", wave_index + 1, wave_done, wave_total));
                if active_waves.len() == 3 {
                    break;
                }
            }
        }

        let running_str = if running.is_empty() {
            String::new()
        } else {
            let mut running_tasks_list: Vec<&TaskId> = running.keys().collect();
            running_tasks_list.sort_by_key(|a| a.to_string());

            let total_running = running_tasks_list.len();
            let shown_count = std::cmp::min(5, total_running);
            let shown = &running_tasks_list[..shown_count];

            fn get_scope(pkg: &str) -> Option<&str> {
                if pkg.starts_with('@') {
                    if let Some(slash_idx) = pkg.find('/') {
                        return Some(&pkg[..=slash_idx]);
                    }
                }
                None
            }

            let mut uniform_scope = true;
            let first_scope = if running_tasks_list[0].is_root() {
                None
            } else {
                get_scope(running_tasks_list[0].package.as_str())
            };
            if first_scope.is_none() {
                uniform_scope = false;
            } else {
                for t in &running_tasks_list[1..] {
                    let scope = if t.is_root() {
                        None
                    } else {
                        get_scope(t.package.as_str())
                    };
                    if scope != first_scope {
                        uniform_scope = false;
                        break;
                    }
                }
            }

            let inner_str = if uniform_scope {
                let prefix = first_scope.unwrap();
                shown
                    .iter()
                    .map(|t| {
                        format!(
                            "{}#{}",
                            t.package.as_str().strip_prefix(prefix).unwrap(),
                            t.task
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                let mut tasks_by_scope: BTreeMap<Option<&str>, Vec<&TaskId>> = BTreeMap::new();
                for t in shown {
                    let scope = if t.is_root() {
                        None
                    } else {
                        get_scope(t.package.as_str())
                    };
                    tasks_by_scope.entry(scope).or_default().push(*t);
                }

                // Render scoped groups first (sorted by scope), then any
                // unscoped tasks last — reads more naturally than the BTreeMap's
                // natural ordering, which would place `None` (unscoped) first.
                let mut scope_parts = Vec::new();
                let mut unscoped_part = None;
                for (scope, tasks) in tasks_by_scope {
                    if let Some(s) = scope {
                        if tasks.len() == 1 {
                            scope_parts.push(tasks[0].to_string());
                        } else {
                            let inner = tasks
                                .iter()
                                .map(|t| {
                                    format!(
                                        "{}#{}",
                                        t.package.as_str().strip_prefix(s).unwrap(),
                                        t.task
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join(", ");
                            scope_parts.push(format!("{}{{{}}}", s, inner));
                        }
                    } else {
                        unscoped_part = Some(
                            tasks
                                .iter()
                                .map(|t| t.to_string())
                                .collect::<Vec<_>>()
                                .join(", "),
                        );
                    }
                }
                scope_parts.extend(unscoped_part);
                scope_parts.join(" · ")
            };

            if total_running > shown_count {
                format!(" · running: {} +{}", inner_str, total_running - shown_count)
            } else {
                format!(" · running: {}", inner_str)
            }
        };

        let active_waves_str = if active_waves.is_empty() {
            String::new()
        } else {
            format!(" · {}", active_waves.join(" · "))
        };

        format!(
            "{}{}{} · {}s · RSS {}",
            agg_parts, active_waves_str, running_str, elapsed_total, rss_formatted
        )
    }

    // Consumed by final summary print (task 79423739).
    pub fn render_summary(&self) -> String {
        let done = self.done.load(Ordering::SeqCst);
        let skipped = self.skipped.load(Ordering::SeqCst);
        let elapsed_total = self.start.elapsed().as_secs();

        let mut summary = format!("Done: {} tasks done after {} seconds.", done, elapsed_total);
        if skipped > 0 {
            summary.push_str(&format!(", {} skipped", skipped));
        }
        summary
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
            TaskOutcome::SkippedCacheHit => {
                self.wave_skipped[wave_index].fetch_add(1, Ordering::SeqCst);
                self.skipped.fetch_add(1, Ordering::SeqCst);
            }
            // Uncounted outcomes (prev-failure, config-error, not-in-subgraph,
            // execution failure): removed from the running set above, but not
            // added to done or skipped.
            TaskOutcome::Uncounted => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, time::Instant};

    use luchta_types::TaskId;

    use super::ProgressReporter;
    use crate::cli::OutputMode;

    #[test]
    fn test_render_progress_aggregate_counts() {
        let wave_of = HashMap::from([(task_id("pkg-a", "build"), 0)]);
        let reporter = ProgressReporter::new(OutputMode::Default, wave_of, 1);

        let out_zero = reporter.render_progress("10 MB");
        assert!(out_zero.starts_with("0/1 done · 0 running"));
        assert!(!out_zero.contains("skipped"));
        assert!(out_zero.contains("1 pending"));

        reporter.task_skipped_cache_hit(&task_id("pkg-a", "build"));
        let out_skipped = reporter.render_progress("10 MB");
        assert!(out_skipped.starts_with("0/1 done · 1 skipped · 0 running"));
        assert!(!out_skipped.contains("pending"));
    }

    #[test]
    fn test_render_progress_frontier_waves_and_rss() {
        let wave_of = HashMap::from([
            (task_id("pkg-a", "build"), 0),
            (task_id("pkg-b", "build"), 1),
            (task_id("pkg-c", "build"), 2),
            (task_id("pkg-d", "build"), 3),
        ]);
        let reporter = ProgressReporter::new(OutputMode::Default, wave_of, 4);

        reporter
            .running
            .lock()
            .unwrap()
            .insert(task_id("pkg-b", "build"), Instant::now());
        reporter
            .running
            .lock()
            .unwrap()
            .insert(task_id("pkg-c", "build"), Instant::now());
        reporter
            .running
            .lock()
            .unwrap()
            .insert(task_id("pkg-d", "build"), Instant::now());

        let out = reporter.render_progress("42 MB");
        assert!(!out.contains("W1")); // no running tasks
        assert!(out.contains("W2 0/1"));
        assert!(out.contains("W3 0/1"));
        assert!(out.contains("W4 0/1"));
        assert!(out.contains("RSS 42 MB"));
    }

    #[test]
    fn test_render_progress_scope_uniform_stripping() {
        let wave_of = HashMap::from([
            (task_id("@acme/a", "build"), 0),
            (task_id("@acme/b", "build"), 0),
        ]);
        let reporter = ProgressReporter::new(OutputMode::Default, wave_of, 1);
        reporter
            .running
            .lock()
            .unwrap()
            .insert(task_id("@acme/a", "build"), Instant::now());
        reporter
            .running
            .lock()
            .unwrap()
            .insert(task_id("@acme/b", "build"), Instant::now());

        let out = reporter.render_progress("10 MB");
        // Strips @acme/
        assert!(out.contains("running: a#build, b#build"));
        assert!(!out.contains("@acme"));
    }

    #[test]
    fn test_render_progress_scope_mixed_grouping() {
        let wave_of = HashMap::from([
            (task_id("@acme/a", "build"), 0),
            (task_id("@formative/b", "build"), 0),
            (task_id("@formative/c", "build"), 0),
            (task_id("unscoped", "build"), 0),
        ]);
        let reporter = ProgressReporter::new(OutputMode::Default, wave_of, 1);
        reporter
            .running
            .lock()
            .unwrap()
            .insert(task_id("@acme/a", "build"), Instant::now());
        reporter
            .running
            .lock()
            .unwrap()
            .insert(task_id("@formative/b", "build"), Instant::now());
        reporter
            .running
            .lock()
            .unwrap()
            .insert(task_id("@formative/c", "build"), Instant::now());
        reporter
            .running
            .lock()
            .unwrap()
            .insert(task_id("unscoped", "build"), Instant::now());

        let out = reporter.render_progress("10 MB");
        assert!(
            out.contains("running: @acme/a#build · @formative/{b#build, c#build} · unscoped#build")
        );
    }

    #[test]
    fn test_render_progress_cap_running() {
        let mut wave_of = HashMap::new();
        for i in 0..10 {
            wave_of.insert(task_id("unscoped", &format!("build{}", i)), 0);
        }
        let reporter = ProgressReporter::new(OutputMode::Default, wave_of, 1);
        for i in 0..10 {
            reporter
                .running
                .lock()
                .unwrap()
                .insert(task_id("unscoped", &format!("build{}", i)), Instant::now());
        }

        let out = reporter.render_progress("10 MB");
        // Should show 5 items and +5
        assert!(out.contains(" +5"));
        assert_eq!(out.matches("unscoped#build").count(), 5);
    }

    #[test]
    fn render_summary_omits_skipped_suffix_when_zero() {
        let task_a = task_id("pkg-a", "build");
        let task_b = task_id("pkg-b", "build");
        let reporter = ProgressReporter::new(
            OutputMode::Summary,
            HashMap::from([(task_a.clone(), 0), (task_b.clone(), 0)]),
            1,
        );

        reporter.task_ran(&task_a);
        reporter.task_ran(&task_b);

        let summary = reporter.render_summary();

        assert!(summary.starts_with("Done: 2 tasks done after "));
        assert!(!summary.contains("skipped"));
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

        let summary = reporter.render_summary();

        assert!(summary.starts_with("Done: 1 tasks done after "));
        assert!(summary.contains(", 1 skipped"));
    }

    fn task_id(package: &str, task: &str) -> TaskId {
        TaskId::new(package, task)
    }
}
