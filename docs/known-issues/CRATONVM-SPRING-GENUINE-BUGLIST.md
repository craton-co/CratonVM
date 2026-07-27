# CratonVM Spring suite — genuine bug list

| | |
|---|---|
| **Status** | OPEN — **8 residual classes** (was 19 at the start of this session, 57 the session before, 127 the one before that). Eleven VM bugs closed here; every one has a standalone HotSpot-vs-CratonVM probe. |
| **Captured** | 2026-07-27 (third session), branch `fix/spring-buglist-final-20260727` merged into `origin/dev` at `1f538bf76`, Azure host `20.83.144.174`, real JDK 25, worktree `/data/data/wt-sprbuglist-20260727`, binaries `localbin/cratonvm-sprfinal-v*.bin`. Every number below was measured with the class run **in isolation** (`apps/spring-suite-runner/onea.sh <fqcn>`), not from a sharded batch — the shared host runs at load 25–100 and batch runs emit spurious FAIL/TIMEOUT rows. |
| **History** | Everything before this session is archived in [`../internal/fixed-suite-bugs/spring/CRATONVM-SPRING-GENUINE-BUGLIST-history-20260727.md`](../internal/fixed-suite-bugs/spring/CRATONVM-SPRING-GENUINE-BUGLIST-history-20260727.md). Its conclusions are superseded by the entries below wherever the two disagree. |

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

Plus **`ProcessHandle.Info.command()`** now reports this VM's own executable
instead of an empty `Optional` (the standard "find my JVM and spawn a child"
idiom raised `NoSuchElementException`), which is what took
`PathMatchingResourcePatternResolverTests` the last two tests to 22/22.

Three debug levers were added along the way, because their absence is what made
the last two bugs slow to find: `CRATONVM_DBG_LINKAGE_BT=1` now also fires at
`raise_no_class_def_found` and at the JIT dispatch-error mapper (previously only
`linkage_throwable`), and `CRATONVM_DBG_STUB_BT` also covers
`ensure_synthetic_class`.

## What is left (8 classes)

Verified in isolation against `cratonvm-sprfinal-v15.bin` (branch merged to
`origin/dev` `1f538bf76`).

| class | state | note |
|---|---|---|
| `beans.PropertyDescriptorUtilsPropertyResolutionTests` | LOADERR | `OutOfMemoryError` after ~100s. The Java stack is a repeating cycle — `ClassTemplateTestDescriptor.execute` → `TemplateExecutor.execute` → `TestMethodTestDescriptor.cleanUp` → `invokeTestInstancePreDestroyCallbacks` → `CallbackSupport.invokeAfterCallbacks` → `AutoCloseExtension.preDestroyTestInstance` → `AutoCloseExtension.closeFields` → back to `ClassTemplateTestDescriptor.execute` — which is not a call graph JUnit has: a functional-interface dispatch is landing on the wrong target and looping. Reproduces at 2 GB and 8 GB heap, so it is a genuine allocation blow-up, not host pressure. The class is a JUnit 5 `@ParameterizedClass` + `@FieldSource` with `@Nested` children |
| `orm.jpa.support.PersistenceInjectionTests` | 26/27 | `publicExtendedPersistenceContextSetterWithSerialization`. NOT plain proxy serialization: a JDK dynamic proxy with a `Serializable` handler round-trips correctly and still dispatches to its handler (`probes/ProxySerProbe.java` matches HotSpot). The gap is narrower — a `SimpleMapScope` destruction callback wrapping an `ExtendedEntityManagerCreator` proxy must survive Java serialization and still invoke `close()` |
| `test.context.junit.jupiter.event.ParallelApplicationEventsIntegrationTests` | 0/2 | Both are JUnit-parallel-execution × Spring `ApplicationEvents`. `rejectTestsInParallelWithInstancePerClassAndRecordApplicationEvents` runs a nested `EngineTestKit` engine with `CONCURRENT` mode and expects exactly one FAILED event; CratonVM produces zero, i.e. the guard Spring is supposed to trip never fires — most likely because the nested engine is not actually executing concurrently |
| `web.reactive.result.view.FragmentViewResolutionResultHandlerTests` | 2/6 | All four failures are the `FluxSubscribeOn` variants; the two non-flux ones pass. `renderFragmentStream` alone: HotSpot passes, CratonVM times out on the 60 s `block(...)`, identically with `--nojit`. Reactor's schedulers themselves are fine (`probes/BoundedElasticProbe.java` — `parallel`/`single`/`boundedElastic` all match HotSpot), so the hang is in the SSE render path executed on the elastic worker, not in the scheduler. **Was 5/6 in the previous session's baseline and was already 2/6 in this session's pre-fix baseline — a regression from `origin/dev` drift, not from these fixes** |
| `web.reactive.function.client.WebClientIntegrationTests` | 165/170 | Four `VerifySubscriber timed out` across the Reactor-Netty / JDK / Jetty parameterisations. Needs a re-measure on a quiet host before being treated as a VM defect — the host ran at load 25–100 throughout |
| `test.context.aot.AotIntegrationTests` | 1/4 (2 skipped) | `endToEndTestsForBeanOverrides`: `IllegalArgumentException: Unable to adapt value of type ContextConfiguration[] to …` — the same array-class-identity family as the row below |
| `test.context.aot.TestContextAotGeneratorIntegrationTests` | 2/4 | **Root-caused, not fixed.** Both failures end in `IllegalStateException: Attribute 'method' in annotation …RequestMapping should be compatible with …RequestMethod[] but a …RequestMethod[] value was returned` — two same-named-but-different `RequestMethod[]` array classes. Enum VALUES already resolve through the container loader; what does not is the ARRAY class, because `synthesize_array_class` caches ONE array class GLOBALLY per descriptor name (and deliberately pins `loader_id == Bootstrap`, guarded by a `debug_assert_eq!` from a Round-7 audit). Fixing it means either per-(loader, name) array-class caching or wiring up the permanently-`None` `array_info` field — read that audit's history first. Was a SIGSEGV before this session's fixes |
| `context.aot.ApplicationContextAotGeneratorTests` | TIMEOUT | not re-measured to completion this session |

Two more that were on the list are **not** CratonVM bugs and need no further work:

- `context.annotation.ConfigurationClassEnhancerTests.withPublicClass` — fails
  identically on HotSpot in this checkout (`apps/spring-suite-runner/hs.sh`).
- `aot.nativex.FileNativeConfigurationWriterTests` — fixture artifact, see the
  archived history.

Not re-run this session because the previous session closed them and nothing
here touches their area: `beans.factory.aot.BeanRegistrationsAotContributionTests`
(the separately tracked ~227×-vs-HotSpot interpreter throughput defect, which
also SIGSEGVs under batch load) and
`web.reactive.result.method.annotation.RequestMappingMessageConversionIntegrationTests`.

## Reproducing

```bash
cd /data/data/wt-sprbuglist-20260727/apps/spring-suite-runner
CRATONVM_BIN=/data/data/wt-sprbuglist-20260727/localbin/cratonvm-sprfinal-v15.bin ./onea.sh <fqcn>
```

`onea.sh` runs one class and prints every failure (`KRUN_STACK=1` adds stacks);
`onem.sh <fqcn> <method>` runs a single method, `onep.sh <fqcn> <m1,m2,…>` a
subset (the two-method form is what isolates cross-test contamination), and
`hs.sh` / `hsm.sh` are the HotSpot equivalents — **always check HotSpot in this
same checkout before calling a failure a VM bug**. `seqrun.sh <listfile> <outdir>`
runs a list sequentially in isolation.
