# Proposal: take the class manager out of the interpreter's `new` fast path

**Status: stage 1 landed (wave 3, lane L5), a first slice of stage 3 in wave 4, its JIT-helper half in wave 5 and the interpreter's compact-body skip in wave 16 — filed 2026-09-23 by interpreter round i1, lane L5; the design was revised in wave 2 (below). Needs the `allocOnly` / `AllocScaleProbe` measurement; stage 2 and the rest of stage 3 open.**

## Progress (wave 16)

**Landed — stage 3's compact half: a compact interpreted `new` stores no
default at all.** Re-derived from the current code: the interpreter's TLAB
path is no longer legacy-only (the compact TLAB shape has been default-on
since 2026-09-03, `compact_tlab_alloc_enabled`, for single-`ClassStore`
processes), so most warm interpreted allocations get a compact body, and the
recipe loop was writing `Long(0)` / `Float(0.0)` / `Double(0.0)` /
`Object(None)` into it through `VmHeap::set_field` — each one a header read,
the array and bounds checks, a compact-layout lookup and an atomic store of
zero bytes over zero bytes.

* `vm/src/runtime/interpreter/gc_and_alloc.rs::gc_alloc_object`: when the
  object came from `tlab_alloc_object` with a COMPACT reservation
  (`compact_body_holds_defaults`, read back from the reserved size by
  `shape_of_reserved`, the derivation that stamped the header), only the
  finalizer registration runs. Legacy bodies, shared-heap allocations
  (`alloc_object_shared`, whose shape this path does not see) and the locked
  fallback are unchanged. Kill switch: the `const`
  `COMPACT_BODY_DEFAULT_SKIP_ENABLED`.
* Why it is byte-for-byte the same heap: the TLAB chunk is zeroed when carved
  (the contract the wave-4/5 int skip already rests on), the compact header
  store zeroes its second word, and at every compact storage kind the
  recipe's default is written as zero bytes and zero bytes read as that
  default (narrow oops encode null as 0).
* `gc_alloc_object` now reads the recipe BEFORE allocating, so the first `new`
  of a class takes its class-manager read with no unrooted new object in hand.
* `vm/src/jit/alloc_class_cache.rs::ClassAllocInfo::build` refuses (stub in
  the chain, missing class, cyclic chain) in an allocation-free first pass
  and sizes both lists exactly in the second. A refused class is never
  published, so both readers re-ask on EVERY allocation of a stub-descended
  class in `--compatible`; that repeated refusal used to allocate and free the
  partial lists each time.
* Tests (`default_field_init_tests`):
  `compact_zero_body_already_holds_every_recipe_default` (every storage kind:
  the default is stored as zero and zero reads as the default),
  `only_a_compact_reservation_skips_the_default_stores`; the existing
  `gc_alloc_object_writes_every_default_and_registers_the_finalizable` still
  pins the legacy path and the finalizer registration.
* Bench: `tools/probes/interp/L5/NewAllocBench.java` under `--nojit`, three
  allocations per iteration (a two-level finalizer-free hierarchy with 9
  fields, and a 3-field pair), ns/alloc per round on stderr. Expect the warm
  `new` to lose one `set_field` per `long`/`float`/`double`/reference field
  (7 per `Sub`, 2 per `Base`, 3 per `Pair`) when the compact shape is in use;
  no change with `CRATONVM_COMPACT_TLAB_ALLOC=0`.

**Also found and fixed in the `new` path (not part of this proposal):**
bytecode `new` of an interface or abstract class allocated an instance
(JVMS 6.5 requires `InstantiationError`, before initialization). Fixed in
`opcodes.rs::op_new`, the JIT's CP-indexed helper
(`helpers.rs::jit_new_object_cp_body`) and the compile-time `new` row
(`jit_bridge.rs::jit_new_target_init_walk` answers `Pending`, so the site is
deferred to that helper), through one predicate
(`gc_and_alloc::new_instantiation_refusal`; compatibility stubs exempt).
Probe `tools/probes/interp/L5/NewAbstractInstantiationError.java`.

**Next stages:**

1. The JIT half of the compact skip: `jit_post_alloc_init`'s cached arm still
   writes `J`/`F`/`D` defaults into compact bodies, and the compile-time skip
   gate (`jit_new_site_flags` / `jit_post_alloc_init_is_noop`) keys on
   `prim_inits` without knowing the shape. The inline TLAB `new` knows at
   emission time whether it bumps a compact body; when it does, the helper
   call can be skipped for every finalizer-free class. A jit-crate change,
   measured with `L5IntOnlyAllocDefaults` and `NewAllocBench` with the JIT on.
