# ZGC-real vs. Generational — first full Spring Boot suite comparison, 2026-08-07

**Status: OPEN — characterized, not root-caused.** First time this
comparison has ever been run: the `zgc` cargo feature did not compile at
all before this session (see "Build was broken" below), so no prior full-suite
data exists for this backend.

## Summary

Same binary (`cratonvm.exe` built at `dev@275887cb3` + the fix below), same
1975-class Windows-box full suite, same 4-way shard split, `-Xmx 2g`,
300s/class timeout — the only variable is `-XX:+UseZGC` vs. the default
(unspecified → Generational):

| | Generational (default) | ZGC-real (`-XX:+UseZGC`) |
|---|---:|---:|
| PASS | 1902 (96.3%) | 1860 (94.2%) |
| HANG | 18 | 49 |
| FAIL | 11 | 22 |
| EMPTY | 43 | 43 |
| BOTH-FAIL | 1 | 1 |
| **Total** | **1975** | **1975** |

EMPTY and BOTH-FAIL are identical counts on both arms (as expected — those
are vacuous-skip and shared-with-HotSpot-baseline outcomes, neither of which
should be collector-sensitive). The delta is concentrated entirely in
HANG and FAIL: **50 classes changed status**, 46 of them regressions.

Results:
`apps/spring-boot-suite-runner/.suite/results/craton-fullsuite-windows-20260806-s{1..4}/all-jit/results.tsv`
(Generational) vs.
`apps/spring-boot-suite-runner/.suite/results/craton-fullsuite-zgc-20260807-s{1..4}/all-jit/results.tsv`
(ZGC-real).

## Build was broken before this session

`cargo build --features cratonvm-vm/zgc` failed with `E0063: missing field
layout_domain in initializer of ZgcRealHeap` — `with_capacity()` was never
updated when the `layout_domain` field was added to the struct (the
`Heap`/`G1Collector`/`GenHeap` siblings all got it). Invisible to normal CI
since the feature is off by default and nothing in the tree builds it.
Fixed by initializing it the same way the three sibling collectors do
(`cratonvm_types::FIRST_LAYOUT_DOMAIN`) — see the commit on this branch.
Confirmed after the fix: `-XX:+UseZGC` no longer falls back to Generational
with an "unsupported garbage collector" warning, and the backend actually
runs (`GcBackend::Zgc` is selected, `vm/src/vm/vm_init.rs:1323`).

## 35 classes: PASS -> HANG

