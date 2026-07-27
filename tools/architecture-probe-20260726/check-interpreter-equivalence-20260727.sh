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

"$java_home/bin/javac" -d "$classes" \
    "$script_dir/ArchitectureInterpreterSemantics20260727.java"

run_probe() {
    timeout 180 "$exe" --nojit --java-home "$java_home" "$@" \
        -cp "$classes" ArchitectureInterpreterSemantics20260727 20000 |
        tail -1
}

verified="$(run_probe)"
decoded="$(run_probe --noverify)"

if [ "$verified" != "$decoded" ]; then
    echo "interpreter mismatch: verified=$verified decoded=$decoded" >&2
    exit 1
fi

echo "verified raw-byte path == forced decoded fallback: $verified"
