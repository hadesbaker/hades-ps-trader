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
    /// Copy-trade entry mode: mirror live BUYs from followed alpha wallets
    /// (PumpPortal `subscribeAccountTrade`) and manage each position with our
    /// own mechanical exit. Followed wallet addresses come from the
    /// `COPY_TRADE_WALLETS` env var (comma-separated) — NOT this file — to keep
    /// the research-subject wallets out of the committed public repo.
    #[serde(default)]
    pub copy_trade: CopyTradeConfig,
    /// Jito bundle submission. When enabled, signed trade txs are submitted as a
    /// single-tx bundle to the Jito block engine (atomic single-block landing)
    /// instead of a plain RPC `sendTransaction`. Cost-attack lever: cuts the
    /// decide→land half of adverse selection. The `tip_sol` is paid via the
    /// priorityFee of the (first) bundled tx — PumpPortal builds the tip in when
    /// the trade-local request is an array. Falls back to RPC send if the bundle
    /// submission errors. If the section is omitted, Jito is DISABLED.
    #[serde(default)]
    pub jito: JitoConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JitoConfig {
    pub enabled: bool,
    /// Jito tip in SOL, paid as the priorityFee of the bundled tx. Ordering on
    /// the block engine is by tip; ~0.0003-0.001 buys fast inclusion and
    /// replaces the need for a high regular priority fee.
    pub tip_sol: f64,
    /// Jito block-engine bundles endpoint.
    pub block_engine_url: String,
}

impl Default for JitoConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            tip_sol: 0.0003,
            block_engine_url: "https://mainnet.block-engine.jito.wtf/api/v1/bundles".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CopyTradeConfig {
    pub enabled: bool,
    /// SOL per copied entry.
    pub trade_amount_sol: f64,
    /// Max concurrent copy positions (independent of `[trading].max_positions`).
    pub max_positions: u64,
    /// Rolling 24h spend cap (SOL) across copy entries — bug protection.
    pub daily_spend_cap_sol: f64,
    /// Ignore a mint we entered/exited within this many seconds (dedup/anti-churn).
    pub reentry_cooldown_secs: u64,
    /// Ignore a followed wallet's buys within this many seconds of its last
    /// copied buy — stops one hyperactive (bot/MM) wallet from dominating the
    /// daily cap and starving slower, higher-quality alphas.
    pub per_wallet_cooldown_secs: u64,
    /// Only follow an alpha BUY if the pool TVL (SOL) at entry is at least this.
    pub min_pool_sol: f64,
    /// Seconds to hold a copy position before force-selling. 0 = monitor indefinitely.
    pub max_hold_secs: u64,
    /// Hard stop-loss percent for copy positions.
    pub stop_loss_pct: f64,
    /// Trailing-stop tiers "gain:trail,..." for copy positions.
    pub trail_tiers: String,
    /// Take-profit ceiling percent (safety; high = effectively trailing-only).
    pub take_profit_pct: f64,
    /// MIRROR-EXIT mode. When true, the primary exit is "sell when the alpha we
    /// followed closes the mint" — the offline backtest showed the alphas' edge
    /// is in their EXIT timing, so our mechanical trailing/TP cut winners short.
    /// In mirror mode, `trail_tiers` and `take_profit_pct` are IGNORED; only the
    /// rug guard, `stop_loss_pct`, and `mirror_max_hold_secs` remain as safety
    /// backstops. When false, the legacy mechanical exit applies and sells from
    /// followed wallets are ignored.
    #[serde(default = "default_mirror_exit")]
    pub mirror_exit: bool,
    /// In mirror mode, exit on the entry wallet's FIRST sell of the mint instead
    /// of waiting for a full close. Effectively REQUIRED: PumpPortal's
    /// subscribeAccountTrade sell frames do not carry `newTokenBalance`, so a
    /// partial-vs-full close can't be told apart from the frame. Also latency-
    /// favourable (we're "last out") and 91% of alpha sells are full closes.
    #[serde(default = "default_mirror_exit_on_first_sell")]
    pub mirror_exit_on_first_sell: bool,
    /// In mirror mode (full-close detection), treat the alpha as "closed" once
    /// their post-sell balance drops to this fraction of their post-entry
    /// balance or below. 0.10 = they've offloaded ≥90%.
    #[serde(default = "default_mirror_close_fraction")]
    pub mirror_close_fraction: f64,
    /// In mirror mode, the safety backstop hold time (seconds) — force-sell if
    /// the alpha never closes within this window. Replaces `max_hold_secs` while
    /// mirror_exit is on. 0 = hold indefinitely (only rug/SL/process-exit close).
    #[serde(default = "default_mirror_max_hold_secs")]
    pub mirror_max_hold_secs: u64,
}

