# CratonVM Spring Suite — Consolidated Open Bugs
**Latest Update: July 14, 2026**

## Executive Summary
Multiple major bug clusters have been successfully resolved (including the JIT SIGSEGV in Groovy, the `java.home` Locale regression, the `Semaphore` deadlock, and dozens of classloader visibility/AOT fixes).

This document tracks the **genuine remaining failures**.

---

## 1. Deep-Dive Investigations (Root-Caused, Pending Fix)

*   **Mockito `spy()` StackOverflowError** (`context.annotation.ImportSelectorTests`)
  *   **Status**: **OPEN** (Fails 4/9 tests)
  *   **Root Cause**: Mockito's `spy()` inline mock maker retransforms the class hierarchy; the `ThreadLocal`-based `MockMethodAdvice$SelfCallInfo.checkSelfCall` guard fails to match on CratonVM, causing infinite recursion.
  *   **Next Step**: Instrument `MockMethodDispatcher.get()` for distinct Class/Advice instances across the redefined hierarchy.
  *   **2026-07-15 (reactive-cluster session, mockk sibling analysis)**: the same self-call SOE family
    blocks ~10 Kotlin reactive test classes via **mockk** (not Mockito): `WebTestClientExtensionsTests`,
    `WebClientExtensionsTests`, `ServerResponseExtensionsTests`, `ServerRequestExtensionsTests`,
    `RenderingResponseExtensionsTests`, `ClientResponseExtensionsTests`, `RSocketRequesterExtensionsTests`,
    `WebClientObservationTests`, `CoExchangeFilterFunctionTests`, `InvocableHandlerMethodKotlinTests`. All
    fail identically on **pure origin/dev** (pre-existing, not a regression). Sharpened via mockk 1.14.5
    decompile + probes: the recursion is `mock.hashCode()` -> `JvmMockKProxyInterceptor.intercept` ->
    `JvmMockKDispatcher.get(id, mock)` returns the advice -> `BaseAdvice.handle` -> `BaseAdvice.handler`
    -> `handlers.get(mock)`, where `handlers` is a plain `Collections.synchronizedMap(LinkedHashMap)` (NOT
    identity-keyed -- confirmed from the no-arg `SynchronizedMockHandlersMap()` ctor bytecode), so
    `LinkedHashMap.get(mock)` calls `mock.hashCode()` again -> infinite. This recursion exists in the
    bytecode on BOTH VMs, yet HotSpot terminates -- so **CratonVM routes the ByteBuddy-generated mock's
    `hashCode()` through the interceptor where HotSpot dispatches to the real `Object.hashCode`**.
    **ThreadLocal is RULED OUT** (TLProbe on v13: identity-preserved, get-twice-same, withInitial, remove,
    guard-flip all correct). So the sibling "ThreadLocal guard fails to match" hypothesis above is likely
    WRONG for both -- the real divergence is ByteBuddy method-resolution/vtable: which methods of the
    generated mock subclass are instrumented vs left as real super-calls. The `SelfCallEliminator.isSelf`
    guard runs too LATE (inside `handler`, AFTER `handlers.get(mock)` already triggered the recursive
    `hashCode`). **Next step**: instrument, on a minimal mockk/Mockito mock, WHICH methods route to the
    interceptor on CratonVM vs HotSpot (esp. `hashCode`/`equals`/`toString`); the fix is almost certainly
    in how CratonVM resolves the mock subclass's inherited-vs-overridden method dispatch.
*   **`@Import` attribute CCE across `@CompileWithForkedClassLoader`** (`web.service.registry.ImportHttpServiceRegistrarTests`)
  *   **Status**: **OPEN** (2/5 methods: `basicListingWithAot`, `basicScanWithAot` — `ClassCastException: java.lang.Class cannot be cast to [Ljava.lang.String;` at `ConfigurationClassParser$SourceClass.getAnnotationAttributes`)
  *   **2026-07-15 update**: reproduces SOLO in ~1s (`/data/tmp/aotfix-runs/MethodRun.java` single-method launcher on the Azure host). The 2026-07-14 SoftReference/GC-relocation hypothesis is now DOUBTED: this failure survived eight classloader-identity fixes, and three focused probes (plain `@Import` reflection, forked-loader variant, `@Import` as meta-annotation on a repeatable annotation type — `ImportProbe2.java`) all PASS. The divergence is somewhere in the full `ConfigurationClassParser`/`MergedAnnotations` path for the repeatable `@ImportHttpServices` container under a forked loader.
