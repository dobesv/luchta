//! Executable configuration loader for `luchta-config.*`.

use std::{
    collections::VecDeque,
    env, fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::Arc,
    time::Duration,
};

use miette::{bail, miette, Context, IntoDiagnostic, Result};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStderr, ChildStdout, Command},
    sync::Mutex,
    task::JoinHandle,
    time::{sleep, timeout},
};

pub use luchta_types::LuchtaConfig;

const EXECUTE_CONFIG_ETXTBSY_RETRIES: usize = 10;
const EXECUTE_CONFIG_ETXTBSY_BACKOFF: Duration = Duration::from_millis(5);
/// Default timeout for executing `luchta-config.*` scripts.
///
/// Override with `LUCHTA_CONFIG_TIMEOUT_SECS=<seconds>`. Invalid, zero, or unset values fall back
/// to this default so config loading stays predictable.
const DEFAULT_CONFIG_TIMEOUT: Duration = Duration::from_secs(30);
const CONFIG_TIMEOUT_ENV_VAR: &str = "LUCHTA_CONFIG_TIMEOUT_SECS";
const STDERR_TAIL_MAX_LINES: usize = 20;
const STDERR_TAIL_MAX_BYTES: usize = 2048;
#[cfg(unix)]
/// Linux errno for "text file busy". Used only on Unix spawn-retry path.
const ETXTBSY_ERRNO: i32 = 26;

/// Load config by discovering and executing `luchta-config.*` in workspace root.
pub async fn load_config(workspace_root: impl AsRef<Path>) -> Result<LuchtaConfig> {
    load_config_with_timeout(workspace_root, config_timeout()).await
}

async fn load_config_with_timeout(
    workspace_root: impl AsRef<Path>,
    timeout_duration: Duration,
) -> Result<LuchtaConfig> {
    let workspace_root = workspace_root.as_ref();
    let config_path = discover_config_script(workspace_root)?;
    ensure_shebang(&config_path)?;
    let stdout = execute_config_script(workspace_root, &config_path, timeout_duration).await?;
    let jd = &mut serde_json::Deserializer::from_slice(&stdout);
    let config: LuchtaConfig = serde_path_to_error::deserialize(jd).map_err(|error| {
        let stdout_str = String::from_utf8_lossy(&stdout);
        format_config_error(&stdout_str, &config_path, &error)
    })?;

    Ok(config)
}

fn format_config_error(
    stdout: &str,
    config_path: &Path,
    error: &serde_path_to_error::Error<serde_json::Error>,
) -> miette::Report {
    let json_path = error.path().to_string();
    let inner = error.inner();
    let line = inner.line();
    let column = inner.column();

    let lines: Vec<&str> = stdout.lines().collect();
    let error_line_idx = line.saturating_sub(1).min(lines.len().saturating_sub(1));
    let start = error_line_idx.saturating_sub(2);
    let end = error_line_idx.saturating_add(3).min(lines.len());

    let mut excerpt = String::new();
    if lines.is_empty() {
        excerpt.push_str("(config output was empty)\n");
    } else {
        for (i, line_text) in lines[start..end].iter().enumerate() {
            let line_num = start + i + 1;
            let marker = if line_num == line { " --> " } else { "     " };
            excerpt.push_str(&format!("{marker}{line_num:>4} | {line_text}\n"));
        }
    }

    if json_path.is_empty() || json_path == "." {
        miette!(
            "failed to parse config from {}\n\nerror: {inner}\n  at line {line}, column {column}\n\n{excerpt}",
            config_path.display()
        )
    } else {
        miette!(
            "failed to parse config from {}\n\nerror: {inner}\n  at {json_path} (line {line}, column {column})\n\n{excerpt}",
            config_path.display()
        )
    }
}

fn discover_config_script(workspace_root: &Path) -> Result<PathBuf> {
    let mut matches = Vec::new();
    for entry in fs::read_dir(workspace_root)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read workspace root {}", workspace_root.display()))?
    {
        let entry = entry.into_diagnostic()?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with("luchta-config.") {
            matches.push(path);
        }
    }
    matches.sort();

    match matches.len() {
        0 => bail!("no config file found (expected a file matching luchta-config.*)"),
        1 => Ok(matches.remove(0)),
        _ => bail!(
            "multiple config files found: {:?} — remove all but one",
            matches
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
        ),
    }
}

