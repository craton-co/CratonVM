#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# jit-loopgen.sh -- FUZZ21-6, generated loop programs against --nojit.
#
# Generates COUNT deterministic random loop programs with
# regression-suite/jit-fuzz/LoopGen.java (seeds SEED .. SEED+COUNT-1), compiles
# them, runs each under CratonVM --nojit (the oracle: no HotSpot needed per
# program) and under each JIT arm, and compares the `fact` lines and the exit
# code. Every program that DIFFs is re-run on HotSpot (default and -Xint, both
# with -XX:-OmitStackTraceInFastThrow) to say which side is wrong, and its
# source is copied to <out>/keep/ (rename it R21FuzzGen<seed>.java into the
# probe set if it is a real defect).
#
# Usage:
#   jit-loopgen.sh -b <cratonvm binary> -j <JDK home> [options]
#
# Required:
#   -b BIN      CratonVM launcher binary.
#   -j JDK      JDK 25 home: --java-home for CratonVM; its java runs the
#               generator (source launcher) and is the HotSpot oracle; its
#               javac compiles the programs.
# Options:
#   -s SEED     First seed (default 1). Seeds are non-negative.
#   -n COUNT    Number of programs (default 50).
#   -k K        Kernels per program (default 3).
#   -N OSRN     The long call's trip count (default 50000).
#   -W WARM     Warm calls per kernel (default 20000).
#   -m MODES    JIT arms, space-separated (default `jo`): jo = --jdk-only
#               (launcher default), co = --compatible, or any literal launcher
#               flag(s) in quotes, e.g. -m 'jo co'.
#   -e ENVS     `K=V` words applied to the JIT arms only (not to the --nojit
#               oracle), e.g. -e 'CRATONVM_DBG=jit-deopt-a-lot=97:1' to run the
#               programs under a stress mode (use the grouped CRATONVM_<GROUP>=
#               spelling; -C merges its token into a CRATONVM_DBG word).
#   -A          Run HotSpot on EVERY program, not only on DIFFs (checks the
#               --nojit oracle itself; ~2x the time).
#   -C          Engagement control: add CRATONVM_DBG=ir-compiles to the JIT
#               arms and count `[ir] acceptance <class>.kN(..): accepted` lines
#               (read from the run's stderr file; facts come from stdout only)
#               (optimizing-tier kernel bodies the acceptance gate kept); a
#               program with none is reported UNENGAGED (its SAME
#               says nothing about the optimizing tier).
#   -V          FUZZTOOL21-2 variants (LoopGen's `variants=1`): programs
#               R21FuzzGenV<seed> with a subclass loaded mid-loop (CHA), a
#               <clinit> run mid-loop, an int local negative at OSR entry,
#               int/long mixing across calls and callee exceptions caught in
#               the loop. Without -V the programs are byte-identical to the
#               earlier generator's for the same seeds.
#   -t SECS     Per-run timeout (default 120).
#   -J N        Parallel programs (default 2).
#   -o DIR      Output directory (default ./loopgen-out).
#   -a ARGS     Extra VM args for every CratonVM and HotSpot run (default none).
#
# Output (stdout and <out>/summary.txt):
#   LG <class> <mode> SAME [accepted=<n>]
#   LG <class> <mode> DIFF <class of fault>   where the fault class is
#       JIT-WRONG     --nojit == HotSpot, the JIT arm differs (a JIT defect: keep it)
#       NOJIT-WRONG   the JIT arm == HotSpot, --nojit differs (an interpreter defect)
#       BOTH-WRONG    neither CratonVM arm equals HotSpot
#       HS-TIERED     HotSpot default != HotSpot -Xint (should not happen for
#                     these programs; a generator bug or a HotSpot answer to look at)
#   LG <class> <mode> TIMEOUT | UNENGAGED
#   LG <class> <mode> INTERLEAVED  (the "missing" fact ends some raw output line:
#                                   a split line, not a verdict; rerun)
#   LG <class> nojit NOJIT-WRONG   (with -A: the oracle itself differs from HotSpot)
#   LG <class> - GENFAIL           (the program did not compile: a generator bug, report it)
#   LG-DONE programs=<n> same=<n> diff=<n> jit_wrong=<n> timeout=<n> genfail=<n>
# Exit status: 0 when nothing DIFFed, 1 otherwise, 2 on usage errors.
#
# Example (host):
#   regression-suite/jit-loopgen.sh -b /data/jr20/bin/cratonvm-j21-w14 \
#     -j /data/jdkimages/jdk25-linux/jdk-25.0.4+7 -s 1 -n 200 -m 'jo co' -C \
#     -o /data/jr20/out/lg-w14

set -u

HERE=$(cd "$(dirname "$0")" && pwd)
BIN= JDK= SEED=1 COUNT=50 KERNELS=3 OSRN=50000 WARM=20000 MODES=jo JENV= ALLHS=0 CONTROL=0 VARIANTS=0
TMO=120 JOBS=2 OUT=./loopgen-out VMARGS=

