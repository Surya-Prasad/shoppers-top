# Shopping Sessions — Incremental Optimization Plan (v2, for Claude Code)

> **Purpose:** Improve the existing Rust submission in `Surya-Prasad/shoppers-top` **one measured change at a time**, preserving exact correctness. Keep a change only when tests pass and repeatable benchmarks justify it.
>
> **Hard rule:** **Never submit to the leaderboard** (or push anything treated as a submission) without explicit user approval. The quota is 2 submissions per hour.
>
> This is a plan, not a record of work done. None of the changes below have been implemented or benchmarked yet.
>
> **Review note (2026-11-21):** This copy was checked against the live track page, the official starter README / `docker_run.sh`, and `docs/rules.md`. Corrections are marked **[corrected]**; the original is unchanged in `~/Downloads`. Measured results go to `benchmarks/optimization-log.md`.

---

## 0. Sources and baseline

- Repo: <https://github.com/Surya-Prasad/shoppers-top>; scored baseline commit `34a18e7f2d5c8652b3860b34abde3562df248501`.
- Track: <https://www.joinbuilders.org/buildathon/tracks/shopping-sessions>
- **Authoritative contract:** `docs/rules.md` (takes precedence over the brief and starters). Also read `docs/DESIGN.md`, `INFERENCES.md` and `Makefile` before touching code; the source may have changed since this plan was written.
- Official practice score: **5.79 s** (`BB-DB9B9054`). PySpark baseline: ~172 s.

### Official scoring (verified on the track page)

| Item | Rule |
|---|---|
| Practice leaderboard | Hidden practice data, **~20M events**, run in a fresh container, timed start → exit. |
| Repeats | Runs under 20 s are timed **3 times, scored on the median**. |
| Final | **~60M events**, **3-minute cap per run**. Over the cap means no final score. Score is the median of the runs. |
| Final retime **[corrected: verified]** | "At least the 10 fastest entries are then retimed 7 times alone on one idle machine, and that median replaces their first score" (live page; the older brief said 5 × 5). |
| Practice time limit **[added]** | min(2 × baseline, 5 min); repeats share the first run's limit. |
| Deadline **[added]** | Final cutoff **Sun 2026-11-22, 11:00 AM EST** (live page). Leave time for the gates before any submit. |
| Submission | You provide a Dockerfile; organizers build it, check it on the dev set, time it on hidden data. |
| I/O | Input at `/data` (read-only); Parquet output to `/output/user_daily/` and `/output/campaign_daily/`. |

**[corrected] Published (live track page + official starter):**
- Image must be **linux/amd64**, ≤ 8 GB; the build has **network for ≤ 10 min**; runs have **no network**.
- **8 pinned vCPUs (hyperthreads), 24 GB memory, no swap**; ≤ 4,096 processes and threads; stdout ≤ 50 MB (log to stderr).
- `/data` read-only; `/scratch` writable disk for spills; `/output` + `/scratch` ≤ 50 GB; root FS read-only except **`/tmp` (64 MB, in memory)**; **uid 1000**.
- Env: `DATA_DIR=/data OUTPUT_DIR=/output SCRATCH_DIR=/scratch HOME=/scratch NCPU=8 MEM_GB=24`.
- Starter README: scorers are **Google Cloud n2 Intel Xeon, Cascade Lake or newer, AVX-512 available**. Starter `scripts/docker_run.sh` says it uses "the same flags as the scorer box": `--network none --read-only --tmpfs /tmp:rw,size=64m,mode=1777 --cap-drop ALL --security-opt no-new-privileges --pids-limit 4096 --cpus 8 --memory 24g --memory-swap 24g --user 1000:1000`.
- Repo ≤ 50 MB, public, every submitted commit kept reachable.

**Still not published:** exact CPU generation, disk type, page-cache state between runs.

The engine writes nothing outside `$OUTPUT_DIR` (P0 re-checks this). Its only temporary files are dot-prefixed `.part-*.tmp` files inside `$OUTPUT_DIR`, renamed atomically.

### Current implementation (summary; confirm in source)

1. `mmap` CSVs, scan in 4 MiB chunks (`CHUNK = 4 << 20`) with Rayon.
2. Union-find over links; canonical person = byte-wise minimum raw ID.
3. Clicks: partition on `click_id` → dedup → validate → repartition on person.
4. Events: partition on `event_id` → keep min `ingest_ts` → **then** validate / late-filter → repartition on person.
5. `NB = 512` hash partitions; scatter → `transpose` → per-partition dedup → `dst` repartition.
6. Std `HashMap` with in-house seeded hasher (`src/hashing.rs`), separate `PART_SEED` and `TABLE_SEED`.
7. Inside each session partition, persons are **already interned to dense `u32`** (`pid` / `pname`).
8. Sessionize: sort events `(person, ts)`, clicks `(person, ts asc, click_id desc)`; sliding 7-day window; distinct active campaigns tracked in O(1) via `cnt` / `pos`; per session, every active campaign updates `camp.entry((campaign, day))` (~104M assist pairs at 60M).
9. Parallel Parquet write for both tables.