fn ensure_shebang(config_path: &Path) -> Result<()> {
    let mut file = fs::File::open(config_path)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to open config file {}", config_path.display()))?;
    let mut first_two_bytes = [0_u8; 2];
    let bytes_read = file.read(&mut first_two_bytes).into_diagnostic()?;
    if bytes_read < 2 || first_two_bytes != *b"#!" {
        bail!(
            "config file `{}` has no shebang line — add a shebang (e.g. #!/usr/bin/env node) so luchta knows how to execute it",
            config_path.display()
        );
    }
    Ok(())
}

/// Execute repo-controlled `luchta-config.*` code from workspace root.
///
/// Trust boundary is repository itself: luchta intentionally executes config code checked into repo.
/// On Unix it also sets mode `0o755` before exec so script can run even if execute bit was not set.
/// This mutates workspace file permissions by design; protocol stays direct-exec via shebang.
async fn execute_config_script(
    workspace_root: &Path,
    config_path: &Path,
    timeout_duration: Duration,
) -> Result<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        // Ensure the config script is executable. Skip the write entirely when
        // the owner-execute bit is already set so we don't fail on a read-only
        // filesystem (e.g. a read-only mount) where the file is already
        // runnable. Only surface an error if the file truly is not executable
        // and we cannot make it so.
        let already_executable = fs::metadata(config_path)
            .map(|metadata| metadata.permissions().mode() & 0o100 != 0)
            .unwrap_or(false);

        if !already_executable {
            let permissions = fs::Permissions::from_mode(0o755);
            fs::set_permissions(config_path, permissions)
                .into_diagnostic()
                .wrap_err_with(|| {
                    format!("failed to set executable bit on {}", config_path.display())
                })?;
        }
    }

    let mut child = spawn_config_script_with_retry(workspace_root, config_path).await?;
    let stderr_tail = Arc::new(Mutex::new(StderrTail::default()));
    let stderr_task = spawn_stderr_forwarder(child.stderr.take(), Arc::clone(&stderr_tail));
    let stdout_task = spawn_stdout_reader(child.stdout.take());

    let wait_result = timeout(timeout_duration, child.wait()).await;
    match wait_result {
        Ok(waited_status) => {
            let status = waited_status.into_diagnostic().wrap_err_with(|| {
                format!("failed to wait for config script {}", config_path.display())
            })?;
            let stdout = finish_stdout_reader(stdout_task, config_path).await?;
            let stderr = finish_stderr_forwarder(stderr_task, stderr_tail).await;

            if !status.success() {
                bail!(
                    "config script `{}` {}{}",
                    config_path.display(),
                    format_exit_status(status),
                    format_stderr_tail(&stderr)
                );
            }

            Ok(stdout)
        }
        Err(_) => {
            abort_stdout_reader(stdout_task).await;
            terminate_config_script(&mut child).await;
            let stderr = finish_stderr_forwarder(stderr_task, stderr_tail).await;
            bail!(
                "config script `{}` timed out after {}s{}",
                config_path.display(),
                timeout_duration.as_secs(),
                format_stderr_tail(&stderr)
            );
        }
    }
}

async fn spawn_config_script_with_retry(
    workspace_root: &Path,
    config_path: &Path,
) -> Result<Child> {
    let error_message = || format!("failed to execute config script {}", config_path.display());
    spawn_with_retry(
        || spawn_config_script(workspace_root, config_path),
        RetryConfig {
            retries: EXECUTE_CONFIG_ETXTBSY_RETRIES,
            backoff: EXECUTE_CONFIG_ETXTBSY_BACKOFF,
            execute_error: error_message,
            exhausted_error: error_message,
        },
    )
    .await
}

/// Retry policy and error-message providers for [`spawn_with_retry`].
struct RetryConfig<E, X>
where
    E: Fn() -> String,
    X: Fn() -> String,
{
    retries: usize,
    backoff: Duration,
    /// Context for a non-ETXTBSY spawn failure.
    execute_error: E,
    /// Context for exhausting all ETXTBSY retries.
    exhausted_error: X,
}

