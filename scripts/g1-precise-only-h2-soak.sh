#!/usr/bin/env bash
# Precise-only-roots soak on a REAL APPLICATION: H2 and its own test suite.
#
# §16.3 left the question as sample size — six probes over 14 compiled frames is
# far too small to license a default. This runs the same two questions over H2
# test classes, which exercise a real database engine.
#
# Per class, three arms:
#   ORACLE — precise-only switches + `CRATONVM_DBG_VERIFY_OOP_MAPS=1`. The
#            suppression never fires (the gate runs the full scan anyway); this
#            is the correctness reading. The numbers that matter are the
#            VERIFIER columns, not the raw counters: a word that merely looks
#            like a heap address is dead storage the precise map is right to
#            omit (audit §16).
#   LIVE   — precise-only switches only, so the suppression really fires.
#   CTRL   — neither switch: the baseline the LIVE arm must match.
#
# Usage: h2soak.sh <cratonvm.exe> <classlist-file> [timeout-secs]
set -u
S="$(dirname "$0")"
CV="$1"; LIST="$2"; TMO="${3:-240}"
CP=$(cat /c/craton/h2corpus/cp.txt)
OUT="$S/h2out"; mkdir -p "$OUT"

printf '%-42s %-6s %-4s %7s %7s %8s %8s %8s %8s %s\n' \
  CLASS ARM RC FRAMES WORDS NEVERMAP V_OOP WRONGMAP WM_VOOP NOTE

while read -r cls; do
  [ -z "$cls" ] && continue
  case "$cls" in \#*) continue;; esac
  short="${cls##*.}"
  for arm in ctrl live oracle; do
    E=""
    case "$arm" in
      live)   E="CRATONVM_GC_PRECISE_ONLY_ROOTS=1 CRATONVM_G1_PRECISE_ONLY_ROOTS=1";;
      oracle) E="CRATONVM_GC_PRECISE_ONLY_ROOTS=1 CRATONVM_G1_PRECISE_ONLY_ROOTS=1 CRATONVM_DBG_VERIFY_OOP_MAPS=1";;
    esac
    f="$OUT/$short-$arm"
    # shellcheck disable=SC2086
    timeout "$TMO" env $E "$CV" -Xmx1g -XX:+UseG1GC --verbose:gc -cp "$CP" "$cls" \
      > "$f.out" 2> "$f.err"
    rc=$?
    line=$(grep -ohE 'oop-map audit: frames=[0-9]+ unreadable_frames=[0-9]+ words=[0-9]+ never_mapped=[0-9]+ \(while_covered=[0-9]+' "$f.err" | tail -1)
    fr=$(echo "$line"  | grep -oE 'frames=[0-9]+'       | head -1 | cut -d= -f2)
    wd=$(echo "$line"  | grep -oE 'words=[0-9]+'        | head -1 | cut -d= -f2)
    nm=$(echo "$line"  | grep -oE 'never_mapped=[0-9]+' | head -1 | cut -d= -f2)
    vo=$(grep -ohE 'verifier_oop=[0-9]+' "$f.err" | head -1 | cut -d= -f2)
    wm=$(grep -ohE 'wrong_map=[0-9]+'    "$f.err" | tail -1 | cut -d= -f2)
    wv=$(grep -ohE 'wrong_map=[0-9]+ +\(verifier_oop=[0-9]+' "$f.err" | tail -1 | grep -oE 'verifier_oop=[0-9]+' | cut -d= -f2)
    note=""
    [ "$rc" = 124 ] && note="TIMEOUT"
    [ "$rc" != 0 ] && [ "$rc" != 124 ] && note="rc=$rc"
    grep -qE 'SIGSEGV|EXCEPTION_ACCESS|panicked' "$f.err" && note="$note CRASH"
    # The refutations that count are the VERIFIER-confirmed ones.
    [ -n "${vo:-}" ] && [ "${vo:-0}" != 0 ] && note="$note VERIFIED-NEVERMAP"
    [ -n "${wv:-}" ] && [ "${wv:-0}" != 0 ] && note="$note VERIFIED-WRONGMAP"
    printf '%-42s %-6s %-4s %7s %7s %8s %8s %8s %8s %s\n' \
      "$short" "$arm" "$rc" "${fr:--}" "${wd:--}" "${nm:--}" "${vo:--}" "${wm:--}" "${wv:--}" "$note"
  done
  # The LIVE arm must produce what the CTRL arm produced.
  if ! diff -q "$OUT/$short-ctrl.out" "$OUT/$short-live.out" >/dev/null 2>&1; then
    printf '%-42s %-6s %s\n' "$short" "DIFF" "LIVE output differs from CTRL"
  fi
done < "$LIST"
