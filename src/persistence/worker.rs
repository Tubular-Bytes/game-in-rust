use std::{
    collections::HashMap,
    fmt::Display,
    sync::{Arc, RwLock},
};

type MemoryDB = Arc<RwLock<HashMap<String, String>>>;

#[derive(Debug)]
struct MemoryDBError;

impl Display for MemoryDBError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MemoryDB Error")
    }
}

#[allow(dead_code)] // TODO remove once persistence is fully implemented
enum OpType {
    Stop,
    Set(String, String),
    Delete(String),
    Get(String),
}

pub struct Op {
    op_type: OpType,
    reply: Option<tokio::sync::oneshot::Sender<Result<String, MemoryDBError>>>,
}

pub struct PersistenceWorker {
    db: MemoryDB,
    inbox: tokio::sync::mpsc::Receiver<Op>,
}

impl PersistenceWorker {
    pub fn new(db: MemoryDB, inbox: tokio::sync::mpsc::Receiver<Op>) -> Self {
        Self { db, inbox }
    }

    fn set(&self, key: String, value: String) {
        let mut db = self.db.write().unwrap();
        let entry = db.entry(key).or_default();
        *entry = value;
    }

    pub async fn run(&mut self) {
        while let Some(op) = self.inbox.recv().await {
            match op.op_type {
                OpType::Set(key, value) => {
                    self.set(key, value);
                    if let Some(reply) = op.reply {
                        let _ = reply.send(Ok("{key} set".to_string()));
                    }
                }
                OpType::Delete(key) => {
                    let mut db = self.db.write().unwrap();
                    db.remove(&key);
                    if let Some(reply) = op.reply {
                        let _ = reply.send(Ok("{key} deleted".to_string()));
                    }
                }
                OpType::Get(key) => {
                    let db = self.db.read().unwrap();
                    let value = db.get(&key).cloned();
                    let value = value.ok_or(MemoryDBError);
                    if let Some(reply) = op.reply {
                        let _ = reply.send(value);
                    }
                }
                OpType::Stop => {
                    println!("Stopping PersistenceWorker");
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

    #[tokio::test]
    async fn test_persistence_handle_insert() {
        let db: MemoryDB = Arc::new(RwLock::new(HashMap::new()));
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

        assert_eq!(db.read().unwrap().get("key1"), Some(&"value1".to_string()));

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
        let db: MemoryDB = Arc::new(RwLock::new(HashMap::new()));
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let mut worker = PersistenceWorker::new(db.clone(), rx);

        db.write()
            .unwrap()
            .insert("key1".to_string(), "value1".to_string());

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

        assert_eq!(db.read().unwrap().get("key1"), Some(&"value2".to_string()));

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
        let db: MemoryDB = Arc::new(RwLock::new(HashMap::new()));
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let mut worker = PersistenceWorker::new(db.clone(), rx);

        db.write()
            .unwrap()
            .insert("key2".to_string(), "value2".to_string());

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
        assert_eq!(db.read().unwrap().get("key2"), None);

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
        let db: MemoryDB = Arc::new(RwLock::new(HashMap::new()));
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let mut worker = PersistenceWorker::new(db.clone(), rx);

        db.write()
            .unwrap()
            .insert("key3".to_string(), "value3".to_string());

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
        let db: MemoryDB = Arc::new(RwLock::new(HashMap::new()));
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
