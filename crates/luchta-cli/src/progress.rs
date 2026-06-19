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

use crate::{
    cli::OutputMode,
    memory_pressure::{PressureReason, PressureSnapshot},
};

/// Outcome of a task as recorded by the progress reporter.
///
/// A successful run (including ordering-only no-command nodes) increments the
/// wave's `done` bucket; a cache hit increments `skipped`. Shared-cache hits
/// also increment a dedicated `shared_hits` counter. Everything else —
/// previous-failure skips, config errors, tasks outside the requested subgraph,
/// and execution failures — is `Uncounted`: removed from the running set but
/// not added to the done or skipped totals.
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
    pub running: Mutex<HashMap<TaskId, Instant>>,
    done: AtomicUsize,
    skipped: AtomicUsize,
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
            wave_total,
            running: Mutex::new(HashMap::new()),
            start: Instant::now(),
            done: AtomicUsize::new(0),
            skipped: AtomicUsize::new(0),
            shared_hits: AtomicUsize::new(0),
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
        self.finish_task(id, TaskOutcome::SkippedLocalCache);
    }

    pub fn task_skipped_shared_cache(&self, id: &TaskId) {
        self.finish_task(id, TaskOutcome::SkippedSharedCache);
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

    pub fn render_progress(
        &self,
        rss_formatted: &str,
        warnings: &[PressureReason],
        pressure: &PressureSnapshot,
    ) -> String {
        let elapsed_total = self.start.elapsed().as_secs();
        let running = self
            .running
            .lock()
            .expect("progress reporter running mutex poisoned");

        let total_tasks: usize = self.wave_total.iter().sum();
        let done = self.done.load(Ordering::SeqCst);
        let skipped = self.skipped.load(Ordering::SeqCst);
        let shared_hits = self.shared_hits.load(Ordering::SeqCst);
        let done_or_skipped = done + skipped;
        let running_count = running.len();
        let pending = total_tasks.saturating_sub(done_or_skipped + running_count);
        let waves_done = self
            .wave_total
            .iter()
            .enumerate()
            .filter(|(wave_index, wave_total)| {
                **wave_total > 0
                    && self.wave_done[*wave_index].load(Ordering::SeqCst)
                        + self.wave_skipped[*wave_index].load(Ordering::SeqCst)
                        == **wave_total
            })
            .count();

        let mut segments = vec![
            format!("☑️ {done_or_skipped}"),
        ];
        if skipped > 0 {
            segments.push(format!("⏭️ {skipped}"));
        }
        if shared_hits > 0 {
            segments.push(format!("📥 {shared_hits}"));
        }
        if pending > 0 {
            segments.push(format!("⌛ {pending}"));
        }
        if running_count > 0 {
            segments.push(format!(
                "🏃 {running_count} ({})",
                render_running_task_list(&running)
            ));
        }
        segments.push(format!("⏱️ {elapsed_total}s"));
        segments.push(format!("🐏 {rss_formatted}"));
        segments.push(format!("🌊 {waves_done} / {}", self.total_waves));

        let mut line = segments.join(" ");
        line.push_str(&pressure_suffix(warnings, pressure));
        line
    }

    pub fn render_summary(&self, rss_formatted: &str) -> String {
        let elapsed_total = self.start.elapsed().as_secs();
        let total_tasks: usize = self.wave_total.iter().sum();
        let done = self.done.load(Ordering::SeqCst);
        let skipped = self.skipped.load(Ordering::SeqCst);
        let shared_hits = self.shared_hits.load(Ordering::SeqCst);
        let done_or_skipped = done + skipped;
        let shared_segment = if shared_hits > 0 {
            format!(" 📥 {shared_hits}")
        } else {
            String::new()
        };

        format!(
            "☑️ {done_or_skipped} ⏭️ {skipped}{shared_segment} ⏱️ {elapsed_total}s 🐏 {rss_formatted} 🌊 {} / {}",
            self.total_waves, self.total_waves
        )
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
}

fn render_running_tasks(running: &HashMap<TaskId, Instant>) -> String {
    if running.is_empty() {
        return String::new();
    }

    let mut all: Vec<&TaskId> = running.keys().collect();
    all.sort_by_key(|task_id| task_id.to_string());
    let total_running = all.len();
    let shown_count = total_running.min(5);
    let shown = &all[..shown_count];
    let inner = render_running_task_groups(shown);

    if total_running > shown_count {
        format!("{} +{}", inner, total_running - shown_count)
    } else {
        inner
    }
}

fn render_running_task_list(running: &HashMap<TaskId, Instant>) -> String {
    render_running_tasks(running)
}

fn render_running_task_groups(shown: &[&TaskId]) -> String {
    let (mut rendered, consumed) = group_by_shared_task_name(shown);
    rendered.extend(group_remaining_by_package(shown, &consumed));
    rendered.join(", ")
}

fn group_by_shared_task_name(shown: &[&TaskId]) -> (Vec<String>, Vec<bool>) {
    let mut tasks_by_name: BTreeMap<&str, Vec<(usize, &TaskId)>> = BTreeMap::new();
    for (index, task) in shown.iter().copied().enumerate() {
        tasks_by_name
            .entry(task.task.as_ref())
            .or_default()
            .push((index, task));
    }

    let mut consumed = vec![false; shown.len()];
    let mut rendered = Vec::new();
    for (task_name, tasks) in tasks_by_name {
        let packages = shared_task_name_packages(&tasks);
        if packages.len() < 2 {
            continue;
        }

        rendered.push(format!("{}:{}", format_package_set(&packages), task_name));
        mark_consumed(&mut consumed, &tasks);
    }

    (rendered, consumed)
}

fn shared_task_name_packages<'a>(
    tasks: &'a [(usize, &'a TaskId)],
) -> std::collections::BTreeSet<&'a str> {
    tasks
        .iter()
        .filter(|(_, task)| !task.package.is_root())
        .map(|(_, task)| task.package.as_str())
        .collect()
}

