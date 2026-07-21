use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Full};
use hyper::header::CONTENT_TYPE;
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyperlocal::{UnixClientExt, Uri};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;
use tempfile::TempDir;
use tokio::process::{Child, Command};
use tokio::runtime::Runtime;
use tokio::time::{timeout, MissedTickBehavior};

use super::transport::detect_rc_error;
use super::{Bytes, EmptyResponse, NoopResponse, RcloneError};

pub(super) const RCLONE_READY_TIMEOUT: Duration = Duration::from_secs(3);
pub(super) const RCLONE_READY_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug)]
pub(super) struct State {
    pub(super) daemon: Option<DaemonState>,
}

#[derive(Debug)]
pub(super) struct DaemonState {
    pub(super) _temp_dir: TempDir,
    pub(super) socket_path: PathBuf,
    pub(super) client: Client<hyperlocal::UnixConnector, Full<Bytes>>,
    pub(super) child: Child,
    pub(super) pid: u32,
}

pub(super) async fn spawn_daemon(timeout: Duration) -> Result<DaemonState, RcloneError> {
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
        .arg("--transfers")
        .arg(
            super::env_usize(
                "LUCHTA_SHARED_CACHE_RCLONE_TRANSFERS",
                super::DEFAULT_RCLONE_TRANSFERS,
            )
            .to_string(),
        )
        .arg("--checkers")
        .arg(
            super::env_usize(
                "LUCHTA_SHARED_CACHE_RCLONE_CHECKERS",
                super::DEFAULT_RCLONE_CHECKERS,
            )
            .to_string(),
        )
        .arg("--rc-job-expire-duration")
        .arg(super::env_string(
            "LUCHTA_SHARED_CACHE_RCLONE_JOB_EXPIRE_DURATION",
            super::DEFAULT_RCLONE_JOB_EXPIRE_DURATION,
        ))
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
                "failed to spawn `rclone rcd`: {err}"
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

pub(super) fn quit_and_wait(
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

impl DaemonState {
    pub(super) async fn wait_until_ready(
        &mut self,
        timeout_duration: Duration,
    ) -> Result<(), RcloneError> {
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

    pub(super) async fn post_json<P, T>(
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

    pub(super) async fn wait_for_exit(
        &mut self,
        timeout_duration: Duration,
    ) -> Result<(), RcloneError> {
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

    pub(super) async fn kill_force(&mut self) -> Result<(), RcloneError> {
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
