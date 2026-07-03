//! Lockfile abstractions for Luchta.
//!
//! This crate defines a small trait surface for resolving external packages from
//! workspace dependency declarations. Concrete parsers can implement this trait
//! for different lockfile formats.

mod yarn1;
mod yarn_berry;

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};

use thiserror::Error;

pub use yarn1::Yarn1Lockfile;
pub use yarn_berry::YarnBerryLockfile;

/// Resolved package entry in a lockfile.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Package {
    /// Opaque lockfile key used to look up dependency metadata later.
    pub key: String,
    /// Concrete version selected by lockfile.
    pub version: String,
}

impl Package {
    /// Creates a resolved package record.
    pub fn new(key: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            version: version.into(),
        }
    }
}

/// Common lockfile operations needed by workspace graph construction.
pub trait Lockfile: Send + Sync + std::fmt::Debug {
    /// Resolves dependency declaration from a workspace package into lockfile entry.
    fn resolve_package(
        &self,
        workspace_path: &str,
        name: &str,
        version: &str,
    ) -> Result<Option<Package>, LockfileError>;

    /// Returns all direct dependencies for package identified by lockfile key.
    fn all_dependencies(&self, key: &str) -> Result<BTreeMap<String, String>, LockfileError>;

    /// Returns full transitive dependency closure for package identified by lockfile key.
    ///
    /// Result contains external dependency `(name, version)` pairs reachable from
    /// `key` through its dependency closure. In cyclic graphs this may conservatively
    /// include the seed package if a cycle points back to it.
    fn transitive_dependencies(
        &self,
        key: &str,
    ) -> Result<BTreeSet<(String, String)>, LockfileError> {
        let mut dependencies = BTreeSet::new();
        let mut visited = HashSet::new();
        let mut pending = VecDeque::new();

        pending.extend(self.all_dependencies(key)?);

        while let Some((dependency_name, dependency_range)) = pending.pop_front() {
            let Some(package) = self.resolve_package("", &dependency_name, &dependency_range)?
            else {
                continue;
            };

            dependencies.insert((dependency_name, package.version.clone()));

            if visited.insert(package.key.clone()) {
                pending.extend(self.all_dependencies(&package.key)?);
            }
        }

        Ok(dependencies)
    }
}

/// Parses supported Yarn lockfile content and returns matching backend.
///
/// Detects Yarn Berry via mandatory `__metadata` block. Falls back to Yarn v1.
pub fn parse_lockfile(content: &str) -> Result<Box<dyn Lockfile>, LockfileError> {
    if content.contains("__metadata:") {
        YarnBerryLockfile::parse(content).map(|lockfile| Box::new(lockfile) as Box<dyn Lockfile>)
    } else {
        Yarn1Lockfile::parse(content).map(|lockfile| Box::new(lockfile) as Box<dyn Lockfile>)
    }
}

/// Errors produced by lockfile loading or lookup.
#[derive(Debug, Error)]
pub enum LockfileError {
    /// Lockfile contents could not be parsed.
    #[error("failed to parse lockfile: {0}")]
    Parse(String),

    /// Requested package key was malformed for backend.
    #[error("invalid package reference: {0}")]
    InvalidPackageReference(String),
}
