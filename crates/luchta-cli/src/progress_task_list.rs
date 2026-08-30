use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    time::Duration,
};

use luchta_engine::TaskProgress;
use luchta_types::TaskId;

const SLOW_TASK_THRESHOLD: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskListOrder {
    Deterministic,
    OldestFirst,
}

struct RenderedTaskGroup {
    first_position: usize,
    text: String,
}

pub(crate) fn render_task_id_list(mut all: Vec<&TaskId>) -> String {
    if all.is_empty() {
        return String::new();
    }

    all.sort_by_key(|task_id| task_id.to_string());
    render_task_groups(
        &all,
        &HashMap::new(),
        &HashMap::new(),
        TaskListOrder::Deterministic,
    )
}

pub(crate) fn render_running_task_list(
    shown: &[&TaskId],
    progress: &HashMap<TaskId, TaskProgress>,
    elapsed: &HashMap<TaskId, Duration>,
) -> String {
    render_task_groups(shown, progress, elapsed, TaskListOrder::OldestFirst)
}

pub(crate) fn render_task_id_with_status(
    task: &TaskId,
    shared_scope: Option<&str>,
    package_prefix: Option<&str>,
    progress: &HashMap<TaskId, TaskProgress>,
    elapsed: &HashMap<TaskId, Duration>,
) -> String {
    let rendered = render_single_task(task, shared_scope, progress, elapsed);
    package_prefix
        .and_then(|prefix| rendered.strip_prefix(prefix))
        .unwrap_or(&rendered)
        .to_owned()
}

#[cfg(test)]
pub(crate) fn render_running_task_groups(shown: &[&TaskId]) -> String {
    render_running_task_groups_with_progress(shown, &HashMap::new())
}

#[cfg(test)]
pub(crate) fn render_running_task_groups_with_progress(
    shown: &[&TaskId],
    progress: &HashMap<TaskId, TaskProgress>,
) -> String {
    render_task_groups(
        shown,
        progress,
        &HashMap::new(),
        TaskListOrder::Deterministic,
    )
}

fn render_task_groups(
    shown: &[&TaskId],
    progress: &HashMap<TaskId, TaskProgress>,
    elapsed: &HashMap<TaskId, Duration>,
    order: TaskListOrder,
) -> String {
    let shared_scope = shared_scope_for_tasks(shown);
    let (mut rendered, consumed) =
        group_by_shared_task_name_with_progress(shown, shared_scope, progress, elapsed, order);
    rendered.extend(group_remaining_by_package_with_progress(
        shown,
        &consumed,
        shared_scope,
        progress,
        elapsed,
        order,
    ));
    if order == TaskListOrder::OldestFirst {
        rendered.sort_by_key(|group| group.first_position);
    }
    rendered
        .into_iter()
        .map(|group| group.text)
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn shared_scope_for_tasks<'a>(shown: &[&'a TaskId]) -> Option<&'a str> {
    let packages = shown
        .iter()
        .filter(|task| !task.package.is_root())
        .map(|task| task.package.as_str())
        .collect::<BTreeSet<_>>();
    common_scope(&packages)
}

pub(crate) fn shared_package_prefix_for_tasks<'a>(
    shown: &[&'a TaskId],
    shared_scope: Option<&str>,
) -> Option<&'a str> {
    if shown.len() < 2 || shown.iter().any(|task| task.package.is_root()) {
        return None;
    }

    let display_packages = shown
        .iter()
        .map(|task| display_package_name(task.package.as_str(), shared_scope))
        .collect::<Vec<_>>();
    longest_shared_boundary_prefix(&display_packages)
}

fn group_by_shared_task_name_with_progress(
    shown: &[&TaskId],
    shared_scope: Option<&str>,
    progress: &HashMap<TaskId, TaskProgress>,
    elapsed: &HashMap<TaskId, Duration>,
    order: TaskListOrder,
) -> (Vec<RenderedTaskGroup>, Vec<bool>) {
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

        rendered.push(RenderedTaskGroup {
            first_position: tasks.iter().map(|(index, _)| *index).min().unwrap_or(0),
            text: format!(
                "{}#{}",
                format_annotated_package_set(&tasks, shared_scope, progress, elapsed, order),
                task_name
            ),
        });
        mark_consumed(&mut consumed, &tasks);
    }

    (rendered, consumed)
}

