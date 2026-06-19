use std::fmt;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::header::CONTENT_TYPE;
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyperlocal::{UnixClientExt, Uri};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::process::{Child, Command};
use tokio::runtime::{Builder, Runtime};
use tokio::time::{timeout, MissedTickBehavior};

const RCLONE_READY_TIMEOUT: Duration = Duration::from_secs(3);
const RCLONE_READY_POLL_INTERVAL: Duration = Duration::from_millis(100);
pub const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

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

/// Source/destination for an `operations/copyfile` call.
///
/// `src_remote`/`dst_remote` are paths relative to `src_fs`/`dst_fs`.
pub struct CopyFile<'a> {
    pub src_fs: &'a str,
    pub src_remote: &'a str,
    pub dst_fs: &'a str,
    pub dst_remote: &'a str,
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
pub struct RcloneRcd {
    /// Owned so it can be torn down off the async context on Drop (see below).
    /// Always `Some` until `Drop`/`shutdown` takes it.
    runtime: Option<Runtime>,
    state: Mutex<State>,
    default_timeout: Duration,
}

#[derive(Debug)]
struct State {
    daemon: Option<DaemonState>,
}

#[derive(Debug)]
struct DaemonState {
    _temp_dir: TempDir,
    socket_path: PathBuf,
    client: Client<hyperlocal::UnixConnector, Full<Bytes>>,
    child: Child,
    pid: u32,
}

impl RcloneRcd {
    pub fn new(default_timeout: Duration) -> Result<Self, RcloneError> {
        let runtime = Builder::new_current_thread().enable_all().build()?;
        Ok(Self {
            runtime: Some(runtime),
            state: Mutex::new(State { daemon: None }),
            default_timeout,
        })
    }

