# spring-boot-graphql-test: Hibernate Validator can't find `@ExtractedValue` on Spring GraphQL's `ArgumentValueValueExtractor`

**Status: FIXED 2026-07-18** (branch `fix/hv000203-valueextractor-20260717`)

## Symptom

Both classes in the module fail identically, at application-context startup,
via the same chain:

| Module | Class |
|---|---|
| `module/spring-boot-graphql-test` | `GraphQlTestIntegrationTests` |
| `module/spring-boot-graphql-test` | `GraphQlTestPropertiesIntegrationTests` |

```
Caused by: org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'graphQlSource' ...: Factory method 'graphQlSource' threw exception with message: Error creating bean with name 'annotatedControllerConfigurerDataFetcherExceptionResolver' ...: Unsatisfied dependency expressed through method 'annotatedControllerConfigurerDataFetcherExceptionResolver' parameter 0: Error creating bean with name 'annotatedControllerConfigurer' ...: Error creating bean with name 'defaultValidator' defined in class path resource [org/springframework/boot/validation/autoconfigure/ValidationAutoConfiguration.class]: HV000203: Value extractor type org.springframework.graphql.data.method.annotation.support.ArgumentValueValueExtractor fails to declare the extracted type parameter using @ExtractedValue.
Caused by: org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'defaultValidator' ...
Caused by: jakarta.validation.valueextraction.ValueExtractorDefinitionException: HV000203: Value extractor type org.springframework.graphql.data.method.annotation.support.ArgumentValueValueExtractor fails to declare the extracted type parameter using @ExtractedValue.
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-graphql-test.org.springframework.boot.graphql.test.autoconfigure.GraphQlTes-00a9ede9f6bb.out.log`

## Root cause (confirmed)

Same underlying bug as
[`controllerendpointdiscoverertests-hv000203-valueextractor-FIXED.md`](controllerendpointdiscoverertests-hv000203-valueextractor-FIXED.md)
(fixed in the same change) — this doc's hypothesis (a gap in CratonVM's `AnnotatedType`/`getGenericInterfaces()`
reflection machinery for `ValueExtractor<ArgumentValue<@ExtractedValue ?>>`'s
nested type-use annotation) was confirmed exactly:

1. The class-level `RuntimeVisibleTypeAnnotations` attribute (JVMS 4.7.20
   `CLASS_EXTENDS` target) was silently discarded during class loading —
   `Class.getAnnotatedInterfaces()` had nowhere to read it from.
2. The one existing per-type-argument annotation extraction path only
   modeled a single `TYPE_ARGUMENT` nesting level; `ArgumentValue<@ExtractedValue
   ?>`'s annotation sits at nesting depth 2 (`type_path` =
   `[TYPE_ARGUMENT(0), TYPE_ARGUMENT(0)]`, confirmed via `javap -v` on the
   real `spring-graphql` jar), which had no representation.

Fixed by adding a `TypeArgAnnotations` tree (arbitrary nesting depth) plus
`NativeContext::class_extends_type_annotations` (recovering the class-level
attribute by re-parsing the cached original class bytes) and making
`AnnotatedParameterizedType.getAnnotatedActualTypeArguments()`
self-sustaining across repeated chained calls — see the sibling doc for the
full mechanism writeup.

## Verification

- `GraphQlTestIntegrationTests` under CratonVM: 1/1 tests pass (was failing
  at context startup with HV000203).
- `GraphQlTestPropertiesIntegrationTests` under CratonVM: 2/2 tests pass
  (same).
- Standalone `Hv203Probe.java` (depth-1 and depth-2 nested TYPE_USE
  annotation on an implemented interface) matches real HotSpot output.
- `cargo test --release -p cratonvm-native-builtins`: 3001 passed, 0 failed.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-graphql-test` | `org.springframework.boot.graphql.test.autoconfigure.GraphQlTestIntegrationTests` — now 1/1 PASS |
| `module/spring-boot-graphql-test` | `org.springframework.boot.graphql.test.autoconfigure.GraphQlTestPropertiesIntegrationTests` — now 2/2 PASS |
