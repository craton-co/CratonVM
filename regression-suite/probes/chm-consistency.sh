#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# chm-consistency.sh — drive ChmConsistencyProbe ONE CASE PER PROCESS.
#
# `H0-8` retracted four "discriminators" of a supposed third ConcurrentHashMap
# defect: all four were artifacts of the order the cases ran in, because
# `jdk_only_enforce_shadow_for` reaches only COLD, step-1 dispatches
# (`H17-2`) — so the first case in a process gets a treatment the rest do not.
#
# This driver removes the confound two ways at once, because either alone can be
# argued with:
#
#   * ONE CASE PER PROCESS. Every case gets a fresh VM, so no case can be the
#     "second" one.
#   * ROTATED REPEATS. The whole set runs ROUNDS times, rotated by one each
#     round, so a result that still depends on position shows up as a case whose
#     answer changes between rounds — and this script reports that as ORDER-
#     DEPENDENT rather than printing one of the two answers.
#
# The unarmed arm is the control and is run too. `H0-8` §4: the unarmed control
# was correct in every one of those runs, so a difference that appears WITHOUT
# arming is a different and more serious finding than one that needs the dial.
#
# Usage: CV=... JDK=... regression-suite/probes/chm-consistency.sh [scope]
#        regression-suite/probes/chm-consistency.sh --selftest
# Env:   CV, JDK, ROUNDS (default 3), SCOPE
# Exit:  0 every case gave one stable answer per arm
#        1 a case is ORDER-DEPENDENT — its answer changed between rounds
#        3 could not run
# ---------------------------------------------------------------------------
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
SCOPE="${1:-${SCOPE:-java/util/concurrent/ConcurrentHashMap}}"
ROUNDS="${ROUNDS:-3}"
CASES="plain withsize withabs intkeys viachm viamap"

# Rotate a space-separated list left by $2 positions. Extracted so --selftest can
# prove the rotation actually rotates: a rotation that returns its input would
# make every round identical and the order check vacuously green.
rotate() {
  # $2 is read BEFORE `set --` replaces the positional parameters. Getting that
  # order wrong makes rotate() return its input unchanged, which would make every
  # round identical and the order check below vacuously green -- which is why the
  # selftest asserts the rotation, not just that it runs.
  _rot_n=$2
  set -- $1
  while [ "$_rot_n" -gt 0 ]; do
    _rot_first=$1; shift
    set -- "$@" "$_rot_first"
    _rot_n=$((_rot_n - 1))
  done
  echo "$*"
}

# Given lines `<case> <answer>` across rounds, report a case whose answer moved.
order_dependent() {
  sort | awk '{ c = $1; $1 = ""; sub(/^ /, "")
                if (!(c in seen)) { seen[c] = $0 }
                else if (seen[c] != $0) { moved[c] = seen[c] " <> " $0 } }
              END { for (c in moved) printf "%s %s\n", c, moved[c] }'
}

if [ "${1:-}" = "--selftest" ]; then
  fails=0
  rotfail=0
  [ "$(rotate "a b c" 0)" = "a b c" ] || { echo "  FAIL rotate 0: $(rotate "a b c" 0)"; rotfail=1; }
  [ "$(rotate "a b c" 1)" = "b c a" ] || { echo "  FAIL rotate 1: $(rotate "a b c" 1)"; rotfail=1; }
  [ "$(rotate "a b c" 2)" = "c a b" ] || { echo "  FAIL rotate 2: $(rotate "a b c" 2)"; rotfail=1; }
  [ "$(rotate "a b c" 3)" = "a b c" ] || { echo "  FAIL rotate 3: $(rotate "a b c" 3)"; rotfail=1; }
  [ "$rotfail" -eq 0 ] && echo "  ok   rotate() actually rotates (0,1,2,3)" || fails=1

  stable=$(printf 'plain size=4\nplain size=4\nviamap ck=true\n' | order_dependent)
  [ -z "$stable" ] || { echo "  FAIL a stable case was reported order-dependent: $stable"; fails=1; }
  echo "  ok   a case with one answer is not reported"

  moved=$(printf 'plain size=4\nplain size=1\n' | order_dependent)
  case "$moved" in
    plain*'size=1 <> size=4'*|plain*'size=4 <> size=1'*) echo "  ok   a case whose answer moved IS reported" ;;
    *) echo "  FAIL a moved case was not reported: [$moved]"; fails=1 ;;
  esac

  # The confound this whole file exists for, replayed as data: H0-8's measured
  # `plain` result flips depending on whether it ran first. If order_dependent
  # ever stops catching that, the driver is decoration.
  h08=$(printf 'plain size=1_keys=[]\nplain size=4_keys=[k0,k1,k2,k3]\n' | order_dependent)
  [ -n "$h08" ] && echo "  ok   H0-8's own retracted result is caught as ORDER-DEPENDENT" \
    || { echo "  FAIL H0-8's retracted result was not caught"; fails=1; }

  [ "$fails" -eq 0 ] && { echo "  selftest OK — rotation, stability, movement, and the H0-8 replay"; exit 0; }
  exit 3
