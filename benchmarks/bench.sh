#!/bin/sh
# Runs INSIDE the Linux Docker host (colima VM now, the x86 timing VM later).
#
#   bench.sh time IMAGE DATAVOL LABEL WARM_RUNS
#       1 host-cold run (page cache dropped) + WARM_RUNS warm runs, each a fresh container
#       with the official starter's scorer flags. Prints wall seconds per run, measured in
#       the VM around `docker run` (container create -> exit).
#   bench.sh warm IMAGE DATAVOL LABEL N
#       N warm runs, no cache drop (interleave A/B on a noisy host).
#   bench.sh mem IMAGE DATAVOL LABEL
#       one warm run, sampling cgroup memory.peak and peak `anon` (every 20 ms); kept
#       separate so the sampling never affects timed runs.
#
# EXTRA_ARGS (env) is appended to `docker run`, e.g. EXTRA_ARGS="-e SWEEP_NB=256" for sweeps.
# Output volumes are fresh for every run; the phase log of the last run is in /tmp/bench-LABEL.log.
set -u
MODE=$1 IMG=$2 VOL=$3 LABEL=$4 N=${5:-5}
FLAGS="--network none --read-only --tmpfs /tmp:rw,size=64m,mode=1777 --cap-drop ALL \
 --security-opt no-new-privileges --pids-limit 4096 --cpus 8 --memory 16g --memory-swap 16g \
 --user 1000:1000 -e DATA_DIR=/data -e OUTPUT_DIR=/output -e SCRATCH_DIR=/scratch \
 -e HOME=/scratch -e NCPU=8 -e MEM_GB=16"
fresh() {  # $1 = output volume name
  docker volume rm -f "$1" scr >/dev/null 2>&1
  docker volume create "$1" >/dev/null; docker volume create scr >/dev/null
  docker run --rm -v "$1":/o -v scr:/s debian:bookworm-slim chown 1000:1000 /o /s
}
drop() { sync; echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null; }
now() { date +%s.%N; }

if [ "$MODE" = time ] || [ "$MODE" = warm ]; then
  # `warm`: N warm runs only, no cache drop; used to interleave A/B/A/B on a noisy host.
  i=0; [ "$MODE" = warm ] && i=1
  while [ $i -le "$N" ]; do
    o="o-$LABEL"; fresh "$o"
    kind=warm; [ $i -eq 0 ] && { drop; kind=cold; }
    t0=$(now)
    docker run --rm $FLAGS ${EXTRA_ARGS:-} -v "$VOL":/data:ro -v "$o":/output -v scr:/scratch "$IMG" \
      >/dev/null 2>"/tmp/bench-$LABEL.log"
    rc=$?; t1=$(now)
    echo "$LABEL $kind run$i exit=$rc wall=$(echo "$t1 - $t0" | bc)"
    i=$((i + 1))
  done
  docker volume rm -f "o-$LABEL" scr >/dev/null 2>&1
elif [ "$MODE" = mem ]; then
  o="o-$LABEL"; fresh "$o"
  docker run --rm $FLAGS ${EXTRA_ARGS:-} -v "$VOL":/data:ro -v "$o":/output -v scr:/scratch --entrypoint sh "$IMG" -c '
    /app/shopping-sessions >/dev/null 2>&1 & p=$!; amax=0
    while kill -0 $p 2>/dev/null; do
      a=$(sed -n "s/^anon //p" /sys/fs/cgroup/memory.stat); [ "$a" -gt "$amax" ] && amax=$a
      sleep 0.02
    done
    wait $p; echo "exit=$? memory.peak=$(( $(cat /sys/fs/cgroup/memory.peak) / 1048576 ))MiB anon_peak=$(( amax / 1048576 ))MiB"'
  docker volume rm -f "$o" scr >/dev/null 2>&1
fi
