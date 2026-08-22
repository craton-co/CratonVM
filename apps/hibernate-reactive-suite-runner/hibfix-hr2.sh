#!/usr/bin/env bash
# hibfix-hr2.sh <tag> <binary> <argfile> [extra...] -- <class...>
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -W)"
TAG="$1"; BIN="$2"; AF="$3"; shift 3
EXTRA=(); while [ "${1:-}" != "--" ]; do EXTRA+=("$1"); shift; done; shift
mkdir -p "$HERE/runs/hibfix-20260822"; OUT="$HERE/runs/hibfix-20260822/$TAG.log"
JDK="$(ls -d "C:/Program Files/Eclipse Adoptium"/jdk-2* 2>/dev/null | head -1)"
start=$(date +%s%3N)
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 "$BIN" --java-home "$JDK" --Xmx 1500m "${EXTRA[@]}" \
  "@$HERE/$AF" -Dcraton.batch=1 CratonRunner "$@" >"$OUT" 2>&1
rc=$?; end=$(date +%s%3N)
echo "TAG=$TAG rc=$rc wall_ms=$((end-start))"
grep -aE '@@RESULT|@@TESTFAIL' "$OUT" | head -8
