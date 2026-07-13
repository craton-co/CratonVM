# Spring Boot suite — known issues

Found triaging `FAIL`s from the first full Spring Boot 4.1.0-SNAPSHOT test
suite run against CratonVM (1975 classes; see
[[project_spring_boot_suite_runner_20260711]] and
`apps/spring-boot-suite-runner/RESULTS-20260711.md`). 508 classes FAILed;
five clusters were characterized here, together accounting for ~146 of them
directly (many more indirectly, via the "Unstarted application context"/
"Failed to parse configuration class" wrapper noise these root causes
produce). Most have since been retired (fixed or found not to reproduce on
current dev, see table). The rest is an uncharacterized long tail of
smaller/individual differences not yet clustered.

| Doc | Classes | Severity | Status |
|---|---:|---|---|
| `OnClassCondition.addAll` NPE-cast-to-`String[]` | 75 (348 occurrences) | CRITICAL | **FIXED/RETIRED 2026-07-13** — moved to [`../../internal/springboot/onclasscondition-npe-cast-string-array-cluster-FIXED.md`](../../internal/springboot/onclasscondition-npe-cast-string-array-cluster-FIXED.md); `@ConditionalOnClass`'s unresolvable-`Class`-element handling now defers to a `TypeNotPresentException` sentinel matching HotSpot, instead of a bare `null`. Verified against all 75/75 originally-affected classes |
| `DisposableBeanAdapter` "Invalid destruction signature" | 34 | HIGH | **RESOLVED 2026-07-12** — moved to [`../../internal/fixed-suite-bugs/spring-disposablebeanadapter-invalid-destruction-signature-cluster-RESOLVED.md`](../../internal/fixed-suite-bugs/spring-disposablebeanadapter-invalid-destruction-signature-cluster-RESOLVED.md); a direct probe against current dev found the destroy-method reflection path works fine, closing the cluster (no distinct residual identified) |
| `sun.misc.Unsafe$MemoryAccessOption` NPE | 26 | HIGH | **FIXED/RETIRED 2026-07-12** — moved to [`../../internal/fixed-suite-bugs/spring-boot-unsafe-memoryaccessoption-npe-FIXED.md`](../../internal/fixed-suite-bugs/spring-boot-unsafe-memoryaccessoption-npe-FIXED.md); same bug independently found+fixed via a concurrent Keycloak investigation, canonical doc is `testsuite-model-unsafe-putorderedlong-memoryaccessoption-npe-FIXED.md` in the same directory |
| `HttpClient.Builder` dead registration | 13 | HIGH | **RESOLVED 2026-07-12** — moved to [`../../internal/fixed-suite-bugs/springboot-httpclient-builder-dead-registration-abstractmethoderror-FIXED.md`](../../internal/fixed-suite-bugs/springboot-httpclient-builder-dead-registration-abstractmethoderror-FIXED.md); the active real-JDK registrar now covers every Java 17 fluent builder method |
| `zip-filedatablock-bulk-bytebuffer-put-aioobe.md` | 9 (whole `spring-boot-loader` module) | HIGH | **FIXED 2026-07-12** — moved to [`../../internal/springboot/zip-filedatablock-bulk-bytebuffer-put-aioobe-FIXED.md`](../../internal/springboot/zip-filedatablock-bulk-bytebuffer-put-aioobe-FIXED.md) |
| `flyway-cglib-heap-corruption-sigsegv-crash.md` | 1 (`FlywayAutoConfigurationTests`) | HIGH (SIGSEGV) | **FIXED 2026-07-12** — moved to [`../../internal/flyway-cglib-heap-corruption-sigsegv-crash-FIXED.md`](../../internal/flyway-cglib-heap-corruption-sigsegv-crash-FIXED.md); `read_string` misidentified a `String[]` array as `java/lang/String` by class ID alone |
| `TestCompiler` annotation/platform listing cluster | 7 (`spring-boot-configuration-processor`) | MEDIUM | **FIXED/RETIRED 2026-07-13** — moved to [`../../internal/springboot/testcompiler-annotation-classes-not-found-cluster-FIXED.md`](../../internal/springboot/testcompiler-annotation-classes-not-found-cluster-FIXED.md); full JRT package listing and generated in-memory `resource:` URL handling fixed, 94/94 tests pass |
| [`brave-baggagefields-classcast-summary-printing.md`](brave-baggagefields-classcast-summary-printing.md) | 1 (`BraveAutoConfigurationTests`) | LOW (not memory-unsafe) | OPEN — `ClassCastException` in JUnit summary printing (`Formatter`/CLDR `Locale` bootstrap); deeply investigated 2026-07-13, traced to `java/util/Collections.<clinit>` silently failing during early VM bootstrap, exact cause not yet found |
| [`jdbcsession-hsqldb-rangegroupempty-hang.md`](jdbcsession-hsqldb-rangegroupempty-hang.md) | 1 (`JdbcSessionAutoConfigurationTests`) | MEDIUM (hang, not memory-unsafe) | OPEN — found 2026-07-13 while re-verifying the `OnClassCondition` fix; confirmed pre-existing on unmodified `dev`, unrelated to that fix. Out-of-bounds `get_field` loop on `org/hsqldb/RangeGroup$RangeGroupEmpty` |

## HANG-rerun follow-up (150 classes @ 1500s timeout)

Reran the original 150 HANG classes at 5x the timeout (see
`apps/spring-boot-suite-runner/RESULTS-20260711.md` "HANG rerun" section):
90 still hung, 51 turned FAIL, 7 PASS, 2 CRASH. Of the 51 FAILs: 7 are the
the now-retired `TestCompiler` cluster above; ~16 more
overlap with the (now fixed) `OnClassCondition` cluster and
the two retired clusters (destroy-method resolution, `MemoryAccessOption`)
above — those classes just needed more wall time to *reach* the
already-known failure instead of timing out first. The remaining ~28 are a
scattered long tail (mostly one-off `AssertionError`s, a couple of
`GroovyRuntimeException`s from the Thymeleaf layout-dialect integration, two
`ParameterResolutionException`s for `WebTestClient` autowiring) — not yet
clustered; no single dominant pattern found. Of the 2 CRASHes: one
(`FlywayAutoConfigurationTests`) is the now-fixed heap-corruption doc above; the
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
