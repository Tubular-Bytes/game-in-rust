use std::collections::HashMap;
use std::sync::Arc;

use std::sync::Mutex;

use crate::actor::model::InternalMessage;

pub const WEBSOCKET_TOPIC: &str = "websocket";
pub const INVENTORY_TOPIC: &str = "inventory";

#[derive(Clone, Debug)]
pub struct Topic {
    pub name: String,
    pub sender: tokio::sync::broadcast::Sender<InternalMessage>,
}

impl Topic {
    fn new(name: String) -> Self {
        let (sender, _) = tokio::sync::broadcast::channel(100);
        Topic { name, sender }
    }

    pub fn publish(
        &self,
        message: InternalMessage,
    ) -> Result<usize, tokio::sync::broadcast::error::SendError<InternalMessage>> {
        self.sender.send(message)
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<InternalMessage> {
        self.sender.subscribe()
    }
}

#[derive(Debug, Clone)]
pub struct Broker {
    topics: Arc<Mutex<HashMap<String, Topic>>>,
}

impl Broker {
    pub fn new() -> Self {
        let topics = Arc::new(Mutex::new(HashMap::new()));
        Broker { topics }
    }

    #[tracing::instrument(name = "Broker::topic", skip(self))]
    pub fn topic(&self, name: &str) -> Topic {
        let name = name.to_string();
        self.topics
            .lock()
            .unwrap()
            .entry(name.clone())
            .or_insert_with(|| Topic::new(name.clone()))
            .clone()
    }

    pub fn subscribe(&self, name: &str) -> tokio::sync::broadcast::Receiver<InternalMessage> {
        self.topic(name).subscribe()
    }
}

impl Default for Broker {
    fn default() -> Self {
        Self::new()
    }
}
