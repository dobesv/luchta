use super::*;
use std::fs;

use crate::cli::OutputMode;
use crate::watch::session::WatchSession;
use tokio_util::sync::CancellationToken;

fn write_watch_test_workspace(workspace_root: &std::path::Path, worker_script_body: &str) {
    fs::create_dir_all(workspace_root.join("packages/app")).expect("create package dir");
    fs::write(
        workspace_root.join("package.json"),
        r#"{
            "name": "root",
            "private": true,
            "workspaces": ["packages/*"]
        }"#,
    )
    .expect("write root package.json");
    fs::write(
        workspace_root.join("packages/app/package.json"),
        r#"{
            "name": "app",
            "version": "1.0.0",
            "scripts": {
                "build": "echo build"
            }
        }"#,
    )
    .expect("write app package.json");

    let worker_script = workspace_root.join("fake-worker.sh");
    fs::write(&worker_script, worker_script_body).expect("write worker script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&worker_script, fs::Permissions::from_mode(0o755))
            .expect("chmod worker script");
    }

    fs::write(
        workspace_root.join("luchta-config.sh"),
        format!(
            "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":4}},\"workers\":{{\"fake\":{{\"command\":\"{}\"}}}},\"tasks\":{{\"build\":{{\"worker\":\"fake\"}}}}}}'\n",
            worker_script.display()
        ),
    )
    .expect("write config");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            workspace_root.join("luchta-config.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .expect("chmod config");
    }
}

fn write_watch_counter_workspace(workspace_root: &std::path::Path) {
    fn set_executable(path: &std::path::Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod executable");
        }
    }

    fs::create_dir_all(workspace_root.join("packages/app")).expect("create package dir");
    fs::write(
        workspace_root.join("package.json"),
        r#"{
            "name": "root",
            "private": true,
            "workspaces": ["packages/*"]
        }"#,
    )
    .expect("write root package.json");
    fs::write(
        workspace_root.join("packages/app/package.json"),
        r#"{
            "name": "app",
            "version": "1.0.0",
            "scripts": {
                "build": "echo ignored"
            }
        }"#,
    )
    .expect("write app package.json");
    fs::write(
        workspace_root.join("packages/app/src.txt"),
        "stable-input\n",
    )
    .expect("write input file");

    let worker_script = workspace_root.join("shell-worker.sh");
    fs::write(
        &worker_script,
        r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"type":"resolveTask"'*)
      printf '{"type":"resolved","id":"%s","result":{"decision":"accept"}}\n' "$id"
      ;;
    *'"type":"run"'*)
      cmd=$(printf '%s\n' "$line" | sed -n 's/.*"command":"\([^"]*\)".*/\1/p' | sed 's/\\"/"/g; s/\\\\/\\/g')
      cwd=$(printf '%s\n' "$line" | sed -n 's/.*"cwd":"\([^"]*\)".*/\1/p' | sed 's/\\"/"/g; s/\\\\/\\/g')
      (cd "$cwd" && sh -lc "$cmd")
      code=$?
      printf '{"type":"done","id":"%s","exitCode":%s}\n' "$id" "$code"
      ;;
  esac
done
"#,
    )
    .expect("write worker script");
    set_executable(&worker_script);

    fs::write(
        workspace_root.join("luchta-config.sh"),
        format!(
            "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":4}},\"workers\":{{\"shell\":{{\"command\":\"{}\"}}}},\"tasks\":{{\"build\":{{\"cache\":{{}},\"worker\":\"shell\",\"inputs\":[\"src.txt\"],\"outputs\":[\"counter.txt\"],\"command\":\"count=$(cat counter.txt 2>/dev/null || echo 0); count=$((count+1)); echo $count > counter.txt\"}}}}}}'\n",
            worker_script.display()
        ),
    )
    .expect("write config");
    set_executable(&workspace_root.join("luchta-config.sh"));

    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(workspace_root)
            .status()
            .expect("run git command");
        assert!(status.success(), "git command failed: {args:?}");
    };
    git(&["init"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test User"]);
    git(&["add", "."]);
    git(&["commit", "-m", "init"]);
}

fn watch_selection<'a>(requested_tasks: &'a [String]) -> TaskSelection<'a> {
    TaskSelection {
        requested_tasks,
        packages: &[],
        top_level: false,
        since: None,
    }
}

