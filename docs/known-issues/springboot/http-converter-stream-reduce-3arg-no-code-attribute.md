# `HttpMessageConvertersAutoConfigurationTests` — `Stream.reduce(identity, accumulator, combiner)` "has no Code attribute"

**Status: OPEN — found 2026-07-17**

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

## Root cause (hypothesis — the message itself pins the defect class, but the CratonVM registration site was not located in this pass)

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

**Not root-caused to a specific `native-builtins`/interpreter file:line in
this pass** — the `Stream` native-registration source (likely
`native-builtins/src/lang_stream.rs` or similar, not opened in this
investigation) was not read to confirm which `reduce` overloads are wired
up and which are missing.

**What would confirm/refute:** grep the `Stream`-related native registration
table for all `"reduce"` entries and compare their descriptors against the
three real JDK `Stream.reduce` overloads
(`reduce(BinaryOperator)`, `reduce(T,BinaryOperator)`,
`reduce(U,BiFunction,BinaryOperator)`); a standalone one-liner
(`Stream.of(1,2,3).reduce(0, (a,b) -> a+b, Integer::sum)`) would reproduce
in isolation if confirmed missing.

## Affected classes

| Module | Class | Failing tests |
|---|---|---|
| `module/spring-boot-http-converter` | `org.springframework.boot.http.converter.autoconfigure.HttpMessageConvertersAutoConfigurationTests` | 1 of 38 (`typeConstrainedConverterFromSpringDataDoesNotPreventAutoConfigurationOfJacksonConverter`) |
