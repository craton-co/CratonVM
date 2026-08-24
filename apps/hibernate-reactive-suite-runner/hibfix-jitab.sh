#!/usr/bin/env bash
# hibfix-jitab.sh <binary> <pairs> — JIT vs --nojit, INTERLEAVED.
#
# The failure has a continuous measure, not just pass/fail: `rows` out of 1440.
# A severe run sends only ~134 of ~1093 attempted INSERTs, so the arms separate
# on the ratio long before they separate on a pass count.
#
# Interleaved because the host is shared and the failure rate is not stable
# across time; running one arm as a block measures the hour, not the change.
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
HERE="C:/craton/CratonVM/apps/hibernate-reactive-suite-runner"
BIN="$1"; PAIRS="${2:-8}"
THREADS="${DUP_THREADS:-24}"; N="${DUP_N:-60}"
EXPECT=$((THREADS * N))
cd "$HERE" || exit 1
mkdir -p runs/jitab
run_one() { # <tag> <extra vm args...>
  local tag="$1"; shift
  ./hibfix-mtins-run.sh "$tag" "$BIN" hibfix-common-mysql-ext.args "$@" \
    -Dmti.threads="$THREADS" -Dmti.n="$N" \
    -- org.hibernate.reactive.MultithreadedInsertionWithLazyConnectionTest \
    testIdentityGeneratorWithTransaction 2>&1 | grep -oE 'wall_ms=[0-9]+'
  cp "runs/mtins/$tag.log" "runs/jitab/$tag.log" 2>/dev/null
  docker exec mysql mysql -N -uhreact -phreact hreact \
    -e "select count(*) from Entity;" 2>/dev/null | tr -d '\r'
}
jf=0; nf=0
for p in $(seq 1 "$PAIRS"); do
  for arm in jit nojit; do
    tag="$arm-$p"
    if [ "$arm" = nojit ]; then out=$(run_one "$tag" --nojit); else out=$(run_one "$tag"); fi
    wall=$(echo "$out" | grep -o '[0-9]*' | head -1)
    rows=$(echo "$out" | tail -1)
    bad=""; [ "${rows:-0}" != "$EXPECT" ] && { bad=" LOST=$((EXPECT-${rows:-0}))"; \
      [ "$arm" = jit ] && jf=$((jf+1)) || nf=$((nf+1)); }
    echo "  pair $p $arm: rows=${rows:-?}/$EXPECT wall_ms=${wall:-?}$bad"
  done
done
echo "TOTAL jit_failures=$jf/$PAIRS  nojit_failures=$nf/$PAIRS"
