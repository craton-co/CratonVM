# Keycloak SmallRye Config Charset / MemorySize converter gap

Status: fixed on 2026-07-04 by making `io.quarkus.runtime.logging.LoggingSetupRecorder.handleFailedStart` build its transient logging config with discovered Quarkus converters.

## Fix

CratonVM now routes Quarkus `LoggingSetupRecorder.handleFailedStart()` through a late native bridge that mirrors the real recorder flow, but explicitly calls `SmallRyeConfigBuilder.addDiscoveredConverters()` before mapping validation. This preserves the `io.quarkus.runtime.configuration.CharsetConverter` and `MemorySizeConverter` registrations needed by `LogRuntimeConfig` mapping validation.

## Validation

- Direct repro: `ProbeLogHandler` under CratonVM now exits `rc=0` and prints `loghandler-ok`; before the fix it failed with `SRCFG00013` for `java.nio.charset.Charset` and `io.quarkus.runtime.configuration.MemorySize`.
- Suite probe: `tests/base :: org.keycloak.tests.admin.identityprovider.IdentityProviderMapperTest` no longer fails in `beforeAll` with `ConfigValidationException`; it starts all 5 test methods and now fails later with `Failed to resolve next requested instance to deploy`. That bug is now fixed too (root cause: `LinkedList.addAll(LinkedList)`, see `keycloak-testframework-linkedlist-addall-deployrequestedinstances-FIXED.md`); the residual is `docs/known-issues/keycloak-07-04/keycloak-testframework-phaser-forkjoinpool-sisu-hang.md`.

---

# SmallRye Config missing built-in Converters for Charset / MemorySize

Historical original status: open - same-day deep dive narrowed downstream
impact and ruled out ServiceLoader/reflection as the direct mechanism. Kept
for provenance; resolved by the fix above.

Date observed: 2026-07-04
Date investigated: 2026-07-04 (same-day deep dive, ~3 hours)

## Summary

Every Keycloak `tests/base` (`org.keycloak.tests.*`) JUnit class dies
identically in `@BeforeAll`/container setup, before any `@Test` method runs:

```
io.smallrye.config.ConfigValidationException: Configuration validation failed:
    java.lang.IllegalArgumentException: SRCFG00013: No Converter registered for class java.nio.charset.Charset
    java.lang.IllegalArgumentException: SRCFG00013: No Converter registered for class io.quarkus.runtime.configuration.MemorySize

io.smallrye.config.SmallRyeConfig.buildMappings(SmallRyeConfig.java:172)
io.smallrye.config.SmallRyeConfig.<init>(SmallRyeConfig.java:126)
io.smallrye.config.SmallRyeConfigBuilder.build(SmallRyeConfigBuilder.java:785)
io.quarkus.runtime.logging.LoggingSetupRecorder.handleFailedStart(LoggingSetupRecorder.java:122)
io.quarkus.runtime.logging.LoggingSetupRecorder.handleFailedStart(LoggingSetupRecorder.java:93)
org.keycloak.testframework.LogHandler.initializeQuarkusLogging(LogHandler.java:41)
org.keycloak.testframework.LogHandler.<init>(LogHandler.java:29)
org.keycloak.testframework.KeycloakIntegrationTestExtension.lambda$getLogHandler$0(...)
org.keycloak.testframework.KeycloakIntegrationTestExtension.beforeAll(...)
```

341/341 `tests/base` FAIL rows share this exact signature (exhaustively
confirmed, every row checked directly against its log, not sampled).

## ⚠️ Corrected impact — read this before working on the fix

**Fixing this bug would NOT flip any of the 341 `tests/base` classes to
PASS in this test harness environment.** Confirmed by running the identical
classes under real HotSpot (`-Vm hotspot`) in the same harness: HotSpot's
`beforeAll` genuinely succeeds (no trace of Charset/MemorySize/SRCFG00013
anywhere in its log — grepped both stdout and stderr, zero hits), but the
SAME classes then fail per-test in `beforeEach` with:

```
java.lang.RuntimeException: Failed to resolve artifact: org.keycloak.testframework:keycloak-test-framework-remote-providers
    org.keycloak.it.utils.Maven.getArtifact(Maven.java:88)
    org.keycloak.it.utils.Maven.resolveArtifact(Maven.java:51)
    org.keycloak.testframework.server.ProviderDeployer.getDependencyPath(ProviderDeployer.java:119)
    org.keycloak.testframework.server.ProviderDeployer.updateDependencies(ProviderDeployer.java:43)
    org.keycloak.testframework.server.DistributionKeycloakServer.start(DistributionKeycloakServer.java:93)
Caused by: java.lang.RuntimeException: Failed to resolve artifact [...] from project [org.keycloak:keycloak-parent:pom:999.0.0-SNAPSHOT] dependency graph
```

