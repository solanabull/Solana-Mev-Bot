use std::sync::Arc;
use std::time::{Duration, Instant};
use anyhow::{anyhow, Result};
use anchor_client::solana_sdk::{
    pubkey::Pubkey, 
    signature::{Signature, Keypair}, 
    instruction::Instruction,
    transaction::{VersionedTransaction, Transaction},
    signer::Signer,
    hash::Hash,
};
use spl_associated_token_account::get_associated_token_address;
use colored::Colorize;
use tokio::time::sleep;
use base64;

use crate::common::{
    config::{AppState, SwapConfig},
    logger::Logger,
};
use crate::processor::swap::SwapDirection;
use crate::library::jupiter_api::JupiterClient;
use crate::processor::transaction_parser::TradeInfoFromToken;
use crate::block_engine::tx;

/// Maximum number of retry attempts for selling transactions
const MAX_RETRIES: u32 = 3;

/// Delay between retry attempts
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// Timeout for transaction verification
const VERIFICATION_TIMEOUT: Duration = Duration::from_secs(30);

/// Result of a selling transaction attempt
#[derive(Debug)]
pub struct SellTransactionResult {
    pub success: bool,
    pub signature: Option<Signature>,
    pub error: Option<String>,
    pub used_jupiter_fallback: bool,
    pub attempt_count: u32,
}

/// Enhanced transaction verification with retry logic
pub async fn verify_transaction_with_retry(
    signature: &Signature,
    app_state: Arc<AppState>,
    logger: &Logger,
    max_retries: u32,
) -> Result<bool> {
    let start_time = Instant::now();
    
    for attempt in 1..=max_retries {
        if start_time.elapsed() > VERIFICATION_TIMEOUT {
            logger.log(format!("Transaction verification timeout after {:?}", start_time.elapsed()).yellow().to_string());
            return Ok(false);
        }

        logger.log(format!("Verifying transaction attempt {}/{}: {}", attempt, max_retries, signature));

        match app_state.rpc_nonblocking_client.get_signature_statuses(&[*signature]).await {
            Ok(result) => {
                if let Some(status_opt) = result.value.get(0) {
                    if let Some(status) = status_opt {
                        if status.err.is_none() {
                            logger.log(format!("✅ Transaction verified successfully: {}", signature).green().to_string());
                            return Ok(true);
                        } else {
                            logger.log(format!("❌ Transaction failed with error: {:?}", status.err).red().to_string());
                            return Ok(false);
                        }
                    }
                }
            }
            Err(e) => {
                logger.log(format!("RPC error during verification attempt {}: {}", attempt, e).yellow().to_string());
            }
        }

        if attempt < max_retries {
            sleep(Duration::from_millis(1000)).await;
        }
    }

    logger.log(format!("Transaction verification failed after {} attempts", max_retries).red().to_string());
    Ok(false)
}