    /// The owned tokio runtime. Present for the whole lifetime except during the
    /// final teardown in `shutdown`/`drop`.
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
        self.call::<_, NoopResponse>("rc/noop", json!({}), timeout)
            .map(|_| ())
    }

    /// `src_remote` and `dst_remote` must be paths relative to `src_fs` and `dst_fs`.
    /// Callers own fs/root split, e.g. `:local:/abs/dir` + `file.txt`, not `:local:` + `/abs/dir/file.txt`.
    pub fn copyfile(&self, copy: CopyFile<'_>, timeout: Duration) -> Result<(), RcloneError> {
        self.call::<_, EmptyResponse>(
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

    /// `remote` must be path relative to `fs`.
    pub fn list(
        &self,
        fs: &str,
        remote: &str,
        timeout: Duration,
    ) -> Result<Vec<Entry>, RcloneError> {
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

    /// `remote` must be path relative to `fs`.
    pub fn deletefile(&self, fs: &str, remote: &str, timeout: Duration) -> Result<(), RcloneError> {
        self.call::<_, EmptyResponse>(
            "operations/deletefile",
            json!({
                "fs": fs,
                "remote": remote,
            }),
            timeout,
        )
        .map(|_| ())
    }

    /// `remote` must be path relative to `fs`.
    pub fn stat(
        &self,
        fs: &str,
        remote: &str,
        timeout: Duration,
    ) -> Result<Option<StatInfo>, RcloneError> {
        let response: StatResponse = self.call(
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
        // Run the blocking teardown on a dedicated OS thread. `block_on` must
        // never be called from within an async context (e.g. when the owning
        // `SharedCache` Arc is dropped inside the build's tokio runtime), so we
        // borrow the runtime into a scoped thread instead of blocking here.
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
        let mut state = self.lock_state()?;
        let runtime = self.runtime();
        call_on_runtime_thread(&mut state, runtime, endpoint, payload, timeout)
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, State>, RcloneError> {
        self.state.lock().map_err(|_| RcloneError::Process {
            reason: "rclone state mutex poisoned".to_string(),
        })
    }

    async fn spawn_daemon(timeout: Duration) -> Result<DaemonState, RcloneError> {
        let temp_dir = TempDir::new()?;
        let socket_path = temp_dir.path().join("rclone.rcd.sock");
        let socket_addr = format!("unix://{}", socket_path.display());

        let mut command = Command::new("rclone");
        command
            .arg("rcd")
            .arg("--rc-addr")
            .arg(&socket_addr)
            .arg("--rc-no-auth")
            .arg("--log-format")
            .arg("date,time")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());

        let child = match command.spawn() {
            Ok(child) => child,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Err(RcloneError::remote_unavailable(
                    "`rclone` not found on PATH",
                ));
            }
            Err(err) => {
                return Err(RcloneError::remote_unavailable(format!(
                    "failed to spawn `rclone rcd`: {err}`"
                )));
            }
        };

        let pid = child.id().ok_or_else(|| RcloneError::Process {
            reason: "spawned rclone missing child pid".to_string(),
        })?;
        let client = Client::unix();
        let mut daemon = DaemonState {
            _temp_dir: temp_dir,
            socket_path,
            client,
            child,
            pid,
        };
        daemon
            .wait_until_ready(timeout.min(RCLONE_READY_TIMEOUT))
            .await?;
        Ok(daemon)
    }
}

fn call_on_runtime_thread<P, T>(
    state: &mut State,
    runtime: &Runtime,
    endpoint: &str,
    payload: P,
    timeout: Duration,
) -> Result<T, RcloneError>
where
    P: Serialize,
    T: DeserializeOwned,
{
    let payload = serde_json::to_value(payload)?;
    let response = std::thread::scope(|scope| {
        scope
            .spawn(move || -> Result<Value, RcloneError> {
                if state.daemon.is_none() {
                    state.daemon = Some(runtime.block_on(RcloneRcd::spawn_daemon(timeout))?);
                }
                let daemon = state.daemon.as_mut().expect("daemon initialized");
                runtime.block_on(daemon.post_json::<_, Value>(endpoint, payload, timeout))
            })
            .join()
            .map_err(|_| RcloneError::Process {
                reason: "rclone call thread panicked".to_string(),
            })?
    })?;
    serde_json::from_value(response).map_err(RcloneError::from)
}

/// Sends `core/quit` and waits for the daemon to exit, on the given runtime.
/// Intended to run on a dedicated OS thread, never inside an async context.
fn quit_and_wait(
    runtime: &Runtime,
    mut daemon: DaemonState,
    timeout: Duration,
) -> Result<(), RcloneError> {
    runtime.block_on(async move {
        let quit_result = daemon
            .post_json::<_, EmptyResponse>("core/quit", json!({}), timeout)
            .await;
        let wait_result = daemon.wait_for_exit(timeout).await;
        match (quit_result, wait_result) {
            (Err(err), _) => Err(err),
            (_, Err(err)) => Err(err),
            _ => Ok(()),
        }
    })
}

impl Drop for RcloneRcd {
    fn drop(&mut self) {
        let daemon = self
            .state
            .get_mut()
            .ok()
            .and_then(|state| state.daemon.take());
        // Move the runtime OUT of `self` so it is dropped on the teardown thread
        // below, NOT here. Dropping a tokio runtime from within an async context
        // (e.g. when this Arc is released inside the build's runtime) panics
        // with "Cannot drop a runtime in a context where blocking is not
        // allowed". Running the teardown — and the runtime's own drop — on a
        // dedicated OS thread sidesteps that entirely.
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
            // `runtime` (and its background threads) is dropped here, on this
            // dedicated thread, outside any async context.
            drop(runtime);
        });
        // Join so the daemon is fully reaped before this drop returns; ignore a
        // panic in the teardown thread (best-effort cleanup, never fatal).
        let _ = handle.join();
    }
}

impl DaemonState {
    async fn wait_until_ready(&mut self, timeout_duration: Duration) -> Result<(), RcloneError> {
        let deadline = Instant::now() + timeout_duration;
        let mut ticker = tokio::time::interval(RCLONE_READY_POLL_INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            match self
                .post_json::<_, NoopResponse>("rc/noop", json!({}), RCLONE_READY_POLL_INTERVAL)
                .await
            {
                Ok(_) => return Ok(()),
                Err(err @ RcloneError::RemoteUnavailable { .. }) => return Err(err),
                Err(err @ RcloneError::Process { .. }) => return Err(err),
                Err(_) => {}
            }

            if Instant::now() >= deadline {
                self.kill_force().await?;
                return Err(RcloneError::Timeout {
                    timeout: timeout_duration,
                });
            }
            ticker.tick().await;
        }
    }

