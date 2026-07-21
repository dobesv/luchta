use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use http_body_util::Full;
use hyper_util::client::legacy::Client;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use super::transport::{post_json_raw_with_client, post_json_with_client};
use super::{Bytes, RcloneError};

pub(super) const DEFAULT_RCLONE_SUBMIT_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const RCLONE_JOB_STATUS_INITIAL_BACKOFF: Duration = Duration::from_millis(50);
pub(super) const RCLONE_JOB_STATUS_MAX_BACKOFF: Duration = Duration::from_millis(500);

#[derive(Debug, Deserialize)]
pub(super) struct AsyncJobSubmitResponse {
    pub(super) jobid: i64,
    #[serde(rename = "executeId")]
    pub(super) execute_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AsyncJobStatusResponse {
    pub(super) finished: bool,
    pub(super) success: bool,
    #[serde(default)]
    pub(super) error: String,
    #[serde(default)]
    pub(super) output: Value,
    #[serde(rename = "executeId")]
    pub(super) execute_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct SubmittedAsyncJob {
    pub(super) jobid: i64,
    pub(super) execute_id: Option<String>,
}

pub(super) async fn submit_async_job(
    client: &Client<hyperlocal::UnixConnector, Full<Bytes>>,
    socket_path: &std::path::Path,
    endpoint: &str,
    mut payload: Value,
    submit_timeout: Duration,
) -> Result<SubmittedAsyncJob, RcloneError> {
    let object = payload
        .as_object_mut()
        .ok_or_else(|| RcloneError::Request {
            reason: format!("rclone async payload for {endpoint} must be a JSON object"),
        })?;
    object.insert("_async".to_string(), Value::Bool(true));
    let response: AsyncJobSubmitResponse =
        post_json_with_client(client, socket_path, endpoint, payload, submit_timeout).await?;
    Ok(SubmittedAsyncJob {
        jobid: response.jobid,
        execute_id: response.execute_id,
    })
}

pub(super) async fn poll_async_job<T>(
    client: &Client<hyperlocal::UnixConnector, Full<Bytes>>,
    socket_path: &std::path::Path,
    submitted: SubmittedAsyncJob,
    execution_timeout: Duration,
) -> Result<T, RcloneError>
where
    T: DeserializeOwned,
{
    let client = Arc::new(client.clone());
    let socket_path = Arc::<std::path::PathBuf>::from(socket_path.to_path_buf());
    poll_async_job_with(submitted, execution_timeout, move |submitted, remaining| {
        let client = Arc::clone(&client);
        let socket_path = Arc::clone(&socket_path);
        let submitted = submitted.clone();
        async move {
            poll_async_job_status(client.as_ref(), socket_path.as_path(), submitted, remaining)
                .await
        }
    })
    .await
}

async fn poll_async_job_with<T, F, Fut>(
    submitted: SubmittedAsyncJob,
    execution_timeout: Duration,
    mut poll_status: F,
) -> Result<T, RcloneError>
where
    T: DeserializeOwned,
    F: FnMut(&SubmittedAsyncJob, Duration) -> Fut,
    Fut: Future<Output = Result<AsyncJobStatusResponse, RcloneError>>,
{
    let deadline = Instant::now() + execution_timeout;
    let mut backoff = RCLONE_JOB_STATUS_INITIAL_BACKOFF;
    loop {
        if let Some(result) =
            poll_status_once(&mut poll_status, &submitted, execution_timeout, deadline).await?
        {
            return result;
        }
        tokio::time::sleep(backoff.min(deadline.saturating_duration_since(Instant::now()))).await;
        backoff = next_job_status_backoff(backoff);
    }
}

async fn poll_status_once<T, F, Fut>(
    poll_status: &mut F,
    submitted: &SubmittedAsyncJob,
    execution_timeout: Duration,
    deadline: Instant,
) -> Result<Option<Result<T, RcloneError>>, RcloneError>
where
    T: DeserializeOwned,
    F: FnMut(&SubmittedAsyncJob, Duration) -> Fut,
    Fut: Future<Output = Result<AsyncJobStatusResponse, RcloneError>>,
{
    let now = Instant::now();
    if now >= deadline {
        return Ok(Some(Err(RcloneError::Timeout {
            timeout: execution_timeout,
        })));
    }
    let remaining = deadline.saturating_duration_since(now);
    let status = match poll_status(submitted, remaining).await {
        Ok(status) => status,
        Err(err) => {
            return Err(classify_poll_status_error(
                err,
                &submitted.execute_id,
                execution_timeout,
            ))
        }
    };
    if !status.finished {
        return Ok(None);
    }
    Ok(Some(map_async_job_status::<T>(status)))
}

async fn poll_async_job_status(
    client: &Client<hyperlocal::UnixConnector, Full<Bytes>>,
    socket_path: &std::path::Path,
    submitted: SubmittedAsyncJob,
    remaining: Duration,
) -> Result<AsyncJobStatusResponse, RcloneError> {
    let status: AsyncJobStatusResponse = post_json_raw_with_client(
        client,
        socket_path,
        "job/status",
        json!({ "jobid": submitted.jobid }),
        remaining,
    )
    .await?;
    if is_execute_id_mismatch(&submitted.execute_id, &status.execute_id) {
        return Err(RcloneError::Request {
            reason: "rclone async job executeId mismatch".to_string(),
        });
    }
    Ok(status)
}

pub(super) fn next_job_status_backoff(current: Duration) -> Duration {
    current.saturating_mul(2).min(RCLONE_JOB_STATUS_MAX_BACKOFF)
}

fn is_execute_id_mismatch(expected: &Option<String>, actual: &Option<String>) -> bool {
    matches!((expected.as_deref(), actual.as_deref()), (Some(expected), Some(actual)) if expected != actual)
}

fn classify_poll_status_error(
    err: RcloneError,
    submitted_execute_id: &Option<String>,
    execution_timeout: Duration,
) -> RcloneError {
    if is_reaped_job_status_error(&err) {
        return classify_reaped_job_error(submitted_execute_id, execution_timeout, err);
    }
    err
}

fn classify_reaped_job_error(
    submitted_execute_id: &Option<String>,
    execution_timeout: Duration,
    err: RcloneError,
) -> RcloneError {
    if submitted_execute_id.is_some() {
        return RcloneError::Timeout {
            timeout: execution_timeout,
        };
    }
    err
}

fn is_reaped_job_status_error(err: &RcloneError) -> bool {
    match err {
        RcloneError::Rc { message } => message.contains("job not found"),
        RcloneError::HttpStatus { body, .. } => body.contains("job not found"),
        _ => false,
    }
}

pub(super) fn map_async_job_status<T>(status: AsyncJobStatusResponse) -> Result<T, RcloneError>
where
    T: DeserializeOwned,
{
    if status.success {
        return serde_json::from_value(status.output).map_err(RcloneError::from);
    }

    let message = if status.error.is_empty() {
        "async rclone job failed".to_string()
    } else {
        status.error
    };
    Err(classify_async_job_failure(message))
}

fn classify_async_job_failure(message: String) -> RcloneError {
    let lower = message.to_ascii_lowercase();
    let is_not_found = lower.contains("object not found")
        || lower.contains("directory not found")
        || lower.contains("not found");
    RcloneError::HttpStatus {
        status: if is_not_found { 404 } else { 500 },
        body: message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::rclone::EmptyResponse;
    use crate::shared::rclone::StatResponse;

    fn assert_mapped_job_status_failure(error: &str, expected_status: u16) {
        let status = AsyncJobStatusResponse {
            finished: true,
            success: false,
            error: error.to_string(),
            output: json!({}),
            execute_id: Some("exec-1".to_string()),
        };

        let err = map_async_job_status::<EmptyResponse>(status).unwrap_err();
        assert!(matches!(
            err,
            RcloneError::HttpStatus { status, body } if status == expected_status && body == error
        ));
    }

    #[test]
    fn async_job_status_success_deserializes_output() {
        let status = AsyncJobStatusResponse {
            finished: true,
            success: true,
            error: String::new(),
            output: json!({ "item": { "Path": "blob", "Name": "blob", "IsDir": false, "Size": 7 } }),
            execute_id: Some("exec-1".to_string()),
        };

        let response: StatResponse = map_async_job_status(status).unwrap();
        let item = response.item.unwrap();
        assert_eq!(item.path, "blob");
        assert_eq!(item.size, 7);
    }

    #[test]
    fn async_job_status_stat_null_item_maps_to_miss() {
        let status = AsyncJobStatusResponse {
            finished: true,
            success: true,
            error: String::new(),
            output: json!({ "item": null }),
            execute_id: Some("exec-1".to_string()),
        };

        let response: StatResponse = map_async_job_status(status).unwrap();
        assert!(response.item.is_none());
    }

    #[test]
    fn async_job_status_copyfile_empty_output_maps_to_ok() {
        let status = AsyncJobStatusResponse {
            finished: true,
            success: true,
            error: String::new(),
            output: json!({}),
            execute_id: Some("exec-1".to_string()),
        };

        map_async_job_status::<EmptyResponse>(status).unwrap();
    }

    #[test]
    fn async_job_status_failure_maps_not_found_to_http_404() {
        for error in ["404 page not found", "object not found"] {
            assert_mapped_job_status_failure(error, 404);
        }
    }

    #[test]
    fn async_job_status_failure_real_error_maps_to_http_500() {
        assert_mapped_job_status_failure("permission denied", 500);
    }

    #[test]
    fn raw_job_status_failure_deserializes_before_http_status_mapping() {
        let status: AsyncJobStatusResponse = serde_json::from_value(json!({
            "finished": true,
            "success": false,
            "error": "object not found",
            "output": {},
            "executeId": "exec-1"
        }))
        .unwrap();

        let err = map_async_job_status::<EmptyResponse>(status).unwrap_err();
        assert!(matches!(
            err,
            RcloneError::HttpStatus { status: 404, body } if body == "object not found"
        ));
    }

    #[test]
    fn async_job_status_backoff_caps_at_max() {
        let mut backoff = RCLONE_JOB_STATUS_INITIAL_BACKOFF;
        for _ in 0..8 {
            backoff = next_job_status_backoff(backoff);
        }
        assert_eq!(backoff, RCLONE_JOB_STATUS_MAX_BACKOFF);
    }

    #[test]
    fn async_job_execute_id_mismatch_requires_different_non_empty_ids() {
        assert!(is_execute_id_mismatch(
            &Some("exec-1".to_string()),
            &Some("exec-2".to_string())
        ));
        assert!(!is_execute_id_mismatch(
            &Some("exec-1".to_string()),
            &Some("exec-1".to_string())
        ));
        assert!(!is_execute_id_mismatch(&Some("exec-1".to_string()), &None));
        assert!(!is_execute_id_mismatch(&None, &Some("exec-1".to_string())));
    }

    #[test]
    fn reaped_job_error_classification_uses_execute_id_presence() {
        let timeout = Duration::from_secs(17);
        for (submitted_execute_id, expected_timeout) in [(Some("exec-1"), true), (None, false)] {
            let err = classify_reaped_job_error(
                &submitted_execute_id.map(str::to_string),
                timeout,
                RcloneError::HttpStatus {
                    status: 500,
                    body: "job not found".to_string(),
                },
            );
            if expected_timeout {
                assert!(matches!(err, RcloneError::Timeout { timeout: t } if t == timeout));
            } else {
                assert!(
                    matches!(err, RcloneError::HttpStatus { status: 500, body } if body == "job not found")
                );
            }
        }
    }

    #[test]
    fn reaped_job_status_detection_matches_expected_errors() {
        assert!(is_reaped_job_status_error(&RcloneError::Rc {
            message: "job not found".to_string(),
        }));
        assert!(is_reaped_job_status_error(&RcloneError::HttpStatus {
            status: 500,
            body: "job not found".to_string(),
        }));
        assert!(!is_reaped_job_status_error(&RcloneError::HttpStatus {
            status: 500,
            body: "permission denied".to_string(),
        }));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn poll_async_job_times_out_when_status_never_finishes() {
        let submitted = SubmittedAsyncJob {
            jobid: 1,
            execute_id: Some("exec-1".to_string()),
        };
        let result: Result<EmptyResponse, RcloneError> =
            poll_async_job_with(submitted, Duration::from_millis(1), |_, _| async {
                Ok(AsyncJobStatusResponse {
                    finished: false,
                    success: true,
                    error: String::new(),
                    output: json!({}),
                    execute_id: Some("exec-1".to_string()),
                })
            })
            .await;
        assert!(
            matches!(result, Err(RcloneError::Timeout { timeout }) if timeout == Duration::from_millis(1))
        );
    }
}
