use std::vec;

use building_game::{
    blueprint::model::Value,
    persistence::{
        inmemory::MemoryDatabase,
        worker::{Op, OpType, PersistenceWorker, Persister},
    },
};
use opentelemetry::{
    global,
    trace::{Span, SpanContext, TraceContextExt, Tracer, TracerProvider}, Context,
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

#[tokio::main]
async fn main() {
    // Initialize OpenTelemetry tracer for Jaeger using OTLP
    let exporter = opentelemetry_otlp::new_exporter()
        .tonic()
        .build_span_exporter()
        .expect("Failed to create OTLP exporter");

    let resource = opentelemetry_sdk::Resource::new(vec![
        opentelemetry::KeyValue::new("service.name", "persistence-demo"),
        opentelemetry::KeyValue::new("service.version", "0.1.0"),
    ]);

    let tracer_provider = opentelemetry_sdk::trace::TracerProvider::builder()
        .with_config(opentelemetry_sdk::trace::config().with_resource(resource))
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
        .build();

    // Get the tracer for use with tracing-opentelemetry layer
    let tracer = tracer_provider.tracer("persistence-demo");

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

    tracing::info!("Starting persistence demo with enhanced tracing...");
    tracing::info!("Tracing enabled for inventory and persistence modules");
    tracing::info!("OTLP endpoint: http://localhost:4318/v1/traces (default)");

    let db = MemoryDatabase::new();

    let broker = building_game::actor::broker::Broker::new();
    let (store_tx, store_rx) = tokio::sync::mpsc::channel(100);

    let tracer = global::tracer("persistence-demo");

    let mut span = tracer.start("persistence-demo.main");

    let cx = span.span_context().clone();

    tracing::info!(
        "span.context.span_id" = cx.span_id().to_string(),
        "span.context.trace_id" = cx.trace_id().to_string(),
        "span.context.is_remote" = cx.is_remote().to_string(),
        // "span.context.trace_flags" = cx.trace_flags(),
        // "span.context.trace_state" = cx.trace_state().to_string(),
        
        "span context created"
    );

    // Create example inventory data and store it in the database
    span.add_event("creating example inventory data", vec![]);
    let existing_inventory = example_inventory_data(&cx, &store_tx);

    span.set_attribute(opentelemetry::KeyValue::new(
        "inventory.id",
        existing_inventory.id.to_string(),
    ));

    span.add_event("store example data", vec![
        opentelemetry::KeyValue::new("inventory.id", existing_inventory.id.to_string()),
    ]);

    db.set(
        Some(cx),
        format!("inventory:{}", existing_inventory.id.clone()),
        existing_inventory.serialize().unwrap(),
    )
    .unwrap();

    let mut persistence = PersistenceWorker::new(Box::new(db), store_rx);

    let persistence_handle = tokio::spawn(async move {
        persistence.run().await;
    });

    // Create a new inventory instance that will restore from persistence
    span.add_event(
        "creating new inventory instance",
        vec![opentelemetry::KeyValue::new(
            "inventory.id",
            existing_inventory.id.to_string(),
        )],
    );

    let _inventory = building_game::actor::inventory::Inventory::new(
        existing_inventory.id.clone(),
        broker.clone().topic("inventory").sender.clone(),
        &store_tx.clone(),
    );

    // Give enough time for the inventory to restore its data
    tracing::info!("Waiting for inventory restoration to complete...");
    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
    tracing::info!("Proceeding to shutdown...");

    let context = span.span_context().clone();

    store_tx
        .send(Op {
            op_type: OpType::Stop,
            reply: None,
            span_context: Some(context),
        })
        .await
        .unwrap();

    persistence_handle.await.unwrap();

    // End the main span before shutdown
    span.end();

    tracing::info!("Demo completed successfully!");

    // Give time for spans to be exported before shutdown
    tracing::info!("Waiting for span export to complete...");
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Shutdown OpenTelemetry to flush remaining spans
    opentelemetry::global::shutdown_tracer_provider();
    tracing::info!("OpenTelemetry tracer shutdown complete.");
}

fn example_inventory_data(
    cx: &SpanContext,
    persistence_tx: &tokio::sync::mpsc::Sender<building_game::persistence::worker::Op>,
) -> building_game::actor::inventory::Inventory {
    let tracer = global::tracer("persistence-demo.example_inventory_data");
    let context = Context::current().with_remote_span_context(cx.clone());
    let mut span = tracer.start_with_context("persistence-demo.example", &context);

    tracing::info!(
        "span.context.span_id" = span.span_context().span_id().to_string(),
        "span.context.trace_id" = span.span_context().trace_id().to_string(),
        "span.context.is_remote" = span.span_context().is_remote().to_string(),
        // "span.context.trace_flags" = span.span_context().trace_flags(),
        // "span.context.trace_state" = span.span_context().trace_state().to_string(),
        
        "span context created"
    );

    let id = Uuid::new_v4();
    span.add_event("creating example inventory", vec![
        opentelemetry::KeyValue::new("inventory.id", id.to_string()),
    ]);

    let broker = building_game::actor::broker::Broker::new();

    let inv = building_game::actor::inventory::Inventory::new(
        id,
        broker.topic("inventory").sender.clone(),
        persistence_tx,
    );

    span.add_event("adding resource to inventory", vec![
        opentelemetry::KeyValue::new("inventory.id", id.to_string()),
        opentelemetry::KeyValue::new("resource.name", "wood".to_string()),
        opentelemetry::KeyValue::new("resource.value", 100.to_string()),
        ]);
    inv.resources.lock().unwrap().insert(
        "wood".to_string(),
        Value {
            name: "wood".to_string(),
            value: 100,
        },
    );

    tracing::info!("Example inventory data created successfully");
    
    // End the span before returning
    span.end();
    
    return inv;
}
