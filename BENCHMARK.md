# CratonVM Benchmarks

Methodology, full notes, and provenance for the numbers in
[README.md](README.md). The headline rule for every table we publish:
**checksums must match HotSpot on every run** — a fast wrong answer is a
bug, not a result.

## CPU benchmarks

### Environment and Hardware

All measurements were performed on the dedicated Azure Linux benchmark host with the following specification:
- **Processor**: AMD EPYC 9V45 96-Core Processor (Zen 4 architecture, family 26, model 2, stepping 1), 8 vCPUs assigned, SMT enabled (2 threads/core, 4 physical cores)
- **Caches**: L1d 192 KiB (4 instances), L1i 128 KiB (4 instances), L2 4 MiB (4 instances), L3 32 MiB shared
- **Host OS & Kernel**: Ubuntu 24.04.1 LTS, Linux kernel `7.0.0-1014-azure #14~24.04.1-Ubuntu SMP PREEMPT x86_64`
- **Reference JDK**: OpenJDK JDK 25 LTS (Temurin-25.0.4+7, build 25.0.4+7-LTS, C2 HotSpot Server VM)
- **CratonVM**: Release build tested under compatible mode (`--compatible`)

### Methodology

- **Isolated Processes**: Every measurement runs in a fresh, isolated process invocation per benchmark phase and iteration to eliminate heap pollution, code cache bias, and GC state leakage.
- **Interleaved Execution**: Runs are strictly interleaved across VM configurations (`cvm_zgc` → `cvm_g1` → `hs_zgc` → `hs_g1`) to ensure transient host conditions affect all configurations symmetrically.
- **Garbage Collectors Tested**:
  1. **ZGC**: Generational / low-latency concurrent ZGC (`-XX:+UseZGC`)
  2. **G1**: Multi-threaded balanced throughput/latency collector (`-XX:+UseG1GC`)
- **Memory Flags**: Fresh process per measurement with `-Xmx16g` for large heap allocations and `-Xmx8g` baseline.
- **100% Checksum Verification**: Output checksums are printed and strictly verified against HotSpot across every run and every size parameter. Zero mismatches permitted.
- **Metric**:
  $$\text{Ratio (best for CratonVM)} = \frac{\min(\text{CVM ZGC}, \text{CVM G1})}{\min(\text{HotSpot ZGC}, \text{HotSpot G1})}$$
  (Values $< 1.0\times$ indicate CratonVM is faster than HotSpot).

### Benchmark Matrix Results

#### 1. Arithmetic

Long integer arithmetic loop exercising arithmetic operations, shifts, and remainder calculation (`sum += i * 3 - i / 2 + i % 7`).

| Iteration | CVM + ZGC | HotSpot + ZGC | CVM + G1 | HotSpot + G1 | Ratio (best for CratonVM) |
|:----------|----------:|--------------:|---------:|-------------:|--------------------------:|
| **1B**    |  1,023 ms |       916 ms  | 1,023 ms |     916 ms   |                     1.12x |
| **1.5B**  |  1,566 ms |      1,377 ms | 1,531 ms |     1,381 ms |                     1.11x |
| **2B**    |  2,045 ms |      1,831 ms | 2,059 ms |     1,829 ms |                     1.12x |
| **2.4B**  |  2,455 ms |      2,222 ms | 2,451 ms |     2,193 ms |                     1.12x |
| **3B**    |  3,083 ms |      2,743 ms | 3,069 ms |     2,749 ms |                     1.12x |

*Scaling is near-perfect linear across both VMs; CratonVM tracks within 11–12% of HotSpot C2 throughout the sweep.*

#### 2. Fibonacci

Recursive Fibonacci calculation (`fib(n) = fib(n-1) + fib(n-2)`) testing method prologue/epilogue overhead, fast guarded recursive call dispatch, and call frame management.

