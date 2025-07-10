use building_game::actor::{dispatcher, model};
use tokio::net::TcpStream;
use uuid::Uuid;
use std::env;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() {
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .pretty()
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("Failed to set global subscriber");
    tracing::info!("Starting the application...");

    let (dispatcher_tx, _) = tokio::sync::broadcast::channel::<model::TaskRequest>(100);
    let mut dispatcher = dispatcher::Dispatcher::new(dispatcher_tx.clone());

    dispatcher.start(2).await;

    
    let addr = env::args().nth(1).unwrap_or_else(|| "127.0.0.1:9100".to_string());

    let try_socket = tokio::net::TcpListener::bind(&addr).await;
    let listener = match try_socket {
        Ok(socket) => socket,
        Err(e) => {
            tracing::error!("Failed to bind to address {}: {}", addr, e);
            return;
        }
    };

    let tcp_handler = tokio::spawn(async move {
        tracing::info!("Listening for TCP connections on {}", addr);
        
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(accept_connection(stream, dispatcher_tx.clone()));
        }
    });


    tokio::signal::ctrl_c()
        .await
        .expect("Failed to listen for Ctrl+C");

    dispatcher.stop().await;
    tcp_handler.abort();

    tracing::info!("All workers have been stopped.");
}

async fn accept_connection(stream: TcpStream, tx: tokio::sync::broadcast::Sender<model::TaskRequest>) {
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