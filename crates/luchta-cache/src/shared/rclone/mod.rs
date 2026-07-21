use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

mod async_job;
mod daemon;
mod transport;

use async_job::{poll_async_job, submit_async_job, DEFAULT_RCLONE_SUBMIT_TIMEOUT};
use bytes::Bytes;
use daemon::{quit_and_wait, spawn_daemon, State};
use hyper_util::client::legacy::Client;
use hyperlocal::UnixClientExt;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::runtime::{Builder, Runtime};
use transport::{
    build_upload_multipart_body, multipart_boundary, post_json_with_client,
    post_multipart_with_client, url_encode_query_value,
};

pub const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_RCLONE_CONCURRENCY: usize = 16;
const DEFAULT_RCLONE_TRANSFERS: usize = 4;
const DEFAULT_RCLONE_CHECKERS: usize = 8;
const DEFAULT_RCLONE_JOB_EXPIRE_DURATION: &str = "10m";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Entry {
    #[serde(rename = "Path")]
    pub path: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "IsDir")]
    pub is_dir: bool,
    #[serde(rename = "Size")]
    pub size: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct StatInfo {
    #[serde(rename = "Path")]
    pub path: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "IsDir")]
    pub is_dir: bool,
    #[serde(rename = "Size")]
    pub size: i64,
}

pub struct CopyFile<'a> {
    pub src_fs: &'a str,
    pub src_remote: &'a str,
    pub dst_fs: &'a str,
    pub dst_remote: &'a str,
}

pub struct UploadFile<'a> {
    pub fs: &'a str,
    pub remote_dir: &'a str,
    pub file_name: &'a str,
    pub bytes: &'a [u8],
}

#[derive(Debug, thiserror::Error)]
pub enum RcloneError {
    #[error("remote unavailable: {reason}")]
    RemoteUnavailable { reason: String },
    #[error("rclone operation timed out after {timeout:?}")]
    Timeout { timeout: Duration },
    #[error("rclone process failed: {reason}")]
    Process { reason: String },
    #[error("rclone rc request failed: {reason}")]
    Request { reason: String },
    #[error("rclone rc returned status {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("rclone rc returned error: {message}")]
    Rc { message: String },
    #[error("rclone rc response decode failed: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("rclone io failed: {0}")]
    Io(#[from] io::Error),
}

impl RcloneError {
    fn remote_unavailable(reason: impl Into<String>) -> Self {
        Self::RemoteUnavailable {
            reason: reason.into(),
        }
    }
}

#[derive(Debug)]
struct OpLimiter {
    state: Mutex<OpLimiterState>,
    ready: Condvar,
}

#[derive(Debug)]
struct OpLimiterState {
    max_in_flight: usize,
    in_flight: usize,
}

#[derive(Debug)]
struct OpPermit<'a> {
    limiter: &'a OpLimiter,
}

impl OpLimiter {
    fn new(max_in_flight: usize) -> Self {
        Self {
            state: Mutex::new(OpLimiterState {
                max_in_flight,
                in_flight: 0,
            }),
            ready: Condvar::new(),
        }
    }

    fn acquire(&self) -> OpPermit<'_> {
        let mut state = self.state.lock().unwrap();
        while state.in_flight >= state.max_in_flight {
            state = self.ready.wait(state).unwrap();
        }
        state.in_flight += 1;
        OpPermit { limiter: self }
    }
}

impl Drop for OpPermit<'_> {
    fn drop(&mut self) {
        let mut state = self.limiter.state.lock().unwrap();
        state.in_flight -= 1;
        self.limiter.ready.notify_one();
    }
}

#[derive(Debug)]
pub struct RcloneRcd {
    runtime: Option<Runtime>,
    state: Mutex<State>,
    default_timeout: Duration,
    submit_timeout: Duration,
    limiter: OpLimiter,
}

impl RcloneRcd {
    pub fn new(default_timeout: Duration) -> Result<Self, RcloneError> {
        Self::with_concurrency_limit(default_timeout, DEFAULT_RCLONE_CONCURRENCY)
    }

