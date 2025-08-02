use crate::actor::{self, model::WebsocketMessage};
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

    let ws_stream = match establish_websocket_connection(stream, &mut id, addr).await {
        Some(stream) => stream,
        None => return,
    };

    if !setup_inventory(&tx, id).await {
        return;
    }

    handle_websocket_session(ws_stream, &tx, id, addr).await;
    cleanup_inventory(&tx, id).await;
}

async fn establish_websocket_connection(
    stream: TcpStream,
    id: &mut Uuid,
    addr: std::net::SocketAddr,
) -> Option<tokio_tungstenite::WebSocketStream<TcpStream>> {
    let callback = |req: &Request, response: Response| {
        extract_id_from_request(req, id);
        Ok(response)
    };

    match tokio_tungstenite::accept_hdr_async(stream, callback).await {
        Ok(stream) => {
            tracing::debug!("Accepted connection with ID: {}, address: {}", id, addr);
            Some(stream)
        }
        Err(e) => {
            tracing::error!("WebSocket handshake failed for address {}: {}", addr, e);
            None
        }
    }
}

fn extract_id_from_request(req: &Request, id: &mut Uuid) {
    if let Some(id_header) = req.headers().get("Authorization") {
        if let Ok(id_str) = id_header.to_str() {
            if let Ok(parsed_id) = Uuid::parse_str(id_str) {
                *id = parsed_id;
            } else {
                tracing::warn!("Invalid UUID in Authorization header: {}", id_str);
            }
        } else {
            tracing::warn!("Failed to convert Authorization header to string");
        }
    }
}

async fn setup_inventory(tx: &tokio::sync::mpsc::Sender<actor::model::Message>, id: Uuid) -> bool {
    let (inv_tx, inv_rx) = tokio::sync::oneshot::channel::<Result<String, String>>();
    let mut msg = actor::model::Message::from(WebsocketMessage::AddInventory(id));
    msg.reply = Some(inv_tx);

    if let Err(e) = tx.send(msg).await {
        tracing::error!("Failed to send AddInventory message to dispatcher: {}", e);
        return false;
    }

    let response = inv_rx
        .await
        .unwrap_or_else(|_| Err("Failed to receive response".to_string()));

    match response {
        Ok(_) => {
            tracing::info!("Inventory added for ID: {}", id);
            true
        }
        Err(e) => {
            tracing::error!("Failed to add inventory for ID {}: {}", id, e);
            false
        }
    }
}

async fn handle_websocket_session(
    ws_stream: tokio_tungstenite::WebSocketStream<TcpStream>,
    tx: &tokio::sync::mpsc::Sender<actor::model::Message>,
    id: Uuid,
    _addr: std::net::SocketAddr,
) {
    let (write, mut read) = ws_stream.split();
    let (response_tx, response_rx) =
        tokio::sync::mpsc::channel::<actor::model::ResponseSignal>(100);

    let stop_handle = spawn_response_handler(write, response_rx, id).await;

    process_incoming_messages(&mut read, tx, &response_tx, id).await;

    stop_handle.abort();
    let _ = response_tx.send(actor::model::ResponseSignal::Stop).await;
}

async fn spawn_response_handler(
    mut write: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<TcpStream>,
        tokio_tungstenite::tungstenite::Message,
    >,
    mut response_rx: tokio::sync::mpsc::Receiver<actor::model::ResponseSignal>,
    id: Uuid,
) -> tokio::task::JoinHandle<()> {
    let (write_tx, mut write_rx) = tokio::sync::mpsc::channel::<actor::model::ResponseSignal>(100);

    // Forward responses to the write channel
    let _forward_handle = {
        let write_tx = write_tx.clone();
        tokio::spawn(async move {
            while let Some(response) = response_rx.recv().await {
                if let actor::model::ResponseSignal::Stop = response {
                    tracing::debug!("Stopping response handler for ID: {}", id);
                    let _ = write_tx.send(response).await;
                    break;
                }
                let _ = write_tx.send(response).await;
            }
        })
    };

    tokio::spawn(async move {
        while let Some(response) = write_rx.recv().await {
            if let actor::model::ResponseSignal::Stop = response {
                break;
            }

            if write
                .send(Message::Text(format!("{response}").into()))
                .await
                .is_err()
            {
                break;
            }
        }
    })
}

