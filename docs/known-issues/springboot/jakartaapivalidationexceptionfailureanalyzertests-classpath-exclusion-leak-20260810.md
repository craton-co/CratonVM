# `JakartaApiValidationExceptionFailureAnalyzerTests` FAILs on all three collectors — a `ClassPathExclusions`-excluded jar's `META-INF/services` entry leaks back in through `ucl_find_resources`'s flat-scan fallback

**Status: OPEN — characterized via static log/source inspection, not yet confirmed
with a live repro. Filed 2026-08-10, from the 139-class non-passed union of the
2026-08-08 `default`/`g1`/`zgc` full-suite rerun (binaries
`cratonvm-{default,g1,zgc}-20260808f.exe`, `dev@6365de194`).**

**Likely the same bug family as
[`loggingsystemtests-logbackexcluded-serviceloader-flatscan-20260810.md`](loggingsystemtests-logbackexcluded-serviceloader-flatscan-20260810.md),
filed the same round from the same rerun — not confirmed identical, but the
shape matches closely enough to be worth checking together before fixing
either in isolation.** That doc's own root-cause section independently
names a documented precedent for this exact anti-pattern: `classloader.rs`'s
history has a "WF32-fix" for one resource where a loader-scoped resolution
was previously bypassed by an unconditional flat, process-wide classpath
scan. This doc's `ucl_find_resources` and the other doc's
`service_loader.rs::discover_providers` are two *different* call sites with
the same general shape — a correctly loader-scoped lookup (`local_urls` /
`getResources`-via-URL-array) computed first, then unconditionally
supplemented or overridden by a flat scan
(`cl_get_resources_impl`/`find_all_resource_bytes`) that walks every jar on
the process classpath regardless of the loader's own exclusions. If a
general fix is warranted (e.g. only fall back to the flat scan when the
receiver is confirmed to NOT be an isolated/modified-classpath loader,
rather than whenever the local result happens to be empty), it likely wants
to cover both sites — and possibly others sharing the same pattern that
weren't hit by this rerun's specific 139-class set.

## Symptom

`core/spring-boot` `org.springframework.boot.diagnostics.analyzer.
JakartaApiValidationExceptionFailureAnalyzerTests` FAILs — not a hang — under all
three collectors, fast and identical every time:

| Collector | Seconds | Result |
|---|---:|---|
| default | 6.834 | FAIL 1/2 |
| G1 | 7.421 | FAIL 1/2 |
| ZGC | 7.468 | FAIL 1/2 |

`results.tsv` rows in
`apps/spring-boot-suite-runner/.suite/results/craton-nonpassed-{default,g1,zgc}-20260808f-s1/all-jit/results.tsv`.
Logs:
`.../logs/core_spring-boot.org.springframework.boot.diagnostics.analyzer.JakartaApiV*-1fd0556e0459.{out,err}.log`
(one directory per collector).

`nonValidatedPropertiesTest()` passes on all three; `validatedPropertiesTest()`
fails on all three, with byte-for-byte the same failing assertion and the same
root WARN message in every `.out.log`:

```
WARN ... AnnotationConfigApplicationContext : Exception encountered during context
initialization - cancelling refresh attempt:
org.springframework.beans.factory.UnsatisfiedDependencyException: ... :
jakarta.validation.spi.ValidationProvider: Provider org.hibernate.validator.HibernateValidator not found

Failures (1):
  JUnit Jupiter:JakartaApiValidationExceptionFailureAnalyzerTests:validatedPropertiesTest()
    => org.assertj.core.error.AssertJMultipleFailuresError
```

**Collector-agnostic (reproduces under Generational, G1, and ZGC)** — identical
message, identical failing test, run time within 1s across all three. This is
unambiguous: a 7s deterministic FAIL with the same exception text on every
collector cannot be a GC-timing artifact.

