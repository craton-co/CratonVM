# Benchmarking CratonVM

Performance depends on the workload, hardware, JDK, collector, execution policy,
and build profile. Public result claims require checksum parity and measurements
from the same source revision; stored investigation results are local artifacts.

## CPU benchmarks

`tools/bench/CratonBench.java` contains the arithmetic, Fibonacci, sieve, matrix,
binary-tree, HashMap, and string/regex workloads. The maintained runner builds
and compares the supported suites with HotSpot:

```sh
cargo build --release -p cratonvm-cli
python tools/bench/run-published-ci.py --help
python -m unittest tools/bench/test_published_ci.py
```

Use the [benchmark methodology](docs/benchmarking/methodology.md) and
[reliability gate](docs/benchmarking/reliability-gate.md). Record compiler,
CPU, JDK, flags, checksums, repeated samples, and variation with the results.
Compare the foundational `--real-jdk` policy and the `--compatible` optimization
layer separately. A microbenchmark result does not establish framework throughput.

### CPU measurement environment


The main CPU matrix was measured on the dedicated Contabo Linux benchmark host with the following specification:
- **Processor**: AMD EPYC 9V45 96-Core Processor (Zen 4 architecture, family 26, model 2, stepping 1), 8 vCPUs assigned, SMT enabled (2 threads/core, 4 physical cores)
- **Caches**: L1d 192 KiB (4 instances), L1i 128 KiB (4 instances), L2 4 MiB (4 instances), L3 32 MiB shared
- **Host OS & Kernel**: Ubuntu 24.04.1 LTS, Linux kernel `7.0.0-1014-contabo #14~24.04.1-Ubuntu SMP PREEMPT x86_64`
- **Reference JDK**: OpenJDK JDK 25 LTS (Temurin-25.0.4+7, build 25.0.4+7-LTS, C2 HotSpot Server VM)
- **CratonVM**: Release build tested under compatible mode (`--compatible`)

### Methodology

- **Isolated Processes**: Every measurement runs in a fresh, isolated process invocation per benchmark phase and iteration to eliminate heap pollution, code cache bias, and GC state leakage.
- **Interleaved Execution**: Runs are strictly interleaved across VM configurations (`cvm_zgc` → `cvm_g1` → `hs_zgc` → `hs_g1`) to ensure transient host conditions affect all configurations symmetrically.
- **Garbage Collectors Tested**:
  1. **ZGC**: Low-latency ZGC (`-XX:+UseZGC`)
  2. **G1**: Multi-threaded balanced throughput/latency collector (`-XX:+UseG1GC`)
- **Memory Flags**: Fresh process per measurement with `-Xmx16g` for large heap allocations and `-Xmx8g` baseline.
- **100% Checksum Verification**: Output checksums are printed and strictly verified against HotSpot across every run and every size parameter. Zero mismatches permitted.
- **Metric**:
  $$\text{Ratio (best for CratonVM)} = \min\left(\frac{\text{CVM ZGC}}{\text{HotSpot ZGC}}, \frac{\text{CVM G1}}{\text{HotSpot G1}}\right)$$
  (Values $< 1.0\times$ indicate CratonVM is faster than HotSpot).

### CPU benchmark results

#### 1. Arithmetic

Long integer arithmetic loop exercising arithmetic operations, shifts, and remainder calculation (`sum += i * 3 - i / 2 + i % 7`).

| Iteration | CVM + ZGC | HotSpot + ZGC | CVM + G1 | HotSpot + G1 | Ratio (best for CratonVM) |
|:----------|---:|---:|---:|---:|---:|
| **100M**  | 132 ms | 112 ms | 134 ms | 112 ms | 1.18x |
| **1B**    | 1,315 ms | 1,106 ms | 1,317 ms | 1,103 ms | 1.19x |
| **10B**   | 13,133 ms | 11,027 ms | 13,143 ms | 11,026 ms | 1.19x |

#### 2. Fibonacci

Recursive Fibonacci calculation (`fib(n) = fib(n-1) + fib(n-2)`) testing method prologue/epilogue overhead, fast guarded recursive call dispatch, and call frame management.

