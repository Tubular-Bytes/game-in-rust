use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct Inventory {
    id: Uuid,
}

impl Inventory {
    pub fn new(id: Uuid) -> Self {
        Self { id }
    }

    pub fn id(&self) -> Uuid {
        self.id
    }
}
