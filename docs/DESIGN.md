# Design: Shopping Sessions as a relational pipeline

The guardrails come from Ramakrishnan & Gehrke, *Database Management Systems* (the
"cow book"). Every stage below is stated as a relational operator with its key,
integrity constraints and evaluation strategy, and the implementation follows it.
The source of truth for semantics is [rules.md](rules.md).

## 1. Conceptual model (ER, ch. 2) and relational schema (ch. 3)

| Relation | Key | Notes |
| --- | --- | --- |
| `Product(product_id, category)` | `product_id` | FD `product_id → category`. Reference table. |
| `Campaign(campaign_id, channel)` | `campaign_id` | FD `campaign_id → channel`. Reference table. |
| `Link(user_id_a, user_id_b)` | none (a multiset of edges) | Each edge says two IDs are one person. |
| `Person(user_id → person)` | `user_id` | **Derived**: connected components of `Link`. FD `user_id → person`. |
| `EventCopy(event_id, user_id, ts, ingest_ts, …)` | none | Raw input, duplicates included. |
| `Event(event_id, person, ts, type, category, price)` | `event_id` | After dedup; then a selection. |
| `Click(click_id, person, campaign_id, ts)` | `click_id` | After dedup; `campaign_id` is a FK to `Campaign`. |
| `Session(person, start, …)` | `(person, start)` | Derived by a windowed grouping over `Event`. |
| `user_daily(user_id, day, …)` | `(user_id, day)` | Output. `top_category` nullable. |
| `campaign_daily(campaign_id, day, …)` | `(campaign_id, day)` | Output. FD `campaign_id → channel` holds in it. |

The outputs are each in BCNF with respect to their key, except `campaign_daily`,
which carries `channel` redundantly (FD `campaign_id → channel`, ch. 19). That is
deliberate: the output schema is fixed by the contract. The pipeline derives
`channel` from the single `Campaign` row at write time, so the redundant copy can
never disagree with the source (no update anomaly).

## 2. Integrity constraints, and what happens to violations (ch. 3.2–3.3, 5.7)

The cow book distinguishes declaring a constraint from choosing how violations are
handled. The rules prescribe the handling, which here is always to **reject the
tuple**. Nothing is ever repaired.

| Constraint | Where | Action on violation |
| --- | --- | --- |
| Domain: line must parse under the dialect | all CSVs | drop line |
| Key not null: `event_id`, `click_id` | events, clicks | drop row |
| Primary key: `event_id` unique | events | dedup: keep smallest `ingest_ts` |
| Primary key: `click_id` unique | clicks | dedup: copies identical, keep one |
| Domain: `ts` ∈ F1/F2/F3 and in window | events, clicks | drop row |
| Domain: `event_type` ∈ {view, cart, purchase} | events | drop row |
| Domain: purchase `price_cents` ∈ [1, 10⁷], ASCII digits | events | drop row |
| Temporal: `ingest_ts − ts ≤ 24 h` | events | drop row |
| **Foreign key** `Click.campaign_id → Campaign` | clicks | drop row (reject) |
| **Foreign key** `Event.product_id → Product` | events | *not* rejected: left outer join, default `unknown` |

The two FKs are handled differently on purpose. The rules make the campaign FK
strict, so the click is rejected. The product FK uses a default value, which
corresponds to `LEFT OUTER JOIN … COALESCE(category, 'unknown')`.

## 3. Operator ordering: where algebraic rewrites are *not* allowed (ch. 4, 15)

A query optimizer pushes selections down below other operators. The rules forbid
the obvious push-down here:

```
Event = σ_valid( δ_event_id[min ingest_ts]( EventCopy ) )        -- required
     ≠ δ_event_id[min ingest_ts]( σ_valid( EventCopy ) )        -- WRONG
```

Selection commutes with key-based duplicate elimination only when the predicate
depends on the key alone. `σ_valid` reads non-key attributes (`ts`, `price`, …),
so pushing it below δ would let a later valid copy "rescue" an event whose earliest
copy was invalid (fixture E5, and the mutation test `validate_first`).

The engine still gets most of the benefit of pushing work early. Each row's
predicate *inputs* (parsed `ts`, type code, category, price) are computed during the
scan, in the same pass. The predicate itself is only *applied* after δ, in its legal
place in the plan. The algebra is unchanged.

Allowed rewrites that the engine does use:

* **Selection before the person repartition.** `σ` and the identity map `user_id → person`
  are applied before rows are shuffled by person, so dropped rows are never moved.
