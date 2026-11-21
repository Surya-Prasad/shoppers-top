# A/B `7ebabca` (candidate) vs `d7c42a7` (base)

- Runner: Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz x 4
- **Correctness gate: PASS**

| Data | Base median (min–max) | Candidate median (min–max) | Δ median | Candidate faster in | Base / cand max RSS |
|---|---:|---:|---:|---:|---:|
| d20 | 3.163 (3.150–3.184) | 3.163 (3.152–3.196) | **+0.0%** | 3/7 | 3.34 / 3.34 GB |
| d60 | 10.014 (9.715–10.043) | 10.020 (9.778–10.234) | **+0.1%** | 2/5 | 9.96 / 9.95 GB |

Candidate phase log (last 60M run):
```
[  0.012s] reference tables
    links: parse                         0.040s
    links: intern                        0.262s
    links: union                         0.067s
    links: names                         0.098s
    links: out map                       0.076s
[  0.566s] links resolved
    clicks: scan+parse+partition         0.864s
    clicks: transpose                    0.001s
    clicks: dedup+validate+repartition   0.178s
    clicks: transpose                    0.001s
[  1.610s] clicks
    events: scan+parse+partition         5.158s
    events: transpose                    0.005s
    events: dedup+validate+repartition   1.073s
    events: transpose                    0.001s
[  7.847s] events
[  9.329s] sessions
[  9.717s] outputs written
[  9.717s] done

x86 single-thread scan layers (candidate, SS_EXP=1, d20):
lines only (memchr)           0.232s  (20872908 lines, 1.60 GB)
records (RW16 production)     1.205s    57.7 ns/line   1.33 GB/s  (acc 186232865)
records (RW4 struct index)    2.057s    98.5 ns/line  (acc 186232865)
records (night-04, direct)    1.585s    75.9 ns/line  (acc 186232865)
records (RW9 memchr+SWAR)     2.883s   138.1 ns/line  (acc 186232865)
records (RW10 two-stage)      1.286s    61.6 ns/line  (acc 186232865)
+ key_of(eid)                 1.320s    63.2 ns/line   1.22 GB/s  (acc 13129365304273469409)
+ key_of(user)                1.430s    68.5 ns/line   1.12 GB/s  (acc 6542972264725638445)
+ ts parse_in                 1.748s    83.7 ns/line   0.92 GB/s  (acc 2011881441597699725)
+ ingest parse                1.934s    92.7 ns/line   0.83 GB/s  (acc 15894505099581498581)
+ type + price                2.094s   100.3 ns/line   0.77 GB/s  (acc 15894505099805131216)
+ person lookup               4.931s   236.2 ns/line   0.33 GB/s  (acc 9938545247810663594)
+ person + scatter            5.872s   281.3 ns/line  (kept 20864254)
+ product lookup              2.798s   134.0 ns/line   0.57 GB/s  (acc 15894505099806148549)
```

Decision rule (OPTIMIZATION_PLAN.md): keep only if the gate passes, 20M improves, and 60M does not regress (the candidate is faster in most pairs and the median isn't worse by more than ~1%).
