# Spring Boot suite — full rerun on Azure Linux, 2026-08-02

First full-suite (1975-class) run on the **Azure Linux host** rather than the
usual Windows box, per direct request. Same runner script
(`run-spring-boot-suite.ps1`), run via `pwsh` on Linux — required two
Linux-specific workarounds documented below.

- **Host:** Azure Linux box (`victor@20.83.144.174`), Ubuntu 24.04, 16 cores,
  32GB RAM.
- **Worktree:** `/data/data/cratonvm`, branch `dev-merge-staging4-20260727`,
  merged forward to `origin/dev` @ `c1fe51a244` (460 commits ahead of the
  prior state of this branch).
- **Binary:** `target/release/cratonvm` (rebuilt, `cargo build --release`,
  ~4 minutes).
- **JDK:** `/data/jdk25-real-20260717/jdk-25.0.3+9` (Temurin 25.0.3+9;
  system `java` is 21, do not use it).
- **Fixture:** reused the pre-built
  `/data/data/springboot-jsonreader-deprecation-20260718` tree via
  `-SpringBootRoot` instead of a fresh Gradle seed (saves significant time —
  full `testClasses` compile for ~150 modules already done).
- **Sharding:** 8 shards (`-Start`/`-Count`, 247 classes each except the last
  at 246), `-Parallel 2` per shard (16 concurrent CratonVM processes total),
  `-TimeoutSec 300`, `RunName=craton-fullsuite-azure-20260802`. Wall clock:
  ~40 minutes.

## Two Linux-specific issues found and fixed before the run

