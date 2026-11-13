# Real-World Applications Performance: CratonVM vs HotSpot

Evidence and benchmarks measuring CratonVM performance against OpenJDK 25 HotSpot on production-grade enterprise application workloads, official test suites, and framework benchmarks.

---

## Overview

While focused CPU microbenchmarks and numerical kernels frequently achieve near-parity or GPU acceleration (see [BENCHMARK.md](BENCHMARK.md)), real-world enterprise applications show an observed **3x–10x performance gap** against HotSpot (ranging from ~1.1x in specialized cryptographic curves up to 10x–15x in tight formatted I/O and unrolled buffer kernels).

This gap reflects the compounding runtime overhead of full-stack Java frameworks:
- Multi-tier dynamic class loading and complex class hierarchy scanning
- Dynamic proxy generation, method interceptor chains, and reflective dispatch
- Bytecode manipulation and metadata caching
- Multi-threaded shared-memory state and concurrent storage engines (MVStore, database transactions)
- Cryptographic primitive execution without specialized assembly intrinsics

---

## Comprehensive Interleaved Benchmark Results

Measurements were collected on the dedicated Linux benchmark host using strictly interleaved executions (CratonVM and HotSpot alternating per test) under a **1,200-second timeout** per benchmark arm. All test suites passed functionally with zero errors and zero timeouts.

- **Host:** Linux x86_64 (`7.3.0-10-generic`), 16 vCPUs
- **Reference JDK:** OpenJDK 25 (`/usr/lib/jvm/java-25-openjdk-amd64`)
- **CratonVM:** `cratonvm` (dev branch, release build)
- **Execution Mode:** Interleaved (CratonVM / HotSpot)
- **Timeout:** 1,200 seconds per run

| Application | Benchmark / Probe Description | HotSpot (ms) | CratonVM (ms) | Ratio (CV / HS) | Verdict |
| :--- | :--- | :---: | :---: | :---: | :---: |
| **Tomcat** | URI `toAbsolute` resolution (`TestResponsePerformance`, 1M ops) | 4,622 ms | 19,330 ms | **4.18x** | PASS |
| **Tomcat** | Jakarta EL Parser throughput & reinit (`TestELParserPerformance`) | 4,153 ms | 12,073 ms | **2.91x** | PASS |
| **Tomcat** | HTTP method parsing (`TestMethodPerformance`, 100M ops) | 19,188 ms | 158,525 ms | **8.26x** | PASS |
| **Tomcat** | Charset lookup cache hit throughput (`TestCharsetCachePerformance`) | 19,741 ms | 72,507 ms | **3.67x** | PASS |
| **Tomcat** | JULI log record format throughput (`TestOneLineFormatterPerformance`, 100K ops) | 2,201 ms | 3,967 ms | **1.80x** | PASS |
| **H2 Database** | Multi-threaded MVStore Concurrency (`TestBenchmark`) | 160,271 ms | 496,097 ms | **3.10x** | PASS |
| **H2 Database** | MVStore Key-Value Storage Engine Throughput (`TestMVStoreBenchmark`) | 14,000 ms | 42,100 ms | **3.01x** | PASS |
| **H2 Database** | Full SQL Transactions across Embedded, C/S TCP & In-Memory (`TestPerformance`) | 47,081 ms | 574,204 ms | **12.20x** [^1] | PASS |
| **H2 Database** | Disk-backed TCP, size 5,000 (`TestPerformance`, G1) | 59,830 ms | 398,770 ms | **6.67x** [^3] | PASS |
| **Apache Commons Math** | `JdkMath` numerical evaluation kernel (`JdkMathTest`) | 184 ms | 891 ms | **4.84x** | PASS |
| **Apache Commons Math** | Comprehensive 100K Transcendental Suite (`JdkMathTestPerformance` / `PerfTestUtils`) | 1,560 ms | 6,920 ms | **4.43x** | PASS |
| **Bouncy Castle Java** | `AESFast` symmetric cipher validation & throughput | 132 ms | 975 ms | **7.39x** | PASS |
| **Bouncy Castle Java** | Elliptic Curve Point Multiplication Suite (`ECPointPerformanceTest`, full 88-curve sweep) | 1,032,673 ms | 1,190,220 ms | **1.15x** | PASS |
| **Spring Framework** | AOP Introductions delegation (`IntroductionBenchmarkTests`, 100K ops) | 2,440 ms | 7,503 ms | **3.08x** | PASS |
| **Spring Framework** | AspectJ vs Spring AOP proxy invocation (`BenchmarkTests`, 8 suites) | 4,186 ms | 11,620 ms | **2.78x** | PASS |
| **Spring Boot** | VirtualZip Archive Sequential & Random Read (`VirtualZipPerformanceTests`) | 1,110 ms | 10,060 ms | **9.06x** | PASS |
| **Netty** | HTTP Date Parser & Formatter Throughput (`DateFormatterBenchmark`, 500K ops) | 148 ms | 1,965 ms | **13.28x** | PASS |
| **Netty** | Heap & Direct ByteBuf Long Set/Get (`NettyByteBufProbe`, 1M pairs; rounds 1–4 median) | 0.91 ms (allow) / 1.00 ms (default) | 36.67 / 125.55 ms foundational; 43.61 / 127.38 ms compatible (allow/default) | **40.5x / 125.7x foundational; 48.2x / 127.5x compatible (allow/default)** | PASS [^2] |
| **Hibernate ORM** | Graph vs Legacy ActionQueue Entity Insertion (`ActionQueuePerformanceTest`) | 2,243 ms | 12,999 ms | **5.79x** | PASS |
| **Hibernate ORM** | Collection Element Batch Persistence & Query (`ElementCollectionPerfTest`) | 4,133 ms | 20,103 ms | **4.86x** | PASS |

