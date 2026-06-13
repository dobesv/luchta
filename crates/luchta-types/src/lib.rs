//! Shared core domain types for Luchta.
//!
//! These types model package names, task names, task identifiers, task
//! dependency declarations, and the `luchta-config.*` configuration shared across crates.

mod config;

use std::{collections::BTreeMap, fmt, str::FromStr};

pub use config::*;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

/// Synthetic package id used for workspace-root (`#task`) tasks.
///
/// This sentinel is an internal identifier — it must never be shown to users.
/// Root tasks render as `#task` (see [`TaskId`]'s `Display`).
pub const ROOT_PACKAGE_NAME: &str = "//root";

/// Name of package within workspace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PackageName(pub String);

impl PackageName {
    /// Creates package name from owned string.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Returns inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True when this is the synthetic workspace-root package.
    pub fn is_root(&self) -> bool {
        self.0 == ROOT_PACKAGE_NAME
    }
}

impl From<String> for PackageName {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for PackageName {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl AsRef<str> for PackageName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for PackageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Name of task defined for package.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskName(pub String);

impl TaskName {
    /// Creates task name from owned string.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Returns inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for TaskName {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for TaskName {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl AsRef<str> for TaskName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for TaskName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Fully-qualified task identifier: package plus task name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId {
    pub package: PackageName,
    pub task: TaskName,
}

impl TaskId {
    /// Creates task identifier from package and task names.
    pub fn new(package: impl Into<PackageName>, task: impl Into<TaskName>) -> Self {
        Self {
            package: package.into(),
            task: task.into(),
        }
    }

    /// True when this identifies a workspace-root (`#task`) task.
    pub fn is_root(&self) -> bool {
        self.package.is_root()
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Root tasks render with the `#task` config syntax; the synthetic
        // `//root` package id is an internal detail and never shown.
        if self.is_root() {
            write!(f, "#{}", self.task)
        } else {
            write!(f, "{}#{}", self.package, self.task)
        }
    }
}

/// Build-cache environment variable specification.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvSpec {
    /// Explicit value to provide to task. When absent, value is inherited from
    /// luchta process environment at run time.
    #[serde(default)]
    pub value: Option<String>,
    /// When false, variable is still provided to task but excluded from
    /// significant-environment hash used for cache keys.
    #[serde(default = "default_env_input")]
    pub input: bool,
}

/// Task configuration shared across package graph and execution layers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct CacheConfig {}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskDefinition {
    /// Dependencies that must complete before task may run.
    #[serde(default, rename = "dependsOn", alias = "depends_on")]
    pub depends_on: Vec<DependsOn>,
    /// Relative weight used by weighted scheduler.
    #[serde(default = "default_task_weight")]
    pub weight: u32,
    /// Explicit command for task.
    ///
    /// A command is only executed when task also names a `worker`; worker is
    /// responsible for running it (for worker task without explicit command,
    /// bare task name is used). A command without worker is configuration error.
    /// Tasks are never resolved from `package.json` scripts.
    #[serde(default)]
    pub command: Option<String>,
    /// Named worker used to execute this task. A task without a worker (and
    /// without a command) is a no-op ordering node.
    #[serde(default)]
    pub worker: Option<String>,
    /// Enables build-cache for this task when config object is present.
    #[serde(default)]
    pub cache: Option<CacheConfig>,
    /// Significant input paths or globs relative to workspace/package context.
    #[serde(default)]
    pub inputs: Vec<String>,
    /// Output paths or globs produced by task for cache restore.
    #[serde(default)]
    pub outputs: Vec<String>,
    /// Environment variables provided to task, keyed by variable name.
    #[serde(default)]
    pub env: BTreeMap<String, EnvSpec>,
}

impl TaskDefinition {
    /// Creates task definition with explicit dependencies and weight.
    pub fn new(depends_on: Vec<DependsOn>, weight: u32) -> Self {
        Self {
            depends_on,
            weight,
            command: None,
            worker: None,
            cache: None,
            inputs: Vec::new(),
            outputs: Vec::new(),
            env: BTreeMap::new(),
        }
    }
}

/// Resolves the script/command name a worker task runs: the explicit
/// non-blank, trimmed `command` if present, otherwise the task name.
///
/// This is the single source of truth for the "command overrides task name"
/// rule shared by the engine, CLI, and worker protocol. Both inputs are
/// treated leniently — a blank/whitespace-only command is equivalent to no
/// command.
pub fn resolve_script_name<'a>(command: Option<&'a str>, task_name: &'a str) -> &'a str {
    match command.map(str::trim) {
        Some(command) if !command.is_empty() => command,
        _ => task_name,
    }
}

