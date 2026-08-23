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
# results.csv columns: class,rc,seconds,status,loadavg1
#
# CHOOSING <shard_count>: this is a timing harness, so shard_count is part of
# the measurement, not just a speed knob. One shard occupies one core for the
# whole sweep; a class capped at TIMEOUT_SEC is recorded HANG whether it is
# stuck or merely starved. On a shared box, count the cores you actually have
# and leave the neighbours theirs. Measured 2026-08-23 on an 8-core host with
# ~3 cores already busy: 4 shards put twelve healthy classes over a 300 s cap
# and made one timing-sensitive class FAIL, for a 15-class phantom regression
# against the same binary. The loadavg1 column exists so that is visible from
# the results file alone.
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
#   HTTPD_PATH    Apache httpd binary for org.apache.tomcat.integration.httpd.*
#                 (default: whatever `command -v httpd` finds). Debian names it
#                 apache2, so on Debian either symlink it onto PATH as httpd or
#                 set HTTPD_PATH=/usr/sbin/apache2.
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
HTTPD_PATH="${HTTPD_PATH:-$(command -v httpd 2>/dev/null || true)}"

# org.apache.tomcat.integration.httpd.* proxies real traffic through an httpd
# each test starts itself. Without a binary every class in that family fails
# with a connection-refused to the proxy port - identically on HotSpot, so it
# reads like a VM defect when it is only a missing fixture.
# Always one argument (never an empty word, which `set -u` + an empty array
# would make awkward): TesterHttpd falls back to a bare "httpd" on PATH when
# the property is empty.
HTTPD_PROP="-Dtomcat.test.httpd.path=$HTTPD_PATH"

# Fixture precondition check (VERIFY-01, docs/known-issues/c2/verify-01-differential-harness.md):
# a missing/unbuilt CATALINA_BASE-equivalent (output/build - conf/, webapps/)
# or unbuilt test classes makes every class fail the same way
# (FileNotFoundException/ClassNotFoundError before the test itself runs),
# which reads exactly like a real regression sweep across the whole suite.
# Fail loudly once, up front, instead of producing 645 identical wrong
# results (this is the CATALINA_BASE gap verify-01 named explicitly).
die_fixture() { echo "ERROR: $*" >&2; exit 1; }
_missing=()
{ [ -d "$TC_ROOT/output/testclasses" ] && [ -n "$(ls -A "$TC_ROOT/output/testclasses" 2>/dev/null)" ]; } || _missing+=("$TC_ROOT/output/testclasses (compiled test classes - ant test-compile)")
[ -d "$TC_ROOT/output/build/conf" ] || _missing+=("$TC_ROOT/output/build/conf (CATALINA_BASE conf/ - ant deploy)")
[ -d "$TC_ROOT/output/build/webapps" ] || _missing+=("$TC_ROOT/output/build/webapps (CATALINA_BASE webapps/ - ant deploy)")
[ -s "$CP_FILE" ] || _missing+=("$CP_FILE (classpath file)")
[ -s "$CLASSLIST" ] || _missing+=("$CLASSLIST (class list)")
if [ "${#_missing[@]}" -gt 0 ]; then
  die_fixture "Tomcat fixture incomplete under TC_ROOT=$TC_ROOT - missing or empty: ${_missing[*]}. See run-tomcat-suite.md."
fi

OUTDIR="$TC_ROOT/.suite/results/$RUN_NAME/shard-$SHARD_IDX"
mkdir -p "$OUTDIR"
# The examples webapp's compiled classes are a TEST-classpath entry upstream,
# not just a webapp payload: Tomcat's own build.xml puts
# `${tomcat.build}/webapps/examples/WEB-INF/classes` FIRST on
# `tomcat.test.classpath` (build.xml:248), because `test/util/TestCookieFilter`
# exercises `util.CookieFilter`, which lives in
# `webapps/examples/WEB-INF/classes/util/` and is compiled by `ant deploy` —
# NOT by `ant test-compile`, so it never lands in `output/testclasses`.
# `cp-linux-fixed.txt` omitted it and `util.TestCookieFilter` failed with
# `NoClassDefFoundError: util/CookieFilter` — identically on HotSpot, i.e. a
# fixture gap that reads like a VM defect. The Windows harness
# (`run-tomcat-suite.ps1`) has always had this entry; this is the Linux side
# catching up. Prepended, matching upstream's order. Guarded so a fixture built
# without `ant deploy` does not get an empty classpath element.
CP="$(cat "$CP_FILE"):$HAMCREST_JAR"
EXAMPLES_CLASSES="$TC_ROOT/output/build/webapps/examples/WEB-INF/classes"
if [ -d "$EXAMPLES_CLASSES" ]; then
  CP="$EXAMPLES_CLASSES:$CP"
fi

RESULTS="$OUTDIR/results.csv"
touch "$RESULTS"

