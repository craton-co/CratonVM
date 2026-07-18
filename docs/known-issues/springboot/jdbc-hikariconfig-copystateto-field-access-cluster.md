# `HikariCheckpointRestoreLifecycleTests` — `Field.get` throws `IllegalAccessException` on a private final `AtomicReference` field despite `setAccessible(true)`

**Status: OPEN — found 2026-07-17**

## Symptom

All 6 tests in the class fail identically, at construction time (in the
`@BeforeEach`/field-initializer path, `HikariCheckpointRestoreLifecycleTests.java:51`):

```
java.lang.RuntimeException: Failed to copy HikariConfig state: cannot access member: modifiers 0x0012, Field.get(Ljava/util/concurrent/atomic/AtomicReference;)
       com.zaxxer.hikari.HikariConfig.copyStateTo(HikariConfig.java:1033)
       com.zaxxer.hikari.HikariDataSource.<init>(HikariDataSource.java:77)
       org.springframework.boot.jdbc.HikariCheckpointRestoreLifecycleTests.<init>(HikariCheckpointRestoreLifecycleTests.java:51)
     Caused by: java.lang.IllegalAccessException: cannot access member: modifiers 0x0012, Field.get(Ljava/util/concurrent/atomic/AtomicReference;)
       com.zaxxer.hikari.HikariConfig.copyStateTo(HikariConfig.java:1033)
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-jdbc.org.springframework.boot.jdbc.HikariCheckpointRestoreLifecycleTests.out.log`

`0x0012` = `ACC_PRIVATE (0x0002) | ACC_FINAL (0x0010)` — an ordinary
private final instance field. `HikariDataSource(HikariConfig)` (the
copy-constructor used by Spring Boot's checkpoint/restore lifecycle support
to snapshot a live pool's config before pausing it) calls
`HikariConfig.copyStateTo(other)`, which reflectively iterates
`this.getClass().getDeclaredFields()`, calls `field.setAccessible(true)`,
then `field.get(this)` for every field to shallow-copy config state — one of
those fields is declared `private final AtomicReference<...>` and the
`get()` call throws `IllegalAccessException` even though `setAccessible`
was (per the real JDK contract HotSpot honors, since this test passes on
HotSpot) supposed to have suppressed the access check.

## Root cause (hypothesis, not confirmed against CratonVM's reflection natives)

Not traced to a specific `native-builtins` file:line in this pass (no
`java/lang/reflect/Field` native-registration source was read). The
symptom — `setAccessible(true)` followed by `Field.get()` still throwing
`IllegalAccessException` for a `private final` field of a **reference**
type (`AtomicReference`), reached via `getClass().getDeclaredFields()` +
bulk reflection over a third-party class (`HikariConfig`) — is consistent
with the general shape of prior CratonVM reflection-access gaps recorded in
this codebase's history (loader-identity mismatches and field-descriptor
resolution quirks in the `Field`/`Method` reflection natives), but this
specific field (private final `AtomicReference`) was not confirmed against
current source. Two candidate mechanisms, neither verified:

1. The native `Field.get` implementation checks the field's access
   modifiers directly from the class file rather than consulting the
   `Field` mirror object's own `override`/accessible flag that
   `setAccessible(true)` is supposed to set — i.e. `setAccessible` updates
   the wrong piece of state, or `Field.get` reads the wrong one.
2. Something specific to `AtomicReference`-typed fields (a reference type
   nested inside `java.util.concurrent.atomic`) trips a different code path
   than the scalar/`String` fields `copyStateTo` also copies (the same
   method copies many other `HikariConfig` fields of primitive/String type
   without error, per the trace showing the failure happens once, on this
   one field, not for every field) — possibly a field-descriptor or
   slot-lookup issue specific to that one field's position/type in the
   class layout.

**What would confirm/refute:** a standalone repro —
`Field f = SomeClass.class.getDeclaredField("someAtomicReferenceField");
f.setAccessible(true); f.get(instance);` for a private final
`AtomicReference` field declared on an ordinary (non-JDK) class, run in
isolation, comparing behavior for that field vs. a private final `String`/
`int` field on the same class to see if the type or the position is what
triggers it.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-jdbc` | `org.springframework.boot.jdbc.HikariCheckpointRestoreLifecycleTests` (6 of 6 tests — whole class) |
