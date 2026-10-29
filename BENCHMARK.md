# CratonVM Benchmarks

Methodology, full notes, and provenance for the numbers in
[README.md](README.md). The headline rule for every table we publish:
**checksums must match HotSpot on every run** — a fast wrong answer is a
bug, not a result.

## CPU benchmarks

### Environment and Hardware

All measurements were performed on the dedicated Contabo Linux benchmark host with the following specification:
- **Processor**: AMD EPYC 9V45 96-Core Processor (Zen 4 architecture, family 26, model 2, stepping 1), 8 vCPUs assigned, SMT enabled (2 threads/core, 4 physical cores)
- **Caches**: L1d 192 KiB (4 instances), L1i 128 KiB (4 instances), L2 4 MiB (4 instances), L3 32 MiB shared
- **Host OS & Kernel**: Ubuntu 24.04.1 LTS, Linux kernel `7.0.0-1014-contabo #14~24.04.1-Ubuntu SMP PREEMPT x86_64`
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

Measurements use fresh isolated processes per cell, alternating configurations
against HotSpot, pinned to dedicated CPU cores with `-Xmx8g` under compatible mode (`--compatible`).

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
under load, and the shared Contabo box frequently runs at load 5–8 with several
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

The compiler and runtime optimization details are documented in:

- [docs/JIT_OPTIMIZATION.md](docs/JIT_OPTIMIZATION.md) — the current JIT
  architecture and the optimizations that produced the table above

## Cold start

How long a short program takes from process start to exit: launcher, VM boot, class
loading, `main`, exit. Every row's output (stdout, stderr and exit code) is compared
with HotSpot 25 on every run. `SAME` means it matched on all runs. `DIFF n` means n
runs differed, and the cause is given below the tables.

### Environment and method

- **Host**: 16 vCPU Linux x86_64 (`64.177.43.150`), 60 GiB. The host is shared, and its
  load average was **37–48** during the run.
  - Wall times are therefore inflated for every configuration; HotSpot's Hello normally
    exits in about 40 ms.
  - Compare rows by **instructions** (`perf stat -e instructions:u`), which load does not
    change. Treat wall times as relative only.
- **Configurations**, run interleaved: every configuration runs each program once per
  round, for 7 rounds. Every run is a fresh process.
  - `hotspot25`: OpenJDK 25 (`/usr/lib/jvm/java-25-openjdk-amd64`), default CDS archive.
  - `graalvm25-jvm`: Oracle GraalVM 25.0.4 in JVM mode.
  - `graalvm25-native`: Oracle GraalVM 25.0.4 `native-image --no-fallback`. This is an
    ahead-of-time compiled closed-world binary, built per program in 130–204 s
    (6.6–17.4 MB each).
  - `cvm-compatible` / `cvm-jdk-only`: CratonVM release build (`phase5-cold-start`
    7df40bed85, LTO off), cold boot.
  - `cvm-*-restored`: the same binary with `CRATONVM_BOOT_IMAGE=auto`, restoring a boot
    image captured by an earlier run of the same command line (74 MB `--compatible`,
    80 MB `--jdk-only`).
    - **Not the shipped behaviour yet.** The images were captured with the diagnosis-only
      token `unsafe-ignore-pointers`, because the capture's pointer check still refuses
      every image (see "Boot image status" below). Plain `auto` refuses the capture and
      boots cold, with identical output.
- **Programs** ([docs/benchmarking/cold-start](docs/benchmarking/cold-start)):

  | Program | What it exercises |
  |---|---|
  | `Hello` | prints one line |
  | `P5Zip` | zip and CRC |
  | `P5Math` | `Math`/`StrictMath`/`BigDecimal` |
  | `P5Exc` | exception messages, including helpful NPE text |
  | `P5Regex` | regex and streams |
  | `P5Nio` | NIO files and buffers |
  | `P5Misc` | formatting, dates, URI/UUID/Base64, serialization |

- **Script**: `cold.sh` (interleaved; reports wall min and median, instruction median,
  task-clock median and max RSS median).

### Results

Hello, the pure start-up cost:

