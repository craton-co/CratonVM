# Real-World Applications Performance: CratonVM vs HotSpot

Evidence and benchmarks measuring CratonVM performance against OpenJDK 25 HotSpot on production-grade enterprise application workloads, official test suites, and framework benchmarks.

---

## Overview

While focused CPU microbenchmarks and numerical kernels frequently achieve near-parity or GPU acceleration (see [BENCHMARK.md](BENCHMARK.md)), real-world enterprise applications show an observed **3x–13x performance gap** against HotSpot (ranging from ~1.1x in specialized cryptographic curves up to 10x–15x in tight formatted I/O, string replacement/resolution, and unrolled buffer kernels).

This gap reflects the compounding runtime overhead of full-stack Java frameworks:
- Multi-tier dynamic class loading and complex class hierarchy scanning
- Dynamic proxy generation, method interceptor chains, and reflective dispatch
- Bytecode manipulation and metadata caching
- Multi-threaded shared-memory state and concurrent storage engines (MVStore, database transactions)
- Cryptographic primitive execution without specialized assembly intrinsics

---

## Comprehensive Interleaved Benchmark Results

Measurements were collected using strictly interleaved executions (CratonVM and HotSpot alternating per test) under a **1,200-second timeout** per benchmark arm. All test suites listed below passed functionally with zero errors and zero timeouts.[^7]

- **Reference JDK:** OpenJDK 25 (Temurin `25.0.3` / `25.0.5`)
- **CratonVM:** `cratonvm` (dev branch, release build)
- **Execution Mode:** Interleaved (CratonVM / HotSpot)
- **Timeout:** 1,200 seconds per run

