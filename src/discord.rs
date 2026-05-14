use log::warn;
use serde_json::{json, Value};

const COLOR_GREEN: u32 = 0x57F287;
const COLOR_RED: u32 = 0xED4245;

#[derive(Clone)]
pub struct Notifier {
    http: reqwest::Client,
    webhook_url: Option<String>,
}

impl Notifier {
    pub fn new(http: reqwest::Client, webhook_url: Option<String>) -> Self {
        let webhook_url = webhook_url.filter(|s| !s.trim().is_empty());
        Self { http, webhook_url }
    }

    pub fn enabled(&self) -> bool {
        self.webhook_url.is_some()
    }

    pub fn notify_buy(
        &self,
        mint: &str,
        name: Option<&str>,
        symbol: Option<&str>,
        amount_sol: f64,
        tokens: f64,
        entry_price_sol: f64,
    ) {
        let Some(url) = self.webhook_url.clone() else { return };
        let payload = json!({
            "embeds": [{
                "title": format!("BUY: {}", display(name, symbol, mint)),
                "url": pumpfun_url(mint),
                "color": COLOR_GREEN,
                "fields": [
                    {"name": "Mint",        "value": format!("`{mint}`"),                  "inline": false},
                    {"name": "Spent",       "value": format!("{amount_sol:.4} SOL"),       "inline": true},
                    {"name": "Tokens",      "value": format_tokens(tokens),                "inline": true},
                    {"name": "Entry Price", "value": format!("{entry_price_sol:.10} SOL"), "inline": true},
                ],
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }],
        });
        self.spawn_send(url, payload);
    }

    pub fn notify_sell(
        &self,
        mint: &str,
        name: Option<&str>,
        symbol: Option<&str>,
        reason: &str,
        pnl_pct: f64,
        net_return_sol: f64,
    ) {
        let Some(url) = self.webhook_url.clone() else { return };
        let color = if net_return_sol >= 0.0 { COLOR_GREEN } else { COLOR_RED };
        let payload = json!({
            "embeds": [{
                "title": format!("SELL: {}", display(name, symbol, mint)),
                "url": pumpfun_url(mint),
                "color": color,
                "fields": [
                    {"name": "Mint",       "value": format!("`{mint}`"),               "inline": false},
                    {"name": "Reason",     "value": reason,                            "inline": false},
                    {"name": "PnL",        "value": format!("{pnl_pct:+.2}%"),         "inline": true},
                    {"name": "Net Return", "value": format!("{net_return_sol:+.4} SOL (at trigger price)"), "inline": true},
                ],
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }],
        });
        self.spawn_send(url, payload);
    }

    /// Loud alert: buy landed on-chain but we couldn't establish a monitor.
    /// Manual intervention required to sell the position.
    pub fn notify_orphan_buy(
        &self,
        mint: &str,
        name: Option<&str>,
        symbol: Option<&str>,
        buy_sig: &str,
        amount_sol: f64,
        reason: &str,
    ) {
        let Some(url) = self.webhook_url.clone() else { return };
        let payload = json!({
            "embeds": [{
                "title": format!("ORPHAN POSITION — MANUAL SELL REQUIRED: {}", display(name, symbol, mint)),
                "url": pumpfun_url(mint),
                "color": COLOR_RED,
                "fields": [
                    {"name": "Mint",   "value": format!("`{mint}`"),            "inline": false},
                    {"name": "Buy tx", "value": format!("`{buy_sig}`"),         "inline": false},
                    {"name": "Spent",  "value": format!("{amount_sol:.4} SOL"), "inline": true},
                    {"name": "Reason", "value": reason,                         "inline": false},
                    {"name": "Action", "value": "Bot cannot monitor this position. Sell manually via pump.fun or Phantom.", "inline": false},
                ],
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }],
        });
        self.spawn_send(url, payload);
    }

    fn spawn_send(&self, url: String, payload: Value) {
        let http = self.http.clone();
        tokio::spawn(async move {
            match http.post(&url).json(&payload).send().await {
                Ok(resp) if resp.status().is_success() => {}
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    warn!("discord webhook returned {status}: {body}");
                }
                Err(e) => warn!("discord webhook send failed: {e}"),
            }
        });
    }
}

fn pumpfun_url(mint: &str) -> String {
    format!("https://pump.fun/coin/{mint}")
}

fn display(name: Option<&str>, symbol: Option<&str>, mint: &str) -> String {
    match (name, symbol) {
        (Some(n), Some(s)) => format!("{n} ({s})"),
        (Some(n), None) => n.to_string(),
        (None, Some(s)) => format!("({s})"),
        (None, None) => short_mint(mint),
    }
}

fn short_mint(mint: &str) -> String {
    if mint.len() > 8 {
        format!("{}…{}", &mint[..4], &mint[mint.len() - 4..])
    } else {
        mint.to_string()
    }
}

fn format_tokens(t: f64) -> String {
    if t >= 1.0 { format!("{t:.2}") } else { format!("{t:.6}") }
}
