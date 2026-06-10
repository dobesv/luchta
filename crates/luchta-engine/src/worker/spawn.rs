use std::{io, time::Duration};

use tokio::{process::Child, time::sleep};

use super::{io_tasks::worker_command, manager::WorkerError};

const SPAWN_RETRY_ATTEMPTS: usize = 10;
const SPAWN_RETRY_DELAY: Duration = Duration::from_millis(5);

struct SpawnAttempt<'a> {
    worker: &'a str,
    command_line: &'a str,
}

pub(crate) async fn spawn_worker_process(
    worker: &str,
    command_line: &str,
) -> Result<Child, WorkerError> {
    let attempt = SpawnAttempt {
        worker,
        command_line,
    };
    let mut last_error = None;

    for retry in spawn_retry_indices() {
        match spawn_worker_once(&attempt, retry, &mut last_error).await? {
            Some(child) => return Ok(child),
            None => continue,
        }
    }

    Err(spawn_error(
        worker,
        last_error.unwrap_or_else(etxtbsy_error),
    ))
}

async fn spawn_worker_once(
    attempt: &SpawnAttempt<'_>,
    retry: usize,
    last_error: &mut Option<io::Error>,
) -> Result<Option<Child>, WorkerError> {
    match spawn_worker_child(attempt) {
        Ok(child) => Ok(Some(child)),
        Err(source) => handle_spawn_failure(attempt, retry, last_error, source).await,
    }
}

async fn handle_spawn_failure(
    attempt: &SpawnAttempt<'_>,
    retry: usize,
    last_error: &mut Option<io::Error>,
    source: io::Error,
) -> Result<Option<Child>, WorkerError> {
    if !should_retry_spawn(&source) {
        return Err(spawn_error(attempt.worker, source));
    }

    *last_error = Some(source);
    maybe_wait_for_spawn_retry(retry).await;
    Ok(None)
}

async fn maybe_wait_for_spawn_retry(retry: usize) {
    if retry + 1 < SPAWN_RETRY_ATTEMPTS {
        sleep(SPAWN_RETRY_DELAY).await;
    }
}

fn spawn_retry_indices() -> std::ops::Range<usize> {
    0..SPAWN_RETRY_ATTEMPTS
}

fn spawn_worker_child(attempt: &SpawnAttempt<'_>) -> io::Result<Child> {
    worker_command(attempt.command_line).spawn()
}

fn should_retry_spawn(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ETXTBSY)
}

fn spawn_error(worker: &str, source: io::Error) -> WorkerError {
    WorkerError::Spawn {
        worker: worker.to_owned(),
        source,
    }
}

fn etxtbsy_error() -> io::Error {
    io::Error::from_raw_os_error(libc::ETXTBSY)
}
