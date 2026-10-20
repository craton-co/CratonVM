#!/usr/bin/env bash
# RI.13 — Scala 3 hello-world.

source "$(dirname "$0")/common.sh"

V="${SCALA_VERSION:-3.4.2}"
DIST_DIR="$FIXTURE_CACHE/scala-$V"
URL="https://github.com/lampepfl/dotty/releases/download/$V/scala3-$V.tar.gz"

if [[ ! -d "$DIST_DIR/bin" ]]; then
    TGZ="$FIXTURE_CACHE/scala3-$V.tar.gz"
    smoke_download "$URL" "$TGZ"
    tar -C "$FIXTURE_CACHE" -xzf "$TGZ"
    [[ -d "$FIXTURE_CACHE/scala3-$V" ]] && mv "$FIXTURE_CACHE/scala3-$V" "$DIST_DIR"
fi
export PATH="$DIST_DIR/bin:$PATH"

FIX_DIR="$FIXTURE_CACHE/fixture"
mkdir -p "$FIX_DIR"
cat > "$FIX_DIR/Hello.scala" <<'SCALA'
@main def hello() = println("SCALA_SMOKE_OK")
SCALA

JAVA_HOME="$JAVA_HOME_FOR_SMOKE" scalac -d "$FIX_DIR/out" "$FIX_DIR/Hello.scala"

STDLIB=$(ls "$DIST_DIR"/lib/scala3-library_3-*.jar | head -1)
SCALA_LIB=$(ls "$DIST_DIR"/lib/scala-library-*.jar | head -1)

SMOKE_TIMEOUT=120 smoke_run_cratonvm \
    --classpath "$FIX_DIR/out:$STDLIB:$SCALA_LIB" \
    -- hello

smoke_require_signal "SCALA_SMOKE_OK"
