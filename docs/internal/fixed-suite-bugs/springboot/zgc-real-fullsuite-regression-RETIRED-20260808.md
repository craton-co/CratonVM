# ZGC-real vs. Generational, full Spring Boot suite — RETIRED 2026-08-10

**Status: CLOSED.** Every ZGC-only regression the page recorded is either
root-caused and fixed here, or is a class with its own open page that fails the
same way under the default collector. Neither of the two defects behind them is
a ZGC defect: both are collector-agnostic bugs that ZGC is simply the first
backend to hit on every collection.

Original pages: `zgc-real-fullsuite-regression-20260808` (the clean comparison
after the two mark-word fixes) and its predecessor
[`zgc-real-fullsuite-regression-RETIRED-20260807.md`](zgc-real-fullsuite-regression-RETIRED-20260807.md)
(the first-ever ZGC full-suite run, against a binary that also had a heap
corruption bug). Their numbers are unchanged and not restated here; this record
says what each finding turned out to be.

## 1. Four of the fourteen ZGC-only rows (seven classes) — FIXED

**Root cause: ZGC had the WRITE half of the reference-array auto-box and no
read half.**

A reference-array element is a raw 8-byte pointer, so a non-`Object` `Value`
cannot be stored in one directly. Natives across the tree nonetheless use a
reference array as a generic `Value` store, and `GenerationalHeap` — and, since
`42ce72b18`, `G1Collector` — honour that by auto-boxing into a one-field
`AUTOBOX_CLASS_ID` wrapper and un-boxing on read.

`ZgcRealHeap::set_array_element` boxed. `ZgcRealHeap::get_array_element`
returned the **wrapper object**: an instance of a synthetic class with no name,
no methods, and no relation to the primitive it carries. That single missing
arm has two very different-looking faces, which is why the page read it as
several unrelated bugs:

```
list.stream().mapToLong(Long::longValue).toArray()
  default collector -> [66, 1234, -7]
  -XX:+UseZGC       -> [0, 0, 0]            <- wrapper written into a long[] slot,
                                               whose encoder has no arm for an
                                               object and stores 0

... .mapToLong(...).forEach(x -> print(x))
  -XX:+UseZGC       -> the wrapper's own ADDRESS, printed as a long

... .mapToLong(...).boxed().toArray()
  -XX:+UseZGC       -> [?@2, ?@3, ?@4]      <- the wrapper reaching Java, whose
                                               getClass().getName() reads
                                               `unknown_4294967295`
                                               (AUTOBOX_CLASS_ID is u32::MAX)
```

**Do not read that `?` as the same `?` in §2.** They are different objects with
the same face, and assuming otherwise cost this investigation a round: the
auto-box wrapper prints `unknown_4294967295` from `Class.getName()` and `?` from
`Object.toString`, while §2's receiver has an id that resolves to nothing at all.
The `?class_id=N` the `checkcast` message now carries is what tells them apart —
`4294967295` is this wrapper, anything else is not.

`42ce72b18`, which fixed the identical hole in G1, named this exact residual and
left it: *"Not fixed here: ZGC-real has the WRITE half and no read half… because
`gc/src/zgc.rs` does not currently compile at all under `--features zgc`."* That
build break was fixed on 2026-08-07; the read half is what remained.

Deterministic, identical with `--nojit`, and identical at `-Xmx 12g` where no
collection runs — so despite presenting as ZGC corruption it is not a
collection-time defect at all. `mapToInt` survived throughout, because
`Value::Int` is the only primitive the reference-array encoder round-trips.

Fixed by `ZgcRealHeap::autobox_payload`, mirroring `G1Collector::autobox_payload`
including its `is_object_address` screen — a reference-array element is a raw
word and a stale one could point anywhere, so the un-box must never dereference
an address the registry does not vouch for.

Classes this closed, `FAIL -> PASS` under `-XX:+UseZGC`, each with the failure
traced back to a zeroed or wrapper-valued primitive array:

| Class | Failure it produced |
|---|---|
| `VirtualZipDataBlockTests` | `centralRecordPositions.stream().mapToLong(Long::longValue).toArray()` returned `[0]`, so the virtual zip read its entry names from file offset 46 instead of 112 |
| `PropertiesMeterFilterTests` | `.mapToDouble(Double::doubleValue).toArray()` → `serviceLevelObjectiveBoundaries must contain only the values greater than 0. Found 0.0` |
| `PrometheusExemplarsAutoConfigurationTests` (both modules) | all-zero bucket array → `histogramClassicUpperBounds must be sorted and must not contain duplicates` |
| `OtlpExemplarsAutoConfigurationTests` (both modules) | all-zero bucket array → `ArrayIndexOutOfBoundsException: Index -1` |

Three more went `FAIL -> PASS` in the same build and stayed green on the rerun,
but are recorded as *unattributed*, not as this fix's:
`LazyTracingSpanContextTests` (`MockitoException: cannot mock …
io.micrometer.tracing.Span`), `JettyReactiveWebServerFactoryTests` (h2c
`IOException: protocol_error`) and `NettyReactiveWebServerFactoryTests` (h2c
`EOFException: Stream has been reset`). Mockito's plugin loading and the h2c
handshake both run through the stream pipeline, so this fix is a plausible
cause for all three — but plausible is not traced, and a green after a fix is
not evidence the fix caused it when the test could also have been flaky.