struct WatchTestHarness {
    _temp_dir: tempfile::TempDir,
    session: Arc<WatchSession>,
    workspace_root: std::path::PathBuf,
    started: std::path::PathBuf,
}

impl WatchTestHarness {
    async fn with_workspace<F>(write_workspace: F) -> Self
    where
        F: FnOnce(&std::path::Path, &std::path::Path),
    {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let workspace_root = temp_dir.path();
        let started = workspace_root.join("job-started");
        write_workspace(workspace_root, &started);

        let session = Arc::new(
            WatchSession::new(workspace_root, None)
                .await
                .expect("create watch session")
                .expect("session should not be None"),
        );

        Self {
            workspace_root: workspace_root.to_path_buf(),
            _temp_dir: temp_dir,
            session,
            started,
        }
    }

    fn worker_manager_handle(&self) -> Arc<luchta_engine::WorkerManager> {
        self.session.worker_manager_handle()
    }

    fn selection(&self) -> TaskSelection<'static> {
        watch_selection(Box::leak(Box::new(vec!["build".to_string()])))
    }

    fn package_paths(&self) -> std::collections::BTreeSet<std::path::PathBuf> {
        let packages_dir = self.workspace_root.join("packages");
        std::fs::read_dir(&packages_dir)
            .expect("read packages dir")
            .map(|entry| entry.expect("package entry").path())
            .collect()
    }

    fn package_names(&self) -> Vec<String> {
        self.session
            .run_context_for_test()
            .package_nodes
            .iter()
            .map(|package| package.name.as_ref().to_string())
            .collect()
    }

    async fn rebuild_for_current_packages(&self) {
        let package_paths = self.package_paths();
        self.session
            .rebuild_for_packages(&package_paths)
            .await
            .expect("rebuild watch session");
    }

    async fn run_cycle(
        &self,
        selection: &TaskSelection<'_>,
        token: CancellationToken,
    ) -> CycleOutcome {
        self.run_cycle_with_no_cache(selection, false, token).await
    }

    async fn run_cycle_with_no_cache(
        &self,
        selection: &TaskSelection<'_>,
        no_cache: bool,
        token: CancellationToken,
    ) -> CycleOutcome {
        self.session
            .run_cycle(
                RunCycleParams {
                    no_cache,
                    ..default_watch_cycle_params(selection)
                },
                token,
            )
            .await
            .expect("watch cycle result")
    }
}

fn default_watch_cycle_params<'a>(selection: &'a TaskSelection<'a>) -> RunCycleParams<'a> {
    RunCycleParams {
        selection,
        since_affected: None,
        output: OutputMode::Default,
        continue_on_failure: false,
        no_cache: false,
        memory_pressure: MemoryPressureConfig {
            usage: None,
            free: None,
        },
    }
}

fn read_counter_file(workspace_root: &std::path::Path) -> u32 {
    fs::read_to_string(workspace_root.join("packages/app/counter.txt"))
        .expect("read counter file")
        .trim()
        .parse()
        .expect("parse counter file")
}

fn worker_script(success: bool) -> String {
    format!(
        "#!/bin/sh\nwhile IFS= read -r line; do\n  case \"$line\" in\n    *'\"type\":\"resolveTask\"'*)\n      id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\"\\([^\"]*\\)\".*/\\1/p')\n      printf '{{\"type\":\"resolved\",\"id\":\"%s\",\"result\":{{\"decision\":\"accept\"}}}}\\n' \"$id\"\n      ;;\n    *'\"type\":\"run\"'*)\n      id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\"\\([^\"]*\\)\".*/\\1/p')\n      printf '{{\"type\":\"done\",\"id\":\"%s\",\"success\":{},\"exitCode\":{}}}\\n' \"$id\"\n      ;;\n  esac\ndone\n",
        success,
        if success { 0 } else { 1 }
    )
}

