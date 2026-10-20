#!/usr/bin/env bash
# The JDI conformance runner (interpreter round i1 wave 24, lane L1; stage 1
# of docs/internal/fixed-bugs/interpreter-L1-proposal-jdi-conformance-harness-FIXED-20261003.md).
#
# Each scenario is one probe class under tools/probes/interp/L1/ that is both
# the debuggee (`wait`) and a JDI debugger (`debug <port>`), printing a
# canonical transcript (no ids, no addresses, no timings). The runner runs
# every scenario with HotSpot's own debuggee — the reference transcript — and
# then with a CratonVM debuggee in each mode asked for, always with HotSpot's
# JDI as the debugger, and diffs each transcript against the reference. A
# differing line fails the run unless tools/jdi/known-differences.txt allows
# it, naming the page that records the difference.
#
#   tools/jdi/run-jdi-conformance.sh --jdk /opt/jdk-25 \
#       --cratonvm target/release/cratonvm [--modes "jdk-only compatible jdk-only:nojit"] \
#       [--scenario L1W24JdiThreads ...] [--out /tmp/jdi-conformance] \
#       [--debuggee-flag -agentpath:/tmp/libagent.so ...]
#
#   (--debuggee-flag, repeatable, goes to the HotSpot and every CratonVM
#   debuggee alike: e.g. a C JVMTI agent bound beside the JDWP session.)
#
#   # HotSpot only: check each probe header's transcript against HotSpot.
#   tools/jdi/run-jdi-conformance.sh --jdk /opt/jdk-25 --check-headers
#
# CratonVM must be built with `--features cratonvm-vm/experimental-debug`
# (`--jdwp-port` does nothing otherwise: the launcher warns). A mode is
# `jdk-only` (a bare invocation) or `compatible` (`--compatible`), with
# `:nojit` for `--nojit`. Exit status: 0 when every transcript matches up to
# the allow-list, 1 otherwise, 2 on a usage or setup error.
set -u

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
probes="$repo/tools/probes/interp/L1"
allow="$here/known-differences.txt"