/// Outcome of a single spawn attempt within the retry loop.
enum SpawnAttempt<T> {
    /// Spawn succeeded.
    Done(T),
    /// Spawn hit ETXTBSY; retry after backoff (carrying the last error).
    Retry(io::Error),
    /// Spawn failed for a non-retryable reason.
    Fatal(io::Error),
}

async fn spawn_with_retry<T, F, E, X>(mut spawn: F, config: RetryConfig<E, X>) -> Result<T>
where
    F: FnMut() -> io::Result<T>,
    E: Fn() -> String,
    X: Fn() -> String,
{
    let mut last_etxtbsy_error = None;

    for attempt in 0..=config.retries {
        match classify_spawn(spawn()) {
            SpawnAttempt::Done(value) => return Ok(value),
            SpawnAttempt::Fatal(error) => {
                return Err(error)
                    .into_diagnostic()
                    .wrap_err_with(&config.execute_error)
            }
            SpawnAttempt::Retry(error) => {
                last_etxtbsy_error = Some(error);
                if attempt < config.retries {
                    // Linux can return ETXTBSY if exec races with a just-written/chmodded file.
                    // Retry briefly so config loading stays reliable under parallel test load.
                    sleep(config.backoff).await;
                }
            }
        }
    }

    let error = last_etxtbsy_error.expect("ETXTBSY retry loop should capture last error");
    Err(miette!(
        "{} after {} retries because file was still busy: {}",
        (config.exhausted_error)(),
        config.retries,
        error
    ))
}

/// Classifies a spawn result into a retry-loop control outcome.
fn classify_spawn<T>(result: io::Result<T>) -> SpawnAttempt<T> {
    match result {
        Ok(value) => SpawnAttempt::Done(value),
        Err(error) if is_etxtbsy(&error) => SpawnAttempt::Retry(error),
        Err(error) => SpawnAttempt::Fatal(error),
    }
}

fn spawn_config_script(workspace_root: &Path, config_path: &Path) -> io::Result<Child> {
    let mut command = Command::new(config_path);
    command
        .current_dir(workspace_root)
        .stdout(Stdio::piped())
        // Keep script stderr visible live by teeing piped stderr to process stderr while retaining a
        // bounded tail for diagnostics. Direct inherit would hide context in CI/remote failures.
        .stderr(Stdio::piped());

    #[cfg(unix)]
    command.process_group(0);

    command.spawn()
}

fn config_timeout() -> Duration {
    resolve_timeout(env::var(CONFIG_TIMEOUT_ENV_VAR).ok())
}

/// Resolve the config-script timeout from a raw `LUCHTA_CONFIG_TIMEOUT_SECS` value.
///
/// Accepts a positive integer number of seconds. Unset, empty, zero, or
/// unparseable values fall back to [`DEFAULT_CONFIG_TIMEOUT`]; a non-empty but
/// invalid value also emits a warning so the silent fallback is visible.
fn resolve_timeout(raw: Option<String>) -> Duration {
    let Some(value) = raw else {
        return DEFAULT_CONFIG_TIMEOUT;
    };

    if let Some(duration) = parse_positive_secs(&value) {
        return duration;
    }

    warn_invalid_timeout(&value);
    DEFAULT_CONFIG_TIMEOUT
}

/// Parses a trimmed positive integer number of seconds into a [`Duration`].
fn parse_positive_secs(value: &str) -> Option<Duration> {
    match value.trim().parse::<u64>() {
        Ok(seconds) if seconds > 0 => Some(Duration::from_secs(seconds)),
        _ => None,
    }
}

/// Warns about a non-empty but invalid timeout value being ignored.
fn warn_invalid_timeout(value: &str) {
    if !value.trim().is_empty() {
        eprintln!(
            "warning: ignoring invalid {CONFIG_TIMEOUT_ENV_VAR}=\"{value}\"; \
             using default of {}s",
            DEFAULT_CONFIG_TIMEOUT.as_secs()
        );
    }
}

