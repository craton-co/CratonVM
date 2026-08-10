# ZGC-real vs. Generational, full Spring Boot suite — rerun 2026-08-08 on a clean binary

**Status: OPEN — characterized, not root-caused.** Supersedes
[`zgc-real-fullsuite-regression-20260807.md`](zgc-real-fullsuite-regression-20260807.md)
as the current data point (that page is not wrong, just stale — it ran
against a binary with a near-total heap-corruption bug active). This run is
the first clean, apples-to-apples ZGC-vs-Generational comparison since the
two 2026-08-07 mark-word fixes landed (the inline allocator's unconditional
mark-word write, and a thin-unlock quartet clobber), and it also includes the
ZGC backend's own major rewrite that landed the same day (`gc/src/zgc/`
split into adapters/barrier/census/forwarding/generation/mark/metrics/page/
relocate/remembered/tlab/vaddr modules).

## Summary

Same binary (`cratonvm-*-20260807e.exe`, built at `dev@7ea883be8` with
`--features cratonvm-vm/zgc`), same 1975-class Windows full suite, `-Xmx 2g`,
300s/class timeout, single shard — only `-XX:+UseZGC` vs. the default
(unspecified → Generational) varies:

| | Generational (default) | ZGC-real |
|---|---:|---:|
| PASS | 1853 (93.8%) | 1844 (93.4%) |
| FAIL | 65 | 78 |
| HANG | 13 | 8 |
| CRASH | 0 | 1 |
| EMPTY | 43 | 43 |
| BOTH-FAIL | 1 | 1 |
| Wall time | 32412s (~9.0h) | 26709s (~7.4h) |
| **Total** | **1975** | **1975** |

**24 classes changed status** — dramatically fewer than the pre-fix
comparison's 50, and the gap to Generational's FAIL count narrowed from a
2x+ multiple to 78 vs 65 (~1.2x). Like G1, ZGC now has *fewer* HANGs than
Generational (8 vs 13). This is by far the healthiest ZGC-real result on
record for this suite.

Results:
`apps/spring-boot-suite-runner/.suite/results/craton-fullsuite-default-20260807e/all-jit/results.tsv`
vs.
`apps/spring-boot-suite-runner/.suite/results/craton-fullsuite-zgc-20260807e/all-jit/results.tsv`.

## The 24 changes

| Class | Module | Generational | ZGC |
|---|---|---|---|
| `SpringApplicationTests` | `core/spring-boot` | PASS | HANG |
| `VirtualZipDataBlockTests` | `loader/spring-boot-loader` | PASS | FAIL |
| `ZipContentTests` | `loader/spring-boot-loader` | PASS | CRASH |
| `BatchJdbcAutoConfigurationTests` | `module/spring-boot-batch-jdbc` | PASS | FAIL |
| `FreeMarkerAutoConfigurationReactiveIntegrationTests` | `module/spring-boot-freemarker` | PASS | FAIL |
| `HikariDataSourceConfigurationTests` | `module/spring-boot-jdbc` | PASS | FAIL |
| `JettyReactiveWebServerFactoryTests` | `module/spring-boot-jetty` | PASS | FAIL |
| `PropertiesMeterFilterTests` | `module/spring-boot-micrometer-metrics` | PASS | FAIL |
| `LazyTracingSpanContextTests` | `module/spring-boot-micrometer-tracing` | PASS | FAIL |
| `OtlpExemplarsAutoConfigurationTests` | `module/spring-boot-micrometer-tracing-brave` | PASS | FAIL |
| `PrometheusExemplarsAutoConfigurationTests` | `module/spring-boot-micrometer-tracing-brave` | PASS | FAIL |
| `OpenTelemetrySdkAutoConfigurationTests` | `module/spring-boot-opentelemetry` | PASS | FAIL |
| `NettyReactiveWebServerFactoryTests` | `module/spring-boot-reactor-netty` | PASS | FAIL |
| `SessionAutoConfigurationEarlyInitializationIntegrationTests` | `module/spring-boot-session` | PASS | FAIL |
| `ConfigurationMetadataAnnotationProcessorTests` | `configuration-metadata/spring-boot-configuration-processor` | HANG | PASS |
| `ConfigurationPropertySourcesTests` | `core/spring-boot` | HANG | PASS |
| `CloudFoundryActuatorAutoConfigurationTests` | `module/spring-boot-cloudfoundry` | HANG | PASS |
| `JettyWebServerFactoryCustomizerTests` | `module/spring-boot-jetty` | HANG | PASS |
| `KafkaAutoConfigurationTests` | `module/spring-boot-kafka` | HANG | PASS |
| `ConfigDataEnvironmentPostProcessorIntegrationTests` | `core/spring-boot` | HANG | FAIL |
| `CachesEndpointWebIntegrationTests` | `module/spring-boot-cache` | FAIL | HANG |
| `QuartzEndpointWebIntegrationTests` | `module/spring-boot-quartz` | HANG | FAIL |
| `BasicErrorControllerIntegrationTests` | `module/spring-boot-webmvc` | HANG | FAIL |
| `Log4J2LoggingSystemTests` | `core/spring-boot` | FAIL | HANG |

