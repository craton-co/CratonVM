# `*TestWithoutJacksonIntegrationTests`: two-bug cluster under `@ClassPathExclusions("jackson-*.jar")`

**Status: Bug A FIXED 2026-07-21. Bug B OPEN — found 2026-07-21.**

## Symptom

`RestClientTestWithoutJacksonIntegrationTests` (`module/spring-boot-restclient-test`)
and `WebClientTestWithoutJacksonIntegrationTests`
(`module/spring-boot-webclient-test`) both exclude all Jackson jars via
`@ClassPathExclusions("jackson-*.jar")`, which routes them through Spring
Boot's `ModifiedClassPathExtension` (`ModifiedClassPathClassLoader` + a
nested, nested-`Launcher` re-execution of the test — the same mechanism
covered by
[`junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md`](../../internal/springboot/junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md)
and the (now-fixed)
[`observationregistry-conditionalonmissingbean-classpathexclusions-FIXED.md`](../../internal/springboot/observationregistry-conditionalonmissingbean-classpathexclusions-FIXED.md)).

Two distinct bugs were found in this area, discovered in sequence (fixing
Bug A unmasked Bug B):

## Bug A — `URLClassLoader.findClass` concurrent double-define race (FIXED)

### Symptom

```
Caused by: org.springframework.beans.factory.support... [elsewhere, wrapped]
java.lang.NoClassDefFoundError: org.springframework.boot.autoconfigure.condition.ConditionOutcome
```

with, when traced via `CRATONVM_DBG_NCDFE=1` (see
`vm/src/runtime/exceptions.rs::convert_class_not_found`), the underlying
cause:

```
WARN cratonvm_native_builtins::classloader: URLClassLoader.findClass(org/springframework/boot/autoconfigure/condition/ConditionOutcome) define failed: Linkage(IncompatibleClassChangeError { message: "class org/springframework/boot/autoconfigure/condition/ConditionOutcome already defined by user-defined(4) loader" })
```

fired from `OnClassCondition$ThreadedOutcomesResolver.lambda$new$0` →
`OnClassCondition$StandardOutcomesResolver.resolveOutcomes` →
`OnClassCondition$StandardOutcomesResolver.getOutcomes`.

### Root cause

Spring Boot's `OnClassCondition` (backing `@ConditionalOnClass`) evaluates
autoconfiguration candidates via a `ThreadedOutcomesResolver` — a
**background thread pool** running concurrently with the main thread — both
needing to resolve Spring Boot's own framework classes (like
`ConditionOutcome`) through the **same** `ModifiedClassPathClassLoader`
instance.

`native-builtins/src/classloader.rs`'s `ucl_try_define_local_class` (backing
`URLClassLoader.findClass`, reached for this loader topology via
`classloader_real.rs::cl_real_load_class_base`'s
`url_classloader_isolated_from_app` branch) had a classic check-then-act
race: it checked `find_loaded_class_for_loader` once at entry, then — with
no lock held across the gap — fetched the class's bytes and called
`define_class_full`. Two threads racing to load the same not-yet-defined
class through the same loader could both pass the initial check before
either called `define_class_full`; the second call then hit a **real**
`IncompatibleClassChangeError` ("already defined by this loader"), which
`convert_class_not_found` correctly wraps as a Java `NoClassDefFoundError`
— but the underlying condition (two threads legitimately both needing the
same class from the same loader) is exactly what a real JVM's per-class
`ClassLoader.getClassLoadingLock` monitor exists to serialize, and CratonVM
had no equivalent for this specific native-backed `findClass` path.

### Fix

