#!/usr/bin/env bash
# hibfix-mtins-run.sh <tag> <binary|HOTSPOT> <argfile> [extra vm args...] -- <class> <method>
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
HERE="C:/craton/CratonVM/apps/hibernate-reactive-suite-runner"
TAG="$1"; BIN="$2"; AF="$3"; shift 3
EXTRA=(); while [ "${1:-}" != "--" ]; do EXTRA+=("$1"); shift; done; shift
mkdir -p "$HERE/runs/mtins"
OUT="$HERE/runs/mtins/$TAG.log"
JDK="$(ls -d "C:/Program Files/Eclipse Adoptium"/jdk-2* 2>/dev/null | head -1)"
start=$(date +%s%3N)
if [ "$BIN" = "HOTSPOT" ]; then
  "$JDK/bin/java.exe" "@$HERE/$AF" "${EXTRA[@]}" -Dcraton.batch=1 MethodRunner "$@" >"$OUT" 2>&1
else
  CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 "$BIN" --java-home "$JDK" --Xmx "${CV_XMX:-1500m}" "${EXTRA[@]}" \
    "@$HERE/$AF" -Dcraton.batch=1 MethodRunner "$@" >"$OUT" 2>&1
fi
rc=$?; end=$(date +%s%3N)
echo "TAG=$TAG rc=$rc wall_ms=$((end-start))"
grep -aE '@@RESULT|@@TESTFAIL' "$OUT" | head -4
