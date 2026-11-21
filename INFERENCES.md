# Inference & data-point log

A running record of every judgement call made while building the prototype, and every
measurement taken, so you can audit them later. Date: 2026-11-20.

Legend: **[RULE]** stated in rules.md · **[INF]** my inference where the rules are silent ·
**[DATA]** measured · **[DECISION]** engineering choice.

---

## A. Sources and precedence

- [RULE] `rules.md` (downloaded to `docs/rules.md`) wins over the starters and the brief.
- [DATA] Downloads, all HTTP 200: rules.md (17 KB), the DuckDB/Polars/PySpark starters,
  dev-answer.zip, generator.zip. The fixture sets in the three starters are byte-identical
  (`diff -r`).
- [INF] The brief says to "follow rules.md for every disputed detail". The only places the
  brief is looser than rules.md are wording; no semantic conflicts were found.

## B. Semantic inferences (where rules.md is silent or only implied)

| # | Situation | Choice | Why / risk |
| --- | --- | --- | --- |
| B1 | Which line is the header | **Only the first line of each file** is skipped. A later line equal to the header is data. | [RULE] confirmed by fixture L3, where `user_id_a,user_id_b` as data joins those two IDs. |
| B2 | Header order differs from the documented order | If the header is a permutation of the expected names, columns are mapped by name; otherwise the documented order is assumed. | [INF] The generator always writes the documented order, so this is defensive only. Caveat: an equal-`ingest_ts` dedup tie between files with *different* header orders breaks ties on file-order fields. Irrelevant, since such copies are promised identical. |
| B3 | Zero-byte CSV file (no header at all) | Treated as no rows. | [INF] rules.md says every file has a header. Handled without crashing (Linux cannot mmap zero bytes). |
| B4 | Last line without `\n` | It is a line. | [INF] Standard. A trailing `\n` does not create an extra empty (malformed) line. |
| B5 | `*.csv` matching | Regular files directly in the folder whose names end in `.csv`; names starting with `.` are skipped (shell-glob semantics); not recursive; `.CSV` not matched. | [INF] |
| B6 | `ingest_ts` unparseable | Ranks **after** every parseable copy in dedup. If kept, the event is dropped (lateness cannot be checked). | [INF] rules.md promises valid F1, so this never triggers on real data. Python oracle and Rust agree. |
| B7 | `ingest_ts` F3 too large for i64 | Saturates to i64::MAX (still "a number"), and so is dropped as late. | [INF] Ultra edge case; the oracle uses Python big ints, so an exact tie at i64::MAX could order differently. Unreachable with valid F1. |
| B8 | Equal `ingest_ts` among copies | Promised identical; tie broken by the raw field tuple, for determinism only. | [RULE] + [DECISION] |
| B9 | Click copies differ (against the promise) | Keep the copy with the smallest (user, campaign, ts) tuple. | [INF] Deterministic. Note that with identical copies, click dedup cannot change any output (same ts, campaign and id). |
| B10 | `products.csv` / `campaigns.csv` repeat a key | First occurrence wins. | [INF] They are promised clean and unique. |
| B11 | A category literally named `unknown` in products.csv | Merges with the `unknown` default (compared as strings). | [INF] rules.md says no category uses that name. |
| B12 | Empty category or channel value | Kept as the empty string. | [INF] Promised clean. |
| B13 | Non-UTF-8 bytes in an ID | All comparisons are on raw bytes. Output strings use lossy UTF-8 (Parquet strings must be UTF-8). | [INF] Input is promised UTF-8, and `,` `"` `\n` are ASCII, so no character is ever split. |
| B14 | F3 with more than 19 significant digits | Bad (outside every window). | [RULE] "too long for a 64-bit integer is ... outside the window." |
| B15 | F1/F2 year range | Any four ASCII digits, 0000–9999, proleptic Gregorian. | [INF] |
| B16 | Manifest timestamps | Parsed with the same F1/F2/F3 parser; the program fails loudly if unparseable. | [DECISION] Better a visible crash than silently wrong windows. |
| B17 | Late check when `ingest_ts < ts` | Kept (difference is negative). | [RULE] |
| B18 | Session gap | Compared between consecutive events in `ts` order, so equal timestamps always share a session. | [RULE] fixture E16 |
| B19 | `top_category` with no purchases | null. A purchase always has price ≥ 1, so null ⇔ revenue 0. | [RULE] + [INF] used as an invariant check. |
| B20 | `day` | `floor(start_ms / 86 400 000)` as a Parquet DATE (int32 days since epoch). | [RULE] |
| B21 | Output integer width | Int64 for all counters (any width is accepted). | [DECISION] |
| B22 | Empty result tables | One zero-row Parquet file with the full schema is still written. | [INF] The checker errors with "no parquet" if a folder is empty. |

