use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::task::JoinSet;
use uuid::Uuid;

use crate::actor::broker::{Broker, INVENTORY_TOPIC, TASK_TOPIC};
use crate::actor::inventory::Inventory;
use crate::actor::model::{InternalMessage, Message, Queue, Task, WebsocketMessage};
use crate::actor::worker::spawn_worker;

const MAX_WAIT_TIME: u64 = 10; // seconds

pub struct Dispatcher {
    broker: Broker,
    queue: Queue,
    active_tasks: Arc<AtomicUsize>,
    inventories: Arc<Mutex<HashMap<Uuid, Inventory>>>,
    ws_receiver: tokio::sync::mpsc::Receiver<Message>,

    handles: JoinSet<()>,
    task_handle: Option<tokio::task::JoinHandle<()>>,
}

impl Dispatcher {
    pub fn new(broker: Broker, ws_receiver: tokio::sync::mpsc::Receiver<Message>) -> Self {
        let queue: Queue = Arc::new(Mutex::new(VecDeque::<Task>::new()));
        let handles = JoinSet::new();
        let active_tasks = Arc::new(AtomicUsize::new(0));
        let inventories = Arc::new(Mutex::new(HashMap::new()));

        Self {
            broker,
            queue,
            active_tasks,
            inventories,
            ws_receiver,
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
        self.broker.topic(TASK_TOPIC).sender.clone()
    }

    pub fn send(
        &self,
        msg: InternalMessage,
    ) -> Result<usize, tokio::sync::broadcast::error::SendError<InternalMessage>> {
        self.broker.topic(TASK_TOPIC).publish(msg)
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

            if !Self::poll_tasks(start_time, active, pending) {
                break;
            }

            // If there are pending tasks but no active tasks, send a TaskAdded signal to wake up workers
            if pending > 0 && active == 0 {
                let _ = self.send(InternalMessage::TaskAdded);
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        tracing::debug!("All tasks completed (or timed out), stopping workers...");

        // Then send stop signal to terminate workers
        let _ = self.send(InternalMessage::Stop);

        tracing::debug!("Sent stop signal to workers");

        // Stop the WebSocket task handle first
        if let Some(task_handle) = self.task_handle.take() {
            tracing::debug!("Waiting for WebSocket task handle to finish...");

            // Add timeout for WebSocket task handle
            match tokio::time::timeout(tokio::time::Duration::from_secs(5), task_handle).await {
                Ok(_) => {
                    tracing::debug!("WebSocket task handle finished gracefully");
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
                tracing::debug!("All worker handles completed");
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

        tracing::debug!("All tasks completed and workers stopped.");
    }

    pub async fn start(&mut self, workers: u8) {
        self.start_with_shutdown(workers, tokio::sync::oneshot::channel().1)
            .await;
    }

    pub async fn start_with_shutdown(
        &mut self,
        workers: u8,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) {
        for _ in 0..workers {
            let rx = self.subscribe();
            let queue = self.queue.clone();
            let active_tasks = self.active_tasks.clone();

            self.handles.spawn(spawn_worker(rx, queue, active_tasks));
        }

        // Move only the receiver and sender, not self, into the spawned task
        let broker = self.broker.clone();
        let task_tx = self.topic();
        let queue = self.queue();
        let inventories = self.inventories.clone();

        loop {
            tokio::select! {
                message = self.ws_receiver.recv() => {
                    match message {
                        Some(message) => {
                            match message.content {
                                WebsocketMessage::TaskRequest(task_request) => {
                                    Self::handle_task_request(task_tx.clone(), task_request, &queue, message.reply);
                                }

                                WebsocketMessage::AddInventory(id) => {
                                    Self::handle_add_inventory(id, &broker, &inventories, message.reply);
                                }
                                WebsocketMessage::RemoveInventory(id) => {
                                    Self::handle_remove_inventory(id, &inventories, message.reply);
                                }
                            }
                        }
                        None => {
                            tracing::debug!("WebSocket receiver channel closed, stopping dispatcher");
                            break;
                        }
                    }
                }
                _ = &mut shutdown_rx => {
                    tracing::debug!("Received shutdown signal, stopping dispatcher");
                    break;
                }
            }
        }

        // Perform graceful shutdown
        self.stop().await;
    }

    pub async fn force_stop(&mut self) {
        tracing::warn!("Force stopping dispatcher...");

        // Send stop signal immediately
        let _ = self.send(InternalMessage::Stop);

        // Abort WebSocket task handle
        if let Some(task_handle) = self.task_handle.take() {
            task_handle.abort();
            tracing::debug!("WebSocket task handle aborted");
        }

        // Abort all worker handles
        self.handles.abort_all();

        tracing::warn!("Force stop completed - all tasks and workers terminated immediately.");
    }

    pub fn active_task_count(&self) -> usize {
        self.active_tasks.load(Ordering::SeqCst)
    }

    pub fn pending_task_count(&self) -> usize {
        self.queue.lock().map(|queue| queue.len()).unwrap_or(0)
    }

    pub fn total_task_count(&self) -> usize {
        self.active_task_count() + self.pending_task_count()
    }

    fn handle_task_request(
        tx: tokio::sync::broadcast::Sender<InternalMessage>,
        task_request: crate::actor::model::TaskRequest,
        queue: &Queue,
        reply: Option<tokio::sync::oneshot::Sender<Result<String, String>>>,
    ) {
        tracing::debug!("Received task request: {:?}", task_request);
        let task = Task {
            id: task_request.owner,
            request_id: task_request.request_id,
            kind: task_request.kind,
            respond_to: task_request.respond_to.clone(),
        };

        match queue.lock() {
            Ok(mut queue) => {
                queue.push_back(task);

                tx.send(InternalMessage::TaskAdded)
                    .expect("Failed to send TaskRequest message");

                if let Some(reply_sender) = reply {
                    let _ = reply_sender.send(Ok("Task request received".to_string()));
                }
            }
            Err(e) => {
                tracing::error!("Failed to acquire queue lock: {}", e);
                if let Some(reply_sender) = reply {
                    let _ = reply_sender.send(Err("Internal error: queue unavailable".to_string()));
                }
            }
        }
    }

    fn handle_add_inventory(
        id: Uuid,
        broker: &Broker,
        inventories: &Arc<Mutex<HashMap<Uuid, Inventory>>>,
        reply: Option<tokio::sync::oneshot::Sender<Result<String, String>>>,
    ) {
        tracing::info!("Adding inventory with ID: {}", id);

        match inventories.lock() {
            Ok(mut inventories) => {
                if let Some(inventory) = inventories.get(&id) {
                    tracing::debug!("Inventory already exists: {:?}", inventory);
                    if let Some(reply_sender) = reply {
                        let _ = reply_sender.send(Ok("Inventory already exists".to_string()));
                    }
                } else {
                    let inventory =
                        Inventory::new(id, broker.topic(INVENTORY_TOPIC).sender.clone());
                    inventories.insert(id, inventory.clone());
                    tokio::spawn(async move { inventory.listen().await });
                    tracing::debug!("New inventory added and listening: {}", id);
                    if let Some(reply_sender) = reply {
                        let _ = reply_sender.send(Ok("Inventory added and listening".to_string()));
                    }
                }
            }
            Err(e) => {
                tracing::error!("Failed to acquire inventories lock: {}", e);
                if let Some(reply_sender) = reply {
                    let _ = reply_sender
                        .send(Err("Internal error: inventories unavailable".to_string()));
                }
            }
        }
    }

    fn handle_remove_inventory(
        id: Uuid,
        inventories: &Arc<Mutex<HashMap<Uuid, Inventory>>>,
        reply: Option<tokio::sync::oneshot::Sender<Result<String, String>>>,
    ) {
        tracing::debug!("Removing inventory with ID: {}", id);

        match inventories.lock() {
            Ok(mut inventories) => {
                // Fix: Check existence and remove in a single atomic operation
                if let Some(inventory) = inventories.remove(&id) {
                    inventory.stop();
                    tracing::debug!("Inventory stopped and removed: {}", id);
                    if let Some(reply_sender) = reply {
                        let _ = reply_sender.send(Ok(format!("Inventory {id} removed")));
                    }
                } else {
                    tracing::warn!("Inventory with ID {} does not exist", id);
                    if let Some(reply_sender) = reply {
                        let _ = reply_sender.send(Err(format!("Inventory {id} doesn't exist")));
                    }
                }
            }
            Err(e) => {
                tracing::error!("Failed to acquire inventories lock: {}", e);
                if let Some(reply_sender) = reply {
                    let _ = reply_sender
                        .send(Err("Internal error: inventories unavailable".to_string()));
                }
            }
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

        tracing::debug!(
            "Waiting for tasks to complete... Active: {}, Pending: {} (elapsed: {:?})",
            active,
            pending,
            start_time.elapsed()
        );

        true
    }
}
