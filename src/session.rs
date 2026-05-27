//! Per-graduation trading session.
//!
//! Flow:
//!   1. Run trading filters (if enabled) once at graduation. Reject → no session.
//!   2. Spawn the price-poll task and start aggregating candles.
//!   3. On every price tick, run the rug guard. If it fires, force-sell any
//!      open position and abandon the session.
//!   4. If `[capitulation].enabled`, run the capitulation detector per tick;
//!      a fired signal triggers a buy (subject to no-pyramiding, cooldown,
//!      max_positions). MACD candle aggregation still runs for logging but
//!      bullish crossovers no longer fire buys.
//!   5. Otherwise, on each candle close, feed the close to MACD.
//!        - Bullish crossover, no open position, cooldown elapsed, slot
//!          available → buy.
//!        - Bearish crossover → no action (exits are PnL-driven).
//!   6. When `max_monitor_time` elapses, stop accepting new signals and
//!      force-sell any open position before returning.

use crate::capitulation::CapitulationDetector;
use crate::config::Config;
use crate::discord::Notifier;
use crate::macd::{CandleAggregator, Crossover, Macd, MacdEvent};

use crate::onchain;
use crate::position::{self, Position};
use crate::price_feed;
use crate::pumpportal::MigrationEvent;
use crate::rug::RugGuard;
use crate::trader::{self, BuyParams, SellAllParams};
use log::{debug, error, info, warn};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const LAMPORTS_PER_SOL: u64 = 1_000_000_000;
const BALANCE_FETCH_ATTEMPTS: u32 = 20;
const BALANCE_FETCH_INTERVAL: Duration = Duration::from_millis(1000);

