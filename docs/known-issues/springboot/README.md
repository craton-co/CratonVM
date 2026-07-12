# Spring Boot suite — known issues

Found triaging `FAIL`s from the first full Spring Boot 4.1.0-SNAPSHOT test
suite run against CratonVM (1975 classes; see
[[project_spring_boot_suite_runner_20260711]] and
`apps/spring-boot-suite-runner/RESULTS-20260711.md`). 508 classes FAILed;
five clusters were characterized here, together accounting for ~146 of them
directly (many more indirectly, via the "Unstarted application context"/
"Failed to parse configuration class" wrapper noise these root causes
produce). Two have since been retired (fixed or found not to reproduce on
current dev, see table); three remain OPEN (97 classes). The rest is an
uncharacterized long tail of smaller/individual differences not yet
clustered.

| Doc | Classes | Severity | Status |
|---|---:|---|---|
| [`onclasscondition-npe-cast-to-string-array-cluster.md`](onclasscondition-npe-cast-to-string-array-cluster.md) | 75 (348 occurrences) | CRITICAL | OPEN, precisely pinned to `OnClassCondition.addAll` |
| `DisposableBeanAdapter` "Invalid destruction signature" | 34 | HIGH | **RESOLVED 2026-07-12** — moved to [`../../internal/fixed-suite-bugs/spring-disposablebeanadapter-invalid-destruction-signature-cluster-RESOLVED.md`](../../internal/fixed-suite-bugs/spring-disposablebeanadapter-invalid-destruction-signature-cluster-RESOLVED.md); a direct probe against current dev found the destroy-method reflection path works fine, closing the cluster (no distinct residual identified) |
| `sun.misc.Unsafe$MemoryAccessOption` NPE | 26 | HIGH | **FIXED/RETIRED 2026-07-12** — moved to [`../../internal/fixed-suite-bugs/spring-boot-unsafe-memoryaccessoption-npe-FIXED.md`](../../internal/fixed-suite-bugs/spring-boot-unsafe-memoryaccessoption-npe-FIXED.md); same bug independently found+fixed via a concurrent Keycloak investigation, canonical doc is `testsuite-model-unsafe-putorderedlong-memoryaccessoption-npe-FIXED.md` in the same directory |
| [`httpclient-builder-dead-registration-abstractmethoderror.md`](httpclient-builder-dead-registration-abstractmethoderror.md) | 13 | HIGH | OPEN, fully root-caused (dead `synthetic-jdk` registration) |
| [`zip-filedatablock-bulk-bytebuffer-put-aioobe.md`](zip-filedatablock-bulk-bytebuffer-put-aioobe.md) | 9 (whole `spring-boot-loader` module) | HIGH | OPEN, characterized |
| [`flyway-cglib-heap-corruption-sigsegv-crash.md`](flyway-cglib-heap-corruption-sigsegv-crash.md) | 1 (`FlywayAutoConfigurationTests`) | HIGH (SIGSEGV) | OPEN, characterized — likely a new occurrence of the tracked HIB-CV-32 heap-corruption family |
| [`testcompiler-annotation-classes-not-found-cluster.md`](testcompiler-annotation-classes-not-found-cluster.md) | 7 (`spring-boot-configuration-processor`) | MEDIUM | OPEN, characterized |

## HANG-rerun follow-up (150 classes @ 1500s timeout)

Reran the original 150 HANG classes at 5x the timeout (see
`apps/spring-boot-suite-runner/RESULTS-20260711.md` "HANG rerun" section):
90 still hung, 51 turned FAIL, 7 PASS, 2 CRASH. Of the 51 FAILs: 7 are the
`testcompiler-annotation-classes-not-found-cluster.md` above; ~16 more
overlap with the `onclasscondition-npe-cast-to-string-array-cluster.md` and
the two retired clusters (destroy-method resolution, `MemoryAccessOption`)
above — those classes just needed more wall time to *reach* the
already-known failure instead of timing out first. The remaining ~28 are a
scattered long tail (mostly one-off `AssertionError`s, a couple of
`GroovyRuntimeException`s from the Thymeleaf layout-dialect integration, two
`ParameterResolutionException`s for `WebTestClient` autowiring) — not yet
clustered; no single dominant pattern found. Of the 2 CRASHes: one
(`FlywayAutoConfigurationTests`) is the new heap-corruption doc above; the
other (`OriginTrackedYamlLoaderTests`, `NoClassDefFoundError:
org/junit/platform/commons/util/ExceptionUtils`) is a **runner classpath
artifact** (likely pathing-jar manifest truncation for a very long
classpath), not a CratonVM bug — not filed.

## Methodology note (runner fix, not a bug)

The first run (`full-20260711b`) did **not** set `CRATONVM_REAL_NET_SOCKETS`,
so any test doing a real socket bind (e.g. `HazelcastAutoConfigurationClientTests`)
hit the already-known, already-fixed-behind-a-flag
`java.net.ServerSocket.socketLock` null bug (see `reference_server_socket_gap`
in memory) rather than a new issue. `run-spring-boot-suite.ps1` now sets
`CRATONVM_REAL_NET_SOCKETS`/`CRATONVM_REAL_AQS`/
`CRATONVM_DISABLE_DEFAULT_WATCHDOG`/`CRATONVM_ROOTSNAP_CACHE` for craton runs,
matching `tomcat-suite-runner`'s convention — a rerun with the fixed runner
would likely shift some FAIL/HANG counts.
