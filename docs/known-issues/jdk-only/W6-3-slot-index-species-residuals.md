# W6-3 — slot-index species: the last Lookup residual, and the Panama verdict

Status: one residual FIXED out-of-file (reported as a patch, see below), two
hardening fixes landed in-lane, the whole Panama/FFM surface verified SAFE with
a reason per site. Wave 6, lane W6-3.

Files in scope: `native-builtins/src/lookup_define.rs`,
`native-builtins/src/phases_late/foreign_ffm.rs`,
`native-builtins/src/panama.rs`, `native-builtins/src/panama_libffi.rs`.

## The species and the reachability rule

See `W4-4-slot-index-species-sweep.md` for both. Nothing here restates them;
this document only records what W6-3 measured.

One correction to the inventory: the campaign list said `alloc_lookup_for` was
"flagged three times, never fixed". It *was* fixed — by a wave-4 lane, for
`lookupClass` / `prevLookupClass` / `allowedModes`. The residual was next door,
in `classloader.rs::alloc_lookup`, and it is the `cachedProtectionDomain` half.

## The oracle

`javap -p java.lang.invoke.MethodHandles$Lookup`, JDK 25.0.3.9, instance fields
in declaration order (everything else the class declares is `static`):

```text
  0 lookupClass            private final    Class<?>
  1 prevLookupClass        private final    Class<?>
  2 allowedModes           private final    int
  3 cachedProtectionDomain private volatile java.security.ProtectionDomain
```

The synthetic layout agrees only on slot 0:

```text
  0 lookupClass | 1 allowedModes | 2 previousLookupClass | 3 lookupMode (dup)
```

Measured mode words on JDK 25 — `lookup()` = 95 (0x5F), `publicLookup()` = 32
(UNCONDITIONAL only, **not** PUBLIC), `lookup().in(String.class)` = 1,
`dropLookupMode(PRIVATE)` = 25. `0x5F` = PUBLIC|PRIVATE|PROTECTED|PACKAGE|
MODULE|ORIGINAL = `FULL_POWER_MODES`, and it is the correct value for the Lookup
that `defineHiddenClass` returns (`Lookup.defineClassAsLookup` →
`new Lookup(c, null, FULL_POWER_MODES)`).

## What `alloc_lookup_for` already had, and what W6-3 changed

Already correct on arrival: the by-name arm for `lookupClass`,
`prevLookupClass` and `allowedModes`; the slot-0 index write (slot 0 is
`lookupClass` in BOTH layouts); and — this is easy to misread as a gap —
**not writing `cachedProtectionDomain` at all**. That field is a lazy volatile
cache that `Lookup.lookupClassProtectionDomain()` fills on first use, so null is
its correct fresh value. "Finishing" it by writing something would reintroduce
the bug.

Two things W6-3 changed, both about the *negative* half:

1. **A positive class-side witness.** The discriminator was
   `matches!(get_field_by_name(obj, "prevLookupClass"), Value::Object(_))`. That
   works, but it is a value-shape test on a field that is null in the real
   layout and absent in the synthetic one — ~~the exact pair the `Int(0)`-for-
   absent convention exists to separate~~, and it cannot be checked from the
   value alone without already knowing the answer. `resolve_field_index_by_class_id`
   asks the CLASS instead. Kept as a disjunct with the old test, so the real arm
   fires whenever it fired before.

   > **CORRECTED 2026-08-07 — there is no `Int(0)`-for-absent convention in
   > production, and the correction strengthens this item rather than weakening
   > it.** `vm/src/vm/vm_exec.rs`'s `get_field_by_name` answers
   > `Value::Object(None)` for an absent field; `Int(0)` is what
   > `MockNativeContext` (`native-builtins/src/test_utils.rs`) answers, and what
   > a present-but-**unwritten** reference slot decodes as
   > (`docs/feature-designs/by-name-field-reads.md` §1). Absent and real-null are
   > therefore not merely hard to tell apart from the value — they are
   > **identical**, so the old discriminator took the real-layout arm on the
   > synthetic layout as well. The class-side witness is not hardening; it is the
   > only thing that answers the question at all. See
   > [§4 of *Natives over real JDK classes*](../../architecture/natives-over-real-jdk-classes.md).
