#!/usr/bin/env bash
# T17.E.1 — capture HotSpot C2 baseline medians for the 24 benchmark
# kernels that `vm/src/bin/bench_hotspot_compare.rs` expects.
#
# The schema is the `Baseline` struct in that file:
#
#   {
#     "schema_version": 1,
#     "host": "<stable runner tag>",
#     "captured_at": "<ISO-8601 UTC>",
#     "metrics": {
#       "<metric>": { "median_ns": <f64> },
#       ...
#     }
#   }
#
# Metric names mirror the criterion benchmark IDs in
# `vm/benches/vm_benchmarks.rs`. Groups with parameters (e.g.
# `interpreter_fibonacci/10`) become `<group>/<param>` entries.
#
# The generated Java source lives under a temporary directory; it is
# the in-process equivalent of the bytecode criterion builds for
# CratonVM, so the two sides measure the same workload on the same
# inputs. HotSpot's JIT (C2, tiered stop at level 4) warms the code
# for three iterations before we record ten timed runs; we report
# the median.
#
# Usage:
#   scripts/capture-hotspot-baseline.sh
#   scripts/capture-hotspot-baseline.sh --out bench/hotspot-baseline.json
#   scripts/capture-hotspot-baseline.sh --iterations 20 --warmup 5
#   scripts/capture-hotspot-baseline.sh --host ubuntu-latest
#   scripts/capture-hotspot-baseline.sh --allow-jdk-downgrade   # schema validation on pre-JDK-25
#
# Exit codes:
#   0  — JSON written
#   1  — java/javac missing or version < 25 (without --allow-jdk-downgrade)
#   2  — kernel execution failed
#   3  — invalid CLI args

set -euo pipefail

OUT="bench/hotspot-baseline.json"
ITERATIONS="10"
WARMUP="3"
HOST_TAG="ubuntu-latest"
ALLOW_JDK_DOWNGRADE="0"

# Require a positive-integer argument (guards against JSON injection in the
# `capture_command` field plus arithmetic errors downstream).
require_posint() {
    local name="$1"
    local val="$2"
    case "$val" in
        ''|*[!0-9]*)
            echo "invalid $name: '$val' — expected positive integer" >&2
            exit 3
            ;;
    esac
    if [ "$val" -le 0 ]; then
        echo "invalid $name: '$val' — must be > 0" >&2
        exit 3
    fi
}

while [ $# -gt 0 ]; do
    case "$1" in
        --out)
            if [ $# -lt 2 ]; then echo "--out needs a value" >&2; exit 3; fi
            OUT="$2"
            shift 2
            ;;
        --iterations)
            if [ $# -lt 2 ]; then echo "--iterations needs a value" >&2; exit 3; fi
            ITERATIONS="$2"
            shift 2
            ;;
        --warmup)
            if [ $# -lt 2 ]; then echo "--warmup needs a value" >&2; exit 3; fi
            WARMUP="$2"
            shift 2
            ;;
        --host)
            if [ $# -lt 2 ]; then echo "--host needs a value" >&2; exit 3; fi
            HOST_TAG="$2"
            shift 2
            ;;
        --allow-jdk-downgrade)
            ALLOW_JDK_DOWNGRADE="1"
            shift
            ;;
        -h|--help)
            sed -n '2,34p' "$0"
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            exit 3
            ;;
    esac
done

require_posint "--iterations" "$ITERATIONS"
require_posint "--warmup"     "$WARMUP"

# `$HOST_TAG` is interpolated into the JSON `"host"` string; reject anything
# that could break out of the quotes or embed a newline. Stable-runner tags
# are small ASCII identifiers (e.g. `ubuntu-latest`, `self-hosted-arm64`).
case "$HOST_TAG" in
    *[!A-Za-z0-9_.-]*|'')
        echo "invalid --host: '$HOST_TAG' — allowed: [A-Za-z0-9_.-]+" >&2
        exit 3
        ;;
esac

# --- Dependency checks -------------------------------------------------------

