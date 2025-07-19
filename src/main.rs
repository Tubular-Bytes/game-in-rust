use building_game::{
    actor::{broker, dispatcher},
    api::websocket,
    instrumentation,
};
use std::env;
use tokio::task::JoinSet;

const WORKER_COUNT: u8 = 2; // Number of worker threads

#[tokio::main]
async fn main() {
    instrumentation::tracing::init_tracing_subscriber()
        .expect("Failed to initialize tracing subscriber");

    tracing::info!("Starting the application...");

    let broker = broker::Broker::new();
    let mut dispatcher = dispatcher::Dispatcher::new(broker);
    let broker_tx = dispatcher.topic().clone();

    dispatcher.start(WORKER_COUNT).await;

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
                tracing::debug!("New connection from {}", stream.peer_addr().unwrap());
                let dispatcher_tx = broker_tx.clone();
                handles.spawn(websocket::accept_connection(stream, dispatcher_tx));
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::debug!("Received Ctrl+C, initiating graceful shutdown...");
                break;
            }
        }
    }

    // Stop accepting new connections
    drop(listener);

    // Stop the dispatcher gracefully (this will wait for all tasks to complete)
    tracing::debug!("Stopping dispatcher and waiting for all tasks to complete...");
    dispatcher.stop().await;
    tracing::debug!("Dispatcher stopped successfully.");

    // Close the broadcast channel to signal no more tasks
    drop(broker_tx);
    tracing::debug!("Broadcast channel closed.");

    // Wait for all WebSocket connections to close (with timeout)
    tracing::debug!("Waiting for all WebSocket connections to close...");

    // Set a timeout for WebSocket connections to close gracefully
    let timeout_duration = tokio::time::Duration::from_secs(5);
    let start_time = tokio::time::Instant::now();

    // Wait for handles to complete or timeout
    loop {
        if handles.is_empty() {
            tracing::debug!("All WebSocket connections closed gracefully.");
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

    tracing::debug!("All workers have been stopped.");
    tracing::info!("Shutting down daemon...");
}
