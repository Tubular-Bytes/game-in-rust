use opentelemetry::{
    Context, global,
    trace::{Span, Status, TraceContextExt, Tracer},
};

use super::error::MemoryDBError;

#[derive(Debug)] // TODO remove once persistence is fully implemented
pub enum OpType {
    Stop,
    Set(String, String),
    Delete(String),
    Get(String),
}

pub struct Op {
    pub op_type: OpType,
    pub reply: Option<tokio::sync::oneshot::Sender<Result<String, MemoryDBError>>>,
    pub span_context: Option<opentelemetry::trace::SpanContext>,
}

pub struct PersistenceWorker {
    backend: Box<dyn Persister + 'static + Send>,
    // Using a channel to receive operations
    inbox: tokio::sync::mpsc::Receiver<Op>,
}

pub trait Persister {
    fn set(
        &self,
        ctx: Option<opentelemetry::trace::SpanContext>,
        key: String,
        value: String,
    ) -> Result<(), MemoryDBError>;
    fn delete(
        &self,
        ctx: Option<opentelemetry::trace::SpanContext>,
        key: String,
    ) -> Result<(), MemoryDBError>;
    fn get(
        &self,
        ctx: Option<opentelemetry::trace::SpanContext>,
        key: String,
    ) -> Result<String, MemoryDBError>;
}

impl PersistenceWorker {
    pub fn new(
        db: Box<dyn Persister + 'static + Send>,
        inbox: tokio::sync::mpsc::Receiver<Op>,
    ) -> Self {
        Self { inbox, backend: db }
    }

