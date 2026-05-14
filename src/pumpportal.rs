use crate::config::BoxError;
use futures::{SinkExt, StreamExt};
use log::{debug, error, info, warn};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[derive(Debug, Clone)]
pub struct MigrationEvent {
    pub mint: String,
    pub name: Option<String>,
    pub symbol: Option<String>,
}

pub fn spawn_migration_listener(ws_url: String) -> mpsc::UnboundedReceiver<MigrationEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut backoff = 1u64;
        loop {
            match run_migration(&ws_url, &tx).await {
                Ok(()) => info!("migration ws closed; reconnecting in {backoff}s"),
                Err(e) => error!("migration ws error: {e}; reconnecting in {backoff}s"),
            }
            sleep(Duration::from_secs(backoff)).await;
            backoff = backoff.saturating_mul(2).min(30);
        }
    });
    rx
}

async fn run_migration(
    ws_url: &str,
    tx: &mpsc::UnboundedSender<MigrationEvent>,
) -> Result<(), BoxError> {
    let (ws, _) = connect_async(ws_url).await?;
    let (mut write, mut read) = ws.split();

    let sub = json!({ "method": "subscribeMigration" }).to_string();
    write.send(Message::Text(sub)).await?;
    info!("subscribed: subscribeMigration");

    while let Some(msg) = read.next().await {
        match msg? {
            Message::Text(text) => {
                let v: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("non-JSON ws frame: {e} :: {text}");
                        continue;
                    }
                };
                debug!("migration ws: {v}");
                if let Some(mint) = extract_mint(&v) {
                    let name = v.get("name").and_then(|x| x.as_str()).map(str::to_string);
                    let symbol = v.get("symbol").and_then(|x| x.as_str()).map(str::to_string);
                    let _ = tx.send(MigrationEvent { mint, name, symbol });
                } else {
                    debug!("migration frame had no mint; skipping: {v}");
                }
            }
            Message::Ping(p) => write.send(Message::Pong(p)).await?,
            Message::Close(_) => break,
            _ => {}
        }
    }
    Ok(())
}

fn extract_mint(v: &Value) -> Option<String> {
    for key in ["mint", "tokenMint", "token_mint", "ca", "address"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            return Some(s.to_string());
        }
    }
    None
}
