#!/usr/bin/env bash
# Run the RSK / JC probes on HotSpot and on a CratonVM binary and print both.
#
#   ./run.sh <path-to-cratonvm.exe> [extra cratonvm args...]
#
# HotSpot is the oracle. Every line of both probes reads OK there.
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
JH="${JAVA_HOME_25:-/c/Program Files/Microsoft/jdk-25.0.3.9-hotspot}"
BIN="${1:?usage: run.sh <cratonvm.exe> [args...]}"
shift || true

mkdir -p "$HERE/out" "$HERE/jcout"
"$JH/bin/javac" -d "$HERE/out" "$HERE/RSK.java" "$HERE/JC.java" || exit 1

echo "=== HotSpot RSK ==="
"$JH/bin/java" -cp "$HERE/out" RSK
echo "=== HotSpot JC ==="
"$JH/bin/java" -Djc.out="$HERE/jcout" -cp "$HERE/out" JC

echo "=== CratonVM RSK ==="
"$BIN" --java-home "$JH" "$@" -cp "$HERE/out" RSK
echo "=== CratonVM JC ==="
"$BIN" --java-home "$JH" \
  --add-opens=java.base/java.lang=ALL-UNNAMED \
  --add-opens=java.base/java.util=ALL-UNNAMED \
  "$@" -Djc.out="$HERE/jcout" -cp "$HERE/out" JC
