# G1 vs. Generational, full Spring Boot suite — rerun 2026-08-08 on a clean binary

**Status: OPEN — characterized, not root-caused.** Supersedes
[`g1-fullsuite-regression-RETIRED-20260807.md`](../../internal/fixed-suite-bugs/springboot/g1-fullsuite-regression-RETIRED-20260807.md)
(that page's own findings are still valid and closed; this is a fresh
comparison, not a reopening). The prior comparison ran against a binary that
turned out to have a near-total heap-corruption bug (`gen_heap::read_slot:
corrupt Value cell`, fixed 2026-08-07 by two commits — the inline allocator's
mark-word write and a thin-unlock quartet clobber). This run is the first
clean, apples-to-apples G1-vs-Generational comparison since those fixes
landed.

## Summary

Same binary (`cratonvm-*-20260807e.exe`, built at `dev@7ea883be8`), same
1975-class Windows full suite, `-Xmx 2g`, 300s/class timeout, single shard
(no sharding-related host-contention variable) — the only difference between
runs is `-XX:+UseG1GC` vs. the default (unspecified → Generational):

| | Generational (default) | G1 |
|---|---:|---:|
| PASS | 1853 (93.8%) | 1854 (93.9%) |
| FAIL | 65 | 69 |
| HANG | 13 | 8 |
| CRASH | 0 | 0 |
| EMPTY | 43 | 43 |
| BOTH-FAIL | 1 | 1 |
| Wall time | 32412s (~9.0h) | 28213s (~7.8h) |
| **Total** | **1975** | **1975** |

Only **14 classes changed status** — far tighter than the pre-fix
comparison's ~50, and G1 is essentially at parity with Generational now (69
vs 65 FAIL, and *fewer* HANGs: 8 vs 13). This matches the "G1 is wired into
the safepoint driver" maturity note from the original page.

Results:
`apps/spring-boot-suite-runner/.suite/results/craton-fullsuite-default-20260807e/all-jit/results.tsv`
vs.
`apps/spring-boot-suite-runner/.suite/results/craton-fullsuite-g1-20260807e/all-jit/results.tsv`.

## The 14 changes

| Class | Module | Generational | G1 |
|---|---|---|---|
| `SpringApplicationTests` | `core/spring-boot` | PASS | HANG |
| `BindConverterTests` | `core/spring-boot` | PASS | FAIL |
| `ChildManagementContextInitializerAotTests` | `module/spring-boot-actuator-autoconfigure` | PASS | FAIL |
| `CacheAutoConfigurationTests` | `module/spring-boot-cache` | PASS | FAIL |
| `HikariDataSourceConfigurationTests` | `module/spring-boot-jdbc` | PASS | HANG |
| `ConfigurationMetadataAnnotationProcessorTests` | `configuration-metadata/spring-boot-configuration-processor` | HANG | PASS |
| `ConfigDataEnvironmentPostProcessorIntegrationTests` | `core/spring-boot` | HANG | PASS |
| `ConfigurationPropertySourcesTests` | `core/spring-boot` | HANG | PASS |
| `CloudFoundryActuatorAutoConfigurationTests` | `module/spring-boot-cloudfoundry` | HANG | PASS |
| `JettyWebServerFactoryCustomizerTests` | `module/spring-boot-jetty` | HANG | PASS |
| `KafkaAutoConfigurationTests` | `module/spring-boot-kafka` | HANG | PASS |
| `Log4J2LoggingSystemTests` | `core/spring-boot` | FAIL | HANG |
| `QuartzEndpointWebIntegrationTests` | `module/spring-boot-quartz` | HANG | FAIL |
| `BasicErrorControllerIntegrationTests` | `module/spring-boot-webmvc` | HANG | FAIL |

5 genuine regressions (PASS -> FAIL/HANG), 6 classes that improve
(HANG -> PASS), 3 that swap one bad status for another (still not a clean
pass either way).

