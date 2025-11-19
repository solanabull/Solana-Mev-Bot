use std::{str::FromStr, sync::Arc, time::Instant};
use solana_program_pack::Pack;
use anchor_client::solana_client::rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use anchor_client::solana_client::rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType};
use solana_account_decoder::UiAccountEncoding;
use anyhow::{anyhow, Result};
use colored::Colorize;
use anchor_client::solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::Keypair,
    system_program,
    signer::Signer,
};
use crate::engine::transaction_parser::DexType;
use spl_associated_token_account::{
    get_associated_token_address,
    instruction::create_associated_token_account_idempotent
};
use spl_token::ui_amount_to_amount;


use crate::{
    common::{config::SwapConfig, logger::Logger, cache::WALLET_TOKEN_ACCOUNTS},
    core::token,
    engine::swap::{SwapDirection, SwapInType},
};

// Constants - moved to lazy_static for single initialization
lazy_static::lazy_static! {
    static ref TOKEN_PROGRAM: Pubkey = Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();
    static ref TOKEN_2022_PROGRAM: Pubkey = Pubkey::from_str("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb").unwrap();
    static ref ASSOCIATED_TOKEN_PROGRAM: Pubkey = Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL").unwrap();
    static ref RAYDIUM_LAUNCHPAD_PROGRAM: Pubkey = Pubkey::from_str("LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj").unwrap();
    static ref RAYDIUM_LAUNCHPAD_AUTHORITY: Pubkey = Pubkey::from_str("WLHv2UAZm6z4KyaaELi5pjdbJh6RESMva1Rnn8pJVVh").unwrap();
    static ref RAYDIUM_GLOBAL_CONFIG: Pubkey = Pubkey::from_str("6s1xP3hpbAfFoNtUNF8mfHsjr2Bd97JxFJRWLbL6aHuX").unwrap();
    static ref RAYDIUM_PLATFORM_CONFIG: Pubkey = Pubkey::from_str("FfYek5vEz23cMkWsdJwG2oa6EphsvXSHrGpdALN4g6W1").unwrap();
    static ref EVENT_AUTHORITY: Pubkey = Pubkey::from_str("2DPAtwB8L12vrMRExbLuyGnC7n2J5LNoZQSejeQGpwkr").unwrap();
    static ref SOL_MINT: Pubkey = Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap();
    static ref BUY_DISCRIMINATOR: [u8; 8] = [250, 234, 13, 123, 213, 156, 19, 236]; //buy_exact_in discriminator
    static ref SELL_DISCRIMINATOR: [u8; 8] = [149, 39, 222, 155, 211, 124, 152, 26]; //sell_exact_in discriminator
}

const TEN_THOUSAND: u64 = 10000;
const POOL_VAULT_SEED: &[u8] = b"pool_vault";



/// A struct to represent the Raydium pool which uses constant product AMM
#[derive(Debug, Clone)]
pub struct RaydiumPool {
    pub pool_id: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub pool_base_account: Pubkey,
    pub pool_quote_account: Pubkey,
}

pub struct Raydium {
    pub keypair: Arc<Keypair>,
    pub rpc_client: Option<Arc<anchor_client::solana_client::rpc_client::RpcClient>>,
    pub rpc_nonblocking_client: Option<Arc<anchor_client::solana_client::nonblocking::rpc_client::RpcClient>>,
}

impl Raydium {
    pub fn new(
        keypair: Arc<Keypair>,
        rpc_client: Option<Arc<anchor_client::solana_client::rpc_client::RpcClient>>,
        rpc_nonblocking_client: Option<Arc<anchor_client::solana_client::nonblocking::rpc_client::RpcClient>>,
    ) -> Self {
        Self {
            keypair,
            rpc_client,
            rpc_nonblocking_client,
        }
    }

    pub async fn get_raydium_pool(
        &self,
        mint_str: &str,
    ) -> Result<RaydiumPool> {
        let mint = Pubkey::from_str(mint_str).map_err(|_| anyhow!("Invalid mint address"))?;
        let rpc_client = self.rpc_client.clone()
            .ok_or_else(|| anyhow!("RPC client not initialized"))?;
        get_pool_info(rpc_client, mint).await
    }

