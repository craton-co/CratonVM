#!/usr/bin/env bash
# l4run.sh <Probe> [more...] — the three-arm differential runner for lane L4
# (`java.io` / `java.nio`), the one the record's numbers come from.
#
#   oracle    HotSpot   ($JDK, default /data/toolchain/jdk-25 = Temurin 25.0.4+7)
#   strict    cratonvm --jdk-only
#   compat    cratonvm  (default mode)
#
# Three pieces of hygiene are encoded here rather than left to the caller,
# because each was paid for somewhere in this campaign:
#
#   * stdout ONLY (`2>/dev/null`). `2>&1` puts VM tracing into the diff, and the
#     asymmetric version of that mistake invented four phantom differences.
#   * ROW COUNTS are printed BEFORE the diff. A run that died partway produces a
#     short file whose missing tail `diff` reports as ordinary `<` lines — which
#     is how a `PrintStream` panic hid nine defects behind one, and how a
#     mis-indexed visitor argument hid two more.
#   * $CV defaults to a FROZEN copy of the binary, not to `target/release`, so a
#     concurrent rebuild cannot clobber the thing under test between arms.
#
# Usage:
#   bash probes/l4run.sh L4FileSweep L4FilesSweep L4ByteBufferSweep \
#                        L4PrintStreamSweep L4StreamTailSweep
set +e
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1

W="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null)"
if [ -z "$W" ] || [ ! -d "$W/probes" ]; then
  echo "l4run.sh: cannot resolve the repository root — run it from inside its worktree." >&2
  exit 3
fi
CV="${CV:-$W/target/release/cratonvm}"
[ -x "$CV" ] || CV="$W/target/release/cratonvm.exe"
JDK="${JDK:-${JAVA_HOME:-/data/toolchain/jdk-25}}"
OUT="${OUT:-${TMPDIR:-/tmp}/l4out}"
mkdir -p "$OUT"
cd "$W" || exit 1
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1

for C in "$@"; do
  timeout 900 "$JDK/bin/java" -cp "$W/probes/out" "$C" > "$OUT/$C.oracle" 2>/dev/null
  o=$?
  timeout 900 "$CV" --java-home "$JDK" --jdk-only -cp "$W/probes/out" "$C" > "$OUT/$C.strict" 2>/dev/null
  s=$?
  timeout 900 "$CV" --java-home "$JDK" -cp "$W/probes/out" "$C" > "$OUT/$C.compat" 2>/dev/null
  c=$?
  echo "=== $C   rc oracle=$o strict=$s compat=$c"
  echo "    lines  oracle=$(wc -l < "$OUT/$C.oracle")  strict=$(wc -l < "$OUT/$C.strict")  compat=$(wc -l < "$OUT/$C.compat")"
  echo "    rows   oracle $(grep '^rows ' "$OUT/$C.oracle") | strict $(grep '^rows ' "$OUT/$C.strict") | compat $(grep '^rows ' "$OUT/$C.compat")"
  ds=$(diff "$OUT/$C.oracle" "$OUT/$C.strict" | grep -c '^[<>]')
  dc=$(diff "$OUT/$C.oracle" "$OUT/$C.compat" | grep -c '^[<>]')
  echo "    DIFF   strict=$ds  compat=$dc"
  diff "$OUT/$C.oracle" "$OUT/$C.strict" > "$OUT/$C.strict.diff"
  diff "$OUT/$C.oracle" "$OUT/$C.compat" > "$OUT/$C.compat.diff"
done
echo "L4RUN-DONE"
