# `ThymeleafServletAutoConfigurationTests.createLayoutFromConfigClass` hangs building a Groovy `MetaClass` during real template rendering

**Status: OPEN — found 2026-07-19. One real root cause found+FIXED 2026-07-19
(GC reference-queue field collision); the hang persists one level deeper in
a second, still-unfixed bug (`ArraysSupport.hashCode` on a
`ParameterizedTypeImpl`'s `actualTypeArguments`).**

## Background

`docs/known-issues/springboot/thymeleaf-groovy-layoutdialect-cluster.md`
(now closed — see
`docs/internal/springboot/thymeleaf-groovy-layoutdialect-cluster-FIXED.md`)
tracked a `GroovyRuntimeException: Could not find matching constructor for
...DecorateProcessor(...)` that made `ThymeleafServletAutoConfigurationTests`
fail 7/8 tests and crash before ever reaching real template rendering. Once
that bug was fixed, the whole test class runs far enough to expose this
**new, previously-unreachable** hang — and today's session found+fixed a
real, previously-unknown VM bug along the way, which changed WHERE (but not
whether) the hang occurs. Everything below reflects the state after that fix
(worktree `fix/thymeleaf-groovy-residuals-20260719`).

## Symptom

`createLayoutFromConfigClass` (the one test in this class that actually
renders a Thymeleaf template through the real `nz.net.ultraq` layout-dialect
`FragmentProcessor`) never completes — the suite runner reports `HANG` at
whatever timeout is configured (confirmed hung past 300s, in both JIT and
`--nojit` mode). It is also the first test JUnit5 selects to run in this
class (no `@TestMethodOrder` declared), so left in, it blocks every other
test in `ThymeleafServletAutoConfigurationTests` from ever running in the
same process (use `SbRunnerMethod` — see Reproduction — to run the class's
other tests despite this).

The full call chain (both before and after today's fix) is Groovy building a
`MetaClass` for the first time (via `java.beans.Introspector`) for the
receiver of the `getFragmentCollection(ITemplateContext)` extension-method
call site — this receiver was never pinned down to a specific class, but
doesn't need to be: `java.beans.Introspector.getBeanInfo()` (100% real JDK
bytecode — its `native-builtins::phases_late::introspector_get_bean_info`
Rust function exists fully written but **is never registered with the
native-method registry**, confirmed via `grep -rn '"getBeanInfo"'` across
`native-builtins/src` returning zero hits; a `CRATONVM_TRACE_BEANINFO=1`
env-gated trace was added to it this session in case it's ever wired up, but
it never fires today) calls the real `java.beans.MethodDescriptor(Method)`
constructor once per public method of the receiver's class, which calls
`java.beans.FeatureDescriptor.getParameterTypes` →
`com.sun.beans.TypeResolver.resolve` (real generic-type resolution,
recursing a few levels for nested parameterized types — NOT itself a sign
of a bug) → `com.sun.beans.WeakCache.get` (a
`private final Map<K, Reference<V>> map = new WeakHashMap<>()`-backed cache
TypeResolver keeps for resolved-type results) → `java.util.WeakHashMap.get`.

A standalone repro (`Introspector.getBeanInfo(SomeClass.class)` called
directly on `org.thymeleaf.context.{EngineContext,AbstractEngineContext,
WebEngineContext,AbstractContext,AbstractExpressionContext,
WebExpressionContext,ExpressionContext,Context,WebContext}` and
`nz.net.ultraq...FragmentExtensions`, all candidate receiver classes) is
**fast on both HotSpot and CratonVM** (each &lt;100ms) — the bug needs the
full application's GC/object-churn history to manifest, not just the target
class. This is why the earlier "which class is slow" framing in this doc's
first version was a dead end.

## Root cause #1 — FOUND AND FIXED 2026-07-19

**GC-driven `ReferenceQueue` auto-enqueue used the wrong field index for a
`WeakReference` subclass that redeclares its own field also named `next`.**

`java.util.WeakHashMap$Entry` extends `WeakReference<Object>` (real JDK
source) and additionally declares its OWN `value`, `hash`, and **`next`**
fields — `next` here being the entry's HASH-BUCKET chain pointer, an
entirely different linked list from `Reference`'s own `next` field (used
only for `ReferenceQueue` linkage: `referent, queue, next, discovered`).

