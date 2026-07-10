# `EnumMap` real-JDK-mode corruption + `StackWalker` frame order — FIXED

Status: FIXED (dev, commit `b939e303`, merged `528fde12`)

Date observed: 2026-07-09
Date fixed: 2026-07-09

## Context

Found while verifying `docs/known-issues/elasticsearch-suite/ES-RUN-20260709-currentdev-nonpassed-rerun-120s-summary.md`
(2649-class ES non-passed rerun, 2640/2649 `rc=139` crashes dominated by
`MemoryLayout.varHandle` `AbstractMethodError`). That rerun (run
`es-nonpassed-currentdev-20260709-082115`, started 08:21:15) predates the
varHandle fix in commit `9494a0a5` (landed 09:17:23 the same day, already on
`dev`), so its crash counts are stale. Building fresh from current `dev` and
re-probing uncovered two further bugs that the varHandle crash had been
masking end-to-end.

## Bug 1: `EnumMap.<init>(Class)` corrupts real-JDK objects

`EnumMap.<init>(Ljava/lang/Class;)V` is dispatched via **invokespecial**,
where a registered native always wins over bytecode (see
`invoke_special_shared` in `vm/src/vm/vm_exec.rs`: "Native registry
overrides take priority"). `native_em_init`
(`native-builtins/src/phases_early.rs`) was promoted into the essential
(both-mode) registration set (`native-builtins/src/lib.rs`,
`register_essential_natives`) so early bootstrap (logging, before the
synthetic phase-50 registrations land) sees a working synthetic `EnumMap`.

That essential registration also fires for a **real** `java/util/EnumMap`
loaded from the real JDK in real-JDK mode. `native_em_init` unconditionally
writes `Value::Object(buckets-array)`/`Value::Int(0)`/`Value::Int(16)` to
field indices 0/1/2 — correct for the synthetic 3-field layout
(buckets/size/capacity), but the real class's own field table is
`keySet`/`values` (inherited from `AbstractMap`, indices 0/1) then
`keyType`/`keyUniverse`/`vals`/`size`/`entrySet` (indices 2-6). So the
"init" clobbers `keyType` (real index 2) with a `Reference` array and
coerces `keyUniverse`/`vals` (ref-typed fields hit with `Value::Int`) to
null. Every subsequent `EnumMap.put()` (real bytecode, confirmed via
`EnumMap.java` line numbers in the stack trace) throws
`ClassCastException: class X != null` from `typeCheck()`, wrapped as
`ExceptionInInitializerError` wherever a static `EnumMap` field is
populated at class-init time — e.g.
`com.carrotsearch.randomizedtesting.Threads.<clinit>`, hit by every ES
JUnit run through `RandomizedRunner`/`ThreadLeakControl`.

Minimal repro (no ES needed):

```java
import java.util.EnumMap;
public class Repro {
    enum Color { RED, GREEN, BLUE }
    public static void main(String[] args) {
        EnumMap<Color, String> m = new EnumMap<>(Color.class);
        m.put(Color.RED, "r"); // ClassCastException: class Repro$Color != null
    }
}
```

### Fix

- `native_em_init` now checks `ctx.is_class_synthetic_stub("java/util/EnumMap")`.
  When false (real class loaded), it mirrors the real constructor by field
  name instead of by hardcoded index: sets `keyType`, calls
  `key.getClass().getEnumConstants()` to populate `keyUniverse`, and
  allocates `vals` to match.
- Split the essential registration: added
  `register_enum_map_init_native` (registers only `<init>`) for the
  essential (`lib.rs`) call site. The full `register_enum_map_natives`
  (put/get/size/containsKey/etc) stays registered **only** from the
  synthetic-mode phase-50 path — those are ordinary invokevirtual calls
  that already ran correctly against real bytecode once `<init>` leaves the
  object properly initialized. Registering them on the concrete class was
  independently defeating `try_delegate_real_collection`'s real-JDK
  fallback: its `invoke_special` call resolves straight back to the same
  natively-registered method, the self-recursion guard in
  `native-collections/src/lib.rs::try_delegate_real_collection` trips, and
  the caller silently gets the stale synthetic answer (`EnumMap.size()`/
  `get()` reporting empty right after a real `put()` — confirmed via a
  second repro before this half of the fix landed).

Files: `native-builtins/src/phases_early.rs`, `native-builtins/src/lib.rs`.

## Bug 2: `StackWalker.walk()`/`forEach()` frame order reversed

`p59_sw_walk`/`p59_sw_for_each` (`native-builtins/src/phases_late.rs`,
`register_p59_stackwalker`) — the natives actually registered for
`StackWalker.walk(Function)`/`forEach(Consumer)` — built the
`Stream<StackFrame>`/iterated frames directly from
`ctx.capture_stack_trace(0)`, which returns outer→inner (oldest-frame-first)
order. Real JDK `StackWalker` streams are inner→outer (the immediate caller
of `walk()` first). Any `skip()/limit()`-based caller check over the stream
therefore selected the wrong frame.

This is why fixing Bug 1 immediately exposed a new near-universal blocker:
Lucene's `TestSecrets.ensureCaller()` (`lucene-core`) does
`StackWalker.getInstance().walk(s -> s.skip(2).limit(1)...)` to verify its
caller is in `org.apache.lucene.tests.*`; with frames reversed, `skip(2)`
landed on an unrelated frame from the *bottom* of the stack and the check
failed with `UnsupportedOperationException: Lucene TestSecrets can only be
used by the test-framework.`, thrown from
`org.apache.lucene.tests.util.LuceneTestCase.<clinit>` — hit by 96/99
classes in a curated worst-offender ES probe.

Minimal repro:

```java
public class Repro {
    static void level0() {
        var frames = StackWalker.getInstance().walk(s ->
            s.map(f -> f.getMethodName()).toList());
        System.out.println(frames); // real JDK: [level0, level1, level2, main]
    }
    static void level1() { level0(); }
    static void level2() { level1(); }
    public static void main(String[] a) { level2(); }
}
```
Before the fix, CratonVM printed `[main, level2, level1, level0]`.

### Fix

`AbstractStackWalker.callStackWalk`'s implementation
(`native-builtins/src/lang_stackwalker.rs`) already had the correct
ordering helper, `ordered_stack_walk_frames` (reverses + strips
VM-internal walker frames) — but that path is only reached when real-JDK
bytecode subclasses `AbstractStackWalker` directly, not for the common
`StackWalker.walk()`/`forEach()` call. Made `ordered_stack_walk_frames`
`pub(crate)` and reused it from `p59_sw_walk`/`p59_sw_for_each` instead of
the raw `capture_stack_trace` order.

Files: `native-builtins/src/lang_stackwalker.rs`, `native-builtins/src/phases_late.rs`.

## Verification

- Both repros above match real JDK 25 output after the fix.
- Full `native-builtins` unit suite: 2937/2938 pass (the one failure,
  `security_manager::policy::tests::wp68_substitution_dollar_escape_preserves_literal`,
  is a pre-existing env-coupled test unrelated to this change — untouched
  files).
- A 99-class probe drawn from the worst rows of the `20260709-082115` ES
  rerun (all previous `rc=139` crashes + blank-note crashes + the 2
  historical FAIL/PASS rows): before this fix, 100% hit the
  `Threads.<clinit>`/`EnumMap` `ClassCastException`; after Bug 1's fix,
  ~97% hit the `TestSecrets`/`StackWalker` blocker instead; after both
  fixes, the crash family and the `TestSecrets` blocker are both gone —
  every probed class reaches real JUnit execution (FAIL/HANG on actual
  test logic, not an early VM-level crash).
- A full 2649-class rerun of the same non-passed selection (see
  `docs/known-issues/elasticsearch-suite/ES-RUN-20260709-currentdev-nonpassed-rerun-120s-summary.md`
  for the updated numbers) confirms `rc=139` crashes are gone at scale.
  That rerun surfaced the *next* chokepoint — a separate, already-tracked
  `EnumSet.allOf`/`of` bug (see
  `docs/known-issues/enumset-of-broken-for-non-jdk-enums.md`) now hit via
  `Log4j Level.<clinit>` at logging bootstrap in nearly every class — not
  addressed by this change.

## Related

- `docs/known-issues/elasticsearch-suite/ES-RUN-20260709-currentdev-nonpassed-rerun-120s-summary.md`
- `docs/known-issues/elasticsearch-suite/ES-CRASH-FAMILY-20260709-currentdev-fail-probe-rc139.md`
- `docs/internal/elasticsearch-suite/ES-FAIL-FAMILY-20260709-foreign-memorylayout-varhandle-FIXED.md`
- `docs/known-issues/enumset-of-broken-for-non-jdk-enums.md` (next blocker, not fixed here)
