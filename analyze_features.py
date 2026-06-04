#!/usr/bin/env python3
"""Join DIP_BUY SIGNAL fire-time features to outcomes across iter logs; find rug/loser discriminators."""
import re, sys
from collections import defaultdict, deque

sig_re  = re.compile(r"\[(\w{4}…\w{4})\] DIP_BUY SIGNAL: drawdown=([0-9.]+) quantile=([0-9.]+) pool_drain=([0-9.]+) pool_sol=([0-9.]+)")
sold_re = re.compile(r"\[(\w{4}…\w{4})\] SOLD \((.*?)\) pnl=([-+0-9.]+)%")

rows = []
for log in sys.argv[1:]:
    pending = defaultdict(deque)   # mint -> queue of feature dicts awaiting their SOLD
    for ln in open(log):
        m = sig_re.search(ln)
        if m:
            pending[m.group(1)].append(dict(mint=m.group(1), dd=float(m.group(2)),
                q=float(m.group(3)), pd=float(m.group(4)), ps=float(m.group(5))))
            continue
        m = sold_re.search(ln)
        if m and pending[m.group(1)]:
            r = pending[m.group(1)].popleft()
            reason = m.group(2)
            r["pnl"] = float(m.group(3))
            r["rug"] = reason.startswith("Rug")
            r["cls"] = "RUG" if r["rug"] else ("WIN" if r["pnl"] > 0 else "LOSE")
            rows.append(r)

print(f"{'mint':12} {'cls':5} {'pnl%':>7} {'dd':>5} {'q':>5} {'pd':>5} {'ps':>6}")
for r in sorted(rows, key=lambda x: x["cls"]):
    print(f"{r['mint']:12} {r['cls']:5} {r['pnl']:+7.1f} {r['dd']:5.2f} {r['q']:5.2f} {r['pd']:5.2f} {r['ps']:6.1f}")

def stats(cls):
    g = [r for r in rows if (r["cls"] in cls if isinstance(cls, tuple) else r["cls"] == cls)]
    if not g: return None
    out = {}
    for f in ("dd", "q", "pd", "ps"):
        vals = sorted(r[f] for r in g)
        out[f] = (min(vals), sum(vals)/len(vals), max(vals))
    return len(g), out

print(f"\n{'feature':8} | {'WINNERS (n,min/mean/max)':28} | {'RUGS+LOSERS':28}")
nw, w = stats("WIN"); nl, l = stats(("RUG", "LOSE"))
print(f"{'count':8} | n={nw:<26} | n={nl}")
for f in ("dd", "q", "pd", "ps"):
    print(f"{f:8} | {w[f][0]:.2f} / {w[f][1]:.2f} / {w[f][2]:.2f}{'':14} | {l[f][0]:.2f} / {l[f][1]:.2f} / {l[f][2]:.2f}")

# Isolate rugs specifically
nr, r = stats("RUG")
if nr:
    print(f"\nRUGS only (n={nr}):")
    for f in ("dd", "q", "pd", "ps"):
        print(f"  {f}: {r[f][0]:.2f} / {r[f][1]:.2f} / {r[f][2]:.2f}")
print(f"\ntotals: WIN={nw}  RUG+LOSE={nl}  (overall {len(rows)} trades)")
