use crate::config::BoxError;
use log::{debug, warn};
use solana_account_decoder::UiAccountEncoding;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use solana_client::rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use solana_client::rpc_filter::{Memcmp, RpcFilterType};
use solana_client::rpc_request::TokenAccountsFilter;
use solana_sdk::account::Account;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address_with_program_id;
use std::str::FromStr;
use tokio::time::{sleep, Duration};

/// PumpPortal's migration WS can fire before our RPC has indexed the migrate
/// slot. Retry "account not visible yet" a few times with a small backoff
/// before treating it as truly missing. Real RPC errors abort immediately.
const ACCOUNT_LOOKUP_ATTEMPTS: u32 = 4;
const ACCOUNT_LOOKUP_INTERVAL: Duration = Duration::from_millis(1000);

async fn get_account_with_retry(rpc: &RpcClient, pubkey: &Pubkey) -> Result<Account, BoxError> {
    for i in 0..ACCOUNT_LOOKUP_ATTEMPTS {
        match rpc
            .get_account_with_commitment(pubkey, CommitmentConfig::confirmed())
            .await
        {
            Ok(resp) => {
                if let Some(acc) = resp.value {
                    if i > 0 {
                        debug!("account {pubkey} became visible on attempt {}", i + 1);
                    }
                    return Ok(acc);
                }
                debug!(
                    "account {pubkey} not visible yet (attempt {}/{ACCOUNT_LOOKUP_ATTEMPTS})",
                    i + 1
                );
            }
            Err(e) => {
                return Err(format!("get_account {pubkey}: {e}").into());
            }
        }
        if i + 1 < ACCOUNT_LOOKUP_ATTEMPTS {
            sleep(ACCOUNT_LOOKUP_INTERVAL).await;
        }
    }
    Err(format!(
        "account {pubkey} not visible after {ACCOUNT_LOOKUP_ATTEMPTS} attempts"
    )
    .into())
}

pub const PUMPFUN_PROGRAM_ID: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
pub const TOKEN_2022_PROGRAM_ID: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
pub const METAPLEX_PROGRAM_ID: &str = "metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s";
const SPL_TOKEN_AMOUNT_OFFSET: usize = 64;
const SPL_OWNER_OFFSET: usize = 32;
const SPL_OWNER_END: usize = 64;

pub fn bonding_curve_pda(mint: &Pubkey) -> Pubkey {
    let program_id = Pubkey::from_str(PUMPFUN_PROGRAM_ID).expect("static program id");
    Pubkey::find_program_address(&[b"bonding-curve", mint.as_ref()], &program_id).0
}

/// Returns the SPL Token program (Classic or Token-2022) that owns this mint.
/// Pump.fun launches use either, so we have to ask the chain.
async fn resolve_token_program(rpc: &RpcClient, mint: &Pubkey) -> Result<Pubkey, BoxError> {
    let acc = get_account_with_retry(rpc, mint).await?;
    let token_2022: Pubkey = Pubkey::from_str(TOKEN_2022_PROGRAM_ID).unwrap();
    if acc.owner == spl_token::ID || acc.owner == token_2022 {
        Ok(acc.owner)
    } else {
        Err(format!("mint {mint} is not owned by any SPL Token program (owner={})", acc.owner).into())
    }
}

