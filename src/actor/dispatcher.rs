use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::task::JoinSet;
use uuid::Uuid;

use crate::actor::broker::Broker;
use crate::actor::error::ProcessTaskError;
use crate::actor::model::{Queue, ResponseSignal, InternalMessage, Task, TaskRequest};

pub struct Dispatcher {
    broker: Broker,
    queue: Queue,
    active_tasks: Arc<AtomicUsize>,

    handles: JoinSet<()>,
    task_handle: Option<tokio::task::JoinHandle<()>>,
}

impl Dispatcher {
    pub fn new(broker: Broker) -> Self {
        let queue: Queue = Arc::new(Mutex::new(VecDeque::<Task>::new()));
        let handles = JoinSet::new();
        let active_tasks = Arc::new(AtomicUsize::new(0));

        Self {
            broker,
            queue,
            active_tasks,
            handles,
            task_handle: None,
        }
    }

    pub fn queue(&self) -> Queue {
        self.queue.clone()
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<InternalMessage> {
        self.broker.sender.subscribe()
    }

    pub async fn dispatch(&self, task: Task) {
        self.queue.lock().unwrap().push_back(task);
        let _ = self.broker.sender.send(InternalMessage::TaskAdded);
    }

    pub async fn stop(&mut self) {
        tracing::info!("Initiating graceful shutdown...");

        // First, signal graceful stop to prevent new tasks from being processed
        let _ = self.broker.sender.send(InternalMessage::GracefulStop);

        // Wait for all tasks (active and pending) to complete with timeout
        let max_wait_time = tokio::time::Duration::from_secs(10);
        let start_time = tokio::time::Instant::now();

        loop {
            let active = self.active_task_count();
            let pending = self.pending_task_count();

            if active == 0 && pending == 0 {
                break;
            }

            // Check for timeout
            if start_time.elapsed() > max_wait_time {
                tracing::warn!(
                    "Timeout waiting for tasks to complete. Active: {}, Pending: {}. Forcing shutdown.",
                    active,
                    pending
                );
                break;
            }

            tracing::info!(
                "Waiting for tasks to complete... Active: {}, Pending: {} (elapsed: {:?})",
                active,
                pending,
                start_time.elapsed()
            );

            // If there are pending tasks but no active tasks, send a TaskAdded signal to wake up workers
            if pending > 0 && active == 0 {
                let _ = self.broker.sender.send(InternalMessage::TaskAdded);
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        tracing::info!("All tasks completed (or timed out), stopping workers...");

        // Then send stop signal to terminate workers
        let _ = self.broker.sender.send(InternalMessage::Stop);

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
            let worker_id = Uuid::new_v4();
            let rx = self.subscribe();
            let queue = self.queue.clone();
            let active_tasks = self.active_tasks.clone();

            self.handles.spawn(async move {
                let mut rx = rx;
                let mut graceful_stop = false;
                tracing::info!("Worker started, waiting for signals...");

                loop {
                    // Add timeout to signal receiving to prevent hanging
                    let signal_result = tokio::time::timeout(
                        tokio::time::Duration::from_secs(1),
                        rx.recv()
                    ).await;

                    match signal_result {
                        Ok(Ok(signal)) => {
                            tracing::debug!("Worker received signal: {:?}", signal);

                            match signal {
                                InternalMessage::TaskAdded => {
                                    if !graceful_stop {
                                        if let Err(e) = process_next(queue.clone(), active_tasks.clone(), worker_id).await {
                                            tracing::warn!("Error processing next task: {:?}", e);
                                        }
                                    } else {
                                        tracing::debug!("Graceful stop initiated, ignoring TaskAdded signal");
                                    }
                                }
                                InternalMessage::GracefulStop => {
                                    tracing::info!("Worker received graceful stop signal");
                                    graceful_stop = true;
                                }
                                InternalMessage::Stop => {
                                    tracing::info!("Worker received stop signal, exiting...");
                                    break;
                                },
                                _ => {
                                    tracing::debug!("Worker ignoring signal: {:?}", signal);
                                }
                            }
                        }
                        Ok(Err(e)) => {
                            tracing::warn!("Worker signal receive error: {:?}", e);
                            break;
                        }
                        Err(_) => {
                            // Timeout - check if we should continue or exit
                            if graceful_stop {
                                tracing::debug!("Worker timeout during graceful stop, checking for remaining tasks...");
                                // During graceful stop, check if there are still pending tasks
                                let pending = queue.lock().unwrap().len();
                                if pending == 0 {
                                    tracing::info!("No pending tasks, worker exiting during graceful stop");
                                    break;
                                }
                            }
                            // Continue loop for normal timeout
                        }
                    }
                }

                tracing::info!("Worker finished processing signals.");
            });
        }

        // Move only the receiver and sender, not self, into the spawned task
        let mut websocket_receiver = self.broker.sender.subscribe();
        let queue = self.queue();
        let sender = self.broker.sender.clone();

        self.task_handle = Some(tokio::spawn(async move {
            loop {
                let msg = websocket_receiver.recv().await;
                match msg {
                    Ok(message) => {
                        match message {
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
                            },
                            _ => tracing::debug!("skipping message: {:?}", message),
                        }

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

    pub async fn force_stop(&mut self) {
        tracing::warn!("Force stopping dispatcher...");

        // Send stop signal immediately
        let _ = self.broker.sender.send(InternalMessage::Stop);

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

async fn process_next(
    queue: Arc<Mutex<VecDeque<Task>>>,
    active_tasks: Arc<AtomicUsize>,
    worker_id: Uuid,
) -> Result<(), ProcessTaskError> {
    tracing::debug!("TaskAdded signal received, checking for tasks...");
    let task = { queue.lock().unwrap().pop_front() };

    if let Some(task) = task {
        // Increment active task counter
        active_tasks.fetch_add(1, Ordering::SeqCst);
        tracing::debug!(
            "Worker {} started processing {:?} task with ID: {}",
            worker_id,
            task.kind,
            task.id
        );

        // Simulate task processing time
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Decrement active task counter when done
        active_tasks.fetch_sub(1, Ordering::SeqCst);
        tracing::info!("Completed task with ID: {}", task.id);

        task.respond_to
            .send(ResponseSignal::Success(format!(
                "Task {} completed",
                task.id
            )))
            .await
            .map_err(|e| tracing::error!("Failed to send response: {}", e))
            .ok();
        Ok(())
    } else {
        tracing::debug!("No tasks available in queue for worker {}", worker_id);
        Err(ProcessTaskError {
            message: "No tasks available in queue".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::model::{Task, TaskKind};

    #[tokio::test]
    async fn test_process_next_success() {
        let queue: Queue = Arc::new(Mutex::new(VecDeque::new()));
        let active_tasks = Arc::new(AtomicUsize::new(0));
        let worker_id = Uuid::new_v4();

        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let task = Task {
            id: Uuid::new_v4(),
            request_id: "test_request".to_string(),
            kind: TaskKind::Build,
            respond_to: tx,
        };

        queue.lock().unwrap().push_back(task);

        let res = process_next(queue.clone(), active_tasks.clone(), worker_id).await;

        assert_eq!(active_tasks.load(Ordering::SeqCst), 0);
        assert!(res.is_ok());
        assert!(queue.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_process_next_queue_empty() {
        let queue: Queue = Arc::new(Mutex::new(VecDeque::new()));
        let active_tasks = Arc::new(AtomicUsize::new(0));
        let worker_id = Uuid::new_v4();

        let res = process_next(queue.clone(), active_tasks.clone(), worker_id).await;

        assert_eq!(active_tasks.load(Ordering::SeqCst), 0);
        assert!(res.is_err());
        assert!(queue.lock().unwrap().is_empty());
    }
}
