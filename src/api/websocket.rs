use tokio::net::TcpStream;
use uuid::Uuid;
use futures_util::{SinkExt, StreamExt};
use crate::actor::model;
use tokio_tungstenite::tungstenite::Message;

pub async fn accept_connection(stream: TcpStream, tx: tokio::sync::broadcast::Sender<model::TaskRequest>) {
    let addr = stream.peer_addr().expect("connected streams should have a peer address");
    let id = Uuid::new_v4();

    let ws_stream = tokio_tungstenite::accept_async(stream)
        .await
        .expect("Error during WebSocket handshake");
    
    tracing::info!("Accepted connection with ID: {}, address: {}", id, addr);

    let (mut write, mut read) = ws_stream.split();

    while let Some(message) = read.next().await {
        match message {
            Ok(msg) => {
                if !msg.is_text() {
                    continue;
                }
                tracing::info!("Received message from {}: {:?}", id, msg);
                tx.send(model::TaskRequest {
                    owner: id,
                    item: msg.to_string(),
                    kind: model::TaskKind::Build, // Default kind, can be modified as needed
                }).expect("Failed to send task request");
                write.send(Message::Text("OK".into())).await.expect("Failed to send response");
                // Here you can handle the message, e.g., dispatch it to an actor
            }
            Err(e) => {
                write.send(Message::Text("error".into())).await.expect("Failed to send response");
                tracing::error!("Error reading message from {}: {}", id, e);
                break;
            }
        }
    }
}