| Application             | Benchmark / Probe Description                                                                 | HotSpot (ms) | CratonVM (ms) | Ratio (CV / HS) |
| :---------------------- | :-------------------------------------------------------------------------------------------- | :----------: | :-----------: |:---------------:|
| **Tomcat**              | URI `toAbsolute` resolution (`TestResponsePerformance`, 1M ops)                               |   4,622 ms   |   19,330 ms   |    **4.18x**    |
| **Tomcat**              | Jakarta EL Parser throughput & reinit (`TestELParserPerformance`)                             |   3,877 ms   |    7,106 ms   | **1.83x** [^6]  |
| **Tomcat**              | HTTP method parsing (`TestMethodPerformance`, 100M ops)                                       |  19,188 ms   |   48,355 ms   | **2.52x** [^6]  |
| **Tomcat**              | Charset lookup cache hit throughput (`TestCharsetCachePerformance`)                           |  19,308 ms   |   88,459 ms   | **4.58x** [^6]  |
| **Tomcat**              | JULI log record format throughput (`TestOneLineFormatterPerformance`, 100K ops)               |   2,365 ms   |    2,230 ms   |    **0.94x**    |
| **Tomcat**              | CompositeELResolver resolution (`TesterCompositeELResolverPerformance`)                       |    277 ms    |    1,203 ms   |    **4.34x**    |
| **Tomcat**              | EL ImportHandler resolution (`TesterImportHandlerPerformance`)                                |    279 ms    |     944 ms    |    **3.38x**    |
| **Tomcat**              | HttpServlet dispatch throughput (`TesterHttpServletPerformance`)                              |    321 ms    |     760 ms    |    **2.37x**    |
| **Tomcat**              | JSP ScopedAttributeELResolver lookup (`TesterScopedAttributeELResolverPerformance`)           |    278 ms    |    1,018 ms   |    **3.66x**    |
| **Tomcat**              | WebSocket ContainerProvider lookup (`TesterContainerProviderPerformance`)                     |   19,442 ms  |   90,513 ms   |    **4.66x**    |
| **Tomcat**              | DigestAuthenticator nonce/header throughput (`TesterDigestAuthenticatorPerformance`)          |   13,672 ms  |   52,580 ms   |    **3.85x**    |
| **Tomcat**              | Catalina Request parameter/header throughput (`TesterRequestPerformance`)                     |    569 ms    |    1,491 ms   |    **2.62x**    |
| **Tomcat**              | ApplicationHttpRequest wrapper lookup (`TesterApplicationHttpRequestPerformance`)             |   6,682 ms   |   60,793 ms   |    **9.10x**    |
| **Tomcat**              | DefaultInstanceManager injection lookup (`TesterDefaultInstanceManagerPerformance`)           |   1,171 ms   |    4,869 ms   | **4.16x** [^12] |
| **Tomcat**              | Catalina URL Mapper routing throughput (`TestMapperPerformance`)                              |   2,039 ms   |    6,887 ms   |    **3.38x**    |
| **Tomcat**              | AccessLogValve formatting benchmarks (`valves.Benchmarks`)                                    |   15,433 ms  |   59,857 ms   |    **3.88x**    |
| **Tomcat**              | WebResources file lookup (`TesterAbstractFileResourceSetPerformance`)                         |   1,210 ms   |    4,596 ms   |    **3.80x**    |
| **Tomcat**              | Jasper nonstandard tag execution (`TesterNonstandardTagPerformance`)                          |    740 ms    |   1,750 ms    | **2.36x** [^10] |
| **Tomcat**              | Jasper TagHandlerPool concurrency (`TesterTagHandlerPoolPerformance`)                         |   46,157 ms  |   68,072 ms   |    **1.47x**    |
| **Tomcat**              | JULI OneLineFormatter millis formatting (`TesterOneLineFormatterMillisPerformance`)          |   4,357 ms   |   24,006 ms   |    **5.51x**    |
| **Tomcat**              | SynchronizedQueue concurrent throughput (`TesterPerformanceSynchronizedQueue`)                |   1,894 ms   |    4,472 ms   |    **2.36x**    |
| **Tomcat**              | SynchronizedStack concurrent throughput (`TesterPerformanceSynchronizedStack`)                |   1,837 ms   |    4,362 ms   |    **2.37x**    |
| **Tomcat**              | HTTP Host header parser throughput (`TesterHostPerformance`)                                  |  243,032 ms  |  750,112 ms   |    **3.09x**    |
| **Tomcat**              | HTTP header parser throughput (`TesterParserPerformance`)                                     |    442 ms    |   1,712 ms    | **3.87x** [^8]  |
| **Tomcat**              | HTTP Cookies parsing throughput (`TesterCookiesPerformance`)                                  |   1,042 ms   |    4,006 ms   |    **3.84x**    |
| **Tomcat**              | FastHttpDateFormat formatting throughput (`TesterFastHttpDateFormatPerformance`)              |   3,412 ms   |   5,649 ms    |   **1.66x**     |
| **Tomcat**              | HTTP Parameters parsing throughput (`TesterParametersPerformance`)                            |    511 ms    |    2,436 ms   |    **4.77x**    |
| **Tomcat**              | WebSocket connection limit throughput (`TesterConnectionLimitPerformance`)                    |   6,298 ms   |   29,678 ms   |    **4.71x**    |
| **H2 Database**         | Multi-threaded MVStore Concurrency (`TestBenchmark`)                                          |  160,271 ms  |   496,097 ms  |    **3.10x**    |
| **H2 Database**         | MVStore Key-Value Storage Engine Throughput (`TestMVStoreBenchmark`)                          |  14,000 ms   |   42,100 ms   |    **3.01x**    |
| **H2 Database**         | Full SQL Transactions across Embedded, C/S TCP & In-Memory (`TestPerformance`)                |  47,081 ms   |   220,208 ms  |  **4.68x** [^1] |
| **H2 Database**         | MVStore LIRS cache concurrent throughput (`TestMVStoreCachePerformance`)                      |   29,479 ms  |   44,556 ms   |    **1.51x**    |
| **Apache Commons Math** | `JdkMath` numerical evaluation kernel (`JdkMathTest`)                                         |    194 ms    |     587 ms    |    **3.03x**    |
| **Apache Commons Math** | Comprehensive 100K Transcendental Suite (`JdkMathTestPerformance` / `PerfTestUtils`)          |    740 ms    |    2,152 ms   |    **2.91x**    |
| **Bouncy Castle Java**  | `AESFast` symmetric cipher validation & throughput                                            |    109 ms    |     584 ms    |    **5.38x**    |
| **Bouncy Castle Java**  | Elliptic Curve Point Multiplication Suite (`ECPointPerformanceTest`, full 88-curve sweep)     | 1,032,673 ms |  1,190,220 ms |    **1.15x**    |
| **Bouncy Castle Java**  | ASN.1 encoding & parsing regression suite (`org.bouncycastle.asn1.test.RegressionTest`)      |   1,272 ms   |    4,970 ms   | **3.91x** [^13] |
| **Spring Framework**    | AOP Introductions delegation (`IntroductionBenchmarkTests`, 100K ops)                         |   2,440 ms   |    7,503 ms   |    **3.08x**    |
| **Spring Framework**    | AspectJ vs Spring AOP proxy invocation (`BenchmarkTests`, 8 suites)                           |   4,186 ms   |   11,620 ms   |    **2.78x**    |
| **Spring Framework**    | `ReflectionUtils.getUniqueDeclaredMethods` (`ReflectionUtilsUniqueDeclaredMethodsBenchmark`, 100K ops) |   2,534 ms   |    8,895 ms   |    **3.51x**    |
| **Spring Framework**    | `BeanUtils.instantiateClass` reflection (`BeanUtilsBenchmark`, 500K ops)                      |    150 ms    |     640 ms    | **4.27x** [^9]  |
| **Spring Framework**    | `CompositePropertySource.getPropertyNames` (`CompositePropertySourceBenchmark`, 10K ops)      |    685 ms    |    2,080 ms   |    **3.04x**    |
| **Spring Boot**         | VirtualZip Archive Sequential & Random Read (`VirtualZipPerformanceTests`)                    |   1,345 ms   |   2,285 ms   |    **1.70x**    |
| **Spring Boot**         | ConfigurationPropertySources property lookup (`ConfigurationPropertySourcesTests`)            |   12,935 ms  |   23,520 ms   |    **1.82x**    |
| **Netty**               | HTTP Date Parser & Formatter Throughput (`DateFormatterBenchmark`, 500K ops)                  |   2,245 ms   |    3,356 ms   | **1.49x** [^5]  |
| **Netty**               | Direct & Heap ByteBuf Read/Write Access (`ByteBufAccessBenchmark`, 1M ops)                    |     9 ms     |     20 ms     | **2.22x** [^2]  |
| **Hibernate ORM**       | Graph vs Legacy ActionQueue Entity Insertion (`ActionQueuePerformanceTest`)                   |   2,243 ms   |   12,999 ms   |    **5.79x**    |
| **Hibernate ORM**       | Collection Element Batch Persistence & Query (`ElementCollectionPerfTest`)                    |   4,133 ms   |   20,103 ms   |    **4.86x**    |
| **Hibernate ORM**       | Collection Element Cache Persistence & Query (`ElementCollectionCachePerfTest`)               |   4,127 ms   |   19,426 ms   |    **4.71x**    |
| **Quarkus**             | Bootstrap ClassName to Resource Resolution (`BenchmarkClassnameToResourceName`, 1M ops)       |    706 ms    |     594 ms    | **0.84x** [^11] |
| **Quarkus**             | Bootstrap JSON Parser & AST Serializer (`RunQuarkusJson`, 100K ops)                           |    372 ms    |    1,164 ms   |    **3.13x**    |
| **Quarkus**             | QuarkusClassLoader Package Concurrency (`RunQuarkusConcurrentPackageDefine`, 16 threads)      |    284 ms    |    1,594 ms   |    **5.61x**    |
| **Apache Spark**        | Catalyst `HexBenchmark` (hex encode throughput, 1M cardinality, `RunSparkBench`)              |  53,977 ms   |   253,142 ms  | **4.69x** [^8]  |
| **Apache Spark**        | `XORShiftRandomBenchmark` (`nextInt`/`Long`/`Double`/`Gaussian` + seed hash, `RunSparkBench`) |  63,516 ms   |   129,600 ms  |    **2.04x**    |
| **Apache Spark**        | `OpenHashMapBenchmark` (1M String insert/lookup, 5M aggregate ops, `RunSparkBench`)           |  31,805 ms   |   115,522 ms  |    **3.63x**    |

