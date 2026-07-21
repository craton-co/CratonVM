# `*TestWithoutJacksonIntegrationTests`: two-bug cluster under `@ClassPathExclusions("jackson-*.jar")`

**Status: Bug A FIXED 2026-07-21. Bug B OPEN — found 2026-07-21, extensively
re-investigated 2026-07-21 (session #2, see below) — narrowed significantly
but still not root-caused.**

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

### 2026-07-21 follow-up session #2 — confirmed WHERE it breaks, still not WHY

Picked up next-step #1 from above (confirm whether `@Bean` methods fire
during the INNER boot specifically, using CCL correlation instead of
timestamps). Worked in a fresh worktree
(`C:\craton\CratonVM-resttemplatebuilder-bugb-20260721`, branch
`fix/resttemplatebuilder-bugb-beanvisibility-20260721`, binary
`cratonvm-bugb.exe`) so as not to disturb the shared `apps/spring-boot`
checkout for long; all scratch edits below were reverted and the jars
rebuilt from pristine sources before this session ended — `apps/spring-boot`
should be clean (verify with `grep -rn CVDBG apps/spring-boot/module/spring-boot-restclient*` before trusting that if picking this up later).

**Confirmed, with direct CCL-correlated evidence (not timestamp inference):**

- `RestTemplateAutoConfiguration`'s `@Bean` methods — **both** the existing
  `@Lazy` ones and a temporary **non-lazy, eager** `@Bean` added purely as a
  probe (so absence-of-firing can't be blamed on nothing ever requesting the
  lazy bean) — **never fire under the loader that actually runs the test**
  (`org.springframework.boot.testsupport.classpath.ModifiedClassPathClassLoader`).
  They fire exactly once, tied to an *earlier*, unrelated, fully-successful
  boot that runs under the plain `jdk.internal.loader.ClassLoaders$AppClassLoader`
  (confirmed via a `RestTemplateAutoConfiguration` static-initializer print
  logging `Thread.currentThread().getContextClassLoader()` +
  `RestTemplateAutoConfiguration.class.getClassLoader()`; also confirmed via
  `ExampleWebClientApplication`'s own static initializer, which DOES
  reliably re-fire under a fresh `ModifiedClassPathClassLoader@xxxx` each
  real (inner) run — so the inner boot is definitely happening and definitely
  loading fresh classes, just not registering/invoking `RestTemplateAutoConfiguration`'s
  bean methods).
- This is **not specific to `RestTemplateAutoConfiguration`** or to
  `@Import`-based auto-configuration. A hand-added nested `@TestConfiguration`
  static class (`CvdbgScanConfig`) on the test class itself, containing a
  plain `@Bean BeanDefinitionRegistryPostProcessor` (guaranteed to run during
  `invokeBeanFactoryPostProcessors`, **before** any singleton instantiation
  — so its non-firing can't be explained by `preInstantiateSingletons`
  aborting early on a different bean) **also never fires** under the inner
  loader. Whatever's broken affects `@Configuration`/`@Bean`-method
  processing broadly under this loader topology, not one autoconfiguration
  class specifically.
- **New, reusable diagnostic technique** — a temporary trace-and-delegate
  native override on the *concrete* `DefaultListableBeanFactory.registerBeanDefinition`
  (as opposed to the existing interface-only stubs at
  `dlbf_register_bean_definition`/`dlbf_contains_bean_definition`, which are
  correctly gated to the *synthetic*-DLBF-fallback case only and confirmed
  to **never** fire in this investigation — ruling that fallback path out
  again, definitively). The override always calls through to the real
  bytecode via `ctx.invoke_virtual_bytecode_only(...)` so it's
  behavior-preserving; it just observes. Needs entries in **both**
  `force_native_over_real_jdk_bytecode` (`vm/src/runtime/interpreter.rs`)
  and the `check_override` chain in `invoke_on_class_shared_inner`
  (`vm/src/vm/vm_exec.rs`) per the usual dual-gate requirement. With this in
  place (env-gated behind a scratch `CRATONVM_DBG_BUGB=1`), **zero**
  `registerBeanDefinition` calls of *any* bean name are observed anywhere
  after the inner boot's `ModifiedClassPathClassLoader` marker fires, right
  up until the `NoSuchBeanDefinitionException` failure — not for
  `restTemplateBuilder`, not for the Jackson/RestClient autoconfiguration
  beans, nothing. (Contrast with the outer/successful boot, where this same
  trace shows the full, correctly-ordered ~100-entry registration sequence
  including `restTemplateBuilder` itself.)
