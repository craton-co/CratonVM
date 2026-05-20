#!/usr/bin/env bash
# RI.15 — Quarkus 3 fast-jar (hot-path of KC26).

source "$(dirname "$0")/common.sh"

FIX_DIR="$FIXTURE_CACHE/fixture"
APP_DIR="$FIX_DIR/quarkus-smoke"
mkdir -p "$FIX_DIR"

if [[ ! -d "$APP_DIR" ]]; then
    ZIP="$FIX_DIR/q.zip"
    if ! curl --silent --fail --location \
            "https://code.quarkus.io/d?g=com.example&a=quarkus-smoke&v=1.0.0&e=rest" \
            -o "$ZIP"; then
        echo "RI.15: code.quarkus.io unreachable, skipping" >&2
        exit 78
    fi
    unzip -q "$ZIP" -d "$FIX_DIR"
    ( cd "$APP_DIR" && ./mvnw -q package -Dquarkus.package.jar.type=fast-jar -DskipTests )
fi

SMOKE_TIMEOUT=300 smoke_run_cratonvm \
    --Xmx 1g \
    --jar "$APP_DIR/target/quarkus-app/quarkus-run.jar" -- &
BOOT_PID=$!
for _ in $(seq 1 30); do
    if grep -q "started in" "$SMOKE_LOG" 2>/dev/null; then
        kill "$BOOT_PID" 2>/dev/null || true
        smoke_require_signal "started in"
        exit 0
    fi
    sleep 2
done
kill "$BOOT_PID" 2>/dev/null || true
echo "RI.15: FAIL — Quarkus never printed 'started in'" >&2
tail -n 30 "$SMOKE_LOG" >&2
exit 1