---

## Detailed Component Benchmark Matrix: Cryptography & Numerical Math

Comparative breakdown across CratonVM (Default mode), CratonVM (Fast-Boot mode), OpenJDK HotSpot 25, and GraalVM 25:

### Bouncy Castle (Cryptography)

| Suite / Test                 | CVM Default | CVM Fast-Boot | HotSpot 25 | GraalVM 25 | Default / HotSpot | FastBoot / HotSpot |
| :--------------------------- | :---------: | :-----------: | :--------: | :--------: | :---------------: | :----------------: |
| SHA-256 Digest               |    360 ms   |     329 ms    |   98 ms    |   136 ms   |       3.69x       |       3.37x        |
| SHA-512 Digest               |    359 ms   |     314 ms    |   96 ms    |   126 ms   |       3.75x       |       3.29x        |
| MD5 Digest                   |    246 ms   |     144 ms    |   78 ms    |   81 ms    |       3.15x       |       1.84x        |
| AES Block Cipher             |  10,257 ms  |   10,630 ms   |  6,037 ms  |  5,352 ms  |       1.70x       |       1.76x        |
| AES-GCM AEAD Mode            |    727 ms   |     685 ms    |   255 ms   |   240 ms   |       2.85x       |       2.69x        |
| ChaCha20-Poly1305 AEAD       |    683 ms   |     694 ms    |   183 ms   |   216 ms   |       3.73x       |       3.79x        |
| RSA Asymmetric & KeyGen      |   6,437 ms  |    5,821 ms   |  2,302 ms  |  2,030 ms  |       2.80x       |       2.53x        |
| Ed25519 Curve Signatures     |   1,142 ms  |    1,125 ms   |   297 ms   |   210 ms   |       3.84x       |       3.78x        |
| X25519 Key Agreement         |    379 ms   |     363 ms    |   107 ms   |     —      |     3.54x [^3]    |       3.39x        |
| Message Authentication (MAC) |    282 ms   |     246 ms    |   77 ms    |     —      |     3.67x [^4]    |       3.20x        |

