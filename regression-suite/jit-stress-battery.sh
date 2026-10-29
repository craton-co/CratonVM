#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# jit-stress-battery.sh -- the round-21 stress modes with a positive control
# that cannot be skipped (STRESS21-4, script half).
#
# Runs every probe class under the lane-stress debug modes
# (jit-r21-fuzz-proposals.md, "Round 21 build (lane stress)"):
#   dal     CRATONVM_DBG=jit-deopt-a-lot=<N>:<seed>   (FUZZ21-1, forced poll exits)
#   gcbe    CRATONVM_DBG=gc-at-back-edge=<N>:<seed>   (FUZZ21-7, a GC at every Nth poll)
#   poison  CRATONVM_DBG=dead-home-poison             (FUZZ21-3, poisoned dead homes)
#   all     the three at once (period <N2>)
#   verify  CRATONVM_DBG=ir-verify-each-pass          (FUZZ21-2, findings only)
# and compares each arm's `fact` lines and exit code with the base arm. Each run
# writes stdout and stderr to separate files (<run>.out, <run>.out.err): facts
# come from stdout only, so a diagnostic line cannot split a fact line; a DIFF
# whose "missing" fact still appears inside a raw output line is reported
# INTERLEAVED, never DIFF.
#
# A stress run that never engaged proves nothing, and the modes fail SILENTLY
# when they do not engage (a binary without the switch ignores the variable; an
# OSR-only probe never reaches a method-entry poll exit). So every arm measures
# its own engagement in the same run, from diagnostics the modes already print:
#   dal / all  the `granted=` of the VM's exit line
#              `[cratonvm-deopt] FUZZ21-1 forced arrivals=<a> granted=<g>
#              refused=<r> ... collections=<c>` (FUZZTOOL21-1, printed by both
#              shutdown arms whenever a stress switch is set); on a binary
#              without that line, the count of per-exit `FUZZ21-1 deopt-a-lot:
#              ... told to leave` lines (the arm adds the `deopt` DBG token);
#   gcbe       the exit line's `collections=`; without it, collections
#              (distinct `GC(<id>)` on `-Xlog:gc` lines) minus the base arm's
#              (both arms get -Xlog:gc:stderr);
# An unengaged dal / gcbe / all verdict carries the exit line's
# `arrivals=` / `refused=` (`src=exit`), or `src=lines` for the fallback:
# arrivals=0 means the mode never reached a poll of the probe; arrivals>0 with
# granted=0 means it reached one and was refused every time (the
# `refused-*` fields of the line in the run's .err file say why).
#   poison     `[ir-slots] dead-home clears at` lines (the arm adds the
#              `ir-slots` DBG token): compiled clear sites that now store
#              the poison.
# A probe whose arm is SAME with zero engagement is reported SAME-UNENGAGED,
# never SAME. Before any probe runs, a built-in control program (a named
# method-entry body with a hot loop, an escaping allocation per iteration and a
# reference local that dies before a call) runs under every arm; if dal or gcbe
# does not engage on it, the battery stops with CONTROL-FAIL and exit 3: the
# binary lacks the modes or they are broken, and every probe verdict would be
# vacuous. The poison control is advisory (CONTROL-WEAK): whether a dead-home
# clear is planned depends on the register allocator.
#
# Usage:
#   jit-stress-battery.sh -b <cratonvm binary> -j <JDK home> -c <class dir> [options]
#
# Required:
#   -b BIN      CratonVM launcher binary.
#   -j JDK      JDK home (--java-home; its javac compiles the control).
#   -c DIR      Compiled probe classes (top-level classes; `X$Y` skipped).
# Options:
#   -p GLOB     Probe class-name glob(s), `|`-separated (default `R2[01]Fuzz*`).
#   -A ARMS     Arms to run (default `dal gcbe poison all verify`).
#   -P N        Period of dal / gcbe (default 97); seed is 1. The control
#               program's gcbe run uses the `all` period (-Q) instead: it needs
#               only MIN_GC collections, and at 97 its 7.5M compiled polls are
#               ~77k full collections -- past -t on a loaded host (2026-10-26:
#               24k collections in 300 s, TIMEOUT, no probe run).
#   -Q N        Period of the `all` arm (default 4093).
#   -m MODES    `jo` (--jdk-only, default) and/or `co` (--compatible).
#   -a ARGS     VM args for every run (default: the round's --add-opens /
#               --add-exports / --enable-native-access set).
#   -t SECS     Per-run timeout (default 300: the modes are slow by design).
#   -J N        Parallel probes (default 2).
#   -o DIR      Output directory (default ./stress-out).
#   -S          Strict: SAME-UNENGAGED counts as a failure in the exit status.
#   -K          Skip the control program (only to re-run probes after a control
#               already passed on the same binary; the summary says so).
#
# Output (stdout: non-SAME lines; <out>/summary.txt: everything):
#   CTL <arm> engaged=<n> OK | CONTROL-FAIL | CONTROL-WEAK
#   STR <probe> <mode> <arm> SAME engaged=<n> | SAME-UNENGAGED | DIFF <n> |
#       ABORT <what> | TIMEOUT | RC <a>-><b> | FINDINGS <n> (verify) | INTERLEAVED
#   STR-DONE probes=<n> diff=<n> abort=<n> unengaged=<n> findings=<n> timeout=<n> interleaved=<n>
# Exit: 0 clean, 1 a DIFF / ABORT / RC (or, with -S, an unengaged arm),
#       2 usage, 3 CONTROL-FAIL.
#
# Example (host):
#   regression-suite/jit-stress-battery.sh -b /data/jr20/bin/cratonvm-j21-w14 \
#     -j /data/jdkimages/jdk25-linux/jdk-25.0.4+7 -c /data/jr20/cls21 \
#     -m 'jo co' -o /data/jr20/out/stress-w14

