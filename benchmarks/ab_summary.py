#!/usr/bin/env python3
"""Summarize CI A/B timings. Usage: ab_summary.py times.tsv BASE CAND GATE phases.txt CPU NOTES out.md out.json"""
import json
import statistics as st
import sys

tsv, base, cand, gate, phases, cpu, notes, out_md, out_json = sys.argv[1:10]
rows = [l.rstrip("\n").split("\t") for l in open(tsv) if l.strip()]
res = {"base": base, "cand": cand, "gate": gate, "cpu": cpu, "notes": notes, "sizes": {}}
md = [f"# A/B `{cand[:7]}` (candidate) vs `{base[:7]}` (base)", "",
      f"- Runner: {cpu}", f"- **Correctness gate: {gate.upper()}**" + (f": {notes}" if notes else ""), ""]
if rows:
    md += ["| Data | Base median (min–max) | Candidate median (min–max) | Δ median | Candidate faster in | Base / cand max RSS |",
           "|---|---:|---:|---:|---:|---:|"]
for d in ("d20", "d60"):
    a = [r for r in rows if r[0] == d and r[1] == "A"]
    b = [r for r in rows if r[0] == d and r[1] == "B"]
    if not a or not b:
        continue
    ta, tb = [float(r[2]) for r in a], [float(r[2]) for r in b]
    if any(r[4] != "0" for r in a + b):
        res["gate"] = gate = "fail"
    wins = sum(y < x for x, y in zip(ta, tb))
    delta = 100 * (st.median(tb) / st.median(ta) - 1)
    ra, rb = max(int(r[3]) for r in a) / 1048576, max(int(r[3]) for r in b) / 1048576
    res["sizes"][d] = {"base": ta, "cand": tb, "base_median": st.median(ta), "cand_median": st.median(tb),
                       "delta_pct": round(delta, 2), "cand_wins": wins, "pairs": len(ta),
                       "base_rss_gb": round(ra, 2), "cand_rss_gb": round(rb, 2)}
    md.append(f"| {d} | {st.median(ta):.3f} ({min(ta):.3f}–{max(ta):.3f}) | {st.median(tb):.3f} "
              f"({min(tb):.3f}–{max(tb):.3f}) | **{delta:+.1f}%** | {wins}/{len(ta)} | {ra:.2f} / {rb:.2f} GB |")
try:
    ph = open(phases).read().strip()
    if ph:
        md += ["", "Candidate phase log (last 60M run):", "```", ph, "```"]
except OSError:
    pass
md += ["", "Decision rule (OPTIMIZATION_PLAN.md): keep only if the gate passes, 20M improves, and 60M does not "
       "regress (the candidate is faster in most pairs and the median isn't worse by more than ~1%)."]
open(out_md, "w").write("\n".join(md) + "\n")
json.dump(res, open(out_json, "w"), indent=1)
