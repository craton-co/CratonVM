# A Class-valued annotation element/array entry intermittently resolves to `null`, crashing three independent reflection-heavy consumers (Spring's class-file metadata reader, JUnit's `AnnotationUtils`, Byte Buddy's parameter binder)

**Status: OPEN — found 2026-08-05**

## Symptom

Three unrelated frameworks, all walking annotation metadata reflectively/via
class-file parsing under CratonVM, hit the same shape of NPE: code that
unconditionally calls `.getName()`, `.equals()`, or `.represents()` on what
should be a resolved `Class`/`TypeDescription` gets `null` instead and NPEs.
HotSpot passes all affected classes 100% (`hotspot-baseline-latest.tsv`:
`RestClientAutoConfigurationTests` 15/15, `ReactiveOAuth2ResourceServerAutoConfigurationTests`
50/50, `TracingAndMeterObservationHandlerGroupTests` 4/4).

### Variant 1 — Spring's `java.lang.classfile`-based `ConfigurationClassParser`

`RestClientAutoConfigurationTests.shouldSupplyRestClientSslIfSslBundlesIsThereWithCustomHttpSettingsAndBuilder`
(context fails to start) and all 6 failures in
`ReactiveOAuth2ResourceServerAutoConfigurationTests` (`autoConfigurationWhenJwkSetUriNullShouldNotFail`,
`shouldConfigureJwtConverterIfPrincipalClaimNameIsSet`,
`autoConfigurationShouldBeConditionalOnReactiveJwtDecoderClass`,
`jwtDecoderByIssuerUriBeanIsConditionalOnMissingBean`,
`autoConfigurationShouldConfigureResourceServerUsingOidcIssuerUri`,
`autoConfigurationWhenSecurityWebFilterChainConfigPresentShouldNotAddOne`):

```
org.springframework.beans.factory.BeanDefinitionStoreException: Failed to parse
configuration class [...ReactiveHttpClientAutoConfiguration] / [...ReactiveOAuth2ResourceServerAutoConfiguration]
Caused by: java.lang.NullPointerException: Cannot invoke "java.lang.Class.getName()" because "type" is null
	at org.springframework.core.annotation.AnnotationFilter.matches(AnnotationFilter.java:117)
	at org.springframework.core.annotation.AnnotationsScanner.isIgnorable(AnnotationsScanner.java:468)
	at org.springframework.core.annotation.AnnotationTypeMappings.addMetaAnnotationsToQueue(AnnotationTypeMappings.java:92)
	at org.springframework.core.annotation.AnnotationTypeMappings.addAllMappings(AnnotationTypeMappings.java:87)
	at org.springframework.core.annotation.AnnotationTypeMappings.<init>(AnnotationTypeMappings.java:74)
	at org.springframework.core.annotation.AnnotationTypeMappings$Cache.createMappings(AnnotationTypeMappings.java:290)
	at org.springframework.core.annotation.TypeMappedAnnotation.of(TypeMappedAnnotation.java:615)
	at org.springframework.core.annotation.MergedAnnotation.of(MergedAnnotation.java:632)
	at org.springframework.core.type.classreading.ClassFileAnnotationDelegate.createMergedAnnotation(ClassFileAnnotationDelegate.java:80)
	at org.springframework.core.type.classreading.ClassFileMethodMetadata.of(ClassFileMethodMetadata.java:155)
	at org.springframework.core.type.classreading.ClassFileMetadataReader.<init>(ClassFileMetadataReader.java:45)
	at org.springframework.context.annotation.ConfigurationClassParser.retrieveBeanMethodMetadata(ConfigurationClassParser.java:466)
	at org.springframework.context.annotation.ConfigurationClassParser.processImports(ConfigurationClassParser.java:634/644)
```
(one instance instead reads `"annType" is null`, same call shape, same fix target.)

