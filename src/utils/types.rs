//! Common types and traits used across the MEV bot
//!
//! This module defines shared interfaces and data structures
//! that are used by multiple components of the bot.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Trait for opportunities that can be executed by the bot
#[async_trait]
pub trait ExecutableOpportunity: Send + Sync {
    /// Get data needed for transaction simulation
    async fn get_simulation_data(&self) -> Result<SimulationData, Box<dyn std::error::Error>>;

    /// Get data needed for transaction execution
    async fn get_execution_data(&self) -> Result<ExecutionData, Box<dyn std::error::Error>>;

    /// Get expected profit in USD
    fn get_expected_profit(&self) -> f64;

    /// Get strategy name for logging
    fn get_strategy_name(&self) -> &str;
}

/// Data needed for transaction simulation
#[derive(Debug, Clone)]
pub struct SimulationData {
    pub instructions: Vec<solana_sdk::instruction::Instruction>,
    pub signers: Vec<solana_sdk::pubkey::Pubkey>,
    pub recent_blockhash: String,
}

/// Data needed for transaction execution
#[derive(Debug, Clone)]
pub struct ExecutionData {
    pub instructions: Vec<solana_sdk::instruction::Instruction>,
    pub signers: Vec<solana_sdk::pubkey::Pubkey>,
    pub compute_unit_limit: Option<u32>,
    pub compute_unit_price: Option<u64>,
}

/// Common opportunity metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpportunityMetadata {
    pub strategy: String,
    pub token_in: String,
    pub token_out: String,
    pub amount_in: u64,
    pub expected_profit_usd: f64,
    pub timestamp: u64,
    pub dex_path: Vec<String>,
}

/// Transaction execution priorities
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionPriority {
    Low,
    Medium,
    High,
    Critical,
}

/// Risk assessment levels
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

/// Health status for components
#[derive(Debug, Clone)]
pub struct ComponentHealth {
    pub healthy: bool,
    pub last_active: u64,
    pub error_count: u32,
    pub status_message: String,
}

/// Engine health status
#[derive(Debug, Clone)]
pub struct EngineHealth {
    pub overall_healthy: bool,
    pub mempool: ComponentHealth,
    pub router: ComponentHealth,
    pub executor: ComponentHealth,
}