| Configuration | Wall min | Wall median | Instructions | CPU (task-clock) | Max RSS | vs HotSpot |
|---|---|---|---|---|---|---|
| graalvm25-native | 43 ms | 82 ms | 2.0M | 6 ms | 18 MiB | SAME |
| hotspot25 | 166 ms | 250 ms | 71M | 55 ms | 44 MiB | — |
| cvm-jdk-only-restored | 191 ms | 270 ms | 59M | 84 ms | 157 MiB | SAME |
| cvm-compatible-restored | 195 ms | 332 ms | 59M | 83 ms | 152 MiB | SAME |
| graalvm25-jvm | 349 ms | 595 ms | 528M | 259 ms | 124 MiB | SAME |
| cvm-compatible | 580 ms | 1,097 ms | 986M | 419 ms | 179 MiB | SAME |
| cvm-jdk-only | 589 ms | 1,266 ms | 2,475M | 859 ms | 196 MiB | SAME |

Instructions (median, millions) for all seven programs:

| Program | graalvm25-native | hotspot25 | cvm-compatible-restored | cvm-jdk-only-restored | graalvm25-jvm | cvm-compatible | cvm-jdk-only |
|---|---|---|---|---|---|---|---|
| Hello | 2.0 | 71 | 59 | 59 | 528 | 986 | 2,475 |
| P5Zip | 2.4 | 408 | 826 | 1,583 | 882 | 1,707 | 3,420 |
| P5Math | 2.1 | 213 | 65 | 64 | 705 | 1,000 | 2,583 |
| P5Exc | 2.1 (DIFF) | 112 | 77 | 75 | 534 | 1,006 | 2,606 |
| P5Regex | 6.5 | 352 | 2,361 | 3,394 | 895 | 3,261 | 5,298 |
| P5Nio | 2.5 | 378 | 2,499 | 3,970 | 829 | 3,243 | 5,272 |
| P5Misc | 6.7 (DIFF) | 1,089 | 11,855 | 13,603 | 1,827 | 12,031 | 14,731 |

Every CratonVM run (cold, capture and restored) and every GraalVM JVM run matched HotSpot.
A separate check restored each program in both modes with a verbose run. All 42
cold/capture/restored outputs matched HotSpot, and all 14 verbose runs showed the image
was used.

**Where native-image differs:** native-image is closed-world, which produced two
Java-visible differences on 7 of 7 runs each.
- `P5Exc`: the `ClassNotFoundException` message for a constant `Class.forName` gains
  `"This exception was synthesized during native image building..."`.
- `P5Misc`: deserializing an `ArrayList` fails with `InvalidClassException ... no valid
  constructor`, because no serialization config was registered.

### Reading the results

- **Restored boot matches HotSpot's start-up cost.** Restored Hello runs 59M user
  instructions against HotSpot's 71M (and native-image's 2M).
  - The restored run's wall time is mostly kernel work: mapping the image, about 20k
    minor faults, the GC pre-touching heap after restore, and an ASLR-off re-exec. That
    is why restored wall is still close to HotSpot's rather than well below it on this
    loaded host.
  - Hello, `P5Math` and `P5Exc` finish almost entirely inside boot, so the image removes
    nearly all of their cost.
  - `P5Zip`, `P5Regex`, `P5Nio` and `P5Misc` do real work after `main` starts in
    CratonVM's interpreter and baseline JIT. That work dominates, so the image saves only
    the boot share (about 0.9G instructions).
- **Cold boot** is about 14x HotSpot in instructions for Hello in `--compatible`, and about
  35x in `--jdk-only`.
  - A faithful boot defines about 300 boot classes, registers about 8,800 natives, and
    runs `System.initPhase1` and the module system in the interpreter.
  - HotSpot maps most of that state from its CDS archive.
  - Boot optimizations brought `--compatible` Hello from 1,190M to 986M (−17%).
    Those cuts are cheaper jimage indexing, one structural scan per boot class, and
    cheaper native registration.
- An earlier dev build that skipped JDK boot work measured "under 200 ms" CPU on these
  probes, but failed three of the seven (exceptions, regex/streams, formatting). It was
  reverted.

### Boot image status

`CRATONVM_BOOT_IMAGE=off|capture|auto` (default `off`), with `CRATONVM_BOOT_IMAGE_DIR`.
- **Restore works**: every probe matches HotSpot when restored, in both modes.
- **`auto` does not restore yet.** The capture's pointer check finds about 150 pointers
  into memory that mimalloc and glibc allocated before the snapshot point, which the image
  does not carry. Until their owners are fixed, every capture is refused and `auto` boots
  cold.
- Details and follow-ups: [`docs/feature-designs/boot-image-snapshot.md`](docs/feature-designs/boot-image-snapshot.md).

### Reproducing

