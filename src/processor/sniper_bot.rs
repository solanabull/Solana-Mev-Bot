use std::collections::{HashSet, VecDeque};
use std::str::FromStr;
use bs58;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::common::config::import_env_var;
use crate::processor::selling_strategy::{SellingEngine, SellingConfig};
use anyhow::Result;

/// Extract the signer (fee payer) from a yellowstone grpc transaction
/// Returns the first signer which is typically the transaction fee payer
fn extract_signer_from_transaction(txn: &SubscribeUpdateTransaction) -> Option<String> {
    if let Some(transaction_info) = &txn.transaction {
        if let Some(transaction) = &transaction_info.transaction {
            if let Some(message) = &transaction.message {
                // Get account keys - the first one is typically the signer/fee payer
                if !message.account_keys.is_empty() {
                    // Convert the first account key (signer) to string
                    return Some(bs58::encode(&message.account_keys[0]).into_string());
                }
            }
        }
    }
    None
}
use anchor_client::solana_sdk::{pubkey::Pubkey, signature::Signature};
use solana_sdk::signature::Signer;
use spl_associated_token_account::get_associated_token_address;
use colored::Colorize;
use tokio::time;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use solana_sdk::signer::keypair::Keypair;
use futures_util::stream::StreamExt;
use futures_util::{SinkExt, Sink};
use yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient};
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest, SubscribeRequestPing,
    SubscribeRequestFilterTransactions,  SubscribeUpdate, SubscribeUpdateTransaction,
};
use solana_transaction_status::TransactionConfirmationStatus;
use crate::processor::transaction_parser;
use crate::common::{
    config::{Config, AppState, SwapConfig},
    logger::Logger,
    cache::WALLET_TOKEN_ACCOUNTS,
    constants::WHALE_SELLING_AMOUNT_FOR_SELLING_TRIGGER,
};
use crate::processor::swap::{SwapDirection, SwapProtocol, SwapInType};
use crate::processor::transaction_parser::{DexType, TradeInfoFromToken};
use crate::processor::selling_strategy::{TokenTrackingInfo as SellingTokenTrackingInfo, TokenMetrics};
use crate::processor::transaction_retry;
use dashmap::DashMap;
use crate::dex::pump_fun::PUMP_FUN_PROGRAM;
use crate::dex::pump_swap::PUMP_SWAP_PROGRAM;
use crate::dex::raydium_launchpad::RAYDIUM_LAUNCHPAD_PROGRAM;
use chrono::Timelike;

// Enum for different selling actions
#[derive(Debug, Clone)]
pub enum SellingAction {
    Hold,
    SellAll(String), // Reason for selling all
}

// Data structure for tracking bought tokens with comprehensive selling logic
#[derive(Clone, Debug)]
pub struct BoughtTokenInfo {
    pub token_mint: String,
    pub entry_price: u64,
    pub current_price: u64,
    pub highest_price: u64,
    pub lowest_price_after_highest: u64,
    pub initial_amount: f64, // Amount of SOL initially spent
    pub current_amount: f64, // Current token amount held
    pub buy_timestamp: Instant,
    pub protocol: SwapProtocol,
    pub trade_info: transaction_parser::TradeInfoFromToken,
    pub pnl_percentage: f64,
    pub highest_pnl_percentage: f64,
    pub trailing_stop_percentage: f64,
    pub selling_time_seconds: u64, // SELLING_TIME in seconds
    pub last_price_update: Instant,
    pub first_20_percent_reached_time: Option<Instant>, // When 20% PnL was first reached
}

impl BoughtTokenInfo {
    pub fn new(
        token_mint: String,
        entry_price: u64,
        initial_amount: f64,
        current_amount: f64,
        protocol: SwapProtocol,
        trade_info: transaction_parser::TradeInfoFromToken,
        selling_time_seconds: u64,
    ) -> Self {
        Self {
            token_mint,
            entry_price,
            current_price: entry_price,
            highest_price: entry_price,
            lowest_price_after_highest: entry_price,
            initial_amount,
            current_amount,
            buy_timestamp: Instant::now(),
            protocol,
            trade_info,
            pnl_percentage: 0.0,
            highest_pnl_percentage: 0.0,
            trailing_stop_percentage: 1.0, // Start with 1% trailing stop
            selling_time_seconds,
            last_price_update: Instant::now(),
            first_20_percent_reached_time: None,
        }
    }

    pub fn update_price(&mut self, new_price: u64) {
        self.current_price = new_price;
        self.last_price_update = Instant::now();
        
        // Update highest price
        if new_price > self.highest_price {
            self.highest_price = new_price;
            self.lowest_price_after_highest = new_price; // Reset lowest after new high
        } else if new_price < self.lowest_price_after_highest {
            self.lowest_price_after_highest = new_price;
        }
        
        // Calculate PnL percentage - prevent division by zero
        self.pnl_percentage = if self.entry_price > 0 {
            ((new_price as f64 - self.entry_price as f64) / self.entry_price as f64) * 100.0
        } else {
            0.0 // No PnL calculation if entry_price is not set
        };
        
        // Debug logging for price calculations
        if self.pnl_percentage.abs() > 1000.0 { // Log if PnL is unusually high
            println!("DEBUG PRICE: Token {} - Entry: {}, Current: {}, PnL: {:.2}%", 
                self.token_mint, self.entry_price, new_price, self.pnl_percentage);
        }
        
        // Update highest PnL
        if self.pnl_percentage > self.highest_pnl_percentage {
            self.highest_pnl_percentage = self.pnl_percentage;
        }
        
        // Check if 20% PnL is reached for the first time and within 1.5 seconds
        if self.pnl_percentage >= 20.0 && self.first_20_percent_reached_time.is_none() {
            let time_since_buy = self.buy_timestamp.elapsed().as_millis();
            if time_since_buy <= 1500 { // 1.5 seconds
                self.first_20_percent_reached_time = Some(Instant::now());
            }
        }
        
        // Update trailing stop based on PnL
        self.trailing_stop_percentage = self.calculate_dynamic_trailing_stop();
    }
    
    fn calculate_dynamic_trailing_stop(&self) -> f64 {
        match self.highest_pnl_percentage {
            pnl if pnl < 20.0 => 1.0,     // <20% PnL: 1% trailing stop (default)
            pnl if pnl < 50.0 => 5.0,     // 20-49% PnL: 5% trailing stop
            pnl if pnl < 100.0 => 10.0,   // 50-99% PnL: 10% trailing stop
            pnl if pnl < 200.0 => 30.0,   // 100-199% PnL: 30% trailing stop
            pnl if pnl < 500.0 => 100.0,  // 200-499% PnL: 100% trailing stop
            pnl if pnl < 1000.0 => 100.0, // 500-999% PnL: 100% trailing stop
            _ => 100.0,                    // ≥1000% PnL: 100% trailing stop
        }
    }
    
    /// Legacy method - use get_selling_action() instead
    /// DEPRECATED: This method contained bad logic and should not be used
    pub fn should_sell_all_due_to_time(&self) -> bool {
        // This method is deprecated and always returns false
        // Use get_selling_action() instead which has proper logic
        false
    }
    
    pub fn should_sell_due_to_trailing_stop(&self) -> bool {
        // Don't trigger trailing stop if entry_price is not set (buy not processed yet)
        if self.entry_price == 0 || self.highest_price == 0 {
            return false;
        }
        
        let drop_from_highest = ((self.highest_price as f64 - self.current_price as f64) / self.highest_price as f64) * 100.0;
        drop_from_highest >= self.trailing_stop_percentage
    }
    
    /// Determine selling action based on comprehensive rules
    pub fn get_selling_action(&self) -> SellingAction {
        // CRITICAL: Don't sell if entry_price is 0 (buy transaction not yet processed)
        if self.entry_price == 0 {
            return SellingAction::Hold;
        }
        
        // Read thresholds from .env with sensible defaults
        let take_profit = import_env_var("TAKE_PROFIT").parse::<f64>().unwrap_or(25.0);
        let stop_loss = import_env_var("STOP_LOSS").parse::<f64>().unwrap_or(-30.0);
        let max_hold_time = import_env_var("MAX_HOLD_TIME").parse::<u64>().unwrap_or(86400);
        
        let time_since_buy = self.buy_timestamp.elapsed().as_secs();
        
        // Stop Loss
        if self.pnl_percentage <= stop_loss {
            return SellingAction::SellAll(format!("Stop loss triggered: {:.2}% loss", self.pnl_percentage));
        }
        
        // Take Profit
        if self.pnl_percentage >= take_profit {
            return SellingAction::SellAll(format!("Take profit triggered: {:.2}% profit", self.pnl_percentage));
        }
        
        // Maximum hold time
        if time_since_buy >= max_hold_time {
            return SellingAction::SellAll(format!("Max hold time reached: {} seconds", time_since_buy));
        }
        
        // Trailing stop logic is disabled
        SellingAction::Hold
    }
    

    
    /// Check trailing stop with specific percentage
    fn should_sell_due_to_trailing_stop_with_percentage(&self, trailing_stop_percentage: f64) -> bool {
        // Don't trigger trailing stop if entry_price is not set (buy not processed yet)
        if self.entry_price == 0 || self.highest_price == 0 {
            return false;
        }
        
        let drop_from_highest = ((self.highest_price as f64 - self.current_price as f64) / self.highest_price as f64) * 100.0;
        drop_from_highest >= trailing_stop_percentage
    }
    

}

// Global state for sniper bot
lazy_static::lazy_static! {
    static ref COUNTER: Arc<DashMap<(), u64>> = Arc::new(DashMap::new());
    static ref SOLD_TOKENS: Arc<DashMap<(), u64>> = Arc::new(DashMap::new());
    static ref BOUGHT_TOKENS: Arc<DashMap<(), u64>> = Arc::new(DashMap::new());
    static ref LAST_BUY_TIME: Arc<DashMap<(), Option<Instant>>> = Arc::new(DashMap::new());
    static ref BUYING_ENABLED: Arc<DashMap<(), bool>> = Arc::new(DashMap::new());
    static ref TOKEN_TRACKING: Arc<DashMap<String, TokenTrackingInfo>> = Arc::new(DashMap::new());
    // Global registry for monitoring task cancellation tokens
    static ref MONITORING_TASKS: Arc<DashMap<String, (String, CancellationToken)>> = Arc::new(DashMap::new());
    // New: Bought token list for comprehensive tracking (public for risk management)
    pub static ref BOUGHT_TOKEN_LIST: Arc<DashMap<String, BoughtTokenInfo>> = Arc::new(DashMap::new());
    // Global flag to control GRPC stream lifecycle
    static ref SHOULD_CONTINUE_STREAMING: Arc<AtomicBool> = Arc::new(AtomicBool::new(true));
    // Add: Permanent blacklist for tokens that have been bought before (never rebuy)
    static ref BOUGHT_TOKENS_BLACKLIST: Arc<DashMap<String, u64>> = Arc::new(DashMap::new());
    // SNIPER BOT: Focus token list for tracking target wallet purchases
    pub static ref FOCUS_TOKEN_LIST: Arc<DashMap<String, FocusTokenInfo>> = Arc::new(DashMap::new());
    // SNIPER BOT: Price monitoring tasks for focus tokens
    static ref PRICE_MONITORING_TASKS: Arc<DashMap<String, CancellationToken>> = Arc::new(DashMap::new());
}

// Initialize the global counters with default values
fn init_global_state() {
    COUNTER.insert((), 0);
    SOLD_TOKENS.insert((), 0);
    BOUGHT_TOKENS.insert((), 0);
    LAST_BUY_TIME.insert((), None);
    BUYING_ENABLED.insert((), true);
}

// Track token performance for selling strategies
#[derive(Clone, Debug)]
pub struct TokenTrackingInfo {
    pub top_pnl: f64,
    pub last_sell_time: Instant,
    pub completed_intervals: HashSet<String>,
    pub sell_attempts: usize,
    pub sell_success: usize,
}

// SNIPER BOT: Track focus tokens bought by target wallets
#[derive(Clone, Debug)]
pub struct FocusTokenInfo {
    pub mint: String,
    pub initial_price: f64,
    pub current_price: f64,
    pub lowest_price: f64,
    pub highest_price: f64,
    pub price_dropped: bool,
    pub buy_count: u32,
    pub sell_count: u32,
    pub trade_cycles: u32, // Completed buy-sell cycles
    pub protocol: SwapProtocol,
    pub added_timestamp: Instant,
    pub last_price_update: Instant,
    pub price_history: VecDeque<f64>,
    // Track (slot, price) to detect sudden multi-slot drops
    pub slot_price_history: VecDeque<(u64, f64)>,
    // Flag becomes true if price drops across two consecutive slots
    pub two_slot_drop_active: bool,
    pub whale_wallets: HashSet<String>, // Track whale wallets that bought this token
    pub total_trades: u32, // Total buy+sell trades for this token (limit to 10)
}

/// Configuration for sniper bot
#[derive(Clone)]
pub struct SniperConfig {
    pub yellowstone_grpc_http: String,
    pub yellowstone_grpc_token: String,
    pub app_state: AppState,
    pub swap_config: SwapConfig,
    pub counter_limit: u64,
    pub target_addresses: Vec<String>,
    pub excluded_addresses: Vec<String>,
    pub protocol_preference: SwapProtocol,
}

/// Helper to send heartbeat pings to maintain connection
async fn send_heartbeat_ping(
    subscribe_tx: &Arc<tokio::sync::Mutex<impl Sink<SubscribeRequest, Error = impl std::fmt::Debug> + Unpin>>,
) -> Result<(), String> {
    let ping_request = SubscribeRequest {
        ping: Some(SubscribeRequestPing { id: 0 }),
        ..Default::default()
    };
    
    let mut tx = subscribe_tx.lock().await;
    match tx.send(ping_request).await {
        Ok(_) => {
            Ok(())
        },
        Err(e) => Err(format!("Failed to send ping: {:?}", e)),
    }
}

/// Cancel monitoring task for a sold token and clean up tracking
async fn cancel_token_monitoring(token_mint: &str, logger: &Logger) -> Result<(), String> {
    logger.log(format!("🔌 Cancelling monitoring and closing gRPC stream for sold token: {}", token_mint));
    
    // Cancel the monitoring task (this will trigger stream cleanup in monitor_token_for_selling)
    if let Some((_removed_key, (_token_name, cancel_token))) = MONITORING_TASKS.remove(token_mint) {
        cancel_token.cancel(); // This cancels the CancellationToken, triggering cleanup
        logger.log(format!("✅ Cancelled monitoring task and triggered gRPC stream closure for token: {}", token_mint));
    } else {
        logger.log(format!("⚠️ No monitoring task found for token: {} (stream may already be closed)", token_mint).yellow().to_string());
    }
    
    // Remove from token tracking
    if TOKEN_TRACKING.remove(token_mint).is_some() {
        logger.log(format!("Removed token from tracking: {}", token_mint));
    }
    
    // Check if all tokens are sold and stop streaming if needed
    check_and_stop_streaming_if_all_sold(&logger).await;
    
    Ok(())
}

/// Check if all tokens are sold and stop GRPC streaming to prevent connection accumulation
pub async fn check_and_stop_streaming_if_all_sold(logger: &Logger) {
    let active_tokens_count = BOUGHT_TOKEN_LIST.len();
    let active_monitoring_count = MONITORING_TASKS.len();
    let active_tracking_count = TOKEN_TRACKING.len();
    
    // If all tracking systems are empty, we can stop streaming
    if active_tokens_count == 0 && active_monitoring_count == 0 && active_tracking_count == 0 {
        SHOULD_CONTINUE_STREAMING.store(false, Ordering::SeqCst);
    }
}

/// Main function to start sniper bot
pub async fn start_target_wallet_monitoring(config: SniperConfig) -> Result<(), String> {
    let logger = Logger::new("[SNIPER-BOT] => ".green().bold().to_string());
    
    // Reset streaming flag for fresh start
    SHOULD_CONTINUE_STREAMING.store(true, Ordering::SeqCst);
    
    // Initialize global state
    init_global_state();
            
     // Start enhanced selling monitor
     let app_state_clone = Arc::new(config.app_state.clone());
     let swap_config_clone = Arc::new(config.swap_config.clone());
     tokio::spawn(async move {
         start_enhanced_selling_monitor(app_state_clone, swap_config_clone).await;
     });
    
    // Connect to Yellowstone gRPC
    let mut client = GeyserGrpcClient::build_from_shared(config.yellowstone_grpc_http.clone())
        .map_err(|e| format!("Failed to build client: {}", e))?
        .x_token::<String>(Some(config.yellowstone_grpc_token.clone()))
        .map_err(|e| format!("Failed to set x_token: {}", e))?
        .tls_config(ClientTlsConfig::new().with_native_roots())
        .map_err(|e| format!("Failed to set tls config: {}", e))?
        .connect()
        .await
        .map_err(|e| format!("Failed to connect: {}", e))?;

    // Set up subscribe
    let mut retry_count = 0;
    const MAX_RETRIES: u32 = 3;
    let (subscribe_tx, mut stream) = loop {
        match client.subscribe().await {
            Ok(pair) => break pair,
            Err(e) => {
                retry_count += 1;
                if retry_count >= MAX_RETRIES {
                    return Err(format!("Failed to subscribe after {} attempts: {}", MAX_RETRIES, e));
                }
                time::sleep(Duration::from_secs(5)).await;
            }
        }
    };

    // Convert to Arc to allow cloning across tasks
    let subscribe_tx = Arc::new(tokio::sync::Mutex::new(subscribe_tx));
    // Enable buying
    BUYING_ENABLED.insert((), true);

    // Create config for subscription
    let target_addresses = config.target_addresses.clone();

    // Set up subscription
    let subscription_request = SubscribeRequest {
        transactions: maplit::hashmap! {
            "All".to_owned() => SubscribeRequestFilterTransactions {
                vote: Some(false), // Exclude vote transactions
                failed: Some(false), // Exclude failed transactions
                signature: None,
                account_include: target_addresses.clone(), // Only include transactions involving our targets
                account_exclude: vec![], // Listen to all transactions
                account_required: Vec::<String>::new(),
            }
        },
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    };
    
    subscribe_tx
        .lock()
        .await
        .send(subscription_request)
        .await
        .map_err(|e| format!("Failed to send subscribe request: {}", e))?;
    
    // Create Arc config for tasks
    let config = Arc::new(config);

    // Spawn heartbeat task
    let subscribe_tx_clone = subscribe_tx.clone();
    
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(30));
        
        loop {
            interval.tick().await;
            if let Err(_e) = send_heartbeat_ping(&subscribe_tx_clone).await {
                break;
            }
        }
    });
    
    // Main stream processing loop
    while SHOULD_CONTINUE_STREAMING.load(Ordering::SeqCst) {
        match stream.next().await {
            Some(msg_result) => {
                match msg_result {
                    Ok(msg) => {
                        if let Err(e) = process_message_for_target_monitoring(&msg, &subscribe_tx, config.clone(), &logger).await {
                            logger.log(format!("Error processing message: {}", e).red().to_string());
                        }
                    },
                    Err(e) => {
                        logger.log(format!("Stream error: {:?}", e).red().to_string());
                        // Check if it's a connection limit error
                        if format!("{:?}", e).contains("Maximum connection count reached") {
                            logger.log("🚫 Connection limit reached - this indicates a connection leak. Streams should be properly closed when tokens are sold.".red().bold().to_string());
                        }
                        // Try to reconnect
                        break;
                    },
                }
            },
            None => {
                logger.log("Stream ended".yellow().to_string());
                break;
            }
        }
    }
    
    if !SHOULD_CONTINUE_STREAMING.load(Ordering::SeqCst) {
        // Explicitly drop the stream and client to close connections
        drop(stream);
        drop(subscribe_tx);
        drop(client);
        
        return Ok(());
    }
    
    // Here you would implement reconnection logic
    
    Ok(())
}