Not a fresh regression signature either: `JakartaApiValidationExceptionFailureAnalyzerTests`
is mentioned in three prior docs
(`fixed-suite-bugs/springboot/properties-keyset-view-not-live-FIXED.md`,
`fixed-suite-bugs/springboot/classutils-forname-platform-loader-false-positive.md`,
`fixed-suite-bugs/springboot/core39-clusterD-lifecycle-ssl-validation-FIXED.md`,
`fixed-suite-bugs/springboot/spring-boot-core39-clusterb-diagnostics-process-base64-json-FIXED.md`,
`fixed-suite-bugs/springboot/string-chars-wrong-array-kind-and-properties-formfeed-escape-FIXED.md`),
but only ever as a **regression control** for unrelated fixes ("previously-solid
green", "`@ClassPathExclusions`-driven") — none of them owns or characterizes this
class's own behavior, and none names the `HibernateValidator not found` message.
This appears to be a new, undocumented failure for this class specifically (the
prior docs' snapshots all record it passing).

## What the test is actually designed to do

```java
@ClassPathExclusions("hibernate-validator-*.jar")
class JakartaApiValidationExceptionFailureAnalyzerTests {

    @Test
    void validatedPropertiesTest() {
        assertThatException().isThrownBy(() -> new AnnotationConfigApplicationContext(TestConfiguration.class).close())
            .satisfies((ex) -> assertThat(new ValidationExceptionFailureAnalyzer().analyze(ex)).isNotNull());
    }
    ...
    @ConfigurationProperties("test")
    @Validated
    static class TestProperties { }
}
```

(`apps/spring-boot/core/spring-boot/src/test/java/org/springframework/boot/diagnostics/analyzer/JakartaApiValidationExceptionFailureAnalyzerTests.java`)

The `@ClassPathExclusions("hibernate-validator-*.jar")` is **deliberate** — the test
exists specifically to exercise `ValidationExceptionFailureAnalyzer` for the case
where the Bean Validation API is present but no provider implementation is on the
classpath. `validatedPropertiesTest()` expects `AnnotationConfigApplicationContext`
construction to throw, and expects
`ValidationExceptionFailureAnalyzer().analyze(ex)` to return a non-null
`FailureAnalysis` for that thrown exception. So a `ValidationException` being
thrown is the *intended*, correct behavior on both HotSpot and CratonVM — the
failure is that `analyze()` apparently doesn't recognize it (an
`AssertJMultipleFailuresError` from the `.satisfies(...)` lambda is exactly what
`assertThat(null).isNotNull()` produces).

`ValidationExceptionFailureAnalyzer.analyze` only recognizes a thrown exception if:

```java
// apps/spring-boot/core/spring-boot/src/main/java/org/springframework/boot/diagnostics/analyzer/ValidationExceptionFailureAnalyzer.java
if (cause instanceof NoProviderFoundException || message.startsWith(JAVAX_MISSING_IMPLEMENTATION_MESSAGE)
        || message.startsWith(JAKARTA_MISSING_IMPLEMENTATION_MESSAGE)) {
```

i.e. a `jakarta.validation.NoProviderFoundException` (thrown by
`jakarta.validation.Validation` when `ServiceLoader` finds **zero** registered
`ValidationProvider`s), or a message starting "Unable to create a Configuration,
because no ... Bean Validation provider could be found".

The message actually observed —
`jakarta.validation.spi.ValidationProvider: Provider org.hibernate.validator.HibernateValidator
not found` — is neither of those. It is the JDK `java.util.ServiceLoader`'s own
error format (`ServiceLoader.fail(service, msg)` → `service.getName() + ": " + msg`,
`msg = "Provider " + cn + " not found"`), produced when `ServiceLoader` **finds a
`META-INF/services/jakarta.validation.spi.ValidationProvider` entry naming
`org.hibernate.validator.HibernateValidator`, but then cannot load that class**.
That is a different code path from "found zero providers" — and, critically, means
`ServiceLoader` found the SPI registration entry even though the excluding jar
(`hibernate-validator-*.jar`, which carries both the class *and* that registration
file together) is supposed to be invisible to this test's classloader.

## Root-cause hypothesis: `ucl_find_resources`'s flat-scan fallback reintroduces excluded resources

`JakartaApiValidationExceptionFailureAnalyzerTests` runs through
`ModifiedClassPathClassLoader`
(`apps/spring-boot/test-support/spring-boot-test-support/src/main/java/org/springframework/boot/testsupport/classpath/ModifiedClassPathClassLoader.java`),
a `URLClassLoader` subclass built by filtering the original classpath's URLs
through an Ant-pattern matcher (`ClassPathEntryFilter`) — `hibernate-validator-*.jar`
is dropped from its own `URL[]` entirely; its parent is `classLoader.getParent()`
(i.e. **not** the original unfiltered application loader, so normal parent-first
delegation cannot reintroduce it either). `ClassLoader.getResources(name)` is
therefore expected to see zero matches for
`META-INF/services/jakarta.validation.spi.ValidationProvider` through this loader —
exactly the "no provider" case the analyzer is coded for.

CratonVM's native implementation of this path
(`native-builtins/src/classloader.rs`) is where the leak is:

- `cl_get_resources_impl` (`classloader.rs:5209`) special-cases `URLClassLoader`
  receivers (`classloader.rs:5254`) to combine `parent.getResources(name)` with
  this receiver's own `ucl_find_resources` — explicitly documented as needed "to
  stay local rather than falling into the process-wide resource enumeration."
- `ucl_find_resources` (`classloader.rs:7829`) computes two candidate results:
  `local_urls` via `loader_local_resource_urls` (`classloader.rs:7285`), which is
  correctly scoped to *this loader's own* constructor URL list
  (`loader_constructor_url_paths`, `classloader.rs:7012`) and does exclude
  `hibernate-validator-*.jar` — and `std_enum`, obtained by calling
  `cl_get_resources_impl(ctx, args, false)` (`classloader.rs:7856`), whose own
  comment (`classloader.rs:5215`) says it "Walks EVERY classpath entry" — i.e. a
  **process-wide, unfiltered flat scan**, not scoped to any one loader.
- The merge at the end of `ucl_find_resources` (`classloader.rs:7893-7902`) is
  `local_ref.or(std_ref)` (when there is no custom-handler match, the common case
  here): **if the correctly-scoped local scan finds nothing, the function falls
  back to the flat process-wide scan instead of returning empty.**

For an ordinary `URLClassLoader` that is not deliberately excluding anything, that
fallback is presumably intentional/harmless (or even required for some other case
this file's history addresses). For a `ModifiedClassPathClassLoader` built
specifically to make `hibernate-validator-*.jar` invisible, it is exactly backwards:
a genuinely empty `local_urls` result **is** the correct, intended answer (the jar
was excluded on purpose), and substituting the flat scan hands back the excluded
jar's `META-INF/services` entry anyway. The `HibernateValidator` *class* itself
stays correctly excluded (`ModifiedClassPathClassLoader.loadClass`,
`ModifiedClassPathClassLoader.java:90-100`, throws `ClassNotFoundException` for excluded
packages), which is consistent with `ServiceLoader` finding the registration
(leaked via the resource-scan fallback) but then failing to instantiate the class
it names — producing exactly the observed `"Provider ...HibernateValidator not
found"` message instead of the `NoProviderFoundException` the analyzer (and the
test) expects.

## What is not established

- **Not confirmed with a live repro.** This is inferred from the observed message
  text, the test's source, and the `classloader.rs` resource-resolution code —
  no debug run was made (out of scope for this reconciliation pass, which is
  investigation/documentation only). A standalone run with
  `CRATONVM_DBG_UCLRES=1` (see `nbflags().dbg_uclres`, gating the
  `[UCLRES-DBG]` trace already wired into `loader_local_resource_urls`) against
  just this class would show directly whether `local_urls` comes back empty for
  `META-INF/services/jakarta.validation.spi.ValidationProvider` and whether
  `std_ref` is what the merged enumeration ends up returning.
- **Whether the flat-scan fallback is ever load-bearing for a legitimate case.**
  `ucl_find_resources`'s own comments describe several other fixes bundled into
  this function (custom-handler URLs, `close()` handling, a "pathing JAR" cache) —
  the `local_ref.or(std_ref)` fallback may exist to paper over a case where
  `loader_local_resource_urls`/`loader_constructor_url_paths` cannot yet determine
  a loader's own URLs (e.g. before `record_ucl_urls` has populated them) and
  falling back to the flat scan is better than returning nothing. Any fix needs to
  distinguish "local scan legitimately found zero because of an exclusion" from
  "local scan could not run at all," which this doc does not attempt.
- **Whether other `@ClassPathExclusions`-driven tests are affected the same way.**
  This mechanism is generic (any resource, not just `META-INF/services` files) and
  would apply to any test using `@ClassPathExclusions` on a jar that also ships a
  resource some other code does `getResources()`/`ServiceLoader.load()` against.
  Not surveyed here.

## Noise ruled out: the log4j2 `PluginBuilder` errors in `.err.log`

All three `.err.log`s also show `main ERROR Could not create plugin of type class
org.apache.logging.log4j.core.config.LoggerConfig$RootLogger ...
IllegalArgumentException: argument type mismatch` (in `PluginBuilder.injectFields`)
during the nested context's own log4j2 bootstrap. This is a known, already-documented
red herring — `docs/internal/fixed-suite-bugs/CRATONVM-SPRING-GENUINE-BUGLIST.md`
(~line 1582) confirms it is "transient/host-load-dependent, reproduces on 1 of 10+
repeat runs of the identical binary/command, and does NOT correlate with the actual
... failures." It is unrelated to the `ValidationProvider`/`analyze()` failure above
and was not investigated further here.

## Reproduce

```
CV=target/release/cratonvm.exe
CP="$(cat core/spring-boot/build/cratonvm-test-cp.txt);<sb>/sb-runner"
"$CV" --Xmx 2g --cp "$CP" SbRunner org.springframework.boot.diagnostics.analyzer.JakartaApiValidationExceptionFailureAnalyzerTests
```

## Affected classes

| Module | Class |
|---|---|
| `core/spring-boot` | `org.springframework.boot.diagnostics.analyzer.JakartaApiValidationExceptionFailureAnalyzerTests` |
