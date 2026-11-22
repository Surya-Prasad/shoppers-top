# v2 status (branch `opt/v2-fast`)

Baseline for every comparison: `b9481b11a506d099ebe47963c927c8fe7bbba988` (1.92 s scored). `benchmarks/BEST` points at it.

Gates run locally for every commit below: `cargo test --release`, 36 fixtures, dev answer MATCH, `tests/fuzz.py`
strict + default (0 failures, 150–500 cases each), Parquet MATCH vs b9481b1 on 20M seed 11, 20M seed 5 and
60M seed 11; plus the CI gate (unit tests, fixtures, dev, 20M/60M equal to the base) on every CI A/B.

Caveat: one confirmation run of `a02705c` vs b9481b1 (Xeon 6973P, `ab/v2-final1`) came out +9.3% / +4.8%, but that
runner was I/O-bound (base 8.3 s at 20M instead of ~2 s; 60M events scan 20.8 s while dedup/sessions ran at normal
speed: input not in page cache). The scorer's page cache is warm, so this is not representative; two more confirmation
runs (`ab/v2-final2`, `ab/v2-final3`, EPYC 9V74) were I/O-bound the same way (base ~6 s at 20M, ~22 s at 60M) and gave
+0.8% (3/7) / +2.9% (0/5) and +2.3% (1/7) / +2.0% (0/5). So: on warm page cache (all earlier runs, and the scorer per
public probes) a02705c is 14–18% faster; when the input must be read from disk, it is ~1–9% slower than b9481b1
(the parse savings vanish behind I/O). Since 02:00 UTC every CI runner has behaved this way, even with the 20M input on tmpfs (base 6.4 s on EPYC 9V74), so
it is not disk I/O: the scan, which touches the mapped input, is ~3x slower while later phases are normal (suspect:
expensive file-mapping faults on these VMs; profiling). The harness now keeps one dataset at a time and puts 20M on
/dev/shm.

CI runners vary a lot (EPYC 7763 / 9V45 / 9V74, Xeon 8370C / 6975P / 8573C), so only same-run pairs are compared.

## Re-evaluation (checked 2026-11-22 ~04:30 UTC)

- `docs/rules.md` is updated to the official text: the only change is the generator paragraph (1 in 8 distinct
  user/event/click IDs has a 32-character hex suffix; IDs pass a per-seed keyed bijection; treat IDs as opaque
  variable-width strings). Event/click/session semantics are unchanged.
- Scorer version `shopping-sessions-5`; practice runs are timed once (3 times, median, when under 20 s).
- The leaderboard was re-scored on the new data: #1 "67" 0.5469 s, #2 ND 0.5528 s, #10 azing 0.6919 s, #50 1.0261 s
  (it was 0.28 s at the top on the old data). Baseline 167.6 s.
- Our estimate: CI 4-vCPU time × ~0.75 (the old CI→scorer ratio) ≈ **1.6–1.8 s** for the current head (CI 20M
  ≈ 2.35 s on EPYC 9V74); b9481b1 on the new data is ~6.4–8.3 s on CI (≈ 5–6 s on the scorer, if the ratio holds).

## The generator changed (found 2026-11-22 ~04:00 UTC)

The official generator.zip and dev-answer.zip were replaced. New version: user, event and click IDs come from a keyed
Feistel bijection and **1 in 8 IDs is the prefix plus 32 lowercase hex digits** (33 bytes) instead of base 36.
Those IDs do not pack into u64 keys, so every engine built on that assumption (b9481b1 and all v2 commits up to
`d7cc29e`) sends them through one global `Mutex` interner, and the v2 fast decoder declined every line with a long ID.
That, not the CI runners, is why every A/B since ~02:00 UTC ran ~3x slower (profile: `keys::intern` 11% plus ~20%
mutex/futex). Local, new 20M seed 11: b9481b1 **~11 s**, a02705c ~11 s, `3b0bd10` (sharded arena interner, long IDs in
the fast decoder, no THP advice) **~4.2 s**. All local gates re-run on regenerated data (new dev answer MATCH).

## Steps