#[cfg(unix)]
async fn terminate_config_script(child: &mut Child) {
    if let Some(id) = child.id() {
        let _ = kill_process_group(id);
    }

    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[cfg(not(unix))]
async fn terminate_config_script(child: &mut Child) {
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[cfg(unix)]
fn kill_process_group(pid: u32) -> io::Result<()> {
    let pid = i32::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "pid exceeds i32 range"))?;
    let rc = unsafe { libc::kill(-pid, libc::SIGKILL) };
    if rc == 0 {
        return Ok(());
    }

    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(()),
        _ => Err(error),
    }
}

fn spawn_stdout_reader(stdout: Option<ChildStdout>) -> Option<JoinHandle<io::Result<Vec<u8>>>> {
    stdout.map(|stdout| tokio::spawn(async move { read_stdout(stdout).await }))
}

async fn read_stdout<R>(stdout: R) -> io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stdout);
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).await?;
    Ok(bytes)
}

async fn finish_stdout_reader(
    stdout_task: Option<JoinHandle<io::Result<Vec<u8>>>>,
    config_path: &Path,
) -> Result<Vec<u8>> {
    let Some(handle) = stdout_task else {
        return Ok(Vec::new());
    };

    let stdout = handle.await.into_diagnostic().wrap_err_with(|| {
        format!(
            "failed to join stdout reader for config script {}",
            config_path.display()
        )
    })?;

    stdout.into_diagnostic().wrap_err_with(|| {
        format!(
            "failed to read stdout from config script {}",
            config_path.display()
        )
    })
}

async fn abort_stdout_reader(stdout_task: Option<JoinHandle<io::Result<Vec<u8>>>>) {
    if let Some(handle) = stdout_task {
        handle.abort();
        let _ = handle.await;
    }
}

fn spawn_stderr_forwarder(
    stderr: Option<ChildStderr>,
    tail: Arc<Mutex<StderrTail>>,
) -> Option<JoinHandle<io::Result<()>>> {
    stderr.map(|stderr| tokio::spawn(async move { forward_stderr(stderr, tail).await }))
}

async fn forward_stderr<R>(stderr: R, tail: Arc<Mutex<StderrTail>>) -> io::Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stderr);
    let mut stderr_out = tokio::io::stderr();
    let mut line = Vec::new();

    loop {
        line.clear();
        let bytes_read = reader.read_until(b'\n', &mut line).await?;
        if bytes_read == 0 {
            break;
        }
        stderr_out.write_all(&line).await?;
        stderr_out.flush().await?;
        tail.lock().await.push_bytes(&line);
    }

    Ok(())
}

async fn finish_stderr_forwarder(
    stderr_task: Option<JoinHandle<io::Result<()>>>,
    stderr_tail: Arc<Mutex<StderrTail>>,
) -> StderrTail {
    if let Some(handle) = stderr_task {
        let _ = handle.await;
    }

    stderr_tail.lock().await.clone()
}

fn format_exit_status(status: ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;

        if let Some(signal) = status.signal() {
            return format!("terminated by signal {}", signal);
        }
    }

    status.code().map_or_else(
        || "exited with unknown status".to_owned(),
        |code| format!("exited with status {}", code),
    )
}

fn format_stderr_tail(stderr: &StderrTail) -> String {
    let rendered = stderr.render();
    if rendered.is_empty() {
        String::new()
    } else {
        format!("\nstderr tail:\n{}", rendered)
    }
}

#[cfg(unix)]
fn is_etxtbsy(error: &io::Error) -> bool {
    error.raw_os_error() == Some(ETXTBSY_ERRNO)
}

#[cfg(not(unix))]
fn is_etxtbsy(_error: &io::Error) -> bool {
    false
}

#[derive(Clone, Debug, Default)]
struct StderrTail {
    lines: VecDeque<Vec<u8>>,
    bytes: usize,
}

impl StderrTail {
    fn push_bytes(&mut self, bytes: &[u8]) {
        self.lines.push_back(bytes.to_vec());
        self.bytes += bytes.len();

        while self.lines.len() > STDERR_TAIL_MAX_LINES || self.bytes > STDERR_TAIL_MAX_BYTES {
            if let Some(removed) = self.lines.pop_front() {
                self.bytes = self.bytes.saturating_sub(removed.len());
            } else {
                break;
            }
        }
    }

