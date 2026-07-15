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
| `PersistenceManagedTypesBeanRegistrationAotProcessorTests` | FAIL | **OK 2/2** |
| `TestClassScannerTests` | TIMEOUT 600 s | **completes 177 s** (7/7 or flaky 7/6) |
| `TestCompilerTests` | TIMEOUT 600 s+ | **completes 40 s**, FAIL 22/18/4 |
| `ApplicationContextAotGeneratorTests` | ABEND (CGLIB load) | discovers+runs 40 methods (see residuals) |
| `BeanDefinitionMethodGeneratorTests` | FAIL 34/3 | FAIL 34/31/3 |
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
*   `BeanDefinitionMethodGeneratorTests` — 3 deterministic residuals:
    `NoSuchMethodError CustomBean__BeanDefinitions.getTestBeanDefinition` /
    `AnnotatedBean__BeanDefinitions.getTestInnerBeanBeanDefinition` (stale
    same-FQN generated class), plus one AssertionFailedError
    (`...HasExplicitResolvableType`). SOLO and PAIRWISE runs PASS — needs the
    full-class accumulated GC/JIT state to reproduce.
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
*   `InstanceSupplierCodeGeneratorKotlinTests` — 4/0/5, all
    `ClassCastException: kotlin.reflect...protobuf.SmallSortedMap$Entry cannot
    be cast to java.lang.reflect.Field / AnnotationSpec` (separate
    heap/collection-identity family, kotlin-reflect metadata parsing).
*   `TestCompilerTests` — 4 residuals: package-private access via
    `@CompileWithTargetClassAccess`-style flows + additional-class references
    (`CompilationException: Unable to compile source`).
*   `aot.nativex.feature.ThrowawayClassLoaderTests` — 1 residual
    (`InputStream closed` contract). Minimal repro `TCLProbe.java`:
    `new ClassLoader(null){}.loadClass(<app class>)` RESOLVES the app class
    under CratonVM real mode where HotSpot throws CNFE, so the loader's
    resource fallback never runs. The leaking surface is inside the
    real-bytecode `loadClass(String,boolean)` chain (not forName / JLA probe /
    findBootstrapClass native / findLoadedClass native — all traced clean).

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
*   **Current Residual (OPEN)**: classes complete but sub-tests fail depending on backend:
  *   **Jetty**: `NoClassDefFoundError: org/eclipse/jetty/http/MimeTypes$Mutable`
  *   **Tomcat**: `IllegalStateException: ... Failed to initialize component [StandardServer[-1]]`
  *   **Reactor Netty**: `IllegalStateException: failed to create a child event loop`
