//! Risk management and safety controls
//!
//! Implements comprehensive risk controls to prevent catastrophic losses:
//! - Position size limits
//! - Daily loss limits
//! - Consecutive failure handling
//! - Kill switch functionality

use std::sync::Arc;
use tokio::sync::RwLock;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use crate::utils::config::Config;

/// Risk manager for bot safety controls
#[derive(Debug)]
pub struct RiskManager {
    config: Config,
    daily_stats: Arc<RwLock<DailyStats>>,
    session_stats: Arc<RwLock<SessionStats>>,
    kill_switch_activated: Arc<RwLock<bool>>,
}

impl RiskManager {
    /// Create new risk manager
    pub fn new(config: Config) -> Self {
        Self {
            config,
            daily_stats: Arc::new(RwLock::new(DailyStats::new())),
            session_stats: Arc::new(RwLock::new(SessionStats::new())),
            kill_switch_activated: Arc::new(RwLock::new(false)),
        }
    }

    /// Check if trade is allowed based on risk controls
    pub async fn can_execute_trade(&self, trade_size_sol: f64, expected_profit_usd: f64) -> Result<bool, RiskError> {
        // Check kill switch
        if *self.kill_switch_activated.read().await {
            return Err(RiskError::KillSwitchActivated);
        }

        // Check position size limit
        if trade_size_sol > self.config.risk_management.max_sol_per_trade {
            return Err(RiskError::PositionSizeExceeded {
                requested: trade_size_sol,
                limit: self.config.risk_management.max_sol_per_trade,
            });
        }

        // Check daily loss limit
        let daily_stats = self.daily_stats.read().await;
        if daily_stats.total_loss_usd >= self.config.risk_management.daily_loss_limit_usd {
            return Err(RiskError::DailyLossLimitExceeded {
                current_loss: daily_stats.total_loss_usd,
                limit: self.config.risk_management.daily_loss_limit_usd,
            });
        }

        // Check consecutive failures
        let session_stats = self.session_stats.read().await;
        if session_stats.consecutive_failures >= self.config.risk_management.max_consecutive_failures {
            if self.config.risk_management.auto_disable_on_failures {
                *self.kill_switch_activated.write().await = true;
                return Err(RiskError::TooManyConsecutiveFailures {
                    failures: session_stats.consecutive_failures,
                    limit: self.config.risk_management.max_consecutive_failures,
                });
            }
        }

        // Check if trade would exceed daily loss limit
        let potential_total_loss = daily_stats.total_loss_usd - expected_profit_usd.min(0.0).abs();
        if potential_total_loss >= self.config.risk_management.daily_loss_limit_usd {
            return Err(RiskError::TradeWouldExceedDailyLimit {
                potential_loss: potential_total_loss,
                limit: self.config.risk_management.daily_loss_limit_usd,
            });
        }

        Ok(true)
    }

    /// Record trade execution result
    pub async fn record_trade_result(&self, success: bool, profit_loss_usd: f64, trade_size_sol: f64) {
        let mut session_stats = self.session_stats.write().await;
        let mut daily_stats = self.daily_stats.write().await;

        // Update session stats
        session_stats.total_trades += 1;
        session_stats.total_volume_sol += trade_size_sol;

        if success {
            session_stats.successful_trades += 1;
            session_stats.consecutive_failures = 0;
            session_stats.total_profit_usd += profit_loss_usd.max(0.0);
        } else {
            session_stats.failed_trades += 1;
            session_stats.consecutive_failures += 1;
            session_stats.total_loss_usd += profit_loss_usd.min(0.0).abs();
        }

        // Update daily stats
        daily_stats.total_trades += 1;
        daily_stats.total_volume_sol += trade_size_sol;

        if success {
            daily_stats.successful_trades += 1;
            daily_stats.total_profit_usd += profit_loss_usd.max(0.0);
        } else {
            daily_stats.failed_trades += 1;
            daily_stats.total_loss_usd += profit_loss_usd.min(0.0).abs();
        }

        // Check if daily reset is needed
        let now = Utc::now();
        if now.date_naive() != daily_stats.date {
            *daily_stats = DailyStats::new();
        }
    }

    /// Activate kill switch
    pub async fn activate_kill_switch(&self) {
        *self.kill_switch_activated.write().await = true;
        crate::utils::logger::log_risk_event("kill_switch_activated", "Kill switch manually activated");
    }

    /// Deactivate kill switch
    pub async fn deactivate_kill_switch(&self) {
        *self.kill_switch_activated.write().await = false;
        crate::utils::logger::log_risk_event("kill_switch_deactivated", "Kill switch manually deactivated");
    }

    /// Check if kill switch is active
    pub async fn is_kill_switch_active(&self) -> bool {
        *self.kill_switch_activated.read().await
    }

    /// Get current risk status
    pub async fn get_risk_status(&self) -> RiskStatus {
        let daily_stats = self.daily_stats.read().await;
        let session_stats = self.session_stats.read().await;
        let kill_switch_active = *self.kill_switch_activated.read().await;

        RiskStatus {
            kill_switch_active,
            daily_loss_usd: daily_stats.total_loss_usd,
            daily_loss_limit_usd: self.config.risk_management.daily_loss_limit_usd,
            consecutive_failures: session_stats.consecutive_failures,
            max_consecutive_failures: self.config.risk_management.max_consecutive_failures,
            session_success_rate: if session_stats.total_trades > 0 {
                session_stats.successful_trades as f64 / session_stats.total_trades as f64
            } else {
                0.0
            },
        }
    }

