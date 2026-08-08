#!/usr/bin/env bash
###############################################################################
# run-h2-suite.sh - H2 Database test suite driver for CratonVM
#
# Mirrors apps/wildfly-suite-runner/run-suite.sh's CLI shape (discover /
# categorize / run / quad / hotspot), but drives H2 directly instead of
# through Maven/Surefire: every org.h2.test.* class that extends TestBase or
# TestDb already carries its own
#   public static void main(String... a) { TestBase.createCaller().init().testFromMain(); }
# (upstream H2 convention, verified across 218/218 concrete test classes) so
# each class is its own one-process-per-class JUnitCore-equivalent entry
# point - no custom launcher class is needed.
#
# Quick examples (Linux bash, e.g. the Azure build host):
#   ./run-h2-suite.sh discover
#   ./run-h2-suite.sh categorize
#   ./run-h2-suite.sh run --category all --count 50
#   ./run-h2-suite.sh run --category failed --start 1 --count 50 --jit off
#   ./run-h2-suite.sh quad --category all --count 200
#   ./run-h2-suite.sh hotspot --category all --count 200
###############################################################################
set -u
set -o pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APPS_ROOT="$(cd "$HERE/.." && pwd)"
H2_ROOT="${H2_ROOT:-$APPS_ROOT/h2database/h2}"
H2_TEST_SRC="${H2_TEST_SRC:-$H2_ROOT/src/test}"
OUTROOT="${OUTROOT:-$HERE/out}"
META="$HERE/meta"
mkdir -p "$OUTROOT" "$META"

ALLIDX="$META/all-classes.tsv"      # fully.qualified.ClassName
PASSED="$META/passed.tsv"
OTHERS="$META/others.tsv"

JDK25="${JDK25:-/home/victor/jdk25}"
CP_FILE="${CP_FILE:-$H2_ROOT/craton-testcp.txt}"

CLASS_TO="${CLASS_TO:-300}"          # per-class timeout seconds
MAX_HEAP="${MAX_HEAP:-1g}"

die() { echo "ERROR: $*" >&2; exit 1; }
# stderr, not stdout: log() output must never land in a command-substitution
# capture (list_for_category()'s callers do `listf="$(list_for_category)"`,
# and list_for_category calls ensure_idx -> discover on a fresh checkout,
# whose log() lines used to get captured into $listf right along with the
# real path, silently corrupting it into "nothing to run" on any worktree
# that hasn't run discover() yet - found while validating VERIFY-01 on a
# fresh worktree 2026-08-03).
log() { echo "[$(date +%H:%M:%S)] $*" >&2; }

now_ms() { date +%s%3N; }

find_vm() {
  local c
  for c in "${CRATONVM_BIN:-}" \
           "$APPS_ROOT/../target/release/cratonvm-h2database-suite" \
           "$APPS_ROOT/../target/release/cratonvm"; do
    [ -n "$c" ] && [ -x "$c" ] && { echo "$c"; return 0; }
  done
  return 1
}

safe_name() { printf '%s' "$1" | sed 's/[^A-Za-z0-9_.-]/_/g'; }

ensure_paths() {
  [ -d "$H2_TEST_SRC" ] || die "H2 test source tree not found: $H2_TEST_SRC (set H2_ROOT)"
}

# Fixture precondition check (VERIFY-01, docs/known-issues/c2/verify-01-differential-harness.md):
# a swept or never-built target/{classes,test-classes} makes every class fail
# the same way (NoClassDefFoundError before the test itself even starts), which
# reads exactly like a real regression sweep. Fail loudly once, up front,
# instead of producing 218 identical wrong results. Called from run_mode(),
# not from discover/categorize (which only need H2_TEST_SRC).
ensure_built() {
  local missing=()
  { [ -d "$H2_ROOT/target/classes" ] && [ -n "$(ls -A "$H2_ROOT/target/classes" 2>/dev/null)" ]; } || missing+=("$H2_ROOT/target/classes")
  { [ -d "$H2_ROOT/target/test-classes" ] && [ -n "$(ls -A "$H2_ROOT/target/test-classes" 2>/dev/null)" ]; } || missing+=("$H2_ROOT/target/test-classes")
  [ -s "$CP_FILE" ] || missing+=("$CP_FILE (classpath file)")
  if [ "${#missing[@]}" -gt 0 ]; then
    die "H2 fixture not built (or was swept) - missing/empty: ${missing[*]}. Run: $0 setup"
  fi
}

# --- discovery ---------------------------------------------------------

class_for_source() {
  local src="$1" rel cls
  rel="${src#"$H2_TEST_SRC"/}"
  cls="${rel%.java}"
  cls="${cls//\//.}"
  echo "$cls"
}

