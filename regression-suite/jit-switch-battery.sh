#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# jit-switch-battery.sh -- FUZZ21-5, the per-switch differential battery.
#
# Runs every probe class of a class directory once with the defaults (the BASE
# arm) and then once per default-ON switch with that one switch turned OFF, and
# reports every switch whose OFF arm prints `fact` lines (or an exit code)
# different from the base arm. When a fuzz probe DIFFs against HotSpot, the
# switch whose kill restores HotSpot's lines is the optimisation to look at;
# with -H the battery names it directly (RESTORES-HS).
#
# The switch list is read from types/src/flag_groups.rs: every `E { ... }` row
# whose `off_word` is `Some(..)` (the default-ON rows: setting `on_key` to the
# off word turns the switch off; a row that also has an `off_key` gets that key
# set to 1 as well, which is how the token's OFF spelling expands). Nothing in
# this script knows switch names; a new row is picked up by the next run.
#
# Usage:
#   jit-switch-battery.sh -b <cratonvm binary> -j <JDK home> -c <class dir> [options]
#
# Required:
#   -b BIN      CratonVM launcher binary.
#   -j JDK      JDK home: passed to CratonVM as --java-home, and its bin/java is
#               the HotSpot oracle for -H.
#   -c DIR      Directory of compiled probe classes (top-level classes only;
#               nested `X$Y.class` files are skipped).
# Options:
#   -p GLOB     Probe class-name glob (shell pattern), default `*`. Repeatable
#               as a `|`-separated list: -p 'R21Fuzz*|R20Fuzz*'.
#   -s REGEX    Only switches whose on_key matches this ERE (default: all of the
#               selected groups). Example: -s '^CRATONVM_JIT_IR_(UNROLL|LOOP_)'.
#   -g GROUPS   `|`-separated flag_groups.rs groups to take rows from, default
#               `JIT`. Example: -g 'JIT|GC'.
#   -x FILE     Exclude list: every CRATONVM_[A-Z0-9_]+ word in FILE is
#               skipped (an opt-in list, known-noisy switches, ...).
#   -f FILE     flag_groups.rs to read (default: ../types/src/flag_groups.rs
#               next to this script). On a host without a checkout, copy it.
#   -m MODES    Space-separated arms of the VM to battery, default `jo`:
#               jo = --jdk-only (the launcher default, no flag), co =
#               --compatible. Each mode has its own base arm.
#   -a ARGS     VM args for every CratonVM and HotSpot run (default: the
#               round's probe set: --add-opens java.base/java.lang=ALL-UNNAMED
#               --add-exports java.base/jdk.internal.misc=ALL-UNNAMED
#               --enable-native-access=ALL-UNNAMED).
#   -e ENVS     Extra `K=V` words applied to EVERY arm (base included), e.g.
#               -e 'CRATONVM_C2_ACCEPT=always'.
#   -t SECS     Per-run timeout (default 120). A timed-out run is `TIMEOUT`.
#   -J N        Parallel runs (default 2; the shared host wants <= 3).
#   -r N        Base runs per probe and mode (default 2). If the base runs
#               disagree, the probe is `UNSTABLE` and not batteried (its DIFFs
#               would be noise).
#   -H          Also run HotSpot (`-XX:-OmitStackTraceInFastThrow`, cached under
#               the output dir) and mark a switch arm whose facts EQUAL
#               HotSpot's while the base arm's do not as `RESTORES-HS`.
#   -d          With -H: battery only the probes whose base arm DIFFs from
#               HotSpot (the bounded "which switch?" run).
#   -M MODE     `each` (default): one run per switch. `split`: adaptive group
#               testing -- turn half of the remaining switches off at once,
#               recurse only into halves whose facts differ, confirm each leaf
#               with a single-switch run (about k*log2(S) runs for k culprits
#               instead of S; a group that differs while none of its members
#               does alone is reported `GROUP-ONLY` with its range).
#   -o DIR      Output directory (default ./swb-out). Per-run outputs are kept.
#   -n          Dry run: print the switch list and the planned run count only.
#
# Output (stdout and <out>/summary.txt; stdout shows only non-SAME lines):
#   SWB <probe> <mode> BASE facts=<n> rc=<rc> [hs=SAME|DIFF]
#   SWB <probe> <mode> <SWITCH>=<off word> SAME|DIFF <n lines>|RESTORES-HS|TIMEOUT|RC <a>-><b>|INTERLEAVED
#       (INTERLEAVED: the "missing" fact still appears inside a raw output
#       line -- a split line, not a verdict. Each run's stdout and stderr go to
#       separate files, <run>.out and <run>.out.err; facts come from stdout.)
#   SWB <probe> <mode> UNSTABLE   (base runs disagree)
#   SWB-DONE probes=<n> runs=<n> diff=<n> restores=<n> group_only=<n> timeout=<n> unstable=<n> interleaved=<n>
# Exit status: 0 when no switch arm differed, 1 when one did, 2 on usage errors.
#
# Cost: probes x modes x switches x (probe run time). The JIT group has about
# 1300 default-ON rows, so the `each` mode wants -s / -p / -d; `split` is the
# mode for "which of all of them?" on one DIFFing probe.
#
# Example (host):
#   regression-suite/jit-switch-battery.sh -b /data/jr20/bin/cratonvm-j21-w14 \
#     -j /data/jdkimages/jdk25-linux/jdk-25.0.4+7 -c /data/jr20/cls21 \
#     -p 'R21FuzzOsrPred1' -M split -H -o /data/jr20/out/swb-osrpred

