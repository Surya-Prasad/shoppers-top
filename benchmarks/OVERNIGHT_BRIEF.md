# Overnight optimization loop: brief

Paste this as the first message of a Claude Code session (cloud at claude.ai/code on
`Surya-Prasad/shoppers-top`, or local) to run the loop.

You are the overnight optimizer for our Rust entry in the Builders Buildathon "Shopping Sessions" track. Work
autonomously until **Sat 2026-11-21 09:00 AM EST (14:00 UTC)**, improving speed one measured change at a time
while keeping output **exactly** correct. GitHub Actions does the x86 timing; you do the engineering.

## Setup
1. `git fetch origin && git checkout opt/overnight` (**never work on main**).
2. Read `docs/rules.md` (canonical; wins every dispute), `benchmarks/OPTIMIZATION_PLAN.md`, `benchmarks/optimization-log.md`,
   `benchmarks/RESEARCH.md`, `docs/DESIGN.md`, `INFERENCES.md`, `.github/workflows/bench.yml`, `benchmarks/ci_ab.sh`, `src/`.
3. Keep running with the `/loop` skill in dynamic mode (no interval): about 900 s wake-ups while CI runs (about 13 min per run).

## CI measurement
- Push `opt/night-NN-<slug>` (from `opt/overnight` HEAD) with changes under `src/` or the Cargo files, and
  `benchmarks/ci_ab.sh` runs on an x86 runner (AMD EPYC 9V74, 4 vCPU). It builds the candidate (HEAD) and the base
  (`benchmarks/BEST`), gates correctness (unit tests, 36 fixtures, dev answer, identical Parquet vs base at 20M and 60M),
  then times interleaved warm pairs (7 at 20M, 5 at 60M).
- Results: `git fetch origin bench-results && git show origin/bench-results:results/<sha>.md` (and `.json`).
- Calibration (A/A, same engine both sides): 20M 8.19 vs 8.22 s, 60M 29.07 vs 29.03 s; noise about ±1%.
  x86 60M profile (A2): links 2.7 s, clicks 3.5 s, **events 17.1 s (59%)**, sessions 4.9 s, write 0.6 s.
  Practice score 5.79 s; leaderboard top 0.48 s.

## Keep or revert
- Keep only if: gate PASS **and** 20M median −2% or better with the candidate faster in at least 5/7 pairs **and**
  60M median no worse than +0.5%. Any correctness failure means revert.
- Keep: merge into `opt/overnight`, set `benchmarks/BEST` to the full SHA, log a register row and a section in
  `benchmarks/optimization-log.md`, commit with `[skip ci]`, push.
- Revert: keep the branch, log the result and the reason (`[skip ci]`).
- Rebase any in-flight candidate onto a newly kept best and push it again. Prepare the next experiment while CI runs.

## Local checks before every push
`cargo test --release`; fixtures 36/36 (`python scripts/run_fixtures.py fixtures -- target/release/shopping-sessions`);
dev answer MATCH (generator.zip and dev-answer.zip from https://www.joinbuilders.org/buildathon/tracks/shopping-sessions/files/,
`--seed 1 --events 1000000 --clicks 250000`); fuzz vs oracle `STRICT_SPEC=1 python tests/fuzz.py <bin> 500 <seed>` plus
default mode, both with 0 failures.

## Queue (one change each)
1. Dense campaign/day accumulators in `sessionize` (replace about 104M hash updates; days from the manifest).
2. Parallel link resolution (the name stays the byte-wise smallest raw ID).
3. Order-preserving packed u64 IDs (up to 9 bytes, ASCII 1..=127, 7 bits/char, left-aligned) with exact interning
   fallback; compact records (EvRaw well below 64 B). Unit-test the order property.
4. Fused single-pass SIMD/SWAR scanner + SWAR F1 timestamps (exact dialect; runtime detection + scalar fallback; no target-cpu).
5. Per-thread reused `pread` buffers instead of mmap (boundary lines handled exactly once).
6. Cache-friendly dedup (prefetch / batched probes); prefix-sum partitioning.
7. Minimal hand-written Parquet writer, only if write time matters (`day` DATE, top_category nullable).
Ideas from RESEARCH.md; **never copy code from other teams' repos.**

## Hard rules
Never push to main, never force-push, never delete or move branches or tags, **never submit to the leaderboard**.
Push only `opt/overnight` and `opt/night-*`. No datasets in git. rules.md semantics are inviolable. Keep
`panic = "unwind"`. Never write outside `$OUTPUT_DIR`. Every hasher change needs a spread test. When in doubt, choose
correctness and log it.

## Hand-off (by 09:00 AM EST)
Stop new experiments at about 08:30. Write `benchmarks/OVERNIGHT_REPORT.md` on `opt/overnight` (`[skip ci]`): every
experiment (branch, commit, CI result, decision), the **recommended commit** (final BEST) with CI numbers against `34a18e7`
and `b85649a`, risks, and next ideas. Stop the loop. The user decides about submitting.
