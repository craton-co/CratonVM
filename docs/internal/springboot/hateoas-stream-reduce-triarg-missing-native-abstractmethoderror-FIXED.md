# `Stream.reduce(identity, accumulator, combiner)` 3-arg overload not registered — `AbstractMethodError` breaks all HATEOAS media-type configuration

**Status: FIXED — 2026-07-18**

## Symptom

| Class | Failures |
|---|---:|
| `HypermediaAutoConfigurationTests` | 5/6 |
| `HypermediaWebMvcTestIntegrationTests` | 2/2 |

Every failure is the identical chain, bottoming out in:

```
Caused by: java.lang.AbstractMethodError: method java/util/stream/Stream.reduce(Ljava/lang/Object;Ljava/util/function/BiFunction;Ljava/util/function/BinaryOperator;)Ljava/lang/Object; has no Code attribute
       org.springframework.hateoas.mediatype.MediaTypeConfigurationFactory.getConfiguration(MediaTypeConfigurationFactory.java:73)
       org.springframework.hateoas.mediatype.hal.HalMediaTypeConfiguration.configureJsonMapper(HalMediaTypeConfiguration.java:84)
       org.springframework.hateoas.config.WebConverters.augment(WebConverters.java:104)
       org.springframework.hateoas.config.WebConverters.augmentServer(WebConverters.java:82)
       org.springframework.hateoas.config.WebMvcHateoasConfiguration$HypermediaWebMvcConfigurer.extendMessageConverters(WebMvcHateoasConfiguration.java:101)
       ...
       org.springframework.web.servlet.config.annotation.WebMvcConfigurationSupport.mvcContentNegotiationManager(WebMvcConfigurationSupport.java:402)
```

wrapped by Spring as a `BeanInstantiationException` for
`ContentNegotiationManager` / `UnsatisfiedDependencyException` for
`requestMappingHandlerMapping`. Since `mvcContentNegotiationManager` is a
dependency of nearly every `WebMvcAutoConfiguration` bean, this takes down
essentially the entire HATEOAS + WebMvc autoconfiguration surface — every
test in both classes that touches the web/hypermedia context fails this way
(the 1 passing test in `HypermediaAutoConfigurationTests` doesn't exercise
`ContentNegotiationManager`).

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-hateoas.org.springframework.boot.hateoas.autoconfigure.HypermediaAutoConfigurationTests.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-hateoas.org.springframework.boot.hateoas.autoconfigure.HypermediaWebMvcTest-d19d928d0afb.out.log`

## Root cause (confirmed against source)

`MediaTypeConfigurationFactory.getConfiguration` calls the JDK's
`java.util.stream.Stream`'s **3-argument** `reduce(U identity,
BiFunction<U,? super T,U> accumulator, BinaryOperator<U> combiner)` default
method. `native-collections/src/lib.rs`'s `register_stream_natives`
(~line 14106) registers exactly two `reduce` overloads for `Stream`:

```rust
r.register(c, "reduce",
    "(Ljava/lang/Object;Ljava/util/function/BinaryOperator;)Ljava/lang/Object;",
    native_stream_reduce_identity);   // 2-arg: reduce(identity, accumulator)
r.register(c, "reduce",
    "(Ljava/util/function/BinaryOperator;)Ljava/util/Optional;",
    native_stream_reduce_optional);   // 1-arg: reduce(accumulator)
