#!/usr/bin/env bash
# Run the standalone-CGLIB CGL.1 regression witness.
# Usage: run-cglib-probe.sh <cratonvm> <cglib-jar> <asm-jar> <jit|nojit> [runs]
set -euo pipefail

if [[ $# -lt 4 || $# -gt 5 ]]; then
    echo "usage: $0 <cratonvm> <cglib-jar> <asm-jar> <jit|nojit> [runs]" >&2
    exit 64
fi

vm=$1
cglib_jar=$2
asm_jar=$3
mode=$4
runs=${5:-1}
java_home=${JAVA_HOME:-/home/victor/jdk25}
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
classes=$(mktemp -d "${TMPDIR:-/tmp}/cglib-probe.XXXXXX")
trap 'rm -rf "$classes"' EXIT

case "$mode" in
    jit) ;;
    nojit) ;;
    *) echo "mode must be jit or nojit" >&2; exit 64 ;;
esac

"$java_home/bin/javac" -cp "$cglib_jar:$asm_jar" -d "$classes" "$root/CglibProbe.java"

for ((run = 1; run <= runs; run++)); do
    echo "CGLIB_PROBE mode=$mode run=$run/$runs"
    if [[ "$mode" == jit ]]; then
        "$vm" --java-home "$java_home" -cp "$classes:$cglib_jar:$asm_jar" CglibProbe
    else
        CRATONVM_DISABLE_JIT=1 "$vm" --java-home "$java_home" -cp "$classes:$cglib_jar:$asm_jar" CglibProbe
    fi
done