use super::driver_e2e_support::*;
use std::collections::BTreeSet;
use std::sync::Arc;
use tokio::time::Duration;

/// NO-LOST-CHANGES: Prove that injecting a change during an in-flight cycle triggers a follow-up cycle.
#[tokio::test]
async fn no_lost_changes_change_during_build_triggers_second_cycle() {
    let harness = E2eHarness::start().await;

    harness.wait_for_jobs(1).await;
    harness.send_package_change().await;
    harness.release_first_cycle();
    harness.wait_for_jobs(2).await;
    harness.wait_for_markers(2).await;

    assert_eq!(
        read_marker_count(&harness.workspace_root),
        2,
        "expected exactly 2 worker jobs before shutdown"
    );

    harness.shutdown().await;
}

/// KEEPS-MANAGER-ALIVE: Prove manager survives a change-triggered cancel.
#[tokio::test]
async fn change_during_cycle_keeps_worker_manager_alive() {
    let harness = E2eHarness::start().await;
    let h1 = harness.worker_manager_handle();
    let h2 = harness.worker_manager_handle();

    harness.wait_for_jobs(1).await;
    harness.send_package_change().await;
    harness.release_first_cycle();
    harness.wait_for_jobs(2).await;
    harness.wait_for_markers(2).await;

    assert_eq!(
        read_marker_count(&harness.workspace_root),
        2,
        "expected exactly 2 worker jobs before shutdown"
    );
    assert!(
        Arc::ptr_eq(&h1, &h2),
        "worker manager Arc identity should stay stable across cycles"
    );
    assert!(
        !h1.is_shutdown(),
        "worker manager should NOT be shut down mid-watch"
    );

    harness.shutdown().await;
}

