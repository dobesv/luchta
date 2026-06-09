use luchta_types::TaskId;
use petgraph::graph::DiGraph;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskNode {
    pub id: TaskId,
    pub weight: u32,
}

#[derive(Debug, Default)]
pub struct TaskGraph {
    graph: DiGraph<TaskNode, ()>,
}

impl TaskGraph {
    pub fn new() -> Self {
        Self {
            graph: DiGraph::new(),
        }
    }

    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }
}