    pub async fn run(&mut self) {
        tracing::info!("Starting persistence worker");
        while let Some(op) = self.inbox.recv().await {
            let span_context = op
                .span_context
                .clone()
                .unwrap_or_else(opentelemetry::trace::SpanContext::empty_context);

            let tracer = global::tracer("persistence_worker");

            let context = Context::current().with_remote_span_context(span_context.clone());
            let mut op_span = tracer.start_with_context("persistence.op", &context);

            // let span_context = tracing::Span::current().context().with_span(op_span);
            let cx = op_span.span_context().clone();

            match op.op_type {
                OpType::Set(key, value) => {
                    op_span.set_attributes(vec![
                        opentelemetry::KeyValue::new("key", key.clone()),
                        opentelemetry::KeyValue::new("value_size", value.len() as i64),
                        opentelemetry::KeyValue::new("operation", "SET"),
                    ]);

                    tracing::debug!("Processing SET operation for key: {}", key);

                    let result = self.backend.set(Some(cx), key.clone(), value.clone());
                    if let Some(reply) = op.reply {
                        match result {
                            Ok(_) => {
                                op_span.add_event(
                                    "SET operation successful",
                                    vec![
                                        opentelemetry::KeyValue::new("key", key.clone()),
                                        opentelemetry::KeyValue::new(
                                            "value_length",
                                            value.len() as i64,
                                        ),
                                    ],
                                );
                                op_span.set_status(Status::Ok);
                                let _ = reply.send(Ok("Value set successfully".to_string()));
                            }
                            Err(e) => {
                                op_span.add_event(
                                    "SET operation failed",
                                    vec![
                                        opentelemetry::KeyValue::new("key", key.clone()),
                                        opentelemetry::KeyValue::new("error", format!("{e:?}")),
                                    ],
                                );
                                op_span.set_status(Status::error(e.to_string()));
                                let _ = reply.send(Err(e));
                            }
                        }
                    }
                }
                OpType::Delete(key) => {
                    op_span.set_attributes(vec![
                        opentelemetry::KeyValue::new("key", key.clone()),
                        opentelemetry::KeyValue::new("operation", "DELETE"),
                    ]);

                    tracing::debug!("Processing DELETE operation for key: {}", key);

                    let result = self.backend.delete(Some(cx), key.clone());
                    if let Some(reply) = op.reply {
                        match result {
                            Ok(_) => {
                                op_span.add_event(
                                    "DELETE operation successful",
                                    vec![opentelemetry::KeyValue::new("key", key.clone())],
                                );
                                op_span.set_status(Status::Ok);
                                let _ = reply.send(Ok("Value deleted successfully".to_string()));
                            }
                            Err(e) => {
                                op_span.add_event(
                                    "DELETE operation failed",
                                    vec![
                                        opentelemetry::KeyValue::new("key", key.clone()),
                                        opentelemetry::KeyValue::new("error", format!("{e:?}")),
                                    ],
                                );
                                op_span.set_status(Status::error(e.to_string()));
                                let _ = reply.send(Err(e));
                            }
                        }
                    }
                }
                OpType::Get(key) => {
                    op_span.set_attributes(vec![
                        opentelemetry::KeyValue::new("key", key.clone()),
                        opentelemetry::KeyValue::new("operation", "GET"),
                    ]);

                    tracing::debug!("Processing GET operation for key: {}", key);

                    let result = self.backend.get(Some(cx), key.clone());
                    if let Some(reply) = op.reply {
                        match result {
                            Ok(value) => {
                                op_span.add_event(
                                    "GET operation successful",
                                    vec![
                                        opentelemetry::KeyValue::new("key", key.clone()),
                                        opentelemetry::KeyValue::new(
                                            "value_length",
                                            value.len() as i64,
                                        ),
                                    ],
                                );
                                op_span.set_status(Status::Ok);
                                let _ = reply.send(Ok(value));
                            }
                            Err(e) => {
                                op_span.add_event(
                                    "GET operation failed",
                                    vec![
                                        opentelemetry::KeyValue::new("key", key.clone()),
                                        opentelemetry::KeyValue::new("error", format!("{e:?}")),
                                    ],
                                );
                                op_span.set_status(Status::error(e.to_string()));
                                let _ = reply.send(Err(e));
                            }
                        }
                    }
                }
                OpType::Stop => {
                    tracing::info!("Stopping PersistenceWorker - shutdown signal received");
                    op_span.set_attributes(vec![opentelemetry::KeyValue::new("operation", "STOP")]);
                    if let Some(reply) = op.reply {
                        let _ = reply.send(Ok("Worker stopped".to_string()));
                    }
                    break;
                }
            }
        }
        tracing::info!("Persistence worker shutdown complete");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::inmemory::MemoryDatabase;

    #[tokio::test]
    async fn test_persistence_handle_insert() {
        let db = Box::new(MemoryDatabase::new());
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let mut worker = PersistenceWorker::new(db.clone(), rx);

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.send(Op {
            op_type: OpType::Set("key1".to_string(), "value1".to_string()),
            reply: Some(reply_tx),
            span_context: None,
        })
        .await
        .unwrap();

        let handle = tokio::spawn(async move {
            worker.run().await;
        });

        assert!(reply_rx.await.is_ok());

        assert_eq!(
            db.get(None, "key1".to_string()).unwrap(),
            "value1".to_string()
        );

        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        tx.send(Op {
            op_type: OpType::Stop,
            reply: Some(stop_tx),
            span_context: None,
        })
        .await
        .unwrap();
        assert!(stop_rx.await.is_ok());

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_persistence_handle_update() {
        let db = Box::new(MemoryDatabase::new());
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let mut worker = PersistenceWorker::new(db.clone(), rx);

        assert!(
            db.set(None, "key1".to_string(), "value1".to_string())
                .is_ok()
        );

        let handle = tokio::spawn(async move {
            worker.run().await;
        });

        let (update_reply_tx, update_reply_rx) = tokio::sync::oneshot::channel();
        tx.send(Op {
            op_type: OpType::Set("key1".to_string(), "value2".to_string()),
            reply: Some(update_reply_tx),
            span_context: None,
        })
        .await
        .unwrap();

        assert!(update_reply_rx.await.is_ok());

        assert_eq!(
            db.get(None, "key1".to_string()).unwrap(),
            "value2".to_string()
        );

        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        tx.send(Op {
            op_type: OpType::Stop,
            reply: Some(stop_tx),
            span_context: None,
        })
        .await
        .unwrap();
        assert!(stop_rx.await.is_ok());

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_persistence_handle_delete() {
        let db = Box::new(MemoryDatabase::new());
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let mut worker = PersistenceWorker::new(db.clone(), rx);

        assert!(
            db.set(None, "key2".to_string(), "value2".to_string())
                .is_ok()
        );

        let handle = tokio::spawn(async move {
            worker.run().await;
        });

        let (delete_reply_tx, delete_reply_rx) = tokio::sync::oneshot::channel();
        tx.send(Op {
            op_type: OpType::Delete("key2".to_string()),
            reply: Some(delete_reply_tx),
            span_context: None,
        })
        .await
        .unwrap();

        let reply = delete_reply_rx.await.unwrap();
        assert!(reply.is_ok());
        assert_eq!(
            db.get(None, "key2".to_string()),
            Err(MemoryDBError::new("Key not found"))
        );

        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        tx.send(Op {
            op_type: OpType::Stop,
            reply: Some(stop_tx),
            span_context: None,
        })
        .await
        .unwrap();
        assert!(stop_rx.await.is_ok());

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_persistence_handle_get_existing() {
        let db = Box::new(MemoryDatabase::new());
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let mut worker = PersistenceWorker::new(db.clone(), rx);

        assert!(
            db.set(None, "key3".to_string(), "value3".to_string())
                .is_ok()
        );

        let handle = tokio::spawn(async move {
            worker.run().await;
        });

        let (get_reply_tx, get_reply_rx) = tokio::sync::oneshot::channel();
        tx.send(Op {
            op_type: OpType::Get("key3".to_string()),
            reply: Some(get_reply_tx),
            span_context: None,
        })
        .await
        .unwrap();

        let reply = get_reply_rx.await.unwrap();
        assert_eq!(reply.unwrap(), "value3".to_string());

        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        tx.send(Op {
            op_type: OpType::Stop,
            reply: Some(stop_tx),
            span_context: None,
        })
        .await
        .unwrap();
        assert!(stop_rx.await.is_ok());

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_persistence_handle_get_nonexistent() {
        let db = Box::new(MemoryDatabase::new());
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let mut worker = PersistenceWorker::new(db.clone(), rx);

        let handle = tokio::spawn(async move {
            worker.run().await;
        });

        let (get_reply_tx, get_reply_rx) = tokio::sync::oneshot::channel();
        tx.send(Op {
            op_type: OpType::Get("nonexistent".to_string()),
            reply: Some(get_reply_tx),
            span_context: None,
        })
        .await
        .unwrap();

        let reply = get_reply_rx.await.unwrap();
        assert!(reply.is_err());

        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        tx.send(Op {
            op_type: OpType::Stop,
            reply: Some(stop_tx),
            span_context: None,
        })
        .await
        .unwrap();
        assert!(stop_rx.await.is_ok());

        handle.await.unwrap();
    }
}