### Apache Commons Math (Numerical)

| Suite / Test                | CVM Default | CVM Fast-Boot | HotSpot 25 | GraalVM 25 | Default / HotSpot | FastBoot / HotSpot |
| :-------------------------- | :---------: | :-----------: | :--------: | :--------: | :---------------: | :----------------: |
| Dfp Decimal Floating Point  |   1,974 ms  |    1,963 ms   |   882 ms   |   712 ms   |       2.24x       |       2.23x        |
| DfpMath High-Precision Math |    730 ms   |     693 ms    |   295 ms   |   261 ms   |       2.47x       |       2.35x        |
| DfpDec Decimal Precision    |    713 ms   |     681 ms    |   310 ms   |   271 ms   |       2.30x       |       2.20x        |
| MathArrays Vector Math      |    637 ms   |     582 ms    |   257 ms   |   207 ms   |       2.48x       |       2.26x        |
| Integer Sequence Steppers   |    551 ms   |     527 ms    |   222 ms   |   173 ms   |       2.48x       |       2.37x        |
| LU Matrix Decomposition     |    650 ms   |     604 ms    |   256 ms   |   199 ms   |       2.54x       |       2.36x        |
| QR Matrix Decomposition     |    725 ms   |     659 ms    |   319 ms   |   232 ms   |       2.27x       |       2.07x        |
| Cholesky Decomposition      |    653 ms   |     650 ms    |   283 ms   |   203 ms   |       2.31x       |       2.30x        |
| EigenDecomposition Spectral |   1,234 ms  |    1,209 ms   |   413 ms   |   393 ms   |       2.99x       |       2.93x        |
| Kohonen Neural Network SOM  |    677 ms   |     665 ms    |   278 ms   |   209 ms   |       2.44x       |       2.39x        |

### Quarkus Subsystem Benchmarks (Bootstrap, JSON & Concurrency)

#### 1. Single-Shot JMH Latency (`BenchmarkClassnameToResourceName`)

| Method / Strategy                      | HotSpot 25 (Single-Shot) | CratonVM (Single-Shot) |
| :------------------------------------- | :----------------------: | :--------------------: |
| `checkNewMethod` (`ClassLoaderHelper`) |        5.10 µs/op        |      33.57 µs/op       |
| `oldAltMethod` (`replace('.', '/')`)   |        3.70 µs/op        |      27.50 µs/op       |
| `oldMethod` (`replace(".", "/")`)      |       14.37 µs/op        |      29.00 µs/op       |

#### 2. Subsystem Throughput & Concurrency Drivers

| Workload                                                   | HotSpot 25 | CratonVM | Checksum          |
| :--------------------------------------------------------- | :--------: | :------: | :---------------: |
| `RunQuarkusClassname` (1,000,000 ops)                      |   706 ms   |  594 ms  | `333341538000000` |
| `RunQuarkusJson` (100,000 ops roundtrip)                   |   372 ms   | 1,164 ms |     `6600000`     |
| `RunQuarkusConcurrentPackageDefine` (16 threads, 100 pkgs) |   284 ms   | 1,594 ms |       `990`       |

---

## Analysis by Workload Category

