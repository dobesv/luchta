use super::*;
use crate::watch::session::WatchSession;
use crate::watch::watcher::{WatchBatch, WatcherHandle};
use luchta_workspace::PackageNode;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};

pub(super) const TEST_TIMEOUT: Duration = Duration::from_secs(15);
pub(super) const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Write a workspace with a blocking shell worker.
/// Worker appends a newline to `.run-marker` on every job.
/// First job BLOCKS until `.release-1` sentinel appears.
/// Subsequent jobs run free (don't block).
pub(super) fn blocking_workspace_config(worker_script_path: &std::path::Path) -> String {
    format!(
        r##"#!/bin/sh
echo '{{"concurrency":{{"maxWeight":4}},"workers":{{"fake":{{"command":"{}"}}}},"tasks":{{"build":{{"worker":"fake"}}}}}}'
"##,
        worker_script_path.display()
    )
}

pub(super) fn write_blocking_workspace(workspace_root: &std::path::Path) {
    std::fs::create_dir_all(workspace_root.join("packages/app")).expect("create package dir");
    std::fs::write(
        workspace_root.join("package.json"),
        r#"{"name": "root", "private": true, "workspaces": ["packages/*"]}"#,
    )
    .expect("write root package.json");
    std::fs::write(
        workspace_root.join("packages/app/package.json"),
        r#"{"name": "app", "version": "1.0.0", "scripts": {"build": "echo build"}}"#,
    )
    .expect("write app package.json");

    // Worker script:
    // - On first job, wait for .release-1 sentinel (blocking)
    // - Appends to .run-marker on every job
    // - Jobs 2+ run free
    let marker_file = workspace_root.join(".run-marker");
    let release_1 = workspace_root.join(".release-1");
    let job_count = workspace_root.join(".job-count");
    let worker_script = format!(
        r##"#!/bin/sh
job_count=0
while IFS= read -r line; do
  case "$line" in
*'"type":"resolveTask"'*)
  id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  printf '{{"type":"resolved","id":"%s","result":{{"decision":"accept"}}}}\n' "$id"
  ;;
*'"type":"run"'*)
  id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  job_count=$((job_count + 1))
  echo "$job_count" > '{}'
  if [ "$job_count" -eq 1 ]; then
    while [ ! -f '{}' ]; do sleep 0.01; done
  fi
  echo "" >> '{}'
  printf '{{"type":"done","id":"%s","success":true,"exitCode":0}}\n' "$id"
  ;;
  esac
done
"##,
        job_count.display(),
        release_1.display(),
        marker_file.display(),
    );
    let worker_script_path = workspace_root.join("fake-worker.sh");
    std::fs::write(&worker_script_path, &worker_script).expect("write worker script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&worker_script_path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod worker script");
    }

    let config = blocking_workspace_config(&worker_script_path);
    std::fs::write(workspace_root.join("luchta-config.sh"), &config).expect("write config");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            workspace_root.join("luchta-config.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod config");
    }
}

pub(super) fn write_failing_initial_cycle_workspace(workspace_root: &std::path::Path) {
    std::fs::create_dir_all(workspace_root.join("packages/app/src")).expect("create package dir");
    std::fs::write(
        workspace_root.join("package.json"),
        r#"{"name": "root", "private": true, "workspaces": ["packages/*"]}"#,
    )
    .expect("write root package.json");
    std::fs::write(
        workspace_root.join("packages/app/package.json"),
        r#"{"name": "app", "version": "1.0.0", "scripts": {"build": "echo build"}}"#,
    )
    .expect("write app package.json");
    std::fs::write(
        workspace_root.join("packages/app/src/lib.rs"),
        "// initial\n",
    )
    .expect("write app source");

    let run_git = |args: &[&str]| {
        let ok = Command::new("git")
            .args(args)
            .current_dir(workspace_root)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git {:?} failed in test workspace", args);
    };
    run_git(&["init"]);
    run_git(&["add", "-A"]);

    let marker_file = workspace_root.join(".run-marker");
    let job_count = workspace_root.join(".job-count");
    let worker_script = format!(
        r##"#!/bin/sh
job_count=0
while IFS= read -r line; do
  case "$line" in
*'"type":"resolveTask"'*)
  id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  printf '{{"type":"resolved","id":"%s","result":{{"decision":"accept"}}}}\n' "$id"
  ;;
*'"type":"run"'*)
  id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  job_count=$((job_count + 1))
  echo "$job_count" > '{job_count}'
  echo "" >> '{marker_file}'
  if [ "$job_count" -eq 1 ]; then
    printf '{{"type":"done","id":"%s","success":false,"exitCode":2}}\n' "$id"
  else
    printf '{{"type":"done","id":"%s","success":true,"exitCode":0}}\n' "$id"
  fi
  ;;
  esac
