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
                                    let task_tx = task_tx.clone();
                                    let queue = queue.clone();
                                    tokio::spawn(async move {
                                        Self::handle_task_request(task_tx, task_request, &queue, message.reply).await;
                                    });
                                }
                                WebsocketMessage::AddInventory(id) => {
                                    let broker = broker.clone();
                                    let inventories = inventories.clone();
                                    tokio::spawn(async move {
                                        Self::handle_add_inventory(id, &broker, &inventories, message.reply).await;
                                    });
                                }
                                WebsocketMessage::RemoveInventory(id) => {
                                    let inventories = inventories.clone();
                                    tokio::spawn(async move {
                                        Self::handle_remove_inventory(id, &inventories, message.reply).await;
                                    });
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

    async fn handle_task_request(
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

        // Queue operations are fast, no need for spawn_blocking
        match queue.lock() {
            Ok(mut queue) => {
                queue.push_back(task);
                let _ = tx.send(InternalMessage::TaskAdded);
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

    async fn handle_add_inventory(
        id: Uuid,
        broker: &Broker,
        inventories: &Arc<Mutex<HashMap<Uuid, Inventory>>>,
        reply: Option<tokio::sync::oneshot::Sender<Result<String, String>>>,
    ) {
        tracing::info!("Adding inventory with ID: {}", id);

        let inventories_clone = inventories.clone();
        let broker_clone = broker.clone();

        let result = tokio::task::spawn_blocking(move || {
            match inventories_clone.lock() {
                Ok(mut inventories) => {
                    if let std::collections::hash_map::Entry::Vacant(e) = inventories.entry(id) {
                        let inventory =
                            Inventory::new(id, broker_clone.topic(INVENTORY_TOPIC).sender.clone());
                        let inventory_clone = inventory.clone();
                        e.insert(inventory);
                        tracing::debug!("New inventory created: {}", id);
                        Ok((true, Some(inventory_clone))) // needs spawning
                    } else {
                        tracing::debug!("Inventory already exists: {}", id);
                        Ok((false, None)) // (needs_spawn, inventory)
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to acquire inventories lock: {}", e);
                    Err("Internal error: inventories unavailable".to_string())
                }
            }
        })
        .await;

        // Handle the result and spawn inventory listener if needed
        match result {
            Ok(Ok((needs_spawn, inventory_opt))) => {
                if needs_spawn {
                    if let Some(inventory) = inventory_opt {
                        tokio::spawn(async move { inventory.listen().await });
                        tracing::debug!("New inventory added and listening: {}", id);
                        if let Some(reply_sender) = reply {
                            let _ =
                                reply_sender.send(Ok("Inventory added and listening".to_string()));
                        }
                    }
                } else if let Some(reply_sender) = reply {
                    let _ = reply_sender.send(Ok("Inventory already exists".to_string()));
                }
            }
            Ok(Err(e)) => {
                if let Some(reply_sender) = reply {
                    let _ = reply_sender.send(Err(e));
                }
            }
            Err(e) => {
                tracing::error!("Add inventory handler panicked: {}", e);
                if let Some(reply_sender) = reply {
                    let _ = reply_sender.send(Err("Internal error: handler failed".to_string()));
                }
            }
        }
    }

    async fn handle_remove_inventory(
        id: Uuid,
        inventories: &Arc<Mutex<HashMap<Uuid, Inventory>>>,
        reply: Option<tokio::sync::oneshot::Sender<Result<String, String>>>,
    ) {
        tracing::debug!("Removing inventory with ID: {}", id);

        let inventories_clone = inventories.clone();

        let result = tokio::task::spawn_blocking(move || {
            match inventories_clone.lock() {
                Ok(mut inventories) => {
                    // Check existence and remove in a single atomic operation
                    if let Some(inventory) = inventories.remove(&id) {
                        inventory.stop();
                        tracing::debug!("Inventory stopped and removed: {}", id);
                        Ok(format!("Inventory {id} removed"))
                    } else {
                        tracing::warn!("Inventory with ID {} does not exist", id);
                        Err(format!("Inventory {id} doesn't exist"))
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to acquire inventories lock: {}", e);
                    Err("Internal error: inventories unavailable".to_string())
                }
            }
        })
        .await;

        match result {
            Ok(Ok(msg)) => {
                if let Some(reply_sender) = reply {
                    let _ = reply_sender.send(Ok(msg));
                }
            }
            Ok(Err(e)) => {
                if let Some(reply_sender) = reply {
                    let _ = reply_sender.send(Err(e));
                }
            }
            Err(e) => {
                tracing::error!("Remove inventory handler panicked: {}", e);
                if let Some(reply_sender) = reply {
                    let _ = reply_sender.send(Err("Internal error: handler failed".to_string()));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_dispatcher_add_inventory() {
        let broker = Broker::new();
        let (_tx, rx) = tokio::sync::mpsc::channel::<Message>(100);
        let dispatcher = Dispatcher::new(broker, rx);

        let id = Uuid::new_v4();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

        Dispatcher::handle_add_inventory(
            id,
            &dispatcher.broker,
            &dispatcher.inventories,
            Some(reply_tx),
        )
        .await;

        let response = reply_rx.await.unwrap();
        assert!(response.is_ok());
        assert_eq!(
            response.unwrap(),
            "Inventory added and listening".to_string()
        );
    }

    #[tokio::test]
    async fn test_dispatcher_remove_inventory() {
        let broker = Broker::new();
        let (_tx, rx) = tokio::sync::mpsc::channel::<Message>(100);
        let dispatcher = Dispatcher::new(broker, rx);

        let inventory_id = Uuid::new_v4();

        Dispatcher::handle_add_inventory(
            inventory_id,
            &dispatcher.broker,
            &dispatcher.inventories,
            None,
        )
        .await;

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

        Dispatcher::handle_remove_inventory(inventory_id, &dispatcher.inventories, Some(reply_tx))
            .await;

        let response = reply_rx.await.unwrap();

        assert!(response.is_ok());
        assert_eq!(
            response.unwrap(),
            format!("Inventory {inventory_id} removed")
        );
    }

    #[tokio::test]
    async fn test_dispatcher_handle_task_request() {
        let broker = Broker::new();
        let (_tx, rx) = tokio::sync::mpsc::channel::<Message>(100);
        let dispatcher = Dispatcher::new(broker, rx);
        let (response_tx, _response_rx) = tokio::sync::mpsc::channel(100);

        let task_request = crate::actor::model::TaskRequest {
            owner: Uuid::new_v4(),
            request_id: "test_request".to_string(),
            kind: crate::actor::model::TaskKind::Build,
            respond_to: response_tx.clone(),
            item: "test_item".to_string(),
        };

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

        Dispatcher::handle_task_request(
            dispatcher.topic(),
            task_request,
            &dispatcher.queue,
            Some(reply_tx),
        )
        .await;

        let response = reply_rx.await.unwrap();
        assert!(response.is_ok());
        assert_eq!(response.unwrap(), "Task request received".to_string());
        assert_eq!(dispatcher.queue.lock().unwrap().len(), 1);
    }
}
