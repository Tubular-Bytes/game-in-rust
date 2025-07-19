// pub mod tracing;

use std::collections::HashMap;

use opentelemetry::trace::SpanContext;

pub fn build_carrier(ctx: &SpanContext) -> HashMap<String, String> {
    let mut carrier = HashMap::new();
    carrier.insert("traceparent".to_string(), ctx.trace_id().to_string());
    carrier
}