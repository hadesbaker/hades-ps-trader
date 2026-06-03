use crate::config::{DipBuyExitConfig, Trading};
use std::time::Instant;

/// Which entry trigger created a position. Drives per-strategy exit logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryTrigger {
    Macd,
    Capitulation,
    DipBuy,
}

/// Record of an open trade. The trading loop holds at most one per monitored
/// token at any time (no pyramiding).
#[derive(Debug)]
pub struct Position {
    pub mint: String,
    pub name: Option<String>,
    pub symbol: Option<String>,
    /// SOL per token (UI units), derived from `cost_sol / tokens_received`.
    pub entry_price_sol: f64,
    /// SOL spent on the entry buy. Used for net-return calc on exit.
    pub cost_sol: f64,
    /// Highest PnL pct seen since entry. Drives the dynamic trailing stop.
    pub peak_pct: f64,
    /// Monotonic timestamp captured the moment the buy tx confirmed on-chain.
    /// Used to enforce `max_hold_time`.
    pub bought_at: Instant,
    /// Which trigger fired the buy. Routes exit logic to the right config.
    pub entry_trigger: EntryTrigger,
}

impl Position {
    pub fn pnl_pct(&self, current_price_sol: f64) -> f64 {
        if self.entry_price_sol <= 0.0 {
            return 0.0;
        }
        ((current_price_sol - self.entry_price_sol) / self.entry_price_sol) * 100.0
    }

    pub fn update_peak(&mut self, current_pct: f64) {
        if current_pct > self.peak_pct {
            self.peak_pct = current_pct;
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TrailTier {
    pub gain_pct: f64,
    pub trail_pct: f64,
}

/// Parse "12:7,20:10,40:10,80:20,120:25" into ascending-by-gain tiers.
pub fn parse_tiers(s: &str) -> Vec<TrailTier> {
    let mut tiers: Vec<TrailTier> = s
        .split(',')
        .filter_map(|p| {
            let mut it = p.trim().split(':');
            let g: f64 = it.next()?.trim().parse().ok()?;
            let t: f64 = it.next()?.trim().parse().ok()?;
            Some(TrailTier { gain_pct: g, trail_pct: t })
        })
        .collect();
    tiers.sort_by(|a, b| a.gain_pct.partial_cmp(&b.gain_pct).unwrap_or(std::cmp::Ordering::Equal));
    tiers
}

#[derive(Debug, Clone, Copy)]
pub enum ExitReason {
    StopLoss { pct: f64 },
    TakeProfit { pct: f64 },
    DynamicTrail { tier_gain: f64, trail: f64, trigger_at_pct: f64, current_pct: f64 },
    MaxHoldTime { elapsed_secs: u64, current_pct: f64 },
}

impl ExitReason {
    pub fn display(&self) -> String {
        match self {
            ExitReason::StopLoss { pct } => format!("Stop loss ({pct:+.2}%)"),
            ExitReason::TakeProfit { pct } => format!("Take profit ({pct:+.2}%)"),
            ExitReason::DynamicTrail { tier_gain, trail, trigger_at_pct, current_pct } => format!(
                "Trailing stop (tier {tier_gain:.0}% / trail {trail:.0}%, trigger {trigger_at_pct:+.2}%, exit {current_pct:+.2}%)"
            ),
            ExitReason::MaxHoldTime { elapsed_secs, current_pct } => format!(
                "Max hold time ({elapsed_secs}s elapsed, exit {current_pct:+.2}%)"
            ),
        }
    }
}

pub fn decide_exit(
    pos: &Position,
    current_pct: f64,
    elapsed_secs: u64,
    cfg: &Trading,
    dip_buy_exit: &DipBuyExitConfig,
    universal_tiers: &[TrailTier],
    dip_buy_tiers: &[TrailTier],
) -> Option<ExitReason> {
    match pos.entry_trigger {
        EntryTrigger::DipBuy => {
            decide_exit_dip_buy(pos, current_pct, elapsed_secs, dip_buy_exit, dip_buy_tiers)
        }
        EntryTrigger::Macd | EntryTrigger::Capitulation => {
            decide_exit_universal(pos, current_pct, elapsed_secs, cfg, universal_tiers)
        }
    }
}

fn decide_exit_dip_buy(
    pos: &Position,
    current_pct: f64,
    elapsed_secs: u64,
    cfg: &DipBuyExitConfig,
    tiers: &[TrailTier],
) -> Option<ExitReason> {
    // Time-stop first — empirically the matching loser in validation held
    // 1283s; this cap would have killed it well before -27%.
    if cfg.max_hold_secs > 0 && elapsed_secs >= cfg.max_hold_secs {
        return Some(ExitReason::MaxHoldTime { elapsed_secs, current_pct });
    }
    if cfg.stop_loss_pct > 0.0 && current_pct <= -cfg.stop_loss_pct {
        return Some(ExitReason::StopLoss { pct: current_pct });
    }
    // Dynamic trailing stop — primary exit for winners. Lets a +50% peak
    // ride to +200% if it keeps going, instead of capping at a fixed TP.
    if let Some(tier) = tiers.iter().rev().find(|t| pos.peak_pct >= t.gain_pct) {
        let trigger = pos.peak_pct - tier.trail_pct;
        if current_pct <= trigger {
            return Some(ExitReason::DynamicTrail {
                tier_gain: tier.gain_pct,
                trail: tier.trail_pct,
                trigger_at_pct: trigger,
                current_pct,
            });
        }
    }
    // Hard TP ceiling — safety stop in case price spikes past the highest
    // trail tier without triggering it. Set high in defaults (300%).
    if cfg.take_profit_pct > 0.0 && current_pct >= cfg.take_profit_pct {
        return Some(ExitReason::TakeProfit { pct: current_pct });
    }
    None
}

fn decide_exit_universal(
    pos: &Position,
    current_pct: f64,
    elapsed_secs: u64,
    cfg: &Trading,
    tiers: &[TrailTier],
) -> Option<ExitReason> {
    // Hard time cap takes priority — if it's been hit, exit regardless of PnL.
    if cfg.max_hold_time > 0 && elapsed_secs >= cfg.max_hold_time {
        return Some(ExitReason::MaxHoldTime { elapsed_secs, current_pct });
    }
    if cfg.stop_loss_percent > 0.0 && current_pct <= -cfg.stop_loss_percent {
        return Some(ExitReason::StopLoss { pct: current_pct });
    }
    if cfg.profit_target_percent > 0.0 && current_pct >= cfg.profit_target_percent {
        return Some(ExitReason::TakeProfit { pct: current_pct });
    }
    if let Some(tier) = tiers.iter().rev().find(|t| pos.peak_pct >= t.gain_pct) {
        let trigger = pos.peak_pct - tier.trail_pct;
        if current_pct <= trigger {
            return Some(ExitReason::DynamicTrail {
                tier_gain: tier.gain_pct,
                trail: tier.trail_pct,
                trigger_at_pct: trigger,
                current_pct,
            });
        }
    }
    None
}
