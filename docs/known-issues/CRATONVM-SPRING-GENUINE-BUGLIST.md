# CratonVM Spring suite — genuine bug list

| | |
|---|---|
| **Status** | OPEN — **3 residual classes**, all AOT (was 9 after the third session, 19 before it, 57 before that, 127 before that). Thirteen VM bugs closed in the third session, seven more in the fourth; every one has a standalone HotSpot-vs-CratonVM probe. |
| **Captured** | 2026-07-27 (third session), branch `fix/spring-buglist-final-20260727` merged into `origin/dev` at `1f538bf76`, Azure host `20.83.144.174`, real JDK 25, worktree `/data/data/wt-sprbuglist-20260727`, binaries `localbin/cratonvm-sprfinal-v*.bin`. Every number below was measured with the class run **in isolation** (`apps/spring-suite-runner/onea.sh <fqcn>`), not from a sharded batch — the shared host runs at load 25–100 and batch runs emit spurious FAIL/TIMEOUT rows. |
| **Fourth session** | 2026-07-28, branch `fix/spring-nonaot-20260727` merged into `origin/dev`, worktree `/data/data/wt-spr-nonaot-20260727`, binaries `localbin/cratonvm-nonaot-v*.bin`. Took the five non-AOT residuals from LOADERR/0/2/2/6/26-27/165-170 to **fully green**, and turned the sixth (`RequestMappingMessageConversionIntegrationTests`) out to be a heap-sizing artifact rather than a linkage bug. See *Closed in the fourth session* below. |
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

## What is left (3 classes)

All three are AOT. Verified in isolation against
`localbin/cratonvm-nonaot-v12.bin` (branch `fix/spring-nonaot-20260727`,
merged to `origin/dev`).

| class | state | note |
|---|---|---|
| `test.context.aot.AotIntegrationTests` | 1/4 (1 fail, 2 skipped) | **Not a hang — the 1500 s ceiling was simply too low.** Re-measured at 3600 s: it completes. The array-identity `IllegalArgumentException` that used to end it at ~589 s is gone; what is left is `endToEndTestsForBeanOverrides`, which drives 175 test classes through a forked loader and reports `MultipleFailuresError` with **8** sub-failures — four bare `AssertionFailedError`s and four `BeanCreationException: Could not inject field …MockitoSpyBeanAndSpring…`. That is the same bean-override family the archived history tracks (13 failures at follow-up 10, Family A fixed at follow-up 11), now down to 8. Give it a ceiling above 1500 s or it reports a spurious TIMEOUT |
| `context.aot.ApplicationContextAotGeneratorTests` | TIMEOUT | measured: no `RESULT` line at a 1500 s ceiling |
| `beans.factory.aot.BeanRegistrationsAotContributionTests` | TIMEOUT | no `RESULT` line at a **5400 s** ceiling with a 6 GB heap, and none at 2400 s / 3000 s across three earlier builds (HotSpot: 14/14 in 25.8 s, so ≥209×). This is the separately tracked interpreter-throughput defect, not a discrete bug. The JIT-cache fix above removed the one algorithmic hotspot it had — a re-profile is now flat: interpreter execution ~9%, jimage/classpath resource lookup ~9%, allocator ~8%, nothing above 6.3%. It SIGSEGV'd under batch load earlier, so treat a crash there as a symptom of the slowness rather than a second bug |

`web.reactive.result.method.annotation.RequestMappingMessageConversionIntegrationTests`
is **off this list**: it passes **160/160** at a 6 GB heap. The long-standing
`NoClassDefFoundError: org/junit/platform/commons/util/ExceptionUtils` — "a
core JUnit-Platform class that is unconditionally on the classpath" — was an
`OutOfMemoryError` inside that class's `<clinit>`, which parked the class in
`InitializationError` so every later use raised `NoClassDefFoundError` from
`ensure_class_initialized_shared`. `CRATONVM_DBG_CLINIT_FAIL=1` named it in one
run. The archived history's guess of memory pressure was right after all; what
was wrong was ruling it out. The residual question is footprint, not linkage:
HotSpot runs this class under its default heap and CratonVM needs ~3× the
runner's 2 GB default.

## Reproducing

```bash
cd /data/data/spr-nonaot-runner   # a copy of apps/spring-suite-runner
CRATONVM_BIN=/data/data/wt-spr-nonaot-20260727/localbin/cratonvm-nonaot-v12.bin ./onea.sh <fqcn>
```

`onea.sh` runs one class and prints every failure (`KRUN_STACK=1` adds stacks);
`onem.sh <fqcn> <method>` runs a single method, `onep.sh <fqcn> <m1,m2,…>` a
subset (the two-method form is what isolates cross-test contamination), and
`hs.sh` / `hsm.sh` are the HotSpot equivalents — **always check HotSpot in this
same checkout before calling a failure a VM bug**. `seqrun.sh <listfile> <outdir>`
runs a list sequentially in isolation.
