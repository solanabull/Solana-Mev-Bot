//! Fee calculation utilities for Solana transactions
//!
//! Handles priority fees, DEX fees, and network fee optimization.

use rust_decimal::Decimal;
use solana_sdk::native_token::LAMPORTS_PER_SOL;

/// Fee structure for transaction cost analysis
#[derive(Debug, Clone)]
pub struct TransactionFees {
    pub network_fee: u64,
    pub priority_fee: u64,
    pub dex_fee: u64,
    pub jito_tip: u64,
    pub total: u64,
}

/// Calculate network fee for a transaction
pub fn calculate_network_fee(
    signatures: usize,
    write_accounts: usize,
    data_bytes: usize,
) -> u64 {
    // Base fee per signature
    let signature_fee = signatures as u64 * 5000; // 5000 lamports per signature

    // Account rent and data fees are paid by the runtime
    // This is a simplified calculation

    signature_fee
}

/// Calculate optimal priority fee based on recent blocks
pub fn calculate_priority_fee(
    recent_fees: &[u64],
    target_percentile: f64,
    base_fee: u64,
) -> u64 {
    if recent_fees.is_empty() {
        return base_fee;
    }

    let mut sorted_fees = recent_fees.to_vec();
    sorted_fees.sort();

    let index = ((sorted_fees.len() - 1) as f64 * target_percentile) as usize;
    let percentile_fee = sorted_fees[index];

    base_fee.max(percentile_fee)
}

/// Calculate DEX trading fee
pub fn calculate_dex_fee(amount: u64, fee_bps: u16) -> u64 {
    let amount_dec = Decimal::from(amount);
    let fee_dec = Decimal::new(fee_bps as i64, 4); // Convert bps to decimal

    let fee_amount = amount_dec * fee_dec;
    fee_amount.to_u64().unwrap_or(0)
}

/// Calculate Jito tip based on bundle size and priority
pub fn calculate_jito_tip(
    bundle_size: usize,
    priority_level: PriorityLevel,
    base_tip: u64,
) -> u64 {
    let multiplier = match priority_level {
        PriorityLevel::Low => 1.0,
        PriorityLevel::Medium => 1.5,
        PriorityLevel::High => 2.0,
        PriorityLevel::Urgent => 3.0,
    };

    let bundle_multiplier = match bundle_size {
        1 => 1.0,
        2 => 1.2,
        3..=5 => 1.5,
        _ => 2.0,
    };

    ((base_tip as f64 * multiplier * bundle_multiplier) as u64).max(1000) // Minimum 1000 lamports
}

/// Estimate total transaction cost
pub fn estimate_total_cost(fees: &TransactionFees) -> u64 {
    fees.network_fee + fees.priority_fee + fees.dex_fee + fees.jito_tip
}

/// Check if profit exceeds total fees
pub fn is_profitable_after_fees(
    gross_profit: u64,
    fees: &TransactionFees,
    min_profit_margin: f64,
) -> bool {
    let total_fees = estimate_total_cost(fees);
    let net_profit = gross_profit.saturating_sub(total_fees);

    if total_fees == 0 {
        return true;
    }

    let profit_margin = net_profit as f64 / total_fees as f64;
    profit_margin >= min_profit_margin
}

/// Calculate fee-adjusted profit
pub fn calculate_net_profit(gross_profit: u64, fees: &TransactionFees) -> i64 {
    gross_profit as i64 - estimate_total_cost(fees) as i64
}

/// Priority level for transactions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriorityLevel {
    Low,
    Medium,
    High,
    Urgent,
}

impl PriorityLevel {
    /// Convert from profit amount to priority level
    pub fn from_profit_usd(profit_usd: f64) -> Self {
        match profit_usd {
            p if p < 0.1 => PriorityLevel::Low,
            p if p < 1.0 => PriorityLevel::Medium,
            p if p < 10.0 => PriorityLevel::High,
            _ => PriorityLevel::Urgent,
        }
    }

    /// Convert from slippage tolerance to priority level
    pub fn from_slippage_bps(slippage_bps: u16) -> Self {
        match slippage_bps {
            s if s <= 10 => PriorityLevel::Urgent, // Low slippage = high priority
            s if s <= 50 => PriorityLevel::High,
            s if s <= 100 => PriorityLevel::Medium,
            _ => PriorityLevel::Low,
        }
    }
}

/// Fee optimization strategy
#[derive(Debug, Clone)]
pub struct FeeOptimization {
    pub target_block_delay: u8,
    pub max_fee_increase: f64,
    pub fee_adjustment_interval: u64,
}

impl Default for FeeOptimization {
    fn default() -> Self {
        Self {
            target_block_delay: 1,
            max_fee_increase: 2.0, // Max 2x fee increase
            fee_adjustment_interval: 1000, // 1 second
        }
    }
}

/// Optimize fee based on recent block data
pub fn optimize_fee_for_target(
    recent_fees: &[u64],
    target_slot: u64,
    current_slot: u64,
    optimization: &FeeOptimization,
) -> u64 {
    if recent_fees.is_empty() {
        return 1000; // Default fee
    }

    let slot_delay = current_slot.saturating_sub(target_slot);
    let target_percentile = match slot_delay {
        0 => 0.95, // Urgent - use high percentile
        1..=2 => 0.80,
        3..=5 => 0.60,
        _ => 0.40, // Not urgent
    };

    calculate_priority_fee(recent_fees, target_percentile, 1000)
}

/// Calculate fee efficiency (profit per lamport)
pub fn calculate_fee_efficiency(profit_lamports: u64, total_fees: u64) -> f64 {
    if total_fees == 0 {
        return f64::INFINITY;
    }

    profit_lamports as f64 / total_fees as f64
}

/// Check if fee is within budget
pub fn is_fee_within_budget(
    current_fee: u64,
    max_fee_per_trade: u64,
    max_fee_percentage: f64,
    trade_size_lamports: u64,
) -> bool {
    // Check absolute fee limit
    if current_fee > max_fee_per_trade {
        return false;
    }

    // Check percentage of trade size
    let fee_percentage = current_fee as f64 / trade_size_lamports as f64;
    fee_percentage <= max_fee_percentage
}
