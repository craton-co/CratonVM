# Benchmarks

CratonVM is benchmarked against HotSpot's C2 compiler (JDK 25). These are
representative single-run snapshots from the historical **QuickBench**
micro-suite plus the allocation-heavy **Binary Trees** workload. Numbers move
with hardware, OS load, JDK version, and VM configuration.

## QuickBench vs. HotSpot JDK 25 C2

*Best-of-N snapshot on a shared Azure Linux build host (16
cores, sustained load average 10-16 from concurrent sessions) against JDK
25.0.3 Temurin C2 and a CratonVM release build off `dev` at `bfc26c2d`. N=10
samples for JDK 25, N=7 for CratonVM (one run of 50,516 ms excluded as a
host-contention outlier) — best-of-N rather than a single run because
run-to-run variance on this shared host was 2-4x. Ratio = CratonVM time /
HotSpot time (lower is better; 1.00x is parity). `dev` enables back-edge OSR
by default, so this column already includes OSR; forcing
`CRATONVM_JIT_THRESHOLD=1` on top no longer showed a distinct benefit in this
snapshot, so that tuned column was dropped. Set `CRATONVM_JIT_OSR=0` to
reproduce the old OSR-off lane.*

| Benchmark                 | JDK 25 C2    | CratonVM default | Default ratio |
|---------------------------|--------------|------------------|---------------|
| Arithmetic (300M ops)     | 343 ms       | 692 ms           | 2.0x          |
| Fibonacci(42), recursive  | 603 ms       | 2,944 ms         | 4.9x          |
| Sieve (100K x 500 reps)   | 70 ms        | 349 ms           | 5.0x          |
| Matrix 500x500 multiply   | 161 ms       | 370 ms           | 2.3x          |
| **QuickBench TOTAL**      | **1,177 ms** | **4,355 ms**     | **3.7x**      |
| Binary Trees (depth = 18) | 347 ms       | 8,214 ms         | 23.7x         |

**Reading the results:**

- Now that back-edge OSR defaults on, forcing `CRATONVM_JIT_THRESHOLD=1` on
  top no longer buys a distinct advantage over the plain default — that
  tuning only mattered previously, when OSR itself was opt-in.
- Arithmetic, Sieve, and Matrix now run within ~2-5x of HotSpot C2. Recursive
  Fibonacci and Binary Trees remain the largest gaps — Fibonacci from
  call-heavy JIT dispatch overhead, Binary Trees from allocation/GC
  throughput.
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
