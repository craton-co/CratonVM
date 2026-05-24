#!/usr/bin/env bash
# Functional test runner — for each app folder under apps/, launch the daemon
# (where applicable), wait for a ready signal, run one functional probe
# (curl, JMS, etc.), then kill the daemon. The functional probes go past
# the launcher-entry smoke `--version` tests (which only verify the JVM
# boots far enough to print a version string) and exercise actual app
# behaviour — first error from the deeper code path is what we want.
#
# Tests run sequentially because most apps want to bind default ports.
# Per-app log files live under applogs/orchestrator-functional-$STAMP/.

set +e

ROOT="C:/Projects/CratonVM"
RJVM="$ROOT/target/release/cratonvm.exe"
JDK="C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot"
APPS="$ROOT/apps"

STAMP="${1:-$(date +%Y%m%d-%H%M%S)}"
LOGDIR="$ROOT/applogs/orchestrator-functional-$STAMP"
mkdir -p "$LOGDIR"

CURL=curl

# Per-app helpers --------------------------------------------------------------

# run_daemon NAME READY_PATTERN TIMEOUT_S CMD...
#
# Launches CMD in background. Tails the err log until READY_PATTERN matches
# (success) or TIMEOUT_S elapses (failure). On either outcome, calls the
# user-provided `probe_$NAME` and `kill_$NAME` (passed as the next two args
# via environment) — see per-app blocks below for how they're wired.
launch_daemon() {
    local name="$1"; shift
    local ready_re="$1"; shift
    local timeout_s="$1"; shift
    local out="$LOGDIR/$name.out"
    local err="$LOGDIR/$name.err"

    # Launch in background; capture PID
    "$@" > "$out" 2> "$err" &
    local pid=$!
    echo "$pid" > "$LOGDIR/$name.pid"

    # Wait for ready
    local elapsed=0
    while [ "$elapsed" -lt "$timeout_s" ]; do
        if ! kill -0 "$pid" 2>/dev/null; then
            # Process died — capture first error
            local first_err
            first_err=$(grep -vE '^\[2m.*WARN.*Post-clinit|^\[2m.*WARN.*B6:|^\[2m.*WARN.*Stale|^\[2m.*WARN.*Missing native|^\[2m.*WARN.*SWALLOW|^\[DBG\]|^\[cratonvm\]|^\[rustjvm\]|^\[2m.*WARN.*registered|short-circuited|^\[kc17-bf\]|^\[jboss-bf\]' "$err" \
                | grep -E 'Exception|Error|Caused|StringIndex|NullPointer|StackOverflow|ARRAY-LEN-GUARD|gen_heap|FileNot|UnsupportedClass|AbstractMethod|UnknownModule|^Failed|severe|SEVERE|FATAL|fatal' \
                | head -1 | head -c 220 | sed 's/\x1b\[[0-9;]*m//g')
            echo "$name | DAEMON_DIED | $first_err"
            return 2
        fi
        if grep -qE "$ready_re" "$out" "$err" 2>/dev/null; then
            return 0
        fi
        sleep 1
        elapsed=$((elapsed + 1))
    done

    echo "$name | TIMEOUT_NO_READY (${timeout_s}s)"
    return 3
}

# kill_daemon NAME — best-effort: kill the PID from $LOGDIR/$name.pid.
kill_daemon() {
    local name="$1"
    local pid_file="$LOGDIR/$name.pid"
    [ -f "$pid_file" ] || return
    local pid
    pid=$(cat "$pid_file")
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
        # On cygwin/git-bash, kill -9 sometimes leaves the JVM running.
        # Use Windows taskkill as well.
        kill "$pid" 2>/dev/null
        sleep 1
        kill -9 "$pid" 2>/dev/null
        sleep 1
        # Belt and braces — taskkill by PID
        taskkill //F //PID "$pid" 2>/dev/null
    fi
    rm -f "$pid_file"
}