### Known historical numbers (Apple silicon, native, warm; NOT predictive of the scorer)

| Input | Wall | links | clicks | events | sessions | write |
|---|---|---|---|---|---|---|
| 20M | 2.05 s | 0.14 | 0.23 | 1.12 | 0.32 | 0.22 |
| 60M | 6.6–8.8 s | 0.6 | 0.8–1.2 | 3.5–5.2 | 1.1 | 0.5 |

Events dominate. The memory figures in the old notes disagree (15.6 GB native RSS vs ~8.3 GB container cgroup), so **re-baseline memory in P0** with the metric defined in §2. **[added]** The likely reason: macOS `ru_maxrss` counts the memory-mapped input pages (~5.3 GB at 60M) as resident, while cgroup `memory.peak` counts page cache differently. Use the cgroup metric.

---

## 1. Non-negotiable correctness contract

Reject any optimization that breaks one of these, whatever its speed.

- **CSV dialect:** only `\n` ends a line; `\r` is data; quotes wrap only a whole field; no commas/quotes/newlines inside values; no trimming or case-folding; skip **only the first line of each file**.
- **Event dedup before validation.** The smallest `ingest_ts` copy wins; a later valid copy never rescues an invalid earliest copy. `rules.md`: copies with equal `ingest_ts` are identical.
- Click duplicates are identical. Valid `click.ts` ∈ `[window_start − 7d, window_end)`; event `ts` ∈ `[window_start, window_end)`.
- **Late** only if `ingest_ts − ts > 24h`. Exactly 24h is allowed, and so is negative latency.
- Linked IDs are resolved transitively; canonical name = **lexicographically smallest raw bytes** (never hash or interning order).
- **Session gap** `> 30 min` splits; `== 30 min` does not; equal timestamps never split. A session counts on its **start** UTC date.
- **Attribution window** inclusive both ends: `start − 7d ≤ click.ts ≤ start`. Winner = latest `ts`, then smallest byte-wise `click_id`.
- **Assists:** one per distinct campaign with ≥1 qualifying click, including the winner's campaign. `organic` never assists.
- **Top category:** max purchase revenue across the person's start-day sessions; ties → byte-wise smallest category; null when there are no purchases; `unknown` competes normally.
- **Output:** exact columns and types; `day` is Parquet `DATE`; campaign rows include assist-only campaigns; totals reconcile across both tables.
- **Don't assume** file counts, dataset size, user distribution, day window length (derive it from data/manifest), ID formats or core count.
- **Container:** read-only input, no runtime network, UID 1000, atomic output writes, error path **exits** (keep `panic = "unwind"`; `abort` previously hung PID 1 for 11+ minutes).
- **Hashing:** byte-level equality is always the final check. No lossy keys, case folding or non-injective numeric parsing of IDs. Remember the historical `ahash::hash_one` → constant `0` disaster: every hasher change needs a spread test.
- **Don't port another contestant's code.** Comparator timings may guide profiling only.

---

## 2. Benchmark environment and metrics

### Environment

- **Timing host:** native **x86-64 Intel Xeon Linux VM, 8 vCPU, 32 GiB RAM**. **[corrected]** Prefer **GCP `n2-standard-8`**: the same family as the scorer (n2, Cascade Lake/Ice Lake). AWS `m6i.2xlarge` (Ice Lake) is a reasonable second choice. **The user provisions it**; the agent works over SSH.
- **[added] Interim host, until that VM exists:** the colima Linux VM on the Mac (arm64, 8 vCPU, 18 GB), with data on **VM-local Docker volumes** (never the macOS shared folder, which penalizes mmap about 2×). Valid for **relative A/B decisions only**; absolute numbers are not scorer-predictive, and anything ISA-specific must be re-checked on x86.
- **Run exactly like the scorer:** build the submission Dockerfile on the VM; every timed run is a **fresh container**:

  ```bash
  docker run --rm --cpus 8 --memory 24g --memory-swap 24g --network none \
    --read-only --tmpfs /tmp:rw,size=64m,mode=1777 --cap-drop ALL \
    --security-opt no-new-privileges --pids-limit 4096 --user 1000:1000 \
    -v /vm-local/data:/data:ro -v /vm-local/out:/output -v /vm-local/scratch:/scratch \
    -e DATA_DIR=/data -e OUTPUT_DIR=/output -e SCRATCH_DIR=/scratch -e HOME=/scratch \
    -e NCPU=8 -e MEM_GB=24 <image>
  ```

  **[corrected]** These are the starter `docker_run.sh` flags (the official "same flags as the scorer box"). The original command lacked `/scratch`, the env vars, `--cap-drop`, `no-new-privileges` and `--pids-limit`.
