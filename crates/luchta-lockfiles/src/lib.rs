//! Lockfile abstractions for Luchta.
//!
//! This crate defines a small trait surface for resolving external packages from
//! workspace dependency declarations. Concrete parsers can implement this trait
//! for different lockfile formats.

mod yarn1;

use std::collections::BTreeMap;

use thiserror::Error;

pub use yarn1::Yarn1Lockfile;

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
