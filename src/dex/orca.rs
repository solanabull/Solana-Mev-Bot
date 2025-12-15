//! Orca DEX integration (Orca Whirlpool)

pub struct OrcaDex;

impl OrcaDex {
    pub async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self)
    }
}
