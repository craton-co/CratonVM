#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# dispatch-witness.sh — WHICH WITNESSES CAN STILL SEE?
#
# THE PROBLEM THIS EXISTS FOR
# ---------------------------
# Every witness this effort has used was a coincidence of the VM being wrong in
# a visible way, so each died as the VM got more correct. `H17` measured FOUR OF
# SIX blind on a current binary, and one went blind BECAUSE OF A FIX: `H16-2`
# taught the native to mint real `HashMap$Node`s, so the bucket-head witness now
# agrees in both directions.
#
# A blind witness does not report blindness. It AGREES, and agreement reads as
# "no defect". So the instrument's job is not to be right about the VM — it is
# to state, from this run, WHICH of its signals can currently tell the two
# dispatch paths apart, and to REFUSE A VERDICT when none of them can.
#
# WORKER-1 cannot measure the enforcement dial without this: an armed FAILURE is
# real, but an armed ZERO is unreliable (the dial reaches one of four+ dispatch
# doors, `H17-2`), and a zero from a blind witness is indistinguishable from a
# zero from a fixed VM.
#
# HOW IT DECIDES
# --------------
# Two runs of `DispatchWitness`, ONE CASE PER PROCESS (`H0-8`: with anything
# latched per process, a second case in the same process is confounded with
# position — four "discriminators" of a supposed third CHM defect were pure
# order). Arm A is unarmed, arm B has the class armed. A signal DISCRIMINATES
# when the two arms print different values for it.
#
# Each arm runs REPEAT times (default 2) and a signal whose own arm is not
# self-consistent is reported UNSTABLE and excluded — otherwise noise reads as
# discrimination, which is the same mistake in the other direction.
#
# The HotSpot oracle is run too when available. It does not decide liveness; it
# says WHICH ARM IS RIGHT, which is a different question and the one a lane
# actually wants answered.
#
# WHAT IT MEASURED WHEN IT WAS WRITTEN
# ------------------------------------
# MEASURED 2026-08-21, cratonvm-r10.exe, scope java/util/HashMap, oracle
# HotSpot 25.0.3+9, REPEAT=2, one case per process:
#
#   case=put      3 of 13 discriminate
#     LIVE  frames      none | java.util.HashMap.hash,java.util.HashMap.put
#                       (ORACLE agrees with the ARMED arm)
#     LIVE  framedepth  3 | 5                       (oracle 5)
#     LIVE  iter        HashMap$EntryIterator | throws:NullPointerException
#                       (oracle HashMap$EntryIterator — the ARMED arm is wrong)
#     BLIND case op hccount eqcount table head tablen modcount consistent sizes
#
#   case=get      3 of 13 discriminate — same three, frames now
#                 `hash,getNode,get`, framedepth 3 | 6
#
#   case=iterate  1 of 14 discriminates
#     LIVE  iterated    8 | 1   (oracle 8)  <- H0-4 §7's "keySet() yields 1 of
#                                              3000", reproduced at n=8
#
# TEN of the thirteen signals are blind, and the three that are not are all
# DISPATCH-side or defect-side. That is the reusable finding:
#
#   a witness that asks WHAT WAS COMPUTED dies when the VM computes the right
#   thing. A witness that asks WHICH CODE RAN does not, because "more correct"
#   never means "reproduce the JDK's internal frame names".
#
# `head` is in the blind list, and it is the witness `H16-2` killed by teaching
# the native to mint real `HashMap$Node`s. This run PRINTS it as blind instead
# of letting its agreement read as good news. That is the whole point.
#
# THE FAILURE PATH, EXERCISED: arming a class the probe never touches
# (`dispatch-witness.sh put java/nio/file/NoSuchThing`) gives `0 signal(s)
# DISCRIMINATE, 13 blind, of 13` and exits 1. MEASURED, not argued — a gate that
# cannot fail is worse than no gate.
#
# Usage:
#   CV=/c/craton/cratonvm.exe JDK=C:/path/to/jdk regression-suite/probes/dispatch-witness.sh [case] [scope]
#   regression-suite/probes/dispatch-witness.sh --selftest
#
# Env: CV, JDK, CASE (put|get|iterate), SCOPE (armed class prefix), REPEAT
# Exit: 0 at least one signal discriminates · 1 NO signal does (the instrument
#       is blind — say so loudly) · 3 could not run
# ---------------------------------------------------------------------------
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
CASE="${1:-${CASE:-put}}"
SCOPE="${2:-${SCOPE:-java/util/HashMap}}"
REPEAT="${REPEAT:-2}"

# The signal names DispatchWitness is expected to emit. Listed here so a signal
# that stops being printed is reported MISSING rather than silently dropping out
# of the comparison — an absent signal and an equal signal must never look alike.
EXPECTED="case op hccount eqcount frames framedepth table head tablen modcount consistent sizes iter"

