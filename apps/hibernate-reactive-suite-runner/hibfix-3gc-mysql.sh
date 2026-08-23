#!/usr/bin/env bash
# hibfix-3gc-mysql.sh — the full hibernate-reactive testlist on the fixed
# binary, three GC arms in parallel, MySQL in Docker (one Testcontainers
# container per class, which is what gives per-class schema isolation).
set -uo pipefail
HERE="C:/craton/CratonVM/apps/hibernate-reactive-suite-runner"
BIN="${BIN:-C:/craton/CratonVM-hibfix-20260822/target/release/cratonvm-hibfix-3gc.exe}"
STAMP="${STAMP:-20260822-3gc-mysql}"
SHARDS="${SHARDS:-3}"
TIMEOUT="${TIMEOUT:-240}"
pids=()
for gc in default g1 generational; do
  (
    HR_COMMON="$HERE/hibfix-common-mysql.args" \
    "$HERE/hibfix-hr-suite.sh" \
      --list "$HERE/testlist.txt" --gc "$gc" --shards "$SHARDS" \
      --timeout "$TIMEOUT" --bin "$BIN" \
      --out "$HERE/runs/mysql-$gc-$STAMP" \
      > "$HERE/runs/arm-$gc-$STAMP.log" 2>&1
    echo "ARM $gc DONE rc=$?"
  ) &
  pids+=($!)
done
for p in "${pids[@]}"; do wait "$p"; done
echo "ALL ARMS DONE"
