#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# jit-xint-gate.sh -- CONF21-3, the conformance gate with a second oracle.
#
# Compares every probe's CratonVM output with HotSpot 25 run twice:
#   default  java -XX:-OmitStackTraceInFastThrow <probe>
#   xint     java -XX:-OmitStackTraceInFastThrow -Xint <probe>
# Both oracle runs keep stack traces on hot implicit exceptions: without
# -XX:-OmitStackTraceInFastThrow, C2's fast throw drops the message of an
# exception thrown often enough, and the oracle's own line becomes a C2
# artefact (two round-21 probe DIFFs were exactly that).
#
# Rule, per output line (paired by position):
#   CratonVM == default                       -> ok
#   CratonVM == -Xint != default, and the fact is listed in the tier-made
#   allow file for this probe                 -> TIER-OK (accepted)
#   CratonVM == -Xint != default, not listed  -> XINT-ONLY (not accepted: a
#                                                candidate for the allow file,
#                                                or a real difference)
#   anything else                             -> DIFF
# Lines where the two oracle runs disagree but CratonVM equals default are
# counted as `tiered` (informational: answers C2 makes that the gate does not
# need to forgive yet).
#
# Usage:
#   jit-xint-gate.sh -b <cratonvm binary> -j <JDK home> -c <class dir> [options]
#
# Required:
#   -b BIN      CratonVM launcher binary.
#   -j JDK      JDK 25 home: --java-home of CratonVM and the HotSpot oracle.
#   -c DIR      Compiled probe classes (top-level classes; `X$Y` skipped).
# Options:
#   -p GLOB     Probe class-name glob(s), `|`-separated (default `*`).
#   -m MODES    CratonVM arms (default `jo co`): jo = --jdk-only (launcher
#               default), co = --compatible, nojit = --nojit.
#   -l FILE     Tier-made allow file (default jit-fuzz/tier-made.txt next to
#               this script): `<probe> <fact-name>` rows; a name ending in `*`
#               is a prefix match; `#` comments.
#   -N          Compare every normalised output line (conf20's rule: drop
#               `time ` lines, `[...]` log lines, warnings and ns/op lines;
#               `@<hex>` -> `@HASH`), not only `fact` lines. The exit code is
#               compared in both modes.
#   -e ENVS     `K=V` words for every CratonVM arm.
#   -a ARGS     VM args for every run (default: the round's --add-opens /
#               --add-exports / --enable-native-access set).
#   -t SECS     Per-run timeout (default 600; HotSpot -Xint gets 4x).
#   -J N        Parallel probes (default 2).
#   -o DIR      Output directory (default ./xint-gate-out). HotSpot outputs are
#               cached in <out>/hs and reused by later runs into the same dir.
#   -F          Re-run HotSpot even when a cached output exists.
#
# Output (stdout: non-PASS lines; <out>/summary.txt: all; <out>/runs/<p>.<m>.gate:
# the per-line detail):
#   XG <probe> <mode> PASS [tiered=<n>]
#   XG <probe> <mode> PASS-TIER tier_ok=<n> [tiered=<n>]
#   XG <probe> <mode> XINT-ONLY <n> (names...)
#   XG <probe> <mode> DIFF <n> [xint_only=<n>] (first differing line)
#   XG <probe> <mode> INTERLEAVED ... was: <verdict>   (the "missing" fact still
#       appears inside a raw output line: a split line, not a verdict; rerun)
#   XG-DONE probes=<n> pass=<n> pass_tier=<n> xint_only=<n> diff=<n> interleaved=<n>
# Exit: 0 when every probe is PASS / PASS-TIER, 1 otherwise, 2 usage.
#
# Example (host):
#   regression-suite/jit-xint-gate.sh -b /data/jr20/bin/cratonvm-j21-w14 \
#     -j /data/jdkimages/jdk25-linux/jdk-25.0.4+7 -c /data/jr20/cls21 \
#     -o /data/jr20/out/xg21-w14

set -u

HERE=$(cd "$(dirname "$0")" && pwd)
BIN= JDK= CLS= PGLOB='*' MODES='jo co' ALLOWF="$HERE/jit-fuzz/tier-made.txt" NORM=0 ENVS=
TMO=600 JOBS=2 OUT=./xint-gate-out REFRESH=0
VMARGS='--add-opens java.base/java.lang=ALL-UNNAMED --add-exports java.base/jdk.internal.misc=ALL-UNNAMED --enable-native-access=ALL-UNNAMED'

