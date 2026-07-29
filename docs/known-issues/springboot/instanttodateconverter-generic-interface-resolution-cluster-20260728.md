# `GenericConversionService.addConverter` can't resolve `<S,T>` for a converter implementing a non-generic + a generic interface

**Status: OPEN — found 2026-07-28**

## Symptom

Affected classes (all from `RunName=craton-rerun-20260728`):

| Module | Class | Test method |
|---|---|---|
| `module/spring-boot-data-redis` | `org.springframework.boot.data.redis.autoconfigure.health.DataRedisHealthContributorAutoConfigurationTests` | `runWhenDisabledShouldNotCreateIndicator` |
| `module/spring-boot-data-redis` | `org.springframework.boot.data.redis.autoconfigure.DataRedisAutoConfigurationJedisTests` | 11 of 23 methods (`shouldUseVirtualThreadsIfEnabled`, `testRedisConfigurationWithDefaultTimeouts`, `testRedisConfigurationWithTimeoutAndConnectTimeout`, `testRedisConfigurationWithSslEnabled`, `testPasswordInUrlStartsWithColon`, `connectionFactoryIsNotCreatedWhenLettuceIsSelected`, `testRedisConfigurationWithClientName`, `connectionFactoryDefaultsToJedis`, `testOverrideRedisConfiguration`, `testRedisConfigurationWithSslDisabledAndBundle`, `testPasswordInUrlWithColon`) |

Every failure has the byte-for-byte identical root exception:

```
java.lang.IllegalArgumentException: Unable to determine source type <S> and target type <T> for your Converter [org.springframework.core.convert.support.InstantToDateConverter]; does the class parameterize those types?
       org.springframework.core.convert.support.GenericConversionService.addConverter(GenericConversionService.java:92)
       org.springframework.core.convert.support.DefaultConversionService.addDefaultConverters(DefaultConversionService.java:95)
       org.springframework.format.support.DefaultFormattingConversionService.<init>(DefaultFormattingConversionService.java:92)
       org.springframework.format.support.DefaultFormattingConversionService.<init>(DefaultFormattingConversionService.java:63)
       org.springframework.data.redis.config.RedisListenerEndpointRegistrar.apply(RedisListenerEndpointRegistrar.java:209)
       org.springframework.data.redis.annotation.RedisListenerAnnotationBeanPostProcessor.afterSingletonsInstantiated(RedisListenerAnnotationBeanPostProcessor.java:149)
```

Every `AnnotationConfigApplicationContext` in these test classes registers
`RedisListenerAnnotationBeanPostProcessor`, whose
`afterSingletonsInstantiated` builds a `DefaultFormattingConversionService`,
which (in its constructor) calls `DefaultConversionService
.addDefaultConverters`, which unconditionally registers
`InstantToDateConverter` — so **every** context in this suite that reaches
that bean-post-processor callback fails identically, regardless of what the
individual test is actually about.

Full logs:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260728/shard2/logs/module_spring-boot-data-redis.org.springframework.boot.data.redis.autoconfigure.health.Data-dc6ecb50c06b.out.log`,
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260728/shard2/logs/module_spring-boot-data-redis.org.springframework.boot.data.redis.autoconfigure.DataRedisAu-7b09ad148f2a.out.log`

## Root cause (confirmed via javap disassembly of the real jars)

`org.springframework.core.convert.support.InstantToDateConverter`
(`spring-core-7.1.0-SNAPSHOT.jar`) declares:

```java
final class InstantToDateConverter implements ConditionalConverter, Converter<Instant, Date>
```

i.e. it implements **two** interfaces: `ConditionalConverter` (plain, no
generics) and `Converter<Instant, Date>` (generic, concrete type
arguments). `GenericConversionService.addConverter`'s private helper
`getRequiredTypeInfo(Class, Class)` (disassembled from
`GenericConversionService.class`) does:

```java
ResolvableType.forClass(converterClass).as(Converter.class).getGenerics();
```

and returns `null` (triggering the observed `IllegalArgumentException`) if
`getGenerics()` comes back shorter than 2 elements or either generic
resolves to `null`. `ResolvableType.as(Converter.class)` walks the class's
generic-interface hierarchy looking for the `Converter` entry.

`Class.getGenericInterfaces()`'s CratonVM implementation
(`native-builtins/src/lang_class.rs:13611-13827`,
`native_class_get_generic_interfaces`) parses the class's `Signature`
attribute (`class_sig.interfaces`, one `TypeSig` per direct interface, in
declaration order — here `[ConditionalConverter, Converter<Instant,Date>]`)
and converts each to a real `Type` via `crate::generics::typesig_to_real_type`.
Critically, its own comment (lines 13677-13690) documents a **fallback**:

> "A malformed or not-yet-resolvable generic argument must not leave a null
> element in `Type[]`. Java reflection degrades to the matching raw direct
> interface in this situation"

i.e. if `typesig_to_real_type` fails to resolve the `Converter<Instant,Date>`
signature entry, `getGenericInterfaces()` silently substitutes the **raw**
`Converter` `Class` mirror (no type arguments) instead. That is exactly
consistent with the observed symptom: `ResolvableType.as(Converter.class)`
would still find `Converter` in the interface list, but since it's the raw
`Class` form rather than a `ParameterizedType`, `.getGenerics()` returns an
empty/unresolved array, `getRequiredTypeInfo` returns `null`, and
`addConverter` throws.

**Not fully bisected**: the exact reason `typesig_to_real_type` fails to
resolve `Converter<Ljava/time/Instant;Ljava/util/Date;>;` specifically (as
opposed to succeeding, which it must do for the very common case of a class
implementing a *single* generic interface) was not traced to a specific line
inside `typesig_to_real_type` this session. The distinguishing feature of
this class — implementing one non-generic interface *before* one generic
interface in the same `Signature` — is the strongest lead, since
`getGenericInterfaces()`'s main loop (`lang_class.rs:13669-13693`) processes
`class_sig.interfaces` by simple index (`enumerate()`) and separately reads
`ctx.class_interfaces(class_id)` (the raw/bytecode-order interface list) as
its per-index fallback source (line 13684:
`raw_interfaces.get(i).map(...)`) — if the `Signature` attribute's parsed
interface list and the raw constant-pool interface list are not in the same
order for this class (or if the fallback substitution itself is what's
firing here, for a different structural reason), that would explain a
generic-arguments loss specific to "non-generic interface listed before a
generic one."

## Confirming/refuting this hypothesis

Add a temporary trace to `typesig_to_real_type` (or the `class_sig.interfaces`
fallback branch at `lang_class.rs:13683-13690`) gated on the class name
containing `InstantToDateConverter`, rebuild, and rerun any of the affected
classes to see whether the fallback path fires and, if so, why
`typesig_to_real_type` returned `Value::Object(None)` for the
`Converter<Instant,Date>` signature entry specifically. A minimal repro
(a hand-written class implementing `ConditionalConverter, Converter<A,B>` in
that order, calling `getGenericInterfaces()` directly) would confirm this
without needing the full Spring Data Redis context.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-data-redis` | `org.springframework.boot.data.redis.autoconfigure.health.DataRedisHealthContributorAutoConfigurationTests` |
| `module/spring-boot-data-redis` | `org.springframework.boot.data.redis.autoconfigure.DataRedisAutoConfigurationJedisTests` |
