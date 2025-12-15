//! Configuration management for the MEV bot
//!
//! This module handles loading and validation of configuration from TOML files
//! and environment variables.

use std::collections::HashMap;
use std::fs;
use serde::{Deserialize, Serialize};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey};
use std::sync::Arc;

use crate::utils::logger;

/// Main configuration structure
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub bot: BotConfig,
    pub solana: SolanaConfig,
    pub jito: JitoConfig,
    pub strategies: StrategyConfig,
    pub arbitrage: ArbitrageConfig,
    pub sandwich: SandwichConfig,
    pub liquidation: LiquidationConfig,
    pub risk_management: RiskManagementConfig,
    pub execution: ExecutionConfig,
    pub simulation: SimulationConfig,
    pub logging: LoggingConfig,
    pub monitoring: MonitoringConfig,
    pub mempool: MempoolConfig,
    pub dex_configs: HashMap<String, DexConfig>,
}

/// Bot operational settings
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BotConfig {
    pub enabled: bool,
    pub name: String,
    pub version: String,
}

/// Solana network configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SolanaConfig {
    pub rpc_url: String,
    pub ws_url: String,
    pub commitment: String,
    pub wallet_public_key: String,
}

/// Jito Block Engine configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JitoConfig {
    pub enabled: bool,
    pub block_engine_url: String,
    pub tip_account: String,
    pub max_tip_lamports: u64,
}

/// Strategy enable/disable flags
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StrategyConfig {
    pub arbitrage: bool,
    pub sandwich: bool,
    pub liquidation: bool,
}

/// Arbitrage strategy configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ArbitrageConfig {
    pub enabled: bool,
    pub min_profit_usd: f64,
    pub max_slippage_bps: u16,
    pub max_hops: usize,
    pub supported_dexes: Vec<String>,
    pub refresh_interval_ms: u64,
}

/// Sandwich strategy configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SandwichConfig {
    pub enabled: bool,
    pub min_target_size_usd: f64,
    pub max_front_run_bps: u16,
    pub max_back_run_bps: u16,
    pub max_slippage_bps: u16,
}

/// Liquidation strategy configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LiquidationConfig {
    pub enabled: bool,
    pub protocols: Vec<String>,
    pub min_liquidation_profit_usd: f64,
    pub max_positions_to_monitor: usize,
}

/// Risk management controls
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RiskManagementConfig {
    pub max_sol_per_trade: f64,
    pub daily_loss_limit_usd: f64,
    pub max_consecutive_failures: u32,
    pub auto_disable_on_failures: bool,
    pub kill_switch: bool,
}

/// Transaction execution settings
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExecutionConfig {
    pub compute_unit_limit: u32,
    pub compute_unit_price_micro_lamports: u64,
    pub priority_fee_lamports: u64,
    pub max_retries: u32,
    pub blockhash_refresh_interval_ms: u64,
}

/// Transaction simulation settings
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SimulationConfig {
    pub enabled: bool,
    pub max_simulation_time_ms: u64,
    pub validate_profit: bool,
    pub validate_slippage: bool,
    pub validate_compute_units: bool,
}

/// Logging configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoggingConfig {
    pub level: String,
    pub json_format: bool,
    pub file_path: String,
    pub max_file_size_mb: usize,
    pub max_files: usize,
}

/// Monitoring configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MonitoringConfig {
    pub enabled: bool,
    pub metrics_port: u16,
    pub alert_webhook_url: String,
    pub health_check_interval_seconds: u64,
}

/// Mempool monitoring configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MempoolConfig {
    pub enabled: bool,
    pub subscription_filters: Vec<String>,
    pub dex_programs: Vec<String>,
    pub max_pending_transactions: usize,
    pub transaction_timeout_seconds: u64,
}

/// DEX-specific configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DexConfig {
    pub program_id: String,
    pub fee_bps: u16,
}

impl Config {
    /// Load configuration from TOML file
    pub fn load(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = fs::read_to_string(path)?;
        let mut config: Config = toml::from_str(&contents)?;

        // Override with environment variables if present
        config.load_env_vars()?;

        Ok(config)
    }

    /// Load environment variable overrides
    fn load_env_vars(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if let Ok(wallet_key) = std::env::var("WALLET_PUBLIC_KEY") {
            self.solana.wallet_public_key = wallet_key;
        }

        if let Ok(rpc_url) = std::env::var("SOLANA_RPC_URL") {
            self.solana.rpc_url = rpc_url;
        }

        if let Ok(ws_url) = std::env::var("SOLANA_WS_URL") {
            self.solana.ws_url = ws_url;
        }

        Ok(())
    }

    /// Validate configuration
    pub fn validate(&self) -> Result<(), Box<dyn std::error::Error>> {
        // Validate wallet public key
        if self.solana.wallet_public_key.is_empty() {
            return Err("Wallet public key is required".into());
        }

        Pubkey::try_from(&self.solana.wallet_public_key)?;

        // Validate DEX program IDs
        for dex_config in self.dex_configs.values() {
            Pubkey::try_from(&dex_config.program_id)?;
        }

        // Validate risk management
        if self.risk_management.max_sol_per_trade <= 0.0 {
            return Err("Max SOL per trade must be positive".into());
        }

        // Validate arbitrage settings
        if self.arbitrage.min_profit_usd < 0.0 {
            return Err("Minimum profit must be non-negative".into());
        }

        if self.arbitrage.max_slippage_bps > 10000 {
            return Err("Max slippage cannot exceed 100%".into());
        }

        logger::info!("Configuration validation passed");
        Ok(())
    }

    /// Create Solana RPC client from configuration
    pub fn create_solana_client(&self) -> Result<RpcClient, Box<dyn std::error::Error>> {
        let commitment = match self.solana.commitment.as_str() {
            "confirmed" => CommitmentConfig::confirmed(),
            "finalized" => CommitmentConfig::finalized(),
            _ => CommitmentConfig::processed(),
        };

        Ok(RpcClient::new_with_commitment(
            self.solana.rpc_url.clone(),
            commitment,
        ))
    }

    /// Get DEX program ID by name
    pub fn get_dex_program_id(&self, dex_name: &str) -> Option<Pubkey> {
        self.dex_configs
            .get(dex_name)
            .and_then(|config| Pubkey::try_from(&config.program_id).ok())
    }

    /// Get DEX fee in basis points
    pub fn get_dex_fee_bps(&self, dex_name: &str) -> u16 {
        self.dex_configs
            .get(dex_name)
            .map(|config| config.fee_bps)
            .unwrap_or(30) // Default 0.3%
    }
}
