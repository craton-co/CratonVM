# core/spring-boot-test — config-data loading gaps, duplicate classpath scan results, missing PropertySource

**Status: OPEN — found 2026-07-17, none root-caused to a CratonVM file:line yet**

Six `core/spring-boot-test` classes fail/hang via 5 distinct signatures — grouped here as one doc since they're all small, related to test-context bootstrapping, and were investigated together, but they are **not confirmed to share a single root cause**. Treat each cluster below independently.

## Cluster A — classpath-root `application.properties` config-data not composed into Environment

| Class | Failure |
|---|---|
| `ConfigDataApplicationContextInitializerTests` | `expected: "bucket" but was: null` (`ConfigDataApplicationContextInitializerTests.java:47`) |
| `ConfigDataApplicationContextInitializerWithLegacySwitchTests` | same shape (`...WithLegacySwitchTests.java:49`) |

The test asserts `environment.getProperty("foo")).isEqualTo("bucket")`. `foo: bucket` is defined in `apps/spring-boot/core/spring-boot-test/src/test/resources/application.properties` (the classpath-root config-data file), loaded via `ConfigDataApplicationContextInitializer` as a JUnit `@ContextConfiguration(initializers=...)`. The property comes back `null` — this classpath-root `application.properties` config-data location isn't being composed into the `Environment` under CratonVM.

