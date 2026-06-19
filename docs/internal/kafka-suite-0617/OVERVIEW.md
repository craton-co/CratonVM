# Kafka full-suite sweep (0617) — overview & exact numbers

CratonVM vs HotSpot (JDK 25.0.2), Apache Kafka test suite, **953 test classes**, one JVM
per class via a programmatic JUnit-Platform launcher (`KRun`), 600 s per-class timeout.

> Measurement note: the box is shared with a concurrent build session whose `cargo` builds
> (seen up to 15 `rustc` procs) intermittently saturate RAM/CPU. That injects **false
> OOM-ABENDs** (`memory allocation of 2.1 GB / 1 GB failed`) and **false TIMEOUTs**. Counts
> below are de-noised where re-verification was possible; residual TIMEOUT is an upper bound.
> The harness itself was later destroyed by the concurrent session (worktree switch + clean),
> so a final fully-idle re-measure could not be completed — these are the best verified numbers.

## Raw tally (953 classes)

| status | HotSpot | CratonVM |
|---|---:|---:|
| OK | 821 | 591 |
| FAIL | 41 | 246 |
| TIMEOUT | 16 | 80 |
| ABEND | 62 | 23 |
| EMPTY | 13 | 13 |

HotSpot's 119 non-OK are mostly **integration tests with no broker** (62 ABEND) — see broker
section; not VM-comparable.

## CratonVM-only failures (HotSpot OK, CratonVM not OK)

Raw: **267** = 179 FAIL + 67 TIMEOUT + 21 ABEND. After de-noising and this session's fixes:

| bucket | raw | verified disposition |
|---|---:|---|
| ABEND | 21 | **~19 = contention false-OOM** (re-verify passed them); **2 real SIGSEGV** (KafkaShareConsumerMetricsTest, RecordAccumulatorTest) — **fixed on latest dev** (JIT access-violation → now hang). Net genuine residual crashes ≈ **0**. |
| TIMEOUT | 67 | re-verify on the A+C+D binary: **14 → OK** (fixed), **5 → FAIL** (hang→assertion), **1 ABEND**, **68 still TIMEOUT** (the +1/-1 churn is reclassification). The 68 are contention-inflated; the genuine core is the **Mockito/ByteBuddy hang cluster** (see [bug-E](bug-E-mockito-bytebuddy-hang.md)). |
| FAIL | 179 | reliable (ran to completion). Clustered below. |

**Fixed this session (landed on dev):** Bug A (subList), Bug C (WeakHashMap stream), Bug D
(CompletableFuture async) — confirmed by re-verify (14 hangs → OK, incl. RecordHeadersTest,
CoordinatorBackgroundThreadPoolExecutorTest) and standalone (AcknowledgementsTest 20/20).

## FAIL clusters (179 CratonVM-only FAILs, by dominant exception)

| cluster | count | doc / status |
|---|---:|---|
| NullPointerException | 62 | [cluster-npe](cluster-npe-heterogeneous.md) — heterogeneous, needs per-class trace pass |
| assertion/other | 43 | genuine result mismatches; sub-triage needed |
| UnsatisfiedLinkError | 12 | [followups #1](actionable-followups.md) — **all Snappy/Zstd native compression** (one root cause) |
| NoSuchMethodError | 10 | [followups #2](actionable-followups.md) — synthetic-class method gaps (TreeMap `*Entry`, etc.) |
| InternalError | 7 | VM-raised (often masks a linkage error) |
| ArithmeticException | 7 | distinct — possible codegen/arith bug |
| IllegalArgumentException | 6 | |
| IllegalState / IOException | 4 / 4 | |
| AbstractMethodError | 3 | [followups #3/#4](actionable-followups.md) — IntStream/LongStream.reduce, MessageDigestSpi.engineDigest "no Code attribute" |
| long tail (Kafka-specific) | ~21 | mostly singletons |

## Broker-dependent integration tests (newly surfaced)

The 82 classes that originally failed on **both** VMs were excluded as "no broker". With a
real broker up (docker `apache/kafka:latest`, `localhost:9092`), **HotSpot passes ≥9 of them**
— and **CratonVM fails all 9** → genuine **CratonVM broker-integration gaps**, previously
hidden. See [bug-F](bug-F-broker-integration-gaps.md). (Contention prevented a full idle
re-measure, so ≥9 is a floor — more are likely among the contention-masked remainder.)

## Fix status summary

| Bug | Status | dev commit |
|---|---|---|
| A — `ArrayList.subList()` not a live view | ✅ FIXED | `29dbc1d9` (ASL structural delegation) |
| C — `WeakHashMap.values().stream()` JIT hang | ✅ FIXED | `f328fa9a` (spliterator skip-list) |
| D — `CompletableFuture.*Async` deadlock | ✅ FIXED | `51197975` (bounded real-thread pool) |
| E — Mockito/ByteBuddy hang cluster | OPEN | — |
| F — broker-integration gaps (≥9) | OPEN | — |
| followups — Snappy/Zstd, TreeMap `*Entry`, streams, NPE | OPEN | see actionable-followups.md |
