#!/usr/bin/env bash
# Paired A/B for ROverlaySystemGcStress.
#
# The probe's outcome under its DEFAULT heap turned out to be load-sensitive on
# a shared host (4/4 fail in one window, 8/8 pass an hour later, with the only
# source change provably inert -- `CRATONVM_DBG_PROMO_SEED` reported zero
# promotion entries, so the code under test never executed). Two rules follow,
# and this script exists to enforce both:
#
#   1. PIN THE HEAP. `--Xmx 256m` is what actually produces promotions; on the
#      default heap this workload promotes nothing at all and cannot express a
#      promotion-related defect.
#   2. INTERLEAVE THE ARMS. Runs alternate A,B,A,B... so both arms see the same
#      background load. Running one arm now and the other later is exactly how
#      the original misattribution happened.
#
#   A=<exe> B=<exe>   the two binaries (required)
#   N=<count>         iterations per arm (default 8)
#   ARGS=<probe args> default "40 24 1 0"
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1

HERE="$(cd "$(dirname "$0")/.." && pwd)"
A="${A:?set A to the first cratonvm.exe}"
B="${B:?set B to the second cratonvm.exe}"
N="${N:-8}"
ARGS="${ARGS:-40 24 1 0}"
XMX="${XMX:-256m}"
BUILD="${BUILD:-$HERE/regression-suite/build}"

detect_jdk() {
  local c
  for c in "C:/Program Files/Eclipse Adoptium"/jdk-25* \
           "C:/Program Files/Java"/jdk-25* \
           "C:/Program Files/Eclipse Adoptium"/jdk-2* \
           "C:/Program Files/Java"/jdk-2*; do
    [ -x "$c/bin/java.exe" ] && { printf '%s' "$c"; return 0; }
  done
  return 1
}
JDK="${JDK:-$(detect_jdk)}"
[ -n "$JDK" ] || { echo "no JDK found" >&2; exit 2; }

a_pass=0; a_fail=0; b_pass=0; b_fail=0
run_one() { # $1=exe $2=label $3=iter
  local log="/tmp/paired-$2-$3.log"
  "$1" --java-home "$JDK" --Xmx "$XMX" -cp "$BUILD" ROverlaySystemGcStress $ARGS >"$log" 2>&1
  local rc=$?
  local what
  if [ $rc -eq 0 ]; then what="PASS"; else
    what=$(grep -m1 -oE "tm size [0-9]*|lhm size [0-9]*|lhs size [0-9]*|ts size [0-9]*|ll size [0-9]*|ClassCastException|OutOfMemory" "$log")
    [ -n "$what" ] || what="rc=$rc (no known signature)"
  fi
  printf '  %s[%d] rc=%d %s\n' "$2" "$3" "$rc" "$what"
  return $rc
}

echo "=== paired A/B  N=$N  Xmx=$XMX  ARGS='$ARGS' ==="
echo "A=$A"
echo "B=$B"
for i in $(seq 1 "$N"); do
  if run_one "$A" A "$i"; then a_pass=$((a_pass+1)); else a_fail=$((a_fail+1)); fi
  if run_one "$B" B "$i"; then b_pass=$((b_pass+1)); else b_fail=$((b_fail+1)); fi
done
echo "--- A: pass=$a_pass fail=$a_fail ---"
echo "--- B: pass=$b_pass fail=$b_fail ---"