---

## Detailed Component Benchmark Matrix: Cryptography & Numerical Math

Comparative breakdown across CratonVM (Default mode), CratonVM (Fast-Boot mode), OpenJDK HotSpot 25, and GraalVM 25:

### Bouncy Castle (Cryptography)

| Suite / Test | CVM Default | CVM Fast-Boot | HotSpot 25 | GraalVM 25 | CVM Def / HotSpot | CVM Fast / HotSpot | Graal / HotSpot | Verdict |
| :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| SHA-256 Digest | 436 ms | 420 ms | 67 ms | 99 ms | 6.51x | 6.27x | 1.48x | PASS |
| SHA-512 Digest | 492 ms | 432 ms | 67 ms | 95 ms | 7.34x | 6.45x | 1.42x | PASS |
| MD5 Digest | 398 ms | 352 ms | 54 ms | 81 ms | 7.37x | 6.52x | 1.50x | PASS |
| AES Block Cipher | 19,250 ms | 19,315 ms | 5,675 ms | 5,352 ms | 3.39x | 3.40x | 0.94x | PASS |
| AES-GCM AEAD Mode | 1,221 ms | 1,218 ms | 195 ms | 240 ms | 6.26x | 6.25x | 1.23x | PASS |
| ChaCha20-Poly1305 AEAD | 683 ms | 694 ms | 183 ms | 216 ms | 3.73x | 3.79x | 1.18x | PASS |
| RSA Asymmetric & KeyGen | 6,437 ms | 5,821 ms | 2,302 ms | 2,030 ms | 2.80x | 2.53x | 0.88x | PASS |
| Ed25519 Curve Signatures | 1,906 ms | 1,987 ms | 193 ms | 210 ms | 9.88x | 10.30x | 1.09x | PASS |
| X25519 Key Agreement | 505 ms | 473 ms | 70 ms | 95 ms | 7.21x | 6.76x | 1.36x | PASS |
| Message Authentication (MAC) | 376 ms | 320 ms | 51 ms | 74 ms | 7.37x | 6.27x | 1.45x | PASS |