Added `url_classloader_define_locks()` — a global
`Mutex<HashMap<(u32 loader_namespace_id, String class_name), Arc<(Mutex<bool>, Condvar)>>>`
in `native-builtins/src/classloader.rs`, mirroring the existing per-class-name
lock pattern already used by `SharedVm::load_class_concurrent`
(`vm/src/vm/vm_init.rs`) for the built-in delegation chain. `ucl_try_define_local_class`
now acquires the lock for its `(loader, name)` pair before the "already
defined?" check, waits (with a 30s timeout, re-checking on each wake) if
another thread is already defining the same class through the same loader,
and only proceeds to fetch bytes + `define_class_full` once it holds the
slot — releasing and notifying waiters via an RAII guard on every exit path
(including panics). Keyed by loader namespace id (not the loader `ObjectRef`
directly) plus name, so unrelated loaders defining a same-named class
concurrently are never serialized against each other.

### Verification

- `CRATONVM_DBG_NCDFE=1` trace confirms **zero** "already defined by"
  recurrences across 2 separate post-fix reruns (previously: fired on every
  run).
- Re-ran `RestClientTestWithoutJacksonIntegrationTests` 5x post-fix
  (`-Jit on` x4, includes 3 back-to-back) — the `ConditionOutcome`
  `NoClassDefFoundError` never recurred; the test now proceeds substantially
  further into context initialization (see Bug B below).
- 44-class regression sweep across `module/spring-boot-restclient`,
  `spring-boot-restclient-test`, `spring-boot-webclient`,
  `spring-boot-webclient-test` — all classes pass except the 2 Bug-B classes;
  one apparent `HANG` (`WebClientAutoConfigurationTests`, which does **not**
  use `@ClassPathExclusions` and so cannot touch the new lock at all)
  reproduced as a clean 37.8s `PASS` in isolation (`-Parallel 1`), confirming
  it was shared-host CPU contention (host load was 76-100% during the
  parallel run), not a deadlock introduced by the fix.

## Bug B — `RestTemplateBuilder` bean never registered (OPEN, not root-caused)

### Symptom

Once Bug A stopped masking it, both classes now fail (or, under host load,
occasionally still time out) with:

```
Caused by: org.springframework.beans.factory.NoSuchBeanDefinitionException: No qualifying bean of type 'org.springframework.boot.restclient.RestTemplateBuilder' available: expected at least 1 bean which qualifies as autowire candidate.
	at ... DefaultListableBeanFactory.raiseNoMatchingBeanFound
	at ... ConstructorResolver.resolveAutowiredArgument
```

(`WebClientTestWithoutJacksonIntegrationTests` likely has an analogous
`WebClient.Builder`/`ReactorResourceFactory`-shaped symptom — not yet
individually confirmed, but it fails/hangs identically under the same
`@ClassPathExclusions("jackson-*.jar")` mechanism.)

### What's been ruled out (2026-07-21 follow-up session — extensive, all evidence-backed)

- **Not the same concurrent-define race as Bug A.** Three separate
  `CRATONVM_DBG_NCDFE=1` reruns after the Bug A fix show **zero**
  `NoClassDefFoundError`/`IncompatibleClassChangeError` activity of any
  kind — the failure is a plain Spring bean-resolution miss, not a
  classloading exception.
- **Not present without the exclusion**: the sibling non-excluded
  autoconfiguration tests (`RestTemplateAutoConfigurationTests`,
  `RestClientAutoConfigurationTests`, etc. — see the regression sweep above)
  all pass cleanly.
- **Not `RestTemplate.class`/`HttpMessageConverters.class` failing to
  resolve.** Both `CRATONVM_FORNAME_TRACE=1` and the pre-existing
  `CRATONVM_S111_DBG=1` trace (`native-builtins/src/lang_class.rs`'s
  `native_class_for_name`, logging every `Class.forName`/`loadClass`
  resolution stage) show both classes resolving cleanly
  (`"loadClass(...) succeeded via invoke_virtual"`) through the
  `ModifiedClassPathClassLoader`, not just the outer `AppClassLoader`.
