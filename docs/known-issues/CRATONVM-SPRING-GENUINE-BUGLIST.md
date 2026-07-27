# CratonVM Spring suite — genuine bug list (dev `57c89f2de`)

| | |
|---|---|
| **Status** | OPEN — 127 non-passed of 2925 classes (47 FAIL / 74 EMPTY / 3 LOADERR / 3 TIMEOUT); but **70 of the 74 EMPTY are `Abstract*Tests` base classes the harness shouldn't be indexing at all** (0 tests is the correct, expected result for an abstract JUnit class) — the *meaningful* residual is **57 classes** (47 FAIL + 4 real EMPTY + 3 LOADERR + 3 TIMEOUT). See "2026-07-27 suite-wide reconfirmation" below for the full breakdown and named clusters. |
| **Captured** | 2026-07-27, full 8-shard suite run, dev `57c89f2de`, Azure host `20.83.144.174`, worktree `/data/data/wt-springsuite8b-20260726`. Supersedes the stale "35 confirmed genuine bugs" banner previously here (2026-07-21, dev `8719dca85` — 6 weeks and hundreds of commits out of date; the number was also never suite-wide, only tracking a hand-maintained subset). |

## 2026-07-27 suite-wide reconfirmation: current numbers, named remaining clusters

Full-suite run (all 2925 classes, `--category all`, 8-way sharded, jit-real,
real JDK 25), immediately followed by re-running just the non-passed set
after merging `origin/dev` forward from `346c74b71` to `57c89f2de`
(hundreds of intervening commits from concurrent sessions). Numbers below
are **after** the merge.

### Top-line

| status | classes |
|---|--:|
| OK | 2798 |
| FAIL | 47 |
| EMPTY | 74 (**70 are `Abstract*Tests` harness false-positives — see below**) |
| LOADERR | 3 |
| TIMEOUT | 3 |
| **Total** | 2925 |

### `EMPTY` cluster (74) — mostly not a bug, a harness discovery gap

**70 of the 74** `EMPTY` classes are literally named `Abstract*Tests`
(e.g. `aop.framework.AbstractAopProxyTests`, `oxm.AbstractMarshallerTests`,
`test.context.transaction.AbstractTransactionalSpringTests`) — real Java
`abstract` classes meant only to be extended by concrete subclasses, with
zero `@Test` methods of their own. **0 tests found is the objectively
correct result** for these under any JVM; this is `run-suite.sh discover`'s
own `find ... -name '*Tests.class'` filename heuristic picking up abstract
classes it shouldn't index as runnable standalone classes (it doesn't
check the `ACC_ABSTRACT` flag). Not a CratonVM bug. **Fix**: teach
`discover()` to skip abstract classes (check the class file's access flags,
or simpler: `javap`/reflection-check each candidate before indexing) so
these stop inflating the "non-passed" count in every future run.

The remaining **4** are genuinely worth a look — not obviously abstract,
possibly a JUnit5-feature discovery gap (interface default test methods /
generic type-parameterized test classes) rather than a VM bug, but not
confirmed either way this session:
```
test.context.async.AsyncMethodsSpringTestContextIntegrationTests
test.context.junit.jupiter.defaultmethods.GenericComicCharactersInterfaceDefaultMethodsTests
test.context.junit.jupiter.generics.GenericComicCharactersTests
web.servlet.handler.PathPatternsParameterizedTest
```

### `TIMEOUT` cluster (3) — all already tracked, no new investigation needed

```
context.aot.ApplicationContextAotGeneratorTests
core.io.buffer.DataBufferTests
test.context.aot.AotIntegrationTests
```
All three are pre-existing, extensively characterized hangs — see the AOT
bean-registration TIMEOUT cluster and Blocker 1/2 sections elsewhere in
this doc, and
[`../internal/fixed-suite-bugs/spring/CRATONVM-SPRING-TIMEOUT-CLUSTER-1500S-RERUN.md`](../internal/fixed-suite-bugs/spring/CRATONVM-SPRING-TIMEOUT-CLUSTER-1500S-RERUN.md).
Also independently reconfirmed genuine (not slow-but-finite) at a 1500s
ceiling on 2026-07-26.

### `LOADERR` cluster (3) — likely host-contention artifacts, needs an isolated rerun to confirm

```
LOADERR beans.PropertyDescriptorUtilsPropertyResolutionTests :: java.lang.OutOfMemoryError: Java heap space (anewarray component 0 length 0)
LOADERR beans.factory.aot.BeanRegistrationsAotContributionTests :: java.lang.OutOfMemoryError: Java heap space (anewarray component 0 length 0)
LOADERR web.reactive.result.method.annotation.RequestMappingMessageConversionIntegrationTests :: java.lang.NoClassDefFoundError: org/junit/platform/commons/util/ExceptionUtils
```
Two `OutOfMemoryError`s and a `NoClassDefFoundError` on a core
JUnit-Platform class that is trivially always on the classpath — the
latter in particular looks like memory/resource pressure corrupting
classloading rather than a real missing dependency. This run used
`CRATONVM_DEFAULT_HEAP_MAX_MB=2048` with 8 shards running concurrently on
a host that had 30+ other sessions' builds/tests running at the same
time (see other memory notes on this host's contention). Not re-verified
in isolation this session — do that before treating these as VM bugs.
(`BeanRegistrationsAotContributionTests` is also independently known-slow/
perf-bound per the TIMEOUT cluster doc — plausible this is that same
class simply going OOM instead of hanging, depending on host load that
run.)

### `FAIL` cluster breakdown (47) — named by common signature

**A. Suite-runner CWD/resource-path artifacts (3) — NOT CratonVM bugs.**
All fail trying to resolve a resource relative to the process's working
directory, which is `apps/spring-suite-runner/`, not the owning module's
test-resources root:
```
oxm.jaxb.Jaxb2UnmarshallerTests :: Resource does not exist: file [.../spring-suite-runner/src/test/schema/flight.xsd]
test.context.groovy.AbsolutePathGroovySpringContextTests :: Failed to load ApplicationContext (classpath:/.../context.groovy)
test.context.env.ExplicitPropertiesFileTestPropertySourceTests :: Failed to load ApplicationContext (file:src/test/resources/.../explicit.properties)
```
Harness gap (working-directory-relative resource resolution), not a VM
defect — needs the suite runner to `cd` into each module before invoking
`KRun`, or resolve resources against the module root instead of the
runner's own CWD.

**B. `synchronized` block NPE — "Cannot enter synchronized block because
`this.lock` is null" (3) — recurrence of a previously-fixed bug family.**
```
scripting.bsh.BshScriptFactoryTests
scripting.config.ScriptingDefaultsTests
scripting.groovy.GroovyScriptFactoryTests
```
Identical message across all three. This is the same symptom shape as the
`CopyOnWriteArrayList` "this.lock is null" bug fixed `68c44f62`
(`register_properties_sidetable` whole-function category-tagging gap,
documented in
[`../internal/fixed-suite-bugs/spring/CRATONVM-SPRING-TIMEOUT-CLUSTER-1500S-RERUN.md`](../internal/fixed-suite-bugs/spring/CRATONVM-SPRING-TIMEOUT-CLUSTER-1500S-RERUN.md))
-- but for a *different* class this time (scripting-related, not
`java.util.concurrent`). Worth checking whether it's the same native
category-tagging gap hitting a new class, or a new instance of the same
bug shape. Not re-investigated this session.

**C. In-memory `TestCompiler`/AOT-javac `CompilationException` (5) —
already-tracked AOT/javac cluster, still open.**
```
beans.factory.aot.BeanDefinitionPropertyValueCodeGeneratorDelegatesTests
beans.factory.aot.CodeWarningsTests
context.index.processor.CandidateComponentsIndexerTests (IllegalStateException: Compilation failed -- same family)
core.test.tools.TestCompilerTests
test.context.aot.TestContextAotGeneratorIntegrationTests
```
See the existing AOT bean-registration / in-memory-javac sections
elsewhere in this doc and in the TIMEOUT-cluster doc; no new
characterization needed.

**D. AOT bean-registration codegen `AssertionError` (4) — already-tracked
AOT cluster, still open.**
```
aot.nativex.FileNativeConfigurationWriterTests
beans.factory.annotation.AutowiredAnnotationBeanRegistrationAotContributionTests
beans.factory.aot.BeanDefinitionMethodGeneratorTests
beans.factory.aot.InstanceSupplierCodeGeneratorTests
```

**E. CGLIB enhancer synthetic `$$beanFactory` field access inside
`BeanCreationException` (2) — possibly related to "Blocker 2" elsewhere in
this doc, not confirmed.**
```
context.annotation.BeanMethodPolymorphismTests :: .../BeanMethodPolymorphismTests$Config$$SpringCGLIB$$0.$$beanFactory
context.annotation.NestedConfigurationClassTests :: .../S1ConfigWithProxy$$SpringCGLIB$$0.$$beanFactory
```
Both fail accessing a CGLIB-synthesized `$$beanFactory` field on a
`$$SpringCGLIB$$0` enhanced class. Worth cross-checking against Blocker
2's `Class.getDeclaredMethods()`/dynamically-defined-class investigation
elsewhere in this doc before assuming it's a third, unrelated CGLIB
defect.

**F. Static-resource-serving 404s (3) — needs environmental-vs-bug
triage.**
```
test.web.servlet.samples.client.context.WebAppResourceTests :: Status expected:<200 OK> but was:<404 NOT_FOUND>
test.web.servlet.samples.context.WebAppResourceTests :: Status expected:<200> but was:<404>
web.reactive.resource.ResourceWebHandlerTests :: 404 for test/foo%20with%20spaces.css
```
Could be a missing test-resource file on this classpath (environmental)
or a genuine URL-decoding/static-resource-lookup bug (the
space-in-filename case is suspicious) — not distinguished this session.

**G. JMX MBean registration residuals (2) — recheck against the
previously-CLOSED JMX cluster.**
```
jmx.export.MBeanExporterTests :: [Must have unregistered all previously registered MBeans due to RuntimeException]
transaction.annotation.AnnotationTransactionNamespaceHandlerTests :: UnableToRegisterMBeanException ... key 'testBean'
```
Memory of this project's JMX work says that cluster was closed 26/26 —
this may be a regression, or a batch-ordering artifact (a prior class in
the same 8-per-batch run leaking an MBean registration into the next
class), not necessarily a fresh bug. Rerun `--batch 1` on both before
concluding either way.

**H. Long-tail singleton residuals (~20) — not clustered, one-off
`AssertionError`/`NullPointerException`/misc failures each** across
`core.annotation.MergedAnnotationsTests`, `core.io.PathResourceTests`,
`core.io.ModuleResourceTests`, `core.io.support.*`,
`format.datetime.DateFormattingTests`,
`messaging.simp.config.MessageBrokerConfigurationTests`,
`orm.jpa.support.PersistenceInjectionTests`,
`resilience.ConcurrencyLimitTests`,
`test.context.junit.jupiter.{event,parallel}.*` (2, parallel-execution
flakiness — possibly host-load timing, not investigated),
`test.web.servlet.assertj.MockMvcTesterIntegrationTests`,
`util.SerializationUtilsTests`,
`web.context.support.StandardServletEnvironmentTests`,
`web.method.annotation.RequestHeaderMethodArgumentResolverTests` (+
its reactive twin), `web.reactive.function.client.{DefaultWebClientTests,WebClientIntegrationTests}`,
`web.servlet.config.annotation.WebMvcConfigurationSupportTests`,
`web.reactive.result.view.FragmentViewResolutionResultHandlerTests`
(NPE inside AssertJ's own `Objects.assertEqual` — likely the same
AssertJ-native-shim-null-field family as the now-fixed
`spring-kotlin-reflect-illegalstateexception-root-FIXED.md`'s second bug —
worth a quick cross-check),
`test.context.bean.override.mockito.MockitoBeanByTypeLookupForConstructorParametersIntegrationKotlinTests`
(`[is a Mockito mock]` — unrelated to the now-fixed Kotlin-reflect
cluster, confirmed by cross-check when that doc was closed).

None of H were individually root-caused this session — grouped here so
the next person doesn't have to re-run the full suite just to get this
list again.
# CratonVM Spring suite — genuine bug list (dev `57c89f2de`)

| | |
|---|---|
| **Status** | OPEN — **17 residual classes**, down from the 57 captured on 2026-07-27 (see the "second session" entry below for the ten VM fixes and two harness fixes that closed the other 40, and for the per-class state of what is left). The older "127 non-passed of 2925" figure is superseded: 73 of those 74 `EMPTY` classes were `Abstract*Tests`/annotation-interface entries the runner should never have indexed, and the runner no longer does. |
| **Captured** | 2026-07-27 (second session), branch `fix/spring-buglist-close-20260727` merged forward to `origin/dev` `ffc7f90d4`, Azure host `20.83.144.174`, real JDK 25. The baseline it improves on is the 2026-07-27 full 8-shard run recorded immediately below. **The shared host ran at load 70–100 throughout, so batch runs emit spurious FAIL/TIMEOUT rows — re-check any residual in isolation before believing it.** |

## 2026-07-27 suite-wide reconfirmation: current numbers, named remaining clusters

Full-suite run (all 2925 classes, `--category all`, 8-way sharded, jit-real,
real JDK 25), immediately followed by re-running just the non-passed set
after merging `origin/dev` forward from `346c74b71` to `57c89f2de`
(hundreds of intervening commits from concurrent sessions). Numbers below
are **after** the merge.

### Top-line

| status | classes |
|---|--:|
| OK | 2798 |
| FAIL | 47 |
| EMPTY | 74 (**70 are `Abstract*Tests` harness false-positives — see below**) |
| LOADERR | 3 |
| TIMEOUT | 3 |
| **Total** | 2925 |

### `EMPTY` cluster (74) — mostly not a bug, a harness discovery gap

**70 of the 74** `EMPTY` classes are literally named `Abstract*Tests`
(e.g. `aop.framework.AbstractAopProxyTests`, `oxm.AbstractMarshallerTests`,
`test.context.transaction.AbstractTransactionalSpringTests`) — real Java
`abstract` classes meant only to be extended by concrete subclasses, with
zero `@Test` methods of their own. **0 tests found is the objectively
correct result** for these under any JVM; this is `run-suite.sh discover`'s
own `find ... -name '*Tests.class'` filename heuristic picking up abstract
classes it shouldn't index as runnable standalone classes (it doesn't
check the `ACC_ABSTRACT` flag). Not a CratonVM bug. **Fix**: teach
`discover()` to skip abstract classes (check the class file's access flags,
or simpler: `javap`/reflection-check each candidate before indexing) so
these stop inflating the "non-passed" count in every future run.

The remaining **4** are genuinely worth a look — not obviously abstract,
possibly a JUnit5-feature discovery gap (interface default test methods /
generic type-parameterized test classes) rather than a VM bug, but not
confirmed either way this session:
```
test.context.async.AsyncMethodsSpringTestContextIntegrationTests
test.context.junit.jupiter.defaultmethods.GenericComicCharactersInterfaceDefaultMethodsTests
test.context.junit.jupiter.generics.GenericComicCharactersTests
web.servlet.handler.PathPatternsParameterizedTest
```

### `TIMEOUT` cluster (3) — all already tracked, no new investigation needed

```
context.aot.ApplicationContextAotGeneratorTests
core.io.buffer.DataBufferTests
test.context.aot.AotIntegrationTests
```
All three are pre-existing, extensively characterized hangs — see the AOT
bean-registration TIMEOUT cluster and Blocker 1/2 sections elsewhere in
this doc, and
[`../internal/fixed-suite-bugs/spring/CRATONVM-SPRING-TIMEOUT-CLUSTER-1500S-RERUN.md`](../internal/fixed-suite-bugs/spring/CRATONVM-SPRING-TIMEOUT-CLUSTER-1500S-RERUN.md).
Also independently reconfirmed genuine (not slow-but-finite) at a 1500s
ceiling on 2026-07-26.

### `LOADERR` cluster (3) — likely host-contention artifacts, needs an isolated rerun to confirm

