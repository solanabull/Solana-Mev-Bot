//! Mempool listener for real-time transaction monitoring
//!
//! Monitors Solana mempool via WebSocket subscriptions to detect
//! MEV opportunities in real-time.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{pubkey::Pubkey, signature::Signature};

use crate::utils::config::Config;
use crate::utils::logger;

/// Transaction data from mempool
#[derive(Debug, Clone)]
pub struct MempoolTransaction {
    pub signature: Signature,
    pub account_keys: Vec<Pubkey>,
    pub instructions: Vec<InstructionData>,
    pub recent_blockhash: String,
    pub fee: u64,
    pub timestamp: u64,
    pub slot: u64,
}

/// Instruction data extracted from transaction
#[derive(Debug, Clone)]
pub struct InstructionData {
    pub program_id: Pubkey,
    pub accounts: Vec<Pubkey>,
    pub data: Vec<u8>,
    pub decoded_instruction: Option<DecodedInstruction>,
}

/// Decoded instruction types
#[derive(Debug, Clone)]
pub enum DecodedInstruction {
    Swap(SwapInstruction),
    Transfer(TransferInstruction),
    // Add more instruction types as needed
}

/// Swap instruction data
#[derive(Debug, Clone)]
pub struct SwapInstruction {
    pub token_in: Pubkey,
    pub token_out: Pubkey,
    pub amount_in: u64,
    pub amount_out_min: u64,
    pub signer: Pubkey,
}

/// Transfer instruction data
#[derive(Debug, Clone)]
pub struct TransferInstruction {
    pub from: Pubkey,
    pub to: Pubkey,
    pub amount: u64,
}

/// Mempool listener state
#[derive(Debug)]
pub struct MempoolListener {
    config: Config,
    solana_client: Arc<RpcClient>,
    websocket_url: String,
    subscriptions: HashSet<String>,
    pending_transactions: Arc<RwLock<HashMap<Signature, MempoolTransaction>>>,
    opportunity_sender: broadcast::Sender<MempoolTransaction>,
    running: Arc<RwLock<bool>>,
    last_health_check: Arc<RwLock<u64>>,
    error_count: Arc<RwLock<u32>>,
}

impl MempoolListener {
    /// Create new mempool listener
    pub async fn new(
        solana_client: Arc<RpcClient>,
        config: Config,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let websocket_url = config.solana.ws_url.replace("https", "wss");
        let (opportunity_sender, _) = broadcast::channel(1000);

        Ok(Self {
            config,
            solana_client,
            websocket_url,
            subscriptions: HashSet::new(),
            pending_transactions: Arc::new(RwLock::new(HashMap::new())),
            opportunity_sender,
            running: Arc::new(RwLock::new(false)),
            last_health_check: Arc::new(RwLock::new(0)),
            error_count: Arc::new(RwLock::new(0)),
        })
    }

    /// Start listening to mempool
    pub async fn listen(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        *self.running.write().await = true;

        logger::info!("Starting mempool listener on {}", self.websocket_url);

        loop {
            if !*self.running.read().await {
                break;
            }

            match self.connect_and_listen().await {
                Ok(_) => {
                    logger::info!("Mempool listener connection closed gracefully");
                }
                Err(e) => {
                    *self.error_count.write().await += 1;
                    logger::error!("Mempool listener connection error: {}", e);
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                }
            }
        }

        Ok(())
    }

    /// Connect to WebSocket and start listening
    async fn connect_and_listen(&self) -> Result<(), Box<dyn std::error::Error>> {
        let (ws_stream, _) = connect_async(&self.websocket_url).await?;
        let (mut write, mut read) = ws_stream.split();

        // Subscribe to relevant feeds
        self.subscribe_to_feeds(&mut write).await?;

        // Listen for messages
        while let Some(message) = read.next().await {
            if !*self.running.read().await {
                break;
            }

            match message {
                Ok(Message::Text(text)) => {
                    if let Err(e) = self.process_message(&text).await {
                        logger::error!("Error processing message: {}", e);
                    }
                }
                Ok(Message::Binary(_)) => {
                    // Handle binary messages if needed
                }
                Ok(Message::Close(_)) => {
                    logger::info!("WebSocket connection closed");
                    break;
                }
                Err(e) => {
                    logger::error!("WebSocket error: {}", e);
                    break;
                }
                _ => {}
            }

            // Update health check timestamp
            *self.last_health_check.write().await = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();
        }

        Ok(())
    }

