use std::collections::{HashMap, HashSet, VecDeque};

use luchta_types::{DependsOn, PackageName, TaskDefinition, TaskId, TaskName, WorkerDefinition};
use luchta_workspace::{PackageGraph, PackageNode};
use petgraph::{
    algo::toposort,
    graph::{DiGraph, NodeIndex},
};
use thiserror::Error;

use crate::worker::protocol::{ResolveDecision, ResolveMode, ResolveTask};
use crate::EngineError;

/// Per-package context the resolution phase passes to a worker so it can decide
/// a task's fate without reading the filesystem.
#[derive(Debug, Clone, Default)]
pub struct PackageResolveInfo {
    /// Package root directory (string form), for diagnostics / worker cwd.
    pub cwd: Option<String>,
    /// Script names declared by the package.
    pub scripts: Vec<String>,
}

impl PackageResolveInfo {
    fn from_node(package: &PackageNode) -> Self {
        let mut scripts: Vec<String> = package.scripts.iter().cloned().collect();
        scripts.sort();
        Self {
            cwd: Some(package.path.to_string_lossy().into_owned()),
            scripts,
        }
    }

    /// Builds the per-package resolution lookup from discovered package nodes.
    pub fn map_from_packages(packages: &[PackageNode]) -> HashMap<PackageName, PackageResolveInfo> {
        packages
            .iter()
            .map(|package| (package.name.clone(), Self::from_node(package)))
            .collect()
    }

    /// Like [`map_from_packages`], but also registers the workspace-root
    /// package under the synthetic [`ROOT_PACKAGE_NAME`] so worker resolution of
    /// root (`#task`) tasks sees the root package's scripts and cwd. The root
    /// package is the discovered node whose path is `workspace_root`.
    pub fn map_from_packages_with_root(
        packages: &[PackageNode],
        workspace_root: &std::path::Path,
    ) -> HashMap<PackageName, PackageResolveInfo> {
        let mut map = Self::map_from_packages(packages);
        if let Some(root) = packages
            .iter()
            .find(|package| package.path == workspace_root)
        {
            map.insert(PackageName::from(ROOT_PACKAGE_NAME), Self::from_node(root));
        }
        map
    }
}

/// Re-exported from `luchta-types`, the single source of truth for the
/// synthetic workspace-root package id.
pub use luchta_types::ROOT_PACKAGE_NAME;

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

/// Outcome of resolving a single task that did not result in `Accept`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrunedTask {
    /// The scoped id of the task that was removed from the graph.
    pub task_id: TaskId,
    /// Why it was removed.
    pub outcome: PruneOutcome,
}

/// The reason a task was excluded from the graph during resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PruneOutcome {
    /// Worker returned `Prune` (a clean, expected exclusion).
    Pruned { reason: Option<String> },
    /// Worker returned `Reject`. In run mode this is downgraded to a prune
    /// (with a warning); in check mode it is an error.
    Rejected { message: String },
}

impl PruneOutcome {
    /// Human-readable explanation for diagnostics / CLI output.
    pub fn describe(&self) -> String {
        match self {
            Self::Pruned { reason } => reason
                .clone()
                .unwrap_or_else(|| "pruned by worker".to_owned()),
            Self::Rejected { message } => message.clone(),
        }
    }

    /// True when this outcome originated from a worker `Reject`.
    pub fn is_rejected(&self) -> bool {
        matches!(self, Self::Rejected { .. })
    }
}

/// Error raised when the resolution phase cannot complete (e.g. a worker
/// round-trip failed), or when check mode encounters a `Reject`.
#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("worker resolution failed for task '{task}': {message}")]
    Worker { task: TaskId, message: String },
    #[error("task '{task}' rejected by worker: {message}")]
    Rejected { task: TaskId, message: String },
}

/// Abstraction over the worker round-trip used to resolve a task. The engine
/// implements this with a real `WorkerManager`; tests use a stub.
pub trait TaskResolver {
    /// Resolve a single task by sending it to its worker and awaiting the
    /// decision.
    fn resolve(
        &self,
        worker: &str,
        request: crate::worker::protocol::ResolveTask,
    ) -> impl std::future::Future<Output = Result<crate::worker::protocol::ResolveResult, String>>;
}

impl TaskGraph {
    pub fn build(
        package_graph: &PackageGraph,
        pipeline: &HashMap<TaskName, TaskDefinition>,
    ) -> Result<Self, EngineError> {
        let resolved_pipeline = ResolvedPipeline::build(package_graph, pipeline)?;
        Self::from_resolved_pipeline(package_graph, &resolved_pipeline)
    }

    /// Builds the task graph after running the worker-mediated resolution phase.
    ///
    /// Each expanded task with a worker is sent to that worker via `resolver`;
    /// the returned decision is applied before nodes/edges are materialized, so
    /// pruned tasks never become graph nodes. Returns the graph plus the list of
    /// tasks that were pruned (for CLI/diagnostic reporting). In check mode a
    /// worker `Reject` aborts with [`ResolveError::Rejected`]; in run mode it is
    /// downgraded to a prune entry.
    ///
    /// Worker definitions are used to inject native `depends_on` dependencies
    /// from each worker's configuration into tasks that use that worker. This
    /// injection happens after resolution (so worker `Modify` decisions cannot
    /// erase injected deps) but before prune application (so injected deps are
    /// subject to normal pruning rules).
    pub async fn build_resolved<R: TaskResolver>(
        package_graph: &PackageGraph,
        pipeline: &HashMap<TaskName, TaskDefinition>,
        packages: &HashMap<PackageName, PackageResolveInfo>,
        worker_definitions: &HashMap<String, WorkerDefinition>,
        resolver: &R,
        mode: ResolveMode,
    ) -> Result<(Self, Vec<PrunedTask>), EngineError> {
        let mut resolved_pipeline = ResolvedPipeline::build(package_graph, pipeline)?;
        let pruned = resolved_pipeline.resolve(packages, resolver, mode).await?;
        resolved_pipeline.inject_worker_dependencies(worker_definitions);
        resolved_pipeline.apply_prunes(&pruned);
        let graph = Self::from_resolved_pipeline(package_graph, &resolved_pipeline)?;
        Ok((graph, pruned))
    }

