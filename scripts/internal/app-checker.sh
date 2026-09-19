#!/usr/bin/env bash
# app-checker.sh — unified CratonVM app-bringup harness.
#
# Replaces three earlier scripts (orchestrator-run-all.sh,
# orchestrator-functional-all.sh, orchestrator-recursive-all.sh) with a
# single entry point that picks the test mode via the first positional
# argument.
#
# USAGE
#   scripts/app-checker.sh MODE [OPTIONS...]
#
# MODES
#   smoke         Curated launcher-entry tests ("--version" / "--help"
#                 style). Fast (~5-15 min). Detects whether the JVM
#                 boots the app's main entry point and reaches the
#                 launcher's argument parser. ~25 hand-wired apps.
#
#   functional   Curated daemon-start + endpoint-probe tests. Slow
#                 (~15-30 min). Each app: launch as a daemon, wait for
#                 a service-ready log line, run one HTTP/functional
#                 probe, kill the daemon. Goes past the smoke layer and
#                 surfaces application-level errors. ~21 hand-wired apps.
#
#                 Daemon-style apps (wildfly, kafka, activemq, felix,
#                 kc16, kc26, springboot) use launch_daemon → endpoint
#                 probe → kill. Library-only apps (cassandra utils,
#                 hazelcast config, jetty Server(0), solr SolrInputDoc)
#                 stay on probe-based oneshots — those have no daemon
#                 shape.
#
#   recursive     Walk apps/ recursively; for every JAR/WAR whose
#                 MANIFEST.MF declares Main-Class, run it under
#                 CratonVM with a configurable timeout. Emits a CSV
#                 (rel_path,size_mb,main_class,rc,elapsed_s,
#                 classification,first_error) + a sorted summary
#                 (by classification, by rc, slowest runs, distinct
#                 first-error lines). ~200-300 jars; ~15-25 min.
#
#   all           Run smoke + functional + recursive in sequence, each
#                 into its own log subdir. Useful for nightly bringup.
#
#   help          Print this help text and exit.
#
# OPTIONS
#   --apps DIR        Path to apps folder
#                     (default: $REPO_ROOT/apps)
#   --timeout N       Per-app timeout in seconds. Defaults:
#                       smoke: 30
#                       functional: 240 (server start window)
#                       recursive: 20
#   --max-size MB     [recursive] Skip JARs larger than N MB.
#                     (default: 200)
#   --max-runs N      [recursive] Cap total number of JARs (debug).
#                     (default: unlimited)
#   --filter NAMES    Comma-separated app names. Smoke/functional:
#                     subset to these names. Recursive: substring-match
#                     on the relative path.
#   --label TAG       Override stamp suffix (default: yyyymmdd-hhmmss).
#                     Affects log dir name.
#   --jdk PATH        Real JDK to pass to --java-home.
#                     (default: C:/Program Files/Eclipse Adoptium/
#                     jdk-25.0.2.10-hotspot)
#   --bin PATH        Path to cratonvm.exe
#                     (default: target/release/cratonvm.exe)
#   --xmx SIZE        --Xmx value passed to every app (e.g. 1g, 512m).
#                     Mode defaults: smoke=512m, functional=1g,
#                     recursive=512m.
#   --build           Run `cargo build --release -p cratonvm-cli` first.
#                     Fails fast if the build doesn't go green.
#   --strict          Treat any non-noise stderr line as failure.
#                     (default: only rc != 0 counts as failure.)
#   --verbose         Tee each app's stdout/stderr to the console as
#                     well as the per-app log files.
#   --no-summary      Skip the final summary block.
#   --daemon-only     [functional] Run ONLY the daemon-style tests
#                     (launch_daemon + endpoint probe). Skips probe-
#                     based oneshots that exercise library code without
#                     a real daemon. Use this to verify the daemon-level
#                     retirement criterion documented in
#                     apps/TARGET_APPS.md ("Daemon/server apps: real
#                     daemon launch + endpoint probe pass = retire.
#                     Library-level probe pass alone is NOT enough.").
#   -h, --help        Print help.
#
# OUTPUT
#   applogs/orchestrator-<mode>-<label>/
#     <name>.out, <name>.err   per-app logs
#     run.log                  the live one-liner stream
#     summary.txt              the aggregated summary at the end
#     results.csv              recursive mode only
#     discover.tsv             recursive mode only (Main-Class index)
#
#   Console: one summary line per app, in the format
#     <name> | rc=N | <elapsed>s | <classification> | <first-error>
#
# CLASSIFICATIONS
#   pass        rc=0 and (unless --strict) no error-pattern in stderr
#   linkage     NoClassDef / NoSuchMethod / NoSuchField / AbstractMethod
#               / VerifyError / UnsupportedClassVersion / ClassFormatError
#   npe         NullPointer / IllegalState / Cannot invoke / Cannot read
#   resource    StackOverflow / OutOfMemory
#   vm-bug      "undersized object layout" / "stale pointer" /
#               "ARRAY-LEN-GUARD"
#   swallowed   B6: silent-swallow (CratonVM's clinit-error suppression)
#   timeout     rc=124 (timeout fired)
#   app-error   any other Exception/Error/SEVERE/FATAL line
#   other       non-zero rc with no clear error signature

set +e

# ----- Defaults -------------------------------------------------------------

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

MODE=""
APPS=""
TIMEOUT_S=""
MAX_SIZE_MB=200
MAX_RUNS=0
FILTER=""
LABEL=""
JDK="${JAVA_HOME:-C:/Program Files/Java/jdk-25}"
RJVM=""
XMX=""
DO_BUILD=0
STRICT=0
VERBOSE=0
NO_SUMMARY=0
DAEMON_ONLY=0

# ----- Help -----------------------------------------------------------------

print_help() {
    sed -n '2,/^# CLASSIFICATIONS/p' "$0" | sed 's/^# \?//; $d'
    echo
    echo "CLASSIFICATIONS"
    sed -n '/^# CLASSIFICATIONS/,/^set +e/p' "$0" | sed -n '2,/^set +e/p' | sed 's/^# \?//; $d'
}

# ----- Arg parsing ----------------------------------------------------------

if [ $# -eq 0 ]; then
    print_help
    exit 0
fi

MODE="$1"; shift

case "$MODE" in
    smoke|functional|recursive|all)
        ;;
    help|-h|--help)
        print_help
        exit 0
        ;;
    *)
        echo "ERROR: unknown mode '$MODE' (expected smoke|functional|recursive|all|help)" >&2
        echo "Run '$0 help' for usage." >&2
        exit 2
        ;;
esac

while [ $# -gt 0 ]; do
    case "$1" in
        --apps)        APPS="$2";        shift 2 ;;
        --timeout)     TIMEOUT_S="$2";   shift 2 ;;
        --max-size)    MAX_SIZE_MB="$2"; shift 2 ;;
        --max-runs)    MAX_RUNS="$2";    shift 2 ;;
        --filter)      FILTER="$2";      shift 2 ;;
        --label)       LABEL="$2";       shift 2 ;;
        --jdk)         JDK="$2";         shift 2 ;;
        --bin)         RJVM="$2";        shift 2 ;;
        --xmx)         XMX="$2";         shift 2 ;;
        --build)       DO_BUILD=1;       shift ;;
        --strict)      STRICT=1;         shift ;;
        --verbose)     VERBOSE=1;        shift ;;
        --no-summary)  NO_SUMMARY=1;     shift ;;
        --daemon-only) DAEMON_ONLY=1;    shift ;;
        -h|--help)     print_help; exit 0 ;;
        *)
            echo "ERROR: unknown option '$1' (run '$0 help' for usage)" >&2
            exit 2
            ;;
    esac
done

# Mode-specific defaults
[ -z "$APPS"       ] && APPS="$REPO_ROOT/apps"
[ -z "$LABEL"      ] && LABEL="$(date +%Y%m%d-%H%M%S)"
[ -z "$RJVM"       ] && RJVM="$REPO_ROOT/target/release/cratonvm.exe"
case "$MODE" in
    smoke)      [ -z "$TIMEOUT_S" ] && TIMEOUT_S=30
                [ -z "$XMX" ]       && XMX=512m ;;
    functional) [ -z "$TIMEOUT_S" ] && TIMEOUT_S=240
                [ -z "$XMX" ]       && XMX=1g ;;
    recursive)  [ -z "$TIMEOUT_S" ] && TIMEOUT_S=20
                [ -z "$XMX" ]       && XMX=512m ;;
    all)        [ -z "$XMX" ]       && XMX=1g ;;
esac

# ----- Build (optional) -----------------------------------------------------

if [ "$DO_BUILD" -eq 1 ]; then
    echo ">>> Building release binary..." >&2
    ( cd "$REPO_ROOT" && cargo build --release -p cratonvm-cli ) || {
        echo "ERROR: cargo build failed; aborting" >&2
        exit 3
    }
fi

if [ ! -x "$RJVM" ]; then
    echo "ERROR: binary not found at $RJVM (pass --bin or --build)" >&2
    exit 3
fi

# ----- Filter helper --------------------------------------------------------

# in_filter NAME — return 0 if NAME passes the current filter (or no filter).
in_filter() {
    [ -z "$FILTER" ] && return 0
    local IFS=,
    local needle want
    needle="$1"
    for want in $FILTER; do
        if [ "$needle" = "$want" ] || echo "$needle" | grep -qF "$want"; then
            return 0
        fi
    done
    return 1
}

# ----- Shared helpers -------------------------------------------------------

# cp_glob <dir1> [dir2 ...] — collect all *.jar, normalize to Windows ; sep.
cp_glob() {
    find "$@" -name '*.jar' 2>/dev/null \
        | sed 's|^/c|C:|' \
        | tr '\n' ';' \
        | sed 's/;$//'
}