discover() {
  ensure_paths
  log "discovering H2 test classes under $H2_TEST_SRC ..."
  : > "$ALLIDX.tmp"
  # Every runnable H2 test is a concrete (non-abstract) top-level class that
  # extends TestBase or TestDb - this is H2's own convention (see TestAll.java,
  # which builds its list the same way, just by hand). Abstract bases
  # (TestDb, AbstractBaseForCommonTableExpressions, synth/TestHalt) are
  # excluded by requiring "public class" (not "public abstract class").
  grep -rlE '^public class [A-Za-z0-9_]+ extends (TestBase|TestDb)\b' \
    --include='*.java' "$H2_TEST_SRC" | sort | while IFS= read -r src; do
      class_for_source "$src"
    done > "$ALLIDX.tmp"
  sort -u "$ALLIDX.tmp" > "$ALLIDX"
  rm -f "$ALLIDX.tmp"
  log "indexed $(wc -l < "$ALLIDX") test classes -> $ALLIDX"
}

ensure_idx() { [ -s "$ALLIDX" ] || discover; }

categorize() {
  ensure_idx
  local results="${1:-}"
  if [ -z "$results" ]; then
    log "no results.tsv given - running canonical jit-real baseline over the full index"
    run_mode "jit-real" "$ALLIDX" 0 0 "$OUTROOT/baseline-jit-real"
    results="$OUTROOT/baseline-jit-real/results.tsv"
  fi
  [ -s "$results" ] || die "results file empty/missing: $results"
  awk -F'\t' '$3=="PASS"{print $2}' "$results" | sort -u > "$META/.passed.classes"
  awk -F'\t' 'NR==FNR{ok[$1]=1; next} ($1 in ok){print}' "$META/.passed.classes" "$ALLIDX" > "$PASSED"
  awk -F'\t' 'NR==FNR{ok[$1]=1; next} !($1 in ok){print}' "$META/.passed.classes" "$ALLIDX" > "$OTHERS"
  rm -f "$META/.passed.classes"
  log "passed=$(wc -l < "$PASSED")  others=$(wc -l < "$OTHERS")  (from $results)"
}

# --- setup (compile + classpath) ---------------------------------------

setup() {
  ensure_paths
  command -v mvn >/dev/null 2>&1 || die "mvn not found on PATH"
  # Some hosts install a JRE-only "java-21-openjdk" package (no javac) as the
  # default `java`, which mvn happily runs under but which then breaks the
  # in-process compiler mojo with a confusing "release version 11 not
  # supported" error. MVN_JAVA_HOME lets the caller point mvn at a real JDK
  # (release 11 just needs any JDK 11-21) without touching CratonVM's own
  # --java-home (JDK25, set via $JDK25) used later to run the tests.
  local mvn_java_home="${MVN_JAVA_HOME:-}"
  log "compiling H2 main+test sources (mvn test-compile) ..."
  ( cd "$H2_ROOT" || exit 1
    [ -n "$mvn_java_home" ] && export JAVA_HOME="$mvn_java_home"
    mvn -q -B -ntp test-compile ) || die "mvn test-compile failed"
  log "generating test classpath ($CP_FILE) ..."
  ( cd "$H2_ROOT" || exit 1
    [ -n "$mvn_java_home" ] && export JAVA_HOME="$mvn_java_home"
    mvn -q -B -ntp dependency:build-classpath \
      -DincludeScope=test -Dmdep.outputFile="$CP_FILE" ) || die "mvn dependency:build-classpath failed"
  [ -s "$CP_FILE" ] || die "classpath file empty: $CP_FILE"
  log "setup complete: classes=$H2_ROOT/target/classes test-classes=$H2_ROOT/target/test-classes cp=$CP_FILE"
}

full_classpath() {
  [ -s "$CP_FILE" ] || die "no classpath file at $CP_FILE - run: $0 setup"
  printf '%s:%s:%s' "$H2_ROOT/target/classes" "$H2_ROOT/target/test-classes" "$(cat "$CP_FILE")"
}

# --- per-class execution -------------------------------------------------

mode_name() {
  local j="jit" d="real"
  [ "$JIT" = "off" ] && j="nojit"
  [ "$JDK" = "synthetic" ] && d="syn"
  echo "$j-$d"
}

crash_pattern='EXCEPTION_ACCESS_VIOLATION|STATUS_ACCESS_VIOLATION|SIGSEGV|SIGABRT|fatal runtime error|panicked at|thread .* panicked|internal error:|not yet implemented|illegal instruction|caught fatal signal|cratonvm panic|entered unreachable|Fatal error|hs_err_pid'

