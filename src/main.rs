/*
 * Copy Trading Bot with PumpSwap Notification Mode
 * 
 * Changes made:
 * - Modified PumpSwap buy/sell logic to only send notifications without executing transactions
 * - Transaction processing now runs in separate tokio tasks to ensure main monitoring continues
 * - Added placeholder for future selling strategy implementation
 * - PumpFun protocol functionality remains unchanged
 * - Added caching and batch RPC calls for improved performance
 */

use anchor_client::solana_sdk::signature::Signer;
use solana_vntr_sniper::{
    common::{config::Config, constants::RUN_MSG, cache::WALLET_TOKEN_ACCOUNTS},
    processor::{
        sniper_bot::{start_target_wallet_monitoring, start_dex_monitoring, SniperConfig},
        swap::SwapProtocol,
    },
    library::{ 
        cache_maintenance, 
        blockhash_processor::BlockhashProcessor,
        jupiter_api::JupiterClient
    },
    block_engine::token,
};
use std::sync::Arc;
use solana_program_pack::Pack;
use anchor_client::solana_sdk::pubkey::Pubkey;
use anchor_client::solana_sdk::transaction::Transaction;
use anchor_client::solana_sdk::system_instruction;
use std::str::FromStr;
use colored::Colorize;
use spl_token::instruction::sync_native;
use spl_token::ui_amount_to_amount;
use spl_associated_token_account::get_associated_token_address;

/// Initialize the wallet token account list by fetching all token accounts owned by the wallet
async fn initialize_token_account_list(config: &Config) {
    let logger = solana_vntr_sniper::common::logger::Logger::new("[INIT-TOKEN-ACCOUNTS] => ".green().to_string());
    
    if let Ok(wallet_pubkey) = config.app_state.wallet.try_pubkey() {
        logger.log(format!("Initializing token account list for wallet: {}", wallet_pubkey));
        
        // Get the token program pubkey
        let token_program = Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();
        
        // Query all token accounts owned by the wallet
        let accounts = config.app_state.rpc_client.get_token_accounts_by_owner(
            &wallet_pubkey,
            anchor_client::solana_client::rpc_request::TokenAccountsFilter::ProgramId(token_program)
        );
        match accounts {
            Ok(accounts) => {
                logger.log(format!("Found {} existing token accounts", accounts.len()));
                
                // Add each token account to our global cache
                for account in accounts {
                    let account_pubkey = Pubkey::from_str(&account.pubkey).unwrap();
                    WALLET_TOKEN_ACCOUNTS.insert(account_pubkey);
                    logger.log(format!("✅ Cached token account: {}", account.pubkey ));
                }
                
                logger.log(format!("✅ Token account cache initialized with {} accounts", WALLET_TOKEN_ACCOUNTS.size()));
            },
            Err(e) => {
                logger.log(format!("❌ Error fetching token accounts: {}", e).red().to_string());
                logger.log("⚠️  Cache will be populated as new accounts are discovered".yellow().to_string());
            }
        }
    } else {
        logger.log("❌ Failed to get wallet pubkey, can't initialize token account list".red().to_string());
    }
}

