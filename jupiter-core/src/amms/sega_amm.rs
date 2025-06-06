use anyhow::{anyhow, Context, Result};
use anchor_lang::{AccountDeserialize, ToAccountMetas};
use jupiter_amm_interface::{
    try_get_account_data, AccountMap, Amm, AmmContext, KeyedAccount, Quote, QuoteParams,
    SwapAndAccountMetas, SwapParams, Swap,
};
use rust_decimal::prelude::FromPrimitive;
use spl_token_2022::extension::BaseStateWithExtensions;
use spl_token_2022::extension::{
    transfer_fee::TransferFeeConfig, StateWithExtensions, StateWithExtensionsOwned,
};
use lazy_static::lazy_static;
use spl_token_2022::state::Mint;
use std::sync::atomic::{AtomicI64, AtomicU64};
use std::sync::Arc;
use std::collections::HashMap;
use solana_sdk::{pubkey, pubkey::Pubkey};

use crate::sega::{
    AmmConfig, PoolState, AUTH_SEED, ObservationState, CurveCalculator, SegaSwap,
    PoolStatusBitIndex
};

mod sega_swap_programs {
    use super::*;
    pub const SEGA: Pubkey = pubkey!("SegaXNnoXYTZiqUt9Xn2XqGcL56b25yzXLuJSpadcMu");
}

lazy_static! {
    pub static ref SEGA_SWAP_PROGRAMS: HashMap<Pubkey, String> = {
        let mut m = HashMap::new();
        m.insert(sega_swap_programs::SEGA, "Sega".into());
        m
    };
}

#[derive(Clone)]
pub struct TokenMints {
    token0: Pubkey,
    token1: Pubkey,
    token0_mint: StateWithExtensionsOwned<Mint>,
    token1_mint: StateWithExtensionsOwned<Mint>,
    token0_program: Pubkey,
    token1_program: Pubkey,
}

#[derive(Clone)]
pub struct SegaAmm {
    key: Pubkey,
    pool_state: PoolState,
    amm_config: Option<AmmConfig>,
    vault_0_amount: Option<u64>,
    vault_1_amount: Option<u64>,
    token_mints_and_token_programs: Option<TokenMints>,
    epoch: Arc<AtomicU64>,
    timestamp: Arc<AtomicI64>,
    program_id: Pubkey,
}

impl SegaAmm {
    fn get_authority(&self) -> Pubkey {
        Pubkey::create_program_address(
            &[AUTH_SEED.as_bytes(), &[self.pool_state.auth_bump]],
            &self.program_id,
        )
        .unwrap()
    }
   
}

impl Amm for SegaAmm {
    fn from_keyed_account(keyed_account: &KeyedAccount, amm_context: &AmmContext) -> Result<Self> {
        let pool_state = PoolState::try_deserialize(&mut keyed_account.account.data.as_ref())?;

        Ok(Self {
            key: keyed_account.key,
            pool_state,
            amm_config: None,
            vault_0_amount: None,
            vault_1_amount: None,
            token_mints_and_token_programs: None,
            epoch: amm_context.clock_ref.epoch.clone(),
            timestamp: amm_context.clock_ref.unix_timestamp.clone(),
            program_id: keyed_account.account.owner,
        })
    }

    fn label(&self) -> String {
        "SEGA".into()
    }

    fn program_id(&self) -> Pubkey {
        self.program_id
    }

    fn key(&self) -> Pubkey {
        self.key
    }

