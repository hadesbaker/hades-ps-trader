use crate::config::BoxError;
use log::{error, info, warn};
use serde_json::{json, Value};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSendTransactionConfig;
use solana_sdk::commitment_config::CommitmentLevel;
use solana_sdk::message::VersionedMessage;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_sdk::transaction::VersionedTransaction;
use solana_transaction_status::TransactionConfirmationStatus;
use std::time::Instant;
use tokio::time::{sleep, Duration};

const TRADE_LOCAL_URL: &str = "https://pumpportal.fun/api/trade-local";
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(60);

// Slippage escalation: on tx failure, retry with a higher slippage tolerance.
// `submit` (HTTP + sign + send + confirm) is treated as one atomic attempt;
// any error in that chain triggers a retry with `slippage += STEP`.
const SLIPPAGE_STEP_PCT: f64 = 5.0;
const BUY_MAX_ATTEMPTS: u32 = 3;   // initial + 2 escalations → cap at start + 10%
const SELL_MAX_ATTEMPTS: u32 = 20; // initial + 19 escalations → cap at start + 95% (never bag-hold)
const CONFIRM_POLL_INTERVAL: Duration = Duration::from_millis(500);

pub struct BuyParams<'a> {
    pub mint: &'a str,
    pub symbol: Option<&'a str>,
    pub amount_sol: f64,
    pub slippage_pct: f64,
    pub priority_fee_sol: f64,
    pub max_retries: u64,
}

pub struct SellAllParams<'a> {
    pub mint: &'a str,
    pub symbol: Option<&'a str>,
    pub slippage_pct: f64,
    pub priority_fee_sol: f64,
    pub max_retries: u64,
}

fn display(mint: &str, symbol: Option<&str>) -> String {
    match symbol {
        Some(s) => format!("{mint} ({s})"),
        None => mint.to_string(),
    }
}

pub async fn buy(
    http: &reqwest::Client,
    rpc: &RpcClient,
    keypair: &Keypair,
    p: BuyParams<'_>,
    dry_run: bool,
) -> Result<Option<Signature>, BoxError> {
    let mut slippage = p.slippage_pct;
    let mut last_err: Option<BoxError> = None;
    for attempt in 1..=BUY_MAX_ATTEMPTS {
        let body = json!({
            "publicKey": keypair.pubkey().to_string(),
            "action": "buy",
            "mint": p.mint,
            "amount": p.amount_sol,
            "denominatedInSol": "true",
            "slippage": slippage,
            "priorityFee": p.priority_fee_sol,
            "pool": "pump-amm",
        });
        match submit(http, rpc, keypair, &body, dry_run, "buy", p.mint, p.symbol, p.max_retries).await {
            Ok(opt) => {
                if attempt > 1 {
                    info!(
                        "buy succeeded on attempt {attempt}/{BUY_MAX_ATTEMPTS} (slippage={slippage:.1}%)"
                    );
                }
                return Ok(opt);
            }
            Err(e) => {
                warn!(
                    "buy attempt {attempt}/{BUY_MAX_ATTEMPTS} (slippage={slippage:.1}%) failed: {e}"
                );
                last_err = Some(e);
                slippage += SLIPPAGE_STEP_PCT;
            }
        }
    }
    Err(last_err.unwrap_or_else(|| "buy slippage escalation exhausted".into()))
}

