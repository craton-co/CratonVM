#!/usr/bin/env bash
# RI.12 — Kotlin hello-world (kotlinc-compiled .class).

source "$(dirname "$0")/common.sh"

V="${KOTLIN_VERSION:-1.9.24}"
DIST_ZIP="$FIXTURE_CACHE/kotlin-compiler-$V.zip"
DIST_DIR="$FIXTURE_CACHE/kotlinc"
URL="https://github.com/JetBrains/kotlin/releases/download/v$V/kotlin-compiler-$V.zip"

if [[ ! -d "$DIST_DIR/bin" ]]; then
    smoke_download "$URL" "$DIST_ZIP"
    unzip -q -d "$FIXTURE_CACHE" "$DIST_ZIP"
fi
export PATH="$DIST_DIR/bin:$PATH"

FIX_DIR="$FIXTURE_CACHE/fixture"
mkdir -p "$FIX_DIR"
cat > "$FIX_DIR/Hello.kt" <<'KOTLIN'
fun main() { println("KOTLIN_SMOKE_OK") }
KOTLIN

JAVA_HOME="$JAVA_HOME_FOR_SMOKE" kotlinc "$FIX_DIR/Hello.kt" -d "$FIX_DIR/hello.jar"

SMOKE_TIMEOUT=120 smoke_run_cratonvm \
    --classpath "$FIX_DIR/hello.jar:$DIST_DIR/lib/kotlin-stdlib.jar" \
    -- HelloKt

smoke_require_signal "KOTLIN_SMOKE_OK"