/// Main function to start sniper bot
pub async fn start_dex_monitoring(config: SniperConfig) -> Result<(), String> {
    let logger = Logger::new("[SNIPER-BOT] => ".green().bold().to_string());
    
    // Reset streaming flag for fresh start
    SHOULD_CONTINUE_STREAMING.store(true, Ordering::SeqCst);
    
     // Start enhanced selling monitor
     let app_state_clone = Arc::new(config.app_state.clone());
     let swap_config_clone = Arc::new(config.swap_config.clone());
     tokio::spawn(async move {
         start_enhanced_selling_monitor(app_state_clone, swap_config_clone).await;
     });
    
    // Connect to Yellowstone gRPC
    let mut client = GeyserGrpcClient::build_from_shared(config.yellowstone_grpc_http.clone())
        .map_err(|e| format!("Failed to build client: {}", e))?
        .x_token::<String>(Some(config.yellowstone_grpc_token.clone()))
        .map_err(|e| format!("Failed to set x_token: {}", e))?
        .tls_config(ClientTlsConfig::new().with_native_roots())
        .map_err(|e| format!("Failed to set tls config: {}", e))?
        .connect()
        .await
        .map_err(|e| format!("Failed to connect: {}", e))?;

    // Set up subscribe
    let mut retry_count = 0;
    const MAX_RETRIES: u32 = 3;
    let (subscribe_tx, mut stream) = loop {
        match client.subscribe().await {
            Ok(pair) => break pair,
            Err(e) => {
                retry_count += 1;
                if retry_count >= MAX_RETRIES {
                    return Err(format!("Failed to subscribe after {} attempts: {}", MAX_RETRIES, e));
                }
                time::sleep(Duration::from_secs(5)).await;
            }
        }
    };

    // Convert to Arc to allow cloning across tasks
    let subscribe_tx = Arc::new(tokio::sync::Mutex::new(subscribe_tx));

    // Create config for subscription
    let dexs = vec![
        PUMP_FUN_PROGRAM.to_string(),
        PUMP_SWAP_PROGRAM.to_string(),
        RAYDIUM_LAUNCHPAD_PROGRAM.to_string(),
    ];
    // Set up subscription
    let subscription_request = SubscribeRequest {
        transactions: maplit::hashmap! {
            "All".to_owned() => SubscribeRequestFilterTransactions {
                vote: Some(false), // Exclude vote transactions
                failed: Some(false), // Exclude failed transactions
                signature: None,
                account_include: dexs.clone(), // Only include transactions involving our targets
                account_exclude: vec![], // Listen to all transactions
                account_required: Vec::<String>::new(),
            }
        },
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    };
    
    subscribe_tx
        .lock()
        .await
        .send(subscription_request)
        .await
        .map_err(|e| format!("Failed to send subscribe request: {}", e))?;
    
    // Create Arc config for tasks
    let config = Arc::new(config);

    // Spawn heartbeat task
    let subscribe_tx_clone = subscribe_tx.clone();
    
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(30));
        
        loop {
            interval.tick().await;
            if let Err(_e) = send_heartbeat_ping(&subscribe_tx_clone).await {
                break;
            }
        }
    });
    
    // Main stream processing loop
    while SHOULD_CONTINUE_STREAMING.load(Ordering::SeqCst) {
        match stream.next().await {
            Some(msg_result) => {
                match msg_result {
                    Ok(msg) => {
                        if let Err(e) = process_message_for_dex_monitoring(&msg, &subscribe_tx, config.clone(), &logger).await {
                            logger.log(format!("Error processing message: {}", e).red().to_string());
                        }
                    },
                    Err(e) => {
                        logger.log(format!("Stream error: {:?}", e).red().to_string());
                        // Check if it's a connection limit error
                        if format!("{:?}", e).contains("Maximum connection count reached") {
                            logger.log("🚫 Connection limit reached - this indicates a connection leak. Streams should be properly closed when tokens are sold.".red().bold().to_string());
                        }
                        // Try to reconnect
                        break;
                    },
                }
            },
            None => {
                logger.log("Stream ended".yellow().to_string());
                break;
            }
        }
    }
    
    if !SHOULD_CONTINUE_STREAMING.load(Ordering::SeqCst) {
        // Explicitly drop the stream and client to close connections
        drop(stream);
        drop(subscribe_tx);
        drop(client);
        
        return Ok(());
    }
    
    // Here you would implement reconnection logic
    
    Ok(())
}




/// Verify that a transaction was successful
async fn verify_transaction(
    signature_str: &str,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<bool, String> {
    // Parse signature
    let signature = match Signature::from_str(signature_str) {
        Ok(sig) => sig,
        Err(e) => return Err(format!("Invalid signature: {}", e)),
    };
    
    // Verify transaction success with retries
    let max_retries = 5;
    for retry in 0..max_retries {
        // Check transaction status
        match app_state.rpc_nonblocking_client.get_signature_statuses(&[signature]).await {
            Ok(result) => {
                if let Some(status_opt) = result.value.get(0) {
                    if let Some(status) = status_opt {
                        if status.err.is_some() {
                            // Transaction failed
                            return Err(format!("Transaction failed: {:?}", status.err));
                        } else if let Some(conf_status) = &status.confirmation_status {
                            if matches!(conf_status, TransactionConfirmationStatus::Finalized | 
                                                      TransactionConfirmationStatus::Confirmed) {
                                return Ok(true);
                            } else {
                                logger.log(format!("Transaction not yet confirmed (status: {:?}), retrying...", 
                                         conf_status).yellow().to_string());
                            }
                        } else {
                        }
                    } else {
                    }
                }
            },
            Err(e) => {
                logger.log(format!("Failed to get transaction status: {}, retrying...", e).red().to_string());
            }
        }
        
        if retry < max_retries - 1 {
            // Wait before retrying
            sleep(Duration::from_millis(500)).await;
        } else {
            return Err("Transaction verification timed out".to_string());
        }
    }
    
    // If we get here, verification failed
    Err("Transaction verification failed after retries".to_string())
}

/// Execute buy operation based on detected transaction
pub async fn execute_buy(
    trade_info: transaction_parser::TradeInfoFromToken,
    app_state: Arc<AppState>,
    swap_config: Arc<SwapConfig>,
    protocol: SwapProtocol,
) -> Result<(), String> {
    let logger = Logger::new("[EXECUTE-BUY] => ".green().to_string());
    let start_time = Instant::now();
    
    // Check if this token is in the permanent blacklist (never rebuy)
    if BOUGHT_TOKENS_BLACKLIST.contains_key(&trade_info.mint) {
        logger.log(format!("🚫 Token {} is blacklisted (previously bought), skipping buy", trade_info.mint).yellow().to_string());
        return Err("Token is blacklisted - previously bought".to_string());
    }
    
    // Create a modified swap config based on the trade_info
    let mut buy_config = (*swap_config).clone();
    buy_config.swap_direction = SwapDirection::Buy;
    
    // Store the amount_in before potential moves
    let amount_in = buy_config.amount_in;
    
    // Get token amount and SOL cost from trade_info
    let (_amount_in, _token_amount) = match trade_info.dex_type {
        transaction_parser::DexType::PumpSwap => {
            let sol_amount = trade_info.sol_change.abs();
            let token_amount = trade_info.token_change.abs();
            (sol_amount, token_amount)
        },
        transaction_parser::DexType::PumpFun => {
            let sol_amount = trade_info.sol_change.abs();
            let token_amount = trade_info.token_change.abs();
            (sol_amount, token_amount)
        },
        transaction_parser::DexType::RaydiumLaunchpad => {
            let sol_amount = trade_info.sol_change.abs();
            let token_amount = trade_info.token_change.abs();
            (sol_amount, token_amount)
        },
        _ => {
            return Err("Unsupported transaction type".to_string());
        }
    };
    
    // Protocol string for notifications
    let _protocol_str = match protocol {
        SwapProtocol::PumpSwap => "PumpSwap",
        SwapProtocol::PumpFun => "PumpFun",
        SwapProtocol::RaydiumLaunchpad => "RaydiumLaunchpad",
        _ => "Unknown",
    };
    
    // Send notification that we're attempting to copy the trade
    
    // Execute based on protocol
    let result = match protocol {
        SwapProtocol::PumpFun => {
            logger.log("Using PumpFun protocol for buy".to_string());
            
            // Create the PumpFun instance
            let pump = crate::dex::pump_fun::Pump::new(
                app_state.rpc_nonblocking_client.clone(),
                app_state.rpc_client.clone(),
                app_state.wallet.clone(),
            );
            // Build swap instructions from parsed data
            match pump.build_swap_from_parsed_data(&trade_info, buy_config.clone()).await {
                Ok((keypair, instructions, price)) => {
                    logger.log(format!("Generated PumpFun buy instruction at price: {}", price));
                    logger.log(format!("copy transaction {}", trade_info.signature));
                    let start_time = Instant::now();
                    // Get real-time blockhash from processor
                    let recent_blockhash = match crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                        Some(hash) => hash,
                        None => {
                            logger.log("Failed to get real-time blockhash, skipping transaction".red().to_string());
                            return Err("Failed to get real-time blockhash".to_string());
                        }
                    };
                    println!("time taken for get_latest_blockhash: {:?}", start_time.elapsed());
                    println!("using zeroslot for buy transaction >>>>>>>>");
                    // Execute the transaction using zeroslot for buying
                    match crate::block_engine::tx::new_signed_and_send_zeroslot(
                        app_state.zeroslot_rpc_client.clone(),
                        recent_blockhash,
                        &keypair,
                        instructions,
                        &logger,
                    ).await {
                        Ok(signatures) => {
                            if signatures.is_empty() {
                                return Err("No transaction signature returned".to_string());
                            }
                            
                            let signature = &signatures[0];
                            logger.log(format!("Buy transaction sent: {}", signature));
                            
                            
                            // Verify transaction
                            match verify_transaction(&signature.to_string(), app_state.clone(), &logger).await {
                                Ok(verified) => {
                                    if verified {
                                        logger.log("Buy transaction verified successfully".to_string());
                                        
                                        // Add token account to our global list and tracking
                                        if let Ok(wallet_pubkey) = app_state.wallet.try_pubkey() {
                                            let token_mint = Pubkey::from_str(&trade_info.mint)
                                                .map_err(|_| "Invalid token mint".to_string())?;
                                            let token_ata = get_associated_token_address(&wallet_pubkey, &token_mint);
                                            WALLET_TOKEN_ACCOUNTS.insert(token_ata);
                                            logger.log(format!("Added token account {} to global list", token_ata));
                                            
                                            // Add to enhanced tracking system for PumpFun
                                            let bought_token_info = BoughtTokenInfo::new(
                                                trade_info.mint.clone(),
                                                trade_info.price, // Use price directly from TradeInfoFromToken (already scaled)
                                                amount_in,
                                                _token_amount,
                                                protocol.clone(),
                                                trade_info.clone(),
                                                3, // 3 seconds selling time
                                            );
                                            BOUGHT_TOKEN_LIST.insert(trade_info.mint.clone(), bought_token_info);
                                            logger.log(format!("Added {} to enhanced tracking system (PumpFun)", trade_info.mint));
                                            
                                            // Add to permanent blacklist (never rebuy this token)
                                            let timestamp = std::time::SystemTime::now()
                                                .duration_since(std::time::UNIX_EPOCH)
                                                .unwrap_or_default()
                                                .as_secs();
                                            BOUGHT_TOKENS_BLACKLIST.insert(trade_info.mint.clone(), timestamp);
                                            logger.log(format!("🚫 Added {} to permanent blacklist", trade_info.mint));
                                            
                                            // CRITICAL FIX: Update selling strategy with actual token balance after successful buy
                                            let selling_engine = crate::processor::selling_strategy::SellingEngine::new(
                                                app_state.clone(),
                                                Arc::new(buy_config.clone()),
                                                crate::processor::selling_strategy::SellingConfig::default()
                                            );
                                            if let Err(e) = selling_engine.update_metrics(&trade_info.mint, &trade_info).await {
                                                logger.log(format!("Warning: Failed to update token metrics after buy: {}", e).yellow().to_string());
                                            }
                                            
                                            // Selling instructions are now built on-demand, no caching needed
                                            logger.log(format!("✅ Buy transaction completed for token: {}", trade_info.mint).green().to_string());
                                        }
                                        
                                        
                                        Ok(())
                                    } else {
                                        
                                        Err("Buy transaction verification failed".to_string())
                                    }
                                },
                                Err(e) => {
                                    Err(format!("Transaction verification error: {}", e))
                                },
                            }
                        },
                        Err(e) => {
                            Err(format!("Transaction error: {}", e))
                        },
                    }
                },
                Err(e) => {
                    Err(format!("Failed to build PumpFun buy instruction: {}", e))
                }
            }
        },
        SwapProtocol::PumpSwap => {
            logger.log("Using PumpSwap protocol for buy".to_string());
            
            // Create the PumpSwap instance
            let pump_swap = crate::dex::pump_swap::PumpSwap::new(
                app_state.wallet.clone(),
                Some(app_state.rpc_client.clone()),
                Some(app_state.rpc_nonblocking_client.clone()),
            );
            
            // Build swap instructions from parsed data for buy
            match pump_swap.build_swap_from_parsed_data(&trade_info, buy_config.clone()).await {
                Ok((keypair, instructions, price)) => {
                    logger.log(format!("Generated PumpSwap buy instruction at price: {}", price));
                    logger.log(format!("copy transaction {}", trade_info.signature));
                    
                    // Get real-time blockhash from processor
                    let recent_blockhash = match crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                        Some(hash) => hash,
                        None => {
                            logger.log("Failed to get real-time blockhash, skipping transaction".red().to_string());
                            return Err("Failed to get real-time blockhash".to_string());
                        }
                    };

                    println!("using zeroslot for buy transaction >>>>>>>>");
                    // Execute the transaction using zeroslot for buying
                    match crate::block_engine::tx::new_signed_and_send_zeroslot(
                        app_state.zeroslot_rpc_client.clone(),
                        recent_blockhash,
                        &keypair,
                        instructions,
                        &logger,
                    ).await {
                        Ok(signatures) => {
                            if signatures.is_empty() {
                                return Err("No transaction signature returned".to_string());
                            }
                            
                            let signature = &signatures[0];
                            logger.log(format!("Buy transaction sent: {}", signature));
                            
                            // Verify transaction
                            match verify_transaction(&signature.to_string(), app_state.clone(), &logger).await {
                                Ok(verified) => {
                                    if verified {
                                        logger.log("Buy transaction verified successfully".to_string());
                                        
                                        // Add token account to our global list and tracking
                                        if let Ok(wallet_pubkey) = app_state.wallet.try_pubkey() {
                                            let token_mint = Pubkey::from_str(&trade_info.mint)
                                                .map_err(|_| "Invalid token mint".to_string())?;
                                            let token_ata = get_associated_token_address(&wallet_pubkey, &token_mint);
                                            WALLET_TOKEN_ACCOUNTS.insert(token_ata);
                                            logger.log(format!("Added token account {} to global list", token_ata));
                                            
                                            // Add to enhanced tracking system for PumpSwap
                                            let bought_token_info = BoughtTokenInfo::new(
                                                trade_info.mint.clone(),
                                                trade_info.price, // Use price directly from TradeInfoFromToken (already scaled)
                                                amount_in,
                                                _token_amount,
                                                protocol.clone(),
                                                trade_info.clone(),
                                                3, // 3 seconds selling time
                                            );
                                            BOUGHT_TOKEN_LIST.insert(trade_info.mint.clone(), bought_token_info);
                                            logger.log(format!("Added {} to enhanced tracking system (PumpSwap)", trade_info.mint));
                                            
                                            // Add to permanent blacklist (never rebuy this token)
                                            let timestamp = std::time::SystemTime::now()
                                                .duration_since(std::time::UNIX_EPOCH)
                                                .unwrap_or_default()
                                                .as_secs();
                                            BOUGHT_TOKENS_BLACKLIST.insert(trade_info.mint.clone(), timestamp);
                                            logger.log(format!("🚫 Added {} to permanent blacklist", trade_info.mint));
                                            
                                            // CRITICAL FIX: Update selling strategy with actual token balance after successful buy
                                            let selling_engine = crate::processor::selling_strategy::SellingEngine::new(
                                                app_state.clone(),
                                                Arc::new(buy_config.clone()),
                                                crate::processor::selling_strategy::SellingConfig::default()
                                            );
                                            if let Err(e) = selling_engine.update_metrics(&trade_info.mint, &trade_info).await {
                                                logger.log(format!("Warning: Failed to update token metrics after buy: {}", e).yellow().to_string());
                                            }
                                        }
                                        
                                        Ok(())
                                    } else {
                                        Err("Buy transaction verification failed".to_string())
                                    }
                                },
                                Err(e) => {
                                    Err(format!("Transaction verification error: {}", e))
                                },
                            }
                        },
                        Err(e) => {
                            Err(format!("Transaction error: {}", e))
                        },
                    }
                },
                Err(e) => {
                    Err(format!("Failed to build PumpSwap buy instruction: {}", e))
                },
            }
        },
                    SwapProtocol::RaydiumLaunchpad => {
                logger.log("Using RaydiumLaunchpad protocol for buy".to_string());
                
                // Create the Raydium instance
                let raydium = crate::dex::raydium_launchpad::Raydium::new(
                app_state.wallet.clone(),
                Some(app_state.rpc_client.clone()),
                Some(app_state.rpc_nonblocking_client.clone()),
            );
            
            // Build swap instructions from parsed data for buy
            match raydium.build_swap_from_parsed_data(&trade_info, buy_config.clone()).await {
                Ok((keypair, instructions, _price)) => {
                    
                    // Get real-time blockhash from processor
                    let recent_blockhash = match crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                        Some(hash) => hash,
                        None => {
                            logger.log("Failed to get real-time blockhash, skipping transaction".red().to_string());
                            return Err("Failed to get real-time blockhash".to_string());
                        }
                    };
                    
                    // Execute the transaction using zeroslot for buying
                    match crate::block_engine::tx::new_signed_and_send_zeroslot(
                        app_state.zeroslot_rpc_client.clone(),
                        recent_blockhash,
                        &keypair,
                        instructions,
                        &logger,
                    ).await {
                        Ok(signatures) => {
                            if signatures.is_empty() {
                                return Err("No transaction signature returned".to_string());
                            }
                            
                            let signature = &signatures[0];
                            logger.log(format!("Buy transaction sent: {}", signature));
                            
                            // Verify transaction
                            match verify_transaction(&signature.to_string(), app_state.clone(), &logger).await {
                                Ok(verified) => {
                                    if verified {
                                        logger.log("Buy transaction verified successfully".to_string());
                                        
                                        // Add token account to our global list and tracking
                                        if let Ok(wallet_pubkey) = app_state.wallet.try_pubkey() {
                                            let token_mint = Pubkey::from_str(&trade_info.mint)
                                                .map_err(|_| "Invalid token mint".to_string())?;
                                            let token_ata = get_associated_token_address(&wallet_pubkey, &token_mint);
                                            WALLET_TOKEN_ACCOUNTS.insert(token_ata);
                                            logger.log(format!("Added token account {} to global list", token_ata));
                                            
                                            // Add to enhanced tracking system for Raydium
                                            let bought_token_info = BoughtTokenInfo::new(
                                                trade_info.mint.clone(),
                                                trade_info.price, // Use price directly from TradeInfoFromToken (already scaled)
                                                amount_in,
                                                _token_amount,
                                                protocol.clone(),
                                                trade_info.clone(),
                                                3, // 3 seconds selling time
                                            );
                                            BOUGHT_TOKEN_LIST.insert(trade_info.mint.clone(), bought_token_info);
                                            logger.log(format!("Added {} to enhanced tracking system (Raydium)", trade_info.mint));
                                            
                                            // Add to permanent blacklist (never rebuy this token)
                                            let timestamp = std::time::SystemTime::now()
                                                .duration_since(std::time::UNIX_EPOCH)
                                                .unwrap_or_default()
                                                .as_secs();
                                            BOUGHT_TOKENS_BLACKLIST.insert(trade_info.mint.clone(), timestamp);
                                            logger.log(format!("🚫 Added {} to permanent blacklist", trade_info.mint));
                                            
                                            // CRITICAL FIX: Update selling strategy with actual token balance after successful buy
                                            let selling_engine = crate::processor::selling_strategy::SellingEngine::new(
                                                app_state.clone(),
                                                Arc::new(buy_config.clone()),
                                                crate::processor::selling_strategy::SellingConfig::default()
                                            );
                                            if let Err(e) = selling_engine.update_metrics(&trade_info.mint, &trade_info).await {
                                                logger.log(format!("Warning: Failed to update token metrics after buy: {}", e).yellow().to_string());
                                            }
                                        }
                                        
                                        Ok(())
                                    } else {
                                        Err("Buy transaction verification failed".to_string())
                                    }
                                },
                                Err(e) => {
                                    Err(format!("Transaction verification error: {}", e))
                                },
                            }
                        },
                        Err(e) => {
                            Err(format!("Transaction error: {}", e))
                        },
                    }
                },
                Err(e) => {
                    Err(format!("Failed to build RaydiumLaunchpad buy instruction: {}", e))
                },
            }
        },
        SwapProtocol::Auto | SwapProtocol::Unknown => {
            logger.log("Auto/Unknown protocol detected, defaulting to PumpFun for buy".yellow().to_string());
            
            // Create the PumpFun instance
            let pump = crate::dex::pump_fun::Pump::new(
                app_state.rpc_nonblocking_client.clone(),
                app_state.rpc_client.clone(),
                app_state.wallet.clone(),
            );
            // Build swap instructions from parsed data
            match pump.build_swap_from_parsed_data(&trade_info, buy_config.clone()).await {
                Ok((keypair, instructions, price)) => {
                    logger.log(format!("Generated PumpFun buy instruction at price: {}", price));
                    logger.log(format!("copy transaction {}", trade_info.signature));
                    let start_time = Instant::now();
                    // Get real-time blockhash from processor
                    let recent_blockhash = match crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                        Some(hash) => hash,
                        None => {
                            logger.log("Failed to get real-time blockhash, skipping transaction".red().to_string());
                            return Err("Failed to get real-time blockhash".to_string());
                        }
                    };
                    println!("time taken for get_latest_blockhash: {:?}", start_time.elapsed());
                    println!("using zeroslot for buy transaction >>>>>>>>");
                    // Execute the transaction using zeroslot for buying
                    match crate::block_engine::tx::new_signed_and_send_zeroslot(
                        app_state.zeroslot_rpc_client.clone(),
                        recent_blockhash,
                        &keypair,
                        instructions,
                        &logger,
                    ).await {
                        Ok(signatures) => {
                            if signatures.is_empty() {
                                return Err("No transaction signature returned".to_string());
                            }
                            
                            let signature = &signatures[0];
                            logger.log(format!("Buy transaction sent: {}", signature));
                            
                            // Verify transaction
                            match verify_transaction(&signature.to_string(), app_state.clone(), &logger).await {
                                Ok(verified) => {
                                    if verified {
                                        logger.log("Buy transaction verified successfully".to_string());
                                        
                                        // Add token account to our global list and tracking
                                        if let Ok(wallet_pubkey) = app_state.wallet.try_pubkey() {
                                            let token_mint = Pubkey::from_str(&trade_info.mint)
                                                .map_err(|_| "Invalid token mint".to_string())?;
                                            let token_ata = get_associated_token_address(&wallet_pubkey, &token_mint);
                                            WALLET_TOKEN_ACCOUNTS.insert(token_ata);
                                            logger.log(format!("Added token account {} to global list", token_ata));
                                            
                                            // Add to enhanced tracking system for PumpFun
                                            let bought_token_info = BoughtTokenInfo::new(
                                                trade_info.mint.clone(),
                                                trade_info.price, // Use price directly from TradeInfoFromToken (already scaled)
                                                amount_in,
                                                _token_amount,
                                                SwapProtocol::PumpFun, // Use PumpFun as the fallback protocol
                                                trade_info.clone(),
                                                import_env_var("SELLING_TIME").parse::<u64>().unwrap_or(600),
                                            );
                                            
                                            // Insert the token into the bought tokens map for monitoring
                                            BOUGHT_TOKEN_LIST.insert(trade_info.mint.clone(), bought_token_info);
                                            logger.log(format!("Added {} to bought tokens tracking", trade_info.mint));
                                            
                                            // Add to permanent blacklist (never rebuy this token)
                                            let timestamp = std::time::SystemTime::now()
                                                .duration_since(std::time::UNIX_EPOCH)
                                                .unwrap_or_default()
                                                .as_secs();
                                            BOUGHT_TOKENS_BLACKLIST.insert(trade_info.mint.clone(), timestamp);
                                            logger.log(format!("🚫 Added {} to permanent blacklist", trade_info.mint));
                                            
                                            // Start enhanced selling monitor for this token
                                            let app_state_clone = app_state.clone();
                                            let swap_config_clone = swap_config.clone();
                                            let _token_mint = trade_info.mint.clone();
                                            tokio::spawn(async move {
                                                start_enhanced_selling_monitor(app_state_clone, swap_config_clone).await;
                                            });
                                        }
                                        
                                        Ok(())
                                    } else {
                                        Err("Buy transaction verification failed".to_string())
                                    }
                                },
                                Err(e) => {
                                    Err(format!("Transaction verification error: {}", e))
                                },
                            }
                        },
                        Err(e) => {
                            Err(format!("Transaction error: {}", e))
                        },
                    }
                },
                Err(e) => {
                    Err(format!("Failed to build PumpFun buy instruction: {}", e))
                },
            }
        },
    };
    
    // Log execution time
    let elapsed = start_time.elapsed();
    logger.log(format!("Buy execution time: {:?}", elapsed));
    
    // Increment bought counter on success
    if result.is_ok() {
        // Update counters and tracking
        let bought_count = {
            let mut entry = BOUGHT_TOKENS.entry(()).or_insert(0);
            *entry += 1;
            *entry
        };
        logger.log(format!("Total bought: {}", bought_count));
        
        // Add token to bought token list for comprehensive tracking
        let bought_token_info = BoughtTokenInfo::new(
            trade_info.mint.clone(),
            trade_info.price, // Use price directly from TradeInfoFromToken (already scaled)
            amount_in, // SOL amount spent (using stored value)
            trade_info.token_change.abs(), // Token amount received
            protocol.clone(),
            trade_info.clone(),
            std::env::var("SELLING_TIME").unwrap_or_else(|_| "300".to_string()).parse().unwrap_or(300),
        );
        
        // Debug logging for token tracking
        println!("DEBUG TRACKING: Adding token {} to BOUGHT_TOKEN_LIST with entry_price: {}", 
            trade_info.mint, bought_token_info.entry_price);
        
        // Only add to tracking if entry_price is valid
        if bought_token_info.entry_price > 0 {
            BOUGHT_TOKEN_LIST.insert(trade_info.mint.clone(), bought_token_info);
            
            // Add to permanent blacklist (never rebuy this token)
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            BOUGHT_TOKENS_BLACKLIST.insert(trade_info.mint.clone(), timestamp);
            logger.log(format!("🚫 Added {} to permanent blacklist", trade_info.mint));
        } else {
            println!("WARNING: Refusing to track token {} with entry_price = 0", trade_info.mint);
        }
        
        // Token added to selling system via the selling_engine.update_metrics call above
        
        // Legacy tracking for compatibility
        TOKEN_TRACKING.entry(trade_info.mint.clone()).or_insert(TokenTrackingInfo {
            top_pnl: 0.0,
            last_sell_time: Instant::now(),
            completed_intervals: HashSet::new(),
            sell_attempts: 0,
            sell_success: 0,
        });
        
        // Get active tokens list
        let _active_tokens: Vec<String> = TOKEN_TRACKING.iter().map(|entry| entry.key().clone()).collect();
        let _sold_count = SOLD_TOKENS.get(&()).map(|r| *r).unwrap_or(0);
        
    }
    
    result
}

