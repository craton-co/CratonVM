#!/usr/bin/env bash
# RI.9 — Spring Boot 3.2 "Hello World" starter app.
#
# We lean on start.spring.io as the canonical generator; if it's
# unreachable the job exits 78 (EX_CONFIG, "skipped"). The generated
# app is built once per CI cache-hit and reused across runs.

source "$(dirname "$0")/common.sh"

FIX_DIR="$FIXTURE_CACHE/fixture"
APP_DIR="$FIX_DIR/springsmoke"
JAR="$FIX_DIR/springsmoke.jar"
mkdir -p "$FIX_DIR"

if [[ ! -s "$JAR" ]]; then
    ZIP="$FIX_DIR/springsmoke.zip"
    if ! curl --silent --fail --location \
            "https://start.spring.io/starter.zip?type=maven-project&language=java&bootVersion=3.2.6&javaVersion=21&packageName=com.example.smoke&name=SpringSmoke&dependencies=web&baseDir=springsmoke" \
            -o "$ZIP"; then
        echo "RI.9: start.spring.io unreachable, skipping" >&2
        exit 78
    fi
    unzip -q "$ZIP" -d "$FIX_DIR"
    ( cd "$APP_DIR" && ./mvnw -q package -DskipTests )
    cp "$APP_DIR/target/"SpringSmoke-*.jar "$JAR"
fi

SMOKE_TIMEOUT=300 smoke_run_cratonvm --Xmx 1500m --jar "$JAR" -- --server.port=0 &
BOOT_PID=$!
for _ in $(seq 1 30); do
    if grep -q "Started SpringSmokeApplication" "$SMOKE_LOG" 2>/dev/null; then
        kill "$BOOT_PID" 2>/dev/null || true
        smoke_require_signal "Started SpringSmokeApplication"
        exit 0
    fi
    sleep 2
done
kill "$BOOT_PID" 2>/dev/null || true
echo "RI.9: FAIL — 'Started SpringSmokeApplication' never observed" >&2
tail -n 30 "$SMOKE_LOG" >&2
exit 1
