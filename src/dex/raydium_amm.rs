use solana_client::nonblocking::rpc_client::RpcClient;
use std::{str::FromStr, sync::Arc, time::Duration};
use anyhow::{anyhow, Result};
use colored::Colorize;
use std::cmp;
use std::env;
use crate::common::pool::get_program_acccounts_with_filter_async;
use crate::dex::meteora_pools::{METEORA_POOLS_PROGRAM, METEORA_POOLS_POOL_SIZE, METEORA_POOLS_MINT1_POSITION, METEORA_POOLS_MINT2_POSITION};
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

pub const RAYDIUM_AMM_PROGRAM: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
pub const RAYDIUM_AMM_POOL_SIZE: u64 = 752;
pub const RAYDIUM_AMM_MINT1_POSITION: u64 = 248;
pub const RAYDIUM_AMM_MINT2_POSITION: u64 = 280;

#[derive(Debug, Clone)]
pub struct RaydiumAMM {
    pub status: u8,
    pub nonce: u8,
    pub max_order: u8,
    pub depth: u8,
    pub base_decimal: u8,
    pub quote_decimal: u8,
    pub state: u8,
    pub reset_flag: u8,
    pub min_size: u64,
    pub vol_max_cut_ratio: u64,
    pub amount_wave_ratio: u64,
    pub base_lot_size: u64,
    pub quote_lot_size: u64,
    pub min_price_multiplier: u64,
    pub max_price_multiplier: u64,
    pub system_decimal_value: u64,
    pub min_separate_numerator: u64,
    pub min_separate_denominator: u64,
    pub trade_fee_numerator: u64,
    pub trade_fee_denominator: u64,
    pub pnl_numerator: u64,
    pub pnl_denominator: u64,
    pub swap_fee_numerator: u64,
    pub swap_fee_denominator: u64,
    pub base_need_take_pnl: u64,
    pub quote_need_take_pnl: u64,
    pub quote_total_pnl: u64,
    pub base_total_pnl: u64,
    pub pool_open_time: u64,
    pub punish_pc_amount: u64,
    pub punish_coin_amount: u64,
    pub orderbook_to_init_time: u64,
    pub swap_base_in_amount: u64,
    pub swap_quote_out_amount: u64,
    pub swap_base2quote_fee: u64,
    pub swap_quote_in_amount: u64,
    pub swap_base_out_amount: u64,
    pub swap_quote2base_fee: u64,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub lp_mint: Pubkey,
    pub open_orders: Pubkey,
    pub market_id: Pubkey,
    pub market_program_id: Pubkey,
    pub target_orders: Pubkey,
    pub withdraw_queue: Pubkey,
    pub lp_vault: Pubkey,
    pub owner: Pubkey,
    pub lp_reserve: u64,
    pub padding: [u64; 3],
}

