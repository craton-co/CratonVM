#!/usr/bin/env bash
# Does a promoted invoke resolution survive a class redefinition?
#
# Builds the `java.lang.instrument` agent and the two-version fixtures, then
# runs `RedefMain` on HotSpot and on cratonvm and prints both. The lines must
# match; `otherThreadPost` is the one that matters (see RedefMain.java).
#
#   ./build-and-run.sh <path-to-cratonvm> <path-to-jdk-home> [reps]
#
# Answers the "How to reproduce" section of the retired
# `promoted-invoke-resolutions-survive-class-redefinition-20260903` write-up.
set -eu
VM=${1:?usage: build-and-run.sh <cratonvm> <java-home> [reps]}
JH=${2:?usage: build-and-run.sh <cratonvm> <java-home> [reps]}
REPS=${3:-200000}
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${TMPDIR:-/tmp}/cratonvm-redefine-probe
rm -rf "$OUT"; mkdir -p "$OUT/cp" "$OUT/v2" "$OUT/v2src" "$OUT/agent"

"$JH/bin/javac" -d "$OUT/cp" \
    "$HERE/Impl.java" "$HERE/Sup.java" "$HERE/Sub.java" \
    "$HERE/RedefAgent.java" "$HERE/RedefMain.java"

# The v2 sources declare the same classes, so they must be compiled apart.
cp "$HERE/Impl_v2.java" "$OUT/v2src/Impl.java"
cp "$HERE/Sup_v2.java"  "$OUT/v2src/Sup.java"
"$JH/bin/javac" -d "$OUT/v2" "$OUT/v2src/Impl.java" "$OUT/v2src/Sup.java"

cat > "$OUT/agent/MANIFEST.MF" <<'MF'
Premain-Class: RedefAgent
Can-Redefine-Classes: true
Can-Retransform-Classes: true
MF
"$JH/bin/jar" cfm "$OUT/agent/redefagent.jar" "$OUT/agent/MANIFEST.MF" \
    -C "$OUT/cp" RedefAgent.class

echo "== HotSpot"
"$JH/bin/java" -javaagent:"$OUT/agent/redefagent.jar" -cp "$OUT/cp" \
    RedefMain "$REPS" "$OUT/v2/Impl.class" "$OUT/v2/Sup.class"
echo "== cratonvm"
"$VM" --java-home "$JH" -javaagent:"$OUT/agent/redefagent.jar" -cp "$OUT/cp" \
    RedefMain "$REPS" "$OUT/v2/Impl.class" "$OUT/v2/Sup.class"