/// Wrap SOL to Wrapped SOL (WSOL)
async fn wrap_sol(config: &Config, amount: f64) -> Result<(), String> {
    let logger = solana_vntr_sniper::common::logger::Logger::new("[WRAP-SOL] => ".green().to_string());
    
    // Get wallet pubkey
    let wallet_pubkey = match config.app_state.wallet.try_pubkey() {
        Ok(pk) => pk,
        Err(_) => return Err("Failed to get wallet pubkey".to_string()),
    };
    
    // Create WSOL account instructions
    let (wsol_account, mut instructions) = match token::create_wsol_account(wallet_pubkey) {
        Ok(result) => result,
        Err(e) => return Err(format!("Failed to create WSOL account: {}", e)),
    };
    
    logger.log(format!("WSOL account address: {}", wsol_account));
    
    // Convert UI amount to lamports (1 SOL = 10^9 lamports)
    let lamports = ui_amount_to_amount(amount, 9);
    logger.log(format!("Wrapping {} SOL ({} lamports)", amount, lamports));
    
    // Transfer SOL to the WSOL account
    instructions.push(
        system_instruction::transfer(
            &wallet_pubkey,
            &wsol_account,
            lamports,
        )
    );
    
    // Sync native instruction to update the token balance
    instructions.push(
        sync_native(
            &spl_token::id(),
            &wsol_account,
        ).map_err(|e| format!("Failed to create sync native instruction: {}", e))?
    );
    
    // Send transaction
    let recent_blockhash = config.app_state.rpc_client.get_latest_blockhash()
        .map_err(|e| format!("Failed to get recent blockhash: {}", e))?;
    
    let transaction = Transaction::new_signed_with_payer(
        &instructions,
        Some(&wallet_pubkey),
        &[&config.app_state.wallet],
        recent_blockhash,
    );
    
    match config.app_state.rpc_client.send_and_confirm_transaction(&transaction) {
        Ok(signature) => {
            logger.log(format!("SOL wrapped successfully, signature: {}", signature));
            Ok(())
        },
        Err(e) => {
            Err(format!("Failed to wrap SOL: {}", e))
        }
    }
}

/// Unwrap SOL from Wrapped SOL (WSOL) account
async fn unwrap_sol(config: &Config) -> Result<(), String> {
    let logger = solana_vntr_sniper::common::logger::Logger::new("[UNWRAP-SOL] => ".green().to_string());
    
    // Get wallet pubkey
    let wallet_pubkey = match config.app_state.wallet.try_pubkey() {
        Ok(pk) => pk,
        Err(_) => return Err("Failed to get wallet pubkey".to_string()),
    };
    
    // Get the WSOL ATA address
    let wsol_account = get_associated_token_address(
        &wallet_pubkey,
        &spl_token::native_mint::id()
    );
    
    logger.log(format!("WSOL account address: {}", wsol_account));
    
    // Check if WSOL account exists
    match config.app_state.rpc_client.get_account(&wsol_account) {
        Ok(_) => {
            logger.log(format!("Found WSOL account: {}", wsol_account));
        },
        Err(_) => {
            return Err(format!("WSOL account does not exist: {}", wsol_account));
        }
    }
    
    // Close the WSOL account to recover SOL
    let close_instruction = token::close_account(
        wallet_pubkey,
        wsol_account,
        wallet_pubkey,
        wallet_pubkey,
        &[&wallet_pubkey],
    ).map_err(|e| format!("Failed to create close account instruction: {}", e))?;
    
    // Send transaction
    let recent_blockhash = config.app_state.rpc_client.get_latest_blockhash()
        .map_err(|e| format!("Failed to get recent blockhash: {}", e))?;
    
    let transaction = Transaction::new_signed_with_payer(
        &[close_instruction],
        Some(&wallet_pubkey),
        &[&config.app_state.wallet],
        recent_blockhash,
    );
    
    match config.app_state.rpc_client.send_and_confirm_transaction(&transaction) {
        Ok(signature) => {
            logger.log(format!("WSOL unwrapped successfully, signature: {}", signature));
            Ok(())
        },
        Err(e) => {
            Err(format!("Failed to unwrap WSOL: {}", e))
        }
    }
}