- Data and output live on the **VM's own filesystem**. Keep the same volume type for all candidates.
- **Mac:** development and correctness only. **Never** time on emulated amd64 or through macOS shared folders.
- `perf stat` / `perf record` run on the **host** against the release binary, outside Docker.

### Cache protocol (measure both for every candidate)

The scorer guarantees a fresh container, **not** a cold page cache. Its 3-run median probably lands on a warm run, so warm matters at least as much as cold.

- **Cold run:** `sync; echo 3 | sudo tee /proc/sys/vm/drop_caches`, then one run. (This clears Linux caches only, not EBS/device caching. Label results "host-cold".)
- **Warm runs:** ≥5 consecutive fresh-container runs immediately after.
- For close calls, alternate A/B/A/B runs.

### Metrics and decision weights

| Measurement | Role |
|---|---|
| 20M warm median (≥5) + host-cold | **Primary** dev / practice-leaderboard metric |
| 60M warm median (≥3–5) + host-cold | **Final-score metric**; a clear 60M regression blocks a change even if 20M improves |
| 60M peak memory | Hard constraint; stay well under 24 GB |
| Correctness / determinism | Mandatory gate |

- **Memory metric (use everywhere):** cgroup `memory.peak`, plus `anon` from `memory.stat`. Also record max RSS.
- Report median, min, max. A 1–3% single-run difference is noise.
- Use ≥2 generator seeds (seed 11 + one other) for 20M and 60M; add a third if a change affects skew.

---

## 3. Decision policy

- **Tier A:** semantics-preserving simplifications or cheap tuning. **May be kept when performance is neutral** (within noise) if all tests pass and the code gets simpler or smaller.
- **Tier B:** likely wins that depend on the workload. Keep only with a measurable improvement on 20M **and** no meaningful 60M regression.
- **Tier C:** algorithm or execution-shape changes. Only after a profile shows the bottleneck; demand stronger correctness evidence (whales, ties, boundaries).

Loop for every step: **baseline → one change → correctness gate → performance gate → log → keep/revert → next.** Never bundle experiments.

---

## 4. Roadmap (in order)

### P0 — Reproducible baseline + initial profile (required first)

- Tag the scored commit (`git tag scored-baseline 34a18e7`); work on branches/worktrees.
- Inspect the `Makefile`; run the documented checks (`make all-checks` or its parts): 36 fixtures, dev answer, fuzz/oracle, invariants.
- Check that nothing writes to `/tmp` or outside `/output` (grep for `temp_dir`, `/tmp`, `TMPDIR`; run under the docker flags above).
- On the VM: generate 1M/dev, 20M and 60M for seed 11 plus one other seed on local disk.
- Record: commit, toolchain, `RUSTFLAGS`, image digest, CPU (`lscpu`), cgroup limits, dataset seeds and sizes.
- Measure per §2: wall time, phase timings, `memory.peak`/anon, cold and warm.
- **Profile now:** `perf stat` (cycles, instructions, cache-misses, page-faults) + `perf record` hot spots on 20M and 60M. Split the events phase into scan/parse vs dedup vs repartition if the logs lump them together. Log the slowest partitions (row count, duration).
- **Output:** `benchmarks/optimization-log.md` with the baseline table and written definitions of "host-cold" and "warm".

### A1 — Single-probe event dedup (Tier A)

`src/main.rs`, `events()` per-partition dedup: replace `get_mut` + `insert` with `HashMap::entry`:

```rust
use std::collections::hash_map::Entry;
match best.entry(r.eid) {
    Entry::Vacant(slot) => { slot.insert(*r); }
    Entry::Occupied(mut slot) => {
        if earlier(r, slot.get()) { slot.insert(*r); }
    }
}
```

Dedup ordering is unchanged. Expect a small gain; keeping it on neutral performance is allowed (Tier A).
**Tests:** all suites; earliest-ingest-invalid copy; equal ingest; duplicates across input chunks; 1-vs-8 thread equality.

### A2 — Drop `EvRaw.line_fields` (Tier A, spec-backed)

