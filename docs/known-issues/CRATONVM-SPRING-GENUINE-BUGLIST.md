# CratonVM Spring Suite — Consolidated Open Bugs
**Latest Update: July 15, 2026**

## Executive Summary
Multiple major bug clusters have been successfully resolved (including the JIT SIGSEGV in Groovy, the `java.home` Locale regression, the `Semaphore` deadlock, and dozens of classloader visibility/AOT fixes).

This document tracks the **genuine remaining failures**.

---

## 1. Deep-Dive Investigations (Root-Caused, Pending Fix)

These failures have been deeply investigated and their precise mechanisms identified, but they remain open awaiting a fix.

*   **Mockito `spy()` StackOverflowError** (`context.annotation.ImportSelectorTests`)
  *   **Status**: **OPEN** (Fails 4/9 tests)
  *   **Root Cause**: This is a Mockito/ByteBuddy bug, not a Spring bug. Mockito's `spy()` uses an inline mock maker that retransforms the class hierarchy. A `ThreadLocal`-based guard (`MockMethodAdvice$SelfCallInfo.checkSelfCall`) designed to prevent infinite recursion on `CALLS_REAL_METHODS` fails to match the ThreadLocal object on CratonVM, causing an infinite loop.
  *   **Next Step**: Instrument `MockMethodDispatcher.get()` to see if CratonVM is minting multiple distinct Class objects/Advice instances for the redefined hierarchy.
*   **AOT / Forked ClassLoader Cache Bug** (`web.service.registry.ImportHttpServiceRegistrarTests`)
  *   **Status**: **OPEN** (Fails 2/5 tests with `ClassCastException: java.lang.Class cannot be cast to [Ljava.lang.String;`)
  *   **Root Cause**: Occurs exactly when Spring's `ConfigurationClassParser` reads `@Import` annotations across a `@CompileWithForkedClassLoader` boundary.
  *   **Hypothesis**: Spring's `AnnotationTypeMappings` caches metadata behind `SoftReference`s. Suspected GC/Reference relocation bug in CratonVM where a soft-reference read-back yields a corrupted/wrong-typed object after collection.
*   **STOMP Message Hang** (`web.socket.messaging.StompWebSocketIntegrationTests`)
  *   **Status**: **OPEN** (TIMEOUT)
  *   **Root Cause**: The original bean startup failure and a underlying GC-safepoint VM bug were fixed. However, the test now gets further, opens a STOMP connection, and genuinely hangs waiting for a STOMP message from the broker that never arrives. Functional gap in routing/delivery, not a VM deadlock.

---

## 2. Updated Status on Partially Fixed Clusters (Still OPEN)

These classes had blocking VM crashes fixed, but now fail on new, distinct residuals.

### WebFlux Backend-Specific Failures
(`web.reactive.result.method.annotation.CrossOriginAnnotationIntegrationTests`, `RequestMappingMessageConversionIntegrationTests`)
*   **Update**: The missing `ApiVersionStrategy` bean, the `Semaphore.release()` deadlock, the `Objects.toString` defect, and a `CopyOnWriteArrayList` NPE were all fixed.
*   **Update 2026-07-15 (reactive-cluster session, branch `fix/reactive-cluster-20260715`)**: the whole
    per-backend residual cluster was root-caused to process-global poisoning chains and fixed:
    `CharBuffer.getArray` AIOOBE (missing `Buffer.address` seed on `asCharBuffer()` views) poisoned
    `jdk.internal.icu` → `java.net.IDN` → Netty's buffer stack; a JIT NPE in `sun.misc.Unsafe.putOrderedLong`
    (the native `<clinit>` shadow never populated `theInternalUnsafe`; the interpreter's C11/C16 null-receiver
    rescue papered over it but compiled code has no such rescue) killed `ByteBufUtil.<clinit>` at Netty's
    `MpmcArrayQueue(4096)` init loop. **`CrossOriginAnnotationIntegrationTests` is now fully OK (68/68)**
    on all backends.
*   **Current Residual (OPEN)**: `RequestMappingMessageConversionIntegrationTests` (160 tests) runs but is
    pathologically slow — it makes steady progress (fresh `AnnotationConfigApplicationContext` + HTTP server
    per test; watchdog stack dumps show active bean creation, no deadlock) yet does not finish within 1800s
    (HotSpot: 13s). Needs a dedicated perf investigation.

