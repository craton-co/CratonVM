#!/usr/bin/env bash
# Per-FAMILY retirement safety, using the dial now that it works.
#
# The earlier armed sweep armed all of java/io/ + java/nio/ at once, which is
# not a retirement anyone would perform. A retirement is per family. This arms
# one family at a time and asks whether the lane's thirteen probes still agree
# with HotSpot.
#
# The oracle output does not depend on the scope, so it is captured ONCE and
# reused -- otherwise this is 156 redundant HotSpot runs.
#
# Reading the result: the floor was `diff=2` until 2026-09-10 — the
# `FileInputStream.skip` residual, which is now FIXED, so **the floor is 0**.
# Anything above it is what arming that family costs today.
#
# W / CV / OUT / JDK COME FROM THE ENVIRONMENT, with the 2026-08-28 lane's
# values as defaults. They were hard-coded to that lane's worktree and its
# frozen binary, which made this script unrunnable from anywhere else — a
# retirement instrument only its author could point at anything. `l4run.sh`
# beside it already took `$CV`; this now matches.
set +e
W="${W:-$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null)}"
W="${W:-/data/cvm-l4io-20260828}"
OUT="${OUT:-/data/l4fam}"
JDK="${JDK:-/data/toolchain/jdk-25}"
CV="${CV:-/data/vm-l4io}"
rm -rf "$OUT"; mkdir -p "$OUT/oracle"
cd "$W" || exit 1
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1

PROBES="L4CensusTail L4BridgeSweep L4TailSweep2 L4TypedBufferSweep L4FileSweep \
L4FilesSweep L4ByteBufferSweep L4PrintStreamSweep L4StreamTailSweep \
TailFamilySweep IoSystemSweep FilesSweep FilePathSweep"

norm() { sed -i -E "s#/tmp/([a-z]{3,})[0-9]{6,}#/tmp/\1<TMP>#g" "$1"; }

for C in $PROBES; do
  timeout 900 "$JDK/bin/java" -cp "$W/apps/probes/out" "$C" > "$OUT/oracle/$C" 2>/dev/null
  norm "$OUT/oracle/$C"
done
echo "oracle captured"

# One family per line. Deliberately NOT java/io/ or java/nio/ wholesale.
SCOPES="
java/io/PrintStream
java/io/File
java/io/FileInputStream
java/io/FileOutputStream
java/io/ByteArrayInputStream
java/io/ByteArrayOutputStream
java/io/DataInputStream
java/io/DataOutputStream
java/io/BufferedReader
java/io/BufferedWriter
java/io/FilterOutputStream
java/nio/ByteBuffer
java/nio/CharBuffer
java/nio/file/Files
java/nio/file/Path
java/nio/file/spi/FileSystemProvider
java/nio/file/attribute/
java/nio/channels/FileChannel
"

printf '%-42s %8s %8s  %s\n' SCOPE DIFF SHORT WORST
for S in $SCOPES; do
  tot=0; short=0; worst=""; worstn=0
  for C in $PROBES; do
    CRATONVM_ENFORCE_NATIVE_SHADOW="$S" timeout 900 "$CV" --java-home "$JDK" \
        --jdk-only -cp "$W/apps/probes/out" "$C" > "$OUT/$C.armed" 2>/dev/null
    norm "$OUT/$C.armed"
    ol=$(wc -l < "$OUT/oracle/$C"); al=$(wc -l < "$OUT/$C.armed")
    [ "$al" -lt "$ol" ] && short=$((short+1))
    d=$(diff "$OUT/oracle/$C" "$OUT/$C.armed" | grep -ac '^[<>]')
    tot=$((tot+d))
    if [ "$d" -gt "$worstn" ]; then worstn=$d; worst="$C($d)"; fi
  done
  printf '%-42s %8s %8s  %s\n' "$S" "$tot" "$short" "$worst"
done
echo FAM-DONE