impl RaydiumAMM {
    //new liquidity pool based on the tokn mint
    async fn get_pool_by_mint (mint1: &str, mint2: &str) -> Result<RaydiumAMM> {
        let rpc_client = RpcClient::new(env::var("RPC_HTTP").unwrap());
        let mint1_pubkey = Pubkey::from_str(mint1)?;
        let mint2_pubkey = Pubkey::from_str(mint2)?;
        let pools = get_program_acccounts_with_filter_async(
            &rpc_client,
            &RAYDIUM_AMM_PROGRAM.parse().unwrap(),
            RAYDIUM_AMM_POOL_SIZE,
            &RAYDIUM_AMM_MINT1_POSITION.try_into().unwrap(),
            &RAYDIUM_AMM_MINT2_POSITION.try_into().unwrap(),
            &mint1_pubkey,
            &mint2_pubkey
            ).await?;
            
        if pools.is_empty() {
            return Err(anyhow!("No Raydium AMM pool found for the given mints"));
        }
        
        let (pubkey, account) = &pools[0];
        let pool_id = *pubkey;
        let data = &account.data;
        // 8-byte discriminator (skip if not needed)
        let _discriminator = &data[0..8];

        // u8 fields (8 total)
        let status = data[8];
        let nonce = data[9];
        let max_order = data[10];
        let depth = data[11];
        let base_decimal = data[12];
        let quote_decimal = data[13];
        let state = data[14];
        let reset_flag = data[15];

        // u64 fields (29 total)
        let min_size = u64::from_le_bytes(data[16..24].try_into().unwrap());
        let vol_max_cut_ratio = u64::from_le_bytes(data[24..32].try_into().unwrap());
        let amount_wave_ratio = u64::from_le_bytes(data[32..40].try_into().unwrap());
        let base_lot_size = u64::from_le_bytes(data[40..48].try_into().unwrap());
        let quote_lot_size = u64::from_le_bytes(data[48..56].try_into().unwrap());
        let min_price_multiplier = u64::from_le_bytes(data[56..64].try_into().unwrap());
        let max_price_multiplier = u64::from_le_bytes(data[64..72].try_into().unwrap());
        let system_decimal_value = u64::from_le_bytes(data[72..80].try_into().unwrap());
        let min_separate_numerator = u64::from_le_bytes(data[80..88].try_into().unwrap());
        let min_separate_denominator = u64::from_le_bytes(data[88..96].try_into().unwrap());
        let trade_fee_numerator = u64::from_le_bytes(data[96..104].try_into().unwrap());
        let trade_fee_denominator = u64::from_le_bytes(data[104..112].try_into().unwrap());
        let pnl_numerator = u64::from_le_bytes(data[112..120].try_into().unwrap());
        let pnl_denominator = u64::from_le_bytes(data[120..128].try_into().unwrap());
        let swap_fee_numerator = u64::from_le_bytes(data[128..136].try_into().unwrap());
        let swap_fee_denominator = u64::from_le_bytes(data[136..144].try_into().unwrap());
        let base_need_take_pnl = u64::from_le_bytes(data[144..152].try_into().unwrap());
        let quote_need_take_pnl = u64::from_le_bytes(data[152..160].try_into().unwrap());
        let quote_total_pnl = u64::from_le_bytes(data[160..168].try_into().unwrap());
        let base_total_pnl = u64::from_le_bytes(data[168..176].try_into().unwrap());
        let pool_open_time = u64::from_le_bytes(data[176..184].try_into().unwrap());
        let punish_pc_amount = u64::from_le_bytes(data[184..192].try_into().unwrap());
        let punish_coin_amount = u64::from_le_bytes(data[192..200].try_into().unwrap());
        let orderbook_to_init_time = u64::from_le_bytes(data[200..208].try_into().unwrap());
        let swap_base_in_amount = u64::from_le_bytes(data[208..216].try_into().unwrap());
        let swap_quote_out_amount = u64::from_le_bytes(data[216..224].try_into().unwrap());
        let swap_base2quote_fee = u64::from_le_bytes(data[224..232].try_into().unwrap());
        let swap_quote_in_amount = u64::from_le_bytes(data[232..240].try_into().unwrap());
        let swap_base_out_amount = u64::from_le_bytes(data[240..248].try_into().unwrap());
        let swap_quote2base_fee = u64::from_le_bytes(data[248..256].try_into().unwrap());

        // Pubkey fields (13 total)
        let base_vault = Pubkey::try_from(&data[256..288]).unwrap();
        let quote_vault = Pubkey::try_from(&data[288..320]).unwrap();
        let base_mint = Pubkey::try_from(&data[320..352]).unwrap();
        let quote_mint = Pubkey::try_from(&data[352..384]).unwrap();
        let lp_mint = Pubkey::try_from(&data[384..416]).unwrap();
        let open_orders = Pubkey::try_from(&data[416..448]).unwrap();
        let market_id = Pubkey::try_from(&data[448..480]).unwrap();
        let market_program_id = Pubkey::try_from(&data[480..512]).unwrap();
        let target_orders = Pubkey::try_from(&data[512..544]).unwrap();
        let withdraw_queue = Pubkey::try_from(&data[544..576]).unwrap();
        let lp_vault = Pubkey::try_from(&data[576..608]).unwrap();
        let owner = Pubkey::try_from(&data[608..640]).unwrap();

        // Final u64 and padding
        let lp_reserve = u64::from_le_bytes(data[640..648].try_into().unwrap());
        let padding = [
            u64::from_le_bytes(data[648..656].try_into().unwrap()),
            u64::from_le_bytes(data[656..664].try_into().unwrap()),
            u64::from_le_bytes(data[664..672].try_into().unwrap()),
        ];



    Ok(RaydiumAMM{
        status,
        nonce,
        max_order,
        depth,
        base_decimal,
        quote_decimal,
        state,
        reset_flag,
        min_size,
        vol_max_cut_ratio,
        amount_wave_ratio,
        base_lot_size,
        quote_lot_size,
        min_price_multiplier,
        max_price_multiplier,
        system_decimal_value,
        min_separate_numerator,
        min_separate_denominator,
        trade_fee_numerator,
        trade_fee_denominator,
        pnl_numerator,
        pnl_denominator,
        swap_fee_numerator,
        swap_fee_denominator,
        base_need_take_pnl,
        quote_need_take_pnl,
        quote_total_pnl,
        base_total_pnl,
        pool_open_time,
        punish_pc_amount,
        punish_coin_amount,
        orderbook_to_init_time,
        swap_base_in_amount,
        swap_quote_out_amount,
        swap_base2quote_fee,
        swap_quote_in_amount,
        swap_base_out_amount,
        swap_quote2base_fee,
        base_vault,
        quote_vault,
        base_mint,
        quote_mint,
        lp_mint,
        open_orders,
        market_id,
        market_program_id,
        target_orders,
        withdraw_queue,
        lp_vault,
        owner,
        lp_reserve,
        padding,
        })

    }
}
