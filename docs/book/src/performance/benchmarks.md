# Benchmarks

CratonVM is benchmarked against HotSpot's C2 compiler (OpenJDK 25). These are
representative results from the **QuickBench** micro-suite plus the
allocation-heavy **Binary Trees** workload. Numbers are illustrative of the
JIT's standing, not a guarantee — see the caveats below.

## QuickBench vs. HotSpot JDK 25 C2

*Measured on Windows 11 against OpenJDK 25 C2. Ratio = CratonVM time ÷ HotSpot
time (lower is better; 1.00× is parity).*

| Benchmark | JDK 25 C2 | CratonVM | Ratio |
|-----------|-----------|----------|-------|
| Arithmetic (300M ops) | 889 ms | 1,676 ms | 1.89× |
| Fibonacci(42), recursive | 1,876 ms | 2,457 ms | 1.31× |
| Sieve (100K × 500 reps) | 324 ms | 510 ms | 1.57× |
| Matrix 500×500 multiply | 351 ms | 518 ms | 1.48× |
| **QuickBench TOTAL** | **3,440 ms** | **5,161 ms** | **1.50×** |
| Binary Trees (depth = 18) | 714 ms | 16,657 ms | 23.3× |

**Reading the results:**

- On compute-bound code, CratonVM's JIT lands within **~1.3×–1.9×** of HotSpot
  C2, and **~1.5× overall** on QuickBench. Fibonacci — recursive call overhead —
  is within ~7% of C2.
- **Binary Trees is the outlier.** It is dominated by short-lived allocation and
  garbage collection, and CratonVM is currently far behind there (~23×).
  Closing this gap through GC-throughput work is a tracked roadmap item — see
  [Roadmap](../contributing/roadmap.md).

For context, the same JIT is roughly **28× faster than the HotSpot
interpreter** (`-Xint`) on these workloads.

## Running the benchmarks yourself

```bash
# Build the release binary first
cargo build --release -p cratonvm-cli

# CratonVM
cargo run --release -p cratonvm-cli -- --classpath bench QuickBench

# HotSpot for comparison
java -cp bench QuickBench
```

For larger workloads, give CratonVM more heap (e.g. `--Xmx 8g` for Binary
Trees), and use `--nojit` to capture interpreter-only timings.

## Caveats & methodology

Benchmark numbers move with hardware, OS, JDK version, and workload. To get
reproducible figures:

1. **Use `--release` builds only** — debug builds are 10–50× slower.
2. Run multiple times and take the **median**.
3. Disable CPU turbo boost for stable timing.
4. Close other applications to reduce noise.
5. Use single-threaded settings for single-threaded benchmarks.

See [Profiling](profiling.md) for measurement tooling, and [How the JIT Got
Fast](jit-internals.md) for the engineering behind these numbers.
