# CratonVM Spring suite — genuine bug list

| | |
|---|---|
| **Sixth session** | 2026-07-30, branch `fix/spring-aot-cluster-20260730`, worktree `/data/data/wt-aot-20260730`, binaries `localbin/cratonvm-aot30-v*.bin`. Closed both reproducible AOT items and the architectural gap under them, plus a dev regression that had made **every** AOT probe die in 1.3 s. The third item is not an AOT defect — it is the separately-tracked Mockito redefine-throughput issue. See *Closed in the sixth session*. |
| **Status** | OPEN — **1 residual class** (was 3 after the fourth session, 9 after the third, 19 before it, 57 before that, 127 before that). Thirteen VM bugs closed in the third session, seven in the fourth, three in the fifth; every one has a standalone HotSpot-vs-CratonVM probe. |
| **Captured** | 2026-07-27 (third session), branch `fix/spring-buglist-final-20260727` merged into `origin/dev` at `1f538bf76`, Azure host `20.83.144.174`, real JDK 25, worktree `/data/data/wt-sprbuglist-20260727`, binaries `localbin/cratonvm-sprfinal-v*.bin`. Every number below was measured with the class run **in isolation** (`apps/spring-suite-runner/onea.sh <fqcn>`), not from a sharded batch — the shared host runs at load 25–100 and batch runs emit spurious FAIL/TIMEOUT rows. |
| **Fourth session** | 2026-07-28, branch `fix/spring-nonaot-20260727` merged into `origin/dev`, worktree `/data/data/wt-spr-nonaot-20260727`, binaries `localbin/cratonvm-nonaot-v*.bin`. Took the five non-AOT residuals from LOADERR/0/2/2/6/26-27/165-170 to **fully green**, and turned the sixth (`RequestMappingMessageConversionIntegrationTests`) out to be a heap-sizing artifact rather than a linkage bug. See *Closed in the fourth session* below. |
| **Fifth session** | 2026-07-29, branch `fix/spring-aot-final-20260728` merged into `origin/dev` at `9a2fc262b`, worktree `/data/data/wt-spraot-20260728`, binaries `localbin/cratonvm-spraot-v*.bin`. Closed two of the three AOT residuals — **both were SIGSEGVs mis-filed as timeouts**, not throughput — and narrowed the third from "8 sub-failures" to three named items with a one-minute repro each. See *Closed in the fifth session* below. |
| **History** | Everything before the third session is archived in [`../internal/fixed-suite-bugs/spring/CRATONVM-SPRING-GENUINE-BUGLIST-history-20260727.md`](../internal/fixed-suite-bugs/spring/CRATONVM-SPRING-GENUINE-BUGLIST-history-20260727.md). Its conclusions are superseded by the entries below wherever the two disagree. |

## Closed this session

| class | before | after |
|---|--:|--:|
| `beans.factory.annotation.AutowiredAnnotationBeanRegistrationAotContributionTests` | 1/14 | **14/14** |
| `core.annotation.MergedAnnotationsTests` | 177/178 | **178/178** |
| `core.io.ModuleResourceTests` | 2/3 | **3/3** |
| `core.io.ResourceTests` | 66/68 | **68/68** |
| `core.io.support.PathMatchingResourcePatternResolverTests` | 19/22 | **22/22** |
| `scripting.groovy.GroovyScriptFactoryTests` | 27/38 | **38/38** |
| `test.web.servlet.assertj.MockMvcTesterIntegrationTests` | 72/74 | **74/74** |
| `util.SerializationUtilsTests` | 8/9 | **9/9** |
| `web.reactive.function.client.DefaultWebClientTests` | 24/25 | **25/25** |
| `test.context.aot.TestContextAotGeneratorIntegrationTests` | 2/4 | **4/4** |
| `context.annotation.ConfigurationClassEnhancerTests` | 3/5 | **4/5** — the residual `withPublicClass` fails identically on HotSpot in this checkout (fixture artifact, see below) |

### The eleven fixes

1. **`URL.openConnection()` gave `jrt:` URLs an `HttpURLConnection` carrier.**
   `con instanceof HttpURLConnection` was therefore true for a runtime-image
   resource, so `AbstractFileResolvingResource.isReadable` fired a HEAD request
   at a `jrt:` URL and answered false for a class that `openStream()` served
   fine. Return `sun.net.www.protocol.jrt.JavaRuntimeURLConnection`, as the
   real JDK does. `probes/JrtProbe.java`.

2. **Resource URLs embedded the raw filesystem path.** A directory literally
   named `custom#root` came back from `ClassLoader.getResource` as
   `file:/…/custom#root/…`, where the bare `#` is a fragment delimiter, so
   everything after it was dropped downstream. Percent-encode in
   `class_path.rs`, matching `File.toURI()` and the sibling encoder in
   `classloader::file_url_spec`. `probes/UclProbe.java`.

