#!/usr/bin/env bash
# CI A/B driver (x86 GitHub runner). Candidate = HEAD; baseline = $BASE_INPUT or benchmarks/BEST.
# Gate: candidate unit tests, 36 fixtures, dev answer, and identical Parquet output to the baseline at
# 20M and 60M. Timing: interleaved warm pairs (A,B,A,B...) on 20M and 60M after one warm-up each.
# Writes $RUNNER_TEMP/result.md and result.json. Exit code is 0 even when the gate fails (the result
# file says so); it is non-zero only if the harness itself breaks.
set -euo pipefail
T=${RUNNER_TEMP:-/tmp}; W=$T/ab; mkdir -p "$W"
REPO=$(pwd)
CAND=$(git rev-parse HEAD)
BASE_REF=${BASE_INPUT:-}; [ -n "$BASE_REF" ] || BASE_REF=$(tr -d ' \n' < benchmarks/BEST)
BASE=$(git rev-parse "$BASE_REF^{commit}")
NPROC=$(nproc)
PAIRS20=${PAIRS20:-7}; PAIRS60=${PAIRS60:-5}
# SMOKE=1: tiny sizes and 2 pairs, to test the harness itself (labels stay d20/d60).
if [ "${SMOKE:-0}" = 1 ]; then EV20=400000; EV60=800000; PAIRS20=2; PAIRS60=2; else EV20=20000000; EV60=60000000; fi
log() { echo "[$(date +%T)] $*" >&2; }
log "candidate $CAND  base $BASE  cpus $NPROC  $(lscpu | sed -n 's/^Model name: *//p')"

# ---- toolchain + official tools (cached by nothing; ~2 min) ----
rustup default stable >/dev/null 2>&1 || true
[ -x /usr/bin/time ] || { sudo apt-get install -y -qq time >/dev/null 2>&1 || apt-get install -y -qq time >/dev/null 2>&1; }
# own virtualenv: system pip is refused on Debian/Ubuntu (PEP 668)
python3 -m venv "$W/venv" && "$W/venv/bin/pip" install -q polars pyarrow >/dev/null
PY="$W/venv/bin/python"
B=https://www.joinbuilders.org/buildathon/tracks/shopping-sessions/files
( cd "$W" && curl -fsSL -o gen.zip $B/generator.zip && curl -fsSL -o ans.zip $B/dev-answer.zip \
  && unzip -qo gen.zip -d gen && unzip -qo ans.zip -d ans )
GEN_DIR=$(dirname "$(find "$W/gen" -name Cargo.toml | head -1)")
( cd "$GEN_DIR" && cargo build --release -q )
GEN="$GEN_DIR/target/release/generate"
ANS=$(dirname "$(find "$W/ans" -name user_daily.csv | head -1)")

# ---- build both engines ----
build() { # sha -> binary path
  local d="$W/src-$1"; rm -rf "$d"; git worktree add -q --detach "$d" "$1"
  # v3h: a tree with a PGO profile builds with it, as its Dockerfile does (x86-64)
  local rf=; [ -f "$d/pgo/merged.profdata" ] && [ "$(uname -m)" = x86_64 ] && rf="-C target-cpu=x86-64-v3 -C profile-use=$d/pgo/merged.profdata"
  ( cd "$d" && if [ -n "$rf" ]; then RUSTFLAGS="$rf" cargo build --release --locked -q; else cargo build --release --locked -q; fi ) >&2
  cp "$d/target/release/shopping-sessions" "$W/bin-$1"; echo "$W/bin-$1"
}
BA=$(build "$BASE"); BB=$(build "$CAND")

# ---- data ----
"$GEN" --seed 1 --events 1000000 --clicks 250000 --output "$W/d1" >/dev/null
log "memory: $(free -g | awk '/^Mem:/{print $2" GB total, "$7" GB available"}')"

run() { # bin data out -> prints "wall maxrss_kb rc"
  rm -rf "$3"
  local t0 t1; t0=$(date +%s%N)
  /usr/bin/time -f '%M' -o "$W/time.txt" env DATA_DIR="$2" OUTPUT_DIR="$3" NCPU="$NPROC" "$1" >/dev/null 2>"$3.log" && rc=0 || rc=$?
  t1=$(date +%s%N)
  # wall in seconds with ns resolution (time(1) only gives 10 ms); max RSS (KB) from time(1)
  echo "$(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.4f", (b-a)/1e9}') $(tail -1 "$W/time.txt") $rc"
}