set -u

HERE=$(cd "$(dirname "$0")" && pwd)
BIN= JDK= CLS= PGLOB='*' SREGEX= FGROUPS=JIT XFILE= FLAGS="$HERE/../types/src/flag_groups.rs"
MODES=jo EXTRA_ENV= TMO=120 JOBS=2 BASERUNS=2 WITH_HS=0 DIFFING_ONLY=0 SMODE=each OUT=./swb-out DRY=0
VMARGS='--add-opens java.base/java.lang=ALL-UNNAMED --add-exports java.base/jdk.internal.misc=ALL-UNNAMED --enable-native-access=ALL-UNNAMED'

usage() { sed -n '2,/^set -u/p' "$0" | sed -e '/^set -u/d' -e 's/^# \{0,1\}//'; exit 2; }

while getopts "b:j:c:p:s:g:x:f:m:a:e:t:J:r:HdM:o:nh" opt; do
  case $opt in
    b) BIN=$OPTARG ;; j) JDK=$OPTARG ;; c) CLS=$OPTARG ;; p) PGLOB=$OPTARG ;;
    s) SREGEX=$OPTARG ;; g) FGROUPS=$OPTARG ;; x) XFILE=$OPTARG ;; f) FLAGS=$OPTARG ;;
    m) MODES=$OPTARG ;; a) VMARGS=$OPTARG ;; e) EXTRA_ENV=$OPTARG ;; t) TMO=$OPTARG ;;
    J) JOBS=$OPTARG ;; r) BASERUNS=$OPTARG ;; H) WITH_HS=1 ;; d) DIFFING_ONLY=1 ;;
    M) SMODE=$OPTARG ;; o) OUT=$OPTARG ;; n) DRY=1 ;; h|*) usage ;;
  esac
done
[ -n "$BIN" ] && [ -n "$JDK" ] && [ -n "$CLS" ] || usage
[ -x "$BIN" ] || { echo "no executable binary: $BIN" >&2; exit 2; }
[ -x "$JDK/bin/java" ] || { echo "no JDK at $JDK" >&2; exit 2; }
[ -d "$CLS" ] || { echo "no class dir: $CLS" >&2; exit 2; }
[ -f "$FLAGS" ] || { echo "no flag table: $FLAGS (pass -f)" >&2; exit 2; }
case $SMODE in each|split) ;; *) echo "-M must be each or split" >&2; exit 2 ;; esac
[ "$DIFFING_ONLY" = 1 ] && [ "$WITH_HS" = 0 ] && { echo "-d needs -H" >&2; exit 2; }

