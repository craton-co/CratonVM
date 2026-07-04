# SmallRye Config missing built-in Converters for Charset / MemorySize

Status: open — highest-impact single lever found in this sweep (blocks 341+ classes)

Date observed: 2026-07-04

## Summary

Every Keycloak `testsuite/model` (`tests/base`, `org.keycloak.tests.*`) JUnit
class dies identically in `@BeforeAll`/container setup, before any `@Test`
method runs:

```
io.smallrye.config.ConfigValidationException: Configuration validation failed:
    java.lang.IllegalArgumentException: SRCFG00013: No Converter registered for class java.nio.charset.Charset
    java.lang.IllegalArgumentException: SRCFG00013: No Converter registered for class io.quarkus.runtime.configuration.MemorySize

io.smallrye.config.SmallRyeConfig.buildMappings(SmallRyeConfig.java:172)
io.smallrye.config.SmallRyeConfig.<init>(SmallRyeConfig.java:126)
io.smallrye.config.SmallRyeConfigBuilder.build(SmallRyeConfigBuilder.java:785)
io.quarkus.runtime.logging.LoggingSetupRecorder.handleFailedStart(LoggingSetupRecorder.java:122)
io.quarkus.runtime.logging.LoggingSetupRecorder.handleFailedStart(LoggingSetupRecorder.java:93)
org.keycloak.testframework.LogHandler.initializeQuarkusLogging(...)
org.keycloak.testframework.LogHandler.<init>(...)
org.keycloak.testframework.KeycloakIntegrationTestExtension.lambda$getLogHandler$0(...)
org.keycloak.testframework.KeycloakIntegrationTestExtension.beforeAll(...)
```

`KCRUNNER_RESULT tests=0 failed=0 aborted=0 skipped=0 containersFailed=1` in
every case — the class is reported FAIL but genuinely zero test methods ever
run.

This is the same failure that later blocks `quarkus/deployment` classes once
two other, now-fixed, earlier blockers are cleared (see
`docs/internal/fixed-suite-bugs/smallrye-getconfigmapping-1arg-bare-interface-abstractmethoderror.md`)
— fixing the two `quarkus/deployment` CRASH-class blockers just advances
those classes far enough to hit this exact same converter gap. It is a single
shared root cause, not per-class breakage.

## Scale

- `tests/base` (`org.keycloak.tests.*`): **341/341 FAIL rows — exhaustively
  confirmed** (every single FAIL row in this module checked directly, not
  sampled; the two rows that initially looked like exceptions turned out to
  be a log-lookup script bug on truncated/hashed log filenames for two
  long class names — both carry the identical ConfigValidationException on
  direct inspection). 100% of this module's FAILs share this one root cause.
- `quarkus/deployment` (`PersistenceXmlDatasourcesTest` and siblings): reached
  after fixing the two CRASH-class blockers ahead of it (see above);
  confirmed via direct repro on this exact class.
- `quarkus/runtime` (`LoggingConfigurationTest`, `TelemetryConfigurationTest`,
  `IgnoredArtifactsTest`): plausibly related — all show config-resolution
  divergences from expected values, filed as their own docs
  (`quarkus-runtime-logging-wildcard-debug-level-null.md`,
  `quarkus-runtime-logging-getpropertynames-garbage-key.md`,
  `quarkus-runtime-telemetry-service-name-wrong-value.md`,
  `quarkus-runtime-ignoredartifacts-multipledatasources-boolean.md`); not
  confirmed to share this exact SRCFG00013 path, flagged for re-triage after
  this is fixed.

## Root cause (not yet pinned)

Real SmallRye Config ships built-in `Converter` implementations for common
non-primitive types (`Charset`, `MemorySize`, `InetAddress`, `Pattern`, …) via
`io.smallrye.config.Converters` — a class whose converter table is populated
either by a static initializer (many entries are method-reference lambdas,
e.g. effectively `Charset::forName`) or via `ServiceLoader`-discovered
`META-INF/services/org.eclipse.microprofile.config.spi.Converter` entries
bundled in `smallrye-config-core-*.jar`. Under CratonVM, at least these two
types' converters are missing by the time `SmallRyeConfig.buildMappings`
validates every `@ConfigMapping` property's type — everything else about the
mapping (including other converters) apparently works, since only these two
throw.

Two working hypotheses, not yet distinguished:

1. **Lambda/method-reference gap**: if `Converters`' built-in table is
   populated via `invokedynamic`-based method references in a static
   initializer, a CratonVM `LambdaMetafactory`/`invokedynamic` bug for this
   specific lambda shape could silently drop just these entries while others
   (built via different call shapes) succeed. CratonVM has prior history of
   narrow `invokedynamic`/`LambdaMetafactory` gaps (see
   `reference_reflective_lambdametafactory`,
   `reference_mh_bound_virtual_unbox` in project memory).
2. **ServiceLoader gap**: if these two converters are registered via
   `META-INF/services/org.eclipse.microprofile.config.spi.Converter` inside
   `smallrye-config-core-*.jar` specifically (as opposed to a hardcoded Java
   map), this could be the same class of bug as the `crypto/fips1402`
   `CryptoProvider` ServiceLoader gap in
   `crypto-fips1402-cryptoprovider-serviceloader-empty.md` — CratonVM's
   `ServiceLoader`/classpath-resource-discovery not finding a services file
   inside a specific jar under this harness's classpath assembly.

No native shim/override touches `SmallRyeConfigBuilder`'s converter-discovery
path today (checked — `native-builtins/src/phases_late.rs` only shims
`getConfigMapping` itself, not converter registration), so whatever's
happening is either genuine CratonVM behavior on unmodified SmallRye
bytecode, or a classpath-assembly gap in how the test harness builds this
module's runtime classpath.

## Next steps

1. `javap -c` decompile `io.smallrye.config.Converters.<clinit>` (from
   `smallrye-config-core-3.17.2.jar`, in `~/.m2/repository/io/smallrye/config/
   smallrye-config-core/3.17.2/`) to see exactly how the `Charset`/
   `MemorySize` entries are constructed, to pick between the two hypotheses
   above.
2. Write a small isolated repro: `new SmallRyeConfigBuilder().build()` then
   `config.getConverter(Charset.class)` — if this alone reproduces the gap
   without any Keycloak/Quarkus involvement, it narrows the bug to
   SmallRye-Config-in-isolation under CratonVM, which is a much smaller,
   more tractable repro than the full Keycloak test harness.

## Repro

```
ssh victor@20.84.156.31   # Azure build host, see reference_azure_build_host
cd /data/wt-keycloak-full-20260704
apps/keycloak-suite-runner/run-keycloak-suite.ps1 -Vm craton \
  -ClassList <(printf 'module\tclass\ntests/base\torg.keycloak.tests.admin.AdminConsoleTest\n') \
  -TimeoutSec 60 -RunName repro-charset-converter \
  -Exe target/release/cratonvm-kcfull1124 -JdkHome /home/victor/jdk25
```

## Evidence

Full run: `/data/wt-keycloak-full-20260704/apps/keycloak-suite-runner/.suite/results/kcfull-others-1124-20260704-v2/others-jit/` (results.tsv + per-class logs), run 2026-07-04, wall time 2469s for 1124 classes.
