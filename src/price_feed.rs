//! Polled price feed for an open position. Reads PumpSwap pool vaults directly
//! over our own Solana RPC. No third-party indexers.

use crate::pumpswap::{self, PoolInfo};
use log::{debug, info, warn};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};

#[derive(Debug, Clone, Copy)]
pub struct PriceUpdate {
    pub price_sol: f64,
}

/// Spawn a background task that discovers the mint's PumpSwap pool, then
/// polls vault balances at `interval` and pushes `PriceUpdate`s. The task
/// ends when the receiver is dropped.
pub fn spawn_price_poll(
    rpc: Arc<RpcClient>,
    mint: String,
    interval: Duration,
) -> mpsc::UnboundedReceiver<PriceUpdate> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mint_pk = match Pubkey::from_str(&mint) {
            Ok(p) => p,
            Err(e) => {
                warn!("[{mint}] price poll: bad mint pubkey: {e}");
                return;
            }
        };

        let pool = match discover_with_retries(&rpc, &mint_pk, &mint).await {
            Some(p) => p,
            None => {
                warn!("[{mint}] price poll: gave up on PumpSwap pool discovery");
                return;
            }
        };
        info!(
            "[{mint}] PumpSwap pool: id={} base_vault={} quote_vault={}",
            pool.pool_id, pool.base_vault, pool.quote_vault
        );

        let mut consecutive_failures: u32 = 0;
        loop {
            match pumpswap::fetch_price_sol(&rpc, &pool).await {
                Ok(price) => {
                    if consecutive_failures >= 5 {
                        info!(
                            "[{mint}] price poll recovered after {consecutive_failures} failures"
                        );
                    }
                    consecutive_failures = 0;
                    if tx.send(PriceUpdate { price_sol: price }).is_err() {
                        debug!("[{mint}] price poll: receiver dropped");
                        return;
                    }
                }
                Err(e) => {
                    consecutive_failures += 1;
                    if consecutive_failures == 1 || consecutive_failures % 10 == 0 {
                        warn!(
                            "[{mint}] price poll error (attempt #{consecutive_failures}): {e}"
                        );
                    } else {
                        debug!("[{mint}] price poll error: {e}");
                    }
                }
            }
            sleep(interval).await;
            if tx.is_closed() {
                return;
            }
        }
    });
    rx
}

async fn discover_with_retries(
    rpc: &RpcClient,
    mint: &Pubkey,
    mint_str: &str,
) -> Option<PoolInfo> {
    let attempts = 10;
    let interval = Duration::from_secs(2);
    for i in 0..attempts {
        match pumpswap::find_pool(rpc, mint).await {
            Ok(p) => return Some(p),
            Err(e) => warn!(
                "[{mint_str}] pool discovery {}/{attempts} failed: {e}; retrying in {}s",
                i + 1,
                interval.as_secs(),
            ),
        }
        sleep(interval).await;
    }
    None
}