async fn process_incoming_messages(
    read: &mut futures_util::stream::SplitStream<tokio_tungstenite::WebSocketStream<TcpStream>>,
    tx: &tokio::sync::mpsc::Sender<actor::model::Message>,
    response_tx: &tokio::sync::mpsc::Sender<actor::model::ResponseSignal>,
    id: Uuid,
) {
    while let Some(message) = read.next().await {
        match message {
            Ok(msg) => {
                if !msg.is_text() {
                    continue;
                }

                if !handle_text_message(msg, tx, response_tx, id).await {
                    break;
                }
            }
            Err(e) => {
                let _ = response_tx
                    .send(actor::model::ResponseSignal::Error(e.to_string()))
                    .await;
                tracing::error!("Error reading message from {}: {}", id, e);
                break;
            }
        }
    }
}

async fn handle_text_message(
    msg: Message,
    tx: &tokio::sync::mpsc::Sender<actor::model::Message>,
    response_tx: &tokio::sync::mpsc::Sender<actor::model::ResponseSignal>,
    id: Uuid,
) -> bool {
    let api_request = match parse_api_request(&msg, response_tx, id).await {
        Some(request) => request,
        None => return true, // Continue on parse error
    };

    let (task_tx, task_rx) = tokio::sync::oneshot::channel::<Result<String, String>>();
    let mut msg =
        actor::model::Message::from(WebsocketMessage::TaskRequest(actor::model::TaskRequest {
            owner: id,
            request_id: api_request.id.clone(),
            kind: actor::model::TaskKind::Build(api_request.params.blueprint.clone()),
            respond_to: response_tx.clone(),
        }));
    msg.reply = Some(task_tx);

    if let Err(e) = tx.send(msg).await {
        tracing::error!("Failed to send TaskRequest message to dispatcher: {}", e);
        let _ = response_tx
            .send(actor::model::ResponseSignal::Error(e.to_string()))
            .await;
        return false;
    }

    handle_task_response(task_rx, response_tx).await
}

async fn parse_api_request(
    msg: &Message,
    response_tx: &tokio::sync::mpsc::Sender<actor::model::ResponseSignal>,
    id: Uuid,
) -> Option<crate::api::model::ApiRequest> {
    match serde_json::from_str::<crate::api::model::ApiRequest>(&msg.to_string()) {
        Ok(request) => Some(request),
        Err(e) => {
            tracing::error!("Failed to parse message from {}: {}", id, e);
            let _ = response_tx
                .send(actor::model::ResponseSignal::Error(e.to_string()))
                .await;
            None
        }
    }
}

async fn handle_task_response(
    task_rx: tokio::sync::oneshot::Receiver<Result<String, String>>,
    response_tx: &tokio::sync::mpsc::Sender<actor::model::ResponseSignal>,
) -> bool {
    match task_rx
        .await
        .unwrap_or_else(|_| Err("Failed to receive response".to_string()))
    {
        Ok(response) => {
            let _ = response_tx
                .send(actor::model::ResponseSignal::Success(response))
                .await;
            true
        }
        Err(e) => {
            let _ = response_tx
                .send(actor::model::ResponseSignal::Error(e))
                .await;
            true
        }
    }
}

async fn cleanup_inventory(tx: &tokio::sync::mpsc::Sender<actor::model::Message>, id: Uuid) {
    let (remove_tx, remove_rx) = tokio::sync::oneshot::channel::<Result<String, String>>();
    let mut msg = actor::model::Message::from(WebsocketMessage::RemoveInventory(id));
    msg.reply = Some(remove_tx);

    if let Err(e) = tx.send(msg).await {
        tracing::error!(
            "Failed to send RemoveInventory message to dispatcher: {}",
            e
        );
        return;
    }

    let response = remove_rx
        .await
        .unwrap_or_else(|_| Err("Failed to receive response".to_string()));

    match response {
        Ok(_) => tracing::info!("Inventory removed for ID: {}", id),
        Err(e) => tracing::error!("Failed to remove inventory for ID {}: {}", id, e),
    }
}