    pub fn with_concurrency_limit(
        default_timeout: Duration,
        max_in_flight: usize,
    ) -> Result<Self, RcloneError> {
        let runtime = Builder::new_multi_thread().enable_all().build()?;
        Ok(Self {
            runtime: Some(runtime),
            state: Mutex::new(State { daemon: None }),
            default_timeout,
            submit_timeout: env_duration_secs(
                "LUCHTA_SHARED_CACHE_RCLONE_SUBMIT_TIMEOUT",
                DEFAULT_RCLONE_SUBMIT_TIMEOUT,
            ),
            limiter: OpLimiter::new(max_in_flight.max(1)),
        })
    }

    fn runtime(&self) -> &Runtime {
        self.runtime
            .as_ref()
            .expect("rclone runtime used after teardown")
    }

    pub fn with_default_timeout() -> Result<Self, RcloneError> {
        Self::new(DEFAULT_OPERATION_TIMEOUT)
    }

    pub fn default_timeout(&self) -> Duration {
        self.default_timeout
    }

    pub fn child_pid(&self) -> Option<u32> {
        self.state
            .lock()
            .ok()?
            .daemon
            .as_ref()
            .map(|daemon| daemon.pid)
    }

    pub fn socket_path(&self) -> Option<PathBuf> {
        self.state
            .lock()
            .ok()?
            .daemon
            .as_ref()
            .map(|daemon| daemon.socket_path.clone())
    }

    pub fn noop(&self, timeout: Duration) -> Result<(), RcloneError> {
        self.call_async::<_, NoopResponse>("rc/noop", json!({}), timeout)
            .map(|_| ())
    }

    pub fn copyfile(&self, copy: CopyFile<'_>, timeout: Duration) -> Result<(), RcloneError> {
        self.call_async::<_, EmptyResponse>(
            "operations/copyfile",
            json!({
                "srcFs": copy.src_fs,
                "srcRemote": copy.src_remote,
                "dstFs": copy.dst_fs,
                "dstRemote": copy.dst_remote,
            }),
            timeout,
        )
        .map(|_| ())
    }

    pub fn upload_bytes(
        &self,
        upload: UploadFile<'_>,
        timeout: Duration,
    ) -> Result<(), RcloneError> {
        let _permit = self.limiter.acquire();
        let runtime = self.runtime();
        let socket_path = self.ensure_daemon_socket(timeout)?;
        let query = format!(
            "fs={}&remote={}",
            url_encode_query_value(upload.fs),
            url_encode_query_value(upload.remote_dir)
        );
        let boundary = multipart_boundary();
        let body = build_upload_multipart_body(&boundary, upload.file_name, upload.bytes);
        std::thread::scope(|scope| {
            scope
                .spawn(move || -> Result<(), RcloneError> {
                    let client = Client::unix();
                    runtime.block_on(post_multipart_with_client(
                        &client,
                        &socket_path,
                        &query,
                        &boundary,
                        body,
                        timeout,
                    ))
                })
                .join()
                .map_err(|_| RcloneError::Process {
                    reason: "rclone upload thread panicked".to_string(),
                })?
        })
    }

    pub fn copy_dir(
        &self,
        src_fs: &str,
        dst_fs: &str,
        timeout: Duration,
    ) -> Result<(), RcloneError> {
        let _permit = self.limiter.acquire();
        self.call::<_, EmptyResponse>(
            "sync/copy",
            json!({
                "srcFs": src_fs,
                "dstFs": dst_fs,
            }),
            timeout,
        )
        .map(|_| ())
    }

    pub fn list(
        &self,
        fs: &str,
        remote: &str,
        timeout: Duration,
    ) -> Result<Vec<Entry>, RcloneError> {
        let _permit = self.limiter.acquire();
        let response: ListResponse = self.call(
            "operations/list",
            json!({
                "fs": fs,
                "remote": remote,
            }),
            timeout,
        )?;
        Ok(response.list.unwrap_or_default())
    }

    pub fn deletefile(&self, fs: &str, remote: &str, timeout: Duration) -> Result<(), RcloneError> {
        self.call_async::<_, EmptyResponse>(
            "operations/deletefile",
            json!({
                "fs": fs,
                "remote": remote,
            }),
            timeout,
        )
        .map(|_| ())
    }

