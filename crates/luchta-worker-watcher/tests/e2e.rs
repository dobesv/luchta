use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use assert_cmd::cargo::cargo_bin;
use serde_json::Value;
use tempfile::TempDir;

const DEBOUNCE_MS: u64 = 50;
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const WATCHER_ARM_SETTLE: Duration = Duration::from_millis(500);
const RESTART_POLL_WAIT: Duration = Duration::from_millis(150);
const RESTART_TIMEOUT: Duration = Duration::from_secs(10);

enum WatchGlob {
    Relative(&'static str),
    Absolute(&'static str),
}

struct Harness {
    _tempdir: TempDir,
    watched_dir: PathBuf,
    ignored_dir: PathBuf,
    child: Child,
    stdin: ChildStdin,
    stdout_rx: mpsc::Receiver<Value>,
    stderr_rx: mpsc::Receiver<String>,
    instance_markers: Vec<String>,
    eof_markers: Vec<String>,
}

impl Harness {
    fn start(mock_delay_ms: u64, watch_glob: WatchGlob) -> Self {
        let tempdir = TempDir::new().expect("tempdir");
        let watched_dir = tempdir.path().join("watched");
        let ignored_dir = tempdir.path().join("ignored");
        std::fs::create_dir_all(&watched_dir).expect("create watched dir");
        std::fs::create_dir_all(&ignored_dir).expect("create ignored dir");
        std::fs::write(tempdir.path().join(".gitignore"), "ignored/\n").expect("write gitignore");

        let watch_arg = match watch_glob {
            WatchGlob::Relative(glob) => glob.to_owned(),
            WatchGlob::Absolute(glob) => tempdir.path().join(glob).display().to_string(),
        };

        let child = std::process::Command::new(cargo_bin("luchta-worker-watcher"))
            .current_dir(tempdir.path())
            .arg("--watch")
            .arg(watch_arg)
            .arg("--debounce-ms")
            .arg(DEBOUNCE_MS.to_string())
            .arg("--")
            .arg(cargo_bin("mock-worker-delegate"))
            .env("MOCK_DELAY_MS", mock_delay_ms.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn watcher binary");

        let mut child = child;
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        let stdout_rx = spawn_stdout_reader(stdout);
        let stderr_rx = spawn_stderr_reader(stderr);

        let mut harness = Self {
            _tempdir: tempdir,
            watched_dir,
            ignored_dir,
            child,
            stdin,
            stdout_rx,
            stderr_rx,
            instance_markers: Vec::new(),
            eof_markers: Vec::new(),
        };
        harness.wait_for_instance_count(1, READ_TIMEOUT);
        harness
    }

    fn start_relative(mock_delay_ms: u64) -> Self {
        Self::start(mock_delay_ms, WatchGlob::Relative("watched/**/*.txt"))
    }

    fn start_absolute(mock_delay_ms: u64) -> Self {
        Self::start(mock_delay_ms, WatchGlob::Absolute("ignored/**/*.txt"))
    }

    fn send_run(&mut self, id: &str, command: &str) {
        let line = serde_json::json!({
            "type": "run",
            "id": id,
            "command": command,
        });
        writeln!(self.stdin, "{line}").expect("write run line");
        self.stdin.flush().expect("flush run line");
    }

    fn read_response(&self, timeout: Duration) -> Value {
        self.stdout_rx
            .recv_timeout(timeout)
            .expect("response within timeout")
    }

    fn touch_matching(&self, name: &str) {
        let path = self.watched_dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create matching parent");
        }
        std::fs::write(path, format!("changed {:?}\n", Instant::now()))
            .expect("touch matching file");
    }

    fn touch_non_matching(&self, name: &str) {
        let path = self.watched_dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create non-matching parent");
        }
        std::fs::write(path, format!("ignored {:?}\n", Instant::now()))
            .expect("touch non-matching file");
    }

    fn touch_gitignored_matching(&self, name: &str) {
        let path = self.ignored_dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create ignored parent");
        }
        std::fs::write(path, format!("gitignored {:?}\n", Instant::now()))
            .expect("touch ignored matching file");
    }

    fn wait_for_instance_count(&mut self, expected: usize, timeout: Duration) {
        self.collect_markers_until(expected, timeout);
        assert!(
            self.instance_count() >= expected,
            "expected at least {expected} instances, got {}",
            self.instance_count()
        );
    }

    fn wait_for_eof_count(&mut self, expected: usize, timeout: Duration) {
        self.wait_for_marker_count(
            |harness| harness.eof_markers.len(),
            expected,
            timeout,
            "EOF marker before timeout",
        );
    }