### AOT / In-Memory Javac Residuals
The original 1500-second infinite hangs have largely shifted into execution
failures after recent file-manager and compiler fixes. The most recent
11-class AOT-cluster batch on the pre-merge `dev` baseline (`b7971c50`) still
contained the residuals below; the source branch was subsequently fast-forwarded
to `3a6bd4a6` for follow-up work, but that newer commit has not yet had a fresh
full AOT batch.
*   **Still Hanging (TIMEOUT at 600s)**:
  *   `beans.factory.aot.BeanRegistrationsAotContributionTests`
  *   `test.context.aot.TestClassScannerTests`
  *   `core.test.tools.TestCompilerTests`
*   **Residual Failures (Reaches execution but fails)**:
  *   `beans.factory.annotation.AutowiredAnnotationBeanRegistrationAotContributionTests`
*   `beans.factory.aot.BeanDefinitionMethodGeneratorTests` — retained pending
    a fresh batch: its direct failing method passed when invoked reflectively
    on the same release VM and Spring classpath, contradicting the older batch
    summary.
  *   `beans.factory.aot.BeanDefinitionPropertiesCodeGeneratorTests`
  *   `beans.factory.aot.InstanceSupplierCodeGeneratorTests`
  *   `context.annotation.CommonAnnotationBeanRegistrationAotContributionTests`
  *   `context.annotation.ConfigurationClassPostProcessorAotContributionTests`
  *   `test.context.aot.TestContextAotGeneratorIntegrationTests` (Fails after 393s)
  *   `orm.jpa.support.PersistenceAnnotationBeanPostProcessorAotContributionTests` (Fails with `NoClassDefFoundError: org/springframework/orm/jpa/support/PersistenceAnnotationBeanPostProcessor`)
  *   `context.aot.ApplicationContextAotGeneratorTests` (ABENDs while loading generated Spring CGLIB code)
*   **WritableContent Residual**:
  *   `web.service.registry.GroupsMetadataValueDelegateTests`: VM Abort fixed; now fails with `IllegalStateException: WritableContent did not append any content`.

#### July 15 AOT evidence and rejected hypotheses

* `TestCompilerTests` still reached the 90-second watchdog while javac scanned
  system modules. Its captured stack was in `ClassFinder.scanModulePaths`,
  through Spring's `DynamicJavaFileManager`, ending in
  `JavacFileManager.inferBinaryName` or `getJavaFileForInput` depending on the
  diagnostic build. A 600-second diagnostic also failed to complete.
* The broad `WritableContent` symptom is **not** a generic lambda, captured
  method-reference, or `ThrowingConsumer<Appendable>` dispatch failure. A
  focused probe passed both `SourceFile.of(javaFile::writeTo)` and the complete
  `JavaFile::writeTo → ThrowingConsumer → AppendableConsumerInputStreamSource`
  generated-files path on the same release VM. The residual therefore remains
  open in the real generated-code graph rather than in the generic callback
  mechanism.
* A direct invocation of
  `BeanDefinitionMethodGeneratorTests.generateBeanDefinitionMethodWhenHasInstancePostProcessorGeneratesMethod`
  completed successfully on that release VM. This is useful reconciliation
  evidence, but does not prove the class fixed because the full batch had
  reported four method failures and has not been rerun after the check.
* Two JRT/javac performance experiments were rejected and are not committed:
  directly reading `JrtPath.path` did not clear the watchdog, and directly
  constructing `JRTFileObject`s advanced execution but ultimately recurred in
  package resolution and timed out. The branch contains no experimental VM
  source changes from these probes.

No AOT item was removed in this update: no full-class or full-cluster rerun
has yet proven an AOT residual fixed.

---

## 3. Untriaged Clusters & Per-Class Details

The following tests are genuinely open (FAIL, ABEND, or TIMEOUT). *Note: 21 classes related to the `HIB-CV-32` heap corruption known-issue have been filtered out of this list as they are load-dependent side-effects tracked under a separate ticket.*

**AOP / Beans**
*   `aop.scope.ScopedProxyBeanRegistrationAotProcessorTests` - FAIL (`BeanCreationException` / `CompilationException`)
*   `aot.nativex.fea


---

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

Classes that FAIL in *batched* suite runs but pass solo at HotSpot parity (batch-context
contamination — a prior class in the shared VM poisons a `<clinit>`; not yet root-caused, likely one
more cross-class-state bug): `SseIntegrationTests` (solo 48 found / 42 succ / 6 aborted == HotSpot),
`WebSocketIntegrationTests` (solo 72/72), `DefaultRenderingBuilderTests` (solo 11/11, batched shows
`ExceptionInInitializerError` → `NoClassDefFoundError: ViewResolverSupport` on the redirect tests).

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
*   Batch-context `<clinit>` contamination (see above) — affects SSE/WebSocket/DefaultRenderingBuilder
    only when many reactive classes share one VM; every one of them is green solo.