fn format_annotated_package_set(
    tasks: &[(usize, &TaskId)],
    shared_scope: Option<&str>,
    progress: &HashMap<TaskId, TaskProgress>,
    elapsed: &HashMap<TaskId, Duration>,
    order: TaskListOrder,
) -> String {
    let task_by_package = tasks
        .iter()
        .filter(|(_, task)| !task.package.is_root())
        .map(|(_, task)| (task.package.as_str(), *task))
        .collect::<BTreeMap<_, _>>();
    let ordered_tasks = match order {
        TaskListOrder::Deterministic => task_by_package.values().copied().collect::<Vec<_>>(),
        TaskListOrder::OldestFirst => tasks
            .iter()
            .filter(|(_, task)| !task.package.is_root())
            .map(|(_, task)| *task)
            .collect(),
    };
    let display_packages = ordered_tasks
        .iter()
        .map(|task| display_package_name(task.package.as_str(), shared_scope))
        .collect::<Vec<_>>();
    let prefix = display_packages
        .len()
        .gt(&1)
        .then(|| longest_shared_boundary_prefix(&display_packages))
        .flatten();
    let members = ordered_tasks
        .into_iter()
        .zip(display_packages)
        .map(|(task, display)| {
            let display = prefix
                .and_then(|prefix| display.strip_prefix(prefix))
                .unwrap_or(display);
            let annotation =
                render_progress_annotation(progress.get(task).copied(), elapsed.get(task).copied())
                    .unwrap_or_default();
            format!("{display}{annotation}")
        })
        .collect::<Vec<_>>()
        .join(",");

    match prefix {
        Some(prefix) => format!("{prefix}{{{members}}}"),
        None => format!("{{{members}}}"),
    }
}

pub(crate) fn shared_task_name_packages<'a>(tasks: &'a [(usize, &'a TaskId)]) -> BTreeSet<&'a str> {
    tasks
        .iter()
        .filter(|(_, task)| !task.package.is_root())
        .map(|(_, task)| task.package.as_str())
        .collect()
}

pub(crate) fn format_package_set(packages: &BTreeSet<&str>, shared_scope: Option<&str>) -> String {
    let display_packages = packages_for_display(packages, shared_scope);
    let prefix = display_packages
        .len()
        .gt(&1)
        .then(|| longest_shared_boundary_prefix(&display_packages))
        .flatten();

    if let Some(prefix) = prefix {
        let suffixes = display_packages
            .iter()
            .map(|package| package.strip_prefix(prefix).unwrap_or(package))
            .collect::<Vec<_>>()
            .join(",");
        return format!("{prefix}{{{suffixes}}}");
    }

    format!("{{{}}}", display_packages.join(","))
}

fn packages_for_display<'a>(
    packages: &BTreeSet<&'a str>,
    shared_scope: Option<&str>,
) -> Vec<&'a str> {
    if let Some(scope) = shared_scope {
        return packages
            .iter()
            .map(|package| strip_shared_scope(package, scope))
            .collect();
    }

    packages.iter().copied().collect()
}

fn strip_shared_scope<'a>(package: &'a str, scope: &str) -> &'a str {
    match package.strip_prefix(scope) {
        Some(rest) => rest.strip_prefix('/').unwrap_or(rest),
        None => package,
    }
}

fn longest_shared_boundary_prefix<'a>(packages: &[&'a str]) -> Option<&'a str> {
    let first = *packages.first()?;
    let max_len = shared_prefix_len(packages);
    separator_boundaries(first, max_len)
        .rev()
        .find_map(|index| {
            let prefix = &first[..index];
            all_suffixes_non_empty(packages, prefix).then_some(prefix)
        })
}

fn shared_prefix_len(packages: &[&str]) -> usize {
    let first = packages[0].as_bytes();
    let mut shared = first.len();

    for package in &packages[1..] {
        shared = shared.min(common_prefix_len(first, package.as_bytes()));
    }

    shared
}

fn common_prefix_len(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right.iter())
        .take_while(|(left, right)| left == right)
        .count()
}

