#!/bin/bash
# A2 ReflRepro experiment battery
BIN=./cvmregroots.exe
JH="C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot"
CP="docs/known-issues/repros/A2-reflrepro"
run() {
  local label="$1"; shift
  echo "=== $label ==="
  "$@" "$BIN" --java-home "$JH" -cp "$CP" ReflRepro 8000 2>&1 | grep -E "done|firstBad|ref=" | head -4
  echo "rc=${PIPESTATUS[0]}"
  echo ""
}
