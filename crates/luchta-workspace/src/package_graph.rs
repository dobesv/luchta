//! Package graph primitives.
//!
//! Phase 1 keeps wrapper minimal: package node type plus empty directed graph.
//! Graph population and traversal helpers land in Phase 2.

use std::path::PathBuf;

use luchta_types::PackageName;
use petgraph::graph::DiGraph;

/// Workspace package represented as graph node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageNode {
    /// Package name from package.json.
    pub name: PackageName,
    /// Filesystem path to package root.
    pub path: PathBuf,
}

impl PackageNode {
    /// Creates package node from name and path.
    pub fn new(name: PackageName, path: impl Into<PathBuf>) -> Self {
        Self {
            name,
            path: path.into(),
        }
    }
}

/// Directed graph of workspace packages and their dependency edges.
#[derive(Debug, Default)]
pub struct PackageGraph {
    graph: DiGraph<PackageNode, ()>,
}

impl PackageGraph {
    /// Creates empty package graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns number of package nodes currently stored.
    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    /// Returns true when graph contains no package nodes.
    pub fn is_empty(&self) -> bool {
        self.graph.node_count() == 0
    }

    /// Exposes underlying petgraph directed graph.
    pub fn as_graph(&self) -> &DiGraph<PackageNode, ()> {
        &self.graph
    }
}