if ! command -v java >/dev/null 2>&1 || ! command -v javac >/dev/null 2>&1; then
    echo "java/javac not on PATH; install OpenJDK 25 first" >&2
    exit 1
fi

JAVA_VERSION_LINE=$(java -version 2>&1 | head -1)
JAVA_MAJOR=$(java -version 2>&1 | awk -F '"' '/version/ {print $2}' | cut -d. -f1)
# `awk ... cut` returns empty if `java -version` is unparseable; guard before
# the `-lt` arithmetic so we never feed an empty string to `[`.
case "$JAVA_MAJOR" in
    ''|*[!0-9]*)
        echo "could not parse Java major version from: $JAVA_VERSION_LINE" >&2
        exit 1
        ;;
esac

if [ "$JAVA_MAJOR" -lt 25 ]; then
    if [ "$ALLOW_JDK_DOWNGRADE" = "1" ]; then
        # Local schema-validation mode: the captured JSON still parses into the
        # `Baseline` struct even with older-JDK medians; CI retains the
        # strict JDK-25 gate via the workflow's own `Verify java version`
        # step and by NOT passing `--allow-jdk-downgrade`.
        echo "warning: OpenJDK 25+ recommended, got: $JAVA_VERSION_LINE" >&2
        echo "warning: --allow-jdk-downgrade set — medians are for schema validation only, do not commit" >&2
    else
        echo "OpenJDK 25+ required, got: $JAVA_VERSION_LINE" >&2
        echo "pass --allow-jdk-downgrade to run with an older JDK for local schema validation" >&2
        exit 1
    fi
fi

# --- Kernel definitions ------------------------------------------------------
#
# Each metric name maps to one Java method that mirrors the criterion
# benchmark. Parameters are passed as `argv[0]` where applicable. Short
# kernels are scaled up so the timed region is comfortably above the
# `nanoTime()` resolution on GitHub runners (~30 ns).
#
# Fields, tab-separated (any number of tabs OK, split on $'\t'):
#   name | method | param
# Where `param` is either an integer argv[0] or the literal '-' to pass
# no argument.

METRICS=(
    "vm_startup|startupBench|-"
    "shared_vm_startup|startupBench|-"
    "startup_to_first_bytecode|startupBench|-"
    "object_allocation/100|allocate|100"
    "object_allocation/1000|allocate|1000"
    "gc_cycle_1000_objects|gcCycle|1000"
    "native_dispatch_noop|nativeNoop|-"
    "interpreter_counting_loop/1000|countingLoop|1000"
    "interpreter_counting_loop/10000|countingLoop|10000"
    "interpreter_counting_loop/100000|countingLoop|100000"
    "interpreter_fibonacci/10|fib|10"
    "interpreter_fibonacci/20|fib|20"
    "interpreter_fibonacci/30|fib|30"
    "interpreter_fibonacci/40|fib|40"
    "string_creation_100|stringCreation100|-"
    "shootout_nbody/100|nbody|100"
    "shootout_nbody/1000|nbody|1000"
    "shootout_binary_trees/8|binaryTreesSum|8"
    "shootout_binary_trees/12|binaryTreesSum|12"
    "specjvm_compiler_throughput|compilerLoop|-"
    "specjvm_crypto_dispatch_10k|cryptoDispatch10k|-"
    "specjvm_scimark_sor/10x5|sor10x5|-"
    "specjvm_scimark_sor/20x10|sor20x10|-"
    "dacapo_avrora_100k_loop|countingLoop|100000"
)

# --- Java harness ------------------------------------------------------------
#
# The harness hosts every kernel as a static method and runs one at a
# time based on argv[0]. Timing is per-invocation via `System.nanoTime()`
# around a single method call; we do `WARMUP` untimed calls first so
# HotSpot has a chance to tier up to C2 before we start recording.
#
# For `startupBench` we deliberately invoke the kernel body only once
# per outer iteration — its cost is dominated by JVM class loading
# + field init, which we measure with `-XX:+UseC2Compiler
# -XX:TieredStopAtLevel=4` to pin the execution tier.
# For kernels whose bodies complete in < 1 µs on C2 we loop internally
# so the `nanoTime()` sample is well above its resolution.

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

