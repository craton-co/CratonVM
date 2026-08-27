#!/usr/bin/env bash
# RFJP.1 at the OPTIMIZING tier, at a recursion depth the shipped vector does
# not reach.
#
# WHY THIS EXISTS
#
# `is_fjp_subclass_blocklisted` (RFJP.1) forces the interpreter for every
# method on a `ForkJoinTask` subclass, because a recursive
# `RecursiveTask<Long>.compute()` returned 0 past depth ~10: a long local on
# the caller's operand stack lived in a register the recursive callee
# clobbered. The note names "regalloc / spill handling" — i.e. the OPTIMIZING
# tier — as where the real fix belongs.
#
# The blocklist is not free: it also catches `CompletableFuture$UniCompose` and
# `$UniRelay` (they extend `Completion` extends `ForkJoinTask`), which is the
# whole composition hot path, and lifting it measures 1.63x on
# `HibfixComposeProbe2`. Anyone narrowing it needs to know whether the original
# miscompile still reproduces. This is that test.
#
# WHY THE EXISTING COVERAGE IS NOT ENOUGH
#
#   * `regression-suite/src/RJdkForkJoin.java` is a JDKONLY_CLASSES vector, so
#     a default CORE run never schedules it; and
#   * its `SumTask` sums a `long[20000]` with a threshold of 64, i.e. depth
#     ~log2(20000/64) ~= 9 — just BELOW the "~10+" the note names; and
#   * left to itself the method compiles at C1 only. Twelve rounds over
#     ~100k tasks did not tier it up, so a run without the flags below
#     exercises the fast tier and says nothing about regalloc.
#
# THE VACUOUS-GREEN GUARD
#
# The failure mode this script exists to avoid is passing without having
# compiled the method at C2 at all. So the tier is not assumed: the run is
# traced, and a PASS is only reported if the target method was seen entering
# the optimizing pipeline. No admission line => FAIL, loudly.
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
# Windows-form paths throughout: `javac` and `cratonvm.exe` are Windows
# binaries and cannot read an MSYS `/c/...` path.
winpath() { if command -v cygpath >/dev/null 2>&1; then cygpath -m "$1"; else
  printf %s "$1" | sed -E 's|^/([a-zA-Z])/|:/|'; fi; }
HERE="$(winpath "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)")"
CV="${CV:-$HERE/../target/release/cratonvm.exe}"
JDK="${JDK:-$(ls -d "C:/Program Files/Eclipse Adoptium"/jdk-2* 2>/dev/null | head -1)}"
N="${N:-200000}"          # depth ~= log2(N/2) ~= 17
OUT="$(winpath "$(mktemp -d 2>/dev/null || echo "$HERE/.fjpc2")")"; mkdir -p "$OUT"
TARGET='FjpDeepSum$SumTask.compute()Ljava/lang/Long;'

"$JDK/bin/javac.exe" -nowarn -d "$OUT" "$HERE/FjpDeepSum.java" || { echo "FAIL: javac"; exit 1; }

log="$OUT/run.log"
# `FORCE_C2` alone is not enough: it sent C2 to the covariant-return BRIDGE
# (`compute()Ljava/lang/Object;`) and left the real body at C1. Both flags are
# needed to admit the Long-returning body to the optimizing pipeline.
CRATONVM_JIT_C2_FIRST_CALL=1 \
CRATONVM_JIT_FORCE_C2=1 \
CRATONVM_JIT_FJP_SUBCLASS_BLOCKLIST="${BLOCKLIST:-0}" \
CRATONVM_DBG_JITC=1 \
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
  "$CV" --java-home "$JDK" --Xmx 1500m -Dn="$N" -cp "$OUT" FjpDeepSum > "$log" 2>&1

verdict="$(grep -ao '@@FJP [A-Z]* n=[0-9]* approx_depth=[0-9]*' "$log" | head -1)"
admitted="$(grep -acF "admission $TARGET: admitted to the optimizing pipeline" "$log")"
compiled="$(grep -acF "full-compile $TARGET" "$log")"

echo "  verdict : ${verdict:-<none>}"
echo "  C2 admission for the Long-returning compute : $admitted"
echo "  published compiles of it                    : $compiled"

if [ "$admitted" -eq 0 ]; then
  # The whole point. A green here without this line would be a test that
  # never ran the code it exists to test.
  echo "FAIL fjp-c2-deep-recursion: the target never entered the OPTIMIZING pipeline."
  echo "     The result below is about the C1 tier and says nothing about RFJP.1."
  exit 1
fi
case "$verdict" in
  *PASS*) echo "PASS fjp-c2-deep-recursion (C2, depth ~17)"; exit 0 ;;
  *)      echo "FAIL fjp-c2-deep-recursion: ${verdict:-no verdict line}"; exit 1 ;;
esac