# probe_http URL TIMEOUT_S — return 0 on HTTP 2xx/3xx, 1 otherwise. Quiet.
probe_http() {
    local url="$1"
    local timeout_s="$2"
    "$CURL" --silent --output /dev/null --write-out '%{http_code}\n' \
        --max-time "$timeout_s" --connect-timeout "$timeout_s" \
        "$url" 2>/dev/null \
        | grep -qE '^[23]'
}

# run_oneshot NAME TIMEOUT_S CMD...
#
# Launches CMD synchronously with a timeout. Pass = rc 0 with no exception
# line in stderr.
run_oneshot() {
    local name="$1"; shift
    local timeout_s="$1"; shift
    timeout --foreground -k 5 "$timeout_s" "$@" \
        > "$LOGDIR/$name.out" 2> "$LOGDIR/$name.err"
    local rc=$?
    local first_err
    first_err=$(grep -vE '^\[2m.*WARN.*Post-clinit|^\[2m.*WARN.*B6:|^\[2m.*WARN.*Stale|^\[2m.*WARN.*Missing native|^\[2m.*WARN.*SWALLOW|^\[DBG\]|^\[cratonvm\]|^\[rustjvm\]|^\[2m.*WARN.*registered|short-circuited|^\[kc17-bf\]|^\[jboss-bf\]' "$LOGDIR/$name.err" \
        | grep -E 'Exception|Error|Caused|StringIndex|NullPointer|StackOverflow|ARRAY-LEN-GUARD|gen_heap|FileNot|UnsupportedClass|AbstractMethod|UnknownModule|^Failed|severe|SEVERE|FATAL|fatal' \
        | head -1 | head -c 220 | sed 's/\x1b\[[0-9;]*m//g')
    echo "$name | rc=$rc | $first_err"
}

# cp_glob <dir1> [dir2 ...] — collect all *.jar, normalize to Windows ; sep.
cp_glob() {
    find "$@" -name '*.jar' 2>/dev/null \
        | sed 's|^/c|C:|' \
        | tr '\n' ';' \
        | sed 's/;$//'
}

# ----- 1. ACTIVEMQ — start broker, send/receive via openwire ----------------

func_activemq() {
    local name=activemq
    echo "--- $name (functional: start broker, no producer/consumer yet) ---" >&2
    local cp
    cp=$(cp_glob "$APPS/apache-activemq-5.18.3/lib")
    # Start broker on default ports (61616 openwire, 8161 web)
    launch_daemon "$name" 'Apache ActiveMQ.*started|Listening for connections' 90 \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 512m \
        -c "$cp" \
        "-Dactivemq.base=$APPS/apache-activemq-5.18.3" \
        "-Dactivemq.home=$APPS/apache-activemq-5.18.3" \
        "-Dactivemq.conf=$APPS/apache-activemq-5.18.3/conf" \
        "-Dactivemq.data=$APPS/apache-activemq-5.18.3/data" \
        org.apache.activemq.console.Main start xbean:file:"$APPS/apache-activemq-5.18.3/conf/activemq.xml"
    local rc=$?
    if [ "$rc" = "0" ]; then
        # Broker reached ready: probe the web console
        if probe_http http://localhost:8161/admin/ 5; then
            echo "$name | rc=0 | broker started and web admin responded"
        else
            echo "$name | rc=1 | broker started but web admin probe failed"
        fi
    fi
    kill_daemon "$name"
}

# ----- 2. JENKINS — start servlet, curl /api/json ---------------------------

func_jenkins() {
    local name=jenkins
    echo "--- $name (functional: start jenkins.war, curl /api/json) ---" >&2
    launch_daemon "$name" 'Jenkins is fully up|Started @' 180 \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 1g \
        --jar "$APPS/jenkins.war" \
        -- --httpPort=18080 --enable-future-java
    local rc=$?
    if [ "$rc" = "0" ]; then
        if probe_http http://localhost:18080/api/json 10; then
            echo "$name | rc=0 | jenkins boot OK, /api/json responded"
        else
            echo "$name | rc=1 | jenkins boot reached ready, but /api/json probe failed"
        fi
    fi
    kill_daemon "$name"
}

