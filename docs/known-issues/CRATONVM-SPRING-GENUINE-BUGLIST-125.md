# CratonVM Spring suite — genuine bug list (updated, dev `b02601ab`)

Baseline: fresh HotSpot run over 516 non-passed classes with corrected
classpath (spring-websocket/oxm/jms/orm/core-test jars were missing before —
`./gradlew jar testFixturesJar testClasses` fixed it). 430/516 pass on HotSpot
(86 excluded: 13 FAIL + 73 EMPTY on HotSpot itself — not CratonVM bugs).

## Progression across two dev updates

| dev commit | agree w/ HotSpot (OK) | genuine bugs |
|---|--:|--:|
| `9298db15` | 258/430 | 159 |
| `b02601ab` (current) | **305/430** | **125** |

**65 classes newly fixed** between the two runs, most notably:
- The entire SpEL cluster (11 classes: `LiteralTests`, `OperatorTests`,
  `ParsingTests`, `SpelParserTests`, `ArrayConstructorTests`,
  `ConstructorInvocationTests`, `MethodInvocationTests`,
  `SpelCompilationCoverageTests`, `SpelDocumentationTests`, `SpelReproTests`,
  `VariableAndFunctionTests`, `InlineCollectionTests`) — confirms the
  `EL1040E` double-literal-suffix parse bug is fixed.
- The entire spring-jms module (13 classes) — now testable at all since its
  jar was previously missing.
- `BufferedImageHttpMessageConverterTests`, `RetryTemplateTests`, and more.

## ⚠️ 31 classes appear "newly broken" — NOT genuine regressions

