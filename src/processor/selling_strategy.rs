use crate::common::config::import_env_var;
use crate::processor::monitor::PoolInfo;
use solana_sdk::signature::Signer;
use std::collections::{HashSet, VecDeque};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use anyhow::{anyhow, Result};
use anchor_client::solana_sdk::{hash::Hash, instruction::Instruction, pubkey::Pubkey, signature::{Keypair, Signature}};
use colored::Colorize;
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use spl_associated_token_account::get_associated_token_address;
use dashmap::DashMap;
use solana_program_pack::Pack;

use crate::common::{
    config::{AppState, SwapConfig},
    logger::Logger,
    cache::WALLET_TOKEN_ACCOUNTS,
};
use crate::processor::transaction_parser::{TradeInfoFromToken, DexType};
use crate::common::timeseries as ts;
use crate::processor::swap::{SwapDirection, SwapProtocol, SwapInType};
use crate::dex::pump_fun::Pump;
use crate::dex::pump_swap::PumpSwap;

// Implement conversion from SwapProtocol to DexType
impl From<SwapProtocol> for DexType {
    fn from(protocol: SwapProtocol) -> Self {
        match protocol {
            SwapProtocol::PumpFun => DexType::PumpFun,
            SwapProtocol::PumpSwap => DexType::PumpSwap,
            SwapProtocol::RaydiumLaunchpad => DexType::RaydiumLaunchpad,
            SwapProtocol::Auto | SwapProtocol::Unknown => DexType::Unknown,
        }
    }
}

// Global state for token metrics
lazy_static! {
    pub static ref TOKEN_METRICS: Arc<DashMap<String, TokenMetrics>> = Arc::new(DashMap::new());
    pub static ref TOKEN_TRACKING: Arc<DashMap<String, TokenTrackingInfo>> = Arc::new(DashMap::new());
    pub static ref HISTORICAL_TRADES: Arc<DashMap<(), VecDeque<TradeExecutionRecord>>> = Arc::new({
        let map = DashMap::new();
        map.insert((), VecDeque::with_capacity(100));
        map
    });
}

/// Token metrics for selling strategy
#[derive(Clone, Debug)]
pub struct TokenMetrics {
    pub entry_price: f64,
    pub highest_price: f64,
    pub lowest_price: f64,
    pub current_price: f64,
    pub volume_24h: f64,
    pub market_cap: f64,
    pub time_held: u64,
    pub last_update: Instant,
    pub buy_timestamp: u64,
    pub amount_held: f64,
    pub cost_basis: f64,
    pub price_history: VecDeque<f64>,     // Rolling window of prices
    pub volume_history: VecDeque<f64>,    // Rolling window of volumes
    pub liquidity_at_entry: f64,
    pub liquidity_at_current: f64,
    pub protocol: SwapProtocol,           // Track which protocol was used to buy
}

/// Token tracking info for progressive selling
pub struct TokenTrackingInfo {
    pub top_pnl: f64,
    pub last_sell_time: Instant,
    pub completed_intervals: HashSet<String>,
    pub sell_attempts: usize,
    pub sell_success: usize,
}

/// Record of executed trades for analytics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeExecutionRecord {
    pub mint: String,
    pub entry_price: f64,
    pub exit_price: f64,
    pub pnl: f64,
    pub reason: String,
    pub timestamp: u64,
    pub amount_sold: f64,
    pub protocol: String,
}

/// Market condition enum for dynamic strategy adjustment
#[derive(Debug, Clone)]
pub enum MarketCondition {
    Bullish,
    Bearish,
    Volatile,
    Stable,
}

/// Configuration for profit taking strategy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfitTakingConfig {
    pub target_percentage: f64,           // 1.0 = 100%
    pub scale_out_percentages: Vec<f64>,  // [0.5, 0.3, 0.2] for 50%, 30%, 20%
}

impl Default for ProfitTakingConfig {
    fn default() -> Self {
        Self {
            target_percentage: 1.0,       // 100% profit target
            scale_out_percentages: vec![0.5, 0.3, 0.2], // 50%, 30%, 20%
        }
    }
}

// Add this helper function after the lazy_static block and before the structs
fn parse_vec_f64(input: String) -> Vec<f64> {
    input
        .split(',')
        .filter_map(|s| s.trim().parse::<f64>().ok())
        .collect()
}

impl ProfitTakingConfig {
    pub fn set_from_env() -> Self {
        let target_percentage = import_env_var("PROFIT_TAKING_TARGET_PERCENTAGE").parse::<f64>().unwrap_or(1.0);
        let scale_out_percentages = parse_vec_f64(import_env_var("PROFIT_TAKING_SCALE_OUT_PERCENTAGES"))
            .into_iter()
            .filter(|&x| x > 0.0 && x <= 1.0)
            .collect::<Vec<f64>>();
        
        Self {
            target_percentage,
            scale_out_percentages: if scale_out_percentages.is_empty() {
                vec![0.5, 0.3, 0.2]
            } else {
                scale_out_percentages
            },
        }
    }
}