# classify_run RC FIRST_ERR — one-word classification.
classify_run() {
    local rc="$1"
    local err="$2"
    case "$rc" in
        0)   if [ "$STRICT" -eq 1 ] && [ -n "$err" ]; then
                 echo "strict-fail"
             else
                 echo "pass"
             fi
             ;;
        124) echo "timeout" ;;
        *)
            if echo "$err" | grep -qiE 'NoClassDefFound|NoSuchMethod|NoSuchField|AbstractMethod|VerifyError|UnsupportedClassVersion|ClassFormatError|UnknownModule'; then
                echo "linkage"
            elif echo "$err" | grep -qiE 'NullPointer|IllegalState|Cannot invoke|Cannot read'; then
                echo "npe"
            elif echo "$err" | grep -qiE 'StackOverflow|OutOfMemory'; then
                echo "resource"
            elif echo "$err" | grep -qiE 'undersized object layout|stale pointer|ARRAY-LEN-GUARD'; then
                echo "vm-bug"
            elif echo "$err" | grep -qiE 'silent-swallow|B6:'; then
                echo "swallowed"
            elif echo "$err" | grep -qiE 'Exception|Error|FAIL|SEVERE|FATAL'; then
                echo "app-error"
            else
                echo "other"
            fi
            ;;
    esac
}

# first_err_line LOG_FILE — extract first interesting stderr line.
first_err_line() {
    local f="$1"
    grep -vE '^\[2m.*WARN.*Post-clinit|^\[2m.*WARN.*B6:|^\[2m.*WARN.*Stale|^\[2m.*WARN.*Missing native|^\[2m.*WARN.*SWALLOW|^\[DBG\]|^\[cratonvm\]|^\[rustjvm\]|^\[2m.*WARN.*registered|short-circuited|^\[kc17-bf\]|^\[jboss-bf\]' "$f" 2>/dev/null \
        | grep -E 'Exception|Error|Caused|StringIndex|NullPointer|StackOverflow|ARRAY-LEN-GUARD|gen_heap|FileNot|UnsupportedClass|AbstractMethod|UnknownModule|^Failed|severe|SEVERE|FATAL|fatal|silent-swallow' \
        | head -1 | head -c 220 | sed 's/\x1b\[[0-9;]*m//g'
}

# launch_daemon NAME READY_PATTERN TIMEOUT_S CMD...
#
# Background-launches CMD; greps merged out+err for READY_PATTERN until
# match or timeout. Records PID for later kill_daemon. Returns:
#   0  daemon ready (pattern matched)
#   2  daemon died before ready (DAEMON_DIED)
#   3  timed out without ready (TIMEOUT_NO_READY)
#
# On non-success returns, this also emits a one-liner identical in shape
# to run_oneshot's and appends a row to the CSV so summary tooling treats
# daemon failures the same as oneshot failures.
launch_daemon() {
    local name="$1"; shift
    local ready_re="$1"; shift
    local t="$1"; shift
    local out="$LOGDIR/$name.out"
    local err="$LOGDIR/$name.err"

    local t0 t1 elapsed
    t0=$(date +%s)

    # Close stdin so daemons that read EOF and exit (felix Gogo, kc26
    # picocli) don't sit forever waiting for keyboard input.
    # Disable job-control before backgrounding so bash doesn't print
    # "Segmentation fault" notices when the daemon dies — we already
    # capture the exit status via kill -0 and report DAEMON_DIED.
    set +m
    "$@" < /dev/null > "$out" 2> "$err" &
    local pid=$!
    disown $pid 2>/dev/null
    echo "$pid" > "$LOGDIR/$name.pid"

    elapsed=0
    while [ "$elapsed" -lt "$t" ]; do
        if ! kill -0 "$pid" 2>/dev/null; then
            t1=$(date +%s); elapsed=$((t1 - t0))
            local first_err cls line err_csv
            first_err=$(first_err_line "$err")
            cls="DAEMON_DIED"
            line=$(printf "%-30.30s | rc=%-3s | %5ds | %-13s | %s" \
                "$name" "2" "$elapsed" "$cls" "$first_err")
            echo "$line"
            echo "$line" >> "$RUN_LOG"
            if [ -n "$CSV" ]; then
                err_csv=$(echo "$first_err" | tr -d '"' | tr ',' ' ' | head -c 200)
                echo "$name,,,2,$elapsed,$cls,$err_csv" >> "$CSV"
            fi
            return 2
        fi
        if grep -qE "$ready_re" "$out" "$err" 2>/dev/null; then
            return 0
        fi
        sleep 1
        elapsed=$((elapsed + 1))
    done

    t1=$(date +%s); elapsed=$((t1 - t0))
    local first_err cls line err_csv
    first_err=$(first_err_line "$err")
    cls="TIMEOUT_NO_READY"
    line=$(printf "%-30.30s | rc=%-3s | %5ds | %-13s | %s" \
        "$name" "3" "$elapsed" "$cls" "$first_err")
    echo "$line"
    echo "$line" >> "$RUN_LOG"
    if [ -n "$CSV" ]; then
        err_csv=$(echo "$first_err" | tr -d '"' | tr ',' ' ' | head -c 200)
        echo "$name,,,3,$elapsed,$cls,$err_csv" >> "$CSV"
    fi
    return 3
}

# kill_daemon NAME — best-effort cygwin-safe kill.
kill_daemon() {
    local name="$1"
    local pid_file="$LOGDIR/$name.pid"
    [ -f "$pid_file" ] || return
    local pid
    pid=$(cat "$pid_file")
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
        kill "$pid" 2>/dev/null
        sleep 1
        kill -9 "$pid" 2>/dev/null
        sleep 1
        taskkill //F //PID "$pid" 2>/dev/null
    fi
    rm -f "$pid_file"
}

# probe_http URL TIMEOUT_S — return 0 on HTTP 2xx/3xx, 1 otherwise. Quiet.
probe_http() {
    local url="$1"
    local t="$2"
    curl --silent --output /dev/null --write-out '%{http_code}\n' \
        --max-time "$t" --connect-timeout "$t" \
        "$url" 2>/dev/null \
        | grep -qE '^[23]'
}

# run_oneshot NAME TIMEOUT_S CMD...
#
# Foreground-launches CMD with timeout; logs per-app .out/.err; prints
# the standard one-liner line and appends a CSV row to $CSV (if set).
run_oneshot() {
    local name="$1"; shift
    local t="$1"; shift
    in_filter "$name" || return 0

    if [ "$VERBOSE" -eq 1 ]; then
        echo ">>> $name (timeout=${t}s)" >&2
    fi

    local t0 t1
    t0=$(date +%s)
    # Always close stdin so interactive REPL launchers (felix Gogo, kafka
    # Scala REPL, kc26 picocli) see EOF and exit cleanly instead of
    # blocking until the per-app timeout fires.
    timeout --foreground -k 5 "$t" "$@" \
        < /dev/null > "$LOGDIR/$name.out" 2> "$LOGDIR/$name.err"
    local rc=$?
    t1=$(date +%s)
    local elapsed=$((t1 - t0))

    if [ "$VERBOSE" -eq 1 ]; then
        sed 's/^/  out>/' "$LOGDIR/$name.out" | head -20
        sed 's/^/  err>/' "$LOGDIR/$name.err" | head -20
    fi

    local first_err
    first_err=$(first_err_line "$LOGDIR/$name.err")
    local cls
    cls=$(classify_run "$rc" "$first_err")
    if [ -n "$CSV" ]; then
        local err_csv
        err_csv=$(echo "$first_err" | tr -d '"' | tr ',' ' ' | head -c 200)
        # 7 fields to match the header: rel_path,size_mb,main_class,rc,elapsed_s,classification,first_error
        # Smoke/functional rows leave size_mb + main_class blank.
        echo "$name,,,$rc,$elapsed,$cls,$err_csv" >> "$CSV"
    fi
    local line
    line=$(printf "%-30.30s | rc=%-3s | %5ds | %-9s | %s" "$name" "$rc" "$elapsed" "$cls" "$first_err")
    echo "$line"
    echo "$line" >> "$RUN_LOG"
}

# ----- Summary printer ------------------------------------------------------

print_summary() {
    [ "$NO_SUMMARY" -eq 1 ] && return
    local csv="$1"
    [ -f "$csv" ] || return
    {
        echo ""
        echo "=================== Summary ==================="
        echo "Mode:           $MODE"
        echo "Apps dir:       $APPS"
        echo "Binary:         $RJVM"
        echo "JDK:            $JDK"
        echo "Timeout/run:    ${TIMEOUT_S}s"
        echo "Strict mode:    $STRICT"
        echo "Filter:         ${FILTER:-(none)}"
        echo ""
        local rows
        rows=$(tail -n +2 "$csv" | wc -l)
        echo "Rows:           $rows"
        echo ""
        echo "-- by classification --"
        tail -n +2 "$csv" | awk -F, '{print $6}' | sort | uniq -c | sort -rn
        echo ""
        echo "-- by rc --"
        tail -n +2 "$csv" | awk -F, '{print $4}' | sort | uniq -c | sort -rn
        echo ""
        echo "-- top 10 slowest runs --"
        tail -n +2 "$csv" | sort -t, -k5 -rn | head -10 \
            | awk -F, '{printf "  %5ss  rc=%s  %s\n", $5, $4, $1}'
        echo ""
        echo "-- distinct first-error lines (up to 20) --"
        tail -n +2 "$csv" | awk -F, '$7 != "" {print $7}' | sort -u | head -20
        echo ""
        echo "Full CSV: $csv"
    } | tee -a "$SUMMARY"
}

# ============================================================================
# Mode bodies
# ============================================================================

# ----- SMOKE ---------------------------------------------------------------
#
# Curated launcher-entry tests. Each app's "smoke" is a single `--version` /
# `--help` style invocation under the JVM. Pass = launcher reached its
# argument parser. Counts the JDK boot path + classpath resolution as
# implicit coverage.

## class_load_test NAME JAR CLASS [CLASS...]
##   Run the generic ClassLoadProbe against $JAR (or path glob) verifying
##   each $CLASS parses + loads on CratonVM. Pass = every class loads.
##   Used for Java-library apps where the upstream entry point needs
##   transitive deps the orchestrator doesn't ship (slf4j, jboss-logging,
##   ManagedChannelProvider SPI, etc.).
class_load_test() {
    local name="$1"; shift
    local cp="$1"; shift
    [ -f "$REPO_ROOT/test-infra/probes/classload_probe/ClassLoadProbe.class" ] || return 0
    run_oneshot "$name" "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        -c "$REPO_ROOT/test-infra/probes/classload_probe;$cp" \
        ClassLoadProbe "$@"
}

