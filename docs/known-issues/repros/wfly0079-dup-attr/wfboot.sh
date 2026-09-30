#!/usr/bin/env bash
# One WildFly 32 standalone boot under CratonVM, with the WFLYCTL0079 canary.
# Usage: wfboot.sh <slot> <outdir> [cvm-binary] [reps]
set -u
SLOT="${1:-0}"
OUT="${2:-/data/tmp/wfly0079/boots/slot$SLOT}"
BIN="${3:-/data/tmp/wfly0079/cvm-base-0079}"
REPS="${4:-0}"
WF=/data/data/wildfly-dist-keep/wildfly-32.0.1.Final
BASE=/data/tmp/wfly0079/bases/base$SLOT
OFFSET=$((10000 + SLOT * 200))

mkdir -p "$OUT"
rm -rf "$BASE"
mkdir -p "$BASE/deployments"
cp -r "$WF/standalone/configuration" "$BASE/"

export JAVA_HOME=/data/tmp/wfly0079/cvm-javahome
export CVM_BIN="$BIN"
export JBOSS_HOME="$WF"
export TMPDIR=/data/data/tmp

CONSOLE="$OUT/console.log"
: > "$CONSOLE"

setsid "$WF/bin/standalone.sh" \
  -Djboss.server.base.dir="$BASE" \
  -Djboss.socket.binding.port-offset="$OFFSET" \
  -Djboss.bind.address=127.0.0.1 \
  -Djboss.bind.address.management=127.0.0.1 \
  -Dcvm.dupattr.reps="$REPS" \
  -Dcvm.dupattr.threads="${CANARY_THREADS:-1}" \
  ${CANARY_SELFTEST:+-Dcvm.dupattr.selftest=1} \
  > "$CONSOLE" 2>&1 &
P=$!

# Generous: under heavy host load a boot plus a few thousand canary reps can
# take minutes, and a timed-out boot is a wasted sample, not a signal.
for i in $(seq 1 "${BOOT_WAIT_TICKS:-240}"); do
  if grep -qE "WFLYSRV0025|WFLYSRV0026|WFLYSRV0049.*aborted" "$CONSOLE" 2>/dev/null; then break; fi
  kill -0 $P 2>/dev/null || break
  sleep 2
done
sleep 1
kill -9 -$P 2>/dev/null
wait $P 2>/dev/null

# Classify
if grep -qE "CVM-DUPATTR-CANARY FAIL|CVM-DUPATTR-REAL FAIL|already registered|WFLYCTL0043" "$CONSOLE"; then
  echo "HIT"
  exit 3
fi
if grep -qE "WFLYSRV0025|WFLYSRV0026" "$CONSOLE"; then
  echo "BOOT-OK"
  exit 0
fi
echo "BOOT-INCOMPLETE"
exit 1
