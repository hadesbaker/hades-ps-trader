use crate::config::{BoxError, JitoConfig};
use log::{error, info, warn};
use serde_json::{json, Value};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSendTransactionConfig;
use solana_sdk::commitment_config::CommitmentLevel;
use solana_sdk::message::VersionedMessage;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_sdk::transaction::VersionedTransaction;
use solana_transaction_status::TransactionConfirmationStatus;
use std::sync::OnceLock;
use std::time::Instant;
use tokio::time::{sleep, Duration};

const TRADE_LOCAL_URL: &str = "https://pumpportal.fun/api/trade-local";
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(60);

/// Process-global Jito settings, set once at startup via [`init_jito`]. `Some`
/// only when `[jito].enabled = true`; otherwise trades go via plain RPC send.
/// Global (vs threaded through every `BuyParams`) because `enabled`/url/tip are
/// process-wide, keeping the many call sites untouched.
static JITO: OnceLock<Option<JitoConfig>> = OnceLock::new();

/// Initialize Jito submission from config. Call once at startup before trading.
pub fn init_jito(cfg: &JitoConfig) {
    let settings = if cfg.enabled { Some(cfg.clone()) } else { None };
    if let Some(s) = &settings {
        info!(
            "JITO bundle submission ENABLED — tip={} SOL, engine={}",
            s.tip_sol, s.block_engine_url
        );
    }
    let _ = JITO.set(settings);
}

fn jito_settings() -> Option<&'static JitoConfig> {
    JITO.get().and_then(|o| o.as_ref())
}

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

    // Jito path: submit the trade as a single-tx bundle for atomic, fast
    // (single-block) landing. Only a send-side failure (trade-local / sendBundle
    // HTTP error) falls back to RPC — a confirm timeout AFTER a successful bundle
    // send does NOT fall back, to avoid double-executing a trade that may have
    // landed (matches the RPC path's confirm-timeout-is-terminal behavior).
    if let Some(j) = jito_settings() {
        match send_bundle(http, rpc, keypair, body, action, &label, j).await {
            Ok(sig) => {
                info!("{action} {label}: jito bundle submitted: {sig}");
                confirm(rpc, &sig, action).await?;
                return Ok(Some(sig));
            }
            Err(e) => warn!(
                "{action} {label}: jito bundle send failed ({e}); falling back to RPC send"
            ),
        }
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

/// Submit the trade as a single-tx Jito bundle. Requests the trade-local tx in
/// ARRAY form (PumpPortal uses the first tx's priorityFee as the Jito tip and
/// builds the tip transfer in), signs it with a fresh blockhash, base58-encodes
/// it, and POSTs a `sendBundle` JSON-RPC to the block engine. Returns the tx
/// signature once the engine accepts the bundle. Any HTTP / encoding error is
/// returned so the caller can fall back to a plain RPC send.
async fn send_bundle(
    http: &reqwest::Client,
    rpc: &RpcClient,
    keypair: &Keypair,
    body: &Value,
    action: &str,
    label: &str,
    jito: &JitoConfig,
) -> Result<Signature, BoxError> {
    // Array request; the tip is paid via the first tx's priorityFee in bundle mode.
    let mut tx_body = body.clone();
    tx_body["priorityFee"] = json!(jito.tip_sol);
    let arr = Value::Array(vec![tx_body]);

    info!("POST trade-local (jito array) {action} {label}");
    let resp = http.post(TRADE_LOCAL_URL).json(&arr).send().await?;
    let status = resp.status();
    let bytes = resp.bytes().await?;
    if !status.is_success() {
        return Err(format!(
            "trade-local (jito) HTTP {status}: {}",
            String::from_utf8_lossy(&bytes)
        )
        .into());
    }
    // Array response: a JSON array of base58-encoded unsigned txs.
    let encoded: Vec<String> = serde_json::from_slice(&bytes).map_err(|e| {
        format!(
            "jito trade-local response not a base58 tx array ({e}): {}",
            String::from_utf8_lossy(&bytes)
        )
    })?;
    if encoded.is_empty() {
        return Err("jito trade-local returned an empty tx array".into());
    }

    let fresh = rpc.get_latest_blockhash().await?;
    let mut signed_b58: Vec<String> = Vec::with_capacity(encoded.len());
    let mut first_sig: Option<Signature> = None;
    for enc in &encoded {
        let raw = bs58::decode(enc)
            .into_vec()
            .map_err(|e| format!("jito tx base58 decode: {e}"))?;
        let unsigned: VersionedTransaction = bincode::deserialize(&raw)
            .map_err(|e| format!("jito tx deserialize ({} bytes): {e}", raw.len()))?;
        let mut message = unsigned.message;
        match &mut message {
            VersionedMessage::Legacy(m) => m.recent_blockhash = fresh,
            VersionedMessage::V0(m) => m.recent_blockhash = fresh,
        }
        let signed = VersionedTransaction {
            signatures: vec![keypair.sign_message(&message.serialize())],
            message,
        };
        if first_sig.is_none() {
            first_sig = Some(signed.signatures[0]);
        }
        let ser =
            bincode::serialize(&signed).map_err(|e| format!("jito signed tx serialize: {e}"))?;
        signed_b58.push(bs58::encode(ser).into_string());
    }

    let bundle_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "sendBundle",
        "params": [signed_b58],
    });
    let jresp = http.post(&jito.block_engine_url).json(&bundle_req).send().await?;
    let jstatus = jresp.status();
    let jbytes = jresp.bytes().await?;
    if !jstatus.is_success() {
        return Err(format!(
            "sendBundle HTTP {jstatus}: {}",
            String::from_utf8_lossy(&jbytes)
        )
        .into());
    }
    let jval: Value = serde_json::from_slice(&jbytes).map_err(|e| {
        format!(
            "sendBundle response parse ({e}): {}",
            String::from_utf8_lossy(&jbytes)
        )
    })?;
    if let Some(err) = jval.get("error") {
        return Err(format!("sendBundle error: {err}").into());
    }
    first_sig.ok_or_else(|| "jito bundle produced no signature".into())
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
