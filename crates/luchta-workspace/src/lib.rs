//! Workspace discovery and package graph primitives for Luchta.
//!
//! Phase 1 exposes minimal stubs only. Real discovery and graph construction
//! logic land in later tasks.

pub mod discovery;
pub mod package_graph;

use thiserror::Error;

pub use discovery::{find_package_name_at, WorkspaceDiscovery, YarnWorkspace};
pub use package_graph::{PackageGraph, PackageNode};

/// Errors produced by workspace discovery and package graph operations.
#[derive(Debug, Error)]
pub enum WorkspaceError {
    /// Workspace metadata could not be read or interpreted.
    #[error("workspace discovery failed: {0}")]
    Discovery(String),

    /// Package graph could not be built or queried.
    #[error("package graph error: {0}")]
    Graph(String),
}
