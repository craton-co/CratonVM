# CratonVM Spring suite — genuine bug list (fixed-environment cross-reference)

VM: dev `9298db15`, Azure host. Baseline: fresh HotSpot run over 516 non-passed
classes with corrected classpath (spring-websocket/oxm/jms/orm/core-test jars
were missing before — `./gradlew jar testFixturesJar testClasses` fixed it).
430/516 pass on HotSpot (86 excluded: 13 FAIL + 73 EMPTY on HotSpot itself —
not CratonVM bugs, removed from all future run lists). Cross-referenced against
a fresh CratonVM rerun of the same 430 classes.

A first pass found 172 CV-unique failures, but 15 were a harness artifact
(`KRun` transiently unresolvable — this host's recursive /data/data mount
shifts path depth mid-session; see [[azure-host-data-data-recursive-mount-trap]]).
Rerun individually: 13 now pass cleanly, 2 are genuine (folded in below).

**159 confirmed genuine CratonVM-unique bugs** (HotSpot OK, CratonVM != OK):
111 FAIL, 17 ABEND, 29 TIMEOUT, 2 LOADERR. 258/430 (60%) now agree with HotSpot.

## Module breakdown

```
     43 web
     23 test
     14 expression
     14 context
     13 beans
     12 jms
     10 core
      5 orm
      4 scripting
      3 validation
      3 oxm
      3 http
      2 util
      2 scheduling
      2 mock
      2 jdbc
      2 aot
      1 ejb
      1 aop
```

## Per-class detail

