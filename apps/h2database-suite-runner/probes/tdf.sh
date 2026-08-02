#!/bin/bash
# tdf.sh <label> <binary> <runs>
# TestDiskFull short form: a fresh empty CWD makes it a ~2 s run that fails at a
# measurable rate with the same ClassId(0) signature as
# TestMVStoreCachePerformance. See
# docs/known-issues/h2/bug-h2-testdiskfull-classid0-corruption-segv-cce.md
set -u
LABEL="$1"; BIN="$2"; RUNS="${3:-60}"
H2=/data/data/h2database/h2
CP="$H2/target/classes:$H2/target/test-classes:$(cat $H2/craton-testcp.txt)"
OUT=/data/data/h2cid0-runs/tdf-$LABEL
mkdir -p "$OUT"
: > "$OUT/summary.txt"
pass=0; chunk=0; timeoutn=0; segv=0; cce=0; other=0; guard=0
for i in $(seq 1 "$RUNS"); do
  d=$(mktemp -d -p /data/tmp tdf.XXXXXX)
  ( cd "$d" && timeout 90 env TMPDIR=/data/tmp "$BIN" --java-home /home/victor/jdk25 \
      --Xmx 1g -c "$CP" org.h2.test.synth.TestDiskFull > "$d/out.log" 2>&1 )
  rc=$?
  tag=other
  if grep -q 'cratonvm::gc::guard' "$d/out.log"; then
    guard=$((guard+1)); cp "$d/out.log" "$OUT/guard-$i.log"
  fi
  if [ $rc -eq 0 ]; then tag=pass; pass=$((pass+1))
  elif [ $rc -eq 139 ]; then tag=segv; segv=$((segv+1)); cp "$d/out.log" "$OUT/segv-$i.log"
  elif [ $rc -eq 124 ]; then tag=timeout; timeoutn=$((timeoutn+1))
  elif grep -q 'ClassCastException' "$d/out.log"; then tag=cce; cce=$((cce+1)); cp "$d/out.log" "$OUT/cce-$i.log"
  elif grep -q 'not found' "$d/out.log"; then tag=chunk; chunk=$((chunk+1))
  else other=$((other+1)); cp "$d/out.log" "$OUT/other-$i.log"
  fi
  echo "$i rc=$rc $tag" >> "$OUT/summary.txt"
  rm -rf "$d"
done
echo "LABEL=$LABEL runs=$RUNS pass=$pass chunk=$chunk timeout=$timeoutn segv=$segv cce=$cce other=$other guard=$guard" >> "$OUT/summary.txt"
echo "LABEL=$LABEL runs=$RUNS pass=$pass chunk=$chunk timeout=$timeoutn segv=$segv cce=$cce other=$other guard=$guard"
