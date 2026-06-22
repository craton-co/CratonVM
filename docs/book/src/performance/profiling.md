# Profiling

How to measure CratonVM performance and find hotspots.

## Running the standard suite

```bash
# CratonVM
cargo run --release -p cratonvm-cli -- --classpath bench QuickBench

# HotSpot for comparison
java -cp bench QuickBench
```

QuickBench covers Arithmetic (300M ops), Fibonacci(42), the Sieve of
Eratosthenes (100K × 500), and Matrix multiplication (500×500). See
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
invocations) — or lower it with `CRATONVM_JIT_THRESHOLD=1` for short runs.

## JIT knobs that affect timing

| Setting | Effect |
|---------|--------|
| `--nojit` | Interpreter-only — use to measure the interpreter or isolate JIT effects. |
| `CRATONVM_JIT_THRESHOLD=<n>` | When methods become JIT-eligible (default 500). |
| AVX2 SIMD | Auto-detected via CPUID; affects data-parallel reduction loops. |

On-Stack Replacement compiles hot loops mid-method, so long-running loops get
optimized even if the enclosing method is never re-entered.

## System profilers

### Linux (perf)

```bash
cargo build --release -p cratonvm-cli
perf record -g ./target/release/cratonvm --classpath bench QuickBench
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
cargo flamegraph --release -p cratonvm-cli -- --classpath bench QuickBench
```

## Java Flight Recorder

CratonVM includes a JFR implementation for event-based, in-VM profiling and
diagnostics — useful for understanding allocation, GC, and method activity from
the Java side.

## Reproducible results

1. **`--release` builds only** — debug is 10–50× slower.
2. Run multiple times; take the **median**.
3. Disable CPU turbo boost.
4. Close other applications.
5. Use single-threaded settings for single-threaded benchmarks.

**Interpreting ratios:** `< 1.0×` means CratonVM is faster than HotSpot, `1.0×`
is parity, `> 1.0×` is slower by that factor.
