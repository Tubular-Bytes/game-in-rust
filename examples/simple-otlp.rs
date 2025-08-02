use opentelemetry::{
    KeyValue,
    trace::{Span, Tracer},
};
use opentelemetry_sdk::{
    Resource,
    trace::{self, TracerProvider as SdkTracerProvider},
};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Testing simple OTLP export...");

    // Create OTLP exporter
    let exporter = opentelemetry_otlp::new_exporter()
        .tonic()
        .build_span_exporter()?;

    // Create resource with proper service identification
    let resource = Resource::new(vec![
        KeyValue::new("service.name", "simple-test"),
        KeyValue::new("service.version", "1.0.0"),
    ]);

    // Create tracer provider and set it globally
    let tracer_provider = SdkTracerProvider::builder()
        .with_config(trace::config().with_resource(resource))
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
        .build();

    // Set the global tracer provider
    opentelemetry::global::set_tracer_provider(tracer_provider);

    // Get a tracer
    let tracer = opentelemetry::global::tracer("simple-test");

    // Create a span directly using OpenTelemetry API
    println!("Creating span with OpenTelemetry API...");
    let mut span = tracer
        .span_builder("test_operation")
        .with_attributes(vec![
            KeyValue::new("operation.type", "test"),
            KeyValue::new("test.value", 42),
        ])
        .start(&tracer);

    // Add an event to the span
    span.add_event("Processing started", vec![]);

    // Simulate some work
    tokio::time::sleep(Duration::from_millis(100)).await;

    span.add_event("Processing completed", vec![]);
    span.end();

    println!("Span created and ended. Flushing...");

    // Give it time to export
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Shutdown
    println!("Shutting down tracer provider...");
    opentelemetry::global::shutdown_tracer_provider();

    println!("Test completed! Check Jaeger UI for 'simple-test' service");

    Ok(())
}
