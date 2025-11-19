use solana_client::nonblocking::rpc_client::RpcClient;
use std::{str::FromStr, sync::Arc, time::Duration};
use anyhow::{anyhow, Result};
use colored::Colorize;
use std::cmp;
use std::env;
use crate::common::pool::get_program_acccounts_with_filter;
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
use spl_token_client::token::TokenError;
use tokio::time::{Instant, sleep};

use crate::{
    common::{config::SwapConfig, logger::Logger},
    core::token,
};

pub const RAYDIUM_CLMM_PROGRAM: &str = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK";
pub const RAYDIUM_CLMM_POOL_SIZE: u64 = 1544;
pub const RAYDIUM_CLMM_TOKEN_MINT_0_POSITION: u64 = 73;
pub const RAYDIUM_CLMM_TOKEN_MINT_1_POSITION: u64 = 105;

//token_mint0 = 73
//token_mint1 = 105 


#[derive(Debug, Clone)]
pub struct RaydiumCLMM {
    // Account Discriminator (8 bytes)
    pub bump: u8,                          // 1 byte
    pub amm_config: Pubkey,                // 32 bytes
    pub owner: Pubkey,                     // 32 bytes
    pub token_mint0: Pubkey,               // 32 bytes
    pub token_mint1: Pubkey,               // 32 bytes
    pub token_vault0: Pubkey,              // 32 bytes
    pub token_vault1: Pubkey,              // 32 bytes
    pub observation_key: Pubkey,           // 32 bytes
    pub mint_decimals0: u8,                // 1 byte
    pub mint_decimals1: u8,                // 1 byte
    pub tick_spacing: u16,                 // 2 bytes
    pub liquidity: u128,                   // 16 bytes
    pub sqrt_price_x64: u128,              // 16 bytes
    pub tick_current: i32,                 // 4 bytes
    pub padding3: u16,                     // 2 bytes
    pub padding4: u16,                     // 2 bytes
    pub fee_growth_global0_x64: u128,      // 16 bytes
    pub fee_growth_global1_x64: u128,      // 16 bytes
    pub protocol_fees_token0: u64,         // 8 bytes
    pub protocol_fees_token1: u64,         // 8 bytes
    pub swap_in_amount_token0: u128,       // 16 bytes
    pub swap_out_amount_token1: u128,      // 16 bytes
    pub swap_in_amount_token1: u128,       // 16 bytes
    pub swap_out_amount_token0: u128,      // 16 bytes
    pub status: u8,                        // 1 byte
    pub padding: [u8; 7],                  // 7 bytes
    pub reward_infos: [RewardInfo; 3],     // 3 * 104 = 312 bytes
    pub tick_array_bitmap: [u64; 16],      // 16 * 8 = 128 bytes
    pub total_fees_token0: u64,            // 8 bytes
    pub total_fees_claimed_token0: u64,    // 8 bytes
    pub total_fees_token1: u64,            // 8 bytes
    pub total_fees_claimed_token1: u64,    // 8 bytes
    pub fund_fees_token0: u64,             // 8 bytes
    pub fund_fees_token1: u64,             // 8 bytes
    pub open_time: u64,                    // 8 bytes
    pub recent_epoch: u64,                 // 8 bytes
    pub padding1: [u64; 24],               // 192 bytes
    pub padding2: [u64; 32],               // 256 bytes
}

#[derive(Debug, Copy, Clone)]
pub struct RewardInfo {
    pub mint: Pubkey,                     // 32 bytes
    pub vault: Pubkey,                    // 32 bytes
    pub authority: Pubkey,                // 32 bytes
    pub emissions_per_second_x64: u128,   // 16 bytes
    pub growth_global_x64: u128,          // 16 bytes
}

impl Default for RewardInfo {
    fn default() -> Self {
        Self {
            mint: Pubkey::default(),
            vault: Pubkey::default(),
            authority: Pubkey::default(),
            emissions_per_second_x64: 0,
            growth_global_x64: 0,
        }
    }
}

