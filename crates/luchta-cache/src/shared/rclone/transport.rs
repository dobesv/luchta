use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::{BufMut, Bytes, BytesMut};
use http_body_util::{BodyExt, Full};
use hyper::header::CONTENT_TYPE;
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyperlocal::Uri;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use tokio::time::timeout;

use super::RcloneError;

pub(super) async fn post_json_with_client<P, T>(
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
    let value =
        post_json_value_with_client(client, socket_path, endpoint, payload, timeout_duration)
            .await?;
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        if !error.is_empty() {
            return Err(RcloneError::Rc {
                message: error.to_string(),
            });
        }
    }
    serde_json::from_value(value).map_err(RcloneError::from)
}

pub(super) async fn post_json_raw_with_client<P, T>(
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
    let value =
        post_json_value_with_client(client, socket_path, endpoint, payload, timeout_duration)
            .await?;
    serde_json::from_value(value).map_err(RcloneError::from)
}

pub(super) async fn post_json_value_with_client<P>(
    client: &Client<hyperlocal::UnixConnector, Full<Bytes>>,
    socket_path: &std::path::Path,
    endpoint: &str,
    payload: P,
    timeout_duration: Duration,
) -> Result<Value, RcloneError>
where
    P: Serialize,
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

    serde_json::from_slice(&bytes).map_err(RcloneError::from)
}

pub(super) async fn post_multipart_with_client(
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

    if !bytes.is_empty() {
        let body = String::from_utf8_lossy(&bytes);
        if let Some(message) = detect_rc_error(&body)? {
            return Err(RcloneError::Rc { message });
        }
    }
    Ok(())
}

pub(super) fn build_upload_multipart_body(boundary: &str, file_name: &str, bytes: &[u8]) -> Bytes {
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

pub(super) fn multipart_boundary() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0_u128, |duration| duration.as_nanos());
    format!("luchta-rclone-upload-{nanos:x}")
}

pub(super) fn url_encode_query_value(value: &str) -> String {
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

pub(super) fn detect_rc_error(body: &str) -> Result<Option<String>, RcloneError> {
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

pub(super) fn display_entry(entry: &super::Entry, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{}", entry.path)
}
