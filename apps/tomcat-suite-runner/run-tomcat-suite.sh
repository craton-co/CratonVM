#!/usr/bin/env bash
###############################################################################
# run-tomcat-suite.sh - Apache Tomcat JUnit test suite driver for CratonVM
#
# Linux counterpart to the Windows apps/tomcat-suite-runner/run-tomcat-suite.ps1
# harness (local, not git-tracked) - one process per test class,
# org.junit.runner.JUnitCore <class>, sharded N ways for concurrency, with a
# resumable per-shard results.csv (append-only, already-recorded classes are
# skipped on re-invocation).
#
# Prerequisites (not automated here - see run-tomcat-suite.md):
#   - a Tomcat checkout with compiled test classes under
#     $TC_ROOT/output/testclasses and a full CATALINA_BASE under
#     $TC_ROOT/output/build (conf/, webapps/, etc. - `ant deploy` +
#     `ant test-compile`, or reuse an existing fixture).
#   - a flat classpath file listing every jar + the compiled classes dirs.
#   - a newline-separated class-name list (one FQCN per line, no CRLF).
#
# Usage:
#   ./run-tomcat-suite.sh craton   <shard_idx> <shard_count> <run_name> [classlist]
#   ./run-tomcat-suite.sh hotspot  <shard_idx> <shard_count> <run_name> [classlist]
#
# Env overrides:
#   TC_ROOT       Tomcat checkout root (default: /data/data/apps/tomcat)
#   CP_FILE       classpath file (default: $TC_ROOT/.suite/cp-linux-fixed.txt)
#   HAMCREST_JAR  hamcrest jar to append to CP_FILE's classpath (JUnit4 needs it
#                 separately on some builds)
#   CRATONVM_EXE  cratonvm binary under test (craton mode only)
#   JAVA_HOME25   real JDK used both as CratonVM's --java-home and as the
#                 HotSpot baseline binary
#   CLASSLIST     default class-list file if not passed positionally
#   TIMEOUT_SEC   per-class hang timeout (default: 300)
#   MAX_HEAP      -Xmx / --Xmx (default: 2g)
#
# Example - all 6 shards of a craton run:
#   for i in 0 1 2 3 4 5; do
#     ./run-tomcat-suite.sh craton $i 6 full-suite-20260721 &
#   done; wait
###############################################################################
set -u

MODE="$1"          # craton | hotspot
SHARD_IDX="$2"
SHARD_COUNT="$3"
RUN_NAME="$4"

TC_ROOT="${TC_ROOT:-/data/data/apps/tomcat}"
CP_FILE="${CP_FILE:-$TC_ROOT/.suite/cp-linux-fixed.txt}"
HAMCREST_JAR="${HAMCREST_JAR:-/home/victor/tomcat-build-libs/hamcrest-3.0/hamcrest-3.0.jar}"
JAVA_HOME25="${JAVA_HOME25:-/home/victor/jdk25}"
CLASSLIST="${5:-${CLASSLIST:-$TC_ROOT/.suite/all-tests.txt}}"
TIMEOUT_SEC="${TIMEOUT_SEC:-300}"
MAX_HEAP="${MAX_HEAP:-2g}"

OUTDIR="$TC_ROOT/.suite/results/$RUN_NAME/shard-$SHARD_IDX"
mkdir -p "$OUTDIR"
CP="$(cat "$CP_FILE"):$HAMCREST_JAR"

RESULTS="$OUTDIR/results.csv"
touch "$RESULTS"

# Already-recorded classes are skipped (resumable across restarts/timeouts).
declare -A DONE
if [ -s "$RESULTS" ]; then
  while IFS=, read -r cls _rc _secs _status; do
    DONE["$cls"]=1
  done < "$RESULTS"
fi

if [ "$MODE" = "craton" ]; then
  CRATONVM_EXE="${CRATONVM_EXE:?set CRATONVM_EXE to the cratonvm binary under test}"
  export CRATONVM_REAL_NET_SOCKETS=1
  export CRATONVM_REAL_AQS=1
  export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
  export CRATONVM_ROOTSNAP_CACHE=1