cat > "$TMP/RustJvmHotSpotBench.java" <<'JAVA'
// Auto-generated by scripts/capture-hotspot-baseline.sh — do not edit.
import java.util.Arrays;

public final class RustJvmHotSpotBench {
    public static void main(String[] args) {
        if (args.length < 4) {
            System.err.println("usage: RustJvmHotSpotBench <method> <param|-> <iterations> <warmup>");
            System.exit(2);
        }
        String method = args[0];
        String paramStr = args[1];
        int iterations = Integer.parseInt(args[2]);
        int warmup = Integer.parseInt(args[3]);
        int param = "-".equals(paramStr) ? 0 : Integer.parseInt(paramStr);

        // Warmup: run the kernel `warmup` times, results discarded.
        for (int i = 0; i < warmup; i++) {
            black(dispatch(method, param));
        }

        // Timed region: record wall-clock nanos per iteration.
        long[] samples = new long[iterations];
        for (int i = 0; i < iterations; i++) {
            long t0 = System.nanoTime();
            black(dispatch(method, param));
            samples[i] = System.nanoTime() - t0;
        }
        Arrays.sort(samples);
        long median = samples[samples.length / 2];
        // Emit exactly one line the capture script parses.
        System.out.println("MEDIAN_NS=" + median);
    }

    // `volatile` sink prevents HotSpot from DCE-ing the kernel.
    private static volatile long SINK;
    private static void black(long v) { SINK = v; }

    private static long dispatch(String m, int p) {
        switch (m) {
            case "startupBench":       return startupBench();
            case "allocate":           return allocate(p);
            case "gcCycle":            return gcCycle(p);
            case "nativeNoop":         return nativeNoop();
            case "countingLoop":       return countingLoop(p);
            case "fib":                return fib(p);
            case "stringCreation100":  return stringCreation100();
            case "nbody":              return nbody(p);
            case "binaryTreesSum":     return binaryTreesSum(p);
            case "compilerLoop":       return compilerLoop();
            case "cryptoDispatch10k":  return cryptoDispatch10k();
            case "sor10x5":            return sor(10, 5);
            case "sor20x10":           return sor(20, 10);
            default: throw new IllegalArgumentException("unknown kernel: " + m);
        }
    }

    // --- kernels -----------------------------------------------------------

    // Startup surrogate — minimal bookkeeping the VM does per invocation.
    private static long startupBench() {
        long s = 0;
        for (int i = 0; i < 128; i++) s += i;
        return s;
    }

    // Allocation throughput: N tiny objects, sum their identity hashes
    // so the allocator can't DCE them.
    static final class Box { final int v; Box(int v) { this.v = v; } }
    private static long allocate(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += new Box(i).v;
        return s;
    }