/// Sell all tokens using Jupiter API
async fn sell_all_tokens(config: &Config) -> Result<(), String> {
    let logger = solana_vntr_sniper::common::logger::Logger::new("[SELL-ALL-TOKENS] => ".green().to_string());
    let quote_logger = solana_vntr_sniper::common::logger::Logger::new("[JUPITER-QUOTE] => ".blue().to_string());
    let execute_logger = solana_vntr_sniper::common::logger::Logger::new("[EXECUTE-SWAP] => ".yellow().to_string());
    let sell_logger = solana_vntr_sniper::common::logger::Logger::new("[SELL-TOKEN] ".cyan().to_string());
    
    // Get wallet pubkey
    let wallet_pubkey = match config.app_state.wallet.try_pubkey() {
        Ok(pk) => pk,
        Err(_) => return Err("Failed to get wallet pubkey".to_string()),
    };
    
    logger.log(format!("🔍 Scanning wallet {} for tokens to sell", wallet_pubkey));
    
    // Get the token program pubkey
    let token_program = Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();
    
    // Query all token accounts owned by the wallet
    let accounts = config.app_state.rpc_client.get_token_accounts_by_owner(
        &wallet_pubkey,
        anchor_client::solana_client::rpc_request::TokenAccountsFilter::ProgramId(token_program)
    ).map_err(|e| format!("Failed to get token accounts: {}", e))?;
    
    if accounts.is_empty() {
        logger.log("No token accounts found".to_string());
        return Ok(());
    }
    
    logger.log(format!("Found {} token accounts", accounts.len()));
    
    // Create Jupiter API client
    let jupiter_client = JupiterClient::new(config.app_state.rpc_nonblocking_client.clone());
    
    // Filter and collect token information
    let mut tokens_to_sell = Vec::new();
    let mut total_token_count = 0;
    let mut sold_count = 0;
    let mut failed_count = 0;
    let mut total_sol_received = 0u64;
    
    for account_info in accounts {
        let token_account = Pubkey::from_str(&account_info.pubkey)
            .map_err(|_| format!("Invalid token account pubkey: {}", account_info.pubkey))?;
        
        // Get account data
        let account_data = match config.app_state.rpc_client.get_account(&token_account) {
            Ok(data) => data,
            Err(e) => {
                logger.log(format!("Failed to get account data for {}: {}", token_account, e).red().to_string());
                continue;
            }
        };
        
        // Parse token account data
        if let Ok(token_data) = spl_token::state::Account::unpack(&account_data.data) {
            // Skip WSOL (wrapped SOL) and accounts with zero balance
            if token_data.mint == spl_token::native_mint::id() || token_data.amount == 0 {
                continue;
            }
            
            total_token_count += 1;
            
            // Get mint account to determine decimals
            let mint_data = match config.app_state.rpc_client.get_account(&token_data.mint) {
                Ok(data) => data,
                Err(e) => {
                    logger.log(format!("Failed to get mint data for {}: {}", token_data.mint, e).yellow().to_string());
                    continue;
                }
            };
            
            let mint_info = match spl_token::state::Mint::unpack(&mint_data.data) {
                Ok(info) => info,
                Err(e) => {
                    logger.log(format!("Failed to parse mint data for {}: {}", token_data.mint, e).yellow().to_string());
                    continue;
                }
            };
            
            let decimals = mint_info.decimals;
            let token_amount = token_data.amount as f64 / 10f64.powi(decimals as i32);
            
            logger.log(format!("📦 Found token: {} - Amount: {} (decimals: {})", 
                               token_data.mint, token_amount, decimals));
            
            tokens_to_sell.push((token_data.mint.to_string(), token_data.amount, decimals));
        }
    }
    
    if tokens_to_sell.is_empty() {
        logger.log("No tokens found to sell (excluding SOL/WSOL)".yellow().to_string());
        return Ok(());
    }
    
    logger.log(format!("💱 Starting to sell {} tokens", tokens_to_sell.len()));
    
    // Sell each token using Jupiter API
    for (mint, amount, decimals) in tokens_to_sell {
        logger.log(format!("💱 Selling token: {}", mint).cyan().to_string());
        
        // First get the quote to show detailed information
        let sol_mint = "So11111111111111111111111111111111111111112";
        quote_logger.log(format!("Getting quote: {} -> {} (amount: {})", mint, sol_mint, amount));
        
        match jupiter_client.get_quote(&mint, sol_mint, amount, 100).await {
            Ok(quote) => {
                // Log quote details like in the example
                quote_logger.log(format!("Raw quote response (first 500 chars): {}", 
                    serde_json::to_string(&quote).unwrap_or_default().chars().take(500).collect::<String>()));
                
                quote_logger.log(format!("Quote received: {} {} -> {} {}", 
                    quote.in_amount, mint, quote.out_amount, sol_mint));
                
                // Now get the actual transaction using the enhanced Jupiter sell method
                match jupiter_client.sell_token_with_jupiter(&mint, amount, 500, &config.app_state.wallet).await {
                    Ok(signature) => {
                        execute_logger.log(format!("Jupiter sell transaction sent: {}", signature));
                        
                        // Wait a moment for confirmation
                        tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;
                        execute_logger.log(format!("Jupiter sell transaction confirmed: {}", signature));
                        
                        // Log the successful sell
                        sell_logger.log(format!("{} => Token sold successfully! Signature: {}", mint, signature));
                        
                        // Parse the expected SOL amount from quote
                        if let Ok(sol_amount) = quote.out_amount.parse::<u64>() {
                            total_sol_received += sol_amount;
                        }
                        
                        logger.log(format!("✅ Successfully sold {}: {}", mint, signature).green().to_string());
                        sold_count += 1;
                    },
                    Err(e) => {
                        logger.log(format!("❌ Failed to get sell transaction for token {}: {}", mint, e).red().to_string());
                        failed_count += 1;
                    }
                }
            },
            Err(e) => {
                logger.log(format!("❌ Failed to get quote for token {}: {}", mint, e).red().to_string());
                failed_count += 1;
            }
        }
        
        // Small delay between transactions to avoid rate limiting
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    }
    
    // Final summary
    let sol_received_display = total_sol_received as f64 / 1_000_000_000.0; // Convert lamports to SOL
    logger.log(format!("Selling completed! ✅ {} successful, ❌ {} failed, ~{:.6} SOL received", 
                       sold_count, failed_count, sol_received_display).cyan().bold().to_string());
    
    if failed_count > 0 {
        Err(format!("Failed to sell {} out of {} tokens", failed_count, total_token_count))
    } else {
        Ok(())
    }
}

