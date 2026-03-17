use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Module3Data {
    pub id: u64,
    pub value: String,
}

impl Module3Data {
    pub fn new(id: u64, value: String) -> Self {
        Self { id, value }
    }
    
    pub fn process(&self) -> Result<String, String> {
        Ok(format!("Processed module 3: {}", self.value))
    }
}
