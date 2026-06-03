//! Dip-buy entry detector + daily safety cap.
//!
//! Fires when, over the last `window_secs`, the PumpSwap pool has experienced:
//!   - a price drawdown ≥ `min_drawdown_pct` from window max
//!   - the current price sits at quantile ≤ `max_entry_quantile` of the
//!     window's [min, max] range (i.e. near the local low)
//!   - the pool WSOL TVL has drained ≥ `min_pool_sol_drawdown_pct` from
//!     window max (confirms real sells, not a single fat seller)
//!   - the pool currently holds ≥ `min_pool_sol` (mature pool, not micro)
//!
//! Empirical basis: validated against 142 reconstructed entries from
//! `hades-wallet-monitor`. Among winners (n=123), the criteria matched
//! 35% (43 positions). Among losers (n=19), the criteria matched 0/19
//! after tightening pool_sol_drawdown threshold from 0.15 → 0.20.
//! See `hades-wallet-monitor/analysis/output/strategy_draft.md` and
//! `validate_dip_buy.py` for the underlying analysis.
//!
//! Daily-spend safety cap protects against bug-induced runaway firing:
//! the `DailySpendTracker` tracks dip_buy spending over a rolling 24h
//! window and refuses new fires when cumulative spend would breach the cap.

use crate::config::DipBuyConfig;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MIN_TICKS_FOR_FIRE: usize = 5;

#[derive(Debug)]
pub struct DipBuyDetector {
    enabled: bool,
    window: Duration,
    min_drawdown_pct: f64,
    max_entry_quantile: f64,
    min_pool_sol_drawdown_pct: f64,
    max_pool_sol_drawdown_pct: f64,
    min_pool_sol: f64,
    cooldown: Duration,
    // Pre-buy rug filter: pool must show TVL recovery over the recent window.
    pool_recovery_secs: u64,
    pool_recovery_pct: f64,
    // Pre-buy rug filter: price must show bounce off recent low.
    price_recovery_secs: u64,
    price_recovery_pct: f64,
    // Session-age filter: reject fires past this many seconds into the session.
    // Yesterday's data: all 8 winners ≤643s, 4/11 losers >643s including the
    // -88% rug at 5261s. Set 0 to disable.
    max_session_age_secs: u64,
    session_start: Instant,

    // (timestamp, price_sol, pool_sol)
    ticks: VecDeque<(Instant, f64, f64)>,
    last_fire: Option<Instant>,
}