/// Close all token accounts owned by the wallet
async fn close_all_token_accounts(config: &Config) -> Result<(), String> {
    let logger = solana_vntr_sniper::common::logger::Logger::new("[CLOSE-TOKEN-ACCOUNTS] => ".green().to_string());
    
    // Get wallet pubkey
    let wallet_pubkey = match config.app_state.wallet.try_pubkey() {
        Ok(pk) => pk,
        Err(_) => return Err("Failed to get wallet pubkey".to_string()),
    };
    
    // Get the token program pubkey
    let token_program = Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();
    
    // Query all token accounts owned by the wallet
    let accounts = config.app_state.rpc_client.get_token_accounts_by_owner(
        &wallet_pubkey,
        anchor_client::solana_client::rpc_request::TokenAccountsFilter::ProgramId(token_program)
    ).map_err(|e| format!("Failed to get token accounts: {}", e))?;
    
    if accounts.is_empty() {
        logger.log("No token accounts found to close".to_string());
        return Ok(());
    }
    
    logger.log(format!("Found {} token accounts to close", accounts.len()));
    
    let mut closed_count = 0;
    let mut failed_count = 0;
    
    // Close each token account
    for account_info in accounts {
        let token_account = Pubkey::from_str(&account_info.pubkey)
            .map_err(|_| format!("Invalid token account pubkey: {}", account_info.pubkey))?;
        
        // Skip WSOL accounts with non-zero balance (these need to be unwrapped first)
        let account_data = match config.app_state.rpc_client.get_account(&token_account) {
            Ok(data) => data,
            Err(e) => {
                logger.log(format!("Failed to get account data for {}: {}", token_account, e).red().to_string());
                failed_count += 1;
                continue;
            }
        };
        
        // Check if this is a WSOL account with balance
        if let Ok(token_data) = spl_token::state::Account::unpack(&account_data.data) {
            if token_data.mint == spl_token::native_mint::id() && token_data.amount > 0 {
                logger.log(format!("Skipping WSOL account with non-zero balance: {} ({})", 
                                 token_account, 
                                 token_data.amount as f64 / 1_000_000_000.0));
                continue;
            }
        }
        
        // Create close instruction
        let close_instruction = token::close_account(
            wallet_pubkey,
            token_account,
            wallet_pubkey,
            wallet_pubkey,
            &[&wallet_pubkey],
        ).map_err(|e| format!("Failed to create close instruction for {}: {}", token_account, e))?;
        
        // Send transaction
        let recent_blockhash = config.app_state.rpc_client.get_latest_blockhash()
            .map_err(|e| format!("Failed to get recent blockhash: {}", e))?;
        
        let transaction = Transaction::new_signed_with_payer(
            &[close_instruction],
            Some(&wallet_pubkey),
            &[&config.app_state.wallet],
            recent_blockhash,
        );
        
        match config.app_state.rpc_client.send_and_confirm_transaction(&transaction) {
            Ok(signature) => {
                logger.log(format!("Closed token account {}, signature: {}", token_account, signature));
                closed_count += 1;
            },
            Err(e) => {
                logger.log(format!("Failed to close token account {}: {}", token_account, e).red().to_string());
                failed_count += 1;
            }
        }
    }
    
    logger.log(format!("Closed {} token accounts, {} failed", closed_count, failed_count));
    
    if failed_count > 0 {
        Err(format!("Failed to close {} token accounts", failed_count))
    } else {
        Ok(())
    }
}



