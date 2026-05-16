//! Pre-buy rug detection.
//!
//! A [`RugGuard`] lives for one monitoring session. The price-poll loop feeds
//! it every tick via [`RugGuard::observe`], so it accumulates the session's
//! price and pool-liquidity peaks. Just before a bullish crossover would buy,
//! the session calls [`RugGuard::check`]: it compares the *current* reading
//! against those peaks and the configured floors, and returns a reason string
//! when the token looks rugged — in which case the session skips that buy.
//!
//! Limitation: the peaks only cover the window the bot has actually observed.
//! If pool discovery was slow, the real post-graduation top can be missed, so
//! the drawdown figure is a lower bound. The absolute liquidity floor
//! (`min_pool_sol`) has no such caveat and is the most reliable signal.

use crate::config::RugFilter;

/// Per-session price + liquidity history for rug detection.
#[derive(Debug, Default)]
pub struct RugGuard {
    samples: u32,
    peak_price: f64,
    peak_pool_sol: f64,
}

impl RugGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one price tick. Call this on every price update.
    pub fn observe(&mut self, price_sol: f64, pool_sol: f64) {
        self.samples = self.samples.saturating_add(1);
        if price_sol > self.peak_price {
            self.peak_price = price_sol;
        }
        if pool_sol > self.peak_pool_sol {
            self.peak_pool_sol = pool_sol;
        }
    }

    /// Decide whether a buy should be blocked right now. `Some(reason)` means
    /// the token looks rugged and the buy should be skipped; `None` means clear.
    pub fn check(&self, f: &RugFilter, price_sol: f64, pool_sol: f64) -> Option<String> {
        if !f.enabled {
            return None;
        }

        if self.samples < f.min_samples {
            return Some(format!(
                "only {} price sample(s) so far (<{}) — not enough history to judge",
                self.samples, f.min_samples
            ));
        }

        // 1. Absolute liquidity floor — the most reliable rug signal.
        if f.min_pool_sol > 0.0 && pool_sol < f.min_pool_sol {
            return Some(format!(
                "pool liquidity {pool_sol:.2} SOL is below the {:.2} SOL floor",
                f.min_pool_sol
            ));
        }

        // 2. Liquidity drained from its session peak.
        if f.max_liquidity_drop_pct > 0.0 && self.peak_pool_sol > 0.0 {
            let drop_pct = (1.0 - pool_sol / self.peak_pool_sol) * 100.0;
            if drop_pct > f.max_liquidity_drop_pct {
                return Some(format!(
                    "pool liquidity down {drop_pct:.1}% from session peak {:.2} SOL (limit {:.1}%)",
                    self.peak_pool_sol, f.max_liquidity_drop_pct
                ));
            }
        }

        // 3. Price collapsed from its session high.
        if f.max_drawdown_pct > 0.0 && self.peak_price > 0.0 {
            let drawdown_pct = (1.0 - price_sol / self.peak_price) * 100.0;
            if drawdown_pct > f.max_drawdown_pct {
                return Some(format!(
                    "price down {drawdown_pct:.1}% from session high {:.10} SOL (limit {:.1}%)",
                    self.peak_price, f.max_drawdown_pct
                ));
            }
        }

        None
    }
}
