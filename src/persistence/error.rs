use std::fmt::Display;

#[derive(Debug)]
pub struct MemoryDBError {
    reason: String,
}

impl PartialEq for MemoryDBError {
    fn eq(&self, other: &Self) -> bool {
        self.reason == other.reason
    }
}

impl MemoryDBError {
    pub fn new(reason: &str) -> Self {
        Self {
            reason: reason.to_string(),
        }
    }
}

impl Display for MemoryDBError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MemoryDB Error: {}", self.reason)
    }
}
