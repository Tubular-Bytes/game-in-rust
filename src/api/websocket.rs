use crate::actor;
use crate::actor::model::InternalMessage;
use crate::api;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

pub async fn accept_connection(
    stream: TcpStream,
    tx: tokio::sync::broadcast::Sender<actor::model::InternalMessage>,
) {
    let addr = stream
        .peer_addr()
        .expect("connected streams should have a peer address");
    let id = Uuid::new_v4();

    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
        Ok(stream) => stream,
        Err(e) => {
            tracing::error!("WebSocket handshake failed for address {}: {}", addr, e);
            return;
        }
    };

    tracing::info!("Accepted connection with ID: {}, address: {}", id, addr);

    let (mut write, mut read) = ws_stream.split();
    let (response_tx, mut response_rx) =
        tokio::sync::mpsc::channel::<actor::model::ResponseSignal>(100);

    let response_handler = tokio::spawn(async move {
        while let Some(response) = response_rx.recv().await {
            if let actor::model::ResponseSignal::Stop = response {
                tracing::info!("Stopping response handler for ID: {}", id);
                break;
            }

            tracing::info!("Sending response to {}: {}", id, response);
            write
                .send(Message::Text(format!("{response}").into()))
                .await
                .expect("Failed to send response");
        }
    });

    while let Some(message) = read.next().await {
        match message {
            Ok(msg) => {
                if !msg.is_text() {
                    continue;
                }
                tracing::info!("Received message from {}: {:?}", id, msg);
                let mmsg = match serde_json::from_str::<api::model::ApiRequest>(&msg.to_string()) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::error!("Failed to parse message from {}: {}", id, e);
                        response_tx
                            .send(actor::model::ResponseSignal::Error(e.to_string()))
                            .await
                            .expect("Failed to send error response");
                        continue;
                    }
                };
                if let Err(e) = tx.send(InternalMessage::TaskRequest(actor::model::TaskRequest {
                    owner: id,
                    request_id: mmsg.id.clone(),
                    item: mmsg.params.blueprint.clone(),
                    kind: actor::model::TaskKind::Build, // Default kind, can be modified as needed
                    respond_to: response_tx.clone(),
                })) {
                    tracing::error!("Failed to send task request: {}", e);
                    response_tx
                        .send(actor::model::ResponseSignal::Error(e.to_string()))
                        .await
                        .expect("Failed to send error response");
                    break;
                }
            }
            Err(e) => {
                response_tx
                    .clone()
                    .send(actor::model::ResponseSignal::Error(e.to_string()))
                    .await
                    .expect("Failed to send response");
                tracing::error!("Error reading message from {}: {}", id, e);
                break;
            }
        }
    }

    tracing::info!("Connection with ID: {} closed", id);
    response_tx
        .send(actor::model::ResponseSignal::Stop)
        .await
        .expect("Failed to send stop signal");
    let _ = response_handler.await;
}