/// Internal wallet monitoring function using second GRPC stream
async fn start_wallet_monitoring_internal(app_state: Arc<AppState>) -> Result<(), String> {
    use futures_util::stream::StreamExt;
    use futures_util::{SinkExt, Sink};
    use yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient};
    use yellowstone_grpc_proto::geyser::{
        subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest, SubscribeRequestPing,
        SubscribeRequestFilterTransactions, SubscribeUpdate,
    };
    
    let logger = Logger::new("[WALLET-MONITOR-INTERNAL] => ".purple().bold().to_string());
    
    let second_grpc_http = std::env::var("SECOND_YELLOWSTONE_GRPC_HTTP")
        .map_err(|_| "SECOND_YELLOWSTONE_GRPC_HTTP not set".to_string())?;
    let second_grpc_token = std::env::var("SECOND_YELLOWSTONE_GRPC_TOKEN")
        .map_err(|_| "SECOND_YELLOWSTONE_GRPC_TOKEN not set".to_string())?;
    let wallet_pubkey = app_state.wallet.pubkey().to_string();
    

    
    // Connect to second Yellowstone gRPC
    let mut client = GeyserGrpcClient::build_from_shared(second_grpc_http)
        .map_err(|e| format!("Failed to build second client: {}", e))?
        .x_token::<String>(Some(second_grpc_token))
        .map_err(|e| format!("Failed to set x_token: {}", e))?
        .tls_config(ClientTlsConfig::new().with_native_roots())
        .map_err(|e| format!("Failed to set tls config: {}", e))?
        .connect()
        .await
        .map_err(|e| format!("Failed to connect to second gRPC: {}", e))?;

    // Set up subscribe
    let (subscribe_tx, mut stream) = client.subscribe().await
        .map_err(|e| format!("Failed to subscribe to second gRPC: {}", e))?;
    let subscribe_tx = Arc::new(tokio::sync::Mutex::new(subscribe_tx));

    // Set up wallet-specific subscription
    let subscription_request = SubscribeRequest {
        transactions: maplit::hashmap! {
            "WalletMonitor".to_owned() => SubscribeRequestFilterTransactions {
                vote: Some(false),
                failed: Some(false),
                signature: None,
                account_include: vec![wallet_pubkey.clone()],
                account_exclude: vec![],
                account_required: Vec::<String>::new(),
            }
        },
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    };
    
    subscribe_tx.lock().await.send(subscription_request).await
        .map_err(|e| format!("Failed to send wallet subscription: {}", e))?;

        // Process wallet transactions
    while SHOULD_CONTINUE_STREAMING.load(Ordering::SeqCst) {
        match stream.next().await {
            Some(msg_result) => {
                match msg_result {
                    Ok(msg) => {
                        if let Some(UpdateOneof::Transaction(txn)) = &msg.update_oneof {
                            if let Some(transaction) = &txn.transaction {
                                let signature_bytes = &transaction.signature;
                                let signature = bs58::encode(signature_bytes).into_string();
                                    
                                if let Some(meta) = &transaction.meta {
                                    let is_buy = meta.log_messages.iter().any(|log| {
                                        log.contains("Program log: Instruction: Buy") || log.contains("MintTo")
                                    });
                                    
                                    let is_sell = meta.log_messages.iter().any(|log| {
                                        log.contains("Program log: Instruction: Sell")
                                    });
                                    
                                    if is_sell {
                                        // Try to extract token mint and remove from bought list
                                        for token_balance in &meta.post_token_balances {
                                            if token_balance.ui_token_amount.as_ref().map(|ui| ui.ui_amount).unwrap_or(0.0) == 0.0 {
                                                // This token was sold completely
                                                BOUGHT_TOKEN_LIST.remove(&token_balance.mint);
                                                // Remove token from the global tracking system
                                                crate::processor::selling_strategy::TOKEN_METRICS.remove(&token_balance.mint);
                                                crate::processor::selling_strategy::TOKEN_TRACKING.remove(&token_balance.mint);
                                                
                                                // Check if all tokens are sold
                                                check_and_stop_streaming_if_all_sold(&logger).await;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    },
                    Err(e) => {
                        logger.log(format!("Wallet monitor stream error: {:?}", e).red().to_string());
                        // Check if it's a connection limit error
                        if format!("{:?}", e).contains("Maximum connection count reached") {
                            logger.log("🚫 Wallet monitor connection limit reached - closing stream".red().bold().to_string());
                        }
                        return Err(format!("Wallet monitor stream error: {:?}", e));
                    },
                }
            },
            None => {
                logger.log("Wallet monitor stream ended".yellow().to_string());
                break;
            }
        }
    }
    
    if !SHOULD_CONTINUE_STREAMING.load(Ordering::SeqCst) {
    }
    
    // Explicitly drop the stream and client to close connections
    drop(stream);
    drop(subscribe_tx);
    drop(client);
    

    
    Ok(())
}


/// Execute whale emergency sell with comprehensive multi-fallback system
/// Execute PumpFun emergency sell with zeroslot
async fn execute_pumpfun_emergency_sell_with_zeroslot(
    trade_info: &transaction_parser::TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<(), String> {
    let pump = crate::dex::pump_fun::Pump::new(
        app_state.rpc_nonblocking_client.clone(),
        app_state.rpc_client.clone(),
        app_state.wallet.clone(),
    );
    
    match pump.build_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            logger.log(format!("🐋 Generated PumpFun whale emergency sell instruction at price: {}", price));
            
            let recent_blockhash = match crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                Some(hash) => hash,
                None => {
                    return Err("Failed to get recent blockhash".to_string());
                }
            };
            
            match crate::block_engine::tx::new_signed_and_send_zeroslot(
                app_state.zeroslot_rpc_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    logger.log(format!("🐋 ZEROSLOT whale emergency sell transaction sent: {}", signature));
                    
                    verify_transaction(&signature.to_string(), app_state.clone(), logger).await
                        .map_err(|e| format!("Transaction verification error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Zeroslot transaction error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed to build PumpFun whale emergency sell instruction: {}", e)),
    }
}

/// Execute PumpSwap emergency sell with zeroslot
async fn execute_pumpswap_emergency_sell_with_zeroslot(
    trade_info: &transaction_parser::TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<(), String> {
    let pump_swap = crate::dex::pump_swap::PumpSwap::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );
    
    match pump_swap.build_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            logger.log(format!("🐋 Generated PumpSwap whale emergency sell instruction at price: {}", price));
            
            let recent_blockhash = match crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                Some(hash) => hash,
                None => {
                    return Err("Failed to get recent blockhash".to_string());
                }
            };
            
            match crate::block_engine::tx::new_signed_and_send_zeroslot(
                app_state.zeroslot_rpc_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    logger.log(format!("🐋 ZEROSLOT whale emergency sell transaction sent: {}", signature));
                    
                    verify_transaction(&signature.to_string(), app_state.clone(), logger).await
                        .map_err(|e| format!("Transaction verification error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Zeroslot transaction error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed to build PumpSwap whale emergency sell instruction: {}", e)),
    }
}

/// Execute emergency sell using Jupiter API as fallback
async fn execute_jupiter_emergency_sell(
    token_mint: &str,
    _amount_to_sell: f64,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<(), String> {
    logger.log(format!("🪐 Attempting Jupiter API emergency sell for {}", token_mint).cyan().to_string());
    
    // Create Jupiter client
    let jupiter_client = crate::library::jupiter_api::JupiterClient::new(app_state.rpc_nonblocking_client.clone());
    
    // Get token account address
    let wallet_pubkey = app_state.wallet.try_pubkey()
        .map_err(|e| format!("Failed to get wallet pubkey: {}", e))?;
    
    let token_pubkey = Pubkey::from_str(token_mint)
        .map_err(|e| format!("Invalid token mint: {}", e))?;
    
    let token_account = get_associated_token_address(&wallet_pubkey, &token_pubkey);
    
    // Get actual token balance
    let token_balance = match app_state.rpc_nonblocking_client.get_token_account(&token_account).await {
        Ok(Some(account)) => {
            let amount_value = account.token_amount.amount.parse::<u64>()
                .map_err(|e| format!("Failed to parse token amount: {}", e))?;
            amount_value
        },
        Ok(None) => {
            return Err("Token account not found".to_string());
        },
        Err(e) => {
            return Err(format!("Failed to get token balance: {}", e));
        }
    };
    
    if token_balance == 0 {
        return Err("No tokens to sell".to_string());
    }
    
    // Execute Jupiter sell with high slippage for emergency
    match jupiter_client.sell_token_with_jupiter(
        token_mint,
        token_balance,
        2000, // 20% slippage for emergency
        &app_state.wallet,
    ).await {
        Ok(signature) => {
            logger.log(format!("🪐 Jupiter emergency sell successful: {}", signature).green().to_string());
            Ok(())
        },
        Err(e) => {
            Err(format!("Jupiter emergency sell failed: {}", e))
        }
    }
}

/// Execute emergency sell with specified method (zeroslot or normal)
async fn execute_emergency_sell_with_method(
    trade_info: &transaction_parser::TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    protocol: &SwapProtocol,
    method: &str,
    logger: &Logger,
) -> Result<(), String> {
    match protocol {
        SwapProtocol::PumpFun => {
            if method == "zeroslot" {
                execute_pumpfun_emergency_sell_with_zeroslot(trade_info, sell_config, app_state, logger).await
            } else {
                execute_pumpfun_emergency_sell_with_normal(trade_info, sell_config, app_state, logger).await
            }
        },
        SwapProtocol::PumpSwap => {
            if method == "zeroslot" {
                execute_pumpswap_emergency_sell_with_zeroslot(trade_info, sell_config, app_state, logger).await
            } else {
                execute_pumpswap_emergency_sell_with_normal(trade_info, sell_config, app_state, logger).await
            }
        },
        SwapProtocol::RaydiumLaunchpad => {
            if method == "zeroslot" {
                execute_raydium_emergency_sell_with_zeroslot(trade_info, sell_config, app_state, logger).await
            } else {
                execute_raydium_emergency_sell_with_normal(trade_info, sell_config, app_state, logger).await
            }
        },
        SwapProtocol::Auto | SwapProtocol::Unknown => {
            logger.log("Auto/Unknown protocol, defaulting to PumpFun for emergency sell".yellow().to_string());
            if method == "zeroslot" {
                execute_pumpfun_emergency_sell_with_zeroslot(trade_info, sell_config, app_state, logger).await
            } else {
                execute_pumpfun_emergency_sell_with_normal(trade_info, sell_config, app_state, logger).await
            }
        },
    }
}

/// Execute PumpFun emergency sell with normal RPC (fallback)
async fn execute_pumpfun_emergency_sell_with_normal(
    trade_info: &transaction_parser::TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<(), String> {
    let pump = crate::dex::pump_fun::Pump::new(
        app_state.rpc_nonblocking_client.clone(),
        app_state.rpc_client.clone(),
        app_state.wallet.clone(),
    );
    
    match pump.build_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            logger.log(format!("🐋 Generated PumpFun emergency sell (normal RPC) at price: {}", price));
            
            let recent_blockhash = match crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                Some(hash) => hash,
                None => {
                    return Err("Failed to get recent blockhash".to_string());
                }
            };
            
            match crate::block_engine::tx::new_signed_and_send_normal(
                app_state.rpc_nonblocking_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    logger.log(format!("🐋 NORMAL RPC emergency sell transaction sent: {}", signature));
                    Ok(())
                },
                Err(e) => Err(format!("Normal RPC transaction error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed to build PumpFun emergency sell instruction: {}", e)),
    }
}

/// Execute PumpSwap emergency sell with normal RPC (fallback)
async fn execute_pumpswap_emergency_sell_with_normal(
    trade_info: &transaction_parser::TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<(), String> {
    let pump_swap = crate::dex::pump_swap::PumpSwap::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );
    
    match pump_swap.build_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            logger.log(format!("🐋 Generated PumpSwap emergency sell (normal RPC) at price: {}", price));
            
            let recent_blockhash = match crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                Some(hash) => hash,
                None => {
                    return Err("Failed to get recent blockhash".to_string());
                }
            };
            
            match crate::block_engine::tx::new_signed_and_send_normal(
                app_state.rpc_nonblocking_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    logger.log(format!("🐋 NORMAL RPC emergency sell transaction sent: {}", signature));
                    Ok(())
                },
                Err(e) => Err(format!("Normal RPC transaction error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed to build PumpSwap emergency sell instruction: {}", e)),
    }
}

/// Execute Raydium emergency sell with normal RPC (fallback)
async fn execute_raydium_emergency_sell_with_normal(
    trade_info: &transaction_parser::TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<(), String> {
    let raydium = crate::dex::raydium_launchpad::Raydium::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );
    
    match raydium.build_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            logger.log(format!("🐋 Generated Raydium emergency sell (normal RPC) at price: {}", price));
            
            let recent_blockhash = match crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                Some(hash) => hash,
                None => {
                    return Err("Failed to get recent blockhash".to_string());
                }
            };
            
            match crate::block_engine::tx::new_signed_and_send_normal(
                app_state.rpc_nonblocking_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    logger.log(format!("🐋 NORMAL RPC emergency sell transaction sent: {}", signature));
                    Ok(())
                },
                Err(e) => Err(format!("Normal RPC transaction error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed to build Raydium emergency sell instruction: {}", e)),
    }
}