## Cross-reference: 9 of these 14 are the *identical* change under ZGC too

See the companion doc, retired 2026-08-10:
[`zgc-real-fullsuite-regression-RETIRED-20260808.md`](../../internal/fixed-suite-bugs/springboot/zgc-real-fullsuite-regression-RETIRED-20260808.md).
Two **collector-agnostic** defects were root-caused there, and one of them is
live for G1 too, so this page's own residuals are worth re-measuring before
being triaged as G1 behaviour: a generated-`$ProxyN` cache that kept handing
out `ClassId`s after class unloading had removed them, which surfaces as
`ClassCastException: ? cannot be cast to …` in any collector that reaches
`memory::roots::conditional_loader_metadata` — G1 included, via
`gc_quiescence::class_unload_marking()`. (The other, ZGC's missing
reference-array un-box, is G1's already-fixed `42ce72b18` hole seen from the
read side, so G1 is unaffected by it.)
`ConfigurationMetadataAnnotationProcessorTests`, `SpringApplicationTests`,
`ConfigurationPropertySourcesTests`, `Log4J2LoggingSystemTests`,
`CloudFoundryActuatorAutoConfigurationTests`,
`JettyWebServerFactoryCustomizerTests`, `KafkaAutoConfigurationTests`,
`QuartzEndpointWebIntegrationTests` and `BasicErrorControllerIntegrationTests`
all move in the exact same direction under both G1 and ZGC. That is strong
evidence these 9 are **collector-agnostic** — either timeout-boundary noise
(the 6 HANG->PASS ones are all classes close enough to the 300s ceiling under
Generational that a different collector's timing nudges them under budget —
consistent with the "borderline-slow, tipped over under load" pattern this
suite has already documented for several of these exact classes) or a shared
non-Generational-path bug (the 3 that get worse: `Log4J2LoggingSystemTests`
FAIL->HANG, `QuartzEndpointWebIntegrationTests` and
`BasicErrorControllerIntegrationTests` HANG->FAIL). Not independently
confirmed which explanation applies to which class this round.

Two classes appear in both diffs but with a *different* concrete outcome —
worth noting as still-linked, not coincidence:

- `ConfigDataEnvironmentPostProcessorIntegrationTests`: G1 HANG->PASS, ZGC
  HANG->FAIL. Same starting HANG, different collector-specific landing spot.
- `HikariDataSourceConfigurationTests`: G1 PASS->HANG (300.183s, TIMEOUT), ZGC
  PASS->FAIL (256.111s, `AssertionError`). This class already has an open,
  unresolved doc from 2026-08-07
  (`hikaridatasourceconfigurationtests-pool-start-hang-20260807.md`) — the
  ZGC arm finally erroring out at 256s rather than running the full 300s is
  consistent with the same underlying slow/stuck mechanism, not a new one.

## Not investigated further this round

None of the 5 G1-only regressions were individually root-caused — this doc
is a characterization pass, matching the ZGC companion doc's scope. Worth a
follow-up triage pass the way the earlier 08-06/08-07 default-GC HANG/FAIL
classes got one.

**Update 2026-08-10** (from the ZGC page's retirement round, same binary,
`-XX:+UseG1GC`, 2-way parallel): `BindConverterTests` now PASSes (8.9s).
`ChildManagementContextInitializerAotTests` (296s) and
`CacheAutoConfigurationTests` (337s, 15 failed) still fail; the latter fails on
Infinispan JCache context startup
(`UnsatisfiedDependencyException` behind `infinispanAsJCacheWithConfig`), which
is a different family from anything on this page. `HikariDataSourceConfiguration-
Tests` and `SpringApplicationTests` reproduce on the DEFAULT collector too in
that round, so they are not G1-only either. That leaves this page with two
genuinely open rows, not five.

## Affected classes

See the table above. Full per-class raw data in the two `results.tsv` files
linked at the top.
