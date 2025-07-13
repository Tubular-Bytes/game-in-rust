use std::fmt;

#[derive(Debug)]
pub struct ProcessTaskError {
    pub message: String,
}

impl fmt::Display for ProcessTaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "error processing task: {}", self.message)
    }
}

impl std::error::Error for ProcessTaskError {}