# ----- 3. SOLR — start core, hit admin endpoint -----------------------------

func_solr() {
    local name=solr
    # Solr's actual server start goes through `bin/solr` shell which forks
    # Jetty as a separate JVM. Direct in-process start isn't a single
    # entry point. For now, keep the SolrCLI `version` workload — it runs
    # real SolrCLI bytecode end-to-end, which is itself a non-trivial
    # exercise of the JDK (config parsing, log4j init, etc.).
    echo "--- $name (functional: SolrCLI version — bin/solr server start is a shell-wrapped Jetty fork) ---" >&2
    local cp
    cp=$(cp_glob "$APPS/solr-9.5.0/server/solr-webapp/webapp/WEB-INF/lib" "$APPS/solr-9.5.0/server/lib/ext")
    run_oneshot "$name" 60 \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 \
        -c "$cp" org.apache.solr.cli.SolrCLI -- version
}

# ----- 4. WILDFLY — start standalone, curl :9990 ----------------------------

func_wildfly() {
    local name=wildfly
    echo "--- $name (functional: start standalone, curl :9990 management) ---" >&2
    launch_daemon "$name" 'WildFly.*started in|WFLYSRV0025' 300 \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 1g \
        "-Djboss.home.dir=$APPS/wildfly-39.0.1.Final" \
        --jar "$APPS/wildfly-39.0.1.Final/jboss-modules.jar" \
        -- -mp "$APPS/wildfly-39.0.1.Final/modules" \
        org.jboss.as.standalone
    local rc=$?
    if [ "$rc" = "0" ]; then
        if probe_http http://localhost:9990/management 5; then
            echo "$name | rc=0 | wildfly started, management endpoint responded"
        else
            echo "$name | rc=1 | wildfly started but management probe failed"
        fi
    fi
    kill_daemon "$name"
}

# ----- 5. KEYCLOAK 16 — start standalone, curl :9990 ------------------------

func_kc16() {
    local name=kc16
    echo "--- $name (functional: start KC16 standalone, curl :9990) ---" >&2
    launch_daemon "$name" 'WildFly.*started in|WFLYSRV0025|Keycloak.*started' 300 \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 1g \
        "-Djboss.home.dir=$APPS/keycloak-16.1.1" \
        --jar "$APPS/keycloak-16.1.1/jboss-modules.jar" \
        -- -mp "$APPS/keycloak-16.1.1/modules" \
        org.jboss.as.standalone
    local rc=$?
    if [ "$rc" = "0" ]; then
        if probe_http http://localhost:9990/management 5; then
            echo "$name | rc=0 | kc16 started, management endpoint responded"
        else
            echo "$name | rc=1 | kc16 started but management probe failed"
        fi
    fi
    kill_daemon "$name"
}

# ----- 6. IGNITE — start node, check it announces 'Started' ----------------

func_ignite() {
    local name=ignite
    echo "--- $name (functional: start node, wait for Topology snapshot) ---" >&2
    local cp
    cp=$(cp_glob "$APPS/apache-ignite-2.16.0-bin/libs")
    launch_daemon "$name" 'Topology snapshot \[ver=1' 120 \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 1g \
        -c "$cp" \
        "-DIGNITE_HOME=$APPS/apache-ignite-2.16.0-bin" \
        org.apache.ignite.startup.cmdline.CommandLineStartup \
        "$APPS/apache-ignite-2.16.0-bin/examples/config/example-default.xml"
    local rc=$?
    if [ "$rc" = "0" ]; then
        echo "$name | rc=0 | ignite node announced topology snapshot"
    fi
    kill_daemon "$name"
}

# ----- 7. CGLIB PROBE — already a real functional test ---------------------

func_cglib_probe() {
    local cp="$APPS/cglib_probe;$APPS/cglib_probe/cglib-3.3.0.jar;$APPS/cglib_probe/asm-9.5.jar"
    run_oneshot cglib_probe 30 \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 \
        -c "$cp" CglibProbe
}