    /// Subscribe to WebSocket feeds
    async fn subscribe_to_feeds(
        &self,
        write: &mut futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>
            >,
            Message
        >
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut subscriptions = Vec::new();

        // Subscribe to logs for DEX programs
        if self.config.mempool.subscription_filters.contains(&"logs".to_string()) {
            for program_id in &self.config.mempool.dex_programs {
                let subscription = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "logsSubscribe",
                    "params": [
                        {"mentions": [program_id]},
                        {"commitment": "processed"}
                    ]
                });
                subscriptions.push(subscription);
            }
        }

        // Subscribe to program account changes
        if self.config.mempool.subscription_filters.contains(&"program".to_string()) {
            for program_id in &self.config.mempool.dex_programs {
                let subscription = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "programSubscribe",
                    "params": [
                        program_id,
                        {"commitment": "processed", "encoding": "base64"}
                    ]
                });
                subscriptions.push(subscription);
            }
        }

        // Send subscriptions
        for subscription in subscriptions {
            let message = Message::Text(subscription.to_string());
            write.send(message).await?;
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        Ok(())
    }

    /// Process incoming WebSocket message
    async fn process_message(&self, text: &str) -> Result<(), Box<dyn std::error::Error>> {
        let message: Value = serde_json::from_str(text)?;

        // Check if this is a notification
        if let Some(method) = message.get("method") {
            match method.as_str() {
                Some("logsNotification") => {
                    self.process_logs_notification(&message).await?;
                }
                Some("programNotification") => {
                    self.process_program_notification(&message).await?;
                }
                Some("accountNotification") => {
                    self.process_account_notification(&message).await?;
                }
                _ => {
                    // Other notification types
                }
            }
        }

        Ok(())
    }

    /// Process logs notification
    async fn process_logs_notification(&self, message: &Value) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(params) = message.get("params") {
            if let Some(result) = params.get("result") {
                if let Some(logs) = result.get("value").and_then(|v| v.get("logs")) {
                    if let Some(logs_array) = logs.as_array() {
                        for log in logs_array {
                            if let Some(log_str) = log.as_str() {
                                self.analyze_log(log_str).await?;
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Process program notification
    async fn process_program_notification(&self, message: &Value) -> Result<(), Box<dyn std::error::Error>> {
        // Extract account data and decode instructions
        if let Some(params) = message.get("params") {
            if let Some(result) = params.get("result") {
                if let Some(account_data) = result.get("value").and_then(|v| v.get("account")) {
                    // Process account changes for DEX programs
                    self.analyze_account_change(account_data).await?;
                }
            }
        }

        Ok(())
    }

    /// Process account notification
    async fn process_account_notification(&self, message: &Value) -> Result<(), Box<dyn std::error::Error>> {
        // Handle account-specific notifications
        if let Some(params) = message.get("params") {
            if let Some(result) = params.get("result") {
                if let Some(account_info) = result.get("value") {
                    // Process account updates
                    self.analyze_account_update(account_info).await?;
                }
            }
        }

        Ok(())
    }

    /// Analyze transaction log for MEV opportunities
    async fn analyze_log(&self, log: &str) -> Result<(), Box<dyn std::error::Error>> {
        // Look for swap-related logs
        if log.contains("Instruction: Swap") ||
           log.contains("Instruction: ExactIn") ||
           log.contains("Instruction: ExactOut") {

            logger::log_opportunity_detected(
                "potential_swap",
                0.0, // Will be calculated later
                "unknown",
                "unknown",
                &[],
            );

            // Extract signature if available and create transaction record
            // This is a simplified version - in production you'd parse the full log
        }

        Ok(())
    }

    /// Analyze account changes
    async fn analyze_account_change(&self, account_data: &Value) -> Result<(), Box<dyn std::error::Error>> {
        // Analyze DEX pool state changes
        // This would decode AMM pool data and look for price movements
        Ok(())
    }

    /// Analyze account updates
    async fn analyze_account_update(&self, account_info: &Value) -> Result<(), Box<dyn std::error::Error>> {
        // Analyze account balance changes that might indicate MEV opportunities
        Ok(())
    }

    /// Get opportunity receiver for strategies
    pub fn get_opportunity_receiver(&self) -> broadcast::Receiver<MempoolTransaction> {
        self.opportunity_sender.subscribe()
    }

    /// Stop the mempool listener
    pub async fn stop(&self) -> Result<(), Box<dyn std::error::Error>> {
        *self.running.write().await = false;
        Ok(())
    }

    /// Health check for monitoring
    pub async fn health_check(&self) -> ComponentHealth {
        let last_active = *self.last_health_check.read().await;
        let error_count = *self.error_count.read().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let healthy = now - last_active < 60; // Healthy if active within last minute

        ComponentHealth {
            healthy,
            last_active,
            error_count,
            status_message: if healthy {
                "Mempool listener active".to_string()
            } else {
                "Mempool listener inactive".to_string()
            },
        }
    }

    /// Get pending transaction count
    pub async fn pending_transaction_count(&self) -> usize {
        self.pending_transactions.read().await.len()
    }
}
