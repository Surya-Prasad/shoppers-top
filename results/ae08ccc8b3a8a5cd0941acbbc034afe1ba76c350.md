# A/B `ae08ccc` (candidate) vs `02c37c7` (base)

- Runner: INTEL(R) XEON(R) PLATINUM 8573C x 4
- **Correctness gate: PASS**

| Data | Base median (min–max) | Candidate median (min–max) | Δ median | Candidate faster in | Base / cand max RSS |
|---|---:|---:|---:|---:|---:|
| d20 | 3.686 (3.666–3.778) | 3.287 (3.264–3.356) | **-10.8%** | 7/7 | 3.45 / 3.44 GB |
| d60 | 11.512 (11.465–11.540) | 10.399 (10.138–10.544) | **-9.7%** | 5/5 | 10.16 / 10.17 GB |

Candidate phase log (last 60M run):
```
[  0.009s] reference tables
    links: parse                         0.030s
    links: intern                        0.246s
    links: union                         0.052s
    links: names                         0.168s
    links: out map                       0.097s
[  0.603s] links resolved
    clicks: scan+parse+partition         0.872s
    clicks: transpose                    0.001s
    clicks: dedup+validate+repartition   0.272s
    clicks: transpose                    0.002s
[  1.748s] clicks
    events: scan+parse+partition         4.990s
    events: transpose                    0.005s
    events: dedup+validate+repartition   1.683s
    events: transpose                    0.001s
[  8.428s] events
[  9.918s] sessions
[ 10.307s] outputs written
[ 10.307s] done

x86 single-thread scan layers (candidate, SS_EXP=1, d20):
lines only (memchr)           0.292s  (20872908 lines, 1.60 GB)
records (fused scanner)       1.708s    81.8 ns/line   0.94 GB/s  (acc 186232865)
records (RW4 struct index)    2.335s   111.9 ns/line  (acc 186232865)
records (night-04, direct)    1.538s    73.7 ns/line  (acc 186232865)
records (RW9 memchr+SWAR)     2.977s   142.6 ns/line  (acc 186232865)
records (RW10 two-stage)      1.308s    62.7 ns/line  (acc 186232865)
+ key_of(eid)                 1.994s    95.5 ns/line   0.80 GB/s  (acc 13129365304273469409)
+ key_of(user)                2.158s   103.4 ns/line   0.74 GB/s  (acc 6542972264725638445)
+ ts parse_in                 2.553s   122.3 ns/line   0.63 GB/s  (acc 2011881441597699725)
+ ingest parse                2.793s   133.8 ns/line   0.57 GB/s  (acc 15894505099581498581)
+ type + price                2.917s   139.8 ns/line   0.55 GB/s  (acc 15894505099805131216)
+ person lookup               5.853s   280.4 ns/line   0.27 GB/s  (acc 9938545247810663594)
+ person + scatter            5.771s   276.5 ns/line  (kept 20864254)
+ product lookup              3.324s   159.3 ns/line   0.48 GB/s  (acc 15894505099806148549)
```

Decision rule (OPTIMIZATION_PLAN.md): keep only if the gate passes, 20M improves, and 60M does not regress (the candidate is faster in most pairs and the median isn't worse by more than ~1%).