/// Count token accounts of `mint` with non-zero balance.
/// Resolves the correct token program (Classic / Token-2022) from the mint,
/// then memcmp-filters on the mint pubkey at offset 0. We deliberately do NOT
/// constrain by dataSize: Token-2022 accounts may carry extensions and exceed 165 bytes.
pub async fn holder_count(rpc: &RpcClient, mint: &Pubkey) -> Result<u64, BoxError> {
    let token_program = resolve_token_program(rpc, mint).await?;
    let config = RpcProgramAccountsConfig {
        filters: Some(vec![RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
            0,
            mint.to_bytes().to_vec(),
        ))]),
        account_config: RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64),
            data_slice: None,
            commitment: Some(CommitmentConfig::confirmed()),
            min_context_slot: None,
        },
        with_context: Some(false),
    };
    let accounts = rpc
        .get_program_accounts_with_config(&token_program, config)
        .await?;
    let count = accounts
        .iter()
        .filter(|(_, a)| {
            a.data.len() >= SPL_TOKEN_AMOUNT_OFFSET + 8
                && u64::from_le_bytes(
                    a.data[SPL_TOKEN_AMOUNT_OFFSET..SPL_TOKEN_AMOUNT_OFFSET + 8]
                        .try_into()
                        .unwrap(),
                ) > 0
        })
        .count() as u64;
    Ok(count)
}

pub struct TokenBalance {
    pub raw_amount: u64,
    pub decimals: u8,
}

impl TokenBalance {
    pub fn ui_amount(&self) -> f64 {
        self.raw_amount as f64 / 10f64.powi(self.decimals as i32)
    }
}

/// Fetch SPL token balance held by `owner` for `mint`. Each attempt tries two
/// strategies in order:
///   1. Direct read at the derived ATA (correct path, fast).
///   2. Scan `getTokenAccountsByOwner` filtered by mint and read whichever
///      account holds tokens. Survives any ATA-program mismatch edge case.
///
/// Both reads use "confirmed" commitment explicitly so we don't accidentally
/// wait for finalization (~13s) right after a confirmed buy.
pub async fn fetch_token_balance(
    rpc: &RpcClient,
    owner: &Pubkey,
    mint: &Pubkey,
    attempts: u32,
    interval: Duration,
) -> Result<TokenBalance, BoxError> {
    let token_program = resolve_token_program(rpc, mint).await?;
    let ata = get_associated_token_address_with_program_id(owner, mint, &token_program);
    let commitment = CommitmentConfig::confirmed();
    let mut last_err: Option<String> = None;

    for i in 0..attempts {
        // Strategy 1: direct ATA lookup.
        match rpc
            .get_token_account_balance_with_commitment(&ata, commitment)
            .await
        {
            Ok(resp) => {
                let amt = resp.value;
                match amt.amount.parse::<u64>() {
                    Ok(raw) if raw > 0 => {
                        return Ok(TokenBalance { raw_amount: raw, decimals: amt.decimals });
                    }
                    Ok(_) => last_err = Some("ATA balance is zero".to_string()),
                    Err(e) => last_err = Some(format!("token amount parse: {e}")),
                }
            }
            Err(e) => last_err = Some(e.to_string()),
        }

        // Strategy 2: scan owner's token accounts filtered by mint.
        match scan_owner_for_mint(rpc, owner, mint, commitment).await {
            Ok(Some(bal)) => return Ok(bal),
            Ok(None) => {}
            Err(e) => last_err = Some(format!("scan fallback: {e}")),
        }

        if i + 1 < attempts {
            warn!(
                "token balance fetch attempt {}/{attempts} for {ata}: {}; retrying in {}ms",
                i + 1,
                last_err.as_deref().unwrap_or(""),
                interval.as_millis(),
            );
            sleep(interval).await;
        }
    }
    Err(format!(
        "could not fetch token balance for {ata} after {attempts} attempts: {}",
        last_err.unwrap_or_default()
    )
    .into())
}

async fn scan_owner_for_mint(
    rpc: &RpcClient,
    owner: &Pubkey,
    mint: &Pubkey,
    commitment: CommitmentConfig,
) -> Result<Option<TokenBalance>, BoxError> {
    let accounts = rpc
        .get_token_accounts_by_owner(owner, TokenAccountsFilter::Mint(*mint))
        .await
        .map_err(|e| format!("get_token_accounts_by_owner: {e}"))?;
    for keyed in accounts {
        let pubkey = match Pubkey::from_str(&keyed.pubkey) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if let Ok(resp) = rpc
            .get_token_account_balance_with_commitment(&pubkey, commitment)
            .await
        {
            let amt = resp.value;
            if let Ok(raw) = amt.amount.parse::<u64>() {
                if raw > 0 {
                    return Ok(Some(TokenBalance { raw_amount: raw, decimals: amt.decimals }));
                }
            }
        }
    }
    Ok(None)
}