*   **STOMP Message Hang** (`web.socket.messaging.StompWebSocketIntegrationTests`)
  *   **Status**: **OPEN** (TIMEOUT) — functional gap in STOMP routing/delivery, not a VM deadlock.

---

## 2. AOT / In-Memory Javac Cluster — LARGELY RESOLVED (2026-07-15)

The 11-class AOT cluster turned out to be a FAMILY of **classloader-identity
bugs**: `@CompileWithForkedClassLoader` re-defines the whole framework in a
fork loader (and every `TestCompiler` compile uses a fresh
`DynamicClassLoader`), so any name-global lookup inside the VM could resolve
the WRONG same-named copy. Eight fixes landed on
`fix/spring-aot-cluster-20260715` (commits `7c5aa7ce`, `5833302e`):

1. Link-time Pass-3 re-verification removed (name-indexed adapter threw
   spurious `VerifyError`s; define-time Pass 3 is loader-aware and
   authoritative). Also ~2x faster on javac-heavy tests.
2. `LambdaCallSite` records the loader-resolved functional-interface
   `ClassId` at indy bootstrap; non-SAM (default) interface methods on lambda
   proxies dispatch through it (fork-side `ArgumentCodeGenerator.and()`
   chains no longer produce app-side javapoet `TypeName`s).
3. `loader_namespace_id` keyed by loader OBJECT with GC reconcile (identity
   hashes recur; fresh per-compile loaders inherited dead siblings'
   namespaces -> stale `Test__Injector` CCEs).
4. Field/method/parameter annotation TYPES resolve through the declaring
   class's loader (fork-side `@Autowired` members no longer materialize
   app-loader annotation types that fail identity comparisons).
5. Jars appended via `Instrumentation.appendToBootstrapClassLoaderSearch`
   are recorded; the synthetic-mode loadClass defer-gate serves them via
   parent delegation.
6. `defineClass1` returns the already-defined BOOTSTRAP copy for
   appended-jar classes (Mockito `MockMethodDispatcher` null-loader assert).
7. `Class.forName` on Spring's `DynamicClassLoader` reroutes to the parent
   ONLY when the parent is the forked test loader (CGLIB `$$SpringCGLIB$$`
   classes defined into a plain DCL were CNFE-invisible).
8. JVMS 5.3 chain-scoped flat-store fallback in the synthetic-mode base
   delegation.

**Full-cluster validation (2026-07-15, one VM per class, real JDK 25, JIT on,
900 s watchdogs, results `/data/tmp/aotfix-runs/V7_shard*/results.tsv`):**

| class | doc baseline (2026-07-14) | now |
|---|---|---|
| `AutowiredAnnotationBeanRegistrationAotContributionTests` | FAIL 14/1 | **OK 14/14** |
| `CommonAnnotationBeanRegistrationAotContributionTests` | FAIL 8/1 | **OK 8/8** |
| `BeanDefinitionPropertiesCodeGeneratorTests` | FAIL 47/0 | **OK 47/47** |
| `InstanceSupplierCodeGeneratorTests` | FAIL 26/0 | **OK 26/24 (2 skip)** |
| `BeanDefinitionPropertyValueCodeGeneratorDelegatesTests` | LOADERR/ABEND | **OK 44/44** |
| `DefaultBeanRegistrationCodeFragmentsTests` | (was fixed) | **OK 19/19** |
| `GroupsMetadataValueDelegateTests` (WritableContent residual) | FAIL | **OK 8/8** |
| `ScopedProxyBeanRegistrationAotProcessorTests` | FAIL (3 methods) | **OK 5/5** |
| `PersistenceManagedTypesBeanRegistrationAotProcessorTests` | FAIL | FAIL 2/0 (see below, unrelated regression) |
| `TestClassScannerTests` | TIMEOUT 600 s | **completes 177 s** (7/7 or flaky 7/6) |
| `TestCompilerTests` | TIMEOUT 600 s+ | **completes 40 s**, FAIL 22/18/4 |
| `ApplicationContextAotGeneratorTests` | ABEND (CGLIB load) | discovers+runs 40 methods (see residuals) |
| `BeanDefinitionMethodGeneratorTests` | FAIL 34/3 | **OK 34/34** |
| `ConfigurationClassPostProcessorAotContributionTests` | FAIL 20/8 | FAIL 20/18/2 |
| `PersistenceAnnotationBeanPostProcessorAotContributionTests` | FAIL 8/0 (NCDFE) | FAIL 8/2/6 (Mockito attach residuals) |
| `TestContextAotGeneratorIntegrationTests` | FAIL 4/0 @393 s | (see residuals) |
| `BeanRegistrationsAotContributionTests` | TIMEOUT | TIMEOUT (throughput, see below) |