impl DipBuyDetector {
    pub fn new(cfg: &DipBuyConfig, session_start: Instant) -> Self {
        Self {
            enabled: cfg.enabled,
            window: Duration::from_secs(cfg.window_secs),
            min_drawdown_pct: cfg.min_drawdown_pct,
            max_entry_quantile: cfg.max_entry_quantile,
            min_pool_sol_drawdown_pct: cfg.min_pool_sol_drawdown_pct,
            max_pool_sol_drawdown_pct: cfg.max_pool_sol_drawdown_pct,
            min_pool_sol: cfg.min_pool_sol,
            cooldown: Duration::from_secs(cfg.cooldown_secs),
            pool_recovery_secs: cfg.pool_recovery_secs,
            pool_recovery_pct: cfg.pool_recovery_pct,
            price_recovery_secs: cfg.price_recovery_secs,
            price_recovery_pct: cfg.price_recovery_pct,
            max_session_age_secs: cfg.max_session_age_secs,
            session_start,
            ticks: VecDeque::new(),
            last_fire: None,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    fn evict_old(&mut self, now: Instant) {
        while let Some(&(ts, _, _)) = self.ticks.front() {
            if now.duration_since(ts) > self.window {
                self.ticks.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn observe(&mut self, now: Instant, price: f64, pool_sol: f64) {
        if !self.enabled {
            return;
        }
        self.evict_old(now);
        self.ticks.push_back((now, price, pool_sol));
    }

    /// `Some(reason)` if all dip-buy conditions hold AND we're past the
    /// post-fire cooldown. Resets the cooldown on fire.
    pub fn check(&mut self, now: Instant) -> Option<String> {
        if !self.enabled {
            return None;
        }
        if let Some(t) = self.last_fire {
            if now.duration_since(t) < self.cooldown {
                return None;
            }
        }
        // Session-age filter — late fires (post-discovery, post-pump) are
        // empirically the worst rug risk. Yesterday's -88% rug fired 88
        // minutes after the session started.
        if self.max_session_age_secs > 0 {
            let age = now.duration_since(self.session_start).as_secs();
            if age > self.max_session_age_secs {
                return None;
            }
        }
        if self.ticks.len() < MIN_TICKS_FOR_FIRE {
            return None;
        }

        // Require the window to be meaningfully full before judging, so a
        // freshly-started session doesn't compare against its first few ticks.
        let &(oldest_ts, _, _) = self.ticks.front()?;
        let needed = self.window.saturating_sub(Duration::from_secs(1));
        if now.duration_since(oldest_ts) < needed {
            return None;
        }

        let (_, entry_price, entry_pool_sol) = *self.ticks.back()?;
        if entry_pool_sol < self.min_pool_sol {
            return None;
        }

        let max_price = self.ticks.iter().map(|t| t.1).fold(f64::MIN, f64::max);
        let min_price = self.ticks.iter().map(|t| t.1).fold(f64::MAX, f64::min);
        let max_pool_sol = self.ticks.iter().map(|t| t.2).fold(f64::MIN, f64::max);

        if max_price <= 0.0 || max_pool_sol <= 0.0 || (max_price - min_price).abs() < f64::EPSILON {
            return None;
        }

        let drawdown_pct = (max_price - entry_price) / max_price;
        let quantile = (entry_price - min_price) / (max_price - min_price);
        let pool_drawdown_pct = (max_pool_sol - entry_pool_sol) / max_pool_sol;

        // Core dip criteria — same as before.
        if !(drawdown_pct >= self.min_drawdown_pct
            && quantile <= self.max_entry_quantile
            && pool_drawdown_pct >= self.min_pool_sol_drawdown_pct)
        {
            return None;
        }

        // Pre-buy rug filter #1: cap on how badly the pool has drained.
        // Beyond this, the pool isn't dipping — it's dying. The 6 rugs from
        // the first live run all had pool_drain ≥ 0.20 at fire AND went on
        // to drain further toward zero.
        if self.max_pool_sol_drawdown_pct > 0.0
            && pool_drawdown_pct > self.max_pool_sol_drawdown_pct
        {
            return None;
        }

        // Pre-buy rug filter #2: require pool TVL to have bounced off its
        // recent low. If the pool is still draining at fire time, we're
        // catching a falling knife — exactly the failure mode we saw live.
        if self.pool_recovery_secs > 0 {
            let recovery_window = Duration::from_secs(self.pool_recovery_secs);
            let cutoff = now.checked_sub(recovery_window);
            let mut min_pool_recent = f64::INFINITY;
            for &(ts, _, ps) in &self.ticks {
                if cutoff.map_or(true, |c| ts >= c) && ps < min_pool_recent {
                    min_pool_recent = ps;
                }
            }
            if min_pool_recent.is_finite()
                && entry_pool_sol < min_pool_recent * (1.0 + self.pool_recovery_pct)
            {
                return None;
            }
        }

        // Pre-buy rug filter #3: require price to have bounced off its
        // recent low. Together with pool recovery, this is the "wait for
        // reclaim" pattern from the capitulation detector.
        if self.price_recovery_secs > 0 {
            let recovery_window = Duration::from_secs(self.price_recovery_secs);
            let cutoff = now.checked_sub(recovery_window);
            let mut min_price_recent = f64::INFINITY;
            for &(ts, p, _) in &self.ticks {
                if cutoff.map_or(true, |c| ts >= c) && p < min_price_recent {
                    min_price_recent = p;
                }
            }
            if min_price_recent.is_finite()
                && entry_price < min_price_recent * (1.0 + self.price_recovery_pct)
            {
                return None;
            }
        }

        self.last_fire = Some(now);
        Some(format!(
            "drawdown={:.2} quantile={:.2} pool_drain={:.2} pool_sol={:.1}",
            drawdown_pct, quantile, pool_drawdown_pct, entry_pool_sol
        ))
    }
}

/// Rolling 24h spend cap for the dip_buy entry trigger. Shared across all
/// concurrent sessions via `Arc<Mutex<_>>`. Counts both dry-run and live
/// spending so the cap behavior is verifiable end-to-end before going live.
#[derive(Debug)]
pub struct DailySpendTracker {
    cap_sol: f64,
    window: Duration,
    spends: VecDeque<(Instant, f64)>,
}

impl DailySpendTracker {
    pub fn new(cap_sol: f64) -> Self {
        Self {
            cap_sol,
            window: Duration::from_secs(24 * 60 * 60),
            spends: VecDeque::new(),
        }
    }

    fn evict_old(&mut self, now: Instant) {
        while let Some(&(ts, _)) = self.spends.front() {
            if now.duration_since(ts) > self.window {
                self.spends.pop_front();
            } else {
                break;
            }
        }
    }

    /// Returns `Ok(())` if a spend of `amount_sol` would stay within the cap,
    /// `Err(spent_so_far)` if it would breach. Caller should record on success.
    pub fn check(&mut self, now: Instant, amount_sol: f64) -> Result<(), f64> {
        self.evict_old(now);
        let total: f64 = self.spends.iter().map(|(_, a)| a).sum();
        if total + amount_sol > self.cap_sol {
            Err(total)
        } else {
            Ok(())
        }
    }

    /// Record a spend at `now`. Caller is responsible for having checked
    /// first; this never refuses (the cap is enforced via `check`).
    pub fn record(&mut self, now: Instant, amount_sol: f64) {
        self.evict_old(now);
        self.spends.push_back((now, amount_sol));
    }

    pub fn total_24h(&mut self, now: Instant) -> f64 {
        self.evict_old(now);
        self.spends.iter().map(|(_, a)| a).sum()
    }

    pub fn cap(&self) -> f64 {
        self.cap_sol
    }
}

pub type SharedSpendTracker = std::sync::Arc<Mutex<DailySpendTracker>>;
