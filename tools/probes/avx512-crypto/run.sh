#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
set -euo pipefail
vm=$(realpath "${1:?usage: run.sh /absolute/path/to/cratonvm}")
repo=$(cd "$(dirname "$0")/../../.." && pwd)
java=${JAVA:-java}
javac=${JAVAC:-javac}
work=$(mktemp -d "${TMPDIR:-/tmp}/cvm-avx512-crypto.XXXXXX")
finish() { status=$?; if [ "$status" -eq 0 ]; then rm -rf -- "$work"; else echo "Probe artifacts: $work" >&2; fi; }
trap finish EXIT
mkdir "$work/classes" "$work/fixtures"
"$java" -version
"$javac" -d "$work/classes" "$repo/tools/probes/avx512-crypto/Avx512CryptoProbe.java" "$repo/tools/probes/SbrW5Bigint.java"
"$java" -cp "$work/classes" Avx512CryptoProbe prepare "$work/fixtures"
"$java" -cp "$work/classes" Avx512CryptoProbe check "$work/fixtures" "$work/hotspot" > "$work/reference.out"
opens=(--add-opens=java.base/java.math=ALL-UNNAMED --add-opens=java.base/sun.security.util.math.intpoly=ALL-UNNAMED)
# Java transcription is the oracle for unusual reflected EC limbs.
"$java" -Xint "${opens[@]}" -cp "$work/classes" SbrW5Bigint fuzz > "$work/fuzz-reference.out"
for mode in jdk-only compatible; do
    for setting in default on veto; do
        flags=''
        if [ "$setting" != default ]; then flags='avx512-rsa,avx512-ec,avx512-aes'; fi
        if [ "$setting" = veto ]; then flags+=',avx512=0'; fi
        env CRATONVM_JIT="$flags" "$vm" --"$mode" -cp "$work/classes" Avx512CryptoProbe check "$work/fixtures" "$work/$mode-$setting" > "$work/$mode-$setting.out" 2> "$work/$mode-$setting.err"
        cmp "$work/reference.out" "$work/$mode-$setting.out"
        "$java" -cp "$work/classes" Avx512CryptoProbe verify "$work/fixtures" "$work/$mode-$setting"
        env CRATONVM_JIT="$flags" "$vm" --"$mode" "${opens[@]}" -cp "$work/classes" SbrW5Bigint fuzz > "$work/fuzz-$mode-$setting.out" 2> "$work/fuzz-$mode-$setting.err"
        cmp "$work/fuzz-reference.out" "$work/fuzz-$mode-$setting.out"
        echo "$mode $setting JCA_AND_KERNELS_HOTSPOT_MATCH"
        # Compilation-worker panics may fall back to interpretation; expose them.
        count=$(grep -c 'panicked' "$work/$mode-$setting.err" || true)
        fuzz_count=$(grep -c 'panicked' "$work/fuzz-$mode-$setting.err" || true)
        echo "jit_worker_panics jca=$count kernels=$fuzz_count"
    done
done
