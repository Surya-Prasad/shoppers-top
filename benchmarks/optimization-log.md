# Optimization log

Plan: [OPTIMIZATION_PLAN.md](OPTIMIZATION_PLAN.md). One change at a time; keep only what passes every
gate. **Nothing here has been submitted.** Scored baseline: **5.79 s** (`BB-DB9B9054`, `34a18e7`, tag `scored-baseline`).

## Definitions

- **Host:** *interim* timing host = colima Linux VM on Apple silicon (linux/arm64, 8 vCPU, 18 GB,
  Docker 29.5.2). Data and output on **VM-local Docker volumes**. Numbers are valid for **A/B decisions
  only**; they don't predict the x86 scorer. The plan's x86 Xeon VM (GCP `n2-standard-8` recommended)
  has not been provisioned.
- **Flags:** the official starter's `docker_run.sh` scorer flags (no network, read-only root, 64 MB
  `/tmp` tmpfs, cap-drop ALL, no-new-privileges, pids 4096, uid 1000, 8 CPUs), with `--memory 16g`
  instead of 24g (the VM has 18 GB). Harness: [bench.sh](bench.sh), timed **inside** the VM around
  `docker run` (fresh container each run; the Mac CLI round-trip is excluded).
- **host-cold:** `sync; echo 3 > /proc/sys/vm/drop_caches` in the VM, then one run. This clears the
  guest page cache only, not host or device caches.
- **warm:** 5 consecutive fresh-container runs right after the cold run. Median, min and max reported.
- **Memory:** a separate run per candidate: cgroup `memory.peak` plus peak `anon` from `memory.stat`
  (sampled every 20 ms). `anon` is the stable figure; `memory.peak` also counts page cache, so it
  varies with cache state.
- **Datasets** (official generator, built inside the VM): `d20` = 20M seed 9 (window 2028-01-18 → 02-01,
  73 event files); `d20b` = 20M seed 11 (2026-09-06 → 09-20, 85 files); `d60` = 60M seed 11. A second
  60M seed is **not** generated: the Mac has ~10 GB free disk.

## P0: baseline

Toolchain: rustc 1.95 (builder image `rust:1.95-slim-bookworm`), runtime `debian:bookworm-slim`, no
`RUSTFLAGS`, release profile with fat LTO, `codegen-units=1`, `panic=unwind`.

`/tmp` audit: the engine writes only under `$OUTPUT_DIR` (dot-prefixed temp files renamed into place);
`STATS_FILE` is opt-in and unset by the scorer. Nothing touches `/tmp` or `/scratch`.

| Image | Data | host-cold | warm median | warm min / max | anon peak | memory.peak |
|---|---|---:|---:|---:|---:|---:|
| `base` (34a18e7, as scored) | 20M s9 | 4.57 s | **2.814 s** | 2.788 / 2.863 | 2.7 GB | 4.6 GB |
| `p0i` (+ stage timers) | 20M s9 | 3.60 s | 2.796 s | 2.748 / 2.848 | 2.7 GB | 2.7 GB |
| `base` | 20M s11 | 3.71 s | **2.752 s** | 2.732 / 2.890 | – | – |
| `p0i` | 20M s11 | 3.67 s | 2.751 s | 2.733 / 2.849 | – | – |
| `base` | 60M s11 | 15.15 s | **11.298 s** | 11.106 / 12.122 | 8.2 GB | 8.3 GB |
| `p0i` | 60M s11 | 14.95 s | 11.468 s | 11.315 / 11.798 | 8.2 GB | 8.3 GB |

The stage timers cost nothing measurable (differences are within run-to-run noise), so `p0i` becomes the
A/B reference for later steps. Host-cold is noisy (3.6–4.6 s at 20M); treat a single cold run as indicative.

### P0 profile (60M s11, warm, `p0i`)

| Stage | Wall | Share |
|---|---:|---:|
| links (single-threaded union-find + interning) | 0.97 s | 9% |
| clicks: scan + parse + partition | 0.24 s | 2% |
| clicks: dedup + validate + repartition | 1.02 s | 9% |
| **events: scan + parse + partition** | **3.87 s** | **35%** |
| **events: dedup + validate + repartition** | **2.56 s** | **23%** |
| transposes (both) | 0.02 s | <1% |
| sessions (sum of partitions 14.8 s CPU; slowest partition 0.195 s) | 1.86 s | 17% |
| write Parquet | 0.47 s | 4% |
| **total (in-process)** | **11.01 s** | |

