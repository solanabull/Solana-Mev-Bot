//! DEX protocol integrations
//!
//! Provides unified interface to interact with different DEXes:
//! - Raydium AMM
//! - Orca Whirlpool
//! - OpenBook orderbook

pub mod raydium;
pub mod orca;
pub mod openbook;

pub use raydium::RaydiumDex;
pub use orca::OrcaDex;
pub use openbook::OpenBookDex;

/// DEX manager for unified access
pub struct DexManager {
    raydium: Option<RaydiumDex>,
    orca: Option<OrcaDex>,
    openbook: Option<OpenBookDex>,
}

impl DexManager {
    /// Create new DEX manager
    pub async fn new(config: &crate::utils::config::Config) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            raydium: if config.arbitrage.supported_dexes.contains(&"raydium".to_string()) {
                Some(RaydiumDex::new().await?)
            } else {
                None
            },
            orca: if config.arbitrage.supported_dexes.contains(&"orca".to_string()) {
                Some(OrcaDex::new().await?)
            } else {
                None
            },
            openbook: if config.arbitrage.supported_dexes.contains(&"openbook".to_string()) {
                Some(OpenBookDex::new().await?)
            } else {
                None
            },
        })
    }
}