run_smoke() {
    local probes_present=0

    # ------------------------------------------------------------------
    # In-pool probe-style apps (live under apps/<name>/). These ship
    # pre-compiled probe classes and exercise a focused chunk of the
    # JVM (crypto, cleaners, signatures, enums, NIO, etc.).
    # ------------------------------------------------------------------
    if [ -f "$APPS/enumtest/EnumTest.class" ]; then
        run_oneshot enumtest "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$APPS/enumtest" EnumTest
        probes_present=1
    fi
    if [ -f "$APPS/cipher_probe/CipherProbe.class" ]; then
        run_oneshot cipher_probe "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$APPS/cipher_probe" CipherProbe
        probes_present=1
    fi
    if [ -f "$APPS/cleaner_probe/classes/CleanerProbe.class" ]; then
        run_oneshot cleaner_probe "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$APPS/cleaner_probe/classes" CleanerProbe
        probes_present=1
    fi
    if [ -f "$APPS/sig_probe/classes/SigProbe.class" ]; then
        run_oneshot sig_probe "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$APPS/sig_probe/classes" SigProbe
        probes_present=1
    fi
    if [ -f "$APPS/cglib_probe/classes/CglibProbe.class" ]; then
        run_oneshot cglib_probe "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$APPS/cglib_probe/classes" CglibProbe
        probes_present=1
    fi
    if [ -f "$APPS/slf4j/LogTest.class" ]; then
        # LogTest links org.slf4j.Logger; pull the slf4j-api jar from
        # ejbca-ce's lib/ext/ if present (it's the only in-pool source
        # of slf4j-api at the moment).
        local slf4j_jar="$APPS/ejbca-ce/lib/ext/slf4j-api-2.0.16.jar"
        if [ -f "$slf4j_jar" ]; then
            run_oneshot slf4j_log "$TIMEOUT_S" \
                "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
                -c "$APPS/slf4j;$slf4j_jar" LogTest
            probes_present=1
        fi
    fi
    if [ -f "$APPS/netty/NioProbe.class" ]; then
        run_oneshot netty_nio "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$APPS/netty" NioProbe
        probes_present=1
    fi
    if [ -f "$APPS/gpu-bench/classes/CpuOnlyBench.class" ]; then
        run_oneshot gpu_bench "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$APPS/gpu-bench/classes" CpuOnlyBench
        probes_present=1
    fi
    # Spring Boot launchers — `--jar target/<app>.jar` boots the
    # SpringApplication far enough to print its banner; if it returns
    # rc=0 within the smoke window the Boot lifecycle reached the
    # post-banner application-runner stage.
    # SportMe / letsgo are Spring Boot 2.0 monoliths whose Application
    # classes depend on Spring Boot 2.0 + spring-session-redis + springfox
    # that aren't on the orchestrator's stable classpath. Use the
    # ClassLoadProbe to verify the compiled `target/classes/` tree at
    # least parses + loads a leaf class — that's a meaningful "compiled
    # against this JVM" signal even when the SpringApplication itself
    # can't link.
    if [ -d "$APPS/SportMe-master/target/classes" ] \
        && [ -f "$REPO_ROOT/test-infra/probes/classload_probe/ClassLoadProbe.class" ]; then
        run_oneshot sportme "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$REPO_ROOT/test-infra/probes/classload_probe;$APPS/SportMe-master/target/classes" \
            ClassLoadProbe \
            ru.sberbank.sportme.api.BaseRequest \
            ru.sberbank.sportme.api.BaseResponse
        probes_present=1
    fi
    if [ -d "$APPS/letsgo/letsgo-main/target/classes" ] \
        && [ -f "$REPO_ROOT/test-infra/probes/classload_probe/ClassLoadProbe.class" ]; then
        run_oneshot letsgo "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$REPO_ROOT/test-infra/probes/classload_probe;$APPS/letsgo/letsgo-main/target/classes" \
            ClassLoadProbe \
            com.digsol.main.category.Category
        probes_present=1
    fi
    # DaCapo benchmarks — the Harness Main needs a custom ClassLoader
    # that loads benchmarks from harness/ inside the jar; that boot
    # path fails in our env. Probe via ClassLoadProbe instead so the
    # signal is "Harness.class parses + loads on this JVM".
    [ -f "$APPS/dacapo-9.12-MR1-bach.jar" ] && {
        class_load_test dacapo "$APPS/dacapo-9.12-MR1-bach.jar" Harness; probes_present=1; }

    # ---- Wave 3: Apache + Java-ecosystem libraries via ClassLoadProbe.
    # Each app ships as a maven-central jar (or jar set) under its own
    # apps/<name>/ directory; the probe verifies a representative class
    # from the library's public surface parses + loads on CratonVM.
    [ -f "$APPS/maven-3.9.9/lib/maven-core-3.9.9.jar" ] && {
        # Maven gets a richer probe (model + version) since it's a
        # tool whose API surface we exercise directly elsewhere.
        if [ -f "$REPO_ROOT/test-infra/probes/maven_probe/MavenProbe.class" ]; then
            local mvncp=$(find "$APPS/maven-3.9.9/lib" -name '*.jar' | tr '\n' ';' | sed 's/;$//')
            run_oneshot maven "$TIMEOUT_S" \
                "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
                -c "$REPO_ROOT/test-infra/probes/maven_probe;$mvncp" MavenProbe
            probes_present=1
        fi
    }
    [ -d "$APPS/lucene-9.10.0/modules" ] && {
        class_load_test lucene "$APPS/lucene-9.10.0/modules/lucene-core-9.10.0.jar" \
            org.apache.lucene.util.UnicodeUtil org.apache.lucene.document.Document
        probes_present=1
    }
    if [ -f "$APPS/spring-framework-6/spring-context.jar" ] \
        && [ -f "$REPO_ROOT/test-infra/probes/springfwk_probe/SpringFrameworkProbe.class" ]; then
        local sfcp="$APPS/spring-framework-6/spring-context.jar;$APPS/spring-framework-6/spring-core.jar;$APPS/spring-framework-6/spring-beans.jar;$APPS/spring-framework-6/spring-aop.jar;$APPS/spring-framework-6/spring-expression.jar;$REPO_ROOT/test-infra/spring-libs/jspecify-1.0.0.jar"
        run_oneshot spring_framework "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$REPO_ROOT/test-infra/probes/springfwk_probe;$sfcp" SpringFrameworkProbe
        probes_present=1
    fi
    [ -f "$APPS/hibernate-6/hibernate-core-6.6.0.Final.jar" ] && {
        class_load_test hibernate "$APPS/hibernate-6/hibernate-core-6.6.0.Final.jar" \
            org.hibernate.Version org.hibernate.dialect.H2Dialect
        probes_present=1
    }
    [ -f "$APPS/rabbitmq-client/amqp-client.jar" ] && {
        class_load_test rabbitmq "$APPS/rabbitmq-client/amqp-client.jar" \
            com.rabbitmq.client.Connection com.rabbitmq.client.AMQP\$BasicProperties
        probes_present=1
    }
    [ -f "$APPS/grpc-java/grpc-api.jar" ] && {
        class_load_test grpc "$APPS/grpc-java/grpc-api.jar;$APPS/grpc-java/grpc-core.jar" \
            io.grpc.Status io.grpc.ManagedChannel
        probes_present=1
    }
    [ -f "$APPS/flink/flink-core.jar" ] && {
        class_load_test flink "$APPS/flink/flink-core.jar" \
            org.apache.flink.api.common.JobID org.apache.flink.api.common.ExecutionConfig
        probes_present=1
    }
    [ -f "$APPS/spark/spark-core.jar" ] && {
        class_load_test spark "$APPS/spark/spark-core.jar" \
            org.apache.spark.api.java.JavaSparkContext
        probes_present=1
    }
    [ -f "$APPS/camel/camel-api.jar" ] && {
        # DefaultCamelContext needs transitive deps not on our classpath;
        # the CamelContext interface alone proves the JVM parses the
        # camel-api jar's bytecode + annotation chain.
        class_load_test camel "$APPS/camel/camel-api.jar;$APPS/camel/camel-core.jar" \
            org.apache.camel.CamelContext
        probes_present=1
    }
    [ -f "$APPS/tomee/openejb-core.jar" ] && {
        class_load_test tomee "$APPS/tomee/openejb-core.jar" \
            org.apache.openejb.OpenEJB
        probes_present=1
    }
    [ -f "$APPS/quarkus/quarkus-core.jar" ] && {
        class_load_test quarkus "$APPS/quarkus/quarkus-core.jar" \
            io.quarkus.runtime.Quarkus
        probes_present=1
    }
    [ -f "$APPS/micronaut/micronaut-core.jar" ] && {
        class_load_test micronaut "$APPS/micronaut/micronaut-core.jar" \
            io.micronaut.core.version.VersionUtils
        probes_present=1
    }
    [ -f "$APPS/ignite/ignite-core.jar" ] && {
        class_load_test ignite "$APPS/ignite/ignite-core.jar" \
            org.apache.ignite.IgniteSystemProperties
        probes_present=1
    }
    [ -f "$APPS/hbase/hbase-common.jar" ] && {
        class_load_test hbase "$APPS/hbase/hbase-common.jar" \
            org.apache.hadoop.hbase.HConstants
        probes_present=1
    }
    [ -f "$APPS/neo4j/neo4j-driver.jar" ] && {
        class_load_test neo4j "$APPS/neo4j/neo4j-driver.jar" \
            org.neo4j.driver.GraphDatabase
        probes_present=1
    }
    [ -f "$APPS/elasticsearch/elasticsearch-java.jar" ] && {
        class_load_test elasticsearch "$APPS/elasticsearch/elasticsearch-java.jar" \
            co.elastic.clients.elasticsearch.ElasticsearchClient
        probes_present=1
    }
    [ -f "$APPS/hadoop/hadoop-common.jar" ] && {
        class_load_test hadoop "$APPS/hadoop/hadoop-common.jar" \
            org.apache.hadoop.fs.Path
        probes_present=1
    }
    [ -f "$APPS/jedit5.7.0install.jar" ] && {
        class_load_test jedit "$APPS/jedit5.7.0install.jar" installer.Install
        probes_present=1
    }
    # ---- Wave 4: more master-list apps via ClassLoadProbe.
    [ -f "$APPS/glassfish/glassfish-api.jar" ] && {
        class_load_test glassfish "$APPS/glassfish/glassfish-api.jar" \
            com.sun.appserv.server.LifecycleEvent
        probes_present=1
    }
    [ -f "$APPS/jenkins/jenkins-core.jar" ] && {
        class_load_test jenkins "$APPS/jenkins/jenkins-core.jar" \
            jenkins.ClassLoaderReflectionToolkit
        probes_present=1
    }
    [ -f "$APPS/cas/cas-api.jar" ] && {
        class_load_test cas "$APPS/cas/cas-api.jar" \
            org.apereo.cas.authentication.principal.PrincipalProvisioner
        probes_present=1
    }
    [ -f "$APPS/jhipster/jhipster-framework.jar" ] && {
        class_load_test jhipster "$APPS/jhipster/jhipster-framework.jar" \
            tech.jhipster.config.JHipsterDefaults
        probes_present=1
    }
    [ -f "$APPS/josm/josm.jar" ] && {
        class_load_test josm "$APPS/josm/josm.jar" \
            org.openstreetmap.josm.command.SequenceCommand
        probes_present=1
    }
    [ -f "$APPS/minecraft/minecraft_server.jar" ] && {
        # Minecraft Java Edition server. Uses the bundler-Main entry that
        # in turn launches the real net.minecraft.server.MinecraftServer
        # inside a custom classloader (similar shape to DaCapo's Harness).
        # ClassLoadProbe on net.minecraft.bundler.Main verifies the
        # bundler stage class-loads cleanly on this JVM.
        class_load_test minecraft "$APPS/minecraft/minecraft_server.jar" \
            net.minecraft.bundler.Main
        probes_present=1
    }
    [ -f "$APPS/mindustry/mindustry.jar" ] && {
        class_load_test mindustry "$APPS/mindustry/mindustry.jar" \
            mindustry.server.ServerLauncher
        probes_present=1
    }
    [ -f "$APPS/imagej/imagej.jar" ] && {
        class_load_test imagej "$APPS/imagej/imagej.jar" ij.IJ
        probes_present=1
    }
    [ -f "$APPS/libgdx/gdx.jar" ] && {
        class_load_test libgdx "$APPS/libgdx/gdx.jar" com.badlogic.gdx.Gdx
        probes_present=1
    }
    [ -f "$APPS/liberty/com.ibm.ws.kernel.boot_1.0.91.jar" ] && {
        class_load_test liberty "$APPS/liberty/com.ibm.ws.kernel.boot_1.0.91.jar" \
            com.ibm.ws.kernel.boot.LaunchArguments
        probes_present=1
    }
    [ -f "$APPS/payara/payara-api.jar" ] && {
        class_load_test payara "$APPS/payara/payara-api.jar" \
            fish.payara.cdi.auth.roles.RolesPermitted
        probes_present=1
    }
    # ---- Wave 5: server-side admin / IDE / repo apps via ClassLoadProbe.
    [ -f "$APPS/tomee2/tomee-server.jar" ] && {
        class_load_test tomee2 "$APPS/tomee2/tomee-server.jar" \
            org.apache.tomee.overlay.Deployer
        probes_present=1
    }
    [ -f "$APPS/sonarqube/sonar-plugin-api.jar" ] && {
        class_load_test sonarqube "$APPS/sonarqube/sonar-plugin-api.jar" \
            org.sonar.api.SonarRuntime
        probes_present=1
    }
    [ -f "$APPS/nexus/nexus-bundle.jar" ] && {
        class_load_test nexus "$APPS/nexus/nexus-bundle.jar" \
            org.sonatype.nexus.wonderland.AuthTicketCache
        probes_present=1
    }
    [ -f "$APPS/eclipse/eclipse-jdt.jar" ] && {
        class_load_test eclipse "$APPS/eclipse/eclipse-jdt.jar" \
            org.eclipse.jdt.internal.codeassist.CompletionElementNotifier
        probes_present=1
    }
    [ -f "$APPS/netbeans/netbeans-lookup.jar" ] && {
        class_load_test netbeans "$APPS/netbeans/netbeans-lookup.jar" \
            org.openide.util.Lookup
        probes_present=1
    }
    [ -f "$APPS/jdownloader/jdownloader.jar" ] && {
        class_load_test jdownloader "$APPS/jdownloader/jdownloader.jar" jd.Main
        probes_present=1
    }
    [ -f "$APPS/intellij/util_rt.jar" ] && {
        class_load_test intellij "$APPS/intellij/util_rt.jar" \
            com.intellij.openapi.util.SystemInfoRt
        probes_present=1
    }
    [ -f "$APPS/arduino/arduino-core.jar" ] && {
        class_load_test arduino "$APPS/arduino/arduino-core.jar" \
            cc.arduino.CompilerProgressListener
        probes_present=1
    }

    # Spring Boot probe: builds a non-web SpringApplication via
    # SpringApplicationBuilder and calls .run(), prints a bean it
    # registered, then closes the context. Requires the full Spring
    # framework stack including spring-aop (proxy/AOT-proxy code paths
    # reach AopProxyUtils during context refresh) and spring-expression
    # (StandardBeanExpressionResolver in prepareBeanFactory). Compiled once against
    # Spring Boot 4.0; runs unchanged on 3.x because the
    # SpringApplicationBuilder + Banner.Mode + WebApplicationType APIs
    # are stable.
    if [ -f "$REPO_ROOT/test-infra/probes/springboot_probe/SpringBootProbe.class" ] \
        && [ -f "$REPO_ROOT/test-infra/spring-libs/spring-boot-4.0.6.jar" ]; then
        if [ -f "$APPS/demo/target/demo-0.0.1-SNAPSHOT.jar" ]; then
            local sbcp="$REPO_ROOT/test-infra/probes/springboot_probe;$REPO_ROOT/test-infra/spring-libs/spring-boot-4.0.6.jar;$REPO_ROOT/test-infra/spring-libs/spring-context-7.0.7.jar;$REPO_ROOT/test-infra/spring-libs/spring-core-7.0.7.jar;$REPO_ROOT/test-infra/spring-libs/spring-beans-7.0.7.jar;$REPO_ROOT/test-infra/spring-libs/spring-aop-7.0.7.jar;$REPO_ROOT/test-infra/spring-libs/spring-expression-7.0.7.jar;$REPO_ROOT/test-infra/spring-libs/jspecify-1.0.0.jar"
            run_oneshot springboot_demo "$TIMEOUT_S" \
                "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
                -c "$sbcp" SpringBootProbe
            probes_present=1
        fi
        if [ -f "$APPS/insurance-backend/target/insurance-0.0.1-SNAPSHOT.jar" ] \
            && [ -f "$REPO_ROOT/test-infra/spring-libs/spring-boot-3.2.0.jar" ]; then
            local sbcp="$REPO_ROOT/test-infra/probes/springboot_probe;$REPO_ROOT/test-infra/spring-libs/spring-boot-3.2.0.jar;$REPO_ROOT/test-infra/spring-libs/spring-context-6.1.1.jar;$REPO_ROOT/test-infra/spring-libs/spring-core-6.1.1.jar;$REPO_ROOT/test-infra/spring-libs/spring-beans-6.1.1.jar;$REPO_ROOT/test-infra/spring-libs/spring-aop-6.1.1.jar;$REPO_ROOT/test-infra/spring-libs/spring-expression-6.1.1.jar;$REPO_ROOT/test-infra/spring-libs/jspecify-1.0.0.jar"
            run_oneshot insurance "$TIMEOUT_S" \
                "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
                -c "$sbcp" SpringBootProbe
            probes_present=1
        fi
    fi
    # Tomcat — out-of-tree probe lives under test-infra/probes/tomcat_probe/
    if [ -d "$APPS/apache-tomcat-10.1.31" ] \
        && [ -f "$REPO_ROOT/test-infra/probes/tomcat_probe/TomcatProbe.class" ]; then
        local tcp=$(find "$APPS/apache-tomcat-10.1.31" -name '*.jar' | tr '\n' ';' | sed 's/;$//')
        run_oneshot tomcat "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$REPO_ROOT/test-infra/probes/tomcat_probe;$tcp" TomcatProbe
        probes_present=1
    fi
    # EJBCA — out-of-tree probe drives BouncyCastle key generation + DN parsing
    if [ -d "$APPS/ejbca-ce" ] \
        && [ -f "$REPO_ROOT/test-infra/probes/ejbca_probe/EjbcaProbe.class" ]; then
        local ecp=$(find "$APPS/ejbca-ce/lib" -maxdepth 2 -name '*.jar' | tr '\n' ';' | sed 's/;$//')
        run_oneshot ejbca "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$REPO_ROOT/test-infra/probes/ejbca_probe;$ecp" EjbcaProbe
        probes_present=1
    fi

    [ "$probes_present" -eq 0 ] && echo "(no probes present; skipping)" >&2

    [ -f "$APPS/jedit.jar"     ] && run_oneshot jedit     "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        --jar "$APPS/jedit.jar"
    [ -f "$APPS/hazelcast.jar" ] && run_oneshot hazelcast "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        --jar "$APPS/hazelcast.jar"
    [ -f "$APPS/mindustry.jar" ] && run_oneshot mindustry "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        --jar "$APPS/mindustry.jar"
    [ -f "$APPS/jenkins.war"   ] && run_oneshot jenkins   "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        --jar "$APPS/jenkins.war" -- --version --enable-future-java

    if [ -d "$APPS/jetty-home-11.0.20" ] && [ -f "$REPO_ROOT/test-infra/probes/jetty_probe/JettyProbe.class" ]; then
        # Jetty 11 start.jar requires an enabled-modules set (start.d/*.ini
        # or --add-modules) before any flag prints output — even --help and
        # --version exit non-zero with "No enabled jetty modules found!".
        # The probe instead loads jetty-util's `org.eclipse.jetty.util.Jetty`
        # class (whose static initializer pulls in the slf4j chain and reads
        # build-time version constants) and prints VERSION + POWERED_BY.
        # Pass = the JVM can class-load jetty-util + slf4j cleanly.
        local jcp="$REPO_ROOT/test-infra/probes/jetty_probe;$APPS/jetty-home-11.0.20/lib/jetty-util-11.0.20.jar;$APPS/jetty-home-11.0.20/lib/logging/slf4j-api-2.0.9.jar"
        run_oneshot jetty "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$jcp" JettyProbe
    fi

    if [ -d "$APPS/wlp" ]; then
        run_oneshot liberty "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            --jar "$APPS/wlp/bin/tools/ws-server.jar" -- --version
    fi

    if [ -d "$APPS/apache-activemq-5.18.3" ]; then
        local cp; cp=$(cp_glob "$APPS/apache-activemq-5.18.3/lib")
        [ -n "$cp" ] && run_oneshot activemq "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$cp" org.apache.activemq.console.Main -- --version
    fi

    if [ -d "$APPS/apache-cassandra-4.1.4" ] && [ -f "$REPO_ROOT/test-infra/probes/cassandra_probe/CassandraProbe.class" ]; then
        # Cassandra's `nodetool version` shells out to JMX-over-RMI which
        # requires a working `rmi:` URL stream handler — we don't ship one,
        # so nodetool aborts with "MalformedURLException: Unsupported
        # protocol: rmi" before printing the version. The probe instead
        # reads FBUtilities.getReleaseVersionString() directly, which
        # exercises Cassandra's class init + utils chain without needing
        # a live JMX endpoint.
        local cp; cp=$(cp_glob "$APPS/apache-cassandra-4.1.4/lib")
        [ -n "$cp" ] && run_oneshot cassandra "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$REPO_ROOT/test-infra/probes/cassandra_probe;$cp" CassandraProbe
    fi

    if [ -d "$APPS/apache-ignite-2.16.0-bin" ]; then
        local cp; cp=$(cp_glob "$APPS/apache-ignite-2.16.0-bin/libs")
        [ -n "$cp" ] && run_oneshot ignite "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$cp" "-DIGNITE_HOME=$APPS/apache-ignite-2.16.0-bin" \
            org.apache.ignite.startup.cmdline.CommandLineStartup -- --help
    fi

    if [ -d "$APPS/elasticsearch-8.15.5" ]; then
        local cp; cp=$(cp_glob "$APPS/elasticsearch-8.15.5/lib")
        run_oneshot elasticsearch "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$cp" "-Dcli.name=server" \
            "-Des.path.home=$APPS/elasticsearch-8.15.5" \
            "-Des.path.conf=$APPS/elasticsearch-8.15.5/config" \
            org.elasticsearch.launcher.CliToolLauncher -- --version
    fi

    if [ -d "$APPS/flink-1.18.1" ]; then
        local cp; cp=$(cp_glob "$APPS/flink-1.18.1/lib")
        [ -n "$cp" ] && run_oneshot flink "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$cp" "-DFLINK_HOME=$APPS/flink-1.18.1" \
            org.apache.flink.client.cli.CliFrontend -- --help
    fi

    if [ -d "$APPS/spark-3.5.1-bin-hadoop3" ]; then
        local cp; cp=$(cp_glob "$APPS/spark-3.5.1-bin-hadoop3/jars")
        [ -n "$cp" ] && run_oneshot spark "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$cp" org.apache.spark.deploy.SparkSubmit -- --version
    fi

    if [ -d "$APPS/kafka_2.13-3.7.0" ]; then
        local cp; cp=$(cp_glob "$APPS/kafka_2.13-3.7.0/libs")
        [ -n "$cp" ] && run_oneshot kafka "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$cp" kafka.Kafka --version
    fi

    if [ -d "$APPS/gradle-8.10.2" ]; then
        local cp; cp=$(cp_glob "$APPS/gradle-8.10.2/lib")
        [ -n "$cp" ] && run_oneshot gradle "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$cp" org.gradle.launcher.GradleMain -- --version
    fi

    if [ -d "$APPS/solr-9.5.0" ]; then
        local cp; cp=$(cp_glob \
            "$APPS/solr-9.5.0/server/solr-webapp/webapp/WEB-INF/lib" \
            "$APPS/solr-9.5.0/server/lib/ext")
        [ -n "$cp" ] && run_oneshot solr "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$cp" org.apache.solr.cli.SolrCLI -- version
    fi

    if [ -d "$APPS/neo4j-community-5.18.1" ]; then
        local cp; cp=$(cp_glob "$APPS/neo4j-community-5.18.1/lib")
        [ -n "$cp" ] && run_oneshot neo4j "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$cp" org.neo4j.server.startup.Neo4jBoot -- version
    fi

    # Felix smoke: launching felix.jar directly hangs indefinitely when
    # stdout is a regular file (the Gogo shell + JLine combination blocks
    # somewhere in the activator dispatch that doesn't manifest with a
    # TTY). The probe instead drives the OSGi framework directly via
    # `FrameworkFactory.newFramework().init() ... stop()`, which exercises
    # Felix's class init + bundle resolver + module wiring without
    # touching the interactive shell.
    if [ -d "$APPS/felix-framework-7.0.5" ] && [ -f "$REPO_ROOT/test-infra/probes/felix_probe/FelixProbe.class" ]; then
        run_oneshot felix "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$REPO_ROOT/test-infra/probes/felix_probe;$APPS/felix-framework-7.0.5/bin/felix.jar" \
            FelixProbe
    fi

    # WildFly smoke: route through the wildfly_probe (LocalModuleLoader
    # rooted at modules/, loadModule("org.jboss.logging")) instead of
    # the standalone launcher. The launcher works in smoke for older
    # wildfly releases but the new wildfly-40 distribution stalls past
    # the launcher's --version arg parse in our environment.
    if [ -d "$APPS/wildfly-40.0.0.Final" ] \
        && [ -f "$REPO_ROOT/test-infra/probes/wildfly_probe/WildflyProbe.class" ]; then
        run_oneshot wildfly "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$REPO_ROOT/test-infra/probes/wildfly_probe;$APPS/wildfly-40.0.0.Final/jboss-modules.jar" \
            WildflyProbe "$APPS/wildfly-40.0.0.Final"
    fi

    if [ -d "$APPS/payara6" ]; then
        local cp; cp=$(cp_glob "$APPS/payara6/glassfish/modules")
        [ -n "$cp" ] && run_oneshot payara "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$cp" "-Dcom.sun.aas.installRoot=$APPS/payara6/glassfish" \
            com.sun.enterprise.glassfish.bootstrap.ASMain -- --version
    fi

    if [ -d "$APPS/keycloak-16.1.1" ]; then
        run_oneshot kc16 "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            "-Djboss.home.dir=$APPS/keycloak-16.1.1" \
            --jar "$APPS/keycloak-16.1.1/jboss-modules.jar" \
            -- -mp "$APPS/keycloak-16.1.1/modules" \
            org.jboss.as.standalone --version
    fi

    # KC26 smoke: `quarkus-run.jar show-config` SEGVs ~4s into boot when
    # stdout/stderr go to regular files (the picocli rendering path hits
    # a non-TTY output stream bug we haven't tracked down). The probe
    # reads `org.keycloak.common.Version.NAME / VERSION` directly, which
    # exercises Keycloak's common-lib clinit chain without the picocli
    # rendering path.
    if [ -d "$APPS/keycloak-26.2.4" ] && [ -f "$REPO_ROOT/test-infra/probes/kc26_probe/Keycloak26Probe.class" ]; then
        local kc_cp="$REPO_ROOT/test-infra/probes/kc26_probe;$APPS/keycloak-26.2.4/lib/lib/main/org.keycloak.keycloak-common-26.2.4.jar"
        run_oneshot kc26 "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$kc_cp" Keycloak26Probe
    fi

    if [ -d "$APPS/batch4" ]; then
        [ -f "$APPS/batch4/cas-shell.jar" ] && run_oneshot batch4_cas "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            --jar "$APPS/batch4/cas-shell.jar" -- --help
        [ -f "$APPS/batch4/grpc-examples.jar" ] && run_oneshot batch4_grpc "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$APPS/batch4/grpc-examples.jar" io.grpc.examples.helloworld.HelloWorldServer
        [ -f "$APPS/batch4/perf-test.jar" ] && run_oneshot batch4_rabbitmq "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            --jar "$APPS/batch4/perf-test.jar" -- --help
        [ -f "$APPS/batch4/JDownloader.jar" ] && run_oneshot batch4_jdownloader "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            --jar "$APPS/batch4/JDownloader.jar" -- -h
        if [ -d "$APPS/batch4/freemind_ext/lib" ]; then
            local cp; cp=$(cp_glob "$APPS/batch4/freemind_ext/lib")
            [ -n "$cp" ] && run_oneshot batch4_freemind "$TIMEOUT_S" \
                "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
                -c "$cp" "-Dfreemind.base.dir=$APPS/batch4/freemind_ext" \
                freemind.main.FreeMindStarter
        fi
        if [ -f "$APPS/batch4/nexus-main.jar" ] && [ -f "$APPS/batch4/karaf-main.jar" ]; then
            mkdir -p "$APPS/batch4/karaf-base/etc" "$APPS/batch4/karaf-base/data"
            run_oneshot batch4_nexus "$TIMEOUT_S" \
                "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
                -c "$APPS/batch4/nexus-main.jar;$APPS/batch4/karaf-main.jar;$APPS/batch4/osgi-core.jar" \
                "-Dkaraf.base=$APPS/batch4/karaf-base" \
                "-Dkaraf.home=$APPS/batch4/karaf-base" \
                "-Dkaraf.data=$APPS/batch4/karaf-base/data" \
                "-Dkaraf.etc=$APPS/batch4/karaf-base/etc" \
                "-Djava.io.tmpdir=$APPS/batch4/karaf-base/tmp" \
                org.sonatype.nexus.karaf.NexusMain
        fi
    fi
}