All timed out at the 300s ceiling (not slow-but-progressing the way
`JooqAutoConfigurationTests` is — these never appeared in the Generational
run's results at all under any status but PASS). Overwhelmingly
`*AutoConfigurationTests` classes — the shape that builds and tears down
many Spring `ApplicationContext`s in a tight loop, i.e. an allocation-churn
workload:

```
configuration-metadata/spring-boot-configuration-processor  ConfigurationMetadataAnnotationProcessorTests
core/spring-boot                                             ConfigDataEnvironmentPostProcessorIntegrationTests
core/spring-boot                                             ConfigurationPropertySourcesTests
module/spring-boot-actuator-autoconfigure                    ChildManagementContextInitializerAotTests
module/spring-boot-amqp                                      RabbitAutoConfigurationTests
module/spring-boot-amqp                                      RabbitStreamConfigurationTests
module/spring-boot-batch-jdbc                                BatchJdbcAutoConfigurationTests
module/spring-boot-cache                                     CachesEndpointWebIntegrationTests
module/spring-boot-cache                                     CacheAutoConfigurationTests
module/spring-boot-cloudfoundry                               CloudFoundryReactiveActuatorAutoConfigurationTests
module/spring-boot-cloudfoundry                               CloudFoundryActuatorAutoConfigurationTests
module/spring-boot-data-jdbc                                 DataJdbcRepositoriesAutoConfigurationTests
module/spring-boot-data-redis                                DataRedisAutoConfigurationTests
module/spring-boot-devtools                                  LocalDevToolsAutoConfigurationTests
module/spring-boot-graphql                                   GraphQlWebFluxAutoConfigurationTests
module/spring-boot-graphql                                   GraphQlWebMvcAutoConfigurationTests
module/spring-boot-grpc-client                               GrpcClientAutoConfigurationTests
module/spring-boot-grpc-server                               GrpcServerAutoConfigurationTests
module/spring-boot-hazelcast                                 HazelcastAutoConfigurationServerTests
module/spring-boot-hibernate                                 HibernateJpaAutoConfigurationTests
module/spring-boot-jackson                                   JacksonAutoConfigurationTests
module/spring-boot-jms                                       JmsAutoConfigurationTests
module/spring-boot-kafka                                     KafkaAutoConfigurationTests
module/spring-boot-liquibase                                 LiquibaseAutoConfigurationTests
module/spring-boot-quartz                                    QuartzAutoConfigurationTests
module/spring-boot-security                                  UserDetailsServiceAutoConfigurationTests
module/spring-boot-security                                  ReactiveManagementWebSecurityAutoConfigurationTests
module/spring-boot-security                                  JerseyEndpointRequestIntegrationTests
module/spring-boot-security                                  MvcEndpointRequestIntegrationTests
module/spring-boot-security-oauth2-resource-server            OAuth2ResourceServerAutoConfigurationTests
module/spring-boot-security-oauth2-resource-server            ReactiveOAuth2ResourceServerAutoConfigurationTests
module/spring-boot-security-saml2                            Saml2RelyingPartyAutoConfigurationTests
module/spring-boot-session-jdbc                               JdbcSessionAutoConfigurationTests
module/spring-boot-webmvc                                     WebMvcAutoConfigurationTests
module/spring-boot-webmvc                                     BasicErrorControllerIntegrationTests
```

## Working hypothesis, not confirmed

`docs/GC.md` already documents ZGC-real as "a memory-backed, non-moving,
whole-heap stop-the-world mark-sweep over one arena... No colored pointers,
no load barriers, no concurrency, no compaction." A full **stop-the-world,
whole-arena** mark-sweep on every collection, with no generational
short-lived-object fast path, is the textbook shape that turns "many short
`ApplicationContext` lifecycles" into a throughput cliff — every collection
scans the *entire* live set instead of just a small young generation.
`gc_rearm`'s own doc comment in `gc/src/zgc.rs` independently describes a
previously-fixed livelock mode ("a live set that sits above the static 75%
threshold... ran a full STW mark-sweep per allocation: a livelock-grade GC
storm") — plausible that these 35 classes hit a related-but-different
throughput wall rather than that exact fixed bug, but **not verified**: no
GC-count/pause-time instrumentation was pulled from any of these 35 logs
this round.

## 11 classes: PASS -> FAIL

Fast failures (1-24s, not timeouts), so a different mechanism than the HANG
group:

```
core/spring-boot                          BindConverterTests
core/spring-boot                          ServletListenerRegistrationBeanTests
loader/spring-boot-loader                 VirtualZipDataBlockTests
module/spring-boot-data-neo4j             DataNeo4jAutoConfigurationTests
module/spring-boot-jetty                  JettyReactiveWebServerFactoryTests
module/spring-boot-jooq                   SqlDialectLookupTests
module/spring-boot-micrometer-metrics     PropertiesMeterFilterTests
module/spring-boot-micrometer-tracing-brave  OtlpExemplarsAutoConfigurationTests
module/spring-boot-micrometer-tracing-brave  PrometheusExemplarsAutoConfigurationTests
module/spring-boot-reactor-netty          NettyReactiveWebServerFactoryTests
module/spring-boot-tomcat                 TomcatReactiveWebServerFactoryTests
```

Checked one closely — `VirtualZipDataBlockTests` (2/2 tests fail,
`java.nio.file.NoSuchFileException` plus a byte-content `AssertionFailedError`
whose actual zip bytes are missing a `META-INF/` entry present in the
expected bytes). **This does not look GC-related on its face** — it reads
like a fixture/working-directory or zip-construction content bug, not
memory corruption or a collector artifact. Not confirmed either way, and the
other 10 in this group were not individually inspected. Worth checking
whether any of the 11 reproduce under Generational with the same binary run
back-to-back (ruling out unrelated host-load flake) before assuming all 11
share one cause with each other, or with the HANG group.

## 4 classes: HANG (Generational) -> PASS (ZGC)

All four are jOOQ-family classes, the same family already tracked as a
severe-throughput HANG under Generational (the retired
`jooqautoconfigurationtests-timeout-regression-20260805` write-up). That HANG
was root-caused and FIXED on 2026-08-07 — `Method.getModifiers()` rebuilt the
declaring class method table on every call, 39 us per call on jOOQ 1003-method
`DefaultDSLContext` — so the Generational side of this comparison is stale for
the jOOQ family and would need re-running to mean anything.
Plausibly just noise near the 300s timeout boundary (these are exactly the
kind of borderline-slow classes that flip status run to run), not a genuine
ZGC advantage — not investigated further.

```
module/spring-boot-jooq       JooqFlywayDatabaseInitializationTests
module/spring-boot-jooq-test  JooqTestIntegrationTests
module/spring-boot-jooq-test  JooqTestPropertiesIntegrationTests
module/spring-boot-jooq-test  JooqTestWithAutoConfigureTestDatabaseIntegrationTests
```

## Not done this round

- No per-class GC pause/count instrumentation pulled (would confirm or
  refute the throughput-cliff hypothesis for the HANG group).
  `CRATONVM_DBG_JIT_METHOD_STATS`/GC-report env vars exist elsewhere in this
  codebase; none were applied here.
- The 11 FAIL classes were not individually triaged past the one sample
  above.
- No re-run for reproducibility on either arm — this is one data point per
  class per collector, not an averaged/repeated measurement. Given
  `docs/GC.md`'s and `docs/gc-tuning.md`'s own "experimental / research
  vehicle, do not depend on in production" framing for this backend, that
  tradeoff (breadth over depth) seemed the right one for a first-ever
  characterization run.

## Affected classes

See the three lists above (35 HANG regressions, 11 FAIL regressions, 4
HANG->PASS flips). Full per-class raw data in the results.tsv files linked
at the top.