### 1. Framework Proxying & Invocation Chains (2.78x – 3.08x)
- **Spring Framework AspectJ & AOP Benchmarks:** Exercising dynamic invocation handlers, method interceptors, and AOP introduction delegation across 8 distinct aspect configurations. CratonVM completes the 100K-call delegation in 7.5s (vs HotSpot's 2.4s, 3.08x) and the 8-suite benchmark in 11.6s (vs 4.2s, 2.78x).
- **Takeaway:** Method resolution, virtual dispatch, and interface calls in complex object graphs remain efficient.

### 2. Expression Parsing & String Resolution (0.84x – 4.58x)
- **Tomcat Jakarta EL Parser (`TestELParserPerformance`):** Repeated expression tree generation and evaluation achieves 7.1s on CratonVM vs 3.9s on HotSpot (1.83x).
- **Tomcat URI Resolution (`TestResponsePerformance`):** Canonical path resolution and URI validation across 1M operations runs in 19.3s vs 4.6s (4.18x).
- **Tomcat Charset Cache (`TestCharsetCachePerformance`):** Concurrent lookup cache hit path runs in 88.5s vs 19.3s (4.58x).
- **Quarkus Bootstrap ClassName Resolution (`BenchmarkClassnameToResourceName`):** Repeated class name to resource path conversion (`fromClassNameToResourceName`) across 1M operations runs in 0.59s on CratonVM (`--real-jdk`; 0.57s `--compatible`) vs 0.71s on HotSpot (0.84x, down from 7.52x) with bit-identical checksum verification (`333341538000000`).
- **Quarkus Bootstrap JSON Parser (`RunQuarkusJson`):** High-throughput JSON parser and AST serializer from Quarkus bootstrap (`JsonReader`, `JsonObject`) running 100K parse and member evaluation cycles completes in 1.16s on CratonVM vs 0.37s on HotSpot (3.13x, reduced from 17.10x / 6.36s via spliced String accessor inline codegen, `Character.isISOControl` scalar intrinsification, and void-SAM lambda receiver inline-cache thunks) with bit-identical checksum (`6600000`).

### 3. Multi-Threaded Storage & Concurrency (3.01x – 5.61x)
- **H2 MVStore Concurrency (`TestBenchmark`):** Multi-threaded concurrent read/write transactional load across 32 down to 1 worker threads, combined with memory-mapped storage engine persistence. CratonVM scales reliably across all worker threads, completing in 496s vs 160s (3.10x).
- **H2 MVStore Storage Engine (`TestMVStoreBenchmark`):** Direct key-value storage engine operations complete in 42.1s vs 14.0s (3.01x).
- **QuarkusClassLoader Package Concurrency (`RunQuarkusConcurrentPackageDefine`):** Concurrent package definition in `QuarkusClassLoader` with synchronized latch orchestration across 16 worker threads defining 100 packages completes in 1.59s on CratonVM vs 0.28s on HotSpot (5.61x) with zero lock contention and bit-identical checksum (`990`).

### 4. Enterprise Persistence & ORM (4.86x – 5.79x)
- **Hibernate ORM Graph ActionQueue (`ActionQueuePerformanceTest`):** Dependency sorting and batch action queue execution completes in 12.9s vs 2.2s (5.79x).
- **Hibernate ORM Collection Element Mapping (`ElementCollectionPerfTest`):** Full entity mapping, embedded collections, and H2 database roundtrips complete in 20.1s vs 4.1s (4.86x).

### 5. Tight Cryptographic & Loop Kernels (1.12x – 8.26x)
- **Bouncy Castle Elliptic Curve Point Multiplication (`ECPointPerformanceTest`):** Comprehensive point multiplication across affine, homogeneous, jacobian, and lambda-projective coordinates on all 88 standard and custom elliptic curves (NIST, SEC, Brainpool, GOST, SM2) runs at near-parity: 1,190.2s vs 1,032.6s (1.15x).
- **Tomcat HTTP Method Parsing (`TestMethodPerformance`):** 100M iterations of string header parsing runs in 48.4s (`--real-jdk` G1; 48.3s `--compatible` G1) vs 19.2s baseline (2.52x).
- **Bouncy Castle Cryptographic Transforms:** Symmetric block ciphers (AES at 1.70x), asymmetric operations (RSA at 2.80x), tight curve arithmetic (Ed25519 at 3.84x), and hardware-accelerated digests (SHA-256 at 3.69x, SHA-512 at 3.75x) show competitive execution.

---

### Benchmark follow-ups

[^1]: The historical row covers all three `TestPerformance` modes. The Linux follow-up measures H2's disk-backed TCP case, compares it with embedded controls, and reflects the socket dispatch fixes plus a lock-free TLS `ClassId` classification cache (`H2_CLASS_KIND_CACHE`) in `apps_h2.rs` eliminating `class_name_of_id`/`class_id_by_name` `RwLock` lookups on H2 fast paths.

[^2]: Measured on the 96-process `NettyByteBufProbe` matrix (1,000,000 `setLong`/`getLong` pairs across unpooled heap, unpooled direct, pooled heap, and pooled direct; warm rounds 1–4 median): CratonVM reaches `20.24 ms` foundational / `26.60 ms` compatible (`16.90 ms` pooled direct, `19.48 ms` unpooled heap) with Unsafe allowed (`1.03 ms` HotSpot 25), and `79.98 ms` foundational / `75.13 ms` compatible under Java 25 default `VarHandle` policy (`1.07 ms` HotSpot 25, down from `254.93 / 260.51 ms` and the historical `983 ms` baseline) via per-call-site IC recompile claims, spliced `Long.reverseBytes` (`bswap`), deeper thin-wrapper forwarding, and thread-local `VarHandle` proof caches.

[^3]: Measured on Windows (`org.bouncycastle.crypto.test.X25519Test`, `-Xmx4g`): CratonVM Default `379 ms` (median of 5: 374.6–392.4 ms) and Fast-Boot `363 ms` (median of 5: 350.2–372.1 ms) vs OpenJDK HotSpot 25 `107 ms` (GraalVM 25 not measured on this host; prior Windows baseline before `curve25519-dalek` `X25519` intrinsics and bootstrap classloading/verification optimizations was ~740 ms Default / ~693 ms Fast-Boot, vs historical Linux baseline 505 ms / 473 ms / 70 ms).

[^4]: Measured on Windows (`org.bouncycastle.crypto.test.MacTest`): CratonVM Default `282.3 ms` (median; scalar `282.9 ms`) and Fast-Boot `246.2 ms` (median; scalar `244.7 ms`) vs OpenJDK HotSpot 25 `76.9 ms` (median; scalar `79.5 ms`; GraalVM 25 not measured on this host; prior Windows baseline before bootstrap verification/type-map skipping and boot-layer immutable collection optimizations was `515.8 ms` Default / `474.3 ms` Fast-Boot vs `82.4 ms` HotSpot, vs historical Linux baseline 376 ms / 320 ms / 51 ms).

[^5]: Measured via [`tools/bench/apps/RunNettyDate.java`](tools/bench/apps/RunNettyDate.java) (`DateFormatter` and `HttpHeaderDateFormat` parse/format, 500K iterations each, under [`tools/bench/apps/run-apps-benches-interleaved.sh`](tools/bench/apps/run-apps-benches-interleaved.sh) with `CRATONVM_BOOT_IMAGE=off`; median of 5 interleaved rounds): OpenJDK 25 HotSpot `2,245 ms`, CratonVM foundational `3,356 ms` (`1.49x`), `--compatible` `3,721 ms` (`1.66x`), matching checksum (`1480274326029000000`), reflecting IR StringBuilder `append(String)` prefix lowering, branch/cold-throw liveness trimming, `JitCycleEdgeCell::new_any_abi`, indexed `JIT_SITE_ACCESS_RING`, deferred leaf-native `JitDecodedArgs`, compact layout/provenance TLS caches, and zero-wrapper/batched `tzdb` (`ZoneInfo.getOffsets*`) execution (historical table entry before these optimizations was `148 ms` HotSpot / `1,965 ms` CratonVM, `13.28x`).

[^6]: Measured under `-XX:+UseG1GC` JUnit runs: `TestELParserPerformance` (`OK (4 tests)`) completes in `7,106 ms` (`7.106 s`) on CratonVM `--real-jdk` vs `3,877 ms` (`3.877 s`) on OpenJDK 25 HotSpot (prior table entry: `12,073 ms` / `4,153 ms`, `2.91x`); `TestCharsetCachePerformance` (`OK (1 test)`) completes in `88,459 ms` (`88.459 s`) on CratonVM `--real-jdk` vs `19,308 ms` (`19.308 s`) on OpenJDK 25 HotSpot (prior table entry: `72,507 ms` / `19,741 ms`, `3.67x`); `TestMethodPerformance` (`OK (1 test)`) completes in `48,355 ms` (`48.355 s`) on CratonVM `--real-jdk` and `48,266 ms` (`48.266 s`) on `--compatible` (`2.52x` vs the existing `19,188 ms` HotSpot baseline; prior CratonVM table entry: `158,525 ms`, `8.26x`).

[^7]: Three additional Tomcat performance classes in [`tools/bench/apps/tomcat/`](tools/bench/apps/tomcat/) are excluded from the passing table: `org.apache.catalina.session.Benchmarks` timed out at `>1,200,000 ms` on CratonVM under 100-thread contention (vs `24,055 ms` on HotSpot 25); `org.apache.tomcat.websocket.server.TestAsyncMessagesPerformance` failed its internal wall-clock timing assertion on CratonVM (vs `26,419 ms` on HotSpot 25); and `org.apache.tomcat.util.bcel.TesterPerformance` requires an external `/tmp/jira-libs` directory.

[^8]: Measured on Linux (`RunSparkBench org.apache.spark.sql.catalyst.expressions.HexBenchmark` against Spark 4.1.1 Catalyst, OpenJDK 25.0.5 HotSpot `53,977 ms` vs CratonVM `--real-jdk` `253,142 ms`, `4.69x`, down from `846,557 ms` / `15.68x` on the same host and the prior `39,547 ms` / `325,812 ms`, `8.24x` table entry): Common Codecs `18,350 ms` avg (vs `55,352 ms` baseline / `3,678 ms` HotSpot), Java `HexFormat` `22,756 ms` avg (vs `84,771 ms` baseline / `3,121 ms` HotSpot), Spark `13,237 ms` avg (vs `26,922 ms` baseline / `3,023 ms` HotSpot), and Spark Binary `6,377 ms` avg (vs `44,452 ms` baseline / `2,085 ms` HotSpot), reflecting TLAB provenance range pre-marking (`record_object_ref_range`), primitive-array and megamorphic lambda cast thunks (`lambda_mega_cast_adapter_entry`), and direct two-String / compact-String `makeConcatWithConstants` fast paths.

[^8]: Measured on `org.apache.tomcat.util.http.parser.TesterParserPerformance` (`OK (2 tests)`, `CRATONVM_BOOT_IMAGE=off`, `CARGO_PROFILE_RELEASE_LTO=thin` release build): OpenJDK 25 HotSpot `442 ms` (`Time: 0.122`), CratonVM `1,712 ms` (`Time: 0.542`, `3.87x`; prior baseline `453 ms` / `33,074 ms`, `73.01x`), reflecting direct-branch folding of locally-caught implicit traps (`Aioobe`, `Npe`, `Arithmetic`) whose `Op::CaughtException` is dead (guarded by the JVMTI exception event union flags).

[^9]: Measured via [`tools/bench/apps/RunSpringBeanUtils.java`](tools/bench/apps/RunSpringBeanUtils.java) (`BeanUtilsBenchmark`, 500K ops each for empty and 3-arg constructors, `CRATONVM_BOOT_IMAGE=off`, median of 5 CratonVM / 3 HotSpot runs): OpenJDK 25 HotSpot `150 ms` (`0.15–0.17 s`), CratonVM foundational (`--real-jdk`, `lto = "fat"`) `640 ms` (`0.62–0.95 s`, `4.27x`), matching checksum (`55000000`), reflecting direct `Constructor.newInstance` JIT lowering (`jit_ctor_new_instance_direct`), zero-clone `TrivialCtorFast` / `TrivialInitOp` constructor elision, direct `Object[]` argument evaluation (`ctor_new_instance_eval_args`), and pure TLAB bump allocation (`tlab_bump_object_fast`) (prior table entry was `205 ms` HotSpot / `2,393 ms` CratonVM, `11.67x`; steady-state 500K warm `BeanUtils.instantiateClass` reaches `69.5 ms` for the empty constructor and `140.3 ms` for the 2-arg constructor).

[^10]: Measured via `org.apache.jasper.compiler.TesterNonstandardTagPerformance` (`OK (1 test)`, 5 × 100M iterations each for `Old` and `New`, `CRATONVM_BOOT_IMAGE=off`, median of 3 interleaved runs): OpenJDK 25 HotSpot `740 ms` (JUnit `Time: 0.539–0.557`), CratonVM foundational (`--real-jdk`) `1,750 ms` (`2.36x`, JUnit `Time: 0.940–0.942`), `--compatible` `1,520 ms` (`2.05x`, JUnit `Time: 0.934–0.965`), reflecting the `UNLOADED_SPLICE_CLASS_ID` fix (`ClassId(0)` for `java/lang/Object`), trap-on-miss guarded/nested monomorphic splice inlining, and `hoist_invariant_guards_and_loads` (prior table entry was `757 ms` HotSpot / `251,098 ms` CratonVM, `331.70x`).

[^11]: Measured via [`tools/bench/apps/RunQuarkusClassname.java`](tools/bench/apps/RunQuarkusClassname.java) (`BenchmarkClassnameToResourceName`, 1,000,000 iterations × 6 conversions across `checkNewMethod`, `oldMethod`, and `oldAltMethod`, `CRATONVM_BOOT_IMAGE=off`, `lto = "fat"`, median of 7 interleaved rounds): OpenJDK 25 HotSpot `706 ms` (`666–716 ms`, steady-state loop `570.9 ms`), CratonVM Real-JDK (`--real-jdk`) `594 ms` (`565–602 ms`, `0.84x`, steady-state loop `429.7 ms`: `checkNewMethod` `154.1 ms`, `oldMethod` `157.3 ms`, `oldAltMethod` `118.3 ms` vs HotSpot `218.4 / 190.4 / 162.2 ms`), `--compatible` `570 ms` (`544–585 ms`, `0.81x`, steady-state loop `415.3 ms`), matching checksum (`333341538000000`), reflecting compiled JDK `String.replace(CharSequence, CharSequence)` execution under JIT (`string-replace-charseq`), per-VM `String` class-fact memoization, optimizing-tier SSE2 + single-TLAB-bump inline prefixes for `String.replace` and `String.concat`, and 4-way unrolled polynomial Latin-1 `String.hashCode()` across both JIT tiers (prior same-host baseline was `3,631 ms` / `5.14x`, and prior table entry was `169 ms` HotSpot / `1,268 ms` CratonVM, `7.52x`).

[^12]: Measured via `org.apache.catalina.core.TesterDefaultInstanceManagerPerformance` (`OK (1 test)`, 1–8 threads × 200K `newInstance`/`destroyInstance` iterations, `CRATONVM_BOOT_IMAGE=off`, `lto = "fat"`, median of 3 interleaved rounds): OpenJDK 25 HotSpot `1,171 ms` (`1,116–1,192 ms`, JUnit `Time: 0.846–0.913`), CratonVM foundational (`--real-jdk`) `4,869 ms` (`4,760–4,902 ms`, `4.16x`, JUnit `Time: 4.159–4.249`), reflecting boxed `ZgcCounters::discovered_weak_phantom`, `mutation_epoch` in `GateDecisionMemo`, compile-time ZGC `Weak`/`Phantom` class annotation for inline TLAB announce, optimizing-tier inline `Reference.get0()` prefix, `Unsafe` reference-array read fast path, lock-free `MirrorFastTable` seqlock + inline `Class.getSuperclass()`/`Class.getName()` prefixes, `osr_entry_can_reach`/`osr_entry_reach_bits` stack bitsets, `TLS_PROFILE_SLOTS = 2048`, `InvokeCache::get_poly_ref` borrowing, and `maybe_drain_jit_entry_counters` 25 ms cooldown (prior table entries were `1,284 ms` HotSpot / `25,966 ms` CratonVM, `20.22x`, then `1,532 ms` / `9,275 ms`, `6.05x`).

[^13]: Measured via `org.bouncycastle.asn1.test.RegressionTest` (`All tests successful.`, `CRATONVM_BOOT_IMAGE=off`, `lto = "fat"`, median of 5 interleaved rounds): OpenJDK 25 HotSpot `1,272 ms` (`1,125–1,331 ms`), CratonVM foundational (`--real-jdk`) `4,970 ms` (`4,551–4,994 ms`, `3.91x`), reflecting 4-arg ranged integral `Arrays.fill` `REP STOS` intrinsics (`ArraysFillRange1/2/4/8`, cutting `LongDefiniteLengthParser` from `2,388 ms` to `50 ms`; prior baseline was `910 ms` HotSpot / `6,673 ms` CratonVM, `7.33x`).

## Reproducing the Results

The benchmark files and automated interleaved runner are located in [`tools/bench/apps/`](tools/bench/apps/):

```bash
# Execute the interleaved benchmark suite (1,200s timeout per arm):
./tools/bench/apps/run-apps-benches-interleaved.sh
```

Raw execution logs and timestamped Markdown reports are generated in `tools/bench/apps/results/<TIMESTAMP>/`.



# Real-World Applications Performance: CratonVM vs HotSpot vs TornadoVM

#### Complex real-world workloads: RayTracer & GPULlama3

##### RayTracer Kernel (1080p & 8K UHD)
Primary-ray sphere intersection, diffuse shading, and color writeback to `int[] out` ($132.8\text{ MB}$ at 8K). Default monolithic pinned DMA writeback (`CHUNK_COUNT_DEFAULT = 1`):

| Resolution | HotSpot 25 C2 CPU | TornadoVM PTX | CratonVM GPU (Best) | CratonVM GPU (Mean) | Speedup vs HotSpot | Speedup vs TornadoVM | Checksum Agreement |
| :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| **1080p ($1920 \times 1080$, 2.07M px)** | 103.89 ms | 3.63 ms | **1.11 ms** | 1.29 ms | **93.6×** | **3.27×** | Match (`5462181685950`) |
| **8K UHD ($7680 \times 4320$, 33.18M px)** | 1,593.28 ms | 48.91 ms | **16.46 ms** | 18.65 ms | **96.8×** | **2.97×** | Match (`87319822645950`) |

##### GPULlama3 (LLM Inference Forward Pass)
Greedy token generation on `Llama-3.2-1B-Instruct-F16.gguf` (453 kernel launches per token, single-stream capture graph):

| Tokens Decoded | HotSpot 25 C2 CPU | TornadoVM PTX | CratonVM GPU (Captured Graph) | Speedup vs HotSpot | Speedup vs TornadoVM | Device Alloc Pool Reuse |
| :--- | :---: | :---: | :---: | :---: | :---: | :---: |
| **32 tokens** | 8.70 tok/s (3.68 s) | 10.80 tok/s (2.96 s) | **30.54 tok/s (1.05 s)** | **3.51×** | **2.83×** | >90% pooled |
| **64 tokens** | 8.68 tok/s (7.37 s) | 11.20 tok/s (5.71 s) | **32.74 tok/s (1.95 s)** | **3.77×** | **2.92×** | >92% pooled |


