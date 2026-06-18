//! Package graph primitives.
//!
//! Graph edges point from dependent package to dependency package: `A -> B`
//! means `A` directly depends on `B`. Topological ordering returned by this
//! module reverses `petgraph::algo::toposort` output so dependencies come before
//! dependents, which matches build-order semantics.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use luchta_types::PackageName;
use petgraph::{
    algo::toposort,
    graph::{DiGraph, NodeIndex},
    Direction,
};
use serde::Deserialize;

use crate::WorkspaceError;

/// Workspace package represented as graph node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageNode {
    /// Package name from package.json.
    pub name: PackageName,
    /// Filesystem path to package root.
    pub path: PathBuf,
    /// Script names declared in the package's `package.json` `scripts` map.
    ///
    /// Empty when the package has no `scripts` field or no `package.json`.
    pub scripts: HashSet<String>,
}

impl PackageNode {
    /// Creates package node from name and path with no declared scripts.
    pub fn new(name: PackageName, path: impl Into<PathBuf>) -> Self {
        Self {
            name,
            path: path.into(),
            scripts: HashSet::new(),
        }
    }

    /// Sets the package's declared script names.
    pub fn with_scripts(mut self, scripts: HashSet<String>) -> Self {
        self.scripts = scripts;
        self
    }

    /// Returns true when the package declares a script with the given name.
    pub fn has_script(&self, name: &str) -> bool {
        self.scripts.contains(name)
    }
}

/// A package graph under construction: the node graph plus a name→index lookup.
type GraphNodes = (DiGraph<PackageNode, ()>, HashMap<PackageName, NodeIndex>);

/// Directed graph of workspace packages and their dependency edges.
#[derive(Debug, Default, Clone)]
pub struct PackageGraph {
    graph: DiGraph<PackageNode, ()>,
    indices_by_name: HashMap<PackageName, NodeIndex>,
    root_package: Option<PackageName>,
}

impl PackageGraph {
    /// Creates empty package graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds graph from discovered workspace packages by re-reading each
    /// package's `package.json` and extracting dependency names from both
    /// `dependencies` and `devDependencies`.
    pub fn build(packages: Vec<PackageNode>) -> Result<Self, WorkspaceError> {
        let (mut graph, indices_by_name) = Self::insert_nodes(packages)?;
        let edges = Self::collect_dependency_edges(&graph, &indices_by_name)?;

        for (source_index, dependency_index) in edges {
            graph.add_edge(source_index, dependency_index, ());
        }

        Ok(Self {
            graph,
            indices_by_name,
            root_package: None,
        })
    }

    /// Tags graph with workspace root package name.
    pub fn with_root_package(mut self, name: PackageName) -> Self {
        self.root_package = Some(name);
        self
    }

    /// Returns workspace root package when known.
    pub fn root_package(&self) -> Option<&PackageName> {
        self.root_package.as_ref()
    }

    /// Adds every package as a node, erroring on duplicate names.
    fn insert_nodes(packages: Vec<PackageNode>) -> Result<GraphNodes, WorkspaceError> {
        let mut graph = DiGraph::new();
        let mut indices_by_name = HashMap::with_capacity(packages.len());

        for package in packages {
            let name = package.name.clone();
            let index = graph.add_node(package);
            if indices_by_name.insert(name.clone(), index).is_some() {
                return Err(WorkspaceError::Graph(format!(
                    "duplicate package name in workspace graph: {name}"
                )));
            }
        }

        Ok((graph, indices_by_name))
    }

    /// Resolves intra-workspace dependency edges from each package's manifest.
    fn collect_dependency_edges(
        graph: &DiGraph<PackageNode, ()>,
        indices_by_name: &HashMap<PackageName, NodeIndex>,
    ) -> Result<HashSet<(NodeIndex, NodeIndex)>, WorkspaceError> {
        let workspace_names: HashSet<_> = indices_by_name.keys().cloned().collect();
        let mut edges = HashSet::new();

        for (source_index, package) in graph.node_indices().zip(graph.node_weights()) {
            let dependency_names =
                read_workspace_dependency_names(&package.path, &workspace_names)?;
            for dependency_name in dependency_names {
                let dependency_index = *indices_by_name.get(&dependency_name).ok_or_else(|| {
                    WorkspaceError::Graph(format!(
                        "workspace dependency '{}' referenced by '{}' was not discovered",
                        dependency_name, package.name
                    ))
                })?;
                edges.insert((source_index, dependency_index));
            }
        }

        Ok(edges)
    }