# ----- FUNCTIONAL ----------------------------------------------------------
#
# Daemon + endpoint probe. Each helper launches the app, waits for a
# service-ready log line, runs a single functional probe, then kills.

# --------------------------------------------------------------------------
# Daemon-style helpers (func_*) — these are the canonical retirement gate
# for server/broker/app-server apps. Each launches the real upstream
# daemon, waits for its canonical "ready" log line, probes the canonical
# endpoint, and kills the daemon. A pass here means "the daemon actually
# runs on CratonVM" — not "library code links cleanly".
#
# Library-only probe helpers (probe_*) are kept below as a fallback for
# the non-daemon mode; they exercise library code paths without a live
# daemon. --daemon-only skips them so the retirement gate is strict.
# --------------------------------------------------------------------------

func_activemq() {
    local name=activemq
    in_filter "$name" || return 0
    [ -d "$APPS/apache-activemq-5.18.3" ] || return 0
    local cp; cp=$(cp_glob "$APPS/apache-activemq-5.18.3/lib")
    [ -z "$cp" ] && return 0
    # Real broker start. Last seen failure: XBean → Spring
    # BeanDefinitionParsingException on activemq.xml well before any
    # port-bind. DAEMON_DIED expected on current CratonVM.
    launch_daemon "$name" 'Apache ActiveMQ.*started|Listening for connections' "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        -c "$cp" \
        "-Dactivemq.base=$APPS/apache-activemq-5.18.3" \
        "-Dactivemq.home=$APPS/apache-activemq-5.18.3" \
        "-Dactivemq.conf=$APPS/apache-activemq-5.18.3/conf" \
        "-Dactivemq.data=$APPS/apache-activemq-5.18.3/data" \
        org.apache.activemq.console.Main start \
        xbean:file:"$APPS/apache-activemq-5.18.3/conf/activemq.xml"
    if [ $? -eq 0 ]; then
        if probe_http http://localhost:8161/admin/ 5; then
            echo "$name | rc=0 | broker up, /admin OK"
        else
            echo "$name | rc=1 | broker up, /admin probe failed"
        fi
    fi
    kill_daemon "$name"
}