    async fn post_json<P, T>(
        &mut self,
        endpoint: &str,
        payload: P,
        timeout_duration: Duration,
    ) -> Result<T, RcloneError>
    where
        P: Serialize,
        T: DeserializeOwned,
    {
        self.ensure_running()?;
        let body = serde_json::to_vec(&payload)?;
        let path = format!("/{endpoint}");
        let uri: hyper::Uri = Uri::new(&self.socket_path, &path).into();
        let request = Request::post(uri)
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body)))
            .map_err(|err| RcloneError::Request {
                reason: err.to_string(),
            })?;

        let response = timeout(timeout_duration, self.client.request(request))
            .await
            .map_err(|_| RcloneError::Timeout {
                timeout: timeout_duration,
            })?
            .map_err(|err| RcloneError::Request {
                reason: err.to_string(),
            })?;

        let status = response.status();
        let bytes = timeout(timeout_duration, response.into_body().collect())
            .await
            .map_err(|_| RcloneError::Timeout {
                timeout: timeout_duration,
            })?
            .map_err(|err| RcloneError::Request {
                reason: err.to_string(),
            })?
            .to_bytes();
        let text = String::from_utf8_lossy(&bytes).into_owned();

        if !status.is_success() {
            return Err(RcloneError::HttpStatus {
                status: status.as_u16(),
                body: text,
            });
        }

        let rc_error = detect_rc_error(&text)?;
        if let Some(message) = rc_error {
            return Err(RcloneError::Rc { message });
        }

        serde_json::from_slice(&bytes).map_err(RcloneError::Decode)
    }

    async fn wait_for_exit(&mut self, timeout_duration: Duration) -> Result<(), RcloneError> {
        match timeout(timeout_duration, self.child.wait()).await {
            Ok(Ok(status)) if status.success() => Ok(()),
            Ok(Ok(status)) => Err(RcloneError::Process {
                reason: format!("rclone exited with status {status}"),
            }),
            Ok(Err(err)) => Err(RcloneError::Io(err)),
            Err(_) => {
                self.kill_force().await?;
                Err(RcloneError::Timeout {
                    timeout: timeout_duration,
                })
            }
        }
    }

    async fn kill_force(&mut self) -> Result<(), RcloneError> {
        match self.child.try_wait() {
            Ok(Some(_)) => Ok(()),
            Ok(None) => {
                self.child.start_kill()?;
                let _ = self.child.wait().await;
                Ok(())
            }
            Err(err) => Err(RcloneError::Io(err)),
        }
    }

    fn ensure_running(&mut self) -> Result<(), RcloneError> {
        match self.child.try_wait() {
            Ok(Some(status)) => Err(RcloneError::Process {
                reason: format!("rclone exited before request with status {status}"),
            }),
            Ok(None) => Ok(()),
            Err(err) => Err(RcloneError::Io(err)),
        }
    }
}

fn detect_rc_error(body: &str) -> Result<Option<String>, RcloneError> {
    let value: Value = serde_json::from_str(body)?;
    let Some(error) = value.get("error") else {
        return Ok(None);
    };
    if error.is_null() {
        return Ok(None);
    }
    Ok(Some(match error {
        Value::String(message) => message.clone(),
        other => other.to_string(),
    }))
}

#[derive(Debug, Deserialize)]
struct NoopResponse {
    #[serde(default)]
    _status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EmptyResponse {}

#[derive(Debug, Deserialize)]
struct ListResponse {
    #[serde(rename = "list")]
    list: Option<Vec<Entry>>,
}

#[derive(Debug, Deserialize)]
struct StatResponse {
    #[serde(rename = "item")]
    item: Option<StatInfo>,
}

impl fmt::Display for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
}
