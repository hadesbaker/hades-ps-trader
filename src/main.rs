mod capitulation;
mod config;
mod copy_session;
mod dip_buy;
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
use crate::dip_buy::{DailySpendTracker, SharedSpendTracker};
use crate::discord::Notifier;
use clap::Parser;
use log::{debug, error, info, warn};
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

    /// Liquidate mode: comma-separated mint addresses to sell 100% of and then
    /// exit. Reuses the normal sell path. Ignores copy/graduation modes.
    #[arg(long)]
    liquidate: Option<String>,
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

    // Initialize Jito bundle submission (no-op unless [jito].enabled = true).
    trader::init_jito(&cfg.jito);

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

    // ---- liquidate mode: sell 100% of the given mints, then exit ----
    if let Some(mints) = args.liquidate.clone() {
        let list: Vec<String> = mints
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        info!("LIQUIDATE MODE — selling {} mint(s), then exiting", list.len());
        let mut ok = 0usize;
        for mint in &list {
            info!("liquidating {mint}…");
            match trader::sell_all(
                &http,
                &rpc,
                &keypair,
                trader::SellAllParams {
                    mint,
                    symbol: None,
                    slippage_pct: cfg.trading.slippage,
                    priority_fee_sol: cfg.trading.priority_fee_sol,
                    max_retries: cfg.trading.sell_tx_retries,
                },
                args.dry_run,
            )
            .await
            {
                Ok(Some(sig)) => {
                    ok += 1;
                    info!("liquidated {mint}: {sig}");
                }
                Ok(None) => info!("liquidate {mint}: dry-run (no tx sent)"),
                Err(e) => error!("liquidate {mint} FAILED: {e}"),
            }
        }
        match onchain::fetch_sol_balance(&rpc, &keypair.pubkey()).await {
            Ok(sol) => info!("liquidate done ({ok}/{} ok); wallet balance: {sol:.5} SOL", list.len()),
            Err(e) => warn!("liquidate done ({ok}/{} ok); balance fetch failed: {e}", list.len()),
        }
        return Ok(());
    }

    // ---- copy-trade mode: mirror followed alpha wallets' live PumpSwap buys ----
    // When enabled, this REPLACES the graduation/dip_buy path entirely. Followed
    // wallet addresses come from the COPY_TRADE_WALLETS env (gitignored), never
    // the committed config.
    if cfg.copy_trade.enabled {
        let wallets: Vec<String> = std::env::var("COPY_TRADE_WALLETS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if wallets.is_empty() {
            error!("copy_trade.enabled but COPY_TRADE_WALLETS is empty; nothing to follow. Exiting.");
            return Ok(());
        }
        info!(
            "COPY-TRADE MODE — following {} wallet(s); size={} SOL, max_positions={}, 24h cap={} SOL, reentry_cooldown={}s",
            wallets.len(),
            cfg.copy_trade.trade_amount_sol,
            cfg.copy_trade.max_positions,
            cfg.copy_trade.daily_spend_cap_sol,
            cfg.copy_trade.reentry_cooldown_secs,
        );

        let copy_positions = Arc::new(AtomicUsize::new(0));
        let spend_tracker: SharedSpendTracker = Arc::new(std::sync::Mutex::new(
            DailySpendTracker::new(cfg.copy_trade.daily_spend_cap_sol),
        ));
        let recent: Arc<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>> =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let recent_wallet: Arc<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>> =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let cooldown = std::time::Duration::from_secs(cfg.copy_trade.reentry_cooldown_secs);
        let wallet_cooldown = std::time::Duration::from_secs(cfg.copy_trade.per_wallet_cooldown_secs);

        let mut trades = pumpportal::spawn_account_trade_listener(ws_url.clone(), wallets);

        // Sell broadcast: every followed-wallet SELL is fanned out to all open
        // copy positions so a position mirror-exits when the alpha that we
        // followed in closes the mint. Each copy_handle filters for its own
        // mint + entry wallet. Capacity is generous — sells are low-frequency.
        let (sell_tx, _sell_rx0) =
            tokio::sync::broadcast::channel::<pumpportal::AlphaTrade>(1024);
        if cfg.copy_trade.mirror_exit {
            info!(
                "MIRROR-EXIT enabled — positions sell when the followed alpha closes the mint (close_fraction={}, first_sell={}, max_hold backstop={}s); trailing/TP disabled",
                cfg.copy_trade.mirror_close_fraction,
                cfg.copy_trade.mirror_exit_on_first_sell,
                cfg.copy_trade.mirror_max_hold_secs,
            );
        }
        // Graceful shutdown: on SIGTERM/Ctrl-C, tell every open copy position to
        // force-sell so we don't orphan tokens when stopping between iterations.
        let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(8);
        info!("hades-ps-trader running. Following alpha buys…");

        let shutdown = shutdown_signal();
        tokio::pin!(shutdown);
        loop {
            let t = tokio::select! {
                maybe = trades.recv() => match maybe {
                    Some(t) => t,
                    None => {
                        error!("PumpPortal account-trade channel closed; exiting.");
                        break;
                    }
                },
                _ = &mut shutdown => {
                    let n = copy_positions.load(std::sync::atomic::Ordering::SeqCst);
                    warn!("shutdown signal — telling {n} open copy position(s) to force-sell; waiting up to 25s");
                    let _ = shutdown_tx.send(());
                    for _ in 0..25 {
                        if copy_positions.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                    let left = copy_positions.load(std::sync::atomic::Ordering::SeqCst);
                    if left > 0 {
                        error!("{left} copy position(s) may not have closed cleanly — check the wallet");
                    } else {
                        info!("all copy positions closed; exiting cleanly");
                    }
                    break;
                }
            };
            if t.tx_type == "sell" {
                // Fan out to any open copy position holding this mint.
                debug!(
                    "sell frame: {} by {} newTokenBalance={:?} pump_swap={}",
                    t.mint, t.trader, t.token_balance, t.is_pumpswap
                );
                let _ = sell_tx.send(t);
                continue;
            }
            if t.tx_type != "buy" {
                continue; // ignore non-trade frames
            }
            if !t.is_pumpswap {
                debug!("copy skip {}: not a PumpSwap (pump-amm) trade", t.mint);
                continue;
            }
            // Per-wallet cooldown — one hyperactive wallet can't dominate the cap.
            {
                let mut wmap = recent_wallet.lock().expect("recent_wallet map poisoned");
                let now = std::time::Instant::now();
                if let Some(&last) = wmap.get(&t.trader) {
                    if now.duration_since(last) < wallet_cooldown {
                        debug!("copy skip {}: wallet {} cooldown", t.mint, t.trader);
                        continue;
                    }
                }
                wmap.insert(t.trader.clone(), now);
            }
            // Per-mint dedup / re-entry cooldown.
            {
                let mut map = recent.lock().expect("recent map poisoned");
                let now = std::time::Instant::now();
                if let Some(&last) = map.get(&t.mint) {
                    if now.duration_since(last) < cooldown {
                        debug!("copy skip {}: re-entry cooldown", t.mint);
                        continue;
                    }
                }
                map.insert(t.mint.clone(), now);
            }
            let cfg2 = cfg.clone();
            let http2 = http.clone();
            let rpc2 = rpc.clone();
            let kp2 = keypair.clone();
            let positions2 = copy_positions.clone();
            let spend2 = spend_tracker.clone();
            let notifier2 = notifier.clone();
            let mirror_rx = sell_tx.subscribe();
            let shutdown_rx = shutdown_tx.subscribe();
            let dry = args.dry_run;
            tokio::spawn(async move {
                copy_session::copy_handle(
                    cfg2, http2, rpc2, kp2, positions2, spend2, notifier2, t, mirror_rx, shutdown_rx, dry,
                )
                .await;
            });
        }
        return Ok(());
    }

    let mut migrations = pumpportal::spawn_migration_listener(ws_url.clone());
    let open_positions = Arc::new(AtomicUsize::new(0));

    // Shared rolling-24h cap on dip_buy spending. Bug protection — refuses
    // new dip_buy fires when cumulative spend over 24h would breach the cap.
    let spend_tracker: SharedSpendTracker = Arc::new(std::sync::Mutex::new(
        DailySpendTracker::new(cfg.dip_buy.daily_spend_cap_sol),
    ));
    if cfg.dip_buy.enabled {
        info!(
            "dip_buy ENABLED — MACD + capitulation buys suppressed; 24h spend cap = {:.2} SOL",
            cfg.dip_buy.daily_spend_cap_sol
        );
    }

    info!("hades-ps-trader running. Waiting for graduations…");

    while let Some(ev) = migrations.recv().await {
        let cfg2 = cfg.clone();
        let http2 = http.clone();
        let rpc2 = rpc.clone();
        let kp2 = keypair.clone();
        let positions2 = open_positions.clone();
        let notifier2 = notifier.clone();
        let spend2 = spend_tracker.clone();
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
            session::handle(cfg2, http2, rpc2, kp2, positions2, notifier2, spend2, ev, dry).await;
        });
    }

    error!("PumpPortal listener channel closed; exiting.");
    Ok(())
}

/// Resolves on the first SIGTERM or Ctrl-C (SIGINT). Used to trigger graceful
/// shutdown of open copy positions before the process exits.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                warn!("could not install SIGTERM handler: {e}; Ctrl-C only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Lowest `gain%` among the configured trailing-stop tiers, or +∞ if none.
fn first_tier_gain(s: &str) -> f64 {
    position::parse_tiers(s)
        .first()
        .map(|t| t.gain_pct)
        .unwrap_or(f64::INFINITY)
}