Regression oracle: `probes/G1ReferenceArrayValueProbe.java`, which gains a ZGC
arm and two checks that assert on the **class** of a re-boxed element, not only
its value — a value check alone passes vacuously against a backend that boxes
into a real `java.lang.Long`. `PROBE-FAILURES=8` before, `PROBE-OK` after, and
`PROBE-OK` on all three collectors after. Unit-level: four new tests beside
`G1Collector`'s three in `gc/src/zgc.rs`, including one pinning that a
non-object word in a reference array is returned rather than dereferenced.

## 2. The `ClassCastException: ? cannot be cast to …` cluster — FIXED

**Root cause: nothing invalidated the generated-`$ProxyN` cache when a class
was unloaded.** Not a GC bug either.

`PROXY_CLASS_CACHE` maps `(vm_identity, loader_namespace, ordered interface
ClassIds) -> generated $ProxyN ClassId`. When a user loader dies, the next
collection prunes its `defining_loader_store` row and
`unload_dead_class_metadata` removes the generated proxy class from the class
store — but the cache kept the row. The next `Proxy.newProxyInstance` with the
same key was handed the dead `ClassId` straight back, allocated an instance
against it, and the cast at the call site failed against a class that no longer
existed.

The cache's own doc comment already named that `?` as the symptom of handing out
a `ClassId` from a *foreign* class manager. This is the same face reached the
other way round: from the class manager that unloaded the id.

ZGC surfaced it because **every ZGC collection is a full mark**, so
`memory::roots::conditional_loader_metadata` is true on every cycle and loader
reclamation runs on every cycle. The generational collector enters that mode
only in its narrow full-mark windows, which is why the same three classes pass
under the default collector on the same binary.

| Class | `-XX:+UseZGC` before | after |
|---|---:|---:|
| `BatchJdbcAutoConfigurationTests` | 14 of 34 failed | 0 of 34 failed |
| `FreeMarkerAutoConfigurationReactiveIntegrationTests` | 2 of 7 failed | 0 of 7 failed |
| `OpenTelemetrySdkAutoConfigurationTests` | 15 of 21 failed | 0 of 21 failed |

(`BatchJdbcAutoConfigurationTests` shows as a 300s HANG in the parallel rerun
table below rather than a PASS — that is contention, not this defect: run alone
it is 34/34 green in 166s under ZGC.)

Two things made this one expensive to find, and both are now fixed in the
tooling rather than in a comment:

* The message said `?` and nothing else. `?` reads identically for a reclaimed
  header, a foreign layout domain and the synthetic auto-box wrapper of §1 — and
  the wrapper hypothesis was wrong here. Both the `checkcast` opcode and the
  lambda-instantiation check now print `?class_id=N`. That one change identified
  the bug on the next run: the failing cast reported `?class_id=3003`, and
  `CRATONVM_DBG_MIRRORPIN` had just logged
  `defining_loader_store cid=3003 … is_marked=false` in the same process.
* `CRATONVM_LOADER_UNLOAD=0` turned the failing class green in one run, which
  named the subsystem without naming the defect. An inert lever is not an
  elimination; the class id is what closed it.

## 3. `ZipContentTests` — NOT a ZGC regression

The page recorded a `CRASH` (a catchable `OutOfMemoryError: Java heap space
(native primitive array of length 8192)` at 234.6s) and offered ZGC
fragmentation as the mechanism. That reading does not survive the class's own
history: it is `HANG`/`CRASH`/`PASS` in turn under the **default** collector
across the 2026-07/08 runs, including one `OutOfMemoryError` at
`alloc_array length 8192` under Generational on 2026-08-04. It is a
borderline-capacity class at `-Xmx 2g` on every backend, tracked by its own open
page,
[`zipcontenttests-gc-pressure-timeout-not-disk-capacity-20260807.md`](../../../known-issues/springboot/zipcontenttests-gc-pressure-timeout-not-disk-capacity-20260807.md),
and it stays there. It HANGs on both arms of this round's rerun.

## 4. Everything else in the 24-row table — not ZGC regressions

* `SpringApplicationTests` (`PASS -> HANG`) and `Log4J2LoggingSystemTests`
  (`FAIL -> HANG`) move the same way under G1, and Log4J2 has its own page
  (`log4j2-logback-loggingsystemtests-modifiedclasspath-throughput-hang-20260807.md`).
* `HikariDataSourceConfigurationTests` and `QuartzEndpointWebIntegrationTests`
  each already have an open page from 2026-08-07 and reproduce on both arms.
* `CachesEndpointWebIntegrationTests`,
  `SessionAutoConfigurationEarlyInitializationIntegrationTests` and
  `BasicErrorControllerIntegrationTests` all fail with
  `ApplicationContextException: Failed to start bean 'webServerStartStop'` —
  the embedded-server-start family, which fails on the default arm too.
