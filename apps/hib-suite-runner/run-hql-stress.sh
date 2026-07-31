#!/usr/bin/env bash
# =============================================================================
# run-hql-stress.sh — drive HqlParseStress across the (jit x gc-stress) matrix
#
# Fast inner-loop gate for the native-ANTLR root defect. Each arm parses
# `--iters` x 10 valid HQL statements whose shape is the one the defect breaks
# (`<expression> <comparison-op>`) and reports the misparse count. Arms run
# concurrently; each prints one `arm=<name> misparsed=<n> rc=<n>` line.
#
# usage: run-hql-stress.sh --bin <cratonvm.exe> [--iters N] [--tag NAME]
#                          [--repeat N]
# =============================================================================
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1

HERE="C:/craton/CratonVM/apps/hib-suite-runner"
COMMON="$HERE/common.args"
JDK="${JDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot}"
CV_XMX="${CV_XMX:-1500m}"
TIMEOUT="${TIMEOUT:-1800}"

BIN=""
ITERS=200
TAG="hqlstress"
REPEAT=1

while [ $# -gt 0 ]; do
  case "$1" in
    --bin)    BIN="$2"; shift 2;;
    --iters)  ITERS="$2"; shift 2;;
    --tag)    TAG="$2"; shift 2;;
    --repeat) REPEAT="$2"; shift 2;;
    *) echo "unknown option: $1" >&2; exit 2;;
  esac
done
[ -f "$BIN" ] || { echo "ERROR: binary not found: $BIN (pass --bin)" >&2; exit 1; }

OUT="$HERE/runs/hqlstress-$TAG-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$OUT"

# arm name | jit | CRATONVM_GC_STRESS (empty = default GC pacing)
ARMS=(
  "jit-plain|on|"
  "jit-stress4m|on|4194304"
  "jit-stress1m|on|1048576"
  "nojit-plain|off|"
  "nojit-stress4m|off|4194304"
)

run_arm() {
  local name="$1" jit="$2" stress="$3" rep="$4"
  local flags=(--java-home "$JDK" --Xmx "$CV_XMX")
  [ "$jit" = off ] && flags+=(--nojit)
  flags+=(@"$COMMON")
  local log="$OUT/$name-r$rep.log"
  if [ -n "$stress" ]; then export CRATONVM_GC_STRESS="$stress"; else unset CRATONVM_GC_STRESS; fi
  CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout "$TIMEOUT" \
      "$BIN" "${flags[@]}" HqlParseStress "$ITERS" >"$log" 2>&1
  local rc=$?
  local line; line=$(grep -m1 '^@@HQLSTRESS ' "$log")
  local n; n=$(printf '%s' "$line" | grep -o 'misparsed=[0-9]*' | cut -d= -f2)
  printf 'arm=%-16s rep=%s misparsed=%s rc=%s\n' "$name" "$rep" "${n:-NORESULT}" "$rc"
}

echo "=== $TAG :: HqlParseStress iters=$ITERS repeat=$REPEAT bin=$BIN ==="
for ((r=0; r<REPEAT; r++)); do
  pids=()
  for spec in "${ARMS[@]}"; do
    IFS='|' read -r name jit stress <<<"$spec"
    ( run_arm "$name" "$jit" "$stress" "$r" ) & pids+=($!)
  done
  for p in "${pids[@]}"; do wait "$p"; done
done
echo "=== $TAG done | logs=$OUT ==="
