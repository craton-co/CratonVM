#!/usr/bin/env bash
# hibfix-seqcheck.sh <binary> <max runs> — run until a FAILING run, then read
# each verticle's sequence numbers straight out of the `name` column.
#
# `storeEntity` sets name = <thread>__<localVerticleOperationSequence>, so the
# rows ARE the per-verticle sequence log. A repeated sequence means the
# downstream event fired twice for one index — the thing this test's javadoc
# says it exists to catch. A gap means an index was skipped. Neither means the
# count is simply where the verticle died.
#
# The query has to run BEFORE the next run, because HBM2DDL_AUTO=create drops
# the table every time.
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
HERE="C:/craton/CratonVM/apps/hibernate-reactive-suite-runner"
BIN="$1"; MAX="${2:-15}"
THREADS="${DUP_THREADS:-24}"; N="${DUP_N:-60}"
EXPECT=$((THREADS * N))
cd "$HERE" || exit 1
mkdir -p runs/seqcheck
Q() { docker exec mysql mysql -uhreact -phreact hreact -e "$1" 2>/dev/null; }
for i in $(seq 1 "$MAX"); do
  L="runs/seqcheck/run-$i.log"
  ./hibfix-mtins-run.sh "seq-$i" "$BIN" hibfix-common-mysql-ext.args \
    -Dmti.threads="$THREADS" -Dmti.n="$N" \
    -- org.hibernate.reactive.MultithreadedInsertionWithLazyConnectionTest \
    testIdentityGeneratorWithTransaction >/dev/null 2>&1
  cp "runs/mtins/seq-$i.log" "$L" 2>/dev/null
  rows=$(Q "select count(*) from Entity;" | tail -1 | tr -d '\r')
  echo "run $i: rows=${rows:-?}/$EXPECT"
  [ "${rows:-0}" = "$EXPECT" ] && continue

  echo "=== FAILING RUN $i — sequence analysis ==="
  echo "--- totals: rows vs DISTINCT names (a gap here IS a repeat) ---"
  Q "select count(*) rows_total, count(distinct name) distinct_names from Entity;"
  echo "--- REPEATED (thread, seq) pairs ---"
  Q "select substring_index(name,'__',1) thr, substring_index(name,'__',-1) seq, count(*) c
       from Entity group by 1,2 having c > 1 order by c desc, thr limit 20;"
  echo "--- per-thread: rows landed, seq range, and whether 0..max is complete ---"
  Q "select substring_index(name,'__',1) thr,
            count(*) landed,
            min(cast(substring_index(name,'__',-1) as signed)) lo,
            max(cast(substring_index(name,'__',-1) as signed)) hi,
            max(cast(substring_index(name,'__',-1) as signed)) + 1 - count(*) missing
       from Entity group by 1 order by landed limit 30;"
  echo "--- ITERS reported by each verticle, for comparison ---"
  grep -ao 'ITERS [0-9]*' "$L" | sort -n -k2 | uniq -c
  echo "--- log: $L ---"
  exit 0
done
echo "no failing run in $MAX attempts"
