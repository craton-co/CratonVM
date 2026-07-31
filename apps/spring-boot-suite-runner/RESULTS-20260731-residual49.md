# Spring Boot suite — 49-class residual rerun, 2026-07-31

Reran the 49-class residual from the [full-suite rerun](RESULTS-20260731.md)
(20 FAIL / 26 HANG / 3 CRASH out of 1975) against current `dev`, after
merging and rebuilding again.

- **Worktree:** `C:\craton\CratonVM-spring-boot-residual-20260728`, branch
  `feat/spring-boot-residual-rerun-20260728`, merged forward to `dev` @
  `9fcd1b63f`.
- **Binary:** `cratonvm-spring-boot-residual0728.exe` (rebuilt).
- **Sharding:** 1 shard, `-Parallel 1` (serialized), `-TimeoutSec 300`,
  `RunName=craton-rerun-20260731`. Wall clock: ~2h53m (10366s).

Note: `origin/dev` broke shortly after this merge point (commit `dc75ae1214`,
introduced by `b45eb2325f` — missing `types/src/compat.rs` and related
JDK-only compat/census implementation). `9fcd1b63f`, the commit this round
merged and built from, predates that breakage — this build was clean
(`cargo build --release` exit 0), so these results are trustworthy, but a
*subsequent* `merge dev` attempt from this branch would need to route around
`dc75ae1214` until it's fixed upstream.

## A data-quality caveat on this round: `-TimeoutSec 300` is short

Unlike the 07-29/07-30 residual rounds (`-TimeoutSec 1500`), this round used
300s per the request. The runner's `Get-EffectiveClassTimeoutSec` override
still kicked in for a few known-slow classes (`RabbitAutoConfigurationTests`
900s, `CacheAutoConfigurationTests` 600s, `HibernateJpaAutoConfigurationTests`
1200s, `JerseyEndpointRequestIntegrationTests` 600s,
`BasicErrorControllerIntegrationTests` 1800s), but the other 43 classes ran
at the base 300s ceiling. **Most of the 20 HANG results below hit exactly
300.0-300.2s** — consistent with genuinely slow-but-progressing tests as much
as with true stuck hangs; this round can't distinguish the two. Treat the
HANG list as "needs a longer-timeout reverify," not as 20 confirmed hangs.

## Totals

| Status | Count |
|---|---:|
| PASS | 8 (16.3%) |
| FAIL | 18 |
| HANG | 20 |
| CRASH | 3 |
| **Total** | **49** |

8 of the original 49 residuals now pass outright:
`RabbitAutoConfigurationTests`, `CachesEndpointWebIntegrationTests`,
`DataRestAutoConfigurationTests`, `PropertiesServerBuilderCustomizerTests`,
`HazelcastAutoConfigurationServerTests`, `Neo4jAutoConfigurationTests`,
`DefaultErrorWebExceptionHandlerIntegrationTests`,
`WebFluxAutoConfigurationTests`.

### 3 CRASH (same as the full-suite round, not re-diagnosed)

| Module | Class | Seconds |
|---|---|---:|
| `module/spring-boot-devtools` | `RestartClassLoaderTests` | 6.3 |
| `module/spring-boot-webmvc` | `WebMvcHealthEndpointAdditionalPathIntegrationTests` | 209.3 |
| `module/spring-boot-webmvc` | `BasicErrorControllerIntegrationTests` | 69.1 |

### 18 FAIL

