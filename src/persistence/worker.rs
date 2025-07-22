use super::error::MemoryDBError;

#[allow(dead_code)] // TODO remove once persistence is fully implemented
pub enum OpType {
    Stop,
    Set(String, String),
    Delete(String),
    Get(String),
}

pub struct Op {
    pub op_type: OpType,
    pub reply: Option<tokio::sync::oneshot::Sender<Result<String, MemoryDBError>>>,
}

pub struct PersistenceWorker {
    backend: Box<dyn Persister + 'static + Send>,
    // Using a channel to receive operations
    inbox: tokio::sync::mpsc::Receiver<Op>,
}

pub trait Persister {
    fn set(&self, key: String, value: String) -> Result<(), MemoryDBError>;
    fn delete(&self, key: String) -> Result<(), MemoryDBError>;
    fn get(&self, key: String) -> Result<String, MemoryDBError>;
}

impl PersistenceWorker {
    pub fn new(
        db: Box<dyn Persister + 'static + Send>,
        inbox: tokio::sync::mpsc::Receiver<Op>,
    ) -> Self {
        Self { inbox, backend: db }
    }

    pub async fn run(&mut self) {
        while let Some(op) = self.inbox.recv().await {
            match op.op_type {
                OpType::Set(key, value) => {
                    let result = self.backend.set(key, value);
                    if let Some(reply) = op.reply {
                        match result {
                            Ok(_) => {
                                let _ = reply.send(Ok("Value set successfully".to_string()));
                            }
                            Err(e) => {
                                let _ = reply.send(Err(e));
                            }
                        }
                    }
                }
                OpType::Delete(key) => {
                    let result = self.backend.delete(key);
                    if let Some(reply) = op.reply {
                        match result {
                            Ok(_) => {
                                let _ = reply.send(Ok("Value deleted successfully".to_string()));
                            }
                            Err(e) => {
                                let _ = reply.send(Err(e));
                            }
                        }
                    }
                }
                OpType::Get(key) => {
                    let result = self.backend.get(key);
                    if let Some(reply) = op.reply {
                        match result {
                            Ok(value) => {
                                let _ = reply.send(Ok(value));
                            }
                            Err(e) => {
                                let _ = reply.send(Err(e));
                            }
                        }
                    }
                }
                OpType::Stop => {
                    tracing::info!("Stopping PersistenceWorker");
                    if let Some(reply) = op.reply {
                        let _ = reply.send(Ok("Worker stopped".to_string()));
                    }
                    break;
                }
            }
        }
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
        })
        .await
        .unwrap();

        let handle = tokio::spawn(async move {
            worker.run().await;
        });

        assert!(reply_rx.await.is_ok());

        assert_eq!(db.get("key1".to_string()).unwrap(), "value1".to_string());

        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        tx.send(Op {
            op_type: OpType::Stop,
            reply: Some(stop_tx),
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

        assert!(db.set("key1".to_string(), "value1".to_string()).is_ok());

        let handle = tokio::spawn(async move {
            worker.run().await;
        });

        let (update_reply_tx, update_reply_rx) = tokio::sync::oneshot::channel();
        tx.send(Op {
            op_type: OpType::Set("key1".to_string(), "value2".to_string()),
            reply: Some(update_reply_tx),
        })
        .await
        .unwrap();

        assert!(update_reply_rx.await.is_ok());

        assert_eq!(db.get("key1".to_string()).unwrap(), "value2".to_string());

        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        tx.send(Op {
            op_type: OpType::Stop,
            reply: Some(stop_tx),
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

        assert!(db.set("key2".to_string(), "value2".to_string()).is_ok());

        let handle = tokio::spawn(async move {
            worker.run().await;
        });

        let (delete_reply_tx, delete_reply_rx) = tokio::sync::oneshot::channel();
        tx.send(Op {
            op_type: OpType::Delete("key2".to_string()),
            reply: Some(delete_reply_tx),
        })
        .await
        .unwrap();

        let reply = delete_reply_rx.await.unwrap();
        assert!(reply.is_ok());
        assert_eq!(
            db.get("key2".to_string()),
            Err(MemoryDBError::new("Key not found"))
        );

        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        tx.send(Op {
            op_type: OpType::Stop,
            reply: Some(stop_tx),
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

        assert!(db.set("key3".to_string(), "value3".to_string()).is_ok());

        let handle = tokio::spawn(async move {
            worker.run().await;
        });

        let (get_reply_tx, get_reply_rx) = tokio::sync::oneshot::channel();
        tx.send(Op {
            op_type: OpType::Get("key3".to_string()),
            reply: Some(get_reply_tx),
        })
        .await
        .unwrap();

        let reply = get_reply_rx.await.unwrap();
        assert_eq!(reply.unwrap(), "value3".to_string());

        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        tx.send(Op {
            op_type: OpType::Stop,
            reply: Some(stop_tx),
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
        })
        .await
        .unwrap();

        let reply = get_reply_rx.await.unwrap();
        assert!(reply.is_err());

        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        tx.send(Op {
            op_type: OpType::Stop,
            reply: Some(stop_tx),
        })
        .await
        .unwrap();
        assert!(stop_rx.await.is_ok());

        handle.await.unwrap();
    }
}