2. **The by-name write is verified.** A `set_field_by_name` that silently did
   not land left a Lookup whose `allowedModes` reads back 0 — "no access at
   all" — and threw nothing. That *is* the original failure mode. If the named
   write does not read back, the synthetic indices are re-asserted.
   `classloader.rs::lk_set_modes` and `lang_invoke.rs::lk_write_allowed_modes`
   already do this; `alloc_lookup_for` was the one of the three that did not.

## `java.nio.ByteOrder` — a fresh instance of the species, in-lane

`javap -p java.nio.ByteOrder` (JDK 25): exactly one instance field,
`private final java.lang.String name`. `BIG_ENDIAN`, `LITTLE_ENDIAN` and
`NATIVE_ORDER` are all static.

`foreign_ffm.rs::p67_byte_order_object` allocated a `java/nio/ByteOrder` and
wrote `Int(0 | 1)` into slot 0 — an integer in a String reference slot. The GC
scans that slot as an oop, and real `ByteOrder.toString()` bytecode reads it as
the name. Fixed by additionally writing the real name by name when the class
declares `name` at a slot the object has; the flag write stays, because it is
the synthetic-stub layout and every existing reader
(`p67_layout_is_little`, `reflect_invoke::vh_byte_order_is_little`) falls back
to it.

## Panama / FFM — the verdict the prior lane asked for

`register_p67_foreign_memory` (`foreign_ffm.rs:1509`) is **not**
synthetic-mode-only: `lib.rs:9614` calls it from `register_essential_natives`,
alongside `panama::register_pe_raw_native_libraries` (:9598),
`register_pe_symbol_lookup` (:9604), `register_pe_linker_options` (:9608) and
`register_pe_memory_segment` (:9618). So door-3/door-4 reachability has to be
argued per triple, not waved away with "phase 67 is synthetic-only". It is not.

It is nonetheless **safe**, for a reason that is worth writing down because it
is not obvious:

* Every FFM instance triple these registrars own is declared on a
  `java.lang.foreign` **interface** (`MemoryLayout`, `GroupLayout`,
  `ValueLayout`, `MemorySegment`, `Arena`, `SymbolLookup`, `Linker$Option`).
  `vm_exec.rs:24391`'s guard drops an interface-declared non-static native
  unless one of the `force_ffm_*_interface_native` predicates matches.
* Those predicates test `class_name_for_override` — the **resolved declaring
  class**, not the constant-pool class. And `javap --module java.base -p`
  shows the real hierarchy declares every one of them concretely:
  `jdk.internal.foreign.layout.AbstractLayout` has `public final long
  byteSize()`, `byteAlignment()`, `name()`, `withName(String)`;
  `ValueLayouts$AbstractValueLayout` has `public final ByteOrder order()`,
  `withOrder(...)`, `carrier()`, `varHandle()`;
  `jdk.internal.foreign.ArenaImpl` has `scope()`/`close()`/`allocate(long,long)`.
  A real receiver therefore resolves to `AbstractLayout` / `AbstractValueLayout`
  / `ArenaImpl`, never to the interface, and the predicate does not fire.
* The FFM **entry points** are static interface methods (`Arena.ofConfined()`,
  `MemoryLayout.structLayout(...)`, `Linker.nativeLinker()`,
  `Linker.Option.critical(...)`, `SymbolLookup.loaderLookup()`), and the
  interface skip covers instance methods only — so the natives *do* answer
  there and hand back CratonVM-fabricated receivers. The object graph is
  fabricated end to end; it is not a mixed graph.

