#!/usr/bin/env bash
# CPU-time A/B for the loader-latch step, one loader state per process.
#
# Reports USER CPU SECONDS, not elapsed: this host runs 2-3x oversubscribed, so
# elapsed time swamps a 1.5x effect while CPU time measures work done.
#
# Each cell is  cpu(mode, ROUNDS) - cpu(mode, 0), so VM startup and the cost of
# defining the classes cancel out and what is left is ROUNDS parse passes under
# that loader state. Without the subtraction "three" would look expensive purely
# because it defines 3x156 classes before parsing anything.
#
# Arms alternate per measurement and the order flips each repeat, so neither arm
# sits on one side of a drifting host.
set -u
cd /data/data/latch-probe

JDK=/data/jdk25-real-20260717/jdk-25.0.3+9
CTL=./cratonvm-latch-ctl-20260804
FIX=./cratonvm-latch-fix-20260804
REPS=${1:-2}
ROUNDS=${2:-8}
MODE_FLAG=${3:---nojit}
MODES="none empty one all two three"

cpu() {   # user CPU seconds for one process
  local exe=$1 mode=$2 rounds=$3
  /usr/bin/time -f '%U' $exe --java-home "$JDK" $MODE_FLAG -Xmx2g \
      -cp out:tomcat-bcel.jar LoaderStepOneShotProbe lib "$mode" "$rounds" \
      2>&1 >/dev/null | tail -1
}

net() {   # parse-only CPU for ROUNDS passes, setup and startup subtracted
  local exe=$1 mode=$2
  local full base
  full=$(cpu "$exe" "$mode" "$ROUNDS")
  base=$(cpu "$exe" "$mode" 0)
  awk -v a="$full" -v b="$base" 'BEGIN{printf "%.2f", a-b}'
}

echo "ROUNDS=$ROUNDS  flag=$MODE_FLAG  (user CPU s, setup+startup subtracted)"
printf '%-7s' "mode"
for r in $(seq 1 "$REPS"); do printf '%8s%8s' "CTL$r" "FIX$r"; done
echo
for mode in $MODES; do
  printf '%-7s' "$mode"
  for r in $(seq 1 "$REPS"); do
    if [ $((r % 2)) -eq 1 ]; then
      c=$(net "$CTL" "$mode"); f=$(net "$FIX" "$mode")
    else
      f=$(net "$FIX" "$mode"); c=$(net "$CTL" "$mode")
    fi
    printf '%8s%8s' "$c" "$f"
  done
  echo
done
