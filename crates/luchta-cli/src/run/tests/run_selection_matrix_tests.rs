//! Table-driven tests for `collect_requested_subgraph` package/task selection.
//!
//! Split into its own file so the selection-matrix test data, helper, and the
//! two matrix tests form a self-contained unit. Fixtures live in the parent
//! `tests` module and are reached via `use super::*`.

use super::*;

/// A table-driven success case: (name, task patterns, package patterns,
/// top_level, expected (package, task) goals, fixture).
type SuccessCase = (
    &'static str,
    &'static [&'static str],
    &'static [&'static str],
    bool,
    &'static [(&'static str, &'static str)],
    fn() -> TaskGraph,
);

/// A table-driven error case: (name, task patterns, package patterns,
/// top_level, expected error substring, fixture).
type ErrorCase = (
    &'static str,
    &'static [&'static str],
    &'static [&'static str],
    bool,
    &'static str,
    fn() -> TaskGraph,
);

/// 9 success cases covering the selection matrix.
const SUCCESS_CASES: &[SuccessCase] = &[
    (
        "excludes_root_by_default",
        &["build"],
        &[],
        false,
        &[("@repo/pkg", "build")],
        matching_scope_task_graph,
    ),
    (
        "selects_root_for_top_level",
        &["build"],
        &[],
        true,
        &[("//root", "build")],
        matching_scope_task_graph,
    ),
    (
        "named_pkg_build_with_deps",
        &["build"],
        &["@repo/app"],
        false,
        &[("@repo/app", "build"), ("@repo/api", "build")],
        package_selection_task_graph,
    ),
    (
        "multiple_named_pkgs",
        &["build"],
        &["@repo/app", "@repo/api"],
        false,
        &[("@repo/app", "build"), ("@repo/api", "build")],
        package_selection_task_graph,
    ),
    (
        "pkg_glob_matches_multiple",
        &["build"],
        &["@repo/*"],
        false,
        &[("@repo/app", "build"), ("@repo/api", "build")],
        package_selection_task_graph,
    ),
    (
        "task_glob_in_scope",
        &["test*"],
        &["@repo/app"],
        false,
        &[
            ("@repo/app", "build"),
            ("@repo/app", "test"),
            ("@repo/app", "test:e2e"),
            ("@repo/api", "build"),
        ],
        package_selection_task_graph,
    ),
    (
        "pkg_and_task_globs_and",
        &["build*"],
        &["pkg-*"],
        false,
        &[
            ("pkg-foo", "build"),
            ("pkg-foo", "build-lib"),
            ("pkg-bar", "build"),
            ("pkg-bar", "build-lib"),
        ],
        package_selection_task_graph,
    ),
    (
        "glob_no_false_positive",
        &["build*"],
        &["@repo/app"],
        false,
        &[
            ("@repo/app", "build"),
            ("@repo/app", "build-lib"),
            ("@repo/api", "build"),
        ],
        package_selection_task_graph,
    ),
    (
        "both_literals_exist_with_package_filter",
        &["build", "test"],
        &["@repo/app"],
        false,
        &[
            ("@repo/app", "build"),
            ("@repo/app", "test"),
            ("@repo/api", "build"),
        ],
        package_selection_task_graph,
    ),
    (
        "top_level_with_pkg_filter",
        &["build"],
        &["@repo/app"],
        true,
        &[
            ("@repo/app", "build"),
            ("//root", "build"),
            ("@repo/api", "build"),
        ],
        package_selection_task_graph,
    ),
    (
        "top_level_no_pkg_filter",
        &["build"],
        &[],
        true,
        &[("//root", "build")],
        matching_scope_task_graph,
    ),
];

/// Error cases distinguishing "no packages matched" from "no tasks matched",
/// including the eval-order regression (`-T` with an unmatched package).
const ERROR_CASES: &[ErrorCase] = &[
    (
        "no_pkg_match",
        &["build"],
        &["no-such-pkg"],
        false,
        "No packages matched",
        package_selection_task_graph,
    ),
    (
        "top_level_no_pkg_match",
        &["build"],
        &["no-such-pkg"],
        true,
        "No packages matched",
        package_selection_task_graph,
    ),
    (
        "no_task_in_selected_pkg",
        &["no-such-task*"],
        &["@repo/app"],
        false,
        "No tasks matched",
        package_selection_task_graph,
    ),
    (
        "literal_partial_miss",
        &["build", "missing"],
        &[],
        false,
        "missing",
        package_selection_task_graph,
    ),
    (
        "literal_miss_with_package_filter",
        &["build", "nope"],
        &["@repo/app"],
        false,
        "nope",
        package_selection_task_graph,
    ),
];

/// Runs `collect_requested_subgraph` for one table case, building the
/// `TaskSelection` from `&str` patterns against the given fixture. Shared by
/// the success and error matrix tests so their setup lives in one place.
#[allow(clippy::type_complexity)]
fn run_selection_case(
    case: &(&str, &[&str], &[&str], bool, impl Sized, fn() -> TaskGraph),
) -> Result<HashSet<TaskId>> {
    let (_, tasks, packages, top_level, _, fixture) = case;
    let task_graph = fixture();
    let requested_tasks: Vec<String> = tasks.iter().map(|s| s.to_string()).collect();
    let packages: Vec<String> = packages.iter().map(|s| s.to_string()).collect();
    let selection = TaskSelection {
        requested_tasks: &requested_tasks,
        packages: &packages,
        top_level: *top_level,
        since: None,
    };
    collect_requested_subgraph(&task_graph, &selection, &[], None)
}

/// Verifies table-driven success cases for collect_requested_subgraph.
#[test]
fn collect_requested_subgraph_matrix_success() {
    for case in SUCCESS_CASES {
        let (name, .., expected, _) = case;
        let requested = run_selection_case(case).expect(name);
        let expected_ids: HashSet<TaskId> =
            expected.iter().map(|(p, t)| TaskId::new(*p, *t)).collect();
        assert_eq!(requested, expected_ids, "{}", name);
    }
}

/// Verifies table-driven error cases for collect_requested_subgraph.
#[test]
fn collect_requested_subgraph_matrix_error() {
    for case in ERROR_CASES {
        let (name, .., error_contains, _) = case;
        let error = run_selection_case(case).expect_err(name);
        assert!(
            error.to_string().contains(error_contains),
            "{}: expected '{}' in '{}'",
            name,
            error_contains,
            error
        );
    }
}