`rules.md` guarantees equal-ingest copies are identical, so the raw-line tie-break in `earlier()` is redundant on valid input. Compare only `(ingest_bad, ingest)`; the minimum-ingest choice is unchanged.

- Determinism: the winner becomes the first copy seen in its partition. This should be thread-count independent because `transpose` preserves source-chunk order. **Prove it** with a 1-vs-8 thread test on data containing equal-ingest duplicates.
- Measure `size_of::<EvRaw>()` before and after (old notes guess 56–64 B, down from 80 B).
- If a fuzz case fails **only** because it feeds out-of-spec non-identical equal-ingest copies, restrict that case to in-spec input and document the relaxation. **Never silently disable tests.**

### A3 — `NB` / `CHUNK` sweep (Tier A, never tuned)

- `NB` ∈ {128, 256, 512, 1024} × `CHUNK` ∈ {1, 4, 16} MiB, 20M warm first; re-run the top 2–3 on 60M and host-cold.
- `NB` must stay a power of two. Don't overfit to one seed.

### A4 — Prefetch hint experiment (Tier A/B)

Compare the current `madvise(SEQUENTIAL)` with adding `MADV_WILLNEED` (bounded or whole-file). Judge on **host-cold and warm**, 20M and 60M; keep only if it helps cold without hurting warm.

### Profiling checkpoint

Re-run `perf` on the accepted code. Rank the remaining phases; use this to confirm or reorder the B items.

### B1 — Contiguous partitioning + fused emission (Tier B, top priority)

Target: `Vec<Vec<Vec<EvRaw>>>` / `ClickRaw` scatter, `transpose`, and the second `Vec<Vec<EvV>>` / `ClickV` repartition (512 destination vectors per dedup bucket; `dst` preallocates `n/NB + 8` even when sparse).

Prototype separately and compare:
1. Worker-local reusable bucket buffers, flushed into large contiguous arrays.
2. Histogram → exclusive prefix sum → scatter into flat buffers with offsets.
3. Fused parse → compact record → partition emit in one pass. (The dialect has no quoted delimiters, so `memchr` splitting is already near-ideal; the win is fewer passes and copies.)

**Invariant:** every copy of an ID, and every surviving row of a canonical person, reaches the same bucket. Metrics: events/clicks phase time, wall time, `memory.peak`, LLC misses.

### B3 — Dense campaign/day accumulators (Tier B, high priority)

Replace the per-session `camp.entry((campaign, day))` hash updates (~104M at 60M) with indexed arrays `[(campaign_dense, day_offset)] -> [u64; 4]` plus an organic slot.

- **Worker-local** tables (e.g. Rayon `fold` + `reduce`). Do **not** allocate one per each of the 512 partitions.
- Day count comes from the data/manifest, not hard-coded 14. Campaign count comes from the dense index.
- Emit rows for assist-only campaigns; organic never assists; exactly one assist per campaign per session.
- Compare session-phase time and memory with the current maps.

### B2 — Compact person keys through repartition (Tier B, rescoped)

Persons are already interned to `u32` inside session partitions. What remains is the 16-byte `&[u8]` person slice carried in `EvV`/`ClickV` and re-hashed at interning. Options: carry a precomputed 64-bit hash with the slice, or assign compact IDs earlier **only if** it doesn't need an expensive global dictionary. Canonical naming stays raw-byte minimum. **Never** merge `u1`, `u01`, `U1`, `u1 `. Expect a modest gain; measure the dictionary cost.

### B4 — Cached hashes via `hashbrown::HashTable` (Tier B, only if profiling shows hashing cost)

- Compute a strong 64-bit hash once; use separate bits/seeds for partition choice and in-table probing.
- Use `hashbrown::HashTable` (explicit hash + equality closure). **Do not** use the raw-entry API; it's being removed from hashbrown.
- Pin the version in `Cargo.lock`. **[corrected]** Build-time network is confirmed (≤ 10 min), so vendoring is not needed; `--locked` builds are enough.
- Collisions are always resolved by exact byte equality. Add hash-spread and pathological-ID tests (guards against repeating the `ahash` failure).

### Conditional (only if a profile shows a meaningful remaining bottleneck)

