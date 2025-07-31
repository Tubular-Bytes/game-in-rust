use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use opentelemetry::{
    Context, KeyValue, global,
    trace::{Span, Status, TraceContextExt, Tracer},
};

use crate::persistence::{error::MemoryDBError, worker::Persister};

type MemoryDB = Arc<RwLock<HashMap<String, String>>>;

#[derive(Clone)]
pub struct MemoryDatabase {
    db: MemoryDB,
}

impl MemoryDatabase {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for MemoryDatabase {
    fn default() -> Self {
        Self {
            db: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Persister for MemoryDatabase {
    fn set(
        &self,
        cx: Option<opentelemetry::trace::SpanContext>,
        key: String,
        value: String,
    ) -> Result<(), MemoryDBError> {
        let tracer = global::tracer("persistence.inmemory");
        let parent_context = cx.unwrap_or_else(opentelemetry::trace::SpanContext::empty_context);
        let context = Context::current().with_remote_span_context(parent_context.clone());
        let mut span = tracer.start_with_context("persistence.inmemory.set", &context);

        span.add_event(
            "setting value in memory database",
            vec![
                KeyValue::new("key", key.clone()),
                KeyValue::new("value.size", value.len() as i64),
            ],
        );
        let mut db = self.db.write().map_err(|_| {
            span.add_event(
                "failed to acquire write lock for key",
                vec![KeyValue::new("key", key.clone())],
            );
            span.set_status(Status::error("failed to acquire write lock"));
            span.end();
            MemoryDBError::new("Failed to acquire write lock")
        })?;
        db.insert(key.clone(), value);
        span.set_status(Status::Ok);
        span.end();
        Ok(())
    }

    fn delete(
        &self,
        cx: Option<opentelemetry::trace::SpanContext>,
        key: String,
    ) -> Result<(), MemoryDBError> {
        let tracer = global::tracer("persistence.inmemory");
        let parent_context = cx.unwrap_or_else(opentelemetry::trace::SpanContext::empty_context);
        let context = Context::current().with_remote_span_context(parent_context.clone());
        let mut span = tracer.start_with_context("persistence.inmemory.set", &context);

        let mut db = self.db.write().map_err(|_| {
            span.add_event(
                "failed to acquire write lock for key",
                vec![KeyValue::new("key", key.clone())],
            );
            span.set_status(Status::error("failed to acquire write lock"));
            span.end();
            MemoryDBError::new("Failed to acquire write lock")
        })?;
        let existed = db.remove(&key).is_some();
        span.set_attribute(KeyValue::new("key.deleted", existed));

        span.set_status(Status::Ok);
        span.end();
        Ok(())
    }

    fn get(
        &self,
        cx: Option<opentelemetry::trace::SpanContext>,
        key: String,
    ) -> Result<String, MemoryDBError> {
        let tracer = global::tracer("persistence.inmemory");
        let parent_context = cx.unwrap_or_else(opentelemetry::trace::SpanContext::empty_context);
        let context = Context::current().with_remote_span_context(parent_context.clone());
        let mut span = tracer.start_with_context("persistence.inmemory.set", &context);

        let db = self.db.read().map_err(|_| {
            span.add_event(
                "failed to acquire read lock for key",
                vec![KeyValue::new("key", key.clone())],
            );
            span.set_status(Status::error("failed to acquire read lock"));
            span.end();
            MemoryDBError::new("Failed to acquire read lock")
        })?;

        tracing::info!("keys in memory database: {:?}", db.keys());

        match db.get(&key) {
            Some(value) => {
                span.add_event(
                    "retrieved value from memory database",
                    vec![
                        KeyValue::new("key", key.clone()),
                        KeyValue::new("value.size", value.len() as i64),
                    ],
                );
                span.set_status(Status::Ok);
                Ok(value.clone())
            }
            None => {
                span.add_event(
                    "key not found in memory database",
                    vec![KeyValue::new("key", key.clone())],
                );
                span.set_status(Status::error("key not found"));
                span.end();
                Err(MemoryDBError::new("Key not found"))
            }
        }
    }
}
