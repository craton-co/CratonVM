#!/usr/bin/env bash
# hibfix-hr.sh <tag> <binary|HOTSPOT> [extra vm args...] -- <class...>
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -W)"
TAG="$1"; BIN="$2"; shift 2
EXTRA=(); while [ "${1:-}" != "--" ]; do EXTRA+=("$1"); shift; done; shift
mkdir -p "$HERE/runs/hibfix-20260822"
OUT="$HERE/runs/hibfix-20260822/$TAG.log"
JDK="$(ls -d "C:/Program Files/Eclipse Adoptium"/jdk-2* 2>/dev/null | head -1)"
start=$(date +%s%3N)
if [ "$BIN" = "HOTSPOT" ]; then
  "$JDK/bin/java.exe" "@$HERE/common.args" "${EXTRA[@]}" -Dcraton.batch=1 CratonRunner "$@" >"$OUT" 2>&1
else
  CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 "$BIN" --java-home "$JDK" --Xmx 1500m "${EXTRA[@]}" \
    "@$HERE/common.args" -Dcraton.batch=1 CratonRunner "$@" >"$OUT" 2>&1
fi
rc=$?; end=$(date +%s%3N)
echo "TAG=$TAG rc=$rc wall_ms=$((end-start))"
grep -aE '@@RESULT|@@TESTFAIL' "$OUT" | head -8
