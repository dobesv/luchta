use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkerRequest {
    pub id: String,
    pub command: String,
    pub cwd: Option<String>,
    /// Yarn workspace hint for yarn worker.
    /// `None` => run `command` as raw shell command (generic behavior).
    /// `Some("")` => run `yarn <command>` at workspace root.
    /// `Some(name)` => run `yarn workspace <name> <command>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

impl WorkerRequest {
    pub fn new(id: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            command: command.into(),
            cwd: None,
            workspace: None,
            env: HashMap::new(),
        }
    }

    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_workspace(mut self, workspace: impl Into<String>) -> Self {
        self.workspace = Some(workspace.into());
        self
    }

    pub fn with_env(mut self, env: HashMap<String, String>) -> Self {
        self.env = env;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum WorkerResponse {
    #[serde(rename_all = "camelCase")]
    Log {
        id: String,
        stream: LogStream,
        line: String,
    },
    #[serde(rename_all = "camelCase")]
    Done { id: String, exit_code: i32 },
}

impl WorkerResponse {
    pub fn log(id: impl Into<String>, stream: LogStream, line: impl Into<String>) -> Self {
        Self::Log {
            id: id.into(),
            stream,
            line: line.into(),
        }
    }

    pub fn done(id: impl Into<String>, exit_code: i32) -> Self {
        Self::Done {
            id: id.into(),
            exit_code,
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Log { id, .. } | Self::Done { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogStream {
    Stdout,
    Stderr,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::{json, Value};

    use super::{LogStream, WorkerRequest, WorkerResponse};

    #[test]
    fn worker_request_roundtrip_preserves_fields() {
        let mut env = HashMap::new();
        env.insert("NODE_ENV".to_owned(), "test".to_owned());

        let request = WorkerRequest::new("pkg#task", "yarn test")
            .with_cwd("packages/pkg")
            .with_env(env.clone());

        let json = serde_json::to_string(&request).expect("request serializes");
        assert!(!json.contains('\n'));

        let decoded: WorkerRequest = serde_json::from_str(&json).expect("request deserializes");
        assert_eq!(decoded, request);
        assert_eq!(decoded.env, env);
    }

    #[test]
    fn worker_request_roundtrip_preserves_workspace_some_and_none() {
        let with_workspace = WorkerRequest::new("pkg#task", "build --flag")
            .with_cwd("packages/pkg")
            .with_workspace("pkg");
        let with_workspace_json =
            serde_json::to_string(&with_workspace).expect("request serializes");
        let with_workspace_decoded: WorkerRequest =
            serde_json::from_str(&with_workspace_json).expect("request deserializes");
        assert_eq!(with_workspace_decoded, with_workspace);

        let without_workspace = WorkerRequest::new("pkg#task", "build --flag");
        let without_workspace_json =
            serde_json::to_string(&without_workspace).expect("request serializes");
        let without_workspace_decoded: WorkerRequest =
            serde_json::from_str(&without_workspace_json).expect("request deserializes");
        assert_eq!(without_workspace_decoded, without_workspace);
        assert_eq!(without_workspace_decoded.workspace, None);
    }

    #[test]
    fn worker_request_workspace_serializes_empty_and_named_values() {
        let root_value =
            serde_json::to_value(WorkerRequest::new("root#build", "install").with_workspace(""))
                .expect("request serializes");
        assert_eq!(root_value["workspace"], Value::String(String::new()));
        let root_decoded: WorkerRequest = serde_json::from_value(root_value).expect("deserialize");
        assert_eq!(root_decoded.workspace.as_deref(), Some(""));

        let package_value = serde_json::to_value(
            WorkerRequest::new("pkg#build", "build --flag").with_workspace("pkg"),
        )
        .expect("request serializes");
        assert_eq!(package_value["workspace"], Value::String("pkg".to_owned()));
        let package_decoded: WorkerRequest =
            serde_json::from_value(package_value).expect("deserialize");
        assert_eq!(package_decoded.workspace.as_deref(), Some("pkg"));
    }

    #[test]
    fn worker_request_none_workspace_omits_key() {
        let value = serde_json::to_value(WorkerRequest::new("pkg#task", "yarn test"))
            .expect("request serializes");
        let object = value.as_object().expect("request is object");
        assert!(!object.contains_key("workspace"));
    }

    #[test]
    fn worker_request_json_uses_camel_case_fields() {
        let request = WorkerRequest::new("pkg#task", "yarn test").with_cwd("packages/pkg");

        let value = serde_json::to_value(request).expect("request serializes");
        assert_eq!(
            value,
            json!({
                "id": "pkg#task",
                "command": "yarn test",
                "cwd": "packages/pkg",
                "env": {}
            })
        );
    }

    #[test]
    fn worker_response_variants_roundtrip_and_match_json_shape() {
        let cases = [
            (
                WorkerResponse::log("pkg#task", LogStream::Stdout, "hello"),
                json!({
                    "type": "log",
                    "id": "pkg#task",
                    "stream": "stdout",
                    "line": "hello"
                }),
            ),
            (
                WorkerResponse::done("pkg#task", 0),
                json!({
                    "type": "done",
                    "id": "pkg#task",
                    "exitCode": 0
                }),
            ),
        ];

        for (response, expected) in cases {
            let json = serde_json::to_string(&response).expect("response serializes");
            let decoded: WorkerResponse =
                serde_json::from_str(&json).expect("response deserializes");

            assert_eq!(decoded, response);
            assert_eq!(response.id(), "pkg#task");
            assert_eq!(
                serde_json::to_value(response).expect("response serializes"),
                expected
            );
        }
    }

    #[test]
    fn malformed_worker_protocol_line_returns_error() {
        let err = serde_json::from_str::<WorkerResponse>("not json");
        assert!(err.is_err());
    }

    #[test]
    fn worker_request_deserialization_defaults_env_and_workspace() {
        let decoded: WorkerRequest = serde_json::from_value(json!({
            "id": "pkg#task",
            "command": "yarn test",
            "cwd": null
        }))
        .expect("request deserializes");

        assert_eq!(decoded.env, HashMap::new());
        assert_eq!(decoded.workspace, None);
    }

    #[test]
    fn worker_response_log_stream_serializes_lowercase() {
        let value = serde_json::to_value(LogStream::Stderr).expect("stream serializes");
        assert_eq!(value, Value::String("stderr".to_owned()));
    }
}
