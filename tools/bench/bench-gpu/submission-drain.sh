#!/usr/bin/env bash
# Does the GPU submission registry actually drain?
#
# `offload::SUBMISSIONS` has exactly one insert and one remove, and until
# 2026-09-02 the remove had no production caller: every async submission
# stayed registered for the life of the process. `GpuExecutor.
# releaseSubmission(h)` compiles to `Native.releaseFuture(h)`, which
# removed the entry from `native-builtins`' own future table and stopped
# there -- the offload registry, keyed by the same handle, was never
# touched. So NO program could drain it, however correctly written.
#
# `GpuAsyncChainBench` is the right witness precisely because it does
# everything right: it awaits and releases every handle it takes. Before
# the fix it still reported `released=0 live_at_exit=2001` and tripped
# the runtime's "1024 submissions are alive" warning.
#
# WHAT THIS ASSERTS, and why each part:
#
#   registered > 0    else the run never offloaded and proves nothing --
#                     a no-device box would otherwise "pass".
#   live_at_exit == 0 the drain happened.
#   peak_live         reported, and must not exceed the number of
#                     launches: the bound should be the program's own
#                     outstanding-chain length, not every submission it
#                     ever made. A drain that only ran at exit would
#                     still show live_at_exit=0 but a peak of `launches *
#                     rounds`, so this is what distinguishes "drained"
#                     from "drained too late to matter".
#
# `CRATONVM_GPU_NO_SUBMISSION_DRAIN=1` restores the old behaviour, which
# is this script's own negative control -- run it that way and every
# assertion below must fail.
#
# Usage:
#   CV=path/to/cratonvm.exe JDK=path/to/jdk bash bench-gpu/submission-drain.sh [n] [launches] [rounds]
set -u
export MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

CV="${CV:-$ROOT/target-gpu/release/cratonvm.exe}"
JDK="${JDK:-C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot}"
N="${1:-65536}"
LAUNCHES="${2:-400}"
ROUNDS="${3:-5}"

# The craton.gpu classes are a build artefact of the `cratonvm-gpu`
# crate, compiled from an external Maven project rather than from .java
# in this tree -- which is why searching this repo for `GpuExecutor.java`
# finds nothing even though the class exists.
JAR="${GPU_JAR:-}"
if [ -z "$JAR" ]; then
  JAR=$(ls -1 "$ROOT"/target*/release/build/cratonvm-gpu-*/out/craton-gpu-annotations.jar 2>/dev/null | head -1)
fi
if [ -z "$JAR" ] || [ ! -f "$JAR" ]; then
  echo "SKIP: craton-gpu-annotations.jar not found (build the cratonvm-gpu crate first)"
  exit 0
fi
# javac and the VM are Windows binaries and cannot open the MSYS-style
# "/c/..." path the glob above produces.
if command -v cygpath >/dev/null 2>&1; then
  JAR="$(cygpath -m "$JAR")"
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "compiling GpuAsyncChainBench"
TMPW="$TMP"
if command -v cygpath >/dev/null 2>&1; then
  TMPW="$(cygpath -m "$TMP")"
fi
(cd "$ROOT" && "$JDK/bin/javac" -cp "$JAR" -d "$TMPW" "bench-gpu/GpuAsyncChainBench.java") || exit 1

echo "=== gpu submission registry drain (n=$N launches=$LAUNCHES rounds=$ROUNDS) ==="
echo "CV=$CV"

"$CV" --java-home "$JDK" -cp "$JAR;$TMPW" --gpu \
    GpuAsyncChainBench "$N" "$LAUNCHES" "$ROUNDS" > "$TMP/out" 2> "$TMP/err"

census=$(grep "gpu submissions:" "$TMP/err" | head -1)
if [ -z "$census" ]; then
  echo "FAIL: no submission census line. Either nothing was offloaded or the"
  echo "      census is not wired -- see types::gpu_submission_census."
  sed -n '1,12p' "$TMP/err" | sed 's/^/       /'
  exit 1
fi
echo "  $census"

num() { printf '%s' "$census" | grep -oE "$1=[0-9]+" | grep -oE "[0-9]+"; }
registered=$(num registered)
released=$(num released)
live=$(num live_at_exit)
peak=$(num peak_live)

FAILS=0

if [ "${registered:-0}" -eq 0 ]; then
  echo "  FAIL: registered=0 -- nothing was offloaded, so this run proves nothing"
  FAILS=$((FAILS + 1))
else
  echo "  PASS registered=$registered (the run really offloaded)"
fi

if [ "${live:-1}" -ne 0 ]; then
  echo "  FAIL: live_at_exit=$live -- the registry did not drain."
  echo "        GpuExecutor.releaseSubmission(h) reaches"
  echo "        builtin_release_future; that must call"
  echo "        ctx.gpu_release_submission(handle) for the offload"
  echo "        registry to see it."
  FAILS=$((FAILS + 1))
else
  echo "  PASS live_at_exit=0 (registered=$registered released=$released)"
fi

if [ "${peak:-0}" -gt "$LAUNCHES" ]; then
  echo "  FAIL: peak_live=$peak exceeds launches=$LAUNCHES -- entries are"
  echo "        accumulating across rounds, so the drain is happening too"
  echo "        late to bound anything even if live_at_exit is 0."
  FAILS=$((FAILS + 1))
else
  echo "  PASS peak_live=$peak <= launches=$LAUNCHES (bounded by the chain)"
fi

if grep -q "submissions are alive and un-finalized" "$TMP/err"; then
  echo "  FAIL: the registry's own overflow warning fired"
  FAILS=$((FAILS + 1))
fi

if ! grep -q "ok=true" "$TMP/out"; then
  echo "  FAIL: the benchmark's own checksum did not verify"
  grep -E "ASYNCCHAIN|checksum" "$TMP/out" | sed 's/^/       /'
  FAILS=$((FAILS + 1))
fi

echo
echo "=== summary ==="
if [ "$FAILS" = "0" ]; then
  echo "SUBMISSION REGISTRY DRAINS"
  exit 0
fi
echo "$FAILS CHECK(S) FAILED"
exit 1