    pub async fn get_token_price(&self, mint_str: &str) -> Result<f64> {
        // For Raydium Launchpad, this method is mainly used for standalone price queries
        // Since we're now using trade_info.price directly in the main flow,
        // this fallback method returns a placeholder value
        let _pool = self.get_raydium_pool(mint_str).await?;
        
        // Return a placeholder price since the real price should come from trade_info
        // This method is rarely used in the main trading flow
        // Note: The correct Raydium Launchpad price formula is:
        // Price = (virtual_quote_reserve - real_quote_after) / (virtual_base_reserve - real_base_after)
        Ok(0.0001) // Placeholder price in SOL
    }

    async fn get_or_fetch_pool_info(
        &self,
        trade_info: &crate::engine::transaction_parser::TradeInfoFromToken,
        mint: Pubkey
    ) -> Result<RaydiumPool> {
        // Use pool_id from trade_info instead of fetching it dynamically
        let pool_id = Pubkey::from_str(&trade_info.pool_id)
            .map_err(|e| anyhow!("Invalid pool_id in trade_info: {}", e))?;
        
        // For Raydium Launchpad, derive pool vault addresses using PDA (Program Derived Address)
        let pump_program = *RAYDIUM_LAUNCHPAD_PROGRAM;
        let sol_mint = *SOL_MINT;
        
        // Derive pool vault addresses using PDA with specific seeds
        let base_seeds = [POOL_VAULT_SEED, pool_id.as_ref(), mint.as_ref()];
        let (pool_base_account, _) = Pubkey::find_program_address(&base_seeds, &pump_program);
        
        let quote_seeds = [POOL_VAULT_SEED, pool_id.as_ref(), sol_mint.as_ref()];
        let (pool_quote_account, _) = Pubkey::find_program_address(&quote_seeds, &pump_program);
        
        let pool_info = RaydiumPool {
            pool_id,
            base_mint: mint,
            quote_mint: sol_mint,
            pool_base_account,
            pool_quote_account,
        };
        

            
        Ok(pool_info)
    }
    
    // Helper method to determine the correct token program for a mint
    async fn get_token_program(&self, mint: &Pubkey) -> Result<Pubkey> {
        if let Some(rpc_client) = &self.rpc_client {
            match rpc_client.get_account(mint) {
                Ok(account) => {
                    if account.owner == *TOKEN_2022_PROGRAM {
                        Ok(*TOKEN_2022_PROGRAM)
                    } else {
                        Ok(*TOKEN_PROGRAM)
                    }
                },
                Err(_) => {
                    // Default to TOKEN_PROGRAM if we can't fetch the account
                    Ok(*TOKEN_PROGRAM)
                }
            }
        } else {
            // Default to TOKEN_PROGRAM if no RPC client
            Ok(*TOKEN_PROGRAM)
        }
    }
    