/// Renders the set of packages sharing a task name. When every package shares a
/// common npm scope (e.g. `@acme/`), the scope is factored out:
/// `@acme/{web,api}` instead of `{@acme/web,@acme/api}`. Otherwise the full
/// package names are listed: `{a,b}`.
fn format_package_set(packages: &std::collections::BTreeSet<&str>) -> String {
    if let Some(scope) = common_scope(packages) {
        let inner = packages
            .iter()
            .map(|package| package.trim_start_matches(scope).trim_start_matches('/'))
            .collect::<Vec<_>>()
            .join(",");
        format!("{scope}/{{{inner}}}", scope = scope, inner = inner)
    } else {
        format!(
            "{{{}}}",
            packages.iter().copied().collect::<Vec<_>>().join(",")
        )
    }
}

/// Returns the npm scope (`@scope`) shared by every package, if any. A package's
/// scope is the segment before its last `/`; only scoped packages (`@`-prefixed)
/// qualify. Returns `None` unless all packages share the same scope.
fn common_scope<'a>(packages: &std::collections::BTreeSet<&'a str>) -> Option<&'a str> {
    let mut scopes = packages.iter().map(|package| scope_of(package));
    let first = scopes.next().flatten()?;
    scopes.all(|scope| scope == Some(first)).then_some(first)
}

/// The npm scope (`@scope`) of a single package: the segment before its last
/// `/`, only for `@`-prefixed packages. `None` otherwise.
fn scope_of(package: &str) -> Option<&str> {
    if !package.starts_with('@') {
        return None;
    }
    package.rsplit_once('/').map(|(scope, _)| scope)
}

fn mark_consumed(consumed: &mut [bool], tasks: &[(usize, &TaskId)]) {
    for (index, task) in tasks {
        if !task.package.is_root() {
            consumed[*index] = true;
        }
    }
}

fn group_remaining_by_package(shown: &[&TaskId], consumed: &[bool]) -> Vec<String> {
    let mut tasks_by_package: BTreeMap<&str, Vec<&TaskId>> = BTreeMap::new();
    for (index, task) in shown.iter().copied().enumerate() {
        if consumed[index] {
            continue;
        }
        tasks_by_package
            .entry(task.package.as_str())
            .or_default()
            .push(task);
    }

    tasks_by_package
        .into_values()
        .map(render_package_group)
        .collect()
}

fn render_package_group(mut tasks: Vec<&TaskId>) -> String {
    tasks.sort_by_key(|task| task.task.to_string());
    if tasks.len() == 1 {
        return tasks[0].to_string();
    }

    let names = tasks
        .iter()
        .map(|task| task.task.to_string())
        .collect::<Vec<_>>()
        .join(",");

    // The synthetic `//root` package id is an internal detail and must never be
    // shown (matching `TaskId`'s Display contract). Render the root group with
    // the `#{...}` config syntax instead of leaking the sentinel package name.
    if tasks[0].package.is_root() {
        format!("#{{{names}}}")
    } else {
        format!("{}:{{{names}}}", tasks[0].package.as_str())
    }
}