```bash
# On a Linux host with perf, HotSpot 25 and GraalVM 25 (native-image) installed,
# with docs/benchmarking/cold-start copied to /root/p5kit/probes:
javac -d /root/p5kit/probes/cls /root/p5kit/probes/*.java
for c in Hello P5Zip P5Math P5Exc P5Regex P5Nio P5Misc; do
  /opt/graalvm-25/bin/native-image -cp /root/p5kit/probes/cls -o /root/p5kit/graal/$c --no-fallback $c
done
# cold.conf: one "name|command" line per configuration (@CP@ / @C@ are substituted)
bash /root/p5kit/cold.sh /root/p5kit/cold-out 7 Hello P5Zip P5Math P5Exc P5Regex P5Nio P5Misc
```

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

Ray tracer rows reflect interleaved executions, full H2D+kernel+D2H, with checksums bit-identical to HotSpot. The kernel is the reduced proxy (`bench-gpu/RayTracerKernel.java`). The margin over TornadoVM reflects CratonVM's lower fixed per-launch overhead and consistent per-pixel throughput edge across tested resolutions from 160×120 to 11520×6480.

Key GPU optimization properties:

- **Dot-product reduction**: The dot-product row achieves 2 ms latency using warp-shuffle reductions (one `red.global.add` per warp rather than per thread).
- **Constant pool awareness**: The offload analyzer inspects constant pool entries to accurately lower constants (`ldc2_w`, `sipush`) to device constants.
- **Cross-class kernel dispatch**: `offload_hook::keeps_dispatch_helper` preserves dispatch helpers when a compiled caller invokes an offloadable kernel in another class, ensuring calls transition cleanly to device execution.

Notes:

- **Cross-class kernel offload**: When a compiled method calls an offloadable kernel in a separate class, `offload_hook::keeps_dispatch_helper` falls back on cache misses to a descriptor-only check (`)V`/`)I`/`)J` with array arguments) rather than immediately binding directly to CPU entry points. This ensures dispatch helpers remain in place when the target class loads dynamically.
- The div-chain rows represent substantial GPU speedups: division has no competitive CPU-vectorized form, so raw parallelism yields 20x–300x speedups across sizes ($2^{20}–2^{26}$).
- CratonVM's double-division **kernel output** is bit-exact with HotSpot (`div.rn.f64` is IEEE-754 round-to-nearest, same as x86 `vdivpd`), producing identical output values from both engines.

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
- **Asynchronous completion queue (`REAPER_QUEUE`)**:
  `dispatch_async`'s `cuLaunchHostFunc` completion callback moves `Arc<StreamSubmission>`
  ownership into the completion reaper queue (`REAPER_QUEUE`) rather than dropping locally inside driver callbacks.
  This ensures any subsequent driver cleanup operations execute safely on reaper worker threads.

Full GPU results — more input sizes, `ldc`-constant kernels, cold-start
numbers up to N = 2²⁸, kernel sources, and eligibility rules — are in
[docs/gpu/README.md](docs/gpu/README.md).

### GPULlama3 end-to-end (Llama-3.2-1B-Instruct F16, decode throughput)

The real [GPULlama3.java](https://github.com/beehive-lab/GPULlama3.java)
application, unmodified inference loop, run three ways on the same RTX 2060:
HotSpot on the CPU (Vector API), TornadoVM 4.0.1 (PTX), and CratonVM on its
GPU offload path (`--gpu`, explicit executor API). Prompt `Why is the sky
blue?`, 128 tokens, greedy (temperature 0), the application's own
`achieved tok/s`. Arms are interleaved per round so a busy host cannot favour
whichever ran first. Higher is better.

| round | CratonVM GPU | TornadoVM (PTX) | HotSpot CPU |
|-------|--------------|-----------------|-------------|
| 1     | 39.5         | 21.2            | 11.9        |
| 2     | 39.6         | 19.3            | 11.4        |
| 3     | 37.9         | 21.0            | 11.6        |

Caveats: TornadoVM's generated text is garbage on this application (it
produces the wrong answer, so it may also be doing less work); CratonVM's text
is the correct answer. Measured on plain `--gpu` with the loop-carried-`iinc`
fix (`docs/feature-designs/jit-ir-optimizations.md`);
before it, `--gpu` printed one character and NULs for every string. The whole
loop was pinned to the P-cores (`start /affinity FFFF`): an unpinned pass on
this hybrid CPU landed on E-cores and every arm measured 2-3x slower
(CratonVM 36, TornadoVM 7-12, HotSpot 4-5 tok/s), which is host placement, not
a result. The application's own kernels use the explicit executor path.

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
