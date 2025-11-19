/*!
# Risk Management Module

This module provides automated risk management for copy trading operations by monitoring target wallet token balances.

## Environment Variables

The following environment variables control the risk management system:

- `RISK_MINIMUM_TARGET_BALANCE`: Minimum token balance threshold below which to trigger emergency sells (default: `1000.0`)
- `RISK_CHECK_INTERVAL_MINUTES`: Interval in minutes between balance checks (default: `10`)

## How It Works

1. **Monitoring**: Every 10 minutes (configurable), the service automatically checks target wallet balances for all currently held tokens
2. **Risk Detection**: If a target wallet's balance for any held token drops below the configured threshold, it triggers an emergency sell
3. **Emergency Selling**: Uses the existing enhanced sell mechanism to immediately sell all of the token
4. **Cleanup**: Removes the sold token from the tracking system

## Example Configuration

```env
RISK_MINIMUM_TARGET_BALANCE=1000.0
RISK_CHECK_INTERVAL_MINUTES=10
```

With this configuration, the system will automatically check target wallet balances every 10 minutes and sell any held token 
where the target wallet's balance falls below 1000 tokens.
*/

use std::time::{Duration, Instant};
use solana_program_pack::Pack;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time;
use colored::Colorize;
use anchor_client::solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use spl_token::state::Account as TokenAccount;
use spl_token_2022::extension::StateWithExtensionsOwned;

use crate::common::logger::Logger;
use crate::common::config::{AppState, SwapConfig, import_env_var};
use crate::processor::sniper_bot::{BOUGHT_TOKEN_LIST, BoughtTokenInfo};

/// Risk management configuration
pub struct RiskManagementConfig {
    pub target_addresses: Vec<String>,
    pub minimum_target_balance: f64, // Minimum token balance threshold (default: 1000)
    pub check_interval_minutes: u64, // Check interval in minutes (default: 10)
    pub app_state: Arc<AppState>,
    pub swap_config: Arc<SwapConfig>,
}

impl RiskManagementConfig {
    pub fn new(
        target_addresses: Vec<String>,
        app_state: Arc<AppState>,
        swap_config: Arc<SwapConfig>,
    ) -> Self {
        let minimum_target_balance = import_env_var("RISK_MINIMUM_TARGET_BALANCE")
            .parse::<f64>()
            .unwrap_or(1000.0);
        
        let check_interval_minutes = import_env_var("RISK_CHECK_INTERVAL_MINUTES")
            .parse::<u64>()
            .unwrap_or(10);

        Self {
            target_addresses,
            minimum_target_balance,
            check_interval_minutes,
            app_state,
            swap_config,
        }
    }
}

/// Risk management service that monitors target wallet balances
pub struct RiskManagementService {
    config: RiskManagementConfig,
    logger: Logger,
}

impl RiskManagementService {
    pub fn new(config: RiskManagementConfig) -> Self {
        Self {
            config,
            logger: Logger::new("[RISK-MANAGEMENT] => ".red().bold().to_string()),
        }
    }

    /// Start the risk management monitoring service
    pub async fn start(&self) -> Result<(), String> {
        self.logger.log(format!(
            "🚨 Starting risk management service - checking every {} minutes for target balance < {}",
            self.config.check_interval_minutes,
            self.config.minimum_target_balance
        ).yellow().to_string());

        let mut interval = time::interval(Duration::from_secs(self.config.check_interval_minutes * 60));

        loop {
            interval.tick().await;
            
            if let Err(e) = self.check_target_balances().await {
                self.logger.log(format!("Error during balance check: {}", e).red().to_string());
            }
        }
    }

    /// Check target wallet balances and trigger sells if needed
    async fn check_target_balances(&self) -> Result<(), String> {
        self.logger.log("🔍 Checking target wallet balances...".cyan().to_string());

        // Get all currently held tokens
        let held_tokens: Vec<(String, BoughtTokenInfo)> = BOUGHT_TOKEN_LIST
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect();

        if held_tokens.is_empty() {
            self.logger.log("No tokens currently held, skipping balance check".yellow().to_string());
            return Ok(());
        }

        self.logger.log(format!("Found {} held tokens to check", held_tokens.len()).cyan().to_string());

        // Check each target wallet for each held token
        for target_address in &self.config.target_addresses {
            if let Err(e) = self.check_target_wallet_balances(target_address, &held_tokens).await {
                self.logger.log(format!("Error checking balances for target {}: {}", target_address, e).red().to_string());
                continue;
            }
        }

        Ok(())
    }