# The source of scenario $1: under L1/, or (interpreter round i2 wave 53,
# lane L6) under any other lane's directory of tools/probes/interp/, so a
# named --scenario of another lane runs too. The default list stays L1's.
probe_file() {
    if [ -f "$probes/$1.java" ]; then
        echo "$probes/$1.java"
        return
    fi
    for f in "$repo"/tools/probes/interp/*/"$1".java; do
        [ -f "$f" ] && { echo "$f"; return; }
    done
    echo "$probes/$1.java"
}

jdk=""
cratonvm=""
modes="jdk-only compatible"
scenarios=()
out="${TMPDIR:-/tmp}/jdi-conformance"
check_headers=0
port=5790
debugger_timeout=600
debuggee_flags=()

usage() {
    sed -n '2,31p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --jdk) jdk="$2"; shift 2 ;;
        --cratonvm) cratonvm="$2"; shift 2 ;;
        --modes) modes="$2"; shift 2 ;;
        --scenario) scenarios+=("$2"); shift 2 ;;
        --out) out="$2"; shift 2 ;;
        --port) port="$2"; shift 2 ;;
        --check-headers) check_headers=1; shift ;;
        --debuggee-flag) debuggee_flags+=("$2"); shift 2 ;;
        -h|--help) usage ;;
        *) echo "unknown argument: $1" >&2; usage ;;
    esac
done

if [ ${#scenarios[@]} -eq 0 ]; then
    scenarios=(L1W23JdiConformance L1W24JdiSurface L1W24JdiThreads
        L1W25JdiStopMonitors L1W25JdiVersion L1W26JdiNativeMethodEvents
        L1W27JdiIntrinsicMethodEvents L1W28RawJdwpStaleFrameId
        L1W37JdiCompiledLoopMethodEvents L1W37RawJdwpUnknownSuspendPolicy
        L1W37JdiBreakpointInCompiledCallee L1W38JdiBreakpointInCrossClassCallee
        L1W38JdiBreakpointInStoodInCalleeOfCompiledLoop
        L1W38JdiFieldWatchInCompiledLoop L1W39JdiStepSessionOverCompiledLoop
        L1W39JdiBreakpointBeforeCallersCompile
        L1W40JdiBreakpointInJitIntrinsicCallee L1W41JdiSetValuesOnBlockedThread
        L1W42RawJdwpErrorAnswers L1W43JdiSourceDebugExtension
        L1W43JdiForceEarlyReturn L1W43RawJdwpObjectErrorAnswers
        L1W44JdiCallerFrameOfBreakpointCallee L1W44JdiPopFrames
        L1W45JdiSuspendedInNative L1W45JdiEventModifiers L1W46JdiSpentCountFilter
        L1W47JdiBlockedBeforeAttach L1W47JdiReflectiveFrames
        L1W48JdiCompatibleBlockingBytecode L1W48JdiVirtualThreadFrames
        L1W49JdiVirtualContinuationFrames L1W49JdiSuspendCompletesBeforeReply
        L1W29RawJdwpEventRequestErrors L1W102JdiGroupChildren
        L1W102JdiUnpreparedClassesHidden L1W103RawJdwpCapabilities
        L1W103JdiUnpreparedClassMembers L1W103JdiMonitorInfo L1W103JdiInstances
        L1W103JdiMonitorEvents L1W104JdiRedefineClasses L1W104JdiHeapWalkUnsuspended
        L1W104JdiUnlinkedJdkClassHidden L1W104JdiDestroyJavaVmThreadEvents
        L6W105JdiMainHeldAtThreadDeath
        L1W105JdiThreadFilterOfEndedMain L1W105JdiUnlinkedJdkClassMembers
        L1W106JdiUnlinkedJdkClassLoadedBeforeAttach L1W106JdiThreadHeldAtThreadDeath
        L1W110JdiStopMonitorsStress)
    # Interpreter round i5 waves 110-111 (lane L1): L1W110JdiStopMonitorsStress
    # repeats the owned-monitor and MonitorInfo reads under collections (the
    # i110-L1 owned-monitors race; MonitorInfo's entry waiters); its header
    # was measured on HotSpot 25 by the orchestrator (WSL, wave 110).
    # Interpreter round i5 wave 106 (lane L1): a JDK class loaded unlinked
    # BEFORE the attach and never used is hidden too (the i102-L1 residue);
    # an ordinary thread's ThreadDeath with SUSPEND_EVENT_THREAD holds it
    # before its joiners wake (the hold audit after L6's wave-105 finding).
    # L1W105JdiClassPrepareBeforeAck is NOT listed: HotSpot 25 itself answers
    # "19 of 20" in some runs (orchestrator, Linux w105c, 2026-10-16) -- JDI
    # builds a ClassPrepare's reference type on its own event thread, so a reply
    # that arrives right after the event can still beat it. Run it by hand with
    # --scenario for statistics only; L1W102JdiUnpreparedClassesHidden is the row.
    # Interpreter round i5 wave 105 (lane L1): a ClassPrepare reaches JDI
    # before the reply to a read of state reached after it (i102-L1, the
    # intermittent row), and a ThreadOnly filter naming main matches none of
    # DestroyJavaVM's events (i101-L6 residue); L1W105JdiUnlinkedJdkClassMembers
    # the members of a JDK class loaded unlinked (i103-L1).
    # Interpreter round i5 wave 105 (lane L6): main's ThreadDeath with
    # SUSPEND_EVENT_THREAD holds main before its join wake-up.
    # Interpreter round i5 wave 104 (lane L1): RedefineClasses
    # (canRedefineClasses), heap walks with nothing suspended and from the
    # roots, a JDK class loaded unlinked hidden, and main's ThreadDeath /
    # DestroyJavaVM's ThreadStart under their own ids.
    # Interpreter round i5 wave 103 (lane L1): the last five prove the
    # i100-L1 event-filter page, the i102-L1 thread-list and unprepared-class
    # pages, the i100-L1 capabilities page and the i103-L1 members of an
    # unprepared class; L1W103JdiMonitorInfo, L1W103JdiInstances and
    # L1W103JdiMonitorEvents the capabilities granted in wave 103 (i103-L1
    # capabilities page). The C JVMTI shim probes have their own runner,
    # tools/jvmti/run-jvmti-shims.sh.
    # L1W27JdiStandInMethodEvents is left out: it matches HotSpot under
    # --jdk-only only (run it with --modes "jdk-only jdk-only:nojit"); under
    # --compatible StringBuilder.setLength keeps its stand-in native and posts
    # no events (i27-L1 page, "What remains" item 5).
fi
[ -n "$jdk" ] || { echo "--jdk is required" >&2; exit 2; }
java="$jdk/bin/java"
javac="$jdk/bin/javac"
[ -x "$java" ] || [ -x "$java.exe" ] || { echo "no java under $jdk/bin" >&2; exit 2; }
if [ "$check_headers" -eq 0 ]; then
    [ -n "$cratonvm" ] || { echo "--cratonvm is required (or --check-headers)" >&2; exit 2; }
    [ -x "$cratonvm" ] || { echo "not an executable: $cratonvm" >&2; exit 2; }
fi

# The class directory is named `out`, as in the probes' headers: a transcript
# may name it (`VirtualMachine.ClassPaths`).
mkdir -p "$out/out"
for s in "${scenarios[@]}"; do
    "$javac" -g -d "$out/out" "$(probe_file "$s")" || { echo "javac failed for $s" >&2; exit 2; }
done

# The transcript a probe's header records: the indented comment lines after
# "HotSpot 25... as the debuggee", up to the next prose comment line. Prose
# lines before the first indented one continue the heading (interpreter
# round i1 wave 45: a heading wrapped onto a second line, as in
# L1W44JdiPopFrames' header, yielded an empty transcript).
header_transcript() {
    awk '
        /^\/\/ HotSpot 25.* as the debuggee/ { on = 1; next }
        on && /^\/\/   / { started = 1; sub(/^\/\/   /, ""); print; next }
        on && /^\/\/$/ { next }
        on && !started { next }
        on { exit }
    ' "$(probe_file "$1")"
}

# The HotSpot debuggee's extra JVM flags for scenario $1: the probe header's
# `// HotSpot debuggee flags: ...` line, if any (interpreter round i1 wave
# 48: the virtual-thread scenarios compare against HotSpot without
# continuations, `-XX:-VMContinuations`, which is what this VM's
# `BoundVirtualThread`s are). Word-split by the caller.
hotspot_flags() {
    sed -n 's|^// HotSpot debuggee flags: ||p' "$(probe_file "$1")" | head -n 1 | tr -d '\r'
}

# Run scenario $1 with the debuggee command "${@:2}" and write the
# debugger's transcript to stdout. The debuggee's own output goes to
# $out/<scenario>.<tag>.debuggee.txt ($tag from the environment).
run_session() {
    local scenario="$1"
    shift
    "$@" > "$out/$scenario.$tag.debuggee.txt" 2>&1 &
    local debuggee=$!
    timeout "$debugger_timeout" "$java" -cp "$out/out" "$scenario" debug "$port" \
        | tr -d '\r'
    local status=${PIPESTATUS[0]}
    # The debuggee ends with the session (every scenario runs it to its end
    # or exits it); do not leave one behind if the debugger failed.
    for _ in $(seq 1 100); do
        kill -0 "$debuggee" 2>/dev/null || break
        sleep 0.1
    done
    kill "$debuggee" 2>/dev/null
    wait "$debuggee" 2>/dev/null
    port=$((port + 1))
    return "$status"
}

# Diff lines of $1 (reference) against $2, minus the allow-list's entries for
# scenario $3 and mode $4. Prints the remaining changed lines; empty = pass.
#
# Interpreter round i5 wave 103 (lane L1): both sides are compared with
# carriage returns dropped and every run of blanks read as one space, so an
# entry matches however many spaces the transcript line starts with (an
# entry for "-   canGetMonitorInfo=1" was not applied on the w103 host run).
normalize_line() {
    tr -d '\r' | sed 's/[[:space:]][[:space:]]*/ /g; s/ $//'
}

