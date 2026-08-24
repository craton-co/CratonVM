#!/usr/bin/env bash
# hibfix-seqrace.sh <binary> <max runs> — the id generator's CAS loop, as the
# server saw it.
#
# TableReactiveIdentifierGenerator does: select next_val; update Entity_SEQ set
# next_val=NEW where next_val=OLD; rowCount==0 means someone else won, retry.
# Only ONE update per OLD value can ever affect a row. If two racers both
# proceed, they take the same id block -- which is what duplicate ids on the
# 50-apart block boundaries would mean.
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
HERE="C:/craton/CratonVM/apps/hibernate-reactive-suite-runner"
BIN="$1"; MAX="${2:-40}"
THREADS="${DUP_THREADS:-24}"; N="${DUP_N:-60}"
EXPECT=$((THREADS * N))
cd "$HERE" || exit 1
mkdir -p runs/seqrace
Q() { docker exec mysql mysql -uhreact -phreact hreact -e "$1" 2>/dev/null; }
cleanup() { Q "set global general_log='OFF';" >/dev/null 2>&1; }
trap cleanup EXIT
Q "set global log_output='TABLE'; set global general_log='ON';" >/dev/null
for i in $(seq 1 "$MAX"); do
  Q "truncate table mysql.general_log;" >/dev/null 2>&1
  L="runs/seqrace/run-$i.log"
  ./hibfix-mtins-run.sh "seqrace-$i" "$BIN" hibfix-common-mysql-ext.args \
    -Dmti.threads="$THREADS" -Dmti.n="$N" \
    -- org.hibernate.reactive.MultithreadedInsertionWithLazyConnectionTest \
    testIdentityGeneratorWithTransaction >/dev/null 2>&1
  cp "runs/mtins/seqrace-$i.log" "$L" 2>/dev/null
  rows=$(Q "select count(*) from Entity;" | tail -1 | tr -d '\r')
  echo "run $i: rows=${rows:-?}/$EXPECT"
  [ "${rows:-0}" = "$EXPECT" ] && continue
  echo "=== FAILING RUN $i — the id-generator CAS as the server saw it ==="
  echo "--- SEQ statement mix ---"
  Q "select case when argument like 'update Entity_SEQ%' then 'UPDATE'
                 when argument like '%Entity_SEQ%' then 'SELECT' else 'x' end k, count(*) n
       from mysql.general_log where argument like '%Entity_SEQ%' group by 1;"
  echo "--- CAS updates repeated for the SAME old value (only one can win) ---"
  Q "select argument, count(*) n from mysql.general_log
       where argument like 'update Entity_SEQ%' group by 1 having n > 1 order by n desc limit 10;"
  echo "--- distinct UPDATEs vs total UPDATEs ---"
  Q "select count(*) total_updates, count(distinct argument) distinct_updates
       from mysql.general_log where argument like 'update Entity_SEQ%';"
  echo "--- duplicate ids reported by the app ---"
  grep -ao \"Duplicate entry '[0-9]*'\" "$L" | sort -u | head -10
  echo "--- log: $L ---"
  exit 0
done
echo "no failing run in $MAX attempts"