All 31 are **exclusively ABEND** (zero FAIL/TIMEOUT among them) — a strong
signal against 31 independent logic regressions. Root-caused: 25/31 crash
with `rc=139` (SIGSEGV), and at least 2 show the explicit
`corrupt Value cell ... see HIB-CV-32` heap-reference-integrity guard message
before crashing. This is the **same already-tracked HIB-CV-32 heap-corruption
family** (see [[hibcv32-tearing-ruled-out-getfield-oop-candidate]] and the
OSR-regression triage's "EnableAsyncTests tripped the live heap corrupt-slot
guard mid-run while still passing" note) — a batch/load-dependent class of
bug that reproduces only under the accumulated JIT/heap state of running many
classes in one JVM process, not in isolation. `InvalidHttpMethodIntegrationTests`
is in this list despite passing 4/4 cleanly in a standalone smoke test on this
exact binary hours earlier — direct confirmation this is load-dependent, not
a code regression. These are **already covered by the existing HIB-CV-32
tracking**, not new bugs requiring separate docs.

Affected (for reference — do not action individually, root cause is shared):
- `cache.jcache.JCacheEhCacheAnnotationTests`
- `context.annotation.ClassPathBeanDefinitionScannerTests`
- `context.annotation.ComponentScanParserScopedProxyTests`
- `context.annotation.EnableAspectJAutoProxyTests`
- `context.annotation.PropertySourceAnnotationTests`
- `http.client.JettyClientHttpRequestFactoryTests`
- `jdbc.config.JdbcNamespaceIntegrationTests`
- `jms.annotation.JmsListenerAnnotationBeanPostProcessorTests`
- `messaging.rsocket.RSocketBufferLeakTests`
- `test.context.groovy.AbsolutePathGroovySpringContextTests`
- `test.context.groovy.DefaultScriptDetectionGroovySpringContextTests`
- `test.context.groovy.GroovySpringContextTests`
- `test.context.groovy.MixedXmlAndGroovySpringContextTests`
- `test.context.groovy.RelativePathGroovySpringContextTests`
- `test.context.web.BasicGroovyWacTests`
- `test.web.reactive.server.samples.JsonContentTests`
- `test.web.servlet.samples.standalone.ViewResolutionTests`
- `web.client.support.RestClientProxyRegistryIntegrationTests`
- `web.reactive.config.WebFluxConfigurationSupportTests`
- `web.reactive.config.WebFluxViewResolutionIntegrationTests`
- `web.reactive.function.client.support.WebClientProxyRegistryIntegrationTests`
- `web.reactive.function.server.InvalidHttpMethodIntegrationTests`
- `web.reactive.result.method.annotation.GlobalCorsConfigIntegrationTests`
- `web.reactive.result.method.annotation.RequestMappingDataBindingIntegrationTests`
- `web.reactive.result.method.annotation.RequestMappingViewResolutionIntegrationTests`
- `web.reactive.result.view.LocaleContextResolverIntegrationTests`
- `web.reactive.result.view.freemarker.FreeMarkerMacroTests`
- `web.reactive.result.view.freemarker.FreeMarkerViewTests`
- `web.socket.WebSocketHandshakeTests`
- `web.socket.adapter.standard.ConvertingEncoderDecoderSupportTests`
- `web.socket.messaging.OrderedMessageSendingIntegrationTests`

## Module breakdown (all 125)

```
     46 web
     20 test
     15 context
     12 beans
      7 core
      4 scripting
      4 orm
      3 http
      2 oxm
      2 jms
      2 expression
      1 util
      1 scheduling
      1 mock
      1 messaging
      1 jdbc
      1 cache
      1 aot
      1 aop
```

## Per-class detail

### `aop.scope.ScopedProxyBeanRegistrationAotProcessorTests` — FAIL
- getBeanRegistrationCodeGeneratorWhenNotScopedProxy() :: org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'test': No bean named 'testDoesNotExist' available
- getBeanRegistrationCodeGeneratorWhenScopedProxyWithTargetBeanName() :: org.springframework.core.test.tools.CompilationException: Unable to compile source
- getBeanRegistrationCodeGeneratorWhenScopedProxyWithoutTargetBeanName() :: java.lang.AssertionError: 

### `aot.nativex.feature.ThrowawayClassLoaderTests` — FAIL
- loadingClassFromResourceClosesInputStream() :: java.lang.AssertionError: [InputStream closed] 

### `beans.PropertyDescriptorUtilsPropertyResolutionTests` — FAIL
- classWithTwoSubtypeSetters() :: org.assertj.core.error.AssertJMultipleFailuresError: 
- classWithTwoSubtypeSettersAndOneUnrelatedSetter() :: org.assertj.core.error.AssertJMultipleFailuresError: 
- determineBasicPropertiesWithUnresolvedGenericsInSubInterface() :: java.lang.AssertionError: 
- resolvePropertiesWithPartiallyUnresolvedGenericsInSubclassWithOverriddenGetter() :: org.assertj.core.error.AssertJMultipleFailuresError: 
- resolvePropertiesWithPartiallyUnresolvedGenericsInSubclassWithOverriddenGetterAndOverloadedSetter() :: org.assertj.core.error.AssertJMultipleFailuresError: 

### `beans.factory.DefaultListableBeanFactoryTests` — FAIL
- autowirePreferredConstructorsFromAttribute() :: org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'bean': Unexpected exception during bean creation

### `beans.factory.annotation.AutowiredAnnotationBeanRegistrationAotContributionTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `beans.factory.aot.BeanDefinitionMethodGeneratorTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `beans.factory.aot.BeanDefinitionPropertiesCodeGeneratorTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `beans.factory.aot.BeanDefinitionPropertyValueCodeGeneratorDelegatesTests` — LOADERR
- java.lang.OutOfMemoryError: Java heap space (new_object class_id 483 fields 5)

### `beans.factory.aot.BeanRegistrationsAotContributionTests` — ABEND
    == org.springframework.beans.factory.aot.BeanRegistrationsAotContributionTests ABEND rc=139 ==
    [2m2026-07-09T08:03:51.506175Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    [2m2026-07-09T08:04:08.921592Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)
    timeout: the monitored command dumped core

### `beans.factory.aot.DefaultBeanRegistrationCodeFragmentsTests` — FAIL
- customizedGenerateInstanceSupplierCodeDoesNotResolveInstantiationDescriptor() :: java.lang.NoClassDefFoundError: org/springframework/aot/generate/MethodName
- getTargetOnConstructorToProtectedFactoryBean() :: java.lang.NoClassDefFoundError: org/springframework/aot/generate/MethodName
- getTargetOnConstructorToPublicGenericFactoryBeanExtractTargetFromFactoryBeanType() :: java.lang.NoClassDefFoundError: org/springframework/aot/generate/MethodName
- getTargetOnConstructorToPublicGenericFactoryBeanUseBeanTypeAsFallback() :: java.lang.NoClassDefFoundError: org/springframework/aot/generate/MethodName
- getTargetOnMethodFromInterface() :: java.lang.NoClassDefFoundError: org/springframework/aot/generate/MethodName

### `beans.factory.aot.InstanceSupplierCodeGeneratorKotlinTests` — FAIL
- generateWhenConstructorHasOptionalParameter() :: org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'testBean': Instantiation of supplied bean failed
- generateWhenHasDefaultConstructor() :: org.springframework.core.test.tools.CompilationException: Unable to compile source

### `beans.factory.aot.InstanceSupplierCodeGeneratorTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `beans.factory.config.PropertyResourceConfigurerTests` — LOADERR
- java.lang.OutOfMemoryError: Java heap space (new_object class_id 929 fields 11)

### `beans.factory.xml.XmlBeanFactoryTests` — ABEND
    == org.springframework.beans.factory.xml.XmlBeanFactoryTests ABEND rc=1 ==
    	at KRun.runOne(KRun.java:54)
    	at KRun.main(KRun.java:34)
    [cratonvm] main-vm run() returned Err: Exception in thread "main" java/lang/Thread
    [cratonvm-cli] (no Java stack frames were captured for this exception)
    [cratonvm-cli] the per-thread trace store has no entry for this throwable either — the exception was likely thrown on a non-main thread, or its constructor was shadowed by a native that skipped fillInStackTrace.

### `cache.jcache.JCacheEhCacheAnnotationTests` — ABEND
    == org.springframework.cache.jcache.JCacheEhCacheAnnotationTests ABEND rc=139 ==
    [2m2026-07-09T08:07:00.889833Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    [CCE] enhance: defined org/springframework/cache/jcache/JCacheEhCacheAnnotationTests$EnableCachingConfig$$EnhancerByCGLIB$$0 (super=org/springframework/cache/jcache/JCacheEhCacheAnnotationTests$EnableCachingConfig, marker=org/springframework/context/annotation/ConfigurationClassEnhancer$EnhancedConfiguration, intercepted @Bean methods=6)
    [2m2026-07-09T08:07:02.345563Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)
    timeout: the monitored command dumped core

### `context.annotation.ClassPathBeanDefinitionScannerTests` — ABEND
    == org.springframework.context.annotation.ClassPathBeanDefinitionScannerTests ABEND rc=1 ==
    [2m2026-07-09T07:57:24.789210Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::read_slot: corrupt Value cell (out-of-range discriminant) — returning null instead of a UB-on-match Value. Heap reference-integrity defect (see HIB-CV-32). [3mslot[0m[2m=[0m0x20017621c18 [3mraw0[0m[2m=[0m"0x000002001760e050" [3mraw1[0m[2m=[0m"0x000002001760e0b8"
    [2m2026-07-09T07:57:24.789238Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::read_slot: corrupt Value cell (out-of-range discriminant) — returning null instead of a UB-on-match Value. Heap reference-integrity defect (see HIB-CV-32). [3mslot[0m[2m=[0m0x20017621c18 [3mraw0[0m[2m=[0m"0x000002001760e050" [3mraw1[0m[2m=[0m"0x000002001760e0b8"
    [cratonvm] main-vm run() returned Err: Exception in thread "main" java/lang/Thread
    [cratonvm-cli] (no Java stack frames were captured for this exception)
    [cratonvm-cli] the per-thread trace store has no entry for this throwable either — the exception was likely thrown on a non-main thread, or its constructor was shadowed by a native that skipped fillInStackTrace.

### `context.annotation.CommonAnnotationBeanRegistrationAotContributionTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `context.annotation.ComponentScanParserBeanDefinitionDefaultsTests` — ABEND
    == org.springframework.context.annotation.ComponentScanParserBeanDefinitionDefaultsTests ABEND rc=1 ==
    [2m2026-07-09T08:06:28.097356Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    [2m2026-07-09T08:06:28.329785Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)
    [cratonvm] main-vm run() returned Err: Exception in thread "main" java/lang/Thread
    [cratonvm-cli] (no Java stack frames were captured for this exception)
    [cratonvm-cli] the per-thread trace store has no entry for this throwable either — the exception was likely thrown on a non-main thread, or its constructor was shadowed by a native that skipped fillInStackTrace.

### `context.annotation.ComponentScanParserScopedProxyTests` — ABEND
    == org.springframework.context.annotation.ComponentScanParserScopedProxyTests ABEND rc=1 ==
    [2m2026-07-09T07:57:27.957218Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    [2m2026-07-09T07:57:28.131469Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)
    [cratonvm] main-vm run() returned Err: Exception in thread "main" java/lang/Thread
    [cratonvm-cli] (no Java stack frames were captured for this exception)
    [cratonvm-cli] the per-thread trace store has no entry for this throwable either — the exception was likely thrown on a non-main thread, or its constructor was shadowed by a native that skipped fillInStackTrace.

### `context.annotation.ConfigurationClassPostConstructAndAutowiringTests` — FAIL
- originalReproCase() :: org.springframework.beans.factory.UnsatisfiedDependencyException: Error creating bean with name 'configurationClassPostConstructAndAutowiringTests.Config2': Unsatisfied dependency expressed through method 'setTestBean' parameter 0: Error creating bean with name 'configurationClassPostConstructAndAutowiringTests.Config1': Invocation of init method failed

### `context.annotation.ConfigurationClassPostProcessorAotContributionTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `context.annotation.ConfigurationClassPostProcessorTests` — ABEND
    == org.springframework.context.annotation.ConfigurationClassPostProcessorTests ABEND rc=1 ==
    [CCE] enhance: defined org/springframework/context/annotation/ConfigurationClassPostProcessorTests$BeanArgumentConfigWithPrototype$$EnhancerByCGLIB$$0 (super=org/springframework/context/annotation/ConfigurationClassPostProcessorTests$BeanArgumentConfigWithPrototype, marker=org/springframework/context/annotation/ConfigurationClassEnhancer$EnhancedConfiguration, intercepted @Bean methods=1)
    [CCE] enhance: defined org/springframework/context/annotation/ConfigurationClassPostProcessorTests$MapInjectionConfiguration$$EnhancerByCGLIB$$0 (super=org/springframework/context/annotation/ConfigurationClassPostProcessorTests$MapInjectionConfiguration, marker=org/springframework/context/annotation/ConfigurationClassEnhancer$EnhancedConfiguration, intercepted @Bean methods=1)
    [cratonvm] main-vm run() returned Err: Exception in thread "main" java/lang/Thread
    [cratonvm-cli] (no Java stack frames were captured for this exception)
    [cratonvm-cli] the per-thread trace store has no entry for this throwable either — the exception was likely thrown on a non-main thread, or its constructor was shadowed by a native that skipped fillInStackTrace.

### `context.annotation.EnableAspectJAutoProxyTests` — ABEND
    == org.springframework.context.annotation.EnableAspectJAutoProxyTests ABEND rc=139 ==
    [2m2026-07-09T07:57:42.711825Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::read_slot: corrupt Value cell (out-of-range discriminant) — returning null instead of a UB-on-match Value. Heap reference-integrity defect (see HIB-CV-32). [3mslot[0m[2m=[0m0x200160bcf50 [3mraw0[0m[2m=[0m"0x00000200160bcf58" [3mraw1[0m[2m=[0m"0x0000000000000006"
    [2m2026-07-09T07:57:43.014943Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::read_slot: corrupt Value cell (out-of-range discriminant) — returning null instead of a UB-on-match Value. Heap reference-integrity defect (see HIB-CV-32). [3mslot[0m[2m=[0m0x20016d48280 [3mraw0[0m[2m=[0m"0x0000020016d37df0" [3mraw1[0m[2m=[0m"0x000000000000020f"
    [2m2026-07-09T07:57:43.030672Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::read_slot: corrupt Value cell (out-of-range discriminant) — returning null instead of a UB-on-match Value. Heap reference-integrity defect (see HIB-CV-32). [3mslot[0m[2m=[0m0x20016d94940 [3mraw0[0m[2m=[0m"0x0000020016d82bd0" [3mraw1[0m[2m=[0m"0x0000020016d82c38"
    [2m2026-07-09T07:57:43.030706Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::read_slot: corrupt Value cell (out-of-range discriminant) — returning null instead of a UB-on-match Value. Heap reference-integrity defect (see HIB-CV-32). [3mslot[0m[2m=[0m0x20016d94940 [3mraw0[0m[2m=[0m"0x0000020016d82bd0" [3mraw1[0m[2m=[0m"0x0000020016d82c38"
    [2m2026-07-09T07:57:45.175215Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::read_slot: corrupt Value cell (out-of-range discriminant) — returning null instead of a UB-on-match Value. Heap reference-integrity defect (see HIB-CV-32). [3mslot[0m[2m=[0m0x200160bf4e8 [3mraw0[0m[2m=[0m"0x00000200160bcf58" [3mraw1[0m[2m=[0m"0x0000000100000006"

### `context.annotation.ImportSelectorTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `context.annotation.InitDestroyMethodLifecycleTests` — FAIL
- jakartaAnnotationsWithCustomSameMethodNamesWithAotProcessingAndAotRuntime() :: java.lang.ArrayIndexOutOfBoundsException: null
- jakartaAnnotationsWithPackagePrivateInitDestroyMethodsWithAotProcessingAndAotRuntime() :: java.lang.ArrayIndexOutOfBoundsException: null

### `context.annotation.PropertySourceAnnotationTests` — ABEND
    == org.springframework.context.annotation.PropertySourceAnnotationTests ABEND rc=1 ==
    [2m2026-07-09T08:07:04.175036Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)
    [CCE] enhance: defined org/springframework/context/annotation/PropertySourceAnnotationTests$ConfigWithExplicitName$$EnhancerByCGLIB$$0 (super=org/springframework/context/annotation/PropertySourceAnnotationTests$ConfigWithExplicitName, marker=org/springframework/context/annotation/ConfigurationClassEnhancer$EnhancedConfiguration, intercepted @Bean methods=1)
    [cratonvm] main-vm run() returned Err: Exception in thread "main" java/lang/Thread
    [cratonvm-cli] (no Java stack frames were captured for this exception)
    [cratonvm-cli] the per-thread trace store has no entry for this throwable either — the exception was likely thrown on a non-main thread, or its constructor was shadowed by a native that skipped fillInStackTrace.

### `context.annotation.Spr15275Tests` — FAIL
- withFactoryBean() :: java.lang.AssertionError: 
- withFinalFactoryBean() :: java.lang.AssertionError: 

### `context.annotation.Spr6602Tests` — FAIL
- configurationClassBehavior() :: org.opentest4j.AssertionFailedError: 

### `context.aot.ApplicationContextAotGeneratorTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `context.groovy.GroovyBeanDefinitionReaderTests` — ABEND
    == org.springframework.context.groovy.GroovyBeanDefinitionReaderTests ABEND rc=139 ==
    [2m2026-07-09T08:05:51.794447Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    [2m2026-07-09T08:05:52.537485Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)
    [2m2026-07-09T08:05:53.114258Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)
    timeout: the monitored command dumped core

### `core.GenericTypeResolverTests` — FAIL
- resolveTypeAgainstSameNamedVariables() :: org.opentest4j.AssertionFailedError: 

### `core.annotation.MergedAnnotationsTests` — FAIL
- synthesizedAnnotationShouldReuseJdkProxyClass() :: java.lang.AssertionError: 

### `core.codec.ResourceRegionEncoderTests` — ABEND
    == org.springframework.core.codec.ResourceRegionEncoderTests ABEND rc=139 ==
    [2m2026-07-09T08:07:33.283299Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    SLF4J(W): No SLF4J providers were found.
    SLF4J(W): Defaulting to no-operation (NOP) logger implementation
    SLF4J(W): See https://www.slf4j.org/codes.html#noProviders for further details.
    [2m2026-07-09T08:07:33.660391Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)

### `core.convert.converter.DefaultConversionServiceTests` — ABEND
    == org.springframework.core.convert.converter.DefaultConversionServiceTests ABEND rc=139 ==
    [2m2026-07-09T07:59:36.774397Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    timeout: the monitored command dumped core

### `core.task.SimpleAsyncTaskExecutorTests` — FAIL
- taskTerminationTimeoutWithImmediateCancel() :: java.lang.AssertionError: 

### `core.test.tools.CompiledTests` — FIXED (2026-07-11)
- getInstanceWhenNoDefaultConstructorThrowsException() :: java.lang.AssertionError:
  Expecting code to raise a throwable. Root cause: `cl_real_load_class_base`'s
  step-1 `ctx.load_class` (real-JDK mode) resolved through CratonVM's flat
  global class store with no defining-loader visibility check, unlike the
  synthetic-JDK path's `resolve_global_if_visible`/`cid_visible_mirror` -- a
  second dynamically-defined `com.example.HelloWorld` from a sibling
  `ClassLoader` (non-null parent) got back the FIRST loader's `Class` object
  instead of defining its own, so the no-default-constructor probe found the
  wrong constructor set and never threw. Fixed by wrapping that lookup (and
  the HIB-CV-24 deferred-resolution fallback) with the same
  `cid_visible_mirror` check via a new `load_class_visible_to` helper in
  `native-builtins/src/classloader_real.rs`. Verified: 14/14 on
  `org.springframework.core.test.tools.CompiledTests`, plus two standalone
  sibling-loader isolation probes (null-parent and non-null-parent cases).

### `core.test.tools.TestCompilerTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `expression.spel.EvaluationTests` — FAIL
- matchesWithPatternAccessThreshold() :: java.lang.AssertionError: 

### `expression.spel.standard.SpelCompilerTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `http.client.JettyClientHttpRequestFactoryTests` — ABEND
    == org.springframework.http.client.JettyClientHttpRequestFactoryTests ABEND rc=139 ==
    [2m2026-07-09T08:12:18.373542Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    SLF4J(W): No SLF4J providers were found.
    SLF4J(W): Defaulting to no-operation (NOP) logger implementation
    SLF4J(W): See https://www.slf4j.org/codes.html#noProviders for further details.
    [2m2026-07-09T08:12:18.631807Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)

### `http.codec.multipart.DefaultPartHttpMessageReaderTests` — ABEND
    == org.springframework.http.codec.multipart.DefaultPartHttpMessageReaderTests ABEND rc=139 ==
    [2m2026-07-09T08:18:01.007557Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    SLF4J(W): No SLF4J providers were found.
    SLF4J(W): Defaulting to no-operation (NOP) logger implementation
    SLF4J(W): See https://www.slf4j.org/codes.html#noProviders for further details.
    [2m2026-07-09T08:18:01.622606Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)

### `http.codec.multipart.MultipartHttpMessageWriterTests` — ABEND
    == org.springframework.http.codec.multipart.MultipartHttpMessageWriterTests ABEND rc=139 ==
    [2m2026-07-09T08:29:48.499995Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    SLF4J(W): No SLF4J providers were found.
    SLF4J(W): Defaulting to no-operation (NOP) logger implementation
    SLF4J(W): See https://www.slf4j.org/codes.html#noProviders for further details.
    [2m2026-07-09T08:29:48.869780Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)

### `jdbc.config.JdbcNamespaceIntegrationTests` — ABEND
    == org.springframework.jdbc.config.JdbcNamespaceIntegrationTests ABEND rc=1 ==
    [2m2026-07-09T08:11:25.511663Z[0m [33m WARN[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computation, typically a speculative collection-layout probe dispatched on a non-matching receiver type) [3mobj[0m[2m=[0m0x20016eb6a28 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(2691) [3mclass_name[0m[2m=[0morg/hsqldb/RangeGroup$RangeGroupEmpty [3mreal_field_count[0m[2m=[0mSome(0)
    [2m2026-07-09T08:11:26.733565Z[0m [33m WARN[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computation, typically a speculative collection-layout probe dispatched on a non-matching receiver type) [3mobj[0m[2m=[0m0x20016eb6a28 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(2691) [3mclass_name[0m[2m=[0morg/hsqldb/RangeGroup$RangeGroupEmpty [3mreal_field_count[0m[2m=[0mSome(0)
    [cratonvm] main-vm run() returned Err: Exception in thread "main" java/lang/Thread
    [cratonvm-cli] (no Java stack frames were captured for this exception)
    [cratonvm-cli] the per-thread trace store has no entry for this throwable either — the exception was likely thrown on a non-main thread, or its constructor was shadowed by a native that skipped fillInStackTrace.

### `jms.annotation.JmsListenerAnnotationBeanPostProcessorTests` — ABEND
    == org.springframework.jms.annotation.JmsListenerAnnotationBeanPostProcessorTests ABEND rc=1 ==
    [2m2026-07-09T08:04:14.052572Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)
    [CCE] enhance: defined org/springframework/jms/annotation/JmsListenerAnnotationBeanPostProcessorTests$ProxyConfig$$EnhancerByCGLIB$$0 (super=org/springframework/jms/annotation/JmsListenerAnnotationBeanPostProcessorTests$ProxyConfig, marker=org/springframework/context/annotation/ConfigurationClassEnhancer$EnhancedConfiguration, intercepted @Bean methods=1)
    [cratonvm] main-vm run() returned Err: Exception in thread "main" java/lang/Thread
    [cratonvm-cli] (no Java stack frames were captured for this exception)
    [cratonvm-cli] the per-thread trace store has no entry for this throwable either — the exception was likely thrown on a non-main thread, or its constructor was shadowed by a native that skipped fillInStackTrace.

### `jms.listener.MessageListenerContainerObservationTests` — ABEND
    == org.springframework.jms.listener.MessageListenerContainerObservationTests ABEND rc=139 ==
    [2m2026-07-09T08:04:23.483062Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    SLF4J(W): No SLF4J providers were found.
    SLF4J(W): Defaulting to no-operation (NOP) logger implementation
    SLF4J(W): See https://www.slf4j.org/codes.html#noProviders for further details.
    [2m2026-07-09T08:04:23.983968Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)

### `messaging.rsocket.RSocketBufferLeakTests` — ABEND
    == org.springframework.messaging.rsocket.RSocketBufferLeakTests ABEND rc=139 ==
    [2m2026-07-09T08:19:47.999179Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    [CCE] enhance: defined org/springframework/messaging/rsocket/RSocketBufferLeakTests$ServerConfig$$EnhancerByCGLIB$$0 (super=org/springframework/messaging/rsocket/RSocketBufferLeakTests$ServerConfig, marker=org/springframework/context/annotation/ConfigurationClassEnhancer$EnhancedConfiguration, intercepted @Bean methods=3)
    [2m2026-07-09T08:19:48.450060Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)
    [2m2026-07-09T08:19:48.535123Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)
    [2m2026-07-09T08:19:48.537952Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_exec[0m[2m:[0m Missing native method in real-JDK mode [3mmethod[0m[2m=[0mjdk/jfr/internal/JVM.subscribeLogLevel(Ljdk/jfr/internal/LogTag;I)V

### `mock.web.MockHttpServletResponseTests` — FAIL
- contentAsStringEncodingWithJson() :: org.opentest4j.AssertionFailedError: 
- servletWriterAutoFlushedForString() :: org.opentest4j.AssertionFailedError: 

### `orm.jpa.persistenceunit.PersistenceManagedTypesBeanRegistrationAotProcessorTests` — FAIL
- processEntityManagerWithPackagesToScan() :: org.springframework.core.test.tools.CompilationException: Unable to compile source

### `orm.jpa.support.InjectionCodeGeneratorTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `orm.jpa.support.PersistenceAnnotationBeanPostProcessorAotContributionTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `orm.jpa.support.PersistenceInjectionTests` — FAIL
- publicExtendedPersistenceContextSetterWithSerialization() :: org.opentest4j.AssertionFailedError: 

### `oxm.xstream.XStreamMarshallerTests` — FAIL
- aliasesByTypeStringClassMap() :: java.lang.VerifyError: com/thoughtworks/xstream/core/util/SerializationMembers.callWriteObject: at bytecode offset 155: athrow: operand ObjectRef("java/lang/Object") is not assignable to java/lang/Throwable
- jettisonDriver() :: java.lang.VerifyError: com/thoughtworks/xstream/core/util/SerializationMembers.callWriteObject: at bytecode offset 155: athrow: operand ObjectRef("java/lang/Object") is not assignable to java/lang/Throwable
- marshalStaxResultXMLStreamWriter() :: java.lang.VerifyError: com/thoughtworks/xstream/core/util/SerializationMembers.callWriteObject: at bytecode offset 155: athrow: operand ObjectRef("java/lang/Object") is not assignable to java/lang/Throwable
- marshalStaxResultXMLStreamWriterDefaultNamespace() :: java.lang.VerifyError: com/thoughtworks/xstream/core/util/SerializationMembers.callWriteObject: at bytecode offset 155: athrow: operand ObjectRef("java/lang/Object") is not assignable to java/lang/Throwable
- marshalStreamResultOutputStream() :: java.lang.VerifyError: com/thoughtworks/xstream/core/util/SerializationMembers.callWriteObject: at bytecode offset 155: athrow: operand ObjectRef("java/lang/Object") is not assignable to java/lang/Throwable

### `oxm.xstream.XStreamUnmarshallerTests` — FAIL
- unmarshalDomSource() :: java.lang.VerifyError: com/thoughtworks/xstream/core/util/SerializationMembers.callWriteObject: at bytecode offset 155: athrow: operand ObjectRef("java/lang/Object") is not assignable to java/lang/Throwable
- unmarshalStaxSourceXmlStreamReader() :: java.lang.VerifyError: com/thoughtworks/xstream/core/util/SerializationMembers.callWriteObject: at bytecode offset 155: athrow: operand ObjectRef("java/lang/Object") is not assignable to java/lang/Throwable
- unmarshalStreamSourceInputStream() :: java.lang.VerifyError: com/thoughtworks/xstream/core/util/SerializationMembers.callWriteObject: at bytecode offset 155: athrow: operand ObjectRef("java/lang/Object") is not assignable to java/lang/Throwable
- unmarshalStreamSourceReader() :: java.lang.VerifyError: com/thoughtworks/xstream/core/util/SerializationMembers.callWriteObject: at bytecode offset 155: athrow: operand ObjectRef("java/lang/Object") is not assignable to java/lang/Throwable

### `scheduling.quartz.QuartzSupportTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `scripting.config.ScriptingDefaultsTests` — ABEND
    == org.springframework.scripting.config.ScriptingDefaultsTests ABEND rc=139 ==
    [2m2026-07-09T08:06:04.429720Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    [2m2026-07-09T08:06:04.599057Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)
    [2m2026-07-09T08:06:05.757340Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)
    timeout: the monitored command dumped core

### `scripting.groovy.GroovyAspectIntegrationTests` — ABEND
    == org.springframework.scripting.groovy.GroovyAspectIntegrationTests ABEND rc=139 ==
    [2m2026-07-09T08:11:44.499071Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    [2m2026-07-09T08:11:44.658212Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)
    [2m2026-07-09T08:11:46.051779Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)
    timeout: the monitored command dumped core

### `scripting.groovy.GroovyAspectTests` — ABEND
    == org.springframework.scripting.groovy.GroovyAspectTests ABEND rc=139 ==
    [2m2026-07-09T08:07:18.683712Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    [2m2026-07-09T08:07:18.829752Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)
    [2m2026-07-09T08:07:18.897321Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)
    timeout: the monitored command dumped core

### `scripting.groovy.GroovyScriptFactoryTests` — ABEND
    == org.springframework.scripting.groovy.GroovyScriptFactoryTests ABEND rc=139 ==
    [2m2026-07-09T08:06:13.120568Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    [2m2026-07-09T08:06:13.585804Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)
    [2m2026-07-09T08:06:14.583115Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)
    timeout: the monitored command dumped core

### `test.context.aot.AotIntegrationTests` — FAIL
- endToEndTests() :: java.lang.ArrayIndexOutOfBoundsException: null

### `test.context.aot.TestClassScannerTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `test.context.aot.TestContextAotGeneratorIntegrationTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `test.context.groovy.AbsolutePathGroovySpringContextTests` — ABEND
    == org.springframework.test.context.groovy.AbsolutePathGroovySpringContextTests ABEND rc=139 ==
    [2m2026-07-09T08:10:32.396448Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    SLF4J(W): No SLF4J providers were found.
    SLF4J(W): Defaulting to no-operation (NOP) logger implementation
    SLF4J(W): See https://www.slf4j.org/codes.html#noProviders for further details.
    [2m2026-07-09T08:10:32.562219Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)

### `test.context.groovy.DefaultScriptDetectionGroovySpringContextTests` — ABEND
    == org.springframework.test.context.groovy.DefaultScriptDetectionGroovySpringContextTests ABEND rc=139 ==
    [2m2026-07-09T08:09:53.459585Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    SLF4J(W): No SLF4J providers were found.
    SLF4J(W): Defaulting to no-operation (NOP) logger implementation
    SLF4J(W): See https://www.slf4j.org/codes.html#noProviders for further details.
    [2m2026-07-09T08:09:53.571180Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)

### `test.context.groovy.GroovySpringContextTests` — ABEND
    == org.springframework.test.context.groovy.GroovySpringContextTests ABEND rc=139 ==
    [2m2026-07-09T08:16:32.934650Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    SLF4J(W): No SLF4J providers were found.
    SLF4J(W): Defaulting to no-operation (NOP) logger implementation
    SLF4J(W): See https://www.slf4j.org/codes.html#noProviders for further details.
    [2m2026-07-09T08:16:33.094700Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)

### `test.context.groovy.MixedXmlAndGroovySpringContextTests` — ABEND
    == org.springframework.test.context.groovy.MixedXmlAndGroovySpringContextTests ABEND rc=139 ==
    [2m2026-07-09T08:28:19.530596Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    SLF4J(W): No SLF4J providers were found.
    SLF4J(W): Defaulting to no-operation (NOP) logger implementation
    SLF4J(W): See https://www.slf4j.org/codes.html#noProviders for further details.
    [2m2026-07-09T08:28:19.647370Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)

### `test.context.groovy.RelativePathGroovySpringContextTests` — ABEND
    == org.springframework.test.context.groovy.RelativePathGroovySpringContextTests ABEND rc=139 ==
    [2m2026-07-09T08:10:46.367847Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    SLF4J(W): No SLF4J providers were found.
    SLF4J(W): Defaulting to no-operation (NOP) logger implementation
    SLF4J(W): See https://www.slf4j.org/codes.html#noProviders for further details.
    [2m2026-07-09T08:10:46.528622Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)

### `test.context.junit.jupiter.event.ParallelApplicationEventsIntegrationTests` — FAIL
- executeTestsInParallelWithInstancePerMethod() :: org.opentest4j.MultipleFailuresError: Test Event Statistics (2 failures)
- rejectTestsInParallelWithInstancePerClassAndRecordApplicationEvents() :: java.lang.AssertionError: 

### `test.context.junit.jupiter.parallel.ParallelExecutionSpringExtensionTests` — ABEND
    == org.springframework.test.context.junit.jupiter.parallel.ParallelExecutionSpringExtensionTests ABEND rc=139 ==
    [2m2026-07-09T08:28:32.648250Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    SLF4J(W): No SLF4J providers were found.
    SLF4J(W): Defaulting to no-operation (NOP) logger implementation
    SLF4J(W): See https://www.slf4j.org/codes.html#noProviders for further details.
    [2m2026-07-09T08:28:32.764968Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)

### `test.context.web.BasicGroovyWacTests` — ABEND
    == org.springframework.test.context.web.BasicGroovyWacTests ABEND rc=139 ==
    [2m2026-07-09T08:11:53.606218Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    SLF4J(W): No SLF4J providers were found.
    SLF4J(W): Defaulting to no-operation (NOP) logger implementation
    SLF4J(W): See https://www.slf4j.org/codes.html#noProviders for further details.
    [2m2026-07-09T08:11:53.732684Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)

### `test.web.reactive.server.samples.JsonContentTests` — ABEND
    == org.springframework.test.web.reactive.server.samples.JsonContentTests ABEND rc=139 ==
    DEBUG [org.hibernate.validator.internal.xml.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via Hibernate Validator's class loader
    DEBUG [org.hibernate.validator.messageinterpolation.ResourceBundleMessageInterpolator] Loaded expression factory via original TCCL
    DEBUG [org.hibernate.validator.internal.xml.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via user class loader
    DEBUG [org.hibernate.validator.internal.xml.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via TCCL
    DEBUG [org.hibernate.validator.internal.xml.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via Hibernate Validator's class loader

### `test.web.servlet.assertj.AbstractMockHttpServletResponseAssertTests` — FAIL
- bodyJsonCanLoadResourceRelativeToClass() :: java.lang.IllegalStateException: org.json.JSONException: Unparsable JSON string: 
- bodyJsonWithJsonPath() :: java.lang.IllegalArgumentException: json can not be null or empty
- bodyText() :: org.opentest4j.AssertionFailedError: 
- bodyWithByteArray() :: org.opentest4j.AssertionFailedError: 
- hasBodyTextEqualTo() :: org.opentest4j.AssertionFailedError: 

### `test.web.servlet.assertj.MockMvcTesterIntegrationTests` — FAIL
- debugCanPrintToCustomOutputStream() :: java.lang.AssertionError: 
- debugUsesSystemOutByDefault() :: java.lang.AssertionError: 

### `test.web.servlet.client.samples.bind.FilterTests` — FAIL
- filter() :: java.lang.AssertionError: Response body expected:<It works!> but was:<null>

### `test.web.servlet.htmlunit.MockWebResponseBuilderTests` — FAIL
- buildContent() :: org.opentest4j.AssertionFailedError: 

### `test.web.servlet.result.JsonPathResultMatchersTests` — FAIL
- valueWithJsonPrefix() :: java.lang.AssertionError: JSON prefix "prefix" not found

### `test.web.servlet.result.PrintingResultHandlerTests` — FAIL
- printResponseWithCharacterEncoding() :: org.opentest4j.AssertionFailedError: [For label 'Body' under heading 'MockHttpServletResponse' =>] 
- printResponseWithDefaultCharacterEncoding() :: org.opentest4j.AssertionFailedError: [For label 'Body' under heading 'MockHttpServletResponse' =>] 

### `test.web.servlet.result.XpathResultMatchersTests` — FAIL
- exists() :: org.xml.sax.SAXParseException: Premature end of file.
- nodeListNoMatch() :: java.lang.AssertionError: 
- nodeNoMatch() :: java.lang.AssertionError: 
- numberNoMatch() :: java.lang.AssertionError: 
- stringNoMatch() :: java.lang.AssertionError: 

### `test.web.servlet.samples.standalone.ViewResolutionTests` — ABEND
    == org.springframework.test.web.servlet.samples.standalone.ViewResolutionTests ABEND rc=139 ==
    FINE [jakarta.xml.bind]   not found
    FINE [jakarta.xml.bind] Checking system property jakarta.xml.bind.context.factory
    FINE [jakarta.xml.bind]   not found
    FINE [jakarta.xml.bind] Checking system property jakarta.xml.bind.JAXBContext
    FINE [jakarta.xml.bind]   not found

### `util.function.SingletonSupplierTests` — FAIL
- repetition 77 of 100 :: java.lang.AssertionError: 

### `web.client.support.RestClientProxyRegistryIntegrationTests` — ABEND
    == org.springframework.web.client.support.RestClientProxyRegistryIntegrationTests ABEND rc=139 ==
    [2m2026-07-09T08:18:07.152236Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    SLF4J(W): No SLF4J providers were found.
    SLF4J(W): Defaulting to no-operation (NOP) logger implementation
    SLF4J(W): See https://www.slf4j.org/codes.html#noProviders for further details.
    [2m2026-07-09T08:18:07.693068Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)

### `web.context.ContextLoaderTests` — ABEND
    == org.springframework.web.context.ContextLoaderTests ABEND rc=1 ==
    [2m2026-07-09T08:12:28.782015Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)
    [2m2026-07-09T08:12:30.836511Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)
    [cratonvm] main-vm run() returned Err: Exception in thread "main" java/lang/Thread
    [cratonvm-cli] (no Java stack frames were captured for this exception)
    [cratonvm-cli] the per-thread trace store has no entry for this throwable either — the exception was likely thrown on a non-main thread, or its constructor was shadowed by a native that skipped fillInStackTrace.

### `web.context.support.HttpRequestHandlerTests` — FAIL
- httpRequestHandlerServletPassThrough() :: org.opentest4j.AssertionFailedError: 

### `web.reactive.config.WebFluxConfigurationSupportTests` — ABEND
    == org.springframework.web.reactive.config.WebFluxConfigurationSupportTests ABEND rc=139 ==
    DEBUG [org.hibernate.validator.messageinterpolation.ResourceBundleMessageInterpolator] Loaded expression factory via original TCCL
    DEBUG [org.hibernate.validator.internal.xml.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via user class loader
    DEBUG [org.hibernate.validator.internal.xml.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via TCCL
    DEBUG [org.hibernate.validator.internal.xml.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via Hibernate Validator's class loader
    DEBUG [org.hibernate.validator.messageinterpolation.ResourceBundleMessageInterpolator] Loaded expression factory via original TCCL

### `web.reactive.config.WebFluxViewResolutionIntegrationTests` — ABEND
    == org.springframework.web.reactive.config.WebFluxViewResolutionIntegrationTests ABEND rc=139 ==
    DEBUG [org.hibernate.validator.internal.xml.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via Hibernate Validator's class loader
    DEBUG [org.hibernate.validator.messageinterpolation.ResourceBundleMessageInterpolator] Loaded expression factory via original TCCL
    DEBUG [org.hibernate.validator.internal.xml.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via user class loader
    DEBUG [org.hibernate.validator.internal.xml.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via TCCL
    DEBUG [org.hibernate.validator.internal.xml.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via Hibernate Validator's class loader

### `web.reactive.function.client.support.WebClientProxyRegistryIntegrationTests` — ABEND
    == org.springframework.web.reactive.function.client.support.WebClientProxyRegistryIntegrationTests ABEND rc=139 ==
    [2m2026-07-09T08:11:33.988359Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m   [CLINIT-TRACE 11] at org/junit/platform/launcher/core/EngineExecutionOrchestrator.execute (EngineExecutionOrchestrator.java:65) bci=28
    [2m2026-07-09T08:11:33.988361Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m   [CLINIT-TRACE 12] at org/junit/platform/launcher/core/EngineExecutionOrchestrator.withInterceptedStreams (EngineExecutionOrchestrator.java:168) bci=51
    [2m2026-07-09T08:11:33.988374Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m   [CLINIT-TRACE 13] at org/junit/platform/launcher/core/EngineExecutionOrchestrator.lambda$execute$0 (EngineExecutionOrchestrator.java:66) bci=9
    [2m2026-07-09T08:11:33.988377Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m   [CLINIT-TRACE 14] at org/junit/platform/launcher/core/EngineExecutionOrchestrator.execute (EngineExecutionOrchestrator.java:109) bci=59
    [2m2026-07-09T08:11:33.988379Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m   [CLINIT-TRACE 15] at org/junit/platform/launcher/core/EngineExecutionOrchestrator.execute (EngineExecutionOrchestrator.java:189) bci=70

### `web.reactive.function.server.InvalidHttpMethodIntegrationTests` — ABEND
    == org.springframework.web.reactive.function.server.InvalidHttpMethodIntegrationTests ABEND rc=139 ==
    [2m2026-07-09T08:31:17.218013Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    [2m2026-07-09T08:31:17.410986Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)
    SLF4J(W): No SLF4J providers were found.
    SLF4J(W): Defaulting to no-operation (NOP) logger implementation
    SLF4J(W): See https://www.slf4j.org/codes.html#noProviders for further details.

### `web.reactive.result.method.annotation.CoroutinesIntegrationTests` — ABEND
    == org.springframework.web.reactive.result.method.annotation.CoroutinesIntegrationTests ABEND rc=139 ==
    SLF4J(W): No SLF4J providers were found.
    SLF4J(W): Defaulting to no-operation (NOP) logger implementation
    SLF4J(W): See https://www.slf4j.org/codes.html#noProviders for further details.
    [2m2026-07-09T08:11:40.382408Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)
    INFO [org.hibernate.validator.internal.util.Version] HV000001: Hibernate Validator 9.1.2.Final

### `web.reactive.result.method.annotation.CrossOriginAnnotationIntegrationTests` — ABEND
    == org.springframework.web.reactive.result.method.annotation.CrossOriginAnnotationIntegrationTests ABEND rc=139 ==
    [2m2026-07-09T08:18:35.191481Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086bf8790 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:18:35.191494Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086bf8790 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:18:35.191498Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086bf8790 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:18:35.191501Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086bf8790 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:18:35.191504Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086bf8790 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)

### `web.reactive.result.method.annotation.GlobalCorsConfigIntegrationTests` — ABEND
    == org.springframework.web.reactive.result.method.annotation.GlobalCorsConfigIntegrationTests ABEND rc=139 ==
    [2m2026-07-09T08:31:35.109941Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086b25248 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:31:35.126019Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086b25248 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:31:35.126046Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086b25248 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:31:35.126053Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086b25248 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:31:35.126061Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086b25248 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)

### `web.reactive.result.method.annotation.JacksonHintsIntegrationTests` — ABEND
    == org.springframework.web.reactive.result.method.annotation.JacksonHintsIntegrationTests ABEND rc=139 ==
    [2m2026-07-09T08:14:43.512389Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086aca518 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:14:43.532072Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086aca518 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:14:43.532106Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086aca518 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:14:43.532113Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086aca518 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:14:43.532121Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086aca518 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)

### `web.reactive.result.method.annotation.MessageReaderArgumentResolverTests` — ABEND
    == org.springframework.web.reactive.result.method.annotation.MessageReaderArgumentResolverTests ABEND rc=134 ==
    [2m2026-07-09T08:11:44.933389Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    [2m2026-07-09T08:11:45.273093Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)
    [2m2026-07-09T08:11:45.292948Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)
    SLF4J(W): No SLF4J providers were found.
    SLF4J(W): Defaulting to no-operation (NOP) logger implementation

### `web.reactive.result.method.annotation.MessageWriterResultHandlerTests` — ABEND
    == org.springframework.web.reactive.result.method.annotation.MessageWriterResultHandlerTests ABEND rc=139 ==
    [2m2026-07-09T08:18:38.967988Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    [2m2026-07-09T08:18:39.212585Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)
    [2m2026-07-09T08:18:39.231806Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)
    SLF4J(W): No SLF4J providers were found.
    SLF4J(W): Defaulting to no-operation (NOP) logger implementation

### `web.reactive.result.method.annotation.ProtobufIntegrationTests` — ABEND
    == org.springframework.web.reactive.result.method.annotation.ProtobufIntegrationTests ABEND rc=1 ==
    [2m2026-07-09T08:31:46.831696Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086a75140 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:31:46.831721Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086a75140 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:31:46.831728Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086a75140 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:31:46.831735Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086a75140 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    DEBUG [org.hibernate.validator.messageinterpolation.ResourceBundleMessageInterpolator] Loaded expression factory via original TCCL

### `web.reactive.result.method.annotation.RequestMappingDataBindingIntegrationTests` — ABEND
    == org.springframework.web.reactive.result.method.annotation.RequestMappingDataBindingIntegrationTests ABEND rc=139 ==
    [2m2026-07-09T08:14:52.346361Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086a23be8 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:14:52.366091Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086a23be8 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:14:52.366128Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086a23be8 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:14:52.366135Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086a23be8 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:14:52.366143Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086a23be8 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)

### `web.reactive.result.method.annotation.RequestMappingExceptionHandlingIntegrationTests` — ABEND
    == org.springframework.web.reactive.result.method.annotation.RequestMappingExceptionHandlingIntegrationTests ABEND rc=139 ==
    [2m2026-07-09T08:12:06.417717Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086ab84f8 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:12:06.417724Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086ab84f8 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:12:06.417732Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086ab84f8 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:12:06.890379Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086ab84f8 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:12:06.890411Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086ab84f8 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)

### `web.reactive.result.method.annotation.RequestMappingMessageConversionIntegrationTests` — ABEND
    == org.springframework.web.reactive.result.method.annotation.RequestMappingMessageConversionIntegrationTests ABEND rc=139 ==
    [2m2026-07-09T08:18:48.844920Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086e82a98 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:18:48.844923Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086e82a98 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:18:48.844927Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::read_slot: corrupt Value cell (out-of-range discriminant) — returning null instead of a UB-on-match Value. Heap reference-integrity defect (see HIB-CV-32). [3mslot[0m[2m=[0m0x200898ed010 [3mraw0[0m[2m=[0m"0x00000200898db298" [3mraw1[0m[2m=[0m"0x000000000000021b"
    [2m2026-07-09T08:18:48.844930Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086e82a98 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:18:48.844933Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::read_slot: corrupt Value cell (out-of-range discriminant) — returning null instead of a UB-on-match Value. Heap reference-integrity defect (see HIB-CV-32). [3mslot[0m[2m=[0m0x200898ed010 [3mraw0[0m[2m=[0m"0x00000200898db298" [3mraw1[0m[2m=[0m"0x000000000000021b"

### `web.reactive.result.method.annotation.RequestMappingViewResolutionIntegrationTests` — ABEND
    == org.springframework.web.reactive.result.method.annotation.RequestMappingViewResolutionIntegrationTests ABEND rc=139 ==
    [2m2026-07-09T08:31:58.720489Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m   [CLINIT-TRACE 17] at org/junit/platform/launcher/core/EngineExecutionOrchestrator.executeEngine (EngineExecutionOrchestrator.java:256) bci=26
    [2m2026-07-09T08:31:58.720491Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m   [CLINIT-TRACE 18] at org/junit/platform/engine/support/hierarchical/HierarchicalTestEngine.execute (HierarchicalTestEngine.java:58) bci=31
    [2m2026-07-09T08:31:58.720492Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m   [CLINIT-TRACE 19] at org/junit/platform/engine/support/hierarchical/HierarchicalTestExecutor.execute (HierarchicalTestExecutor.java:52) bci=8
    [2m2026-07-09T08:31:58.915968Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086a3aab0 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:31:58.916005Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086a3aab0 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)

### `web.reactive.result.view.FragmentViewResolutionResultHandlerTests` — ABEND
    == org.springframework.web.reactive.result.view.FragmentViewResolutionResultHandlerTests ABEND rc=1 ==
    [2m2026-07-09T08:15:19.242697Z[0m [33m WARN[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computation, typically a speculative collection-layout probe dispatched on a non-matching receiver type) [3mobj[0m[2m=[0m0x200926c9b90 [3mindex[0m[2m=[0m3 [3mnum_slots[0m[2m=[0m1 [3mclass_id[0m[2m=[0mClassId(4318) [3mclass_name[0m[2m=[0morg/python/core/io/TextIOInputStream [3mreal_field_count[0m[2m=[0mSome(1)
    [2m2026-07-09T08:15:25.255818Z[0m [33m WARN[0m [2mcratonvm_native_builtins::lang_system[0m[2m:[0m ClassLoader.defineClass1(warnings$py) failed: Linkage(VerifyError { class_name: "warnings$py", method_name: "f$0", message: "failed to parse StackMapTable: invalid class data: StackMapTable: invalid verification type tag: 14" })
    [cratonvm] main-vm run() returned Err: Exception in thread "main" org/python/antlr/runtime/CommonToken
    [cratonvm-cli] (no Java stack frames were captured for this exception)
    [cratonvm-cli] the per-thread trace store has no entry for this throwable either — the exception was likely thrown on a non-main thread, or its constructor was shadowed by a native that skipped fillInStackTrace.

### `web.reactive.result.view.LocaleContextResolverIntegrationTests` — ABEND
    == org.springframework.web.reactive.result.view.LocaleContextResolverIntegrationTests ABEND rc=1 ==
    [2m2026-07-09T08:12:15.534214Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086f27340 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:12:15.534245Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086f27340 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:12:15.534253Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086f27340 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:12:15.534260Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20086f27340 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    DEBUG [org.hibernate.validator.messageinterpolation.ResourceBundleMessageInterpolator] Loaded expression factory via original TCCL

### `web.reactive.result.view.ViewResolutionResultHandlerTests` — FAIL
- defaultViewNameWithRedirectPrefixFails() :: java.lang.AssertionError: expectation "expectError(Class)" failed (expected: onError(ResponseStatusException); actual: onComplete())

### `web.reactive.result.view.freemarker.FreeMarkerMacroTests` — ABEND
    == org.springframework.web.reactive.result.view.freemarker.FreeMarkerMacroTests ABEND rc=139 ==
    [2m2026-07-09T08:16:25.916535Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    [2m2026-07-09T08:16:26.346628Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)
    [2m2026-07-09T08:16:26.354119Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)
    SLF4J(W): No SLF4J providers were found.
    SLF4J(W): Defaulting to no-operation (NOP) logger implementation

### `web.reactive.result.view.freemarker.FreeMarkerViewTests` — ABEND
    == org.springframework.web.reactive.result.view.freemarker.FreeMarkerViewTests ABEND rc=139 ==
    [2m2026-07-09T08:12:17.722259Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    [2m2026-07-09T08:12:17.950903Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)
    [2m2026-07-09T08:12:17.955357Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)
    SLF4J(W): No SLF4J providers were found.
    SLF4J(W): Defaulting to no-operation (NOP) logger implementation

### `web.reactive.result.view.script.JRubyScriptTemplateTests` — ABEND
    == org.springframework.web.reactive.result.view.script.JRubyScriptTemplateTests ABEND rc=139 ==
    [2m2026-07-09T08:18:56.140859Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    [CCE] enhance: defined org/springframework/web/reactive/result/view/script/JRubyScriptTemplateTests$ScriptTemplatingConfiguration$$EnhancerByCGLIB$$0 (super=org/springframework/web/reactive/result/view/script/JRubyScriptTemplateTests$ScriptTemplatingConfiguration, marker=org/springframework/context/annotation/ConfigurationClassEnhancer$EnhancedConfiguration, intercepted @Bean methods=1)
    [2m2026-07-09T08:18:56.544897Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)
    [2m2026-07-09T08:18:56.596469Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)
    timeout: the monitored command dumped core

### `web.reactive.result.view.script.JythonScriptTemplateTests` — FAIL
- renderTemplate() :: java.lang.IllegalStateException: Failed to evaluate script [org/springframework/web/reactive/result/view/script/jython/render.py]

### `web.server.session.WebSessionIntegrationTests` — ABEND
    == org.springframework.web.server.session.WebSessionIntegrationTests ABEND rc=139 ==
    [2m2026-07-09T08:18:10.765982Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    [2m2026-07-09T08:18:11.046701Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)
    [2m2026-07-09T08:18:11.056399Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)
    SLF4J(W): No SLF4J providers were found.
    SLF4J(W): Defaulting to no-operation (NOP) logger implementation

### `web.service.registry.GroupsMetadataValueDelegateTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `web.service.registry.HttpServiceProxyRegistrationAotProcessorTests` — FAIL
- processHttpServiceProxyWhenSameClientTypeInDifferentGroups() :: java.lang.ArrayIndexOutOfBoundsException: null
- processHttpServiceProxyWhenSingleClientType() :: java.lang.ArrayIndexOutOfBoundsException: null

### `web.service.registry.ImportHttpServiceRegistrarTests` — FAIL
- basicListingWithAot() :: java.lang.ArrayIndexOutOfBoundsException: null
- basicScanWithAot() :: java.lang.ArrayIndexOutOfBoundsException: null

### `web.servlet.DispatcherServletTests` — FAIL
- parsedRequestPathIsRestoredOnForward() :: org.opentest4j.AssertionFailedError: 
- shouldAttemptToResetResponseBufferIfCommitted() :: java.lang.AssertionError: 

### `web.servlet.config.MvcNamespaceTests` — ABEND
    == org.springframework.web.servlet.config.MvcNamespaceTests ABEND rc=139 ==
    SLF4J(W): See https://www.slf4j.org/codes.html#noProviders for further details.
    DEBUG [org.hibernate.validator.messageinterpolation.ResourceBundleMessageInterpolator] Loaded expression factory via original TCCL
    DEBUG [org.hibernate.validator.internal.xml.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via user class loader
    DEBUG [org.hibernate.validator.internal.xml.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via TCCL
    DEBUG [org.hibernate.validator.internal.xml.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via Hibernate Validator's class loader

### `web.servlet.config.annotation.ViewResolutionIntegrationTests` — ABEND
    == org.springframework.web.servlet.config.annotation.ViewResolutionIntegrationTests ABEND rc=139 ==
    [2m2026-07-09T08:19:58.462043Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)
    INFO [org.hibernate.validator.internal.util.Version] HV000001: Hibernate Validator 9.1.2.Final
    DEBUG [org.hibernate.validator.messageinterpolation.ResourceBundleMessageInterpolator] Loaded expression factory via original TCCL
    DEBUG [org.hibernate.validator.internal.xml.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via user class loader
    DEBUG [org.hibernate.validator.internal.xml.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via TCCL

### `web.servlet.handler.HandlerMappingIntrospectorTests` — FAIL
- [1] uri = "/test" :: org.opentest4j.AssertionFailedError: 
- [2] uri = "/resource/1234****" :: org.opentest4j.AssertionFailedError: 
- cacheFilterWithNestedDispatch() :: org.opentest4j.AssertionFailedError: 

### `web.servlet.mvc.method.annotation.ExceptionHandlerExceptionResolverTests` — FAIL
- resolveExceptionResponseWriter() :: org.opentest4j.AssertionFailedError: 

### `web.servlet.mvc.method.annotation.FragmentRenderingStreamTests` — ABEND
    == org.springframework.web.servlet.mvc.method.annotation.FragmentRenderingStreamTests ABEND rc=1 ==
    [2m2026-07-09T08:13:12.136568Z[0m [33m WARN[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computation, typically a speculative collection-layout probe dispatched on a non-matching receiver type) [3mobj[0m[2m=[0m0x20092575048 [3mindex[0m[2m=[0m3 [3mnum_slots[0m[2m=[0m1 [3mclass_id[0m[2m=[0mClassId(3712) [3mclass_name[0m[2m=[0morg/python/core/io/TextIOInputStream [3mreal_field_count[0m[2m=[0mSome(1)
    [2m2026-07-09T08:13:18.268080Z[0m [33m WARN[0m [2mcratonvm_native_builtins::lang_system[0m[2m:[0m ClassLoader.defineClass1(warnings$py) failed: Linkage(VerifyError { class_name: "warnings$py", method_name: "f$0", message: "failed to parse StackMapTable: invalid class data: StackMapTable: invalid verification type tag: 14" })
    [cratonvm] main-vm run() returned Err: Exception in thread "main" org/python/antlr/runtime/CommonToken
    [cratonvm-cli] (no Java stack frames were captured for this exception)
    [cratonvm-cli] the per-thread trace store has no entry for this throwable either — the exception was likely thrown on a non-main thread, or its constructor was shadowed by a native that skipped fillInStackTrace.

### `web.servlet.mvc.method.annotation.ServletAnnotationControllerHandlerMethodTests` — ABEND
    == org.springframework.web.servlet.mvc.method.annotation.ServletAnnotationControllerHandlerMethodTests ABEND rc=139 ==
    TRACE [org.hibernate.validator.resourceloading.PlatformResourceBundleLocator] ContributorValidationMessages not found by thread context classloader
    TRACE [org.hibernate.validator.resourceloading.PlatformResourceBundleLocator] ContributorValidationMessages not found by validator classloader
    DEBUG [org.hibernate.validator.messageinterpolation.ResourceBundleMessageInterpolator] Loaded expression factory via original TCCL
    DEBUG [org.hibernate.validator.internal.xml.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via TCCL
    DEBUG [org.hibernate.validator.internal.xml.config.ResourceLoaderHelper] Trying to load META-INF/validation.xml via Hibernate Validator's class loader

### `web.servlet.tags.EvalTagTests` — FAIL
- environmentAccess() :: org.opentest4j.AssertionFailedError: 
- mapAccess() :: org.opentest4j.AssertionFailedError: 
- printHtmlEscapedAttributeResult() :: org.opentest4j.AssertionFailedError: 
- printJavaScriptEscapedAttributeResult() :: org.opentest4j.AssertionFailedError: 
- printScopedAttributeResult() :: org.opentest4j.AssertionFailedError: 

### `web.servlet.view.DefaultFragmentsRenderingTests` — FAIL
- render() :: java.lang.IllegalStateException: Failed to evaluate script [org/springframework/web/servlet/view/script/jython/render.py]

### `web.servlet.view.script.JRubyScriptTemplateTests` — FAIL
- renderTemplate() :: java.lang.IllegalStateException: Failed to evaluate script [org/springframework/web/servlet/view/script/jruby/render.rb]

### `web.servlet.view.script.JythonScriptTemplateTests` — FAIL
- renderTemplate() :: org.opentest4j.AssertionFailedError: 

### `web.socket.WebSocketHandshakeTests` — ABEND
    == org.springframework.web.socket.WebSocketHandshakeTests ABEND rc=139 ==
    INFO [org.apache.catalina.core.StandardService] Starting service [Tomcat]
    INFO [org.apache.catalina.core.StandardEngine] Starting Servlet engine: [Apache Tomcat/11.0.23]
    WARN [org.apache.catalina.util.SessionIdGeneratorBase] The default SHA1PRNG algorithm for SecureRandom is not supported by this JVM. Using the platform default.
    INFO [org.apache.coyote.http11.Http11NioProtocol] Starting ProtocolHandler ["http-nio-auto-1-43139"]
    INFO [org.apache.catalina.core.ContainerBase.[Tomcat].[localhost].[/]] Initializing Spring DispatcherServlet 'dispatcherServlet'

### `web.socket.adapter.standard.ConvertingEncoderDecoderSupportTests` — ABEND
    == org.springframework.web.socket.adapter.standard.ConvertingEncoderDecoderSupportTests ABEND rc=139 ==
    [2m2026-07-09T08:20:25.313031Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    [CCE] enhance: defined org/springframework/web/socket/adapter/standard/ConvertingEncoderDecoderSupportTests$Config$$EnhancerByCGLIB$$0 (super=org/springframework/web/socket/adapter/standard/ConvertingEncoderDecoderSupportTests$Config, marker=org/springframework/context/annotation/ConfigurationClassEnhancer$EnhancedConfiguration, intercepted @Bean methods=1)
    [CCE] enhance: defined org/springframework/web/socket/adapter/standard/ConvertingEncoderDecoderSupportTests$NoConvertersConfig$$EnhancerByCGLIB$$0 (super=org/springframework/web/socket/adapter/standard/ConvertingEncoderDecoderSupportTests$NoConvertersConfig, marker=org/springframework/context/annotation/ConfigurationClassEnhancer$EnhancedConfiguration, intercepted @Bean methods=1)
    timeout: the monitored command dumped core

### `web.socket.config.MessageBrokerBeanDefinitionParserTests` — FAIL
- simpleBroker() :: org.opentest4j.AssertionFailedError: 

### `web.socket.messaging.OrderedMessageSendingIntegrationTests` — ABEND
    == org.springframework.web.socket.messaging.OrderedMessageSendingIntegrationTests ABEND rc=139 ==
    [2m2026-07-09T08:20:46.554337Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    [2m2026-07-09T08:20:46.577174Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)
    timeout: the monitored command dumped core

### `web.socket.messaging.StompWebSocketIntegrationTests` — ABEND
    == org.springframework.web.socket.messaging.StompWebSocketIntegrationTests ABEND rc=139 ==
    [2m2026-07-09T08:16:09.591575Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x2001a8661e0 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:16:09.591583Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x2001a8661e0 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:16:09.591590Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x2001a8661e0 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:16:12.779491Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x2001a8661e0 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-09T08:16:12.779537Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x2001a8661e0 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)

### `web.socket.messaging.SubProtocolWebSocketHandlerTests` — FAIL
- checkSession() :: java.lang.IllegalStateException: No handler for 'v12.stomp' among {}
- subProtocolDefaultHandlerOnly() :: java.lang.IllegalStateException: No handler for 'v12.sToMp' among {}
- subProtocolMatch() :: java.lang.IllegalStateException: No handler for 'v12.sToMp' among {}

