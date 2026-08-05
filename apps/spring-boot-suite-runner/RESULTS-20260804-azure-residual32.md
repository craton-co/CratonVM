# Spring Boot suite — 32-class residual rerun on Azure Linux, 2026-08-04

Reran the 32-class residual (24 FAIL / 7 HANG / 1 CRASH) from the
[08-02 Azure full-suite run](RESULTS-20260802-azure-fullsuite.md) after
merging `dev` forward and rebuilding.

- **Worktree:** `/data/data/cratonvm`, branch `dev-merge-staging4-20260727`,
  merged forward to `origin/dev` @ `d118d46ab3` (367 commits ahead of the
  08-02 merge point).
- **Binary:** `target/release/cratonvm` (rebuilt, `cargo build --release`,
  ~4m24s).
- **Sharding:** 4 shards (8 classes each), `-Parallel 2` per shard,
  `-TimeoutSec 1500` (vs. 300s in the 08-02 round — deliberately longer to
  separate genuine hangs from just-slow, per the same caveat flagged in that
  round's writeup). Each shard used its own `-RunName` this time to avoid the
  concurrent-write race from the 08-02 run.

## Totals

| Status | 08-02 (at 300s) | 08-04 (at 1500s) |
|---|---:|---:|
| PASS | 0 | **11** |
| FAIL | 24 | 20 |
| HANG | 7 | **0** |
| CRASH | 1 | 1 |
| **Total** | **32** | **32** |

All 7 previously-HANG classes resolved at the longer timeout — 6 now PASS,
1 (`KafkaAutoConfigurationIntegrationTests`) now FAILs in 7.8s, a real fast
failure, not a timeout artifact. Confirms the 300s ceiling in the 08-02 round
was too short to distinguish slow-but-working from stuck, same pattern
observed on the Windows host's HANG-reverify round on 07-31.

`SslServerCustomizerTests` (the sole CRASH from 08-02) now PASSes.
`ZipContentTests` (a FAIL in 08-02, 149.2s) is now a **CRASH** at 156.8s —
different symptom, not investigated this round.

### 11 now PASS

`MongoReactiveAutoConfigurationTests`, `TomcatServletWebServerFactoryTests`,
`SslServerCustomizerTests`, `QuartzEndpointWebIntegrationTests`,
`JettyServletWebServerFactoryTests`,
`JooqFlywayDatabaseInitializationTests`,
`JdkClientHttpRequestFactoryBuilderTests`, `JooqAutoConfigurationTests`
(801.2s — genuinely slow, close to the 1500s ceiling),
`JdkClientHttpConnectorBuilderTests`, `WebServiceMessageSenderFactoryTests`,
`JacksonAutoConfigurationTests`.

### 1 CRASH

| Module | Class | Seconds |
|---|---|---:|
| `loader/spring-boot-loader` | `ZipContentTests` | 156.8 |

### 20 FAIL

| Module | Class | Seconds |
|---|---|---:|
| `configuration-metadata/spring-boot-configuration-metadata-changelog-generator` | `ChangelogWriterTests` | 0.6 |
| `module/spring-boot-flyway` | `Flyway110AutoConfigurationTests` | 1.9 |
| `loader/spring-boot-jarmode-tools` | `ExtractCommandTests` | 1.8 |
| `loader/spring-boot-jarmode-tools` | `ExtractLayersCommandTests` | 2.2 |
| `module/spring-boot-jdbc` | `HikariDataSourceConfigurationTests` | 26.9 |
| `loader/spring-boot-jarmode-tools` | `HelpCommandTests` | 1.6 |
| `loader/spring-boot-jarmode-tools` | `ListCommandTests` | 1.4 |
| `loader/spring-boot-jarmode-tools` | `ListLayersCommandTests` | 0.6 |
| `loader/spring-boot-jarmode-tools` | `ToolsJarModeTests` | 0.9 |
| `core/spring-boot` | `ApplicationPidTests` | 0.6 |
| `module/spring-boot-tomcat` | `TomcatWebServerFactoryCustomizerTests` | 59.0 |
| `core/spring-boot-autoconfigure` | `ConditionalOnCheckpointRestoreTests` | 1.8 |
| `module/spring-boot-gson` | `Gson210AutoConfigurationTests` | 1.4 |
| `core/spring-boot` | `NoSuchMethodFailureAnalyzerTests` | 9.7 |
| `module/spring-boot-health` | `DiskSpaceHealthIndicatorTests` | 3.8 |
| `module/spring-boot-liquibase` | `Liquibase423AutoConfigurationTests` | 1.8 |
| `module/spring-boot-kafka` | `KafkaAutoConfigurationIntegrationTests` | 7.8 |
| `test-support/spring-boot-test-support` | `ModifiedClassPathExtensionOverridesParameterizedTests` | 2.8 |
| `test-support/spring-boot-test-support` | `ModifiedClassPathExtensionOverridesTests` | 2.6 |
| `test-support/spring-boot-test-support` | `ResourcesTests` | 0.8 |

Not triaged against `docs/known-issues/springboot/` this round — a
rerun-only pass.

## Reproduce / rerun

```bash
ssh -i ~/.ssh/azure.pem victor@20.83.144.174
cd /data/data/cratonvm
/snap/bin/pwsh -NoProfile -File /tmp/run-shard-cl.ps1 -Start <N> -Count <M> \
  -RunName <unique-per-shard-name> -Parallel 2 -TimeoutSec 1500 \
  -ClassList apps/spring-boot-suite-runner/.suite/residual-azure-20260802-32.tsv
```

Where `/tmp/run-shard-cl.ps1` wraps the real runner with `-Exe
/data/data/cratonvm/target/release/cratonvm -JdkHome
/data/jdk25-real-20260717/jdk-25.0.3+9 -SpringBootRoot
/data/data/springboot-jsonreader-deprecation-20260718 -ClassList <path>`.

Input list:
`apps/spring-boot-suite-runner/.suite/residual-azure-20260802-32.tsv` (32
classes, `module\tclass` header included).

Full results, per shard:
`apps/spring-boot-suite-runner/.suite/results/craton-residual32-20260804-s{1..4}/all-jit/results.tsv`.
