#!/usr/bin/env python3
"""
Shopping Sessions: independent, deliberately naive reference implementation.

Purpose: a TEST ORACLE, not the submission. It is written line by line from
docs/rules.md with no performance tricks, so the fast Rust engine can be
checked against a second, independently written program (N-version testing).
Everything works on raw bytes, so "byte-wise smallest" is Python's bytes order.

Usage: reference.py DATA_DIR OUT_DIR   (writes OUT_DIR/{user_daily,campaign_daily}/part-0.parquet)
"""
import json
import os
import sys
from collections import defaultdict

import pyarrow as pa
import pyarrow.parquet as pq

DAY = 86_400_000
MIN30 = 1_800_000
WEEK = 7 * DAY
DIGITS = b"0123456789"


# ---------------------------------------------------------------- CSV dialect
def read_rows(path, nfields):
    """Yield the parsed data rows of one file: lists of bytes-or-None. Header skipped."""
    with open(path, "rb") as f:
        data = f.read()
    lines = data.split(b"\n")
    if data.endswith(b"\n"):
        lines.pop()  # the empty string after the final newline is not a line
    for line in lines[1:]:  # line 0 is the header
        row = parse_line(line, nfields)
        if row is not None:
            yield row


def parse_line(line, nfields):
    if line == b"":
        return None
    parts = line.split(b",")
    if len(parts) != nfields:
        return None
    out = []
    for p in parts:
        if b'"' in p:
            if len(p) >= 2 and p[:1] == b'"' and p[-1:] == b'"' and b'"' not in p[1:-1]:
                p = p[1:-1]
            else:
                return None
        out.append(p if p != b"" else None)
    return out


def csv_files(folder):
    if not os.path.isdir(folder):
        return []
    return sorted(
        os.path.join(folder, n) for n in os.listdir(folder)
        if n.endswith(".csv") and not n.startswith(".") and os.path.isfile(os.path.join(folder, n))
    )


# ---------------------------------------------------------------- timestamps
def is_digits(b):
    return len(b) > 0 and all(c in DIGITS for c in b)


def days_from_civil(y, m, d):
    y -= m <= 2
    era = y // 400
    yoe = y - era * 400
    doy = (153 * ((m + 9) % 12) + 2) // 5 + d - 1
    doe = yoe * 365 + yoe // 4 - yoe // 100 + doy
    return era * 146097 + doe - 719468


def dim(y, m):
    if m == 2:
        return 29 if (y % 4 == 0 and (y % 100 != 0 or y % 400 == 0)) else 28
    return 30 if m in (4, 6, 9, 11) else 31


def parse_ts(b):
    """bytes -> epoch ms (Python int, may be huge for F3) or None."""
    if b is None:
        return None
    if is_digits(b):  # F3
        return int(b)
    if len(b) == 20 and b[19:] == b"Z":
        off = 0
    elif len(b) == 25 and b[19:20] in (b"+", b"-") and b[22:23] == b":":
        oh, om = b[20:22], b[23:25]
        if not (is_digits(oh) and is_digits(om)):
            return None
        oh, om = int(oh), int(om)
        if oh > 23 or om > 59:
            return None
        off = (oh * 60 + om) * 60_000 * (1 if b[19:20] == b"+" else -1)
    else:
        return None
    s = b[:19]
    if s[4:5] != b"-" or s[7:8] != b"-" or s[10:11] != b"T" or s[13:14] != b":" or s[16:17] != b":":
        return None
    parts = [s[0:4], s[5:7], s[8:10], s[11:13], s[14:16], s[17:19]]
    if not all(is_digits(p) for p in parts):
        return None
    y, mo, d, h, mi, se = (int(p) for p in parts)
    if not (1 <= mo <= 12 and 1 <= d <= dim(y, mo) and h <= 23 and mi <= 59 and se <= 59):
        return None
    return days_from_civil(y, mo, d) * DAY + ((h * 60 + mi) * 60 + se) * 1000 - off


