#!/usr/bin/env bash
# hibfix-wtcheck.sh <binary> <max runs> — does the withTransaction WORK lambda
# run at all?
#
# The server log showed 1093 storeEntity calls producing 86 BEGINs, so the
# chain short-circuits before any DB traffic. This narrows WHERE: WT counts
# entries to the outer `(s, entity) -> ...` lambda, PERSIST counts entries to
# the `t -> s.persist(entity)` work lambda that withTransaction is supposed to
# invoke. WT >> PERSIST means the composition completed without running the
# work it was given.
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
HERE="C:/craton/CratonVM/apps/hibernate-reactive-suite-runner"
BIN="$1"; MAX="${2:-30}"
THREADS="${DUP_THREADS:-24}"; N="${DUP_N:-60}"
EXPECT=$((THREADS * N))
cd "$HERE" || exit 1
mkdir -p runs/wtcheck
for i in $(seq 1 "$MAX"); do
  L="runs/wtcheck/run-$i.log"
  ./hibfix-mtins-run.sh "wt-$i" "$BIN" hibfix-common-mysql-ext.args \
    -Dmti.threads="$THREADS" -Dmti.n="$N" \
    -- org.hibernate.reactive.MultithreadedInsertionWithLazyConnectionTest \
    testIdentityGeneratorWithTransaction >/dev/null 2>&1
  cp "runs/mtins/wt-$i.log" "$L" 2>/dev/null
  rows=$(docker exec mysql mysql -N -uhreact -phreact hreact \
    -e "select count(*) from Entity;" 2>/dev/null | tr -d '\r')
  # The counters are process-wide, so the LAST line printed is the final total.
  last=$(grep -ao 'ITERS [0-9]* WT [0-9]* PERSIST [0-9]* pOK [0-9]* pERR [0-9]* wtOK [0-9]* wtERR [0-9]*' "$L" | tail -1)
  echo "run $i: rows=${rows:-?}/$EXPECT  final=[$last]"
  [ "${rows:-0}" -ge "${SEVERE:-$((EXPECT * 6 / 10))}" ] && continue
  echo "=== SEVERE RUN $i ==="
  echo "--- sum of per-verticle ITERS ---"
  grep -ao 'ITERS [0-9]*' "$L" | awk '{s+=$2} END {print "attempted_iters="s+0}'
  echo "--- highest WT / PERSIST seen (process-wide counters) ---"
  grep -ao 'WT [0-9]* PERSIST [0-9]*' "$L" | awk '{if($2>w)w=$2; if($4>p)p=$4} END {print "WT="w" PERSIST="p}'
  echo "--- every verticle line ---"
  grep -ao 'ITERS [0-9]* WT [0-9]* PERSIST [0-9]* pOK [0-9]* pERR [0-9]* wtOK [0-9]* wtERR [0-9]*' "$L"
  echo "--- distinct first-seen errors ---"
  grep -a FIRSTERR "$L" | sed 's/.*FIRSTERR /  /' | sort -u | head -10
  echo "--- log: $L ---"
  exit 0
done
echo "no severe run in $MAX attempts"
