use std::collections::HashMap;

pub struct Building {
    pub name: String,
    pub description: String,
    pub cost: Option<Cost>,
    pub requirements: Option<HashMap<String, u32>>,
    pub production: Option<Production>,
}

#[derive(Debug, Clone)]
pub struct Value<T> {
    pub name: String,
    pub value: T,
}

pub struct Cost {
    pub resources: Vec<Value<u32>>,
    pub labor: Option<Vec<Value<u32>>>,
    pub time: Value<u32>,
}

pub struct Production {
    pub inputs: Option<Vec<Value<u32>>>,
    pub outputs: Vec<Value<u32>>,
    pub labor: Option<Vec<Value<u32>>>,
    pub time: Value<u32>,
}
