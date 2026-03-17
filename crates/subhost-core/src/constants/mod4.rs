use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstantsModule4 {
    pub id: u64,
    pub data: Vec<u8>,
}

impl ConstantsModule4 {
    pub fn new(id: u64, data: Vec<u8>) -> Self {
        Self { id, data }
    }
}
