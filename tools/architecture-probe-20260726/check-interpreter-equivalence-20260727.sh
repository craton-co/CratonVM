#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 2 ]; then
    echo "usage: $0 /absolute/cratonvm /absolute/java-home" >&2
    exit 2
fi

exe="$1"
java_home="$2"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
classes="$(mktemp -d)"
trap 'rm -rf "$classes"' EXIT

javac_bin="$java_home/bin/javac"
if [ ! -x "$javac_bin" ]; then
    javac_bin="$(command -v javac)"
fi

"$javac_bin" -d "$classes" \
    "$script_dir/ArchitectureInterpreterSemantics20260727.java"

run_craton() {
    timeout 180 "$exe" --java-home "$java_home" "$@" \
        -cp "$classes" ArchitectureInterpreterSemantics20260727 20000 |
        tail -1
}

verified="$(run_craton --nojit)"
decoded="$(run_craton --nojit --noverify)"
jitted="$(run_craton)"
hotspot="$(timeout 180 "$java_home/bin/java" \
    -cp "$classes" ArchitectureInterpreterSemantics20260727 20000)"

if [ "$verified" != "$decoded" ]; then
    echo "interpreter mismatch: verified=$verified decoded=$decoded" >&2
    exit 1
fi

if [ "$verified" != "$jitted" ] || [ "$verified" != "$hotspot" ]; then
    echo "execution mismatch: interpreter=$verified jit=$jitted hotspot=$hotspot" >&2
    exit 1
fi

echo "verified interpreter == decoded fallback == JIT == HotSpot: $verified"
