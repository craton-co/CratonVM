#!/usr/bin/env bash
# Paired A/B driver for the DefaultCatalogAndSchemaTest crash rate.
#
# This class crashes intermittently at roughly 1 run in 3 on an UNCHANGED
# binary, so a small unpaired sample invents differences that are not there —
# it already produced one false "3/3 vs 0/3 regression" this session, on which
# a real fix was reverted. Two rules follow, and this script exists to enforce
# them:
#
#   * INTERLEAVE. One A run, then one B run, then repeat. Batching all of A
#     before all of B lets a change in background load land on one arm.
#   * Enough rounds. Telling ~0% from ~33% needs about 8 per arm; 3 does not.
#
#   A=<exe> B=<exe> ROUNDS=8 bash probes/hib-paired-ab-20260801.sh
set -uo pipefail
A="${A:?set A to the first cratonvm.exe}"
B="${B:?set B to the second cratonvm.exe}"
ROUNDS="${ROUNDS:-8}"
TAG_A="${TAG_A:-armA}"
TAG_B="${TAG_B:-armB}"
HERE="$(cd "$(dirname "$0")" && pwd)"
declare -a rc_a rc_b
for i in $(seq 1 "$ROUNDS"); do
  for arm in A B; do
    if [ "$arm" = A ]; then cv="$A"; tag="$TAG_A"; else cv="$B"; tag="$TAG_B"; fi
    CV="$cv" TAG="${tag}-p${i}" bash "$HERE/hib-mapresize-repro-20260731.sh" >/dev/null 2>&1
    rc=$?
    if [ "$arm" = A ]; then rc_a+=("$rc"); else rc_b+=("$rc"); fi
    echo "pair $i  $arm($tag) rc=$rc"
  done
done
crash() { local n=0; for r in "$@"; do [ "$r" != 0 ] && n=$((n+1)); done; echo "$n"; }
echo "=== $TAG_A: $(crash "${rc_a[@]}")/$ROUNDS crashed  [${rc_a[*]}] ==="
echo "=== $TAG_B: $(crash "${rc_b[@]}")/$ROUNDS crashed  [${rc_b[*]}] ==="