    // Highly optimized build_swap_from_parsed_data
    pub async fn build_swap_from_parsed_data(
        &self,
        trade_info: &crate::engine::transaction_parser::TradeInfoFromToken,
        swap_config: SwapConfig,
    ) -> Result<(Arc<Keypair>, Vec<Instruction>, f64)> {
        let owner = self.keypair.pubkey();
        let mint = Pubkey::from_str(&trade_info.mint)?;
        
        // Get token program for the mint
        let token_program = self.get_token_program(&mint).await?;
        
        // Prepare swap parameters
        let (_token_in, _token_out, discriminator) = match swap_config.swap_direction {
            SwapDirection::Buy => (*SOL_MINT, mint, *BUY_DISCRIMINATOR),
            SwapDirection::Sell => (mint, *SOL_MINT, *SELL_DISCRIMINATOR),
        };
        
        let mut instructions = Vec::with_capacity(3); // Pre-allocate for typical case
        
        // Check and create token accounts if needed
        let token_ata = get_associated_token_address(&owner, &mint);
        let wsol_ata = get_associated_token_address(&owner, &SOL_MINT);
        
        // Check if token account exists and create if needed
        if !WALLET_TOKEN_ACCOUNTS.contains(&token_ata) {
            // Double-check with RPC to see if the account actually exists
            let account_exists = if let Some(rpc_client) = &self.rpc_client {
                match rpc_client.get_account(&token_ata) {
                    Ok(_) => {
                        // Account exists, add to cache
                        WALLET_TOKEN_ACCOUNTS.insert(token_ata);
                        true
                    },
                    Err(_) => false
                }
            } else {
                false // No RPC client, assume account doesn't exist
            };
            
            if !account_exists {
                let logger = Logger::new("[RAYDIUM-ATA-CREATE] => ".yellow().to_string());
                logger.log(format!("Creating token ATA for mint {} at address {}", mint, token_ata));
                
                instructions.push(create_associated_token_account_idempotent(
                    &owner,
                    &owner,
                    &mint,
                    &TOKEN_PROGRAM, // Always use legacy token program for ATA creation
                ));
                
                // Cache the account since we're creating it
                WALLET_TOKEN_ACCOUNTS.insert(token_ata);
            }
        }
        
        // Check if WSOL account exists and create if needed
        if !WALLET_TOKEN_ACCOUNTS.contains(&wsol_ata) {
            // Double-check with RPC to see if the account actually exists
            let account_exists = if let Some(rpc_client) = &self.rpc_client {
                match rpc_client.get_account(&wsol_ata) {
                    Ok(_) => {
                        // Account exists, add to cache
                        WALLET_TOKEN_ACCOUNTS.insert(wsol_ata);
                        true
                    },
                    Err(_) => false
                }
            } else {
                false // No RPC client, assume account doesn't exist
            };
            
            if !account_exists {
                let logger = Logger::new("[RAYDIUM-WSOL-CREATE] => ".yellow().to_string());
                logger.log(format!("Creating WSOL ATA at address {}", wsol_ata));
                
                instructions.push(create_associated_token_account_idempotent(
                    &owner,
                    &owner,
                    &SOL_MINT,
                    &TOKEN_PROGRAM, // WSOL always uses legacy token program
                ));
                
                // Cache the account since we're creating it
                WALLET_TOKEN_ACCOUNTS.insert(wsol_ata);
            }
        }
        
        // Convert amount_in to lamports/token units
        // For Raydium Launchpad:
        // - Buy: amount_in is SOL amount (convert to lamports)
        // - Sell: amount_in is token amount (convert to token units with proper decimals)
        let amount_in = match swap_config.swap_direction {
            SwapDirection::Buy => {
                // For buy: amount_in is SOL amount, convert to lamports
                ui_amount_to_amount(swap_config.amount_in, 9)
            },
            SwapDirection::Sell => {
                // For sell: amount_in is token amount, need to get token decimals
                // First try to get from cache, then fallback to RPC with timeout
                let decimals = 6; // all tokens are 6 decimals
                // Convert token amount to token units (with decimals)
                ui_amount_to_amount(swap_config.amount_in, decimals)
            }
        };
        
        // Calculate the actual quote amount using virtual reserves from trade_info
        let minimum_amount_out: u64 = 1; // to ignore slippage
        
        // Create accounts based on swap direction
        let accounts = match swap_config.swap_direction {
            SwapDirection::Buy => {
                // For buy, we need pool info for accounts
                let pool_info = self.get_or_fetch_pool_info(trade_info, mint).await?;
                create_buy_accounts(
                    pool_info.pool_id,
                    owner,
                    mint,
                    *SOL_MINT,
                    token_ata,
                    wsol_ata,
                    pool_info.pool_base_account,
                    pool_info.pool_quote_account,
                    &token_program,
                )?
            },
            SwapDirection::Sell => {
                // For sell, we need pool info for accounts
                let pool_info = self.get_or_fetch_pool_info(trade_info, mint).await?;
                create_sell_accounts(
                    pool_info.pool_id,
                    owner,
                    mint,
                    *SOL_MINT,
                    token_ata,
                    wsol_ata,
                    pool_info.pool_base_account,
                    pool_info.pool_quote_account,
                    &token_program,
                )?
            }
        };
        
        instructions.push(create_swap_instruction(
            *RAYDIUM_LAUNCHPAD_PROGRAM,
            discriminator,
            amount_in,
            minimum_amount_out,
            accounts,
        ));
        
        // Return the actual price from trade_info (convert from lamports to SOL)
        let price_in_sol = trade_info.price as f64 / 1_000_000_000.0;
        
        Ok((self.keypair.clone(), instructions, price_in_sol))
    }
    

}