| Step | Commit | What | CI A/B (20M / 60M), same runner |
|---|---|---|---|
| P1 | `0e1b744` | shape-specialized events/clicks decoder, exact fallback | vs b9481b1: **−14.9% (7/7) / −12.1% (5/5)** (EPYC 7763) |
| P1b | `aead356` | monomorphic events scan loop | vs b9481b1: **−15.7% (6/7) / −17.8% (5/5)** (EPYC 7763); Docker check PASS |
| links-first | `7df6987` | resolve links before the scans | vs aead356: +1.3% (2/7) / +0.9% (1/5) (Xeon 8370C) — neutral |
| P3 | `6382217` | dense-index sessions sort | vs 7df6987: **+3.0% / +2.0%** (EPYC 9V45) — regression, **reverted** in `537f83e` |
| P2a | `ac6caa2` | person lookup in the scan's prefetch batch | vs 6382217: −0.8% (4/7) / −1.5% (3/5) (EPYC 9V45) — neutral |
| P5a | `571ea2e` | unmap inputs in background, exit without destructors | vs ac6caa2: −0.1% (5/7) / **−3.7% (5/5)** (Xeon 6975P) |
| P2b | `70d5eef` | no click dedup (copies identical by rules.md) | vs 571ea2e: −1.6% (6/7) / −0.7% (3/5) (EPYC 9V74) — keep |
| revert P3 | `537f83e` | back to RW6 sessions sort | vs 70d5eef: **−3.7% (7/7) / −3.6% (3/5)** (EPYC 9V74) |
| P1c | `9b850aa` | dense product index, same-hour ingest shortcut | vs 537f83e: −0.3% (3/7) / **−5.2% (5/5)** (EPYC 7763) |
| ClScan | `80c1ff3` | monomorphic clicks loop (no measured change locally) | vs b9481b1: **−15.9% (7/7) / −15.7% (5/5)** (EPYC 7763); Docker PASS |
| NB 512 | `466f6a7` (exp/v2-nb512) | 512 hash partitions | vs 80c1ff3: **+5.2% (0/7)** / +0.5% (Xeon 6975P) — dropped |
| P2c | `5306bd6` | bit filter of linked users (skip probes for unlinked) | vs 80c1ff3: −1.2% (6/7) / **+4.0% (0/5)** (Xeon 6973P) — reverted |
| v3 THP again | `b9d02ae` (= `971ee32` on c48f601) | MADV_HUGEPAGE on Parted mappings again | `971ee32` vs c48f601: **−7.1% (7/7)** / +2.8% (1/5) (Xeon 6975P; 60M on disk near the 15 GB runner limit, RSS 12.25 → 13.54 GB; 20M RSS 4.20 → 5.15 GB) — confirm on Xeon 8573C: **−6.4% (7/7)** / +4.3% (1/5); Docker PASS; `b9d02ae` vs b9481b1 **−63.6% (7/7) / −63.2% (5/5)** (EPYC 7763) — kept for the scorer (warm cache, 20M-like) |
| v3 EvRec hash | (cherry-pick of `22b466b`) | EvRec carries its dedup hash key (computed at scan) | `22b466b` vs 89c8210: **−4.4% (7/7) / −8.5% (5/5)** (EPYC 9V74) — keep; Docker PASS (`c48f601`) |
| v3 hex LUT | `89c8210` | branch-free hex check in the interner | vs 2bd0fe0: **−2.8% (5/7) / −4.1% (4/5)** (EPYC 7763) — keep; vs b9481b1 −66.7% / −70.8% (Xeon 6973P); Docker PASS |
| v3 interner slots | `2bd0fe0` | hex IDs matched by value in the slot, others by address (one line per lookup) | vs 7256f58: **−3.3% (7/7) / −4.6% (5/5)** (Xeon 6975P) — keep; vs b9481b1 −55.7% / −60.8% (EPYC 7763) |
| v3 dedup slots once | `7256f58` | filter slot per row computed once | vs 2fe1381: **−5.5% (6/7) / −3.0% (5/5)** (EPYC 9V74) — keep; Docker PASS |
| v3 pointer keys | `2fe1381` | long event/click IDs as pointer keys, no interning | vs 3b0bd10: −2.2% (6/7) / +1.2% (2/5) (Xeon 6975P; 60M input on disk); local −24%; Docker PASS |
| v3 long IDs | `3b0bd10` | sharded arena interner; fast decoder interns IDs > 9 bytes; Parted without THP advice | vs b9481b1 (new data): **−54.2% (7/7) / −57.1% (5/5)** (Xeon 6975P); vs a02705c: **−52.0% / −56.3%** (EPYC 7763); Docker PASS |
| v3 Parted | `d7cc29e` | per-source partition buffers on huge-page mappings | vs 599c093: −2.9% (7/7) / −5.0% (5/5) (EPYC 7763, I/O-bound runner); vs 1c0881a: **−3.5% (6/7) / −3.0% (4/5)** (EPYC 7763, 20M on tmpfs) — keep; RSS 60M 11.0 → 12.9 GB |
| PBATCH 64 | `4ad18e2` (ab/v2-pb64) | prefetch batch 32 → 64 | vs e450f86: −0.9% (4/7) / **−2.3% (5/5)** (EPYC 9V45) — take |
| CHUNK 2 MiB | `788ec62` (ab/v2-chunk2) | scan granule 4 → 2 MiB | vs e450f86: **+6.7% (0/7) / +8.2% (0/5)** (EPYC 9V74) — dropped |
| P2e hybrid | `33e6bad` (exp/v2-hybrid) | persons in the scan once links are published, flagged rest in dedup | vs a02705c: **+4.5% (0/7) / +2.8% (0/5)** (Xeon 6975P) — dropped |
| P2d EvC | `0180390` (exp/v2-evc) | 16-byte deduped event layout, generic sessions | vs a02705c: −0.2% (4/7) / **+6.7% (2/5)** (EPYC 7763) — dropped |
| PBATCH 128 | `8e3e842` (ab/v2-pb128) | prefetch batch 64 → 128 | vs a02705c: −0.2% (4/7) / +0.4% (1/5) (Xeon 6975P) — dropped |
| 1 link thread | `039fd37` (ab/v2-lk1) | link pool ncpu/4 | vs e450f86: −0.0% (4/7) / −0.7% (4/5) (EPYC 7763) — neutral, keep ncpu/2 |
| links-overlap | `e450f86` | links on own pool concurrent with the events scan; persons in dedup | vs 80c1ff3: **−3.9% (6/7) / −2.9% (4/5)** (EPYC 9V74); Docker PASS |

