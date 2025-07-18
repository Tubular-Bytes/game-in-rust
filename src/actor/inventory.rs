use crate::{actor::model::InternalMessage, blueprint::model::Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use uuid::Uuid;

#[derive(Debug, Clone, PartialEq)]
enum Status {
    Listening,
    Stopped,
    Stopping,
}

#[derive(Debug, Clone)]
pub struct Receipt {
    pub id: Uuid,
    pub resources: HashMap<String, Value<u64>>,
}

#[derive(Debug, Clone)]
pub struct Inventory {
    id: Uuid,
    status: Arc<Mutex<Status>>,
    pub resources: Arc<Mutex<HashMap<String, Value<u64>>>>,
    pub reserved: Arc<Mutex<HashMap<Uuid, Receipt>>>,
    broker: tokio::sync::broadcast::Sender<InternalMessage>,
}

impl Inventory {
    pub fn new(id: Uuid, broker: tokio::sync::broadcast::Sender<InternalMessage>) -> Self {
        Self {
            id,
            status: Arc::new(Mutex::new(Status::Listening)),
            resources: Arc::new(Mutex::new(HashMap::new())),
            reserved: Arc::new(Mutex::new(HashMap::new())),
            broker,
        }
    }

    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn stop(&self) {
        let mut status = self.status.lock().unwrap();
        *status = Status::Stopping;
    }

    pub async fn listen(&self) {
        tracing::info!("Listening for inventory updates for ID: {}", self.id);
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
                    tracing::info!("Stopping inventory listener for ID: {}", id);
                    break; // Exit the loop if stopping
                }
                tokio::select! {
                    msg = receiver.recv() => {
                        match msg {
                            Ok(InternalMessage::InventoryReserveRequest(request)) => {
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

                                    tracing::debug!("Resources reserved successfully with receipt: {}", receipt_id);
                                    let _ = sender.send(InternalMessage::InventoryReserveResponse(Ok(receipt_id)));

                                } else {
                                    tracing::debug!("Insufficient resources for request: {:?}", request);
                                    let _ = sender.send(InternalMessage::InventoryReserveResponse(Err("insufficient resources".to_string())));
                                }
                            }
                            Ok(InternalMessage::InventoryReleaseRequest(id)) => {
                                tracing::debug!("Received inventory release request: {:?}", id);
                                let mut reserve_lock = reserved.lock().unwrap();
                                let mut resource_lock = resources.lock().unwrap();

                                match reserve_lock.get(&id) {
                                    Some(receipt) => {
                                        tracing::debug!("Found receipt for release: {:?}", receipt);
                                        for (name, value) in &receipt.resources {
                                            resource_lock.entry(name.clone()).and_modify(|res| {
                                                res.value += value.value;
                                            });
                                        }
                                        reserve_lock.remove(&id);
                                        tracing::debug!("Resources released successfully for receipt: {}", id);
                                        let _ = sender.send(InternalMessage::InventoryReleaseResponse(Ok(id)));
                                    }
                                    None => {
                                        tracing::debug!("No reservation found for ID: {}", id);
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

            tracing::info!("Inventory listener stopped");
            *status.lock().unwrap() = Status::Stopped;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::broker::Broker;

    #[tokio::test]
    async fn test_reserve_success() {
        let broker = Broker::new();
        let inventory = Inventory::new(Uuid::new_v4(), broker.sender.clone());

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

        broker.sender.send(reserve_request).unwrap();

        let mut rec = broker.sender.subscribe();

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
        let broker = Broker::new();
        let inventory = Inventory::new(Uuid::new_v4(), broker.sender.clone());

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

        broker.sender.send(reserve_request).unwrap();

        let mut rec = broker.sender.subscribe();
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
        let inventory = Inventory::new(Uuid::new_v4(), broker.sender.clone());

        inventory.listen().await;

        let stop_message = InternalMessage::Stop;
        broker.sender.send(stop_message).unwrap();

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
        let inventory = Inventory::new(Uuid::new_v4(), broker.sender.clone());

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
        let tx = broker.sender.clone();
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
        let inventory = Inventory::new(Uuid::new_v4(), broker.sender.clone());

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
            assert_eq!(*status, Status::Listening);
        }
    }
}
