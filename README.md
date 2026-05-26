# hades-ps-trader

A Rust bot that listens for pump.fun → PumpSwap token graduations in real time, opens a per-token monitoring session, and trades the token over its monitoring window. It **enters** positions on a configurable MACD crossover strategy and **exits** them on a PnL-based rule set — hard stop-loss, take-profit, dynamic trailing stop, and a max-hold-time cap. Each session can buy and sell the same token any number of times within its monitoring window.

> **Warning — this bot moves real money.** Test with `--dry-run` first. Set `trade_amount` to a small value when you go live. Any position open at session end is force-sold via the standard sell flow.
>
> **⚠️ Educational use only — see the [Disclaimer](#disclaimer) before running this software.**

## Features

- Real-time graduation feed via PumpPortal `subscribeMigration`
- Per-graduation trading session
  - **Entry — MACD:** candle aggregator built from polled PumpSwap pool prices (no third-party indexers, no candle data subscription); classic MACD (configurable fast / slow / signal EMAs) on candle closes; buy on a bullish crossover
  - **Exit — PnL rules:** hard stop-loss, take-profit, tiered dynamic trailing stop, and a max-hold-time cap — evaluated on every price tick while a position is open
  - Cooldown between a sell and the next buy to filter whipsaw re-entries
  - No pyramiding — at most one open trade per token at a time
- Concurrency cap (`max_positions`) on open positions across all monitored tokens
- `max_monitor_time` per-token monitoring window; any open position is force-sold before the session ends
- Optional one-shot on-chain trading filters at graduation (holders, bonding-curve activity, top-10 concentration)
- Continuous rug guard — abandons tokens (force-selling any open position) on drained liquidity or collapsed price
- Optional capitulation dip-buy entry mode (alternative to MACD): per-tick rolling-window dip detector, picked via `[capitulation].enabled`
- Buy + sell via PumpPortal `/api/trade-local` (locally signed, you keep custody)
- `send_transaction_with_config` with RPC rebroadcast retries and explicit on-chain confirmation polling
- Slippage escalation on tx failure (buy: up to +10%, sell: up to +95% to avoid bag-holding)
- Direct PumpSwap pool reads for per-token price tracking — one `getMultipleAccounts` per poll
- Optional Discord webhook notifications on buy / sell / orphan-position events
- Auto-reconnect on WS drops with exponential backoff

## Prerequisites

| Tool                   | Version                                                           | Why                                                                  |
| ---------------------- | ----------------------------------------------------------------- | -------------------------------------------------------------------- |
| Rust                   | 1.85+ (edition 2024)                                              | Required by `edition = "2024"` in `Cargo.toml`                       |
| A Solana mainnet RPC   | any HTTP endpoint that supports `getProgramAccounts` with filters | QuickNode, Helius, Triton, etc. The free public RPC will not work.   |
| A PumpPortal account   | free                                                              | API key optional, recommended for a stable migration WS subscription |
| A funded Solana wallet | small balance to start                                            | Used as both the trader and the signer                               |
| A Discord webhook URL  | optional                                                          | Buy / sell / orphan notifications, off when blank                    |

## Setup

```bash
# 1. Clone
git clone <your-fork-url> hades-ps-trader
cd hades-ps-trader

# 2. Environment
cp .env.example .env
# edit .env:
#   SOLANA_RPC_URL=https://...your-rpc...       (required)
#   DISCORD_WEBHOOK_URL=https://discord...       (optional)
#   PUMPPORTAL_API_KEY=...                       (optional, recommended for stable WS)
#   RUST_LOG=info                                (set to "debug" to see per-tick price polls and bar updates)

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

Logs are controlled by `RUST_LOG` (defaults to `info`, can be set in `.env`). Use `RUST_LOG=debug` to see every price poll, candle close, and MACD update; use `RUST_LOG=hades_ps_trader=debug` to keep upstream crates (solana-client, reqwest) quiet.

## Configuration

`config.toml`:

### `[trading]`

| Key                                        | Example         | Purpose                                                                                                                                                                                                                                       |
| ------------------------------------------ | --------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `wallet_path`                              | `"wallet.json"` | Path to Solana CLI keypair JSON (top-level key, not under `[trading]`)                                                                                                                                                                        |
| `trading.slippage`                         | `10.0`          | Starting slippage tolerance %. Sells escalate up to +95% in 5% steps; buys escalate up to +10%.                                                                                                                                               |
| `trading.trade_amount`                     | `250_000_000`   | Lamports per buy (0.25 SOL). Each MACD bullish crossover spends this.                                                                                                                                                                         |
| `trading.priority_fee_sol`                 | `0.0005`        | Priority fee per tx in SOL                                                                                                                                                                                                                    |
| `trading.profit_target_percent`            | `0`             | Take-profit: exit once PnL reaches this `%` gain. `0` disables it.                                                                                                                                                                            |
| `trading.stop_loss_percent`                | `30.0`          | Hard stop-loss: exit once PnL falls to this `%` loss. `0` disables it.                                                                                                                                                                        |
| `trading.max_positions`                    | `3`             | Concurrent OPEN positions cap across all monitored tokens. A monitored token without an open trade does NOT consume a slot.                                                                                                                   |
| `trading.buy_tx_retries`                   | `5`             | RPC-side rebroadcast retries for the buy tx (`maxRetries` in `RpcSendTransactionConfig`). Bot then polls for on-chain confirmation.                                                                                                           |
| `trading.sell_tx_retries`                  | `5`             | Same as above, for the sell tx.                                                                                                                                                                                                               |
| `trading.price_poll_interval_ms`           | `500`           | How often the price feed reads the PumpSwap pool's vaults to compute price. Each read = one `getMultipleAccounts`. Each tick feeds the candle aggregator.                                                                                     |
| `trading.pnl_log_every_n_ticks`            | `5`             | Emit a `PNL MONITOR: <mint> (TICKER) +X.XX%` info line every Nth price tick while a position is open. Set `0` to disable (per-tick debug log unaffected).                                                                                     |
| `trading.max_monitor_time`                 | `3600`          | Total seconds to monitor a single token after graduation. Within this window the bot may buy and sell that token any number of times. When the window expires, no new buys; any open position is force-sold. Set `0` to monitor indefinitely. |
| `trading.max_hold_time`                    | `900`           | Per-position cap: max seconds to hold a single open position before exiting it. Timer starts when the buy tx confirms. Distinct from `max_monitor_time` (the per-token session window). Set `0` to disable.                                   |
| `trading.dynamic_trailing_stop_thresholds` | `"20:8,40:13"`  | Tiered trailing stop. Comma-separated `gain%:trail%` pairs. Once peak PnL crosses a tier's `gain%`, the position exits if PnL drops `trail%` back from its peak. Higher tiers widen the trail so a strong runner isn't cut early.             |

### `[macd]`

| Key                         | Example | Purpose                                                                                                                                   |
| --------------------------- | ------- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| `macd.candle_interval_secs` | `60`    | Candle (bar) timeframe. MACD runs on candle closes. Smaller = more signals + more noise; larger = fewer, cleaner signals + longer warmup. |
| `macd.fast`                 | `12`    | Fast EMA period.                                                                                                                          |
| `macd.slow`                 | `26`    | Slow EMA period.                                                                                                                          |
| `macd.signal`               | `9`     | Signal-line EMA period (EMA of the MACD line itself).                                                                                     |
| `macd.cooldown_secs`        | `30`    | Minimum seconds between a sell and the next buy on the same token. Filters whipsaw re-entries on noisy crossovers. Set `0` to disable.    |

### `[tradingfilters]`

Run **once** at graduation, before monitoring starts. If a filter fails, the token is not monitored at all.

| Key                                        | Example | Purpose                                                                                                                                                                           |
| ------------------------------------------ | ------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `tradingfilters.enabled`                   | `false` | Master switch. `false` skips every check.                                                                                                                                         |
| `tradingfilters.min_holders`               | `100`   | Minimum holders required; `0` disables this check.                                                                                                                                |
| `tradingfilters.min_txs`                   | `250`   | Minimum bonding-curve trade signatures required (page-walked up to 5000, then capped); `0` disables.                                                                              |
| `tradingfilters.top_ten_holder_percentage` | `0.30`  | Reject if combined top-10 holder balance exceeds this fraction of total supply (PumpSwap pool vault + bonding-curve PDA excluded). Fraction in [0, 1]: `0.05` = 5%. `0` disables. |

### `[rug]`

Continuous rug guard. Evaluated **on every price tick** against the price + pool-liquidity history gathered since the session started. When it fires, the session force-sells any open position (reason `"Rug detected"`) and abandons the token — no more RPC polls for it. Independent of `[tradingfilters]` (which run once at graduation) — this runs throughout the session. If the whole `[rug]` section is omitted, the defaults below apply.

| Key                          | Example | Purpose                                                                                                                                                                         |
| ---------------------------- | ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `rug.enabled`                | `true`  | Master switch. `false` never blocks a buy.                                                                                                                                      |
| `rug.min_pool_sol`           | `5.0`   | Minimum WSOL liquidity (in SOL) the PumpSwap pool must hold. A pulled-liquidity rug drains this toward zero. Below the floor → abandon the token. Tune to healthy graduated-pool sizes. `0` disables. |
| `rug.max_liquidity_drop_pct` | `50.0`  | Abandon the token if pool liquidity is down more than this % from its session peak (liquidity being removed). `0` disables.                                                                            |
| `rug.max_drawdown_pct`       | `70.0`  | Abandon the token if price is down more than this % from the session high (token already pumped and dumped). `0` disables.                                                                             |
| `rug.min_samples`            | `20`    | Minimum price samples gathered before the guard will judge a token. Avoids false positives on a near-empty history.                                                                                    |

### `[capitulation]`

Optional alternative entry mode. When `enabled = true`, MACD bullish crossovers no longer fire buys — capitulation **replaces** MACD as the entry trigger. (MACD candle aggregation and logging continue, but only for analysis.) The detector is a **two-stage state machine**: it watches for a sustained dump, then waits for a confirmed bounce off the local low before firing. If this whole section is omitted, defaults apply (the detector is **disabled**).

**STAGE 1 — detect dump:** When current price is at least `dip_pct` below the price at the start of a `window_secs` rolling window, record the local low (`pending_low`). Continue tracking the lowest price during the dump. Does NOT fire.

**STAGE 2 — wait for confirmed reclaim:** When price reclaims at least `reclaim_pct` above `pending_low` AND sustains that reclaim for `reclaim_confirm_secs` continuously → fire a buy. Falling back below the reclaim threshold resets the confirmation timer. If no confirmed reclaim within `pending_expire_secs` of the dump → abandon the pending state and reset.

After a fire the detector is debounced for `debounce_secs` (post-sell `cooldown_secs` from `[macd]` still applies as a separate gate). Defaults for `dip_pct=50, window_secs=60` came from the `analysis/capitulation.py` sweep on test5+test6; STAGE-2 defaults (`reclaim_pct=10`, `reclaim_confirm_secs=10`, `pending_expire_secs=30`) are placeholders pending a sweep for the reclaim variant.

| Key                                  | Example | Purpose                                                                                                                                                                                |
| ------------------------------------ | ------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `capitulation.enabled`               | `false` | Master switch. `false` (default) → MACD remains the entry trigger and this detector never runs.                                                                                        |
| `capitulation.dip_pct`               | `50.0`  | STAGE 1. Required dip size, in percent. Pending state begins when current price is at least this many percent below the price at the start of the window.                              |
| `capitulation.window_secs`           | `60`    | STAGE 1. Rolling-window length. The detector compares current price to the OLDEST tick still inside this window.                                                                       |
| `capitulation.reclaim_pct`           | `10.0`  | STAGE 2. Required reclaim off the local low, in percent. Larger waits for stronger bounces but misses fast V-recoveries; smaller catches weaker bounces with more false starts.        |
| `capitulation.reclaim_confirm_secs`  | `10`    | STAGE 2. How many seconds the reclaim threshold must be continuously sustained before firing. Filters spike-and-fade fake bounces. `0` fires on the first reclaim tick.                |
| `capitulation.pending_expire_secs`   | `30`    | How long to keep watching for a reclaim after STAGE 1 trips. If no confirmed reclaim within this window, the pending dump is abandoned and the detector resets.                        |
| `capitulation.debounce_secs`         | `60`    | Post-fire debounce — suppresses re-firing on the same session. Independent of post-sell `cooldown_secs` from `[macd]`, which still applies after an exit.                              |

## How it works

```
graduation event (PumpPortal subscribeMigration)
  -> optional one-shot filters: holder_count, bonding-curve tx count, top-10 concentration
       any fail -> token not monitored
  -> spawn per-token session:
       price-poll task reads PumpSwap pool every price_poll_interval_ms

       RUG GUARD — runs on every price tick after observing it. If pool
       liquidity falls below the floor / drops too far from its session peak,
       or price collapses too far from its session high:
           - position open? -> force-sell at market (reason "Rug detected"),
                               then abandon the session
           - no position?  -> abandon the session immediately
       Either way the token is dropped from monitoring — no more RPC polls.

       ENTRY — one of two modes, picked by [capitulation].enabled:

         MODE A (default): MACD bullish crossover
           each tick feeds the candle aggregator (bar = candle_interval_secs);
           on each candle close MACD updates its fast / slow / signal EMAs:
             on bullish crossover:
               - already in a trade on this token?  -> skip (no pyramiding)
               - inside cooldown_secs of last sell? -> skip
               - max_positions full?                -> skip, keep watching
               - else                               -> reserve slot, buy
                                                       resolve fill via ATA (with by-mint scan fallback)
                                                       Discord: BUY embed
             on bearish crossover                   -> no action (exits are PnL-driven)

         MODE B ([capitulation].enabled = true): dip-and-reclaim (two stage)
           STAGE 1: per-tick, if current price is at least dip_pct below the
             OLDEST tick still inside a window_secs rolling window, record
             the local low (pending_low). Do NOT fire. Track the lowest
             price while the dump deepens. If pending_expire_secs elapses
             without a confirmed reclaim, abandon the pending state.
           STAGE 2: once pending, watch for price to bounce reclaim_pct
             above pending_low AND sustain that bounce for
             reclaim_confirm_secs continuously. Falling back below the
             reclaim threshold resets the confirmation timer. On confirm,
             fire a buy (subject to the same no-pyramiding / cooldown /
             max_positions gates as MACD). After firing, debounce for
             debounce_secs.
           MACD candle aggregation and logging continue but bullish
           crossovers no longer trigger buys.

       EXIT — while a position is open, every price tick checks (first match wins):
         1. max_hold_time elapsed since the buy            -> sell
         2. PnL <= -stop_loss_percent                      -> sell
         3. PnL >= profit_target_percent                   -> sell
         4. peak PnL crossed a trail tier and PnL has
            since fallen trail% back from that peak        -> sell
         on sell: sell ALL (amount="100%"), release slot, record sell time
                  Discord: SELL embed

  -> max_monitor_time elapsed:
       stop accepting new signals
       any open position is force-sold via the standard sell flow
       session ends
```

Notes:

- **MACD warmup** needs `slow + signal` candles before any signals can fire. At defaults (60s × 35 bars) that is ~35 minutes. The bot prints a `WARN` at startup if the warmup consumes ≥50% of `max_monitor_time`.
- **No pyramiding.** At most one open trade per token at any time. A bullish crossover while already holding is ignored.
- **Exits are PnL-driven, not MACD.** A bearish MACD crossover no longer sells. An open position is closed only by the four exit rules above (checked every price tick), or by the session-end force-sell.
- **Exit priority** is fixed: `max_hold_time` → `stop_loss` → `profit_target` → trailing stop. The first rule that trips wins. If `profit_target_percent` is set _below_ the lowest trailing tier's gain, the take-profit always fires first and trailing never engages — the bot prints a `WARN` at startup if so.
- **Cooldown** is applied only after a sell, not after the first buy in a session.
- **`max_positions` only counts open positions.** Monitoring many tokens simultaneously is fine; the cap kicks in only when a buy would push you past it.
- **`max_hold_time` vs `max_monitor_time`.** `max_hold_time` caps a single open position; `max_monitor_time` bounds the whole per-token session (which may open and close several positions). A position can be ended by either.
- **Force-sell on session end** uses the full slippage-escalation chain (up to +95%). If even that fails, the bot logs loudly and exits — the position remains on-chain and must be sold manually.

## Project structure

```
src/
  main.rs        orchestrator + CLI
  config.rs      typed loader for config.toml
  wallet.rs      load Keypair from Solana CLI JSON
  pumpportal.rs  subscribeMigration WS feed (graduations)
  pumpswap.rs    PumpSwap pool discovery + vault price math
  price_feed.rs  polling loop driving per-token price updates
  macd.rs        EMA, MACD indicator, candle aggregator, crossover detector
  onchain.rs     RPC helpers: holder count, bonding-curve tx count, ATA balance (with fallback scan), top-10 concentration, Metaplex / Token-2022 metadata
  trader.rs      PumpPortal /api/trade-local: buy + sell_all + send-and-confirm with slippage escalation
  position.rs    Position state + PnL exit rules (stop-loss / take-profit / trailing / max-hold)
  session.rs     per-graduation session: filters, candle loop, MACD entry, PnL exit, force-exit
  discord.rs     optional webhook notifier (buy / sell / orphan-position alerts)
```

## Disclaimer

**This software is provided for educational and informational purposes only.**

- **Not financial advice.** Nothing in this repository — code, comments, documentation, or examples — constitutes financial, investment, trading, legal, or tax advice. It is a technical demonstration of automated trading concepts.
- **Use entirely at your own risk.** Cryptocurrency trading is extremely high risk. Automated trading of newly graduated pump.fun tokens is especially speculative and you should assume you can lose **100% of any funds the bot has access to**. Never run this bot with money you cannot afford to lose entirely.
- **No warranty.** This software is provided "AS IS", without warranty of any kind, express or implied. It may contain bugs, may execute trades incorrectly, may fail to sell a position, and may lose money — including through software defects, network or RPC failures, slippage, or adverse market conditions.
- **No liability.** To the maximum extent permitted by law, the author(s) and contributors shall not be liable for any claim, damages, or other liability — including but not limited to any financial losses, lost profits, or lost funds — arising from or in connection with the use of, or inability to use, this software.
- **You are solely responsible** for how you use this software, for securing your wallet and private keys, for any funds placed at risk, and for complying with all laws, regulations, and third-party terms of service (including those of pump.fun, PumpSwap, PumpPortal, and your RPC provider) applicable in your jurisdiction.

By using, running, modifying, or distributing this software, you acknowledge that you have read and understood this disclaimer and accept full responsibility for the outcomes.

## Author

**Taki Hades Baker Alyasri**

## License

MIT — see the [Disclaimer](#disclaimer) above. The MIT license's "AS IS", no-warranty, and no-liability terms apply to all use of this software.