## C. Engineering decisions

- [DECISION] **Engine: Rust** (your choice). A hand-written dialect parser was required anyway:
  the starter README warns that the DuckDB and Polars CSV readers disagree with the rules.
- [DECISION] **No `target-cpu` flag.** `x86-64-v3` broke the image build under local amd64
  emulation (it also applies to build scripts), and the gain is small because `memchr`
  already picks SIMD at run time. A portable binary is also safer on the scorer.
- [DECISION] **Replaced `ahash` with an in-house hasher** (`src/hashing.rs`). On this toolchain,
  `ahash::RandomState::hash_one` returned `0` for every key. That turned every hash map into a
  linear scan: the 1M dev run took **51.4 s**, and **0.35 s** after the fix, with identical
  output. The cause was found by sampling the process: all time was in `memcmp` inside
  hash-map probes. It is now covered by a unit test (`hashing::tests::spreads`).
- [DECISION] **`panic = "unwind"`, not `"abort"`.** Found in container testing: the binary runs
  as **PID 1**, and PID 1 does not get the default action for `SIGABRT`. When an output-dir
  permission error panicked, the container **hung at 100% CPU for over 11 minutes** instead of
  exiting. With unwind, the same failure exits with code 101 in 0.4 s (verified with a
  read-only `/output`). On the scorer this means a failure costs seconds, not the full time limit.
- [DATA] Local Docker caveat: colima shares only `$HOME` with its VM, so a harness temp dir under
  `/var/folders` reaches the container as a root-owned empty folder. Run the harness with
  `TMPDIR` under the project. This is a local-only issue; the scorer mounts writable dirs.
- [DECISION] 512 hash partitions and 4 MB scan chunks; not tuned yet.
- [DECISION] Atomic writes: write to `.name.tmp`, then rename.
- [DECISION] Docker: `rust:1.95-slim-bookworm` builder, `debian:bookworm-slim` runtime,
  final image **29.6 MB**, uid 1000, `CMD ["/app/shopping-sessions"]`.

## D. Data points

### D1. Dev set (seed 1, 1M events, 250k clicks): window 2027-12-09 → 2027-12-23

| Stage | Count |
| --- | --- |
| event lines (after headers) | 1,072,086 |
| malformed event lines | 451 |
| empty event_id | 81 |
| duplicate copies removed | 71,953 (6.7% of lines) |
| kept copy invalid | 9,555: bad_user 1,966 · bad_ts 4,603 · bad_type 960 · bad_product 514 · bad_price 1,512 |
| late (> 24 h) | 12,116 |
| unknown product (kept, category `unknown`) | 4,410 |
| **events surviving** | **977,930** |
| click lines | 255,240 → malformed 143 · empty id 28 · dup copies 5,219 · bad_user 476 · bad_ts 984 · bad_campaign 1,202 → **247,188 valid** |
| link lines | 44,874 → malformed 29 · empty side 187 → 44,658 kept |
| distinct linked IDs | 60,395 |
| people with ≥ 2 IDs | 19,086 |
| largest person (IDs) | 280 |
| persons with sessions | 33,396 |
| sessions | 179,366, of which 109,923 (61.3%) credited to a campaign (the rules say "about 60%") |
| assist pairs (session × campaign) | 1,932,099 |
| output rows | user_daily 117,038 · campaign_daily 27,400 (matches dev-answer exactly) |