    fn render(&self) -> String {
        let bytes = self
            .lines
            .iter()
            .flat_map(|line| line.iter().copied())
            .collect::<Vec<_>>();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs, io,
        process::Stdio,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    };

    use tempfile::tempdir;
    use tokio::process::Command;

    use super::{
        load_config, load_config_with_timeout, resolve_timeout, spawn_with_retry, RetryConfig,
        DEFAULT_CONFIG_TIMEOUT, EXECUTE_CONFIG_ETXTBSY_RETRIES,
    };

    const TEST_CONFIG_ERROR: &str = "failed to execute config script /tmp/luchta-config.sh";

    /// Builds a [`RetryConfig`] whose error messages match [`TEST_CONFIG_ERROR`].
    fn test_retry_config(retries: usize) -> RetryConfig<impl Fn() -> String, impl Fn() -> String> {
        RetryConfig {
            retries,
            backoff: Duration::ZERO,
            execute_error: || TEST_CONFIG_ERROR.to_owned(),
            exhausted_error: || TEST_CONFIG_ERROR.to_owned(),
        }
    }

    /// Runs `spawn_with_retry` against a closure that always fails with `errno`,
    /// returning the attempt count and the resulting error.
    async fn spawn_failing_with_errno(errno: i32, retries: usize) -> (usize, miette::Report) {
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_spawn = Arc::clone(&attempts);
        let error = spawn_with_retry::<(), _, _, _>(
            move || {
                attempts_for_spawn.fetch_add(1, Ordering::SeqCst);
                Err(io::Error::from_raw_os_error(errno))
            },
            test_retry_config(retries),
        )
        .await
        .expect_err("spawn should fail");
        (attempts.load(Ordering::SeqCst), error)
    }

    #[test]
    fn resolve_timeout_uses_positive_override() {
        assert_eq!(
            resolve_timeout(Some("5".to_owned())),
            Duration::from_secs(5)
        );
        assert_eq!(
            resolve_timeout(Some("  12 ".to_owned())),
            Duration::from_secs(12)
        );
    }

    #[test]
    fn resolve_timeout_falls_back_on_unset_or_invalid() {
        assert_eq!(resolve_timeout(None), DEFAULT_CONFIG_TIMEOUT);
        assert_eq!(resolve_timeout(Some(String::new())), DEFAULT_CONFIG_TIMEOUT);
        assert_eq!(
            resolve_timeout(Some("0".to_owned())),
            DEFAULT_CONFIG_TIMEOUT
        );
        assert_eq!(
            resolve_timeout(Some("abc".to_owned())),
            DEFAULT_CONFIG_TIMEOUT
        );
        assert_eq!(
            resolve_timeout(Some("-3".to_owned())),
            DEFAULT_CONFIG_TIMEOUT
        );
    }

    #[tokio::test]
    async fn loads_config_from_executable_script() {
        let temp = tempdir().expect("tempdir");
        fs::write(
            temp.path().join("luchta-config.sh"),
            r#"#!/bin/sh
echo '{"tasks":{"build":{"dependsOn":["^build"],"weight":2}},"concurrency":{"maxWeight":10}}'
"#,
        )
        .expect("write script");

        let config = load_config(temp.path()).await.expect("config should load");

        assert_eq!(config.concurrency.max_weight, 10);
        assert_eq!(config.tasks["build"].weight, 2);
        assert_eq!(config.tasks["build"].depends_on.len(), 1);
    }