### Remaining OPEN residuals in the AOT cluster

*   `BeanRegistrationsAotContributionTests` — **TIMEOUT even at 1500 s**, 100%
    CPU, steadily progressing (NOT a deadlock). Stack samples put the main
    thread repeatedly in Mockito's inline-mock-maker constructor interception
    (`InlineDelegateByteBuddyMockMaker.lambda$new$2/3`) plus GC frame-root
    scanning — an interpreter-throughput problem under constructor
    instrumentation, needing perf work rather than a correctness fix.
*   `ConfigurationClassPostProcessorAotContributionTests` — 2 residuals in
    `BeanRegistrarTests` under fork: `IllegalArgumentException: parameter 0 of
    type ListableBeanFactory is not supported` (same DefaultMethodReference
    shape as the fixed lambda family, but NOT cured by the interpreter fix —
    JIT-path lambda dispatch or another identity split suspected).
*   `PersistenceAnnotationBeanPostProcessorAotContributionTests` — 8/2/6.
    Post-fix the forked Mockito path advanced: now (a) fork attach via
    `PremainAttachAccess` -> "Byte Buddy agent is not initialized", and (b) a
    NEW ByteBuddy generics failure past the dispatcher: `IllegalArgumentException:
    Cannot resolve T from class ...EntityManagerFactory$MockitoMock$...`.
*   `PersistenceManagedTypesBeanRegistrationAotProcessorTests` — **REGRESSED
    from OK 2/2 to FAIL 2/0** since this doc's 2026-07-14 baseline (unrelated
    to the 2026-07-15 orphaned-defining-loader fix below — reproduces
    byte-identically on a binary built *without* that fix, from the same
    tree). Both `processEntityManagerWithPackagesToScan` and
    `contributeJpaHints` now hit `NoClassDefFoundError:
    java/lang/classfile/ClassFile` inside
    `ClassFileMetadataReader.parseClassModel` — a JDK 24+ Class-File API
    (JEP 484) class that real-JDK-mode CratonVM apparently can no longer
    resolve. Needs its own investigation; not caused by any fix in this
    document.
*   `InstanceSupplierCodeGeneratorKotlinTests` — 4/0/5, all
    `ClassCastException: kotlin.reflect...protobuf.SmallSortedMap$Entry cannot
    be cast to java.lang.reflect.Field / AnnotationSpec` (separate
    heap/collection-identity family, kotlin-reflect metadata parsing).
*   `TestCompilerTests` — 4 residuals: package-private access via
    `@CompileWithTargetClassAccess`-style flows + additional-class references
    (`CompilationException: Unable to compile source`).
*   ~~`aot.nativex.feature.ThrowawayClassLoaderTests`~~ **FIXED (2026-07-15,
    commit `56a98cc4`)**. `native-builtins/src/classloader_real.rs`'s
    `cl_real_load_class_base` — the REAL-JDK-mode counterpart of the
    synthetic-mode function fixed earlier in this doc's round 2 — had the
    same missing JVMS 5.3 chain-scoping: `new ClassLoader(null){}.loadClass(x)`
    resolved app classes directly from the flat store even though the
    loader's real parent chain never reaches a built-in loader. Ported the
    same `scoped_user_chain` gate. Full class now 2/2 OK.