usage() { sed -n '2,/^set -u/p' "$0" | sed -e '/^set -u/d' -e 's/^# \{0,1\}//'; exit 2; }

while getopts "b:j:s:n:k:N:W:m:e:ACVt:J:o:a:h" opt; do
  case $opt in
    b) BIN=$OPTARG ;; j) JDK=$OPTARG ;; s) SEED=$OPTARG ;; n) COUNT=$OPTARG ;;
    k) KERNELS=$OPTARG ;; N) OSRN=$OPTARG ;; W) WARM=$OPTARG ;; m) MODES=$OPTARG ;;
    e) JENV=$OPTARG ;; A) ALLHS=1 ;; C) CONTROL=1 ;; V) VARIANTS=1 ;; t) TMO=$OPTARG ;; J) JOBS=$OPTARG ;;
    o) OUT=$OPTARG ;; a) VMARGS=$OPTARG ;; h|*) usage ;;
  esac
done
[ -n "$BIN" ] && [ -n "$JDK" ] || usage
[ -x "$BIN" ] || { echo "no executable binary: $BIN" >&2; exit 2; }
[ -x "$JDK/bin/java" ] && [ -x "$JDK/bin/javac" ] || { echo "no JDK (java + javac) at $JDK" >&2; exit 2; }
[ -f "$HERE/jit-fuzz/LoopGen.java" ] || { echo "missing $HERE/jit-fuzz/LoopGen.java" >&2; exit 2; }
if env | grep -q '^CRATONVM_'; then
  echo "WARNING: CRATONVM_* variables are set in the environment and apply to every run:" >&2
  env | grep '^CRATONVM_' | sed 's/^/  /' >&2
fi

mkdir -p "$OUT/src" "$OUT/cls" "$OUT/runs" "$OUT/keep"
SUM="$OUT/summary.txt"; : > "$SUM"

# ---- generate and compile ----
"$JDK/bin/java" "$HERE/jit-fuzz/LoopGen.java" "$SEED" "$COUNT" "$OUT/src" "$KERNELS" "$OSRN" "$WARM" "$VARIANTS" \
  > "$OUT/classes.txt" || { echo "generator failed" >&2; exit 2; }
NAMES=()
while read -r c; do [ -n "$c" ] && NAMES+=("$c"); done < "$OUT/classes.txt"
if ! "$JDK/bin/javac" -nowarn -d "$OUT/cls" "$OUT"/src/R21FuzzGen*.java > "$OUT/javac.txt" 2>&1; then
  # Compile one by one so a single bad program does not sink the batch.
  for c in "${NAMES[@]}"; do
    "$JDK/bin/javac" -nowarn -d "$OUT/cls" "$OUT/src/$c.java" > "$OUT/runs/$c.javac.txt" 2>&1 \
      || echo "LG $c - GENFAIL" | tee -a "$SUM"
  done
fi

mode_args() { case $1 in jo) echo "" ;; co) echo "--compatible" ;; nojit) echo "--nojit" ;; *) echo "$1" ;; esac; }
mode_tag() { echo "$1" | tr -c 'A-Za-z0-9_\n-' '_'; }
sig() { grep -a -E '^(fact |rc=)' "$1"; }
same() { diff <(sig "$1") <(sig "$2") > /dev/null; }
is_timeout() { grep -qaE '^rc=(124|137)$' "$1"; }

# Every run writes stdout to <out> and stderr to <out>.err: the facts are read
# from stdout ONLY. Diagnostics (CRATONVM_DBG=..., warnings) go to stderr, and
# with both streams on one file a DBG line can split a fact line mid-line
# (host w3n: 32 false JIT-WRONG verdicts). Engagement is counted from <out>.err.
run_cvm() { # <out> <class> <mode> [K=V...]
  local out=$1 c=$2 m=$3; shift 3
  # shellcheck disable=SC2086
  env "$@" timeout -k 10 "$TMO" "$BIN" --java-home "$JDK" $(mode_args "$m") $VMARGS --cp "$OUT/cls" "$c" \
    > "$out" 2> "$out.err" < /dev/null
  echo "rc=$?" >> "$out"
}
run_hs() { # <out> <class> [hotspot flags...]
  local out=$1 c=$2; shift 2
  [ -s "$out" ] && return
  # shellcheck disable=SC2086
  timeout -k 10 $((TMO * 4)) "$JDK/bin/java" -XX:-OmitStackTraceInFastThrow "$@" $VMARGS -cp "$OUT/cls" "$c" \
    > "$out" 2> "$out.err" < /dev/null
  echo "rc=$?" >> "$out"
}
# interleaved <ref> <arm>: true when every signature line of <ref> missing from
# <arm>'s signature still occurs inside some line of <arm>'s raw stdout or
# stderr, followed by the end of the line or by a character that cannot
# continue a value (so `fact x 12` is not found inside `fact x 123`): the value
# is there and only a split line hid it from `grep ^fact`.
interleaved() {
  local miss l
  miss=$(diff <(sig "$1") <(sig "$2") | sed -n 's/^< //p')
  [ -n "$miss" ] || return 1
  while IFS= read -r l; do
    case $l in rc=*) return 1 ;; esac
    cat "$2" "$2.err" 2>/dev/null | awk -v l="$l" '
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
# The JIT arms' env: $JENV, plus `ir-compiles` merged into any CRATONVM_DBG
# word of it under -C (the grouped spelling; the per-flag variable prints a
# deprecation line).
jit_env() {
  local w out= merged=0
  for w in $JENV; do
    if [ "$CONTROL" = 1 ] && [ "${w%%=*}" = CRATONVM_DBG ]; then w="$w,ir-compiles"; merged=1; fi
    out="$out $w"
  done
  [ "$CONTROL" = 1 ] && [ $merged = 0 ] && out="$out CRATONVM_DBG=ir-compiles"
  echo "$out"
}

