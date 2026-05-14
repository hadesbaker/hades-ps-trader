/// Record of an open MACD-triggered trade. The trading loop holds at most one
/// of these per monitored token at any time (no pyramiding).
#[derive(Debug)]
pub struct Position {
    pub mint: String,
    pub name: Option<String>,
    pub symbol: Option<String>,
    /// SOL per token (UI units), derived from `cost_sol / tokens_received`.
    pub entry_price_sol: f64,
    /// SOL spent on the entry buy. Used for net-return calc on exit.
    pub cost_sol: f64,
}

impl Position {
    pub fn pnl_pct(&self, current_price_sol: f64) -> f64 {
        if self.entry_price_sol <= 0.0 {
            return 0.0;
        }
        ((current_price_sol - self.entry_price_sol) / self.entry_price_sol) * 100.0
    }
}