/// Get the Raydium pool information for a specific token mint
pub async fn get_pool_info(
    rpc_client: Arc<anchor_client::solana_client::rpc_client::RpcClient>,
    mint: Pubkey,
) -> Result<RaydiumPool> {
    let logger = Logger::new("[RAYDIUM-GET-POOL-INFO] => ".blue().to_string());
    
    // Initialize
    let sol_mint = *SOL_MINT;
    let pump_program = *RAYDIUM_LAUNCHPAD_PROGRAM;
    
    // Use getProgramAccounts with config for better efficiency
    let mut pool_id = Pubkey::default();
    let mut retry_count = 0;
    let max_retries = 2;
    
    // Try to find the pool
    while retry_count < max_retries && pool_id == Pubkey::default() {
        match rpc_client.get_program_accounts_with_config(
            &pump_program,
            RpcProgramAccountsConfig {
                filters: Some(vec![
                    RpcFilterType::DataSize(300),
                    RpcFilterType::Memcmp(Memcmp::new(43, MemcmpEncodedBytes::Base64(base64::encode(mint.to_bytes())))),
                ]),
                account_config: RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    ..Default::default()
                },
                ..Default::default()
            },
        ) {
            Ok(accounts) => {
                for (pubkey, account) in accounts.iter() {
                    if account.data.len() >= 75 {
                        if let Ok(pubkey_from_data) = Pubkey::try_from(&account.data[43..75]) {
                            if pubkey_from_data == mint {
                                pool_id = *pubkey;
                                break;
                            }
                        }
                    }
                }
                
                if pool_id != Pubkey::default() {
                    break;
                } else if retry_count + 1 < max_retries {
                    logger.log("No pools found for the given mint, retrying...".to_string());
                }
            }
            Err(err) => {
                logger.log(format!("Error getting program accounts (attempt {}/{}): {}", 
                                 retry_count + 1, max_retries, err));
            }
        }
        
        retry_count += 1;
        if retry_count < max_retries {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }
    
    if pool_id == Pubkey::default() {
        return Err(anyhow!("Failed to find Raydium pool for mint {}", mint));
    }
    
    // Derive pool vault addresses using PDA
    let base_seeds = [POOL_VAULT_SEED, pool_id.as_ref(), mint.as_ref()];
    let (pool_base_account, _) = Pubkey::find_program_address(&base_seeds, &pump_program);
    
    let quote_seeds = [POOL_VAULT_SEED, pool_id.as_ref(), sol_mint.as_ref()];
    let (pool_quote_account, _) = Pubkey::find_program_address(&quote_seeds, &pump_program);
    
    Ok(RaydiumPool {
        pool_id,
        base_mint: mint,
        quote_mint: sol_mint,
        pool_base_account,
        pool_quote_account,
    })
}

// Optimized account creation with const pubkeys
fn create_buy_accounts(
    pool_id: Pubkey,
    user: Pubkey,
    base_mint: Pubkey,
    quote_mint: Pubkey,
    user_base_token_account: Pubkey,
    wsol_account: Pubkey,
    pool_base_token_account: Pubkey,
    pool_quote_token_account: Pubkey,
    token_program: &Pubkey,
) -> Result<Vec<AccountMeta>> {
    
    Ok(vec![
        AccountMeta::new(user, true),
        AccountMeta::new_readonly(*RAYDIUM_LAUNCHPAD_AUTHORITY, false),
        AccountMeta::new_readonly(*RAYDIUM_GLOBAL_CONFIG, false),
        AccountMeta::new_readonly(*RAYDIUM_PLATFORM_CONFIG, false),
        AccountMeta::new(pool_id, false),
        AccountMeta::new(user_base_token_account, false),
        AccountMeta::new(wsol_account, false),
        AccountMeta::new(pool_base_token_account, false),
        AccountMeta::new(pool_quote_token_account, false),
        AccountMeta::new_readonly(base_mint, false),
        AccountMeta::new_readonly(quote_mint, false),
        AccountMeta::new_readonly(*token_program, false), // Use detected token program for base mint
        AccountMeta::new_readonly(*TOKEN_PROGRAM, false), // Use legacy token program for WSOL
        AccountMeta::new_readonly(*EVENT_AUTHORITY, false),
        AccountMeta::new_readonly(*RAYDIUM_LAUNCHPAD_PROGRAM, false),
        ])
}