fn separator_boundaries(
    package: &str,
    max_len: usize,
) -> impl DoubleEndedIterator<Item = usize> + '_ {
    package
        .char_indices()
        .filter_map(move |(index, ch)| is_word_separator(ch).then_some(index + ch.len_utf8()))
        .filter(move |index| *index <= max_len)
}

fn all_suffixes_non_empty(packages: &[&str], prefix: &str) -> bool {
    packages.iter().all(|package| {
        package
            .strip_prefix(prefix)
            .is_some_and(|suffix| !suffix.is_empty())
    })
}

fn is_word_separator(ch: char) -> bool {
    matches!(ch, '-' | '/' | '.')
}

/// Returns npm scope (`@scope`) shared by every package, if any. Package's
/// scope is segment before last `/`; only scoped packages (`@`-prefixed)
/// qualify. Returns `None` unless all packages share same scope.
pub(crate) fn common_scope<'a>(packages: &BTreeSet<&'a str>) -> Option<&'a str> {
    let mut scopes = packages.iter().map(|package| scope_of(package));
    let first = scopes.next().flatten()?;
    scopes.all(|scope| scope == Some(first)).then_some(first)
}

/// Npm scope (`@scope`) of single package: segment before last `/`, only for
/// `@`-prefixed packages. `None` otherwise.
pub(crate) fn scope_of(package: &str) -> Option<&str> {
    if !package.starts_with('@') {
        return None;
    }
    package.rsplit_once('/').map(|(scope, _)| scope)
}

pub(crate) fn mark_consumed(consumed: &mut [bool], tasks: &[(usize, &TaskId)]) {
    for (index, task) in tasks {
        if !task.package.is_root() {
            consumed[*index] = true;
        }
    }
}

fn group_remaining_by_package_with_progress(
    shown: &[&TaskId],
    consumed: &[bool],
    shared_scope: Option<&str>,
    progress: &HashMap<TaskId, TaskProgress>,
    elapsed: &HashMap<TaskId, Duration>,
    order: TaskListOrder,
) -> Vec<RenderedTaskGroup> {
    let mut tasks_by_package: BTreeMap<&str, Vec<(usize, &TaskId)>> = BTreeMap::new();
    for (index, task) in shown.iter().copied().enumerate() {
        if consumed[index] {
            continue;
        }
        tasks_by_package
            .entry(task.package.as_str())
            .or_default()
            .push((index, task));
    }

    tasks_by_package
        .into_values()
        .map(|indexed_tasks| RenderedTaskGroup {
            first_position: indexed_tasks.first().map(|(index, _)| *index).unwrap_or(0),
            text: render_package_group_with_progress(
                indexed_tasks.into_iter().map(|(_, task)| task).collect(),
                shared_scope,
                progress,
                elapsed,
                order,
            ),
        })
        .collect()
}

fn render_package_group_with_progress(
    mut tasks: Vec<&TaskId>,
    shared_scope: Option<&str>,
    progress: &HashMap<TaskId, TaskProgress>,
    elapsed: &HashMap<TaskId, Duration>,
    order: TaskListOrder,
) -> String {
    if order == TaskListOrder::Deterministic {
        tasks.sort_by_key(|task| task.task.to_string());
    }
    if tasks.len() == 1 {
        return render_single_task(tasks[0], shared_scope, progress, elapsed);
    }

    let names = tasks
        .iter()
        .map(|task| {
            let annotation = render_progress_annotation(
                progress.get(*task).copied(),
                elapsed.get(*task).copied(),
            )
            .unwrap_or_default();
            format!("{}{annotation}", task.task)
        })
        .collect::<Vec<_>>()
        .join(",");

    // Synthetic `//root` package id is internal detail and must never be shown
    // (matching `TaskId` Display contract). Render root group with `#{...}`
    // config syntax instead of leaking sentinel package name.
    if tasks[0].package.is_root() {
        format!("#{{{names}}}")
    } else {
        let package = display_package_name(tasks[0].package.as_str(), shared_scope);
        format!("{package}#{{{names}}}")
    }
}

