#!/usr/bin/env bash
# RI.5 — Jackson databind: round-trip a JSON document.

source "$(dirname "$0")/common.sh"

MVN_CENTRAL="https://repo1.maven.org/maven2"
V="${JACKSON_VERSION:-2.17.1}"
CORE_JAR="$FIXTURE_CACHE/jackson-core-$V.jar"
DATABIND_JAR="$FIXTURE_CACHE/jackson-databind-$V.jar"
ANN_JAR="$FIXTURE_CACHE/jackson-annotations-$V.jar"
FIX_DIR="$FIXTURE_CACHE/fixture"
mkdir -p "$FIX_DIR"

smoke_download "$MVN_CENTRAL/com/fasterxml/jackson/core/jackson-core/$V/jackson-core-$V.jar" "$CORE_JAR"
smoke_download "$MVN_CENTRAL/com/fasterxml/jackson/core/jackson-databind/$V/jackson-databind-$V.jar" "$DATABIND_JAR"
smoke_download "$MVN_CENTRAL/com/fasterxml/jackson/core/jackson-annotations/$V/jackson-annotations-$V.jar" "$ANN_JAR"

cat > "$FIX_DIR/JacksonSmoke.java" <<'JAVA'
import com.fasterxml.jackson.databind.ObjectMapper;
import java.util.Map;
public class JacksonSmoke {
    public static void main(String[] args) throws Exception {
        ObjectMapper m = new ObjectMapper();
        String json = "{\"a\":1,\"b\":\"two\"}";
        Map<?,?> parsed = m.readValue(json, Map.class);
        String roundtrip = m.writeValueAsString(parsed);
        if (!roundtrip.contains("\"a\":1")) {
            throw new RuntimeException("bad roundtrip: " + roundtrip);
        }
        System.out.println("JACKSON_SMOKE_OK " + roundtrip);
    }
}
JAVA

"$JAVA_HOME_FOR_SMOKE/bin/javac" -cp "$CORE_JAR:$DATABIND_JAR:$ANN_JAR" -d "$FIX_DIR" "$FIX_DIR/JacksonSmoke.java"

SMOKE_TIMEOUT=300 smoke_run_cratonvm \
    --Xmx 512m \
    --classpath "$FIX_DIR:$CORE_JAR:$DATABIND_JAR:$ANN_JAR" \
    -- JacksonSmoke

smoke_require_signal "JACKSON_SMOKE_OK"
