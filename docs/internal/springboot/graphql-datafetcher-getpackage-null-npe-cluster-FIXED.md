# `Class.getPackage()` returns null for a lambda-implemented GraphQL `DataFetcher` → NPE building the schema

**Status: FIXED — 2026-07-18**

## Resolution

Two VM gaps were closed.

- `Class.getPackage()` now derives a synthetic lambda proxy's package from its
  defining host, matching `getPackageName()` and removing the GraphQL
  `DataFetcher` NPE.
- Real-JDK resource loading now preserves parent delegation for user-defined
  `URLClassLoader` subclasses. Spring's `FilteredClassLoader` can therefore
  hide Jackson 3 while still reaching the resource overlay that supplies the
  GraphQL schema.

The three affected Spring GraphQL classes pass against the rebuilt Linux
fixture in both JIT and `--nojit` modes: RSocket 6/6, WebFlux 17/17, MVC 17/17.

## Symptom

3 classes in `module/spring-boot-graphql` fail — almost every test in each
class, all with the identical inner exception:

| Class | tests failed/total |
|---|---:|
| `GraphQlRSocketAutoConfigurationTests` | 6/6 |
| `org.springframework.boot.graphql.autoconfigure.reactive.GraphQlWebFluxAutoConfigurationTests` | 16/17 |
| `org.springframework.boot.graphql.autoconfigure.servlet.GraphQlWebMvcAutoConfigurationTests` | 16/17 |

Every failure is the application context failing to start because the
`graphQlSource` bean can't be created:

```
org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'graphQlSource' defined in org.springframework.boot.graphql.autoconfigure.GraphQlAutoConfiguration: Failed to instantiate [org.springframework.graphql.execution.GraphQlSource]: Factory method 'graphQlSource' threw exception with message: Cannot invoke "java.lang.Package.getName()" because the return value of "java.lang.Class.getPackage()" is null
  ...
Caused by: org.springframework.beans.BeanInstantiationException: Failed to instantiate [org.springframework.graphql.execution.GraphQlSource]: Factory method 'graphQlSource' threw exception with message: Cannot invoke "java.lang.Package.getName()" because the return value of "java.lang.Class.getPackage()" is null
  ...
Caused by: java.lang.NullPointerException: Cannot invoke "java.lang.Package.getName()" because the return value of "java.lang.Class.getPackage()" is null
	org.springframework.graphql.execution.ContextDataFetcherDecorator$ContextTypeVisitor.applyDecorator(ContextDataFetcherDecorator.java:180)
	org.springframework.graphql.execution.ContextDataFetcherDecorator$ContextTypeVisitor.visitGraphQLFieldDefinition(ContextDataFetcherDecorator.java:166)
	graphql.util.Traverser.traverse(Traverser.java:144)
	graphql.schema.SchemaTraverser.depthFirstFullSchema(SchemaTraverser.java:73)
	org.springframework.graphql.execution.AbstractGraphQlSourceBuilder.applyTypeVisitors(AbstractGraphQlSourceBuilder.java:159)
	org.springframework.graphql.execution.AbstractGraphQlSourceBuilder.build(AbstractGraphQlSourceBuilder.java:118)
	org.springframework.boot.graphql.autoconfigure.GraphQlAutoConfiguration.graphQlSource(GraphQlAutoConfiguration.java:114)
	org.springframework.beans.factory.support.SimpleInstantiationStrategy.lambda$instantiate$0(SimpleInstantiationStrategy.java:155)
```