    fn collect_stderr_for(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match self.stderr_rx.recv_timeout(remaining) {
                Ok(line) => self.record_marker(line),
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    fn instance_count(&self) -> usize {
        self.instance_markers.iter().collect::<HashSet<_>>().len()
    }

    fn shutdown(self) -> (i32, Vec<String>, Vec<String>) {
        let Harness {
            _tempdir,
            watched_dir: _watched_dir,
            ignored_dir: _ignored_dir,
            mut child,
            stdin,
            stdout_rx: _stdout_rx,
            stderr_rx,
            mut instance_markers,
            mut eof_markers,
        } = self;
        drop(stdin);
        let status = child
            .wait_timeout(Duration::from_secs(10))
            .expect("wait for child exit")
            .expect("child exited before timeout");
        collect_markers_from_receiver(
            &stderr_rx,
            Duration::from_secs(1),
            &mut instance_markers,
            &mut eof_markers,
        );
        (
            status.code().expect("exit code present"),
            instance_markers,
            eof_markers,
        )
    }

    fn collect_markers_until(&mut self, expected_instances: usize, timeout: Duration) {
        self.wait_for_marker_count(
            |harness| harness.instance_count(),
            expected_instances,
            timeout,
            "instance marker before timeout",
        );
    }

    fn wait_for_marker_count(
        &mut self,
        count: impl Fn(&Self) -> usize,
        expected: usize,
        timeout: Duration,
        timeout_message: &str,
    ) {
        let deadline = Instant::now() + timeout;
        while count(self) < expected {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .expect(timeout_message);
            let line = self
                .stderr_rx
                .recv_timeout(remaining)
                .expect("stderr marker within timeout");
            self.record_marker(line);
        }
    }

    fn record_marker(&mut self, line: String) {
        if let Some(instance) = line.strip_prefix("INSTANCE:") {
            self.instance_markers.push(instance.to_owned());
        } else if let Some(instance) = line.strip_prefix("EOF:") {
            self.eof_markers.push(instance.to_owned());
        }
    }
}

trait ChildWaitTimeout {
    fn wait_timeout(
        &mut self,
        timeout: Duration,
    ) -> std::io::Result<Option<std::process::ExitStatus>>;
}

impl ChildWaitTimeout for Child {
    fn wait_timeout(
        &mut self,
        timeout: Duration,
    ) -> std::io::Result<Option<std::process::ExitStatus>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(Some(status));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

fn spawn_line_reader<R, T, F>(reader: R, parse: F) -> mpsc::Receiver<T>
where
    R: std::io::Read + Send + 'static,
    T: Send + 'static,
    F: Fn(String) -> T + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let reader = BufReader::new(reader);
        for line in reader.lines() {
            let line = line.expect("reader line");
            if tx.send(parse(line)).is_err() {
                break;
            }
        }
    });
    rx
}

fn spawn_stdout_reader(stdout: ChildStdout) -> mpsc::Receiver<Value> {
    spawn_line_reader(stdout, |line| {
        serde_json::from_str::<Value>(&line).expect("stdout json")
    })
}

fn spawn_stderr_reader(stderr: ChildStderr) -> mpsc::Receiver<String> {
    spawn_line_reader(stderr, |line| line)
}