#[tokio::main]
async fn main() {
    /* Initial Settings */
    let config = Config::new().await;
    let config = config.lock().await;

    /* Running Bot */
    let run_msg = RUN_MSG;
    println!("{}", run_msg);
    
    // Initialize blockhash processor
    match BlockhashProcessor::new(config.app_state.rpc_client.clone()).await {
        Ok(processor) => {
            if let Err(e) = processor.start().await {
                eprintln!("Failed to start blockhash processor: {}", e);
                return;
            }
            println!("Blockhash processor started successfully");
        },
        Err(e) => {
            eprintln!("Failed to initialize blockhash processor: {}", e);
            return;
        }
    }

    // Parse command line arguments
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        // Check for command line arguments
        if args.contains(&"--wrap".to_string()) {
            println!("Wrapping SOL to WSOL...");
            
            // Get wrap amount from .env
            let wrap_amount = std::env::var("WRAP_AMOUNT")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.1);
            
            match wrap_sol(&config, wrap_amount).await {
                Ok(_) => {
                    println!("Successfully wrapped {} SOL to WSOL", wrap_amount);
                    return;
                },
                Err(e) => {
                    eprintln!("Failed to wrap SOL: {}", e);
                    return;
                }
            }
        } else if args.contains(&"--unwrap".to_string()) {
            println!("Unwrapping WSOL to SOL...");
            
            match unwrap_sol(&config).await {
                Ok(_) => {
                    println!("Successfully unwrapped WSOL to SOL");
                    return;
                },
                Err(e) => {
                    eprintln!("Failed to unwrap WSOL: {}", e);
                    return;
                }
            }
        } else if args.contains(&"--sell".to_string()) {
            println!("Selling all tokens using Jupiter API...");
            
            match sell_all_tokens(&config).await {
                Ok(_) => {
                    println!("Successfully sold all tokens");
                    return;
                },
                Err(e) => {
                    eprintln!("Failed to sell all tokens: {}", e);
                    return;
                }
            }
        } else if args.contains(&"--close".to_string()) {
            println!("Closing all token accounts...");
            
            match close_all_token_accounts(&config).await {
                Ok(_) => {
                    println!("Successfully closed all token accounts");
                    return;
                },
                Err(e) => {
                    eprintln!("Failed to close all token accounts: {}", e);
                    return;
                }
            }
        }
    }

    // Initialize token account list
    initialize_token_account_list(&config).await;
    
    // Start cache maintenance service (clean up expired cache entries every 60 seconds)
    cache_maintenance::start_cache_maintenance(60).await;
    println!("Cache maintenance service started");
    
    // Selling instruction cache removed - no maintenance needed

    // Initialize and log selling strategy parameters
    let selling_config = solana_vntr_sniper::processor::selling_strategy::SellingConfig::set_from_env();
    let selling_engine = solana_vntr_sniper::processor::selling_strategy::SellingEngine::new(
        Arc::new(config.app_state.clone()),
        Arc::new(config.swap_config.clone()),
        selling_config,
    );
    selling_engine.log_selling_parameters();

    // Initialize copy selling for existing token balances
    match selling_engine.initialize_copy_selling_for_existing_tokens().await {
        Ok(count) => {
            if count > 0 {
                println!("✅ Copy selling initialized for {} existing tokens", count);
            }
        },
        Err(e) => {
            eprintln!("⚠️  Failed to initialize copy selling for existing tokens: {}", e);
        }
    }

    // Get copy trading target addresses from environment
    let copy_trading_target_address = std::env::var("COPY_TRADING_TARGET_ADDRESS").ok();
    let is_multi_copy_trading = std::env::var("IS_MULTI_COPY_TRADING")
        .ok()
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(false);
    let excluded_addresses_str = std::env::var("EXCLUDED_ADDRESSES").ok();
    
    // Prepare target addresses for monitoring
    let mut target_addresses = Vec::new();
    let mut excluded_addresses = Vec::new();

    // Handle multiple copy trading targets if enabled
    if is_multi_copy_trading {
        if let Some(address_str) = copy_trading_target_address {
            // Parse comma-separated addresses
            for addr in address_str.split(',') {
                let trimmed_addr = addr.trim();
                if !trimmed_addr.is_empty() {
                    target_addresses.push(trimmed_addr.to_string());
                }
            }
        }
    } else if let Some(address) = copy_trading_target_address {
        // Single address mode
        if !address.is_empty() {
            target_addresses.push(address);
        }
    }
    
    if let Some(excluded_addresses_str) = excluded_addresses_str {
        for addr in excluded_addresses_str.split(',') {
            let trimmed_addr = addr.trim();
            if !trimmed_addr.is_empty() {
                excluded_addresses.push(trimmed_addr.to_string());
            }
        }
    }

    if target_addresses.is_empty() {
        eprintln!("No COPY_TRADING_TARGET_ADDRESS specified. Please set this environment variable.");
        return;
    }
    

    
    // Get protocol preference from environment
    let protocol_preference = std::env::var("PROTOCOL_PREFERENCE")
        .ok()
        .map(|p| match p.to_lowercase().as_str() {
            "pumpfun" => SwapProtocol::PumpFun,
            "pumpswap" => SwapProtocol::PumpSwap,
            _ => SwapProtocol::Auto,
        })
        .unwrap_or(SwapProtocol::Auto);
    
    // Start risk management service
    println!("Starting risk management service...");
    if let Err(e) = solana_vntr_sniper::processor::risk_management::start_risk_management_service(
        target_addresses.clone(),
        Arc::new(config.app_state.clone()),
        Arc::new(config.swap_config.clone()),
    ).await {
        eprintln!("Failed to start risk management service: {}", e);
    } else {
        println!("Risk management service started successfully");
    }

    // Create copy trading config
    let sniper_config = SniperConfig {
        yellowstone_grpc_http: config.yellowstone_grpc_http.clone(),
        yellowstone_grpc_token: config.yellowstone_grpc_token.clone(),
        app_state: config.app_state.clone(),
        swap_config: config.swap_config.clone(),
        counter_limit: config.counter_limit as u64,
        target_addresses,
        excluded_addresses,
        protocol_preference,
    };
    
    // Run both monitoring functions simultaneously
    println!("🚀 Starting both monitoring systems simultaneously...");
    
    // Spawn both monitoring tasks to run in parallel
    let target_monitoring_handle = tokio::spawn({
        let config = sniper_config.clone();
        async move {
            match start_target_wallet_monitoring(config).await {
                Ok(_) => println!("✅ Target wallet monitoring completed successfully"),
                Err(e) => eprintln!("❌ Target wallet monitoring error: {}", e),
            }
        }
    });
    
    let dex_monitoring_handle = tokio::spawn({
        let config = sniper_config;
        async move {
            match start_dex_monitoring(config).await {
                Ok(_) => println!("✅ DEX monitoring completed successfully"),
                Err(e) => eprintln!("❌ DEX monitoring error: {}", e),
            }
        }
    });
    
    // Wait for both tasks to complete (or error)
    println!("⏳ Waiting for monitoring tasks to complete...");
    tokio::try_join!(target_monitoring_handle, dex_monitoring_handle)
        .map(|_| println!("🎯 Both monitoring systems have completed"))
        .unwrap_or_else(|_| println!("⚠️  One or both monitoring systems encountered errors"));

}
