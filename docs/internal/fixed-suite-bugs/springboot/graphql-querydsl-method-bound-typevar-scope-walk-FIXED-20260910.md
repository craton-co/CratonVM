# A method type-parameter's bound never reached the class that declares it

**Status: FIXED 2026-09-10** (`native-builtins/src/generics.rs`). Closes three
Spring Boot GraphQL classes that were CratonVM-only failures against a HotSpot
25 baseline that passes all three.

| class | before | after |
|---|---|---|
| `org.springframework.boot.graphql.autoconfigure.data.GraphQlQuerydslAutoConfigurationTests` | 2 tests, **2 failed** | 2 tests, **0 failed** |
| `…data.GraphQlReactiveQueryByExampleAutoConfigurationTests` | 1 test, **1 failed** | 1 test, **0 failed** |
| `…data.GraphQlReactiveQuerydslAutoConfigurationTests` | 2 tests, **2 failed** | 2 tests, **0 failed** |

Measured on Azure Linux (`20.80.105.49`), JDK 25 image, one process per class
through `sb-runner`, `dev`@`39a90d2f4` before / the fix after.

## Symptom

`Mockito.mock(MockRepository.class)` throws, and every context that wants that
bean fails to start:

```text
Mockito cannot mock this class: interface …GraphQlQuerydslAutoConfigurationTests$MockRepository
Underlying exception : java.lang.IllegalArgumentException:
    Cannot resolve T from class …$MockRepository$MockitoMock$RfMtcmzV
	at net.bytebuddy.description.TypeVariableSource$AbstractBase.findExpectedVariable(TypeVariableSource.java:174)
	at net.bytebuddy.dynamic.Transformer$ForMethod$TransformedMethod$AttachmentVisitor.onTypeVariable(Transformer.java:599)
	at net.bytebuddy.description.type.TypeList$Generic$ForDetachedTypes$OfTypeVariables$AttachedTypeVariable.getUpperBounds(TypeList.java:741)
	at net.bytebuddy.description.type.TypeDescription$Generic$OfTypeVariable.asErasure(TypeDescription.java:5713)
	at net.bytebuddy.description.method.MethodDescription$AbstractBase.asTypeToken(MethodDescription.java:929)
	at net.bytebuddy.dynamic.scaffold.MethodRegistry$Default$Prepared$Entry.resolveBridgeTypes(MethodRegistry.java:889)
```

The fixture is a one-line interface:

```java
interface MockRepository extends CrudRepository<Book, Long>, QuerydslPredicateExecutor<Book> { }
```

and the method ByteBuddy chokes on is `QuerydslPredicateExecutor<T>`'s

```java
<S extends T, R> R findBy(Predicate predicate, Function<FetchableFluentQuery<S>, R> queryFunction);
```

— a METHOD type parameter (`S`) whose bound names the INTERFACE's type
parameter (`T`).

## Where the two VMs diverge

