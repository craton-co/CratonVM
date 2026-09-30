#!/usr/bin/env bash
# RI.14 — Groovy 4 script.

source "$(dirname "$0")/common.sh"

MVN_CENTRAL="https://repo1.maven.org/maven2"
V="${GROOVY_VERSION:-4.0.21}"
GROOVY_JAR="$FIXTURE_CACHE/groovy-$V.jar"
smoke_download "$MVN_CENTRAL/org/apache/groovy/groovy/$V/groovy-$V.jar" "$GROOVY_JAR"

FIX_DIR="$FIXTURE_CACHE/fixture"
mkdir -p "$FIX_DIR"
cat > "$FIX_DIR/hello.groovy" <<'GROOVY'
println "GROOVY_SMOKE_OK"
GROOVY

SMOKE_TIMEOUT=120 smoke_run_cratonvm \
    --classpath "$GROOVY_JAR" \
    -- groovy.ui.GroovyMain "$FIX_DIR/hello.groovy"

smoke_require_signal "GROOVY_SMOKE_OK"
