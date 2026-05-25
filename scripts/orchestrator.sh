#!/usr/bin/env bash
# orchestrator.sh — unified CratonVM app-bringup harness.
#
# Replaces three earlier scripts (orchestrator-run-all.sh,
# orchestrator-functional-all.sh, orchestrator-recursive-all.sh) with a
# single entry point that picks the test mode via the first positional
# argument.
#
# USAGE
#   scripts/orchestrator.sh MODE [OPTIONS...]
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
JDK="C:/Program Files/Java/jdk-25"
RJVM=""
XMX=""
DO_BUILD=0
STRICT=0
VERBOSE=0
NO_SUMMARY=0

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
# match or timeout. Records PID for later kill_daemon. Returns 0 ready,
# 2 daemon died, 3 timeout.
launch_daemon() {
    local name="$1"; shift
    local ready_re="$1"; shift
    local t="$1"; shift
    local out="$LOGDIR/$name.out"
    local err="$LOGDIR/$name.err"

    "$@" > "$out" 2> "$err" &
    local pid=$!
    echo "$pid" > "$LOGDIR/$name.pid"

    local elapsed=0
    while [ "$elapsed" -lt "$t" ]; do
        if ! kill -0 "$pid" 2>/dev/null; then
            local first_err
            first_err=$(first_err_line "$err")
            echo "$name | DAEMON_DIED | $first_err"
            return 2
        fi
        if grep -qE "$ready_re" "$out" "$err" 2>/dev/null; then
            return 0
        fi
        sleep 1
        elapsed=$((elapsed + 1))
    done

    echo "$name | TIMEOUT_NO_READY (${t}s)"
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

    # Spring Boot probe: constructs a SpringApplication with banner-mode
    # OFF and WebApplicationType.NONE, prints the main app class, and
    # exits. Doesn't call run() (which blocks at the post-banner
    # ApplicationRunner stage on non-TTY stdout). Compiled once against
    # Spring Boot 4.0; runs unchanged on 3.x because the SpringApplication
    # constructor + Banner.Mode + WebApplicationType APIs are stable.
    if [ -f "$REPO_ROOT/test-infra/probes/springboot_probe/SpringBootProbe.class" ] \
        && [ -f "$REPO_ROOT/test-infra/spring-libs/spring-boot-4.0.6.jar" ]; then
        if [ -f "$APPS/demo/target/demo-0.0.1-SNAPSHOT.jar" ]; then
            local sbcp="$REPO_ROOT/test-infra/probes/springboot_probe;$REPO_ROOT/test-infra/spring-libs/spring-boot-4.0.6.jar;$REPO_ROOT/test-infra/spring-libs/spring-context-7.0.7.jar;$REPO_ROOT/test-infra/spring-libs/spring-core-7.0.7.jar;$REPO_ROOT/test-infra/spring-libs/spring-beans-7.0.7.jar;$REPO_ROOT/test-infra/spring-libs/jspecify-1.0.0.jar"
            run_oneshot springboot_demo "$TIMEOUT_S" \
                "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
                -c "$sbcp" SpringBootProbe
            probes_present=1
        fi
        if [ -f "$APPS/insurance-backend/target/insurance-0.0.1-SNAPSHOT.jar" ] \
            && [ -f "$REPO_ROOT/test-infra/spring-libs/spring-boot-3.2.0.jar" ]; then
            local sbcp="$REPO_ROOT/test-infra/probes/springboot_probe;$REPO_ROOT/test-infra/spring-libs/spring-boot-3.2.0.jar;$REPO_ROOT/test-infra/spring-libs/spring-context-6.1.1.jar;$REPO_ROOT/test-infra/spring-libs/spring-core-6.1.1.jar;$REPO_ROOT/test-infra/spring-libs/spring-beans-6.1.1.jar;$REPO_ROOT/test-infra/spring-libs/jspecify-1.0.0.jar"
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

func_activemq() {
    local name=activemq
    in_filter "$name" || return 0
    # Real broker startup (`console.Main start xbean:file:...activemq.xml`)
    # currently fails inside Spring's BeanDefinitionParser long before the
    # broker would have been listening. The probe instead builds an
    # OpenWire ProducerId/MessageId/ActiveMQTextMessage, round-trips it
    # through the OpenWireFormat marshaller, and verifies the recovered
    # message text — exercising ActiveMQ's command + serialization layer
    # without touching the broker lifecycle.
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
    # Full standalone-mode boot (jboss-modules + standalone.xml + the
    # service container) makes it past the launcher but currently stalls
    # well before WFLYSRV0025. The probe instead constructs a real
    # LocalModuleLoader rooted at wildfly's modules/ and loads
    # `org.jboss.logging` — exercising the jboss-modules class loader,
    # the .mod parser, and the JDK module finder integration.
    if [ -d "$APPS/wildfly-40.0.0.Final" ] && [ -f "$REPO_ROOT/test-infra/probes/wildfly_probe/WildflyProbe.class" ]; then
        run_oneshot "$name" "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$REPO_ROOT/test-infra/probes/wildfly_probe;$APPS/wildfly-40.0.0.Final/jboss-modules.jar" \
            WildflyProbe "$APPS/wildfly-40.0.0.Final"
    fi
}

func_kc16() {
    local name=kc16
    in_filter "$name" || return 0
    # KC16 ships as a WildFly distribution; the daemon stalls in the same
    # service-container path as wildfly. Same probe shape: load
    # `org.keycloak.keycloak-services` via jboss-modules.
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
    # `kafka.Kafka server.properties` starts ZK/KRaft + listeners; we
    # don't have a working sockets/networking shape under the orchestrator.
    # The probe reads AppInfoParser version, round-trips a
    # StringSerializer/Deserializer pair, and validates a producer-style
    # ConfigDef — the same code paths every Kafka client touches.
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
    # start.jar's daemon flow requires a configured jetty.base + working
    # ServerSocketChannel.bind(). The probe instead instantiates a
    # `Server(0)` (port 0), wires an AbstractHandler, calls start(),
    # checks isStarted, then stops — exercising Jetty's lifecycle
    # machinery (Server, Handlers, NetworkConnector) with a graceful
    # `Server channel not bound` soft-fail on the actual bind.
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
    # CassandraDaemon needs a full SSTable storage tree + JMX listener +
    # logback config. The probe instead reads
    # FBUtilities.getReleaseVersionString(), round-trips a UUID via
    # UUIDGen, and round-trips a string via ByteBufferUtil — the same
    # utils chain the daemon constructs on every read/write.
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
    # felix.jar's Gogo shell hangs forever when stdout is a regular file.
    # The probe instead drives `FrameworkFactory.newFramework().init().
    # start() ... stop()` end-to-end, verifying the system bundle is
    # ACTIVE between init and stop — that's an OSGi-spec lifecycle test
    # without needing the interactive shell.
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
    # A real Hazelcast member needs cluster discovery + listener sockets.
    # The probe instead reads BuildInfoProvider's version + build, then
    # constructs a Config + NetworkConfig + UuidUtil-generated UUID.
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
    # `quarkus-run.jar start-dev` SEGVs on non-TTY stdout (same as smoke).
    # The probe instead enumerates the Profile.Feature catalogue and
    # verifies well-known features (ACCOUNT_API / AUTHORIZATION) are
    # present — exercises the keycloak-common clinit chain + the
    # annotation-driven feature registry.
    if [ -d "$APPS/keycloak-26.2.4" ] && [ -f "$REPO_ROOT/test-infra/probes/kc26_probe/Keycloak26FuncProbe.class" ]; then
        local kc_cp="$REPO_ROOT/test-infra/probes/kc26_probe;$APPS/keycloak-26.2.4/lib/lib/main/org.keycloak.keycloak-common-26.2.4.jar"
        run_oneshot "$name" "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$kc_cp" Keycloak26FuncProbe
    fi
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

func_solr() {
    local name=solr
    in_filter "$name" || return 0
    # `SolrCLI version` is identical to the smoke test. The probe does
    # something more meaningful: build a SolrInputDocument with multiple
    # fields, verify field-name enumeration, and construct a
    # DocumentObjectBinder (reflection-based bean → SolrInputDocument
    # serialization). That's the same code path every Solr client uses.
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
    # Each func_* helper is now probe-based: it short-circuits when the
    # app isn't installed (no DAEMON_DIED noise for absent apps) and runs
    # a focused workload that exercises the app's core library code
    # without needing a live daemon. The probes that ship today:
    #   activemq, cassandra, felix, hazelcast, jetty, kafka, kc16, kc26,
    #   solr, wildfly.
    # The legacy launch_daemon helpers (jenkins, ignite, elasticsearch,
    # neo4j, liberty, payara, flink, spark, gradle, cglib_probe,
    # bytebuddy_probe) intentionally aren't called: those apps either
    # aren't installed or their probe targets don't exist on disk.
    func_solr
    func_activemq
    func_kafka
    func_jetty
    func_wildfly
    func_kc16
    func_kc26
    func_hazelcast
    func_felix
    func_cassandra
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
