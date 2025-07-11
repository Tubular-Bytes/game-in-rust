use std::sync::{Arc, Mutex};
use std::collections::VecDeque;

use tokio::task::JoinSet;

use crate::actor::model::{Signal, Queue, Task, TaskRequest};

pub struct Dispatcher {
    websocket: tokio::sync::broadcast::Sender<TaskRequest>,
    actor_sender: tokio::sync::broadcast::Sender<Signal>,
    queue: Queue,

    handles: JoinSet<()>,
    task_handle: Option<tokio::task::JoinHandle<()>>,
}

impl Dispatcher {
    pub fn new(websocket: tokio::sync::broadcast::Sender<TaskRequest>) -> Self {
        let (actor_sender, _) = tokio::sync::broadcast::channel::<Signal>(100);
        let queue: Queue = Arc::new(Mutex::new(VecDeque::<Task>::new()));
        let handles = JoinSet::new();

        Self { websocket, actor_sender, queue, handles, task_handle: None }
    }

    pub fn queue(&self) -> Queue {
        self.queue.clone()
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Signal> {
        self.actor_sender.subscribe()
    }

    pub async fn dispatch(&self, task: Task) {
        self.queue.lock().unwrap().push_back(task);
        let _ = self.actor_sender.send(Signal::TaskAdded);
    }

    pub async fn stop(&mut self) {
        let _ = self.actor_sender.send(Signal::Stop);
        
        if let Some(task_handle) = self.task_handle.take() {
            tracing::info!("Waiting for task handle to finish...");
            let _ = tokio::join!(task_handle);
        }

        while self.handles.join_next().await.is_some() {}
    }

    pub async fn start(&mut self, workers: u8) {
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
                                tracing::info!("Processing {:?} task with ID: {}", task.kind, task.id);
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

        // // Move only the receiver and sender, not self, into the spawned task
        let mut websocket_receiver = self.websocket.subscribe();
        let queue = self.queue();
        let sender = self.actor_sender.clone();

        self.task_handle = Some(tokio::spawn(async move {
            loop {
                let msg = websocket_receiver.recv().await;
                match msg {
                    Ok(task_request) => {
                        tracing::info!("Received task request: {:?}", task_request);
                        let task = Task {
                            id: task_request.owner,
                            kind: task_request.kind,
                        };
                        queue.lock().unwrap().push_back(task);
                        let _ = sender.send(Signal::TaskAdded);
                    }
                    Err(e) => {
                        tracing::error!("Error receiving task request: {}", e);
                        break;
                    }
                }
            }

            tracing::info!("WebSocket listener stopped.");
        }));
    }
}