// Similar optimization for sell accounts
fn create_sell_accounts(
    pool_id: Pubkey,
    user: Pubkey,
    base_mint: Pubkey,
    quote_mint: Pubkey,
    user_base_token_account: Pubkey,
    wsol_account: Pubkey,
    pool_base_token_account: Pubkey,
    pool_quote_token_account: Pubkey,
    token_program: &Pubkey,
) -> Result<Vec<AccountMeta>> {


    Ok(vec![
        AccountMeta::new(user, true),
        AccountMeta::new_readonly(*RAYDIUM_LAUNCHPAD_AUTHORITY, false),
        AccountMeta::new_readonly(*RAYDIUM_GLOBAL_CONFIG, false),
        AccountMeta::new_readonly(*RAYDIUM_PLATFORM_CONFIG, false),
        AccountMeta::new(pool_id, false),
        AccountMeta::new(user_base_token_account, false),
        AccountMeta::new(wsol_account, false),
        AccountMeta::new(pool_base_token_account, false),
        AccountMeta::new(pool_quote_token_account, false),
        AccountMeta::new_readonly(base_mint, false),
        AccountMeta::new_readonly(quote_mint, false),
        AccountMeta::new_readonly(*token_program, false), // Use detected token program for base mint
        AccountMeta::new_readonly(*TOKEN_PROGRAM, false), // Use legacy token program for WSOL
        AccountMeta::new_readonly(*EVENT_AUTHORITY, false),
        AccountMeta::new_readonly(*RAYDIUM_LAUNCHPAD_PROGRAM, false),
])
}

#[inline]
fn calculate_raydium_sell_amount_out(
    base_amount_in: u64,
    virtual_base_reserve: u64, 
    virtual_quote_reserve: u64,
    real_base_reserve: u64,
    real_quote_reserve: u64
) -> u64 {
    if base_amount_in == 0 || virtual_base_reserve == 0 || virtual_quote_reserve == 0 {
        return 0;
    }
    
    // Raydium Launchpad constant product formula for selling:
    // input_reserve = virtual_base - real_base  
    // output_reserve = virtual_quote + real_quote
    // amount_out = (amount_in * output_reserve) / (input_reserve + amount_in)
    
    let input_reserve = virtual_base_reserve.saturating_sub(real_base_reserve);
    let output_reserve = virtual_quote_reserve.saturating_add(real_quote_reserve);
    
    if input_reserve == 0 || input_reserve > virtual_base_reserve {
        return 0;
    }
    
    let numerator = (base_amount_in as u128).saturating_mul(output_reserve as u128);
    let denominator = (input_reserve as u128).saturating_add(base_amount_in as u128);
    
    if denominator == 0 {
        return 0;
    }
    
    numerator.checked_div(denominator).unwrap_or(0) as u64
}

// Optimized instruction creation
fn create_swap_instruction(
    program_id: Pubkey,
    discriminator: [u8; 8],
    amount_in: u64,
    minimum_amount_out: u64,
    accounts: Vec<AccountMeta>,
) -> Instruction {
    let mut data = Vec::with_capacity(24);
    let share_fee_rate = 0_u64;
    data.extend_from_slice(&discriminator);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&minimum_amount_out.to_le_bytes());
    data.extend_from_slice(&share_fee_rate.to_le_bytes());
    
    Instruction { program_id, accounts, data }
}
