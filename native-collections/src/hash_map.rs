// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

// HashMap — field 0 = Object[] buckets, field 1 = Int size, field 2 = Int cap
// HashMap$Node — MATCHES the real JDK layout: hash=0, key=1, value=2, next=3.
//
// We deliberately use the REAL JDK field order (not the historical synthetic
// key=0/value=1/hash=2 order) so that real-JDK bytecode that walks `this.table[]`
// directly via `getfield HashMap$Node.{key,value}` — e.g. `HashMap.writeObject`
// → `internalWriteEntries` during Java serialization, which is NOT on the
// force-native override list — reads the correct slots. With the old synthetic
// order, `getfield key` (real slot 1) read our `value`, and `getfield value`
// (real slot 2) read our `hash` Int which the L-descriptor coerced to null,
// serializing `{name=execute}` as `{execute=null}` and desyncing the Gradle
// worker's message stream. Nodes are still allocated as `ClassId(0)` (see
// `map_alloc_node`): slot 0 now holds `Int(hash)`, so there is no Object→int
// descriptor coercion to worry about, and the layout-sniff in `get_node_key`/
// `get_node_value` (Object@0 = legacy, Int@0 = JDK) routes our nodes through
// the JDK branch automatically.
// ===========================================================================

pub(crate) const MAP_FIELD_BUCKETS: usize = 0;
pub(crate) const MAP_FIELD_SIZE: usize = 1;
pub(crate) const MAP_FIELD_CAPACITY: usize = 2;
pub(crate) const MAP_NUM_FIELDS: usize = 3;
pub(crate) const MAP_DEFAULT_CAPACITY: usize = 16;

pub(crate) const NODE_FIELD_HASH: usize = 0;
pub(crate) const NODE_FIELD_KEY: usize = 1;
pub(crate) const NODE_FIELD_VALUE: usize = 2;
pub(crate) const NODE_FIELD_NEXT: usize = 3;
pub(crate) const NODE_NUM_FIELDS: usize = 4;

/// Extract HashMap state: (buckets, size, capacity).
pub(crate) fn map_state(ctx: &dyn NativeContext, this: ObjectRef) -> (Option<ObjectRef>, i32, i32) {
    // See through CratonVM's unmodifiable wrapper views. When a generic
    // `java/util/Map` interface native (registered to `native_map_*`) is
    // dispatched on a `cratonvm/internal/UnmodifiableMap` receiver — which
    // happens when the wrapper class does not itself register the invoked
    // method — slot 0 of the wrapper is the *backing map* ObjectRef, not a
    // bucket array. Without this unwrap, `map_state` reads a non-array
    // Object at slot 0, fires `[MAP-STATE-GUARD]`, and (in `native_map_put`)
    // `map_resize` would clobber the wrapper's backing pointer; the warning
    // floods because Keycloak 26's config code repeatedly calls `get` /
    // `containsKey` / `size` on `Map.of(...)` / `Collections.unmodifiableMap`
    // results. Reading map state from the backing is always correct because
    // every wrapper mutator throws — these are read-only callers.
    let this = unwrap_unmod(ctx, this);
    let nf = ctx.object_num_fields(this);
    if nf == 0 {
        return (None, 0, MAP_DEFAULT_CAPACITY as i32);
    }

    // The receiver's OWN bucket slot: its `table` when it has one, the
    // fabricated model's slot 0 otherwise. This used to read slot 0
    // unconditionally and fall back to the resolved `table` only when slot 0
    // held no array — which worked because the writers ALSO wrote slot 0, i.e.
    // `AbstractMap.keySet` on a real `HashMap`. See `map_buckets_slot`.
    let buckets_slot = map_buckets_slot(ctx, this);
    let buckets_primary = match ctx.get_field(this, buckets_slot) {
        Value::Object(Some(arr)) => {
            if ctx.heap_kind_of(arr) == ObjectKind::Array {
                Some(arr)
            } else {
                // Diagnostic only — the receiver is not bucket-backed at all
                // (a `Collections$EmptyMap`, a wrapper, a JDK-constructed map
                // whose table is still null). Gate the probe behind the cached
                // HM-trace flag so the common path does no stderr I/O.
                if dbg_hm_trace() {
                    let map_cls = ctx
                        .class_name_of_id(ctx.class_id_of_object(this))
                        .unwrap_or_default();
                    let slot0_cls = ctx
                        .class_name_of_id(ctx.class_id_of_object(arr))
                        .unwrap_or_default();
                    eprintln!(
                        "[MAP-STATE-GUARD] non-array buckets slot{}: map={:?}({}) held={:?}({}) \
                         — receiver was not a bucket-backed HashMap; treating buckets as absent",
                        buckets_slot, this, map_cls, arr, slot0_cls
                    );
                }
                None
            }
        }
        _ => None,
    };
    // No slot-0 secondary. A first cut kept one, for "a map some older path
    // populated at the model slot", and the widened census answered that
    // empirically: 50 reads across the probe set, every one of them returning
    // `Int(0)` — an unset `AbstractMap.keySet` — and never an array. It was a
    // read of the wrong field that could not succeed, so it is gone; the one
    // writer of a bucket table is `publish_map_table`, which writes exactly
    // the slot `map_buckets_slot` reads.
    // `uses_native_hashtable_layout` is no longer consulted here: it existed to
    // stop the slot-0 secondary from firing on a `Hashtable` (whose slot 0 IS
    // its `table`, so the secondary would have re-read the primary). With the
    // secondary gone, `map_buckets_slot` already answers 0 for that family for
    // the right reason.
    let buckets = buckets_primary;
    // Read the entry count from the slot the receiver declares for it — the
    // same question `set_map_size` asks, through the same function, so the two
    // cannot drift. See [`map_size_slot`].
    //
    // The long history this replaces is worth keeping in one line: resolving
    // "size" against a hard-coded `java/util/HashMap` landed on `Hashtable`'s
    // `modCount` for a `Hashtable` receiver, so `set_map_size` and the next
    // `bump_map_mod_count` wrote the same physical slot and DOUBLED the size on
    // every put (ecj's `CompilationResult.getClassFiles()` then null-padded its
    // array and NPE'd). Resolving on the receiver's own class fixed that; this
    // change finishes the job by making the fallback slot a property of the
    // receiver rather than a constant.
    let size_by_name = {
        let slot = map_size_slot(ctx, this);
        Some(ctx.get_field(this, slot))
    };
    // spring-bug-09: bound the slot-2 fallbacks below. `map_state` is invoked on
    // any Map-typed receiver, including non-synthetic JDK maps with fewer than 3
    // slots (e.g. `java/util/Collections$EmptyMap`, which has only AbstractMap's
    // 2 reference slots). Reading absolute slot 2 (`MAP_FIELD_CAPACITY`) on such a
    // receiver is out of bounds: the GC guard drops the read in-process, but it
    // fires on a hot path (SpEL `ReflectiveIndexAccessor` map indexing) and the
    // batch JVM eventually SIGSEGVs. Probe slot 2 only when the receiver has it.
    let size = match size_by_name {
        Some(Value::Int(s)) => s,
        _ => match ctx.get_field(this, MAP_FIELD_SIZE) {
            Value::Int(s) => s, // legacy: slot 1 is Int
            _ if nf > 2 => match ctx.get_field(this, 2) {
                // ancient fallback
                Value::Int(s) => s,
                _ => 0,
            },
            _ => 0,
        },
    };
    // S111r26: Use bucket array length as the true capacity.  When
    // make_hashset_with_elements uses the real JDK HashMap field layout
    // (table=0, entrySet=1, size=2, ...), MAP_FIELD_CAPACITY (slot 2)
    // holds the element count, not the bucket count.  Reading the array
    // length of the bucket array always gives the correct value regardless
    // of which layout was used (legacy 3-field or real JDK).
    let cap = if let Some(b) = buckets {
        let arr_len = ctx.array_length(b) as i32;
        if arr_len > 0 {
            arr_len
        } else {
            MAP_DEFAULT_CAPACITY as i32
        }
    } else if nf > MAP_FIELD_CAPACITY {
        match ctx.get_field(this, MAP_FIELD_CAPACITY) {
            Value::Int(c) if c > 0 => c,
            // NO TABLE YET. On a real-JDK `HashMap` layout absolute slot 2 IS
            // `table`, so `MAP_FIELD_CAPACITY` cannot hold the requested
            // capacity for a map whose table has not been allocated -- there is
            // nowhere else in the object for it to live.
            //
            // HotSpot has the same problem and solves it by REUSING
            // `threshold`: while `table == null` that field holds
            // `tableSizeFor(initialCapacity)` -- the size the first `resize()`
            // will allocate -- and only afterwards does it mean the 0.75 load
            // threshold. `table == null` is the discriminator, which is exactly
            // the branch this is. Reading it here is what lets
            // `new HashMap<>(64)` still materialise 64 buckets on its first put
            // rather than collapsing to the default 16.
            //
            // Cost is confined to this arm: a receiver whose slot 2 already
            // holds a positive Int never reaches it, and one with a live table
            // never reaches this branch at all.
            _ => match ctx
                .resolve_field_index_by_class_id(ctx.class_id_of_object(this), "threshold")
                .filter(|s| *s < nf)
                .map(|s| ctx.get_field(this, s))
            {
                Some(Value::Int(t)) if t > 0 => t,
                _ => MAP_DEFAULT_CAPACITY as i32,
            },
        }
    } else {
        // spring-bug-09: receiver has no slot-2 capacity field (e.g. EmptyMap).
        MAP_DEFAULT_CAPACITY as i32
    };
    (buckets, size, cap)
}

/// The slot this receiver keeps its entry count in: its own `size` (the
/// `HashMap` family) or `count` (the `Hashtable` family, which `Properties`
/// inherits), and only otherwise the fabricated model's slot 1.
///
/// Read and write go through this one function on purpose. They used to
/// disagree in shape — the writer wrote BOTH slot 1 and a name-resolved slot,
/// the reader preferred the name-resolved one and fell back to slot 1 — which
/// is survivable only because slot 1 on a real `HashMap` is
/// `AbstractMap.values`, a reference, so the `Int` coerced to `null` and read
/// back as "absent". That is the 8,000-hit benign census row: benign in
/// outcome, still a write to a field the native did not mean.
///
/// Resolving `count` by NAME on the receiver also removes the last fixed-class
/// lookup here: `Properties` has no `size`, and its `count` is inherited from
/// `Hashtable` at absolute slot 1 — the old code got that right by writing the
/// raw slot, which is right for exactly the layouts where it is an accident.
pub(crate) fn map_size_slot(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    let class_id = ctx.class_id_of_object(this);
    let nf = ctx.object_num_fields(this);
    for name in ["size", "count"] {
        if let Some(slot) = ctx.resolve_field_index_by_class_id(class_id, name) {
            if slot < nf {
                return slot;
            }
        }
    }
    MAP_FIELD_SIZE
}

/// Write the map's entry count to the slot the receiver actually declares for
/// it — see [`map_size_slot`].
pub(crate) fn set_map_size(ctx: &mut dyn NativeContext, this: ObjectRef, size: i32) {
    let slot = map_size_slot(ctx, this);
    ctx.set_field(this, slot, Value::Int(size));
}

/// S111r28 helper: best-effort write of a JDK-named HashMap field to the
/// resolved slot, but only when the slot is within the allocated field
/// count of `this`. Synthetic backing maps (e.g. inside HashSet) are
/// allocated with `MAP_NUM_FIELDS = 3` regardless of the real-JDK class
/// metadata, so naively writing to slot 4+ would land in undefined memory.
pub(crate) fn try_set_jdk_map_field(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    field_name: &str,
    value: Value,
) {
    // JDK-ONLY-LAYOUT: resolve the name on the RECEIVER's class, not on a
    // hard-coded `java/util/HashMap`.
    //
    // This was `resolve_field_index("java/util/HashMap", field_name)` and then
    // wrote that index into `this` — whatever class `this` actually was. For a
    // receiver that is not a `HashMap`, the index names a *different field*,
    // and the `slot < object_num_fields` bound below does not help: it stops an
    // out-of-range write, not a wrong-field one. That is the shape of guard
    // that looks protective and is not.
    //
    // Measured 2026-08-04 with `CRATONVM_DBG=overlay,overlay-all` on
    // `new Properties()`: `Float(0.75)` — a `loadFactor` — landed on
    // `Properties` slot 7, which the real class declares as a **reference**,
    // and `Int(0)` / `Int(12)` landed on slots 5 and 6, likewise references. On
    // a real `java.util.Properties` those slots are `defaults` / `map`, so each
    // write both destroyed a live reference and stored the value nowhere useful.
    //
    // `resolve_field_index_by_class_id` walks the receiver's own hierarchy, so
    // `loadFactor` on a `Properties` resolves through `Hashtable` to its true
    // absolute slot; its doc comment already recommends it over the name-based
    // form in exactly this situation, because the caller holds the object. When
    // the receiver's class does not declare the field the lookup returns `None`
    // and nothing is written — the correct outcome, and strictly better than
    // writing whatever field happens to sit at a `HashMap` index.
    let class_id = ctx.class_id_of_object(this);
    if let Some(slot) = ctx.resolve_field_index_by_class_id(class_id, field_name) {
        if slot < ctx.object_num_fields(this) {
            ctx.set_field(this, slot, value);
        }
    }
}

/// The receiver's OWN bucket-array field (`table`), resolved by NAME against
/// the class the receiver actually is, and bounded by its allocated slot count.
///
/// JDK-ONLY-LAYOUT (L2): every caller of this used to ask
/// `resolve_field_index("java/util/HashMap", "table")` — a fixed class name —
/// and then write that index into whatever `this` happened to be. On a
/// `java.util.Properties` (which is deliberately excluded from
/// `CF_HASHTABLE_LAYOUT`, because JDK 25 backs it with a side
/// `ConcurrentHashMap`) `HashMap.table` is index 2, and index 2 on a real
/// `Properties` is the inherited `Hashtable.threshold` — an `int`. That is the
/// surviving `Properties` slot-2 row of the overlay census (`Object` written
/// over an `I`): the bucket array both destroyed a live field and was stored
/// where nothing reads it. The receiver's own `table` (inherited from
/// `Hashtable`) is index 0, which slot 0 already holds.
///
/// `None` is the answer for a receiver that has no real `table` field at all —
/// CratonVM's own fabricated map layouts (`cratonvm/util/MapViewBacking`, the
/// bare `ClassId(0)` CHM segments, a synthetic-mode `HashMap` stub whose fields
/// are `_f0.._fN`). That is the layout predicate this file needs, and it asks
/// for a field NAME the real class declares rather than counting slots: a
/// count test cannot separate the two layouts, because the allocators size
/// every synthetic map to at least `MAP_NUM_FIELDS` and a real one to at least
/// its real field count. (`object_num_fields(x) >= N` was exactly the inert
/// `VarHandle` guard that nearly shipped on 2026-08-04.)
///
/// Caveat, shared with the `try_set_jdk_map_field` conversion that landed the
/// same day: a subclass that declares its own field literally called `table`
/// shadows the inherited one, and this resolves to the subclass's. That is the
/// receiver's own answer to "where is your `table`", which is the best a name
/// lookup can do; it is still strictly better than answering with a fixed
/// class's index, which is right for exactly one receiver class.
pub(crate) fn receiver_table_slot(ctx: &dyn NativeContext, this: ObjectRef) -> Option<usize> {
    let class_id = ctx.class_id_of_object(this);
    ctx.resolve_field_index_by_class_id(class_id, "table")
        .filter(|&slot| slot < ctx.object_num_fields(this))
}

/// The slot this receiver's bucket array lives in: its own `table` field when
/// it has one, and only otherwise the fabricated model's slot 0.
///
/// This is the whole of the "model slots on a real layout" fix. L2 made the
/// WRITES land on the receiver's own `table`, but kept writing slot 0 as well,
/// because every reader still went there. On a real `java.util.HashMap` slot 0
/// is `AbstractMap.keySet` — a `Set`-typed field holding a bucket array.
///
/// No census could see it: both are references, so the cross-type predicate is
/// blind by construction, and L4's shadow-layout diff can only say the model
/// and the image disagree, not what the slot holds at runtime. What sees it is
/// `probes/MapModelSlotProbe`, reading the field reflectively. Before this
/// change, against HotSpot 25.0.3+9:
///
/// ```text
///   HotSpot   fresh/filled/copyCtor/sizedCtor  keySet=null
///   CratonVM  fresh/filled/copyCtor/sizedCtor  keySet=ARRAY[Object]
/// ```
///
/// and it stayed `ARRAY[Object]` after `keySet()` was called, where HotSpot
/// caches a `HashMap$KeySet` there. That is the failure mode, not a tidiness
/// argument: real `HashMap.keySet()` bytecode is `if (ks == null) ks = new
/// KeySet(); return ks`, so a non-null bucket array short-circuits the lazy
/// init and returns an `Object[]` to a caller that will `invokeinterface
/// Set.iterator()` on it. It does not fault today only because the natives
/// shadow every reader — the same conditional safety the `VarHandle` row had,
/// and item 3/7 (L11) is in the business of removing exactly that shadow.
///
/// Cost: one `resolve_field_index_by_class_id` per call on a path that runs
/// per map operation. Deliberately NOT memoized until measured — see the A/B
/// in the commit message.
pub(crate) fn map_buckets_slot(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    receiver_table_slot(ctx, this).unwrap_or(MAP_FIELD_BUCKETS)
}

/// Write the JVMS §2.3 default — `null`, WRITTEN — into the reference-typed
/// fields of a map object THIS CRATE allocated itself, so a slot that is
/// legitimately empty until first use reads back as `null` and not as `Int(0)`.
///
/// # This is the whole of the 2026-09-01 descriptor-coercion census row
///
/// A stock `cratonvm Hello` reported `total=2740
/// primitive-into-reference[read=2740]`, all 2740 of them at ONE locator
/// (`class_id=64 index=2 descriptor=[`) — `java.util.HashMap.table`, read by
/// [`map_state`] through [`map_buckets_slot`] on the `HashSet.add` boot path.
/// The access-kind breakdown is the finding, and it is easy to read past:
/// `read=2740`, **`store=0`**. Not one `set_field` in this crate — or anywhere
/// else in the VM on that path — ever wrote a primitive at that slot. What put
/// the `Int(0)` there was the ALLOCATOR.
///
/// `Value::Object` carries a `NonNull` niche, so the all-zero cell
/// `alloc_zeroed` leaves decodes as `Value::Int(0)` and NOT as
/// `Value::Object(None)`. `gc::heap::alloc_object_with_descriptors` and
/// `interpreter::gc_and_alloc::init_primitive_fields` both exist to write the
/// defaults explicitly for exactly that reason, and the test
/// `zero_memory_does_not_decode_as_null_which_is_why_the_write_exists` pins it.
/// The interpreter's `new` opcode goes through `init_primitive_fields`; the
/// `NativeContext::alloc_object` this crate calls goes through neither. So a
/// map a JAVA constructor allocates has a real `null` at `table` and is silent,
/// while the backing map [`alloc_hs_backing`] allocates for every `HashSet` has
/// `Int(0)` there and is counted on every read until the first insert publishes
/// an array.
///
/// # Why this is a repair and not a way to quiet the census
///
/// The guard is RIGHT that a primitive is sitting in a reference slot; it is
/// only wrong about who wrote it. Filling the slot with a fabricated array to
/// make the number go down would be the anti-pattern this tree has been bitten
/// by — a loud, counted coercion traded for a silent wrong answer. Leaving the
/// slot empty is correct; this makes "empty" spell itself the way the JVMS
/// spells it.
///
/// # Java-visible behaviour is unchanged, by construction
///
/// `coerce_field_value_for_slot` answers a `b'L' | b'['` descriptor with
/// `Object(None)` for `Int(0)` and with `Object(None)` for `Object(None)`. So
/// every descriptor-aware reader — `get_field`, `get_field_volatile`,
/// `get_field_typed`, `compare_and_swap_field`, and the interpreter's own
/// `getfield` — sees the byte-identical `Value` before and after this write.
/// The readers that DO differ are the non-coercing ones (`get_field_raw`,
/// `Object.clone`'s verbatim field copy, the JIT's direct cell reads), and for
/// those `Int(0)` at a reference slot was the wrong answer and `null` is the
/// right one. There is no arm on which the old value is preferable, which is
/// why this is not behind a kill switch: there would be nothing to A/B.
///
/// # Scope, and the fabricated-layout early return
///
/// Names resolved on the RECEIVER — the same question [`receiver_table_slot`]
/// asks, through the same call — and only for a receiver that HAS a real
/// `table` field. A fabricated layout (`cratonvm/synthetic/AnonymousObject$N`,
/// the bare `ClassId(0)` CHM segments) returns early, and that is load-bearing:
/// its `_fN` slots have no declared descriptor, the coercion's `_ => value` arm
/// passes them through untouched, and this file deliberately keeps
/// `Int(capacity)` at `MAP_FIELD_CAPACITY` and `Int(size)` at `MAP_FIELD_SIZE`
/// in two of them. Writing `null` there would delete live state. The early
/// return is the same discriminator [`publish_map_table_inner`] already uses to
/// decide whether the legacy capacity `Int` may be stored at all.
///
/// The four names are the reference-typed instance fields every map this crate
/// allocates carries: `AbstractMap.keySet`, `AbstractMap.values`,
/// `HashMap.table`, `HashMap.entrySet`. `LinkedHashMap.head`/`tail` are
/// deliberately NOT in the list — they are references too, but a name that a
/// shadowing subclass could have redeclared as an `int` would turn one census
/// row (`primitive-into-reference`) into another (`null-into-primitive`)
/// instead of removing one, and no reader in this file consults them before a
/// writer has.
///
/// Cost: four `resolve_field_index_by_class_id` per map ALLOCATION — not per
/// operation — on a path that already spends an `ensure_class_initialized`, a
/// `class_num_total_fields`, and four more name resolutions in the initializer
/// that runs immediately after. Not a GC point: `set_field` only runs the write
/// barrier, which never allocates from the Java heap (see [`publish_map_table`]
/// for the same note), so no caller needs a pin around this call.
pub(crate) fn init_native_map_reference_defaults(ctx: &mut dyn NativeContext, map: ObjectRef) {
    if receiver_table_slot(ctx, map).is_none() {
        // Fabricated layout — see the doc comment above. Nothing here is a
        // reference field by DECLARATION, so there is no default to write, and
        // writing one would clobber the model's `Int` slots.
        return;
    }
    let class_id = ctx.class_id_of_object(map);
    let num_fields = ctx.object_num_fields(map);
    for name in ["table", "entrySet", "keySet", "values"] {
        if let Some(slot) = ctx.resolve_field_index_by_class_id(class_id, name) {
            if slot < num_fields {
                ctx.set_field(map, slot, Value::Object(None));
            }
        }
    }
}

/// Publish a freshly built bucket table on `map`, honouring both storage
/// conventions this crate maintains:
///
/// * absolute slot 0 (`MAP_FIELD_BUCKETS`) always gets the array — it is what
///   every other native map operation reads, and the native surface is the
///   authoritative implementation for the `HashMap` family;
/// * the receiver's REAL `table` field gets the same array when it is a
///   different slot, so JDK bytecode that reads `getfield map.table` directly
///   (`HashMap.resize`, `KeySpliterator`, `writeObject`) sees a live array
///   rather than a stale value or an `Int`;
/// * the legacy `Int(capacity)` at absolute slot 2 (`MAP_FIELD_CAPACITY`) is
///   written ONLY when the receiver actually has our fabricated layout. On a
///   real layout slot 2 is a declared field — `table` on `HashMap`,
///   `threshold` on `Hashtable`/`Properties` — and the raw write either
///   clobbers the bucket array with `Int(16)` (`arraylength` on an int, and
///   the `HashMap` slot-2 census row) or destroys an unrelated field.
///   `map_state` derives the capacity from the bucket array's length and only
///   falls back to slot 2 when there is no array, so dropping the write costs
///   nothing.
///
/// Neither `set_field` variant is a GC point (`gen_heap::write_barrier` never
/// allocates from the Java heap), so callers may pass references they have
/// already re-read through their pins.
pub(crate) fn publish_map_table(ctx: &mut dyn NativeContext, map: ObjectRef, buckets: ObjectRef, cap: i32) {
    publish_map_table_inner(ctx, map, buckets, cap, false);
}

/// [`publish_map_table`] with a release-style store of the bucket array, for
/// `map_resize`'s publication of a rehashed table to lock-free CHM readers.
pub(crate) fn publish_map_table_volatile(
    ctx: &mut dyn NativeContext,
    map: ObjectRef,
    buckets: ObjectRef,
    cap: i32,
) {
    publish_map_table_inner(ctx, map, buckets, cap, true);
}

pub(crate) fn publish_map_table_inner(
    ctx: &mut dyn NativeContext,
    map: ObjectRef,
    buckets: ObjectRef,
    cap: i32,
    volatile: bool,
) {
    let table_slot = receiver_table_slot(ctx, map);
    // ONE store, to the slot this receiver actually keeps its table in.
    //
    // This used to write slot 0 as well, unconditionally, so that the readers
    // could go there without resolving anything. On a real `HashMap` slot 0 is
    // `AbstractMap.keySet`, so every map this VM built handed the image a
    // `Set` field holding an `Object[]` (`probes/MapModelSlotProbe`:
    // `keySet=ARRAY[Object]` where HotSpot has `null`). `map_state` now asks
    // `map_buckets_slot` the same question, so the second store bought
    // nothing but the wrong field.
    let slot = table_slot.unwrap_or(MAP_FIELD_BUCKETS);
    if volatile {
        ctx.set_field_volatile(map, slot, Value::Object(Some(buckets)));
    } else {
        ctx.set_field(map, slot, Value::Object(Some(buckets)));
    }
    // Fabricated layout: slot 2 means `capacity` and nothing else. On a real
    // layout it is a declared field (`table` on `HashMap`, `threshold` on
    // `Hashtable`) and `map_state` derives the capacity from the array length,
    // so there is nothing to store. The write stays unguarded, as it has
    // always been — `set_field` is contractually bounds-checked (M4a) and
    // drops an out-of-range index.
    if table_slot.is_none() {
        ctx.set_field(map, MAP_FIELD_CAPACITY, Value::Int(cap));
    }
}

/// Determine whether `obj`'s runtime class is a `java.lang.Enum` subclass and,
/// if so, return a *canonical logical identity* for the enum constant as
/// `(declaring_class_id, ordinal)`.
///
/// # Why this exists
///
/// `java.lang.Enum` declares `equals`/`hashCode` `final` with pure-identity
/// semantics, so a correct VM only ever has ONE object per enum constant and
/// `HashMap` keyed by enum constants works on pointer identity. CratonVM's
/// class-loading / `$VALUES` plumbing can, on some boot paths, materialise a
/// *second* object for the same constant (e.g. the constant read back via
/// `Enum.values()` during one `<clinit>` is not pointer-equal to the one a
/// later `getstatic Foo.BAR` resolves). When that happens, an enum-keyed
/// `HashMap`/`LinkedHashMap` silently misses every lookup — `Map.get` returns
/// `null` even though the key was `put`.
///
/// Concrete victim: Keycloak's `Profile.isFeatureEnabled(Feature)` does
/// `features.get(feature)` then unboxes the result; a missing entry yields
/// `NullPointerException: Cannot invoke "java.lang.Boolean.booleanValue()"`.
///
/// Keying enum constants by `(declaring class name, constant name)` is
/// *semantically identical* to identity — the JLS guarantees exactly one
/// constant per (class, name) — so this never makes two genuinely-distinct
/// constants compare equal, and it makes enum-keyed maps immune to the
/// duplicate-object bug.
///
/// EQUALITY ONLY since JIT round 20 wave 17 (lane hashOff): the bucket hash
/// ([`map_key_raw_hash`]) is the JDK's `Enum.hashCode()` (the identity hash)
/// again, because the stand-in shares the JDK's `table` with bytecode that
/// hashes that way; a name hash split the two (see the arm there).
///
/// The *class name string* (not the `ClassId`) is used deliberately: if the
/// VM materialised the duplicate by loading the enum class under two
/// different `ClassId`s, the names still agree, whereas the ids would not.
/// The constant's `name` (slot 0) and `ordinal` (slot 1) layout is fixed by
/// `Enum.<init>` (see `native-builtins` `native_enum_init`).
pub(crate) fn enum_key_identity(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<(String, String)> {
    let cid = ctx.class_id_of_object(obj);
    // JIT round 16 (lane compatdrain): the ancestry answer from the memoized
    // class facts ([`CF_ENUM`]) instead of a name walk per call.
    if enum_key_facts_enabled() {
        if !class_facts(ctx, cid).has(CF_ENUM) {
            return None;
        }
        let class_name = ctx.class_name_of_id(cid)?;
        let const_name = match ctx.get_field(obj, 0) {
            Value::Object(Some(s)) => ctx.read_string(s)?,
            _ => return None,
        };
        return Some((class_name, const_name));
    }
    // Walk the superclass chain looking for java/lang/Enum. Cap the walk to
    // guard against malformed class hierarchies.
    let mut cur = Some(cid);
    let mut is_enum = false;
    for _ in 0..64 {
        match cur {
            Some(c) => match ctx.class_name_of_id(c) {
                Some(n) if n == "java/lang/Enum" => {
                    is_enum = true;
                    break;
                }
                Some(n) if n == "java/lang/Object" => break,
                Some(_) => cur = ctx.superclass_of(c),
                None => break,
            },
            None => break,
        }
    }
    if !is_enum {
        return None;
    }
    // Declaring-class name — stable across ClassId duplication.
    let class_name = ctx.class_name_of_id(cid)?;
    // Enum layout: slot 0 = name (String), slot 1 = ordinal (int).
    let const_name = match ctx.get_field(obj, 0) {
        Value::Object(Some(s)) => ctx.read_string(s)?,
        _ => return None,
    };
    Some((class_name, const_name))
}

/// Whether `key` is an enum constant (its class or its constant-body subclass
/// descends from `java/lang/Enum`): [`CF_ENUM`] when the facts are on, else
/// [`enum_key_identity`]'s walk. [`map_key_raw_hash`]'s enum arm (JIT round 20
/// wave 17, lane hashOff) asks only this; it needs no names.
pub(crate) fn is_enum_key(ctx: &dyn NativeContext, key: ObjectRef) -> bool {
    if enum_key_facts_enabled() {
        return class_facts(ctx, ctx.class_id_of_object(key)).has(CF_ENUM);
    }
    enum_key_identity(ctx, key).is_some()
}

/// `CRATONVM_COMPAT_MAP_ENUM_KEY_FACTS` -- default ON since 2026-10-06 (hcb),
/// `=0` restores the name walk. See [`CF_ENUM`]; the `--compatible`
/// `HashMap.get` profile in
/// `docs/feature-designs/collections-interception.md`
/// put the walk at 4-6% of a user-key `get`. Measured on the native doors that
/// stay (`tools/probes/HcbNativeMapKeyGet.java`, 6 + 4 interleaved reps): a
/// user-key `ConcurrentHashMap.get` -8% (min) and `Hashtable.get` -12%;
/// `tools/probes/HcbEnumKeyMaps.java` (plain and constant-body enum keys) is
/// HotSpot-identical either way.
pub(crate) fn enum_key_facts_enabled() -> bool {
    pub(crate) static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_COMPAT_MAP_ENUM_KEY_FACTS")
    })
}

pub(crate) fn enum_ordinal_value(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<i32> {
    match ctx.get_field_by_name(obj, "ordinal") {
        Value::Int(n) => Some(n),
        _ => match ctx.get_field(obj, 1) {
            Value::Int(n) => Some(n),
            _ => None,
        },
    }
}

pub(crate) fn enum_set_contains_member(
    ctx: &dyn NativeContext,
    set: ObjectRef,
    elem: ObjectRef,
) -> Option<bool> {
    let ordinal = enum_ordinal_value(ctx, elem)?;
    if ordinal < 0 {
        return Some(false);
    }
    let word = (ordinal as usize) / 64;
    let bit = 1u64.checked_shl((ordinal as u32) & 63)?;
    match ctx.get_field_by_name(set, "elements") {
        Value::Long(mask) => {
            if word == 0 {
                Some(((mask as u64) & bit) != 0)
            } else {
                Some(false)
            }
        }
        Value::Int(mask) => {
            if word == 0 {
                Some(((mask as u32 as u64) & bit) != 0)
            } else {
                Some(false)
            }
        }
        Value::Object(Some(words)) if ctx.heap_kind_of(words) == ObjectKind::Array => {
            if word >= ctx.array_length(words) {
                return Some(false);
            }
            match ctx.get_array_element(words, word) {
                Value::Long(mask) => Some(((mask as u64) & bit) != 0),
                Value::Int(mask) => Some(((mask as u32 as u64) & bit) != 0),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Pure reference-identity hash, matching `IdentityHashMap`'s documented
/// contract (`System.identityHashCode`), with the same bit-spread every other
/// bucket hash in this file applies. Unlike `map_hash_key`, this never
/// dispatches into Java (`ctx.identity_hash_code` reads the object header),
/// so it cannot trigger a moving GC — an identity-map lookup/insert needs no
/// pin around this call.
#[inline]
pub(crate) fn map_hash_key_identity(ctx: &dyn NativeContext, key: ObjectRef) -> i32 {
    let h = ctx.identity_hash_code(key);
    h ^ ((h as u32) >> 16) as i32
}

/// Pure reference-identity equality, matching `IdentityHashMap`'s documented
/// contract (`==`, not `Object.equals`). Never dispatches into Java, so —
/// unlike `map_keys_equal` — it cannot trigger a moving GC either.
#[inline]
pub(crate) fn map_keys_equal_identity(a: ObjectRef, b: ObjectRef) -> bool {
    std::ptr::eq(a.as_ptr(), b.as_ptr())
}

// ---------------------------------------------------------------------------
// JDK comparison order for the `--compatible` bucket maps (round 13 wave 4,
// lane hashcompat; page `r13w4-hashcompat-compatible-maps-diverge-from-jdk-
// comparison-order-20260928.md`).
// ---------------------------------------------------------------------------

/// `CRATONVM_COMPAT_MAP_JDK_ORDER` — default ON, `=0` restores the old
/// behaviour. When on, the native bucket maps follow the JDK's own order:
///
/// * `put` grows the table only for a key that is ABSENT (`HashMap.putVal`
///   resizes after `++size > threshold`, `Hashtable.addEntry` before linking a
///   NEW entry); overwriting a key at the threshold used to double the table
///   and move every later iteration position;
/// * `java.util.Hashtable` (not `Properties`) stores and indexes the RAW
///   `hashCode()` like its own bytecode (`putIfAbsent`, `computeIfAbsent`,
///   `merge`, `hashCode()`, `readObject` all run real bytecode over the same
///   table), compares `storedKey.equals(key)` with no `==` pre-check, links a
///   new entry at the bucket HEAD, re-reads `key.hashCode()` after a rehash
///   and rehashes the old table from the top down.
pub(crate) fn map_jdk_order_enabled() -> bool {
    pub(crate) static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_MAP_JDK_ORDER")
}

/// `CRATONVM_COMPAT_CHM_VALUE_EQUALS` — default ON, `=0` restores the old
/// identity/String/wrapper-only value test in `ConcurrentHashMap.remove(k, v)`
/// and `replace(k, old, new)`. The JDK decides both with
/// `cv == ev || (ev != null && cv.equals(ev))` (`replaceNode`), i.e. the
/// ARGUMENT's `equals`; without it an equal-but-distinct value object never
/// matched and the call answered `false` without removing or replacing.
pub(crate) fn chm_value_equals_enabled() -> bool {
    pub(crate) static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_CHM_VALUE_EQUALS")
}

/// `CRATONVM_COMPAT_BOOLEAN_ELEMENT_HASH` — default ON, `=0` restores the old
/// answer. A boxed `Boolean` contributes `Boolean.hashCode()` (1231/1237) to
/// the native `List`/`Set`/`Map`/`Map.Entry` `hashCode()` aggregates, not the
/// 1/0 its unboxed field holds.
pub(crate) fn boolean_element_hash_enabled() -> bool {
    pub(crate) static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_BOOLEAN_ELEMENT_HASH")
}

/// Whether `obj` is a `java.lang.Boolean` (a final class, so exact). Read
/// from the per-`ClassId` facts memo ([`CF_BOOLEAN_BOX`]): the bucket-hash
/// callers below sit on the `HashMap.get`/`put` path for every `Integer` key
/// equal to 0 or 1, where a `class_manager` read lock per call would show.
pub(crate) fn is_boolean_box(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    receiver_facts(ctx, obj).has(CF_BOOLEAN_BOX)
}

/// One default-ON `--compatible` kill switch: `true` unless the variable is
/// set to an off spelling. Read once per process, like the neighbouring
/// switches.
///
/// Round 14 wave 1 (lane compat): the off spellings are
/// [`cratonvm_types::flags::runtime_flag_default_on`]'s (`0`, `false`, `off`,
/// `no`, trimmed, any case). Only `0` used to count, so
/// `CRATONVM_COMPAT_X=false` left the fix ON silently -- the kill-switch
/// defect that helper's doc records for `CRATONVM_JIT_LICM`. No value that
/// was off becomes on.
pub(crate) fn compat_switch_on(cell: &std::sync::OnceLock<bool>, name: &str) -> bool {
    *cell.get_or_init(|| cratonvm_types::flags::runtime_flag_default_on(name))
}

/// `CRATONVM_COMPAT_BOOLEAN_KEY_HASH` — default ON, `=0` restores the old
/// buckets. A `Boolean` KEY hashes as `Boolean.hashCode()` (1231/1237) in
/// every native bucket hash — [`map_key_raw_hash`] (so `HashMap`,
/// `LinkedHashMap`, `Hashtable`, `HashSet` and the CHM general path), the
/// no-Java CHM wrapper-key path ([`chm_wrapper_key_hash_for`]) — and the
/// `hm_int_fast` integer overlay refuses `Boolean` keys, because it orders
/// its entries by the unboxed value. All of them read this one switch, so
/// they cannot disagree about where a key lives. Round 13 wave 6 (lane
/// hashtree); page `r13w4-hashcompat-compatible-maps-diverge-from-jdk-
/// comparison-order-20260928.md` item 4.
pub(crate) fn boolean_key_hash_enabled() -> bool {
    pub(crate) static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_BOOLEAN_KEY_HASH")
}

/// `CRATONVM_COMPAT_HASHSET_BUILD_JDK` — default ON, `=0` restores the old
/// builder. [`make_hashset_with_elements`] (the native `Set.of(4..10)` and
/// the `native-builtins` producers) builds the `HashSet` that `new
/// HashSet<>()` plus one `add` per element builds on the JDK: a throwing
/// `hashCode()`/`equals()` PROPAGATES (it was swallowed -- identity hash, or
/// "not equal"), a new node is linked at its bucket's TAIL (it was the head,
/// reversing every bucket), and the table is the one those adds grow to (it
/// was `max(nextPow2(n), 16)`, which overfills past `3/4` from 13 elements).
/// Round 13 wave 6, lane hashtree; page item 7.
pub(crate) fn hashset_build_jdk_enabled() -> bool {
    pub(crate) static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_HASHSET_BUILD_JDK")
}

/// `CRATONVM_COMPAT_HASHSET_BUILD_TREEIFY` — default ON, `=0` restores the
/// chain-only builder. [`make_hashset_with_elements`] links nodes into a table
/// presized for the whole input, which is the JDK's result only while no bin
/// reaches `TREEIFY_THRESHOLD`: `HashSet.add` of a bin's 9th element resizes
/// a table below 64 slots (`treeifyBin`) or turns the bin into a tree. When
/// the builder is about to link a 9th node into one bin it now REPLAYS the
/// elements it has placed through the native `put` path from `new
/// HashMap<>()`'s empty table (their stored hashes, no second `hashCode()`,
/// no second `equals`), and `put`s the rest as `add` would, so the table size,
/// the tree bin and the iteration order are the adds'. Round 13 wave 13, lane
/// compat7 (proposal C6-5).
pub(crate) fn hashset_build_treeify_enabled() -> bool {
    pub(crate) static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_HASHSET_BUILD_TREEIFY")
}

/// `CRATONVM_COMPAT_HASHSET_BUILD_NULLS` -- default ON, `=0` restores the
/// skip. [`make_hashset_with_elements`]' real-layout builder skipped a null
/// element (`_ => continue`), so a producer handing it one got a set without
/// it and no signal; `new HashSet<>()` + `add(null)` keeps one null. The
/// builder now links nothing itself from the first null on: it replays what
/// it placed and `put`s the rest ([`hashset_build_finish_by_put`], whose
/// `put` keeps a null key at bucket 0, as `putVal` does). Round 15 wave 3
/// (lane compat3), `r15w3-compat2-collection-review-findings-CLOSED-20261001.md`
/// item 3.
pub(crate) fn hashset_build_nulls_enabled() -> bool {
    pub(crate) static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_HASHSET_BUILD_NULLS")
}

/// `CRATONVM_COMPAT_HASHSET_BUILD_TREEIFY_LEVELS` — default ON (only with
/// `CRATONVM_COMPAT_HASHSET_BUILD_TREEIFY`), `=0` restores the presized-table
/// test alone. [`make_hashset_with_elements`] also replays when a bin of a
/// SMALLER table the adds pass through is about to take its 9th node: there
/// the adds treeify (64+ slots) or resize early, and a tree formed at 64
/// slots that a later doubling splits into halves of 7-8 nodes stays a tree
/// (or is untreeified root first), which the presized table, where those
/// halves are separate short bins, never sees. Round 14 wave 1, lane compat
/// (`r13w13-compat7-collection-review-findings` item 2).
pub(crate) fn hashset_build_treeify_levels_enabled() -> bool {
    pub(crate) static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_HASHSET_BUILD_TREEIFY_LEVELS")
}

/// `CRATONVM_COMPAT_SET_OF_START_TABLE` — default ON, `=0` restores the
/// presized table. [`make_set_of`] (`Collectors.toSet()`, `groupingBy(..,
/// toSet())` and the other native set producers of the `--compatible`
/// streams) is `HashSet::new` plus one `add` per element on the JDK: the
/// table starts at 16 and grows as the adds cross `3/4`. It was presized to
/// `nextPow2(n)`, so a bin that reached 9 elements saw a table the adds would
/// not have had yet: treeified where the JDK resizes (below 64 slots). The
/// puts grow it exactly as `putVal` does. Round 13 wave 13, lane compat7.
pub(crate) fn set_of_start_table_enabled() -> bool {
    pub(crate) static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_SET_OF_START_TABLE")
}

/// The table `new HashSet<>()` has after `n` distinct `add`s: 16, doubled
/// while `n` exceeds `3/4` of it (`HashMap.putVal`'s `++size > threshold`).
pub(crate) fn hashset_build_capacity(n: usize) -> usize {
    let mut cap = MAP_DEFAULT_CAPACITY;
    while n > (cap * 3) / 4 && cap < MAP_MAX_CAPACITY as usize {
        cap *= 2;
    }
    cap
}

/// `CRATONVM_COMPAT_MAP_LONG_CHAINS` — default ON, `=0` restores the fixed
/// 4096-node cap. See [`chain_walk_limit`].
pub(crate) fn map_long_chains_enabled() -> bool {
    pub(crate) static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_MAP_LONG_CHAINS")
}

/// How many nodes a native bucket-chain walk of a map holding `size` entries
/// may visit before it calls the chain corrupt (a cycle) and throws
/// `IllegalStateException`.
///
/// The cap used to be a flat 4096. The native maps never treeify
/// (`r13w4-hashcompat-native-hashmap-never-treeifies-FIXED-20260929.md`), so 4097
/// keys with one `hashCode()` -- a constant-hash key class, or colliding
/// `String`s -- are one legitimate 4097-node chain, and `put`/`get`/`remove`/
/// `containsKey` threw where HotSpot answers. A chain can hold at most every
/// entry of its map, so `size` plus a little slack (an insert in flight, a
/// racing unsynchronized writer) still detects a cycle -- a cyclic walk never
/// ends -- without refusing a real chain. Round 13 wave 6, lane hashtree.
#[inline]
pub(crate) fn chain_walk_limit(size: i32) -> usize {
    const FIXED: usize = 4096;
    if map_long_chains_enabled() {
        FIXED.max((size.max(0) as usize).saturating_add(64))
    } else {
        FIXED
    }
}

/// `Boolean.hashCode()` for a wrapper key of class `key_class` whose unboxed
/// value is `bit`, when [`boolean_key_hash_enabled`]; `None` for every other
/// key, which keeps hashing by its unboxed value. Screened on the value first
/// so an `Integer` key pays the facts lookup only for 0 and 1.
#[inline]
pub(crate) fn boolean_key_bucket_hash(ctx: &dyn NativeContext, key_class: ClassId, bit: i32) -> Option<i32> {
    if (bit == 0 || bit == 1)
        && boolean_key_hash_enabled()
        && class_facts(ctx, key_class).has(CF_BOOLEAN_BOX)
    {
        Some(if bit != 0 { 1231 } else { 1237 })
    } else {
        None
    }
}

/// Whether `this` takes `java.util.Hashtable`'s own hash and comparison order
/// (see [`map_jdk_order_enabled`]). `Properties` is excluded, as by
/// [`uses_native_hashtable_layout`]: JDK 25 backs it with a
/// `ConcurrentHashMap`, whose order is `HashMap`'s.
#[inline]
pub(crate) fn hashtable_order(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    uses_native_hashtable_layout(ctx, this) && map_jdk_order_enabled()
}

/// `java.util.Hashtable`'s chain predicate after the hash test:
/// `e.key.equals(key)` — the STORED key is the receiver and there is no
/// `e.key == key` short cut (`get`, `put`, `remove`, `containsKey`).
///
/// The wrapper / String / enum arms answer without a Java call exactly as in
/// [`map_keys_equal`]: their `equals` is final, symmetric and reflexive, so
/// neither the direction nor the missing identity test is observable there.
/// Everything else runs `stored.equals(search)`.
pub(crate) fn hashtable_keys_equal(
    ctx: &mut dyn NativeContext,
    search: ObjectRef,
    stored: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    if let (Some(ps), Some(pk)) = (unbox_wrapper(ctx, stored), unbox_wrapper(ctx, search)) {
        if ctx.class_id_of_object(stored) != ctx.class_id_of_object(search) {
            return Ok(false);
        }
        return Ok(wrapper_prims_equal(ps, pk));
    }
    if let Some(equal) = ctx.java_strings_equal(stored, search) {
        return Ok(equal);
    }
    if let (Some(ia), Some(ib)) = (
        enum_key_identity(ctx, stored),
        enum_key_identity(ctx, search),
    ) {
        return Ok(ia == ib);
    }
    // nio20 wave 6, L5: `Object.equals` is `==` ([`CF_IDENTITY_KEY`]).
    if identity_key_hash_enabled() && receiver_facts(ctx, stored).has(CF_IDENTITY_KEY) {
        return Ok(std::ptr::eq(stored.as_ptr(), search.as_ptr()));
    }
    match ctx.invoke_virtual(
        stored,
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(search))],
    )? {
        Some(Value::Int(v)) => Ok(v != 0),
        _ => Ok(false),
    }
}

/// Compute hash for a key.
///
/// MED fix: when the user-supplied `hashCode()` throws (i.e. `invoke_virtual`
/// returns `Err(MethodCallFailed)`), the previous implementation silently
/// fell back to `identity_hash_code`, breaking the equals/hashCode contract
/// in a way that made subsequent `get` calls miss every key (and combined
/// with the matching swallow in `map_keys_equal`, `put` succeeded but `get`
/// always returned null). Real JDK propagates the exception out of
/// `HashMap.put`/`get` — this implementation now does the same by returning
/// `Result<i32, MethodCallFailed>` and surfacing the original error to the
/// caller. Non-exceptional contract violations (e.g. the user returned a
/// non-Int from hashCode) still fall back to identity since those are not
/// exceptional control flow.
///
/// This is `HashMap.hash(key)`: the raw hash of [`map_key_raw_hash`] spread by
/// `h ^ (h >>> 16)`.
pub(crate) fn map_hash_key(ctx: &mut dyn NativeContext, key: ObjectRef) -> Result<i32, MethodCallFailed> {
    let h = map_key_raw_hash(ctx, key)?;
    Ok(h ^ ((h as u32) >> 16) as i32)
}

/// The key's hash with NO spreading: what `java.util.Hashtable` stores in
/// `Entry.hash` and indexes with (`(hash & 0x7FFFFFFF) % tab.length`). The
/// arms are [`map_hash_key`]'s; only the final spread is left to the caller.
pub(crate) fn map_key_raw_hash(ctx: &mut dyn NativeContext, key: ObjectRef) -> Result<i32, MethodCallFailed> {
    // Wrapper types: hash by their primitive value (matches JDK Integer.hashCode
    // etc.) — checked BEFORE the enum check (perf: a JDK wrapper class can never
    // be a `java/lang/Enum` subclass, so this reorder is behavior-preserving; see
    // reference_hashmap_native_call_dispatch_overhead_20260711 — this is by far
    // the most common key type and `enum_key_identity`'s superclass-chain walk
    // took a `class_manager` RwLock read + allocating class-name lookup per hop
    // just to fail for every Integer/Long/etc. key).
    if let Some(prim) = unbox_wrapper(ctx, key) {
        let h = match prim {
            // `Boolean` unboxes to `Int(0|1)`; its `hashCode()` is 1231/1237.
            Value::Int(v) => {
                let key_class = ctx.class_id_of_object(key);
                boolean_key_bucket_hash(ctx, key_class, v).unwrap_or(v)
            }
            Value::Long(v) => (v ^ (v >> 32)) as i32,
            // Canonical bits, not raw: `Float.equals`/`Double.equals` (below,
            // and in `values_equal`) canonicalize the NaN payload, so hashing
            // from raw bits puts two keys that are `equals` in different
            // buckets. A `HashMap` with `Double.NaN` and `Math.sqrt(-1.0)` as
            // keys then held two entries where HotSpot holds one.
            Value::Float(v) => cratonvm_types::jfp::float_hash_code(v),
            Value::Double(v) => cratonvm_types::jfp::double_hash_code(v),
            _ => ctx.identity_hash_code(key),
        };
        return Ok(h);
    }
    // Strings: hash by UTF-16 code units. Wrappers are checked first because
    // the two class sets are disjoint and wrapper-heavy maps otherwise take a
    // class-manager lock just to reject String before every unbox.
    if let Some(h) = ctx.java_string_hash_code(key) {
        return Ok(h);
    }
    // nio20 wave 6, L5: the two key shapes whose `hashCode()` this file can
    // answer without the Java call, with the JDK body's own answer. See
    // [`CF_IDENTITY_KEY`] / [`CF_LOCALE`].
    if identity_key_hash_enabled() {
        let cid = ctx.class_id_of_object(key);
        let facts = class_facts(ctx, cid);
        if facts.has(CF_IDENTITY_KEY) {
            return Ok(ctx.identity_hash_code(key));
        }
        if facts.has(CF_LOCALE) {
            if let Some(h) = locale_cached_hash(ctx, key, cid) {
                return Ok(h);
            }
        }
    }
    // Enum constants: the JDK's own `Enum.hashCode()`, which is `final` and
    // is `System.identityHashCode(this)` (JDK 25 caches it in `hash`; the
    // `--compatible` `native_enum_hash_code` answers the same header word).
    //
    // JIT round 20 wave 17 (lane hashOff), correctness, unconditional. This
    // arm used to hash by (declaring class name, constant name), a defence
    // against a VM that materialised two objects for one constant. But the
    // stand-in's nodes live in the JDK's own `table` (`map_state`), next to
    // nodes the JDK's bytecode writes (S111r27's layout-aware walk exists for
    // exactly that), and every JDK body hashes with `key.hashCode()`:
    // `HashMap.clone()` -> `putMapEntries` -> `putVal(hash(key), ..)`, a
    // `HashMap` subclass, any JDK bytecode the JIT dispatches or splices on a
    // stand-in map, and a Rust `native_map_put_pub` into a map the JDK reads.
    // A name hash put the stand-in's enum node in another bucket with another
    // `hash` than the JDK's, so each side missed the other's enum keys (`get`
    // answered null for a key that was `put`). Under `--compatible` with the
    // `jdk-hash-collections` row off that is the javac
    // `Locations.handlersForLocation` (`StandardLocation` keys) shape; see
    // `docs/feature-designs/jdk-only-mode.md`.
    // Equality stays `map_keys_equal`'s (an enum compares by constant), which
    // is identity for one object per constant. The same split hung Hibernate
    // Reactive's single-process `--compatible` run (`TenantDependentPool`'s
    // `poolMap.get(Tenant.DEFAULT)` after a GC emptied the compiled `get`
    // helper's memo; probe `tools/probes/HriEnumMapKey.java`).
    if is_enum_key(ctx, key) {
        return Ok(ctx.identity_hash_code(key));
    }
    // S111r-bug-fix (peaceful-sammet): for arbitrary user-defined objects we
    // MUST call their `hashCode()` so HashMap honours the equals/hashCode
    // contract — otherwise `HashMap.get(equalKey) == null` for non-identical
    // but equal keys (Spring's `AnnotationTypeMapping.aliasedBy` keyed by
    // `java.lang.reflect.Method` is the canonical victim, surfacing as the
    // `@AliasFor ... is not meta-present` chain on Spring Boot startup).
    //
    // MED fix: a thrown exception from `hashCode()` propagates via `?` —
    // previously it was swallowed and substituted with `identity_hash_code`,
    // silently breaking the equals/hashCode contract.
    let h = match ctx.invoke_virtual(key, "hashCode", "()I", &[])? {
        Some(Value::Int(v)) => v,
        _ => ctx.identity_hash_code(key),
    };
    Ok(h)
}

/// Compute the *raw* Java `hashCode()` of an element `Value` — i.e. the value
/// returned by `Object.hashCode()` with NO HashMap bit-spreading applied.
///
/// This is what the `List`/`Set`/`Map` `hashCode` contracts require (unlike
/// `map_hash_key`, which additionally spreads via `h ^ h>>>16` for bucket
/// distribution). `null` hashes to 0. Strings and primitive wrappers are
/// hashed by value to match the JDK; arbitrary objects dispatch to their
/// virtual `hashCode()`.
///
/// Fallible for the reason `map_hash_key` already documents one screen up —
/// this is the same swallow, in the helper the *collection* `hashCode`
/// contracts run through instead of the bucket path. `_ => identity_hash_code`
/// answered a plausible number for a `hashCode()` that threw or was refused,
/// and a plausible number is the worst possible answer here: the aggregate
/// `List`/`Set`/`Map` hash comes out stable and wrong, so the collection is
/// filed under a bucket its own equal twin will never be found in.
///
/// A callee that returns something other than an `Int` — a contract violation
/// with no exceptional control flow — still falls back to the identity hash,
/// exactly as `map_hash_key` does.
pub(crate) fn element_hash_code(ctx: &mut dyn NativeContext, v: &Value) -> Result<i32, MethodCallFailed> {
    match v {
        Value::Object(None) => Ok(0),
        Value::Int(x) => Ok(*x),
        Value::Long(x) => Ok((*x ^ (*x >> 32)) as i32),
        // Canonical bits — see the note in `map_hash_key`.
        Value::Float(x) => Ok(cratonvm_types::jfp::float_hash_code(*x)),
        Value::Double(x) => Ok(cratonvm_types::jfp::double_hash_code(*x)),
        Value::Object(Some(obj)) => {
            // String hashCode by value, asked of the VM so it is computed from
            // the receiver's own UTF-16 storage. Decoding to a Rust `String`
            // first and re-encoding — what this did — turns every unpaired
            // surrogate into `U+FFFD` before the multiply-add ever runs, so a
            // `Map`/`Set` holding such content reported a stable but WRONG
            // aggregate hash (MEASURED 2026-08-17: `HashMap.hashCode()` 2124877
            // where HotSpot says 1839336). `map_hash_key`, one screen up,
            // already asks the same question this way; the two must not answer
            // differently about the same String.
            if let Some(h) = ctx.java_string_hash_code(*obj) {
                return Ok(h);
            }
            // Primitive wrapper types hash by their boxed primitive value.
            if let Some(prim) = unbox_wrapper(ctx, *obj) {
                // ...except `Boolean`, which unboxes to `Int(0|1)` while
                // `Boolean.hashCode()` is 1231/1237 (the record intrinsic's
                // G9-1 fix, same trap). Screened on the value first so an
                // `Integer` key pays the class-name read only for 0 and 1.
                if let Value::Int(bit @ (0 | 1)) = prim {
                    if boolean_element_hash_enabled() && is_boolean_box(ctx, *obj) {
                        return Ok(if bit != 0 { 1231 } else { 1237 });
                    }
                }
                return element_hash_code(ctx, &prim);
            }
            // Arbitrary objects: honour the contract via their virtual hashCode().
            match ctx.invoke_virtual(*obj, "hashCode", "()I", &[]) {
                Ok(Some(Value::Int(h))) => Ok(h),
                Err(e) => Err(e),
                _ => Ok(ctx.identity_hash_code(*obj)),
            }
        }
        // Internal VM values that cannot legitimately be collection elements.
        Value::ReturnAddress(_) | Value::Uninitialized => Ok(0),
    }
}

/// Check if two keys are equal.
///
/// MED fix: when the user-supplied `equals(Object)` throws (i.e.
/// `invoke_virtual` returns `Err(MethodCallFailed)`), the previous
/// implementation silently treated it as `false`, breaking the
/// equals/hashCode contract — combined with the matching `map_hash_key`
/// swallowing this meant `put` succeeded but `get` always returned null.
/// Real JDK propagates the exception out of `HashMap.put`/`get` — this
/// implementation now does the same by returning
/// `Result<bool, MethodCallFailed>` and surfacing the original error to
/// the caller. Non-exceptional contract violations (e.g. the user returned
/// a non-Int from equals) still fall through as `false`.
pub(crate) fn map_keys_equal(
    ctx: &mut dyn NativeContext,
    a: ObjectRef,
    b: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    if std::ptr::eq(a.as_ptr(), b.as_ptr()) {
        return Ok(true);
    }
    // Wrapper type equality: unbox and compare primitives. Checked before the
    // enum check below (perf: no enum class can also be a JDK wrapper class,
    // so this reorder is behavior-preserving; see
    // reference_hashmap_native_call_dispatch_overhead_20260711 — wrapper keys
    // are by far the most common case, and the check below takes a
    // `class_manager` RwLock read + class-name lookup per call just to fail
    // for every Integer/Long/etc. key). (A Thread-mirror check that stood
    // between them was removed by gc-common w29-a.)
    if let (Some(pa), Some(pb)) = (unbox_wrapper(ctx, a), unbox_wrapper(ctx, b)) {
        // JDK contract: every wrapper's `equals(Object)` opens with an
        // `instanceof` of its OWN type, so two wrappers of DIFFERENT classes
        // are never equal. Without this guard the unboxed comparison below
        // conflated key types that real `HashMap` keeps apart, because
        // `unbox_wrapper` maps Integer/Short/Byte/Character/Boolean onto the
        // same `Value::Int` and the Int-vs-Long arms cross-compare explicitly:
        //
        //   Map<Object,String> m = new HashMap<>();
        //   m.put(Integer.valueOf(1), "int");
        //   m.put(Long.valueOf(1L),   "long");   // JDK: 2 entries
        //
        // `Integer.hashCode()==1` and `Long.hashCode()==1`, so both keys land
        // in the same bucket; the chain walk then found the Integer node
        // "equal" and OVERWROTE it -> size()==1 and get(1) == "long". Same for
        // Integer.valueOf(65) vs Character.valueOf('A') (both hash 65) and
        // Boolean.TRUE vs Integer.valueOf(1) (Boolean unboxes to Int(1)).
        //
        // Deliberately compared by ClassId, not by class name: it is a
        // heap-header read, with no `class_manager` lock and no `String`
        // allocation, on the hottest comparison in the map natives. The
        // same-class Int-vs-Long arms are KEPT: within one wrapper class they
        // only paper over a representational difference in how the VM stored
        // the value field, never over two genuinely distinct Java keys.
        if ctx.class_id_of_object(a) != ctx.class_id_of_object(b) {
            return Ok(false);
        }
        return Ok(wrapper_prims_equal(pa, pb));
    }
    // String value equality. Wrappers are checked first because the type sets
    // are disjoint and primitive-wrapper maps avoid two failed String checks.
    if let Some(equal) = ctx.java_strings_equal(a, b) {
        return Ok(equal);
    }
    // gc-common w29-a (`common-w28b-remaining-identity-hash-keyed-side-tables`
    // rank 13): an arm here treated two `java.lang.Thread` keys with one
    // identity hash as EQUAL, to paper over a stale mirror address in
    // RandomizedTesting's `WeakHashMap<Thread, ..>`. Identity hashes are 32
    // bits from a per-heap counter that wraps, so two distinct LIVE threads
    // were one key in every native map: `contains` answered for the wrong
    // thread and `remove` dropped the other thread's entry. Removed: a Thread
    // is equal only to itself (`Object.equals`, below), and a caller holding
    // a stale Thread address is a stale-reference bug the stale-handle /
    // stale-receiver audits gate, not something equality may guess at.
    //
    // Enum constants: compare by (declaring class, ordinal). `Enum.equals` is
    // `final` identity, so a correct VM never reaches here for two non-equal
    // enum objects; but if the VM produced a duplicate object for the same
    // constant, identity comparison spuriously fails. (class, ordinal) is the
    // JLS-canonical identity and cannot collide across distinct constants.
    // See `enum_key_identity`.
    if let (Some(ia), Some(ib)) = (enum_key_identity(ctx, a), enum_key_identity(ctx, b)) {
        return Ok(ia == ib);
    }
    // nio20 wave 6, L5: `a.equals(b)` of an identity class is `a == b`, and
    // the identity test opened this function ([`CF_IDENTITY_KEY`]).
    if identity_key_hash_enabled() && receiver_facts(ctx, a).has(CF_IDENTITY_KEY) {
        return Ok(false);
    }
    // S111r-bug-fix (peaceful-sammet): fall back to the user-defined
    // `equals(Object)` so HashMap honours the equals/hashCode contract for
    // arbitrary key types. See `map_hash_key` for the matching contract
    // commentary and the Spring `AnnotationTypeMapping.aliasedBy` symptom.
    //
    // MED fix: thrown exceptions from `equals(Object)` propagate via `?` —
    // previously they were silently mapped to `false`, which combined with
    // the matching swallow in `map_hash_key` meant `put` succeeded but
    // `get` always returned null.
    let res = ctx.invoke_virtual(
        a,
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(b))],
    );
    if dbg_hm_trace() {
        eprintln!("[HM-EQ] invoke_virtual(equals) -> {:?}", res);
    }
    match res? {
        Some(Value::Int(v)) => Ok(v != 0),
        _ => Ok(false),
    }
}

/// Equality of two UNBOXED values of the SAME wrapper class, as that class's
/// `equals` decides it. Split out of [`map_keys_equal`] (r9w11 chm11) so the
/// no-Java `ConcurrentHashMap` wrapper-key walk answers with the very same
/// arms; the caller owns the same-class guard.
#[inline]
pub(crate) fn wrapper_prims_equal(pa: Value, pb: Value) -> bool {
    match (pa, pb) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Long(x), Value::Long(y)) => x == y,
        // `Float.equals`/`Double.equals` compare floatToIntBits /
        // doubleToLongBits, NOT `==`. Two consequences that plain `==` got
        // backwards, both silent-wrong-answer:
        //   * NaN equals itself, so `m.put(Double.NaN, v)` followed by
        //     `m.get(Double.NaN)` MUST find the entry; with `==` the key
        //     could never be looked up again (get/remove/containsKey all
        //     returned "absent" for a key the map demonstrably held).
        //   * +0.0 does NOT equal -0.0, so they are two distinct keys;
        //     with `==` the second put silently overwrote the first.
        // `is_nan() || to_bits()` reproduces floatToIntBits exactly,
        // including its canonicalisation of every NaN payload.
        (Value::Float(x), Value::Float(y)) => cratonvm_types::jfp::float_equals(x, y),
        (Value::Double(x), Value::Double(y)) => cratonvm_types::jfp::double_equals(x, y),
        (Value::Int(x), Value::Long(y)) => (x as i64) == y,
        (Value::Long(x), Value::Int(y)) => x == (y as i64),
        _ => false,
    }
}

/// Get the bucket index for a given hash and capacity.
pub(crate) fn map_bucket_index(hash: i32, capacity: i32) -> usize {
    // ONE rule, with a fast path — not two rules.
    //
    // The general form is the JDK `Hashtable`'s: `(hash & 0x7FFFFFFF) % len`.
    // For a power-of-two length that is ARITHMETICALLY IDENTICAL to
    // `hash & (len - 1)`, because `len - 1` has no bit 31 set, so masking the
    // sign bit first cannot change any bit the AND keeps. The branch below is
    // therefore a division-elimination optimisation and nothing else: no map
    // changes bucket for it, and the existing `map_bucket_index_power_of_two`
    // test still pins the power-of-two answers.
    //
    // Why the general arm has to exist: `java.util.Hashtable` sizes its table
    // 11, 23, 47, 95, … — `(oldCapacity << 1) + 1`, never a power of two — and
    // MEASURED against HotSpot 25.0.3+9 this VM had it at 16 and 64. Making
    // the RULE follow the table's actual length, rather than teaching every
    // one of the 34 call sites which family its receiver belongs to, keeps the
    // reader and the writer of a bucket index in agreement by construction:
    // a table allocated at a non-power-of-two length is indexed modulo that
    // length everywhere, including the `alloc_bucket_table` low-memory
    // fallback that silently substitutes a different size.
    let cap = capacity as u32;
    if cap != 0 && cap.is_power_of_two() {
        return ((hash as u32) & (cap - 1)) as usize;
    }
    if cap == 0 {
        return 0;
    }
    (((hash as u32) & 0x7FFF_FFFF) % cap) as usize
}

/// `java.util.Hashtable`'s default table length, and the one number in this
/// file that is deliberately not [`MAP_DEFAULT_CAPACITY`].
///
/// `new Hashtable()` is `this(11, 0.75f)` in JDK 25, and 11 * 0.75 = 8.25
/// truncates to the threshold 8 that [`ensure_hashtable_load_factor`] already
/// writes — so the two constants were already consistent with each other and
/// only the array length was wrong.
pub(crate) const HASHTABLE_DEFAULT_CAPACITY: usize = 11;

/// The real JDK class a `java.util.HashMap.table` is an array of.
///
/// The `ConcurrentHashMap` twin is [`CHM_NODE_CLASS`], and the two are resolved
/// by the same helper for the same reason — see [`hm_node_class_id`].
pub(crate) const HM_NODE_CLASS: &str = "java/util/HashMap$Node";

// ---------------------------------------------------------------------------
// Tree bins for the `--compatible` native `HashMap` (round 13 wave 11, lane
// compat6; `r13w4-hashcompat-native-hashmap-never-treeifies-FIXED-20260929.md`).
// ---------------------------------------------------------------------------
//
// `HashMap.putVal` converts a bin of 8 nodes into a red-black tree of
// `HashMap$TreeNode`s once the table has 64 slots, and every later lookup,
// insert, removal and resize of that bin goes through the tree. The natives
// only ever built `next` chains, so `equals` / `compareTo` counts, the
// treeified bin's iteration order (root first) and the O(log n) cost on
// colliding keys all differed from the JDK.
//
// The tree is not re-implemented here. Every TREE operation is the JDK's own
// `HashMap` / `HashMap$TreeNode` bytecode, run through
// `invoke_special_bytecode_only` on the receiver's real `table` and real nodes
// (`getTreeNode`, `putTreeVal`, `removeTreeNode`, `split`, `untreeify`,
// `treeifyBin`, `removeNode`), so the comparison order, the tie-break rules
// and the rebalancing are HotSpot's by construction. Only BIN-LOCAL methods
// are called: never `putVal` or `resize()`, which would also rebuild bins the
// natives own (a table can hold primitive-valued carrier nodes that a typed
// `Node[]` from `resize()` refuses). The natives keep deciding WHEN: the
// chain walks, the growth points and the accounting stay theirs, each moved
// to the JDK's order where a tree is involved.
//
// The invariant every native writer of a `HashMap` bin keeps while the switch
// is on: a bin whose head is a `TreeNode` is changed only through the tree
// methods above (or turned back into a chain with `untreeify` first). The
// writers are `native_map_put_evict_pinned_inner`, `native_map_remove_pinned
// _hashed`, `map_unlink_node_movable`, `map_remove_node_by_walk`,
// `map_resize_inner`, `map_link_new_node_at_head` (through
// `hm_remap_insert`), `map_append_distinct` (untreeifies first) and
// `native_map_clear`; the view builders that link nodes themselves
// (`native_map_entry_set`, `resync_view_set_inner`, ...) write only view
// backings, which never treeify ([`hm_tree_may_treeify`]). `LinkedHashMap`
// keeps the same invariant under its own switch (the `LinkedHashMap` section
// below). `Hashtable`, `IdentityHashMap` and the CHM segments are outside it:
// their natives never create a tree.

/// The JDK's tree-bin node class. It extends `LinkedHashMap$Entry`, which
/// extends `HashMap$Node`, so slots 0-3 are still `hash`/`key`/`value`/`next`
/// (`NODE_FIELD_*`) and every `next`-walking reader keeps seeing every entry.
pub(crate) const HM_TREE_NODE_CLASS: &str = "java/util/HashMap$TreeNode";

/// `HashMap.TREEIFY_THRESHOLD`.
pub(crate) const HM_TREEIFY_THRESHOLD: usize = 8;

/// `HashMap.MIN_TREEIFY_CAPACITY`: below it `treeifyBin` resizes instead.
pub(crate) const HM_MIN_TREEIFY_CAPACITY: i32 = 64;

pub(crate) const HM_GET_TREE_NODE_DESC: &str = "(ILjava/lang/Object;)Ljava/util/HashMap$TreeNode;";
pub(crate) const HM_PUT_TREE_VAL_DESC: &str = "(Ljava/util/HashMap;[Ljava/util/HashMap$Node;ILjava/lang/Object;Ljava/lang/Object;)Ljava/util/HashMap$TreeNode;";
pub(crate) const HM_REMOVE_TREE_NODE_DESC: &str = "(Ljava/util/HashMap;[Ljava/util/HashMap$Node;Z)V";
pub(crate) const HM_SPLIT_DESC: &str = "(Ljava/util/HashMap;[Ljava/util/HashMap$Node;II)V";
pub(crate) const HM_UNTREEIFY_DESC: &str = "(Ljava/util/HashMap;)Ljava/util/HashMap$Node;";
pub(crate) const HM_TREEIFY_BIN_DESC: &str = "([Ljava/util/HashMap$Node;I)V";
pub(crate) const HM_REMOVE_NODE_DESC: &str =
    "(ILjava/lang/Object;Ljava/lang/Object;ZZ)Ljava/util/HashMap$Node;";

/// How [`map_chain_find_from`] searches a tree bin.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TreeLookup {
    /// Walk `next` only (no tree protocol for this receiver or switch off).
    Off,
    /// `getNode` / `removeNode`: test the first node, then `getTreeNode`.
    AfterFirst,
    /// `compute` / `computeIfAbsent` / `merge`: `getTreeNode` on the head.
    Direct,
}

impl TreeLookup {
    /// `mode` for a receiver the tree protocol covers ([`hm_tree_family`]),
    /// [`TreeLookup::Off`] otherwise.
    pub(crate) fn for_receiver(ctx: &dyn NativeContext, this: ObjectRef, mode: TreeLookup) -> TreeLookup {
        if hm_tree_family(ctx, this) {
            mode
        } else {
            TreeLookup::Off
        }
    }
}

/// `CRATONVM_COMPAT_HASHMAP_TREE_BINS` — default ON, `=0` restores the
/// chain-only natives. See the section comment above.
pub(crate) fn hm_tree_bins_enabled() -> bool {
    pub(crate) static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_HASHMAP_TREE_BINS")
}

/// Whether `node` is a `HashMap$TreeNode` (one memoized header read).
#[inline]
pub(crate) fn is_hm_tree_node(ctx: &dyn NativeContext, node: ObjectRef) -> bool {
    receiver_facts(ctx, node).has(CF_HM_TREE_NODE)
}

/// The bin head `head` when it is a tree bin's (`first instanceof TreeNode`).
#[inline]
pub(crate) fn hm_tree_head(ctx: &dyn NativeContext, head: Value) -> Option<ObjectRef> {
    match head {
        Value::Object(Some(h)) if is_hm_tree_node(ctx, h) => Some(h),
        _ => None,
    }
}

/// Whether the natives must treat a `TreeNode`-headed bin of `this` as a tree
/// (the switch is on and `this` is a `HashMap`, or a user subclass, on the
/// plain bucket layout -- not `LinkedHashMap`, `Hashtable`/`Properties`,
/// `IdentityHashMap`, CHM, `TreeMap` or a wrapper). Pure reads.
pub(crate) fn hm_tree_family(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    if !hm_tree_bins_enabled() {
        return false;
    }
    let facts = receiver_facts(ctx, this);
    facts.has(CF_BUCKET_MAP_NAME)
        && !facts.has(CF_LHM)
        && !facts.has(CF_CHM)
        && !facts.has(CF_TREE_MAP)
        && !facts.has(CF_IDENTITY_MAP)
        && !facts.has(CF_UNMOD_WRAPPER)
        && !facts.has(CF_HASHTABLE_ANCESTRY)
}

/// Whether an insert into `this` may TREEIFY a bin: [`hm_tree_family`], a real
/// `table` field, no integer-overlay row, and not a view backing (the view
/// builders link nodes into those themselves, outside the tree protocol).
/// Pure reads.
pub(crate) fn hm_tree_may_treeify(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    hm_tree_family(ctx, this)
        && receiver_table_slot(ctx, this).is_some()
        && view_backing_source(ctx, this).is_none()
        && hm_int_fast_len(ctx, this).is_none()
}

/// The number of nodes in the chain starting at `head`, and whether every one
/// of them is a plain `HashMap$Node` (what `treeifyBin` may convert: a
/// primitive-valued carrier node has no real `key`/`value` slots, and a
/// `TreeNode` means the bin is not a chain). A chain shorter than
/// `TREEIFY_THRESHOLD - 1` answers `(len, false)` without looking at classes;
/// otherwise the walk stops at the first node that is not plain (the count is
/// then partial), and a chain longer than `limit` (a cycle) answers
/// `(limit, false)`. Pure reads.
pub(crate) fn hm_chain_census(ctx: &dyn NativeContext, head: Value, limit: usize) -> (usize, bool) {
    chain_census_of(ctx, head, limit, CF_HM_NODE)
}

/// [`hm_chain_census`] for a family whose plain node class is `plain` (a
/// class-facts flag: [`CF_HM_NODE`] or [`CF_LHM_ENTRY`]).
pub(crate) fn chain_census_of(
    ctx: &dyn NativeContext,
    head: Value,
    limit: usize,
    plain: u32,
) -> (usize, bool) {
    // Every caller asks about a chain of at least `TREEIFY_THRESHOLD - 1`;
    // a shorter one (every ordinary bin) answers from a bare `next` count,
    // without a class lookup per node on the `put` path.
    let len = chain_len(ctx, head, limit);
    if len < HM_TREEIFY_THRESHOLD - 1 {
        return (len, false);
    }
    let mut count = 0usize;
    let mut cur = head;
    while let Value::Object(Some(n)) = cur {
        if count >= limit {
            return (limit, false);
        }
        count += 1;
        if !receiver_facts(ctx, n).has(plain) {
            return (count, false);
        }
        cur = ctx.get_field(n, NODE_FIELD_NEXT);
    }
    (count, true)
}

/// The number of nodes in the `next` chain starting at `head`, at most
/// `limit`. Pure reads.
pub(crate) fn chain_len(ctx: &dyn NativeContext, head: Value, limit: usize) -> usize {
    let mut count = 0usize;
    let mut cur = head;
    while let Value::Object(Some(n)) = cur {
        if count >= limit {
            break;
        }
        count += 1;
        cur = ctx.get_field(n, NODE_FIELD_NEXT);
    }
    count
}

thread_local! {
    /// Per-VM memo for [`hm_tree_classes_real`]: `(vm_identity, answer)`,
    /// scoped like [`HM_NODE_CLASS_MEMO`].
    pub(crate) static HM_TREE_CLASSES_MEMO: std::cell::Cell<Option<(usize, bool)>> =
        const { std::cell::Cell::new(None) };
}

/// Whether this image has the REAL `HashMap`, `HashMap$Node` and
/// `HashMap$TreeNode` (so the tree bytecode exists to run). A synthetic-JDK
/// image answers `false` and keeps chains. GC: the first call per VM per
/// thread can load a class; the caller holds its references pinned.
pub(crate) fn hm_tree_classes_real(ctx: &mut dyn NativeContext) -> bool {
    let vm = ctx.vm_identity();
    if let Some((cached_vm, answer)) = HM_TREE_CLASSES_MEMO.with(|c| c.get()) {
        if cached_vm == vm {
            return answer;
        }
    }
    let answer = hm_node_class_id(ctx).is_some()
        && chm_real_class(ctx, HM_TREE_NODE_CLASS).is_some()
        && !ctx.is_class_synthetic_stub("java/util/HashMap");
    HM_TREE_CLASSES_MEMO.with(|c| c.set(Some((vm, answer))));
    answer
}

/// `((TreeNode) node).getTreeNode(hash, key)`: the JDK's `find` from the root
/// of `node`'s tree (any node of the tree will do: `getTreeNode` climbs to the
/// root). `key` must be the current, pinned search key.
pub(crate) fn hm_tree_get_node(
    ctx: &mut dyn NativeContext,
    node: ObjectRef,
    hash: i32,
    key: Value,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let found = ctx.invoke_special_bytecode_only(
        HM_TREE_NODE_CLASS,
        "getTreeNode",
        HM_GET_TREE_NODE_DESC,
        &[Value::Object(Some(node)), Value::Int(hash), key],
    )?;
    Ok(match found {
        Some(Value::Object(Some(n))) => Some(n),
        _ => None,
    })
}

/// `((TreeNode) first).putTreeVal(this, tab, hash, key, value)`: the existing
/// node for `key` (nothing changed), or `None` when a new `TreeNode` was
/// linked into the tree and its `next` list. The caller accounts the insert.
pub(crate) fn hm_tree_put_tree_val(
    ctx: &mut dyn NativeContext,
    first: ObjectRef,
    this: ObjectRef,
    tab: ObjectRef,
    hash: i32,
    key: Value,
    value: Value,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let found = ctx.invoke_special_bytecode_only(
        HM_TREE_NODE_CLASS,
        "putTreeVal",
        HM_PUT_TREE_VAL_DESC,
        &[
            Value::Object(Some(first)),
            Value::Object(Some(this)),
            Value::Object(Some(tab)),
            Value::Int(hash),
            key,
            value,
        ],
    )?;
    Ok(match found {
        Some(Value::Object(Some(n))) => Some(n),
        _ => None,
    })
}

/// `removeNode(hash, key, value, matchValue, movable)` as `HashMap`'s own
/// bytecode runs it: the first-node test, `getTreeNode`, `removeTreeNode`,
/// `++modCount`, `--size`. Used for a bin whose head is a `TreeNode`. Answers
/// the removed node.
#[allow(clippy::too_many_arguments)]
pub(crate) fn hm_remove_node_bytecode(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    hash: i32,
    key: Value,
    value: Value,
    match_value: bool,
    movable: bool,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let removed = ctx.invoke_special_bytecode_only(
        "java/util/HashMap",
        "removeNode",
        HM_REMOVE_NODE_DESC,
        &[
            Value::Object(Some(this)),
            Value::Int(hash),
            key,
            value,
            Value::Int(i32::from(match_value)),
            Value::Int(i32::from(movable)),
        ],
    )?;
    Ok(match removed {
        Some(Value::Object(Some(n))) => Some(n),
        _ => None,
    })
}

/// Unlink the `TreeNode` `node` from `this` by identity:
/// `node.removeTreeNode(this, tab, movable)` (which relinks `next`/`prev`,
/// rebalances, and untreeifies a bin that became too small), then the
/// `size - 1` / `modCount` accounting [`map_unlink_node_movable`] does.
/// `false`, having changed nothing, when the node is no longer in its bin
/// (`removeTreeNode` requires it to be). Runs JDK bytecode, which can collect.
pub(crate) fn hm_tree_unlink_node(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    node: ObjectRef,
    movable: bool,
) -> bool {
    let (buckets, size, cap) = map_state(ctx, this);
    let Some(tab) = buckets else {
        return false;
    };
    let idx = map_bucket_index(get_node_hash(ctx, node), cap);
    let mut cur = ctx.get_array_element(tab, idx);
    let mut present = false;
    for _ in 0..chain_walk_limit(size) {
        let Value::Object(Some(c)) = cur else {
            break;
        };
        if std::ptr::eq(c.as_ptr(), node.as_ptr()) {
            present = true;
            break;
        }
        cur = ctx.get_field(c, NODE_FIELD_NEXT);
    }
    if !present {
        return false;
    }
    let this_pin = ctx.pin_native_root(this);
    let unlinked = ctx.invoke_special_bytecode_only(
        HM_TREE_NODE_CLASS,
        "removeTreeNode",
        HM_REMOVE_TREE_NODE_DESC,
        &[
            Value::Object(Some(node)),
            Value::Object(Some(this)),
            Value::Object(Some(tab)),
            Value::Int(i32::from(movable)),
        ],
    );
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    if unlinked.is_err() {
        return false;
    }
    set_map_size(ctx, this, size - 1);
    bump_map_mod_count(ctx, this);
    true
}

/// Turn the tree bin at `idx` of `this`'s table back into a chain of plain
/// nodes in the same order (`tab[idx] = first.untreeify(this)`), for a native
/// writer that has to link something a tree cannot hold (a primitive-valued
/// carrier node). A no-op for a bin that is not a tree. Allocates.
pub(crate) fn hm_untreeify_bin(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    idx: usize,
) -> Result<(), MethodCallFailed> {
    let Some(tab) = map_state(ctx, this).0 else {
        return Ok(());
    };
    untreeify_bin_at(ctx, this, tab, idx)
}

/// [`hm_untreeify_bin`] over an explicit table `tab` of `this` (a
/// `LinkedHashMap`'s comes from [`lhm_state`]; its `replacementNode` keeps the
/// insertion-order links, `transferLinks`).
pub(crate) fn untreeify_bin_at(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    tab: ObjectRef,
    idx: usize,
) -> Result<(), MethodCallFailed> {
    let head = ctx.get_array_element(tab, idx);
    let Some(first) = hm_tree_head(&*ctx, head) else {
        return Ok(());
    };
    let tab_pin = ctx.pin_native_root(tab);
    let plain = ctx.invoke_special_bytecode_only(
        HM_TREE_NODE_CLASS,
        "untreeify",
        HM_UNTREEIFY_DESC,
        &[Value::Object(Some(first)), Value::Object(Some(this))],
    );
    let tab = ctx.read_native_pin(tab_pin, tab);
    ctx.unpin_native_roots(tab_pin);
    let plain = plain?.unwrap_or(Value::Object(None));
    ctx.set_array_element(tab, idx, plain);
    Ok(())
}

/// `treeifyBin(tab, hash)` for the bin `hash` names in `this`'s current table:
/// below [`HM_MIN_TREEIFY_CAPACITY`] slots the JDK resizes instead, which is
/// the native [`map_resize`] here (its split is `resize()`'s); otherwise the
/// JDK's `treeifyBin` bytecode converts the bin in place. The caller has
/// checked the bin with [`hm_chain_census`]. `this` is refreshed.
pub(crate) fn hm_treeify_bin(
    ctx: &mut dyn NativeContext,
    this: &mut ObjectRef,
    hash: i32,
    grow: GrowAlloc,
) -> Result<(), MethodCallFailed> {
    let (buckets, _, cap) = map_state(ctx, *this);
    let Some(tab) = buckets else {
        return Ok(());
    };
    if cap < HM_MIN_TREEIFY_CAPACITY {
        map_resize(ctx, this, grow);
        return Ok(());
    }
    let this_pin = ctx.pin_native_root(*this);
    let treeified = ctx.invoke_special_bytecode_only(
        "java/util/HashMap",
        "treeifyBin",
        HM_TREEIFY_BIN_DESC,
        &[
            Value::Object(Some(*this)),
            Value::Object(Some(tab)),
            Value::Int(hash),
        ],
    );
    *this = ctx.read_native_pin(this_pin, *this);
    ctx.unpin_native_roots(this_pin);
    treeified.map(|_| ())
}

// --- `LinkedHashMap` tree bins -------------------------------------------
//
// The same protocol over a `LinkedHashMap`'s table. Its tree nodes are the
// same `HashMap$TreeNode` (which extends `LinkedHashMap$Entry`, so
// `LHM_NODE_BEFORE`/`_AFTER` are its slots 4/5 too), and the JDK's own
// `LinkedHashMap` overrides keep the insertion-order list right: `newTreeNode`
// links the new node at the list's end (`linkNodeAtEnd`), `replacementTreeNode`
// / `replacementNode` hand a replaced node's list links over
// (`transferLinks`), and `removeNode` runs `afterNodeRemoval`. They write the
// REAL `head` / `tail` fields, which [`lhm_get`] reads first for every map
// whose structural fields the natives mirror there (every real
// `LinkedHashMap`); a map without them never treeifies
// ([`lhm_tree_may_treeify`]). The natives keep the list for everything that
// is not a tree operation, as before.

/// The real JDK class of a `LinkedHashMap`'s plain bucket nodes.
pub(crate) const LHM_ENTRY_CLASS: &str = "java/util/LinkedHashMap$Entry";

/// `CRATONVM_COMPAT_LHM_TREE_BINS` — default ON, `=0` keeps `LinkedHashMap`
/// bins chains (and it is off whenever `CRATONVM_COMPAT_HASHMAP_TREE_BINS` is).
pub(crate) fn lhm_tree_bins_enabled() -> bool {
    pub(crate) static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    hm_tree_bins_enabled() && compat_switch_on(&ON, "CRATONVM_COMPAT_LHM_TREE_BINS")
}

/// Whether the natives must treat a `TreeNode`-headed bin of `this` as a tree:
/// the switches are on and `this` is a `LinkedHashMap` (or subclass). Pure
/// reads.
pub(crate) fn lhm_tree_family(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    lhm_tree_bins_enabled() && is_lhm_receiver(ctx, this)
}

/// Whether an insert into the `LinkedHashMap` `this` may treeify a bin: it
/// has the real `table`, `head` and `tail` fields the tree bytecode reads and
/// writes, and it is not a `reversed()` snapshot (rebuilt by appending nodes
/// itself, [`lhm_append_distinct`]). Pure reads.
pub(crate) fn lhm_tree_may_treeify(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    if !lhm_tree_family(ctx, this) || receiver_table_slot(ctx, this).is_none() {
        return false;
    }
    let nf = ctx.object_num_fields(this);
    let linked = ["head", "tail"].iter().all(|name| {
        ctx.resolve_field_index("java/util/LinkedHashMap", name)
            .is_some_and(|slot| slot < nf)
    });
    linked
        && !matches!(
            lhm_get(ctx, this, LHM_REVERSED_SOURCE, 0),
            Value::Object(Some(_))
        )
}

/// [`hm_treeify_bin`] for a `LinkedHashMap`: below
/// [`HM_MIN_TREEIFY_CAPACITY`] slots the native [`lhm_resize`], otherwise the
/// JDK's `treeifyBin` (whose `replacementTreeNode` is `LinkedHashMap`'s). The
/// caller holds `this` pinned and re-reads it.
pub(crate) fn lhm_treeify_bin(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    hash: i32,
    grow: GrowAlloc,
) -> Result<(), MethodCallFailed> {
    let (buckets, _, cap) = lhm_state(ctx, this);
    let Some(tab) = buckets else {
        return Ok(());
    };
    if cap < HM_MIN_TREEIFY_CAPACITY {
        lhm_resize(ctx, this, grow);
        return Ok(());
    }
    ctx.invoke_special_bytecode_only(
        "java/util/HashMap",
        "treeifyBin",
        HM_TREEIFY_BIN_DESC,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(tab)),
            Value::Int(hash),
        ],
    )
    .map(|_| ())
}

/// After `HashMap` bytecode changed a `LinkedHashMap`'s REAL `size`
/// (`removeNode`: `--size`, `++modCount`), write the same count through
/// [`lhm_set`], so the overlay's copy -- which decides whether the next native
/// size write moves `modCount` -- agrees with it again. (The extra `modCount`
/// step is not observable: only its movement is.)
pub(crate) fn lhm_sync_size_after_bytecode(ctx: &mut dyn NativeContext, this: ObjectRef) {
    if let Value::Int(real) = lhm_get(&*ctx, this, "size", LHM_FIELD_SIZE) {
        lhm_set(ctx, this, "size", LHM_FIELD_SIZE, Value::Int(real));
    }
}

/// [`hm_tree_unlink_node`] for a `LinkedHashMap`: `removeTreeNode`, then the
/// insertion-order unlink and the `size - 1` [`lhm_unlink_node`] does.
/// `false`, having changed nothing, when the node is no longer in its bin.
/// Runs JDK bytecode, which can collect.
pub(crate) fn lhm_tree_unlink_node(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    node: ObjectRef,
    movable: bool,
) -> bool {
    let (buckets, size, cap) = lhm_state(ctx, this);
    let Some(tab) = buckets else {
        return false;
    };
    let idx = map_bucket_index(get_node_hash(ctx, node), cap);
    let mut cur = ctx.get_array_element(tab, idx);
    let mut present = false;
    for _ in 0..chain_walk_limit(size) {
        let Value::Object(Some(c)) = cur else {
            break;
        };
        if std::ptr::eq(c.as_ptr(), node.as_ptr()) {
            present = true;
            break;
        }
        cur = ctx.get_field(c, LHM_NODE_NEXT);
    }
    if !present {
        return false;
    }
    let this_pin = ctx.pin_native_root(this);
    let node_pin = ctx.pin_native_root(node);
    let unlinked = ctx.invoke_special_bytecode_only(
        HM_TREE_NODE_CLASS,
        "removeTreeNode",
        HM_REMOVE_TREE_NODE_DESC,
        &[
            Value::Object(Some(node)),
            Value::Object(Some(this)),
            Value::Object(Some(tab)),
            Value::Int(i32::from(movable)),
        ],
    );
    let this = ctx.read_native_pin(this_pin, this);
    let node = ctx.read_native_pin(node_pin, node);
    ctx.unpin_native_roots(this_pin);
    if unlinked.is_err() {
        return false;
    }
    lhm_unlink(ctx, this, node);
    lhm_set(ctx, this, "size", LHM_FIELD_SIZE, Value::Int(size - 1));
    true
}

/// The real JDK class a `java.util.Hashtable.table` is an array of, and the
/// class of every node in its chains.
///
/// MEASURED (`H23-2` §2, re-measured this lane on `vm-com2w-base` against
/// HotSpot 25.0.3+9, one case per process): a three-entry `Hashtable` on
/// HotSpot has `table.getClass() == [Ljava.util.Hashtable$Entry;` and nodes of
/// `java.util.Hashtable$Entry`; on CratonVM it had `[Ljava.lang.Object;` and
/// nodes of `java.util.HashMap$Node`. `Hashtable$Entry.isAssignableFrom(
/// HashMap$Node)` is **false on both VMs**, so the node half had to move first
/// — the same order `H16`/`H23` established for `HashMap`.
///
/// The two layouts are identical, which is why this is a pure class-identity
/// change and not a slot migration (`javap -p`, JDK 25):
///
/// ```text
///   java.util.HashMap$Node      final int hash; final K key; V value; HashMap$Node next
///   java.util.Hashtable$Entry   final int hash; final K key; V value; Hashtable$Entry next
/// ```
///
/// So `NODE_FIELD_HASH`/`_KEY`/`_VALUE`/`_NEXT` address the right slots on
/// either class and `get_node_key`'s `Int@0 = JDK layout` sniff still routes
/// correctly.
pub(crate) const HT_ENTRY_CLASS: &str = "java/util/Hashtable$Entry";

thread_local! {
    /// Per-VM memo for [`HM_NODE_CLASS`]'s `ClassId`: `(vm_identity, answer)`.
    ///
    /// The inner `Option` is the answer, and its `None` is a resolved NEGATIVE
    /// — this image has no real `java/util/HashMap$Node` — cached exactly like
    /// the positive. Not caching the negative would put a
    /// `class_id_by_name` + `ensure_class_initialized` + `class_name_of_id` on
    /// every `put` of every `synthetic-jdk` build, which is the cost this memo
    /// exists to remove.
    ///
    /// Scoped to `vm_identity` for the reason `RECEIVER_FACTS` is, and not for
    /// a weaker one: a Rust test can build several `Vm`s in one process and a
    /// `ClassId` minted by one is meaningless in another. A process-global
    /// `OnceLock` here would latch the FIRST VM's answer forever.
    pub(crate) static HM_NODE_CLASS_MEMO: std::cell::Cell<Option<(usize, Option<ClassId>)>> =
        const { std::cell::Cell::new(None) };

    /// Per-VM memo for [`HT_ENTRY_CLASS`]'s `ClassId`. Same shape, same
    /// negative-caching rule and same `vm_identity` scoping as
    /// [`HM_NODE_CLASS_MEMO`]; a separate cell rather than a map because there
    /// are exactly two of these and a `Cell` read is the whole hot path.
    pub(crate) static HT_ENTRY_CLASS_MEMO: std::cell::Cell<Option<(usize, Option<ClassId>)>> =
        const { std::cell::Cell::new(None) };
}

/// [`HM_NODE_CLASS`]'s `ClassId`, or `None` when this image has no real one.
///
/// Resolution goes through [`chm_real_class`] — already in this file, already
/// written against the `ensure-class-initialized-fabricates-instead-of-failing`
/// trap: an `Ok` from `ensure_class_initialized` is not evidence the image has
/// the class, so both `is_class_synthetic_stub` and the name the id resolves
/// BACK to are checked. Binding a node to a FABRICATED `HashMap$Node` would be
/// strictly worse than the untyped sentinel, because the sentinel at least
/// declares no descriptors.
///
/// GC: the first call per VM per thread can LOAD a class and therefore
/// allocate. Every caller must already hold its cross-allocation roots pinned.
/// Subsequent calls are a `Cell` read — no lock, no allocation, no Java.
pub(crate) fn hm_node_class_id(ctx: &mut dyn NativeContext) -> Option<ClassId> {
    let vm = ctx.vm_identity();
    if let Some((cached_vm, answer)) = HM_NODE_CLASS_MEMO.with(|c| c.get()) {
        if cached_vm == vm {
            return answer;
        }
    }
    let answer = chm_real_class(ctx, HM_NODE_CLASS);
    HM_NODE_CLASS_MEMO.with(|c| c.set(Some((vm, answer))));
    answer
}

/// [`HT_ENTRY_CLASS`]'s `ClassId`, or `None` when this image has no real one.
///
/// The `Hashtable` twin of [`hm_node_class_id`], and everything that doc says
/// about `chm_real_class` refusing a fabricated stand-in, about caching the
/// NEGATIVE, and about the GC contract applies here unchanged.
pub(crate) fn ht_entry_class_id(ctx: &mut dyn NativeContext) -> Option<ClassId> {
    let vm = ctx.vm_identity();
    if let Some((cached_vm, answer)) = HT_ENTRY_CLASS_MEMO.with(|c| c.get()) {
        if cached_vm == vm {
            return answer;
        }
    }
    let answer = chm_real_class(ctx, HT_ENTRY_CLASS);
    HT_ENTRY_CLASS_MEMO.with(|c| c.set(Some((vm, answer))));
    answer
}

/// The chain-node / bucket-table component class for `this`, as ONE decision.
///
/// [`map_node_class_for`] and [`bucket_table_component`] must never disagree:
/// the invariant the whole cluster turns on is
///
/// > **A table may be typed only if every node that can enter it is real.**
///
/// and the cheapest way to hold it is to derive both answers from one
/// function, so a receiver family added to one is added to the other by
/// construction. `H23-2` §3a's decline existed precisely because the two were
/// separate and only one half had moved.
///
/// Returns the sentinel `ClassId::new(0)` for a receiver whose table is
/// legitimately untyped (`IdentityHashMap`), whose table is built elsewhere
/// (`ConcurrentHashMap`, `Properties`), or on an image with no real class.
pub(crate) fn map_carrier_class_for_receiver(ctx: &mut dyn NativeContext, this: ObjectRef) -> ClassId {
    // `IdentityHashMap`: `Object[]` is correct, not a defect. It stores keys
    // and values FLAT in one array rather than in nodes, and HotSpot's own
    // table is `[Ljava.lang.Object;`. MEASURED identical on both VMs.
    if is_identity_map_receiver(ctx, this) {
        return ClassId::new(0);
    }
    // `ConcurrentHashMap`: its real table is `[Ljava/util/concurrent/
    // ConcurrentHashMap$Node;`, a DIFFERENT class, built and typed separately
    // by `chm_publish_real_table_pinned`. The arrays reached through the
    // generic path for a CHM receiver are its per-SEGMENT bucket tables — a
    // CratonVM-internal shape with no HotSpot counterpart — so typing them
    // `HashMap$Node` would be INVENTING a component type, not restoring one.
    if is_chm_receiver(ctx, this) {
        return ClassId::new(0);
    }
    // `Hashtable` and its ordinary subclasses. `uses_native_hashtable_layout`
    // deliberately EXCLUDES `Properties`, which JDK 25 backs with a separate
    // `ConcurrentHashMap` and which this VM backs with a Rust side-table: a
    // `Properties` bucket table holds no nodes at all (MEASURED: `occupied=0`
    // with `size=2`), so there is nothing for a component type to describe and
    // HotSpot leaves the inherited `table` field null.
    if uses_native_hashtable_layout(ctx, this) {
        return match ht_entry_class_id(ctx) {
            Some(cid) => cid,
            None => ClassId::new(0),
        };
    }
    if is_hashtable_receiver(ctx, this) {
        // `Properties` (or a subclass): side-table backed, table stays untyped.
        return ClassId::new(0);
    }
    // `HashMap`, and `LinkedHashMap` — which shares `HashMap`'s INHERITED
    // `table` field, so its component is `HashMap$Node` and not
    // `LinkedHashMap$Entry`. Its nodes are a subclass, so the store is
    // covariant-legal; MEASURED on both VMs (`H23-2` §4a).
    match hm_node_class_id(ctx) {
        Some(cid) => cid,
        None => ClassId::new(0),
    }
}

/// Which class the ordinary `put` path's chain node is allocated with.
///
/// # Why this is not unconditionally the real class
///
/// A real `java.util.HashMap$Node` declares `key` and `value` as
/// `Ljava/lang/Object;`, and `coerce_field_value_by_descriptor`'s `b'L'` arm
/// turns a primitive written at a reference slot into NULL (`gc/src/heap.rs`,
/// and that rule is pinned by a test — it is not going to be relaxed). The
/// ordinary put path is reached with a non-`Object` `Value` from two directions
/// that are not hypothetical:
///
///   * `present_marker` keeps a legacy `Int(1)` for a NULL set element, because
///     a null element has no reference to mark itself with; and
///   * `materialize_hm_int_fast` re-puts every side-stored entry through
///     `native_map_put_evict_pinned` with the `Value` its writer supplied, and
///     `try_hm_int_fast_put` stores that `Value` verbatim. `jit_hashmap_put_direct`'s
///     own doc comment counts 42 direct Rust `native_map_put_pub` call sites
///     "that are not obliged to store an object".
///
/// So the split is the boundary between a **Java-visible** mapping and a
/// **Rust-private** one, not a compromise. A primitive `Value` in a map is
/// already unreadable from Java — `Map.get` returns `Ljava/lang/Object;` and
/// the same coercion nulls it at the return boundary — so no bytecode can tell
/// which class carries it, while every mapping bytecode CAN see is
/// object-shaped and gets the real node.
///
/// This is the same rule, and the same direction of error,
/// [`chm_publish_real_table`] already applies one screen over: *"a primitive in
/// a slot a real `$Node` declares as a reference … returns `None` and the
/// caller keeps today's snapshot carrier verbatim."*
///
/// # On inspecting `key`/`value` BEFORE the allocation
///
/// Only the `Value` VARIANT is read, never the address inside it. A moving
/// collection can relocate an `Object(Some(_))` but cannot turn it into an
/// `Int`, so deciding here and allocating after is sound with no refresh.
///
/// # On the RECEIVER argument
///
/// A node's class is not one constant: a `Hashtable` chain holds
/// `java.util.Hashtable$Entry`, not `java.util.HashMap$Node`, and
/// `Hashtable$Entry.isAssignableFrom(HashMap$Node)` is **false on both VMs**.
/// The receiver is therefore part of the question, and it is answered by
/// [`map_carrier_class_for_receiver`] — the SAME function
/// [`bucket_table_component`] calls, so the node and the array holding it can
/// no longer disagree by omission.
pub(crate) fn map_node_class_for(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key: Value,
    value: Value,
) -> ClassId {
    if !matches!(key, Value::Object(_)) || !matches!(value, Value::Object(_)) {
        return ClassId::new(0);
    }
    map_carrier_class_for_receiver(ctx, this)
}

/// The class a receiver's bucket table is an array **OF** — the other half of
/// [`map_node_class_for`].
///
/// # The two halves fail in opposite directions
///
/// `H16` made the NODES real. The array holding them stayed `Object[]`, because
/// `alloc_bucket_table` allocated it with the untyped-allocation sentinel. That
/// pair is wrong in a way neither half shows alone, and the two failure modes
/// point opposite ways:
///
///   * a FABRICATED node in a REAL `Node[]` throws `ArrayStoreException` — real
///     `HashMap.resize()` at `HashMap.java:719` is the site (`H0-6` §7); and
///   * a REAL node in an `Object[]` stores fine, but `table.getClass()` is
///     `[Ljava.lang.Object;` where HotSpot says `[Ljava.util.HashMap$Node;`,
///     and anything reading the component type is misled.
///
/// So node-class-first was the only safe order, and the invariant this function
/// exists to hold is:
///
/// > **A table may be typed only if every node that can enter it is real.**
///
/// [`chm_publish_real_table_pinned`] already implements that invariant for CHM,
/// all-or-nothing: any entry with a primitive key or value and it abandons the
/// publish rather than emit a typed array holding a fabrication.
///
/// # Why this is per-RECEIVER and not one constant
///
/// MEASURED on HotSpot 25.0.3+9, one case per process
/// (`scratchpad/h23/Family.java`) — the declared and runtime component type of
/// each family's `table`, and the class of its nodes:
///
/// ```text
///   HashMap           [Ljava/util/HashMap$Node;         HashMap$Node
///   LinkedHashMap     [Ljava/util/HashMap$Node;         LinkedHashMap$Entry  (a SUBCLASS)
///   Hashtable         [Ljava/util/Hashtable$Entry;      Hashtable$Entry
///   Properties        [Ljava/util/Hashtable$Entry;      (table lazily null)
///   IdentityHashMap   [Ljava/lang/Object;               keys/values stored FLAT
/// ```
///
/// Three consequences, and each one is a rule below:
///
///   * `LinkedHashMap` shares `HashMap`'s **inherited** `table` field, so one
///     component type serves both callers of [`alloc_bucket_table`]. Its
///     `LinkedHashMap$Entry` nodes are a subclass of `HashMap$Node`, so the
///     store is covariant-legal — `aastore_element_assignable`'s by-name
///     superclass walk resolves it, and `lhm_alloc_node` has bound to the real
///     `java/util/LinkedHashMap$Entry` since `7bf427af1`.
///   * `IdentityHashMap` is **legitimately** `Object[]` on HotSpot too — it
///     stores keys and values flat in one array rather than in nodes. The
///     sentinel is the RIGHT answer there and must not be "fixed".
///   * `Hashtable` wants `Hashtable$Entry`, and `H23-2` §3a DECLINED it because
///     this VM's Hashtable nodes were `HashMap$Node` — the armed hybrid, since
///     `HashMap$Node` is not a subclass of `Hashtable$Entry` by any route.
///     **That decline is now spent**: `map_carrier_class_for_receiver` moves the
///     node half and the array half together, so a `Hashtable` receiver gets
///     `Hashtable$Entry` in BOTH answers or in neither. `Properties` is still
///     declined, for a different reason — its entries live in a side-table and
///     its bucket array holds nothing at all.
///
/// # Both modes
///
/// Under `synthetic-jdk` there is no real `java/util/HashMap$Node`, so
/// [`hm_node_class_id`] answers `None` (a resolved NEGATIVE, memoised) and this
/// returns the sentinel — the table stays `Object[]`, exactly as today. That is
/// also what a real-JDK image returns if the class somehow will not resolve, so
/// the fallback is one path, not a mode switch.
///
/// GC: the first call per VM per thread can LOAD a class and therefore
/// allocate; callers must hold their cross-allocation roots pinned and re-read
/// `this` afterwards. Later calls are a `Cell` read.
pub(crate) fn bucket_table_component(ctx: &mut dyn NativeContext, this: ObjectRef) -> ClassId {
    map_carrier_class_for_receiver(ctx, this)
}

/// Replace a receiver's TYPED bucket table with an untyped `Object[]` of the
/// same length and contents, and republish it.
///
/// # Why this exists — `H23-2` §7.1, and the residual it declined to close
///
/// The invariant is *"a table may be typed only if every node that can enter it
/// is real"*. [`map_node_class_for`] holds it in one direction and cannot hold
/// it in the other: a mapping whose key or value is a non-`Object` `Value`
/// keeps the untyped carrier, because a real `$Node` declares both slots
/// `Ljava/lang/Object;` and `coerce_field_value_by_descriptor` would null the
/// primitive. So a Rust-private producer CAN, in principle, put a fabricated
/// node into a table that a Java-visible put already typed.
///
/// `H23-2` §4b measured that case at **0 in 14 shapes** from Java — every one
/// of `HashSet.add(null)`, boxed-`Integer` values, `int`->`int` maps and their
/// resizes yielded a real node — and §7.1 then declined to WRITE the repair,
/// on the grounds that an unexercised GC-sensitive reallocation is a worse risk
/// than the measured residual. That judgement was right about the risk and
/// wrong about the ceiling: `0 in 14` is not a proof, the remaining producers
/// are the 42 direct `native_map_put_pub` call sites that are *not obliged to
/// store an object*, and none of them is reachable from a Java probe.
///
/// # Why the risk argument is different now
///
/// The WORST case of this path is that a map's table reverts to `Object[]` —
/// which is exactly the state every map in this VM was in before `H23` landed,
/// i.e. a known-good configuration, not a novel one. It cannot lose a mapping:
/// every element is copied and the chain links are untouched. And it cannot
/// loop: the replacement array's header class id is the sentinel, so the
/// caller's `== want` test is false on the next put, forever.
///
/// GC: `new_ref_array` allocates. The caller passes its LIVE pins for `this`
/// and the table; both are re-read here and the caller must re-read its own
/// copies afterwards.
pub(crate) fn degrade_bucket_table_to_untyped(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
    buckets_pin: usize,
    buckets: ObjectRef,
    cap: i32,
) {
    let len = cap.max(0) as usize;
    let untyped = ctx.new_ref_array(ClassId::new(0), len);
    let untyped_pin = ctx.pin_native_root(untyped);
    let buckets = ctx.read_native_pin(buckets_pin, buckets);
    let untyped = ctx.read_native_pin(untyped_pin, untyped);
    for i in 0..len {
        // Only chain HEADS live in the array; a node linked through
        // `NODE_FIELD_NEXT` is reached from its head and is not copied here.
        let head = ctx.get_array_element(buckets, i);
        let untyped = ctx.read_native_pin(untyped_pin, untyped);
        ctx.set_array_element(untyped, i, head);
    }
    let this = ctx.read_native_pin(this_pin, this);
    let untyped = ctx.read_native_pin(untyped_pin, untyped);
    publish_map_table(ctx, this, untyped, cap);
    ctx.unpin_native_roots(untyped_pin);
}

/// Allocate a HashMap$Node entry using the REAL JDK field layout
/// (slot 0 = hash:I, slot 1 = key, slot 2 = value, slot 3 = next).
///
/// We allocate with `ClassId::new(0)` (untyped) rather than binding to the
/// real `java/util/HashMap$Node` class so no descriptor-aware coercion runs
/// on our writes — but the SLOT ORDER deliberately matches the real JDK so
/// that real-JDK bytecode walking `this.table[]` via `getfield
/// HashMap$Node.{hash,key,value,next}` reads the right slots. The critical
/// consumer is `HashMap.writeObject`→`internalWriteEntries` (Java
/// serialization), which is NOT on the force-native override list and so runs
/// real bytecode against our nodes; with the old synthetic order it read
/// `value` as the key and the `hash` Int (L-coerced to null) as the value,
/// serializing `{k=v}` as `{v=null}`. Slot 0 holds `Int(hash)`, so the
/// `get_node_key`/`get_node_value` layout-sniff (Object@0 = legacy,
/// Int@0 = JDK) routes our nodes through the JDK branch.
pub(crate) fn map_alloc_node(
    ctx: &mut dyn NativeContext,
    key: ObjectRef,
    value: Value,
    hash: i32,
    next: Option<ObjectRef>,
) -> Result<ObjectRef, MethodCallFailed> {
    // gcstress residual face-1 fix — `alloc_object` can move the `key`/`value`/
    // `next` object args; writing the bare (stale) refs into the node below
    // would store dangling references. Pin+re-read across the allocation.
    let key_pin = ctx.pin_native_root(key);
    let value_pin = pin_value(ctx, value);
    let next_pin = next.map(|n| ctx.pin_native_root(n));
    // Use a concrete node class: Object now correctly has zero writable fields.
    // Not `?` (gc-common w20-a): a refusal releases the three pins, whose base
    // is `key_pin`, instead of leaving them to the funnel's floor.
    let node = match try_alloc_synthetic(ctx, "java/util/HashMap$Node", NODE_NUM_FIELDS) {
        Ok(n) => n,
        Err(e) => {
            ctx.unpin_native_roots(key_pin);
            return Err(e);
        }
    };
    let key = ctx.read_native_pin(key_pin, key);
    let value = read_pinned_elem(ctx, value_pin, value);
    let next = next.map(|n| ctx.read_native_pin(next_pin.unwrap(), n));
    ctx.unpin_native_roots(key_pin);
    // A node's value slot is `V value` -> `Ljava/lang/Object;` on the real
    // `java/util/HashMap$Node` the line above binds to, so the
    // descriptor-aware `set_field` path coerces a primitive there to NULL
    // (`coerce_field_value_by_descriptor`, the `b'L'` arm).
    //
    // Every caller of this function is a view/snapshot set builder that passes
    // a raw `Value::Int(1)` PRESENT marker, so every one of those markers was
    // reaching the heap as null — ~1000 destructive writes in a program that
    // does nothing but iterate an entrySet. Nothing read them back (the
    // view branches of `native_hs_contains`/`native_hs_remove` resolve
    // membership against the SOURCE map's `containsKey`/`get`), so it was
    // invisible; it also drowned `CRATONVM_DBG=overlay` in benign noise, which
    // is how it went unnoticed.
    //
    // Use the node's own key as the marker. It is a reference, it is never
    // null here (every caller passes a freshly allocated entry), it costs no
    // allocation and no Java dispatch — and "the entry is present" is exactly
    // what a set's value slot means. A caller with a genuine value is
    // unaffected.
    let value = match value {
        Value::Object(_) => value,
        _ => Value::Object(Some(key)),
    };
    ctx.set_field(node, NODE_FIELD_KEY, Value::Object(Some(key)));
    ctx.set_field(node, NODE_FIELD_VALUE, value);
    ctx.set_field(node, NODE_FIELD_HASH, Value::Int(hash));
    ctx.set_field(node, NODE_FIELD_NEXT, Value::Object(next));
    Ok(node)
}

/// The value a `Set` stores in its backing map to mean "this element is
/// present".
///
/// It has to survive the round trip as NON-NULL, because `native_hs_add` /
/// `native_hs_remove` report membership from whether the previous value was
/// null — that is the whole encoding. A raw `Value::Int(1)` does not survive on
/// a node bound to a real JDK class: the value slot is `V value` ->
/// `Ljava/lang/Object;`, and `coerce_field_value_by_descriptor`'s `b'L'` arm
/// turns a primitive there into null, so `remove(x)` deletes the element and
/// still answers `false`. That is the defect `7bf427af1` fixed on the
/// LinkedHashMap side after it broke Jersey's `Resource.Builder`.
///
/// The element is its own marker: a reference, no allocation, no Java
/// dispatch, and exactly what a real `HashSet.PRESENT` stands in for.
///
/// A NULL element has no reference to use and keeps the legacy `Int(1)`, so
/// membership for that one element must not be read from the value —
/// `native_hs_add`/`native_hs_remove` settle it with `containsKey` instead.
pub(crate) fn present_marker(elem: Value) -> Value {
    match elem {
        Value::Object(Some(_)) => elem,
        _ => Value::Int(1),
    }
}

/// Where `java.util.HashSet.PRESENT` lives: `(HashSet's class id, its static
/// slot)`, or `None` when this VM has no such field.
///
/// Resolved rather than assumed, and read through `&dyn NativeContext` on
/// purpose: every call here is a pure read (`class_id_by_name`,
/// `static_field_index_by_name`, `get_static_field` are all `&self`), so it
/// **cannot allocate and cannot complete a moving GC**. That is what lets
/// [`hs_present_marker_at`] be called between a `pin_native_root` and its
/// `read_native_pin` without a refresh — see the loops in
/// [`make_hashset_with_elements`] and [`make_set_of`].
///
/// `None` on a synthetic-JDK build, where `java/util/HashSet` is a fabricated
/// carrier with no static block at all. Every caller falls back to
/// [`present_marker`], so that configuration is byte-identical to before.
pub(crate) fn hs_present_slot(ctx: &dyn NativeContext) -> Option<(ClassId, usize)> {
    let cid = ctx.class_id_by_name("java/util/HashSet")?;
    let idx = ctx.static_field_index_by_name(cid, "PRESENT")?;
    Some((cid, idx))
}

/// The value a **HashSet-family** receiver's backing map stores to mean "this
/// element is present", preferring the real `java.util.HashSet.PRESENT`.
///
/// [`present_marker`]'s "the element is its own marker" is sound for the
/// encoding CratonVM's own natives read — *previous value non-null* — and it is
/// **wrong for real `java.util.HashSet` bytecode**, which does not ask whether
/// the value is null. JDK 25 `HashSet` is:
///
/// ```java
///     public boolean add(E e)         { return map.put(e, PRESENT) == null; }
///     public boolean remove(Object o) { return map.remove(o) == PRESENT; }
/// ```
///
/// so `remove` is an **identity** test against one specific `Object`. Any other
/// non-null marker — the element itself, `Int(1)`, or the `Value::Object(None)`
/// two producers in this file were writing — makes that comparison false, and
/// real `HashSet.remove(x)` then deletes the element and answers `false`. That
/// is the same failure `7bf427af1` fixed on the LinkedHashMap side and that
/// [`try_native_hashset_remove`]'s doc comment already names; this function is
/// the other half of it, for the direction where the *native* writes the value
/// and *bytecode* reads it.
///
/// That direction is not hypothetical: it is exactly what
/// `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/HashSet` (and a permanent
/// retirement of these registrations) turns on.
///
/// Only the HashSet FAMILY needs this. A keySet/entrySet view carrier's real
/// bytecode is `HashMap$KeySet.remove`, which is
/// `HashMap.removeNode(...) != null` and never looks at the value — so
/// [`make_view_set_of`] and [`resync_view_set`] deliberately keep
/// [`present_marker`].
pub(crate) fn hs_present_marker_at(
    ctx: &dyn NativeContext,
    slot: Option<(ClassId, usize)>,
    elem: Value,
) -> Value {
    if let Some((cid, idx)) = slot {
        if let v @ Value::Object(Some(_)) = ctx.get_static_field(cid, idx) {
            return v;
        }
    }
    present_marker(elem)
}

/// [`hs_present_marker_at`] for a single write, resolving the slot itself.
/// Use the `_at` spelling in a loop so the name lookup is paid once.
pub(crate) fn hs_present_marker(ctx: &dyn NativeContext, elem: Value) -> Value {
    hs_present_marker_at(ctx, hs_present_slot(ctx), elem)
}

/// S111r26: Layout-aware node key reader.
///
/// The legacy synthetic layout stores: key=0, value=1, hash=2, next=3.
/// The real JDK HashMap$Node layout stores: hash=0, key=1, value=2, next=3.
///
/// Detect which layout is in use by checking slot 0:
///   - Object → legacy layout (slot 0 = key)
///   - Int    → JDK layout   (slot 0 = hash, key is at slot 1)
pub(crate) fn get_node_key(ctx: &dyn NativeContext, node: ObjectRef) -> Value {
    match ctx.get_field(node, 0) {
        v @ Value::Object(_) => v,   // legacy: slot 0 is the key
        _ => ctx.get_field(node, 1), // JDK:    slot 1 is the key
    }
}

/// S111r26: Layout-aware node value reader (see `get_node_key`).
pub(crate) fn get_node_value(ctx: &dyn NativeContext, node: ObjectRef) -> Value {
    match ctx.get_field(node, 0) {
        Value::Object(_) => ctx.get_field(node, 1), // legacy: value at slot 1
        _ => ctx.get_field(node, 2),                // JDK: value at slot 2 (our nodes)
    }
}

/// Read the spread hash stored in either supported node layout.
///
/// HashMap's chain predicate is `(node.hash == hash) && key.equals(node.key)`.
/// The full-hash comparison is semantically required even though the bucket
/// index already matched: different hashes routinely share the same low bits.
pub(crate) fn get_node_hash(ctx: &dyn NativeContext, node: ObjectRef) -> i32 {
    match ctx.get_field(node, 0) {
        Value::Int(hash) => hash, // JDK layout
        Value::Object(_) => match ctx.get_field(node, 2) {
            Value::Int(hash) => hash, // legacy synthetic layout
            _ => 0,
        },
        _ => 0,
    }
}

/// Maximum capacity for HashMap buckets (~1 billion).
pub(crate) const MAP_MAX_CAPACITY: i32 = 1 << 30;

// Bug 5 (HIGH) round-10: thread-local flag set while a CHM mutator
// (native_chm_put / put_if_absent / compute_* / merge / replace*) is on
// the stack. `map_resize` consults the flag — only CHM-segment resizes
// pay the striped-lock cost; plain HashMap resizes run lock-free.
thread_local! {
    pub(crate) static CHM_RESIZE_LOCK_NEEDED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// RAII guard that marks the current thread as being inside a CHM
/// mutator. While alive, `map_resize` calls performed by code below this
/// frame take the per-segment write lock against concurrent readers.
pub(crate) struct ChmResizeLockGuard {
    prev: bool,
}

impl ChmResizeLockGuard {
    pub(crate) fn enter() -> Self {
        let prev = CHM_RESIZE_LOCK_NEEDED.with(|c| {
            let p = c.get();
            c.set(true);
            p
        });
        ChmResizeLockGuard { prev }
    }
}

impl Drop for ChmResizeLockGuard {
    fn drop(&mut self) {
        let prev = self.prev;
        CHM_RESIZE_LOCK_NEEDED.with(|c| c.set(prev));
    }
}

/// Resize the HashMap when load factor is exceeded.
///
/// Whether the per-segment resize lock is taken is governed by the
/// thread-local `CHM_RESIZE_LOCK_NEEDED` flag, set by `ChmResizeLockGuard`
/// inside CHM mutator entry points. Plain `java/util/HashMap` callers
/// resize lock-free (HashMap is not thread-safe). CHM segment callers
/// take the write lock so concurrent CHM lock-free readers in
/// `chm_seg_get` serialize against the in-place NEXT mutations below.
///
/// The receiver is `&mut` because a resize ALLOCATES: `map_resize_inner` pins
/// `this`, re-reads it across `new_ref_array` and again across the rehash, and
/// then dropped that refreshed value at its own closing brace -- so every
/// caller was left holding a pre-move address (`WORKER-5-NOTE-12` §7; the same
/// species as `chm_publish_real_table`). Of the three call sites exactly one
/// re-read through its own pin and two did not. Taking `&mut` makes forgetting
/// a COMPILE ERROR rather than something the next audit has to find again.
/// `HashMap.resize()`'s closing line: `newThr = (int)(newCap * loadFactor)`.
///
/// Extracted so the arithmetic has a test that does not need a heap — the
/// same reason [`unmod_view_size`] is a free function. The write itself is one
/// `set_field` in [`map_resize_inner`]'s non-`Hashtable` arm, and it is the
/// half of `resize()` this VM never did.
///
/// Saturating rather than wrapping: `new_cap` is bounded by
/// `MAP_MAX_CAPACITY` well below the `i32` range, so the product cannot
/// overflow in practice, but a `f32` cast of a value that did would produce
/// an implementation-defined result rather than a clamp.
pub(crate) fn resize_threshold(new_cap: i32, load_factor: f32) -> i32 {
    if !load_factor.is_finite() || load_factor <= 0.0 {
        // A non-positive or NaN factor is not a threshold. `map_ctor_capacity
        // _load_check` already refuses these at the constructor; reaching here
        // means the field was written by bytecode this VM did not screen, and
        // the JDK's own default is the honest answer.
        return ((new_cap as f32) * 0.75_f32).max(0.0) as i32;
    }
    ((new_cap as f32) * load_factor).max(0.0) as i32
}

/// `CRATONVM_COMPAT_MAP_LOAD_FACTOR` — default ON, `=0` restores the old
/// sizing. When on, the native `HashMap`/`Hashtable` family sizes its table
/// the way the JDK does (round 13 wave 6, lane hashtree; page
/// `r13w4-hashcompat-compatible-maps-diverge-from-jdk-comparison-order-
/// 20260928.md` items 2-3):
///
/// * `HashMap(int, float)` / `Hashtable(int, float)` KEEP the load factor, and
///   `put` grows at the receiver's own `threshold` when that factor is not
///   0.75 ([`map_grow_threshold`]); `resize` maintains `threshold` with the
///   JDK's own rule ([`jdk_hashmap_threshold`], `oldThr << 1`,
///   [`jdk_hashtable_threshold`]);
/// * a lazily-constructed `HashMap(n)` materialises `tableSizeFor(n)`
///   buckets, not `max(n, 16)` (`HashMap.resize()`: `newCap = oldThr`);
/// * the lo/hi split rehash is only used from a power-of-two table: its
///   `hash & oldCap` partition is `%`-indexing's answer only there.
///
/// A factor of exactly 0.75 (every map that never passed one) takes the
/// arithmetic it always took, so the common map is unchanged.
pub(crate) fn map_load_factor_enabled() -> bool {
    pub(crate) static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_MAP_LOAD_FACTOR")
}

/// `HashMap.MAXIMUM_CAPACITY`.
pub(crate) const JDK_HASHMAP_MAXIMUM_CAPACITY: i32 = 1 << 30;

/// `HashMap.resize()`'s recomputed threshold: `ft = (float)newCap *
/// loadFactor; newThr = (newCap < MAXIMUM_CAPACITY && ft < (float)
/// MAXIMUM_CAPACITY ? (int)ft : Integer.MAX_VALUE)`. An infinite factor is
/// legal in the JDK (only `<= 0` and NaN are refused) and never grows.
pub(crate) fn jdk_hashmap_threshold(new_cap: i32, load_factor: f32) -> i32 {
    let ft = (new_cap as f32) * load_factor;
    if new_cap < JDK_HASHMAP_MAXIMUM_CAPACITY && ft < JDK_HASHMAP_MAXIMUM_CAPACITY as f32 {
        ft as i32
    } else {
        i32::MAX
    }
}

/// `Hashtable`'s threshold: `(int)Math.min(capacity * loadFactor,
/// MAX_ARRAY_SIZE + 1)` (constructor and `rehash()` alike). Rust's float to
/// int cast saturates, as Java's does.
pub(crate) fn jdk_hashtable_threshold(capacity: i32, load_factor: f32) -> i32 {
    let ft = (capacity as f32) * load_factor;
    ft.min((i32::MAX - 8) as f32 + 1.0) as i32
}

/// `CRATONVM_COMPAT_HASHMAP_COPY_PRESIZE` -- default ON, `=0` restores the
/// 16-bucket copies. Proposal C4W4-1 of `jit-r15w4-compat4-proposals-RETIRED-20261003.md`
/// (round 15 wave 4, lane compat5), page
/// `r15w4-compat4-hashmap-copy-table-size-FIXED-20260930.md`. The JDK sizes the
/// table of a map it fills from a known count BEFORE the first put:
///
/// * `HashMap(Map)` and `putAll` into a map whose `table` is still null --
///   `putMapEntries`: `t = ceil(s / loadFactor)`, `if (t > threshold)
///   threshold = tableSizeFor(t)` -- 2 buckets for one entry, 4 for 2-3, 8 for
///   4-6 ([`hashmap_presize_null_table`]);
/// * `HashSet(Collection)` -- `HashMap.newHashMap(max(c.size(), 12))`, sized
///   by the ASKED count, duplicates included (the dedup idiom `new
///   HashSet<>(listWithRepeats)`).
///
/// The natives built 16 buckets and grew per put, so a small copy (and a
/// deduplicated one) iterated in a different order: `new HashMap<>(Map.of(10,
/// "x", 3, "y"))` is `{10=x, 3=y}` on HotSpot and was `{3=y, 10=x}`
/// (`R15Compat4SmallCopyOrder` rows 0-2, 5). Only a plain `HashMap`-family map
/// with the real `table` / `threshold` fields is presized
/// ([`hashmap_copy_presize_applies`]); the table itself stays lazy, so the
/// first put allocates it from `threshold` exactly as `resize()` does, and the
/// integer overlay takes the same size as its `start_cap`. Needs
/// `CRATONVM_COMPAT_MAP_LOAD_FACTOR` (the lazy table's `threshold` is only
/// honoured under it).
pub(crate) fn hashmap_copy_presize_enabled() -> bool {
    pub(crate) static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_HASHMAP_COPY_PRESIZE")
}

/// The largest table [`hashmap_presize_null_table`] parks. An HONEST count
/// above it reaches the same table by per-put growth anyway (the growth rule
/// and `tableSizeFor(ceil(s / 0.75))` agree from 16 buckets up), so the bound
/// only declines to allocate what a lying `size()` asks for.
pub(crate) const HASHMAP_COPY_PRESIZE_MAX: i32 = 1 << 20;

/// Whether `map` is a `HashMap` the copy presize models: the switches on, a
/// `HashMap` (or a program subclass) that is not a `LinkedHashMap` (insertion
/// order; its own natives), with its own `table` and `threshold` fields in
/// bounds -- a fabricated 3-slot map keeps its eager 16. Pure reads.
pub(crate) fn hashmap_copy_presize_applies(ctx: &dyn NativeContext, map: ObjectRef) -> bool {
    if !hashmap_copy_presize_enabled() || !map_load_factor_enabled() {
        return false;
    }
    let facts = receiver_facts(ctx, map);
    facts.has(CF_BUCKET_MAP_NAME)
        && !facts.has(CF_LHM)
        && !facts.has(CF_HASHTABLE_ANCESTRY)
        && !facts.has(CF_CHM)
        && !facts.has(CF_TREE_MAP)
        && receiver_table_slot(ctx, map).is_some()
        && map_threshold_field(ctx, map).is_some()
}

/// `putMapEntries`' pre-size arm for a map whose `table` is null: `dt =
/// Math.ceil(s / (double) loadFactor)`, `t = dt < MAXIMUM_CAPACITY ? (int) dt
/// : MAXIMUM_CAPACITY`, `if (t > threshold) threshold = tableSizeFor(t)`. The
/// caller has established the null table ([`hashmap_copy_presize_applies`]
/// plus its own state test). `s <= 0` presizes nothing, as the JDK's `if (s >
/// 0)` does. No Java, no allocation.
pub(crate) fn hashmap_presize_null_table(ctx: &mut dyn NativeContext, map: ObjectRef, s: i32) {
    if s <= 0 {
        return;
    }
    let Some(threshold) = map_threshold_field(&*ctx, map) else {
        return;
    };
    let load_factor = map_custom_load_factor(&*ctx, map).unwrap_or(0.75);
    let t = putmap_entries_target(s, load_factor);
    if t > threshold {
        let cap = hashmap_table_size_for(t);
        if cap <= HASHMAP_COPY_PRESIZE_MAX {
            try_set_jdk_map_field(ctx, map, "threshold", Value::Int(cap));
        }
    }
}

/// `putMapEntries`' other pre-size arm, for a map whose table EXISTS: `while
/// (s > threshold && table.length < MAXIMUM_CAPACITY) resize();` before the
/// first put, `s` being the source's `size()` answer. JIT round 16 (lane
/// compatdrain), `r15w4-compat5-review-findings-FIXED-20261001.md` item 3: the
/// natives grew per put, which reaches the same table for an honest `s` (the
/// map never sits above its threshold, so `s > threshold` implies the final
/// size does too) but a smaller one than HotSpot's for a program map whose
/// `size()` over-reports -- a different iteration order. An integer-overlay
/// map ([`hm_int_fast_len`]) is materialised first, and only when `s` exceeds
/// the threshold its node table would have. Bounded by
/// [`HASHMAP_COPY_PRESIZE_MAX`] (a lie of `Integer.MAX_VALUE` is not worth a
/// 2^30-bucket table), and it stops if a resize does not grow the table (a
/// refused allocation). Under `CRATONVM_COMPAT_HASHMAP_COPY_PRESIZE`, for the
/// receivers [`hashmap_copy_presize_applies`] admits; the caller checks both.
pub(crate) fn hashmap_presize_live_table(
    ctx: &mut dyn NativeContext,
    (this_pin, this): (usize, ObjectRef),
    s: i32,
) {
    let mut current = ctx.read_native_pin(this_pin, this);
    let (_, _, cap0) = map_state(&*ctx, current);
    if cap0 <= 0 || s <= map_grow_threshold(&*ctx, current, cap0) {
        return;
    }
    if receiver_facts(&*ctx, current).has(CF_EXACT_HASHMAP)
        && hm_int_fast_len(&*ctx, current).is_some()
    {
        // A refused materialisation rolls itself back and leaves the overlay
        // answering; the puts then grow as before.
        if materialize_hm_int_fast(ctx, current).is_err() {
            return;
        }
        current = ctx.read_native_pin(this_pin, this);
    }
    loop {
        let (buckets, _, cap) = map_state(&*ctx, current);
        if buckets.is_none() || cap <= 0 || cap >= HASHMAP_COPY_PRESIZE_MAX {
            return;
        }
        if s <= map_grow_threshold(&*ctx, current, cap) {
            return;
        }
        map_resize(ctx, &mut current, GrowAlloc::SingleAttempt);
        current = ctx.read_native_pin(this_pin, this);
        let (_, _, grown) = map_state(&*ctx, current);
        if grown <= cap {
            return;
        }
    }
}

/// `putMapEntries`' `t`: `dt = Math.ceil(s / (double) loadFactor)`, `t = dt <
/// (double) MAXIMUM_CAPACITY ? (int) dt : MAXIMUM_CAPACITY` (the `float`
/// factor widened exactly, as Java widens it).
pub(crate) fn putmap_entries_target(s: i32, load_factor: f32) -> i32 {
    let dt = (f64::from(s) / f64::from(load_factor)).ceil();
    if dt < f64::from(JDK_HASHMAP_MAXIMUM_CAPACITY) {
        dt as i32
    } else {
        JDK_HASHMAP_MAXIMUM_CAPACITY
    }
}

/// Whether `putAll` finds `map` in the JDK's `table == null` state: no bucket
/// array, no entries, and no integer-overlay row (an overlay-backed map has
/// had a put, so its JDK table exists). Pure reads.
pub(crate) fn hashmap_table_is_null(ctx: &dyn NativeContext, map: ObjectRef) -> bool {
    let (buckets, size, _) = map_state(ctx, map);
    buckets.is_none()
        && size == 0
        && !(receiver_facts(ctx, map).has(CF_EXACT_HASHMAP) && hm_int_fast_len(ctx, map).is_some())
}

thread_local! {
    /// `(vm_identity, [(ClassId, loadFactor slot + 1, or 0 for none); 64])`,
    /// direct-mapped like `RECEIVER_FACTS`. [`map_custom_load_factor`] runs on
    /// every new-key `put`, and `resolve_field_index_by_class_id` is a
    /// `class_manager` read lock plus a hierarchy walk. Sound for the reason
    /// the facts memo is: a `ClassId` is never reissued and a linked class's
    /// field layout never changes, so a cached answer (present or absent)
    /// stays true for as long as any object of that class can be asked about.
    pub(crate) static LOAD_FACTOR_SLOT_MEMO: std::cell::RefCell<(Option<usize>, [(u32, u32); 64])> =
        const { std::cell::RefCell::new((None, [(u32::MAX, 0); 64])) };
}

/// The absolute slot of the `loadFactor` field `class_id` declares or
/// inherits, memoized per thread ([`LOAD_FACTOR_SLOT_MEMO`]).
pub(crate) fn load_factor_slot(ctx: &dyn NativeContext, class_id: ClassId) -> Option<usize> {
    let raw = class_id.as_u32();
    let vm = ctx.vm_identity();
    let index = facts_slot_index(raw) & 63;
    let hit = LOAD_FACTOR_SLOT_MEMO.with(|cell| {
        let mut memo = cell.borrow_mut();
        if memo.0 != Some(vm) {
            *memo = (Some(vm), [(u32::MAX, 0); 64]);
        }
        let (id, encoded) = memo.1[index];
        (id == raw).then_some(encoded)
    });
    let encoded = match hit {
        Some(encoded) => encoded,
        None => {
            // The borrow above is released: resolving re-enters the VM.
            let encoded = ctx
                .resolve_field_index_by_class_id(class_id, "loadFactor")
                .map_or(0, |slot| slot as u32 + 1);
            LOAD_FACTOR_SLOT_MEMO.with(|cell| {
                let mut memo = cell.borrow_mut();
                if memo.0 == Some(vm) {
                    memo.1[index] = (raw, encoded);
                }
            });
            encoded
        }
    };
    encoded.checked_sub(1).map(|slot| slot as usize)
}

/// The receiver's own `loadFactor` when [`map_load_factor_enabled`] and it is
/// a real factor OTHER than 0.75; `None` otherwise. `None` covers a layout
/// without the field (the fabricated maps, CHM segments), an unwritten
/// `0.0` (maps no constructor ran on), and the default, so every one of those
/// keeps the `(cap * 3) / 4` arithmetic it always had. `f > 0.0` is false for
/// NaN.
pub(crate) fn map_custom_load_factor(ctx: &dyn NativeContext, this: ObjectRef) -> Option<f32> {
    if !map_load_factor_enabled() {
        return None;
    }
    let slot = load_factor_slot(ctx, ctx.class_id_of_object(this))?;
    if slot >= ctx.object_num_fields(this) {
        return None;
    }
    match ctx.get_field(this, slot) {
        Value::Float(f) if f > 0.0 && f != 0.75 => Some(f),
        _ => None,
    }
}

/// The receiver's own `threshold` field, bounded by its allocated slots.
pub(crate) fn map_threshold_field(ctx: &dyn NativeContext, this: ObjectRef) -> Option<i32> {
    let slot = ctx.resolve_field_index_by_class_id(ctx.class_id_of_object(this), "threshold")?;
    if slot >= ctx.object_num_fields(this) {
        return None;
    }
    match ctx.get_field(this, slot) {
        Value::Int(t) => Some(t),
        _ => None,
    }
}

/// The largest entry count a native `put` may leave in a table of `cap`
/// buckets before it grows it: `put` grows when `size + 1 >` this, which is
/// `HashMap.putVal`'s `++size > threshold` and `Hashtable.addEntry`'s
/// `count >= threshold`. A 0.75 map answers `(cap * 3) / 4` exactly as before
/// (equal to the JDK's threshold at every power of two and at every
/// `Hashtable` `2n + 1` size). A custom factor answers the receiver's own
/// `threshold`, which the constructor and [`map_resize_inner`] now maintain
/// by the JDK's rules; a negative (never written) one falls back to the
/// recomputed value rather than growing on every put.
pub(crate) fn map_grow_threshold(ctx: &dyn NativeContext, this: ObjectRef, cap: i32) -> i32 {
    let legacy = (cap * 3) / 4;
    let Some(lf) = map_custom_load_factor(ctx, this) else {
        return legacy;
    };
    match map_threshold_field(ctx, this) {
        Some(t) if t >= 0 => t,
        _ if uses_native_hashtable_layout(ctx, this) => jdk_hashtable_threshold(cap, lf),
        _ => jdk_hashmap_threshold(cap, lf),
    }
}

pub(crate) fn map_resize(ctx: &mut dyn NativeContext, this: &mut ObjectRef, grow: GrowAlloc) {
    let is_concurrent = CHM_RESIZE_LOCK_NEEDED.with(|c| c.get());
    *this = map_resize_inner(ctx, *this, is_concurrent, grow);
}

/// Returns the receiver, refreshed across every collection this can complete.
///
/// `grow` ([`GrowAlloc`]) says whether a refused table allocation may collect
/// and retry. Only the registered `put` doors pass `Reclaim`; they reach here
/// through `native_map_put_evict_pinned`, whose put window pins the receiver,
/// the key and the value and re-reads all three after this call.
pub(crate) fn map_resize_inner(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    is_concurrent: bool,
    grow: GrowAlloc,
) -> ObjectRef {
    // Bug 1+2+5 (CRIT/HIGH) round-10 fix: only take the resize lock when
    // resizing a CHM segment. Plain `java/util/HashMap.put` is single-
    // threaded; serializing its resize through the striped lock array
    // (round-9 native-misc HIGH-3) was pure overhead.
    //
    // For CHM segments, the per-segment striped RwLock (256 stripes,
    // keyed by `ctx.identity_hash_code(this)`) excludes concurrent
    // lock-free readers in `chm_seg_get` while we mutate NEXT pointers
    // in place.
    //
    // TODO(round-6+): replace with a clone-resize that allocates fresh
    // Node objects for the new sub-chains and leaves the old chain
    // untouched, so reads can stay fully lock-free and this lock can be
    // removed entirely.
    let (_write_guard, this) = if is_concurrent {
        let (guard, this) = ChmSegmentResizeGuard::acquire_gc_safe(ctx, this);
        (Some(guard), this)
    } else {
        (None, this)
    };

    let (old_buckets0, size, old_cap) = map_state(ctx, this);
    if old_cap >= MAP_MAX_CAPACITY {
        return this; // cannot grow further -- above the pin, so unmoved
    }
    // `native_map_put` also routes here to MATERIALISE a table for a map that
    // has none (a JDK-bytecode constructor that leaves `table` null, or any
    // path that skipped the synthetic `<init>` native). That is not a growth
    // step: HotSpot's `resize()` on a null table allocates
    // DEFAULT_INITIAL_CAPACITY buckets, it does not double. Doubling gave
    // every lazily-initialised map a 32-bucket table where HotSpot has 16,
    // which shifts every key's bucket index and makes iteration order diverge
    // from HotSpot for the identical set of keys (found via a json-smart
    // parse -> serialize -> re-parse round trip, where one map came from the
    // interpreter and the other from JIT-compiled code).
    // `java.util.Hashtable.rehash()` is `(oldCapacity << 1) + 1`, not a
    // doubling: 11 -> 23 -> 47 -> 95. Deriving the rule from the receiver keeps
    // it in one place; `map_bucket_index` needs no matching change, because it
    // takes the table's actual length as its modulus.
    //
    // The `doubled` split-rehash below tests `new_cap == old_cap * 2`, which is
    // false for every 2n+1 step, so a Hashtable automatically takes the legacy
    // per-bucket re-insert path — and that path's head-prepend is exactly what
    // `Hashtable.rehash` itself does (`e.next = newMap[index]; newMap[index] =
    // e;`), so the within-bucket order matches too.
    let ht_layout = uses_native_hashtable_layout(ctx, this);
    let new_cap = if old_buckets0.is_none() {
        let floor = if ht_layout {
            HASHTABLE_DEFAULT_CAPACITY as i32
        } else {
            MAP_DEFAULT_CAPACITY as i32
        };
        if !ht_layout && !is_concurrent && map_load_factor_enabled() {
            // `HashMap.resize()` on a null table: `newCap = oldThr > 0 ? oldThr
            // : DEFAULT_INITIAL_CAPACITY`, and `map_state` already answers
            // exactly that for a table-less map (its `threshold`, else 16).
            // `new HashMap<>(4)` therefore gets 4 buckets, as on HotSpot,
            // where the old floor gave 16. Rounded up to a power of two: a
            // real constructor only ever parks `tableSizeFor(n)` there, and
            // the rounding keeps a stale LOAD threshold (a table nulled after
            // growth, e.g. 24 for a 32-bucket map) from producing a table the
            // lo/hi split below cannot rehash. `CRATONVM_COMPAT_MAP_LOAD_FACTOR`.
            let wanted = std::cmp::max(old_cap, 1) as u32;
            let pow2 = wanted
                .checked_next_power_of_two()
                .unwrap_or(MAP_MAX_CAPACITY as u32);
            std::cmp::min(pow2, MAP_MAX_CAPACITY as u32) as i32
        } else {
            std::cmp::max(old_cap, floor)
        }
    } else if ht_layout {
        std::cmp::min(
            old_cap.saturating_mul(2).saturating_add(1),
            MAP_MAX_CAPACITY,
        )
    } else {
        std::cmp::min(old_cap * 2, MAP_MAX_CAPACITY)
    };
    // gcstress residual face-1 fix — `alloc_ref_array` can trigger a moving
    // young GC (deterministic under CRATONVM_DBG_GC_STRESS) that relocates
    // `this` and its bucket array. Both were captured as bare Rust locals
    // above and are used throughout the rehash + the final field writes; a
    // stale address would make every `get_array_element(old_b, ..)` /
    // `set_field(this, ..)` land in freed-and-reused storage (the same
    // corrupt-cell family as native_map_put). Pin `this` across the alloc,
    // then re-read `this` and re-fetch `old_buckets` from the live map. The
    // split loop below reuses existing nodes (no further allocation), so one
    // re-read suffices; nodes reached via the re-read `old_b` are already
    // forwarded. The only early `return` (MAX_CAPACITY) is above this pin.
    //
    // The component class must be carried across a resize or the table would be
    // typed at construction and revert to `Object[]` on first growth — a
    // divergence that appears only once a map exceeds its load factor, i.e. in
    // exactly the maps big enough for anyone to look at. The split loop below
    // REUSES the existing nodes, so the new table inherits the old table's
    // contents unchanged and the "every node is real" invariant carries over
    // with them; deriving the type from the receiver keeps this consistent with
    // `alloc_bucket_table` rather than introducing a second rule.
    let this_pin = ctx.pin_native_root(this);
    let component = bucket_table_component(ctx, this);
    let this = ctx.read_native_pin(this_pin, this);
    // gc-common w9-d: under `GrowAlloc::Reclaim` a refused table collects and
    // retries before it fails, as `HashMap.resize()`'s `new Node[newCap]` does
    // on HotSpot. `live` is empty because the only reference this function
    // holds across the allocation is `this`, pinned just above and re-read
    // below (`old_buckets` is re-fetched from the refreshed map, and
    // `old_buckets0` is only tested for `None`). The CHM stripe write guard may
    // be held here; that is by design -- see `ChmSegmentResizeGuard`, whose
    // waiters use the blocking protocol precisely because a resize can
    // collect. A refused retry falls through to the historical allocator.
    let new_buckets = match grow {
        GrowAlloc::SingleAttempt => ctx.new_ref_array(component, new_cap as usize),
        GrowAlloc::Reclaim => {
            match try_ref_array_reclaiming(ctx, component, new_cap as usize, &mut []) {
                Some(table) => table,
                None => ctx.new_ref_array(component, new_cap as usize),
            }
        }
    };
    let this = ctx.read_native_pin(this_pin, this);
    let old_buckets = map_state(ctx, this).0;

    // Re-hash all entries. When `new_cap == 2 * old_cap` (the common
    // doubling case) we use JDK's split semantics: each entry whose
    // `hash & old_cap == 0` stays at bucket `i`, and the rest move to
    // bucket `i + old_cap`. This walks each chain exactly once and
    // writes the two head slots — strictly better than the legacy
    // head-prepend path which performs N `set_array_element` calls per
    // bucket.
    //
    // AUDIT 2026-05-17: previously this branch was gated on a
    // successful `bulk_array_copy` of the reference bucket array as a
    // "pre-seed" optimization. `vm_exec::bulk_array_copy` refuses
    // reference arrays (write-barrier reasons), so the gate always
    // failed and we always took the slow legacy path. The split is a
    // perf win even without the pre-seed — overwriting the two head
    // slots is what the JDK does itself — so we always take it.
    // `bulk_array_copy` is still attempted as an optional fast path so
    // that if a future VM override does support reference arrays, we
    // skip the per-bucket head reads.
    // HIB-MAPRESIZE-STALE.1 (2026-07-31) — the rehash below is NOT
    // allocation-free, contrary to the `this_pin` comment above ("the split
    // loop below reuses existing nodes (no further allocation), so one re-read
    // suffices"). Its `set_field(node, NODE_FIELD_NEXT, ..)` and
    // `set_array_element(new_buckets, ..)` calls are REFERENCE-typed stores, so
    // each can allocate a remembered-set entry through the write barrier and
    // therefore complete a moving young GC — the exact hazard
    // `native_map_put_evict_pinned` documents and pins against. `old_b` and
    // `new_buckets` are bare Rust locals invisible to `collect_roots`, so after
    // such a GC they name pre-move addresses; the loops then write through
    // freed nodes and, far worse, publish stale `lo_head`/`hi_head` into
    // `new_buckets`, leaving the map with dangling chain heads permanently.
    // Pin both arrays for the whole rehash and re-read them after every store
    // that can collect.
    let new_buckets_h = ctx.pin_native_root(new_buckets);
    let mut new_buckets = ctx.read_native_pin(new_buckets_h, new_buckets);
    // `CRATONVM_COMPAT_HASHMAP_TREE_BINS`: a tree bin is split by the JDK's own
    // `TreeNode.split` (see the tree-bin section at `HM_TREE_NODE_CLASS`).
    let tree_bins = !is_concurrent && !ht_layout && hm_tree_family(ctx, this);
    if let Some(old_b) = old_buckets {
        let old_b_h = ctx.pin_native_root(old_b);
        let mut old_b = ctx.read_native_pin(old_b_h, old_b);
        // The lo/hi split is only `%`-indexing's answer for a power-of-two old
        // table (`hash & oldCap` is the new bit only there); any other length
        // takes the per-node re-insert below, which recomputes every index.
        // `CRATONVM_COMPAT_MAP_LOAD_FACTOR`.
        let doubled = (new_cap as i64) == (old_cap as i64) * 2
            && (!map_load_factor_enabled() || (old_cap as u32).is_power_of_two());
        if tree_bins && !doubled {
            // The per-node re-insert below relinks by `next` only, which a
            // tree cannot survive: turn every tree bin back into a chain
            // first (in the OLD table, which is still the map's). A
            // non-power-of-two `HashMap` table is not one the JDK makes, so
            // this is a safety net rather than a JDK path.
            for i in 0..(old_cap as usize) {
                let head = ctx.get_array_element(old_b, i);
                if hm_tree_head(&*ctx, head).is_some() {
                    let this_now = ctx.read_native_pin(this_pin, this);
                    let _ = hm_untreeify_bin(ctx, this_now, i);
                    new_buckets = ctx.read_native_pin(new_buckets_h, new_buckets);
                    old_b = ctx.read_native_pin(old_b_h, old_b);
                }
            }
        }
        if doubled {
            // Optional fast path: if the VM's bulk_array_copy supports
            // reference arrays, pre-seed `new_buckets[0..old_cap]` from
            // `old_b[0..old_cap]`. If not, the split loop below still
            // writes every head slot, so correctness is unaffected.
            let _preseeded = ctx.bulk_array_copy(old_b, 0, new_buckets, 0, old_cap as usize);
            // JDK-style split: walk each old bucket, partition its chain
            // into "low" (stays at i) and "high" (moves to i+old_cap)
            // lists, then overwrite the two head slots.
            let split_mask = old_cap; // power-of-two: the new high bit
            for i in 0..(old_cap as usize) {
                let mut node_val = ctx.get_array_element(old_b, i);
                // `resize()`: `if (e.next == null) newTab[..] = e; else if (e
                // instanceof TreeNode) ((TreeNode) e).split(this, newTab, j,
                // oldCap);`. A one-node tree bin takes the plain move below,
                // as it does on the JDK.
                if tree_bins {
                    if let Some(first) = hm_tree_head(&*ctx, node_val) {
                        if !matches!(ctx.get_field(first, NODE_FIELD_NEXT), Value::Object(None)) {
                            // `split` writes only the slots it fills: undo a
                            // pre-seed of `newTab[j]` (a null store, no barrier).
                            ctx.set_array_element(new_buckets, i, Value::Object(None));
                            let this_now = ctx.read_native_pin(this_pin, this);
                            let _ = ctx.invoke_special_bytecode_only(
                                HM_TREE_NODE_CLASS,
                                "split",
                                HM_SPLIT_DESC,
                                &[
                                    Value::Object(Some(first)),
                                    Value::Object(Some(this_now)),
                                    Value::Object(Some(new_buckets)),
                                    Value::Int(i as i32),
                                    Value::Int(split_mask),
                                ],
                            );
                            new_buckets = ctx.read_native_pin(new_buckets_h, new_buckets);
                            old_b = ctx.read_native_pin(old_b_h, old_b);
                            continue;
                        }
                    }
                }
                // Track lo/hi head + tail so we preserve original chain order.
                let mut lo_head: Option<ObjectRef> = None;
                let mut lo_tail: Option<ObjectRef> = None;
                let mut hi_head: Option<ObjectRef> = None;
                let mut hi_tail: Option<ObjectRef> = None;
                let mut steps: usize = 0;
                let step_cap = (size as usize).saturating_add(8);
                let mut cycle_tripped = false;
                while let Value::Object(Some(node)) = node_val {
                    steps += 1;
                    if steps > step_cap {
                        // Total live entries bound the chain length;
                        // exceeding it indicates a cycle. Round-5 fix: do
                        // NOT silently keep the partial lo/hi chains —
                        // that loses every entry past the bound. Mark
                        // this bucket for full re-insert via the
                        // standard hash-prepend path below.
                        eprintln!(
                            "[HM-RESIZE-GUARD] aborting old-chain split walk at {} nodes (suspected cycle); bucket={} — falling back to full re-insert",
                            steps, i
                        );
                        cycle_tripped = true;
                        break;
                    }
                    let key_hash = match ctx.get_field(node, NODE_FIELD_HASH) {
                        Value::Int(h) => h,
                        _ => 0,
                    };
                    let next = ctx.get_field(node, NODE_FIELD_NEXT);
                    let is_lo = (key_hash & split_mask) == 0;
                    // HIB-MAPRESIZE-STALE.1: the link store below is this
                    // step's only GC point, and it only happens once a
                    // partition already has a head — so the common
                    // single-node bucket pays no pin at all. When it does
                    // fire, pin every reference that has to outlive it and
                    // re-read each one afterwards. The frame sits ABOVE the
                    // two array pins, so unwinding it leaves those intact.
                    let will_store = if is_lo {
                        lo_head.is_some() && lo_tail.is_some()
                    } else {
                        hi_head.is_some() && hi_tail.is_some()
                    };
                    let mut node = node;
                    let mut next = next;
                    if will_store {
                        let step_base = ctx.pin_native_root(node);
                        let node_h = step_base;
                        let next_h = pin_value(ctx, next);
                        let lo_head_h = opt_pin(ctx, lo_head);
                        let lo_tail_h = opt_pin(ctx, lo_tail);
                        let hi_head_h = opt_pin(ctx, hi_head);
                        let hi_tail_h = opt_pin(ctx, hi_tail);

                        let tail = if is_lo { lo_tail } else { hi_tail };
                        let tail_h = if is_lo { lo_tail_h } else { hi_tail_h };
                        if let Some(t) = tail {
                            let t = ctx.read_native_pin(tail_h, t);
                            let linked = ctx.read_native_pin(node_h, node);
                            ctx.set_field(t, NODE_FIELD_NEXT, Value::Object(Some(linked)));
                        }

                        // Everything below this line is post-GC.
                        node = ctx.read_native_pin(node_h, node);
                        next = read_pinned_elem(ctx, next_h, next);
                        lo_head = opt_read(ctx, lo_head_h, lo_head);
                        lo_tail = opt_read(ctx, lo_tail_h, lo_tail);
                        hi_head = opt_read(ctx, hi_head_h, hi_head);
                        hi_tail = opt_read(ctx, hi_tail_h, hi_tail);
                        new_buckets = ctx.read_native_pin(new_buckets_h, new_buckets);
                        old_b = ctx.read_native_pin(old_b_h, old_b);
                        ctx.unpin_native_roots(step_base);
                    }
                    // Assign this step's cursors from the (refreshed) node.
                    // Emptiness is preserved by the refresh, so testing
                    // `is_none()` here matches the `will_store` decision above.
                    if is_lo {
                        if lo_head.is_none() {
                            lo_head = Some(node);
                        }
                        lo_tail = Some(node);
                    } else {
                        if hi_head.is_none() {
                            hi_head = Some(node);
                        }
                        hi_tail = Some(node);
                    }
                    node_val = next;
                }
                if cycle_tripped {
                    // Round-5 cycle-bound fallback: discard the partial
                    // lo/hi work for this bucket and re-walk the
                    // original chain with a visited-set so we never
                    // follow the cycle twice, inserting each unique
                    // node into `new_buckets` via head-prepend at the
                    // correct index. This preserves correctness at the
                    // cost of bucket order — much better than silently
                    // dropping ~50% of entries.
                    let mut seen: std::collections::HashSet<*const ()> =
                        std::collections::HashSet::new();
                    let mut nv = ctx.get_array_element(old_b, i);
                    while let Value::Object(Some(node)) = nv {
                        let key = node.as_ptr() as *const ();
                        if !seen.insert(key) {
                            // Already inserted — cycle detected, stop.
                            break;
                        }
                        let key_hash = match ctx.get_field(node, NODE_FIELD_HASH) {
                            Value::Int(h) => h,
                            _ => 0,
                        };
                        let next = ctx.get_field(node, NODE_FIELD_NEXT);
                        let new_idx = map_bucket_index(key_hash, new_cap);
                        // HIB-MAPRESIZE-STALE.1: same hazard as the split walk
                        // — both stores below are reference-typed and can
                        // collect, and `node`/`next`/`new_buckets` are bare
                        // locals across them.
                        let step_base = ctx.pin_native_root(node);
                        let node_h = step_base;
                        let next_h = pin_value(ctx, next);
                        let existing = ctx.get_array_element(new_buckets, new_idx);
                        let existing_h = pin_value(ctx, existing);
                        let node_now = ctx.read_native_pin(node_h, node);
                        ctx.set_field(
                            node_now,
                            NODE_FIELD_NEXT,
                            read_pinned_elem(ctx, existing_h, existing),
                        );
                        new_buckets = ctx.read_native_pin(new_buckets_h, new_buckets);
                        let node_now = ctx.read_native_pin(node_h, node);
                        ctx.set_array_element(new_buckets, new_idx, Value::Object(Some(node_now)));
                        new_buckets = ctx.read_native_pin(new_buckets_h, new_buckets);
                        old_b = ctx.read_native_pin(old_b_h, old_b);
                        nv = read_pinned_elem(ctx, next_h, next);
                        ctx.unpin_native_roots(step_base);
                    }
                    continue;
                }
                // HIB-MAPRESIZE-STALE.1: these four stores are the ones that
                // actually PUBLISH the partitioned chains, so a stale
                // `lo_head`/`hi_head` here is what corrupts the map for good.
                // Two of them are reference-typed and can collect, so pin the
                // cursors and refresh between stores.
                // The frame is anchored on `new_buckets` rather than on one of
                // the cursors: any cursor may legitimately be `None`, and a
                // `usize::MAX` base would silently leak the rest of the frame.
                let tail_base = ctx.pin_native_root(new_buckets);
                let lo_head_h = opt_pin(ctx, lo_head);
                let lo_tail_h = opt_pin(ctx, lo_tail);
                let hi_head_h = opt_pin(ctx, hi_head);
                let hi_tail_h = opt_pin(ctx, hi_tail);
                // Terminate both partitioned chains. A null store does not fire
                // the write barrier, but refresh anyway rather than depend on
                // that staying true.
                if let Some(t) = opt_read(ctx, lo_tail_h, lo_tail) {
                    ctx.set_field(t, NODE_FIELD_NEXT, Value::Object(None));
                }
                if let Some(t) = opt_read(ctx, hi_tail_h, hi_tail) {
                    ctx.set_field(t, NODE_FIELD_NEXT, Value::Object(None));
                }
                // Overwrite the two slots (low keeps `i`, high goes to i+old_cap).
                new_buckets = ctx.read_native_pin(new_buckets_h, new_buckets);
                let lo_head_now = opt_read(ctx, lo_head_h, lo_head);
                ctx.set_array_element(new_buckets, i, Value::Object(lo_head_now));
                new_buckets = ctx.read_native_pin(new_buckets_h, new_buckets);
                let hi_head_now = opt_read(ctx, hi_head_h, hi_head);
                ctx.set_array_element(
                    new_buckets,
                    i + old_cap as usize,
                    Value::Object(hi_head_now),
                );
                new_buckets = ctx.read_native_pin(new_buckets_h, new_buckets);
                old_b = ctx.read_native_pin(old_b_h, old_b);
                ctx.unpin_native_roots(tail_base);
            }
        } else {
            // Legacy rebuild path: per-bucket re-insert (head-prepend).
            //
            // `Hashtable.rehash` walks the OLD table from the top down
            // (`for (int i = oldCapacity ; i-- > 0 ;)`), and with head-prepend
            // the walk order decides the order of entries from different old
            // buckets that meet in one new bucket -- i.e. the enumeration
            // order. `CRATONVM_COMPAT_MAP_JDK_ORDER` (see `map_jdk_order_enabled`).
            let descending = ht_layout && map_jdk_order_enabled();
            for step in 0..(old_cap as usize) {
                let i = if descending {
                    old_cap as usize - 1 - step
                } else {
                    step
                };
                let mut node_val = ctx.get_array_element(old_b, i);
                let mut steps: usize = 0;
                while let Value::Object(Some(node)) = node_val {
                    steps += 1;
                    if steps > 1_000_000 {
                        eprintln!(
                            "[HM-RESIZE-GUARD] aborting old-chain walk at {} nodes (suspected cycle); bucket={}",
                            steps, i
                        );
                        break;
                    }
                    let key_hash = match ctx.get_field(node, NODE_FIELD_HASH) {
                        Value::Int(h) => h,
                        _ => 0,
                    };
                    let next = ctx.get_field(node, NODE_FIELD_NEXT);
                    // Self-cycle guard before we splice into the new bucket
                    if let Value::Object(Some(nx)) = next {
                        if std::ptr::eq(nx.as_ptr(), node.as_ptr()) {
                            eprintln!(
                                "[HM-RESIZE-GUARD] self-cycle in old bucket {} step {}",
                                i, steps
                            );
                            // Truncate: splice node alone, do not continue
                            // (HIB-MAPRESIZE-STALE.1 pinning, as below).
                            let new_idx = map_bucket_index(key_hash, new_cap);
                            let step_base = ctx.pin_native_root(node);
                            let node_h = step_base;
                            let existing = ctx.get_array_element(new_buckets, new_idx);
                            let existing_h = pin_value(ctx, existing);
                            let node_now = ctx.read_native_pin(node_h, node);
                            ctx.set_field(
                                node_now,
                                NODE_FIELD_NEXT,
                                read_pinned_elem(ctx, existing_h, existing),
                            );
                            new_buckets = ctx.read_native_pin(new_buckets_h, new_buckets);
                            let node_now = ctx.read_native_pin(node_h, node);
                            ctx.set_array_element(
                                new_buckets,
                                new_idx,
                                Value::Object(Some(node_now)),
                            );
                            new_buckets = ctx.read_native_pin(new_buckets_h, new_buckets);
                            old_b = ctx.read_native_pin(old_b_h, old_b);
                            ctx.unpin_native_roots(step_base);
                            break;
                        }
                    }

                    // Insert into new bucket (HIB-MAPRESIZE-STALE.1 pinning:
                    // both stores are reference-typed and can collect).
                    let new_idx = map_bucket_index(key_hash, new_cap);
                    let step_base = ctx.pin_native_root(node);
                    let node_h = step_base;
                    let next_h = pin_value(ctx, next);
                    let existing = ctx.get_array_element(new_buckets, new_idx);
                    let existing_h = pin_value(ctx, existing);
                    let node_now = ctx.read_native_pin(node_h, node);
                    ctx.set_field(
                        node_now,
                        NODE_FIELD_NEXT,
                        read_pinned_elem(ctx, existing_h, existing),
                    );
                    new_buckets = ctx.read_native_pin(new_buckets_h, new_buckets);
                    let node_now = ctx.read_native_pin(node_h, node);
                    ctx.set_array_element(new_buckets, new_idx, Value::Object(Some(node_now)));
                    new_buckets = ctx.read_native_pin(new_buckets_h, new_buckets);
                    old_b = ctx.read_native_pin(old_b_h, old_b);
                    node_val = read_pinned_elem(ctx, next_h, next);
                    ctx.unpin_native_roots(step_base);
                }
            }
        }
        ctx.unpin_native_roots(old_b_h);
    }
    // HIB-MAPRESIZE-STALE.1: `this` was pinned before `alloc_ref_array` and
    // re-read once, but the rehash above can now collect too, so the local was
    // a pre-move address by the time the publication writes below used it.
    // Re-read both roots one last time.
    let this = ctx.read_native_pin(this_pin, this);
    let new_buckets = ctx.read_native_pin(new_buckets_h, new_buckets);

    // Round-5 CRIT fix (publication race): publish the new buckets
    // array with a volatile/Release-style write so that concurrent
    // readers in `chm_get_volatile` (which use `get_field_volatile` on
    // MAP_FIELD_BUCKETS) are guaranteed to either see the OLD fully-
    // linked array or the NEW fully-linked array — never a half-spliced
    // chain whose NEXT pointers were mid-rewrite. All chain mutations
    // for `new_buckets` (the lo/hi splice and the head-prepend
    // fallback) complete before this volatile store. The non-CHM
    // (single-threaded HashMap) callers see identical semantics — a
    // volatile store is at least as strong as a plain store.
    // Mirror the bucket array to the receiver's own `table` slot when present
    // (and different from slot 0). This is critical for AnnotationAttributes
    // and other JDK-constructed maps where slot 0 may not be the `table`
    // field — without this mirror, subsequent reads via `map_state` (which
    // reads slot 0) would see the new buckets, but any JDK-bytecode path
    // that reads `table` directly would see null. Also keeps the two
    // storage locations in sync.
    //
    // JDK-ONLY-LAYOUT (L2): `publish_map_table_volatile` resolves `table` on
    // the RECEIVER's class. This used to resolve it on `java/util/HashMap` and
    // write index 2 into whatever `this` was — on a `Properties` (excluded
    // from `CF_HASHTABLE_LAYOUT`, so it reaches this arm) index 2 is
    // `Hashtable.threshold`, an `int`, and every rehash overwrote it with the
    // bucket array. Same census row as `native_map_init`'s.
    if !uses_native_hashtable_layout(ctx, this) {
        publish_map_table_volatile(ctx, this, new_buckets, new_cap);
        set_map_size(ctx, this, size);
        // `HashMap.resize()`'s OTHER postcondition. The `Hashtable` arm below
        // has always written its `threshold`; the HashMap family never did, so
        // a grown map kept whatever its constructor wrote and the field went
        // stale the moment the table changed size.
        //
        // While `table == null` the JDK overloads `threshold` to mean "the
        // size the first `resize()` will allocate" (see [`map_state`]'s
        // capacity reader, which depends on exactly that). `resize()` ends
        // that overload: it allocates `newCap` and leaves
        // `threshold = newCap * loadFactor`. Skipping the second half left the
        // two meanings mixed — MEASURED 2026-09-11,
        // `apps/probes/L1MapFieldProbe`'s `F.hashSet.backing.fields`:
        // `new HashSet<>(List.of("a","b","c"))` read `threshold = 16` where
        // HotSpot reads 12, because 16 is the CAPACITY its constructor parked
        // there and nothing replaced it.
        //
        // COST: two name-resolved field lookups per resize, each a
        // class-manager read lock plus a hierarchy walk. Bounded by what it
        // sits inside — `map_resize` already does one such lookup in
        // `publish_map_table_volatile`, and the rehash above allocates a new
        // table and walks every chain, so this is small change on an O(n)
        // operation. Named rather than measured; if a resize-heavy profile
        // ever shows it, the fix is to cache the pair per class id, not to
        // drop the write.
        //
        // Resolve on the RECEIVER's class, never on `java/util/HashMap` — the
        // note above this branch is about the census row that cost, where a
        // `HashMap`-resolved index 2 landed on `Hashtable.threshold`. A class
        // with no such field (`ConcurrentHashMap`, which keeps `sizeCtl`
        // instead, and every fabricated-layout map) resolves to `None` and is
        // left alone. The slot is bounded against the OBJECT rather than the
        // class for the reason `hs_backing_map` is.
        if let Some(slot) =
            ctx.resolve_field_index_by_class_id(ctx.class_id_of_object(this), "threshold")
        {
            if slot < ctx.object_num_fields(this) {
                // `newCap * loadFactor`, with the field's own load factor when
                // the receiver carries one. `(cap * 3) / 4` is the 0.75 case
                // and is what the `Hashtable` arm hardcodes; a map built with
                // a different factor deserves its own arithmetic.
                let lf = match ctx
                    .resolve_field_index_by_class_id(ctx.class_id_of_object(this), "loadFactor")
                    .filter(|s| *s < ctx.object_num_fields(this))
                    .map(|s| ctx.get_field(this, s))
                {
                    Some(Value::Float(f)) if f > 0.0 && f.is_finite() => f,
                    _ => 0.75_f32,
                };
                // A custom factor (`CRATONVM_COMPAT_MAP_LOAD_FACTOR`) takes
                // `resize()`'s own rule, because `put` now grows at this
                // field: doubling a table of at least 16 DOUBLES the old
                // threshold (`newThr = oldThr << 1`), which is not
                // `newCap * loadFactor` for a factor like 0.1; every other
                // step recomputes. The field still holds the old value here.
                let threshold = match map_custom_load_factor(ctx, this) {
                    Some(custom) => {
                        let doubling = old_buckets0.is_some()
                            && old_cap >= MAP_DEFAULT_CAPACITY as i32
                            && (new_cap as i64) == (old_cap as i64) * 2
                            && new_cap < JDK_HASHMAP_MAXIMUM_CAPACITY;
                        match ctx.get_field(this, slot) {
                            Value::Int(old_thr) if doubling && old_thr >= 0 => old_thr << 1,
                            _ => jdk_hashmap_threshold(new_cap, custom),
                        }
                    }
                    None => resize_threshold(new_cap, lf),
                };
                ctx.set_field(this, slot, Value::Int(threshold));
            }
        }
    } else {
        ctx.set_field_volatile(this, MAP_FIELD_BUCKETS, Value::Object(Some(new_buckets)));
        set_map_size(ctx, this, size);
        if let Some(slot) = ctx.resolve_field_index("java/util/Hashtable", "threshold") {
            if slot < ctx.object_num_fields(this) {
                // `rehash()`: `(int)Math.min(newCapacity * loadFactor, ..)`.
                let threshold = match map_custom_load_factor(ctx, this) {
                    Some(custom) => jdk_hashtable_threshold(new_cap, custom),
                    None => (new_cap * 3) / 4,
                };
                ctx.set_field(this, slot, Value::Int(threshold));
            }
        }
    }
    ctx.unpin_native_roots(this_pin); // gcstress residual face-1 fix
    this
}

/// Collect all keys from a HashMap into a Vec.
/// If `this` is a `java.util.Properties` whose `map` field (JDK 25 layout)
/// points at a `ConcurrentHashMap`, return that CHM. Otherwise None.
///
/// JDK 25 changed `Properties` to back its entries with a private
/// `ConcurrentHashMap<Object,Object> map` field instead of the inherited
/// `Hashtable.table`. The synthetic-mode slot-0 `buckets` we populate on
/// `Properties.load()` therefore looks empty to JDK-style readers, and any
/// helper that walks slot 0 (`map_collect_keys` / `map_collect_entries` /
/// etc.) reports zero entries. That made `new HashMap<>(props)` empty, which
/// in turn made Kafka's `KafkaConfig$.populateSynonyms` drop every property
/// and surface as `Missing required configuration "process.roles"`.
pub(crate) fn properties_backing_chm(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    // `CRATONVM_COMPAT_PROPERTIES_CHM_BY_CLASS` (round 15 wave 4, lane
    // compat4): the slot below is `Properties.map`'s INDEX, read off whatever
    // receiver arrives. A program's `HashMap` / `LinkedHashMap` / `TreeMap` /
    // `ConcurrentHashMap` subclass holding a `ConcurrentHashMap` field at that
    // index (the second field it declares, for a `HashMap` subclass) was read
    // as a `Properties`: `map_collect_*` answered the FIELD's entries as the
    // map's. Those families are never a `Properties` (memoized class facts,
    // no walk); anything else keeps the slot test.
    if properties_chm_by_class_enabled() {
        let facts = receiver_facts(ctx, this);
        if facts.has(CF_TREE_MAP)
            || facts.has(CF_CHM)
            || (facts.has(CF_BUCKET_MAP_NAME) && !facts.has(CF_HASHTABLE_ANCESTRY))
        {
            return None;
        }
    }
    let slot = ctx.resolve_field_index("java/util/Properties", "map")?;
    if slot >= ctx.object_num_fields(this) {
        return None;
    }
    let m = match ctx.get_field(this, slot) {
        Value::Object(Some(m)) => m,
        _ => return None,
    };
    let cid = ctx.class_id_of_object(m);
    let cn = ctx.class_name_of_id(cid).unwrap_or_default();
    if cn == "java/util/concurrent/ConcurrentHashMap"
        || cn.starts_with("java/util/concurrent/ConcurrentHashMap$")
    {
        Some(m)
    } else {
        None
    }
}

/// `CRATONVM_COMPAT_PROPERTIES_CHM_BY_CLASS` -- default ON, `=0` restores the
/// bare slot test. See [`properties_backing_chm`].
pub(crate) fn properties_chm_by_class_enabled() -> bool {
    pub(crate) static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_PROPERTIES_CHM_BY_CLASS")
}

/// [`properties_backing_chm`] for a receiver whose class IS
/// `java.util.Properties` or a subclass of it; `None` for every other map.
/// `properties_backing_chm` reads `Properties.map`'s slot INDEX off whatever
/// receiver it is handed, so a program's `HashMap` subclass holding a
/// `ConcurrentHashMap` field at that slot reads as a `Properties`. Pure reads.
/// Round 15 wave 4 (lane compat4).
pub(crate) fn properties_receiver_chm(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    let props = ctx.class_id_by_name("java/util/Properties")?;
    let cid = ctx.class_id_of_object(this);
    if cid != props && !ctx.is_subclass(cid, props) {
        return None;
    }
    properties_backing_chm(ctx, this)
}

/// If `obj` is one of CratonVM's unmodifiable wrapper views, return the
/// backing collection it wraps (recursing through nested wrappers); otherwise
/// return `obj` unchanged. Lets the bucket-walking map helpers transparently
/// see through `Collections.unmodifiableMap` / `Map.of` results.
///
/// PERF (collections-classification-cost): the name set tested here is exactly
/// `is_unmod_wrapper`'s (`{Map, List, Set, SortedSet, NavigableSet, EntrySet,
/// Collection}`), so both read the one `CF_UNMOD_WRAPPER` bit out of the
/// per-`ClassId` memo instead of allocating a `String` under the
/// `class_manager` lock. `unwrap_unmod` is called from `map_state` and from
/// every `map_collect_*`, so it was on the `HashMap` hot path for maps that are
/// not wrappers at all.
pub(crate) fn unwrap_unmod(ctx: &dyn NativeContext, obj: ObjectRef) -> ObjectRef {
    if receiver_facts(ctx, obj).has(CF_UNMOD_WRAPPER) {
        if let Value::Object(Some(inner)) = ctx.get_field(obj, UNMOD_FIELD_BACKING) {
            return unwrap_unmod(ctx, inner);
        }
    }
    obj
}

pub(crate) fn map_collect_keys(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<Value> {
    let this = unwrap_unmod(ctx, this);
    let object_key = hm_int_fast_obj_key(ctx, this);
    if let Some(keys) = hm_int_fast_shard_for(object_key)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&object_key)
        .map(|state| {
            state
                .entries
                .keys_in_java_hashmap_order(state.start_cap)
                .into_iter()
                .filter_map(|k| {
                    state
                        .entries
                        .get(&k)
                        .map(|(key, _)| Value::Object(Some(key)))
                })
                .collect()
        })
    {
        return keys;
    }
    if let Some(chm) = properties_backing_chm(ctx, this) {
        // `JdkConcurrentHashMap` (wave 76, L1): the JDK's `table`.
        if let Some(entries) = chm_jdk_entries_if_row(ctx, chm) {
            return entries.into_iter().map(|(k, _)| k).collect();
        }
        return chm_collect_all_keys(ctx, chm);
    }
    if is_chm_receiver(ctx, this) {
        if let Some(entries) = chm_jdk_entries_if_row(ctx, this) {
            return entries.into_iter().map(|(k, _)| k).collect();
        }
        return chm_collect_all_keys(ctx, this);
    }
    // LinkedHashMap-family receivers (incl. subclasses like Spring's
    // AnnotationAttributes) store entries in the LHM overlay, not the
    // HashMap bucket array — reading the buckets returns NOTHING. The
    // prior exact-name dispatch in callers missed subclasses, so e.g.
    // `keySet()`/`entrySet()` views over an AnnotationAttributes looked
    // empty while size()/get() saw the data (SB-04 fallout).
    if is_lhm_receiver(ctx, this) {
        return lhm_collect_keys(ctx, this);
    }
    let (buckets, _size, cap) = map_state(ctx, this);
    let mut keys = Vec::new();
    if let Some(b) = buckets {
        for i in map_bucket_scan_order(ctx, this, cap as usize) {
            let mut node_val = ctx.get_array_element(b, i);
            while let Value::Object(Some(node)) = node_val {
                let key = get_node_key(ctx, node);
                keys.push(key);
                node_val = ctx.get_field(node, NODE_FIELD_NEXT);
            }
        }
    }
    keys
}

/// The bucket indices of `this`'s table, in the order the receiver's family
/// ENUMERATES them.
///
/// `HashMap` walks its table upwards, 0..len. `java.util.Hashtable` walks it
/// DOWNWARDS: `Hashtable$Enumerator`'s constructor sets `index = table.length`
/// and `hasMoreElements` decrements, so bucket `len-1` is enumerated first.
/// That is not an implementation detail — it is the observable order of
/// `Hashtable.toString()`, `keySet()`, `values()`, `entrySet()` and
/// `keys()`/`elements()` alike, because every one of them is that enumerator.
///
/// MEASURED, `--jdk-only`, four keys `a b c d` in a default `Hashtable`
/// (table length 11, so `a`->9 `b`->10 `c`->0 `d`->1):
///
/// ```text
///                    HotSpot 25.0.3+9      CratonVM before      after
///   toString()       {b=2, a=1, d=4, c=3}  {c=3, d=4, a=1, b=2}  matches
///   keySet()         [b, a, d, c]          [c, d, a, b]          matches
///   values()         [2, 1, 4, 3]          [3, 4, 1, 2]          matches
///   entrySet() iter  b,a,d,c               c,d,a,b               matches
///   keys()           b,a,d,c               b,a,d,c               unchanged
/// ```
///
/// The last row is the point: `native_ht_keys` was ALREADY descending and
/// right, so this VM had two enumeration orders for one `Hashtable` and they
/// disagreed with each other. Only the three bulk collectors were wrong.
///
/// This divergence was INVISIBLE until the table length matched HotSpot's.
/// With a 16-bucket table the keys land on different indices entirely, so the
/// orders differed for a reason that swamped this one; fixing the capacity is
/// what made the ordering rule measurable.
pub(crate) fn map_bucket_scan_order(ctx: &dyn NativeContext, this: ObjectRef, cap: usize) -> Vec<usize> {
    if uses_native_hashtable_layout(ctx, this) {
        (0..cap).rev().collect()
    } else {
        (0..cap).collect()
    }
}

/// Collect all values from a HashMap into a Vec.
pub(crate) fn map_collect_values(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<Value> {
    let this = unwrap_unmod(ctx, this);
    let object_key = hm_int_fast_obj_key(ctx, this);
    if let Some(values) = hm_int_fast_shard_for(object_key)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&object_key)
        .map(|state| {
            state
                .entries
                .keys_in_java_hashmap_order(state.start_cap)
                .into_iter()
                .filter_map(|k| state.entries.get(&k).map(|(_, value)| value))
                .collect()
        })
    {
        return values;
    }
    if let Some(chm) = properties_backing_chm(ctx, this) {
        // `JdkConcurrentHashMap` (wave 76, L1): the JDK's `table`.
        if let Some(entries) = chm_jdk_entries_if_row(ctx, chm) {
            return entries.into_iter().map(|(_, v)| v).collect();
        }
        return chm_collect_all_values(ctx, chm);
    }
    if is_chm_receiver(ctx, this) {
        if let Some(entries) = chm_jdk_entries_if_row(ctx, this) {
            return entries.into_iter().map(|(_, v)| v).collect();
        }
        return chm_collect_all_values(ctx, this);
    }
    // LHM-family receivers: read the overlay (see map_collect_keys).
    if is_lhm_receiver(ctx, this) {
        return lhm_collect_values(ctx, this);
    }
    let (buckets, _size, cap) = map_state(ctx, this);
    let mut values = Vec::new();
    if let Some(b) = buckets {
        for i in map_bucket_scan_order(ctx, this, cap as usize) {
            let mut node_val = ctx.get_array_element(b, i);
            while let Value::Object(Some(node)) = node_val {
                let value = get_node_value(ctx, node);
                values.push(value);
                node_val = ctx.get_field(node, NODE_FIELD_NEXT);
            }
        }
    }
    values
}

/// Collect all key-value pairs as (key, value) from a HashMap.
pub(crate) fn map_collect_entries(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<(Value, Value)> {
    let this = unwrap_unmod(ctx, this);
    let object_key = hm_int_fast_obj_key(ctx, this);
    if let Some(entries) = hm_int_fast_shard_for(object_key)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&object_key)
        .map(|state| {
            state
                .entries
                .keys_in_java_hashmap_order(state.start_cap)
                .into_iter()
                .filter_map(|k| {
                    state
                        .entries
                        .get(&k)
                        .map(|(key, value)| (Value::Object(Some(key)), value))
                })
                .collect()
        })
    {
        return entries;
    }
    if let Some(chm) = properties_backing_chm(ctx, this) {
        // `JdkConcurrentHashMap` (wave 76, L1): the JDK's `table`.
        if let Some(entries) = chm_jdk_entries_if_row(ctx, chm) {
            return entries;
        }
        return chm_collect_all_entries(ctx, chm);
    }
    if is_chm_receiver(ctx, this) {
        if let Some(entries) = chm_jdk_entries_if_row(ctx, this) {
            return entries;
        }
        return chm_collect_all_entries(ctx, this);
    }
    // LHM-family receivers: read the overlay (see map_collect_keys).
    if is_lhm_receiver(ctx, this) {
        let ks = lhm_collect_keys(ctx, this);
        let vs = lhm_collect_values(ctx, this);
        return ks.into_iter().zip(vs).collect();
    }
    let (buckets, _size, cap) = map_state(ctx, this);
    let mut entries = Vec::new();
    if let Some(b) = buckets {
        for i in map_bucket_scan_order(ctx, this, cap as usize) {
            let mut node_val = ctx.get_array_element(b, i);
            while let Value::Object(Some(node)) = node_val {
                let key = get_node_key(ctx, node);
                let value = get_node_value(ctx, node);
                entries.push((key, value));
                node_val = ctx.get_field(node, NODE_FIELD_NEXT);
            }
        }
    }
    entries
}

// JDK-ONLY-CLASSIFY: stub — both registrations target `org.hibernate.*`, which
// is not in any JDK boot image, so jdk-only-native-review.md rule 1 fires: no
// real declaring class ⇒ CompatibilityShim, forbidden under `JdkOnly`. Note
// these are third-party classes, not JDK classes: they are on the APPLICATION
// class path when Hibernate is present and absent otherwise, so `--jdk-only`
// must refuse the registration rather than the class. This is one of only two
// lexically uncategorised registration sites in the whole crate; both inherit
// `Bridge` dynamically from `register_collections_natives`, which is how a
// third-party shim ends up wearing the tag reserved for VM/OS boundaries.
pub(crate) fn register_hibernate_persistent_map_natives(r: &mut NativeMethodRegistry) {
    r.register(
        "org/hibernate/collection/spi/PersistentMap",
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        native_hibernate_persistent_map_for_each,
    );
    // `List.sort` is an interface default method.  Hibernate's PersistentList
    // inherits it, where the JDK implementation mutates the backing List via
    // a ListIterator proxy.  Under the real-JDK bridge that proxy can reorder
    // the values without reaching PersistentList.set(), leaving the collection
    // unmarked and suppressing its custom SQL updates at flush time.  Route the
    // inherited method through the wrapper's own set() contract instead.
    r.register(
        "org/hibernate/collection/spi/PersistentList",
        "sort",
        "(Ljava/util/Comparator;)V",
        native_hibernate_persistent_list_sort,
    );
}

pub(crate) fn native_hibernate_persistent_list_sort(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let list = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let comparator = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        // A null comparator uses the List contract's natural ordering.  Do it
        // in this implementation rather than dispatching `sort(null)` back to
        // the same PersistentList override.
        Some(Value::Object(None)) | None => None,
        _ => return Ok(None),
    };
    let elems = collect_collection_elements_or_real(ctx, list)?;
    if elems.len() <= 1 {
        return Ok(None);
    }
    let list_pin = ctx.pin_native_root(list);
    let cmp_pin = comparator.map(|comparator| ctx.pin_native_root(comparator));
    let (_, elem_handles) = pin_value_slice(ctx, &elems);
    let mut order: Vec<Value> = (0..elems.len() as i32).map(Value::Int).collect();
    let sort_result = merge_sort_fallible(ctx, &mut order, |c, a, b| {
        let ia = match a {
            Value::Int(i) => *i as usize,
            _ => 0,
        };
        let ib = match b {
            Value::Int(i) => *i as usize,
            _ => 0,
        };
        let left = read_pinned_elem(c, elem_handles[ia], elems[ia]);
        let right = read_pinned_elem(c, elem_handles[ib], elems[ib]);
        let result = match (comparator, cmp_pin) {
            (Some(comparator), Some(cmp_pin)) => {
                let comparator = c.read_native_pin(cmp_pin, comparator);
                comparator_compare(c, comparator, left, right)?
            }
            _ => natural_compare(c, &left, &right)?,
        };
        match result {
            Some(Value::Int(v)) => Ok(v),
            _ => Ok(0),
        }
    });
    if let Err(e) = sort_result {
        if let Some(cmp_pin) = cmp_pin {
            ctx.unpin_native_roots(cmp_pin);
        }
        ctx.unpin_native_roots(list_pin);
        return Err(e);
    }
    for (index, source) in order.iter().enumerate() {
        let source = match source {
            Value::Int(i) => *i as usize,
            _ => 0,
        };
        let list = ctx.read_native_pin(list_pin, list);
        let value = read_pinned_elem(ctx, elem_handles[source], elems[source]);
        ctx.invoke_virtual(
            list,
            "set",
            "(ILjava/lang/Object;)Ljava/lang/Object;",
            &[Value::Int(index as i32), value],
        )?;
    }
    if let Some(cmp_pin) = cmp_pin {
        ctx.unpin_native_roots(cmp_pin);
    }
    ctx.unpin_native_roots(list_pin);
    Ok(None)
}

pub(crate) fn register_hashmap_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/util/HashMap";

    r.register(c, "<init>", "()V", native_map_init);
    r.register(c, "<init>", "(I)V", native_map_init_capacity);
    r.register(c, "<init>", "(IF)V", native_map_init_capacity_load);
    r.register(c, "<init>", "(Ljava/util/Map;)V", native_map_init_from_map);
    // Serialization: drive the stream from the actual backing so a HashMap that
    // was built via the native `<init>(Map)` copy-constructor (which leaves the
    // real `loadFactor`/`table` unset) round-trips instead of serializing
    // `loadFactor=0.0` + zero entries. See `native_hashmap_write_object`.
    r.register(
        c,
        "writeObject",
        "(Ljava/io/ObjectOutputStream;)V",
        native_hashmap_write_object,
    );
    r.register(
        c,
        "readObject",
        "(Ljava/io/ObjectInputStream;)V",
        native_hashmap_read_object,
    );
    r.register(c, "size", "()I", native_map_size);
    r.register(c, "isEmpty", "()Z", native_map_is_empty);
    r.register(
        c,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_put_entry,
    );
    r.register(
        c,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_get,
    );
    r.register(
        c,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_remove,
    );
    r.register(
        c,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_map_contains_key,
    );
    r.register(
        c,
        "containsValue",
        "(Ljava/lang/Object;)Z",
        native_map_contains_value,
    );
    r.register(c, "clear", "()V", native_map_clear);
    r.register(c, "keySet", "()Ljava/util/Set;", native_map_key_set);
    r.register(c, "values", "()Ljava/util/Collection;", native_map_values);
    r.register(c, "entrySet", "()Ljava/util/Set;", native_map_entry_set);
    r.register(c, "toString", "()Ljava/lang/String;", native_map_to_string);
    r.register(
        c,
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_get_or_default,
    );
    r.register(
        c,
        "putIfAbsent",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_put_if_absent,
    );
    r.register(c, "putAll", "(Ljava/util/Map;)V", native_map_put_all);
    r.register(c, "hashCode", "()I", native_map_hash_code);
    r.register(c, "equals", "(Ljava/lang/Object;)Z", native_map_equals);
    r.register(
        c,
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        native_map_for_each,
    );
    r.register(
        c,
        "computeIfAbsent",
        "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;",
        native_map_compute_if_absent,
    );
    r.register(
        c,
        "compute",
        "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        native_map_compute,
    );
    r.register(
        c,
        "computeIfPresent",
        "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        native_map_compute_if_present,
    );
    r.register(
        c,
        "merge",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        native_map_merge,
    );
    r.register(
        c,
        "replaceAll",
        "(Ljava/util/function/BiFunction;)V",
        native_map_replace_all,
    );
    // `Map`'s three conditional mutators. `HashMap` overrides all of them with
    // bucket-walking bodies, so without these the JDK bytecode runs over our
    // natively-allocated nodes — see `register_map_conditional_mutators`.
    register_map_conditional_mutators(r, c);
    r.set_category(__prev_cat);
}

/// `new HashMap<>()` / `new LinkedHashMap<>()` as the JDK's own constructor
/// behaves: allocate NOTHING, and let the first insert size the table.
///
/// # The divergence
///
/// `H23-2` §7.4 nominated it and `WORKER-2-NOTE-1` §9a.3 measured it across four
/// shapes -- `new HashMap<>()`, `new LinkedHashMap<>()`, `new HashSet<>()`'s
/// backing map and `new HashMap<>(64)` all carried a bucket array where HotSpot
/// 25.0.3+9 leaves `table` null until the first put. (`Hashtable` is NOT lazy on
/// HotSpot -- an empty one has `len=11` -- so that family is untouched, and the
/// `uses_native_hashtable_layout` arm below returns before any of this.)
///
/// # Why this is a SPLIT and not a deletion
///
/// §9a.3 declined the change as a ten-site audit, and the count was right: the
/// eager allocation is load-bearing for this crate's OWN maps. This function had
/// NINE internal callers -- the `Collections.EMPTY_MAP`/`EMPTY_SET` singleton
/// fallbacks, `emptyMap`/`emptySet`/`singleton*`, both `Properties`
/// constructors and the `Hashtable(int)` path -- and `alloc_hs_backing` builds
/// every `HashSet` and view backing map through the capacity constructor, at
/// least two consumers of which do `map_state(..).0.unwrap()`.
///
/// The audit's answer was that NONE of them needed to change: they all kept the
/// eager behaviour they had via [`map_init_eager`], and only a map a JAVA
/// constructor asks for became lazy. That was one edit per call site and no
/// behavioural question at any of them, rather than nine judgement calls.
///
/// TWO of those nine have since been asked the question and answered it the
/// other way. Both `Properties` constructors are LAZY as of 2026-09-12, leaving
/// SEVEN eager callers -- see [`props_init_map_half`], which carries the
/// evidence. The short version is that the audit's premise does not hold for
/// that class: its table is not one of "this crate's own maps" in any useful
/// sense, because no `Properties` native reads or writes a bucket table at all.
pub fn native_map_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    map_init_inner(ctx, args, false)
}

/// [`native_map_init`] with the table allocated up front, for this crate's own
/// internal maps. See that function for why both exist.
pub(crate) fn map_init_eager(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    map_init_inner(ctx, args, true)
}

pub(crate) fn map_init_inner(ctx: &mut dyn NativeContext, args: &[Value], eager: bool) -> MethodCallResult {
    // S111r28 (peaceful-sammet bug fix): Restore legacy synthetic layout at
    // absolute slots 0/1/2 (= buckets/size/capacity), which is what
    // `native_map_put` / `native_map_get` / `native_map_size` etc. read and
    // write. The previous S111r27 init wrote to absolute slots 0..5 thinking
    // they were JDK fields (table/entrySet/size/modCount/threshold/loadFactor),
    // but the real JDK class hierarchy puts AbstractMap.keySet at slot 0 and
    // AbstractMap.values at slot 1 first — so absolute slots 0..5 are NOT the
    // JDK HashMap own-fields. The result was that `native_map_put` saw
    // `slot0 = Object(None)` (no buckets) and bailed out without inserting,
    // which surfaced as `HashMap.size() == 0` / `HashSet` empty after every
    // put, breaking SpringApplication.<init> ("Sources must not be empty").
    //
    // Native HashMap path is the authoritative implementation (every
    // observable Map method — put, get, size, isEmpty, containsKey, keySet,
    // values, entrySet, toString, etc. — is registered as a native), so we
    // can just use the legacy synthetic layout at slots 0/1/2. JDK bytecode
    // for `HashMap.<method>` does not run because the natives shadow it.
    //
    // We additionally write the JDK-named fields (`size`, `table`, etc.) by
    // name when the class metadata is resolvable, so any JDK-bytecode caller
    // that reaches into HashMap via direct getfield (rare but possible
    // through reflection / private putVal entry points) sees consistent
    // values. These writes go to the correct real-JDK slots regardless of
    // how parent fields shift the absolute index.
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    if uses_native_hashtable_layout(ctx, this) {
        // GC-safety: `alloc_ref_array` is a collection point and `this` is a
        // bare local, so every store below must use a re-read reference.
        let this_pin = ctx.pin_native_root(this);
        // THE FOURTH ALLOCATION SITE, and the one a `new Hashtable()` actually
        // takes. `H23` typed three sites and missed the no-arg `HashMap()`
        // constructor; this arm is the same shape one family over, and it was
        // MEASURED live: with the node half moved and the other sites routed
        // through `map_carrier_class_for_receiver`, a 3-entry `Hashtable` still
        // reported `tableCls=[Ljava.lang.Object;` while a 40-entry one — which
        // had been through `map_resize` — reported
        // `[Ljava.util.Hashtable$Entry;`. The initial table is allocated here
        // and nowhere else, because this arm returns before the generic path.
        //
        // GC: `map_carrier_class_for_receiver` can load a class and therefore
        // collect, so it runs inside the pin region with `this` re-read after.
        let this = ctx.read_native_pin(this_pin, this);
        let component = map_carrier_class_for_receiver(ctx, this);
        let this = ctx.read_native_pin(this_pin, this);
        let buckets = ctx.new_ref_array(component, HASHTABLE_DEFAULT_CAPACITY);
        let buckets_pin = ctx.pin_native_root(buckets);
        let this = ctx.read_native_pin(this_pin, this);
        let buckets = ctx.read_native_pin(buckets_pin, buckets);
        ctx.set_field(this, MAP_FIELD_BUCKETS, Value::Object(Some(buckets)));
        // Refresh before reusing. (The store above is NOT itself a GC point —
        // see the `map_resize_inner` note; the refresh is kept as cheap
        // store-boundary discipline, not because the barrier can collect.)
        let this = ctx.read_native_pin(this_pin, this);
        ctx.unpin_native_roots(this_pin);
        set_map_size(ctx, this, 0);
        let cname = ctx
            .class_name_of_id(ctx.class_id_of_object(this))
            .unwrap_or_else(|| "java/util/Hashtable".to_string());
        ensure_hashtable_load_factor(ctx, this, &cname);
        return Ok(None);
    }
    // Evict the integer overlay of whatever used to live at this address.
    //
    // The memo is matched on the raw pointer ALONE — deliberately, since its
    // whole job here is to catch the previous tenant of a recycled address, and
    // the newcomer's identity hash can never match the old one. But "a fresh
    // object is at this address" does NOT imply "the previous tenant is dead":
    // under a copying young collection the previous tenant may simply have been
    // relocated (or promoted), leaving its old address free for reuse while it
    // is still very much alive. The overlay is the AUTHORITATIVE store for such
    // a map — its heap buckets are empty and its `size` field is 0 — so
    // dropping that entry silently empties a live `HashMap<Integer,?>`: `get`
    // returns null, `size()` returns 0, `keySet()` iterates nothing, and no
    // exception is raised anywhere. That is the
    // `hql-ordinal-parameter-dropped-under-jit-20260731-FIXED.md`
    // failure: Hibernate's `ParameterMetadataImpl.queryParametersByPosition` is
    // exactly this shape (fresh exact-class `HashMap`, boxed-Integer keys, long
    // lived), and losing it reports `No parameter labelled '?1' in query with
    // ordinal parameters []` on a query that parsed perfectly.
    //
    // `overlay_owner_still_at` separates the two cases using the reverse owner
    // index, which the collector already re-keys on relocation and clears on
    // death — so a moved-but-live tenant is no longer recorded at its old
    // address, while a dead-but-unpruned one still is. The memo itself is
    // cleared either way: it names an address this thread will never see that
    // tenant at again.
    let ptr = this.as_ptr() as usize;
    let vm = ctx.vm_identity();
    if let Some((_, _, _, stale_key)) = HM_INT_FAST_LAST_KEY
        .with(|cache| cache.get())
        .filter(|(cached_vm, cached_ptr, _, _)| *cached_vm == vm && *cached_ptr == ptr)
    {
        HM_INT_FAST_LAST_KEY.with(|cache| cache.set(None));
        let previous_tenant_is_dead = overlay_owner_still_at(ptr, stale_key);
        if dbg_hminit_purge() {
            eprintln!(
                "[hminit-purge] ptr={ptr:#x} key={stale_key:#x} previous_tenant_dead={previous_tenant_is_dead}"
            );
        }
        if previous_tenant_is_dead {
            hm_int_fast_shard_for(stale_key)
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&stale_key);
        }
    }
    // Legacy synthetic layout — what every other native HashMap op expects.
    // GC-safety: same hazard as the Hashtable branch above — `alloc_ref_array`
    // can collect, and every field store from here on uses `this`.
    // NOTE (HIB-MAPRESIZE-STALE.1): the pins below stay live across the whole
    // store sequence, but NOT for the reason originally given here. A
    // reference-typed store is not a GC point: `gen_heap::write_barrier`
    // bails out on a non-cross-generational store and otherwise pushes an
    // offset into a per-mutator Rust buffer — it never allocates from the
    // Java heap. The real hazard is the ALLOCATION above; holding the pins
    // through the stores is harmless and keeps every store using a re-read
    // reference.
    let this_pin = ctx.pin_native_root(this);
    // H23 COMPLETION (2026-08-21). This is the NO-ARG `HashMap()` constructor,
    // and it was the site `H23`'s table typing missed — it typed
    // `native_map_init_capacity` (the capacity-taking ctor), `map_resize` and
    // `lhm_init_with_cap`, while the default constructor here still spelled its
    // allocation `alloc_ref_array`, which hard-codes the untyped sentinel.
    //
    // MEASURED before this change, on a build that CONTAINED H23's fix: three
    // `new HashMap<>()` + puts gave `tableCls=[Ljava.lang.Object;` with
    // `real=3 fabricated=0` — real nodes (H16) inside an untyped array. HotSpot
    // gives `[Ljava.util.HashMap$Node;`. The fix was in the binary, its three
    // call sites were live, and `java.util.HashMap$Node` resolved fine at
    // runtime; it simply was never asked, because the common path is this one.
    //
    // GC-SAFETY, and this is the hazard `H23-2` named as its least-confident
    // falsifier: `bucket_table_component` can resolve a class and therefore
    // COLLECT. It must run inside the pin region, with `this` re-read on both
    // sides of it — the same discipline the allocation below already uses.
    // `new_ref_array` is kept (rather than `alloc_bucket_table`) so the
    // allocation shape and the infallibility of this path are unchanged; at
    // `MAP_DEFAULT_CAPACITY` the reduced-capacity fallback would return the
    // same 16 anyway.
    if !eager {
        // LAZY, matching the JDK. `threshold` stays 0 because no capacity was
        // requested -- HotSpot's own `new HashMap<>()` leaves it 0 and lets
        // `resize()` pick `DEFAULT_INITIAL_CAPACITY`. `native_map_put` already
        // calls `map_resize` when it finds no buckets; that is the path a
        // DESERIALISED map has always taken, so it is not a new one.
        ctx.unpin_native_roots(this_pin);
        set_map_size(ctx, this, 0);
        try_set_jdk_map_field(ctx, this, "modCount", Value::Int(0));
        try_set_jdk_map_field(ctx, this, "threshold", Value::Int(0));
        try_set_jdk_map_field(ctx, this, "loadFactor", Value::Float(0.75_f32));
        return Ok(None);
    }
    let this = ctx.read_native_pin(this_pin, this);
    let component = bucket_table_component(ctx, this);
    let this = ctx.read_native_pin(this_pin, this);
    let buckets = ctx.new_ref_array(component, MAP_DEFAULT_CAPACITY);
    let buckets_pin = ctx.pin_native_root(buckets);
    let this = ctx.read_native_pin(this_pin, this);
    let buckets = ctx.read_native_pin(buckets_pin, buckets);
    // S111r29: Resolve the JDK `table` slot (descriptor `[Ljava/util/HashMap$Node;`)
    // and store the bucket array there. Writing `Int(MAP_DEFAULT_CAPACITY)` to
    // `MAP_FIELD_CAPACITY` (absolute slot 2) is dangerous when the JDK class
    // layout puts `table` at the same slot — JDK bytecode (e.g. `HashMap.resize()`)
    // then runs `arraylength` on `Int(16)` and aborts with
    //   `internal error: expected object reference, got int(16)`.
    // The fix: never let the JDK `table` slot hold an Int. Write the bucket
    // array to the JDK `table` slot when resolvable, and skip the legacy
    // capacity Int write if it would clobber `table`. `map_state` already
    // prefers the bucket array's length over `MAP_FIELD_CAPACITY`, so dropping
    // the Int write is safe.
    //
    // JDK-ONLY-LAYOUT (L2): `publish_map_table` resolves `table` on the
    // RECEIVER's class rather than on a hard-coded `java/util/HashMap`. This is
    // the legacy branch the census pinned the surviving `Properties` slot-2 row
    // on: a `Properties` reaches here (it is deliberately excluded from
    // `CF_HASHTABLE_LAYOUT` because JDK 25 backs it with a side
    // `ConcurrentHashMap`), `HashMap.table` is index 2, and index 2 on a
    // `Properties` is the inherited `Hashtable.threshold` — an `int`. Its own
    // `table` is index 0, which slot 0 already holds, so the mirror correctly
    // collapses to a single write.
    publish_map_table(ctx, this, buckets, MAP_DEFAULT_CAPACITY as i32);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    set_map_size(ctx, this, 0);

    // Best-effort JDK-named field population for bytecode readers. `size`
    // was already mirrored by `set_map_size`; the rest are best-effort.
    try_set_jdk_map_field(ctx, this, "modCount", Value::Int(0));
    try_set_jdk_map_field(
        ctx,
        this,
        "threshold",
        Value::Int((MAP_DEFAULT_CAPACITY as i32 * 3) / 4),
    );
    try_set_jdk_map_field(ctx, this, "loadFactor", Value::Float(0.75_f32));
    Ok(None)
}

/// `HashMap(int)` / `HashMap(int, float)`, lazy like the JDK: record the size
/// the first insert will allocate and allocate nothing. See
/// [`native_map_init`].
pub(crate) fn native_map_init_capacity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    map_init_capacity_inner(ctx, args, false)
}

/// [`native_map_init_capacity`] with the table allocated up front, for this
/// crate's own backing maps -- `alloc_hs_backing`'s consumers dereference it.
pub(crate) fn map_init_capacity_eager(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    map_init_capacity_inner(ctx, args, true)
}

/// The `(int initialCapacity[, float loadFactor])` guards every hash-ordered
/// constructor in `java.util` opens with, in one place.
///
/// MEASURED against HotSpot 25.0.3+9 in BOTH modes for `HashMap`
/// (`probes/HashMapShadowSweep`), and again 2026-08-28 for `LinkedHashMap` and
/// `LinkedHashSet` (`apps/probes/LinkedSequencedShadowSweep` 19-21, 35):
///
/// ```text
///   new HashMap<>(-1)          HotSpot IllegalArgumentException  CratonVM no-throw
///   new HashMap<>(16, -1f)     HotSpot IllegalArgumentException  CratonVM no-throw
///   new HashMap<>(16, NaN)     HotSpot IllegalArgumentException  CratonVM no-throw
/// ```
///
/// The JDK's own body is two guards before it does anything else:
/// `initialCapacity < 0`, then `loadFactor <= 0 || Float.isNaN(loadFactor)`.
/// Both messages name the offending value, which is the whole reason a caller
/// with a computed capacity can find the bug.
///
/// NaN is the one that has to be spelled out rather than folded into the
/// `<= 0` test: every comparison against NaN is false, so `NaN <= 0` does NOT
/// catch it, and a NaN load factor silently produces a threshold of NaN and a
/// table that never resizes.
///
/// It is a free function over `args` — not a method on a receiver — because the
/// four callers reach it from three different layouts (`HashMap`,
/// `LinkedHashMap`, the `HashSet` family's backing map) and the argument list is
/// the only thing they share. Keeping ONE spelling is the point: this file has
/// twice found a rule half-applied across a family that shares the contract and
/// not the code.
/// How a map family SPELLS the refusals above.
///
/// The doc comment on the helper argues for keeping one spelling. That is the
/// right instinct when the family shares the contract, and measured against
/// HotSpot on JDK 25 (`apps/probes/MapCtorMsgProbe.java`) this one does not —
/// there are five spellings of the same negative-capacity check:
///
/// ```text
///   HashMap / LinkedHashMap / HashSet / LinkedHashSet  Illegal initial capacity: -1
///   Hashtable                                          Illegal Capacity: -1
///   WeakHashMap                                        Illegal Initial Capacity: -1
///   IdentityHashMap                                    expectedMaxSize is negative: -1
///   ConcurrentHashMap                                  (no message at all)
/// ```
///
/// Only the first row is this helper's. `ConcurrentHashMap` reached it anyway
/// and inherited a message the JDK does not produce, so the spelling is now a
/// parameter rather than an assumption.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MapCtorRefusalSpelling {
    /// `HashMap` and the classes that copy its wording.
    HashMapFamily,
    /// `ConcurrentHashMap`: `throw new IllegalArgumentException()`, bare.
    Messageless,
}

pub(crate) fn map_ctor_capacity_load_check(
    args: &[Value],
    spelling: MapCtorRefusalSpelling,
) -> Result<(), MethodCallFailed> {
    // An EMPTY message is this crate's marker for a null `getMessage()` — see
    // the `IllegalArgumentException` arm of `RuntimeError::render`. `Some("")`
    // and `None` are different answers to `getMessage()`, and the probe reads
    // the difference.
    let messageless = spelling == MapCtorRefusalSpelling::Messageless;
    if let Some(Value::Int(c)) = args.get(1) {
        if *c < 0 {
            return Err(RuntimeError::IllegalArgumentException {
                message: if messageless {
                    String::new()
                } else {
                    format!("Illegal initial capacity: {c}")
                },
            }
            .into());
        }
    }
    if let Some(Value::Float(f)) = args.get(2) {
        if *f <= 0.0 || f.is_nan() {
            return Err(RuntimeError::IllegalArgumentException {
                message: if messageless {
                    String::new()
                } else {
                    // `java_float_to_string`, not `{f}`: Rust prints 0.0f32 as
                    // "0" and Java prints "0.0", so the plain interpolation was
                    // wrong for every caller at once.
                    format!("Illegal load factor: {}", java_float_to_string(*f))
                },
            }
            .into());
        }
    }
    Ok(())
}

pub(crate) fn map_init_capacity_inner(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    eager: bool,
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    map_ctor_capacity_load_check(args, MapCtorRefusalSpelling::HashMapFamily)?;
    let cap = match args.get(1) {
        Some(Value::Int(c)) => {
            // JDK `HashMap(int initialCapacity)` / `HashSet(int)` semantics:
            // the initial table size is `tableSizeFor(initialCapacity)` — the
            // smallest power of two >= the requested capacity. It is NOT
            // inflated by the load factor: `new HashMap<>(16)` allocates a
            // 16-bucket table (threshold 16*0.75 = 12) and DOES resize on the
            // 13th insert. An earlier "hold N mappings without resizing"
            // optimisation rounded `ceil(c*4/3)` up instead, so
            // `new HashSet<>()` (whose backing passes cap 16) and
            // `new HashMap<>(16)` allocated 32 buckets — which shifts every
            // key's bucket index and makes HashSet/HashMap iteration order
            // diverge from HotSpot (B-E: Kafka AdminApiDriver request-key and
            // consumer-group ordering). Match the JDK exactly: tableSizeFor.
            let requested = std::cmp::max(*c, 1) as u32;
            let capped = std::cmp::min(requested, MAP_MAX_CAPACITY as u32);
            let n = capped
                .checked_next_power_of_two()
                .unwrap_or(MAP_MAX_CAPACITY as u32);
            std::cmp::min(n as usize, MAP_MAX_CAPACITY as usize)
        }
        _ => MAP_DEFAULT_CAPACITY,
    };
    // `HashMap(int, float)` keeps its factor (`this.loadFactor = loadFactor`)
    // under `CRATONVM_COMPAT_MAP_LOAD_FACTOR`; it used to be dropped, so
    // `new HashMap<>(16, 0.5f)` kept 16 buckets past 8 entries. The check
    // above has already refused `<= 0` and NaN. See `map_grow_threshold`.
    let load_factor = match args.get(2) {
        Some(Value::Float(f)) if map_load_factor_enabled() => *f,
        _ => 0.75_f32,
    };
    // S111r28: same legacy synthetic layout as `native_map_init`. See the
    // longer comment there for the rationale.
    // Cap the eager bucket-table allocation to what the heap can hold (HotSpot
    // sizes the table lazily, so `new HashMap(Integer.MAX_VALUE)` must not
    // abort); `cap` is rebound to the actually-allocated size so the JDK
    // `threshold`/`__capacity` fields below stay consistent with the table.
    // GC-safety: `lhm_init_with_cap` pins `this` across the identical
    // `alloc_bucket_table` call (confirmed live under CRATONVM_DBG_STALE_OBJREF
    // during WildFly parallel-extension-add); the plain-HashMap constructor —
    // which is what `new HashSet<>(...)`'s backing map goes through — never
    // got the same treatment, so the bucket-array store below could land on a
    // stale receiver and leave the fresh map with no table at all.
    // NOTE (HIB-MAPRESIZE-STALE.1): the pins below stay live across the whole
    // store sequence, but NOT for the reason originally given here. A
    // reference-typed store is not a GC point: `gen_heap::write_barrier`
    // bails out on a non-cross-generational store and otherwise pushes an
    // offset into a per-mutator Rust buffer — it never allocates from the
    // Java heap. The real hazard is the ALLOCATION above; holding the pins
    // through the stores is harmless and keeps every store using a re-read
    // reference.
    if !eager {
        // LAZY. `threshold` carries the table size the first insert will use,
        // which is where HotSpot keeps it while `table` is null and what
        // `map_state` reads back -- so `new HashMap<>(64)` still materialises
        // 64 buckets rather than collapsing to the default.
        set_map_size(ctx, this, 0);
        try_set_jdk_map_field(ctx, this, "modCount", Value::Int(0));
        try_set_jdk_map_field(ctx, this, "threshold", Value::Int(cap as i32));
        try_set_jdk_map_field(ctx, this, "loadFactor", Value::Float(load_factor));
        return Ok(None);
    }
    let this_pin = ctx.pin_native_root(this);
    // Resolve the table's component class BEFORE the allocation below, and
    // re-read `this` after it: the first `bucket_table_component` per VM per
    // thread can load `java/util/HashMap$Node`, which allocates and can
    // therefore move `this`. Every later call is a `Cell` read.
    let component = bucket_table_component(ctx, this);
    let this = ctx.read_native_pin(this_pin, this);
    let (buckets, cap) = alloc_bucket_table(ctx, component, cap);
    let buckets_pin = ctx.pin_native_root(buckets);
    let this = ctx.read_native_pin(this_pin, this);
    let buckets = ctx.read_native_pin(buckets_pin, buckets);
    // S111r29: see `native_map_init` for rationale. Mirror the bucket array
    // into the receiver's own `table` slot so `HashMap.resize()` bytecode sees
    // an array (or null), never an Int; write the legacy Int capacity only on
    // our fabricated layout (JDK-ONLY-LAYOUT, L2).
    publish_map_table(ctx, this, buckets, cap as i32);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    set_map_size(ctx, this, 0);

    // Best-effort JDK-named field population for bytecode readers.
    // `size` is mirrored by `set_map_size`.
    try_set_jdk_map_field(ctx, this, "modCount", Value::Int(0));
    let threshold = if load_factor == 0.75 {
        (cap as i32 * 3) / 4
    } else {
        jdk_hashmap_threshold(cap as i32, load_factor)
    };
    try_set_jdk_map_field(ctx, this, "threshold", Value::Int(threshold));
    try_set_jdk_map_field(ctx, this, "loadFactor", Value::Float(load_factor));
    Ok(None)
}

/// `HashMap(int initialCapacity, float loadFactor)`. `map_init_capacity_inner`
/// reads the trailing float itself: under `CRATONVM_COMPAT_MAP_LOAD_FACTOR`
/// (default on) it is stored in `loadFactor` and `put` grows at it; with the
/// switch off it is accepted and dropped as before (0.75).
/// `native_hs_init_capacity_load` hands its factor to the backing map under
/// `CRATONVM_COMPAT_HASHSET_CTOR_JDK` (round 13 wave 9);
/// `native_lhm_init_capacity_lf` keeps it too since round 14 wave 1
/// (`CRATONVM_COMPAT_LHM_LOAD_FACTOR`).
pub(crate) fn native_map_init_capacity_load(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_map_init_capacity(ctx, args)
}

/// Keep explicit-capacity Hashtable instances on the native backing used by
/// the rest of the Hashtable bridge. Letting the real constructor initialise
/// its JDK entry table strands mappings when native `put`/`get` later resize
/// their separate representation (observed as a missing PKCS#9 contentType
/// while verifying signed Spring Boot jars).
pub(crate) fn native_hashtable_init_capacity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let capacity = match args.get(1) {
        Some(Value::Int(value)) => *value,
        _ => MAP_DEFAULT_CAPACITY as i32,
    };
    if capacity < 0 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: format!("Illegal Capacity: {capacity}"),
            }
            .into(),
        );
    }
    if let Some(Value::Float(load_factor)) = args.get(2) {
        // The JDK refuses `loadFactor <= 0 || Float.isNaN(loadFactor)`, so an
        // INFINITE factor is legal (a table that never rehashes); refusing it
        // was a spurious `IllegalArgumentException`.
        // `CRATONVM_COMPAT_MAP_LOAD_FACTOR`.
        let refused_non_number = if map_load_factor_enabled() {
            load_factor.is_nan()
        } else {
            !load_factor.is_finite()
        };
        if refused_non_number || *load_factor <= 0.0 {
            return Err(
                cratonvm_types::error::RuntimeError::IllegalArgumentException {
                    message: format!("Illegal Load: {}", java_float_to_string(*load_factor)),
                }
                .into(),
            );
        }
    }
    // gc-common w10-g: `this` is rooted for the whole constructor. It was a bare
    // copy of the argument, used after `map_init_eager` (which allocates the
    // default table) and, below the resize block, after the resize's own
    // allocation -- the block's re-read `this` went out of scope with it, so
    // `ensure_hashtable_load_factor` wrote through the pre-collection address.
    let this_pin = ctx.pin_native_root(this);
    if let Err(e) = map_init_eager(ctx, &[Value::Object(Some(this))]) {
        ctx.unpin_native_roots(this_pin);
        return Err(e);
    }
    // `Hashtable(int initialCapacity)` is `new Entry[initialCapacity]` — the
    // requested length VERBATIM, with 0 rounded up to 1. It is not rounded to a
    // power of two (that is `HashMap`'s `tableSizeFor`) and not inflated by the
    // load factor. `native_map_init` has just allocated the 11-bucket default,
    // so re-size it here rather than threading a capacity through that native's
    // whole legacy-layout arm.
    //
    // Rehashing is `map_resize`'s job and it only GROWS, so a smaller-than-11
    // request is honoured by allocating directly; the map is empty at this
    // point, so there is nothing to rehash either way.
    let requested = std::cmp::min(std::cmp::max(capacity, 1), MAP_MAX_CAPACITY) as usize;
    if requested != HASHTABLE_DEFAULT_CAPACITY {
        let this = ctx.read_native_pin(this_pin, this);
        let component = map_carrier_class_for_receiver(ctx, this);
        // Registered-only (`Hashtable.<init>(I)V` / `(IF)V`), so the table may
        // collect and retry (gc-common w10-g, `GrowAlloc::Reclaim`). `live`
        // audit: `this` is pinned above and re-read below; `component` is a
        // `ClassId`; the fresh table is stored before anything else allocates.
        let buckets = new_ref_array_grow(ctx, component, requested, GrowAlloc::Reclaim);
        let this = ctx.read_native_pin(this_pin, this);
        ctx.set_field(this, MAP_FIELD_BUCKETS, Value::Object(Some(buckets)));
        if let Some(slot) = ctx.resolve_field_index("java/util/Hashtable", "threshold") {
            if slot < ctx.object_num_fields(this) {
                ctx.set_field(this, slot, Value::Int(((requested as i32) * 3) / 4));
            }
        }
    }
    // `Hashtable(int, float)` keeps its factor: `this.loadFactor = loadFactor;
    // threshold = (int)Math.min(initialCapacity * loadFactor, ..)`
    // (`CRATONVM_COMPAT_MAP_LOAD_FACTOR`). `map_init_eager` has already written
    // the 0.75 default, which is exactly what a 0.75 request keeps.
    if let Some(Value::Float(load_factor)) = args.get(2) {
        if map_load_factor_enabled() && *load_factor != 0.75 {
            let this = ctx.read_native_pin(this_pin, this);
            try_set_jdk_map_field(ctx, this, "loadFactor", Value::Float(*load_factor));
            try_set_jdk_map_field(
                ctx,
                this,
                "threshold",
                Value::Int(jdk_hashtable_threshold(requested as i32, *load_factor)),
            );
        }
    }
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ensure_hashtable_load_factor(ctx, this, "java/util/Hashtable");
    Ok(None)
}

pub(crate) fn native_hashtable_init_capacity_load(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_hashtable_init_capacity(ctx, args)
}

// Public wrappers for cross-module access (EnumMap, IdentityHashMap, WeakHashMap)
pub fn native_map_put_pub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_map_put(ctx, args)
}

/// The reclaiming twin of [`native_map_put_pub`] ([`GrowAlloc::Reclaim`]), for
/// another crate's REGISTRATION of a `put`-shaped Java method (`r.register(..,
/// native_map_put_entry_pub)`, or a closure that holds no reference across the
/// call). Never for a Rust caller that keeps a bare `ObjectRef` across it:
/// that caller keeps [`native_map_put_pub`]. gc-common w10-g; the
/// `native-builtins` registrations it is for are listed in
/// `docs/feature-designs/jdk-only-mode.md`.
pub fn native_map_put_entry_pub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_map_put_entry(ctx, args)
}

/// The reclaiming twin of [`native_al_add`] ([`GrowAlloc::Reclaim`]): a
/// refused grow collects and retries before it fails. For another crate's
/// registration of an `add`-shaped Java method, or a Rust caller that has
/// pinned (and re-reads afterwards) every `ObjectRef` it still uses after this
/// call, in its own frame and in every Rust frame above it. A caller that
/// cannot promise that keeps [`native_al_add`]. The door pins its own
/// receiver and element. gc-common w11-c
/// (`handoff-w10g-direct-rust-growth-doors`, Part B).
pub fn native_al_add_entry_pub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_al_add_entry(ctx, args)
}

/// The reclaiming twin of [`native_al_add_at`] ([`GrowAlloc::Reclaim`]), under
/// [`native_al_add_entry_pub`]'s contract. gc-common w11-c.
pub fn native_al_add_at_entry_pub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_al_add_at_entry(ctx, args)
}

/// The reclaiming twin of [`native_al_to_array`] ([`GrowAlloc::Reclaim`]),
/// under [`native_al_add_entry_pub`]'s contract. The returned array is fresh
/// (allocated after any collection). gc-common w11-c.
pub fn native_al_to_array_entry_pub(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_al_to_array_entry(ctx, args)
}
pub fn native_map_get_pub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_map_get(ctx, args)
}
pub fn native_map_remove_pub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_map_remove(ctx, args)
}
pub fn native_map_contains_key_pub(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_map_contains_key(ctx, args)
}
pub fn native_map_contains_value_pub(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_map_contains_value(ctx, args)
}
pub fn native_map_size_pub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_map_size(ctx, args)
}
pub fn native_map_is_empty_pub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_map_is_empty(ctx, args)
}
pub fn native_map_clear_pub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_map_clear(ctx, args)
}
pub fn native_map_key_set_pub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_map_key_set(ctx, args)
}
/// Collect a synthetic `HashMap`'s keys into a freshly-allocated `Object[]`.
///
/// Unlike `native_map_key_set_pub` (which builds a `HashSet` view), this
/// returns a plain reference array — the shape consumed by the synthetic
/// `java/util/Enumeration$Impl` helper (field 0 = `Object[]`, field 1 =
/// `int` cursor). Used by `ResourceBundle.getKeys()` overrides so a
/// caller iterating the bundle via `Enumeration` walks the real keys.
///
/// `map` must be the backing `HashMap` ref. Returns an empty array when
/// `map` is null or unreadable.
pub fn native_map_keys_as_array(ctx: &mut dyn NativeContext, map: Option<ObjectRef>) -> ObjectRef {
    let keys = match map {
        Some(m) => map_collect_keys(ctx, m),
        None => Vec::new(),
    };
    // Family-1 fix (cce0079): the array alloc can move every snapshot key —
    // store their refreshed addresses.
    let (pin_base, handles) = pin_value_slice(ctx, &keys);
    let arr = alloc_ref_array(ctx, keys.len());
    for (i, k) in keys.iter().enumerate() {
        let k = read_pinned_elem(ctx, handles[i], *k);
        ctx.set_array_element(arr, i, k);
    }
    if pin_base != usize::MAX {
        ctx.unpin_native_roots(pin_base);
    }
    arr
}
pub fn native_map_values_pub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_map_values(ctx, args)
}
pub fn native_map_entry_set_pub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_map_entry_set(ctx, args)
}
pub fn native_map_to_string_pub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_map_to_string(ctx, args)
}

/// Detect if a receiver is a TreeMap (or subclass). The `java/util/Map`
/// interface natives below use a HashMap-style field layout (buckets/size/
/// capacity at slots 0/1/2) which conflicts with TreeMap's
/// (data/size/comparator). Without this check, a `Map.put`/`get`/`size` call
/// on a TreeMap receiver silently no-ops, leaving size=0 and
/// dropping every entry. This blocked Keycloak FeatureOptions.<clinit>:
/// FeaturePropertyMappers stores feature→mapper entries in a TreeMap, and
/// when the JDK's TreeMap.keySpliteratorFor ran it traversed a null root
/// because the put native had never reached `tm_put`.
///
/// PERF (collections-classification-cost): this used to walk the superclass
/// chain calling `class_name_of_id` — a `class_manager` read lock plus a heap
/// `String` — once per hop, on every `Map.get`/`put`/`remove`/`size`. It now
/// reads a bit out of the per-`ClassId` classification memo; see
/// `CF_TREE_MAP` and the module comment on `class_facts`.
#[inline]
pub(crate) fn is_tree_map_receiver(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    receiver_facts(ctx, this).has(CF_TREE_MAP)
}

/// True when `this`'s runtime class is `java/util/concurrent/ConcurrentHashMap`.
///
/// CratonVM's CHM natives store a *segmented* layout — slot 0 is a ref-array
/// of per-segment map objects, not a HashMap bucket array. A generic
/// `java/util/Map.<method>` interface native (registered on `java/util/Map`)
/// dispatched on a CHM receiver — which happens whenever JDK code accesses a
/// CHM polymorphically through the `Map` interface, e.g. `MethodType`'s
/// `ReferencedKeySet`/`ReferencedKeyMap` backing store — must NOT run the
/// plain-HashMap path: `map_state` would mistake the segments array for a
/// bucket array and the node walk would dereference a segment object (3-field
/// synthetic map) as a 4-field `HashMap$Node`, reading field index 3 past the
/// end (the recurring `AnonymousObject$3` out-of-bounds-field-read error).
/// The plain-Map natives consult this and reroute to the CHM natives.
///
/// PERF (collections-classification-cost): memoized per `ClassId` — see
/// `CF_CHM`.
#[inline]
pub(crate) fn is_chm_receiver(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    receiver_facts(ctx, this).has(CF_CHM)
}

/// True when `source` is a `Map` whose entries CratonVM stores in a native
/// backing it can read field-by-field by treating slot 0 as a `HashMap` bucket
/// table: a real or synthetic `java/util/HashMap` (or any subclass), a
/// `ConcurrentHashMap`, or a `Properties`/`Hashtable` backed by a side CHM.
///
/// For any OTHER `Map` — an arbitrary `AbstractMap` subclass such as Spring's
/// `ConcurrentReferenceHashMap` (whose slot 0 is a `Segment[] segments` array,
/// not a bucket table), Guava maps, or the JDK's
/// `IdentityHashMap`/`WeakHashMap`/`EnumMap` — slot 0 is NOT a bucket array, so
/// the layout-sniffing bucket scan in `map_collect_entries`/`map_state` would
/// surface internal objects (segment locks, reference queues) as bogus entries.
/// Callers copying from such a source must instead use the polymorphic
/// `entrySet().iterator()` walk (`collect_entries_via_iterator`).
///
/// `LinkedHashMap` and `TreeMap` are also natively modeled but via their own
/// overlays, not the HashMap bucket path; callers check `is_lhm_receiver` /
/// `is_tree_map_receiver` before consulting this.
pub(crate) fn is_native_bucket_map(ctx: &dyn NativeContext, source: ObjectRef) -> bool {
    if is_chm_receiver(ctx, source) {
        return true;
    }
    if properties_backing_chm(ctx, source).is_some() {
        return true;
    }
    // PERF (collections-classification-cost): the `HashMap`-or-`Hashtable`
    // ancestry walk below is memoized per `ClassId` as `CF_BUCKET_MAP_NAME`.
    // The reasoning it encodes is unchanged and preserved here:
    //
    //   * `java/util/HashMap` in the chain -> true.
    //   * A plain `java/util/Hashtable` stores its entries in the same
    //     synthetic HashMap-style bucket table (`native_map_put` on
    //     `java/util/Hashtable` — its `size()`/`keySet()` read those buckets
    //     via `map_state`/`map_collect_keys`), so it is true as well. Without
    //     recognising it, `collect_entries_any` skipped the direct
    //     `map_collect_entries` bucket walk and fell through to the polymorphic
    //     `entrySet().iterator()` path — which re-enters the very
    //     `Hashtable.entrySet()` view being resynced and comes back EMPTY. That
    //     made `entrySet()` iterate zero times while `keySet()` worked,
    //     breaking `new ObjectName(domain, Hashtable)` (Spring JMX naming).
    //     `Properties` (a Hashtable subclass) keeps its data in a side-table so
    //     `map_collect_entries` returns empty for it — the caller's
    //     `!entries.is_empty()` guard then falls through to the existing
    //     Properties path, so this does not disturb Properties.
    //   * `java/lang/Object` reached first -> false.
    receiver_facts(ctx, source).has(CF_BUCKET_MAP_NAME)
}

/// True when `this`'s runtime class is `java/util/LinkedHashMap` or a subclass.
///
/// CratonVM's LHM natives store entries in a side-table overlay, not in the
/// HashMap bucket array. A generic `java/util/Map.<method>` interface native
/// dispatched on a LinkedHashMap receiver must route to the LHM natives so that
/// entries written via `native_lhm_put` can be found. Without this routing,
/// `native_map_get` / `native_map_contains_key` use the bucket code and return
/// null/false for every key — causing e.g. JBoss DMR `ObjectModelValue.getChild`
/// to return UNDEFINED for all ModelNode keys (the WildFly WFLYCTL0013 bug).
///
/// PERF (collections-classification-cost): memoized per `ClassId` — see
/// `CF_LHM`.
#[inline]
pub(crate) fn is_lhm_receiver(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    receiver_facts(ctx, this).has(CF_LHM)
}

/// True when `this`'s runtime class is `java/util/IdentityHashMap` or a
/// subclass. `native_hashmap_get_exact`/`native_map_put_evict_pinned` consult
/// this to compute the bucket hash/equality via reference identity
/// (`ctx.identity_hash_code` + pointer equality) instead of the receiver
/// key's virtual `hashCode()`/`equals()` — see `CF_IDENTITY_MAP`.
///
/// PERF: memoized per `ClassId` — see `CF_IDENTITY_MAP`.
#[inline]
pub(crate) fn is_identity_map_receiver(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    receiver_facts(ctx, this).has(CF_IDENTITY_MAP)
}

pub(crate) fn native_map_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    if let Some(b) = unmod_receiver_backing(ctx, this) {
        return native_map_size(ctx, &[Value::Object(Some(b))]);
    }
    if is_tree_map_receiver(ctx, this) {
        return native_tm_size(ctx, args);
    }
    if is_chm_receiver(ctx, this) {
        if let Some(r) = chm_jdk_delegate(ctx, args, "size", "()I", 0) {
            return r;
        }
        return native_chm_size(ctx, args);
    }
    if let Some(size) = hm_int_fast_len(ctx, this) {
        return Ok(Some(Value::Int(size as i32)));
    }
    let (buckets, size, _) = map_state(ctx, this);
    if buckets.is_none() {
        if let Some(r) = try_delegate_real_collection(ctx, this, "java/util/Map", "size", "()I") {
            return r;
        }
        if let Some(n) = singleton_wrapper_size(ctx, this) {
            return Ok(Some(Value::Int(n)));
        }
    }
    Ok(Some(Value::Int(size)))
}

pub(crate) fn native_map_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(1))),
    };
    if is_tree_map_receiver(ctx, this) {
        return native_tm_is_empty(ctx, args);
    }
    if is_chm_receiver(ctx, this) {
        if let Some(r) = chm_jdk_delegate(ctx, args, "isEmpty", "()Z", 0) {
            return r;
        }
        return native_chm_is_empty(ctx, args);
    }
    if let Some(size) = hm_int_fast_len(ctx, this) {
        return Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })));
    }
    let (buckets, size, _) = map_state(ctx, this);
    if buckets.is_none() {
        if let Some(r) = try_delegate_real_collection(ctx, this, "java/util/Map", "isEmpty", "()Z")
        {
            return r;
        }
    }
    Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))
}

/// True when `obj` is one of CratonVM's unmodifiable wrapper views.
///
/// PERF (collections-classification-cost): memoized per `ClassId` — see
/// `CF_UNMOD_WRAPPER`.
#[inline]
pub(crate) fn is_unmod_wrapper(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    receiver_facts(ctx, obj).has(CF_UNMOD_WRAPPER)
}

/// Initialize a `Hashtable`/`Properties` instance's own `loadFactor` (and a
/// sane `threshold`) the first time we see it unset (0.0). Hashtable extends
/// Dictionary (not HashMap) and keeps its own fields; the shared native bucket
/// path sets only `table`/`count`, leaving `loadFactor` at 0.0. Real-JDK
/// `Hashtable.writeObject` then serializes loadFactor=0.0, and a peer's
/// `Hashtable.readObject` throws `StreamCorruptedException: Illegal load factor:
/// 0.0` — breaking any Properties / system-properties round-trip (e.g. the
/// Gradle worker shipping its system properties to the daemon). The peer
/// recomputes `threshold` from `loadFactor`, so the threshold value itself is
/// irrelevant; we set 8 to avoid serializing uninitialized garbage. Resolved by
/// the object's real class name so inherited (`Properties`) fields are found.
pub(crate) fn ensure_hashtable_load_factor(ctx: &mut dyn NativeContext, this: ObjectRef, cname: &str) {
    if let Some(lf) = ctx.resolve_field_index(cname, "loadFactor") {
        if lf < ctx.object_num_fields(this)
            && matches!(ctx.get_field(this, lf), Value::Float(f) if f == 0.0)
        {
            ctx.set_field(this, lf, Value::Float(0.75));
            if let Some(th) = ctx.resolve_field_index(cname, "threshold") {
                if th < ctx.object_num_fields(this) {
                    ctx.set_field(this, th, Value::Int(8));
                }
            }
        }
    }
}

/// Keep the real JDK HashMap fail-fast version in sync with native structural
/// edits.
///
/// # What reads this generation
///
/// The doc here used to claim it was "the invalidation generation for the
/// bounded String-node lookup cache below". No cache by that name is findable
/// in this file, and the claim had been stale long enough that the
/// lazy-map-views plan had to open a blocker on it rather than trust it. The
/// two live readers, both verified:
///
/// * [`map_itr_check_comod`] — the JDK's `HashMap$HashIterator.nextNode()`
///   comodification test, i.e. `ConcurrentModificationException`;
/// * [`view_source_generation`] — the keySet-view rebuild elision, which SKIPS
///   a resync when this counter has not moved since the view's backing was
///   built.
///
/// The second is why a missing bump is now a silently stale collection rather
/// than only a missing CME, and why every map family's structural mutators
/// were audited against it: `native_map_put_evict_pinned`,
/// `native_map_remove_pinned` (x2) and `native_map_clear` for the HashMap
/// family (which `Hashtable` shares), `lhm_set("size")` and
/// [`lhm_move_to_tail`] for `LinkedHashMap`, and `tm_set_slot` for `TreeMap`.
/// `ConcurrentHashMap` bumps its SEGMENT's counter and never its own, which is
/// exactly why `view_source_generation` refuses a CHM source.
pub(crate) fn bump_map_mod_count(ctx: &dyn NativeContext, this: ObjectRef) {
    let Some(slot) = map_mod_count_slot(ctx, this) else {
        MAP_MODCOUNT_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        report_mod_count_refusal(ctx, this);
        return;
    };
    if let Value::Int(current) = ctx.get_field(this, slot) {
        ctx.set_field(this, slot, Value::Int(current.wrapping_add(1)));
        MAP_MODCOUNT_BUMPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    } else {
        MAP_MODCOUNT_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        report_mod_count_refusal(ctx, this);
    }
}

/// Read `this`'s current generation without moving it -- the twin of
/// [`bump_map_mod_count`], through the same slot resolver.
pub(crate) fn read_map_mod_count(ctx: &dyn NativeContext, this: ObjectRef) -> Option<i32> {
    match ctx.get_field(this, map_mod_count_slot(ctx, this)?) {
        Value::Int(v) => Some(v),
        _ => None,
    }
}

/// Put back a generation captured by [`read_map_mod_count`], undoing bumps a
/// purely internal representation change made along the way.
pub(crate) fn restore_map_mod_count(ctx: &mut dyn NativeContext, this: ObjectRef, gen: Option<i32>) {
    let (Some(gen), Some(slot)) = (gen, map_mod_count_slot(&*ctx, this)) else {
        return;
    };
    ctx.set_field(this, slot, Value::Int(gen));
}

/// The slot this receiver keeps its structural-modification counter in, or
/// `None` when it keeps none *within its own allocated fields*.
///
/// The bound is the point, and it is the twin of the one [`map_size_slot`]
/// already applies. `modCount` is declared by `java.util.HashMap`, so
/// `resolve_field_index_by_class_id` answers a slot for every receiver in the
/// family -- including ones allocated with fewer fields than the class
/// declares, where a write is dropped and a read answers a constant. The
/// by-name accessors this replaced (`get_field_by_name`/`set_field_by_name`)
/// are that same resolver WITHOUT the bound, which is how a reader and a
/// writer could silently end up on different storage.
///
/// A frozen generation is worse than a missing one: [`view_source_generation`]
/// reads `Some(g)` as "this map reports when it changes", so a constant makes
/// the keySet-view rebuild elision skip every resync for the life of the map.
pub(crate) fn map_mod_count_slot(ctx: &dyn NativeContext, this: ObjectRef) -> Option<usize> {
    let class_id = ctx.class_id_of_object(this);
    // Memoized per `(vm, class)`: the raw resolver hashes the name and walks
    // the superclass chain under the class-manager read lock, and this now
    // runs on the integer-overlay put -- which `jit_overlay_hashmap_put`
    // reaches from compiled code, and whose whole reason to exist is that it
    // touches nothing but a Rust side table. Same memo the iterator path has
    // used for this question since perf/collections-classification-cost.
    thread_local! {
        pub(crate) static MEMO: ClassMemo<usize> = const { std::cell::RefCell::new(Vec::new()) };
    }
    let slot = MEMO.with(|t| memoized_field_slot(ctx, t, class_id, "modCount"))?;
    (slot < ctx.object_num_fields(this)).then_some(slot)
}

/// Structural mutations that moved a readable `modCount`.
pub(crate) static MAP_MODCOUNT_BUMPED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Structural mutations whose receiver had no `modCount` slot of its own to
/// move. Every one of these is a map [`view_source_generation`] must refuse,
/// and does -- `map_itr_mod_count` answers `None` for exactly this set.
pub(crate) static MAP_MODCOUNT_REFUSED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Name the first receiver of each class whose `modCount` could not be moved,
/// under `CRATONVM_DBG=map-view-cache`. The allocated field count is printed
/// beside the resolved slot because the two ways to lose the field look
/// identical from the call site: a name that resolves to no slot at all, and a
/// slot that resolves past what the object was actually allocated with.
pub(crate) fn report_mod_count_refusal(ctx: &dyn NativeContext, this: ObjectRef) {
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_MAP_VIEW_CACHE").is_none() {
        return;
    }
    let cid = ctx.class_id_of_object(this);
    pub(crate) static SEEN: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());
    let Ok(mut seen) = SEEN.lock() else {
        return;
    };
    let key = cid.as_u32();
    if seen.contains(&key) {
        return;
    }
    seen.push(key);
    let name = ctx
        .class_name_of_id(cid)
        .unwrap_or_else(|| format!("<cid {cid:?}>"));
    let nf = ctx.object_num_fields(this);
    let unbounded = ctx.resolve_field_index_by_class_id(cid, "modCount");
    eprintln!(
        "[MAP-VIEW-CACHE] modCount UNMOVABLE on {name}: num_fields={nf} resolved_slot={unbounded:?}"
    );
}

/// `Map.put` for a direct Rust caller (about thirty internal ones, plus
/// `native_map_put_pub`'s callers in `native-builtins`): a table resize gets
/// one no-GC attempt ([`GrowAlloc::SingleAttempt`]), because such a caller may
/// hold bare references across the put. The registered door is
/// [`native_map_put_entry`].
pub(crate) fn native_map_put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    map_put_impl(ctx, args, GrowAlloc::SingleAttempt)
}

/// The registered `put` (`HashMap`, `Map`, `Hashtable`, `Properties`,
/// `Properties.setProperty`): reached only from Java dispatch or
/// `invoke_virtual`, so a resize may collect and retry ([`GrowAlloc::Reclaim`];
/// gc-common w9-d).
pub(crate) fn native_map_put_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    map_put_impl(ctx, args, GrowAlloc::Reclaim)
}

pub(crate) fn map_put_impl(ctx: &mut dyn NativeContext, args: &[Value], grow: GrowAlloc) -> MethodCallResult {
    // THE MISSING ROUTE. `native_map_get`, `native_map_remove` and
    // `native_map_contains_key` all open with `if is_tree_map_receiver(..)
    // { return native_tm_*(..) }`; this one did not, and it is the one WRITE
    // every generic body performs. So `native_map_replace` read a `TreeMap`
    // correctly through `native_map_get` and then wrote its answer into
    // `HashMap` buckets the receiver does not have -- the map was unchanged and
    // `replace` reported the update anyway (apps/probes/TreeShadowSweep 212-213).
    //
    // Deliberately here rather than inside `native_map_put_evict`: the `evict`
    // flag is `LinkedHashMap.removeEldestEntry`'s, and a TreeMap has no eldest.
    // An immutable JDK stand-in (`Collections$EmptyMap`, `$SingletonMap`)
    // refuses — see `is_immutable_jdk_stand_in`. Ahead of the TreeMap route
    // because neither of those is a TreeMap and the refusal is unconditional.
    if let Some(Value::Object(Some(this))) = args.first() {
        if is_immutable_jdk_stand_in(ctx, *this) {
            return Err(
                cratonvm_types::error::RuntimeError::UnsupportedOperationException {
                    message: String::new(),
                }
                .into(),
            );
        }
    }
    if let Some(Value::Object(Some(this))) = args.first() {
        if is_tree_map_receiver(ctx, *this) {
            // The TreeMap route keeps this door's policy (gc-common w11-c).
            return tm_put_impl(ctx, args, grow);
        }
    }
    native_map_put_evict_grow(ctx, args, true, grow)
}

/// [`native_map_put_evict_grow`] with the historical single-attempt resize.
pub(crate) fn native_map_put_evict(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    evict: bool,
) -> MethodCallResult {
    native_map_put_evict_grow(ctx, args, evict, GrowAlloc::SingleAttempt)
}

/// Like `native_map_put`, but with an explicit eviction-hook toggle. Real
/// HotSpot's `HashMap.readObject` restores entries via
/// `putVal(hash, key, value, false, false)` -- the trailing `evict=false`
/// suppresses `afterNodeInsertion`'s `removeEldestEntry` callback while the
/// object graph is still being reconstructed (a `LinkedHashMap` subclass's
/// own fields -- e.g. an anonymous inner class's synthetic `this$0` -- may
/// not be restored yet). `native_hashmap_read_object` passes `evict=false`
/// for exactly this reason; every other caller wants ordinary evict=true
/// put semantics via `native_map_put`. `grow` is the resize policy
/// ([`GrowAlloc`]), passed through to [`map_resize`].
pub(crate) fn native_map_put_evict_grow(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    evict: bool,
    grow: GrowAlloc,
) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    if is_bare_java_lang_object(ctx, this) {
        // Defensive interface-native guard: a java/util/Map.put dispatch on a
        // plain Object must not synthesize a HashMap bucket layout onto a
        // zero-field receiver.
        return Ok(Some(Value::Object(None)));
    }
    // If a generic `java/util/Map.put` interface native is dispatched on an
    // unmodifiable wrapper receiver, honour the JDK contract and throw rather
    // than mutating (or, worse, `map_resize`-clobbering) the private backing.
    if is_unmod_wrapper(ctx, this) {
        return Err(unsupported_op());
    }
    // The `Hashtable` null axis, VALUE BEFORE KEY. `Hashtable.put` reads:
    //
    // ```java
    // // Make sure the value is not null
    // if (value == null) { throw new NullPointerException(); }
    // // Makes sure the key is not already in the hashtable.
    // Entry<?,?> tab[] = table;
    // int hash = key.hashCode();
    // ```
    //
    // so `put(null, null)` is message-less (MEASURED), not the helpful-NPE
    // text — the order is observable and must be preserved. A plain
    // `Hashtable` receiver never reaches the CHM / LHM / TreeMap redirects
    // below, so testing here costs nothing on those paths.
    ht_reject_null_value(ctx, this, args.get(2))?;
    ht_reject_null_key(ctx, this, args.get(1))?;
    // A `java/util/Map.put` interface native dispatched on a
    // ConcurrentHashMap receiver must use the segmented CHM path — the
    // plain-HashMap bucket code would treat the segments array as buckets.
    if is_chm_receiver(ctx, this) {
        // A JDK-model map (nio21 w1 L4): its own `put`, the JDK's bytecode.
        if let Some(r) = chm_jdk_delegate(
            ctx,
            args,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            2,
        ) {
            return r;
        }
        return native_chm_put(ctx, args);
    }
    // S111r34: when the receiver is a LinkedHashMap (or subclass like
    // `org/springframework/core/annotation/AnnotationAttributes`),
    // delegate to `native_lhm_put` so that subsequent `LinkedHashMap.get`
    // calls — which JDK25 overrides and we redirect to `native_lhm_get`
    // (reads from the LHM overlay) — find the entries. Without this
    // redirect, the put writes to slot-0-based storage which the LHM
    // `get` native cannot see, and Spring's `AnnotationAttributes.get(...)`
    // returns null for every key written via `TypeMappedAnnotation.asMap`.
    // Surfaces as `IllegalArgumentException: Attribute 'type' not found
    // in attributes for annotation [...ComponentScan$Filter]` in
    // `ComponentScanAnnotationParser.parse` for `@SpringBootApplication`.
    let cid = ctx.class_id_of_object(this);
    // `mut`: `materialize_hm_int_fast` below can collect, so these are re-read
    // from their pins across it (HIB-MAPPUT-PINORDER.1).
    let mut key_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let mut value = args.get(2).copied().unwrap_or(Value::Object(None));
    // PERF (collections-classification-cost): this block used to be a
    // `class_name_of_id` for the exact-HashMap test PLUS a second full
    // superclass walk in the `else` arm, each hop of which allocated a
    // `String` under the `class_manager` read lock — on top of the three
    // predicate walks above. All of it is now bits out of the per-`ClassId`
    // memo. `class_name_rc` is consulted only for the `Hashtable` arm, which
    // genuinely needs the name string.
    let facts = class_facts(ctx, cid);
    if facts.has(CF_HAS_NAME) {
        if facts.has(CF_EXACT_HASHMAP) {
            // Exact dispatch is installed only after the callsite becomes
            // hot. Start the integer overlay during the generic tiering
            // window; once real nodes exist the fresh-map guard must refuse
            // the overlay to preserve the already-materialized contents.
            if evict {
                if let Some(result) = try_hm_int_fast_put(ctx, this, key_val, value) {
                    return result;
                }
            }
            // A non-integer put (or serialization put with `evict=false`)
            // transitions an overlay-backed exact HashMap to its ordinary JDK
            // node table before inserting the new entry. Without this, the
            // node path sees an empty heap map and silently strands all prior
            // integer entries in the side store.
            // HIB-MAPPUT-PINORDER.1 (2026-07-31): `materialize_hm_int_fast`
            // re-puts every side-stored entry through
            // `native_map_put_evict_pinned`, which allocates a node per entry —
            // so it can collect. `key_val`/`value` were copied out of `args`
            // above and are bare Rust locals here; the pins that protect them
            // are not taken until after this call, and PINNING AN ALREADY-STALE
            // ADDRESS PRESERVES THE STALENESS (see the pin-time canary in
            // `vm_exec::pin_native_root`). Pin them across the materialisation
            // and re-read afterwards, so the pins taken below root the current
            // addresses. `this` is already handled — it comes back as the
            // return value.
            let mat_base = ctx.pin_native_root(this);
            let mat_key = pin_value(ctx, key_val);
            let mat_val = pin_value(ctx, value);
            let materialized = materialize_hm_int_fast(ctx, this);
            this = unpin_on_err(ctx, mat_base, materialized)?;
            key_val = read_pinned_elem(ctx, mat_key, key_val);
            value = read_pinned_elem(ctx, mat_val, value);
            ctx.unpin_native_roots(mat_base);
        } else {
            // Walk parent chain to detect LinkedHashMap or TreeMap ancestry.
            // Without the TreeMap branch, the `java/util/Map.put` interface
            // override (registered as an abstract-method native) falls
            // through to the HashMap-bucket code below for a TreeMap
            // receiver. TreeMap's field 0 is `root` (a TreeMap.Entry),
            // not a bucket array, so `map_state`/`map_resize` corrupt the
            // object and every subsequent put is a silent no-op. Symptom:
            // `new TreeMap().put(k,v)` leaves size=0 and get returns null,
            // which broke Keycloak FeatureOptions.<clinit> (the JDK
            // TreeMap.keySpliteratorFor NPE was downstream — actual data
            // never reached the tree).
            // PERF (collections-classification-cost): this was a fourth full
            // `class_name_of_id` walk on the `put` hot path. Its three outcomes
            // are `CF_LHM`, `CF_TREE_MAP` and `CF_HASHTABLE_ANCESTRY` in the
            // per-`ClassId` memo, which decide identically:
            //
            //   * `LinkedHashMap` and `TreeMap` live in disjoint hierarchies
            //     (`LinkedHashMap extends HashMap`, `TreeMap extends
            //     AbstractMap`), and `Hashtable extends Dictionary` is disjoint
            //     from both, so no chain contains two of them. The extra
            //     sentinels each arm listed can therefore never fire ahead of
            //     the arm that matters, and dropping them changes nothing.
            //   * The Hashtable arm needs `Properties` INCLUDED — it must still
            //     run `ensure_hashtable_load_factor` — which is exactly why
            //     `CF_HASHTABLE_ANCESTRY` exists alongside the
            //     `Properties`-excluding `CF_HASHTABLE_LAYOUT`.
            if facts.has(CF_LHM) {
                return native_lhm_put_evict(ctx, args, evict, grow);
            }
            if facts.has(CF_TREE_MAP) {
                return tm_put_impl(ctx, args, grow);
            }
            if facts.has(CF_HASHTABLE_ANCESTRY) {
                // Hashtable/Properties run the plain-bucket path below (table +
                // count get set), but their OWN loadFactor/threshold fields
                // (they extend Dictionary, not HashMap) are never initialized.
                //
                // This arm is the only consumer of the receiver's name string
                // here, and it is off the common path, so it pays a memoized
                // `class_name_rc` rather than keeping the whole block's
                // `class_name_of_id` alive.
                if let Some(name) = class_name_rc(ctx, cid) {
                    ensure_hashtable_load_factor(ctx, this, &name);
                }
            }
        }
    }
    // `map_hash_key` / `map_keys_equal` dispatch arbitrary Java code. When
    // this path is reached by a direct native-to-native call (for example
    // HashSet.add -> HashMap.put while Selector.selectedKeys() builds a set),
    // `safe_native_call` has not pinned this inner map receiver or the
    // key/value locals. Pin the whole put window so a stress-GC during
    // hashCode()/equals() cannot leave the later bucket/node writes using stale
    // ObjectRefs.
    let put_pin_base = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key_val);
    let value_pin = pin_value(ctx, value);
    let result = native_map_put_evict_pinned(
        ctx,
        &mut this,
        key_val,
        value,
        evict,
        put_pin_base,
        key_pin,
        value_pin,
        grow,
    );
    ctx.unpin_native_roots(put_pin_base);
    result
}

/// Exact-class HashMap put entry used after the VM has already guarded the
/// receiver ClassId. This skips the generic Map/CHM/LHM/TreeMap classification
/// while retaining the identical pinning, resize, node, and eviction logic.
pub fn native_hashmap_put_exact(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let mut key_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let mut value = args.get(2).copied().unwrap_or(Value::Object(None));
    // H7-1 O2/N3: `native_hashmap_get_exact` skips the overlay for an
    // `IdentityHashMap` receiver (it keys by CONTENT, which collapses
    // distinct `==`-unequal `Integer`s of equal value); this put path called
    // the overlay unconditionally, so a `put` could file such a receiver's
    // keys where the matching `get` would refuse to read them back.
    let identity_mode = is_identity_map_receiver(ctx, this);
    if !identity_mode {
        if let Some(result) = try_hm_int_fast_put(ctx, this, key_val, value) {
            return result;
        }
    }
    // HIB-MAPPUT-PINORDER.1 — same ordering hazard as `native_map_put_evict`:
    // `materialize_hm_int_fast` allocates a node per side-stored entry, so it
    // can collect, and the pins below would otherwise root already-stale
    // key/value addresses.
    let mat_base = ctx.pin_native_root(this);
    let mat_key = pin_value(ctx, key_val);
    let mat_val = pin_value(ctx, value);
    let materialized = materialize_hm_int_fast(ctx, this);
    let mut this = unpin_on_err(ctx, mat_base, materialized)?;
    key_val = read_pinned_elem(ctx, mat_key, key_val);
    value = read_pinned_elem(ctx, mat_val, value);
    ctx.unpin_native_roots(mat_base);
    let put_pin_base = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key_val);
    let value_pin = pin_value(ctx, value);
    // gc-common w9-d: `GrowAlloc::Reclaim` -- this is a native-call door (the
    // JIT's `jit_hashmap_put_direct` enters it through
    // `safe_native_call_prevalidated_objects`) with no direct Rust caller, so
    // nothing above holds a bare reference across it, and the put window just
    // pinned covers everything the put uses after a resize.
    let result = native_map_put_evict_pinned(
        ctx,
        &mut this,
        key_val,
        value,
        true,
        put_pin_base,
        key_pin,
        value_pin,
        GrowAlloc::Reclaim,
    );
    ctx.unpin_native_roots(put_pin_base);
    result
}

pub(crate) fn native_map_put_evict_pinned(
    ctx: &mut dyn NativeContext,
    this: &mut ObjectRef,
    key_val: Value,
    value: Value,
    evict: bool,
    put_pin_base: usize,
    key_pin: usize,
    value_pin: usize,
    grow: GrowAlloc,
) -> MethodCallResult {
    native_map_put_evict_pinned_hashed(
        ctx,
        this,
        key_val,
        value,
        evict,
        put_pin_base,
        key_pin,
        value_pin,
        grow,
        None,
        false,
    )
}

/// [`native_map_put_evict_pinned`] with the key's [`map_hash_key`] optionally
/// already in hand (`known_hash`), so the put does not call `hashCode()` a
/// second time. Only a caller that hashed with `map_hash_key` may pass one,
/// and it is honoured only on the plain `HashMap` hash path (not an
/// `IdentityHashMap` or a `Hashtable` receiver, which hash differently).
/// The `ConcurrentHashMap` segment put is the caller
/// (`CRATONVM_COMPAT_CHM_SINGLE_HASH`, [`chm_single_hash_enabled`]).
///
/// `only_if_absent` is `putVal`'s `onlyIfAbsent`: a key already mapped to a
/// non-null value keeps it and the call answers that value, so `putIfAbsent`
/// is one hash and one walk ([`map_put_if_absent_single_walk`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn native_map_put_evict_pinned_hashed(
    ctx: &mut dyn NativeContext,
    this: &mut ObjectRef,
    key_val: Value,
    value: Value,
    evict: bool,
    put_pin_base: usize,
    key_pin: usize,
    value_pin: usize,
    grow: GrowAlloc,
    known_hash: Option<i32>,
    only_if_absent: bool,
) -> MethodCallResult {
    native_map_put_evict_pinned_inner(
        ctx,
        this,
        key_val,
        value,
        evict,
        put_pin_base,
        key_pin,
        value_pin,
        grow,
        known_hash,
        only_if_absent,
        PutMode::default(),
    )
}

/// How [`native_map_put_evict_pinned_inner`] departs from an ordinary `put`
/// (round 13 wave 11, lane compat6). The default is the ordinary `put`.
#[derive(Clone, Copy, Default)]
pub(crate) struct PutMode {
    /// Never resize an existing table: [`materialize_hm_int_fast`] re-puts an
    /// integer overlay into a table already sized to the overlay's JDK table,
    /// which a compute-family insert can leave one entry past its threshold
    /// (the JDK grows on the NEXT insert, not on this re-representation).
    pub(crate) no_grow: bool,
    /// The caller has just proved the key absent with a walk of its own (and
    /// nothing structural happened since): link at the chain's tail without
    /// comparing keys again, as the JDK's single-walk bodies do
    /// (`pred.next = new Node(..)`). [`chm_seg_remap_single_walk`].
    pub(crate) assume_absent: bool,
}

/// [`native_map_put_evict_pinned_hashed`] with a [`PutMode`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn native_map_put_evict_pinned_inner(
    ctx: &mut dyn NativeContext,
    this: &mut ObjectRef,
    key_val: Value,
    value: Value,
    evict: bool,
    put_pin_base: usize,
    key_pin: usize,
    value_pin: usize,
    grow: GrowAlloc,
    known_hash: Option<i32>,
    only_if_absent: bool,
    mode: PutMode,
) -> MethodCallResult {
    let no_grow = mode.no_grow;
    // `IdentityHashMap` receiver: use reference-identity hash/equality
    // (`CF_IDENTITY_MAP`) instead of virtual `hashCode()`/`equals()` — see
    // `map_hash_key_identity`. `this` is fresh here (just pinned by the
    // caller, no GC-triggering call has run yet), so this read is safe.
    let identity_mode = is_identity_map_receiver(ctx, *this);
    // `CRATONVM_COMPAT_MAP_JDK_ORDER`: see [`map_jdk_order_enabled`].
    let jdk_order = map_jdk_order_enabled();
    let ht_mode = !identity_mode && hashtable_order(ctx, *this);

    // Handle null key: hash=0, bucket=0, key field stores null
    // hashCode() can collect before this helper reaches its first field access;
    // refresh the caller's already-pinned key before dispatching it.
    let key_for_hash = read_pinned_elem(ctx, key_pin, key_val);
    let (hash, is_null_key) = match key_for_hash {
        Value::Object(Some(k)) => (
            if identity_mode {
                map_hash_key_identity(ctx, k)
            } else if let (Some(h), false) = (known_hash, ht_mode) {
                h
            } else if ht_mode {
                map_key_raw_hash(ctx, k)?
            } else {
                map_hash_key(ctx, k)?
            },
            false,
        ),
        Value::Object(None) => (0, true),
        _ => return Ok(Some(Value::Object(None))), // non-object keys not supported
    };
    *this = ctx.read_native_pin(put_pin_base, *this);
    let key_val = read_pinned_elem(ctx, key_pin, key_val);
    let value = read_pinned_elem(ctx, value_pin, value);
    let key_ref = match key_val {
        Value::Object(Some(k)) => Some(k),
        Value::Object(None) if is_null_key => None,
        _ => return Ok(Some(Value::Object(None))),
    };

    // Check for resize first. Also initialize table when buckets is None
    // (e.g. AnnotationAttributes constructed via JDK bytecode constructor —
    // LinkedHashMap.<init>() leaves `table` null until first put, but our
    // synthetic put native previously returned a silent no-op when buckets
    // were None, dropping `type` from `@ComponentScan.Filter` AnnotationAttributes
    // and surfacing as `IllegalArgumentException: Attribute 'type' not found`
    // deep in Spring's bean factory).
    //
    // Under `jdk_order` only the missing table is materialised here; growth
    // waits until the walk below has proved the key absent (see the block
    // after the walk). Growing first also grew on an OVERWRITE at the
    // threshold, which the JDK never does.
    let (initial_buckets, size, cap) = map_state(ctx, *this);
    // `map_grow_threshold`: `(cap * 3) / 4` unless the receiver carries its
    // own load factor (`CRATONVM_COMPAT_MAP_LOAD_FACTOR`).
    if initial_buckets.is_none()
        || (!no_grow && !jdk_order && size + 1 > map_grow_threshold(ctx, *this, cap))
    {
        map_resize(ctx, this, grow);
    }

    let (buckets, size, cap) = map_state(ctx, *this);
    let buckets = match buckets {
        Some(b) => b,
        None => return Ok(Some(Value::Object(None))),
    };
    // `map_keys_equal` below can run arbitrary Java code before `buckets` is
    // used again to link a newly allocated node. Root the array now, while the
    // address read from the refreshed map is still current. Pinning it only
    // after the chain walk merely preserved an already-forwarded from-space
    // address (the repaired WildFly canary caught exactly that ordering).
    let buckets_pin = ctx.pin_native_root(buckets);

    let idx = map_bucket_index(hash, cap);
    let mut node_val = ctx.get_array_element(buckets, idx);

    // `CRATONVM_COMPAT_HASHMAP_TREE_BINS` (the tree-bin section at
    // `HM_TREE_NODE_CLASS`): a tree bin takes `putVal`'s tree arm, and a chain
    // that already holds `TREEIFY_THRESHOLD` plain nodes is treeified after
    // this insert appends to it (`binCount >= TREEIFY_THRESHOLD - 1`).
    let mut treeify_after_insert = false;
    if !identity_mode && !ht_mode && hm_tree_family(ctx, *this) {
        let objects = matches!(key_val, Value::Object(_)) && matches!(value, Value::Object(_));
        if hm_tree_head(&*ctx, node_val).is_some() {
            if objects {
                return hm_tree_bin_put(
                    ctx,
                    this,
                    put_pin_base,
                    idx,
                    hash,
                    key_pin,
                    key_val,
                    value_pin,
                    value,
                    only_if_absent,
                    grow,
                );
            }
            // A primitive-valued (Rust-private) mapping cannot live in a
            // tree: the bin becomes a chain again, and the walk below runs
            // over it.
            hm_untreeify_bin(ctx, *this, idx)?;
            *this = ctx.read_native_pin(put_pin_base, *this);
            let buckets_now = ctx.read_native_pin(buckets_pin, buckets);
            node_val = ctx.get_array_element(buckets_now, idx);
        } else if objects && !no_grow {
            // Pure reads, the census first: `hm_tree_may_treeify` takes the
            // overlay shard lock, which the common short chain never pays.
            let (len, all_plain) = hm_chain_census(ctx, node_val, chain_walk_limit(size));
            if len >= HM_TREEIFY_THRESHOLD && all_plain && hm_tree_may_treeify(ctx, *this) {
                // Can load a class (first call per VM): re-read the head.
                treeify_after_insert = hm_tree_classes_real(ctx);
                *this = ctx.read_native_pin(put_pin_base, *this);
                let buckets_now = ctx.read_native_pin(buckets_pin, buckets);
                node_val = ctx.get_array_element(buckets_now, idx);
            }
        }
    }

    // Walk chain looking for existing key (S111r27: layout-aware).
    // Safety: cap the chain walk to detect pathological cases (cycles or
    // O(n^2) blowup from massive single-bucket pile-ups). Real chains
    // should be O(log n) even with poor hashes; anything past 4096 is a
    // strong signal of a cycle/corruption.
    //
    // MED fix: tripping the cap previously continued silently with a head
    // insert, producing duplicate keys in the chain and breaking the map's
    // basic key-uniqueness invariant. An attacker who can force >4096
    // colliding keys (hash-collision DoS) thus turned each subsequent
    // `put` into a degenerate O(n) duplicate-insert with no error path.
    // We now throw `IllegalStateException` instead, signalling the
    // attack/corruption to the caller rather than silently producing
    // wrong data.
    let mut walk_count: usize = 0;
    // Track the last node visited so a not-found key can be appended at the
    // tail of the chain (HotSpot HashMap.putVal semantics), not prepended.
    let mut tail_node: Option<ObjectRef> = None;
    // `chain_walk_limit`: 4096, or the map's size when that is larger
    // (`CRATONVM_COMPAT_MAP_LONG_CHAINS`).
    let chain_walk_cap = chain_walk_limit(size);
    while let Value::Object(Some(mut node)) = node_val {
        walk_count += 1;
        tail_node = Some(node);
        if walk_count > chain_walk_cap {
            eprintln!(
                "[HM-PUT-GUARD] aborting chain walk at {} nodes (suspected cycle); map={:?} idx={} cap={}",
                walk_count, *this, idx, cap
            );
            return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                message: "hashmap chain exceeded safety cap; possible hash-collision DoS"
                    .to_string(),
            }
            .into());
        }
        let node_hash = get_node_hash(ctx, node);
        let node_key_field = get_node_key(ctx, node);
        if dbg_hmput() {
            eprintln!(
                "[HMPUT] walk node_key_field={:?} key_ref={:?} hash_arg={} is_null={}",
                node_key_field, key_ref, hash, is_null_key
            );
        }
        if mode.assume_absent {
            // The caller's own walk proved the key absent: only the tail is
            // wanted (`PutMode::assume_absent`).
        } else if is_null_key {
            // Looking for a null-key node
            if matches!(node_key_field, Value::Object(None)) {
                let old_value = get_node_value(ctx, node);
                if only_if_absent && !matches!(old_value, Value::Object(None)) {
                    return Ok(Some(old_value));
                }
                // `map_resize` above may have collected after the initial
                // value read even though null-key lookup itself has no
                // `equals` callback.
                let value = read_pinned_elem(ctx, value_pin, value);
                // Update value in-place using the detected layout
                match ctx.get_field(node, 0) {
                    Value::Object(_) => ctx.set_field(node, 1, value), // legacy value slot 1
                    _ => ctx.set_field(node, 2, value), // JDK value slot 2 (our nodes)
                }
                return Ok(Some(old_value));
            }
        } else if node_hash == hash {
            let Value::Object(Some(node_key)) = node_key_field else {
                node_val = ctx.get_field(node, NODE_FIELD_NEXT);
                continue;
            };
            // equals() is arbitrary Java code and can trigger a moving GC.
            // Refresh the chain node before its next-link dereference below;
            // the old raw `node` was observed stale during WildFly extension
            // provider construction.
            let node_pin = ctx.pin_native_root(node);
            let node_key_pin = ctx.pin_native_root(node_key);
            let node_key = ctx.read_native_pin(node_key_pin, node_key);
            let key_for_eq = match read_pinned_elem(ctx, key_pin, key_val) {
                Value::Object(Some(k)) => k,
                _ => {
                    ctx.unpin_native_roots(node_key_pin);
                    ctx.unpin_native_roots(node_pin);
                    return Ok(Some(Value::Object(None)));
                }
            };
            // JDK HashMap.putVal compares the caller's SEARCH key against the
            // stored key: `key.equals(k)`, not `k.equals(key)`. The direction
            // is observable for asymmetric equality implementations and was
            // load-bearing for ANTLR DFA-state canonicalization.
            //
            // `IdentityHashMap` receiver: reference identity, not `equals()`.
            // `Hashtable` (under `jdk_order`): `e.key.equals(key)` instead.
            let eq = if identity_mode {
                map_keys_equal_identity(key_for_eq, node_key)
            } else if ht_mode {
                hashtable_keys_equal(ctx, key_for_eq, node_key)?
            } else {
                map_keys_equal(ctx, key_for_eq, node_key)?
            };
            node = ctx.read_native_pin(node_pin, node);
            ctx.unpin_native_roots(node_key_pin);
            ctx.unpin_native_roots(node_pin);
            tail_node = Some(node);
            if dbg_hmput() {
                eprintln!(
                    "[HMPUT] map_keys_equal(key={:?}, node_key={:?}) = {}",
                    key_for_eq, node_key, eq
                );
            }
            if eq {
                let old_value = get_node_value(ctx, node);
                if only_if_absent && !matches!(old_value, Value::Object(None)) {
                    return Ok(Some(old_value));
                }
                // `map_keys_equal` above can run arbitrary Java code. Refresh
                // the caller-pinned replacement immediately before updating
                // the existing node; the earlier pre-walk copy is stale after
                // any GC completed inside `equals`.
                let value = read_pinned_elem(ctx, value_pin, value);
                match ctx.get_field(node, 0) {
                    Value::Object(_) => ctx.set_field(node, 1, value), // legacy value slot 1
                    _ => ctx.set_field(node, 2, value), // JDK value slot 2 (our nodes)
                }
                return Ok(Some(old_value));
            }
        }
        let next = ctx.get_field(node, NODE_FIELD_NEXT);
        // Self-cycle guard: if node.next == node, abort immediately.
        if let Value::Object(Some(nx)) = next {
            if std::ptr::eq(nx.as_ptr(), node.as_ptr()) {
                eprintln!(
                    "[HM-PUT-GUARD] self-cycle node detected at walk {}; map={:?} idx={}",
                    walk_count, *this, idx
                );
                break;
            }
        }
        node_val = next;
    }

    // GC-safety: the chain-walk loop above can call `map_keys_equal`
    // (arbitrary Java `equals()`) an arbitrary number of times, each a
    // moving-GC risk. The loop already refreshes `node`/`node_key`/
    // `tail_node` after each such call, but NOT the enclosing `this` --
    // confirmed live via CRATONVM_DBG_STALE_OBJREF during WildFly
    // parallel-extension-add (`set_field` inside `set_map_size`, reached
    // from here, panicked on a stale `this` even after `this` was
    // "refreshed" from a `this_pin` seeded below with an already-stale
    // value). `put_pin_base` is the caller-provided pin for `this` and
    // stays valid for this whole function's lifetime, so re-read through
    // it here -- before it is used to seed the new `this_pin` below -- to
    // guarantee `this` is current regardless of how many GCs the walk
    // triggered. See wildfly-parallel-boot-stale-objectref-residual.md.
    *this = ctx.read_native_pin(put_pin_base, *this);

    // `jdk_order`: the key is absent, so NOW the table may grow. Resizing
    // before linking is equivalent to the JDK's order for both families:
    // `Hashtable.addEntry` rehashes first by definition, and `HashMap`'s
    // split keeps each old chain's relative order, so the tail of the new
    // node's new bucket is where `putVal` + `resize()` would have put it.
    // The table, bucket index and tail are re-derived from the grown map.
    // A bin about to be treeified keeps `putVal`'s own order instead: append,
    // `treeifyBin`, then `if (++size > threshold) resize()` (below).
    let grows = !no_grow
        && !treeify_after_insert
        && jdk_order
        && size + 1 > map_grow_threshold(ctx, *this, cap);
    let (buckets, buckets_pin, cap, idx, hash) = if grows {
        map_resize(ctx, this, grow);
        let mut hash = hash;
        if ht_mode {
            // `Hashtable.addEntry` re-reads `hash = key.hashCode()` after
            // `rehash()` -- a second, observable `hashCode()` call.
            if let Value::Object(Some(k)) = read_pinned_elem(ctx, key_pin, key_val) {
                hash = map_key_raw_hash(ctx, k)?;
            }
            *this = ctx.read_native_pin(put_pin_base, *this);
        }
        let (grown, _, grown_cap) = map_state(ctx, *this);
        let grown = match grown {
            Some(b) => b,
            None => return Ok(Some(Value::Object(None))),
        };
        let grown_pin = ctx.pin_native_root(grown);
        let grown_idx = map_bucket_index(hash, grown_cap);
        // A chain's nodes split into chains, never into a tree bin, so the
        // grown bin of a key whose old bin was a chain is one too. Should it
        // ever not be, a plain node must not be appended to a tree: take the
        // tree arm (it re-tests the key, which the walk proved absent).
        if !ht_mode
            && !identity_mode
            && hm_tree_family(ctx, *this)
            && hm_tree_head(&*ctx, ctx.get_array_element(grown, grown_idx)).is_some()
            && matches!(key_val, Value::Object(_))
            && matches!(value, Value::Object(_))
        {
            return hm_tree_bin_put(
                ctx,
                this,
                put_pin_base,
                grown_idx,
                hash,
                key_pin,
                key_val,
                value_pin,
                value,
                only_if_absent,
                grow,
            );
        }
        // Only the `HashMap` family appends; a `Hashtable` links at the head.
        // No Java code and no allocation below, so the raw locals are safe.
        tail_node = None;
        if !ht_mode {
            let mut nv = ctx.get_array_element(grown, grown_idx);
            let mut steps: usize = 0;
            while let Value::Object(Some(n)) = nv {
                steps += 1;
                if steps > chain_walk_cap {
                    return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                        message: "hashmap chain exceeded safety cap; possible corruption"
                            .to_string(),
                    }
                    .into());
                }
                tail_node = Some(n);
                nv = ctx.get_field(n, NODE_FIELD_NEXT);
            }
        }
        (grown, grown_pin, grown_cap, grown_idx, hash)
    } else {
        (buckets, buckets_pin, cap, idx, hash)
    };

    // Key not found — append at the TAIL of the chain. This matches HotSpot
    // HashMap.putVal (JDK 8+), which links the new node after the last bin
    // entry rather than prepending it. Tail-append makes within-bucket
    // iteration order equal to insertion order, reproducing HotSpot's
    // encounter order for keys that collide into the same bucket (the prior
    // head-prepend reversed them, the dominant source of HashMap iteration-
    // order divergence vs HotSpot across the gauntlet).
    //
    // gcstress residual face-1 fix — `alloc_object` below can trigger a moving
    // young GC (deterministically under CRATONVM_DBG_GC_STRESS) that relocates
    // every survivor. `this`, `buckets`, and `tail_node` are bare Rust locals
    // captured BEFORE this allocation, invisible to `collect_roots`; using the
    // stale post-move addresses for the `set_field`/`set_array_element`/
    // `set_map_size` writes below stores into the object's OLD (now
    // freed-and-reused) address, landing misaligned in whatever now occupies
    // it — the observed `{raw ptr, 0}` corrupt Value cells in a neighboring
    // live object (Fork6Hard$StrTask / Thread mirror). Pin the cross-allocation
    // roots and re-read them after the allocation.
    let this_pin = ctx.pin_native_root(*this);
    let tail_pin = tail_node.map(|t| ctx.pin_native_root(t));
    // `key_val`/`value` are also consumed AFTER the alloc (into new_node's
    // slots); the bare local copies would go stale even though the natives
    // args are pinned by safe_native_call, so pin+re-read them too.
    // hashCode()/equals() above may have moved the caller-owned key/value;
    // refresh their long-lived pins before taking the short-lived roots used
    // across `alloc_object`.
    let key_val = read_pinned_elem(ctx, key_pin, key_val);
    let value = read_pinned_elem(ctx, value_pin, value);
    let key_pin = pin_value(ctx, key_val);
    let value_pin = pin_value(ctx, value);
    // Create node — for null keys, store Value::Object(None) in key field.
    //
    // THE NODE'S CLASS IS THE DEFECT `H0-6` §7 MEASURED, and this line is its
    // single producer. `ClassId::new(0)` resolves to a
    // `cratonvm/synthetic/AnonymousObject$4` (`VmExec::alloc_object`), so the
    // table real `java/util/HashMap` bytecode walks is full of objects that are
    // not `HashMap$Node` and do not implement `Map.Entry`. Armed with
    // `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/HashMap`, `size()` is 3000 and
    // all 3000 `get()`s are right while `entrySet()` throws
    // `ClassCastException` after 0 and `keySet()` yields 1 — the nodes are
    // STORED and they are the wrong CLASS (`H0-4` §7, correcting §4's
    // "the inserts went to a side structure", which was wrong).
    //
    // [`map_node_class_for`] carries the whole argument for when the real class
    // is used and when this keeps the sentinel; read it before touching this
    // line. In one sentence: object-shaped mappings — every mapping bytecode
    // can see — get the real `java/util/HashMap$Node`; a mapping carrying a
    // primitive `Value` keeps the untyped carrier, because a real node's
    // `Ljava/lang/Object;` value slot would coerce that primitive to null.
    //
    // # What the previous revision of this comment said, and what was wrong
    //
    // It said the flip was MEASURED "necessary and NOT sufficient" — that on
    // top of the marker fix it still failed `probes/LinkedHashMapNodeProbe.java`
    // and `SetSurface` with `Map$Entry.getKey()` null and `keySet().remove`
    // leaving the map unshrunk — and concluded "whatever else this node's real
    // descriptors change has not been chased down". That measurement is
    // `f8aff25bb`, 2026-08-04. It is 17 days and one repair old:
    //
    //   * The `Map$Entry.getKey()`-is-null half was hypothesised (H16) to be
    //     the `java/util/Map$Entry` INTERFACE native — `native_entry_get_key`
    //     reads slot 0, which on a real node is `hash:I`, not the key, and the
    //     `Ljava/lang/Object;` return descriptor then nulls the `Int`.
    //     **MEASURED AND DISPROVED** on `cratonvm-r5.exe`, `--jdk-only`: a
    //     genuine reflectively-constructed `java.util.HashMap$Node` answers
    //     `getKey`/`getValue`/`setValue`/`hashCode` identically to HotSpot
    //     25.0.3+9, so that door does not open for a receiver whose own class
    //     declares the method. The arm's source is in `H16-2`, §2.
    //   * The `HashSet.remove`-reports-false half was the PRESENT marker, and
    //     `11798a8a2` (`hs_present_marker_at`, 2026-08-20) landed the real
    //     `java.util.HashSet.PRESENT` at all six HashSet-family producers —
    //     AFTER that measurement was taken.
    //
    // So the 2026-08-04 result is not evidence about the tree of 2026-08-21,
    // and the residual it named was never localised. `LinkedHashMapNodeProbe`
    // is still the guard and it PASSES on `cratonvm-r5.exe` under `--jdk-only`
    // today (H16, measured) — which is what makes a red after this change
    // attributable rather than ambient.
    //
    // The LinkedHashMap side is the standing existence proof: `lhm_alloc_node`
    // has bound its node to the REAL `java/util/LinkedHashMap$Entry` since
    // `7bf427af1`, and that probe's set-membership, map-view, serialization and
    // 2000-entry-resize sections all pass over it.
    let node_cid = map_node_class_for(ctx, *this, key_val, value);
    // `H23-2` §7.1's enforcement, written. `node_cid == 0` means this mapping
    // carries a primitive `Value` and gets the untyped carrier; if the table it
    // is about to head is typed to the class this receiver's REAL nodes use,
    // the pair would be the armed hybrid the invariant exists to forbid. Retype
    // the table down instead of typing the node up — a fabrication in an
    // `Object[]` is today's pre-`H23` state and reads back correctly, while a
    // fabrication in a `Node[]` misleads everything that trusts the component.
    //
    // `class_id_of_object` on a reference array answers the COMPONENT class id
    // (`VmHeap::class_id_of` returns the header field `new_ref_array` wrote),
    // so the test is one header read and no allocation on the hot path. It is
    // also self-limiting: the replacement array carries the sentinel id, which
    // is never equal to `want`.
    if node_cid == ClassId::new(0) {
        let want = map_carrier_class_for_receiver(ctx, *this);
        *this = ctx.read_native_pin(this_pin, *this);
        let buckets_now = ctx.read_native_pin(buckets_pin, buckets);
        if want != ClassId::new(0) && ctx.class_id_of_object(buckets_now) == want {
            degrade_bucket_table_to_untyped(ctx, this_pin, *this, buckets_pin, buckets_now, cap);
        }
    }
    // Every reference used below is re-read from a live pin, so the two
    // possibly-allocating calls above need no additional refresh here.
    let new_node = ctx.alloc_object(node_cid, NODE_NUM_FIELDS);
    // Keep the node and both object values rooted through population and
    // refresh every reference immediately before its store. The later
    // `[SETFIELD-GC]` epoch probe established that a plain ref store does not
    // itself complete a moving GC; these refreshes are retained as explicit
    // post-allocation provenance and defensive store-boundary discipline.
    let node_pin = ctx.pin_native_root(new_node);
    let key_val = read_pinned_elem(ctx, key_pin, key_val);
    ctx.set_field(new_node, NODE_FIELD_HASH, Value::Int(hash));
    ctx.set_field(new_node, NODE_FIELD_KEY, key_val);
    let new_node = ctx.read_native_pin(node_pin, new_node);
    let value = read_pinned_elem(ctx, value_pin, value);
    ctx.set_field(new_node, NODE_FIELD_VALUE, value);
    let new_node = ctx.read_native_pin(node_pin, new_node);
    ctx.set_field(new_node, NODE_FIELD_NEXT, Value::Object(None));
    let new_node = ctx.read_native_pin(node_pin, new_node);
    let buckets = ctx.read_native_pin(buckets_pin, buckets);
    let tail_node = tail_node.map(|t| ctx.read_native_pin(tail_pin.unwrap(), t));
    if ht_mode {
        // `Hashtable.addEntry`: `tab[index] = new Entry<>(hash, key, value, e)`
        // -- the new entry becomes the bucket HEAD, so enumeration (and the
        // next lookup's comparison order) sees the newest entry first.
        let head = ctx.get_array_element(buckets, idx);
        ctx.set_field(new_node, NODE_FIELD_NEXT, head);
        let new_node = ctx.read_native_pin(node_pin, new_node);
        let buckets = ctx.read_native_pin(buckets_pin, buckets);
        ctx.set_array_element(buckets, idx, Value::Object(Some(new_node)));
    } else {
        match tail_node {
            // Non-empty chain: link after the last node walked above.
            Some(t) => ctx.set_field(t, NODE_FIELD_NEXT, Value::Object(Some(new_node))),
            // Empty bucket: the new node becomes the chain head.
            None => ctx.set_array_element(buckets, idx, Value::Object(Some(new_node))),
        }
    }
    // GC-safety: the reference-typed `set_field`/`set_array_element` writes
    // just above can each trigger a write-barrier remembered-set allocation
    // (and therefore a moving young GC) when linking a young `new_node` into
    // an old-generation chain/bucket array -- confirmed live via
    // CRATONVM_DBG_STALE_OBJREF (`this` read stale inside `set_map_size`,
    // reached from `resync_view_set`'s per-key `native_map_put` loop during
    // WildFly parallel-extension-add). Re-read `this` from its pin
    // immediately before this last use.
    *this = ctx.read_native_pin(this_pin, *this);
    set_map_size(ctx, *this, size + 1);
    bump_map_mod_count(ctx, *this);
    ctx.unpin_native_roots(this_pin);

    if treeify_after_insert {
        // `putVal`: `if (binCount >= TREEIFY_THRESHOLD - 1) treeifyBin(tab,
        // hash);` after the append, then `if (++size > threshold) resize();`
        // (`treeifyBin` itself resizes a table below 64 slots instead).
        *this = ctx.read_native_pin(put_pin_base, *this);
        hm_treeify_bin(ctx, this, hash, grow)?;
        let (_, size_now, cap_now) = map_state(ctx, *this);
        if size_now > map_grow_threshold(ctx, *this, cap_now) {
            map_resize(ctx, this, grow);
        }
    }

    Ok(Some(Value::Object(None))) // no old value
}

/// `putVal` for a key whose bin (`idx` of `this`'s current table) is a tree
/// bin: the first-node test, then `((TreeNode) p).putTreeVal(this, tab, hash,
/// key, value)`; an existing mapping is overwritten (unless `only_if_absent`
/// finds a non-null value) and its old value answered, a new one is accounted
/// as `++modCount; if (++size > threshold) resize();`. `this_pin` roots
/// `this`, `key_pin`/`value_pin` the key and value, which must be objects.
/// `this` is refreshed. Round 13 wave 11, lane compat6.
#[allow(clippy::too_many_arguments)]
pub(crate) fn hm_tree_bin_put(
    ctx: &mut dyn NativeContext,
    this: &mut ObjectRef,
    this_pin: usize,
    idx: usize,
    hash: i32,
    key_pin: usize,
    key_val: Value,
    value_pin: usize,
    value: Value,
    only_if_absent: bool,
    grow: GrowAlloc,
) -> MethodCallResult {
    *this = ctx.read_native_pin(this_pin, *this);
    let Some(tab) = map_state(ctx, *this).0 else {
        return Ok(Some(Value::Object(None)));
    };
    let head = ctx.get_array_element(tab, idx);
    let Some(first) = hm_tree_head(&*ctx, head) else {
        return Ok(Some(Value::Object(None)));
    };
    // `if (p.hash == hash && ((k = p.key) == key || (key != null &&
    // key.equals(k)))) e = p;`
    let first_pin = ctx.pin_native_root(first);
    let first_matches = hm_node_matches_key(ctx, first_pin, first, hash, key_pin, key_val);
    let first = ctx.read_native_pin(first_pin, first);
    ctx.unpin_native_roots(first_pin);
    let existing = if first_matches? {
        Some(first)
    } else {
        // `else if (p instanceof TreeNode) e = ((TreeNode<K,V>)p).putTreeVal(..)`
        *this = ctx.read_native_pin(this_pin, *this);
        let Some(tab) = map_state(ctx, *this).0 else {
            return Ok(Some(Value::Object(None)));
        };
        let key_now = read_pinned_elem(ctx, key_pin, key_val);
        let value_now = read_pinned_elem(ctx, value_pin, value);
        let e = hm_tree_put_tree_val(ctx, first, *this, tab, hash, key_now, value_now)?;
        *this = ctx.read_native_pin(this_pin, *this);
        e
    };
    match existing {
        Some(e) => {
            // `V oldValue = e.value; if (!onlyIfAbsent || oldValue == null)
            // e.value = value; afterNodeAccess(e); return oldValue;`
            let old = get_node_value(ctx, e);
            if !only_if_absent || matches!(old, Value::Object(None)) {
                let value_now = read_pinned_elem(ctx, value_pin, value);
                set_node_value(ctx, e, value_now);
            }
            Ok(Some(old))
        }
        None => {
            let (_, size, cap) = map_state(ctx, *this);
            set_map_size(ctx, *this, size + 1);
            bump_map_mod_count(ctx, *this);
            if size + 1 > map_grow_threshold(ctx, *this, cap) {
                map_resize(ctx, this, grow);
            }
            Ok(Some(Value::Object(None)))
        }
    }
}

/// `p.hash == hash && ((k = p.key) == key || (key != null && key.equals(k)))`
/// for the node rooted at `node_pin` and the search key rooted at `key_pin`:
/// `HashMap`'s first-node test, the search key's `equals` asked of the stored
/// key ([`map_keys_equal`]). Can run Java; the caller re-reads the node.
pub(crate) fn hm_node_matches_key(
    ctx: &mut dyn NativeContext,
    node_pin: usize,
    node: ObjectRef,
    hash: i32,
    key_pin: usize,
    key_val: Value,
) -> Result<bool, MethodCallFailed> {
    let node = ctx.read_native_pin(node_pin, node);
    if get_node_hash(ctx, node) != hash {
        return Ok(false);
    }
    let stored = get_node_key(ctx, node);
    match read_pinned_elem(ctx, key_pin, key_val) {
        Value::Object(None) => Ok(matches!(stored, Value::Object(None))),
        Value::Object(Some(k)) => {
            let Value::Object(Some(sk)) = stored else {
                return Ok(false);
            };
            if std::ptr::eq(k.as_ptr(), sk.as_ptr()) {
                return Ok(true);
            }
            let sk_pin = ctx.pin_native_root(sk);
            let sk = ctx.read_native_pin(sk_pin, sk);
            let k = match read_pinned_elem(ctx, key_pin, key_val) {
                Value::Object(Some(k)) => k,
                _ => k,
            };
            let equal = map_keys_equal(ctx, k, sk);
            ctx.unpin_native_roots(sk_pin);
            equal
        }
        _ => Ok(false),
    }
}

pub(crate) fn native_map_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // RULE K. `Hashtable.get` is `int hash = key.hashCode();` on its second
    // line, so a null key never reaches a bucket. `HashMap.get(null)` is legal
    // and answers null, which is why this is receiver-routed rather than a
    // guard on the shared body. It also covers `Hashtable.getOrDefault`, whose
    // JDK body is `V result = get(key);` over this very native, and the
    // `compute*` natives, which open with a `native_map_get`.
    ht_reject_null_key(ctx, this, args.get(1))?;
    if is_tree_map_receiver(ctx, this) {
        return native_tm_get(ctx, args);
    }
    if is_chm_receiver(ctx, this) {
        if let Some(r) = chm_jdk_delegate(
            ctx,
            args,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            1,
        ) {
            return r;
        }
        return native_chm_get(ctx, args);
    }
    if is_lhm_receiver(ctx, this) {
        return native_lhm_get(ctx, args);
    }
    native_hashmap_get_exact(ctx, args)
}

/// Fast, allocation-free-from-the-Java-heap String-key lookup for real and
/// synthetic HashMaps. `map_hash_key` and `map_keys_equal` already recognize
/// Strings, but route through `read_string` twice and pin every bucket node in
/// case user code runs. String hash/equality are final, pure operations, so
/// handling them here avoids that native-call machinery while preserving the
/// exact Java `HashMap` hash spreading and content-equality contracts.
///
/// # Both operations are asked of the VM, not of a decoded `String`
///
/// This path used to derive the hash from `read_string(key).encode_utf16()` and
/// compare node keys as decoded `&str`. A Rust `str` cannot hold an unpaired
/// UTF-16 surrogate, so that decode substitutes `U+FFFD` — and the two halves
/// of the resulting divergence are both silent:
///
///   * `put` files the entry under [`map_hash_key`], which asks
///     `java_string_hash_code` and therefore uses the REAL units. `get` looked
///     in the `U+FFFD` bucket. MEASURED on 2026-08-17: `m.put(k, v)` followed
///     by `m.get(k)` — the *same object* — answered `null`. A map that accepts
///     a key and then cannot find it again is the shape that corrupts an
///     application without ever raising anything;
///   * two DIFFERENT keys whose only difference is which unpaired surrogate
///     they carry both decode to `U+FFFD`, so the `&str` comparison called
///     them equal and the walk would have returned the wrong node.
///
/// `java_string_hash_code` and `java_strings_equal` answer from the receiver's
/// own character storage, so both defects go away together — and the hash here
/// is now literally the same call `map_hash_key` makes, which is what makes
/// "`put` and `get` agree" structural rather than coincidental.
/// `java_string_hash_code` also IS the "is this a `java.lang.String`" gate it
/// replaces: the VM answers `None` for anything else.
pub(crate) fn native_hashmap_get_string_fast(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key: ObjectRef,
    unspread: bool,
) -> Option<MethodCallResult> {
    if let Some(value) = ctx.hashmap_string_node_cache_get_object(this, key) {
        return Some(Ok(Some(value)));
    }
    let raw_hash = ctx.java_string_hash_code(key)?;
    // The second memo is indexed by `&str`, so it may only be consulted for a
    // key whose decoded text is FAITHFUL. A decode containing no `U+FFFD` is
    // provably exact; one that does may be either a genuine `U+FFFD` or a
    // mangled surrogate, and those two must never share a memo entry. Skipping
    // the memo costs a chain walk; sharing it would resurrect the defect above
    // one layer up.
    let key_text = ctx.read_string(key).filter(|t| decode_is_faithful(t));
    if let Some(text) = key_text.as_deref() {
        if let Some(value) = ctx.hashmap_string_node_cache_get(this, text) {
            return Some(Ok(Some(value)));
        }
    }
    // `unspread`: a `Hashtable` under `CRATONVM_COMPAT_MAP_JDK_ORDER` indexes
    // with the raw `hashCode()` (see `map_key_raw_hash`).
    let hash = if unspread {
        raw_hash
    } else {
        raw_hash ^ ((raw_hash as u32) >> 16) as i32
    };
    let (buckets, chain_size, cap) = map_state(ctx, this);
    let buckets = buckets?;
    let mut node_value = ctx.get_array_element(buckets, map_bucket_index(hash, cap));
    for _ in 0..chain_walk_limit(chain_size) {
        let Value::Object(Some(node)) = node_value else {
            return Some(Ok(Some(Value::Object(None))));
        };
        if let Value::Object(Some(node_key)) = get_node_key(ctx, node) {
            match ctx.java_strings_equal(node_key, key) {
                Some(true) => {
                    if let Some(text) = key_text.as_deref() {
                        ctx.hashmap_string_node_cache_put(this, key, text, node);
                    }
                    return Some(Ok(Some(get_node_value(ctx, node))));
                }
                Some(false) => {}
                // A chain node whose key this comparison could not read is a
                // lookup this fast path did not make. Hand the whole thing back
                // to the general equality ladder — answering "absent" for a
                // pair that was never compared is the mistake
                // `chm-get-misses-stored-key-in-process-RETIRED-20260804.md`
                // records, and `native_chm_get_string_chain` refuses the same
                // way.
                None => return None,
            }
        }
        node_value = ctx.get_field(node, NODE_FIELD_NEXT);
    }
    Some(Err(
        cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "hashmap chain exceeded safety cap; possible corruption".to_string(),
        }
        .into(),
    ))
}

/// Whether a cached virtual-native call is a two-object map lookup.
///
/// Both callbacks accept exactly `(receiver, key)` and preserve their own
/// collision, equality, mutation and exception behavior. The interpreter uses
/// this identity check only to reuse its prevalidated cached-native plumbing
/// without re-resolving the already-known descriptor on every lookup.
pub fn is_hot_map_get_native_callback(callback: cratonvm_native_api::NativeCallback) -> bool {
    let callback = callback as usize;
    callback == native_map_get as usize || callback == native_chm_get as usize
}

/// Exact-class HashMap get entry used after a receiver-ClassId guard.
pub fn native_hashmap_get_exact(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key_val = args.get(1).copied().unwrap_or(Value::Object(None));
    // `IdentityHashMap` receiver: the string/int fast paths below key by
    // content (all equal-content Strings collapse to one entry, ignoring
    // `==`), which is wrong for an identity map. Skip straight to the
    // general path, which honours `CF_IDENTITY_MAP` below.
    let identity_mode = is_identity_map_receiver(ctx, this);
    // `Hashtable` under `CRATONVM_COMPAT_MAP_JDK_ORDER`: raw hash, and
    // `e.key.equals(key)` with no `==` short cut (see `hashtable_order`).
    let ht_mode = !identity_mode && hashtable_order(ctx, this);
    if !identity_mode {
        if let Value::Object(Some(key)) = key_val {
            if let Some(result) = native_hashmap_get_string_fast(ctx, this, key, ht_mode) {
                return result;
            }
        }
        if let Some(result) = try_hm_int_fast_get(ctx, this, key_val) {
            return result;
        }
    }
    let this = materialize_hm_int_fast(ctx, this)?;

    // GC-safety: `map_hash_key`/`map_keys_equal` dispatch arbitrary Java code
    // (hashCode()/equals()), which can trigger a moving GC. Pin the raw
    // receiver/key up front and re-read them after every such call, matching
    // `native_map_put_evict_pinned`'s established pattern. Caught live via
    // CRATONVM_DBG_STALE_OBJREF during WildFly parallel-extension-add: the
    // bucket-chain `node` local (and the search key reused across every
    // comparison) were captured before `map_keys_equal`'s `equals()`
    // dispatch and then reused afterward unpinned, both in the eventual
    // `get_node_value(ctx, node)` match branch and in this loop's
    // `ctx.get_field(node, NODE_FIELD_NEXT)` tail — reached from real
    // ConcurrentHashMap.computeIfPresent (whose segment storage walks this
    // same plain-HashMap bucket code) via
    // native_chm_compute_if_present -> native_map_compute_if_present ->
    // native_map_get -> native_hashmap_get_exact. See
    // wildfly-parallel-boot-stale-objectref-residual.md.
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key_val);
    let key_for_hash = read_pinned_elem(ctx, key_pin, key_val);
    // A throwing `hashCode()` / `equals()` releases this window before it
    // propagates (round 15 wave 3, lane compat3: the `?`s left it pinned).
    let (hash, is_null_key) = match key_for_hash {
        Value::Object(Some(k)) => (
            if identity_mode {
                map_hash_key_identity(ctx, k)
            } else if ht_mode {
                let h = map_key_raw_hash(ctx, k);
                unpin_on_err(ctx, this_pin, h)?
            } else {
                let h = map_hash_key(ctx, k);
                unpin_on_err(ctx, this_pin, h)?
            },
            false,
        ),
        Value::Object(None) => (0, true),
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(None)));
        }
    };
    let this = ctx.read_native_pin(this_pin, this);
    let key_val = read_pinned_elem(ctx, key_pin, key_val);
    let key_ref = match key_val {
        Value::Object(Some(k)) => Some(k),
        Value::Object(None) if is_null_key => None,
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(None)));
        }
    };

    let (buckets, chain_size, cap) = map_state(ctx, this);
    let buckets = match buckets {
        Some(b) => b,
        None => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(None)));
        }
    };

    let idx = map_bucket_index(hash, cap);
    let mut node_val = ctx.get_array_element(buckets, idx);

    // S111r27: Use layout-aware helpers so that JDK-created nodes
    // (hash=0, key=1, value=2, next=3) are handled correctly alongside
    // legacy-created nodes (key=0, value=1, hash=2, next=3).
    //
    // Chain-walk cycle guard: mirrors native_map_put's CHAIN_WALK_LIMIT.
    // Without this, a corrupted (cyclic) bucket chain spins this native
    // call forever with no way to recover or diagnose.
    let chain_walk_cap = chain_walk_limit(chain_size);
    // `CRATONVM_COMPAT_HASHMAP_TREE_BINS`: `getNode` tests the first node,
    // then asks a tree bin `getTreeNode` (see `HM_TREE_NODE_CLASS`).
    let first_is_tree = !identity_mode
        && !ht_mode
        && hm_tree_family(ctx, this)
        && hm_tree_head(&*ctx, node_val).is_some();
    let mut walk_count: usize = 0;
    while let Value::Object(Some(mut node)) = node_val {
        walk_count += 1;
        if walk_count > chain_walk_cap {
            eprintln!(
                "[HM-GET-GUARD] aborting chain walk at {} nodes (suspected cycle); map={:?} idx={} cap={}",
                walk_count, this, idx, cap
            );
            ctx.unpin_native_roots(this_pin);
            return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                message: "hashmap chain exceeded safety cap; possible corruption".to_string(),
            }
            .into());
        }
        if first_is_tree && walk_count == 2 && is_hm_tree_node(&*ctx, node) {
            // The first node did not match: `((TreeNode) first).getTreeNode(
            // hash, key)` (any node of the tree climbs to its root).
            let key_now = read_pinned_elem(ctx, key_pin, key_val);
            let found = hm_tree_get_node(ctx, node, hash, key_now);
            let value = match found {
                Ok(Some(n)) => get_node_value(ctx, n),
                Ok(None) => Value::Object(None),
                Err(e) => {
                    ctx.unpin_native_roots(this_pin);
                    return Err(e);
                }
            };
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(value));
        }
        let node_hash = get_node_hash(ctx, node);
        let node_key_field = get_node_key(ctx, node);
        if is_null_key {
            if matches!(node_key_field, Value::Object(None)) {
                let value = get_node_value(ctx, node);
                ctx.unpin_native_roots(this_pin);
                return Ok(Some(value));
            }
        } else if node_hash == hash {
            let Value::Object(Some(node_key)) = node_key_field else {
                node_val = ctx.get_field(node, NODE_FIELD_NEXT);
                continue;
            };
            // `map_keys_equal` invokes the key's real `equals(Object)` (Java
            // bytecode), which can trigger a moving GC -- refresh both
            // `node` and the search key from their pins before any further
            // use, and again immediately after the call before dereferencing
            // `node` again.
            let node_pin = ctx.pin_native_root(node);
            let node_key_pin = ctx.pin_native_root(node_key);
            let node_key = ctx.read_native_pin(node_key_pin, node_key);
            let key_cur = match read_pinned_elem(ctx, key_pin, key_val) {
                Value::Object(Some(k)) => k,
                _ => key_ref.unwrap(),
            };
            // `IdentityHashMap` receiver: reference identity, not `equals()`.
            let eq = if identity_mode {
                map_keys_equal_identity(key_cur, node_key)
            } else if ht_mode {
                let eq = hashtable_keys_equal(ctx, key_cur, node_key);
                unpin_on_err(ctx, this_pin, eq)?
            } else {
                let eq = map_keys_equal(ctx, key_cur, node_key);
                unpin_on_err(ctx, this_pin, eq)?
            };
            node = ctx.read_native_pin(node_pin, node);
            ctx.unpin_native_roots(node_key_pin);
            ctx.unpin_native_roots(node_pin);
            if eq {
                let value = get_node_value(ctx, node);
                ctx.unpin_native_roots(this_pin);
                return Ok(Some(value));
            }
        }
        node_val = ctx.get_field(node, NODE_FIELD_NEXT);
    }

    // Diagnostic: enum-keyed HashMap miss — prime suspect for Keycloak's
    // `Profile.isFeatureEnabled` NPE. Dump every node's enum identity.
    if dbg_kcbool() {
        let key_ref = match read_pinned_elem(ctx, key_pin, key_val) {
            Value::Object(Some(k)) => Some(k),
            _ => key_ref,
        };
        if let Some(k) = key_ref {
            if enum_key_identity(ctx, k).is_some() {
                let mut node_keys: Vec<ObjectRef> = Vec::new();
                for b in 0..(cap.max(0) as usize) {
                    let mut nv = ctx.get_array_element(buckets, b);
                    let mut guard = 0;
                    while let Value::Object(Some(node)) = nv {
                        guard += 1;
                        if guard > 4096 {
                            break;
                        }
                        if let Value::Object(Some(nk)) = get_node_key(ctx, node) {
                            node_keys.push(nk);
                        }
                        nv = ctx.get_field(node, NODE_FIELD_NEXT);
                    }
                }
                dbg_kcbool_report_miss(ctx, "HashMap.get", k, &node_keys);
            }
        }
    }

    // `CRATONVM_DBG_MAP_MISS_AUDIT` — a miss is normal, but a miss on a key the
    // map still holds by reference identity is not. Only the identity case is
    // reported, so this stays quiet on the millions of legitimate misses.
    //
    // Written for the Spring `@Bean` attribute flake: `ClassUtils`'
    // `IdentityHashMap<Class,Class>` returned null for a key it demonstrably
    // still contained, once in ~70 runs, with the map intact before and after.
    // That shape — a single bad lookup against a healthy map — is invisible from
    // Java, because by the time anything notices, the evidence is gone.
    if dbg_map_miss_audit() {
        let key_now = match read_pinned_elem(ctx, key_pin, key_val) {
            Value::Object(Some(k)) => Some(k),
            _ => key_ref,
        };
        if let Some(k) = key_now {
            report_identity_present_miss(ctx, this, buckets, cap, k, hash, idx);
        }
    }

    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(None)))
}

/// `true` iff `CRATONVM_DBG_HMINIT_PURGE` is set. Cached: `HashMap.<init>` is
/// one of the hottest natives in the VM, so this must not re-read the
/// environment. Reports every recycled-address overlay eviction `native_map_init`
/// considers, and — the number that matters — whether the address's previous
/// tenant was actually dead. A `previous_tenant_dead=false` line is a live
/// `HashMap<Integer,?>` that the pre-fix code would have silently emptied.
pub(crate) fn dbg_hminit_purge() -> bool {
    pub(crate) static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_HMINIT_PURGE").is_some()
    })
}

/// `true` iff `CRATONVM_DBG_MAP_MISS_AUDIT` is set. Cached: this is consulted on
/// every map miss, which is a hot path.
pub(crate) fn dbg_map_miss_audit() -> bool {
    pub(crate) static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_MAP_MISS_AUDIT").is_some()
    })
}

/// Re-walk every bucket after a miss. If a node's key is the SAME OBJECT as the
/// one we searched for, the lookup was wrong and we print everything needed to
/// say why: which bucket the key actually lives in versus the one we probed, and
/// the stored node hash versus the hash we just computed.
///
/// Capped at `AUDIT_MAX_CAP` buckets so enabling this on a large map cannot turn
/// a miss into an O(n) walk.
pub(crate) fn report_identity_present_miss(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    buckets: ObjectRef,
    cap: i32,
    key: ObjectRef,
    searched_hash: i32,
    searched_idx: usize,
) {
    const AUDIT_MAX_CAP: i32 = 4096;
    if cap <= 0 || cap > AUDIT_MAX_CAP {
        return;
    }
    for b in 0..(cap as usize) {
        let mut node_val = ctx.get_array_element(buckets, b);
        let mut guard = 0usize;
        while let Value::Object(Some(node)) = node_val {
            guard += 1;
            if guard > 4096 {
                break;
            }
            if let Value::Object(Some(node_key)) = get_node_key(ctx, node) {
                if node_key == key {
                    let node_hash = get_node_hash(ctx, node);
                    let key_class = ctx.class_name_of_id(ctx.class_id_of_object(key));
                    eprintln!(
                        "[MAP-MISS-AUDIT] map={:?} MISSED a key it still holds by identity: \
                         key={:?} key_class={:?} searched_hash={} searched_bucket={} \
                         found_in_bucket={} node_hash={} cap={} \
                         (hash_mismatch={}, bucket_mismatch={})",
                        this,
                        key,
                        key_class,
                        searched_hash,
                        searched_idx,
                        b,
                        node_hash,
                        cap,
                        node_hash != searched_hash,
                        b != searched_idx,
                    );
                    return;
                }
            }
            node_val = ctx.get_field(node, NODE_FIELD_NEXT);
        }
    }
}

pub(crate) fn native_map_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key_val = args.get(1).copied().unwrap_or(Value::Object(None));
    // RULE K — `Hashtable.remove(Object)` is `int hash = key.hashCode();` on
    // its second line, exactly like `get`.
    ht_reject_null_key(ctx, this, args.get(1))?;

    // Keep both arguments rooted from native entry. `HashSet.remove()` is used
    // by ThreadPoolExecutor.processWorkerExit while another thread can drive a
    // collection cycle. The old version installed these roots only after the
    // receiver-routing and integer-overlay checks, leaving their raw entry
    // values vulnerable to a GC at that native boundary. Every delegated path
    // now receives freshly read values, and the ordinary map path retains the
    // same pins through hashCode()/equals().
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key_val);
    let result = (|| -> MethodCallResult {
        let mut this = ctx.read_native_pin(this_pin, this);
        let key_val = read_pinned_elem(ctx, key_pin, key_val);
        if is_unmod_wrapper(ctx, this) {
            return Err(unsupported_op());
        }
        if is_chm_receiver(ctx, this) {
            let call = [Value::Object(Some(this)), key_val];
            if let Some(r) = chm_jdk_delegate(
                ctx,
                &call,
                "remove",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                1,
            ) {
                return r;
            }
            return native_chm_remove(ctx, &call);
        }
        if is_tree_map_receiver(ctx, this) {
            return native_tm_remove(ctx, &[Value::Object(Some(this)), key_val]);
        }
        if is_lhm_receiver(ctx, this) {
            return native_lhm_remove(ctx, &[Value::Object(Some(this)), key_val]);
        }

        if let Value::Object(Some(key_ref)) = key_val {
            if let Some(Value::Int(int_key)) = unbox_wrapper(ctx, key_ref) {
                let object_key = hm_int_fast_obj_key(ctx, this);
                let key_class = ctx.class_id_of_object(key_ref);
                let mut table = hm_int_fast_shard_for(object_key)
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Some(state) = table.get_mut(&object_key) {
                    // Wrapper-class mismatch: `remove(Character.valueOf('A'))`
                    // must NOT delete the `Integer.valueOf(65)` entry (see
                    // `HmIntFastState::key_class`). Drop out of the overlay and
                    // let the node path decide.
                    if hm_int_fast_key_class_ok(state, key_class) {
                        let old = state.entries.remove(&int_key).map(|(_, value)| value);
                        let structural = old.is_some();
                        drop(table);
                        // The overlay twin of the bump `native_map_remove_pinned`
                        // does on the node path -- see [`hm_int_fast_bump`]. A
                        // `remove` that found nothing changes no key and, like
                        // HotSpot's, moves nothing.
                        if structural {
                            hm_int_fast_bump(ctx, this);
                        }
                        return Ok(Some(old.unwrap_or(Value::Object(None))));
                    }
                }
            }
        }

        this = materialize_hm_int_fast(ctx, this)?;
        let this = ctx.read_native_pin(this_pin, this);
        let key_val = read_pinned_elem(ctx, key_pin, key_val);
        native_map_remove_pinned(ctx, this, key_val, this_pin, key_pin)
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_map_remove_pinned(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key_val: Value,
    remove_pin_base: usize,
    key_pin: usize,
) -> MethodCallResult {
    native_map_remove_pinned_hashed(ctx, this, key_val, remove_pin_base, key_pin, None)
}

/// [`native_map_remove_pinned`] with the key's [`map_hash_key`] optionally
/// precomputed; same contract as [`native_map_put_evict_pinned_hashed`].
pub(crate) fn native_map_remove_pinned_hashed(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key_val: Value,
    remove_pin_base: usize,
    key_pin: usize,
    known_hash: Option<i32>,
) -> MethodCallResult {
    // `IdentityHashMap` receiver: reference identity, not virtual
    // `hashCode()`/`equals()` — see `CF_IDENTITY_MAP`. `this` is fresh here
    // (just re-read from its pin by the caller).
    let identity_mode = is_identity_map_receiver(ctx, this);
    // `Hashtable` under `CRATONVM_COMPAT_MAP_JDK_ORDER`: raw hash, and
    // `e.key.equals(key)` with no `==` short cut (see `hashtable_order`).
    let ht_mode = !identity_mode && hashtable_order(ctx, this);
    let (hash, is_null_key) = match key_val {
        Value::Object(Some(k)) => (
            if identity_mode {
                map_hash_key_identity(ctx, k)
            } else if let (Some(h), false) = (known_hash, ht_mode) {
                h
            } else if ht_mode {
                map_key_raw_hash(ctx, k)?
            } else {
                map_hash_key(ctx, k)?
            },
            false,
        ),
        Value::Object(None) => (0, true),
        _ => return Ok(Some(Value::Object(None))),
    };
    let this = ctx.read_native_pin(remove_pin_base, this);
    let key_val = read_pinned_elem(ctx, key_pin, key_val);
    match (key_val, is_null_key) {
        (Value::Object(Some(_)), false) | (Value::Object(None), true) => {}
        _ => return Ok(Some(Value::Object(None))),
    }

    let (buckets, size, cap) = map_state(ctx, this);
    let buckets = match buckets {
        Some(b) => b,
        None => return Ok(Some(Value::Object(None))),
    };
    // `map_keys_equal` below (via `node_matches_inner`) runs the key's
    // `equals()` -- arbitrary Java bytecode that can allocate and trigger a
    // GC. Root the array now, before any `equals()` call, mirroring
    // `native_map_put`'s `buckets_pin` (added for the WildFly
    // parallel-extension-add canary): pinning only after the chain walk
    // would merely preserve an already-forwarded from-space address.
    let buckets_pin = ctx.pin_native_root(buckets);

    let idx = map_bucket_index(hash, cap);
    let head_val = ctx.get_array_element(buckets, idx);

    // `CRATONVM_COMPAT_HASHMAP_TREE_BINS`: a tree bin is removed from by the
    // JDK's own `removeNode(hash, key, null, false, true)` (first-node test,
    // `getTreeNode`, `removeTreeNode`, `++modCount`, `--size`).
    if !identity_mode
        && !ht_mode
        && hm_tree_family(ctx, this)
        && hm_tree_head(&*ctx, head_val).is_some()
    {
        ctx.unpin_native_roots(buckets_pin);
        let key_now = read_pinned_elem(ctx, key_pin, key_val);
        let removed =
            hm_remove_node_bytecode(ctx, this, hash, key_now, Value::Object(None), false, true)?;
        return Ok(Some(match removed {
            Some(n) => get_node_value(ctx, n),
            None => Value::Object(None),
        }));
    }

    // Helper closure: check if node matches our key (S111r27: layout-aware).
    // Returns Result so a thrown `equals` from the user-supplied key class
    // propagates instead of being silently treated as "not equal".
    pub(crate) fn node_matches_inner(
        ctx: &mut dyn NativeContext,
        node_pin: usize,
        node_fallback: ObjectRef,
        is_null_key: bool,
        search_hash: i32,
        key_pin: usize,
        key_fallback: Value,
        identity_mode: bool,
        ht_mode: bool,
    ) -> Result<bool, MethodCallFailed> {
        // The node and requested key can both be relocated while this native
        // call is running on a Tomcat worker. Reload the node *before* reading
        // its key field; pinning only the field value below was too late when
        // the node's raw local had already crossed a collection.
        let node = ctx.read_native_pin(node_pin, node_fallback);
        let node_key_field = get_node_key(ctx, node);
        if is_null_key {
            return Ok(matches!(node_key_field, Value::Object(None)));
        }
        if get_node_hash(ctx, node) != search_hash {
            return Ok(false);
        }
        let Value::Object(Some(nk)) = node_key_field else {
            return Ok(false);
        };
        let Value::Object(Some(k)) = read_pinned_elem(ctx, key_pin, key_fallback) else {
            return Ok(false);
        };
        // `IdentityHashMap` receiver: reference identity, not `equals()` — and
        // no Java dispatch means no GC risk, so skip the pin dance below.
        if identity_mode {
            return Ok(map_keys_equal_identity(k, nk));
        }
        // The freshly loaded node key has no caller-owned pin. Retain it
        // together with the already-rooted requested key for every
        // fast-wrapper probe and the eventual Java equals dispatch.
        let nk_pin = ctx.pin_native_root(nk);
        let nk = ctx.read_native_pin(nk_pin, nk);
        let k = ctx.read_native_pin(key_pin, k);
        let result = if ht_mode {
            hashtable_keys_equal(ctx, k, nk)
        } else {
            map_keys_equal(ctx, k, nk)
        };
        ctx.unpin_native_roots(nk_pin);
        result
    }

    // Check if the head node is the target
    if let Value::Object(Some(head)) = head_val {
        // `node_matches_inner` can invoke the key's `equals()` and therefore
        // GC; pin `head` across it and refresh both `head` and `buckets`
        // before dereferencing either again. Without this, a GC mid-`equals`
        // leaves the plain Rust-local `head`/`buckets` dangling -- observed
        // as `get_field`/`set_field` OOB drops on a genuine, unrelated,
        // freshly-allocated `java/lang/Object` now sitting at the stale
        // address (dohead-post-fix-sporadic-residuals-FIXED.md's header-count residual).
        let head_pin = ctx.pin_native_root(head);
        let head_matches = node_matches_inner(
            ctx,
            head_pin,
            head,
            is_null_key,
            hash,
            key_pin,
            key_val,
            identity_mode,
            ht_mode,
        )?;
        let head = ctx.read_native_pin(head_pin, head);
        let buckets = ctx.read_native_pin(buckets_pin, buckets);
        // GC SAFETY (2026-07-20, DoHead sporadic transport-flake
        // investigation): the refresh above covers `head`/`buckets` but not
        // `this` (the map object itself) -- `this` was only ever derived
        // from `remove_pin_base` once, at function entry, before this
        // GC-capable `equals()` call. A GC landing during it left `this`
        // dangling, so the `set_map_size` below silently corrupted whatever
        // object now sat at that stale address (guarded OOB drop) instead of
        // updating the map's real `size` field. Re-derive `this` from its
        // pin on every refresh, same as the other locals.
        let this = ctx.read_native_pin(remove_pin_base, this);
        ctx.unpin_native_roots(buckets_pin); // pops head_pin too (pushed after it)
        if head_matches {
            let next = ctx.get_field(head, NODE_FIELD_NEXT);
            ctx.set_array_element(buckets, idx, next);
            set_map_size(ctx, this, size - 1);
            bump_map_mod_count(ctx, this);
            let old_value = get_node_value(ctx, head);
            return Ok(Some(old_value));
        }
        // `buckets` is not referenced again below.

        // Walk chain.
        //
        // Chain-walk cycle guard: mirrors native_map_put's CHAIN_WALK_LIMIT.
        // Without this, a corrupted (cyclic) bucket chain spins this native
        // call forever with no way to recover or diagnose -- this is
        // exactly what a --stack-dump-on-timeout capture showed: a
        // ThreadPoolExecutor worker permanently stuck inside
        // HashSet.remove() during processWorkerExit, racing
        // interruptIdleWorkers() on the main thread over the same
        // `workers` set.
        let chain_walk_cap = chain_walk_limit(size);
        let mut walk_count: usize = 0;
        let mut prev = head;
        let mut curr_val = ctx.get_field(head, NODE_FIELD_NEXT);

        while let Value::Object(Some(curr)) = curr_val {
            walk_count += 1;
            if walk_count > chain_walk_cap {
                eprintln!(
                    "[HM-REMOVE-GUARD] aborting chain walk at {} nodes (suspected cycle); map={:?} idx={} cap={}",
                    walk_count, this, idx, cap
                );
                return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                    message: "hashmap chain exceeded safety cap; possible corruption".to_string(),
                }
                .into());
            }
            // Same GC hazard as the head check above: pin `prev`/`curr`
            // across `node_matches_inner` and refresh both before
            // dereferencing either again.
            let prev_pin = ctx.pin_native_root(prev);
            let curr_pin = ctx.pin_native_root(curr);
            let curr_matches = node_matches_inner(
                ctx,
                curr_pin,
                curr,
                is_null_key,
                hash,
                key_pin,
                key_val,
                identity_mode,
                ht_mode,
            )?;
            prev = ctx.read_native_pin(prev_pin, prev);
            let curr = ctx.read_native_pin(curr_pin, curr);
            // GC SAFETY: same `this`-goes-stale hazard as the head check
            // above -- re-derive it from `remove_pin_base` every iteration.
            let this = ctx.read_native_pin(remove_pin_base, this);
            ctx.unpin_native_roots(prev_pin); // pops curr_pin too
            if curr_matches {
                let next = ctx.get_field(curr, NODE_FIELD_NEXT);
                ctx.set_field(prev, NODE_FIELD_NEXT, next);
                set_map_size(ctx, this, size - 1);
                bump_map_mod_count(ctx, this);
                let old_value = get_node_value(ctx, curr);
                return Ok(Some(old_value));
            }
            prev = curr;
            curr_val = ctx.get_field(curr, NODE_FIELD_NEXT);
        }
    } else {
        ctx.unpin_native_roots(buckets_pin);
    }

    Ok(Some(Value::Object(None)))
}

// ---------------------------------------------------------------------------
// `Map`'s three conditional mutators: remove(k,v) / replace(k,v) / replace(k,o,n)
// ---------------------------------------------------------------------------
//
// `native_map_init`'s header states the premise this whole layer rests on:
// "every observable Map method is registered as a native, so JDK bytecode for
// `HashMap.<method>` does not run". These three were the exception. They are
// `java.util.Map` *default* methods that `HashMap` and `Hashtable` override
// with implementations that walk the bucket array directly, so every call ran
// REAL JDK bytecode over a table whose nodes this native layer allocates —
// while `ConcurrentHashMap` had all three registered (see the
// "ConcurrentHashMap-specific methods" block in `register_chm_natives`; they
// are not CHM-specific at all).
//
// On a `LinkedHashMap` that bytecode path is
// `HashMap.remove(k,v)` -> `HashMap.removeNode` -> `LinkedHashMap.afterNodeRemoval`,
// whose first statement is `(LinkedHashMap.Entry<K,V>) e`. Our nodes were
// `java/util/LinkedHashMap$Node` (see `lhm_alloc_node`, which now mints the
// real `LinkedHashMap$Entry`), so it threw
// `ClassCastException: java.util.LinkedHashMap$Node cannot be cast to
// java.util.LinkedHashMap$Entry`. Kafka's
// `MetadataLoader.removeAndClosePublisher` calls exactly
// `publishers.remove(name, publisher)` on a `LinkedHashMap` field, which failed
// embedded-broker shutdown ("Failed to shut down embedded Kafka cluster") in
// Spring Boot's `KafkaAutoConfigurationIntegrationTests`.
//
// The cast is only the loudest symptom. Even where no cast is involved (a plain
// `HashMap`), the JDK bytecode mutates the real bucket array and `size` field
// behind the native bookkeeping — `map_state`'s size, the LHM overlay's
// `size`/`head`/`tail`, the integer fast-path overlay — desyncing the map.
//
// Each is implemented exactly as `java.util.Map`'s default: in terms of
// `get` / `containsKey` / `put` / `remove`, which are registered and which do
// the receiver routing (LinkedHashMap / TreeMap / ConcurrentHashMap /
// Hashtable) themselves.

// ---------------------------------------------------------------------------
// The `java.util.Hashtable` null axis
// ---------------------------------------------------------------------------
//
// MEASURED on the oracle 2026-08-16 (HotSpot 25.0.3+9-LTS,
// `scratchpad/g1/{Probe,Probe2}.java`; the full table is in
// `docs/known-issues/jdk-only/G1-1-...`). `Hashtable`'s rule is TWO rules, and
// they are not the two `Properties` has:
//
//   RULE V (value/function). Every `Hashtable` method that takes a value or a
//     function opens with `Objects.requireNonNull(...)` — or, in `contains` and
//     `containsValue`, a literal `throw new NullPointerException()`. The NPE
//     carries **no message**, and it is checked FIRST, before the key is even
//     looked at: `put(null, null)`, `remove(null, null)`,
//     `replace(null, null)`, `replace(null, null, null)` all measure
//     message-less.
//
//   RULE K (key). Once the value check passes, a null key dies on the bucket
//     walk's own `key.hashCode()`, so HotSpot's helpful-NPE names the local:
//     `Cannot invoke "Object.hashCode()" because "key" is null`. That is a
//     TRANSCRIBED string, not a derived one.
//
// The one row that generalising would have got wrong: `Hashtable.merge` does
// **not** `requireNonNull` its value — only its remapping function — so
// `ht.merge("k", null, f)` computes and returns normally where
// `HashMap.merge("k", null, f)` throws. (`Hashtable.merge` is not registered
// natively here, so no code below needs the exception; it is recorded so the
// next lane does not "fix" it.)
//
// `Properties` is a DIFFERENT contract and must not be folded in: its write
// path delegates to a side `ConcurrentHashMap`, so every refusal is
// message-less (no `hashCode` text) and `props.remove(k, null)` does not throw
// at all — it answers `false`. Hence every predicate below distinguishes
// `is_plain_hashtable_receiver` (Hashtable, Properties EXCLUDED) from
// `is_hashtable_receiver` (Properties INCLUDED).

/// HotSpot's helpful-NPE text when a `java.util.Hashtable` bucket walk
/// dereferences a null key. MEASURED, transcribed character for character:
///
/// ```text
/// java.util.Hashtable | get(null) | java.lang.NullPointerException
///     msg=<<Cannot invoke "Object.hashCode()" because "key" is null>>
/// ```
///
/// The local name is `key` in every `Hashtable` method that produces it, which
/// is why one constant serves the whole family.
pub(crate) const HASHTABLE_NULL_KEY_MSG: &str = "Cannot invoke \"Object.hashCode()\" because \"key\" is null";

/// `java.util.Hashtable` or an ordinary subclass, with `java.util.Properties`
/// EXCLUDED — the receiver test the null axis needs.
///
/// It is the same bit `uses_native_hashtable_layout` reads
/// (`CF_HASHTABLE_LAYOUT`, memoized per `ClassId`), asked for a different
/// reason: there, "does this receiver keep its entries in the native bucket
/// array"; here, "does this receiver have `Hashtable`'s null contract rather
/// than `Properties`' side-`ConcurrentHashMap` one". Both questions have the
/// same answer for the same reason — `Properties` is the subclass that stops
/// behaving like `Hashtable` — so they share the flag rather than paying for a
/// second class walk.
#[inline]
pub(crate) fn is_plain_hashtable_receiver(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    uses_native_hashtable_layout(ctx, this)
}

/// RULE K. Refuse an explicitly-passed null key on a plain `Hashtable`
/// receiver, with the transcribed helpful-NPE text.
///
/// `key` is `args.get(n)`, not an `unwrap_or`-flattened `Value`, on purpose:
/// a MISSING argument is a malformed native call and must not be reported as a
/// program-level NPE (the same distinction `native_map_merge` draws for its
/// remapping function).
pub(crate) fn ht_reject_null_key(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    key: Option<&Value>,
) -> Result<(), MethodCallFailed> {
    if matches!(key, Some(Value::Object(None))) && is_plain_hashtable_receiver(ctx, this) {
        return Err(RuntimeError::NullPointerException {
            message: Some(HASHTABLE_NULL_KEY_MSG.to_string()),
        }
        .into());
    }
    Ok(())
}

/// RULE V. Refuse an explicitly-passed null value on a plain `Hashtable`
/// receiver. `Objects.requireNonNull` and `throw new NullPointerException()`
/// both produce a message-less NPE, and inventing text here is exactly the
/// defect F41-1 §3 caught twice.
pub(crate) fn ht_reject_null_value(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    value: Option<&Value>,
) -> Result<(), MethodCallFailed> {
    if matches!(value, Some(Value::Object(None))) && is_plain_hashtable_receiver(ctx, this) {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    }
    Ok(())
}

/// Message-less NPE, the shape `Objects.requireNonNull` and
/// `ConcurrentHashMap`'s explicit `throw new NullPointerException()` share.
#[inline]
pub(crate) fn bare_npe() -> MethodCallFailed {
    RuntimeError::NullPointerException { message: None }.into()
}

/// RULE F. Refuse an explicitly-passed null **functional argument** — the
/// `Consumer`, `BiConsumer`, `Function`, `BiFunction`, `UnaryOperator` or
/// `Predicate` that `forEach` / `replaceAll` / `removeIf` / `compute*` /
/// `merge` take.
///
/// MEASURED on HotSpot 25.0.3+9-LTS, 2026-08-16, and the answer is the one
/// uniform column this whole sweep produced: **a bare, message-less
/// `NullPointerException` on every mutable receiver** — `HashMap`,
/// `LinkedHashMap`, `TreeMap`, `Hashtable`, `Properties`,
/// `ConcurrentHashMap`, `ConcurrentSkipListMap`, `IdentityHashMap`,
/// `WeakHashMap`, `ArrayList`, `LinkedList`, `Vector`, `Stack`, `HashSet`,
/// `LinkedHashSet`, `TreeSet`, `ArrayDeque`, `PriorityQueue`,
/// `CopyOnWriteArrayList`, `ConcurrentLinkedQueue`, and every `values()` /
/// `keySet()` / `entrySet()` view of the six map families. Every one of those
/// methods opens `Objects.requireNonNull(<the function>)`.
///
/// Three properties of that rule are load-bearing and none is guessable:
///
/// * **It fires on an EMPTY receiver too.** `new HashMap<>().forEach(null)`
///   throws; the refusal is not a side effect of reaching the first element.
///   Every body below therefore checks BEFORE it looks at the contents.
/// * **It fires BEFORE the null-key check.** MEASURED across all six map
///   receivers: `ht.computeIfAbsent(null, null)` is message-less, while
///   `ht.computeIfAbsent(null, F)` carries [`HASHTABLE_NULL_KEY_MSG`]. That is
///   G1-1's RULE V / RULE K ordering, extended to the function argument, and
///   it is why callers put this call ahead of [`ht_reject_null_key`].
/// * **It does NOT extend to an immutable receiver, and it does NOT extend to
///   `sort`.** MEASURED: `List.of("a").removeIf(null)`,
///   `.replaceAll(null)`, `.sort(null)` and
///   `Collections.unmodifiableMap(m).replaceAll(null)` all throw
///   `UnsupportedOperationException`, not NPE — immutability is checked first
///   — while `List.of("a").forEach(null)` (which mutates nothing) throws the
///   NPE. And `list.sort(null)` is not a null-argument error at all: a null
///   `Comparator` MEANS natural ordering and sorts normally
///   (`ArrayList`, `LinkedList`, `Vector`, `Stack`, `CopyOnWriteArrayList`,
///   `Arrays.asList`, `subList`, `Collections.sort(l, null)` and
///   `Arrays.sort(a, null)` were all measured returning `[a, b]`).
///   So this helper is called from the mutable-receiver bodies only, and never
///   from a `*_sort_comparator` body — `native_al_sort_comparator` and
///   `native_collections_sort_comparator` already route a null comparator to
///   natural ordering and are correct as they stand.
///
/// `arg` is `args.get(n)`, NOT an `unwrap_or`-flattened `Value`, for the same
/// reason [`ht_reject_null_key`] takes it that way: a **missing** argument is a
/// malformed native call and must keep its existing no-op, while an
/// **explicitly passed** null is a program-level error the JDK reports. The
/// bodies below all previously collapsed both cases into "return the empty
/// answer", which is the fabricated-success shape — the caller is told the
/// operation succeeded and nothing happened.
#[inline]
pub(crate) fn reject_null_functional(arg: Option<&Value>) -> Result<(), MethodCallFailed> {
    if matches!(arg, Some(Value::Object(None))) {
        return Err(bare_npe());
    }
    Ok(())
}

/// RULE C — the null COLLECTION argument, the sibling of [`reject_null_functional`].
///
/// `addAll`, `removeAll`, `retainAll`, `containsAll` and `toArray(T[])` all open
/// `Objects.requireNonNull(c)` in `ArrayList`, `LinkedList`, `ArrayDeque`,
/// `Vector` and `AbstractCollection`, so the refusal fires on an EMPTY receiver
/// and for an empty argument alike. MEASURED on HotSpot 25.0.4+7
/// (`apps/probes/ArrayListShadowSweep` 24/147/148, `apps/probes/DequeListShadowSweep`
/// 9/115/162/163).
///
/// Deliberately the SAME predicate rather than a second spelling of it: the
/// distinction that matters is missing-vs-explicitly-null, and this file has
/// paid for keeping one rule in five places before. The separate name exists so
/// a reader of a `removeAll` body is not told a `Collection` is a "functional"
/// argument.
///
/// CORRECTION (round 15 wave 4, lane compat5): the premise holds for
/// `removeAll` / `retainAll` only. `addAll`, `containsAll`, `toArray(T[])` and
/// the copy constructors DEREFERENCE their argument in JDK 25, so HotSpot's NPE
/// carries the helpful message -- see [`reject_null_collection_deref`] and
/// `r15w4-compat5-null-argument-message-residuals-FIXED-20261001.md` for the sites
/// still on this helper.
#[inline]
pub(crate) fn reject_null_collection(arg: Option<&Value>) -> Result<(), MethodCallFailed> {
    reject_null_functional(arg)
}

/// `CRATONVM_COMPAT_NULL_COLLECTION_MESSAGE` -- default ON, `=0` restores
/// RULE C's bare NPE for these sites. Proposal C4W4-6 of
/// `jit-r15w4-compat4-proposals-RETIRED-20261003.md` (round 15 wave 4, lane compat5), page
/// `r15w4-compat4-collection-review-findings-FIXED-20260930.md` item 1. RULE C's
/// premise holds for `removeAll` / `retainAll` (`Objects.requireNonNull(c)`,
/// message-less) and NOT for `addAll` or the copy constructors, whose JDK 25
/// bodies DEREFERENCE `c`, so HotSpot's helpful message names the first call.
/// MEASURED on HotSpot 25 (`R15Compat4NullArgMessages` rows 0-5, 8-9):
///
/// * `ArrayList` / `Vector` `.addAll(c)` and `(c)` constructors, `LinkedList`
///   `.addAll([index, ]c)` and `LinkedList(c)`: `c.toArray()`
///   ([`NULL_C_TO_ARRAY`]);
/// * `ArrayDeque.addAll(c)` / `ArrayDeque(c)`, `HashSet(c)` /
///   `LinkedHashSet(c)`: `c.size()` ([`NULL_C_SIZE`]).
pub(crate) fn null_collection_message_enabled() -> bool {
    pub(crate) static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_NULL_COLLECTION_MESSAGE")
}

/// HotSpot's helpful NPE for `Object[] a = c.toArray();` on a null `c`.
pub(crate) const NULL_C_TO_ARRAY: &str =
    "Cannot invoke \"java.util.Collection.toArray()\" because \"c\" is null";
/// HotSpot's helpful NPE for `c.size()` on a null `c`.
pub(crate) const NULL_C_SIZE: &str = "Cannot invoke \"java.util.Collection.size()\" because \"c\" is null";
/// HotSpot's helpful NPE for `for (Object e : c)` on a null `c`
/// (`AbstractCollection.containsAll`, `ConcurrentHashMap$CollectionView
/// .containsAll`): MEASURED for `containsAll(null)` on every receiver of the
/// G8-1 lane record's section 2.2 (HotSpot 25.0.3,
/// `G8-1-the-collections-view-families-and-the-null-function-axis-20260816-RETIRED-20260920.md`).
pub(crate) const NULL_C_ITERATOR: &str =
    "Cannot invoke \"java.util.Collection.iterator()\" because \"c\" is null";
/// HotSpot's helpful NPE for `a.length` on a null `T[] a`
/// (`ArrayList` / `LinkedList` / `ArrayDeque` / `AbstractCollection`
/// `.toArray(T[])`, whose first use of `a` is its length). MEASURED on
/// HotSpot 25, `R15Compat5NullArgMore` rows 4-5 (round 15 wave 5, lane compat6).
pub(crate) const NULL_A_LENGTH: &str = "Cannot read the array length because \"a\" is null";
/// HotSpot's helpful NPE for `TreeMap(SortedMap m)`'s `m.comparator()`
/// (MEASURED, `R15Compat5NullArgMore` row 9).
pub(crate) const NULL_M_COMPARATOR: &str =
    "Cannot invoke \"java.util.SortedMap.comparator()\" because \"m\" is null";

/// `containsAll(null)` under [`null_collection_message_enabled`]: the
/// helpful NPE of the JDK's `for (Object e : c)`. The natives answered
/// `false` -- a fabricated result where HotSpot throws. With the switch off,
/// `Ok(())` keeps that answer.
pub(crate) fn reject_null_contains_all_arg(arg: Option<&Value>) -> Result<(), MethodCallFailed> {
    if null_collection_message_enabled() && matches!(arg, Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException {
            message: Some(NULL_C_ITERATOR.to_string()),
        }
        .into());
    }
    Ok(())
}

/// `toArray(T[] a)` with an explicitly null `a`, under
/// [`null_collection_message_enabled`]: HotSpot's [`NULL_A_LENGTH`]. Every JDK
/// 25 body these natives stand for reads `a.length` first (`ArrayList`,
/// `AbstractCollection`, `HashMap.prepareArray` behind the `HashSet` /
/// `KeySet` / `Values` overrides, `ConcurrentHashMap$CollectionView`). The
/// `ArrayList`-shaped, map-view and `HashSet`-family natives answered a fresh
/// array instead -- a fabricated success; with the switch off, `Ok(())` keeps
/// that answer. Round 15 wave 5, lane compat6.
pub(crate) fn reject_null_to_array_template(arg: Option<&Value>) -> Result<(), MethodCallFailed> {
    if null_collection_message_enabled() && matches!(arg, Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException {
            message: Some(NULL_A_LENGTH.to_string()),
        }
        .into());
    }
    Ok(())
}

/// [`reject_null_collection`] for a JDK body that dereferences its null
/// collection `c` rather than `requireNonNull`-ing it: the helpful `message`
/// of that dereference under [`null_collection_message_enabled`], else the
/// bare NPE. A MISSING argument stays the malformed-call no-op.
pub(crate) fn reject_null_collection_deref(
    arg: Option<&Value>,
    message: &'static str,
) -> Result<(), MethodCallFailed> {
    if null_collection_message_enabled() && matches!(arg, Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException {
            message: Some(message.to_string()),
        }
        .into());
    }
    reject_null_collection(arg)
}

/// `CRATONVM_COMPAT_NULL_ARG_MESSAGE_FROM_BYTECODE` -- default ON since j27
/// (CD16-1, 2026-10-28; `=0` restores the literal-only answer). On, the
/// null-argument natives below ask the JDK method they stand for
/// ([`NullArgSite`]) for its JEP 358 line (`NativeContext::null_arg_message`,
/// read from the method's own bytecode and `LocalVariableTable`) before
/// their transcribed literal, which stays the answer whenever the query
/// declines. JIT round 16 (lane compatdrain), proposal C5W4-1 of
/// `jit-r15w4-compat5-proposals-RETIRED-20261003.md`: the `vm` unit test
/// `the_query_reproduces_hotspots_null_argument_lines_from_jdk_bytecode`
/// shows the query reproducing every literal wired here from JDK 25, so the
/// switch changes nothing on that JDK; it is what keeps the lines right on
/// the next one. `CRATONVM_DBG_NULL_ARG_MESSAGE=1` prints each site whose
/// asked line differs from its literal. The literals stay: they answer
/// whenever the query declines (class not loaded, no `Code`, a branch before
/// the dereference), e.g. whenever no JDK body of that method is loaded.
pub(crate) fn null_arg_message_from_bytecode_enabled() -> bool {
    pub(crate) static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    compat_switch_on(&ON, "CRATONVM_COMPAT_NULL_ARG_MESSAGE_FROM_BYTECODE")
}

pub(crate) fn null_arg_message_dbg() -> bool {
    pub(crate) static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_NULL_ARG_MESSAGE"))
}

/// The JDK method a null-argument native stands for, and which parameter:
/// `(internal class name, method, descriptor, 0-based parameter)`.
pub(crate) type NullArgSite = (&'static str, &'static str, &'static str, usize);

pub(crate) const NULL_ARG_AL_ADD_ALL: NullArgSite = (
    "java/util/ArrayList",
    "addAll",
    "(Ljava/util/Collection;)Z",
    0,
);
pub(crate) const NULL_ARG_AL_INIT: NullArgSite = (
    "java/util/ArrayList",
    "<init>",
    "(Ljava/util/Collection;)V",
    0,
);
pub(crate) const NULL_ARG_HS_INIT: NullArgSite = (
    "java/util/HashSet",
    "<init>",
    "(Ljava/util/Collection;)V",
    0,
);
pub(crate) const NULL_ARG_LL_INIT: NullArgSite = (
    "java/util/LinkedList",
    "<init>",
    "(Ljava/util/Collection;)V",
    0,
);
pub(crate) const NULL_ARG_LL_ADD_ALL: NullArgSite = (
    "java/util/LinkedList",
    "addAll",
    "(Ljava/util/Collection;)Z",
    0,
);
pub(crate) const NULL_ARG_LL_ADD_ALL_AT: NullArgSite = (
    "java/util/LinkedList",
    "addAll",
    "(ILjava/util/Collection;)Z",
    1,
);
pub(crate) const NULL_ARG_LL_TO_ARRAY: NullArgSite = (
    "java/util/LinkedList",
    "toArray",
    "([Ljava/lang/Object;)[Ljava/lang/Object;",
    0,
);
pub(crate) const NULL_ARG_AD_INIT: NullArgSite = (
    "java/util/ArrayDeque",
    "<init>",
    "(Ljava/util/Collection;)V",
    0,
);
pub(crate) const NULL_ARG_AD_ADD_ALL: NullArgSite = (
    "java/util/ArrayDeque",
    "addAll",
    "(Ljava/util/Collection;)Z",
    0,
);
pub(crate) const NULL_ARG_TM_INIT_SORTED: NullArgSite =
    ("java/util/TreeMap", "<init>", "(Ljava/util/SortedMap;)V", 0);

/// [`reject_null_collection_deref`] for a native that names the JDK method
/// it stands for: under [`null_arg_message_from_bytecode_enabled`] the
/// message is the one that method's bytecode yields, else (or when the query
/// declines) the transcribed `message`.
pub(crate) fn reject_null_collection_asked(
    ctx: &dyn NativeContext,
    arg: Option<&Value>,
    site: NullArgSite,
    message: &'static str,
) -> Result<(), MethodCallFailed> {
    if null_arg_message_from_bytecode_enabled()
        && null_collection_message_enabled()
        && matches!(arg, Some(Value::Object(None)))
    {
        let (class, method, descriptor, param) = site;
        if let Some(asked) = ctx.null_arg_message(class, method, descriptor, param) {
            if null_arg_message_dbg() && asked != message {
                eprintln!(
                    "[NULLARG] {class}.{method}{descriptor} param {param}: bytecode says {asked:?}, literal {message:?}"
                );
            }
            return Err(RuntimeError::NullPointerException {
                message: Some(asked),
            }
            .into());
        }
    }
    reject_null_collection_deref(arg, message)
}

/// `remove(Object, Object)`'s receiver-routed null contract.
///
/// MEASURED, and the three receivers disagree in three different ways:
///
/// | receiver | `remove(null, v)` | `remove(k, null)` | `remove(null, null)` |
/// |---|---|---|---|
/// | `HashMap` | `false` | `false` | `false` |
/// | `Hashtable` | NPE `Cannot invoke "Object.hashCode()"...` | NPE (no msg) | NPE (no msg) |
/// | `Properties` | NPE (no msg) | **`false`** | NPE (no msg) |
///
/// `Hashtable.remove(k,v)` is `Objects.requireNonNull(value)` then the bucket
/// walk, so the VALUE is checked first. `Properties.remove(k,v)` delegates to
/// its side `ConcurrentHashMap`, whose `remove(k,v)` is
/// `if (key == null) throw new NullPointerException(); return value != null && ...`
/// — the key is checked first and a null value is simply "no match".
pub(crate) fn map_remove_kv_null_contract(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    key: Option<&Value>,
    value: Option<&Value>,
) -> Result<(), MethodCallFailed> {
    if is_plain_hashtable_receiver(ctx, this) {
        ht_reject_null_value(ctx, this, value)?;
        ht_reject_null_key(ctx, this, key)?;
    } else if is_hashtable_receiver(ctx, this) {
        // `Properties`: key only, message-less. Deliberately no value check —
        // measured `false`, and the previous shared helper threw here.
        if matches!(key, Some(Value::Object(None))) {
            return Err(bare_npe());
        }
    }
    Ok(())
}

/// `replace(K, V)`'s receiver-routed null contract. `Hashtable.replace` is
/// `Objects.requireNonNull(value)` then the bucket walk;
/// `Properties.replace` delegates to `ConcurrentHashMap.replace`, which
/// refuses both, message-less. MEASURED: `ht.replace(null, null)` is
/// message-less, so the value must be checked BEFORE the key.
pub(crate) fn map_replace_kv_null_contract(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    key: Option<&Value>,
    value: Option<&Value>,
) -> Result<(), MethodCallFailed> {
    if is_plain_hashtable_receiver(ctx, this) {
        ht_reject_null_value(ctx, this, value)?;
        ht_reject_null_key(ctx, this, key)?;
    } else if is_hashtable_receiver(ctx, this) {
        if matches!(value, Some(Value::Object(None))) || matches!(key, Some(Value::Object(None))) {
            return Err(bare_npe());
        }
    }
    Ok(())
}

/// `replace(K, V, V)`'s receiver-routed null contract. `Hashtable.replace`
/// requires BOTH values non-null (old first, then new) before the bucket walk
/// reaches the key; `Properties` refuses all three message-less. MEASURED.
pub(crate) fn map_replace3_null_contract(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    key: Option<&Value>,
    old_value: Option<&Value>,
    new_value: Option<&Value>,
) -> Result<(), MethodCallFailed> {
    if is_plain_hashtable_receiver(ctx, this) {
        ht_reject_null_value(ctx, this, old_value)?;
        ht_reject_null_value(ctx, this, new_value)?;
        ht_reject_null_key(ctx, this, key)?;
    } else if is_hashtable_receiver(ctx, this) {
        if matches!(old_value, Some(Value::Object(None)))
            || matches!(new_value, Some(Value::Object(None)))
            || matches!(key, Some(Value::Object(None)))
        {
            return Err(bare_npe());
        }
    }
    Ok(())
}

/// `Map.remove(Object key, Object value)` — remove only if currently mapped to
/// `value`. Mirrors the JDK default:
/// ```text
/// Object cur = get(key);
/// if (!Objects.equals(cur, value) || (cur == null && !containsKey(key))) return false;
/// remove(key);
/// return true;
/// ```
pub(crate) fn native_map_remove_kv(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let expected = args.get(2).copied().unwrap_or(Value::Object(None));
    map_remove_kv_null_contract(ctx, this, args.get(1), args.get(2))?;
    // One hash, one walk (`CRATONVM_COMPAT_MAP_SINGLE_WALK`).
    if map_single_walk_eligible(ctx, this) {
        return map_single_walk_mutate(
            ctx,
            this,
            key,
            expected,
            Value::Object(None),
            SingleWalkOp::RemoveIfValue,
        );
    }
    if lhm_single_walk_eligible(ctx, this) {
        return lhm_single_walk_mutate(
            ctx,
            this,
            key,
            expected,
            Value::Object(None),
            SingleWalkOp::RemoveIfValue,
        );
    }
    // GC-safety: `get`/`containsKey`/`remove` each dispatch the key's
    // `hashCode()`/`equals()` — arbitrary Java that can complete a moving young
    // GC — so every receiver/key/value local is re-read from its pin after each
    // call, the same discipline `native_chm_remove_kv` follows.
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let expected_pin = pin_value(ctx, expected);
    let result = (|| -> MethodCallResult {
        let this = ctx.read_native_pin(this_pin, this);
        let key = read_pinned_elem(ctx, key_pin, key);
        let current =
            native_map_get(ctx, &[Value::Object(Some(this)), key])?.unwrap_or(Value::Object(None));
        let current_pin = pin_value(ctx, current);
        let current = read_pinned_elem(ctx, current_pin, current);
        let expected = read_pinned_elem(ctx, expected_pin, expected);
        // `HashMap.removeNode(.., matchValue)` asks `value.equals(v)` -- the
        // ARGUMENT is the receiver -- while `Hashtable.remove(k, v)` asks
        // `e.value.equals(value)`. `CRATONVM_COMPAT_MAP_JDK_ORDER`.
        let this_now = ctx.read_native_pin(this_pin, this);
        // `TreeMap` inherits `Map.remove(k, v)`, whose test is
        // `Objects.equals(curValue, value)`: the STORED value is the receiver
        // (`CRATONVM_COMPAT_TREEMAP_VALUE_EQUALS_ORDER`).
        let argument_first = map_jdk_order_enabled()
            && !uses_native_hashtable_layout(ctx, this_now)
            && !(treemap_value_equals_order_enabled() && is_tree_map_receiver(ctx, this_now));
        let matched = if argument_first {
            values_equal_deep(ctx, &expected, &current)?
        } else {
            values_equal_deep(ctx, &current, &expected)?
        };
        if !matched {
            return Ok(Some(Value::Int(0)));
        }
        // `get` cannot distinguish "absent" from "mapped to null"; only the
        // null case needs the extra probe.
        if matches!(current, Value::Object(None)) {
            let this = ctx.read_native_pin(this_pin, this);
            let key = read_pinned_elem(ctx, key_pin, key);
            let present = matches!(
                native_map_contains_key(ctx, &[Value::Object(Some(this)), key])?,
                Some(Value::Int(1))
            );
            if !present {
                return Ok(Some(Value::Int(0)));
            }
        }
        let this = ctx.read_native_pin(this_pin, this);
        let key = read_pinned_elem(ctx, key_pin, key);
        native_map_remove(ctx, &[Value::Object(Some(this)), key])?;
        Ok(Some(Value::Int(1)))
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

/// `Map.replace(K key, V value)` — replace only if the key is currently mapped,
/// returning the previous value (or `null` when absent). Mirrors the JDK
/// default: `if (get(key) != null || containsKey(key)) cur = put(key, value);`
pub(crate) fn native_map_replace(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let new_val = args.get(2).copied().unwrap_or(Value::Object(None));
    map_replace_kv_null_contract(ctx, this, args.get(1), args.get(2))?;
    // One hash, one walk (`CRATONVM_COMPAT_MAP_SINGLE_WALK`).
    if map_single_walk_eligible(ctx, this) {
        return map_single_walk_mutate(
            ctx,
            this,
            key,
            Value::Object(None),
            new_val,
            SingleWalkOp::Replace,
        );
    }
    if lhm_single_walk_eligible(ctx, this) {
        return lhm_single_walk_mutate(
            ctx,
            this,
            key,
            Value::Object(None),
            new_val,
            SingleWalkOp::Replace,
        );
    }
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let new_pin = pin_value(ctx, new_val);
    let result = (|| -> MethodCallResult {
        let this = ctx.read_native_pin(this_pin, this);
        let key = read_pinned_elem(ctx, key_pin, key);
        let current =
            native_map_get(ctx, &[Value::Object(Some(this)), key])?.unwrap_or(Value::Object(None));
        let current_pin = pin_value(ctx, current);
        let current = read_pinned_elem(ctx, current_pin, current);
        if matches!(current, Value::Object(None)) {
            let this = ctx.read_native_pin(this_pin, this);
            let key = read_pinned_elem(ctx, key_pin, key);
            let present = matches!(
                native_map_contains_key(ctx, &[Value::Object(Some(this)), key])?,
                Some(Value::Int(1))
            );
            if !present {
                return Ok(Some(Value::Object(None)));
            }
        }
        let this = ctx.read_native_pin(this_pin, this);
        let key = read_pinned_elem(ctx, key_pin, key);
        let new_val = read_pinned_elem(ctx, new_pin, new_val);
        let old = native_map_put(ctx, &[Value::Object(Some(this)), key, new_val])?
            .unwrap_or(Value::Object(None));
        Ok(Some(old))
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

/// `Map.replace(K key, V oldValue, V newValue)` — compare-and-set. Mirrors the
/// JDK default:
/// ```text
/// Object cur = get(key);
/// if (!Objects.equals(cur, oldValue) || (cur == null && !containsKey(key))) return false;
/// put(key, newValue);
/// return true;
/// ```
pub(crate) fn native_map_replace_kv(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let old_val = args.get(2).copied().unwrap_or(Value::Object(None));
    let new_val = args.get(3).copied().unwrap_or(Value::Object(None));
    map_replace3_null_contract(ctx, this, args.get(1), args.get(2), args.get(3))?;
    // One hash, one walk (`CRATONVM_COMPAT_MAP_SINGLE_WALK`).
    if map_single_walk_eligible(ctx, this) {
        return map_single_walk_mutate(
            ctx,
            this,
            key,
            old_val,
            new_val,
            SingleWalkOp::ReplaceIfValue,
        );
    }
    if lhm_single_walk_eligible(ctx, this) {
        return lhm_single_walk_mutate(
            ctx,
            this,
            key,
            old_val,
            new_val,
            SingleWalkOp::ReplaceIfValue,
        );
    }
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let old_pin = pin_value(ctx, old_val);
    let new_pin = pin_value(ctx, new_val);
    let result = (|| -> MethodCallResult {
        let this = ctx.read_native_pin(this_pin, this);
        let key = read_pinned_elem(ctx, key_pin, key);
        let current =
            native_map_get(ctx, &[Value::Object(Some(this)), key])?.unwrap_or(Value::Object(None));
        let current_pin = pin_value(ctx, current);
        let current = read_pinned_elem(ctx, current_pin, current);
        let old_val = read_pinned_elem(ctx, old_pin, old_val);
        // `TreeMap.replace(k, oldValue, newValue)` tests
        // `Objects.equals(oldValue, p.value)`: the ARGUMENT is the receiver
        // (`CRATONVM_COMPAT_TREEMAP_VALUE_EQUALS_ORDER`). `HashMap`'s body asks
        // the stored value, as this composite always did.
        let this_now = ctx.read_native_pin(this_pin, this);
        let matched = if treemap_value_equals_order_enabled() && is_tree_map_receiver(ctx, this_now)
        {
            values_equal_deep(ctx, &old_val, &current)?
        } else {
            values_equal_deep(ctx, &current, &old_val)?
        };
        if !matched {
            return Ok(Some(Value::Int(0)));
        }
        if matches!(current, Value::Object(None)) {
            let this = ctx.read_native_pin(this_pin, this);
            let key = read_pinned_elem(ctx, key_pin, key);
            let present = matches!(
                native_map_contains_key(ctx, &[Value::Object(Some(this)), key])?,
                Some(Value::Int(1))
            );
            if !present {
                return Ok(Some(Value::Int(0)));
            }
        }
        let this = ctx.read_native_pin(this_pin, this);
        let key = read_pinned_elem(ctx, key_pin, key);
        let new_val = read_pinned_elem(ctx, new_pin, new_val);
        native_map_put(ctx, &[Value::Object(Some(this)), key, new_val])?;
        Ok(Some(Value::Int(1)))
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

/// `CRATONVM_COMPAT_TREEMAP_VALUE_EQUALS_ORDER` — default ON, `=0` restores
/// the `HashMap` order for a `TreeMap` receiver. The `remove(k, v)` /
/// `replace(k, old, new)` composites asked `value.equals(stored)` and
/// `stored.equals(old)` -- `HashMap`'s receivers -- for every map they serve.
/// A `TreeMap` inherits `Map.remove(k, v)`, which asks
/// `Objects.equals(curValue, value)` (the stored value first), and its own
/// `replace(k, oldValue, newValue)` asks `Objects.equals(oldValue, p.value)`
/// (the argument first): both the other way round, which a value class with
/// an asymmetric `equals` (or one that logs) observes. Round 13 wave 10, lane
/// compat5 (residual page item 8, the `TreeMap` half).
pub(crate) fn treemap_value_equals_order_enabled() -> bool {
    pub(crate) static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_TREEMAP_VALUE_EQUALS_ORDER")
}

/// `CRATONVM_COMPAT_MAP_SINGLE_WALK` — default ON, `=0` restores the
/// composites. See [`map_single_walk_mutate`].
pub(crate) fn map_single_walk_enabled() -> bool {
    pub(crate) static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_MAP_SINGLE_WALK")
}

/// Whether `remove(k,v)` / `replace(k,v)` / `replace(k,old,new)` on `this`
/// take [`map_single_walk_mutate`]: a `HashMap` or `Hashtable` (or a user
/// subclass) on the plain bucket layout, with no integer-overlay row.
/// `LinkedHashMap` (its overlay list and access order), the CHM, `TreeMap`
/// and `IdentityHashMap` routes, `Properties` (its side table), the
/// unmodifiable wrappers, nameless fabricated maps and an overlay-backed map
/// keep the composite, whose answer is already right for them.
pub(crate) fn map_single_walk_eligible(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    if !map_single_walk_enabled() {
        return false;
    }
    let facts = receiver_facts(ctx, this);
    if !facts.has(CF_BUCKET_MAP_NAME)
        || facts.has(CF_LHM)
        || facts.has(CF_CHM)
        || facts.has(CF_TREE_MAP)
        || facts.has(CF_IDENTITY_MAP)
        || facts.has(CF_UNMOD_WRAPPER)
    {
        return false;
    }
    // `Properties`: Hashtable ancestry without the Hashtable bucket layout.
    if facts.has(CF_HASHTABLE_ANCESTRY) && !facts.has(CF_HASHTABLE_LAYOUT) {
        return false;
    }
    hm_int_fast_len(ctx, this).is_none()
}

/// Which conditional mutator [`map_single_walk_mutate`] performs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SingleWalkOp {
    /// `remove(key, value)`.
    RemoveIfValue,
    /// `replace(key, value)`.
    Replace,
    /// `replace(key, oldValue, newValue)`.
    ReplaceIfValue,
}

/// One walk of the chain starting at `start` for the key pinned at `key_pin`
/// (bucket hash `hash`), in the receiver family's order: `hash`, then `==`,
/// then `key.equals(k)` ([`map_keys_equal`]), or `e.key.equals(key)` under
/// `ht_mode` ([`hashtable_keys_equal`]); a null key matches a null-keyed
/// node. Answers the first matching node, re-read after the last Java call
/// -- the caller must root it before running Java again.
///
/// `tree` says how a tree bin (a `TreeNode` head) is searched
/// (`CRATONVM_COMPAT_HASHMAP_TREE_BINS`); the caller passes
/// [`TreeLookup::Off`] for a receiver outside [`hm_tree_family`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn map_chain_find_from(
    ctx: &mut dyn NativeContext,
    start: Value,
    hash: i32,
    key_pin: usize,
    key_fallback: Value,
    ht_mode: bool,
    walk_cap: usize,
    tree: TreeLookup,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let is_null_key = matches!(key_fallback, Value::Object(None));
    let first_tree = if ht_mode || tree == TreeLookup::Off {
        None
    } else {
        hm_tree_head(&*ctx, start)
    };
    if let (Some(first), TreeLookup::Direct) = (first_tree, tree) {
        // `if (first instanceof TreeNode) old = (t = (TreeNode) first)
        // .getTreeNode(hash, key);` -- no first-node test.
        let key_now = read_pinned_elem(ctx, key_pin, key_fallback);
        return hm_tree_get_node(ctx, first, hash, key_now);
    }
    let mut node_val = start;
    let mut walk_count: usize = 0;
    while let Value::Object(Some(mut node)) = node_val {
        walk_count += 1;
        if walk_count > walk_cap {
            return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                message: "hashmap chain exceeded safety cap; possible corruption".to_string(),
            }
            .into());
        }
        if first_tree.is_some() && walk_count == 2 && is_hm_tree_node(&*ctx, node) {
            // `getNode` after a first node that did not match.
            let key_now = read_pinned_elem(ctx, key_pin, key_fallback);
            return hm_tree_get_node(ctx, node, hash, key_now);
        }
        let node_key_field = get_node_key(ctx, node);
        if is_null_key {
            if matches!(node_key_field, Value::Object(None)) {
                return Ok(Some(node));
            }
        } else if get_node_hash(ctx, node) == hash {
            if let Value::Object(Some(node_key)) = node_key_field {
                // `equals` is Java and can collect: root the node and its key.
                let node_pin = ctx.pin_native_root(node);
                let node_key_pin = ctx.pin_native_root(node_key);
                let node_key = ctx.read_native_pin(node_key_pin, node_key);
                let equal = match read_pinned_elem(ctx, key_pin, key_fallback) {
                    Value::Object(Some(k)) if ht_mode => hashtable_keys_equal(ctx, k, node_key),
                    Value::Object(Some(k)) => map_keys_equal(ctx, k, node_key),
                    _ => Ok(false),
                };
                node = ctx.read_native_pin(node_pin, node);
                ctx.unpin_native_roots(node_pin); // pops `node_key_pin` too
                if equal? {
                    return Ok(Some(node));
                }
            }
        }
        node_val = ctx.get_field(node, NODE_FIELD_NEXT);
    }
    Ok(None)
}

/// Unlink `node` from `this`'s table by IDENTITY -- no hash call and no
/// `equals`, so no Java and no collection -- and account for it exactly as
/// `native_map_remove_pinned` does (`size - 1`, one `modCount` bump).
/// `false` if the node is no longer in its bucket.
///
/// A `TreeNode` (`CRATONVM_COMPAT_HASHMAP_TREE_BINS`) is unlinked by the JDK's
/// own `removeTreeNode(this, tab, movable)` ([`hm_tree_unlink_node`]), which
/// runs bytecode and CAN collect; `movable` is `removeNode`'s (`true` for
/// `remove`, `false` for an iterator's `remove()`).
pub(crate) fn map_unlink_node(ctx: &mut dyn NativeContext, this: ObjectRef, node: ObjectRef) -> bool {
    map_unlink_node_movable(ctx, this, node, true)
}

/// [`map_unlink_node`] with `removeNode`'s `movable`.
pub(crate) fn map_unlink_node_movable(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    node: ObjectRef,
    movable: bool,
) -> bool {
    if is_hm_tree_node(&*ctx, node) && hm_tree_family(&*ctx, this) {
        return hm_tree_unlink_node(ctx, this, node, movable);
    }
    let (buckets, size, cap) = map_state(ctx, this);
    let Some(buckets) = buckets else {
        return false;
    };
    let idx = map_bucket_index(get_node_hash(ctx, node), cap);
    let next = ctx.get_field(node, NODE_FIELD_NEXT);
    let walk_cap = chain_walk_limit(size);
    let mut prev: Option<ObjectRef> = None;
    let mut cur = ctx.get_array_element(buckets, idx);
    let mut steps: usize = 0;
    while let Value::Object(Some(c)) = cur {
        steps += 1;
        if steps > walk_cap {
            return false;
        }
        if std::ptr::eq(c.as_ptr(), node.as_ptr()) {
            match prev {
                None => ctx.set_array_element(buckets, idx, next),
                Some(p) => ctx.set_field(p, NODE_FIELD_NEXT, next),
            }
            set_map_size(ctx, this, size - 1);
            bump_map_mod_count(ctx, this);
            return true;
        }
        prev = Some(c);
        cur = ctx.get_field(c, NODE_FIELD_NEXT);
    }
    false
}

/// Write a node's value slot in either node layout (see `get_node_value`).
pub(crate) fn set_node_value(ctx: &mut dyn NativeContext, node: ObjectRef, value: Value) {
    match ctx.get_field(node, 0) {
        Value::Object(_) => ctx.set_field(node, 1, value), // legacy value slot 1
        _ => ctx.set_field(node, NODE_FIELD_VALUE, value), // JDK value slot 2
    }
}

/// `Hashtable`'s value predicate: `e.value.equals(value)` -- the STORED value
/// is the receiver and there is no `==` short cut (both are non-null: the
/// null contracts ran first).
pub(crate) fn hashtable_value_equals(
    ctx: &mut dyn NativeContext,
    argument: Value,
    stored: Value,
) -> Result<bool, MethodCallFailed> {
    match (argument, stored) {
        (Value::Object(Some(a)), Value::Object(Some(s))) => hashtable_keys_equal(ctx, a, s),
        (a, s) => Ok(values_equal(ctx, &s, &a)),
    }
}

/// `remove(k, v)`, `replace(k, v)` and `replace(k, old, new)` with ONE
/// `hashCode()` and ONE chain walk, as the JDK's own bodies do
/// (`HashMap.removeNode(.., matchValue, ..)` / `getNode`, and `Hashtable`'s
/// loops). The composites these replace (`get` + `containsKey` + `put`/
/// `remove`) gave the right answers but called `hashCode()` and walked
/// `equals` two or three times per call. `CRATONVM_COMPAT_MAP_SINGLE_WALK`,
/// round 13 wave 6 (lane hashtree); comparison-order page item 5.
///
/// Value predicates, all as the JDK:
/// * `HashMap.remove(k, v)`: `v == value || value.equals(v)` -- the ARGUMENT
///   is the receiver (under `CRATONVM_COMPAT_MAP_JDK_ORDER`, as the wave-4
///   composite);
/// * `HashMap.replace(k, old, new)`: `v == old || v.equals(old)` -- stored;
/// * `Hashtable` (under `CRATONVM_COMPAT_MAP_JDK_ORDER`, `ht_mode`):
///   `e.value.equals(..)`, and `remove(k, v)` keeps walking the chain after a
///   key match whose value differs (its loop condition is the conjunction)
///   and nulls the removed entry's value.
///
/// A `replace` writes the value in place: no `modCount` bump, exactly like
/// both JDK bodies. The caller has run the family's null contract.
pub(crate) fn map_single_walk_mutate(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key: Value,
    expected: Value,
    replacement: Value,
    op: SingleWalkOp,
) -> MethodCallResult {
    let not_found = match op {
        SingleWalkOp::Replace => Value::Object(None),
        SingleWalkOp::RemoveIfValue | SingleWalkOp::ReplaceIfValue => Value::Int(0),
    };
    let ht_mode = hashtable_order(ctx, this);
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let expected_pin = pin_value(ctx, expected);
    let replacement_pin = pin_value(ctx, replacement);
    let result = (|| -> MethodCallResult {
        let hash = match read_pinned_elem(ctx, key_pin, key) {
            Value::Object(Some(k)) if ht_mode => map_key_raw_hash(ctx, k)?,
            Value::Object(Some(k)) => map_hash_key(ctx, k)?,
            Value::Object(None) => 0,
            _ => return Ok(Some(not_found)),
        };
        let this = ctx.read_native_pin(this_pin, this);
        let (buckets, size, cap) = map_state(ctx, this);
        let Some(buckets) = buckets else {
            return Ok(Some(not_found));
        };
        let walk_cap = chain_walk_limit(size);
        let mut start = ctx.get_array_element(buckets, map_bucket_index(hash, cap));
        // `removeNode` / `getNode`: the first node, then a tree bin's
        // `getTreeNode` (`CRATONVM_COMPAT_HASHMAP_TREE_BINS`).
        let tree = TreeLookup::for_receiver(&*ctx, this, TreeLookup::AfterFirst);
        // Only `Hashtable.remove(k, v)` goes round more than once; the bound
        // keeps a corrupt (cyclic) chain from spinning that loop.
        for _ in 0..walk_cap {
            let key_now = read_pinned_elem(ctx, key_pin, key);
            let Some(node) =
                map_chain_find_from(ctx, start, hash, key_pin, key_now, ht_mode, walk_cap, tree)?
            else {
                return Ok(Some(not_found));
            };
            let node_pin = ctx.pin_native_root(node);
            let current = get_node_value(ctx, node);
            let current_pin = pin_value(ctx, current);
            let outcome = (|| -> MethodCallResult {
                let expected = read_pinned_elem(ctx, expected_pin, expected);
                let matched = match op {
                    SingleWalkOp::Replace => true,
                    SingleWalkOp::ReplaceIfValue if ht_mode => {
                        hashtable_value_equals(ctx, expected, current)?
                    }
                    SingleWalkOp::ReplaceIfValue => values_equal_deep(ctx, &current, &expected)?,
                    SingleWalkOp::RemoveIfValue if ht_mode => {
                        hashtable_value_equals(ctx, expected, current)?
                    }
                    SingleWalkOp::RemoveIfValue if map_jdk_order_enabled() => {
                        values_equal_deep(ctx, &expected, &current)?
                    }
                    SingleWalkOp::RemoveIfValue => values_equal_deep(ctx, &current, &expected)?,
                };
                let node = ctx.read_native_pin(node_pin, node);
                let current = read_pinned_elem(ctx, current_pin, current);
                if !matched {
                    // `None` = keep walking (`Hashtable.remove(k, v)` only).
                    return Ok(None);
                }
                match op {
                    SingleWalkOp::Replace => {
                        let replacement = read_pinned_elem(ctx, replacement_pin, replacement);
                        set_node_value(ctx, node, replacement);
                        Ok(Some(current))
                    }
                    SingleWalkOp::ReplaceIfValue => {
                        let replacement = read_pinned_elem(ctx, replacement_pin, replacement);
                        set_node_value(ctx, node, replacement);
                        Ok(Some(Value::Int(1)))
                    }
                    SingleWalkOp::RemoveIfValue => {
                        let this = ctx.read_native_pin(this_pin, this);
                        if !map_unlink_node(ctx, this, node) {
                            // `equals` mutated the map under us and the node
                            // is gone: nothing was removed by this call.
                            return Ok(Some(Value::Int(0)));
                        }
                        if ht_mode {
                            // `Hashtable.remove(k, v)`: `e.value = null`.
                            set_node_value(ctx, node, Value::Object(None));
                        }
                        Ok(Some(Value::Int(1)))
                    }
                }
            })();
            match outcome {
                Ok(None) if ht_mode && op == SingleWalkOp::RemoveIfValue => {
                    let node = ctx.read_native_pin(node_pin, node);
                    start = ctx.get_field(node, NODE_FIELD_NEXT);
                    ctx.unpin_native_roots(node_pin);
                }
                Ok(None) => {
                    ctx.unpin_native_roots(node_pin);
                    return Ok(Some(not_found));
                }
                other => {
                    ctx.unpin_native_roots(node_pin);
                    return other;
                }
            }
        }
        Ok(Some(not_found))
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

/// Whether `remove(k,v)` / `replace(k,v)` / `replace(k,old,new)` on `this`
/// take [`lhm_single_walk_mutate`]: a `LinkedHashMap` (or subclass), under
/// the same `CRATONVM_COMPAT_MAP_SINGLE_WALK` switch as the `HashMap` family.
/// Round 13 wave 8, lane hashcompat3 (wave 6 left this family on the
/// composite; `R13HashtreeCondMutators`, `LinkedHashMap` line).
pub(crate) fn lhm_single_walk_eligible(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    map_single_walk_enabled() && is_lhm_receiver(ctx, this)
}

/// [`map_single_walk_mutate`] for a `LinkedHashMap`: ONE `hashCode()` and ONE
/// chain walk ([`lhm_find_node_hashed`]), as the inherited `HashMap` bodies
/// (`removeNode(.., matchValue, ..)` / `getNode`) do, where the composite ran
/// `get` (+ `containsKey`) + `put`/`remove`.
///
/// It also stops the composite's `get` from REORDERING an access-ordered map
/// when nothing changes: the JDK calls `afterNodeAccess` only after a
/// successful `replace`, and never from `remove(k, v)`, while the composite's
/// `get` moved the entry to the tail on every call, a miss included.
///
/// Value predicates as the JDK: `remove(k, v)` asks `value.equals(v)` (the
/// argument is the receiver, under `CRATONVM_COMPAT_MAP_JDK_ORDER`, as the
/// composite did), `replace(k, old, new)` asks `v.equals(old)`. A removal is
/// accounted exactly as [`native_lhm_remove`] accounts one.
pub(crate) fn lhm_single_walk_mutate(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key: Value,
    expected: Value,
    replacement: Value,
    op: SingleWalkOp,
) -> MethodCallResult {
    let not_found = match op {
        SingleWalkOp::Replace => Value::Object(None),
        SingleWalkOp::RemoveIfValue | SingleWalkOp::ReplaceIfValue => Value::Int(0),
    };
    // A `reversed()` view rebuilds from its source first, as `native_lhm_get`
    // does; the three arguments are carried across that (it can collect).
    let mut carried = [key, expected, replacement];
    let this = resync_reversed_map_carrying(ctx, this, &mut carried)?;
    let [key, expected, replacement] = carried;
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let expected_pin = pin_value(ctx, expected);
    let replacement_pin = pin_value(ctx, replacement);
    // Every pin above `this_pin` is released with it, on every exit.
    let result = (|| -> MethodCallResult {
        let hash = match read_pinned_elem(ctx, key_pin, key) {
            Value::Object(Some(k)) => Some(map_hash_key(ctx, k)?),
            Value::Object(None) => None,
            _ => return Ok(Some(not_found)),
        };
        let this = ctx.read_native_pin(this_pin, this);
        let key_now = read_pinned_elem(ctx, key_pin, key);
        let Some(node) = lhm_find_node_hashed(ctx, this, &key_now, hash)? else {
            return Ok(Some(not_found));
        };
        let node_pin = ctx.pin_native_root(node);
        let current = ctx.get_field(node, LHM_NODE_VALUE);
        let current_pin = pin_value(ctx, current);
        let expected = read_pinned_elem(ctx, expected_pin, expected);
        let matched = match op {
            SingleWalkOp::Replace => true,
            SingleWalkOp::ReplaceIfValue => values_equal_deep(ctx, &current, &expected)?,
            SingleWalkOp::RemoveIfValue if map_jdk_order_enabled() => {
                values_equal_deep(ctx, &expected, &current)?
            }
            SingleWalkOp::RemoveIfValue => values_equal_deep(ctx, &current, &expected)?,
        };
        if !matched {
            return Ok(Some(not_found));
        }
        let node = ctx.read_native_pin(node_pin, node);
        let current = read_pinned_elem(ctx, current_pin, current);
        let this = ctx.read_native_pin(this_pin, this);
        match op {
            SingleWalkOp::Replace | SingleWalkOp::ReplaceIfValue => {
                let replacement = read_pinned_elem(ctx, replacement_pin, replacement);
                ctx.set_field(node, LHM_NODE_VALUE, replacement);
                // `afterNodeAccess(e)`.
                if lhm_is_access_order(ctx, this) {
                    lhm_move_to_tail(ctx, this, node);
                }
                Ok(Some(if op == SingleWalkOp::Replace {
                    current
                } else {
                    Value::Int(1)
                }))
            }
            // `false` when `equals` mutated the map and the node is gone:
            // nothing was removed by this call.
            SingleWalkOp::RemoveIfValue => Ok(Some(Value::Int(i32::from(lhm_unlink_node(
                ctx, this, node,
            ))))),
        }
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

/// Unlink `node` from a `LinkedHashMap` by IDENTITY -- its bucket chain, the
/// insertion-order list and `size` -- exactly as [`native_lhm_remove`]
/// accounts a removal, with no hash call and no `equals`, so no Java and no
/// collection. `false` if the node is no longer in its bucket.
///
/// A `TreeNode` (`CRATONVM_COMPAT_LHM_TREE_BINS`) goes through the JDK's
/// `removeTreeNode` ([`lhm_tree_unlink_node`], `movable`), which runs bytecode
/// and CAN collect.
pub(crate) fn lhm_unlink_node(ctx: &mut dyn NativeContext, this: ObjectRef, node: ObjectRef) -> bool {
    lhm_unlink_node_movable(ctx, this, node, true)
}

/// [`lhm_unlink_node`] with `removeNode`'s `movable`.
pub(crate) fn lhm_unlink_node_movable(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    node: ObjectRef,
    movable: bool,
) -> bool {
    if is_hm_tree_node(&*ctx, node) && lhm_tree_family(&*ctx, this) {
        return lhm_tree_unlink_node(ctx, this, node, movable);
    }
    let (buckets, size, cap) = lhm_state(ctx, this);
    let Some(buckets) = buckets else {
        return false;
    };
    let hash = match ctx.get_field(node, LHM_NODE_HASH) {
        Value::Int(h) => h,
        _ => 0,
    };
    let idx = map_bucket_index(hash, cap);
    let next = ctx.get_field(node, LHM_NODE_NEXT);
    let walk_cap = chain_walk_limit(size);
    let mut prev: Option<ObjectRef> = None;
    let mut cur = ctx.get_array_element(buckets, idx);
    let mut steps: usize = 0;
    while let Value::Object(Some(c)) = cur {
        steps += 1;
        if steps > walk_cap {
            return false;
        }
        if std::ptr::eq(c.as_ptr(), node.as_ptr()) {
            match prev {
                None => ctx.set_array_element(buckets, idx, next),
                Some(p) => ctx.set_field(p, LHM_NODE_NEXT, next),
            }
            lhm_unlink(ctx, this, node);
            lhm_set(ctx, this, "size", LHM_FIELD_SIZE, Value::Int(size - 1));
            return true;
        }
        prev = Some(c);
        cur = ctx.get_field(c, LHM_NODE_NEXT);
    }
    false
}

/// Register `remove(k,v)` / `replace(k,v)` / `replace(k,old,new)` on one
/// map class. Called for every map family whose JDK class overrides them with
/// a bucket-walking body (`HashMap`, `LinkedHashMap`, `Hashtable`);
/// `ConcurrentHashMap` has its own segment-locked versions and `TreeMap` does
/// not override them at all (its inherited `Map` defaults call the registered
/// `get`/`containsKey`/`put`/`remove` natives, which is already correct).
pub(crate) fn register_map_conditional_mutators(r: &mut NativeMethodRegistry, c: &str) {
    r.register(
        c,
        "remove",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
        native_map_remove_kv,
    );
    r.register(
        c,
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_replace,
    );
    r.register(
        c,
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z",
        native_map_replace_kv,
    );
}

pub(crate) fn native_map_contains_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Round 72: must not conflate `containsKey` with `get(...) != null`.
    // HashMap allows null values, so an entry with a null value must still
    // report containsKey == true. Walk the bucket chain directly and check
    // for node presence, mirroring `native_map_get` but returning a boolean
    // on node match regardless of the stored value. (Kafka 4.2
    // `ConfigDef.parse` puts `early.start.listeners` with a null default;
    // `AbstractConfig.get` then calls `values.containsKey(...)` and would
    // wrongly throw "Unknown configuration" if we returned false here.)
    let mut this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    // RULE K — `Hashtable.containsKey` opens with `key.hashCode()` too.
    ht_reject_null_key(ctx, this, args.get(1))?;
    if is_tree_map_receiver(ctx, this) {
        return native_tm_contains_key(ctx, args);
    }
    if is_chm_receiver(ctx, this) {
        if let Some(r) = chm_jdk_delegate(ctx, args, "containsKey", "(Ljava/lang/Object;)Z", 1) {
            return r;
        }
        return native_chm_contains_key(ctx, args);
    }
    if is_lhm_receiver(ctx, this) {
        return native_lhm_contains_key(ctx, args);
    }
    let key_val = args.get(1).copied().unwrap_or(Value::Object(None));

    if let Value::Object(Some(key_ref)) = key_val {
        if let Some(Value::Int(int_key)) = unbox_wrapper(ctx, key_ref) {
            let object_key = hm_int_fast_obj_key(ctx, this);
            let key_class = ctx.class_id_of_object(key_ref);
            let table = hm_int_fast_shard_for(object_key)
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(state) = table.get(&object_key) {
                // Wrapper-class mismatch: `containsKey(Character.valueOf('A'))`
                // is false for a map holding `Integer.valueOf(65)` (see
                // `HmIntFastState::key_class`). Fall through to the node path.
                if hm_int_fast_key_class_ok(state, key_class) {
                    return Ok(Some(
                        Value::Int(state.entries.contains_key(&int_key) as i32),
                    ));
                }
            }
        }
    }

    // GC-safety: same "Family 1" stale-ObjectRef pattern fixed in
    // `native_hashmap_get_exact` (see its comment for the live
    // CRATONVM_DBG_STALE_OBJREF capture this mirrors) -- `map_hash_key`/
    // `map_keys_equal` dispatch arbitrary Java code (hashCode()/equals()),
    // which can trigger a moving GC. Pin `this` and the search key up front
    // and re-read them (plus the bucket-chain `node`) after every such call.
    // `IdentityHashMap` receiver: reference identity, not virtual
    // `hashCode()`/`equals()` — see `CF_IDENTITY_MAP`.
    let identity_mode = is_identity_map_receiver(ctx, this);
    // `Hashtable` under `CRATONVM_COMPAT_MAP_JDK_ORDER`: raw hash, and
    // `e.key.equals(key)` with no `==` short cut (see `hashtable_order`).
    let ht_mode = !identity_mode && hashtable_order(ctx, this);
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key_val);
    // Every `?` in this pinned window releases the window first (round 15
    // wave 3, lane compat3): a throwing `hashCode()` / `equals()` returned
    // with `this` and the key still pinned.
    let materialized = materialize_hm_int_fast(ctx, this);
    this = unpin_on_err(ctx, this_pin, materialized)?;
    let this = ctx.read_native_pin(this_pin, this);
    let key_val = read_pinned_elem(ctx, key_pin, key_val);

    let key_for_hash = key_val;
    let (hash, is_null_key) = match key_for_hash {
        Value::Object(Some(k)) => (
            if identity_mode {
                map_hash_key_identity(ctx, k)
            } else if ht_mode {
                let h = map_key_raw_hash(ctx, k);
                unpin_on_err(ctx, this_pin, h)?
            } else {
                let h = map_hash_key(ctx, k);
                unpin_on_err(ctx, this_pin, h)?
            },
            false,
        ),
        Value::Object(None) => (0, true),
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Int(0)));
        }
    };
    let this = ctx.read_native_pin(this_pin, this);
    let key_val = read_pinned_elem(ctx, key_pin, key_val);
    let key_ref = match key_val {
        Value::Object(Some(k)) => Some(k),
        Value::Object(None) if is_null_key => None,
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Int(0)));
        }
    };

    let (buckets, chain_size, cap) = map_state(ctx, this);
    let buckets = match buckets {
        Some(b) => b,
        None => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Int(0)));
        }
    };

    let idx = map_bucket_index(hash, cap);
    let mut node_val = ctx.get_array_element(buckets, idx);

    // Chain-walk cycle guard: mirrors native_map_put's CHAIN_WALK_LIMIT.
    let chain_walk_cap = chain_walk_limit(chain_size);
    // `containsKey` is `getNode(key) != null`: the tree arm of
    // `native_hashmap_get_exact` (`CRATONVM_COMPAT_HASHMAP_TREE_BINS`).
    let first_is_tree = !identity_mode
        && !ht_mode
        && hm_tree_family(ctx, this)
        && hm_tree_head(&*ctx, node_val).is_some();
    let mut walk_count: usize = 0;
    while let Value::Object(Some(mut node)) = node_val {
        walk_count += 1;
        if walk_count > chain_walk_cap {
            eprintln!(
                "[HM-CONTAINSKEY-GUARD] aborting chain walk at {} nodes (suspected cycle); map={:?} idx={} cap={}",
                walk_count, this, idx, cap
            );
            ctx.unpin_native_roots(this_pin);
            return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                message: "hashmap chain exceeded safety cap; possible corruption".to_string(),
            }
            .into());
        }
        if first_is_tree && walk_count == 2 && is_hm_tree_node(&*ctx, node) {
            let key_now = read_pinned_elem(ctx, key_pin, key_val);
            let found = hm_tree_get_node(ctx, node, hash, key_now);
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Int(i32::from(found?.is_some()))));
        }
        let node_hash = get_node_hash(ctx, node);
        let node_key_field = get_node_key(ctx, node);
        if is_null_key {
            if matches!(node_key_field, Value::Object(None)) {
                ctx.unpin_native_roots(this_pin);
                return Ok(Some(Value::Int(1)));
            }
        } else if node_hash == hash {
            let Value::Object(Some(node_key)) = node_key_field else {
                node_val = ctx.get_field(node, NODE_FIELD_NEXT);
                continue;
            };
            // `map_keys_equal` invokes the key's real `equals(Object)` (Java
            // bytecode), which can trigger a moving GC -- refresh `node` and
            // the search key from their pins before and after the call.
            let node_pin = ctx.pin_native_root(node);
            let node_key_pin = ctx.pin_native_root(node_key);
            let node_key = ctx.read_native_pin(node_key_pin, node_key);
            let key_cur = match read_pinned_elem(ctx, key_pin, key_val) {
                Value::Object(Some(k)) => k,
                _ => key_ref.unwrap(),
            };
            let eq = if identity_mode {
                map_keys_equal_identity(key_cur, node_key)
            } else if ht_mode {
                let eq = hashtable_keys_equal(ctx, key_cur, node_key);
                unpin_on_err(ctx, this_pin, eq)?
            } else {
                let eq = map_keys_equal(ctx, key_cur, node_key);
                unpin_on_err(ctx, this_pin, eq)?
            };
            node = ctx.read_native_pin(node_pin, node);
            ctx.unpin_native_roots(node_key_pin);
            ctx.unpin_native_roots(node_pin);
            if eq {
                ctx.unpin_native_roots(this_pin);
                return Ok(Some(Value::Int(1)));
            }
        }
        node_val = ctx.get_field(node, NODE_FIELD_NEXT);
    }

    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Int(0)))
}

pub(crate) fn native_map_contains_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    // RULE V. `Hashtable.containsValue` is `return contains(value);` and
    // `Hashtable.contains` opens with a literal
    // `if (value == null) throw new NullPointerException();` — so it refuses
    // even on an EMPTY table (measured). `HashMap.containsValue(null)` is legal
    // and answers false.
    ht_reject_null_value(ctx, this, args.get(1))?;
    if is_chm_receiver(ctx, this) {
        if let Some(r) = chm_jdk_delegate(ctx, args, "containsValue", "(Ljava/lang/Object;)Z", 1) {
            return r;
        }
        return native_chm_contains_value(ctx, args);
    }
    let target_pin = pin_value(ctx, target);
    let materialized = materialize_hm_int_fast(ctx, this);
    this = unpin_on_err(ctx, target_pin, materialized)?;
    let target = read_pinned_elem(ctx, target_pin, target);
    // Family-1 fix (cce0079): every `list_element_matches` below dispatches
    // `equals()` (GC-capable) — the previous code walked buckets/chains and
    // snapshot vectors through stale refs after the first dispatch, and its
    // early `return`s also leaked `target_pin`. Pin the walk state, refresh
    // after each dispatch, unpin on every exit.
    let mut found = false;
    // Properties-backed ConcurrentHashMap path: keep the existing
    // segment-aware collection (rare; correctness over speed).
    if properties_backing_chm(ctx, this).is_some() {
        let values = map_collect_values(ctx, this);
        let (v_base, v_handles) = pin_value_slice(ctx, &values);
        // The lowest pin of this window: `target_pin`, or the values' group
        // when the target is null (round 15 wave 3, lane compat3: with a null
        // target the values' pins were never released, and a throwing
        // `equals` left both groups up).
        let window = target_pin.min(v_base);
        let mut target = target;
        for (i, val_orig) in values.iter().enumerate() {
            let val = read_pinned_elem(ctx, v_handles[i], *val_orig);
            let matched = list_element_matches(ctx, &val, &target);
            let matched = unpin_on_err(ctx, window, matched)?;
            target = read_pinned_elem(ctx, target_pin, target);
            if matched {
                found = true;
                break;
            }
        }
        if window != usize::MAX {
            ctx.unpin_native_roots(window);
        }
        return Ok(Some(Value::Int(if found { 1 } else { 0 })));
    }
    // Plain HashMap: walk the buckets directly and short-circuit on the
    // first matching value instead of materializing every value into a Vec.
    let (buckets, chain_size, cap) = map_state(ctx, this);
    if let Some(b) = buckets {
        let b_pin = ctx.pin_native_root(b);
        let mut b = b;
        let mut target = target;
        // Chain-walk cycle guard: bound each chain by the table-wide node
        // count to avoid a hang on a corrupt (cyclic) chain. The flat 4096
        // here did not throw, it BROKE -- a value stored past the 4096th node
        // of one bucket answered `false`. `chain_walk_limit` scales with the
        // map (`CRATONVM_COMPAT_MAP_LONG_CHAINS`).
        let chain_walk_cap = chain_walk_limit(chain_size);
        // `Hashtable.contains(value)` (its `containsValue`) walks the table
        // from the TOP down and asks `e.value.equals(value)` -- the STORED
        // value is the receiver, with no `==` short cut. `HashMap` walks up
        // and asks `value.equals(v)`, which `list_element_matches` is.
        // `CRATONVM_COMPAT_MAP_JDK_ORDER` (see `hashtable_order`).
        let ht_mode = hashtable_order(ctx, this);
        let table_len = cap.max(0) as usize;
        'outer: for step in 0..table_len {
            let i = if ht_mode { table_len - 1 - step } else { step };
            let mut node_val = ctx.get_array_element(b, i);
            let mut walk_count: usize = 0;
            while let Value::Object(Some(node)) = node_val {
                walk_count += 1;
                if walk_count > chain_walk_cap {
                    break;
                }
                let node_pin = ctx.pin_native_root(node);
                let value = get_node_value(ctx, node);
                let matched = if ht_mode {
                    hashtable_value_equals(ctx, target, value)
                } else {
                    list_element_matches(ctx, &value, &target)
                };
                let matched = match matched {
                    Ok(matched) => matched,
                    Err(e) => {
                        // A throwing `equals` used to leave all three pins
                        // behind (`?`); release them on the way out.
                        ctx.unpin_native_roots(b_pin);
                        if target_pin != usize::MAX {
                            ctx.unpin_native_roots(target_pin);
                        }
                        return Err(e);
                    }
                };
                let node = ctx.read_native_pin(node_pin, node);
                b = ctx.read_native_pin(b_pin, b);
                target = read_pinned_elem(ctx, target_pin, target);
                if matched {
                    ctx.unpin_native_roots(node_pin);
                    found = true;
                    break 'outer;
                }
                node_val = ctx.get_field(node, NODE_FIELD_NEXT);
                ctx.unpin_native_roots(node_pin);
            }
        }
        ctx.unpin_native_roots(b_pin);
    }
    if target_pin != usize::MAX {
        ctx.unpin_native_roots(target_pin);
    }
    Ok(Some(Value::Int(if found { 1 } else { 0 })))
}

pub(crate) fn native_map_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    if is_unmod_wrapper(ctx, this) {
        return Err(unsupported_op());
    }
    if is_chm_receiver(ctx, this) {
        if let Some(r) = chm_jdk_delegate(ctx, args, "clear", "()V", 0) {
            return r;
        }
        return native_chm_clear(ctx, args);
    }
    let had_entries = hm_int_fast_len(ctx, this).unwrap_or(0) != 0;
    if hm_int_fast_purge(&*ctx, this) {
        // Dropping the overlay IS this map's `clear()`, so it owes the same
        // generation bump the node path below pays -- see
        // [`hm_int_fast_bump`]. Guarded on non-empty to match the node arm
        // (`if size != 0`), which is the stricter of the two: over-
        // invalidating a view is free, under-invalidating it is the defect.
        if had_entries {
            bump_map_mod_count(ctx, this);
        }
        return Ok(None);
    }
    let (buckets, size, cap) = map_state(ctx, this);
    if let Some(b) = buckets {
        for i in 0..(cap as usize) {
            ctx.set_array_element(b, i, Value::Object(None));
        }
    }
    set_map_size(ctx, this, 0);
    if size != 0 {
        bump_map_mod_count(ctx, this);
    }
    Ok(None)
}

pub(crate) fn native_map_key_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    if is_tree_map_receiver(ctx, this) {
        return native_tm_key_set(ctx, args);
    }
    // A `ConcurrentHashMap` reached polymorphically through `Map.keySet()` must
    // get the same `KeySetView` its own `keySet()` returns — otherwise the
    // static receiver type silently decides the class, the cast and whether
    // `add` throws.
    if is_chm_receiver(ctx, this) {
        if let Some(r) = chm_jdk_delegate(ctx, args, "keySet", "()Ljava/util/Set;", 0) {
            return r;
        }
        return native_chm_key_set(ctx, args);
    }
    // Build the live view through the shared helper so every key, including a
    // legal HashMap null key, is inserted through native_map_put.
    //
    // The wrapper verdict is taken BEFORE the build: `make_view_set_of`
    // allocates, so `this` afterwards may be a pre-move address and asking it
    // for its class then would be reading a corpse.
    // The view this map already has, if any. A keySet view is LIVE — every read
    // through it resyncs from the source — so the instance can be reused
    // indefinitely, which is exactly what HotSpot does and why
    // `map.keySet() == map.keySet()` holds there. Building a fresh one per call
    // cost 1609 us on a 1000-entry `LinkedHashMap` (`probes/KeySetBench
    // viewOnly`), paid on every `getPropertyNames()` in Spring.
    if let Some(cached) = cached_live_view(ctx, this, VIEW_KIND_KEYSET) {
        return Ok(Some(Value::Object(Some(cached))));
    }
    MAP_VIEW_BUILT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let sync = wants_synchronized_views(&*ctx, this);
    let this_pin = ctx.pin_native_root(this);
    let keys = map_collect_keys(ctx, this);
    let set = make_view_set_of(ctx, &mut this, VIEW_KIND_KEYSET, &keys)?;
    let set = if sync {
        wrap_synchronized_view(ctx, set, true)?
    } else {
        set
    };
    // GC-SAFETY: everything above allocates, so `this` is re-read from its pin
    // before the store; `set` is the value just returned and has not been
    // followed by an allocation.
    let this = ctx.read_native_pin(this_pin, this);
    store_live_view(ctx, this, VIEW_KIND_KEYSET, set);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(set))))
}

pub(crate) fn native_map_values(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    if is_tree_map_receiver(ctx, this) {
        return native_tm_values(ctx, args);
    }
    // A JDK-model `ConcurrentHashMap` (nio21 w1 L4): its own `ValuesView`,
    // not a carrier whose methods the JDK's bytecode would then run.
    if let Some(r) = chm_jdk_delegate(ctx, args, "values", "()Ljava/util/Collection;", 0) {
        return r;
    }
    // The CONSTRUCTION half only, and it is the same argument as `keySet()`'s:
    // a values view is live because `resync_values_view` rebuilds its element
    // array on every read, so the instance never goes stale and can simply be
    // handed out again. What it does NOT get is the rebuild elision -- a
    // value-replacing `put` changes what this view must answer while moving no
    // structural counter. See `cached_live_values_view`.
    if let Some(cached) = cached_live_values_view(ctx, this) {
        return Ok(Some(Value::Object(Some(cached))));
    }
    let values = map_collect_values(ctx, this);
    // Build an ArrayList from the values. Reserve one extra trailing slot and
    // stash the source map there so `values().iterator().remove()` /
    // `values().remove(v)` can write through to the backing map (live-view
    // semantics). The slot lives beyond the logical size, so it is invisible
    // to size-based operations yet still GC-scanned as part of the array.
    // cceres3: pin across GC-capable call (stream stale-at-store wave) — the
    // list/buffer allocations move `this` and every snapshotted value before
    // the stores below (mirrors native_map_keys_as_array).
    let carrier = values_carrier_for(&*ctx, this);
    // Taken before anything allocates — see `native_map_key_set`.
    let sync = wants_synchronized_views(&*ctx, this);
    let this_pin = ctx.pin_native_root(this);
    let (_, val_handles) = pin_value_slice(ctx, &values);
    // gc-common w20-a: not `?` -- a refused carrier (`--jdk-only`) used to
    // strand `this_pin` and every value pin above it.
    let list = match alloc_view_carrier(ctx, carrier) {
        Ok(l) => l,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let list_pin = ctx.pin_native_root(list);
    let cap = std::cmp::max(values.len(), AL_DEFAULT_CAPACITY) + 1;
    let buf: ObjectRef = alloc_ref_array(ctx, cap);
    for (i, val) in values.iter().enumerate() {
        let val = read_pinned_elem(ctx, val_handles[i], *val);
        ctx.set_array_element(buf, i, val);
    }
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_array_element(buf, cap - 1, Value::Object(Some(this)));
    let list = ctx.read_native_pin(list_pin, list);
    store_view_carrier_backref(ctx, list, this);
    al_set_data(ctx, list, buf);
    al_set_size(ctx, list, values.len() as i32);
    ctx.unpin_native_roots(this_pin);
    // `this` is a pre-allocation address by now. Re-derive the source from the
    // view's own back-reference, which is the one pointer to it guaranteed live
    // and current -- and do it BEFORE the synchronized wrap, because the
    // wrapper is not an ArrayList and `values_view_source` cannot read one.
    // Taking it afterwards is why a `Hashtable` values view was never stored.
    let src = values_view_source(&*ctx, list);
    let list = if sync {
        wrap_synchronized_view(ctx, list, false)?
    } else {
        list
    };
    if let Some(src) = src {
        store_live_values_view(ctx, src, list);
    }
    Ok(Some(Value::Object(Some(list))))
}

/// Build a live-view `java.util.ArrayList` from an already-collected `values`
/// slice, tagging it with `source` in the trailing capacity slot exactly like
/// [`native_map_values`] does -- so it participates in the same already-hardened
/// `values_view_source`/`resync_values_view`/`propagate_list_removal` machinery
/// used by `native_al_remove_obj`/`native_al_clear`/iterator `.remove()`
/// (`values().remove(v)`, `.iterator().remove()`, and `.clear()` write through
/// to `source` via its virtual `remove`/`clear`).
///
/// Exposed for callers (e.g. `java.util.Properties`'s side-table-backed
/// `values()`) whose data does not live in a native bucket table, so
/// `map_collect_values` cannot be reused directly -- the caller collects the
/// correct value list itself and hands it here just to get live-view wiring.
pub fn make_live_values_list(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
    values: &[Value],
) -> Result<ObjectRef, MethodCallFailed> {
    let carrier = values_carrier_for(&*ctx, source);
    let sync = wants_synchronized_views(&*ctx, source);
    let source_pin = ctx.pin_native_root(source);
    let (_, val_handles) = pin_value_slice(ctx, values);
    // gc-common w20-a: not `?` -- a refused carrier used to strand
    // `source_pin` and every value pin above it.
    let list = match alloc_view_carrier(ctx, carrier) {
        Ok(l) => l,
        Err(e) => {
            ctx.unpin_native_roots(source_pin);
            return Err(e);
        }
    };
    let list_pin = ctx.pin_native_root(list);
    let cap = std::cmp::max(values.len(), AL_DEFAULT_CAPACITY) + 1;
    let buf: ObjectRef = alloc_ref_array(ctx, cap);
    for (i, val) in values.iter().enumerate() {
        let val = read_pinned_elem(ctx, val_handles[i], *val);
        ctx.set_array_element(buf, i, val);
    }
    let mut source = ctx.read_native_pin(source_pin, source);
    ctx.set_array_element(buf, cap - 1, Value::Object(Some(source)));
    let list = ctx.read_native_pin(list_pin, list);
    store_view_carrier_backref(ctx, list, source);
    al_set_data(ctx, list, buf);
    al_set_size(ctx, list, values.len() as i32);
    ctx.unpin_native_roots(source_pin);
    if sync {
        return wrap_synchronized_view(ctx, list, false);
    }
    Ok(list)
}

pub(crate) fn native_map_entry_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    if is_tree_map_receiver(ctx, this) {
        return native_tm_entry_set(ctx, args);
    }
    // A JDK-model `ConcurrentHashMap` (nio21 w1 L4): its own `EntrySetView`.
    if let Some(r) = chm_jdk_delegate(ctx, args, "entrySet", "()Ljava/util/Set;", 0) {
        return r;
    }
    // See `native_map_key_set`. An entrySet view is live the same way — every
    // read resyncs it — so the instance is reusable. Unlike keySet the READ is
    // not elided (a value-replacing `put` changes an entrySet's contents
    // without moving `modCount`), so this saves the construction only.
    if let Some(cached) = cached_live_view(ctx, this, VIEW_KIND_ENTRYSET) {
        return Ok(Some(Value::Object(Some(cached))));
    }
    MAP_VIEW_BUILT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Taken before anything allocates — see `native_map_key_set`.
    let sync = wants_synchronized_views(&*ctx, this);
    // Both allocations below may run a moving young collection. Keep the
    // source and newly created view rooted, then collect entries only after
    // the view is fully wired; otherwise `set_field(set, ...)` can target the
    // pre-move Set and leave entrySet().iterator() observing a null backing.
    let this_pin = ctx.pin_native_root(this);
    let entry_count = map_collect_entries(ctx, this).len();
    // Build an entrySet carrier holding Map.Entry objects, backed by a view
    // backing that remembers the source map so removing an entry through the
    // set (or its iterator) deletes the corresponding key from the source map.
    let carrier = set_view_carrier_for(&*ctx, this, VIEW_KIND_ENTRYSET);
    let set = alloc_set_view_carrier(ctx, carrier)?;
    let set_pin = ctx.pin_native_root(set);
    let cap = std::cmp::max(entry_count.next_power_of_two(), MAP_DEFAULT_CAPACITY);
    let mut this = ctx.read_native_pin(this_pin, this);
    let backing_map = alloc_view_backing(ctx, &mut this, VIEW_KIND_ENTRYSET, cap)?;
    let backing_pin = ctx.pin_native_root(backing_map);
    let set = ctx.read_native_pin(set_pin, set);
    let backing_map = ctx.read_native_pin(backing_pin, backing_map);
    hs_set_backing_map(ctx, set, backing_map);
    let this = ctx.read_native_pin(this_pin, this);
    let set = ctx.read_native_pin(set_pin, set);
    store_set_view_backref(ctx, set, this);
    let this = ctx.read_native_pin(this_pin, this);
    let entries = map_collect_entries(ctx, this);
    // cceres3: pin across GC-capable call (stream stale-at-store wave) — the
    // per-entry alloc_synthetic/map_alloc_node allocations move `this`,
    // `backing_map`, its bucket array, and every not-yet-stored (key, value)
    // pair; re-read all of them through their pins per iteration (mirrors the
    // already-fixed resync_view_set).
    let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (_, pair_handles) = pin_value_slice(ctx, &flat);

    // Each entry is a Map.Entry object with 2 fields: key and value.
    // Use `java/util/AbstractMap$SimpleEntry` rather than the interface
    // `java/util/Map$Entry` (HotSpot cannot produce an instance of an
    // interface -- `H16-2` N2) or the concrete nested class `HashMap$Entry`:
    // in early bootstrap the concrete nested class can be unresolved and
    // degrade to cid=0 (`java/lang/Object`), breaking downstream checkcasts,
    // while `AbstractMap$SimpleEntry` is always resolvable and is the exact
    // class HotSpot itself hands out for a detached/live entry snapshot.
    for (i, (key, value)) in entries.iter().enumerate() {
        // 3-field entry: key@0, value@1, sourceMap@2. The source-map
        // reference makes `Entry.setValue(v)` write back to the originating map
        // (`native_entry_set_value`), matching the JDK live-entry contract
        // (e.g. keycloak's StripSecretsUtils masks config values via
        // `entrySet().iterator() ... entry.setValue(maskedList)`). It is a
        // GC-scanned object field, so it survives relocation (a Rust side-table
        // holding the ObjectRef would go stale).
        let mut entry_obj = try_alloc_synthetic(ctx, "java/util/AbstractMap$SimpleEntry", 3)?;
        let key = read_pinned_elem(ctx, pair_handles[2 * i], *key);
        let value = read_pinned_elem(ctx, pair_handles[2 * i + 1], *value);
        let this = ctx.read_native_pin(this_pin, this);
        ctx.set_field(entry_obj, 0, key);
        ctx.set_field(entry_obj, 1, value);
        ctx.set_field(entry_obj, 2, Value::Object(Some(this)));

        // Add to the set's backing map
        let hash = ctx.identity_hash_code(entry_obj);
        let mut backing_map = ctx.read_native_pin(backing_pin, backing_map);
        let (b, size, c) = map_state(ctx, backing_map);
        // Materialise instead of unwrapping. These builders run over a backing
        // map this crate allocated, and `alloc_view_backing` still allocates its
        // bucket array up front -- but the `unwrap` was the single hazard that
        // made `WORKER-2-NOTE-1` §9a.3 call lazy allocation a ten-site audit, so
        // it is removed rather than reasoned about. `map_resize` on a
        // table-less map is exactly the path `native_map_put` already takes.
        let b = match b {
            Some(b) => b,
            None => {
                // gc-common w20-a: the resize allocates; the entry (not yet
                // reachable from anything) is rooted across it.
                rooted_across(ctx, &mut [&mut entry_obj], |ctx| {
                    map_resize(ctx, &mut backing_map, GrowAlloc::SingleAttempt)
                });
                let (b2, _, _) = map_state(ctx, backing_map);
                match b2 {
                    Some(b2) => b2,
                    None => continue,
                }
            }
        };
        let idx = map_bucket_index(hash, c);
        let existing = ctx.get_array_element(b, idx);
        let head = match existing {
            Value::Object(obj_opt) => obj_opt,
            _ => None,
        };
        let sentinel = Value::Int(1);
        let b_pin = ctx.pin_native_root(b);
        let node = map_alloc_node(ctx, entry_obj, sentinel, hash, head)?;
        let b = ctx.read_native_pin(b_pin, b);
        ctx.set_array_element(b, idx, Value::Object(Some(node)));
        let backing_map = ctx.read_native_pin(backing_pin, backing_map);
        set_map_size(ctx, backing_map, size + 1);
        ctx.unpin_native_roots(b_pin);
    }

    let set = ctx.read_native_pin(set_pin, set);
    let this_now = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(backing_pin);
    ctx.unpin_native_roots(set_pin);
    ctx.unpin_native_roots(this_pin);
    let set = if sync {
        wrap_synchronized_view(ctx, set, true)?
    } else {
        set
    };
    // `wrap_synchronized_view` allocates, but only on the `sync` arm — which
    // `store_live_view` refuses anyway, so `this_now` cannot be stale here.
    store_live_view(ctx, this_now, VIEW_KIND_ENTRYSET, set);
    Ok(Some(Value::Object(Some(set))))
}

/// Build a **static** live `entrySet()` Set from an explicit list of
/// `(key, value)` pairs backed by `source`. Mirrors [`native_map_entry_set`]
/// (3-field live `Map$Entry` elements, a view backing that remembers `source`
/// so `remove`/`iterator().remove()` and `Entry.setValue` write through to
/// `source` via its virtual `remove`/`put`), but tagged
/// `VIEW_KIND_ENTRYSET_STATIC`, which decides HOW the view refreshes rather
/// than whether it does.
///
/// This is for sources whose entries cannot be read back by walking their
/// fields — `java/util/Properties`, whose data lives in a Rust side-table. A
/// standard (field-walking) entrySet view would walk `Properties.entrySet()` on
/// every `iterator()` and recurse, so a read of this kind instead calls the
/// source's own `entrySet()` once and adopts the fresh view's backing
/// ([`adopt_fresh_view_backing`]). Because the write-through is keyed on this
/// specific view backing (not on the entries' source field), a later
/// `new HashSet<>(props.entrySet())` copy is a plain set and its `remove` is
/// correctly detached from the source.
pub fn make_static_entry_set(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
    entries: &[(Value, Value)],
) -> Result<ObjectRef, MethodCallFailed> {
    // cceres5 (metrics-registry `(String) entry.getKey()` CCE, live-captured
    // 2026-07-22): every allocation below — the set, the view backing, and the
    // per-entry Map$Entry/node pair — can trigger a moving GC while the
    // caller's snapshot `(key, value)` refs sit only in this Rust slice.
    // Storing the raw refs baked pre-move addresses into the freshly built
    // entries; `(String) entry.getKey()` then observed a zeroed/reused block
    // identifying as bare `java.lang.Object`. Pin + re-read everything,
    // mirroring `resync_view_set`'s entry loop.
    let sync = wants_synchronized_views(&*ctx, source);
    let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (elem_base, flat_pins) = pin_value_slice(ctx, &flat);
    let source_pin = ctx.pin_native_root(source);
    let first_pin = if elem_base == usize::MAX {
        source_pin
    } else {
        elem_base
    };
    let carrier = set_view_carrier_for(&*ctx, source, VIEW_KIND_ENTRYSET_STATIC);
    let set = alloc_set_view_carrier(ctx, carrier)?;
    let set_pin = ctx.pin_native_root(set);
    let cap = std::cmp::max(entries.len().next_power_of_two(), MAP_DEFAULT_CAPACITY);
    let mut source_now = ctx.read_native_pin(source_pin, source);
    let backing_map = alloc_view_backing(ctx, &mut source_now, VIEW_KIND_ENTRYSET_STATIC, cap)?;
    let backing_pin = ctx.pin_native_root(backing_map);
    let set_now = ctx.read_native_pin(set_pin, set);
    hs_set_backing_map(ctx, set_now, backing_map);
    let sentinel = Value::Int(1);
    for i in 0..entries.len() {
        // 3-field entry: key@0, value@1, sourceMap@2 — so `Entry.setValue`
        // writes through to `source` (see `native_entry_set_value`).
        // `AbstractMap$SimpleEntry`, not the interface `Map$Entry` — HotSpot
        // cannot produce an instance of an interface (`H16-2` N2).
        let entry_obj = try_alloc_synthetic(ctx, "java/util/AbstractMap$SimpleEntry", 3)?;
        let entry_pin = ctx.pin_native_root(entry_obj);
        let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
        let value = read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]);
        let source_now = ctx.read_native_pin(source_pin, source);
        ctx.set_field(entry_obj, 0, key);
        ctx.set_field(entry_obj, 1, value);
        ctx.set_field(entry_obj, 2, Value::Object(Some(source_now)));

        let hash = ctx.identity_hash_code(entry_obj);
        let backing_now = ctx.read_native_pin(backing_pin, backing_map);
        let (b, size, c) = map_state(ctx, backing_now);
        let b = match b {
            Some(b) => b,
            None => {
                ctx.unpin_native_roots(entry_pin);
                continue;
            }
        };
        let b_pin = ctx.pin_native_root(b);
        let idx = map_bucket_index(hash, c);
        let existing = ctx.get_array_element(b, idx);
        let head = match existing {
            Value::Object(obj_opt) => obj_opt,
            _ => None,
        };
        let entry_now = ctx.read_native_pin(entry_pin, entry_obj);
        let node = map_alloc_node(ctx, entry_now, sentinel, hash, head)?;
        let b_now = ctx.read_native_pin(b_pin, b);
        ctx.set_array_element(b_now, idx, Value::Object(Some(node)));
        let backing_now = ctx.read_native_pin(backing_pin, backing_map);
        set_map_size(ctx, backing_now, size + 1);
        // LIFO scope: entry_pin was this iteration's first pin; truncating from
        // it releases b_pin too.
        ctx.unpin_native_roots(entry_pin);
    }
    let set_now = ctx.read_native_pin(set_pin, set);
    ctx.unpin_native_roots(first_pin);
    if sync {
        return wrap_synchronized_view(ctx, set_now, true);
    }
    Ok(set_now)
}

/// The keySet twin of [`make_static_entry_set`]: a live `keySet()` view over
/// `keys`, backed by `source`, tagged [`VIEW_KIND_KEYSET_STATIC`] so a read
/// refreshes it through the source's own `keySet()` rather than by walking the
/// source's fields.
///
/// `java/util/Properties` is the caller. Its keys are half in a Rust side-table
/// and half in a `ConcurrentHashMap` field, so `map_collect_keys` — which every
/// other keySet view resyncs through — sees only the second half. Handing back
/// a plain snapshot instead was the older answer, and it cost both liveness on
/// read and write-through: `remove`/`retainAll` had to be re-implemented as an
/// override on the snapshot's class. A view carrier gets both from the shared
/// machinery (`native_hs_remove` consults the view backing).
pub fn make_static_key_set(
    ctx: &mut dyn NativeContext,
    source: &mut ObjectRef,
    keys: &[Value],
) -> Result<ObjectRef, MethodCallFailed> {
    let sync = wants_synchronized_views(&*ctx, *source);
    let set = make_view_set_of(ctx, source, VIEW_KIND_KEYSET_STATIC, keys)?;
    if sync {
        return wrap_synchronized_view(ctx, set, true);
    }
    Ok(set)
}

pub(crate) fn native_map_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let entries = map_collect_entries(ctx, this);
    // gc-common w20-a: rendered through pins (every `toString()` can move the
    // keys and values not yet rendered).
    let parts: Vec<Units> = display_entry_units_pinned(ctx, &entries)?
        .iter()
        .map(|(ks, vs)| entry_units(ks, vs))
        .collect();
    let text = wrap_join("{", &parts, ", ", "}");
    let s = ctx.create_string_from_units(&text);
    Ok(Some(Value::Object(Some(s))))
}

pub(crate) fn native_map_get_or_default(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // cceres5 (live-captured 2026-07-22, `checkcast OperationStepHandler` on a
    // bare cid=0 Object, ListenerResourceDefinition.registerAttributes'
    // `writeAttributeHandlers.getOrDefault(attr, handler)` during undertow's
    // parallel-extension-add): `native_map_get` dispatches the key's
    // hashCode/equals (arbitrary, GC-capable bytecode), and the DEFAULT value
    // sat raw in `args` across it — the common absent-key branch then
    // returned a pre-move address. Pin the default across the lookup.
    //
    // `this` and `key` are pinned for the same reason, added with the
    // `containsKey` re-ask below: that second lookup needs BOTH of them AFTER
    // the first one has run arbitrary bytecode, and re-reading them out of
    // `args` would hand the disambiguating question a pre-move receiver — the
    // very defect this comment block was written about, one call later.
    let this0 = args.first().copied().unwrap_or(Value::Object(None));
    let key0 = args.get(1).copied().unwrap_or(Value::Object(None));
    let default0 = args.get(2).copied().unwrap_or(Value::Object(None));
    let this_pin = pin_value(ctx, this0);
    let key_pin = pin_value(ctx, key0);
    let default_pin = pin_value(ctx, default0);
    let pin_base = [this_pin, key_pin, default_pin]
        .into_iter()
        .find(|p| *p != usize::MAX);
    // gen r4w4/rooting2: the pins now stay up across the `containsKey`
    // re-ask below as well. They used to be dropped here, and the default was
    // re-read BEFORE that second lookup -- which runs the key's
    // `hashCode()`/`equals()` again -- so the key-absent branch (the common
    // one) could still hand back a pre-move default.
    let result = (|| -> MethodCallResult {
        let got = native_map_get(ctx, args)?;
        let this_now = read_pinned_elem(ctx, this_pin, this0);
        let key_now = read_pinned_elem(ctx, key_pin, key0);
        Ok(match got {
            // See the note below the match on why a null `get` is re-asked.
            Some(Value::Object(None)) => {
                let present = matches!(
                    native_map_contains_key(ctx, &[this_now, key_now])?,
                    Some(Value::Int(1))
                );
                if present {
                    Some(Value::Object(None))
                } else {
                    Some(read_pinned_elem(ctx, default_pin, default0))
                }
            }
            other => other,
        })
    })();
    if let Some(base) = pin_base {
        ctx.unpin_native_roots(base);
    }
    // Why a null `get` is re-asked: a null answer from `get` is ambiguous in
    // exactly the way `Map.getOrDefault` is specified to resolve: "the value to
    // which the specified key is mapped, or `defaultValue` if this map contains
    // **no mapping** for the key". A key present with a null value has a
    // mapping, so it answers `null` — not the default.
    //
    // `HashMap.getOrDefaultOverNullValue` measured `DEFAULT` here against
    // HotSpot's `null`, after `nm.put("k", null)`. `HashMap.getOrDefault` reads
    // the NODE (`(e = getNode(key)) == null ? defaultValue : e.value`) and so
    // never has to ask twice; this path only has the value, so it asks the
    // second question the `Map` interface's own default implementation asks —
    // `(((v = get(key)) != null) || containsKey(key))`.
    //
    // The extra `containsKey` costs one more lookup on a MISS, which is the
    // common case for this method. That is the price of the distinction and
    // the interface default pays it too; the alternative is answering "absent"
    // for a mapping that exists, which no caller can detect.
    result
}

/// `HashMap.putIfAbsent` as the JDK runs it, `putVal(hash(key), key, value,
/// true, true)`: ONE `hashCode()` and one chain walk, where the composite
/// below (`get` + `put`) made two of each for an absent key. Under
/// `CRATONVM_COMPAT_MAP_SINGLE_WALK`, for the plain `HashMap` receivers
/// [`map_single_walk_eligible`] admits (no `Hashtable`, whose own bytecode
/// serves it). A primitive-wrapper key keeps the composite: its hash and
/// equality are not observable, and the composite's `put` is what starts the
/// integer overlay on a fresh exact `HashMap`. `None` before any Java has run.
/// Round 13 wave 8, lane hashcompat3.
pub(crate) fn map_put_if_absent_single_walk(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key: Value,
    value: Value,
) -> Option<MethodCallResult> {
    if !map_single_walk_eligible(&*ctx, this)
        || receiver_facts(&*ctx, this).has(CF_HASHTABLE_ANCESTRY)
    {
        return None;
    }
    match key {
        Value::Object(Some(k)) if unbox_wrapper(&*ctx, k).is_some() => return None,
        Value::Object(_) => {}
        _ => return None,
    }
    let mut this = this;
    let put_pin_base = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let value_pin = pin_value(ctx, value);
    let result = native_map_put_evict_pinned_hashed(
        ctx,
        &mut this,
        key,
        value,
        true,
        put_pin_base,
        key_pin,
        value_pin,
        GrowAlloc::SingleAttempt,
        None,
        true,
    );
    ctx.unpin_native_roots(put_pin_base);
    Some(result)
}

pub(crate) fn native_map_put_if_absent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    if let Some(result) = map_put_if_absent_single_walk(ctx, this, key, value) {
        return result;
    }
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let value_pin = pin_value(ctx, value);
    let result = (|| -> MethodCallResult {
        let this = ctx.read_native_pin(this_pin, this);
        let key = read_pinned_elem(ctx, key_pin, key);
        let value = read_pinned_elem(ctx, value_pin, value);
        let get_result = native_map_get(ctx, &[Value::Object(Some(this)), key, value])?;
        match get_result {
            Some(Value::Object(None)) => {
                let this = ctx.read_native_pin(this_pin, this);
                let key = read_pinned_elem(ctx, key_pin, key);
                let value = read_pinned_elem(ctx, value_pin, value);
                native_map_put(ctx, &[Value::Object(Some(this)), key, value])
            }
            _ => Ok(get_result),
        }
    })();
    ctx.unpin_native_roots(this_pin);
    return result;

    #[allow(unreachable_code)]
    // Check if key exists
    let get_result = native_map_get(ctx, args)?;
    match get_result {
        Some(Value::Object(None)) => {
            // Key not present — do the put
            native_map_put(ctx, args)
        }
        _ => Ok(get_result), // Key present — return existing value
    }
}

pub(crate) fn native_map_put_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // A null source map silently no-op'd here. MEASURED on the oracle, and the
    // message is receiver-routed because the two JDK bodies dereference
    // different members of it:
    //
    // * `HashMap.putAll(m)`   -> `putMapEntries(m, true)` -> `int s = m.size();`
    //   `Cannot invoke "java.util.Map.size()" because "m" is null`
    //   (`LinkedHashMap` and `Properties` measure identically)
    // * `Hashtable.putAll(t)` -> `for (Map.Entry e : t.entrySet())`
    //   `Cannot invoke "java.util.Map.entrySet()" because "t" is null`
    //
    // Both are TRANSCRIBED, including the JDK's own parameter names `m` / `t`.
    // As everywhere else here, only an explicitly-passed null throws; a
    // missing argument stays the malformed-call no-op it was.
    if matches!(args.get(1), Some(Value::Object(None))) {
        let message = if is_plain_hashtable_receiver(ctx, this) {
            "Cannot invoke \"java.util.Map.entrySet()\" because \"t\" is null"
        } else {
            "Cannot invoke \"java.util.Map.size()\" because \"m\" is null"
        };
        return Err(RuntimeError::NullPointerException {
            message: Some(message.to_string()),
        }
        .into());
    }
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // Collect entries from the source, dispatching on its concrete backend
    // (LinkedHashMap overlay, TreeMap tree, HashMap/CHM buckets, or — via the
    // `entrySet().iterator()` fallback — any other `Map` such as the real-JDK
    // `Collections$Unmodifiable*Map` wrappers). Previously this only handled
    // LinkedHashMap and fell back to HashMap-bucket scanning, so `putAll(tm)`
    // for a TreeMap / unmodifiable-sorted-map source silently copied nothing
    // (the Elasticsearch `Settings.Builder.put(Settings)` failure).
    // cceres3: pin across GC-capable call (stream stale-at-store wave) — each
    // native_map_put re-enters Java, moving `this` and every pending pair.
    let this_pin = ctx.pin_native_root(this);
    // `Properties.putAll(t)` is `map.putAll(t)` over its CHM, whose `putVal`
    // refuses a null key or value (`CRATONVM_COMPAT_PUT_ALL_NULL_KEY`). Pure
    // reads, before any Java.
    let chm_nulls = put_all_null_key_enabled() && properties_receiver_chm(&*ctx, this).is_some();
    // `Hashtable.putAll` asks `put` virtually (`CRATONVM_COMPAT_PUT_ALL_VIRTUAL_PUT`).
    let virtual_put = put_all_virtual_put_enabled()
        && is_plain_hashtable_receiver(&*ctx, this)
        && class_is_programs(&*ctx, this);
    // C4W4-1: a plain `HashMap`-family receiver (never `Hashtable` /
    // `Properties` / `LinkedHashMap`). Pure reads.
    let presize = hashmap_copy_presize_applies(&*ctx, this);
    // C3W3-2 (`CRATONVM_COMPAT_MAP_COPY_JDK_QUESTIONS`, round 15 wave 4, lane
    // compat4): a program's map is asked `HashMap.putMapEntries(m, true)`'s
    // questions (`size()` gates the walk), `Hashtable.putAll`'s (the walk
    // alone) or, for `Properties`, `ConcurrentHashMap.putAll`'s (`size()`
    // once its table exists), each put interleaved.
    if let Some(prog) = foreign_map_source(&*ctx, other) {
        let size_ask = if is_plain_hashtable_receiver(&*ctx, this) {
            MapSizeAsk::Skip
        } else if let Some(chm) = properties_receiver_chm(&*ctx, this) {
            if chm_entry_count(&*ctx, chm) > 0 {
                MapSizeAsk::Ask
            } else {
                MapSizeAsk::Skip
            }
        } else {
            MapSizeAsk::Gate
        };
        let prog_pin = ctx.pin_native_root(prog);
        let walked = map_for_each_entry_sized(
            ctx,
            (prog_pin, prog),
            size_ask,
            |ctx, s| {
                // C4W4-1: `putMapEntries`' pre-size, after `m.size()`.
                let this_now = ctx.read_native_pin(this_pin, this);
                if presize && hashmap_table_is_null(&*ctx, this_now) {
                    hashmap_presize_null_table(ctx, this_now, s);
                } else if presize {
                    // JIT round 16 (lane compatdrain), r15w4-compat5 item 3:
                    // the live-table arm, for the program's `size()` answer.
                    hashmap_presize_live_table(ctx, (this_pin, this), s);
                }
            },
            |ctx, key, value| {
                if chm_nulls {
                    chm_bare_npe_on_null(&key, &value)?;
                }
                let this_now = ctx.read_native_pin(this_pin, this);
                if virtual_put {
                    map_put_virtual(ctx, this_now, key, value)?;
                } else {
                    native_map_put(ctx, &[Value::Object(Some(this_now)), key, value])?;
                }
                Ok(())
            },
        );
        ctx.unpin_native_roots(this_pin);
        return walked.map(|()| None);
    }
    let collected = collect_entries_any(ctx, other);
    let entries = unpin_on_err(ctx, this_pin, collected)?;
    let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (_, pair_handles) = pin_value_slice(ctx, &flat);
    // C4W4-1 (`CRATONVM_COMPAT_HASHMAP_COPY_PRESIZE`, round 15 wave 4, lane
    // compat5): `putMapEntries(m, true)`'s `table == null` arm sizes the table
    // from `m.size()` before the first put -- `new HashMap<>()` then
    // `putAll(twoEntries)` iterates over 4 buckets on HotSpot, not 16. A map
    // with a table grows per put to the table the JDK's `while (s >
    // threshold) resize()` reaches for an honest `size()`.
    if presize {
        let this_now = ctx.read_native_pin(this_pin, this);
        if hashmap_table_is_null(&*ctx, this_now) {
            let s = i32::try_from(entries.len()).unwrap_or(i32::MAX);
            hashmap_presize_null_table(ctx, this_now, s);
        }
    }
    for (i, (key, value)) in entries.iter().enumerate() {
        let key = read_pinned_elem(ctx, pair_handles[2 * i], *key);
        // `CRATONVM_COMPAT_PUT_ALL_NULL_KEY`: a null key reaches the put (it
        // was skipped); `=0` keeps only object keys, as before.
        let keep = match key {
            Value::Object(Some(_)) => true,
            Value::Object(None) => put_all_null_key_enabled(),
            _ => false,
        };
        if keep {
            // Call put on this map
            let this = ctx.read_native_pin(this_pin, this);
            let value = read_pinned_elem(ctx, pair_handles[2 * i + 1], *value);
            if chm_nulls {
                if let Err(e) = chm_bare_npe_on_null(&key, &value) {
                    ctx.unpin_native_roots(this_pin);
                    return Err(e);
                }
            }
            let put_args = [Value::Object(Some(this)), key, value];
            let put = if virtual_put {
                map_put_virtual(ctx, this, key, value)
            } else {
                native_map_put(ctx, &put_args)
            };
            if let Err(e) = put {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        }
    }
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub(crate) fn native_map_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let entries = map_collect_entries(ctx, this);
    let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (pin_base, handles) = pin_value_slice(ctx, &flat);
    let mut hash: i32 = 0;
    for i in 0..entries.len() {
        let key = read_pinned_elem(ctx, handles[2 * i], flat[2 * i]);
        // Map.hashCode contract: sum of Map.Entry hashes,
        // where Entry hash = keyHash ^ valueHash.
        let kh = match element_hash_code(ctx, &key) {
            Ok(h) => h,
            Err(e) => {
                ctx.unpin_native_roots(pin_base);
                return Err(e);
            }
        };
        let value = read_pinned_elem(ctx, handles[2 * i + 1], flat[2 * i + 1]);
        let vh = match element_hash_code(ctx, &value) {
            Ok(h) => h,
            Err(e) => {
                ctx.unpin_native_roots(pin_base);
                return Err(e);
            }
        };
        hash = hash.wrapping_add(kh ^ vh);
    }
    ctx.unpin_native_roots(pin_base);
    Ok(Some(Value::Int(hash)))
}

/// True when `obj`'s runtime class implements `java/util/Map` (directly or via
/// any superclass / super-interface). `is_subclass` walks the interface graph,
/// so this answers "is `obj` a Map" even for arbitrary user implementations.
pub(crate) fn object_is_map(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    match ctx.class_id_by_name("java/util/Map") {
        Some(map_cid) => ctx.is_subclass(ctx.class_id_of_object(obj), map_cid),
        None => false,
    }
}

pub(crate) fn native_map_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    if std::ptr::eq(this.as_ptr(), other.as_ptr()) {
        return Ok(Some(Value::Int(1)));
    }
    // `Map.equals(o)` is `false` for a non-Map argument (`AbstractMap.equals`:
    // `if (!(o instanceof Map)) return false`). The virtual-dispatch reads
    // below would otherwise invoke `other.size()` / `other.get(...)` on an
    // object that has no such methods and throw.
    if !object_is_map(ctx, other) {
        return Ok(Some(Value::Int(0)));
    }

    // RC-3 fix (SC-map-multivaluemap-family): `this` is always a
    // natively-modelled map — this native is registered on
    // HashMap/Hashtable/Properties and LinkedHashMap inherits it — so its size
    // and entries are read directly via the native helpers. `other`, however,
    // may be ANY `Map`: a TreeMap / ConcurrentHashMap (segmented or tree
    // layouts that `map_state` cannot read as buckets), or an arbitrary Java
    // `Map` such as Spring's `LinkedMultiValueMap` whose slot 0 is its
    // `targetMap` field, not a bucket array. Reading `other` through
    // `map_state` / `native_map_get` reported size 0 / no matching entries, so
    // `nativeMap.equals(foreignMap)` wrongly returned `false`
    // (LinkedMultiValueMapTests.equals). Route every `other` access through
    // virtual dispatch — `other.size()` / `other.get(k)` / `other.containsKey(k)`
    // resolve to whatever natives or bytecode `other`'s real class provides,
    // restoring `AbstractMap.equals` semantics for cross-implementation
    // comparisons.
    let this_pin = ctx.pin_native_root(this);
    let other_pin = ctx.pin_native_root(other);
    let result = (|| {
        let this = ctx.read_native_pin(this_pin, this);
        let other = ctx.read_native_pin(other_pin, other);
        let size_a = hm_int_fast_len(ctx, this)
            .map(|size| size as i32)
            .unwrap_or_else(|| map_state(ctx, this).1);
        let size_b = match ctx.invoke_virtual(other, "size", "()I", &[])? {
            Some(Value::Int(s)) => s,
            _ => {
                let other = ctx.read_native_pin(other_pin, other);
                let (_, s, _) = map_state(ctx, other);
                s
            }
        };
        if size_a != size_b {
            return Ok(Some(Value::Int(0)));
        }
        let this = ctx.read_native_pin(this_pin, this);
        let other = ctx.read_native_pin(other_pin, other);
        match map_equals_entries(ctx, this, other) {
            Ok(equal) => Ok(Some(Value::Int(if equal { 1 } else { 0 }))),
            Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc))
                if exc_is_cce_or_npe(ctx, exc) =>
            {
                Ok(Some(Value::Int(0)))
            }
            Err(e) => Err(e),
        }
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

/// `CRATONVM_COMPAT_MAP_EQUALS_JDK` — default ON, `=0` restores the old
/// comparison. `AbstractMap.equals` (and `Hashtable.equals`) decide a
/// non-null value with `value.equals(m.get(key))`: the STORED value's
/// `equals`, called even when the other map answers the very same object or
/// null. The native short-cut identical values and never asked about a null
/// answer, so a side-effecting or counting `equals` saw fewer calls. Round 13
/// wave 9, lane hashcompat4.
pub(crate) fn map_equals_jdk_enabled() -> bool {
    pub(crate) static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_MAP_EQUALS_JDK")
}

/// Returns `true` iff `exc` is a `ClassCastException` or `NullPointerException`
/// (the two exception types `AbstractMap.equals` / `AbstractMap.hashCode`
/// swallow during cross-implementation comparison).
pub(crate) fn exc_is_cce_or_npe(ctx: &mut dyn NativeContext, exc: ObjectRef) -> bool {
    let class_id = ctx.class_id_of_object(exc);
    matches!(
        ctx.class_name_arc_of_id(class_id).as_deref(),
        Some("java/lang/ClassCastException") | Some("java/lang/NullPointerException")
    )
}

/// The per-entry comparison loop of `Map.equals`, factored out so the caller can
/// apply `AbstractMap.equals`'s `catch (ClassCastException | NullPointerException)
/// { return false; }` around it. Returns `Ok(true)` if every entry in `this` is
/// present-and-equal in `other`, `Ok(false)` otherwise.
///
/// Null-value handling (MED fix, preserved): a `null`-valued entry must be
/// matched by `other` mapping the same key to `null` AND actually containing it
/// (distinguish present-null from absent, per the contract). Null-keyed entries
/// are included by dispatching the lookup with a `null` key.
pub(crate) fn map_equals_entries(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    other: ObjectRef,
) -> Result<bool, cratonvm_types::error::MethodCallFailed> {
    let entries = map_collect_entries(ctx, this);
    // `AbstractMap.equals` / `Hashtable.equals` shape only: `Properties`
    // delegates to its `ConcurrentHashMap`, whose `equals` asks the OTHER
    // map's value (`v == val || v.equals(val)`).
    let jdk_values = map_equals_jdk_enabled() && {
        let facts = receiver_facts(&*ctx, this);
        facts.has(CF_BUCKET_MAP_NAME)
            && !facts.has(CF_CHM)
            && !(facts.has(CF_HASHTABLE_ANCESTRY) && !facts.has(CF_HASHTABLE_LAYOUT))
    };
    let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (entries_pin, handles) = pin_value_slice(ctx, &flat);
    let other_pin = ctx.pin_native_root(other);
    let result = (|| {
        for i in 0..entries.len() {
            let key = read_pinned_elem(ctx, handles[2 * i], flat[2 * i]);
            let other = ctx.read_native_pin(other_pin, other);
            let other_val = ctx
                .invoke_virtual(
                    other,
                    "get",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                    &[key],
                )?
                .unwrap_or(Value::Object(None));
            let value = read_pinned_elem(ctx, handles[2 * i + 1], flat[2 * i + 1]);
            match &value {
                Value::Object(None) => {
                    if !matches!(other_val, Value::Object(None)) {
                        return Ok(false);
                    }
                    let other = ctx.read_native_pin(other_pin, other);
                    let key = read_pinned_elem(ctx, handles[2 * i], flat[2 * i]);
                    let has =
                        ctx.invoke_virtual(other, "containsKey", "(Ljava/lang/Object;)Z", &[key])?;
                    if !matches!(has, Some(Value::Int(1))) {
                        return Ok(false);
                    }
                }
                _ => {
                    // `if (!value.equals(m.get(key))) return false;` -- the
                    // stored value's own `equals`, with no `==` short cut and
                    // also for a null answer (`CRATONVM_COMPAT_MAP_EQUALS_JDK`).
                    // Wrappers and String pairs keep the no-Java comparison:
                    // theirs is not observable.
                    let stored_equals = jdk_values
                        && match (&value, &other_val) {
                            (Value::Object(Some(va)), Value::Object(Some(vb))) => {
                                unbox_wrapper(&*ctx, *va).is_none()
                                    && ctx.java_strings_equal(*va, *vb).is_none()
                            }
                            (Value::Object(Some(va)), _) => unbox_wrapper(&*ctx, *va).is_none(),
                            _ => false,
                        };
                    let eq = match (&value, &other_val) {
                        (Value::Object(Some(va)), _) if stored_equals => {
                            let answer = ctx.invoke_virtual(
                                *va,
                                "equals",
                                "(Ljava/lang/Object;)Z",
                                &[other_val],
                            )?;
                            matches!(answer, Some(Value::Int(v)) if v != 0)
                        }
                        (Value::Object(Some(va)), Value::Object(Some(vb))) => {
                            map_keys_equal(ctx, *va, *vb)?
                        }
                        _ => values_equal(ctx, &value, &other_val),
                    };
                    if !eq {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    })();
    ctx.unpin_native_roots(entries_pin);
    result
}