/// Enhanced sell execution with comprehensive selling logic
pub async fn execute_enhanced_sell(
    token_mint: String,
    app_state: Arc<AppState>,
    swap_config: Arc<SwapConfig>,
) -> Result<(), String> {
    let logger = Logger::new("[ENHANCED-SELL] => ".green().to_string());
    
    // Get token info from global tracking
    let mut token_info = match BOUGHT_TOKEN_LIST.get_mut(&token_mint) {
        Some(info) => info,
        None => {
            return Err(format!("Token {} not found in tracking list", token_mint));
        }
    };
    
    // Get selling action based on comprehensive rules
    let selling_action = token_info.get_selling_action();
    
    // Debug logging for tokens with invalid entry price
    if token_info.entry_price == 0 {
        logger.log(format!("WARNING: Token {} has entry_price = 0, buy transaction may not be processed yet", token_mint).yellow().to_string());
    }
    
    match selling_action {
        SellingAction::Hold => {
            logger.log(format!("Holding token {}", token_mint));
            return Ok(());
        },
        SellingAction::SellAll(reason) => {
            logger.log(format!("Selling ALL of token {} - Reason: {}", token_mint, reason));
            execute_sell_all_enhanced(&token_mint, &mut token_info, app_state, swap_config).await
        }
    }
}

/// Execute sell all with zeroslot for maximum speed
async fn execute_sell_all_enhanced(
    token_mint: &str,
    token_info: &mut BoughtTokenInfo,
    app_state: Arc<AppState>,
    swap_config: Arc<SwapConfig>,
) -> Result<(), String> {
    let logger = Logger::new("[SELL-ALL-ENHANCED] => ".red().to_string());
    
    // Get current token balance
    let wallet_pubkey = app_state.wallet.try_pubkey()
        .map_err(|e| format!("Failed to get wallet pubkey: {}", e))?;
    let token_pubkey = Pubkey::from_str(token_mint)
        .map_err(|e| format!("Invalid token mint: {}", e))?;
    let ata = get_associated_token_address(&wallet_pubkey, &token_pubkey);
    
    let token_amount = match app_state.rpc_nonblocking_client.get_token_account(&ata).await {
        Ok(Some(account)) => {
            let amount_value = account.token_amount.amount.parse::<f64>()
                .map_err(|e| format!("Failed to parse token amount: {}", e))?;
            amount_value / 10f64.powi(account.token_amount.decimals as i32)
        },
        Ok(None) => {
            return Err(format!("No token account found for mint: {}", token_mint));
        },
        Err(e) => {
            return Err(format!("Failed to get token account: {}", e));
        }
    };
    
    if token_amount <= 0.0 {
        logger.log("No tokens to sell".yellow().to_string());
        return Ok(());
    }
    
    // Create sell config
    let mut sell_config = (*swap_config).clone();
    sell_config.swap_direction = SwapDirection::Sell;
    sell_config.in_type = SwapInType::Pct; // Use percentage for sell all operations
    sell_config.amount_in = 1.0; // Sell 100% of tokens
    sell_config.slippage = 1000; // 10% slippage for emergency sells
    
    // Create trade info for sell using original trade info
    let trade_info = create_sell_trade_info_from_original(token_mint, token_amount, &token_info.trade_info);
    
    let result = match token_info.protocol {
        SwapProtocol::PumpFun => {
            execute_pumpfun_sell_with_zeroslot(&trade_info, sell_config, app_state.clone(), &logger).await
        },
        SwapProtocol::PumpSwap => {
            execute_pumpswap_sell_with_zeroslot(&trade_info, sell_config, app_state.clone(), &logger).await
        },
        SwapProtocol::RaydiumLaunchpad => {
            execute_raydium_sell_with_zeroslot(&trade_info, sell_config, app_state.clone(), &logger).await
        },
        SwapProtocol::Auto | SwapProtocol::Unknown => {
            logger.log("Auto/Unknown protocol detected, defaulting to PumpFun for sell all".yellow().to_string());
            execute_pumpfun_sell_with_zeroslot(&trade_info, (*swap_config).clone(), app_state.clone(), &logger).await
        },
    };
    
    if result.is_ok() {
        // Use comprehensive verification and cleanup
        match verify_sell_transaction_and_cleanup(
            token_mint,
            None, // No specific transaction signature for enhanced sell
            app_state.clone(),
            &logger,
        ).await {
            Ok(cleaned_up) => {
                if cleaned_up {
                    logger.log(format!("✅ Comprehensive cleanup completed for sell all: {}", token_mint));
                } else {
                    logger.log(format!("⚠️  Sell all cleanup verification failed for: {}", token_mint).yellow().to_string());
                }
            },
            Err(e) => {
                logger.log(format!("❌ Error during sell all cleanup verification: {}", e).red().to_string());
                // Fallback to basic removal
                BOUGHT_TOKEN_LIST.remove(token_mint);
                TOKEN_TRACKING.remove(token_mint);
                logger.log(format!("Fallback: Removed {} from basic tracking systems", token_mint));
                
                // Check if all tokens are sold and stop streaming if needed
                check_and_stop_streaming_if_all_sold(&logger).await;
            }
        }
    }
    
    result
}

/// Execute progressive sell with normal transaction method


/// Clean up tracking systems by removing tokens with zero balance
async fn cleanup_token_tracking(app_state: &Arc<AppState>) {
    let logger = Logger::new("[TRACKING-CLEANUP] => ".blue().to_string());
    
    // Get all tokens from both tracking systems
    let tokens_to_check: Vec<String> = BOUGHT_TOKEN_LIST.iter()
        .map(|entry| entry.key().clone())
        .collect();
    
    if tokens_to_check.is_empty() {
        return;
    }
    
    for token_mint in tokens_to_check {
        if let Ok(wallet_pubkey) = app_state.wallet.try_pubkey() {
            if let Ok(token_pubkey) = Pubkey::from_str(&token_mint) {
                let ata = get_associated_token_address(&wallet_pubkey, &token_pubkey);
                
                match app_state.rpc_nonblocking_client.get_token_account(&ata).await {
                    Ok(account_result) => {
                        match account_result {
                            Some(account) => {
                                if let Ok(amount_value) = account.token_amount.amount.parse::<f64>() {
                                    let decimal_amount = amount_value / 10f64.powi(account.token_amount.decimals as i32);
                                    if decimal_amount <= 0.000001 {
                                        // Remove from all tracking systems
                                        BOUGHT_TOKEN_LIST.remove(&token_mint);
                                        TOKEN_TRACKING.remove(&token_mint);
                                        
                                        // Check if all tokens are sold and stop streaming if needed
                                        check_and_stop_streaming_if_all_sold(&logger).await;
                                    }
                                }
                            },
                            None => {
                                // Token account doesn't exist, remove from tracking
                                BOUGHT_TOKEN_LIST.remove(&token_mint);
                                TOKEN_TRACKING.remove(&token_mint);
                                
                                // Check if all tokens are sold and stop streaming if needed
                                check_and_stop_streaming_if_all_sold(&logger).await;
                            }
                        }
                    },
                    Err(_) => {
                        // Error getting account, keep in tracking for now
                    }
                }
            }
        }
    }
}

/// Monitor cleanup tasks for token tracking (transaction-driven selling logic handles actual selling)
async fn start_enhanced_selling_monitor(
    app_state: Arc<AppState>,
    _swap_config: Arc<SwapConfig>,
) {
    let logger = Logger::new("[ENHANCED-SELLING-MONITOR] => ".cyan().to_string());
    
    // Run cleanup every 30 seconds
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    let mut cleanup_cycle = 0;
    
    loop {
        interval.tick().await;
        cleanup_cycle += 1;
        
        // Run basic cleanup every cycle (30 seconds)
        cleanup_token_tracking(&app_state).await;
        
        // Run comprehensive cleanup every 4 cycles (2 minutes)
        if cleanup_cycle >= 4 {
            periodic_comprehensive_cleanup(app_state.clone(), &logger).await;
            cleanup_cycle = 0;
        }
    }
}

/// Update token price from TradeInfoFromToken (transaction-driven only)
/// This function is now only used for updating prices from parsed transaction data
async fn update_token_price_from_trade_info(
    token_mint: &str,
    trade_info: &crate::processor::transaction_parser::TradeInfoFromToken,
) -> Result<(), String> {
    if let Some(mut token_info) = BOUGHT_TOKEN_LIST.get_mut(token_mint) {
        // Use the price from the parsed transaction data directly
        let current_price = trade_info.price;
        
        // Debug logging for price updates
        println!("DEBUG PRICE UPDATE FROM TRANSACTION: Token {} - Protocol: {:?}, Old: {}, New: {}", 
            token_mint, token_info.protocol, token_info.current_price, current_price);
        
        // Update the price using the enhanced method
        token_info.update_price(current_price);
        
        Ok(())
    } else {
        Err(format!("Token {} not found in tracking", token_mint))
    }
}

/// Create trade info for selling using original trade info to preserve important fields
fn create_sell_trade_info_from_original(
    token_mint: &str,
    token_amount: f64,
    original_trade_info: &transaction_parser::TradeInfoFromToken,
) -> transaction_parser::TradeInfoFromToken {
    transaction_parser::TradeInfoFromToken {
        dex_type: original_trade_info.dex_type.clone(),
        slot: original_trade_info.slot,
        signature: "enhanced_sell".to_string(),
        pool_id: original_trade_info.pool_id.clone(),
        mint: token_mint.to_string(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        is_buy: false,
        price: original_trade_info.price,
        is_reverse_when_pump_swap: original_trade_info.is_reverse_when_pump_swap,
        coin_creator: original_trade_info.coin_creator.clone(), // This is the key field that was missing!
        sol_change: 0.0,
        token_change: token_amount,
        liquidity: original_trade_info.liquidity,
        virtual_sol_reserves: original_trade_info.virtual_sol_reserves,
        virtual_token_reserves: original_trade_info.virtual_token_reserves,
    }
}



/// Execute PumpFun sell with zeroslot
async fn execute_pumpfun_sell_with_zeroslot(
    trade_info: &transaction_parser::TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<(), String> {
    let pump = crate::dex::pump_fun::Pump::new(
        app_state.rpc_nonblocking_client.clone(),
        app_state.rpc_client.clone(),
        app_state.wallet.clone(),
    );
    
    match pump.build_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            logger.log(format!("Generated PumpFun sell instruction at price: {}", price));
            
            let recent_blockhash = match crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                Some(hash) => hash,
                None => {
                    return Err("Failed to get recent blockhash".to_string());
                }
            };
            
            match crate::block_engine::tx::new_signed_and_send_zeroslot(
                app_state.zeroslot_rpc_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    logger.log(format!("ZEROSLOT sell transaction sent: {}", signature));
                    
                    verify_transaction(&signature.to_string(), app_state.clone(), logger).await
                        .map_err(|e| format!("Transaction verification error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Transaction error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed to build PumpFun sell instruction: {}", e)),
    }
}

/// Execute PumpSwap sell with zeroslot
async fn execute_pumpswap_sell_with_zeroslot(
    trade_info: &transaction_parser::TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<(), String> {
    let pump_swap = crate::dex::pump_swap::PumpSwap::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );
    
    match pump_swap.build_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            logger.log(format!("Generated PumpSwap sell instruction at price: {}", price));
            
            let recent_blockhash = match crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                Some(hash) => hash,
                None => {
                    return Err("Failed to get recent blockhash".to_string());
                }
            };
            
            match crate::block_engine::tx::new_signed_and_send_zeroslot(
                app_state.zeroslot_rpc_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    logger.log(format!("🐋 ZEROSLOT whale emergency sell transaction sent: {}", signature));
                    
                    verify_transaction(&signature.to_string(), app_state.clone(), logger).await
                        .map_err(|e| format!("Transaction verification error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Zeroslot transaction error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed to build PumpSwap whale emergency sell instruction: {}", e)),
    }
}

/// Execute Raydium emergency sell with zeroslot
async fn execute_raydium_emergency_sell_with_zeroslot(
    trade_info: &transaction_parser::TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<(), String> {
    let raydium = crate::dex::raydium_launchpad::Raydium::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );
    
    match raydium.build_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            logger.log(format!("🐋 Generated Raydium whale emergency sell instruction at price: {}", price));
            
            let recent_blockhash = match crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                Some(hash) => hash,
                None => {
                    return Err("Failed to get recent blockhash".to_string());
                }
            };
            
            match crate::block_engine::tx::new_signed_and_send_zeroslot(
                app_state.zeroslot_rpc_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    logger.log(format!("🐋 ZEROSLOT Raydium whale emergency sell transaction sent: {}", signature));
                    
                    verify_transaction(&signature.to_string(), app_state.clone(), logger).await
                        .map_err(|e| format!("Transaction verification error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Zeroslot transaction error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed to build Raydium whale emergency sell instruction: {}", e)),
    }
}

/// Execute PumpFun sell with normal transaction method
async fn execute_pumpfun_sell_with_normal(
    trade_info: &transaction_parser::TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<(), String> {
    let pump = crate::dex::pump_fun::Pump::new(
        app_state.rpc_nonblocking_client.clone(),
        app_state.rpc_client.clone(),
        app_state.wallet.clone(),
    );
    
    match pump.build_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            logger.log(format!("Generated PumpFun sell instruction at price: {}", price));
            
            let recent_blockhash = match crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                Some(hash) => hash,
                None => {
                    return Err("Failed to get recent blockhash".to_string());
                }
            };
            
            match crate::block_engine::tx::new_signed_and_send_normal(
                app_state.rpc_nonblocking_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    logger.log(format!("NORMAL sell transaction sent: {}", signature));
                    
                    verify_transaction(&signature.to_string(), app_state.clone(), logger).await
                        .map_err(|e| format!("Transaction verification error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Transaction error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed to build PumpFun sell instruction: {}", e)),
    }
}

/// Execute PumpSwap sell with normal transaction method
async fn execute_pumpswap_sell_with_normal(
    trade_info: &transaction_parser::TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<(), String> {
    let pump_swap = crate::dex::pump_swap::PumpSwap::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );
    
    match pump_swap.build_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            logger.log(format!("Generated PumpSwap sell instruction at price: {}", price));
            
            let recent_blockhash = match crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                Some(hash) => hash,
                None => {
                    return Err("Failed to get recent blockhash".to_string());
                }
            };
            
            match crate::block_engine::tx::new_signed_and_send_normal(
                app_state.rpc_nonblocking_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    logger.log(format!("NORMAL sell transaction sent: {}", signature));
                    
                    verify_transaction(&signature.to_string(), app_state.clone(), logger).await
                        .map_err(|e| format!("Transaction verification error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Transaction error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed to build PumpSwap sell instruction: {}", e)),
    }
}

/// Execute Raydium sell with zeroslot
async fn execute_raydium_sell_with_zeroslot(
    trade_info: &transaction_parser::TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<(), String> {
    let raydium = crate::dex::raydium_launchpad::Raydium::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );
    
    match raydium.build_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            logger.log(format!("Generated Raydium sell instruction at price: {}", price));
            
            let recent_blockhash = match crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                Some(hash) => hash,
                None => {
                    return Err("Failed to get recent blockhash".to_string());
                }
            };
            
            match crate::block_engine::tx::new_signed_and_send_zeroslot(
                app_state.zeroslot_rpc_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    logger.log(format!("ZEROSLOT Raydium sell transaction sent: {}", signature));
                    
                    verify_transaction(&signature.to_string(), app_state.clone(), logger).await
                        .map_err(|e| format!("Transaction verification error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Transaction error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed to build Raydium sell instruction: {}", e)),
    }
}

/// Execute Raydium sell with normal transaction method
async fn execute_raydium_sell_with_normal(
    trade_info: &transaction_parser::TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<(), String> {
    let raydium = crate::dex::raydium_launchpad::Raydium::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );
    
    match raydium.build_swap_from_parsed_data(trade_info, sell_config).await {
        Ok((keypair, instructions, price)) => {
            logger.log(format!("Generated Raydium sell instruction at price: {}", price));
            
            let recent_blockhash = match crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                Some(hash) => hash,
                None => {
                    return Err("Failed to get recent blockhash".to_string());
                }
            };
            
            match crate::block_engine::tx::new_signed_and_send_normal(
                app_state.rpc_nonblocking_client.clone(),
                recent_blockhash,
                &keypair,
                instructions,
                logger,
            ).await {
                Ok(signatures) => {
                    if signatures.is_empty() {
                        return Err("No transaction signature returned".to_string());
                    }
                    
                    let signature = &signatures[0];
                    logger.log(format!("NORMAL Raydium sell transaction sent: {}", signature));
                    
                    verify_transaction(&signature.to_string(), app_state.clone(), logger).await
                        .map_err(|e| format!("Transaction verification error: {}", e))?;
                    
                    Ok(())
                },
                Err(e) => Err(format!("Transaction error: {}", e)),
            }
        },
        Err(e) => Err(format!("Failed to build Raydium sell instruction: {}", e)),
    }
}

