mod engine;
mod strategies;
mod dex;
mod utils;

use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, error, warn};

use engine::{Engine, EngineConfig};
use utils::config::Config;
use utils::logger::init_logger;

/// Main entry point for the Solana MEV Bot
///
/// This bot implements multiple MEV strategies including:
/// - Arbitrage between DEXes (Raydium, Orca, OpenBook)
/// - Sandwich attacks (optional)
/// - Liquidation monitoring (optional)
///
/// The bot uses real-time mempool monitoring, transaction simulation,
/// and optimized execution through Jito bundles for minimal latency.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    init_logger()?;

    info!("Starting Solana MEV Bot v{}", env!("CARGO_PKG_VERSION"));

    // Load configuration
    let config = Config::load("config/config.toml")?;
    info!("Configuration loaded successfully");

    // Validate configuration
    config.validate()?;

    // Check if kill switch is enabled
    if config.risk_management.kill_switch {
        error!("Kill switch is enabled. Bot will not start.");
        return Ok(());
    }

    // Initialize Solana client
    let solana_client = Arc::new(config.create_solana_client()?);

    // Initialize engine with configuration
    let engine_config = EngineConfig {
        solana_client: solana_client.clone(),
        config: config.clone(),
    };

    let engine = Arc::new(RwLock::new(Engine::new(engine_config).await?));

    // Setup graceful shutdown handler
    let engine_clone = engine.clone();
    let shutdown_handle = tokio::spawn(async move {
        tokio::signal::ctrl_c().await.expect("Failed to listen for shutdown signal");
        warn!("Shutdown signal received, stopping bot...");

        let mut engine = engine_clone.write().await;
        if let Err(e) = engine.stop().await {
            error!("Error during shutdown: {}", e);
        }
    });

    // Start the MEV engine
    info!("Starting MEV engine...");
    let engine_clone = engine.clone();
    let engine_handle = tokio::spawn(async move {
        let mut engine = engine_clone.write().await;
        if let Err(e) = engine.run().await {
            error!("Engine error: {}", e);
        }
    });

    // Wait for either completion or shutdown
    tokio::select! {
        _ = engine_handle => {
            info!("Engine completed");
        }
        _ = shutdown_handle => {
            info!("Bot shutdown complete");
        }
    }

    Ok(())
}
