# Spring Boot suite — HANG reverify, 2026-07-31

Reran the 20 HANG classes from the
[49-class residual rerun](RESULTS-20260731-residual49.md) (which used
`-TimeoutSec 300`) at `-TimeoutSec 1500` to separate genuine hangs from
just-slow tests, per the data-quality caveat noted in that round's writeup.

- **Worktree/binary:** same as the 49-class round — no rebuild, `dev`
  unchanged (`9fcd1b63f`).
- **Sharding:** 1 shard, `-Parallel 1`, `-TimeoutSec 1500`,
  `RunName=craton-hangverify-20260731`. Wall clock: ~1h57m (7019s).

## Result: none were genuine hangs

All 20 resolved within 1500s — **0 HANG remaining**.

| Status | Count |
|---|---:|
| PASS | 15 |
| FAIL | 5 |
| HANG | 0 |
| **Total** | **20** |

Confirms the caveat: at 300s these were all just slow (up to 1255.7s for
`JooqAutoConfigurationTests`, 973.8s for `TomcatServletWebServerFactoryTests`),
not stuck.

### 15 now PASS
`LogbackLoggingSystemTests`, `SpringApplicationTests`,
`BatchJdbcAutoConfigurationTests`, `CacheAutoConfigurationTests`,
`CloudFoundryReactiveActuatorAutoConfigurationTests`,
`FlywayAutoConfigurationTests`, `IntegrationAutoConfigurationTests`,
`JacksonAutoConfigurationTests`, `JooqAutoConfigurationTests`,
`JooqFlywayDatabaseInitializationTests`, `JooqTestIntegrationTests`,
`JooqTestPropertiesIntegrationTests`,
`JooqTestWithAutoConfigureTestDatabaseIntegrationTests`,
`PulsarAutoConfigurationTests`, `TomcatServletWebServerFactoryTests`.

### 5 FAIL (real assertion/behavior failures once given time to run)

| Module | Class | Seconds |
|---|---|---:|
| `core/spring-boot` | `Log4J2LoggingSystemTests` | 102.8 |
| `module/spring-boot-cloudfoundry` | `CloudFoundryActuatorAutoConfigurationTests` (servlet) | 151.1 |
| `module/spring-boot-jetty` | `JettyServletWebServerFactoryTests` | 543.6 |
| `module/spring-boot-quartz` | `QuartzEndpointWebIntegrationTests` | 738.7 |
| `module/spring-boot-security` | `ManagementWebSecurityAutoConfigurationTests` | 260.7 |

Not triaged against `docs/known-issues/springboot/` this round — a
rerun-only pass.

## Combined picture: the full 49-class residual, resolved

Folding this reverify back into the [49-class round](RESULTS-20260731-residual49.md):

| Status | Count |
|---|---:|
| PASS | 23 (46.9%) |
| FAIL | 23 |
| HANG | 0 |
| CRASH | 3 |
| **Total** | **49** |

## Reproduce / rerun

```powershell
$env:JAVA_HOME = "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
$exe = "<worktree>\target\release\cratonvm-spring-boot-residual0728.exe"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -Exe $exe -JdkHome $env:JAVA_HOME `
  -ClassList apps\spring-boot-suite-runner\.suite\hang-20260731.tsv `
  -RunName <name> -Parallel 1 -TimeoutSec 1500
```

Input list: `apps/spring-boot-suite-runner/.suite/hang-20260731.tsv` (20
classes, `module\tclass` header included).

Full results:
`apps/spring-boot-suite-runner/.suite/results/craton-hangverify-20260731/all-jit/results.tsv`.
