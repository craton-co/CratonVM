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

### What's been ruled out

- **Not the same concurrent-define race as Bug A.** Two separate
  `CRATONVM_DBG_NCDFE=1` reruns after the Bug A fix show **zero**
  `NoClassDefFoundError`/`IncompatibleClassChangeError` activity of any
  kind — the failure is a plain Spring bean-resolution miss, not a
  classloading exception.
- **Not present without the exclusion**: the sibling non-excluded
  autoconfiguration tests (`RestTemplateAutoConfigurationTests`,
  `RestClientAutoConfigurationTests`, etc. — see the regression sweep above)
  all pass cleanly.

### Leading hypothesis (unconfirmed — starting point for next session)

`org.springframework.boot.restclient.autoconfigure.RestTemplateAutoConfiguration`
is gated `@ConditionalOnClass({ RestTemplate.class, HttpMessageConverters.class })`.
`RestTemplateBuilder` is one of its `@Bean` methods
(`restTemplateBuilder()`, itself `@ConditionalOnMissingBean`). A
`NoSuchBeanDefinitionException` for `RestTemplateBuilder` with **no**
classloading exception anywhere in the trace is consistent with the entire
`RestTemplateAutoConfiguration` class having been excluded outright by its
class-level `@ConditionalOnClass` — i.e. `OnClassCondition`'s fast-path
`ClassNameFilter.MISSING` check (`ClassUtils.isPresent`, which deliberately
swallows any `Throwable` and reports "absent" — see
`OnBeanCondition.getOutcome`/`OnClassCondition`'s own metadata-driven fast
path) incorrectly judged `RestTemplate.class` or `HttpMessageConverters.class`
as missing, even though neither is excluded by `@ClassPathExclusions("jackson-*.jar")`
and both are ordinary, always-present Spring Framework classes.

Since `ClassUtils.isPresent` swallows exceptions by design, this could still
be a **residual concurrency issue** distinct from Bug A — e.g. a transient
resolution hiccup on the `ThreadedOutcomesResolver` background thread for
`RestTemplate.class`/`HttpMessageConverters.class` specifically (not
`ConditionOutcome`) that gets silently absorbed into a "class missing"
verdict instead of surfacing as a loud crash. Worth checking:

1. Re-run with `CRATONVM_DBG_FORNAME_TRACE=1` (or add a similar targeted
   trace for `RestTemplate`/`HttpMessageConverters`) to see whether either
   class resolution genuinely fails at some point during the test, and via
   which thread/mechanism.
2. Check whether `OnClassCondition`'s background-thread pool
   (`ThreadedOutcomesResolver`) itself has any other CratonVM-side
   thread-safety gap beyond the one fixed in Bug A — e.g. in
   `find_loaded_class_for_loader`'s own read path, or in whatever resolves
   `RestTemplate`/`HttpMessageConverters` specifically (they may go through
   a different code path than `ucl_try_define_local_class` if already
   loaded under the Application loader before the isolated loader's
   delegation kicks in).
3. Confirm against real HotSpot that this test passes there (expected,
   since the test's whole point is asserting things work without Jackson —
   but not yet explicitly re-verified in this investigation).
4. Check `WebClientTestWithoutJacksonIntegrationTests`'s exact failure mode
   independently — this doc assumes it's the same bug family based on
   identical exclusion mechanism and timing, but its specific missing-bean
   type hasn't been individually confirmed.

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