/// Execute sell operation for a token
pub async fn execute_sell(
    token_mint: String,
    trade_info: transaction_parser::TradeInfoFromToken,
    app_state: Arc<AppState>,
    swap_config: Arc<SwapConfig>,
    protocol: SwapProtocol,
    chunks: Option<usize>,
    interval_ms: Option<u64>,
) -> Result<(), String> {
    let logger = Logger::new("[EXECUTE-SELL] => ".green().to_string());
    let start_time = Instant::now();
    
    logger.log(format!("Selling token: {}", token_mint));
    
    // Protocol string for notifications
    let _protocol_str = match protocol {
        SwapProtocol::PumpSwap => "PumpSwap",
        SwapProtocol::PumpFun => "PumpFun",
        SwapProtocol::RaydiumLaunchpad => "RaydiumLaunchpad",
        _ => "Unknown",
    };
    
    // Create a minimal trade info for notification using the new structure
    let notification_trade_info = transaction_parser::TradeInfoFromToken {
        dex_type: match protocol {
            SwapProtocol::PumpSwap => transaction_parser::DexType::PumpSwap,
            SwapProtocol::PumpFun => transaction_parser::DexType::PumpFun,
            SwapProtocol::RaydiumLaunchpad => transaction_parser::DexType::RaydiumLaunchpad,
            _ => transaction_parser::DexType::Unknown,
        },
        slot: trade_info.slot,
        signature: trade_info.signature.clone(),
        pool_id: trade_info.pool_id.clone(),
        mint: trade_info.mint.clone(),
        timestamp: trade_info.timestamp,
        is_buy: false, // This is a sell notification
        price: trade_info.price,
        is_reverse_when_pump_swap: trade_info.is_reverse_when_pump_swap,
        coin_creator: trade_info.coin_creator.clone(),
        sol_change: trade_info.sol_change,
        token_change: trade_info.token_change,
        liquidity: trade_info.liquidity,
        virtual_sol_reserves: trade_info.virtual_sol_reserves,
        virtual_token_reserves: trade_info.virtual_token_reserves,
    };

    // Create a modified swap config for selling
    let mut sell_config = (*swap_config).clone();
    sell_config.swap_direction = SwapDirection::Sell;
    // CRITICAL FIX: Always sell 100% of token balance
    sell_config.in_type = SwapInType::Pct;
    sell_config.amount_in = 1.0; // 100% of balance

    // Get wallet pubkey - handle the error properly instead of using ?
    let wallet_pubkey = match app_state.wallet.try_pubkey() {
        Ok(pubkey) => pubkey,
        Err(e) => return Err(format!("Failed to get wallet pubkey: {}", e)),
    };

    // Get token account to determine how much we own
    let token_pubkey = match Pubkey::from_str(&token_mint) {
        Ok(pubkey) => pubkey,
        Err(e) => return Err(format!("Invalid token mint address: {}", e)),
    };
    let ata = get_associated_token_address(&wallet_pubkey, &token_pubkey);

    // Get token account and amount
    let token_amount = match app_state.rpc_nonblocking_client.get_token_account(&ata).await {
        Ok(Some(account)) => {
            // Parse the amount string instead of casting
            let amount_value = match account.token_amount.amount.parse::<f64>() {
                Ok(val) => val,
                Err(e) => return Err(format!("Failed to parse token amount: {}", e)),
            };
            let decimal_amount = amount_value / 10f64.powi(account.token_amount.decimals as i32);
            logger.log(format!("Token amount to sell (100% of balance): {}", decimal_amount));
            decimal_amount
        },
        Ok(None) => {
            return Err(format!("No token account found for mint: {}", token_mint));
        },
        Err(e) => {
            return Err(format!("Failed to get token account: {}", e));
        }
    };
    
    // Update trade info with token amount
    let notification_trade_info = notification_trade_info.clone();
    // token_amount is already set in token_change field

    // Now that we have token_amount, set slippage based on token value
    // Use a fixed slippage since calculate_dynamic_slippage is private
    let token_value = token_amount * 0.01; // Estimate value at 0.01 SOL per token as a conservative default
    let slippage_bps = if token_value > 10.0 {
        300 // 3% for high value tokens (300 basis points)
    } else if token_value > 1.0 {
        200 // 2% for medium value tokens (200 basis points)
    } else {
        100 // 1% for low value tokens (100 basis points)
    };

    logger.log(format!("Using slippage of {}%", slippage_bps as f64 / 100.0));
    sell_config.slippage = slippage_bps;
    
    // Always use immediate sell (progressive selling removed)
    if false {
        let chunks_count = chunks.unwrap_or(3);
        let interval = interval_ms.unwrap_or(2000); // 2 seconds default
        
        logger.log(format!("Executing progressive sell in {} chunks with {} ms intervals", chunks_count, interval));
        
        // Calculate chunk size
        let chunk_size = token_amount / chunks_count as f64;
        
        // Execute each chunk
        for i in 0..chunks_count {
            // Create a fresh sell config for each iteration by cloning
            let mut chunk_sell_config = (*swap_config).clone();
            chunk_sell_config.swap_direction = SwapDirection::Sell;
            chunk_sell_config.slippage = slippage_bps;
            
            // Adjust the final chunk to account for any rounding errors
            let amount_to_sell = if i == chunks_count - 1 {
                // For the last chunk, sell whatever is left
                match app_state.rpc_nonblocking_client.get_token_account(&ata).await {
                    Ok(Some(account)) => {
                        // Parse the amount string instead of casting
                        let amount_value = match account.token_amount.amount.parse::<f64>() {
                            Ok(val) => val,
                            Err(e) => return Err(format!("Failed to parse token amount: {}", e)),
                        };
                        let remaining = amount_value / 10f64.powi(account.token_amount.decimals as i32);
                        if remaining < 0.000001 { // Very small amount, not worth selling
                            logger.log("Remaining amount too small, skipping final chunk".to_string());
                            break;
                        }
                        remaining
                    },
                    Ok(None) => chunk_size, // Fallback if we can't get the account
                    Err(e) => return Err(format!("Failed to get token account: {}", e)),
                }
            } else {
                chunk_size
            };
            
            if amount_to_sell <= 0.0 {
                logger.log("No tokens left to sell in this chunk".to_string());
                continue;
            }
            
            // Update config for this chunk
            chunk_sell_config.amount_in = amount_to_sell;
            
            logger.log(format!("Selling chunk {}/{}: {} tokens", i + 1, chunks_count, amount_to_sell));
            
            // Update trade info for this chunk
            let mut chunk_trade_info = notification_trade_info.clone();
            chunk_trade_info.token_change = amount_to_sell;
            
            // Execute sell based on protocol
            let result = match protocol {
                SwapProtocol::PumpFun => {
                    logger.log("Using PumpFun protocol for sell".to_string());
                    
                    // Create the PumpFun instance
                    let pump = crate::dex::pump_fun::Pump::new(
                        app_state.rpc_nonblocking_client.clone(),
                        app_state.rpc_client.clone(),
                        app_state.wallet.clone(),
                    );
                    
                    // Create a minimal trade info struct for the sell
                    let trade_info_clone = transaction_parser::TradeInfoFromToken {
                        dex_type: transaction_parser::DexType::PumpFun,
                        slot: 0,
                        signature: "standard_sell".to_string(),
                        pool_id: String::new(),
                        mint: token_mint.clone(),
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                        is_buy: false,
                        price: 0,
                        is_reverse_when_pump_swap: false,
                        coin_creator: None,
                        sol_change: 0.0,
                        token_change: amount_to_sell,
                        liquidity: 0.0,
                        virtual_sol_reserves: 0,
                        virtual_token_reserves: 0,
                    };
                    
                    // Build swap instructions for sell
                    match pump.build_swap_from_parsed_data(&trade_info_clone, sell_config.clone()).await {
                        Ok((keypair, instructions, price)) => {
                            logger.log(format!("Generated PumpFun sell instruction at price: {}", price));
                            // Execute the transaction
                            match crate::block_engine::tx::new_signed_and_send_zeroslot(
                                app_state.zeroslot_rpc_client.clone(),
                                match crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                                    Some(hash) => hash,
                                    None => {
                                        logger.log("Failed to get recent blockhash".red().to_string());
                                        return Err("Failed to get recent blockhash".to_string());
                                    }
                                },
                                &keypair,
                                instructions,
                                &logger,
                            ).await {
                                Ok(signatures) => {
                                    if signatures.is_empty() {
                                        return Err("No transaction signature returned".to_string());
                                    }
                                    
                                    let signature = &signatures[0];
                                    logger.log(format!("Sell transaction sent: {}", signature));
                                    
                                    // Verify transaction
                                    match verify_transaction(&signature.to_string(), app_state.clone(), &logger).await {
                                        Ok(verified) => {
                                            if verified {
                                                logger.log("Sell transaction verified successfully".to_string());
                                                
                                                Ok(())
                                            } else {
                                                Err("Sell transaction verification failed".to_string())
                                            }
                                        },
                                        Err(e) => {
                                            Err(format!("Transaction verification error: {}", e))
                                        },
                                    }
                                },
                                Err(e) => {
                                    Err(format!("Transaction error: {}", e))
                                },
                            }
                        },
                        Err(e) => {
                            Err(format!("Failed to build PumpFun sell instruction: {}", e))
                        },
                    }
                },
                SwapProtocol::PumpSwap => {
                    logger.log("Using PumpSwap protocol for sell".to_string());
                    
                    // Create the PumpSwap instance
                    let pump_swap = crate::dex::pump_swap::PumpSwap::new(
                        app_state.wallet.clone(),
                        Some(app_state.rpc_client.clone()),
                        Some(app_state.rpc_nonblocking_client.clone()),
                    );
                    
                    // Create a minimal trade info struct for the sell
                    let trade_info_clone = transaction_parser::TradeInfoFromToken {
                        dex_type: transaction_parser::DexType::PumpSwap,
                        slot: trade_info.slot,
                        signature: "standard_sell".to_string(),
                        pool_id: trade_info.pool_id.clone(),
                        mint: token_mint.clone(),
                        timestamp: trade_info.timestamp,
                        is_buy: false,
                        price: trade_info.price,
                        is_reverse_when_pump_swap: trade_info.is_reverse_when_pump_swap,
                        coin_creator: trade_info.coin_creator.clone(),
                        sol_change: trade_info.sol_change,
                        token_change: amount_to_sell,
                        liquidity: trade_info.liquidity,
                        virtual_sol_reserves: trade_info.virtual_sol_reserves,
                        virtual_token_reserves: trade_info.virtual_token_reserves,
                    };
                    
                    // Build swap instructions for sell - use chunk_sell_config
                    match pump_swap.build_swap_from_parsed_data(&trade_info_clone, sell_config.clone()).await {
                        Ok((keypair, instructions, price)) => {
                            // Get recent blockhash from the processor
                            let recent_blockhash = match crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                                Some(hash) => hash,
                                None => {
                                    logger.log("Failed to get recent blockhash".red().to_string());
                                    return Err("Failed to get recent blockhash".to_string());
                                }
                            };
                            logger.log(format!("Generated PumpSwap sell instruction at price: {}", price));
                            logger.log(format!("copy transaction {}", trade_info_clone.signature));
                            
                            // Execute the transaction
                            match crate::block_engine::tx::new_signed_and_send_zeroslot(
                                app_state.zeroslot_rpc_client.clone(),
                                recent_blockhash,
                                &keypair,
                                instructions,
                                &logger,
                            ).await {
                                Ok(signatures) => {
                                    if signatures.is_empty() {
                                        return Err("No transaction signature returned".to_string());
                                    }
                                    
                                    let signature = &signatures[0];
                                    logger.log(format!("Sell transaction sent: {}", signature));
                                    
                                    // Verify transaction
                                    match verify_transaction(&signature.to_string(), app_state.clone(), &logger).await {
                                        Ok(verified) => {
                                            if verified {
                                                logger.log("Sell transaction verified successfully".to_string());
                                                
                                                
                                                Ok(())
                                            } else {
                                                
                                                Err("Sell transaction verification failed".to_string())
                                            }
                                        },
                                        Err(e) => {
                                            Err(format!("Transaction verification error: {}", e))
                                        },
                                    }
                                },
                                Err(e) => {
                                    Err(format!("Transaction error: {}", e))
                                },
                            }
                        },
                        Err(e) => {
                            Err(format!("Failed to build PumpSwap sell instruction: {}", e))
                        },
                    }
                },
                SwapProtocol::RaydiumLaunchpad => {
                    logger.log("Using Raydium protocol for sell".to_string());
                    
                    let raydium = crate::dex::raydium_launchpad::Raydium::new(
                        app_state.wallet.clone(),
                        Some(app_state.rpc_client.clone()),
                        Some(app_state.rpc_nonblocking_client.clone()),
                    );
                    
                    let trade_info_clone = transaction_parser::TradeInfoFromToken {
                        dex_type: transaction_parser::DexType::RaydiumLaunchpad,
                        slot: trade_info.slot,
                        signature: "standard_sell".to_string(),
                        pool_id: trade_info.pool_id.clone(),
                        mint: token_mint.clone(),
                        timestamp: trade_info.timestamp,
                        is_buy: false,
                        price: trade_info.price,
                        is_reverse_when_pump_swap: trade_info.is_reverse_when_pump_swap,
                        coin_creator: trade_info.coin_creator.clone(),
                        sol_change: trade_info.sol_change,
                        token_change: amount_to_sell,
                        liquidity: trade_info.liquidity,
                        virtual_sol_reserves: trade_info.virtual_sol_reserves,
                        virtual_token_reserves: trade_info.virtual_token_reserves,
                    };
                    
                    match raydium.build_swap_from_parsed_data(&trade_info_clone, sell_config.clone()).await {
                        Ok((keypair, instructions, price)) => {
                            let recent_blockhash = match crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                                Some(hash) => hash,
                                None => {
                                    logger.log("Failed to get recent blockhash".red().to_string());
                                    return Err("Failed to get recent blockhash".to_string());
                                }
                            };
                            logger.log(format!("Generated Raydium sell instruction at price: {}", price));
                            
                            match crate::block_engine::tx::new_signed_and_send_zeroslot(
                                app_state.zeroslot_rpc_client.clone(),
                                recent_blockhash,
                                &keypair,
                                instructions,
                                &logger,
                            ).await {
                                Ok(signatures) => {
                                    if signatures.is_empty() {
                                        return Err("No transaction signature returned".to_string());
                                    }
                                    
                                    let signature = &signatures[0];
                                    logger.log(format!("Sell transaction sent: {}", signature));
                                    
                                    match verify_transaction(&signature.to_string(), app_state.clone(), &logger).await {
                                        Ok(verified) => {
                                            if verified {
                                                logger.log("Sell transaction verified successfully".to_string());
                                                Ok(())
                                            } else {
                                                Err("Sell transaction verification failed".to_string())
                                            }
                                        },
                                        Err(e) => {
                                            Err(format!("Transaction verification error: {}", e))
                                        },
                                    }
                                },
                                Err(e) => {
                                    Err(format!("Transaction error: {}", e))
                                },
                            }
                        },
                        Err(e) => {
                            Err(format!("Failed to build Raydium sell instruction: {}", e))
                        },
                    }
                },
                SwapProtocol::Auto | SwapProtocol::Unknown => {
                    logger.log("Auto/Unknown protocol detected, defaulting to PumpFun for sell".yellow().to_string());
                    
                    let pump = crate::dex::pump_fun::Pump::new(
                        app_state.rpc_nonblocking_client.clone(),
                        app_state.rpc_client.clone(),
                        app_state.wallet.clone(),
                    );
                    
                    let trade_info_clone = transaction_parser::TradeInfoFromToken {
                        dex_type: transaction_parser::DexType::PumpFun,
                        slot: 0,
                        signature: "standard_sell".to_string(),
                        pool_id: String::new(),
                        mint: token_mint.clone(),
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                        is_buy: false,
                        price: 0,
                        is_reverse_when_pump_swap: false,
                        coin_creator: None,
                        sol_change: 0.0,
                        token_change: amount_to_sell,
                        liquidity: 0.0,
                        virtual_sol_reserves: 0,
                        virtual_token_reserves: 0,
                    };
                    
                    match pump.build_swap_from_parsed_data(&trade_info_clone, sell_config.clone()).await {
                        Ok((keypair, instructions, price)) => {
                            logger.log(format!("Generated PumpFun sell instruction at price: {}", price));
                            match crate::block_engine::tx::new_signed_and_send_zeroslot(
                                app_state.zeroslot_rpc_client.clone(),
                                match crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                                    Some(hash) => hash,
                                    None => {
                                        logger.log("Failed to get recent blockhash".red().to_string());
                                        return Err("Failed to get recent blockhash".to_string());
                                    }
                                },
                                &keypair,
                                instructions,
                                &logger,
                            ).await {
                                Ok(signatures) => {
                                    if signatures.is_empty() {
                                        return Err("No transaction signature returned".to_string());
                                    }
                                    
                                    let signature = &signatures[0];
                                    logger.log(format!("Sell transaction sent: {}", signature));
                                    
                                    match verify_transaction(&signature.to_string(), app_state.clone(), &logger).await {
                                        Ok(verified) => {
                                            if verified {
                                                logger.log("Sell transaction verified successfully".to_string());
                                                Ok(())
                                            } else {
                                                Err("Sell transaction verification failed".to_string())
                                            }
                                        },
                                        Err(e) => {
                                            Err(format!("Transaction verification error: {}", e))
                                        },
                                    }
                                },
                                Err(e) => {
                                    Err(format!("Transaction error: {}", e))
                                },
                            }
                        },
                        Err(e) => {
                            Err(format!("Failed to build PumpFun sell instruction: {}", e))
                        },
                    }
                },
            };
            
            // If any chunk fails, return the error
            if let Err(e) = result {
                logger.log(format!("Failed to sell chunk {}/{}: {}", i + 1, chunks_count, e));
                

                
                return Err(e);
            }
            
            // Wait for the specified interval before next chunk
            if i < chunks_count - 1 {
                logger.log(format!("Waiting {}ms before next chunk", interval));
                tokio::time::sleep(Duration::from_millis(interval)).await;
            }
        }
        
        // Log execution time for progressive sell
        let elapsed = start_time.elapsed();
        logger.log(format!("Progressive sell execution time: {:?}", elapsed));

        // Increment sold counter and update tracking
        let sold_count = {
            let mut entry = SOLD_TOKENS.entry(()).or_insert(0);
            *entry += 1;
            *entry
        };
        logger.log(format!("Total sold: {}", sold_count));
        
        let _bought_count = BOUGHT_TOKENS.get(&()).map(|r| *r).unwrap_or(0);
        let _active_tokens: Vec<String> = TOKEN_TRACKING.iter().map(|entry| entry.key().clone()).collect();
        
        // Note: Keeping token account in WALLET_TOKEN_ACCOUNTS for potential future use
        // The ATA may be empty but could be used again later
        
        // Cancel monitoring task for this token since it's been sold
        if let Err(e) = cancel_token_monitoring(&token_mint, &logger).await {
            logger.log(format!("Failed to cancel monitoring for token {}: {}", token_mint, e).yellow().to_string());
        }
        

        

        
        Ok(())
    } else {
        // Standard single-transaction sell
        logger.log("Executing standard sell".to_string());
        
        // Configure to sell 100% of tokens
        sell_config.in_type = SwapInType::Pct; // Use percentage for sell all operations
        sell_config.amount_in = 1.0; // Sell 100% of tokens
        
        // Execute based on protocol
        let result = match protocol {
            SwapProtocol::PumpFun => {
                logger.log("Using PumpFun protocol for sell".to_string());
                
                // Create the PumpFun instance
                let pump = crate::dex::pump_fun::Pump::new(
                    app_state.rpc_nonblocking_client.clone(),
                    app_state.rpc_client.clone(),
                    app_state.wallet.clone(),
                );
                
                // Create a minimal trade info struct for the sell
                let trade_info_clone = transaction_parser::TradeInfoFromToken {
                    dex_type: transaction_parser::DexType::PumpFun,
                    slot: 0,
                    signature: "standard_sell".to_string(),
                    pool_id: String::new(),
                    mint: token_mint.clone(),
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    is_buy: false,
                    price: 0,
                    is_reverse_when_pump_swap: false,
                    coin_creator: None,
                    sol_change: 0.0,
                    token_change: token_amount,
                    liquidity: 0.0,
                    virtual_sol_reserves: 0,
                    virtual_token_reserves: 0,
                };
                
                // Build swap instructions for sell
                                // Use the new retry mechanism with Jupiter fallback
                logger.log("🔄 Using retry mechanism with Jupiter fallback".cyan().to_string());
                match crate::processor::transaction_retry::execute_sell_with_retry_and_fallback(
                    &trade_info_clone,
                    sell_config,
                    app_state.clone(),
                    &logger,
                ).await {
                    Ok(result) => {
                        if result.success {
                            if result.used_jupiter_fallback {
                                logger.log(format!("✅ PumpFun sell succeeded using Jupiter fallback on attempt {}", result.attempt_count).green().to_string());
                            } else {
                                logger.log(format!("✅ PumpFun sell succeeded on attempt {}", result.attempt_count).green().to_string());
                            }
                            if let Some(signature) = result.signature {
                                logger.log(format!("Final transaction signature: {}", signature));
                            }
                            Ok(())
                        } else {
                            Err(result.error.unwrap_or("Unknown selling error".to_string()))
                        }
                    },
                    Err(e) => {
                        Err(format!("Retry mechanism failed: {}", e))
                    }
                }
            },
            SwapProtocol::PumpSwap => {
                logger.log("Using PumpSwap protocol for sell".to_string());
                
                // Create the PumpSwap instance
                let pump_swap = crate::dex::pump_swap::PumpSwap::new(
                    app_state.wallet.clone(),
                    Some(app_state.rpc_client.clone()),
                    Some(app_state.rpc_nonblocking_client.clone()),
                );
                
                // Create a minimal trade info struct for the sell
                let trade_info_clone = transaction_parser::TradeInfoFromToken {
                    dex_type: transaction_parser::DexType::PumpSwap,
                    slot: 0,
                    signature: "standard_sell".to_string(),
                    pool_id: String::new(),
                    mint: token_mint.clone(),
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    is_buy: false,
                    price: 0,
                    is_reverse_when_pump_swap: false,
                    coin_creator: None,
                    sol_change: 0.0,
                                    token_change: token_amount,
                liquidity: 0.0,
                virtual_sol_reserves: 0,
                virtual_token_reserves: 0,
            };
                
                // Use the new retry mechanism with Jupiter fallback
                logger.log("🔄 Using retry mechanism with Jupiter fallback".cyan().to_string());
                match crate::processor::transaction_retry::execute_sell_with_retry_and_fallback(
                    &trade_info_clone,
                    sell_config,
                    app_state.clone(),
                    &logger,
                ).await {
                    Ok(result) => {
                        if result.success {
                            if result.used_jupiter_fallback {
                                logger.log(format!("✅ PumpSwap sell succeeded using Jupiter fallback on attempt {}", result.attempt_count).green().to_string());
                            } else {
                                logger.log(format!("✅ PumpSwap sell succeeded on attempt {}", result.attempt_count).green().to_string());
                            }
                            if let Some(signature) = result.signature {
                                logger.log(format!("Final transaction signature: {}", signature));
                            }
                            Ok(())
                        } else {
                            Err(result.error.unwrap_or("Unknown selling error".to_string()))
                        }
                    },
                    Err(e) => {
                        Err(format!("Retry mechanism failed: {}", e))
                    }
                }
            },
            SwapProtocol::RaydiumLaunchpad => {
                logger.log("Using Raydium protocol for sell".to_string());
                
                let raydium = crate::dex::raydium_launchpad::Raydium::new(
                    app_state.wallet.clone(),
                    Some(app_state.rpc_client.clone()),
                    Some(app_state.rpc_nonblocking_client.clone()),
                );
                
                let trade_info_clone = transaction_parser::TradeInfoFromToken {
                    dex_type: transaction_parser::DexType::RaydiumLaunchpad,
                    slot: 0,
                    signature: "standard_sell".to_string(),
                    pool_id: trade_info.pool_id.clone(),
                    mint: token_mint.clone(),
                    timestamp: trade_info.timestamp,
                    is_buy: false,
                    price: trade_info.price,
                    is_reverse_when_pump_swap: trade_info.is_reverse_when_pump_swap,
                    coin_creator: trade_info.coin_creator.clone(),
                    sol_change: trade_info.sol_change,
                    token_change: token_amount,
                    liquidity: trade_info.liquidity,
                    virtual_sol_reserves: trade_info.virtual_sol_reserves,
                    virtual_token_reserves: trade_info.virtual_token_reserves,
                };
                
                // Use the new retry mechanism with Jupiter fallback
                logger.log("🔄 Using retry mechanism with Jupiter fallback".cyan().to_string());
                match crate::processor::transaction_retry::execute_sell_with_retry_and_fallback(
                    &trade_info_clone,
                    sell_config,
                    app_state.clone(),
                    &logger,
                ).await {
                    Ok(result) => {
                        if result.success {
                            if result.used_jupiter_fallback {
                                logger.log(format!("✅ Raydium sell succeeded using Jupiter fallback on attempt {}", result.attempt_count).green().to_string());
                            } else {
                                logger.log(format!("✅ Raydium sell succeeded on attempt {}", result.attempt_count).green().to_string());
                            }
                            if let Some(signature) = result.signature {
                                logger.log(format!("Final transaction signature: {}", signature));
                            }
                            Ok(())
                        } else {
                            Err(result.error.unwrap_or("Unknown selling error".to_string()))
                        }
                    },
                    Err(e) => {
                        Err(format!("Retry mechanism failed: {}", e))
                    }
                }
            },
            SwapProtocol::Auto | SwapProtocol::Unknown => {
                logger.log("Auto/Unknown protocol detected, defaulting to PumpFun for sell".yellow().to_string());
                
                let pump = crate::dex::pump_fun::Pump::new(
                    app_state.rpc_nonblocking_client.clone(),
                    app_state.rpc_client.clone(),
                    app_state.wallet.clone(),
                );
                
                let trade_info_clone = transaction_parser::TradeInfoFromToken {
                    dex_type: transaction_parser::DexType::PumpFun,
                    slot: 0,
                    signature: "standard_sell".to_string(),
                    pool_id: String::new(),
                    mint: token_mint.clone(),
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    is_buy: false,
                    price: 0,
                    is_reverse_when_pump_swap: false,
                    coin_creator: None,
                    sol_change: 0.0,
                    token_change: token_amount,
                    liquidity: 0.0,
                    virtual_sol_reserves: 0,
                    virtual_token_reserves: 0,
                };
                
                // Use the new retry mechanism with Jupiter fallback
                logger.log("🔄 Using retry mechanism with Jupiter fallback".cyan().to_string());
                match crate::processor::transaction_retry::execute_sell_with_retry_and_fallback(
                    &trade_info_clone,
                    sell_config,
                    app_state.clone(),
                    &logger,
                ).await {
                    Ok(result) => {
                        if result.success {
                            if result.used_jupiter_fallback {
                                logger.log(format!("✅ Auto/Unknown sell succeeded using Jupiter fallback on attempt {}", result.attempt_count).green().to_string());
                            } else {
                                logger.log(format!("✅ Auto/Unknown sell succeeded on attempt {}", result.attempt_count).green().to_string());
                            }
                            if let Some(signature) = result.signature {
                                logger.log(format!("Final transaction signature: {}", signature));
                            }
                            Ok(())
                        } else {
                            Err(result.error.unwrap_or("Unknown selling error".to_string()))
                        }
                    },
                    Err(e) => {
                        Err(format!("Retry mechanism failed: {}", e))
                    }
                }
            },
        };
        
        // Log execution time for standard sell
        let elapsed = start_time.elapsed();
        logger.log(format!("Standard sell execution time: {:?}", elapsed));
        
        // Increment sold counter on success
        if result.is_ok() {
            let sold_count = {
                let mut entry = SOLD_TOKENS.entry(()).or_insert(0);
                *entry += 1;
                *entry
            };
            logger.log(format!("Total sold: {}", sold_count));
            
            let bought_count = BOUGHT_TOKENS.get(&()).map(|r| *r).unwrap_or(0);
            let _active_tokens: Vec<String> = TOKEN_TRACKING.iter().map(|entry| entry.key().clone()).collect();
            
            // Note: Keeping token account in WALLET_TOKEN_ACCOUNTS for potential future use
            // The ATA may be empty but could be used again later
            
            // Use comprehensive verification and cleanup
            match verify_sell_transaction_and_cleanup(
                &token_mint,
                None, // No specific transaction signature available here
                app_state.clone(),
                &logger,
            ).await {
                Ok(cleaned_up) => {
                    if cleaned_up {
                        logger.log(format!("✅ Comprehensive cleanup completed for standard sell: {}", token_mint));
                    }
                },
                Err(e) => {
                    logger.log(format!("❌ Error during standard sell cleanup verification: {}", e).red().to_string());
                    // Fallback to cancel monitoring
                    if let Err(e) = cancel_token_monitoring(&token_mint, &logger).await {
                        logger.log(format!("Failed to cancel monitoring for token {}: {}", token_mint, e).yellow().to_string());
                    }
                }
            }
            

        }
        
        result
    }
}

