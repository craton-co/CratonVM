#!/usr/bin/env bash
# hibfix-seqtime.sh <binary> <max runs> — is the id-generator CAS storm the
# CAUSE or an EFFECT?
#
# Both series live in MySQL's own general log, so no cross-log clock
# correlation is needed: bucket INSERTs and Entity_SEQ UPDATEs per second and
# read which one turns first. If the retry storm starts BEFORE the inserts
# collapse it is the cause; if after, the search moves upstream of the id
# generator.
#
# Does NOT truncate after a failing run -- the first attempt at this lost its
# data to a control run that did.
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
HERE="C:/craton/CratonVM/apps/hibernate-reactive-suite-runner"
BIN="$1"; MAX="${2:-40}"
THREADS="${DUP_THREADS:-24}"; N="${DUP_N:-60}"
EXPECT=$((THREADS * N))
cd "$HERE" || exit 1
mkdir -p runs/seqtime
Q() { docker exec mysql mysql -uhreact -phreact hreact -e "$1" 2>/dev/null; }
Q "set global log_output='TABLE'; set global general_log='ON';" >/dev/null
for i in $(seq 1 "$MAX"); do
  Q "truncate table mysql.general_log;" >/dev/null 2>&1
  L="runs/seqtime/run-$i.log"
  ./hibfix-mtins-run.sh "seqtime-$i" "$BIN" hibfix-common-mysql-ext.args \
    -Dmti.threads="$THREADS" -Dmti.n="$N" \
    -- org.hibernate.reactive.MultithreadedInsertionWithLazyConnectionTest \
    testIdentityGeneratorWithTransaction >/dev/null 2>&1
  cp "runs/mtins/seqtime-$i.log" "$L" 2>/dev/null
  rows=$(Q "select count(*) from Entity;" | tail -1 | tr -d '\r')
  echo "run $i: rows=${rows:-?}/$EXPECT"
  [ "${rows:-0}" = "$EXPECT" ] && continue
  echo "=== FAILING RUN $i — per-second timeline ==="
  echo "  sec = tenths of a second since the run's first logged statement"
  Q "select floor(timestampdiff(microsecond,
              (select min(event_time) from mysql.general_log), event_time)/100000) ds,
            sum(argument like '%insert into Entity%') inserts,
            sum(argument like 'update Entity_SEQ%') seq_updates,
            sum(argument like '%Entity_SEQ%' and argument not like 'update%') seq_selects,
            sum(argument='COMMIT') commits
       from mysql.general_log
       group by 1 having inserts+seq_updates+seq_selects+commits > 0 order by 1;"
  echo "--- general_log LEFT INTACT for run $i; app log: $L ---"
  echo "--- app-side first errors (seconds column is the app's own) ---"
  grep -aoE "^[0-9]+ - [^:]*: (FIRSTERR [^ ]*|java\.lang\.IllegalStateException: HR[0-9]*)" "$L" | head -6
  exit 0
done
Q "set global general_log='OFF';" >/dev/null
echo "no failing run in $MAX attempts"
