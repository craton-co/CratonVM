#!/usr/bin/env bash
# Usage: run-bytebuf-probe.sh CRATONVM JAVA_HOME NETTY_CLASSPATH OUTPUT_DIR [allow|default]
set -euo pipefail
[[ $# == 4 || $# == 5 ]] || { echo 'Expected CRATONVM JAVA_HOME NETTY_CLASSPATH OUTPUT_DIR [allow|default]' >&2; exit 2; }
policy=${5:-allow}
case $policy in
    allow) unsafe_args=(--sun-misc-unsafe-memory-access=allow); expected_unsafe=true;;
    default) unsafe_args=(); expected_unsafe=false;;
    *) echo 'Unsafe policy must be allow or default' >&2; exit 2;;
esac
cv=$(realpath "$1")
java_home=$(realpath "$2")
netty_cp=$3
out=$(realpath -m "$4")
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
mkdir -p "$out/classes"
"$java_home/bin/javac" -cp "$netty_cp" -d "$out/classes" "$here/NettyByteBufProbe.java"
cp="$out/classes:$netty_cp"
printf '%s\n' "$cp" > "$out/classpath.txt"
"$java_home/bin/java" -version > "$out/java-version.txt" 2>&1
sha256sum "$cv" > "$out/cratonvm-sha256.txt"
printf '%s\n' "$policy" > "$out/unsafe-policy.txt"
for rep in 1 2 3 4; do
    for kind in 0 1 2 3; do
        # Reverse the order every other repetition to expose load drift.
        arms=(hotspot foundational compatible)
        if (( rep % 2 == 0 )); then arms=(compatible foundational hotspot); fi
        for arm in "${arms[@]}"; do
            case $arm in
                hotspot) command=("$java_home/bin/java" "${unsafe_args[@]}");;
                foundational) command=("$cv" --java-home "$java_home" "${unsafe_args[@]}");;
                compatible) command=("$cv" --java-home "$java_home" "${unsafe_args[@]}" --compatible);;
            esac
            prefix="$out/$rep-$kind-$arm"
            cat /proc/loadavg > "$prefix.load"
            timeout 180 perf stat -x , -e instructions:u -o "$prefix.perf" -- \
                "${command[@]}" -cp "$cp" NettyByteBufProbe 1000000 "$kind" > "$prefix.log" 2>&1
            cat /proc/loadavg >> "$prefix.load"
            grep -q "^unsafe=$expected_unsafe$" "$prefix.log"
            [[ $(grep -c 'checksum=8198552683866904288$' "$prefix.log") == 5 ]]
        done
    done
done
