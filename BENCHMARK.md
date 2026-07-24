# CratonVM Benchmarks

Methodology, full notes, and provenance for the numbers in
[README.md](README.md). The headline rule for every table we publish:
**checksums must match HotSpot on every run** — a fast wrong answer is a
bug, not a result.

## CPU benchmarks

### Harness

All seven CPU rows come from one unified harness,
[`bench/CratonBench.java`](bench/CratonBench.java): arithmetic, fib, sieve,
matrix, hashmap, stringregex, and bintrees, each runnable in-process or as
an isolated phase (`CratonBench <phase>`). Each phase prints its time **and
its checksum**. The legacy single-purpose harnesses
(`QuickBenchLong2.java`, `HashMapOnly.java`, `StringRegexOnly.java`,
`BinTreesClassic.java`) remain in `bench/` for historical comparability.

### Methodology

- Both VMs run the **same flags** (notably `-Xmx8g` for Binary Trees).
- Fresh process per measurement, alternating HotSpot / CratonVM runs.
- Pinned to one logical CPU with `taskset` on the benchmark host
  (Azure Linux, EPYC 9V45, SMT).
- Medians over 5–7 runs; **no samples discarded**.
- Reference JDK: Temurin JDK 25.0.3 (C2).
- Measurements are taken in quiet windows (1-minute load average below 2);
  the shared host's load otherwise inflates both columns — and CratonVM's
  memory-heavy rows more — so ratios, not absolute times, are the durable
  content across host re-provisionings.

### Current table (measured 2026-07-18, Binary Trees re-validated 2026-07-24)

| Benchmark                         | JDK 25 C2 | CratonVM  | Ratio |
|-----------------------------------|-----------|-----------|-------|
| Arithmetic (2B ops)               | 2,006 ms  | 4,895 ms  | 2.44x |
| Fibonacci(44)                     | 1,719 ms  | 4,790 ms  | 2.79x |
| Sieve (100K × 20,000)             | 2,851 ms  | 6,508 ms  | 2.28x |
| Matrix 1280×1280                  | 2,349 ms  | 6,875 ms  | 2.93x |
| HashMap (10M put/get, isolated)   | 1,471 ms  | 5,488 ms  | 3.73x |
| String/Regex (100K, isolated)     | 54 ms     | 193 ms    | 3.57x |
| Binary Trees (depth 18, isolated) | 176 ms    | 1,468 ms  | 8.34x |

Row notes:

- **Fibonacci** is recursion-bound; the recursive self-call already
  compiles to a guarded direct call, and the remaining gap is register
  allocation and recursion inlining, which the current backends do not do.
- **Binary Trees** was measured at `-Xmx8g` as seven alternating
  fresh-process pairs; all fourteen checksums were `68332206`. The full
  optimization story is
  [docs/internal/performance/binarytrees-half-gap-20260718.md](docs/internal/performance/binarytrees-half-gap-20260718.md).
  A July 2026 dev regression that temporarily quadrupled this row was
  root-caused (two independent causes) and fixed — see
  [docs/internal/performance/bt18-inline-tlab-regression-20260724.md](docs/internal/performance/bt18-inline-tlab-regression-20260724.md).
  The perf gate's anchored baseline is 1,550 ms, reflecting a deliberate
  ~2% correctness hardening (explicit header initialization in the inline
  allocator) accepted after that fix.

### The performance gate

Perf regressions are guarded by a mandatory gate:
[`regression-suite/perf/run-cratonbench-gate.sh`](regression-suite/perf/run-cratonbench-gate.sh)
runs every phase isolated (pinned, `-Xmx8g`, median of 5), verifies the
exact checksum on every run, enforces a 5% budget over per-host anchored
baselines, and **refuses to measure on a loaded host** rather than produce
noisy verdicts. Anchored baselines may only be re-anchored with a linked
evidence document.

### Optimization history

The journey from the earliest 20–100x gaps to the current table is
documented round by round:

