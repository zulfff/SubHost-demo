use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrimitivesModule3 {
    pub id: u64,
    pub data: Vec<u8>,
}

impl PrimitivesModule3 {
    pub fn new(id: u64, data: Vec<u8>) -> Self {
        Self { id, data }
    }
}
