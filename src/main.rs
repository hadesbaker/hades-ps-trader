mod capitulation;
mod config;
mod discord;
mod macd;
mod onchain;
mod position;
mod price_feed;
mod pumpportal;
mod pumpswap;
mod rug;
mod session;
mod trader;
mod wallet;

use crate::config::{BoxError, Config};
use crate::discord::Notifier;
use clap::Parser;
use log::{error, info, warn};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::signature::Signer;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

#[derive(Parser, Debug)]
#[command(version, about = "PumpSwap MACD trader")]
struct Args {
    /// Don't submit buy/sell transactions; log what would be sent instead.
    #[arg(long)]
    dry_run: bool,

    /// Path to config TOML.
    #[arg(long, default_value = "config.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    dotenvy::dotenv().ok();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();
    let cfg = Arc::new(Config::load(&args.config)?);
    match serde_json::to_string(&*cfg) {
        Ok(json) => info!("config: {json}"),
        Err(e) => warn!("config: could not serialize to JSON ({e}); proceeding"),
    }

    // Warn if MACD warmup eats the whole monitoring window.
    if cfg.trading.max_monitor_time > 0 {
        let warmup_bars = (cfg.macd.slow + cfg.macd.signal) as u64;
        let warmup_secs = warmup_bars.saturating_mul(cfg.macd.candle_interval_secs);
        if warmup_secs >= cfg.trading.max_monitor_time {
            warn!(
                "MACD warmup needs ~{warmup_secs}s ({warmup_bars} candles × {}s) but max_monitor_time={}s — MACD will never produce signals. Raise max_monitor_time, shorten candle_interval_secs, or lower slow/signal periods.",
                cfg.macd.candle_interval_secs, cfg.trading.max_monitor_time,
            );
        } else if warmup_secs * 2 >= cfg.trading.max_monitor_time {
            warn!(
                "MACD warmup ~{warmup_secs}s is >=50% of max_monitor_time={}s — only a short trading window will remain.",
                cfg.trading.max_monitor_time,
            );
        }
    }

    // A take-profit that fires before the lowest trailing tier engages means
    // trailing stops can never run — flag it.
    if cfg.trading.profit_target_percent > 0.0
        && cfg.trading.profit_target_percent
            < first_tier_gain(&cfg.trading.dynamic_trailing_stop_thresholds)
    {
        warn!(
            "profit_target_percent={} fires below the lowest trailing tier — trailing stops will never engage. Raise or set to 0 to disable.",
            cfg.trading.profit_target_percent
        );
    }

    if args.dry_run {
        warn!("--dry-run is ON: buys and sells will not be submitted");
    }

    let keypair = Arc::new(wallet::load(&cfg.wallet_path)?);
    info!("wallet pubkey: {}", keypair.pubkey());

    let rpc_url = std::env::var("SOLANA_RPC_URL").map_err(|_| "SOLANA_RPC_URL not set in env")?;
    // "confirmed" everywhere — finalized would inject ~13s of lag and break
    // post-buy ATA reads right after a confirmed buy.
    let rpc = Arc::new(RpcClient::new_with_commitment(
        rpc_url,
        CommitmentConfig::confirmed(),
    ));

    // Starting wallet balance — the baseline for a run's gross PnL.
    match onchain::fetch_sol_balance(&rpc, &keypair.pubkey()).await {
        Ok(sol) => info!("wallet balance: {sol:.5} SOL"),
        Err(e) => warn!("could not fetch starting wallet balance: {e}"),
    }

    let ws_url = match std::env::var("PUMPPORTAL_API_KEY") {
        Ok(k) if !k.is_empty() => format!("wss://pumpportal.fun/api/data?api-key={k}"),
        _ => "wss://pumpportal.fun/api/data".to_string(),
    };

    let mut migrations = pumpportal::spawn_migration_listener(ws_url.clone());
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    let webhook_url = std::env::var("DISCORD_WEBHOOK_URL").ok();
    let notifier = Notifier::new(http.clone(), webhook_url);
    if args.dry_run {
        info!("discord notifications suppressed under --dry-run");
    } else if notifier.enabled() {
        info!("discord notifications enabled");
    } else {
        info!("discord notifications disabled (DISCORD_WEBHOOK_URL not set)");
    }

    let open_positions = Arc::new(AtomicUsize::new(0));

    info!("hades-ps-trader running. Waiting for graduations…");

    while let Some(ev) = migrations.recv().await {
        let cfg2 = cfg.clone();
        let http2 = http.clone();
        let rpc2 = rpc.clone();
        let kp2 = keypair.clone();
        let positions2 = open_positions.clone();
        let notifier2 = notifier.clone();
        let dry = args.dry_run;
        tokio::spawn(async move {
            // Migration frames don't include name/symbol, so look them up via
            // Metaplex (Token-2022 TLV fallback) if missing. Best-effort.
            let mut ev = ev;
            if ev.symbol.is_none() {
                if let Ok(mint_pk) = solana_sdk::pubkey::Pubkey::from_str(&ev.mint) {
                    match onchain::fetch_token_metadata(&rpc2, &mint_pk).await {
                        Ok(meta) => {
                            ev.name = Some(meta.name);
                            ev.symbol = Some(meta.symbol);
                        }
                        Err(e) => warn!("metadata lookup failed for {}: {e}", ev.mint),
                    }
                }
            }
            match &ev.symbol {
                Some(s) => info!("\x1b[1;4;95mgraduation -> {} ({s})\x1b[0m", ev.mint),
                None => info!("\x1b[1;4;95mgraduation -> {}\x1b[0m", ev.mint),
            }
            session::handle(cfg2, http2, rpc2, kp2, positions2, notifier2, ev, dry).await;
        });
    }

    error!("PumpPortal listener channel closed; exiting.");
    Ok(())
}

/// Lowest `gain%` among the configured trailing-stop tiers, or +∞ if none.
fn first_tier_gain(s: &str) -> f64 {
    position::parse_tiers(s)
        .first()
        .map(|t| t.gain_pct)
        .unwrap_or(f64::INFINITY)
}
