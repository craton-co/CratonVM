#!/usr/bin/env bash
# hibfix-dupins-loop.sh <tag> <binary> <runs> — the duplicated-INSERT repro.
#
# Each run is a WHOLE experiment: reset, run, then count what landed. The row
# count is the primary signal, not the exit code — a run that inserts 60/1440
# rows can still report rc=0.
#
# Table SIZE is an experimental variable: the run inserts into a live index,
# and an empty table does NOT reproduce the failure at all. The filler rows
# carry negative ids so they can never collide with the sequence.
# DUP_NOTRUNC=1 leaves whatever is already there.
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
HERE="C:/craton/CratonVM/apps/hibernate-reactive-suite-runner"
TAG="$1"; BIN="$2"; RUNS="${3:-10}"
THREADS="${DUP_THREADS:-24}"; N="${DUP_N:-60}"
EXPECT=$((THREADS * N))
cd "$HERE" || exit 1
mkdir -p runs/dupins
pass=0; missing=0; dup=0; hr90=0; shortloop=0
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
  # ArrayLoop's own terminal state. A loop that ran every index has
  # applied == end; applied < end on a multi-index loop means the loop was cut
  # short WITHOUT the consumer ever being told.
  ends=$(grep -ao 'HIBFIX-END applied=[0-9]* current=[0-9]* end=[0-9]*' "$L" 2>/dev/null \
    | awk -F'[= ]' '$7 > 1 && $3 != $7' | sort | uniq -c | sort -rn | head -3 | tr '\n' ';')
  [ "$ok" -gt 0 ] && pass=$((pass+1))
  [ "$rows" != "$EXPECT" ] && missing=$((missing+1))
  [ "$d" -gt 0 ] && dup=$((dup+1))
  [ "$h" -gt 0 ] && hr90=$((hr90+1))
  [ -n "$ends" ] && shortloop=$((shortloop+1))
  echo "  run $i: rows=$rows/$EXPECT ok=$ok dup=$d hr90=$h iters=[${short:-ok}] cut=[${ends:-none}]"
done
echo "TAG=$TAG pass=$pass/$RUNS missing_rows=$missing dup=$dup hr90=$hr90 short_loops=$shortloop"