Observations, used to order the next steps:
- Events are 58% of wall time. Scan+partition (3.9 s) costs more than dedup (2.6 s) even though
  parsing alone was cheap natively, which points at **memory traffic**: 80-byte `EvRaw` copies scattered
  into 512 × 600+ small vectors. This supports B1 and A2.
- Sessions show **no skew** (slowest of 512 partitions is 0.19 s against a 1.86 s phase), so C2 (whale
  parallelism) is not needed.
- Links take nearly 1 s single-threaded; the plan has no item for this (candidate for a later addition).
- Hardware `perf` counters are unavailable in this Apple-silicon VM; cycle and cache-miss profiling is
  deferred to the x86 host.

## A1: single-probe event dedup (Tier A): **KEEP**

Change (`src/main.rs`, `events()` dedup): `best.entry(eid)` with `Vacant`/`Occupied` replaces
`get_mut` + `insert`, so each copy needs one hash probe instead of up to two. Dedup ordering is unchanged
(`earlier()`: smallest parseable `ingest_ts`, raw-tuple tie-break).

**Correctness gate (all passed):**

| Check | Result |
|---|---|
| `cargo test --release` | 4/4 |
| Official fixtures (native) | 36/36 |
| Dev answer (1M, seed 1) | MATCH |
| `tests/invariants.py` on dev | 15/15 |
| Fuzz, strict-spec, 1,000 cases (seed 21) | 0 failures |
| Fuzz, adversarial, 1,000 cases (seed 22) | 0 failures |
| Python oracle, generator seeds 2 and 15 (1M) | MATCH, MATCH |
| Parquet logical equality vs `p0i`, in container: 20M s9, 20M s11, 60M s11 | MATCH ×3 |
| 1 thread vs 8 threads: 20M s9 and 60M s11 | MATCH ×2 |
| Scorer flags; forced failure (read-only `/output`) | exit 101 (fails fast) |

Focused coverage for the plan's named cases: an invalid earliest-ingest copy is fixture E5 plus the
fuzz "earlier bad-ts copy" generator; equal ingest is the fuzz identical-copy generator; duplicates
across input files are fuzz multi-part files; duplicates across 4 MB chunks are covered by the dev set
and the 20M/60M equality checks. No new unit test was added: the dedup loop isn't separable from
`events()` without a refactor, which the plan rules out as an unrelated change.

**Performance (`p0i` = A, `a1` = B, same host, data, flags):**

| Data | A host-cold | B host-cold | A warm median (min/max) | B warm median (min/max) | Δ warm |
|---|---:|---:|---:|---:|---:|
| 20M s9 | 4.80 s | 3.87 s | 2.862 (2.824 / 2.902) | **2.795** (2.650 / 3.044) | −2.3% |
| 20M s11 | 3.53 s | 3.67 s | 2.772 (2.759 / 2.813) | **2.730** (2.720 / 2.757) | −1.5% |
| 60M s11, sequential | 16.98 s | 17.93 s | 14.407 (13.298 / 14.555) | 13.541 (13.366 / 14.017) | (host drift) |
| 60M s11, **A/B/A/B ×4** | – | – | 13.317 (13.203 / 13.442) | **13.092** (12.000 / 13.168) | **−1.7%, B faster in 4/4 pairs** |

Memory: anon peak 2.70 GB (20M) / 8.21 GB (60M): unchanged.

**Host-noise note:** during the A1 runs the Mac's load average was about 8, and 60M times for *both*
images rose about 2 s against P0 (11.3 → 13.3 s). Absolute numbers from this session aren't comparable
to P0's; only the paired A/B comparison is. Host-cold stays noisy (±1 s), so single cold runs didn't
drive the decision.

**Decision:** keep. Tier A, every gate green, consistently a little faster (around the noise threshold
at 20M, 4/4 paired wins at 60M), no memory change, simpler code. `a1` is the new A/B reference.

## A2: drop `EvRaw.line_fields` (Tier A): **KEEP**

Change (`src/main.rs`): `earlier()` now compares only `(ingest_bad, ingest)`. On a tie, the copy already
kept stays. The 16-byte line slice that existed only to re-split lines for a raw-tuple tie-break is removed.
Relies on rules.md, Events step 2: *"Copies with equal `ingest_ts` are identical."* (cited in the code).
**`size_of::<EvRaw>()`: 80 → 64 bytes** (a new `layout::record_sizes` test prints all record sizes).

