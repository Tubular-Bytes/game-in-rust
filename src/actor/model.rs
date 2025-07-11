use std::{collections::VecDeque, sync::{Arc, Mutex}};

use uuid::Uuid;

pub type Queue = Arc<Mutex<VecDeque<Task>>>;

#[derive(Clone, Debug)]
pub enum TaskKind {
    Build,
    Produce,
    Train,
}

#[derive(Clone, Debug)]
pub struct TaskRequest<T = TaskKind> {
    pub owner: Uuid,
    pub item: String,
    pub kind: T,
}

pub struct Task<T = TaskKind> {
    pub id: Uuid,
    pub kind: T,
}

#[derive(Clone)]
pub enum Signal {
    TaskAdded,
    Stop,
}