fn collect_markers_from_receiver(
    stderr_rx: &mpsc::Receiver<String>,
    duration: Duration,
    instance_markers: &mut Vec<String>,
    eof_markers: &mut Vec<String>,
) {
    let deadline = Instant::now() + duration;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match stderr_rx.recv_timeout(remaining) {
            Ok(line) => {
                if let Some(instance) = line.strip_prefix("INSTANCE:") {
                    instance_markers.push(instance.to_owned());
                } else if let Some(instance) = line.strip_prefix("EOF:") {
                    eof_markers.push(instance.to_owned());
                }
            }
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn response_id(value: &Value) -> &str {
    value["id"].as_str().expect("response id string")
}

fn response_exit_code(value: &Value) -> i64 {
    value["exitCode"].as_i64().expect("exit code integer")
}

fn response_type(value: &Value) -> &str {
    value["type"].as_str().expect("response type string")
}

fn assert_done(value: &Value, id: &str) {
    assert_eq!(response_type(value), "done");
    assert_eq!(response_id(value), id);
    assert_eq!(response_exit_code(value), 0);
}

enum TouchTarget {
    Watched,
    Gitignored,
}

fn touch_until_restart_in(
    harness: &mut Harness,
    target: TouchTarget,
    path_name: &str,
    expected_instances: usize,
) {
    let deadline = Instant::now() + RESTART_TIMEOUT;
    while harness.instance_count() < expected_instances {
        match target {
            TouchTarget::Watched => harness.touch_matching(path_name),
            TouchTarget::Gitignored => harness.touch_gitignored_matching(path_name),
        }
        harness.collect_stderr_for(RESTART_POLL_WAIT);
        if Instant::now() >= deadline {
            break;
        }
    }
    assert!(
        harness.instance_count() >= expected_instances,
        "expected restart to reach {expected_instances} instances, got {}",
        harness.instance_count()
    );
}

fn assert_clean_multi_gen_shutdown(harness: Harness, min_gens: usize) {
    let (exit, instances, eof_markers) = harness.shutdown();
    assert_eq!(exit, 0);
    assert!(instances.iter().collect::<HashSet<_>>().len() >= min_gens);
    assert!(eof_markers.len() >= min_gens);
}

#[test]
fn forwards_run_and_done() {
    let mut harness = Harness::start_relative(0);
    harness.send_run("job-1", "build");

    let response = harness.read_response(READ_TIMEOUT);
    assert_done(&response, "job-1");

    let (exit, instances, eof_markers) = harness.shutdown();
    assert_eq!(exit, 0);
    assert_eq!(instances.iter().collect::<HashSet<_>>().len(), 1);
    assert_eq!(eof_markers.len(), 1);
}

#[test]
fn file_change_spawns_new_generation() {
    let mut harness = Harness::start_relative(300);
    harness.send_run("old", "delay:300");
    touch_until_restart_in(&mut harness, TouchTarget::Watched, "nested/restart.txt", 2);
    harness.send_run("new", "build");

    let first = harness.read_response(READ_TIMEOUT);
    let second = harness.read_response(READ_TIMEOUT);
    let ids = [response_id(&first), response_id(&second)];
    assert!(ids.contains(&"old"));
    assert!(ids.contains(&"new"));
    assert_eq!(response_type(&first), "done");
    assert_eq!(response_type(&second), "done");

    assert_clean_multi_gen_shutdown(harness, 2);
}

#[test]
fn old_generation_exits_on_stdin_eof() {
    let mut harness = Harness::start_relative(300);
    harness.send_run("old", "delay:300");
    touch_until_restart_in(&mut harness, TouchTarget::Watched, "handoff/restart.txt", 2);
    harness.send_run("new", "build");

    let _ = harness.read_response(READ_TIMEOUT);
    let _ = harness.read_response(READ_TIMEOUT);
    harness.wait_for_eof_count(1, READ_TIMEOUT);

    assert_clean_multi_gen_shutdown(harness, 2);
}

#[test]
fn multiple_concurrent_draining_generations() {
    let mut harness = Harness::start_relative(300);
    harness.send_run("first", "delay:300");
    touch_until_restart_in(&mut harness, TouchTarget::Watched, "multi/first.txt", 2);
    harness.send_run("second", "delay:300");
    touch_until_restart_in(&mut harness, TouchTarget::Watched, "multi/second.txt", 3);
    harness.send_run("third", "build");

    let mut ids = Vec::new();
    for _ in 0..3 {
        let response = harness.read_response(READ_TIMEOUT);
        assert_eq!(response_type(&response), "done");
        ids.push(response_id(&response).to_owned());
    }
    ids.sort();
    assert_eq!(
        ids,
        vec!["first".to_owned(), "second".to_owned(), "third".to_owned()]
    );

    assert_clean_multi_gen_shutdown(harness, 3);
}

#[test]
fn non_matching_file_change_ignored() {
    let mut harness = Harness::start_relative(0);
    // Watch registration completes slightly after delegate startup. Settle before
    // negative assertion so a missed event cannot falsely pass this test.
    thread::sleep(WATCHER_ARM_SETTLE);
    harness.touch_non_matching("not-a-match.md");
    thread::sleep(RESTART_POLL_WAIT);
    harness.collect_stderr_for(Duration::from_millis(200));
    assert_eq!(harness.instance_count(), 1);

    harness.send_run("job-1", "build");
    let response = harness.read_response(READ_TIMEOUT);
    assert_done(&response, "job-1");

    let (exit, instances, eof_markers) = harness.shutdown();
    assert_eq!(exit, 0);
    assert_eq!(instances.iter().collect::<HashSet<_>>().len(), 1);
    assert_eq!(eof_markers.len(), 1);
}

#[test]
fn gitignored_path_still_watched() {
    let mut harness = Harness::start_absolute(0);
    touch_until_restart_in(
        &mut harness,
        TouchTarget::Gitignored,
        "nested/restart.txt",
        2,
    );
    harness.send_run("job-1", "build");

    let response = harness.read_response(READ_TIMEOUT);
    assert_done(&response, "job-1");

    assert_clean_multi_gen_shutdown(harness, 2);
}