set -u

BIN= JDK= CLS= PGLOB='R2[01]Fuzz*' ARMS='dal gcbe poison all verify' PER=97 PER2=4093 MODES=jo
TMO=300 JOBS=2 OUT=./stress-out STRICT=0 SKIPCTL=0
VMARGS='--add-opens java.base/java.lang=ALL-UNNAMED --add-exports java.base/jdk.internal.misc=ALL-UNNAMED --enable-native-access=ALL-UNNAMED'
MIN_GC=5

usage() { sed -n '2,/^set -u/p' "$0" | sed -e '/^set -u/d' -e 's/^# \{0,1\}//'; exit 2; }

while getopts "b:j:c:p:A:P:Q:m:a:t:J:o:SKh" opt; do
  case $opt in
    b) BIN=$OPTARG ;; j) JDK=$OPTARG ;; c) CLS=$OPTARG ;; p) PGLOB=$OPTARG ;; A) ARMS=$OPTARG ;;
    P) PER=$OPTARG ;; Q) PER2=$OPTARG ;; m) MODES=$OPTARG ;; a) VMARGS=$OPTARG ;; t) TMO=$OPTARG ;;
    J) JOBS=$OPTARG ;; o) OUT=$OPTARG ;; S) STRICT=1 ;; K) SKIPCTL=1 ;; h|*) usage ;;
  esac
done
[ -n "$BIN" ] && [ -n "$JDK" ] && [ -n "$CLS" ] || usage
[ -x "$BIN" ] || { echo "no executable binary: $BIN" >&2; exit 2; }
[ -x "$JDK/bin/javac" ] || { echo "no javac under $JDK" >&2; exit 2; }
[ -d "$CLS" ] || { echo "no class dir: $CLS" >&2; exit 2; }
if env | grep -q '^CRATONVM_'; then
  echo "WARNING: CRATONVM_* variables are set in the environment and apply to every arm:" >&2
  env | grep '^CRATONVM_' | sed 's/^/  /' >&2
fi
mkdir -p "$OUT/runs" "$OUT/ctl"
SUM="$OUT/summary.txt"; : > "$SUM"

mode_args() { case $1 in jo) echo "" ;; co) echo "--compatible" ;; *) echo "$1" ;; esac; }
sig() { grep -a -E '^(fact |rc=)' "$1"; }
emit() { echo "$*" >> "$SUM"; case "$*" in *' SAME engaged='*|*' OK') ;; *) echo "$*" ;; esac; }

