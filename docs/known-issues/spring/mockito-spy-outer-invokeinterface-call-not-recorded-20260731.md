# Mockito `spy()` on a class reached via an interface field does not record the outer call

**Status: OPEN — found 2026-07-31**

## Symptom

`ConversionServiceParameterValueMapperTests.mapParameterShouldDelegateToConversionService`
fails a Mockito verification:

```
=> Argument(s) are different! Wanted:
defaultFormattingConversionService.convert(
    "123",
    class java.lang.Integer
);
-> at org.springframework.core.convert.support.GenericConversionService.convert(GenericConversionService.java:164)
Actual invocations have different arguments:
defaultFormattingConversionService.convert(
    "123",
    java.lang.String,
    java.lang.Integer
);
-> at org.springframework.core.convert.support.GenericConversionService.convert(GenericConversionService.java:164)
defaultFormattingConversionService.getConverter(
    java.lang.String,
    java.lang.Integer
);
-> at org.springframework.core.convert.support.GenericConversionService.convert(GenericConversionService.java:179)
```

## Root cause (diagnosis, not yet fully pinned down)

The test (`apps/spring-boot/module/spring-boot-actuator/src/test/java/org/springframework/boot/actuate/endpoint/invoke/convert/ConversionServiceParameterValueMapperTests.java:49-54`)
does:

```java
DefaultFormattingConversionService conversionService = spy(new DefaultFormattingConversionService());
ConversionServiceParameterValueMapper mapper = new ConversionServiceParameterValueMapper(conversionService);
mapper.mapParameterValue(new TestOperationParameter(Integer.class), "123");
then(conversionService).should().convert("123", Integer.class);
```

`ConversionServiceParameterValueMapper.mapParameterValue`
(`.../invoke/convert/ConversionServiceParameterValueMapper.java:60`) calls
`this.conversionService.convert(value, parameter.getType())` where the field
is typed as the **interface** `ConversionService` — so the call site is an
`invokeinterface` on the spy. `GenericConversionService.convert(Object,
Class)`'s own real-bytecode body (line 164) then internally calls
`convert(Object, TypeDescriptor, TypeDescriptor)` (line ~166), which in turn
calls `getConverter(...)` (line 179) — both of those are `invokevirtual`
self-calls on `this` within `GenericConversionService`'s own class.

The Mockito log shows `Mockito is currently self-attaching to enable the
inline-mock-maker` — Mockito's default (inline) mock maker retransforms the
real class bytecode via `Instrumentation`, so every call on the spied
instance, including internal self-calls, is expected to be interceptable.

What actually gets recorded on CratonVM: the two *internal* self-calls
(`convert(Object,TypeDescriptor,TypeDescriptor)` and `getConverter`) show up
in Mockito's invocation history, but the *outer* call —
`convert(Object,Class)`, made via `invokeinterface` from
`ConversionServiceParameterValueMapper` against the interface-typed field —
is missing entirely. The real method still executed correctly (`mapped`
equals `123`, so the first assertion passes), so the call happened; it just
isn't being recorded/matched by Mockito against the expected 2-arg overload.

This points at a CratonVM dispatch difference between `invokeinterface` calls
into an inline-mock-maker-instrumented instance (not recorded) versus
`invokevirtual` self-calls the same instrumented bytecode makes on itself
(recorded correctly). Not root-caused to a specific source location within
this pass — needs a smaller repro isolating `invokeinterface` vs
`invokevirtual` dispatch against a Mockito inline-mock-maker spy to confirm
and locate the defect (candidate areas: interface-method resolution for
redefined/retransformed classes, or the interceptor hook only being wired
into `invokevirtual`'s call-site cache and not `invokeinterface`'s).

## Affected classes

- `module/spring-boot-actuator` — `org.springframework.boot.actuate.endpoint.invoke.convert.ConversionServiceParameterValueMapperTests`
