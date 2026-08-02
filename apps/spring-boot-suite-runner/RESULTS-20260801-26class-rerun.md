# Spring Boot suite — 26-class FAIL/CRASH residual rerun, 2026-08-01

Reran the 26 FAIL/CRASH classes from the 2026-07-31 residual rounds
([49-class round](RESULTS-20260731-residual49.md) +
[HANG reverify](RESULTS-20260731-hangverify.md)) against current `dev`, after
merging and rebuilding again, following the [full triage of all 26
classes](../../docs/known-issues/springboot/) done between the two rounds.

- **Worktree:** `C:\craton\CratonVM-spring-boot-residual-20260728`, branch
  `feat/spring-boot-residual-rerun-20260728`, merged forward to `dev` @
  `1b24cca1f` (624 commits ahead of the previous merge point — this repo
  moves very fast, multiple concurrent sessions).
- **Binary:** `cratonvm-spring-boot-residual0728.exe` (rebuilt).
- **Sharding:** 1 shard, `-Parallel 1` (serialized), `-TimeoutSec 1500`,
  `RunName=craton-rerun-20260801`. Wall clock: ~1h18m (4705s).

## Totals

| Status | Count |
|---|---:|
| PASS | 2 |
| FAIL | 23 |
| CRASH | 1 |
| **Total** | **26** |

Only 2 fixed since 07-31: `WebMvcHealthEndpointAdditionalPathIntegrationTests`
(322.4s) and `BasicErrorControllerIntegrationTests` (546.4s, 1800s override).

## A significant finding: two "FIXED 2026-08-01" docs don't hold on this build

Between the 07-31 triage and this rerun, a concurrent session did deep
root-cause work and closed two of the bug clusters this triage had reopened —
[`basicerrorcontroller-class-cluster-20260728.md`](../../docs/known-issues/springboot/basicerrorcontroller-class-cluster-20260728.md)
(the `ConditionEvaluationReport$ConditionAndOutcomes` CCE + the
`CaseInsensitiveComparator` checkcast abort) and
[`webflux-defaultpathcontainer-defaultseparator-classcast.md`](../../docs/known-issues/springboot/webflux-defaultpathcontainer-defaultseparator-classcast.md)
(the `DefaultPathContainer$DefaultSeparator` CCE), each with detailed
validation matrices showing 4/4 and 5/5 affected classes green.

This rerun reproduces the identical signatures on **5 of the 7 classes** those
two closures validated, on a binary built from a commit
(`1b24cca1f`) that has every one of their cited fix commits
(`0b18f15eb`, `20cab92aa`, `c3dbb011a`, `063be4f18`) as a genuine ancestor
(verified via `git merge-base --is-ancestor`) — so this isn't a stale-binary
artifact. Both docs have been reopened with a "Regression note (2026-08-01,
again)" section carrying today's exact evidence. Notably, in both clusters
**one class still passes while its siblings sharing the identical call site
fail** (`BasicErrorControllerIntegrationTests` /
`WebMvcHealthEndpointAdditionalPathIntegrationTests`), suggesting the GC
fixes are real but don't cover every promotion/load pattern that reaches the
affected side-table state, not that the fixes are fake.

## Per-class results vs. prior status

| Module | Class | 07-31 status | 08-01 status | Seconds |
|---|---|---|---:|---:|
| `core/spring-boot` | `ApplicationConversionServiceTests` | FAIL | FAIL | 24.8 |
| `core/spring-boot` | `ConfigTreePropertySourceTests` | FAIL | FAIL | 5.5 |
| `core/spring-boot` | `PemSslStoreTests` | FAIL | FAIL | 10.7 |
| `core/spring-boot` | `ApplicationTempTests` | FAIL | FAIL | 2.2 |
| `core/spring-boot` | `LambdaSafeTests` | FAIL | FAIL | 13.1 |
| `core/spring-boot-autoconfigure` | `FileWatcherTests` | FAIL | FAIL | 5.3 |
| `core/spring-boot-autoconfigure` | `CertificateMatcherTests` | FAIL | FAIL | 32.9 |
| `module/spring-boot-actuator` | `ConversionServiceParameterValueMapperTests` | FAIL | FAIL | 14.0 |
| `module/spring-boot-actuator` | `GitInfoContributorTests` | FAIL | FAIL | 2.5 |
| `module/spring-boot-devtools` | `ChangeableUrlsTests` | FAIL | FAIL | 3.9 |
| `module/spring-boot-devtools` | `RestartClassLoaderTests` | CRASH | CRASH | 4.9 |
| `module/spring-boot-http-client` | `SimpleClientHttpRequestFactoryBuilderTests` | FAIL | FAIL | 7.6 |
| `module/spring-boot-integration` | `IntegrationGraphEndpointWebIntegrationTests` | FAIL | FAIL | 118.0 |
| `module/spring-boot-hibernate` | `HibernateJpaAutoConfigurationTests` | FAIL | FAIL | 931.2 |
| `module/spring-boot-ldap` | `EmbeddedLdapAutoConfigurationTests` | FAIL | FAIL | 36.0 |
| `module/spring-boot-security` | `JerseyEndpointRequestIntegrationTests` | FAIL | FAIL | 177.1 |
| `module/spring-boot-security-oauth2-resource-server` | `OAuth2ResourceServerAutoConfigurationTests` | FAIL | FAIL | 323.5 |
| `module/spring-boot-webmvc` | `WebMvcHealthEndpointAdditionalPathIntegrationTests` | CRASH | **PASS** | 322.4 |
| `module/spring-boot-webmvc` | `BasicErrorControllerDirectMockMvcTests` | FAIL | FAIL | 82.5 |
| `module/spring-boot-webmvc` | `BasicErrorControllerIntegrationTests` | CRASH | **PASS** | 546.4 |
| `module/spring-boot-webservices` | `WebServiceMessageSenderFactoryTests` | FAIL | FAIL | 11.3 |
| `core/spring-boot` | `Log4J2LoggingSystemTests` | FAIL | FAIL | 201.5 |
| `module/spring-boot-cloudfoundry` | `CloudFoundryActuatorAutoConfigurationTests` | FAIL | FAIL | 364.9 |
| `module/spring-boot-jetty` | `JettyServletWebServerFactoryTests` | FAIL | FAIL | 917.5 |
| `module/spring-boot-quartz` | `QuartzEndpointWebIntegrationTests` | FAIL | FAIL | 381.7 |
| `module/spring-boot-security` | `ManagementWebSecurityAutoConfigurationTests` | FAIL | FAIL | 161.2 |

`Log4J2LoggingSystemTests` and `EmbeddedLdapAutoConfigurationTests` were
triaged 07-31 as NOT CratonVM's fault (pre-existing HotSpot-reproducible
fixture gap; accepted Windows-only DSA/TLS gap respectively) — their
continued FAIL here is expected, not a regression signal.

Not re-triaged against the 9 new bug docs from the 07-31 round this session —
a rerun-only pass, except for the two closures above which needed reopening
given direct contradicting evidence in hand.

## Reproduce / rerun

```powershell
$env:JAVA_HOME = "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
$exe = "<worktree>\target\release\cratonvm-spring-boot-residual0728.exe"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -Exe $exe -JdkHome $env:JAVA_HOME `
  -ClassList apps\spring-boot-suite-runner\.suite\residual-20260801-26.tsv `
  -RunName <name> -Parallel 1 -TimeoutSec 1500
```

Input list: `apps/spring-boot-suite-runner/.suite/residual-20260801-26.tsv`
(26 classes, `module\tclass` header included).

Full results:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260801/all-jit/results.tsv`.
