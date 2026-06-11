use std::collections::HashMap;

use luchta_types::{DependsOn, TaskDefinition};
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
    /// Worker's decision for a `ResolveTask` request, correlated by `id`.
    #[serde(rename_all = "camelCase")]
    Resolved { id: String, result: ResolveResult },
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

    pub fn resolved(id: impl Into<String>, result: ResolveResult) -> Self {
        Self::Resolved {
            id: id.into(),
            result,
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Log { id, .. } | Self::Done { id, .. } | Self::Resolved { id, .. } => id,
        }
    }

    /// A short, stable name for the response variant, for diagnostics.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Log { .. } => "log",
            Self::Done { .. } => "done",
            Self::Resolved { .. } => "resolved",
        }
    }

    /// The exit code if this is a `Done` response, else `None`. Used to select
    /// the terminal response of an execution job.
    pub fn into_exit_code(self) -> Option<i32> {
        match self {
            Self::Done { exit_code, .. } => Some(exit_code),
            _ => None,
        }
    }

    /// The resolve decision if this is a `Resolved` response, else `None`. Used
    /// to select the terminal response of a resolution round-trip.
    pub fn into_resolve_result(self) -> Option<ResolveResult> {
        match self {
            Self::Resolved { result, .. } => Some(result),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogStream {
    Stdout,
    Stderr,
}

/// A message sent from the engine to a resident worker on its stdin channel.
///
/// Tagged on the wire by `type`: `run` carries an execution [`WorkerRequest`];
/// `resolveTask` carries a [`ResolveTask`] decision request. Both are correlated
/// with their responses by the contained `id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum WorkerMessage {
    /// Execute a task: run `command` (existing behavior).
    Run(WorkerRequest),
    /// Ask the worker to resolve a task before it enters the graph.
    ResolveTask(ResolveTask),
}

impl WorkerMessage {
    /// Returns the correlation id carried by the message.
    pub fn id(&self) -> &str {
        match self {
            Self::Run(request) => &request.id,
            Self::ResolveTask(resolve) => &resolve.id,
        }
    }
}

impl From<WorkerRequest> for WorkerMessage {
    fn from(request: WorkerRequest) -> Self {
        Self::Run(request)
    }
}

impl From<ResolveTask> for WorkerMessage {
    fn from(resolve: ResolveTask) -> Self {
        Self::ResolveTask(resolve)
    }
}

/// Graph-build mode, controlling how a worker `Reject` is treated downstream.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum ResolveMode {
    /// Normal execution: a `Reject` is downgraded to a warning + prune.
    #[default]
    Run,
    /// `luchta check`: a `Reject` is a hard error.
    Check,
}

/// Request asking a worker whether and how a task should enter the graph.
///
/// The engine supplies the target package's declared `scripts` so the worker
/// does not re-read `package.json` from disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResolveTask {
    /// Correlation id (the scoped task id).
    pub id: String,
    /// Task name (used as the script name when `command` is blank).
    pub name: String,
    /// Explicit command override; when non-blank it is the script name to look up.
    #[serde(default)]
    pub command: String,
    /// Target package name (for diagnostics).
    pub package: String,
    /// Target package root directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Script names declared by the target package (from `PackageNode`).
    #[serde(default)]
    pub scripts: Vec<String>,
    /// Graph-build mode.
    #[serde(default)]
    pub mode: ResolveMode,
}

impl ResolveTask {
    /// The script name this task resolves to: explicit non-blank `command`,
    /// otherwise the task `name`.
    pub fn resolved_script_name(&self) -> &str {
        luchta_types::resolve_script_name(Some(&self.command), &self.name)
    }
}

/// A worker's decision about a task being resolved.
///
/// `Modify` is included for forward-compatibility (the engine replaces parts of
/// the task spec) but is not produced by `luchta-yarn-worker` in this change.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "decision", rename_all = "camelCase")]
pub enum ResolveDecision {
    /// Keep the task unchanged.
    Accept,
    /// Update part of the task spec, keeping the rest. Each field is optional;
    /// `None`/absent leaves that part of the task untouched.
    Modify(TaskModification),
    /// Drop the task from the graph (with an optional human-readable reason).
    #[serde(rename_all = "camelCase")]
    Prune {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Reject the task: warn+prune in run mode, error in check mode.
    #[serde(rename_all = "camelCase")]
    Reject { message: String },
}

/// The subset of a task's spec a worker may replace via `Modify`. Any field
/// left `None` is unchanged; provided fields overwrite the task's value.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TaskModification {
    /// Replacement command (the script/argument the worker runs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Replacement dependency list (config-string form, e.g. `^build`, `#lint`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<Vec<DependsOn>>,
    /// Replacement scheduler weight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<u32>,
}

impl TaskModification {
    /// Applies this modification to a task definition in place.
    pub fn apply_to(&self, definition: &mut TaskDefinition) {
        if let Some(command) = &self.command {
            definition.command = Some(command.clone());
        }
        if let Some(depends_on) = &self.depends_on {
            definition.depends_on = depends_on.clone();
        }
        if let Some(weight) = self.weight {
            definition.weight = weight;
        }
    }
}

/// Response wrapper for a [`ResolveDecision`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct ResolveResult {
    pub decision: ResolveDecision,
}

impl ResolveResult {
    pub fn accept() -> Self {
        Self {
            decision: ResolveDecision::Accept,
        }
    }

    /// Builds a `Modify` result from a full modification spec.
    pub fn modify(modification: TaskModification) -> Self {
        Self {
            decision: ResolveDecision::Modify(modification),
        }
    }