| Iteration | CVM + ZGC | HotSpot + ZGC | CVM + G1 | HotSpot + G1 | Ratio (best for CratonVM) |
|:---|---:|---:|---:|---:|---:|
| **40** | 88 ms | 211 ms | 89 ms | 239 ms | **0.42x** |
| **42** | 228 ms | 554 ms | 228 ms | 566 ms | **0.41x** |
| **44** | 593 ms | 1,446 ms | 590 ms | 1,450 ms | **0.41x** |
| **46** | 1,552 ms | 3,784 ms | 1,549 ms | 4,252 ms | **0.41x** |
| **48** | 4,134 ms | 9,976 ms | 4,050 ms | 9,952 ms | **0.41x** |

*CratonVM is ~2.4x faster than HotSpot across all depths (0.41x–0.42x ratio), maintaining consistent speedup due to optimized recursion handling.*

#### 3. Sieve

Sieve of Eratosthenes prime generation across multiple repetitions over dynamically allocated `boolean[]` heaps, testing bounds-check elimination and array store performance.

| Iteration      | CVM + ZGC | HotSpot + ZGC | CVM + G1 | HotSpot + G1 |          Ratio |
|:---------------|----------:|--------------:|---------:|-------------:|---------------:|
| **50K × 10K**  |    648 ms |        468 ms |   641 ms |       461 ms |          1.39x |
| **50K × 20K**  |  1,269 ms |        962 ms | 1,271 ms |       856 ms |          1.48x |
| **100K × 20K** |  2,832 ms |      3,030 ms | 2,838 ms |     2,572 ms |          1.10x |
| **100K × 40K** |  5,608 ms |      5,978 ms | 5,496 ms |     3,416 ms |          1.61x |
| **100K × 60K** |  8,396 ms |      8,916 ms | 7,638 ms |     4,958 ms |          1.54x |

*At higher iteration counts, HotSpot G1 pulls ahead while CratonVM comfortably outperforms HotSpot under ZGC.*

#### 4. Matrix

Square matrix integer multiplication with allocation and triple-nested loops ($O(N^3)$), testing vectorisation, cache locality, and nested loop lowering.

| Iteration     | CVM + ZGC | HotSpot + ZGC |  CVM + G1 | HotSpot + G1 | Ratio (best for CratonVM) |
|:--------------|----------:|--------------:|----------:|-------------:|--------------------------:|
| **720x720**   |    383 ms |        384 ms |    377 ms |       374 ms |                     1.01x |
| **960x960**   |    871 ms |        869 ms |    887 ms |       873 ms |                     1.00x |
| **1280x1280** |  2,095 ms |      2,118 ms |  2,061 ms |     2,109 ms |                 **0.98x** |
| **1920x1920** |  8,574 ms |     10,526 ms |  8,089 ms |    10,526 ms |                 **0.77x** |
| **2560x2560** | 45,581 ms |     41,186 ms | 35,389 ms |    44,761 ms |                 **0.86x** |

*CratonVM beats HotSpot on medium and large matrix sizes under G1 (0.77x at 1920x1920, 0.86x at 2560x2560).*

#### 5. Binary Trees

Classic binary trees allocation, traversal, and deallocation benchmark testing high-rate object allocation, GC young generation collection, and pointer chasing.

| Iteration | CVM + ZGC | HotSpot + ZGC |   CVM + G1 | HotSpot + G1 |   Ratio  |
|:----------|----------:|--------------:|-----------:|-------------:|---------:|
| **16**    |    277 ms |        151 ms |     246 ms |       108 ms |    2.28x |
| **18**    |  1,267 ms |        853 ms |   1,200 ms |       373 ms |    3.22x |
| **20**    |  5,507 ms |      3,004 ms |   6,387 ms |     1,024 ms |    5.38x |
| **22**    | 30,888 ms |      8,275 ms |  32,575 ms |     4,249 ms |    7.27x |
| **24**    | N/A (OOM) |     22,896 ms | 111,818 ms |    17,411 ms |    6.42x |

*At depth 24 under ZGC, CratonVM encountered an OOM exit, whereas under G1 it completed successfully in 111,818 ms.*

