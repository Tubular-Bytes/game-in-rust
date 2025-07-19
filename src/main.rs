use building_game::{
    actor::{broker, dispatcher},
    api::websocket,
};
use opentelemetry_sdk::{propagation::TraceContextPropagator, Resource};
use std::env;
use tokio::task::JoinSet;

use opentelemetry::{global, KeyValue};

const WORKER_COUNT: u8 = 2; // Number of worker threads

#[tokio::main]
async fn main() {
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .pretty()
        .finish();

    // Initialize OTLP exporter using HTTP binary protocol
    let otlp_exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .build()
        .unwrap();

    let resource = Resource::builder()
        .with_service_name("building_game")
        .with_attributes([
            KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
        ])
        .build();
    // Create a tracer provider with the exporter
    let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(otlp_exporter)
        .with_resource(resource)
        .build();

    // Set it as the global provider
    global::set_tracer_provider(tracer_provider);
    global::set_text_map_propagator(TraceContextPropagator::new());

    tracing::subscriber::set_global_default(subscriber).expect("Failed to set global subscriber");
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
