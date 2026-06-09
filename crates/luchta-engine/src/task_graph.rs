use std::collections::{HashMap, HashSet, VecDeque};

use luchta_types::{DependsOn, PackageName, TaskDefinition, TaskId, TaskName};
use luchta_workspace::PackageGraph;
use petgraph::{
    algo::toposort,
    graph::{DiGraph, NodeIndex},
};

use crate::EngineError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskNode {
    pub id: TaskId,
    pub weight: u32,
}

#[derive(Debug, Default)]
pub struct TaskGraph {
    pub graph: DiGraph<TaskNode, ()>,
    pub indices_by_id: HashMap<TaskId, NodeIndex>,
}

impl TaskGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn build(
        package_graph: &PackageGraph,
        pipeline: &HashMap<TaskName, TaskDefinition>,
    ) -> Result<Self, EngineError> {
        let mut graph = DiGraph::new();
        let mut indices_by_id = HashMap::new();

        for package in package_graph.topological_order()? {
            for (task_name, definition) in pipeline {
                let task_id = TaskId::new(package.name.clone(), task_name.clone());
                let node_index = graph.add_node(TaskNode {
                    id: task_id.clone(),
                    weight: definition.weight,
                });
                indices_by_id.insert(task_id, node_index);
            }
        }

        let mut task_graph = Self {
            graph,
            indices_by_id,
        };

        let mut edges = HashSet::new();
        for package in package_graph.topological_order()? {
            for (task_name, definition) in pipeline {
                let source_id = TaskId::new(package.name.clone(), task_name.clone());
                for dependency in &definition.depends_on {
                    for target_id in task_graph.expand_dependency(
                        package_graph,
                        pipeline,
                        &package.name,
                        &source_id,
                        dependency,
                    )? {
                        let source_index = task_graph
                            .node_index(&source_id)
                            .expect("source task node should exist");
                        let target_index = task_graph.node_index(&target_id).ok_or_else(|| {
                            EngineError::UnknownDependencyTarget {
                                from: source_id.clone(),
                                target: target_id.clone(),
                            }
                        })?;
                        edges.insert((source_index, target_index));
                    }
                }
            }
        }

        for (source_index, target_index) in edges {
            task_graph.graph.add_edge(source_index, target_index, ());
        }

        task_graph.validate_acyclic()?;
        Ok(task_graph)
    }

    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }

    pub fn as_graph(&self) -> &DiGraph<TaskNode, ()> {
        &self.graph
    }

    pub fn task_node(&self, task_id: &TaskId) -> Option<&TaskNode> {
        self.node_index(task_id).map(|index| &self.graph[index])
    }

    pub fn topological_order(&self) -> Result<Vec<&TaskNode>, EngineError> {
        let mut order =
            toposort(&self.graph, None).map_err(|cycle| EngineError::TaskGraphCycle {
                task: self.graph[cycle.node_id()].id.clone(),
            })?;
        order.reverse();
        Ok(order.into_iter().map(|index| &self.graph[index]).collect())
    }

    fn expand_dependency(
        &self,
        package_graph: &PackageGraph,
        pipeline: &HashMap<TaskName, TaskDefinition>,
        package_name: &PackageName,
        source_id: &TaskId,
        dependency: &DependsOn,
    ) -> Result<Vec<TaskId>, EngineError> {
        match dependency {
            DependsOn::SamePackage(task_name) => {
                self.ensure_pipeline_task_exists(pipeline, source_id, task_name)?;
                Ok(vec![TaskId::new(package_name.clone(), task_name.clone())])
            }
            DependsOn::DirectUpstream(task_name) => {
                self.ensure_pipeline_task_exists(pipeline, source_id, task_name)?;
                Ok(package_graph
                    .dependencies_of(package_name)?
                    .into_iter()
                    .map(|package| TaskId::new(package.name.clone(), task_name.clone()))
                    .collect())
            }
            DependsOn::TransitiveUpstream(task_name) => {
                self.ensure_pipeline_task_exists(pipeline, source_id, task_name)?;
                Ok(self
                    .transitive_upstream_packages(package_graph, package_name)?
                    .into_iter()
                    .map(|package| TaskId::new(package, task_name.clone()))
                    .collect())
            }
            DependsOn::Specific(task_id) => Ok(vec![task_id.clone()]),
        }
    }

    fn ensure_pipeline_task_exists(
        &self,
        pipeline: &HashMap<TaskName, TaskDefinition>,
        source_id: &TaskId,
        task_name: &TaskName,
    ) -> Result<(), EngineError> {
        if pipeline.contains_key(task_name) {
            Ok(())
        } else {
            Err(EngineError::UnknownDependencyTask {
                from: source_id.clone(),
                task: task_name.clone(),
            })
        }
    }

    fn transitive_upstream_packages(
        &self,
        package_graph: &PackageGraph,
        package_name: &PackageName,
    ) -> Result<Vec<PackageName>, EngineError> {
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();

        enqueue_unvisited_dependencies(package_graph, package_name, &mut visited, &mut queue)?;

        while let Some(current) = queue.pop_front() {
            enqueue_unvisited_dependencies(package_graph, &current, &mut visited, &mut queue)?;
        }

        Ok(visited.into_iter().collect())
    }

    fn validate_acyclic(&self) -> Result<(), EngineError> {
        toposort(&self.graph, None)
            .map(|_| ())
            .map_err(|cycle| EngineError::TaskGraphCycle {
                task: self.graph[cycle.node_id()].id.clone(),
            })
    }

    fn node_index(&self, task_id: &TaskId) -> Option<NodeIndex> {
        self.indices_by_id.get(task_id).copied()
    }
}

