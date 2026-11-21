# Field research: public Shopping Sessions entries (2026-11-21, 03:00 EST)

**Scope and ethics.** 116 public GitHub repos found with `gh search` (repos updated this month that mention
the track), shallow-cloned as read-only data (nothing built or run). This file records **ideas and public
measurements**; it contains no copied code. Every idea we adopt is implemented from scratch and must pass our
own gates (`benchmarks/OPTIMIZATION_PLAN.md`). Direction changed by the user on 2026-11-21: other entries may
now inform ideas (previously "benchmark only").

## Where we stand

| Entry | Practice score | Notes |
|---|---:|---|
| Leader "67" | **0.48 s** | not found in public repos |
| Fraun (`mliao0128/fraun-shopping-sessions`, #7) | 0.64 s | C++20 + Arrow C++; their own WSL 4-core box: 1.27 s at 20M, 4.68 s at 60M |
| Several tuned public Rust entries (self-reported) | 1.0 to 2.3 s | e.g. a team logging 1.62, 1.69, 1.79, 1.88 and 2.17 s across commits |
| **Ours** (`34a18e7`) | **5.79 s** | about 2.5 s on 8 Apple cores; the scorer is about 4 physical Xeon cores |

Inference: the leaders spend about **8 to 10× less CPU per event** than we do. Fraun scored *faster* than on
their own 4-core machine; we scored 2.3× *slower* than on 8 Apple cores. We're CPU-bound, and the scorer
punishes it.

## Techniques the strong entries converge on (how often they're mentioned across 116 repos)

| Idea | What it buys | Seen in |
|---|---|---|
| Hand-written SIMD CSV scan (AVX2 masks for `,` `"` `\n`, tzcnt loop; one pass finds lines *and* fields) | Parse cost about 2–4× lower than memchr per field | most Rust/C++ top entries |
| **Order-preserving packed IDs**: IDs of ≤ 9 ASCII bytes → one u64 whose integer order equals byte order (7 bits/char, or `pext`); exact interning fallback for anything else | Integer hash/compare instead of byte slices; no random reads back into the CSV; 8 B keys | very common |
| **Compact records** (32 B or less; ts as u32 offset from window start, ingest as i32 seconds) | Halves memory traffic in scatter/dedup | common |
| `pread`/`read` into per-thread **reused, L2-sized buffers** (≈ 512 KB) instead of one big `mmap` | Avoids page-fault and TLB cost and munmap teardown (≈ 125 ms/GB); one team measured −50% sys time | several |
| SWAR / SSE timestamp parsing (8 digits per u64 op), with a per-thread cache of the date part | Timestamps were about 25% of single-thread parse time | several |
| Huge-page arenas for big vectors (`MADV_HUGEPAGE` for allocations of 8 MB or more) | Fewer first-touch faults | common |
| Prefetch / batched lookups in dedup and identity resolution | Dedup is latency-bound (random probes into large tables) | several |
| Small staging buffers per destination partition (16 records, then a burst flush) | Scatter writes stay in L1 | one detailed log |
| **Minimal hand-written Parquet writer** (PLAIN, uncompressed) instead of the arrow/parquet crates | Smaller binary, no Arrow overhead; one team reports −5% | many |
| Static binary in an empty (`scratch`) image; `target-cpu` set for Cascade Lake via runtime dispatch | Faster container start; usable SIMD | many |
| Parallel union-find over links (lock-free table) | Our links stage takes about 1 s single-threaded at 60M | a few |
| Heavy persons first (scheduling) | The walk phase idled about 22% at the tail | one |
| PGO (profile-guided optimization) | 5–15% | a few |
| **x86 benchmarking on GitHub Actions runners** (Ice Lake / Zen 3 / Zen 4, 4 vCPU) | Scorer-like x86 timing for free on a public repo | one detailed log |

## Things others found *not* to help (their measurements)

AVX-512 (no gain over AVX2 on the scorer: frequency licensing isn't the gap), a single global hash table vs
partitioning (scored worse: 2.17 vs 1.79 s), `MAP_POPULATE`, bulk populate, oversubscribing threads,
parallel unmap. Our own A3/A4 results agree that bigger partitions and `WILLNEED` don't help.

## Public scorer observations

- One team: "scorer ≈ CI-in-container × 1.3 + ~0.35 s Docker overhead" (their estimate, 4-vCPU CI).
- Scores shift ±10% between identical commits; the organizers attribute this to machine variance.
- Fraun: 20M = 2.25M user-day rows; their 60M RSS 10.4 GB.

## Implications for our roadmap (proposed order; each still goes through the gates)

1. **Packed u64 IDs + compact records** (our B2, extended): the biggest structural change; affects dedup, identity
   and sessions.
2. **Fused SIMD line/field scanner + SWAR timestamps** (parse is 35% of our 60M time).
3. **Per-thread reused read buffers** instead of mmap (A4 showed mmap hints don't help; this goes further).
4. **Parallel links** (≈ 9% at 60M).
5. **Dense campaign/day arrays** (our B3) and a minimal Parquet writer.
6. Then tuning: huge pages, staging buffers, PGO, static `scratch` image.
