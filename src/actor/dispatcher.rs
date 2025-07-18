use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::task::JoinSet;
use uuid::Uuid;

use crate::actor::broker::{Broker, INVENTORY_TOPIC, WEBSOCKET_TOPIC};
use crate::actor::inventory::Inventory;
use crate::actor::model::{InternalMessage, Queue, Task};
use crate::actor::worker::spawn_worker;

const MAX_WAIT_TIME: u64 = 10; // seconds

pub struct Dispatcher {
    broker: Broker,
    queue: Queue,
    active_tasks: Arc<AtomicUsize>,
    inventories: Arc<Mutex<HashMap<Uuid, Inventory>>>,

    handles: JoinSet<()>,
    task_handle: Option<tokio::task::JoinHandle<()>>,
}

impl Dispatcher {
    pub fn new(broker: Broker) -> Self {
        let queue: Queue = Arc::new(Mutex::new(VecDeque::<Task>::new()));
        let handles = JoinSet::new();
        let active_tasks = Arc::new(AtomicUsize::new(0));
        let inventories = Arc::new(Mutex::new(HashMap::new()));

        Self {
            broker,
            queue,
            active_tasks,
            inventories,
            handles,
            task_handle: None,
        }
    }

    pub fn queue(&self) -> Queue {
        self.queue.clone()
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<InternalMessage> {
        self.topic().subscribe()
    }

    pub fn topic(&self) -> tokio::sync::broadcast::Sender<InternalMessage> {
        self.broker.topic(WEBSOCKET_TOPIC).sender.clone()
    }

    pub fn send(
        &self,
        msg: InternalMessage,
    ) -> Result<usize, tokio::sync::broadcast::error::SendError<InternalMessage>> {
        self.broker.topic(WEBSOCKET_TOPIC).publish(msg)
    }

    pub async fn stop(&mut self) {
        tracing::info!("Initiating graceful shutdown...");

        // First, signal graceful stop to prevent new tasks from being processed
        let _ = self.send(InternalMessage::GracefulStop);

        // Wait for all tasks (active and pending) to complete with timeout
        let start_time = tokio::time::Instant::now();

        loop {
            let active = self.active_task_count();
            let pending = self.pending_task_count();

            if !poll_tasks(start_time, active, pending) {
                break;
            }

            // If there are pending tasks but no active tasks, send a TaskAdded signal to wake up workers
            if pending > 0 && active == 0 {
                let _ = self.send(InternalMessage::TaskAdded);
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        tracing::info!("All tasks completed (or timed out), stopping workers...");

        // Then send stop signal to terminate workers
        let _ = self.send(InternalMessage::Stop);

        tracing::debug!("Sent stop signal to workers");

        // Stop the WebSocket task handle first
        if let Some(task_handle) = self.task_handle.take() {
            tracing::info!("Waiting for WebSocket task handle to finish...");

            // Add timeout for WebSocket task handle
            match tokio::time::timeout(tokio::time::Duration::from_secs(5), task_handle).await {
                Ok(_) => {
                    tracing::info!("WebSocket task handle finished gracefully");
                }
                Err(_) => {
                    tracing::warn!("WebSocket task handle timed out, continuing shutdown");
                }
            }
        }

        // Wait for worker handles with timeout
        let worker_timeout = tokio::time::Duration::from_secs(5);
        let worker_start = tokio::time::Instant::now();

        loop {
            if self.handles.is_empty() {
                tracing::info!("All worker handles completed");
                break;
            }

            if worker_start.elapsed() > worker_timeout {
                tracing::warn!("Timeout waiting for workers to stop. Aborting remaining workers.");
                self.handles.abort_all();
                break;
            }

            // Try to join next handle with a small timeout
            match tokio::time::timeout(
                tokio::time::Duration::from_millis(100),
                self.handles.join_next(),
            )
            .await
            {
                Ok(Some(result)) => match result {
                    Ok(_) => tracing::debug!("Worker stopped successfully"),
                    Err(e) => tracing::warn!("Worker stopped with error: {:?}", e),
                },
                Ok(None) => {
                    tracing::debug!("No more workers to join");
                    break;
                }
                Err(_) => {
                    // Timeout on join_next, continue loop
                }
            }
        }

        tracing::info!("All tasks completed and workers stopped.");
    }

    pub async fn start(&mut self, workers: u8) {
        for _ in 0..workers {
            let rx = self.subscribe();
            let queue = self.queue.clone();
            let active_tasks = self.active_tasks.clone();

            self.handles.spawn(spawn_worker(rx, queue, active_tasks));
        }

        // Move only the receiver and sender, not self, into the spawned task
        let broker = self.broker.clone();
        let mut websocket_receiver = self.topic().subscribe();
        let sender = self.topic().clone();
        let queue = self.queue();
        let inventories = self.inventories.clone();

        self.task_handle = Some(tokio::spawn(async move {
            loop {
                let msg = websocket_receiver.recv().await;
                match msg {
                    Ok(message) => match message {
                        InternalMessage::TaskRequest(task_request) => {
                            tracing::info!("Received task request: {:?}", task_request);
                            let task = Task {
                                id: task_request.owner,
                                request_id: task_request.request_id,
                                kind: task_request.kind,
                                respond_to: task_request.respond_to.clone(),
                            };
                            {
                                queue.lock().unwrap().push_back(task);
                            }
                            let _ = sender.send(InternalMessage::TaskAdded);
                        }
                        InternalMessage::AddInventory(id) => {
                            tracing::info!("Adding inventory with ID: {}", id);
                            if let Some(inventory) = inventories.lock().unwrap().get(&id) {
                                tracing::info!("Inventory already exists: {:?}", inventory);
                            } else {
                                let inventory = Inventory::new(
                                    id,
                                    broker.topic(INVENTORY_TOPIC).sender.clone(),
                                );
                                inventories.lock().unwrap().insert(id, inventory.clone());
                                tokio::spawn(async move { inventory.listen().await });
                                tracing::info!("New inventory added and listening: {}", id);
                            }
                        }
                        InternalMessage::RemoveInventory(id) => {
                            tracing::info!("Removing inventory with ID: {}", id);
                            let mut inventories = inventories.lock().unwrap();
                            if inventories.get(&id).is_none() {
                                tracing::warn!("Inventory with ID {} does not exist", id);
                            } else if let Some(inventory) = inventories.get(&id) {
                                inventory.stop();
                                tracing::info!("Inventory stopped: {}", id);
                                if inventories.remove(&id).is_some() {
                                    tracing::info!("Inventory removed: {}", id);
                                }
                            }
                        }
                        _ => tracing::debug!("skipping message: {:?}", message),
                    },
                    Err(e) => {
                        tracing::error!("Error receiving task request: {}", e);
                        break;
                    }
                }
            }

            tracing::info!("WebSocket listener stopped.");
        }));
    }

    pub async fn force_stop(&mut self) {
        tracing::warn!("Force stopping dispatcher...");

        // Send stop signal immediately
        let _ = self.send(InternalMessage::Stop);

        // Abort WebSocket task handle
        if let Some(task_handle) = self.task_handle.take() {
            task_handle.abort();
            tracing::info!("WebSocket task handle aborted");
        }

        // Abort all worker handles
        self.handles.abort_all();

        tracing::warn!("Force stop completed - all tasks and workers terminated immediately.");
    }

    pub fn active_task_count(&self) -> usize {
        self.active_tasks.load(Ordering::SeqCst)
    }

    pub fn pending_task_count(&self) -> usize {
        self.queue.lock().unwrap().len()
    }

    pub fn total_task_count(&self) -> usize {
        self.active_task_count() + self.pending_task_count()
    }
}

fn poll_tasks(start_time: tokio::time::Instant, active: usize, pending: usize) -> bool {
    if active == 0 && pending == 0 {
        return false;
    }

    // Check for timeout
    if start_time.elapsed() > tokio::time::Duration::from_secs(MAX_WAIT_TIME) {
        tracing::warn!(
            "Timeout waiting for tasks to complete. Active: {}, Pending: {}. Forcing shutdown.",
            active,
            pending
        );
        return false;
    }

    tracing::info!(
        "Waiting for tasks to complete... Active: {}, Pending: {} (elapsed: {:?})",
        active,
        pending,
        start_time.elapsed()
    );

    true
}