The one test that does NOT hit this in `GraphQlWebFluxAutoConfigurationTests`
and `GraphQlWebMvcAutoConfigurationTests` is the `CapturedOutput`-based
`schemaInspectionShouldBeEnabledByDefault`-shaped test, which fails for a
completely unrelated reason folded into
[`capturedoutput-empty-console-cluster.md`](capturedoutput-empty-console-cluster.md)
instead — not double-counted here.

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-graphql.org.springframework.boot.graphql.autoconfigure.rsocket.GraphQlRSock-5cd479cc8274.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-graphql.org.springframework.boot.graphql.autoconfigure.reactive.GraphQlWebF-6887be131002.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-graphql.org.springframework.boot.graphql.autoconfigure.servlet.GraphQlWebMv-277682d2f09a.out.log`

## Root cause (high-confidence hypothesis, grounded in source — not verified via live debugger)

Spring GraphQL's `ContextDataFetcherDecorator$ContextTypeVisitor.applyDecorator`
walks every `GraphQLFieldDefinition` in the schema and, for each one's
resolved `DataFetcher`, calls `.getClass().getPackage().getName()` (the
exact call shape the trace names) — almost certainly to check whether the
fetcher implementation lives in a Spring-internal/framework package (to
decide whether to wrap it in a context-propagating decorator) vs
user/application code. Spring GraphQL auto-registers a `DataFetcher` per
schema field using a **method reference / lambda** (`PropertyDataFetcher`
and the generated `SourceMethodArgumentResolver`-backed fetchers frequently
wrap `this::someGetter`-style method references), so the receiver class here
is very plausibly a JVM-synthesized lambda proxy class, not an ordinarily
loaded/registered class.

Grepped CratonVM's `Class.getPackage()`/`getPackageName()` native
implementations
(`native-builtins/src/lang_class.rs`) and found an existing, deliberate
asymmetry between the two:

- `native_class_get_package_name` (`getPackageName()`, returns a `String`,
  `lang_class.rs:12955-12995`) has an explicit fallback for exactly this
  case:
  ```rust
  // Synthetic lambda proxies aren't in the class store, so
  // `class_name_of_id` below misses them and this whole branch used to
  // fall through to the empty-string fallback further down ... Derive the
  // package from the lambda's HOST class instead — matches HotSpot ...
  if let Some(host) = ctx.lambda_proxy_host(class_id) {
      let pkg = match host.rfind('/') { ... };
      return Ok(Some(Value::Object(Some(ctx.create_string(&pkg)))));
  }
  ```
- `native_class_get_package` (`getPackage()`, returns a `Package` *object*,
  `lang_class.rs:13257-13270`) has **no such fallback**:
  ```rust
  let name = mirror_class_name(ctx, this).unwrap_or_default();
  if name.is_empty() || name.starts_with('[') {
      return Ok(Some(Value::Object(None)));   // <-- null Package
  }
  ```
  For a synthetic lambda proxy `ClassId`, `mirror_class_name` resolves via
  the same class-store lookup the `getPackageName()` comment explicitly
  says "misses" lambda proxies — so `name` comes back empty here too, and
  `getPackage()` returns `null` instead of a `Package` object. Spring
  GraphQL's `.getPackage().getName()` call then NPEs exactly as observed.

This is a **narrow gap in an already-fixed sibling**: the fix that taught
`getPackageName()` to special-case `ctx.lambda_proxy_host(class_id)` was
never mirrored onto `getPackage()`, which shares the same
`mirror_class_name`-returns-empty failure mode but has no equivalent
recovery path — it silently falls through to the "no package" (`null`)
branch instead.

**Not confirmed:** which specific `DataFetcher` implementation class is the
actual receiver in this failure (no `CRATONVM_DBG_*` reflection trace was
captured this session, and the log doesn't print the offending class name).
The `lambda_proxy_host` theory is the strongest fit for the observed call
shape and the documented asymmetry between the two sibling functions, but an
alternative (e.g. a CGLIB-generated or otherwise unregistered synthetic
class hitting the same `mirror_class_name`-empty path through a different
route) has not been ruled out. Confirming would need either a
`CRATONVM_DBG_OOBFIELD`/reflection-trace-style instrumentation of
`native_class_get_package`'s `this` argument's class kind, or a standalone
repro constructing a Spring GraphQL schema with a lambda-backed
`DataFetcher` and calling `.getPackage()` on it directly.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-graphql` | `org.springframework.boot.graphql.autoconfigure.rsocket.GraphQlRSocketAutoConfigurationTests` |
| `module/spring-boot-graphql` | `org.springframework.boot.graphql.autoconfigure.reactive.GraphQlWebFluxAutoConfigurationTests` |
| `module/spring-boot-graphql` | `org.springframework.boot.graphql.autoconfigure.servlet.GraphQlWebMvcAutoConfigurationTests` |