**Correctness gate (all passed):** unit 5/5; fixtures 36/36; dev MATCH; invariants 15/15; fuzz strict
1,000 (seed 31) and adversarial 1,000 (seed 32), 0 failures; oracle seeds 3 and 15 MATCH; Parquet equality
vs A1 in container at 20M s9, 20M s11 and 60M s11: MATCH ×3; 1 vs 8 threads at 20M and 60M: MATCH ×2;
forced failure exits 101. No fuzz case needed relaxing (the fuzzer only makes identical equal-ingest
copies, as the rules promise).

**Determinism proof (plan requirement):** the dev set with **106,525 injected equal-`ingest_ts` copies**:
values identical, quoting re-randomized, half in a new file and half shuffled into the original files so they
cross 4 MB chunk boundaries (lines parsed with the reference dialect first, so malformed lines are never
"repaired" into new events). The Python oracle confirms the correct answer is unchanged (still equal to
the official dev answer). A2 with `NCPU=1` and with `NCPU=8`: both **MATCH the official dev answer**,
178,478 copies removed in both.
(A first attempt built copies by stripping quotes from raw lines, which turned malformed lines into valid
events. A1 and A2 both "differed" on it, exposing the faulty test data; it was rebuilt as above.)

**Performance (A = `a1`, B = `a2`). Cold: one run each. Warm: 5 runs interleaved A,B,A,B…:**

| Data | A cold | B cold | A warm median (min/max) | B warm median (min/max) | Δ warm | B faster |
|---|---:|---:|---:|---:|---:|---:|
| 20M s9 | 4.61 s | 3.52 s | 2.760 (2.723 / 2.955) | **2.713** (2.701 / 2.732) | −1.7% | 5/5 |
| 20M s11 | 3.94 s | 3.43 s | 2.885 (2.643 / 3.514) | **2.740** (2.720 / 3.047) | −5.0% | 4/5 |
| 60M s11 | 16.10 s | 14.98 s | 12.715 (12.397 / 13.495) | **12.601** (12.281 / 13.536) | −0.9% | 3/5 |

Memory (anon peak): 20M **2.70 → 2.49 GB** (−8%); 60M **8.21 → 7.26 GB** (−12%).

**Decision:** keep. Every gate green, simpler code (no re-split on ties), clearly less memory, faster or
neutral everywhere (60M is within noise). Host load average was about 7 during the runs; the A/B
interleaving controls for it. `a2` is the new A/B reference.

## A3: `NB` / `CHUNK` sweep (Tier A): **REVERT**, keep 512 / 4 MiB

**Method.** A throwaway build read `NB`/`CHUNK` from env vars (patch kept as
[experiments/a3-sweep-knobs.patch](experiments/a3-sweep-knobs.patch); never merged). Each configuration was
checked against the dev answer first. The harness gained `EXTRA_ARGS` to pass env vars into the container.

**Stage 1: 12 configurations at 20M** (NB ∈ {128, 256, 512, 1024} × CHUNK ∈ {1, 4, 16} MiB), 3 warm
rounds × 2 seeds, interleaved per round. Sum of the two seeds' medians, relative to 512/4:

| NB | CHUNK | 20M s9 median | 20M s11 median | vs 512/4 |
|---:|---:|---:|---:|---:|
| 256 | 16 MiB | 2.747 | 2.602 | −10.5% |
| 128 | 16 MiB | 2.862 | 2.612 | −8.4% |
| 128 | 4 MiB | 2.918 | 2.669 | −6.5% |
| 256 | 4 MiB | 2.943 | 2.748 | −4.7% |
| 512 | 16 MiB | 2.976 | 2.918 | −1.3% |
| **512** | **4 MiB** | 3.101 | 2.873 | 0 (current) |
| 128 | 1 MiB | 2.760 | 3.249 | +0.6% |
| 256 | 1 MiB | 3.265 | 2.822 | +1.9% |
| 512 | 1 MiB | 3.099 | 3.025 | +2.5% |
| 1024 | 16 MiB | 3.205 | 3.176 | +6.8% |
| 1024 | 4 MiB | 3.460 | 3.336 | +13.8% |
| 1024 | 1 MiB | 4.285 | 3.917 | +37.3% |

Robust signals: NB=1024 and 1 MiB chunks are always worse. Host load average swung 1.3–6.4 during
the sweep, so single-cell differences under about 5% aren't reliable.

**Stage 2: top 3 + current at 60M** (env build; one cold run each + 4 interleaved warm rounds):
128/16 −6.6% (anon 6.95 GB), 128/4 −5.5%, 256/16 −3.9%, against 512/4 (7.26 GB).