/// Process incoming stream messages
async fn process_message_for_target_monitoring(
    msg: &SubscribeUpdate,
    _subscribe_tx: &Arc<tokio::sync::Mutex<impl Sink<SubscribeRequest, Error = impl std::fmt::Debug> + Unpin>>,
    config: Arc<SniperConfig>,
    logger: &Logger,
) -> Result<(), String> {
    // Handle ping messages
    if let Some(UpdateOneof::Ping(_ping)) = &msg.update_oneof {
        return Ok(());
    }
    
    // Handle transaction messages
    if let Some(UpdateOneof::Transaction(txn)) = &msg.update_oneof {
        let target_signature = if let Some(transaction) = &txn.transaction {
            match Signature::try_from(transaction.signature.clone()) {
                Ok(signature) => Some(signature),
                Err(e) => {
                    logger.log(format!("Invalid signature: {:?}", e).red().to_string());
                    return Err(format!("Invalid signature: {:?}", e));
                }
            }
        } else {
            None
        };
        
        let inner_instructions = match &txn.transaction {
            Some(txn_info) => match &txn_info.meta {
                Some(meta) => meta.inner_instructions.clone(),
                None => vec![],
            },
            None => vec![],
        };

        if !inner_instructions.is_empty() {
            let cpi_log_data = inner_instructions
                .iter()
                .flat_map(|inner| &inner.instructions)
                .find(|ix| ix.data.len() == 368 || ix.data.len() == 266 || ix.data.len() == 270  || ix.data.len() == 146 || ix.data.len() == 170 || ix.data.len() == 138 )
                .map(|ix| ix.data.clone());

            if let Some(data) = cpi_log_data {
                let config = config.clone();
                let logger = logger.clone();
                let txn = txn.clone();
                tokio::spawn(async move {
                    if let Some(parsed_data) = crate::processor::transaction_parser::parse_transaction_data(&txn, &data) {
                        if parsed_data.mint != "So11111111111111111111111111111111111111112" {
                            // SNIPER BOT: Handle target wallet transactions differently
                            let _ = handle_sniper_bot_logic(parsed_data, config, target_signature, &txn, &logger).await;
                        }
                    }
                });
            }
        }
    }
    
    Ok(())  
}


/// Process incoming stream messages
async fn process_message_for_dex_monitoring(
    msg: &SubscribeUpdate,
    _subscribe_tx: &Arc<tokio::sync::Mutex<impl Sink<SubscribeRequest, Error = impl std::fmt::Debug> + Unpin>>,
    config: Arc<SniperConfig>,
    logger: &Logger,
) -> Result<(), String> {
    // Handle ping messages
    if let Some(UpdateOneof::Ping(_ping)) = &msg.update_oneof {
        return Ok(());
    }
    
    // Handle transaction messages
    if let Some(UpdateOneof::Transaction(txn)) = &msg.update_oneof {
        let inner_instructions = match &txn.transaction {
            Some(txn_info) => match &txn_info.meta {
                Some(meta) => meta.inner_instructions.clone(),
                None => vec![],
            },
            None => vec![],
        };

        if !inner_instructions.is_empty() {
            let cpi_log_data = inner_instructions
                .iter()
                .flat_map(|inner| &inner.instructions)
                .find(|ix| ix.data.len() == 368 || ix.data.len() == 266 || ix.data.len() == 270  || ix.data.len() == 146 || ix.data.len() == 170 || ix.data.len() == 138 )
                .map(|ix| ix.data.clone());

            if let Some(data) = cpi_log_data {
                let config = config.clone();
                let logger = logger.clone();
                let txn = txn.clone();
                tokio::spawn(async move {
                    if let Some(parsed_data) = crate::processor::transaction_parser::parse_transaction_data(&txn, &data) {
                        if parsed_data.mint != "So11111111111111111111111111111111111111112" {
                            // Check if this token mint is in our focus token list
                            if FOCUS_TOKEN_LIST.contains_key(&parsed_data.mint) {
                                // Process the transaction for this focus token (no extra logs here)
                                let _ = handle_sniper_bot_logic(parsed_data, config, None, &txn, &logger).await;
                            }
                            // No logging for tokens not in focus list
                        }
                    }
                });
            }
        }
    }
    
    Ok(())  
}


/// SNIPER BOT: Main logic for handling both target wallet and DEX monitoring transactions
async fn handle_sniper_bot_logic(
    parsed_data: transaction_parser::TradeInfoFromToken,
    config: Arc<SniperConfig>,
    target_signature: Option<Signature>,
    txn: &SubscribeUpdateTransaction,
    logger: &Logger,
) -> Result<(), String> {
    let instruction_type = parsed_data.dex_type.clone();
    
    // Extract signer from transaction to identify target wallet
    if let Some(ref target_signature) = target_signature {
        // Extract the actual signer from the transaction
        if let Some(signer) = extract_signer_from_transaction(txn) {
            // Check if this transaction is from one of our target wallets
            if config.target_addresses.iter().any(|target| target == &signer) {
                logger.log(format!(
                    "🎯 Target wallet {} {} token {} for {} SOL",
                    signer,
                    if parsed_data.is_buy { "buying" } else { "selling" },
                    parsed_data.mint,
                    parsed_data.sol_change.abs()
                ).purple().bold().to_string());
                
                // Handle buy transactions from target wallets
                if parsed_data.is_buy {
                    return handle_target_wallet_buy(parsed_data, config, logger, signer).await;
                } else {
                    // Handle sell transactions from target wallets
                    return handle_target_wallet_sell(parsed_data, config, logger).await;
                }
            }
        }
    }
    
    // Handle other transactions (non-target wallets) - check if token is in focus list for volume-based buying
    handle_volume_based_buying(parsed_data, config, logger).await
}

/// SNIPER BOT: Handle buy transactions from target wallets
async fn handle_target_wallet_buy(
    parsed_data: transaction_parser::TradeInfoFromToken,
    config: Arc<SniperConfig>,
    logger: &Logger,
    whale_wallet: String,
) -> Result<(), String> {
    let mint = parsed_data.mint.clone();
    
    // Determine protocol based on instruction type
    let protocol = match parsed_data.dex_type {
        transaction_parser::DexType::PumpSwap => SwapProtocol::PumpSwap,
        transaction_parser::DexType::PumpFun => SwapProtocol::PumpFun,
        transaction_parser::DexType::RaydiumLaunchpad => SwapProtocol::RaydiumLaunchpad,
        _ => config.protocol_preference.clone(),
    };
    
    // Check if token already exists in focus list
    if let Some(mut focus_info) = FOCUS_TOKEN_LIST.get_mut(&mint) {
        // Add whale wallet to existing token
        focus_info.whale_wallets.insert(whale_wallet.clone());
    } else {
        // Create new focus token entry
        let mut whale_wallets = HashSet::new();
        whale_wallets.insert(whale_wallet.clone());
        
        let focus_info = FocusTokenInfo {
            mint: mint.clone(),
            initial_price: parsed_data.price as f64,
            current_price: parsed_data.price as f64,
            lowest_price: parsed_data.price as f64,
            highest_price: parsed_data.price as f64,
            price_dropped: false,
            buy_count: 0,
            sell_count: 0,
            trade_cycles: 0,
            protocol,
            added_timestamp: Instant::now(),
            last_price_update: Instant::now(),
            price_history: VecDeque::with_capacity(100),
            slot_price_history: VecDeque::with_capacity(16),
            two_slot_drop_active: false,
            whale_wallets,
            total_trades: 0,
        };
        
        FOCUS_TOKEN_LIST.insert(mint.clone(), focus_info);
        
        // Start price monitoring for this token
        start_price_monitoring(mint.clone(), config.clone(), logger).await?;
    }
    
    Ok(())
}

/// SNIPER BOT: Check and increment trade count, remove token if limit reached
fn check_and_increment_trade_count(mint: &str, logger: &Logger) -> bool {
    if let Some(mut focus_info) = FOCUS_TOKEN_LIST.get_mut(mint) {
        focus_info.total_trades += 1;
        
        if focus_info.total_trades >= 10 {
            drop(focus_info);
            FOCUS_TOKEN_LIST.remove(mint);
            
            // Cancel price monitoring
            if let Some((_removed_key, cancel_token)) = PRICE_MONITORING_TASKS.remove(mint) {
                cancel_token.cancel();
            }
            
            return false; // Token removed
        }
        true // Token still active
    } else {
        false // Token not found
    }
}

/// SNIPER BOT: Handle sell transactions from target wallets
async fn handle_target_wallet_sell(
    parsed_data: transaction_parser::TradeInfoFromToken,
    config: Arc<SniperConfig>,
    logger: &Logger,
) -> Result<(), String> {
    let mint = parsed_data.mint.clone();
    
    // Check if we own this token and execute emergency sell
    if let Some(_token_info) = BOUGHT_TOKEN_LIST.get(&mint) {
        // Execute emergency sell (reuse existing logic)
        let app_state_clone = config.app_state.clone();
        let logger_clone = logger.clone();
        let mint_clone = mint.clone();
        
        tokio::spawn(async move {
            let config = crate::common::config::Config::get().await;
            let selling_config = crate::processor::selling_strategy::SellingConfig::set_from_env();
            let selling_engine = crate::processor::selling_strategy::SellingEngine::new(
                app_state_clone.clone().into(),
                Arc::new(config.swap_config.clone()),
                selling_config,
            );
            drop(config);
            
            match selling_engine.unified_emergency_sell(&mint_clone, false, None, None).await {
                Ok(signature) => {
                    // Update focus token sell count and check trade limit
                    if let Some(mut focus_info) = FOCUS_TOKEN_LIST.get_mut(&mint_clone) {
                        focus_info.sell_count += 1;
                        focus_info.trade_cycles += 1;
                        
                        // Check trade count limit
                        if !check_and_increment_trade_count(&mint_clone, &logger_clone) {
                            logger_clone.log(format!("🚫 Token {} removed due to trade limit after sell", mint_clone).red().to_string());
                        } else if focus_info.trade_cycles >= 3 {
                            drop(focus_info);
                            FOCUS_TOKEN_LIST.remove(&mint_clone);
                            
                            // Cancel price monitoring
                            if let Some((_removed_key, cancel_token)) = PRICE_MONITORING_TASKS.remove(&mint_clone) {
                                cancel_token.cancel();
                            }
                        }
                    }
                    
                    // Remove from bought tokens list
                    BOUGHT_TOKEN_LIST.remove(&mint_clone);
                    TOKEN_TRACKING.remove(&mint_clone);
                    
                    // Cancel regular monitoring
                    if let Err(e) = cancel_token_monitoring(&mint_clone, &logger_clone).await {
                        logger_clone.log(format!("Failed to cancel monitoring for token {}: {}", mint_clone, e).yellow().to_string());
                    }
                },
                Err(e) => {
                    logger_clone.log(format!("❌ Emergency sell failed: {}", e).red().to_string());
                }
            }
        });
    }
    
    Ok(())
}

/// SNIPER BOT: Handle volume-based buying decisions
async fn handle_volume_based_buying(
    parsed_data: transaction_parser::TradeInfoFromToken,
    config: Arc<SniperConfig>,
    logger: &Logger,
) -> Result<(), String> {
    let mint = parsed_data.mint.clone();
    // Read sniper trigger config once (avoid awaiting while holding map guard)
    let cfg_guard = crate::common::config::Config::get().await;
    let trigger_size = cfg_guard.focus_trigger_sol.max(1.0);
    
    // Check if token is in focus list
    if let Some(mut focus_info) = FOCUS_TOKEN_LIST.get_mut(&mint) {
        // Update price information
        let current_price = parsed_data.price as f64;
        focus_info.current_price = current_price;
        focus_info.last_price_update = Instant::now();
        // Maintain (slot, price) history for slot-aware drop detection
        focus_info.slot_price_history.push_back((parsed_data.slot, current_price));
        if focus_info.slot_price_history.len() > 16 { focus_info.slot_price_history.pop_front(); }
        
        // Update lowest price
        if current_price < focus_info.lowest_price {
            focus_info.lowest_price = current_price;
        }
        
        // Update highest price
        if current_price > focus_info.highest_price {
            focus_info.highest_price = current_price;
        }
        
        // Add to price history
        focus_info.price_history.push_back(current_price);
        if focus_info.price_history.len() > 100 {
            focus_info.price_history.pop_front();
        }
        
        // Compute two-slot sudden drop: price decreased over each of the last two slots compared to prior slot price
        let mut two_slot_drop = false;
        if focus_info.slot_price_history.len() >= 3 {
            let n = focus_info.slot_price_history.len();
            let (s2, p2) = focus_info.slot_price_history[n-1];
            let (s1, p1) = focus_info.slot_price_history[n-2];
            let (s0, p0) = focus_info.slot_price_history[n-3];
            if s0 < s1 && s1 < s2 && p0 > p1 && p1 > p2 {
                two_slot_drop = true;
            }
        }
        focus_info.two_slot_drop_active = two_slot_drop;

        // Trigger only AFTER a 2-slot drop and on a buy >= configured SOL
        if parsed_data.is_buy && parsed_data.sol_change.abs() >= trigger_size && focus_info.two_slot_drop_active {
            let price_drop_percentage = if focus_info.initial_price > 0.0 {
                ((focus_info.initial_price - focus_info.lowest_price) / focus_info.initial_price) * 100.0
            } else { 0.0 };

            logger.log(format!(
                "🎯 SNIPER TRIGGER: {} dropped {:.2}% from initial and a {} SOL buy detected",
                mint, price_drop_percentage, parsed_data.sol_change.abs()
            ).green().bold().to_string());

            if focus_info.trade_cycles < 3 {
                return execute_sniper_buy(parsed_data, config, focus_info.protocol.clone(), logger).await;
            }
        }
    }
    
    Ok(())
}

/// SNIPER BOT: Start price monitoring for a focus token
async fn start_price_monitoring(
    mint: String,
    config: Arc<SniperConfig>,
    logger: &Logger,
) -> Result<(), String> {
    // Create cancellation token for this monitoring task
    let cancel_token = CancellationToken::new();
    PRICE_MONITORING_TASKS.insert(mint.clone(), cancel_token.clone());
    
    // Spawn monitoring task
    let mint_clone = mint.clone();
    let config_clone = config.clone();
    let logger_clone = logger.clone();
    
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5)); // Check every 5 seconds
        
        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    break;
                }
                _ = interval.tick() => {
                    // Update price information from current market data
                    // TODO: In real implementation, fetch actual price from Jupiter API or DEX
                    // Read config before locking map
                    let cfg = crate::common::config::Config::get().await;
                    let price_drop_threshold = cfg.focus_drop_threshold_pct; // fraction e.g. 0.15
                    drop(cfg);
                    if let Some(mut focus_info) = FOCUS_TOKEN_LIST.get_mut(&mint_clone) {
                        // Update last seen time
                        focus_info.last_price_update = Instant::now();
                        
                        // Simulate price monitoring (replace with real price fetching)
                        // For now, we'll detect price drops based on transaction analysis
                        
                        // Check if price has dropped significantly from initial
                        let current_vs_initial = (focus_info.initial_price - focus_info.current_price) / focus_info.initial_price;
                        
                        if current_vs_initial >= price_drop_threshold && !focus_info.price_dropped {
                            focus_info.price_dropped = true;
                        }
                        
                        // Update price history (time-based) and slot-price history if we can infer slot from trades elsewhere
                        let current_price_snapshot = focus_info.current_price;
                        focus_info.price_history.push_back(current_price_snapshot);
                        if focus_info.price_history.len() > 100 {
                            focus_info.price_history.pop_front();
                        }
                        // We cannot access slot here; keep only price history in this timer.
                        
                        // Update lowest price
                        if focus_info.current_price < focus_info.lowest_price {
                            focus_info.lowest_price = focus_info.current_price;
                        }
                        
                        // Check if token should be removed due to inactivity (e.g., 1 hour)
                        if focus_info.added_timestamp.elapsed().as_secs() > 3600 {
                            drop(focus_info);
                            FOCUS_TOKEN_LIST.remove(&mint_clone);
                            break;
                        }
                    } else {
                        // Token removed from focus list, stop monitoring
                        break;
                    }
                }
            }
        }
        
        // Clean up
        PRICE_MONITORING_TASKS.remove(&mint_clone);
    });
    
    Ok(())
}

