#!/usr/bin/env bash
# RI.6 — SLF4J + Logback: log a message to a file.

source "$(dirname "$0")/common.sh"

MVN_CENTRAL="https://repo1.maven.org/maven2"
SLF4J_V="${SLF4J_VERSION:-2.0.13}"
LB_V="${LOGBACK_VERSION:-1.5.6}"
SLF4J_JAR="$FIXTURE_CACHE/slf4j-api-$SLF4J_V.jar"
LB_CORE="$FIXTURE_CACHE/logback-core-$LB_V.jar"
LB_CLASSIC="$FIXTURE_CACHE/logback-classic-$LB_V.jar"
FIX_DIR="$FIXTURE_CACHE/fixture"
OUT_LOG="$FIX_DIR/smoke.log"
mkdir -p "$FIX_DIR"
rm -f "$OUT_LOG"

smoke_download "$MVN_CENTRAL/org/slf4j/slf4j-api/$SLF4J_V/slf4j-api-$SLF4J_V.jar" "$SLF4J_JAR"
smoke_download "$MVN_CENTRAL/ch/qos/logback/logback-core/$LB_V/logback-core-$LB_V.jar" "$LB_CORE"
smoke_download "$MVN_CENTRAL/ch/qos/logback/logback-classic/$LB_V/logback-classic-$LB_V.jar" "$LB_CLASSIC"

cat > "$FIX_DIR/logback.xml" <<XML
<configuration>
  <appender name="F" class="ch.qos.logback.core.FileAppender">
    <file>$OUT_LOG</file>
    <encoder><pattern>%msg%n</pattern></encoder>
  </appender>
  <root level="info"><appender-ref ref="F"/></root>
</configuration>
XML

cat > "$FIX_DIR/LogSmoke.java" <<'JAVA'
import org.slf4j.*;
public class LogSmoke {
    public static void main(String[] args) {
        LoggerFactory.getLogger("smoke").info("LOGBACK_SMOKE_OK");
    }
}
JAVA

"$JAVA_HOME_FOR_SMOKE/bin/javac" -cp "$SLF4J_JAR" -d "$FIX_DIR" "$FIX_DIR/LogSmoke.java"

SMOKE_TIMEOUT=180 smoke_run_cratonvm \
    --Xmx 512m \
    --classpath "$FIX_DIR:$SLF4J_JAR:$LB_CORE:$LB_CLASSIC" \
    -- LogSmoke

if [[ ! -s "$OUT_LOG" ]] || ! grep -q LOGBACK_SMOKE_OK "$OUT_LOG"; then
    echo "RI.6: FAIL — smoke.log missing or does not contain marker" >&2
    exit 1
fi
echo "RI.6: PASS — Logback wrote LOGBACK_SMOKE_OK to $OUT_LOG"