impl RaydiumCLMM {
    async fn get_pool_by_mint (mint1: &str, mint2: &str) -> Result<RaydiumCLMM> {
        let rpc_client = RpcClient::new(env::var("RPC_HTTP").unwrap());
        let mint1_pubkey = Pubkey::from_str(mint1)?;
        let mint2_pubkey = Pubkey::from_str(mint2)?;
        let pools = crate::common::pool::get_program_acccounts_with_filter_async(
            &rpc_client,
            &RAYDIUM_CLMM_PROGRAM.parse().unwrap(),
            RAYDIUM_CLMM_POOL_SIZE,
            &RAYDIUM_CLMM_TOKEN_MINT_0_POSITION.try_into().unwrap(),
            &RAYDIUM_CLMM_TOKEN_MINT_1_POSITION.try_into().unwrap(),
            &mint1_pubkey,
            &mint2_pubkey
            ).await?;  

        if pools.is_empty() {
            return Err(anyhow!("No Raydium CLMM pool found for the given mints"));
        }

        let (pubkey, account) = &pools[0];
        let pool_id = *pubkey;
        let data = &account.data;

        // Account discriminator (8 bytes)
        let _discriminator = &data[0..8];

        // Initial fields
        let bump = data[8];
        let amm_config = Pubkey::try_from(&data[9..41]).unwrap();
        let owner = Pubkey::try_from(&data[41..73]).unwrap();
        let token_mint0 = Pubkey::try_from(&data[73..105]).unwrap();
        let token_mint1 = Pubkey::try_from(&data[105..137]).unwrap();
        let token_vault0 = Pubkey::try_from(&data[137..169]).unwrap();
        let token_vault1 = Pubkey::try_from(&data[169..201]).unwrap();
        let observation_key = Pubkey::try_from(&data[201..233]).unwrap();
        let mint_decimals0 = data[233];
        let mint_decimals1 = data[234];
        let tick_spacing = u16::from_le_bytes(data[235..237].try_into().unwrap());
        let liquidity = u128::from_le_bytes(data[237..253].try_into().unwrap());
        let sqrt_price_x64 = u128::from_le_bytes(data[253..269].try_into().unwrap());
        let tick_current = i32::from_le_bytes(data[269..273].try_into().unwrap());
        let padding3 = u16::from_le_bytes(data[273..275].try_into().unwrap());
        let padding4 = u16::from_le_bytes(data[275..277].try_into().unwrap());
        let fee_growth_global0_x64 = u128::from_le_bytes(data[277..293].try_into().unwrap());
        let fee_growth_global1_x64 = u128::from_le_bytes(data[293..309].try_into().unwrap());
        let protocol_fees_token0 = u64::from_le_bytes(data[309..317].try_into().unwrap());
        let protocol_fees_token1 = u64::from_le_bytes(data[317..325].try_into().unwrap());
        let swap_in_amount_token0 = u128::from_le_bytes(data[325..341].try_into().unwrap());
        let swap_out_amount_token1 = u128::from_le_bytes(data[341..357].try_into().unwrap());
        let swap_in_amount_token1 = u128::from_le_bytes(data[357..373].try_into().unwrap());
        let swap_out_amount_token0 = u128::from_le_bytes(data[373..389].try_into().unwrap());
        let status = data[389];
        let padding = [data[390], data[391], data[392], data[393], data[394], data[395], data[396]];

        // RewardInfos (3 items)
        let mut reward_infos = [RewardInfo::default(); 3];
        for i in 0..3 {
            let offset = 397 + i * 104;
            reward_infos[i] = RewardInfo {
                mint: Pubkey::try_from(&data[offset..offset+32]).unwrap(),
                vault: Pubkey::try_from(&data[offset+32..offset+64]).unwrap(),
                authority: Pubkey::try_from(&data[offset+64..offset+96]).unwrap(),
                emissions_per_second_x64: u128::from_le_bytes(data[offset+96..offset+112].try_into().unwrap()),
                growth_global_x64: u128::from_le_bytes(data[offset+112..offset+128].try_into().unwrap()),
            };
        }

        // Tick array bitmap (16 u64 values)
        let mut tick_array_bitmap = [0u64; 16];
        for i in 0..16 {
            let offset = 709 + i * 8;
            tick_array_bitmap[i] = u64::from_le_bytes(data[offset..offset+8].try_into().unwrap());
        }

        // Remaining fields
        let total_fees_token0 = u64::from_le_bytes(data[837..845].try_into().unwrap());
        let total_fees_claimed_token0 = u64::from_le_bytes(data[845..853].try_into().unwrap());
        let total_fees_token1 = u64::from_le_bytes(data[853..861].try_into().unwrap());
        let total_fees_claimed_token1 = u64::from_le_bytes(data[861..869].try_into().unwrap());
        let fund_fees_token0 = u64::from_le_bytes(data[869..877].try_into().unwrap());
        let fund_fees_token1 = u64::from_le_bytes(data[877..885].try_into().unwrap());
        let open_time = u64::from_le_bytes(data[885..893].try_into().unwrap());
        let recent_epoch = u64::from_le_bytes(data[893..901].try_into().unwrap());

        // Padding arrays
        let mut padding1 = [0u64; 24];
        for i in 0..24 {
            let offset = 901 + i * 8;
            padding1[i] = u64::from_le_bytes(data[offset..offset+8].try_into().unwrap());
        }

        let mut padding2 = [0u64; 32];
        for i in 0..32 {
            let offset = 1093 + i * 8;
            padding2[i] = u64::from_le_bytes(data[offset..offset+8].try_into().unwrap());
        }

        Ok(RaydiumCLMM {
            bump,
            amm_config,
            owner,
            token_mint0,
            token_mint1,
            token_vault0,
            token_vault1,
            observation_key,
            mint_decimals0,
            mint_decimals1,
            tick_spacing,
            liquidity,
            sqrt_price_x64,
            tick_current,
            padding3,
            padding4,
            fee_growth_global0_x64,
            fee_growth_global1_x64,
            protocol_fees_token0,
            protocol_fees_token1,
            swap_in_amount_token0,
            swap_out_amount_token1,
            swap_in_amount_token1,
            swap_out_amount_token0,
            status,
            padding,
            reward_infos,
            tick_array_bitmap,
            total_fees_token0,
            total_fees_claimed_token0,
            total_fees_token1,
            total_fees_claimed_token1,
            fund_fees_token0,
            fund_fees_token1,
            open_time,
            recent_epoch,
            padding1,
            padding2,
        })
    }
}



