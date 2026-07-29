use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};

use petgraph::{
    graph::NodeIndex,
    visit::{EdgeRef, IntoNodeReferences},
    Direction,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use crate::task_graph::{TaskGraph, TaskNode};

pub type CompletionSignal = oneshot::Sender<bool>;
pub type ReadyTaskMessage = (TaskNode, CompletionSignal);

fn compute_downstream_weights(
    nodes: &HashMap<NodeIndex, TaskNode>,
    dependents: &HashMap<NodeIndex, Vec<NodeIndex>>,
    order: &[NodeIndex],
) -> HashMap<NodeIndex, u64> {
    let mut downstream_weights = HashMap::with_capacity(nodes.len());

    for node_index in order {
        let own_weight = u64::from(
            nodes
                .get(node_index)
                .expect("downstream weight node missing task payload")
                .weight,
        );
        let dependent_weight = dependents
            .get(node_index)
            .into_iter()
            .flatten()
            .map(|dependent| {
                downstream_weights
                    .get(dependent)
                    .copied()
                    .expect("dependent downstream weight not computed")
            })
            .sum::<u64>();
        downstream_weights.insert(*node_index, own_weight + dependent_weight);
    }

    downstream_weights
}

#[derive(Debug)]
pub struct Walker {
    join_handle: JoinHandle<()>,
}

impl Walker {
    /// Spawn async driver that emits ready tasks through returned receiver.
    ///
    /// Message contract: each emitted item is `(TaskNode, oneshot::Sender<bool>)`.
    /// Caller must send `true` when task succeeds or `false` when task fails.
    ///
    /// Readiness rule follows `TaskGraph` edge direction: `X -> Y` means
    /// `X depends on Y`, so node is ready when all of its out-neighbors have
    /// completed successfully.
    pub fn new(graph: &TaskGraph) -> (Self, mpsc::Receiver<ReadyTaskMessage>) {
        let state = Arc::new(WalkerState::from_graph(graph));
        let buffer_size = state.nodes.len().max(1);
        let (ready_sender, ready_receiver) = mpsc::channel(buffer_size);
        let join_handle = tokio::spawn({
            let state = Arc::clone(&state);
            async move {
                state.run(ready_sender).await;
            }
        });

        (Self { join_handle }, ready_receiver)
    }

    /// Wait for walker driver to finish after receiver channel closes.
    pub async fn wait(self) -> Result<(), tokio::task::JoinError> {
        self.join_handle.await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Skipped,
}

#[derive(Debug)]
struct WalkerState {
    nodes: HashMap<NodeIndex, TaskNode>,
    dependencies_remaining: std::sync::Mutex<HashMap<NodeIndex, usize>>,
    dependents: HashMap<NodeIndex, Vec<NodeIndex>>,
    downstream_weights: HashMap<NodeIndex, u64>,
    states: std::sync::Mutex<HashMap<NodeIndex, NodeState>>,
    terminal_count: std::sync::Mutex<usize>,
    total_count: usize,
}

impl WalkerState {
    fn from_graph(graph: &TaskGraph) -> Self {
        let mut nodes = HashMap::new();
        let mut dependencies_remaining = HashMap::new();
        let mut dependents: HashMap<NodeIndex, Vec<NodeIndex>> = HashMap::new();
        let mut states = HashMap::new();

        for (index, node) in graph.as_graph().node_references() {
            let dependency_count = graph
                .as_graph()
                .neighbors_directed(index, Direction::Outgoing)
                .count();
            nodes.insert(index, node.clone());
            dependencies_remaining.insert(index, dependency_count);
            dependents.entry(index).or_default();
            states.insert(index, NodeState::Pending);
        }

        for edge in graph.as_graph().edge_references() {
            dependents
                .entry(edge.target())
                .or_default()
                .push(edge.source());
        }

        let indices_by_id = nodes
            .iter()
            .map(|(index, node)| (node.id.clone(), *index))
            .collect::<HashMap<_, _>>();
        let mut order = graph
            .topological_order()
            .expect("walker graph must be acyclic")
            .into_iter()
            .map(|node| {
                indices_by_id
                    .get(&node.id)
                    .copied()
                    .expect("topological node missing walker index")
            })
            .collect::<Vec<_>>();
        order.reverse();
        // Diamond paths intentionally double-count shared work. This accepted approximation is a
        // dispatch hint, not an exact unlock metric, and is computed once because walks use an
        // immutable DAG.
        let downstream_weights = compute_downstream_weights(&nodes, &dependents, &order);
        let total_count = nodes.len();

        Self {
            nodes,
            dependencies_remaining: std::sync::Mutex::new(dependencies_remaining),
            dependents,
            downstream_weights,
            states: std::sync::Mutex::new(states),
            terminal_count: std::sync::Mutex::new(0),
            total_count,
        }
    }

    async fn run(self: Arc<Self>, ready_sender: mpsc::Sender<ReadyTaskMessage>) {
        let mut join_set = tokio::task::JoinSet::new();
        self.enqueue_ready_nodes(&mut join_set, &ready_sender).await;

        while self.terminal_count() < self.total_count {
            let Some(join_result) = join_set.join_next().await else {
                break;
            };

            let (node_index, outcome) = match join_result {
                Ok(result) => result,
                Err(_) => break,
            };

            match outcome {
                true => self.mark_succeeded(node_index),
                false => self.mark_failed(node_index),
            }

            self.enqueue_ready_nodes(&mut join_set, &ready_sender).await;
        }
    }

    async fn enqueue_ready_nodes(
        self: &Arc<Self>,
        join_set: &mut tokio::task::JoinSet<(NodeIndex, bool)>,
        ready_sender: &mpsc::Sender<ReadyTaskMessage>,
    ) {
        let ready_nodes = self.take_ready_nodes();

        for node_index in ready_nodes {
            self.set_running(node_index);
            let (completion_tx, completion_rx) = oneshot::channel();
            let task_node = self
                .nodes
                .get(&node_index)
                .expect("walker node missing task payload")
                .clone();

            if ready_sender.send((task_node, completion_tx)).await.is_err() {
                return;
            }

            join_set.spawn(async move {
                let outcome = completion_rx.await.unwrap_or(false);
                (node_index, outcome)
            });
        }
    }

    fn take_ready_nodes(&self) -> Vec<NodeIndex> {
        let states = self.states.lock().expect("walker states poisoned");
        let remaining = self
            .dependencies_remaining
            .lock()
            .expect("walker dependencies poisoned");

        let mut ready = self
            .nodes
            .keys()
            .copied()
            .filter(|node_index| {
                states.get(node_index) == Some(&NodeState::Pending)
                    && remaining.get(node_index).copied().unwrap_or_default() == 0
            })
            .collect::<Vec<_>>();

        // dispatch priority; does not affect topological readiness or semaphore acquisition.
        // Head-of-line blocking at the semaphore is a known limitation deferred to a follow-up.
        ready.sort_by(|left, right| {
            let left_node = self
                .nodes
                .get(left)
                .expect("ready node missing task payload");
            let right_node = self
                .nodes
                .get(right)
                .expect("ready node missing task payload");
            (
                Reverse(
                    self.downstream_weights
                        .get(left)
                        .copied()
                        .unwrap_or_default(),
                ),
                Reverse(left_node.weight),
                &left_node.id.package.0,
                &left_node.id.task.0,
            )
                .cmp(&(
                    Reverse(
                        self.downstream_weights
                            .get(right)
                            .copied()
                            .unwrap_or_default(),
                    ),
                    Reverse(right_node.weight),
                    &right_node.id.package.0,
                    &right_node.id.task.0,
                ))
        });

        ready
    }

    fn set_running(&self, node_index: NodeIndex) {
        let mut states = self.states.lock().expect("walker states poisoned");
        if states.get(&node_index) == Some(&NodeState::Pending) {
            states.insert(node_index, NodeState::Running);
        }
    }

    fn mark_succeeded(&self, node_index: NodeIndex) {
        let mut states = self.states.lock().expect("walker states poisoned");
        if states.insert(node_index, NodeState::Succeeded) != Some(NodeState::Running) {
            return;
        }
        drop(states);
        self.bump_terminal_count(1);

        if let Some(dependents) = self.dependents.get(&node_index) {
            let states = self.states.lock().expect("walker states poisoned");
            let mut remaining = self
                .dependencies_remaining
                .lock()
                .expect("walker dependencies poisoned");
            for dependent in dependents {
                if states.get(dependent) != Some(&NodeState::Pending) {
                    continue;
                }

                let dependency_count = remaining
                    .get_mut(dependent)
                    .expect("walker dependent missing dependency count");
                *dependency_count = dependency_count.saturating_sub(1);
            }
        }
    }

    fn mark_failed(&self, node_index: NodeIndex) {
        let mut states = self.states.lock().expect("walker states poisoned");
        if states.insert(node_index, NodeState::Failed) != Some(NodeState::Running) {
            return;
        }
        drop(states);
        self.bump_terminal_count(1);
        self.skip_dependents(node_index);
    }

    fn skip_dependents(&self, node_index: NodeIndex) {
        let mut queue = VecDeque::new();
        let mut visited = HashSet::new();
        let mut skipped = 0;

        self.enqueue_dependents(node_index, &mut queue);

        let mut states = self.states.lock().expect("walker states poisoned");
        while let Some(dependent) = queue.pop_front() {
            if !visited.insert(dependent) {
                continue;
            }

            if states.get(&dependent).copied() == Some(NodeState::Pending) {
                states.insert(dependent, NodeState::Skipped);
                skipped += 1;
                self.enqueue_dependents(dependent, &mut queue);
            }
        }
        drop(states);

        if skipped > 0 {
            self.bump_terminal_count(skipped);
        }
    }

    /// Pushes the direct dependents of `node_index` onto `queue`.
    fn enqueue_dependents(&self, node_index: NodeIndex, queue: &mut VecDeque<NodeIndex>) {
        if let Some(dependents) = self.dependents.get(&node_index) {
            queue.extend(dependents.iter().copied());
        }
    }

    fn terminal_count(&self) -> usize {
        *self
            .terminal_count
            .lock()
            .expect("walker terminal count poisoned")
    }

    fn bump_terminal_count(&self, count: usize) {
        let mut terminal_count = self
            .terminal_count
            .lock()
            .expect("walker terminal count poisoned");
        *terminal_count += count;
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, fs, path::Path, time::Duration};

    use luchta_types::{DependsOn, PackageName, TaskDefinition, TaskId, TaskName};
    use luchta_workspace::{PackageGraph, PackageNode};
    use petgraph::graph::NodeIndex;
    use tokio::time::timeout;

    use super::{compute_downstream_weights, Walker};
    use crate::task_graph::{TaskGraph, TaskNode};

    #[tokio::test]
    async fn emits_linear_chain_in_dependency_first_order() {
        let task_graph = chain_task_graph();

        let (walker, mut ready) = Walker::new(&task_graph);

        let (task, done) = recv_task(&mut ready).await;
        assert_eq!(task.id.to_string(), "@repo/c#build");
        done.send(true).expect("signal c success");

        let (task, done) = recv_task(&mut ready).await;
        assert_eq!(task.id.to_string(), "@repo/b#build");
        done.send(true).expect("signal b success");

        let (task, done) = recv_task(&mut ready).await;
        assert_eq!(task.id.to_string(), "@repo/a#build");
        done.send(true).expect("signal a success");

        assert!(timeout(Duration::from_millis(100), ready.recv())
            .await
            .expect("walker should close receiver")
            .is_none());

        walker.wait().await.expect("walker join");
    }

    #[tokio::test]
    async fn failure_skips_transitive_dependents() {
        let task_graph = chain_task_graph();

        let (walker, mut ready) = Walker::new(&task_graph);

        let (task, done) = recv_task(&mut ready).await;
        assert_eq!(task.id.to_string(), "@repo/c#build");
        done.send(false).expect("signal c failure");

        assert!(timeout(Duration::from_millis(100), ready.recv())
            .await
            .expect("walker should close receiver")
            .is_none());

        walker.wait().await.expect("walker join");
    }

    #[tokio::test]
    async fn emits_independent_leaves_initially() {
        let task_graph = diamond_task_graph();

        let (walker, mut ready) = Walker::new(&task_graph);

        let (first, first_done) = recv_task(&mut ready).await;
        let (second, second_done) = recv_task(&mut ready).await;
        let mut emitted = vec![first.id.to_string(), second.id.to_string()];
        emitted.sort();
        assert_eq!(
            emitted,
            vec!["@repo/b#build".to_string(), "@repo/c#build".to_string()]
        );

        first_done.send(true).expect("signal first success");
        second_done.send(true).expect("signal second success");

        let (task, done) = recv_task(&mut ready).await;
        assert_eq!(task.id.to_string(), "@repo/a#build");
        done.send(true).expect("signal root success");

        assert!(timeout(Duration::from_millis(100), ready.recv())
            .await
            .expect("walker should close receiver")
            .is_none());

        walker.wait().await.expect("walker join");
    }

    #[tokio::test]
    async fn emits_higher_downstream_weight_first() {
        let task_graph = build_task_graph_weighted(vec![
            ("@repo/medium", 4, vec![]),
            ("@repo/light", 1, vec![]),
            ("@repo/heavy", 9, vec![]),
        ]);

        assert_dispatch_order(
            &task_graph,
            &[
                "@repo/heavy#build",
                "@repo/medium#build",
                "@repo/light#build",
            ],
        )
        .await;
    }

    #[tokio::test]
    async fn emits_deterministic_order_for_equal_weight_tasks() {
        let task_graph = build_task_graph(vec![
            ("@repo/c", vec![]),
            ("@repo/a", vec![]),
            ("@repo/b", vec![]),
        ]);

        assert_dispatch_order(
            &task_graph,
            &["@repo/a#build", "@repo/b#build", "@repo/c#build"],
        )
        .await;
    }

    #[tokio::test]
    async fn prioritizes_task_that_unlocks_more_work() {
        let task_graph = build_task_graph_weighted(vec![
            ("@repo/heavy-dependent", 20, vec!["@repo/z-unlocker"]),
            ("@repo/z-unlocker", 1, vec![]),
            ("@repo/a-independent", 1, vec![]),
        ]);
        let (walker, mut ready) = Walker::new(&task_graph);

        let (first, first_done) = recv_task(&mut ready).await;
        assert_eq!(first.id.to_string(), "@repo/z-unlocker#build");
        let (second, second_done) = recv_task(&mut ready).await;
        assert_eq!(second.id.to_string(), "@repo/a-independent#build");
        first_done.send(true).expect("signal unlocker success");
        second_done.send(true).expect("signal independent success");

        let (dependent, dependent_done) = recv_task(&mut ready).await;
        assert_eq!(dependent.id.to_string(), "@repo/heavy-dependent#build");
        dependent_done.send(true).expect("signal dependent success");
        walker.wait().await.expect("walker join");
    }

    #[test]
    fn computes_downstream_weights_with_diamond_double_counting() {
        let root = NodeIndex::new(0);
        let left = NodeIndex::new(1);
        let right = NodeIndex::new(2);
        let dependency = NodeIndex::new(3);
        let nodes = [root, left, right, dependency]
            .into_iter()
            .map(|index| {
                (
                    index,
                    TaskNode {
                        id: TaskId::new("@repo/package", format!("task-{}", index.index())),
                        weight: 1,
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        let dependents = HashMap::from([
            (root, vec![]),
            (left, vec![root]),
            (right, vec![root]),
            (dependency, vec![left, right]),
        ]);

        let weights =
            compute_downstream_weights(&nodes, &dependents, &[root, left, right, dependency]);

        assert_eq!(weights.get(&root), Some(&1));
        assert_eq!(weights.get(&left), Some(&2));
        assert_eq!(weights.get(&right), Some(&2));
        assert_eq!(weights.get(&dependency), Some(&5));
    }

    async fn assert_dispatch_order(task_graph: &TaskGraph, expected: &[&str]) {
        let (walker, mut ready) = Walker::new(task_graph);

        for expected_id in expected {
            let (task, done) = recv_task(&mut ready).await;
            assert_eq!(task.id.to_string(), *expected_id);
            done.send(true).expect("signal task success");
        }

        walker.wait().await.expect("walker join");
    }

    async fn recv_task(
        ready: &mut tokio::sync::mpsc::Receiver<super::ReadyTaskMessage>,
    ) -> super::ReadyTaskMessage {
        timeout(Duration::from_secs(1), ready.recv())
            .await
            .expect("timed out waiting for ready task")
            .expect("walker closed before expected task")
    }

    fn chain_task_graph() -> TaskGraph {
        build_task_graph(vec![
            ("@repo/a", vec!["@repo/b"]),
            ("@repo/b", vec!["@repo/c"]),
            ("@repo/c", vec![]),
        ])
    }

    fn diamond_task_graph() -> TaskGraph {
        build_task_graph(vec![
            ("@repo/a", vec!["@repo/b", "@repo/c"]),
            ("@repo/b", vec![]),
            ("@repo/c", vec![]),
        ])
    }

    fn build_task_graph(packages: Vec<(&str, Vec<&str>)>) -> TaskGraph {
        build_task_graph_weighted(
            packages
                .into_iter()
                .map(|(name, dependencies)| (name, 1, dependencies))
                .collect(),
        )
    }

    fn build_task_graph_weighted(packages: Vec<(&str, u32, Vec<&str>)>) -> TaskGraph {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let mut package_nodes = Vec::new();
        let mut pipeline = HashMap::new();

        for (name, weight, dependencies) in packages {
            let package_dir = temp_dir
                .path()
                .join(name.trim_start_matches('@').replace('/', "_"));
            write_package(package_dir.join("package.json"), name, &dependencies);
            package_nodes.push(package_node(package_dir, name));
            pipeline.insert(
                TaskName::from(format!("{name}#build")),
                TaskDefinition {
                    depends_on: vec![DependsOn::DirectUpstream(TaskName::from("build"))],
                    weight,
                    ..TaskDefinition::default()
                },
            );
        }

        let package_graph = PackageGraph::build(package_nodes).expect("build package graph");
        TaskGraph::build(&package_graph, &pipeline).expect("build task graph")
    }

    fn package_node(path: impl AsRef<Path>, name: &str) -> PackageNode {
        PackageNode::new(PackageName::from(name), path.as_ref())
    }

    fn write_package(path: impl AsRef<Path>, name: &str, dependencies: &[&str]) {
        let dependencies_json = dependency_entries_json(dependencies);
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

    fn dependency_entries_json(entries: &[&str]) -> String {
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
