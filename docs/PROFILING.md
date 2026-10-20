# Performance Profiling Guide

How to measure and improve CratonVM performance.

## Running Benchmarks

### QuickBench (historical standard suite)

`bench/` is currently ignored and not checked in. To reproduce the README
snapshot, restore the historical benchmark sources into ignored scratch space:

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

# Compare with HotSpot JDK
java -cp .bench-cache/quickbench QuickBench
```

QuickBench includes Arithmetic (300M ops), Fibonacci(42), Sieve of Eratosthenes
(100K * 500), and Matrix multiplication (500x500). Binary Trees is a separate
class in the same scratch directory.

### Custom benchmarks

Write a Java class with `System.currentTimeMillis()` timing:

```java
public class MyBench {
    public static void main(String[] args) {
        long start = System.currentTimeMillis();
        // ... workload ...
        System.out.println("Time: " + (System.currentTimeMillis() - start) + " ms");
    }
}
```

## JIT Compiler Tuning

| Setting | Default | Description |
|---------|---------|-------------|
| JIT threshold | 500 invocations | Methods compiled after this many calls (`CRATONVM_JIT_THRESHOLD`). |
| OSR | On unless `CRATONVM_JIT_OSR=0` | Enables On-Stack Replacement for hot loop back-edges. |
| SIMD | AVX2 (auto-detected) | Vector operations for reduction loops. |

## Profiling with System Tools

### Linux (perf)

```bash
# Record CPU profile
cargo build --release -p cratonvm-cli
perf record -g ./target/release/cratonvm --classpath .bench-cache/quickbench QuickBench
perf report

# Generate flamegraph
perf script | stackcollapse-perf.pl | flamegraph.pl > flamegraph.svg
```

### Windows (ETW)

Use Windows Performance Recorder (WPR) or Tracy profiler with the release binary.

### Rust-specific profiling

```bash
# Cargo flamegraph (install: cargo install flamegraph)
cargo flamegraph --release -p cratonvm-cli -- --classpath .bench-cache/quickbench QuickBench

# Criterion benchmarks
cargo bench
```

## Interpreting Results

- **Ratio < 1.0x**: CratonVM is faster than HotSpot.
- **Ratio = 1.0x**: Performance parity with HotSpot.
- **Ratio > 1.0x**: CratonVM is slower by that factor.

An older snapshot vs JDK 25.0.1 C2 on Windows 11, taken before back-edge OSR
became the default: default QuickBench is 46.2x slower; with
`CRATONVM_JIT_OSR=1 CRATONVM_JIT_THRESHOLD=1`,
QuickBench is 8.25x slower overall, with Arithmetic/Sieve/Matrix near
1.30x-1.55x and Fibonacci still 13.8x slower. See the README and the mdBook
benchmark page for the full table.

## Tips for Reproducible Results

1. Use `--release` builds only (debug builds are 10-50x slower).
2. Run benchmarks multiple times and take the median.
3. Disable CPU turbo boost for consistent results.
4. Close other applications to reduce noise.
5. Use `--test-threads=1` for single-threaded benchmarks.
