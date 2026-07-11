# `EnumSet.of(...)`/`EnumSet.allOf(Class)` broken for non-JDK enums — FIXED

Status: FIXED (dev, commit `<COMMIT_HASH>`)

Date observed: 2026-07-09
Date fixed: 2026-07-09

Previously tracked at `docs/known-issues/enumset-of-broken-for-non-jdk-enums.md`
(this file replaces it).

## Symptom

In real-JDK mode, `EnumSet.of(...)`/`EnumSet.allOf(Class)` on any
non-bootstrap-loaded enum (application/framework enums — `StandardLevel`,
`DispatcherType`, or a trivial standalone `enum Color`) silently returned
a broken/empty set: `size()` returned 0, `iterator()` returned `null`
(NPE on the first `hasNext()`), and `toString()` printed the default
`Object@xx` format instead of `[VALUE1, VALUE2]`. No exception was ever
thrown by `of()`/`allOf()` themselves. This was the dominant blocker for
the real-JDK-mode Elasticsearch suite (essentially the whole 2648-class
non-passed selection failed through `org.apache.logging.log4j.Level.<clinit>`
→ `StandardLevel.getStandardLevel`) and blocked Tomcat's
`WsServerContainer` constructor (`EnumSet.of(DispatcherType.REQUEST,
DispatcherType.FORWARD)`), which in turn blocked the
`docs/known-issues/tomcat-08-07/wsremoteendpoint-close-delay-near-deadlock.md`
investigation from ever reaching Tomcat startup.

## Root cause (confirmed empirically via targeted debug instrumentation)

Neither of the two hypotheses in the original doc was quite right in
isolation — the real mechanism, confirmed by instrumenting
`try_jdk_enum_set_of_elements` and the synthetic `ArrayList` backing-list
helpers directly:

`native_es_none_of` (backing **both** `EnumSet.noneOf` and `EnumSet.allOf`
— they were registered to the same native, per a pre-existing "simplified"
comment) unconditionally allocates its return value via
`alloc_concurrent_synthetic(ctx, "java/util/EnumSet", 2)`. In real-JDK
mode, once the real (non-stub) `java.util.EnumSet` class is loaded,
`alloc_concurrent_synthetic` reuses **that real, abstract class's
`ClassId`** for the allocated object — producing an object whose runtime
class is literally the *abstract* `java.util.EnumSet` itself (impossible
to construct in real Java, but our raw `alloc_object` bypasses the
instantiability check). The native then pokes the object's two field
slots with the *synthetic* 2-field bridge semantics (field 0 = backing
`ArrayList`, field 1 = element type), which do not correspond to real
`EnumSet`'s actual fields (`elementType`, `universe`).

This mismatched hybrid object breaks in two independent, compounding ways:

1. **Virtual dispatch of `add()` lands on real `AbstractCollection.add()`,
   which unconditionally throws `UnsupportedOperationException`.**
   `EnumSet.add()` is abstract (no bytecode) on the real abstract
   `EnumSet` class itself, so `invoke_or_native`'s `has_real` check (which
   walks the class hierarchy looking for a concrete, `Code`-bearing
   method) skips past it and finds `AbstractCollection.add()` — a real,
   concrete method whose entire body is `throw new
   UnsupportedOperationException()`. Because a concrete ancestor
   implementation was found, dispatch treats the SyntheticStub native as
   "protected" (real bytecode should win) and never calls
   `native_es_add`, so `add()` always fails for objects allocated this
   way. `try_jdk_enum_set_of_elements` (the `of(...)` helper that tries
   to build a "real" `EnumSet` via `noneOf` + `add`) correctly treats this
   `add()` failure as a hard error and falls back to its own local
   synthetic-bridge construction — so `of(...)` always ends up on the
   *same* synthetic bridge as `allOf`/`noneOf`, just via a slightly
   different path.
