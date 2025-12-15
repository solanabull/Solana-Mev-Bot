//! OpenBook DEX integration

pub struct OpenBookDex;

impl OpenBookDex {
    pub async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self)
    }
}