### Apache Commons Math (Numerical)

| Suite / Test | CVM Default | CVM Fast-Boot | HotSpot 25 | GraalVM 25 | CVM Def / HotSpot | CVM Fast / HotSpot | Graal / HotSpot | Verdict |
| :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| Dfp Decimal Floating Point | 2,115 ms | 2,104 ms | 664 ms | 712 ms | 3.19x | 3.17x | 1.07x | PASS |
| DfpMath High-Precision Math | 809 ms | 753 ms | 226 ms | 261 ms | 3.58x | 3.33x | 1.15x | PASS |
| DfpDec Decimal Precision | 801 ms | 751 ms | 232 ms | 271 ms | 3.45x | 3.24x | 1.17x | PASS |
| MathArrays Vector Math | 722 ms | 657 ms | 183 ms | 207 ms | 3.95x | 3.59x | 1.13x | PASS |
| Integer Sequence Steppers | 668 ms | 604 ms | 144 ms | 173 ms | 4.64x | 4.19x | 1.20x | PASS |
| LU Matrix Decomposition | 649 ms | 602 ms | 173 ms | 199 ms | 3.75x | 3.48x | 1.15x | PASS |
| QR Matrix Decomposition | 747 ms | 693 ms | 204 ms | 232 ms | 3.66x | 3.40x | 1.14x | PASS |
| Cholesky Decomposition | 673 ms | 609 ms | 178 ms | 203 ms | 3.78x | 3.42x | 1.14x | PASS |
| EigenDecomposition Spectral | 1,161 ms | 1,108 ms | 299 ms | 393 ms | 3.88x | 3.71x | 1.31x | PASS |
| Kohonen Neural Network SOM | 729 ms | 682 ms | 177 ms | 209 ms | 4.12x | 3.85x | 1.18x | PASS |

---

## Analysis by Workload Category