/// SNIPER BOT: Execute buy when sniper conditions are met
async fn execute_sniper_buy(
    parsed_data: transaction_parser::TradeInfoFromToken,
    config: Arc<SniperConfig>,
    protocol: SwapProtocol,
    logger: &Logger,
) -> Result<(), String> {
    let mint = parsed_data.mint.clone();
    
    // Check if we already own this token
    if BOUGHT_TOKEN_LIST.contains_key(&mint) {
        return Ok(());
    }
    
    // Check counter limit
    let active_tokens_count = TOKEN_TRACKING.len();
    if active_tokens_count >= config.counter_limit as usize {
        return Ok(());
    }
    
    // Check trade count limit before buying
    if !check_and_increment_trade_count(&mint, logger) {
        return Ok(());
    }
    
    // Execute buy using existing logic
    match execute_buy(
        parsed_data.clone(),
        config.app_state.clone().into(),
        Arc::new(config.swap_config.clone()),
        protocol.clone(),
    ).await {
        Ok(_) => {
            // Update focus token buy count
            if let Some(mut focus_info) = FOCUS_TOKEN_LIST.get_mut(&mint) {
                focus_info.buy_count += 1;
            }
            
            // Setup selling strategy
            match setup_selling_strategy(
                mint.clone(),
                config.app_state.clone().into(),
                Arc::new(config.swap_config.clone()),
                protocol,
            ).await {
                Ok(_) => {},
                Err(e) => {
                    logger.log(format!("❌ Failed to setup selling strategy for token {}: {}", mint, e).red().to_string());
                }
            }
            
            Ok(())
        },
        Err(e) => {
            logger.log(format!("❌ Sniper buy failed for token {}: {}", mint, e).red().to_string());
            Err(e)
        }
    }
}

async fn handle_parsed_data_for_selling(
    parsed_data: transaction_parser::TradeInfoFromToken,
    config: Arc<SniperConfig>,
    txn: &SubscribeUpdateTransaction,
    target_signature: Option<Signature>,
    logger: &Logger,
) -> Result<(), String> {
    let start_time = Instant::now();
    let instruction_type = parsed_data.dex_type.clone();
    let mint = parsed_data.mint.clone();
    
    // TARGET WALLET SELL DETECTION - Check if this sell is from one of our target wallets
    if let Some(ref target_signature) = target_signature {
        // Extract signer from the target signature - this represents the target wallet that made the transaction
        if let Some(signer) = extract_signer_from_transaction(&txn) {
            // Check if the signer is in our target wallet list
            if config.target_addresses.iter().any(|target| target == &signer) {
                logger.log(format!(
                    "🎯 TARGET WALLET SELL DETECTED: Wallet {} is selling token {} for {} SOL",
                    signer, parsed_data.mint, parsed_data.sol_change.abs()
                ).purple().bold().to_string());
                
                // Check if we own this token
                if let Some(mut token_info) = BOUGHT_TOKEN_LIST.get_mut(&parsed_data.mint) {
                    logger.log(format!(
                        "🚨 We own token {} that target wallet is selling - executing IMMEDIATE COPY SELL",
                        parsed_data.mint
                    ).red().bold().to_string());
                    
                    // Execute immediate copy sell using non-blocking task
                    let mint_clone = parsed_data.mint.clone();
                    let app_state_clone = config.app_state.clone();
                    let logger_clone = logger.clone();
                    
                    tokio::spawn(async move {
                        // Use unified emergency sell directly for better consistency
                        let config = crate::common::config::Config::get().await;
                        let selling_config = crate::processor::selling_strategy::SellingConfig::set_from_env();
                        let selling_engine = crate::processor::selling_strategy::SellingEngine::new(
                            app_state_clone.clone().into(),
                            Arc::new(config.swap_config.clone()),
                            selling_config,
                        );
                        drop(config);
                        
                        // Use Copy Target Selling mode (higher priority than regular emergency sell)
                        let result = selling_engine.unified_emergency_sell(&mint_clone, false, None, None).await;
                        
                        match result {
                            Ok(signature) => {
                                logger_clone.log(format!("✅ Successfully executed target wallet copy sell for token: {} with signature: {}", mint_clone, signature).green().to_string());
                                
                                // Cancel monitoring for this token since it's been sold
                                if let Err(e) = cancel_token_monitoring(&mint_clone, &logger_clone).await {
                                    logger_clone.log(format!("Failed to cancel monitoring for token {}: {}", mint_clone, e).yellow().to_string());
                                }
                            },
                            Err(e) => {
                                logger_clone.log(format!("❌ Failed to execute target wallet copy sell for token {}: {}", mint_clone, e).red().to_string());
                            }
                        }
                    });
                    
                    logger.log(format!("🚀 Target wallet copy sell task spawned for token: {}, continuing main flow", parsed_data.mint).cyan().to_string());
                } else {
                    logger.log(format!("🎯 Target wallet selling {} but we don't own this token", parsed_data.mint).blue().to_string());
                }
            }
        }
    }
    
    // WHALE DETECTION LOGIC - Check for large sell transactions
    if !parsed_data.is_buy {
        // This is a sell transaction - check if it's a whale selling
        // For sell transactions, sol_change represents SOL received from selling (positive value)
        // For buy transactions, sol_change represents SOL spent (negative value)
        let sol_amount = parsed_data.sol_change.abs();

        // Check if this is a whale selling (>= 10 SOL)
        if sol_amount >= crate::common::constants::WHALE_SELLING_AMOUNT_FOR_SELLING_TRIGGER {
            logger.log(format!(
                "🐋 WHALE SELLING DETECTED: {} SOL for token {} - triggering EMERGENCY SELL with zeroslot",
                sol_amount, parsed_data.mint
            ).red().bold().to_string());
            
            // Check if we own this token
            if let Some(mut token_info) = BOUGHT_TOKEN_LIST.get_mut(&parsed_data.mint) {
                logger.log(format!(
                    "🚨 We own token {} that whale is selling - executing EMERGENCY SELL via zeroslot",
                    parsed_data.mint
                ).red().bold().to_string());
                
                // Execute emergency sell using zeroslot for maximum speed - NON-BLOCKING
                let mint_clone = parsed_data.mint.clone();
                let app_state_clone = config.app_state.clone();
                let logger_clone = logger.clone();
                
                // Spawn emergency sell task without blocking main flow
                tokio::spawn(async move {
                    // Use unified emergency sell directly for better consistency
                    let config = crate::common::config::Config::get().await;
                    let selling_config = crate::processor::selling_strategy::SellingConfig::set_from_env();
                    let selling_engine = crate::processor::selling_strategy::SellingEngine::new(
                        app_state_clone.clone().into(),
                        Arc::new(config.swap_config.clone()),
                        selling_config,
                    );
                    drop(config);
                    
                    // Use Whale Emergency Sell mode for faster execution
                    let result = selling_engine.unified_emergency_sell(&mint_clone, true, None, None).await;
                    
                    match result {
                        Ok(signature) => {
                            logger_clone.log(format!("✅ Successfully executed fast sell for token: {} with signature: {}", mint_clone, signature).green().to_string());
                            
                            // Cancel monitoring for this token since it's been sold
                            if let Err(e) = cancel_token_monitoring(&mint_clone, &logger_clone).await {
                                logger_clone.log(format!("Failed to cancel monitoring for token {}: {}", mint_clone, e).yellow().to_string());
                            }
                        },
                        Err(e) => {
                            logger_clone.log(format!("❌ Failed to execute whale emergency sell for token {}: {}", mint_clone, e).red().to_string());
                        }
                    }
                });
                
                // Continue processing without waiting for emergency sell to complete
                logger.log(format!("🚀 Whale emergency sell task spawned for token: {}, continuing main flow", parsed_data.mint).cyan().to_string());
            } else {
                logger.log(format!("🐋 Whale selling detected for {} but we don't own this token", parsed_data.mint).yellow().to_string());
            }
        }
    }
    
    // Log the parsed transaction data
    logger.log(format!(
        "Token transaction detected for {}: Instruction: {}, Is buy: {}",
        mint,
        match instruction_type {
            transaction_parser::DexType::PumpSwap => "PumpSwap",
            transaction_parser::DexType::PumpFun => "PumpFun",
            transaction_parser::DexType::RaydiumLaunchpad => "RaydiumLaunchpad",
            _ => "Unknown",
        },
        parsed_data.is_buy
    ).green().to_string());
    
    // Create selling engine
    let selling_engine = SellingEngine::new(
        config.app_state.clone().into(),
        Arc::new(config.swap_config.clone()),
        SellingConfig::set_from_env(), // SellingConfig::default(), 
    );
    
    // Update token price from transaction data (no external API calls)
    if let Err(e) = update_token_price_from_trade_info(&mint, &parsed_data).await {
        logger.log(format!("Error updating price from transaction: {}", e).yellow().to_string());
    }
    
    // Update token metrics using the TradeInfoFromToken directly
    if let Err(e) = selling_engine.update_metrics(&mint, &parsed_data).await {
        logger.log(format!("Error updating metrics: {}", e).red().to_string());
    } else {
        logger.log(format!("Updated metrics for token: {}", mint).green().to_string());
    }
    
    // Check if we should sell this token
    match selling_engine.evaluate_sell_conditions(&mint).await {
        Ok((should_sell, use_whale_emergency)) => {
            if should_sell {
                logger.log(format!("Sell conditions met for token: {}", mint).green().to_string());
                
                // Determine protocol to use for selling
                let protocol = match instruction_type {
                    transaction_parser::DexType::PumpSwap => SwapProtocol::PumpSwap,
                    transaction_parser::DexType::PumpFun => SwapProtocol::PumpFun,
                    _ => config.protocol_preference.clone(),
                };
                
                if use_whale_emergency {
                    // Whale emergency sell using zeroslot for maximum speed
                    logger.log(format!("🐋 WHALE EMERGENCY SELL triggered for token: {}", mint).cyan().bold().to_string());
                    
                    // Get the current PNL to determine whale threshold
                    if let Some(metrics) = crate::processor::selling_strategy::TOKEN_METRICS.get(&mint) {
                        let pnl = if metrics.entry_price > 0.0 {
                            (metrics.current_price - metrics.entry_price) / metrics.entry_price * 100.0
                        } else {
                            0.0
                        };
                        
                        if let Some(_whale_threshold) = selling_engine.get_config().dynamic_whale_selling.get_whale_threshold_for_pnl(pnl) {
                            // CRITICAL FIX: Use non-blocking spawned task to prevent bot from getting stuck
                            let mint_clone = mint.clone();
                            let parsed_data_clone = parsed_data.clone();
                            let logger_clone = logger.clone();
                            let selling_engine_clone = selling_engine.clone();
                            let protocol_clone = protocol.clone();
                            
                            tokio::spawn(async move {
                                // Use the existing selling_engine for whale emergency sell
                                match selling_engine_clone.unified_emergency_sell(&mint_clone, true, Some(&parsed_data_clone), None).await {
                                    Ok(signature) => {
                                        logger_clone.log(format!("🐋 Successfully executed whale emergency sell for token: {} with signature: {}", mint_clone, signature).green().bold().to_string());
                                        // Cancel monitoring task for this token since it's been sold
                                        if let Err(e) = cancel_token_monitoring(&mint_clone, &logger_clone).await {
                                            logger_clone.log(format!("Failed to cancel monitoring for token {}: {}", mint_clone, e).yellow().to_string());
                                        }
                                    },
                                    Err(e) => {
                                        logger_clone.log(format!("🐋 Error executing emergency sell: {}", e).red().to_string());
                                        
                                        // Fallback to regular emergency sell
                                        logger_clone.log("Falling back to regular emergency sell".yellow().to_string());
                                        match selling_engine_clone.unified_emergency_sell(&mint_clone, false, Some(&parsed_data_clone), Some(protocol_clone.clone())).await {
                                            Ok(_) => {
                                                logger_clone.log(format!("Successfully executed fallback emergency sell for token: {}", mint_clone).green().to_string());
                                                if let Err(e) = cancel_token_monitoring(&mint_clone, &logger_clone).await {
                                                    logger_clone.log(format!("Failed to cancel monitoring for token {}: {}", mint_clone, e).yellow().to_string());
                                                }
                                            },
                                            Err(e) => {
                                                logger_clone.log(format!("Error executing fallback emergency sell: {}", e).red().to_string());
                                            }
                                        }
                                    }
                                }
                            });
                            
                            // Log that task was spawned and continue processing
                            logger.log(format!("🚀 Whale emergency sell task spawned for token: {}, continuing main flow", mint).cyan().to_string());
                        } else {
                            logger.log(format!("🐋 No whale threshold found for PNL {:.2}%, using regular emergency sell", pnl).yellow().to_string());
                            match selling_engine.unified_emergency_sell(&mint, false, Some(&parsed_data), Some(protocol.clone())).await {
                                Ok(_) => {
                                    logger.log(format!("Successfully executed emergency sell for token: {}", mint).green().to_string());
                                    if let Err(e) = cancel_token_monitoring(&mint, &logger).await {
                                        logger.log(format!("Failed to cancel monitoring for token {}: {}", mint, e).yellow().to_string());
                                    }
                                },
                                Err(e) => {
                                    logger.log(format!("Error executing emergency sell: {}", e).red().to_string());
                                    return Err(format!("Failed to execute emergency sell: {}", e));
                                }
                            }
                        }
                    } else {
                        logger.log("🐋 No metrics found for whale emergency sell, using regular emergency sell".yellow().to_string());
                        match selling_engine.unified_emergency_sell(&mint, false, Some(&parsed_data), Some(protocol.clone())).await {
                            Ok(_) => {
                                logger.log(format!("Successfully executed emergency sell for token: {}", mint).green().to_string());
                                if let Err(e) = cancel_token_monitoring(&mint, &logger).await {
                                    logger.log(format!("Failed to cancel monitoring for token {}: {}", mint, e).yellow().to_string());
                                }
                            },
                            Err(e) => {
                                logger.log(format!("Error executing emergency sell: {}", e).red().to_string());
                                return Err(format!("Failed to execute emergency sell: {}", e));
                            }
                        }
                    }
                } else {
                    // Regular emergency sell all tokens immediately to prevent further losses
                    logger.log(format!("EMERGENCY SELL ALL triggered for token: {}", mint).red().bold().to_string());
                    
                    match selling_engine.unified_emergency_sell(&mint, false, Some(&parsed_data), Some(protocol.clone())).await {
                        Ok(_) => {
                            logger.log(format!("Successfully executed emergency sell all for token: {}", mint).green().to_string());
                            // Cancel monitoring task for this token since it's been sold
                            if let Err(e) = cancel_token_monitoring(&mint, &logger).await {
                                logger.log(format!("Failed to cancel monitoring for token {}: {}", mint, e).yellow().to_string());
                            }
                        },
                        Err(e) => {
                            logger.log(format!("Error executing emergency sell all: {}", e).red().to_string());
                            return Err(format!("Failed to execute emergency sell: {}", e));
                        }
                    }
                }
                
                logger.log(format!("Successfully processed sell for token: {}", mint).green().to_string());
            } else {
                logger.log(format!("Not selling token yet: {}", mint).blue().to_string());
            }
        },
        Err(e) => {
            logger.log(format!("Error evaluating sell conditions: {}", e).red().to_string());
        }
    }
    
    logger.log(format!("Processing time for sell transaction: {:?}", start_time.elapsed()).blue().to_string());
    Ok(())
}

/// Set up selling strategy for a token
async fn setup_selling_strategy(
    token_mint: String,
    app_state: Arc<AppState>,
    swap_config: Arc<SwapConfig>,
    protocol_preference: SwapProtocol,
) -> Result<(), String> {
    let logger = Logger::new("[SETUP-SELLING-STRATEGY] => ".green().to_string());
    
    // Initialize
    logger.log(format!("Setting up selling strategy for token: {}", token_mint));
    
    // Create cancellation token for this monitoring task
    let cancellation_token = CancellationToken::new();
    
    // Register the cancellation token
    MONITORING_TASKS.insert(token_mint.clone(), (token_mint.clone(), cancellation_token.clone()));
    
    // Clone values that will be moved into the task
    let token_mint_cloned = token_mint.clone();
    let app_state_cloned = app_state.clone();
    let swap_config_cloned = swap_config.clone();
    let protocol_preference_cloned = protocol_preference.clone();
    let logger_cloned = logger.clone();
    
    // Spawn a task to handle the monitoring and selling
    tokio::spawn(async move {
        let _ = monitor_token_for_selling(
            token_mint_cloned, 
            app_state_cloned, 
            swap_config_cloned, 
            protocol_preference_cloned, 
            &logger_cloned,
            cancellation_token
        ).await;
    });
    Ok(())
}

/// Monitor a token specifically for selling opportunities
async fn monitor_token_for_selling(
    token_mint: String,
    app_state: Arc<AppState>,
    swap_config: Arc<SwapConfig>,
    protocol_preference: SwapProtocol,
    logger: &Logger,
    cancellation_token: CancellationToken,
) -> Result<(), String> {
    // Create config for the Yellowstone connection
    // This is a simplified version of what's in the main copy_trading function
    let mut yellowstone_grpc_http = "https://helsinki.rpcpool.com/".to_string(); // Default value
    let mut yellowstone_grpc_token = "your_token_here".to_string(); // Default value
    
    // Try to get config values from environment if available
    if let Ok(url) = std::env::var("YELLOWSTONE_GRPC_HTTP") {
        yellowstone_grpc_http = url;
    }
    
    if let Ok(token) = std::env::var("YELLOWSTONE_GRPC_TOKEN") {
        yellowstone_grpc_token = token;
    }
    
    logger.log("Connecting to Yellowstone gRPC for selling, will close connection after selling ...".green().to_string());
    
    // Connect to Yellowstone gRPC
    let mut client = GeyserGrpcClient::build_from_shared(yellowstone_grpc_http.clone())
        .map_err(|e| format!("Failed to build client: {}", e))?
        .x_token::<String>(Some(yellowstone_grpc_token.clone()))
        .map_err(|e| format!("Failed to set x_token: {}", e))?
        .tls_config(ClientTlsConfig::new().with_native_roots())
        .map_err(|e| format!("Failed to set tls config: {}", e))?
        .connect()
        .await
        .map_err(|e| format!("Failed to connect: {}", e))?;

    // Set up subscribe with retries
    let mut retry_count = 0;
    const MAX_RETRIES: u32 = 3;
    let (subscribe_tx, mut stream) = loop {
        match client.subscribe().await {
            Ok(pair) => break pair,
            Err(e) => {
                retry_count += 1;
                if retry_count >= MAX_RETRIES {
                    return Err(format!("Failed to subscribe after {} attempts: {}", MAX_RETRIES, e));
                }
                logger.log(format!(
                    "[CONNECTION ERROR] => Failed to subscribe (attempt {}/{}): {}. Retrying in 5 seconds...",
                    retry_count, MAX_RETRIES, e
                ).red().to_string());
                time::sleep(Duration::from_secs(5)).await;
            }
        }
    };

    // Convert to Arc to allow cloning across tasks
    let subscribe_tx = Arc::new(tokio::sync::Mutex::new(subscribe_tx));
    // Set up subscription focused on the token mint
    let subscription_request = SubscribeRequest {
        transactions: maplit::hashmap! {
            "TokenMonitor".to_owned() => SubscribeRequestFilterTransactions {
                vote: Some(false), // Exclude vote transactions
                failed: Some(false), // Exclude failed transactions
                signature: None,
                account_include: vec![token_mint.clone()], // Only include transactions involving our token
                account_exclude: vec![], // Listen to all transactions
                account_required: Vec::<String>::new(),
            }
        },
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    };
  
    subscribe_tx
        .lock()
        .await
        .send(subscription_request)
        .await
        .map_err(|e| format!("Failed to send subscribe request: {}", e))?;


    // Create config for tasks
    let copy_trading_config = SniperConfig {
        yellowstone_grpc_http,
        yellowstone_grpc_token,
        app_state: (*app_state).clone(),
        swap_config: (*swap_config).clone(),
        counter_limit: 5,
        target_addresses: vec![token_mint.clone()],
                    excluded_addresses: vec![],
        protocol_preference,
    };

    let config = Arc::new(copy_trading_config);

    // Spawn heartbeat task
    let subscribe_tx_clone = subscribe_tx.clone();
    let cancellation_token_clone = cancellation_token.clone();
    
    let heartbeat_handle = tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(30));
        
        loop {
            tokio::select! {
                _ = cancellation_token_clone.cancelled() => {
                    break;
                }
                _ = interval.tick() => {
                    if let Err(_e) = send_heartbeat_ping(&subscribe_tx_clone).await {
                        break;
                    }
                }
            }
        }
    });

    // Timer removed - selling checks are now transaction-driven only
    
    // Main stream processing loop
    logger.log("Starting main processing loop with transaction-driven selling checks...".green().to_string());
    loop {
        tokio::select! {
            // Check for cancellation
            _ = cancellation_token.cancelled() => {
                logger.log(format!("🔌 Monitoring cancelled for token: {} - closing gRPC stream", token_mint).yellow().to_string());
                break;
            }
            // Process stream messages (transaction-driven selling only)
            msg_result = stream.next() => {
                match msg_result {
                    Some(Ok(msg)) => {
                        if let Err(e) = process_selling(&msg, &subscribe_tx, config.clone(), &logger).await {
                            logger.log(format!("Error processing message: {}", e).red().to_string());
                        }
                        
                        // Check if token has been sold and should close stream
                        if !BOUGHT_TOKEN_LIST.contains_key(&token_mint) {
                            logger.log(format!("🔌 Token {} sold - closing dedicated gRPC stream", token_mint).cyan().to_string());
                            break;
                        }
                    },
                    Some(Err(e)) => {
                        logger.log(format!("Stream error: {:?}", e).red().to_string());
                        // Try to reconnect
                        break;
                    },
                    None => {
                        logger.log("Stream ended".yellow().to_string());
                        break;
                    }
                }
            }
        }
    }
    
    // Cleanup: Cancel heartbeat task and close connections
    heartbeat_handle.abort();
    
    // Explicitly close the stream by dropping it
    drop(stream);
    drop(subscribe_tx);
    drop(client);
    
    logger.log(format!("🔌 gRPC stream and connections properly closed for token: {}", token_mint).green().to_string());
    
    Ok(())
}

