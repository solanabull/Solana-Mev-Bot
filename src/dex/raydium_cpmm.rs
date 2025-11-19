use std::{str::FromStr, sync::Arc, time::Duration};
use anyhow::{anyhow, Result};
use colored::Colorize;
use std::cmp;
use std::env;
use solana_client::nonblocking::rpc_client::RpcClient;
use anchor_client::solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    system_program,
};
use spl_associated_token_account::{
    get_associated_token_address, instruction::create_associated_token_account_idempotent,
};
use tokio::time::{Instant, sleep};
use crate::common::pool::get_program_acccounts_with_filter_async;
use crate::{
    common::{config::SwapConfig, logger::Logger},
    core::token,
};

const RAYDIUM_CPMM_PROGRAM: &str = "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C";
const RAYDIUM_CPMM_POOL_SIZE: u64 = 637;
const RAYDIUM_CPMM_TOKEN_MINT_0_POSITION: u64 = 73;
const RAYDIUM_CPMM_TOKEN_MINT_1_POSITION: u64 = 105;

#[derive(Debug, Clone)]
pub struct RaydiumCPMM {
    // Account Discriminator (8 bytes) - not shown in JSON but present in account data
    pub amm_config: Pubkey,               // 32 bytes
    pub pool_creator: Pubkey,             // 32 bytes
    pub token0_vault: Pubkey,             // 32 bytes
    pub token1_vault: Pubkey,             // 32 bytes
    pub lp_mint: Pubkey,                  // 32 bytes
    pub token0_mint: Pubkey,              // 32 bytes
    pub token1_mint: Pubkey,              // 32 bytes
    pub token0_program: Pubkey,           // 32 bytes
    pub token1_program: Pubkey,           // 32 bytes
    pub observation_key: Pubkey,          // 32 bytes
    pub auth_bump: u8,                    // 1 byte
    pub status: u8,                       // 1 byte
    pub lp_mint_decimals: u8,             // 1 byte
    pub mint0_decimals: u8,               // 1 byte
    pub mint1_decimals: u8,               // 1 byte
    pub lp_supply: u64,                   // 8 bytes
    pub protocol_fees_token0: u64,        // 8 bytes
    pub protocol_fees_token1: u64,        // 8 bytes
    pub fund_fees_token0: u64,            // 8 bytes
    pub fund_fees_token1: u64,            // 8 bytes
    pub open_time: u64,                   // 8 bytes
    pub padding: [u64; 32],               // 256 bytes (32 * 8)
}


impl RaydiumCPMM {
    //new liquidity pool based on the tokn mint
    async fn get_pool_by_mint (mint1: &str, mint2: &str) -> Result<RaydiumCPMM> {
        let rpc_client = RpcClient::new(env::var("RPC_HTTP").unwrap());
        let mint1_pubkey = Pubkey::from_str(mint1)?;
        let mint2_pubkey = Pubkey::from_str(mint2)?;
        let pools = get_program_acccounts_with_filter_async(
            &rpc_client,
            &RAYDIUM_CPMM_PROGRAM.parse().unwrap(),
            RAYDIUM_CPMM_POOL_SIZE,
            &RAYDIUM_CPMM_TOKEN_MINT_0_POSITION.try_into().unwrap(),
            &RAYDIUM_CPMM_TOKEN_MINT_1_POSITION.try_into().unwrap(),
            &mint1_pubkey,
            &mint2_pubkey
            ).await?;
            
        if pools.is_empty() {
            return Err(anyhow!("No Raydium CPMM pool found for the given mints"));
        }
        
        let (pubkey, account) = &pools[0];
        let pool_id = *pubkey;
        let data = &account.data;
        // Account discriminator (8 bytes)
        let _discriminator = &data[0..8];

        // Pubkey fields (10 total)
        let amm_config = Pubkey::try_from(&data[8..40]).unwrap();
        let pool_creator = Pubkey::try_from(&data[40..72]).unwrap();
        let token0_vault = Pubkey::try_from(&data[72..104]).unwrap();
        let token1_vault = Pubkey::try_from(&data[104..136]).unwrap();
        let lp_mint = Pubkey::try_from(&data[136..168]).unwrap();
        let token0_mint = Pubkey::try_from(&data[168..200]).unwrap();
        let token1_mint = Pubkey::try_from(&data[200..232]).unwrap();
        let token0_program = Pubkey::try_from(&data[232..264]).unwrap();
        let token1_program = Pubkey::try_from(&data[264..296]).unwrap();
        let observation_key = Pubkey::try_from(&data[296..328]).unwrap();

        // u8 fields (5 total)
        let auth_bump = data[328];
        let status = data[329];
        let lp_mint_decimals = data[330];
        let mint0_decimals = data[331];
        let mint1_decimals = data[332];

        // u64 fields (6 total)
        let lp_supply = u64::from_le_bytes(data[333..341].try_into().unwrap());
        let protocol_fees_token0 = u64::from_le_bytes(data[341..349].try_into().unwrap());
        let protocol_fees_token1 = u64::from_le_bytes(data[349..357].try_into().unwrap());
        let fund_fees_token0 = u64::from_le_bytes(data[357..365].try_into().unwrap());
        let fund_fees_token1 = u64::from_le_bytes(data[365..373].try_into().unwrap());
        let open_time = u64::from_le_bytes(data[373..381].try_into().unwrap());

        // Padding (32 u64 values)
        let mut padding = [0u64; 32];
        for i in 0..32 {
            let offset = 381 + i * 8;
            padding[i] = u64::from_le_bytes(data[offset..offset+8].try_into().unwrap());
        }

        Ok(RaydiumCPMM {
            amm_config,
            pool_creator,
            token0_vault,
            token1_vault,
            lp_mint,
            token0_mint,
            token1_mint,
            token0_program,
            token1_program,
            observation_key,
            auth_bump,
            status,
            lp_mint_decimals,
            mint0_decimals,
            mint1_decimals,
            lp_supply,
            protocol_fees_token0,
            protocol_fees_token1,
            fund_fees_token0,
            fund_fees_token1,
            open_time,
            padding,
        })
    }
}
