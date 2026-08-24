#!/usr/bin/env bash
# hibfix-commitcheck.sh <binary> <max runs> — does the COMMIT actually reach
# the server?
#
# The `name`-column analysis showed 1107 of 1200 inserts vanishing with almost
# no errors. Two hypotheses survive that: the INSERTs never reach MySQL, or
# they reach it inside transactions that are never committed. MySQL's own
# general log distinguishes them without touching the VM or the application.
#
# Restores general_log to its previous setting on exit.
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
HERE="C:/craton/CratonVM/apps/hibernate-reactive-suite-runner"
BIN="$1"; MAX="${2:-20}"
THREADS="${DUP_THREADS:-24}"; N="${DUP_N:-60}"
EXPECT=$((THREADS * N))
cd "$HERE" || exit 1
mkdir -p runs/commitcheck
Q() { docker exec mysql mysql -uhreact -phreact hreact -e "$1" 2>/dev/null; }
cleanup() { Q "set global general_log='OFF';" >/dev/null 2>&1; echo "(general_log restored to OFF)"; }
trap cleanup EXIT
Q "set global log_output='TABLE'; set global general_log='ON';" >/dev/null
for i in $(seq 1 "$MAX"); do
  Q "truncate table mysql.general_log;" >/dev/null 2>&1
  L="runs/commitcheck/run-$i.log"
  ./hibfix-mtins-run.sh "commit-$i" "$BIN" hibfix-common-mysql-ext.args \
    -Dmti.threads="$THREADS" -Dmti.n="$N" \
    -- org.hibernate.reactive.MultithreadedInsertionWithLazyConnectionTest \
    testIdentityGeneratorWithTransaction >/dev/null 2>&1
  cp "runs/mtins/commit-$i.log" "$L" 2>/dev/null
  rows=$(Q "select count(*) from Entity;" | tail -1 | tr -d '\r')
  echo "run $i: rows=${rows:-?}/$EXPECT"
  # Only a SEVERE run is worth the analysis: a run that loses a handful of rows
  # cannot distinguish "never sent" from "sent but not committed" at scale.
  LIM=${SEVERE:-$((EXPECT * 6 / 10))}
  [ "${rows:-0}" -ge "$LIM" ] && continue

  echo "=== FAILING RUN $i — the four-stage accounting ==="
  att=$(grep -ao 'ITERS [0-9]*' "$L" | awk '{s+=$2} END {print s+0}')
  echo "--- attempted (sum of ITERS) = $att ---"
  echo "--- what the SERVER received (no command_type filter: the client uses"
  echo "    prepared statements, so the INSERTs arrive as Execute, not Query) ---"
  Q "select sum(argument like '%insert into Entity%') inserts_executed,
            sum(argument='BEGIN') begins,
            sum(argument='COMMIT') commits,
            sum(argument like 'ROLLBACK%') rollbacks
       from mysql.general_log;"
  echo "--- rows that survived ---"
  Q "select count(*) rows_in_table from Entity;"
  echo "--- per CONNECTION (the pool is shared, so these are not verticles) ---"
  Q "select thread_id,
            sum(argument='BEGIN') begins,
            sum(argument like '%insert into Entity%') inserts,
            sum(argument='COMMIT') commits,
            sum(argument like 'ROLLBACK%') rollbacks
       from mysql.general_log group by 1
       having inserts > 0 or begins > 0 order by inserts desc limit 15;"
  echo "--- ITERS reported ---"
  grep -ao 'ITERS [0-9]*' "$L" | awk '{print $2}' | sort -n | uniq -c | tr '\n' ' '; echo
  echo "--- errors ---"
  echo "  hr90=$(grep -ac 'HR000090' "$L")  dup=$(grep -ac 'Duplicate entry' "$L")"
  echo "--- log: $L ---"
  exit 0
done
echo "no failing run in $MAX attempts"