classify() {
  local rc="$1" logf="$2" status
  if [ "$rc" -eq 124 ] || [ "$rc" -eq 137 ]; then
    echo "HANG"; return
  fi
  if grep -qaiE "$crash_pattern" "$logf"; then
    echo "CRASH"; return
  fi
  if [ "$rc" -eq 0 ]; then
    echo "PASS"
  else
    echo "FAIL"
  fi
}

note_for() {
  local logf="$1" note
  note="$(grep -aiE '(Exception|Error|AssertionError|panicked|NoClassDef|NoSuchMethod|AbstractMethod)' "$logf" 2>/dev/null | head -1)"
  note="${note//$'\t'/ }"
  note="$(printf '%s' "$note" | tr -d '\r\n')"
  if [ "${#note}" -gt 180 ]; then note="${note:0:180}"; fi
  printf '%s' "$note"
}

run_one_class() {
  local idx="$1" mode="$2" cls="$3" outdir="$4" javabin="$5"; shift 5
  local -a extra_args=("$@")
  local logs="$outdir/logs" res="$outdir/results.tsv" wd="$outdir/workdirs/$(safe_name "$cls")"
  mkdir -p "$logs" "$wd"
  local safe logf t0 t1 ms rc status note
  safe="$(safe_name "$cls")"
  logf="$logs/$(printf '%05d' "$idx")-$safe.log"

  # Suite runs disable the default watchdog: a class that legitimately takes
  # the whole $CLASS_TO budget would otherwise abort()+dump, which is noise
  # here and loses the timeout classification. But disable it ONLY when the
  # caller has not asked for a watchdog itself: vm-cli's `default_watchdog_env`
  # (vm-cli/src/main.rs) resolves to None the moment
  # CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 is present, *whatever*
  # CRATONVM_DEFAULT_WATCHDOG_SEC says -- so hard-setting it here is what made
  # `env CRATONVM_DEFAULT_WATCHDOG_SEC=45 ./run-h2-suite.sh run ...` produce a
  # per-class log with the startup banner and no T19.H1 dump, long past the
  # deadline (recorded as an unexplained residual in
  # fixed-suite-bugs/h2-suite-bugs/bug-h2-mvstore-insert-loop-perf-hang-RESOLVED-20260807.md until
  # 2026-08-07).
  local -a wd_env=(CRATONVM_DISABLE_DEFAULT_WATCHDOG=1)
  [ -n "${CRATONVM_DEFAULT_WATCHDOG_SEC:-}" ] && wd_env=()

  t0="$(now_ms)"
  ( cd "$wd" && env "${wd_env[@]}" timeout --kill-after=5 "$CLASS_TO" \
      "$javabin" "${extra_args[@]}" "$cls" ) > "$logf" 2>&1
  rc=$?
  t1="$(now_ms)"
  ms=$((t1 - t0))

  status="$(classify "$rc" "$logf")"
  note=""
  [ "$status" != "PASS" ] && note="$(note_for "$logf")"

  printf '%s\t%s\t%s\t%s\t%d\t%d\t%s\t%s\t%s\n' \
    "$idx" "$cls" "$status" "$rc" "$ms" "0" "$mode" "$logf" "$note" >> "$res"
  printf '%s\t%d\t%s\t%s\n' "$cls" "$ms" "$status" "$mode" >> "$outdir/timing.tsv"

  log "[$mode] $(printf '%-9s' "$status") $(awk "BEGIN{printf \"%7.1f\", $ms/1000}")s $cls"

  # Each class gets its own scratch CWD (H2's TestBase.BASE_TEST_DIR="./data"
  # and error.lock/error.txt are CWD-relative) so parallel runs never collide;
  # clean it up immediately to bound disk usage across a full-suite run.
  rm -rf "$wd"
}