    /// Reset daily statistics
    pub async fn reset_daily_stats(&self) {
        *self.daily_stats.write().await = DailyStats::new();
    }

    /// Validate configuration safety
    pub fn validate_config(&self) -> Result<(), RiskError> {
        if self.config.risk_management.max_sol_per_trade <= 0.0 {
            return Err(RiskError::InvalidConfiguration("Max SOL per trade must be positive".to_string()));
        }

        if self.config.risk_management.daily_loss_limit_usd <= 0.0 {
            return Err(RiskError::InvalidConfiguration("Daily loss limit must be positive".to_string()));
        }

        if self.config.risk_management.max_consecutive_failures == 0 {
            return Err(RiskError::InvalidConfiguration("Max consecutive failures must be positive".to_string()));
        }

        Ok(())
    }
}

/// Daily trading statistics
#[derive(Debug, Clone)]
pub struct DailyStats {
    pub date: chrono::NaiveDate,
    pub total_trades: u32,
    pub successful_trades: u32,
    pub failed_trades: u32,
    pub total_volume_sol: f64,
    pub total_profit_usd: f64,
    pub total_loss_usd: f64,
}

impl DailyStats {
    pub fn new() -> Self {
        Self {
            date: Utc::now().date_naive(),
            total_trades: 0,
            successful_trades: 0,
            failed_trades: 0,
            total_volume_sol: 0.0,
            total_profit_usd: 0.0,
            total_loss_usd: 0.0,
        }
    }
}

/// Session trading statistics
#[derive(Debug, Clone)]
pub struct SessionStats {
    pub total_trades: u32,
    pub successful_trades: u32,
    pub failed_trades: u32,
    pub total_volume_sol: f64,
    pub total_profit_usd: f64,
    pub total_loss_usd: f64,
    pub consecutive_failures: u32,
    pub session_start: DateTime<Utc>,
}

impl SessionStats {
    pub fn new() -> Self {
        Self {
            total_trades: 0,
            successful_trades: 0,
            failed_trades: 0,
            total_volume_sol: 0.0,
            total_profit_usd: 0.0,
            total_loss_usd: 0.0,
            consecutive_failures: 0,
            session_start: Utc::now(),
        }
    }
}

/// Current risk status
#[derive(Debug, Clone)]
pub struct RiskStatus {
    pub kill_switch_active: bool,
    pub daily_loss_usd: f64,
    pub daily_loss_limit_usd: f64,
    pub consecutive_failures: u32,
    pub max_consecutive_failures: u32,
    pub session_success_rate: f64,
}

/// Risk management errors
#[derive(Debug, thiserror::Error)]
pub enum RiskError {
    #[error("Kill switch is activated")]
    KillSwitchActivated,

    #[error("Position size {requested} SOL exceeds limit of {limit} SOL")]
    PositionSizeExceeded { requested: f64, limit: f64 },

    #[error("Daily loss limit exceeded: {current_loss} >= {limit}")]
    DailyLossLimitExceeded { current_loss: f64, limit: f64 },

    #[error("Trade would cause total daily loss of {potential_loss} to exceed limit {limit}")]
    TradeWouldExceedDailyLimit { potential_loss: f64, limit: f64 },

    #[error("Too many consecutive failures: {failures} >= {limit}")]
    TooManyConsecutiveFailures { failures: u32, limit: u32 },

    #[error("Invalid configuration: {0}")]
    InvalidConfiguration(String),
}

/// Risk monitoring alerts
pub enum RiskAlert {
    HighLossRate { rate: f64, threshold: f64 },
    DailyLossApproaching { current: f64, limit: f64 },
    ConsecutiveFailures { count: u32, limit: u32 },
    KillSwitchActivated { reason: String },
}

impl RiskManager {
    /// Check for risk alerts
    pub async fn check_alerts(&self) -> Vec<RiskAlert> {
        let mut alerts = Vec::new();
        let status = self.get_risk_status().await;

        // Check loss rate
        if status.session_success_rate < 0.5 && status.session_success_rate > 0.0 {
            alerts.push(RiskAlert::HighLossRate {
                rate: 1.0 - status.session_success_rate,
                threshold: 0.5,
            });
        }

        // Check daily loss approaching limit
        let loss_percentage = status.daily_loss_usd / status.daily_loss_limit_usd;
        if loss_percentage > 0.8 {
            alerts.push(RiskAlert::DailyLossApproaching {
                current: status.daily_loss_usd,
                limit: status.daily_loss_limit_usd,
            });
        }

        // Check consecutive failures
        if status.consecutive_failures >= status.max_consecutive_failures / 2 {
            alerts.push(RiskAlert::ConsecutiveFailures {
                count: status.consecutive_failures,
                limit: status.max_consecutive_failures,
            });
        }

        alerts
    }
}
