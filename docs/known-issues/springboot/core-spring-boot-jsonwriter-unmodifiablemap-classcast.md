# `JsonWriterTests$ValueProcessorTests`: CratonVM's `Collections.unmodifiableMap()` wrapper leaks into a `String`-typed lambda parameter

**Status: OPEN — found 2026-07-17**

## Symptom

`core/spring-boot`, `org.springframework.boot.json.JsonWriterTests`, 3 of
86 tests fail (all in the nested `ValueProcessorTests` class), all with the
identical `ClassCastException`:

```
JUnit Jupiter:JsonWriterTests:ValueProcessorTests:processValueWhenInMap()
  => java.lang.ClassCastException: cratonvm.internal.UnmodifiableMap cannot be cast to java.lang.String
     org.springframework.boot.json.JsonWriter$ValueProcessor.lambda$of$0(JsonWriter.java:1059)
     org.springframework.boot.json.JsonValueWriter.lambda$processValue$0(JsonValueWriter.java:354)
     org.springframework.boot.util.LambdaSafe$Callback.lambda$invokeAnd$0(LambdaSafe.java:271)
     org.springframework.boot.util.LambdaSafe$LambdaSafeCallback.invoke(LambdaSafe.java:161)
     org.springframework.boot.util.LambdaSafe$Callback.invokeAnd(LambdaSafe.java:272)
     org.springframework.boot.json.JsonValueWriter.processValue(JsonValueWriter.java:354)
     org.springframework.boot.json.JsonValueWriter.processValue(JsonValueWriter.java:343)
     org.springframework.boot.json.JsonValueWriter.write(JsonValueWriter.java:123)
     org.springframework.boot.json.JsonValueWriter.write(JsonValueWriter.java:104)
     org.springframework.boot.json.JsonWriter$Member.write(JsonWriter.java:654)
     org.springframework.boot.json.JsonWriter$Members.write(JsonWriter.java:342)
     org.springframework.boot.json.JsonWriter.lambda$of$0(JsonWriter.java:156)
     org.springframework.boot.json.JsonWriter.lambda$write$0(JsonWriter.java:106)
     org.springframework.boot.json.WritableJson$1.to(WritableJson.java:164)
     org.springframework.boot.json.WritableJson.toJsonString(WritableJson.java:55)
     org.springframework.boot.json.JsonWriter.writeToString(JsonWriter.java:96)
     org.springframework.boot.json.JsonWriterTests$ValueProcessorTests.processValueWhenInMap(JsonWriterTests.java:815)
```

The same exact message/shape repeats for `processValueWhen()` and
`processValueWhenInNestedMap()` (lines 1035 and 1059 of `JsonWriter.java`,
both `ValueProcessor` lambda bodies).

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard1/logs/core_spring-boot.org.springframework.boot.json.JsonWriterTests.out.log`

## Root cause (hypothesis, grounded in the observed type but not traced to a specific native call site)

`cratonvm.internal.UnmodifiableMap` is a CratonVM-internal collection
wrapper type (analogous to real JDK's package-private
`java.util.Collections$UnmodifiableMap`) — it should never be visible to,
or reach, ordinary Java bytecode that expects a `java.lang.String`. The
crash sites (`JsonWriter.java:1035`/`1059`, inside
`JsonWriter$ValueProcessor`'s `of(...)`/`when(...)` lambda bodies) are
generic `BiFunction`/predicate callbacks that Spring Boot's `JsonValueWriter`
invokes once it has already dispatched on the *declared* value type — i.e.
by the time these lambdas run, calling code expects the value parameter to
already be narrowed to a `String` (per the test names —
`processValueWhenInMap`/`processValueWhenInNestedMap` are specifically
about processing `String` values found *inside* a `Map`).

The most likely explanation is that CratonVM's `Map`/`Collections`
machinery for **map values accessed while iterating an unmodifiable-wrapped
map** returns the raw internal `UnmodifiableMap` wrapper object itself
(instead of the actual entry's value) in some code path exercised by
`JsonValueWriter`'s recursive map-processing (`processValue` calling itself
for nested maps, per the stack: `processValue(JsonValueWriter.java:343)` →
`processValue(...:354)`), or that map-entry-value extraction for a
CratonVM-internal `UnmodifiableMap` receiver has a bug where it hands back
the wrapper itself in place of the properly-unwrapped value. This is the
same general shape as this project's history of "internal collection
wrapper type leaks past its intended boundary" bugs (see
`reference_hib_temporal_gc_lambda_native_corruption` and the
`SC-map-multivaluemap-family.md`/`structured-logging-map-entry-getkey-lambda-dispatch-precedence-FIXED.md`
docs in this codebase's history for the same *family* of "wrong
value/receiver surfaces from a native collection" defect), but the exact
call site inside `cratonvm.internal.UnmodifiableMap`'s value-accessor path
was not located this session — filed as OPEN with the confirmed symptom and
call chain; whoever picks this up next should set a breakpoint /
`CRATONVM_DBG`-style trace inside `JsonValueWriter.processValue`
(`processValue(JsonValueWriter.java:343-354)`) to see exactly which map
lookup/iteration call is handing back the wrapper instead of the value.

## Affected classes

| Module | Class |
|---|---|
| `core/spring-boot` | `org.springframework.boot.json.JsonWriterTests` (`ValueProcessorTests` nested class, 3/86 tests: `processValueWhenInMap`, `processValueWhen`, `processValueWhenInNestedMap`) |
