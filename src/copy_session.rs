//! Copy-trade session.
//!
//! Triggered when a followed alpha wallet BUYs a PumpSwap mint. We enter
//! immediately (the alpha already did the entry selection that the re-derived
//! dip_buy detector couldn't) and manage the position with our own mechanical
//! exit — trailing stop + stop-loss + time-stop + rug guard — rather than
//! mirroring the alpha's sell, because by the time we observe their exit we'd
//! be last out.
//!
//! Self-contained: reuses the public execution/price/exit primitives but does
//! NOT touch the graduation `session` path, so the existing strategies are
//! unaffected.

use crate::config::Config;
use crate::dip_buy::SharedSpendTracker;
use crate::discord::Notifier;
use crate::onchain;
use crate::position::{self, ExitReason, TrailTier};
use crate::price_feed;
use crate::pumpportal::AlphaTrade;
use crate::rug::RugGuard;
use crate::trader::{self, BuyParams, SellAllParams};
use log::{error, info, warn};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const BALANCE_FETCH_ATTEMPTS: u32 = 20;
const BALANCE_FETCH_INTERVAL: Duration = Duration::from_millis(1000);

fn short(mint: &str) -> String {
    if mint.len() <= 8 {
        mint.to_string()
    } else {
        format!("{}…{}", &mint[..4], &mint[mint.len() - 4..])
    }
}