| Iteration | CVM + ZGC | HotSpot + ZGC | CVM + G1 | HotSpot + G1 | Ratio (best for CratonVM) |
|:---|---:|---:|---:|---:|---:|
| **40** | 81 ms | 255 ms | 82 ms | 254 ms | **0.32x** |
| **44** | 540 ms | 1,741 ms | 542 ms | 1,892 ms | **0.31x** |
| **48** | 3,710 ms | 11,949 ms | 3,698 ms | 12,103 ms | **0.31x** |

#### 3. Sieve

Sieve of Eratosthenes prime generation across multiple repetitions over dynamically allocated `boolean[]` heaps, testing bounds-check elimination and array store performance.

| Iteration | CVM + ZGC | HotSpot + ZGC | CVM + G1 | HotSpot + G1 | Ratio (best for CratonVM) |
|:---|---:|---:|---:|---:|---:|
| **50K × 10K** | 600 ms | 700 ms | 598 ms | 560 ms | **1.07x** |
| **100K × 20K** | 2,492 ms | 3,362 ms | 2,494 ms | 2,758 ms | **0.90x** |
| **100K × 60K** | 7,489 ms | 9,484 ms | 7,469 ms | 8,091 ms | **0.92x** |

#### 4. Matrix

Square matrix integer multiplication with allocation and triple-nested loops ($O(N^3)$), testing vectorisation, cache locality, and nested loop lowering.

| Iteration | CVM + ZGC | HotSpot + ZGC | CVM + G1 | HotSpot + G1 | Ratio (best for CratonVM) |
|:---|---:|---:|---:|---:|---:|
| **720x720** | 458 ms | 454 ms | 450 ms | 452 ms | **1.00x** |
| **1280x1280** | 2,504 ms | 2,550 ms | 2,451 ms | 2,508 ms | **0.98x** |
| **2560x2560** | 60,815 ms | 57,161 ms | 44,828 ms | 51,510 ms | **0.87x** |

#### 5. Binary Trees

Classic binary trees allocation, traversal, and deallocation benchmark testing high-rate object allocation, GC young generation collection, and pointer chasing.

| Iteration | CVM + ZGC | HotSpot + ZGC | CVM + G1 | HotSpot + G1 | Ratio (best for CratonVM) |
|:---|---:|---:|---:|---:|---:|
| **16** | 152 ms | 151 ms | 270 ms | 124 ms | **1.01x** |
| **18** | 630 ms | 434 ms | 821 ms | 411 ms | 1.45x |
| **20** | 2,709 ms | 1,561 ms | 3,140 ms | 1,472 ms | 1.74x |
| **22** | 12,450 ms | 9,293 ms | 11,840 ms | 5,597 ms | 1.34x |
| **24** | 52,140 ms | 37,827 ms | 46,280 ms | 21,751 ms | 1.38x |

#### 6. HashMap

Heavy insertion and retrieval workload on standard `java.util.HashMap` (`put` and `get`), testing object hashing, table resizing, reference manipulation, and boxed key access.

| Iteration | CVM + ZGC | HotSpot + ZGC |  CVM + G1 | HotSpot + G1 | Ratio (best for CratonVM) |
|:----------|----------:|--------------:|----------:|-------------:|--------------------------:|
| **1M**    |     72 ms |         65 ms |     90 ms |        82 ms |                     1.11x |
| **5M**    |    226 ms |        216 ms |    296 ms |       324 ms |                     1.05x |
| **10M**   |    429 ms |        444 ms |  4,632 ms |       582 ms |                 **0.97x** |
| **50M**   |  3,491 ms |      3,523 ms | 15,176 ms |     2,972 ms |                     0.99x |
| **100M**  | 11,630 ms |      6,205 ms | 26,884 ms |     5,710 ms |                     2.04x |

#### 7. String / Regex

Pattern compilation and regex matching using `java.util.regex.Pattern` and `Matcher.find()`, testing string slice extraction, capture group allocations, and standard library bridge interactions.