    pub fn stat(
        &self,
        fs: &str,
        remote: &str,
        timeout: Duration,
    ) -> Result<Option<StatInfo>, RcloneError> {
        let response: StatResponse = self.call_async(
            "operations/stat",
            json!({
                "fs": fs,
                "remote": remote,
            }),
            timeout,
        )?;
        Ok(response.item)
    }

    pub fn shutdown(&self, timeout: Duration) -> Result<(), RcloneError> {
        let mut state = self.lock_state()?;
        let Some(daemon) = state.daemon.take() else {
            return Ok(());
        };
        let runtime = self.runtime();
        std::thread::scope(|scope| {
            scope
                .spawn(move || quit_and_wait(runtime, daemon, timeout))
                .join()
                .map_err(|_| RcloneError::Process {
                    reason: "rclone shutdown thread panicked".to_string(),
                })?
        })
    }

    fn call<P, T>(&self, endpoint: &str, payload: P, timeout: Duration) -> Result<T, RcloneError>
    where
        P: Serialize,
        T: DeserializeOwned,
    {
        let payload = serde_json::to_value(payload)?;
        let runtime = self.runtime();
        let socket_path = self.ensure_daemon_socket(timeout)?;
        let response = std::thread::scope(|scope| {
            scope
                .spawn(move || -> Result<Value, RcloneError> {
                    let client = Client::unix();
                    runtime.block_on(post_json_with_client(
                        &client,
                        &socket_path,
                        endpoint,
                        payload,
                        timeout,
                    ))
                })
                .join()
                .map_err(|_| RcloneError::Process {
                    reason: "rclone call thread panicked".to_string(),
                })?
        })?;
        serde_json::from_value(response).map_err(RcloneError::from)
    }

    fn call_async<P, T>(
        &self,
        endpoint: &str,
        payload: P,
        timeout: Duration,
    ) -> Result<T, RcloneError>
    where
        P: Serialize,
        T: DeserializeOwned + Send,
    {
        let payload = serde_json::to_value(payload)?;
        let runtime = self.runtime();
        let socket_path = self.ensure_daemon_socket(timeout)?;
        let submit_timeout = self.submit_timeout;
        std::thread::scope(|scope| {
            scope
                .spawn(move || -> Result<T, RcloneError> {
                    let client = Client::unix();
                    runtime.block_on(async move {
                        let submitted = {
                            let _permit = self.limiter.acquire();
                            submit_async_job(
                                &client,
                                &socket_path,
                                endpoint,
                                payload,
                                submit_timeout,
                            )
                            .await?
                        };
                        poll_async_job::<T>(&client, &socket_path, submitted, timeout).await
                    })
                })
                .join()
                .map_err(|_| RcloneError::Process {
                    reason: "rclone async call thread panicked".to_string(),
                })?
        })
    }

    fn ensure_daemon_socket(&self, timeout: Duration) -> Result<PathBuf, RcloneError> {
        let runtime = self.runtime();
        let mut state = self.lock_state()?;
        if state.daemon.is_none() {
            state.daemon = Some(std::thread::scope(|scope| {
                scope
                    .spawn(move || runtime.block_on(spawn_daemon(timeout)))
                    .join()
                    .map_err(|_| RcloneError::Process {
                        reason: "rclone spawn thread panicked".to_string(),
                    })?
            })?);
        }
        Ok(state
            .daemon
            .as_ref()
            .expect("daemon initialized")
            .socket_path
            .clone())
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, State>, RcloneError> {
        self.state.lock().map_err(|_| RcloneError::Process {
            reason: "rclone state mutex poisoned".to_string(),
        })
    }
}

impl Drop for RcloneRcd {
    fn drop(&mut self) {
        let daemon = self
            .state
            .get_mut()
            .ok()
            .and_then(|state| state.daemon.take());
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        let handle = std::thread::spawn(move || {
            if let Some(mut daemon) = daemon {
                runtime.block_on(async move {
                    let _ = daemon
                        .post_json::<_, EmptyResponse>(
                            "core/quit",
                            json!({}),
                            Duration::from_secs(1),
                        )
                        .await;
                    let _ = daemon.wait_for_exit(Duration::from_secs(1)).await;
                    let _ = daemon.kill_force().await;
                });
            }
            drop(runtime);
        });
        let _ = handle.join();
    }
}

fn env_duration_secs(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(default)
}

pub(super) fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map(|value| value.max(1))
        .unwrap_or(default)
}

