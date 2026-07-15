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
*   **Current Residual (OPEN)**: The classes now complete but fail 0% of their sub-tests depending on the backend used:
  *   **Jetty**: `NoClassDefFoundError: org/eclipse/jetty/http/MimeTypes$Mutable`
  *   **Tomcat**: `IllegalStateException: org.apache.catalina.LifecycleException: Failed to initialize component [StandardServer[-1]]`
  *   **Reactor Netty**: `IllegalStateException: failed to create a child event loop`

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
