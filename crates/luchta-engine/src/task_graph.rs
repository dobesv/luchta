use std::collections::{HashMap, HashSet, VecDeque};

use luchta_types::{DependsOn, PackageName, TaskDefinition, TaskId, TaskName};
use luchta_workspace::PackageGraph;
use petgraph::{
    algo::toposort,
    graph::{DiGraph, NodeIndex},
};
use thiserror::Error;

use crate::EngineError;

pub const ROOT_PACKAGE_NAME: &str = "//root";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskNode {
    pub id: TaskId,
    pub weight: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum PipelineKey {
    Global {
        task: TaskName,
    },
    Package {
        package: PackageName,
        task: TaskName,
    },
    Root {
        task: TaskName,
    },
}

#[derive(Debug, Clone)]
pub struct TaskGraph {
    graph: DiGraph<TaskNode, ()>,
    indices_by_id: HashMap<TaskId, NodeIndex>,
    definitions_by_id: HashMap<TaskId, TaskDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedPipeline {
    tasks_by_id: HashMap<TaskId, TaskDefinition>,
    task_names_by_package: HashMap<PackageName, HashSet<TaskName>>,
    root_task_names: HashSet<TaskName>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DependencyValidationError {
    #[error("task validation failed")]
    InvalidTasks {
        diagnostics: Vec<TaskValidationDiagnostic>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskValidationDiagnostic {
    pub task_id: TaskId,
    pub reason: TaskValidationReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskValidationReason {
    DeadDependencyReference {
        dependency: DependsOn,
        reason: DeadDependencyReason,
    },
    CommandWithoutWorker,
    UnknownWorker {
        worker: String,
    },
}

impl std::fmt::Display for TaskValidationReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeadDependencyReference { dependency, reason } => {
                write!(f, "{}: {}", dependency, reason)
            }
            Self::CommandWithoutWorker => write!(
                f,
                "defines a command but no worker; specify a worker to execute it"
            ),
            Self::UnknownWorker { worker } => {
                write!(f, "references unknown worker '{worker}'")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadDependencyReference {
    pub source: TaskId,
    pub dependency: DependsOn,
    pub reason: DeadDependencyReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeadDependencyReason {
    UnknownTaskEverywhere {
        task: TaskName,
    },
    UnknownTaskInPackage {
        package: PackageName,
        task: TaskName,
    },
    UnknownPackage {
        package: PackageName,
    },
    UnknownRootTask {
        task: TaskName,
    },
}

impl std::fmt::Display for DeadDependencyReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTaskEverywhere { task } => {
                write!(f, "task '{task}' not declared in any package")
            }
            Self::UnknownTaskInPackage { package, task } => {
                write!(f, "task '{task}' not declared in package '{package}'")
            }
            Self::UnknownPackage { package } => {
                write!(f, "package '{package}' not found in workspace")
            }
            Self::UnknownRootTask { task } => {
                write!(f, "root task '#{task}' not declared")
            }
        }
    }
}

impl TaskGraph {
    pub fn build(
        package_graph: &PackageGraph,
        pipeline: &HashMap<TaskName, TaskDefinition>,
    ) -> Result<Self, EngineError> {
        let resolved_pipeline = ResolvedPipeline::build(package_graph, pipeline)?;
        let root_package = root_package_name();
        let mut graph = DiGraph::new();
        let mut indices_by_id = HashMap::new();

        for (task_id, definition) in &resolved_pipeline.tasks_by_id {
            let index = graph.add_node(TaskNode {
                id: task_id.clone(),
                weight: definition.weight,
            });
            indices_by_id.insert(task_id.clone(), index);
        }

        let mut with_edges = Self {
            graph,
            indices_by_id,
            definitions_by_id: resolved_pipeline.tasks_by_id.clone(),
        };

        for (source_id, definition) in &resolved_pipeline.tasks_by_id {
            for dependency in &definition.depends_on {
                for dependency_id in with_edges.expand_dependency(
                    source_id,
                    package_graph,
                    &resolved_pipeline,
                    &root_package,
                    dependency,
                )? {
                    with_edges.add_edge_if_present(source_id, &dependency_id);
                }
            }
        }

        with_edges.validate_acyclic()?;
        Ok(with_edges)
    }

    /// Validates the resolved pipeline for configuration problems that `luchta
    /// check` should surface: dead dependency references, commands declared
    /// without a worker, and references to workers that are not defined.
    pub fn validate_tasks(
        package_graph: &PackageGraph,
        pipeline: &HashMap<TaskName, TaskDefinition>,
        worker_names: &HashSet<String>,
    ) -> Result<(), DependencyValidationError> {
        let resolved_pipeline = ResolvedPipeline::build(package_graph, pipeline)
            .expect("resolved pipeline should match task graph construction");
        let mut diagnostics = Vec::new();

        for (task_id, definition) in &resolved_pipeline.tasks_by_id {
            match &definition.worker {
                Some(worker) if !worker_names.contains(worker) => {
                    diagnostics.push(TaskValidationDiagnostic {
                        task_id: task_id.clone(),
                        reason: TaskValidationReason::UnknownWorker {
                            worker: worker.clone(),
                        },
                    });
                }
                // A blank/whitespace-only command is treated as absent (the run
                // path trims commands too), so it is not a CommandWithoutWorker error.
                None if has_non_blank_command(definition) => {
                    diagnostics.push(TaskValidationDiagnostic {
                        task_id: task_id.clone(),
                        reason: TaskValidationReason::CommandWithoutWorker,
                    });
                }
                _ => {}
            }

            for dependency in &definition.depends_on {
                if let Some(reason) =
                    dead_dependency_reason(package_graph, &resolved_pipeline, task_id, dependency)
                {
                    diagnostics.push(TaskValidationDiagnostic {
                        task_id: task_id.clone(),
                        reason: TaskValidationReason::DeadDependencyReference {
                            dependency: dependency.clone(),
                            reason,
                        },
                    });
                }
            }
        }

        if diagnostics.is_empty() {
            return Ok(());
        }

        diagnostics.sort_by(|left, right| {
            left.task_id
                .to_string()
                .cmp(&right.task_id.to_string())
                .then_with(|| left.reason.to_string().cmp(&right.reason.to_string()))
        });
        diagnostics.dedup();

        Err(DependencyValidationError::InvalidTasks { diagnostics })
    }

    pub fn nodes(&self) -> impl Iterator<Item = &TaskNode> {
        self.graph.node_weights()
    }

    pub fn dependencies_of(&self, task_id: &TaskId) -> Vec<&TaskNode> {
        let Some(index) = self.node_index(task_id) else {
            return Vec::new();
        };

        self.graph
            .neighbors_directed(index, petgraph::Direction::Outgoing)
            .map(|dependency_index| &self.graph[dependency_index])
            .collect()
    }

    fn add_edge_if_present(&mut self, source: &TaskId, dependency: &TaskId) {
        let (Some(&source_index), Some(&dependency_index)) = (
            self.indices_by_id.get(source),
            self.indices_by_id.get(dependency),
        ) else {
            return;
        };

        if self
            .graph
            .find_edge(source_index, dependency_index)
            .is_none()
        {
            self.graph.add_edge(source_index, dependency_index, ());
        }
    }

    fn expand_dependency(
        &self,
        source_task_id: &TaskId,
        package_graph: &PackageGraph,
        resolved_pipeline: &ResolvedPipeline,
        root_package: &PackageName,
        dependency: &DependsOn,
    ) -> Result<Vec<TaskId>, EngineError> {
        match dependency {
            DependsOn::SamePackage(task_name) => {
                // A same-package dependency resolves only to the task of the SAME
                // package as the source. If that package does not declare the task,
                // the edge is simply skipped (no cross-package resolution).
                let candidate = TaskId::new(source_task_id.package.clone(), task_name.clone());
                Ok(self
                    .task_definition(&candidate)
                    .map(|_| vec![candidate])
                    .unwrap_or_default())
            }
            DependsOn::Specific(task_id) => Ok(self
                .task_definition(task_id)
                .map(|_| vec![task_id.clone()])
                .unwrap_or_default()),
            DependsOn::DirectUpstream(task_name) => {
                let candidates = self.task_definition_candidates(resolved_pipeline, task_name);
                Ok(candidates
                    .into_iter()
                    .filter(|task_id| {
                        self.is_direct_upstream(
                            source_task_id,
                            task_id,
                            package_graph,
                            root_package,
                        )
                    })
                    .collect())
            }
            DependsOn::TransitiveUpstream(task_name) => {
                let candidates = self.task_definition_candidates(resolved_pipeline, task_name);
                Ok(candidates
                    .into_iter()
                    .filter(|task_id| {
                        self.is_transitive_upstream(
                            source_task_id,
                            task_id,
                            package_graph,
                            root_package,
                        )
                    })
                    .collect())
            }
            DependsOn::Root(task_name) => Ok(self
                .task_definition_candidates(resolved_pipeline, task_name)
                .into_iter()
                .filter(|task_id| &task_id.package == root_package)
                .collect()),
        }
    }

    pub fn task_definition(&self, task_id: &TaskId) -> Option<&TaskDefinition> {
        self.definitions_by_id.get(task_id)
    }

    pub fn task_node(&self, task_id: &TaskId) -> Option<&TaskNode> {
        self.node_index(task_id).map(|index| &self.graph[index])
    }

    pub fn as_graph(&self) -> &DiGraph<TaskNode, ()> {
        &self.graph
    }

    pub fn topological_order(&self) -> Result<Vec<&TaskNode>, EngineError> {
        let order = toposort(&self.graph, None).map_err(|cycle| EngineError::TaskGraphCycle {
            task: self.graph[cycle.node_id()].id.clone(),
        })?;

        let mut order = order
            .into_iter()
            .map(|index| &self.graph[index])
            .collect::<Vec<_>>();
        order.reverse();
        Ok(order)
    }

    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }

    fn node_index(&self, task_id: &TaskId) -> Option<NodeIndex> {
        self.indices_by_id.get(task_id).copied()
    }

    fn task_definition_candidates(
        &self,
        resolved_pipeline: &ResolvedPipeline,
        task_name: &TaskName,
    ) -> Vec<TaskId> {
        resolved_pipeline
            .tasks_by_id
            .keys()
            .filter(|task_id| &task_id.task == task_name)
            .cloned()
            .collect()
    }

    fn is_direct_upstream(
        &self,
        source_task_id: &TaskId,
        dependency_task_id: &TaskId,
        package_graph: &PackageGraph,
        root_package: &PackageName,
    ) -> bool {
        if &dependency_task_id.package == root_package {
            return false;
        }

        package_graph
            .dependencies_of(&source_task_id.package)
            .map(|dependencies| {
                dependencies
                    .into_iter()
                    .any(|dependency| dependency.name == dependency_task_id.package)
            })
            .unwrap_or(false)
    }

    fn is_transitive_upstream(
        &self,
        source_task_id: &TaskId,
        dependency_task_id: &TaskId,
        package_graph: &PackageGraph,
        root_package: &PackageName,
    ) -> bool {
        if &dependency_task_id.package == root_package {
            return false;
        }

        transitive_upstream_packages(package_graph, &source_task_id.package)
            .map(|packages| {
                packages
                    .into_iter()
                    .any(|package_name| package_name == dependency_task_id.package)
            })
            .unwrap_or(false)
    }

    fn validate_acyclic(&self) -> Result<(), EngineError> {
        self.topological_order().map(|_| ())
    }
}

impl ResolvedPipeline {
    fn build(
        package_graph: &PackageGraph,
        pipeline: &HashMap<TaskName, TaskDefinition>,
    ) -> Result<Self, EngineError> {
        let package_names = package_graph
            .topological_order()?
            .into_iter()
            .map(|package| package.name.clone())
            .collect::<HashSet<_>>();
        let parsed_pipeline = parse_pipeline_entries(pipeline, &package_names);
        let mut tasks_by_id = HashMap::new();
        let mut task_names_by_package = HashMap::new();
        let mut root_task_names = HashSet::new();
        let mut global_tasks = HashMap::new();
        let mut package_tasks = HashMap::new();
        let mut root_tasks = HashMap::new();

        for (pipeline_key, definition) in parsed_pipeline {
            match pipeline_key {
                PipelineKey::Global { task } => {
                    global_tasks.insert(task, definition);
                }
                PipelineKey::Package { package, task } => {
                    package_tasks.insert((package, task), definition);
                }
                PipelineKey::Root { task } => {
                    root_tasks.insert(task, definition);
                }
            }
        }

        for package_name in package_graph
            .topological_order()?
            .into_iter()
            .map(|package| package.name.clone())
        {
            let mut task_names = HashSet::new();

            for (task_name, definition) in &global_tasks {
                let task_id = TaskId::new(package_name.clone(), task_name.clone());
                tasks_by_id.insert(task_id, definition.clone());
                task_names.insert(task_name.clone());
            }

            for ((package, task_name), definition) in &package_tasks {
                if package == &package_name {
                    let task_id = TaskId::new(package_name.clone(), task_name.clone());
                    tasks_by_id.insert(task_id, definition.clone());
                    task_names.insert(task_name.clone());
                }
            }

            if !task_names.is_empty() {
                task_names_by_package.insert(package_name, task_names);
            }
        }

        for (task, definition) in root_tasks {
            let task_id = root_task_id(task.clone());
            tasks_by_id.insert(task_id, definition);
            root_task_names.insert(task);
        }

        Ok(Self {
            tasks_by_id,
            task_names_by_package,
            root_task_names,
        })
    }

    fn contains_package_task(&self, package_name: &PackageName, task_name: &TaskName) -> bool {
        self.task_names_by_package
            .get(package_name)
            .is_some_and(|task_names| task_names.contains(task_name))
    }

    fn contains_root_task(&self, task_name: &TaskName) -> bool {
        self.root_task_names.contains(task_name)
    }

    fn task_exists_in_any_package(&self, task_name: &TaskName) -> bool {
        self.task_names_by_package
            .values()
            .any(|task_names| task_names.contains(task_name))
    }
}

fn dead_dependency_reason(
    package_graph: &PackageGraph,
    resolved_pipeline: &ResolvedPipeline,
    source_task_id: &TaskId,
    dependency: &DependsOn,
) -> Option<DeadDependencyReason> {
    match dependency {
        DependsOn::SamePackage(task_name) => {
            // A bare task-name dependency resolves within the source's own scope.
            // For a root-task source that is the set of root tasks; otherwise it
            // is dead only when no package declares the task anywhere.
            let exists = if is_root_task(source_task_id) {
                resolved_pipeline.contains_root_task(task_name)
            } else {
                resolved_pipeline.task_exists_in_any_package(task_name)
            };
            (!exists).then(|| DeadDependencyReason::UnknownTaskEverywhere {
                task: task_name.clone(),
            })
        }
        DependsOn::Specific(task_id) => {
            let package_exists = package_graph
                .topological_order()
                .map(|packages| {
                    packages
                        .into_iter()
                        .any(|package| package.name == task_id.package)
                })
                .unwrap_or(false);
            if !package_exists {
                return Some(DeadDependencyReason::UnknownPackage {
                    package: task_id.package.clone(),
                });
            }

            let exists_in_package =
                resolved_pipeline.contains_package_task(&task_id.package, &task_id.task);
            (!exists_in_package).then(|| DeadDependencyReason::UnknownTaskInPackage {
                package: task_id.package.clone(),
                task: task_id.task.clone(),
            })
        }
        DependsOn::DirectUpstream(task_name) | DependsOn::TransitiveUpstream(task_name) => {
            let exists_anywhere = resolved_pipeline.task_exists_in_any_package(task_name);
            (!exists_anywhere).then(|| DeadDependencyReason::UnknownTaskEverywhere {
                task: task_name.clone(),
            })
        }
        DependsOn::Root(task_name) => {
            (!resolved_pipeline.contains_root_task(task_name)).then(|| {
                DeadDependencyReason::UnknownRootTask {
                    task: task_name.clone(),
                }
            })
        }
    }
}

fn transitive_upstream_packages(
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

fn parse_pipeline_entries(
    pipeline: &HashMap<TaskName, TaskDefinition>,
    package_names: &HashSet<PackageName>,
) -> Vec<(PipelineKey, TaskDefinition)> {
    let mut parsed = Vec::with_capacity(pipeline.len());

    for (raw_key, definition) in pipeline {
        let key_str = raw_key.as_str();

        let key = if let Some(task) = key_str.strip_prefix('#') {
            // `#task` root key. An empty task name (`#`) is malformed; skip it.
            if task.is_empty() {
                continue;
            }
            PipelineKey::Root {
                task: TaskName::from(task),
            }
        } else if let Some((package, task)) = key_str.split_once('#') {
            // `pkg#task` package-scoped key. Reject empty package or task names,
            // and drop keys for packages that are not in the workspace.
            let package_name = PackageName::from(package);
            if package.is_empty() || task.is_empty() || !package_names.contains(&package_name) {
                continue;
            }
            PipelineKey::Package {
                package: package_name,
                task: TaskName::from(task),
            }
        } else {
            PipelineKey::Global {
                task: raw_key.clone(),
            }
        };

        parsed.push((key, definition.clone()));
    }

    parsed
}

/// Returns true when a task declares a non-empty, non-whitespace command.
///
/// The run path trims commands before executing, so a blank command is
/// equivalent to no command and must not be treated as a misconfiguration.
fn has_non_blank_command(definition: &TaskDefinition) -> bool {
    definition
        .command
        .as_deref()
        .map(str::trim)
        .is_some_and(|command| !command.is_empty())
}

pub fn root_package_name() -> PackageName {
    PackageName::from(ROOT_PACKAGE_NAME)
}

pub fn root_task_id(task_name: TaskName) -> TaskId {
    TaskId::new(root_package_name(), task_name)
}

pub fn is_root_task(task_id: &TaskId) -> bool {
    task_id.package == root_package_name()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        fs,
        path::Path,
    };

    use tempfile::tempdir;

    use luchta_types::{DependsOn, PackageName, TaskDefinition, TaskId, TaskName};
    use luchta_workspace::{PackageGraph, PackageNode};

    use super::{
        root_task_id, DeadDependencyReason, DependencyValidationError, TaskGraph,
        TaskValidationDiagnostic, TaskValidationReason,
    };
    use crate::EngineError;

    #[test]
    fn builds_direct_upstream_edges() {
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
        assert_eq!(task_graph.edge_count(), 2);
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
    fn same_package_dependencies_support_colons_in_task_names() {
        let package_graph = package_graph_single("@repo/app");
        let pipeline = HashMap::from([
            (
                TaskName::from("build"),
                TaskDefinition {
                    depends_on: vec![DependsOn::SamePackage(TaskName::from(
                        "build:graphql-codegen",
                    ))],
                    ..TaskDefinition::default()
                },
            ),
            (
                TaskName::from("build:graphql-codegen"),
                TaskDefinition::default(),
            ),
        ]);

        let task_graph = TaskGraph::build(&package_graph, &pipeline).expect("build task graph");

        assert!(has_edge(
            &task_graph,
            TaskId::new("@repo/app", "build"),
            TaskId::new("@repo/app", "build:graphql-codegen")
        ));
    }

    #[test]
    fn package_scoped_override_shadows_global_for_own_package_only() {
        let package_graph = package_graph_chain();
        let pipeline = HashMap::from([
            (
                TaskName::from("build"),
                TaskDefinition {
                    weight: 1,
                    ..TaskDefinition::default()
                },
            ),
            (
                TaskName::from("@repo/b#build"),
                TaskDefinition {
                    weight: 5,
                    ..TaskDefinition::default()
                },
            ),
        ]);

        let task_graph = TaskGraph::build(&package_graph, &pipeline).expect("build task graph");

        assert_eq!(
            task_graph
                .task_node(&TaskId::new("@repo/a", "build"))
                .map(|node| node.weight),
            Some(1)
        );
        assert_eq!(
            task_graph
                .task_node(&TaskId::new("@repo/b", "build"))
                .map(|node| node.weight),
            Some(5)
        );
        assert_eq!(
            task_graph
                .task_node(&TaskId::new("@repo/c", "build"))
                .map(|node| node.weight),
            Some(1)
        );
    }

    #[test]
    fn task_definition_prefers_package_override_over_global_definition() {
        let package_graph = package_graph_chain();
        let pipeline = HashMap::from([
            (
                TaskName::from("build"),
                TaskDefinition {
                    depends_on: vec![DependsOn::SamePackage(TaskName::from("prepare"))],
                    weight: 1,
                    ..TaskDefinition::default()
                },
            ),
            (
                TaskName::from("@repo/b#build"),
                TaskDefinition {
                    depends_on: vec![DependsOn::SamePackage(TaskName::from("bundle"))],
                    weight: 5,
                    ..TaskDefinition::default()
                },
            ),
            (TaskName::from("prepare"), TaskDefinition::default()),
            (TaskName::from("@repo/b#bundle"), TaskDefinition::default()),
        ]);

        let task_graph = TaskGraph::build(&package_graph, &pipeline).expect("build task graph");

        assert_eq!(
            task_graph
                .task_definition(&TaskId::new("@repo/a", "build"))
                .expect("global definition for @repo/a")
                .depends_on,
            vec![DependsOn::SamePackage(TaskName::from("prepare"))]
        );
        assert_eq!(
            task_graph
                .task_definition(&TaskId::new("@repo/b", "build"))
                .expect("package override for @repo/b")
                .depends_on,
            vec![DependsOn::SamePackage(TaskName::from("bundle"))]
        );
        assert_eq!(
            task_graph
                .task_node(&TaskId::new("@repo/b", "build"))
                .map(|node| node.weight),
            Some(5)
        );
    }

    #[test]
    fn package_scoped_key_does_not_leak_onto_other_packages() {
        let package_graph = package_graph_chain();
        let pipeline = HashMap::from([(
            TaskName::from("@repo/b#build"),
            TaskDefinition {
                ..TaskDefinition::default()
            },
        )]);

        let task_graph = TaskGraph::build(&package_graph, &pipeline).expect("build task graph");

        assert!(task_graph
            .task_node(&TaskId::new("@repo/b", "build"))
            .is_some());
        assert!(task_graph
            .task_node(&TaskId::new("@repo/a", "build"))
            .is_none());
        assert!(task_graph
            .task_node(&TaskId::new("@repo/c", "build"))
            .is_none());
    }

    #[test]
    fn unknown_package_pipeline_key_is_skipped() {
        let package_graph = package_graph_chain();
        let pipeline =
            HashMap::from([(TaskName::from("nosuchpkg#build"), TaskDefinition::default())]);

        let task_graph = TaskGraph::build(&package_graph, &pipeline).expect("build task graph");

        assert_eq!(task_graph.node_count(), 0);
    }

    #[test]
    fn pipeline_keys_with_empty_task_names_are_skipped() {
        let package_graph = package_graph_chain();
        let pipeline = HashMap::from([
            // `#` (empty root task) and `@repo/a#` (empty package task) are malformed.
            (TaskName::from("#"), TaskDefinition::default()),
            (TaskName::from("@repo/a#"), TaskDefinition::default()),
        ]);

        let task_graph = TaskGraph::build(&package_graph, &pipeline).expect("build task graph");

        assert_eq!(task_graph.node_count(), 0);
    }

    #[test]
    fn blank_command_without_worker_is_not_flagged() {
        let package_graph = package_graph_single("@repo/app");
        let pipeline = HashMap::from([(
            TaskName::from("build"),
            TaskDefinition {
                // Whitespace-only command is equivalent to no command.
                command: Some("   ".to_string()),
                ..TaskDefinition::default()
            },
        )]);

        TaskGraph::validate_tasks(&package_graph, &pipeline, &HashSet::new())
            .expect("blank command without worker should not be a validation error");
    }

    #[test]
    fn root_task_same_package_dependency_on_another_root_task_is_not_flagged() {
        // A root task depending (via a bare task name) on another root task must
        // resolve against the root task set, not be reported as a dead dependency.
        let package_graph = package_graph_single("@repo/app");
        let pipeline = HashMap::from([
            (
                TaskName::from("#release"),
                TaskDefinition {
                    depends_on: vec![DependsOn::SamePackage(TaskName::from("audit"))],
                    ..TaskDefinition::default()
                },
            ),
            (TaskName::from("#audit"), TaskDefinition::default()),
        ]);

        // The graph links the two root tasks...
        let task_graph = TaskGraph::build(&package_graph, &pipeline).expect("build task graph");
        assert!(has_edge(
            &task_graph,
            root_task_id(TaskName::from("release")),
            root_task_id(TaskName::from("audit")),
        ));
        // ...and validation must not flag it as dead.
        TaskGraph::validate_tasks(&package_graph, &pipeline, &HashSet::new())
            .expect("root-to-root same-package dependency should be valid");
    }

    #[test]
    fn root_task_same_package_dependency_on_undeclared_root_task_is_flagged() {
        let package_graph = package_graph_single("@repo/app");
        let pipeline = HashMap::from([(
            TaskName::from("#release"),
            TaskDefinition {
                depends_on: vec![DependsOn::SamePackage(TaskName::from("ghost"))],
                ..TaskDefinition::default()
            },
        )]);

        let error = TaskGraph::validate_tasks(&package_graph, &pipeline, &HashSet::new())
            .expect_err("undeclared root dependency should fail validation");

        assert_eq!(
            error,
            DependencyValidationError::InvalidTasks {
                diagnostics: vec![TaskValidationDiagnostic {
                    task_id: root_task_id(TaskName::from("release")),
                    reason: TaskValidationReason::DeadDependencyReference {
                        dependency: DependsOn::SamePackage(TaskName::from("ghost")),
                        reason: DeadDependencyReason::UnknownTaskEverywhere {
                            task: TaskName::from("ghost"),
                        },
                    },
                }],
            }
        );
    }

    #[test]
    fn root_scoped_task_creates_singleton_root_node() {
        let package_graph = package_graph_chain();
        let pipeline = HashMap::from([(
            TaskName::from("#lint"),
            TaskDefinition {
                ..TaskDefinition::default()
            },
        )]);

        let task_graph = TaskGraph::build(&package_graph, &pipeline).expect("build task graph");

        assert_eq!(task_graph.node_count(), 1);
        assert!(task_graph
            .task_node(&root_task_id(TaskName::from("lint")))
            .is_some());
    }

    #[test]
    fn root_dependency_links_package_task_to_root_node() {
        let package_graph = package_graph_single("@repo/app");
        let pipeline = HashMap::from([
            (
                TaskName::from("build"),
                TaskDefinition {
                    depends_on: vec![DependsOn::Root(TaskName::from("prepare"))],
                    ..TaskDefinition::default()
                },
            ),
            (TaskName::from("#prepare"), TaskDefinition::default()),
        ]);

        let task_graph = TaskGraph::build(&package_graph, &pipeline).expect("build task graph");

        assert!(has_edge(
            &task_graph,
            TaskId::new("@repo/app", "build"),
            root_task_id(TaskName::from("prepare"))
        ));
    }

    #[test]
    fn missing_specific_dependency_is_ignored() {
        let task_graph =
            build_single_dep_graph(DependsOn::Specific(TaskId::new("@repo/other", "build")));

        assert_eq!(task_graph.edge_count(), 0);
    }

    #[test]
    fn missing_root_dependency_is_ignored() {
        let task_graph = build_single_dep_graph(DependsOn::Root(TaskName::from("prepare")));

        assert_eq!(task_graph.edge_count(), 0);
    }

    #[test]
    fn unresolvable_same_package_dependency_is_ignored() {
        let task_graph = build_single_dep_graph(DependsOn::SamePackage(TaskName::from("prepare")));

        assert_eq!(task_graph.edge_count(), 0);
    }

    #[test]
    fn same_package_dependency_does_not_resolve_across_packages() {
        // Regression: a SamePackage dependency must resolve ONLY within the
        // source package, even when another package declares the same task name
        // via a package-scoped key. Resolving it cross-package previously caused
        // a massive spurious cycle.
        let package_graph = package_graph_chain();
        let pipeline = HashMap::from([
            (
                // Global `build` (materialized per package) depends on same-package `gen`.
                TaskName::from("build"),
                TaskDefinition {
                    depends_on: vec![DependsOn::SamePackage(TaskName::from("gen"))],
                    ..TaskDefinition::default()
                },
            ),
            (
                // `gen` is declared ONLY for package @repo/b.
                TaskName::from("@repo/b#gen"),
                TaskDefinition::default(),
            ),
        ]);

        let task_graph = TaskGraph::build(&package_graph, &pipeline).expect("task graph builds");

        // @repo/b#build resolves its same-package gen.
        assert!(has_edge(
            &task_graph,
            TaskId::new("@repo/b", "build"),
            TaskId::new("@repo/b", "gen"),
        ));
        // @repo/a#build must NOT pick up @repo/b's gen (cross-package leak).
        assert!(!has_edge(
            &task_graph,
            TaskId::new("@repo/a", "build"),
            TaskId::new("@repo/b", "gen"),
        ));
        // @repo/c#build must NOT pick it up either.
        assert!(!has_edge(
            &task_graph,
            TaskId::new("@repo/c", "build"),
            TaskId::new("@repo/b", "gen"),
        ));
    }

    #[test]
    fn detects_cycle_between_same_package_tasks() {
        let package_graph = package_graph_single("@repo/app");
        let pipeline = HashMap::from([
            (
                TaskName::from("build"),
                TaskDefinition {
                    depends_on: vec![DependsOn::SamePackage(TaskName::from("lint"))],
                    ..TaskDefinition::default()
                },
            ),
            (
                TaskName::from("lint"),
                TaskDefinition {
                    depends_on: vec![DependsOn::SamePackage(TaskName::from("build"))],
                    ..TaskDefinition::default()
                },
            ),
        ]);

        let error = TaskGraph::build(&package_graph, &pipeline).expect_err("cycle expected");

        assert!(matches!(
            error,
            EngineError::TaskGraphCycle { task } if task == TaskId::new("@repo/app", "build")
                || task == TaskId::new("@repo/app", "lint")
        ));
    }

    #[test]
    fn validates_dead_same_package_dependency() {
        let package_graph = package_graph_single("@repo/app");
        let pipeline = HashMap::from([(
            TaskName::from("build"),
            TaskDefinition {
                depends_on: vec![DependsOn::SamePackage(TaskName::from("ghost"))],
                ..TaskDefinition::default()
            },
        )]);

        let error = TaskGraph::validate_tasks(&package_graph, &pipeline, &HashSet::new())
            .expect_err("dead dependency expected");

        assert_eq!(
            error,
            DependencyValidationError::InvalidTasks {
                diagnostics: vec![TaskValidationDiagnostic {
                    task_id: TaskId::new("@repo/app", "build"),
                    reason: TaskValidationReason::DeadDependencyReference {
                        dependency: DependsOn::SamePackage(TaskName::from("ghost")),
                        reason: DeadDependencyReason::UnknownTaskEverywhere {
                            task: TaskName::from("ghost"),
                        },
                    },
                }],
            }
        );
    }

    #[test]
    fn validates_dead_specific_dependency_for_unknown_task_and_package() {
        let package_graph = package_graph_chain();
        let pipeline = HashMap::from([(
            TaskName::from("build"),
            TaskDefinition {
                depends_on: vec![
                    DependsOn::Specific(TaskId::new("@repo/missing", "build")),
                    DependsOn::Specific(TaskId::new("@repo/b", "ghost")),
                ],
                ..TaskDefinition::default()
            },
        )]);

        let error = TaskGraph::validate_tasks(&package_graph, &pipeline, &HashSet::new())
            .expect_err("dead dependencies expected");

        assert_eq!(
            error,
            DependencyValidationError::InvalidTasks {
                diagnostics: vec![
                    TaskValidationDiagnostic {
                        task_id: TaskId::new("@repo/a", "build"),
                        reason: TaskValidationReason::DeadDependencyReference {
                            dependency: DependsOn::Specific(TaskId::new("@repo/b", "ghost")),
                            reason: DeadDependencyReason::UnknownTaskInPackage {
                                package: PackageName::from("@repo/b"),
                                task: TaskName::from("ghost"),
                            },
                        },
                    },
                    TaskValidationDiagnostic {
                        task_id: TaskId::new("@repo/a", "build"),
                        reason: TaskValidationReason::DeadDependencyReference {
                            dependency: DependsOn::Specific(TaskId::new("@repo/missing", "build")),
                            reason: DeadDependencyReason::UnknownPackage {
                                package: PackageName::from("@repo/missing"),
                            },
                        },
                    },
                    TaskValidationDiagnostic {
                        task_id: TaskId::new("@repo/b", "build"),
                        reason: TaskValidationReason::DeadDependencyReference {
                            dependency: DependsOn::Specific(TaskId::new("@repo/b", "ghost")),
                            reason: DeadDependencyReason::UnknownTaskInPackage {
                                package: PackageName::from("@repo/b"),
                                task: TaskName::from("ghost"),
                            },
                        },
                    },
                    TaskValidationDiagnostic {
                        task_id: TaskId::new("@repo/b", "build"),
                        reason: TaskValidationReason::DeadDependencyReference {
                            dependency: DependsOn::Specific(TaskId::new("@repo/missing", "build")),
                            reason: DeadDependencyReason::UnknownPackage {
                                package: PackageName::from("@repo/missing"),
                            },
                        },
                    },
                    TaskValidationDiagnostic {
                        task_id: TaskId::new("@repo/c", "build"),
                        reason: TaskValidationReason::DeadDependencyReference {
                            dependency: DependsOn::Specific(TaskId::new("@repo/b", "ghost")),
                            reason: DeadDependencyReason::UnknownTaskInPackage {
                                package: PackageName::from("@repo/b"),
                                task: TaskName::from("ghost"),
                            },
                        },
                    },
                    TaskValidationDiagnostic {
                        task_id: TaskId::new("@repo/c", "build"),
                        reason: TaskValidationReason::DeadDependencyReference {
                            dependency: DependsOn::Specific(TaskId::new("@repo/missing", "build")),
                            reason: DeadDependencyReason::UnknownPackage {
                                package: PackageName::from("@repo/missing"),
                            },
                        },
                    },
                ],
            }
        );
    }

    #[test]
    fn validates_dead_root_dependency() {
        let package_graph = package_graph_single("@repo/app");
        let pipeline = HashMap::from([(
            TaskName::from("build"),
            TaskDefinition {
                depends_on: vec![DependsOn::Root(TaskName::from("audit-licenses"))],
                ..TaskDefinition::default()
            },
        )]);

        let error = TaskGraph::validate_tasks(&package_graph, &pipeline, &HashSet::new())
            .expect_err("dead root dependency expected");

        assert_eq!(
            error,
            DependencyValidationError::InvalidTasks {
                diagnostics: vec![TaskValidationDiagnostic {
                    task_id: TaskId::new("@repo/app", "build"),
                    reason: TaskValidationReason::DeadDependencyReference {
                        dependency: DependsOn::Root(TaskName::from("audit-licenses")),
                        reason: DeadDependencyReason::UnknownRootTask {
                            task: TaskName::from("audit-licenses"),
                        },
                    },
                }],
            }
        );
    }

    #[test]
    fn validates_dead_direct_upstream_dependency_when_no_direct_upstream_defines_task() {
        let package_graph = package_graph_chain();
        let pipeline = HashMap::from([(
            TaskName::from("build"),
            TaskDefinition {
                depends_on: vec![DependsOn::DirectUpstream(TaskName::from("prepare"))],
                ..TaskDefinition::default()
            },
        )]);

        let error = TaskGraph::validate_tasks(&package_graph, &pipeline, &HashSet::new())
            .expect_err("dead upstream dependency expected");

        assert_eq!(
            error,
            DependencyValidationError::InvalidTasks {
                diagnostics: vec![
                    TaskValidationDiagnostic {
                        task_id: TaskId::new("@repo/a", "build"),
                        reason: TaskValidationReason::DeadDependencyReference {
                            dependency: DependsOn::DirectUpstream(TaskName::from("prepare")),
                            reason: DeadDependencyReason::UnknownTaskEverywhere {
                                task: TaskName::from("prepare"),
                            },
                        },
                    },
                    TaskValidationDiagnostic {
                        task_id: TaskId::new("@repo/b", "build"),
                        reason: TaskValidationReason::DeadDependencyReference {
                            dependency: DependsOn::DirectUpstream(TaskName::from("prepare")),
                            reason: DeadDependencyReason::UnknownTaskEverywhere {
                                task: TaskName::from("prepare"),
                            },
                        },
                    },
                    TaskValidationDiagnostic {
                        task_id: TaskId::new("@repo/c", "build"),
                        reason: TaskValidationReason::DeadDependencyReference {
                            dependency: DependsOn::DirectUpstream(TaskName::from("prepare")),
                            reason: DeadDependencyReason::UnknownTaskEverywhere {
                                task: TaskName::from("prepare"),
                            },
                        },
                    },
                ],
            }
        );
    }

    #[test]
    fn validates_dead_transitive_upstream_dependency_when_no_upstream_defines_task() {
        let package_graph = package_graph_chain();
        let pipeline = HashMap::from([(
            TaskName::from("build"),
            TaskDefinition {
                depends_on: vec![DependsOn::TransitiveUpstream(TaskName::from("prepare"))],
                ..TaskDefinition::default()
            },
        )]);

        let error = TaskGraph::validate_tasks(&package_graph, &pipeline, &HashSet::new())
            .expect_err("dead transitive upstream dependency expected");

        assert_eq!(
            error,
            DependencyValidationError::InvalidTasks {
                diagnostics: vec![
                    TaskValidationDiagnostic {
                        task_id: TaskId::new("@repo/a", "build"),
                        reason: TaskValidationReason::DeadDependencyReference {
                            dependency: DependsOn::TransitiveUpstream(TaskName::from("prepare")),
                            reason: DeadDependencyReason::UnknownTaskEverywhere {
                                task: TaskName::from("prepare"),
                            },
                        },
                    },
                    TaskValidationDiagnostic {
                        task_id: TaskId::new("@repo/b", "build"),
                        reason: TaskValidationReason::DeadDependencyReference {
                            dependency: DependsOn::TransitiveUpstream(TaskName::from("prepare")),
                            reason: DeadDependencyReason::UnknownTaskEverywhere {
                                task: TaskName::from("prepare"),
                            },
                        },
                    },
                    TaskValidationDiagnostic {
                        task_id: TaskId::new("@repo/c", "build"),
                        reason: TaskValidationReason::DeadDependencyReference {
                            dependency: DependsOn::TransitiveUpstream(TaskName::from("prepare")),
                            reason: DeadDependencyReason::UnknownTaskEverywhere {
                                task: TaskName::from("prepare"),
                            },
                        },
                    },
                ],
            }
        );
    }

    #[test]
    fn does_not_flag_transitive_upstream_dependency_when_upstream_defines_task() {
        let package_graph = package_graph_chain();
        let pipeline = HashMap::from([
            (
                TaskName::from("build"),
                TaskDefinition {
                    depends_on: vec![DependsOn::TransitiveUpstream(TaskName::from("prepare"))],
                    ..TaskDefinition::default()
                },
            ),
            (TaskName::from("@repo/c#prepare"), TaskDefinition::default()),
        ]);

        TaskGraph::validate_tasks(&package_graph, &pipeline, &HashSet::new())
            .expect("transitive upstream resolution should be tolerated");
    }

    #[test]
    fn does_not_flag_upstream_dependency_that_only_resolves_for_some_packages() {
        let package_graph = package_graph_chain();
        let pipeline = HashMap::from([
            (TaskName::from("build"), TaskDefinition::default()),
            (
                TaskName::from("@repo/b#build"),
                TaskDefinition {
                    depends_on: vec![DependsOn::DirectUpstream(TaskName::from("prepare"))],
                    ..TaskDefinition::default()
                },
            ),
            (TaskName::from("@repo/c#prepare"), TaskDefinition::default()),
        ]);

        TaskGraph::validate_tasks(&package_graph, &pipeline, &HashSet::new())
            .expect("partial upstream resolution should be tolerated");
    }

    #[test]
    fn considers_only_declared_tasks_during_validation() {
        let package_graph = package_graph_single("@repo/app");
        let pipeline = HashMap::from([
            (
                TaskName::from("build"),
                TaskDefinition {
                    depends_on: vec![
                        DependsOn::SamePackage(TaskName::from("lint")),
                        DependsOn::Root(TaskName::from("audit-licenses")),
                    ],
                    ..TaskDefinition::default()
                },
            ),
            (TaskName::from("lint"), TaskDefinition::default()),
            (TaskName::from("#audit-licenses"), TaskDefinition::default()),
        ]);

        TaskGraph::validate_tasks(&package_graph, &pipeline, &HashSet::new())
            .expect("declared tasks should satisfy dependency validation");
    }

    #[test]
    fn reports_command_without_worker_validation_error() {
        let package_graph = package_graph_single("@repo/app");
        let pipeline = HashMap::from([(
            TaskName::from("build"),
            TaskDefinition {
                command: Some("echo build".to_string()),
                ..TaskDefinition::default()
            },
        )]);

        let error = TaskGraph::validate_tasks(&package_graph, &pipeline, &HashSet::new())
            .expect_err("command without worker should fail validation");

        assert_eq!(
            error,
            DependencyValidationError::InvalidTasks {
                diagnostics: vec![TaskValidationDiagnostic {
                    task_id: TaskId::new("@repo/app", "build"),
                    reason: TaskValidationReason::CommandWithoutWorker,
                }],
            }
        );
    }

    #[test]
    fn reports_unknown_worker_validation_error() {
        let package_graph = package_graph_single("@repo/app");
        let pipeline = HashMap::from([(
            TaskName::from("build"),
            TaskDefinition {
                worker: Some("missing".to_string()),
                ..TaskDefinition::default()
            },
        )]);

        // No workers are defined, so referencing one is invalid.
        let error = TaskGraph::validate_tasks(&package_graph, &pipeline, &HashSet::new())
            .expect_err("unknown worker should fail validation");

        assert_eq!(
            error,
            DependencyValidationError::InvalidTasks {
                diagnostics: vec![TaskValidationDiagnostic {
                    task_id: TaskId::new("@repo/app", "build"),
                    reason: TaskValidationReason::UnknownWorker {
                        worker: "missing".to_string(),
                    },
                }],
            }
        );

        // A defined worker passes validation.
        let workers = HashSet::from(["missing".to_string()]);
        TaskGraph::validate_tasks(&package_graph, &pipeline, &workers)
            .expect("defined worker should pass validation");
    }

    fn has_edge(task_graph: &TaskGraph, source_id: TaskId, target_id: TaskId) -> bool {
        let source_index = task_graph
            .as_graph()
            .node_indices()
            .find(|index| task_graph.as_graph()[*index].id == source_id);
        let target_index = task_graph
            .as_graph()
            .node_indices()
            .find(|index| task_graph.as_graph()[*index].id == target_id);

        match (source_index, target_index) {
            (Some(source), Some(target)) => task_graph.as_graph().contains_edge(source, target),
            _ => false,
        }
    }

    fn build_single_dep_graph(depends_on: DependsOn) -> TaskGraph {
        let package_graph = package_graph_single("@repo/app");
        let pipeline = HashMap::from([(
            TaskName::from("build"),
            TaskDefinition {
                depends_on: vec![depends_on],
                ..TaskDefinition::default()
            },
        )]);

        TaskGraph::build(&package_graph, &pipeline).expect("task graph should build")
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
