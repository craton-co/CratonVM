# `RepeatableContainers$StandardRepeatableContainers` method-cache lookup returns a non-`Method` object — `ChildManagementContextInitializerAotTests`

**Status: OPEN — found 2026-07-17, hypothesis unconfirmed**

## Symptom

```
JUnit Jupiter:ChildManagementContextInitializerAotTests:aotContributedInitializerStartsManagementContext(CapturedOutput)
  => org.springframework.beans.factory.BeanDefinitionStoreException: Failed to parse configuration class [org.springframework.boot.actuate.autoconfigure.endpoint.web.WebEndpointAutoConfiguration]
     org.springframework.context.annotation.ConfigurationClassParser.parse(ConfigurationClassParser.java:196)
     ...
   Caused by: java.lang.ClassCastException: java.lang.Object cannot be cast to java.lang.reflect.Method
     org.springframework.core.annotation.RepeatableContainers$StandardRepeatableContainers.getRepeatedAnnotationsMethod(RepeatableContainers.java:266)
     org.springframework.core.annotation.RepeatableContainers$StandardRepeatableContainers.findRepeatedAnnotations(RepeatableContainers.java:256)
     org.springframework.core.annotation.AnnotationTypeMappings.addMetaAnnotationsToQueue(AnnotationTypeMappings.java:97)
     org.springframework.core.annotation.AnnotationTypeMappings.<init>(AnnotationTypeMappings.java:74)
     org.springframework.core.annotation.AnnotationTypeMappings$Cache.createMappings(AnnotationTypeMappings.java:290)
     org.springframework.core.annotation.TypeMappedAnnotation.of(TypeMappedAnnotation.java:615)
     org.springframework.core.type.classreading.ClassFileAnnotationDelegate.createMergedAnnotation(ClassFileAnnotationDelegate.java:80)
     org.springframework.core.type.classreading.ClassFileMetadataReader.<init>(ClassFileMetadataReader.java:45)
     org.springframework.context.annotation.ConfigurationClassParser.retrieveBeanMethodMetadata(ConfigurationClassParser.java:466)
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-actuator-autoconfigure.org.springframework.boot.actuate.autoconfigure.web.s-ca2e24fee868.out.log`

Only 1 of 1 tests in this class; the class is an AOT-processing test
(`ApplicationContextAotGenerator.processAheadOfTime`) exercising annotation
metadata reading from `.class` bytecode (`ClassFileAnnotationDelegate`,
"class-file" annotation reading path used during AOT generation, distinct
from reflective/runtime annotation reading).

## Root cause

**Not confirmed — hypothesis.** `RepeatableContainers$StandardRepeatableContainers`
caches, per repeatable-annotation-container `Class`, either the resolved
`value()` accessor `Method` or a sentinel "no repeated-annotations method"
marker object, in a `Map<Class<? extends Annotation>, Object>`
(`ConcurrentReferenceHashMap` in real Spring source). Its lookup helper does
roughly:

```java
Object result = this.cache.computeIfAbsent(annotationType, this::computeRepeatedAnnotationsMethod);
return (result != NONE) ? (Method) result : null;
```

The `ClassCastException` means `result` was neither `NONE` (the sentinel)
nor a `Method` — i.e. the cache returned some *other* object for this key.
The two live candidates, neither confirmed:

1. **Map correctness under CratonVM.** If CratonVM's `Map`/`ConcurrentHashMap`-family
   implementation ever returns a value from a *different* key (a hash/bucket
   collision or stale-entry bug), `getRepeatedAnnotationsMethod` would
   receive whatever heterogeneous object was cached for a colliding
   annotation type instead of the `Method`/`NONE` sentinel expected for
   *this* type. This project has prior confirmed instances of CratonVM
   collection/cache implementations returning wrong entries under specific
   shapes (see `reference_invoke_virtual_native_dispatch_cache_quirk`,
   the `HASHMAP_NATIVE_DISPATCH_CACHE` mentions in
   `docs/known-issues/springboot/README.md`'s `collectionbindertests-classcast-testdescriptor-crash.md`
   row) — same general "wrong-value-from-native-collection" shape.
2. **Sentinel identity.** If the `NONE` sentinel's reference identity is not
   stable across some CratonVM code path (e.g. a static field being
   re-read/re-initialized), `result != NONE` could spuriously evaluate
   `true` for the actual `NONE` value, causing the cast to run on the
   sentinel object itself.

Neither was traced to a specific native/file:line this session — this
happens entirely inside real Spring Framework bytecode
(`RepeatableContainers`, `AnnotationTypeMappings`, `ClassFileAnnotationDelegate`
are all real `.class` files, no CratonVM native substitution known for
this call path), so the bug is most likely in a shared, lower-level
CratonVM primitive (`Map`/cache implementation) rather than anything
annotation-specific. Confirming would require a standalone probe
constructing a `Map` with colliding/adversarial keys and checking
`computeIfAbsent`/`get` fidelity under CratonVM vs. HotSpot, or attaching
`CRATONVM_DBG_REFLECT`-style tracing to a live repro of this exact class.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-actuator-autoconfigure` | `org.springframework.boot.actuate.autoconfigure.web.server.ChildManagementContextInitializerAotTests` |
