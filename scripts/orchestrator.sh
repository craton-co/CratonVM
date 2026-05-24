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
    timeout --foreground -k 5 "$t" "$@" \
        > "$LOGDIR/$name.out" 2> "$LOGDIR/$name.err"
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

run_smoke() {
    local probes_present=0

    if [ -f "$APPS/bytebuddy_probe/ByteBuddyProbe.class" ]; then
        local cp="$APPS/bytebuddy_probe;$APPS/bytebuddy_probe/byte-buddy-1.14.18.jar"
        run_oneshot bytebuddy_probe "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$cp" ByteBuddyProbe
        probes_present=1
    fi

    if [ -f "$APPS/cglib_probe/CglibProbe.class" ]; then
        local cp="$APPS/cglib_probe;$APPS/cglib_probe/cglib-3.3.0.jar;$APPS/cglib_probe/asm-9.5.jar"
        run_oneshot cglib_probe "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$cp" CglibProbe
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

    if [ -d "$APPS/jetty-home-11.0.20" ]; then
        run_oneshot jetty "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            --jar "$APPS/jetty-home-11.0.20/start.jar" -- --list-config
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

    if [ -d "$APPS/apache-cassandra-4.1.4" ]; then
        local cp; cp=$(cp_glob "$APPS/apache-cassandra-4.1.4/lib")
        [ -n "$cp" ] && run_oneshot cassandra "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            -c "$cp" org.apache.cassandra.tools.NodeTool -- version
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
            -c "$cp" kafka.Kafka
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

    [ -d "$APPS/felix-framework-7.0.5" ] && run_oneshot felix "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        --jar "$APPS/felix-framework-7.0.5/bin/felix.jar"

    if [ -d "$APPS/wildfly-32.0.1.Final" ]; then
        run_oneshot wildfly "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            "-Djboss.home.dir=$APPS/wildfly-32.0.1.Final" \
            --jar "$APPS/wildfly-32.0.1.Final/jboss-modules.jar" \
            -- -mp "$APPS/wildfly-32.0.1.Final/modules" \
            org.jboss.as.standalone --version
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

    if [ -d "$APPS/keycloak-26.2.4" ]; then
        run_oneshot kc26 "$TIMEOUT_S" \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
            --jar "$APPS/keycloak-26.2.4/lib/quarkus-run.jar" -- show-config
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
    local cp; cp=$(cp_glob "$APPS/apache-activemq-5.18.3/lib")
    launch_daemon "$name" 'Apache ActiveMQ.*started|Listening for connections' \
        "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        -c "$cp" \
        "-Dactivemq.base=$APPS/apache-activemq-5.18.3" \
        "-Dactivemq.home=$APPS/apache-activemq-5.18.3" \
        "-Dactivemq.conf=$APPS/apache-activemq-5.18.3/conf" \
        "-Dactivemq.data=$APPS/apache-activemq-5.18.3/data" \
        org.apache.activemq.console.Main start \
        "xbean:file:$APPS/apache-activemq-5.18.3/conf/activemq.xml"
    if [ $? -eq 0 ]; then
        if probe_http http://localhost:8161/admin/ 5; then
            echo "$name | rc=0 | broker started, web admin responded"
        else
            echo "$name | rc=1 | broker started, web admin probe failed"
        fi
    fi
    kill_daemon "$name"
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
    launch_daemon "$name" 'WildFly.*started in|WFLYSRV0025' "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        "-Djboss.home.dir=$APPS/wildfly-32.0.1.Final" \
        --jar "$APPS/wildfly-32.0.1.Final/jboss-modules.jar" \
        -- -mp "$APPS/wildfly-32.0.1.Final/modules" org.jboss.as.standalone
    if [ $? -eq 0 ]; then
        if probe_http http://localhost:9990/management 5; then
            echo "$name | rc=0 | wildfly up, :9990 responded"
        else
            echo "$name | rc=1 | wildfly up, :9990 probe failed"
        fi
    fi
    kill_daemon "$name"
}

