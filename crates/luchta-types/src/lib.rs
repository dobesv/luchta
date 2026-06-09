//! Shared core domain types for Luchta.
//!
//! These types model package names, task names, task identifiers, task
//! dependency declarations, and the `luchta-config.*` configuration shared across crates.

mod config;

use std::{fmt, str::FromStr};

pub use config::*;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

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
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.package, self.task)
    }
}

/// Task configuration shared across package graph and execution layers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskDefinition {
    /// Dependencies that must complete before task may run.
    #[serde(default, rename = "dependsOn", alias = "depends_on")]
    pub depends_on: Vec<DependsOn>,
    /// Relative weight used by weighted scheduler.
    #[serde(default = "default_task_weight")]
    pub weight: u32,
    /// Explicit command line for task.
    ///
    /// Command resolution priority: explicit `command` in the config first,
    /// otherwise matching `scripts` entry from `package.json` for task name.
    #[serde(default)]
    pub command: Option<String>,
}

impl TaskDefinition {
    /// Creates task definition with explicit dependencies and weight.
    pub fn new(depends_on: Vec<DependsOn>, weight: u32) -> Self {
        Self {
            depends_on,
            weight,
            command: None,
        }
    }
}

impl Default for TaskDefinition {
    fn default() -> Self {
        Self {
            depends_on: Vec::new(),
            weight: default_task_weight(),
            command: None,
        }
    }
}

fn default_task_weight() -> u32 {
    1
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
/// `^task`, `^^task`, `task`, or `pkg#task`.
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
}

impl DependsOn {
    fn as_config_string(&self) -> String {
        match self {
            Self::DirectUpstream(task) => format!("^{}", task),
            Self::TransitiveUpstream(task) => format!("^^{}", task),
            Self::SamePackage(task) => task.to_string(),
            Self::Specific(task_id) => task_id.to_string(),
        }
    }
}

impl FromStr for DependsOn {
    type Err = ParseDependsOnError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        // `^^` must be tried before `^` since the latter is a prefix of the former.
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

/// Parses an upstream dependency (`^`/`^^` prefixed) from `value`.
///
/// Returns `None` when `value` lacks `prefix` so the caller can try the next
/// form. When the prefix matches, yields the constructed variant, or an error
/// using `empty_message` if the task name is empty.
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
