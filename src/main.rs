use building_game::{
    actor::{broker, dispatcher},
    api::websocket,
    persistence,
};
use opentelemetry::trace::TracerProvider;
use std::env;
use tokio::task::JoinSet;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() {
    setup_tracing().await;

    let (persistence_handle, store_tx) = start_persistence_worker().await;
    let (dispatcher_handle, shutdown_tx, ws_tx) = start_dispatcher(&store_tx).await;

    let listener = setup_tcp_listener().await;
    let mut handles = JoinSet::new();

    run_server_loop(listener, &ws_tx, &mut handles).await;

    shutdown_services(shutdown_tx, dispatcher_handle, store_tx, persistence_handle).await;
    shutdown_websocket_connections(handles).await;

    finalize_shutdown().await;
}

async fn setup_tracing() {
    // Initialize OpenTelemetry tracer for Jaeger using OTLP
    let exporter = opentelemetry_otlp::new_exporter()
        .tonic()
        .build_span_exporter()
        .expect("Failed to create OTLP exporter");

    let resource = opentelemetry_sdk::Resource::new(vec![
        opentelemetry::KeyValue::new("service.name", "game-in-rust"),
        opentelemetry::KeyValue::new("service.version", "0.1.0"),
    ]);

    let tracer_provider = opentelemetry_sdk::trace::TracerProvider::builder()
        .with_config(opentelemetry_sdk::trace::config().with_resource(resource))
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
        .build();

    // Get the tracer for use with tracing-opentelemetry layer
    let tracer = tracer_provider.tracer("game-in-rust");

    // Set the global tracer provider for direct OpenTelemetry usage
    opentelemetry::global::set_tracer_provider(tracer_provider);

    // Create OpenTelemetry layer for tracing
    let opentelemetry_layer = tracing_opentelemetry::layer().with_tracer(tracer);

    // Create filter specifically for inventory and persistence modules with debug level
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(
            "building_game::actor::inventory=debug,building_game::persistence=debug,info",
        )
    });

    // Create stdout layer for console output with structured formatting
    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE);

    // Combine layers
    tracing_subscriber::registry()
        .with(filter)
        .with(stdout_layer)
        .with(opentelemetry_layer)
        .init();
}

async fn start_persistence_worker() -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::mpsc::Sender<persistence::worker::Op>,
) {
    let (store_tx, store_rx) = tokio::sync::mpsc::channel(100);
    let mut persistence = persistence::worker::PersistenceWorker::new(
        Box::new(persistence::inmemory::MemoryDatabase::new()),
        store_rx,
    );

    let persistence_handle = tokio::spawn(async move {
        persistence.run().await;
    });

    (persistence_handle, store_tx)
}

async fn start_dispatcher(
    store_tx: &tokio::sync::mpsc::Sender<persistence::worker::Op>,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::mpsc::Sender<building_game::actor::model::Message>,
) {
    let broker = broker::Broker::new();
    let (ws_tx, ws_rx) = tokio::sync::mpsc::channel(100);
    let mut dispatcher = dispatcher::Dispatcher::new(&broker, ws_rx, store_tx);

    // Create a shutdown signal channel
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

    let dispatcher_handle = tokio::spawn(async move {
        dispatcher.start_with_shutdown(2, shutdown_rx).await;
    });

    (dispatcher_handle, shutdown_tx, ws_tx)
}

async fn setup_tcp_listener() -> tokio::net::TcpListener {
    let addr = env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:9100".to_string());

    let try_socket = tokio::net::TcpListener::bind(&addr).await;
    let listener = match try_socket {
        Ok(socket) => socket,
        Err(e) => {
            tracing::error!("Failed to bind to address {}: {}", addr, e);
            std::process::exit(1);
        }
    };

    tracing::info!("Listening for TCP connections on {}", addr);
    listener
}

async fn run_server_loop(
    listener: tokio::net::TcpListener,
    ws_tx: &tokio::sync::mpsc::Sender<building_game::actor::model::Message>,
    handles: &mut JoinSet<()>,
) {
    loop {
        tokio::select! {
            Ok((stream, _)) = listener.accept() => {
                tracing::debug!("New connection from {}", stream.peer_addr().unwrap());
                handles.spawn(websocket::accept_connection(stream, ws_tx.clone()));
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::debug!("Received Ctrl+C, initiating graceful shutdown...");
                break;
            }
        }
    }

    // Stop accepting new connections
    drop(listener);
}

async fn shutdown_services(
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
    dispatcher_handle: tokio::task::JoinHandle<()>,
    store_tx: tokio::sync::mpsc::Sender<persistence::worker::Op>,
    persistence_handle: tokio::task::JoinHandle<()>,
) {
    // Signal the dispatcher to stop gracefully
    tracing::debug!("Signaling dispatcher to stop...");
    let _ = shutdown_tx.send(());

    // Wait for the dispatcher to stop
    tracing::debug!("Waiting for dispatcher to complete shutdown...");
    match tokio::time::timeout(tokio::time::Duration::from_secs(10), dispatcher_handle).await {
        Ok(_) => tracing::debug!("Dispatcher stopped successfully."),
        Err(_) => tracing::warn!("Dispatcher shutdown timed out."),
    }

    // Stop persistence worker
    tracing::debug!("Signaling persistence worker to stop...");
    let _ = store_tx
        .send(persistence::worker::Op {
            op_type: persistence::worker::OpType::Stop,
            reply: None,
            span_context: None,
        })
        .await;

    tracing::debug!("Waiting for persister to complete shutdown...");
    match tokio::time::timeout(tokio::time::Duration::from_secs(10), persistence_handle).await {
        Ok(_) => tracing::debug!("Persister stopped successfully."),
        Err(_) => tracing::warn!("Persister shutdown timed out."),
    }
}

async fn shutdown_websocket_connections(mut handles: JoinSet<()>) {
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
}

async fn finalize_shutdown() {
    tracing::info!("Shutting down daemon...");

    // Give time for spans to be exported before shutdown
    tracing::debug!("Waiting for span export to complete...");
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Shutdown OpenTelemetry to flush remaining spans
    opentelemetry::global::shutdown_tracer_provider();
    tracing::debug!("OpenTelemetry tracer shutdown complete.");
}
