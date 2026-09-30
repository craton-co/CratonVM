#!/bin/bash
# P4-A: run a CORPUS under `--jdk-only`, with a census per vector.
#
# The roadmap's Phase 4 says "no corpus has yet been run under `--jdk-only`,
# which is the mode this campaign is named for". This is that run, on H2 --
# 218 test classes, one of the three definition-of-done workloads.
#
# THE TRAP THIS EXISTS TO AVOID, from HANDOFF-20260828-L7 §3: the regression
# suite writes its own census to a PID-scoped directory and DELETES it at the
# end, and passing your own `--jdk-only-report` makes the suite skip its census
# and point every vector at ONE path -- so the last vector overwrites all of
# them and the "corpus census" is a single class's. Every vector here gets its
# own report path, and the union is computed afterwards from the files.
#
# Both arms run the same class list with the same shard layout, so host load --
# which is real, this box carries other lanes -- moves both alike. Timeouts are
# reported as NOT MEASURED rather than as failures: a class capped at the limit
# is one whose verdict this run does not have.
set -u
source /data/toolchain/env.sh

MODE="${1:?usage: corpus.sh <strict|hotspot> <shard_idx> <shard_count>}"
IDX="${2:-0}"
N="${3:-1}"

# Every path is overridable; the defaults are azure-host-2's layout, which is
# where the three definition-of-done corpora are checked out.
H2="${H2:-/data/cratonvm/apps/h2database/h2}"
LIST="${LIST:-/data/cratonvm/apps/h2database-suite-runner/meta/all-classes.tsv}"
J="${JDK:-/data/jdkimages/jdk25-linux/jdk-25.0.4+7}"
CVM="${CVM:-$PWD/target/release/cratonvm}"
OUT="${OUT:-/data/corpus}/$MODE"
TMO="${TMO:-90}"

mkdir -p "$OUT/rep" "$OUT/log" "$OUT/wd"
export CRATONVM_THREADS=-default-watchdog

CP="$(tr -d '\n' < "$H2/craton-testcp.txt"):$H2/target/classes:$H2/target/test-classes"

i=0
while IFS= read -r cls; do
  [ -z "$cls" ] && continue
  if [ $(( i % N )) -ne "$IDX" ]; then i=$((i+1)); continue; fi
  i=$((i+1))
  wd="$OUT/wd/$cls"; rm -rf "$wd"; mkdir -p "$wd"
  rep="$OUT/rep/$cls.json"
  start=$(date +%s)
  if [ "$MODE" = strict ]; then
    ( cd "$wd" && timeout "$TMO" "$CVM" --java-home "$J" --Xmx 1g \
        --jdk-only --explain-jdk-only --jdk-only-report "$rep" \
        -cp "$CP" "$cls" ) > "$OUT/log/$cls.out" 2> "$OUT/log/$cls.err"
  else
    ( cd "$wd" && timeout "$TMO" "$J/bin/java" -Xmx1g -cp "$CP" "$cls" ) \
        > "$OUT/log/$cls.out" 2> "$OUT/log/$cls.err"
  fi
  rc=$?
  secs=$(( $(date +%s) - start ))
  case $rc in
    0)   v=PASS ;;
    124) v=TIMEOUT ;;
    *)   v=FAIL ;;
  esac
  # A report is expected for every strict vector that did not time out. Its
  # ABSENCE is a result too -- it is what a System.exit or a flag-order mistake
  # looks like -- so say so rather than letting the union quietly shrink.
  repnote=""
  if [ "$MODE" = strict ] && [ "$v" != TIMEOUT ] && [ ! -f "$rep" ]; then
    repnote=" NO-REPORT"
  fi
  printf 'CV %-52s %-8s rc=%-4s %4ss%s\n' "$cls" "$v" "$rc" "$secs" "$repnote"
  rm -rf "$wd"
done < "$LIST"
echo "SHARD-DONE $MODE idx=$IDX $(date -Is)"
