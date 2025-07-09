use std::{collections::VecDeque, sync::{Arc, Mutex}};

use uuid::Uuid;

pub type Queue = Arc<Mutex<VecDeque<Task>>>;

pub struct Task {
    pub id: Uuid,
}

#[derive(Clone)]
pub enum Signal {
    TaskAdded,
    Stop,
}