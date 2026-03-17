use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstantsModule5 {
    pub id: u64,
    pub data: Vec<u8>,
}

impl ConstantsModule5 {
    pub fn new(id: u64, data: Vec<u8>) -> Self {
        Self { id, data }
    }
}
