#!/usr/bin/env python3
"""
Integrity constraints every correct output must satisfy (usable at any scale, no oracle):
  * schema: exact column sets, `day` is DATE, integer counters
  * keys: (user_id, day) unique in user_daily; (campaign_id, day) unique in campaign_daily
  * domains: counts >= 0, sessions >= 1 in user_daily, events >= sessions,
    top_category null <=> revenue_cents == 0, purchasing_sessions <= sessions,
    organic <=> channel organic, organic assisted_sessions == 0
  * reconciliation (rules.md self-check): global sessions and revenue equal across
    tables; total assisted >= credited non-organic sessions
  * window: every day lies in [window_start day, window_end day)
Usage: invariants.py OUT_DIR [DATA_DIR]
"""
import json
import os
import sys

import polars as pl

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "scripts"))
from run_fixtures import SCHEMAS, load_actual  # noqa: E402


def main():
    out = sys.argv[1]
    u = load_actual(os.path.join(out, "user_daily"), SCHEMAS["user_daily"])
    c = load_actual(os.path.join(out, "campaign_daily"), SCHEMAS["campaign_daily"])
    checks = []

    def check(name, ok):
        checks.append((name, bool(ok)))

    check("user_daily key unique", u.height == u.n_unique(subset=["user_id", "day"]))
    check("campaign_daily key unique", c.height == c.n_unique(subset=["campaign_id", "day"]))
    check("user sessions >= 1", u.filter(pl.col("sessions") < 1).height == 0)
    check("events >= sessions", u.filter(pl.col("events") < pl.col("sessions")).height == 0)
    check("revenue >= 0", u.filter(pl.col("revenue_cents") < 0).height == 0)
    check("top_category null <=> revenue 0",
          u.filter(pl.col("top_category").is_null() != (pl.col("revenue_cents") == 0)).height == 0)
    check("purchasing <= sessions", c.filter(pl.col("purchasing_sessions") > pl.col("sessions")).height == 0)
    check("purchasing 0 => revenue 0",
          c.filter((pl.col("purchasing_sessions") == 0) & (pl.col("revenue_cents") != 0)).height == 0)
    check("organic <=> channel organic",
          c.filter((pl.col("campaign_id") == "organic") != (pl.col("channel") == "organic")).height == 0)
    check("organic never assists", c.filter((pl.col("campaign_id") == "organic") & (pl.col("assisted_sessions") != 0)).height == 0)
    check("every row credited or assisted", c.filter((pl.col("sessions") == 0) & (pl.col("assisted_sessions") == 0)).height == 0)
    check("sessions reconcile", u["sessions"].sum() == c["sessions"].sum())
    check("revenue reconcile", u["revenue_cents"].sum() == c["revenue_cents"].sum())
    credited = c.filter(pl.col("campaign_id") != "organic")["sessions"].sum()
    check("assisted >= credited non-organic", c["assisted_sessions"].sum() >= credited)
    if len(sys.argv) > 2:
        m = json.load(open(os.path.join(sys.argv[2], "manifest.json")))
        lo, hi = pl.Series([m["window_start"][:10], m["window_end"][:10]]).str.to_date()
        check("days inside window", u.filter((pl.col("day") < lo) | (pl.col("day") >= hi)).height == 0
              and c.filter((pl.col("day") < lo) | (pl.col("day") >= hi)).height == 0)
    for name, ok in checks:
        print(("ok   " if ok else "FAIL ") + name)
    print(f"user_daily {u.height} rows, campaign_daily {c.height} rows, sessions {u['sessions'].sum()}, "
          f"revenue {u['revenue_cents'].sum()}")
    sys.exit(0 if all(ok for _, ok in checks) else 1)


if __name__ == "__main__":
    main()
