# Performance Profiling Guide

How to measure and improve CratonVM performance.

## Running Benchmarks

### QuickBench (standard suite)

```bash
# CratonVM
cargo run --release -p cratonvm-cli -- --classpath bench QuickBench

# Compare with HotSpot JDK
java -cp bench QuickBench
```

QuickBench includes: Arithmetic (300M ops), Fibonacci(42), Sieve of Eratosthenes
(100K * 500), and Matrix multiplication (500x500).

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
| JIT threshold | 100 invocations | Methods compiled after this many calls |
| OSR threshold | 10,000 back-edges | Loops compiled via On-Stack Replacement |
| SIMD | AVX2 (auto-detected) | Vector operations for reduction loops |

## Profiling with System Tools

### Linux (perf)

```bash
# Record CPU profile
cargo build --release -p cratonvm-cli
perf record -g ./target/release/cratonvm --classpath bench QuickBench
perf report

# Generate flamegraph
perf script | stackcollapse-perf.pl | flamegraph.pl > flamegraph.svg
```

### Windows (ETW)

Use Windows Performance Recorder (WPR) or Tracy profiler with the release binary.

### Rust-specific profiling

```bash
# Cargo flamegraph (install: cargo install flamegraph)
cargo flamegraph --release -p cratonvm-cli -- --classpath bench QuickBench

# Criterion benchmarks (if available)
cargo bench
```

## Interpreting Results

- **Ratio < 1.0x**: CratonVM is faster than HotSpot
- **Ratio = 1.0x**: Performance parity with HotSpot
- **Ratio > 1.0x**: CratonVM is slower by that factor

Current performance: ~1.41x overall vs HotSpot JDK 25 C2 (Round 26).
Fibonacci is within 7% of C2.

## Tips for Reproducible Results

1. Use `--release` builds only (debug builds are 10-50x slower)
2. Run benchmarks multiple times and take the median
3. Disable CPU turbo boost for consistent results
4. Close other applications to reduce noise
5. Use `--test-threads=1` for single-threaded benchmarks
