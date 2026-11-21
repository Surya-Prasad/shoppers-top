# v2 status (branch `opt/v2-fast`)

Baseline for every comparison: `b9481b11a506d099ebe47963c927c8fe7bbba988` (1.92 s scored). `benchmarks/BEST` points at it.
All gates per milestone: `cargo test --release`, 36 fixtures, dev answer MATCH, `tests/fuzz.py` strict + default (0 failures),
Parquet MATCH vs b9481b1 on 20M seed 11, 20M seed 5 and 60M seed 11 (local), plus the CI gate (fixtures, dev, 20M/60M vs base).

## Milestones

| Step | Commit | What | Local gates | x86 CI A/B vs b9481b1 (20M / 60M) | Docker check |
|---|---|---|---|---|---|
| P1 | `0e1b74414da1e4d4f1da3f5127e810e738cc9194` | shape-specialized events/clicks decoder, exact fallback | all pass | **−14.9% (7/7) / −12.1% (5/5)** (EPYC 7763: 2.811→2.392, 8.649→7.602 s) | pending |

## Best submittable commit

Not yet declared (needs the Docker check).

## Parked / rejected

- `exp/v2-parted-thp` (`e8c50e3`): per-chunk partition buffers on explicit THP mappings. Faults 480K → 200K, but local wall
  got noisier and slower (multi-second stalls from direct compaction under page-cache pressure; munmap shootdowns). Not used.
