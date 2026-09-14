#!/usr/bin/env bash
# The backtrace census `simpleclienthttpresponse-hang-is-not-arena-fragmentation`
# lists as not-yet-done, taken SAFELY.
#
# CRATONVM_DBG_COERCION=1 on this class wrote 690 MB in 120 s and filled `/` on
# this host on 2026-08-29. Three things keep that from recurring:
#   * output goes to /data (62 GiB free), never /tmp (under 1 GiB free),
#   * `head -c` bounds the stream regardless of how long the run lasts,
#   * the run is capped in wall time as well.
# A truncated stream is fine for this question: the storm is homogeneous and
# what is wanted is WHICH SITES coerce, not how many times.
set -uo pipefail

RUNNER=/data/cratonvm/apps/spring-suite-runner
OUT=${OUT:-/data/cvm-coerce-20260829/census}
BIN=${CRATONVM_BIN:?set CRATONVM_BIN}
export CRATONVM_BIN
export JDK25=${JDK25:-/data/toolchain/jdk-25}
CLS=org.springframework.http.client.SimpleClientHttpResponseTests
CAP=${CAP:-90}
BYTES=${BYTES:-150000000}   # 150 MB hard ceiling on each arm's log

mkdir -p "$OUT"
df -h /data | tail -1

run() {  # run <name> <extra one.sh args...>
  local name=$1; shift
  echo "=== $name ==="
  # `head -c` closes the pipe at the ceiling; the VM then dies of SIGPIPE, which
  # is the intended stop and not a finding.
  CRATONVM_DBG_COERCION=1 timeout "$CAP" bash "$RUNNER/one.sh" "$CLS" "$@" 2>&1 \
    | head -c "$BYTES" > "$OUT/$name.log"
  ls -la "$OUT/$name.log" | awk '{print "  bytes="$5}'
}

run coercion-default
run coercion-nojit --nojit

echo "=== disk after ==="
df -h /data | tail -1
