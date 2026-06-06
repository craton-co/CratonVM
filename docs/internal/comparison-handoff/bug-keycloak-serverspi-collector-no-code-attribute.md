# keycloak-server-spi MapperTypeSerializerTest — `Collector.accumulator() has no Code attribute`

## Symptom
Module `apps/keycloak/server-spi`, JUnit4 suite (via `RunDirTests`):

| VM | tests | failures | wall |
|---|---|---|---|
| CratonVM-CPU | 18 | **8** | 6.3s |
| HotSpot (jdk-25) | 18 | **0** | 0.9s |

Two of the eight CratonVM-only failures:
```
FAIL testBasicSerializeAndDeserialize(org.keycloak.utils.MapperTypeSerializerTest)
     :: method java/util/stream/Collector.accumulator()Ljava/util/function/BiConsumer; has no Code attribute
FAIL testMultivaluedSerializeAndDeserialize(org.keycloak.utils.MapperTypeSerializerTest)
     :: method java/util/stream/Collector.accumulator()Ljava/util/function/BiConsumer; has no Code attribute
```

## Why it's a CratonVM bug
`java.util.stream.Collector.accumulator()` is an **abstract interface method** —
it legitimately has no Code attribute. The error means CratonVM tried to
**execute the abstract method directly** instead of dispatching to the concrete
`Collector` implementation's `accumulator()` (e.g. the `Collectors.toList()` /
`toMap()` collector object). This is an interface/virtual-dispatch resolution
bug: an `invokeinterface Collector.accumulator()` is binding to the abstract
declaration rather than the receiver's concrete method.

HotSpot runs the identical bytecode + classpath and passes, so it is
CratonVM-specific. It is exercised by a Jackson/stream serialization path that
collects a stream into a map/list.

## Reproduce
```
CP="apps/_test-harness;apps/keycloak/server-spi/target/classes;apps/keycloak/server-spi/target/test-classes;$(cat apps/keycloak/server-spi/cp.txt)"
target/release/java.exe --java-home "C:/Program Files/Java/jdk-25" -Xmx2g -cp "$CP" \
    org.junit.runner.JUnitCore org.keycloak.utils.MapperTypeSerializerTest
```
Full CratonVM log: `test-infra/suite-results/applog-keycloak-server-spi-cratonvm.log`.

## What an agent should try next
1. Minimal repro: `Stream.of(1,2,3).collect(Collectors.toList())` (or `toMap`) —
   does CratonVM throw "Collector.accumulator() has no Code attribute"?
2. Trace `invokeinterface` resolution for `java/util/stream/Collector` methods:
   the dispatch must select the receiver object's concrete `accumulator()`
   (the `Collectors$CollectorImpl` lambda-backed method), not the interface's
   abstract slot. Likely an itable/selector bug for collectors whose methods are
   provided as constructor-supplied lambdas (function fields), not overrides.
3. Cross-check against `reference_wrong_intrinsic_stubs` (Collectors.toMap had a
   wrong intrinsic before) — confirm this isn't a stubbed Collector path.