| Iteration | CVM + ZGC | HotSpot + ZGC | CVM + G1 | HotSpot + G1 | Ratio (best for CratonVM) |
|:---|---:|---:|---:|---:|---:|
| **1M** | 163 ms | 89 ms | 164 ms | 94 ms | 1.74x |
| **5M** | 583 ms | 360 ms | 596 ms | 379 ms | 1.57x |
| **10M** | 1,042 ms | 689 ms | 1,121 ms | 749 ms | 1.50x |
| **50M** | 5,536 ms | 3,389 ms | 5,650 ms | 3,166 ms | 1.63x |
| **100M** | 10,666 ms | 6,477 ms | 12,001 ms | 6,187 ms | 1.65x |

The String / Regex row uses foundational `--real-jdk` execution on AMD EPYC
Turin (16 vCPUs, pinned to CPUs 8–15), Linux 7.3 and HotSpot OpenJDK 25.0.5.
CratonVM revision `2d84874046` uses a fat-LTO release build. Each cell is the
median of three interleaved fresh-process rounds with `-Xmx16g`, host load
1.5–3.5 and the kernel in `tools/bench/StringRegexOnly.java`. All 60 runs match
the expected checksum. ZGC uses concurrent marking and relocation in the pause.

### Experimental matrix and flat-map results

`CratonBench matrix-tiled` uses a 64-square tiled integer multiplication loop.
`HashMapSimd` compares a flat integer-key map using scalar probing and an explicit
512-bit JDK Vector API species. The original matrix and `java.util.HashMap`
workloads remain the controls. Hardware SIMD execution requires AVX-512.

Tile calibration on the AMD EPYC-Turin AVX-512 host (three fresh-process
rounds, CPU 2, G1, 2 GiB heap; medians in milliseconds):

| Tile | HotSpot 25 | CratonVM compatible |
|---|---:|---:|
| 16 | 1357 | 476 |
| 32 | 1206 | 316 |
| 64 | 1096 | 296 |

The fixed tile is 64, which won in both runtimes.

#### Matrix (Tiled)

Tiled $64 \times 64$ square integer matrix multiplication loop with AVX-512 vectorization.

| Iteration | CVM + ZGC | HotSpot + ZGC | CVM + G1 | HotSpot + G1 | Ratio (best for CratonVM) |
|:---|---:|---:|---:|---:|---:|
| **720x720** | 74 ms | 199 ms | 77 ms | 209 ms | **0.37x** |
| **960x960** | 157 ms | 457 ms | 160 ms | 476 ms | **0.34x** |
| **1280x1280** | 364 ms | 1,082 ms | 379 ms | 1,133 ms | **0.34x** |
| **1920x1920** | 1,255 ms | 4,035 ms | 1,227 ms | 3,758 ms | **0.33x** |
| **2560x2560** | 2,974 ms | 9,240 ms | 2,967 ms | 9,203 ms | **0.32x** |
These experimental variants
do not replace or re-anchor the original CPU benchmark tables.

Five-round runtime comparison on `64.177.43.150` (milliseconds; medians;
identical Java sources, G1, 2 GiB heap):

| Kernel | HotSpot 25 | CratonVM real-jdk | CratonVM compatible |
|---|---:|---:|---:|
| Matrix, original | 2671 | 2561 | 2565 |
| Matrix, 64 tile | 1117 | 293 | 293 |
| `java.util.HashMap` control | 157 | 113 | 104 |
| Flat map, scalar | 78 | 122 | 119 |
| Flat map, AVX-512 SIMD | 219 | 21914 | 21631 |

The tiled matrix is faster than the original loop in each runtime. The flat
SIMD map preserves its checksum but is about 182 times slower than the scalar
flat map in Compatible mode. It remains an experimental workload and does not
replace `java.util.HashMap` or establish a general SIMD-map speedup.

## Cold start and framework workloads