impl TaskDefinition {
    #[must_use]
    pub fn cache_enabled(&self) -> bool {
        self.cache.is_some()
    }
}

impl Default for TaskDefinition {
    fn default() -> Self {
        Self {
            depends_on: Vec::new(),
            weight: default_task_weight(),
            command: None,
            worker: None,
            cache: None,
            inputs: Vec::new(),
            outputs: Vec::new(),
            env: BTreeMap::new(),
        }
    }
}

fn default_task_weight() -> u32 {
    1
}

fn default_env_input() -> bool {
    true
}

/// Dependency parse error for `DependsOn` string syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseDependsOnError {
    message: String,
}

impl ParseDependsOnError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ParseDependsOnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ParseDependsOnError {}

/// Dependency reference used in task pipeline definitions.
///
/// Serde representation is string-based for config UX:
/// `^task`, `^^task`, `task`, `#task`, or `pkg#task`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DependsOn {
    /// `^task`: task on direct upstream workspace packages.
    DirectUpstream(TaskName),
    /// `^^task`: task on transitive upstream workspace packages.
    TransitiveUpstream(TaskName),
    /// `task`: task in same package.
    SamePackage(TaskName),
    /// `pkg#task`: task in specific package.
    Specific(TaskId),
    /// `#task`: singleton root task.
    Root(TaskName),
}

impl DependsOn {
    pub fn as_config_string(&self) -> String {
        match self {
            Self::DirectUpstream(task) => format!("^{task}"),
            Self::TransitiveUpstream(task) => format!("^^{task}"),
            Self::SamePackage(task) => task.to_string(),
            Self::Specific(task_id) => task_id.to_string(),
            Self::Root(task) => format!("#{task}"),
        }
    }
}

impl fmt::Display for DependsOn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_config_string())
    }
}

impl FromStr for DependsOn {
    type Err = ParseDependsOnError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        // `^^` must be tried before `^` since latter is prefix of former.
        if let Some(parsed) = parse_upstream(
            value,
            "^^",
            Self::TransitiveUpstream,
            "transitive upstream dependency must include task name",
        ) {
            return parsed;
        }

        if let Some(parsed) = parse_upstream(
            value,
            "^",
            Self::DirectUpstream,
            "direct upstream dependency must include task name",
        ) {
            return parsed;
        }

        if let Some(task) = value.strip_prefix('#') {
            if task.is_empty() {
                return Err(ParseDependsOnError::new(
                    "root dependency must include task name",
                ));
            }

            return Ok(Self::Root(TaskName::from(task)));
        }

        if let Some((package, task)) = value.split_once('#') {
            if package.is_empty() || task.is_empty() {
                return Err(ParseDependsOnError::new(
                    "specific dependency must be `package#task`",
                ));
            }

            return Ok(Self::Specific(TaskId::new(package, task)));
        }

        if value.is_empty() {
            return Err(ParseDependsOnError::new(
                "same-package dependency must include task name",
            ));
        }

        Ok(Self::SamePackage(TaskName::from(value)))
    }
}

/// Parses upstream dependency (`^`/`^^` prefixed) from `value`.
///
/// Returns `None` when `value` lacks `prefix` so caller can try next form.
/// When prefix matches, yields constructed variant, or an error using
/// `empty_message` if task name is empty.
fn parse_upstream(
    value: &str,
    prefix: &str,
    construct: fn(TaskName) -> DependsOn,
    empty_message: &str,
) -> Option<Result<DependsOn, ParseDependsOnError>> {
    let task = value.strip_prefix(prefix)?;
    if task.is_empty() {
        return Some(Err(ParseDependsOnError::new(empty_message)));
    }
    Some(Ok(construct(TaskName::from(task))))
}

impl Serialize for DependsOn {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.as_config_string())
    }
}