/// Execute a selling transaction with retry and Jupiter fallback
pub async fn execute_sell_with_retry_and_fallback(
    trade_info: &TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<SellTransactionResult> {
    let token_mint = &trade_info.mint;
    logger.log(format!("🔄 Starting sell transaction with retry for token: {}", token_mint).cyan().to_string());

    // First, try the normal selling flow with retries
    match execute_normal_sell_with_retry(trade_info, sell_config.clone(), app_state.clone(), logger).await {
        Ok(result) => {
            if result.success {
                logger.log(format!("✅ Normal sell succeeded on attempt {}", result.attempt_count).green().to_string());
                return Ok(result);
            }
        }
        Err(e) => {
            logger.log(format!("❌ Normal sell attempts failed: {}", e).yellow().to_string());
        }
    }

    // If normal selling failed after retries, try Jupiter fallback
    logger.log(format!("🚀 Attempting Jupiter API fallback for token: {}", token_mint).purple().to_string());
    
    match execute_jupiter_fallback_sell(trade_info, &sell_config, app_state.clone(), logger).await {
        Ok(signature) => {
            logger.log(format!("✅ Jupiter fallback sell succeeded: {}", signature).green().to_string());
            Ok(SellTransactionResult {
                success: true,
                signature: Some(signature),
                error: None,
                used_jupiter_fallback: true,
                attempt_count: MAX_RETRIES + 1,
            })
        }
        Err(e) => {
            logger.log(format!("❌ Jupiter fallback sell failed: {}", e).red().to_string());
            Ok(SellTransactionResult {
                success: false,
                signature: None,
                error: Some(format!("All sell attempts failed. Last error: {}", e)),
                used_jupiter_fallback: true,
                attempt_count: MAX_RETRIES + 1,
            })
        }
    }
}

/// Execute normal selling flow with retry logic
async fn execute_normal_sell_with_retry(
    trade_info: &TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<SellTransactionResult> {
    let mut last_error = String::new();

    for attempt in 1..=MAX_RETRIES {
        logger.log(format!("🔄 Normal sell attempt {}/{} for token: {}", attempt, MAX_RETRIES, trade_info.mint).cyan().to_string());

        match execute_single_sell_attempt(trade_info, sell_config.clone(), app_state.clone(), logger).await {
            Ok(signature) => {
                // Verify the transaction
                match verify_transaction_with_retry(&signature, app_state.clone(), logger, 5).await {
                    Ok(verified) => {
                        if verified {
                            logger.log(format!("✅ Normal sell succeeded on attempt {}: {}", attempt, signature).green().to_string());
                            return Ok(SellTransactionResult {
                                success: true,
                                signature: Some(signature),
                                error: None,
                                used_jupiter_fallback: false,
                                attempt_count: attempt,
                            });
                        } else {
                            last_error = format!("Transaction verification failed for signature: {}", signature);
                            logger.log(format!("❌ Attempt {} failed: {}", attempt, last_error).yellow().to_string());
                        }
                    }
                    Err(e) => {
                        last_error = format!("Verification error: {}", e);
                        logger.log(format!("❌ Attempt {} failed: {}", attempt, last_error).yellow().to_string());
                    }
                }
            }
            Err(e) => {
                last_error = e.to_string();
                logger.log(format!("❌ Attempt {} failed: {}", attempt, last_error).yellow().to_string());
            }
        }

        if attempt < MAX_RETRIES {
            logger.log(format!("⏳ Waiting {:?} before retry...", RETRY_DELAY).yellow().to_string());
            sleep(RETRY_DELAY).await;
        }
    }

    Err(anyhow!("Normal sell failed after {} attempts. Last error: {}", MAX_RETRIES, last_error))
}

/// Execute a single sell attempt using the existing selling logic
async fn execute_single_sell_attempt(
    trade_info: &TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<Signature> {
    // Determine which DEX to use based on trade info
    match trade_info.dex_type {
        crate::processor::transaction_parser::DexType::PumpFun => {
            execute_pumpfun_sell_attempt(trade_info, sell_config, app_state, logger).await
        }
        crate::processor::transaction_parser::DexType::PumpSwap => {
            execute_pumpswap_sell_attempt(trade_info, sell_config, app_state, logger).await
        }
        crate::processor::transaction_parser::DexType::RaydiumLaunchpad => {
            execute_raydium_sell_attempt(trade_info, sell_config, app_state, logger).await
        }
        _ => {
            // Default to PumpFun for unknown protocols
            execute_pumpfun_sell_attempt(trade_info, sell_config, app_state, logger).await
        }
    }
}

/// Execute PumpFun sell attempt
async fn execute_pumpfun_sell_attempt(
    trade_info: &TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<Signature> {
    let pump = crate::dex::pump_fun::Pump::new(
        app_state.rpc_nonblocking_client.clone(),
        app_state.rpc_client.clone(),
        app_state.wallet.clone(),
    );

    let (keypair, instructions, _price) = pump.build_swap_from_parsed_data(trade_info, sell_config).await
        .map_err(|e| anyhow!("Failed to build PumpFun swap: {}", e))?;

    let recent_blockhash = crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await
        .ok_or_else(|| anyhow!("Failed to get recent blockhash"))?;

    let signatures = crate::block_engine::tx::new_signed_and_send_with_landing_mode(
        crate::common::config::TransactionLandingMode::Normal,
        &app_state,
        recent_blockhash,
        &keypair,
        instructions,
        logger,
    ).await.map_err(|e| anyhow!("Failed to send transaction: {}", e))?;

    if signatures.is_empty() {
        return Err(anyhow!("No transaction signature returned"));
    }

    // Parse the string signature to Signature type
    let signature = signatures[0].parse::<Signature>()
        .map_err(|e| anyhow!("Failed to parse signature: {}", e))?;
    Ok(signature)
}

/// Execute Raydium sell attempt
async fn execute_raydium_sell_attempt(
    trade_info: &TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<Signature> {
    let raydium = crate::dex::raydium_launchpad::Raydium::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );

    let (keypair, instructions, _price) = raydium.build_swap_from_parsed_data(trade_info, sell_config).await
        .map_err(|e| anyhow!("Failed to build Raydium swap: {}", e))?;

    let recent_blockhash = crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await
        .ok_or_else(|| anyhow!("Failed to get recent blockhash"))?;

    let signatures = crate::block_engine::tx::new_signed_and_send_zeroslot(
        app_state.zeroslot_rpc_client.clone(),
        recent_blockhash,
        &keypair,
        instructions,
        logger,
    ).await.map_err(|e| anyhow!("Failed to send transaction: {}", e))?;

    if signatures.is_empty() {
        return Err(anyhow!("No transaction signature returned"));
    }

    // Parse the string signature to Signature type
    let signature = signatures[0].parse::<Signature>()
        .map_err(|e| anyhow!("Failed to parse signature: {}", e))?;
    Ok(signature)
}

/// Execute PumpSwap sell attempt
async fn execute_pumpswap_sell_attempt(
    trade_info: &TradeInfoFromToken,
    sell_config: SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<Signature> {
    let pump_swap = crate::dex::pump_swap::PumpSwap::new(
        app_state.wallet.clone(),
        Some(app_state.rpc_client.clone()),
        Some(app_state.rpc_nonblocking_client.clone()),
    );

    let (keypair, instructions, _price) = pump_swap.build_swap_from_parsed_data(trade_info, sell_config).await
        .map_err(|e| anyhow!("Failed to build PumpSwap swap: {}", e))?;

    let recent_blockhash = crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await
        .ok_or_else(|| anyhow!("Failed to get recent blockhash"))?;

    let signatures = crate::block_engine::tx::new_signed_and_send_with_landing_mode(
        crate::common::config::TransactionLandingMode::Normal,
        &app_state,
        recent_blockhash,
        &keypair,
        instructions,
        logger,
    ).await.map_err(|e| anyhow!("Failed to send transaction: {}", e))?;

    if signatures.is_empty() {
        return Err(anyhow!("No transaction signature returned"));
    }

    let signature = signatures[0].parse::<Signature>()
        .map_err(|e| anyhow!("Failed to parse signature: {}", e))?;
    Ok(signature)
}

/// Execute Jupiter API fallback sell
async fn execute_jupiter_fallback_sell(
    trade_info: &TradeInfoFromToken,
    sell_config: &SwapConfig,
    app_state: Arc<AppState>,
    logger: &Logger,
) -> Result<Signature> {
    logger.log("🚀 Executing Jupiter API fallback sell".purple().to_string());

    // Get wallet pubkey
    let wallet_pubkey = app_state.wallet.try_pubkey()
        .map_err(|e| anyhow!("Failed to get wallet pubkey: {}", e))?;

    // Get token mint pubkey
    let token_pubkey = trade_info.mint.parse::<Pubkey>()
        .map_err(|e| anyhow!("Invalid token mint address: {}", e))?;

    // Get associated token account
    let ata = get_associated_token_address(&wallet_pubkey, &token_pubkey);

    // Get current token balance
    let token_account = app_state.rpc_nonblocking_client.get_token_account(&ata).await
        .map_err(|e| anyhow!("Failed to get token account: {}", e))?
        .ok_or_else(|| anyhow!("Token account not found"))?;

    let token_amount = token_account.token_amount.amount.parse::<u64>()
        .map_err(|e| anyhow!("Failed to parse token amount: {}", e))?;

    if token_amount == 0 {
        return Err(anyhow!("No tokens to sell"));
    }

    // Apply sell percentage based on amount_in field (which represents percentage for sells)
    let amount_to_sell = if sell_config.amount_in >= 1.0 {
        token_amount
    } else {
        ((token_amount as f64) * sell_config.amount_in) as u64
    };

    logger.log(format!("💱 Selling {} tokens via Jupiter API", amount_to_sell));

    // Initialize Jupiter API client
            let jupiter_client = JupiterClient::new(app_state.rpc_nonblocking_client.clone());

    // Execute sell transaction via Jupiter API (this handles signing and sending)
    let (signature_str, expected_sol) = jupiter_client.sell_token(
        &trade_info.mint,
        amount_to_sell,
        (sell_config.slippage as u32 * 100) as u64, // Convert to basis points as u64
        &wallet_pubkey,
    ).await.map_err(|e| anyhow!("Jupiter API sell failed: {}", e))?;

    logger.log(format!("💰 Expected SOL from sale: {:.6}", expected_sol));
    
    // Parse the signature string into a Signature type
    let signature = signature_str.parse::<anchor_client::solana_sdk::signature::Signature>()
        .map_err(|e| anyhow!("Failed to parse signature: {}", e))?;

    logger.log(format!("✅ Jupiter transaction sent: {}", signature).green().to_string());

    // Verify the transaction
    match verify_transaction_with_retry(&signature, app_state, logger, 5).await {
        Ok(verified) => {
            if verified {
                Ok(signature)
            } else {
                Err(anyhow!("Jupiter transaction verification failed"))
            }
        }
        Err(e) => Err(anyhow!("Jupiter transaction verification error: {}", e))
    }
} 