- `get_or_create_bean_factory`'s synthetic-fallback path is **not** involved
  (`CRATONVM_DBG_GOCBF=1` shows `fast_path=true` for every call in the inner
  boot too — the real `DefaultListableBeanFactory()` constructor always
  succeeds there, consistent with the original session's finding).
- Despite zero observed `registerBeanDefinition` calls, `exampleRestTemplateService`
  (the test's explicit `@RestClientTest(ExampleRestTemplateService.class)`
  component) clearly **does** have *some* bean definition, since
  `AbstractBeanFactory.resolveBeanClass`'s native strategy (`m5_abstract_bean_factory_resolve_bean_class_with_name`
  in `native-builtins/src/spring_startup_bootstrap.rs`) gets probed for its
  name during the inner boot, and it's ultimately what the
  `NoSuchBeanDefinitionException` reports as failing to construct. This is
  the central unresolved contradiction: some registration path *other than*
  `DefaultListableBeanFactory.registerBeanDefinition` on the concrete class
  is populating (at least) `exampleWebClientApplication` and
  `exampleRestTemplateService`, while `RestTemplateAutoConfiguration`'s
  (and everyone else's) `@Bean`-method-derived definitions never appear at
  all through either path.
- **Suspicious but not conclusively resolved**: `identity_hash_code`-based
  tracking (GC-safe — raw `ObjectRef` pointers are **not** reliable here,
  CratonVM's heap is moving/GC'd and a freed address can coincidentally be
  reused for an unrelated object; an earlier pass of this same investigation
  briefly concluded "the context is reused!" purely from matching raw
  pointers across the loader boundary and had to be corrected once
  `identity_hash_code` was added instead) of the `AnnotationConfigApplicationContext`/
  `DefaultListableBeanFactory` pair returned by `getBeanFactory()`
  (`get_or_create_bean_factory`) shows **inconsistent** behavior across
  repeated runs of the identical repro: in some runs, the identity hash
  right after the inner boot's fresh `ExampleWebClientApplication` clinit is
  a genuinely new value never seen before (a fresh context, as expected —
  no bug); in other runs, it's the **same** identity hash as the context
  that had *just* finished the *outer* boot's full, successful
  ~100-bean registration sequence (`restTemplateBuilder` included) only
  moments earlier — i.e. `getBeanFactory()` calls immediately following the
  inner boot's own fresh class-loading evidence appear to land on the
  *outer* boot's already-populated bean factory, at least some of the time.
  This reads as either a genuine (if narrow) race/timing-dependent bug, or
  an artifact of `TestContextManager`/`ModifiedClassPathExtension` teardown
  of the outer context happening to interleave, single-threaded, with the
  inner context's construction in the trace window — **not disambiguated
  this session**. This is the most promising remaining lead: if confirmed
  as genuine reuse, the mechanism is almost certainly related to Spring's
  own *intentional* JVM-static `ContextCache` (test contexts are cached and
  reused across test classes by design — `@DirtiesContext` is the opt-out)
  combined with a possible CratonVM `Class`/`MergedContextConfiguration`
  identity-equality gap that fails to distinguish the two loaders' otherwise
  same-named config classes as cache keys (the exact pattern behind every
  other loader-identity bug already fixed in this codebase, e.g.
  `get_or_create_bean_factory`'s own DLBF-loader-identity comment further up
  this file, `reference_dual_registration_classloader_vs_classloader_real`,
  `reference_overlay_real_class_corruption`) — but this was **not verified**,
  only observed as a correlation.
- A scratch `application.properties` (`logging.level.org.springframework.context.annotation=TRACE`
  etc.) added to `module/spring-boot-restclient-test/src/test/resources` to
  try to get Spring's own internal `ConfigurationClassParser`/`PostProcessorRegistrationDelegate`
  logging did **not** surface any TRACE output at all for the inner boot
  (Logback/Spring Boot's `LoggingSystem` apparently does not reinitialize
  logging levels for the second `SpringApplication.run()` under a new
  classloader in this environment) — a dead end, noted here so a future
  session doesn't repeat the ~5 minutes it cost.

**Revised next steps, in order of likely value:**

1. **Settle the context/bean-factory identity question definitively.** Add
   `identity_hash_code` logging (not raw pointers) to `get_or_create_bean_factory`
   gated behind a scratch env var, and run the repro several times in a row
   (`-Parallel 1`, single class) to see whether "same hash across the loader
   boundary" reproduces consistently, or was a one-off race. If it
   reproduces reliably, the next question is WHERE the reuse happens: is it
   Spring's own `DefaultContextCache` (a real, by-design, JVM-static cache —
   check `org.springframework.test.context.cache.DefaultContextCache`'s
   equality/hash usage of `MergedContextConfiguration`, which itself hashes
   `Class[] classes` and other Class-typed fields) genuinely hitting a cache
   entry it shouldn't, versus something CratonVM-side. A cheap
   differentiator: `@DirtiesContext(classMode = AFTER_CLASS)` added
   (temporarily, scratch) to the test would force Spring to evict/not reuse
   any cached context — if the failure disappears with that annotation
   present, that's strong evidence the *real* Spring context-cache path is
   involved (whether via a genuine CratonVM Class-identity bug in the cache
   key comparison, or some other cause), not a CratonVM-only construction
   bug.
