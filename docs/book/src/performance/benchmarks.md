# Benchmarks

CratonVM is benchmarked against HotSpot's C2 compiler (JDK 25). These are
representative single-run snapshots from the historical **QuickBench**
micro-suite plus the allocation-heavy **Binary Trees** workload. Numbers move
with hardware, OS load, JDK version, and VM configuration.

## QuickBench vs. HotSpot JDK 25 C2

*Historical pre-2026-07-04-OSR-default-flip snapshot measured on Windows 11
against JDK 25.0.1 C2 and CratonVM release builds.
QuickBench rows are from CratonVM code `b80c50b5` on 2026-07-02. The Binary
Trees CratonVM columns were rechecked on 2026-07-03 at `8292ec9c`, using the
same 681 ms HotSpot baseline from the 2026-07-02 JDK run. Ratio = CratonVM time
/ HotSpot time (lower is better; 1.00x is parity). The old default column used
a launcher default where OSR was disabled. Current `dev` enables OSR by default;
set `CRATONVM_JIT_OSR=0` to reproduce the old OSR-off lane. The OSR column sets
`CRATONVM_JIT_OSR=1` and `CRATONVM_JIT_THRESHOLD=1`.*

| Benchmark                 | JDK 25 C2    | CratonVM default | Default ratio | CratonVM OSR, threshold=1 | OSR ratio |
|---------------------------|--------------|------------------|---------------|---------------------------|-----------|
| Arithmetic (300M ops)     | 991 ms       | 73,820 ms        | 74.5x         | 1,534 ms                  | 1.55x     |
| Fibonacci(42), recursive  | 2,071 ms     | 28,969 ms        | 14.0x         | 28,525 ms                 | 13.8x     |
| Sieve (100K x 500 reps)   | 358 ms       | 31,665 ms        | 88.4x         | 466 ms                    | 1.30x     |
| Matrix 500x500 multiply   | 336 ms       | 39,078 ms        | 116.3x        | 452 ms                    | 1.35x     |
| **QuickBench TOTAL**      | **3,756 ms** | **173,532 ms**   | **46.2x**     | **30,977 ms**             | **8.25x** |
| Binary Trees (depth = 18) | 681 ms       | 36,525 ms        | 53.6x         | 36,983 ms                 | 54.3x     |

**Reading the results:**

- In this historical snapshot, default launcher settings left these one-shot hot
  loops mostly interpreted, so the default QuickBench total was not competitive.
- With OSR enabled and the invocation threshold lowered, Arithmetic, Sieve, and
  Matrix were close to HotSpot C2. Recursive Fibonacci remains a call-heavy JIT
  gap, and Binary Trees remains dominated by allocation/GC throughput.
- Closing the Fibonacci and Binary Trees gaps is a tracked roadmap item; see
  [Roadmap](../contributing/roadmap.md).

## Running the benchmarks yourself

```bash
# Build the release binary first.
cargo build --release -p cratonvm-cli

# Restore the historical benchmark sources into ignored scratch space.
mkdir -p .bench-cache/quickbench
git show 2cea208:bench/QuickBench.java > .bench-cache/quickbench/QuickBench.java
git show 2cea208:bench/binarytrees.java > .bench-cache/quickbench/binarytrees.java
javac -d .bench-cache/quickbench .bench-cache/quickbench/QuickBench.java .bench-cache/quickbench/binarytrees.java

# CratonVM default.
target/release/cratonvm --classpath .bench-cache/quickbench QuickBench

# CratonVM with the default OSR path and a low threshold.
CRATONVM_JIT_THRESHOLD=1 target/release/cratonvm --classpath .bench-cache/quickbench QuickBench

# HotSpot for comparison.
java -cp .bench-cache/quickbench QuickBench
```

For larger workloads, give CratonVM more heap (for example `--Xmx 8g` for
Binary Trees) and use `--nojit` to capture interpreter-only timings.

The VM Criterion gate reads committed baselines from `vm/bench/`. A missing or
malformed baseline is a gate error; the checked-in zero-valued baselines are
only placeholders and are reported as bootstrap entries until a deliberate
baseline refresh records real medians. Generated Criterion output under
`vm/target_bench/` is historical local data, not the package or gate baseline.

## Caveats & methodology

Benchmark numbers move with hardware, OS, JDK version, and workload. To get
reproducible figures:

1. **Use `--release` builds only** - debug builds are 10-50x slower.
2. Run multiple times and take the **median**.
3. Disable CPU turbo boost for stable timing.
4. Close other applications to reduce noise.
5. Use single-threaded settings for single-threaded benchmarks.

See [Profiling](profiling.md) for measurement tooling.
