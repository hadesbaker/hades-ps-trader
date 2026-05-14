//! Per-graduation MACD trading session.
//!
//! Flow:
//!   1. Run trading filters (if enabled) once at graduation. Reject → no session.
//!   2. Spawn the price-poll task and start aggregating candles.
//!   3. On each candle close, feed the close to MACD.
//!        - Bullish crossover, no open position, cooldown elapsed, slot
//!          available → buy.
//!        - Bearish crossover with an open position → sell.
//!   4. When `max_monitor_time` elapses, stop accepting new signals and
//!      force-sell any open position before returning.

use crate::config::Config;
use crate::discord::Notifier;
use crate::macd::{CandleAggregator, Crossover, Macd, MacdEvent};

use crate::onchain;
use crate::position::Position;
use crate::price_feed;
use crate::pumpportal::MigrationEvent;
use crate::trader::{self, BuyParams, SellAllParams};
use log::{debug, error, info, warn};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
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
    let MigrationEvent { mint: mint_str, name, symbol } = event;
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
        cfg, http, rpc, keypair, open_positions, notifier,
        mint, mint_str, name, symbol, tag, dry_run,
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
    let candle_interval = Duration::from_secs(cfg.macd.candle_interval_secs);
    let cooldown = Duration::from_secs(cfg.macd.cooldown_secs);
    let pnl_log_every = cfg.trading.pnl_log_every_n_ticks;

    let mut rx = price_feed::spawn_price_poll(rpc.clone(), mint_str.clone(), poll_interval);
    let mut aggregator = CandleAggregator::new(candle_interval);
    let mut macd = Macd::new(cfg.macd.fast, cfg.macd.slow, cfg.macd.signal);

    let mut position: Option<Position> = None;
    let mut last_sell_at: Option<Instant> = None;
    let mut last_price: Option<f64> = None;
    let mut tick: u64 = 0;

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
                last_price = Some(price);
                tick += 1;

                if let Some(pos) = &position {
                    if pnl_log_every > 0 && tick % pnl_log_every == 0 {
                        info!(
                            "PNL MONITOR: {} {:+.2}%",
                            display_label(&mint_str, symbol.as_deref()),
                            pos.pnl_pct(price)
                        );
                    }
                }
                debug!("[{tag}] tick price={:.10}", price);

                let now = Instant::now();
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
                                if position.is_some() {
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
                                if let Some(pos) = position.take() {
                                    let pct = pos.pnl_pct(price);
                                    match try_sell(
                                        &cfg, &http, &rpc, &keypair, &notifier,
                                        &pos, pct, "MACD bearish crossover", &tag, dry_run,
                                    ).await {
                                        Ok(()) => {
                                            open_positions.fetch_sub(1, Ordering::SeqCst);
                                            last_sell_at = Some(Instant::now());
                                        }
                                        Err(_) => {
                                            warn!(
                                                "[{tag}] sell failed — keeping position open for next signal"
                                            );
                                            position = Some(pos);
                                        }
                                    }
                                } else {
                                    debug!("[{tag}] bearish ignored: no open position");
                                }
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
            &cfg, &http, &rpc, &keypair, &notifier,
            &pos, pct, "Forced exit (max_monitor_time)", &tag, dry_run,
        ).await {
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
        http, rpc, keypair,
        BuyParams {
            mint: mint_str,
            symbol,
            amount_sol,
            slippage_pct: cfg.trading.max_slippage,
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
                mint_str, name, symbol, &sig.to_string(), amount_sol, &e.to_string(),
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
    notifier.notify_buy(mint_str, name, symbol, amount_sol, token_ui, entry_price_sol);

    Some(Position {
        mint: mint_str.to_string(),
        name: name.map(str::to_string),
        symbol: symbol.map(str::to_string),
        entry_price_sol,
        cost_sol: amount_sol,
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
        http, rpc, keypair,
        SellAllParams {
            mint: &pos.mint,
            symbol: pos.symbol.as_deref(),
            slippage_pct: cfg.trading.max_slippage,
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
            notifier.notify_sell(
                &pos.mint, pos.name.as_deref(), pos.symbol.as_deref(),
                reason, pct, net_return_sol,
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
