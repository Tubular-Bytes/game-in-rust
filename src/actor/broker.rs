use std::collections::HashMap;
use std::sync::Arc;

use std::sync::Mutex;

use crate::actor::model::InternalMessage;

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

    fn publish(&self, message: InternalMessage) {
        let _ = self.sender.send(message);
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<InternalMessage> {
        self.sender.subscribe()
    }
}

#[derive(Debug)]
pub struct Broker {
    topics: Arc<Mutex<HashMap<String, Topic>>>,
    pub receiver: tokio::sync::broadcast::Receiver<InternalMessage>,
    pub sender: tokio::sync::broadcast::Sender<InternalMessage>,
}

impl Broker {
    pub fn new() -> Self {
        let topics = Arc::new(Mutex::new(HashMap::new()));
        let (sender, receiver) = tokio::sync::broadcast::channel(100);
        Broker {
            topics,
            sender,
            receiver,
        }
    }

    pub fn topic(&self, name: String) -> Topic {
        self.topics
            .lock()
            .unwrap()
            .entry(name.clone())
            .or_insert_with(|| Topic::new(name.clone()))
            .clone()
    }

    pub fn subscribe(&self, name: String) -> tokio::sync::broadcast::Receiver<InternalMessage> {
        self.topic(name).subscribe()
    }

    pub async fn publish(&self, topic_name: &String, message: InternalMessage) {
        self.topic(topic_name.clone()).publish(message);
    }
}

impl Default for Broker {
    fn default() -> Self {
        Self::new()
    }
}
