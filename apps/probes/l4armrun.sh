#!/usr/bin/env bash
# Every L4 probe against HotSpot with the shadow dial ARMED.
#
# Armed, java.io/java.nio Bridge natives yield to real JDK bytecode, which then
# calls DOWN into a second layer of natives that is otherwise dead: 16
# UnixFileSystem rows, 190 invocations, reachable no other way. Unarmed runs
# cannot see a defect there, and this lane has ~90 fixes on it since the one
# time that layer was measured.
#
# Armed output should be MORE like HotSpot, not less: the dial replaces our
# native with the JDK's own bytecode. Any row that differs armed but matches
# unarmed is a defect in the fallback path.
set +e
W=/data/cvm-l4io-20260828
OUT=/data/l4armed
JDK=/data/toolchain/jdk-25
CV=/data/vm-l4io
SCOPE="${SCOPE:-java/io/,java/nio/}"
rm -rf "$OUT"; mkdir -p "$OUT"
cd "$W" || exit 1
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1

for C in L4CensusTail L4BridgeSweep L4TailSweep2 L4TypedBufferSweep L4FileSweep \
         L4FilesSweep L4ByteBufferSweep L4PrintStreamSweep L4StreamTailSweep \
         TailFamilySweep IoSystemSweep FilesSweep FilePathSweep; do
  timeout 900 "$JDK/bin/java" -cp "$W/apps/probes/out" "$C" > "$OUT/$C.oracle" 2>/dev/null
  o=$?
  CRATONVM_ENFORCE_NATIVE_SHADOW="$SCOPE" timeout 900 "$CV" --java-home "$JDK" \
      --jdk-only -cp "$W/apps/probes/out" "$C" > "$OUT/$C.armed" 2>/dev/null
  a=$?
  for arm in oracle armed; do
    sed -i -E "s#/tmp/([a-z]{3,})[0-9]{6,}#/tmp/\1<TMP>#g" "$OUT/$C.$arm"
  done
  d=$(diff "$OUT/$C.oracle" "$OUT/$C.armed" | grep -ac '^[<>]')
  printf '%-22s rc oracle=%s armed=%s   lines %s/%s   DIFF %s\n' \
      "$C" "$o" "$a" "$(wc -l < "$OUT/$C.oracle")" "$(wc -l < "$OUT/$C.armed")" "$d"
  diff "$OUT/$C.oracle" "$OUT/$C.armed" > "$OUT/$C.diff"
done
echo ARMED-DONE