**Stage 3: confirmation with constants** (`NB=128`, `CHUNK=16 MiB` as plain `const`, on a clean
branch from A2). Full correctness gate **passed** (unit 5/5, fixtures 36/36, dev MATCH, invariants 15/15,
fuzz 2,000 cases with 0 failures, oracle seeds 4 and 15, Parquet equality vs A2 at 20M ×2 and 60M, 1 vs 8
threads at 20M and 60M, error path 101). Performance vs A2, interleaved:

| Data | A2 cold | A3 cold | A2 warm median | A3 warm median | Δ | A3 faster |
|---|---:|---:|---:|---:|---:|---:|
| 20M s9 | 10.30 s* | 3.88 s | 2.584 | **2.466** | −4.6% | 5/5 |
| 20M s11 | 3.24 s | 3.16 s | 2.555 | **2.447** | −4.2% | 4/5 |
| 60M s11 (11 pairs) | 13.18 s | 13.08 s | **12.144** | 12.405 | **+2.2%** | **2/11** (mean paired +0.37 s) |

\* A host stall; not used.

Memory (anon): 20M 2.49 → 2.69 GB (+8%); 60M 7.26 → 6.96 GB (−4%).

**Decision: revert.** At 60M (the final-score metric) the constant build was slower in 9 of 11 pairs,
and the plan says a clear 60M regression blocks a change even when 20M improves. Code stays at
**A2: NB=512, CHUNK=4 MiB**.

