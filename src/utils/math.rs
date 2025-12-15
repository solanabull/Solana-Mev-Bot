//! Mathematical utilities for MEV calculations
//!
//! Provides high-precision decimal arithmetic for trading calculations,
//! slippage computations, and profit analysis.

use rust_decimal::prelude::*;
use std::cmp;

/// Decimal precision for calculations
const DECIMAL_SCALE: u32 = 12;

/// Calculate percentage difference between two values
pub fn calculate_percentage_diff(old_value: f64, new_value: f64) -> f64 {
    if old_value == 0.0 {
        return 0.0;
    }
    ((new_value - old_value) / old_value) * 100.0
}

/// Calculate basis points (1 bp = 0.01%)
pub fn calculate_bps_diff(old_value: f64, new_value: f64) -> i32 {
    (calculate_percentage_diff(old_value, new_value) * 100.0) as i32
}

/// Convert basis points to decimal multiplier
pub fn bps_to_multiplier(bps: u16) -> Decimal {
    Decimal::new(10000 + bps as i64, 4)
}

/// Convert basis points to decimal
pub fn bps_to_decimal(bps: u16) -> Decimal {
    Decimal::new(bps as i64, 4)
}

/// Calculate slippage-adjusted amount
pub fn calculate_slippage_amount(amount: u64, slippage_bps: u16, is_buy: bool) -> u64 {
    let amount_dec = Decimal::from(amount);
    let slippage_dec = bps_to_decimal(slippage_bps);

    if is_buy {
        // For buys, we might get less than expected (slippage up)
        let adjusted = amount_dec * (Decimal::ONE - slippage_dec);
        adjusted.to_u64().unwrap_or(amount)
    } else {
        // For sells, we might get less than expected (slippage down)
        let adjusted = amount_dec * (Decimal::ONE - slippage_dec);
        adjusted.to_u64().unwrap_or(amount)
    }
}

/// Calculate profit percentage
pub fn calculate_profit_percentage(investment: f64, profit: f64) -> f64 {
    if investment == 0.0 {
        return 0.0;
    }
    (profit / investment) * 100.0
}

/// Check if profit meets minimum threshold
pub fn meets_profit_threshold(current_profit: f64, min_profit: f64, tolerance: f64) -> bool {
    current_profit >= (min_profit * (1.0 - tolerance))
}

/// Calculate compound profit across multiple trades
pub fn calculate_compound_profit(initial_amount: f64, profits: &[f64]) -> f64 {
    let mut total = initial_amount;
    for &profit in profits {
        total += profit;
    }
    total - initial_amount
}

/// Calculate impermanent loss for LP positions
pub fn calculate_impermanent_loss(price_ratio_initial: f64, price_ratio_current: f64) -> f64 {
    let ratio = (price_ratio_current / price_ratio_initial).sqrt();
    2.0 * ratio / (1.0 + ratio) - 1.0
}

/// Calculate optimal trade size based on available liquidity
pub fn calculate_optimal_trade_size(
    available_liquidity: u64,
    max_slippage_bps: u16,
    safety_factor: f64,
) -> u64 {
    let liquidity_dec = Decimal::from(available_liquidity);
    let max_slippage = bps_to_decimal(max_slippage_bps);
    let safety_dec = Decimal::from_f64(safety_factor).unwrap_or(Decimal::ONE);

    // Optimal size = liquidity * max_slippage * safety_factor
    let optimal = liquidity_dec * max_slippage * safety_dec;
    optimal.to_u64().unwrap_or(available_liquidity / 10)
}

/// Calculate price impact
pub fn calculate_price_impact(
    trade_size: u64,
    pool_reserve: u64,
    fee_bps: u16,
) -> f64 {
    if pool_reserve == 0 {
        return 0.0;
    }

    let trade_dec = Decimal::from(trade_size);
    let reserve_dec = Decimal::from(pool_reserve);
    let fee_dec = bps_to_decimal(fee_bps);

    // Price impact formula: (trade_size / (reserve + trade_size)) * (1 - fee)
    let impact = trade_dec / (reserve_dec + trade_dec) * (Decimal::ONE - fee_dec);
    impact.to_f64().unwrap_or(0.0)
}

/// Weighted average price calculation
pub fn calculate_weighted_average_price(prices: &[(f64, f64)]) -> f64 {
    let mut total_weight = 0.0;
    let mut weighted_sum = 0.0;

    for &(price, weight) in prices {
        weighted_sum += price * weight;
        total_weight += weight;
    }

    if total_weight == 0.0 {
        0.0
    } else {
        weighted_sum / total_weight
    }
}

/// Calculate confidence interval for price predictions
pub fn calculate_confidence_interval(
    mean: f64,
    variance: f64,
    confidence_level: f64,
    sample_size: usize,
) -> (f64, f64) {
    if sample_size < 2 {
        return (mean, mean);
    }

    let standard_error = (variance / sample_size as f64).sqrt();
    let z_score = match confidence_level {
        0.95 => 1.96,
        0.99 => 2.576,
        _ => 1.96, // Default to 95%
    };

    let margin = z_score * standard_error;
    (mean - margin, mean + margin)
}

/// Safe division that handles zero divisor
pub fn safe_divide(numerator: f64, denominator: f64, default: f64) -> f64 {
    if denominator == 0.0 {
        default
    } else {
        numerator / denominator
    }
}

/// Clamp value between min and max
pub fn clamp<T: PartialOrd>(value: T, min: T, max: T) -> T {
    if value < min {
        min
    } else if value > max {
        max
    } else {
        value
    }
}

/// Calculate exponential moving average
pub fn calculate_ema(current_price: f64, previous_ema: f64, smoothing: f64) -> f64 {
    smoothing * current_price + (1.0 - smoothing) * previous_ema
}

/// Calculate relative strength index (RSI)
pub fn calculate_rsi(prices: &[f64], period: usize) -> f64 {
    if prices.len() < period + 1 {
        return 50.0; // Neutral RSI
    }

    let mut gains = 0.0;
    let mut losses = 0.0;

    for i in 1..=period {
        let change = prices[prices.len() - i] - prices[prices.len() - i - 1];
        if change > 0.0 {
            gains += change;
        } else {
            losses -= change;
        }
    }

    if losses == 0.0 {
        return 100.0;
    }

    let avg_gain = gains / period as f64;
    let avg_loss = losses / period as f64;
    let rs = avg_gain / avg_loss;

    100.0 - (100.0 / (1.0 + rs))
}