impl<'de> Deserialize<'de> for DependsOn {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        resolve_script_name, CacheConfig, DependsOn, EnvSpec, PackageName, TaskDefinition, TaskId,
        TaskName, ROOT_PACKAGE_NAME,
    };

    #[test]
    fn root_task_id_displays_with_hash_syntax_not_synthetic_package() {
        let id = TaskId::new(
            PackageName::from(ROOT_PACKAGE_NAME),
            TaskName::from("release"),
        );
        assert!(id.is_root());
        // The synthetic `//root` package id must never leak into output.
        assert_eq!(id.to_string(), "#release");
        assert!(!id.to_string().contains("//root"));
    }

    #[test]
    fn package_scoped_task_id_displays_with_package_prefix() {
        let id = TaskId::new(PackageName::from("@repo/app"), TaskName::from("build"));
        assert!(!id.is_root());
        assert_eq!(id.to_string(), "@repo/app#build");
    }

    #[test]
    fn resolve_script_name_prefers_non_blank_command_else_task_name() {
        assert_eq!(resolve_script_name(Some("  serve "), "start"), "serve");
        assert_eq!(resolve_script_name(Some("   "), "start"), "start");
        assert_eq!(resolve_script_name(None, "start"), "start");
    }

    #[test]
    fn parses_root_task_dependency() {
        assert_eq!(
            "#build".parse::<DependsOn>().expect("root task"),
            DependsOn::Root(TaskName::from("build"))
        );
    }

    #[test]
    fn rejects_empty_root_task_dependency() {
        let error = "#"
            .parse::<DependsOn>()
            .expect_err("missing root task name");
        assert_eq!(error.to_string(), "root dependency must include task name");
    }

    #[test]
    fn root_task_dependency_formats_as_config_string() {
        assert_eq!(
            DependsOn::Root(TaskName::from("test")).as_config_string(),
            "#test"
        );
    }

    #[test]
    fn serde_round_trips_root_task_dependency() {
        let dependency = DependsOn::Root(TaskName::from("test"));
        let json = serde_json::to_string(&dependency).expect("serialize dependency");
        assert_eq!(json, "\"#test\"");

        let parsed: DependsOn = serde_json::from_str(&json).expect("deserialize dependency");
        assert_eq!(parsed, dependency);
    }

    #[test]
    fn serde_round_trips_env_spec_variants() {
        let with_value: EnvSpec =
            serde_json::from_str(r#"{"value":"x"}"#).expect("deserialize value variant");
        assert_eq!(
            with_value,
            EnvSpec {
                value: Some("x".to_owned()),
                input: true,
            }
        );
        assert_eq!(
            serde_json::to_string(&with_value).expect("serialize value variant"),
            r#"{"value":"x","input":true}"#
        );

        let excluded_input: EnvSpec =
            serde_json::from_str(r#"{"input":false}"#).expect("deserialize input variant");
        assert_eq!(
            excluded_input,
            EnvSpec {
                value: None,
                input: false,
            }
        );
        assert_eq!(
            serde_json::to_string(&excluded_input).expect("serialize input variant"),
            r#"{"value":null,"input":false}"#
        );

        let inherited: EnvSpec = serde_json::from_str(r#"{}"#).expect("deserialize empty env spec");
        assert_eq!(
            inherited,
            EnvSpec {
                value: None,
                input: true,
            }
        );
        assert_eq!(
            serde_json::to_string(&inherited).expect("serialize empty env spec"),
            r#"{"value":null,"input":true}"#
        );
    }

    #[test]
    fn task_definition_defaults_new_cache_fields_when_omitted() {
        let task: TaskDefinition = serde_json::from_str(r#"{"dependsOn":["^build"],"weight":2}"#)
            .expect("deserialize task without cache fields");

        assert_eq!(
            task.depends_on,
            vec![DependsOn::DirectUpstream(TaskName::from("build"))]
        );
        assert_eq!(task.weight, 2);
        assert_eq!(task.cache, None);
        assert!(!task.cache_enabled());
        assert!(task.inputs.is_empty());
        assert!(task.outputs.is_empty());
        assert_eq!(task.env, BTreeMap::new());
    }

    #[test]
    fn task_definition_enables_cache_when_object_present() {
        let task: TaskDefinition =
            serde_json::from_str(r#"{"cache":{}}"#).expect("deserialize task with cache object");

        assert_eq!(task.cache, Some(CacheConfig::default()));
        assert!(task.cache_enabled());
    }

    #[test]
    fn task_definition_rejects_boolean_cache_field() {
        let error = serde_json::from_str::<TaskDefinition>(r#"{"cache":true}"#)
            .expect_err("boolean cache field must fail");

        assert!(error.to_string().contains("invalid type: boolean `true`"));
    }
}