# An ambient CRATONVM_* variable would silently ride into every arm.
if env | grep -q '^CRATONVM_'; then
  echo "WARNING: CRATONVM_* variables are set in the environment and apply to every arm:" >&2
  env | grep '^CRATONVM_' | sed 's/^/  /' >&2
fi

mkdir -p "$OUT/runs" "$OUT/hs"
SUM="$OUT/summary.txt"; : > "$SUM"

# ---------------------------------------------------------------------------
# The switch list: "<on_key> <off_word> <off_key|->", one per line.
# ---------------------------------------------------------------------------
SWLIST="$OUT/switches.txt"
grep -E '^[[:space:]]*E \{ group: Group::[A-Z_]+,.*off_word: Some\("' "$FLAGS" \
  | sed -E 's/.*group: Group::([A-Z_]+),.*on_key: Some\("([A-Z0-9_]+)"\), off_key: (None|Some\("([A-Z0-9_]+)"\)), off_word: Some\("([^"]*)"\).*/\1 \2 \5 \4/' \
  | grep -E "^($FGROUPS) " \
  | awk '{ ok = ($4 == "") ? "-" : $4; print $2, $3, ok }' > "$SWLIST.all"
if [ -n "$SREGEX" ]; then awk -v re="$SREGEX" '$1 ~ re' "$SWLIST.all" > "$SWLIST.sel"
else cp "$SWLIST.all" "$SWLIST.sel"; fi
if [ -n "$XFILE" ]; then
  grep -oE 'CRATONVM_[A-Z0-9_]+' "$XFILE" | sort -u > "$OUT/exclude.txt"
  awk 'NR == FNR { x[$1] = 1; next } !($1 in x)' "$OUT/exclude.txt" "$SWLIST.sel" > "$SWLIST"
else cp "$SWLIST.sel" "$SWLIST"; fi
rm -f "$SWLIST.all" "$SWLIST.sel"
NSW=$(wc -l < "$SWLIST" | tr -d ' ')
[ "$NSW" -gt 0 ] || { echo "no switch selected (groups=$FGROUPS regex=$SREGEX)" >&2; exit 2; }

SW_KEY=(); SW_WORD=(); SW_OFFKEY=()
while read -r k w o; do SW_KEY+=("$k"); SW_WORD+=("$w"); SW_OFFKEY+=("$o"); done < "$SWLIST"

# The env words that turn switches [lo, hi) off.
off_env() {
  local lo=$1 hi=$2 i
  for ((i = lo; i < hi; i++)); do
    printf '%s=%s\n' "${SW_KEY[$i]}" "${SW_WORD[$i]}"
    [ "${SW_OFFKEY[$i]}" != - ] && printf '%s=1\n' "${SW_OFFKEY[$i]}"
  done
}

