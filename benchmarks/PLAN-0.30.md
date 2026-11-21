# Action plan: 1.92 s → 0.30 s

Written Sat 2026-11-21 ~14:30 EST. Final cutoff Sun 11:00 AM EST. Current best: 1.92 s (`b9481b11a506d099ebe47963c927c8fe7bbba988`).
Leaderboard top: five teams tied at 0.28 s.

## Sources (public, no opt-out)
- `mwkhettab/shopping-sessions-cpp` BENCHMARKS.md: 1.80 → 0.99 → 0.87 → **0.33 s**. The 0.33 s release: *"partitioned
  event decoding, compact click index, session reconciliation and direct Parquet output"*; batches of 64 rows; event
  field decoding ≈ 510 worker-ms at 20M on 4 vCPUs (≈ 24 ns/line); writing events to partitions 126 worker-ms; the
  whole parse finished in ≈ 0.26 s; PGO and Clang LTO −2 to −4%; runtime ISA dispatch (AVX-512 path + portable path).
- `SSM11011/shopping-sessions` PLAN.md: value shapes from the generator; click dedup is skippable; ts fits u32 ms
  relative to window_start − 7 d.
- `Th3r0n/shopping-sessions` BENCH.md (their own scorer probes): THP works on the scorer, page cache is warm,
  mmap ≈ pread there, process-exit teardown of large mappings is a measurable fixed cost.
- **Excluded:** the top entry's repository asked automated tools not to use it while the event runs (fair-play
  notice). Not used here.

## Where we are vs where 0.30 s needs to be (4-vCPU x86 CI runner, 20M)
| Part | Now (RW32) | Budget for ≈ 0.30 s scorer | How the 0.33 s entry gets there |
|---|---:|---:|---|
| Parse events + clicks + links | ≈ 1.7 s | ≈ 0.25 s | ~24 ns/line: shape-specific decoding of whole batches, no field slices |
| Dedup + regroup | ≈ 0.5 s | ≈ 0.03 s | partition once, compact records, no second full scatter |
| Sessions + attribution | ≈ 0.45 s | ≈ 0.03 s | per-person timelines, compact click index |
| Output | ≈ 0.1 s | overlapped | direct Parquet writer while sessions run |
| Start + exit | ≈ 0.05 s | ≈ 0.02 s | no teardown on the critical path |

The gap is about 8×, and about 80% of it is the parser.

## Phases (each gated: unit + 36 fixtures + dev MATCH + fuzz strict/adversarial + byte-identical output vs
## b9481b1 at 20M and 60M + Docker check; only then an x86 A/B vs the current best)

### P1. Shape-specialized parser (the 80% item), target ≤ 30 ns/line/thread on x86
- Fast path only when a line matches the generator's shapes; **any other line goes to today's exact RW16 path**
  (so exactness never depends on the fast path):
  - IDs `[a-z0-9]{1,9}` with a known prefix letter → base36 to a ≤ 46-bit integer (exact, injective; also handle the
    upper-case variants exactly or send them to the slow path).
  - products `p<digits>` → direct index into a dense array (no hash); campaigns `c_<digits>` likewise.
  - type: dispatch on length + first byte (view / cart / purchase), exact compare.
  - ts: F1/F2/F3 SWAR + month table (have it); ingest F1 only.
  - price: SWAR digits.
- Decode batches of 64 lines from one structural mask pass (AVX-512 when available via runtime dispatch, AVX2
  otherwise), writing the packed record directly, with no `[&[u8]; 7]` arrays and no closures per row.
- Quoted fields (≈20% of fields): strip in the fast path with the branch-free quote rule (RW16).

### P2. Compact records, one partition pass
- Event = one u64 where it fits (u128 otherwise): ts offset (u32 ms), dense person id, price/category packed;
  dedup on a separate (event id, ingest) key.
- Dense person ids: built during link resolution for linked IDs; unlinked IDs map through the base36 integer
  (direct, no hash) when the shape allows it.
- Scatter once, by person, at scan time. Event-id dedup across persons (later copies can name another user) via
  a global singleton filter on event ids, plus an exact table only for the few non-singletons, reconciled before
  sessions.

### P3. Sessions on compact records
- Per person: sort u64 keys (radix by bucket), 30-min-bin sessionization, a compact per-person click index with
  binary search for the 7-day window; assists stamped once per session.

### P4. Direct Parquet writer, overlapped
- Hand-written PLAIN pages (we know the schema), written by idle workers as each person bucket finishes.

### P5. Build and process
- Runtime ISA dispatch (baseline x86-64 + AVX-512/AVX2 paths); PGO with a generated profile at image build;
  `_exit` after the output is durable; unmap input on a helper thread.

## Order and time (≈ 20 h to cutoff)
| # | Work | Expected 20M scorer | Effort |
|---|---|---:|---|
| 1 | P1 parser fast path (events, then clicks/links) | ~0.9–1.1 s | 5–7 h |
| 2 | P2 compact records + single scatter | ~0.6–0.7 s | 3–4 h |
| 3 | P3 sessions | ~0.45–0.55 s | 2–3 h |
| 4 | P4 + P5 | ~0.35–0.45 s | 2–3 h |
| 5 | Scorer-side tuning (threads, huge pages, exit) | → 0.30 s? | remainder |

Honest odds: P1+P2 alone are a strong 2× or better. Reaching 0.30 s needs every phase to land, with no time left
for surprises; plan for about 0.4–0.6 s and treat 0.30 s as the stretch goal.

## Running it in parallel (to fit the time)
- One owner per phase, on separate branches, all measured against b9481b1 with the existing CI A/B, Docker and
  profile workflows (`benchmarks/ci_ab.sh`, `.github/workflows/{bench,docker,profile}.yml`).
- P1 and P4 are independent; P2 depends on P1's record format; P3 on P2.
- Submit after each phase that beats the best in a direct A/B (2 submissions per hour).