pub async fn handle(
    cfg: Arc<Config>,
    http: reqwest::Client,
    rpc: Arc<RpcClient>,
    keypair: Arc<Keypair>,
    open_positions: Arc<AtomicUsize>,
    notifier: Notifier,
    event: MigrationEvent,
    dry_run: bool,
) {
    let MigrationEvent {
        mint: mint_str,
        name,
        symbol,
    } = event;
    let tag = short(&mint_str);
    let mint = match Pubkey::from_str(&mint_str) {
        Ok(p) => p,
        Err(e) => {
            warn!("[{tag}] bad mint pubkey: {e}");
            return;
        }
    };

    if cfg.tradingfilters.enabled {
        if !run_filters(&cfg, &rpc, &mint, &tag).await {
            return;
        }
    } else {
        info!("[{tag}] filters disabled — starting monitor directly");
    }

    info!("[{tag}] watch: \x1b[1;4;97mhttps://pump.fun/coin/{mint_str}\x1b[0m");
    info!(
        "[{tag}] MACD session: candle={}s fast={} slow={} signal={} cooldown={}s deadline={}",
        cfg.macd.candle_interval_secs,
        cfg.macd.fast,
        cfg.macd.slow,
        cfg.macd.signal,
        cfg.macd.cooldown_secs,
        match cfg.trading.max_monitor_time {
            0 => "none".to_string(),
            n => format!("{n}s"),
        }
    );

    run_session(
        cfg,
        http,
        rpc,
        keypair,
        open_positions,
        notifier,
        mint,
        mint_str,
        name,
        symbol,
        tag,
        dry_run,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn run_session(
    cfg: Arc<Config>,
    http: reqwest::Client,
    rpc: Arc<RpcClient>,
    keypair: Arc<Keypair>,
    open_positions: Arc<AtomicUsize>,
    notifier: Notifier,
    mint: Pubkey,
    mint_str: String,
    name: Option<String>,
    symbol: Option<String>,
    tag: String,
    dry_run: bool,
) {
    let session_start = Instant::now();
    let deadline = match cfg.trading.max_monitor_time {
        0 => None,
        n => Some(session_start + Duration::from_secs(n)),
    };
    let poll_interval = Duration::from_millis(cfg.trading.price_poll_interval_ms);
    let read_timeout = Duration::from_millis(cfg.trading.pumpswap_read_timeout_ms);
    let candle_interval = Duration::from_secs(cfg.macd.candle_interval_secs);
    let cooldown = Duration::from_secs(cfg.macd.cooldown_secs);
    let pnl_log_every = cfg.trading.pnl_log_every_n_ticks;

    let mut rx = price_feed::spawn_price_poll(rpc.clone(), mint_str.clone(), poll_interval, read_timeout);
    let mut aggregator = CandleAggregator::new(candle_interval);
    let mut macd = Macd::new(cfg.macd.fast, cfg.macd.slow, cfg.macd.signal);
    let tiers = position::parse_tiers(&cfg.trading.dynamic_trailing_stop_thresholds);

    let mut position: Option<Position> = None;
    let mut last_sell_at: Option<Instant> = None;
    let mut last_price: Option<f64> = None;
    let mut tick: u64 = 0;
    let mut rug_guard = RugGuard::new();
    let mut cap_detector = CapitulationDetector::new(&cfg.capitulation);
    if cap_detector.enabled() {
        info!(
            "[{tag}] capitulation entry ENABLED — MACD bullish crossovers will not trigger buys"
        );
    }

    let deadline_fut = async {
        if let Some(d) = deadline {
            tokio::time::sleep_until(tokio::time::Instant::from_std(d)).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(deadline_fut);

    loop {
        tokio::select! {
            biased;
            _ = &mut deadline_fut => {
                info!("[{tag}] max_monitor_time reached — closing session");
                break;
            }
            maybe = rx.recv() => {
                let Some(update) = maybe else {
                    warn!("[{tag}] price feed ended unexpectedly");
                    break;
                };
                let price = update.price_sol;
                let pool_sol = update.pool_sol;
                last_price = Some(price);
                tick += 1;
                rug_guard.observe(price, pool_sol);

                // Per-tick rug detection — abandon the session the moment
                // pool liquidity drains or price collapses past the configured
                // floors. Any open position is force-sold first; the loop then
                // exits so the bot stops polling a dead token.
                if let Some(reason) = rug_guard.is_rugged(&cfg.rug, price, pool_sol) {
                    if let Some(pos) = position.take() {
                        let pct = pos.pnl_pct(price);
                        warn!(
                            "[{tag}] RUG DETECTED — {reason}; force-selling open position at pnl={pct:+.2}%"
                        );
                        let sell_reason = format!("Rug detected: {reason}");
                        match try_sell(
                            &cfg, &http, &rpc, &keypair, &notifier,
                            &pos, pct, &sell_reason, &tag, dry_run,
                        ).await {
                            Ok(()) => {
                                open_positions.fetch_sub(1, Ordering::SeqCst);
                            }
                            Err(e) => {
                                error!(
                                    "[{tag}] RUG-TRIGGERED SELL FAILED: {e}; position still on-chain. \
                                     Sell manually. mint={mint_str}"
                                );
                                open_positions.fetch_sub(1, Ordering::SeqCst);
                            }
                        }
                    } else {
                        info!("[{tag}] RUG DETECTED — {reason}; abandoning session");
                    }
                    break;
                }

                // PnL-based exit — evaluated on every price tick while a
                // position is open: stop-loss, take-profit, dynamic trailing
                // stop, max_hold_time. Mirrors hades-ps-sniper's monitor.
                let exit = if let Some(pos) = position.as_mut() {
                    let pct = pos.pnl_pct(price);
                    pos.update_peak(pct);
                    if pnl_log_every > 0 && tick % pnl_log_every == 0 {
                        info!(
                            "PNL MONITOR: {} {pct:+.2}%",
                            display_label(&mint_str, symbol.as_deref())
                        );
                    }
                    let elapsed_secs = pos.bought_at.elapsed().as_secs();
                    position::decide_exit(pos, pct, elapsed_secs, &cfg.trading, &tiers)
                        .map(|reason| (reason, pct, pos.peak_pct))
                } else {
                    None
                };
                if let Some((reason, pct, peak)) = exit {
                    let pos = position.take().expect("position present for exit");
                    info!(
                        "[{tag}] EXIT: {} (price={price:.10} pnl={pct:+.2}% peak={peak:+.2}%)",
                        reason.display()
                    );
                    match try_sell(
                        &cfg, &http, &rpc, &keypair, &notifier,
                        &pos, pct, &reason.display(), &tag, dry_run,
                    )
                    .await
                    {
                        Ok(()) => {
                            open_positions.fetch_sub(1, Ordering::SeqCst);
                            last_sell_at = Some(Instant::now());
                        }
                        Err(_) => {
                            warn!("[{tag}] sell failed — keeping position open for next tick");
                            position = Some(pos);
                        }
                    }
                }

                debug!("[{tag}] tick price={:.10}", price);

                let now = Instant::now();

                // Capitulation dip-buy detector — alternative entry mode to
                // MACD. Per-tick: observe, then check; on fire, run the same
                // buy gating as the MACD bullish branch.
                if cap_detector.enabled() {
                    cap_detector.observe(now, price);
                    if let Some(reason) = cap_detector.check(now, price) {
                        if position.is_some() {
                            debug!("[{tag}] capitulation skipped: already in position");
                        } else if let Some(t) = last_sell_at {
                            let since = now.duration_since(t);
                            if since < cooldown {
                                debug!(
                                    "[{tag}] capitulation skipped: cooldown {}s remaining",
                                    (cooldown - since).as_secs()
                                );
                            } else {
                                info!("[{tag}] CAPITULATION SIGNAL: {reason}");
                                position = try_buy(
                                    &cfg, &http, &rpc, &keypair, &open_positions,
                                    &notifier, &mint, &mint_str, name.as_deref(),
                                    symbol.as_deref(), &tag, dry_run,
                                ).await;
                            }
                        } else {
                            info!("[{tag}] CAPITULATION SIGNAL: {reason}");
                            position = try_buy(
                                &cfg, &http, &rpc, &keypair, &open_positions,
                                &notifier, &mint, &mint_str, name.as_deref(),
                                symbol.as_deref(), &tag, dry_run,
                            ).await;
                        }
                    }
                }

                let Some(close) = aggregator.on_tick(price, now) else {
                    continue;
                };

                match macd.on_close(close) {
                    MacdEvent::Warmup => debug!(
                        "[{tag}] candle close={close:.10} (MACD warming up)"
                    ),
                    MacdEvent::NoCross(s) => debug!(
                        "[{tag}] candle close={close:.10} macd={:.10} signal={:.10} hist={:+.10}",
                        s.macd, s.signal, s.histogram
                    ),
                    MacdEvent::Crossover { snapshot, direction } => {
                        info!(
                            "[{tag}] {} CROSSOVER macd={:.10} signal={:.10} hist={:+.10}",
                            match direction { Crossover::Bullish => "BULLISH", Crossover::Bearish => "BEARISH" },
                            snapshot.macd, snapshot.signal, snapshot.histogram
                        );
                        match direction {
                            Crossover::Bullish => {
                                if cap_detector.enabled() {
                                    debug!(
                                        "[{tag}] bullish ignored: capitulation entry mode is active"
                                    );
                                } else if position.is_some() {
                                    debug!("[{tag}] bullish ignored: already in position");
                                } else if let Some(t) = last_sell_at {
                                    let since = now.duration_since(t);
                                    if since < cooldown {
                                        let left = cooldown - since;
                                        debug!(
                                            "[{tag}] bullish skipped: cooldown {}s remaining",
                                            left.as_secs()
                                        );
                                    } else {
                                        position = try_buy(
                                            &cfg, &http, &rpc, &keypair, &open_positions,
                                            &notifier, &mint, &mint_str, name.as_deref(),
                                            symbol.as_deref(), &tag, dry_run,
                                        ).await;
                                    }
                                } else {
                                    position = try_buy(
                                        &cfg, &http, &rpc, &keypair, &open_positions,
                                        &notifier, &mint, &mint_str, name.as_deref(),
                                        symbol.as_deref(), &tag, dry_run,
                                    ).await;
                                }
                            }
                            Crossover::Bearish => {
                                // Exits are PnL-driven (stop-loss / take-profit
                                // / trailing stop / max_hold_time), evaluated
                                // per price tick. A bearish MACD crossover no
                                // longer triggers a sell.
                                debug!("[{tag}] bearish crossover — no action (PnL-driven exits)");
                            }
                        }
                    }
                }
            }
        }
    }

    // Force-sell anything still open before ending the session.
    if let Some(pos) = position.take() {
        let pct = last_price.map(|p| pos.pnl_pct(p)).unwrap_or(0.0);
        warn!("[{tag}] session ending with open position — force-selling");
        match try_sell(
            &cfg,
            &http,
            &rpc,
            &keypair,
            &notifier,
            &pos,
            pct,
            "Forced exit (max_monitor_time)",
            &tag,
            dry_run,
        )
        .await
        {
            Ok(()) => {
                open_positions.fetch_sub(1, Ordering::SeqCst);
            }
            Err(e) => {
                error!(
                    "[{tag}] FORCED SELL FAILED at session end ({e}); position still on-chain. \
                     Sell manually. mint={mint_str}"
                );
                open_positions.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    info!("[{tag}] session ended");
}

#[allow(clippy::too_many_arguments)]
async fn try_buy(
    cfg: &Config,
    http: &reqwest::Client,
    rpc: &RpcClient,
    keypair: &Keypair,
    open_positions: &AtomicUsize,
    notifier: &Notifier,
    mint: &Pubkey,
    mint_str: &str,
    name: Option<&str>,
    symbol: Option<&str>,
    tag: &str,
    dry_run: bool,
) -> Option<Position> {
    // Reserve a slot first; release on any failure path.
    let slot = open_positions.fetch_add(1, Ordering::SeqCst);
    if slot >= cfg.trading.max_positions as usize {
        open_positions.fetch_sub(1, Ordering::SeqCst);
        info!(
            "[{tag}] bullish skipped: max_positions={} full",
            cfg.trading.max_positions
        );
        return None;
    }

    let amount_sol = cfg.trading.trade_amount as f64 / LAMPORTS_PER_SOL as f64;
    let buy_res = trader::buy(
        http,
        rpc,
        keypair,
        BuyParams {
            mint: mint_str,
            symbol,
            amount_sol,
            slippage_pct: cfg.trading.slippage,
            priority_fee_sol: cfg.trading.priority_fee_sol,
            max_retries: cfg.trading.buy_tx_retries,
        },
        dry_run,
    )
    .await;

    let sig = match buy_res {
        Ok(Some(sig)) => sig,
        Ok(None) => {
            info!("[{tag}] BUY (dry-run) — no real position tracked");
            open_positions.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Err(e) => {
            warn!("[{tag}] BUY FAILED: {e}");
            open_positions.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
    };
    // max_hold_time clock starts the moment the buy tx confirms.
    let bought_at = Instant::now();

    let balance = match onchain::fetch_token_balance(
        rpc,
        &keypair.pubkey(),
        mint,
        BALANCE_FETCH_ATTEMPTS,
        BALANCE_FETCH_INTERVAL,
    )
    .await
    {
        Ok(b) => b,
        Err(e) => {
            error!(
                "[{tag}] ORPHAN POSITION: post-buy balance fetch failed: {e}. \
                 buy tx: {sig}. Sell manually."
            );
            notifier.notify_orphan_buy(
                mint_str,
                name,
                symbol,
                &sig.to_string(),
                amount_sol,
                &e.to_string(),
            );
            open_positions.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
    };

    let token_ui = balance.ui_amount();
    if token_ui <= 0.0 {
        warn!("[{tag}] post-buy token balance is zero; releasing slot");
        open_positions.fetch_sub(1, Ordering::SeqCst);
        return None;
    }
    let entry_price_sol = amount_sol / token_ui;

    info!(
        "[{tag}] BOUGHT: {token_ui} tokens @ {entry_price_sol:.10} SOL/token (cost ~{amount_sol} SOL)"
    );
    notifier.notify_buy(
        mint_str,
        name,
        symbol,
        amount_sol,
        token_ui,
        entry_price_sol,
    );

    Some(Position {
        mint: mint_str.to_string(),
        name: name.map(str::to_string),
        symbol: symbol.map(str::to_string),
        entry_price_sol,
        cost_sol: amount_sol,
        peak_pct: 0.0,
        bought_at,
    })
}

#[allow(clippy::too_many_arguments)]
async fn try_sell(
    cfg: &Config,
    http: &reqwest::Client,
    rpc: &RpcClient,
    keypair: &Keypair,
    notifier: &Notifier,
    pos: &Position,
    pct: f64,
    reason: &str,
    tag: &str,
    dry_run: bool,
) -> Result<(), crate::config::BoxError> {
    let res = trader::sell_all(
        http,
        rpc,
        keypair,
        SellAllParams {
            mint: &pos.mint,
            symbol: pos.symbol.as_deref(),
            slippage_pct: cfg.trading.slippage,
            priority_fee_sol: cfg.trading.priority_fee_sol,
            max_retries: cfg.trading.sell_tx_retries,
        },
        dry_run,
    )
    .await;

    match res {
        Ok(Some(_sig)) => {
            let net_return_sol = pos.cost_sol * pct / 100.0;
            info!("[{tag}] SOLD ({reason}) pnl={pct:+.2}%");
            // Post-sell wallet balance — lets a run's gross PnL be read off the log.
            match onchain::fetch_sol_balance(rpc, &keypair.pubkey()).await {
                Ok(sol) => info!("[{tag}] wallet balance: {sol:.5} SOL"),
                Err(e) => warn!("[{tag}] could not fetch wallet balance after sell: {e}"),
            }
            notifier.notify_sell(
                &pos.mint,
                pos.name.as_deref(),
                pos.symbol.as_deref(),
                reason,
                pct,
                net_return_sol,
            );
            Ok(())
        }
        Ok(None) => {
            info!("[{tag}] SELL (dry-run) {reason}");
            Ok(())
        }
        Err(e) => {
            warn!("[{tag}] SELL FAILED ({reason}): {e}");
            Err(e)
        }
    }
}

async fn run_filters(cfg: &Config, rpc: &RpcClient, mint: &Pubkey, tag: &str) -> bool {
    let f = &cfg.tradingfilters;

    if f.min_holders > 0 {
        match onchain::holder_count(rpc, mint).await {
            Ok(n) => {
                info!("[{tag}] holders={n} (min={})", f.min_holders);
                if n < f.min_holders {
                    info!("[{tag}] REJECT: holders below threshold");
                    return false;
                }
            }
            Err(e) => {
                warn!("[{tag}] holder fetch failed ({e}); rejecting conservatively");
                return false;
            }
        }
    }

    if f.min_txs > 0 {
        match onchain::bonding_curve_tx_count(rpc, mint).await {
            Ok(activity) => {
                info!(
                    "[{tag}] bonding-curve txs={} (min={})",
                    activity.display(),
                    f.min_txs
                );
                if activity.count < f.min_txs {
                    info!("[{tag}] REJECT: tx count below threshold");
                    return false;
                }
            }
            Err(e) => {
                warn!("[{tag}] tx count fetch failed ({e}); rejecting conservatively");
                return false;
            }
        }
    }

    if f.top_ten_holder_percentage > 0.0 {
        match onchain::top_n_holder_concentration(rpc, mint, 10).await {
            Ok(frac) => {
                info!(
                    "[{tag}] top10 concentration={:.2}% (max={:.2}%)",
                    frac * 100.0,
                    f.top_ten_holder_percentage * 100.0,
                );
                if frac > f.top_ten_holder_percentage {
                    info!("[{tag}] REJECT: top10 concentration above threshold");
                    return false;
                }
            }
            Err(e) => {
                warn!("[{tag}] top10 concentration fetch failed ({e}); rejecting conservatively");
                return false;
            }
        }
    }

    true
}

fn short(mint: &str) -> String {
    if mint.len() > 8 {
        format!("{}…{}", &mint[..4], &mint[mint.len() - 4..])
    } else {
        mint.to_string()
    }
}

fn display_label(mint: &str, symbol: Option<&str>) -> String {
    match symbol {
        Some(s) => format!("{mint} ({s})"),
        None => mint.to_string(),
    }
}
