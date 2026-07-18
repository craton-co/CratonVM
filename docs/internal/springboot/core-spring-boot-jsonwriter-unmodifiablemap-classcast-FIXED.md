# `JsonWriterTests$ValueProcessorTests`: synthetic map wrappers must use their Java-visible class name in lambda bridge casts

**Status: FIXED 2026-07-18.**

## Symptom

`core/spring-boot`'s `org.springframework.boot.json.JsonWriterTests` had
three failures in its nested `ValueProcessorTests` class:

- `processValueWhenInMap`
- `processValueWhen`
- `processValueWhenInNestedMap`

Each failed with a `ClassCastException` while a `ValueProcessor<String>` was
being considered for the outer map value.

## Root cause

This was not a map iteration or `Map.forEach` dispatch bug. `JsonValueWriter`
first passes each value through Spring Boot's `LambdaSafe` filter, including the
outer map. The lambda-metafactory bridge in
`vm/src/runtime/interpreter.rs::checkcast_lambda_instantiated_args` correctly
replays javac's erased-generic `checkcast`, but built its exception message from
the private storage stamp `cratonvm/internal/UnmodifiableMap`.

The same object is deliberately exposed by `Object.getClass()` as a concrete
JDK implementation class. Spring's `LambdaSafe` uses the exception prefix and
`argument.getClass().getName()` to recognize and suppress this expected generic
mismatch. The two names differed, so it rethrew instead of continuing to the
map's actual string values.

`cce_display_class_name` now gives bridge-generated and ordinary `checkcast`
errors the same Java-visible map identity as `Object.getClass()`:

- `Map.of()` and multi-entry immutable maps -> `ImmutableCollections$MapN`
- one-entry immutable maps -> `ImmutableCollections$Map1`
- `Collections.unmodifiableMap(...)` -> `Collections$UnmodifiableMap`

The immutable-map size uses the backing map's resolved `size` field rather
than assuming a physical field slot, so it remains correct with real-JDK field
layouts.

## Regression coverage

`vm/tests/lambda_safe_unmodifiable_map_classcast.rs` compiles a small
LambdaSafe-equivalent program and checks all four relevant forms (empty,
one-entry, multi-entry, and explicit unmodifiable maps) in JIT and interpreter
mode.

## Validation

On the isolated Azure `/data` worktree, using Java 25 and unique binary
`cratonvm-springboot-jsonwriter-unmodifiablemap-v4-20260718`:

- focused regression: pass in JIT and `--nojit`
- `SbRunner org.springframework.boot.json.JsonWriterTests`: **86/86 passed**
  in JIT and **86/86 passed** with `--nojit`

The original baseline was 83/86 passed with the three failures above.
