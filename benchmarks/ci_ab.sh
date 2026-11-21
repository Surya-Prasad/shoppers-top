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
python3 -m pip install -q polars pyarrow >/dev/null
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
  ( cd "$d" && cargo build --release --locked -q ) >&2
  cp "$d/target/release/shopping-sessions" "$W/bin-$1"; echo "$W/bin-$1"
}
BA=$(build "$BASE"); BB=$(build "$CAND")

# ---- data ----
"$GEN" --seed 1 --events 1000000 --clicks 250000 --output "$W/d1" >/dev/null
"$GEN" --seed 11 --events "$EV20" --clicks $((EV20 / 4)) --output "$W/d20" >/dev/null
"$GEN" --seed 11 --events "$EV60" --clicks $((EV60 / 4)) --output "$W/d60" >/dev/null
log "data ready: $(du -sh "$W/d20" | cut -f1) / $(du -sh "$W/d60" | cut -f1); free disk $(df -h "$W" | awk 'NR==2{print $4}')"

run() { # bin data out -> prints "wall maxrss_kb rc"
  rm -rf "$3"
  /usr/bin/time -f '%e %M' -o "$W/time.txt" env DATA_DIR="$2" OUTPUT_DIR="$3" NCPU="$NPROC" "$1" >/dev/null 2>"$3.log" && rc=0 || rc=$?
  echo "$(cat "$W/time.txt" | tail -1) $rc"
}

# ---- correctness gate ----
GATE=pass; NOTES=()
( cd "$REPO" && cargo test --release --locked -q >/dev/null 2>&1 ) || { GATE=fail; NOTES+=("unit tests failed"); }
fx=$(python3 scripts/run_fixtures.py fixtures -- "$BB" 2>&1 | tail -1) || true
[[ "$fx" == "36/36 fixtures passed" ]] || { GATE=fail; NOTES+=("fixtures: $fx"); }
read -r _ _ rc < <(run "$BB" "$W/d1" "$W/o-dev")
dev=$(python3 scripts/compare.py "$W/o-dev" "$ANS" 2>&1 | tail -1) || true
[[ "$dev" == "MATCH" && "$rc" == 0 ]] || { GATE=fail; NOTES+=("dev answer: $dev (exit $rc)"); }
for d in d20 d60; do
  read -r _ _ ra < <(run "$BA" "$W/$d" "$W/oa-$d"); read -r _ _ rb < <(run "$BB" "$W/$d" "$W/ob-$d")
  eq=$(python3 scripts/compare.py "$W/oa-$d" "$W/ob-$d" 2>&1 | tail -1) || true
  [[ "$eq" == "MATCH" && "$ra" == 0 && "$rb" == 0 ]] || { GATE=fail; NOTES+=("$d A vs B: $eq (exits $ra/$rb)"); }
  rm -rf "$W/oa-$d" "$W/ob-$d"
done
log "gate: $GATE ${NOTES[*]:-}"

# ---- timing: interleaved warm pairs (the gate runs above warmed the page cache) ----
: > "$W/times.tsv"
if [ "$GATE" = pass ]; then
  for d in d20 d60; do
    n=$PAIRS20; [ $d = d60 ] && n=$PAIRS60
    for i in $(seq 1 "$n"); do
      for side in A B; do
        bin=$BA; [ $side = B ] && bin=$BB
        read -r wall rss rc < <(run "$bin" "$W/$d" "$W/o-t")
        printf '%s\t%s\t%s\t%s\t%s\n' "$d" "$side" "$wall" "$rss" "$rc" >> "$W/times.tsv"
      done
    done
  done
fi
grep -h '^\[' "$W/o-t.log" 2>/dev/null | tail -12 > "$W/phases.txt" || true
python3 benchmarks/ab_summary.py "$W/times.tsv" "$BASE" "$CAND" "$GATE" "$W/phases.txt" \
  "$(lscpu | sed -n 's/^Model name: *//p') x $NPROC" "${NOTES[*]:-}" "$T/result.md" "$T/result.json"
cat "$T/result.md"