    #[tokio::test]
    async fn loads_large_config_stdout_without_timeout() {
        let temp = tempdir().expect("tempdir");
        fs::write(
            temp.path().join("luchta-config.py"),
            r#"#!/usr/bin/env python3
import json

workers = {}
for i in range(6000):
    workers[f"worker-{i:04}"] = {
        "command": f"echo worker-{i:04}-" + ("x" * 24)
    }

config = {
    "tasks": {
        "build": {
            "worker": "worker-0000"
        }
    },
    "workers": workers,
    "concurrency": {
        "maxWeight": 10
    }
}

print(json.dumps(config, separators=(",", ":")))
"#,
        )
        .expect("write script");

        let started = Instant::now();
        let config = load_config_with_timeout(temp.path(), Duration::from_secs(2))
            .await
            .expect("large config should load");
        let elapsed = started.elapsed();

        assert!(elapsed < Duration::from_millis(500));
        assert_eq!(config.concurrency.max_weight, 10);
        assert_eq!(config.tasks["build"].worker.as_deref(), Some("worker-0000"));
        assert_eq!(config.workers.len(), 6000);
        assert!(config.workers.contains_key("worker-5999"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn loads_already_executable_config_on_read_only_dir() {
        use std::os::unix::fs::PermissionsExt;

        // An already-executable config in a read-only directory must load: the
        // loader must not try (and fail) to re-chmod it. Regression for the
        // "failed to set executable bit ... Read-only file system" error seen
        // on read-only mounts.
        let temp = tempdir().expect("tempdir");
        let script = temp.path().join("luchta-config.sh");
        fs::write(
            &script,
            "#!/bin/sh\necho '{\"tasks\":{\"build\":{}},\"concurrency\":{\"maxWeight\":3}}'\n",
        )
        .expect("write script");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod script");
        // Make the directory read-only so re-chmodding the file would fail.
        let dir_perms = fs::metadata(temp.path())
            .expect("dir metadata")
            .permissions();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o555))
            .expect("make dir read-only");

        let result = load_config(temp.path()).await;

        // Restore writable permissions so the temp dir can be cleaned up.
        fs::set_permissions(temp.path(), dir_perms).expect("restore dir perms");

        let config = result.expect("config should load on a read-only dir");
        assert_eq!(config.concurrency.max_weight, 3);
    }

    #[tokio::test]
    async fn errors_when_no_config_file_exists() {
        let temp = tempdir().expect("tempdir");

        let error = load_config(temp.path())
            .await
            .expect_err("missing config should fail");

        assert!(error
            .to_string()
            .contains("no config file found (expected a file matching luchta-config.*)"));
    }

    #[tokio::test]
    async fn errors_when_multiple_config_files_exist() {
        let temp = tempdir().expect("tempdir");
        fs::write(
            temp.path().join("luchta-config.sh"),
            "#!/bin/sh\necho '{}'\n",
        )
        .expect("write script");
        fs::write(
            temp.path().join("luchta-config.js"),
            "#!/usr/bin/env node\nconsole.log('{}')\n",
        )
        .expect("write second script");

        let error = load_config(temp.path())
            .await
            .expect_err("multiple configs should fail");
        let message = error.to_string();

        assert!(message.contains("multiple config files found:"));
        assert!(message.contains("luchta-config.js"));
        assert!(message.contains("luchta-config.sh"));
    }

    #[tokio::test]
    async fn errors_when_config_has_no_shebang() {
        let temp = tempdir().expect("tempdir");
        fs::write(temp.path().join("luchta-config.sh"), "echo '{}'\n").expect("write script");

        let error = load_config(temp.path())
            .await
            .expect_err("missing shebang should fail");

        assert!(error.to_string().contains("has no shebang line"));
    }

    #[tokio::test]
    async fn errors_when_config_script_exits_non_zero() {
        let temp = tempdir().expect("tempdir");
        fs::write(
            temp.path().join("luchta-config.sh"),
            "#!/bin/sh\necho 'boom from stderr' >&2\nexit 7\n",
        )
        .expect("write script");

        let error = load_config(temp.path())
            .await
            .expect_err("non-zero exit should fail");

        assert!(error.to_string().contains("config script"));
        assert!(error.to_string().contains("exited with status 7"));
        assert!(error.to_string().contains("stderr tail:\nboom from stderr"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reports_signal_termination_for_config_script() {
        let temp = tempdir().expect("tempdir");
        fs::write(
            temp.path().join("luchta-config.sh"),
            "#!/bin/sh\nkill -TERM $$\n",
        )
        .expect("write script");

        let error = load_config(temp.path())
            .await
            .expect_err("signal exit should fail");

        assert!(error.to_string().contains("terminated by signal 15"));
    }

    #[tokio::test]
    async fn errors_when_config_script_prints_invalid_json() {
        let temp = tempdir().expect("tempdir");
        fs::write(
            temp.path().join("luchta-config.sh"),
            "#!/bin/sh\necho 'not-json'\n",
        )
        .expect("write script");

        let error = load_config(temp.path())
            .await
            .expect_err("invalid json should fail");
        let message = error.to_string();

        assert!(message.contains("failed to parse config from"));
        assert!(message.contains("error: expected ident"));
        assert!(message.contains("at line 1, column 2"));
        assert!(message.contains("-->    1 | not-json"));
    }

    #[tokio::test]
    async fn error_with_json_path_shows_path_and_excerpt() {
        let temp = tempdir().expect("tempdir");
        // dependsOn contains an invalid entry that will fail during deserialization
        fs::write(
            temp.path().join("luchta-config.sh"),
            "#!/bin/sh\necho '{\"tasks\":{\"build\":{\"dependsOn\":[\"#\"],\"weight\":2}}}'",
        )
        .expect("write script");

        let error = load_config(temp.path())
            .await
            .expect_err("bad dependsOn should fail");
        let message = error.to_string();

        assert!(
            message.contains("tasks.build.dependsOn"),
            "should show JSON path, got: {message}"
        );
    }

    #[tokio::test]
    async fn error_formatting_does_not_panic_with_minimal_output() {
        let temp = tempdir().expect("tempdir");
        // Single-line invalid JSON — serde may report line numbers beyond content length
        fs::write(temp.path().join("luchta-config.sh"), "#!/bin/sh\necho 'x'")
            .expect("write script");

        let error = load_config(temp.path())
            .await
            .expect_err("invalid json should fail");
        // Should not panic — just check it produces a message
        let message = error.to_string();
        assert!(message.contains("failed to parse config from"));
    }

    #[tokio::test]
    async fn times_out_stuck_config_script() {
        let temp = tempdir().expect("tempdir");
        fs::write(
            temp.path().join("luchta-config.sh"),
            "#!/bin/sh\necho sleeping >&2\nsleep 30\n",
        )
        .expect("write script");

        let started = Instant::now();
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            load_config_with_timeout(temp.path(), Duration::from_secs(1)),
        )
        .await
        .expect("loader should return promptly")
        .expect_err("stuck script should time out");

        let elapsed = started.elapsed();
        let message = error.to_string();

        assert!(elapsed < Duration::from_secs(5));
        assert!(message.contains("timed out after 1s"));
        assert!(message.contains("stderr tail:\nsleeping"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retries_etxtbsy_then_succeeds() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_spawn = Arc::clone(&attempts);
        let child = spawn_with_retry(
            move || {
                let attempt = attempts_for_spawn.fetch_add(1, Ordering::SeqCst);
                if attempt < 3 {
                    Err(io::Error::from_raw_os_error(26))
                } else {
                    Command::new("/bin/sh")
                        .arg("-c")
                        .arg("exit 0")
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()
                }
            },
            test_retry_config(EXECUTE_CONFIG_ETXTBSY_RETRIES),
        )
        .await
        .expect("spawn should eventually succeed");

        assert_eq!(attempts.load(Ordering::SeqCst), 4);
        drop(child);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn errors_after_etxtbsy_retry_exhaustion() {
        // 26 == ETXTBSY: every attempt retries until the budget is exhausted.
        let (attempts, error) = spawn_failing_with_errno(26, 3).await;

        assert_eq!(attempts, 4);
        assert!(error.to_string().contains(&format!(
            "{TEST_CONFIG_ERROR} after 3 retries because file was still busy"
        )));
    }

    #[tokio::test]
    async fn propagates_non_etxtbsy_spawn_error_without_retry() {
        // 13 == EACCES: a non-retryable error must fail on the first attempt.
        let (attempts, error) = spawn_failing_with_errno(13, EXECUTE_CONFIG_ETXTBSY_RETRIES).await;

        assert_eq!(attempts, 1);
        assert!(error.to_string().contains(TEST_CONFIG_ERROR));
    }
}
