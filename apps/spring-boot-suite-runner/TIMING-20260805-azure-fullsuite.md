# Timing comparison — passed classes, 08-05 vs 08-02 vs HotSpot

Compares wall-clock time for classes that PASS in the 2026-08-05 Azure
full-suite run against the same classes' time in the 2026-08-02 Azure run
and against the HotSpot baseline (`.suite/baseline/hotspot-baseline-latest.tsv`,
1975 classes, collected 2026-08-04).

## Aggregate

| Comparison | Classes | Sum (08-05) | Sum (other) | Ratio |
|---|---:|---:|---:|---:|
| 08-05 PASS classes, total | 1886 | 15822.3s | — | — |
| Also PASS in 08-02 | 1861 of 1886 | 15092.7s | 18136.9s (08-02) | **0.832x** (17% faster) |
| Also PASS on HotSpot | 1886 of 1886 | 15822.3s | 20235.5s (HotSpot) | **0.782x** (22% faster) |

**Caveat:** this only covers classes that PASS today. It excludes the 4
HANG + 2 CRASH + 39 FAIL classes from today's run, several of which were
timing out at the 300s ceiling — a proper apples-to-apples comparison would
need those resolved first (as the residual-rerun rounds on 08-01/08-04 did
at 1500s). The aggregate "CratonVM faster than HotSpot" reading is also
likely dominated by per-process JVM/Spring-context startup overhead more
than steady-state execution speed — not evidence CratonVM out-executes
HotSpot on CPU-bound work, just that end-to-end per-class wall time (which
these suites are dominated by, given most classes finish in single-digit
seconds) is lower.

## Top 15 slowdowns vs 08-02 (absolute seconds)

| Class | Δ | Ratio |
|---|---:|---:|
| `ImagePackagerTests` (loader-tools) | +36.1s | 4.04x |
| `ConfigDataEnvironmentPostProcessorIntegrationTests` | +25.0s | 1.19x |
| `TomcatServletWebServerServletContextListenerTests` | +23.6s | 2.20x |
| `ConfigDataEnvironmentPostProcessorTests` | +17.8s | 3.33x |
| `CacheManagerCustomizersTests` | +17.5s | 4.94x |
| `QuartzEndpointTests` | +15.2s | 2.61x |
| `ConditionalOnPropertyTests` | +13.8s | 1.28x |
| `SimpleAsyncTaskExecutorBuilderTests` | +13.2s | 3.05x |
| `DefaultErrorWebExceptionHandlerIntegrationTests` (webflux) | +11.6s | 1.17x |
| `DefaultTimeZoneOffsetTests` (loader-tools) | +10.0s | 8.13x |
| `WebFluxManagementChildContextConfigurationIntegrationTests` | +9.1s | 1.18x |
| `ConditionalOnBooleanPropertyTests` | +8.9s | 1.33x |
| `CouchbaseAutoConfigurationTests` | +8.3s | 1.37x |
| `DataCassandraAutoConfigurationTests` | +7.2s | 1.17x |
| `JerseyChildManagementContextConfigurationTests` | +5.4s | 1.64x |

All modest in absolute terms (under 40s); none suggest a severe regression
on their own, though `DefaultTimeZoneOffsetTests` at 8.1x (likely ~1.2s→10s)
and `CacheManagerCustomizersTests` at 4.9x are worth a second look given
how fast/lightweight these classes normally are.

## Top 15 speedups vs 08-02 (absolute seconds)

| Class | Δ | Ratio |
|---|---:|---:|
| `JooqTestWithAutoConfigureTestDatabaseIntegrationTests` | -121.1s | 0.54x |
| `HibernateJpaAutoConfigurationTests` | -107.3s | 0.76x |
| `JooqTestPropertiesIntegrationTests` | -80.3s | 0.66x |
| `BasicErrorControllerIntegrationTests` | -78.0s | 0.69x |
| `JooqTestIntegrationTests` | -72.7s | 0.71x |
| `OAuth2ResourceServerAutoConfigurationTests` | -69.6s | 0.62x |
| `IntegrationAutoConfigurationTests` | -58.8s | 0.74x |
| `Saml2RelyingPartyAutoConfigurationTests` | -46.4s | 0.51x |
| `MvcEndpointRequestIntegrationTests` | -45.9s | 0.67x |
| `LocalDevToolsAutoConfigurationTests` | -45.7s | 0.53x |
| `CloudFoundryActuatorAutoConfigurationTests` (servlet) | -43.3s | 0.72x |
| `DataRedisAutoConfigurationTests` | -42.6s | 0.65x |
| `WebMvcHealthEndpointAdditionalPathIntegrationTests` | -39.3s | 0.71x |
| `JerseyEndpointRequestIntegrationTests` | -36.2s | 0.69x |
| `WebFluxAutoConfigurationTests` | -34.6s | 0.79x |

These line up well with the GC/JIT fixes landed 08-01→08-05 for the
`DefaultPathContainer`/`ConditionEvaluationReport` reclaimed-object family
and the moving-young sweep fixes — most of the biggest speedups are exactly
the classes those fixes targeted.

## Top 15 slowest vs HotSpot (ratio, among today's PASS classes)

| Class | Ratio |
|---|---:|
| `JooqFlywayDatabaseInitializationTests` | 14.4x |
| `ConfigDataEnvironmentPostProcessorIntegrationTests` | 13.2x |
| `JooqTestIntegrationTests` | 11.7x |
| `ConfigurationMetadataAnnotationProcessorTests` | 11.3x |
| `JooqTestWithAutoConfigureTestDatabaseIntegrationTests` | 11.3x |
| `SpringApplicationTests` | 10.1x |
| `BasicErrorControllerIntegrationTests` | 9.9x |
| `OriginTrackedYamlLoaderTests` | 9.4x |
| `QuartzEndpointWebIntegrationTests` | 9.3x |
| `ConditionalOnPropertyTests` | 7.4x |
| `WebFluxAutoConfigurationTests` | 6.9x |
| `SpringApplicationBuilderTests` | 6.8x |
| `DefaultErrorWebExceptionHandlerIntegrationTests` | 6.4x |
| `WebMvcHealthEndpointAdditionalPathIntegrationTests` | 6.4x |
| `DataRedisAutoConfigurationJedisTests` | 6.3x |

The jOOQ cluster dominates the slowest-vs-HotSpot list (3 of top 5) —
consistent with `JooqAutoConfigurationTests` itself HANGing today and being
a known-slow area (801s in the 08-04 residual round, close to its 1500s
ceiling). Worth a dedicated performance investigation if jOOQ startup speed
matters for real workloads; not pursued further this round (this was a
timing survey, not a perf debugging session).

## Method

```bash
# on the Azure host
cd /data/data/cratonvm/apps/spring-boot-suite-runner/.suite
awk -F'\t' 'NR>1{print $2"|"$3"\t"$7"\t"$8}' \
  results/craton-fullsuite-azure-20260805-s{1..8}/all-jit/results.tsv > /tmp/c805.tsv
awk -F'\t' 'NR>1{print $2"|"$3"\t"$7"\t"$8}' \
  results/craton-fullsuite-azure-20260802/all-jit/results.tsv > /tmp/c802.tsv
awk -F'\t' 'NR>1{print $2"|"$3"\t"$7"\t"$8}' \
  baseline/hotspot-baseline-latest.tsv > /tmp/hs.tsv
# then join on module|class in Python — see /tmp/timing_compare.py
```