- **Not `@ConditionalOnClass` excluding `RestTemplateAutoConfiguration`.**
  Confirmed via a scratch-instrumented `OnClassCondition.java` (temporarily
  patched in the local `apps/spring-boot` checkout, reverted after use —
  see the methodology note below on why the FIRST attempt at this gave a
  false negative): `getMatchOutcome`'s slow path
  logged `onClasses=[RestTemplate, HttpMessageConverters] missing=[]` for
  `RestTemplateAutoConfiguration` under **both** the outer `AppClassLoader`
  and the inner `ModifiedClassPathClassLoader` — the condition genuinely
  matches (required classes present) every time it was observed.
- **Not `@Conditional(NotReactiveWebApplicationCondition)` excluding it
  either.** Same scratch-instrumented run (this time patching
  `AbstractNestedCondition.getMatchOutcome`, the framework base class
  `NotReactiveWebApplicationCondition` extends) logged
  `"NoneNestedConditions 0 matched 1 did not; ... did not find reactive web
  application classes"` — i.e. the app is correctly detected as
  non-reactive and the condition matches — for `RestTemplateAutoConfiguration`
  under both loaders.
- **Not `restTemplateBuilder()`/`restTemplateBuilderConfigurer()` failing
  to execute.** A scratch-instrumented `RestTemplateAutoConfiguration.java`
  (static initializer print + a print in each `@Bean` method) showed **all
  three fire successfully** — the class initializes, both bean methods run
  and return real objects with no exception — confirmed during at least the
  outer (successful) boot's timeframe (timestamp-correlated against the
  `err.log`'s adjacent `Post-clinit fixup` lines). **Not yet individually
  reconfirmed for the specific inner/failing boot** — see "still open"
  below.
- **Not the `get_or_create_bean_factory` loader-identity recovery-shim gap**
  (`native-builtins/src/spring_startup_bootstrap.rs`, already fixed once in
  commit `65d738bb5` for a different symptom/test —
  `WebFluxManagementChildContextConfigurationIntegrationTests`). Added a
  `CRATONVM_DBG_GOCBF=1` diagnostic (kept in the source, zero-cost when
  unset) logging every call's receiver class and whether it took the "fast
  path" (bytecode constructor already set `beanFactory`) or the "recovery"
  path (constructs a fresh `DefaultListableBeanFactory` via a
  loader-scoped lookup). **All 159 calls observed during a full
  `RestClientTestWithoutJacksonIntegrationTests` run show `fast_path=true`
  for `AnnotationConfigApplicationContext`** — the recovery shim is never
  exercised at all in this test, so a stale/wrong `DefaultListableBeanFactory`
  instance from *that* mechanism cannot be the cause here.

### Still open — what's genuinely unknown