* The five `HANG -> PASS` improvements and `ConfigDataEnvironmentPostProcessor-
  IntegrationTests` are timeout-boundary movement, exactly as the page
  suspected — see the table below, where three of them now land on ZGC's side
  and `ConfigurationPropertySourcesTests` takes 2158s under ZGC against 5156s
  under the default collector, both inside the runner's 5400s override.

## What this page's method got wrong, for the next one

The page attributed 14 ZGC-only regressions to "ZGC-real's non-moving,
whole-arena mark-sweep" and flagged the observability cluster as worth triaging
together. The clustering instinct was right and the mechanism was wrong twice
over: **neither** fix is in the collector. A 30-line Java probe
(`list.stream().mapToLong(Long::longValue).toArray()`) reproduced the whole
observability cluster in under a second, with `--nojit`, at `-Xmx 12g` — before
any suite class was re-run. That probe was reachable from the page's own
`PropertiesMeterFilterTests` sample, which quotes `Found 0.0`: an all-zero
`double[]` out of a stream is a statement about the array, not about the
collector, and it costs one probe to ask which.

## Verification

Same binary, same 26 classes (the 24 in the page's table, plus the two classes
that exist in two modules), `-XX:+UseZGC` vs. the default, `-Xmx 2g`,
300s/class, 3-way parallel on the Windows box:
`apps/spring-boot-suite-runner/.suite/results/zgcres-final-zgc-20260810/` vs.
`.../zgcres-final-default-20260810/`.

**No functional ZGC-vs-default difference is left.** Every class either agrees
on both arms or moves only across the timeout boundary — and in both
directions, which is what says "budget", not "collector":

| Class | ZGC | default |
|---|---|---|
| `VirtualZipDataBlockTests` | PASS 1.7s | PASS 1.3s |
| `PropertiesMeterFilterTests` | PASS 10s | PASS 6s |
| `LazyTracingSpanContextTests` | PASS 16s | PASS 10s |
| `OtlpExemplarsAutoConfigurationTests` (x2) | PASS | PASS |
| `PrometheusExemplarsAutoConfigurationTests` (x2) | PASS | PASS |
| `FreeMarkerAutoConfigurationReactiveIntegrationTests` | PASS 20s | PASS 12s |
| `OpenTelemetrySdkAutoConfigurationTests` | PASS 25s | PASS 26s |
| `JettyReactiveWebServerFactoryTests` | PASS 54s | PASS 32s |
| `NettyReactiveWebServerFactoryTests` | PASS 30s | PASS 30s |
| `ConfigDataEnvironmentPostProcessorIntegrationTests` | PASS 256s | PASS 289s |
| `ConfigurationPropertySourcesTests` | PASS 2158s | PASS 5156s |
| `Log4J2LoggingSystemTests` | HANG | HANG |
| `SpringApplicationTests` | HANG | HANG |
| `ZipContentTests` | HANG | HANG |
| `HikariDataSourceConfigurationTests` | HANG | HANG |
| `KafkaAutoConfigurationTests` | HANG | HANG |
| `QuartzEndpointWebIntegrationTests` | HANG | HANG |
| `CachesEndpointWebIntegrationTests` | FAIL 13 | FAIL 13 |
| `SessionAutoConfigurationEarlyInitializationIntegrationTests` | FAIL 1 | FAIL 1 |
| `BatchJdbcAutoConfigurationTests` | HANG 300s | PASS 251s |
| `CloudFoundryActuatorAutoConfigurationTests` | PASS 290s | HANG 300s |
| `JettyWebServerFactoryCustomizerTests` | PASS 164s | HANG 300s |
| `ConfigurationMetadataAnnotationProcessorTests` | PASS 294s | HANG 1200s |
| `BasicErrorControllerIntegrationTests` | FAIL 363s | HANG 1800s |

The last five rows are the boundary movement, three of them in ZGC's favour.
`BatchJdbcAutoConfigurationTests` is the one that goes the other way, and it is
3-way-parallel contention, not the collector: run alone on an otherwise idle
host it PASSes on both arms at 166s (ZGC) and 170s (default).

Spot-check on the companion G1 page's three G1-only regressions, same binary,
`-XX:+UseG1GC`: `BindConverterTests` now PASSes (8.9s).
`ChildManagementContextInitializerAotTests` and `CacheAutoConfigurationTests`
still fail, the latter on Infinispan JCache context startup — a different
family, and the G1 page stays open for them.

`cargo test -p cratonvm-gc --features zgc`: 1456 lib tests + every integration
target green.

Known gap: `cargo test -p cratonvm-native-builtins --lib` does not compile on
dev at all (619 errors, e.g. `alloc_concurrent_synthetic` not in scope at
`lib.rs:42292`), confirmed pre-existing by stashing this branch's change and
rebuilding the target. The three unit tests added beside `PROXY_CLASS_CACHE`
are therefore written but unrun; the end-to-end oracle for §2 is the three
Spring Boot classes above.
