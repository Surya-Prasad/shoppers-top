# Databricks — Shopping Sessions Buildathon

> Agent-ready project brief based on the [Builders Buildathon Shopping Sessions track](https://www.joinbuilders.org/buildathon/tracks/shopping-sessions), inspected on 2026-11-20. This is a **navigation and implementation brief**, not a replacement for the full rules. Follow the [organizers' `rules.md`](https://www.joinbuilders.org/buildathon/tracks/shopping-sessions/files/rules.md) for every disputed detail.

## Task for a coding agent

Build a fast, **exact** ETL pipeline that ingests dirty online-shopping CSV data from a read-only mounted directory, resolves multiple user identifiers into people, constructs browsing/purchasing sessions, assigns ad attribution and campaign assists, and writes two daily Parquet datasets. Correctness is a hard gate: an approximately correct output receives no performance score.

A complete submission is a **public GitHub repository** with a root-level `Dockerfile`. The evaluator builds the specified commit, mounts data at runtime, and measures elapsed time from **container startup through process exit** (including engine startup, input parsing, and output writing).

### Source of truth / precedence

1. **Canonical specification:** [rules.md](https://www.joinbuilders.org/buildathon/tracks/shopping-sessions/files/rules.md). This defines the exact CSV dialect, timestamp parsing, data-cleaning order, linked-ID semantics, sessions, attribution, assists, schemas, and input generator.
2. **Track and infrastructure requirements:** [Shopping Sessions track](https://www.joinbuilders.org/buildathon/tracks/shopping-sessions).
3. The starter project is illustrative. **When starter behavior and `rules.md` disagree, follow `rules.md`.**

**Agent directive:** Retrieve and read the entire canonical `rules.md` before implementing or optimizing. Do not infer edge-case semantics from this summary alone. If network access is unavailable, ask for a local copy of `rules.md` instead of silently guessing.

## Input and output contract

The organizer mounts the dataset at `${DATA_DIR}` (normally `/data`). The manifest provides a dataset-dependent time window; **never hard-code its dates**.

| Input path (relative to `/data`) | Purpose |
| --- | --- |
| `events/*.csv` | Shopping events (views, carts, purchases), including event and ingestion timestamps |
| `clicks/*.csv` | Advertisement clicks and campaign IDs |
| `links/*.csv` | Pairs of user IDs belonging to the same person |
| `products.csv` | Product-to-category reference table |
| `campaigns.csv` | Campaign-to-channel reference table |
| `manifest.json` | Window start/end boundaries |

Read **every** CSV part file; part counts and sizes vary and some files contain only headers. Input is read-only.

Generate two Parquet table directories (direct child `*.parquet` files, no unintended columns):

**`${OUTPUT_DIR}/user_daily/`** — one record for each person and UTC day on which that person **began** at least one session:

| Column | Logical type | Meaning |
| --- | --- | --- |
| `user_id` | string | Canonical person ID |
| `day` | Parquet `DATE` | UTC day of session start |
| `sessions` | integer | Number of sessions started that day |
| `events` | integer | Kept events in those sessions |
| `revenue_cents` | integer | Revenue from purchases in those sessions |
| `top_category` | nullable string | Category with the greatest purchase revenue, or null if no purchases |

**`${OUTPUT_DIR}/campaign_daily/`** — one record for each campaign/day with credited **or** assisted sessions:

| Column | Logical type | Meaning |
| --- | --- | --- |
| `campaign_id` | string | Campaign ID or `organic` |
| `channel` | string | Campaign channel or `organic` |
| `day` | Parquet `DATE` | UTC day of session start |
| `sessions` | integer | Sessions credited to campaign |
| `purchasing_sessions` | integer | Credited sessions containing a purchase |
| `revenue_cents` | integer | Revenue of credited sessions |
| `assisted_sessions` | integer | Sessions that the campaign assisted |

Row and file ordering do not matter. Use exactly these field sets. Check the canonical spec for output-file acceptance rules and types.

## Critical correctness checkpoints

These reminders are **not exhaustive**; use the canonical rules for implementation:

- Parse the specified CSV dialect, including strict quote placement, exact untrimmed strings, ASCII-only digit checks, and nonstandard treatment of carriage returns. Drop malformed rows.
- Accept only the specified ISO-8601-with-zone and integer epoch-millisecond timestamp forms. Use UTC millisecond arithmetic and manifest-defined, half-open windows.
- Resolve all valid user-ID links as transitive connected components, including long chains and shared devices. Name each component by its byte-wise smallest ID; an isolated ID names itself.
- For events, **deduplicate by event ID before validating**. Keep the earliest ingested parseable copy even if it later fails validation. Validate purchase amounts and remove events more than 24 hours late.
- Within a person, start a new session only when the event-time gap is **greater** than 30 minutes. Give the entire session the UTC date of its first event, including activity after midnight.
- Attribute each session to the latest eligible click in the preceding seven days (inclusive of boundaries); use smallest byte-wise click ID for timestamp ties. Otherwise attribute it to `organic`.
- Count an assist **once per distinct eligible campaign per session**, including the campaign that won attribution. `organic` receives no assists.
- Obtain categories and channels from reference tables. Preserve case and whitespace in identifiers. Apply the full official rules for unknown products, tied categories, and invalid campaign references.

### Useful reconciliation checks

- Global session totals agree between `user_daily` and `campaign_daily`.
- Global `revenue_cents` totals agree between both outputs.
- Every assisted session must have a qualifying campaign click; assisted-session counts can exceed total sessions because several campaigns may assist one session.
- Campaigns that only assisted on a day still need a row, even with no credited sessions.

## Execution environment

| Constraint | Value |
| --- | --- |
| Platform | `linux/amd64` |
| Maximum Docker image size | 8 GB |
| Docker build | Network allowed, up to 10 minutes |
| Container execution | **No network access** |
| CPU | 8 pinned vCPUs (hyperthreads) |
| Memory | 24 GB RAM; **no swap** |
| Writable spill/storage | `/scratch`, with `/output` |
| Combined output + scratch quota | 50 GB |
| Root filesystem | Read-only; `/tmp` is a 64 MB in-memory exception |
| Run UID | 1000 |
| Process/thread cap | 4,096 |
| Standard output cap | 50 MB; use stderr for logging |

Environment variables supplied by the evaluator:

```bash
DATA_DIR=/data
OUTPUT_DIR=/output
SCRATCH_DIR=/scratch
HOME=/scratch
NCPU=8
MEM_GB=24
```

Avoid downloading packages or models at container runtime. Bundle everything required by the program in the image. Do not use `/data` for writes or rely on unmounted files being present.

## Testing and submission workflow

1. Choose an engine or language: **DuckDB, Polars, PySpark, Rust, C++, Python**, or another viable stack. The organizers do not restrict AI assistance.
2. Download the official starter and review its test harness. The starter writes correctly shaped **empty** outputs; it is **not** a completed solution.
3. Implement the canonical rules, then run the starter's **36 edge-case fixtures**.
4. Use the official Rust generator to create the approximately **1-million-event development dataset** (seed 1).
5. Extract the official development answer and run `make check-dev` according to the starter instructions. Match every row exactly.
6. Profile **end-to-end** container runtime on representative data. Optimize after validation, and rerun correctness checks after every relevant change.
7. Publish a GitHub repository with a root `Dockerfile`; keep the repository **at most 50 MB**, without input or output datasets committed. Submit the repository and commit via the event site.
8. Keep the repository public and all submitted commits reachable until the results are published.

### Official downloads

- [Canonical rules.md](https://www.joinbuilders.org/buildathon/tracks/shopping-sessions/files/rules.md)
- [PySpark starter](https://www.joinbuilders.org/buildathon/tracks/shopping-sessions/files/starter-pyspark.zip)
- [DuckDB starter](https://www.joinbuilders.org/buildathon/tracks/shopping-sessions/files/starter-duckdb.zip)
- [Polars starter](https://www.joinbuilders.org/buildathon/tracks/shopping-sessions/files/starter-polars.zip)
- [Development answer (CSV)](https://www.joinbuilders.org/buildathon/tracks/shopping-sessions/files/dev-answer.zip)
- [Rust dataset generator](https://www.joinbuilders.org/buildathon/tracks/shopping-sessions/files/generator.zip)

## Benchmark and scoring

| Stage | Input size | Limits / scoring |
| --- | --- | --- |
| Local development | ~1 million events | Fixture tests plus official expected results |
| Pre-scoring development check | Public 1M-event dataset | Up to 3 minutes; failures return a difference report and logs |
| Hidden practice | ~20 million events | Time limit: min(2 × baseline, 5 minutes); typically one timed run |
| Practice for fast solutions | Same hidden input | If first runtime is under 20 seconds, 3 runs; median determines score |
| Final | ~60 million fresh events | 3 independent runs, each capped at 3 minutes; median if **all** finish |
| Fastest finalists | Same final benchmark | At least five fastest retimed five times on one idle machine |

As displayed on 2026-11-20:

- **Baseline:** 171.91 seconds (PySpark local mode).
- **Scorer:** `shopping-sessions-4`.
- **Metric:** wall-clock container duration; lower wins.
- **Ties:** earlier submission ranks higher.
- **Submissions:** 2 per team per hour.
- **Practice results:** may vary by roughly 20% across machines.
- **Final risk:** exceeding 3 minutes in **any** final run means no final score; organizers recommend aiming for **under ~60 seconds in practice**.
- **Deadline shown by the page:** Sunday at 11:00 a.m. ET. **Confirm the calendar date and current status on the live event page**; the page does not give an absolute date in the track text.
- **Prize advertised:** $5,000.

A bad output, crash, or violation of runtime/memory/disk/log caps invalidates a run. Hidden data and logs are not disclosed; hidden-run feedback is limited to process status, out-of-memory status, and aggregate row differences.

## Suggested coding-agent acceptance criteria

Treat these as a stop/go checklist, not permission to change the specification:

- [ ] Read complete official `rules.md` and starter instructions.
- [ ] Docker image builds for `linux/amd64` and works with no runtime network.
- [ ] Input paths, output paths, and manifest windows are configurable through the supplied environment.
- [ ] Parser, timestamp, deduplication, linked-identity, session, attribution, assist, and aggregation behavior match the canonical rules.
- [ ] Output directories contain valid Parquet with **exact** columns and a true `DATE` field.
- [ ] All 36 fixtures pass, and `make check-dev` matches the official development answer.
- [ ] Resource caps are respected; repo contains no datasets.
- [ ] Full start-to-exit runtime is measured, not just query execution time.
- [ ] Final git commit and public repository are reproducible.

## Source links

- Track page: <https://www.joinbuilders.org/buildathon/tracks/shopping-sessions>
- Full specification: <https://www.joinbuilders.org/buildathon/tracks/shopping-sessions/files/rules.md>

*The live leaderboard is intentionally omitted because rankings are dynamic and do not affect the pipeline specification.*
