//! Capitulation dip-buy entry detector.
//!
//! Alternative entry trigger to MACD. A [`CapitulationDetector`] lives for one
//! monitoring session. Each price tick is fed via [`CapitulationDetector::observe`];
//! the detector keeps a rolling window of recent ticks. On each
//! [`CapitulationDetector::check`] call it returns `Some(reason)` when the
//! current price is more than `dip_pct` below the price at the start of the
//! window — i.e., a sustained drop of `dip_pct` over `window_secs`. After
//! firing the detector is debounced for `debounce_secs` so it doesn't re-fire
//! on every tick during a deep dump.
//!
//! Rule choice: rolling-window pct change (current vs oldest in window) — NOT
//! local-max drawdown. The capitulation backtest (`analysis/capitulation.py`)
//! found localmax fires too early in a dump on these tokens; the rolling
//! variant requires sustained downward velocity which is the real capitulation
//! signal. Default `dip_pct=50` and `window_secs=60` came out of that sweep
//! as the most profitable point under the live exit config.

use crate::config::CapitulationConfig;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct CapitulationDetector {
    enabled: bool,
    dip_pct: f64,
    window: Duration,
    debounce: Duration,
    ticks: VecDeque<(Instant, f64)>,
    last_fire: Option<Instant>,
}

impl CapitulationDetector {
    pub fn new(cfg: &CapitulationConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            dip_pct: cfg.dip_pct,
            window: Duration::from_secs(cfg.window_secs),
            debounce: Duration::from_secs(cfg.debounce_secs),
            ticks: VecDeque::new(),
            last_fire: None,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Record one price tick. Evicts ticks older than the window so the front
    /// of the deque is always the earliest tick still inside `[now - window, now]`.
    pub fn observe(&mut self, now: Instant, price: f64) {
        if !self.enabled {
            return;
        }
        while let Some(&(ts, _)) = self.ticks.front() {
            if now.duration_since(ts) > self.window {
                self.ticks.pop_front();
            } else {
                break;
            }
        }
        self.ticks.push_back((now, price));
    }

    /// Decide whether a capitulation signal should fire right now. Returns
    /// `Some(reason)` if the rule trips and we're past the debounce window;
    /// otherwise `None`. On `Some`, the debounce timer is reset.
    pub fn check(&mut self, now: Instant, price: f64) -> Option<String> {
        if !self.enabled {
            return None;
        }
        if let Some(t) = self.last_fire {
            if now.duration_since(t) < self.debounce {
                return None;
            }
        }
        let &(oldest_ts, oldest_price) = self.ticks.front()?;
        // Require a meaningfully-full window before judging — otherwise a
        // session that just started would compare against its first tick and
        // any normal price wobble looks like a dip.
        let needed = self.window.saturating_sub(Duration::from_secs(1));
        if now.duration_since(oldest_ts) < needed {
            return None;
        }
        if oldest_price <= 0.0 {
            return None;
        }
        let pct = (price / oldest_price - 1.0) * 100.0;
        if pct <= -self.dip_pct {
            self.last_fire = Some(now);
            Some(format!(
                "price down {:.1}% over {}s ({:.10} → {:.10})",
                -pct,
                self.window.as_secs(),
                oldest_price,
                price
            ))
        } else {
            None
        }
    }
}
