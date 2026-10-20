#!/usr/bin/env bash
# The C JVMTI shim runner (interpreter round i5 wave 103, lane L1): the twin
# of tools/jdi/run-jdi-conformance.sh for the probes that need a native
# JVMTI agent.
#
# Each scenario is a probe pair under tools/probes/interp/<lane>/:
# <Scenario>.java and its shim <Scenario>.c, loaded both as an agent
# (`-agentpath:<lib>`) and with `System.load(<lib>)`, the library path being
# the probe's one argument (the convention of every L1 JVMTI probe since
# wave 44). The runner builds the shim with gcc, compiles the probe with
# `javac -g`, runs it on HotSpot (the reference) and on CratonVM in each mode
# asked for, and diffs each CratonVM stdout against HotSpot's. Stderr is kept
# beside it but not compared (HotSpot prints its restricted-method WARNING
# lines for `System.load` there).
#
#   tools/jvmti/run-jvmti-shims.sh --jdk /opt/jdk-25 \
#       --cratonvm target/release/cratonvm [--modes "jdk-only compatible jdk-only:nojit"] \
#       [--scenario L1W103JvmtiLocalScopeEnd ...] [--out /tmp/jvmti-shims] [--cc gcc]
#
#   # HotSpot only (prints each transcript; for refreshing a probe header):
#   tools/jvmti/run-jvmti-shims.sh --jdk /opt/jdk-25 --hotspot-only
#
# Linux (gcc, $JDK/include/linux). CratonVM must be built with
# `--features cratonvm-vm/experimental-debug` (the C JVMTI table is compiled
# only then). A mode is `jdk-only` (a bare invocation) or `compatible`
# (`--compatible`), with `:nojit` for `--nojit`. Exit status: 0 when every
# transcript matches, 1 otherwise, 2 on a usage or setup error.
set -u

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"

probe_dir() {
    for d in "$repo"/tools/probes/interp/*/; do
        if [ -f "$d$1.java" ] && [ -f "$d$1.c" ]; then
            echo "${d%/}"
            return
        fi
    done
    echo ""
}

jdk=""
cratonvm=""
modes="jdk-only compatible"
scenarios=()
out="${TMPDIR:-/tmp}/jvmti-shims"
cc="gcc"
hotspot_only=0
run_timeout=300

usage() {
    sed -n '2,28p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --jdk) jdk="$2"; shift 2 ;;
        --cratonvm) cratonvm="$2"; shift 2 ;;
        --modes) modes="$2"; shift 2 ;;
        --scenario) scenarios+=("$2"); shift 2 ;;
        --out) out="$2"; shift 2 ;;
        --cc) cc="$2"; shift 2 ;;
        --hotspot-only) hotspot_only=1; shift ;;
        -h|--help) usage ;;
        *) echo "unknown argument: $1" >&2; usage ;;
    esac
done

if [ ${#scenarios[@]} -eq 0 ]; then
    # The probes whose output is deterministic on both VMs. Pages each one
    # proves: L1W44 (wave 44 locals and stack), L1W101 (i101-L1 out-param
    # order), L1W102 (i102-L1 thread lists), L1W103 (i100-L1 scope end).
    # Wave 104: L1W104 (i101-L6 DestroyJavaVM thread events).
    # Wave 107: L1W107 (i106-L1 detach posts ThreadEnd before ensure_join;
    # i107-L1 detach hands the pending exception to the uncaught handler).
    scenarios=(L1W44JvmtiLocalsAndStack L1W101JvmtiOutParamBeforeThread
        L1W102JvmtiPlatformThreadsOnly L1W103JvmtiLocalScopeEnd
        L1W104JvmtiDestroyJavaVmThreadEvents L1W107JvmtiDetachThreadEndBeforeJoin
        L1W107JniDetachPendingException)
fi
[ -n "$jdk" ] || { echo "--jdk is required" >&2; exit 2; }
java="$jdk/bin/java"
javac="$jdk/bin/javac"
[ -x "$java" ] || { echo "no java under $jdk/bin" >&2; exit 2; }
command -v "$cc" > /dev/null || { echo "no C compiler: $cc" >&2; exit 2; }
if [ "$hotspot_only" -eq 0 ]; then
    [ -n "$cratonvm" ] || { echo "--cratonvm is required (or --hotspot-only)" >&2; exit 2; }
    [ -x "$cratonvm" ] || { echo "not an executable: $cratonvm" >&2; exit 2; }
fi

mkdir -p "$out"
# Absolute paths: every JVM runs with its working directory in $out (below),
# so a crash's hs_err / core file lands there, never in the source tree
# (the w103 run left HotSpot's in the checkout).
abs() {
    case "$1" in
        /*) echo "$1" ;;
        */*) echo "$(cd "$(dirname "$1")" && pwd)/$(basename "$1")" ;;
        *) command -v "$1" || echo "$1" ;;
    esac
}
out="$(cd "$out" && pwd)"
jdk="$(cd "$jdk" && pwd)"
java="$jdk/bin/java"
javac="$jdk/bin/javac"
if [ -n "$cratonvm" ]; then
    cratonvm="$(abs "$cratonvm")"
