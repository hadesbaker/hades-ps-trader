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
    /// Optional capitulation dip-buy entry mode. If the whole `[capitulation]`
    /// section is omitted, defaults apply (the detector is DISABLED — MACD
    /// remains the entry trigger).
    #[serde(default)]
    pub capitulation: CapitulationConfig,
    /// Optional dip-buy entry mode (derived from monitored-wallet analysis).
    /// When `enabled = true`, takes precedence over BOTH MACD and capitulation
    /// — they will not fire buys while dip_buy is active.
    #[serde(default)]
    pub dip_buy: DipBuyConfig,
    /// Per-strategy exit profile applied only to dip_buy positions. MACD and
    /// capitulation positions continue to use the universal `[trading]` exit
    /// fields. Defaults match the empirically-derived strategy spec.
    #[serde(default)]
    pub dip_buy_exit: DipBuyExitConfig,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Trading {
    pub slippage: f64,
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
    /// Per-call deadline for one PumpSwap vault read (`getMultipleAccounts`).
    /// If the RPC doesn't respond within this many ms, the call is killed and
    /// counted as a failure. Distinct from `price_poll_interval_ms`, which is
    /// the pause BETWEEN polls; this is the budget for a SINGLE poll. On
    /// Solana mainnet `getMultipleAccounts` can take 200-1500ms under load —
    /// 1500-2000ms is a safe ceiling, anything tighter risks self-induced
    /// failure storms when many sessions poll concurrently.
    pub pumpswap_read_timeout_ms: u64,
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

/// Capitulation dip-and-reclaim entry. Alternative to MACD: when
/// `enabled = true`, the bot watches for a sustained dump (price down at
/// least `dip_pct` over `window_secs`) and then waits for a confirmed bounce
/// off the low before buying. The fire condition is two-stage:
///
///   STAGE 1 — dump detected (price ≤ -dip_pct over window). Record the
///             local low; do NOT buy.
///   STAGE 2 — price reclaims at least `reclaim_pct` above the local low AND
///             sustains that reclaim for `reclaim_confirm_secs` seconds. If
///             no reclaim happens within `pending_expire_secs` of the dump,
///             abandon this dump and start over.
///
/// Buying the bounce (not the dip) targets the test9 finding that mid-dump
/// fills cost ~9pp of edge — liquidity flees during the dump and returns on
/// the bounce. After firing the detector is debounced for `debounce_secs` to
/// avoid restacking entries.
#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct CapitulationConfig {
    /// Master switch. `false` (default) → MACD is the entry trigger and this
    /// detector never runs.
    pub enabled: bool,
    /// Required dip size, in percent. STAGE 1 trips when current price is at
    /// least this many percent below the price at the start of the window.
    pub dip_pct: f64,
    /// Rolling-window length in seconds. The detector compares the current
    /// price to the OLDEST tick still inside this window.
    pub window_secs: u64,
    /// Required reclaim off the local low, in percent. STAGE 2 fires when
    /// current price has bounced at least this much above `pending_low` AND
    /// sustains it for `reclaim_confirm_secs`. Smaller = catches weaker
    /// bounces but more false starts; larger = waits for stronger bounces
    /// but misses fast V-recoveries.
    pub reclaim_pct: f64,
    /// How long (seconds) to keep watching for a reclaim after STAGE 1
    /// trips. If no reclaim within this window, the pending dump is
    /// abandoned and the detector resets. Tunes patience: too short misses
    /// slow bounces; too long means we keep watching dead tokens.
    pub pending_expire_secs: u64,
    /// How long (seconds) the reclaim threshold must be continuously
    /// sustained before STAGE 2 fires. Filters spike-and-fade fake bounces.
    /// 0 disables sustain (fires on first reclaim tick).
    pub reclaim_confirm_secs: u64,
    /// Debounce in seconds after a fire — suppresses re-firing on the same
    /// session. Independent of the post-sell `cooldown_secs` from `[macd]`,
    /// which still applies after an exit.
    pub debounce_secs: u64,
}

impl Default for CapitulationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dip_pct: 50.0,
            window_secs: 60,
            reclaim_pct: 10.0,
            pending_expire_secs: 30,
            reclaim_confirm_secs: 10,
            debounce_secs: 60,
        }
    }
}

/// Dip-buy entry trigger derived from hades-wallet-monitor analysis.
/// Fires when a mature PumpSwap pool experiences a real drawdown (price +
/// TVL together) and the current price sits near the local low of the
/// rolling window. Validated against 142 reconstructions; the criteria
/// matched 35% of winners and 0% of losers after tightening
/// `min_pool_sol_drawdown_pct` from 0.15 → 0.20.
///
/// Takes precedence over MACD and capitulation when `enabled = true`.
#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct DipBuyConfig {
    pub enabled: bool,
    /// Rolling window length for drawdown / quantile / TVL-drain calc.
    pub window_secs: u64,
    /// Required price drawdown from window max to current price.
    pub min_drawdown_pct: f64,
    /// Current price must be at or below this quantile of [min, max] in window.
    pub max_entry_quantile: f64,
    /// Required TVL drain from window max — confirms real sells.
    pub min_pool_sol_drawdown_pct: f64,
    /// Pool size floor at entry — avoid micro-pools.
    pub min_pool_sol: f64,
    /// Post-fire cooldown per session.
    pub cooldown_secs: u64,
    /// Rolling 24h cap on cumulative dip_buy spending. Bug-protection,
    /// independent of strategy risk.
    pub daily_spend_cap_sol: f64,
}

impl Default for DipBuyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            window_secs: 60,
            min_drawdown_pct: 0.30,
            max_entry_quantile: 0.30,
            min_pool_sol_drawdown_pct: 0.20,
            min_pool_sol: 30.0,
            cooldown_secs: 60,
            daily_spend_cap_sol: 2.0,
        }
    }
}

/// Exit profile applied ONLY to dip_buy positions. Empirical hold for the
/// reconstructed winners was ~195s median; the time-stop here is at 600s
/// (10 min) which would have killed the single matching loser in the
/// validation set (which held 1283s and ended -27%).
///
/// The trader currently does full-exit-only (no partial sells), so the
/// original spec's "scale 50% at +50%, rest at +100%" ladder is folded
/// into a single take-profit at +75%. This captures most of the winner
/// distribution without leaving big tails on the table; revisit if/when
/// partial sells are added.
#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct DipBuyExitConfig {
    pub take_profit_pct: f64,
    pub stop_loss_pct: f64,
    pub max_hold_secs: u64,
}

impl Default for DipBuyExitConfig {
    fn default() -> Self {
        Self {
            take_profit_pct: 75.0,
            stop_loss_pct: 25.0,
            max_hold_secs: 600,
        }
    }
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, BoxError> {
        let raw = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&raw)?)
    }
}
