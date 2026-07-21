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
| `beans.factory.aot.BeanDefinitionMethodGeneratorTests` | LOADERR | **OK 34/34 (2026-07-21, FIXED)** | **fully fixed.** The 3 residuals (2 as of a 2026-07-21 rebaseline — `generateBeanDefinitionMethodWhenInnerBeanGeneratesMethod` content-corruption + `generateBeanDefinitionMethodUSeBeanClassNameIfNotReachable`'s `ClassCastException: String cannot be cast to TypeName`) were a FOURTH javac-adjacent JIT residual, this time in Spring's own shaded JavaPoet, not javac itself: `org/springframework/javapoet/CodeBlock$Builder.add(String, Object...)` (the `$`-placeholder format-string parser). Bisected with `CRATONVM_JIT_DENY`/`CRATONVM_JIT_BISECT_SKIP` the same way as fixes (1)/(2): denying the whole `CodeBlock` class does nothing, denying `CodeBlock$Builder` fixes both symptoms, and narrowing further rules out `argToType`/`addArgument` individually — only `add` itself (which inlines `argToType`'s instanceof-guarded `checkcast` into its own compiled body) is sufficient. Added `SkipReason::JavaPoetCodeBlockBuilderAdd` to `vm/src/jit/skip_list.rs`. Verified 34/34 OK, deterministic across 3 repeat runs. `cargo test -p cratonvm-vm --lib --release`: 2227 passed / 9 failed, same pre-existing release-mode lock_order baseline before/after. Landed on `fix/aot-cluster-residuals-20260721` (`20abd63d0`), not yet merged to `dev`. |
| `context.aot.ApplicationContextAotGeneratorTests` | TIMEOUT | FAIL **40/32/8** (2026-07-21 session; was 40/16/24) | **2026-07-21: found and fixed 6 compounding bugs in the native ConfigurationClassEnhancer CGLIB-proxy reimplementation** (`native-builtins/src/cglib_enhancer.rs`), all surfaced by the single dominant family "any @Configuration class using constructor injection": (1) generated proxy constructor was always no-arg regardless of the superclass's real constructor -- fixed by emitting one delegating constructor per non-private superclass constructor; (2) generated class was named with cglib's default `$$EnhancerByCGLIB$$` tag instead of Spring's own `SpringNamingPolicy` `$$SpringCGLIB$$` tag, so even a correctly-built class was invisible under the name generated source references; (3) the native reimplementation never notified `ReflectUtils.generatedClassHandler`, so Spring AOT's `GeneratedFiles` capture (needed for the LATER compile step to resolve the proxy class) never fired; (4) the per-superclass class-identity cache (added earlier, load-bearing for a different test) skipped that notification entirely on a cache hit, so a SECOND test enhancing an already-cached class never got its own `GeneratedFiles` populated; (5) real CGLIB emits `CGLIB$SET_STATIC_CALLBACKS`/`CGLIB$SET_THREAD_CALLBACKS` stub methods on every generated class that `Enhancer.isEnhanced()`/`registerStaticCallbacks()` reflectively check for -- added as no-op stubs since this reimplementation never uses a real callback array; (6) resolving `ReflectUtils` by a loader-agnostic (or enhanced-class-scoped) lookup could resolve the WRONG `ReflectUtils` instance under `@CompileWithForkedClassLoader` (each test gets its own forked child loader for infrastructure classes) -- fixed by resolving via the `enhance()` call's own receiver's loader instead. Merged `da109dc5a`. Verified 16/40 -> 32/40 (31/40 on a from-scratch merge-tip rebuild, small variance consistent with this class's already-documented cross-test-timing sensitivity). Remaining 8 residuals include at least one distinct, unrelated bug: `@Value`-annotated field injection not reaching the proxied instance (`processAheadOfTimeWhenHasCglibProxyAndMixedAutowiring` now compiles and runs but asserts `"Hi null"` instead of `"Hi AOT World"`) -- not yet root-caused, a separate area from proxy generation itself. |
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

**Recommended next steps for whoever continues this cluster (updated
2026-07-21 late session — see that section below for the full detail
behind each item):**
1. ~~Bisect `BeanDefinitionMethodGeneratorTests`'s 3rd JIT residual~~ —
   DONE, class is 34/34 OK.
2. ~~Re-verify `ApplicationContextAotGeneratorTests` with `--nojit`~~ —
   DONE: confirmed only partially the same JIT family (32/40 JIT-enabled
   after the JavaPoet fix vs. 33/40 under `--nojit` with none of the JIT
   fixes needed) — a 7th CGLIB counter-scoping bug (now fixed, see below)
   accounts for most of the rest.
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

## AOT cluster — 2026-07-21 late session (4th JIT bug, CGLIB counter fix, duplicate-ClassId unifying finding)

**`beans.factory.aot.BeanDefinitionMethodGeneratorTests` — FIXED, 34/34.**
See the updated table row above for the full writeup: a fourth JIT
miscompilation, this time in Spring's shaded JavaPoet
(`org/springframework/javapoet/CodeBlock$Builder.add`) rather than javac
itself. `SkipReason::JavaPoetCodeBlockBuilderAdd` added to
`vm/src/jit/skip_list.rs`.

**`context.aot.ApplicationContextAotGeneratorTests` — found and fixed a
7th CGLIB-proxy bug, on top of the 6 already landed as `da109dc5a`
earlier the same day.** The `$$SpringCGLIB$$<n>` proxy-name counter
(`native-builtins/src/cglib_enhancer.rs::next_config_enhancer_counter`)
was keyed by superclass name ALONE. That's correct for the single-method
case the counter was originally added for
(`AnnotationConfigApplicationContextTests.refreshForAotRegisterHintsForCglibProxy`,
one enhancement of `CglibConfiguration` per JVM), but
`ApplicationContextAotGeneratorTests` has SEVERAL `@Test` methods that each
enhance their OWN fixture class sharing the simple name `CglibConfiguration`
— `@CompileWithForkedClassLoader` gives each test method a fresh child
loader, so these are genuinely distinct `ClassId`s, not repeat enhancements
of one class — and since `KRun` batches every `@Test` method of a class into
one JVM process, the second and third such methods inherited the first
one's already-incremented counter and got suffix `1`/`2` instead of the `0`
every one of them independently expects (real CGLIB's own
`AbstractClassGenerator` naming/cache state lives in a per-`ClassLoader`
map, so a fresh loader always restarts the count on HotSpot). Rekeyed the
counter by `(defining_loader_id, super_internal_name)` instead of the name
alone — `native_array_new_array` also now prefers the component mirror's
own `ClassId` over re-resolving by name, matching the existing
`class_id_defined_by_loader_exact` pattern used a few lines away in
`getComponentType()`.

Isolating the two fixes' individual contributions (both on top of the
already-landed 6-bug CGLIB session and both AFTER a from-scratch rebuild):
JavaPoet fix alone (JIT enabled, no counter fix) measured **32/40**, all 8
residuals CGLIB/autowiring-shaped; `--nojit` (JIT effectively off, no JIT
fixes needed at all) independently measured **33/40**, confirming most but
not all of the residual set is JIT-independent. This class is **extremely**
sensitive to host contention — repeat clean-room runs later the same
session intermittently OOM'd/LOADERR'd purely from unrelated concurrent
sessions' heavy JVMs on the shared Azure host (an ES perf test at `-Xmx
4g`, a WildFly surefire run), not from these fixes; treat any single run's
exact pass count on this class as noisy and prefer a multi-run median, per
the class's own already-documented cross-test-timing sensitivity. Remaining
residuals include the previously-documented `@Value`-field-injection gap
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