    /// Check balances for a specific target wallet
    async fn check_target_wallet_balances(
        &self,
        target_address: &str,
        held_tokens: &[(String, BoughtTokenInfo)],
    ) -> Result<(), String> {
        let target_pubkey = Pubkey::from_str(target_address)
            .map_err(|e| format!("Invalid target address {}: {}", target_address, e))?;

        // Get target wallet's token balances for our held tokens
        let target_balances = self.get_target_token_balances(&target_pubkey, held_tokens).await?;

        // Check each token balance and trigger sells if needed
        for (token_mint, token_info) in held_tokens {
            if let Some(target_balance) = target_balances.get(token_mint) {
                self.logger.log(format!(
                    "Token {} - Target balance: {:.2}, Threshold: {:.2}",
                    token_mint,
                    target_balance,
                    self.config.minimum_target_balance
                ).cyan().to_string());

                // If target balance is below threshold, trigger emergency sell
                if *target_balance < self.config.minimum_target_balance {
                    self.logger.log(format!(
                        "🚨 RISK TRIGGER: Target {} balance ({:.2}) below threshold ({:.2}) for token {}",
                        target_address,
                        target_balance,
                        self.config.minimum_target_balance,
                        token_mint
                    ).red().bold().to_string());

                    // Trigger emergency sell
                    if let Err(e) = self.trigger_emergency_sell(token_mint, token_info).await {
                        self.logger.log(format!(
                            "❌ Failed to execute emergency sell for {}: {}",
                            token_mint, e
                        ).red().to_string());
                    } else {
                        self.logger.log(format!(
                            "✅ Successfully triggered emergency sell for {}",
                            token_mint
                        ).green().to_string());
                    }
                }
            } else {
                self.logger.log(format!(
                    "⚠️  Target {} has no balance for token {} - triggering emergency sell",
                    target_address, token_mint
                ).yellow().to_string());

                // If no balance found, also trigger emergency sell
                if let Err(e) = self.trigger_emergency_sell(token_mint, token_info).await {
                    self.logger.log(format!(
                        "❌ Failed to execute emergency sell for {}: {}",
                        token_mint, e
                    ).red().to_string());
                }
            }
        }

        Ok(())
    }

    /// Get token balances for target wallet for specific tokens
    async fn get_target_token_balances(
        &self,
        target_pubkey: &Pubkey,
        held_tokens: &[(String, BoughtTokenInfo)],
    ) -> Result<HashMap<String, f64>, String> {
        let mut balances = HashMap::new();

        // Get all token accounts for the target wallet
        let token_program = Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")
            .map_err(|e| format!("Invalid token program ID: {}", e))?;

        let accounts = self.config.app_state
            .rpc_client
            .get_token_accounts_by_owner(
                target_pubkey,
                anchor_client::solana_client::rpc_request::TokenAccountsFilter::ProgramId(token_program),
            )
            .map_err(|e| format!("Failed to get token accounts: {}", e))?;

        // Parse each account and match with our held tokens
        for account_info in accounts {
            if let Ok(account_pubkey) = Pubkey::from_str(&account_info.pubkey) {
                if let Ok(account_data) = self.config.app_state.rpc_client.get_account(&account_pubkey) {
                    if let Ok(parsed_account) = TokenAccount::unpack(&account_data.data) {
                        let mint_str = parsed_account.mint.to_string();
                        
                        // Check if this mint is one of our held tokens
                        if held_tokens.iter().any(|(token_mint, _)| token_mint == &mint_str) {
                            // Get mint info to calculate actual balance
                            if let Ok(mint_data) = self.config.app_state.rpc_client.get_account(&parsed_account.mint) {
                                if let Ok(mint_info) = spl_token::state::Mint::unpack(&mint_data.data) {
                                    let raw_balance = parsed_account.amount;
                                    let decimals = mint_info.decimals;
                                    let actual_balance = raw_balance as f64 / 10_f64.powi(decimals as i32);
                                    
                                    balances.insert(mint_str.clone(), actual_balance);
                                    
                                    self.logger.log(format!(
                                        "Found target balance for {}: {:.2} tokens",
                                        mint_str, actual_balance
                                    ).blue().to_string());
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(balances)
    }

    /// Trigger emergency sell for a specific token
    async fn trigger_emergency_sell(
        &self,
        token_mint: &str,
        token_info: &BoughtTokenInfo,
    ) -> Result<(), String> {
        self.logger.log(format!(
            "🔥 Executing emergency sell for token {} due to risk management trigger",
            token_mint
        ).red().bold().to_string());

        // Use the existing emergency sell mechanism
        crate::processor::sniper_bot::execute_enhanced_sell(
            token_mint.to_string(),
            self.config.app_state.clone(),
            self.config.swap_config.clone(),
        ).await?;

        // Remove from bought tokens list
        BOUGHT_TOKEN_LIST.remove(token_mint);
        
        // Check if all tokens are sold and stop streaming if needed
        crate::processor::sniper_bot::check_and_stop_streaming_if_all_sold(&self.logger).await;
        
        self.logger.log(format!(
            "✅ Emergency sell completed and token {} removed from tracking",
            token_mint
        ).green().to_string());

        Ok(())
    }
}

/// Start the risk management service
pub async fn start_risk_management_service(
    target_addresses: Vec<String>,
    app_state: Arc<AppState>,
    swap_config: Arc<SwapConfig>,
) -> Result<(), String> {
    let logger = Logger::new("[RISK-MANAGEMENT] => ".red().bold().to_string());
    logger.log("Starting risk management service...".green().to_string());

    let config = RiskManagementConfig::new(target_addresses, app_state, swap_config);
    let service = RiskManagementService::new(config);
    
    // Start the service in a background task
    tokio::spawn(async move {
        if let Err(e) = service.start().await {
            eprintln!("Risk management service error: {}", e);
        }
    });

    Ok(())
} 