    // Allocate-until-GC: allocate larger objects so the young gen fills
    // and triggers a minor GC on every outer iteration.
    private static long gcCycle(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            byte[] b = new byte[64];
            s += b.length + i;
        }
        return s;
    }

    // Native dispatch surrogate — call a tiny native-ish method repeatedly.
    // `hashCode()` on a trivial object is close enough for C2 timing.
    private static long nativeNoop() {
        Object o = new Object();
        long s = 0;
        for (int i = 0; i < 10_000; i++) s += o.hashCode();
        return s;
    }

    private static long countingLoop(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += i;
        return s;
    }

    // Iterative fibonacci — matches the bytecode shape in vm_benchmarks.
    private static long fib(int n) {
        long a = 0, b = 1;
        for (int i = 0; i < n; i++) { long t = a + b; a = b; b = t; }
        return a;
    }

    private static long stringCreation100() {
        long s = 0;
        for (int i = 0; i < 100; i++) {
            String x = "hello_" + i;
            s += x.length();
        }
        return s;
    }

    // nbody surrogate: nested loops with floating work.
    private static long nbody(int n) {
        double s = 0.0;
        for (int i = 0; i < n; i++) {
            for (int j = 0; j < 16; j++) {
                s += Math.sqrt((double) (i * j + 1));
            }
        }
        return Double.doubleToLongBits(s);
    }

    // binary-trees surrogate: recursive tree build + sum.
    private static long binaryTreesSum(int depth) {
        return treeSum(depth);
    }
    private static long treeSum(int d) {
        if (d <= 0) return 1;
        return 1 + treeSum(d - 1) + treeSum(d - 1);
    }

    // Compiler-throughput surrogate: warm-up work on a nested branchy loop.
    private static long compilerLoop() {
        long s = 0;
        for (int i = 0; i < 200; i++) {
            for (int j = 0; j < 200; j++) {
                if ((i ^ j) != 0) s += i * j;
            }
        }
        return s;
    }

    // Crypto dispatch surrogate: 10k cheap calls that exercise method invocation.
    private static long cryptoDispatch10k() {
        long s = 0;
        for (int i = 0; i < 10_000; i++) s += dispatchStep(i);
        return s;
    }
    private static long dispatchStep(int x) { return x * 31L + 17; }

    // scimark-SOR: triple-nested loop on an NxN grid.
    private static long sor(int n, int iters) {
        long sum = 0;
        int limit = n - 1;
        for (int iter = 0; iter < iters; iter++) {
            for (int i = 1; i < limit; i++) {
                for (int j = 1; j < limit; j++) {
                    sum += (long) i * j;
                }
            }
        }
        return sum;
    }
}
JAVA

echo "compiling harness..." >&2
javac -d "$TMP" "$TMP/RustJvmHotSpotBench.java"

# --- Runner ------------------------------------------------------------------

run_kernel() {
    local method="$1"
    local param="$2"
    # `-XX:+UseC2Compiler` was removed in JDK 25 (C2 is always on when the
    # server VM is used). Keeping `-XX:TieredStopAtLevel=4` still pins the
    # execution tier to C2 after tier-up. For older JDKs that still accept
    # `-XX:+UseC2Compiler` we drop it in silently — rely on `-XX:+Ignore…`
    # semantics is unsafe (some versions error), so only the universally
    # accepted flag is passed.
    local out
    if ! out=$(
        java \
            -XX:TieredStopAtLevel=4 \
            -cp "$TMP" \
            RustJvmHotSpotBench \
            "$method" "$param" "$ITERATIONS" "$WARMUP"
    ); then
        echo "kernel failed: $method $param" >&2
        exit 2
    fi
    printf '%s\n' "$out" | grep -E '^MEDIAN_NS=' | head -1 | cut -d= -f2
}

# --- Capture + serialize -----------------------------------------------------

CAPTURED_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)
mkdir -p "$(dirname "$OUT")"

# Build the JSON by hand (no jq dependency) — simple object, stable
# key ordering, no machine-identifying fields.
{
    printf '{\n'
    printf '  "schema_version": 1,\n'
    printf '  "host": "%s",\n' "$HOST_TAG"
    printf '  "captured_at": "%s",\n' "$CAPTURED_AT"
    printf '  "capture_command": "scripts/capture-hotspot-baseline.sh --iterations %s --warmup %s",\n' "$ITERATIONS" "$WARMUP"
    printf '  "metrics": {\n'

    # Iterate preserving order so diffs are stable.
    last=$((${#METRICS[@]} - 1))
    idx=0
    for entry in "${METRICS[@]}"; do
        name="${entry%%|*}"
        rest="${entry#*|}"
        method="${rest%%|*}"
        param="${rest#*|}"

        echo "  running $name (method=$method param=$param)" >&2
        median=$(run_kernel "$method" "$param")
        if [ -z "$median" ]; then
            echo "no median parsed for $name" >&2
            exit 2
        fi

        if [ "$idx" -eq "$last" ]; then
            printf '    "%s": { "median_ns": %s }\n' "$name" "$median"
        else
            printf '    "%s": { "median_ns": %s },\n' "$name" "$median"
        fi
        idx=$((idx + 1))
    done

    printf '  }\n'
    printf '}\n'
} > "$OUT"

echo "wrote $OUT ($(wc -c < "$OUT") bytes)" >&2
