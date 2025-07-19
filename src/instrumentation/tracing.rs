use opentelemetry::{global, trace::TracerProvider as _, KeyValue};
use opentelemetry_sdk::trace::Tracer;
use opentelemetry_sdk::{
    runtime,
    trace::{RandomIdGenerator, Sampler, SdkTracerProvider},
    Resource,
};
use opentelemetry_semantic_conventions::{
    attribute::{DEPLOYMENT_ENVIRONMENT_NAME, SERVICE_NAME, SERVICE_VERSION},
    SCHEMA_URL,
};
use tracing_core::Level;
use tracing_opentelemetry::{MetricsLayer, OpenTelemetryLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

fn resource() -> Resource {
    Resource::builder()
        .with_schema_url(
            [
                KeyValue::new(SERVICE_NAME, "richomens"),
                KeyValue::new(SERVICE_VERSION, "0.1.0"),
                KeyValue::new(DEPLOYMENT_ENVIRONMENT_NAME, "development"),
            ],
            SCHEMA_URL,
        )
        .build()
}

// Construct Tracer for OpenTelemetryLayer
fn init_tracer() -> Result<Tracer, anyhow::Error> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .build()?;

    let provider = SdkTracerProvider::builder()
        .with_sampler(Sampler::AlwaysOn)
        .with_id_generator(RandomIdGenerator::default())
        .with_resource(resource())
        .with_batch_exporter(exporter)
        .build();

    global::set_tracer_provider(provider.clone());
    Ok(provider.tracer("tracing-otel-subscriber"))
}

pub fn init_tracing_subscriber() -> Result<(), anyhow::Error> {
    let tracer = init_tracer()?;

    tracing_subscriber::registry()
        .with(tracing_subscriber::filter::LevelFilter::from_level(
            Level::INFO,
        ))
        .with(tracing_subscriber::fmt::layer())
        .with(OpenTelemetryLayer::new(tracer))
        .init();

    Ok(())
}