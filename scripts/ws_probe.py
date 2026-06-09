#!/usr/bin/env python3
"""One-off: confirm whether PumpPortal subscribeAccountTrade streams SELL frames
for our followed wallets, and whether sells carry newTokenBalance. Redacts
addresses. Reads creds from .env. Not committed-secret-safe to print frames raw,
so we only print txType counts + the KEY SET of a sample sell."""
import asyncio, json, sys, time, ssl, os
import websockets

def ssl_ctx():
    ctx = ssl.create_default_context()
    for p in ("/etc/ssl/cert.pem", "/private/etc/ssl/cert.pem"):
        if os.path.exists(p):
            try:
                ctx.load_verify_locations(p); return ctx
            except Exception:
                pass
    try:
        import certifi
        ctx.load_verify_locations(certifi.where()); return ctx
    except Exception:
        return ctx

def load_env(path=".env"):
    env = {}
    for line in open(path):
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, v = line.split("=", 1)
        env[k] = v.strip().strip('"')
    return env

async def main(run_secs=120):
    env = load_env()
    key = env.get("PUMPPORTAL_API_KEY", "")
    wallets = [w.strip() for w in env.get("COPY_TRADE_WALLETS", "").split(",") if w.strip()]
    url = f"wss://pumpportal.fun/api/data?api-key={key}" if key else "wss://pumpportal.fun/api/data"
    counts = {"buy": 0, "sell": 0, "other": 0}
    sample_sell_keys = None
    sample_sell_redacted = None
    async with websockets.connect(url, ssl=ssl_ctx()) as ws:
        await ws.send(json.dumps({"method": "subscribeAccountTrade", "keys": wallets}))
        print(f"subscribed for {len(wallets)} wallets; listening {run_secs}s...", flush=True)
        end = time.time() + run_secs
        while time.time() < end:
            try:
                msg = await asyncio.wait_for(ws.recv(), timeout=end - time.time())
            except asyncio.TimeoutError:
                break
            try:
                v = json.loads(msg)
            except Exception:
                continue
            if "errors" in v or "message" in v:
                print("CONTROL:", v, flush=True)
                continue
            tt = v.get("txType")
            if tt == "buy":
                counts["buy"] += 1
            elif tt == "sell":
                counts["sell"] += 1
                if sample_sell_keys is None:
                    sample_sell_keys = sorted(v.keys())
                    red = dict(v)
                    for f in ("mint", "traderPublicKey", "signature", "bondingCurveKey", "pool"):
                        if f in red and isinstance(red[f], str):
                            red[f] = red[f][:4] + "…"
                    sample_sell_redacted = red
            else:
                counts["other"] += 1
    print("COUNTS:", counts, flush=True)
    print("sample sell keys:", sample_sell_keys, flush=True)
    print("sample sell (redacted):", json.dumps(sample_sell_redacted), flush=True)
    print("newTokenBalance in sell?:",
          (sample_sell_redacted or {}).get("newTokenBalance", "<<ABSENT>>"), flush=True)

asyncio.run(main(int(sys.argv[1]) if len(sys.argv) > 1 else 120))