# Library-only probe (exercises OpenWire marshaller without a broker).
# Kept for non-daemon-only runs as a sanity check on the serialization
# layer. NOT a substitute for daemon pass.
probe_activemq() {
    local name=activemq_probe
    in_filter "$name" || return 0
    if [ -d "$APPS/apache-activemq-5.18.3" ] && [ -f "$REPO_ROOT/test-infra/probes/activemq_probe/ActiveMQProbe.class" ]; then
        local cp; cp=$(cp_glob "$APPS/apache-activemq-5.18.3/lib")
        run_oneshot "$name" "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$REPO_ROOT/test-infra/probes/activemq_probe;$cp" ActiveMQProbe
    fi
}

func_jenkins() {
    local name=jenkins
    in_filter "$name" || return 0
    launch_daemon "$name" 'Jenkins is fully up|Started @' "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        --jar "$APPS/jenkins.war" -- --httpPort=18080 --enable-future-java
    if [ $? -eq 0 ]; then
        if probe_http http://localhost:18080/api/json 10; then
            echo "$name | rc=0 | jenkins up, /api/json OK"
        else
            echo "$name | rc=1 | jenkins up, /api/json probe failed"
        fi
    fi
    kill_daemon "$name"
}

func_wildfly() {
    local name=wildfly
    in_filter "$name" || return 0
    # Try wildfly-32 first (re-staged daemon-blocked app), fall back to
    # wildfly-40 if only the newer dist is on disk. Either way this is
    # the real standalone-mode boot — last seen failure: boots through
    # standalone.xml parse, then stalls in service-container wiring
    # well before WFLYSRV0025.
    local home=""
    for cand in wildfly-32.0.1.Final wildfly-40.0.0.Final; do
        if [ -d "$APPS/$cand" ] && [ -f "$APPS/$cand/jboss-modules.jar" ]; then
            home="$APPS/$cand"
            break
        fi
    done
    [ -z "$home" ] && return 0
    launch_daemon "$name" 'WFLYSRV0025|WildFly.*started in' "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        "-Djboss.home.dir=$home" \
        --jar "$home/jboss-modules.jar" \
        -- -mp "$home/modules" \
        org.jboss.as.standalone
    if [ $? -eq 0 ]; then
        if probe_http http://localhost:9990/management 5; then
            echo "$name | rc=0 | wildfly up, /management OK"
        else
            echo "$name | rc=1 | wildfly up, /management probe failed"
        fi
    fi
    kill_daemon "$name"
}

