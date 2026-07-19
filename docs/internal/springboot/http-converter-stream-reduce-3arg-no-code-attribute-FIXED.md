# `HttpMessageConvertersAutoConfigurationTests` — generic `Stream.reduce` no-Code failure — FIXED

**Status: FIXED — 2026-07-18**

## Symptom

`typeConstrainedConverterFromSpringDataDoesNotPreventAutoConfigurationOfJacksonConverter`
failed while creating Spring HATEOAS's `HalFormsTemplateBuilder`:

```
method java/util/stream/Stream.reduce(Ljava/lang/Object;Ljava/util/function/BiFunction;Ljava/util/function/BinaryOperator;)Ljava/lang/Object; has no Code attribute
```

## Root cause

`cratonvm-native-collections` registered only the object `Stream.reduce`
overloads taking a `BinaryOperator` (the two-argument identity overload and
the `Optional` overload). The distinct generic three-argument descriptor was
absent. Its invocation therefore reached the abstract `Stream` interface
declaration, which has no `Code` attribute.

## Fix

`native-collections/src/lib.rs` now registers and implements
`Stream.reduce(U, BiFunction<U, ? super T, U>, BinaryOperator<U>)`. CratonVM
executes object streams sequentially, so the implementation folds each element
through `BiFunction.apply`; Java's combiner is intentionally not invoked on the
sequential path. The stream, accumulator callback, elements, and each
intermediate object accumulator are pinned across callback dispatch so moving
GC cannot leave a stale reference.

The regression suite adds a direct generic-reduce test and makes interpreter
tests prefer the build-generated Java test-class directory over stale committed
`.class` fixtures.

## Validation

- `cargo test -p cratonvm-native-collections stream_reduce_generic_accumulator_is_registered -- --nocapture` — passed.
- `CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS=1 cargo test -p cratonvm-vm --test interpreter_tests test_s20_reduce_with_generic_accumulator -- --nocapture --test-threads=1` — passed.
- Spring Boot `HttpMessageConvertersAutoConfigurationTests` using unique binary
  `cratonvm-httpconverter-reduce-019f733b.exe` — JIT on: **38/38 passed**
  (114.890 s); `--nojit`: **38/38 passed** (116.513 s).
- The captured JIT and `--nojit` logs contain neither `has no Code attribute`
  nor `AbstractMethodError`.