# ---------------------------------------------------------------------------
# The scoring, factored out so --selftest can drive it with no VM and no JDK.
#   $1 arm-A records (one `name value` per line, possibly repeated)
#   $2 arm-B records
# Emits `<state> <name> <a> | <b>` per signal, states:
#   LIVE      the arms differ and each arm agrees with itself
#   BLIND     the arms agree
#   UNSTABLE  an arm disagrees with itself across repeats
#   MISSING   the signal was not emitted by one or both arms
# ---------------------------------------------------------------------------
score() {
  printf 'A\n%s\nB\n%s\nEXPECTED %s\n' "$1" "$2" "$EXPECTED" | awk '
    /^A$/ { arm = "a"; next }
    /^B$/ { arm = "b"; next }
    /^EXPECTED / { for (i = 2; i <= NF; i++) want[$i] = 1; next }
    NF >= 1 {
      name = $1
      v = ""
      for (i = 2; i <= NF; i++) v = v (i > 2 ? " " : "") $i
      seen[arm "\t" name]++
      if (!((arm "\t" name) in val)) { val[arm "\t" name] = v }
      else if (val[arm "\t" name] != v) { unstable[arm "\t" name] = 1 }
      allnames[name] = 1
    }
    END {
      for (n in want) allnames[n] = 1
      k = 0
      for (n in allnames) names[k++] = n
      # deterministic order: insertion sort, so two runs of the gate diff cleanly
      for (i = 1; i < k; i++) { t = names[i]; j = i - 1
        while (j >= 0 && names[j] > t) { names[j+1] = names[j]; j-- }
        names[j+1] = t }
      for (i = 0; i < k; i++) {
        n = names[i]
        ha = (("a\t" n) in val); hb = (("b\t" n) in val)
        if (!ha || !hb) { printf "MISSING %s %s | %s\n", n, ha ? val["a\t" n] : "-", hb ? val["b\t" n] : "-"; continue }
        if ((("a\t" n) in unstable) || (("b\t" n) in unstable)) {
          printf "UNSTABLE %s %s | %s\n", n, val["a\t" n], val["b\t" n]; continue }
        if (val["a\t" n] == val["b\t" n]) printf "BLIND %s %s | %s\n", n, val["a\t" n], val["b\t" n]
        else printf "LIVE %s %s | %s\n", n, val["a\t" n], val["b\t" n]
      }
    }'
}

