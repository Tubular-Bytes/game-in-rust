use opentelemetry::trace::TracerProvider;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Simple test to verify OTLP connectivity
    println!("Testing OTLP connectivity...");

    // Initialize OpenTelemetry tracer for Jaeger using OTLP gRPC
    let tracer = opentelemetry_otlp::new_exporter()
        .tonic()
        .build_span_exporter()
        .map(|exporter| {
            use opentelemetry::KeyValue;
            use opentelemetry_sdk::Resource;
            use opentelemetry_sdk::trace::Config;
            use opentelemetry_sdk::trace::TracerProvider;

            let resource = Resource::new(vec![
                KeyValue::new("service.name", "otlp-test"),
                KeyValue::new("service.version", "0.1.0"),
            ]);

            TracerProvider::builder()
                .with_config(Config::default().with_resource(resource))
                .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
                .build()
                .tracer("otlp-test")
        })?;

    // Create OpenTelemetry layer for tracing
    let opentelemetry_layer = tracing_opentelemetry::layer().with_tracer(tracer);

    // Create stdout layer for console output
    let stdout_layer = tracing_subscriber::fmt::layer().with_target(true).pretty();

    // Combine layers
    tracing_subscriber::registry()
        .with(stdout_layer)
        .with(opentelemetry_layer)
        .init();

    println!("Creating test span...");

    // Create a simple test span
    let span = tracing::info_span!("test_span", test_field = "test_value");
    let _enter = span.enter();

    tracing::info!("This is a test log message");
    tracing::warn!("This is a test warning");

    drop(_enter);

    println!("Waiting for export...");
    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

    println!("Shutting down...");
    opentelemetry::global::shutdown_tracer_provider();

    println!("Test completed! Check Jaeger UI at http://localhost:16686 for 'otlp-test' service");

    Ok(())
}
