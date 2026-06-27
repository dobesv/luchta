use std::collections::{BTreeMap, BTreeSet};

use luchta_types::TaskId;

pub(crate) fn render_task_id_list(mut all: Vec<&TaskId>) -> String {
    if all.is_empty() {
        return String::new();
    }

    all.sort_by_key(|task_id| task_id.to_string());
    render_running_task_groups(&all)
}

pub(crate) fn render_running_task_groups(shown: &[&TaskId]) -> String {
    let shared_scope = shared_scope_for_tasks(shown);
    let (mut rendered, consumed) = group_by_shared_task_name(shown, shared_scope);
    rendered.extend(group_remaining_by_package(shown, &consumed, shared_scope));
    rendered.join(", ")
}

fn shared_scope_for_tasks<'a>(shown: &[&'a TaskId]) -> Option<&'a str> {
    let packages = shown
        .iter()
        .filter(|task| !task.package.is_root())
        .map(|task| task.package.as_str())
        .collect::<BTreeSet<_>>();
    common_scope(&packages)
}

pub(crate) fn group_by_shared_task_name(
    shown: &[&TaskId],
    shared_scope: Option<&str>,
) -> (Vec<String>, Vec<bool>) {
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

        rendered.push(format!(
            "{}#{}",
            format_package_set(&packages, shared_scope),
            task_name
        ));
        mark_consumed(&mut consumed, &tasks);
    }

    (rendered, consumed)
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

pub(crate) fn group_remaining_by_package(
    shown: &[&TaskId],
    consumed: &[bool],
    shared_scope: Option<&str>,
) -> Vec<String> {
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
        .map(|tasks| render_package_group(tasks, shared_scope))
        .collect()
}

pub(crate) fn render_package_group(mut tasks: Vec<&TaskId>, shared_scope: Option<&str>) -> String {
    tasks.sort_by_key(|task| task.task.to_string());
    if tasks.len() == 1 {
        return render_single_task(tasks[0], shared_scope);
    }

    let names = tasks
        .iter()
        .map(|task| task.task.to_string())
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

fn render_single_task(task: &TaskId, shared_scope: Option<&str>) -> String {
    if task.package.is_root() {
        return task.to_string();
    }

    let package = display_package_name(task.package.as_str(), shared_scope);
    format!("{package}#{}", task.task)
}

fn display_package_name<'a>(package: &'a str, shared_scope: Option<&str>) -> &'a str {
    shared_scope
        .map(|scope| strip_shared_scope(package, scope))
        .unwrap_or(package)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use luchta_types::TaskId;

    use super::{format_package_set, render_running_task_groups};

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