The `RestTemplateAutoConfiguration` class initializes, both its `@Bean`
methods execute and return real objects, its class-level conditions all
correctly match (confirmed under the actual failing loader), and no
classloading exception occurs anywhere — yet
`ConstructorResolver.resolveAutowiredArgument` still cannot find a
`RestTemplateBuilder` bean when wiring `ExampleRestTemplateService`
(`UnsatisfiedDependencyException` → `NoSuchBeanDefinitionException`,
`DefaultListableBeanFactory.raiseNoMatchingBeanFound`). This means the
defect (if it is one, rather than a genuine race whose failing run simply
wasn't the one instrumented) sits in a very narrow gap: **something
happens between a successful `@Bean` method invocation and that bean
becoming visible to `getBeanNamesForType`/dependency resolution**, and it
was not reproduced under direct scratch instrumentation in this session —
every probe that fired showed things working correctly, yet the overall
test still failed in the same run.

The `DefaultListableBeanFactory`/`ConstructorResolver` classes themselves
are binary Maven dependencies (real `spring-beans`, not vendored source in
this repo), so they cannot be scratch-instrumented the same way; this is
part of why the gap couldn't be closed further with the tools available.

**Concrete next steps, in order of likely value:**

1. **Confirm whether the `@Bean` methods fire during the specific INNER
   (failing) boot, not just the outer one.** Re-add the
   `RestTemplateAutoConfiguration` class-body prints (remember: rebuild via
   `gradlew -p module\spring-boot-restclient testClasses jar`, NOT
   `testClasses` alone — see methodology note below) together with a print
   of `Thread.currentThread().getContextClassLoader()` at each print site,
   so the loader can be correlated directly instead of via log-file
   timestamps. If the inner boot's own copy of the bean methods never
   fires, the exclusion is happening earlier than any condition traced so
   far (worth checking `ConfigurationClassParser`'s own
   `partial`/`existingClass` merging logic, or whether the INNER instance
   is even reaching `@Configuration` class processing for this
   auto-configuration at all — possibly a duplicate-registration or
   `ConfigurationClass` identity issue given how many other loader-identity
   bugs this whole investigation family has turned up).
2. **Consider whether this is a genuine race whose specific failure window
   simply wasn't hit by the instrumented runs.** Every scratch probe in
   this session showed "things look correct" yet the run still failed —
   consistent with a race that manifests in a code path none of the probes
   happened to cover (e.g. bean-definition-map mutation from
   `OnClassCondition`'s background thread happening concurrently with the
   main thread's own bean registration, corrupting/dropping an entry in a
   way that doesn't leave a trace in any of the checks above).
3. Confirm against real HotSpot that this test passes there (expected,
   since the test's whole point is asserting things work without Jackson —
   not yet explicitly re-verified this session).
4. Check `WebClientTestWithoutJacksonIntegrationTests`'s exact failure mode
   independently — assumed to be the same bug family based on identical
   exclusion mechanism and timing, but its specific missing-bean type
   hasn't been individually confirmed.

### Methodology note (costly lesson this session — save future time)

`apps/spring-boot`'s Gradle modules are consumed via their **built JAR**
(`build/libs/<module>-4.1.0-SNAPSHOT.jar`) on the test classpath, not
`build/classes/java/main` directly. Running `gradlew -p <module>
testClasses` recompiles `.class` files but does **not** rebuild the `jar`
task — a scratch edit to Spring source can compile successfully and yet
have **zero effect** on the actual test run, silently, with no error
anywhere. Always follow up with `gradlew -p <module> jar` and verify via
`unzip -p build/libs/<module>-*.jar path/To/Class.class | grep -a
YOUR_MARKER` before trusting a "my print never fired" result. This cost
most of this session's early instrumentation rounds a wasted cycle before
being caught — the exact same class of trap as the `CV_BIN`
env-var-propagation issue noted in dev commit `6b9c0776b`.

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -Jit on -TimeoutSec 180 -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -ClassList <a TSV with header "module`tclass" and the 2 rows below>
```
```
module	class
module/spring-boot-restclient-test	org.springframework.boot.restclient.test.autoconfigure.RestClientTestWithoutJacksonIntegrationTests
module/spring-boot-webclient-test	org.springframework.boot.webclient.test.autoconfigure.WebClientTestWithoutJacksonIntegrationTests
```

Diagnostic: `CRATONVM_DBG_NCDFE=1` (pre-existing, in
`vm/src/runtime/exceptions.rs::convert_class_not_found`) prints
`[NCDFE] class=... err=...` plus a 20-frame call stack for every
`NoClassDefFoundError`/converted linkage failure at an opcode boundary —
useful for Bug A-style issues but shows nothing for Bug B, confirming Bug B
is not a classloading exception.

## Affected classes

| Module | Class | Status |
|---|---|---|
| `module/spring-boot-restclient-test` | `org.springframework.boot.restclient.test.autoconfigure.RestClientTestWithoutJacksonIntegrationTests` | Bug A fixed; Bug B OPEN |
| `module/spring-boot-webclient-test` | `org.springframework.boot.webclient.test.autoconfigure.WebClientTestWithoutJacksonIntegrationTests` | Bug A fixed; Bug B OPEN |
