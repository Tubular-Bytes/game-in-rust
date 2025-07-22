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
    fn set(&self, key: String, value: String) -> Result<(), MemoryDBError> {
        let mut db = self
            .db
            .write()
            .map_err(|_| MemoryDBError::new("Failed to acquire write lock"))?;
        db.insert(key, value);
        Ok(())
    }

    fn delete(&self, key: String) -> Result<(), MemoryDBError> {
        let mut db = self
            .db
            .write()
            .map_err(|_| MemoryDBError::new("Failed to acquire write lock"))?;
        db.remove(&key);
        Ok(())
    }

    fn get(&self, key: String) -> Result<String, MemoryDBError> {
        let db = self
            .db
            .read()
            .map_err(|_| MemoryDBError::new("Failed to acquire read lock"))?;
        match db.get(&key) {
            Some(value) => Ok(value.clone()),
            None => Err(MemoryDBError::new("Key not found")),
        }
    }
}
