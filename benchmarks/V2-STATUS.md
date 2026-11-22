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

**`a02705ccf8d8bfff6a09f0311a7330018dfeb302`** (branch `opt/v2-fast`; also `docker/v2-a02705c`) = `e450f86` + prefetch
batch 64. Evidence: CI gate PASS; vs b9481b1 **−17.0% (7/7) at 20M, −13.8% (5/5) at 60M** on EPYC 7763; same-runner vs
`e450f86` −0.9% (4/7) / −2.3% (5/5) on EPYC 9V45; Docker check PASS (image builds in 120 s, 30 MB, uid 1000; 36/36
fixtures and dev MATCH in the container with scorer flags). Local gates all pass (fuzz strict + default 200 each, 0
failures; MATCH vs b9481b1 on 20M seed 11, 20M seed 5, 60M seed 11).

Previous: `e450f86724aa71882be512bc7ae8ba39bc0fd221` (vs b9481b1 −17.8% / −16.8% on EPYC 7763, Docker PASS); `80c1ff330450e1638a7b29d438a37f71f3b385a9` (−15.9% / −15.7% vs b9481b1 on EPYC 7763, Docker PASS); before that `aead35696f173fa9ba4a2909a00bc01f823f7d0d` (−15.7% / −17.8% on EPYC 7763, Docker PASS).

## Tried and dropped

- `exp/v2-parted-thp` (`e8c50e3`): per-chunk partition buffers on explicit THP mappings. Faults 480K → 200K, but
  local wall noisier and slower (multi-second stalls from direct compaction; munmap shootdowns).
- `exp/v2-direct-parquet` (`cd1f388`): hand-written PLAIN Parquet writer. Correct (reads back in parquet-rs,
  pyarrow, polars; MATCH), but files 3× larger and slower to write than Arrow's dictionary pages.
- Two interleaved scan cursors per chunk (to overlap per-line latency): slower (twice the write streams).
- Bulk newline index before per-line decode (`exp/v2-lineidx`, local): 4-thread scan 0.82–0.87 → 0.91–1.00 s; dropped.
- No dictionary for `user_id` in the Arrow writer (local): no faster, files 32 → 37 MB; dropped.
- 64-bit-half structural masks: −10% on the mask step only; not kept separately.