/// IGNORE/EMPTY-AFFECTED: Prove changes outside all packages don't trigger rebuild.
#[tokio::test]
async fn change_outside_package_does_not_trigger_rebuild() {
    let harness = E2eHarness::start().await;

    harness.wait_for_jobs(1).await;
    harness.release_first_cycle();
    tokio::time::sleep(Duration::from_millis(100)).await;
    harness.send_outside_change().await;

    assert!(
        harness
            .stays_for_markers(1, Duration::from_millis(500))
            .await,
        "expected marker count to stay at 1 for change outside packages, got {}",
        read_marker_count(&harness.workspace_root)
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn structural_add_package_triggers_rebuild() {
    let harness = E2eHarness::start().await;

    harness.wait_for_jobs(1).await;
    harness.release_first_cycle();
    harness.wait_for_markers(1).await;
    harness
        .mutate_packages_and_signal(PackageMutation::Add {
            relative_path: "packages/newpkg",
            name: "newpkg",
        })
        .await;
    harness
        .wait_for_session_package_names(&["app", "newpkg"])
        .await;

    assert_eq!(
        harness.package_names(),
        vec!["app".to_string(), "newpkg".to_string()],
        "expected structural add to rebuild package graph"
    );
    assert!(
        harness
            .stays_for_markers(1, Duration::from_millis(500))
            .await,
        "expected noop watcher harness to avoid extra worker cycle for pure structural add, got {}",
        read_marker_count(&harness.workspace_root)
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn structural_remove_package_drops_it_and_settles() {
    let harness = E2eHarness::start().await;

    harness.wait_for_jobs(1).await;
    harness.release_first_cycle();
    harness.wait_for_markers(1).await;
    harness
        .mutate_packages_and_signal(PackageMutation::Remove {
            relative_path: "packages/app",
        })
        .await;

    assert!(
        harness.stays_for_jobs(1, Duration::from_millis(500)).await,
        "expected removed package graph to stay idle after drop, got {} jobs",
        read_job_count(&harness.workspace_root)
    );
    assert!(
        harness.package_paths().is_empty(),
        "expected no workspace packages after structural remove"
    );
    assert!(
        harness.package_names().is_empty(),
        "expected removed package to be dropped from workspace discovery"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn structural_rename_package_dir_updates_tracked_path() {
    let harness = E2eHarness::start().await;
    let old_path = harness
        .workspace_root
        .join("packages/app")
        .canonicalize()
        .expect("old app path");

    harness.wait_for_jobs(1).await;
    harness.release_first_cycle();
    harness.wait_for_markers(1).await;
    harness
        .mutate_packages_and_signal(PackageMutation::Rename {
            from: "packages/app",
            to: "packages/app2",
        })
        .await;
    let renamed_path = harness
        .workspace_root
        .join("packages/app2")
        .canonicalize()
        .expect("renamed app path");
    harness
        .wait_for_session_package_paths(&BTreeSet::from([renamed_path.clone()]))
        .await;

    let package_paths = harness.package_paths();
    assert!(
        !package_paths.contains(&old_path),
        "expected old package path to be removed after rename"
    );
    assert!(
        package_paths.contains(&renamed_path),
        "expected renamed package path to be tracked"
    );
    assert!(
        harness.stays_for_markers( 1, Duration::from_millis(500)).await,
        "expected noop watcher harness to avoid extra worker cycle for pure structural rename, got {}",
        read_marker_count(&harness.workspace_root)
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn structural_move_package_to_new_top_level_dir_triggers_rebuild() {
    let harness = E2eHarness::start().await;
    let old_path = harness
        .workspace_root
        .join("packages/app")
        .canonicalize()
        .expect("old app path");

    harness
        .run_structural_case(
            PackageMutation::Rename {
                from: "packages/app",
                to: "packages/cat/app",
            },
            &["app"],
            "expected noop watcher harness to avoid extra worker cycle for pure structural move",
        )
        .await;
    let moved_path = harness
        .workspace_root
        .join("packages/cat/app")
        .canonicalize()
        .expect("moved app path");
    harness
        .wait_for_session_package_paths(&BTreeSet::from([moved_path.clone()]))
        .await;

    let package_paths = harness.package_paths();
    assert!(
        !package_paths.contains(&old_path),
        "expected old package path to be removed after move"
    );
    assert!(
        package_paths.contains(&moved_path),
        "expected moved package path to be tracked"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn structural_change_during_build_updates_graph_without_deadlock() {
    let harness = E2eHarness::start().await;

    harness
        .run_structural_case(
            PackageMutation::Add {
                relative_path: "packages/newpkg",
                name: "newpkg",
            },
            &["app", "newpkg"],
            "expected noop watcher harness to settle after cancelled mid-build structural change",
        )
        .await;

    harness.shutdown().await;
}

#[tokio::test]
async fn structural_rebuild_keeps_worker_manager_identity() {
    let harness = E2eHarness::start().await;
    let before = harness.worker_manager_handle();

    harness.wait_for_jobs(1).await;
    harness.release_first_cycle();
    harness.wait_for_markers(1).await;
    harness
        .mutate_packages_and_signal(PackageMutation::Add {
            relative_path: "packages/newpkg",
            name: "newpkg",
        })
        .await;
    harness
        .wait_for_session_package_names(&["app", "newpkg"])
        .await;

    let after = harness.worker_manager_handle();
    assert!(
        Arc::ptr_eq(&before, &after),
        "worker manager Arc identity inside live session should stay stable across structural rebuild"
    );
    assert!(
        !harness.worker_manager_is_shutdown(),
        "worker manager should NOT be shut down by structural rebuild"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn malformed_package_json_keeps_previous_graph_and_loop_alive() {
    let harness = E2eHarness::start().await;

    harness.wait_for_jobs(1).await;
    harness.release_first_cycle();
    harness.wait_for_markers(1).await;
    harness
        .mutate_packages_and_signal(PackageMutation::Malformed {
            relative_path: "packages/badpkg",
        })
        .await;

    assert!(
        discover_package_names(&harness.workspace_root).is_err(),
        "expected malformed package to break fresh discovery while watch loop keeps prior graph"
    );
    assert!(
        harness
            .stays_for_markers(1, Duration::from_millis(500))
            .await,
        "expected malformed package discovery to avoid extra rebuild, got {} markers",
        read_marker_count(&harness.workspace_root)
    );
    std::fs::remove_dir_all(harness.workspace_root.join("packages/badpkg"))
        .expect("remove malformed package dir");
    harness.send_package_change().await;
    harness.wait_for_jobs(2).await;
    harness.wait_for_markers(2).await;

    assert_eq!(
        harness.package_names(),
        vec!["app".to_string()],
        "expected malformed package discovery to keep previous good graph"
    );
    assert_eq!(
        read_marker_count(&harness.workspace_root),
        2,
        "expected watch loop to remain responsive after malformed package.json"
    );

    harness.shutdown().await;
}