3. **`Class.forPrimitiveName` fabricated a mirror for any name.** It shares its
   native with `getPrimitiveClass`, which the JDK only ever calls with real
   primitive names; JDK 25's `ObjectInputStream.resolveClass` calls
   `forPrimitiveName` in its `ClassNotFoundException` catch block, so a
   genuinely missing stream class resolved to a bogus class and deserialization
   reported `InvalidClassException` instead of `ClassNotFoundException`.
   `probes/OisProbe6.java`.

4. **Annotation-proxy `equals` was asymmetric.** Two dispatch paths reached
   `annotation_proxy_dispatch_impl` directly (`NativeContext::invoke_virtual`,
   and the real `$ProxyN` handler shim), both skipping the delegation to a
   FOREIGN proxy of the same annotation type. `realAnnotation.equals(springSynthesized)`
   answered false while the reverse answered true — visible only through
   AssertJ, whose `areEqual` is natively shimmed and therefore took the native
   path. `probes/AnnEqProbe2.java`.

5. **`UnixFileSystem.normalize` was a no-op.** `new File("/a/b/")` kept its
   trailing separator and `new File("/a//b")` its duplicate one, so
   `rootDir.getAbsolutePath() + "/" + subPattern` produced `…//*.txt`.
   `probes/FileNormProbe.java`.

6. **`Path.of(URI)` / `FileSystemProvider.getPath(URI)` used the RAW path.**
   Percent-escapes survived into the filesystem path and `Files.exists` /
   `Files.walk` saw nothing. Ask the URI for its decoded `getPath()` first —
   the same shape as the earlier `new File(URI)` fix. `probes/PathWalkProbe.java`.

7. **`Class.getClassLoader()` answered "application" for user-defined
   namespaces.** Any class a native defines through `define_class_full` (the
   config-class enhancer among them) has a real user-defined loader namespace
   but no `register_defining_loader` entry, and the fallback chain only special-
   cased bootstrap and platform. Consult `loader_object_for_namespace_id`.

8. **`getDeclaredAnnotations()` returned an EMPTY array for ambiguous
   annotation types.** The loadability filter used `class_id_by_name`
   (= `find_unique_class_by_name`), which deliberately answers `None` when a
   name is defined by more than one loader. Spring's
   `@CompileWithForkedClassLoader` fork re-defines the framework, annotation
   types included, so `MergedAnnotations.from(field)` found nothing and
   `AutowiredAnnotationBeanPostProcessor.processAheadOfTime` returned null for
   every forked test. Resolve the annotation type relative to the DECLARING
   class first (`class_id_by_name_near`). `probes/ForkAnnProbe.java`.
   **This is the "regression landed by `origin/dev`" the previous session
   bisected to the `60a710ad8..ffc7f90d4` window** — `a50ce9348 fix(classloading):
   eliminate loader-blind VM lookups` made the by-name lookup ambiguity-strict,
   which is correct; this filter was the caller that could not cope.

9. **`invoke_or_native` resolved the dispatch class by NAME.** Ambiguous as
   soon as two loaders define it — `GroovyScriptFactory` compiles the same
   script class through a fresh `GroovyClassLoader` per application context, so
   by the second context `…groovy/TestFactoryBean` names two classes. The
   interpreter's own dispatch is ClassId-based, so this surfaced only through
   the JIT's MIC helper, as a `NoClassDefFoundError` for a class that was very
   much loaded (`--nojit` was 38/38 throughout). Prefer the receiver's own
   ClassId when its runtime class carries exactly that name.

10. **Two occurrences of the same method reference collapsed to one object.**
    The zero-capture lambda singleton cache was keyed by `proxy_class_id`,
    which is per CP entry — and javac folds repeated `Foo::bar` occurrences
    onto ONE `CONSTANT_InvokeDynamic`. JVMS §5.4.3.6 links each invokedynamic
    *instruction* separately, and HotSpot hands out a distinct instance per
    occurrence. Spring's `WebClient.Builder.defaultStatusHandler` keys a
    `LinkedHashMap` on the predicate, so two `HttpStatusCode::is4xxClientError`
    registrations became ONE entry and the second silently replaced the first.
    Key the cache by the call-site bci as well. `probes/LambdaId2Probe.java`.

11. **`PrintWriter.println` ignored `autoFlush`.** The shared `println` natives
    wrote straight through with no `if (autoFlush) out.flush()` tail, so a
    `PrintWriter(stream, true)` kept everything after the last 8 KiB boundary.
    `MockMvcTester.debug(out)`'s report came back truncated — and always right
    after a heading, because `printf` (which does flush) and `println`
    disagreed. `print` still deliberately does not flush.
    `probes/PwFlushProbe.java`.

12. **An annotation array VALUE's component was resolved loader-blind.** The
    scalar `Enum`/`Class` arms already went through the declaring class's
    loader; the array arm did not, so under classloader isolation the array's
    component came from the app loader while the attribute's declared return
    type came from the fork.