2. **The synthetic bridge's own `ArrayList` backing-list helper writes
   fields at indices that don't match where the readers look.**
   `enum_set_backing_list` defensively writes the backing list's
   `elementData`/`size` via both hardcoded indices 0/1 *and*
   `resolve_field_index`-based real indices (to cover both synthetic- and
   real-JDK-mode `ArrayList` layouts) — but the reader side
   (`native_es_size`, `native_es_iterator`, `native_es_is_empty`,
   `native_es_to_array`, `native_es_contains`, `native_es_remove`,
   `native_es_clear`) all read via the **hardcoded** indices 0/1 only. In
   real-JDK mode, real `java.util.ArrayList`'s actual layout is
   `[0]=modCount (inherited from AbstractList), [1]=elementData,
   [2]=size`, confirmed via debug instrumentation
   (`resolved elementData idx=Some(1) size idx=Some(2)`). Reading the
   hardcoded index 1 for "size" therefore read the `elementData` object
   reference instead (which doesn't match the `Value::Int` pattern),
   silently defaulting to 0 — and reading hardcoded index 0 for "data"
   read `modCount` (an `Int`), which doesn't match `Value::Object`,
   yielding a `null` iterator. This is the same class of defect as an
   already-documented `Vector`/`Stack` field-layout bug in the shared
   `native-collections` crate (`al_slots_for`) — but the EnumSet natives
   in `native-builtins` had their own hand-rolled, unfixed copy of the
   same hardcoded-index mistake.

## Fix

Rather than patch every individual reader/writer with resolved indices
(viable, but leaves the deeper issue #1 above — the bogus abstract-class
receiver identity — unaddressed for any future method added to the
synthetic surface), the fix drops the **entire** `java/util/EnumSet`
native surface in real-JDK mode (`NativeMethodRegistry::register` in
`native-api/src/registry.rs`, gated by the existing
`drop_real_layout_synthetic` flag that real-JDK-mode `vm_init.rs` already
sets for the analogous `StringJoiner` real-layout problem). With no
native intercepting `noneOf`/`allOf`/`of`/`add`/`size`/`iterator`/etc.,
real bytecode runs uninterrupted end-to-end: `EnumSet.noneOf` correctly
allocates a concrete `RegularEnumSet`/`JumboEnumSet` (not the abstract
`EnumSet` class), and all subsequent method calls dispatch to that
concrete class's own real bytecode, which is internally consistent by
construction.

This relies on `Class.getEnumConstantsShared()`
(`native_class_get_enum_constants` in `native-builtins/src/lang_class.rs`)
correctly resolving `$VALUES` for non-bootstrap-loaded enums, which turned
out to already work correctly for ordinary (including non-bootstrap)
enums — confirming the original doc's Hypothesis 1 was a red herring for
the reported symptom; it only manifested as a narrower, separate,
still-open issue for nested enums initialized *during* their enclosing
class's own `<clinit>` (see
`docs/known-issues/stackwalker-option-enum-constants-null-blocks-es-suite.md`,
newly discovered — was masked by this bug and is now the new dominant ES
suite blocker; NOT fixed this session, out of scope for this fix).

A related, independently-diagnosed bug was fixed in the same change:
`vm/src/vm/vm_init.rs` registered a synthetic
`ScheduledThreadPoolExecutor.<init>(int, ThreadFactory)` native that only
poked two field slots (`corePoolSize`, and a hardcoded `0`), never
initializing the real inherited `ThreadPoolExecutor` state (`ctl`,
`workQueue`, `mainLock`, `workers`). This left `getQueue()` returning
`null` on real `ScheduledThreadPoolExecutor` instances in real-JDK mode,
breaking Tomcat's `ContainerBase.scheduleWithFixedDelay` →
`delayedExecute` path (and independently, `TestSwallowAbortedUploads`,
tracked in a separate concurrent investigation of
`docs/internal/tomcat-08-07/swallowabortedupploads-unexpected-socketexception-RESOLVED.md`
— this fix resolves that blocker too). Fixed the same way: the synthetic
override and its `Executors.newScheduledThreadPool`/
`newSingleThreadScheduledExecutor` factory overrides are dropped in
real-JDK mode so the real constructors/factories run.

## Verification

- Standalone repro (`EnumSetProbe.java`, top-level 3-value enum): before
  the fix, `EnumSet.allOf(Color.class)` / `EnumSet.of(RED, GREEN)` printed
  `Object@xx size=0`, iterator `null`, NPE on for-each. After the fix:
  ```
  all=[RED, GREEN, BLUE] size=3
  some=[RED, GREEN] size=2
  some class=class java.util.RegularEnumSet
  all class=class java.util.RegularEnumSet
  iterating some:
    elem=RED
    elem=GREEN
  ```
  (Note the receiver class is now the real, concrete `RegularEnumSet` —
  confirming the abstract-class-receiver bug is gone.)
- `cargo test -p cratonvm-native-api real_layout_mode_drops_enumset_native_surface`
  — new test, passes; asserts the registry drops `EnumSet`/
  `ScheduledThreadPoolExecutor`/`Executors.newScheduledThreadPool` natives
  under `drop_real_layout_synthetic` while leaving unrelated classes
  (`HashSet`) untouched.
- `cargo test -p cratonvm-native-builtins`: 2937 passed, 1 failed (the
  known pre-existing, unrelated `security_manager::policy::tests::
  wp68_substitution_dollar_escape_preserves_literal` failure), 6 ignored —
  matches the documented pre-fix baseline exactly.
- Tomcat end-to-end repro (the original blocking consumer):
  `TestWsRemoteEndpointImplServerDeadlock` now starts Tomcat successfully
  and reaches/runs the actual close-handshake test logic (previously never
  got past `WsServerContainer`'s constructor). See
  `docs/known-issues/tomcat-08-07/wsremoteendpoint-close-delay-near-deadlock.md`
  for the continuation of that investigation — the close-delay/hang itself
  is a separate, still-open issue.
- ES suite spot check (2 sampled classes,
  `org.elasticsearch.index.mapper.UpdateMappingTests` and
  `org.elasticsearch.index.query.CombineIntervalsSourceProviderTests`):
  the original `Level.<clinit>`/`StandardLevel`/`EnumSet` failure is gone
  in both; a **different**, newly-exposed blocker
  (`StackWalker$Option` — see the new doc referenced above) now stops
  them earlier in bootstrap. Net effect for the ES suite specifically is
  not yet a full unblock — tracked as a new, separate issue.