Core reflection is byte-identical on both VMs down to one value, and that value
is the whole bug. `probes` used for this were three: `GenProbe` (generic
interfaces / type parameters), `BbProbe` (ByteBuddy's `TypeDescription` view)
and `TvProbe` (the bound's own declaration site).

`Class.getGenericInterfaces()` on `MockRepository`: identical, both
`ParameterizedType`. `QuerydslPredicateExecutor.getTypeParameters()`:
identical, `[T]`. `findBy`'s own type parameters: identical, `[S, R]`, and `S`'s
bound prints as `T` on both.

Then ask that bound `T` who declared it:

| | HotSpot 25 | CratonVM (before) |
|---|---|---|
| `S.getBounds()[0]` runtime class | `sun.reflect…TypeVariableImpl` | `java.lang.reflect.TypeVariable` (a synthetic stand-in) |
| `…getGenericDeclaration()` | **`interface QuerydslPredicateExecutor`** | **`…findBy(Predicate, Function)`** — the METHOD |
| is it among that declaration's own type parameters? | `found(equals=true)` | `NOT-FOUND-in-decl` |

`T` is not one of `findBy`'s type parameters and never will be, so ByteBuddy's
`findExpectedVariable("T")` can only throw. The same value read through
ByteBuddy: `tv S upperBounds=[class Book]` on HotSpot, `tv S upperBounds=[T]`
(and `erasure=class java.lang.Object`) on CratonVM.

## Root cause

`generics.rs`'s `TypeSig::TypeVar` arm already had the right idea, and had had
it since the netty `TypeParameterMatcherTest.testInnerClass` fix: a
type-variable USE resolves by walking the ENCLOSING generic declarations —
method, then declaring class, then outer classes — exactly like Java's lexical
scope. Its own comment names this exact failure ("a method type-parameter bound
such as `<S extends T> withType(Class<S>)` references the CLASS's `T` … 
ByteBuddy's `TypeVariableSource.findExpectedVariable` then fails with 'Cannot
resolve T'").

The walk never climbed. One line:

```rust
let mut scope = decl;
let scope_pin = ctx.pin_native_root(scope);
for _ in 0..16 {
    let mut scope = ctx.read_native_pin(scope_pin, scope);   // <-- SHADOWS the outer binding
    if let Some(real) = resolve_declared_type_variable(ctx, scope, name) { return Ok(real); }
    …
    match next {
        Some(enclosing) => scope = enclosing,                 // writes the SHADOW
        None => break,
    }
}
```

The loop body opens by rebinding `scope`. `scope = enclosing` at the bottom
therefore assigns the inner binding, which dies with the iteration; the next
pass re-reads the ORIGINAL `decl` out of the one pin that was ever taken. The
walk re-tested the immediate declaration sixteen times and climbed zero
levels, so every use fell through to the synthetic stand-in below — whose name
is right, whose `genericDeclaration` is the wrong scope, and whose bound
defaults to `Object`.

A shadowing bug is invisible to the test that guards the feature, because the
feature's *first* level still works: a type variable declared by the immediate
scope resolves on iteration 1 and never needs the climb.

## Fix

Re-pin on each climb so `read_native_pin` returns the CURRENT scope, and
release the whole run of pins on the way out. The old code also leaked one pin
per conversion — `scope_pin` was never unpinned, and the `return Ok(real)`
inside the loop leaked it unconditionally.

```rust
let pin_base = ctx.pin_native_root(decl);
let mut scope_pin = pin_base;
let mut scope = decl;
let mut resolved: Option<Value> = None;
for _ in 0..16 {
    scope = ctx.read_native_pin(scope_pin, scope);
    if let Some(real) = resolve_declared_type_variable(ctx, scope, name) { resolved = Some(real); break; }
    scope = ctx.read_native_pin(scope_pin, scope);   // that call allocates
    … climb …
    Some(enclosing) => { scope = enclosing; scope_pin = ctx.pin_native_root(scope); }
}
ctx.unpin_native_roots(pin_base);
if let Some(real) = resolved { return Ok(real); }
```

## Verdict

`TvProbe` after the fix, byte-for-byte HotSpot's answer:

```text
bound T … declClass=java.lang.Class  decl=interface org.springframework.data.querydsl.QuerydslPredicateExecutor
          declIsClass=true  identityEqualsOwnerTV=found(equals=true)
```

Identity equality matters as much as the name: a synthetic stand-in compares
unequal to the real `TypeVariable` a resolver substituting across a hierarchy
holds, which is the same reason the arm resolves to the declaration's real
type-parameter object in the first place.

## What this does NOT explain

The other three CratonVM-only Spring Boot classes in the same sweep are
unrelated and are filed separately:

* `loader/spring-boot-loader … UrlJarFilesTests` —
  `urljarfilestests-zip-immunity-vs-mockito-inline-mock-FIXED-20260910.md`
* `module/spring-boot-kafka … KafkaAutoConfigurationIntegrationTests` —
  `internal/fixed-suite-bugs/springboot/kafka-scala-statics-anyhash-jit-miscompile-FIXED-20260910.md`
* `module/spring-boot-flyway … ResourceProviderCustomizerBeanRegistrationAotProcessorTests` —
  `internal/springboot/flyway-aot-receiver-class-confusion-under-concurrency-FIXED-20260911.md`
