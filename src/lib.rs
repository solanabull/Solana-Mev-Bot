//! # Solana MEV Bot
//!
//! A production-ready MEV (Maximal Extractable Value) trading bot for the Solana blockchain.
//!
//! ## Architecture
//!
//! The bot is structured into several key modules:
//!
//! - `engine`: Core bot engine handling orchestration, mempool monitoring, and execution
//! - `strategies`: MEV strategy implementations (arbitrage, sandwich, liquidation)
//! - `dex`: DEX protocol integrations (Raydium, Orca, OpenBook)
//! - `utils`: Shared utilities for math, fees, logging, and configuration
//!
//! ## Safety
//!
//! This software is experimental and carries significant financial risk.
//! Always test thoroughly on devnet before mainnet deployment.
//!
//! ## Features
//!
//! - Real-time mempool monitoring via WebSocket subscriptions
//! - Multiple MEV strategies with configurable risk controls
//! - Transaction simulation before execution
//! - Jito bundle support for optimized execution
//! - Comprehensive logging and monitoring
//! - Graceful shutdown handling

pub mod engine;
pub mod strategies;
pub mod dex;
pub mod utils;

// Re-export commonly used types
pub use utils::config::Config;
pub use engine::Engine;
