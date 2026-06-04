#!/usr/bin/env python3
"""Decompose iter-3 economics: per-position trough analysis + fixed/variable cost split."""
import re, sys

LOG = "logs/iter3-live.log"
lines = open(LOG).read().splitlines()

def abbr(mint):  # match log's [first4…last4] display form
    return mint[:4] + "…" + mint[-4:]

# Walk the log, reconstruct per-position lifecycles (handle re-entries on same mint).
open_pos = {}        # abbr -> dict(entry_idx, ticks=[])
closed = []          # list of dicts

buy_re  = re.compile(r"\[(\w{4}…\w{4})\] BOUGHT:")
tick_re = re.compile(r"PNL MONITOR: (\w+pump|\w+) \(.*?\) ([-+0-9.]+)%")
sold_re = re.compile(r"\[(\w{4}…\w{4})\] SOLD \((.*?)\) pnl=([-+0-9.]+)%")
exit_re = re.compile(r"\[(\w{4}…\w{4})\] EXIT: .*peak=([-+0-9.]+)%")

last_peak = {}
for ln in lines:
    m = buy_re.search(ln)
    if m:
        open_pos[m.group(1)] = {"ticks": []}
        continue
    m = tick_re.search(ln)
    if m:
        a = abbr(m.group(1));
        if a in open_pos:
            open_pos[a]["ticks"].append(float(m.group(2)))
        continue
    m = exit_re.search(ln)
    if m:
        last_peak[m.group(1)] = float(m.group(2)); continue
    m = sold_re.search(ln)
    if m:
        a, reason, pnl = m.group(1), m.group(2), float(m.group(3))
        p = open_pos.pop(a, {"ticks": []})
        ticks = p["ticks"]
        closed.append({
            "mint": a, "reason": reason.split("(")[0].strip(), "pnl": pnl,
            "trough": min(ticks) if ticks else pnl,
            "peak": last_peak.get(a, max(ticks) if ticks else pnl),
        })

print(f"{'mint':12} {'reason':14} {'final%':>8} {'trough%':>8} {'peak%':>7}")
for c in closed:
    print(f"{c['mint']:12} {c['reason']:14} {c['pnl']:+8.2f} {c['trough']:+8.2f} {c['peak']:+7.2f}")

winners = [c for c in closed if c["pnl"] > 0]
losers  = [c for c in closed if c["pnl"] <= 0]
print(f"\nn={len(closed)}  winners={len(winners)}  losers={len(losers)}")
print(f"avg winner {sum(c['pnl'] for c in winners)/len(winners):+.2f}%   "
      f"avg loser {sum(c['pnl'] for c in losers)/len(losers):+.2f}%   "
      f"sum {sum(c['pnl'] for c in closed):+.2f}%")

# Counterfactual: how many WINNERS would a tighter SL have killed (trough <= -X)?
for sl in (15, 18, 20):
    killed = [c for c in winners if c["trough"] <= -sl]
    saved  = [c for c in losers  if c["trough"] <= -sl and c["pnl"] < -sl]
    print(f"\nSL=-{sl}%:  winners killed (dipped below before recovering): {len(killed)} "
          f"(would forfeit {sum(c['pnl'] for c in killed):+.1f}%);  "
          f"losers capped earlier: {len(saved)} of {len(losers)}")

# Cost decomposition from aggregate ground truth
gross_sum_pct = sum(c["pnl"] for c in closed)
size = 0.1
gross_sol = gross_sum_pct/100*size
realized = 16.53902 - 16.61541
allcost = gross_sol - realized
priority = 0.001*2*len(closed)   # configured priority fee, both legs, FIXED
variable = allcost - priority
print(f"\n--- cost decomposition (size={size} SOL) ---")
print(f"gross price PnL:   {gross_sol:+.4f} SOL ({gross_sum_pct:+.2f}% summed)")
print(f"realized:          {realized:+.4f} SOL")
print(f"all-in cost:       {allcost:.4f} SOL  ({allcost/len(closed):.4f}/trade = {allcost/len(closed)/size*100:.1f}% of position)")
print(f"  fixed priority:  {priority:.4f} SOL  ({priority/len(closed)/size*100:.1f}% of position — DILUTES when sizing up)")
print(f"  variable (AMM+slip+rent): {variable:.4f} SOL  ({variable/len(closed)/size*100:.1f}% of position — SCALES with size)")
gross_per = gross_sum_pct/len(closed)
var_per = variable/len(closed)/size*100
print(f"\ngross edge/trade {gross_per:+.2f}%  vs variable cost/trade {var_per:.2f}%  -> "
      f"{'SIZING HELPS' if gross_per>var_per else 'SIZING HURTS (variable cost exceeds gross edge)'}")
