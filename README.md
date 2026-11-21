# Shopping Sessions: Rust engine

An exact, parallel ETL for the Builders Buildathon *Shopping Sessions* track. It reads the
dirty CSVs in `$DATA_DIR`, resolves linked IDs into people, builds sessions, attributes
and assists campaigns, and writes `user_daily/` and `campaign_daily/` Parquet files to
`$OUTPUT_DIR`.

- Semantics: [docs/rules.md](docs/rules.md) (canonical copy)
- Design and guardrails (relational / cow-book): [docs/DESIGN.md](docs/DESIGN.md)
- Every judgement call and measurement: [INFERENCES.md](INFERENCES.md)

## Layout

| Path | What |
| --- | --- |
| `src/dialect.rs` | the exact CSV dialect (quotes, `\r`, field counts) |
| `src/timefmt.rs` | F1/F2/F3 timestamps, ASCII digits only |
| `src/hashing.rs` | deterministic byte-string hasher |
| `src/main.rs` | links → clicks → events → sessions/attribution, all hash-partitioned |
| `src/output.rs` | Parquet writer (exact schema, atomic rename) |
| `reference/reference.py` | independent, naive Python oracle (test only) |
| `tests/fuzz.py` | differential fuzzer: Rust vs oracle on adversarial datasets |
| `tests/invariants.py` | keys / domains / reconciliation checks, any scale |
| `fixtures/`, `scripts/` | the official 36 fixtures and harness |

## Run

```sh
make build fixtures                # amd64 image, 36 fixtures with scorer flags
make dev run-dev check-dev         # 1M dev set in the container vs dev-answer/
make all-checks                    # native: unit, fixtures, dev, invariants, fuzz
```

Dev tooling: Rust ≥ 1.88, Docker with buildx, and a Python venv with `polars` and `pyarrow`
(`uv venv .venv && uv pip install --python .venv/bin/python polars pyarrow`).
`tools/generate` is the official generator built from `generator.zip`. It is not committed.