This is Spring Framework's newer `ClassFileMetadataReader`/`ClassFileAnnotationMetadata`
(built on the JDK's own `java.lang.classfile` API, JEP 484) reading
`RuntimeVisibleAnnotations`/meta-annotations directly off `.class` bytes —
**not** ASM, and not `java.lang.reflect`. Somewhere while queuing a bean
method's meta-annotations, CratonVM's class-file annotation reader hands
back a mapping entry whose declared annotation type is `null` instead of a
resolved `Class`.

### Variant 2 — JUnit's `AnnotationUtils.findRepeatableAnnotations` (plain reflection)

`RestClientAutoConfigurationTests.shouldSupplyRestClientSslIfSslBundlesIsThereWithAutoConfiguredHttpSettingsAndBuilder`
and `configurerShouldCallCustomizers` — this one crashes JUnit's own test
*infrastructure*, before the test body even runs:

```
java.lang.NullPointerException: Cannot invoke "Object.equals(Object)" because "candidateAnnotationType" is null
	at org.junit.platform.commons.util.AnnotationUtils.findRepeatableAnnotations(AnnotationUtils.java:341/325/370/325/297)
	at org.junit.jupiter.engine.descriptor.ExtensionUtils.streamDeclarativeExtensionTypes(ExtensionUtils.java:214)
	at org.junit.jupiter.engine.descriptor.ExtensionUtils.populateNewExtensionRegistryFromExtendWithAnnotation(ExtensionUtils.java:78)
	at org.junit.jupiter.engine.descriptor.TestMethodTestDescriptor.populateNewExtensionRegistry(TestMethodTestDescriptor.java:139)
```

`findRepeatableAnnotations` recurses over a `@Repeatable` container
annotation's `value()` array, comparing each entry's `annotationType()`
against the candidate — here an array entry's `annotationType()` (or the
container's own declared element type) comes back `null`.

### Variant 3 — Byte Buddy's `TargetMethodAnnotationDrivenBinder` (plain reflection over Byte Buddy's own bundled classes)

All 3 failures in `TracingAndMeterObservationHandlerGroupTests`
(`registerMembersOnlyUsesCompositeWhenMoreThanOneHandler`,
`registerMembersWrapsMeterObservationHandlersAndRegistersDistinctGroups`,
`isMemberAcceptsMeterObservationHandlerOrTracingObservationHandler`) — all
`Mockito.mock(MeterObservationHandler.class)` (an interface, routed through
Byte Buddy's `SubclassBytecodeGenerator`, not the inline retransformer):

```
org.mockito.exceptions.base.MockitoException: Mockito cannot mock this class: interface io.micrometer.core.instrument.observation.MeterObservationHandler.
Underlying exception : java.lang.NullPointerException: Cannot invoke "net.bytebuddy.description.type.TypeDescription.represents(java.lang.reflect.Type)"
  because the return value of "...ForFieldBinding.declaringType(AnnotationDescription$Loadable)" is null
	at net.bytebuddy.implementation.bind.annotation.TargetMethodAnnotationDrivenBinder$ParameterBinder$ForFieldBinding.bind(...java:353)
	at net.bytebuddy.implementation.bind.annotation.FieldValue$Binder.bind(FieldValue.java:140)
```

Here Byte Buddy is reading its own `@FieldValue` annotation's `declaringType`
element off one of its bundled interceptor methods via
`AnnotationDescription$Loadable` — again a `Class`-valued annotation element
resolving to `null` where the annotation is present and the element has a
real (non-default) value.

## Root cause (diagnosis, not yet pinned to a line)

Three different call paths — one via the new `java.lang.classfile`-based
metadata reader (`native-builtins/src/classfile_api.rs` /
`vm/src/vm/vm_init.rs`), one via ordinary `java.lang.reflect` annotation
proxies (`AnnotatedElement.getDeclaredAnnotations()` /
`Annotation.annotationType()`), one via Byte Buddy's own
`AnnotationDescription$Loadable` reflection wrapper — all reach the exact
same failure shape: a `Class`-typed annotation element (either a
single-value element or an array/`@Repeatable` container entry) that is
genuinely present with a real value comes back `null` instead of the
resolved `Class` object. Given the shared shape across three independent,
non-cooperating reflection front-ends, this points at one lower-level
CratonVM primitive shared by all three — most likely the annotation-element
`Class` resolution used when materializing an `Annotation` proxy (or a
`java.lang.classfile` `AnnotationValue.OfClass`/array element), rather than
three unrelated framework bugs. **Needs further investigation** to pin the
exact native/interpreter site; start with:

```
grep -rn "AnnotationValue\|annotation_element\|OfClass\|annotation_type" native-builtins/src/classfile_api.rs
grep -rn "fn.*annotation" native-builtins/src/lang_class.rs native-builtins/src/lang_invoke.rs
```

and a minimal repro comparing `getAnnotationsByType`/`getDeclaredAnnotations`
array-element resolution against HotSpot for a `@Repeatable` container and a
class-valued annotation element with a non-default value, both via ordinary
reflection and via `java.lang.classfile`.

## Affected classes
- `spring-boot-restclient` — `org.springframework.boot.restclient.autoconfigure.RestClientAutoConfigurationTests` (3/15 failed: 1 classfile variant, 2 JUnit-AnnotationUtils variant)
- `spring-boot-security-oauth2-resource-server` — `org.springframework.boot.security.oauth2.server.resource.autoconfigure.reactive.ReactiveOAuth2ResourceServerAutoConfigurationTests` (6/50 failed, classfile variant)
- `spring-boot-micrometer-tracing` — `org.springframework.boot.micrometer.tracing.autoconfigure.TracingAndMeterObservationHandlerGroupTests` (3/4 failed, Byte Buddy variant)