    /// Convenience: a `Modify` that only replaces the command.
    pub fn modify_command(command: impl Into<String>) -> Self {
        Self::modify(TaskModification {
            command: Some(command.into()),
            ..TaskModification::default()
        })
    }

    pub fn prune(reason: Option<String>) -> Self {
        Self {
            decision: ResolveDecision::Prune { reason },
        }
    }

    pub fn reject(message: impl Into<String>) -> Self {
        Self {
            decision: ResolveDecision::Reject {
                message: message.into(),
            },
        }
    }
}

impl From<ResolveDecision> for ResolveResult {
    fn from(decision: ResolveDecision) -> Self {
        Self { decision }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::{json, Value};

    use luchta_types::{DependsOn, TaskName};

    use super::{
        LogStream, ResolveMode, ResolveResult, ResolveTask, TaskModification, WorkerMessage,
        WorkerRequest, WorkerResponse,
    };

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

    #[test]
    fn worker_message_run_round_trips_with_type_tag() {
        let message =
            WorkerMessage::Run(WorkerRequest::new("pkg#build", "build").with_workspace("pkg"));
        let value = serde_json::to_value(&message).expect("message serializes");
        assert_eq!(value["type"], Value::String("run".to_owned()));
        assert_eq!(value["id"], Value::String("pkg#build".to_owned()));
        assert_eq!(value["command"], Value::String("build".to_owned()));

        let decoded: WorkerMessage = serde_json::from_value(value).expect("message deserializes");
        assert_eq!(decoded, message);
        assert_eq!(decoded.id(), "pkg#build");
    }

    #[test]
    fn worker_message_resolve_task_round_trips_and_uses_camel_case() {
        let resolve = ResolveTask {
            id: "pkg#build".to_owned(),
            name: "build".to_owned(),
            command: String::new(),
            package: "@repo/app".to_owned(),
            cwd: Some("packages/app".to_owned()),
            scripts: vec!["build".to_owned(), "test".to_owned()],
            mode: ResolveMode::Check,
        };
        let message = WorkerMessage::from(resolve.clone());
        let value = serde_json::to_value(&message).expect("message serializes");
        assert_eq!(value["type"], Value::String("resolveTask".to_owned()));
        assert_eq!(value["mode"], Value::String("check".to_owned()));

        let decoded: WorkerMessage = serde_json::from_value(value).expect("message deserializes");
        assert_eq!(decoded, WorkerMessage::ResolveTask(resolve));
    }

    #[test]
    fn resolve_mode_defaults_to_run_when_absent() {
        let decoded: ResolveTask = serde_json::from_value(json!({
            "type": "resolveTask",
            "id": "pkg#build",
            "name": "build",
            "package": "@repo/app"
        }))
        .expect("resolve task deserializes");
        assert_eq!(decoded.mode, ResolveMode::Run);
        assert!(decoded.scripts.is_empty());
        assert_eq!(decoded.command, "");
    }

    #[test]
    fn resolved_script_name_prefers_non_blank_command() {
        let with_command = ResolveTask {
            id: "id".to_owned(),
            name: "build".to_owned(),
            command: "  compile  ".to_owned(),
            package: "p".to_owned(),
            cwd: None,
            scripts: Vec::new(),
            mode: ResolveMode::Run,
        };
        assert_eq!(with_command.resolved_script_name(), "compile");

        let blank_command = ResolveTask {
            command: "   ".to_owned(),
            ..with_command.clone()
        };
        assert_eq!(blank_command.resolved_script_name(), "build");
    }

    #[test]
    fn resolve_result_decisions_round_trip() {
        let cases = [
            (ResolveResult::accept(), json!({ "decision": "accept" })),
            (
                ResolveResult::modify_command("compile"),
                json!({ "decision": "modify", "command": "compile" }),
            ),
            (
                ResolveResult::modify(TaskModification {
                    command: Some("compile".to_owned()),
                    depends_on: Some(vec![DependsOn::DirectUpstream(TaskName::from("build"))]),
                    weight: Some(4),
                }),
                json!({
                    "decision": "modify",
                    "command": "compile",
                    "dependsOn": ["^build"],
                    "weight": 4
                }),
            ),
            (
                ResolveResult::modify(TaskModification::default()),
                json!({ "decision": "modify" }),
            ),
            (
                ResolveResult::prune(Some("script `build` missing in `b`".to_owned())),
                json!({ "decision": "prune", "reason": "script `build` missing in `b`" }),
            ),
            (ResolveResult::prune(None), json!({ "decision": "prune" })),
            (
                ResolveResult::reject("nope"),
                json!({ "decision": "reject", "message": "nope" }),
            ),
        ];

        for (result, expected) in cases {
            let value = serde_json::to_value(&result).expect("result serializes");
            assert_eq!(value, expected);
            let decoded: ResolveResult =
                serde_json::from_value(value).expect("result deserializes");
            assert_eq!(decoded, result);
        }
    }

    #[test]
    fn worker_response_resolved_round_trips() {
        let response = WorkerResponse::resolved("pkg#build", ResolveResult::accept());
        let value = serde_json::to_value(&response).expect("response serializes");
        assert_eq!(value["type"], Value::String("resolved".to_owned()));
        assert_eq!(value["id"], Value::String("pkg#build".to_owned()));

        let decoded: WorkerResponse = serde_json::from_value(value).expect("response deserializes");
        assert_eq!(decoded, response);
        assert_eq!(decoded.id(), "pkg#build");
    }
}
