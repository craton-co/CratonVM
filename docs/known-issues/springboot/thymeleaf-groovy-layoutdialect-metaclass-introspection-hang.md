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

## Root cause #2 — OPEN, not yet fixed

With root cause #1 fixed, the SAME test still hangs, now one level deeper.
`--stack-dump-on-timeout` + `--nojit` shows the interpreter permanently
parked (single dump only this time — not yet re-confirmed across multiple
samples the way root cause #1 was, so treat "permanently" as provisional)
at:

```
com/sun/beans/TypeResolver.resolve (recursing, as before)
  -> com/sun/beans/WeakCache.get -> java/util/WeakHashMap.get -> java/util/WeakHashMap.hash
  -> sun/reflect/generics/reflectiveObjects/ParameterizedTypeImpl.hashCode
  -> java/util/Arrays.hashCode([Ljava/lang/Object;)
  -> jdk/internal/util/ArraysSupport.hashCode([Ljava/lang/Object;III)
```

i.e. `WeakHashMap.get(key)`'s very first step — `hash(k)`, called BEFORE any
bucket walk — computes `k.hashCode()` where `k` is a real
`sun.reflect.generics.reflectiveObjects.ParameterizedTypeImpl` (one of
`TypeResolver`'s cache keys). That class's real `hashCode()` is
`Arrays.hashCode(actualTypeArguments) ^ owner.hashCode() ^ rawType.hashCode()`,
and `ArraysSupport.hashCode(Object[], int, int, int)`'s bytecode (a simple
per-element loop calling `Objects.hashCode()`, NOT the SIMD/vectorized path
used for primitive-array overloads — confirmed via `javap -c
jdk.internal.util.ArraysSupport`) is where the interpreter is stuck.

**Leading hypothesis (not yet confirmed):** the `actualTypeArguments` array
on this `ParameterizedTypeImpl` — built by
`native-builtins/src/generics.rs`'s `typesig_to_real_type` (constructs REAL
`sun.reflect.generics.reflectiveObjects.ParameterizedTypeImpl`/
`WildcardTypeImpl` objects via `ctx.alloc_object` + `set_field_by_name`, for
`Field.getGenericType`/`Class.getGenericInterfaces`/etc.) — has a corrupted
or unexpectedly-huge length, making the per-element loop take a very long
time (or genuinely never terminate, if the length field itself reads as
garbage). Two candidate mechanisms, neither yet verified:

1. A construction-time bug in `typesig_to_real_type`'s array-fill loop
   (`native-builtins/src/generics.rs` ~line 698-705) — the loop already
   follows the established pin/re-read-after-every-allocation GC-safety
   pattern used throughout that file, so a straightforward reread of that
   code didn't turn up an obvious bug, but it wasn't ruled out by direct
   testing.
2. A moving-GC bug corrupting the array's header/length field during a
   LATER evacuation (after correct construction) — consistent with this
   codebase's history of array-header-corruption bugs in other contexts
   (`reference_synthetic_native_wrong_layout_corrupts_adjacent_object`,
   `reference_compact_field_slot_fabricated_nonref_bug` in project memory).

**Ruled out:** NOT unbounded/self-referential recursion through nested
`Type.hashCode()` calls — the stack dump shows each frame exactly once (no
repeated `ParameterizedTypeImpl.hashCode → Arrays.hashCode → ...` cycle),
which would be visible if the array contained a cyclic reference back to
itself or an ancestor type.

**Next steps for whoever picks this up:**
- Re-run with `--stack-dump-on-timeout` at a SHORT interval multiple times
  in a row (as was done for root cause #1) to confirm the PC is genuinely
  frozen (not just slow) before investing further — the single dump
  captured this session doesn't yet establish that as conclusively as root
  cause #1's 80-sample confirmation did.
- Add an env-gated `eprintln!` in `typesig_to_real_type`'s
  `ParameterizedType`-building arm printing `type_args.len()` at
  construction and (separately) the array's `ctx.array_length()` right
  after the fill loop, to catch a construction-time mismatch directly.
- If construction looks correct, suspect GC corruption instead — try
  reproducing with a non-moving/simpler GC backend if CratonVM supports
  switching, or add a length-sanity assertion at the `Arrays.hashCode`
  native call boundary (there isn't one currently, since `Arrays.hashCode`
  runs unmodified real bytecode).

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