# ---- correctness gate (unit tests, fixtures, dev answer) ----
GATE=pass; NOTES=()
( cd "$REPO" && cargo test --release --locked -q >/dev/null 2>&1 ) || { GATE=fail; NOTES+=("unit tests failed"); }
fx=$("$PY" scripts/run_fixtures.py fixtures -- "$BB" 2>&1 | tail -1) || true
[[ "$fx" == "36/36 fixtures passed" ]] || { GATE=fail; NOTES+=("fixtures: $fx"); }
read -r _ _ rc < <(run "$BB" "$W/d1" "$W/o-dev")
dev=$("$PY" scripts/compare.py "$W/o-dev" "$ANS" 2>&1 | tail -1) || true
[[ "$dev" == "MATCH" && "$rc" == 0 ]] || { GATE=fail; NOTES+=("dev answer: $dev (exit $rc)"); }

# ---- per dataset: generate, compare A vs B, timed warm pairs, delete ----
# One dataset on disk at a time, so it stays in the page cache (runs became I/O-bound with
# both the 20M and 60M sets present on runners with less free memory).
: > "$W/times.tsv"
for d in d20 d60; do
  ev=$EV20; n=$PAIRS20; [ $d = d60 ] && { ev=$EV60; n=$PAIRS60; }
  # The 20M set goes to RAM-backed /dev/shm when it fits (runners no longer keep the input in
  # the page cache between runs); the 60M set (5.3 GB + ~11 GB process) would not fit in RAM.
  if [ $d = d20 ] && [ -d /dev/shm ] && [ "$(df -Pk /dev/shm | awk 'NR==2{print $4}')" -gt 4000000 ]; then
    mkdir -p /dev/shm/ab && rm -rf "$W/$d" && ln -s /dev/shm/ab/$d "$W/$d"
    "$GEN" --seed 11 --events "$ev" --clicks $((ev / 4)) --output /dev/shm/ab/$d >/dev/null
    echo "$d input: /dev/shm (tmpfs)" >> "$W/where.txt"
  else
    "$GEN" --seed 11 --events "$ev" --clicks $((ev / 4)) --output "$W/$d" >/dev/null
    echo "$d input: disk ($W)" >> "$W/where.txt"
  fi
  log "$d ready: $(du -sh "$W/$d" | cut -f1); $(free -g | awk '/^Mem:/{print $7" GB available"}')"
  read -r _ _ ra < <(run "$BA" "$W/$d" "$W/oa-$d"); read -r _ _ rb < <(run "$BB" "$W/$d" "$W/ob-$d")
  eq=$("$PY" scripts/compare.py "$W/oa-$d" "$W/ob-$d" 2>&1 | tail -1) || true
  [[ "$eq" == "MATCH" && "$ra" == 0 && "$rb" == 0 ]] || { GATE=fail; NOTES+=("$d A vs B: $eq (exits $ra/$rb)"); }
  rm -rf "$W/oa-$d" "$W/ob-$d"
  if [ "$GATE" = pass ]; then
    for i in $(seq 1 "$n"); do
      for side in A B; do
        bin=$BA; [ $side = B ] && bin=$BB
        read -r wall rss rc < <(run "$bin" "$W/$d" "$W/o-t")
        printf '%s\t%s\t%s\t%s\t%s\n' "$d" "$side" "$wall" "$rss" "$rc" >> "$W/times.tsv"
      done
    done
  fi
  [ $d = d60 ] || rm -rf "$W/$d" /dev/shm/ab/$d
done
log "gate: $GATE ${NOTES[*]:-}"
grep -hE '^\[|^    ' "$W/o-t.log" 2>/dev/null | tail -30 > "$W/phases.txt" || true
echo "runner memory: $(free -g | awk '/^Mem:/{print $2" GB total"}')" >> "$W/phases.txt"
cat "$W/where.txt" >> "$W/phases.txt" 2>/dev/null || true
"$PY" benchmarks/ab_summary.py "$W/times.tsv" "$BASE" "$CAND" "$GATE" "$W/phases.txt" \
  "$(lscpu | sed -n 's/^Model name: *//p') x $NPROC" "${NOTES[*]:-}" "$T/result.md" "$T/result.json"
cat "$T/result.md"
