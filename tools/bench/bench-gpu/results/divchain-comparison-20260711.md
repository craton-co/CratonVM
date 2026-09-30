# Div-chain — GPU vs best-CPU concept prover (2026-07-11)

**Kernel:** `GpuDivChain.divChain` — 48 serial, data-dependent integer divisions
per element (`x = x / b[i] + 12345`, divisor from a second array). x86 has no
SIMD integer division and the divisor is not a compile-time constant, so
HotSpot C2 cannot vectorize or strength-reduce it — every CPU pays ~20-30
cycles per division, serially. The GPU emulates idiv too, but across tens of
thousands of parallel threads.

**Timing:** warm best-of (5 reps for the GPU rows, 3 for HotSpot, 2 for
CratonVM CPU), full per-call H2D + kernel + D2H round-trip included for GPU rows.
**Hardware:** RTX 2060 (sm_75, 12 GiB), driver 591.86 / CUDA 13.1;
Intel hybrid 24C/32T host. TornadoVM 4.0.1-jdk25 PTX backend; HotSpot = JDK 25.0.1.

| N | CratonVM CPU (JIT) | HotSpot CPU (C2) | CratonVM GPU (`--gpu`) | TornadoVM GPU | CV-GPU vs HotSpot | CV-GPU vs CV-CPU |
|---|---|---|---|---|---|---|
| 2^22 | 569 ms | 470 ms | **2 ms** | 7 ms | 235× | 285× |
| 2^24 | 2,232 ms | 1,910 ms | **9 ms** | 28 ms | 212× | 248× |
| 2^26 | 9,162 ms | 6,735 ms | **33 ms** | 86 ms | 204× | 278× |

Checksums (`DIV_CHECKSUM`): 2^22 = 58915440413, 2^24 = 246467335469,
2^26 = 952311409739 — identical across every VM measured.

## Box-load caveat

Measured with ~10-30% background CPU load from unrelated processes (including
a known cryptominer infection on this host — see session notes). GPU timings
are unaffected (GPU otherwise idle); CPU baselines are pessimistic by roughly
that margin, which does not change the orders of magnitude here.