#### 6. HashMap

Heavy insertion and retrieval workload on standard `java.util.HashMap` (`put` and `get`), testing object hashing, table resizing, reference manipulation, and boxed key access.

| Iteration | CVM + ZGC | HotSpot + ZGC |  CVM + G1 | HotSpot + G1 |   Ratio |
|:----------|----------:|--------------:|----------:|-------------:|--------:|
| **1M**    |    285 ms |         55 ms |    312 ms |        72 ms |   5.18x |
| **5M**    |  1,445 ms |        182 ms |  1,569 ms |       291 ms |   4.97x |
| **10M**   |  2,863 ms |        484 ms |  3,135 ms |       593 ms |   5.92x |
| **50M**   | 31,348 ms |      3,099 ms | 33,059 ms |     2,612 ms |  12.00x |
| **100M**  | 72,709 ms |      6,124 ms | 76,982 ms |     5,122 ms |  14.20x |

*ZGC exhibits lower overhead than G1 on large HashMap instances in CratonVM.*

#### 7. String / Regex

Pattern compilation and regex matching using `java.util.regex.Pattern` and `Matcher.find()`, testing string slice extraction, capture group allocations, and standard library bridge interactions.

| Iteration | CVM + ZGC | HotSpot + ZGC |  CVM + G1 | HotSpot + G1 | Ratio (best for CratonVM) |
|:----------|----------:|--------------:|----------:|-------------:|--------------------------:|
| **100K**  |    203 ms |         98 ms |    211 ms |       100 ms |                     2.07x |
| **500K**  |    825 ms |        189 ms |    878 ms |       196 ms |                     4.37x |
| **1M**    |  1,582 ms |        240 ms |  1,649 ms |       253 ms |                     6.59x |
| **5M**    |  7,612 ms |        559 ms |  8,067 ms |       596 ms |                    13.62x |
| **10M**   | 15,460 ms |        931 ms | 13,759 ms |       937 ms |                    14.78x |

Updated 2026-10-16: seven fresh processes per cell, alternating the before/after
binaries and HotSpot, pinned to CPU 6 with `-Xmx8g` and `--compatible`.
The host was shared (1-minute load 5.70–13.71); these are paired
measurements, not a quiet-host baseline. Against the freshly rebuilt pre-change
`dev`, the best-GC gap to HotSpot shrank **43.6–48.5%** over this sweep.
The maintained quiet-host gate refused this host; its anchored baseline is unchanged.
[Before/after medians, every sample, profiles, and correctness evidence](docs/internal/performance/regex-gap-20261016.md).

*Runtime is approximately linear in the number of matches. The optimization removes
repeated native transitions, layout lookups and capture-buffer allocation; each
extracted group still receives a fresh Java String and backing array.*

Perf regressions are guarded by
[`regression-suite/perf/run-cratonbench-gate.sh`](regression-suite/perf/run-cratonbench-gate.sh),
which runs every phase isolated (pinned, `-Xmx8g`, median of 7), verifies the
exact checksum on every run, enforces a 5% budget over the per-host anchored
baselines in
[`cratonbench-baseline-azure-epyc.tsv`](regression-suite/perf/cratonbench-baseline-azure-epyc.tsv),
and **refuses to measure on a loaded host** rather than produce noisy verdicts.
Anchored baselines may only be re-anchored with a linked evidence document.

This gate was absent from `dev` for a period —
if you are working on a commit that predates the restore, it will not be there.

Even with the gate available, use the process below for a perf-affecting change
whenever you cannot get a quiet host — the gate deliberately refuses to measure
under load, and the shared Azure box frequently runs at load 5–8 with several
sessions building:

1. Build the pre-change commit and the post-change commit as **two
   uniquely-named binaries on the same host**.
2. Run the relevant per-kernel harness from the table above against both,
   alternating, several runs each.
3. **Compare checksums first.** A changed checksum means the change is wrong;
   a faster wrong answer is a bug, not a result.