# ---------------------------------------------------------------------------
# Probes.
# ---------------------------------------------------------------------------
PROBES=()
for c in "$CLS"/*.class; do
  [ -f "$c" ] || continue
  p=$(basename "$c" .class)
  case $p in *'$'*) continue ;; esac
  match=0; IFS='|' read -r -a pats <<< "$PGLOB"
  for g in "${pats[@]}"; do case $p in $g) match=1 ;; esac; done
  [ $match = 1 ] && PROBES+=("$p")
done
[ ${#PROBES[@]} -gt 0 ] || { echo "no probe class matches '$PGLOB' in $CLS" >&2; exit 2; }

echo "switches=$NSW probes=${#PROBES[@]} modes='$MODES' mode=$SMODE timeout=${TMO}s jobs=$JOBS" | tee -a "$SUM"
if [ "$DRY" = 1 ]; then
  cat "$SWLIST"
  echo "planned runs (each, upper bound): $(( ${#PROBES[@]} * $(echo $MODES | wc -w) * (NSW + BASERUNS) ))"
  exit 0
fi

mode_args() { case $1 in jo) echo "" ;; co) echo "--compatible" ;; nojit) echo "--nojit" ;; *) echo "$1" ;; esac; }

# run_cvm <out> <probe> <mode> [K=V...]: stdout in <out> with "rc=<n>"
# appended, stderr in <out>.err. Facts are read from stdout ONLY: with both
# streams in one file a diagnostic line (a DBG token, the per-flag-variable
# deprecation line the off arms print) can split a fact line mid-line.
run_cvm() {
  local out=$1 p=$2 m=$3; shift 3
  # shellcheck disable=SC2086
  env $EXTRA_ENV "$@" timeout -k 10 "$TMO" "$BIN" --java-home "$JDK" $(mode_args "$m") $VMARGS \
    --cp "$CLS" "$p" > "$out" 2> "$out.err" < /dev/null
  echo "rc=$?" >> "$out"
}
# The compared signature of a run: its fact lines and its exit code.
sig() { grep -a -E '^(fact |rc=)' "$1"; }
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

pool_wait() { while [ "$(jobs -rp | wc -l)" -ge "$JOBS" ]; do wait -n 2>/dev/null || sleep 0.2; done; }

emit() { echo "$*" >> "$SUM"; case "$*" in *' SAME'|*' SAME '*) ;; *) echo "$*" ;; esac; }

hs_ref() { # cached HotSpot run of probe $1
  local h="$OUT/hs/$1.out"
  if [ ! -s "$h" ]; then
    # shellcheck disable=SC2086
    timeout -k 10 "$TMO" "$JDK/bin/java" -XX:-OmitStackTraceInFastThrow $VMARGS -cp "$CLS" "$1" \
      > "$h" 2> "$h.err" < /dev/null
    echo "rc=$?" >> "$h"
  fi
  echo "$h"
}

# verdict <base file> <arm file> <hs file|-> -> "SAME" | "DIFF n" | ...
verdict() {
  local b=$1 a=$2 h=$3 rb ra n
  ra=$(grep -a '^rc=' "$a" | tail -1); rb=$(grep -a '^rc=' "$b" | tail -1)
  [ "$ra" = rc=124 ] || [ "$ra" = rc=137 ] && { echo TIMEOUT; return; }
  if diff <(sig "$b") <(sig "$a") > /dev/null; then echo SAME; return; fi
  if [ "$h" != - ] && diff <(sig "$h") <(sig "$a") > /dev/null; then echo RESTORES-HS; return; fi
  if [ "$ra" != "$rb" ]; then echo "RC ${rb#rc=}->${ra#rc=}"; return; fi
  if interleaved "$b" "$a" || interleaved "$a" "$b"; then echo "INTERLEAVED"; return; fi
  n=$(diff <(sig "$b") <(sig "$a") | grep -c '^[<>]')
  echo "DIFF $n"
}

# ---------------------------------------------------------------------------
# One probe x mode: base arm(s), then the switch arms.
# ---------------------------------------------------------------------------
battery_one() {
  local p=$1 m=$2 d="$OUT/runs/$1.$2" i hs=- hsv=
  mkdir -p "$d"
  for ((i = 1; i <= BASERUNS; i++)); do run_cvm "$d/base$i.out" "$p" "$m"; done
  for ((i = 2; i <= BASERUNS; i++)); do
    if ! diff <(sig "$d/base1.out") <(sig "$d/base$i.out") > /dev/null; then
      emit "SWB $p $m UNSTABLE"; return
    fi
  done
  if [ "$WITH_HS" = 1 ]; then
    hs=$(hs_ref "$p")
    if diff <(sig "$hs") <(sig "$d/base1.out") > /dev/null; then hsv=" hs=SAME"; else hsv=" hs=DIFF"; fi
    if [ "$DIFFING_ONLY" = 1 ] && [ "$hsv" = " hs=SAME" ]; then
      emit "SWB $p $m BASE facts=$(grep -ac '^fact ' "$d/base1.out") $(grep -a '^rc=' "$d/base1.out" | tail -1)$hsv (skipped: -d)"
      return
    fi
  fi
  emit "SWB $p $m BASE facts=$(grep -ac '^fact ' "$d/base1.out") $(grep -a '^rc=' "$d/base1.out" | tail -1)$hsv"
  [ "$(grep -ac '^fact ' "$d/base1.out")" -gt 0 ] || emit "SWB $p $m NOFACTS (the base arm printed no fact line: every switch arm compares only rc)"
  if [ "$SMODE" = each ]; then
    for ((i = 0; i < NSW; i++)); do
      pool_wait
      (
        local f="$d/sw-${SW_KEY[$i]}.out"
        # shellcheck disable=SC2046
        run_cvm "$f" "$p" "$m" $(off_env "$i" $((i + 1)))
        emit "SWB $p $m ${SW_KEY[$i]}=${SW_WORD[$i]} $(verdict "$d/base1.out" "$f" "$hs")"
      ) &
    done
    wait
  else
    split_range "$p" "$m" "$d" "$hs" 0 "$NSW"
  fi
}

# Adaptive group testing over switches [lo, hi).
split_range() {
  local p=$1 m=$2 d=$3 hs=$4 lo=$5 hi=$6 f v mid
  f="$d/grp-$lo-$hi.out"
  # shellcheck disable=SC2046
  run_cvm "$f" "$p" "$m" $(off_env "$lo" "$hi")
  v=$(verdict "$d/base1.out" "$f" "$hs")
  if [ $((hi - lo)) -eq 1 ]; then
    emit "SWB $p $m ${SW_KEY[$lo]}=${SW_WORD[$lo]} $v"
    return
  fi
  echo "SWB-SPLIT $p $m [$lo,$hi) ${SW_KEY[$lo]}..${SW_KEY[$((hi - 1))]} $v" >> "$SUM"
  [ "$v" = SAME ] && return
  mid=$(( (lo + hi) / 2 ))
  local before
  before=$(grep -cE "^SWB $p $m (CRATONVM_|GROUP-ONLY)" "$SUM")
  # Each half that still differs is narrowed; a half that is SAME stops.
  split_range "$p" "$m" "$d" "$hs" "$lo" "$mid"
  split_range "$p" "$m" "$d" "$hs" "$mid" "$hi"
  # Did any leaf below this group differ on its own?
  if [ "$(grep -E "^SWB $p $m (CRATONVM_|GROUP-ONLY)" "$SUM" | tail -n +$((before + 1)) | grep -vc ' SAME$')" -eq 0 ]; then
    emit "SWB $p $m GROUP-ONLY [$lo,$hi) ${SW_KEY[$lo]}..${SW_KEY[$((hi - 1))]} $v (no single member differs alone: an interaction)"
  fi
}

# Each probe x mode is a task; in `each` mode the task itself fans out its
# switch runs, so tasks run one at a time. In `split` mode the task is
# sequential and tasks run in parallel.
for p in "${PROBES[@]}"; do
  # The HotSpot reference first, once per probe (parallel mode tasks share it).
  [ "$WITH_HS" = 1 ] && hs_ref "$p" > /dev/null
  for m in $MODES; do
    if [ "$SMODE" = split ]; then pool_wait; battery_one "$p" "$m" & else battery_one "$p" "$m"; fi
  done
done
wait

runs=$(find "$OUT/runs" -name '*.out' | wc -l | tr -d ' ')
ndiff=$(grep -E '^SWB .* CRATONVM_[A-Z0-9_]+=[^ ]* (DIFF|RC )' "$SUM" | wc -l | tr -d ' ')
nrest=$(grep -c ' RESTORES-HS$' "$SUM"); ntmo=$(grep -c ' TIMEOUT$' "$SUM"); nuns=$(grep -c ' UNSTABLE$' "$SUM")
ngrp=$(grep -c ' GROUP-ONLY ' "$SUM")
nil=$(grep -c ' INTERLEAVED$' "$SUM")
echo "SWB-DONE probes=${#PROBES[@]} runs=$runs diff=$ndiff restores=$nrest group_only=$ngrp timeout=$ntmo unstable=$nuns interleaved=$nil" | tee -a "$SUM"
[ $((ndiff + nrest + ngrp)) -eq 0 ]
