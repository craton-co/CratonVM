# Profiling

How to measure CratonVM performance and find hotspots.

## Running the historical QuickBench suite

`bench/` is currently ignored and not checked in. To reproduce the benchmark
snapshot used in the README, restore the historical sources into ignored scratch
space:

```bash
cargo build --release -p cratonvm-cli

mkdir -p .bench-cache/quickbench
git show 2cea208:bench/QuickBench.java > .bench-cache/quickbench/QuickBench.java
git show 2cea208:bench/binarytrees.java > .bench-cache/quickbench/binarytrees.java
javac -d .bench-cache/quickbench .bench-cache/quickbench/QuickBench.java .bench-cache/quickbench/binarytrees.java

# CratonVM default
target/release/cratonvm --classpath .bench-cache/quickbench QuickBench

# CratonVM with the default OSR path and a low threshold
CRATONVM_JIT_THRESHOLD=1 target/release/cratonvm --classpath .bench-cache/quickbench QuickBench

# HotSpot for comparison
java -cp .bench-cache/quickbench QuickBench
```

QuickBench covers Arithmetic (300M ops), Fibonacci(42), the Sieve of
Eratosthenes (100K x 500), and Matrix multiplication (500x500). See
[Benchmarks](benchmarks.md) for representative results.

## Writing a custom benchmark

```java
public class MyBench {
    public static void main(String[] args) {
        long start = System.currentTimeMillis();
        // ... workload ...
        System.out.println("Time: " + (System.currentTimeMillis() - start) + " ms");
    }
}
```

Warm the code up enough that hot methods cross the JIT threshold (default 500
invocations), lower it with `CRATONVM_JIT_THRESHOLD=1` for short runs, and set
`CRATONVM_JIT_OSR=0` when you specifically want to disable hot-loop OSR during
diagnosis.

## JIT knobs that affect timing

| Setting | Effect |
|---------|--------|
| `--nojit` | Interpreter-only; use to measure the interpreter or isolate JIT effects. |
| `CRATONVM_JIT_THRESHOLD=<n>` | When methods become JIT-eligible (default 500). |
| `CRATONVM_JIT_OSR=0` | Disables On-Stack Replacement for hot loop back-edges. |
| AVX2 SIMD | Auto-detected via CPUID; affects data-parallel reduction loops. |

## System profilers

### Linux (perf)

```bash
cargo build --release -p cratonvm-cli
perf record -g ./target/release/cratonvm --classpath .bench-cache/quickbench QuickBench
perf report

# Flame graph
perf script | stackcollapse-perf.pl | flamegraph.pl > flamegraph.svg
```

### Windows

Use Windows Performance Recorder (WPR) or the Tracy profiler against the release
binary.

### Rust tooling

```bash
# cargo flamegraph (install: cargo install flamegraph)
cargo flamegraph --release -p cratonvm-cli -- --classpath .bench-cache/quickbench QuickBench
```

## Java Flight Recorder

CratonVM includes a JFR implementation for event-based, in-VM profiling and
diagnostics, useful for understanding allocation, GC, and method activity from
the Java side.

## Reproducible results

1. **`--release` builds only** - debug is 10-50x slower.
2. Run multiple times; take the **median**.
3. Disable CPU turbo boost.
4. Close other applications.
5. Use single-threaded settings for single-threaded benchmarks.

**Interpreting ratios:** `< 1.0x` means CratonVM is faster than HotSpot, `1.0x`
is parity, `> 1.0x` is slower by that factor.