done
"##,
        job_count = job_count.display(),
        marker_file = marker_file.display(),
    );
    let worker_script_path = workspace_root.join("fake-worker.sh");
    std::fs::write(&worker_script_path, &worker_script).expect("write worker script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&worker_script_path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod worker script");
    }

    let config = format!(
        r##"#!/bin/sh
echo '{{"concurrency":{{"maxWeight":4}},"workers":{{"fake":{{"command":"{}"}}}},"tasks":{{"build":{{"worker":"fake","inputs":["src/**"]}}}}}}'
"##,
        worker_script_path.display()
    );
    std::fs::write(workspace_root.join("luchta-config.sh"), &config).expect("write config");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            workspace_root.join("luchta-config.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod config");
    }
}

pub(super) fn write_two_package_dependency_watch_workspace(workspace_root: &std::path::Path) {
    write_two_package_dependency_watch_workspace_with_api_source(workspace_root, "fail\n");
}

pub(super) fn write_two_package_dependency_success_workspace(workspace_root: &std::path::Path) {
    write_two_package_dependency_watch_workspace_with_api_source(workspace_root, "pass\n");
}

fn write_two_package_dependency_watch_workspace_with_api_source(
    workspace_root: &std::path::Path,
    api_source: &str,
) {
    std::fs::create_dir_all(workspace_root.join("packages/api/src")).expect("create api dir");
    std::fs::create_dir_all(workspace_root.join("packages/app/src")).expect("create app dir");
    std::fs::write(
        workspace_root.join("package.json"),
        r#"{"name": "root", "private": true, "workspaces": ["packages/*"]}"#,
    )
    .expect("write root package.json");
    std::fs::write(
        workspace_root.join("packages/api/package.json"),
        r#"{"name": "api", "version": "1.0.0", "scripts": {"build": "echo build"}}"#,
    )
    .expect("write api package.json");
    std::fs::write(
        workspace_root.join("packages/app/package.json"),
        r#"{"name": "app", "version": "1.0.0", "dependencies": {"api": "1.0.0"}, "scripts": {"build": "echo build"}}"#,
    )
    .expect("write app package.json");
    std::fs::write(workspace_root.join("packages/api/src/lib.rs"), api_source)
        .expect("write api src");
    std::fs::write(workspace_root.join("packages/app/src/lib.rs"), "app\n").expect("write app src");

    let run_git = |args: &[&str]| {
        let ok = Command::new("git")
            .args(args)
            .current_dir(workspace_root)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git {:?} failed in test workspace", args);
    };
    run_git(&["init"]);
    run_git(&["add", "-A"]);

    let job_count = workspace_root.join(".job-count");
    let marker_api = workspace_root.join(".run-marker-api");
    let marker_app = workspace_root.join(".run-marker-app");
    let worker_script = format!(
        r##"#!/bin/sh
job_count=0
while IFS= read -r line; do
  case "$line" in
  *'"type":"resolveTask"'*)
    id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
    printf '{{"type":"resolved","id":"%s","result":{{"decision":"accept"}}}}\n' "$id"
    ;;
  *'"type":"run"'*)
    id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
    package=${{id%%#*}}
    job_count=$((job_count + 1))
    echo "$job_count" > '{job_count}'
    case "$package" in
      api)
        echo "$job_count:$id" >> '{marker_api}'
        if grep -q 'fail' '{api_src}'; then
          printf '{{"type":"done","id":"%s","success":false,"exitCode":2}}\n' "$id"
        else
          printf '{{"type":"done","id":"%s","success":true,"exitCode":0}}\n' "$id"
        fi
        ;;
      app)
        echo "$job_count:$id" >> '{marker_app}'
        printf '{{"type":"done","id":"%s","success":true,"exitCode":0}}\n' "$id"
        ;;
      *)
        printf '{{"type":"done","id":"%s","success":true,"exitCode":0}}\n' "$id"
        ;;
    esac
    ;;
  esac
