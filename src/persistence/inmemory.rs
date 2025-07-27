use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
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
    #[tracing::instrument(skip(self), fields(key = %key))]
    fn set(&self, key: String, value: String) -> Result<(), MemoryDBError> {
        tracing::debug!(
            "Setting value in memory database, value_size: {}",
            value.len()
        );
        let mut db = self.db.write().map_err(|_| {
            tracing::error!("Failed to acquire write lock for key: {}", key);
            MemoryDBError::new("Failed to acquire write lock")
        })?;
        db.insert(key.clone(), value);
        tracing::debug!("Successfully set value for key: {}", key);
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(key = %key))]
    fn delete(&self, key: String) -> Result<(), MemoryDBError> {
        tracing::debug!("Deleting value from memory database");
        let mut db = self.db.write().map_err(|_| {
            tracing::error!("Failed to acquire write lock for key: {}", key);
            MemoryDBError::new("Failed to acquire write lock")
        })?;
        let existed = db.remove(&key).is_some();
        if existed {
            tracing::debug!("Successfully deleted key: {}", key);
        } else {
            tracing::debug!("Key did not exist for deletion: {}", key);
        }
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(key = %key))]
    fn get(&self, key: String) -> Result<String, MemoryDBError> {
        tracing::debug!("Getting value from memory database");
        let db = self.db.read().map_err(|_| {
            tracing::error!("Failed to acquire read lock for key: {}", key);
            MemoryDBError::new("Failed to acquire read lock")
        })?;
        match db.get(&key) {
            Some(value) => {
                tracing::debug!(
                    "Successfully retrieved value for key: {}, value_size: {}",
                    key,
                    value.len()
                );
                Ok(value.clone())
            }
            None => {
                tracing::debug!("Key not found: {}", key);
                Err(MemoryDBError::new("Key not found"))
            }
        }
    }
}
