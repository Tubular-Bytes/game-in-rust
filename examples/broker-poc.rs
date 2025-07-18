use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::task::JoinSet;

#[derive(Debug, Clone)]
pub struct Topic {
    pub name: String,
    pub sender: tokio::sync::broadcast::Sender<String>,
}

impl Topic {
    fn new(name: String) -> Self {
        let (sender, _) = tokio::sync::broadcast::channel(100);
        Topic { name, sender }
    }

    fn publish(&self, message: String) {
        let _ = self.sender.send(message);
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.sender.subscribe()
    }
}

#[derive(Debug, Clone)]
pub struct Broker {
    pub topics: Arc<Mutex<HashMap<String, Topic>>>,
}

impl Broker {
    fn new() -> Self {
        Broker {
            topics: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    // fn create_topic(&self, name: String) -> Topic {
    //     let topic = Topic::new(name.clone());
    //     self.topics.lock().unwrap().insert(name, topic.clone());
    //     topic
    // }

    fn topic(&self, name: String) -> Topic {
        self.topics
            .lock()
            .unwrap()
            .entry(name.clone())
            .or_insert_with(|| Topic::new(name.clone()))
            .clone()
    }

    async fn publish(&self, topic_name: &String, message: String) -> Result<(), std::fmt::Error> {
        let topics = self.topics.lock().unwrap();
        if let Some(topic) = topics.get(topic_name) {
            topic.publish(message);
            Ok(())
        } else {
            Err(std::fmt::Error)
        }
    }
}

#[tokio::main]
async fn main() {
    let broker = Broker::new();

    // Example usage
    let topic_name = "example_topic".to_string();
    let topic = broker.topic(topic_name.clone());

    let foo_name = "foo".to_string();
    let topic_foo = broker.topic(foo_name.clone());

    // Subscribe to the topic
    let mut receiver = topic.subscribe();
    let mut receiver_foo = topic_foo.subscribe();

    let topic_clone = topic.name.clone();
    let _listener = tokio::spawn(async move {
        loop {
            let message = receiver.recv().await.unwrap();

            match message.as_str() {
                "stop" => {
                    println!("Stopping listener for topic '{}'", topic_clone);
                    // Break the loop to stop listening
                    break;
                }
                _ => {
                    println!("Received message on topic '{}': {}", topic_clone, message);
                }
            }
        }
    });

    let foo_clone = topic_foo.name.clone();
    let _listener = tokio::spawn(async move {
        loop {
            let message = receiver_foo.recv().await.unwrap();

            match message.as_str() {
                "stop" => {
                    println!("Stopping listener for topic '{}'", foo_clone);
                    // Break the loop to stop listening
                    break;
                }
                _ => {
                    println!("Received message on topic '{}': {}", foo_clone, message);
                }
            }
        }
    });

    let mut set = JoinSet::new();
    for i in 1..=10 {
        let topic_name: String;
        if i % 2 == 0 {
            topic_name = topic_foo.name.clone();
        } else {
            topic_name = topic.name.clone();
        }
        let broker = broker.clone();
        set.spawn(async move {
            let message = format!("Message {} from {}", i, topic_name);
            let _ = broker.publish(&topic_name, message).await;
        });
    }

    set.join_all().await;
    topic.publish("stop".to_string());
}