usage() { sed -n '2,/^set -u/p' "$0" | sed -e '/^set -u/d' -e 's/^# \{0,1\}//'; exit 2; }

while getopts "b:j:c:p:m:l:Ne:a:t:J:o:Fh" opt; do
  case $opt in
    b) BIN=$OPTARG ;; j) JDK=$OPTARG ;; c) CLS=$OPTARG ;; p) PGLOB=$OPTARG ;; m) MODES=$OPTARG ;;
    l) ALLOWF=$OPTARG ;; N) NORM=1 ;; e) ENVS=$OPTARG ;; a) VMARGS=$OPTARG ;; t) TMO=$OPTARG ;;
    J) JOBS=$OPTARG ;; o) OUT=$OPTARG ;; F) REFRESH=1 ;; h|*) usage ;;
  esac
done
[ -n "$BIN" ] && [ -n "$JDK" ] && [ -n "$CLS" ] || usage
[ -x "$BIN" ] || { echo "no executable binary: $BIN" >&2; exit 2; }
[ -x "$JDK/bin/java" ] || { echo "no JDK at $JDK" >&2; exit 2; }
[ -d "$CLS" ] || { echo "no class dir: $CLS" >&2; exit 2; }
[ -f "$ALLOWF" ] || { echo "no allow file: $ALLOWF (pass -l /dev/null for none)" >&2; exit 2; }
if env | grep -q '^CRATONVM_'; then
  echo "WARNING: CRATONVM_* variables are set in the environment and apply to every CratonVM run:" >&2
  env | grep '^CRATONVM_' | sed 's/^/  /' >&2
fi
mkdir -p "$OUT/hs" "$OUT/runs"
SUM="$OUT/summary.txt"; : > "$SUM"

mode_args() { case $1 in jo) echo "" ;; co) echo "--compatible" ;; nojit) echo "--nojit" ;; *) echo "$1" ;; esac; }

# Every run writes stdout to <out> (plus "rc=<n>") and stderr to <out>.err.
# Facts are read from stdout only: with both streams in one file a diagnostic
# line (a CRATONVM_DBG token in -e, a deprecation line) can split a fact line
# mid-line. -N compares stdout's normalised lines followed by stderr's.
extract() {
  if [ "$NORM" = 1 ]; then
    cat "$1" "$1.err" 2>/dev/null | grep -av -E '^time |^\[|WARNING|^warning|ns/op|^rc=' \
      | sed -E 's/\x1b\[[0-9;]*m//g; s/@[0-9a-f]{4,16}/@HASH/g'
  else
    grep -a '^fact ' "$1"
  fi
  grep -a '^rc=' "$1" | tail -1
}
sig() { grep -a -E '^(fact |rc=)' "$1"; }
# interleaved <ref> <arm>: every fact line of <ref> missing from <arm>'s facts
# still occurs inside a raw line of <arm> (stdout or stderr), followed by the
# end of the line or a character that cannot continue a value: a split line,
# not a verdict.
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

run_hs() { # <out> <class> [flags]
  local out=$1 c=$2; shift 2
  [ "$REFRESH" = 0 ] && [ -s "$out" ] && return
  # shellcheck disable=SC2086
  timeout -k 10 "$TMO" "$JDK/bin/java" -XX:-OmitStackTraceInFastThrow "$@" $VMARGS -cp "$CLS" "$c" \
    > "$out" 2> "$out.err" < /dev/null
  echo "rc=$?" >> "$out"
}
run_cvm() { # <out> <class> <mode>
  # shellcheck disable=SC2086
  env $ENVS timeout -k 10 "$TMO" "$BIN" --java-home "$JDK" $(mode_args "$3") $VMARGS --cp "$CLS" "$2" \
    > "$1" 2> "$1.err" < /dev/null
  echo "rc=$?" >> "$1"
}

