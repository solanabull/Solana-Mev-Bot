//! Priority fee management for Solana transactions
//!
//! Manages dynamic priority fee calculation and adjustment based on
//! network congestion and transaction urgency.

use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::RwLock;
use solana_client::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;

/// Recent fee data point
#[derive(Debug, Clone)]
pub struct FeeDataPoint {
    pub slot: u64,
    pub fee: u64,
    pub timestamp: u64,
}

/// Priority fee manager for dynamic fee calculation
#[derive(Debug)]
pub struct PriorityFeeManager {
    recent_fees: Arc<RwLock<VecDeque<FeeDataPoint>>>,
    max_history_size: usize,
    base_fee: u64,
    rpc_client: Arc<RpcClient>,
}

impl PriorityFeeManager {
    /// Create new priority fee manager
    pub fn new(rpc_client: Arc<RpcClient>, max_history: usize, base_fee: u64) -> Self {
        Self {
            recent_fees: Arc::new(RwLock::new(VecDeque::with_capacity(max_history))),
            max_history_size: max_history,
            base_fee,
            rpc_client,
        }
    }

    /// Update fee history with recent data
    pub async fn update_fee_history(&self) -> Result<(), Box<dyn std::error::Error>> {
        // Get recent blocks with their fees
        let recent_blockhash = self.rpc_client.get_recent_blockhash()?.0;

        // In a real implementation, you'd fetch recent priority fees from blocks
        // For now, we'll simulate with some fee data
        let current_slot = self.rpc_client.get_slot()?;
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();

        // Simulate fee data (in production, parse from block data)
        let simulated_fee = self.base_fee + (current_slot % 1000) as u64 * 100;

        let fee_point = FeeDataPoint {
            slot: current_slot,
            fee: simulated_fee,
            timestamp,
        };

        let mut fees = self.recent_fees.write().await;
        fees.push_back(fee_point);

        // Maintain history size
        while fees.len() > self.max_history_size {
            fees.pop_front();
        }

        Ok(())
    }

    /// Calculate optimal priority fee for given parameters
    pub async fn calculate_optimal_fee(
        &self,
        target_percentile: f64,
        min_fee: u64,
        urgency_multiplier: f64,
    ) -> u64 {
        let fees = self.recent_fees.read().await;

        if fees.is_empty() {
            return (self.base_fee as f64 * urgency_multiplier) as u64;
        }

        let mut fee_values: Vec<u64> = fees.iter().map(|f| f.fee).collect();
        fee_values.sort();

        let index = ((fee_values.len() - 1) as f64 * target_percentile) as usize;
        let percentile_fee = fee_values.get(index).copied().unwrap_or(self.base_fee);

        let optimal_fee = (percentile_fee as f64 * urgency_multiplier) as u64;
        optimal_fee.max(min_fee)
    }

    /// Get fee statistics
    pub async fn get_fee_stats(&self) -> FeeStatistics {
        let fees = self.recent_fees.read().await;

        if fees.is_empty() {
            return FeeStatistics {
                count: 0,
                mean: 0.0,
                median: 0.0,
                p95: 0.0,
                min: 0,
                max: 0,
            };
        }

        let mut fee_values: Vec<u64> = fees.iter().map(|f| f.fee).collect();
        fee_values.sort();

        let count = fee_values.len();
        let sum: u64 = fee_values.iter().sum();
        let mean = sum as f64 / count as f64;

        let median = if count % 2 == 0 {
            (fee_values[count / 2 - 1] + fee_values[count / 2]) as f64 / 2.0
        } else {
            fee_values[count / 2] as f64
        };

        let p95_index = ((count - 1) as f64 * 0.95) as usize;
        let p95 = fee_values[p95_index] as f64;

        FeeStatistics {
            count,
            mean,
            median,
            p95,
            min: *fee_values.first().unwrap_or(&0),
            max: *fee_values.last().unwrap_or(&0),
        }
    }

    /// Calculate urgency multiplier based on profit and time sensitivity
    pub fn calculate_urgency_multiplier(
        profit_usd: f64,
        time_to_expiry: Option<u64>, // seconds
        competition_level: f64, // 0.0 to 1.0
    ) -> f64 {
        let mut multiplier = 1.0;

        // Profit-based urgency
        multiplier *= match profit_usd {
            p if p < 0.1 => 0.8,
            p if p < 1.0 => 1.0,
            p if p < 10.0 => 1.5,
            p if p < 100.0 => 2.0,
            _ => 3.0,
        };

        // Time-based urgency
        if let Some(ttl) = time_to_expiry {
            multiplier *= match ttl {
                t if t < 10 => 2.0,  // Very urgent
                t if t < 30 => 1.5,
                t if t < 60 => 1.2,
                _ => 1.0,
            };
        }

        // Competition-based urgency
        multiplier *= 1.0 + competition_level * 0.5;

        multiplier
    }

    /// Predict fee for next slot based on trend
    pub async fn predict_next_fee(&self) -> u64 {
        let fees = self.recent_fees.read().await;

        if fees.len() < 2 {
            return self.base_fee;
        }

        // Simple linear regression on recent fees
        let recent_fees: Vec<_> = fees.iter().rev().take(10).collect();
        let n = recent_fees.len() as f64;

        if n < 2.0 {
            return self.base_fee;
        }

        let sum_x: f64 = (0..recent_fees.len()).map(|i| i as f64).sum();
        let sum_y: f64 = recent_fees.iter().map(|f| f.fee as f64).sum();
        let sum_xy: f64 = recent_fees.iter().enumerate()
            .map(|(i, f)| i as f64 * f.fee as f64).sum();
        let sum_xx: f64 = (0..recent_fees.len()).map(|i| i as f64 * i as f64).sum();

        let slope = (n * sum_xy - sum_x * sum_y) / (n * sum_xx - sum_x * sum_x);
        let intercept = (sum_y - slope * sum_x) / n;

        // Predict next value
        let next_x = recent_fees.len() as f64;
        let predicted = slope * next_x + intercept;

        predicted.max(self.base_fee as f64) as u64
    }
}

/// Fee statistics for monitoring
#[derive(Debug, Clone)]
pub struct FeeStatistics {
    pub count: usize,
    pub mean: f64,
    pub median: f64,
    pub p95: f64,
    pub min: u64,
    pub max: u64,
}

/// Fee strategy for different transaction types
#[derive(Debug, Clone)]
pub enum FeeStrategy {
    Conservative,     // Use lower percentile, slower but cheaper
    Balanced,         // Use median fees, balanced speed/cost
    Aggressive,       // Use higher percentile, faster but expensive
    Dynamic,          // Adjust based on recent network conditions
}

impl FeeStrategy {
    pub fn target_percentile(&self) -> f64 {
        match self {
            FeeStrategy::Conservative => 0.25,
            FeeStrategy::Balanced => 0.50,
            FeeStrategy::Aggressive => 0.75,
            FeeStrategy::Dynamic => 0.60, // Will be adjusted dynamically
        }
    }

    pub fn urgency_base(&self) -> f64 {
        match self {
            FeeStrategy::Conservative => 0.8,
            FeeStrategy::Balanced => 1.0,
            FeeStrategy::Aggressive => 1.5,
            FeeStrategy::Dynamic => 1.0,
        }
    }
}
