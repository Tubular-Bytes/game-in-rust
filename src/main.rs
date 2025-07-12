use building_game::{
    actor::{dispatcher, model},
    api::websocket
};
use tokio::task::JoinSet;
use std::env;

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

    tracing::info!("Listening for TCP connections on {}", addr);
    
    let mut handles = JoinSet::new();

    loop {
        tokio::select! {
            Ok((stream, _)) = listener.accept() => {
                tracing::info!("New connection from {}", stream.peer_addr().unwrap());
                let dispatcher_tx = dispatcher_tx.clone();
                handles.spawn(websocket::accept_connection(stream, dispatcher_tx));
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Received Ctrl+C, stopping dispatcher...");
                break;
            }
        }
    }

    dispatcher.stop().await;
    drop(dispatcher_tx);
    handles.join_all().await;

    tracing::info!("All workers have been stopped.");
}