func_kc16() {
    local name=kc16
    in_filter "$name" || return 0
    launch_daemon "$name" 'WildFly.*started in|WFLYSRV0025|Keycloak.*started' \
        "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        "-Djboss.home.dir=$APPS/keycloak-16.1.1" \
        --jar "$APPS/keycloak-16.1.1/jboss-modules.jar" \
        -- -mp "$APPS/keycloak-16.1.1/modules" org.jboss.as.standalone
    if [ $? -eq 0 ]; then
        probe_http http://localhost:9990/management 5 \
            && echo "$name | rc=0 | kc16 up, :9990 responded" \
            || echo "$name | rc=1 | kc16 up, :9990 probe failed"
    fi
    kill_daemon "$name"
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
    local cp; cp=$(cp_glob "$APPS/kafka_2.13-3.7.0/libs")
    launch_daemon "$name" 'Kafka Server started|Awaiting socket connections' \
        "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        -c "$cp" kafka.Kafka \
        "$APPS/kafka_2.13-3.7.0/config/server.properties"
    [ $? -eq 0 ] && echo "$name | rc=0 | kafka announced ready"
    kill_daemon "$name"
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
    launch_daemon "$name" 'Started @|oejs.Server.*Started' "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        --jar "$APPS/jetty-home-11.0.20/start.jar" \
        -- "jetty.home=$APPS/jetty-home-11.0.20" \
           "jetty.base=$APPS/jetty-home-11.0.20" \
           --modules=http,deploy,resources jetty.http.port=18080
    if [ $? -eq 0 ]; then
        probe_http http://localhost:18080/ 5 \
            && echo "$name | rc=0 | jetty up, :18080 responded" \
            || echo "$name | rc=1 | jetty up, :18080 probe failed"
    fi
    kill_daemon "$name"
}

func_cassandra() {
    local name=cassandra
    in_filter "$name" || return 0
    local cp; cp=$(cp_glob "$APPS/apache-cassandra-4.1.4/lib")
    launch_daemon "$name" 'Listening for native transport|Starting listening for CQL' \
        "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        -c "$cp" \
        "-Dcassandra.config=file:$APPS/apache-cassandra-4.1.4/conf/cassandra.yaml" \
        "-Dcassandra.storagedir=$APPS/apache-cassandra-4.1.4/data" \
        "-Dlogback.configurationFile=$APPS/apache-cassandra-4.1.4/conf/logback.xml" \
        org.apache.cassandra.service.CassandraDaemon
    [ $? -eq 0 ] && echo "$name | rc=0 | cassandra listening"
    kill_daemon "$name"
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
    launch_daemon "$name" 'g!|Welcome to Apache Felix Gogo' 60 \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        --jar "$APPS/felix-framework-7.0.5/bin/felix.jar"
    [ $? -eq 0 ] && echo "$name | rc=0 | felix gogo prompt"
    kill_daemon "$name"
}

func_hazelcast() {
    local name=hazelcast
    in_filter "$name" || return 0
    launch_daemon "$name" 'STARTED|Cluster name:|Members \{size:1' 90 \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        --jar "$APPS/hazelcast.jar"
    [ $? -eq 0 ] && echo "$name | rc=0 | hazelcast STARTED"
    kill_daemon "$name"
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
    launch_daemon "$name" 'Listening on:|Profile dev activated|Keycloak.*started' \
        "$TIMEOUT_S" \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        --jar "$APPS/keycloak-26.2.4/lib/quarkus-run.jar" -- start-dev
    if [ $? -eq 0 ]; then
        probe_http http://localhost:8080/ 5 \
            && echo "$name | rc=0 | kc26 up, :8080 responded" \
            || echo "$name | rc=1 | kc26 up, :8080 probe failed"
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

func_solr() {
    local name=solr
    in_filter "$name" || return 0
    local cp; cp=$(cp_glob \
        "$APPS/solr-9.5.0/server/solr-webapp/webapp/WEB-INF/lib" \
        "$APPS/solr-9.5.0/server/lib/ext")
    run_oneshot "$name" 60 \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx "$XMX" \
        -c "$cp" org.apache.solr.cli.SolrCLI -- version
}

run_functional() {
    func_cglib_probe
    func_bytebuddy_probe
    func_gradle
    func_solr
    func_flink
    func_spark
    func_activemq
    func_jenkins
    func_ignite
    func_kafka
    func_jetty
    func_liberty
    func_payara
    func_wildfly
    func_kc16
    func_kc26
    func_hazelcast
    func_felix
    func_elasticsearch
    func_cassandra
    func_neo4j
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