/// Pushes the not-yet-visited direct dependencies of `package_name` onto `queue`.
fn enqueue_unvisited_dependencies(
    package_graph: &PackageGraph,
    package_name: &PackageName,
    visited: &mut HashSet<PackageName>,
    queue: &mut VecDeque<PackageName>,
) -> Result<(), EngineError> {
    for dependency in package_graph.dependencies_of(package_name)? {
        if visited.insert(dependency.name.clone()) {
            queue.push_back(dependency.name.clone());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, fs, path::Path};

    use luchta_types::{DependsOn, PackageName, TaskDefinition, TaskId, TaskName};
    use luchta_workspace::{PackageGraph, PackageNode};
    use petgraph::visit::EdgeRef;
    use tempfile::tempdir;

    use super::TaskGraph;
    use crate::EngineError;

    #[test]
    fn builds_direct_upstream_edges_with_dependency_first_topological_order() {
        let package_graph = package_graph_chain();
        let pipeline = HashMap::from([(
            TaskName::from("build"),
            TaskDefinition {
                depends_on: vec![DependsOn::DirectUpstream(TaskName::from("build"))],
                ..TaskDefinition::default()
            },
        )]);

        let task_graph = TaskGraph::build(&package_graph, &pipeline).expect("build task graph");

        assert_eq!(task_graph.node_count(), 3);
        assert!(has_edge(
            &task_graph,
            TaskId::new("@repo/a", "build"),
            TaskId::new("@repo/b", "build")
        ));
        assert!(has_edge(
            &task_graph,
            TaskId::new("@repo/b", "build"),
            TaskId::new("@repo/c", "build")
        ));
        assert!(!has_edge(
            &task_graph,
            TaskId::new("@repo/a", "build"),
            TaskId::new("@repo/c", "build")
        ));

        let order = task_graph
            .topological_order()
            .expect("topological order")
            .into_iter()
            .map(|node| node.id.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            vec![
                "@repo/c#build".to_string(),
                "@repo/b#build".to_string(),
                "@repo/a#build".to_string(),
            ]
        );
    }

    #[test]
    fn builds_transitive_upstream_edges() {
        let package_graph = package_graph_chain();
        let pipeline = HashMap::from([(
            TaskName::from("build"),
            TaskDefinition {
                depends_on: vec![DependsOn::TransitiveUpstream(TaskName::from("build"))],
                ..TaskDefinition::default()
            },
        )]);

        let task_graph = TaskGraph::build(&package_graph, &pipeline).expect("build task graph");

        assert_eq!(task_graph.node_count(), 3);
        assert!(has_edge(
            &task_graph,
            TaskId::new("@repo/a", "build"),
            TaskId::new("@repo/b", "build")
        ));
        assert!(has_edge(
            &task_graph,
            TaskId::new("@repo/a", "build"),
            TaskId::new("@repo/c", "build")
        ));
        assert!(has_edge(
            &task_graph,
            TaskId::new("@repo/b", "build"),
            TaskId::new("@repo/c", "build")
        ));
    }

    #[test]
    fn errors_on_unknown_dependency_targets() {
        let app_build = TaskId::new("@repo/app", "build");

        // An unknown pipeline task name is reported as UnknownDependencyTask.
        let error =
            build_single_dep_error(DependsOn::DirectUpstream(TaskName::from("nonexistent")));
        assert!(matches!(
            error,
            EngineError::UnknownDependencyTask { from, task }
                if from == app_build && task == TaskName::from("nonexistent")
        ));

        // An unknown specific `package#task` target is reported as UnknownDependencyTarget.
        let error = build_single_dep_error(DependsOn::Specific(TaskId::new("ghost", "build")));
        assert!(matches!(
            error,
            EngineError::UnknownDependencyTarget { from, target }
                if from == app_build && target == TaskId::new("ghost", "build")
        ));
    }

    #[test]
    fn detects_same_package_task_cycles() {
        let package_graph = package_graph_single("@repo/app");
        let pipeline = HashMap::from([
            (
                TaskName::from("task-x"),
                TaskDefinition {
                    depends_on: vec![DependsOn::SamePackage(TaskName::from("task-y"))],
                    ..TaskDefinition::default()
                },
            ),
            (
                TaskName::from("task-y"),
                TaskDefinition {
                    depends_on: vec![DependsOn::SamePackage(TaskName::from("task-x"))],
                    ..TaskDefinition::default()
                },
            ),
        ]);

        let error = TaskGraph::build(&package_graph, &pipeline).expect_err("cycle expected");
        assert!(matches!(error, EngineError::TaskGraphCycle { .. }));
    }

    fn has_edge(task_graph: &TaskGraph, source_id: TaskId, target_id: TaskId) -> bool {
        task_graph.as_graph().edge_references().any(|edge| {
            task_graph.as_graph()[edge.source()].id == source_id
                && task_graph.as_graph()[edge.target()].id == target_id
        })
    }

    /// Builds a single-package graph whose `build` task carries `depends_on` and
    /// returns the resulting build error for the caller to match against.
    fn build_single_dep_error(depends_on: DependsOn) -> EngineError {
        let package_graph = package_graph_single("@repo/app");
        let pipeline = HashMap::from([(
            TaskName::from("build"),
            TaskDefinition {
                depends_on: vec![depends_on],
                ..TaskDefinition::default()
            },
        )]);

        TaskGraph::build(&package_graph, &pipeline).expect_err("build error expected")
    }

    fn package_graph_chain() -> PackageGraph {
        let temp_dir = tempdir().expect("create temp dir");
        write_package(
            temp_dir.path().join("packages/a/package.json"),
            PackageManifest {
                name: "@repo/a",
                dependencies: &["@repo/b"],
            },
        );
        write_package(
            temp_dir.path().join("packages/b/package.json"),
            PackageManifest {
                name: "@repo/b",
                dependencies: &["@repo/c"],
            },
        );
        write_package(
            temp_dir.path().join("packages/c/package.json"),
            PackageManifest {
                name: "@repo/c",
                dependencies: &[],
            },
        );

        PackageGraph::build(vec![
            package_node(temp_dir.path().join("packages/a"), "@repo/a"),
            package_node(temp_dir.path().join("packages/b"), "@repo/b"),
            package_node(temp_dir.path().join("packages/c"), "@repo/c"),
        ])
        .expect("build package graph")
    }

    fn package_graph_single(name: &str) -> PackageGraph {
        let temp_dir = tempdir().expect("create temp dir");
        write_package(
            temp_dir.path().join("packages/app/package.json"),
            PackageManifest {
                name,
                dependencies: &[],
            },
        );

        PackageGraph::build(vec![package_node(
            temp_dir.path().join("packages/app"),
            name,
        )])
        .expect("build package graph")
    }

    fn package_node(path: impl AsRef<Path>, name: &str) -> PackageNode {
        PackageNode::new(PackageName::from(name), path.as_ref())
    }

    /// A package.json fixture: the package name plus its workspace dependencies.
    struct PackageManifest<'a> {
        name: &'a str,
        dependencies: &'a [&'a str],
    }

    fn write_package(path: impl AsRef<Path>, manifest: PackageManifest<'_>) {
        let dependencies_json = manifest.dependency_entries_json();
        let name = manifest.name;
        write_json(
            path,
            &format!(
                r#"{{
                    "name": "{name}",
                    "scripts": {{ "build": "echo build" }},
                    "dependencies": {dependencies_json},
                    "devDependencies": {{}}
                }}"#
            ),
        );
    }

    impl PackageManifest<'_> {
        fn dependency_entries_json(&self) -> String {
            if self.dependencies.is_empty() {
                return "{}".to_string();
            }

            let joined = self
                .dependencies
                .iter()
                .map(|name| format!(r#""{name}": "workspace:*""#))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{ {joined} }}")
        }
    }

    fn write_json(path: impl AsRef<Path>, contents: &str) {
        let path = path.as_ref();
        fs::create_dir_all(path.parent().expect("parent dir")).expect("create parent dir");
        fs::write(path, contents).expect("write json");
    }
}