fi

run_one() {
  local cls="$1" logfile="$2"
  if [ "$MODE" = "craton" ]; then
    timeout "${TIMEOUT_SEC}s" "$CRATONVM_EXE" \
      --java-home "$JAVA_HOME25" --Xmx "$MAX_HEAP" \
      -Dfile.encoding=UTF-8 -Djava.net.preferIPv4Stack=true \
      -Dtomcat.test.basedir="$TC_ROOT/output/build" \
      -Dtomcat.test.temp="$TC_ROOT/output/test-tmp" \
      -Dtomcat.test.tomcatbuild="$TC_ROOT/output/build" \
      -Dtomcat.test.relaxTiming=true \
      --add-opens java.base/java.lang=ALL-UNNAMED \
      --add-opens java.base/java.io=ALL-UNNAMED \
      --add-opens java.base/java.util=ALL-UNNAMED \
      --add-opens java.base/java.util.concurrent=ALL-UNNAMED \
      -c "$CP" org.junit.runner.JUnitCore "$cls" > "$logfile" 2>&1
  else
    timeout "${TIMEOUT_SEC}s" "$JAVA_HOME25/bin/java" \
      -Xmx"$MAX_HEAP" -Dfile.encoding=UTF-8 -Djava.net.preferIPv4Stack=true \
      -Dtomcat.test.basedir="$TC_ROOT/output/build" \
      -Dtomcat.test.temp="$TC_ROOT/output/test-tmp" \
      -Dtomcat.test.tomcatbuild="$TC_ROOT/output/build" \
      -Dtomcat.test.relaxTiming=true \
      --add-opens java.base/java.lang=ALL-UNNAMED \
      --add-opens java.base/java.io=ALL-UNNAMED \
      --add-opens java.base/java.util=ALL-UNNAMED \
      --add-opens java.base/java.util.concurrent=ALL-UNNAMED \
      -cp "$CP" org.junit.runner.JUnitCore "$cls" > "$logfile" 2>&1
  fi
}

mkdir -p "$TC_ROOT/output/test-tmp"

# Ant's <junit> task runs with dir="." (its own basedir == the checkout root),
# and a lot of TomcatBaseTest-derived tests open resources via bare relative
# paths like new File("test/webapp") that resolve against the JVM's actual
# CWD at launch - NOT against -Dtomcat.test.basedir. Without this cd, every
# such class fails with FileNotFoundException/NoSuchFileException regardless
# of which VM runs it (found 2026-07-24: ~107 of a 172-class "fixture gap"
# bucket were actually this bug, not real environment gaps).
cd "$TC_ROOT" || { echo "cannot cd to TC_ROOT=$TC_ROOT" >&2; exit 1; }

idx=0
while IFS= read -r cls; do
  [ -z "$cls" ] && continue
  mod=$(( idx % SHARD_COUNT ))
  idx=$(( idx + 1 ))
  [ "$mod" -ne "$SHARD_IDX" ] && continue
  [ -n "${DONE[$cls]:-}" ] && continue

  logfile="$OUTDIR/$cls.log"
  start=$(date +%s)
  run_one "$cls" "$logfile"
  rc=$?
  end=$(date +%s)
  secs=$(( end - start ))

  if [ "$rc" -eq 124 ]; then
    status=HANG
  elif grep -qE "panicked at|SIGSEGV|SIGABRT|core dumped|thread '.*' panicked" "$logfile"; then
    status=CRASH
  elif grep -q "^OK (" "$logfile"; then
    status=PASS
  elif grep -q "FAILURES!!!" "$logfile"; then
    status=FAIL
  else
    status=NOSUMMARY
  fi

  echo "$cls,$rc,$secs,$status" >> "$RESULTS"
  # keep full logs only for non-PASS classes, to save disk on long runs
  [ "$status" = "PASS" ] && rm -f "$logfile"
done < "$CLASSLIST"

echo "shard $SHARD_IDX ($MODE) done" > "$OUTDIR/DONE"
