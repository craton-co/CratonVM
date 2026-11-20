# Profiling

How to measure CratonVM performance and find hotspots.

## Running maintained benchmarks

Use `tools/bench/run-published-ci.py --help` and the
[benchmark guide](../../../../BENCHMARK.md) for current harnesses and prerequisites.

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

Use `--nojit` for an interpreter control. Baseline compilation, background
optimizing compilation, and loop OSR have different admission paths; see the
[JIT reference](../../../JIT_OPTIMIZATION.md) for their defaults. SIMD emission
depends on the host CPU and OS. Compile the maintained fixture before profiling:

```bash
javac -d .bench-cache/cpu tools/bench/CratonBench.java
```

## System profilers

### Linux (perf)

```bash
cargo build --release -p cratonvm-cli
perf record -g ./target/release/cratonvm -Xmx8g -XX:+UseG1GC -cp .bench-cache/cpu CratonBench matrix
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
cargo flamegraph --release -p cratonvm-cli -- -Xmx8g -XX:+UseG1GC -cp .bench-cache/cpu CratonBench matrix
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