2. Stage 2 (collapse `jit_init_primitive_fields` onto `ClassAllocInfo::build`)
   is still open.
3. The legacy-shape body image (multi-`ClassStore` processes and
   `CRATONVM_COMPACT_TLAB_ALLOC=0`) is still open; with the compact shape
   default-on it is worth less than when the proposal was written.
4. The per-`new` `maybe_gc` poll is now a visible share of the warm `new`:
   `interpreter-L2-proposal-refill-driven-gc-poll-after-new-FIXED-20260930.md`.
5. The `allocOnly` / `AllocScaleProbe` measurement is still owed; this page
   stays a proposal for the user to triage.

## Progress (wave 5)

**Landed — the JIT helper stores no int-family default either, and an
int-only class skips the call.**

* Zeroing proven on every body `jit_post_alloc_init` sees: the inline TLAB
  bump (both tiers, via `jit_post_tlab_init`) and
  `tlab_alloc_object_guarded_refill` carve from a TLAB chunk, and
  `gc/src/tlab.rs::Tlab::new`'s safety contract requires that chunk to be
  zeroed ("A chunk is zeroed when it is carved and the cursor only moves
  forward" — the `CRATONVM_DBG_DEADREF_STORE` tripwire checks exactly that);
  `VmHeap::try_alloc_object_full` hands its header initializer a zeroed span on
  every backend (`gen_heap.rs` young `try_alloc_young_initialized` and the old
  spill `OldGen::alloc`, `g1.rs::alloc_fallible_initialized`, `zgc/arena_tlab.rs::alloc_raw_tlab`).
  Zero bytes read `Int(0)` in the legacy shape (tag word 0) and are the int
  default in the tagless compact shape. `jit_bridge.rs::jit_new_site_flags`
  already relied on the same fact at compile time (only `J`/`F`/`D` set
  `has_prim_init`), so the inline bump's compile-time skip and the runtime
  helper disagreed about int-only classes; they now agree.
* `vm/src/jit/helpers.rs::jit_post_alloc_init`'s cached arm skips
  `PrimKind::Int` (as `gc_alloc_object` has since wave 4);
  `jit_post_alloc_init_is_noop` ignores `Int` entries, so `jit_post_tlab_init`
  (the IR tier's inline bump, and every inline bump under ZGC) skips the call
  for an int-only, finalizer-free class. The legacy fallback
  (`jit_init_primitive_fields`) is unchanged.
* Tests: `r9w3_vmhelpers3_post_alloc_init_skip::only_a_published_empty_recipe_skips_the_call`
  (extended: int-only recipe is a no-op; a `long` entry is not) and
  `::the_cached_arm_leaves_int_defaults_to_the_zeroed_body`.
* **Merge consistency re-checked** (task from the wave-5 brief): the merged
  `gc_alloc_object` kept the recipe arm over `dev`'s `init_new_instance` call.
  The two agree — same defaults (the recipe's `build` is the walk of
  `write_default_instance_fields`, split by kind; a class missing from the
  chain publishes no recipe and takes the walk), same single
  `register_finalizable` for a finalizable class, and neither can fail (the
  fallible, collecting part is the allocation itself, `alloc_object_shared`'s
  `collect_and_retry` ladder, which `native_alloc_collecting` is the
  VM-internal twin of). The uncached tail, however, was a verbatim inline copy
  of `init_new_instance`; it now calls it.
* **The `ifnull` exposure recorded in wave 4 below is closed.**
  `interpreter.rs::ref_operand_is_null` now reads `Int(0)` as null, the answer
  every other reader (and `if_acmpeq` against null, via `refs_equal`) already
  gave; test `interpreter::tests::ref_null_agrees_with_acmp_on_a_never_written_slot`.
  So a never-written reference slot reads as null through every interpreter
  path, and not writing `ref_inits` in the JIT helper stays a
  representation choice, not an observable one.

**Remaining:** unchanged from wave 4 (stage 2, the compact-body skip of stage
3, and the `allocOnly` / `AllocScaleProbe` measurement). The int skip makes
the ZGC/IR-tier helper-call count for int-only classes measurable directly
(`CRATONVM_DBG_JIT_ALLOC` filter, or a perf count on `jit_post_alloc_init`).

## Progress (wave 4)

**Landed — the warm interpreted `new` stores no int-family default.**
`gc_and_alloc.rs::gc_alloc_object`'s recipe loop skips `PrimKind::Int` entries
(`I B C S Z`): the body is zeroed when the TLAB chunk is carved and by the
shared-heap allocator, an all-zero LEGACY slot decodes as `Int(0)` (tag word
0; the G56-1 table on `init_primitive_fields`), and a COMPACT slot is tagless
(`types/src/field_layout.rs::read_compact_field`), so zero is that kind's
default in both shapes. `long`/`float`/`double` and references still need
their tag and are still written. The locked fallback walk is unchanged. One
`set_field` (a bounds check, an array check, a compact-layout lookup and the
store) saved per int-family instance field per `new`. Covered by
`default_field_init_tests::gc_alloc_object_writes_every_default_and_registers_the_finalizable`,
whose warm allocation now reads `base.i` back from the untouched zero.

**Decided — the JIT helper path's reference-slot defaults are NOT a
correctness defect; recorded, not changed.** Read in full: the inline TLAB
`new` skips `jit_post_alloc_init` when the recipe has no `prim_inits` and no
finalizer (`jit_post_alloc_init_is_noop`), and the helper's cached arm writes
`prim_inits` only, so a JIT-allocated object's never-written reference slots
are raw zero (`Int(0)` in the legacy shape) where the interpreter's are
`Object(None)`. Every reader that can see such a slot treats it as null:

* interpreter `getfield` rewrites `Int(0)`/`Long(0)` to `Object(None)`;
* compiled `getfield` reads the payload word only;
* reflection / descriptor-aware reads go through `coerce_field_value_for_slot`;
* `Unsafe.getReference*` passes the raw read through `recover_object_arg`,
  which maps `Int(0)` to null;
* CAS equality (`values_equal_for_cas`) equates the two; the collector's ref
  scan matches neither.

The residual exposure is the one `native-builtins/src/field_read.rs` already
documents as a native-side rule: a native that returns a raw
`get_field_by_name` of an unwritten reference field straight to bytecode can
hand back `Int(0)`, and the interpreter's `ifnull` (`ref_operand_is_null`)
does NOT read `Int(0)` as null. That is a bug in such a native, fixed by the
`ref_field` helpers there, and it applies equally to any carrier that skips
the default walk, not only to JIT allocations. Writing `ref_inits` in the
helper would narrow it for the classes the helper already runs for, but it
cannot reach the inline-skip population without widening the skip gate — the
measured 1.9 s → 6.0 s regression — and the helper's legacy fallback
(`jit_init_primitive_fields`) already writes them. So: no change without a
measurement. The one cleanup worth doing when someone measures: make
`jit_post_alloc_init`'s cached arm skip `PrimKind::Int` too (same argument as
above), and have `jit_post_alloc_init_is_noop` ignore `Int` entries, so an
int-only class takes the inline skip instead of a helper call that stores
nothing the zeroed body does not already say.

**Remaining:** stage 2 (collapse `jit_init_primitive_fields` onto
`ClassAllocInfo::build`), stage 3's compact-body skip (a compact-shaped
allocation needs no default store at all — every storage kind reads zero as its
default — but the interpreter's TLAB path allocates the legacy shape
deliberately; see `tlab_alloc_object_inner`), and the measurement.

