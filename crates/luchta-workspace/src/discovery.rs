//! Workspace discovery abstractions.
//!
//! Real filesystem walking and package.json parsing land in Phase 2. Current
//! implementation only fixes trait and type shapes for downstream crates.

use std::path::PathBuf;

use crate::{PackageNode, WorkspaceError};

/// Abstraction for loading workspace package metadata from a repository root.
pub trait WorkspaceDiscovery: Send + Sync + std::fmt::Debug {
    /// Discovers packages that participate in current workspace.
    fn discover(&self) -> Result<Vec<PackageNode>, WorkspaceError>;
}

/// Stub Yarn workspace discovery backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YarnWorkspace {
    root: PathBuf,
}

impl YarnWorkspace {
    /// Creates Yarn workspace discovery backend rooted at provided path.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Returns configured workspace root.
    pub fn root(&self) -> &PathBuf {
        &self.root
    }
}

impl WorkspaceDiscovery for YarnWorkspace {
    fn discover(&self) -> Result<Vec<PackageNode>, WorkspaceError> {
        Ok(Vec::new())
    }
}
