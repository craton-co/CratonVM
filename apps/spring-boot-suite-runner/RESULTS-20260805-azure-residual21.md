# Spring Boot suite — 21-class residual rerun on Azure Linux, 2026-08-05

Reran the 21-class residual (20 FAIL / 1 CRASH) from the
[08-04 triage](RESULTS-20260804-azure-residual32.md) after merging `dev`
forward and rebuilding.

- **Worktree:** `/data/data/cratonvm`, branch `dev-merge-staging4-20260727`,
  merged forward to `origin/dev` @ `2925b8cc40` (411 commits ahead of the
  08-04 merge point).
- **Binary:** `target/release/cratonvm` (rebuilt, `cargo build --release`,
  ~4m04s).
- **Sharding:** 4 shards (5/5/5/6 classes), `-Parallel 2` per shard,
  `-TimeoutSec 1500`, each with its own `-RunName`.

## Result: 21/21 PASS

Every single class from the 08-04 residual now passes — including all 3
classes documented as genuine new CratonVM bugs
(`ApplicationPidTests`/NOFOLLOW_LINKS, `DiskSpaceHealthIndicatorTests`/
`UnixFileSystem.getBooleanAttributes0`, `KafkaAutoConfigurationIntegrationTests`),
the 3 reopened regressions (jarmode-tools timestamp preservation,
`ZipContentTests`, `ResourcesTests`), and — notably — all 8 classes affected
by the `SSLSocketFactory.getDefault()` double-registration bug
(`Flyway110AutoConfigurationTests`, `HikariDataSourceConfigurationTests`,
`Liquibase423AutoConfigurationTests`, `Gson210AutoConfigurationTests`,
`NoSuchMethodFailureAnalyzerTests`, `TomcatWebServerFactoryCustomizerTests`,
both `ModifiedClassPathExtensionOverrides*Tests`).

The 6 classes triaged as NOT-CratonVM's-fault (CRLF-corrupted
`jarmode-tools`/changelog-generator fixtures, Maven/Aether network
resolution) also pass now — consistent with those never having been real VM
defects (a fixture fix or transient network recovery, not a CratonVM code
change, explains those).

| Module | Class | Seconds |
|---|---|---:|
| `configuration-metadata/spring-boot-configuration-metadata-changelog-generator` | `ChangelogWriterTests` | 0.4 |
| `loader/spring-boot-jarmode-tools` | `ExtractCommandTests` | 1.4 |
| `loader/spring-boot-jarmode-tools` | `ExtractLayersCommandTests` | 2.0 |
| `module/spring-boot-flyway` | `Flyway110AutoConfigurationTests` | 7.0 |
| `module/spring-boot-jdbc` | `HikariDataSourceConfigurationTests` | 20.8 |
| `loader/spring-boot-jarmode-tools` | `HelpCommandTests` | 1.2 |
| `loader/spring-boot-jarmode-tools` | `ListCommandTests` | 1.3 |
| `loader/spring-boot-jarmode-tools` | `ListLayersCommandTests` | 0.6 |
| `loader/spring-boot-jarmode-tools` | `ToolsJarModeTests` | 0.6 |
| `core/spring-boot` | `ApplicationPidTests` | 0.8 |
| `core/spring-boot-autoconfigure` | `ConditionalOnCheckpointRestoreTests` | 1.6 |
| `module/spring-boot-tomcat` | `TomcatWebServerFactoryCustomizerTests` | 44.7 |
| `module/spring-boot-gson` | `Gson210AutoConfigurationTests` | 1.6 |
| `core/spring-boot` | `NoSuchMethodFailureAnalyzerTests` | 3.2 |
| `loader/spring-boot-loader` | `ZipContentTests` | 174.5 |
| `module/spring-boot-health` | `DiskSpaceHealthIndicatorTests` | 2.7 |
| `module/spring-boot-liquibase` | `Liquibase423AutoConfigurationTests` | 6.1 |
| `test-support/spring-boot-test-support` | `ModifiedClassPathExtensionOverridesParameterizedTests` | 1.8 |
| `test-support/spring-boot-test-support` | `ModifiedClassPathExtensionOverridesTests` | 1.0 |
| `test-support/spring-boot-test-support` | `ResourcesTests` | 1.0 |
| `module/spring-boot-kafka` | `KafkaAutoConfigurationIntegrationTests` | 40.8 |

## Doc closure recommended

Given this clean 21/21, the 3 docs reopened/written on 2026-08-04
(`sslsocketfactory-getdefault-aether-resolution-regression-20260804.md`,
`jarmode-tools-extract-timestamp-preservation.md`,
`spring-boot-loader-residual-20260723.md`, plus the new
`nio-write-ignores-nofollow-links-symlink-20260804.md`,
`unixfilesystem-getbooleanattributes0-missing-native-20260804.md`,
`resourcestests-trailing-slash-path-normalization.md`, and
`kafka-embedded-kraft-boundport-listeners-distinct-classcastexception-20260804.md`)
should be re-validated and likely marked FIXED/closed by whoever picks this
up next — not done in this session (a rerun-only pass, no source changes
made, no doc edits this round).

## Reproduce / rerun

```bash
ssh -i ~/.ssh/azure.pem victor@20.83.144.174
cd /data/data/cratonvm
/snap/bin/pwsh -NoProfile -File /tmp/run-shard-cl.ps1 -Start <N> -Count <M> \
  -RunName <unique-per-shard-name> -Parallel 2 -TimeoutSec 1500 \
  -ClassList apps/spring-boot-suite-runner/.suite/residual-20260805-21.tsv
```

Input list:
`apps/spring-boot-suite-runner/.suite/residual-20260805-21.tsv` (21 classes,
`module\tclass` header included).

Full results, per shard:
`apps/spring-boot-suite-runner/.suite/results/craton-residual21-20260805-s{1..4}/all-jit/results.tsv`.