run_mode() {
  local mode="$1" listfile="$2" start="$3" count="$4" outdir="$5"
  ensure_paths
  ensure_built
  mkdir -p "$outdir/logs" "$outdir/workdirs"
  local RES="$outdir/results.tsv" RUN="$outdir/run.log"
  : > "$RES"; : > "$RUN"; : > "$outdir/timing.tsv"

  CP="$(full_classpath)"

  local javabin
  local -a jargs=()
  case "$mode" in
    hotspot)
      javabin="$JDK25/bin/java"
      jargs=("-Xmx$MAX_HEAP" "-cp" "$CP")
      ;;
    jit-real)
      javabin="$(find_vm)" || die "cratonvm executable not found (set CRATONVM_BIN or build it)"
      jargs=("--java-home" "$JDK25" "--Xmx" "$MAX_HEAP" "-c" "$CP")
      ;;
    nojit-real)
      javabin="$(find_vm)" || die "cratonvm executable not found (set CRATONVM_BIN or build it)"
      jargs=("--java-home" "$JDK25" "--Xmx" "$MAX_HEAP" "--nojit" "-c" "$CP")
      ;;
    jit-syn)
      javabin="$(find_vm)" || die "cratonvm executable not found (set CRATONVM_BIN or build it)"
      jargs=("--synthetic-jdk" "--Xmx" "$MAX_HEAP" "-c" "$CP")
      ;;
    nojit-syn)
      javabin="$(find_vm)" || die "cratonvm executable not found (set CRATONVM_BIN or build it)"
      jargs=("--synthetic-jdk" "--Xmx" "$MAX_HEAP" "--nojit" "-c" "$CP")
      ;;
    *) die "unknown mode: $mode" ;;
  esac

  local sliced; sliced="$(mktemp)"
  awk -v s="$start" -v c="$count" 'NR >= (s > 0 ? s : 1) { print; n++; if (c > 0 && n >= c) exit }' "$listfile" > "$sliced"
  local total; total="$(wc -l < "$sliced")"
  {
    echo "mode=$mode"
    echo "list=$listfile  start=$start  count=$count  -> $total classes"
    echo "h2_root=$H2_ROOT"
    echo "javabin=$javabin ${jargs[*]}"
    echo "class_timeout=${CLASS_TO}s  max_heap=$MAX_HEAP"
    echo "started=$(date '+%F %T')"
  } | tee -a "$RUN"
  [ "$total" -eq 0 ] && { echo "nothing to run"; rm -f "$sliced"; return 0; }

  local t0 t1 wall idx cls
  t0="$(date +%s)"
  idx=0
  while IFS= read -r cls; do
    [ -z "$cls" ] && continue
    idx=$((idx + 1))
    run_one_class "$idx" "$mode" "$cls" "$outdir" "$javabin" "${jargs[@]}"
  done < "$sliced"
  rm -f "$sliced"
  t1="$(date +%s)"
  wall=$((t1 - t0))
  {
    echo "finished=$(date '+%F %T')  wall=${wall}s"
    awk -F'\t' '{c[$3]++} END {printf "classes:"; for (k in c) printf " %s=%d", k, c[k]; printf "\n"}' "$RES"
    echo "wall-clock=${wall}s"
  } | tee -a "$RUN" | tee "$outdir/summary.txt"
  log "[$mode] done in ${wall}s -> $outdir"
}

# --- CLI -----------------------------------------------------------------

CATEGORY="all"; JIT="on"; JDK="real"; START=0; COUNT=0; TAG=""; ONLY=""; SHARD=""
parse_run_args() {
  while [ $# -gt 0 ]; do
    case "$1" in
      --category) CATEGORY="$2"; shift 2 ;;
      --jit) JIT="$2"; shift 2 ;;
      --jdk) JDK="$2"; shift 2 ;;
      --start) START="$2"; shift 2 ;;
      --count) COUNT="$2"; shift 2 ;;
      --only) ONLY="$2"; shift 2 ;;
      --shard) SHARD="$2"; shift 2 ;;
      --class-to|--timeout) CLASS_TO="$2"; shift 2 ;;
      --max-heap) MAX_HEAP="$2"; shift 2 ;;
      --tag) TAG="$2"; shift 2 ;;
      *) die "unknown option: $1" ;;
    esac
  done
}

list_for_category() {
  local base
  case "$CATEGORY" in
    passed) [ -s "$PASSED" ] || die "passed.tsv missing - run: $0 categorize"; base="$PASSED" ;;
    failed|others) [ -s "$OTHERS" ] || die "others.tsv missing - run: $0 categorize"; base="$OTHERS" ;;
    all) ensure_idx; base="$ALLIDX" ;;
    *) die "category must be passed|failed|others|all" ;;
  esac
  if [ -n "$ONLY" ]; then
    local f="$META/.only-$$.tsv"
    grep -E "$ONLY" "$base" > "$f" || true
    base="$f"
  fi
  if [ -n "$SHARD" ]; then
    local i m f
    i="${SHARD%/*}"
    m="${SHARD#*/}"
    f="$META/.shard-${i}of${m}-$$.tsv"
    awk -v i="$i" -v m="$m" 'NR % m == (i - 1) % m' "$base" > "$f"
    base="$f"
  fi
  echo "$base"
}