* **Projection push-down.** After dedup, only `(person, ts, category, price)` survive;
  the 7-column row is never materialized again.
* **Link resolution as a dimension lookup.** `Person` is computed once (union-find),
  then joined to events and clicks through a hash table with an index nested-loop
  join. Only non-trivial mappings are stored; an ID absent from the table maps to itself.

## 4. Physical plan: hash partitioning (ch. 13–14)

Every key-based operator (dedup, grouping by person) is evaluated by a
**partitioned hash** algorithm, the parallel form of GRACE hash join/aggregation:

```
scan(chunks of 4 MB, parallel)
  └─ partition on h(event_id) into 512 buckets
       └─ per bucket: in-memory hash dedup (min ingest_ts) → σ_valid → π
            └─ partition on h(person) into 512 buckets
                 └─ per bucket: sort (person, ts) → sessionize → attribute → aggregate
```

* Partitioning guarantees that **all copies of one key land in one bucket**, so each
  bucket is processed independently, with no locks and no cross-bucket merging (the
  final `campaign_daily` merge is a tiny aggregate over at most 2,001 × 14 keys).
* The partition hash and the in-bucket table hash use **different seeds**. The keys of
  one bucket share their partition bits, and reusing those bits inside the table would
  cluster the hash table (the cow book's warning about skew in recursive partitioning).
* Sessionization is a **sort-based grouping** (ch. 14.3/14.6). Within a person, sessions
  are contiguous runs in `ts` order, which is exactly what a sort-grouping produces.
* Attribution is a **band join** `Click ⋈ Session ON click.ts ∈ [start − 7d, start]`,
  evaluated as a *merge* of two sorted streams per person, with a sliding window
  (two monotone cursors). Clicks are ordered `(ts ASC, click_id DESC)`, so the last
  element of the window is the credited click: latest `ts`, then smallest `click_id`.
  Assists use a count-per-campaign structure over the same window. Total cost is
  O(events + clicks + assist output) per person, never quadratic.
* Skew: a whale or shared-device person lands in one bucket. Work stealing (rayon)
  keeps the other cores busy while that bucket finishes.

## 5. Access paths and buffer management (ch. 8–9)

* Input files are **memory-mapped read-only**, and the OS page cache is the buffer pool.
  The scan is sequential (`madvise(SEQUENTIAL)`), and values are **zero-copy slices**
  into the mapping: no string is copied until output.
* Memory budget (60M events, measured): about 15.6 GB peak RSS. Roughly 5.3 GB of that
  is clean file-backed pages (reclaimable). The rest is the dedup partitions. This sits
  under the 24 GB cap without spilling, so `/scratch` is unused. A spill path would be
  the next step if inputs grew well beyond 60M.

## 6. Output: atomic, idempotent writes (ch. 16–18, the "A" and "D" in ACID)

Each Parquet file is written to a dot-prefixed temporary name (which the checker
ignores) and `rename(2)`d into place only once complete. A crash mid-write never
leaves a torn file under a name the checker reads. Re-running is idempotent: same
input, same bytes of output, independent of thread count. This was verified by
comparing 1-thread and 8-thread runs at 60M events.

## 7. Testing strategy (N-version programming)

| Layer | What | Result |
| --- | --- | --- |
| Unit | dialect, timestamps, hashing | `cargo test` |
| Official fixtures | 36 hand-written edge cases | 36/36 (native and in the amd64 container) |
| Official dev answer | 1M events, seed 1 | exact match |
| Independent oracle | `reference/reference.py`, a naive line-by-line reading of rules.md | 36/36 fixtures and exact dev match on its own |
| Differential, generator | Rust vs oracle on seeds 2, 3, 4, 5, 6, 42 (1M) and 15 (2M, includes Feb 29 2028) | all match |
| Differential fuzz | `tests/fuzz.py`: 5,400 adversarial mini-datasets | 0 differences |
| Mutation testing | 10 planted bugs (off-by-one gaps, window edges, tie-breaks, quoting, dedup order, offset range, price bound) | 10/10 caught. 3 of them are missed by the official fixtures |
| Integrity at scale | `tests/invariants.py`: keys, domains, reconciliation (60M) | all pass |
| Determinism | 1 thread vs 8 threads, 60M | identical |
| Container | amd64 image under the scorer's `docker run` flags | 36/36 fixtures, dev MATCH; failures exit 101 rather than hang (PID 1 + abort) |
