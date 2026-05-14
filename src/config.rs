use serde::{Deserialize, Serialize};
use std::path::Path;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    pub wallet_path: String,
    pub trading: Trading,
    pub macd: Macd,
    pub tradingfilters: TradingFilters,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Trading {
    pub max_slippage: f64,
    pub trade_amount: u64,
    pub priority_fee_sol: f64,
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

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, BoxError> {
        let raw = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&raw)?)
    }
}