    fn get_reserve_mints(&self) -> Vec<Pubkey> {
        vec![self.pool_state.token_0_mint, self.pool_state.token_1_mint]
    }

    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        let mut keys = vec![
            self.key,
            self.pool_state.token_0_vault,
            self.pool_state.token_1_vault,
            self.pool_state.amm_config,
        ];
        keys.extend([self.pool_state.token_0_mint, self.pool_state.token_1_mint]);
        keys
    }

    fn update(&mut self, account_map: &AccountMap) -> Result<()> {
        let pool_state_data = try_get_account_data(account_map, &self.key)?;
        self.pool_state = PoolState::try_deserialize(&mut pool_state_data.as_ref())?;

        let token0_mint = try_get_account_data(account_map, &self.pool_state.token_0_mint)
            .ok()
            .and_then(|account_data| {
                StateWithExtensionsOwned::<spl_token_2022::state::Mint>::unpack(
                    account_data.to_vec(),
                )
                .ok()
            })
            .context("Token 0 mint not found")?;

        let token1_mint = try_get_account_data(account_map, &self.pool_state.token_1_mint)
            .ok()
            .and_then(|account_data| {
                StateWithExtensionsOwned::<spl_token_2022::state::Mint>::unpack(
                    account_data.to_vec(),
                )
                .ok()
            })
            .context("Token 1 mint not found")?;

        self.token_mints_and_token_programs = Some(TokenMints {
            token0: self.pool_state.token_0_mint,
            token1: self.pool_state.token_1_mint,
            token0_mint,
            token1_mint,
            token0_program: self.pool_state.token_0_program,
            token1_program: self.pool_state.token_1_program,
        });

        let amm_config_data = try_get_account_data(account_map, &self.pool_state.amm_config)?;
        self.amm_config = Some(AmmConfig::try_deserialize(&mut amm_config_data.as_ref())?);

        let get_unfrozen_token_amount = |token_vault| {
            try_get_account_data(account_map, token_vault)
                .ok()
                .and_then(|account_data| {
                    StateWithExtensions::<spl_token_2022::state::Account>::unpack(account_data).ok()
                })
                .and_then(|token_account| {
                    if token_account.base.is_frozen() {
                        None
                    } else {
                        Some(token_account.base.amount)
                    }
                })
        };

        self.vault_0_amount = get_unfrozen_token_amount(&self.pool_state.token_0_vault);
        self.vault_1_amount = get_unfrozen_token_amount(&self.pool_state.token_1_vault);

        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote> {
        if !self.pool_state.get_status_by_bit(PoolStatusBitIndex::Swap)
            || (self.timestamp.load(std::sync::atomic::Ordering::Relaxed) as u64)
                < self.pool_state.open_time
        {
            return Err(anyhow!("Pool is not trading"));
        }
        let amm_config = self.amm_config.as_ref().context("Missing AmmConfig")?;

        let zero_for_one: bool = quote_params.input_mint == self.pool_state.token_0_mint;

        let TokenMints {
            token0_mint: token_mint_0,
            token1_mint: token_mint_1,
            ..
        } = self
            .token_mints_and_token_programs
            .as_ref()
            .ok_or(anyhow!("Missing token mints and token programs"))?;

        let token_mint_0_transfer_fee_config =
            token_mint_0.get_extension::<TransferFeeConfig>().ok();
        let token_mint_1_transfer_fee_config =
            token_mint_1.get_extension::<TransferFeeConfig>().ok();

        let (source_mint_transfer_fee_config, destination_mint_transfer_fee_config) =
            if zero_for_one {
                (
                    token_mint_0_transfer_fee_config,
                    token_mint_1_transfer_fee_config,
                )
            } else {
                (
                    token_mint_1_transfer_fee_config,
                    token_mint_0_transfer_fee_config,
                )
            };

        let amount = quote_params.amount;
        let epoch = self.epoch.load(std::sync::atomic::Ordering::Relaxed);
        let actual_amount_in = if let Some(transfer_fee_config) = source_mint_transfer_fee_config {
            amount.saturating_sub(
                transfer_fee_config
                    .calculate_epoch_fee(epoch, amount)
                    .context("Fee calculation failure")?,
            )
        } else {
            amount
        };
        if actual_amount_in == 0 {
            return Err(anyhow!("Amount too low"));
        }

        // Calculate the trade amounts
        let (total_token_0_amount, total_token_1_amount) = match vault_amount_without_fee(
            &self.pool_state,
            self.vault_0_amount.context("Vault 0 missing or frozen")?,
            self.vault_1_amount.context("Vault 1 missing or frozen")?,
        ) {
            (Some(vault_0), Some(vault_1)) => (vault_0, vault_1),
            _ => return Err(anyhow!("Vault amount underflow")),
        };

        let swap_result = CurveCalculator::swap_base_input(
            u128::from(actual_amount_in),
            total_token_0_amount.into(),
            total_token_1_amount.into(),
            amm_config.trade_fee_rate,
            amm_config.protocol_fee_rate,
            amm_config.fund_fee_rate,
        )
        .context("Swap failed")?;
    
        let amount_out: u64 = swap_result.destination_amount_swapped.try_into()?;
        let actual_amount_out = if let Some(transfer_fee_config) = destination_mint_transfer_fee_config {
            amount_out.saturating_sub(
                transfer_fee_config
                    .calculate_epoch_fee(epoch, amount_out)
                    .context("Fee calculation failure")?,
            )
        } else {
            amount_out
        };

        let fee_amount = swap_result.trade_fee;

        Ok(Quote {
            in_amount: swap_result.source_amount_swapped.try_into()?,
            out_amount: actual_amount_out,
            fee_mint: quote_params.input_mint,
            fee_amount: fee_amount.try_into()?,
            fee_pct: rust_decimal::Decimal::from(fee_amount) / rust_decimal::Decimal::from(100),            
            ..Default::default()

        })
    }

    fn get_accounts_len(&self) -> usize {
        14
    }

    fn get_swap_and_account_metas(&self, swap_params: &SwapParams) -> Result<SwapAndAccountMetas> {
        if self.token_mints_and_token_programs.is_none() {
            return Err(anyhow!("Missing token mints and token programs"));
        }

        let TokenMints {
            token0_program: token_0_token_program,
            token1_program: token_1_token_program,
            ..
        } = self
            .token_mints_and_token_programs
            .as_ref()
            .ok_or(anyhow!("Missing token mints and token programs"))?;

        let (
            input_token_program,
            input_vault,
            input_token_mint,
            output_token_program,
            output_vault,
            output_token_mint,
        ) = if swap_params.source_mint == self.pool_state.token_0_mint {
            (
                *token_0_token_program,
                self.pool_state.token_0_vault,
                self.pool_state.token_0_mint,
                *token_1_token_program,
                self.pool_state.token_1_vault,
                self.pool_state.token_1_mint,
            )
        } else {
            (
                *token_1_token_program,
                self.pool_state.token_1_vault,
                self.pool_state.token_1_mint,
                *token_0_token_program,
                self.pool_state.token_0_vault,
                self.pool_state.token_0_mint,
            )
        };

        let account_metas = SegaSwap {
            program: self.program_id,
            payer: swap_params.token_transfer_authority,
            authority: self.get_authority(),
            amm_config: self.pool_state.amm_config,
            pool_state: self.key,
            input_token_account: swap_params.source_token_account,
            output_token_account: swap_params.destination_token_account,
            input_vault,
            output_vault,
            input_token_program,
            output_token_program,
            input_token_mint,
            output_token_mint,
            observation_state: self.pool_state.observation_key,
        }
        .to_account_metas(None);

        Ok(SwapAndAccountMetas {
            swap: Swap::RaydiumCP,
            account_metas,
        })
    }

    fn clone_amm(&self) -> Box<dyn Amm + Send + Sync> {
        Box::new(self.clone())
    }
}


// We are extracting this here to avoid the need to fix the contract it self.
// https://github.com/raydium-io/raydium-cp-swap/blob/master/programs/cp-swap/src/states/pool.rs#L139-L148
fn vault_amount_without_fee(
    pool: &PoolState,
    vault_0: u64,
    vault_1: u64,
) -> (Option<u64>, Option<u64>) {
    (
        vault_0.checked_sub(pool.protocol_fees_token_0 + pool.fund_fees_token_0),
        vault_1.checked_sub(pool.protocol_fees_token_1 + pool.fund_fees_token_1),
    )
}