- [docs/JIT_OPTIMIZATION.md](docs/JIT_OPTIMIZATION.md) — the full 26-round JIT journey
- [docs/internal/performance/halfgap-residuals-20260718.md](docs/internal/performance/halfgap-residuals-20260718.md)
- [docs/internal/performance/halfgap-20260717.md](docs/internal/performance/halfgap-20260717.md)
- [docs/internal/performance/hashmap-sieve-half-gap-20260714.md](docs/internal/performance/hashmap-sieve-half-gap-20260714.md)
- [docs/internal/performance/quickbench-half-gap-3rows-20260713.md](docs/internal/performance/quickbench-half-gap-3rows-20260713.md)
- [docs/internal/performance/string-regex-overallocated-groups-fastpath-20260714.md](docs/internal/performance/string-regex-overallocated-groups-fastpath-20260714.md)

## GPU benchmarks

### Setup

Measured 2026-07-11 on a GeForce RTX 2060 (sm_75) against HotSpot JDK 25
(C2) and TornadoVM 4.0.1 (PTX backend, `@Parallel`/`@Reduce` + TaskGraph
API). CratonVM offload is **transparent**: plain static methods over
primitive arrays, no annotations, no API
(`cargo build --features gpu-driver`, run with `--gpu`). All timings are
warm and include the full per-call H2D + kernel + D2H round-trip.

| Kernel (N = 2²⁴)                         | HotSpot C2 | TornadoVM GPU | CratonVM GPU | vs HotSpot | vs TornadoVM |
|------------------------------------------|------------|---------------|--------------|------------|--------------|
| Integer div-chain (48 divs/elem)         | 1,910 ms   | 28 ms         | **9 ms**     | **212x**   | **3.1x**     |
| Double div-chain (64 divs/elem)          | 1,508 ms   | 129 ms        | **91 ms**    | **16.6x**  | **1.4x**     |
| 96 multiply-adds/elem (AVX2 on CPU)      | 8 ms       | 17 ms         | 11 ms        | 0.7x       | 1.5x         |
| Dot-product reduction (int·int → long)   | 7 ms       | unimplemented | 18 ms        | 0.4x       | n/a          |

Notes:

- The div-chain rows are the "GPU wins big" cases: division has no
  competitive CPU-vectorized form, so raw parallelism wins at every size
  tested (2²⁰–2²⁶; the ratios hold steady across sizes).
- CratonVM's double-division checksum is **bit-exact** with HotSpot at
  every size (`div.rn.f64` is IEEE-754 round-to-nearest, same as x86
  `vdivpd`); TornadoVM's diverges slightly — its PTX backend doesn't
  guarantee bit-exact division.
- TornadoVM 4.0.1 throws `TornadoInternalError: unimplemented` on the
  equivalent `@Reduce`-over-`LongArray` kernel; CratonVM's transparent
  reduction handles it (slowly — a proper tree/shared-memory reduction is
  an open item).
- The multiply-add and dot-product rows are kept as honest counter-cases:
  CPU AVX2 stays competitive on MAD-dominated kernels at every size, and a
  single atomic accumulator doesn't get relatively cheaper with more
  elements.

Full GPU results — more input sizes, `ldc`-constant kernels, cold-start
numbers up to N = 2²⁸, kernel sources, and eligibility rules — are in
[docs/gpu/README.md](docs/gpu/README.md).

## Reproducing

```bash
# CPU, all phases in-process:
cargo build --release -p cratonvm-cli
javac -d bench-classes bench/CratonBench.java
./target/release/cratonvm -Xmx8g -cp bench-classes CratonBench

# One isolated phase (the per-row methodology):
./target/release/cratonvm -Xmx8g -cp bench-classes CratonBench bintrees

# The gated regression check (Linux bench host):
bash regression-suite/perf/run-cratonbench-gate.sh -Exe /abs/path/to/cratonvm
```