4. Report absolute times for both binaries and the host's load average, since
   this host is shared and contended — ratios between your two binaries are
   meaningful, absolute numbers across hosts and dates are not.
5. `perf record -g --call-graph=dwarf -F 199` on both, and report the symbol
   deltas, not just wall clock.

### Optimization history

The journey from the earliest 20–100x gaps to the current table is
documented round by round:

- [docs/JIT_OPTIMIZATION.md](docs/JIT_OPTIMIZATION.md) — the current JIT
  architecture and the optimizations that produced the table above

## GPU benchmarks

### Setup

Measured on a GeForce RTX
2060 (sm_75, driver 591.86) against HotSpot JDK 25.0.3 (C2) and TornadoVM
4.0.1 (PTX backend, `@Parallel`/`@Reduce` + TaskGraph API). CratonVM offload
is **opt-in and automatic within a narrow envelope**: the kernels below are
plain static methods over primitive arrays with no annotations and no API at
the call site, but they require a GPU build and an explicit flag (`cargo
build --features gpu-driver`, run with `--gpu`), and anything the analyzer
doesn't accept stays on the CPU. All timings are warm and include the full
per-call H2D + kernel + D2H round-trip. There is no self-hosted GPU hardware
CI, so these are point-in-time measurements rather than a continuously
enforced budget.

| Kernel (N = 2²⁴)                                    | HotSpot C2 | TornadoVM GPU | CratonVM GPU | vs HotSpot | vs TornadoVM |
|-------------------------------------------------------|------------|---------------|--------------|------------|--------------|
| Integer div-chain (48 divs/elem)                       | 2,179 ms   | 27 ms         | **7 ms**     | **311x**   | **3.9x**     |
| Double div-chain (64 divs/elem)                        | 1,784 ms   | 135 ms        | **82 ms**    | **21.8x**  | **1.6x**     |
| 128 multiply-adds/elem (data-dependent multiplier)     | 1,298 ms   | 26 ms         | **7 ms**     | **185x**   | **3.7x**     |
| Dot-product reduction (int·int → long, x300/elem)      | 1,168 ms   | unimplemented | **2 ms**     | **584x**   | n/a          |

Ray tracer rows are 6 interleaved rounds each (arm order alternated per round
to cancel drift), full H2D+kernel+D2H, checksums bit-identical to HotSpot; the
3840×2160 row is pooled over two independent 6-round passes (12 rounds total,
craton faster in all 12) run 30 minutes apart, which agreed within noise. The
kernel is the reduced proxy (`bench-gpu/RayTracerKernel.java`), documented in
`raytracer-vs-tornadovm-RESOLVED-20260821.md` in the internal tree,
not the full `apps/TornadoVM-Ray-Tracer` app (whose real kernel — reflections,
soft shadows, a skybox — needs dynamic-length scene loops neither engine's
analyzer admits yet). The margin over TornadoVM shrinks with resolution
(2.5x → 2.2x → 2.0x → 1.85x, the last at 11520×6480 / 74.6M pixels, 6/6
rounds) as CratonVM's fixed per-launch cost advantage amortises away,
leaving a smaller but still consistent per-pixel-throughput edge. Every
frame from 160×120 to 11520×6480 is bit-identical to HotSpot's.

Re-verified 2026-09-05 on the same box (RTX 2060, TornadoVM 4.0.1 PTX,
N = 2²⁴). The four non-ray-tracer rows still hold and TornadoVM's integer
div-chain reproduced exactly at 26 ms. Two things that run did change:

- The **dot-product row is now 2 ms**, not 12 — the warp-shuffle reduction
  (one `red.global.add` per warp rather than per thread) landed after the
  original measurement.
- The **double div-chain row had silently stopped reproducing**: it measured
  9,276 ms, slower than HotSpot, because `runtime::offload_jit_gate` asked the
  constant-pool-*free* analyzer, which rejects `ldc2_w` unconditionally, while
  the dispatcher it gates for asks the pool-aware one. That kernel's
  `+ 1.0000001` is an `ldc2_w`; its integer twin's `+ 12345` is a `sipush`
  with no pool entry, which is why one row worked and the other did not.
  Fixed the same day; the row now measures 81 ms.

