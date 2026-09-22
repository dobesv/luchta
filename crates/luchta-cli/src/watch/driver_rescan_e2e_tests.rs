use super::driver_e2e_support::{read_marker_count, E2eHarness};

/// Prove that a dropped-event signal rehashes the full selection even when the
/// backend cannot name the paths whose events were lost.
#[tokio::test]
async fn backend_rescan_detects_change_without_a_path_event() {
    let harness = E2eHarness::start().await;

    harness.wait_for_jobs(1).await;
    harness.release_first_cycle();
    harness.wait_for_markers(1).await;
    harness.wait_for_completed_cycles(1).await;

    let changed_input = harness.workspace_root.join("packages/app/src/lib.rs");
    std::fs::create_dir_all(changed_input.parent().expect("changed input parent"))
        .expect("create input directory");
    std::fs::write(changed_input, "// changed while events were dropped\n")
        .expect("write changed input");
    harness.send_rescan().await;

    harness.wait_for_jobs(2).await;
    harness.wait_for_markers(2).await;
    assert_eq!(
        read_marker_count(&harness.workspace_root),
        2,
        "rescan must detect changed content without a path event"
    );

    harness.shutdown().await;
}
