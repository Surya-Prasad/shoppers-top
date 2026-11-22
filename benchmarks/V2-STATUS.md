# v2 status (branch `opt/v2-fast`)

Baseline for every comparison: `b9481b11a506d099ebe47963c927c8fe7bbba988` (1.92 s scored). `benchmarks/BEST` points at it.

Gates run locally for every commit below: `cargo test --release`, 36 fixtures, dev answer MATCH, `tests/fuzz.py`
strict + default (0 failures, 150–500 cases each), Parquet MATCH vs b9481b1 on 20M seed 11, 20M seed 5 and
60M seed 11; plus the CI gate (unit tests, fixtures, dev, 20M/60M equal to the base) on every CI A/B.

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
| ClScan | `80c1ff3` | monomorphic clicks loop (no measured change locally) | head vs b9481b1 pending; Docker check pending |
| NB 512 | `466f6a7` (exp/v2-nb512) | 512 hash partitions | vs 80c1ff3 pending |

Whole-branch checks vs b9481b1: `537f83e` **−15.4% (7/7) / −12.9% (5/5)** on EPYC 7763; `571ea2e` −2.2% (6/7) / −12.4% (5/5) on Xeon 6975P; `70d5eef` −13.0% (7/7) /
−11.5% (5/5) on EPYC 9V74 (both still contain the P3 regression).

## Best submittable commit

`aead35696f173fa9ba4a2909a00bc01f823f7d0d` (branch `opt/v2-fast` history; also `docker/v2-p1b`): CI gate PASS,
−15.7% / −17.8% vs b9481b1 on EPYC 7763, Docker check PASS (image builds, 36/36 fixtures and dev MATCH in the
container with scorer flags). To be replaced by a later head once its Docker check and A/B are in.

## Tried and dropped

- `exp/v2-parted-thp` (`e8c50e3`): per-chunk partition buffers on explicit THP mappings. Faults 480K → 200K, but
  local wall noisier and slower (multi-second stalls from direct compaction; munmap shootdowns).
- `exp/v2-direct-parquet` (`cd1f388`): hand-written PLAIN Parquet writer. Correct (reads back in parquet-rs,
  pyarrow, polars; MATCH), but files 3× larger and slower to write than Arrow's dictionary pages.
- Two interleaved scan cursors per chunk (to overlap per-line latency): slower (twice the write streams).
- 64-bit-half structural masks: −10% on the mask step only; not kept separately.
