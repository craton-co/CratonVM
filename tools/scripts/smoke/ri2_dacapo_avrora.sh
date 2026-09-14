#!/usr/bin/env bash
# RI.2 — DaCapo avrora benchmark runs.
# DaCapo 9.12-MR1 is redistributable under the ASL-2.

source "$(dirname "$0")/common.sh"

JAR="$FIXTURE_CACHE/dacapo-9.12-MR1-bach.jar"
URL="https://downloads.sourceforge.net/project/dacapobench/9.12-bach-MR1/dacapo-9.12-MR1-bach.jar"

smoke_download "$URL" "$JAR"

SMOKE_TIMEOUT=600 smoke_run_cratonvm --Xmx 1500m --jar "$JAR" -- avrora -s small -n 1

# DaCapo prints "===== DaCapo 9.12-MR1 avrora PASSED in <N> msec ====="
smoke_require_signal "PASSED in [0-9]+ msec"