Logs: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/core_spring-boot-test.org.springframework.boot.test.context.ConfigDataApplicationContextInitializerTests.out.log`, `...ConfigDataApplicationContextInitia-ad3a926bdb6c.out.log`.

No existing doc found. **Not root-caused at VM-source level** — the specific config-data-loading code path wasn't identified.

## Cluster B — `@Value` placeholder left unresolved

`SpringBootTestCustomConfigNameTests`: `expected: "bar" but was: "${test.foo}"` (`SpringBootTestCustomConfigNameTests.java:41`).

Test uses `@SpringBootTest(properties = "spring.config.name=custom-config-name")` with field `@Value("${test.foo}") String foo`. Getting back the literal, unexpanded `${test.foo}` string (not an exception) means no `PropertySourcesPlaceholderConfigurer`/embedded-value-resolver ran — either the custom-named config file was never loaded, or the autoconfiguration that normally supplies the placeholder-configurer bean didn't fire.

Log: `.../SpringBootTestCustomConfigNameTests.out.log`.

Same general theme as Cluster A (a config-data loading gap) but a **different assertion shape** (raw placeholder vs. `null`) — not proven to be the identical mechanism. No existing doc found.

## Cluster C — classpath package scan finds sibling test classes' nested `@SpringBootConfiguration`s

`SpringBootContextLoaderAotTests`:

```
java.lang.IllegalStateException: Found multiple @SpringBootConfiguration annotated classes [...]
```

— found `SpringBootContextLoaderAotTests$ExampleConfig` **plus ~11 nested static config classes belonging to sibling top-level classes** `SpringBootContextLoaderTests` and `SpringBootTestUseMainMethodWithPropertiesTests` (same package, different outer class), via `AnnotatedClassFinder.scanPackage` → `TestContextAotGenerator.processAheadOfTime` (`SpringBootContextLoaderAotTests.java:61`).

Verified in source: all those nested classes really are `@SpringBootConfiguration`-annotated (`SpringBootContextLoaderTests.java:446,455,464,473,482,491,496,510,520`). Real HotSpot only finds one; CratonVM's package-wide classpath scan surfaces all of them — a classpath-resource-enumeration difference.

Log: `.../SpringBootContextLoaderAotTests.out.log`.

Weak, unconfirmed lead: `docs/internal/app-jvm-bugs/bug-hibernate-duplicate-persistence-unit-scan.md` describes a thematically similar (but **not confirmed matching**) `ClassLoader.getResources` duplicate-URL issue for Hibernate `persistence.xml` — different mechanism, different suite. Not root-caused.

## Cluster D — missing `"random"` PropertySource

`SpringBootContextLoaderTests`, test `propertySourceOrdering()` (only 1 of 26 tests in this class fails):

AssertJ `containsExactly` diff — actual `[..., "systemEnvironment", "applicationInfo"]` vs. expected `[..., "systemEnvironment", "random", "applicationInfo"]` (`SpringBootContextLoaderTests.java:168`). `RandomValuePropertySource` never gets registered/composed into the Environment's property-source list.

Log: `.../SpringBootContextLoaderTests.out.log`.

Standalone signature — not proven related to Cluster C despite being the same test class file. No existing doc found for `propertySourceOrdering`/`RandomValuePropertySource`.

## Cluster E — `DuplicateJsonObjectContextCustomizerFactoryTests` HANG — possible regression-in-place-of-fix

**Status: possible discrepancy against a same-day FIXED doc — flag for re-verification.**

HANG, timed out at exactly 300.026s. `.out.log` is empty (genuine hang, never printed JUnit output). `.err.log` (341 lines) contains only the pre-existing documented `gc::guard` noise pattern (`InterceptingExecutableInvoker`/`InvocationInterceptorChain`, `num_slots=0`) — **no** `NoSuchMethodError`, `Socket`/`SSLSocketFactory`/Aether/`HttpTransporter`/`ModifiedClassPathClassLoader` lines anywhere.

This class is explicitly named in
[`../../internal/fixed-suite-bugs/wrong-receiver-virtual-dispatch-corruption-cluster-FIXED.md`](../../internal/fixed-suite-bugs/wrong-receiver-virtual-dispatch-corruption-cluster-FIXED.md)
— in the 2026-07-16 rerun it **FAILED** (not hung) with `NoSuchMethodError:
java/lang/String.setOption(ILjava/lang/Object;)V` via
`ModifiedClassPathClassLoader.resolveCoordinates` → Aether → Apache
HttpClient → `SSLConnectionSocketFactory` → `SSLSocketFactory.createSocket()`
handing back a malformed 2-slot synthetic `Socket`
(`native-builtins/src/tls.rs:1568-1615`). The doc claims **FIXED
2026-07-17** by extending the real-network drop-filter in
`native-api/src/registry.rs` to also cover `javax/net/ssl/SSLSocketFactory`
`SyntheticStub` registrations.

**Verified the fix IS present and live in this exact worktree**:
`native-api/src/registry.rs:3560-3565` currently contains the
`|| (class_name == "javax/net/ssl/SSLSocketFactory" && self.current_category
== NativeKind::SyntheticStub)` clause exactly as the doc describes.
Worktree `HEAD` was `7dd6a5c240` (2026-07-17 18:54:45) when this rerun ran
— built after the fix landed.

**Hypothesis (not proven from the log alone):** this is very likely the
*same trigger path*, now hanging instead of crashing. Reasoning: the fix
makes `SSLSocketFactory.createSocket()` fall through to real Bridge
registrations that do genuine TLS work and produce a real, layout-correct
`SSLSocket` — meaning the Aether HTTPS artifact-download call in
`ModifiedClassPathClassLoader` now actually attempts real network I/O (DNS
+ TCP connect + TLS handshake to a Maven Central mirror) instead of
crashing on a malformed object before ever reaching the socket layer. A
real blocking `connect()`/`read()` that never completes (unreachable,
firewalled, or rate-limited network in this build environment) would
produce exactly this signature: zero output, zero error logs (CratonVM
doesn't log successful-but-blocked socket ops), silence until the hard
300s suite-runner timeout. **Not confirmed** — no thread stack dump was
taken, and no log line directly proves the blocked call is a socket op.

**Action for the FIXED doc**: its "Resolution" section makes no mention of
this class still not passing (via HANG instead of CRASH) post-fix — worth
a follow-up entry there once this hypothesis is confirmed or refuted with
a live repro + thread dump.

## Affected classes

- `core/spring-boot-test` | `ConfigDataApplicationContextInitializerTests` (Cluster A)
- `core/spring-boot-test` | `ConfigDataApplicationContextInitializerWithLegacySwitchTests` (Cluster A)
- `core/spring-boot-test` | `SpringBootTestCustomConfigNameTests` (Cluster B)
- `core/spring-boot-test` | `SpringBootContextLoaderAotTests` (Cluster C)
- `core/spring-boot-test` | `SpringBootContextLoaderTests` (Cluster D, 1 of 26 tests)
- `core/spring-boot-test` | `DuplicateJsonObjectContextCustomizerFactoryTests` (Cluster E, HANG)
