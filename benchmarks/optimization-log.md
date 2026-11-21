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

## Register

| ID | Change | Tier | Commit | Correctness | 20M warm (s9 / s11) | 20M cold | 60M warm | 60M cold | anon peak 60M | Decision |
|---|---|---|---|---|---:|---:|---:|---:|---:|---|
| P0 | Scored baseline | — | `34a18e7` | dev MATCH | 2.814 / 2.752 | 4.57 / 3.71 | 11.298 | 15.15 | 8.2 GB | Preserve |
| P0i | + stage timers | — | `5d40e99` | all gates | 2.796 / 2.751 | 3.60 / 3.67 | 11.468 | 14.95 | 8.2 GB | Reference |
| A1 | Single-probe dedup | A | see git log | all gates | 2.795 / 2.730 | 3.87 / 3.67 | 13.092 vs 13.317 (paired) | 17.93 | 8.2 GB | **Keep** |
| A2 | Drop `line_fields` | A | | | | | | | | next |