2. **If context reuse is confirmed and IS the root cause**, look at how
   `MergedContextConfiguration.equals()`/`hashCode()` compares its `Class[]
   classes` field (and `ContextCustomizer`s, which also embed Class
   references, e.g. `ImportsContextCustomizer`'s key list) — this ultimately
   bottoms out in `Class.equals()`/`hashCode()` for two same-named,
   different-loader `Class` mirrors. Search for how CratonVM represents
   `Class` identity/hashCode (`lang_class.rs`) and whether it's keyed
   purely by name anywhere reachable from this path.
3. **If context reuse is ruled out**, the mystery reverts to the original
   framing: something in `ConfigurationClassPostProcessor`'s
   `postProcessBeanDefinitionRegistry`/`ConfigurationClassBeanDefinitionReader.loadBeanDefinitions`
   flow silently produces zero bean definitions for `@Configuration`
   classes specifically under `ModifiedClassPathClassLoader`, while
   `AnnotatedBeanDefinitionReader`-style direct registrations (primary
   sources, `@RestClientTest` explicit components) still work. Since
   `ConfigurationClassPostProcessor`/`ConfigurationClassParser` are real,
   unmodifiable `spring-context` bytecode (binary Maven dependency, not
   vendored — confirmed no local source available to patch), the only path
   forward is more native-side tracing of the sort added this session
   (trace-and-delegate on the concrete registration/scan methods actually
   involved) rather than Java-side scratch instrumentation.
4. Confirm against real HotSpot that this test passes there (still not
   explicitly re-verified — expected to pass, since the test's whole point
   is asserting things work without Jackson).
5. Check `WebClientTestWithoutJacksonIntegrationTests`'s exact failure mode
   independently — still not individually confirmed in either session.

**Checked and ruled out (same session, after the above):** dev commit
`01690055c` ("fix(vm): six compounding bugs in the native
ConfigurationClassEnhancer proxy", landed on `dev` mid-session, unrelated —
found while syncing this branch) fixes CGLIB `@Configuration`-class
enhancement bugs in `native-builtins/src/cglib_enhancer.rs`, including a
loader-identity bug (bug #6 in that commit) in the same thematic space as
this investigation. Given the strong overlap, merged latest `dev` into this
branch and re-ran the repro against a rebuilt binary — **the failure is
byte-for-byte identical** (same `NoSuchBeanDefinitionException` for
`RestTemplateBuilder`, same `UnsatisfiedDependencyException` wrapping it).
That commit's fixes are exercised via `@CompileWithForkedClassLoader`
(a different loader topology, forking loaders per-test-method for
CGLIB/`ReflectUtils` infrastructure specifically) rather than
`ModifiedClassPathClassLoader`, so this null result doesn't rule out CGLIB
enhancement as *a* contributing bug family here — it only rules out *that
specific* commit's fixes as sufficient on their own. Still worth checking
`cglib_enhancer.rs` directly against the `ModifiedClassPathClassLoader`
topology (its own loader-identity resolution — bug #6's pattern, resolving
via the wrong receiver's loader — is exactly the shape of bug this whole
investigation keeps circling back to).

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
