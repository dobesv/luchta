use std::fmt;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::{BufMut, Bytes, BytesMut};
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
pub const DEFAULT_RCLONE_CONCURRENCY: usize = 16;

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

/// Destination for an `operations/uploadfile` call. The bytes land at
/// `<fs>/<remote_dir>/<file_name>`; `file_name` must be a plain file name (the
/// multipart part's `filename`).
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
    /// Owned so it can be torn down off the async context on Drop (see below).
    /// Always `Some` until `Drop`/`shutdown` takes it.
    runtime: Option<Runtime>,
    state: Mutex<State>,
    default_timeout: Duration,
    limiter: OpLimiter,
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
            limiter: OpLimiter::new(max_in_flight.max(1)),
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
        let _permit = self.limiter.acquire();
        self.call::<_, NoopResponse>("rc/noop", json!({}), timeout)
            .map(|_| ())
    }

    /// `src_remote` and `dst_remote` must be paths relative to `src_fs` and `dst_fs`.
    /// Callers own fs/root split, e.g. `:local:/abs/dir` + `file.txt`, not `:local:` + `/abs/dir/file.txt`.
    pub fn copyfile(&self, copy: CopyFile<'_>, timeout: Duration) -> Result<(), RcloneError> {
        let _permit = self.limiter.acquire();
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

    /// Streams `bytes` straight to the remote via `operations/uploadfile`
    /// (multipart/form-data) instead of staging a local temp file and calling
    /// `operations/copyfile`. This avoids a race where the temp source file is
    /// removed before rclone stats it, which surfaced as a spurious
    /// `404 object not found` (on the SOURCE) under concurrent stores.
    ///
    /// The file lands at `<fs>/<remote_dir>/<file_name>`. `file_name` must be a
    /// plain file name (no path separators); rclone takes it from the multipart
    /// part's `filename`.
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

    /// `remote` must be path relative to `fs`.
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

    /// `remote` must be path relative to `fs`.
    pub fn deletefile(&self, fs: &str, remote: &str, timeout: Duration) -> Result<(), RcloneError> {
        let _permit = self.limiter.acquire();
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
        let _permit = self.limiter.acquire();
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

    /// Spawns the rclone daemon once (under the state lock) if needed and returns
    /// a clone of its unix socket path. The lock is held ONLY for the spawn and
    /// path read, never for in-flight requests, so concurrent RC calls run
    /// against the daemon without serializing behind this mutex.
    fn ensure_daemon_socket(&self, timeout: Duration) -> Result<PathBuf, RcloneError> {
        let runtime = self.runtime();
        let mut state = self.lock_state()?;
        if state.daemon.is_none() {
            state.daemon = Some(std::thread::scope(|scope| {
                scope
                    .spawn(move || runtime.block_on(RcloneRcd::spawn_daemon(timeout)))
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

async fn post_json_with_client<P, T>(
    client: &Client<hyperlocal::UnixConnector, Full<Bytes>>,
    socket_path: &std::path::Path,
    endpoint: &str,
    payload: P,
    timeout_duration: Duration,
) -> Result<T, RcloneError>
where
    P: Serialize,
    T: DeserializeOwned,
{
    let uri: hyper::Uri = Uri::new(socket_path, &format!("/{endpoint}")).into();
    let body = serde_json::to_vec(&payload)?;
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body)))
        .map_err(|err| RcloneError::Request {
            reason: err.to_string(),
        })?;

    let response = timeout(timeout_duration, client.request(request))
        .await
        .map_err(|_| RcloneError::Timeout {
            timeout: timeout_duration,
        })
        .and_then(|result| {
            result.map_err(|err| RcloneError::Request {
                reason: err.to_string(),
            })
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
    if !status.is_success() {
        let body = String::from_utf8_lossy(&bytes).into_owned();
        return Err(RcloneError::HttpStatus {
            status: status.as_u16(),
            body,
        });
    }

    let value: Value = serde_json::from_slice(&bytes)?;
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return Err(RcloneError::Rc {
            message: error.to_string(),
        });
    }
    serde_json::from_value(value).map_err(RcloneError::from)
}

/// POSTs a multipart/form-data body to `operations/uploadfile` (with the
/// `fs`/`remote` query string already built) over the rcd unix socket. Mirrors
/// `post_json_with_client` for status/error handling but carries the file as a
/// streamed multipart part rather than a JSON payload.
async fn post_multipart_with_client(
    client: &Client<hyperlocal::UnixConnector, Full<Bytes>>,
    socket_path: &std::path::Path,
    query: &str,
    boundary: &str,
    body: Bytes,
    timeout_duration: Duration,
) -> Result<(), RcloneError> {
    let path = format!("/operations/uploadfile?{query}");
    let uri: hyper::Uri = Uri::new(socket_path, &path).into();
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(
            CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Full::new(body))
        .map_err(|err| RcloneError::Request {
            reason: err.to_string(),
        })?;

    let response = timeout(timeout_duration, client.request(request))
        .await
        .map_err(|_| RcloneError::Timeout {
            timeout: timeout_duration,
        })
        .and_then(|result| {
            result.map_err(|err| RcloneError::Request {
                reason: err.to_string(),
            })
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
    if !status.is_success() {
        let body = String::from_utf8_lossy(&bytes).into_owned();
        return Err(RcloneError::HttpStatus {
            status: status.as_u16(),
            body,
        });
    }

    // uploadfile returns 200 with an (empty) JSON object on success, but may
    // still carry an `error` field; surface it like the JSON path does, reusing
    // the shared `detect_rc_error` helper.
    if !bytes.is_empty() {
        let body = String::from_utf8_lossy(&bytes);
        if let Some(message) = detect_rc_error(&body)? {
            return Err(RcloneError::Rc { message });
        }
    }
    Ok(())
}

fn build_upload_multipart_body(boundary: &str, file_name: &str, bytes: &[u8]) -> Bytes {
    let mut body =
        BytesMut::with_capacity(boundary.len() * 2 + file_name.len() + bytes.len() + 160);
    body.put(format!("--{boundary}\r\n").as_bytes());
    body.put(
        format!(
            "Content-Disposition: form-data; name=\"file0\"; filename=\"{}\"\r\n",
            escape_multipart_header_value(file_name)
        )
        .as_bytes(),
    );
    body.put(b"Content-Type: application/octet-stream\r\n\r\n".as_slice());
    body.put(bytes);
    body.put(format!("\r\n--{boundary}--\r\n").as_bytes());
    body.freeze()
}

fn escape_multipart_header_value(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn multipart_boundary() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0_u128, |duration| duration.as_nanos());
    format!("luchta-rclone-upload-{nanos:x}")
}

fn url_encode_query_value(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(char::from(byte));
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
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

    #[test]
    fn op_limiter_caps_max_in_flight_and_unblocks_after_release() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::mpsc;
        use std::sync::Arc;
        use std::thread;

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

        // We always pass a plain file name; the destination subdir is supplied
        // via `remote_dir` (the `remote` query param), which rclone joins under
        // `fs`. Verify the bytes land at `<fs>/<remote_dir>/<file_name>` exactly,
        // including creating the nested dir on a fresh prefix (the empty-prefix
        // case that previously surfaced spurious 404s on the copyfile path).
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