pub(super) fn env_string(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

#[derive(Debug, Deserialize)]
struct NoopResponse {
    #[serde(default)]
    _status: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct EmptyResponse {}

#[derive(Debug, Deserialize)]
struct ListResponse {
    #[serde(rename = "list")]
    list: Option<Vec<Entry>>,
}

#[derive(Debug, Deserialize)]
pub(super) struct StatResponse {
    #[serde(rename = "item")]
    item: Option<StatInfo>,
}

impl fmt::Display for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        transport::display_entry(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use luchta_test_support::require_nextest;
    use std::fs;
    use std::process::Stdio;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::thread;
    use tempfile::TempDir;

    fn should_run_rclone_test() -> bool {
        match std::env::var("LUCHTA_TEST_RCLONE") {
            Ok(value) => value != "0" && !value.eq_ignore_ascii_case("false"),
            Err(_) => std::process::Command::new("rclone")
                .arg("version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false),
        }
    }

    #[test]
    fn op_limiter_caps_max_in_flight_and_unblocks_after_release() {
        let limiter = Arc::new(OpLimiter::new(2));
        let current = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::new();

        for _ in 0..6 {
            let limiter = Arc::clone(&limiter);
            let current = Arc::clone(&current);
            let max_seen = Arc::clone(&max_seen);
            threads.push(thread::spawn(move || {
                let _permit = limiter.acquire();
                let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                loop {
                    let observed = max_seen.load(Ordering::SeqCst);
                    if now <= observed
                        || max_seen
                            .compare_exchange(observed, now, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok()
                    {
                        break;
                    }
                }
                thread::sleep(Duration::from_millis(20));
                current.fetch_sub(1, Ordering::SeqCst);
            }));
        }

        for thread in threads {
            thread.join().unwrap();
        }
        assert!(max_seen.load(Ordering::SeqCst) <= 2);

        let blocker = Arc::new(OpLimiter::new(1));
        let held = blocker.acquire();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (passed_tx, passed_rx) = mpsc::channel();
        let proceeded = Arc::new(AtomicBool::new(false));
        thread::scope(|scope| {
            let blocker_in_thread = Arc::clone(&blocker);
            let proceeded_in_thread = Arc::clone(&proceeded);
            scope.spawn(move || {
                ready_tx.send(()).unwrap();
                let _permit = blocker_in_thread.acquire();
                proceeded_in_thread.store(true, Ordering::SeqCst);
                passed_tx.send(()).unwrap();
            });
            ready_rx.recv().unwrap();
            assert!(passed_rx.recv_timeout(Duration::from_millis(50)).is_err());
            assert!(!proceeded.load(Ordering::SeqCst));
            drop(held);
            passed_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert!(proceeded.load(Ordering::SeqCst));
        });
    }

    #[tokio::test(flavor = "current_thread")]
    async fn noop_from_runtime_context_returns_result_instead_of_panicking() {
        let timeout = Duration::from_millis(50);
        let rclone = RcloneRcd::new(timeout).unwrap();

        let result = std::panic::catch_unwind(|| rclone.noop(timeout));

        assert!(result.is_ok(), "noop panicked inside runtime context");
        let call_result = result.unwrap();
        if !should_run_rclone_test() {
            assert!(call_result.is_err(), "missing rclone should return Err");
        }
    }

    #[test]
    fn remote_unavailable_when_rclone_missing() {
        require_nextest();
        let rclone = RcloneRcd::new(Duration::from_secs(1)).unwrap();
        let original_path = std::env::var_os("PATH");
        std::env::set_var("PATH", "");
        let result = rclone.noop(Duration::from_millis(200));
        if let Some(path) = original_path {
            std::env::set_var("PATH", path);
        }
        match result {
            Err(RcloneError::RemoteUnavailable { .. }) => {}
            other => panic!("expected remote unavailable error, got {other:?}"),
        }
    }

    #[test]
    fn rclone_rcd_lifecycle_and_file_ops() {
        if !should_run_rclone_test() {
            eprintln!("skipping rclone integration test; rclone not on PATH or LUCHTA_TEST_RCLONE disabled");
            return;
        }

        let source_dir = TempDir::new().unwrap();
        let remote_dir = TempDir::new().unwrap();
        let restore_dir = TempDir::new().unwrap();
        let source_file = source_dir.path().join("input.txt");
        fs::write(&source_file, "forge").unwrap();

        let timeout = Duration::from_secs(10);
        let rclone = RcloneRcd::new(timeout).unwrap();
        rclone.noop(timeout).unwrap();
        let pid = rclone.child_pid().unwrap();

        let source_fs = format!(":local:{}", source_dir.path().display());
        let remote_fs = format!(":local:{}", remote_dir.path().join("nested").display());
        let restore_fs = format!(":local:{}", restore_dir.path().display());
        let source_remote = "input.txt";
        let remote_file = "out.txt";
        let restore_remote = "restored.txt";

        rclone
            .copyfile(
                CopyFile {
                    src_fs: &source_fs,
                    src_remote: source_remote,
                    dst_fs: &remote_fs,
                    dst_remote: remote_file,
                },
                timeout,
            )
            .unwrap();

        let stat = rclone
            .stat(&remote_fs, remote_file, timeout)
            .unwrap()
            .unwrap();
        assert_eq!(stat.path, remote_file);

        let list = rclone.list(&remote_fs, "", timeout).unwrap();
        assert!(list.iter().any(|entry| entry.path == remote_file));

        rclone
            .copyfile(
                CopyFile {
                    src_fs: &remote_fs,
                    src_remote: remote_file,
                    dst_fs: &restore_fs,
                    dst_remote: restore_remote,
                },
                timeout,
            )
            .unwrap();
        assert_eq!(
            fs::read_to_string(restore_dir.path().join(restore_remote)).unwrap(),
            "forge"
        );

        rclone.deletefile(&remote_fs, remote_file, timeout).unwrap();
        assert!(rclone
            .stat(&remote_fs, remote_file, timeout)
            .unwrap()
            .is_none());

        drop(rclone);
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "rclone pid {pid} still alive after drop"
        );
    }

    #[test]
    fn upload_bytes_writes_exact_file_contents() {
        if !should_run_rclone_test() {
            eprintln!("skipping rclone upload integration test; rclone not on PATH or LUCHTA_TEST_RCLONE disabled");
            return;
        }

        let timeout = Duration::from_secs(10);
        let remote_dir = TempDir::new().unwrap();
        let rclone = RcloneRcd::new(timeout).unwrap();
        rclone.noop(timeout).unwrap();
        let remote_fs = format!(
            ":local:{}",
            remote_dir.path().join("snapshots/commit").display()
        );
        let file_name = "part-01.bincode";
        let payload = b"snapshot-bytes\nwith-second-line";

        rclone
            .upload_bytes(
                UploadFile {
                    fs: &remote_fs,
                    remote_dir: "",
                    file_name,
                    bytes: payload,
                },
                timeout,
            )
            .unwrap();

        let remote_path = remote_dir.path().join("snapshots/commit").join(file_name);
        assert_eq!(fs::read(&remote_path).unwrap(), payload);
        let stat = rclone
            .stat(&remote_fs, file_name, timeout)
            .unwrap()
            .unwrap();
        assert_eq!(stat.path, file_name);
    }

    #[test]
    fn upload_bytes_into_nested_remote_dir_writes_to_joined_path() {
        if !should_run_rclone_test() {
            eprintln!("skipping rclone nested upload integration test; rclone not on PATH or LUCHTA_TEST_RCLONE disabled");
            return;
        }

        let timeout = Duration::from_secs(10);
        let remote_dir = TempDir::new().unwrap();
        let rclone = RcloneRcd::new(timeout).unwrap();
        rclone.noop(timeout).unwrap();
        let remote_fs = format!(":local:{}", remote_dir.path().display());
        let payload = b"nested-payload";

        rclone
            .upload_bytes(
                UploadFile {
                    fs: &remote_fs,
                    remote_dir: "snapshots/commit",
                    file_name: "shard.bincode",
                    bytes: payload,
                },
                timeout,
            )
            .unwrap();

        let written = remote_dir
            .path()
            .join("snapshots/commit")
            .join("shard.bincode");
        assert_eq!(fs::read(&written).unwrap(), payload);
    }
}