pub async fn sell_all(
    http: &reqwest::Client,
    rpc: &RpcClient,
    keypair: &Keypair,
    p: SellAllParams<'_>,
    dry_run: bool,
) -> Result<Option<Signature>, BoxError> {
    let mut slippage = p.slippage_pct;
    let mut last_err: Option<BoxError> = None;
    for attempt in 1..=SELL_MAX_ATTEMPTS {
        let body = json!({
            "publicKey": keypair.pubkey().to_string(),
            "action": "sell",
            "mint": p.mint,
            "amount": "100%",
            "denominatedInSol": "false",
            "slippage": slippage,
            "priorityFee": p.priority_fee_sol,
            "pool": "pump-amm",
        });
        match submit(http, rpc, keypair, &body, dry_run, "sell", p.mint, p.symbol, p.max_retries).await {
            Ok(opt) => {
                if attempt > 1 {
                    info!(
                        "sell succeeded on attempt {attempt}/{SELL_MAX_ATTEMPTS} (slippage={slippage:.1}%)"
                    );
                }
                return Ok(opt);
            }
            Err(e) => {
                warn!(
                    "sell attempt {attempt}/{SELL_MAX_ATTEMPTS} (slippage={slippage:.1}%) failed: {e}"
                );
                last_err = Some(e);
                slippage += SLIPPAGE_STEP_PCT;
            }
        }
    }
    error!(
        "SELL EXHAUSTED ALL {SELL_MAX_ATTEMPTS} SLIPPAGE ESCALATIONS up to {:.1}% — BAG HELD: mint={}",
        slippage - SLIPPAGE_STEP_PCT,
        p.mint
    );
    Err(last_err.unwrap_or_else(|| "sell slippage escalation exhausted".into()))
}

async fn submit(
    http: &reqwest::Client,
    rpc: &RpcClient,
    keypair: &Keypair,
    body: &Value,
    dry_run: bool,
    action: &str,
    mint: &str,
    symbol: Option<&str>,
    max_retries: u64,
) -> Result<Option<Signature>, BoxError> {
    let label = display(mint, symbol);
    if dry_run {
        info!("[dry-run] {action} {label}: {body}");
        return Ok(None);
    }

    info!("POST trade-local {action} {label}");
    let resp = http.post(TRADE_LOCAL_URL).json(body).send().await?;
    let status = resp.status();
    let bytes = resp.bytes().await?;
    if !status.is_success() {
        return Err(format!(
            "trade-local HTTP {status}: {}",
            String::from_utf8_lossy(&bytes)
        )
        .into());
    }

    let unsigned: VersionedTransaction = bincode::deserialize(&bytes)
        .map_err(|e| format!("VersionedTransaction deserialize ({} bytes): {e}", bytes.len()))?;

    let required = unsigned.message.header().num_required_signatures as usize;
    if required != 1 {
        warn!("{action} tx requires {required} signatures; signing only with our wallet — submission may fail");
    }

    // PumpPortal-supplied blockhash often isn't visible to our RPC yet, causing
    // a "Blockhash not found" preflight failure. Replace it with a fresh one
    // from our own RPC before signing.
    let mut message = unsigned.message;
    let fresh = rpc.get_latest_blockhash().await?;
    match &mut message {
        VersionedMessage::Legacy(m) => m.recent_blockhash = fresh,
        VersionedMessage::V0(m) => m.recent_blockhash = fresh,
    }

    let signed = VersionedTransaction {
        signatures: vec![keypair.sign_message(&message.serialize())],
        message,
    };

    let send_config = RpcSendTransactionConfig {
        skip_preflight: false,
        preflight_commitment: Some(CommitmentLevel::Confirmed),
        encoding: None,
        max_retries: Some(max_retries as usize),
        min_context_slot: None,
    };

    let sig = rpc.send_transaction_with_config(&signed, send_config).await?;
    info!("{action} submitted (max_retries={max_retries}): {sig}");

    confirm(rpc, &sig, action).await?;
    Ok(Some(sig))
}

async fn confirm(rpc: &RpcClient, sig: &Signature, action: &str) -> Result<(), BoxError> {
    let started = Instant::now();
    loop {
        let resp = rpc.get_signature_statuses(&[*sig]).await?;
        if let Some(Some(status)) = resp.value.first() {
            if let Some(err) = &status.err {
                return Err(format!("{action} tx {sig} failed on-chain: {err}").into());
            }
            if matches!(
                status.confirmation_status,
                Some(TransactionConfirmationStatus::Confirmed | TransactionConfirmationStatus::Finalized)
            ) {
                info!("{action} confirmed: {sig}");
                return Ok(());
            }
        }
        if started.elapsed() > CONFIRM_TIMEOUT {
            return Err(format!(
                "{action} confirmation timed out after {}s for {sig}",
                CONFIRM_TIMEOUT.as_secs()
            )
            .into());
        }
        sleep(CONFIRM_POLL_INTERVAL).await;
    }
}