The CPU columns above are the original idle-box measurements and were **not**
re-taken — that re-run shared the host with an unrelated build, which makes a
CPU baseline pessimistic and every ratio derived from it flattering. The GPU
figures quoted in this note were measured under that same load, so they are
conservative rather than optimistic. The ray-tracer row was not re-run: it
needs `craton-gpu-0.2.0.jar` and a `cratonvm-gpuray` binary that are not
present in this tree.

**Re-measured 2026-09-07**, and this table is now that measurement rather
than an accumulation of three vintages. Quiet host (`bench-gpu/wait-for-quiet.sh`
gated it), `target-gpu/release/cratonvm.exe`, HotSpot Adoptium 25.0.3.9,
TornadoVM 4.0.1-jdk25-ptx, N = 2²⁴, best of 5, warm, full
host→device→host. Reproduce with `bash bench-gpu/rerun-table-rows.sh`.

Every arm of a row was run in the same session this time, including the CPU
baselines — which had previously been carried over from an older idle-box run.
They reproduced almost exactly (2,179 vs 2,146; 1,784 vs 1,780; 1,298 vs 1,300;
1,168 vs 1,172), which is the check that the setup is sound: the ratios moved
because CratonVM got faster, not because the baseline drifted.

Three rows moved against the previous table, and the double div-chain row
finally agrees with the note that has sat under it since 2026-09-05:

- **int div-chain 11 ms → 7 ms**, and **128 multiply-adds 8 ms → 7 ms.**
- **double div-chain 95 ms → 82 ms, TornadoVM 128 ms → 135 ms.** The note
  below said "the row now measures 81 ms against TornadoVM's 135" and the table
  above it still said 95/128. The table was stale; it is not any more.
- **The 128-multiply-add row needed a different harness to measure at all** —
  see the offload-gate defect recorded below.

The pre-2026-09-07 notes follow, kept because they explain how rows got where
they are:

- The **dot-product row is now 2 ms**, not 12 — the warp-shuffle reduction
  (one `red.global.add` per warp rather than per thread) landed after the
  original measurement.
- The **double div-chain row had silently stopped reproducing**: it measured
  9,276 ms, slower than HotSpot, because `runtime::offload_jit_gate` asked the
  constant-pool-*free* analyzer, which rejects `ldc2_w` unconditionally, while
  the dispatcher it gates for asks the pool-aware one. That kernel's
  `+ 1.0000001` is an `ldc2_w`; its integer twin's `+ 12345` is a `sipush`
  with no pool entry, which is why one row worked and the other did not.
  Fixed the same day; the row now measures 81 ms against TornadoVM's 135.

The CPU columns above are the original idle-box measurements and were **not**
re-taken — that re-run shared the host with an unrelated build, which makes a
CPU baseline pessimistic and every ratio derived from it flattering. The GPU
figures quoted in this note were measured under that same load, so they are
conservative rather than optimistic. The ray-tracer row was not re-run: it
needs `craton-gpu-0.2.0.jar` and a `cratonvm-gpuray` binary that are not
present in this tree.

Notes:

- **FIXED 2026-09-07: a compiled caller silently stopped offloading a kernel in
  another class.** `bench-gpu/GpuComputeWarm.java` has `main()` call
  `GpuCompute.heavy` in a second class. Under `--gpu --print-gpu-decisions`
  that method never appeared in the decision log at all — not
  `Rejected(...)`, never asked — and the row read **2,611-2,934 ms on the
  CPU** against 7 ms on the device. The same kernel declared beside `main()`
  (`GpuComputeWarmSelf.java`) offloaded normally, which is what made it
  look like an analyzer problem. It was not.

  Root cause: the JIT has two one-way doors for a static call site — bind it
  directly to the callee's entry (`jit/src/lib.rs`) and inline it
  (`jit/src/x64/bytecode_walk.rs`). Both were gated on
  `offload_hook::is_kernel`, a lookup in a registry that `offload_jit_gate`
  fills as a side effect of scanning callers — and that scan cannot judge a
  target whose declaring class is not loaded yet. A caller is scanned when it
  is admitted to the JIT, which happens **before** it runs, so a callee in
  another class has typically never been touched at that moment. `main` here
  fills two 2²⁴ arrays first, so it is compiled at exactly the wrong time.
  The site was bound directly, the dispatch helper the offload hook lives
  behind was gone, and `try_compiled_offload`'s late registration (the
  2026-09-06 fix for the same underlying limitation) had no site left to run
  on.

  The fix is one predicate. A registry **miss** means either "not a kernel" or
  "could not have known yet", and these doors treated the two identically while
  making a decision that is irreversible. `offload_hook::keeps_dispatch_helper`
  now falls back on a miss to a descriptor-only test — `)V`/`)I`/`)J` with an
  array parameter, mirroring `target_can_ever_dispatch` — which needs no class
  loading and no locks. Keeping a helper is reversible and cheap; binding
  directly is neither.

  Verified: `GpuComputeWarm` went 2,934 ms → **6 ms**, and the census now
  prints `registered late, by the compiled site: GpuCompute.heavy([I[I[I)V`.
  Across five runs each on a quiet host the two harnesses are indistinguishable
  (cross-class 5-6 ms, same-class 5-6 ms), which is the property that was
  broken. `test_classes/gpu/GpuForwardRef.java` passes on both arms with equal
  checksums. `GpuHookOverheadBench`'s `base_ns_per_call` — a loop calling an
  **ineligible** target, which is what a broader predicate would have taxed —
  reads 7.07 ns with `--gpu` against 7.58 ns without, so the CPU side pays
  nothing; for scale, the old caller-refusal approach cost 407.9 ns there.

  `bench-gpu/rerun-table-rows.sh` now measures this row with the cross-class
  harness on purpose, and keeps the same-class one as a control arm. If the two
  ever diverge again, that is what regressed.

- The div-chain rows are the "GPU wins big" cases: division has no
  competitive CPU-vectorized form, so raw parallelism wins at every size
  tested (2²⁰–2²⁶; the ratios hold steady across sizes).
- CratonVM's double-division **kernel output** is bit-exact with HotSpot
  (`div.rn.f64` is IEEE-754 round-to-nearest, same as x86 `vdivpd`): the
  2026-09-07 run prints identical `OUT0=45.40459327514974` and
  `OUTN=7.2490508729547125` from both.

  Its **harness checksum** does not match, and the distinction matters. The
  checksum is a serial sum of 2²⁴ doubles in the benchmark's own epilogue;
  floating-point addition is not associative, so summing in a different order
  gives a different but equally correct total. CratonVM reads
  `5.928010028745152E7` against HotSpot's `5.92801002867254E7` — a relative
  difference of 1.2e-11, the size of a reassociated sum.

  TornadoVM's `5.9583712290819384E7` is 5.1e-3 away, eight orders of magnitude
  larger, and that one *is* a division difference: its PTX backend does not
  guarantee bit-exact division. Do not read the two divergences as the same
  kind of thing.