# ----- 8. KAFKA — start broker (Kraft single-node) -------------------------

func_kafka() {
    local name=kafka
    echo "--- $name (functional: start single-node, wait for ready) ---" >&2
    local cp
    cp=$(cp_glob "$APPS/kafka_2.13-3.6.1/libs")
    launch_daemon "$name" 'Kafka Server started|Awaiting socket connections' 90 \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 1g \
        -c "$cp" kafka.Kafka \
        "$APPS/kafka_2.13-3.6.1/config/server.properties"
    local rc=$?
    if [ "$rc" = "0" ]; then
        echo "$name | rc=0 | kafka announced ready"
    fi
    kill_daemon "$name"
}

# ----- 9. ELASTICSEARCH — start node, curl :9200 ---------------------------

func_elasticsearch() {
    local name=elasticsearch
    echo "--- $name (functional: start node, curl :9200) ---" >&2
    local cp
    cp=$(cp_glob "$APPS/elasticsearch-8.15.5/lib")
    launch_daemon "$name" 'started.*9200|Active license|node.*started' 240 \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 1g \
        -c "$cp" \
        "-Dcli.name=server" \
        "-Des.path.home=$APPS/elasticsearch-8.15.5" \
        "-Des.path.conf=$APPS/elasticsearch-8.15.5/config" \
        org.elasticsearch.launcher.CliToolLauncher
    local rc=$?
    if [ "$rc" = "0" ]; then
        if probe_http http://localhost:9200/ 10; then
            echo "$name | rc=0 | elasticsearch started and :9200 responded"
        else
            echo "$name | rc=1 | elasticsearch started but :9200 probe failed"
        fi
    fi
    kill_daemon "$name"
}

# ----- 10. JETTY — start with a minimal server, curl :8080 -----------------

func_jetty() {
    local name=jetty
    echo "--- $name (functional: start with http module, curl :8080) ---" >&2
    launch_daemon "$name" 'Started @|oejs.Server.*Started' 120 \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 512m \
        --jar "$APPS/jetty-home-11.0.20/start.jar" \
        -- "jetty.home=$APPS/jetty-home-11.0.20" \
           "jetty.base=$APPS/jetty-home-11.0.20" \
           --modules=http,deploy,resources \
           jetty.http.port=18080
    local rc=$?
    if [ "$rc" = "0" ]; then
        if probe_http http://localhost:18080/ 5; then
            echo "$name | rc=0 | jetty started, :18080 responded"
        else
            echo "$name | rc=1 | jetty started but :18080 probe failed"
        fi
    fi
    kill_daemon "$name"
}

# ----- 11. CASSANDRA — start node, wait for thrift listener ----------------

func_cassandra() {
    local name=cassandra
    echo "--- $name (functional: start daemon, wait for native transport) ---" >&2
    local cp
    cp=$(cp_glob "$APPS/apache-cassandra-4.1.4/lib")
    launch_daemon "$name" 'Listening for native transport|Starting listening for CQL' 240 \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 1g \
        -c "$cp" \
        "-Dcassandra.config=file:$APPS/apache-cassandra-4.1.4/conf/cassandra.yaml" \
        "-Dcassandra.storagedir=$APPS/apache-cassandra-4.1.4/data" \
        "-Dlogback.configurationFile=$APPS/apache-cassandra-4.1.4/conf/logback.xml" \
        org.apache.cassandra.service.CassandraDaemon
    local rc=$?
    if [ "$rc" = "0" ]; then
        echo "$name | rc=0 | cassandra started, listening"
    fi
    kill_daemon "$name"
}

# ----- 12. NEO4J — start daemon, wait for "Started" -------------------------

