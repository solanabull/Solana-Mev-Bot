use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use solana_sdk::hash::Hash;
use solana_client::rpc_client::RpcClient;
use anyhow::{Result, anyhow};
use colored::Colorize;
use lazy_static::lazy_static;
use crate::common::logger::Logger;

// Global state for latest blockhash and timestamp
lazy_static! {
    static ref LATEST_BLOCKHASH: Arc<RwLock<Option<Hash>>> = Arc::new(RwLock::new(None));
    static ref BLOCKHASH_LAST_UPDATED: Arc<RwLock<Option<Instant>>> = Arc::new(RwLock::new(None));
}

const BLOCKHASH_STALENESS_THRESHOLD: Duration = Duration::from_secs(10);
const UPDATE_INTERVAL: Duration = Duration::from_millis(300);

pub struct BlockhashProcessor {
    rpc_client: Arc<RpcClient>,
    logger: Logger,
}

impl BlockhashProcessor {
    pub async fn new(rpc_client: Arc<RpcClient>) -> Result<Self> {
        let logger = Logger::new("[BLOCKHASH-PROCESSOR] => ".cyan().to_string());
        
        Ok(Self {
            rpc_client,
            logger,
        })
    }

    pub async fn start(&self) -> Result<()> {
        self.logger.log("Starting blockhash processor...".green().to_string());

        // Clone necessary components for the background task
        let rpc_client = self.rpc_client.clone();
        let logger = self.logger.clone();

        tokio::spawn(async move {
            loop {
                match Self::update_blockhash_from_rpc(&rpc_client).await {
                    Ok(blockhash) => {
                        // Update global blockhash
                        let mut latest = LATEST_BLOCKHASH.write().await;
                        *latest = Some(blockhash);
                        
                        // Update timestamp
                        let mut last_updated = BLOCKHASH_LAST_UPDATED.write().await;
                        *last_updated = Some(Instant::now());
                        
                        // logger.log(format!("Updated latest blockhash: {}", blockhash));
                    }
                    Err(e) => {
                        logger.log(format!("Error getting latest blockhash: {}", e).red().to_string());
                    }
                }

                tokio::time::sleep(UPDATE_INTERVAL).await;
            }
        });

        Ok(())
    }

    async fn update_blockhash_from_rpc(rpc_client: &RpcClient) -> Result<Hash> {
        rpc_client.get_latest_blockhash()
            .map_err(|e| anyhow!("Failed to get blockhash from RPC: {}", e))
    }

    /// Update the latest blockhash and its timestamp
    async fn update_blockhash(hash: Hash) {
        let mut latest = LATEST_BLOCKHASH.write().await;
        *latest = Some(hash);
        
        let mut last_updated = BLOCKHASH_LAST_UPDATED.write().await;
        *last_updated = Some(Instant::now());
    }

    /// Get the latest cached blockhash with freshness check
    pub async fn get_latest_blockhash() -> Option<Hash> {
        // Check if blockhash is stale
        let last_updated = BLOCKHASH_LAST_UPDATED.read().await;
        if let Some(instant) = *last_updated {
            if instant.elapsed() > BLOCKHASH_STALENESS_THRESHOLD {
                return None;
            }
        }
        
        let latest = LATEST_BLOCKHASH.read().await;
        *latest
    }

    /// Get a fresh blockhash, falling back to RPC if necessary
    pub async fn get_fresh_blockhash(&self) -> Result<Hash> {
        if let Some(hash) = Self::get_latest_blockhash().await {
            return Ok(hash);
        }
        
        // Fallback to RPC if cached blockhash is stale or missing
        self.logger.log("Cached blockhash is stale or missing, falling back to RPC...".yellow().to_string());
        let new_hash = self.rpc_client.get_latest_blockhash()
            .map_err(|e| anyhow!("Failed to get blockhash from RPC: {}", e))?;
        
        Self::update_blockhash(new_hash).await;
        Ok(new_hash)
    }
} 