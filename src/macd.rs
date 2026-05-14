//! MACD indicator + candle aggregator.
//!
//! Pure logic, no I/O. Designed to be fed by the price poll loop:
//! tick → CandleAggregator → on bar close → Macd → MacdEvent.

use std::time::{Duration, Instant};

/// Builds fixed-interval candle closes from streamed price ticks.
///
/// Time is monotonic (Instant). Bar boundaries are aligned to the first tick.
/// If a gap in the feed straddles multiple intervals, the aggregator skips
/// intermediate (empty) bars rather than fabricating flat ones.
pub struct CandleAggregator {
    interval: Duration,
    bar_start: Option<Instant>,
    last: f64,
}

impl CandleAggregator {
    pub fn new(interval: Duration) -> Self {
        Self { interval, bar_start: None, last: 0.0 }
    }

    /// Feed a price tick. Returns `Some(close)` exactly when a candle has
    /// just closed (the closed bar's close is returned; a new bar opens).
    pub fn on_tick(&mut self, price: f64, now: Instant) -> Option<f64> {
        let Some(start) = self.bar_start else {
            self.bar_start = Some(now);
            self.last = price;
            return None;
        };

        let elapsed = now.saturating_duration_since(start);
        if elapsed < self.interval {
            self.last = price;
            return None;
        }

        let close = self.last;
        // Align new bar to the most recent interval boundary so we don't drift
        // when ticks arrive slightly late.
        let intervals_passed = (elapsed.as_nanos() / self.interval.as_nanos()) as u32;
        let advance = self.interval * intervals_passed;
        self.bar_start = Some(start + advance);
        self.last = price;
        Some(close)
    }
}

/// Exponential moving average. Seeded with the SMA of the first `period`
/// values, then standard `k * x + (1-k) * prev` thereafter.
pub struct Ema {
    period: u32,
    k: f64,
    value: Option<f64>,
    warmup_sum: f64,
    warmup_count: u32,
}

impl Ema {
    pub fn new(period: u32) -> Self {
        assert!(period >= 1, "EMA period must be >= 1");
        Self {
            period,
            k: 2.0 / (period as f64 + 1.0),
            value: None,
            warmup_sum: 0.0,
            warmup_count: 0,
        }
    }

    /// Feed one input. Returns `Some(ema)` once enough values have been seen
    /// for the SMA seed, `None` during warmup.
    pub fn update(&mut self, x: f64) -> Option<f64> {
        if let Some(prev) = self.value {
            let next = x * self.k + prev * (1.0 - self.k);
            self.value = Some(next);
            return Some(next);
        }
        self.warmup_sum += x;
        self.warmup_count += 1;
        if self.warmup_count >= self.period {
            let sma = self.warmup_sum / self.warmup_count as f64;
            self.value = Some(sma);
            Some(sma)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MacdSnapshot {
    pub macd: f64,
    pub signal: f64,
    pub histogram: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Crossover {
    Bullish,
    Bearish,
}

#[derive(Debug, Clone, Copy)]
pub enum MacdEvent {
    /// Not yet enough closes for both fast/slow EMAs + signal EMA.
    Warmup,
    /// Indicator is live, no crossover this tick.
    NoCross(MacdSnapshot),
    /// MACD line crossed the signal line.
    Crossover {
        snapshot: MacdSnapshot,
        direction: Crossover,
    },
}

pub struct Macd {
    fast: Ema,
    slow: Ema,
    signal: Ema,
    prev_macd: Option<f64>,
    prev_signal: Option<f64>,
}

impl Macd {
    pub fn new(fast: u32, slow: u32, signal: u32) -> Self {
        assert!(
            fast < slow,
            "MACD fast period ({fast}) must be < slow period ({slow})"
        );
        Self {
            fast: Ema::new(fast),
            slow: Ema::new(slow),
            signal: Ema::new(signal),
            prev_macd: None,
            prev_signal: None,
        }
    }

    /// Feed one candle close.
    pub fn on_close(&mut self, close: f64) -> MacdEvent {
        let f = self.fast.update(close);
        let s = self.slow.update(close);
        let (Some(fast), Some(slow)) = (f, s) else {
            return MacdEvent::Warmup;
        };
        let macd = fast - slow;
        let Some(signal) = self.signal.update(macd) else {
            // MACD line live but signal EMA still warming.
            return MacdEvent::Warmup;
        };
        let snapshot = MacdSnapshot {
            macd,
            signal,
            histogram: macd - signal,
        };

        let direction = match (self.prev_macd, self.prev_signal) {
            (Some(pm), Some(ps)) => {
                if pm <= ps && macd > signal {
                    Some(Crossover::Bullish)
                } else if pm >= ps && macd < signal {
                    Some(Crossover::Bearish)
                } else {
                    None
                }
            }
            _ => None,
        };
        self.prev_macd = Some(macd);
        self.prev_signal = Some(signal);

        match direction {
            Some(direction) => MacdEvent::Crossover { snapshot, direction },
            None => MacdEvent::NoCross(snapshot),
        }
    }
}