    fn from_resolved_pipeline(
        package_graph: &PackageGraph,
        resolved_pipeline: &ResolvedPipeline,
    ) -> Result<Self, EngineError> {
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
                    resolved_pipeline,
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
        worker_definitions: &HashMap<String, WorkerDefinition>,
    ) -> Result<(), DependencyValidationError> {
        Self::validate_tasks_with_pruned(
            package_graph,
            pipeline,
            worker_definitions,
            &HashSet::new(),
        )
    }

    /// Like [`validate_tasks`], but tolerant of references to tasks that were
    /// intentionally pruned during the resolution phase.
    ///
    /// The resolved pipeline is rebuilt and the given `pruned` ids removed, so a
    /// surviving task whose `depends_on` points at a pruned task is NOT flagged
    /// as a dead dependency (the dropped edge is informational, not an error).
    /// A dependency on a task that never existed still errors.
    pub fn validate_tasks_with_pruned(
        package_graph: &PackageGraph,
        pipeline: &HashMap<TaskName, TaskDefinition>,
        worker_definitions: &HashMap<String, WorkerDefinition>,
        pruned: &HashSet<TaskId>,
    ) -> Result<(), DependencyValidationError> {
        let mut resolved_pipeline = ResolvedPipeline::build(package_graph, pipeline)
            .expect("resolved pipeline should match task graph construction");
        resolved_pipeline.inject_worker_dependencies(worker_definitions);
        // Apply prunes after worker dependency injection so validation sees the
        // same dependency shape as graph construction before pruned targets drop.
        if !pruned.is_empty() {
            let pruned_tasks: Vec<PrunedTask> = pruned
                .iter()
                .cloned()
                .map(|task_id| PrunedTask {
                    task_id,
                    outcome: PruneOutcome::Pruned { reason: None },
                })
                .collect();
            resolved_pipeline.apply_prunes(&pruned_tasks);
        }
        let worker_names: HashSet<String> = worker_definitions.keys().cloned().collect();
        let mut diagnostics = Vec::new();
        let dependency_context = DependencyContext::new(package_graph, &resolved_pipeline, pruned);

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
                if let Some(reason) = dependency_context.dead_dependency_reason(task_id, dependency)
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
            let skip_global_tasks_for_root = package_graph.root_package() == Some(&package_name);

            if !skip_global_tasks_for_root {
                for (task_name, definition) in &global_tasks {
                    let task_id = TaskId::new(package_name.clone(), task_name.clone());
                    tasks_by_id.insert(task_id, definition.clone());
                    task_names.insert(task_name.clone());
                }
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

    /// Runs the worker-mediated resolution phase over every expanded task that
    /// has a worker. Tasks without a worker are left untouched (pure ordering
    /// nodes). For each worker-task the resolver is asked for a decision:
    ///
    /// - `Accept`  → keep unchanged.
    /// - `Modify`  → replace the task's command in place.
    /// - `Prune`   → record for removal.
    /// - `Reject`  → run mode: record for removal (caller warns); check mode:
    ///   error out immediately.
    ///
    /// Returns the set of tasks to prune (applied separately via
    /// [`apply_prunes`]) so the caller can surface them.
    async fn resolve<R: TaskResolver>(
        &mut self,
        packages: &HashMap<PackageName, PackageResolveInfo>,
        resolver: &R,
        mode: ResolveMode,
    ) -> Result<Vec<PrunedTask>, ResolveError> {
        // Deterministic iteration order so prune diagnostics are stable.
        let mut task_ids: Vec<TaskId> = self.tasks_by_id.keys().cloned().collect();
        task_ids.sort_by_key(|task_id| task_id.to_string());

        let mut pruned = Vec::new();

        for task_id in task_ids {
            let definition = &self.tasks_by_id[&task_id];
            let Some(worker) = definition.worker.clone() else {
                continue;
            };

            let info = packages.get(&task_id.package);
            let request = ResolveTask {
                id: task_id.to_string(),
                name: task_id.task.as_str().to_owned(),
                command: definition
                    .command
                    .clone()
                    .map(|command| command.trim().to_owned())
                    .unwrap_or_default(),
                package: task_id.package.as_str().to_owned(),
                cwd: info.and_then(|info| info.cwd.clone()),
                scripts: info.map(|info| info.scripts.clone()).unwrap_or_default(),
                mode,
            };

            let result = resolver
                .resolve(&worker, request)
                .await
                .map_err(|message| ResolveError::Worker {
                    task: task_id.clone(),
                    message,
                })?;

            match result.decision {
                ResolveDecision::Accept => {}
                ResolveDecision::Modify(modification) => {
                    if let Some(definition) = self.tasks_by_id.get_mut(&task_id) {
                        modification.apply_to(definition);
                    }
                }
                ResolveDecision::Prune { reason } => {
                    pruned.push(PrunedTask {
                        task_id,
                        outcome: PruneOutcome::Pruned { reason },
                    });
                }
                ResolveDecision::Reject { message } => {
                    if mode == ResolveMode::Check {
                        return Err(ResolveError::Rejected {
                            task: task_id,
                            message,
                        });
                    }
                    pruned.push(PrunedTask {
                        task_id,
                        outcome: PruneOutcome::Rejected { message },
                    });
                }
            }
        }

        Ok(pruned)
    }

    /// Removes pruned tasks from the resolved pipeline so they never become
    /// graph nodes. Returns the set of removed ids for validation tolerance.
    fn apply_prunes(&mut self, pruned: &[PrunedTask]) -> HashSet<TaskId> {
        let mut removed = HashSet::new();
        for entry in pruned {
            let task_id = &entry.task_id;
            self.tasks_by_id.remove(task_id);
            if let Some(task_names) = self.task_names_by_package.get_mut(&task_id.package) {
                // Only drop the task name for this package when no other task of
                // the same name survives in the package (each package has at most
                // one task per name, so this simply removes it).
                task_names.remove(&task_id.task);
                if task_names.is_empty() {
                    self.task_names_by_package.remove(&task_id.package);
                }
            }
            if is_root_task(task_id) {
                self.root_task_names.remove(&task_id.task);
            }
            removed.insert(task_id.clone());
        }
        removed
    }

    /// Injects worker-defined dependencies into tasks that use those workers.
    ///
    /// For each task with a configured worker, if that worker is defined and has
    /// non-empty `depends_on`, those dependencies are appended to the task's
    /// dependency list (deduped). This runs after resolution so worker `Modify`
    /// decisions cannot override injected deps, but before prune application.
    fn inject_worker_dependencies(
        &mut self,
        worker_definitions: &HashMap<String, WorkerDefinition>,
    ) {
        for definition in self.tasks_by_id.values_mut() {
            let Some(worker_name) = &definition.worker else {
                continue;
            };
            let Some(worker_def) = worker_definitions.get(worker_name) else {
                continue;
            };
            if worker_def.depends_on.is_empty() {
                continue;
            }
            // Dedupe against both the task's existing deps AND any duplicates
            // within the worker's own depends_on list (insert as we go).
            let mut existing: HashSet<DependsOn> = definition.depends_on.iter().cloned().collect();
            for dep in &worker_def.depends_on {
                if existing.insert(dep.clone()) {
                    definition.depends_on.push(dep.clone());
                }
            }
        }
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

/// The lookups a dead-dependency check needs, bundled to keep the per-arm
/// helpers small and the public entry point at four arguments. Package names
/// are precomputed once so per-dependency checks avoid repeated graph walks.
struct DependencyContext<'a> {
    /// Names of every package in the workspace (precomputed from the graph).
    package_names: HashSet<PackageName>,
    resolved_pipeline: &'a ResolvedPipeline,
    /// Tasks intentionally pruned during resolution. A reference resolving to a
    /// pruned task is tolerated (dropped edge), not a dead-dependency error.
    pruned: &'a HashSet<TaskId>,
}

impl<'a> DependencyContext<'a> {
    fn new(
        package_graph: &PackageGraph,
        resolved_pipeline: &'a ResolvedPipeline,
        pruned: &'a HashSet<TaskId>,
    ) -> Self {
        // A failed topological order (cycle) leaves the set empty; the same
        // missing-package handling as before then applies.
        let package_names = package_graph
            .topological_order()
            .map(|packages| packages.into_iter().map(|node| node.name.clone()).collect())
            .unwrap_or_default();
        Self {
            package_names,
            resolved_pipeline,
            pruned,
        }
    }

    fn pruned_has_id(&self, task_id: &TaskId) -> bool {
        self.pruned.contains(task_id)
    }

    fn pruned_has_package_task(&self, task_name: &TaskName) -> bool {
        self.pruned
            .iter()
            .any(|task_id| &task_id.task == task_name && !is_root_task(task_id))
    }

    fn pruned_has_root_task(&self, task_name: &TaskName) -> bool {
        self.pruned
            .iter()
            .any(|task_id| &task_id.task == task_name && is_root_task(task_id))
    }

    fn package_exists(&self, package: &PackageName) -> bool {
        self.package_names.contains(package)
    }

    /// `name` dependency from a source task, resolved within its own scope.
    fn same_package_reason(
        &self,
        source_task_id: &TaskId,
        task_name: &TaskName,
    ) -> Option<DeadDependencyReason> {
        let (exists, pruned_match) = if is_root_task(source_task_id) {
            (
                self.resolved_pipeline.contains_root_task(task_name),
                self.pruned_has_root_task(task_name),
            )
        } else {
            let candidate = TaskId::new(source_task_id.package.clone(), task_name.clone());
            (
                self.resolved_pipeline.task_exists_in_any_package(task_name),
                self.pruned_has_id(&candidate) || self.pruned_has_package_task(task_name),
            )
        };
        (!exists && !pruned_match).then(|| DeadDependencyReason::UnknownTaskEverywhere {
            task: task_name.clone(),
        })
    }

    fn specific_reason(&self, task_id: &TaskId) -> Option<DeadDependencyReason> {
        if !self.package_exists(&task_id.package) {
            return Some(DeadDependencyReason::UnknownPackage {
                package: task_id.package.clone(),
            });
        }
        let exists = self
            .resolved_pipeline
            .contains_package_task(&task_id.package, &task_id.task);
        (!exists && !self.pruned_has_id(task_id)).then(|| {
            DeadDependencyReason::UnknownTaskInPackage {
                package: task_id.package.clone(),
                task: task_id.task.clone(),
            }
        })
    }

    fn upstream_reason(&self, task_name: &TaskName) -> Option<DeadDependencyReason> {
        let exists = self.resolved_pipeline.task_exists_in_any_package(task_name);
        (!exists && !self.pruned_has_package_task(task_name)).then(|| {
            DeadDependencyReason::UnknownTaskEverywhere {
                task: task_name.clone(),
            }
        })
    }

    fn root_reason(&self, task_name: &TaskName) -> Option<DeadDependencyReason> {
        let exists = self.resolved_pipeline.contains_root_task(task_name);
        (!exists && !self.pruned_has_root_task(task_name)).then(|| {
            DeadDependencyReason::UnknownRootTask {
                task: task_name.clone(),
            }
        })
    }

    /// Returns why `source_task_id`'s `dependency` is dead, or `None` when it
    /// resolves (or was intentionally pruned and is therefore tolerated).
    fn dead_dependency_reason(
        &self,
        source_task_id: &TaskId,
        dependency: &DependsOn,
    ) -> Option<DeadDependencyReason> {
        match dependency {
            DependsOn::SamePackage(task_name) => {
                self.same_package_reason(source_task_id, task_name)
            }
            DependsOn::Specific(task_id) => self.specific_reason(task_id),
            DependsOn::DirectUpstream(task_name) | DependsOn::TransitiveUpstream(task_name) => {
                self.upstream_reason(task_name)
            }
            DependsOn::Root(task_name) => self.root_reason(task_name),
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
    task_id.is_root()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        fs,
        path::Path,
    };

    use tempfile::tempdir;

    use luchta_types::{
        DependsOn, PackageName, TaskDefinition, TaskId, TaskName, WorkerDefinition,
    };
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

        TaskGraph::validate_tasks(&package_graph, &pipeline, &HashMap::new())
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
        TaskGraph::validate_tasks(&package_graph, &pipeline, &HashMap::new())
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

        let error = TaskGraph::validate_tasks(&package_graph, &pipeline, &HashMap::new())
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

        let error = TaskGraph::validate_tasks(&package_graph, &pipeline, &HashMap::new())
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

        let error = TaskGraph::validate_tasks(&package_graph, &pipeline, &HashMap::new())
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

        let error = TaskGraph::validate_tasks(&package_graph, &pipeline, &HashMap::new())
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

        let error = TaskGraph::validate_tasks(&package_graph, &pipeline, &HashMap::new())
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

        let error = TaskGraph::validate_tasks(&package_graph, &pipeline, &HashMap::new())
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

        TaskGraph::validate_tasks(&package_graph, &pipeline, &HashMap::new())
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

        TaskGraph::validate_tasks(&package_graph, &pipeline, &HashMap::new())
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

        TaskGraph::validate_tasks(&package_graph, &pipeline, &HashMap::new())
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

        let error = TaskGraph::validate_tasks(&package_graph, &pipeline, &HashMap::new())
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
        let error = TaskGraph::validate_tasks(&package_graph, &pipeline, &HashMap::new())
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
        let workers = HashMap::from([(
            "missing".to_string(),
            WorkerDefinition {
                command: "echo".to_string(),
                depends_on: vec![],
            },
        )]);
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

    #[test]
    fn repro_global_task_specific_cross_package_dep() {
        // Mirrors the user's config: a GLOBAL `build:node` task that depends on a
        // package-scoped `@formative/babel-cli#build`, plus an explicit
        // package-scoped key for that babel build.
        let temp_dir = tempdir().expect("create temp dir");
        write_package(
            temp_dir.path().join("packages/babel-cli/package.json"),
            PackageManifest {
                name: "@formative/babel-cli",
                dependencies: &[],
            },
        );
        write_package(
            temp_dir.path().join("packages/other/package.json"),
            PackageManifest {
                name: "@formative/other",
                dependencies: &[],
            },
        );
        let package_graph = PackageGraph::build(vec![
            package_node(
                temp_dir.path().join("packages/babel-cli"),
                "@formative/babel-cli",
            ),
            package_node(temp_dir.path().join("packages/other"), "@formative/other"),
        ])
        .expect("build package graph");

        let pipeline = HashMap::from([
            (
                TaskName::from("build:node"),
                TaskDefinition {
                    depends_on: vec![DependsOn::Specific(TaskId::new(
                        "@formative/babel-cli",
                        "build",
                    ))],
                    worker: Some("yarn".to_string()),
                    ..TaskDefinition::default()
                },
            ),
            (
                TaskName::from("@formative/babel-cli#build"),
                TaskDefinition {
                    worker: Some("yarn".to_string()),
                    ..TaskDefinition::default()
                },
            ),
        ]);

        let task_graph = TaskGraph::build(&package_graph, &pipeline).expect("build task graph");

        let other_build_node = TaskId::new("@formative/other", "build:node");
        let babel_build = TaskId::new("@formative/babel-cli", "build");

        // The babel build node must exist.
        assert!(
            task_graph.task_node(&babel_build).is_some(),
            "expected @formative/babel-cli#build node to exist"
        );
        // @formative/other#build:node must depend on @formative/babel-cli#build.
        let deps: Vec<_> = task_graph
            .dependencies_of(&other_build_node)
            .into_iter()
            .map(|node| node.id.clone())
            .collect();
        assert!(
            deps.contains(&babel_build),
            "expected {other_build_node} to depend on {babel_build}, got {deps:?}"
        );
    }

    #[test]
    fn global_tasks_skip_named_root_package_expansion() {
        let temp_dir = tempdir().expect("create temp dir");
        write_package(
            temp_dir.path().join("package.json"),
            PackageManifest {
                name: "repo",
                dependencies: &[],
            },
        );
        write_package(
            temp_dir.path().join("packages/app/package.json"),
            PackageManifest {
                name: "app",
                dependencies: &[],
            },
        );

        let package_graph = PackageGraph::build(vec![
            package_node(temp_dir.path(), "repo"),
            package_node(temp_dir.path().join("packages/app"), "app"),
        ])
        .expect("build package graph")
        .with_root_package(PackageName::from("repo"));

        let pipeline = HashMap::from([(
            TaskName::from("build"),
            TaskDefinition {
                worker: Some("yarn".to_string()),
                ..TaskDefinition::default()
            },
        )]);

        let task_graph = TaskGraph::build(&package_graph, &pipeline).expect("build task graph");

        assert!(
            task_graph
                .task_node(&TaskId::new("repo", "build"))
                .is_none(),
            "expected bare global task to skip named root package"
        );
        assert!(
            task_graph.task_node(&TaskId::new("app", "build")).is_some(),
            "expected bare global task to expand to child package"
        );
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

    // ---- Resolution-phase tests (Task 3 + Task 4) -------------------------

    use super::{PackageResolveInfo, PruneOutcome, PrunedTask, ResolveError, TaskResolver};
    use crate::worker::protocol::{ResolveMode, ResolveResult, ResolveTask, TaskModification};

    /// Stub resolver driven by a closure mapping a `ResolveTask` to a result.
    struct StubResolver<F>(F);

    impl<F> TaskResolver for StubResolver<F>
    where
        F: Fn(&ResolveTask) -> ResolveResult + Sync,
    {
        async fn resolve(
            &self,
            _worker: &str,
            request: ResolveTask,
        ) -> Result<ResolveResult, String> {
            Ok((self.0)(&request))
        }
    }

    /// Two independent packages `@repo/a` and `@repo/b`, no deps.
    fn package_graph_pair() -> PackageGraph {
        let temp_dir = tempdir().expect("create temp dir");
        write_package(
            temp_dir.path().join("packages/a/package.json"),
            PackageManifest {
                name: "@repo/a",
                dependencies: &[],
            },
        );
        write_package(
            temp_dir.path().join("packages/b/package.json"),
            PackageManifest {
                name: "@repo/b",
                dependencies: &[],
            },
        );
        PackageGraph::build(vec![
            package_node(temp_dir.path().join("packages/a"), "@repo/a"),
            package_node(temp_dir.path().join("packages/b"), "@repo/b"),
        ])
        .expect("build package graph")
    }

    fn worker_task(depends_on: Vec<DependsOn>) -> TaskDefinition {
        TaskDefinition {
            depends_on,
            worker: Some("yarn".to_string()),
            ..TaskDefinition::default()
        }
    }

    fn empty_resolve_info() -> HashMap<PackageName, PackageResolveInfo> {
        HashMap::new()
    }

    /// Asserts that the graph contains a node for the given package/task.
    fn assert_in_graph(graph: &TaskGraph, package: &str, task: &str) {
        assert!(
            graph.task_node(&TaskId::new(package, task)).is_some(),
            "expected {package}#{task} to be a graph node"
        );
    }

    /// Asserts that the graph does NOT contain a node for the given package/task.
    fn assert_not_in_graph(graph: &TaskGraph, package: &str, task: &str) {
        assert!(
            graph.task_node(&TaskId::new(package, task)).is_none(),
            "expected {package}#{task} to be absent (pruned)"
        );
    }

    /// Asserts that `pruned` contains exactly the given task id with a
    /// `Pruned` outcome.
    fn assert_single_prune(pruned: &[PrunedTask], package: &str, task: &str) {
        assert_eq!(pruned.len(), 1, "expected exactly one pruned task");
        assert_eq!(pruned[0].task_id, TaskId::new(package, task));
        assert!(matches!(pruned[0].outcome, PruneOutcome::Pruned { .. }));
    }

    /// Asserts the resolved task `@repo/a#build` in `graph` has the expected
    /// command, dependencies, and weight (mirrored on its graph node).
    fn assert_app_build_spec(
        graph: &TaskGraph,
        command: &str,
        depends_on: Vec<DependsOn>,
        weight: u32,
    ) {
        let task_id = TaskId::new("@repo/a", "build");
        let definition = graph.task_definition(&task_id).expect("task kept");
        assert_eq!(definition.command.as_deref(), Some(command));
        assert_eq!(definition.depends_on, depends_on);
        assert_eq!(definition.weight, weight);
        assert_eq!(graph.task_node(&task_id).expect("node").weight, weight);
    }

    #[tokio::test]
    async fn resolution_prunes_tasks_the_worker_drops() {
        let package_graph = package_graph_pair();
        let pipeline = HashMap::from([(TaskName::from("build"), worker_task(vec![]))]);

        // Worker accepts @repo/a#build, prunes @repo/b#build.
        let resolver = StubResolver(|request: &ResolveTask| {
            if request.package == "@repo/b" {
                ResolveResult::prune(Some("script `build` not found in package `@repo/b`".into()))
            } else {
                ResolveResult::accept()
            }
        });

        let (graph, pruned) = TaskGraph::build_resolved(
            &package_graph,
            &pipeline,
            &empty_resolve_info(),
            &HashMap::new(),
            &resolver,
            ResolveMode::Run,
        )
        .await
        .expect("build resolved graph");

        assert_in_graph(&graph, "@repo/a", "build");
        assert_not_in_graph(&graph, "@repo/b", "build");
        assert_single_prune(&pruned, "@repo/b", "build");
    }

    #[tokio::test]
    async fn resolution_modify_updates_command_depends_on_and_weight() {
        let package_graph = package_graph_single("@repo/a");
        // Start with weight 1 and a same-package dependency.
        let pipeline = HashMap::from([(
            TaskName::from("build"),
            TaskDefinition {
                weight: 1,
                ..worker_task(vec![DependsOn::SamePackage(TaskName::from("prepare"))])
            },
        )]);

        // Worker replaces command, dependency list, and weight.
        let resolver = StubResolver(|_: &ResolveTask| {
            ResolveResult::modify(TaskModification {
                command: Some("compile".to_owned()),
                depends_on: Some(vec![DependsOn::DirectUpstream(TaskName::from("build"))]),
                weight: Some(7),
            })
        });
        let (graph, pruned) = TaskGraph::build_resolved(
            &package_graph,
            &pipeline,
            &empty_resolve_info(),
            &HashMap::new(),
            &resolver,
            ResolveMode::Run,
        )
        .await
        .expect("build resolved graph");

        assert!(pruned.is_empty());
        assert_app_build_spec(
            &graph,
            "compile",
            vec![DependsOn::DirectUpstream(TaskName::from("build"))],
            7,
        );
    }

    #[tokio::test]
    async fn resolution_modify_command_only_leaves_other_fields_intact() {
        let package_graph = package_graph_single("@repo/a");
        let pipeline = HashMap::from([(
            TaskName::from("build"),
            TaskDefinition {
                weight: 3,
                ..worker_task(vec![DependsOn::SamePackage(TaskName::from("prepare"))])
            },
        )]);

        let resolver = StubResolver(|_: &ResolveTask| ResolveResult::modify_command("compile"));
        let (graph, _pruned) = TaskGraph::build_resolved(
            &package_graph,
            &pipeline,
            &empty_resolve_info(),
            &HashMap::new(),
            &resolver,
            ResolveMode::Run,
        )
        .await
        .expect("build resolved graph");

        // Only the command changed; depends_on and weight are untouched.
        assert_app_build_spec(
            &graph,
            "compile",
            vec![DependsOn::SamePackage(TaskName::from("prepare"))],
            3,
        );
    }

    #[tokio::test]
    async fn reject_warns_and_prunes_in_run_mode_but_errors_in_check_mode() {
        let package_graph = package_graph_single("@repo/a");
        let pipeline = HashMap::from([(TaskName::from("build"), worker_task(vec![]))]);
        let resolver = StubResolver(|_: &ResolveTask| ResolveResult::reject("nope"));

        // Run mode: downgraded to a prune.
        let (graph, pruned) = TaskGraph::build_resolved(
            &package_graph,
            &pipeline,
            &empty_resolve_info(),
            &HashMap::new(),
            &resolver,
            ResolveMode::Run,
        )
        .await
        .expect("run-mode resolve succeeds");
        assert_eq!(graph.node_count(), 0);
        assert_eq!(pruned.len(), 1);
        assert!(pruned[0].outcome.is_rejected());

        // Check mode: a Reject is a hard error.
        let error = TaskGraph::build_resolved(
            &package_graph,
            &pipeline,
            &empty_resolve_info(),
            &HashMap::new(),
            &resolver,
            ResolveMode::Check,
        )
        .await
        .expect_err("check-mode reject errors");
        assert!(matches!(
            error,
            EngineError::Resolve(ResolveError::Rejected { .. })
        ));
    }

    #[tokio::test]
    async fn dependent_still_runs_when_its_dependency_is_pruned() {
        // @repo/b#build depends on a specific @repo/a#build; @repo/a#build is pruned.
        let package_graph = package_graph_pair();
        let pipeline = HashMap::from([(
            TaskName::from("build"),
            worker_task(vec![DependsOn::Specific(TaskId::new("@repo/a", "build"))]),
        )]);
        let resolver = StubResolver(|request: &ResolveTask| {
            if request.package == "@repo/a" {
                ResolveResult::prune(None)
            } else {
                ResolveResult::accept()
            }
        });
        let workers = HashMap::from([(
            "yarn".to_string(),
            WorkerDefinition {
                command: "yarn".to_string(),
                depends_on: vec![],
            },
        )]);

        let (graph, pruned) = TaskGraph::build_resolved(
            &package_graph,
            &pipeline,
            &empty_resolve_info(),
            &workers,
            &resolver,
            ResolveMode::Run,
        )
        .await
        .expect("build resolved graph");

        // Dependent survives; the dropped edge does not abort the run.
        assert_in_graph(&graph, "@repo/b", "build");
        assert_not_in_graph(&graph, "@repo/a", "build");

        // Validation tolerates the reference to the pruned dependency.
        let pruned_ids: HashSet<TaskId> = pruned.iter().map(|e| e.task_id.clone()).collect();
        TaskGraph::validate_tasks_with_pruned(&package_graph, &pipeline, &workers, &pruned_ids)
            .expect("dependency on a pruned task is tolerated");
    }

    #[test]
    fn dependency_on_a_genuinely_unknown_task_still_errors() {
        // No resolution / no prunes: a dependency on a never-declared specific
        // task must still surface as a dead dependency.
        let package_graph = package_graph_pair();
        let pipeline = HashMap::from([(
            TaskName::from("build"),
            worker_task(vec![DependsOn::Specific(TaskId::new("@repo/a", "ghost"))]),
        )]);
        let workers = HashMap::from([(
            "yarn".to_string(),
            WorkerDefinition {
                command: "yarn".to_string(),
                depends_on: vec![],
            },
        )]);

        let error =
            TaskGraph::validate_tasks(&package_graph, &pipeline, &workers).expect_err("dead");
        let DependencyValidationError::InvalidTasks { diagnostics } = error;
        assert!(diagnostics.iter().any(|diagnostic| matches!(
            &diagnostic.reason,
            TaskValidationReason::DeadDependencyReference {
                reason: DeadDependencyReason::UnknownTaskInPackage { task, .. },
                ..
            } if task.as_str() == "ghost"
        )));
    }

    #[test]
    fn worker_depends_on_unknown_task_fails_validation() {
        let package_graph = package_graph_single("@repo/app");
        let pipeline = HashMap::from([(TaskName::from("build"), worker_task(vec![]))]);
        let workers = HashMap::from([(
            "yarn".to_string(),
            WorkerDefinition {
                command: "yarn".to_string(),
                depends_on: vec![DependsOn::Root(TaskName::from("missing"))],
            },
        )]);

        let error = TaskGraph::validate_tasks(&package_graph, &pipeline, &workers)
            .expect_err("worker-injected dead dependency should fail validation");
        let DependencyValidationError::InvalidTasks { diagnostics } = error;
        assert!(diagnostics.iter().any(|diagnostic| matches!(
            &diagnostic.reason,
            TaskValidationReason::DeadDependencyReference {
                dependency,
                reason: DeadDependencyReason::UnknownRootTask { task },
            } if dependency == &DependsOn::Root(TaskName::from("missing")) && task.as_str() == "missing"
        )));
    }

    #[test]
    fn worker_depends_on_pruned_task_is_tolerated_during_validation() {
        let package_graph = package_graph_pair();
        let pipeline = HashMap::from([(TaskName::from("build"), worker_task(vec![]))]);
        let workers = HashMap::from([(
            "yarn".to_string(),
            WorkerDefinition {
                command: "yarn".to_string(),
                depends_on: vec![DependsOn::Specific(TaskId::new("@repo/a", "build"))],
            },
        )]);
        let pruned_ids = HashSet::from([TaskId::new("@repo/a", "build")]);

        TaskGraph::validate_tasks_with_pruned(&package_graph, &pipeline, &workers, &pruned_ids)
            .expect("worker-injected dependency on pruned task should be tolerated");
    }

    // ==========================================================================
    // Worker dependency injection tests
    // ==========================================================================

    #[tokio::test]
    async fn worker_dep_injection_no_worker_or_empty_deps_no_change() {
        // (a) bare-string worker config (no deps) / worker with empty deps / task with no worker
        // => task unchanged (no injected edges).
        let package_graph = package_graph_single("@repo/a");

        // Task with worker that has no depends_on
        let pipeline = HashMap::from([(TaskName::from("build"), worker_task(vec![]))]);

        let resolver = StubResolver(|_: &ResolveTask| ResolveResult::accept());

        // Worker definition with empty depends_on
        let workers: HashMap<String, WorkerDefinition> = HashMap::from([(
            "yarn".to_string(),
            WorkerDefinition {
                command: "yarn".to_string(),
                depends_on: vec![],
            },
        )]);

        let (graph, pruned) = TaskGraph::build_resolved(
            &package_graph,
            &pipeline,
            &empty_resolve_info(),
            &workers,
            &resolver,
            ResolveMode::Run,
        )
        .await
        .expect("build resolved graph");

        assert!(pruned.is_empty());
        // Task has no dependencies
        let task_id = TaskId::new("@repo/a", "build");
        let definition = graph.task_definition(&task_id).expect("task exists");
        assert!(definition.depends_on.is_empty());
    }

    #[tokio::test]
    async fn worker_dep_injection_injects_to_all_tasks_using_worker() {
        // (b) object worker config with dependsOn => deps injected into EVERY task using that worker
        let package_graph = package_graph_pair();

        let pipeline = HashMap::from([(TaskName::from("build"), worker_task(vec![]))]);

        let resolver = StubResolver(|_: &ResolveTask| ResolveResult::accept());

        // Worker with depends_on = [DirectUpstream("prepare")]
        let injected_dep = DependsOn::DirectUpstream(TaskName::from("prepare"));
        let workers: HashMap<String, WorkerDefinition> = HashMap::from([(
            "yarn".to_string(),
            WorkerDefinition {
                command: "yarn".to_string(),
                depends_on: vec![injected_dep.clone()],
            },
        )]);

        let (graph, pruned) = TaskGraph::build_resolved(
            &package_graph,
            &pipeline,
            &empty_resolve_info(),
            &workers,
            &resolver,
            ResolveMode::Run,
        )
        .await
        .expect("build resolved graph");

        assert!(pruned.is_empty());

        // Both @repo/a#build and @repo/b#build should have the injected dep
        for pkg in ["@repo/a", "@repo/b"] {
            let task_id = TaskId::new(pkg, "build");
            let definition = graph.task_definition(&task_id).expect("task exists");
            assert!(
                definition.depends_on.contains(&injected_dep),
                "task {pkg}#build should have injected dep"
            );
        }
    }

    #[tokio::test]
    async fn worker_dep_injection_survives_worker_modify() {
        // (c) injected worker deps SURVIVE a worker `Modify(depends_on=[...])`
        // (because inject runs after resolve)
        let package_graph = package_graph_single("@repo/a");

        let pipeline = HashMap::from([(TaskName::from("build"), worker_task(vec![]))]);

        // Worker Modify changes depends_on to something else
        let resolver = StubResolver(|_: &ResolveTask| {
            ResolveResult::modify(TaskModification {
                command: None,
                depends_on: Some(vec![DependsOn::SamePackage(TaskName::from("other"))]),
                weight: None,
            })
        });

        // Worker definition has its own depends_on
        let injected_dep = DependsOn::DirectUpstream(TaskName::from("prepare"));
        let workers: HashMap<String, WorkerDefinition> = HashMap::from([(
            "yarn".to_string(),
            WorkerDefinition {
                command: "yarn".to_string(),
                depends_on: vec![injected_dep.clone()],
            },
        )]);

        let (graph, pruned) = TaskGraph::build_resolved(
            &package_graph,
            &pipeline,
            &empty_resolve_info(),
            &workers,
            &resolver,
            ResolveMode::Run,
        )
        .await
        .expect("build resolved graph");

        assert!(pruned.is_empty());

        let task_id = TaskId::new("@repo/a", "build");
        let definition = graph.task_definition(&task_id).expect("task exists");

        // The worker's Modify(depends_on) was applied first
        assert!(
            definition
                .depends_on
                .contains(&DependsOn::SamePackage(TaskName::from("other"))),
            "worker Modify depends_on should be present"
        );
        // And the injected dep is ALSO present (appended after)
        assert!(
            definition.depends_on.contains(&injected_dep),
            "injected worker dep should survive Modify"
        );
    }

    #[tokio::test]
    async fn worker_dep_injection_dedupes_with_existing_dep() {
        // (d) injected dep + task's OWN identical dep => deduped (single edge, no panic)
        let package_graph = package_graph_single("@repo/a");

        let injected_dep = DependsOn::DirectUpstream(TaskName::from("prepare"));
        // Task already has this dep
        let pipeline = HashMap::from([(
            TaskName::from("build"),
            worker_task(vec![injected_dep.clone()]),
        )]);

        let resolver = StubResolver(|_: &ResolveTask| ResolveResult::accept());

        // Worker tries to inject the same dep
        let workers: HashMap<String, WorkerDefinition> = HashMap::from([(
            "yarn".to_string(),
            WorkerDefinition {
                command: "yarn".to_string(),
                depends_on: vec![injected_dep.clone()],
            },
        )]);

        let (graph, pruned) = TaskGraph::build_resolved(
            &package_graph,
            &pipeline,
            &empty_resolve_info(),
            &workers,
            &resolver,
            ResolveMode::Run,
        )
        .await
        .expect("build resolved graph");

        assert!(pruned.is_empty());

        let task_id = TaskId::new("@repo/a", "build");
        let definition = graph.task_definition(&task_id).expect("task exists");

        // Count occurrences: should be exactly 1 (deduped)
        let count = definition
            .depends_on
            .iter()
            .filter(|d| *d == &injected_dep)
            .count();
        assert_eq!(count, 1, "dep should be deduped, not duplicated");
    }

    #[tokio::test]
    async fn worker_dep_injection_tolerates_pruned_or_missing_target() {
        // (e) injected dep pointing at a pruned or nonexistent task => build does NOT fail
        let package_graph = package_graph_single("@repo/a");

        let pipeline = HashMap::from([(TaskName::from("build"), worker_task(vec![]))]);

        // Prune the build task
        let resolver = StubResolver(|_: &ResolveTask| ResolveResult::prune(None));

        // Worker definition has depends_on pointing somewhere
        let workers: HashMap<String, WorkerDefinition> = HashMap::from([(
            "yarn".to_string(),
            WorkerDefinition {
                command: "yarn".to_string(),
                depends_on: vec![DependsOn::Specific(TaskId::new("@repo/a", "nonexistent"))],
            },
        )]);

        // Build should succeed without error
        let (graph, pruned) = TaskGraph::build_resolved(
            &package_graph,
            &pipeline,
            &empty_resolve_info(),
            &workers,
            &resolver,
            ResolveMode::Run,
        )
        .await
        .expect("build resolved graph");

        // Task was pruned, so graph is empty
        assert_eq!(graph.node_count(), 0);
        assert_eq!(pruned.len(), 1);
    }

    #[tokio::test]
    async fn worker_dep_injection_for_task_without_worker_unchanged() {
        // Task without a worker should not get deps injected
        let package_graph = package_graph_single("@repo/a");

        let pipeline = HashMap::from([(
            TaskName::from("build"),
            TaskDefinition {
                worker: None,
                ..TaskDefinition::default()
            },
        )]);

        let resolver = StubResolver(|_: &ResolveTask| ResolveResult::accept());

        let workers: HashMap<String, WorkerDefinition> = HashMap::from([(
            "yarn".to_string(),
            WorkerDefinition {
                command: "yarn".to_string(),
                depends_on: vec![DependsOn::DirectUpstream(TaskName::from("prepare"))],
            },
        )]);

        let (graph, pruned) = TaskGraph::build_resolved(
            &package_graph,
            &pipeline,
            &empty_resolve_info(),
            &workers,
            &resolver,
            ResolveMode::Run,
        )
        .await
        .expect("build resolved graph");

        assert!(pruned.is_empty());

        let task_id = TaskId::new("@repo/a", "build");
        let definition = graph.task_definition(&task_id).expect("task exists");
        // No worker, so no injection
        assert!(definition.depends_on.is_empty());
    }
}
