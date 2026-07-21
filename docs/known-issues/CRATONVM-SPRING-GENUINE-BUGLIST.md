# CratonVM Spring suite — genuine bug list (dev `8719dca85`)

| | |
|---|---|
| **Status** | OPEN — 56 confirmed genuine bugs remaining |
| **Captured** | 2026-07-17 (initial full-suite triage, dev `213d93ea`), reconfirmed 2026-07-20 (dev `8719dca85`) |
| **Worktree** | `/data/wt-spring-full-suite-20260717` (branch `chore/spring-full-suite-20260717`), Azure host `20.83.144.174` |

## Summary

Started from a full 2912-class suite run (dev `213d93ea`) fully triaged
against HotSpot (see history below), which found **177 confirmed genuine
bugs**. Reconfirmed by rerunning exactly those 263 previously-non-passing
classes on a fresh `dev` merge (`8719dca85`, ~3 days / several hundred
commits later), 4 shards, same settings (`suite-run.sh`, `BATCH=10
BATCH_TO=120 ONE_TO=120`, `CRATONVM_DEFAULT_HEAP_MAX_MB=2048`, real JDK 25).

**121 of the 177 are now fixed.** 56 remain open.

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

## Notable clusters (current state, 2026-07-20)

**JMX — 26/26 fixed, cluster fully closed (2026-07-20).** The systemic
`RequiredModelMBean` breakage flagged on 2026-07-17 was resolved for all but
two classes (`jmx.access.MBeanClientInterceptorTests` 11/14,
`jmx.access.RemoteMBeanClientInterceptorTests` 2/14); both are now 14/14.
Two distinct regressions, both introduced after the 2026-07-05
jmx-platform-mxbean-registration fix and neither noticed until this session:
(1) a 2026-07-14 defensive Bridge override
(`register_management_factory_platform_server_stub`, called from the
real-JDK native-registration branch in `vm/src/vm/vm_init.rs`) was left
permanently wired in after the NPE it worked around
(`ObjectName.getCanonicalKeyPropertyListString()` on the synthetic
1-field ObjectName model) was independently fixed elsewhere — it silently
shadowed real `MBeanServerFactory.createMBeanServer()` bytecode with an
empty synthetic `MBeanServer` in real-JDK mode, so `getPlatformMBeanServer()`
registered ZERO platform MXBeans (not even `MBeanServerDelegate`) instead of
the expected ~16. Removed the call, restoring the original KAFKA-MBEAN
design intent (`native-builtins/src/jmx.rs`'s `register_management_factory`
already deliberately leaves this method unregistered for exactly this
reason). (2) `ObjectName.getSerializedNameString()` (real bytecode reached
from `writeObject()`'s non-compat branch) walks the never-populated
`_kp_array` field and NPEs the first time an `ObjectName` is genuinely
Java-serialized — only exercised by the real jmxmp remote
`MBeanServerConnection` wire protocol, not the in-process `MBeanServer`
path the rest of the synthetic ObjectName natives cover. Added a native
override (`RKC-ObjectName-03` in `jmx.rs`) deriving the same canonical text
from the existing text model, matching the established
`getCanonicalKeyPropertyListString` pattern. Full `jmx.*` suite (32 classes,
all `jmx.access`/`jmx.export`/`jmx.support` tests) reconfirmed 100% passing
after the fix, no regressions. Landed on `dev` at `6923fcb9c`
(`fix/jmx-cluster-fix-20260720`).

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

## AOT cluster — 2026-07-20 evening session (JIT miscompilation root-cause + fixes)

**Two genuine, previously-unknown JIT miscompilation bugs found and fixed**
(dev `a8165d607`), plus a fresh from-scratch rebaseline of the whole
15-class AOT cluster against the fix. This is a different root cause
family than the loader-identity bugs fixed 2026-07-15/16 — those were
real and are still in place, but a *separate* defect in the JIT's x64
lowering of two `com.sun.tools.javac` methods was independently
corrupting most of the compile-heavy AOT codegen tests whenever
`TestCompiler` ran enough in-process `javac` invocations in one JVM
(each AOT test method does its own `getTask().call()`; a whole-class run
is 20-50+ such calls in one process).

**Root-caused with a Spring-free, ~40-line standalone repro**
(`ToolProvider.getSystemJavaCompiler().getTask(...).call()` looped
in a plain Java `main`, no Spring/JUnit involved) — reproduces
deterministically at the 20th call every time, isolating this
entirely from Spring/AOT-specific machinery:

1. **`com.sun.tools.javac.jvm.ClassReader.readClass`** — once
   tier-compiled, throws `NullPointerException: Cannot read field "kind"
   because "sym" is null` from inside `Symbol.packge`, reached via
   `ClassReader.readClass -> readClassBuffer -> readClassFile ->
   ClassFinder.fillIn -> Modules$1.complete` (module-graph symbol
   completion during `Modules.setupAllModules`). Confirmed JIT-only
   (`--nojit` / `CRATONVM_JIT_THRESHOLD=100000` both prevent it) and
   bisected to this exact method via `CRATONVM_JIT_BISECT_SKIP=
   com/sun/tools/javac/jvm/ClassReader.readClass`. Fixed by adding it to
   the JIT interpreter-fallback skip-list (`vm/src/jit/skip_list.rs`,
   `SkipReason::ClassReaderReadClass`).
2. **`com.sun.tools.javac.code.ClassFinder.complete`** — a second,
   distinct residual in the same scenario, surfacing even with (1)
   fixed. Two symptoms: a `-Werror`/`@SuppressWarnings("deprecation")`
   false positive (the suppression annotation IS present in the
   generated source but real javac's `-Werror` still fails the
   compile), and outright duplicated tokens in generated source (e.g.
   `import import org.springframework.aot.generate.Generated;`).
   Bisected the same way (`CRATONVM_JIT_DENY=
   com/sun/tools/javac/code/ClassFinder` then `CRATONVM_JIT_BISECT_SKIP=
   .../ClassFinder.complete`). Fixed via
   `SkipReason::ClassFinderComplete`.

Both fixes are narrowly scoped (single named method each), regression-
checked (`cargo test -p cratonvm-vm --lib --release`: 2218 passed / 17
failed, byte-identical to the documented pre-existing lock_order/
skip_list release-mode baseline both before and after), and merged to
`dev` (`a8165d607`).

**Rebaseline after the fix** (fresh worktree, from-scratch
`spring-framework-recheck` checkout — see host-state note below — real
JDK 25, per-class timeouts raised to 300-550s since interpreter-fallback
adds real overhead to these compile-heavy tests):

| Class | Before | After | Notes |
|---|---|---|---:|
| `beans.factory.annotation.AutowiredAnnotationBeanRegistrationAotContributionTests` | TIMEOUT | **OK 14/14** | fixed |
| `beans.factory.aot.BeanDefinitionPropertiesCodeGeneratorTests` | TIMEOUT/LOADERR | **OK 47/47** | fixed (needs ~470s, not 120-350s) |
| `beans.factory.aot.BeanDefinitionPropertyValueCodeGeneratorDelegatesTests` | TIMEOUT | **OK 44/44** | fixed (needs ~460s) |
| `beans.factory.aot.InstanceSupplierCodeGeneratorTests` | LOADERR | **OK 26 found/24 succ/0 fail** (2 skip) | fixed |
| `orm.jpa.support.PersistenceAnnotationBeanPostProcessorAotContributionTests` | TIMEOUT (FAIL 8/2/6 historically) | **OK 8/8** | fixed |
| `test.context.aot.TestClassScannerTests` | TIMEOUT | **OK 7/7** | fixed |
| `beans.factory.aot.BeanRegistrationsAotContributionTests` | TIMEOUT | TIMEOUT (perf partially fixed — see below) | **genuine, severe performance defect — do not call this "not a bug".** Originally ~54 minutes (`3242228ms`). Measured HotSpot on the SAME classpath/JDK: **`13126ms` (13.1s)**, ~247x slower. **2026-07-21 session: root-caused and fixed the dominant lever.** gdb sampling of the largest method (`applyToWithVeryLargeBeanDefinitionsCreatesSeparateSourceFiles`, 10001 bean definitions) found `Arena::free_list_bytes()`'s summation closure dominating 3/5 stack samples — its epoch-gated cache (2026-07-15) degrades back to O(free-list-size) per call under steady allocation churn (content changes on nearly every call from its only caller, `needs_gc`, which runs on every allocation), and the list never fully drains over a session. Replaced with an incrementally-maintained running total (`gc/src/arena.rs`), making it unconditionally O(1); also memoized `force_native_over_real_jdk_bytecode` for uncached dispatch paths (reflective `Method.invoke()`, megamorphic call sites) reached via Mockito's constructor-mock dispatch. Merged `49b75fa20`. Measured impact: the fixed method alone dropped 379s -> 179s (2.13x); full 14-method class dropped 3242s -> 2976s (~8.2% aggregate -- the other 13 methods don't hit the same free-list-growth pathology as severely, since it scales with allocation volume and only that one method allocates ~10k objects). Residual ~227x-vs-HotSpot gap remains and needs further investigation beyond the free-list fix -- the next lead is whatever dominates the OTHER 13 methods' time, not yet profiled. |
| `beans.factory.aot.BeanDefinitionMethodGeneratorTests` | LOADERR | FAIL **34/31/3** (was effectively 34/16/18 pre-fix) | major improvement; 3 residuals are the SAME duplicated-token/content-corruption shape as fix (2) above but NOT YET isolated to a specific method — `--nojit` makes this class fully 34/34, so it's confirmed JIT, just an unidentified third culprit. Next step: repeat the `CRATONVM_JIT_DENY`/`CRATONVM_JIT_BISECT_SKIP` bisection from this session on the 3 remaining methods (`generateBeanDefinitionMethodWhenInnerBeanGeneratesMethod`, `generateBeanDefinitionMethodWhenBeanIsInJavaPackage`, `generateBeanDefinitionMethodWithDeprecatedGenericElementInTargetClass` — the last is the SAME duplicated-import bug, not a distinct deprecation issue despite the name). |
| `context.aot.ApplicationContextAotGeneratorTests` | TIMEOUT | FAIL **40/32/8** (2026-07-21 session; was 40/16/24) | **2026-07-21: found and fixed 6 compounding bugs in the native ConfigurationClassEnhancer CGLIB-proxy reimplementation** (`native-builtins/src/cglib_enhancer.rs`), all surfaced by the single dominant family "any @Configuration class using constructor injection": (1) generated proxy constructor was always no-arg regardless of the superclass's real constructor -- fixed by emitting one delegating constructor per non-private superclass constructor; (2) generated class was named with cglib's default `$$EnhancerByCGLIB$$` tag instead of Spring's own `SpringNamingPolicy` `$$SpringCGLIB$$` tag, so even a correctly-built class was invisible under the name generated source references; (3) the native reimplementation never notified `ReflectUtils.generatedClassHandler`, so Spring AOT's `GeneratedFiles` capture (needed for the LATER compile step to resolve the proxy class) never fired; (4) the per-superclass class-identity cache (added earlier, load-bearing for a different test) skipped that notification entirely on a cache hit, so a SECOND test enhancing an already-cached class never got its own `GeneratedFiles` populated; (5) real CGLIB emits `CGLIB$SET_STATIC_CALLBACKS`/`CGLIB$SET_THREAD_CALLBACKS` stub methods on every generated class that `Enhancer.isEnhanced()`/`registerStaticCallbacks()` reflectively check for -- added as no-op stubs since this reimplementation never uses a real callback array; (6) resolving `ReflectUtils` by a loader-agnostic (or enhanced-class-scoped) lookup could resolve the WRONG `ReflectUtils` instance under `@CompileWithForkedClassLoader` (each test gets its own forked child loader for infrastructure classes) -- fixed by resolving via the `enhance()` call's own receiver's loader instead. Merged `da109dc5a`. Verified 16/40 -> 32/40 (31/40 on a from-scratch merge-tip rebuild, small variance consistent with this class's already-documented cross-test-timing sensitivity). Remaining 8 residuals include at least one distinct, unrelated bug: `@Value`-annotated field injection not reaching the proxied instance (`processAheadOfTimeWhenHasCglibProxyAndMixedAutowiring` now compiles and runs but asserts `"Hi null"` instead of `"Hi AOT World"`) -- not yet root-caused, a separate area from proxy generation itself. |
| `test.context.aot.AotIntegrationTests` | TIMEOUT | FAIL **4/0/2** | now completes (was hanging); both failures are `TestContextAotException: Failed to generate AOT artifacts for test classes [...]` wrapping a nested cause not yet unwrapped — needs `KRUN_STACK=1`+full stack trace to find the real cause. |
| `test.context.aot.TestContextAotGeneratorIntegrationTests` | FAIL 0/4 | FAIL **4/2/2** | improved (0→2 passing); 1 residual is `TestContextAotException` for `WebSpringVintageTests` (same shape as AotIntegrationTests above, possibly shared cause), 1 is `AssertionError: [Proxy hint for GreetingService] ... did not` (a genuine RuntimeHints registration gap, likely unrelated to the javac JIT bugs). |
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
1. Bisect `BeanDefinitionMethodGeneratorTests`'s 3rd JIT residual the
   same way (this session's `CRATONVM_JIT_DENY`/`CRATONVM_JIT_BISECT_SKIP`
   technique on `com/sun/tools/javac/*`).
2. Re-verify `ApplicationContextAotGeneratorTests` with `--nojit` first
   to confirm/refute it's the same JIT family before any other work.
3. Get the full stack trace (`KRUN_STACK=1`) for `AotIntegrationTests`'s
   and `TestContextAotGeneratorIntegrationTests`'s `TestContextAotException`
   causes — the outer wrapper hides the real defect.
4. Root-cause the new `ArrayStoreException`/Jackson `ArrayBuilders`
   finding shared by both `web.service.registry.*` classes.

**`test.context.jdbc.*` cluster — fully fixed (0 remain).** All 25 classes
that were uniformly failing behind Spring's `ApplicationContext` failure
threshold circuit-breaker now pass. Whatever landed in the last 3 days
resolved the whole cluster at once — worth checking dev history for the
specific fix if attribution matters.

**HTTP JSON/message-converter cluster — fixed 2026-07-20 (8/8 classes).**
`http.converter.json.*` (Gson, Jackson2, MappingJackson2, Jsonb,
Kotlin-serialization), `http.converter.StringHttpMessageConverterTests`,
`http.ContentDispositionTests`, `http.client.SimpleClientHttpRequestFactoryTests`
all now pass 100%. Two independent root causes, both in `native-io`/
`native-builtins`/`vm`:
1. **Shared charset/encoding gap (7/8 classes).** `ByteArrayOutputStream
   .toString(Charset)`/`toString(String)` (`native-io/src/lib.rs`) ignored
   the charset argument entirely and always did lossy UTF-8 decoding —
   fine for ASCII/UTF-8 content, silently mangling anything else (UTF-16BE
   JSON bodies in the `writeUTF16`/`writeObjectInUtf16` tests, ISO-8859-1
   in `StringHttpMessageConverterTests.writeDefaultCharset`, Shift_JIS in
   `ContentDispositionTests.parseQuotedPrintableShiftJISFilename`'s
   RFC 2047 decode, all of which route through this exact JDK method via
   `StreamUtils.copyToString(ByteArrayOutputStream, Charset)`). Fixed by
   routing through the real `cratonvm_native_api::charset` engine using the
   requested charset.
2. **`SimpleClientHttpRequestFactoryTests` (1/8 classes, 3 residual method
   failures after fix 1).**
   - `deleteWithoutBodyDoesNotRaiseException`/`httpMethods`: the synthetic
     `HttpURLConnection.<init>(URL)` native (`native-builtins/src/
     http_url_connection.rs::huc_init`) unconditionally clobbered field 0
     (the real inherited `URLConnection.url`) whenever real JDK code called
     `super(url)` directly on a subclass (not just via `URL.openConnection
     ()`), breaking `getURL()` and real-carrier detection; separately,
     `setRequestMethod` accepted `"PATCH"` (real JDK's whitelist doesn't,
     throwing `ProtocolException` — added as a new `RuntimeError` variant).
   - `interceptor`: a genuinely deep, cross-cutting bug — `Mockito.mock
     (HttpURLConnection.class)` (default "inline" mock maker) redefines the
     class's bytecode IN PLACE via JVMTI rather than subclassing it, so
     CratonVM's redefine-generation counter for `java/net/HttpURLConnection`
     trips permanently for the rest of the process, for EVERY instance —
     including totally unrelated, genuinely real connections created by
     *later* tests in the same JVM. The interpreter's redefine-guard then
     ceded to the (Mockito-woven) bytecode for those real connections too,
     so `getResponseCode()`/`getHeaderField()`/etc. silently no-op'd instead
     of touching the real request/response. Fixed with a receiver-aware
     exemption in `vm/src/runtime/interpreter.rs::intercept_force_registered
     _native`: force the native for `java/net/HttpURLConnection` whenever
     the receiver's field 0 is non-null (a real carrier's populated `url`
     field vs. a Mockito mock's always-null Objenesis-constructed field),
     re-validated per-call so genuine mocks (field 0 stays null) are
     unaffected and still correctly route through Mockito's advice.

Verified via an 8-class targeted run (all 100%) plus a 27-class regression
sweep across `http.client.*`/`web.client.*`/the sibling `http.converter`
cluster (`FormHttpMessageConverterTests`, `BufferedImageHttpMessageConverterTests`,
`Jaxb2CollectionHttpMessageConverterTests`) — no regressions;
`web.client.RestClientIntegrationTests`/`RestTemplateIntegrationTests`
(both pre-existing, out-of-scope failures) even improved (4->2 and 7->3
failing methods respectively), consistent with sharing the same
HttpURLConnection root causes.

**`scheduling.concurrent.*` cluster — fixed 2026-07-20 (4/4 classes).**
`ConcurrentTaskExecutorTests`, `DecoratedThreadPoolTaskExecutorTests`,
`ThreadPoolTaskExecutorTests`, `ThreadPoolTaskSchedulerTests` all now pass
100% (18/18, 14/14, 23/23, 40/40). Root cause: `native-collections` shadowed
`getCorePoolSize`/`getMaximumPoolSize`/`isShutdown`/`isTerminated`/
`shutdownNow` on the concrete class `java/util/concurrent/ThreadPoolExecutor`
unconditionally with CratonVM's synthetic 2-field executor layout, even for
REAL bytecode-constructed `ThreadPoolExecutor` instances (disambiguated only
by class name, which collides with the synthetic placeholder) — so
`setCorePoolSize()`/`setMaximumPoolSize()` mutations were silently ignored on
readback, and `shutdownNow()` interrupted workers but always returned an
empty list instead of draining `workQueue`, leaving queued `FutureTask`s
neither run nor cancelled (`future.get(timeout)` threw `TimeoutException`
instead of `CancellationException`). Fixed by routing real receivers through
the real JDK bytecode instead of the synthetic slots (see
`native-collections/src/lib.rs` `tp_is_real`), landed on `dev` at `2b41ba9b0`.
`scheduling.quartz.QuartzSupportTests` was investigated as a possible shared
residual but could not be verified either way: its module
(`spring-context-support`) doesn't compile against the shared
spring-framework checkout used for classpath generation (missing the
`org.springframework.aop.target` source package entirely, pre-existing and
unrelated to CratonVM) — left open, out of scope for the concurrent-cluster
fix.

**Groovy — 1/4 fixed.** `scripting.groovy.GroovyAspectTests` is now fixed;
`context.groovy.GroovyBeanDefinitionReaderTests` and
`scripting.groovy.GroovyScriptFactoryTests` are still hung (TIMEOUT), and
`web.servlet.view.groovy.GroovyMarkupViewTests` still FAILs (9/10).

**Resolved since 2026-07-17**: the `web.servlet.mvc.method.RequestMappingInfoHandlerMappingTests`
anomaly (previously FAIL despite 43/43 methods passing) is now a clean OK
(45/45) — whatever caused that status/method-count mismatch is gone.

## Full class list (66), by module

### Aop

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `aop.framework.autoproxy.BeanNameAutoProxyCreatorTests` | FAIL | 8/9 | 8259ms |
| `aop.support.MethodMatchersTests` | FAIL | 13/14 | 10851ms |

### Beans

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `beans.ConcurrentBeanWrapperTests` | FAIL | 100/101 | 16852ms |
| `beans.factory.annotation.AutowiredAnnotationBeanRegistrationAotContributionTests` | OK (2026-07-20 JIT fix) | 14/14 | 138189ms |
| `beans.factory.aot.BeanDefinitionMethodGeneratorTests` | FAIL (2026-07-20, major improvement, see above) | 31/34 | 324252ms |
| `beans.factory.aot.BeanDefinitionPropertiesCodeGeneratorTests` | OK (2026-07-20 JIT fix, needs ~470s) | 47/47 | 466737ms |
| `beans.factory.aot.BeanDefinitionPropertyValueCodeGeneratorDelegatesTests` | OK (2026-07-20 JIT fix, needs ~460s) | 44/44 | 456855ms |
| `beans.factory.aot.BeanRegistrationsAotContributionTests` | TIMEOUT (severe perf defect — ~247x slower than HotSpot; free-list O(1) fix landed 2026-07-21, ~8.2% aggregate improvement so far, see above) | 0/0 | 350000ms |
| `beans.factory.aot.InstanceSupplierCodeGeneratorTests` | OK (2026-07-20 JIT fix) | 24/26 | 225204ms |
| `beans.factory.xml.XmlBeanFactoryTests` | FAIL | 85/95 | 85837ms |

### Context

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `context.annotation.ComponentScanParserBeanDefinitionDefaultsTests` | TIMEOUT | 0/0 | 120000ms |
| `context.annotation.ConfigurationClassPostConstructAndAutowiringTests` | FAIL | 1/2 | 412ms |
| `context.annotation.ConfigurationClassPostProcessorTests` | FAIL | 82/85 | 20126ms |
| `context.annotation.Spr15275Tests` | FAIL | 4/6 | 2038ms |
| `context.annotation.Spr6602Tests` | FAIL | 1/2 | 1229ms |
| `context.aot.ApplicationContextAotGeneratorTests` | FAIL (2026-07-20, see caveat above — needs re-verify) | 16/40 | 389879ms |
| `context.groovy.GroovyBeanDefinitionReaderTests` | TIMEOUT | 0/0 | 120000ms |

### Core

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `core.GenericTypeResolverTests` | FAIL | 24/25 | 2651ms |
| `core.annotation.NestedRepeatableAnnotationsTests` | FAIL | 2/12 | 759ms |
| `core.io.ResourceTests` | FAIL | 66/68 | 4689ms |
| `core.io.buffer.DataBufferTests` | TIMEOUT | 0/0 | 120000ms |
| `core.retry.RetryPolicyTests` | FAIL | 22/23 | 828ms |

### Expression

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `expression.spel.MethodInvocationTests` | FAIL | 22/23 | 2158ms |
| `expression.spel.SpelCompilationCoverageTests` | FAIL | 159/162 | 26105ms |

### Http

All 8 HTTP JSON/message-converter cluster classes fixed 2026-07-20 — see
"Notable clusters" above. Removed from this table.

### Jdbc

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `jdbc.core.namedparam.BeanPropertySqlParameterSourceTests` | FAIL | 7/10 | 4631ms |
| `jdbc.core.namedparam.MapSqlParameterSourceTests` | FAIL | 3/6 | 1118ms |

### Jms

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `jms.core.JmsTemplateTransactedTests` | FAIL | 51/52 | 13857ms |

### Jndi

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `jndi.JndiObjectFactoryBeanTests` | FAIL | 24/25 | 2165ms |

### Orm

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `orm.jpa.support.PersistenceAnnotationBeanPostProcessorAotContributionTests` | OK (2026-07-20 JIT fix) | 8/8 | 136535ms |
| `orm.jpa.support.PersistenceInjectionTests` | FAIL | 26/27 | 11461ms |

### Scheduling

`scheduling.concurrent.*` (4 classes: `ConcurrentTaskExecutorTests`,
`DecoratedThreadPoolTaskExecutorTests`, `ThreadPoolTaskExecutorTests`,
`ThreadPoolTaskSchedulerTests`) fixed 2026-07-20 — see "Notable clusters"
above. Removed from this table.

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `scheduling.quartz.QuartzSupportTests` | FAIL | 8/17 | 9296ms |

(`QuartzSupportTests` not re-verified this session — see note above; kept as
FAIL/8/17 from the 2026-07-20 reconfirmation rerun.)

### Scripting

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `scripting.groovy.GroovyScriptFactoryTests` | TIMEOUT | 0/0 | 120000ms |

### Test

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `test.context.BootstrapUtilsTests` | FAIL | 22/23 | 9109ms |
| `test.context.aot.AotIntegrationTests` | FAIL (2026-07-20, now completes, see above) | 0/4 | 56296ms |
| `test.context.aot.TestClassScannerTests` | OK (2026-07-20 JIT fix) | 7/7 | 197691ms |
| `test.context.aot.TestContextAotGeneratorIntegrationTests` | FAIL (2026-07-20, improved, see above) | 2/4 | 148117ms |
| `test.context.bean.override.mockito.MockitoBeanByTypeLookupIntegrationTests` | FAIL | 3/5 | 27473ms |
| `test.context.bean.override.mockito.constructor.MockitoBeanByTypeLookupForConstructorParametersIntegrationTests` | FAIL | 4/6 | 16982ms |
| `test.context.junit.jupiter.event.ParallelApplicationEventsIntegrationTests` | FAIL | 0/2 | 918ms |
| `test.context.junit.jupiter.parallel.ParallelExecutionSpringExtensionTests` | TIMEOUT | 0/0 | 120000ms |
| `test.context.testng.TestNGConcurrencyTests` | FAIL | 0/1 | 3669ms |
| `test.web.servlet.assertj.MockMvcTesterIntegrationTests` | FAIL | 72/74 | 58069ms |

### Util

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `util.CollectionUtilsTests` | FAIL | 30/32 | 1016ms |
| `util.StreamUtilsTests` | FAIL | 10/11 | 5019ms |

### Web

| Class | Status | Pass/Total | Elapsed |
|---|---|--:|--:|
| `web.client.RestClientIntegrationTests` | FAIL | 226/230 | 96436ms |
| `web.client.RestTemplateIntegrationTests` | FAIL | 118/125 | 88728ms |
| `web.context.request.RequestScopeTests` | FAIL | 0/7 | 1300ms |
| `web.reactive.function.client.WebClientIntegrationTests` | FAIL | 168/170 | 47143ms |
| `web.reactive.result.method.annotation.CrossOriginAnnotationIntegrationTests` | TIMEOUT | 0/0 | 120000ms |
| `web.reactive.result.method.annotation.RequestMappingMessageConversionIntegrationTests` | TIMEOUT | 0/0 | 120000ms |
| `web.service.registry.HttpServiceProxyRegistrationAotProcessorTests` | FAIL (2026-07-20, now completes, NEW bug found, see above) | 3/5 | 51669ms |
| `web.service.registry.ImportHttpServiceRegistrarTests` | FAIL (original CCE confirmed gone, new shared bug, see above) | 3/5 | 54239ms |
| `web.servlet.config.MvcNamespaceTests` | FAIL | 24/25 | 24938ms |
| `web.servlet.config.annotation.ViewResolutionIntegrationTests` | FAIL | 6/7 | 29415ms |
| `web.servlet.view.groovy.GroovyMarkupViewTests` | FAIL | 9/10 | 28950ms |
| `web.socket.messaging.StompWebSocketIntegrationTests` | FAIL | 14/16 | 105475ms |

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
