# `java.util.Properties.computeIfAbsent` NPEs on `this.map` for a binder-instantiated `Properties` target

**Status: OPEN — found 2026-07-23 (hypothesis, not fully pinned to file:line)**

## Symptom

```
org.springframework.boot.context.properties.bind.BindException: Failed to bind properties under 'foo' to java.util.Properties
     Caused by: java.lang.NullPointerException: Cannot invoke "java.util.concurrent.ConcurrentHashMap.computeIfAbsent(Object, java.util.function.Function)" because "this.map" is null
       java.util.Properties.computeIfAbsent(Properties.java:1496)
       org.springframework.boot.context.properties.bind.MapBinder$EntryBinder.bindEntries(MapBinder.java:186)
       org.springframework.boot.context.properties.bind.MapBinder.bindAggregate(MapBinder.java:76)
```

`MapBinderTests.bindToPropertiesShouldBeEquivalentToMapOfStringString()`
fails while `Binder.bind("foo", Bindable.of(Properties.class))` is filling
in a freshly-created `Properties` instance.

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard4/logs/core_spring-boot.org.springframework.boot.context.properties.bind.MapBinderTests.out.log`

## Root cause (hypothesis)

`java.util.Properties.computeIfAbsent(Properties.java:1496)` is real,
unmodified JDK bytecode — since JDK 9, `Properties` stopped using its
inherited `Hashtable.table` storage and instead wraps a `private transient
ConcurrentHashMap<Object, Object> map` field, initialized by `Properties()`'s
constructor (`this.map = new ConcurrentHashMap<>(8)`), which
`computeIfAbsent` (and most other mutators) delegate to. A `NullPointerException`
on `this.map` here means the `Properties` object `MapBinder`'s aggregate
binder is mutating was never run through its real constructor.

CratonVM allocates `java/util/Properties` via `alloc_concurrent_synthetic`
shortcuts — which construct an object with N raw fields directly, bypassing
`<init>` — in several places already
(`native-builtins/src/cds.rs:621`, `native-builtins/src/deprecated_io_util.rs:2353`,
`native-builtins/src/deprecated_util.rs:2691`, `native-builtins/src/lib.rs:29242`),
confirming this is an established pattern in the codebase for this exact
class. The working hypothesis is that whatever mechanism Spring's binder
infrastructure uses to instantiate a fresh, empty `Properties` target (most
likely reflective `Constructor.newInstance()` via `BeanUtils.instantiateClass`
or an equivalent `Bindable`-driven "new empty aggregate" path) goes through
one of these — or a similar, not-yet-located — synthetic-allocation
shortcut instead of genuinely invoking `Properties()`'s real constructor
chain, leaving `map` null.

Not confirmed to the exact allocation call site `MapBinder`'s reflective
instantiation path reaches — none of the four `alloc_concurrent_synthetic`
call sites found above are obviously reachable from
`Bindable.of(Properties.class)`'s construction path, so there may be a
fifth, not-yet-located one, or a different mechanism (e.g. a JIT-fast-path
`invokespecial <init>` skip) with the same net effect. **What would
confirm/refute:** a standalone repro —
`Properties p = Properties.class.getDeclaredConstructor().newInstance(); p.computeIfAbsent("k", x -> "v");`
— would show whether the gap is in generic no-arg reflective construction
of `Properties` (broad) or specific to the `MapBinder`/`Bindable`
instantiation path (narrow).

## Affected classes

| Module | Class |
|---|---|
| core/spring-boot | org.springframework.boot.context.properties.bind.MapBinderTests (1 of 45 failures: `bindToPropertiesShouldBeEquivalentToMapOfStringString`) |
