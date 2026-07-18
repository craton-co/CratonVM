# `HttpMessageConvertersAutoConfigurationTests` — `Stream.reduce(identity, accumulator, combiner)` "has no Code attribute"

**Status: FIXED — 2026-07-18**

## Symptom

`typeConstrainedConverterFromSpringDataDoesNotPreventAutoConfigurationOfJacksonConverter`
is the only failure (1 of 38 tests):

```
java.lang.AssertionError:
Expecting:
 <Unstarted application context ... [startupFailure=org.springframework.beans.factory.BeanCreationException]>
to have a single bean of type:
 <org.springframework.boot.http.converter.autoconfigure.JacksonHttpMessageConvertersConfiguration.JacksonJsonHttpMessageConvertersCustomizer>:
but context failed to start:
 org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'halFormsTemplateBuilder' defined in class path resource [org/springframework/hateoas/mediatype/hal/forms/HalFormsMediaTypeConfiguration.class]: Failed to instantiate [org.springframework.hateoas.mediatype.hal.forms.HalFormsTemplateBuilder]: Factory method 'halFormsTemplateBuilder' threw exception with message: method java/util/stream/Stream.reduce(Ljava/lang/Object;Ljava/util/function/BiFunction;Ljava/util/function/BinaryOperator;)Ljava/lang/Object; has no Code attribute
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-http-converter.org.springframework.boot.http.converter.autoconfigure.HttpMe-26a1ff7207f2.out.log`

## Root cause (confirmed)

"Method X has no Code attribute" is CratonVM's own diagnostic for invoking a
method whose class-file entry is `abstract`/has no bytecode body and for
which no native implementation is registered to back it — i.e. this is a
**missing native registration**, not a Spring or HAL-Forms bug. The affected
overload is the 3-arg `Stream.reduce(U identity, BiFunction<U,? super T,U>
accumulator, BinaryOperator<U> combiner)` — the *general* reduce overload
used when the accumulator's return type differs from the stream's element
type (used somewhere in Spring HATEOAS's `HalFormsTemplateBuilder`
initialization, most likely folding a collection of parameters/fields into
a builder object). CratonVM evidently has native/interpreter support for
the simpler 1-arg (`reduce(BinaryOperator)`) and/or 2-arg
(`reduce(identity, BinaryOperator)`) overloads (this is the only Stream-related
failure across the whole batch, and `Stream` is used pervasively elsewhere
in this suite without incident) but not this specific 3-arg signature.

The original report did not locate the registration site. It is now confirmed
in `native-collections/src/lib.rs`; the exact missing descriptor and its
native implementation are documented below.

## Fix

The confirmed registration site is `native-collections/src/lib.rs`'s
`register_stream_natives`. It had registrations for the one-argument and
two-argument `reduce` overloads but not
`(Ljava/lang/Object;Ljava/util/function/BiFunction;Ljava/util/function/BinaryOperator;)Ljava/lang/Object;`.
The fallback to the bare `Stream` interface declaration therefore produced the
reported no-Code `AbstractMethodError`.

The missing registration and its sequential `BiFunction`-accumulator native
implementation are now present, with moving-GC roots pinned across callback
execution. The same fix closes the HATEOAS and REST Docs reports because they
all dispatch the identical interface method.

## Affected classes

| Module | Class | Failing tests |
|---|---|---|
| `module/spring-boot-http-converter` | `org.springframework.boot.http.converter.autoconfigure.HttpMessageConvertersAutoConfigurationTests` | 1 of 38 (`typeConstrainedConverterFromSpringDataDoesNotPreventAutoConfigurationOfJacksonConverter`) |

## Validation

`HttpMessageConvertersAutoConfigurationTests` passed all 38 tests with the
fix in both JIT (111.725s) and `--nojit` (103.264s) mode. It was run together
with the four HATEOAS/REST Docs classes sharing this descriptor; all 48 tests
passed in each mode, and neither run reported the former no-Code
`AbstractMethodError`.
