use super::helpers::*;

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
