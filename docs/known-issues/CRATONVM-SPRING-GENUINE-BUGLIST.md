# CratonVM Spring Suite — Consolidated Open Bugs
**Latest Update: July 14, 2026**

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
The original 1500-second infinite hangs have largely shifted into execution failures after recent file-manager and compiler fixes.
*   **Still Hanging (TIMEOUT at 600s)**:
  *   `beans.factory.aot.BeanRegistrationsAotContributionTests`
  *   `test.context.aot.TestClassScannerTests`
  *   `core.test.tools.TestCompilerTests`
*   **Residual Failures (Reaches execution but fails)**:
  *   `beans.factory.annotation.AutowiredAnnotationBeanRegistrationAotContributionTests`
  *   `beans.factory.aot.BeanDefinitionMethodGeneratorTests`
  *   `beans.factory.aot.BeanDefinitionPropertiesCodeGeneratorTests`
  *   `beans.factory.aot.InstanceSupplierCodeGeneratorTests`
  *   `context.annotation.CommonAnnotationBeanRegistrationAotContributionTests`
  *   `context.annotation.ConfigurationClassPostProcessorAotContributionTests`
  *   `test.context.aot.TestContextAotGeneratorIntegrationTests` (Fails after 393s)
  *   `orm.jpa.support.PersistenceAnnotationBeanPostProcessorAotContributionTests` (Fails with `NoClassDefFoundError: org/springframework/orm/jpa/support/PersistenceAnnotationBeanPostProcessor`)
  *   `context.aot.ApplicationContextAotGeneratorTests` (ABENDs while loading generated Spring CGLIB code)
*   **WritableContent Residual**:
  *   `web.service.registry.GroupsMetadataValueDelegateTests`: VM Abort fixed; now fails with `IllegalStateException: WritableContent did not append any content`.

---

## 3. Untriaged Clusters & Per-Class Details

The following tests are genuinely open (FAIL, ABEND, or TIMEOUT). *Note: 21 classes related to the `HIB-CV-32` heap corruption known-issue have been filtered out of this list as they are load-dependent side-effects tracked under a separate ticket.*

**AOP / Beans**
*   `aop.scope.ScopedProxyBeanRegistrationAotProcessorTests` - FAIL (`BeanCreationException` / `CompilationException`)
*   `aot.nativex.feature.ThrowawayClassLoaderTests` - FAIL (`InputStream closed` AssertionError)
*   `beans.PropertyDescriptorUtilsPropertyResolutionTests` - FAIL (AssertJ multiple failures)
*   `beans.factory.DefaultListableBeanFactoryTests` - FAIL (`BeanCreationException` during autowiring)
*   `beans.factory.aot.BeanDefinitionPropertyValueCodeGeneratorDelegatesTests` - LOADERR (`OutOfMemoryError: Java heap space`)
*   `beans.factory.aot.InstanceSupplierCodeGeneratorKotlinTests` - FAIL (`CompilationException`)
*   `beans.factory.xml.XmlBeanFactoryTests` - ABEND (Uncaptured exception in main-vm)

**Context / Core**
*   `context.annotation.ConfigurationClassPostConstructAndAutowiringTests` - FAIL (`UnsatisfiedDependencyException`)
*   `context.annotation.ConfigurationClassPostProcessorTests` - ABEND (CGLIB enhancer crash)
*   `context.annotation.Spr15275Tests` - FAIL (AssertionError)
*   `context.annotation.Spr6602Tests` - FAIL (AssertionFailedError)
*   `core.GenericTypeResolverTests` - FAIL (AssertionFailedError)
*   `core.annotation.MergedAnnotationsTests` - FAIL (AssertionError)
*   `core.codec.ResourceRegionEncoderTests` - ABEND (Crashes immediately before test execution)
*   `core.convert.converter.DefaultConversionServiceTests` - ABEND (rc=139)

