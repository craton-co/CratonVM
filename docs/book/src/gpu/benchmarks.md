# GPU Offload Benchmarks

Measured on a GeForce RTX 2060 (sm_75, 12 GiB, driver 591.86,
CUDA 13.1), Intel hybrid 24C/32T host. Compared against
[TornadoVM](https://github.com/beehive-lab/TornadoVM) 4.0.1-jdk25 (PTX
backend, explicit `@Parallel` + TaskGraph API) and HotSpot JDK 25.0.1 (C2).
This is the first systematic validation pass of the GPU offload launch path
on real hardware — see [Status & follow-ups](overview.md#status--follow-ups)
for what it confirmed.

**Methodology.** CratonVM and TornadoVM timings are warm best-of (5
repetitions), HotSpot is best-of-3, CratonVM CPU is best-of-2, unless a table
says otherwise. Every GPU timing is the *full* per-call round trip — host→device
copy, kernel launch, device→host copy — not just kernel execution. Every row's
checksum or result was cross-checked against HotSpot and matched bit-for-bit
(shown inline where the source result files include them). Raw output lives in
`bench-gpu/results/*-20260711.md`; the same numbers are summarized in the
repository README's "GPU offload benchmarks" section.

Box caveat: measurements were taken on a machine carrying roughly 10-30%
background CPU load from unrelated processes (including a known cryptominer
infection on this host, tracked separately). The GPU is otherwise idle during
these runs, so GPU-side timings are unaffected; CPU baselines (CratonVM CPU
and HotSpot) are pessimistic by roughly that margin. That margin doesn't
change the orders of magnitude below.

## Integer division chain — the GPU's best case

`GpuDivChain.divChain`: 48 serial, data-dependent integer divisions per
element (`x = x / b[i] + 12345`, divisor from a second array). x86 has no SIMD
integer divide, and the divisor isn't a compile-time constant, so HotSpot C2
can't vectorize or strength-reduce any of it — every CPU division costs
roughly 20-30 cycles, paid serially, 48 times per element. The GPU emulates
`idiv` too, but spreads the work across tens of thousands of threads.

| N | CratonVM CPU (JIT) | HotSpot CPU (C2) | CratonVM GPU (`--gpu`) | TornadoVM GPU | GPU vs HotSpot | GPU vs CratonVM CPU |
|---|---|---|---|---|---|---|
| 2²² | 569 ms | 470 ms | **2 ms** | 7 ms | 235× | 285× |
| 2²⁴ | 2,232 ms | 1,910 ms | **9 ms** | 28 ms | 212× | 248× |
| 2²⁶ | 9,162 ms | 6,735 ms | **33 ms** | 86 ms | 204× | 278× |

That's roughly **210× over HotSpot C2** and **~3× over TornadoVM's own PTX
backend** on the same kernel, averaged across sizes — this is the shape where
CPU vectorization genuinely can't help and the GPU's raw thread count wins
outright. Checksums (`DIV_CHECKSUM`) matched exactly across all three VMs at
every N: 2²² = 58915440413, 2²⁴ = 246467335469, 2²⁶ = 952311409739.

Source: `bench-gpu/results/divchain-comparison-20260711.md`.

## 96 multiply-adds per element — the honest hard case

`GpuWarm.heavy`: 96 chained integer multiply-adds per element. Unlike the
division chain, this *is* a shape HotSpot C2 can auto-vectorize with AVX2 —
so it's the fairer test of whether the GPU earns its keep once the CPU JIT is
allowed to use SIMD.

| N | CratonVM CPU | HotSpot CPU (C2, vectorized) | CratonVM GPU | TornadoVM GPU | GPU vs CratonVM CPU | GPU vs HotSpot |
|---|---|---|---|---|---|---|
| 2²² | 472 ms | 2 ms | **1 ms** | 5 ms | 472× | 2.0× |
| 2²⁴ | 1,955 ms | 8 ms | **11 ms** | 17 ms | 178× | 0.7× |
| 2²⁶ | 7,689 ms | 25 ms | **27 ms** | 51 ms | 285× | 0.9× |

Against vectorized HotSpot the GPU roughly **ties** — ahead at the smallest
size, slightly behind at the larger two, all within the same order of
magnitude. Against TornadoVM's PTX backend on the identical kernel, CratonVM's
GPU path is consistently faster, by **~2×**. Against CratonVM's own
interpreter/JIT CPU path (which has no auto-vectorizer for this shape), the
GPU wins by **178×-472×**. All three CratonVM/HotSpot checksums matched
exactly at every N (e.g. n=67108864: `cv-cpu=1857856255 cv-gpu=1857856255
hotspot=1857856255`).

Source: `bench-gpu/results/warm-comparison-20260711.md`.

## Cold start, and the largest size tested

The two tables above are warm numbers — the kernel is already compiled to PTX
and resident on the device. `bench-gpu/results/gpu-comparison-20260711.md`
instead measures CratonVM's GPU path on its *first* call to `GpuCompute.heavy`
(96 MADs/element, the same shape as above), which folds in PTX compilation and
buffer allocation, up to N = 2²⁸ (269M elements):

| N | CratonVM CPU | CratonVM GPU (cold) | HotSpot CPU | TornadoVM GPU (warm) | GPU vs CratonVM CPU | GPU vs HotSpot | TornadoVM vs HotSpot |
|---|---|---|---|---|---|---|---|
| 2²⁰ (1.1M) | 144 ms | 12 ms | 14 ms | 2 ms | 12.0× | 1.2× | 7.0× |
| 2²² (4.2M) | 482 ms | 11 ms | 15 ms | 4 ms | 43.8× | 1.4× | 3.8× |
| 2²⁴ (17M) | 1,992 ms | 31 ms | 20 ms | 15 ms | 64.3× | 0.6× | 1.3× |
| 2²⁶ (68M) | 7,782 ms | 101 ms | 38 ms | 67 ms | 77.0× | 0.4× | 0.6× |
| 2²⁸ (269M) | 30,805 ms | 579 ms | 123 ms | 207 ms | 53.2× | 0.2× | 0.6× |

("GPU vs HotSpot" and "TornadoVM vs HotSpot" are the source file's own
`CV-GPU speedup`/`TVM speedup` columns — both are HotSpot-CPU-relative, not
relative to each other.) Two things stand out. First, against CratonVM's own
CPU path the *cold* GPU call — PTX compile, buffer allocation, and all — is
still 12×-77× faster; offload pays for itself even without amortizing
compile cost across repeated calls. Second, against HotSpot C2 (and against
TornadoVM, which is warm here since its own methodology runs a warmup call
before measuring) CratonVM's cold GPU path starts ahead at small N and falls
behind at larger N, because the fixed one-time PTX compilation cost is a
proportionally bigger slice of a small run and gets amortized away on a
bigger one — this is a cold-start artifact, not a reflection of the warm
numbers above. The 2²⁸ row exists mainly to confirm the offload path doesn't
crash or regress at the largest size tested.

The companion memory-bound kernel in the same results file,
`GpuProbe.vaddMap` (`out[i] = a[i] + b[i]`), reports only CratonVM/HotSpot
checksums plus TornadoVM's timing — CratonVM's own GPU timing isn't broken
out separately in that table, but every checksum matches across all N from
2²⁰ to 2²⁸ (e.g. 2²⁸: `CS:36028930968244920` identical on CratonVM, HotSpot,
and TornadoVM).

Source: `bench-gpu/results/gpu-comparison-20260711.md`.

## When the GPU does not win

Two honest caveats fall out of the tables above:

- **If the CPU can auto-vectorize the kernel, the GPU's edge shrinks or
  disappears.** The div-chain kernel (no vectorization possible) sees the GPU
  win by two orders of magnitude; the MAD-chain kernel (AVX2-vectorizable by
  HotSpot C2) sees the GPU roughly tie or trail HotSpot at larger sizes. If a
  hot loop is already vectorizer-friendly, offloading it may buy little over
  a good CPU JIT.
- **Memory-bound kernels pay for the PCIe round trip.** Every offloaded call
  pays full host→device and device→host copies, not just kernel execution.
  For a kernel that does almost no compute per byte moved (`vaddMap` above is
  the example: one add per two loads and a store), that copy cost can
  dominate, especially at small-to-medium N where the fixed launch and
  transfer overhead isn't amortized over enough work. Compute-bound kernels
  (division chains, multi-step MAD chains) amortize that overhead far better
  because the GPU does much more work per byte transferred.

`--gpu-min-work` (default `4096`, see [CLI flags](overview.md#cli-flags-only-under---features-gpu))
exists specifically to skip offload below a work-size threshold for this
reason. The dispatcher sizes the launch from the true runtime array length
rather than a fixed placeholder — see
[`docs/gpu/launch-work-sizing.md`](https://github.com/craton-co/cratonvm/blob/dev/docs/gpu/launch-work-sizing.md).
