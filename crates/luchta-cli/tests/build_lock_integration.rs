use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};

use assert_cmd::cargo::cargo_bin;
use assert_fs::{prelude::*, TempDir};

struct KillOnDrop(Option<Child>);

impl KillOnDrop {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    fn child_mut(&mut self) -> &mut Child {
        self.0.as_mut().expect("child already taken")
    }

    fn into_inner(mut self) -> Child {
        self.0.take().expect("child already taken")
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn make_script(temp: &TempDir, name: &str, body: &str) -> assert_fs::fixture::ChildPath {
    let script = temp.child(name);
    script.write_str(body).expect("write script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(script.path(), fs::Permissions::from_mode(0o755))
            .expect("chmod script");
    }
    script
}

fn shell_worker_body(run_body: &str) -> String {
    format!(
        "#!/bin/sh\nwhile IFS= read -r line; do\n  id=$(printf '%s\\n' \"$line\" | sed -n 's/.*\"id\":\"\\([^\"]*\\)\".*/\\1/p')\n  case \"$line\" in\n    *'\"type\":\"resolveTask\"'*)\n      printf '{{\"type\":\"resolved\",\"id\":\"%s\",\"result\":{{\"decision\":\"accept\"}}}}\\n' \"$id\"\n      continue\n      ;;\n  esac\n{run_body}done\n"
    )
}

fn setup_workspace(temp: &TempDir) {
    temp.child("package.json")
        .write_str(
            r#"{
    "name": "root",
    "private": true,
    "workspaces": ["packages/*"]
}"#,
        )
        .expect("write root package.json");

    let package_dir = temp.child("packages/app");
    fs::create_dir_all(package_dir.path()).expect("create package dir");
    package_dir
        .child("package.json")
        .write_str(
            r#"{
    "name": "app",
    "scripts": {
        "build": "echo built-app"
    }
}"#,
        )
        .expect("write package.json");
}

fn build_fixture(temp: &TempDir) -> (PathBuf, PathBuf, PathBuf) {
    setup_workspace(temp);

    let hold = temp.child("hold");
    let release = temp.child("release");
    let finished = temp.child("finished");

    let worker = make_script(
        temp,
        "lock-worker.sh",
        &shell_worker_body(&format!(
            "      printf held > '{}'\n      while [ ! -f '{}' ]; do\n        sleep 0.05\n      done\n      printf done > '{}'\n      printf '{{\"type\":\"done\",\"id\":\"%s\",\"exitCode\":0}}\\n' \"$id\"\n",
            hold.path().display(),
            release.path().display(),
            finished.path().display(),
        )),
    );

    temp.child("luchta-config.sh")
        .write_str(&format!(
            "#!/bin/sh\necho '{{\"concurrency\":{{\"maxWeight\":1}},\"workers\":{{\"fake\":{{\"command\":\"{}\"}}}},\"tasks\":{{\"build\":{{\"worker\":\"fake\"}}}}}}'\n",
            worker.path().display()
        ))
        .expect("write luchta-config.sh");

    (
        hold.path().to_path_buf(),
        release.path().to_path_buf(),
        finished.path().to_path_buf(),
    )
}

fn binary_path() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| cargo_bin("luchta"))
}

fn wait_for_waiting_message(stderr_path: &Path) {
    wait_for(
        Duration::from_secs(30),
        || read_file(stderr_path).contains("Waiting for concurrent build ..."),
        "waiting message from second process",
    );
}

fn read_file(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

fn wait_for<F>(timeout: Duration, mut predicate: F, description: &str)
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {description}");
}

#[test]
fn run_without_contention_succeeds_without_waiting_message() {
    let temp = TempDir::new().expect("create temp dir");
    let (hold_path, release_path, _finished_path) = build_fixture(&temp);
    fs::write(&release_path, "release").expect("pre-release worker");

    let cache_dir = temp.child("cache");
    fs::create_dir_all(cache_dir.path()).expect("create cache dir");

    let output = Command::new(binary_path())
        .arg("run")
        .arg("build")
        .arg("--workspace-root")
        .arg(temp.path())
        .env("LUCHTA_CACHE_DIR", cache_dir.path())
        .env("NO_COLOR", "1")
        .output()
        .expect("run luchta without contention");

    assert!(
        output.status.success(),
        "single run should succeed: {output:?}"
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("Waiting for concurrent build ..."),
        "single run should not print waiting message"
    );
    assert_eq!(read_file(&hold_path), "held", "worker should have started");

    temp.close().expect("cleanup temp dir");
}

#[test]
fn concurrent_runs_wait_for_build_lock_across_processes() {
    let temp = TempDir::new().expect("create temp dir");
    let (hold_path, release_path, finished_path) = build_fixture(&temp);
    let cache_dir = temp.child("cache");
    fs::create_dir_all(cache_dir.path()).expect("create cache dir");
    let second_stderr = temp.child("second.stderr");

    let first = KillOnDrop::new(
        Command::new(binary_path())
            .arg("run")
            .arg("build")
            .arg("--workspace-root")
            .arg(temp.path())
            .env("LUCHTA_CACHE_DIR", cache_dir.path())
            .env("NO_COLOR", "1")
            .spawn()
            .expect("spawn first luchta process"),
    );

    wait_for(
        Duration::from_secs(30),
        || hold_path.exists(),
        "first process to signal lock hold",
    );

    let mut second = KillOnDrop::new(
        Command::new(binary_path())
            .arg("run")
            .arg("build")
            .arg("--workspace-root")
            .arg(temp.path())
            .env("LUCHTA_CACHE_DIR", cache_dir.path())
            .env("NO_COLOR", "1")
            .stderr(fs::File::create(second_stderr.path()).expect("create second stderr log"))
            .spawn()
            .expect("spawn second luchta process"),
    );

    wait_for_waiting_message(second_stderr.path());
    assert!(
        second.child_mut().try_wait().expect("poll second process").is_none(),
        "second process should still be blocked after waiting message"
    );

    fs::write(&release_path, "release").expect("release first worker");

    let first_output = first
        .into_inner()
        .wait_with_output()
        .expect("collect first process output");
    assert!(
        first_output.status.success(),
        "first process should succeed, stderr: {}",
        String::from_utf8_lossy(&first_output.stderr)
    );

    let second_status = second.into_inner().wait().expect("wait for second process");
    assert!(second_status.success(), "second process should succeed");
    assert_eq!(
        read_file(&finished_path),
        "done",
        "worker should finish after release"
    );

    temp.close().expect("cleanup temp dir");
}