fn render_single_task(
    task: &TaskId,
    shared_scope: Option<&str>,
    progress: &HashMap<TaskId, TaskProgress>,
    elapsed: &HashMap<TaskId, Duration>,
) -> String {
    let annotation =
        render_progress_annotation(progress.get(task).copied(), elapsed.get(task).copied())
            .unwrap_or_default();
    if task.package.is_root() {
        return format!("{}{annotation}", task);
    }

    let package = display_package_name(task.package.as_str(), shared_scope);
    format!("{package}#{}{annotation}", task.task)
}

fn render_progress_annotation(
    progress: Option<TaskProgress>,
    elapsed: Option<Duration>,
) -> Option<String> {
    let mut counters = Vec::new();
    if let Some(progress) = progress {
        if progress.completed > 0 {
            counters.push(format!("✔ {}", progress.completed));
        }
        if progress.skipped > 0 {
            counters.push(format!("⏩ {}", progress.skipped));
        }
        if progress.pending > 0 {
            counters.push(format!("⌛ {}", progress.pending));
        }
        if progress.running > 0 {
            counters.push(format!("🏃 {}", progress.running));
        }
    }
    if let Some(elapsed) = elapsed.filter(|elapsed| *elapsed > SLOW_TASK_THRESHOLD) {
        counters.push(format!("⌚ {}s", elapsed.as_secs()));
    }
    (!counters.is_empty()).then(|| format!("({})", counters.join(" ")))
}

