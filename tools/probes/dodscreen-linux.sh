#!/usr/bin/env bash
# dodscreen-linux.sh -- one arm of the roadmap §6 definition-of-done screen.
#
#   dodscreen-linux.sh <tag> <mode> <cpfile-or-cp> <MainClass> [args...]
#
#     mode = strict   cratonvm --jdk-only --explain-jdk-only --jdk-only-report
#            compat   cratonvm in its default mode, --jdk-only-report
#            hotspot  the real JDK -- the "runs to completion" oracle
#
# Linux sibling of `probes/dodscreen.sh` (Windows). Same three instrument traps,
# and a fourth that only a Linux runner meets:
#
#   * a dump/report flag placed AFTER the main class is silently ignored -- no
#     file, no warning, exit 0. So every VM flag here goes BEFORE `-cp`,
#     including whatever the caller passes in `DOD_JVM_ARGS`;
#   * the report is NOT written when the program calls System.exit, so every
#     driver returns normally from main -- and that is CHECKED, not assumed: a
#     missing report on an rc=0 run prints NO-REPORT-WRITTEN rather than letting
#     the arm read as clean;
#   * the Windows path-shape trap (`os error 3`) does not apply here, but the
#     stderr grep for it is kept because a wrong path fails the same way;
#   * `--Xmx` takes its size as a SEPARATE argument on this VM. `--Xmx2g` is
#     rejected during argument parsing with exit 2 and no report -- which looks
#     exactly like the System.exit case if you only read the report.
#
# Env: DOD_OUT DOD_JDK DOD_CVM DOD_TIMEOUT DOD_HEAP DOD_CP_EXTRA DOD_JVM_ARGS DOD_CWD
set -u
[ -r /data/toolchain/env.sh ] && . /data/toolchain/env.sh

TAG="$1"; MODE="$2"; CPARG="$3"; MAIN="$4"; shift 4

OUT="${DOD_OUT:-/data/dod-out}"
mkdir -p "$OUT"
JDK="${DOD_JDK:-/data/jdkimages/jdk25-linux/jdk-25.0.4+7}"
CVM="${DOD_CVM:-$PWD/target/release/cratonvm}"
TMO="${DOD_TIMEOUT:-1800}"
HEAP="${DOD_HEAP:-2g}"

if [ -f "$CPARG" ]; then CP="$(tr -d '\n' < "$CPARG")"; else CP="$CPARG"; fi
[ -n "${DOD_CP_EXTRA:-}" ] && CP="$CP:$DOD_CP_EXTRA"

REP="$OUT/rep-$TAG-$MODE.json"
LOG="$OUT/run-$TAG-$MODE.out"
ERR="$OUT/run-$TAG-$MODE.err"
rm -f "$REP" "$LOG" "$ERR"

# The supported spelling; setting CRATONVM_DISABLE_DEFAULT_WATCHDOG directly
# still works but prints a deprecation line into every arm's stderr.
export CRATONVM_THREADS=-default-watchdog

EXTRA=()
if [ -n "${DOD_JVM_ARGS:-}" ]; then read -r -a EXTRA <<< "$DOD_JVM_ARGS"; fi

case "$MODE" in
  strict)
    set -- "$CVM" --java-home "$JDK" --Xmx "$HEAP" --jdk-only --explain-jdk-only \
        --jdk-only-report "$REP" ${EXTRA[@]+"${EXTRA[@]}"} -cp "$CP" "$MAIN" "$@" ;;
  compat)
    set -- "$CVM" --java-home "$JDK" --Xmx "$HEAP" \
        --jdk-only-report "$REP" ${EXTRA[@]+"${EXTRA[@]}"} -cp "$CP" "$MAIN" "$@" ;;
  hotspot)
    set -- "$JDK/bin/java" "-Xmx$HEAP" ${EXTRA[@]+"${EXTRA[@]}"} -cp "$CP" "$MAIN" "$@" ;;
  *) echo "unknown mode $MODE" >&2; exit 2 ;;
esac

echo "DODCMD $*" > "$OUT/cmd-$TAG-$MODE.txt"

# Several Tomcat tests open resources through bare relative paths that resolve
# against the JVM's CWD, not against -Dtomcat.test.basedir.
if [ -n "${DOD_CWD:-}" ]; then cd "$DOD_CWD" || exit 2; fi

start=$(date +%s)
timeout "$TMO" "$@" > "$LOG" 2> "$ERR"
rc=$?
elapsed=$(( $(date +%s) - start ))

# The row count is printed on every arm, before anyone reads a diff: a run that
# died partway produces a short file whose missing tail `diff` reports as
# ordinary `<` lines.
result="$(grep -a '^DOD RESULT' "$LOG" | tail -1)"
echo "DODRUN tag=$TAG mode=$MODE rc=$rc secs=$elapsed lines=$(wc -l < "$LOG") ${result:-NO-DOD-RESULT-LINE}"
if [ "$MODE" != "hotspot" ]; then
  if [ -f "$REP" ]; then
    echo "DODREP $REP $(wc -c < "$REP") bytes"
  else
    echo "DODREP NO-REPORT-WRITTEN -- flag order, argument parsing, or System.exit"
    head -3 "$ERR"
  fi
fi
exit 0