#[derive(Debug, Clone)]
pub struct TokenMetadata {
    pub name: String,
    pub symbol: String,
}

/// Look up the token's display name + symbol. Tries Metaplex Token Metadata
/// first (used by classic SPL Token pump.fun tokens). Falls back to the
/// SPL Token-2022 `TokenMetadata` extension on the mint itself (used by
/// newer pump.fun launches).
pub async fn fetch_token_metadata(
    rpc: &RpcClient,
    mint: &Pubkey,
) -> Result<TokenMetadata, BoxError> {
    match fetch_metaplex_metadata(rpc, mint).await {
        Ok(m) => Ok(m),
        Err(metaplex_err) => match fetch_token2022_metadata(rpc, mint).await {
            Ok(m) => Ok(m),
            Err(t22_err) => Err(format!(
                "metaplex: {metaplex_err}; token-2022: {t22_err}"
            )
            .into()),
        },
    }
}

async fn fetch_metaplex_metadata(
    rpc: &RpcClient,
    mint: &Pubkey,
) -> Result<TokenMetadata, BoxError> {
    let metaplex = Pubkey::from_str(METAPLEX_PROGRAM_ID).unwrap();
    let (pda, _) = Pubkey::find_program_address(
        &[b"metadata", metaplex.as_ref(), mint.as_ref()],
        &metaplex,
    );
    let acc = get_account_with_retry(rpc, &pda).await?;
    // Layout: key(1) | update_authority(32) | mint(32) | name(borsh) | symbol(borsh) | uri | …
    let mut offset = 1 + 32 + 32;
    let name = read_borsh_string(&acc.data, &mut offset)
        .ok_or("name parse failed")?;
    let symbol = read_borsh_string(&acc.data, &mut offset)
        .ok_or("symbol parse failed")?;
    Ok(TokenMetadata { name, symbol })
}

/// Walk the SPL Token-2022 TLV extension area on the mint and pull the
/// embedded `TokenMetadata` extension (extension type = 19).
async fn fetch_token2022_metadata(
    rpc: &RpcClient,
    mint: &Pubkey,
) -> Result<TokenMetadata, BoxError> {
    const ACCOUNT_TYPE_OFFSET: usize = 165;
    const TLV_START: usize = 166;
    const EXT_TOKEN_METADATA: u16 = 19;

    let acc = get_account_with_retry(rpc, mint).await?;
    if acc.data.len() < TLV_START {
        return Err("mint too short for extensions".into());
    }
    // Byte 165 must be AccountType::Mint (= 1) on Token-2022.
    if acc.data[ACCOUNT_TYPE_OFFSET] != 1 {
        return Err("mint has no Token-2022 extension area".into());
    }
    let mut offset = TLV_START;
    while offset + 4 <= acc.data.len() {
        let ext_type = u16::from_le_bytes([acc.data[offset], acc.data[offset + 1]]);
        let ext_len = u16::from_le_bytes([acc.data[offset + 2], acc.data[offset + 3]]) as usize;
        offset += 4;
        if offset + ext_len > acc.data.len() {
            break;
        }
        if ext_type == EXT_TOKEN_METADATA {
            let ext = &acc.data[offset..offset + ext_len];
            // Layout: update_authority(32) | mint(32) | name(borsh) | symbol(borsh) | uri | …
            if ext.len() < 64 {
                return Err("token-metadata ext too short".into());
            }
            let mut o = 64;
            let name = read_borsh_string(ext, &mut o)
                .ok_or("token-metadata name parse")?;
            let symbol = read_borsh_string(ext, &mut o)
                .ok_or("token-metadata symbol parse")?;
            return Ok(TokenMetadata { name, symbol });
        }
        offset += ext_len;
    }
    Err("no TokenMetadata extension found".into())
}

