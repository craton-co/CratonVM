# Hibernate Validator rejects `ArgumentValueValueExtractor` (`HV000203`) — `ControllerEndpointDiscovererTests`

**Status: OPEN — found 2026-07-17, hypothesis unconfirmed**

## Symptom

```
JUnit Jupiter:ControllerEndpointDiscovererTests:<2 test methods>
  => java.lang.IllegalStateException: Unstarted application context ...[startupFailure=BeanCreationException] failed to start
   Caused by: org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'defaultValidator' defined in ...$ProxyBeanConfiguration: HV000203: Value extractor type org.springframework.graphql.data.method.annotation.support.ArgumentValueValueExtractor fails to declare the extracted type parameter using @ExtractedValue.
   Caused by: jakarta.validation.valueextraction.ValueExtractorDefinitionException: HV000203: Value extractor type org.springframework.graphql.data.method.annotation.support.ArgumentValueValueExtractor fails to declare the extracted type parameter using @ExtractedValue.
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-actuator.org.springframework.boot.actuate.endpoint.web.annotation.Controlle-23f2a1805bec.out.log`
(both failing test methods hit the identical exception during
`defaultValidator` bean creation.)

## What the class under scrutiny looks like (confirmed via `javap` on the real jar)

`org.springframework.graphql.data.method.annotation.support.ArgumentValueValueExtractor`
(`spring-graphql-2.0.4-SNAPSHOT.jar`):

```java
public final class ArgumentValueValueExtractor
    implements jakarta.validation.valueextraction.ValueExtractor<org.springframework.graphql.data.ArgumentValue<?>>
```

The extracted type argument to `ValueExtractor<T>` is itself a
**parameterized type**, `ArgumentValue<?>` (not a plain class) — a nested
generic. Hibernate Validator's extractor-registration check
(`HV000203`) needs to walk `getGenericInterfaces()` on the extractor class
to find the `ValueExtractor<...>` interface, resolve its type argument as a
`ParameterizedType` (`ArgumentValue<?>`), then find `@ExtractedValue` on
`ArgumentValue<T>`'s own declared type-parameter `T`. This same class
passes registration on real HotSpot (the class is unmodified,
ships in `spring-graphql`, and this suite's own real-HotSpot baseline the
same day did not flag it) — CratonVM is the one rejecting it.

## Root cause

**Not confirmed — hypothesis.** Leading suspect: CratonVM's reflective
generic-signature machinery (`Class.getGenericInterfaces()` /
`ParameterizedType.getActualTypeArguments()` / annotated-type-parameter
lookup) loses or flattens the **nested** parameterization here — i.e. it
may report the raw `ArgumentValue` interface/type instead of the
`ParameterizedType` wrapping it, or fail to expose the `@ExtractedValue`
annotation on `ArgumentValue`'s own type-parameter declaration when reached
through this second level of generic nesting. That would make Hibernate
Validator's `@ExtractedValue`-search come up empty, matching HV000203's
message exactly. Not traced to a specific CratonVM native/file:line this
session — would need a standalone probe calling
`ArgumentValueValueExtractor.class.getGenericInterfaces()` and walking the
returned `ParameterizedType` under CratonVM vs. real HotSpot to confirm
where the two diverge.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-actuator` | `org.springframework.boot.actuate.endpoint.web.annotation.ControllerEndpointDiscovererTests` (both failing test methods) |