Cold-start measurements include process launch, VM boot, class loading, `main`,
and exit. The comparison uses seven interleaved fresh-process rounds on a shared
Linux x86-64 host with 16 vCPUs and 60 GiB RAM. Host load is 37–48, so wall times
include substantial scheduling contention; instruction counts are the more stable
comparison. CratonVM uses a release build with LTO off. The reference runtimes
are HotSpot 25 with its default CDS archive and GraalVM 25.0.4 in JVM/native-image
modes. Restored rows use `CRATONVM_BOOT_IMAGE=auto` and an image for each command
line and execution policy.

### Launching Hello

| Configuration         | Wall min | Wall median | Instructions | CPU (task-clock) | Max RSS | vs HotSpot |
|-----------------------|---|---|---|---|---|---|
| graalvm25-native      | 43 ms | 82 ms | 2.0M | 6 ms | 18 MiB | SAME |
| hotspot25             | 166 ms | 250 ms | 71M | 55 ms | 44 MiB | — |
| cvm-real-jdk-restored | 191 ms | 270 ms | 59M | 84 ms | 157 MiB | SAME |
| cvm-compatible-restored | 195 ms | 332 ms | 59M | 83 ms | 152 MiB | SAME |
| graalvm25-jvm         | 349 ms | 595 ms | 528M | 259 ms | 124 MiB | SAME |
| cvm-compatible        | 580 ms | 1,097 ms | 986M | 419 ms | 179 MiB | SAME |
| cvm-real-jdk          | 589 ms | 1,266 ms | 2,475M | 859 ms | 196 MiB | SAME |

### Instructions across JDK startup workloads

The following values are median user-mode instructions in millions.

| Program | graalvm25-native | hotspot25 | cvm-compatible-restored | cvm-real-jdk-restored | graalvm25-jvm | cvm-compatible | cvm-real-jdk |
|---|---|---|---|---|---|---|---|
| Hello | 2.0 | 71 | 59 | 59 | 528 | 986 | 2,475 |
| P5Zip | 2.4 | 408 | 826 | 1,583 | 882 | 1,707 | 3,420 |
| P5Math | 2.1 | 213 | 65 | 64 | 705 | 1,000 | 2,583 |
| P5Exc | 2.1 (DIFF) | 112 | 77 | 75 | 534 | 1,006 | 2,606 |
| P5Regex | 6.5 | 352 | 2,361 | 3,394 | 895 | 3,261 | 5,298 |
| P5Nio | 2.5 | 378 | 2,499 | 3,970 | 829 | 3,243 | 5,272 |
| P5Misc | 6.7 (DIFF) | 1,089 | 11,855 | 13,603 | 1,827 | 12,031 | 14,731 |

| Program | What it exercises |
|---|---|
| `Hello` | prints one line |
| `P5Zip` | zip and CRC |
| `P5Math` | `Math`/`StrictMath`/`BigDecimal` |
| `P5Exc` | exception messages, including helpful NPE text |
| `P5Regex` | regex and streams |
| `P5Nio` | NIO files and buffers |
| `P5Misc` | formatting, dates, URI/UUID/Base64, serialization |

`SAME` means stdout, stderr, and exit status match HotSpot on every measured run.
CratonVM cold and restored runs, and GraalVM JVM runs, match the HotSpot controls.
The native-image `DIFF` rows have Java-visible differences: `P5Exc` adds a
native-image-specific `ClassNotFoundException` message, and `P5Misc` fails
`ArrayList` deserialization without a registered serialization configuration.
Native-image is a separate binary built for each program, rather than JVM launch.

Use the [cold-start harness](docs/benchmarking/cold-start/README.md) to compile
these programs, compare their outputs, and measure cold/captured/restored runs.
[Application performance](performance.md) contains the framework and enterprise
workload tables. Their maintained runner is
`tools/bench/apps/run-apps-benches-interleaved.sh`; external framework checkouts
are supplied by the operator under ignored `apps/` directories.

## GPU benchmarks

GPU offload is opt-in (`--gpu`) and operates on authentic OpenJDK class libraries
under foundational `--real-jdk` execution. Unlike frameworks requiring proprietary
APIs or annotations, CratonVM transparently analyzes eligible static array methods,
lowers them to PTX, manages host-pinned DMA memory, and executes them via native
driver streams. Unsupported methods safely fall back to CPU execution without
semantic divergence.