- **Root-caused: TornadoVM's `unimplemented` is a standing gap in
  mixed-type reductions, not a version/driver issue.** `TornadoSnippetReflectionProvider
  .forBoxed` (what the whole stack trace bottoms out in) is an unconditional
  stub — `unimplemented(); return null;` — in both the TornadoVM 4.0.1 jar on
  this box *and* the current `master` branch on GitHub, so upgrading would not
  fix it. Isolated the exact trigger with three minimal repros on this same
  GPU: a single-array `LongArray` sum reduction works; a two-array
  `LongArray`+`LongArray` sum reduction (no cast, no multiply) works; a
  single-array `IntArray` reduced into a `LongArray` accumulator with one
  `(long) a.get(i)` widening cast fails with the identical stack trace. So the
  precise gap is **a `@Reduce` kernel whose per-element expression needs a
  primitive widening conversion (`int`→`long`) before accumulating into a
  differently-typed reduce array** — not "long reductions" or "two-array
  reductions" in general. `GpuDotBench.dotReduce`'s `int·int → long` shape
  (needed to avoid `int` overflow in the product) is exactly that mixed-type
  case, and there's no workaround that preserves what the row measures, so
  "unimplemented" here is a real, durable TornadoVM limitation, confirmed by
  reading the source rather than assumed from the error message. CratonVM's
  automatic `--gpu` path handles the same reduction and completes the full
  dispatch-and-readback (an `is_reduction: true` kernel), not just a CPU
  fallback, as an earlier note assumed.
- **The multiply-add row's old "honest counter-case"
  framing was a compiler artifact, not a real result.** The previous kernel
  used a *compile-time-constant* multiplier (`x = x*1103+12345`, repeated 96
  times); composing an affine map with itself under constant coefficients is
  itself affine, and HotSpot C2's GVN/reassociation folds the whole chain
  into a single multiply+add with closed-form coefficients regardless of the
  unroll count — confirmed by unrolling the *old* kernel to 6,528 lines and
  seeing under 2x change in wall time. The benchmark was silently
  memory-bandwidth-bound on the HotSpot side, not compute-bound, which is
  why AVX2 looked "competitive." Sourcing the multiplier from a second
  per-element array (`m = b[i]`, the same trick `GpuDivChain` already uses
  for its divisor) removes the closed form: HotSpot now does genuine work
  and the row flips from "GPU loses" (0.7x) to another 163x GPU win. The
  dot-product row's swing (0.4x → 98x, and 586x since the warp-shuffle
  reduction) is different in kind: `sum += p` was
  never foldable (both operands are runtime array reads), so scaling its
  per-element repeat count from 1x to 300x — needed to push HotSpot over 1
  second — genuinely shifts the kernel from launch/PCIe-overhead-bound (GPU
  loses on a single multiply-add) to compute-bound (GPU wins). Both rows'
  `TornadoGpuCompute`/`GpuWarm`/`GpuCompute` and `GpuDotBench` sources carry
  `AUDIT` comments with the full detail and the measurements that
  back them up.
