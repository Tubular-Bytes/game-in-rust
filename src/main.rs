use building_game::{
    actor::{dispatcher, model},
    api::websocket
};
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

    let tcp_handler = tokio::spawn(async move {
        tracing::info!("Listening for TCP connections on {}", addr);
        
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(websocket::accept_connection(stream, dispatcher_tx.clone()));
        }
    });


    tokio::signal::ctrl_c()
        .await
        .expect("Failed to listen for Ctrl+C");

    dispatcher.stop().await;
    tcp_handler.abort();

    tracing::info!("All workers have been stopped.");
}