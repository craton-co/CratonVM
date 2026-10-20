#!/usr/bin/env bash
# RI.3 — DaCapo jython benchmark runs.
source "$(dirname "$0")/common.sh"

JAR="$FIXTURE_CACHE/dacapo-9.12-MR1-bach.jar"
URL="https://downloads.sourceforge.net/project/dacapobench/9.12-bach-MR1/dacapo-9.12-MR1-bach.jar"

smoke_download "$URL" "$JAR"

SMOKE_TIMEOUT=900 smoke_run_cratonvm --Xmx 2g --jar "$JAR" -- jython -s small -n 1

smoke_require_signal "PASSED in [0-9]+ msec"