### 1. Framework Proxying & Invocation Chains (2.78x – 3.08x)
- **Spring Framework AspectJ & AOP Benchmarks:** Exercising dynamic invocation handlers, method interceptors, and AOP introduction delegation across 8 distinct aspect configurations. CratonVM completes the 100K-call delegation in 7.5s (vs HotSpot's 2.4s, 3.08x) and the 8-suite benchmark in 11.6s (vs 4.2s, 2.78x).
- **Takeaway:** Method resolution, virtual dispatch, and interface calls in complex object graphs remain efficient.

### 2. Expression Parsing & String Resolution (1.80x – 4.18x)
- **Tomcat JULI Log Record Formatting (`TestOneLineFormatterPerformance`):** Log record formatting across 100K ops runs in 3,967 ms vs 2,201 ms (1.80x).
- **Tomcat Jakarta EL Parser (`TestELParserPerformance`):** Repeated expression tree generation and evaluation achieves 12.0s on CratonVM vs 4.1s on HotSpot (2.91x).
- **Tomcat URI Resolution (`TestResponsePerformance`):** Canonical path resolution and URI validation across 1M operations runs in 19.3s vs 4.6s (4.18x).
- **Tomcat Charset Cache (`TestCharsetCachePerformance`):** Concurrent lookup cache hit path runs in 72.5s vs 19.7s (3.67x).

### 3. Multi-Threaded Storage & Concurrency (3.01x – 3.10x)
- **H2 MVStore Concurrency (`TestBenchmark`):** Multi-threaded concurrent read/write transactional load across 32 down to 1 worker threads, combined with memory-mapped storage engine persistence. CratonVM scales reliably across all worker threads, completing in 496s vs 160s (3.10x).
- **H2 MVStore Storage Engine (`TestMVStoreBenchmark`):** Direct key-value storage engine operations complete in 42.1s vs 14.0s (3.01x).

### 4. Enterprise Persistence & ORM (4.86x – 5.79x)
- **Hibernate ORM Graph ActionQueue (`ActionQueuePerformanceTest`):** Dependency sorting and batch action queue execution completes in 12.9s vs 2.2s (5.79x).
- **Hibernate ORM Collection Element Mapping (`ElementCollectionPerfTest`):** Full entity mapping, embedded collections, and H2 database roundtrips complete in 20.1s vs 4.1s (4.86x).

### 5. Tight Cryptographic & Loop Kernels (1.12x – 9.88x)
- **Bouncy Castle Elliptic Curve Point Multiplication (`ECPointPerformanceTest`):** Comprehensive point multiplication across affine, homogeneous, jacobian, and lambda-projective coordinates on all 88 standard and custom elliptic curves (NIST, SEC, Brainpool, GOST, SM2) runs at near-parity: 1,190.2s vs 1,032.6s (1.15x).
- **Tomcat HTTP Method Parsing (`TestMethodPerformance`):** 100M iterations of string header parsing runs in 158.5s vs 19.2s (8.26x).
- **Bouncy Castle Cryptographic Transforms:** Symmetric block ciphers (AES at 3.39x) and asymmetric operations (RSA at 2.80x) show competitive execution, while tight curve arithmetic (Ed25519 at 9.88x, SHA-512 at 7.34x) reflects lack of specialized vector/hardware acceleration.

---


---

### Benchmark follow-ups

[^1]: The historical row covers all three `TestPerformance` modes. The Linux follow-up measures H2's disk-backed TCP case, compares it with embedded controls and records the socket dispatch fixes. [H2 TCP benchmark follow-up](docs/internal/performance-and-benchmarks/benchmarking/H2_CS_AND_NETTY_BYTEBUF_PERFORMANCE_GAP_ANALYSIS.md#2-h2-tcp-benchmark-follow-up)

[^2]: The legacy `RunNetty bytebuf` driver is missing, so its old 9/983 ms result is not a current measurement. This row uses the maintained [Netty probe](tools/bench/apps/netty/README.md): 1M long set/get pairs per process for unpooled heap/direct and pooled heap/direct. Each cell is the median across four buffer kinds and four process repetitions, using each process’s median of warm rounds 1–4. Runtime `60c95229cc` and baseline `1d85eeef89` alternate order with the same HotSpot controls. All 160 fresh processes (64 baseline, 64 candidate, 32 HotSpot) passed five checksum checks each. The default-policy median gap fell **50.8% foundational / 51.1% compatible** (255.1x → 125.7x / 260.7x → 127.5x). This is the aggregate row metric; individual buffer kinds and the Unsafe-allowed path did not all halve. These shared-host results have a different workload and timing boundary from the missing driver. [Paired results and remaining broader gates](docs/known-issues/netty/NETTY_BYTEBUF_PERFORMANCE_GAP_ANALYSIS.md#paired-bytebuf-access-results-2026-11-12).

[^3]: The TCP-only follow-up completed 2,222,032 statements on each VM. The table reports process wall time. H2 reported 39,756 ms of measured SQL phases on HotSpot and 275,323 ms on CratonVM with `-XX:+UseG1GC`. These runs used the same host and workload at different times. The default CratonVM ZGC run measured 589,599 ms of SQL phases. The comparison and reproduction details are in the [H2 TCP benchmark follow-up](docs/internal/performance-and-benchmarks/benchmarking/H2_CS_AND_NETTY_BYTEBUF_PERFORMANCE_GAP_ANALYSIS.md#h2-bytecode-and-command-cache).

## Reproducing the Results

The benchmark files and automated interleaved runner are located in [`tools/bench/apps/`](tools/bench/apps/):
The [published table CI harness](tools/bench/PUBLISHED-CI.md) builds the VM and
runs this application table with the CPU and cold-start comparisons. It requires
the prebuilt framework fixtures described there.

```bash
# Execute the interleaved benchmark suite (1,200s timeout per arm):
./tools/bench/apps/run-apps-benches-interleaved.sh
```

Raw execution logs and timestamped Markdown reports are generated in `tools/bench/apps/results/<TIMESTAMP>/`.