**What we learned (hypotheses, not yet tested):**
1. *The best partition count depends on data size.* With 128 partitions, each per-partition dedup table at
   60M holds about 490k copies × 64 B ≈ 31 MB, far beyond per-core cache; at 512 it's about 8 MB. At 20M,
   128 partitions mean about 10 MB per table, which is why fewer partitions win there. This suggests an
   **adaptive NB chosen from total input bytes** (Tier B: workload-dependent; it needs both sizes, and
   it isn't an A-tier constant).
2. The stage-2 60M ranking (env-knob build) didn't reproduce with constants: runtime `NB` changes code
   generation (no constant mask or loop bounds), and the host was drifting (A2 warm went 9.8 → 15.0 s over
   the session). Sweeps on this host only rank configurations roughly; decisions need constant builds and
   interleaved pairs.
3. Bigger chunks (16 MiB) were never worse at fixed NB in stage 1, but their effect alone (512/16:
   −1.3%) is within noise. Not worth a change on its own.

## A4: `MADV_WILLNEED` prefetch (Tier A/B): **REVERT**

Change tried (`src/dialect.rs`, `Source::open`): after the existing `madvise(SEQUENTIAL)`, also
`madvise(WILLNEED)` over each whole input mapping, so the kernel starts reading every file at open time.

**Correctness gate (all passed):** unit 5/5; fixtures 36/36; dev MATCH; invariants 15/15; fuzz
strict 500 (seed 51) and adversarial 500 (seed 52), 0 failures; Parquet equality vs A2 in container at
20M s9, 20M s11 and 60M s11: MATCH ×3; 1 vs 8 threads at 20M and 60M: MATCH; forced failure exits 101.

**Performance (A = `a2`, B = `a4`).** Host-cold: 4 pairs per dataset, caches dropped before **every**
run, order alternating (A,B / B,A). Warm: 5 interleaved pairs. The host was quieter in this session
(load average 1.7 at the start), so absolute times are lower than in A1–A3; only the pairs are compared.

| Data | Kind | A2 median (min–max) | A4 median (min–max) | Δ | A4 faster |
|---|---|---:|---:|---:|---:|
| 20M s9 | host-cold | 3.291 (3.15–4.70) | 3.400 (3.18–3.67) | +3.3% | 1/4 |
| 20M s9 | warm | 2.476 (2.46–2.50) | 2.507 (2.49–2.57) | +1.3% | 1/5 |
| 20M s11 | host-cold | 3.133 (3.01–3.34) | 3.273 (3.26–3.29) | +4.5% | 1/4 |
| 20M s11 | warm | 2.439 (2.42–2.46) | 2.458 (2.43–2.49) | +0.8% | 1/5 |
| 60M s11 | host-cold | 11.091 (10.38–11.93) | 11.508 (10.92–11.81) | +3.8% | 2/4 |
| 60M s11 | warm | 8.417 (8.29–10.10) | 8.317 (8.20–8.81) | −1.2% | 4/5 |

Memory (60M): anon 7.24 vs 7.27 GB; `memory.peak` 7.37 vs 7.39 GB. Unchanged.

**Decision: revert.** The plan keeps A4 only if it helps cold without hurting warm; here it *hurt*
host-cold at every size (+3–4.5%) and was neutral at best when warm.

**Caveat, to re-check on the x86 VM:** "host-cold" drops only the *guest* page cache. The colima
VM's disk image almost certainly stays in macOS's file cache, so a guest-cold read is served at near-memory
speed through virtio, and read-ahead has no slow I/O to overlap with. The hint can only add up-front
work at open time, which is what we see. On a real disk the result may differ.

## Register

| ID | Change | Tier | Commit | Correctness | 20M warm (s9 / s11) | 20M cold | 60M warm | 60M cold | anon peak 60M | Decision |
|---|---|---|---|---|---:|---:|---:|---:|---:|---|
| P0 | Scored baseline | — | `34a18e7` | dev MATCH | 2.814 / 2.752 | 4.57 / 3.71 | 11.298 | 15.15 | 8.2 GB | Preserve |
| P0i | + stage timers | — | `5d40e99` | all gates | 2.796 / 2.751 | 3.60 / 3.67 | 11.468 | 14.95 | 8.2 GB | Reference |
| A1 | Single-probe dedup | A | see git log | all gates | 2.795 / 2.730 | 3.87 / 3.67 | 13.092 vs 13.317 (paired) | 17.93 | 8.2 GB | **Keep** |
| A2 | Drop `line_fields` (EvRaw 80→64 B) | A | see git log | all gates + equal-ingest proof | 2.713 / 2.740 vs A1 2.760 / 2.885 (paired) | 3.52 / 3.43 | 12.601 vs 12.715 (paired) | 14.98 | **7.3 GB** (−12%) | **Keep** |
| A3 | NB/CHUNK → 128 / 16 MiB | A | not merged | all gates | 2.466 / 2.447 vs A2 2.584 / 2.555 (paired) | 3.88 / 3.16 | 12.405 vs 12.144 (11 pairs, **+2.2%**, A3 faster 2/11) | 13.08 | 6.96 GB | **Revert** (60M regression) |
| A4 | `MADV_WILLNEED` whole-file | A/B | not merged | all gates | 2.507 / 2.458 vs A2 2.476 / 2.439 (paired) | 3.400 / 3.273 vs 3.291 / 3.133 (4 cold pairs each) | 8.317 vs 8.417 (paired) | 11.508 vs 11.091 | 7.27 GB | **Revert** (cold slower) |
| B1 | Contiguous partitioning + fused emit | B | | | | | | | | next (profile first) |
| N01 | Dense campaign/day accumulators | B | `2ff8e5f` | CI gate PASS | CI 9.086 vs 9.279 (−2.1%, 5/7) | – | CI 30.937 vs 31.206 (−0.9%, 4/5) | – | 11.9 GB RSS | **Keep** |
| N03 | Packed u64 event_id dedup keys, EvRaw 64→48 B | B | `e3219ff` | CI gate PASS | CI 8.298 vs 9.297 (−10.7%, 7/7) | – | CI 28.837 vs 32.394 (−11.0%, 5/5) | – | 11.7 GB RSS | **Keep** |
| N02 | Parallel link resolution | B | `77b6ec0` | CI gate PASS | CI 7.163 vs 7.530 (−4.9%, 7/7) | – | CI 24.272 vs 25.778 (−5.8%, 5/5) | – | 11.9 GB RSS | **Keep** |
| N04 | Fused SSE2 line+field scanner | B | `45bdcbd` | CI gate PASS | CI 7.239 vs 7.610 (−4.9%, 7/7) | – | CI 24.865 vs 26.080 (−4.7%, 5/5) | – | 11.4 GB RSS | **Keep** |
| N05 | Packed product/campaign lookup keys (`PMap`) | B | `83fe1be` | CI gate PASS | CI 6.808 vs 7.001 (−2.8%, 5/7) | – | CI 22.137 vs 24.086 (−8.1%, 5/5) | – | 11.4 GB RSS | **Keep** |
| N06 | Packed link interning + name map | B | `3aa0b90` | CI gate PASS | CI 6.506 vs 6.799 (−4.3%, 6/7) | – | CI 23.070 vs 24.202 (−4.7%, 5/5) | – | 11.4 GB RSS | **Keep** |
| N07 | Clicks parsed at scan + packed click_id | B | `2d683c6` | CI gate PASS | CI 6.550 vs 6.706 (−2.3%, 7/7) | – | CI 22.246 vs 22.948 (−3.1%, 5/5) | – | 11.4 GB RSS | **Keep** |
| N09 | Dedup table of row indices | B | not pushed | local only | local 20M dedup 0.774 vs 0.738 s | – | – | – | – | **Revert** (no local gain) |
| N08 | SWAR F1/F2 timestamp parse | B | `48a91c9` | CI gate PASS | CI 5.052 vs 5.136 (−1.6%, 5/7) | – | CI 16.854 vs 16.888 (−0.2%, 3/5) | – | 11.4 GB RSS | **Revert** (below −2%) |
| N10 | Packed person key carried with rows | B | `70531fc` | CI gate PASS | CI 3.360 vs 4.401 (**−23.6%**, 7/7) | – | CI 11.433 vs 15.276 (**−25.2%**, 5/5) | – | 12.1 GB RSS | **Keep** |
| N13 | Flat open-addressing name table | B | not pushed | local only | local 20M scan 2.40 vs 2.18 s | – | – | – | – | **Revert** (slower) |
| N11 | NB 512 → 256 | A | `ca17f21` | CI gate PASS | CI 6.365 vs 6.561 (−3.0%, 6/7) | – | CI 22.402 vs 23.129 (−3.1%, 4/5) | – | 11.4 GB RSS | **Keep** |
| N14 | Single-pack person lookup, fmix-keyed tables | B | `a649871` | CI gate PASS | CI 4.550 vs 4.706 (−3.3%, 6/7) | – | CI 14.415 vs 15.090 (−4.5%, 5/5) | – | 12.2 GB RSS | **Keep** |

## Overnight loop (2026-11-21, CI x86 timing)

All timings below are from `.github/workflows/bench.yml` (GitHub-hosted x86, 4 vCPU, interleaved warm pairs:
7 at 20M seed 11, 5 at 60M seed 11), raw results on the `bench-results` branch (`results/<sha>.md`).
Local checks before every push (4-vCPU cloud container): `cargo test --release`, fixtures 36/36, dev answer
MATCH, `tests/fuzz.py` strict 500 + default 500 with 0 failures, and Parquet equality vs BEST at 20M seed 11.
Keep rule: gate PASS, 20M median −2% or better with ≥ 5/7 pair wins, 60M median ≤ +0.5%.
Note: GitHub keeps only **one pending run** per concurrency group, so a second queued push cancels the first
pending one (night-02's first run was cancelled this way); at most one run is queued at a time from now on.

### Night-01: dense campaign/day accumulators: **KEEP**

`sessionize` writes campaign/day counters into a dense `[(ncamp + 1) × ndays]` array (campaign index, `organic`
last; day offset from the first window day, from the manifest) owned by each rayon `fold` accumulator,
instead of a hash map updated once per credit and once per assist. Arrays are summed and turned back into
the sparse `(campaign, day)` rows (a row exists iff credited sessions or assists are non-zero, exactly as before).
Session starts are in-window event timestamps, so the day offset is always in range (indexing is bounds-checked).

CI (`2ff8e5f` vs `b85649a`, EPYC 7763): gate PASS; **20M 9.279 → 9.086 s (−2.1%, 5/7)**; 60M 31.206 → 30.937 s
(−0.9%, 4/5); RSS 12.21 → 11.91 GB at 60M. Meets the rule (narrowly). BEST → `2ff8e5f`.

### Night-03: packed u64 `event_id` dedup keys + smaller `EvRaw`: **KEEP**

An `event_id` of at most 9 bytes in 1..=127 packs left-aligned at 7 bits/byte into a u64 (`pack9`, injective and
order-preserving; unit test `packed::order_and_domain`). Dedup partitions and tables are keyed by that integer, so a
probe no longer dereferences a slice in the input mapping. Unpackable IDs (none in generated data; allowed by the
rules) go through a separate exact `Map<&[u8], EvRaw>` in the same partition; a packable and an unpackable ID are
never equal, so the two sets dedup independently with the same `earlier()` rule. `EvRaw` drops the 16-byte
`eid` slice (64 → 48 B; 56 B with the key). Partition-spread test for packed keys added.

CI (`e3219ff` vs `b85649a`, EPYC 7763): gate PASS; **20M 9.297 → 8.298 s (−10.7%, 7/7)**; **60M 32.394 → 28.837 s
(−11.0%, 5/5)**. Merged on top of night-01 (merge `4270e61`, local checks all green, 20M Parquet equal to
`b85649a`'s); BEST → `4270e61` (night-01 + night-03; the combination is timed by the next candidates).

### Night-02: parallel link resolution: **KEEP**

Link IDs are scattered by a separate hash (`LINK_SEED`, 64 partitions) and interned in parallel, one table per
partition; the union-find then runs sequentially on dense integer edge ends (no hashing), and the name map is
built in parallel as 64 per-partition tables (`Names`, looked up by partition hash + table). A person's name is
still the byte-wise smallest member ID, so the result is independent of partitioning and union order. Spread
test for the link partitions added. Local 20M: links 0.81 → 0.26 s.

CI (`77b6ec0` = night-02 merged with night-01, vs `2ff8e5f`; Xeon Platinum 8573C): gate PASS; **20M 7.530 → 7.163 s
(−4.9%, 7/7)**; **60M 25.778 → 24.272 s (−5.8%, 5/5)**. Merged on top of night-01 + night-03 (test-module
conflict only; local checks all green, 20M Parquet equal). BEST → this merge (night-01 + 02 + 03).

Runner CPUs vary between runs (EPYC 7763, EPYC 9V74, Xeon 8573C), so absolute times differ across results;
only the paired comparison inside one result is used.

### Night-04: fused one-pass SSE2 line + field scanner: **KEEP**

`dialect::for_each_record::<N>` replaces `for_each_line` + `split::<N>` in the events, clicks and links scans. It walks
each line in 16-byte blocks, building `\n`, `,` and `"` bitmasks with SSE2 (x86_64 baseline, no `target-cpu`, no
runtime detection needed; a scalar mask builder on other targets), then cuts fields at the comma bits and checks
quotes per field by mask: none, or exactly one at each end of a field of length ≥ 2; anything else is malformed,
as is a wrong comma count or an empty line. Lines over 128 bytes use the old `split`. Near the chunk end the
block is copied into a `\n`-padded buffer, so a final line without `\n` ends correctly and nothing is read past
the mapping. Unit test `fused_matches_split`: 30,000 random dialect-noise chunks × N ∈ {2, 4, 7} plus exact
16/128-byte block edges, identical to `for_each_line` + `split`.

CI (`45bdcbd` = night-04 merged with BEST `4270e61`, vs `4270e61`; Xeon 8370C): gate PASS; **20M 7.610 → 7.239 s
(−4.9%, 7/7)**; **60M 26.080 → 24.865 s (−4.7%, 5/5)**. Merged on top of night-01/02/03 (local checks green,
20M Parquet equal). BEST → this merge (night-01 + 02 + 03 + 04).

### Night-05: packed keys for product and campaign lookups: **KEEP**

New `src/packed.rs`: `pack9` (moved from night-03) and `PMap<V>`, a table keyed by the packed u64 when an ID packs and
by the exact byte string otherwise (two tables; a packable and an unpackable ID are never equal). `products` (one
lookup per event in the scan) and `campaigns` (one per click) use it; first occurrence still wins. Unit tests:
order/domain of `pack9`, exact `PMap` semantics (case, spaces, `\r`, long and non-ASCII keys, first wins), table
spread of packed keys.

CI (`83fe1be` = night-05 on BEST `ae88fb7`, vs `ae88fb7`; EPYC 9V45): gate PASS; **20M 7.001 → 6.808 s (−2.8%, 5/7)**;
**60M 24.086 → 22.137 s (−8.1%, 5/5)**. Merged on top of night-01…04 (import conflict only; local checks green,
20M Parquet equal). BEST → this merge (night-01…05).

### Night-06: packed keys for link interning and the person-name map: **KEEP**

Link IDs are interned per partition in a `PMap<u32>` and the name map is a `PMap<&[u8]>` per partition; partitions are
chosen by `packed::id_hash` (hash of the packed u64 when the ID packs, else of its bytes, so equal IDs always meet).
`person_of` (one lookup per event and per click) therefore hashes and compares one integer for every packable ID.
Spread test updated to `id_hash`.

CI (`3aa0b90` = night-06 on BEST `baf6156`, vs `baf6156`; EPYC 7763): gate PASS; **20M 6.799 → 6.506 s (−4.3%, 6/7)**;
**60M 24.202 → 23.070 s (−4.7%, 5/5)**. Merged (no conflict, source identical to `3aa0b90`). BEST → this merge (night-01…06).

### Night-07: clicks parsed at scan time + packed `click_id`: **KEEP**

The click scan now resolves the person, parses `ts` against the click window and looks up the campaign, so dedup and
validation no longer go back to the raw fields; `click_id` is packed (exact fallback set, as for events). Dedup keeps
the **first** copy in scan order: rules.md, Clicks step 2, "Copies of a click have identical values, so keep any one"
(deterministic: transpose keeps file-then-chunk order). Validation of the kept copy is unchanged (empty user, then
bad `ts`, then unknown campaign).

CI (`2d683c6` vs `42fa641`; Xeon 8573C): gate PASS; **20M 6.706 → 6.550 s (−2.3%, 7/7)**; **60M 22.948 → 22.246 s
(−3.1%, 5/5)**. Merged (source identical to `2d683c6`). BEST → this merge (night-01…07).

Screened out locally, not pushed: *dedup table of row indices* (key → (part, row), 16-byte entries instead of
56-byte records): 20M dedup 0.738 → 0.774 s, no gain.

### Night-08: SWAR F1/F2 timestamp parsing: **REVERT**

`timefmt::parse` checks the F1/F2 layout (separators and digit positions) with three word-sized masks and extracts
fields from the XOR-0x30 words; F3 keeps the original code (a one-pass F3 rewrite was slower in a microbenchmark,
26.6 vs 16.7 ns, and was dropped before pushing). Equivalence test vs the original parser (2M random mutations of
every form, every calendar day 2024–2029, leap-day edges) passed. Microbenchmark F1: 29 → 14 ns per parse.

CI (`48a91c9` vs `42fa641`; Xeon 6975P-C): gate PASS; 20M 5.136 → 5.052 s (**−1.6%**, 5/7); 60M 16.888 → 16.854 s
(−0.2%, 3/5). Below the −2% bar: **revert** (branch `opt/night-08-swar-ts` kept). A candidate to combine with
another parse-side change later.

### Night-10: packed person key carried with every event and click: **KEEP**

`Person { name, key }`: the person's name plus its packed form (`NOPK` if it doesn't pack), computed once where
the name is at hand (the `Names` table stores `Person` values; an unlinked user packs its own, hot, ID). Events and
clicks carry it, so the post-dedup repartition (`person_part`) and `sessionize`'s person numbering (`Map<u64, u32>`,
exact byte-string table for unpackable names) no longer read name bytes, which were random reads into the input
mappings. Name bytes are read only for output.

CI (`70531fc` vs `68c57c0`; EPYC 9V45): gate PASS; **20M 4.401 → 3.360 s (−23.6%, 7/7)**; **60M 15.276 → 11.433 s
(−25.2%, 5/5)**; RSS 60M 11.37 → 12.13 GB (records grew by 8 B). BEST → this merge (night-01…07 + 10).
60M phase log: links 0.84, clicks 1.41, events 7.14, sessions 1.35, output 0.42 s.

Screened out locally, not pushed: *night-13 flat open-addressing name table* (one segment per link partition,
32-byte entries at load ≤ 1/2): 20M scan 2.18 → 2.40 s, slower (the table is larger than hashbrown's, so more misses).
*glibc `malloc.hugetlb=1`* (env only, as a probe): no change in faults or time.

### Night-11: `NB` 512 → 256 hash partitions: **KEEP**

A3 (on the Mac) had found 128–256 partitions faster at 20M but slower at 60M, because per-partition dedup tables
outgrew the caches. After night-03/07/10 the tables and records are much smaller, so the sweep point was retried
(the partition-spread test now scales with `NB`).

CI (`ca17f21` vs `68c57c0`, i.e. measured before night-10 was merged; EPYC 7763): gate PASS; **20M 6.561 → 6.365 s
(−3.0%, 6/7)**; **60M 23.129 → 22.402 s (−3.1%, 4/5)**. Merged on top of night-10; the combination was checked
locally (all gates; 20M Parquet equal; 4 local pairs, combined build faster in 4/4, about −7%) and is timed by every
later candidate and by the final comparison runs. BEST → this merge (night-01…07, 10, 11).

### Night-14: person lookup packs once and hashes once: **KEEP**

Packed tables (`PMap`) are keyed by `fmix(pack9(id))` (murmur3's 64-bit finalizer, a bijection, so keys stay exact)
with an identity hasher; link partitions take bits 40.. of the same value. `Names::person(id)` packs the ID once,
mixes once, probes one partition table, and on a miss returns `Person { name: id, key }` without packing again
(before: up to three packs and two hashes per lookup). Tests: `fmix` collision-free on a sample, and spread of
partition bits, low bits and hashbrown's top-7 tag bits.

CI (`a649871` vs `a768775`, before night-11 was merged; Xeon 6973P-C): gate PASS; **20M 4.706 → 4.550 s (−3.3%, 6/7)**;
**60M 15.090 → 14.415 s (−4.5%, 5/5)**. Merged on top of night-11 (local checks green, 20M Parquet equal).
BEST → this merge (night-01…07, 10, 11, 14).
