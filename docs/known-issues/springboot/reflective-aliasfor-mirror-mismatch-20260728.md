# `@Reflective`'s `value`/`processors` `@AliasFor` mirror pair disagree — `AnnotationConfigurationException` during bean post-processing

**Status: OPEN — found 2026-07-28**

## Symptom

| Module | Class | Test method |
|---|---|---|
| `module/spring-boot-servlet` | `org.springframework.boot.servlet.autoconfigure.MultipartAutoConfigurationTests` | `webServerWithMultipartConfigDisabled` |

```
=> org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'controller' defined in org.springframework.boot.servlet.autoconfigure.MultipartAutoConfigurationTests$WebServerWithNoMultipartTomcat: Post-processing of merged bean definition failed
     Caused by: org.springframework.core.annotation.AnnotationConfigurationException: Different @AliasFor mirror values for annotation [org.springframework.aot.hint.annotation.Reflective]; attribute 'processors' and its alias 'value' are declared with values of [{class org.springframework.aot.hint.annotation.SimpleReflectiveProcessor}] and [{class org.springframework.web.bind.annotation.ControllerMappingReflectiveProcessor}].
       org.springframework.core.annotation.AnnotationTypeMapping$MirrorSets$MirrorSet.resolve(AnnotationTypeMapping.java:649)
       ...
       org.springframework.beans.factory.annotation.AutowiredAnnotationBeanPostProcessor.findAutowiredAnnotation(AutowiredAnnotationBeanPostProcessor.java:612)
       org.springframework.beans.factory.annotation.AutowiredAnnotationBeanPostProcessor.buildAutowiringMetadata(AutowiredAnnotationBeanPostProcessor.java:572)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260728/shard2/logs/module_spring-boot-servlet.org.springframework.boot.servlet.autoconfigure.MultipartAutoConf-3522db47dd3e.out.log`

Thrown while `AutowiredAnnotationBeanPostProcessor` scans a controller
bean's methods for merged `@RequestMapping`-family annotations
(`TypeMappedAnnotations`/`AnnotationTypeMapping` building the meta-annotation
closure), specifically while resolving `@Reflective` as a meta-annotation.

## Root cause (confirmed via javap disassembly of the real jars)

`org.springframework.aot.hint.annotation.Reflective`
(`spring-core-7.1.0-SNAPSHOT.jar`) declares:

```java
public @interface Reflective {
    @AliasFor("processors")
    Class<? extends ReflectiveProcessor>[] value() default SimpleReflectiveProcessor.class;

    @AliasFor("value")
    Class<? extends ReflectiveProcessor>[] processors() default SimpleReflectiveProcessor.class;
}
```

`value` and `processors` are `@AliasFor` mirrors of each other with a shared
default (`SimpleReflectiveProcessor.class`) — reading either attribute on
*any single, specific* usage of `@Reflective` must always return the same
value.

`org.springframework.web.bind.annotation.RequestMapping`
(`spring-web-7.1.0-SNAPSHOT.jar`) is meta-annotated
`@Reflective(ControllerMappingReflectiveProcessor.class)` (confirmed via
`javap -v`, its `RuntimeVisibleAnnotations` carries
`Lorg/springframework/aot/hint/annotation/Reflective;` with a `Class` array
value naming `ControllerMappingReflectiveProcessor`). Per the `@AliasFor`
contract, resolving `@Reflective` on `@RequestMapping` must yield
`processors() == value() == [ControllerMappingReflectiveProcessor.class]`.

The observed error shows CratonVM's merged-annotation machinery instead
returning **different** values for the two mirrored attributes of what
should be the *same* `@Reflective` annotation instance:
`processors` = `[SimpleReflectiveProcessor]` (the **default**), `value` =
`[ControllerMappingReflectiveProcessor]` (the **explicit** value actually
declared on `@RequestMapping`). This is not a case of two different
annotations disagreeing — `MirrorSet.resolve` is comparing two attribute
*accessors of the same annotation instance* and finding them inconsistent,
which per the `@AliasFor` contract should be structurally impossible unless
one of the two accessor calls returned a stale or wrongly-scoped value.

**Hypothesis (not confirmed by tracing the actual CratonVM annotation-value
resolution code this session):** something in CratonVM's dynamic annotation
attribute-value resolution (whether a JDK-style `AnnotationInvocationHandler`
proxy, or CratonVM's own merged-annotation-attribute cache) is caching
`processors()`'s return value keyed loosely enough (e.g. per declaring
annotation *type*, or per attribute *name*, rather than per concrete
annotation *usage*/instance) that a different, unrelated `@Reflective` usage
elsewhere in the same context — one that never overrode `processors`/`value`
and so still carries the shared default `SimpleReflectiveProcessor` — leaks
its cached value into this lookup for `@RequestMapping`'s own, explicitly-
overridden `@Reflective`. This would explain why only `processors` (not
`value`, which was read correctly) came back wrong: if only one of the two
mirror accessors is memoized this way while the other is always freshly
resolved from the real annotation data, exactly one of the two would read
stale.

This is not confirmed to be the same underlying cache as any other
already-documented CratonVM annotation-merging bug in this codebase (e.g.
the `OnBeanCondition`/`ObjectProvider` generic-identity mismatches in
`isolated-loader-onbeancondition-type-deduction-bypass-FIXED.md` and
`isolated-loader-objectprovider-generic-identity-mismatch-FIXED.md`) — those
concern classloader-isolation and generic-type identity, not `@AliasFor`
mirror-value resolution specifically. Worth a joint look given the shared
symptom family ("merged-annotation/reflective metadata returns a stale value
from an unrelated usage"), but not claimed identical here.

## Confirming/refuting this hypothesis

Add a temporary trace around whatever CratonVM code answers
`InvocationHandler.invoke`/attribute-value lookups for a dynamic annotation
proxy of type `Reflective`, gated on the annotation type name, and rerun
`MultipartAutoConfigurationTests` (or, more narrowly, any test that triggers
`AutowiredAnnotationBeanPostProcessor` scanning a `@RequestMapping`-annotated
controller method) to see which annotation *usage* each of the two
`processors()`/`value()` calls actually resolved against.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-servlet` | `org.springframework.boot.servlet.autoconfigure.MultipartAutoConfigurationTests` |
