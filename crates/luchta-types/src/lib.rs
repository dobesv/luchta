//! Shared core domain types for Luchta.
//!
//! These types model package names, task names, task identifiers, and task
//! dependency declarations shared across crates.

use std::fmt;

use serde::{Deserialize, Serialize};

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
    pub depends_on: Vec<DependsOn>,
    /// Relative weight used by weighted scheduler.
    pub weight: u32,
}

impl TaskDefinition {
    /// Creates task definition with explicit dependencies and weight.
    pub fn new(depends_on: Vec<DependsOn>, weight: u32) -> Self {
        Self { depends_on, weight }
    }
}

impl Default for TaskDefinition {
    fn default() -> Self {
        Self {
            depends_on: Vec::new(),
            weight: 1,
        }
    }
}

/// Dependency reference used in task pipeline definitions.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