- **FIXED: dot-product CratonVM-GPU background `cudarc` panic.**
  Every `--gpu` run of `GpuDotBench` used to also print one `cudarc` panic per
  completed dispatch to stderr — `DriverError(CUDA_ERROR_NOT_PERMITTED,
  "operation not permitted")` out of cudarc's `CudaStream::drop`. Root cause:
  `dispatch_async`'s `cuLaunchHostFunc` completion callback
  (`vm/src/runtime/offload.rs`) cloned `Arc<StreamSubmission>` for its own use
  and let that clone drop locally at the end of the closure. For a
  synchronous (non-Future-API) dispatch — exactly `GpuDotBench`'s transparent
  `--gpu` path, which never calls `register_submission` — that clone reliably
  ends up the *last* strong reference by the time the driver fires the
  callback, so its drop (cascading into `cuda_bridge::Stream`'s and cudarc's
  `CudaStream`'s destructors, which issue a real CUDA driver call) ran from
  inside the callback — forbidden per CUDA's own `cuLaunchHostFunc` rules
  (and per this codebase's own doc comment on `Stream::add_host_callback`).
  Fix: the completion reaper's queue (`REAPER_QUEUE`) now carries the
  `Arc<StreamSubmission>` alongside the handle, so the callback *moves*
  ownership into the queue instead of dropping it locally — the eventual
  final drop, if any, now happens on the reaper thread (an ordinary thread,
  where CUDA driver calls are allowed) rather than inside the CUDA callback.
  Verified: 3 repeated `RUST_BACKTRACE=1` trials of the exact repro command
  with zero panics, correct `DOT_CHECKSUM == DOT_REF` and unchanged ~12ms
  timing every time; `cargo test -p cratonvm-vm --features gpu-offload --lib
  offload` (35 tests, including the reaper-specific ones) still passes; the
  other three kernels (div-chain, float-div-chain, warm MAD) re-verified with
  matching checksums and no regressions.

Full GPU results — more input sizes, `ldc`-constant kernels, cold-start
numbers up to N = 2²⁸, kernel sources, and eligibility rules — are in
[docs/gpu/README.md](docs/gpu/README.md).

## Reproducing

> **⚠ Build with fat LTO for any number you intend to compare against the
> baselines.** `[profile.release]` is `lto = "fat"` + `codegen-units = 1`, and the
> anchored baselines in `cratonbench-baseline-azure-epyc.tsv` were measured that
> way. Building with `CARGO_PROFILE_RELEASE_LTO=off` — the usual workaround for
> the OOM SIGKILL below — produces a **materially slower** binary whose absolute
> times are not comparable to those baselines. Measured on one commit,
> two binaries differing only in LTO, interleaved, 3 pairs per phase (host load
> ~6-7, so absolutes are inflated and the spread is wide — hashmap ranged
> 23461-31120 *within one arm*):
>
> | phase | LTO=off median | fat-LTO median | fat-LTO effect |
> |---|---|---|---|
> | bintrees | 3019 ms | 2262 ms | **−25%** |
> | hashmap | 25484 ms | 26196 ms | none |
> | sieve | 10262 ms | 9552 ms | ~−7% |
>
> Fat LTO is worth a real amount on bintrees and close to nothing on hashmap and
> sieve. Use it for anything baseline-comparable — but note what it does NOT
> explain, below.
>
> LTO=off is fine for **relative** A/B between two binaries built the same way —
> that is how the layout-registry win was measured — but say so when reporting,
> and never quote an LTO=off absolute against a baseline.

```bash
# Build. Fat LTO — required for baseline-comparable numbers.
cargo build --release -p cratonvm-cli

# If the final cratonvm-cli rustc dies with "signal: 9, SIGKILL" that is the OOM
# killer during fat-LTO codegen, NOT a compile error. Lower parallelism first:
cargo build --release -p cratonvm-cli -j2
# Only as a last resort, and only for relative A/B (see the warning above):
CARGO_PROFILE_RELEASE_LTO=off cargo build --release -p cratonvm-cli -j6

# CPU, all seven phases in-process:
javac -d bench-classes bench/CratonBench.java
./target/release/cratonvm -Xmx8g -cp bench-classes CratonBench

# One isolated phase (the per-row methodology):
./target/release/cratonvm -Xmx8g -cp bench-classes CratonBench bintrees

# The gated regression check (Linux bench host, refuses to run under load):
bash regression-suite/perf/run-cratonbench-gate.sh -Exe /abs/path/to/cratonvm

# Per-kernel harnesses, for one row without the others.
javac -d bench-classes bench/BinTreesClassic.java bench/HashMapOnly.java
# Binary Trees (depth as argv). Expected checksum at d=18: 68332206.
./target/release/cratonvm -Xmx8g -cp bench-classes BinTreesClassic 18
# HashMap put/get. Expected checksum at n=1,000,000: 15499991500000.
./target/release/cratonvm -Xmx8g -cp bench-classes HashMapOnly

# HotSpot reference on the same host, same flags:
$JAVA_HOME/bin/java -Xmx8g -cp bench-classes BinTreesClassic 18

# Profile rather than guess. This is how the layout-registry lookup was found
# to be ~40% of the Binary Trees run (37.8% in class_layout_for_fields plus
# 4.8% in the SipHash it used for a (u32,u32) key).
perf record -g --call-graph=dwarf -F 199 -o bt.perf -- \
  ./target/release/cratonvm -Xmx8g -cp bench-classes BinTreesClassic 18
perf report -i bt.perf --no-children --percent-limit 1 --stdio
```
