//! Logging utilities for structured logging
//!
//! Provides JSON-formatted logs with configurable levels and file rotation.

use std::fs;
use std::io::Write;
use tracing::{Level, info};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::utils::config::Config;

/// Initialize the logging system
pub fn init_logger() -> Result<(), Box<dyn std::error::Error>> {
    // Create logs directory if it doesn't exist
    fs::create_dir_all("logs")?;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let registry = tracing_subscriber::registry().with(filter);

    // Console logging
    let console_layer = fmt::layer()
        .with_target(false)
        .with_thread_ids(false)
        .with_thread_names(false)
        .compact();

    // File logging with JSON format
    let file_appender = tracing_appender::rolling::daily("logs", "mev-bot.log");
    let file_layer = fmt::layer()
        .with_writer(file_appender)
        .json()
        .with_target(false)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_file(true)
        .with_line_number(true);

    registry
        .with(console_layer)
        .with(file_layer)
        .init();

    info!("Logger initialized");
    Ok(())
}

/// Log MEV opportunity detection
pub fn log_opportunity_detected(
    strategy: &str,
    profit_usd: f64,
    token_in: &str,
    token_out: &str,
    dex_path: &[&str],
) {
    info!(
        strategy = strategy,
        profit_usd = profit_usd,
        token_in = token_in,
        token_out = token_out,
        dex_path = ?dex_path,
        "MEV opportunity detected"
    );
}

/// Log transaction execution
pub fn log_transaction_executed(
    signature: &str,
    strategy: &str,
    profit_usd: f64,
    latency_ms: u64,
    success: bool,
) {
    if success {
        info!(
            signature = signature,
            strategy = strategy,
            profit_usd = profit_usd,
            latency_ms = latency_ms,
            "Transaction executed successfully"
        );
    } else {
        tracing::error!(
            signature = signature,
            strategy = strategy,
            profit_usd = profit_usd,
            latency_ms = latency_ms,
            "Transaction execution failed"
        );
    }
}

/// Log risk management events
pub fn log_risk_event(event: &str, details: &str) {
    tracing::warn!(
        event = event,
        details = details,
        "Risk management event"
    );
}

/// Log performance metrics
pub fn log_performance_metric(metric: &str, value: f64, unit: &str) {
    info!(
        metric = metric,
        value = value,
        unit = unit,
        "Performance metric"
    );
}

/// Log simulation results
pub fn log_simulation_result(
    strategy: &str,
    profit_usd: f64,
    slippage_bps: u16,
    compute_units: u32,
    valid: bool,
) {
    info!(
        strategy = strategy,
        profit_usd = profit_usd,
        slippage_bps = slippage_bps,
        compute_units = compute_units,
        valid = valid,
        "Transaction simulation completed"
    );
}
