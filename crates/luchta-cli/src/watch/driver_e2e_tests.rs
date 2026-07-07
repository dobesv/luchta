use super::driver_e2e_support::*;
use std::collections::{BTreeSet, HashMap};
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

/// FAILED-CYCLE-REGISTRY: Prove failed initial cycle still registers watch state so next edit rebuilds.
#[tokio::test]
async fn failed_initial_cycle_edit_triggers_rebuild() {
    let harness = E2eHarness::start_failing_initial_cycle().await;

    harness.wait_for_jobs(1).await;
    harness.wait_for_markers(1).await;

    let app_src = harness.workspace_root.join("packages/app/src/lib.rs");
    std::fs::write(&app_src, "// edited\n").expect("write edited input");
    harness
        .send_batch(std::collections::HashSet::from([app_src]), false)
        .await;

    harness.wait_for_jobs(2).await;
    harness.wait_for_markers(2).await;
    assert_eq!(
        read_marker_count(&harness.workspace_root),
        2,
        "edit after FAILED initial cycle must trigger a rebuild, not [watch] up to date"
    );
    assert!(
        !harness.worker_manager_is_shutdown(),
        "worker manager should survive failed initial cycle and rebuild"
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

#[tokio::test]
async fn config_edit_triggers_rebuild() {
    let harness = E2eHarness::start().await;

    harness.wait_for_jobs(1).await;
    harness.release_first_cycle();
    harness.wait_for_markers(1).await;

    let config_path = harness
        .config_path()
        .canonicalize()
        .expect("canonicalize config path");
    let updated_config = format!(
        "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":7}},\"workers\":{{\"fake\":{{\"command\":\"{}\"}}}},\"tasks\":{{\"build\":{{\"worker\":\"fake\"}}}}}}'\n",
        harness.worker_script_path().display()
    );
    harness.rewrite_config(&updated_config);
    harness
        .send_batch(BTreeSet::from([config_path]).into_iter().collect(), true)
        .await;

    harness
        .wait_until(
            Duration::from_secs(10),
            || "timed out waiting for config edit rebuild to update session max_weight".to_string(),
            || harness.current_max_weight() == 7,
        )
        .await;

    assert_eq!(
        harness.session_package_names(),
        vec!["app".to_string()],
        "expected config-only rebuild to preserve package graph"
    );
    assert!(
        harness
            .stays_for_markers(1, Duration::from_millis(500))
            .await,
        "expected config-only rebuild to avoid extra task execution when package set is unchanged, got {} markers",
        read_marker_count(&harness.workspace_root)
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn malformed_config_does_not_crash_watch() {
    let harness = E2eHarness::start().await;

    harness.wait_for_jobs(1).await;
    harness.release_first_cycle();
    harness.wait_for_markers(1).await;

    let config_path = harness
        .config_path()
        .canonicalize()
        .expect("canonicalize config path");
    harness.rewrite_config("#!/bin/sh\necho not-json\n");
    harness
        .send_batch(
            BTreeSet::from([config_path.clone()]).into_iter().collect(),
            true,
        )
        .await;

    assert!(
        harness
            .stays_for_markers(1, Duration::from_millis(500))
            .await,
        "expected malformed config reload to avoid extra successful rebuild, got {} markers",
        read_marker_count(&harness.workspace_root)
    );
    assert_eq!(
        harness.current_max_weight(),
        4,
        "expected malformed config reload to keep previous run context active"
    );
    assert_eq!(
        harness.session_package_names(),
        vec!["app".to_string()],
        "expected malformed config reload to keep previous good graph alive"
    );

    let recovered_config = format!(
        "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":9}},\"workers\":{{\"fake\":{{\"command\":\"{}\"}}}},\"tasks\":{{\"build\":{{\"worker\":\"fake\"}}}}}}'\n",
        harness.worker_script_path().display()
    );
    harness.rewrite_config(&recovered_config);
    harness
        .send_batch(BTreeSet::from([config_path]).into_iter().collect(), true)
        .await;
    harness
        .wait_until(
            Duration::from_secs(10),
            || {
                "timed out waiting for recovered config rebuild to update session max_weight"
                    .to_string()
            },
            || harness.current_max_weight() == 9,
        )
        .await;

    assert_eq!(
        harness.session_package_names(),
        vec!["app".to_string()],
        "expected recovered config reload to restore same package graph"
    );
    assert!(
        harness
            .stays_for_markers(1, Duration::from_millis(500))
            .await,
        "expected config-only recovery rebuild to avoid extra task execution when package set is unchanged, got {} markers",
        read_marker_count(&harness.workspace_root)
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn config_reload_preserves_explicit_max_weight_override() {
    let harness = E2eHarness::start_with_max_weight_override(Some(5)).await;

    harness.wait_for_jobs(1).await;
    harness.release_first_cycle();
    harness.wait_for_markers(1).await;
    assert_eq!(
        harness.current_max_weight(),
        5,
        "expected explicit max weight override to apply on initial watch session"
    );

    let config_path = harness
        .config_path()
        .canonicalize()
        .expect("canonicalize config path");
    let rebuild_generation_before = harness.rebuild_generation();
    let updated_config = format!(
        "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":8}},\"workers\":{{\"fake\":{{\"command\":\"{}\"}}}},\"tasks\":{{\"build\":{{\"worker\":\"fake\"}}}}}}'\n",
        harness.worker_script_path().display()
    );
    harness.rewrite_config(&updated_config);
    harness
        .send_batch(BTreeSet::from([config_path]).into_iter().collect(), true)
        .await;
    harness
        .wait_until(
            Duration::from_secs(10),
            || "timed out waiting for config reload rebuild generation to increment".to_string(),
            || harness.rebuild_generation() > rebuild_generation_before,
        )
        .await;

    assert!(
        harness
            .stays_for(Duration::from_millis(500), || harness.current_max_weight()
                == 5)
            .await,
        "expected explicit max weight override to survive executed config reload"
    );
    assert_eq!(
        harness.current_max_weight(),
        5,
        "expected explicit max weight override to win over reloaded config"
    );
    assert_eq!(
        harness.session_package_names(),
        vec!["app".to_string()],
        "expected config reload with explicit override to preserve package graph"
    );
    assert!(
        harness
            .stays_for_markers(1, Duration::from_millis(500))
            .await,
        "expected config-only reload with explicit override to avoid extra task execution, got {} markers",
        read_marker_count(&harness.workspace_root)
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn lockfile_change_reruns_only_affected_package() {
    let harness = E2eHarness::start_two_package_lockfile().await;

    harness.wait_for_jobs(1).await;
    harness.release_first_cycle();
    harness.wait_for_jobs(2).await;
    harness
        .wait_until(
            Duration::from_secs(10),
            || "timed out waiting for both initial package markers".to_string(),
            || {
                let counts = harness.marker_counts_by_package();
                counts.get("a") == Some(&1) && counts.get("b") == Some(&1)
            },
        )
        .await;

    assert_eq!(
        harness.marker_counts_by_package(),
        HashMap::from([("a".to_string(), 1), ("b".to_string(), 1)]),
        "expected both packages to build once before lockfile edit"
    );

    std::fs::write(
        harness.workspace_root.join("yarn.lock"),
        two_package_lockfile_contents("1.1.0", "4.0.0"),
    )
    .expect("rewrite yarn.lock");
    harness.send_lockfile_change().await;
    harness
        .wait_until(
            Duration::from_secs(10),
            || "timed out waiting for package a rerun after lockfile change".to_string(),
            || read_marker_count_for(&harness.workspace_root, "a") >= 2,
        )
        .await;

    assert!(
        harness.stays_for_jobs(3, Duration::from_millis(500)).await,
        "expected exactly one extra job after lockfile change, got {} jobs",
        read_job_count(&harness.workspace_root)
    );
    assert_eq!(
        read_marker_count_for(&harness.workspace_root, "b"),
        1,
        "expected unrelated package b to stay idle after left-pad lockfile bump"
    );

    harness.shutdown().await;
}