# The env words and extra VM args of an arm. Grouped CRATONVM_DBG=<token>
# spelling (the per-flag variables print a deprecation line on every run).
arm_env() {
  case $1 in
    base)   echo "" ;;
    dal)    echo "CRATONVM_DBG=jit-deopt-a-lot=$PER:1,deopt" ;;
    gcbe)   echo "CRATONVM_DBG=gc-at-back-edge=${CTL_GC_PER:-$PER}:1" ;;
    poison) echo "CRATONVM_DBG=dead-home-poison,ir-slots" ;;
    all)    echo "CRATONVM_DBG=dead-home-poison,gc-at-back-edge=$PER2,jit-deopt-a-lot=$PER2,deopt" ;;
    verify) echo "CRATONVM_DBG=ir-verify-each-pass" ;;
  esac
}
# The GC log goes to stderr: a log line written by a GC thread must not split
# a fact line on stdout.
arm_args() { case $1 in base|gcbe) echo "-Xlog:gc:stderr" ;; *) echo "" ;; esac; }

# Every run writes stdout to <out> and stderr to <out>.err. Facts and the exit
# code are read from <out> ONLY; engagement, aborts and findings from both. With
# both streams in one file a diagnostic line can split a fact line mid-line
# (host w3n, jit-loopgen.sh -C: 32 false verdicts).
run_arm() { # <out> <cp> <class> <mode> <arm>
  local out=$1 cp=$2 c=$3 m=$4 arm=$5
  # shellcheck disable=SC2046,SC2086
  env $(arm_env "$arm") timeout -k 10 "$TMO" "$BIN" --java-home "$JDK" $(mode_args "$m") $VMARGS \
    $(arm_args "$arm") --cp "$cp" "$c" > "$out" 2> "$out.err" < /dev/null
  echo "rc=$?" >> "$out"
}
raw() { cat "$1" "$1.err" 2>/dev/null; }

# Collections in a -Xlog:gc run: distinct `GC(<id>)` ids on `[gc...]` lines
# (one id per collection whatever the backend prints for it).
pauses() { raw "$1" | grep -a '\[gc' | grep -aoE 'GC\([0-9]+\)' | sort -u | wc -l | tr -d ' '; }
# exitf <file> <field>: <field>= of the FUZZTOOL21-1 exit line
# (`[cratonvm-deopt] FUZZ21-1 forced arrivals=...`), empty when the run printed
# none (a binary from before the census, or a run killed before exit).
exitf() {
  raw "$1" | grep -a '\[cratonvm-deopt\] FUZZ21-1 forced arrivals=' | tail -1 \
    | grep -aoE "(^| )$2=[0-9]+" | head -1 | sed 's/.*=//'
}
# Engagement of <arm> in <file> (gcbe needs the base file too). The exit line
# first; the per-event lines / GC log only when it is absent.
engaged() {
  local v
  case $2 in
    dal|all)
      v=$(exitf "$1" granted)
      if [ -n "$v" ]; then echo "$v"; else raw "$1" | grep -ac 'FUZZ21-1 deopt-a-lot: .* told to leave'; fi ;;
    gcbe)
      v=$(exitf "$1" collections)
      if [ -n "$v" ]; then echo "$v"; else echo $(( $(pauses "$1") - $(pauses "$3") )); fi ;;
    poison)  raw "$1" | grep -ac '\[ir-slots\] dead-home clears at' ;;
    *)       echo 0 ;;
  esac
}
# Loud failures the modes are designed to produce.
abort_of() {
  raw "$1" | grep -aoE 'FUZZ21-3: the deopt frame.{0,80}|holds the JIT dead-home poison|rdx=0x53504[cC]4943454931|SIGSEGV|SIGBUS|SIGILL|hs_err|panicked at|fatal runtime error|has overflowed its stack' \
    | head -1
}
# interleaved <ref> <arm>: every signature line of <ref> missing from <arm>'s
# signature still occurs inside a raw line of <arm> (stdout or stderr), followed
# by the end of the line or a character that cannot continue a value: a split
# line, not a verdict.
interleaved() {
  local miss l
  miss=$(diff <(sig "$1") <(sig "$2") | sed -n 's/^< //p')
  [ -n "$miss" ] || return 1
  while IFS= read -r l; do
    case $l in rc=*) return 1 ;; esac
    raw "$2" | awk -v l="$l" '
      { s = $0
        while ((i = index(s, l)) > 0) {
          nx = substr(s, i + length(l), 1)
          if (nx == "" || nx !~ /[0-9A-Za-z._-]/) { f = 1; exit }
          s = substr(s, i + 1)
        } }
      END { exit f ? 0 : 1 }' || return 1
  done <<< "$miss"
  return 0
}