fn cancellation_worker_script(
    started: &std::path::Path,
    workspace_root: &std::path::Path,
) -> String {
    format!(
        "#!/bin/sh\nwhile IFS= read -r line; do\n  case \"$line\" in\n    *'\"type\":\"resolveTask\"'*)\n      id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\"\\([^\"]*\\)\".*/\\1/p')\n      printf '{{\"type\":\"resolved\",\"id\":\"%s\",\"result\":{{\"decision\":\"accept\"}}}}\\n' \"$id\"\n      ;;\n    *'\"type\":\"run\"'*)\n      echo saw-run >> '{}'
  id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\"\\([^\"]*\\)\".*/\\1/p')\n      : > '{}'\n      sleep 1\n      printf '{{\"type\":\"done\",\"id\":\"%s\",\"success\":true,\"exitCode\":0}}\\n' \"$id\"\n      ;;
*)
  echo saw-other >> '{}'
  ;;
  esac\ndone\n",
        workspace_root.join("worker-trace").display(),
        started.display(),
        workspace_root.join("worker-trace").display()
    )
}

#[tokio::test]
async fn watch_session_reuses_worker_manager_across_two_real_cycles() {
    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let workspace_root = temp_dir.path();
    write_watch_test_workspace(
        workspace_root,
        "#!/bin/sh\nwhile IFS= read -r line; do\n  case \"$line\" in\n    *'\"type\":\"resolveTask\"'*)\n      id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\"\\([^\"]*\\)\".*/\\1/p')\n      printf '{\"type\":\"resolved\",\"id\":\"%s\",\"result\":{\"decision\":\"accept\"}}\\n' \"$id\"\n      ;;\n    *'\"type\":\"run\"'*)\n      id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\"\\([^\"]*\\)\".*/\\1/p')\n      printf '{\"type\":\"done\",\"id\":\"%s\",\"success\":true,\"exitCode\":0}\\n' \"$id\"\n      ;;\n  esac\ndone\n",
    );

    let session = WatchSession::new(workspace_root, None)
        .await
        .expect("create watch session")
        .expect("session should not be None");
    let h1 = session.worker_manager_handle();

    let requested_tasks = vec!["build".to_string()];
    let selection = watch_selection(&requested_tasks);

    let outcome1 = session
        .run_cycle(
            RunCycleParams {
                selection: &selection,
                since_affected: None,
                output: OutputMode::Default,
                continue_on_failure: false,
                no_cache: false,
                memory_pressure: MemoryPressureConfig {
                    usage: None,
                    free: None,
                },
            },
            CancellationToken::new(),
        )
        .await
        .expect("first cycle succeeds");
    assert_eq!(outcome1, CycleOutcome::Success);

    let outcome2 = session
        .run_cycle(
            RunCycleParams {
                selection: &selection,
                since_affected: None,
                output: OutputMode::Default,
                continue_on_failure: false,
                no_cache: false,
                memory_pressure: MemoryPressureConfig {
                    usage: None,
                    free: None,
                },
            },
            CancellationToken::new(),
        )
        .await
        .expect("second cycle succeeds");
    assert_eq!(outcome2, CycleOutcome::Success);

    let h2 = session.worker_manager_handle();
    assert!(
        Arc::ptr_eq(&h1, &h2),
        "worker manager Arc should be reused across cycles"
    );

    session.shutdown().await;
}

#[tokio::test]
async fn watch_session_rebuild_for_packages_swaps_graphs_and_keeps_worker_manager() {
    let harness = WatchTestHarness::with_workspace(|workspace_root, _started| {
        write_watch_test_workspace(workspace_root, &worker_script(true));
    })
    .await;
    let h1 = harness.worker_manager_handle();
    assert_eq!(
        harness.package_names(),
        vec!["root".to_string(), "app".to_string()]
    );

    std::fs::create_dir_all(harness.workspace_root.join("packages/lib")).expect("create lib dir");
    std::fs::write(
        harness.workspace_root.join("packages/lib/package.json"),
        r#"{
            "name": "lib",
            "version": "1.0.0",
            "scripts": {
                "build": "echo build"
            }
        }"#,
    )
    .expect("write lib package");

    harness.rebuild_for_current_packages().await;

    let h2 = harness.worker_manager_handle();
    assert!(
        Arc::ptr_eq(&h1, &h2),
        "worker manager Arc should survive structural rebuild"
    );
    assert_eq!(
        harness.package_names(),
        vec!["app".to_string(), "lib".to_string()],
        "rebuilt run context should expose new packages"
    );
    assert!(
        !harness.session.worker_manager_is_shutdown(),
        "worker manager should stay alive after rebuild"
    );

    std::fs::remove_dir_all(harness.workspace_root.join("packages/lib")).expect("remove lib dir");
    harness.rebuild_for_current_packages().await;

    assert!(
        Arc::ptr_eq(&h1, &harness.worker_manager_handle()),
        "worker manager Arc should remain stable after package removal rebuild"
    );
    assert_eq!(
        harness.package_names(),
        vec!["app".to_string()],
        "rebuilt run context should drop removed packages"
    );

    harness.session.shutdown().await;
}