    /// Returns number of package nodes currently stored.
    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    /// Returns true when graph contains no package nodes.
    pub fn is_empty(&self) -> bool {
        self.graph.node_count() == 0
    }

    /// Returns packages that `name` directly depends on.
    pub fn dependencies_of(&self, name: &PackageName) -> Result<Vec<&PackageNode>, WorkspaceError> {
        let index = self.node_index(name)?;
        Ok(self
            .graph
            .neighbors_directed(index, Direction::Outgoing)
            .map(|dependency_index| &self.graph[dependency_index])
            .collect())
    }

    /// Returns packages that directly depend on `name`.
    pub fn dependents_of(&self, name: &PackageName) -> Result<Vec<&PackageNode>, WorkspaceError> {
        let index = self.node_index(name)?;
        Ok(self
            .graph
            .neighbors_directed(index, Direction::Incoming)
            .map(|dependent_index| &self.graph[dependent_index])
            .collect())
    }

    /// Returns seeds plus all packages that transitively depend on them.
    pub fn transitive_dependents_of(
        &self,
        seeds: impl IntoIterator<Item = PackageName>,
    ) -> Result<HashSet<PackageName>, WorkspaceError> {
        let mut visited = HashSet::new();
        let mut pending = Vec::new();

        for seed in seeds {
            let Some(&index) = self.indices_by_name.get(&seed) else {
                continue;
            };
            if visited.insert(index) {
                pending.push(index);
            }
        }

        while let Some(index) = pending.pop() {
            for dependent_index in self.graph.neighbors_directed(index, Direction::Incoming) {
                if visited.insert(dependent_index) {
                    pending.push(dependent_index);
                }
            }
        }

        Ok(visited
            .into_iter()
            .map(|index| self.graph[index].name.clone())
            .collect())
    }

    /// Returns build-order topological sequence with dependencies before dependents.
    pub fn topological_order(&self) -> Result<Vec<&PackageNode>, WorkspaceError> {
        let mut order = toposort(&self.graph, None).map_err(|cycle| {
            WorkspaceError::Graph(format!(
                "package graph cycle detected at {}",
                self.graph[cycle.node_id()].name
            ))
        })?;
        order.reverse();
        Ok(order.into_iter().map(|index| &self.graph[index]).collect())
    }

    /// Exposes underlying petgraph directed graph.
    pub fn as_graph(&self) -> &DiGraph<PackageNode, ()> {
        &self.graph
    }

    fn node_index(&self, name: &PackageName) -> Result<NodeIndex, WorkspaceError> {
        self.indices_by_name
            .get(name)
            .copied()
            .ok_or_else(|| WorkspaceError::Graph(format!("package '{}' not found in graph", name)))
    }
}

#[derive(Debug, Deserialize)]
struct PackageJsonDependencies {
    #[serde(default)]
    dependencies: HashMap<String, String>,
    #[serde(default, rename = "devDependencies")]
    dev_dependencies: HashMap<String, String>,
}