When the GC auto-enqueues a `WeakReference`(-subclass) instance whose
referent was just collected — done from Rust in
`vm/src/runtime/interpreter.rs`'s two `to_enqueue` consumer loops (one in
`process_references_after_gc`, one in the marking-GC pre-cleanup path) and
mirrored in user-code-triggered `Reference.enqueue()`/`ReferenceQueue.poll()`
(`native-builtins/src/reference.rs`'s `ref_next_slot`) — the OLD code picked
the "next" slot with a bare heuristic: `if object_num_fields(ref_obj) > 2 {
2 } else { 0 }`. Slot 2 is correct **only if the object's actual field
layout happens to put `Reference`'s own `next` at index 2 and nothing else
redeclares a same-named field earlier/differently** — an assumption that
held for plain `WeakReference`/`SoftReference` instances (which add no
fields of their own) but is fragile for any subclass, and evidently breaks
down for `WeakHashMap$Entry` specifically under CratonVM's field-layout
computation.

**Effect:** a GC-driven auto-enqueue of a stale `WeakHashMap$Entry` spliced
the `ReferenceQueue`'s link over the WRONG field, corrupting that entry's
position in its own hash bucket's chain. A later `WeakHashMap.get()` walking
that bucket (`while (e != null) { ...; e = e.next; }` inside
`matchesKey`/the bucket loop) then looped forever over the corrupted chain.
Confirmed via `--stack-dump-on-timeout` + `--nojit`: 80 successive dumps
showed the interpreter PERMANENTLY parked at `WeakHashMap.matchesKey`'s
first bytecode instruction (`pc=0`, never advancing) — not merely slow.

This is the SAME underlying `com.sun.beans.TypeResolver`/`WeakCache`'s
internal `WeakHashMap` every time — `com.sun.beans.WeakCache`'s cache is
long-lived and accumulates entries across the whole application's generic-
type-resolution traffic, so a single corrupted GC-driven auto-enqueue
anywhere in the app's lifetime is enough to wedge `TypeResolver` forever
afterward — explaining why a fresh-process standalone repro on the same
target class never reproduces it (no prior GC/weak-ref churn) while the full
Spring Boot + Thymeleaf app (which allocates and discards enormous numbers
of transient objects before ever reaching this test) does.

### Fix

- `vm/src/runtime/interpreter.rs`: new `gc_reference_next_slot(shared,
  ref_obj)` helper — resolves the `next` slot BY NAME against
  `java/lang/ref/Reference`'s OWN declaring class (via
  `resolve_field_index_in_hierarchy`, called with `Reference`'s own
  `ClassId` so a subclass's shadowing same-named field is never found
  first), falling back to slot 0 only for the legacy 2-field synthetic
  shape. Replaces the `num_fields > 2 ? 2 : 0` heuristic at both
  `to_enqueue` consumer sites.
- `native-builtins/src/reference.rs`'s `ref_next_slot` (used by
  `Reference.enqueue()`/`ReferenceQueue.poll()`, so the poll side agrees
  with whatever the GC-driven enqueue side wrote): same fix, via
  `NativeContext::resolve_field_index("java/lang/ref/Reference", "next")`.

### Verification