done
"##,
        job_count = job_count.display(),
        marker_api = marker_api.display(),
        marker_app = marker_app.display(),
        api_src = workspace_root.join("packages/api/src/lib.rs").display(),
    );
    let worker_script_path = workspace_root.join("fake-worker.sh");
    std::fs::write(&worker_script_path, &worker_script).expect("write worker script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&worker_script_path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod worker script");
    }

    let config = format!(
        r##"#!/bin/sh
echo '{{"concurrency":{{"maxWeight":4}},"workers":{{"fake":{{"command":"{}"}}}},"tasks":{{"build":{{"worker":"fake","dependsOn":["^build"],"inputs":["src/**"]}}}}}}'
"##,
        worker_script_path.display()
    );
    std::fs::write(workspace_root.join("luchta-config.sh"), &config).expect("write config");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            workspace_root.join("luchta-config.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod config");
    }
}

pub(super) fn write_two_package_lockfile_workspace(workspace_root: &std::path::Path) {
    std::fs::create_dir_all(workspace_root.join("packages/a")).expect("create package a dir");
    std::fs::create_dir_all(workspace_root.join("packages/b")).expect("create package b dir");
    std::fs::write(
        workspace_root.join("package.json"),
        r#"{"name": "root", "private": true, "workspaces": ["packages/*"]}"#,
    )
    .expect("write root package.json");
    std::fs::write(
        workspace_root.join("packages/a/package.json"),
        r#"{"name": "@scope/a", "version": "1.0.0", "dependencies": {"left-pad": "^1.0.0"}, "scripts": {"build": "echo build"}}"#,
    )
    .expect("write package a package.json");
    std::fs::write(
        workspace_root.join("packages/b/package.json"),
        r#"{"name": "b", "version": "1.0.0", "dependencies": {"lodash": "^4.0.0"}, "scripts": {"build": "echo build"}}"#,
    )
    .expect("write package b package.json");
    std::fs::write(
        workspace_root.join("yarn.lock"),
        two_package_lockfile_contents("1.0.0", "4.0.0"),
    )
    .expect("write yarn.lock");

    let release_1 = workspace_root.join(".release-1");
    let job_count = workspace_root.join(".job-count");
    let worker_script = format!(
        r##"#!/bin/sh
job_count=0
while IFS= read -r line; do
  case "$line" in
  *'"type":"resolveTask"'*)
    id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
    printf '{{"type":"resolved","id":"%s","result":{{"decision":"accept"}}}}\n' "$id"
    ;;
  *'"type":"run"'*)
    id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
    package=${{id%%#*}}
    job_count=$((job_count + 1))
    echo "$job_count" > '{job_count}'
    case "$package" in
      @scope/a) marker='{marker_a}' ;;
      b) marker='{marker_b}' ;;
      *) marker='{marker_unknown}' ;;
    esac
    echo "$id" >> "$marker"
    if [ "$job_count" -eq 1 ]; then
      while [ ! -f '{release_1}' ]; do sleep 0.01; done
    fi
    printf '{{"type":"done","id":"%s","success":true,"exitCode":0}}\n' "$id"
    ;;
  esac
