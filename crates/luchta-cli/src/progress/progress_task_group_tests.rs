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
        "{a,b,c}#lint, d#{test,tsc}, e#babel"
    );
}

#[test]
fn render_running_task_groups_all_same_task_across_packages() {
    let tasks = running_tasks(&[("a", "build"), ("b", "build"), ("c", "build")]);

    assert_eq!(render_running_task_groups(&tasks), "{a,b,c}#build");
}

#[test]
fn render_running_task_groups_single_package_multiple_tasks() {
    let tasks = running_tasks(&[("pkg", "build"), ("pkg", "test")]);

    assert_eq!(render_running_task_groups(&tasks), "pkg#{build,test}");
}

#[test]
fn render_running_task_groups_singletons_render_individually() {
    let tasks = running_tasks(&[("a", "lint"), ("b", "test"), ("c", "tsc")]);

    assert_eq!(render_running_task_groups(&tasks), "a#lint, b#test, c#tsc");
}

#[test]
fn render_running_task_groups_root_shared_task_does_not_prevent_non_root_grouping() {
    let tasks = running_tasks(&[("//root", "lint"), ("a", "lint"), ("b", "lint")]);

    // Non-root packages still group; root task stays separate as `#lint`.
    assert_eq!(render_running_task_groups(&tasks), "{a,b}#lint, #lint");
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

    assert_eq!(render_running_task_groups(&tasks), "{a,b}#lint, #lint");
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
        "{a,b}#build, c#{lint,test}, d#check"
    );
}

#[test]
fn render_running_task_groups_deterministic_sorting() {
    let tasks = running_tasks(&[("z", "lint"), ("a", "build"), ("m", "build")]);

    assert_eq!(render_running_task_groups(&tasks), "{a,m}#build, z#lint");
}

#[test]
fn render_running_task_groups_scope_rendering_examples() {
    for (tasks, expected) in [
        (
            running_tasks(&[
                ("@acme/web", "lint"),
                ("@acme/api", "lint"),
                ("@acme/admin", "lint"),
            ]),
            "{admin,api,web}#lint",
        ),
        (
            running_tasks(&[
                ("@acme/a", "lint"),
                ("@acme/b", "lint"),
                ("@acme/c", "build"),
                ("@acme/c", "test"),
            ]),
            "{a,b}#lint, c#{build,test}",
        ),
        (
            running_tasks(&[
                ("@acme/web", "build"),
                ("@acme/api", "lint"),
                ("@acme/api", "test"),
            ]),
            "api#{lint,test}, web#build",
        ),
    ] {
        assert_eq!(render_running_task_groups(&tasks), expected);
    }
}

#[test]
fn render_running_task_groups_mixed_scopes_keep_full_names() {
    let tasks = running_tasks(&[("@acme/web", "lint"), ("@other/api", "lint")]);

    assert_eq!(
        render_running_task_groups(&tasks),
        "{@acme/web,@other/api}#lint"
    );
}

#[test]
fn render_running_task_groups_scope_with_unscoped_keeps_full_names() {
    let tasks = running_tasks(&[("@acme/web", "lint"), ("api", "lint")]);

    assert_eq!(render_running_task_groups(&tasks), "{@acme/web,api}#lint");
}

#[test]
fn render_running_task_groups_scoped_single_leftover_uses_hash_join() {
    let tasks = running_tasks(&[("@acme/web", "build"), ("@acme/web", "test")]);

    assert_eq!(render_running_task_groups(&tasks), "web#{build,test}");
}

#[test]
fn render_running_task_groups_global_scope_difference_keeps_full_names() {
    let tasks = running_tasks(&[("@acme/a", "lint"), ("@other/b", "build")]);

    assert_eq!(
        render_running_task_groups(&tasks),
        "@acme/a#lint, @other/b#build"
    );
}

#[test]
fn render_running_task_groups_compacts_word_boundary_prefix() {
    let tasks = running_tasks(&[
        ("@formative/server-answers", "test"),
        ("@formative/server-changes", "test"),
        ("@formative/server-enrollments", "test"),
        ("@formative/server-export", "test"),
        ("@formative/server-folders", "test"),
    ]);

    assert_eq!(
        render_running_task_groups(&tasks),
        "server-{answers,changes,enrollments,export,folders}#test"
    );
}

#[test]
fn render_running_task_groups_issue_145_examples() {
    assert_eq!(
        render_running_task_groups(&running_tasks(&[("a", "lint"), ("b", "lint")])),
        "{a,b}#lint"
    );
    assert_eq!(
        render_running_task_groups(&running_tasks(&[("pkg", "build"), ("pkg", "test")])),
        "pkg#{build,test}"
    );
    assert_eq!(
        render_running_task_groups(&running_tasks(&[
            ("@acme/web", "lint"),
            ("@acme/api", "lint"),
        ])),
        "{api,web}#lint"
    );
    assert_eq!(
        render_running_task_groups(&running_tasks(&[("//root", "build"), ("//root", "test")])),
        "#{build,test}"
    );
    assert_eq!(
        render_running_task_groups(&running_tasks(&[("//root", "lint")])),
        "#lint"
    );
}

#[test]
fn render_running_task_groups_issue_146_examples() {
    assert_eq!(
        render_running_task_groups(&running_tasks(&[
            ("@formative/server-answers", "test"),
            ("@formative/server-changes", "test"),
            ("@formative/server-enrollments", "test"),
            ("@formative/server-export", "test"),
            ("@formative/server-folders", "test"),
        ])),
        "server-{answers,changes,enrollments,export,folders}#test"
    );
    assert_eq!(
        render_running_task_groups(&running_tasks(&[
            ("@acme/admin", "lint"),
            ("@acme/api", "lint"),
            ("@acme/web", "lint"),
        ])),
        "{admin,api,web}#lint"
    );
}
