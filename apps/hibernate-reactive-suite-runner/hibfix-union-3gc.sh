#!/usr/bin/env bash
# Re-run the 28-class non-PASS union on each GC arm at the SAME low concurrency
# the HotSpot control used (shards 2, one arm at a time), so the two are
# comparable. The 9-way full sweep saturated Docker and produced
# "Could not find a valid Docker environment" failures that are not VM results.
set -uo pipefail
HERE="C:/craton/CratonVM/apps/hibernate-reactive-suite-runner"
BIN="${BIN:-C:/craton/CratonVM-hibfix-20260822/target/release/cratonvm-hibfix-3gc.exe}"
for gc in default g1 generational; do
  HR_COMMON="$HERE/hibfix-common-mysql.args" \
  "$HERE/hibfix-hr-suite.sh" --list "$HERE/hibfix-nonpass-union.txt" \
    --gc "$gc" --shards 2 --timeout 300 --bin "$BIN" \
    --out "$HERE/runs/mysql-union-$gc" > "$HERE/runs/union-$gc.log" 2>&1
  echo "UNION ARM $gc DONE"
done
echo "UNION ALL DONE"
