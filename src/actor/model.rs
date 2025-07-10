use std::{collections::VecDeque, sync::{Arc, Mutex}};

use uuid::Uuid;

pub type Queue = Arc<Mutex<VecDeque<Task>>>;

#[derive(Clone, Debug)]
pub struct TaskRequest {
    pub owner: Uuid,
    pub item: String,
}

pub struct Task {
    pub id: Uuid,
}

#[derive(Clone)]
pub enum Signal {
    TaskAdded,
    Stop,
}