1. **122 of 146 `cratonvm-test-cp.txt` files in the fixture were
   Windows-format** (semicolon-delimited, `C:\craton\CratonVM\...` paths) —
   scp'd over from a Windows box originally. Unusable on Linux (wrong
   separator, wrong path syntax). Fixed with a single bulk regeneration
   instead of per-module fixes: `JAVA_HOME=<jdk25> ./gradlew cratonvmTestCp
   --init-script cratonvm-test-cp.init.gradle --no-daemon --continue` (no
   `-p <module>` scoping) — one Gradle invocation across the whole
   multi-project build, ~1m46s, rewrote all 437 classpath files (more than
   the 146 the runner's default `Subtrees` scan covers) to Linux-native
   paths. Verified 0 remaining Windows-format files afterward.
2. **The runner script's `Resolve-CratonExe`/`gradlew.bat` auto-discovery is
   Windows-only** (hardcoded `.exe` suffix, backslash path literals,
   `gradlew.bat`). Worked around entirely by always passing explicit
   `-Exe <path>` and `-JdkHome <path>` (which bypass the broken
   auto-discovery per the script's own `if ($Exe) { ... return }` short
   circuit) and never invoking `-Setup`/`-RefreshClasspaths` through the
   PowerShell script on this host — the classpath regeneration above used
   the Unix `./gradlew` wrapper directly instead.

## A benign race in concurrent shards sharing one RunName

All 8 shards used the same `-RunName`, so they all wrote to the *same*
`results.tsv` concurrently (unlike prior Windows runs, which gave each shard
its own `RunName`/subdirectory). This produced one extra row: a duplicated
header line embedded mid-file, from two shards racing to initialize the file
at startup. No data loss — after filtering the phantom header row, exactly
1975 unique classes are present, 0 duplicate class rows, 0 malformed rows.
Worth using a distinct `-RunName` per shard next time to avoid relying on
this turning out benign.

## Totals — vs. the prior full-suite baseline (Windows, 2026-07-31)

| Status | 2026-07-31 (Windows) | 2026-08-02 (Azure Linux) | Δ |
|---|---:|---:|---:|
| PASS | 1883 (95.3%) | **1900 (96.2%)** | **+17** |
| FAIL | 20 | 24 | +4 |
| HANG | 26 | 7 | **-19** |
| CRASH | 3 | 1 | **-2** |
| EMPTY | 43 | 43 | 0 (unchanged — structurally empty fixtures) |
| **Total** | **1975** | **1975** | |

**96.2% of the full Spring Boot 4.1.0-SNAPSHOT suite passes**, up from 95.3%
three days ago — consistent with the steady stream of fixes landing on `dev`
from concurrent sessions. This is the *first* full-suite run on Linux for
this investigation thread, so the comparison isn't perfectly apples-to-apples
(different OS, different JIT/native code paths in a few places, different
binary provenance) — the FAIL count upticking slightly (+4) despite the
overall PASS improvement could include some Linux-specific behavior, not
purely regressions; not triaged class-by-class against the Windows list this
round.

## Residual (32 non-PASS/non-EMPTY: 24 FAIL / 7 HANG / 1 CRASH)

### 1 CRASH

| Module | Class | Seconds |
|---|---|---:|
| `module/spring-boot-jetty` | `SslServerCustomizerTests` | 1.4 |

### 7 HANG (all at the 300s ceiling — genuinely stuck or just slow, not distinguished this round)

| Module | Class |
|---|---|
| `module/spring-boot-tomcat` | `TomcatServletWebServerFactoryTests` |
| `module/spring-boot-jetty` | `JettyServletWebServerFactoryTests` |
| `module/spring-boot-jooq` | `JooqAutoConfigurationTests` |
| `module/spring-boot-jooq` | `JooqFlywayDatabaseInitializationTests` |
| `module/spring-boot-http-client` | `JdkClientHttpRequestFactoryBuilderTests` |
| `module/spring-boot-jackson` | `JacksonAutoConfigurationTests` |
| `module/spring-boot-kafka` | `KafkaAutoConfigurationIntegrationTests` |

### 24 FAIL

| Module | Class | Seconds |
|---|---|---:|
| `configuration-metadata/spring-boot-configuration-metadata-changelog-generator` | `ChangelogWriterTests` | 0.6 |
| `module/spring-boot-flyway` | `Flyway110AutoConfigurationTests` | 2.8 |
| `module/spring-boot-jdbc` | `HikariDataSourceConfigurationTests` | 30.7 |
| `module/spring-boot-mongodb` | `MongoReactiveAutoConfigurationTests` | 29.1 |
| `loader/spring-boot-jarmode-tools` | `ExtractCommandTests` | 2.0 |
| `loader/spring-boot-jarmode-tools` | `ExtractLayersCommandTests` | 2.8 |
| `loader/spring-boot-jarmode-tools` | `HelpCommandTests` | 2.4 |
| `loader/spring-boot-jarmode-tools` | `ListCommandTests` | 2.6 |
| `loader/spring-boot-jarmode-tools` | `ListLayersCommandTests` | 0.8 |
| `loader/spring-boot-jarmode-tools` | `ToolsJarModeTests` | 1.2 |
| `core/spring-boot` | `ApplicationPidTests` | 1.2 |
| `module/spring-boot-tomcat` | `TomcatWebServerFactoryCustomizerTests` | 81.5 |
| `loader/spring-boot-loader` | `ZipContentTests` | 149.2 |
| `core/spring-boot-autoconfigure` | `ConditionalOnCheckpointRestoreTests` | 3.2 |
| `module/spring-boot-gson` | `Gson210AutoConfigurationTests` | 2.0 |
| `module/spring-boot-quartz` | `QuartzEndpointWebIntegrationTests` | 295.4 |
| `core/spring-boot` | `NoSuchMethodFailureAnalyzerTests` | 9.5 |
| `module/spring-boot-health` | `DiskSpaceHealthIndicatorTests` | 3.6 |
| `module/spring-boot-http-client` | `JdkClientHttpConnectorBuilderTests` | 8.7 |
| `module/spring-boot-liquibase` | `Liquibase423AutoConfigurationTests` | 1.6 |
| `module/spring-boot-webservices` | `WebServiceMessageSenderFactoryTests` | 4.2 |
| `test-support/spring-boot-test-support` | `ModifiedClassPathExtensionOverridesParameterizedTests` | 2.0 |
| `test-support/spring-boot-test-support` | `ModifiedClassPathExtensionOverridesTests` | 1.8 |
| `test-support/spring-boot-test-support` | `ResourcesTests` | 0.6 |

`WebServiceMessageSenderFactoryTests` also appeared in the 08-01 Windows
26-class residual triage (Netty `CompositeByteBuf.<clinit>` NPE, doc
`docs/known-issues/springboot/netty-compositebytebuf-clinit-reads-unpooled-empty-buffer-null-20260731.md`)
— worth checking if this Linux failure is the same bug before assuming a new
platform-specific one. `QuartzEndpointWebIntegrationTests` likewise matches a
known-open doc from that round. Not triaged against the doc corpus otherwise
this round — a rerun-only pass on new territory (first Linux run).

## Reproduce / rerun

```bash
ssh -i ~/.ssh/azure.pem victor@20.83.144.174
cd /data/data/cratonvm
/snap/bin/pwsh -NoProfile -File /tmp/run-shard.ps1 -Start <N> -Count <M> \
  -RunName <unique-per-shard-name> -Parallel 2 -TimeoutSec 300
```

Where `/tmp/run-shard.ps1` wraps the real runner with `-Exe
/data/data/cratonvm/target/release/cratonvm -JdkHome
/data/jdk25-real-20260717/jdk-25.0.3+9 -SpringBootRoot
/data/data/springboot-jsonreader-deprecation-20260718` (explicit `-Exe`/
`-JdkHome` bypass the Windows-only auto-discovery).

Full results:
`apps/spring-boot-suite-runner/.suite/results/craton-fullsuite-azure-20260802/all-jit/results.tsv`
(on the Azure host, `/data/data/cratonvm/...`).