# Library-only probe (jboss-modules LocalModuleLoader → org.jboss.logging).
# Useful as a smoke-of-the-modulesystem; NOT a substitute for daemon pass.
probe_wildfly() {
    local name=wildfly_probe
    in_filter "$name" || return 0
    local home=""
    for cand in wildfly-32.0.1.Final wildfly-40.0.0.Final; do
        [ -d "$APPS/$cand" ] && [ -f "$APPS/$cand/jboss-modules.jar" ] && {
            home="$APPS/$cand"; break;
        }
    done
    [ -z "$home" ] && return 0
    [ -f "$REPO_ROOT/test-infra/probes/wildfly_probe/WildflyProbe.class" ] || return 0
    run_oneshot "$name" "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        -c "$REPO_ROOT/test-infra/probes/wildfly_probe;$home/jboss-modules.jar" \
        WildflyProbe "$home"
}

func_kc16() {
    local name=kc16
    in_filter "$name" || return 0
    [ -d "$APPS/keycloak-16.1.1" ] || return 0
    # KC16 ships as a WildFly distribution; same daemon shape, same
    # failure mode as wildfly-32 (stalls in service-container wiring
    # before WFLYSRV0025 / Keycloak's own ready line).
    launch_daemon "$name" 'WFLYSRV0025|Keycloak.*started|Started @' "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        "-Djboss.home.dir=$APPS/keycloak-16.1.1" \
        --jar "$APPS/keycloak-16.1.1/jboss-modules.jar" \
        -- -mp "$APPS/keycloak-16.1.1/modules" \
        org.jboss.as.standalone
    if [ $? -eq 0 ]; then
        if probe_http http://localhost:8080/ 5; then
            echo "$name | rc=0 | kc16 up, :8080 OK"
        else
            echo "$name | rc=1 | kc16 up, :8080 probe failed"
        fi
    fi
    kill_daemon "$name"
}

# Library-only probe (jboss-modules → org.keycloak.keycloak-services).
probe_kc16() {
    local name=kc16_probe
    in_filter "$name" || return 0
    if [ -d "$APPS/keycloak-16.1.1" ] && [ -f "$REPO_ROOT/test-infra/probes/kc16_probe/Keycloak16Probe.class" ]; then
        run_oneshot "$name" "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$REPO_ROOT/test-infra/probes/kc16_probe;$APPS/keycloak-16.1.1/jboss-modules.jar" \
            Keycloak16Probe "$APPS/keycloak-16.1.1"
    fi
}

func_ignite() {
    local name=ignite
    in_filter "$name" || return 0
    local cp; cp=$(cp_glob "$APPS/apache-ignite-2.16.0-bin/libs")
    launch_daemon "$name" 'Topology snapshot \[ver=1' "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        -c "$cp" "-DIGNITE_HOME=$APPS/apache-ignite-2.16.0-bin" \
        org.apache.ignite.startup.cmdline.CommandLineStartup \
        "$APPS/apache-ignite-2.16.0-bin/examples/config/example-default.xml"
    [ $? -eq 0 ] && echo "$name | rc=0 | ignite node, topology snapshot"
    kill_daemon "$name"
}

func_cglib_probe() {
    in_filter cglib_probe || return 0
    local cp="$APPS/cglib_probe;$APPS/cglib_probe/cglib-3.3.0.jar;$APPS/cglib_probe/asm-9.5.jar"
    run_oneshot cglib_probe 30 \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        -c "$cp" CglibProbe
}

func_bytebuddy_probe() {
    in_filter bytebuddy_probe || return 0
    local cp="$APPS/bytebuddy_probe;$APPS/bytebuddy_probe/byte-buddy-1.14.18.jar"
    run_oneshot bytebuddy_probe 30 \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        -c "$cp" ByteBuddyProbe
}

func_kafka() {
    local name=kafka
    in_filter "$name" || return 0
    [ -d "$APPS/kafka_2.13-3.7.0" ] || return 0
    local cp; cp=$(cp_glob "$APPS/kafka_2.13-3.7.0/libs")
    [ -z "$cp" ] && return 0
    local props="$APPS/kafka_2.13-3.7.0/config/kraft/server.properties"
    [ -f "$props" ] || props="$APPS/kafka_2.13-3.7.0/config/server.properties"
    [ -f "$props" ] || return 0
    # `kafka.Kafka <props>` boots KRaft single-node. Last seen failure:
    # silent rc=1 after BigInteger fixup, no Kafka Server started.
    launch_daemon "$name" 'Kafka Server started|Awaiting socket connections' "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        -c "$cp" kafka.Kafka "$props"
    if [ $? -eq 0 ]; then
        echo "$name | rc=0 | kafka announced ready"
    fi
    kill_daemon "$name"
}

# Library-only probe (AppInfoParser + Serializer + ConfigDef).
probe_kafka() {
    local name=kafka_probe
    in_filter "$name" || return 0
    if [ -d "$APPS/kafka_2.13-3.7.0" ] && [ -f "$REPO_ROOT/test-infra/probes/kafka_probe/KafkaProbe.class" ]; then
        local cp; cp=$(cp_glob "$APPS/kafka_2.13-3.7.0/libs")
        run_oneshot "$name" "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$REPO_ROOT/test-infra/probes/kafka_probe;$cp" KafkaProbe
    fi
}

func_elasticsearch() {
    local name=elasticsearch
    in_filter "$name" || return 0
    local cp; cp=$(cp_glob "$APPS/elasticsearch-8.15.5/lib")
    launch_daemon "$name" 'started.*9200|Active license|node.*started' \
        "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        -c "$cp" "-Dcli.name=server" \
        "-Des.path.home=$APPS/elasticsearch-8.15.5" \
        "-Des.path.conf=$APPS/elasticsearch-8.15.5/config" \
        org.elasticsearch.launcher.CliToolLauncher
    if [ $? -eq 0 ]; then
        probe_http http://localhost:9200/ 10 \
            && echo "$name | rc=0 | ES up, :9200 responded" \
            || echo "$name | rc=1 | ES up, :9200 probe failed"
    fi
    kill_daemon "$name"
}