See [GPU CI](docs/gpu/ci.md), the [GPU reference](docs/gpu/README.md), and
`tools/bench/bench-gpu/run-gpu-comparison.sh` for checksum-checked comparisons.
CPU fallback must be reported separately from device execution.

### Empirical comparison: CratonVM GPU vs. TornadoVM 5.2 PTX vs. HotSpot C2

Measured on an NVIDIA GeForce RTX 2060 (12 GB GDDR6, Turing `sm_75`, CUDA 13.3)
under JDK 25.0.3 and TornadoVM 5.2.0 (`v5.2.0-jdk25` PTX/CUDA runtime):

#### Compute-heavy kernel (`GpuCompute.heavy`, 128 multiply-adds/elem)

Arithmetic: 128 data-dependent steps ($x = x \cdot m + 12345$, with dynamic runtime
multiplier $m = b[i]$ preventing C2 constant folding). Warm best-of-5 (`GpuWarm` /
`TornadoGpuWarm`) plus 1-shot cold launch (`GpuCompute`). Checksums verified
bit-identical across all engines.

| Dataset Size $N$           | HotSpot 25 C2 CPU (Warm) | TornadoVM 5.2 PTX (Warm) | CratonVM GPU (Cold Launch) | CratonVM GPU (Warm Launch) | Speedup vs HotSpot (CratonVM Warm) | Speedup vs TornadoVM (CratonVM Warm) |      Checksum Agreement      |
|:---------------------------|:------------------------:|:------------------------:|:--------------------------:|:--------------------------:|:----------------------------------:|:------------------------------------:|:----------------------------:|
| **$2^{20}$ (1,048,576)**   |          67 ms           |           1 ms           |           21 ms            |       **<1 ms** (`0 ms`)   |             **>67.0×**             |              **>1.0×**               |  Match (`-128786017498022`)  |
| **$2^{22}$ (4,194,304)**   |          274 ms          |           4 ms           |           26 ms            |          **1 ms**          |             **274.0×**             |              **4.00×**               |  Match (`-514700258626850`)  |
| **$2^{24}$ (16,777,216)**  |         1,087 ms         |          17 ms           |           50 ms            |          **6 ms**          |             **181.2×**             |              **2.83×**               | Match (`-2050798668070082`)  |
| **$2^{26}$ (67,108,864)**  |         4,378 ms         |          67 ms           |           170 ms           |         **24 ms**          |             **182.4×**             |              **2.79×**               | Match (`-8073050055711620`)  |
| **$2^{28}$ (268,435,456)** |        17,402 ms         |          265 ms          |           644 ms           |         **96 ms**          |             **181.3×**             |              **2.76×**               | Match (`-30213752286551639`) |

*Cold launch includes runtime bytecode analysis, JIT lowering to PTX, initial driver device allocation, and host pin caching.*

#### Memory-bound kernel (`VectorAdd`, $c[i] = a[i] + b[i]$, 10 iterations)

| Dataset Size $N$ | TornadoVM 5.2 PTX (Mean) | TornadoVM 5.2 PTX (Best) | TornadoVM 5.2 CUDA (Mean / Best) | Correctness Verification |
| :--- | :---: | :---: | :---: | :---: |
| **$2^{20}$ (1,048,576)** | 0.896 ms | 0.711 ms | 0.782 ms / 0.581 ms | `OK` (0 mismatches) |
| **$2^{22}$ (4,194,304)** | 2.003 ms | 1.789 ms | 1.956 ms / 1.700 ms | `OK` (0 mismatches) |
| **$2^{24}$ (16,777,216)** | 6.554 ms | 6.307 ms | 6.503 ms / 6.216 ms | `OK` (0 mismatches) |
| **$2^{26}$ (67,108,864)** | 24.224 ms | 24.038 ms | 24.400 ms / 23.903 ms | `OK` (0 mismatches) |