done
"##,
        job_count = job_count.display(),
        marker_a = workspace_root.join(".run-marker-a").display(),
        marker_b = workspace_root.join(".run-marker-b").display(),
        marker_unknown = workspace_root.join(".run-marker-unknown").display(),
        release_1 = release_1.display(),
    );
    let worker_script_path = workspace_root.join("fake-worker.sh");
    std::fs::write(&worker_script_path, &worker_script).expect("write worker script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&worker_script_path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod worker script");
    }

    let config = blocking_workspace_config(&worker_script_path);
    std::fs::write(workspace_root.join("luchta-config.sh"), &config).expect("write config");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            workspace_root.join("luchta-config.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod config");
    }
}

pub(super) fn read_marker_count(workspace_root: &std::path::Path) -> usize {
    let marker = workspace_root.join(".run-marker");
    std::fs::read_to_string(&marker)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

pub(super) fn read_job_count(workspace_root: &std::path::Path) -> usize {
    std::fs::read_to_string(workspace_root.join(".job-count"))
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(0)
}

pub(super) struct E2eHarness {
    pub(super) _temp_dir: tempfile::TempDir,
    pub(super) workspace_root: PathBuf,
    session: Arc<WatchSession>,
    changes_tx: mpsc::Sender<WatchBatch>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

pub(super) enum PackageMutation<'a> {
    Add {
        relative_path: &'a str,
        name: &'a str,
    },
    Remove {
        relative_path: &'a str,
    },
    Rename {
        from: &'a str,
        to: &'a str,
    },
    Malformed {
        relative_path: &'a str,
    },
}

impl E2eHarness {
    pub(super) async fn start() -> Self {
        Self::start_with_max_weight_override(None).await
    }

    pub(super) async fn start_with_max_weight_override(max_weight_override: Option<u32>) -> Self {
        Self::start_with_workspace_and_max_weight_override(
            write_blocking_workspace,
            max_weight_override,
        )
        .await
    }

    pub(super) async fn start_two_package_lockfile() -> Self {
        Self::start_with_workspace(write_two_package_lockfile_workspace).await
    }

    pub(super) async fn start_two_package_dependency_watch() -> Self {
        Self::start_with_workspace(write_two_package_dependency_watch_workspace).await
    }

    pub(super) async fn start_two_package_dependency_success_watch() -> Self {
        Self::start_with_workspace(write_two_package_dependency_success_workspace).await
    }

    pub(super) async fn start_failing_initial_cycle() -> Self {
        Self::start_with_workspace(write_failing_initial_cycle_workspace).await
    }

    pub(super) async fn start_with_workspace(writer: fn(&std::path::Path)) -> Self {
        Self::start_with_workspace_and_max_weight_override(writer, None).await
    }

    pub(super) async fn start_with_workspace_and_max_weight_override(
        writer: fn(&std::path::Path),
        max_weight_override: Option<u32>,
    ) -> Self {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let workspace_root = temp_dir.path().canonicalize().expect("canonicalize");
        writer(&workspace_root);

        let session = Arc::new(
            WatchSession::new(&workspace_root, max_weight_override)
                .await
                .expect("create watch session")
                .expect("session should not be None"),
        );
        let watcher_handle = WatcherHandle::noop();
        let (changes_tx, changes_rx) = mpsc::channel(32);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let task_session = Arc::clone(&session);
        let handle = tokio::spawn(async move {
            let shutdown_future = async move {
                let _ = shutdown_rx.await;
                Ok::<(), std::io::Error>(())
            };
            let force_shutdown = async { std::future::pending::<std::io::Result<()>>().await };

            run_watch_until(
                WatchInputs {
                    session: task_session,
                    watcher_handle,
                    changes_rx,
                    selection: OwnedSelection {
                        requested_tasks: vec!["build".to_string()],
                        packages: vec![],
                        top_level: false,
                    },
                    config: WatchRunConfig {
                        output: OutputMode::Default,
                        continue_on_failure: false,
                        memory_pressure: crate::run::MemoryPressureConfig {
                            usage: None,
                            free: None,
                        },
                        show_changed_files: false,
                    },
                },
                shutdown_future,
                force_shutdown,
            )
            .await
            .expect("watch loop");
        });

        Self {
            _temp_dir: temp_dir,
            workspace_root,
            session,
            changes_tx,
            shutdown_tx: Some(shutdown_tx),
            handle: Some(handle),
        }
    }

    pub(super) async fn wait_until(
        &self,
        timeout: Duration,
        msg: impl FnOnce() -> String,
        cond: impl Fn() -> bool,
    ) {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if cond() {
                return;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        panic!("{}", msg());
    }

    pub(super) async fn stays_for(&self, duration: Duration, cond: impl Fn() -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + duration;
        while tokio::time::Instant::now() < deadline {
            if !cond() {
                return false;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        cond()
    }

    pub(super) async fn send_batch(&self, changed_paths: HashSet<PathBuf>, structural: bool) {
        self.changes_tx
            .send(WatchBatch {
                changed_paths,
                structural,
            })
            .await
            .expect("send watch batch");
    }

    pub(super) async fn wait_for_jobs(&self, target: usize) {
        self.wait_until(
            Duration::from_secs(10),
            || format!("timed out waiting for job count {target}"),
            || read_job_count(&self.workspace_root) >= target,
        )
        .await;
    }

    pub(super) async fn wait_for_markers(&self, target: usize) {
        self.wait_until(
            Duration::from_secs(10),
            || format!("timed out waiting for marker count {target}"),
            || count_lines(&self.workspace_root.join(".run-marker")) >= target,
        )
        .await;
    }

    pub(super) fn session_package_paths(&self) -> BTreeSet<PathBuf> {
        self.session
            .current_package_paths()
            .into_iter()
            .filter(|path| path != &self.workspace_root)
            .collect()
    }

    pub(super) fn session_package_names(&self) -> Vec<String> {
        let mut names = self
            .session
            .current_package_nodes()
            .into_iter()
            .filter(|package| package.path != self.workspace_root)
            .map(|package| package.name.as_ref().to_string())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    pub(super) async fn wait_for_session_package_paths(&self, expected: &BTreeSet<PathBuf>) {
        self.wait_until(
            Duration::from_secs(10),
            || format!("timed out waiting for session package paths {expected:?}"),
            || &self.session_package_paths() == expected,
        )
        .await;
    }

    pub(super) async fn wait_for_session_package_names(&self, expected: &[&str]) {
        let mut expected_names = expected
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>();
        expected_names.sort();
        self.wait_until(
            Duration::from_secs(10),
            || format!("timed out waiting for session package names {expected_names:?}"),
            || self.session_package_names() == expected_names,
        )
        .await;
    }

    pub(super) fn current_max_weight(&self) -> u32 {
        self.session.run_context_for_test().max_weight
    }

    pub(super) fn rebuild_generation(&self) -> u64 {
        self.session.rebuild_generation()
    }

    pub(super) async fn send_package_change(&self) {
        let app_path = self.workspace_root.join("packages/app/src/lib.rs");
        std::fs::create_dir_all(app_path.parent().expect("app parent")).ok();
        std::fs::write(&app_path, "// change").expect("write change");
        self.send_batch(HashSet::from([app_path]), false).await;
    }

    pub(super) async fn send_outside_change(&self) {
        let outside_path = self.workspace_root.join("README.md");
        std::fs::write(&outside_path, "change").expect("write change");
        self.send_batch(HashSet::from([outside_path]), false).await;
    }

    pub(super) async fn send_structural_change_for_path(&self, changed_path: PathBuf) {
        self.send_batch(HashSet::from([changed_path]), true).await;
    }

    pub(super) async fn send_lockfile_change(&self) {
        let lockfile_path = self
            .workspace_root
            .join("yarn.lock")
            .canonicalize()
            .expect("canonicalize lockfile path");
        self.send_batch(HashSet::from([lockfile_path]), false).await;
    }

    pub(super) fn config_path(&self) -> PathBuf {
        self.workspace_root.join("luchta-config.sh")
    }

    pub(super) fn worker_script_path(&self) -> PathBuf {
        self.workspace_root.join("fake-worker.sh")
    }

    pub(super) fn rewrite_config(&self, contents: &str) {
        std::fs::write(self.config_path(), contents).expect("rewrite config");
    }

    pub(super) async fn mutate_packages_and_signal(&self, mutation: PackageMutation<'_>) {
        let changed_path = match mutation {
            PackageMutation::Add {
                relative_path,
                name,
            } => {
                let package_dir = self.workspace_root.join(relative_path);
                self.add_package(relative_path, name);
                package_dir.join("package.json")
            }
            PackageMutation::Remove { relative_path } => {
                let package_dir = self.workspace_root.join(relative_path);
                let changed_path = package_dir.join("package.json");
                self.remove_package(relative_path);
                changed_path
            }
            PackageMutation::Rename { from, to } => {
                let to_path = self.workspace_root.join(to);
                self.rename_package(from, to);
                to_path.join("package.json")
            }
            PackageMutation::Malformed { relative_path } => {
                let package_dir = self.workspace_root.join(relative_path);
                self.write_malformed_package(relative_path);
                package_dir.join("package.json")
            }
        };
        self.send_structural_change_for_path(changed_path).await;
    }

    /// Run standard structural watcher flow: settle first cycle, apply mutation,
    /// wait for session package names to match, assert on-disk package names match,
    /// then assert no extra worker cycle within 500ms.
    pub(super) async fn run_structural_case(
        &self,
        mutation: PackageMutation<'_>,
        expected_names: &[&str],
        no_extra_cycle_message: &str,
    ) {
        self.wait_for_jobs(1).await;
        self.release_first_cycle();
        self.wait_for_markers(1).await;
        self.mutate_packages_and_signal(mutation).await;
        self.wait_for_session_package_names(expected_names).await;

        let expected_names = expected_names
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            self.package_names(),
            expected_names,
            "expected structural mutation to rebuild package graph"
        );
        assert!(
            self.stays_for_markers(1, Duration::from_millis(500)).await,
            "{no_extra_cycle_message}, got {}",
            read_marker_count(&self.workspace_root)
        );
    }

    pub(super) fn add_package(&self, relative_path: &str, name: &str) {
        let package_dir = self.workspace_root.join(relative_path);
        std::fs::create_dir_all(&package_dir).expect("create package dir");
        std::fs::write(
            package_dir.join("package.json"),
            format!(
                r#"{{"name": "{name}", "version": "1.0.0", "scripts": {{"build": "echo build"}}}}"#
            ),
        )
        .expect("write package json");
    }

    pub(super) fn remove_package(&self, relative_path: &str) {
        let package_dir = self.workspace_root.join(relative_path);
        if package_dir.exists() {
            std::fs::remove_dir_all(package_dir).expect("remove package dir");
        }
    }

    pub(super) fn rename_package(&self, from: &str, to: &str) {
        let from_path = self.workspace_root.join(from);
        let to_path = self.workspace_root.join(to);
        if let Some(parent) = to_path.parent() {
            std::fs::create_dir_all(parent).expect("create renamed package parent");
        }
        std::fs::rename(from_path, to_path).expect("rename package dir");
    }

    pub(super) fn write_malformed_package(&self, relative_path: &str) {
        let package_dir = self.workspace_root.join(relative_path);
        std::fs::create_dir_all(&package_dir).expect("create malformed package dir");
        std::fs::write(package_dir.join("package.json"), "{")
            .expect("write malformed package json");
    }

    pub(super) fn package_paths(&self) -> BTreeSet<PathBuf> {
        discover_package_paths(&self.workspace_root).expect("discover package paths")
    }

    pub(super) fn package_names(&self) -> Vec<String> {
        discover_package_names(&self.workspace_root).expect("discover package names")
    }

    pub(super) fn worker_manager_handle(&self) -> Arc<luchta_engine::WorkerManager> {
        self.session.worker_manager_handle()
    }

    pub(super) fn marker_counts_by_package(&self) -> HashMap<String, usize> {
        HashMap::from([
            (
                "a".to_string(),
                read_marker_count_for(&self.workspace_root, "a"),
            ),
            (
                "b".to_string(),
                read_marker_count_for(&self.workspace_root, "b"),
            ),
        ])
    }

    pub(super) fn worker_manager_is_shutdown(&self) -> bool {
        self.session.worker_manager_is_shutdown()
    }

    pub(super) async fn stays_for_markers(&self, expected: usize, duration: Duration) -> bool {
        self.stays_for(duration, || {
            read_marker_count(&self.workspace_root) == expected
        })
        .await
    }

    pub(super) async fn stays_for_jobs(&self, expected: usize, duration: Duration) -> bool {
        self.stays_for(duration, || {
            read_job_count(&self.workspace_root) == expected
        })
        .await
    }

    pub(super) fn release_first_cycle(&self) {
        std::fs::write(self.workspace_root.join(".release-1"), "").expect("release first cycle");
    }

    pub(super) async fn shutdown(mut self) {
        let _ = self.shutdown_tx.take().expect("shutdown tx").send(());
        timeout(TEST_TIMEOUT, self.handle.take().expect("watch handle"))
            .await
            .expect("watch loop timeout")
            .expect("watch loop join");
    }
}

pub(super) fn count_lines(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

fn discover_packages(workspace_root: &Path) -> Result<Vec<PackageNode>, String> {
    let root_package = workspace_root
        .canonicalize()
        .map_err(|error| error.to_string())?;
    luchta_workspace::YarnWorkspace::new(&root_package)
        .discover()
        .map(|packages| {
            packages
                .into_iter()
                .filter(|package| package.path != root_package)
                .collect()
        })
        .map_err(|error| error.to_string())
}

pub(super) fn discover_package_paths(workspace_root: &Path) -> Result<BTreeSet<PathBuf>, String> {
    Ok(discover_packages(workspace_root)?
        .into_iter()
        .map(|package| package.path.canonicalize().expect("canonical package root"))
        .collect())
}

pub(super) fn discover_package_names(workspace_root: &Path) -> Result<Vec<String>, String> {
    let mut names = discover_packages(workspace_root)?
        .into_iter()
        .map(|package| package.name.as_ref().to_string())
        .collect::<Vec<_>>();
    names.sort();
    Ok(names)
}

pub(super) fn read_marker_count_for(workspace_root: &std::path::Path, package_key: &str) -> usize {
    count_lines(&workspace_root.join(format!(".run-marker-{package_key}")))
}

pub(super) fn read_marker_entries_for(
    workspace_root: &std::path::Path,
    package_key: &str,
) -> Vec<String> {
    std::fs::read_to_string(workspace_root.join(format!(".run-marker-{package_key}")))
        .map(|s| s.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

pub(super) fn two_package_lockfile_contents(
    left_pad_version: &str,
    lodash_version: &str,
) -> String {
    [
        "# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.".to_string(),
        "# yarn lockfile v1".to_string(),
        String::new(),
        "left-pad@^1.0.0:".to_string(),
        format!("  version \"{left_pad_version}\""),
        format!(
            "  resolved \"https://registry.yarnpkg.com/left-pad/-/left-pad-{left_pad_version}.tgz#leftpad\""
        ),
        "  integrity sha512-leftpad".to_string(),
        String::new(),
        "lodash@^4.0.0:".to_string(),
        format!("  version \"{lodash_version}\""),
        format!(
            "  resolved \"https://registry.yarnpkg.com/lodash/-/lodash-{lodash_version}.tgz#lodash\""
        ),
        "  integrity sha512-lodash".to_string(),
        String::new(),
    ]
    .join("\n")
}
