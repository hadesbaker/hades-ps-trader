# hades-ps-sniper

A Rust bot that listens for pump.fun → PumpSwap token graduations in real time, gates each candidate behind on-chain filters (holder count + bonding-curve trade history), waits a configurable timer, snipes the buy via PumpPortal, and exits the position based on a dynamic trailing-stop ladder.

> **Warning — this bot moves real money.** Test with `--dry-run` first. Set `trade_amount` to a small value when you go live. There is no recovery for positions held when the process dies.

## Features

- Real-time graduation feed via PumpPortal `subscribeMigration`
- On-chain filters using your own Solana RPC (no third-party indexers required)
  - Holder count via `getProgramAccounts` (SPL Token, mint-filtered, non-zero balances)
  - Trade count via `getSignaturesForAddress` on the pump.fun bonding-curve PDA
- Pre-buy timer (`tradingfilters.timer`)
- Buy + sell via PumpPortal `/api/trade-local` (locally signed, you keep custody)
- `send_transaction_with_config` with configurable RPC rebroadcast retries and explicit on-chain confirmation polling
- Direct PumpSwap pool reads for per-position price tracking — one `getMultipleAccounts` per poll, no third-party indexers, no premium WS subscriptions
- Three exit triggers, first to fire wins:
  - Hard stop loss
  - Hard take profit
  - Dynamic trailing stop (peak-aware, multi-tier)
- Concurrency cap (`max_positions`)
- Optional Discord webhook notifications on buy / sell / orphan-position events
- Auto-reconnect on WS drops with exponential backoff

## Prerequisites

| Tool                   | Version                                                           | Why                                                                |
| ---------------------- | ----------------------------------------------------------------- | ------------------------------------------------------------------ |
| Rust                   | 1.85+ (edition 2024)                                              | Required by `edition = "2024"` in `Cargo.toml`                     |
| A Solana mainnet RPC   | any HTTP endpoint that supports `getProgramAccounts` with filters | QuickNode, Helius, Triton, etc. The free public RPC will not work. |
| A PumpPortal account   | free                                                              | API key optional, recommended for a stable migration WS subscription |
| A funded Solana wallet | small balance to start                                            | Used as both the trader and the signer                             |
| A Discord webhook URL  | optional                                                          | Buy / sell / orphan notifications, off when blank                 |

## Setup

```bash
# 1. Clone
git clone <your-fork-url> hades-ps-sniper
cd hades-ps-sniper

# 2. Environment
cp .env.example .env
# edit .env:
#   SOLANA_RPC_URL=https://...your-rpc...       (required)
#   DISCORD_WEBHOOK_URL=https://discord...       (optional — enables buy/sell notifications when set)
#   PUMPPORTAL_API_KEY=...                       (optional — recommended for stable WS)
#   RUST_LOG=info                                (set to "debug" to see per-tick price polls)

# 3. Config
cp config.toml.example config.toml
# edit config.toml — see "Configuration" below

# 4. Wallet
# Either copy an existing Solana CLI keypair JSON to ./wallet.json,
# or generate a fresh one:
solana-keygen new --outfile wallet.json
# Fund this wallet with SOL on mainnet before going live.

# 5. Build
cargo build --release
```

## Running

```bash
# Safe smoke test — does everything except submit buys/sells.
cargo run --release -- --dry-run

# Live.
cargo run --release
```

To copy the logs of the run into a file:

```bash
cargo run --release 2>&1 | tee logs/run.log
```

To copy the logs while maintaining your logger coloring:

```bash
RUST_LOG_STYLE=always cargo run --release 2>&1 \
    | tee >(awk '{gsub(/\x1b\[[0-9;]*m/, ""); print; fflush()}' > logs/test.log)
```

CLI flags:

- `--dry-run` — skip every buy and sell tx submission, log what would have been sent. Discord notifications are also suppressed in dry-run.
- `--config <path>` — alternate config file (default `config.toml`)

Logs are controlled by `RUST_LOG` (defaults to `info`, can be set in `.env`). Use `RUST_LOG=debug` to see every price poll tick and PnL update; use `RUST_LOG=hades_ps_sniper=debug` to keep upstream crates (solana-client, reqwest) quiet.

## Configuration

`config.toml`:

