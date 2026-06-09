use std::{
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

        let total_count = nodes.len();

        Self {
            nodes,
            dependencies_remaining: std::sync::Mutex::new(dependencies_remaining),
            dependents,
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

        self.nodes
            .keys()
            .copied()
            .filter(|node_index| {
                states.get(node_index) == Some(&NodeState::Pending)
                    && remaining.get(node_index).copied().unwrap_or_default() == 0
            })
            .collect()
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

        if let Some(dependents) = self.dependents.get(&node_index) {
            queue.extend(dependents.iter().copied());
        }

        let mut states = self.states.lock().expect("walker states poisoned");
        while let Some(dependent) = queue.pop_front() {
            if !visited.insert(dependent) {
                continue;
            }

            if states.get(&dependent).copied() == Some(NodeState::Pending) {
                states.insert(dependent, NodeState::Skipped);
                skipped += 1;
                if let Some(next) = self.dependents.get(&dependent) {
                    queue.extend(next.iter().copied());
                }
            }
        }
        drop(states);

        if skipped > 0 {
            self.bump_terminal_count(skipped);
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

    use luchta_types::{DependsOn, PackageName, TaskDefinition, TaskName};
    use luchta_workspace::{PackageGraph, PackageNode};
    use tokio::time::timeout;

    use super::Walker;
    use crate::task_graph::TaskGraph;

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
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let mut package_nodes = Vec::new();

        for (name, dependencies) in packages {
            let package_dir = temp_dir
                .path()
                .join(name.trim_start_matches('@').replace('/', "_"));
            write_package(package_dir.join("package.json"), name, &dependencies);
            package_nodes.push(package_node(package_dir, name));
        }

        let package_graph = PackageGraph::build(package_nodes).expect("build package graph");
        let pipeline = HashMap::from([(
            TaskName::from("build"),
            TaskDefinition {
                depends_on: vec![DependsOn::DirectUpstream(TaskName::from("build"))],
                ..TaskDefinition::default()
            },
        )]);

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