# ---------------------------------------------------------------- the rules
def run(data, out):
    with open(os.path.join(data, "manifest.json"), "rb") as f:
        man = json.load(f)
    ws = parse_ts(man["window_start"].encode())
    we = parse_ts(man["window_end"].encode())
    assert ws is not None and we is not None, "manifest window unparseable"

    products = {}
    for r in read_rows(os.path.join(data, "products.csv"), 2):
        if r[0] is not None and r[0] not in products:
            products[r[0]] = r[1] if r[1] is not None else b""
    campaigns = {}
    for r in read_rows(os.path.join(data, "campaigns.csv"), 2):
        if r[0] is not None and r[0] not in campaigns:
            campaigns[r[0]] = r[1] if r[1] is not None else b""

    # Linked IDs: union-find, name = byte-wise smallest member.
    parent = {}

    def find(x):
        root = x
        while parent.get(root, root) != root:
            root = parent[root]
        while parent.get(x, x) != root:
            parent[x], x = root, parent[x]
        return root

    for path in csv_files(os.path.join(data, "links")):
        for a, b in read_rows(path, 2):
            if a is None or b is None:
                continue
            ra, rb = find(a), find(b)
            if ra != rb:
                # keep the smaller as root, so the root is always the component's name
                if rb < ra:
                    ra, rb = rb, ra
                parent[rb] = ra

    def person(uid):
        return find(uid)

    # Events: parse -> dedup (smallest ingest_ts) -> validate -> late.
    best = {}
    for path in csv_files(os.path.join(data, "events")):
        for r in read_rows(path, 7):
            if r[0] is None:
                continue
            ing = parse_ts(r[3])
            key = (0, ing, tuple(v or b"" for v in r)) if ing is not None else (1, 0, tuple(v or b"" for v in r))
            cur = best.get(r[0])
            if cur is None or key < cur[0]:
                best[r[0]] = (key, r, ing)
    events = defaultdict(list)  # person -> [(ts, type, category, price)]
    for _, (_, r, ing) in best.items():
        _, user, ts_b, _, typ, prod, price_b = r
        if user is None or prod is None:
            continue
        ts = parse_ts(ts_b)
        if ts is None or not (ws <= ts < we):
            continue
        if typ not in (b"view", b"cart", b"purchase"):
            continue
        price = 0
        if typ == b"purchase":
            if price_b is None or not is_digits(price_b):
                continue
            price = int(price_b)
            if not (1 <= price <= 10_000_000):
                continue
        if ing is None or ing - ts > DAY:
            continue
        events[person(user)].append((ts, typ, products.get(prod, b"unknown"), price))

    # Clicks: parse -> dedup (any copy; deterministic: smallest tuple) -> validate.
    cbest = {}
    for path in csv_files(os.path.join(data, "clicks")):
        for r in read_rows(path, 4):
            if r[0] is None:
                continue
            key = tuple(v or b"" for v in r)
            cur = cbest.get(r[0])
            if cur is None or key < cur[0]:
                cbest[r[0]] = (key, r)
    clicks = defaultdict(list)  # person -> [(ts, click_id, campaign)]
    for _, (_, r) in cbest.items():
        cid, user, camp, ts_b = r
        if user is None or camp is None or camp not in campaigns:
            continue
        ts = parse_ts(ts_b)
        if ts is None or not (ws - WEEK <= ts < we):
            continue
        clicks[person(user)].append((ts, cid, camp))

    # Sessions, attribution, assists, aggregation.
    ud = {}
    cd = defaultdict(lambda: [0, 0, 0, 0])
    for p, evs in events.items():
        evs.sort(key=lambda e: e[0])
        sessions = []
        for e in evs:
            if sessions and e[0] - sessions[-1][-1][0] <= MIN30:
                sessions[-1].append(e)
            else:
                sessions.append([e])
        pcl = clicks.get(p, [])
        for s in sessions:
            start = s[0][0]
            day = start // DAY
            rev = sum(e[3] for e in s if e[1] == b"purchase")
            has_p = any(e[1] == b"purchase" for e in s)
            row = ud.setdefault((p, day), [0, 0, 0, defaultdict(int)])
            row[0] += 1
            row[1] += len(s)
            row[2] += rev
            for e in s:
                if e[1] == b"purchase":
                    row[3][e[2]] += e[3]
            q = [c for c in pcl if start - WEEK <= c[0] <= start]
            if q:
                mts = max(c[0] for c in q)
                win = min(c[1] for c in q if c[0] == mts)
                camp = next(c[2] for c in q if c[0] == mts and c[1] == win)
            else:
                camp = None
            acc = cd[(camp, day)]
            acc[0] += 1
            acc[1] += 1 if has_p else 0
            acc[2] += rev
            for a in {c[2] for c in q}:
                cd[(a, day)][3] += 1

    # Write Parquet.
    def s(b):
        return b.decode("utf-8", errors="replace")

    import datetime
    epoch = datetime.date(1970, 1, 1)
    urows = []
    for (p, day), (n, ne, rev, cats) in ud.items():
        top = None
        if cats:
            top = min(cats.items(), key=lambda kv: (-kv[1], kv[0]))[0]
        urows.append((s(p), epoch + datetime.timedelta(days=day), n, ne, rev, None if top is None else s(top)))
    crows = []
    for (camp, day), (n, npur, rev, ass) in cd.items():
        cid = "organic" if camp is None else s(camp)
        ch = "organic" if camp is None else s(campaigns[camp])
        crows.append((cid, ch, epoch + datetime.timedelta(days=day), n, npur, rev, ass))

    us = pa.schema([("user_id", pa.string()), ("day", pa.date32()), ("sessions", pa.int64()),
                    ("events", pa.int64()), ("revenue_cents", pa.int64()), ("top_category", pa.string())])
    cs = pa.schema([("campaign_id", pa.string()), ("channel", pa.string()), ("day", pa.date32()),
                    ("sessions", pa.int64()), ("purchasing_sessions", pa.int64()),
                    ("revenue_cents", pa.int64()), ("assisted_sessions", pa.int64())])
    for name, schema, rows in (("user_daily", us, urows), ("campaign_daily", cs, crows)):
        os.makedirs(os.path.join(out, name), exist_ok=True)
        cols = list(zip(*rows)) if rows else [[] for _ in schema]
        t = pa.table([pa.array(list(c), type=f.type) for c, f in zip(cols, schema)], schema=schema)
        pq.write_table(t, os.path.join(out, name, "part-0.parquet"))


if __name__ == "__main__":
    d = sys.argv[1] if len(sys.argv) > 2 else os.environ.get("DATA_DIR", "/data")
    o = sys.argv[2] if len(sys.argv) > 2 else os.environ.get("OUTPUT_DIR", "/output")
    run(d, o)