func_jetty() {
    local name=jetty
    in_filter "$name" || return 0
    [ -d "$APPS/jetty-home-11.0.20" ] || return 0
    [ -f "$APPS/jetty-home-11.0.20/start.jar" ] || return 0
    # Real start.jar daemon. Requires a configured jetty.base. Last seen:
    # boots through arg parse, then stalls at module enable. Expected
    # outcome on current CratonVM: TIMEOUT_NO_READY.
    launch_daemon "$name" 'Started @|oejs.Server.*Started|Server.*Started' "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        --jar "$APPS/jetty-home-11.0.20/start.jar" \
        -- "jetty.home=$APPS/jetty-home-11.0.20" \
           "jetty.base=$APPS/jetty-home-11.0.20" \
           --modules=http,deploy,resources \
           jetty.http.port=18080
    if [ $? -eq 0 ]; then
        if probe_http http://localhost:18080/ 5; then
            echo "$name | rc=0 | jetty up, :18080 OK"
        else
            echo "$name | rc=1 | jetty up, :18080 probe failed"
        fi
    fi
    kill_daemon "$name"
}

# Library-only probe (Server(0) + AbstractHandler lifecycle).
probe_jetty() {
    local name=jetty_probe
    in_filter "$name" || return 0
    if [ -d "$APPS/jetty-home-11.0.20" ] && [ -f "$REPO_ROOT/test-infra/probes/jetty_probe/JettyFuncProbe.class" ]; then
        local L="$APPS/jetty-home-11.0.20/lib"
        local jcp="$REPO_ROOT/test-infra/probes/jetty_probe;$L/jetty-server-11.0.20.jar;$L/jetty-http-11.0.20.jar;$L/jetty-io-11.0.20.jar;$L/jetty-util-11.0.20.jar;$L/logging/slf4j-api-2.0.9.jar;$L/jetty-jakarta-servlet-api-5.0.2.jar"
        run_oneshot "$name" "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$jcp" JettyFuncProbe
    fi
}

func_cassandra() {
    local name=cassandra
    in_filter "$name" || return 0
    [ -d "$APPS/apache-cassandra-4.1.4" ] || return 0
    local cp; cp=$(cp_glob "$APPS/apache-cassandra-4.1.4/lib")
    [ -z "$cp" ] && return 0
    launch_daemon "$name" 'Listening for native transport|Starting listening for CQL' "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        -c "$cp" \
        "-Dcassandra.config=file:$APPS/apache-cassandra-4.1.4/conf/cassandra.yaml" \
        "-Dcassandra.storagedir=$APPS/apache-cassandra-4.1.4/data" \
        "-Dlogback.configurationFile=$APPS/apache-cassandra-4.1.4/conf/logback.xml" \
        org.apache.cassandra.service.CassandraDaemon
    if [ $? -eq 0 ]; then
        echo "$name | rc=0 | cassandra listening for CQL"
    fi
    kill_daemon "$name"
}

# Library-only probe (FBUtilities / UUIDGen / ByteBufferUtil round-trip).
probe_cassandra() {
    local name=cassandra_probe
    in_filter "$name" || return 0
    if [ -d "$APPS/apache-cassandra-4.1.4" ] && [ -f "$REPO_ROOT/test-infra/probes/cassandra_probe/CassandraFuncProbe.class" ]; then
        local cp; cp=$(cp_glob "$APPS/apache-cassandra-4.1.4/lib")
        run_oneshot "$name" "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$REPO_ROOT/test-infra/probes/cassandra_probe;$cp" CassandraFuncProbe
    fi
}

func_neo4j() {
    local name=neo4j
    in_filter "$name" || return 0
    local cp; cp=$(cp_glob "$APPS/neo4j-community-5.18.1/lib")
    launch_daemon "$name" 'Started\.|Remote interface available at|Bolt enabled' \
        "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        -c "$cp" "-DNEO4J_HOME=$APPS/neo4j-community-5.18.1" \
        org.neo4j.server.startup.Neo4jBoot start
    if [ $? -eq 0 ]; then
        probe_http http://localhost:7474/ 5 \
            && echo "$name | rc=0 | neo4j up, :7474 responded" \
            || echo "$name | rc=1 | neo4j up, :7474 probe failed"
    fi
    kill_daemon "$name"
}

func_felix() {
    local name=felix
    in_filter "$name" || return 0
    [ -d "$APPS/felix-framework-7.0.5" ] || return 0
    # Real Gogo shell launch. Last seen failure: hangs on non-TTY stdout
    # (kill_daemon will reap; expected outcome is TIMEOUT_NO_READY).
    launch_daemon "$name" 'g!|Welcome to Apache Felix Gogo|Felix.*started' "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        --jar "$APPS/felix-framework-7.0.5/bin/felix.jar"
    if [ $? -eq 0 ]; then
        echo "$name | rc=0 | felix Gogo prompt reached"
    fi
    kill_daemon "$name"
}

# Library-only probe (FrameworkFactory init/start/stop lifecycle).
probe_felix() {
    local name=felix_probe
    in_filter "$name" || return 0
    if [ -d "$APPS/felix-framework-7.0.5" ] && [ -f "$REPO_ROOT/test-infra/probes/felix_probe/FelixFuncProbe.class" ]; then
        run_oneshot "$name" "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$REPO_ROOT/test-infra/probes/felix_probe;$APPS/felix-framework-7.0.5/bin/felix.jar" \
            FelixFuncProbe
    fi
}

func_hazelcast() {
    local name=hazelcast
    in_filter "$name" || return 0
    [ -f "$APPS/hazelcast.jar" ] || { [ -d "$APPS/hazelcast-5.4.0" ] || return 0; }
    local jar="$APPS/hazelcast.jar"
    [ -f "$jar" ] || return 0
    launch_daemon "$name" 'STARTED|Members \{size:1|is STARTED' "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        --jar "$jar"
    if [ $? -eq 0 ]; then
        echo "$name | rc=0 | hazelcast member STARTED"
    fi
    kill_daemon "$name"
}

# Library-only probe (BuildInfoProvider + Config/NetworkConfig).
probe_hazelcast() {
    local name=hazelcast_probe
    in_filter "$name" || return 0
    if [ -d "$APPS/hazelcast-5.4.0" ] && [ -f "$REPO_ROOT/test-infra/probes/hazelcast_probe/HazelcastProbe.class" ]; then
        local cp; cp=$(cp_glob "$APPS/hazelcast-5.4.0/lib")
        run_oneshot "$name" "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$REPO_ROOT/test-infra/probes/hazelcast_probe;$cp" HazelcastProbe
    fi
}

func_liberty() {
    local name=liberty
    in_filter "$name" || return 0
    launch_daemon "$name" 'CWWKF0011I|Server defaultServer is ready|open for e-business' \
        "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        --jar "$APPS/wlp/bin/tools/ws-server.jar" -- defaultServer
    if [ $? -eq 0 ]; then
        probe_http http://localhost:9080/ 5 \
            && echo "$name | rc=0 | liberty up, :9080 responded" \
            || echo "$name | rc=1 | liberty up, :9080 probe failed"
    fi
    kill_daemon "$name"
}

func_payara() {
    local name=payara
    in_filter "$name" || return 0
    local cp; cp=$(cp_glob "$APPS/payara6/glassfish/modules")
    launch_daemon "$name" 'started in|Listening on port|GlassFish .* started' \
        "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        -c "$cp" "-Dcom.sun.aas.installRoot=$APPS/payara6/glassfish" \
        com.sun.enterprise.glassfish.bootstrap.ASMain
    if [ $? -eq 0 ]; then
        probe_http http://localhost:4848/ 5 \
            && echo "$name | rc=0 | payara up, :4848 responded" \
            || echo "$name | rc=1 | payara up, :4848 probe failed"
    fi
    kill_daemon "$name"
}

func_kc26() {
    local name=kc26
    in_filter "$name" || return 0
    [ -d "$APPS/keycloak-26.2.4" ] || return 0
    local jar="$APPS/keycloak-26.2.4/lib/quarkus-run.jar"
    [ -f "$jar" ] || return 0
    # `quarkus-run.jar start-dev` SEGVs ~4s into boot on non-TTY stdout.
    # That manifests as DAEMON_DIED before any "Listening on:" line.
    launch_daemon "$name" 'Listening on:|Profile dev activated|Keycloak.*started' "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        --jar "$jar" -- start-dev
    if [ $? -eq 0 ]; then
        if probe_http http://localhost:8080/ 5; then
            echo "$name | rc=0 | kc26 up, :8080 OK"
        else
            echo "$name | rc=1 | kc26 up, :8080 probe failed"
        fi
    fi
    kill_daemon "$name"
}

# Library-only probe (Profile.Feature enumeration; no quarkus runtime).
probe_kc26() {
    local name=kc26_probe
    in_filter "$name" || return 0
    if [ -d "$APPS/keycloak-26.2.4" ] && [ -f "$REPO_ROOT/test-infra/probes/kc26_probe/Keycloak26FuncProbe.class" ]; then
        local kc_cp="$REPO_ROOT/test-infra/probes/kc26_probe;$APPS/keycloak-26.2.4/lib/lib/main/org.keycloak.keycloak-common-26.2.4.jar"
        run_oneshot "$name" "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$kc_cp" Keycloak26FuncProbe
    fi
}

# Spring Boot daemon attempt — banner-only on non-TTY stdout (same root
# cause as kc26: another agent's non-TTY SEGV/hang fix unblocks this).
# Targets a tiny Boot 4.0 hello-world under apps/demo/ if present.
func_springboot() {
    local name=springboot
    in_filter "$name" || return 0
    local jar=""
    for cand in \
        "$APPS/demo/target/demo-0.0.1-SNAPSHOT.jar" \
        "$APPS/insurance-backend/target/insurance-0.0.1-SNAPSHOT.jar"; do
        [ -f "$cand" ] && { jar="$cand"; break; }
    done
    [ -z "$jar" ] && return 0
    launch_daemon "$name" 'Started .* in .* seconds|Tomcat started on|Netty started on' "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        --jar "$jar"
    if [ $? -eq 0 ]; then
        if probe_http http://localhost:8080/ 5; then
            echo "$name | rc=0 | springboot up, :8080 OK"
        else
            echo "$name | rc=1 | springboot up, :8080 probe failed"
        fi
    fi
    kill_daemon "$name"
}

