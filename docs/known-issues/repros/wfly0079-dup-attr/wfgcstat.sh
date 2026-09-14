#!/usr/bin/env bash
# Boot WildFly once, shut it down gracefully, and print the GC summary.
set -u
WF=/data/data/wildfly-dist-keep/wildfly-32.0.1.Final
SLOT=9
BASE=/data/tmp/wfly0079/bases/base$SLOT
C=/data/tmp/wfly0079/boots/gcstat.log
REPS="${1:-0}"

rm -rf "$BASE"; mkdir -p "$BASE/deployments"
cp -r "$WF/standalone/configuration" "$BASE/"
export JAVA_HOME=/data/tmp/wfly0079/cvm-javahome
export CVM_BIN=/data/tmp/wfly0079/cvm-base-0079
export JBOSS_HOME="$WF"
export TMPDIR=/data/data/tmp
export CRATONVM_GC_STATS=1
: > "$C"
setsid "$WF/bin/standalone.sh" \
  -Djboss.server.base.dir="$BASE" \
  -Djboss.socket.binding.port-offset=11800 \
  -Djboss.bind.address=127.0.0.1 \
  -Djboss.bind.address.management=127.0.0.1 \
  -Dcvm.dupattr.reps="$REPS" > "$C" 2>&1 &
P=$!
for i in $(seq 1 90); do
  grep -qE 'WFLYSRV0025|WFLYSRV0026' "$C" && break
  kill -0 $P 2>/dev/null || break
  sleep 2
done
sleep 2
JP=$(pgrep -f "jboss.server.base.dir=$BASE" | head -1)
[ -n "$JP" ] && kill -TERM "$JP"
sleep 15
kill -9 -$P 2>/dev/null
echo "--- GC summary ---"
grep -aE '\[GC\]' "$C" | head -20