13. **`Class.isInstance` disagreed with `Class.isAssignableFrom`.** `isInstance`
    resolved `mirror_class_id(this)` first and early-returned false when that
    missed — before reaching its own array-aware branch. `isAssignableFrom` has
    the same branch and always ran it first, needing no ClassId. The lookup
    misses for an array mirror handed out by `Method.getReturnType()` under a
    custom loader, because CratonVM mints a **fresh array mirror per request**
    instead of caching one per component class (`probes/ForkArr2Probe.java`
    shows three distinct identity hashes for one component where HotSpot shows
    one). Spring's `ClassUtils.isAssignableValue` calls `isInstance`, so
    `AnnotationTypeMapping.adapt` rejected an annotation's enum-array value
    with the self-contradictory *"should be compatible with `RequestMethod[]`
    but a `RequestMethod[]` value was returned"*. Moving the array branch above
    the ClassId resolution took
    `test.context.aot.TestContextAotGeneratorIntegrationTests` **2/4 → 4/4**
    (it SIGSEGV'd before this session). The mirror proliferation itself is a
    real but separate divergence — `arrayClass == arrayClass` is still false
    across a loader boundary where HotSpot says true; nothing in the suite
    depends on it, and fixing it means touching `synthesize_array_class`'s
    audited `loader_id == Bootstrap` invariant.

Plus **`ProcessHandle.Info.command()`** now reports this VM's own executable
instead of an empty `Optional` (the standard "find my JVM and spawn a child"
idiom raised `NoSuchElementException`), which is what took
`PathMatchingResourcePatternResolverTests` the last two tests to 22/22.

Three debug levers were added along the way, because their absence is what made
the last two bugs slow to find: `CRATONVM_DBG_LINKAGE_BT=1` now also fires at
`raise_no_class_def_found` and at the JIT dispatch-error mapper (previously only
`linkage_throwable`), and `CRATONVM_DBG_STUB_BT` also covers
`ensure_synthetic_class`.

## Closed in the fourth session (2026-07-28)

The five non-AOT residuals, plus one that was never a VM bug.

| class | before | after |
|---|--:|--:|
| `beans.PropertyDescriptorUtilsPropertyResolutionTests` | LOADERR | **42/42** |
| `orm.jpa.support.PersistenceInjectionTests` | 26/27 | **27/27** |
| `test.context.junit.jupiter.event.ParallelApplicationEventsIntegrationTests` | 0/2 | **2/2** |
| `web.reactive.result.view.FragmentViewResolutionResultHandlerTests` | 2/6 | **6/6** |
| `web.reactive.function.client.WebClientIntegrationTests` | 165/170 | **169 + 1 skip, 0 fail** (HotSpot parity) |
| `web.reactive.result.method.annotation.RequestMappingMessageConversionIntegrationTests` | LOADERR | **160/160** at a 6 GB heap — see below |

### The seven fixes

1. **`ForkJoinTask` dispatch invoked `compute()` blind.** The eager-inline
   Bridge policy tried `compute()Ljava/lang/Object;` and then `compute()V`,
   which covers exactly `RecursiveTask` and `RecursiveAction` and nothing
   else. The protocol method every concrete `ForkJoinTask` implements is
   `exec()Z`, and JUnit Platform's
   `ForkJoinPoolHierarchicalTestExecutorService$ExclusiveTask` extends
   `ForkJoinTask` directly — so both invokes raised `NoSuchMethodError`, both
   were swallowed, and a nested engine in `CONCURRENT` mode executed ZERO
   tests (*"started ==> expected: 13 but was: 0"*). Pick the entry point from
   the receiver's runtime class instead of from a failed speculative invoke.

2. **`ConcurrentHashMap` serialised as EMPTY.** CratonVM keeps CHM entries in
   a segmented native layout, so the real JDK `writeObject` — which walks the
   always-null `table` — wrote no entries, and `readObject` rebuilt a `table`
   our natives never read. A two-entry map round-tripped to `size() == 0`
   (`probes/ChmSerProbe.java`); `HashMap`, `ConcurrentLinkedQueue` and
   `CopyOnWriteArrayList` were all fine. Spring's
   `PersistenceAnnotationBeanPostProcessor` tracks extended `EntityManager`s
   in a CHM, so after a `SimpleMapScope` round-trip there was nothing left to
   close. Added natives for both hooks that emit/consume the JDK's serial
   form, plus the `force_native_over_real_jdk_bytecode` entry they need to
   beat the real bytecode through `ObjectStreamClass`'s reflective
   `Method.invoke`.

3. **`find_unique_class_by_name` was a full linear scan** of `loaded_classes`
   with a string compare per entry. A `perf` profile of a Jython module parse
   put it at **55% of ALL CPU samples** (its `memcmp` another 3.8%). The JIT's
   own callers memoize their answers, but `try_compile_direct_dispatcher`
   deliberately does not cache a miss (a not-yet-loaded class can load later),
   so every unresolvable name paid a fresh whole-map scan. Now backed by an
   incrementally maintained `name -> {ClassId: refcount}` index; the linear
   scan survives as the executable spec a new test asserts the index against.

4. **JIT-compiled JDK dynamic-proxy trampolines.** Every `$ProxyN` method is a
   `super.h.invoke(this, mN, args)` trampoline whose semantics CratonVM
   implements at DISPATCH, not in that bytecode — a compiled body is a third
   path that bypasses annotation-member coercion and the `equals` delegation
   to a foreign proxy that fix #4 of the third session had to add. The symptom
   was `OutOfMemoryError` after ~100 s of GCs reclaiming almost nothing, at
   512 MB, 2 GB and 8 GB heaps alike, while `--nojit` ran clean. Package
   bisection over eight configurations pinned it to `jdk/proxy` exactly:
   every configuration with it JIT-eligible died, every configuration without
   it passed — including one with `org/springframework/, org/junit/, java/,
   org/assertj/, net/bytebuddy/` all compiled. Compiling a trampoline buys no
   throughput, so it is now skipped unconditionally.

5. **The native `Introspector.getBeanInfo` reported ERASED property types**
   for anything inherited from a generic supertype. `Person extends
   BaseEntity<Long>`, whose `getId()` erases to `Number`, answered
   `propertyType=Number` where HotSpot answers `Long`; and
   `PersonWithOverriddenGetter` — a `Long getId()` override over the inherited
   `setId(Number)` — lost its WRITE METHOD outright, because the
   setter-selection walk seeds from the getter's type and `Number` is not
   assignable to `Long` (`probes/BridgeProbe.java`, Spring gh-36019). Resolve
   accessor types against the BEAN class through the JDK's own public
   `com.sun.beans.TypeResolver`.

6. **...and the resolved type had nowhere to live.** `PropertyDescriptor`'s
   public `(String, Method, Method)` ctor derives `propertyType` against
   `getClass0()`, which that ctor leaves null until `setReadMethod` sets it to
   the READ METHOD'S DECLARING class — `BaseEntity`, not `Person`. HotSpot's
   `Introspector` never takes that path (it builds descriptors from
   `com.sun.beans.introspect.PropertyInfo`; JDK 25 dropped the package-private
   `(Class, String, Method, Method)` ctor that used to be the shortcut), so
   the resolved type is now stamped on explicitly.

7. **...and it inherited properties into sub-INTERFACES**, which
   `java.beans.Introspector` does not: `getBeanInfo(GenericService)` reports
   the `id` property, `getBeanInfo(SubGenericService extends GenericService)`
   reports nothing. The superinterface walk exists for interface DEFAULT
   methods reached through an implementing CLASS (jakarta.el's
   `TestBeanELResolver.testGetDefaultValue`), so it is now gated on the target
   not being an interface.

Plus one pure-throughput fix that closed two more classes on its own:
**`JitCache::invalidate_matching` did the full job even when nothing
matched.** It runs on EVERY class define, and almost no define invalidates a
CHA assumption — but with an empty match set it still made a whole-cache pass
computing the transitive reverse closure, walked every compiled method's
inline-cache slots to retarget them against an empty set, and CLONED all 128
shard maps only to drop them again. That was ~31% of total CPU on
`BeanRegistrationsAotContributionTests` (`invalidate_cached_targets` 12.8%,
`HashMap::clone` 8.0%, `drop_in_place<JitKey>` 5.6%,
`invalidate_for_class_change` 3.4%). Measured after: `GroovyScriptFactoryTests`
215 s → 82 s, `AutowiredAnnotationBeanRegistrationAotContributionTests`
188 s → 54 s, `core.io.ResourceTests` 12 s → 5 s.

**`FragmentViewResolutionResultHandlerTests` was never a correctness bug.**
Its failures were the cold Jython module parse overrunning the test's own
60 s `block(...)` deadline: `import string` (which pulls in `re`,
`sre_compile`, `sre_parse`, `codecs`, …) took **154 s against HotSpot's
0.275 s** — a 560× gap — and `--nojit` measured 178 s, i.e. the JIT was
contributing ~14% because only 4 of ~100 `PythonParser` rule methods compile
(they all carry exception tables). Fix #3 took the class 2/6 → 5/6 and the
JIT-cache fix took it 5/6 → 6/6. `probes/JythonProbe.java` is the standalone
measurement.

**One new debug lever: `CRATONVM_DBG_CLINIT_FAIL=1`** names the class AND the
exception the first time a `<clinit>` parks a class in `InitializationError`.
Every later use then raises a fresh `NoClassDefFoundError` from
`ensure_class_initialized_shared`, and that is all `CRATONVM_DBG_LINKAGE_BT`
can show — it names the consumer, never the original failure. It paid for
itself immediately on the `ExceptionUtils` mystery below.

## Closed in the fifth session (2026-07-29)

| class | before | after |
|---|--:|--:|
| `context.aot.ApplicationContextAotGeneratorTests` | TIMEOUT (no `RESULT` at 1500 s) | **40/40** in 395 s |
| `beans.factory.aot.BeanRegistrationsAotContributionTests` | TIMEOUT (no `RESULT` at 5400 s) | **14/14** in 10327 s |

**Neither was a timeout.** Both classes were SIGSEGVing partway through and the
runner, seeing no `RESULT` line, reported the wall-clock ceiling. Two distinct
crashes were behind it, and a third fix came out of the AOT residual below.

### The three fixes

1. **The moving-young verifier dereferenced a shadow-stack pointer read out of
   the WRONG JIT frame.** `shadow_window_from_frame` recovers a thread's
   published shadow window by reading `[rbp - cm.shadow_thread_slot_off]` and
   following the `JvmThread` pointer it finds there. That slot only holds a
   thread pointer when `cm` actually describes the frame standing at `rbp` —
   and it does not when an unguarded JIT→JIT call has published the callee's
   (deeper) frame base into the chain entry. `scan_one_frame_precise` already
   declines to publish an oop map under exactly that condition
   (`chain_entry_rbp_is_foreign`); the verifier did not check it at all. The
   only validation was `base != 0 && top >= base && (top - base) % 8 == 0`,
   which a spilled `long` or a `Value` discriminant clears constantly.
   `ApplicationContextAotGeneratorTests.processAheadOfTimeWithPropertySource`
   faulted on `addr=0xefc` about three minutes into every run. Now the verifier
   skips foreign-`rbp` entries (fail-closed: it forces the non-moving sweep for
   that cycle), and the window must satisfy the exact `#[repr(C)]` invariant
   `ShadowStack::ensure_allocated` establishes — three 8-aligned addresses,
   `base < end`, `end - base` exactly the fixed buffer size, `top` inside
   `[base, end]`. A concurrent session landed an equivalent fix on `dev` while
   this one was in flight; the merged result keeps the stricter checks of both.

2. **A retired JIT code buffer was unmapped while a frame was executing it.**
   Signature: a SIGSEGV whose `pc == addr` — an instruction-fetch fault, not a
   bad data access. Reproduced on
   `BeanRegistrationsAotContributionTests.applyToWithLessThanAThousandBeanDefinitionsDoesNotCreateSlices` at roughly one run in three; with
   `CRATONVM_DBG=jit-names` the crash handler names the faulting body as
   `com/sun/tools/javac/tree/TreeScanner.visitApply`, i.e. javac was running it
   while its in-process compilation of the AOT-generated sources kept defining
   classes and invalidating the JIT cache. Same family as the `NodeConnections`
   retired-code jump closed on 2026-07-27, different hole: that one was a caller
   holding a bare entry address, this one is a live frame — `JitEntryGuard::enter_with_compiled` records only a raw `*const CompiledMethod`, and its doc
   comment's claim that "the compiled method itself is kept alive by the JIT
   cache's Arc holding" is exactly the assumption `JitCache::put` breaks.
   **Landed on `dev` by a concurrent session** (the `defer_jit_owner`
   retirement queue plus `CRATONVM_DBG_JIT_UNMAP` / `CRATONVM_JIT_NEVER_FREE_CODE` diagnostics); this session's independent implementation was dropped in
   the merge in its favour after confirming it fixes the repro — **8/8 clean
   runs against 3 crashes in the 8 comparable runs before**.

3. **A type parameter whose bound is used by an EARLIER one lost its own
   bound.** `Base.class.getTypeParameters()[1].getBounds()` answered `[Object]`
   where HotSpot answers `[Something]`, for the shape
   `<T extends Thing<S>, S extends Something>` — and only for that shape;
   `<T extends Thing, S extends Something>` and `<T extends Thing<Something>>`
   were both already correct (`probes/TypeVarProbe2.java` prints all three side
   by side). Building `T` resolves the nested `S`;
   `resolve_declared_type_variable` will not re-enter `getTypeParameters()`
   while that list is under construction, so the fallback publishes an
   `Object`-bounded stand-in for `S` — correct as a placeholder, but nothing
   distinguished it from a finished parameter, so `getTypeParameters()` served
   it verbatim when the loop reached `S` and the declared bound was never built.
   Placeholders are now marked; `type_param_to_java` reuses and PATCHES the
   stand-in in place, which also repairs the reference already baked into `T`'s
   bound and preserves the object identity `com.sun.beans.TypeResolver`'s
   self-mapping check depends on. Found through `AotIntegrationTests` (below):
   Spring resolves a type-variable field to its bound via
   `ResolvableType.forField(field, testClass).resolve()`, got `null`, and
   `@MockitoBean S something` therefore matched every bean in the context —
   *"Unable to select a bean to override: found 17 beans of type ?"*.

**On the throughput residual.** Both closed classes are green but slow:
`ApplicationContextAotGeneratorTests` 395 s vs HotSpot's 31.6 s, and
`BeanRegistrationsAotContributionTests` 10327 s vs 26.9 s, of which
`applyToWithVeryLargeBeanDefinitionsCreatesSeparateSourceFiles` (10 001 bean
definitions through in-process javac) alone is 9446 s. A `perf` profile of that
run is flat — the largest single item is ~4% — and 2166 of the 3045 methods the
JIT compiles during the run are javac's own, so this is the separately tracked
interpreter-throughput gap and not a discrete defect. One profile-driven change
did land here: the conservative JIT-frame root scan walked the innermost frames
once per chain entry (all entries share `scanner_sp` as their low bound, so K
entries meant O(K·depth) work for a root set that is by construction the union)
and re-read the arena bounds through a backend dispatch for every stack word.
Scanning the union once, with the heap's address envelope hoisted out of the
loop, removed the profile's top three symbols —
`GenerationalHeap::is_object_address` (12.7%), `VmHeap::is_object_address`
(4.9%) and `scan_active_jit_frames_with_sp` (4.9%) — none of which appears in
the top twenty afterwards.

## Closed in the sixth session (2026-07-30)

| item | before | after |
|---|--:|--:|
| chunk 3 — `bean.override.BeanOverrideHandlerTests` (AssertJ soft assertions) | 39/42 | **42/42** (= HotSpot) |
| chunk 9 — `mockito.integration.MockitoSpyBeanAndSpringAopProxyIntegrationTests` (CGLIB proxy) | 4/4 **fail** | **8/8** for the chunk (= HotSpot) |
| chunk 6 | 31/33 | **33/33** — CratonVM now passes two the HotSpot harness itself fails |
| chunks 0, 19 | already matching | unchanged, still matching |

Single-class probes: `BeanOverrideHandlerTests` 16/19 → **19/19**,
`MockitoSpyBeanAndSpringAopProxyIntegrationTests` 0/4 → **4/4**.

### First: everything was broken, and not by this cluster

Every AOT probe on `origin/dev@c8da3d9188` died in **1.3 seconds** with

```
NullPointerException: Cannot invoke "org.apache.commons.logging.Log.isDebugEnabled()"
  because "this.logger" is null    at AbstractEnvironment.setActiveProfiles
```

Four layers deep: log4j-api's `StackLocator` walked the stack, a frame's
`getDeclaringClass()` answered **null**, the NPE escaped
`AbstractEnvironment`'s `logger` field initialiser, and
`construct_real_standard_environment` **swallowed it with `.ok()?`** — handing
Spring a synthetic `StandardEnvironment` allocated with no constructor at all.

`declaring_class_native` resolved a frame's declaring class by NAME through
`class_id_by_name`, which is `find_unique_class_by_name`: loader-blind AND
ambiguity-strict, so it answers `None` the moment two loaders define the name.
Under `@CompileWithForkedClassLoader` that is the normal state of every non-JDK
class. The frame's own ClassId was already available and already resolved to a
mirror by both frame builders — the accessors just never looked at it. They do
now, and the swallowed failure is reported instead of hidden.

### The two real items

**A CGLIB AOP proxy inherited the wrong copy of its superclass.** This is the
item the fifth session named and left open, and it turned out to be one
instance of a much older architectural gap:
`BUILTIN_LOADER_DELEGATION_CHAIN`'s doc comment said a user loader's parent
chain "is modelled on the Java side … the Rust side never observes a deep
parent walk for those". True for finding BYTES; never true for RESOLUTION
against already-defined classes, which is entirely Rust-side. A class defined
by `UserDefined(7)` could resolve a supertype only against its own namespace or
`Bootstrap → Extension → Application`; a name defined by its own PARENT
`UserDefined(3)` was invisible, so resolution fell through to the loader-blind
global path — which, for a name that is also on the application classpath,
**defines a second copy there** and links to it.

Reduced to a ~1 s standalone witness with no Spring at all
(`/data/data/pcprobe`, `ParentChainProbe`):

```
ParentLoader (child-first, parent = platform) defines its own Foo
ChildLoader  (parent = ParentLoader)          defines its own Bar extends Foo

HotSpot  : Bar.getSuperclass() == ParentLoader's Foo
CratonVM : Bar.getSuperclass() == the APPLICATION loader's Foo
```

`loaders.rs` now records user-loader parents (`USER_LOADER_PARENTS`, written by
the loader-namespace allocator, the only place that can see the Java
`ClassLoader.parent` field) and `class_manager` walks them in
`resolve_supertype` and in both requester-aware lookups, ahead of the built-in
chain and ahead of the global fallback. `CRATONVM_LOADER_PARENT_CHAIN=0`
restores the old behaviour, which is how the witness was A/B'd on one binary.

**AssertJ soft assertions could not build their ByteBuddy proxy.** Not a
ByteBuddy problem and not a generics problem: CratonVM's own ByteBuddy shims
in `test_frameworks.rs` built their return values with
`new_object_initialized(<literal name>, …)`, which takes only a name and so
resolved globally — returning the APPLICATION loader's copy whoever called.
Bytecode would have resolved that `new` through its own defining loader
(JVMS §5.4.3.1).

The mixed object graph breaks on the first enum comparison. `MgProbe4`, forked:

```
                       before                     HotSpot / after
  mCls     ForLoadedMethod   AppClassLoader       ForkedClassLoader
  sortCls  TypeDefinition$Sort AppClassLoader     ForkedClassLoader
  varCls   TypeDefinition$Sort ForkedClassLoader  ForkedClassLoader
  Sort.VARIABLE.equals(tv.getSort())   false      true
```

`MethodDescription$TypeSubstituting.getTypeVariables()` filters with
`ofSort(Sort.VARIABLE)`; two different `Sort` classes means the filter drops
every type variable, the generic method looks non-generic, and ByteBuddy cannot
attach `T` when writing the access bridge —
`IllegalArgumentException: Could not create type`. The shims now resolve
through `class_id_by_name_via_referencing_class` anchored on the receiver.

### The third item is not an AOT bug

chunk 4 (`mockito.constructor.MockitoBeanByTypeLookupForConstructorParameters…`)
still does not finish. `CRATONVM_DEFAULT_WATCHDOG_SEC=420` settles what it is:
the main thread is in `TestCompiler.compile` → in-process javac →
`JavaTokenizer` → … → `MockMethodAdvice` → `WeakConcurrentMap$LatentKey.hashCode`,
spinning, not blocked. That is exactly
[`mockito-redefine-makes-every-call-40us-20260726.md`](mockito-redefine-makes-every-call-40us-20260726.md),
open since 2026-07-26, and it is reproducible in 90 seconds with `SbCostProbe`
without Spring, JUnit or AOT anywhere. It should be tracked there, not here.

### Full 20-chunk sweep, `cratonvm-aot30-v7.bin`

All twenty chunks were re-run against the stored HotSpot baselines.
**19 of 20 match HotSpot exactly**; chunk 6 is 33/33 where the HotSpot harness
itself only manages 31/33. Chunk 4 is the only one that does not finish.

| chunk | CratonVM | HotSpot |
|--:|---|---|
| 0 | 38/38 | 38/38 |
| 1 | 37 found, 36 succ | same |
| 2 | 6 found, 1 succ | same |
| 3 | **42/42** (was 39/42) | 42/42 |
| 4 | **does not finish** | 25 found, 24 succ |
| 5 | 20/20 | 20/20 |
| 6 | **33/33** | 33 found, 31 succ |
| 7–19 | identical to HotSpot in every case | |

(The `succ < found` rows are HotSpot's own probe artefacts — the TestNG
engine's `ServiceConfigurationError` under the forked TCCL and
`non-public interface is not defined by the given loader`. They are equal on
both VMs, which is the point.)

## What is left

`test.context.aot.AotIntegrationTests#endToEndTestsForBeanOverrides`, chunk 4
only, blocked on the Mockito redefine-throughput issue above. Every other
chunk matches HotSpot or beats it.

## What the fifth session left (1 class)

`test.context.aot.AotIntegrationTests`, and specifically
`endToEndTestsForBeanOverrides`. It does **not** complete in 3 h on this host
(RSS climbs past 5.4 GB at `--Xmx 8g` and the AOT phase is silent), so it was
characterised instead with the per-chunk probe described under *Reproducing*
below: 150 classes grouped by top-level class into 20 chunks, run through
`AotE2EProbe` on HotSpot first and then on CratonVM.

**18 of the 20 chunks match HotSpot exactly**, including the six failures
HotSpot itself produces (all probe artefacts: the TestNG engine's
`ServiceConfigurationError` under the forked TCCL, and
`non-public interface is not defined by the given loader` from a proxy). Three
concrete items are left.

| item | where | state |
|---|---|---|
| A Spring CGLIB AOP proxy inherits the WRONG copy of its superclass | `mockito.integration.MockitoSpyBeanAndSpringAopProxyIntegrationTests` | **4/4 fail** (HotSpot 4/4 pass), root cause named — see below |
| AssertJ soft assertions cannot build their ByteBuddy proxy | `bean.override.BeanOverrideHandlerTests.forTestClassWith{SingleField,MultipleFields,MultipleFieldsWithIdenticalMetadata}` | **3 fail** in AOT replay only — the class passes **19/19** run normally. `IllegalArgumentException: Could not create type` from `net.bytebuddy.TypeCache.findOrInsert`, reached from `SoftProxies.createSoftAssertionProxyClass` |
| chunk 4 does not finish | `mockito.constructor.MockitoBeanByTypeLookupForConstructorParametersIntegrationTests` and neighbours | AOT processing completes (`PROBE aot-processing OK`) and then the replay hangs; killed at the 2400 s ceiling on three separate runs. HotSpot: 25 found, 24 succeeded, 1 failed (a probe artefact) |

The first two reproduce in **about a minute each** — see below. That is the
whole reason this section can name them instead of quoting a
`MultipleFailuresError` count.

### The CGLIB proxy's superclass comes from the wrong loader

`BeanOverrideTestExecutionListener.injectField` fails with
`BeanCreationException: Could not inject field … dateService`, caused by
`IllegalArgumentException: argument type mismatch` from `Field.set`.
`CRATONVM_DBG_COERCE=1` prints the expected/actual ClassIds, their loaders, and
(since 2026-07-29) the actual value's whole superclass chain:

```
expected  = …$DateService                    (cid=1899, loader=3)
arg_class = …$DateService$$SpringCGLIB$$1    (cid=7650, loader=7)
chain     = …$DateService$$SpringCGLIB$$1 (cid=7650, loader=7)
         -> …$DateService                 (cid=7629, loader=2)
         -> java/lang/Object                (cid=0,    loader=0)
```

So this is not a type error and not a `Field.set` bug: the proxy's superclass
resolved to a **second, freshly-defined copy of `DateService` in the
application loader** (`cid=7629`, a very high ClassId — it did not exist before
this run) instead of the copy the fork-loaded test class holds (`cid=1899`,
loader 3). `is_subclass` then correctly answers false. Whatever defines the
CGLIB proxy into namespace 7 resolves its superclass NAME through a delegation
chain that reaches the app loader rather than the fork, and CratonVM defines a
fresh class there rather than finding the fork's.

Start from what loader 7 is and what its recorded parent is, and from
`lookup_define.rs`'s `inherit_lookup_loader` / `define_class_full` — the
superclass ClassId recorded at define time is the evidence, not the coercion
guard that reports it. The same reject fires for log4j2's own plugin
construction (`expected=…/log4j/Level (loader=2)` vs
`arg_class=…/log4j/Level (loader=3)`), which is why every AOT run's log is full
of `Could not create plugin of type … LoggerConfig: argument type mismatch` and
`No factory method found for class … LoggerConfig`; that noise is very
probably the same defect seen from the other side, and closing this should
close it too.

## Reproducing

Whole classes, from a copy of the suite runner:

```bash
cd /data/data/spraot-runner
CRATONVM_BIN=/data/data/wt-spraot-20260728/localbin/cratonvm-spraot-v7.bin   CRATONVM_DEFAULT_HEAP_MAX_MB=6144 ./onea.sh <fqcn>
```

`onea.sh` runs one class and prints every failure (`KRUN_STACK=1` adds stacks);
`onet.sh <fqcn> [method]` is the same thing with per-test `START`/`DONE`
timings, which is what turns "TIMEOUT" into "test 13 of 14 took 9446 s";
`onem.sh` / `onep.sh` run a single method or a subset, and `hs.sh` / `hst.sh`
are the HotSpot equivalents — **always check HotSpot in this same checkout
before calling a failure a VM bug**.

`AotIntegrationTests#endToEndTestsForBeanOverrides` is not usable as a
debugging loop; use the per-class AOT probe instead:

```bash
cd /data/data/aot20260726
# one class (add its @Nested classes explicitly), ~1 minute
CP=$(tr -d '
' < …/spring-test/build/cratonvm-testcp.txt)
PROBE_STACK=1 <cratonvm> --java-home /home/victor/jdk25 --Xmx 3g   -cp "build2:build:$CP"   org.springframework.core.test.tools.ForkedProbeMain AotE2EProbe2 <Test…>
# all 150 classes, grouped by top-level class into 20 chunks
./bochunks2.sh hs                       # HotSpot baseline first
CRATONVM_BIN=<cratonvm> ./bochunks2.sh cv
```

`AotE2EProbe` is `AotIntegrationTests.runEndToEndTests` cut down to the named
classes; `AotE2EProbe2` is the same with `PROBE_STACK=1` support. The
`ForkedProbeMain` wrapper is mandatory (the generated
`__TestContext001_BeanDefinitions` classes touch package-private members of the
test class), and chunks **must** be grouped so a class and its `@Nested`
children stay together — splitting them fails on HotSpot too.

Levers that earned their keep this session: `CRATONVM_DBG=jit-names` (names the
compiled method a SIGSEGV faulted in — `pc == addr` means the code was
unmapped), `CRATONVM_DBG_COERCE=1` (prints the exact expected/actual ClassId and
loader behind an `argument type mismatch`), `CRATONVM_DBG_DUMP_JIT=LIST` (what
actually got compiled), and `CRATONVM_DBG_CLINIT_FAIL=1` from the fourth
session.
