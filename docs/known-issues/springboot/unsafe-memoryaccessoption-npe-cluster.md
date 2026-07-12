# `sun.misc.Unsafe$MemoryAccessOption.ordinal()` NPE — likely a `<clinit>`-time bootstrap gap, same family as the already-fixed `Unsafe`/`Build$CurrentHolder` bug

**Status: OPEN, characterized. Severity: HIGH (≥26 classes; two call shapes,
one root cause).**

Found while triaging `FAIL`s from the first full Spring Boot suite run (see
[[project_spring_boot_suite_runner_20260711]]). Two symptom shapes, same
underlying NPE:

1. **Direct** (15 classes, mostly `spring-boot-couchbase`'s
   `ClusterEnvironment` construction and `spring-boot-data-redis`'s Lettuce
   `DefaultClientResources`):
   ```
   org.springframework.beans.factory.BeanCreationException: ... Factory method 'X' threw exception with message: Cannot invoke "sun.misc.Unsafe$MemoryAccessOption.ordinal()"
   ```
2. **Via Netty** (11 more classes — `spring-boot-http-client`'s reactive
   connector, MongoDB's and Neo4j's Netty-backed drivers, etc.): Netty's
   `MultithreadEventLoopGroup` uses `sun.misc.Unsafe`'s legacy memory-access
   methods internally for performance, so its own child-`NioEventLoop`
   constructor hits the identical NPE, surfacing three levels up as
   `IllegalStateException: failed to create a child event loop`:
   ```
   Caused by: java.lang.IllegalStateException: failed to create a child event loop
   Caused by: java.lang.NullPointerException: Cannot invoke "sun.misc.Unsafe$MemoryAccessOption.ordinal()"
   ```
   Confirmed identical root cause: all 11 "child event loop" classes'
   full logs contain the same `MemoryAccessOption` NPE as their innermost
   `Caused by:`. Fixing the root NPE should clear both symptom groups.

(full NPE text truncated by Spring's exception-chaining, but the shape —
`Cannot invoke "X.ordinal()" because ... is null` — is the standard
helpful-NPE message for calling `.ordinal()` on a null enum reference.)

## What's real vs. suspect

`sun.misc.Unsafe$MemoryAccessOption` **is a real JDK 25 class**
(`final class ... extends Enum<MemoryAccessOption>`, constants `ALLOW` /
`WARN` / `DEBUG` / `DENY`, part of the JEP restricting legacy
`sun.misc.Unsafe` memory-access methods) — confirmed via `javap -p` against
the real `jdk-25` install, not a CratonVM-fabricated name. It exposes a
package-private static `value()` accessor that legacy `Unsafe.getInt`/
`putLong`/etc. call internally to decide whether to warn/deny; Couchbase's
and Lettuce's own low-level off-heap tricks call into that legacy `Unsafe`
API path directly, triggering it.

## Suspected root cause

`MemoryAccessOption.value()`'s backing static field is almost certainly
populated by `<clinit>`-time logic (reading the
`sun.misc.unsafe.memory.access` system property once and caching a default
enum constant) — the same **class of bug** as the already-fixed
"`Unsafe.<clinit>` computes 9 `ARRAY_*_BASE_OFFSET`/`ARRAY_*_INDEX_SCALE`
constants via natives (`arrayBaseOffset0`/`arrayIndexScale0`) not registered
yet this early in real-JDK bootstrap, silently returning 0 instead of
throwing" (see the `docs/known-issues/README.md` "ES suite-wide
`Build$CurrentHolder` manifest-null FIXED" entry, fixed via a
post-clinit success-path backfill in `vm/src/vm/vm_util.rs`). That fix
covered `Unsafe`'s own constants and `UnsafeConstants`; it did not
necessarily cover `Unsafe$MemoryAccessOption`'s separate `<clinit>` (a
nested class with its own bootstrap timing). Needs the same treatment: find
what `Unsafe$MemoryAccessOption.<clinit>`/`defaultValue()` depends on that
isn't available yet at this point in real-JDK bootstrap, and either make it
available earlier or add a post-clinit backfill mirroring the existing
pattern in `vm_util.rs`.

## Repro

```powershell
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -ClassList <TSV row for module/spring-boot-couchbase's CouchbaseAutoConfigurationTests> `
  -Start 1 -Count 1 -Exe <cratonvm exe>
```
Standalone repro (no Spring needed) should be simpler: any class calling
`sun.misc.Unsafe.getUnsafe().getInt(obj, offset)` (a legacy memory-access
method) early in a fresh process.