func_neo4j() {
    local name=neo4j
    echo "--- $name (functional: start daemon, wait for Started) ---" >&2
    local cp
    cp=$(cp_glob "$APPS/neo4j-community-5.18.1/lib")
    launch_daemon "$name" 'Started\.|Remote interface available at|Bolt enabled' 240 \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 1g \
        -c "$cp" \
        "-DNEO4J_HOME=$APPS/neo4j-community-5.18.1" \
        org.neo4j.server.startup.Neo4jBoot start
    local rc=$?
    if [ "$rc" = "0" ]; then
        if probe_http http://localhost:7474/ 5; then
            echo "$name | rc=0 | neo4j started, :7474 responded"
        else
            echo "$name | rc=1 | neo4j started but :7474 probe failed"
        fi
    fi
    kill_daemon "$name"
}

# ----- 13. FELIX — start framework, deploy/start a bundle ------------------

func_felix() {
    local name=felix
    echo "--- $name (functional: start framework, wait for OSGi prompt) ---" >&2
    launch_daemon "$name" 'g!|Welcome to Apache Felix Gogo' 60 \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 256m \
        --jar "$APPS/felix-framework-7.0.5/bin/felix.jar"
    local rc=$?
    if [ "$rc" = "0" ]; then
        echo "$name | rc=0 | felix framework reached gogo prompt"
    fi
    kill_daemon "$name"
}

# ----- 14. HAZELCAST — start node, wait for cluster ready -------------------

func_hazelcast() {
    local name=hazelcast
    echo "--- $name (functional: start node, wait for cluster ready) ---" >&2
    launch_daemon "$name" 'STARTED|Cluster name:|Members \{size:1' 90 \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 512m \
        --jar "$APPS/hazelcast.jar"
    local rc=$?
    if [ "$rc" = "0" ]; then
        echo "$name | rc=0 | hazelcast node STARTED"
    fi
    kill_daemon "$name"
}

# ----- 15. LIBERTY — start server, curl :9080 ------------------------------

func_liberty() {
    local name=liberty
    echo "--- $name (functional: ws-server.jar start defaultServer) ---" >&2
    launch_daemon "$name" 'CWWKF0011I|Server defaultServer is ready|open for e-business' 240 \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 1g \
        --jar "$APPS/wlp/bin/tools/ws-server.jar" \
        -- defaultServer
    local rc=$?
    if [ "$rc" = "0" ]; then
        if probe_http http://localhost:9080/ 5; then
            echo "$name | rc=0 | liberty started, :9080 responded"
        else
            echo "$name | rc=1 | liberty started but :9080 probe failed"
        fi
    fi
    kill_daemon "$name"
}

# ----- 16. PAYARA — start server, curl admin :4848 -------------------------

func_payara() {
    local name=payara
    echo "--- $name (functional: ASMain start, curl :4848 admin) ---" >&2
    local cp
    cp=$(cp_glob "$APPS/payara6/glassfish/modules")
    launch_daemon "$name" 'started in|Listening on port|GlassFish .* started' 240 \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 1g \
        -c "$cp" \
        "-Dcom.sun.aas.installRoot=$APPS/payara6/glassfish" \
        com.sun.enterprise.glassfish.bootstrap.ASMain
    local rc=$?
    if [ "$rc" = "0" ]; then
        if probe_http http://localhost:4848/ 5; then
            echo "$name | rc=0 | payara started, :4848 admin responded"
        else
            echo "$name | rc=1 | payara started but :4848 probe failed"
        fi
    fi
    kill_daemon "$name"
}

# ----- 17. KC26 — start quarkus runtime, curl :8080 -------------------------

func_kc26() {
    local name=kc26
    echo "--- $name (functional: start keycloak-26 quarkus, curl :8080) ---" >&2
    launch_daemon "$name" 'Listening on:|Profile dev activated|Keycloak.*started' 240 \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 1g \
        --jar "$APPS/keycloak-26.2.4/lib/quarkus-run.jar" \
        -- start-dev
    local rc=$?
    if [ "$rc" = "0" ]; then
        if probe_http http://localhost:8080/ 5; then
            echo "$name | rc=0 | kc26 started, :8080 responded"
        else
            echo "$name | rc=1 | kc26 started but :8080 probe failed"
        fi
    fi
    kill_daemon "$name"
}