## Progress (wave 3)

**Landed — the interpreter's warm `new` takes no class-manager lock**, by
extending `JitAllocClassCache` as wave 2 proposed rather than adding a second
table:

1. `vm/src/jit/alloc_class_cache.rs`: `ClassAllocInfo` gained `ref_inits`
   (instance indices of reference / array / malformed-descriptor slots, whose
   default is a WRITTEN `Object(None)`), and the recipe builder moved there as
   `ClassAllocInfo::build(store, class_id)` — the ONE builder, used by both
   readers, so their views of a class's defaults cannot drift.
   `PrimKind::of_descriptor` / `PrimKind::default_value` replace the two local
   matches.
2. `vm/src/runtime/interpreter/gc_and_alloc.rs` `gc_alloc_object` reads the
   recipe (`class_alloc_recipe`: `get`, else build under one class-manager
   read and `insert`) and writes `prim_inits` + `ref_inits` and registers the
   finalizer from it — the same writes the locked walk makes. The locked walk
   stays as the fallback (cache off via `CRATONVM_NO_JIT_ALLOC_CLASS_CACHE`,
   id outside the dense range, class or superclass missing from the store).
3. `vm/src/jit/helpers.rs` `jit_post_alloc_init` calls the shared builder and
   still writes ONLY `prim_inits`; `jit_post_alloc_init_is_noop` (the
   inline-TLAB skip gate) still keys on `prim_inits` + `has_finalizer` only.
   So the JIT's behaviour is byte-for-byte unchanged, including the known
   G56-1 divergence (JIT-allocated reference slots stay raw zero on the
   cached path); closing that is a measured JIT-side change, not taken here.
