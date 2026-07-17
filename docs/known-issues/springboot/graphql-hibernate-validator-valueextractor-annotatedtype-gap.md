# spring-boot-graphql-test: Hibernate Validator can't find `@ExtractedValue` on Spring GraphQL's `ArgumentValueValueExtractor`

**Status: OPEN — found 2026-07-17. Hypothesis, root mechanism not fully pinned.**

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

## Root cause (hypothesis)

Hibernate Validator auto-registers custom `jakarta.validation.valueextraction.ValueExtractor`
implementations found on the classpath (Spring GraphQL ships
`ArgumentValueValueExtractor`, a `ValueExtractor<ArgumentValue<@ExtractedValue
?>>` implementation used to unwrap its `ArgumentValue` wrapper type during
bean validation of GraphQL controller arguments). To register one, Hibernate
Validator's `ValueExtractorManager`/`ValueExtractorResolver` must reflectively
walk the extractor class's **generic interfaces** to find which type
parameter is annotated `@ExtractedValue` — i.e. it needs accurate
`Class.getGenericInterfaces()` and `AnnotatedType`/
`AnnotatedParameterizedType.getAnnotatedActualTypeArguments()` results,
including the **type-use annotation** on the wildcard/type argument itself
(not just the type argument's raw `Class`). `HV000203` is Hibernate
Validator's own error for "walked the generics and found no
`@ExtractedValue`-annotated type parameter" — i.e. from Hibernate
Validator's point of view, the annotation genuinely isn't there on whatever
`AnnotatedType` structure CratonVM's reflection handed back, even though the
real `.class` file has it.

This is consistent with — but this session did not confirm it is the same
bug as — the general "generic-type/`AnnotatedType` reflection gap" family
this codebase already tracks in several other contexts (e.g.
`docs/internal/comparable-classcast-lambda-proxy-unknown-class-RESOLVED.md`'s
`Class.getGenericInterfaces()` gap for lambda-proxy classes — since
RESOLVED/retired, so likely not the same live defect but the same general
area of reflection machinery — and
`docs/internal/elasticsearch-suite/elasticsearch-bytebuddy-annotatedtype-proxy-mismatch.md`).
CratonVM has dedicated machinery for this
(`native-builtins/src/generics.rs`, plus
`getAnnotatedActualTypeArguments`/`AnnotatedParameterizedType` handling in
`native-builtins/src/lang_reflect.rs` and `lang_class.rs`), but this session
did not trace the exact call path Hibernate Validator's
`ValueExtractorResolver` uses (likely
`Class.getGenericInterfaces()` → walk each `ParameterizedType`'s
`getActualTypeArguments()` looking for a `WildcardType`, then re-resolve the
**annotated** form via `Class.getAnnotatedInterfaces()` /
`AnnotatedParameterizedType.getAnnotatedActualTypeArguments()` to check for
`@ExtractedValue` on that wildcard) against CratonVM's implementation to
confirm which specific step drops the annotation. Filed as OPEN with a
grounded but unconfirmed hypothesis — a live probe reflecting on
`org.springframework.graphql.data.method.annotation.support.ArgumentValueValueExtractor`
directly (outside a full Spring context) and dumping
`getGenericInterfaces()`/`getAnnotatedInterfaces()` would confirm or refute
this quickly.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-graphql-test` | `org.springframework.boot.graphql.test.autoconfigure.GraphQlTestIntegrationTests` |
| `module/spring-boot-graphql-test` | `org.springframework.boot.graphql.test.autoconfigure.GraphQlTestPropertiesIntegrationTests` |
