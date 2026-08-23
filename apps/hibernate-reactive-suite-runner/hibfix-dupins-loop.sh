#!/usr/bin/env bash
# hibfix-dupins-loop.sh <tag> <binary> <runs> — the cut-loop repro.
#
# Each run is a WHOLE experiment: reset, run, then count what actually landed
# in the DB. The row count is the signal, not the exit code — a run that
# inserts 60 of 1440 rows still reports rc=0.
#
# The reset below is REDUNDANT and is kept only so the count reads against a
# known start: `BaseReactiveTest` sets `HBM2DDL_AUTO=create`, so Hibernate
# drops and recreates the schema on every run anyway. An earlier revision of
# this script claimed the failure needed a large accumulated table; that claim
# is retracted in section 5.2 of the known-issue page. There is no table-size
# variable, and there cannot be one.
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
HERE="C:/craton/CratonVM/apps/hibernate-reactive-suite-runner"
TAG="$1"; BIN="$2"; RUNS="${3:-10}"
THREADS="${DUP_THREADS:-24}"; N="${DUP_N:-60}"
EXPECT=$((THREADS * N))
cd "$HERE" || exit 1
mkdir -p runs/dupins
pass=0; missing=0; dup=0; hr90=0
for i in $(seq 1 "$RUNS"); do
  [ -z "${DUP_NOTRUNC:-}" ] && docker exec mysql mysql -uhreact -phreact hreact \
    -e "delete from Entity where id > 0;" 2>/dev/null
  L="runs/dupins/$TAG-$i.log"
  ./hibfix-mtins-run.sh "dupins-$TAG-$i" "$BIN" hibfix-common-mysql-ext.args \
    -Dmti.threads="$THREADS" -Dmti.n="$N" \
    -- org.hibernate.reactive.MultithreadedInsertionWithLazyConnectionTest \
    testIdentityGeneratorWithTransaction >/dev/null 2>&1
  cp "runs/mtins/dupins-$TAG-$i.log" "$L" 2>/dev/null
  rows=$(docker exec mysql mysql -N -uhreact -phreact hreact \
    -e "select count(*) from Entity where id > 0;" 2>/dev/null | tr -d '\r')
  rows="${rows:-?}"
  ok=$(grep -ac 'failed=0' "$L" 2>/dev/null)
  d=$(grep -ac 'Duplicate entry' "$L" 2>/dev/null)
  h=$(grep -ac 'HR000090' "$L" 2>/dev/null)
  # Iterations the loop BODY actually ran, per verticle. A short loop and a
  # lost insert give the same row count; only this separates them.
  short=$(grep -ao 'ITERS [0-9]*' "$L" 2>/dev/null | awk -v n="$N" '$2 != n' | tr '\n' ' ')
  [ "$ok" -gt 0 ] && pass=$((pass+1))
  [ "$rows" != "$EXPECT" ] && missing=$((missing+1))
  [ "$d" -gt 0 ] && dup=$((dup+1))
  [ "$h" -gt 0 ] && hr90=$((hr90+1))
  echo "  run $i: rows=$rows/$EXPECT ok=$ok dup=$d hr90=$h iters=[${short:-ok}]"
done
echo "TAG=$TAG pass=$pass/$RUNS missing_rows=$missing dup=$dup hr90=$hr90"
