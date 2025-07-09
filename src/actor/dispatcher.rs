use std::sync::{Arc, Mutex};
use std::collections::VecDeque;

use tokio::task::JoinSet;

use crate::actor::model::{Signal, Queue, Task};

pub struct Dispatcher {
    tx: tokio::sync::broadcast::Sender<Signal>,
    queue: Queue,

    handles: JoinSet<()>,
}

impl Dispatcher {
    pub fn new() -> Self {
        let (tx, _) = tokio::sync::broadcast::channel::<Signal>(100);
        let queue: Queue = Arc::new(Mutex::new(VecDeque::<Task>::new()));
        let handles = JoinSet::new();

        Self { tx, queue, handles }
    }

    pub fn queue(&self) -> Queue {
        self.queue.clone()
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Signal> {
        self.tx.subscribe()
    }

    pub async fn dispatch(&self, task: Task) {
        self.queue.lock().unwrap().push_back(task);
        let _ = self.tx.send(Signal::TaskAdded);
    }

    pub async fn stop(&mut self) {
        let _ = self.tx.send(Signal::Stop);
        while self.handles.join_next().await.is_some() {}
    }

    pub async fn listen(&mut self, workers: u8) {
        for _ in 0..workers {
            let mut rx = self.subscribe();
            let queue = self.queue.clone();

            self.handles.spawn(async move {
                tracing::info!("Worker started, waiting for signals...");

                while let Ok(signal) = rx.recv().await {
                    tracing::info!("Received signal");

                    match signal {
                        Signal::TaskAdded => {
                            tracing::info!("TaskAdded signal received, processing task...");
                            if let Some(task) = queue.lock().unwrap().pop_front() {
                                tracing::info!("Processing task with ID: {}", task.id);
                            }
                        }
                        Signal::Stop => {
                            tracing::info!("Stopping worker...");
                            break;
                        }
                    }
                }

                tracing::info!("Worker finished processing signals.");
            });
        }
    }
}