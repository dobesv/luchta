use std::collections::HashSet;

use luchta_types::PackageName;
use luchta_workspace::{PackageGraph, PackageNode};

use crate::cache_ctx::{gather_pkg_dep_pairs, LockfileState};

/// Returns supplied packages whose external dependency closures changed.
///
/// Failed states on either side or per-package resolution errors count as changes
/// because the closures cannot be diffed. Both Absent states produce an empty set;
/// when only one side is Absent, it compares as an empty closure. Unlike
/// `LockfileWatchState::affected_packages`, which treats a missing lockfile as
/// everything changed, an Absent base here means no external dependencies at that ref.
pub(crate) fn packages_with_changed_dep_closures<'a>(
    packages: impl IntoIterator<Item = &'a PackageNode>,
    package_graph: &PackageGraph,
    base: &LockfileState,
    head: &LockfileState,
) -> HashSet<PackageName> {
    match (base, head) {
        (LockfileState::Failed(_), _) | (_, LockfileState::Failed(_)) => {
            return packages
                .into_iter()
                .map(|package| package.name.clone())
                .collect();
        }
        (LockfileState::Absent, LockfileState::Absent) => return HashSet::new(),
        _ => {}
    }

    packages
        .into_iter()
        .filter(|package| {
            match (
                gather_pkg_dep_pairs(package, Some(package_graph), base),
                gather_pkg_dep_pairs(package, Some(package_graph), head),
            ) {
                (Ok(mut base_pairs), Ok(mut head_pairs)) => {
                    base_pairs.sort();
                    base_pairs.dedup();
                    head_pairs.sort();
                    head_pairs.dedup();
                    base_pairs != head_pairs
                }
                _ => true,
            }
        })
        .map(|package| package.name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use luchta_workspace::{WorkspaceDiscovery, YarnWorkspace};
    use tempfile::TempDir;

    use super::*;
    use crate::cache_ctx::parse_lockfile_content;

    const LOCKFILE_FIXTURE: &str = r#"__metadata:
  version: 8
  cacheKey: 10

"left-pad@npm:^1.0.0":
  version: 1.0.0
  resolution: "left-pad@npm:1.0.0"
  checksum: 10/leftpad

"foo@npm:^2.0.0":
  version: 2.0.1
  resolution: "foo@npm:2.0.1"
  checksum: 10/foo
  dependencies:
    bar: "npm:^3.0.0"

"bar@npm:^3.0.0":
  version: 3.0.1
  resolution: "bar@npm:3.0.1"
  checksum: 10/bar
"#;

    #[test]
    fn identical_states_select_nothing() {
        let workspace = TestWorkspace::new();
        let base = parsed(LOCKFILE_FIXTURE);
        let head = parsed(LOCKFILE_FIXTURE);

        assert!(workspace.changed_packages(&base, &head).is_empty());
    }

    fn assert_lockfile_change_selects(from: &str, to: &str, expected: &[&str]) {
        let workspace = TestWorkspace::new();
        let base = parsed(LOCKFILE_FIXTURE);
        let head = parsed(&LOCKFILE_FIXTURE.replace(from, to));

        assert_eq!(
            workspace.changed_packages(&base, &head),
            package_set(expected)
        );
    }

    #[test]
    fn transitive_only_version_change_selects_dependent_package() {
        assert_lockfile_change_selects("3.0.1", "3.0.2", &["c"]);
    }

    #[test]
    fn dev_dependency_version_change_selects_dependent_package() {
        assert_lockfile_change_selects("2.0.1", "2.0.2", &["c"]);
    }

    #[test]
    fn direct_checksum_only_change_selects_dependent_package() {
        assert_lockfile_change_selects("10/leftpad", "10/changed", &["a"]);
    }

    #[test]
    fn transitive_checksum_only_change_selects_dependent_package() {
        assert_lockfile_change_selects("10/bar", "10/changed", &["c"]);
    }

    #[test]
    fn failed_base_or_head_selects_all_packages() {
        let workspace = TestWorkspace::new();
        let failed = LockfileState::Failed("unreadable lockfile".into());
        let state = parsed(LOCKFILE_FIXTURE);
        for (base, head) in [
            (&failed, &LockfileState::Absent),
            (&failed, &state),
            (&failed, &failed),
            (&LockfileState::Absent, &failed),
            (&state, &failed),
        ] {
            assert_eq!(
                workspace.changed_packages(base, head),
                package_set(&["a", "b", "c"])
            );
        }
    }

    #[test]
    fn both_absent_select_nothing_without_reading_manifests() {
        let workspace = TestWorkspace::new();
        fs::remove_file(workspace.temp_dir.path().join("packages/b/package.json"))
            .expect("remove package manifest");

        assert!(workspace
            .changed_packages(&LockfileState::Absent, &LockfileState::Absent)
            .is_empty());
    }

    #[test]
    fn absent_to_parsed_selects_packages_with_external_dependencies() {
        let workspace = TestWorkspace::new();
        let head = parsed(LOCKFILE_FIXTURE);

        assert_eq!(
            workspace.changed_packages(&LockfileState::Absent, &head),
            package_set(&["a", "c"])
        );
    }

    #[test]
    fn parsed_to_absent_selects_packages_with_external_dependencies() {
        let workspace = TestWorkspace::new();
        let base = parsed(LOCKFILE_FIXTURE);

        assert_eq!(
            workspace.changed_packages(&base, &LockfileState::Absent),
            package_set(&["a", "c"])
        );
    }

    #[test]
    fn manifest_read_error_selects_only_that_package() {
        let workspace = TestWorkspace::new();
        fs::remove_file(workspace.temp_dir.path().join("packages/b/package.json"))
            .expect("remove package manifest");
        let state = parsed(LOCKFILE_FIXTURE);

        for (base, head, expected) in [
            (&state, &state, &["b"][..]),
            (&LockfileState::Absent, &state, &["a", "b", "c"][..]),
            (&state, &LockfileState::Absent, &["a", "b", "c"][..]),
        ] {
            assert_eq!(
                workspace.changed_packages(base, head),
                package_set(expected)
            );
        }
    }

    #[test]
    fn failed_state_selects_only_supplied_packages() {
        let workspace = TestWorkspace::new();
        let selected = workspace
            .packages
            .iter()
            .filter(|package| package.name.as_str() == "b");
        let changed = packages_with_changed_dep_closures(
            selected,
            &workspace.package_graph,
            &LockfileState::Failed("unreadable base".into()),
            &LockfileState::Absent,
        );

        assert_eq!(changed, package_set(&["b"]));
    }

    #[test]
    fn empty_candidates_stay_empty_with_failed_states() {
        let workspace = TestWorkspace::new();
        let failed = LockfileState::Failed("unreadable lockfile".into());

        assert!(
            packages_with_changed_dep_closures([], &workspace.package_graph, &failed, &failed)
                .is_empty()
        );
    }

    struct TestWorkspace {
        temp_dir: TempDir,
        packages: Vec<PackageNode>,
        package_graph: PackageGraph,
    }

    impl TestWorkspace {
        fn new() -> Self {
            let temp_dir = tempfile::tempdir().expect("create workspace");
            let root = temp_dir.path();
            fs::write(
                root.join("package.json"),
                r#"{"name":"root","private":true,"workspaces":["packages/*"]}"#,
            )
            .expect("write workspace manifest");
            for (name, manifest) in [
                (
                    "a",
                    r#"{"name":"a","version":"1.0.0","dependencies":{"left-pad":"^1.0.0"}}"#,
                ),
                ("b", r#"{"name":"b","version":"1.0.0"}"#),
                (
                    "c",
                    r#"{"name":"c","version":"1.0.0","devDependencies":{"foo":"^2.0.0"}}"#,
                ),
            ] {
                let package_dir = root.join("packages").join(name);
                fs::create_dir_all(&package_dir).expect("create package directory");
                fs::write(package_dir.join("package.json"), manifest)
                    .expect("write package manifest");
            }
            let packages: Vec<_> = YarnWorkspace::new(root)
                .discover()
                .expect("discover workspace")
                .into_iter()
                .filter(|package| package.path != root)
                .collect();
            assert_eq!(packages.len(), 3);
            let package_graph = PackageGraph::build(packages.clone()).expect("build package graph");
            Self {
                temp_dir,
                packages,
                package_graph,
            }
        }

        fn changed_packages(
            &self,
            base: &LockfileState,
            head: &LockfileState,
        ) -> HashSet<PackageName> {
            packages_with_changed_dep_closures(&self.packages, &self.package_graph, base, head)
        }
    }

    fn parsed(content: &str) -> LockfileState {
        let state = parse_lockfile_content(content);
        assert!(
            matches!(state, LockfileState::Parsed(_)),
            "expected parsed fixture, got {state:?}"
        );
        state
    }

    fn package_set(names: &[&str]) -> HashSet<PackageName> {
        names.iter().map(|name| PackageName::from(*name)).collect()
    }
}
