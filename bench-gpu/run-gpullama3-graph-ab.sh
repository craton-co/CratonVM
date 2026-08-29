#!/usr/bin/env bash
# Three-arm A/B for the CUDA-graph decode path, one binary, one jar.
#
#   base   the app as it was: per-token scalars passed as kernel arguments
#   sc     the app with those scalars device-resident, graph OFF
#   graph  the same, graph ON
#
# base-vs-sc prices moving two ints into device memory; sc-vs-graph
# prices the graph itself. Arms alternate within each round and the
# MINIMUM per arm is reported, because a slow round is contention and
# contention only ever adds.
#
#   ROUNDS=5 N=64 bash bench-gpu/run-gpullama3-graph-ab.sh
set -u
APP=${APP:-/c/craton/CratonVM/apps/GPULlama3.java}
CV=${CV:?set CV to the cratonvm.exe under test}
ROUNDS=${ROUNDS:-5}
N=${N:-64}
PROMPT=${PROMPT:-"Why is the sky blue?"}
OUT=${OUT:-/tmp/graph-ab}
mkdir -p "$OUT"
cd "$APP" || exit 1

run_arm() {                       # run_arm <name> <classes> <extra>
  CLASSES="$2" CRATON_EXTRA="$3" timeout 900 \
    bash run-craton-gpu-cp.sh "$CV" -p "$PROMPT" -n "$N" 2>&1
}

for r in $(seq 1 "$ROUNDS"); do
  for arm in base sc graph; do
    case $arm in
      base)  cls=classes-craton-base; ex="" ;;
      sc)    cls=classes-craton;      ex="-Dllama.craton.graph=false" ;;
      graph) cls=classes-craton;      ex="" ;;
    esac
    echo "-- round $r arm $arm"
    run_arm "$arm" "$cls" "$ex" > "$OUT/$arm-$r.txt" 2>&1
    grep -h "achieved tok" "$OUT/$arm-$r.txt" | tail -1
  done
done

echo
echo "arm      best tok/s   all rounds"
for arm in base sc graph; do
  vals=$(grep -h "achieved tok" "$OUT"/$arm-*.txt | sed 's/.*: *//;s/\..*//;s/,.*//')
  # decimal comma on this host; compare on the raw string via sort -g
  raw=$(grep -h "achieved tok" "$OUT"/$arm-*.txt | sed 's/.*tok\/s: *//;s/\. .*//' | tr ',' '.')
  best=$(echo "$raw" | sort -g | tail -1)
  printf "%-8s %10s   %s\n" "$arm" "$best" "$(echo "$raw" | tr '\n' ' ')"
done
