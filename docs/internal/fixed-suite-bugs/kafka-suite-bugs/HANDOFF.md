# Kafka unit-suite on CratonVM — bug handoff

Running the full Apache Kafka 3.7.0 **kafka-clients** unit suite (362 test classes,
~7,900 tests) under CratonVM vs HotSpot 25, to find and fix CratonVM-only crashes,
hangs, and wrong results.

## Harness (worktree `C:\craton\CratonVM-ksuite`, branch `fix/kafka-suite-loop`)
- `apps/kafka/tests/` — the runner:
  - `KRun.java` — programmatic JUnit Platform launcher; prints one flushed
    `RESULT <class> found=.. succ=.. fail=.. ... status=..` line per class (the
    JUnit ConsoleLauncher path is unreliable on CratonVM — 0-byte reports + lost
    stdout on `System.exit`).
  - `run-suite.sh <vm> <tag> [args]` — `GRAN=class` runs one JVM **per class** so a
    crash/hang can't abort other classes; `GRAN=package` batches. Env:
    `TIMEOUT_PER_CLASS`, `CLASS_LIST`, `ONLY`. Sets
    `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1` so the external timeout is the sole hang
    detector.
  - `compare.sh <hs-tag> <cv-tag>` — CratonVM-only defects vs HotSpot (skips classes
    HotSpot also fails). `summary.sh <tag>` — class-status + test-level totals.
  - `cp.txt` — full 132-entry classpath (kafka libs + mockito/hamcrest/bc/jqwik from
    the gradle cache + junit-console-standalone). `classes.txt` — the 362 FQCNs.
  - `repro/` — minimal standalone repros (KFGet, Sched, TimeoutRepro, …).
- Build: `build-wt.bat` (worktree-local `target/`, seeded libffi). Kill any running
  `cratonvm.exe` before rebuilding (the .exe lock silently skips relink).

## Baseline (HotSpot 25)
- 293 classes OK; 58 FAIL (env/DNS-dependent) + 11 LOADERR (missing optional deps) —
  those 69 are **excluded** from comparison.
- Test-level: ~7,931 found / ~7,135 passed.

## Status snapshot (CratonVM, partial — full per-class run in progress)
- After the bug-07 fix, a 108-class partial pass: 59 OK, 38 FAIL, 6 ABEND, 5 TIMEOUT;
  test-level 870 passed / 554 failed of 1,538. Full 362-class totals pending.

## Bugs (ranked by impact)

| # | Title | Sev | Status |
|---|-------|-----|--------|
| [07](bug-07-timeout-scheduledfuture-cancel-ame.md) | `@Timeout` → `Future.cancel` AbstractMethodError (synthetic ScheduledFuture) | Critical | **FIXED** |
| [09](bug-09-mockito-inline-mockmaker-selfattach.md) | Mockito inline mock-maker self-attach fails (307 fails, 15+ classes) | **Critical/Dominant** | **FIXED** (10 layers; `7af98b29`). mock()+stub()+verify() all work. Finish = primitive/array `Class.getModifiers()`=0→`PUBLIC\|FINAL\|ABSTRACT` (ByteBuddy `isPackagePrivate` ignoreAlso dropped every primitive-sig method) + `LinkedList.<init>(Collection)`/`stream()`/`spliterator()` overlay. `FutureRecordMetadataTest` 2/2==HotSpot |
| [08](bug-08-completablefuture-synthetic-layout-real-subclass.md) | `CompletableFuture` synthetic layout corrupts real `KafkaFuture` (admin/producer/consumer) | High | **FIXED** (completeExceptionally/isCompletedExceptionally/complete(null)/chaining layout-agnostic; all 5 admin `*ResultTest` pass) |
| [10](bug-10-metadatasnapshot-noclassdeffound-clinit.md) | `NoClassDefFoundError: MetadataSnapshot` (masked `<clinit>` failure) | High | **FIXED** (harness cp skew → kafka-clients 3.7.2; not a VM bug) |
| [11](bug-11-metrics-metricvalue-npe.md) | metrics `metricValue on null` (metric lookup returns null) | High | **FIXED** (`TimeUnit.toMillis` overflow → saturate) |
| [14](bug-14-parameterized-empty-stream-underrun.md) | `@ParameterizedTest` empty arg stream → test under-run (22 vs 129) — `ArrayList.removeAll` over-removal on a full backing array | High | **FIXED** (`8bb77a77`, two-pass removeAll/retainAll) |
| [19](bug-19-bufferpool-blocking-hang.md) | `BufferPoolTest` hang — `new Thread(runnable)` no-ops | High | **RESOLVED (env)** — boot JDK was 17 (`JAVA_HOME`); `Thread$FieldHolder` is JDK 19+, so `holder.task` unset → Runnable-target threads never run → every wait/notify/park/Condition handoff hangs. Fix: boot JDK ≥19 (`--java-home <JDK25>`). No VM code change. Same cause as bug-23 `AbstractCoordinatorTest`. |
| [20](bug-20-silent-abnormal-exit-rc127-rc1.md) | silent rc=127/rc=1 abnormal VM exit (no diagnostic) | High | open |
| [12](bug-12-nodeapiversions-apikey-npe.md) | `apiKey on null` (enum `values()` / generated-enum null hole) | Medium | **FIXED** (`ImplicitLinkedHashCollection.toArray()` null holes) |
| [13](bug-13-timeoutextension-double-proceed.md) | `TimeoutExtension` double-`proceed` JUnitException (post-bug-07) | Medium | **FIXED** (by bug-08; FenceProducersHandlerTest 4/4) |
| [15](bug-15-decompress-zlib-gzip.md) | record GZIP decompress failure — `FilterOutputStream.close()` didn't propagate to wrapped stream | Medium | **FIXED** (`8bb77a77`) |
| [17](bug-17-assignor-assignment-mismatch.md) | consumer assignor wrong assignment (map/set ordering) | Medium | open |
| [18](bug-18-fetcher-topicid-zeroed.md) | fetch `Uuid` topicId zeroed / `PartitionData` mismatch | Medium | open |
| [16](bug-16-unsupportedop-remove-immutable.md) | `UnsupportedOperationException: remove` (wrong collection mutability) | Low | open |

Pre-existing docs from earlier sweeps: [01](bug-01-jit-discovery-hang.md)–[06](bug-06-discovery-findRepeatableAnnotations.md),
[workstream-1](workstream-1-jit-throughput.md).

## Biggest wins to chase next
1. **bug-09 (Mockito self-attach)** — unblocks ~300 failures / 15+ classes at once;
   make ByteBuddy `ByteBuddyAgent.install()` / `java.lang.instrument` self-attach work.
2. **bug-08 (CompletableFuture layout)** — unblocks the whole KafkaFuture cluster
   (admin/producer/consumer result handling); rework CF natives to encode state in
   slot 0 (`AltResult` for exceptions), layout-independent.
3. **bug-10 (MetadataSnapshot clinit)** — recover the masked `ExceptionInInitializerError`.

> Each bug doc lists the affected classes found so far; **append** newly-matching
> classes from the completing full run rather than creating new docs.
