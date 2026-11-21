# A/B `88c96a2` (candidate) vs `f8c8487` (base)

- Runner: INTEL(R) XEON(R) PLATINUM 8573C x 4
- **Correctness gate: PASS**

| Data | Base median (min–max) | Candidate median (min–max) | Δ median | Candidate faster in | Base / cand max RSS |
|---|---:|---:|---:|---:|---:|
| d20 | 3.394 (3.361–3.477) | 3.267 (3.244–3.338) | **-3.7%** | 7/7 | 3.34 / 3.35 GB |
| d60 | 10.542 (10.526–10.725) | 10.182 (10.137–10.369) | **-3.4%** | 5/5 | 9.95 / 9.97 GB |

Candidate phase log (last 60M run):
```
[  0.007s] reference tables
    links: parse                         0.041s
    links: intern                        0.216s
    links: union                         0.054s
    links: names                         0.123s
    links: out map                       0.065s
[  0.516s] links resolved
    clicks: scan+parse+partition         0.852s
    clicks: transpose                    0.001s
    clicks: dedup+validate+repartition   0.238s
    clicks: transpose                    0.001s
[  1.609s] clicks
    events: scan+parse+partition         5.405s
    events: transpose                    0.007s
    events: dedup+validate+repartition   1.397s
    events: transpose                    0.001s
[  8.420s] events
[  9.623s] sessions
[  9.932s] outputs written
[  9.932s] done

x86 single-thread scan layers (candidate, SS_EXP=1, d20):
lines only (memchr)           0.274s  (20872908 lines, 1.60 GB)
records (fused scanner)       2.183s   104.6 ns/line   0.73 GB/s  (acc 186232865)
+ key_of(eid)                 2.329s   111.6 ns/line   0.69 GB/s  (acc 13129365304273469409)
+ key_of(user)                2.449s   117.3 ns/line   0.66 GB/s  (acc 6542972264725638445)
+ ts parse_in                 2.806s   134.4 ns/line   0.57 GB/s  (acc 2011881441597699725)
+ ingest parse                3.002s   143.8 ns/line   0.53 GB/s  (acc 15894505099581498581)
+ type + price                3.070s   147.1 ns/line   0.52 GB/s  (acc 15894505099805131216)
+ person lookup               4.853s   232.5 ns/line   0.33 GB/s  (acc 9938545247810663594)
+ person + scatter            4.975s   238.3 ns/line  (kept 20864254)
+ product lookup              3.372s   161.6 ns/line   0.48 GB/s  (acc 15894505099806148549)
```

Decision rule (OPTIMIZATION_PLAN.md): keep only if the gate passes, 20M improves, and 60M does not regress (the candidate is faster in most pairs and the median isn't worse by more than ~1%).