fn read_borsh_string(data: &[u8], offset: &mut usize) -> Option<String> {
    if data.len() < *offset + 4 { return None; }
    let len = u32::from_le_bytes(data[*offset..*offset + 4].try_into().ok()?) as usize;
    *offset += 4;
    if data.len() < *offset + len { return None; }
    let raw = &data[*offset..*offset + len];
    *offset += len;
    let s = String::from_utf8(raw.to_vec()).ok()?;
    Some(s.trim_end_matches('\0').trim().to_string())
}

/// Combined balance of the top-N holders (by raw token balance) as a fraction
/// of total supply, after excluding the pump.fun bonding-curve PDA and the
/// PumpSwap pool's base vault. Returned value is in [0, 1].
///
/// RPC cost: 1× getTokenLargestAccounts + 1× getMultipleAccounts(top accounts)
/// + 1× getMultipleAccounts(unique owner pubkeys) + 1× getTokenSupply.
pub async fn top_n_holder_concentration(
    rpc: &RpcClient,
    mint: &Pubkey,
    n: usize,
) -> Result<f64, BoxError> {
    let largest = rpc
        .get_token_largest_accounts(mint)
        .await
        .map_err(|e| format!("getTokenLargestAccounts: {e}"))?;
    if largest.is_empty() {
        return Err(format!("no token accounts returned for {mint}").into());
    }

    // (token_account_pubkey, raw_amount)
    let mut entries: Vec<(Pubkey, u64)> = Vec::with_capacity(largest.len());
    for a in &largest {
        let Ok(pk) = Pubkey::from_str(&a.address) else { continue };
        let Ok(raw) = a.amount.amount.parse::<u64>() else { continue };
        entries.push((pk, raw));
    }
    if entries.is_empty() {
        return Err(format!("no decodable largest accounts for {mint}").into());
    }

    // Read those token accounts to extract each one's owner pubkey at offset 32..64.
    let ta_pubkeys: Vec<Pubkey> = entries.iter().map(|(p, _)| *p).collect();
    let ta_accounts = rpc
        .get_multiple_accounts(&ta_pubkeys)
        .await
        .map_err(|e| format!("get_multiple_accounts (token accounts): {e}"))?;

    let owners: Vec<Option<Pubkey>> = ta_accounts
        .iter()
        .map(|acc_opt| {
            let acc = acc_opt.as_ref()?;
            if acc.data.len() < SPL_OWNER_END {
                return None;
            }
            let bytes: [u8; 32] = acc.data[SPL_OWNER_OFFSET..SPL_OWNER_END].try_into().ok()?;
            Some(Pubkey::from(bytes))
        })
        .collect();

    // Resolve which owners are PumpSwap-program-owned (i.e. pool addresses).
    let bonding_pda = bonding_curve_pda(mint);
    let pump_swap_program =
        Pubkey::from_str(crate::pumpswap::PUMP_SWAP_PROGRAM_ID).expect("static program id");

    let mut seen = std::collections::HashSet::new();
    let unique_owners: Vec<Pubkey> = owners
        .iter()
        .filter_map(|o| *o)
        .filter(|o| *o != bonding_pda)
        .filter(|o| seen.insert(*o))
        .collect();

    let mut pumpswap_owners: std::collections::HashSet<Pubkey> = std::collections::HashSet::new();
    if !unique_owners.is_empty() {
        let owner_accounts = rpc
            .get_multiple_accounts(&unique_owners)
            .await
            .map_err(|e| format!("get_multiple_accounts (owners): {e}"))?;
        for (pk, acc_opt) in unique_owners.iter().zip(owner_accounts.iter()) {
            if let Some(acc) = acc_opt {
                if acc.owner == pump_swap_program {
                    pumpswap_owners.insert(*pk);
                }
            }
        }
    }

    // Filter and take top N by raw balance. If we can't determine an account's
    // owner, keep it — it counts toward concentration. Better to over-count
    // than to silently drop a real whale.
    let mut kept: Vec<u64> = entries
        .iter()
        .zip(owners.iter())
        .filter(|(_, owner)| match owner {
            Some(o) => *o != bonding_pda && !pumpswap_owners.contains(o),
            None => true,
        })
        .map(|((_, amt), _)| *amt)
        .collect();
    kept.sort_unstable_by(|a, b| b.cmp(a));
    kept.truncate(n);

    let sum_top: u128 = kept.iter().map(|&v| v as u128).sum();

    let supply = rpc
        .get_token_supply(mint)
        .await
        .map_err(|e| format!("getTokenSupply: {e}"))?;
    let total_raw: u128 = supply
        .amount
        .parse::<u128>()
        .map_err(|e| format!("parse supply amount '{}': {e}", supply.amount))?;
    if total_raw == 0 {
        return Err(format!("total supply of {mint} is zero").into());
    }

    Ok(sum_top as f64 / total_raw as f64)
}