*   ~~`BeanDefinitionMethodGeneratorTests`~~ **FIXED (2026-07-15, commit
    `d017aa36`).** Bisection of the
    `generateBeanDefinitionMethodWhenHasExplicitResolvableType` residual
    (`MethodRun.java` accepts N method names to run together in one process)
    showed the failure was **COUNT-dependent, not content-dependent**: 9
    preceding `TestCompiler` compile cycles before the target passed; 10
    failed, regardless of which methods supplied the 9th/10th cycle. Root
    cause: `gc_reconcile_defining_loaders`
    (`native-builtins/src/classloader.rs`) DROPS a class's defining-loader
    registry entry once that loader is collected, and `cid_visible_mirror`
    reads a missing entry as "never restricted, visible to everyone" — the
    same answer it gives a class that was never loader-scoped. Once the Nth
    cycle's `DynamicClassLoader` was collected, its generated companion class
    silently became visible to every OTHER loader, so the next cycle's
    `DynamicClassLoader` reused the stale, wrong-scenario copy instead of
    generating its own. Fixed by tombstoning pruned class-ids in a permanent
    orphaned set, checked before the live-registry lookup, so a class whose
    defining loader died stays invisible to everyone forever (matches real
    unloading semantics). Full class now 34/34 OK. This is a DIFFERENT root
    cause than originally hypothesized here — it does NOT explain the
    `PersistenceAnnotationBeanPostProcessorAotContributionTests` ByteBuddy
    `NoSuchMethodError` family (confirmed unaffected, still 8/2/6 after this
    fix); that remains open and unrelated.

---

## 3. Untriaged Clusters & Per-Class Details

*Note: this section was accidentally truncated in the 2026-07-14 rewrite; the
full 1013-line per-class detail lives in git history as
`CRATONVM-SPRING-GENUINE-BUGLIST-125.md` (deleted in `ccab25c6`). 21 classes
related to `HIB-CV-32` heap corruption remain filtered out as load-dependent
side-effects tracked separately. The still-open non-AOT clusters from that
list (WebFlux backend failures, Groovy scripting cluster, WebFlux
EMPTY-discovery family, 6 found=0 ABENDs, and the per-class FAIL details)
are unchanged by the 2026-07-15 AOT work — consult the historical doc.*

### WebFlux Backend-Specific Failures
(`web.reactive.result.method.annotation.CrossOriginAnnotationIntegrationTests`, `RequestMappingMessageConversionIntegrationTests`)
*   **Update 2026-07-15 (reactive-cluster session, branch `fix/reactive-cluster-20260715`)**: the whole
    per-backend residual cluster (Jetty `MimeTypes$Mutable` NCDFE / Tomcat `StandardServer`
    LifecycleException / Reactor Netty "failed to create a child event loop") was root-caused to
    process-global poisoning chains and fixed: `CharBuffer.getArray` AIOOBE (missing `Buffer.address`
    seed on `asCharBuffer()` views) poisoned `jdk.internal.icu` → `java.net.IDN` → Netty's buffer
    stack; a JIT NPE in `sun.misc.Unsafe.putOrderedLong` (the native `<clinit>` shadow never populated
    `theInternalUnsafe`; the interpreter's C11/C16 null-receiver rescue papered over it but compiled
    code has no such rescue) killed `ByteBufUtil.<clinit>` at Netty's `MpmcArrayQueue(4096)` init loop.
    **`CrossOriginAnnotationIntegrationTests` is now fully OK (68/68)** on all backends.
*   **Current Residual (OPEN)**: `RequestMappingMessageConversionIntegrationTests` (160 tests) runs but
    is pathologically slow — steady progress (fresh `AnnotationConfigApplicationContext` + HTTP server
    per test; watchdog stack dumps show active bean creation, no deadlock) yet does not finish within
    1800s (HotSpot: 13s). Needs a dedicated perf investigation.

## 4. Reactive cluster session 2026-07-15 (branch `fix/reactive-cluster-20260715`)