This is a Maven reactor/dependency-graph resolution failure — an
environment/harness gap (this test harness doesn't do a full `mvn install`
of the whole Keycloak reactor, or lacks the network/repo state Maven's
resolver needs), **not a CratonVM bug**, and not something either VM can fix
in isolated Rust code. So even a perfect fix for the Charset/MemorySize gap
would just move these 341 classes' failure point later (from `beforeAll` to
`beforeEach`), converging with HotSpot's own failure — not unlocking a
single PASS. The original "single biggest lever, 341 classes" framing from
the initial triage was incorrect; this bug's practical value is much lower
than believed, though it remains a genuine, real CratonVM-vs-HotSpot
behavioral divergence worth fixing on its own correctness merits.

## Investigation (extensive, root cause narrowed but not conclusively pinned)

**Ruled out — CratonVM's ServiceLoader/reflection machinery is NOT broken.**
Built a standalone repro (`ConverterProbe.java`, tests `getConverter()` for
all 12 types Quarkus registers via
`../../../../apps/META-INF/services/org.eclipse.microprofile.config.spi.Converter` in
`quarkus-core-3.33.1.1.jar`) and ran it under CratonVM against `tests/base`'s
*exact* classpath (`smallrye-config-core-3.16.0.jar` + `quarkus-core-3.33.1.1.jar`).
With `.addDiscoveredConverters()` explicitly called, **all 11 real converters
resolve correctly** (`InetSocketAddress`, `Charset`, `InetAddress`, `Pattern`,
`Path`, `Duration`, `MemorySize`, `Locale`, `ZoneId`, `Level` all FOUND). This
conclusively rules out a ServiceLoader/classpath/jar-resolution bug on
CratonVM's side for this exact mechanism — when discovery is actually
invoked, CratonVM finds and instantiates every provider correctly, matching
HotSpot byte-for-byte.

**The actual failing code path never calls `addDiscoveredConverters()` at
all — on either VM, per the bytecode.** Decompiled the full chain:
- `LogHandler.initializeQuarkusLogging()` (Keycloak's own source, read
  directly — not decompiled) calls `LoggingSetupRecorder.handleFailedStart()`
  **unconditionally**, with no try/catch, explicitly "abusing" this
  Quarkus-internal diagnostic method as a lightweight logging-setup helper
  (see the class comment: "We do not care about Config that was created by
  Quarkus' TestConfigProviderResolver... relying on Quarkus' Config is not
  necessary and might be fragile").
- `LoggingSetupRecorder.handleFailedStart()` builds `new SmallRyeConfigBuilder()
  .withCustomizers(new QuarkusConfigBuilderCustomizer()).withMapping(LogBuildTimeConfig.class)
  .withMapping(LogRuntimeConfig.class).withMapping(ConsoleRuntimeConfig.class)
  .withSources(new LoggingSetupRecorder$1(existingConfig)).build()`.
- `QuarkusConfigBuilderCustomizer.configBuilder()` only does
  `.withDefaultValue(...)`, `.withInterceptorFactories(...)` ×3, and
  `.withMappingIgnore("quarkus.**")` — **no converter registration of any
  kind.**
- `SmallRyeConfigBuilder`'s constructor sets `addDiscoveredConverters = false`
  by default (confirmed via bytecode: `iconst_0; putfield addDiscoveredConverters`).
- **Runtime confirmation**: reran the real failing test with
  `CRATONVM_DIAG_SERVICELOADER=1` (an existing env-gated diagnostic in
  `../../../../native-builtins/src/service_loader.rs`) — the full trace shows **zero**
  `ServiceLoader.load(org.eclipse.microprofile.config.spi.Converter, ...)`
  calls anywhere during the entire failing test run. Whatever gives HotSpot
  its Charset/MemorySize converters here, it is not this mechanism, on
  either VM.
- **Runtime confirmation of the actual failure sequence**: reran with
  `CRATONVM_DBG_ATHROW=1` (dumps every Java exception thrown, in order) — the
  very first exception in the entire 740-line trace is
  `NoSuchMethodException: java.nio.charset.Charset.of(java.lang.String)`,
  followed by 5 more `NoSuchMethodException`s for `of(CharSequence)`,
  `valueOf(String)`, `valueOf(CharSequence)`, `parse(String)`,
  `parse(CharSequence)` — this is SmallRye's own `Converters$Implicit`
  reflection-based fallback trying (and correctly failing, since real
  `Charset` has none of these methods — only `forName`) every conventional
  factory-method name before finally throwing SRCFG00013. This is 100%
  correct, expected SmallRye behavior *given* that no explicit Charset
  converter was ever registered on this builder instance — there is no
  CratonVM-specific exception-handling bug in this sequence.

**What remains unexplained**: found Keycloak's own
`org.keycloak.testframework.config.Config.initConfig()` (a separate,
JVM-wide-singleton `SmallRyeConfig`, `private static final ... = initConfig()`)
which *does* explicitly register `new CharsetConverter()`, `new
MemorySizeConverter()`, `new InetSocketAddressConverter()` via
`.withConverters(...)`. `handleFailedStart()` retrieves this exact instance
via `ConfigProvider.getConfig()` (same-classloader lookup, registered moments
earlier by `initializeQuarkusLogging`) — but only uses it as a **value
source** (wrapped in a `ConfigSource`), never as a converter-registry
template for its own freshly-built `SmallRyeConfigBuilder`. Converters and
ConfigSources are separate SmallRye concerns; wrapping a config as a source
does not transfer its converters. Also ruled out a shared/global converter
cache: `SmallRyeConfig.getConverterOrNull` reads `this.converters` — a
**per-instance** field-backed `Map`, populated only from what that specific
builder was given; there is no cross-instance sharing mechanism it could
fall back to.

Given all of the above is pure Java bytecode logic with no CratonVM native
override anywhere in the path (confirmed: `../../../../native-builtins/src/phases_late.rs`
only shims `getConfigMapping`, nothing here), **this should mechanically fail
identically on any conforming JVM** — yet it demonstrably does not fail on
HotSpot for these classes in this harness. The remaining candidate
explanations, none confirmed:
1. A genuinely CratonVM-specific difference in class-initialization timing/order
   that causes some *other*, not-yet-identified code path to run (or not run)
   before `handleFailedStart`, indirectly affecting converter availability.
2. A latent bug in Keycloak's own test framework (the `handleFailedStart()`
   abuse pattern) that happens not to manifest on HotSpot in this exact
   harness for reasons unrelated to VM semantics (e.g. it may depend on
   something in a real Maven Surefire fork's environment/sysprops that this
   harness's direct JUnit-Platform-Launcher invocation doesn't replicate
   identically for both VMs, and CratonVM's manifestation is a side effect
   rather than a direct cause).