/// Handle one copied entry end-to-end. Caller has already passed dedup and
/// concurrency-slot gating; this reserves the spend, buys, and monitors to exit.
#[allow(clippy::too_many_arguments)]
pub async fn copy_handle(
    cfg: Arc<Config>,
    http: reqwest::Client,
    rpc: Arc<RpcClient>,
    keypair: Arc<Keypair>,
    copy_positions: Arc<AtomicUsize>,
    spend_tracker: SharedSpendTracker,
    notifier: Notifier,
    alpha: AlphaTrade,
    dry_run: bool,
) {
    let cc = &cfg.copy_trade;
    let mint_str = alpha.mint.clone();
    let symbol = alpha.symbol.clone();
    let tag = short(&mint_str);
    let mint = match Pubkey::from_str(&mint_str) {
        Ok(p) => p,
        Err(e) => {
            warn!("[{tag}] copy: bad mint pubkey: {e}");
            return;
        }
    };
    let amount_sol = cc.trade_amount_sol;

    // 24h spend cap — counts dry-run and live alike so gating is testable.
    {
        let now = Instant::now();
        let mut tracker = spend_tracker.lock().expect("copy spend tracker poisoned");
        if let Err(spent) = tracker.check(now, amount_sol) {
            warn!(
                "[{tag}] copy BLOCKED by daily cap: would spend {amount_sol:.3} SOL but {spent:.3}/{:.2} used in 24h",
                tracker.cap()
            );
            return;
        }
    }

    // Reserve a concurrency slot; release on every failure path.
    let slot = copy_positions.fetch_add(1, Ordering::SeqCst);
    if slot >= cc.max_positions as usize {
        copy_positions.fetch_sub(1, Ordering::SeqCst);
        info!("[{tag}] copy skipped: max_positions={} full", cc.max_positions);
        return;
    }

    info!(
        "[{tag}] COPY ENTRY following {} buy of {} ({}): {amount_sol} SOL",
        short(&alpha.trader),
        mint_str,
        symbol.as_deref().unwrap_or("?"),
    );

    let buy_res = trader::buy(
        &http,
        &rpc,
        &keypair,
        BuyParams {
            mint: &mint_str,
            symbol: symbol.as_deref(),
            amount_sol,
            slippage_pct: cfg.trading.slippage,
            priority_fee_sol: cfg.trading.priority_fee_sol,
            max_retries: cfg.trading.buy_tx_retries,
        },
        dry_run,
    )
    .await;

    // Record toward the cap (dry-run too) so the gate is verifiable.
    {
        let mut tracker = spend_tracker.lock().expect("copy spend tracker poisoned");
        tracker.record(Instant::now(), amount_sol);
        let total = tracker.total_24h(Instant::now());
        info!("[{tag}] copy spend recorded: {amount_sol:.3} SOL (24h {total:.3}/{:.2})", tracker.cap());
    }

    let sig = match buy_res {
        Ok(Some(sig)) => sig,
        Ok(None) => {
            info!("[{tag}] COPY BUY (dry-run) — no real position tracked");
            copy_positions.fetch_sub(1, Ordering::SeqCst);
            return;
        }
        Err(e) => {
            warn!("[{tag}] COPY BUY FAILED: {e}");
            copy_positions.fetch_sub(1, Ordering::SeqCst);
            return;
        }
    };

    let bought_at = Instant::now();
    let balance = match onchain::fetch_token_balance(
        &rpc, &keypair.pubkey(), &mint, BALANCE_FETCH_ATTEMPTS, BALANCE_FETCH_INTERVAL,
    )
    .await
    {
        Ok(b) => b,
        Err(e) => {
            error!("[{tag}] ORPHAN COPY POSITION: post-buy balance fetch failed: {e}. buy tx: {sig}. Sell manually.");
            notifier.notify_orphan_buy(&mint_str, None, symbol.as_deref(), &sig.to_string(), amount_sol, &e.to_string());
            copy_positions.fetch_sub(1, Ordering::SeqCst);
            return;
        }
    };
    let token_ui = balance.ui_amount();
    if token_ui <= 0.0 {
        warn!("[{tag}] post-buy token balance zero; releasing slot");
        copy_positions.fetch_sub(1, Ordering::SeqCst);
        return;
    }
    let entry_price_sol = amount_sol / token_ui;
    info!("[{tag}] COPY BOUGHT: {token_ui} tokens @ {entry_price_sol:.10} SOL/token (cost ~{amount_sol} SOL)");
    notifier.notify_buy(&mint_str, None, symbol.as_deref(), amount_sol, token_ui, entry_price_sol);

    // ----- monitor to exit -----
    let poll_interval = Duration::from_millis(cfg.trading.price_poll_interval_ms);
    let read_timeout = Duration::from_millis(cfg.trading.pumpswap_read_timeout_ms);
    let tiers = position::parse_tiers(&cc.trail_tiers);
    let mut rx = price_feed::spawn_price_poll(rpc.clone(), mint_str.clone(), poll_interval, read_timeout);
    let mut rug_guard = RugGuard::new();
    let mut peak_pct = 0.0_f64;
    let mut last_price: Option<f64> = None;
    let mut tick: u64 = 0;
    let pnl_log_every = cfg.trading.pnl_log_every_n_ticks;

    let deadline = if cc.max_hold_secs > 0 {
        Some(bought_at + Duration::from_secs(cc.max_hold_secs))
    } else {
        None
    };
    let deadline_fut = async {
        if let Some(d) = deadline {
            tokio::time::sleep_until(tokio::time::Instant::from_std(d)).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(deadline_fut);

    let mut sold = false;
    loop {
        tokio::select! {
            biased;
            _ = &mut deadline_fut => {
                let pct = last_price.map(|p| pnl_pct(entry_price_sol, p)).unwrap_or(0.0);
                info!("[{tag}] COPY max_hold reached — force-selling at pnl={pct:+.2}%");
                let _ = do_sell(&cfg, &http, &rpc, &keypair, &notifier, &mint_str, symbol.as_deref(),
                                amount_sol, pct, "Copy max hold", &tag, dry_run).await;
                sold = true;
                break;
            }
            maybe = rx.recv() => {
                let Some(update) = maybe else { warn!("[{tag}] copy price feed ended"); break; };
                let price = update.price_sol;
                last_price = Some(price);
                tick += 1;
                rug_guard.observe(price, update.pool_sol);

                if let Some(reason) = rug_guard.is_rugged(&cfg.rug, price, update.pool_sol) {
                    let pct = pnl_pct(entry_price_sol, price);
                    warn!("[{tag}] COPY RUG — {reason}; force-selling at pnl={pct:+.2}%");
                    let _ = do_sell(&cfg, &http, &rpc, &keypair, &notifier, &mint_str, symbol.as_deref(),
                                    amount_sol, pct, &format!("Rug detected: {reason}"), &tag, dry_run).await;
                    sold = true;
                    break;
                }

                let pct = pnl_pct(entry_price_sol, price);
                if pct > peak_pct { peak_pct = pct; }
                if pnl_log_every > 0 && tick % pnl_log_every == 0 {
                    info!("PNL MONITOR (copy): {} {pct:+.2}%", display(&mint_str, symbol.as_deref()));
                }
                let elapsed = bought_at.elapsed().as_secs();
                if let Some(reason) = copy_exit(peak_pct, pct, elapsed, cc, &tiers) {
                    info!("[{tag}] COPY EXIT: {} (price={price:.10} pnl={pct:+.2}% peak={peak_pct:+.2}%)", reason.display());
                    match do_sell(&cfg, &http, &rpc, &keypair, &notifier, &mint_str, symbol.as_deref(),
                                  amount_sol, pct, &reason.display(), &tag, dry_run).await {
                        Ok(()) => { sold = true; break; }
                        Err(_) => warn!("[{tag}] copy sell failed — retry next tick"),
                    }
                }
            }
        }
    }

    if !sold {
        let pct = last_price.map(|p| pnl_pct(entry_price_sol, p)).unwrap_or(0.0);
        warn!("[{tag}] copy session ending with open position — force-selling");
        let _ = do_sell(&cfg, &http, &rpc, &keypair, &notifier, &mint_str, symbol.as_deref(),
                        amount_sol, pct, "Copy forced exit", &tag, dry_run).await;
    }
    copy_positions.fetch_sub(1, Ordering::SeqCst);
    info!("[{tag}] copy session ended");
}

fn pnl_pct(entry: f64, current: f64) -> f64 {
    if entry <= 0.0 { return 0.0; }
    ((current - entry) / entry) * 100.0
}

fn display(mint: &str, symbol: Option<&str>) -> String {
    match symbol {
        Some(s) => format!("{mint} ({s})"),
        None => mint.to_string(),
    }
}

/// Copy-position exit: time-stop, then hard stop-loss, then dynamic trailing
/// stop, then a high take-profit ceiling. Mirrors the dip_buy exit shape.
fn copy_exit(
    peak_pct: f64,
    current_pct: f64,
    elapsed_secs: u64,
    cc: &crate::config::CopyTradeConfig,
    tiers: &[TrailTier],
) -> Option<ExitReason> {
    if cc.max_hold_secs > 0 && elapsed_secs >= cc.max_hold_secs {
        return Some(ExitReason::MaxHoldTime { elapsed_secs, current_pct });
    }
    if cc.stop_loss_pct > 0.0 && current_pct <= -cc.stop_loss_pct {
        return Some(ExitReason::StopLoss { pct: current_pct });
    }
    if let Some(tier) = tiers.iter().rev().find(|t| peak_pct >= t.gain_pct) {
        let trigger = peak_pct - tier.trail_pct;
        if current_pct <= trigger {
            return Some(ExitReason::DynamicTrail {
                tier_gain: tier.gain_pct,
                trail: tier.trail_pct,
                trigger_at_pct: trigger,
                current_pct,
            });
        }
    }
    if cc.take_profit_pct > 0.0 && current_pct >= cc.take_profit_pct {
        return Some(ExitReason::TakeProfit { pct: current_pct });
    }
    None
}

#[allow(clippy::too_many_arguments)]
async fn do_sell(
    cfg: &Config,
    http: &reqwest::Client,
    rpc: &RpcClient,
    keypair: &Keypair,
    notifier: &Notifier,
    mint_str: &str,
    symbol: Option<&str>,
    cost_sol: f64,
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
            mint: mint_str,
            symbol,
            slippage_pct: cfg.trading.slippage,
            priority_fee_sol: cfg.trading.priority_fee_sol,
            max_retries: cfg.trading.sell_tx_retries,
        },
        dry_run,
    )
    .await;
    match res {
        Ok(Some(_)) => {
            let net_return_sol = cost_sol * pct / 100.0;
            info!("[{tag}] COPY SOLD ({reason}) pnl={pct:+.2}%");
            match onchain::fetch_sol_balance(rpc, &keypair.pubkey()).await {
                Ok(sol) => info!("[{tag}] wallet balance: {sol:.5} SOL"),
                Err(e) => warn!("[{tag}] could not fetch wallet balance after sell: {e}"),
            }
            notifier.notify_sell(mint_str, None, symbol, reason, pct, net_return_sol);
            Ok(())
        }
        Ok(None) => {
            info!("[{tag}] COPY SELL (dry-run) {reason}");
            Ok(())
        }
        Err(e) => {
            warn!("[{tag}] COPY SELL FAILED ({reason}): {e}");
            Err(e)
        }
    }
}
