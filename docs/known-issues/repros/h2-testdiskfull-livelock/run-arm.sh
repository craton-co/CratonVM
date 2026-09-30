#!/bin/bash
# run-arm.sh <cratonvm-binary|HOTSPOT> <tag> <nruns> <parallel> <timeout_s>
#
#   H2, JAVA_HOME_25, OV   as for build-overlay.sh
#   OUTROOT                where per-arm dirs land (default /tmp)
#   DFULL_MAX / DFULL_START / DFULL_WRITE_DELAY   see README.md
#   EXTRA                  extra VM args, e.g. --nojit (CratonVM) or -Xint (HotSpot)
: "${H2:?set H2}"; : "${JAVA_HOME_25:?set JAVA_HOME_25}"
OV="${OV:-/tmp/dfull-ov}"
BIN="$1"; TAG="$2"; N="${3:-10}"; P="${4:-4}"; TMO="${5:-300}"
CP="$OV/out:$H2/target/classes:$H2/target/test-classes:$(cat "$H2/craton-testcp.txt")"
OUT="${OUTROOT:-/tmp}/dfull-$TAG"
mkdir -p "$OUT"

run_one() {
  i="$1"
  d=$(mktemp -d "${OUTROOT:-/tmp}/dfullcwd-XXXXXX")   # a fresh CWD per run, as the suite runner gives
  log="$OUT/run-$i.log"
  t0=$(date +%s)
  if [ "$BIN" = HOTSPOT ]; then
    ( cd "$d" && timeout -s KILL "$TMO" "$JAVA_HOME_25/bin/java" -Xmx1g $EXTRA \
        -cp "$CP" org.h2.test.synth.TestDiskFull ) > "$log" 2>&1
  else
    ( cd "$d" && timeout -s KILL "$TMO" "$BIN" --java-home "$JAVA_HOME_25" --Xmx 1g $EXTRA \
        -c "$CP" org.h2.test.synth.TestDiskFull ) > "$log" 2>&1
  fi
  rc=$?; t1=$(date +%s)
  cls=pass
  case $rc in 137|124) cls=timeout ;; 139) cls=segv ;; esac
  if [ $rc -ne 0 ] && [ "$cls" = pass ]; then
    if   grep -q 'ClassCastException' "$log"; then cls=cce
    elif grep -q 'Chunk .* not found'  "$log"; then cls=chunk
    else cls="fail$rc"; fi
  fi
  grep -q 'A fatal error has been detected' "$log" && cls=abort
  echo "run=$i rc=$rc cls=$cls secs=$((t1-t0))" \
       "iters=$(grep -c '^\[dfull\] iter=' "$log")" \
       "spin=$(grep -c 'cvm-spin' "$log")" \
       "leftover=$(grep -c 'cvm-leftover\]' "$log")" \
       "wasfalse=$(grep -c 'wasActive=false' "$log")" \
       "oob=$(grep -c 'out-of-bounds field' "$log")" >> "$OUT/results.txt"
  rm -rf "$d"
}
export -f run_one
export OUT BIN CP TMO OV H2 JAVA_HOME_25 EXTRA OUTROOT
: > "$OUT/results.txt"
seq 1 "$N" | xargs -P "$P" -I{} bash -c 'run_one {}'

echo "=== $TAG (DFULL_MAX=${DFULL_MAX:-natural} DFULL_START=${DFULL_START:-0} WRITE_DELAY=${DFULL_WRITE_DELAY:-10}) ==="
awk '{for(i=1;i<=NF;i++){split($i,a,"=");v[a[1]]=a[2]}
      c[v["cls"]]++; s+=(v["spin"]>0); lo+=v["leftover"]; wf+=v["wasfalse"]; ob+=v["oob"]}
     END{for(k in c) print k, c[k];
         print "runs", NR, "| wedged", s, "| leftovers", lo, "| unapplied-committed", wf, "| oob-guard-hits", ob}' \
    "$OUT/results.txt"