3. Something in `ConfigProviderResolver.instance()`'s own lazy singleton
   initialization (confirmed via SL-DBG to correctly ServiceLoader-discover
   `SmallRyeConfigProviderResolver` on CratonVM) differs subtly in a way not
   yet traced.

## Next steps for whoever picks this up

Given the corrected (low) impact above, this is no longer urgent, but if
pursued: a HotSpot-side JFR class-load/method-entry trace comparison against
the same CratonVM `CRATONVM_DBG_ATHROW=1` trace, specifically watching for
what (if anything) populates converters before `handleFailedStart` runs on
HotSpot, is the most direct remaining lever. Given the fix wouldn't unlock
any passing tests in this environment regardless, this is better prioritized
below the Maven-artifact-resolution harness gap and the other, more isolated
findings in this sweep.

## Repro

```
ssh -i "C:\Users\Victor\.ssh\azure.pem" -o IdentitiesOnly=yes victor@20.84.156.31
cd /data/wt-converterfix-20260704   # or any fresh worktree off dev with the keycloak checkout rsynced in
CRATONVM_DBG_ATHROW=1 pwsh -NoProfile -ExecutionPolicy Bypass -File apps/keycloak-suite-runner/run-keycloak-suite.ps1 -Vm craton \
  -ClassList <(printf 'module\tclass\ntests/base\torg.keycloak.tests.vault.KeycloakKeystoreVaultTest\n') \
  -TimeoutSec 60 -RunName repro-charset-athrow \
  -Exe target/release/cratonvm-converterfix -JdkHome /home/victor/jdk25

# HotSpot oracle for comparison (succeeds at beforeAll, fails later at beforeEach on the Maven artifact gap):
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/keycloak-suite-runner/run-keycloak-suite.ps1 -Vm hotspot \
  -ClassList <(printf 'module\tclass\ntests/base\torg.keycloak.tests.vault.KeycloakKeystoreVaultTest\n') \
  -TimeoutSec 60 -RunName repro-charset-hotspot -JdkHome /home/victor/jdk25
```

## Evidence

- Original full sweep: `/data/wt-keycloak-full-20260704/apps/keycloak-suite-runner/.suite/results/kcfull-others-1124-20260704-v2/others-jit/` (2026-07-04).
- This investigation's runs: `/data/wt-converterfix-20260704/apps/keycloak-suite-runner/.suite/results/{repro-charset-2,repro-charset-hotspot,repro-charset-nsme,repro-charset-sldbg,repro-athrow}/` on the Azure build host, branch `fix/smallrye-config-converters-20260704` (off `dev`).
- Keycloak source read directly: `apps/keycloak/test-framework/core/src/main/java/org/keycloak/testframework/{LogHandler,KeycloakIntegrationTestExtension,config/Config}.java`.
