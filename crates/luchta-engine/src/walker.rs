use tokio::sync::{mpsc, oneshot};

use crate::task_graph::{TaskGraph, TaskNode};

pub type CompletionSignal = oneshot::Sender<bool>;
pub type ReadyTaskMessage = (TaskNode, CompletionSignal);

#[derive(Debug)]
pub struct Walker {
    graph: TaskGraph,
    ready_sender: mpsc::Sender<ReadyTaskMessage>,
    ready_receiver: mpsc::Receiver<ReadyTaskMessage>,
}

impl Walker {
    pub fn new(graph: TaskGraph, buffer_size: usize) -> Self {
        let channel_size = buffer_size.max(1);
        let (ready_sender, ready_receiver) = mpsc::channel(channel_size);

        Self {
            graph,
            ready_sender,
            ready_receiver,
        }
    }

    pub fn graph(&self) -> &TaskGraph {
        &self.graph
    }

    pub fn ready_sender(&self) -> mpsc::Sender<ReadyTaskMessage> {
        self.ready_sender.clone()
    }

    pub fn into_parts(
        self,
    ) -> (
        TaskGraph,
        mpsc::Sender<ReadyTaskMessage>,
        mpsc::Receiver<ReadyTaskMessage>,
    ) {
        (self.graph, self.ready_sender, self.ready_receiver)
    }
}
