use std::{
    collections::{HashMap, VecDeque},
    fmt::Display,
    sync::{Arc, Mutex},
};

use uuid::Uuid;

use crate::blueprint::model::Value;

pub type Queue = Arc<Mutex<VecDeque<Task>>>;

#[derive(Clone, Debug)]
pub enum TaskKind {
    Build,
    Produce,
    Train,
}

#[derive(Clone, Debug)]
pub enum ResponseSignal {
    Success(String),
    Error(String),
    Stop,
}

impl Display for ResponseSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResponseSignal::Success(msg) => write!(f, "Success: {msg}"),
            ResponseSignal::Error(msg) => write!(f, "Error: {msg}"),
            ResponseSignal::Stop => write!(f, "Stop"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct TaskRequest<T = TaskKind> {
    pub owner: Uuid,
    pub request_id: String,
    pub item: String,
    pub kind: T,
    pub respond_to: tokio::sync::mpsc::Sender<ResponseSignal>,
}

pub struct Task<T = TaskKind> {
    pub id: Uuid,
    pub request_id: String,
    pub kind: T,
    pub respond_to: tokio::sync::mpsc::Sender<ResponseSignal>,
}

#[derive(Clone, Debug)]
pub enum InternalMessage {
    TaskRequest(TaskRequest),
    TaskResponse(ResponseSignal),
    AddInventory(Uuid),
    RemoveInventory(Uuid),
    InventoryReserveRequest(HashMap<String, Value<u64>>),
    InventoryReserveResponse(Result<Uuid, String>),
    InventoryReleaseRequest(Uuid),
    InventoryReleaseResponse(Result<Uuid, String>),
    TaskAdded,
    Stop,
    GracefulStop,
}