/// Configuration for dynamic trailing stop strategy based on PnL thresholds
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrailingStopConfig {
    pub activation_percentage: f64,   // 0.2 = 20% from peak
    pub trail_percentage: f64,        // 0.05 = 5% trailing (fallback)
    pub dynamic_thresholds: Vec<TrailingStopThreshold>, // Dynamic thresholds based on PnL
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrailingStopThreshold {
    pub pnl_threshold: f64,    // PnL percentage threshold (e.g., 20.0 for 20%)
    pub trail_percentage: f64, // Trailing stop percentage for this threshold
}

impl Default for TrailingStopConfig {
    fn default() -> Self {
        Self {
            activation_percentage: 20.0,   // 20% activation threshold (as percentage)
            trail_percentage: 10.0,        // 10% trail (as percentage) - fallback
            dynamic_thresholds: vec![
                TrailingStopThreshold { pnl_threshold: 20.0, trail_percentage: 5.0 },   // >20% PnL: 5% trailing stop
                TrailingStopThreshold { pnl_threshold: 50.0, trail_percentage: 10.0 },  // >50% PnL: 10% trailing stop
                TrailingStopThreshold { pnl_threshold: 100.0, trail_percentage: 30.0 }, // >100% PnL: 30% trailing stop
                TrailingStopThreshold { pnl_threshold: 200.0, trail_percentage: 100.0 }, // >200% PnL: 100% trailing stop
                TrailingStopThreshold { pnl_threshold: 500.0, trail_percentage: 100.0 }, // >500% PnL: 100% trailing stop
                TrailingStopThreshold { pnl_threshold: 1000.0, trail_percentage: 100.0 }, // >1000% PnL: 100% trailing stop
            ],
        }
    }
}

impl TrailingStopConfig {
    pub fn set_from_env() -> Self {
        let activation_percentage = import_env_var("TRAILING_STOP_ACTIVATION_PERCENTAGE").parse::<f64>().unwrap_or(20.0);
        let trail_percentage = import_env_var("TRAILING_STOP_TRAIL_PERCENTAGE").parse::<f64>().unwrap_or(10.0);
        
        // Parse dynamic thresholds from env or use defaults
        let dynamic_thresholds = Self::parse_dynamic_thresholds_from_env();
        
        Self {
            activation_percentage,
            trail_percentage,
            dynamic_thresholds,
        }
    }
    
    fn parse_dynamic_thresholds_from_env() -> Vec<TrailingStopThreshold> {
        // Try to parse from environment variable in format: "20:5,50:10,100:30,200:100,500:100,1000:100"
        if let Ok(env_value) = std::env::var("DYNAMIC_TRAILING_STOP_THRESHOLDS") {
            let mut thresholds = Vec::new();
            for pair in env_value.split(',') {
                let parts: Vec<&str> = pair.split(':').collect();
                if parts.len() == 2 {
                    if let (Ok(pnl), Ok(trail)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>()) {
                        thresholds.push(TrailingStopThreshold {
                            pnl_threshold: pnl,
                            trail_percentage: trail,
                        });
                    }
                }
            }
            if !thresholds.is_empty() {
                // Sort by PnL threshold ascending
                thresholds.sort_by(|a, b| a.pnl_threshold.partial_cmp(&b.pnl_threshold).unwrap());
                return thresholds;
            }
        }
        
        // Return default thresholds if parsing fails
        vec![
            TrailingStopThreshold { pnl_threshold: 20.0, trail_percentage: 5.0 },
            TrailingStopThreshold { pnl_threshold: 50.0, trail_percentage: 10.0 },
            TrailingStopThreshold { pnl_threshold: 100.0, trail_percentage: 30.0 },
            TrailingStopThreshold { pnl_threshold: 200.0, trail_percentage: 100.0 },
            TrailingStopThreshold { pnl_threshold: 500.0, trail_percentage: 100.0 },
            TrailingStopThreshold { pnl_threshold: 1000.0, trail_percentage: 100.0 },
        ]
    }
    
    /// Get the appropriate trailing stop percentage for a given PnL
    pub fn get_trailing_stop_for_pnl(&self, pnl_percentage: f64) -> f64 {
        // Find the highest threshold that the PnL exceeds
        let mut applicable_trail = self.trail_percentage; // fallback
        
        for threshold in &self.dynamic_thresholds {
            if pnl_percentage >= threshold.pnl_threshold {
                applicable_trail = threshold.trail_percentage;
            } else {
                break; // Since thresholds are sorted, we can break early
            }
        }
        
        applicable_trail
    }
}
/// Configuration for liquidity monitoring
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiquidityMonitorConfig {
    pub min_absolute_liquidity: f64,  // Minimum SOL liquidity to hold
    pub max_acceptable_drop: f64,     // 0.5 = 50% drop from entry
}

impl Default for LiquidityMonitorConfig {
    fn default() -> Self {
        Self {
            min_absolute_liquidity: 1.0,  // 1 SOL minimum liquidity
            max_acceptable_drop: 0.5,     // 50% drop from entry
        }
    }
}

impl LiquidityMonitorConfig {
    pub fn set_from_env() -> Self {
        let min_absolute_liquidity = import_env_var("MIN_ABSOLUTE_LIQUIDITY").parse::<f64>().unwrap_or(1.0);
        let max_acceptable_drop = import_env_var("MAX_ACCEPTABLE_DROP").parse::<f64>().unwrap_or(0.5);
        
        Self {
            min_absolute_liquidity,
            max_acceptable_drop,
        }
    }
}
/// Configuration for volume analysis
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeAnalysisConfig {
    pub lookback_period: usize,       // Number of trades to look back
    pub spike_threshold: f64,         // 3.0 = 3x average volume
    pub drop_threshold: f64,          // 0.3 = 30% of average volume
}

impl Default for VolumeAnalysisConfig {
    fn default() -> Self {
        Self {
            lookback_period: 20,      // 20 trades lookback
            spike_threshold: 3.0,     // 3x average volume for spike
            drop_threshold: 0.3,      // 30% of average volume for drop
        }
    }
}

impl VolumeAnalysisConfig {
    pub fn set_from_env() -> Self {
        let lookback_period = import_env_var("VOLUME_ANALYSIS_LOOKBACK_PERIOD").parse::<usize>().unwrap_or(20);
        let spike_threshold = import_env_var("VOLUME_ANALYSIS_SPIKE_THRESHOLD").parse::<f64>().unwrap_or(3.0);
        let drop_threshold = import_env_var("VOLUME_ANALYSIS_DROP_THRESHOLD").parse::<f64>().unwrap_or(0.3);
        
        Self {
            lookback_period,
            spike_threshold,
            drop_threshold,
        }
    }
}

/// Configuration for time-based exits
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeExitConfig {
    pub max_hold_time_secs: u64,      // Maximum time to hold position
    pub min_profit_time_secs: u64,    // Minimum time to hold profitable trades
}

impl Default for TimeExitConfig {
    fn default() -> Self {
        Self {
            max_hold_time_secs: 3600,     // 1 hour max hold time
            min_profit_time_secs: 120,    // 2 minutes min hold for profitable trades
        }
    }
}

impl TimeExitConfig {
    pub fn set_from_env() -> Self {
        let max_hold_time_secs = import_env_var("MAX_HOLD_TIME_SECS").parse::<u64>().unwrap_or(3600);
        let min_profit_time_secs = import_env_var("MIN_PROFIT_TIME_SECS").parse::<u64>().unwrap_or(120);
        
        Self {
            max_hold_time_secs,
            min_profit_time_secs,
        }
    }
}

/// Configuration for dynamic whale selling based on PNL thresholds
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DynamicWhaleSelling {
    pub whale_selling_thresholds: Vec<WhaleSellingThreshold>,
    pub retracement_percentage: f64,      // Retracement percentage
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhaleSellingThreshold {
    pub pnl_threshold: f64,        // PNL percentage threshold
    pub whale_limit_sol: f64,      // SOL limit for whale selling
    pub use_emergency_zeroslot: bool, // Whether to use emergency zeroslot selling
}

impl Default for DynamicWhaleSelling {
    fn default() -> Self {
        Self {
            whale_selling_thresholds: vec![
                WhaleSellingThreshold { pnl_threshold: 10.0, whale_limit_sol: 5.0, use_emergency_zeroslot: true },
                WhaleSellingThreshold { pnl_threshold: 30.0, whale_limit_sol: 6.0, use_emergency_zeroslot: true },
                WhaleSellingThreshold { pnl_threshold: 50.0, whale_limit_sol: 7.0, use_emergency_zeroslot: true },
                WhaleSellingThreshold { pnl_threshold: 200.0, whale_limit_sol: 8.0, use_emergency_zeroslot: true },
                WhaleSellingThreshold { pnl_threshold: 500.0, whale_limit_sol: 9.0, use_emergency_zeroslot: true },
                WhaleSellingThreshold { pnl_threshold: 1000.0, whale_limit_sol: 10.0, use_emergency_zeroslot: true },
            ],
            retracement_percentage: 15.0,     // 15% retracement threshold
        }
    }
}

impl DynamicWhaleSelling {
    pub fn set_from_env() -> Self {
        let retracement_percentage = import_env_var("DYNAMIC_RETRACEMENT_PERCENTAGE").parse::<f64>().unwrap_or(15.0);
        
        // Use default thresholds for now, could be configurable via env vars if needed
        Self {
            whale_selling_thresholds: Self::default().whale_selling_thresholds,
            retracement_percentage,
        }
    }
    
    /// Get the appropriate whale selling threshold based on current PNL
    pub fn get_whale_threshold_for_pnl(&self, pnl: f64) -> Option<&WhaleSellingThreshold> {
        // Find the highest threshold that this PNL qualifies for
        self.whale_selling_thresholds
            .iter()
            .rev() // Start from highest thresholds
            .find(|threshold| pnl >= threshold.pnl_threshold)
    }
}
/// Configuration for selling strategy
#[derive(Clone, Debug)]
pub struct SellingConfig {
    pub take_profit: f64,       // Percentage (e.g., 2.0 for 2%)
    pub stop_loss: f64,         // Percentage (e.g., -5.0 for -5%)
    pub max_hold_time: u64,     // Seconds
    pub retracement_threshold: f64, // Percentage drop from highest price
    pub min_liquidity: f64,     // Minimum SOL in pool
    // Enhanced selling strategy configurations
    pub profit_taking: ProfitTakingConfig,
    pub trailing_stop: TrailingStopConfig,
    pub liquidity_monitor: LiquidityMonitorConfig,
    pub volume_analysis: VolumeAnalysisConfig,
    pub time_based: TimeExitConfig,
    pub dynamic_whale_selling: DynamicWhaleSelling,
}

impl Default for SellingConfig {
    fn default() -> Self {
        Self {
            take_profit: 25.0,               // 25% profit target  
            stop_loss: -30.0,                // 30% stop loss
            max_hold_time: 3600,             // 1 hour max hold time (updated from 24 hours)
            retracement_threshold: 15.0,     // 15% retracement threshold
            min_liquidity: 1.0,              // 1 SOL minimum liquidity
            
            // Enhanced selling strategy configurations
            profit_taking: ProfitTakingConfig::default(),
            trailing_stop: TrailingStopConfig::default(),
            liquidity_monitor: LiquidityMonitorConfig::default(),
            volume_analysis: VolumeAnalysisConfig::default(),
            time_based: TimeExitConfig::default(),
            dynamic_whale_selling: DynamicWhaleSelling::default(),
        }
    }
}

impl SellingConfig {
    pub fn set_from_env() -> Self {
        let take_profit = import_env_var("TAKE_PROFIT").parse::<f64>().unwrap_or(25.0);
        let stop_loss = import_env_var("STOP_LOSS").parse::<f64>().unwrap_or(-30.0);
        let max_hold_time = import_env_var("MAX_HOLD_TIME").parse::<u64>().unwrap_or(3600); // Default to 1 hour
        let retracement_threshold = import_env_var("RETRACEMENT_THRESHOLD").parse::<f64>().unwrap_or(15.0);
        let min_liquidity = import_env_var("MIN_LIQUIDITY").parse::<f64>().unwrap_or(1.0);
        let profit_taking = ProfitTakingConfig::set_from_env();
        let trailing_stop = TrailingStopConfig::set_from_env();
        let liquidity_monitor = LiquidityMonitorConfig::set_from_env();
        let volume_analysis = VolumeAnalysisConfig::set_from_env();
        let time_based = TimeExitConfig::set_from_env();
        let dynamic_whale_selling = DynamicWhaleSelling::set_from_env();

        Self {
            take_profit,
            stop_loss,
            max_hold_time,
            retracement_threshold,
            min_liquidity,
            profit_taking,
            trailing_stop,
            liquidity_monitor,
            volume_analysis,
            time_based,
            dynamic_whale_selling,
        }   
    }
}

/// Status of a token being managed
#[derive(Debug, Clone, PartialEq)]
pub enum TokenStatus {
    Active,           // Token is actively being managed
    PendingSell,      // Token is in process of being sold
    Sold,             // Token has been completely sold
    Failed,           // Token transaction failed
}

/// Token Manager to track and manage multiple tokens
#[derive(Clone)]
pub struct TokenManager {
    logger: Logger,
}

impl TokenManager {
    pub fn new() -> Self {
        Self {
            logger: Logger::new("[TOKEN-MANAGER] => ".cyan().to_string()),
        }
    }

    /// Get a list of all active token mints
    pub async fn get_active_tokens(&self) -> Vec<String> {
        TOKEN_METRICS.iter().map(|entry| entry.key().clone()).collect()
    }

    /// Check if a token exists in the metrics
    pub async fn token_exists(&self, token_mint: &str) -> bool {
        TOKEN_METRICS.contains_key(token_mint)
    }

    /// Get metrics for a specific token, if it exists
    pub async fn get_token_metrics(&self, token_mint: &str) -> Option<TokenMetrics> {
        TOKEN_METRICS.get(token_mint).map(|metrics| metrics.clone())
    }

    /// Add or update a token in the metrics map
    pub async fn update_token(&self, token_mint: &str, metrics: TokenMetrics) -> Result<()> {
        TOKEN_METRICS.insert(token_mint.to_string(), metrics);
        self.logger.log(format!("Updated metrics for token: {}", token_mint));
        Ok(())
    }

    /// Remove a token from tracking
    pub async fn remove_token(&self, token_mint: &str) -> Result<()> {
        if TOKEN_METRICS.remove(token_mint).is_some() {
            // Also remove from tracking
            TOKEN_TRACKING.remove(token_mint);
            self.logger.log(format!("Removed token from tracking: {}", token_mint));
        } else {
            self.logger.log(format!("Token not found for removal: {}", token_mint));
        }
        Ok(())
    }

    /// Comprehensive verification and cleanup for selling strategy tokens
    pub async fn verify_and_cleanup_sold_tokens(
        &self,
        app_state: &Arc<AppState>,
    ) -> Result<usize> {
        self.logger.log("🔄 Starting comprehensive cleanup for selling strategy tokens...".blue().to_string());
        
        // Get all tokens from both tracking systems
        let tokens_to_check: HashSet<String> = TOKEN_METRICS.iter()
            .map(|entry| entry.key().clone())
            .chain(TOKEN_TRACKING.iter().map(|entry| entry.key().clone()))
            .collect();
        
        if tokens_to_check.is_empty() {
            self.logger.log("📝 No tokens to check in selling strategy cleanup".blue().to_string());
            return Ok(0);
        }
        
        self.logger.log(format!("🔍 Checking {} tokens in selling strategy cleanup", tokens_to_check.len()));
        
        let mut cleaned_count = 0;
        
        for token_mint in tokens_to_check {
            match self.verify_token_balance(&token_mint, app_state).await {
                Ok(is_fully_sold) => {
                    if is_fully_sold {
                        let mut removed_systems = Vec::new();
                        
                        // Remove from TOKEN_METRICS
                        if TOKEN_METRICS.remove(&token_mint).is_some() {
                            removed_systems.push("TOKEN_METRICS");
                        }
                        
                        // Remove from TOKEN_TRACKING
                        if TOKEN_TRACKING.remove(&token_mint).is_some() {
                            removed_systems.push("TOKEN_TRACKING");
                        }
                        
                        if !removed_systems.is_empty() {
                            cleaned_count += 1;
                            self.logger.log(format!(
                                "🧹 Selling strategy cleanup removed {} from: [{}]", 
                                token_mint, 
                                removed_systems.join(", ")
                            ).green().to_string());
                        }
                    }
                },
                Err(e) => {
                    self.logger.log(format!("⚠️  Error verifying token balance for {}: {}", token_mint, e).yellow().to_string());
                }
            }
            
            // Small delay between checks
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }
        
        let final_metrics_count = TOKEN_METRICS.len();
        let final_tracking_count = TOKEN_TRACKING.len();
        self.logger.log(format!(
            "✅ Selling strategy cleanup completed. Removed: {}, Remaining metrics: {}, tracking: {}", 
            cleaned_count, 
            final_metrics_count,
            final_tracking_count
        ).green().to_string());
        
        Ok(cleaned_count)
    }

    /// Verify if a token is fully sold by checking blockchain balance
    async fn verify_token_balance(
        &self,
        token_mint: &str,
        app_state: &Arc<AppState>,
    ) -> Result<bool> {
        use solana_sdk::pubkey::Pubkey;
        use std::str::FromStr;
        use spl_associated_token_account::get_associated_token_address;
        
        if let Ok(wallet_pubkey) = app_state.wallet.try_pubkey() {
            if let Ok(token_pubkey) = Pubkey::from_str(token_mint) {
                let ata = get_associated_token_address(&wallet_pubkey, &token_pubkey);
                
                match app_state.rpc_nonblocking_client.get_token_account(&ata).await {
                    Ok(account_result) => {
                        match account_result {
                            Some(account) => {
                                if let Ok(amount_value) = account.token_amount.amount.parse::<f64>() {
                                    let decimal_amount = amount_value / 10f64.powi(account.token_amount.decimals as i32);
                                    // Consider it fully sold if balance is very small
                                    Ok(decimal_amount <= 0.000001)
                                } else {
                                    Err(anyhow::anyhow!("Failed to parse token amount"))
                                }
                            },
                            None => {
                                // Token account doesn't exist - means fully sold
                                Ok(true)
                            }
                        }
                    },
                    Err(e) => {
                        Err(anyhow::anyhow!("Error checking token account: {}", e))
                    }
                }
            } else {
                Err(anyhow::anyhow!("Invalid token mint format"))
            }
        } else {
            Err(anyhow::anyhow!("Invalid wallet pubkey"))
        }
    }
    
    /// Log current token portfolio status
    pub async fn log_token_portfolio(&self) {
        let token_count = TOKEN_METRICS.len();
        
        if token_count == 0 {
            self.logger.log("No tokens currently in portfolio".yellow().to_string());
            return;
        }
        
        self.logger.log(format!("Current portfolio contains {} tokens:", token_count).green().to_string());
        
        for entry in TOKEN_METRICS.iter() {
            let mint = entry.key();
            let metrics = entry.value();
            
            let current_pnl = if metrics.entry_price > 0.0 {
                ((metrics.current_price - metrics.entry_price) / metrics.entry_price) * 100.0
            } else {
                0.0
            };
            
            let pnl_color = if current_pnl >= 0.0 { "green" } else { "red" };
            
            self.logger.log(format!(
                "Token: {} - Amount: {:.2}, Entry: {:.8}, Current: {:.8}, PNL: {}",
                mint,
                metrics.amount_held,
                metrics.entry_price,
                metrics.current_price,
                format!("{:.2}%", current_pnl).color(pnl_color).to_string()
            ));
        }
    }
    
    /// Monitor all tokens and identify which ones need action
    pub async fn monitor_all_tokens(&self, engine: &SellingEngine) -> Result<()> {
        let active_tokens: Vec<String> = TOKEN_METRICS.iter()
            .map(|entry| entry.key().clone())
            .collect();

        if active_tokens.is_empty() {
            self.logger.log("No active tokens to monitor".yellow().to_string());
            return Ok(());
        }

        self.logger.log(format!("Monitoring {} active tokens", active_tokens.len()));

        for token_mint in active_tokens {
            let token_mint_clone = token_mint.clone();
            let engine_clone = engine.clone();

            // Check if we should sell this token
            match engine.evaluate_sell_conditions(&token_mint).await {
                Ok((should_sell, use_whale_emergency)) => {
                    if should_sell {
                        if use_whale_emergency {
                            // Spawn whale emergency sell task
                            tokio::spawn(async move {
                                // Create trade info for whale emergency sell
                                match engine_clone.metrics_to_trade_info(&token_mint_clone).await {
                                    Ok(trade_info) => {
                                        // Get the protocol from metrics
                                        let protocol = if let Some(metrics) = TOKEN_METRICS.get(&token_mint_clone) {
                                            metrics.protocol.clone()
                                        } else {
                                            SwapProtocol::PumpFun
                                        };
                                        
                                        // Use unified emergency sell for whale emergency selling
                                        if let Err(e) = engine_clone.unified_emergency_sell(&token_mint_clone, true, Some(&trade_info), Some(protocol)).await {
                                            let logger = Logger::new("[TOKEN-MANAGER-WHALE] => ".red().to_string());
                                            logger.log(format!("Failed to whale emergency sell token {}: {}", token_mint_clone, e));
                                        }
                                    },
                                    Err(e) => {
                                        let logger = Logger::new("[TOKEN-MANAGER-WHALE] => ".red().to_string());
                                        logger.log(format!("Failed to create trade info for whale emergency sell {}: {}", token_mint_clone, e));
                                    }
                                }
                            });
                        } else {
                            // Spawn regular emergency sell task
                            tokio::spawn(async move {
                                // Create trade info for emergency sell
                                match engine_clone.metrics_to_trade_info(&token_mint_clone).await {
                                    Ok(trade_info) => {
                                        // Get the protocol from metrics
                                        let protocol = if let Some(metrics) = TOKEN_METRICS.get(&token_mint_clone) {
                                            metrics.protocol.clone()
                                        } else {
                                            SwapProtocol::PumpFun
                                        };
                                        
                                        if let Err(e) = engine_clone.unified_emergency_sell(&token_mint_clone, false, Some(&trade_info), Some(protocol)).await {
                                            let logger = Logger::new("[TOKEN-MANAGER-EMERGENCY] => ".red().to_string());
                                            logger.log(format!("Failed to emergency sell token {}: {}", token_mint_clone, e));
                                        }
                                    },
                                    Err(e) => {
                                        let logger = Logger::new("[TOKEN-MANAGER-EMERGENCY] => ".red().to_string());
                                        logger.log(format!("Failed to create trade info for emergency sell {}: {}", token_mint_clone, e));
                                    }
                                }
                            });
                        }
                    }
                },
                Err(e) => {
                    self.logger.log(format!("Error evaluating sell conditions for {}: {}", token_mint, e).red().to_string());
                }
            }
        }
        
        Ok(())
    }

    pub async fn get_active_tokens_count(&self) -> usize {
        TOKEN_TRACKING.len()
    }
}

/// Engine for executing selling strategies
#[derive(Clone)]
pub struct SellingEngine {
    app_state: Arc<AppState>,
    swap_config: Arc<SwapConfig>,
    config: SellingConfig,
    logger: Logger,
    token_manager: TokenManager,
}

impl SellingEngine {
    pub fn new(
        app_state: Arc<AppState>,
        swap_config: Arc<SwapConfig>,
        config: SellingConfig,
    ) -> Self {
        Self {
            app_state,
            swap_config,
            config,
            logger: Logger::new("[SELLING-STRATEGY] => ".yellow().to_string()),
            token_manager: TokenManager::new(),
        }
    }
    
    /// Get a reference to the selling configuration
    pub fn get_config(&self) -> &SellingConfig {
        &self.config
    }
    
    /// Log current selling strategy parameters
    pub fn log_selling_parameters(&self) {
        self.logger.log("📊 SELLING STRATEGY PARAMETERS:".cyan().bold().to_string());
        self.logger.log(format!("  🎯 Take Profit: {:.1}%", self.config.take_profit).green().to_string());
        self.logger.log(format!("  🛑 Stop Loss: {:.1}%", self.config.stop_loss).red().to_string());
        self.logger.log(format!("  ⏰ Max Hold Time: {} hour(s)", self.config.max_hold_time / 3600).blue().to_string());
        self.logger.log(format!("  📉 Retracement Threshold: {:.1}%", 
                               self.config.dynamic_whale_selling.retracement_percentage).yellow().to_string());
        self.logger.log(format!("  💧 Min Liquidity: {:.1} SOL", self.config.min_liquidity).purple().to_string());
        // Log dynamic trailing stop configuration
        self.logger.log("  🎯 DYNAMIC TRAILING STOP THRESHOLDS:".cyan().bold().to_string());
        for threshold in &self.config.trailing_stop.dynamic_thresholds {
            self.logger.log(format!("    📈 PNL >= {:.0}%: {:.0}% trailing stop", 
                                   threshold.pnl_threshold, 
                                   threshold.trail_percentage).green().to_string());
        }
        self.logger.log(format!("  ⚡ Activation Threshold: {:.0}%", 
                               self.config.trailing_stop.activation_percentage).yellow().to_string());
        self.logger.log("  🚫 Token Rebuying: DISABLED (permanent blacklist)".red().to_string());
        self.logger.log("  🚀 Copy Selling Method: ZEROSLOT".green().to_string());
        
        // Log dynamic whale selling thresholds
        self.logger.log("  🐋 DYNAMIC WHALE SELLING THRESHOLDS:".cyan().bold().to_string());
        for threshold in &self.config.dynamic_whale_selling.whale_selling_thresholds {
            self.logger.log(format!("    💎 PNL >= {:.0}%: Whale Limit {:.1} SOL (Emergency Zeroslot: {})", 
                                   threshold.pnl_threshold, 
                                   threshold.whale_limit_sol,
                                   if threshold.use_emergency_zeroslot { "✅" } else { "❌" }
                                   ).cyan().to_string());
        }
    }
    
    /// Check for existing token balances and initialize them for copy selling
    pub async fn initialize_copy_selling_for_existing_tokens(&self) -> Result<usize> {
        self.logger.log("🔍 Checking for existing token balances to initialize copy selling...".cyan().to_string());
        
        let wallet_pubkey = self.app_state.wallet.try_pubkey()
            .map_err(|e| anyhow!("Failed to get wallet pubkey: {}", e))?;
        
        // Get all token accounts owned by the wallet
        let token_program = Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")
            .map_err(|e| anyhow!("Invalid token program pubkey: {}", e))?;
        
        let accounts = self.app_state.rpc_client.get_token_accounts_by_owner(
            &wallet_pubkey,
            anchor_client::solana_client::rpc_request::TokenAccountsFilter::ProgramId(token_program)
        ).map_err(|e| anyhow!("Failed to get token accounts: {}", e))?;
        
        let mut initialized_count = 0;
        
        for account_info in accounts {
            // Parse token account
            let token_account_pubkey = Pubkey::from_str(&account_info.pubkey)
                .map_err(|e| anyhow!("Invalid token account pubkey: {}", e))?;
            
            // Get account data
            let account_data = self.app_state.rpc_client.get_account(&token_account_pubkey)
                .map_err(|e| anyhow!("Failed to get account data: {}", e))?;
            
            // Parse token account data
            if let Ok(parsed_account) = spl_token::state::Account::unpack(&account_data.data) {
                let token_mint = parsed_account.mint.to_string();
                let token_amount = parsed_account.amount as f64 / 10f64.powi(9); // Assume 9 decimals for simplicity
                
                // Skip WSOL and very small amounts
                if parsed_account.mint == spl_token::native_mint::id() || token_amount <= 0.000001 {
                    continue;
                }
                
                // Skip if already being tracked
                if TOKEN_METRICS.contains_key(&token_mint) {
                    continue;
                }
                
                self.logger.log(format!("📦 Found existing token balance: {} ({:.6} tokens)", token_mint, token_amount).blue().to_string());
                
                // Create basic token metrics for existing balance (use current price as entry price approximation)
                let current_price = match self.get_current_price(&token_mint).await {
                    Ok(price) => price,
                    Err(_) => {
                        self.logger.log(format!("⚠️  Could not get price for {}, skipping", token_mint).yellow().to_string());
                        continue;
                    }
                };
                
                let metrics = TokenMetrics {
                    entry_price: current_price, // Use current price as approximation
                    highest_price: current_price,
                    lowest_price: current_price,
                    current_price,
                    volume_24h: 0.0,
                    market_cap: 0.0,
                    time_held: 0,
                    last_update: Instant::now(),
                    buy_timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    amount_held: token_amount,
                    cost_basis: current_price * token_amount,
                    price_history: VecDeque::new(),
                    volume_history: VecDeque::new(),
                    liquidity_at_entry: 0.0, // Unknown for existing tokens
                    liquidity_at_current: 0.0, // Will be updated
                    protocol: SwapProtocol::Auto, // Unknown protocol for existing tokens
                };
                
                // Add to tracking
                TOKEN_METRICS.insert(token_mint.clone(), metrics);
                TOKEN_TRACKING.insert(token_mint.clone(), TokenTrackingInfo {
                    top_pnl: 0.0,
                    last_sell_time: Instant::now(),
                    completed_intervals: HashSet::new(),
                    sell_attempts: 0,
                    sell_success: 0,
                });
                
                initialized_count += 1;
                self.logger.log(format!("✅ Initialized copy selling for existing token: {}", token_mint).green().to_string());
            }
        }
        
        if initialized_count > 0 {
            self.logger.log(format!("🎯 Copy selling initialized for {} existing tokens", initialized_count).green().bold().to_string());
        } else {
            self.logger.log("📭 No existing token balances found to initialize for copy selling".blue().to_string());
        }
        
        Ok(initialized_count)
    }
    
    /// Get the token manager
    pub fn token_manager(&self) -> &TokenManager {
        &self.token_manager
    }
    
    /// Get a list of all tokens being managed
    pub async fn get_active_tokens(&self) -> Vec<String> {
        self.token_manager.get_active_tokens().await
    }
    
    /// Log the current token portfolio
    pub async fn log_token_portfolio(&self) {
        self.token_manager.log_token_portfolio().await;
    }
    
    /// Monitor all tokens and sell if needed
    pub async fn monitor_all_tokens(&self) -> Result<()> {
        self.token_manager.monitor_all_tokens(self).await
    }
    
    /// Update metrics for a token based on parsed transaction data
    pub async fn update_metrics(&self, token_mint: &str, trade_info: &TradeInfoFromToken) -> Result<()> {
        let logger = Logger::new("[SELLING-STRATEGY] => ".magenta().to_string());
        
        // Extract data
        let sol_change = trade_info.sol_change;
        let token_change = trade_info.token_change;
        let is_buy = trade_info.is_buy;
        let timestamp = trade_info.timestamp;
        
        // Get wallet pubkey
        let wallet_pubkey = self.app_state.wallet.try_pubkey()
            .map_err(|e| anyhow!("Failed to get wallet pubkey: {}", e))?;

        // Get token account to determine actual balance
        let token_pubkey = Pubkey::from_str(token_mint)
            .map_err(|e| anyhow!("Invalid token mint address: {}", e))?;
        let ata = get_associated_token_address(&wallet_pubkey, &token_pubkey);

        // Get current token balance
        let actual_token_balance = match self.app_state.rpc_nonblocking_client.get_token_account(&ata).await {
            Ok(Some(account)) => {
                let amount_value = account.token_amount.amount.parse::<f64>()
                    .map_err(|e| anyhow!("Failed to parse token amount: {}", e))?;
                amount_value / 10f64.powi(account.token_amount.decimals as i32)
            },
            Ok(None) => 0.0,
            Err(_) => 0.0,
        };
        
        // Calculate price using the same logic as transaction_parser.rs
        let price = match trade_info.dex_type {
            DexType::PumpFun => {
                // PumpFun: virtual_sol_reserves / virtual_token_reserves
                if trade_info.virtual_token_reserves > 0 {
                    (trade_info.virtual_sol_reserves as f64 * 1_000_000_000.0) / 
                    (trade_info.virtual_token_reserves as f64) / 1_000_000_000.0
                } else {
                    0.0
                }
            },
            DexType::PumpSwap => {
                // PumpSwap: use the price from trade_info (already calculated correctly)
                trade_info.price as f64 / 1_000_000_000.0
            },
            DexType::RaydiumLaunchpad => {
                // Use the price calculated by the parser (already scaled correctly)
                trade_info.price as f64 / 1_000_000_000.0
            },
            _ => {
                // Fallback to simple calculation if virtual reserves not available
                if token_change != 0.0 && sol_change != 0.0 {
                    (sol_change / token_change).abs()
                } else {
                    0.0
                }
            }
        };
        
        if price <= 0.0 {
            logger.log(format!("Invalid price calculated: {} (sol_change: {}, token_change: {})", 
                price, sol_change, token_change));
            return Err(anyhow!("Invalid price calculation"));
        }

        // Get current liquidity based on protocol
        let current_liquidity = match self.app_state.protocol_preference {
            SwapProtocol::PumpSwap => {
                let pump_swap = PumpSwap::new(
                    self.app_state.wallet.clone(),
                    Some(self.app_state.rpc_client.clone()),
                    Some(self.app_state.rpc_nonblocking_client.clone()),
                );
                match pump_swap.get_pool_liquidity(token_mint).await {
                    Ok(liquidity) => liquidity,
                    Err(_) => 0.0,
                }
            },
            SwapProtocol::PumpFun => {
                // For PumpFun, use virtual SOL reserves as proxy for liquidity
                trade_info.virtual_sol_reserves as f64 / 1e9 // Convert lamports to SOL
            },
            _ => 0.0,
        };
        
        // Update token metrics using entry API
        let mut entry = TOKEN_METRICS.entry(token_mint.to_string()).or_insert_with(|| TokenMetrics {
            entry_price: 0.0,
            highest_price: 0.0,
            lowest_price: 0.0,
            current_price: 0.0,
            volume_24h: 0.0,
            market_cap: 0.0,
            time_held: 0,
            last_update: Instant::now(),
            buy_timestamp: timestamp,
            amount_held: 0.0,
            cost_basis: 0.0,
            price_history: VecDeque::new(),
            volume_history: VecDeque::new(),
            liquidity_at_entry: current_liquidity, // Set initial liquidity
            liquidity_at_current: current_liquidity, // Set current liquidity
            protocol: self.app_state.protocol_preference.clone(),
        });
        
        // Update metrics based on transaction type
        if is_buy {
            // For buys, update entry price using weighted average
            let token_amount = token_change.abs();
            let sol_amount = sol_change.abs();
            
            logger.log(format!(
                "Processing buy transaction: token_amount={}, sol_amount={}, price={}, current_amount_held={}, current_entry_price={}",
                token_amount, sol_amount, price, entry.amount_held, entry.entry_price
            ));
            
            if token_amount > 0.0 {
                let old_entry_price = entry.entry_price;
                
                // Calculate new weighted average entry price
                let new_entry_price = if entry.amount_held > 0.0 && entry.entry_price > 0.0 {
                    // Weighted average of existing position and new purchase
                    ((entry.entry_price * entry.amount_held) + (price * token_amount)) 
                    / (entry.amount_held + token_amount)
                } else {
                    // First purchase or entry price was 0
                    price
                };
                
                // Update entry price and cost basis
                entry.entry_price = new_entry_price;
                entry.cost_basis += sol_amount;
                entry.liquidity_at_entry = current_liquidity; // Update entry liquidity on buy
                
                logger.log(format!(
                    "Updated entry price for buy: old={}, new={} ({})", 
                    old_entry_price, 
                    new_entry_price,
                    if entry.amount_held > 0.0 && old_entry_price > 0.0 { "weighted avg" } else { "first purchase" }
                ));
            } else {
                logger.log("Warning: Buy transaction detected but token_amount is 0".yellow().to_string());
            }
        } else {
            // For sell transactions, ensure we have an entry price set
            if entry.entry_price == 0.0 && price > 0.0 {
                entry.entry_price = price;
                logger.log(format!("Set entry price from sell transaction: {}", price).yellow().to_string());
            }
        }
        
        // Always update current metrics
        entry.amount_held = actual_token_balance;
        entry.current_price = price;
        entry.liquidity_at_current = current_liquidity; // Update current liquidity
        
        // Update highest price if applicable
        if price > entry.highest_price {
            entry.highest_price = price;
        }
        
        // Update lowest price if applicable (initialize or update if lower)
        if entry.lowest_price == 0.0 || price < entry.lowest_price {
            entry.lowest_price = price;
        }
        
        // Update price history
        entry.price_history.push_back(price);
        if entry.price_history.len() > 20 {  // Keep last 20 prices
            entry.price_history.pop_front();
        }

        // Time-series cache: 20-slot rolling price and buy/sell volume
        let sol_volume = trade_info.sol_change.abs();
        ts::update_for_mint(token_mint, trade_info.slot, price, is_buy, sol_volume);
        
        // Log current metrics
        let pnl = if entry.entry_price > 0.0 {
            ((price - entry.entry_price) / entry.entry_price) * 100.0
        } else {
            0.0
        };
        
        logger.log(format!(
            "Token metrics for {}: Price: {}, Entry: {}, Highest: {}, Lowest: {}, PNL: {:.2}%, Balance: {}, Liquidity: {:.2} SOL",
            token_mint, price, entry.entry_price, entry.highest_price, entry.lowest_price, pnl, actual_token_balance, current_liquidity
        ));

        // Detect bottom after drop from time-series to inform sniper/copy logic
        let bottom = ts::analyze_bottom(token_mint, 10.0, 30.0, 3);
        if bottom.is_bottom {
            logger.log(format!(
                "📉 Potential bottom detected for {}: lowest {:.8}, drop {:.1}% over last 20 slots",
                token_mint, bottom.lowest_price, bottom.drop_pct
            ).yellow().to_string());
        }
        
        Ok(())
    }
    
    /// Record a buy transaction for a token with enhanced metrics tracking
    pub async fn record_buy(&self, token_mint: &str, amount: f64, cost: f64, trade_info: &TradeInfoFromToken) -> Result<()> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Get current price and liquidity
        let current_price = cost / amount;
        let liquidity = trade_info.liquidity;

        // Determine protocol from trade info
        let protocol = match trade_info.dex_type {
            DexType::PumpFun => SwapProtocol::PumpFun,
            DexType::PumpSwap => SwapProtocol::PumpSwap,
            _ => self.app_state.protocol_preference.clone(),
        };

        // Create or update token metrics
        let metrics = TokenMetrics {
            entry_price: current_price,
            highest_price: current_price,
            lowest_price: current_price,
            current_price,
            volume_24h: 0.0,
            market_cap: 0.0,
            time_held: 0,
            last_update: Instant::now(),
            buy_timestamp: timestamp,
            amount_held: amount,
            cost_basis: cost,
            price_history: VecDeque::new(),
            volume_history: VecDeque::new(),
            liquidity_at_entry: liquidity,
            liquidity_at_current: liquidity,
            protocol,
        };

        // Update token metrics in global state
        TOKEN_METRICS.insert(token_mint.to_string(), metrics);

        // Initialize tracking info
        TOKEN_TRACKING.insert(token_mint.to_string(), TokenTrackingInfo {
            top_pnl: 0.0,
            last_sell_time: Instant::now(),
            completed_intervals: HashSet::new(),
            sell_attempts: 0,
            sell_success: 0,
        });

        Ok(())
    }
    
    /// Evaluate whether we should sell a token based on various conditions
    /// 
    /// This method combines all selling conditions from the enhanced decision framework
    /// into a single evaluation, providing a comprehensive analysis of when to exit a position.
    /// 
    /// Returns: (should_sell, use_whale_emergency) where:
    /// - should_sell: true if any sell condition is met
    /// - use_whale_emergency: true if should use emergency zeroslot selling for whale transactions
    /// 
    /// Note: Bot always sells all tokens when sell conditions are met
    pub async fn evaluate_sell_conditions(&self, token_mint: &str) -> Result<(bool, bool)> {
        // Get metrics for the token using DashMap's get() method
        let metrics = match TOKEN_METRICS.get(token_mint) {
            Some(metrics) => metrics.clone(),
            None => return Ok((false, false)), // No metrics, so nothing to sell
        };
        
        // Calculate time held
        let time_held = metrics.last_update.elapsed().as_secs();
        
        // Calculate percentage gain from entry (PNL)
        let pnl = if metrics.entry_price > 0.0 {
            (metrics.current_price - metrics.entry_price) / metrics.entry_price * 100.0
        } else {
            0.0
        };
        
        // Calculate percentage change from highest price
        let retracement = if metrics.highest_price > 0.0 {
            (metrics.highest_price - metrics.current_price) / metrics.highest_price * 100.0
        } else {
            0.0
        };
        
        // Log metrics with updated PNL terminology
        self.logger.log(format!(
            "Token: {}, Current Price: {:.8}, Entry: {:.8}, High: {:.8}, PNL: {:.2}%, Retracement: {:.2}%, Time Held: {}s",
            token_mint, metrics.current_price, metrics.entry_price, metrics.highest_price, pnl, retracement, time_held
        ).blue().to_string());
        
        // Log current dynamic trailing stop percentage
        let dynamic_trail_percentage = self.config.trailing_stop.get_trailing_stop_for_pnl(pnl);
        self.logger.log(format!(
            "🎯 Dynamic Trailing Stop: PNL {:.2}% → {:.0}% trail (activation at {:.0}%)",
            pnl, dynamic_trail_percentage, self.config.trailing_stop.activation_percentage
        ).cyan().to_string());
        
        // Check for dynamic whale selling based on PNL thresholds
        if let Some(whale_threshold) = self.config.dynamic_whale_selling.get_whale_threshold_for_pnl(pnl) {
            self.logger.log(format!(
                "🐋 Dynamic whale selling triggered: PNL {:.2}% >= {:.2}%, whale limit: {:.1} SOL",
                pnl, whale_threshold.pnl_threshold, whale_threshold.whale_limit_sol
            ).cyan().bold().to_string());
            
            return Ok((true, whale_threshold.use_emergency_zeroslot));
        }
        
        // Max Hold Time: Sell after 1 hour regardless of performance
        if time_held > self.config.max_hold_time {
            self.logger.log(format!("⏰ Selling due to max hold time exceeded: {}s > {}s (1 hour)", 
                             time_held, self.config.max_hold_time).yellow().to_string());
            return Ok((true, false)); // Not whale emergency
        }
        
        // Check if we've hit stop loss
        if pnl <= self.config.stop_loss {
            self.logger.log(format!("🛑 Selling due to stop loss triggered: {:.2}% <= {:.2}%", 
                             pnl, self.config.stop_loss).red().to_string());
            return Ok((true, false));
        }
        
        // Retracement logic: Apply when price drops from highest point (only if still profitable)
        if pnl > 0.0 && 
           retracement >= self.config.dynamic_whale_selling.retracement_percentage {
            self.logger.log(format!(
                "📉 Selling due to retracement: {:.2}% >= {:.2}% (PNL: {:.2}%, still profitable)",
                retracement, self.config.dynamic_whale_selling.retracement_percentage, 
                pnl
            ).yellow().to_string());
            return Ok((true, false));
        }
        
        // Standard take profit (fallback for lower PNL levels)
        if pnl >= self.config.take_profit {
            self.logger.log(format!("🎯 Selling due to take profit reached: {:.2}% >= {:.2}%", 
                             pnl, self.config.take_profit).green().to_string());
            return Ok((true, false));
        }

        // Enhanced liquidity monitoring
        if metrics.liquidity_at_current > 0.0 && metrics.liquidity_at_entry > 0.0 {
            let liquidity_drop = (metrics.liquidity_at_entry - metrics.liquidity_at_current) / metrics.liquidity_at_entry * 100.0;
            
            // Check absolute liquidity level
            if metrics.liquidity_at_current < self.config.liquidity_monitor.min_absolute_liquidity {
                self.logger.log(format!("💧 Selling due to low absolute liquidity: {:.2} SOL < {:.2} SOL", 
                                 metrics.liquidity_at_current, self.config.liquidity_monitor.min_absolute_liquidity).red().to_string());
                return Ok((true, false));
            }
            
            // Check liquidity drop percentage
            if liquidity_drop >= self.config.liquidity_monitor.max_acceptable_drop * 100.0 {
                self.logger.log(format!("💧 Selling due to liquidity drop: {:.2}% >= {:.2}%", 
                                 liquidity_drop, self.config.liquidity_monitor.max_acceptable_drop * 100.0).red().to_string());
                return Ok((true, false));
            }
        }
        
        // If we've reached here, no sell conditions met
        Ok((false, false))
    }
    

    
    /// Get the current price of a token
    async fn get_current_price(&self, token_mint: &str) -> Result<f64> {
        // Get the token metrics to determine which protocol to use
        let protocol = if let Some(metrics) = TOKEN_METRICS.get(token_mint) {
            metrics.protocol.clone()
        } else {
            self.app_state.protocol_preference.clone()
        };

        match protocol {
            SwapProtocol::PumpFun => {
                // Use cached current price from TOKEN_METRICS instead of RPC call
                if let Some(metrics) = TOKEN_METRICS.get(token_mint) {
                    Ok(metrics.current_price)
                } else {
                    Err(anyhow!("No metrics available for PumpFun token"))
                }
            },
            SwapProtocol::PumpSwap => {
                let pump_swap = PumpSwap::new(
                    self.app_state.wallet.clone(),
                    Some(self.app_state.rpc_client.clone()),
                    Some(self.app_state.rpc_nonblocking_client.clone()),
                );
                
                pump_swap.get_token_price(token_mint).await
            },
            SwapProtocol::RaydiumLaunchpad => {
                // For RaydiumLaunchpad, fall back to stored metrics price
                if let Some(metrics) = TOKEN_METRICS.get(token_mint) {
                    Ok(metrics.current_price)
                } else {
                    Err(anyhow!("No metrics available for Raydium token"))
                }
            },
            SwapProtocol::Auto | SwapProtocol::Unknown => {
                self.logger.log("Auto/Unknown protocol in get_current_price, using cached metrics".yellow().to_string());
                
                // Fall back to stored metrics for Auto/Unknown protocols
                if let Some(metrics) = TOKEN_METRICS.get(token_mint) {
                    Ok(metrics.current_price)
                } else {
                    Err(anyhow!("Unable to get price for Auto/Unknown protocol"))
                }
            }
        }
    }
    
    /// Check if this might be wash trading (self-trading, circular trades)
    pub fn check_wash_trading(&self, trade_info: &TradeInfoFromToken) -> Option<String> {
        // Check if creator is the same as pool (simplified wash trading check)
        if let Some(creator) = &trade_info.coin_creator {
            if !trade_info.pool_id.is_empty() && creator == &trade_info.pool_id {
                return Some("Possible wash trading (creator == pool)".to_string());
            }
        }
        
        // Check if price action looks manipulated
        // This is a simplified approach - in reality you'd need more sophisticated analysis
        let virtual_sol = trade_info.virtual_sol_reserves;
        let virtual_token = trade_info.virtual_token_reserves;
        
        if virtual_token != 0 {
            let expected_price = virtual_sol as f64 / virtual_token as f64;
            if let Some(current_price) = self.calculate_current_price(trade_info) {
                let price_diff = (current_price - expected_price).abs() / expected_price;
                
                if price_diff > 0.1 { // 10% difference
                    return Some(format!("Possible price manipulation: {:.2}% difference", price_diff * 100.0));
                }
            }
        }
        
        None
    }
    
    /// Check large holder actions
    pub fn check_large_holder_actions(&self, trade_info: &TradeInfoFromToken) -> Option<String> {
        // Check if this is a sell transaction from creator (simplified check)
        if let Some(_creator) = &trade_info.coin_creator {
            if !trade_info.is_buy {
                return Some("Creator sell transaction detected".to_string());
            }
        }
        
        // Check for large wallet movements
        let trade_size = self.calculate_trade_volume(trade_info)?;
        let liquidity = self.calculate_liquidity(trade_info)?;
        
        if trade_size > liquidity * 0.1 { // 10% of liquidity
            return Some(format!("Large trade size detected: {:.2} SOL ({:.2}% of liquidity)",
                trade_size, (trade_size / liquidity) * 100.0));
        }
        
        None
    }
    
    /// Adjust strategy based on market conditions
    pub fn adjust_strategy_based_on_market(&mut self, market_condition: MarketCondition) {
        self.logger.log(format!("Adjusting strategy for market condition: {:?}", market_condition));
        
        match market_condition {
            MarketCondition::Bullish => {
                // Be more aggressive in taking profits
                self.config.profit_taking.target_percentage *= 1.2;
                self.config.trailing_stop.activation_percentage *= 1.2;
                self.logger.log("Adjusted for bullish market: increased profit targets".green().to_string());
            },
            MarketCondition::Bearish => {
                // Take profits earlier
                self.config.profit_taking.target_percentage *= 0.8;
                self.config.trailing_stop.activation_percentage *= 0.8;
                self.logger.log("Adjusted for bearish market: reduced profit targets".yellow().to_string());
            },
            MarketCondition::Volatile => {
                // Use tighter stops
                self.config.trailing_stop.trail_percentage *= 0.5;
                self.logger.log("Adjusted for volatile market: tightened stops".yellow().to_string());
            },
            MarketCondition::Stable => {
                // Let winners run longer
                self.config.profit_taking.target_percentage *= 1.5;
                self.logger.log("Adjusted for stable market: letting winners run longer".green().to_string());
            }
        }
    }
    
    /// Record trade execution for analytics
    pub async fn record_trade_execution(
        &self, 
        mint: &str, 
        reason: &str, 
        amount_sold: f64, 
        protocol: &str
    ) -> Result<()> {
        // Get current timestamp
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| anyhow!("Failed to get timestamp: {}", e))?
            .as_secs();
        
        // Get entry price from metrics
        let entry_price = TOKEN_METRICS.get(mint)
            .map(|m| m.entry_price)
            .unwrap_or(0.0);
        
        // Get current price
        let exit_price = match self.get_current_price(mint).await {
            Ok(price) => price,
            Err(_) => 0.0,
        };
        
        // Calculate PNL
        let pnl = if entry_price > 0.0 {
            ((exit_price - entry_price) / entry_price) * 100.0
        } else {
            0.0
        };
        
        // Create record
        let record = TradeExecutionRecord {
            mint: mint.to_string(),
            entry_price,
            exit_price,
            pnl,
            reason: reason.to_string(),
            timestamp,
            amount_sold,
            protocol: protocol.to_string(),
        };
        
        // Log record
        self.logger.log(format!(
            "Trade execution recorded: {} sold at {:.8} SOL (PNL: {:.2}%)",
            mint, exit_price, pnl
        ).green().to_string());
        
        // Add to history using entry API
        HISTORICAL_TRADES.entry(()).and_modify(|history| {
            history.push_back(record.clone());
            
            // Keep history to a reasonable size
            if history.len() > 100 {
                history.pop_front();
            }
        });
        
        Ok(())
    }
   
    /// Convert TokenMetrics to a TradeInfoFromToken for analysis
    pub async fn metrics_to_trade_info(&self, token_mint: &str) -> Result<TradeInfoFromToken> {
        // Get metrics using DashMap's get() method
        let metrics = TOKEN_METRICS.get(token_mint)
            .ok_or_else(|| anyhow!("No metrics found for token {}", token_mint))?
            .clone();
        
        // Use the stored protocol instead of the passed one
        let protocol_to_use = metrics.protocol.clone();
        
        // Create timestamp
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| anyhow!("Failed to get current timestamp"))?
            .as_secs();
        
        // Calculate token amount for selling
        let token_amount = metrics.amount_held;
        
        // Convert to raw amount (assuming 9 decimals)
        let _raw_token_amount = (token_amount * 1_000_000_000.0) as u64;
        
        // Create a DexType based on protocol
        let dex_type = match protocol_to_use {
            SwapProtocol::PumpSwap => DexType::PumpSwap,
            SwapProtocol::PumpFun => DexType::PumpFun,
            SwapProtocol::RaydiumLaunchpad => DexType::RaydiumLaunchpad,
            SwapProtocol::Auto => {
                // For Auto protocol, default to PumpFun as it's most common
                self.logger.log("Auto protocol detected, defaulting to PumpFun".yellow().to_string());
                DexType::PumpFun
            },
            SwapProtocol::Unknown => {
                self.logger.log("Unknown protocol detected, defaulting to PumpFun".yellow().to_string());
                DexType::PumpFun
            },
        };

        // Get pool and necessary reserves information from the blockchain
        let (
            pool,
            _pool_info,
            pool_base_token_reserves,
            pool_quote_token_reserves,
            _sol_amount,
            coin_creator
        ) = match protocol_to_use {
            SwapProtocol::PumpSwap => {
                let pump_swap = PumpSwap::new(
                    self.app_state.wallet.clone(),
                    Some(self.app_state.rpc_client.clone()),
                    Some(self.app_state.rpc_nonblocking_client.clone()),
                );
                match pump_swap.get_pool_info(token_mint).await {
                    Ok((pool_id, base_mint, quote_mint, base_reserve, quote_reserve)) => {
                        // Convert price to estimated SOL amount
                        let est_sol_amount = (metrics.current_price * token_amount * 1_000_000_000.0) as u64;
                        
                        // Use default coin creator since we're selling (not needed for selling operations)
                        let default_coin_creator = Pubkey::default();

                        (
                            Some(pool_id.to_string()),
                            Some(PoolInfo {
                                pool_id,
                                base_mint,
                                quote_mint,
                                base_reserve,
                                quote_reserve,
                                coin_creator: default_coin_creator,
                            }),
                            Some(base_reserve),
                            Some(quote_reserve),
                            Some(est_sol_amount),
                            Some(default_coin_creator.to_string())
                        )
                    },
                    Err(e) => {
                        self.logger.log(format!("Failed to get pool info: {}", e).red().to_string());
                        (None, None, None, None, None, None)
                    }
                }
            },
            SwapProtocol::PumpFun => {
                // For PumpFun, use cached metrics instead of RPC calls
                // Calculate estimated SOL amount and use reasonable defaults for reserves
                let est_sol_amount = (metrics.current_price * token_amount * 1_000_000_000.0) as u64;
                
                // Since we don't have actual reserves data, use reasonable defaults
                let virtual_token_reserves = 1_000_000_000_000; // 1 trillion token units
                let virtual_sol_reserves = (virtual_token_reserves as f64 * metrics.current_price) as u64;
                
                (
                    None, // PumpFun doesn't use pool field
                    None, // No pool_info for PumpFun
                    Some(virtual_token_reserves),
                    Some(virtual_sol_reserves),
                    Some(est_sol_amount),
                    None  // We don't have creator info
                )
            },
            SwapProtocol::RaydiumLaunchpad => {
                // For RaydiumLaunchpad, we don't have a direct method to get pool info
                // Use reasonable defaults based on current metrics
                let est_sol_amount = (metrics.current_price * token_amount * 1_000_000_000.0) as u64;
                
                // Use defaults for Raydium Launchpad
                let virtual_token_reserves = 1_000_000_000_000; // 1 trillion token units
                let virtual_sol_reserves = (virtual_token_reserves as f64 * metrics.current_price) as u64;
                
                (
                    None, // No pool_id for Raydium Launchpad
                    None, // No pool_info for Raydium Launchpad
                    Some(virtual_token_reserves),
                    Some(virtual_sol_reserves),
                    Some(est_sol_amount),
                    None  // We don't have creator info
                )
            },
            SwapProtocol::Auto | SwapProtocol::Unknown => {
                // For Auto/Unknown protocols, use PumpFun defaults
                self.logger.log("Using PumpFun defaults for Auto/Unknown protocol".yellow().to_string());
                
                let est_sol_amount = (metrics.current_price * token_amount * 1_000_000_000.0) as u64;
                let virtual_token_reserves = 1_000_000_000_000; // 1 trillion token units
                let virtual_sol_reserves = (virtual_token_reserves as f64 * metrics.current_price) as u64;
                
                (
                    None, // PumpFun doesn't use pool field
                    None, // No pool_info for PumpFun
                    Some(virtual_token_reserves),
                    Some(virtual_sol_reserves),
                    Some(est_sol_amount),
                    None  // We don't have creator info
                )
            },
        };
        
        // Get user wallet to set as target
        let _wallet_pubkey = self.app_state.wallet.pubkey().to_string();
        
        // Create TradeInfoFromToken with as much real information as possible
        Ok(TradeInfoFromToken {
            dex_type,
            slot: 0, // Not critical for selling
            signature: "metrics_to_trade_info".to_string(),
            pool_id: pool.unwrap_or_default(),
            mint: token_mint.to_string(),
            timestamp,
            is_buy: false, // We're analyzing for sell
            price: (metrics.current_price * 1_000_000_000.0) as u64, // Convert to lamports
            is_reverse_when_pump_swap: false,
            coin_creator,
            sol_change: 0.0,
            token_change: token_amount,
            liquidity: pool_quote_token_reserves.unwrap_or(0) as f64 / 1_000_000_000.0,
            virtual_sol_reserves: pool_quote_token_reserves.unwrap_or(0),
            virtual_token_reserves: pool_base_token_reserves.unwrap_or(0),
        })
    }

  /// Analyze recent trades to determine market condition for dynamic strategy adjustment
    pub async fn analyze_market_condition(&self, recent_trades: &[TradeInfoFromToken]) -> MarketCondition {
        if recent_trades.is_empty() {
            return MarketCondition::Stable; // Default to stable if no data
        }
        
        // Extract prices from trades
        let mut prices: Vec<f64> = Vec::with_capacity(recent_trades.len());
        let mut volumes: Vec<f64> = Vec::with_capacity(recent_trades.len());
        let mut timestamps: Vec<u64> = Vec::with_capacity(recent_trades.len());
        
        for trade in recent_trades {
            // Calculate price from trade info
            if let Some(price) = self.calculate_current_price(trade) {
                prices.push(price);
            }
            
            // Extract volume
            if let Some(volume) = self.calculate_trade_volume(trade) {
                volumes.push(volume);
            }
            
            // Extract timestamp
            timestamps.push(trade.timestamp);
        }
        
        // Sort by timestamp to ensure chronological order
        let mut price_time_pairs: Vec<(u64, f64)> = timestamps.iter()
            .zip(prices.iter())
            .map(|(t, p)| (*t, *p))
            .collect();
        price_time_pairs.sort_by_key(|(t, _)| *t);
        
        // Re-extract sorted prices
        let sorted_prices: Vec<f64> = price_time_pairs.iter()
            .map(|(_, p)| *p)
            .collect();
        
        // Calculate time periods between price points
        // Convert to u64 during the mapping process
        let _time_periods: Vec<u64> = if price_time_pairs.len() >= 2 {
            price_time_pairs.windows(2)
                .map(|w| w[1].0.saturating_sub(w[0].0)) // Using timestamp differences
                .collect()
        } else {
            vec![0] // Default if not enough data
        };
        
        // Price volatility (std deviation / mean)
        let price_volatility = if !sorted_prices.is_empty() {
            let mean_price = sorted_prices.iter().sum::<f64>() / sorted_prices.len() as f64;
            let variance = sorted_prices.iter()
                .map(|p| (p - mean_price).powi(2))
                .sum::<f64>() / sorted_prices.len() as f64;
            (variance.sqrt() / mean_price).abs()
        } else {
            0.0
        };
        
        // Volume volatility
        let volume_volatility = if !volumes.is_empty() {
            let mean_volume = volumes.iter().sum::<f64>() / volumes.len() as f64;
            let variance = volumes.iter()
                .map(|v| (v - mean_volume).powi(2))
                .sum::<f64>() / volumes.len() as f64;
            (variance.sqrt() / mean_volume).abs()
        } else {
            0.0
        };
        
        // Price trend (positive = up, negative = down)
        let price_trend = if sorted_prices.len() >= 2 {
            (sorted_prices[sorted_prices.len() - 1] - sorted_prices[0]) / sorted_prices[0]
        } else {
            0.0
        };
        
        // Log analysis results
        self.logger.log(format!(
            "Market analysis: Volatility: {:.2}%, Trend: {:.2}%, Volume vol: {:.2}%",
            price_volatility * 100.0, price_trend * 100.0, volume_volatility * 100.0
        ).blue().to_string());
        
        // Determine market condition based on analysis
        if price_volatility > 0.15 {
            // High volatility market
            if price_trend > 0.05 {
                self.logger.log("Market condition: Bullish with high volatility".green().to_string());
                MarketCondition::Bullish
            } else if price_trend < -0.05 {
                self.logger.log("Market condition: Bearish with high volatility".red().to_string());
                MarketCondition::Bearish
            } else {
                self.logger.log("Market condition: Volatile with no clear trend".yellow().to_string());
                MarketCondition::Volatile
            }
        } else {
            // Low volatility market
            if price_trend > 0.05 {
                self.logger.log("Market condition: Stable uptrend".green().to_string());
                MarketCondition::Bullish
            } else if price_trend < -0.05 {
                self.logger.log("Market condition: Stable downtrend".red().to_string());
                MarketCondition::Bearish
            } else {
                self.logger.log("Market condition: Stable sideways".blue().to_string());
                MarketCondition::Stable
            }
        }
    }





    /// Unified emergency sell function for both whale and regular emergency selling
    /// This replaces both emergency_sell_all and execute_emergency_sell_via_engine
    /// 
    /// # Parameters
    /// - `token_mint`: The token to sell
    /// - `is_whale_emergency`: Whether to use whale emergency selling (higher slippage, faster execution)
    /// - `parsed_data`: Optional transaction data
    /// - `protocol`: Optional protocol preference
    /// 
    /// # Returns
    /// - `Ok(signature)`: Transaction signature on success
    /// - `Err(error)`: Error message on failure
    pub async fn unified_emergency_sell(&self, token_mint: &str, is_whale_emergency: bool, parsed_data: Option<&TradeInfoFromToken>, protocol: Option<SwapProtocol>) -> Result<String> {
        // Add timeout to prevent hanging
        use tokio::time::{timeout, Duration};
        
        let timeout_duration = if is_whale_emergency { 
            Duration::from_secs(30) // Shorter timeout for whale emergency sells
        } else { 
            Duration::from_secs(60) // Standard timeout for regular sells
        };
        
        let result = timeout(timeout_duration, self.execute_emergency_sell_internal(token_mint, is_whale_emergency, parsed_data, protocol)).await;
        
        match result {
            Ok(inner_result) => inner_result,
            Err(_timeout_err) => {
                self.logger.log(format!("🚨 Emergency sell timed out for token: {} ({}s timeout)", token_mint, timeout_duration.as_secs()).red().bold().to_string());
                Err(anyhow!("Emergency sell operation timed out after {}s", timeout_duration.as_secs()))
            }
        }
    }
    
    /// Internal implementation of emergency sell without timeout wrapper
    async fn execute_emergency_sell_internal(&self, token_mint: &str, is_whale_emergency: bool, parsed_data: Option<&TradeInfoFromToken>, protocol: Option<SwapProtocol>) -> Result<String> {
        // Log the type of emergency sell
        if is_whale_emergency {
            self.logger.log(format!("🐋 WHALE EMERGENCY SELL triggered for token: {}", token_mint).red().bold().to_string());
        } else {
            self.logger.log(format!("🚨 REGULAR EMERGENCY SELL triggered for token: {}", token_mint).red().bold().to_string());
        }
        
        // Get wallet pubkey
        let wallet_pubkey = self.app_state.wallet.try_pubkey()
            .map_err(|e| anyhow!("Failed to get wallet pubkey: {}", e))?;

        // Get token account to determine how much we own
        let token_pubkey = Pubkey::from_str(token_mint)
            .map_err(|e| anyhow!("Invalid token mint address: {}", e))?;
        let ata = get_associated_token_address(&wallet_pubkey, &token_pubkey);

        // Get current token balance
        let token_amount = match self.app_state.rpc_nonblocking_client.get_token_account(&ata).await {
            Ok(Some(account)) => {
                let amount_value = account.token_amount.amount.parse::<f64>()
                    .map_err(|e| anyhow!("Failed to parse token amount: {}", e))?;
                let decimal_amount = amount_value / 10f64.powi(account.token_amount.decimals as i32);
                self.logger.log(format!("Emergency selling {} tokens", decimal_amount).red().to_string());
                decimal_amount
            },
            Ok(None) => {
                return Err(anyhow!("No token account found for mint: {}", token_mint));
            },
            Err(e) => {
                return Err(anyhow!("Failed to get token account: {}", e));
            }
        };

        if token_amount <= 0.0 {
            self.logger.log("No tokens to sell".yellow().to_string());
            return Ok("no_tokens_to_sell".to_string());
        }

        // Determine protocol - use provided protocol or get from metrics
        let sell_protocol = protocol.unwrap_or_else(|| {
            if let Some(metrics) = crate::processor::selling_strategy::TOKEN_METRICS.get(token_mint) {
                metrics.protocol.clone()
            } else {
                SwapProtocol::PumpFun // Default fallback
            }
        });

        // Create emergency sell config with high slippage tolerance
        let mut emergency_config = (*self.swap_config).clone();
        emergency_config.swap_direction = SwapDirection::Sell;
        emergency_config.in_type = SwapInType::Pct; // Use percentage for emergency sells
        emergency_config.amount_in = 1.0; // Sell 100% of tokens
        // Use higher slippage for whale emergency sells for faster execution
        emergency_config.slippage = if is_whale_emergency { 1500 } else { 1000 }; // 15% vs 10% slippage

        // Create or use provided trade info
        let emergency_trade_info = if let Some(data) = parsed_data {
            // Use provided parsed data
            TradeInfoFromToken {
                dex_type: match sell_protocol {
                    SwapProtocol::PumpSwap => crate::processor::transaction_parser::DexType::PumpSwap,
                    SwapProtocol::PumpFun => crate::processor::transaction_parser::DexType::PumpFun,
                    SwapProtocol::RaydiumLaunchpad => crate::processor::transaction_parser::DexType::RaydiumLaunchpad,
                    _ => crate::processor::transaction_parser::DexType::Unknown,
                },
                slot: data.slot,
                signature: if is_whale_emergency { "whale_emergency_sell" } else { "regular_emergency_sell" }.to_string(),
                pool_id: data.pool_id.clone(),
                mint: token_mint.to_string(),
                timestamp: data.timestamp,
                is_buy: false,
                price: data.price,
                is_reverse_when_pump_swap: data.is_reverse_when_pump_swap,
                coin_creator: data.coin_creator.clone(),
                sol_change: data.sol_change,
                token_change: token_amount,
                liquidity: data.liquidity,
                virtual_sol_reserves: data.virtual_sol_reserves,
                virtual_token_reserves: data.virtual_token_reserves,
            }
        } else {
            // Create trade info from metrics (for execute_emergency_sell_via_engine replacement)
            match self.metrics_to_trade_info(token_mint).await {
                Ok(trade_info) => {
                    let mut emergency_trade_info = trade_info;
                    emergency_trade_info.signature = if is_whale_emergency { "whale_emergency_sell" } else { "metrics_emergency_sell" }.to_string();
                    emergency_trade_info.token_change = token_amount;
                    emergency_trade_info.is_buy = false;
                    emergency_trade_info
                },
                Err(e) => {
                    return Err(anyhow!("Failed to create trade info from metrics: {}", e));
                }
            }
        };

        // Protocol string for logging
        let protocol_str = match sell_protocol {
            SwapProtocol::PumpSwap => "PumpSwap",
            SwapProtocol::PumpFun => "PumpFun",
            SwapProtocol::RaydiumLaunchpad => "RaydiumLaunchpad",
            _ => "Unknown",
        };

        // Execute emergency sell based on protocol
        let result = match sell_protocol {
            SwapProtocol::PumpFun => {
                self.logger.log("Using PumpFun protocol for emergency sell".red().to_string());
                
                let pump = crate::dex::pump_fun::Pump::new(
                    self.app_state.rpc_nonblocking_client.clone(),
                    self.app_state.rpc_client.clone(),
                    self.app_state.wallet.clone(),
                );
                
                match pump.build_swap_from_parsed_data(&emergency_trade_info, emergency_config).await {
                    Ok((keypair, instructions, price)) => {
                        // Get recent blockhash from the processor
                        let recent_blockhash = match crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                            Some(hash) => hash,
                            None => {
                                self.logger.log("Failed to get recent blockhash".red().to_string());
                                return Err(anyhow!("Failed to get recent blockhash"));
                            }
                        };
                        self.logger.log(format!("Generated emergency PumpFun sell instruction at price: {}", price));
                        // Execute with zeroslot for copy selling
                        match crate::block_engine::tx::new_signed_and_send_zeroslot(
                            self.app_state.zeroslot_rpc_client.clone(),
                            recent_blockhash,
                            &keypair,
                            instructions,
                            &self.logger,
                        ).await {
                            Ok(signatures) => {
                                if signatures.is_empty() {
                                    return Err(anyhow!("No transaction signature returned"));
                                }
                                
                                let signature = &signatures[0];
                                self.logger.log(format!("Emergency PumpFun sell transaction sent: {}", signature).green().to_string());
                                
                                Ok(signature.to_string())
                            },
                            Err(e) => {
                                self.logger.log(format!("Emergency sell transaction failed: {}", e).red().to_string());
                                Err(anyhow!("Failed to send emergency sell transaction: {}", e))
                            }
                        }
                    },
                    Err(e) => {
                        self.logger.log(format!("Failed to build emergency PumpFun sell instruction: {}", e).red().to_string());
                        Err(anyhow!("Failed to build emergency sell instruction: {}", e))
                    }
                }
            },
            SwapProtocol::PumpSwap => {
                self.logger.log("Using PumpSwap protocol for emergency sell".red().to_string());
                
                let pump_swap = crate::dex::pump_swap::PumpSwap::new(
                    self.app_state.wallet.clone(),
                    Some(self.app_state.rpc_client.clone()),
                    Some(self.app_state.rpc_nonblocking_client.clone()),
                );
                
                match pump_swap.build_swap_from_parsed_data(&emergency_trade_info, emergency_config).await {
                    Ok((keypair, instructions, price)) => {
                        // Get recent blockhash from the processor
                        let recent_blockhash = match crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                            Some(hash) => hash,
                            None => {
                                self.logger.log("Failed to get recent blockhash".red().to_string());
                                return Err(anyhow!("Failed to get recent blockhash"));
                            }
                        };
                        self.logger.log(format!("Generated emergency PumpSwap sell instruction at price: {}", price));
                        // Execute with zeroslot for copy selling
                        match crate::block_engine::tx::new_signed_and_send_zeroslot(
                            self.app_state.zeroslot_rpc_client.clone(),
                            recent_blockhash,
                            &keypair,
                            instructions,
                            &self.logger,
                        ).await {
                            Ok(signatures) => {
                                if signatures.is_empty() {
                                    return Err(anyhow!("No transaction signature returned"));
                                }
                                
                                let signature = &signatures[0];
                                self.logger.log(format!("Emergency PumpSwap sell transaction sent: {}", signature).green().to_string());
                                
                                Ok(signature.to_string())
                            },
                            Err(e) => {
                                self.logger.log(format!("Emergency sell transaction failed: {}", e).red().to_string());
                                Err(anyhow!("Failed to send emergency sell transaction: {}", e))
                            }
                        }
                    },
                    Err(e) => {
                        self.logger.log(format!("Failed to build emergency PumpSwap sell instruction: {}", e).red().to_string());
                        Err(anyhow!("Failed to build emergency sell instruction: {}", e))
                    }
                }
            },
            SwapProtocol::RaydiumLaunchpad => {
                self.logger.log("Using Raydium protocol for emergency sell".red().to_string());
                
                let raydium = crate::dex::raydium_launchpad::Raydium::new(
                    self.app_state.wallet.clone(),
                    Some(self.app_state.rpc_client.clone()),
                    Some(self.app_state.rpc_nonblocking_client.clone()),
                );
                
                match raydium.build_swap_from_parsed_data(&emergency_trade_info, emergency_config).await {
                    Ok((keypair, instructions, price)) => {
                        // Get recent blockhash from the processor
                        let recent_blockhash = match crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                            Some(hash) => hash,
                            None => {
                                self.logger.log("Failed to get recent blockhash".red().to_string());
                                return Err(anyhow!("Failed to get recent blockhash"));
                            }
                        };
                        self.logger.log(format!("Generated emergency Raydium sell instruction at price: {}", price));
                        // Execute with zeroslot for copy selling
                        match crate::block_engine::tx::new_signed_and_send_zeroslot(
                            self.app_state.zeroslot_rpc_client.clone(),
                            recent_blockhash,
                            &keypair,
                            instructions,
                            &self.logger,
                        ).await {
                            Ok(signatures) => {
                                if signatures.is_empty() {
                                    return Err(anyhow!("No transaction signature returned"));
                                }
                                
                                let signature = &signatures[0];
                                self.logger.log(format!("Emergency Raydium sell transaction sent: {}", signature).green().to_string());
                                
                                Ok(signature.to_string())
                            },
                            Err(e) => {
                                self.logger.log(format!("Emergency sell transaction failed: {}", e).red().to_string());
                                Err(anyhow!("Failed to send emergency sell transaction: {}", e))
                            }
                        }
                    },
                    Err(e) => {
                        self.logger.log(format!("Failed to build emergency Raydium sell instruction: {}", e).red().to_string());
                        Err(anyhow!("Failed to build emergency sell instruction: {}", e))
                    }
                }
            },
            SwapProtocol::Auto | SwapProtocol::Unknown => {
                self.logger.log("Auto/Unknown protocol detected, defaulting to PumpFun for emergency sell".yellow().to_string());
                
                let pump = crate::dex::pump_fun::Pump::new(
                    self.app_state.rpc_nonblocking_client.clone(),
                    self.app_state.rpc_client.clone(),
                    self.app_state.wallet.clone(),
                );
                
                match pump.build_swap_from_parsed_data(&emergency_trade_info, emergency_config).await {
                    Ok((keypair, instructions, price)) => {
                        let recent_blockhash = match crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                            Some(hash) => hash,
                            None => {
                                self.logger.log("Failed to get recent blockhash".red().to_string());
                                return Err(anyhow!("Failed to get recent blockhash"));
                            }
                        };
                        self.logger.log(format!("Generated emergency PumpFun sell instruction at price: {}", price));
                        match crate::block_engine::tx::new_signed_and_send_zeroslot(
                            self.app_state.zeroslot_rpc_client.clone(),
                            recent_blockhash,
                            &keypair,
                            instructions,
                            &self.logger,
                        ).await {
                            Ok(signatures) => {
                                if signatures.is_empty() {
                                    return Err(anyhow!("No transaction signature returned"));
                                }
                                
                                let signature = &signatures[0];
                                self.logger.log(format!("Emergency PumpFun (Auto/Unknown) sell transaction sent: {}", signature).green().to_string());
                                
                                Ok(signature.to_string())
                            },
                            Err(e) => {
                                self.logger.log(format!("Emergency sell transaction failed: {}", e).red().to_string());
                                Err(anyhow!("Failed to send emergency sell transaction: {}", e))
                            }
                        }
                    },
                    Err(e) => {
                        self.logger.log(format!("Failed to build emergency PumpFun sell instruction: {}", e).red().to_string());
                        Err(anyhow!("Failed to build emergency sell instruction: {}", e))
                    }
                }
            },
        };

        // If DEX selling failed, try Jupiter API as fallback
        let final_result = match result {
            Ok(signature) => {
                self.logger.log(format!("✅ DEX emergency sell successful: {}", signature).green().to_string());
                Ok(signature)
            },
            Err(dex_error) => {
                self.logger.log(format!("❌ DEX emergency sell failed: {}. Attempting Jupiter API fallback...", dex_error).yellow().to_string());
                
                // Try Jupiter API as fallback
                match self.try_jupiter_fallback_sell(token_mint, token_amount).await {
                    Ok(jupiter_signature) => {
                        self.logger.log(format!("✅ Jupiter API fallback sell successful: {}", jupiter_signature).green().to_string());
                        Ok(jupiter_signature)
                    },
                    Err(jupiter_error) => {
                        self.logger.log(format!("❌ Jupiter API fallback also failed: {}. All selling methods exhausted.", jupiter_error).red().to_string());
                        Err(anyhow!("Both DEX and Jupiter selling failed. DEX error: {}. Jupiter error: {}", dex_error, jupiter_error))
                    }
                }
            }
        };

        // Update metrics after emergency sell
        if final_result.is_ok() {
            // Note: Keeping token account in WALLET_TOKEN_ACCOUNTS for potential future use
            // The ATA may be empty but could be used again later
            
            // Record the emergency trade execution
            if let Err(e) = self.record_trade_execution(
                token_mint,
                "EMERGENCY_STOP_LOSS",
                token_amount,
                protocol_str
            ).await {
                self.logger.log(format!("Failed to record emergency trade execution: {}", e).red().to_string());
            }
        }

        final_result
    }

    /// Try Jupiter API as fallback when DEX selling fails
    async fn try_jupiter_fallback_sell(&self, token_mint: &str, token_amount: f64) -> Result<String> {
        self.logger.log(format!("🌌 Attempting Jupiter API fallback sell for {} tokens of {}", token_amount, token_mint).cyan().to_string());
        
        // Create Jupiter client
        let jupiter_client = crate::library::jupiter_api::JupiterClient::new(
            self.app_state.rpc_nonblocking_client.clone()
        );
        
        // Get token account to get the correct amount in raw units
        let wallet_pubkey = self.app_state.wallet.try_pubkey()
            .map_err(|e| anyhow!("Failed to get wallet pubkey: {}", e))?;
        let token_pubkey = Pubkey::from_str(token_mint)
            .map_err(|e| anyhow!("Invalid token mint address: {}", e))?;
        let ata = get_associated_token_address(&wallet_pubkey, &token_pubkey);

        // Get current token balance in raw units for Jupiter
        let raw_token_amount = match self.app_state.rpc_nonblocking_client.get_token_account(&ata).await {
            Ok(Some(account)) => {
                account.token_amount.amount.parse::<u64>()
                    .map_err(|e| anyhow!("Failed to parse token amount: {}", e))?
            },
            Ok(None) => {
                return Err(anyhow!("No token account found for mint: {}", token_mint));
            },
            Err(e) => {
                return Err(anyhow!("Failed to get token account: {}", e));
            }
        };

        if raw_token_amount == 0 {
            return Err(anyhow!("No tokens to sell"));
        }

        // Use 500 basis points (5%) slippage for Jupiter emergency sells
        let slippage_bps = 500;
        
        self.logger.log(format!("Jupiter selling {} raw tokens with {}bps slippage", raw_token_amount, slippage_bps).cyan().to_string());
        
        // Execute Jupiter sell
        match jupiter_client.sell_token_with_jupiter(
            token_mint,
            raw_token_amount,
            slippage_bps,
            &self.app_state.wallet,
        ).await {
            Ok(signature) => {
                self.logger.log(format!("🌌 Jupiter API sell completed: {}", signature).green().to_string());
                Ok(signature)
            },
            Err(e) => {
                self.logger.log(format!("🌌 Jupiter API sell failed: {}", e).red().to_string());
                Err(anyhow!("Jupiter API sell failed: {}", e))
            }
        }
    }

    pub async fn check_time_conditions(&self, trade_info: &TradeInfoFromToken) -> Option<String> {
        // Get current timestamp
        let current_timestamp = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(n) => n.as_secs(),
            Err(_) => return None,
        };
        
        // Get metrics using DashMap's get() method
        let metrics = TOKEN_METRICS.get(&trade_info.mint)?;
        
        // Calculate time held
        let time_held_seconds = if metrics.buy_timestamp > 0 {
            current_timestamp.saturating_sub(metrics.buy_timestamp)
        } else {
            metrics.time_held
        };
        
        // Check max hold time
        if time_held_seconds >= self.config.time_based.max_hold_time_secs {
            return Some(format!(
                "Max hold time exceeded ({} seconds)", 
                time_held_seconds
            ));
        }
        
        // Calculate current PNL
        let pnl = if metrics.entry_price > 0.0 {
            ((metrics.current_price - metrics.entry_price) / metrics.entry_price) * 100.0
        } else {
            0.0
        };
        
        // Check min profit time only for profitable positions
        if pnl > 0.0 && time_held_seconds < self.config.time_based.min_profit_time_secs {
            return None; // Don't sell yet if profitable but haven't held long enough
        }
        
        None
    }

    // Fix calculate_current_price -> get_current_price
    pub fn calculate_current_price(&self, trade_info: &TradeInfoFromToken) -> Option<f64> {
        // For RaydiumLaunchpad and other DEXes with pre-calculated prices, use the parser's calculation
        match trade_info.dex_type {
            DexType::RaydiumLaunchpad => {
                // Use the price calculated by the parser (already scaled correctly)
                if trade_info.price > 0 {
                    Some(trade_info.price as f64 / 1_000_000_000.0)
                } else {
                    None
                }
            },
            DexType::PumpSwap => {
                // Use the price calculated by the parser
                if trade_info.price > 0 {
                    Some(trade_info.price as f64 / 1_000_000_000.0)
                } else {
                    None
                }
            },
            DexType::PumpFun | _ => {
                // For PumpFun and others, fall back to virtual reserves calculation
                let virtual_sol = trade_info.virtual_sol_reserves;
                let virtual_token = trade_info.virtual_token_reserves;
                
                if virtual_token != 0 {
                    // Apply same scaling as transaction parser and PumpFun methods
                    // But return as f64 for selling strategy calculations
                    let scaled_price = ((virtual_sol as f64) * 1_000_000_000.0) / (virtual_token as f64);
                    Some(scaled_price / 1_000_000_000.0) // Convert back to unscaled f64
                } else {
                    None
                }
            }
        }
    }

    // Add missing methods
    pub async fn check_liquidity_conditions(&self, trade_info: &TradeInfoFromToken) -> Option<String> {
        let liquidity = self.calculate_liquidity(trade_info)?;
        if liquidity < self.config.liquidity_monitor.min_absolute_liquidity {
            Some(format!("Low liquidity: {} SOL", liquidity))
        } else {
            None
        }
    }

    pub async fn check_volume_conditions(&self, trade_info: &TradeInfoFromToken) -> Option<String> {
        let volume = self.calculate_trade_volume(trade_info)?;
        let avg_volume = self.get_average_volume(trade_info.mint.as_str()).await?;
        
        if volume < avg_volume * self.config.volume_analysis.drop_threshold {
            Some(format!("Volume too low: {:.2}x average", volume / avg_volume))
        } else {
            None
        }
    }

    pub async fn check_price_conditions(&self, trade_info: &TradeInfoFromToken) -> Option<String> {
        let current_price = self.calculate_current_price(trade_info)?;
        let metrics = self.token_manager.get_token_metrics(&trade_info.mint).await?;
        
        let gain = (current_price - metrics.entry_price) / metrics.entry_price * 100.0;
        let retracement = (metrics.highest_price - current_price) / metrics.highest_price * 100.0;
        
        // Dynamic trailing stop logic based on PnL
        let current_pnl = gain;
        let dynamic_trail_percentage = self.config.trailing_stop.get_trailing_stop_for_pnl(current_pnl);
        
        // Apply dynamic trailing stop if we're in profit and above activation threshold
        if current_pnl >= self.config.trailing_stop.activation_percentage {
            if retracement >= dynamic_trail_percentage {
                self.logger.log(format!(
                    "🎯 Dynamic trailing stop triggered: PnL {:.2}% → Trail {:.2}% → Retracement {:.2}%",
                    current_pnl, dynamic_trail_percentage, retracement
                ).cyan().to_string());
                return Some(format!("Dynamic trailing stop: {:.2}% retracement (trail: {:.2}%)", 
                           retracement, dynamic_trail_percentage));
            }
        }
        
        // Traditional retracement check (fallback)
        if retracement > self.config.retracement_threshold {
            Some(format!("Price retracement: {:.2}%", retracement))
        } else if gain < self.config.stop_loss {
            Some(format!("Stop loss hit: {:.2}%", gain))
        } else {
            None
        }
    }

    pub fn calculate_trade_volume(&self, trade_info: &TradeInfoFromToken) -> Option<f64> {
        // Calculate from sol_change 
        let amount = (trade_info.sol_change.abs() * 1_000_000_000.0) as u64;
        
        Some(amount as f64 / 1e9) // Convert lamports to SOL
    }

    pub fn calculate_liquidity(&self, trade_info: &TradeInfoFromToken) -> Option<f64> {
        let sol_reserves = trade_info.virtual_sol_reserves;
        Some(sol_reserves as f64 / 1e9) // Convert lamports to SOL
    }

    pub async fn get_average_volume(&self, token_mint: &str) -> Option<f64> {
        let metrics = self.token_manager.get_token_metrics(token_mint).await?;
        if metrics.volume_history.is_empty() {
            return None;
        }
        
        let sum: f64 = metrics.volume_history.iter().sum();
        Some(sum / metrics.volume_history.len() as f64)
    }

    // Add send_priority_transaction method
    pub async fn send_priority_transaction(
        &self,
        recent_blockhash: Hash,
        keypair: &Keypair,
        instructions: Vec<Instruction>,
    ) -> Result<Signature> {
        use solana_sdk::transaction::Transaction;
        
        // Create transaction
        let mut tx = Transaction::new_with_payer(&instructions, Some(&keypair.pubkey()));
        tx.sign(&[keypair], recent_blockhash);
        
        // Send with max priority
        match self.app_state.rpc_nonblocking_client.send_and_confirm_transaction_with_spinner(&tx).await {
            Ok(signature) => Ok(signature),
            Err(e) => Err(anyhow::anyhow!("Failed to send priority transaction: {}", e)),
        }
    }

    pub async fn get_active_tokens_count(&self) -> usize {
        self.token_manager.get_active_tokens_count().await
    }


}
