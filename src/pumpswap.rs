//! Direct PumpSwap AMM pool reads. No third-party indexers.
//!
//! Pricing strategy is borrowed from hades-kat-bot:
//!   1) Discover the pool's `base_vault` (token side) and `quote_vault` (WSOL side)
//!      once per token using `getTokenLargestAccounts` + `getMultipleAccounts`.
//!   2) Read both vaults in a single `getMultipleAccounts` call to compute price.
//!
//! Decimals are hardcoded: pump.fun mints are always 6, WSOL is always 9.

use crate::config::BoxError;
use log::debug;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_request::TokenAccountsFilter;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use std::time::Duration;

pub const PUMP_SWAP_PROGRAM_ID: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

// pump.fun tokens always use 6 decimals (enforced by the pump.fun program).
const BASE_DECIMAL_FACTOR: f64 = 1_000_000.0;
const SOL_DECIMAL_FACTOR: f64 = 1_000_000_000.0;

// SPL token account layout: mint(0..32), owner(32..64), amount(64..72)
const SPL_OWNER_OFFSET: usize = 32;
const SPL_OWNER_END: usize = 64;
const SPL_AMOUNT_OFFSET: usize = 64;
const SPL_AMOUNT_END: usize = 72;

#[derive(Debug, Clone, Copy)]
pub struct PoolInfo {
    pub pool_id: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
}

/// Discover a PumpSwap pool's vault addresses for `mint`.
///
/// Strategy:
///   1. `getTokenLargestAccounts(mint)` → top SPL token accounts holding this mint.
///   2. `getMultipleAccounts` on the top 10 → extract their owner pubkeys.
///   3. `getMultipleAccounts` on those owners → find one whose `acc.owner` is the
///      PumpSwap program. That pubkey is the pool; the token account at the same
///      index is the base vault.
///   4. `getTokenAccountsByOwner(pool, WSOL)` → quote vault.
pub async fn find_pool(rpc: &RpcClient, mint: &Pubkey) -> Result<PoolInfo, BoxError> {
    let pump_swap_program = Pubkey::from_str(PUMP_SWAP_PROGRAM_ID).unwrap();
    let wsol = Pubkey::from_str(WSOL_MINT).unwrap();

    let largest = tokio::time::timeout(
        Duration::from_secs(3),
        rpc.get_token_largest_accounts(mint),
    )
    .await
    .map_err(|_| "getTokenLargestAccounts timed out (3s)")??;

    if largest.is_empty() {
        return Err(format!("no token accounts found for {mint}").into());
    }

    let token_account_pubkeys: Vec<Pubkey> = largest
        .iter()
        .take(10)
        .filter_map(|a| Pubkey::from_str(&a.address).ok())
        .collect();

    let token_accounts = rpc
        .get_multiple_accounts(&token_account_pubkeys)
        .await
        .map_err(|e| format!("get_multiple_accounts (token accounts): {e}"))?;

    let mut candidates: Vec<(Pubkey, usize)> = Vec::new();
    for (idx, acc_opt) in token_accounts.iter().enumerate() {
        if let Some(acc) = acc_opt {
            if acc.data.len() >= SPL_OWNER_END {
                let bytes: [u8; 32] = acc.data[SPL_OWNER_OFFSET..SPL_OWNER_END]
                    .try_into()
                    .unwrap();
                candidates.push((Pubkey::from(bytes), idx));
            }
        }
    }
    if candidates.is_empty() {
        return Err(format!("no decodable token accounts for {mint}").into());
    }

    let owner_pubkeys: Vec<Pubkey> = candidates.iter().map(|(pk, _)| *pk).collect();
    let owner_accounts = rpc
        .get_multiple_accounts(&owner_pubkeys)
        .await
        .map_err(|e| format!("get_multiple_accounts (owners): {e}"))?;

    let mut pool_id: Option<Pubkey> = None;
    let mut base_vault: Option<Pubkey> = None;
    for (i, acc_opt) in owner_accounts.iter().enumerate() {
        if let Some(acc) = acc_opt {
            if acc.owner == pump_swap_program {
                pool_id = Some(owner_pubkeys[i]);
                base_vault = Some(token_account_pubkeys[candidates[i].1]);
                break;
            }
        }
    }
    let pool_id = pool_id
        .ok_or_else(|| format!("no PumpSwap pool found in top holders for {mint}"))?;
    let base_vault = base_vault.unwrap();

    let sol_accounts = rpc
        .get_token_accounts_by_owner(&pool_id, TokenAccountsFilter::Mint(wsol))
        .await
        .map_err(|e| format!("get_token_accounts_by_owner (WSOL): {e}"))?;

    let quote_vault = sol_accounts
        .first()
        .and_then(|a| Pubkey::from_str(&a.pubkey).ok())
        .ok_or_else(|| format!("no SOL vault for PumpSwap pool {pool_id}"))?;

    debug!(
        "PumpSwap pool for {mint}: pool={pool_id} base_vault={base_vault} quote_vault={quote_vault}"
    );

    Ok(PoolInfo { pool_id, base_vault, quote_vault })
}

/// Fetch SOL-per-display-token price. One `getMultipleAccounts` call.
pub async fn fetch_price_sol(rpc: &RpcClient, pool: &PoolInfo) -> Result<f64, BoxError> {
    let accounts = tokio::time::timeout(
        Duration::from_millis(500),
        rpc.get_multiple_accounts(&[pool.base_vault, pool.quote_vault]),
    )
    .await
    .map_err(|_| "PumpSwap vault read timed out (500ms)")??;

    let base = accounts[0]
        .as_ref()
        .ok_or_else(|| format!("base vault {} does not exist", pool.base_vault))?;
    let quote = accounts[1]
        .as_ref()
        .ok_or_else(|| format!("quote vault {} does not exist", pool.quote_vault))?;

    if base.data.len() < SPL_AMOUNT_END {
        return Err(format!("base vault data too short: {} bytes", base.data.len()).into());
    }
    if quote.data.len() < SPL_AMOUNT_END {
        return Err(format!("quote vault data too short: {} bytes", quote.data.len()).into());
    }

    let base_amount = u64::from_le_bytes(
        base.data[SPL_AMOUNT_OFFSET..SPL_AMOUNT_END].try_into().unwrap(),
    );
    let quote_amount = u64::from_le_bytes(
        quote.data[SPL_AMOUNT_OFFSET..SPL_AMOUNT_END].try_into().unwrap(),
    );

    if base_amount == 0 {
        return Err("PumpSwap base vault is zero".into());
    }

    let price_sol = (quote_amount as f64 / SOL_DECIMAL_FACTOR)
        / (base_amount as f64 / BASE_DECIMAL_FACTOR);
    Ok(price_sol)
}