one() {
  local c=$1 d="$OUT/runs/$1" m t f hs hx v n
  mkdir -p "$d"
  run_cvm "$d/nojit.out" "$c" nojit
  if [ "$ALLHS" = 1 ]; then
    run_hs "$d/hs.out" "$c"; run_hs "$d/hsint.out" "$c" -Xint
    same "$d/hs.out" "$d/hsint.out" || echo "LG $c hotspot HS-TIERED" >> "$SUM"
    same "$d/hs.out" "$d/nojit.out" || { echo "LG $c nojit NOJIT-WRONG" >> "$SUM"; cp "$OUT/src/$c.java" "$OUT/keep/"; }
  fi
  for m in $MODES; do
    t=$(mode_tag "$m"); f="$d/$t.out"
    # shellcheck disable=SC2046
    run_cvm "$f" "$c" "$m" $(jit_env)
    if is_timeout "$f"; then echo "LG $c $t TIMEOUT" >> "$SUM"; continue; fi
    if same "$d/nojit.out" "$f"; then
      if [ "$CONTROL" = 1 ]; then
        # `[ir] acceptance <class>.<method><desc>: accepted (...)`, kernels only.
        n=$(cat "$f.err" "$f" | grep -a '\[ir\] acceptance' | grep -aE "$c[.]k[0-9]+[(]" | grep -ac ': accepted')
        if [ "$n" -eq 0 ]; then echo "LG $c $t UNENGAGED accepted=0" >> "$SUM"
        else echo "LG $c $t SAME accepted=$n" >> "$SUM"; fi
      else
        echo "LG $c $t SAME" >> "$SUM"
      fi
      continue
    fi
    # A fact the signature misses but the raw output holds is a split line,
    # not a verdict.
    if interleaved "$d/nojit.out" "$f" || interleaved "$f" "$d/nojit.out"; then
      echo "LG $c $t INTERLEAVED (a fact line is split by another output line; rerun, not a verdict)" >> "$SUM"
      continue
    fi
    # A DIFF: ask HotSpot which side is wrong.
    run_hs "$d/hs.out" "$c"; run_hs "$d/hsint.out" "$c" -Xint
    hs="$d/hs.out"; hx="$d/hsint.out"
    if ! same "$hs" "$hx"; then v=HS-TIERED
    elif same "$hs" "$d/nojit.out"; then v=JIT-WRONG
    elif same "$hs" "$f"; then v=NOJIT-WRONG
    else v=BOTH-WRONG; fi
    echo "LG $c $t DIFF $v ($(diff <(sig "$d/nojit.out") <(sig "$f") | grep -c '^[<>]') lines vs --nojit)" >> "$SUM"
    cp "$OUT/src/$c.java" "$OUT/keep/"
  done
}

pool_wait() { while [ "$(jobs -rp | wc -l)" -ge "$JOBS" ]; do wait -n 2>/dev/null || sleep 0.2; done; }

for c in "${NAMES[@]}"; do
  [ -f "$OUT/cls/$c.class" ] || continue
  pool_wait
  one "$c" &
done
wait

grep -v ' SAME' "$SUM" | grep -v '^$' || true
nsame=$(grep -c ' SAME' "$SUM"); ndiff=$(grep -c ' DIFF ' "$SUM"); njw=$(grep -c 'DIFF JIT-WRONG' "$SUM")
ntmo=$(grep -c ' TIMEOUT$' "$SUM"); ngen=$(grep -c ' GENFAIL$' "$SUM"); nun=$(grep -c ' UNENGAGED ' "$SUM")
nnw=$(grep -c ' nojit NOJIT-WRONG$' "$SUM"); nil=$(grep -c ' INTERLEAVED ' "$SUM")
echo "LG-DONE programs=${#NAMES[@]} same=$nsame diff=$ndiff jit_wrong=$njw nojit_wrong_vs_hs=$nnw timeout=$ntmo unengaged=$nun interleaved=$nil genfail=$ngen" | tee -a "$SUM"
[ $((ndiff + nnw)) -eq 0 ]