cmd_run() {
  parse_run_args "$@"
  local listf mode stamp out
  listf="$(list_for_category)"
  mode="$(mode_name)"
  stamp="$(date +%Y%m%d-%H%M%S)"
  out="$OUTROOT/${TAG:+$TAG-}${mode}-${CATEGORY}-${stamp}"
  log "RUN mode=$mode category=$CATEGORY start=$START count=$COUNT -> $out"
  run_mode "$mode" "$listf" "$START" "$COUNT" "$out"
}

cmd_hotspot() {
  parse_run_args "$@"
  local listf stamp out
  listf="$(list_for_category)"
  stamp="$(date +%Y%m%d-%H%M%S)"
  out="$OUTROOT/${TAG:+$TAG-}hotspot-${CATEGORY}-${stamp}"
  log "HOTSPOT baseline category=$CATEGORY start=$START count=$COUNT -> $out"
  run_mode "hotspot" "$listf" "$START" "$COUNT" "$out"
}

cmd_quad() {
  parse_run_args "$@"
  local listf stamp base m
  listf="$(list_for_category)"
  stamp="$(date +%Y%m%d-%H%M%S)"
  base="$OUTROOT/${TAG:+$TAG-}quad-${CATEGORY}-${stamp}"
  mkdir -p "$base"
  log "QUAD: 4 modes in parallel, category=$CATEGORY start=$START count=$COUNT -> $base"
  for m in jit-real nojit-real jit-syn nojit-syn; do
    ( run_mode "$m" "$listf" "$START" "$COUNT" "$base/$m" ) > "$base/$m.stdout.log" 2>&1 &
    log "  started $m (pid $!)"
  done
  wait
  log "QUAD complete. Summaries:"
  for m in jit-real nojit-real jit-syn nojit-syn; do
    echo "--- $m ---"
    cat "$base/$m/summary.txt" 2>/dev/null || echo "(no summary)"
  done | tee "$base/QUAD-SUMMARY.txt"
}

usage() {
  cat <<EOF
run-h2-suite.sh - H2 Database test suite driver for CratonVM

COMMANDS
  setup                           Compile H2 (mvn test-compile) + generate classpath.
  discover                        Build meta/all-classes.tsv from H2 test sources.
  categorize [results.tsv]        Build meta/passed.tsv and meta/others.tsv.
                                   With no file, runs jit-real over the whole index first.
  run [opts]                      Run one CratonVM mode over a category slice.
  quad [opts]                     Run all four CratonVM modes in parallel.
  hotspot [opts]                  Run the slice under stock HotSpot (JDK25).
  help                            Show this help.

OPTIONS (run / quad / hotspot)
  --category passed|failed|all    Class set (default all; failed aliases others).
  --jit on|off                    JIT toggle for run (default on).
  --jdk real|synthetic            JDK backend for run (default real).
  --start N                       1-based start index into the category list.
  --count N                       Number of classes (0 = all).
  --only REGEX                    Filter fully-qualified class names.
  --shard I/M                     Round-robin shard, 1-based.
  --class-to S                    Per-class timeout seconds (default $CLASS_TO).
  --max-heap H                    Heap for both VMs (default $MAX_HEAP).
  --tag NAME                      Prefix for the output directory.

ENV
  CRATONVM_BIN=...                Override cratonvm executable path.
  CRATONVM_*=...                  Any CratonVM env knob; inherited by child VMs.
  CRATONVM_DEFAULT_WATCHDOG_SEC=N Arm the in-VM stack-dump watchdog at N seconds.
                                  Setting it also stops the runner from passing
                                  CRATONVM_DISABLE_DEFAULT_WATCHDOG=1, which
                                  would otherwise silently veto it.
  H2_ROOT=...                     H2 checkout (default apps/h2database/h2).
  JDK25=...                       Real JDK25 home (default /home/victor/jdk25).
  CP_FILE=...                     Classpath file (default \$H2_ROOT/craton-testcp.txt).
  OUTROOT=...                     Output root (default ./out).

OUTPUT (per mode dir)
  results.tsv     idx class status rc ms tests mode log note
  timing.tsv      class ms status mode
  summary.txt     status tally + wall-clock
  run.log         driver log
  logs/           per-class combined stdout+stderr
  workdirs/       per-class scratch CWD (deleted after each class completes)
EOF
}

cmd="${1:-help}"; shift || true
case "$cmd" in
  setup) setup ;;
  discover) discover ;;
  categorize) categorize "${1:-}" ;;
  run) cmd_run "$@" ;;
  quad) cmd_quad "$@" ;;
  hotspot) cmd_hotspot "$@" ;;
  help|-h|--help) usage ;;
  *) usage; die "unknown command: $cmd" ;;
esac