fi

CV="${CV:-$ROOT/target/release/cratonvm.exe}"
JDK="${JDK:-${JAVA_HOME:-}}"
[ -x "$CV" ] || { case "$CV" in *.exe) [ -x "${CV%.exe}" ] && CV="${CV%.exe}" ;; esac; }
[ -x "$CV" ] || { echo "ERROR: no cratonvm at \$CV=$CV"; exit 3; }
[ -n "$JDK" ] || { echo "ERROR: set JDK= (or JAVA_HOME); on MSYS use cygpath -m"; exit 3; }
case "$JDK" in /*) echo "ERROR: \$JDK is a POSIX path; use cygpath -m"; exit 3 ;; esac
JAVAC="$JDK/bin/javac.exe"; HS="$JDK/bin/java.exe"
[ -x "$JAVAC" ] || { JAVAC="$JDK/bin/javac"; HS="$JDK/bin/java"; }
[ -x "$JAVAC" ] || { echo "ERROR: no javac at $JAVAC"; exit 3; }

export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
TMP=$(mktemp -d) || exit 3
trap 'rm -rf "$TMP"' EXIT
CP="$TMP"; SRC="$HERE/ChmConsistencyProbe.java"
if command -v cygpath > /dev/null 2>&1; then
  CP=$(cygpath -m "$TMP"); SRC=$(cygpath -m "$SRC")
fi
"$JAVAC" -d "$CP" "$SRC" || { echo "ERROR: javac failed"; exit 3; }

# One process per case. The `CK` line is the answer; anything else is noise.
one() { # <env-or-empty> <case>
  if [ -n "$1" ]; then
    env "$1" "$CV" --java-home "$JDK" --jdk-only -cp "$CP" ChmConsistencyProbe "$2" 2>/dev/null
  else
    "$CV" --java-home "$JDK" --jdk-only -cp "$CP" ChmConsistencyProbe "$2" 2>/dev/null
  fi | sed -n 's/^CK ChmConsistencyProbe //p' | tr ' ' '_' | sed "s/^/$2 /"
}

echo "CHM CONSISTENCY  scope=$SCOPE  rounds=$ROUNDS  cases=$(printf '%s' "$CASES" | wc -w)"
echo "  one case per process; the set is rotated by one each round"
bad=0
for arm in unarmed armed; do
  envv=""
  [ "$arm" = armed ] && envv="CRATONVM_LOADER=enforce-native-shadow=$SCOPE"
  out=""
  r=0
  while [ "$r" -lt "$ROUNDS" ]; do
    for c in $(rotate "$CASES" "$r"); do
      out="$out
$(one "$envv" "$c")"
    done
    r=$((r + 1))
  done
  echo "  -- $arm --"
  printf '%s\n' "$out" | grep -v '^$' | sort -u | sed 's/^/     /'
  moved=$(printf '%s\n' "$out" | grep -v '^$' | order_dependent)
  if [ -n "$moved" ]; then
    echo "     ORDER-DEPENDENT in the $arm arm — the answer changed between rounds:"
    printf '%s\n' "$moved" | sed 's/^/       /'
    echo "       This is H0-8's confound, still present. Do NOT quote either answer"
    echo "       as a property of the case; it is a property of the position."
    bad=1
  fi
done

if [ -x "$HS" ]; then
  echo "  -- HotSpot oracle --"
  for c in $CASES; do
    "$HS" -cp "$CP" ChmConsistencyProbe "$c" 2>/dev/null \
      | sed -n 's/^CK ChmConsistencyProbe //p' | tr ' ' '_' | sed "s/^/     $c /"
  done
fi

[ "$bad" -eq 0 ] && echo "  ok — every case gave one stable answer per arm across $ROUNDS rotated rounds."
exit "$bad"
