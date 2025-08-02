use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::actor::broker::{Broker, INVENTORY_TOPIC, TASK_TOPIC};
use crate::actor::inventory::Inventory;
use crate::actor::model::{InternalMessage, Message, Queue, Task, WebsocketMessage};
use crate::actor::worker::spawn_worker;
use crate::persistence::worker;

const MAX_WAIT_TIME: u64 = 10; // seconds
const CONCURRENT_TASKS: usize = 10; // Maximum concurrent tasks

pub struct Dispatcher {
    broker: Broker,
    queue: Queue,
    active_tasks: Arc<AtomicUsize>,
    inventories: Arc<Mutex<HashMap<Uuid, Inventory>>>,
    ws_receiver: tokio::sync::mpsc::Receiver<Message>,
    persistence_sender: tokio::sync::mpsc::Sender<worker::Op>,

    handles: JoinSet<()>,
    task_handle: Option<tokio::task::JoinHandle<()>>,
}

impl Dispatcher {
    pub fn new(
        broker: &Broker,
        ws_receiver: tokio::sync::mpsc::Receiver<Message>,
        persistence_sender: &tokio::sync::mpsc::Sender<worker::Op>,
    ) -> Self {
        let queue: Queue = Arc::new(Mutex::new(VecDeque::<Task>::new()));
        let handles = JoinSet::new();
        let active_tasks = Arc::new(AtomicUsize::new(0));
        let inventories = Arc::new(Mutex::new(HashMap::new()));

        Self {
            broker: broker.clone(),
            queue,
            active_tasks,
            inventories,
            ws_receiver,
            persistence_sender: persistence_sender.clone(),
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

    pub fn broadcast(
        &self,
        msg: InternalMessage,
    ) -> Result<(), tokio::sync::broadcast::error::SendError<InternalMessage>> {
        for topic in self.broker.topics() {
            self.broker.topic(&topic).publish(msg.clone())?;
        }
        Ok(())
    }

    pub async fn stop(&mut self) {
        tracing::info!("Initiating graceful shutdown...");

        // First, signal graceful stop to prevent new tasks from being processed
        let _ = self.broadcast(InternalMessage::GracefulStop);

        // Wait for all tasks to complete
        self.wait_for_tasks_completion().await;

        tracing::debug!("All tasks completed (or timed out), stopping workers...");

        // Then send stop signal to terminate workers
        let _ = self.broadcast(InternalMessage::Stop);

        tracing::debug!("Sent stop signal to workers");

        // Stop handles
        self.stop_websocket_handle().await;
        self.stop_worker_handles().await;

        tracing::debug!("All tasks completed and workers stopped.");
    }

    async fn wait_for_tasks_completion(&self) {
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
    }

    async fn stop_websocket_handle(&mut self) {
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
    }

    async fn stop_worker_handles(&mut self) {
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

        let message_semaphore = Arc::new(Semaphore::new(CONCURRENT_TASKS));

        loop {
            tokio::select! {
                message = self.ws_receiver.recv() => {
                    if !Self::handle_websocket_message(
                        message,
                        &task_tx,
                        &queue,
                        &broker,
                        &inventories,
                        &message_semaphore,
                        &self.persistence_sender,
                    ).await {
                        break;
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
        let _ = self.broadcast(InternalMessage::Stop);

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
        match self.queue.lock() {
            Ok(queue) => queue.len(),
            Err(e) => {
                tracing::error!("Failed to acquire queue lock: {}", e);
                0
            }
        }
    }

    async fn handle_websocket_message(
        message: Option<Message>,
        task_tx: &tokio::sync::broadcast::Sender<InternalMessage>,
        queue: &Queue,
        broker: &Broker,
        inventories: &Arc<Mutex<HashMap<Uuid, Inventory>>>,
        message_semaphore: &Arc<Semaphore>,
        persistence_sender: &tokio::sync::mpsc::Sender<worker::Op>,
    ) -> bool {
        match message {
            Some(message) => {
                Self::process_message_content(
                    message,
                    task_tx,
                    queue,
                    broker,
                    inventories,
                    message_semaphore,
                    persistence_sender,
                )
                .await;
                true
            }
            None => {
                tracing::debug!("WebSocket receiver channel closed, stopping dispatcher");
                false
            }
        }
    }

    async fn process_message_content(
        message: Message,
        task_tx: &tokio::sync::broadcast::Sender<InternalMessage>,
        queue: &Queue,
        broker: &Broker,
        inventories: &Arc<Mutex<HashMap<Uuid, Inventory>>>,
        message_semaphore: &Arc<Semaphore>,
        persistence_sender: &tokio::sync::mpsc::Sender<worker::Op>,
    ) {
        match message.content {
            WebsocketMessage::TaskRequest(task_request) => {
                Self::spawn_task_request_handler(
                    task_tx.clone(),
                    task_request,
                    queue.clone(),
                    message_semaphore.clone(),
                    message.reply,
                )
                .await;
            }
            WebsocketMessage::AddInventory(id) => {
                Self::spawn_add_inventory_handler(
                    id,
                    broker.clone(),
                    inventories.clone(),
                    message_semaphore.clone(),
                    persistence_sender.clone(),
                    message.reply,
                )
                .await;
            }
            WebsocketMessage::RemoveInventory(id) => {
                Self::spawn_remove_inventory_handler(
                    id,
                    inventories.clone(),
                    message_semaphore.clone(),
                    message.reply,
                )
                .await;
            }
        }
    }

    async fn spawn_task_request_handler(
        task_tx: tokio::sync::broadcast::Sender<InternalMessage>,
        task_request: crate::actor::model::TaskRequest,
        queue: Queue,
        semaphore: Arc<Semaphore>,
        reply: Option<tokio::sync::oneshot::Sender<Result<String, String>>>,
    ) {
        tokio::spawn(async move {
            let _permit = semaphore.acquire().await.unwrap();
            Self::handle_task_request(task_tx, task_request, &queue, reply).await;
        });
    }

    async fn spawn_add_inventory_handler(
        id: Uuid,
        broker: Broker,
        inventories: Arc<Mutex<HashMap<Uuid, Inventory>>>,
        semaphore: Arc<Semaphore>,
        persistence_sender: tokio::sync::mpsc::Sender<worker::Op>,
        reply: Option<tokio::sync::oneshot::Sender<Result<String, String>>>,
    ) {
        tokio::spawn(async move {
            let _permit = semaphore.acquire().await.unwrap();
            Self::handle_add_inventory(id, &broker, &inventories, reply, persistence_sender).await;
        });
    }

    async fn spawn_remove_inventory_handler(
        id: Uuid,
        inventories: Arc<Mutex<HashMap<Uuid, Inventory>>>,
        semaphore: Arc<Semaphore>,
        reply: Option<tokio::sync::oneshot::Sender<Result<String, String>>>,
    ) {
        tokio::spawn(async move {
            let _permit = semaphore.acquire().await.unwrap();
            Self::handle_remove_inventory(id, &inventories, reply).await;
        });
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
        persistence_sender: tokio::sync::mpsc::Sender<worker::Op>,
    ) {
        tracing::info!("Adding inventory with ID: {}", id);

        let creation_result =
            Self::create_inventory_if_not_exists(id, broker, inventories, &persistence_sender)
                .await;

        Self::handle_inventory_creation_result(creation_result, id, reply).await;
    }

    async fn create_inventory_if_not_exists(
        id: Uuid,
        broker: &Broker,
        inventories: &Arc<Mutex<HashMap<Uuid, Inventory>>>,
        persistence_sender: &tokio::sync::mpsc::Sender<worker::Op>,
    ) -> Result<Option<Inventory>, String> {
        let inventories_clone = inventories.clone();
        let broker_clone = broker.clone();
        let persistence_sender_clone = persistence_sender.clone();

        tokio::task::spawn_blocking(move || {
            match inventories_clone.lock() {
                Ok(mut inventories) => {
                    if let std::collections::hash_map::Entry::Vacant(e) = inventories.entry(id) {
                        let inventory = Inventory::new(
                            id,
                            broker_clone.topic(INVENTORY_TOPIC).sender.clone(),
                            &persistence_sender_clone,
                            None,
                        );
                        let inventory_clone = inventory.clone();
                        e.insert(inventory);
                        tracing::debug!("New inventory created: {}", id);
                        Ok(Some(inventory_clone)) // needs spawning
                    } else {
                        tracing::debug!("Inventory already exists: {}", id);
                        Ok(None) // doesn't need spawning
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to acquire inventories lock: {}", e);
                    Err("Internal error: inventories unavailable".to_string())
                }
            }
        })
        .await
        .unwrap_or_else(|e| {
            tracing::error!("Create inventory handler panicked: {}", e);
            Err("Internal error: handler failed".to_string())
        })
    }

    async fn handle_inventory_creation_result(
        result: Result<Option<Inventory>, String>,
        id: Uuid,
        reply: Option<tokio::sync::oneshot::Sender<Result<String, String>>>,
    ) {
        match result {
            Ok(Some(inventory)) => {
                tokio::spawn(async move { inventory.listen().await });
                tracing::debug!("New inventory added and listening: {}", id);
                Self::send_reply(reply, Ok("Inventory added and listening".to_string()));
            }
            Ok(None) => {
                Self::send_reply(reply, Ok("Inventory already exists".to_string()));
            }
            Err(e) => {
                Self::send_reply(reply, Err(e));
            }
        }
    }

    fn send_reply(
        reply: Option<tokio::sync::oneshot::Sender<Result<String, String>>>,
        result: Result<String, String>,
    ) {
        if let Some(reply_sender) = reply {
            let _ = reply_sender.send(result);
        }
    }

    async fn handle_remove_inventory(
        id: Uuid,
        inventories: &Arc<Mutex<HashMap<Uuid, Inventory>>>,
        reply: Option<tokio::sync::oneshot::Sender<Result<String, String>>>,
    ) {
        tracing::debug!("Removing inventory with ID: {}", id);

        let removal_result = Self::remove_inventory_and_stop(id, inventories).await;
        Self::send_reply(reply, removal_result);
    }

    async fn remove_inventory_and_stop(
        id: Uuid,
        inventories: &Arc<Mutex<HashMap<Uuid, Inventory>>>,
    ) -> Result<String, String> {
        let inventories_clone = inventories.clone();

        let result = tokio::task::spawn_blocking(move || {
            match inventories_clone.lock() {
                Ok(mut inventories) => {
                    // Check existence and remove in a single atomic operation
                    if let Some(inventory) = inventories.remove(&id) {
                        let rt = tokio::runtime::Handle::current();
                        rt.block_on(async {
                            inventory.stop().await;
                        });
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
            Ok(result) => result,
            Err(e) => {
                tracing::error!("Remove inventory handler panicked: {}", e);
                Err("Internal error: handler failed".to_string())
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
        let persistence_sender = tokio::sync::mpsc::channel(100).0;
        let (_tx, rx) = tokio::sync::mpsc::channel::<Message>(100);
        let dispatcher = Dispatcher::new(&broker, rx, &persistence_sender);

        let id = Uuid::new_v4();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

        Dispatcher::handle_add_inventory(
            id,
            &dispatcher.broker,
            &dispatcher.inventories,
            Some(reply_tx),
            persistence_sender.clone(),
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
        let persistence_sender = tokio::sync::mpsc::channel(100).0;
        let (_tx, rx) = tokio::sync::mpsc::channel::<Message>(100);
        let dispatcher = Dispatcher::new(&broker, rx, &persistence_sender);

        let inventory_id = Uuid::new_v4();

        Dispatcher::handle_add_inventory(
            inventory_id,
            &dispatcher.broker,
            &dispatcher.inventories,
            None,
            persistence_sender.clone(),
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
        let persistence_sender = tokio::sync::mpsc::channel(100).0;
        let (_tx, rx) = tokio::sync::mpsc::channel::<Message>(100);
        let dispatcher = Dispatcher::new(&broker, rx, &persistence_sender);
        let (response_tx, _response_rx) = tokio::sync::mpsc::channel(100);

        let task_request = crate::actor::model::TaskRequest {
            owner: Uuid::new_v4(),
            request_id: "test_request".to_string(),
            kind: crate::actor::model::TaskKind::Build("test_item".to_string()),
            respond_to: response_tx.clone(),
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
        assert_eq!(
            dispatcher
                .queue
                .lock()
                .expect("Failed to acquire queue lock in test_dispatcher_handle_task_request")
                .len(),
            1
        );
    }
}