```
LOADERR beans.PropertyDescriptorUtilsPropertyResolutionTests :: java.lang.OutOfMemoryError: Java heap space (anewarray component 0 length 0)
LOADERR beans.factory.aot.BeanRegistrationsAotContributionTests :: java.lang.OutOfMemoryError: Java heap space (anewarray component 0 length 0)
LOADERR web.reactive.result.method.annotation.RequestMappingMessageConversionIntegrationTests :: java.lang.NoClassDefFoundError: org/junit/platform/commons/util/ExceptionUtils
```
Two `OutOfMemoryError`s and a `NoClassDefFoundError` on a core
JUnit-Platform class that is trivially always on the classpath — the
latter in particular looks like memory/resource pressure corrupting
classloading rather than a real missing dependency. This run used
`CRATONVM_DEFAULT_HEAP_MAX_MB=2048` with 8 shards running concurrently on
a host that had 30+ other sessions' builds/tests running at the same
time (see other memory notes on this host's contention). Not re-verified
in isolation this session — do that before treating these as VM bugs.
(`BeanRegistrationsAotContributionTests` is also independently known-slow/
perf-bound per the TIMEOUT cluster doc — plausible this is that same
class simply going OOM instead of hanging, depending on host load that
run.)

### `FAIL` cluster breakdown (47) — named by common signature

**A. Suite-runner CWD/resource-path artifacts (3) — NOT CratonVM bugs.**
All fail trying to resolve a resource relative to the process's working
directory, which is `apps/spring-suite-runner/`, not the owning module's
test-resources root:
```
oxm.jaxb.Jaxb2UnmarshallerTests :: Resource does not exist: file [.../spring-suite-runner/src/test/schema/flight.xsd]
test.context.groovy.AbsolutePathGroovySpringContextTests :: Failed to load ApplicationContext (classpath:/.../context.groovy)
test.context.env.ExplicitPropertiesFileTestPropertySourceTests :: Failed to load ApplicationContext (file:src/test/resources/.../explicit.properties)
```
Harness gap (working-directory-relative resource resolution), not a VM
defect — needs the suite runner to `cd` into each module before invoking
`KRun`, or resolve resources against the module root instead of the
runner's own CWD.

**B. `synchronized` block NPE — "Cannot enter synchronized block because
`this.lock` is null" (3) — recurrence of a previously-fixed bug family.**
```
scripting.bsh.BshScriptFactoryTests
scripting.config.ScriptingDefaultsTests
scripting.groovy.GroovyScriptFactoryTests
```
Identical message across all three. This is the same symptom shape as the
`CopyOnWriteArrayList` "this.lock is null" bug fixed `68c44f62`
(`register_properties_sidetable` whole-function category-tagging gap,
documented in
[`../internal/fixed-suite-bugs/spring/CRATONVM-SPRING-TIMEOUT-CLUSTER-1500S-RERUN.md`](../internal/fixed-suite-bugs/spring/CRATONVM-SPRING-TIMEOUT-CLUSTER-1500S-RERUN.md))
-- but for a *different* class this time (scripting-related, not
`java.util.concurrent`). Worth checking whether it's the same native
category-tagging gap hitting a new class, or a new instance of the same
bug shape. Not re-investigated this session.

**C. In-memory `TestCompiler`/AOT-javac `CompilationException` (5) —
already-tracked AOT/javac cluster, still open.**
```
beans.factory.aot.BeanDefinitionPropertyValueCodeGeneratorDelegatesTests
beans.factory.aot.CodeWarningsTests
context.index.processor.CandidateComponentsIndexerTests (IllegalStateException: Compilation failed -- same family)
core.test.tools.TestCompilerTests
test.context.aot.TestContextAotGeneratorIntegrationTests
```
See the existing AOT bean-registration / in-memory-javac sections
elsewhere in this doc and in the TIMEOUT-cluster doc; no new
characterization needed.

**D. AOT bean-registration codegen `AssertionError` (4) — already-tracked
AOT cluster, still open.**
```
aot.nativex.FileNativeConfigurationWriterTests
beans.factory.annotation.AutowiredAnnotationBeanRegistrationAotContributionTests
beans.factory.aot.BeanDefinitionMethodGeneratorTests
beans.factory.aot.InstanceSupplierCodeGeneratorTests
```

**E. CGLIB enhancer synthetic `$$beanFactory` field access inside
`BeanCreationException` (2) — possibly related to "Blocker 2" elsewhere in
this doc, not confirmed.**
```
context.annotation.BeanMethodPolymorphismTests :: .../BeanMethodPolymorphismTests$Config$$SpringCGLIB$$0.$$beanFactory
context.annotation.NestedConfigurationClassTests :: .../S1ConfigWithProxy$$SpringCGLIB$$0.$$beanFactory
```
Both fail accessing a CGLIB-synthesized `$$beanFactory` field on a
`$$SpringCGLIB$$0` enhanced class. Worth cross-checking against Blocker
2's `Class.getDeclaredMethods()`/dynamically-defined-class investigation
elsewhere in this doc before assuming it's a third, unrelated CGLIB
defect.

**F. Static-resource-serving 404s (3) — needs environmental-vs-bug
triage.**
```
test.web.servlet.samples.client.context.WebAppResourceTests :: Status expected:<200 OK> but was:<404 NOT_FOUND>
test.web.servlet.samples.context.WebAppResourceTests :: Status expected:<200> but was:<404>
web.reactive.resource.ResourceWebHandlerTests :: 404 for test/foo%20with%20spaces.css
```
Could be a missing test-resource file on this classpath (environmental)
or a genuine URL-decoding/static-resource-lookup bug (the
space-in-filename case is suspicious) — not distinguished this session.

**G. JMX MBean registration residuals (2) — recheck against the
previously-CLOSED JMX cluster.**
```
jmx.export.MBeanExporterTests :: [Must have unregistered all previously registered MBeans due to RuntimeException]
transaction.annotation.AnnotationTransactionNamespaceHandlerTests :: UnableToRegisterMBeanException ... key 'testBean'
```
Memory of this project's JMX work says that cluster was closed 26/26 —
this may be a regression, or a batch-ordering artifact (a prior class in
the same 8-per-batch run leaking an MBean registration into the next
class), not necessarily a fresh bug. Rerun `--batch 1` on both before
concluding either way.

**H. Long-tail singleton residuals (~20) — not clustered, one-off
`AssertionError`/`NullPointerException`/misc failures each** across
`core.annotation.MergedAnnotationsTests`, `core.io.PathResourceTests`,
`core.io.ModuleResourceTests`, `core.io.support.*`,
`format.datetime.DateFormattingTests`,
`messaging.simp.config.MessageBrokerConfigurationTests`,
`orm.jpa.support.PersistenceInjectionTests`,
`resilience.ConcurrencyLimitTests`,
`test.context.junit.jupiter.{event,parallel}.*` (2, parallel-execution
flakiness — possibly host-load timing, not investigated),
`test.web.servlet.assertj.MockMvcTesterIntegrationTests`,
`util.SerializationUtilsTests`,
`web.context.support.StandardServletEnvironmentTests`,
`web.method.annotation.RequestHeaderMethodArgumentResolverTests` (+
its reactive twin), `web.reactive.function.client.{DefaultWebClientTests,WebClientIntegrationTests}`,
`web.servlet.config.annotation.WebMvcConfigurationSupportTests`,
`web.reactive.result.view.FragmentViewResolutionResultHandlerTests`
(NPE inside AssertJ's own `Objects.assertEqual` — likely the same
AssertJ-native-shim-null-field family as the now-fixed
`spring-kotlin-reflect-illegalstateexception-root-FIXED.md`'s second bug —
worth a quick cross-check),
`test.context.bean.override.mockito.MockitoBeanByTypeLookupForConstructorParametersIntegrationKotlinTests`
(`[is a Mockito mock]` — unrelated to the now-fixed Kotlin-reflect
cluster, confirmed by cross-check when that doc was closed).

None of H were individually root-caused this session — grouped here so
the next person doesn't have to re-run the full suite just to get this
list again.


## 2026-07-27 (second session) — ten VM bugs + two harness gaps; 57 residual classes → 17

Worktree `/data/data/wt-sprbuglist-20260727` (branch
`fix/spring-buglist-close-20260727`, from `origin/dev` `60a710ad8`, merged
forward to `ffc7f90d4`), Azure host `20.83.144.174`, real JDK 25, binaries
`localbin/cratonvm-sprbuglist-v*.bin`. No subagents, per this task's standing
instruction.

Method: re-ran the exact 57 "meaningful residual" classes the section below
names, fixed what the failures actually pointed at, re-ran. Every fix has a
standalone HotSpot-vs-CratonVM probe under
`/data/data/wt-sprbuglist-20260727/probes/`.

**Two things to know before reading the older sections below**: the
`EMPTY` cluster and the "static-resource 404" cluster were harness artifacts,
not VM bugs, and the whole `CompilationException` cluster was one collection
bug. Those sections are kept for history but their conclusions are superseded.

### Fix 1 — `LinkedHashSet.remove(Object)` returned `false` while removing

`native_linkedhashset_remove` (`native-builtins/src/properties_sidetable.rs`),
an override that only wants `Properties.keySet()` snapshots, sent every
*ordinary* `LinkedHashSet` to real bytecode via
`invoke_virtual_bytecode_only`. Real `HashSet.remove` is
`return map.remove(o) == PRESENT;`, and this VM's synthetic backing map stores
an `Int(1)` sentinel rather than JDK `HashSet.PRESENT` — so the identity
comparison was always false. `HashSet` itself was fine; only `LinkedHashSet`
carried that extra override.

**This produced the entire section-C `CompilationException: Unable to compile
source` cluster.** javac's
`com.sun.tools.javac.comp.Annotate.attributeAnnotation` collects an annotation
type's elements into a `LinkedHashSet` and logs
`duplicate element 'value' in annotation @X` when `members.remove(method)`
returns false — so the **in-process** compiler
(`ToolProvider.getSystemJavaCompiler()`, which Spring's AOT `TestCompiler`
uses) rejected *every* source containing an annotation with a `value` element:
`@SuppressWarnings("...")`, `@Retention`, any custom `@interface`.
`probes/JavacProbe.java` reproduces it in ~10s and now matches HotSpot on all
six cases.

Fixed by `try_native_hashset_remove` (new `pub fn` in `native-collections`),
called from all three bytecode-only fallbacks.

### Fix 2 — real-JDK `ReferenceQueue.lock` was null

`NullPointerException: Cannot enter synchronized block because "this.lock" is
null` out of `java.lang.ref.ReferenceQueue.enqueue`. The synthetic
`ReferenceQueue.<init>()V` native writes only the two-slot (head, size) shape;
a real JDK 25 `ReferenceQueue` also declares `private final Lock lock` that its
own `enqueue`/`poll`/`remove` bytecode synchronizes on. Fixed by dropping
**only the constructor** under `drop_real_layout_synthetic` so the real one
runs. `poll`/`remove` deliberately stay native: the GC reference processor
enqueues by writing the head slot directly and never notifies that lock, so
real blocking `remove()` bytecode would wait forever. Slot 1 is `size` (int)
synthetically but `queueLength` (long) on the real class, so all three writers
now preserve the stored value's width.

Reached via `Reference.enqueue()` → `ConcurrentReferenceHashMap` purge →
`AbstractApplicationContext.resetCommonCaches()`, so it broke every *cancelled*
context refresh — section B's `scripting.{bsh,config,groovy}.*` cluster.

### Fix 3 — `equals(null)` was short-circuited instead of dispatched

Two natives implemented "one side is null ⇒ not equal", but the contracts they
emulate both end in a virtual `equals(null)`:

- `java.util.Objects.equals(a, b)` is `(a == b) || (a != null && a.equals(b))`.
- AssertJ's `StandardComparisonStrategy.areEqual` (natively shimmed in
  `native-builtins/src/test_frameworks.rs`) only short-circuits on a null
  *actual*; every array branch is guarded by `other != null` and the method
  ends in `return actual.equals(other)`.

Spring's `NullBean` — the placeholder a `@Bean` method that returned null is
registered as — is defined as `equals(obj) { return (this == obj || obj ==
null); }` precisely so `assertThat(getBean(name)).isEqualTo(null)` passes.
Under the old shims that assertion failed with the self-contradictory message
`expected: null but was: null`, the signature seen in three unrelated classes.

### Fix 4 — the native config enhancer and real CGLIB minted the same class name

`cce_enhance` generates `<Config>$$SpringCGLIB$$0` itself, bypassing CGLIB.
Real CGLIB guarantees uniqueness through
`AbstractClassGenerator$ClassLoaderData.reservedClassNames`; a natively
generated class is invisible to that set, so the next real-CGLIB generation for
the same prefix in the same loader picked `$$0` as well. `CglibAopProxy` is
exactly that case — `ClassUtils.getUserClass` strips the `$$SpringCGLIB$$`
suffix, so AOP-proxying an enhanced `@Configuration` bean uses the RAW config
class as its prefix. The second definition took over the name, and every
symbolic field/method ref naming that class then resolved to the AOP proxy:
`NoSuchFieldError: ...$Config$$SpringCGLIB$$0.$$beanFactory`.

Both victims passed in isolation and failed 100% deterministically once a test
that AOP-proxies a `@Configuration` bean ran first. Fixed by wrapping
`SpringNamingPolicy.getClassName`: delegate to the real bytecode for the
candidate, then advance the trailing counter past any name already defined in
this VM.

**This is the same bug shape as the long-running "CGLIB cross-test residual"
family in the sections below** (`processAheadOfTimeUsesCglibClassForFactoryMethod`
/ `...WhenHasCglibProxyUseProxy`: pass in isolation, fail deterministically in
the full class run) — those were closed by an earlier session from the
`config_enhancer_class_cache` side; this closes the naming side.

### Fix 5 — `java.util.Date(String)` threw `UnsupportedOperationException`

Real JDK defines it as `this(parse(s))`, and `Date.parse(String)` has no native
override here — it already runs as real bytecode and returns the right value.
Spring reaches this constructor through `ObjectToObjectConverter`, so
`@RequestHeader java.util.Date` could not bind an ordinary RFC-1123 header.

### Fix 6 — `InitialContext.getEnvironment()` ignored an installed factory builder

Real JDK: `getDefaultInitCtx().getEnvironment()`. The native went straight to
the `java.naming.factory.initial` system property and threw
`NoInitialContextException` when unset — the same omission the
lookup/bind/rebind/unbind natives were already fixed for. Spring probes JNDI
availability with exactly `new InitialContext().getEnvironment()` in a
try/catch, so `StandardServletEnvironment` silently omitted its
`jndiProperties` source.

### Fix 7 — `new File(URI)` used the raw path, so percent-escapes survived

Real `File(URI)` is `String p = uri.getPath();`, and `getPath()` returns the
DECODED path — the `path` FIELD the native read holds the raw one (what
`getRawPath()` returns). A file genuinely named `resource#test1.txt` therefore
came back as `resource%23test1.txt`, and `exists()` was false for any path
containing a character `File.toURI()` had escaped. `URI` itself was already
correct on both VMs. `probes/HashProbe.java`.

### Fix 8 — `InitialContext.getEnvironment()` (see Fix 6 above)

### Fixes 9 and 10 — charset encode/decode ignored direct buffers and the array offset

Both found by `core.io.buffer.DataBufferTests` (294 tests) timing out; the
watchdog caught the main thread parked at `DirectByteBuffer.<init>` pc=0 in
every dump, inside `DataBuffer.write(CharSequence, Charset)`.

- A real-JDK **direct** `ByteBuffer` has no backing array, so `charset.rs`'s
  `buf_state` returned `None` and `CharsetEncoder.encode` answered `OVERFLOW`
  having written nothing and consumed nothing. That is an infinite loop for any
  caller that grows its buffer and retries on OVERFLOW — which is what the
  `encode` contract invites, and what Spring does.
  `NettyDataBuffer.asByteBuffer()` is direct.
- `buf_state` also ignored the buffer's **`offset`** field, so every encode and
  decode read and wrote at the backing array's absolute start rather than the
  buffer's own window. Non-zero `offset` is not exotic: `wrap(array, off, len)`,
  `slice()`, `duplicate()` of a positioned buffer, and every pooled Netty
  `ByteBuf` (a window onto a shared arena) have one — which is why the residual
  44 failures after the first fix were all on the
  `PooledByteBufAllocator - preferDirect = false` parameterisation.

`core.io.buffer.DataBufferTests` TIMEOUT → **294/294**. `probes/EncProbe.java`
and `probes/OffProbe.java` both match HotSpot now.

### Two residuals are NOT CratonVM bugs — check HotSpot in THIS checkout first

`apps/spring-suite-runner/hs.sh <fqcn>` runs one class on HotSpot from its own
module directory, i.e. exactly the way `one.sh` runs it on CratonVM. Doing that
for the whole residual set found two that fail identically on HotSpot:

- **`aot.nativex.FileNativeConfigurationWriterTests` (2/7)** — the writer emits
  `"comment": "Spring Framework 7.1.0-SNAPSHOT"`, and the expected JSON is
  compared `NON_EXTENSIBLE`. Both VMs emit it: the test only passes when
  `SpringVersion.getVersion()` returns null, which needs a classpath without
  the packaged jar. Fixture artifact.
- **`context.annotation.ConfigurationClassEnhancerTests.withPublicClass`** —
  fails on HotSpot too, so only `enhanceReloadedClass` is a genuine CratonVM
  failure in that class.

Everything else in the table below was confirmed green on HotSpot in this same
checkout.

### Harness gap 0 — the last `EMPTY` class was `@Disabled`

`test.context.async.AsyncMethodsSpringTestContextIntegrationTests` carries
`@Disabled("Only meant to be executed manually")`; **HotSpot reports `found=0`
for it too.** It was never a `@RepeatedTest` discovery gap — a standalone
`@RepeatedTest(3)` probe discovers and runs correctly on both VMs.
`is_concrete.py` now also parses the class-level `RuntimeVisibleAnnotations`
and drops `@Disabled` classes, which takes the index to 2847 and **empties the
`EMPTY` cluster entirely**.

### Harness gap 1 — `discover()` indexed classes that cannot be run

`run-suite.sh discover` finds candidates by *filename* (`*Tests.class`), which
also matches `Abstract*Tests` base classes and JUnit meta-annotation interfaces
such as `@PathPatternsParameterizedTest`. Zero tests is the correct result for
those on any JVM. **That accounted for 73 of the 74 `EMPTY` classes**, so the
"EMPTY cluster" section below is closed. `apps/spring-suite-runner/
is_concrete.py` now checks `ACC_ABSTRACT`/`ACC_INTERFACE` in the class file and
drops them: 2925 → 2852 indexed. Only
`test.context.async.AsyncMethodsSpringTestContextIntegrationTests` is a
genuinely concrete class that still reports 0 tests (it uses
`@RepeatedTest(200)`) — a real discovery gap, still open.

### Harness gap 2 — the runner ran every class from its own directory

Section A's three "CWD/resource-path artifacts" were the tip of it. Running
each class from its owning module's directory (what Gradle does) closed six
classes outright: `oxm.jaxb.Jaxb2UnmarshallerTests` (0/12 → 12/12),
`core.io.PathResourceTests` (27/38 → 38/38),
`test.context.env.ExplicitPropertiesFileTestPropertySourceTests` (6/13 →
13/13), `test.context.groovy.AbsolutePathGroovySpringContextTests`, and both
`test.web.servlet.samples.*.WebAppResourceTests` — **so section F's
static-resource 404s were not a URL-decoding bug, they were the harness.**
`core.io.support.PathMatchingResourcePatternResolverTests` improved 15/22 →
19/22.

### Regression landed by `origin/dev` between `60a710ad8` and `ffc7f90d4`

`beans.factory.annotation.AutowiredAnnotationBeanRegistrationAotContributionTests`
went **14/14 → 1/14**, and it is not from this session's work: bisected across
this session's own binaries, `v7` (all six VM fixes, pre-merge) is 14/14 and
`v8` (`v7` + the `origin/dev` merge, nothing else) is 1/14. Thirteen methods now
fail `assertThat(contribution).isNotNull()` at
`getAndApplyContribution(...):278`, i.e.
`AutowiredAnnotationBeanPostProcessor.processAheadOfTime(registeredBean)`
returns null — no autowired members detected. Runtime also drops from ~170s to
~7s, so it bails early.

Ruled out: reflection metadata. A probe
(`probes/AutowiredProbe.java`) printing `isSynthetic`/`isBridge`/
`getAnnotation(Autowired.class)` for private and package-private fields and
methods of a nested class returns **identical, correct** answers on `v7` and
`v11`.

The 61-commit delta's most plausible suspects, by subject:
`b3b999b78 fix(interpreter): remove package-selected dispatch` (the failing
methods are exactly the private / package-private injection ones),
`2e2fea0fc fix(natives): Class.isSynthetic reads the access flag`,
`a50ce9348 fix(classloading): eliminate loader-blind VM lookups`.

Repro:
```
apps/spring-suite-runner/one.sh \
  org.springframework.beans.factory.annotation.AutowiredAnnotationBeanRegistrationAotContributionTests
```

### What is left (21 classes)

Verified in isolation against the merged binary unless noted. The shared host
ran at load 70–100 for much of this session, so batch runs produce spurious
FAIL/TIMEOUT rows — **always re-check a residual in isolation before believing
it**.

