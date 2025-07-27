use crate::{actor::model::InternalMessage, blueprint::model::Value, persistence::worker};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq)]
enum Status {
    Listening,
    Stopped,
    Stopping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub id: Uuid,
    pub resources: HashMap<String, Value<u64>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SerializableInventory {
    id: Uuid,
    resources: HashMap<String, Value<u64>>,
    reserved: HashMap<Uuid, Receipt>,
}

#[derive(Debug, Clone)]
pub struct Inventory {
    pub id: Uuid,
    status: Arc<Mutex<Status>>,
    pub resources: Arc<Mutex<HashMap<String, Value<u64>>>>,
    pub reserved: Arc<Mutex<HashMap<Uuid, Receipt>>>,
    broker: tokio::sync::broadcast::Sender<InternalMessage>,
    persistence_tx: tokio::sync::mpsc::Sender<worker::Op>,
}

impl Inventory {
    #[tracing::instrument(skip(broker, persistence_tx))]
    pub fn new(
        id: Uuid,
        broker: tokio::sync::broadcast::Sender<InternalMessage>,
        persistence_tx: &tokio::sync::mpsc::Sender<worker::Op>,
    ) -> Self {
        let inventory = Self {
            id,
            status: Arc::new(Mutex::new(Status::Listening)),
            resources: Arc::new(Mutex::new(HashMap::new())),
            reserved: Arc::new(Mutex::new(HashMap::new())),
            broker,
            persistence_tx: persistence_tx.clone(),
        };

        // Spawn a blocking task to restore from persistence
        let inventory_for_restore = inventory.clone();
        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                match inventory_for_restore.restore().await {
                    Ok(()) => {
                        tracing::info!(
                            "Successfully restored inventory {}",
                            inventory_for_restore.id
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to restore inventory {}: {}",
                            inventory_for_restore.id,
                            e
                        );
                        // Continue with empty inventory if restore fails
                    }
                }
            });
        });

        inventory
    }

    #[tracing::instrument(skip(self), fields(inventory_id = %self.id))]
    pub fn serialize(&self) -> Result<String, serde_json::Error> {
        tracing::debug!("Serializing inventory");
        let inventory = SerializableInventory {
            id: self.id,
            resources: self.resources.lock().unwrap().clone(),
            reserved: self.reserved.lock().unwrap().clone(),
        };
        let result = serde_json::to_string(&inventory);
        if result.is_ok() {
            tracing::debug!("Inventory serialization successful");
        } else {
            tracing::error!("Inventory serialization failed");
        }
        result
    }

    pub fn id(&self) -> Uuid {
        self.id
    }

    #[tracing::instrument(skip(self), fields(inventory_id = %self.id))]
    pub fn stop(&self) {
        tracing::info!("Stopping inventory");
        let mut status = self.status.lock().unwrap();
        *status = Status::Stopping;
        tracing::debug!("Inventory status set to Stopping");
    }

    #[tracing::instrument(skip(self), fields(inventory_id = %self.id))]
    pub async fn restore(&self) -> Result<(), String> {
        let restore_span = tracing::info_span!("inventory_restore", inventory_id = %self.id);
        let _enter = restore_span.enter();

        tracing::info!("Starting inventory restore process");
        tracing::debug!("Attempting to restore inventory {id}", id = self.id);

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let key = format!("inventory:{}", self.id);
        tracing::debug!("Persistence key: {}", key);

        let op = worker::Op {
            op_type: worker::OpType::Get(key.clone()),
            reply: Some(reply_tx),
        };

        let persistence_span = tracing::debug_span!("send_persistence_request", key = %key);
        if let Err(e) = persistence_span
            .in_scope(|| async { self.persistence_tx.send(op).await })
            .await
        {
            tracing::error!("Failed to send restore operation: {}", e);
            return Err("Failed to send restore operation".to_string());
        }

        let result = reply_rx.await.map_err(|e| {
            tracing::error!("Failed to receive restore response: {}", e);
            "Failed to receive restore response".to_string()
        })?;

        let values = match result {
            Ok(value) => {
                tracing::debug!("Successfully received data from persistence layer");
                value
            }
            Err(e) => {
                tracing::error!("Error restoring inventory: {}", e);
                return Err("Error restoring inventory".to_string());
            }
        };

        let deserialize_span = tracing::debug_span!("deserialize_inventory");
        let inventory: SerializableInventory = deserialize_span.in_scope(|| {
            serde_json::from_str(values.as_str()).map_err(|e| {
                tracing::error!("Failed to deserialize inventory data: {}", e);
                "Failed to deserialize inventory data".to_string()
            })
        })?;

        // try getting locks at once to avoid partial updates
        let lock_span = tracing::debug_span!("acquire_locks");
        let (mut resources, mut reserved) = lock_span.in_scope(|| {
            let resources = self.resources.lock().map_err(|e| {
                tracing::error!("Failed to lock resources: {}", e);
                "Failed to lock resources".to_string()
            })?;
            let reserved = self.reserved.lock().map_err(|e| {
                tracing::error!("Failed to lock reservations: {}", e);
                "Failed to lock reservations".to_string()
            })?;
            Ok::<_, String>((resources, reserved))
        })?;

        // Verify the restored inventory has the same ID
        if inventory.id != self.id {
            tracing::warn!(
                "Restored inventory ID ({}) doesn't match expected ID ({})",
                inventory.id,
                self.id
            );
        }

        *resources = inventory.resources;
        *reserved = inventory.reserved;

        tracing::info!(
            "Inventory restore completed successfully - {} resources, {} reservations",
            resources.len(),
            reserved.len()
        );
        tracing::debug!("Inventory restored successfully: {}", self.id);

        Ok(())
    }

    #[tracing::instrument(skip(self), fields(inventory_id = %self.id))]
    pub async fn persist(&self) -> Result<(), String> {
        let persist_span = tracing::info_span!("inventory_persist", inventory_id = %self.id);
        let _enter = persist_span.enter();

        tracing::info!("Starting inventory persistence");

        let serialize_span = tracing::debug_span!("serialize_inventory");
        let serialized = serialize_span.in_scope(|| {
            self.serialize().map_err(|e| {
                tracing::error!("Failed to serialize inventory for persistence: {}", e);
                format!("Serialization failed: {e}")
            })
        })?;

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let key = format!("inventory:{}", self.id);
        tracing::debug!("Persisting inventory with key: {}", key);

        let op = worker::Op {
            op_type: worker::OpType::Set(key.clone(), serialized),
            reply: Some(reply_tx),
        };

        let persistence_span = tracing::debug_span!("send_persistence_request", key = %key);
        if let Err(e) = persistence_span
            .in_scope(|| async { self.persistence_tx.send(op).await })
            .await
        {
            tracing::error!("Failed to send persist operation: {}", e);
            return Err("Failed to send persist operation".to_string());
        }

        let result = reply_rx.await.map_err(|e| {
            tracing::error!("Failed to receive persist response: {}", e);
            "Failed to receive persist response".to_string()
        })?;

        match result {
            Ok(_) => {
                tracing::info!("Inventory persistence completed successfully");
                Ok(())
            }
            Err(e) => {
                tracing::error!("Persistence operation failed: {:?}", e);
                Err("Persistence operation failed".to_string())
            }
        }
    }

    #[tracing::instrument(skip(self), fields(inventory_id = %self.id))]
    pub async fn listen(&self) {
        tracing::info!("Starting inventory listener");
        tracing::debug!("Listening for inventory updates for ID: {}", self.id);
        let mut receiver = self.broker.subscribe();
        let sender = self.broker.clone();
        let id = self.id;
        let status = self.status.clone();
        let resources = self.resources.clone();
        let reserved = self.reserved.clone();

        {
            *self.status.lock().unwrap() = Status::Listening;
        }

        tokio::spawn(async move {
            loop {
                if *status.lock().unwrap() == Status::Stopping {
                    tracing::debug!("Stopping inventory listener for ID: {}", id);
                    break; // Exit the loop if stopping
                }
                tokio::select! {
                    msg = receiver.recv() => {
                        match msg {
                            Ok(InternalMessage::InventoryReserveRequest(request)) => {
                                let reserve_span = tracing::info_span!(
                                    "inventory_reserve_request",
                                    inventory_id = %id,
                                    resource_count = request.len()
                                );
                                let _enter = reserve_span.enter();

                                tracing::debug!("Received inventory reserve request: {:?}", request);
                                tracing::debug!("resources: {:?} | reserved: {:?}", resources, reserved);

                                let mut resource_lock = resources.lock().unwrap();
                                if request.iter().all(|(name, value)| {
                                    resource_lock
                                        .get(name)
                                        .is_some_and(|res| res.value >= value.value)
                                }) {
                                    tracing::debug!("Found sufficient resources, reserving");
                                    let mut reserve_lock = reserved.lock().unwrap();

                                    for (name, value) in request.clone() {
                                        resource_lock.entry(name.clone()).and_modify(|res| {
                                            res.value -= value.value;
                                        });
                                    }

                                    let receipt_id = Uuid::new_v4();

                                    reserve_lock.insert(receipt_id, Receipt {
                                        id: receipt_id,
                                        resources: request.clone(),
                                    });

                                    tracing::info!("Resources reserved successfully with receipt: {}", receipt_id);
                                    let _ = sender.send(InternalMessage::InventoryReserveResponse(Ok(receipt_id)));

                                } else {
                                    tracing::warn!("Insufficient resources for request: {:?}", request);
                                    let _ = sender.send(InternalMessage::InventoryReserveResponse(Err("insufficient resources".to_string())));
                                }
                            }
                            Ok(InternalMessage::InventoryReleaseRequest(id)) => {
                                let release_span = tracing::info_span!(
                                    "inventory_release_request",
                                    inventory_id = %id,
                                    receipt_id = %id
                                );
                                let _enter = release_span.enter();

                                tracing::debug!("Received inventory release request: {:?}", id);
                                let mut resource_lock = resources.lock().unwrap();
                                let mut reserve_lock = reserved.lock().unwrap();

                                match reserve_lock.get(&id) {
                                    Some(receipt) => {
                                        tracing::debug!("Found receipt for release: {:?}", receipt);
                                        for (name, value) in &receipt.resources {
                                            resource_lock.entry(name.clone()).and_modify(|res| {
                                                res.value += value.value;
                                            });
                                        }
                                        reserve_lock.remove(&id);
                                        tracing::info!("Resources released successfully for receipt: {}", id);
                                        let _ = sender.send(InternalMessage::InventoryReleaseResponse(Ok(id)));
                                    }
                                    None => {
                                        tracing::warn!("No reservation found for ID: {}", id);
                                        let _ = sender.send(InternalMessage::InventoryReleaseResponse(Err("no reservation found".to_string())));
                                    }
                                }
                            }
                            Ok(InternalMessage::Stop) => {
                                tracing::debug!("Stopping inventory listener for ID: {}", id);
                                break; // Exit the loop on stop signal
                            }
                            Err(e) => {
                                tracing::error!("Error receiving message: {}", e);
                                break; // Exit the loop on error
                            }
                            _ => {}
                        }
                    }
                }
            }

            tracing::debug!("Inventory listener stopped");
            *status.lock().unwrap() = Status::Stopped;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::broker::Broker;

    const INVENTORY_TOPIC: &str = "inventory";

    #[tokio::test]
    async fn test_reserve_success() {
        let broker = Broker::new();
        let persistence_tx = tokio::sync::mpsc::channel(100).0;
        let topic = broker.topic(INVENTORY_TOPIC);
        let inventory = Inventory::new(
            Uuid::new_v4(),
            topic.clone().sender.clone(),
            &persistence_tx,
        );

        fn wood() -> String {
            "wood".to_string()
        }

        inventory.resources.lock().unwrap().insert(
            wood(),
            Value {
                name: wood(),
                value: 200,
            },
        );

        inventory.listen().await;

        let request = HashMap::from([(
            wood(),
            Value {
                name: wood(),
                value: 100,
            },
        )]);
        let reserve_request = InternalMessage::InventoryReserveRequest(request);

        topic.clone().sender.send(reserve_request).unwrap();

        let mut rec = topic.clone().sender.subscribe();

        let id;
        let result = rec.recv().await.unwrap();
        match result {
            InternalMessage::InventoryReserveResponse(res) => {
                id = res.unwrap_or_else(|e| panic!("Reservation failed: {}", e));
            }
            _ => panic!("other message received"),
        }

        let resources = inventory.resources.lock().unwrap();
        let reserved = inventory.reserved.lock().unwrap();
        let remaining_wood = resources.get(&wood()).is_some_and(|w| w.value == 100);
        let reserved_wood = reserved
            .get(&id)
            .is_some_and(|w| w.resources.get(&wood()).is_some_and(|v| v.value == 100));

        assert!(remaining_wood);
        assert!(reserved_wood);
    }

    #[tokio::test]
    async fn test_reserve_insufficient_resources() {
        let persistence_tx = tokio::sync::mpsc::channel(100).0;
        let broker = Broker::new();
        let topic = broker.topic(INVENTORY_TOPIC);
        let inventory = Inventory::new(
            Uuid::new_v4(),
            topic.clone().sender.clone(),
            &persistence_tx,
        );

        fn wood() -> String {
            "wood".to_string()
        }

        inventory.resources.lock().unwrap().insert(
            wood(),
            Value {
                name: wood(),
                value: 50,
            },
        );

        inventory.listen().await;

        let request = HashMap::from([(
            wood(),
            Value {
                name: wood(),
                value: 100,
            },
        )]);
        let reserve_request = InternalMessage::InventoryReserveRequest(request);

        topic.clone().sender.send(reserve_request).unwrap();

        let mut rec = topic.clone().sender.subscribe();
        if let Ok(InternalMessage::InventoryReserveResponse(result)) = rec.recv().await {
            assert!(
                result.is_err_and(|res| res == "insufficient resources".to_string()),
                "Reservation should fail and return nil ID"
            );
        } else {
            panic!("Failed to receive inventory reserve response");
        }

        let resources = inventory.resources.lock().unwrap();
        let reserved = inventory.reserved.lock().unwrap();
        let remaining_wood = resources.get(&wood()).is_some_and(|w| w.value == 50);
        let reserved_wood = reserved.len() == 0;

        assert!(remaining_wood);
        assert!(reserved_wood);
    }

    #[tokio::test]
    async fn test_inventory_listener_stop() {
        let broker = Broker::new();
        let persistence_tx = tokio::sync::mpsc::channel(100).0;
        let topic = broker.topic(INVENTORY_TOPIC);
        let inventory = Inventory::new(
            Uuid::new_v4(),
            topic.clone().sender.clone(),
            &persistence_tx,
        );

        inventory.listen().await;

        let stop_message = InternalMessage::Stop;
        topic.clone().sender.send(stop_message).unwrap();

        // Wait a moment to ensure the listener has stopped
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let status = inventory.status.lock().unwrap();
        assert!(
            matches!(*status, Status::Stopped),
            "Inventory listener should be stopped"
        );
    }

    #[tokio::test]
    async fn test_inventory_release() {
        let broker = Broker::new();
        let topic = broker.topic(INVENTORY_TOPIC);
        let persistence_tx = tokio::sync::mpsc::channel(100).0;
        let inventory = Inventory::new(
            Uuid::new_v4(),
            topic.clone().sender.clone(),
            &persistence_tx,
        );

        fn wood() -> String {
            "wood".to_string()
        }

        let receipt_id = Uuid::new_v4();
        {
            let mut resources = inventory.resources.lock().unwrap();
            let mut reserved = inventory.reserved.lock().unwrap();

            resources.insert(
                wood(),
                Value {
                    name: wood(),
                    value: 0,
                },
            );

            reserved.insert(
                receipt_id,
                Receipt {
                    id: receipt_id,
                    resources: HashMap::from([(
                        wood(),
                        Value {
                            name: wood(),
                            value: 100,
                        },
                    )]),
                },
            );
        }

        inventory.listen().await;

        let release_request = InternalMessage::InventoryReleaseRequest(receipt_id);
        let tx = topic.clone().sender.clone();
        let _ = tx.send(release_request);

        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

        {
            let resources = inventory.resources.lock().unwrap();
            let reserved = inventory.reserved.lock().unwrap();

            assert!(resources.get(&wood()).is_some_and(|v| v.value == 100));
            assert!(!reserved.contains_key(&receipt_id));
        }
    }

    #[tokio::test]
    async fn test_inventory_internal_stop() {
        let broker = Broker::new();
        let topic = broker.topic(INVENTORY_TOPIC);
        let persistence_tx = tokio::sync::mpsc::channel(100).0;
        let inventory = Inventory::new(
            Uuid::new_v4(),
            topic.clone().sender.clone(),
            &persistence_tx,
        );

        inventory.listen().await;

        {
            let status = inventory.status.lock().unwrap();
            assert_eq!(*status, Status::Listening);
        }

        inventory.stop();

        // Wait a moment to ensure the listener has stopped
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        {
            let status = inventory.status.lock().unwrap();
            assert_eq!(*status, Status::Stopped);
        }
    }
}