#### Complex real-world workloads: RayTracer & GPULlama3

##### RayTracer Kernel (1080p & 8K UHD)
Primary-ray sphere intersection, diffuse shading, and color writeback to `int[] out` ($132.8\text{ MB}$ at 8K). Default monolithic pinned DMA writeback (`CHUNK_COUNT_DEFAULT = 1`):

| Resolution | HotSpot 25 C2 CPU (Best / Mean) | TornadoVM 5.2 PTX (Best / Mean) | CratonVM GPU (Best) | CratonVM GPU (Mean) | Speedup vs HotSpot (Best) | Speedup vs TornadoVM (Best) | Checksum Agreement |
| :--- | :---: | :---: | :---: | :---: | :---: | :---: | :--- |
| **1080p ($1920 \times 1080$, 2.07M px)** | 52.20 ms / 52.87 ms | 2.007 ms / 2.183 ms | **1.018 ms** | **1.050 ms** | **51.3×** | **1.97×** | CV == HS (`5462181685950`) |
| **8K UHD ($7680 \times 4320$, 33.18M px)** | 831.36 ms / 842.56 ms | 24.143 ms / 24.283 ms | **17.529 ms** | **17.928 ms** | **47.4×** | **1.38×** | CV == HS (`87319822645950`) |

*TornadoVM 5.2 CUDA measured `2.253 ms` best (`2.419 ms` mean) at 1080p and `28.394 ms` best (`28.810 ms` mean) at 8K UHD before hitting a `0xC0000005` JVM teardown crash (`checksum = 5462178198801` / `87319819158801`).*

##### GPULlama3 (LLM Inference Forward Pass)
Greedy token generation (`--temperature 0`) on `Llama-3.2-1B-Instruct-F16.gguf`
(453 kernel launches per token, single-stream capture graph, `--gpu --gpu-approx-math`;
`-Dllama.craton.ctx=1024` for 800-token runs; interleaved multi-round medians):

| Run / Prompt | HotSpot 25 C2 CPU | TornadoVM 5.2 PTX (`--cuda-graphs` off / on) | CratonVM GPU (Captured Graph) | Speedup vs HotSpot | Speedup vs TV PTX (Base / Graphs) | Text Parity vs HotSpot & Pool Reuse |
| :--- | :---: | :---: | :---: | :---: | :---: | :--- |
| **32 tokens** (`"Why is the sky blue?"`) | 10.24 tok/s (3.13 s) | 22.31 / 27.85 tok/s (1.43 / 1.15 s) | **32.17 tok/s** (0.99 s) | **3.14×** | **1.44× / 1.16×** | **CV: 3/3 exact** (`86.9%–93.2%` pooled); TV: 0/3 |
| **64 tokens** (`"Why is the sky blue?"`) | 11.95 tok/s (5.36 s) | 23.17 / 29.03 tok/s (2.76 / 2.20 s) | **36.18 tok/s** (1.77 s) | **3.03×** | **1.56× / 1.25×** | **CV: 3/3 exact** (`85.7%–92.0%` pooled); TV: 0/3 |
| **`-n 800` EOS** (`"Why is the sky blue?"`) | 12.67 tok/s (368 tok, 29.13 s) | 19.34 / 24.62 tok/s (800 tok, 41.70 / 32.52 s) | **36.94 tok/s** (365 tok, 9.88 s) | **2.92×** | **1.91× / 1.50×** | CV stops at EOS (exact thru char 1290, `91.9%–92.0%` pooled); TV loops 800 tok (`0/2`) |
| **`-n 800` full decode** (Essay prompt, 800 tok) | 12.39 tok/s (64.55 s) | 19.98 / 24.53 tok/s (40.04 / 32.62 s) | **35.26 tok/s** (22.70 s) | **2.85×** | **1.76× / 1.44×** | **CV: 2/2 exact (all 3,351 chars)** (`92.7%–92.8%` pooled); TV: 0/3 |

*TornadoVM 5.2 CUDA (`--cuda`) crashes (`0xC0000005`) on `llama-tornado` before completing decode.*