The one crack in "fabricated end to end" is that a native's *arguments* are not
filtered by the interface skip: a fabricated `Arena.allocate(MemoryLayout)`
could still be handed a real `ValueLayout.JAVA_INT`. It is closed one level up —
`foreign_ffm.rs` registers `java/lang/foreign/ValueLayout.<clinit>` (a static
interface method, so the native wins) inside `register_p67_foreign_memory`, and
that registrar is in the essential path. Every `JAVA_*` constant is therefore
CratonVM's own object in both modes, and no real layout instance exists to be
passed. `panama_libffi::read_layout_kind` is nonetheless already written for the
mixed case — `Int` slot 0 = synthetic kind word, `Long` slot 0 = a real layout's
`byteSize`, fall back to the class name. Note that its class-name table lists
the *interface* names (`java/lang/foreign/ValueLayout$OfInt`); a real receiver
would report `jdk/internal/foreign/layout/ValueLayouts$OfIntImpl` and land in
the `_ => LAYOUT_LONG` default. Latent, and only if that `<clinit>` override is
ever dropped.

Had one been reachable, the mismatch would have been real, and it is worth
recording what it *would* have been, because a future force-list addition would
turn it on silently:

| synthetic slot | CratonVM meaning | real `AbstractLayout`(+`AbstractValueLayout`) field |
| --- | --- | --- |
| 0 | byteSize (Long) | `byteSize` (long) — **coincidentally identical** |
| 1 | byteAlignment (Long) | `byteAlignment` (long) — **coincidentally identical** |
| 2 | little-endian flag (Int) | `name` (`Optional<String>` ref) |
| 3 | name value (ref) | `carrier` (`Class<?>` ref) |

So `p67_layout_byte_size`'s slot-0 read is right for the wrong reason, and
`p67_layout_name_value`'s slot-3 read would answer a layout's *carrier class*
as its *name*. The first two slots matching is luck, not design; do not lean
on it.

The two FFM sites that genuinely do see real receivers are already correct:

* `jdk/internal/foreign/MemorySessionImpl` (force-dispatched, and the class is
  real) — `p67_session_is_real` + `p67_session_delegate` hand a real
  `ConfinedSession`/`SharedSession` back to its own bytecode, and
  `p67_session_modelled` gates every slot access on `slot 0 is an Int`, which a
  real session (slot 0 = `resourceList`, a reference) can never satisfy. Real
  layout, from `javap`: `resourceList`(0) `owner`(1) `state`(2)
  `acquireCount`(3) — a full permutation of CratonVM's `state`(0)
  `acquireCount`(1) `owner`(2) `actions`(3), so the guard is doing real work.
* `jdk/internal/loader/RawNativeLibraries.load0` — door 1 (real `ACC_NATIVE`),
  so the receiver is always real. It writes `handle` **by name**. Correct:
  `RawNativeLibraryImpl` is `name`(0, String) `handle`(1, long), so the index
  write it does not do would have clobbered `name`.

## Reported, not edited (other lanes' files)

* `classloader.rs::alloc_lookup` — the residual. See the lane report for the
  exact patch.
* `phases_late/reflect_invoke.rs::build_string_set` — builds a `HashSet` as
  `{0 = String[], 1 = size, 2 = capacity}`. A real `java.util.HashSet` has
  `map` as its **only** instance field. Identical shape to the confirmed
  `Security.getAlgorithms` bug; inert only because its sole caller is
  synthetic-mode-only. Unchanged from the W4-4 finding.
* `classloader.rs::lk_in_method` / `lk_drop_lookup_mode` — both read
  `get_field(this, LK_ALLOWED_MODES /* 1 */)` by index, while the by-name
  reader `lk_modes_of` sits ten lines away in the same file and is not used.
  On a real Lookup slot 1 is `prevLookupClass` (a reference), so the `Int` match
  fails and the code takes its default: `LK_PUBLIC` for `in`, `LK_FULL_POWER`
  for `dropLookupMode`. Measured JDK 25 answers are 1 and 25; the fallback gives
  1 (right by accident) and 93 (wrong). Reachability is *not* established —
  both JDK methods are concrete `Code` and neither is force-listed — so this is
  filed as latent, not as a bug.