func_flink() {
    local name=flink
    in_filter "$name" || return 0
    local cp; cp=$(cp_glob "$APPS/flink-1.18.1/lib")
    local example="$APPS/flink-1.18.1/examples/streaming/WordCount.jar"
    if [ -f "$example" ]; then
        run_oneshot "$name" 60 \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$cp;$example" "-DFLINK_HOME=$APPS/flink-1.18.1" \
            org.apache.flink.streaming.examples.wordcount.WordCount
    else
        echo "$name | rc=skip | example WordCount.jar not found"
    fi
}

func_spark() {
    local name=spark
    in_filter "$name" || return 0
    local cp; cp=$(cp_glob "$APPS/spark-3.5.1-bin-hadoop3/jars")
    local example="$APPS/spark-3.5.1-bin-hadoop3/examples/jars/spark-examples_2.12-3.5.1.jar"
    if [ -f "$example" ]; then
        run_oneshot "$name" 120 \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$cp;$example" org.apache.spark.deploy.SparkSubmit \
            -- --class org.apache.spark.examples.SparkPi \
               --master "local[1]" "$example" 10
    else
        echo "$name | rc=skip | spark-examples jar not found"
    fi
}

func_gradle() {
    local name=gradle
    in_filter "$name" || return 0
    local cp; cp=$(cp_glob "$APPS/gradle-8.10.2/lib")
    run_oneshot "$name" 60 \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        -c "$cp" org.gradle.launcher.GradleMain -- --version
}

# Library-only probe (SolrInputDocument + DocumentObjectBinder).
# Solr's real `bin/solr start` forks Jetty as a subprocess and doesn't
# have a single-jar daemon entry — so the daemon-only criterion isn't
# applicable here. Library-probe pass is the strongest signal we have
# until we either (a) wire start-solr via Jetty embedded or (b) fix the
# spawn path the bin/solr shell uses.
probe_solr() {
    local name=solr_probe
    in_filter "$name" || return 0
    if [ -d "$APPS/solr-9.5.0" ] && [ -f "$REPO_ROOT/test-infra/probes/solr_probe/SolrProbe.class" ]; then
        local cp; cp=$(cp_glob \
            "$APPS/solr-9.5.0/server/solr-webapp/webapp/WEB-INF/lib" \
            "$APPS/solr-9.5.0/server/lib/ext")
        run_oneshot "$name" "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$REPO_ROOT/test-infra/probes/solr_probe;$cp" SolrProbe
    fi
}

run_functional() {
    # Daemon-style helpers (func_*): real upstream daemon → ready log →
    # endpoint probe → kill. These are the canonical retirement gate
    # for daemon/server apps per apps/TARGET_APPS.md. Expected outcome
    # on current CratonVM: DAEMON_DIED / TIMEOUT_NO_READY for the
    # apps on the "Daemon-blocked apps" list in TARGET_APPS.md.
    func_activemq
    func_cassandra
    func_felix
    func_hazelcast
    func_jetty
    func_kafka
    func_kc16
    func_kc26
    func_springboot
    func_wildfly

    # Library-only probes (probe_*): exercise the app's library code
    # paths without a live daemon. NOT a substitute for daemon pass —
    # use --daemon-only to skip these and assert the strict criterion.
    # Also runs solr_probe (no daemon shape — see comment on probe_solr).
    if [ "$DAEMON_ONLY" -eq 0 ]; then
        probe_activemq
        probe_cassandra
        probe_felix
        probe_hazelcast
        probe_jetty
        probe_kafka
        probe_kc16
        probe_kc26
        probe_solr
        probe_wildfly
    fi
}

# ----- RECURSIVE -----------------------------------------------------------
#
# Walk apps/ recursively, find Main-Class jars, run each.

# discover_one PATH — print "<rel>\t<main_class>" if the jar declares one.
discover_one() {
    local rel="$1"
    local jar="$SCAN_DIR/$rel"
    unzip -l "$jar" META-INF/MANIFEST.MF 2>/dev/null \
        | grep -q 'META-INF/MANIFEST.MF' || return 0
    unzip -p "$jar" META-INF/MANIFEST.MF 2>/dev/null \
        | head -c 8192 \
        | tr -d '\r' \
        | awk -v rel="$rel" '/^Main-Class:/ {
              v = substr($0, 13); gsub(/^ +| +$/, "", v);
              if (v != "") print rel "\t" v; exit
          }'
}
export -f discover_one

# run_recursive_one REL_PATH — execute one jar; classify; append to CSV.
run_recursive_one() {
    local rel="$1"
    local jar="$SCAN_DIR/$rel"

    local mb bytes
    bytes=$(stat -c '%s' "$jar" 2>/dev/null) || bytes=0
    mb=$((bytes / 1024 / 1024))
    if [ "$mb" -gt "$MAX_SIZE_MB" ]; then
        echo "$rel,$mb,SKIP,oversize,0,skip-too-big," >> "$CSV"
        return
    fi

    # Filter (substring match on rel)
    if [ -n "$FILTER" ]; then
        local IFS=, want hit=0
        for want in $FILTER; do
            echo "$rel" | grep -qF "$want" && { hit=1; break; }
        done
        if [ "$hit" -eq 0 ]; then
            echo "$rel,$mb,,filter,0,skip-filter," >> "$CSV"
            return
        fi
    fi

    local main
    main=$(grep -F "$rel	" "$LOGDIR/discover.tsv" | head -1 | cut -f2)
    if [ -z "$main" ]; then
        echo "$rel,$mb,,nomain,0,skip-no-main," >> "$CSV"
        return
    fi

    local safe
    safe=$(echo "$rel" | tr '/\\' '__')
    local out="$LOGDIR/$safe.out"
    local err="$LOGDIR/$safe.err"

    local t0 t1
    t0=$(date +%s)
    timeout --foreground -k 5 "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        --jar "$jar" \
        > "$out" 2> "$err"
    local rc=$?
    t1=$(date +%s)
    local elapsed=$((t1 - t0))

    local first_err
    first_err=$(first_err_line "$err")
    local cls
    cls=$(classify_run "$rc" "$first_err")
    local err_csv
    err_csv=$(echo "$first_err" | tr -d '"' | tr ',' ' ' | head -c 200)
    echo "$rel,$mb,$main,$rc,$elapsed,$cls,$err_csv" >> "$CSV"

    local line
    line=$(printf "%-60.60s | rc=%-3s | %5ds | %-9s | %s" \
        "$rel" "$rc" "$elapsed" "$cls" "$first_err")
    echo "$line"
    echo "$line" >> "$RUN_LOG"
}

run_recursive() {
    SCAN_DIR="$APPS"
    export SCAN_DIR

    echo "Scanning $SCAN_DIR for JARs/WARs (timeout=${TIMEOUT_S}s, max-size=${MAX_SIZE_MB}MB)..." | tee -a "$RUN_LOG"
    mapfile -t ALL_JARS < <(cd "$SCAN_DIR" && find . -type f \
        \( -name '*.jar' -o -name '*.war' \) | sed 's|^\./||' | sort)
    echo "Found ${#ALL_JARS[@]} jar/war files." | tee -a "$RUN_LOG"

    echo "Discovering Main-Class entries (parallel, 8-way xargs)..." | tee -a "$RUN_LOG"
    printf '%s\n' "${ALL_JARS[@]}" \
        | xargs -P 8 -I {} bash -c 'discover_one "$@"' _ {} \
        > "$LOGDIR/discover.tsv"
    mapfile -t RUNNABLE < <(cut -f1 "$LOGDIR/discover.tsv" | sort)
    echo "${#RUNNABLE[@]} of those declare Main-Class." | tee -a "$RUN_LOG"

    if [ "$MAX_RUNS" -gt 0 ] && [ "${#RUNNABLE[@]}" -gt "$MAX_RUNS" ]; then
        RUNNABLE=("${RUNNABLE[@]:0:$MAX_RUNS}")
        echo "Limited to first $MAX_RUNS runs." | tee -a "$RUN_LOG"
    fi

    local count=0
    for rel in "${RUNNABLE[@]}"; do
        count=$((count + 1))
        printf "[%3d/%3d] " "$count" "${#RUNNABLE[@]}" >&2
        run_recursive_one "$rel"
    done
}

# ============================================================================
# Dispatch
# ============================================================================

# Set up per-run log dir + CSV per mode and run.
run_one_mode() {
    local m="$1"
    LOGDIR="$REPO_ROOT/applogs/orchestrator-$m-$LABEL"
    mkdir -p "$LOGDIR"
    CSV="$LOGDIR/results.csv"
    RUN_LOG="$LOGDIR/run.log"
    SUMMARY="$LOGDIR/summary.txt"

    echo "rel_path,size_mb,main_class,rc,elapsed_s,classification,first_error" > "$CSV"
    echo ">>> Mode: $m  log dir: $LOGDIR" | tee "$RUN_LOG"

    local t_start t_end
    t_start=$(date +%s)
    case "$m" in
        smoke)      run_smoke ;;
        functional) run_functional ;;
        recursive)  run_recursive ;;
    esac
    t_end=$(date +%s)
    echo "Total wall: $((t_end - t_start))s" | tee -a "$RUN_LOG" >> "$SUMMARY"

    print_summary "$CSV"
    echo "=== $m logs in $LOGDIR ==="
}

case "$MODE" in
    smoke|functional|recursive)
        run_one_mode "$MODE"
        ;;
    all)
        ORIG_TIMEOUT="$TIMEOUT_S"; ORIG_XMX="$XMX"
        TIMEOUT_S=30;  XMX=512m;        run_one_mode smoke
        TIMEOUT_S=240; XMX=1g;          run_one_mode functional
        TIMEOUT_S=20;  XMX=512m;        run_one_mode recursive
        TIMEOUT_S="$ORIG_TIMEOUT"; XMX="$ORIG_XMX"
        ;;
esac
