use tokio::net::TcpStream;
use uuid::Uuid;
use futures_util::{SinkExt, StreamExt};
use crate::actor::model::{self, ResponseSignal};
use tokio_tungstenite::tungstenite::Message;

pub async fn accept_connection(stream: TcpStream, tx: tokio::sync::broadcast::Sender<model::TaskRequest>) {
    let addr = stream.peer_addr().expect("connected streams should have a peer address");
    let id = Uuid::new_v4();

    let ws_stream = tokio_tungstenite::accept_async(stream)
        .await
        .expect("Error during WebSocket handshake");
    
    tracing::info!("Accepted connection with ID: {}, address: {}", id, addr);

    let (mut write, mut read) = ws_stream.split();
    let (response_tx, mut response_rx) = tokio::sync::mpsc::channel::<ResponseSignal>(100);

    let response_handler = tokio::spawn(async move {
        while let Some(response) = response_rx.recv().await {
            if let ResponseSignal::Stop = response {
                tracing::info!("Stopping response handler for ID: {}", id);
                break;
            }

            tracing::info!("Sending response to {}: {}", id, response);
            let _ = write.send(Message::Text(format!("{}", response).into())).await.expect("Failed to send response");
        }
    });

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
                    respond_to: response_tx.clone(),
                }).expect("Failed to send task request");

                response_tx.clone().send(ResponseSignal::Success("Task received".to_string()))
                    .await
                    .expect("Failed to send response");
            }
            Err(e) => {
                response_tx.clone().send(ResponseSignal::Error(e.to_string()))
                    .await
                    .expect("Failed to send response");
                tracing::error!("Error reading message from {}: {}", id, e);
                break;
            }
        }
    }

    tracing::info!("Connection with ID: {} closed", id);
    response_tx.send(ResponseSignal::Stop).await.expect("Failed to send stop signal");
    let _ = response_handler.await;
}