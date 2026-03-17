use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Module2Data {
    pub id: u64,
    pub value: String,
}

impl Module2Data {
    pub fn new(id: u64, value: String) -> Self {
        Self { id, value }
    }
    
    pub fn process(&self) -> Result<String, String> {
        Ok(format!("Processed module 2: {}", self.value))
    }
}