### D2. 60M-event run (seed 11, 15M clicks), 5.3 GB of CSV

| Stage | Count |
| --- | --- |
| event lines | 62,619,519 |
| duplicate copies removed | 2,612,499 |
| kept copy invalid | 628,829 |
| late | 564,149 |
| **events surviving** | **58,782,475** |
| valid clicks | 14,813,710 |
| link lines kept | 2,660,545 |
| distinct linked IDs | 3,594,452 |
| largest person | **4,349 IDs** (the shared-device component) |
| persons with sessions | 2,002,394 |
| sessions | 10,023,673 (61.7% credited) |
| assist pairs | 103,808,215 |
| output rows | user_daily 6,633,859 · campaign_daily 28,014 |

### D3. Timings (Apple-silicon laptop, native arm64, warm page cache, 8 threads)

| Input | Wall | Peak RSS | Phase split |
| --- | --- | --- | --- |
| 1M | 0.35 s | — | links 0.01 · clicks 0.02 · events 0.08 · sessions 0.02 · write 0.05 |
| 20M | 2.05 s | 5.4 GB | links 0.14 · clicks 0.23 · events 1.12 · sessions 0.32 · write 0.22 |
| 60M | 6.6–8.8 s | 15.6 GB | links 0.6 · clicks 0.8–1.2 · events 3.5–5.2 · sessions 1.1 · write 0.5 |
| 60M, 1 thread | 27.1 s | — | (determinism check) |

- [INF] The scorer (8 hyperthreads of a Xeon, with a cold read of 5+ GB) will be slower than
  this laptop. A 2–3× slowdown would still be roughly **15–25 s** for the final, against a
  180 s cap and the recommended ~60 s target. Baseline: 171.9 s. **Not yet verified on the scorer.**
- [DATA] Python oracle: 11 s for 1M events (single-threaded), so differential testing tops out
  at about 2M events per case.

### D4. Test results

| Suite | Result |
| --- | --- |
| Unit tests (`cargo test`) | 4 passed |
| Official fixtures, native | 36/36 |
| Official fixtures, Python oracle | 36/36 |
| Dev answer, Rust | MATCH |
| Dev answer, Python oracle | MATCH |
| Generator seeds 2, 3, 4, 5, 6, 42 (1M) vs oracle | 6/6 MATCH |
| Seed 15 (2M, window 2028-02-17 → 03-02, contains Feb 29) vs oracle | MATCH; 16,644 user rows on 2028-02-29 |
| Fuzz, 400 cases (seed 1) + 5,000 cases (seed 7) | 0 differences; 4,249 of the 5,000 produced sessions |
| Mutation testing | 10/10 mutants caught by fuzz. Fixtures alone miss 3: `inner_quote`, `f2_hour` (+24:00 accepted), `price_max` (10,000,000 rejected) |
| Integrity checks at 60M | 15/15 |
| 1 vs 8 threads at 60M | identical output |
| Extra fuzz: 1,500 cases (seed 99) + 300 cases (seed 123, after the panic change) | 0 differences |
| **amd64 image with scorer flags** (`--read-only`, `--network none`, uid 1000, cap-drop, tmpfs /tmp) | 36/36 fixtures; dev set MATCH in 0.79 s (emulated); schema has `day: date32`, exact columns; no stray files |
| Forced failure in container (read-only /output) | exits 101 in 0.4 s |

Raw counters for the dev run: [docs/data/dev-stats.json](docs/data/dev-stats.json).

## E. Open risks / next steps