Full 295-class reactive sweep (`web.reactive.*`, `messaging.rsocket.*`, `test.web.reactive.*`,
`http.server.reactive.*`, `http.client.reactive.*`, reactive tx/core classes) vs a same-day HotSpot
baseline. Baseline on the 2026-07-15 dev tip: 178 OK / 50 FAIL / 12 LOADERR / 5 ABEND / 4 TIMEOUT /
46 EMPTY. After the session's 7 VM fixes (missing `Buffer.address` on `asCharBuffer()` views;
`theInternalUnsafe` never populated by the `sun/misc/Unsafe` clinit shadow — JIT-compiled
`putOrderedLong` NPE; mutable-`ArrayList`-typed `Collections.EMPTY_LIST/MAP/SET` singletons that
kotlin-reflect's shaded protobuf mutated in place, corrupting `emptyList()` process-wide; speculative-BCE
loop-header guard missing the null-array check — freemarker `TemplateElement.setChildren` SIGSEGV; raw
pointer dereference of tagged Unsafe-arena handles in the TLS engine's direct-buffer accessors —
`SSLEngine.unwrap` SIGSEGV; `cratonvm/net/HttpBodyReplaySubscription` not declaring
`Flow$Subscription` — 40 sub-test failures on the `[2] JDK` WebClient connector; identity `finisher()`
on JOINING/COUNTING collectors when Reactor drives the raw Collector protocol) the sweep reaches
**HotSpot parity minus the residuals below** (the only EMPTY classes are the same 4 abstract classes
HotSpot reports EMPTY, and `ResourceWebHandlerTests` fails the same single
`servesResourcesFromFileSystem` test on both VMs).

An 8th fix landed during final validation: `java.net.URI`'s construction-time field writes
(`uri_store_named`) and the `getScheme`/`getRawSchemeSpecificPart` raw-string fallbacks treated ANY
first colon as a scheme delimiter, so a relative reference with a colon in its first path segment
(`/redirect:account`) parsed as `scheme="/redirect", path="account"`. Spring's view-resolution tests
derive the default view name from the request path, so `ViewResolutionResultHandlerTests.
defaultViewNameWithRedirectPrefixFails` (the FAIL this doc has tracked since the 516-class runs)
resolved the wrong view and completed instead of erroring. Scheme detection now mirrors the real JDK
parser (first stop char among `:/?#` must be `:`, ALPHA-start + alphanum/`+`/`-`/`.` name — the same
rule `uri_scheme_name_fail_index` already enforced for exceptions). The class is now 11/11 OK.

**RETRACTED 2026-07-15**: the "batch-context `<clinit>` contamination" theorized below (classes failing only in batched suite runs, passing solo) was investigated further and is **NOT a CratonVM bug**. Root cause was Azure-host environment corruption, confirmed live: (1) `/home/victor/jdk25` (the symlink itself) had vanished mid-session — every `--java-home ~/jdk25` invocation failed with `path does not exist`, producing zero test output, easily misread as a VM hang; (2) separately, `/tmp` (the directory itself, not just stale contents) had vanished — `Tomcat.initBaseDir()` threw `IllegalStateException: Unable to create the directory [/tmp]`, which looks exactly like a filesystem native bug but is the OS directory missing. After `ln -sf /data/data/jdk25-real ~/jdk25` and `mkdir -p /tmp && chmod 1777 /tmp`, the exact same 8-class batch that previously showed `SseIntegrationTests` failing 9/48 (deterministically, 3/3 reruns) now passes 42/48 with 6 aborted, byte-for-byte matching the HotSpot baseline shape, and `DefaultRenderingBuilderTests` never reproduced its `ExceptionInInitializerError` again across 3 clean reruns of the identical batch order on the identical binary. See `azure-host-disk-full-flapping-20260715.md` memory (Claude's memory system) for the full host instability catalog.

**Remaining OPEN reactive residuals:**
*   `http.client.reactive.ClientHttpConnectorTests` — TIMEOUT. `StepVerifier` in `basic()` waits forever;
    at dump time the Jetty client pool and MockWebServer-side threads are idle and no
    okhttp/MockWebServer accept thread is visible. Also: the T19.H1 watchdog stack-dump itself SIGSEGVs
    when JIT frames are on the stacks (separate small bug; `--nojit` dumps work).
*   `web.reactive.result.method.annotation.RequestMappingMessageConversionIntegrationTests` — pathological
    slowness, see section 2 above.
*   `web.reactive.result.view.script.JRubyScriptTemplateTests` — JRuby-on-CratonVM: Ruby
    `Symbol#to_s`/string interpolation returns empty inside `eval` heredocs
    (`rubygems/specification.rb` generates `@ = nil` from `"@#{key} = nil"`), so the engine bootstrap
    fails with a SyntaxError. Not reactive-specific; JRuby's embedding is its own bug family.
*   ~~Batch-context `<clinit>` contamination~~ — RETRACTED, see above (host environment issue: missing /tmp + missing ~/jdk25 symlink, not CratonVM).
