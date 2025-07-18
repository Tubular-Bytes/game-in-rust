use crate::actor;
use crate::actor::model::InternalMessage;
use crate::api;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{
    Message,
    handshake::server::{Request, Response},
};

use uuid::Uuid;

pub async fn accept_connection(
    stream: TcpStream,
    tx: tokio::sync::broadcast::Sender<actor::model::InternalMessage>,
) {
    let addr = stream
        .peer_addr()
        .expect("connected streams should have a peer address");
    let mut id = Uuid::new_v4();

    let callback = |req: &Request, response: Response| {
        if let Some(id_header) = req.headers().get("Authorization") {
            if let Ok(id_str) = id_header.to_str() {
                if let Ok(parsed_id) = Uuid::parse_str(id_str) {
                    id = parsed_id;
                } else {
                    tracing::warn!("Invalid UUID in Authorization header: {}", id_str);
                }
            } else {
                tracing::warn!("Failed to convert Authorization header to string");
            }
        }

        Ok(response)
    };

    let ws_stream = match tokio_tungstenite::accept_hdr_async(stream, callback).await {
        Ok(stream) => stream,
        Err(e) => {
            tracing::error!("WebSocket handshake failed for address {}: {}", addr, e);
            return;
        }
    };

    tracing::debug!("Accepted connection with ID: {}, address: {}", id, addr);
    if let Err(e) = tx.send(InternalMessage::AddInventory(id)) {
        tracing::error!("Failed to send AddInventory message: {}", e);
        return;
    }

    let (mut write, mut read) = ws_stream.split();
    let (response_tx, mut response_rx) =
        tokio::sync::mpsc::channel::<actor::model::ResponseSignal>(100);

    let stop_handle = tokio::spawn(async move {
        while let Some(response) = response_rx.recv().await {
            if let actor::model::ResponseSignal::Stop = response {
                tracing::debug!("Stopping response handler for ID: {}", id);
                break;
            }

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

    stop_handle.abort();
    response_tx
        .send(actor::model::ResponseSignal::Stop)
        .await
        .expect("Failed to send stop signal");
    if let Err(e) = tx.send(InternalMessage::RemoveInventory(id)) {
        tracing::error!("Failed to send RemoveInventory message: {}", e);
    }
}