#[tokio::test]
async fn watch_session_run_cycle_reports_failed_outcome_without_poisoning_session() {
    let harness = WatchTestHarness::with_workspace(|workspace_root, _started| {
        write_watch_test_workspace(workspace_root, &worker_script(false));
    })
    .await;
    let h1 = harness.worker_manager_handle();
    let selection = harness.selection();

    let outcome1 = harness
        .run_cycle(&selection, CancellationToken::new())
        .await;
    assert_eq!(outcome1, CycleOutcome::Failed);

    let outcome2 = harness
        .run_cycle(&selection, CancellationToken::new())
        .await;
    assert_eq!(outcome2, CycleOutcome::Failed);
    assert!(
        Arc::ptr_eq(&h1, &harness.worker_manager_handle()),
        "worker manager Arc should survive failed cycle for watch reuse"
    );

    harness.session.shutdown().await;
}

#[test]
fn compute_cycle_outcome_maps_interrupted_cycles_to_failed() {
    let any_failed = AtomicBool::new(false);
    assert_eq!(
        compute_cycle_outcome(false, true, &any_failed),
        CycleOutcome::Failed
    );
}

#[tokio::test]
async fn watch_session_cancellation_drains_in_flight_job_and_keeps_workers_alive() {
    let harness = WatchTestHarness::with_workspace(|workspace_root, started| {
        write_watch_test_workspace(
            workspace_root,
            &cancellation_worker_script(started, workspace_root),
        );
    })
    .await;
    let h1 = harness.worker_manager_handle();
    let selection = harness.selection();

    let cancel = CancellationToken::new();
    cancel.cancel();
    let outcome = harness.run_cycle(&selection, cancel).await;
    assert_eq!(outcome, CycleOutcome::Cancelled);
    assert!(
        !harness.started.exists(),
        "cancel before dispatch must not start worker job"
    );
    assert_eq!(outcome, CycleOutcome::Cancelled);
    assert!(
        !harness.session.worker_manager_is_shutdown(),
        "cancel path must not shut down worker manager"
    );

    let h2 = harness.worker_manager_handle();
    assert!(
        Arc::ptr_eq(&h1, &h2),
        "worker manager Arc should survive cancelled cycle"
    );

    let outcome2 = harness
        .run_cycle(&selection, CancellationToken::new())
        .await;
    assert_eq!(outcome2, CycleOutcome::Success);
    assert!(
        Arc::ptr_eq(&h1, &harness.worker_manager_handle()),
        "worker manager Arc should be reused after cancellation"
    );
    assert!(
        !harness.session.worker_manager_is_shutdown(),
        "worker manager should stay alive after successful reuse"
    );

    harness.session.shutdown().await;
}

#[tokio::test]
async fn watch_no_cache_forces_rerun() {
    let harness = WatchTestHarness::with_workspace(|workspace_root, _started| {
        write_watch_counter_workspace(workspace_root);
    })
    .await;
    let selection = harness.selection();

    let outcome1 = harness
        .run_cycle_with_no_cache(&selection, false, CancellationToken::new())
        .await;
    assert_eq!(outcome1, CycleOutcome::Success);
    assert_eq!(read_counter_file(&harness.workspace_root), 1);

    let outcome2 = harness
        .run_cycle_with_no_cache(&selection, false, CancellationToken::new())
        .await;
    assert_eq!(outcome2, CycleOutcome::Success);
    assert_eq!(
        read_counter_file(&harness.workspace_root),
        1,
        "control: cache-enabled repeat cycle skips (counter stays 1)"
    );

    let outcome3 = harness
        .run_cycle_with_no_cache(&selection, true, CancellationToken::new())
        .await;
    assert_eq!(outcome3, CycleOutcome::Success);
    assert_eq!(
        read_counter_file(&harness.workspace_root),
        2,
        "no_cache=true forces rerun (counter=2)"
    );

    harness.session.shutdown().await;
}