/// Activity summary for a pump.fun bonding-curve PDA.
#[derive(Debug)]
pub struct BondingCurveActivity {
    /// Number of signatures observed. If `capped` is true, the true count is at least this.
    pub count: u64,
    /// True if the page-walk hit `MAX_SIG_PAGES` before reaching end-of-history.
    pub capped: bool,
}

impl BondingCurveActivity {
    /// "N" if uncapped, "N+" if capped (i.e. the real count exceeds the internal walk).
    pub fn display(&self) -> String {
        if self.capped {
            format!("{}+", self.count)
        } else {
            self.count.to_string()
        }
    }
}

const SIG_PAGE_SIZE: usize = 1000;
/// Hard upper bound on pages walked when counting bonding-curve signatures.
/// 5 pages × 1000 sigs = 5000-sig cap. Above this we report `capped=true`.
/// Bumping this trades RPC calls for tail-precision on very hot tokens.
const MAX_SIG_PAGES: usize = 5;

/// Count signatures touching the bonding-curve PDA. pump.fun pre-mints all
/// tokens to the bonding curve and trades happen against it, so this signature
/// count ≈ trade count during the bonding phase.
///
/// Walks `getSignaturesForAddress` pages with a `before` cursor up to
/// `MAX_SIG_PAGES`. Returns the exact count when below the page cap; otherwise
/// returns `MAX_SIG_PAGES * SIG_PAGE_SIZE` with `capped=true`.
///
/// Caveat: if the caller's threshold exceeds the internal cap (5000), a
/// `capped` result will reject conservatively even if the true count would
/// have qualified. Raise `MAX_SIG_PAGES` if that ever matters.
pub async fn bonding_curve_tx_count(
    rpc: &RpcClient,
    mint: &Pubkey,
) -> Result<BondingCurveActivity, BoxError> {
    let pda = bonding_curve_pda(mint);
    let mut count: u64 = 0;
    let mut before: Option<solana_sdk::signature::Signature> = None;
    for _ in 0..MAX_SIG_PAGES {
        let cfg = GetConfirmedSignaturesForAddress2Config {
            before,
            until: None,
            limit: Some(SIG_PAGE_SIZE),
            commitment: Some(CommitmentConfig::confirmed()),
        };
        let sigs = rpc
            .get_signatures_for_address_with_config(&pda, cfg)
            .await?;
        count += sigs.len() as u64;
        if sigs.len() < SIG_PAGE_SIZE {
            return Ok(BondingCurveActivity { count, capped: false });
        }
        let last = sigs.last().unwrap();
        before = Some(
            solana_sdk::signature::Signature::from_str(&last.signature)
                .map_err(|e| format!("parse signature '{}' for pagination: {e}", last.signature))?,
        );
    }
    Ok(BondingCurveActivity { count, capped: true })
}
