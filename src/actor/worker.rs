use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use uuid::Uuid;

use crate::actor::{
    error::Error,
    model::{InternalMessage, ResponseSignal, Task},
};

pub async fn spawn_worker(
    receiver: tokio::sync::broadcast::Receiver<InternalMessage>,
    queue: Arc<Mutex<VecDeque<Task>>>,
    active_tasks: Arc<AtomicUsize>,
) {
    let worker_id = uuid::Uuid::new_v4();
    let mut rx = receiver;
    let mut graceful_stop = false;
    tracing::info!("Worker started, waiting for signals...");

    loop {
        // Add timeout to signal receiving to prevent hanging
        let signal_result =
            tokio::time::timeout(tokio::time::Duration::from_secs(1), rx.recv()).await;

        match signal_result {
            Ok(Ok(signal)) => {
                tracing::debug!("Worker received signal: {:?}", signal);

                match signal {
                    InternalMessage::TaskAdded => {
                        if !graceful_stop {
                            if let Err(e) =
                                process_next(queue.clone(), active_tasks.clone(), worker_id).await
                            {
                                match e {
                                    Error::QueueEmptyError => {
                                        tracing::debug!(
                                            "No tasks available in queue for worker {}",
                                            worker_id
                                        );
                                    }
                                    Error::ProcessError { message } => {
                                        tracing::error!("Error processing task: {}", message);
                                    }
                                    Error::InternalError { message } => {
                                        tracing::error!("Internal error: {}", message);
                                    }
                                }
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
                    }
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
                    tracing::debug!(
                        "Worker timeout during graceful stop, checking for remaining tasks..."
                    );
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
}

async fn process_next(
    queue: Arc<Mutex<VecDeque<Task>>>,
    active_tasks: Arc<AtomicUsize>,
    worker_id: Uuid,
) -> Result<(), Error> {
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
        tracing::debug!("Completed task with ID: {}", task.id);

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
        Err(Error::QueueEmptyError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::model::{Queue, Task, TaskKind};

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