if [ "${1:-}" = "--selftest" ]; then
  fails=0
  chk() { # <label> <a> <b> <expected line>
    got=$(score "$2" "$3" | grep -E "^[A-Z]+ $4\$")
    if [ -n "$got" ]; then echo "  ok   $1"; else
      echo "  FAIL $1: expected '$4', got:"; score "$2" "$3" | sed 's/^/      /'; fails=1; fi
  }
  echo "SELFTEST dispatch-witness.sh — the scoring, with no VM and no JDK"

  # a signal that differs is LIVE
  chk "differing signal is LIVE" \
      "frames none" "frames java.util.HashMap.put" \
      "LIVE frames none | java.util.HashMap.put"
  # a signal that agrees is BLIND — the case H16-2 created and nothing reported
  chk "agreeing signal is BLIND" \
      "head java.util.HashMap\$Node" "head java.util.HashMap\$Node" \
      "BLIND head java.util.HashMap.Node | java.util.HashMap.Node"
  # an arm that disagrees with ITSELF is UNSTABLE, never LIVE
  chk "self-inconsistent arm is UNSTABLE" \
      "$(printf 'modcount 8\nmodcount 9\n')" "modcount 8" \
      "UNSTABLE modcount 8 | 8"
  # a signal one arm never printed is MISSING, never BLIND
  chk "one-sided signal is MISSING" \
      "iter java.util.HashMap\$EntryIterator" "" \
      "MISSING iter java.util.HashMap.EntryIterator | -"
  # a signal NOBODY printed is still MISSING, because EXPECTED names it
  chk "expected-but-unprinted signal is MISSING" "" "" "MISSING frames - | -"

  # the whole-instrument verdict: all-blind must be a FAILURE, not a pass
  allblind=$(score "head X
table Y" "head X
table Y")
  if printf '%s\n' "$allblind" | grep -q '^LIVE '; then
    echo "  FAIL all-blind input produced a LIVE signal"; fails=1
  else
    echo "  ok   all-blind input yields no LIVE signal (the run must exit 1)"
  fi

  # ordering is deterministic, so two runs of this gate diff cleanly
  o1=$(score "b 1
a 2" "b 3
a 4"); o2=$(score "a 2
b 1" "a 4
b 3")
  [ "$o1" = "$o2" ] && echo "  ok   output order is independent of input order" \
    || { echo "  FAIL output order depends on input order"; fails=1; }

  [ "$fails" -eq 0 ] && { echo "  selftest OK — LIVE, BLIND, UNSTABLE, MISSING, the all-blind refusal, and stable ordering"; exit 0; }
  exit 3
fi

# ---------------------------------------------------------------------------
# The real run.
# ---------------------------------------------------------------------------
CV="${CV:-$ROOT/target/release/cratonvm.exe}"
JDK="${JDK:-${JAVA_HOME:-}}"
[ -x "$CV" ] || { case "$CV" in *.exe) [ -x "${CV%.exe}" ] && CV="${CV%.exe}" ;; esac; }
[ -x "$CV" ] || { echo "ERROR: no cratonvm at \$CV=$CV"; exit 3; }
[ -n "$JDK" ] || { echo "ERROR: set JDK= (or JAVA_HOME). On MSYS use"
                   echo "  JDK=\$(cygpath -m \"\$(dirname \"\$(dirname \"\$(command -v javap)\")\")\")"
                   exit 3; }
case "$JDK" in
  /*) echo "ERROR: \$JDK is a POSIX path ($JDK). run.sh and this script export"
      echo "  MSYS_NO_PATHCONV=1, so it reaches cratonvm.exe unconverted and the VM"
      echo "  dies in argument parsing. Use: cygpath -m"; exit 3 ;;
esac
JAVAC="$JDK/bin/javac.exe"; HS="$JDK/bin/java.exe"
[ -x "$JAVAC" ] || { JAVAC="$JDK/bin/javac"; HS="$JDK/bin/java"; }
[ -x "$JAVAC" ] || { echo "ERROR: no javac at $JAVAC"; exit 3; }

export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
TMP=$(mktemp -d) || exit 3
trap 'rm -rf "$TMP"' EXIT
# MSYS_NO_PATHCONV is set above so that $JDK reaches cratonvm.exe intact, which
# means EVERY other path handed to a Windows tool must be converted here — the
# same trap in the other direction. Without this, javac is handed
# `\c\craton\...\DispatchWitness.java` and reports "file not found".
CP="$TMP"; SRC="$HERE/DispatchWitness.java"
if command -v cygpath > /dev/null 2>&1; then
  CP=$(cygpath -m "$TMP"); SRC=$(cygpath -m "$HERE/DispatchWitness.java")
fi
"$JAVAC" -d "$CP" "$SRC" || { echo "ERROR: javac failed"; exit 3; }

OPENS="--add-opens java.base/java.util=ALL-UNNAMED"

# ONE case per process, every time. The loop is over REPEATS, never over cases.
run_arm() { # <label> <env-assignment-or-empty>
  i=0
  while [ "$i" -lt "$REPEAT" ]; do
    if [ -n "$2" ]; then
      env "$2" "$CV" --java-home "$JDK" --jdk-only $OPENS -cp "$CP" DispatchWitness "$CASE" 2>/dev/null
    else
      "$CV" --java-home "$JDK" --jdk-only $OPENS -cp "$CP" DispatchWitness "$CASE" 2>/dev/null
    fi
    i=$((i + 1))
  done | sed -n 's/^W //p'
}

echo "DISPATCH WITNESS  case=$CASE  scope=$SCOPE  repeat=$REPEAT"
echo "  CV=$CV"
A=$(run_arm unarmed "")
# The supported spelling since 2026-08: the VM warns that the per-flag variable
# is legacy. Both still arm the dial; this uses the one it asks for.
B=$(run_arm armed "CRATONVM_LOADER=enforce-native-shadow=$SCOPE")
if [ -z "$A" ] || [ -z "$B" ]; then
  echo "  REFUSING: an arm produced no W lines (A=$(printf '%s' "$A" | grep -c .) B=$(printf '%s' "$B" | grep -c .))."
  echo "  A witness that cannot run is not a witness that saw nothing."
  exit 3
fi

ORACLE=""
if [ -x "$HS" ]; then
  ORACLE=$("$HS" $OPENS -cp "$CP" DispatchWitness "$CASE" 2>/dev/null | sed -n 's/^W //p')
fi

RESULT=$(score "$A" "$B")
printf '%s\n' "$RESULT" | while read -r state name rest; do
  o="-"
  [ -n "$ORACLE" ] && o=$(printf '%s\n' "$ORACLE" | awk -v n="$name" '$1 == n { $1 = ""; sub(/^ /, ""); print; exit }')
  printf '  %-9s %-11s unarmed/armed: %-58s oracle: %s\n' "$state" "$name" "$rest" "${o:--}"
done

LIVE=$(printf '%s\n' "$RESULT" | grep -c '^LIVE ')
BLIND=$(printf '%s\n' "$RESULT" | grep -c '^BLIND ')
echo "  ---"
echo "  $LIVE signal(s) DISCRIMINATE, $BLIND blind, of $(printf '%s\n' "$RESULT" | grep -c .)."
if [ "$LIVE" -eq 0 ]; then
  echo "  THE INSTRUMENT IS BLIND. Every signal agrees across the two arms, so this"
  echo "  run cannot tell a fixed VM from an unobservable one. Do NOT read a zero"
  echo "  from it as evidence of anything — add a signal before measuring again."
  echo "  This is the H16-2 shape: a FIX to the VM removed a witness, and a blind"
  echo "  witness reports agreement, which reads as good news."
  exit 1
fi
echo "  A zero from a BLIND signal is not evidence. A difference on a LIVE one is."
exit 0
