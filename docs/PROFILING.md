# Performance Profiling Guide

How to measure and improve CratonVM performance.

## Running benchmarks

Use the maintained CPU workloads in `tools/bench/CratonBench.java`. Compile the
fixture once, then run each runtime in a fresh process with the same heap,
collector, and workload:

```bash
cargo build --release -p cratonvm-cli
javac -d .bench-cache/cpu tools/bench/CratonBench.java
./target/release/cratonvm -Xmx8g -XX:+UseG1GC -cp .bench-cache/cpu CratonBench matrix
java -Xmx8g -XX:+UseG1GC -cp .bench-cache/cpu CratonBench matrix
```

Select `arithmetic`, `fib`, `sieve`, `matrix`, `matrix-tiled`, `hashmap`,
`stringregex`, or `bintrees`. An all-phase run shares JIT and GC state across
workloads and is not equivalent to running each phase in a separate process.
Check output checksums before comparing timings. Use
[the published harness](../tools/bench/PUBLISHED-CI.md) for interleaved repeated
measurements, and [BENCHMARK.md](../BENCHMARK.md) for the result tables.

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

## JIT execution paths

Use `--nojit` for an interpreter control. Baseline compilation, background
recompilation, optimizing-tier admission, and backedge OSR are different paths;
record which path is active when attributing a timing change. The defaults and
coverage are documented in [the JIT reference](JIT_OPTIMIZATION.md).
SIMD emission also depends on the instruction set exposed by the host and OS.

## Profiling with System Tools

### Linux (perf)

```bash
# Record CPU profile
cargo build --release -p cratonvm-cli
perf record -g ./target/release/cratonvm -Xmx8g -XX:+UseG1GC -cp .bench-cache/cpu CratonBench matrix
perf report

# Generate flamegraph
perf script | stackcollapse-perf.pl | flamegraph.pl > flamegraph.svg
```

### Windows (ETW)

Use Windows Performance Recorder (WPR) or Tracy profiler with the release binary.

### Rust-specific profiling

```bash
# Cargo flamegraph (install: cargo install flamegraph)
cargo flamegraph --release -p cratonvm-cli -- -Xmx8g -XX:+UseG1GC -cp .bench-cache/cpu CratonBench matrix

# Criterion benchmarks
cargo bench
```

## Interpreting Results

- **Ratio < 1.0x**: CratonVM is faster than HotSpot.
- **Ratio = 1.0x**: Performance parity with HotSpot.
- **Ratio > 1.0x**: CratonVM is slower by that factor.

## Tips for Reproducible Results

1. Use `--release` builds only (debug builds are 10-50x slower).
2. Run benchmarks multiple times and take the median.
3. Disable CPU turbo boost for consistent results.
4. Close other applications to reduce noise.
5. Match the workload thread count across runtimes.
