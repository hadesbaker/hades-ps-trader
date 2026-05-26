//! Capitulation dip-and-reclaim entry detector.
//!
//! Two-stage alternative entry trigger to MACD. A [`CapitulationDetector`]
//! lives for one monitoring session. Each price tick is fed via
//! [`CapitulationDetector::observe`]; on each [`CapitulationDetector::check`]
//! call the detector runs the state machine:
//!
//!   STAGE 1 — detect a sustained dump. When current price is ≥ `dip_pct`
//!   below the price at the start of a `window_secs` rolling window, record
//!   the local low (`pending_low`) and start the pending timer. Do NOT
//!   fire yet. While in pending state, keep tracking the lowest price.
//!
//!   STAGE 2 — wait for a confirmed reclaim. When price has bounced
//!   ≥ `reclaim_pct` above `pending_low` AND sustained that reclaim for
//!   `reclaim_confirm_secs` (continuous — falling back below the threshold
//!   resets the confirmation timer), fire. If `pending_expire_secs` elapses
//!   from the dump without a confirmed reclaim, abandon the pending state
//!   and reset.
//!
//! Why this variant: test9 (the dip-only variant at 0.5 SOL) showed that
//! capitulation buys cost ~9pp of signal-tick edge because they land
//! mid-dump while liquidity is fleeing. Waiting for the bounce buys into
//! returning liquidity instead. Defaults are placeholders pending a sweep.

use crate::config::CapitulationConfig;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Minimum ticks in the rolling window before STAGE 2 will fire. Defensive
/// against very-sparse sessions; at price_poll_interval_ms=500 this is ~5s
/// of price data, which is the smallest sample worth judging a bounce on.
const MIN_TICKS_FOR_FIRE: usize = 10;

#[derive(Debug)]
pub struct CapitulationDetector {
    enabled: bool,
    dip_pct: f64,
    reclaim_pct: f64,
    window: Duration,
    pending_expire: Duration,
    reclaim_confirm: Duration,
    debounce: Duration,

    ticks: VecDeque<(Instant, f64)>,

    pending_low: Option<f64>,
    pending_started: Option<Instant>,
    reclaim_confirm_started: Option<Instant>,

    last_fire: Option<Instant>,
}

impl CapitulationDetector {
    pub fn new(cfg: &CapitulationConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            dip_pct: cfg.dip_pct,
            reclaim_pct: cfg.reclaim_pct,
            window: Duration::from_secs(cfg.window_secs),
            pending_expire: Duration::from_secs(cfg.pending_expire_secs),
            reclaim_confirm: Duration::from_secs(cfg.reclaim_confirm_secs),
            debounce: Duration::from_secs(cfg.debounce_secs),
            ticks: VecDeque::new(),
            pending_low: None,
            pending_started: None,
            reclaim_confirm_started: None,
            last_fire: None,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    fn evict_old(&mut self, now: Instant) {
        while let Some(&(ts, _)) = self.ticks.front() {
            if now.duration_since(ts) > self.window {
                self.ticks.pop_front();
            } else {
                break;
            }
        }
    }

    fn reset_pending(&mut self) {
        self.pending_low = None;
        self.pending_started = None;
        self.reclaim_confirm_started = None;
    }

    /// Record one price tick. Evicts ticks older than the window so the front
    /// of the deque is always the earliest tick still inside `[now - window, now]`.
    pub fn observe(&mut self, now: Instant, price: f64) {
        if !self.enabled {
            return;
        }
        self.evict_old(now);
        self.ticks.push_back((now, price));
    }

    /// Decide whether a reclaim signal should fire right now. Returns
    /// `Some(reason)` if the two-stage rule trips and we're past the
    /// post-fire debounce; otherwise `None`. On `Some`, the debounce timer
    /// is reset and the pending state cleared.
    pub fn check(&mut self, now: Instant, price: f64) -> Option<String> {
        if !self.enabled {
            return None;
        }
        self.evict_old(now);

        // post-fire debounce
        if let Some(t) = self.last_fire {
            if now.duration_since(t) < self.debounce {
                return None;
            }
        }

        // require a meaningfully-full window before judging — otherwise a
        // session that just started would compare against its first tick
        // and any normal price wobble looks like a dip.
        let &(oldest_ts, oldest_price) = self.ticks.front()?;
        let needed = self.window.saturating_sub(Duration::from_secs(1));
        if now.duration_since(oldest_ts) < needed {
            return None;
        }
        if oldest_price <= 0.0 {
            return None;
        }

        // expire stale pending state before re-evaluating STAGE 1
        if let Some(started) = self.pending_started {
            if now.duration_since(started) > self.pending_expire {
                self.reset_pending();
            }
        }

        let dump_pct = (price / oldest_price - 1.0) * 100.0;

        // STAGE 1: dump detected → record / update local low, do not fire
        if dump_pct <= -self.dip_pct {
            match self.pending_low {
                Some(low) if price < low => {
                    self.pending_low = Some(price);
                    self.reclaim_confirm_started = None;
                }
                None => {
                    self.pending_low = Some(price);
                    self.pending_started = Some(now);
                    self.reclaim_confirm_started = None;
                }
                _ => {}
            }
            return None;
        }

        // STAGE 2: pending state set → check for confirmed reclaim
        let Some(low) = self.pending_low else {
            return None;
        };
        let reclaim_pct_now = (price / low - 1.0) * 100.0;

        if reclaim_pct_now < self.reclaim_pct {
            // bounce hasn't reached threshold (or fell back below it after
            // a partial recovery) — reset any in-flight confirmation
            self.reclaim_confirm_started = None;
            return None;
        }

        // sparseness floor: don't fire on near-empty histories
        if self.ticks.len() < MIN_TICKS_FOR_FIRE {
            return None;
        }

        // begin / continue confirmation
        let started = match self.reclaim_confirm_started {
            Some(t) => t,
            None => {
                self.reclaim_confirm_started = Some(now);
                now
            }
        };
        if now.duration_since(started) < self.reclaim_confirm {
            return None;
        }

        // confirmed — fire
        let reason = format!(
            "reclaim CONFIRMED: low {:.10} → {:.10} (+{:.1}% reclaim, after {:.1}% dump over {}s)",
            low,
            price,
            reclaim_pct_now,
            -dump_pct,
            self.window.as_secs()
        );
        self.last_fire = Some(now);
        self.reset_pending();
        Some(reason)
    }
}
