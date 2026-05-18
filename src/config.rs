use serde::{Deserialize, Serialize};
use std::path::Path;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    pub wallet_path: String,
    pub trading: Trading,
    pub macd: Macd,
    pub tradingfilters: TradingFilters,
    /// Pre-buy rug guard. If the whole `[rug]` section is omitted, defaults
    /// apply (the guard is enabled).
    #[serde(default)]
    pub rug: RugFilter,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Trading {
    pub max_slippage: f64,
    pub trade_amount: u64,
    pub priority_fee_sol: f64,
    /// Take-profit: exit once PnL reaches this percent. 0 disables.
    pub profit_target_percent: f64,
    /// Hard stop-loss: exit once PnL falls to this percent loss. 0 disables.
    pub stop_loss_percent: f64,
    /// Maximum number of concurrent OPEN positions, across all monitored tokens.
    pub max_positions: u64,
    pub buy_tx_retries: u64,
    pub sell_tx_retries: u64,
    /// How often the price feed reads the PumpSwap pool.
    pub price_poll_interval_ms: u64,
    /// Emit a `PNL MONITOR` info line every Nth successful price tick. 0 disables.
    pub pnl_log_every_n_ticks: u64,
    /// Total seconds to monitor a single token after graduation. Any open
    /// position is force-sold before the window ends. 0 = monitor indefinitely.
    pub max_monitor_time: u64,
    /// Maximum seconds to hold a single open position before exiting it on the
    /// standard sell flow. Timer starts when the buy tx confirms. 0 disables.
    pub max_hold_time: u64,
    /// Dynamic trailing stop: comma-separated `gain%:trail%` tiers. Once peak
    /// PnL crosses a tier's gain, the position exits if PnL drops `trail%` from
    /// its peak. E.g. `"20:8,40:13"`.
    pub dynamic_trailing_stop_thresholds: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Macd {
    /// Candle timeframe in seconds. MACD runs on candle closes.
    pub candle_interval_secs: u64,
    pub fast: u32,
    pub slow: u32,
    pub signal: u32,
    /// Minimum seconds between a sell and the next buy on the same token.
    /// 0 disables the cooldown.
    pub cooldown_secs: u64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TradingFilters {
    pub enabled: bool,
    pub min_holders: u64,
    pub min_txs: u64,
    /// Reject if the combined balance of the top 10 holders (excluding the
    /// PumpSwap pool's base vault and the pump.fun bonding-curve PDA) exceeds
    /// this fraction of total supply. Fraction in [0, 1]. 0 disables.
    pub top_ten_holder_percentage: f64,
}

/// Pre-buy rug detection. Evaluated on every bullish crossover, just before a
/// buy would fire, against the price + pool-liquidity history gathered since
/// the session started. A failing check blocks that one buy; the session keeps
/// monitoring, so a later signal can still buy if the token recovers.
///
/// Independent of [`TradingFilters`], which run once at graduation — this runs
/// continuously, per candidate buy. Missing fields fall back to the defaults
/// below, so a partial `[rug]` section is valid.
#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct RugFilter {
    /// Master switch. `false` → never block a buy.
    pub enabled: bool,
    /// Minimum WSOL liquidity (in SOL) the PumpSwap pool must hold to allow a
    /// buy. A pulled-liquidity rug drains this toward zero. 0 disables.
    pub min_pool_sol: f64,
    /// Block the buy if pool liquidity is down more than this percent from its
    /// session peak (liquidity actively being removed). 0 disables.
    pub max_liquidity_drop_pct: f64,
    /// Block the buy if price is down more than this percent from the session
    /// high (token already pumped and dumped). 0 disables.
    pub max_drawdown_pct: f64,
    /// Minimum price samples gathered before the guard will clear a buy.
    /// Avoids judging a token on a near-empty history.
    pub min_samples: u32,
}

impl Default for RugFilter {
    fn default() -> Self {
        Self {
            enabled: true,
            min_pool_sol: 5.0,
            max_liquidity_drop_pct: 50.0,
            max_drawdown_pct: 70.0,
            min_samples: 20,
        }
    }
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, BoxError> {
        let raw = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&raw)?)
    }
}