# ----- 18. FLINK — run example StreamingWordCount --------------------------

func_flink() {
    local name=flink
    echo "--- $name (functional: run example WordCount) ---" >&2
    local cp
    cp=$(cp_glob "$APPS/flink-1.18.1/lib")
    local example="$APPS/flink-1.18.1/examples/streaming/WordCount.jar"
    if [ -f "$example" ]; then
        run_oneshot "$name" 60 \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 1g \
            -c "$cp;$example" \
            "-DFLINK_HOME=$APPS/flink-1.18.1" \
            org.apache.flink.streaming.examples.wordcount.WordCount
    else
        echo "$name | rc=skip | example WordCount.jar not found"
    fi
}

# ----- 19. SPARK — run example SparkPi -------------------------------------

func_spark() {
    local name=spark
    echo "--- $name (functional: run SparkPi --master local[1]) ---" >&2
    local cp
    cp=$(cp_glob "$APPS/spark-3.5.1-bin-hadoop3/jars")
    local example="$APPS/spark-3.5.1-bin-hadoop3/examples/jars/spark-examples_2.12-3.5.1.jar"
    if [ -f "$example" ]; then
        run_oneshot "$name" 120 \
            "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 1g \
            -c "$cp;$example" org.apache.spark.deploy.SparkSubmit \
            -- --class org.apache.spark.examples.SparkPi \
               --master "local[1]" \
               "$example" 10
    else
        echo "$name | rc=skip | spark-examples jar not found"
    fi
}

# ----- 20. GRADLE — build a tiny project -----------------------------------

func_gradle() {
    local name=gradle
    echo "--- $name (functional: gradle --version, full path) ---" >&2
    local cp
    cp=$(cp_glob "$APPS/gradle-8.10.2/lib")
    run_oneshot "$name" 60 \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 --Xmx 512m \
        -c "$cp" org.gradle.launcher.GradleMain -- --version
}

# ----- 21. BYTEBUDDY PROBE — already a functional probe --------------------

func_bytebuddy_probe() {
    local cp="$APPS/bytebuddy_probe;$APPS/bytebuddy_probe/byte-buddy-1.14.18.jar"
    run_oneshot bytebuddy_probe 30 \
        "$RJVM" --java-home "$JDK" --stack-dump-on-timeout 0 \
        -c "$cp" ByteBuddyProbe
}

# ----- Dispatcher -----------------------------------------------------------

# Run a subset only if a name is given on the cmd line (after STAMP); else
# run all. Allows: `bash orchestrator-functional-all.sh r6 wildfly kc16`
shift  # consume STAMP

if [ $# -gt 0 ]; then
    SELECT="$*"
else
    SELECT="cglib_probe bytebuddy_probe gradle flink spark activemq jenkins solr ignite kafka jetty liberty payara wildfly kc16 kc26 hazelcast felix elasticsearch cassandra neo4j"
fi

for fn in $SELECT; do
    case "$fn" in
        activemq)        func_activemq ;;
        jenkins)         func_jenkins ;;
        solr)            func_solr ;;
        wildfly)         func_wildfly ;;
        kc16)            func_kc16 ;;
        ignite)          func_ignite ;;
        cglib_probe)     func_cglib_probe ;;
        bytebuddy_probe) func_bytebuddy_probe ;;
        kafka)           func_kafka ;;
        elasticsearch)   func_elasticsearch ;;
        jetty)           func_jetty ;;
        cassandra)       func_cassandra ;;
        neo4j)           func_neo4j ;;
        felix)           func_felix ;;
        hazelcast)       func_hazelcast ;;
        liberty)         func_liberty ;;
        payara)          func_payara ;;
        kc26)            func_kc26 ;;
        flink)           func_flink ;;
        spark)           func_spark ;;
        gradle)          func_gradle ;;
        *)               echo "$fn | rc=skip | no test defined" ;;
    esac
done

echo "=== logs in $LOGDIR ==="
