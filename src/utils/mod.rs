//! Utility modules for the MEV bot
//!
//! This module contains shared utilities used across the bot:
//! - Configuration management
//! - Logging setup
//! - Mathematical operations
//! - Fee calculations
//! - Priority fee management
//! - Risk management
//! - Monitoring and metrics
//! - Common traits and types

pub mod config;
pub mod logger;
pub mod math;
pub mod fees;
pub mod priority;
pub mod risk;
pub mod monitoring;

/// Common traits and types used across the bot
pub mod types;

pub use types::*;