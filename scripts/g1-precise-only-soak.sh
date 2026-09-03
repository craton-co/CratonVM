#!/usr/bin/env bash
# Precise-only-roots soak.
#
# Two arms, because they answer different questions and neither answers both:
#
#   ORACLE  — `CRATONVM_DBG_VERIFY_OOP_MAPS=1` alongside the two precise-only
#             switches. In this mode `verify_active_coverage_into` runs the FULL
#             conservative scan anyway and only asks whether the precise maps
#             missed anything, so the suppression never actually fires. It is a
#             pure correctness experiment: `never_mapped(while_covered=N)` is
#             the number, and any N > 0 refutes the `fully_oop_covered`
#             presence bit on code this workload exercised.
#
#   LIVE    — the same switches WITHOUT the oracle, so the suppression really
#             happens and G1's pin set really goes empty. Checks the behaviour
#             that mode produces: checksum, dangling references, exit code.
#
# A clean soak needs BOTH: ORACLE says the maps are complete, LIVE says acting
# on that does not break anything.
#
# Usage: soak.sh <cratonvm.exe> <reps> <tag>
set -u
S="$(dirname "$0")"; P="$S/probes"
CV="$1"; REPS="${2:-5}"; TAG="${3:-soak}"
OUT="$S/soak-$TAG"; mkdir -p "$OUT"

PRECISE="CRATONVM_GC_PRECISE_ONLY_ROOTS=1 CRATONVM_G1_PRECISE_ONLY_ROOTS=1"

# name | expected checksum | vm args (class + args last)
WORKLOADS=(
  "hchurn|249707433568|-Xmx160m -cp $P HumongousChurn 48 6000 512"
  "hchurn-tight|262316478568|-Xmx160m -cp $P HumongousChurn 48 20000 512"
  "card|7616601600|-Xmx24m -cp $P G1CardChurn 11 60"
  "churn|111889612800|-Xmx256m -cp $P G1ChurnPauseProbe 24 200"
  "hum|266925450|-Xmx48m -cp $P HumongousHold 300"
  "humwide|434688967096|-Xmx512m -cp $P HumongousWide 64 400"
)
GCS=("-XX:+UseG1GC" "-XX:+UseGenerationalGC")

fail=0
for rep in $(seq 1 "$REPS"); do
  for gc in "${GCS[@]}"; do
    for w in "${WORKLOADS[@]}"; do
      name="${w%%|*}"; rest="${w#*|}"; want="${rest%%|*}"; args="${rest#*|}"
      gctag=$(echo "$gc" | tr -d '-:+')
      for mode in oracle live; do
        extra=""
        [ "$mode" = oracle ] && extra="CRATONVM_DBG_VERIFY_OOP_MAPS=1"
        f="$OUT/$name-$gctag-$mode-$rep"
        # shellcheck disable=SC2086
        env $PRECISE $extra "$CV" $gc --verbose:gc $args > "$f.out" 2> "$f.err"
        rc=$?
        got=$(grep -ohE 'checksum=[0-9]+' "$f.out" | tail -1 | cut -d= -f2)
        # while_covered=N is the refutation count; 0 is the only clean answer.
        wc_=$(grep -ohE 'while_covered=[0-9]+' "$f.err" | tail -1 | cut -d= -f2)
        wm=$(grep -ohE 'wrong_map=[0-9]+' "$f.err" | tail -1 | cut -d= -f2)
        dang=$(grep -ohE 'dangling=[0-9]+' "$f.err" | tail -1 | cut -d= -f2)
        pins=$(grep -ohE 'pin_addrs=[0-9]+' "$f.err" | awk -F= '{s+=$2} END{print s+0}')
        crash=$(grep -cE 'SIGSEGV|EXCEPTION_ACCESS|panicked|FATAL' "$f.err")
        bad=""
        [ "$rc" != 0 ] && bad="$bad rc"
        [ -n "$want" ] && [ "$got" != "$want" ] && bad="$bad checksum"
        [ -n "${wc_:-}" ] && [ "${wc_:-0}" != 0 ] && bad="$bad ORACLE-REFUTED"
        [ -n "${wm:-}" ] && [ "${wm:-0}" != 0 ] && bad="$bad WRONG-MAP"
        [ -n "${dang:-}" ] && [ "${dang:-0}" != 0 ] && bad="$bad dangling"
        [ "$crash" != 0 ] && bad="$bad crash-marker"
        if [ -n "$bad" ]; then fail=$((fail+1)); fi
        echo "$name $gctag $mode #$rep rc=$rc sum=${got:-none} while_covered=${wc_:-n/a} wrong_map=${wm:-n/a} dangling=${dang:-n/a} pins=${pins:-0} ${bad:+BAD:$bad}"
      done
    done
  done
done
echo "SOAK $TAG: failures=$fail"