# gate <probe> <hs file> <xint file> <cvm file> <detail out> -> one verdict line
gate() {
  local p=$1 d=$5
  extract "$2" > "$d.h"; extract "$3" > "$d.x"; extract "$4" > "$d.c"
  # Pad to the longest so `paste` pairs every line (a missing line is `<missing>`).
  local n nh nx nc
  nh=$(wc -l < "$d.h"); nx=$(wc -l < "$d.x"); nc=$(wc -l < "$d.c")
  n=$nh; [ "$nx" -gt "$n" ] && n=$nx; [ "$nc" -gt "$n" ] && n=$nc
  for f in "$d.h" "$d.x" "$d.c"; do
    while [ "$(wc -l < "$f")" -lt "$n" ]; do echo '<missing>' >> "$f"; done
  done
  # A literal 0x01 separator (paste reads `\0` in -d as "no delimiter").
  local sep=$'\001'
  paste -d "$sep" "$d.h" "$d.x" "$d.c" | awk -F "$sep" -v probe="$p" -v allow="$ALLOWF" -v detail="$d" '
    BEGIN {
      np = 0
      while ((getline l < allow) > 0) {
        if (l ~ /^[ \t]*#/ || l ~ /^[ \t]*$/) continue
        nw = split(l, w, /[ \t]+/)
        if (w[1] == "") { k = 2 } else { k = 1 }
        if (w[k] == probe && nw >= k + 1) pats[++np] = w[k + 1]
      }
    }
    function allowed(name,   i, pt) {
      for (i = 1; i <= np; i++) {
        pt = pats[i]
        if (substr(pt, length(pt), 1) == "*") {
          if (index(name, substr(pt, 1, length(pt) - 1)) == 1) return 1
        } else if (name == pt) return 1
      }
      return 0
    }
    {
      h = $1; x = $2; c = $3; name = ""
      if (c ~ /^fact /) { split(c, cw, " "); name = cw[2] }
      else if (h ~ /^fact /) { split(h, hw, " "); name = hw[2] }
      else if (c ~ /^rc=/ || h ~ /^rc=/) name = "rc"
      if (c == h) {
        if (h != x) { tiered++; print "TIERED  " NR ": default=[" h "] xint=[" x "]" > detail }
        next
      }
      if (c == x && name != "" && allowed(name)) { tier++; print "TIER-OK " NR ": " name " cvm=xint=[" c "] default=[" h "]" > detail; next }
      if (c == x) { xo++; xon = xon " " name; print "XINT-ONLY " NR ": " name " cvm=xint=[" c "] default=[" h "]" > detail; next }
      df++; if (first == "") first = "line " NR ": cvm=[" c "] default=[" h "]"
      print "DIFF    " NR ": cvm=[" c "] default=[" h "] xint=[" x "]" > detail
    }
    END {
      t = (tiered > 0) ? " tiered=" tiered : ""
      if (df > 0) { print "DIFF " df (xo > 0 ? " xint_only=" xo : "") " (" substr(first, 1, 200) ")"; exit }
      if (xo > 0) { print "XINT-ONLY " xo " (" substr(xon, 2) ")" t; exit }
      if (tier > 0) { print "PASS-TIER tier_ok=" tier t; exit }
      print "PASS" t
    }'
}

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
  local p=$1 m v
  run_hs "$OUT/hs/$p.default.out" "$p"
  TMO=$((TMO * 4)) run_hs "$OUT/hs/$p.xint.out" "$p" -Xint
  for m in $MODES; do
    run_cvm "$OUT/runs/$p.$m.out" "$p" "$m"
    v=$(gate "$p" "$OUT/hs/$p.default.out" "$OUT/hs/$p.xint.out" "$OUT/runs/$p.$m.out" "$OUT/runs/$p.$m.gate")
    case $v in
      DIFF*|XINT-ONLY*)
        if interleaved "$OUT/hs/$p.default.out" "$OUT/runs/$p.$m.out" \
          || interleaved "$OUT/hs/$p.xint.out" "$OUT/runs/$p.$m.out"; then
          v="INTERLEAVED (a fact line is split by another output line; rerun) was: $v"
        fi ;;
    esac
    echo "XG $p $m $v" >> "$SUM"
    case $v in PASS*) ;; *) echo "XG $p $m $v" ;; esac
  done
}
pool_wait() { while [ "$(jobs -rp | wc -l)" -ge "$JOBS" ]; do wait -n 2>/dev/null || sleep 0.2; done; }
for p in "${PROBES[@]}"; do pool_wait; one "$p" & done
wait

npass=$(grep -cE '^XG [^ ]+ [^ ]+ PASS( |$)' "$SUM"); ntier=$(grep -c ' PASS-TIER ' "$SUM")
nxo=$(grep -cE '^XG [^ ]+ [^ ]+ XINT-ONLY ' "$SUM"); ndiff=$(grep -cE '^XG [^ ]+ [^ ]+ DIFF ' "$SUM")
nil=$(grep -cE '^XG [^ ]+ [^ ]+ INTERLEAVED ' "$SUM")
echo "XG-DONE probes=${#PROBES[@]} modes='$MODES' pass=$npass pass_tier=$ntier xint_only=$nxo diff=$ndiff interleaved=$nil" | tee -a "$SUM"
[ $((nxo + ndiff)) -eq 0 ]
