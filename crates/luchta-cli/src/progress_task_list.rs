use std::collections::{BTreeMap, BTreeSet};

use luchta_types::TaskId;

pub(crate) fn render_task_id_list(mut all: Vec<&TaskId>) -> String {
    if all.is_empty() {
        return String::new();
    }

    all.sort_by_key(|task_id| task_id.to_string());
    let total = all.len();
    let shown_count = total.min(5);
    let shown = &all[..shown_count];
    let inner = render_running_task_groups(shown);

    if total > shown_count {
        format!("{} +{}", inner, total - shown_count)
    } else {
        inner
    }
}

pub(crate) fn render_running_task_groups(shown: &[&TaskId]) -> String {
    let (mut rendered, consumed) = group_by_shared_task_name(shown);
    rendered.extend(group_remaining_by_package(shown, &consumed));
    rendered.join(", ")
}

pub(crate) fn group_by_shared_task_name(shown: &[&TaskId]) -> (Vec<String>, Vec<bool>) {
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

pub(crate) fn shared_task_name_packages<'a>(tasks: &'a [(usize, &'a TaskId)]) -> BTreeSet<&'a str> {
    tasks
        .iter()
        .filter(|(_, task)| !task.package.is_root())
        .map(|(_, task)| task.package.as_str())
        .collect()
}

/// Renders set of packages sharing task name. When every package shares a
/// common npm scope (e.g. `@acme/`), scope is factored out:
/// `@acme/{web,api}` instead of `{@acme/web,@acme/api}`. Otherwise full
/// package names are listed: `{a,b}`.
pub(crate) fn format_package_set(packages: &BTreeSet<&str>) -> String {
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

pub(crate) fn group_remaining_by_package(shown: &[&TaskId], consumed: &[bool]) -> Vec<String> {
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

pub(crate) fn render_package_group(mut tasks: Vec<&TaskId>) -> String {
    tasks.sort_by_key(|task| task.task.to_string());
    if tasks.len() == 1 {
        return tasks[0].to_string();
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
        format!("{}:{{{names}}}", tasks[0].package.as_str())
    }
}

#[cfg(test)]
mod tests {
    use luchta_types::TaskId;

    use super::render_running_task_groups;

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
            "{a,b,c}:lint, d:{test,tsc}, e#babel",
        );
        assert_rendered_groups(
            &[task_ref("a", "lint"), task_ref("b", "lint")],
            "{a,b}:lint",
        );
        assert_rendered_groups(
            &[task_ref("pkg", "build"), task_ref("pkg", "test")],
            "pkg:{build,test}",
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
            "{a,b}:lint, #lint",
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
            "{a,b}:build, c:{lint,test}, d#check",
        );
        assert_rendered_groups(
            &[
                task_ref("z", "lint"),
                task_ref("a", "build"),
                task_ref("m", "build"),
            ],
            "{a,m}:build, z#lint",
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
            "@acme/{admin,api,web}:lint",
        );
        assert_rendered_groups(
            &[
                task_ref("@acme/web", "lint"),
                task_ref("@other/api", "lint"),
            ],
            "{@acme/web,@other/api}:lint",
        );
        assert_rendered_groups(
            &[task_ref("@acme/web", "lint"), task_ref("api", "lint")],
            "{@acme/web,api}:lint",
        );
        assert_rendered_groups(
            &[
                task_ref("@acme/web", "build"),
                task_ref("@acme/web", "test"),
            ],
            "@acme/web:{build,test}",
        );
    }

    fn assert_rendered_groups(tasks: &[TaskRef<'_>], expected: &str) {
        let tasks = running_tasks(tasks);
        assert_eq!(render_running_task_groups(&tasks), expected);
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