/// Process incoming stream messages
async fn process_selling(
    msg: &SubscribeUpdate,
    _subscribe_tx: &Arc<tokio::sync::Mutex<impl Sink<SubscribeRequest, Error = impl std::fmt::Debug> + Unpin>>,
    config: Arc<SniperConfig>,
    logger: &Logger,
) -> Result<(), String> {
    // Handle ping messages
    if let Some(UpdateOneof::Ping(_ping)) = &msg.update_oneof {
        return Ok(());
    }

    // Handle transaction messages
    if let Some(UpdateOneof::Transaction(txn)) = &msg.update_oneof {
        let _start_time = Instant::now();
        
        // Extract target signature
        let target_signature = if let Some(transaction) = &txn.transaction {
            match Signature::try_from(transaction.signature.clone()) {
                Ok(signature) => Some(signature),
                Err(e) => {
                    logger.log(format!("Invalid signature: {:?}", e).red().to_string());
                    None
                }
            }
        } else {
            None
        };
        
        // Extract transaction logs and account keys
        let inner_instructions = match &txn.transaction {
            Some(txn_info) => match &txn_info.meta {
                Some(meta) => meta.inner_instructions.clone(),
                None => vec![],
            },
            None => vec![],
        };

        if !inner_instructions.is_empty() {
            // Find the largest data payload in inner instructions
            let mut largest_data: Option<Vec<u8>> = None;
            let mut largest_size = 0;

            for inner in &inner_instructions {
                for ix in &inner.instructions {
                    if ix.data.len() > largest_size {
                        largest_size = ix.data.len();
                        largest_data = Some(ix.data.clone());
                    }
                }
            }

            let cpi_log_data = inner_instructions
            .iter()
            .flat_map(|inner| &inner.instructions)
            .find(|ix| ix.data.len() == 368 || ix.data.len() == 266 || ix.data.len() == 270  || ix.data.len() == 146 || ix.data.len() == 170 || ix.data.len() == 138)
            .map(|ix| ix.data.clone());

           
            if let Some(data) = cpi_log_data {
                let config = config.clone();
                let logger = logger.clone();
                let txn = txn.clone();  // Clone the transaction data
                let target_signature_clone = target_signature; // Clone the signature
                tokio::spawn(async move {
                    if let Some(parsed_data) = crate::processor::transaction_parser::parse_transaction_data(&txn, &data) {
                        if parsed_data.mint != "So11111111111111111111111111111111111111112" {
                        let _ =  handle_parsed_data_for_selling(parsed_data, config, &txn, target_signature_clone, &logger).await;
                        }
                    }
                });
            }
            
        }
    }
    
    Ok(())
}


async fn handle_parsed_data_for_buying(
    parsed_data: transaction_parser::TradeInfoFromToken,
    config: Arc<SniperConfig>,
    target_signature: Option<Signature>,
    logger: &Logger,
) -> Result<(), String> {
    let start_time = Instant::now();
    let instruction_type = parsed_data.dex_type.clone();
    
    // For sell transactions, check if this is a copy target selling their tokens
    if !parsed_data.is_buy {
        // This is a sell transaction from a target wallet
        // IMMEDIATE COPY SELL - Execute without checking BOUGHT_TOKEN_LIST first for maximum speed
        logger.log(format!(
            "👥 Copy target selling token {} - executing IMMEDIATE COPY SELL",
            parsed_data.mint
        ).purple().bold().to_string());
        
        // Clone values before moving into async block
        let mint_clone = parsed_data.mint.clone();
        let logger_clone = logger.clone();
        let config_clone = config.clone();
        
        // Execute copy sell in parallel without waiting
        tokio::spawn(async move {
            // Execute emergency sell immediately - parallel execution for speed
            let config = crate::common::config::Config::get().await;
            let selling_config = crate::processor::selling_strategy::SellingConfig::set_from_env();
            let selling_engine = crate::processor::selling_strategy::SellingEngine::new(
                config_clone.app_state.clone().into(),
                Arc::new(config.swap_config.clone()),
                selling_config,
            );
            drop(config);
            
            // Use Copy Target Selling mode (higher priority than regular emergency sell)
            let copy_sell_future = selling_engine.unified_emergency_sell(&mint_clone, false, None, None);
            
            match copy_sell_future.await {
                Ok(signature) => {
                    logger_clone.log(format!("✅ Successfully executed copy sell for token: {} with signature: {}", mint_clone, signature).green().to_string());
                    
                    // Update tracking after successful sell - verify we actually owned the token
                    if let Some(mut token_info) = BOUGHT_TOKEN_LIST.get_mut(&mint_clone) {
                        token_info.current_amount = 0.0;
                        BOUGHT_TOKEN_LIST.remove(&mint_clone);
                        TOKEN_TRACKING.remove(&mint_clone);
                        
                        // Check if all tokens are sold and stop streaming if needed
                        check_and_stop_streaming_if_all_sold(&logger_clone).await;
                        
                        // Cancel monitoring for this token since it's been sold
                        if let Err(e) = cancel_token_monitoring(&mint_clone, &logger_clone).await {
                            logger_clone.log(format!("Failed to cancel monitoring for token {}: {}", mint_clone, e).yellow().to_string());
                        }
                        logger_clone.log(format!("🎯 Copy sell verified - we owned token {} and successfully sold", mint_clone).green().to_string());
                    } else {
                        logger_clone.log(format!("⚠️ Copy sell executed but we didn't own token {} - no cleanup needed", mint_clone).yellow().to_string());
                    }
                },
                Err(e) => {
                    logger_clone.log(format!("❌ Copy sell failed for token {}: {}", mint_clone, e).red().bold().to_string());
                    // Check if we actually owned this token for debugging
                    if let Some(token_info) = BOUGHT_TOKEN_LIST.get(&mint_clone) {
                        logger_clone.log(format!("🔍 We owned token {} - Amount: {}, Protocol: {:?}", mint_clone, token_info.current_amount, token_info.protocol).red().to_string());
                    } else {
                        logger_clone.log(format!("🔍 We didn't own token {} - copy sell was unnecessary", mint_clone).blue().to_string());
                    }
                }
            }
        });
        
        // Don't proceed with buying logic for sell transactions
        return Ok(());
    }

    // Check active tokens count against counter_limit
    let active_tokens_count = TOKEN_TRACKING.len();
    let active_token_list: Vec<String> = TOKEN_TRACKING.iter().map(|entry| entry.key().clone()).collect();
    
    if active_tokens_count >= config.counter_limit as usize {
        logger.log(format!(
            "Skipping buy for token {} - Active tokens ({}) at counter limit ({})",
            parsed_data.mint,
            active_tokens_count,
            config.counter_limit
        ).yellow().to_string());
        
        // Log details about current active tokens for debugging
        logger.log(format!(
            "📊 Current active tokens: [{}]",
            active_token_list.join(", ")
        ).cyan().to_string());

        return Ok(());
    }
    
    // Extract transaction data
    logger.log(format!(
        "{} transaction detected: SOL change: {}, Token change: {}, Is buy: {}",
        match instruction_type {
            transaction_parser::DexType::PumpSwap => "PumpSwap",
            transaction_parser::DexType::PumpFun => "PumpFun",
            transaction_parser::DexType::RaydiumLaunchpad => "RaydiumLaunchpad",
            _ => "Unknown",
        },
        parsed_data.sol_change,
        parsed_data.token_change,
        parsed_data.is_buy
    ).green().to_string());
    
    // Determine protocol based on instruction type
    let protocol = if matches!(instruction_type, transaction_parser::DexType::PumpSwap) {
        SwapProtocol::PumpSwap
    } else if matches!(instruction_type, transaction_parser::DexType::PumpFun) {
        SwapProtocol::PumpFun
    } else if matches!(instruction_type, transaction_parser::DexType::RaydiumLaunchpad) {
        SwapProtocol::RaydiumLaunchpad
    } else {
        // Default to the preferred protocol in config if instruction type is unknown
        config.protocol_preference.clone()
    };
    
    // Handle buy transaction
    if parsed_data.is_buy {
        match execute_buy(
            parsed_data.clone(),
            config.app_state.clone().into(),
            Arc::new(config.swap_config.clone()),
            protocol.clone(),
        ).await {
            Err(e) => {
                logger.log(format!("Error executing buy: {}", e).red().to_string());
                
                Err(e) // Return the error from execute_buy
            },
            Ok(_) => {      
                logger.log(format!("Processing time for buy transaction: {:?}", start_time.elapsed()).blue().to_string());
                logger.log(format!("copied transaction {}", target_signature.clone().unwrap_or_default()).blue().to_string());
                logger.log(format!("Now starting to monitor this token to sell at a profit").blue().to_string());
                
                // Start selling strategy immediately to reduce latency and avoid missed opportunities
                
                 // Setup selling strategy based on take profit and stop loss
                 match setup_selling_strategy(
                     parsed_data.mint.clone(), 
                     config.app_state.clone().into(), 
                     Arc::new(config.swap_config.clone()), 
                     protocol.clone(),
                 ).await {
                     Ok(_) => {
                         logger.log("Selling strategy set up successfully".green().to_string());
                         Ok(())
                     },
                     Err(e) => {
                         logger.log(format!("Failed to set up selling strategy: {}", e).red().to_string());
                         Err(e)
                     }
                 }
            }
        }
    } else {
        // For sell transactions, we don't copy them
        // We rely on our own take profit and stop loss strategy
        logger.log(format!("Processing time for buy transaction: {:?}", start_time.elapsed()).blue().to_string());
        logger.log(format!("Not copying selling transaction - using take profit and stop loss").blue().to_string());
        Ok(())
    }
}

/// Comprehensive token verification and cleanup after selling
async fn verify_sell_transaction_and_cleanup(
    token_mint: &str,
    transaction_signature: Option<&str>,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<bool, String> {
    // Skip if token mint is empty (used for general cleanup calls)
    if token_mint.is_empty() {
        return Ok(false);
    }
    
    logger.log(format!("Starting comprehensive verification and cleanup for token: {}", token_mint));
    
    let mut is_fully_sold = false;
    
    // Step 1: Verify transaction if signature provided
    if let Some(signature) = transaction_signature {
        match verify_transaction(signature, app_state.clone(), logger).await {
            Ok(verified) => {
                if verified {
                    logger.log(format!("✅ Sell transaction verified successfully: {}", signature));
                } else {
                    logger.log(format!("❌ Sell transaction verification failed: {}", signature).red().to_string());
                    return Ok(false);
                }
            },
            Err(e) => {
                logger.log(format!("❌ Error verifying transaction {}: {}", signature, e).red().to_string());
                return Ok(false);
            }
        }
    }
    
    // Step 2: Check actual token balance from blockchain
    if let Ok(wallet_pubkey) = app_state.wallet.try_pubkey() {
        if let Ok(token_pubkey) = Pubkey::from_str(token_mint) {
            let ata = get_associated_token_address(&wallet_pubkey, &token_pubkey);
            
            match app_state.rpc_nonblocking_client.get_token_account(&ata).await {
                Ok(account_result) => {
                    match account_result {
                        Some(account) => {
                            if let Ok(amount_value) = account.token_amount.amount.parse::<f64>() {
                                let decimal_amount = amount_value / 10f64.powi(account.token_amount.decimals as i32);
                                logger.log(format!("Current token balance for {}: {}", token_mint, decimal_amount));
                                
                                if decimal_amount <= 0.000001 {
                                    is_fully_sold = true;
                                    logger.log(format!("✅ Token {} has zero balance - fully sold", token_mint));
                                } else {
                                    logger.log(format!("⚠️  Token {} still has balance: {} - not fully sold", token_mint, decimal_amount));
                                }
                            }
                        },
                        None => {
                            // Token account doesn't exist - means fully sold
                            is_fully_sold = true;
                            logger.log(format!("✅ Token account for {} doesn't exist - fully sold", token_mint));
                        }
                    }
                },
                Err(e) => {
                    logger.log(format!("❌ Error checking token balance for {}: {}", token_mint, e).yellow().to_string());
                    // Don't remove from tracking if we can't verify
                    return Ok(false);
                }
            }
        }
    }
    
    // Step 3: If fully sold, remove from all tracking systems
    if is_fully_sold {
        let mut removed_systems = Vec::new();
        
        // Remove from BOUGHT_TOKEN_LIST
        if BOUGHT_TOKEN_LIST.remove(token_mint).is_some() {
            removed_systems.push("BOUGHT_TOKEN_LIST");
        }
        
        // Check if all tokens are sold and stop streaming if needed (without logger since this is a cleanup function)
        let active_tokens_count = BOUGHT_TOKEN_LIST.len();
        let active_monitoring_count = MONITORING_TASKS.len();
        let active_tracking_count = TOKEN_TRACKING.len();
        
        if active_tokens_count == 0 && active_monitoring_count == 0 && active_tracking_count == 0 {
            SHOULD_CONTINUE_STREAMING.store(false, Ordering::SeqCst);
        }
        
        // Remove from TOKEN_TRACKING (copy_trading.rs)
        if TOKEN_TRACKING.remove(token_mint).is_some() {
            removed_systems.push("TOKEN_TRACKING");
        }
        
        // Remove from selling_strategy TOKEN_TRACKING and TOKEN_METRICS
        if crate::processor::selling_strategy::TOKEN_TRACKING.remove(token_mint).is_some() {
            removed_systems.push("SELLING_STRATEGY_TOKEN_TRACKING");
        }
        
        if crate::processor::selling_strategy::TOKEN_METRICS.remove(token_mint).is_some() {
            removed_systems.push("TOKEN_METRICS");
        }
        
        // Cancel monitoring task
        if let Some((_removed_key, (_token_name, cancel_token))) = MONITORING_TASKS.remove(token_mint) {
            cancel_token.cancel();
            removed_systems.push("MONITORING_TASKS");
        }
        
        // Note: Keeping token account in WALLET_TOKEN_ACCOUNTS for potential future use
        // The ATA may be empty but could be used again later
        
        logger.log(format!(
            "🧹 Comprehensive cleanup completed for {}. Removed from: [{}]", 
            token_mint, 
            removed_systems.join(", ")
        ).green().to_string());
        
        // Log updated active token count
        let active_count = TOKEN_TRACKING.len();
        logger.log(format!("📊 Active tokens count after cleanup: {}", active_count).blue().to_string());
    }
    
    Ok(is_fully_sold)
}

/// Periodic comprehensive cleanup of all tracked tokens
async fn periodic_comprehensive_cleanup(app_state: Arc<AppState>, logger: &Logger) {
    // Get all tokens from both tracking systems
    let tokens_to_check: HashSet<String> = BOUGHT_TOKEN_LIST.iter()
        .map(|entry| entry.key().clone())
        .chain(TOKEN_TRACKING.iter().map(|entry| entry.key().clone()))
        .collect();
    
    if tokens_to_check.is_empty() {
        return;
    }
    
    for token_mint in tokens_to_check {
        match verify_sell_transaction_and_cleanup(
            &token_mint,
            None,
            app_state.clone(),
            logger,
        ).await {
            Ok(_) => {},
            Err(_) => {}
        }
        
        // Small delay between checks to avoid overwhelming RPC
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    
    // Also cleanup selling strategy tokens
    use crate::processor::selling_strategy::TokenManager;
    let token_manager = TokenManager::new();
    let _ = token_manager.verify_and_cleanup_sold_tokens(&app_state).await;
}

/// Manually trigger comprehensive cleanup of all tracking systems
/// Useful for debugging and maintenance
pub async fn trigger_comprehensive_cleanup(app_state: Arc<AppState>) -> Result<(usize, usize), String> {
    let logger = Logger::new("[MANUAL-CLEANUP] => ".magenta().bold().to_string());
    
    // First, run the copy trading cleanup
    let copy_trading_cleaned = match verify_sell_transaction_and_cleanup(
        "",  // Empty string will be ignored in the function
        None,
        app_state.clone(),
        &logger,
    ).await {
        Ok(_) => {
            // Run periodic cleanup for all tokens
            let tokens_to_check: HashSet<String> = BOUGHT_TOKEN_LIST.iter()
                .map(|entry| entry.key().clone())
                .collect();
            
            let mut cleaned = 0;
            for token_mint in tokens_to_check {
                match verify_sell_transaction_and_cleanup(
                    &token_mint,
                    None,
                    app_state.clone(),
                    &logger,
                ).await {
                    Ok(was_cleaned) => {
                        if was_cleaned {
                            cleaned += 1;
                        }
                    },
                    Err(_) => {}
                }
            }
            cleaned
        },
        Err(_) => 0,
    };
    
    // Then run selling strategy cleanup
    use crate::processor::selling_strategy::TokenManager;
    let token_manager = TokenManager::new();
    let selling_strategy_cleaned = match token_manager.verify_and_cleanup_sold_tokens(&app_state).await {
        Ok(cleaned) => cleaned,
        Err(e) => {
            logger.log(format!("Error in selling strategy cleanup: {}", e).red().to_string());
            0
        }
    };
    
    Ok((copy_trading_cleaned, selling_strategy_cleaned))
}



/// Execute emergency sell using the unified selling engine function
/// 
/// **DEPRECATED**: This function is kept for backward compatibility only.
/// Use `SellingEngine::unified_emergency_sell` directly for better performance and consistency.
/// 
/// This wrapper function adds overhead and should be replaced with direct calls to the selling engine.
#[deprecated(note = "Use SellingEngine::unified_emergency_sell directly instead")]
pub async fn execute_emergency_sell_via_engine(
    token_mint: &str,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<String, String> {
    let start_time = std::time::Instant::now();
    
    logger.log(format!("🚀 Executing emergency sell via unified selling engine for token: {}", token_mint));
    
    // Create selling engine with current configuration
    let config = Config::get().await;
    let selling_config = SellingConfig::set_from_env();
    let selling_engine = SellingEngine::new(
        app_state.clone(),
        Arc::new(config.swap_config.clone()),
        selling_config,
    );
    drop(config);
    
    // Execute unified emergency sell (no parsed_data, use metrics instead)
    match selling_engine.unified_emergency_sell(token_mint, false, None, None).await {
        Ok(signature) => {
            let elapsed = start_time.elapsed();
            logger.log(format!("⚡ Emergency sell executed in {:?} for token: {} with signature: {}", 
                elapsed, token_mint, signature).green().bold().to_string());
            Ok(signature)
        },
        Err(e) => {
            logger.log(format!("❌ Emergency sell failed for token {}: {}", token_mint, e).red().to_string());
            Err(format!("Emergency sell failed: {}", e))
        }
    }
}