verdict() { # <base> <arm file> <arm>
  local b=$1 f=$2 arm=$3 ra rb n e a
  ra=$(grep -a '^rc=' "$f" | tail -1); rb=$(grep -a '^rc=' "$b" | tail -1)
  if [ "$ra" = rc=124 ] || [ "$ra" = rc=137 ]; then echo TIMEOUT; return; fi
  a=$(abort_of "$f")
  if [ -n "$a" ]; then echo "ABORT $a"; return; fi
  if [ "$arm" = verify ]; then
    n=$(raw "$f" | grep -ac '^\[ir-verify-each-pass\]')
    if [ "$n" -gt 0 ]; then echo "FINDINGS $n"; return; fi
  fi
  if ! diff <(sig "$b") <(sig "$f") > /dev/null; then
    if [ "$ra" != "$rb" ]; then echo "RC ${rb#rc=}->${ra#rc=}"; return; fi
    if interleaved "$b" "$f" || interleaved "$f" "$b"; then
      echo "INTERLEAVED (a fact line is split by another output line; rerun)"; return
    fi
    echo "DIFF $(diff <(sig "$b") <(sig "$f") | grep -c '^[<>]')"; return
  fi
  [ "$arm" = verify ] && { echo "SAME engaged=-"; return; }
  e=$(engaged "$f" "$arm" "$b")
  if [ "$e" -gt 0 ]; then echo "SAME engaged=$e"; else echo "SAME-UNENGAGED engaged=$e$(detail "$f" "$arm")"; fi
}
# detail <file> <arm>: why a dal / gcbe / all arm did not engage, from the exit
# line (` src=exit arrivals=<a> refused=<r>`), or ` src=lines` without it.
detail() {
  case $2 in dal|gcbe|all) ;; *) return ;; esac
  local a
  a=$(exitf "$1" arrivals)
  if [ -n "$a" ]; then echo " src=exit arrivals=$a refused=$(exitf "$1" refused)"; else echo " src=lines"; fi
}