fn display_package_name<'a>(package: &'a str, shared_scope: Option<&str>) -> &'a str {
    shared_scope
        .map(|scope| strip_shared_scope(package, scope))
        .unwrap_or(package)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeSet, HashMap},
        time::Duration,
    };

    use luchta_engine::TaskProgress;
    use luchta_types::TaskId;

    use super::{
        format_package_set, render_running_task_groups, render_running_task_groups_with_progress,
        render_running_task_list,
    };

    #[test]
    fn render_running_task_groups_examples() {
        assert_rendered_groups(
            &[
                task_ref("a", "lint"),
                task_ref("b", "lint"),
                task_ref("c", "lint"),
                task_ref("d", "test"),
                task_ref("d", "tsc"),
                task_ref("e", "babel"),
            ],
            "{a,b,c}#lint, d#{test,tsc}, e#babel",
        );
    }

    #[test]
    fn render_running_task_groups_basic_examples() {
        assert_rendered_groups(
            &[task_ref("a", "lint"), task_ref("b", "lint")],
            "{a,b}#lint",
        );
        assert_rendered_groups(
            &[task_ref("pkg", "build"), task_ref("pkg", "test")],
            "pkg#{build,test}",
        );
        assert_rendered_groups(
            &[
                task_ref("a", "lint"),
                task_ref("b", "test"),
                task_ref("c", "tsc"),
            ],
            "a#lint, b#test, c#tsc",
        );
        assert_rendered_groups(&[task_ref("pkg", "task")], "pkg#task");
    }

    #[test]
    fn worker_progress_annotates_standalone_and_suppresses_zero_counters() {
        let task = task_id("pkg", "build");
        let shown = vec![&task];
        let progress = HashMap::from([(
            task.clone(),
            TaskProgress {
                completed: 5,
                skipped: 1,
                running: 2,
                pending: 8,
            },
        )]);

        assert_eq!(
            render_running_task_groups_with_progress(&shown, &progress),
            "pkg#build(✔ 5 ⏩ 1 ⌛ 8 🏃 2)"
        );

        let progress = HashMap::from([(
            task.clone(),
            TaskProgress {
                running: 2,
                ..TaskProgress::default()
            },
        )]);
        assert_eq!(
            render_running_task_groups_with_progress(&shown, &progress),
            "pkg#build(🏃 2)"
        );
        assert_eq!(
            render_running_task_groups_with_progress(
                &shown,
                &HashMap::from([(task.clone(), TaskProgress::default())])
            ),
            "pkg#build"
        );
    }

    #[test]
    fn worker_progress_preserves_both_grouping_directions_and_mixed_members() {
        let auth_test = task_id("auth", "test");
        let main_test = task_id("main", "test");
        let shared = vec![&main_test, &auth_test];
        let shared_progress = HashMap::from([(
            auth_test.clone(),
            TaskProgress {
                completed: 5,
                skipped: 1,
                running: 1,
                pending: 2,
            },
        )]);
        assert_eq!(
            render_running_task_groups_with_progress(&shared, &shared_progress),
            "{auth(✔ 5 ⏩ 1 ⌛ 2 🏃 1),main}#test"
        );

        let lint = task_id("auth", "lint");
        let package = vec![&auth_test, &lint];
        let package_progress = HashMap::from([
            (
                lint.clone(),
                TaskProgress {
                    completed: 50,
                    running: 16,
                    pending: 100,
                    ..TaskProgress::default()
                },
            ),
            (
                auth_test.clone(),
                TaskProgress {
                    pending: 2,
                    ..TaskProgress::default()
                },
            ),
        ]);
        assert_eq!(
            render_running_task_groups_with_progress(&package, &package_progress),
            "auth#{lint(✔ 50 ⌛ 100 🏃 16),test(⌛ 2)}"
        );
    }

    #[test]
    fn annotated_group_order_is_deterministic() {
        let a = task_id("a", "build");
        let z = task_id("z", "build");
        let shown = vec![&z, &a];
        let progress = HashMap::from([
            (
                z.clone(),
                TaskProgress {
                    pending: 1,
                    ..TaskProgress::default()
                },
            ),
            (
                a.clone(),
                TaskProgress {
                    completed: 1,
                    ..TaskProgress::default()
                },
            ),
        ]);
        assert_eq!(
            render_running_task_groups_with_progress(&shown, &progress),
            "{a(✔ 1),z(⌛ 1)}#build"
        );
    }

    #[test]
    fn slow_task_elapsed_time_uses_progress_parentheses_and_strict_threshold() {
        let task = task_id("pkg", "build");
        let shown = vec![&task];
        let progress = HashMap::from([(
            task.clone(),
            TaskProgress {
                completed: 5,
                pending: 2,
                ..TaskProgress::default()
            },
        )]);

        assert_eq!(
            render_running_task_list(
                &shown,
                &progress,
                &HashMap::from([(task.clone(), Duration::from_secs(5))]),
            ),
            "pkg#build(✔ 5 ⌛ 2)"
        );
        assert_eq!(
            render_running_task_list(
                &shown,
                &progress,
                &HashMap::from([(task.clone(), Duration::from_secs(6))]),
            ),
            "pkg#build(✔ 5 ⌛ 2 ⌚ 6s)"
        );
    }

    #[test]
    fn age_order_places_groups_by_oldest_member_and_members_oldest_first() {
        let oldest_build = task_id("z", "build");
        let middle_lint = task_id("m", "lint");
        let newest_build = task_id("a", "build");
        let shown = vec![&oldest_build, &middle_lint, &newest_build];
        let elapsed = HashMap::from([
            (oldest_build.clone(), Duration::from_secs(12)),
            (middle_lint.clone(), Duration::from_secs(10)),
            (newest_build.clone(), Duration::from_secs(8)),
        ]);

        assert_eq!(
            render_running_task_list(&shown, &HashMap::new(), &elapsed),
            "{z(⌚ 12s),a(⌚ 8s)}#build, m#lint(⌚ 10s)"
        );
    }

    #[test]
    fn render_running_task_groups_root_package_examples() {
        assert_rendered_groups(
            &[
                task_ref("//root", "lint"),
                task_ref("a", "lint"),
                task_ref("b", "lint"),
            ],
            "{a,b}#lint, #lint",
        );
        assert_rendered_groups(
            &[task_ref("//root", "build"), task_ref("//root", "test")],
            "#{build,test}",
        );
        assert_rendered_groups(
            &[
                task_ref("a", "build"),
                task_ref("b", "build"),
                task_ref("c", "lint"),
                task_ref("c", "test"),
                task_ref("d", "check"),
            ],
            "{a,b}#build, c#{lint,test}, d#check",
        );
        assert_rendered_groups(
            &[
                task_ref("z", "lint"),
                task_ref("a", "build"),
                task_ref("m", "build"),
            ],
            "{a,m}#build, z#lint",
        );
    }

    #[test]
    fn render_running_task_groups_scoped_package_examples() {
        assert_rendered_groups(
            &[
                task_ref("@acme/web", "lint"),
                task_ref("@acme/api", "lint"),
                task_ref("@acme/admin", "lint"),
            ],
            "{admin,api,web}#lint",
        );
        assert_rendered_groups(
            &[
                task_ref("@acme/web", "lint"),
                task_ref("@other/api", "lint"),
            ],
            "{@acme/web,@other/api}#lint",
        );
        assert_rendered_groups(
            &[task_ref("@acme/web", "lint"), task_ref("api", "lint")],
            "{@acme/web,api}#lint",
        );
        assert_rendered_groups(
            &[
                task_ref("@acme/web", "build"),
                task_ref("@acme/web", "test"),
            ],
            "web#{build,test}",
        );
    }

    #[test]
    fn render_running_task_groups_global_scope_handling_examples() {
        for (tasks, expected) in [
            (
                vec![
                    task_ref("@acme/a", "lint"),
                    task_ref("@acme/b", "lint"),
                    task_ref("@acme/c", "build"),
                    task_ref("@acme/c", "test"),
                ],
                "{a,b}#lint, c#{build,test}",
            ),
            (
                vec![
                    task_ref("@acme/web", "build"),
                    task_ref("@acme/api", "lint"),
                    task_ref("@acme/api", "test"),
                ],
                "api#{lint,test}, web#build",
            ),
            (
                vec![task_ref("@acme/a", "lint"), task_ref("@other/b", "build")],
                "@acme/a#lint, @other/b#build",
            ),
        ] {
            assert_rendered_groups(&tasks, expected);
        }
    }

    #[test]
    fn format_package_set_compacts_word_boundary_prefix() {
        assert_eq!(
            format_packages(&[
                "@formative/server-answers",
                "@formative/server-changes",
                "@formative/server-enrollments",
                "@formative/server-export",
                "@formative/server-folders",
            ]),
            "server-{answers,changes,enrollments,export,folders}"
        );
    }

    #[test]
    fn format_package_set_omits_common_scope_without_extra_prefix() {
        assert_eq!(
            format_packages(&["@acme/admin", "@acme/api", "@acme/web"]),
            "{admin,api,web}"
        );
    }

    #[test]
    fn format_package_set_repeated_prefix_keeps_literal_prefix_once() {
        assert_eq!(
            format_packages(&["@scope/server-server-a", "@scope/server-server-b"]),
            "server-server-{a,b}"
        );
    }

    #[test]
    fn format_package_set_rejects_prefix_that_would_leave_empty_suffix() {
        assert_eq!(format_packages(&["pkga-", "pkga-api"]), "{pkga-,pkga-api}");
    }

    #[test]
    fn format_package_set_compacts_utf8_prefix_safely() {
        assert_eq!(
            format_packages(&["@scope/café-a", "@scope/café-b"]),
            "café-{a,b}"
        );
    }

    #[test]
    fn format_package_set_scope_omission_contract_and_mixed_scope_contrast() {
        assert_eq!(
            format_packages(&["@acme/admin", "@acme/api", "@acme/web"]),
            "{admin,api,web}"
        );
        assert_eq!(
            format_packages(&["@acme/admin", "@other/api", "@acme/web"]),
            "{@acme/admin,@acme/web,@other/api}"
        );
    }

    fn assert_rendered_groups(tasks: &[TaskRef<'_>], expected: &str) {
        let tasks = running_tasks(tasks);
        assert_eq!(render_running_task_groups(&tasks), expected);
    }

    fn format_packages<'a>(packages: &'a [&'a str]) -> String {
        let packages = packages.iter().copied().collect::<BTreeSet<_>>();
        let shared_scope = super::common_scope(&packages);
        format_package_set(&packages, shared_scope)
    }

    struct TaskRef<'a> {
        package: &'a str,
        task: &'a str,
    }

    const fn task_ref<'a>(package: &'a str, task: &'a str) -> TaskRef<'a> {
        TaskRef { package, task }
    }

    fn running_tasks(tasks: &[TaskRef<'_>]) -> Vec<&'static TaskId> {
        let leaked = Box::leak(
            tasks
                .iter()
                .map(|task| task_id(task.package, task.task))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );
        leaked.iter().collect()
    }

    fn task_id(package: &str, task: &str) -> TaskId {
        TaskId::new(package, task)
    }
}
