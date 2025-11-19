use std::time::Duration;
use tokio::time;
use colored::Colorize;

use crate::common::logger::Logger;
use crate::common::cache::{TOKEN_ACCOUNT_CACHE, TOKEN_MINT_CACHE};

/// CacheMaintenanceService handles periodic cleanup of expired cache entries
pub struct CacheMaintenanceService {
    logger: Logger,
    cleanup_interval: Duration,
}

impl CacheMaintenanceService {
    pub fn new(cleanup_interval_seconds: u64) -> Self {
        Self {
            logger: Logger::new("[CACHE-MAINTENANCE] => ".magenta().to_string()),
            cleanup_interval: Duration::from_secs(cleanup_interval_seconds),
        }
    }
    
    /// Start the cache maintenance service
    pub async fn start(self) {
        self.logger.log("Starting cache maintenance service".to_string());
        
        let mut interval = time::interval(self.cleanup_interval);
        
        loop {
            interval.tick().await;
            self.cleanup_expired_entries().await;
        }
    }
    
    /// Clean up expired cache entries
    async fn cleanup_expired_entries(&self) {
        self.logger.log("Running cache cleanup".to_string());
        
        // Clean up token account cache
        let token_account_count_before = TOKEN_ACCOUNT_CACHE.size();
        TOKEN_ACCOUNT_CACHE.clear_expired();
        let token_account_count_after = TOKEN_ACCOUNT_CACHE.size();
        
        // Clean up token mint cache
        let token_mint_count_before = TOKEN_MINT_CACHE.size();
        TOKEN_MINT_CACHE.clear_expired();
        let token_mint_count_after = TOKEN_MINT_CACHE.size();
        
        // Log cleanup results
        self.logger.log(format!(
            "Cache cleanup complete - Token accounts: {} -> {}, Token mints: {} -> {}",
            token_account_count_before, token_account_count_after,
            token_mint_count_before, token_mint_count_after
        ));
    }
}

/// Start the cache maintenance service in a background task
pub async fn start_cache_maintenance(cleanup_interval_seconds: u64) {
    let service = CacheMaintenanceService::new(cleanup_interval_seconds);
    
    // Spawn a background task for cache maintenance
    tokio::spawn(async move {
        service.start().await;
    });
} 