# ---------------------------------------------------------------------------
# The control program.
# ---------------------------------------------------------------------------
control() {
  local d="$OUT/ctl" arm e f m fail=0
  cat > "$d/R21StressControl.java" <<'EOF'
// The jit-stress-battery.sh positive control: a NAMED method-entry body
// (kernel, called 25000 times) whose loop polls on its back edge, allocates
// an escaping object per iteration and holds a reference that dies before a
// call. Deterministic; prints fact lines.
public class R21StressControl {
    static final Object[] RING = new Object[64];
    static int[] A = new int[1024];

    static int touch(int v) { return (v * 31) ^ (v >>> 3); }

    static long kernel(int n, int salt) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            long[] box = new long[] { i, s };
            RING[i & 63] = box;
            int[] dead = A;
            s += dead[(i * 7) & 1023];
            s += touch(i ^ salt);
            Object back = RING[(i + 5) & 63];
            if (back instanceof long[] l) { s += l[0]; }
            A[i & 1023] += i ^ salt;
        }
        return s;
    }

    public static void main(String[] args) {
        long acc = 0;
        for (int r = 0; r < 25000; r++) { acc = acc * 31 + kernel(300, r); }
        System.out.println("fact control-acc " + acc);
        long sum = 0;
        for (int v : A) { sum += v; }
        System.out.println("fact control-array " + sum);
    }
}
EOF
  "$JDK/bin/javac" -nowarn -d "$d" "$d/R21StressControl.java" || { echo "CTL javac failed" >&2; exit 2; }
  for m in $MODES; do
    run_arm "$d/base.$m.out" "$d" R21StressControl "$m" base
    [ "$(grep -ac '^fact ' "$d/base.$m.out")" -eq 2 ] || { emit "CTL $m base CONTROL-FAIL (no fact lines: the binary cannot run the control)"; fail=1; continue; }
    for arm in $ARMS; do
      f="$d/$arm.$m.out"
      if [ "$arm" = gcbe ]; then CTL_GC_PER=$PER2 run_arm "$f" "$d" R21StressControl "$m" "$arm"
      else run_arm "$f" "$d" R21StressControl "$m" "$arm"; fi
      [ "$arm" = verify ] && { emit "CTL $m verify $(verdict "$d/base.$m.out" "$f" verify)"; continue; }
      e=$(engaged "$f" "$arm" "$d/base.$m.out")
      v=$(verdict "$d/base.$m.out" "$f" "$arm")
      case $arm in
        poison)
          if [ "$e" -gt 0 ]; then emit "CTL $m $arm engaged=$e OK"
          else emit "CTL $m $arm engaged=0 CONTROL-WEAK (no dead-home clear compiled in the control; per-probe counts still apply)"; fi ;;
        gcbe)
          if [ "$e" -ge "$MIN_GC" ]; then emit "CTL $m $arm engaged=$e OK"
          else emit "CTL $m $arm engaged=$e$(detail "$f" "$arm") CONTROL-FAIL (fewer than $MIN_GC collections: the mode is absent or broken)"; fail=1; fi ;;
        *)
          if [ "$e" -gt 0 ]; then emit "CTL $m $arm engaged=$e OK"
          else emit "CTL $m $arm engaged=0$(detail "$f" "$arm") CONTROL-FAIL (no forced poll exit: the mode is absent or broken)"; fail=1; fi ;;
      esac
      case $v in SAME*) ;; *) emit "CTL $m $arm control program verdict: $v"; fail=1 ;; esac
    done
  done
  if [ $fail = 1 ]; then echo "CONTROL-FAIL: see $SUM; no probe was run" | tee -a "$SUM"; exit 3; fi
}

if [ "$SKIPCTL" = 1 ]; then emit "CTL skipped (-K): the probe verdicts lean on an earlier control run"
else control; fi

# ---------------------------------------------------------------------------
# Probes.
# ---------------------------------------------------------------------------
PROBES=()
for c in "$CLS"/*.class; do
  [ -f "$c" ] || continue
  p=$(basename "$c" .class)
  case $p in *'$'*) continue ;; esac
  IFS='|' read -r -a pats <<< "$PGLOB"
  for g in "${pats[@]}"; do case $p in $g) PROBES+=("$p"); break ;; esac; done
done
[ ${#PROBES[@]} -gt 0 ] || { echo "no probe class matches '$PGLOB' in $CLS" >&2; exit 2; }

one() {
  local p=$1 m=$2 d="$OUT/runs/$1.$2" arm
  mkdir -p "$d"
  run_arm "$d/base.out" "$CLS" "$p" "$m" base
  for arm in $ARMS; do
    run_arm "$d/$arm.out" "$CLS" "$p" "$m" "$arm"
    emit "STR $p $m $arm $(verdict "$d/base.out" "$d/$arm.out" "$arm")"
  done
}
pool_wait() { while [ "$(jobs -rp | wc -l)" -ge "$JOBS" ]; do wait -n 2>/dev/null || sleep 0.2; done; }
for p in "${PROBES[@]}"; do for m in $MODES; do pool_wait; one "$p" "$m" & done; done
wait

ndiff=$(grep -cE '^STR .* (DIFF|RC) ' "$SUM"); nab=$(grep -c '^STR .* ABORT ' "$SUM")
nun=$(grep -c '^STR .* SAME-UNENGAGED' "$SUM"); nfi=$(grep -c '^STR .* FINDINGS ' "$SUM")
ntmo=$(grep -c '^STR .* TIMEOUT$' "$SUM")
nil=$(grep -c '^STR .* INTERLEAVED ' "$SUM")
echo "STR-DONE probes=${#PROBES[@]} diff=$ndiff abort=$nab unengaged=$nun findings=$nfi timeout=$ntmo interleaved=$nil" | tee -a "$SUM"
bad=$((ndiff + nab))
[ "$STRICT" = 1 ] && bad=$((bad + nun))
[ $bad -eq 0 ]