### `aop.scope.ScopedProxyBeanRegistrationAotProcessorTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `aot.hint.ReflectionTypeReferenceTests` — FAIL
- [3] clazz = class [Ljava.lang.Integer;, binaryName = "java.lang.Integer[]" :: org.opentest4j.AssertionFailedError: 
- [4] clazz = class [Ljava.lang.Object;, binaryName = "java.lang.Object[]" :: org.opentest4j.AssertionFailedError: 

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

### `beans.factory.aot.BeanRegistrationsAotContributionTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `beans.factory.aot.CodeWarningsTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `beans.factory.aot.DefaultBeanRegistrationCodeFragmentsTests` — FAIL
- customizedGenerateInstanceSupplierCodeDoesNotResolveInstantiationDescriptor() :: java.lang.IllegalStateException: Could not initialize plugin: interface org.mockito.plugins.MockMaker (alternate: null)
- customizedGetTargetDoesNotResolveInstantiationDescriptor() :: java.lang.IllegalStateException: Could not initialize plugin: interface org.mockito.plugins.MockMaker (alternate: null)

### `beans.factory.aot.InstanceSupplierCodeGeneratorKotlinTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `beans.factory.aot.InstanceSupplierCodeGeneratorTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `beans.factory.config.PropertyResourceConfigurerTests` — LOADERR
- java.lang.OutOfMemoryError: Java heap space (new_object class_id 483 fields 5)

### `beans.factory.xml.XmlBeanFactoryTests` — FAIL
- classNotFoundWithDefaultBeanClassLoader() :: java.lang.AssertionError: 
- overrideMethodByArgTypeAttribute() :: org.opentest4j.AssertionFailedError: [should replace] 
- overrideMethodByArgTypeElement() :: org.opentest4j.AssertionFailedError: [should replace] 
- rejectsOverrideOfBogusMethodName() :: java.lang.AssertionError: 
- replaceNonOverloadedInterfaceMethodWithoutSpecifyingExplicitArgTypes() :: org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'replaceEchoMethod' defined in class path resource [org/springframework/beans/factory/xml/XmlBeanFactoryTests-delegationOverrides.xml]: Failed to instantiate [org.springframework.beans.factory.xml.EchoService]: Specified class is an interface

### `context.annotation.CommonAnnotationBeanRegistrationAotContributionTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `context.annotation.ComponentScanParserBeanDefinitionDefaultsTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `context.annotation.ConfigurationClassPostConstructAndAutowiringTests` — FAIL
- originalReproCase() :: org.springframework.beans.factory.UnsatisfiedDependencyException: Error creating bean with name 'configurationClassPostConstructAndAutowiringTests.Config2': Unsatisfied dependency expressed through method 'setTestBean' parameter 0: Error creating bean with name 'configurationClassPostConstructAndAutowiringTests.Config1': Invocation of init method failed

### `context.annotation.ConfigurationClassPostProcessorAotContributionTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `context.annotation.ConfigurationClassPostProcessorTests` — FAIL
- beanLookupFromSameConfigurationClass() :: java.lang.NoSuchMethodException: getTestBean
- configurationClassesWithInvalidOverridingForProgrammaticCall() :: java.lang.AssertionError: 
- nullArgumentThroughBeanMethodCall() :: org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'aFoo' defined in org.springframework.context.annotation.ConfigurationClassPostProcessorTests$BeanArgumentConfigWithNull: Failed to instantiate [org.springframework.context.annotation.ConfigurationClassPostProcessorTests$DependingFoo]: Factory method 'aFoo' threw exception with message: No BarArgument injected

### `context.annotation.ImportSelectorTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `context.annotation.InitDestroyMethodLifecycleTests` — FAIL
- jakartaAnnotationsWithCustomSameMethodNamesWithAotProcessingAndAotRuntime() :: java.lang.ArrayIndexOutOfBoundsException: null
- jakartaAnnotationsWithPackagePrivateInitDestroyMethodsWithAotProcessingAndAotRuntime() :: java.lang.ArrayIndexOutOfBoundsException: null

### `context.annotation.Spr15275Tests` — FAIL
- withFactoryBean() :: java.lang.AssertionError: 
- withFinalFactoryBean() :: java.lang.AssertionError: 

### `context.annotation.Spr6602Tests` — FAIL
- configurationClassBehavior() :: org.opentest4j.AssertionFailedError: 

### `context.aot.ApplicationContextAotGeneratorTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `context.aot.ReflectiveProcessorAotContributionBuilderTests` — FAIL
- classesAndScanWithDuplicatesFiltersThem() :: java.lang.AssertionError: [Extracted: classes] 
- classesWithMatchingCandidates() :: java.lang.AssertionError: [Extracted: classes] 
- classesWithMatchingCandidatesFiltersDuplicates() :: java.lang.AssertionError: [Extracted: classes] 
- scanWithMatchingCandidates() :: java.lang.AssertionError: [Extracted: classes] 
- scanWithMatchingCandidatesInSubPackages() :: java.lang.AssertionError: [Extracted: classes] 

### `context.config.ContextNamespaceHandlerTests` — FAIL
- propertyPlaceholderLocationWithSystemPropertyMissing() :: java.lang.AssertionError: 

### `context.expression.ApplicationContextExpressionTests` — FAIL
- genericApplicationContext() :: org.springframework.beans.factory.NoSuchBeanDefinitionException: No bean named 'tb1' available

### `context.groovy.GroovyBeanDefinitionReaderTests` — FAIL
- beanWithParentRef() :: org.springframework.beans.factory.parsing.BeanDefinitionParsingException: Configuration problem: Error evaluating Groovy script: org/springframework/context/groovy/beans$_run_closure1
- namedArgumentConstructor() :: org.springframework.beans.factory.parsing.BeanDefinitionParsingException: Configuration problem: Error evaluating Groovy script: org/springframework/context/groovy/beans$_run_closure1
- registerBeans() :: org.springframework.beans.factory.parsing.BeanDefinitionParsingException: Configuration problem: Error evaluating Groovy script: org/springframework/context/groovy/beans
- scopes() :: org.springframework.beans.factory.parsing.BeanDefinitionParsingException: Configuration problem: Error evaluating Groovy script: org/springframework/context/groovy/beans
- simpleBean() :: org.springframework.beans.factory.parsing.BeanDefinitionParsingException: Configuration problem: Error evaluating Groovy script: org/springframework/context/groovy/beans$_run_closure1

### `core.GenericTypeResolverTests` — FAIL
- resolveTypeAgainstSameNamedVariables() :: org.opentest4j.AssertionFailedError: 

### `core.SortedPropertiesTests` — FAIL
- entrySet() :: org.opentest4j.AssertionFailedError: 
- entrySetFromPrototype() :: org.opentest4j.AssertionFailedError: 

### `core.annotation.MergedAnnotationsTests` — FAIL
- synthesizedAnnotationShouldReuseJdkProxyClass() :: java.lang.AssertionError: 

### `core.codec.ResourceRegionEncoderTests` — FAIL
- nonExisting() :: java.lang.AssertionError: expectation "consumeNextWith" failed (expected: onNext(); actual: onError(java.io.IOException: InternalError(Internal { message: "ByteBuffer missing backing array (field 0 returned Int(-1) for object ObjectRef { ptr: 0x20018933ca8 })" })))
- shouldEncodeMultipleResourceRegionsFileResource() :: java.lang.AssertionError: expectation "consumeNextWith" failed (expected: onNext(); actual: onError(java.io.IOException: InternalError(Internal { message: "ByteBuffer missing backing array (field 0 returned Int(-1) for object ObjectRef { ptr: 0x2001885da50 })" })))
- shouldEncodeResourceRegionFileResource() :: java.lang.AssertionError: expectation "consumeNextWith" failed (expected: onNext(); actual: onError(java.io.IOException: InternalError(Internal { message: "ByteBuffer missing backing array (field 0 returned Int(-1) for object ObjectRef { ptr: 0x200188c7c58 })" })))

### `core.convert.converter.DefaultConversionServiceTests` — FAIL
- integerToEnumWithSubclass() :: org.springframework.core.convert.ConversionFailedException: Failed to convert from type [java.lang.Integer] to type [org.springframework.core.convert.converter.DefaultConversionServiceTests$SubFoo$1] for value [1]
- stringToEnumWithSubclass() :: org.springframework.core.convert.ConversionFailedException: Failed to convert from type [java.lang.String] to type [org.springframework.core.convert.converter.DefaultConversionServiceTests$SubFoo$1] for value [BAZ]

### `core.io.ModuleResourceTests` — FAIL
- existingClassFileResource() :: org.opentest4j.AssertionFailedError: 

### `core.retry.RetryTemplateTests` — FAIL
- retryableWithTimeoutExceededAfterSecondRetry() :: java.lang.AssertionError: 

### `core.task.SimpleAsyncTaskExecutorTests` — FAIL
- taskTerminationTimeoutWithImmediateCancel() :: java.lang.AssertionError: 

### `core.test.tools.CompiledTests` — FAIL
- getInstanceWhenNoDefaultConstructorThrowsException() :: java.lang.AssertionError: 

### `core.test.tools.TestCompilerTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `ejb.config.JeeNamespaceHandlerTests` — FAIL
- complexLocalSlsb() :: java.lang.AssertionError: 
- complexRemoteSlsb() :: java.lang.AssertionError: 
- simpleLocalSlsb() :: java.lang.AssertionError: 
- simpleRemoteSlsb() :: java.lang.AssertionError: 

### `expression.spel.ArrayConstructorTests` — FAIL
- primitiveTypeArrayConstructors() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '1d' cannot be parsed as a double
- primitiveTypeArrayConstructorsElements() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '1d' cannot be parsed as a double

### `expression.spel.ConstructorInvocationTests` — FAIL
- argumentConversion() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '3.0d' cannot be parsed as a double
- varargsConstructors() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '3.0d' cannot be parsed as a double

### `expression.spel.EvaluationTests` — FAIL
- incrementAllNodeTypes() :: java.lang.AssertionError: 
- matchesWithPatternAccessThreshold() :: java.lang.AssertionError: 

### `expression.spel.LiteralTests` — FAIL
- doubles() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '1.25d' cannot be parsed as a double
- doublesUsingExponents() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '6.0221415E+23d' cannot be parsed as a double
- doublesUsingExponentsWithInvalidInput() :: org.assertj.core.error.AssertJMultipleFailuresError: 
- floats() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '1.25f' cannot be parsed as a double

### `expression.spel.MethodInvocationTests` — FAIL
- varargsInvocation01() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '3.0d' cannot be parsed as a double
- varargsInvocation02() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '3.0d' cannot be parsed as a double
- varargsOptionalInvocation() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '3.0d' cannot be parsed as a double
- varargsWithPrimitiveArrayType() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '3.0d' cannot be parsed as a double
- widening() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '3.0d' cannot be parsed as a double

### `expression.spel.OperatorTests` — FAIL
- divide() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '3.0f' cannot be parsed as a double
- greaterThanOrEqual() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '3.0d' cannot be parsed as a double
- lessThanOrEqual() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '3.0d' cannot be parsed as a double
- mixedOperandsBigDecimal() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '3.0d' cannot be parsed as a double
- mixedOperands_FloatsAndDoubles() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '3.0d' cannot be parsed as a double

### `expression.spel.ParsingTests` — FAIL
- mathOperatorPower() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '3.0d' cannot be parsed as a double

### `expression.spel.SpelCompilationCoverageTests` — FAIL
- constructorReference() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '4.0d' cannot be parsed as a double
- constructorReference_SPR12326() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '5.0f' cannot be parsed as a double
- indexIntoMapUsingPrimitiveLiteral() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '9.99F' cannot be parsed as a double
- methodReferenceVarargs() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '1.0d' cannot be parsed as a double
- realLiteral() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '3.4d' cannot be parsed as a double

### `expression.spel.SpelDocumentationTests` — FAIL
- varargsMethodInvocationWithTypeConversion() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '10F' cannot be parsed as a double

### `expression.spel.SpelReproTests` — FAIL
- SPR9486_floatEqDoubleUnaryMinus() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '10.21f' cannot be parsed as a double
- SPR9486_floatGreaterThanDouble() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '10.21f' cannot be parsed as a double
- SPR9486_floatGreaterThanFloat() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '10.21f' cannot be parsed as a double
- SPR9486_floatModulusFloat() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '10.21f' cannot be parsed as a double
- SPR9486_subtractFloatWithFloat() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '10.21f' cannot be parsed as a double

### `expression.spel.VariableAndFunctionTests` — FAIL
- functionWithPrimitiveVarargsViaMethodHandle() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '3.0F' cannot be parsed as a double
- functionWithVarargsViaMethodHandle() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '3.0F' cannot be parsed as a double

### `expression.spel.ast.InlineCollectionTests` — FAIL
- mapWithNegativeFloatValuesIsCached() :: org.springframework.expression.spel.SpelParseException: EL1040E: The value '1.0f' cannot be parsed as a double

### `expression.spel.standard.SpelCompilerTests` — FAIL
- changingRegisteredVariableTypeDoesNotResultInFailureInMixedMode() :: java.lang.NoSuchMethodError: java/lang/Object.accept(I)V

### `expression.spel.standard.SpelParserTests` — FAIL
- numerics() :: java.lang.AssertionError: EL1040E: The value '3.5f' cannot be parsed as a double

### `http.codec.multipart.DefaultPartHttpMessageReaderTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `http.codec.multipart.MultipartHttpMessageWriterTests` — FAIL
- customContentDisposition() :: reactor.core.Exceptions$ReactiveException: java.io.IOException: InternalError(Internal { message: "ByteBuffer missing backing array (field 0 returned Int(-1) for object ObjectRef { ptr: 0x2001d07e478 })" })
- singleSubscriberWithResource() :: reactor.core.Exceptions$ReactiveException: java.io.IOException: InternalError(Internal { message: "ByteBuffer missing backing array (field 0 returned Int(-1) for object ObjectRef { ptr: 0x2001e549208 })" })
- writeMultipartFormData() :: reactor.core.Exceptions$ReactiveException: java.io.IOException: InternalError(Internal { message: "ByteBuffer missing backing array (field 0 returned Int(-1) for object ObjectRef { ptr: 0x2001de3d760 })" })

### `http.converter.BufferedImageHttpMessageConverterTests` — FAIL
- canRead() :: java.lang.NoClassDefFoundError: javax/imageio/ImageIO
- canWrite() :: java.lang.UnsatisfiedLinkError: java/awt/Toolkit.initIDs()V
- read() :: java.lang.NoClassDefFoundError: javax/imageio/ImageIO
- write() :: java.lang.NoClassDefFoundError: javax/imageio/ImageIO
- writeDefaultContentType() :: java.lang.NoClassDefFoundError: javax/imageio/ImageIO

### `jdbc.core.JdbcTemplateTests` — FAIL
- batchUpdateWithBatchFailingHasUpdateCounts() :: java.lang.AssertionError: 
- batchUpdateWithBatchFailingMatchesOriginalException() :: java.lang.AssertionError: 

### `jdbc.support.incrementer.H2SequenceMaxValueIncrementerTests` — FAIL
- [1] mode = REGULAR :: org.springframework.dao.DataAccessResourceFailureException: StatementCallback; SQL [SHUTDOWN]; Database is already closed (to disable automatic closing at VM shutdown, add ";DB_CLOSE_ON_EXIT=FALSE" to the db URL) [90121-240]
- [2] mode = STRICT :: org.springframework.dao.DataAccessResourceFailureException: StatementCallback; SQL [SHUTDOWN]; Database is already closed (to disable automatic closing at VM shutdown, add ";DB_CLOSE_ON_EXIT=FALSE" to the db URL) [90121-240]
- [3] mode = LEGACY :: org.springframework.dao.DataAccessResourceFailureException: StatementCallback; SQL [SHUTDOWN]; Database is already closed (to disable automatic closing at VM shutdown, add ";DB_CLOSE_ON_EXIT=FALSE" to the db URL) [90121-240]
- [4] mode = DB2 :: org.springframework.dao.DataAccessResourceFailureException: StatementCallback; SQL [SHUTDOWN]; Database is already closed (to disable automatic closing at VM shutdown, add ";DB_CLOSE_ON_EXIT=FALSE" to the db URL) [90121-240]
- [5] mode = Derby :: org.springframework.dao.DataAccessResourceFailureException: StatementCallback; SQL [SHUTDOWN]; Database is already closed (to disable automatic closing at VM shutdown, add ";DB_CLOSE_ON_EXIT=FALSE" to the db URL) [90121-240]

### `jms.config.JmsListenerContainerFactoryIntegrationTests` — FAIL
- messageConverterUsedIfSet() :: java.lang.NoSuchMethodError: java/util/HashSet.hasMoreElements()Z
- messagingMessageConverterCanBeUsed() :: java.lang.NoSuchMethodError: java/util/HashSet.hasMoreElements()Z
- parameterAnnotationWithCglibProxy() :: java.lang.NoSuchMethodError: java/util/HashSet.hasMoreElements()Z
- parameterAnnotationWithJdkProxy() :: java.lang.NoSuchMethodError: java/util/HashSet.hasMoreElements()Z

### `jms.config.MethodJmsListenerEndpointTests` — FAIL
- resolveCustomHeaderNameAndPayload() :: java.lang.NoSuchMethodError: java/util/HashSet.hasMoreElements()Z
- resolveCustomHeaderNameAndPayloadWithHeaderNameSet() :: java.lang.NoSuchMethodError: java/util/HashSet.hasMoreElements()Z
- resolveHeaderAndPayload() :: java.lang.NoSuchMethodError: java/util/HashSet.hasMoreElements()Z
- resolveHeaders() :: java.lang.NoSuchMethodError: java/util/HashSet.hasMoreElements()Z
- resolveJmsMessageHeaderAccessor() :: java.lang.NoSuchMethodError: java/util/HashSet.hasMoreElements()Z

### `jms.core.JmsClientTests` — FAIL
- receiveAndConvert() :: java.lang.NoSuchMethodError: java/util/HashSet.hasMoreElements()Z
- receiveAndConvertName() :: java.lang.NoSuchMethodError: java/util/HashSet.hasMoreElements()Z
- receiveAndConvertNoConverter() :: java.lang.AssertionError: 
- receiveAndConvertWithConversion() :: java.lang.NoSuchMethodError: java/util/HashSet.hasMoreElements()Z
- receiveName() :: java.lang.NoSuchMethodError: java/util/HashSet.hasMoreElements()Z

### `jms.core.JmsMessagingTemplateTests` — FAIL
- convertSendAndReceivePayloadWithDestination() :: java.lang.NoSuchMethodError: java/util/HashSet.hasMoreElements()Z
- receiveAndConvertName() :: java.lang.NoSuchMethodError: java/util/HashSet.hasMoreElements()Z
- receiveAndConvertNoConverter() :: java.lang.AssertionError: 
- receiveAndConvertWithConversion() :: java.lang.NoSuchMethodError: java/util/HashSet.hasMoreElements()Z
- receiveName() :: java.lang.NoSuchMethodError: java/util/HashSet.hasMoreElements()Z

### `jms.core.JmsTemplateJtaTests` — FAIL
- exceptionStackTrace() :: java.lang.AssertionError: [inner jms exception not found] 

### `jms.core.JmsTemplateTests` — FAIL
- exceptionStackTrace() :: java.lang.AssertionError: [inner jms exception not found] 

### `jms.core.JmsTemplateTransactedTests` — FAIL
- exceptionStackTrace() :: java.lang.AssertionError: [inner jms exception not found] 

### `jms.listener.MessageListenerContainerObservationTests` — FAIL
- [2] SimpleMessageListenerContainer :: java.lang.UnsupportedOperationException: null

### `jms.listener.adapter.MessagingMessageListenerAdapterTests` — FAIL
- lazyResolutionMessageToStringWithResolvedPayloadAndHeaders() :: java.lang.NoSuchMethodError: java/util/HashSet.hasMoreElements()Z

### `jms.support.JmsMessageHeaderAccessorTests` — FAIL
- validateJmsHeaders() :: java.lang.NoSuchMethodError: java/util/HashSet.hasMoreElements()Z

### `jms.support.SimpleJmsHeaderMapperTests` — FAIL
- attemptToReadDisallowedCorrelationIdPropertyIsNotFatal() :: java.lang.NoSuchMethodError: java/util/HashSet.hasMoreElements()Z
- attemptToReadDisallowedReplyToPropertyIsNotFatal() :: java.lang.NoSuchMethodError: java/util/HashSet.hasMoreElements()Z
- attemptToReadDisallowedTimestampPropertyIsNotFatal() :: java.lang.NoSuchMethodError: java/util/HashSet.hasMoreElements()Z
- jmsTimestampMappedToHeader() :: java.lang.NoSuchMethodError: java/util/HashSet.hasMoreElements()Z
- jmsTypeMappedToHeader() :: java.lang.NoSuchMethodError: java/util/HashSet.hasMoreElements()Z

### `jms.support.converter.MessagingMessageConverterTests` — FAIL
- customPayloadConverter() :: java.lang.NoSuchMethodError: java/util/HashSet.hasMoreElements()Z

### `mock.web.MockHttpServletRequestTests` — FAIL
- getRequestURLWithIpv6AddressViaHostHeaderWithPort() :: org.opentest4j.AssertionFailedError: 
- getRequestURLWithIpv6AddressViaHostHeaderWithoutPort() :: org.opentest4j.AssertionFailedError: 
- getRequestURLWithIpv6AddressViaServerNameWithPort() :: org.opentest4j.AssertionFailedError: 
- getRequestURLWithIpv6AddressViaServerNameWithoutPort() :: org.opentest4j.AssertionFailedError: 

### `mock.web.MockHttpServletResponseTests` — FAIL
- contentAsStringEncodingWithJson() :: org.opentest4j.AssertionFailedError: 
- servletWriterAutoFlushedForString() :: org.opentest4j.AssertionFailedError: 

### `orm.jpa.persistenceunit.PersistenceManagedTypesBeanRegistrationAotProcessorTests` — FAIL
- processEntityManagerWithPackagesToScan() :: org.springframework.core.test.tools.CompilationException: Unable to compile source

### `orm.jpa.persistenceunit.PersistenceXmlParsingTests` — FAIL
- exampleComplex() :: java.lang.IllegalArgumentException: URL must not be null
- persistenceUnitRootUrlWithJar() :: java.lang.NullPointerException: Cannot invoke "java.net.URLStreamHandler.sameFile(java.net.URL, java.net.URL)" because "this.handler" is null

### `orm.jpa.support.InjectionCodeGeneratorTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `orm.jpa.support.PersistenceAnnotationBeanPostProcessorAotContributionTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `orm.jpa.support.PersistenceInjectionTests` — FAIL
- publicExtendedPersistenceContextSetterWithSerialization() :: org.opentest4j.AssertionFailedError: 

### `oxm.jaxb.Jaxb2MarshallerTests` — FAIL
- supportsClassesToBeBound() :: java.lang.NoClassDefFoundError: javax/imageio/ImageIO
- supportsContextPath() :: java.lang.UnsatisfiedLinkError: java/awt/Toolkit.initIDs()V
- supportsPackagesToScan() :: java.lang.NoClassDefFoundError: javax/imageio/ImageIO

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

### `scheduling.support.CronTriggerTests` — FAIL
- dailyTriggerOnDaylightSavingBoundary() :: org.opentest4j.AssertionFailedError: 
- incrementDayOfMonthAndRollover() :: org.opentest4j.AssertionFailedError: 
- monthlyTriggerInShortMonth() :: org.opentest4j.AssertionFailedError: 
- specificMinuteHour() :: org.opentest4j.AssertionFailedError: 
- weekDaySequence() :: org.opentest4j.AssertionFailedError: 

### `scripting.config.ScriptingDefaultsTests` — ABEND
    == org.springframework.scripting.config.ScriptingDefaultsTests ABEND rc=1 ==
    [2m2026-07-08T23:30:56.244676Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    [2m2026-07-08T23:30:56.488364Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)
    [2m2026-07-08T23:30:58.284392Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)
    [cratonvm] main-vm run() returned Err: Error in thread "main" class file error: class not found: org/springframework/scripting/config/TestBean
    [cratonvm] main-vm run() Err (debug): Error in thread "main" class file error: class not found: org/springframework/scripting/config/TestBean

### `scripting.groovy.GroovyAspectIntegrationTests` — ABEND
    == org.springframework.scripting.groovy.GroovyAspectIntegrationTests ABEND rc=1 ==
    [2m2026-07-08T23:34:33.714933Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    [2m2026-07-08T23:34:33.889681Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)
    [2m2026-07-08T23:34:36.110063Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)
    [cratonvm] main-vm run() returned Err: Error in thread "main" class file error: class not found: org/springframework/scripting/groovy/GroovyServiceImpl
    [cratonvm] main-vm run() Err (debug): Error in thread "main" class file error: class not found: org/springframework/scripting/groovy/GroovyServiceImpl

### `scripting.groovy.GroovyAspectTests` — FAIL
- manualGroovyBeanWithDynamicPointcutProxyTargetClass() :: org.springframework.aop.framework.AopConfigException: Could not generate CGLIB subclass of class org.springframework.scripting.groovy.GroovyServiceImpl: Common causes of this problem include using a final class or a non-visible class

### `scripting.groovy.GroovyScriptFactoryTests` — ABEND
    == org.springframework.scripting.groovy.GroovyScriptFactoryTests ABEND rc=1 ==
    [2m2026-07-08T23:32:07.461292Z[0m [33m WARN[0m [2mcratonvm_gc::gen_heap[0m[2m:[0m   [A2] BREADCRUMB — NO allocation record covers 0x20025ff8008 (never header-written here, or freed+reused past the ring)
    [2m2026-07-08T23:32:07.461297Z[0m [33m WARN[0m [2mcratonvm_gc::gen_heap[0m[2m:[0m mark_young: rejecting object at 0x20025ff8008 with implausible extent 54904 (kind=0, array_len=0, num_slots=3429) — corrupt header, not marked/scanned; context 0x20025ff7fe8:0000020000000004 0x20025ff7ff0:0000020025998ee0 0x20025ff7ff8:0000020000000004 0x20025ff8000:0000020025ff7e98 0x20025ff8008:0000000000000000 0x20025ff8010:0000000000000000 0x20025ff8018:0000000000000d65 0x20025ff8020:0000000000000000 0x20025ff8028:0000000000000002 0x20025ff8030:0000000000000000 
    [2m2026-07-08T23:32:07.461301Z[0m [33m WARN[0m [2mcratonvm_gc::gen_heap[0m[2m:[0m   [A2] BREADCRUMB — NO allocation record covers 0x20025ff8008 (never header-written here, or freed+reused past the ring)
    [2m2026-07-08T23:32:07.462823Z[0m [33m WARN[0m [2mcratonvm_gc::gen_heap[0m[2m:[0m mark_young: rejecting object at 0x20025ff8008 with implausible extent 54904 (kind=0, array_len=0, num_slots=3429) — corrupt header, not marked/scanned; context 0x20025ff7fe8:0000020000000004 0x20025ff7ff0:0000020025998ee0 0x20025ff7ff8:0000020000000004 0x20025ff8000:0000020025ff7e98 0x20025ff8008:0000000000000000 0x20025ff8010:0000000000000000 0x20025ff8018:0000000000000d65 0x20025ff8020:0000000000000000 0x20025ff8028:0000000000000002 0x20025ff8030:0000000000000000 
    [2m2026-07-08T23:32:07.462841Z[0m [33m WARN[0m [2mcratonvm_gc::gen_heap[0m[2m:[0m   [A2] BREADCRUMB — NO allocation record covers 0x20025ff8008 (never header-written here, or freed+reused past the ring)
    [2m2026-07-08T23:32:07.462847Z[0m [33m WARN[0m [2mcratonvm_gc::gen_heap[0m[2m:[0m mark_young: rejecting object at 0x20025ff8008 with implausible extent 54904 (kind=0, array_len=0, num_slots=3429) — corrupt header, not marked/scanned; context 0x20025ff7fe8:0000020000000004 0x20025ff7ff0:0000020025998ee0 0x20025ff7ff8:0000020000000004 0x20025ff8000:0000020025ff7e98 0x20025ff8008:0000000000000000 0x20025ff8010:0000000000000000 0x20025ff8018:0000000000000d65 0x20025ff8020:0000000000000000 0x20025ff8028:0000000000000002 0x20025ff8030:0000000000000000 
    [2m2026-07-08T23:32:07.462850Z[0m [33m WARN[0m [2mcratonvm_gc::gen_heap[0m[2m:[0m   [A2] BREADCRUMB — NO allocation record covers 0x20025ff8008 (never header-written here, or freed+reused past the ring)
    Mockito is currently self-attaching to enable the inline-mock-maker. This will no longer work in future releases of the JDK. Please add Mockito as an agent to your build as described in Mockito's documentation: https://javadoc.io/doc/org.mockito/mockito-core/latest/org.mockito/org/mockito/Mockito.html#0.3
    [cratonvm] main-vm run() returned Err: Error in thread "main" class file error: class not found: org/springframework/scripting/groovy/GroovyMessenger2

### `test.context.aot.AotIntegrationTests` — FAIL
- endToEndTests() :: java.lang.ArrayIndexOutOfBoundsException: null

### `test.context.aot.TestClassScannerTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `test.context.aot.TestContextAotGeneratorIntegrationTests` — FAIL
- endToEndTests() :: java.lang.ArrayIndexOutOfBoundsException: null
- processAheadOfTimeWithBasicTests() :: org.junit.platform.launcher.core.DiscoveryIssueException: TestEngine with ID 'junit-jupiter' encountered a critical issue during test discovery:
- processAheadOfTimeWithWebTests() :: java.lang.ArrayIndexOutOfBoundsException: null
- processAheadOfTimeWithXmlTests() :: java.lang.ArrayIndexOutOfBoundsException: null

### `test.context.bean.override.BeanOverrideHandlerTests` — FAIL
- forTestClassWithSingleField() :: java.lang.NoSuchMethodError: java/lang/Object.supplier()Ljava/util/function/Supplier;

### `test.context.env.CustomEncodingTestPropertySourceTests` — FAIL
- propertyIsAvailableInEnvironment(Environment) :: org.opentest4j.AssertionFailedError: 

### `test.context.jdbc.SqlScriptsTestExecutionListenerTests` — FAIL
- valueAndScriptsDeclared() :: java.lang.AssertionError: 

### `test.context.junit.jupiter.event.ParallelApplicationEventsIntegrationTests` — FAIL
- executeTestsInParallelWithInstancePerMethod() :: org.opentest4j.MultipleFailuresError: Test Event Statistics (2 failures)
- rejectTestsInParallelWithInstancePerClassAndRecordApplicationEvents() :: java.lang.AssertionError: 

### `test.context.junit.jupiter.parallel.ParallelExecutionSpringExtensionTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `test.context.orm.hibernate.HibernateSessionFlushingTests` — ABEND
    == org.springframework.test.context.orm.hibernate.HibernateSessionFlushingTests ABEND rc=134 ==
    INFO [org.hibernate.orm.core] HHH000001: Hibernate ORM core version 7.4.4.Final
    DEBUG [org.hibernate.orm.core] HHH000206: 'hibernate.properties' not found
    TRACE [org.hibernate.orm.boot.jaxb] HHH90005520: Reading mappings from InputStream
    FINE [jakarta.xml.bind] Checking system property jakarta.xml.bind.JAXBContextFactory
    FINE [jakarta.xml.bind]   not found
    FINE [jakarta.xml.bind] Checking system property jakarta.xml.bind.context.factory
    FINE [jakarta.xml.bind]   not found
    FINE [jakarta.xml.bind] Checking system property jakarta.xml.bind.JAXBContext
    FINE [jakarta.xml.bind]   not found

### `test.context.orm.jpa.JpaPersonRepositoryTests` — FAIL
- findAll() :: java.lang.IllegalStateException: Failed to load ApplicationContext for [MergedContextConfiguration@23746a testClass = org.springframework.test.context.orm.jpa.JpaPersonRepositoryTests, locations = [], classes = [org.springframework.test.context.orm.jpa.JpaConfig], contextInitializerClasses = [], activeProfiles = [], propertySourceDescriptors = [], propertySourceProperties = [], contextCustomizers = [org.springframework.test.context.support.DynamicPropertiesContextCustomizer@0], contextLoader = org.springframework.test.context.support.DelegatingSmartContextLoader, parent = null]

### `test.context.support.TestPropertySourceUtilsTests` — FAIL
- addInlinedPropertiesToEnvironmentWithMalformedUnicodeInValue() :: java.lang.AssertionError: 
- locationsAndValueAttributes() :: java.lang.AssertionError: 

### `test.context.testng.transaction.ejb.CommitForRequiresNewEjbTxDaoTestNGTests` — ABEND

### `test.context.transaction.ejb.RollbackForRequiresNewEjbTxDaoTests` — ABEND

### `test.context.web.socket.WebSocketServletServerContainerFactoryBeanTests` — ABEND

### `test.web.client.response.DefaultResponseCreatorTests` — FAIL
- setBodyFromStringWithCharset ["Cp1047"] :: org.testng.SkipException: assumption was not met due to: [charset Cp1047 is not supported by this JVM] 

### `test.web.servlet.assertj.AbstractMockHttpServletResponseAssertTests` — FAIL
- bodyJsonCanLoadResourceRelativeToClass() :: java.lang.IllegalStateException: org.json.JSONException: Unparsable JSON string: 
- bodyJsonWithJsonPath() :: java.lang.IllegalArgumentException: json can not be null or empty
- bodyText() :: org.opentest4j.AssertionFailedError: 
- bodyWithByteArray() :: org.opentest4j.AssertionFailedError: 
- hasBodyTextEqualTo() :: org.opentest4j.AssertionFailedError: 

### `test.web.servlet.assertj.MockMvcTesterIntegrationTests` — ABEND

### `test.web.servlet.client.samples.bind.FilterTests` — FAIL
- filter() :: java.lang.AssertionError: Response body expected:<It works!> but was:<null>

### `test.web.servlet.htmlunit.MockWebResponseBuilderTests` — FAIL
- buildContent() :: org.opentest4j.AssertionFailedError: 

### `test.web.servlet.result.JsonPathResultMatchersTests` — ABEND

### `test.web.servlet.result.PrintingResultHandlerTests` — FAIL
- printResponseWithCharacterEncoding() :: org.opentest4j.AssertionFailedError: [For label 'Body' under heading 'MockHttpServletResponse' =>] 
- printResponseWithDefaultCharacterEncoding() :: org.opentest4j.AssertionFailedError: [For label 'Body' under heading 'MockHttpServletResponse' =>] 

### `test.web.servlet.result.XpathResultMatchersTests` — FAIL
- exists() :: org.xml.sax.SAXParseException: Premature end of file.
- nodeListNoMatch() :: java.lang.AssertionError: 
- nodeNoMatch() :: java.lang.AssertionError: 
- numberNoMatch() :: java.lang.AssertionError: 
- stringNoMatch() :: java.lang.AssertionError: 

### `test.web.servlet.samples.client.standalone.ViewResolutionTests` — ABEND

### `util.FileSystemUtilsTests` — FAIL
- copyRecursively(File) :: java.nio.file.AccessDeniedException: /child

### `util.function.SingletonSupplierTests` — FAIL
- repetition 73 of 100 :: java.lang.NoSuchMethodError: java/lang/Object.supplier()Ljava/util/function/Supplier;

### `validation.beanvalidation.BeanValidationBeanRegistrationAotProcessorTests` — FAIL
- shouldProcessGenericTypeLevelConstraint() :: java.lang.AssertionError: 
- shouldProcessTransitiveGenericTypeLevelConstraint() :: java.lang.AssertionError: 

### `validation.beanvalidation.MethodValidationAdapterTests` — FAIL
- validateArguments() :: java.lang.AssertionError: 
- validateBeanListArgument() :: java.lang.AssertionError: 

### `validation.beanvalidation.SpringValidatorAdapterTests` — FAIL
- listElementConstraint() :: org.opentest4j.AssertionFailedError: 
- mapEntryConstraint() :: org.opentest4j.AssertionFailedError: 
- mapValueConstraint() :: org.opentest4j.AssertionFailedError: 

### `web.context.ContextLoaderTests` — FAIL
- classPathXmlApplicationContext() :: org.springframework.beans.factory.BeanInitializationException: Could not load properties: class path resource [org/springframework/web/context/WEB-INF/myplace*.properties] cannot be opened because it does not exist
- contextLoaderListenerWithCustomizedContextLoader() :: org.springframework.beans.factory.BeanInitializationException: Could not load properties: class path resource [org/springframework/web/context/WEB-INF/myplace*.properties] cannot be opened because it does not exist
- contextLoaderListenerWithDefaultContext() :: org.springframework.beans.factory.BeanInitializationException: Could not load properties: class path resource [org/springframework/web/context/WEB-INF/myplace*.properties] cannot be opened because it does not exist
- singletonDestructionOnStartupFailure() :: java.lang.AssertionError: 

### `web.context.XmlWebApplicationContextTests` — FAIL
- factoryIsInitialized() :: org.springframework.beans.factory.BeanInitializationException: Could not load properties: class path resource [org/springframework/web/context/WEB-INF/myplace*.properties] cannot be opened because it does not exist
- factorySingleton() :: org.springframework.beans.factory.BeanInitializationException: Could not load properties: class path resource [org/springframework/web/context/WEB-INF/myplace*.properties] cannot be opened because it does not exist
- getSharedInstanceByMatchingClass() :: org.springframework.beans.factory.BeanInitializationException: Could not load properties: class path resource [org/springframework/web/context/WEB-INF/myplace*.properties] cannot be opened because it does not exist
- getSharedInstanceByMatchingClassNoCatch() :: org.springframework.beans.factory.BeanInitializationException: Could not load properties: class path resource [org/springframework/web/context/WEB-INF/myplace*.properties] cannot be opened because it does not exist
- inheritance() :: org.springframework.beans.factory.BeanInitializationException: Could not load properties: class path resource [org/springframework/web/context/WEB-INF/myplace*.properties] cannot be opened because it does not exist

### `web.context.support.HttpRequestHandlerTests` — FAIL
- httpRequestHandlerServletPassThrough() :: org.opentest4j.AssertionFailedError: 

### `web.filter.ContentCachingResponseWrapperTests` — FAIL
- [1] setContentType() :: java.lang.AssertionError: 
- [2] setHeader() :: java.lang.AssertionError: 
- [3] addHeader() :: java.lang.AssertionError: 

### `web.reactive.function.client.WebClientUtilsTests` — FAIL
- opaqueUriUnchanged() :: org.opentest4j.AssertionFailedError: 

### `web.reactive.resource.EncodedResourceResolverTests` — FAIL
- resolveGzippedWithVersion(GzippedFiles) :: java.lang.IllegalStateException: Timeout on blocking read for 5000000000 NANOSECONDS

### `web.reactive.resource.ResourceTransformerSupportTests` — FAIL
- resolveUrlPath() :: java.lang.IllegalStateException: Timeout on blocking read for 5000000000 NANOSECONDS
- resolveUrlPathWithRelativePath() :: java.lang.IllegalStateException: Timeout on blocking read for 5000000000 NANOSECONDS
- resolveUrlPathWithRelativePathInParentDirectory() :: java.lang.IllegalStateException: Timeout on blocking read for 5000000000 NANOSECONDS

### `web.reactive.resource.ResourceUrlProviderTests` — FAIL
- bestPatternMatch() :: java.lang.IllegalStateException: Timeout on blocking read for 5000000000 NANOSECONDS
- getVersionedResourceUrl() :: java.lang.IllegalStateException: Timeout on blocking read for 5000000000 NANOSECONDS

### `web.reactive.result.method.annotation.CoroutinesIntegrationTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `web.reactive.result.method.annotation.CrossOriginAnnotationIntegrationTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `web.reactive.result.method.annotation.JacksonHintsIntegrationTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `web.reactive.result.method.annotation.MessageReaderArgumentResolverTests` — ABEND
    == org.springframework.web.reactive.result.method.annotation.MessageReaderArgumentResolverTests ABEND rc=134 ==
    [2m2026-07-08T23:36:14.152687Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: UnsafeConstants populated (5/5)
    [2m2026-07-08T23:36:14.666073Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: File separator/pathSeparator populated (4/4)
    [2m2026-07-08T23:36:14.695736Z[0m [33m WARN[0m [2mcratonvm_vm::vm::vm_util[0m[2m:[0m Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)
    SLF4J(W): No SLF4J providers were found.
    SLF4J(W): Defaulting to no-operation (NOP) logger implementation
    SLF4J(W): See https://www.slf4j.org/codes.html#noProviders for further details.
    FATAL: heap exhausted allocating java/lang/String (259476 units)
    timeout: the monitored command dumped core

### `web.reactive.result.method.annotation.MessageWriterResultHandlerTests` — FAIL
- useDefaultContentType() :: java.lang.IllegalStateException: Timeout on blocking read for 5000000000 NANOSECONDS

### `web.reactive.result.method.annotation.ProtobufIntegrationTests` — FAIL
- [3] Reactor Netty :: java.lang.IllegalStateException: java.lang.NullPointerException: Cannot invoke "java.net.InetSocketAddress.getPort()" because the return value of "reactor.netty.DisposableServer.address()" is null

### `web.reactive.result.method.annotation.RequestMappingExceptionHandlingIntegrationTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `web.reactive.result.method.annotation.RequestMappingMessageConversionIntegrationTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `web.reactive.result.view.FragmentViewResolutionResultHandlerTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `web.reactive.result.view.ViewResolutionResultHandlerTests` — FAIL
- defaultViewNameWithRedirectPrefixFails() :: java.lang.AssertionError: expectation "expectError(Class)" failed (expected: onError(ResponseStatusException); actual: onComplete())

### `web.reactive.result.view.script.JRubyScriptTemplateTests` — FAIL
- renderTemplate() :: java.lang.IllegalStateException: Failed to evaluate script [org/springframework/web/reactive/result/view/script/jruby/render.rb]

### `web.reactive.result.view.script.JythonScriptTemplateTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `web.server.session.WebSessionIntegrationTests` — FAIL

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

### `web.servlet.config.MvcNamespaceTests` — FAIL
- customConversionService() :: java.lang.AssertionError: 

### `web.servlet.config.annotation.ViewResolutionIntegrationTests` — FAIL
- freemarkerWithExplicitDefaultEncodingAndContentType() :: org.opentest4j.AssertionFailedError: 
- groovyMarkup() :: jakarta.servlet.ServletException: Handler processing failed: BUG! exception in phase 'instruction selection' in source unit 'file:/data/data/wt-osr-other516-20260708-2131/apps/spring-framework/spring-webmvc/build/resources/test/org/springframework/web/servlet/config/annotation/WEB-INF/index.tpl' unexpected NullPointerException

### `web.servlet.handler.HandlerMappingIntrospectorTests` — FAIL
- [1] uri = "/test" :: org.opentest4j.AssertionFailedError: 
- [2] uri = "/resource/1234****" :: org.opentest4j.AssertionFailedError: 
- cacheFilterWithNestedDispatch() :: org.opentest4j.AssertionFailedError: 

### `web.servlet.mvc.method.annotation.ExceptionHandlerExceptionResolverTests` — FAIL
- resolveExceptionResponseWriter() :: org.opentest4j.AssertionFailedError: 

### `web.servlet.mvc.method.annotation.FragmentRenderingStreamTests` — TIMEOUT
- (hard timeout at 120s, no stack captured)

### `web.servlet.mvc.method.annotation.ServletAnnotationControllerHandlerMethodTests` — FAIL
- [1] true :: org.opentest4j.AssertionFailedError: 
- [2] false :: org.opentest4j.AssertionFailedError: 

### `web.servlet.tags.EvalTagTests` — FAIL
- environmentAccess() :: org.opentest4j.AssertionFailedError: 
- mapAccess() :: org.opentest4j.AssertionFailedError: 
- printHtmlEscapedAttributeResult() :: org.opentest4j.AssertionFailedError: 
- printJavaScriptEscapedAttributeResult() :: org.opentest4j.AssertionFailedError: 
- printScopedAttributeResult() :: org.opentest4j.AssertionFailedError: 

### `web.servlet.view.DefaultFragmentsRenderingTests` — FAIL
- render() :: org.opentest4j.AssertionFailedError: 

### `web.servlet.view.script.JRubyScriptTemplateTests` — FAIL
- renderTemplate() :: java.lang.IllegalStateException: Failed to evaluate script [org/springframework/web/servlet/view/script/jruby/render.rb]

### `web.servlet.view.script.JythonScriptTemplateTests` — FAIL
- renderTemplate() :: org.opentest4j.AssertionFailedError: 

### `web.socket.config.MessageBrokerBeanDefinitionParserTests` — ABEND
    == org.springframework.web.socket.config.MessageBrokerBeanDefinitionParserTests ABEND rc=134 ==

### `web.socket.config.annotation.WebSocketConfigurationTests` — ABEND
    == org.springframework.web.socket.config.annotation.WebSocketConfigurationTests ABEND rc=134 ==

### `web.socket.handler.ConcurrentWebSocketSessionDecoratorTests` — ABEND
    == org.springframework.web.socket.handler.ConcurrentWebSocketSessionDecoratorTests ABEND rc=134 ==
    timeout: the monitored command dumped core

### `web.socket.messaging.StompWebSocketIntegrationTests` — ABEND
    == org.springframework.web.socket.messaging.StompWebSocketIntegrationTests ABEND rc=134 ==
    INFO [org.apache.catalina.core.StandardService] Stopping service [Tomcat]
    INFO [org.apache.coyote.http11.Http11NioProtocol] Stopping ProtocolHandler ["http-nio-auto-4-36389"]
    [2m2026-07-08T23:46:51.417011Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20087045ba0 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-08T23:46:51.417050Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20087045ba0 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-08T23:46:51.454543Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20087045ba0 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-08T23:46:51.454595Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20087045ba0 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-08T23:46:51.454604Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20087045ba0 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-08T23:46:51.454615Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20087045ba0 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)
    [2m2026-07-08T23:46:55.049334Z[0m [31mERROR[0m [2mcratonvm::gc::guard[0m[2m:[0m gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with) [3mobj[0m[2m=[0m0x20087045ba0 [3mindex[0m[2m=[0m0 [3mnum_slots[0m[2m=[0m0 [3mclass_id[0m[2m=[0mClassId(6) [3mclass_name[0m[2m=[0mjava/lang/String [3mreal_field_count[0m[2m=[0mSome(4)

### `web.socket.messaging.SubProtocolWebSocketHandlerTests` — FAIL
- checkSession() :: java.lang.IllegalStateException: No handler for 'v12.stomp' among {}
- subProtocolDefaultHandlerOnly() :: java.lang.IllegalStateException: No handler for 'v12.sToMp' among {}
- subProtocolMatch() :: java.lang.IllegalStateException: No handler for 'v12.sToMp' among {}

### `web.socket.sockjs.transport.handler.HttpSendingTransportHandlerTests` — ABEND

### `web.socket.sockjs.transport.session.SockJsSessionTests` — ABEND

### `web.util.UriComponentsBuilderTests` — FAIL
- fromOpaqueUri() :: org.opentest4j.AssertionFailedError: 