| class | state | note |
|---|---|---|
| `beans.PropertyDescriptorUtilsPropertyResolutionTests` | LOADERR | `OutOfMemoryError` after ~90s during discovery of a JUnit `@ParameterizedClass` + `@FieldSource` class. Reproduces in isolation at 2GB **and** 8GB heap, so it is a real allocation blow-up, not the host contention the section below guessed at |
| `context.annotation.ConfigurationClassEnhancerTests` | 3/5 (1 genuine) | `withPublicClass` fails on HotSpot too. For `enhanceReloadedClass`, `cce_enhance` ignores the `classLoader` argument and defines into the config class's own loader. Real Spring/CGLIB picks the defining loader per `ReflectUtils.defineClass`'s contextClass/SmartClassLoader rules, which the two failing methods assert case by case |
| `core.annotation.MergedAnnotationsTests` | 177/178 | `equalsForSynthesizedAnnotations` — a synthesized annotation and a real one are not `equals()`; their `toString()`s show one is a real JDK annotation proxy and the other CratonVM's synthetic |
| `core.io.ModuleResourceTests` | 2/3 | Root-caused: the failing assertion is on the **ClassPathResource**, not the ModuleResource — `isReadable()` is false for `jrt:/java.desktop/java/beans/Introspector.class`. `URL.openConnection()` hands that URL a `sun.net.www.protocol.http.HttpURLConnection` (content length -1), so Spring's `AbstractFileResolvingResource.isReadable` takes its `instanceof HttpURLConnection` branch and sends a HEAD. `openStream()` on the same URL returns the right 23755 bytes, so only the protocol-handler choice is wrong. `probes/JrtProbe.java`. `Module.getResourceAsStream` is fine (`probes/ModProbe.java`) |
| `core.io.support.PathMatchingResourcePatternResolverTests` | 19/22 | HotSpot is 22/22 in the same checkout, so all three are genuine. `encodedHashtagInPath` is root-caused: `URLClassLoader.getResource`/`getResources` return the resource URL with the base URL's percent-escapes **decoded** (`file:/…/custom#root/scanned/` where HotSpot keeps `custom%23root`), and a raw `#` in a URL is a fragment delimiter, so everything after it is lost downstream. `probes/UclProbe.java` is a 5-line repro. `classloader.rs::file_url_spec` already encodes correctly but is only used by `getURLs()`/manifest entries — the `getResource` return path builds its URL somewhere else. The two `javaDashJarFinds*ClassPathManifestEntries` are separate (`NoSuchElementException: No value present`) |
| `orm.jpa.support.PersistenceInjectionTests` | 26/27 | unchanged from the older section below |
| `scripting.groovy.GroovyScriptFactoryTests` | 27/38 | `NoClassDefFoundError` for classes GroovyClassLoader compiles from `.groovy` sources (`GroovyCalculator`, `TestFactoryBean`, `GroovyMessenger2`, `TestCustomizer`) |
| `test.context.junit.jupiter.event.ParallelApplicationEventsIntegrationTests` | 0/2 | JUnit parallel execution × `ApplicationEvents` |
| `test.web.servlet.assertj.MockMvcTesterIntegrationTests` | 72/74 | NOT an output-capture problem: `System.setOut` capture and `PrintWriter.printf("%18s = %s%n", "Type", null)` both behave exactly as on HotSpot (`probes/SysOutProbe.java`, `probes/PrintfProbe.java`), and `debug(customStream)` fails the same way. `PrintingResultHandler.handle` prints the `MockHttpServletRequest`, `Handler` and `Async` sections correctly, then emits the `Resolved Exception:` heading and stops — no `Type = ...` line, and none of the later `ModelAndView` / `FlashMap` / `MockHttpServletResponse` sections. So it dies inside `printResolvedException`, and whatever it throws is being swallowed (the assertion under test still reports `hasStatusOk`). Next step: instrument `result.getResolvedException()` |
| `util.SerializationUtilsTests` | 8/9 | Narrowed: real `ObjectInputStream` bytecode runs (no native registered for `resolveClass`), and its `Class.forName(name, false, latestUserDefinedLoader())` **returns a fabricated stub class** for a name that does not exist — so `initNonProxy` records a `deserializeEx` and `checkDeserialize` throws `InvalidClassException` instead of the JDK's `ClassNotFoundException`. The identical `Class.forName` call from ordinary user code, with the identical loader, correctly throws (`probes/OisProbe5.java`, `probes/LoaderProbe.java`), so the stub fabrication is caller/dispatch dependent — the `ProbeGuard` in `native_class_for_name` is evidently not in force on whichever path the JDK-internal call takes |
| `web.reactive.function.client.DefaultWebClientTests` | 24/25 | |
| `web.reactive.function.client.WebClientIntegrationTests` | 168/170 | |
| `web.reactive.result.view.FragmentViewResolutionResultHandlerTests` | 5/6 | reactor `Timeout on blocking read` in the SSE path |
| `beans.factory.aot.BeanRegistrationsAotContributionTests` | TIMEOUT | the separately tracked ~227×-vs-HotSpot interpreter throughput defect |
| `context.aot.ApplicationContextAotGeneratorTests` | TIMEOUT | needs re-measuring on a quiet host — it was 40/40 as of follow-up 10 |
| `test.context.aot.AotIntegrationTests` | TIMEOUT | |
| `test.context.aot.TestContextAotGeneratorIntegrationTests` | TIMEOUT | follow-up 9c's `Spliterators.spliterator` hang |
| `web.reactive.result.method.annotation.RequestMappingMessageConversionIntegrationTests` | TIMEOUT | |

Also closed on the way, without a dedicated fix (they were downstream of the
six above): the whole section-D AOT bean-registration `AssertionError` cluster,
section G's two JMX residuals, `jndi.JndiObjectFactoryBeanTests` (24/25 → 25/25,
including the `lookupWithExposeAccessContext` extra-`close()` the section below
left open), `core.io.ResourceTests`, `format.datetime.DateFormattingTests`,
`resilience.ConcurrencyLimitTests`, `core.io.support.SpringFactoriesLoaderTests`,
`core.test.tools.{SourceFileTests,TestCompilerTests}`,
`context.index.processor.CandidateComponentsIndexerTests`,
`test.context.web.WebAppConfigurationBootstrapWithTests`, and
`web.reactive.resource.ResourceWebHandlerTests`.

## 2026-07-21 late session — non-AOT residual sweep

Scope: every OPEN class in this doc EXCLUDING the AOT cluster (both the
strict `*.aot.*`-package classes and the wider set of TIMEOUT classes swept
into that investigation's narrative — `core.io.buffer.DataBufferTests`,
`scripting.groovy.GroovyScriptFactoryTests`,
`context.groovy.GroovyBeanDefinitionReaderTests`,
`context.annotation.ComponentScanParserBeanDefinitionDefaultsTests`,
`test.context.junit.jupiter.parallel.ParallelExecutionSpringExtensionTests`,
`web.reactive.result.method.annotation.CrossOriginAnnotationIntegrationTests`/
`RequestMappingMessageConversionIntegrationTests`, and
`web.service.registry.*` — left alone per explicit instruction, another
session (`wt-aot-cluster-20260721`, branch `fix/aot-cluster-residuals-20260721`)
is actively working that cluster). Worktree
`/data/wt-spring-genuine-residuals-20260721` (branch
`fix/spring-genuine-buglist-residuals-20260721`), binary
`cratonvm-springresid-v2.bin`.

**Root-caused but NOT fixed this session** (each would need substantial new
native-VM feature work or live/gdb tracing beyond this session's time
budget — flagged for a dedicated follow-up):

- **`context.annotation.ConfigurationClassPostProcessorTests`** (11/85
  fail, unchanged from this session's own pre-fix baseline — note this is
  DOWN from the doc's previously-recorded 82/85/3-fail state, i.e. 8 MORE
  failures appeared here between 2026-07-21's earlier session and this one,
  from unrelated concurrent dev work landing on `dev` in between; not
  investigated). Two distinct root causes found for 5/11:
  - 1 failure (`configurationClassesWithInvalidOverridingForProgrammaticCall`)
    — `emit_bean_override`'s inter-bean-reference path does a raw JVM
    `checkcast <Ret>` after `getBean()`, throwing a bare
    `ClassCastException` on type mismatch instead of replicating real
    Spring's `resolveBeanReference` `ClassUtils.isAssignableValue` check +
    descriptive `IllegalStateException` (`"@Bean method X.y called as bean
    reference for type [...] but overridden by non-compatible bean
    instance of type [...]. Overriding bean of same name declared in:
    ..."`). Full replacement bytecode designed in detail (instanceof+null
    check inside the existing SPR-8080 try-region, `StringBuilder` message
    build using a Rust-precomputed static prefix + `Class.getName()`/
    `Object.getClass()` reflective calls for the dynamic parts, throw
    `IllegalStateException`) but not implemented — mechanical, ~90 more
    bytes, all new constant-pool entries are straightforward reuses of
    patterns already in this file. This ALSO throws a customer-visible raw
    `ClassCastException` instead of Spring's real message ANYWHERE an
    inter-bean `@Bean` reference resolves to an incompatible override
    anywhere else in the suite — likely affects more than just this one
    test, worth fixing first in a follow-up.
  - Remaining 6/11 failures not investigated at all this session.
