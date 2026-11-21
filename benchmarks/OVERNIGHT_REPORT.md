# Overnight optimization report (Sat 2026-11-21)

Unattended loop per [OVERNIGHT_BRIEF.md](OVERNIGHT_BRIEF.md), 03:45 to about 07:45 EST (08:45–12:45 UTC).
**Nothing was submitted. `main` was not touched.** Branches pushed: `opt/overnight` and `opt/night-*` only.
Full details for every experiment: [optimization-log.md](optimization-log.md) (section "Overnight loop").

## Recommendation

**Recommended commit: `7809b5287dd05fd21240ea61493ca4f1eaa5003a`** (`benchmarks/BEST`; `opt/overnight` HEAD has the
same `src/`, `Cargo.toml` and `Cargo.lock`, plus logs only). It contains night-01…07, 10, 11, 14, 15.

Final CI comparisons (GitHub x86 runner, 4 vCPU, interleaved warm pairs, gate = unit tests + 36 fixtures + dev
answer + identical Parquet output vs the base at 20M and 60M):

| Base | Run (candidate = same engine as `7809b52`) | Runner | Gate | 20M base → cand (Δ, wins) | 60M base → cand (Δ, wins) | 60M max RSS base / cand |
|---|---|---|---|---:|---:|---:|
| **`34a18e7`** (scored 5.79 s) | `66101a2`, [results](../../bench-results/results/66101a2b048901893adac2756ff3332f340c9fc0.md) | EPYC 7763 | PASS | 9.028 → **4.628 s** (**−48.7%**, 7/7) | 31.382 → **17.725 s** (**−43.5%**, 5/5) | 13.19 / 12.27 GB |
| **`b85649a`** (previous BEST) | `9dcf1a5`, [results](../../bench-results/results/9dcf1a563f5bd0ed55b49d3765d94e65ddab243f.md) | Xeon 8370C | PASS | 8.600 → **4.959 s** (**−42.3%**, 7/7) | 29.117 → **17.182 s** (**−41.0%**, 5/5) | 12.25 / 12.30 GB |

Results live on the `bench-results` branch (`results/<sha>.md` / `.json`). Both gates include identical Parquet output
to the base at 20M and 60M. A naive scale of the 5.79 s practice score by the 20M ratio suggests roughly 3 s, but the
scorer (8 hyperthreads, Docker, cold-ish cache) is not the CI runner; treat that as a guess, not a prediction.

Correctness of the recommended commit, beyond the CI gate: `cargo test --release` 13/13; fixtures 36/36; dev
answer MATCH; `tests/fuzz.py` strict 500 + default 500 with 0 failures at every merge (different seeds each time,
about 30,000 fuzz cases in total over the night); 20M seed 11 Parquet identical to `b85649a`'s; 1 thread vs 4
threads identical at 20M; **builds and passes (unit, fixtures, dev, 20M equality) with rustc 1.95**, the
Dockerfile's toolchain (CI uses the runner's newer stable).

## Experiments

CI numbers are candidate vs the BEST of the time (each run's own base); runner CPUs varied between runs (EPYC 7763 /
9V45 / 9V74, Xeon 8370C / 8573C / 6973P-C / 6975P-C), so only the paired comparison within one run is meaningful.
Keep rule: gate PASS, 20M median ≤ −2% with ≥ 5/7 pair wins, 60M median ≤ +0.5%.