# Already-recorded classes are skipped (resumable across restarts/timeouts).
declare -A DONE
if [ -s "$RESULTS" ]; then
  while IFS=, read -r cls _rc _secs _status _load; do
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
    # *LargeHeap classes (e.g. TestByteChunkLargeHeap/TestCharChunkLargeHeap)
    # push a SINGLE array up to AbstractChunk.ARRAY_MAX_SIZE
    # (Integer.MAX_VALUE-8 elements, up to ~4.3 GiB for a char[]). CratonVM's
    # default Generational collector has a fixed Xmx/2 old-gen cap (no
    # dynamic growth), so a humongous array that size can't fit even at
    # -Xmx8g even though HotSpot's region-based G1 handles it fine at 8g.
    # CratonVM's OWN G1 backend (gc/src/g1.rs, production-status per
    # gaps/gc-tuning.md) doesn't have that fixed split and
    # passes both classes cleanly -- TestByteChunkLargeHeap at -Xmx8g,
    # TestCharChunkLargeHeap needs -Xmx10g (measured; HotSpot needs neither
    # bump, its G1 is somewhat more memory-efficient at this extreme). Never
    # downgrade a larger caller-supplied MAX_HEAP.
    local heap="$MAX_HEAP" gc_flag=""
    if [[ "$cls" == *LargeHeap ]]; then
      gc_flag="-XX:+UseG1GC"
      # Bump to 10g unless MAX_HEAP is already a larger *g value (bash-only
      # parse — avoids numfmt's case-sensitive iec-suffix quirks (rejects
      # lowercase "8g")). Any non-"<N>g" MAX_HEAP form (e.g. "512m") is
      # smaller than 10g for this test family, so it's bumped too.
      if [[ "$MAX_HEAP" =~ ^([0-9]+)[gG]$ ]] && [ "${BASH_REMATCH[1]}" -ge 10 ]; then
        heap="$MAX_HEAP"
      else
        heap="10g"
      fi
    elif [ "$cls" = "org.apache.tomcat.integration.httpd.TestChunkedTransferEncodingWithProxy" ]; then
      # Same shape without the naming convention: PAYLOAD_SIZE is literally
      # 10 * 1024 * 1024 * 100 = 1 GiB and TomcatBaseTest.postUrl needs a second
      # buffer of the same size. HotSpot fits that in the 2g default (26.6 s
      # measured); the fixed Xmx/2 old-gen cap above means CratonVM cannot, and
      # the class OOMs at exactly "native primitive array of length 1048576000".
      # 4g clears it (110 s measured). `--Xmx 2g -XX:+UseG1GC` also passes but
      # takes 275 s, close enough to the 300 s default timeout to score as a
      # HANG, so prefer the heap bump.
      if [[ "$MAX_HEAP" =~ ^([0-9]+)[gG]$ ]] && [ "${BASH_REMATCH[1]}" -ge 4 ]; then
        heap="$MAX_HEAP"
      else
        heap="4g"
      fi
    fi
    timeout "${TIMEOUT_SEC}s" "$CRATONVM_EXE" \
      --java-home "$JAVA_HOME25" --Xmx "$heap" $gc_flag \
      -Dfile.encoding=UTF-8 -Djava.net.preferIPv4Stack=true \
      -Dtomcat.test.basedir="$TC_ROOT/output/build" \
      -Dtomcat.test.temp="$TC_ROOT/output/test-tmp" \
      -Dtomcat.test.tomcatbuild="$TC_ROOT/output/build" \
      -Dtomcat.test.relaxTiming=true \
      "$HTTPD_PROP" \
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
      "$HTTPD_PROP" \
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

  # 5th column: the host's 1-minute load average as the class finished.
  #
  # A sharded sweep on a SHARED host is a timing measurement, and rc=124 on its
  # own cannot tell "this class is stuck" from "this class was capped because
  # other people's builds had the cores". Measured 2026-08-23: the SAME binary
  # and the same 640 classes scored 599 PASS at 4 shards on a loaded host, and
  # twelve classes whose walls had been 156-275 s all landed on exactly 300 -
  # the cap, not a defect. An interleaved serial A/B put every one of them well
  # under it. Without this column that sweep reads as a 15-class regression.
  #
  # Per class rather than per run: neighbour load moves during a sweep that
  # takes hours, so a single reading taken at the start would describe the wrong
  # part of it. NA where there is no /proc (non-Linux).
  load1=$(cut -d' ' -f1 /proc/loadavg 2>/dev/null || echo NA)
  [ -n "$load1" ] || load1=NA

  echo "$cls,$rc,$secs,$status,$load1" >> "$RESULTS"
  # keep full logs only for non-PASS classes, to save disk on long runs
  [ "$status" = "PASS" ] && rm -f "$logfile"
done < "$CLASSLIST"

echo "shard $SHARD_IDX ($MODE) done" > "$OUTDIR/DONE"
