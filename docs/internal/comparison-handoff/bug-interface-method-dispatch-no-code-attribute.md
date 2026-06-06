# CratonVM interface-method dispatch — abstract method "has no Code attribute" / AbstractMethodError

## Severity: HIGH / broad. Two confirmed manifestations, likely many more.

CratonVM resolves some `invokeinterface` calls to the **abstract interface
method** (which legitimately has no Code) instead of dispatching to the
receiver's **concrete implementation**, throwing:
```
AbstractMethodError: method <Iface>.<m>(...) has no Code attribute
```

This is a virtual/interface dispatch (itable selection) bug. HotSpot runs the
identical bytecode + classpath correctly in every case below.

## Manifestation 1 — java.util.stream.Collector.accumulator()
`apps/keycloak/server-spi`, `MapperTypeSerializerTest` (2 failures):
```
method java/util/stream/Collector.accumulator()Ljava/util/function/BiConsumer; has no Code attribute
```
A stream `.collect(...)` calls `collector.accumulator()`; CratonVM binds to the
abstract `Collector.accumulator()` instead of the concrete collector object's
method (`Collectors$CollectorImpl`, whose method body is a constructor-supplied
lambda field). See `bug-keycloak-serverspi-collector-no-code-attribute.md`.

## Manifestation 2 — java.util.ServiceLoader$Provider.type()  (blocks ALL JUnit5)
Running any JUnit5 module via the Platform `LauncherFactory`:
```
WARN  NoSuchMethodError method="org/junit/platform/launcher/listeners/UniqueIdTrackingListener.type()Ljava/lang/Class;"
Exception: AbstractMethodError: method java/util/ServiceLoader$Provider.type()Ljava/lang/Class; has no Code attribute
```
`LauncherFactory` auto-registers engines/listeners via
`ServiceLoader.stream().map(Provider::type)…`. `ServiceLoader.Provider` is an
interface; its concrete impl is `ServiceLoader$ProviderImpl`. CratonVM calls the
abstract `Provider.type()` → AbstractMethodError. **This blocks the entire
JUnit5 launcher path on CratonVM** (keycloak-common, keycloak-crypto-default,
keycloak-server-spi-private, wildfly-health — all JUnit5 — cannot run, whereas
HotSpot passes, e.g. keycloak-common = 53 tests / 0 failures).

Note: plain `ServiceLoader.load(X).iterator()` (calling `Provider.get()` via the
iterator) WORKS on CratonVM — verified, `ServiceLoader.load(TestEngine)` returns
`junit-jupiter`. Only the `Provider.type()` interface call mis-dispatches. So the
bug is specific to certain interface methods, not ServiceLoader generally.

## Why it matters
- Breaks `Stream.collect(Collector)` for non-trivial collectors.
- Breaks `ServiceLoader.stream()/Provider.type()` → breaks JUnit5, and any
  framework using the SPI stream API (lots of them).
- Both are core JDK interfaces, so the blast radius across real apps is large.

## Reproduce (minimal)
```java
// A: collector
java.util.stream.Stream.of(1,2,3).collect(java.util.stream.Collectors.toList());
// B: serviceloader provider stream
java.util.ServiceLoader.load(java.nio.file.spi.FileSystemProvider.class)
    .stream().map(java.util.ServiceLoader.Provider::type).forEach(System.out::println);
```
Run both under `target/release/java.exe --java-home <jdk> -cp …` — expect the
"has no Code attribute" AbstractMethodError on CratonVM; HotSpot prints normally.

## What an agent should try next
1. Look at `invokeinterface` resolution / itable construction in the
   interpreter (and JIT). For an interface method whose concrete implementation
   lives on a class that implements the interface (incl. JDK-internal final
   classes like `ServiceLoader$ProviderImpl`, `Collectors$CollectorImpl`), the
   selector must pick the implementation's method, not the interface's abstract
   `method_info` (which has no Code).
2. Suspect: the method-resolution falls back to the interface declaration when
   the concrete class's vtable/itable slot isn't populated for that selector
   (name+descriptor), or when the implementing method is itself
   default/bridge/lambda-synthesized.
3. Both cases involve an implementation provided indirectly (lambda field for
   Collector; generated impl for Provider) — check synthetic/bridge method
   handling in itable build.
4. Fixing this unblocks JUnit5 on CratonVM and the server-spi MapperType tests.

## Scope note
This is the upstream cause behind the JUnit5 modules being "unmeasurable" on
CratonVM in the app-gauntlet run, and one of the 8 keycloak-server-spi failures.
Not related to the EC fix.