fn pressure_suffix(warnings: &[PressureReason], pressure: &PressureSnapshot) -> String {
    let mut suffix = String::new();
    let sample = pressure.sample;
    for warning in warnings {
        match warning {
            PressureReason::UsageHigh => {
                let measured = crate::rss::format_rss(sample.map(|sample| sample.tree_rss));
                let threshold = crate::rss::format_rss(Some(pressure.usage_threshold));
                suffix.push_str(&format!(" ⚠️ mem usage high ({measured} / {threshold})"));
            }
            PressureReason::FreeLow => {
                let measured = crate::rss::format_rss(sample.map(|sample| sample.system_available));
                let threshold = crate::rss::format_rss(Some(pressure.free_threshold));
                suffix.push_str(&format!(
                    " ⚠️ system free memory low ({measured} / {threshold})"
                ));
            }
        }
    }
    suffix
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use luchta_types::TaskId;

    use super::{render_running_task_groups, ProgressReporter};
    use crate::{
        cli::OutputMode,
        memory_pressure::{MemorySample, PressureSnapshot},
        rss,
    };

    struct DoneSummaryExpectation {
        done: usize,
        total: usize,
        skipped: usize,
        waves: usize,
    }

    impl DoneSummaryExpectation {
        fn assert_in(&self, out: &str) {
            assert!(
                out.contains(&format!(
                    "☑️ {}/{} ⏭️ {}",
                    self.done, self.total, self.skipped
                )),
                "expected done summary '☑️ {}/{} ⏭️ {}', got: {out}",
                self.done,
                self.total,
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

    fn pressure_snapshot(
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

    fn assert_progress_line_shape(
        out: &str,
        expected_prefix: &str,
        rss: &str,
        wave_progress: &str,
    ) {
        assert!(
            out.starts_with(expected_prefix),
            "expected prefix '{expected_prefix}', got: {out}"
        );
        assert!(
            out.contains(&format!("🐏 {rss}")),
            "expected RSS '{rss}', got: {out}"
        );
        assert!(
            out.contains(&format!("🌊 {wave_progress}")),
            "expected wave progress '{wave_progress}', got: {out}"
        );
        assert!(
            !out.contains("done ·"),
            "should not contain old 'done ·', got: {out}"
        );
    }

    #[test]
    fn render_progress_shows_zero_skipped_and_pending_when_work_remains() {
        let wave_of = HashMap::from([(task_id("pkg-a", "build"), 0)]);
        let reporter = ProgressReporter::new(OutputMode::Default, wave_of, 1);

        let out = reporter.render_progress("10 MB", &[], &pressure_snapshot(None, 0, 0));
        assert_progress_line_shape(&out, "☑️ 0/1 ⏭️ 0 ⌛ 1 ⏱️ ", "10 MB", "0 / 1");
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

        let out = reporter.render_progress("10 MB", &[], &pressure_snapshot(None, 0, 0));
        assert_progress_line_shape(&out, "☑️ 2/3 ⏭️ 1 🏃1 (pkg-c#build) ⏱️ ", "10 MB", "0 / 1");
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

        let out = reporter.render_progress("10 MB", &[], &pressure_snapshot(None, 0, 0));

        assert!(out.contains("📥 1"), "output was: {out}");
        assert!(out.contains("⏭️ 1"), "output was: {out}");
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
        );
        assert_progress_line_shape(
            &out,
            "☑️ 0/6 ⏭️ 0 🏃6 ({a,b,c}:lint, d:{test,tsc} +1) ⏱️ ",
            "42 MB",
            "0 / 1",
        );
        assert!(!out.contains("running:"));
    }

    #[test]
    fn render_progress_counts_completed_waves_from_done_plus_skipped() {
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
        );
        assert!(out.contains("🌊 1 / 3"));
        assert!(!out.contains("W1 "));
        assert!(!out.contains("done ·"));
    }

    #[test]
    fn render_running_task_groups_issue_example() {
        let tasks = running_tasks(&[
            ("a", "lint"),
            ("b", "lint"),
            ("c", "lint"),
            ("d", "test"),
            ("d", "tsc"),
            ("e", "babel"),
        ]);

        assert_eq!(
            render_running_task_groups(&tasks),
            "{a,b,c}:lint, d:{test,tsc}, e#babel"
        );
    }

    #[test]
    fn render_running_task_groups_all_same_task_across_packages() {
        let tasks = running_tasks(&[("a", "build"), ("b", "build"), ("c", "build")]);

        assert_eq!(render_running_task_groups(&tasks), "{a,b,c}:build");
    }

    #[test]
    fn render_running_task_groups_all_different() {
        let tasks = running_tasks(&[("a", "lint"), ("b", "test"), ("c", "tsc")]);

        assert_eq!(render_running_task_groups(&tasks), "a#lint, b#test, c#tsc");
    }

    #[test]
    fn render_running_task_groups_single_leftover() {
        let tasks = running_tasks(&[("pkg", "task")]);

        assert_eq!(render_running_task_groups(&tasks), "pkg#task");
    }

    #[test]
    fn render_running_task_groups_root_package_never_enters_braces() {
        let tasks = running_tasks(&[("//root", "lint"), ("a", "lint"), ("b", "lint")]);

        assert_eq!(render_running_task_groups(&tasks), "{a,b}:lint, #lint");
    }

    #[test]
    fn render_running_task_groups_root_only_package_groups_normally() {
        let tasks = running_tasks(&[("//root", "build"), ("//root", "test")]);

        // The synthetic `//root` package id must never leak into the output.
        assert_eq!(render_running_task_groups(&tasks), "#{build,test}");
    }

    #[test]
    fn render_running_task_groups_shared_task_with_root_still_groups_non_root_packages() {
        let tasks = running_tasks(&[("//root", "lint"), ("a", "lint"), ("b", "lint")]);

        assert_eq!(render_running_task_groups(&tasks), "{a,b}:lint, #lint");
    }

    #[test]
    fn render_running_task_groups_mixed_shared_and_package_leftovers() {
        let tasks = running_tasks(&[
            ("a", "build"),
            ("b", "build"),
            ("c", "lint"),
            ("c", "test"),
            ("d", "check"),
        ]);

        assert_eq!(
            render_running_task_groups(&tasks),
            "{a,b}:build, c:{lint,test}, d#check"
        );
    }

    #[test]
    fn render_running_task_groups_deterministic_sorting() {
        let tasks = running_tasks(&[("z", "lint"), ("a", "build"), ("m", "build")]);

        assert_eq!(render_running_task_groups(&tasks), "{a,m}:build, z#lint");
    }

    #[test]
    fn render_running_task_groups_shared_scope_is_factored_out() {
        let tasks = running_tasks(&[
            ("@acme/web", "lint"),
            ("@acme/api", "lint"),
            ("@acme/admin", "lint"),
        ]);

        assert_eq!(
            render_running_task_groups(&tasks),
            "@acme/{admin,api,web}:lint"
        );
    }

    #[test]
    fn render_running_task_groups_mixed_scopes_keep_full_names() {
        let tasks = running_tasks(&[("@acme/web", "lint"), ("@other/api", "lint")]);

        assert_eq!(
            render_running_task_groups(&tasks),
            "{@acme/web,@other/api}:lint"
        );
    }

    #[test]
    fn render_running_task_groups_scope_with_unscoped_keeps_full_names() {
        let tasks = running_tasks(&[("@acme/web", "lint"), ("api", "lint")]);

        assert_eq!(render_running_task_groups(&tasks), "{@acme/web,api}:lint");
    }

    #[test]
    fn render_running_task_groups_scoped_single_leftover_uses_display() {
        let tasks = running_tasks(&[("@acme/web", "build"), ("@acme/web", "test")]);

        assert_eq!(render_running_task_groups(&tasks), "@acme/web:{build,test}");
    }

    #[test]
    fn render_summary_omits_skipped_suffix_when_zero() {
        let task = task_id("pkg-a", "build");
        let reporter =
            ProgressReporter::new(OutputMode::Summary, HashMap::from([(task.clone(), 0)]), 1);

        reporter.task_ran(&task);

        let summary = reporter.render_summary("10 MB");

        DoneSummaryExpectation {
            done: 1,
            total: 1,
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

        let summary = reporter.render_summary("10 MB");

        DoneSummaryExpectation {
            done: 2,
            total: 2,
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

        let summary = reporter.render_summary("10 MB");

        assert!(
            summary.contains("☑️ 2/2 ⏭️ 1 📥 1"),
            "summary was: {summary}"
        );
    }

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
        );
        assert!(out.contains("⚠️ mem usage high ("));
        assert!(out.contains("⚠️ system free memory low ("));
        assert!(out.ends_with(&format!(
            "⚠️ system free memory low ({} / {})",
            rss::format_rss(Some(sample.system_available)),
            rss::format_rss(Some(16 * 1024 * 1024))
        )));
    }

    #[test]
    fn render_progress_warnings_none() {
        let reporter = ProgressReporter::new(OutputMode::Default, HashMap::new(), 0);
        let out = reporter.render_progress("10 MB", &[], &pressure_snapshot(None, 0, 0));
        assert!(!out.contains("⚠️"));
    }
    fn running_tasks(tasks: &[(&str, &str)]) -> Vec<&'static TaskId> {
        let leaked = Box::leak(
            tasks
                .iter()
                .map(|(package, task)| task_id(package, task))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );
        leaked.iter().collect()
    }

    fn task_id(package: &str, task: &str) -> TaskId {
        TaskId::new(package, task)
    }
}
