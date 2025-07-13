use building_game::{
    actor::{dispatcher, model},
    api::websocket,
};
use std::env;
use tokio::task::JoinSet;

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

    let addr = env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:9100".to_string());

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
                tracing::info!("Received Ctrl+C, initiating graceful shutdown...");
                break;
            }
        }
    }

    // Stop accepting new connections
    drop(listener);

    // Stop the dispatcher gracefully (this will wait for all tasks to complete)
    tracing::info!("Stopping dispatcher and waiting for all tasks to complete...");
    dispatcher.stop().await;
    tracing::info!("Dispatcher stopped successfully.");

    // Close the broadcast channel to signal no more tasks
    drop(dispatcher_tx);
    tracing::info!("Broadcast channel closed.");

    // Wait for all WebSocket connections to close (with timeout)
    tracing::info!("Waiting for all WebSocket connections to close...");

    // Set a timeout for WebSocket connections to close gracefully
    let timeout_duration = tokio::time::Duration::from_secs(5);
    let start_time = tokio::time::Instant::now();

    // Wait for handles to complete or timeout
    loop {
        if handles.is_empty() {
            tracing::info!("All WebSocket connections closed gracefully.");
            break;
        }

        if start_time.elapsed() > timeout_duration {
            tracing::warn!("Timeout waiting for WebSocket connections to close. Forcing shutdown.");
            handles.abort_all();
            break;
        }

        // Try to join the next handle with a short timeout
        if let Ok(Some(_)) =
            tokio::time::timeout(tokio::time::Duration::from_millis(100), handles.join_next()).await
        {
            // A handle completed successfully
        }
    }

    tracing::info!("All workers have been stopped.");
}