14 genuine regressions (PASS -> FAIL/HANG/CRASH), 5 that improve
(HANG -> PASS), 5 that swap one bad status for another.

## Two regressions checked in detail

**`ZipContentTests` — genuine `OutOfMemoryError`, likely ZGC-specific.**
CRASH at 234.6s. The `.err.log` shows a clean, catchable
`java.lang.OutOfMemoryError: Java heap space (native primitive array of
length 8192)` mid-test (`nestedZip64CanBeRead`, reading a zip64 nested-jar
stream through `BufferedReader`/`AssertJ`'s `Diff`), not a native crash — no
fatal-error report header, just a normal Java exception the harness records
as CRASH because the process then exits non-zero. This matches a hypothesis
already on file from the 2026-08-07 default-GC triage
(`zipcontenttests-gc-pressure-timeout-not-disk-capacity-20260807.md`, which
ruled out the disk-capacity theory and suspected GC/heap pressure at `-Xmx
2g`) — direct confirmation under ZGC specifically. Plausible mechanism:
ZGC-real's non-moving, whole-arena mark-sweep (`docs/GC.md`) has no
compaction, so fragmentation from this test's many nested-zip byte-array
allocations could exhaust usable space well before Generational's copying
collector would, at the same nominal heap size. Not confirmed with direct
fragmentation measurement.

**`VirtualZipDataBlockTests` — same exact bug as the pre-fix ZGC run,
completely unaffected by the mark-word fixes.** Byte-for-byte identical
signature to the 2026-08-07 (broken-binary) sighting: `NoSuchFileException`
on one test, and on the other an `AssertionFailedError` whose actual zip
bytes are missing a `META-INF/` directory entry (`[77, 69, 84, 65, 45, 73,
78, 70, 47]`) present in the expected bytes. Confirms this is a real,
persistent, ZGC-specific defect — not a heap-corruption artifact, since it
survived a binary rebuild that fixed the actual corruption bug. **Notably
not shared with G1 this round** (it was shared in the pre-fix 08-07
comparison) — see the companion G1 doc's diff table, this class is absent
from it. Not root-caused; a fixture/zip-construction content bug is still
the most likely shape given the missing-entry-not-corrupted-bytes signature.

## Cross-reference: 9 of these 24 are the identical change under G1 too

See [`g1-fullsuite-regression-20260808.md`](g1-fullsuite-regression-20260808.md)
for the full analysis. `ConfigurationMetadataAnnotationProcessorTests`,
`SpringApplicationTests`, `ConfigurationPropertySourcesTests`,
`Log4J2LoggingSystemTests`, `CloudFoundryActuatorAutoConfigurationTests`,
`JettyWebServerFactoryCustomizerTests`, `KafkaAutoConfigurationTests`,
`QuartzEndpointWebIntegrationTests` and `BasicErrorControllerIntegrationTests`
move in the same direction under both alternate collectors — likely
collector-agnostic (timeout-boundary noise for the HANG->PASS group, a
shared non-Generational-path bug for the 3 that get worse). The remaining 15
changes in this doc's table are ZGC-only.

The **micrometer/tracing cluster is worth a second look together**: 4 of the
14 ZGC-only regressions are in the observability family
(`PropertiesMeterFilterTests`, `LazyTracingSpanContextTests`,
`OtlpExemplarsAutoConfigurationTests` x2 across `micrometer-tracing-brave`
and `micrometer-tracing`). Spot-checked two: `PropertiesMeterFilterTests`
fails with `InvalidConfigurationException: serviceLevelObjectiveBoundaries
must contain only values greater than...` (looks like a numeric/floating-point
boundary check), `LazyTracingSpanContextTests` fails with a
`MockitoException` (a different mechanism — Mockito/ByteBuddy mock creation,
a family with several other documented CratonVM-vs-non-default-GC issues
elsewhere in this repo). Not confirmed to share one root cause across all
four; flagged as a cluster worth triaging together rather than four
independent one-offs.

## Not investigated further this round

The other 8 ZGC-only regressions (`BatchJdbcAutoConfigurationTests`,
`FreeMarkerAutoConfigurationReactiveIntegrationTests`,
`JettyReactiveWebServerFactoryTests`,
`OpenTelemetrySdkAutoConfigurationTests`,
`NettyReactiveWebServerFactoryTests`,
`SessionAutoConfigurationEarlyInitializationIntegrationTests`, plus the two
detailed above) were not individually root-caused this round — this doc is
a characterization pass. Worth a dedicated triage round the way the
2026-08-06/07 default-GC HANG/FAIL classes got one.

## Affected classes

See the table above. Full per-class raw data in the two `results.tsv` files
linked at the top.