| Key                                        | Example                           | Purpose                                                                                                                  |
| ------------------------------------------ | --------------------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| `wallet_path`                              | `"wallet.json"`                   | Path to Solana CLI keypair JSON                                                                                          |
| `trading.trade_amount`                     | `250_000_000`                     | Lamports per buy (0.25 SOL)                                                                                              |
| `trading.max_slippage`                     | `20.0`                            | Slippage tolerance % for buy and sell                                                                                    |
| `trading.priority_fee_sol`                 | `0.0005`                          | Priority fee per tx in SOL                                                                                               |
| `trading.profit_target_percent`            | `10.0`                            | Hard take-profit %; set `0` to disable                                                                                   |
| `trading.stop_loss_percent`                | `25.0`                            | Hard stop-loss %; set `0` to disable                                                                                     |
| `trading.dynamic_trailing_stop_thresholds` | `"12:7,20:10,40:10,80:20,120:25"` | Trailing tiers as `gain%:trail%` pairs. Once peak PnL crosses a tier, the bot exits if PnL drops `trail%` from the peak. |
| `trading.max_positions`                    | `3`                               | Concurrent open positions cap                                                                                            |
| `trading.buy_tx_retries`                   | `2`                               | RPC-side rebroadcast retries for the buy tx (`maxRetries` in `RpcSendTransactionConfig`). Bot then polls for on-chain confirmation. |
| `trading.sell_tx_retries`                  | `2`                               | Same as above, for the sell tx.                                                                                          |
| `trading.price_poll_interval_ms`           | `3000`                            | How often the position monitor reads the PumpSwap pool's vaults to compute price. One `getMultipleAccounts` call per tick. |
| `trading.pnl_log_every_n_ticks`            | `5`                               | Emit a `PNL MONITOR: <mint> (TICKER) +X.XX%` info line every Nth successful price tick. Set `0` to disable (per-tick debug log unaffected). |
| `trading.max_hold_time`                    | `3600`                            | Maximum seconds to hold a position. Timer starts the moment the buy tx is confirmed on-chain. Set `0` to disable. Takes priority over SL/TP/trail. |
| `tradingfilters.enabled`                   | `true`                            | Gate buys behind on-chain filters                                                                                        |
| `tradingfilters.min_holders`               | `100`                             | Minimum holders required; `0` disables this check                                                                        |
| `tradingfilters.min_txs`                   | `250`                             | Minimum bonding-curve trade signatures required; `0` disables                                                            |
| `tradingfilters.top_ten_holder_percentage` | `0.30`                            | Reject if combined top-10 holder balance exceeds this fraction of total supply (PumpSwap pool vault + bonding-curve PDA excluded). Fraction, not percent: `0.05` = 5%. `0` disables. |
| `tradingfilters.rug_percentage`            | `0.60`                            | Pre-buy rug guard. During the `timer` window, the bot watches PumpSwap price; if it drops by more than this fraction from the first observable price, the buy is aborted. Fraction: `0.60` = abort on >60% drop. `0` disables. |
| `tradingfilters.timer`                     | `180`                             | Seconds to wait after filters pass, before submitting the buy                                                            |

### Exit logic note

Four exit signals run simultaneously, **first to fire wins** — `max_hold_time`, stop loss, take profit, and dynamic trailing stop. `max_hold_time` takes priority over the others when both would fire on the same tick: if the timer has expired we exit, period, regardless of PnL. If `profit_target_percent` is below the lowest trailing tier, TP triggers first and trailing never engages. The bot prints a `WARN` at startup if it detects this. To rely on trailing exits, either raise `profit_target_percent` above the highest tier or set it to `0`. Any of `stop_loss_percent`, `profit_target_percent`, or `max_hold_time` can be set to `0` to disable that one signal.

## How it works

```
graduation event (PumpPortal subscribeMigration)
  -> filter: holder_count >= min_holders
  -> filter: bonding_curve_tx_count >= min_txs
  -> filter: top10_concentration <= top_ten_holder_percentage
  -> sleep tradingfilters.timer seconds
       (concurrent rug-watch: aborts buy if price drops > rug_percentage
        from the first observable PumpSwap price during the window)
  -> reserve a position slot (capped at max_positions)
  -> buy via /api/trade-local
       send_transaction_with_config (max_retries = buy_tx_retries)
       poll get_signature_statuses at "confirmed" until landed
  -> read post-buy token balance
       primary:  derived ATA at "confirmed" commitment
       fallback: getTokenAccountsByOwner filtered by mint
       wide retry window so finalization lag can't orphan the position
  -> derive entry price = trade_amount_sol / tokens_received
  -> Discord: BUY embed (mint, spent SOL, tokens, entry price, pump.fun link)
  -> discover PumpSwap pool once (cached): pool_id, base_vault, quote_vault
  -> price poll loop, every price_poll_interval_ms:
       1× getMultipleAccounts(base_vault, quote_vault)
       price  = (quote_lamports / 1e9) / (base_raw / 1e6)
       pct    = (price - entry) / entry * 100
       peak   = max(peak, pct)
       decide: max_hold_time | stop_loss | take_profit | dynamic_trail
       if triggered -> sell_all (amount="100%"), confirm, release slot
  -> Discord: SELL embed (reason, PnL %, net return SOL)
```

## Project structure

```
src/
  main.rs          orchestrator + CLI
  config.rs        typed loader for config.toml
  wallet.rs        load Keypair from Solana CLI JSON
  pumpportal.rs    subscribeMigration WS feed (graduations)
  pumpswap.rs      PumpSwap pool discovery + vault price math
  price_feed.rs    polling loop driving per-position price updates
  onchain.rs       RPC helpers: holder count, bonding-curve tx count, ATA balance (with fallback scan)
  trader.rs        PumpPortal /api/trade-local: buy + sell_all + send-and-confirm
  position.rs      Position state, trail tier parsing, exit decision
  monitor_pos.rs   per-position monitor + exit evaluator
  rug_watch.rs     pre-buy rug guard, racing against the tradingfilters.timer
  sniper.rs        per-graduation handler tying it all together
  discord.rs       optional webhook notifier (buy / sell / orphan-position alerts)
```

## Author

**Taki Hades Baker Alyasri**

## License

MIT
