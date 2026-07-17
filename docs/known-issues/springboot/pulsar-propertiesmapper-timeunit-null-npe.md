# Pulsar `PropertyMapper` → `BiConsumer<Integer,TimeUnit>` lambda dispatch: `TimeUnit` argument arrives `null`

**Status: OPEN — found 2026-07-17**

## Symptom

`module/spring-boot-pulsar`'s `PulsarPropertiesMapperTests` fails 1 of its
14 tests:

```
JUnit Jupiter:PulsarPropertiesMapperTests:customizeClientBuilderWhenHasFailover()
    => java.lang.NullPointerException: Cannot invoke "java.util.concurrent.TimeUnit.toNanos(long)" because "timeUnit" is null
       org.apache.pulsar.client.impl.AutoClusterFailover$AutoClusterFailoverBuilderImpl.failoverDelay(AutoClusterFailover.java:329)
       org.springframework.boot.pulsar.autoconfigure.PulsarPropertiesMapper.lambda$timeoutProperty$0(PulsarPropertiesMapper.java:221)
       org.springframework.boot.context.properties.PropertyMapper$Source.to(PropertyMapper.java:292)
       org.springframework.boot.pulsar.autoconfigure.PulsarPropertiesMapper.customizeServiceUrlProviderBuilder(PulsarPropertiesMapper.java:95)
       org.springframework.boot.pulsar.autoconfigure.PulsarPropertiesMapper.customizeClientBuilder(PulsarPropertiesMapper.java:77)
       org.springframework.boot.pulsar.autoconfigure.PulsarPropertiesMapperTests.customizeClientBuilderWhenHasFailover(PulsarPropertiesMapperTests.java:151)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-pulsar.org.springframework.boot.pulsar.autoconfigure.PulsarPropertiesMapperTests.out.log`

## Root cause (hypothesis, not confirmed against CratonVM source)

`PulsarPropertiesMapper.timeoutProperty` (source:
`apps/spring-boot/module/spring-boot-pulsar/src/main/java/org/springframework/boot/pulsar/autoconfigure/PulsarPropertiesMapper.java:220-222`):

```java
private Consumer<Duration> timeoutProperty(BiConsumer<Integer, TimeUnit> setter) {
    return (duration) -> setter.accept((int) duration.toMillis(), TimeUnit.MILLISECONDS);
}
```

`setter` here is a `BiConsumer<Integer, TimeUnit>` bound (via `PropertyMapper`)
to a real two-arg instance method reference on Pulsar's real
`AutoClusterFailover$AutoClusterFailoverBuilderImpl.failoverDelay(long,
TimeUnit)`. `TimeUnit.MILLISECONDS` is **not a captured lambda variable** —
it is evaluated fresh, via `getstatic`, inside the lambda body on every
invocation, so this is not the same "stale capture" family as other
lambda-proxy bugs in this project. The likely mechanism is instead a
**lambda argument-coercion/positional bug**: `BiConsumer.accept(Object,
Object)` is the erased SAM signature, and the real target
`failoverDelay(long, TimeUnit)` requires unboxing the first boxed
`Integer` argument to a primitive `long` while passing the second
(`TimeUnit`, a reference type) through unchanged. If CratonVM's lambda
argument-coercion path (`coerce_lambda_args`, `vm/src/runtime/interpreter.rs`,
used by `try_lambda_dispatch`'s `InvokeVirtual`/`InvokeInterface` arm) has a
gap specific to a **primitive-unboxing-plus-trailing-reference-arg** shape —
e.g. an off-by-one in the coercion loop after inserting/adjusting for the
first argument's box→primitive conversion — the second (`TimeUnit`)
argument could be dropped or overwritten with a default/null value before
reaching the real method, matching the observed symptom (`timeUnit` is
`null` inside `failoverDelay`) exactly.

This project has a related, already-fixed bug in the same general area
(`docs/known-issues/springboot/README.md`: "`Map.Entry::getKey`/`getValue`
lambda-dispatch precedence... FIXED 2026-07-14 — the receiver-specific 'C25
rescue' native lookup... no longer skips itself when a less-specific
interface-level native already matched") and a noted residual
(`reference_arg_pinning_unwrap_or_eager_eval_panic` in project memory:
"crashed native calls w/ >4 args") in the lambda/native-call argument-passing
area generally — this may be a sibling gap in the same subsystem, but that
is speculative; **no source-level confirmation was done this session**
(`coerce_lambda_args` was not read/traced for this specific case). Would
need either a standalone repro (`BiConsumer<Integer, SomeRefType>` bound to
a real two-arg `(long, SomeRefType)` method reference, called through
`PropertyMapper`-style indirection) or a `CRATONVM_DBG_LAMBDA`-style trace
of the actual argument values at the `try_lambda_dispatch` call site to
confirm.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-pulsar` | `org.springframework.boot.pulsar.autoconfigure.PulsarPropertiesMapperTests` (1 of 14 tests) |
