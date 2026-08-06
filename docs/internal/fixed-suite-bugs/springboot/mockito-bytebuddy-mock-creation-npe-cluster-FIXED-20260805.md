# Mockito's inline-mock-maker NPEs deep inside its own machinery — three web-server classes — RESOLVED

**Status: FIXED (2026-08-05).** Not three unrelated ByteBuddy library bugs, and
not a GC/JIT interaction during agent-driven class transformation. It is the
recycled-`JitInvokeInfo` dispatch aliasing fixed by `383e7f5cf`; see
`flywayautoconfigurationtests-timeout-jit-site-cache-aliasing-FIXED-20260805.md`
for the mechanism and `mockito-bytebuddy-classfile-metadata-cluster-FIXED-20260805.md`
for the cluster-wide validation this page shares.

## Original symptom (as filed)

Three web-server-bootstrap classes failed with `NullPointerException`s thrown
from *inside* Mockito's `InlineByteBuddyMockMaker`/ByteBuddy stack — three
different NPE sites, all in third-party library internals:

1. `AnnotationConfigServletWebServerApplicationContextTests` — 9/9 tests failed,
   NPE at `WeakConcurrentMap.put(WeakConcurrentMap.java:91)` from
   `InlineDelegateByteBuddyMockMaker.doCreateMock`.
2. `ServletWebServerApplicationContextTests` — 2/32 failed, NPE somewhere in the
   recursive `TypeCache.findOrInsert` / `TypeCachingBytecodeGenerator.mockClass`
   chain.
3. `SpringApplicationWebServerTests` — 1/8 failed,
   `NPE: Cannot invoke "TypeDescription$Generic.represents(Type)" because the
   return value of "TypeDescription$Generic$LazyProjection.resolve()" is null`
   at `Advice$AdviceVisitor.<init>`.

HotSpot passed all three cleanly (9/9, 32/32, 8/8).

## Root cause

`383e7f5cf`. `vm/src/jit/helpers.rs` keys its per-thread dispatch memos on
`(vm_identity, JitInvokeInfo pointer)`; those boxes are freed with their
`CompiledMethod`, so a recycled address lets a compiled call site inherit the
previous site's resolution. `NATIVE_SITE_CACHE` holds a resolved leaf-native
callback, so the aliased site calls a *different* native and returns whatever
that returns.

`LazyProjection.resolve()` returning `null` where its own signature promises a
`TypeDescription$Generic`, and a `WeakConcurrentMap.put` NPE on a reference
ByteBuddy has just constructed, are both that: a call that cannot return null
returning null because a different callee answered it.

The page's own reading — *"a value that ByteBuddy's own internals assume is
never null turning out to be null"*, across three unrelated corners of one
pipeline — was the right observation attached to the wrong cause. That pattern
is not upstream data corruption feeding ByteBuddy; it is one call site in every
compiled body being served by another site's memo.

## Why the filed hypothesis was wrong, and why its diagnostic would not have found it

The page proposed a targeted repro that creates several Mockito mocks/spies of
the same shape in a tight loop **under `--nojit` first**, to decide between a
deterministic ByteBuddy/JDK-25 incompatibility and a GC/JIT-timing race.

The `--nojit` half of that would in fact have pointed the right way — the defect
is JIT-only and `--nojit` is clean — but the *tight loop* half would have buried
it. The aliasing needs a `CompiledMethod` to be **dropped** and its info address
re-issued to the **next** compile; a tight loop over one mock shape compiles a
small, stable set of bodies and drops none of them, so it is close to the worst
possible reproducer. The two ingredients that matter are a broad, churning
compile set and concurrency, which is why the full suite saw it and a focused
probe would not have.

The GC branch of the hypothesis would also have consumed a session: the value
is not a stale or moved reference. It is a *correct* reference to the wrong
object, produced by a correct call to the wrong callee.

## Validation

See `mockito-bytebuddy-classfile-metadata-cluster-FIXED-20260805.md` for the
full cluster measurement (13 classes, isolation and concurrent arms, four
binaries). The three classes on this page are part of that set.

Per-class, on the binary the page was filed from (`1078f6f05c`, no `383e7f5cf`)
under the concurrent arm:

| Class | Reproduced pre-fix | Fixed arms |
|---|---|---|
| `AnnotationConfigServletWebServerApplicationContextTests` | 0/6 rounds (this class needs the full-suite's wider compile churn) | 9/9 every run |
| `ServletWebServerApplicationContextTests` | 5/32 tests failed, `ClassCastException: java.lang.Integer cannot be cast to net.bytebuddy.description.NamedElement` — thrown from `AnnotationTypeMatcher.doMatch` → `NameMatcher.doMatch`, i.e. `AnnotationDescription.getAnnotationType()` handing back a `java.lang.Integer` | 32/32 every run |
| `SpringApplicationWebServerTests` | 1/8 failed, `NPE: Cannot invoke "ResolvableType.isArray()" because the return value of "ResolvableType.resolveType()" is null` | 8/8 every run |

The `Integer cannot be cast to NamedElement` face is the clearest witness on
this page: `getAnnotationType()` is declared to return a `TypeDescription`, its
only implementation routes through a `checkcast TypeDescription`, and it still
delivered a boxed `Integer` — which no data-level defect in CratonVM's
annotation or type metadata can produce, and an aliased native site produces
trivially.

`AnnotationConfigServletWebServerApplicationContextTests` did not reproduce in
6 concurrent rounds of 13 classes, so its retirement rests on the shared
mechanism plus 18 clean rounds on the fixed tip rather than on its own
before/after pair. Recorded here rather than smoothed over. It is also the one
class of the three whose filed symptom was **9/9 tests failing** — a
whole-class wipeout, which is what an aliased site in the single
`MockServletWebServer.initialize` → `Mockito.mock(...)` path each test shares
produces once it is hit, and which the full suite's compile churn reaches while
a 13-process rerun does not.

## Affected classes

- `module/spring-boot-web-server` — `org.springframework.boot.web.server.servlet.context.AnnotationConfigServletWebServerApplicationContextTests`
- `module/spring-boot-web-server` — `org.springframework.boot.web.server.servlet.context.ServletWebServerApplicationContextTests`
- `module/spring-boot-web-server` — `org.springframework.boot.web.server.SpringApplicationWebServerTests`
