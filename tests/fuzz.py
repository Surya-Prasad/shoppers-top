#!/usr/bin/env python3
"""
Differential fuzzer: random adversarial mini-datasets, every one run through the Rust
engine AND the independent Python reference; any difference is a failure.

Datasets stay inside the rules' promises (equal-ingest event copies are identical,
click copies are identical, products/campaigns are clean, no category is named
`unknown`) but push hard on everything the rules leave to the data: dialect noise,
every timestamp form and bad form, window and 7-day edges, 30-minute gaps, ties,
duplicates, case/space variants, \\r, Unicode digits, link chains, header-like rows,
empty and header-only files, missing trailing newlines.

Usage: fuzz.py BINARY [N_CASES] [SEED]       failing cases are kept under fuzz-failures/
"""
import os
import random
import shutil
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "scripts"))
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "reference"))
import polars as pl  # noqa: E402
from run_fixtures import SCHEMAS, load_actual, rows  # noqa: E402
import reference  # noqa: E402

DAY = 86_400_000
MIN = 60_000


def civil(ms):
    days, rem = divmod(ms, DAY)
    z = days + 719468
    era = z // 146097
    doe = z - era * 146097
    yoe = (doe - doe // 1460 + doe // 36524 - doe // 146096) // 365
    doy = doe - (365 * yoe + yoe // 4 - yoe // 100)
    mp = (5 * doy + 2) // 153
    d = doy - (153 * mp + 2) // 5 + 1
    m = mp + 3 if mp < 10 else mp - 9
    y = yoe + era * 400 + (m <= 2)
    s = rem // 1000
    return y, m, d, s // 3600, s // 60 % 60, s % 60


def f1(ms):
    y, m, d, h, mi, s = civil(ms)
    return f"{y:04d}-{m:02d}-{d:02d}T{h:02d}:{mi:02d}:{s:02d}Z"


def f2(ms, off_min):
    y, m, d, h, mi, s = civil(ms + off_min * MIN)
    sign = "-" if off_min < 0 else "+"
    o = abs(off_min)
    return f"{y:04d}-{m:02d}-{d:02d}T{h:02d}:{mi:02d}:{s:02d}{sign}{o // 60:02d}:{o % 60:02d}"


class Case:
    def __init__(self, r: random.Random):
        self.r = r
        # window: random midnight (sometimes not midnight), 1..16 days, years 1971..2099
        year = r.choice([1971, 2000, 2024, 2025, 2026, 2027, 2028, 2100 - 1, 2028])
        base = reference.days_from_civil(year, r.randint(1, 12), 1) * DAY
        if r.random() < 0.2:
            base = reference.days_from_civil(2028, 2, r.randint(20, 29)) * DAY  # leap-day windows
        if r.random() < 0.1:
            base += r.randint(1, DAY - 1) // 1000 * 1000
        self.ws = base
        self.we = base + r.randint(1, 16) * DAY
        n_users = r.randint(1, 8)
        base_users = [f"u{i}" for i in range(1, n_users + 1)]
        variants = []
        for u in base_users[:3]:
            variants += [u.upper(), " " + u, u + " ", u + "\t", "ü" + u]
        self.users = base_users + r.sample(variants, k=min(len(variants), r.randint(0, 4)))
        self.products = {f"p{i}": r.choice(["shoes", "books", "a", "B", " z", "Zed", "shoes "]) for i in range(1, r.randint(1, 6) + 1)}
        self.campaigns = {f"c_{i}": r.choice(["search", "social", "email", "Video"]) for i in range(1, r.randint(1, 5) + 1)}
        self.anchors = self.make_anchors()

    def make_anchors(self):
        r = self.r
        ws, we = self.ws, self.we
        pts = [ws, we - 1, we, ws - 1, we - 1000]
        for _ in range(r.randint(1, 6)):
            pts.append(r.randrange(ws, we))
        for p in list(pts):
            for d in (30 * MIN, 30 * MIN + 1, 30 * MIN - 1, 0, 1000, 7 * DAY, 7 * DAY + 1, -7 * DAY, -7 * DAY - 1, DAY, -DAY):
                pts.append(p + d)
        # midnights inside the window
        pts += [ws + k * DAY + d for k in range(0, (we - ws) // DAY + 1) for d in (-1, 0, 1)]
        return pts

    # ---- value generators ----
    def ts_value(self, ms, bad_rate=0.08):
        r = self.r
        if r.random() < bad_rate:
            return self.bad_ts(ms)
        if ms % 1000 or r.random() < 0.3:
            v = str(ms) if ms >= 0 else f1(ms)
            if ms >= 0 and r.random() < 0.15:
                v = "0" * r.randint(1, 6) + v
            return v
        if r.random() < 0.3:
            return f2(ms, r.choice([-600, -570, -60, 0, 60, 330, 345, 720, 23 * 60 + 59, -(23 * 60 + 59)]))
        if r.random() < 0.05:
            return f2(ms, 0).replace("+00:00", "-00:00")
        return f1(ms)

    def bad_ts(self, ms):
        r = self.r
        s = f1(ms - ms % 1000)
        opts = [
            "", s[:-1] + ".000Z", s[:-1] + "z", s.replace("T", "t"), s.replace("T", " "), s[:-1],
            f"{ms}.0", "1.79e12", "-5", s[:5] + "13" + s[7:], s[:5] + "02-30" + s[10:],
            "2027-02-29T00:00:00Z", s[:11] + "24" + s[13:], s[:17] + "60Z", s[:-1] + "+24:00",
            s[:-1] + "+05:60", s[:-1] + "+0200", "٢٥٠", "１" + s[1:], "9" * 25, s + "\r", " " + s,
            s[:-1] + "+1:00", "２０２６", str(ms) + " ", "+" + str(ms),
        ]
        return r.choice(opts)

    def price(self, purchase):
        r = self.r
        good = [str(r.randint(1, 50000)), "1", "10000000", "0250", "00000000001"]
        bad = ["", "0", "10000001", "19.99", "-5", "+5", "1e3", "２５０", "٢٥٠", " 100", "100 ", "1,0".replace(",", ""), "99999999999999999999"]
        if purchase:
            return r.choice(good) if r.random() < 0.8 else r.choice(bad)
        return r.choice(["", "", r.choice(good), r.choice(bad)])

    # ---- line writer with dialect noise ----
    def line(self, fields, quote_rate=0.2):
        r = self.r
        out = []
        for f in fields:
            if f is None:
                f = ""
            out.append(f'"{f}"' if r.random() < quote_rate else f)
        s = ",".join(out)
        x = r.random()
        if x < 0.03:
            s = s.rsplit(",", 1)[0]  # too few fields
        elif x < 0.05:
            s = s + ",x"
        elif x < 0.06:
            s = '"' + s
        elif x < 0.07:
            s = s + '"'
        elif x < 0.08:
            s = s.replace(",", ',"', 1)
        elif x < 0.09:
            s = ""  # empty line
        return s

    def cr(self, fields):
        """\r noise lives in the values (so the copy-consistency registry sees it)."""
        if self.r.random() < 0.04:
            fields = list(fields)
            fields[-1] = (fields[-1] or "") + "\r"
        return fields

    def write_parts(self, folder, header, lines):
        r = self.r
        os.makedirs(folder, exist_ok=True)
        nparts = r.randint(1, 4)
        parts = [[] for _ in range(nparts)]
        for ln in lines:
            parts[r.randrange(nparts)].append(ln)
        for i, p in enumerate(parts):
            body = header + "\n" + "".join(x + "\n" for x in p)
            if p and r.random() < 0.2:
                body = body[:-1]  # no trailing newline
            if not p and r.random() < 0.3:
                body = header  # header only, no newline
            with open(os.path.join(folder, f"part-{i:05d}.csv"), "w", encoding="utf-8", newline="") as f:
                f.write(body)
        if r.random() < 0.1:
            open(os.path.join(folder, "zz-empty.csv"), "w").close()  # zero-byte file
        if r.random() < 0.1:
            with open(os.path.join(folder, "notes.txt"), "w") as f:  # not *.csv: ignored
                f.write("garbage,line\n")

    def write(self, d):
        r = self.r
        with open(os.path.join(d, "manifest.json"), "w") as f:
            f.write(f'{{"window_start": "{f1(self.ws)}", "window_end": "{f1(self.we)}"}}\n')
        with open(os.path.join(d, "products.csv"), "w", newline="") as f:
            f.write("product_id,category\n" + "".join(f"{k},{v}\n" for k, v in self.products.items()))
        with open(os.path.join(d, "campaigns.csv"), "w", newline="") as f:
            f.write("campaign_id,channel\n" + "".join(f"{k},{v}\n" for k, v in self.campaigns.items()))

        # links
        lk = []
        ids = self.users + [f"x{i}" for i in range(3)]
        for _ in range(r.randint(0, 10)):
            a, b = r.choice(ids), r.choice(ids)
            c = r.random()
            if c < 0.08:
                b = ""
            elif c < 0.12:
                a = a
                b = a  # self link
            elif c < 0.15:
                a, b = "user_id_a", r.choice(ids)
            lk.append(self.line([a, b]))
        if r.random() < 0.3:  # a chain
            chain = r.sample(ids, k=min(len(ids), r.randint(2, 6)))
            for a, b in zip(chain, chain[1:]):
                lk.append(self.line([a, b], 0.1))
        self.write_parts(os.path.join(d, "links"), "user_id_a,user_id_b", lk)

        # events
        ev = []
        reg = {}

        def emit(fields, q=0.2):
            fields = self.cr(fields)
            key = (fields[0], fields[3])
            fields = reg.setdefault(key, fields)  # same (event_id, ingest_ts) => identical copy
            ev.append(self.line(fields, q))

        eids = [f"e{i}" for i in range(r.randint(1, 40))] + ["E1", "e1 ", " e1", "e1\r"]
        prods = list(self.products) + ["p99", "P1", " p1", ""]
        for _ in range(r.randint(0, 60)):
            eid = r.choice(eids) if r.random() > 0.03 else ""
            ts = r.choice(self.anchors) + r.choice([0, 0, 0, 1, -1, 1000, r.randint(0, 3600_000)])
            user = r.choice(self.users) if r.random() > 0.05 else ""
            typ = r.choice(["view"] * 4 + ["cart"] * 2 + ["purchase"] * 4 + ["Purchase", "cart ", "VIEW", "", "buy"])
            ing = ts + r.choice([0, 1000, 60_000, DAY, DAY + 1000, DAY - 1000, -5000, 3 * DAY, r.randint(0, 600_000)])
            ing -= ing % 1000  # F1 has whole seconds
            prod = r.choice(prods)
            fields = [eid, user, self.ts_value(ts), f1(ing), typ, prod, self.price(typ == "purchase")]
            emit(fields)
            c = r.random()
            if c < 0.25:  # later copies, maybe changed
                for _ in range(r.randint(1, 3)):
                    g = list(fields)
                    g[3] = f1(ing + r.choice([1000, 600_000, 0 if False else 1000]))
                    if r.random() < 0.3:
                        g[1] = r.choice(self.users)
                    if r.random() < 0.3:
                        g[2] = self.ts_value(ts + r.randint(-3600_000, 3600_000))
                    emit(g)
            elif c < 0.35:  # identical copy (same ingest) with different quoting
                emit(fields, 0.6)
            elif c < 0.40:  # an EARLIER copy with a bad ts (dedup-before-validate)
                g = list(fields)
                g[2] = self.bad_ts(ts)
                g[3] = f1(ing - 1000)
                emit(g)
        # A header-like data row has ingest_ts "ingest_ts", which breaks the rules' promise
        # that ingest_ts is always valid F1. STRICT_SPEC=1 keeps every case inside the promises.
        if r.random() < 0.1 and not os.environ.get("STRICT_SPEC"):
            ev.append("event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents")
        self.write_parts(os.path.join(d, "events"), "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents", ev)

        # clicks
        cl = []
        creg = {}
        cids = [f"k{i}" for i in range(r.randint(1, 25))] + ["K1", "k1 "]
        camps = list(self.campaigns) + ["c_999", "C_1", " c_1", ""]
        for _ in range(r.randint(0, 40)):
            cid = r.choice(cids) if r.random() > 0.03 else ""
            ts = r.choice(self.anchors) + r.choice([0, 0, -1, 1, -7 * DAY, -7 * DAY - 1, -r.randint(0, 7 * DAY)])
            fields = self.cr([cid, r.choice(self.users) if r.random() > 0.05 else "", r.choice(camps), self.ts_value(ts, 0.05)])
            fields = creg.setdefault(cid, fields)  # click copies are identical
            cl.append(self.line(fields))
            if r.random() < 0.15:
                cl.append(self.line(fields, 0.7))  # identical copy
        self.write_parts(os.path.join(d, "clicks"), "click_id,user_id,campaign_id,ts", cl)


def same_cids_identical(d):
    """Enforce the rules' promise that click copies are identical: drop cases that break it."""
    seen = {}
    for p in reference.csv_files(os.path.join(d, "clicks")):
        for row in reference.read_rows(p, 4):
            if row[0] is None:
                continue
            if seen.setdefault(row[0], tuple(row)) != tuple(row):
                return False
    seen = {}
    for p in reference.csv_files(os.path.join(d, "events")):
        for row in reference.read_rows(p, 7):
            if row[0] is None:
                continue
            k = (row[0], row[3])
            if seen.setdefault(k, tuple(row)) != tuple(row):
                return False
    return True


def run_case(binary, d, out_rust, out_ref):
    p = subprocess.run([binary], env=dict(os.environ, DATA_DIR=d, OUTPUT_DIR=out_rust, NCPU="2"),
                       capture_output=True, text=True)
    if p.returncode != 0:
        return f"rust exit {p.returncode}: {p.stderr[-800:]}"
    reference.run(d, out_ref)
    for table, schema in SCHEMAS.items():
        a = rows(load_actual(os.path.join(out_rust, table), schema))
        b = rows(load_actual(os.path.join(out_ref, table), schema))
        if a != b:
            sa, sb = set(a), set(b)
            return f"{table} differs\n  rust only {sorted(sa - sb, key=str)[:5]}\n  ref only  {sorted(sb - sa, key=str)[:5]}"
    return None


def main():
    binary = os.path.abspath(sys.argv[1])
    n = int(sys.argv[2]) if len(sys.argv) > 2 else 500
    seed = int(sys.argv[3]) if len(sys.argv) > 3 else 1
    fail_dir = os.path.join(os.path.dirname(__file__), "..", "fuzz-failures")
    failures = skipped = nonempty = 0
    for i in range(n):
        r = random.Random(seed * 1_000_003 + i)
        with tempfile.TemporaryDirectory() as t:
            d = os.path.join(t, "data")
            os.makedirs(d)
            Case(r).write(d)
            if not same_cids_identical(d):
                skipped += 1
                continue
            err = run_case(binary, d, os.path.join(t, "rust"), os.path.join(t, "ref"))
            if err is None:
                ud = pl.read_parquet(os.path.join(t, "ref", "user_daily", "part-0.parquet"))
                nonempty += ud.height > 0
            else:
                failures += 1
                keep = os.path.join(fail_dir, f"case-{seed}-{i}")
                shutil.rmtree(keep, ignore_errors=True)
                shutil.copytree(d, keep)
                print(f"FAIL case {seed}-{i}: {err}\n  kept in {keep}")
                if failures >= 5:
                    break
    ran = n - skipped
    print(f"fuzz: {ran} cases run ({nonempty} with sessions), {skipped} skipped (broke a data promise), {failures} failures")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