4. Weakness accepted, as wave 2 framed it: an interpreted allocation that read
   the recipe just before `invalidate` writes defaults at the OLD indices (the
   JIT already had this; `set_field` bounds-checks). `invalidate` is fired for
   redefinition, stub upgrade, every re-laid-out descriptor
   (`recompute_subclass_layouts_fires_jit_invalidate_hook_for_changed_descendants`)
   and class unload (`memory/gc.rs`), and every VM registers for the hook in
   `Vm::new`.
5. `init_primitive_fields` (VM-internal allocations) keeps the locked walk:
   it is not the per-bytecode path.

Test: `default_field_init_tests::gc_alloc_object_writes_every_default_and_registers_the_finalizable`
now also allocates warm (from the published recipe) and pins the recipe's
content and walk order.

**Remaining:** the measurement (`allocOnly` in `tools/probes/ThrowCost.java`,
`AllocScaleProbe` at 4 threads, interleaved with the previous binary); stage 2
(point `jit_init_primitive_fields`' legacy fallback at the recipe too, and
decide the JIT's reference-slot writes with a measurement); stage 3 (body-image
copy / skip loop for all-zero-correct compact bodies).

## Progress (wave 2)

**Not landed**, and the reason changes the design: the per-VM, lock-free,
`ClassId`-indexed descriptor table this proposal asks for ALREADY EXISTS —
`vm/src/jit/alloc_class_cache.rs` `JitAllocClassCache` (a field of the JIT
realm on `SharedVm`, `vm.jit.jit_alloc_class_cache`), holding
`ClassAllocInfo { has_finalizer, prim_inits }`, published with a CAS, retired
(not freed) on `invalidate`, which `jit_invalidate_adapter` calls on every
in-place layout change. Building a second table in `gc_and_alloc.rs` would be
the "duplicated logic in disagreeing copies" this round is removing, so stage 1
should extend that one instead:

1. Add the REFERENCE slots to the recipe (`ref_inits`, or one
   `inits: Box<[(u32, Value)]>` in slot order). Today `prim_inits` lists only
   primitive slots, so the JIT's cached path leaves reference slots as raw
   zero (`Int(0)`) while the interpreter and the JIT's own legacy fallback
   write `Object(None)` — the G56-1 divergence `jit_init_primitive_fields`'
   doc describes. The builder is in `vm/src/jit/helpers.rs`
   `jit_post_alloc_init`; the inline-TLAB skip gate
   (`jit_post_alloc_init_is_noop`, `bytecode_walk.rs`'s `skip_helper`) must keep
   keying on the PRIMITIVE list only, or the measured 1.9s→6.0s fast-path
   regression note in that file comes back.
2. `gc_alloc_object` (`gc_and_alloc.rs`) reads the recipe when
   `alloc_class_cache_enabled()` and falls back to today's locked walk on a
   miss (filling the recipe). That removes the class-manager read from every
   warm interpreted `new`.
3. Accept, or close, the recipe's one weakness the locked walk does not have:
   an allocation that read a recipe just before `invalidate` writes defaults at
   the OLD indices (bounded by `set_field`'s range check; documented on
   `JitAllocClassCache::invalidate`). The interpreter's locked walk sees a
   consistent layout today.

These are edits to three files outside lane L5 (`alloc_class_cache.rs`,
`helpers.rs`, `gc_and_alloc.rs`), with a JIT-visible behaviour change in step 1,
so they need a build and the `allocOnly` / `AllocScaleProbe` measurement; not
done blind.

Related wave-2 change in this area: `exceptions::create_exception_object_for_class`
gives a VM-minted throwable the same typed defaults a bytecode `new` gets
(`init_primitive_fields`). The `keep_unset` exclusion that path once needed for
Throwable's `cause` marker is gone (2026-09-24): the cause is now seeded to
`this` before the constructor runs (`seed_throwable_cause`), so a descriptor
built here needs no exclusion.

## What `new` costs after a site-cache hit

`op_new` (`vm/src/runtime/interpreter/opcodes.rs`) answers resolution, access
and initialization from the per-thread `ClassSiteCache` on a hit, then calls
`gc_alloc_object` (`vm/src/runtime/interpreter/gc_and_alloc.rs`), which:

1. plans the shape (`plan_tlab_object_shape_at` — a thread-local layout
   cache read) and bump-allocates from the TLAB;
2. takes the **class-manager read lock** (round i1 merged the two
   acquisitions into one), reads `has_finalizer`, then walks the whole
   superclass chain, and for every non-static field decodes the descriptor's
   first byte and calls `VmHeap::set_field` with the JVM default
   (`write_default_instance_fields`; `init_primitive_fields` is the
   public face);
3. registers the object with the reference processor if the class has a
   finalizer.

Step 2 is the problem. A `parking_lot` read acquisition is an atomic RMW on a
lock word every allocating thread shares, and it WAITS whenever any thread holds
the write lock — i.e. every class definition anywhere in the VM stalls every
interpreted `new`. The chain walk re-derives per allocation a fact that is
fixed per class. `set_field` goes through the `VmHeap` backend dispatch per
field. The JIT's slow path carries a byte-for-byte twin
(`vm/src/jit/helpers.rs::jit_init_primitive_fields`, named in the
`jvm_default_for_descriptor` doc).

HotSpot's `new`: TLAB bump, `memset` the body (or rely on pre-zeroed TLABs),
store the header. No class lock; finalizer registration is a flag on the klass
checked once.

## Design

Precompute, per class and once (at link/initialization — the point where
`ResolvedNewSite` entries become fillable), an allocation descriptor:

```rust
pub struct AllocDescriptor {
    num_fields: u32,
    has_finalizer: bool,
    /// Instance-slot default tags in slot order (0 = Int, 1 = Long,
    /// 2 = Float, 3 = Double, 4 = null) — or, better, a ready-made body image
    /// for the legacy shape and a flag for "compact body is all-zero-correct".
    defaults: Arc<[u8]>,
}
```

and carry an `Arc<AllocDescriptor>` (or an index into a per-VM table) in
`ResolvedNewSite`. `gc_alloc_object` then needs no class-manager access at all:

* compact shape: if every compact field's zero bit pattern already reads as
  its JVM default (true unless a legacy `Value` tag must be written), skip the
  field loop entirely — the TLAB is pre-zeroed;
* legacy shape: `copy_nonoverlapping` a cached body image (tag words
  pre-filled) instead of N `set_field` calls.

The G56-1 note on `init_primitive_fields` already nominates the related
"fill the body with the `Object(None)` pattern in the allocator" change as a
`gc/` item; this proposal is its interpreter-side half and does not need a
`gc/` change for the compact shape.

## Expected benefit

Removes one shared-lock RMW and an O(fields x hierarchy depth) loop from every
interpreted `new`, and removes the stall behind class definition (visible
during framework boot, when class loading and allocation overlap most).
`allocOnly` in `tools/probes/ThrowCost.java` (165-253 ns vs HotSpot 25-37) is
the direct measurement; `AllocScaleProbe` (4-thread scaling) is the contention
one.

## Staged plan

1. Build `AllocDescriptor` lazily per class (a `OnceLock` on the `Class`, or a
   per-VM `ClassId`-indexed table), fill it into `ResolvedNewSite`, and use it
   in `gc_alloc_object` when the caller has one (the site-cache hit path).
2. Point `jit_init_primitive_fields` at the same descriptor, collapsing the
   duplicate.
3. Body-image copy for the legacy shape; skip-loop for all-zero-correct
   compact bodies.

## Verification

`gc_alloc_object_writes_every_default_and_registers_the_finalizable` and the
rest of `default_field_init_tests` (gc_and_alloc.rs) pin the defaults and the
finalizer registration. Measure `allocOnly` and `AllocScaleProbe` interleaved
against the previous binary.

## Risk

Low-medium: redefinition that changes a class's field set must replace the
descriptor (it already replaces the `Class`), and the descriptor must be
per-VM (no process-global table keyed by `ClassId`).
