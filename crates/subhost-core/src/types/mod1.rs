use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypesModule1 {
    pub id: u64,
    pub data: Vec<u8>,
}

impl TypesModule1 {
    pub fn new(id: u64, data: Vec<u8>) -> Self {
        Self { id, data }
    }
}