- **B5 — Adaptive per-person sort:** group by person, then sort by `ts` within the group; radix sort or parallel runs for large groups. Keep every distinct campaign for assists even when clicks tie.
- **[flag] C1 and the `pread` part of C3 match techniques seen in the competitor repo we benchmarked** (Vishi99/builders2026: interval-union assists, pread buffers). The standing instruction is that the competitor is a benchmark only. **Do not implement these without the user's explicit go-ahead.**
- **C1 — Interval-based assists:** only if sessions are still a significant share after B3. Coalesce per-(person, campaign) click coverage `[t, t+7d]` and count covered session starts per start-day with prefix sums. The credited click is still chosen separately. The current sliding window is the oracle; test the exact boundaries (click at `start`, click at `start − 7d`, midnights).
- **C2 — Whale parallelism:** only if the slowest partition bounds wall time. Midnight is not a session boundary; a click affects 7 days of sessions.
- **C3 — Global/sharded dedup table, AVX dispatch, `pread` instead of `mmap`:** low priority. Any ISA-specific code needs runtime detection and a portable fallback (`target-cpu=x86-64-v3` previously broke the build).

---

## 5. Per-change procedure

**Before:** re-read the affected source and rules; record `git rev-parse HEAD`; branch from the best verified commit.

**Implement:** one coherent change; add focused unit tests; cite the `rules.md` clause in a comment when relying on an input promise; no unrelated refactors.

**Correctness gate (stop and fix/revert on any failure):**
- `cargo test --release`
- Makefile targets: fixtures (36/36), dev answer, `tests/fuzz.py` strict + adversarial, Python oracle on targeted seeds, `tests/invariants.py`
- **Logical Parquet equality** vs baseline on 20M and 60M for both seeds: same schema, keys, exact values, nulls; compare sorted, ignoring row/file order
- 1-thread vs 8-thread output equality
- Runs cleanly under the §2 docker flags (read-only, UID 1000, no network, 64 MB `/tmp`, no swap); error path exits

**Performance gate:** §2 protocol, same host, data, flags and build profile for baseline and candidate.

**After acceptance:** commit with the optimization name and its benchmark evidence in the message; update the log and the register; the accepted commit becomes the new baseline. Keep a tag on the best verified commit for rollback. Don't multiply speedups; re-measure.

---

## 6. Submission gate

Only after one or more accepted changes, and **only with explicit user approval**:

1. Full scorer-like Docker build and run on the VM; dev-set check passes; valid Parquet.
2. No runtime network use, no non-portable CPU requirements, memory well under 24 GB.
3. Show the user a table: scored baseline 5.79 s; local baseline vs candidate medians (20M and 60M, warm and host-cold); `memory.peak`; tests passed; remaining risks.
4. Wait for approval. Never spend submissions on unmeasured experiments.

---

## 7. Experiment register (fill in as you go)

| ID | Change | Tier | Commit | Correctness | 20M warm | 20M cold | 60M warm | 60M cold | memory.peak (60M) | Decision |
|---|---|---|---|---|---:|---:|---:|---:|---:|---|
| P0 | Scored baseline | — | `34a18e7` | | | | | | | Preserve |
| A1 | Single-probe dedup | A | | | | | | | | |
| A2 | Drop `line_fields` | A | | | | | | | | |
| A3 | NB/CHUNK sweep | A | | | | | | | | |
| A4 | WILLNEED prefetch | A/B | | | | | | | | |
| B1 | Contiguous partitioning + fused emit | B | | | | | | | | |
| B3 | Dense campaign/day arrays | B | | | | | | | | |
| B2 | Compact person keys | B | | | | | | | | |
| B4 | `HashTable` cached hashes | B | | | | | | | | |
| B5/C* | Conditional items | B/C | | | | | | | | |

---

## 8. Research leads (validate against the real profile)

- Leis et al., *Morsel-Driven Parallelism* (SIGMOD 2014): skew-aware work scheduling.
- Balkesen et al., main-memory radix/hash joins: histogram + prefix-sum partitioning.
- Xue & Marcus, *Global Hash Tables Strike Back!* (PVLDB 19(3), 2025): purpose-built concurrent tables can rival partitioning.
- Ellmann & Neumann, *One Pass to Parse Them All* (PVLDB 19(11), 2026): fused parse + process; most of its quoting complexity doesn't apply to this dialect.
- Interval stabbing / prefix-sum counting: theory behind C1.

The objective is end-to-end time with exact correctness, not fashionable techniques.

---

## 9. Agent instructions

> Work in `Surya-Prasad/shoppers-top` from the best verified commit. First read `docs/rules.md`, `docs/DESIGN.md`, `INFERENCES.md`, the `Makefile` and the current source; treat them as authoritative over this plan. Do **P0** completely (VM baseline, cold + warm, 20M + 60M, two seeds, initial `perf` profile, `/tmp` check), then **A1**. After each step, report the patch, test results, medians, `memory.peak` and the keep/revert decision **before** starting the next step. Implement one optimization at a time. Never weaken or skip tests silently, never hard-code generator data shapes, and never port another contestant's code. Call anything unmeasured experimental. **Do not submit to the leaderboard without my explicit approval.**