- `ThymeleafReactiveAutoConfigurationTests`: 21/21 (also picks up the
  independently-fixed `path-tostring-indy-stringconcat-dead-dispatch`
  residual — see that doc's FIXED writeup).
- The hang's location moved: pre-fix, both JIT and `--nojit` dumps showed
  the interpreter permanently parked in `WeakHashMap.matchesKey` (bucket
  walk). Post-fix, the SAME reproduction now progresses past that point and
  parks somewhere new — see Root cause #2 below. This is strong evidence
  the fix is real (it's not just "a different random sample of the same
  hang") — the stuck point genuinely moved deeper into the call chain.
- No new failures observed in `ThymeleafReactiveAutoConfigurationTests` or
  the standalone `ReproWeakHashMapHang.java` probe (20 rounds of
  put-5000-then-`System.gc()`-then-`size()`, all completing normally).

## Root cause #2 — OPEN, deeply investigated, not yet fixed

With root cause #1 fixed, the SAME test still hangs, now one level deeper,
and this is a GENUINE unbounded loop (not merely slow): a run with no
watchdog and a 300s hard kill logged **574,867** calls to `Arrays.hashCode`
with no sign of terminating or slowing down.

### What the array-length/corruption theory (this doc's first version) got wrong

Added a diagnostic native override for `java/util/Arrays.hashCode
([Ljava/lang/Object;)I` (`native-builtins/src/lib.rs`, kept permanently,
`CRATONVM_TRACE_ARRAYS_HASHCODE=1`-gated tracing, semantically identical to
the real algorithm so it changes nothing when the flag is off) that prints
each array's actual length and, for `TypeVariable` elements, their `name`
and `genericDeclaration`. Result: the array is **always exactly length 1**,
never corrupted, never huge — that whole theory was wrong. The single
element, once the run reaches the truly-stuck tail (isolate it with
`CRATONVM_TRACE_ARRAYS_HASHCODE=1` and look at the LAST couple thousand
trace lines, not the first several seconds — early output is dominated by
completely unrelated `Arrays.hashCode` traffic from ordinary Spring Boot
startup, e.g. `String[]`/`Class[]`/`$Proxy` arrays, since this override is
global, not scoped to the hang), is **the exact same object, every single
time** — same identity hash, thousands of samples in a row:

```
java/lang/reflect/TypeVariable@<hash>(name="E", decl=java/lang/Class:java.util.List)
```

i.e. `java.util.List`'s own declared type parameter `E`. Confirmed this is
CratonVM's own SYNTHETIC stand-in object (`alloc_concurrent_synthetic(ctx,
"java/lang/reflect/TypeVariable", 3)` — the bare INTERFACE as the class,
not `sun.reflect.generics.reflectiveObjects.TypeVariableImpl`, which is
what a REAL, correctly-resolved `List.getTypeParameters()[0]` would be).

### Two real caching bugs found (both fixed) — reduced but did not eliminate the loop

`com.sun.beans.TypeResolver.resolve(TypeVariable, Map)` (real JDK bytecode)
needs to see the SAME `TypeVariable` object recur for its self-mapping
termination check to fire. `native-builtins/src/generics.rs` had TWO
independent gaps that built a FRESH, non-cached `TypeVariable` stand-in on
every call instead of reusing one:

1. `cached_building_type_parameter`/`cache_building_type_parameter`'s cache
   key used `ctx.class_id_from_mirror(decl)` — a reverse lookup that only
   resolves `Class` mirrors, always `None` for a `Method`/`Constructor`
   `decl`. **Fixed**: key on `ctx.identity_hash_code(decl)` instead, which
   works uniformly for any declaration kind.
2. `type_sig_to_java`'s `TypeSig::TypeVar` fallback arm (built when
   `resolve_declared_type_variable` can't find a match walking up to 16
   enclosing scopes) checked the cache on read but — a bug in THIS
   session's own first attempt at fixing gap 1 — never actually **wrote**
   to it, making the read side permanently a no-op. **Fixed**: added the
   missing `cache_building_type_parameter(...)` call after building the
   fallback stand-in.

**Verified impact**: call rate in a fixed 25s window dropped from
~14,700–32,300 (pre-fix runs) to ~8,400 (post gap-1-fix) to a STILL-looping
but somewhat different shape after the gap-2 fix. Both fixes are real,
correct, verified via `cargo test -p cratonvm-native-builtins --lib`
(3040/0/6, unchanged from dev baseline) and
`ThymeleafReactiveAutoConfigurationTests` (21/21, unchanged) — but neither
eliminates the hang. **The recurring object being the SAME identity in
EVERY trace (both before and after these fixes) means object-identity
caching was never actually the blocking factor** — `TypeResolver.resolve`
still doesn't terminate even when it keeps seeing the same `E` back.

### Current leading hypothesis — NOT YET CONFIRMED

Since `List`'s "E" is completely mundane (bound `[Object.class]`, no
self-reference, no F-bounded polymorphism), and a direct, isolated
`List.class.getTypeParameters()` call returns a correct REAL
`TypeVariableImpl` on CratonVM (byte-for-byte identical to HotSpot,
verified standalone — see `ReproListTypeParams.java`), the bug is NOT in
resolving `List`'s type parameter in isolation. It must be specific to the
**substitution map** `TypeResolver.resolveInClass`/`getTypeArguments`
builds by walking the ACTUAL introspected class's full generic
superclass/interface hierarchy — i.e. something about how CratonVM
resolves a generic type belonging to `List` while walking the class
hierarchy of whatever concrete class is actually being introspected
(still not pinned to a specific one — see "What's already ruled out").

Also directly tested and ruled out: a real `List` **default method**'s own
generic signature (`sort(Comparator<? super E> c)`, `replaceAll
(UnaryOperator<E> operator)`), both called directly via reflection and via
an inheriting concrete class (`ArrayList`), resolves instantly and
correctly on CratonVM, matching HotSpot exactly (see
`ReproListDefaultMethod.java`) — so a bare "introspect an inherited List
default method" scenario is NOT sufficient to reproduce this either. The
bug needs something about the FULL `resolveInClass`/substitution-map
machinery operating on the real target class's hierarchy, not any of the
narrower scenarios tested so far.

**Ruled out:** NOT unbounded/self-referential recursion through nested
`Type.hashCode()` calls or through the interpreter's call stack — every
stack dump shows the SAME small, fixed-depth call chain (`resolveInClass`
→ `resolve` → `resolve` → `resolve` → `WeakCache.get`, 3-4 `resolve` levels,
never more) — this is an OUTER loop re-invoking that same bounded chain
over and over, not stack-depth growth. NOT non-canonical Class mirrors
(`get_class_mirror` is a proper get-or-create singleton per `ClassId`,
confirmed by code review of `vm/src/vm/vm_exec.rs`'s
`get_or_create_class_mirror`). NOT `List.class.getTypeParameters()` being
broken in isolation, NOT a `List` default method's own generic parameter
resolution in isolation (both directly tested, both correct).

**Next steps for whoever picks this up:**
- The `CRATONVM_TRACE_ARRAYS_HASHCODE=1` native override
  (`native-builtins/src/lib.rs`, registered unconditionally, only the
  `eprintln!` tracing is env-gated) is left in place — it's the fastest way
  to re-confirm exactly which object is looping after any further change;
  look at the TAIL of a long run, not the head.
- Need to identify the ACTUAL target class(es) being introspected when the
  hang happens (still unknown — `introspector_get_bean_info`, the natural
  place to add this trace, is dead code; the REAL `Introspector`/
  `TypeResolver` bytecode gives no direct hook). Consider: a native
  override (temporary, diagnostic-only) for
  `com.sun.beans.TypeResolver.resolveInClass(Class, Type[])` that traces
  its `Class` argument before delegating to a hand-rolled equivalent of the
  real algorithm (risky to get semantically exact — many call sites/
  overloads) OR instrument `FeatureDescriptor.getParameterTypes`'s CALLER
  side instead (`MethodDescriptor.<init>`, native override already exists
  as dead code per `introspector_get_bean_info` — wiring just enough of it
  up to trace the target `Method` before falling through to real bytecode
  might be lower-risk than intercepting `TypeResolver` itself).
- Once the target class is known, build a MUCH more surgical standalone
  repro: `TypeResolver.resolveInClass(targetClass, someMethod
  .getGenericParameterTypes())` directly, to reproduce outside the full
  Spring Boot/Groovy/Thymeleaf stack — this would make iteration far
  cheaper than the current ~15-25 min per full-rebuild-and-rerun cycle on
  this host.
- Given the substitution-map (`getTypeArguments`) walks the FULL generic
  superclass/interface chain, and `List` surfaced specifically, check
  whether the introspected class (or something in its ancestry) has a
  generic interface/superclass signature that, when converted through
  `native-builtins/src/generics.rs`'s `typesig_to_real_type`/
  `type_sig_to_java`, produces a subtly wrong `ParameterizedType` for
  something involving `List` — e.g. a raw `List` usage, or a
  `List<SomeTypeVar>` where `SomeTypeVar` itself needs multi-hop resolution
  through more than one enclosing scope.

## What's already ruled out

- **Which class is being introspected does not matter in isolation.**
  `Introspector.getBeanInfo()` on every plausible receiver candidate
  (`FragmentExtensions`, and all `org.thymeleaf.context.*` implementations
  of `ITemplateContext`) is fast standalone on both HotSpot and CratonVM.
  The bug requires the full application's prior object/GC churn, not
  anything specific to the target class's own generics.
- `java.beans.Introspector.getBeanInfo` runs 100% real JDK bytecode in
  CratonVM (see Symptom section) — `introspector_get_bean_info`
  (`native-builtins/src/phases_late.rs`) is fully implemented but dead code,
  never registered.

## Reproduction

Single-method run (avoids the whole-class hang from `createLayoutFromConfigClass`
running first with no `@TestMethodOrder`) via the `SbRunnerMethod` JUnit
Platform `selectMethod` launcher (`apps/spring-boot/sb-runner/SbRunnerMethod.java`,
already compiled to `.class` in that directory):

```powershell
$exe.exe --java-home "<JDK25>" --stack-dump-on-timeout 20 -cp "<module classpath + sb-runner + junit-platform launcher jars>" `
  SbRunnerMethod org.springframework.boot.thymeleaf.autoconfigure.ThymeleafServletAutoConfigurationTests createLayoutFromConfigClass
```

Add `--nojit` for a full interpreted stack (JIT mode's dump can be missing
the innermost 1-2 frames — "thread has N active JIT call(s) on its native
stack ... not represented in the frame(s) above").

Excluding just this one test method lets the rest of the class run to
completion normally.

## Affected classes

- `module/spring-boot-thymeleaf` | `ThymeleafServletAutoConfigurationTests` | `createLayoutFromConfigClass()` (HANG, blocks the rest of the class from running in the same process)