1. Measure the amd64 image on real x86 hardware. Local amd64 runs are emulated and only
   prove correctness, not speed.
2. Memory: peak 15.6 GB at 60M is fine for 24 GB, but `EvRaw` (80 B/row) could shrink to
   about 56 B by dropping the stored line slice. Do this if the hidden data is larger.
3. Cold I/O on the scorer is unmeasured; `madvise(WILLNEED)` prefetch is a cheap option.
4. Submissions are rate-limited (2/hour): run `make fixtures`, `make check-dev` and
   `tests/fuzz.py` before every submit.

## F. Head-to-head: Vishi99/builders2026 @ 8a74d83 (2026-11-20)

Their engine: C++ (packed integer IDs, AVX2/NEON CSV scan, pread chunks) called from Python;
PyArrow writes Parquet. Built and run **only inside Docker** (no network, read-only root,
caps dropped, uid 1000). Both images built natively for linux/arm64 on this Mac; colima VM
with 8 CPUs / 18 GB; data on a **VM-local volume** (colima's Mac file sharing penalizes mmap,
which made ours look ~2× slower at first; the scorer mounts local disk).

| Check | Theirs | Ours |
| --- | --- | --- |
| 36 fixtures (container, scorer flags) | 36/36 | 36/36 |
| 1M dev answer | MATCH | MATCH |
| Fuzz, 800 cases, strict-spec | 0 failures | 0 failures |
| Fuzz, 800 cases, incl. header-like *event* row | **crashes** (5 of first 84): `Invalid ingest_ts on a parsed event` | passes |
| 20M and 60M outputs, theirs vs ours | identical (60M: 6,633,859 + 28,014 rows) | — |
| Image size (arm64) | 521 MB | 141 MB |

End-to-end wall time, container start → exit (lower is better):

| Input | Theirs cold / warm | Ours cold / warm |
| --- | --- | --- |
| 1M (Mac file share) | ~0.3 s | ~0.33 s |
| 20M | 3.48 s / 2.06–2.23 s | 4.62 s / 2.61–2.74 s |
| **60M** | **7.64 s / 4.27–4.99 s** | **17.43 s / 11.08–11.77 s** |
| 60M peak memory (cgroup) | **3.3 GB** | **8.3 GB** |

- [DATA] At 60M theirs is **~2.3–2.6× faster** and uses **~2.5× less memory**. Ours is still
  ~10× under the 180 s cap, but on the leaderboard theirs would rank well ahead.
- [INF] Where our time goes (60M warm): events 6.1 s vs their parse+dedup 2.0 s. Causes: 80-byte
  `EvRaw` records holding 16-byte slices (vs their 40-byte copies with 8-byte packed IDs),
  string hashing vs integer hashing, and faulting in 2.5× more memory in a VM.
- [INF] Their crash input (a data row equal to the header in events/) is **outside the rules'
  promise** that `ingest_ts` is always valid F1, and the generator never emits it. Low risk in
  practice, but an unrecoverable failure mode where ours degrades gracefully.
- [DECISION] `tests/fuzz.py` gained `STRICT_SPEC=1`, which keeps every case inside the data
  promises (used to judge them fairly). The default mode still includes the out-of-spec row.
- [DECISION] **Benchmark only, no porting.** Their engine is a yardstick for what is achievable
  (≈4.3 s warm at 60M, 3.3 GB), not a source of techniques. Our improvements come from our own
  profiling and the design principles in docs/DESIGN.md.

## G. Official score

- [DATA] Submission **BB-DB9B9054** (`shoppers-top` @ 34a18e7), Fri 2026-11-20 11:21 PM:
  **5.79 s** with a 362 ms wait. It is our best score and is entered in the final.
  Baseline: 171.91 s (about 30× faster).
- [INF] Scorer 5.79 s vs our local container 2.6 s (20M warm) / 4.6 s (20M cold) suggests the
  practice set was run cold, roughly matching the local cold numbers.