Whole-branch checks vs b9481b1: `e450f86` **−17.8% (7/7) / −16.8% (5/5)** on EPYC 7763; `9b850aa` **−14.5% (7/7) / −14.0% (5/5)** on EPYC 9V45; `537f83e` **−15.4% (7/7) / −12.9% (5/5)** on EPYC 7763; `571ea2e` −2.2% (6/7) / −12.4% (5/5) on Xeon 6975P; `70d5eef` −13.0% (7/7) /
−11.5% (5/5) on EPYC 9V74 (both still contain the P3 regression).

## Best submittable commit

**`b9d02ae579ac4f2253fc83e16c4f1f2b61c6c14d`** (branch `opt/v2-fast`; also `docker/v3-thp`) = `c48f601` + MADV_HUGEPAGE
on the Parted partition buffers. Evidence: same commit (`971ee32` = the same change on c48f601) vs `c48f601` on two runners:
**−7.1% (7/7)** at 20M on Xeon 6975P and **−6.4% (7/7)** on Xeon 8573C (gate PASS both). Caveat: at 60M with the input
on disk and the 15 GB runner near its limit it is slower (+2.8% (1/5), +4.3% (1/5); RSS 12.2 → 13.5–13.7 GB); the
scorer's warm-cache runs track the 20M numbers, so it is kept. Docker check PASS (EPYC 7763: image 123 s, 30 MB,
uid 1000; 36/36 fixtures and dev MATCH in the container). Local gates pass (tests, fixtures, dev, fuzz strict +
default, MATCH vs b9481b1 at 20M seeds 11/5 and 60M).

If the scorer is believed to run large data under memory pressure, submit **`c48f601b2ddd6eefda6379963d7ae914ae0c1ca6`**
instead (no THP; Docker PASS; −4.4% / −8.5% vs 89c8210 on EPYC 9V74).

Fallbacks: `c48f601b2ddd6eefda6379963d7ae914ae0c1ca6` (Docker PASS); `89c8210ae518d499bae3426f8e2b3940f8659944` (Docker PASS); `7256f585299799c1e44e925f945bc320c57bc557` (Docker PASS); `2fe13814b373bdebce90afa8b2fb968fde356ec5` (−61.9% / −67.7% vs b9481b1, Docker PASS); `3b0bd10b53239533a05be84ec766d3fca48f7744` (vs b9481b1 −54.2% / −57.1% on Xeon 6975P, Docker PASS).

Previous (old generator only): `a02705ccf8d8bfff6a09f0311a7330018dfeb302` (−17.0% / −13.8% vs b9481b1 on EPYC 7763, Docker PASS); `e450f86724aa71882be512bc7ae8ba39bc0fd221` (vs b9481b1 −17.8% / −16.8% on EPYC 7763, Docker PASS); `80c1ff330450e1638a7b29d438a37f71f3b385a9` (−15.9% / −15.7% vs b9481b1 on EPYC 7763, Docker PASS); before that `aead35696f173fa9ba4a2909a00bc01f823f7d0d` (−15.7% / −17.8% on EPYC 7763, Docker PASS).

## Tried and dropped

- `ab/v3-lf` (`1620872`): lock-free interner lookups (atomic slot tables; lock only to insert/grow). CI vs c48f601
  (Xeon 6973P): −0.0% (3/7) / −0.3% (3/5): neutral, dropped.
- `exp/v2-parted-thp` (`e8c50e3`): per-chunk partition buffers on explicit THP mappings. Faults 480K → 200K, but
  local wall noisier and slower (multi-second stalls from direct compaction; munmap shootdowns).
- `exp/v2-direct-parquet` (`cd1f388`): hand-written PLAIN Parquet writer. Correct (reads back in parquet-rs,
  pyarrow, polars; MATCH), but files 3× larger and slower to write than Arrow's dictionary pages.
- Two interleaved scan cursors per chunk (to overlap per-line latency): slower (twice the write streams).
- Bulk newline index before per-line decode (`exp/v2-lineidx`, local): 4-thread scan 0.82–0.87 → 0.91–1.00 s; dropped.
- No dictionary for `user_id` in the Arrow writer (local): no faster, files 32 → 37 MB; dropped.
- MAP_POPULATE on the partition buffers (local): 2.5 → 2.9 s (populates serially, slack included); dropped.
- 64-bit-half structural masks: −10% on the mask step only; not kept separately.