fn default_mirror_exit() -> bool {
    true
}
fn default_mirror_exit_on_first_sell() -> bool {
    true
}
fn default_mirror_close_fraction() -> f64 {
    0.10
}
fn default_mirror_max_hold_secs() -> u64 {
    7200
}

impl Default for CopyTradeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            trade_amount_sol: 0.1,
            max_positions: 5,
            daily_spend_cap_sol: 2.0,
            reentry_cooldown_secs: 300,
            per_wallet_cooldown_secs: 60,
            min_pool_sol: 30.0,
            max_hold_secs: 900,
            stop_loss_pct: 20.0,
            trail_tiers: "20:6,40:10,75:15,100:20,150:25,200:30".to_string(),
            take_profit_pct: 300.0,
            mirror_exit: default_mirror_exit(),
            mirror_exit_on_first_sell: default_mirror_exit_on_first_sell(),
            mirror_close_fraction: default_mirror_close_fraction(),
            mirror_max_hold_secs: default_mirror_max_hold_secs(),
        }
    }
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
    /// Tightened from 0.30 → 0.20 after live data showed all real fires landed
    /// at quantile 0.00, so 0.20 still admits every real signal while reducing
    /// false positives on shallow consolidations.
    pub max_entry_quantile: f64,
    /// Required TVL drain from window max — confirms real sells.
    pub min_pool_sol_drawdown_pct: f64,
    /// MAX allowed TVL drain. Beyond this, the pool is dying (rug), not
    /// dipping. 0 disables. Set near the upper edge of normal-dip drains.
    pub max_pool_sol_drawdown_pct: f64,
    /// Pool size floor at entry — avoid micro-pools.
    pub min_pool_sol: f64,
    /// Post-fire cooldown per session.
    pub cooldown_secs: u64,
    /// Rolling 24h cap on cumulative dip_buy spending. Bug-protection,
    /// independent of strategy risk.
    pub daily_spend_cap_sol: f64,
    /// Pre-buy rug filter — pool TVL recovery. Pool_sol at entry must be at
    /// least `pool_recovery_pct` ABOVE the minimum pool_sol observed in the
    /// last `pool_recovery_secs` seconds. 0 secs disables. Targets the
    /// "wait for reclaim" pattern: don't fire while the pool is still draining.
    pub pool_recovery_secs: u64,
    pub pool_recovery_pct: f64,
    /// Pre-buy rug filter — price recovery. Same idea as pool_recovery, for
    /// the token price. Both filters together require the dip to have BOTH
    /// stabilized AND bounced before dip_buy will fire.
    pub price_recovery_secs: u64,
    pub price_recovery_pct: f64,
    /// Reject fires when (now - session_start) exceeds this. Empirically the
    /// late-session fires are the worst rug risk — yesterday's -88% rug
    /// fired 88 min into its session. 0 disables.
    pub max_session_age_secs: u64,
}

impl Default for DipBuyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            window_secs: 60,
            min_drawdown_pct: 0.30,
            max_entry_quantile: 0.20,
            min_pool_sol_drawdown_pct: 0.20,
            max_pool_sol_drawdown_pct: 0.45,
            min_pool_sol: 30.0,
            cooldown_secs: 60,
            daily_spend_cap_sol: 2.0,
            pool_recovery_secs: 8,
            pool_recovery_pct: 0.03,
            price_recovery_secs: 5,
            price_recovery_pct: 0.05,
            max_session_age_secs: 600,
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
    /// Hard take-profit ceiling. Acts as a safety stop; primary exit on
    /// winners is the dynamic trailing stop below. Set high (e.g. 300%)
    /// to effectively disable.
    pub take_profit_pct: f64,
    pub stop_loss_pct: f64,
    pub max_hold_secs: u64,
    /// Dynamic trailing stop for dip_buy winners, same format as
    /// [trading].dynamic_trailing_stop_thresholds. Once peak PnL crosses a
    /// tier's gain, exit when PnL drops `trail%` from peak. Replaces the
    /// fixed-TP behavior — lets winners ride past +75% with a trail.
    pub trail_tiers: String,
}

impl Default for DipBuyExitConfig {
    fn default() -> Self {
        Self {
            take_profit_pct: 300.0,
            stop_loss_pct: 25.0,
            max_hold_secs: 600,
            trail_tiers: "20:6,40:10,75:15,100:20,150:25,200:30".to_string(),
        }
    }
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, BoxError> {
        let raw = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&raw)?)
    }
}
