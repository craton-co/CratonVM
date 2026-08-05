# Mockito's inline-mock-maker (ByteBuddy) intermittently NPEs deep inside its own machinery — `AnnotationConfigServletWebServerApplicationContextTests`, `ServletWebServerApplicationContextTests`, `SpringApplicationWebServerTests`

**Status: OPEN — found 2026-08-05**

## Symptom

Three web-server-bootstrap test classes fail with `NullPointerException`s
thrown from *inside* Mockito's `InlineByteBuddyMockMaker`/ByteBuddy stack
while creating or advising a mock class — three different NPE sites, all
in third-party library internals rather than application/Spring code:

1. `AnnotationConfigServletWebServerApplicationContextTests` — **9/9 tests
   fail**:
   ```
   java.lang.NullPointerException
     org.mockito.internal.util.concurrent.WeakConcurrentMap.put(WeakConcurrentMap.java:91)
     org.mockito.internal.creation.bytebuddy.InlineDelegateByteBuddyMockMaker.doCreateMock(InlineDelegateByteBuddyMockMaker.java:419)
   ```
   via `MockServletWebServer.initialize` → `Mockito.mock(...)`.

2. `ServletWebServerApplicationContextTests` — **2/32 tests fail**, NPE
   somewhere in the recursive `TypeCache.findOrInsert` /
   `TypeCachingBytecodeGenerator.mockClass` chain (exact NPE message cut
   off by log truncation at the point of investigation — the visible
   frames are all `net.bytebuddy.TypeCache`/`InlineBytecodeGenerator`).

3. `SpringApplicationWebServerTests` — **1/8 tests fail**:
   ```
   java.lang.NullPointerException: Cannot invoke "net.bytebuddy.description.type.TypeDescription$Generic.represents(java.lang.reflect.Type)" because the return value of "net.bytebuddy.description.type.TypeDescription$Generic$LazyProjection.resolve()" is null
     net.bytebuddy.description.type.TypeDescription$Generic$LazyProjection.represents(TypeDescription.java:6435)
     net.bytebuddy.asm.Advice$AdviceVisitor.<init>(Advice.java:11922)
   ```
   during `InlineBytecodeGenerator.triggerRetransformation` (Mockito's
   self-attach agent instrumenting a real class for a spy/partial mock).

All three sit in different corners of Mockito/ByteBuddy's own
class-generation pipeline (weak-reference bookkeeping, a type cache, and
generic-type resolution during advice weaving respectively) rather than
one obvious shared code path — but all three are triggered by the same
kind of operation (Mockito generating/transforming a class at runtime via
its self-attach inline-mock-maker agent), and all three manifest as a
value that ByteBuddy's own internals assume is never null turning out to
be null. That pattern (different crash sites, same "library invariant
that's normally unbreakable turned out false") is more consistent with
upstream corruption feeding into ByteBuddy (e.g. a class-metadata read
CratonVM serves for a generated/redefined class, or a GC/JIT interaction
during agent-driven class transformation) than with three unrelated
library bugs.

## Cross-check

HotSpot baseline passes all three classes cleanly:
`AnnotationConfigServletWebServerApplicationContextTests` 9/9,
`ServletWebServerApplicationContextTests` 32/32,
`SpringApplicationWebServerTests` 8/8. Not a CRLF-fixture issue — these are
in-process class generation failures, not resource reads. Genuinely
CratonVM-specific.

No existing doc found for `WeakConcurrentMap.put` NPE,
`InlineDelegateByteBuddyMockMaker.doCreateMock` NPE, `TypeCache.findOrInsert`
NPE, or `LazyProjection.resolve()`-returns-null (searched `docs/known-issues/`
and `docs/internal/` by symptom). The closest prior docs
(`docs/internal/mockito-redefine-makes-every-call-40us-20260726.md`,
`docs/internal/fixed-suite-bugs/springboot/otlpmetricspropertiesconfigadaptertests-mockito-bytebuddy-hang-FIXED.md`,
`docs/internal/fixed-suite-bugs/springboot/jta-testdatabase-mockito-cold-selfattach-mockmethodadvice-FIXED.md`)
cover a Mockito self-attach *performance* regression, a *hang*, and a cold
*classloading* NPE respectively — none describe these three NPE shapes.

## Root cause

Not identified this session. All three logs carry the routine
`Mockito is currently self-attaching to enable the inline-mock-maker...`
warning (present in passing HotSpot/CratonVM runs alike, not itself a
symptom). Needs further investigation: a targeted repro that creates
several Mockito mocks/spies of the same shape as these three tests in a
tight loop, under `--nojit` first (per this repo's own convention of
bisecting JIT vs interpreter before blaming a subsystem), would help
determine whether this is deterministic-given-input (a real ByteBuddy/JDK
25 classfile-generation incompatibility CratonVM exposes) or a
race/corruption (GC-timing- or JIT-compile-timing-dependent, given the
three different crash sites).

## Affected classes

- `module/spring-boot-web-server` — `org.springframework.boot.web.server.servlet.context.AnnotationConfigServletWebServerApplicationContextTests`
- `module/spring-boot-web-server` — `org.springframework.boot.web.server.servlet.context.ServletWebServerApplicationContextTests`
- `module/spring-boot-web-server` — `org.springframework.boot.web.server.SpringApplicationWebServerTests`