**Expression / HTTP / JDBC / JMS / Scheduling**
*   `expression.spel.EvaluationTests` - FAIL (AssertionError)
*   `http.codec.multipart.DefaultPartHttpMessageReaderTests` - ABEND (rc=139)
*   `http.codec.multipart.MultipartHttpMessageWriterTests` - ABEND (rc=139)
*   `jms.listener.MessageListenerContainerObservationTests` - ABEND (rc=139)
*   `scheduling.quartz.QuartzSupportTests` - TIMEOUT (Hard timeout)

**Groovy / Jython / Scripting Cluster**
*   `context.groovy.GroovyBeanDefinitionReaderTests` - ABEND (rc=139)
*   `scripting.config.ScriptingDefaultsTests` - ABEND (rc=139)
*   `scripting.groovy.GroovyAspectIntegrationTests` - ABEND (rc=139)
*   `scripting.groovy.GroovyAspectTests` - ABEND (rc=139)
*   `web.reactive.result.view.FragmentViewResolutionResultHandlerTests` - ABEND (Jython linkage error: `failed to parse StackMapTable`)
*   `web.reactive.result.view.script.JRubyScriptTemplateTests` - ABEND (rc=139)
*   `web.reactive.result.view.script.JythonScriptTemplateTests` - FAIL (`Failed to evaluate script`)
*   `web.servlet.mvc.method.annotation.FragmentRenderingStreamTests` - ABEND (Jython linkage error: `failed to parse StackMapTable`)
*   `web.servlet.view.DefaultFragmentsRenderingTests` - FAIL (`Failed to evaluate script`)
*   `web.servlet.view.script.JRubyScriptTemplateTests` - FAIL (`Failed to evaluate script`)
*   `web.servlet.view.script.JythonScriptTemplateTests` - FAIL (AssertionFailedError)

**ORM / JPA**
*   `orm.jpa.persistenceunit.PersistenceManagedTypesBeanRegistrationAotProcessorTests` - FAIL (`CompilationException`)
*   `orm.jpa.support.InjectionCodeGeneratorTests` - FAIL (`CompilationException`)
*   `orm.jpa.support.PersistenceInjectionTests` - FAIL (AssertionFailedError)

**Test Framework / Mocking**
*   `test.context.aot.AotIntegrationTests` - FAIL (`ArrayIndexOutOfBoundsException: null`)
*   `test.context.junit.jupiter.event.ParallelApplicationEventsIntegrationTests` - FAIL (AssertionError in event statistics)
*   `test.web.servlet.assertj.MockMvcTesterIntegrationTests` - FAIL (AssertionError in debug streams)
*   `test.web.servlet.htmlunit.MockWebResponseBuilderTests` - FAIL (AssertionFailedError)
*   `util.function.SingletonSupplierTests` - FAIL (AssertionError)

**Web / WebFlux / WebSockets**
*   `web.context.ContextLoaderTests` - ABEND (Exception in main-vm)
*   `web.reactive.result.method.annotation.CoroutinesIntegrationTests` - ABEND (rc=139)
*   `web.reactive.result.method.annotation.JacksonHintsIntegrationTests` - ABEND (rc=139)
*   `web.reactive.result.method.annotation.MessageReaderArgumentResolverTests` - ABEND (rc=134)
*   `web.reactive.result.method.annotation.MessageWriterResultHandlerTests` - ABEND (rc=139)
*   `web.reactive.result.method.annotation.ProtobufIntegrationTests` - ABEND (rc=1)
*   `web.reactive.result.method.annotation.RequestMappingExceptionHandlingIntegrationTests` - ABEND (rc=139)
*   `web.reactive.result.view.ViewResolutionResultHandlerTests` - FAIL (Expected `ResponseStatusException`, actual `onComplete()`)
*   `web.server.session.WebSessionIntegrationTests` - ABEND (rc=139)
*   `web.servlet.config.MvcNamespaceTests` - ABEND (rc=139)
*   `web.servlet.config.annotation.ViewResolutionIntegrationTests` - ABEND (rc=139)
*   `web.socket.config.MessageBrokerBeanDefinitionParserTests` - FAIL (AssertionFailedError)