| # | Branch | CI commit | Change | 20M Δ (wins) | 60M Δ (wins) | Decision |
|---|---|---|---|---:|---:|---|
| 01 | `opt/night-01-dense-campday` | `2ff8e5f` | Dense campaign×day accumulators in sessionize | −2.1% (5/7) | −0.9% (4/5) | **Keep** (narrow) |
| 02 | `opt/night-02-parallel-links` | `77b6ec0` | Parallel link interning, integer union-find, partitioned name map | −4.9% (7/7) | −5.8% (5/5) | **Keep** |
| 03 | `opt/night-03-packed-eid` | `e3219ff` | Packed u64 `event_id` dedup keys (exact fallback), EvRaw 64→48 B | −10.7% (7/7) | −11.0% (5/5) | **Keep** |
| 04 | `opt/night-04-fused-scanner` | `45bdcbd` | Fused one-pass SSE2 line+field scanner (scalar fallback) | −4.9% (7/7) | −4.7% (5/5) | **Keep** |
| 05 | `opt/night-05-packed-lookups` | `83fe1be` | `PMap`: packed keys for product/campaign lookups | −2.8% (5/7) | −8.1% (5/5) | **Keep** |
| 06 | `opt/night-06-packed-names` | `3aa0b90` | Packed keys for link interning + name map | −4.3% (6/7) | −4.7% (5/5) | **Keep** |
| 07 | `opt/night-07-packed-cid` | `2d683c6` | Clicks parsed at scan time, packed `click_id`, keep first copy | −2.3% (7/7) | −3.1% (5/5) | **Keep** |
| 08 | `opt/night-08-swar-ts` | `48a91c9` | SWAR F1/F2 timestamp parse | −1.6% (5/7) | −0.2% (3/5) | Revert |
| 09 | (local only) | – | Dedup table of row indices | no local gain | – | Revert |
| 10 | `opt/night-10-person-key` | `70531fc` | Packed person key carried with every event/click | **−23.6%** (7/7) | **−25.2%** (5/5) | **Keep** |
| 11 | `opt/night-11-nb256` | `ca17f21` | `NB` 512 → 256 | −3.0% (6/7) | −3.1% (4/5) | **Keep** |
| 12 | (local only) | – | PLAIN Parquet encoding | ~−1% local (output 0.27→0.20 s) | – | not pushed |
| 13 | (local only) | – | Flat open-addressing name table | slower locally | – | Revert |
| 14 | `opt/night-14-single-pack` | `a649871` | Person lookup packs once, hashes once (fmix-keyed tables) | −3.3% (6/7) | −4.5% (5/5) | **Keep** |
| 15 | `opt/night-15-swar-ts-retest` | `36c4091` | Re-test of 08 on the ~30% faster base | −4.7% (7/7) | −2.9% (4/5) | **Keep** (re-test) |
| 16 | `opt/night-16-links-keys` | `320fbae` | Links naming / name map on packed keys | −1.0% (5/7) | +3.6% (2/5) | Revert |
| 17 | `opt/night-17-chunk16` | `341ae34` | `CHUNK` 4 → 16 MiB | −1.0% (3/7) | +2.5% (0/5) | Revert |
| 18 | (local only) | – | Fast exit (`forget` tables + `exit(0)`) | no local gain | – | Revert |
| 19 | `opt/night-19-nb128` | `1672787` | `NB` 256 → 128 | −1.7% (6/7) | −1.2% (2/5) | Revert |
| 20 | (local only) | – | mimalloc global allocator | no clear local gain, adds a C dep | – | Revert |

Product of the kept steps' paired medians: about 0.49× at 20M and 0.45× at 60M versus `b85649a` (an estimate across
different runners; the final table above is the direct measurement).

Where the time goes now (CI 60M phase log of night-19's run, same engine as BEST apart from `NB`; Xeon 8573C):
links 1.0 s, clicks 2.0 s, **events 11.9 s** (scan+parse dominates), sessions 2.4 s, output 0.5 s.

## Risks (read before submitting)

1. **Rules-based shortcuts.** (a) Night-07 keeps the *first* click copy instead of the smallest raw tuple: correct
   because rules.md says click copies have identical values ("keep any one"). (b) A2 (earlier) and all event dedup keep
   the first copy on equal `ingest_ts` (rules: such copies are identical). If the hidden data ever broke these promises
   the output could differ; the fuzzer stays inside the promises by design.
2. **Packed IDs** (`pack9`: ≤ 9 bytes, each 1..=127) are exact by construction, and every other ID goes through
   byte-string tables; unit tests check order preservation, injectivity and the fallbacks, and the fuzzer covers
   long / non-ASCII / case and space variants. The generated data packs almost everything, so the fallback paths are
   exercised mainly by fuzz cases, not by the 20M/60M runs.
3. **SSE2 scanner**: x86_64 baseline only (no `target-cpu`, no runtime detection needed); a scalar mask builder is
   used elsewhere. Lines over 128 bytes use the old splitter. Covered by a 30,000-case equivalence test vs the old
   splitter plus the fixture/fuzz gates.
4. **Memory**: 60M max RSS rose from 12.2 GB (`b85649a`) to about 12.3 GB (night-10 added 8 B per record); fine
   under 24 GB, and the final runs report it.
5. **Measurement noise**: runner CPUs change between runs; night-01 (−2.1%) and night-05 (5/7 wins) passed narrowly.
   Night-11 and night-14/15 were measured against a base that did not yet include the change merged just before them;
   their combination is what the final comparison runs measure.
6. **Toolchain**: CI compiles with the runner's stable rustc; the Dockerfile uses 1.95. Verified locally that 1.95
   builds the recommended commit and passes unit tests, fixtures, dev answer and 20M equality. The Docker image itself
   was not built tonight (no Docker in the session).

## Next ideas (not tried, or tried and parked)

- **Event scan is still ~65% at 60M.** Remaining per-line costs (local ablations at 20M): person lookup ≈ 35% of the
  scan (one random probe into a multi-million-entry table), product lookup ≈ 15%, timestamps ≈ 10–20%.
  Batched/prefetched person lookups (look ahead a few lines, prefetch the bucket) are the most promising next step.
- Shrink `EvRaw` further (currently 56 B + 8 B key): e.g. 32-bit window-relative `ts` with an exact wide fallback, or
  drop the name slice for packable persons (unpack the name for output).
- PLAIN Parquet encoding (~1%) and the links-on-packed-keys change (night-16, likely noise on 60M) could be re-tried
  together with a larger change.
- PGO and a static binary in a minimal image (research notes) were out of scope tonight.
