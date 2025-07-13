use crate::actor::model::InternalMessage;

pub struct Broker {
    pub receiver: tokio::sync::broadcast::Receiver<InternalMessage>,
    pub sender: tokio::sync::broadcast::Sender<InternalMessage>,
}

impl Broker {
    pub fn new() -> Self {
        let (sender, receiver) = tokio::sync::broadcast::channel(100);
        Broker { sender, receiver }
    }
}