fn read_workspace_dependency_names(
    package_path: &Path,
    workspace_names: &HashSet<PackageName>,
) -> Result<Vec<PackageName>, WorkspaceError> {
    let package_json_path = package_path.join("package.json");
    let contents = fs::read_to_string(&package_json_path).map_err(|error| {
        WorkspaceError::Graph(format!(
            "failed to read {}: {error}",
            package_json_path.display()
        ))
    })?;

    let package_json: PackageJsonDependencies =
        serde_json::from_str(&contents).map_err(|error| {
            WorkspaceError::Graph(format!(
                "failed to parse {}: {error}",
                package_json_path.display()
            ))
        })?;

    let dependency_names = package_json
        .dependencies
        .keys()
        .chain(package_json.dev_dependencies.keys())
        .map(|name| PackageName::from(name.as_str()))
        .filter(|name| workspace_names.contains(name))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    Ok(dependency_names)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, fs, path::Path, path::PathBuf};

    use tempfile::tempdir;

    use super::{PackageGraph, PackageNode};
    use luchta_types::PackageName;

    #[test]
    fn builds_dependency_graph_and_queries_relationships() {
        let temp_dir = tempdir().expect("create temp dir");
        write_package(
            temp_dir.path().join("packages/a/package.json"),
            package_name("@repo/a"),
            &[package_name("@repo/b")],
            &[],
        );
        write_package(
            temp_dir.path().join("packages/b/package.json"),
            package_name("@repo/b"),
            &[package_name("@repo/c")],
            &[],
        );
        write_package(
            temp_dir.path().join("packages/c/package.json"),
            package_name("@repo/c"),
            &[],
            &[],
        );

        let graph = PackageGraph::build(vec![
            package_node(temp_dir.path().join("packages/a"), package_name("@repo/a")),
            package_node(temp_dir.path().join("packages/b"), package_name("@repo/b")),
            package_node(temp_dir.path().join("packages/c"), package_name("@repo/c")),
        ])
        .expect("build graph");

        assert_eq!(
            package_names(
                graph
                    .dependencies_of(&PackageName::from("@repo/a"))
                    .expect("deps")
            ),
            vec!["@repo/b"]
        );
        assert_eq!(
            package_names(
                graph
                    .dependents_of(&PackageName::from("@repo/c"))
                    .expect("dependents")
            ),
            vec!["@repo/b"]
        );

        let order = package_names(graph.topological_order().expect("topological order"));
        assert_eq!(order, vec!["@repo/c", "@repo/b", "@repo/a"]);
    }

    #[test]
    fn returns_transitive_dependents_for_linear_chain_seeds() {
        let graph = transitive_dependents_graph();

        assert_transitive_dependents(
            &graph,
            [package_name("@repo/a")],
            ["@repo/a", "@repo/b", "@repo/c"],
        );
        assert_transitive_dependents(&graph, [package_name("@repo/b")], ["@repo/b", "@repo/c"]);
        assert_transitive_dependents(&graph, [package_name("@repo/c")], ["@repo/c"]);
        assert!(!graph
            .transitive_dependents_of([package_name("@repo/a")])
            .expect("transitive dependents")
            .contains(&package_name("@repo/d")));
    }

    #[test]
    fn transitive_dependents_skip_unknown_seed_packages() {
        let graph = transitive_dependents_graph();

        assert_transitive_dependents(
            &graph,
            [package_name("@repo/a"), package_name("@repo/missing")],
            ["@repo/a", "@repo/b", "@repo/c"],
        );
    }

    #[test]
    fn transitive_dependents_handle_cycles_without_looping() {
        let graph = transitive_dependents_graph();

        assert_transitive_dependents(&graph, [package_name("@repo/e")], ["@repo/e", "@repo/f"]);
    }

    #[test]
    fn stores_root_package_name() {
        let temp_dir = tempdir().expect("create temp dir");
        write_package(
            temp_dir.path().join("package.json"),
            package_name("root"),
            &[],
            &[],
        );

        let graph = PackageGraph::build(vec![package_node(temp_dir.path(), package_name("root"))])
            .expect("build graph")
            .with_root_package(package_name("root"));

        assert_eq!(graph.root_package(), Some(&PackageName::from("root")));
    }

    #[test]
    fn topological_order_reports_cycle() {
        let temp_dir = tempdir().expect("create temp dir");
        write_package(
            temp_dir.path().join("packages/a/package.json"),
            package_name("@repo/a"),
            &[package_name("@repo/b")],
            &[],
        );
        write_package(
            temp_dir.path().join("packages/b/package.json"),
            package_name("@repo/b"),
            &[package_name("@repo/a")],
            &[],
        );

        let graph = PackageGraph::build(vec![
            package_node(temp_dir.path().join("packages/a"), package_name("@repo/a")),
            package_node(temp_dir.path().join("packages/b"), package_name("@repo/b")),
        ])
        .expect("build graph");

        let error = graph.topological_order().expect_err("cycle expected");
        assert!(error.to_string().contains("cycle detected"));
    }

    #[test]
    fn ignores_external_dependencies() {
        let temp_dir = tempdir().expect("create temp dir");
        write_package(
            temp_dir.path().join("packages/a/package.json"),
            package_name("@repo/a"),
            &[package_name("react")],
            &[package_name("eslint")],
        );
        write_package(
            temp_dir.path().join("packages/b/package.json"),
            package_name("@repo/b"),
            &[],
            &[],
        );

        let graph = PackageGraph::build(vec![
            package_node(temp_dir.path().join("packages/a"), package_name("@repo/a")),
            package_node(temp_dir.path().join("packages/b"), package_name("@repo/b")),
        ])
        .expect("build graph");

        assert!(graph
            .dependencies_of(&PackageName::from("@repo/a"))
            .expect("deps")
            .is_empty());
        assert!(graph
            .dependents_of(&PackageName::from("@repo/b"))
            .expect("dependents")
            .is_empty());
        assert_eq!(graph.as_graph().edge_count(), 0);
    }

    fn transitive_dependents_graph() -> PackageGraph {
        let temp_dir = tempdir().expect("create temp dir");
        write_package(
            temp_dir.path().join("packages/a/package.json"),
            package_name("@repo/a"),
            &[],
            &[],
        );
        write_package(
            temp_dir.path().join("packages/b/package.json"),
            package_name("@repo/b"),
            &[package_name("@repo/a")],
            &[],
        );
        write_package(
            temp_dir.path().join("packages/c/package.json"),
            package_name("@repo/c"),
            &[package_name("@repo/b")],
            &[],
        );
        write_package(
            temp_dir.path().join("packages/d/package.json"),
            package_name("@repo/d"),
            &[],
            &[],
        );
        write_package(
            temp_dir.path().join("packages/e/package.json"),
            package_name("@repo/e"),
            &[package_name("@repo/f")],
            &[],
        );
        write_package(
            temp_dir.path().join("packages/f/package.json"),
            package_name("@repo/f"),
            &[package_name("@repo/e")],
            &[],
        );

        PackageGraph::build(vec![
            package_node(temp_dir.path().join("packages/a"), package_name("@repo/a")),
            package_node(temp_dir.path().join("packages/b"), package_name("@repo/b")),
            package_node(temp_dir.path().join("packages/c"), package_name("@repo/c")),
            package_node(temp_dir.path().join("packages/d"), package_name("@repo/d")),
            package_node(temp_dir.path().join("packages/e"), package_name("@repo/e")),
            package_node(temp_dir.path().join("packages/f"), package_name("@repo/f")),
        ])
        .expect("build graph")
    }

    fn assert_transitive_dependents(
        graph: &PackageGraph,
        seeds: impl IntoIterator<Item = PackageName>,
        expected: impl IntoIterator<Item = &'static str>,
    ) {
        assert_eq!(
            package_name_set(
                graph
                    .transitive_dependents_of(seeds)
                    .expect("transitive dependents")
            ),
            hashset(expected)
        );
    }

    fn package_name(value: &'static str) -> PackageName {
        PackageName::from(value)
    }

    fn package_node(path: impl Into<PathBuf>, name: PackageName) -> PackageNode {
        PackageNode::new(name, path.into())
    }

    fn package_names(packages: Vec<&PackageNode>) -> Vec<String> {
        packages
            .into_iter()
            .map(|package| package.name.to_string())
            .collect()
    }

    fn package_name_set(packages: HashSet<PackageName>) -> HashSet<String> {
        packages
            .into_iter()
            .map(|package| package.to_string())
            .collect()
    }

    fn hashset(entries: impl IntoIterator<Item = &'static str>) -> HashSet<String> {
        entries.into_iter().map(str::to_string).collect()
    }

    fn write_package(
        path: impl AsRef<Path>,
        name: PackageName,
        dependencies: &[PackageName],
        dev_dependencies: &[PackageName],
    ) {
        let dependencies_json = dependency_entries_json(dependencies);
        let dev_dependencies_json = dependency_entries_json(dev_dependencies);
        write_json(
            path,
            &format!(
                r#"{{
                    "name": "{name}",
                    "scripts": {{ "build": "echo build" }},
                    "dependencies": {dependencies_json},
                    "devDependencies": {dev_dependencies_json}
                }}"#
            ),
        );
    }

    fn dependency_entries_json(entries: &[PackageName]) -> String {
        if entries.is_empty() {
            return "{}".to_string();
        }

        let joined = entries
            .iter()
            .map(|name| format!(r#""{name}": "workspace:*""#))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{{ {joined} }}")
    }

    fn write_json(path: impl AsRef<Path>, contents: &str) {
        let path = path.as_ref();
        fs::create_dir_all(path.parent().expect("parent dir")).expect("create parent dir");
        fs::write(path, contents).expect("write json");
    }
}