unexpected_differences() {
    local allowed
    allowed="$(tr -d '\r' < "$allow" 2>/dev/null | awk -F'\t' -v s="$3" -v m="$4" '
        /^#/ || NF < 4 { next }
        ($1 == s || $1 == "*") && ($2 == m || $2 == "*") { print $3 }
    ' | normalize_line)"
    diff "$1" "$2" | grep -E '^[<>] ' | sed 's/^< /- /; s/^> /+ /' \
        | while IFS= read -r line; do
            key="$(printf '%s\n' "$line" | normalize_line)"
            if ! printf '%s\n' "$allowed" | grep -qxF -- "$key"; then
                printf '%s\n' "$line"
            fi
        done
}

failed=0
for s in "${scenarios[@]}"; do
    tag=hotspot
    # shellcheck disable=SC2046 # the header's flags are words
    run_session "$s" "$java" $(hotspot_flags "$s") ${debuggee_flags[@]+"${debuggee_flags[@]}"} \
        "-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=$port" \
        -cp "$out/out" "$s" wait > "$out/$s.hotspot.txt"
    if [ "$check_headers" -eq 1 ]; then
        header_transcript "$s" > "$out/$s.header.txt"
        if diff -u "$out/$s.header.txt" "$out/$s.hotspot.txt" > "$out/$s.header.diff"; then
            echo "PASS $s: the header matches HotSpot"
        else
            echo "FAIL $s: the header differs from HotSpot ($out/$s.header.diff)"
            failed=1
        fi
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
        tag="$mode"
        run_session "$s" "$cratonvm" --java-home "$jdk" ${flags[@]+"${flags[@]}"} \
            ${debuggee_flags[@]+"${debuggee_flags[@]}"} --jdwp-port "$port" \
            -cp "$out/out" "$s" wait > "$out/$s.$mode.txt"
        diff -u "$out/$s.hotspot.txt" "$out/$s.$mode.txt" > "$out/$s.$mode.diff"
        left="$(unexpected_differences "$out/$s.hotspot.txt" "$out/$s.$mode.txt" "$s" "$mode")"
        if [ -z "$left" ]; then
            echo "PASS $s [$mode]"
        else
            echo "FAIL $s [$mode] ($out/$s.$mode.diff):"
            printf '%s\n' "$left" | sed 's/^/    /'
            failed=1
        fi
    done
done
exit "$failed"
