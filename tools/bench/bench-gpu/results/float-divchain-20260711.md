# Float (double) division chain — a genuine floating-point GPU win (2026-07-11)

**Kernel:** `GpuFloatDivChain.divChain` — 64 sequential double-precision
divisions per element (`x = x / d + C`, divisor from a second array,
numerically stable fixed-point iteration, no overflow/underflow risk).
Unlike the "96 multiply-adds" kernel (where HotSpot's AVX2 auto-vectorizer
is fully competitive with the GPU), floating-point division is
throughput-limited even under AVX2 — `vdivpd` is a shared, weakly-pipelined
execution unit, unlike multiply/add/FMA. The GPU amortizes this across
thousands of parallel threads instead of 2-4 AVX2 lanes.

**Timing:** warm best-of (5 reps GPU/HotSpot/TornadoVM, 2-3 reps CratonVM
CPU — the CPU path is slow enough at large N that more reps aren't
practical), full per-call H2D + kernel + D2H round-trip for GPU rows.
**Hardware:** RTX 2060 (sm_75, 12 GiB), driver 591.86 / CUDA 13.1; Intel
hybrid 24C/32T host. TornadoVM 4.0.1-jdk25 PTX backend; HotSpot = JDK 25.0.1.

| N | CratonVM CPU (JIT) | HotSpot CPU (C2) | CratonVM GPU (`--gpu`) | TornadoVM GPU | CV-GPU vs HotSpot | CV-GPU vs TornadoVM |
|---|---|---|---|---|---|---|
| 2²⁰ (1,048,576)  | 435 ms    | 89 ms    | **7 ms**   | 11 ms  | **12.7×** | 1.6× |
| 2²² (4,194,304)  | 1,856 ms  | 400 ms   | **23 ms**  | 38 ms  | **17.4×** | 1.65× |
| 2²⁴ (16,777,216) | 6,299 ms  | 1,508 ms | **91 ms**  | 129 ms | **16.6×** | 1.4× |
| 2²⁶ (67,108,864) | 36,082 ms | 5,578 ms | **365 ms** | 482 ms | **15.3×** | 1.3× |

CratonVM-CPU at 2²⁶ took ~99× longer than CratonVM-GPU (36.1s vs 365ms) to
produce the identical checksum — a concrete illustration of what the GPU
path buys on a genuinely compute-bound kernel.

**Correctness note (notable):** `FDIV_CHECKSUM` is bit-exact between
CratonVM-CPU, CratonVM-GPU, and HotSpot at **every** size tested, including
2²⁶ (`2.3711976971862224E8` on all three) — `div.rn.f64` PTX is IEEE-754
round-to-nearest, identical to x86's `vdivpd`. TornadoVM's checksum
diverges slightly from HotSpot's at every size (e.g. `2.3711976971862224E8`
vs `2.3833420668027386E8` at 2²⁶) — TornadoVM's PTX backend does not
guarantee bit-exact IEEE division. CratonVM's GPU path is exact where
TornadoVM's is approximate.

## Why this kernel (and not the other two) shows a clear win

Investigated whether the "96 multiply-adds" or "dot-product reduction"
kernels have a size where CratonVM-GPU beats HotSpot — they don't, for
different reasons, both confirmed on hardware this session:

- **96 multiply-adds/elem:** at N ≤ 2²² both HotSpot and CratonVM-GPU
  report 0-1 ms — sub-2ms timings are pure `System.nanoTime()`/millisecond-
  granularity noise, not a reproducible signal (an earlier "2× GPU win" data
  point at 2²² was this noise, not a real result). At N ≥ 2²⁴ the ratio
  settles to 0.7-0.9× (GPU loses) because HotSpot's AVX2 auto-vectorizer is
  genuinely competitive with the GPU on this shape, and the kernel is
  PCIe-transfer-bound on the GPU side (H2D+D2H of the full int arrays
  dominates the actual compute time). Scaling N further doesn't change the
  ratio — both sides scale ~linearly once past the noise floor.
- **Dot-product reduction:** ratio stays roughly constant (~3× slower on
  GPU) from N=2²² through N=2²⁶ (12 ms → 41 ms GPU vs 4 ms → 19 ms HotSpot
  at 2²⁴/2²⁶). The kernel is memory-bandwidth-bound on both sides (two int
  arrays in, one scalar out) and every GPU thread races an atomic add on the
  *same* accumulator cell — that contention doesn't improve with scale. A
  real win here would need a proper tree/shared-memory reduction instead of
  a single atomic cell (tracked as an open item).

The float div-chain kernel avoids both problems: division has no
competitive CPU-vectorized form (so no crossover-to-loss at large N like
96-MAD), and it has no reduction/accumulator contention (plain element-wise
map, like vector-add, just compute-heavier).

## Box-load caveat

Measured with ~10-30% background CPU load from unrelated processes on this
host (see session notes). GPU timings are largely unaffected (GPU
otherwise idle); CPU baselines are pessimistic by roughly that margin —
does not change the order-of-magnitude conclusions here.