| Module | Class | Seconds |
|---|---|---:|
| `core/spring-boot` | `ApplicationConversionServiceTests` | 19.5 |
| `core/spring-boot` | `ConfigTreePropertySourceTests` | 6.3 |
| `core/spring-boot` | `PemSslStoreTests` | 11.6 |
| `core/spring-boot` | `ApplicationTempTests` | 2.1 |
| `core/spring-boot` | `LambdaSafeTests` | 17.3 |
| `core/spring-boot-autoconfigure` | `FileWatcherTests` | 6.3 |
| `core/spring-boot-autoconfigure` | `CertificateMatcherTests` | 31.7 |
| `module/spring-boot-actuator` | `ConversionServiceParameterValueMapperTests` | 10.8 |
| `module/spring-boot-actuator` | `GitInfoContributorTests` | 2.5 |
| `module/spring-boot-devtools` | `ChangeableUrlsTests` | 2.1 |
| `module/spring-boot-http-client` | `SimpleClientHttpRequestFactoryBuilderTests` | 8.5 |
| `module/spring-boot-integration` | `IntegrationGraphEndpointWebIntegrationTests` | 116.8 |
| `module/spring-boot-hibernate` | `HibernateJpaAutoConfigurationTests` | 1089.0 (1200s override, still FAIL) |
| `module/spring-boot-ldap` | `EmbeddedLdapAutoConfigurationTests` | 63.7 |
| `module/spring-boot-security` | `JerseyEndpointRequestIntegrationTests` | 372.9 (600s override) |
| `module/spring-boot-security-oauth2-resource-server` | `OAuth2ResourceServerAutoConfigurationTests` | 285.6 |
| `module/spring-boot-webmvc` | `BasicErrorControllerDirectMockMvcTests` | 73.9 |
| `module/spring-boot-webservices` | `WebServiceMessageSenderFactoryTests` | 10.1 |

### 20 HANG (mostly at the 300s ceiling — needs a longer-timeout reverify)

| Module | Class | Seconds |
|---|---|---:|
| `core/spring-boot` | `log4j2.Log4J2LoggingSystemTests` | 300.2 |
| `core/spring-boot` | `logback.LogbackLoggingSystemTests` | 300.1 |
| `core/spring-boot` | `SpringApplicationTests` | 300.1 |
| `module/spring-boot-batch-jdbc` | `BatchJdbcAutoConfigurationTests` | 300.2 |
| `module/spring-boot-cache` | `CacheAutoConfigurationTests` | 600.2 (600s override, still HANG) |
| `module/spring-boot-cloudfoundry` | `CloudFoundryReactiveActuatorAutoConfigurationTests` | 300.0 |
| `module/spring-boot-cloudfoundry` | `CloudFoundryActuatorAutoConfigurationTests` | 300.0 |
| `module/spring-boot-flyway` | `FlywayAutoConfigurationTests` | 300.2 |
| `module/spring-boot-integration` | `IntegrationAutoConfigurationTests` | 300.0 |
| `module/spring-boot-jackson` | `JacksonAutoConfigurationTests` | 300.1 |
| `module/spring-boot-jetty` | `JettyServletWebServerFactoryTests` | 300.2 |
| `module/spring-boot-jooq` | `JooqAutoConfigurationTests` | 300.1 |
| `module/spring-boot-jooq` | `JooqFlywayDatabaseInitializationTests` | 300.0 |
| `module/spring-boot-jooq-test` | `JooqTestIntegrationTests` | 300.1 |
| `module/spring-boot-jooq-test` | `JooqTestPropertiesIntegrationTests` | 300.1 |
| `module/spring-boot-jooq-test` | `JooqTestWithAutoConfigureTestDatabaseIntegrationTests` | 300.1 |
| `module/spring-boot-pulsar` | `PulsarAutoConfigurationTests` | 300.2 |
| `module/spring-boot-quartz` | `QuartzEndpointWebIntegrationTests` | 300.1 |
| `module/spring-boot-security` | `ManagementWebSecurityAutoConfigurationTests` | 300.2 |
| `module/spring-boot-tomcat` | `TomcatServletWebServerFactoryTests` | 300.2 |

Not triaged against `docs/known-issues/springboot/` this round — a
rerun-only pass.

## Reproduce / rerun

```powershell
$env:JAVA_HOME = "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
$exe = "<worktree>\target\release\cratonvm-spring-boot-residual0728.exe"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -Exe $exe -JdkHome $env:JAVA_HOME `
  -ClassList apps\spring-boot-suite-runner\.suite\residual-20260731.tsv `
  -RunName <name> -Parallel 1 -TimeoutSec 300
```

Input list: `apps/spring-boot-suite-runner/.suite/residual-20260731.tsv` (49
classes, **requires a `module\tclass` header line** — see
`reference_classlist_tsv_needs_header_row` memory note; a headerless file
silently drops one class and breaks classpath refresh).

Full results:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260731/all-jit/results.tsv`.