fi
failed=0
for s in "${scenarios[@]}"; do
    dir="$(probe_dir "$s")"
    if [ -z "$dir" ]; then
        echo "FAIL $s: no $s.java + $s.c under tools/probes/interp/" >&2
        failed=1
        continue
    fi
    lib="$out/lib$s.so"
    classes="$out/$s"
    mkdir -p "$classes"
    if ! "$cc" -shared -fPIC -pthread -I"$jdk/include" -I"$jdk/include/linux" \
            -o "$lib" "$dir/$s.c" 2> "$out/$s.cc.txt"; then
        echo "FAIL $s: the shim does not build ($out/$s.cc.txt)"
        failed=1
        continue
    fi
    if ! "$javac" -g -d "$classes" "$dir/$s.java" 2> "$out/$s.javac.txt"; then
        echo "FAIL $s: javac failed ($out/$s.javac.txt)"
        failed=1
        continue
    fi
    (cd "$out" && timeout "$run_timeout" "$java" "-XX:ErrorFile=$out/hs_err_$s.log" \
        -XX:-CreateCoredumpOnCrash "-agentpath:$lib" -cp "$classes" "$s" "$lib") \
        > "$out/$s.hotspot.txt" 2> "$out/$s.hotspot.err.txt"
    tr -d '\r' < "$out/$s.hotspot.txt" > "$out/$s.hotspot.tmp" && mv "$out/$s.hotspot.tmp" "$out/$s.hotspot.txt"
    if [ "$hotspot_only" -eq 1 ]; then
        echo "== $s (HotSpot)"
        sed 's/^/    /' "$out/$s.hotspot.txt"
        continue
    fi
    for mode in $modes; do
        flags=()
        case "$mode" in
            jdk-only) ;;
            compatible) flags+=(--compatible) ;;
            jdk-only:nojit) flags+=(--nojit) ;;
            compatible:nojit) flags+=(--compatible --nojit) ;;
            *) echo "unknown mode: $mode" >&2; exit 2 ;;
        esac
        tag="${mode//:/-}"
        (cd "$out" && timeout "$run_timeout" "$cratonvm" --java-home "$jdk" \
            ${flags[@]+"${flags[@]}"} "-agentpath:$lib" -cp "$classes" "$s" "$lib") \
            > "$out/$s.$tag.txt" 2> "$out/$s.$tag.err.txt"
        tr -d '\r' < "$out/$s.$tag.txt" > "$out/$s.$tag.tmp" && mv "$out/$s.$tag.tmp" "$out/$s.$tag.txt"
        if diff -u "$out/$s.hotspot.txt" "$out/$s.$tag.txt" > "$out/$s.$tag.diff"; then
            echo "PASS $s [$mode]"
        else
            echo "FAIL $s [$mode] ($out/$s.$tag.diff):"
            grep -E '^[-+]' "$out/$s.$tag.diff" | grep -vE '^(---|\+\+\+) ' | sed 's/^/    /'
            failed=1
        fi
    done
done
exit "$failed"
