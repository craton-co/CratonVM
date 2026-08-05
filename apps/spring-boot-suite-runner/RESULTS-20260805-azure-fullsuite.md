# Spring Boot suite — full rerun on Azure Linux, 2026-08-05

Second full-suite (1975-class) run on the Azure Linux host, after merging
`dev` forward and rebuilding.

- **Worktree:** `/data/data/cratonvm`, branch `dev-merge-staging4-20260727`,
  merged forward to `origin/dev` @ `1078f6f05c` (54 commits ahead of the
  08-04 merge point).
- **Binary:** `target/release/cratonvm` (rebuilt, `cargo build --release`,
  ~3m58s).
- **Sharding:** 8 shards (247 classes each except the last at 246),
  `-Parallel 2` per shard (16 concurrent CratonVM processes total),
  `-TimeoutSec 300`, each shard with its own `-RunName` this time (avoids
  the concurrent-write race hit on 08-02).

## One class needed a manual kill — the shard's own timeout didn't fire

`ConfigurationPropertySourcesTests` (`core/spring-boot`) ran past its 300s
timeout without the runner's `Drain-Running`/`Kill($true)` logic reaping it —
still alive at 16m27s of CPU time, 99% CPU, no sign of exiting. Killed
manually (`kill -9`) after which the shard immediately finished and recorded
it as CRASH at 1016.8s. Not yet clear whether the runner's timeout-kill is
unreliable under `pwsh` on Linux specifically (worth comparing against the
Windows host's behavior, which has not shown this) or whether this class
itself resists `SIGTERM`/graceful termination in a way that needs `SIGKILL`
— the runner's `Kill($true)` should already send a kill signal to the
process tree, so this may be a genuine gap. Flagging for follow-up; not
investigated further this round.

## Totals — vs. the prior full-suite baseline (Azure, 2026-08-02)

| Status | 2026-08-02 | 2026-08-05 | Δ |
|---|---:|---:|---:|
| PASS | 1900 (96.2%) | **1886 (95.5%)** | -14 |
| FAIL | 24 | 39 | +15 |
| BOTH-FAIL | — (feature didn't exist yet) | 1 | new |
| HANG | 7 | 4 | -3 |
| CRASH | 1 | 2 | +1 |
| EMPTY | 43 | 43 | 0 |
| **Total** | **1975** | **1975** | |

The runner now auto-cross-checks against a HotSpot baseline
(`.suite/baseline/hotspot-baseline-latest.tsv`) and reclassifies a FAIL as
**BOTH-FAIL** when HotSpot fails the same class too (added 2026-08-04, per
the CRLF-fixture-corruption findings — this Azure fixture checkout has
~12,788 CRLF-corrupted files that fail identically on HotSpot). Only 1 class
got that reclassification this round
(`JettyServletWebServerFactoryTests`) — the CRLF corruption mostly hits a
different class subset than today's residual, so most of today's 39 FAIL are
still presumptively real CratonVM-specific issues, not fixture artifacts,
but **not individually cross-checked against HotSpot this round** beyond
what the runner did automatically.

The PASS count dropping since 08-02 despite `dev` moving forward 54+367+411
commits since is a genuine regression signal worth investigating, not just
noise — this is the first full-suite comparison point since 08-02 (the two
08-04/08-05 rounds were residual-only reruns of a much smaller class set,
so they couldn't have caught new regressions outside that set).

## Residual (46 non-PASS/non-EMPTY: 39 FAIL / 1 BOTH-FAIL / 4 HANG / 2 CRASH)

### 2 CRASH

| Module | Class | Seconds |
|---|---|---:|
| `core/spring-boot` | `ConfigurationPropertySourcesTests` | 1016.8 (manually killed, see above) |
| `module/spring-boot-kafka` | `KafkaAutoConfigurationTests` | 22.5 |

### 4 HANG (at the 300s ceiling)

| Module | Class |
|---|---|
| `loader/spring-boot-loader` | `ZipContentTests` |
| `module/spring-boot-flyway` | `FlywayAutoConfigurationTests` |
| `module/spring-boot-jooq` | `JooqAutoConfigurationTests` |
| `module/spring-boot-tomcat` | `TomcatServletWebServerFactoryTests` |

### 1 BOTH-FAIL (fails identically on HotSpot — not CratonVM's fault)

| Module | Class | Seconds |
|---|---|---:|
| `module/spring-boot-jetty` | `JettyServletWebServerFactoryTests` | 272.3 |

### 39 FAIL

| Module | Class | Seconds |
|---|---|---:|
| `core/spring-boot` | `EnableConfigurationPropertiesRegistrarTests` | 38.0 |
| `core/spring-boot-autoconfigure` | `CertificateMatcherTests` | 16.8 |
| `core/spring-boot-testcontainers` | `TestcontainersLifecycleApplicationContextInitializerTests` | 12.1 |
| `core/spring-boot-testcontainers` | `ServiceConnectionContextCustomizerTests` | 16.5 |
| `loader/spring-boot-loader` | `JarUrlConnectionTests` | 14.4 |
| `module/spring-boot-amqp` | `RabbitAutoConfigurationTests` | 93.9 |
| `module/spring-boot-batch-data-mongodb` | `BatchDataMongoAutoConfigurationTests` | 23.2 |
| `module/spring-boot-batch-jdbc` | `BatchJdbcAutoConfigurationTests` | 119.4 |
| `module/spring-boot-cassandra` | `CassandraReactiveHealthContributorAutoConfigurationTests` | 7.3 |
| `module/spring-boot-cache` | `CacheAutoConfigurationTests` | 217.4 |
| `module/spring-boot-data-cassandra` | `DataCassandraReactiveRepositoriesAutoConfigurationTests` | 34.5 |
| `module/spring-boot-data-couchbase` | `DataCouchbaseReactiveRepositoriesAutoConfigurationTests` | 6.4 |
| `module/spring-boot-data-neo4j` | `DataNeo4jReactiveRepositoriesAutoConfigurationTests` | 8.4 |
| `module/spring-boot-grpc-server` | `GrpcServerHealthAutoConfigurationTests` | 43.8 |
| `module/spring-boot-hazelcast` | `HazelcastAutoConfigurationClientTests` | 2.6 |
| `module/spring-boot-hazelcast` | `HazelcastJpaDependencyAutoConfigurationTests` | 7.2 |
| `module/spring-boot-http-client` | `JdkClientHttpRequestFactoryBuilderTests` | 7.4 |
| `module/spring-boot-http-client` | `ReactorClientHttpRequestFactoryBuilderTests` | 9.6 |
| `module/spring-boot-jackson` | `JacksonAutoConfigurationTests` | 70.7 |
| `module/spring-boot-jdbc` | `XADataSourceAutoConfigurationTests` | 8.2 |
| `module/spring-boot-kafka` | `ConcurrentKafkaListenerContainerFactoryConfigurerTests` | 4.6 |
| `module/spring-boot-kafka` | `KafkaAutoConfigurationIntegrationTests` | 2.8 |
| `module/spring-boot-micrometer-metrics` | `TaskExecutorMetricsAutoConfigurationTests` | 21.7 |
| `module/spring-boot-micrometer-observation` | `ObservationHandlerGroupTests` | 3.6 |
| `module/spring-boot-micrometer-tracing` | `TracingAndMeterObservationHandlerGroupTests` | 4.4 |
| `module/spring-boot-pulsar` | `PulsarPropertiesMapperTests` | 14.5 |
| `module/spring-boot-pulsar` | `PulsarAutoConfigurationTests` | 119.6 |
| `module/spring-boot-reactor-netty` | `NettyReactiveWebServerFactoryTests` | 17.1 |
| `module/spring-boot-restclient` | `RestClientAutoConfigurationTests` | 19.6 |
| `module/spring-boot-security-oauth2-resource-server` | `ReactiveOAuth2ResourceServerAutoConfigurationTests` | 68.5 |
| `module/spring-boot-tomcat` | `TomcatServletWebServerAutoConfigurationTests` | 36.4 |
| `module/spring-boot-tomcat` | `TomcatReactiveWebServerFactoryTests` | 48.8 |
| `module/spring-boot-web-server` | `AnnotationConfigServletWebServerApplicationContextTests` | 5.8 |
| `module/spring-boot-web-server` | `ServletComponentScanIntegrationTests` | 7.7 |
| `module/spring-boot-web-server` | `ServletWebServerApplicationContextTests` | 16.1 |
| `module/spring-boot-web-server` | `SpringApplicationWebServerTests` | 19.5 |
| `module/spring-boot-webmvc` | `WebMvcAutoConfigurationTests` | 33.5 |
| `module/spring-boot-webmvc` | `WebMvcObservationAutoConfigurationTests` | 12.1 |
| `module/spring-boot-webtestclient` | `WebTestClientAutoConfigurationTests` | 4.0 |

Notably a cluster of ~15 classes touching web-server bootstrap
(`web-server`, `webmvc`, `tomcat`, `jetty`, `reactor-netty`) failed together
— worth checking for one shared root cause (e.g. a recent regression in
servlet/reactive web server bring-up) rather than ~15 independent bugs.
Not triaged against `docs/known-issues/springboot/` this round — a
rerun-only pass.

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
/data/data/springboot-jsonreader-deprecation-20260718`.

Full results, per shard:
`apps/spring-boot-suite-runner/.suite/results/craton-fullsuite-azure-20260805-s{1..8}/all-jit/results.tsv`.