- **`jndi.JndiObjectFactoryBeanTests`.`lookupWithExposeAccessContext`**
  (24/25). Confirmed the exact expected math from real
  `JndiObjectFactoryBean`/`JndiObjectTargetSource`/
  `JndiContextExposingInterceptor` source: 1 `Context.close()` from
  `JndiObjectTargetSource.afterPropertiesSet()`'s eager `lookup()`, + 1 from
  the single ELIGIBLE proxied invocation (`setAge`, interface-declared).
  `equals()`/`hashCode()` should be short-circuited by `JdkDynamicAopProxy`
  before ever reaching the interceptor; `toString()` reaches it but
  `isEligible()` should return `false` since its `Method.getDeclaringClass()
  == Object.class`. CratonVM produces 3 closes (1 extra) — needs live/gdb
  tracing of the native `java.lang.reflect.Proxy` invocation-handler
  dispatch to find which of the three incorrectly gets routed through with
  a non-`Object` declaring class (or isn't fast-path short-circuited);
  static grep of `native-builtins` found no obvious culprit.
- **`orm.jpa.support.PersistenceInjectionTests`.
  `publicExtendedPersistenceContextSetterWithSerialization`** (26/27).
  `DummyInvocationHandler.closed` stays `false` after a `SimpleMapScope`
  Java-serialization round-trip + `serialized.close()`. Involves a
  scope-destruction-callback object (likely wrapping the
  `ExtendedEntityManagerCreator`-generated `EntityManager` proxy) needing
  to survive Java serialization and still correctly invoke `close()` post-
  deserialization — deep cross-cutting serialization+scope+JPA-proxy
  interaction, not traced to a specific native gap.
- **`test.context.junit.jupiter.event.ParallelApplicationEventsIntegrationTests`**
  (0/2) — `executeTestsInParallelWithInstancePerMethod` fails an AssertJ
  `MultipleFailuresError` ("Test Event Statistics", 2 failures);
  `rejectTestsInParallelWithInstancePerClassAndRecordApplicationEvents`
  fails a plain `AssertionError`. JUnit parallel-execution × Spring
  TestContext `ApplicationEvents` recording interaction, not investigated.
- **`test.web.servlet.assertj.MockMvcTesterIntegrationTests`** (72/74) —
  `debugUsesSystemOutByDefault`/`debugCanPrintToCustomOutputStream` both
  fail plain `AssertionError`s (`MockMvcTester`'s `.debug()`/`.print()`
  output-stream-capture assertions). Not investigated.
- **`web.servlet.config.MvcNamespaceTests`.`customConversionService`**
  (24/25) and **`web.servlet.config.annotation.ViewResolutionIntegrationTests`
  .`freemarkerWithExplicitDefaultEncodingAndContentType`** (6/7) — single
  plain-`AssertionError` failures each, not investigated.
- **`web.socket.messaging.StompWebSocketIntegrationTests`** (14/16 per the
  2026-07-20 baseline) — NOT re-verified with full detail this session; a
  200s rerun timed out (this test spins up a real embedded Tomcat per test
  method across 16 methods, and the host was under heavy concurrent load
  from several other sessions' builds/test-runs during this rerun attempt).
  No regression expected from anything touched this session, but the exact
  current pass count needs reconfirming with a longer timeout when the host
  is quieter.

**Confirmed unchanged / out of scope, no action taken:**
`core.io.ResourceTests` (66/68, same 2 `remoteResourceExists*` methods the
doc already flagged), `core.retry.RetryPolicyTests` (22/23, doc's own
"deliberate design choice, not worth fixing" stands),
`scheduling.quartz.QuartzSupportTests` (doc's own "environmental,
`spring-context-support` doesn't compile against the shared checkout"
stands), `beans.factory.xml.XmlBeanFactoryTests` (10/95, unchanged, doc
already has detailed root-causing for 2/10 pointing at a `try_build_replace
_override` `super_cid` class-resolution bug upstream of this file, likely
the same loader-identity family documented elsewhere in this repo's
history).

**Host note:** `/data/tmp/cores` (7.6GB of stale 2026-07-17 core dumps) was
cleared at the start of this session to relieve disk pressure (29G free
after, was 21G). The shared `spring-framework-recheck` checkout used for
classpath generation currently has uncommitted local modifications to
`spring-aop`/`spring-context` (`git status` shows deletions matching the
"corrupted `spring-aop/src` tree" symptom documented in the 2026-07-20 AOT
session) — NOT touched or fixed this session (shared resource, another
session may be mid-use); none of the modules this session's target classes
live in (`spring-core`/`spring-context`/`spring-web`/`spring-webflux`/
`spring-webmvc`/`spring-websocket`/`spring-orm`/`spring-context-support`/
`spring-test`) needed rebuilding, so this didn't block anything, but
whoever continues should check `git status` there before trusting a
`spring-aop`/`spring-orm` rebuild.

## Summary

Started from a full 2912-class suite run (dev `213d93ea`) fully triaged
against HotSpot (see history below), which found **177 confirmed genuine
bugs**. Reconfirmed by rerunning exactly those 263 previously-non-passing
classes on a fresh `dev` merge (`8719dca85`, ~3 days / several hundred
commits later), 4 shards, same settings (`suite-run.sh`, `BATCH=10
BATCH_TO=120 ONE_TO=120`, `CRATONVM_DEFAULT_HEAP_MAX_MB=2048`, real JDK 25).

| Of the 177 | Count |
|---|--:|
| Now OK (fixed) | 121 |
| Still FAIL | 38 |
| Still/newly TIMEOUT | 16 |
| Now LOADERR (was TIMEOUT) | 2 |
| **Still open** | **56** |

The 86 environmentally-non-OK classes (73 EMPTY + 13 FAIL matching HotSpot,
not CratonVM bugs) were not rerun individually here but the 263-class rerun
included them — EMPTY count held steady at 73, consistent with them still
being environmental.

## Notable clusters (current state, 2026-07-21)

**Still open, not fixed by this session:**
- `core.io.ResourceTests`: 2/68 methods (`remoteResourceExists`/
  `remoteResourceExistsFallback`) still return `0` from `lastModified()`
  for this specific `MockWebServer` HEAD-then-GET-fallback scenario,
  despite correctly parsing the header in an isolated
  `HttpURLConnection` probe — root cause not yet found; investigation was
  cut short by host instability (an unplanned reboot, then heavy
  concurrent load from other sessions causing repeated hangs on this
  exact test class).
- `core.retry.RetryPolicyTests`: 1/23 fails on a `toString()` regex
  expecting `"Lambda"` in a composed `Predicate`'s class name; CratonVM
  implements `Predicate.and()`/`or()`/`negate()` as named synthetic classes
  (`Predicate$And`/`$Or`/`$Negate`) rather than synthesizing true lambdas —
  a deliberate, widely-used design choice, not a bug worth touching for
  one cosmetic assertion.
- `jndi.JndiObjectFactoryBeanTests`: 1/25 fails
  (`lookupWithExposeAccessContext` — Mockito verifies `context.close()`
  called 2 times but sees 3, an extra close through an
  `exposeAccessContext` JDK dynamic proxy) — not yet investigated.
- `beans.factory.xml.XmlBeanFactoryTests`: 10/95 still fail (unchanged from
  the 2026-07-20 reconfirmation). Root-caused 2 of the 10
  (`overrideMethodByArgTypeAttribute`/`overrideMethodByArgTypeElement`,
  `<replaced-method>`/`ReplaceOverride` with `<arg-type>` overload
  disambiguation) partway: `native-builtins::spring_startup_bootstrap
  ::try_build_replace_override` mapped `methodName -> replacerBeanName`
  by NAME ONLY, ignoring `<arg-type>` entirely -- fixed by reading each
  `ReplaceOverride`'s real `getTypeIdentifiers()` (Spring 6.2.9+) and
  replicating `ReplaceOverride.matches(Method)`'s exact algorithm
  (overloaded-name arg-substring matching) in
  `jvm_descriptor_param_types_dot_notation`/`jvm_type_to_java_name`. This
  fix is real and landed (more correct than before for the general
  multi-overload-with-different-replacers case), but did NOT close these
  2 tests: traced with `CRATONVM_DBG_REPLOVR` to find the true blocker --
  `try_build_replace_override`'s `super_cid` parameter resolves to the
  WRONG class entirely for these 2 beans (`org/springframework/beans
  /factory/xml/SerializableMethodReplacerCandidate`, an unrelated helper
  class from a different test method in the same file, instead of the
  bean's actual declared class `OverrideOneMethod` -- confirmed via
  `javap` that the real `OverrideOneMethod.class` correctly has all 3
  `replaceMe()`/`replaceMe(int)`/`replaceMe(String)` overloads). This is a
  class-resolution/caching bug upstream of `try_build_replace_override`
  (in whatever resolves a `RootBeanDefinition`'s declared class to a
  `ClassId` before this function is called) -- likely the same family as
  other loader-identity/class-resolution bugs documented elsewhere in
  this repo's history, but not yet traced to its own root cause. The
  other 8/10 `XmlBeanFactoryTests` failures (`rejectsOverrideOfBogusMethodName`,
  `classNotFoundWithDefaultBeanClassLoader`,
  `replaceNonOverloadedInterfaceMethodWithoutSpecifyingExplicitArgTypes`,
  and the 3 CGLIB config-class-adjacent `context.annotation.*` failures
  below) were not investigated this session.

## Notable clusters (2026-07-20 session)

**AOT/TIMEOUT cluster — 16 classes, still fully hung**, plus 2 that flipped
from TIMEOUT to LOADERR (worth checking — a status-type change, not just
timing): `beans.factory.aot.BeanDefinitionMethodGeneratorTests` and
`beans.factory.aot.InstanceSupplierCodeGeneratorTests`. The still-hanging 16
grew slightly from the original 12 (picked up `test.context.aot.AotIntegrationTests`,
`web.service.registry.HttpServiceProxyRegistrationAotProcessorTests`,
`core.io.buffer.DataBufferTests`, `scripting.groovy.GroovyScriptFactoryTests`
— the last was FAIL before, now TIMEOUT). Full list in the table below
(`beans`, `context`, `orm`, `test`, `web` sections, all `TIMEOUT`/`LOADERR`
rows).

## AOT cluster — 2026-07-20 evening session

| Class | Before | After | Notes |
|---|---|---|---:|
| `beans.factory.aot.BeanRegistrationsAotContributionTests` | TIMEOUT | TIMEOUT (perf partially fixed — see below) | **genuine, severe performance defect — do not call this "not a bug".** Originally ~54 minutes (`3242228ms`). Measured HotSpot on the SAME classpath/JDK: **`13126ms` (13.1s)**, ~247x slower. Residual ~227x-vs-HotSpot gap remains and needs further investigation beyond the free-list fix -- the next lead is whatever dominates the OTHER 13 methods' time, not yet profiled. |
| `context.aot.ApplicationContextAotGeneratorTests` | TIMEOUT | FAIL **40/32/8** (2026-07-21 session; was 40/16/24) | Remaining 8 residuals include at least one distinct, unrelated bug: `@Value`-annotated field injection not reaching the proxied instance (`processAheadOfTimeWhenHasCglibProxyAndMixedAutowiring` now compiles and runs but asserts `"Hi null"` instead of `"Hi AOT World"`) -- not yet root-caused, a separate area from proxy generation itself. |
| `test.context.aot.AotIntegrationTests` | TIMEOUT | FAIL **4/0/2/2** (2026-07-21 rebaseline: found=4 succ=0 fail=2 skip=2) | **new dominant failure as of 2026-07-21** (supersedes the `TestContextAotException` shape below -- that may still be the residual once this is fixed, not yet re-checked): `java.lang.IllegalStateException: A custom 'searchEnclosingClass' predicate can only be combined with SearchStrategy.TYPE_HIERARCHY`, thrown from `MergedAnnotations$Search.withEnclosingClasses` via `TestContextAnnotationUtils.hasAnnotation` <- `TestContextAotGenerator`'s `isDisabledInAotMode` predicate. The calling code literally does `MergedAnnotations.search(SearchStrategy.TYPE_HIERARCHY).withEnclosingClasses(...)` in one expression -- the guard should trivially pass. Confirmed via a standalone minimal repro (`SearchStrategyProbe.java`, same call shape, no Spring-test/AOT machinery) that this is **not** a general enum `==` bug: the isolated repro passes cleanly on the SAME binary. The failure is specific to the real AOT/`@CompileWithForkedClassLoader` context. `CompileWithForkedClassLoaderClassLoader`'s constructor deliberately sets its OWN parent to `testClassLoader.getParent()` (skipping `testClassLoader` itself) and its `findClass` redefines any class it can pull bytes for via `testClassLoader.getResourceAsStream(...)` -- so framework classes (`MergedAnnotations`, `SearchStrategy`, `TestContextAnnotationUtils`) get a genuinely FRESH `Class`/enum-constant identity per forked test, by design (matches real CGLIB/Spring behavior, works fine on HotSpot). Suspected root cause: some CratonVM-side cache/registry (class definition, enum constant, or similar) is keyed by NAME ONLY rather than by (name, loader), letting one of the two sides of the `==` comparison resolve to a STALE instance from an earlier forked-loader instance instead of the current one -- the exact same bug *shape* as the StackWalker regression and the ApplicationContextAotGeneratorTests ReflectUtils-notification bug fixed the same session, just not yet localized to a specific cache/table. Ruled out: a standalone repro mimicking `CompileWithForkedClassLoaderClassLoader`'s EXACT parent-skip + resource-byte-redefine behavior (`ForkedLoaderProbe.java`/`SearchStrategyWorker.java`, no JUnit Platform involved), running the identical `MergedAnnotations.search(...).withEnclosingClasses(...)` call 3x through 3 fresh forked-loader instances, passed cleanly every time -- so the classloader-fork mechanism ALONE isn't sufficient to trigger it; the real cause needs something else specific to the full JUnit Platform Launcher machinery and/or `TestContextAnnotationUtils`/`TestContextAotGenerator` themselves (their own static state, or a DIFFERENT/nested classloader boundary somewhere in that path). Next step: instrument `identityHashCode`/`getClassLoader()` at both operands of the `==` directly inside a copy of `TestContextAnnotationUtils.hasAnnotation`, or bisect by replacing pieces of the JUnit Platform launch path in the standalone repro until it starts reproducing. |
| `test.context.aot.TestContextAotGeneratorIntegrationTests` | FAIL 0/4 | FAIL **4/0/4** (2026-07-21 rebaseline: found=4 succ=0 fail=4, regressed from 2/4 passing) | **same new dominant failure as AotIntegrationTests above** (`IllegalStateException: A custom searchEnclosingClass predicate...`) now hits ALL 4 methods (`processAheadOfTimeWithWebTests`, `processAheadOfTimeWithBasicTests`, `endToEndTests`), except `processAheadOfTimeWithXmlTests` which still shows the older `TestContextAotException: Failed to process test class [...XmlSpringVintageTests] for AOT` shape -- fix the searchEnclosingClass bug first, then re-baseline this class's residuals (the previously-documented 2 residuals below may or may not still apply). |
| `web.service.registry.HttpServiceProxyRegistrationAotProcessorTests` | TIMEOUT | FAIL **5/3/2** | now completes; **NEW finding, NOT JIT-related** (confirmed via `--nojit`, identical 3/5 either way): `java.lang.ArrayStoreException: arraycopy: source element at index 0 is not assignable to destination component type` inside `tools.jackson.databind.util.ArrayBuilders.insertInListNoDup`, thrown while creating the `httpServiceProxyRegistry` bean. Not yet root-caused — likely a reflection/generic-array-creation type bug feeding Jackson a wrongly-typed array, upstream of the `arraycopy` covariance check (which is behaving correctly by rejecting it). |
| `web.service.registry.ImportHttpServiceRegistrarTests` | FAIL 3/5 | FAIL **3/5** (unchanged count, but the ORIGINAL `ClassCastException: Class cannot be cast to String[]` this doc documented as fixed 2026-07-16 is confirmed gone) | same `ArrayStoreException` as the sibling class above — shared root cause, 2 methods (`basicListingWithAot`, `basicScanWithAot`). The previously-documented JDK24+ `java.lang.classfile.ClassFile` host gap does NOT explain this (host now runs real JDK 25 throughout this session's testing, which has that API). |
| `context.annotation.ConfigurationClassPostConstructAndAutowiringTests` | FAIL 1/2 | FAIL 1/2 (unchanged) | **out of scope** — verified this is a `@PostConstruct`/`@Autowired` circular-init bean-lifecycle bug (`UnsatisfiedDependencyException` on `setTestBean`), nothing to do with AOT code generation. Likely miscategorized into this doc's AOT-cluster table originally; leave for a bean-lifecycle investigation, not this cluster. |
| `context.annotation.ConfigurationClassPostProcessorTests` | FAIL 82/85 | FAIL 82/85 (unchanged) | **out of scope** — verified failures are CGLIB proxy method lookup (`NoSuchMethodException: getTestBean`), `@Bean` null-argument handling, config-override validation — none touch AOT/TestCompiler. Do not confuse with the separately-named, already-fixed `ConfigurationClassPostProcessorAotContributionTests` (see the 2026-07-15/16 loader-identity docs) — this is a different class. |

**Host-state gotchas hit and fixed this session** (worth knowing for
whoever continues): the shared `spring-framework-recheck` checkout used
for classpath generation had (a) a corrupted `spring-aop/src` tree
(283 of 314 `.java` files — the `aop.target` package was entirely
missing, breaking `spring-orm` compilation) and (b) a `spring-beans`
jar with one mismatched class-file entry (`AbstractBeanDefinition.class`
containing `BeanDefinition`'s bytecode) plus several modules' test-
fixtures jars (`*-test-fixtures.jar`) simply absent from `build/libs`.
Both were fixed by restoring `spring-aop/src` from the known-good
Windows reference checkout and force-rebuilding the affected jars
(`--rerun-tasks`). Neither was a CratonVM bug — both were host/checkout
corruption (plausibly from the same disk-pressure-driven "harvester"
process documented elsewhere in this repo's known-issues history) — but
they were initially indistinguishable from real compile failures and
cost real investigation time before being ruled out. **Always verify
the classpath/checkout integrity first** when a whole cluster of
AOT/compile-based tests shows the exact same `CompilationException`
shape.

**Recommended next steps for whoever continues this cluster:**
3. The `TestContextAotException` next step is SUPERSEDED — the dominant
   failure in both `test.context.aot.*` classes is now the
   `searchEnclosingClass`/`SearchStrategy` duplicate-`ClassId` bug (see
   below); `KRUN_STACK=1` is no longer the useful lever,
   `CRATONVM_DBG_DUPCLASS=1` is.
4. The `ArrayStoreException` finding IS root-caused now (see below) but
   NOT fixed — it's the SAME duplicate-`ClassId` mechanism as item 3, one
   level removed (an interface, not an enum). Fix both together.
5. **NEW**: the duplicate-`ClassId`-under-`@CompileWithForkedClassLoader`
   mechanism itself (items 3+4's shared root cause) needs a dedicated,
   careful session — likely the single highest-leverage remaining AOT-
   cluster fix, since it plausibly also explains some of
   `ApplicationContextAotGeneratorTests`'s remaining residuals
   (`processAheadOfTimeUsesCglibClassForFactoryMethod`'s intermittent
   `"is not an enhanced class"`) and possibly other `@CompileWith
   ForkedClassLoader`-using classes elsewhere in the suite not yet
   connected to this finding. See the recommended-next-step paragraph in
   the 2026-07-21 late session section for the two candidate fix shapes.
6. `beans.factory.aot.BeanRegistrationsAotContributionTests` perf: the
   free-list O(1) fix landed but the class is still ~227x slower than
   HotSpot and TIMEOUTs; profile which of the OTHER 13 methods (only 1 of
   14 hit the free-list pathology) dominates next.

## AOT cluster — 2026-07-21 late session

Remaining residuals include the previously-documented `@Value`-field-injection gap
(`processAheadOfTimeWhenHasCglibProxyAndMixedAutowiring`) and
`processAheadOfTimeUsesCglibClassForFactoryMethod`'s
`IllegalArgumentException: ... is not an enhanced class` (order-dependent,
only seen on some runs — likely the SAME underlying duplicate-`ClassId`
mechanism below, not yet confirmed).

**Unifying root-cause finding (NOT fixed, needs a dedicated session):
`searchEnclosingClass` (`test.context.aot.*`) and the Jackson
`ArrayStoreException` (`web.service.registry.*`) are the SAME bug shape.**
Added an opt-in diagnostic (`CRATONVM_DBG_DUPCLASS=1`,
`classloading/src/class_manager.rs::resolve_fast_path_class_id`) that logs
whenever a name lookup REJECTS an existing `UserDefined`-loader class
registration in favor of creating a fresh one under `Application` (because
the built-in delegation chain can also find the class's bytes — see that
function's own doc comment for why this is deliberate, added to fix an
earlier, different bug). Running `AotIntegrationTests` with it on shows
`org/springframework/core/annotation/MergedAnnotations$SearchStrategy`
(the exact enum `withEnclosingClasses`'s `IllegalStateException` compares
with `==`) hitting this rejection path 3 times — i.e. the SAME class name
is registered under (at least) two DIFFERENT `ClassId`s: one under the
`@CompileWithForkedClassLoader` fork's own `UserDefined` loader (which
redefines every framework class it can pull bytes for, by design, so code
running inside that forked context should see ITS OWN `SearchStrategy`
identity) and a second, separate one created by any loader-BLIND
name-only resolution helper (`ensure_class_initialized`/
`load_class_concurrent`/`resolve_fast_path_class_id`) reached from that
same forked context — those helpers have no notion of "which loader is
asking" and default to preferring `Application` whenever the built-in
chain can also serve the class, silently creating a duplicate instead of
reusing the forked loader's copy. `MergedAnnotations.search(SearchStrategy
.TYPE_HIERARCHY).withEnclosingClasses(...)`'s two `SearchStrategy.
TYPE_HIERARCHY` references (the literal passed to `.search(...)` and the
one `withEnclosingClasses` compares against with `==`) can therefore
resolve to two numerically-different-but-logically-identical enum
constants depending on which resolution path each one took.

Traced the EXACT SAME mechanism independently for the
`web.service.registry.*` `ArrayStoreException`: `tools/jackson/databind/
deser/KeyDeserializers` (an interface, not an enum, but the identical
duplicate-`ClassId`-for-one-name shape) is registered under two different
`ClassId`s; `old.getClass().getComponentType()` re-derives the component
by NAME (`ensure_class_initialized`) rather than reading back the array's
own already-correct component `ClassId`, so `Array.newInstance(...)`
allocates a new array tagged with the WRONG (duplicate) `ClassId`, and the
subsequent `System.arraycopy` of the old elements into it correctly
throws `ArrayStoreException` against that mismatch. Tried the
locally-obvious fix (route `getComponentType()`'s array branch through the
class registry's own `array_info.component_class_id` instead of the name
string) but discovered `array_info` is populated `None` at EVERY
class-construction site in the codebase (`grep -rn 'array_info: Some'`
across `classloading/src/` returns zero matches) — it's a fully unwired
stub, not a locally-fixable gap, so that patch was reverted rather than
landed as dead code.

**Recommended next step for whoever continues this specific finding**:
this is a genuine, structural gap — native helpers that resolve a class
by NAME ALONE (`ensure_class_initialized`, and whatever underlies
`Array.newInstance`'s reflective component resolution) have no way to
know which loader/context is asking, so under
`@CompileWithForkedClassLoader` (and likely any other scenario where a
custom loader redefines a framework class already reachable via the
built-in delegation chain) they can silently duplicate a class the
CALLER's own context already has a perfectly good copy of. A full fix
needs either (a) threading the CALLER's defining-loader id through these
name-only resolution helpers so they can consult
`class_id_defined_by_loader_exact` first (the pattern already used
correctly a few lines away in `native_class_get_component_type`'s
`classLoader`-field branch), or (b) wiring up `array_info` properly at
every array-class synthesis site so `array_component_class_id` (already
present on the `NativeContext` trait for exactly this purpose) stops
being a permanent no-op. Both are bigger, riskier changes than fit safely
in one sitting — reproduce first with `CRATONVM_DBG_DUPCLASS=1` on
`AotIntegrationTests` or the `web.service.registry.*` classes before
attempting either.

**Follow-up same session: landed a real, partial improvement, but the
full fix is bigger than initially scoped -- three distinct loader-blind
code paths identified, not one.** Added `CRATONVM_DBG_DUPCLASS_BT=1`
(full backtrace on every rejected duplicate-registration) to make this
tractable, then traced both bugs to their EXACT call sites:

1. **`searchEnclosingClass` goes through `resolve_class_loader_aware` /
   `should_use_loader_initiated_resolution`** (`vm/src/runtime/
   interpreter.rs`) -- the SAME loader-aware `CONSTANT_Class`/field-ref
   resolution mechanism already built (and gated off by default) for the
   Tomcat/Hibernate/WildFly custom-loader work, with an EXISTING narrow,
   type-checked carve-out for `GroovyClassLoader`. Widened that carve-out
   to also match Spring's `CompileWithForkedClassLoaderClassLoader`
   (`is_compile_with_forked_class_loader`, mirroring `is_groovy_class_loader`
   exactly -- exact-`ClassId` match, no `is_subclass_of` walk needed since
   the class is `final`). Verified via `CRATONVM_DBG_LOADER_TRACE=1` that
   this DOES work as intended for at least one call site: a `getstatic
   SearchStrategy.TYPE_HIERARCHY` reached from `BootstrapUtils` (itself
   loaded by the fork) now correctly drives the fork's OWN loader first
   and lands on the fork's own consistent `SearchStrategy` `ClassId`,
   instead of falling straight to the global fast path.
   **But this alone does not fix either failing test.** The SAME trace
   shows a SECOND `getstatic SearchStrategy.TYPE_HIERARCHY` -- reached
   from `MergedAnnotations$Search.withEnclosingClasses`'s OWN bytecode
   (the `Assert.state(this.searchStrategy == SearchStrategy.TYPE_HIERARCHY,
   ...)` check that actually throws) -- with `referencing_loader=
   Some(Application)`, NOT the fork. So `MergedAnnotations$Search` itself
   is NOT being given its own forked-loader copy in this VM, even though
   (per Spring's documented design intent, and the doc's own earlier
   writeup) it should be, alongside every other framework class the fork
   redefines. Two references to the same enum constant, resolved via two
   different loaders (fork vs. Application), is the actual mismatch --
   narrower and different from the original hypothesis ("one resolution
   helper is loader-blind"). WHY `MergedAnnotations$Search` itself ends up
   Application-scoped instead of fork-scoped is not yet root-caused --
   likely something in how/when that specific class first got loaded
   in this JVM (possibly before the current test's fork instance even
   existed), which is a `ClassLoader.loadClass()`-level delegation
   question, not a `resolve_class_loader_aware` question -- needs its own
   trace (`CRATONVM_DBG_LOADER_TRACE` widened to also fire on
   `MergedAnnotations$Search`'s OWN class resolution, not just
   `SearchStrategy`'s).

2. **The Jackson `ArrayStoreException` goes through a COMPLETELY
   DIFFERENT path**: `native_object_get_class` (`Object.getClass()`,
   `native-builtins/src/lib.rs`) calls `ctx.load_class(&array_class_name)`
   directly on a synthesized `"[L...;"` descriptor string, which recurses
   into `classloading::class_manager::synthesize_array_class`, which
   resolves the COMPONENT via a bare `self.load_class(component_name)` --
   never touching `resolve_class_loader_aware` at all. So fix #1 above is
   structurally irrelevant to this bug; it needs its own fix in
   `synthesize_array_class` (or its caller). Tried the obvious one --
   populate the always-`None` `array_info` field on the synthesized array
   `Class` with the component `ClassId` this function ALREADY resolves
   internally (pure-additive: nothing currently reads `array_info`, so
   this cannot regress anything; the field's own doc comment even says
   "Wire up real `ArrayInfo` once a consumer... actually reads it", i.e.
   this was always the planned next step) -- but on reflection this does
   NOT reliably fix the bug either: `synthesize_array_class` caches ONE
   array `Class` GLOBALLY per descriptor NAME (not per (loader, name)),
   so whichever caller happens to synthesize `"[Ltools/jackson/databind/
   deser/KeyDeserializers;"` FIRST in the JVM session permanently decides
   `array_info.component_class_id` for every LATER `getClass()` call on
   ANY `KeyDeserializers[]` array, regardless of that specific array's
   own actual (and possibly different) component `ClassId` -- the same
   class-of-bug one level up, just baked into the array-class cache
   instead of the plain-class cache.
   **Deliberately did NOT change the array class's own `loader_id`
   scoping to fix this** (the more "correct-per-JVMS-5.3.3" fix for
   reference-component arrays) -- `synthesize_array_class` has an
   existing, deliberate, audited invariant enforcing `loader_id ==
   Bootstrap` unconditionally regardless of component loader
   ("Round 7 audit fix (CRIT #2)", with a `debug_assert_eq!` guarding
   it and an explicit comment warning future contributors not to change
   `Class::loader_id` without updating the map key too). That invariant
   was presumably added to fix a DIFFERENT, real bug this session has no
   visibility into -- touching it without understanding that history first
   is exactly the kind of change that looks locally correct and
   regresses something else. Left `array_info` un-populated (reverted)
   rather than land a fix that looks plausible but is not verified
   correct.

**Revised recommended next steps**, in order of leverage:
1. Trace `MergedAnnotations$Search`'s OWN class resolution (not
   `SearchStrategy`'s) with `CRATONVM_DBG_LOADER_TRACE`/
   `CRATONVM_DBG_DUPCLASS_BT` to find why it ends up Application-scoped
   instead of fork-scoped inside a `@CompileWithForkedClassLoader` test --
   this is probably a `ClassLoader.loadClass()` top-level delegation bug
   (is `findLoadedClass`/`cl_find_loaded_class` genuinely being consulted
   for EVERY class the fork's `loadClass()` bytecode touches, or is there
   a shortcut somewhere that returns an already-cached Application answer
   without ever asking the fork loader instance at all?), not a
   constant-pool-resolution bug -- different mechanism, different fix
   location, from item 1 above.
2. Before touching `synthesize_array_class`'s loader-scoping, read the
   Round 7 CRIT #2 audit history (git blame / commit message on the
   `debug_assert_eq!` near the end of that function) to understand what
   it was protecting against, so a loader-scoped-for-reference-arrays fix
   can coexist with whatever that was.
3. Once (1) is understood, re-attempt the `array_info` wiring from a
   position of already knowing whether array classes need per-loader
   caching too, rather than guessing.

**Pushed item 1 (above) one step further with `CRATONVM_DBG_LOADER_TRACE`
widened to `MergedAnnotations`/`MergedAnnotations$Search` too, not just
`SearchStrategy`.** At least THREE distinct `MergedAnnotations` outer-class
copies coexist in the SAME JVM run of `AotIntegrationTests` alone: one
under `UserDefined(3)` (one test method's fork), one under `UserDefined(4)`
(a DIFFERENT test method's fork), and one under plain `Application`
(loaded before any fork existed, plausibly by JUnit's own internal
annotation scanning). Each resolves its OWN nested `$Search` class
correctly and self-consistently through the SAME loader
(`UserDefined(3)`'s `MergedAnnotations` -> `UserDefined(3)`'s `Search`;
`Application`'s `MergedAnnotations` -> `Application`'s `Search` --
`resolve_class_loader_aware`/the new carve-out from item 1 works
correctly for ALL three, individually). The ACTUAL failing
`withEnclosingClasses` call executes on an INSTANCE of the
**`Application`-scoped** `Search` class -- meaning whatever code calls
`MergedAnnotations.search(SearchStrategy.TYPE_HIERARCHY)` in the failing
path (`TestContextAnnotationUtils`/`TestContextAotGenerator`'s
`isDisabledInAotMode` predicate, reached via reflection --
`native_method_invoke`/`native_method_invoke_boxed` frames present in
the full backtrace) itself resolves `MergedAnnotations` to the
`Application` copy, not a forked one. If EVERYTHING downstream of that
call also consistently resolved via `Application` (which the "self-
consistent" pattern above says it should), there would be no bug -- so
the actual `SearchStrategy.TYPE_HIERARCHY` value flowing into
`this.searchStrategy` must be getting resolved through a DIFFERENT
loader context than the `Search` instance's own class does. The two
most likely explanations, neither confirmed: (a) the calling method is
itself a lambda/method-reference whose generated class's defining loader
differs subtly from the class that lexically declared it, or (b) the
reflective `Method.invoke()` path (visible in the backtrace) resolves a
literal constant argument in the CALLER frame's context rather than the
declared method's, which is a JIT-adjacent misattribution just like
several of the OTHER argument-decode bugs already fixed elsewhere in
this codebase (see `wildfly-jit-arg-decode-unboxed-primitive-triple-
misattribution` in the fixed-bug archive for the general shape). This
needs live-debugging or per-frame identity instrumentation right at the
`Method.invoke()` boundary to pin down further -- log-based tracing alone
cannot distinguish these two theories. Stopping here for this session;
the `CRATONVM_DBG_LOADER_TRACE` substring widening (`MergedAnnotations`)
is left in place alongside the earlier `SearchStrategy` one for whoever
picks this back up.

**Groovy: `context.groovy.GroovyBeanDefinitionReaderTests` and
`scripting.groovy.GroovyScriptFactoryTests` are still hung (TIMEOUT), and
`web.servlet.view.groovy.GroovyMarkupViewTests` still FAILs (9/10).**

## Full class list (66), by module

### Beans

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `beans.factory.aot.BeanRegistrationsAotContributionTests` | TIMEOUT (severe perf defect — ~247x slower than HotSpot; free-list O(1) fix landed 2026-07-21, ~8.2% aggregate improvement so far, see above) | 0/0 | 350000ms |
| `beans.factory.xml.XmlBeanFactoryTests` | FAIL | 85/95 | 85837ms |

### Context

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `context.annotation.ComponentScanParserBeanDefinitionDefaultsTests` | TIMEOUT | 0/0 | 120000ms |
| `context.annotation.ConfigurationClassPostProcessorTests` | FAIL (2026-07-21 FactoryBean/CGLIB session: 4 genericsBasedInjectionWith* fixed, see notes) | 80/85 | ~25000ms |
| `context.aot.ApplicationContextAotGeneratorTests` | FAIL (2026-07-20, see caveat above — needs re-verify) | 16/40 | 389879ms |
| `context.groovy.GroovyBeanDefinitionReaderTests` | TIMEOUT | 0/0 | 120000ms |

### Core

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `core.io.ResourceTests` | FAIL | 66/68 | 4689ms |
| `core.io.buffer.DataBufferTests` | TIMEOUT | 0/0 | 120000ms |
| `core.retry.RetryPolicyTests` | FAIL | 22/23 | 828ms |

### Jndi

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `jndi.JndiObjectFactoryBeanTests` | FAIL | 24/25 | 2165ms |

### Orm

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `orm.jpa.support.PersistenceInjectionTests` | FAIL | 26/27 | 11461ms |

### Scripting

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `scripting.groovy.GroovyScriptFactoryTests` | TIMEOUT | 0/0 | 120000ms |

### Test

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `test.context.aot.AotIntegrationTests` | FAIL (2026-07-20, now completes, see above) | 0/4 | 56296ms |
| `test.context.aot.TestContextAotGeneratorIntegrationTests` | FAIL (2026-07-20, improved, see above) | 2/4 | 148117ms |
| `test.context.junit.jupiter.event.ParallelApplicationEventsIntegrationTests` | FAIL | 0/2 | 918ms |
| `test.context.junit.jupiter.parallel.ParallelExecutionSpringExtensionTests` | TIMEOUT | 0/0 | 120000ms |
| `test.web.servlet.assertj.MockMvcTesterIntegrationTests` | FAIL | 72/74 | 58069ms |

### Web

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `web.client.RestClientIntegrationTests` | FAIL (2026-07-21, improved via side effect) | 227/230 | ~30000ms |
| `web.client.RestTemplateIntegrationTests` | FAIL (2026-07-21, improved via side effect) | 119/125 | ~20000ms |
| `web.reactive.function.client.WebClientIntegrationTests` | FAIL (2026-07-21, reconfirmed, 1 fail + 1 skip) | 168/170 | 23337ms |
| `web.reactive.result.method.annotation.CrossOriginAnnotationIntegrationTests` | TIMEOUT | 0/0 | 120000ms |
| `web.reactive.result.method.annotation.RequestMappingMessageConversionIntegrationTests` | TIMEOUT | 0/0 | 120000ms |
| `web.service.registry.HttpServiceProxyRegistrationAotProcessorTests` | FAIL (2026-07-20, now completes, NEW bug found, see above) | 3/5 | 51669ms |
| `web.service.registry.ImportHttpServiceRegistrarTests` | FAIL (original CCE confirmed gone, new shared bug, see above) | 3/5 | 54239ms |
| `web.servlet.config.MvcNamespaceTests` | FAIL (2026-07-21, reconfirmed, not investigated) | 24/25 | 24938ms |
| `web.servlet.config.annotation.ViewResolutionIntegrationTests` | FAIL (2026-07-21, reconfirmed, not investigated) | 6/7 | 29415ms |
| `web.socket.messaging.StompWebSocketIntegrationTests` | FAIL (not re-verified 2026-07-21, host load prevented reconfirmation, see notes) | 14/16 | 105475ms |

## Raw data

- Original full-suite triage (177 bugs): 8 shards, dev `213d93ea`,
  binary `cratonvm-fullsuite-20260717.bin`, cross-referenced against HotSpot
  (516-class baseline + a fresh 129-class targeted HotSpot rerun).
- Reconfirmation rerun (this update): 4 shards, dev `8719dca85`, binary
  `cratonvm-fullsuite2-20260720.bin`, `LIST=` the exact 263 non-OK classes
  from the original run.
- Per-class FAILCAUSE and crash-log detail available in
  `/data/tmp/nonpassed263-s{0..3}/{failcauses,crashes}.log` on the Azure
  host at capture time.

## 2026-07-22 AOT follow-up loader identity and synthetic StringBuilder fixes

Worktree: `/data/wt-aot-cluster-complete-20260721-019f873e` (branch
`codex/aot-cluster-complete-20260721-019f873e`), built against the real
JDK 25 Spring fixture. This follow-up intentionally remains **OPEN**: it
eliminated the previously dominant failures below, then exposed a later,
separate generated-AOT execution residual.

### Newly exposed residual not fixed

After generated compilation,
`AotIntegrationTests.endToEndTestsForBeanOverrides` runs its 175-test
AOT-mode suite with **73 successful / 102 failed**. The ordinary direct run
of the same bean-override test class passes, so this is specific to
generated-AOT/forked-loader execution. The first shared symptom is Log4j
plugin configuration failing during reflective field/factory wiring with
`IllegalArgumentException: argument type mismatch` in
`PluginBuilder.injectFields`, followed by missing Logger/Root plugin
objects. This is consistent with another loader-identity/reflective
assignability boundary, but has not yet been localized enough for a safe
fix.

A 360-second full `AotIntegrationTests` r10 run advanced beyond this first
generated suite into later AOT processing, then hit the external timeout;
therefore neither it nor the broader AOT/TIMEOUT list should be marked
complete. Continue from the single-method probe
`/data/aotcomplete-probes-019f873e/KRunMethod` and log
`/data/aotcomplete-r10-singlemethod.log`.

## 2026-07-22 AOT follow-up 2 -- bean-override double-context-refresh root-caused, one contributing gap fixed

Worktree `/data/wt-aot-cluster-final-20260722` (branch
`fix/aot-cluster-final-20260722`). Follows directly from the "2026-07-22 AOT
follow-up" session above, which got `AotIntegrationTests` past generated
compilation but exposed `endToEndTestsForBeanOverrides` running its 175-test
generated-AOT suite at only 73/175. **This session confirms the Log4j
`PluginBuilder.injectFields` error documented there is a red herring** --
transient/host-load-dependent, reproduces on 1 of 10+ repeat runs of the
identical binary/command, and does NOT correlate with the actual 102 test
failures (a full run with the Log4j error absent still showed 102 failures).
Do not chase it further; if it recurs, just retry.

**Root cause of the real 102 failures (`@TestBean`/`@MockitoBean`/
`@MockitoSpyBean` overrides silently not applying) traced to a genuine
double-`ApplicationContext`-refresh bug**, isolated with a fast (~30-40s,
not 300s+) standalone repro
(`org.springframework.test.context.aot.AotIntegrationTests.runEndToEndTests`'s
API called directly against ONE minimal `@TestBean`-using fixture class, own
copy so diagnostics can be added without touching the shared checkout --
saved at `/data/tmp/aotcluster-final-repro/BeanOverrideProbe2.java` on the
Azure host). Confirmed via a `BeanFactoryAware`+`SmartInitializingSingleton`
probe bean that the SAME logical test gets its `DefaultListableBeanFactory`
refreshed TWICE -- once correctly (through
`BeanOverrideContextCustomizer.customizeContext()`, which calls the override
factory and `registerSingleton`), and a SECOND time on a completely
different `BeanFactory` instance that skips customization entirely (so
`preInstantiateSingletons()` creates the plain `@Bean`-defined production
value instead) -- and the SECOND, wrong context is what ends up bound to the
actual `@Test` method. Ruled out a `ConcurrentHashMap` correctness bug via
direct native tracing (`native_chm_get`/`native_chm_put_if_absent` in
`native-collections/src/lib.rs`) -- put/get round-trips perfectly for the
FIRST (correct) BeanFactory's `singletonObjects` map every time it was
queried; the bug is entirely about which `BeanFactory` instance ends up
wired to the test.

Traced (with the EXISTING `CRATONVM_DBG_DUPCLASS=1`/`CRATONVM_DBG_DUPCLASS_BT=1`
and `CRATONVM_FORNAME_TRACE=1` diagnostics, no new code needed to find this
part) to `AotTestContextInitializersFactory`/`AotTestContextInitializers`/
`AotMergedContextConfiguration`/`DefaultCacheAwareContextLoaderDelegate`
(all in `org.springframework.test.context.{aot,cache}`) repeatedly
re-resolving to FRESH Application-loader `ClassId`s instead of reusing the
`@CompileWithForkedClassLoader` fork's own already-loaded copy
(`classloading::class_manager::resolve_fast_path_class_id`'s documented
"always prefer Application over a lone UserDefined candidate" behavior).
Since `AotTestContextInitializersFactory`'s static double-checked-locking
cache (`private static volatile Map<...> contextInitializerClasses`) lives
on the CLASS object, a fresh `ClassId` means fresh (null) static state, so
`Class.forName`-based lookups re-run and can yield a different `Class`
object for the generated `TestContextNNN_ApplicationContextInitializer`.
`AotMergedContextConfiguration.hashCode()`/`.equals()` are defined purely in
terms of THAT `Class` object's identity (by design, matching real JDK
`Class` semantics) -- so a second, non-identical `Class` object busts
`DefaultContextCache.contextMap`'s cache-hit check on a later
`DefaultCacheAwareContextLoaderDelegate.loadContext()` call, causing a
second, uncustomized context load from scratch.

**This fix alone did NOT close the bean-override bug.** Re-running the
repro against the patched binary, `CRATONVM_DBG_DUPCLASS_BT=1` shows the
dominant remaining rejection now comes through a DIFFERENT, deeper path:
```
resolve_fast_path_class_id (classloading/src/class_manager.rs:2783)
  <- load_class_concurrent (vm/src/vm/vm_init.rs:3824)
  <- resolve_class_loader_aware (vm/src/runtime/interpreter.rs:17795)
  <- execute_instruction [New bytecode] (vm/src/runtime/interpreter.rs:15022)
```
i.e. a plain `new AotTestContextInitializers()` instruction (almost
certainly inside `DefaultCacheAwareContextLoaderDelegate`'s own
constructor) resolves its target class via `resolve_class_loader_aware` --
which the existing `is_compile_with_forked_class_loader` carve-out in
`should_use_loader_initiated_resolution` is SUPPOSED to make loader-aware
for exactly this scenario -- but still falls through to the global answer.
Since `should_use_loader_initiated_resolution` gates on
`defining_loader_for(referencing_class_id)` matching the fork's own loader,
and `DefaultCacheAwareContextLoaderDelegate` ITSELF is also among the
classes being repeatedly rejected/re-resolved per the same `DBG_DUPCLASS`
evidence, the most likely explanation is that the REFERENCING class (not
just its downstream reference) is itself sometimes resolved to the wrong
(Application-loader) copy at the moment this instruction executes -- a
strictly harder, more foundational problem than the single missing
fallback fixed above. **Recommended next step**: use
`CRATONVM_DBG_DUPCLASS_BT=1` against the fast repro
(`/data/tmp/aotcluster-final-repro/BeanOverrideProbe2.java` on the Azure
host, ~30-40s iteration, see the companion memory doc
`aot-beanoverride-double-context-refresh-rootcaused-20260722` for the full
probe-construction gotchas) to find WHERE
`DefaultCacheAwareContextLoaderDelegate`/`TestContextManager`'s own class
identity first goes wrong, then apply the same narrow, exact-`ClassId`-match
pattern used elsewhere in this file's history to that specific resolution
site.

**Still fully OPEN**: `AotIntegrationTests#endToEndTestsForBeanOverrides`
(73/175, dominated by this bug -- unchanged by this session's fix),
`ApplicationContextAotGeneratorTests`'s remaining 8 residuals (the `@Value`
field-injection gap, `"Hi null"` instead of `"Hi AOT World"`, is plausibly
the SAME double-refresh mechanism -- not yet cross-checked),
`test.context.aot.TestContextAotGeneratorIntegrationTests` (needs
re-baseline after this AND the prior session's fixes -- not done),
`beans.factory.aot.BeanRegistrationsAotContributionTests` (the separately-
tracked ~227x-vs-HotSpot interpreter throughput defect -- untouched this
session, needs dedicated profiling work, not a correctness bug).

## 2026-07-22 AOT follow-up 3 -- second contributing fix landed, real mechanism for the residual now identified

Same worktree/branch as follow-up 2. Landed a second, real, verified-safe
fix, then traced the remaining bug to its actual mechanism.

**The double-context-refresh bug still reproduces** (confirmed against the
post-both-fixes binary). The loader trace shows why: partway through the
SAME test's lifecycle, a **second, genuinely different**
`DefaultCacheAwareContextLoaderDelegate` **object** comes into play --
`ClassId(6948)`, with `referencing_loader=Some(Application)` (not a
disagreement this time; `class_manager` and the side table AGREE this copy
is Application-loaded) -- and it resolves ITS OWN self-reference and
everything downstream (presumably `AotTestContextInitializers`/
`AotMergedContextConfiguration` too) via the ordinary global path, correctly
per ITS OWN loader identity, landing on the Application-loader's answer.
Since this is a **different delegate instance** (not just a different
`ClassId` for symbolically resolving the SAME logical singleton), it has its
own, independent `DefaultContextCache`, which naturally has never seen the
first delegate's customized context -- so it loads a fresh, uncustomized one
from scratch. Correlated by timing against the `BeanOverrideProbe2` probe's
own timestamped prints: the first (fork-scoped, `ClassId 2486`) delegate is
used for the correctly-customized context (matches
`BeanOverrideTestExecutionListener.prepareTestInstance` -> `injectFields` ->
`testContext.getApplicationContext()`, the FIRST of the two `loadContext()`
call sites `DefaultCacheAwareContextLoaderDelegate.loadContext()`'s own
Javadoc documents); the second (Application-scoped, `ClassId 6948`) delegate
appears shortly before the wrongful `bean1()` call, consistent with the
SECOND call site (the `@Test` method's own `ApplicationContext ctx`
parameter resolution).

**This reframes the remaining problem**: it is very likely NOT a
symbolic-class-resolution bug at all (the mechanism this doc's several AOT
sessions have been fixing all day) but a **`TestContextManager`/
`DefaultCacheAwareContextLoaderDelegate` object-instantiation duplication**
-- i.e. two DIFFERENT calls to `new DefaultCacheAwareContextLoaderDelegate()`
(or whatever constructs/caches the ONE that should be shared for a given
test) happening under two different loader contexts and NOT being
recognized as "the same test's infrastructure" by whatever caches/scopes
`TestContextManager` instances across a test's lifecycle -- plausibly
JUnit Jupiter's own `ExtensionContext.Store` (keyed by `Namespace` +
key objects, which can be subject to the exact same `Class`-identity-based
cache-key instability if `SpringExtension`'s own class resolves
inconsistently under the fork) rather than anything AOT-specific. **This is
a THIRD, distinct investigation layer** (JUnit's own extension-store
caching, not Spring's `DefaultContextCache` or CratonVM's `ClassId`
resolution) and was not pursued further this session -- flagged as the
concrete next step for whoever continues.

**Recommended next step**: widen `CRATONVM_DBG_LOADER_TRACE`'s filter
(already trivial to do, see this session's pattern) to also cover
`SpringExtension`/`TestContextManager`/`ExtensionContext` class names, and
add print instrumentation (via a custom probe fixture, NOT the shared
checkout) around `SpringExtension`'s `getTestContextManager(ExtensionContext)`
-- the actual JUnit-side store lookup -- to determine whether it's finding
two different `Store` instances, two different cached `TestContextManager`
values under the same key, or constructing a fresh one each time due to a
key-equality failure. The fast repro
(`/data/tmp/aotcluster-final-repro/BeanOverrideProbe2.java`, or its
non-forked sibling `BeanOverrideProbe3.java` used to confirm forking is
REQUIRED to trigger this at all -- both on the Azure host) remains the
fastest iteration path (~30-60s depending on host load and whether
`CRATONVM_DBG_LOADER_TRACE` is enabled, which adds significant overhead --
prefer `CRATONVM_DBG_DUPCLASS`/targeted name filters over blanket tracing).

## 2026-07-23 AOT follow-up 4 -- environment gotcha found, two more hypotheses on the double-refresh bug REFUTED

Worktree `/data/wt-aot-junitstore-20260723` (branch `fix/aot-junitstore-
20260723`, from `origin/dev` `cd23640d9`, which already contains every fix
from the prior three follow-ups above -- no new source changes landed this
session, only investigation). No subagents used, per this task's standing
instruction.

**Environment gotcha (not a CratonVM bug, but wasted significant time before
being found)**: the Azure host's PATH-default `java` is OpenJDK 21, which
lacks `java.lang.classfile` (JEP 484, stable since JDK 24). Spring 7's
`ClassFileMetadataReader` (a `src/main/java24` Multi-Release-JAR class,
confirmed genuinely MRJAR-packaged and genuinely selected correctly by
CratonVM) is reached by `ConfigurationClassParser.retrieveBeanMethodMetadata`
whenever a `@Configuration` class has **2+** `@Bean` methods -- which the
`BeanOverrideProbe2` repro's `Probe` diagnostic bean (added in the prior
session) pushed it into. Without a JDK 24+ boot image, this throws
`NoClassDefFoundError: java/lang/classfile/ClassFile`, which
`TestContextAotGenerator` (constructed with `failOnError=false`, the fast
repro's default) silently swallows into a WARN log, surfacing only as the
generic, misleadingly-familiar `IllegalStateException: Failed to load AOT
ApplicationContextInitializer class` -- easy to mistake for the DIFFERENT,
already-diagnosed "incomplete `@Nested` testClasses list" probe artifact from
the prior session's Finding 2. Confirmed via `failOnError=true` to see the
real `Caused by:` chain, and via a from-scratch `Class.forName("java.lang.
classfile.ClassFile")` micro-repro (throws `ClassNotFoundException` by
default, loads fine once pointed at a real JDK 24+ image). Fix: export
`CRATONVM_JAVA_HOME=/data/jdk25-real-20260717/jdk-25.0.3+9` (also reachable
via the shorter symlink `/home/victor/jdk25`, which other concurrent sessions
were already using via the `--java-home` CLI flag) before any ad-hoc
`KRun`/`KRunMethod` repro against Spring 7 fixtures. `classfile_api.rs`'s
`SyntheticStub` registrations are a deliberate non-implementation of JEP 484
(see its own doc comment) and do not make the class independently loadable
without a real backing classfile -- this is by design, not a gap to fix.
See `[[aot-cluster-missing-cratonvm-java-home-jdk25]]` (memory) for full
detail, including the open question (being checked as of this writeup) of
whether this same missing env var partially explains this doc's own
`ApplicationContextAotGeneratorTests`/`TestContextAotGeneratorIntegrationTests`
pass counts below.

**Double-context-refresh bug (`AotIntegrationTests#endToEndTestsForBeanOverrides`,
73/175): two more of the leading hypotheses from the 2026-07-22 write-ups
above are REFUTED, with hard evidence, once the environment gotcha was
fixed and the fast repro reached the real bug again**:
- **NOT JUnit `ExtensionContext.Store` returning a different cached
  `TestContextManager`.** A same-package (`org.springframework.test.context.
  junit.jupiter`) read-only diagnostic extension, registered via
  `@ExtendWith` alongside `@SpringJUnitConfig` on the repro fixture, printed
  `identityHashCode` of the Store's cached `TestContextManager` at
  `beforeAll`/`postProcessTestInstance`/`beforeTestExecution`/
  `afterTestExecution` -- IDENTICAL at all four points, bracketing both the
  correct and the wrong `Probe` lifecycle. There is exactly one
  `TestContextManager` for the whole test.
- **NOT a residual `ClassId`/loader-identity instability for the generated
  initializer class.** The same diagnostic called the public
  `new AotTestContextInitializers().getContextInitializerClass(testClass)`
  at all four points -- identical `Class` object (identity, `.hashCode()`,
  `.equals(self)`) every time, across 3 repeat runs. This independently
  corroborates `[[spring-aot-cluster-loader-identity-fixes-20260715]]`'s
  claim that `b0100920a`/`f5831451a` actually closed this layer.
- **NOT `AotDetector.useGeneratedArtifacts()` flipping.** Same diagnostic,
  also printed at all four points: `true`/`true`/`true` throughout, so
  `DefaultCacheAwareContextLoaderDelegate.replaceIfNecessary` is confirmed
  taking the `AotMergedContextConfiguration`-wrapping branch consistently.
- **NOT a generic `LinkedHashMap`(access-order)+`Collections.synchronizedMap`
  correctness bug** (`DefaultContextCache.contextMap`'s exact shape,
  untested by the earlier `ConcurrentHashMap`-only ruling-out for
  `singletonObjects`). A standalone, Spring-free probe reproducing the exact
  key shape (`equals`/`hashCode` delegating to a wrapped `Class`, matching
  `AotMergedContextConfiguration` exactly) with unrelated noise puts/gets in
  between showed correct cache-hit behavior on CratonVM.

Every input to the delegate's cache-hit decision that is observable from
outside the `org.springframework.test.context.cache` package is now
confirmed stable and correct, yet the bug still reproduces identically (two
`Probe.setBeanFactory`/`afterSingletonsInstantiated` cycles with different
`beanFactory` identityHashes, second one uncustomized). **Still fully OPEN**
-- the next step requires either instrumenting `DefaultContextCache.
contextMap`'s actual `get`/`put` calls directly (risky: needs a temporary
patch to the shared `spring-framework-recheck` checkout, or a
classpath-shadowing private copy of `spring-test`) or Rust-level
`CRATONVM_DBG_LOADER_TRACE`/`DBG_DUPCLASS_BT` tracing specifically checking
whether `DefaultContextCache`'s own class (and hence its static
`defaultContextCache` singleton) is itself duplicated, which was never
directly checked (only the delegate INSTANCE's stability was, which is
confirmed but doesn't rule this out). See
`[[aot-double-context-refresh-not-junit-store-not-classid]]` (memory) for
the full diagnostic-by-diagnostic writeup and reusable probe file locations.

**`ApplicationContextAotGeneratorTests` re-verified with `CRATONVM_JAVA_HOME`
correctly set: 33/40 (unchanged from the documented `--nojit` baseline)** --
the missing-env-var hypothesis does NOT explain this class's residuals; its
official-runner count was already accurate. The 5 confirmed failures are all
the already-documented CGLIB-proxy/duplicate-`ClassId` family
(`processAheadOfTimeWithExplicitResolvableType` `AotBeanProcessingException`,
`...WhenHasCglibProxyWriteProxyAndGenerateReflectionHints`,
`...WhenHasCglibProxyUseProxy`, `...UsesCglibClassForFactoryMethod`
`CompilationException`, `...WhenHasCglibProxyWithAnnotationsOnTheUserClasConstructor`
`CompilationException`) from the "2026-07-21 late session" unifying
root-cause finding above (still NOT fixed, needs a dedicated session per that
write-up). **Good news**: `processAheadOfTimeWhenHasCglibProxyAndMixedAutowiring`
(the `@Value` field-injection gap this doc's follow-up-2 section suspected
might share the double-refresh mechanism) is **no longer failing** -- it
passed in this re-run, so that specific residual is resolved (by one of
today's earlier loader-identity fixes, not cross-checked which one
specifically) and the "plausibly the SAME mechanism -- not yet cross-checked"
question is moot.

`test.context.aot.TestContextAotGeneratorIntegrationTests` re-baseline is
**still blocked, but now by a newly-confirmed-reproducible (2/2) hang/crash,
not by the missing-JAVA_HOME environment issue**: both this session's attempts
(one under heavy host load, `uptime` ~20+, one after load dropped to ~2-11)
died identically, ~1-2 minutes into the run, DURING JUnit test *discovery*
(stack bottoms out at `KRunMethod`/`KRun2.main`'s `LauncherFactory.create().
execute(...)` call, before any `@Test` method actually starts) with:
```
<clinit> failed — wrapping in ExceptionInInitializerError class=groovy/lang/GroovySystem
  cause=java/lang/NullPointerException Cannot read the array length because "<local2>" is null
```
No further output, no clean Java stack trace, no `[cratonvm] main-vm run()
returned Err` exit line either -- the process goes silent and is eventually
killed by the harness's `timeout` wrapper, consistent with a silent hang
rather than a caught-and-reported exception. Not yet root-caused (would need
`CRATONVM_DBG_ATHROW=1` or similar to find why the wrapped
`ExceptionInInitializerError` doesn't propagate/get caught normally, and why
GroovySystem's clinit is reached at all during plain JUnit discovery of a
Spring Framework test class with no declared Groovy dependency). Flag as a
NEW, distinct blocker for whoever continues -- do not conflate with the
already-closed loader-identity family above.

## 2026-07-23 AOT follow-up 5 -- double-context-refresh ROOT-CAUSED AND FIXED

Same session as follow-up 4, continuing directly from its refuted
hypotheses. Worktree `/data/wt-aot-junitstore-20260723` (branch
`fix/aot-junitstore-20260723`). No subagents used, per this task's standing
instruction.

**Still open / not done this session**: re-running the FULL
`AotIntegrationTests#endToEndTestsForBeanOverrides` 175-test suite (only the
fast single-fixture repro was verified, not the full suite -- the mechanism
is proven fixed but the aggregate pass count wasn't re-measured, needs a
300-400s+ run); `ApplicationContextAotGeneratorTests`'s remaining 5
CGLIB/duplicate-`ClassId` failures (confirmed unrelated to this fix, same
"2026-07-21 late session" family, a different native/bytecode call site);
`TestContextAotGeneratorIntegrationTests`'s `GroovySystem` clinit hang
(unrelated, not root-caused); and a worthwhile follow-up audit of whether
OTHER native trampolines that call `ctx.invoke_special`/`ctx.invoke` by name
on Spring TestContext Framework classes have the same loader-blindness (this
fix only touches the one call site that was actually proven to matter here).
See `[[aot-double-refresh-springextension-loader-blind-fix]]` (memory) for
the full diagnostic-by-diagnostic writeup.

## 2026-07-23 AOT follow-up 6 -- second loader-blindness call site fixed, follow-up 5's own next-steps resolved

Same worktree/branch as follow-up 5 (`/data/wt-aot-junitstore-20260723`,
`fix/aot-junitstore-20260723`). No subagents used.

**Full 175-test `endToEndTestsForBeanOverrides` re-verified: 73/175 ->
~158/175.** Ran the actual `KRunMethod ... AotIntegrationTests
endToEndTestsForBeanOverrides` single-method probe (not just the fast
`BeanOverrideProbe2` fixture) with both fixes applied. Result:
`MultipleFailuresError: Test execution failures (17 failures)` -- down from
the documented 102. The remaining 17 cluster into two DISTINCT, UNRELATED
families, neither a loader-identity issue:
- The majority: `@MockitoBean`/`@MockitoSpyBean` "by name" lookup for
  CONSTRUCTOR-injected parameters (`MockitoBeanByNameLookupForConstructorParametersIntegrationTests`,
  `MockitoSpyBeanByNameLookupForConstructorParametersIntegrationTests`,
  `MockitoBeansByNameIntegrationTests`) fail with `No qualifying bean...
  expected single matching bean but found N`, where the listed candidate
  names show the override bean sitting ALONGSIDE the original(s) it should
  have replaced -- a bean-override-not-replacing-original bug specific to
  constructor injection, not investigated further.
- A smaller family: plain `AssertionFailedError: expected: null but was:
  ""` -- not yet isolated to a specific test class.
  See `[[aot-endtoend-beanoverrides-73-to-158-of-175]]` (memory) for the full
  breakdown, including which log4j-noise lines to ignore.

**`ApplicationContextAotGeneratorTests`'s residuals RE-CHARACTERIZED -- NOT
the duplicate-ClassId/CGLIB-naming family this doc's "2026-07-21 late
session" assumed.** Re-examined the actual exception text (not just the
class/method names) for the 2 `CompilationException` failures
(`processAheadOfTimeUsesCglibClassForFactoryMethod`,
`...WithAnnotationsOnTheUserClasConstructor`). Both are REAL JAVAC (`com.
sun.tools.javac.jvm.ClassReader`'s own diagnostic format) reporting
`org.springframework.beans.factory.aot.AutowiredArguments` as a "bad class
file... truncated" -- but this javac instance runs AS INTERPRETED BYTECODE
INSIDE CratonVM (`TestCompiler.forSystem()`'s in-process compile), so every
file read it performs goes through CratonVM's own native I/O. Two
independent tests ruled out "the jar is just corrupted": the existing jar
entry passes external Python `zipfile` validation (correct declared size,
no CRC error) AND a from-scratch recompile with real JDK 25's own `javac`
(prepended to the classpath, confirmed via the changed byte offset in the
error message that THIS file was actually being read) is STILL reported
"truncated," this time at ITS OWN different real size. Truncated-at-exactly-
its-own-real-length, reproduced across two differently-sized files compiled
from the same source, points at a **classfile `TypeAnnotations`-attribute
parsing bug in CratonVM's own reader** rather than file corruption:
`AutowiredArguments` is a `@FunctionalInterface` with JSpecify `@Nullable`
TYPE_USE annotations on a generic method return type and an array return
type -- exactly the structurally-complex `type_path` cases this attribute
exists to encode. Possibly a not-yet-covered edge case of the existing
JSpecify TYPE_USE fix family. NOT fixed this session (core classloading/
reader-crate work, out of scope for the narrowly-scoped native-trampoline
fixes landed today) -- see
`[[aot-cglib-residuals-not-classid-its-typeannotations-classfile-bug]]`
(memory) for the full evidence chain and recommended isolated repro.
`processAheadOfTimeWithExplicitResolvableType`'s `AotBeanProcessingException`
and the two `AssertionError`/`AssertionFailedError` failures for this class
remain uncharacterized.

**`TestContextAotGeneratorIntegrationTests`'s hang: genuine slowness inside
Groovy's own runtime bootstrap, NOT conclusively a deadlock -- host
contention is a major unresolved confound.** `--stack-dump-on-timeout`
(a real CratonVM flag) caught the main thread genuinely busy inside real
javac's `Attr`/`DeferredAttr` in one run, and inside `org/codehaus/groovy/
reflection/stdclasses/CachedSAMClass.hasUsableImplementation` <-
`CompileWithForkedClassLoaderClassLoader.loadClass` in another, with
`CRATONVM_DBG_ATHROW=1` additionally showing the SAME thread had recently
been deep in Groovy's own `MetaClassRegistryImpl.<init>` -> `registerMethods`
-- eagerly registering every "Default Groovy Method," a well-known-heavy,
one-time Groovy runtime bootstrap step, not a lock-wait. However, a full
30-minute run (`timeout 1800`) under severe host load (`uptime` load
average 19-23, 40+ concurrent users) still never completed, frozen at the
exact same point every time. This is NOT yet conclusively distinguished
from "just extremely slow under this much contention" -- needs a re-test
on a quiet host (load average < 2) before concluding either way; if it
still doesn't finish in ~10x the normal ~490s AOT-suite runtime there,
that's real evidence of a genuine hang. See the updated
`[[testcontextaotgeneratorintegrationtests-groovysystem-clinit-hang]]`
(memory).

## 2026-07-23 AOT follow-up 7 -- MockitoBean constructor-param native shortcut restored, sixth JIT residual (ClassReader.readInnerClasses) root-caused and fixed

Worktree `/data/wt-aot-final-close-20260723` (branch `fix/aot-final-close-20260723`,
from `origin/dev` `893ddbc73`). No subagents used, per this task's standing
instruction. Pushed to `origin/dev` at `9fb1b8749`.

**Still open for whoever continues** (see `[[aot-endtoend-beanoverrides-73-to-158-of-175]]`
and this doc's earlier sections for full context):
1. "Family B" of the `endToEndTestsForBeanOverrides` 175-test suite (a
   smaller cluster of plain `AssertionFailedError: expected: null but was:
   ""`) -- still not isolated to specific test classes; the full 175-test
   aggregate run needs several GB of heap (`--Xmx 4g` recommended) and
   repeatedly hit host-level OOM/kill under this session's shared-host
   contention, so a clean full-suite count was not obtained this session.
2. `ApplicationContextAotGeneratorTests`'s remaining 6 residuals (listed
   above) -- 5 distinct, uncharacterized failure modes plus the
   already-known CGLIB duplicate-`ClassId` one.
3. `TestContextAotGeneratorIntegrationTests`'s `GroovySystem` hang/slowness
   -- still needs a re-test on a genuinely quiet host (load average < 2) to
   distinguish real hang from host-contention artifact; this session's host
   oscillated between load 1 and load 100+ repeatedly and was never quiet
   for long enough to attempt it.

## 2026-07-24 AOT follow-up 8 -- three more residuals closed (34/40 -> 36/40), two CGLIB cross-test failures root-caused but not yet fixed, new lambda-singleton gap found

Worktree `/data/wt-aot-residuals3-20260723` (branch
`fix/aot-residuals3-20260723`, from `origin/dev` `1dae989b1`, merged with
`origin/dev` `d58e8ea1e` mid-session with no conflicts). No subagents used,
per this task's standing instruction. Pushed directly to `origin/dev` at
`ce0804f81` (fast-forward, `d58e8ea1e..ce0804f81`) — the shared main
checkout at `/data/data/cratonvm` had uncommitted changes belonging to
another concurrent session, so the merge-and-push was done entirely from
this session's own worktree instead of touching the shared checkout.

### Root-caused but NOT fixed: two CGLIB cross-test residuals, confirmed test-order-dependent

`processAheadOfTimeUsesCglibClassForFactoryMethod` ("`IllegalArgumentException:
class ... is not an enhanced class`") and `processAheadOfTimeWhenHasCglibProxyUseProxy`
("Hello1" instead of "Hello0" -- `CglibConfiguration.prefix()`'s body
running twice) were BOTH independently confirmed, via repeated isolated
`KRunMethod` runs, to **pass 100% reliably every time in isolation** but
**fail 100% deterministically** whenever run as part of the full
40-method `ApplicationContextAotGeneratorTests` class run (verified twice,
identical failure set both times -- not flaky/host-load-dependent).

Initial hypothesis: `config_enhancer_class_cache` (the cache added in an
earlier session so a repeat `enhance()` call for the SAME `@Configuration`
class returns the SAME `Class`, matching real CGLIB's own
`AbstractClassGenerator` caching) was keyed by the bare, **recyclable**
`ClassId` alone rather than `(defining_loader_id, class_name)` -- flagged
as a known gap in an earlier session's `ConfigurationClassEnhancerTests
.withPublicClass` note, and matching the sibling `config_enhancer_counters`
cache's own already-fixed key shape. **Fixed this** (now keyed by
`(loader_id, super_internal_name)`, mirroring `config_enhancer_counters`)
as a genuine, independent correctness improvement -- but empirically, via
a `CRATONVM_DBG_CCECACHE`-gated trace (kept in the code, see
`cce_enhance`), this did **not** turn out to be what's happening here:
both tests, run back-to-back in EITHER order via a minimal custom 2-method
JUnit launcher (`KRun2Methods.java`), showed the SECOND call reusing the
FIRST call's cache entry with the **exact same** `super_class_id` AND
`loader_id` both times -- i.e. `CglibConfiguration` genuinely is loaded
via the SAME stable loader across these nested test methods (contradicting
an initial assumption, checked with a minimal classloader-only repro, that
`@CompileWithForkedClassLoader` gives every test method method a fully
independent copy of every referenced class -- it does NOT for
`testFixtures` classes reached only by name, only for the outer test class
itself and anything the injected `classResourceLookup` covers). The cached
bytes themselves were independently confirmed correct (a `CBProbe.java`
probe directly enhancing `CglibConfiguration` and calling
`Enhancer.registerStaticCallbacks` on the result succeeded, setter method
present and reflectively found) -- so a cache HIT returning them should be
harmless. The actual mechanism was not further isolated this session:
attempts to trace deeper (running the two tests back-to-back with
`CRATONVM_DBG_CCECACHE=1`) repeatedly hit multi-minute delays around
Hibernate Validator's `ResourceBundleMessageInterpolator`/EL-processor
one-time initialization under host contention, consuming the remaining
investigation budget without a clean trace. **Next step for whoever
continues**: bypass the JUnit/Spring-context-refresh machinery entirely
(a raw Java program that directly exercises `ConfigurationClassPostProcessor`
+ `ApplicationContextAotGenerator.processAheadOfTime` twice in one process,
  skipping anything that would trigger Hibernate Validator) to get a clean
  multi-minute-hang-free trace of what differs between the cached-hit
  `Class` mirror returned during AOT PROCESSING and whatever the REPLAY-time
  compiled/loaded class actually is.

### New finding, not fixed: non-capturing lambdas aren't cached as JVM singletons

`processAheadOfTimeWhenHasAutowiringOnUnresolvedGeneric` (confirmed to
fail 100% reliably even in ISOLATION, not cross-test) asserts that
`AutowiredGenericTemplate.genericTemplate` (autowired in a FRESH, AOT-
replayed context) is `.equals()` (== identity, no custom `equals()`) to
`applicationContext.getBean("genericTemplate")` from the ORIGINAL context
used for AOT processing -- both ultimately backed by the exact same
`v -> {}` non-capturing lambda expression in `GenericTemplateConfiguration
.genericTemplate()`. On real HotSpot this holds because
`InnerClassLambdaMetafactory` special-cases a lambda with ZERO captured
arguments: it emits a single cached `private static final INSTANCE` field
on the spun-up hidden lambda class and every invocation of the factory
method just returns that same field, rather than allocating a fresh
instance -- a real, load-bearing JDK optimization, not just an incidental
detail. CratonVM's own lambda implementation (`vm/src/runtime/
invokedynamic.rs`'s `allocate_lambda_proxy`, called from both the fresh-
link and cached-call-site paths) unconditionally allocates a brand new
heap object on every invocation regardless of capture count, so two
separate invocations of the same non-capturing lambda expression produce
two non-identical (and non-`.equals()`) objects on this VM. **Not fixed
this session** -- implementing the singleton-per-zero-capture-callsite
cache correctly needs a GC-safe long-lived `ObjectRef` cache (precedent
exists: `native-builtins/src/lang_math.rs`'s `integer_cache` +
`gc_scan_value_of_cache_roots`/its remap counterpart for `Integer.valueOf`'s
`-128..127` boxed cache), keyed by something that survives class/loader
GC without recycling-related aliasing (the exact same recyclable-`ClassId`
concern as the CGLIB cache above) -- flagged as a real, understood, but
nontrivial VM-level gap for a dedicated follow-up, not attempted given
this session's remaining time budget.

### Not attempted this session (time budget)

Family B of `endToEndTestsForBeanOverrides` and the `TestContextAotGeneratorIntegrationTests`
`GroovySystem` quiet-host recheck (both already flagged above) were not
revisited this session either -- the host was never quiet, and this
session's remaining time went to the CGLIB cross-test investigation above
instead.

Verified: `cargo test -p cratonvm-native-builtins --lib` (3078 passed / 1
pre-existing unrelated failure -- `regex_lookbehind_tests::pem_block_to_der_roundtrip`,
crypto/PEM code untouched by this session -- / 6 ignored), `cargo test -p
cratonvm-vm --lib --release` both before merging `origin/dev` (2230
passed / 13 pre-existing `lock_order`/`skip_list`/`tomcat_scanner`
baseline failures) and after (2233 passed / 10 of the same family --
3 of the 13 were independently fixed by concurrent upstream work merged
in mid-session) -- zero regressions introduced by this session's 3 fixes
at any point. `ApplicationContextAotGeneratorTests` full class:
34/40 -> 36/40.

## 2026-07-24 AOT follow-up 9 -- non-capturing lambda singleton cache fixed (36/40 -> 38/40), CGLIB cross-test residual narrowed but not fixed

Worktree `/data/wt-aot-followup9-20260724` (branch `fix/aot-followup9-20260724`,
from `origin/dev` `93ec6a810`). No subagents used, per this task's standing
instruction.

### CGLIB cross-test residual: two more hypotheses ruled out, real mechanism narrowed

Continued follow-up 8's "next step" (bypass JUnit/Spring-context-refresh
machinery for a clean repro). Two hypotheses that looked highly plausible
going in were both **refuted** with direct evidence:

1. **Loader-id recycling.** Checked `ClassManager`'s own doc comment
   (`classloading/src/class_manager.rs` around `user_loaders`): "the set
   only grows because class-loader unloading is not implemented in this
   VM." Confirmed at the allocator itself
   (`vm/src/vm/vm_exec.rs::allocate_loader_id`): a plain
   `AtomicU32::fetch_add`, starting at 3, never recycled. Two different
   `new CompileWithForkedClassLoaderClassLoader(...)` instances (Spring's
   real per-`@Test`-method forking mechanism, `spring-core-test.jar`)
   genuinely get distinct, monotonically-increasing loader ids -- ruled
   out as the cause.
2. **The forked-classloader mechanism itself being loader-blind for
   testfixture classes reached only by name** (follow-up 8's tentative
   read of its own `CRATONVM_DBG_CCECACHE` trace). Built two
   Spring-free/CGLIB-free repros using Spring's REAL
   `CompileWithForkedClassLoaderClassLoader`/`CompileWithForkedClassLoader`
   classes (already on the classpath, `org.springframework.core.test.tools`)
   against a plain fixture class with a static `AtomicInteger` counter --
   one flat (`ProbeForkTest`, two top-level `@Test` methods), one nested
   with the counter-touching call routed through a private helper method
   on the OUTER class (`OuterProbeTest.Nested2`, matching
   `ApplicationContextAotGeneratorTests.ConfigurationClassCglibProxy`'s own
   shape exactly). **Both reproduced correctly on CratonVM**: fresh counter
   (`==1`) and a fresh, distinct defining loader on every forked
   invocation, matching real HotSpot's behavior byte-for-byte. This rules
   out "the generic forking mechanism is broken" as the cause -- whatever
   is happening is specific to the CGLIB/`ConfigurationClassPostProcessor`
   path, not to `@CompileWithForkedClassLoader` in general.

**New finding, narrows the real mechanism**: re-ran the full 40-method
class with `CRATONVM_DBG_CCECACHE=1`. Across the whole run,
`config_enhancer_class_cache`'s lookup key
(`(super_loader_id, super_class_name)`) is correctly **unique per test
method** -- 9 distinct loader ids observed for 9 CGLIB-using test methods,
confirming (again) no loader-id aliasing across tests. But **within the
two failing tests' own single loader scope**, `ConfigurationClassEnhancer
.enhance()` is called **twice** for the identical `(loader_id,
"CglibConfiguration")` key, and **both calls report a cache MISS** --
which is itself the anomaly: a same-loader repeat call should hit the
cache the second time (real cglib's own `AbstractClassGenerator` caching
guarantees the same generated `Class` on a repeat `enhance()` call for the
same superclass+loader, which is exactly what `config_enhancer_class_cache`
was added to emulate). A double-miss means two DIFFERENT `ClassId`s get
minted for what should be "the same" enhancer class within one test's own
scope -- if the AOT-generated source (compiled once, referencing
whichever `ClassId` was live when generation ran) and the actual
CGLIB-marker/dispatch check at replay time (whichever `ClassId` a second,
missed-cache `enhance()` call produced) end up disagreeing about which of
the two is canonical, that would explain both symptoms directly: "not an
enhanced class" (an identity/marker check against the wrong `ClassId`) and
"Hello1" instead of "Hello0" (the underlying real `CglibConfiguration`'s
static `AtomicInteger` counter genuinely getting exercised twice within
one test run, once per enhance() attempt, if scanning/building a second
enhancer subclass also touches the superclass's own state).

Two tests with a bare `CglibConfiguration` (not one of the `Value/
Autowired/Configurable` variants) are exactly `processAheadOfTime
UsesCglibClassForFactoryMethod` and `processAheadOfTimeWhenHasCglibProxy
UseProxy` -- i.e. this double-miss-within-one-test pattern maps 1:1 onto
the two actually-failing tests, and does NOT occur for any of the other
CGLIB-using tests in the class (which call `enhance()` either once, or
twice with the second call correctly hitting the cache). **Not yet
explained**: why `testCompiledResult`'s generation+replay flow calls
`enhance()` twice specifically for these two tests and not the others
(`processAheadOfTimeWhenHasCglibProxyAndAutowiring`/`AndMixedAutowiring`/
`WithArgumentsUseProxy`, all similarly `testCompiledResult`-based, showed
a clean single miss + single hit in the same trace), nor why the SECOND
call misses instead of hitting given an apparently-identical cache key.
**Next step for whoever continues**: instrument `cce_enhance` with a
cache-size print immediately before/after both the lookup and the insert
(not just hit/miss) to rule out reentrancy (the second call happening
before the first's insert completes, e.g. via a recursive trigger during
`scan_bean_methods`/class initialization) versus the key itself somehow
differing between the two calls despite printing identically in the
existing trace; a raw (non-JUnit, non-Spring-context) driver that calls
`ConfigurationClassPostProcessor`/`ApplicationContextAotGenerator
.processAheadOfTime` directly, twice, in one process for
ONLY these two specific config classes would isolate this far faster than
the ~5-minute full-class run this session used.

### Also found, out of scope, flagged separately

`cargo test -p cratonvm-native-builtins --lib` surfaced a NEW failure not
present in follow-up 8's baseline:
`cglib_enhancer::fb_ref_bytecode_tests::fb_ref_splice_shifts_exception_table_by_exactly_8_bytes`
(`left: 281 right: 161` at `cglib_enhancer.rs:4668`, deterministic in
isolation). Confirmed pre-existing on `origin/dev` (this session never
touched `cglib_enhancer.rs`) -- most likely `86b638d65`
("parameterized @Bean methods" / `@Lookup` fixes) changed
`emit_bean_override`'s generated bytecode shape without updating this
test's hardcoded byte-length constants. Flagged via a spawned background
task rather than fixed here to stay in scope.

### Not attempted this session (time budget)

Family B of `endToEndTestsForBeanOverrides` and the
`TestContextAotGeneratorIntegrationTests` `GroovySystem` quiet-host
recheck (both flagged since follow-up 7/8) were not revisited this session
either -- the CGLIB cross-test investigation above consumed the bulk of
this session's time budget.

## 2026-07-24 AOT follow-up 9b -- Family B investigation blocked by two newly-found regressions (neither fixed), both characterized with repros

Continuation of follow-up 9's session (same worktree, `/data/wt-aot-followup9-20260724`).
Attempted to isolate "Family B" of `endToEndTestsForBeanOverrides` (the
plain `AssertionFailedError: expected: null but was: ""` failures flagged
since [[aot-endtoend-beanoverrides-73-to-158-of-175]] and never isolated to
specific test classes). Did not reach it -- hit two separate, apparently-NEW
blocking issues in sequence, neither present when the prior session
(2026-07-23) got a clean ~158/175 baseline on the exact same test method.
Both are real VM bugs, fully characterized with standalone repro commands,
but not fixed this session (time budget).

### Blocker 1: `Package.getPackageInfo()` NPE during TestNG-engine classpath discovery

Running `AotIntegrationTests` (whole class, or just `endToEndTestsForBeanOverrides`
via `MethodRun`) now fails immediately (~22s) with:

```
org.junit.platform.commons.JUnitException: TestEngine with ID 'testng' failed to discover tests
Caused by: java.lang.NullPointerException: Cannot invoke "java.lang.Module.getClassLoader()" because "module" is null
    java.lang.Package.getPackageInfo(Package.java:417)
    java.lang.Package.getAnnotation(Package.java:446)
    org.testng.internal.annotations.IgnoreListener.findAnnotation(...)
    ...
    org.springframework.test.context.aot.TestClassScanner.scan(TestClassScanner.java:156)
```

`TestClassScanner.scan()` (spring-core-test) uses a plain `LauncherFactory
.create()` (all registered engines auto-discovered via ServiceLoader,
including `org.junit.support.testng`'s TestNG-compat engine) with
`selectClasspathRoots(...)` -- meaning the TestNG engine's own
`TestNGClassFinder` walks (a large slice of) the classpath root looking for
TestNG-style classes, and NPEs on the FIRST class whose `Class.getPackage()`
returns a `Package` object with a null `module` field (real JDK requires
the `module` field to always be at least the defining loader's unnamed
module, never null).

**Isolation attempts, both inconclusive** (2 standalone, Spring-free
repros): a plain `-cp`-loaded class's `getPackage()` correctly returns a
non-null module on CratonVM (though the module's OWN `.getClassLoader()`
already differs from real JDK -- see below); adding a `package-info.java`
with a runtime-retained annotation to force the real `getPackageInfo()`
path (`Class.forName(module, pkg + ".package-info")`) still worked fine.
**Neither reproduces the NPE** -- whatever class/loader combination TestNG's
crawler hits during Spring's classpath-root scan produces a `Package` with
a genuinely null `module` field, and it wasn't isolated to a specific class
this session (the TestNG engine's own class-by-class walk order wasn't
traced). A secondary, likely-related but non-crashing gap found along the
way: `Class.getModule().getClassLoader()` returns `null` on CratonVM for an
ordinary `-cp`-loaded class's unnamed module, where real JDK returns the
actual `AppClassLoader` -- this alone doesn't crash anything found this
session, but is almost certainly the same underlying gap (module objects
not fully wired to their defining loader) just not always fatal.

**Workaround used to unblock further investigation** (not a fix): strip
`testng-engine-*.jar` and `testng-*.jar` from the classpath entirely before
invoking `AotIntegrationTests` --
`cat cratonvm-testcp.txt | tr ':' '\n' | grep -v -i testng | tr '\n' ':'`
-- since `TestClassScanner` only needs the JUnit Jupiter engine for Spring's
own AOT integration tests; removing the optional TestNG engine sidesteps
the crash entirely and lets discovery proceed.

**Next step for whoever continues**: instrument `TestClassScanner.scan()`
(or run with a debug agent) to print the exact class name being examined
when the NPE fires, then trace how THAT class's `Package` object got built
with a null `module` -- likely in `vm/src/native` wherever `Class
.getPackage()`/`ClassLoader.definePackage`-equivalent synthetic Package
construction happens, check whether it's conditioned on the loader type
(bootstrap/app/user-defined) and misses a case.

### Blocker 2: `Class.getDeclaredMethods()` NoSuchMethodError inside a dynamically-defined (non-file) CGLIB class's `<clinit>` -- confirmed a NEW regression

With Blocker 1 worked around, `endToEndTestsForBeanOverrides` actually ran
(~70s) and reached real AOT PROCESSING -- but `runEndToEndTests(testClasses,
true)` uses `failOnError=true`, so it stops at the FIRST test class whose
AOT generation fails, rather than aggregating all 175 like the final
(replay-phase) failure list follow-up 7/8's sessions saw. First (alphabetical
discovery order) failure:

```
TestContextAotException: Failed to generate AOT artifacts for test classes
  [...MockitoSpyBeanAndCircularDependenciesWithLazyResolutionProxyIntegrationTests]
Caused by: AotBeanProcessingException: Error processing bean ...$One
Caused by: AopConfigException: Unexpected AOP exception
Caused by: IllegalStateException: Unable to load cache item
  org.springframework.cglib.core.internal.LoadingCache.createEntry
  org.springframework.cglib.core.AbstractClassGenerator.create
  org.springframework.aop.framework.ObjenesisCglibAopProxy.createProxyClass
Caused by: java.lang.NoSuchMethodError: java.lang.Class.getDeclaredMethods()[Ljava/lang/reflect/Method;
  ...MockitoSpyBeanAndCircularDependenciesWithLazyResolutionProxyIntegrationTests$Two$$SpringCGLIB$$0.CGLIB$STATICHOOK1(<generated>)
  ...MockitoSpyBeanAndCircularDependenciesWithLazyResolutionProxyIntegrationTests$Two$$SpringCGLIB$$0.<clinit>(<generated>)
  org.springframework.cglib.core.ReflectUtils.defineClass(ReflectUtils.java:581)
```

Important: this is **real, bytecode-generated cglib** (`org.springframework
.cglib.core.ReflectUtils.defineClass` / `Enhancer.generate()`, Spring's
repackaged-cglib for `CglibAopProxy` AOP proxies via `ContextAnnotation
AutowireCandidateResolver.buildLazyResolutionProxy` -- a "lazy resolution
proxy" for a circular-dependency `@Autowired` field), NOT the native
`cce_enhance`/`ConfigurationClassEnhancer.enhance()` override this whole
CGLIB-cluster investigation (follow-up 8/9) has otherwise been chasing --
i.e. this is a THIRD, distinct CGLIB code path in CratonVM (native
`ConfigurationClassEnhancer` override, native `FastClass` placeholder, and
now real-bytecode-executed `cglib.core`/`Enhancer` proxy generation all
exist independently). `CGLIB$STATICHOOK1` is cglib's universal
per-generated-class static initializer that populates its internal
method-interception tables by reflectively calling `Class
.getDeclaredMethods()` on itself -- a completely standard, ubiquitous cglib
pattern, so this is NOT specific to lazy-resolution proxies; any real-cglib
(non-Spring-Configuration) proxy generation likely hits the same wall.

`Class.getDeclaredMethods()` IS registered as an unconditional native
override for `java/lang/Class` (`native-builtins/src/lib.rs`, delegates to
`lang_class::native_class_get_declared_methods`) -- the `NoSuchMethodError`
is therefore NOT a missing-registration gap but a method-RESOLUTION failure
specific to this receiver, most likely something about how CratonVM
resolves a `Methodref` constant-pool entry against `java/lang/Class` from
WITHIN a dynamically-`defineClass`'d (not loaded from a `.class` file on
disk) class's own bytecode -- not investigated further this session.

**Confirmed to be a genuine regression, not a pre-existing gap**: follow-up
7's own memory note ([[aot-endtoend-beanoverrides-73-to-158-of-175]])
recorded a clean **158/175** pass on this EXACT test method
(`endToEndTestsForBeanOverrides`, same `failOnError=true`) on 2026-07-23 --
if this CGLIB-proxy class already failed AOT processing then, `failOnError
=true` would have aborted immediately with 1/175, not proceeded to
aggregate 17 replay-phase failures. Something merged into `origin/dev`
between that session and this one (31+ commits of drift on top of this
session's own base) broke real-bytecode cglib class generation's
`Class.getDeclaredMethods()` resolution. Bisecting the responsible commit
was not attempted this session.

**Next step for whoever continues**: (1) bisect `origin/dev` between the
2026-07-23 session's commit and now for whatever changed
`Class.getDeclaredMethods` resolution, dynamic-`defineClass` constant-pool
resolution, or cglib-adjacent native code -- likely a much smaller, faster
fix than re-deriving root cause from scratch; (2) once found/fixed, rerun
`endToEndTestsForBeanOverrides` (classpath with `testng` jars stripped per
Blocker 1's workaround, `--Xmx 4g`, expect several more classes to hit
similar or different failures before Family B's null-vs-"" symptom
actually surfaces -- budget for multiple iterations); (3) Blocker 1 (the
Package/Module NPE) should also be fixed independently since it silently
breaks ANY test suite that includes the TestNG JUnit-Platform engine on the
classpath, a much broader blast radius than just this one Spring test.

Family B itself remains completely unisolated -- zero net progress on the
original goal this session, but two real, previously-unknown-to-this-suite
bugs found and characterized instead.

## 2026-07-24 AOT follow-up 9c -- TestContextAotGeneratorIntegrationTests "GroovySystem hang" REFUTED as Groovy-related AND as JIT-related; real hang site narrowed to Spliterators.spliterator()/ArraySpliterator construction; one real (but insufficient) bug fixed along the way

Re-ran `TestContextAotGeneratorIntegrationTests` on a genuinely quiet host
(`uptime` load average ~0.4-1.6, the condition
[[testcontextaotgeneratorintegrationtests-groovysystem-clinit-hang]] said
was needed to distinguish a real hang from host-contention artifact),
`--stack-dump-on-timeout 600`. **Both of that memory's live hypotheses are
now refuted.**

### Refuted: "genuine (if extreme) Groovy MetaClassRegistryImpl bootstrap slowness"

CratonVM's own T19.H1 native-hang watchdog fired at the 600s mark and
self-aborted with a full diagnostic dump. No `GroovySystem`/Groovy anywhere
in the dump. The watchdog's thread summary and per-thread **native-call
ring buffer** (64 entries, oldest first) instead show the main thread
`STILL-IN-NATIVE(550000ms+ ago)` inside `java/util/Arrays.stream(
[Ljava/lang/Object;)Ljava/util/stream/Stream;`, reached from
`org/springframework/aot/generate/AccessControl.lowest([...])` (real
source: `Arrays.stream(candidates).map(AccessControl::getVisibility)
.toArray(Visibility[]::new)`), itself reached via `DefaultListableBeanFactory
.findAllAnnotationsOnBean` <- `RuntimeHintsBeanFactoryInitializationAotProcessor
.extractFromBeanFactory/processAheadOfTime`. Reproduced identically twice
(both attempts hung at the exact same site, ruling out a one-off fluke).

**However: re-running the exact same hang scenario with the fix applied
reproduces the IDENTICAL hang, same site, same ~550s.** The fence-field
bug, while real, is not (solely) responsible for this hang.

### Refuted: JIT miscompilation

Given this codebase's extensive precedent of JIT-miscompile bugs in
adjacent areas (see `vm/src/jit/skip_list.rs`'s `ClassReaderReadClass`
family), re-ran with `--nojit` (interpreter-only). **The hang reproduces
identically** -- same site conceptually, though the EXACT call sequence at
the point of freezing differs slightly between JIT and no-JIT runs (see
below), ruling out a JIT-compiled-code-specific miscompilation as the
cause. This is a real interpreter/native-dispatch bug, not a codegen bug.

### Narrowed further: the no-JIT run reveals a DIFFERENT concrete call site than initially assumed

The `--nojit` run's dispatch trace, at the point of freezing, shows:

```
[...] BC  java/util/Collection.stream()Ljava/util/stream/Stream;
[...] NAT java/util/Spliterators.spliterator([Ljava/lang/Object;I)Ljava/util/Spliterator;  ->NATIVE java/util/Objects.requireNonNull(Ljava/lang/Object;)Ljava/lang/Object;
===== end dispatch_trace dump =====
```

This is a DIFFERENT path than `p59_collection_spliterator` (which this
session's fix targeted): `java.util.Spliterators.spliterator(Object[],
int)` is the real JDK STATIC factory method that `java.util.Arrays
$ArrayList.spliterator()` (the actual concrete class `Arrays.asList()`
returns on a real JDK -- a private nested class distinct from `java.util
.ArrayList`) calls, per its real source:
`return Spliterators.spliterator(a, Spliterator.ORDERED);`. Its own real
source is `return new Spliterators.ArraySpliterator<>(Objects
.requireNonNull(array), additionalCharacteristics);` -- i.e. it calls
`Objects.requireNonNull` (confirmed via code reading to be a trivial,
non-blocking native: `native_objects_require_non_null` in
`native-builtins/src/lib.rs`, cannot itself hang) and then constructs a
`java.util.Spliterators$ArraySpliterator` -- a REAL, bytecode-defined JDK
class, NOT one of CratonVM's synthetic objects.

**This means the actual hang is most likely inside either (a) object
allocation/constructor execution for `Spliterators$ArraySpliterator`
itself, or (b) whatever real bytecode runs immediately after
`Collection.stream()` returns this real Spliterator to `StreamSupport
.stream()` and onward through the `.map()`/`.toArray()` pipeline stages
`AccessControl.lowest` chains -- NOT inside any of the synthetic-object
native overrides this session inspected.** Given `Arrays.asList()`'s
native override (registered `"java/util/Arrays"`/`"asList"`) stamps its
returned synthetic object as plain `"java/util/ArrayList"` rather than
`"java/util/Arrays$ArrayList"`, there is likely a genuine class-identity
mismatch between what CratonVM's native `Arrays.asList` returns and what
real JDK bytecode (`Arrays$ArrayList.spliterator()`, only reachable if the
object's class resolves correctly to `Arrays$ArrayList`) expects to run --
worth checking whether method resolution for `spliterator()`/`stream()` on
this synthetic object is landing on the RIGHT declaring class consistently
across JIT vs no-JIT execution, since the two runs took visibly different
call paths to reach conceptually the same operation.

### Next step for whoever continues

1. Do NOT assume the fence-field fix (already landed) is sufficient --
   verify against a rebuild.
2. Attach a live debugger per the watchdog's own on-screen instructions
   (`gdb -p <pid>` within the 3s grace window after the dump; lower
   `--stack-dump-on-timeout` to trigger sooner for faster iteration) to
   get a REAL native backtrace of the frozen call -- this is now the only
   way to make further progress; static code reading has been pushed as
   far as it reasonably can without seeing the actual stuck frame.
3. Consider whether `Arrays.asList()`'s native override should stamp its
   result as `java/util/Arrays$ArrayList` (loading/using the REAL JDK
   class, if CratonVM's real-JDK mode can do so) rather than the
   synthetic `java/util/ArrayList`, to make `spliterator()`/`stream()`
   resolution match real JDK's actual dispatch (real `ArraySpliterator`
   construction) instead of landing inconsistently between a synthetic
   native override (JIT run) and real bytecode (no-JIT run) depending on
   execution mode -- this divergence is itself suspicious and worth
   investigating even independent of the hang.
4. A minimal standalone repro of the same `Arrays.stream(arr).map(...)
   .toArray(...)` shape (this session tried `String[3]` with `.map()`
   +`.toArray()`) does NOT reproduce in isolation, meaning some
   additional state (heap layout, prior GC activity, or receiver-class
   identity established only after thousands of prior class loads) is
   needed to trigger it -- a repro embedded inside (or immediately after)
   a real `RuntimeHintsBeanFactoryInitializationAotProcessor
   .extractFromBeanFactory` run, rather than a fresh-process synthetic
   test, is more likely to reproduce reliably.

This memory should be considered **superseded**:
[[testcontextaotgeneratorintegrationtests-groovysystem-clinit-hang]]'s
"most likely genuine (if extreme) Groovy bootstrap slowness" conclusion no
longer holds -- this session found zero Groovy involvement, ruled out JIT
miscompilation, and identified a specific (if not yet fully pinpointed)
real-JDK-class construction site as the actual hang.

## 2026-07-26 AOT follow-up 10 -- Blocker 2 FIXED, the CGLIB cross-test residual FIXED (40/40), a third blocker behind them FIXED; `endToEndTestsForBeanOverrides` runs all 175 tests again (13 failures, one family, fully characterised)

Worktree `/data/data/wt-testngresid-20260726` (branch `fix/aot-cluster-20260726`,
from `origin/dev` `95e4d9929`, with the concurrent session's Blocker-1 fix
`34201b21e` merged in). Azure host `20.83.144.174`, real JDK 25. No subagents,
per this task's standing instruction.

**Scoreboard**

| | before | after |
|---|---|---|
| `ApplicationContextAotGeneratorTests` | 38/40 | **40/40** |
| `AotIntegrationTests#endToEndTestsForBeanOverrides` | aborts on test class #1 (`failOnError=true`) | **runs all 175, 13 failures** |
| `test.context.testng.*` | 8x LOADERR | all discover and run (Blocker 1, concurrent session) |

### `endToEndTestsForBeanOverrides`: 175 tests, 13 failures, all one family

The run now completes (`ms=910642` under a load average of ~28; budget
generously). All 13 failures are Family A -- `@MockitoBean`/`@MockitoSpyBean`
**by-name** lookup for **constructor parameters** -- in exactly three classes:

| class | failures |
|---|---|
| `...mockito.constructor.MockitoBeanByNameLookupForConstructorParametersIntegrationTests` | 7 |
| `...mockito.constructor.MockitoSpyBeanByNameLookupForConstructorParametersIntegrationTests` | 5 |
| `...mockito.typelevel.MockitoBeansByNameIntegrationTests` | 1 |

All shaped like:

```
ParameterResolutionException: Failed to resolve parameter [... ExampleService service2]
in constructor [...]: No qualifying bean of type '...ExampleService' available:
expected single matching bean but found 4: s1,s2,s3,s4
```

**New and important**: all three classes pass **100% in normal (non-AOT) mode**
on this same binary (7/7, 5/5, 1/1). So this is not the field-vs-constructor
override bug follow-up 7 fixed -- it is specific to what AOT *replay* does with
a by-name override, i.e. the AOT-generated bean definitions do not carry the
name-based replacement, leaving all the original candidates in play for
by-type constructor autowiring. That is a far tighter starting point than the
"17 failures, two families, unisolated" this doc has carried since follow-up 7.

**Family B appears to be gone.** The `AssertionFailedError: expected: null but
was: ""` shape that motivated follow-ups 9b/9c does not occur anywhere in this
run (zero `AssertionFailedError`s of any kind). The most likely explanation is
Fix 1: a `String.hashCode()` that returns 0 for arbitrary strings will corrupt
any map keyed by String, and the AOT generator is full of them. Not proven --
recorded as an observation, to be reconfirmed on the next run.

### Still open in the AOT cluster

1. The 13 Family-A failures above (AOT-replay-only, three named classes).
2. `TestContextAotGeneratorIntegrationTests`' `Arrays.stream`/
   `Spliterators.spliterator` hang (follow-up 9c) -- not revisited this session;
   note that follow-up 9c's own runs predate Fix 1, and a corrupted
   `String.hashCode()` is a plausible contributor to a hang in
   `AccessControl.lowest`'s map-heavy call chain, so **re-measure before
   re-investigating**.
3. `cglib_enhancer::fb_ref_bytecode_tests::fb_ref_splice_shifts_exception_table_by_exactly_8_bytes`
   -- still failing, still pre-existing on `origin/dev` (confirmed again this
   session by running the same test against this file's pre-change contents),
   still just stale hardcoded byte-length constants.

## 2026-07-26 AOT follow-up 11 -- "Family A" root-caused and FIXED: `BeanOverrideUtils` was being asked about the wrong classloader's `@BeanOverride`

Same worktree/branch as follow-up 10. The 13 failures that follow-up 10 left --
`@MockitoBean`/`@MockitoSpyBean` **by-name** lookup for **constructor
parameters**, AOT-replay only -- are one bug, and it is a one-line class
resolution defect.

### The full 175-class run did NOT complete -- and that is not this fix

`endToEndTestsForBeanOverrides` itself was left running for over an hour without
reaching its summary (the first attempt heap-thrashed at `--Xmx 4g`: 4.0GB RSS
with `main-vm` pegged; restarted at `--Xmx 8g` it climbed steadily to 5.7GB,
still with no summary at 66 minutes, and was stopped). The pre-fix run of the
same method took ~15 minutes. **Do not read that as a regression from this
change** -- four A/B measurements, pre-fix binary vs post-fix binary on
identical inputs, say otherwise:

| workload (identical behaviour on both binaries) | pre-fix | post-fix |
|---|---|---|
| 1 neutral class | 17.3s | 18.9s |
| 2 neutral classes | 21.97s | 21.92s |
| 4 Mockito classes untouched by this fix | 23.12s | 23.91s |
| 6 non-Mockito classes | 30.5s | — |

Per-class cost is unchanged, and Mockito classes are not inherently slow either.
Whatever makes the 175-class run blow up lives in the AOT compile+replay phase
at full-suite scale and predates this session; the only contribution from this
fix is that 13 tests which used to abort instantly at parameter resolution now
genuinely build their contexts and create their mocks. `gdb` cannot be attached
on this host (`ptrace_scope`), and the phase is silent, so it was not narrowed
further -- **flagged as its own item, not as a blocker for the fix above.**

## 2026-07-26 (later) Blocker 2 is now UNREACHABLE: `AotIntegrationTests` hangs first, in Mockito advice on `AbstractStringBuilder.length()`

An attempt to fix Blocker 2 could not reach it. On `dev` `8819b8e4b`,
`AotIntegrationTests` no longer gets as far as the CGLIB `NoSuchMethodError` --
it **hangs** during AOT generation. A build from a few hours earlier (dev
around `a6f69c388`) ran the same test to completion and produced the
`NoSuchMethodError`, so this is a behaviour change on `dev` within that window,
not an artifact of the investigation: the hang reproduces on a **pristine**
`origin/dev` build with no instrumentation.

### The hang

`--stack-dump-on-timeout 900` on the pristine build, main thread, innermost
frames (full dump: 172 frames):

```
AotIntegrationTests.endToEndTestsForBeanOverrides -> runEndToEndTests
  -> TestCompiler.compile -> JavacTaskImpl.call -> JavaCompiler.parseFiles
  -> JavacParser.nextToken -> Scanner.nextToken -> JavaTokenizer.readToken
  -> JavaTokenizer.scanOperator
  -> java/lang/StringBuilder.length
  -> java/lang/AbstractStringBuilder.length
  -> org/mockito/internal/creation/bytebuddy/MockMethodAdvice.isMocked
  -> MockMethodAdvice.getSingletonMockInterceptor
  -> org/mockito/internal/util/concurrent/DetachedThreadLocal.get
  -> org/mockito/internal/util/concurrent/WeakConcurrentMap.get
  -> WeakConcurrentMap$LatentKey.hashCode
```

Mockito's inline mock-maker advice is installed on
`java.lang.AbstractStringBuilder.length()` and has not been removed, so EVERY
`StringBuilder.length()` call in the process now routes through a Mockito
`WeakConcurrentMap` lookup. javac's tokenizer calls it per token, which is why
this surfaces as an apparently dead hang rather than a slowdown: the AOT test
compiles the generated sources with the in-process javac.

This is the same family as
[[mockito-inline-redefine-leaks-to-unrelated-real-instances]] and the
`MockitoBean` / `AbstractStringBuilder.length` issue recorded as fixed on
2026-07-23 ([[mockitobean-length-abstractstringbuilder-fixed-20260723]]) -- a
recurrence or an adjacent leak, and it needs fixing before Blocker 2 is
reachable again.

### What was established about Blocker 2 itself

All from the last run that DID reach it. These narrow it considerably and
should not be re-derived:

1. **The generated class's constant pool is CORRECT.** Dumped the actual bytes
   with `cglib.debugLocation` and ran `javap -v`:
   `#191 = Utf8 ()[Ljava/lang/reflect/Method;` and
   `#193 = Methodref java/lang/Class.getDeclaredMethods:()[Ljava/lang/reflect/Method;`.
   So the malformed `()[Ljava/lang/reflect/Method[];` in the error is produced
   by CratonVM after parsing, not present in the input.
2. **That exact dumped class file loads and initializes fine on CratonVM** when
   placed on the classpath and `Class.forName`'d (`declaredMethods=49`,
   identical to HotSpot). So neither the bytes nor the class-file parser is at
   fault.
3. **Every runtime `defineClass` route is fine** for an equivalent hand-built
   class whose `<clinit>` calls `Class.getDeclaredMethods()`:
   `MethodHandles.Lookup.defineClass`, a user `ClassLoader.defineClass`, and one
   with the platform loader as parent all work -- including a variant that also
   forces a `CONSTANT_Class` for `[Ljava/lang/reflect/Method;` via
   checkcast/anewarray/ldc in the same class.
4. **The exact failing Spring path works standalone.** Driving
   `ProxyFactory.setProxyTargetClass(true).getProxy()` (i.e.
   `ObjenesisCglibAopProxy.createProxyClass`) on the real
   `...LazyResolutionProxyIntegrationTests$Two` and `$One` produces a working
   proxy with 49 declared methods, same as HotSpot.
5. **CratonVM's own native CGLIB enhancer is not involved**: the `[CCE] enhance:
   defined` log lines cover only `$Config$$SpringCGLIB$$0` classes; the failing
   `$Two$$SpringCGLIB$$0` is never among them.
6. The `NoSuchMethodError` does **not** come from `vm_exec.rs`'s dispatch site --
   `CRATONVM_DBG_NSME=1` produced 10808 `[NSME_DBG] native-probe` lines and not
   one of them mentions `getDeclaredMethods` or contains `[]`. Some other
   construction site is responsible; find it before instrumenting further.

So the defect is **context-dependent**, not a property of the bytes, the
parser, the define route, or the proxy path in isolation. The next step is
unchanged: fix the Mockito advice leak so the test runs again, then catch the
malformed descriptor at its construction site. A cheap probe that works: a
`descriptor.contains("[]")` guard plus `std::backtrace::Backtrace::force_capture()`
at each `LinkageError::NoSuchMethodError` construction -- but put it ONLY on
those cold paths. Putting it at `interpreter::execute`'s entry (a substring scan
on the hottest path in the VM) slows the run enough to look like a hang, and
`RUST_BACKTRACE=full` does the same by making every internal error capture a
backtrace.