```

The 3-arg overload
(`(Ljava/lang/Object;Ljava/util/function/BiFunction;Ljava/util/function/BinaryOperator;)Ljava/lang/Object;`)
is **never registered**. Whatever `Stream` implementation
`MediaTypeConfigurationFactory` is iterating over (a synthetic/native
CratonVM stream pipeline, since a real-JDK `ReferencePipeline` would carry
its own real default-method bytecode and not fail this way) therefore falls
through to the bare `java.util.stream.Stream` interface's abstract
declaration for that signature — which has no `Code` attribute — producing
`AbstractMethodError`.

This is the **same defect family** as two previously-fixed sibling bugs in
this codebase for the identical "interface method silently missing its
native/real registration → dispatch resolves to the bare interface's
no-Code abstract declaration → `AbstractMethodError`" shape:
`docs/internal/fixed-suite-bugs/SC-stream-collector-supplier-no-code.md`
(`Collector.supplier()`/`accumulator()`/`finisher()`/`combiner()` were
missing until fixed) and
`docs/internal/fixed-suite-bugs/x509trustmanager-getacceptedissuers-abstractmethod-FIXED.md`
(`X509TrustManager.getAcceptedIssuers()`). A nearby comment in the same file
(`native-collections/src/lib.rs` ~line 18566, "spring-bug-03") documents an
earlier occurrence of this exact same missing-overload shape for
`IntStream.findFirst`/`findAny`. This is therefore a recurring, well
understood bug class in this codebase — this specific instance (the 3-arg
`Stream.reduce`) has just never been covered.

## Fix

`register_stream_natives` now registers the missing descriptor on synthetic
`java/util/stream/Stream` receivers:

```text
(Ljava/lang/Object;Ljava/util/function/BiFunction;Ljava/util/function/BinaryOperator;)Ljava/lang/Object;
```

The native implementation performs sequential reduction through the
`BiFunction` accumulator and retains moving-GC roots for the accumulator,
intermediate result, and stream elements across callback execution. Synthetic
streams are sequential, so the combiner is not invoked, matching the JDK's
sequential reduction behavior.

The focused regression invokes `Stream.reduce` with a `BiFunction` accumulator
and a `BinaryOperator` combiner, so its bytecode contains this exact
three-argument descriptor.

## Update 2026-07-17 (bin13 rerun triage) — 2 more classes, same module family

`module/spring-boot-restdocs`'s `MockMvcRestDocsAutoConfigurationAdvancedConfigurationIntegrationTests`
and `MockMvcRestDocsAutoConfigurationIntegrationTests` (1/1 test each) fail
with the byte-identical chain, bottoming out in the same
`AbstractMethodError: method java/util/stream/Stream.reduce(Ljava/lang/Object;Ljava/util/function/BiFunction;Ljava/util/function/BinaryOperator;)Ljava/lang/Object; has no Code attribute`
via the same `MediaTypeConfigurationFactory.getConfiguration` →
`HalMediaTypeConfiguration.configureJsonMapper` →
`WebConverters.augment`/`augmentServer` →
`WebMvcHateoasConfiguration$HypermediaWebMvcConfigurer.extendMessageConverters`
→ `mvcContentNegotiationManager` chain as the original 2 classes — REST
Docs' auto-configuration also pulls in HATEOAS's `WebMvcConfigurer`, so it
hits the exact same missing 3-arg `Stream.reduce` registration gap. Full
logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-restdocs.org.springframework.boot.restdocs.test.autoconfigure.MockMvcRestDo-63223fb95497.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-restdocs.org.springframework.boot.restdocs.test.autoconfigure.MockMvcRestDo-a6e2b0da3539.out.log`

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-hateoas` | `org.springframework.boot.hateoas.autoconfigure.HypermediaAutoConfigurationTests` |
| `module/spring-boot-hateoas` | `org.springframework.boot.hateoas.autoconfigure.HypermediaWebMvcTestIntegrationTests` |
| `module/spring-boot-restdocs` | `org.springframework.boot.restdocs.test.autoconfigure.MockMvcRestDocsAutoConfigurationAdvancedConfigurationIntegrationTests` (added bin13) |
| `module/spring-boot-restdocs` | `org.springframework.boot.restdocs.test.autoconfigure.MockMvcRestDocsAutoConfigurationIntegrationTests` (added bin13) |

## Validation

Using the uniquely built `cratonvm-hateoas-reduce-20260718.exe`, all four
classes above plus the duplicate HTTP-converter report passed on 2026-07-18:

| Mode | Classes | Tests | Result |
|---|---:|---:|---|
| JIT | 5 | 48 | all PASS |
| `--nojit` | 5 | 48 | all PASS |

Neither run contained the former `Stream.reduce(...BiFunction...BinaryOperator)`
`AbstractMethodError`.
