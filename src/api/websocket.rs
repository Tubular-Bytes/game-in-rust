use crate::actor::{self, model::WebsocketMessage};
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
    tx: tokio::sync::mpsc::Sender<actor::model::Message>,
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
    let (inv_tx, inv_rx) = tokio::sync::oneshot::channel::<Result<String, String>>();
    let mut msg = actor::model::Message::from(WebsocketMessage::AddInventory(id));
    msg.reply = Some(inv_tx);

    if let Err(e) = tx.send(msg).await {
        tracing::error!("Failed to send AddInventory message to dispatcher: {}", e);
        return;
    }
    let response = inv_rx
        .await
        .unwrap_or_else(|_| Err("Failed to receive response".to_string()));

    if let Err(e) = response {
        tracing::error!("Failed to add inventory for ID {}: {}", id, e);
        return;
    } else {
        tracing::info!("Inventory added for ID: {}", id);
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

                let (task_tx, task_rx) = tokio::sync::oneshot::channel::<Result<String, String>>();
                let mut msg = actor::model::Message::from(WebsocketMessage::TaskRequest(
                    actor::model::TaskRequest {
                        owner: id,
                        request_id: mmsg.id.clone(),
                        item: mmsg.params.blueprint.clone(),
                        kind: actor::model::TaskKind::Build, // Default kind, can be modified as needed
                        respond_to: response_tx.clone(),
                    },
                ));
                msg.reply = Some(task_tx);
                if let Err(e) = tx.send(msg).await {
                    tracing::error!("Failed to send TaskRequest message to dispatcher: {}", e);
                    response_tx
                        .send(actor::model::ResponseSignal::Error(e.to_string()))
                        .await
                        .expect("Failed to send error response");
                    break;
                }
                match task_rx
                    .await
                    .unwrap_or_else(|_| Err("Failed to receive response".to_string()))
                {
                    Ok(response) => {
                        response_tx
                            .send(actor::model::ResponseSignal::Success(response))
                            .await
                            .expect("Failed to send success response");
                    }
                    Err(e) => {
                        response_tx
                            .send(actor::model::ResponseSignal::Error(e))
                            .await
                            .expect("Failed to send error response");
                    }
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

    let (remove_tx, remove_rx) = tokio::sync::oneshot::channel::<Result<String, String>>();
    let mut msg = actor::model::Message::from(WebsocketMessage::RemoveInventory(id));
    msg.reply = Some(remove_tx);
    if let Err(e) = tx.send(msg).await {
        tracing::error!(
            "Failed to send RemoveInventory message to dispatcher: {}",
            e
        );
    }

    let response = remove_rx
        .await
        .unwrap_or_else(|_| Err("Failed to receive response".to_string()));
    if let Err(e) = response {
        tracing::error!("Failed to remove inventory for ID {}: {}", id, e);
    } else {
        tracing::info!("Inventory removed for ID: {}", id);
    }
}
