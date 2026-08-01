#!/usr/bin/env bash
# =============================================================================
# run-astparser-hunt.sh — parallel repro hunt for the HQL ordinal-parameter drop
#
# `run-astparser-witness.sh` runs the witness class SEQUENTIALLY, which is right
# for an acceptance gate but useless as a hunt: the defect this targets
# (docs/known-issues/hibernate/hql-ordinal-parameter-dropped-under-jit-20260731.md)
# reproduces about once in 14 runs of a class that takes 3-10 minutes, so a
# serial hunt costs hours per expected hit. This runs `--par` copies at once and
# keeps going for `--rounds` rounds, reporting every run and grepping each log
# for the failure's exact signature.
#
# Runs share the fixture directory as their working directory, exactly as
# `run-hib.sh`'s own parallel shards do; the suite's H2 URL is `jdbc:h2:mem:db1`,
# which is process-local, so concurrent runs cannot collide over it.
#
# usage: run-astparser-hunt.sh --bin <cratonvm.exe> [--par N] [--rounds N]
#                              [--jit on|off] [--class FQCN] [--tag NAME]
# =============================================================================
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1

HERE="C:/craton/CratonVM/apps/hib-suite-runner"
COMMON="$HERE/common.args"
RUNNER_CLASS="CratonRunner"
JDK="${JDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot}"
CV_XMX="${CV_XMX:-1500m}"
TIMEOUT="${TIMEOUT:-1200}"

BIN=""
PAR=6
ROUNDS=1
JITMODE="on"
CLS="org.hibernate.orm.test.hql.ASTParserLoadingTest"
TAG="hunt"

while [ $# -gt 0 ]; do
  case "$1" in
    --bin)    BIN="$2"; shift 2;;
    --par)    PAR="$2"; shift 2;;
    --rounds) ROUNDS="$2"; shift 2;;
    --jit)    JITMODE="$2"; shift 2;;
    --class)  CLS="$2"; shift 2;;
    --tag)    TAG="$2"; shift 2;;
    *) echo "unknown option: $1" >&2; exit 2;;
  esac
done
[ -f "$BIN" ] || { echo "ERROR: binary not found: $BIN (pass --bin)" >&2; exit 1; }

VMFLAGS=(--java-home "$JDK" --Xmx "$CV_XMX")
[ "$JITMODE" = off ] && VMFLAGS+=(--nojit)
VMFLAGS+=(@"$COMMON")

OUT="$HERE/runs/hunt-$TAG-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$OUT"
echo "=== $TAG :: $CLS par=$PAR rounds=$ROUNDS jit=$JITMODE bin=$BIN ==="
echo "=== logs=$OUT ==="

# The signature to hunt for. `getQueryParameter` renders the collected ordinal
# labels between brackets, so an EMPTY bracket pair is the drop; a non-empty one
# would be an ordinary test bug.
SIG="in query with ordinal parameters \[\]"

one_run() {
  local round="$1" slot="$2"
  local log="$OUT/run-$round-$slot.log"
  CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout "$TIMEOUT" \
      "$BIN" "${VMFLAGS[@]}" -Dcraton.batch=1 "$RUNNER_CLASS" "$CLS" >"$log" 2>&1
  local rc=$?
  local rline; rline=$(grep -m1 "^@@RESULT " "$log")
  local hits; hits=$(grep -c "$SIG" "$log")
  local ok failed
  ok=$(printf '%s' "$rline"     | grep -o 'ok=[0-9]*'     | cut -d= -f2)
  failed=$(printf '%s' "$rline" | grep -o 'failed=[0-9]*' | cut -d= -f2)
  printf 'round=%s slot=%s ok=%s failed=%s ordinalDrop=%s rc=%s\n' \
      "$round" "$slot" "${ok:-NORESULT}" "${failed:-NORESULT}" "$hits" "$rc"
  if [ "$hits" -gt 0 ]; then
    echo "*** ORDINAL DROP REPRODUCED: $log"
  fi
}

total_hits=0
for ((r=0; r<ROUNDS; r++)); do
  pids=()
  for ((s=0; s<PAR; s++)); do
    ( one_run "$r" "$s" ) & pids+=($!)
  done
  for p in "${pids[@]}"; do wait "$p"; done
  hits=$(grep -rl "$SIG" "$OUT"/run-*.log 2>/dev/null | wc -l)
  echo "--- after round $r: runs_with_ordinal_drop=$hits ---"
  total_hits=$hits
done
echo "=== $TAG SUMMARY: rounds=$ROUNDS par=$PAR runs_with_ordinal_drop=$total_hits logs=$OUT ==="
