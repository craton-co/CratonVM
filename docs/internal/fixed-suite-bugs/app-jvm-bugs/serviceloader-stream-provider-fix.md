# ServiceLoader.stream() must yield Provider wrappers — fixes `Provider.type() has no Code attribute`

## Symptom (from comparison-handoff/bug-interface-method-dispatch-no-code-attribute.md)

```
WARN  NoSuchMethodError method="jdk/internal/jrtfs/JrtFileSystemProvider.type()Ljava/lang/Class;"
Exception: AbstractMethodError: method java/util/ServiceLoader$Provider.type()Ljava/lang/Class; has no Code attribute
```

Repro:
```java
java.util.ServiceLoader.load(java.nio.file.spi.FileSystemProvider.class)
    .stream().map(java.util.ServiceLoader.Provider::type).forEach(System.out::println);
```
CratonVM threw `AbstractMethodError`; HotSpot prints the provider classes.

## Root cause — NOT an itable/invokeinterface dispatch bug

The original handoff diagnosed this as `invokeinterface` selecting the abstract
interface method instead of the concrete impl. That is a misread. The real
cause is the synthetic `java.util.ServiceLoader.stream()` native returning the
wrong element type.

`native-builtins/src/service_loader.rs::native_sl_stream` drained `iterator()`
— which correctly yields service **instances** `S` — directly into the
synthetic stream. But real JDK `ServiceLoader.stream()` returns
`Stream<Provider<S>>`: each element is a `java.util.ServiceLoader$Provider`
**wrapper** exposing `type()` (the provider Class, *without* instantiating) and
`get()` (lazy instantiation).

So `.map(Provider::type)` executed `invokeinterface Provider.type()` against a
raw service instance (e.g. `jdk.internal.jrtfs.JrtFileSystemProvider`), which
has no `type()` method. The interpreter retargeted to the receiver's real class
(`JrtFileSystemProvider.type()` → `NoSuchMethodError` WARN), then fell back to
the abstract `Provider.type()` declaration (no Code) → `AbstractMethodError`.
That fallback chain is exactly the symptom the handoff described.

Diagnostic confirming the element-type mismatch:

| VM | stream class | element class | `instanceof Provider` |
|---|---|---|---|
| CratonVM (before) | `java.util.stream.Stream` (synthetic) | `jdk.internal.jrtfs.JrtFileSystemProvider` | false |
| HotSpot | `ReferencePipeline$Head` | `java.util.ServiceLoader$ProviderImpl` | true |

## Fix

`native_sl_stream` now builds a real
`java/util/ServiceLoader$ProviderImpl(service, type, ctor)` per discovered
provider:
- `forName(<impl>)` → the provider `type` Class,
- `type.getDeclaredConstructor()` (+ `setAccessible(true)`) → the no-arg ctor,
- `new ProviderImpl(service, type, ctor)` — the classpath-flavour constructor
  (factoryMethod = null).

`ProviderImpl.type()`/`get()` then run real JDK bytecode (no synthetic stub),
and `o instanceof ServiceLoader.Provider` holds. Wrappers are accumulated in a
pinned `ArrayList` (per-iteration native pins on the intermediate
`type`/`ctor`/`provider` refs) so a moving GC during the repeated
`forName`/`getDeclaredConstructor`/`<init>` re-entries can't leave stale refs —
mirroring `native_sl_iterator`'s GC discipline.

If `ServiceLoader$ProviderImpl` can't be resolved (stripped runtime), it falls
back to draining instances (`drain_instances_to_stream`) so the stream is
non-empty rather than crashing.

Only `stream()` changed. `iterator()` and `spliterator()` correctly keep
yielding service instances `S` (`ServiceLoader implements Iterable<S>`).

## Verification

- `stream().map(Provider::type)` and `Provider::get()` both run on CratonVM and
  match HotSpot element-for-element (old build threw `AbstractMethodError`).
- `ServiceLoader.stream()` elements are now `java.util.ServiceLoader$ProviderImpl`
  with `instanceof Provider == true`.
- `org.junit.platform.launcher.LauncherFactory.create()` + discover/execute
  reaches test execution (the `Provider.type()` crash is gone).
- `Stream.of(...).collect(Collectors.toList()/toMap())` unaffected (manifestation
  1 already passes on dev).
- `cargo test -p cratonvm-native-builtins service_loader` green; ServiceLoader
  e2e/JDBC-driver-loader tests green.

## Out of scope (separate pre-existing bugs, unchanged by this fix)

1. **Provider discovery under-count** — CratonVM finds fewer providers than
   HotSpot for jimage-only SPIs (e.g. CharsetProvider → 0; FileSystemProvider →
   1 vs 2). `discover_providers` walks classpath `../../../../apps/META-INF/services`, not the
   full module/jimage provider set.
2. **`DefaultClassDescriptor` undersized object layout** — once the JUnit5
   launcher runs, `org/junit/jupiter/engine/discovery/DefaultClassDescriptor`
   is allocated with 0 slots though the class declares 2 fields
   (`gen_heap::get_field: out-of-bounds field read dropped`), failing tests.
   Present on dev too; unrelated to dispatch.
