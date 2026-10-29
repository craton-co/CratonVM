// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

// LinkedHashMap — insertion-ordered HashMap
// ===========================================================================
// LinkedHashMap = 5-field synthetic:
//   0: buckets (Object[]), 1: size (Int), 2: capacity (Int), 3: head (Node), 4: tail (Node)
// LinkedHashMap$Entry = the REAL 6-field JDK node (see `LHM_NODE_*` below):
//   0: hash, 1: key, 2: value, 3: next (bucket chain), 4: before, 5: after (insertion order)

pub(crate) const LHM_FIELD_BUCKETS: usize = 0;
pub(crate) const LHM_FIELD_SIZE: usize = 1;
pub(crate) const LHM_FIELD_CAPACITY: usize = 2;
pub(crate) const LHM_FIELD_HEAD: usize = 3;
pub(crate) const LHM_FIELD_TAIL: usize = 4;

// Real-JDK LinkedHashMap inherits from HashMap, which has many declared fields
// (`table`, `entrySet`, `size`, `modCount`, `threshold`, `loadFactor`) plus
// AbstractMap (`keySet`, `values`) and LinkedHashMap's own (`head`, `tail`,
// `accessOrder`, `putMode`). The synthetic 5-slot layout (buckets, size,
// capacity, head, tail at indices 0..4) only matches a stub; against the real
// JDK the indices line up with completely different fields, so writes to the
// `LHM_FIELD_SIZE=1` slot land in `AbstractMap.values` (a Collection ref) and
// reads of size find Object(None), producing `LinkedHashMap.size()==0` even
// after multiple puts. (Fortunately the `buckets` slot also gets aliased to a
// reference field, so the chain itself stays consistent for get/put — only
// `size()` / `isEmpty()` / iteration short-circuit.)
//
// Spring 5.0's `LinkedMultiValueMap.addAll` calls
// `targetMap.computeIfAbsent(key, lambda)` on a `LinkedHashMap` and then walks
// `targetMap.entrySet()`; with the synthetic offsets, both the size-based
// short-circuit in `Map.computeIfAbsent` and the iterator's emptiness check
// see `size==0`, so the cache map ends up empty and `loadFactoryNames` returns
// 0 — precisely the SportMe `ServletWebServerFactory` missing-bean failure.
//
// These helpers route to a side-table when the synthetic slot indices would
// alias real-JDK declared fields. Storing LinkedHashMap state externally,
// keyed by ObjectRef, sidesteps the layout-mismatch problem entirely without
// touching the (other-agent-owned) HashMap allocation/resolution logic.
//
// We try a slot-based read/write first via the legacy `LHM_FIELD_*` indices,
// because (a) prior callers and the JIT may have populated those slots in
// stub-only configurations, and (b) it costs only one map lookup. When the
// stored value clearly isn't the synthetic value we expected (e.g. `size`
// reads back as Object(None) because slot 1 aliases AbstractMap.values), we
// fall through to the side-table.
//
// IMPORTANT: this is a per-object overlay; iteration helpers continue to walk
// the synthetic linked-list pointers, which now live in the side-table too.
use std::collections::HashMap as StdHashMap;
use std::sync::Mutex;
pub(crate) fn lhm_overlay() -> &'static Mutex<OverlayRows<StdHashMap<String, Value>>> {
    static OVERLAY: std::sync::OnceLock<Mutex<OverlayRows<StdHashMap<String, Value>>>> =
        std::sync::OnceLock::new();
    OVERLAY.get_or_init(|| Mutex::new(OverlayRows::new()))
}
/// Overlay keys whose structural `ObjectRef` fields (`head`/`tail`/`table`)
/// were successfully mirrored into the REAL JDK heap fields by `lhm_set`.
///
/// For such a LinkedHashMap, the backing bucket array + head/tail nodes are
/// reachable through the owner's normal heap fields, so the GC's field-tracing
/// scan already keeps them alive (for a LIVE owner) and relocates/updates them.
/// Re-rooting the overlay's *copies* of those refs is therefore redundant — and
/// actively harmful: it pins a DEAD LinkedHashMap's (and LinkedHashSet's, which
/// is LHM-backed) backing forever, because nothing else references it once the
/// owner dies. That unbounded pinning exhausted the young gen under high
/// collection churn (e.g. `WebXml.orderWebFragments` over 720 input
/// permutations). `gc_scan_collection_overlay_roots` consults this set and
/// skips rooting heap-backed entries; the remap path still repoints them.
/// `CRATONVM_LHM_ROOT_ALL=1` — measurement lever; see the call site in
/// [`for_each_overlay_ref`]. Cached, because the root scan runs on every GC.
pub(crate) fn lhm_root_all() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_LHM_ROOT_ALL").is_some())
}

pub(crate) fn lhm_heap_backed() -> &'static Mutex<std::collections::HashSet<usize>> {
    static HB: std::sync::OnceLock<Mutex<std::collections::HashSet<usize>>> =
        std::sync::OnceLock::new();
    HB.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}
// LHM overlay key: rekeyed onto `ctx.identity_hash_code(this)` (was
// `this.as_ptr() as usize`). A moving GC preserves the identity-hash
// word across relocation, so the overlay's bucket table, head/tail,
// and accessOrder flag remain reachable through the new ObjectRef.
pub(crate) fn lhm_overlay_key(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    widened_obj_key(ctx, this)
}
pub(crate) fn lhm_get(ctx: &dyn NativeContext, this: ObjectRef, name: &str, _fallback: usize) -> Value {
    // `head`/`tail`/`size`/`table` are mirrored to the real heap field on
    // EVERY overlay write (`lhm_set`), and a retired bytecode mutator
    // (`LinkedHashIterator.remove` -> `removeNode` -> `afterNodeRemoval`,
    // `HashMap` family, wave 9) writes the real field directly with no way to
    // reach the overlay at all. So once this map has been mirrored even
    // once, the real field can never be STALER than the overlay for these
    // four names -- it is the freshest answer regardless of which side wrote
    // last -- and the overlay must not be consulted first. See w9fix.py's
    // module doc for the measured symptom this fixes.
    //
    // PERF (gc-common w26-a): ONE key per read, computed before either table
    // lock. The four mirrored names used to key twice (once per table), each
    // time taking a registry shard and the process-global owner-index mutex
    // while holding a process-global table lock.
    //
    // JIT round 20 wave 17 (lane hashOff), correctness, unconditional: the
    // real field answers whenever the receiver HAS it, not only once this
    // map's key is in `lhm_heap_backed`. `lhm_set` mirrors these four names
    // on every write that can resolve the slot, so the real field is never
    // staler than the overlay on such a receiver; JDK bytecode (with the
    // `jdk-hash-collections` row off: `HashMap.clone` -> `putMapEntries`, a
    // subclass's bodies, `LinkedHashIterator.remove`, deserialization, any
    // JDK body the JIT runs on the map) writes ONLY the real field. Keyed on
    // the set, a map whose first structural write came from bytecode -- or
    // whose key left the set -- read a stale or absent overlay value first.
    // One source of truth for the structural state on a real layout: the
    // JDK's own fields.
    if matches!(name, "head" | "tail" | "size" | "table") {
        if let Some(slot) = ctx.resolve_field_index("java/util/LinkedHashMap", name) {
            if slot < ctx.object_num_fields(this) {
                return ctx.get_field(this, slot);
            }
        }
    }
    let key = lhm_overlay_key(ctx, this);

    {
        let m = lhm_overlay().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(v) = m.get(&key).and_then(|inner| inner.get(name)).copied() {
            return v;
        }
    }
    // Overlay MISS (entry never set): fall back to the real JDK heap field.
    // This is the path for a LinkedHashMap populated by REAL bytecode that
    // bypassed native_lhm_put — chiefly Java deserialization, whose
    // HashMap.readObject → putVal → LinkedHashMap.newNode writes the real
    // table + head/tail + node before/after but never our overlay. The
    // mirrored writes in lhm_set keep the heap consistent for native-populated
    // maps, so reading the heap here is correct for both cases (an explicitly
    // null overlay entry is returned above as Object(None) before reaching here).
    if matches!(name, "head" | "tail" | "size" | "table") {
        if let Some(slot) = ctx.resolve_field_index("java/util/LinkedHashMap", name) {
            if slot < ctx.object_num_fields(this) {
                return ctx.get_field(this, slot);
            }
        }
    }
    Value::Object(None)
}
pub(crate) fn lhm_set(ctx: &mut dyn NativeContext, this: ObjectRef, name: &str, _fallback: usize, v: Value) {
    let key = lhm_overlay_key(ctx, this);
    // gc-common w13-c: this used to record `this.as_ptr() -> key` in a
    // process-wide `lhm_ptr_cache` for the ctx-less `clone_lhm_overlay` (deleted). That
    // cache was never remapped when a map moved, so the address a live map
    // LEFT kept resolving to its key, and a clone allocated there wrote over
    // the moved map's overlay. It is gone, and with it a global mutex taken on
    // every overlay write. `Object.clone()` uses `clone_lhm_overlay_ctx`.
    let old = {
        let mut m = lhm_overlay().lock().unwrap_or_else(|e| e.into_inner());
        m.entry(key).or_default().insert(name.to_string(), v)
    };
    // `LinkedHashMap` kept its whole state in the overlay and never advanced
    // the JDK `modCount`, the way the HashMap family does through
    // `bump_map_mod_count`. MEASURED with `probes/MapModCountProbe2`, one
    // process per VM, against HotSpot 25.0.3+9:
    //
    // ```text
    //                  bumpOnPut  bumpOnRemove      HotSpot
    //   HashMap             YES        YES          YES YES
    //   LinkedHashMap       NO         NO           YES YES   <-- stuck at 0
    //   TreeMap             YES        YES          YES YES
    //   Hashtable           YES        YES          YES YES
    // ```
    //
    // That matters twice over. `modCount` is the JDK's fail-fast iterator
    // version, and it is also the invalidation generation any cached or lazily
    // materialised map view has to key on — a map whose generation never moves
    // cannot be cached safely, and `LinkedHashMap` is the source type in the
    // workload that fix exists for (the keySet-view rebuild elision this
    // generation now gates -- see `view_source_generation`).
    //
    // Bump on a SIZE write only, and here rather than at the three call sites,
    // for the same reason `set_map_size` is the HashMap family's choke point:
    // insert, removal and `clear` all route their size through this helper, so
    // a future mutator cannot forget it. A value-replacing `put` returns from
    // `native_lhm_put_evict` BEFORE the size write, so it does not bump — which
    // is what HotSpot does too (`put=5 remove=6 replace=6` in the probe's own
    // row), and is exactly the property a keySet-view cache needs: only a
    // change to the KEY SET may move the generation.
    if name == "size" && old != Some(v) {
        bump_map_mod_count(&*ctx, this);
    }
    // Mirror the structural pointers to the REAL JDK heap fields so that
    // real-bytecode paths that bypass our natives — chiefly Java serialization:
    // inherited `HashMap.writeObject` reads `size`/`table`, and
    // `LinkedHashMap.internalWriteEntries` walks the real `head`→`after` links —
    // observe the live state instead of the unset 0/null defaults (which
    // serialized a populated LHM as `{}`, desyncing peers). The overlay stays
    // the source of truth for reads; this keeps the heap consistent for the
    // rare real-bytecode readers. The LHM$Node `before`/`after` links are
    // already maintained on the real node fields by `lhm_alloc_node`/remove.
    if matches!(name, "head" | "tail" | "size" | "table") {
        if let Some(slot) = ctx.resolve_field_index("java/util/LinkedHashMap", name) {
            if slot < ctx.object_num_fields(this) {
                ctx.set_field(this, slot, v);
                // The structural fields resolve to real heap slots (real-JDK
                // layout), so every `ObjectRef` this overlay holds (`head`/
                // `tail`/`table`) is now reachable through the owner's heap
                // fields. Record the key so the GC root scan can skip rooting
                // the overlay copies — otherwise dead LinkedHashMaps/Sets leak
                // (their backing would stay pinned forever). Live owners keep
                // their backing alive via heap-field tracing.
                lhm_heap_backed()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key);
            }
        }
    }
}

/// Copy `src`'s LinkedHashMap overlay state onto its fresh clone `dst`, for
/// `Object.clone()` (the KC26 fix: `LinkedHashMap` and subclasses such as
/// Spring's `AnnotationAttributes` keep state outside the heap fields the
/// shallow copy covers).
///
/// Both keys resolve through `widened_obj_key`, so the clone's row belongs to
/// an object-key slot stamped with the calling VM. It is owner-indexed,
/// remapped when the clone moves, and pruned when the clone dies, like every
/// other overlay row.
///
/// **What is copied (gc-common w13-c).** Everything in the row EXCEPT the four
/// structural names `head`/`tail`/`size`/`table`, plus the source's
/// `lhm_heap_backed` membership. The structural state must not be shared.
/// `HashMap.clone()` is `super.clone()` (this door), then `reinitialize()`,
/// then `putMapEntries(this)`. The last two are real bytecode, which reset the
/// CLONE's heap fields and rebuild its table from the source. A clone that
/// took the source's `head` and `table` out of a copied row would read them
/// ahead of its own reset heap fields, and its inserts would link into the
/// source's nodes. With them left out, every structural read on the clone
/// falls through to its own heap fields, which is what it read before this
/// copy existed. What the clone does get is the overlay-only state:
/// `accessOrder` from the IFZ constructor, and `__capacity`.
///
/// A source with nothing to copy (no row, or only structural names) leaves
/// `dst` untouched: no row, and no object-key slot minted for it here.
pub fn clone_lhm_overlay_ctx(ctx: &dyn NativeContext, src: ObjectRef, dst: ObjectRef) {
    let src_key = lhm_overlay_key(ctx, src);
    // No table lock is held across `lhm_overlay_key` (it takes the object-key
    // shard and the owner index), and the two table locks are never held
    // together.
    let src_state = lhm_overlay()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&src_key)
        .cloned();
    let mut state = match src_state {
        Some(s) => s,
        None => return,
    };
    state.retain(|name, _| !matches!(name.as_str(), "head" | "tail" | "size" | "table"));
    if state.is_empty() {
        return;
    }
    // `CRATONVM_COMPAT_LHM_CLONE_FRESH_CAPACITY`: `HashMap.clone()` runs
    // `reinitialize()` (table null, threshold 0), so the clone's first table
    // is sized by its own `threshold` (`putMapEntries`, or 16 for an empty
    // source), never by the source's table. A copied `__capacity` made the
    // clone of an emptied, grown map allocate the source's size. Removed
    // AFTER the emptiness test, so which clones join the heap-backed set is
    // unchanged (an empty row reads as a miss).
    if lhm_clone_fresh_capacity_enabled() {
        state.remove("__capacity");
    }
    let src_heap_backed = lhm_heap_backed()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&src_key);
    let dst_key = lhm_overlay_key(ctx, dst);
    if dst_key == src_key {
        return;
    }
    lhm_overlay()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(dst_key, state);
    if src_heap_backed {
        lhm_heap_backed()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(dst_key);
    }
}

// Real JDK LinkedHashMap$Entry layout (extends HashMap$Node{hash,key,value,next}
// and adds before,after): hash@0, key@1, value@2, next@3, before@4, after@5.
// Verified with `javap -p --module java.base java.util.LinkedHashMap$Entry` on
// the JDK 25 the suites run, and `compute_field_layout` lays superclass fields
// out first, in declaration order — so these indices are the real ones.
// Using the real slot order (not the historical synthetic key@0/value@1/hash@2)
// is required so inherited real-JDK `HashMap.writeObject`→`internalWriteEntries`
// (Java serialization; NOT force-native) reads the right slots — the old order
// serialized `{k=v}` as `{v=null}`, desyncing peers. Nodes bind to the real
// LinkedHashMap$Entry class, and slot 0 = hash:I now holds an Int, so the
// descriptor-aware field writes/reads coincide with each slot's declared type.
pub(crate) const LHM_NODE_HASH: usize = 0;
pub(crate) const LHM_NODE_KEY: usize = 1;
pub(crate) const LHM_NODE_VALUE: usize = 2;
pub(crate) const LHM_NODE_NEXT: usize = 3;
pub(crate) const LHM_NODE_BEFORE: usize = 4;
pub(crate) const LHM_NODE_AFTER: usize = 5;
pub(crate) const LHM_NODE_NUM_FIELDS: usize = 6;

pub(crate) fn lhm_state(ctx: &dyn NativeContext, this: ObjectRef) -> (Option<ObjectRef>, i32, i32) {
    let buckets = match lhm_get(ctx, this, "table", LHM_FIELD_BUCKETS) {
        Value::Object(Some(arr)) => Some(arr),
        _ => None,
    };
    let size = match lhm_get(ctx, this, "size", LHM_FIELD_SIZE) {
        Value::Int(s) => s,
        _ => 0,
    };
    // Real-JDK HashMap has no explicit `capacity` field — derive from the
    // bucket array length when present. Falls back to the synthetic slot if
    // we couldn't get a buckets array (e.g. uninitialised stub).
    let cap = match buckets {
        Some(arr) => ctx.array_length(arr) as i32,
        None => match lhm_get(ctx, this, "__capacity", LHM_FIELD_CAPACITY) {
            Value::Int(c) => c,
            // A lazily constructed map: the size its first insert allocates
            // (`CRATONVM_COMPAT_LHM_CTOR_CAPACITY`).
            _ => lhm_pending_table_size(ctx, this),
        },
    };
    (buckets, size, cap)
}

pub(crate) fn lhm_alloc_node(
    ctx: &mut dyn NativeContext,
    key: Value,
    value: Value,
    hash: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    // gcstress residual face-1 fix — `alloc_synthetic` can trigger a moving
    // young GC that relocates the `key`/`value` object args; writing the bare
    // (stale) Value copies into the node below would store a dangling ref.
    // Pin+re-read them across the allocation.
    let key_pin = pin_value(ctx, key);
    let value_pin = pin_value(ctx, value);
    // The real JDK's nested node type, NOT the `java/util/LinkedHashMap$Node`
    // this used to mint. That name does not exist in the JDK, so every piece of
    // real-JDK bytecode that reached one of our nodes either threw
    // (`afterNodeRemoval`'s `(LinkedHashMap.Entry) e`, the
    // `kafka-embedded-kraft-...-FIXED` defect 1) or found no method at all
    // (`getKey()`/`getValue()`/`setValue()` are declared on the real
    // `HashMap$Node` and inherited, so on the fake class they were a
    // `NoSuchMethodError`). The slot layout below was already the real one, so
    // this is a change of class IDENTITY only — see `LHM_NODE_*`.
    let node = try_alloc_synthetic(ctx, "java/util/LinkedHashMap$Entry", LHM_NODE_NUM_FIELDS)?;
    let key = read_pinned_elem(ctx, key_pin, key);
    let value = read_pinned_elem(ctx, value_pin, value);
    if key_pin != usize::MAX {
        ctx.unpin_native_roots(key_pin);
    } else if value_pin != usize::MAX {
        ctx.unpin_native_roots(value_pin);
    }
    ctx.set_field(node, LHM_NODE_KEY, key);
    ctx.set_field(node, LHM_NODE_VALUE, value);
    ctx.set_field(node, LHM_NODE_HASH, Value::Int(hash));
    ctx.set_field(node, LHM_NODE_NEXT, Value::Object(None));
    ctx.set_field(node, LHM_NODE_BEFORE, Value::Object(None));
    ctx.set_field(node, LHM_NODE_AFTER, Value::Object(None));
    Ok(node)
}

pub(crate) fn lhm_link_tail(ctx: &mut dyn NativeContext, this: ObjectRef, node: ObjectRef) {
    let tail = lhm_get(ctx, this, "tail", LHM_FIELD_TAIL);
    if let Value::Object(Some(t)) = tail {
        ctx.set_field(t, LHM_NODE_AFTER, Value::Object(Some(node)));
        ctx.set_field(node, LHM_NODE_BEFORE, Value::Object(Some(t)));
    } else {
        // Empty list — node becomes head
        lhm_set(ctx, this, "head", LHM_FIELD_HEAD, Value::Object(Some(node)));
    }
    lhm_set(ctx, this, "tail", LHM_FIELD_TAIL, Value::Object(Some(node)));
}

/// The head-side mirror of [`lhm_link_tail`], for `putFirst`/`addFirst`.
pub(crate) fn lhm_link_head(ctx: &mut dyn NativeContext, this: ObjectRef, node: ObjectRef) {
    let head = lhm_get(ctx, this, "head", LHM_FIELD_HEAD);
    if let Value::Object(Some(h)) = head {
        ctx.set_field(h, LHM_NODE_BEFORE, Value::Object(Some(node)));
        ctx.set_field(node, LHM_NODE_AFTER, Value::Object(Some(h)));
    } else {
        // Empty list — node becomes tail as well.
        lhm_set(ctx, this, "tail", LHM_FIELD_TAIL, Value::Object(Some(node)));
    }
    lhm_set(ctx, this, "head", LHM_FIELD_HEAD, Value::Object(Some(node)));
}

/// Move an existing node to the FRONT of the encounter order.
///
/// The mirror of [`lhm_move_to_tail`], and structural for the same reason: it
/// changes the iteration order of every view over this map, so the mod count
/// moves with it.
pub(crate) fn lhm_move_to_head(ctx: &mut dyn NativeContext, this: ObjectRef, node: ObjectRef) {
    let current_head = lhm_get(ctx, this, "head", LHM_FIELD_HEAD);
    if let Value::Object(Some(h)) = current_head {
        if h.as_ptr() == node.as_ptr() {
            return;
        }
    }
    bump_map_mod_count(&*ctx, this);
    lhm_unlink(ctx, this, node);
    ctx.set_field(node, LHM_NODE_BEFORE, Value::Object(None));
    ctx.set_field(node, LHM_NODE_AFTER, Value::Object(None));
    lhm_link_head(ctx, this, node);
}

pub(crate) fn lhm_unlink(ctx: &mut dyn NativeContext, this: ObjectRef, node: ObjectRef) {
    let before = ctx.get_field(node, LHM_NODE_BEFORE);
    let after = ctx.get_field(node, LHM_NODE_AFTER);

    // Fix before's after
    if let Value::Object(Some(b)) = before {
        ctx.set_field(b, LHM_NODE_AFTER, after);
    } else {
        lhm_set(ctx, this, "head", LHM_FIELD_HEAD, after);
    }
    // Fix after's before
    if let Value::Object(Some(a)) = after {
        ctx.set_field(a, LHM_NODE_BEFORE, before);
    } else {
        lhm_set(ctx, this, "tail", LHM_FIELD_TAIL, before);
    }
}

/// `grow` is the table's allocation policy ([`GrowAlloc`]): `Reclaim` only
/// from the registered `LinkedHashMap.put` / `Map.put` doors (gc-common
/// w10-g), `SingleAttempt` from every internal caller of `native_lhm_put` and
/// from the direct Rust `Map.put` doors.
pub(crate) fn lhm_resize(ctx: &mut dyn NativeContext, this: ObjectRef, grow: GrowAlloc) {
    let (existing, size, cap) = lhm_state(ctx, this);
    // FIRST allocation vs GROWTH. With the constructor lazy (see
    // `lhm_init_with_cap_lazy`) the first insert arrives here with no table at
    // all, and it must allocate `cap` -- not `cap * 2`, which is what made a
    // freshly-filled `LinkedHashMap` report `len=32` against HotSpot's 16.
    let new_cap = if existing.is_none() {
        cap as usize
    } else {
        (cap as usize) * 2
    };
    // HIB-MAPRESIZE-STALE.1: `alloc_ref_array` can complete a moving young GC,
    // and `this` is a bare Rust local — the caller's `safe_native_call` pin
    // keeps the map ALIVE but does not rewrite this copy of its address. The
    // sibling `lhm_init_with_cap` and `map_resize_inner` were both given this
    // treatment; `lhm_resize` and `native_lhm_clear` were missed. Reading
    // `head` through the stale address then walks a freed object's slot as a
    // node chain and writes `LHM_NODE_NEXT` (slot 3) through every "node" it
    // finds — the `index=3 num_slots=0 class_id=ClassId(0)` dropped write in
    // the Hibernate `--nojit` SIGSEGV — and finally publishes those addresses
    // into `new_buckets`, corrupting the map for good.
    // TYPED, not `alloc_ref_array`. That helper hard-codes the
    // `[Ljava.lang.Object;` sentinel, and this is the same species `H23` fixed
    // for `HashMap`'s table and `WORKER-2-NOTE-1` §2 for `Hashtable`'s: an
    // allocation on the path that never asked for a component type.
    //
    // It was a LATENT defect, not one this lane introduced -- `lhm_init_with_cap`
    // allocated the typed array up front, so `lhm_resize` only ran on GROWTH and
    // a grown `LinkedHashMap` silently reverted to `Object[]`. Making the
    // constructor lazy routes the FIRST allocation through here too, which is
    // what finally exposed it: MEASURED `[Ljava.lang.Object; len=32` against
    // HotSpot's `[Ljava.util.HashMap$Node; len=16`.
    //
    // `LinkedHashMap` inherits `HashMap`'s `table`, so the component is
    // `HashMap$Node` even though its nodes are `LinkedHashMap$Entry` -- a
    // SUBCLASS, which stores fine. `bucket_table_component` already encodes
    // that; it can load a class and therefore collect, so it runs inside the
    // same rooted region as the allocation.
    // Pinned explicitly rather than through `rooted_across1`: the component
    // lookup needs `this`, which that helper's closure cannot see. Same
    // discipline, same guarantee -- `this` is re-read after each collection
    // point, and `component` is a `ClassId`, which no collection moves.
    let this_pin = ctx.pin_native_root(this);
    let component = bucket_table_component(ctx, this);
    let this = ctx.read_native_pin(this_pin, this);
    // gc-common w10-g, `live` audit for `GrowAlloc::Reclaim`: `this` is pinned
    // above and re-read below; `component` is a `ClassId`; `existing` is not
    // read again (the walk below starts from `this`'s head). The caller,
    // `native_lhm_put_evict`, re-reads its receiver, key and value from its
    // own pins after this returns.
    let new_buckets = new_ref_array_grow(ctx, component, new_cap, grow);
    let this = ctx.read_native_pin(this_pin, this);

    // `CRATONVM_COMPAT_LHM_TREE_BINS`: a table holding tree bins is split with
    // the JDK's `TreeNode.split` for each of them, which runs bytecode, so
    // everything is re-read from pins across it ([`lhm_split_tree_table`]).
    let old_tab = lhm_state(ctx, this).0;
    let mut has_trees = false;
    if let Some(old) = old_tab {
        if lhm_tree_family(&*ctx, this) {
            for i in 0..ctx.array_length(old) {
                let head = ctx.get_array_element(old, i);
                if hm_tree_head(&*ctx, head).is_some() {
                    has_trees = true;
                    break;
                }
            }
        }
    }
    let bucket_order = lhm_bucket_order_enabled();
    let (this, new_buckets, split_done) = match old_tab {
        Some(old) if has_trees => {
            let nb_pin = ctx.pin_native_root(new_buckets);
            let done = bucket_order
                && lhm_split_tree_table(ctx, this_pin, this, old, nb_pin, new_buckets, size);
            if !done {
                // The list rebuild below relinks by `next` only, which a tree
                // cannot survive: every tree bin becomes a chain first.
                let this_now = ctx.read_native_pin(this_pin, this);
                if let Some(old_now) = lhm_state(ctx, this_now).0 {
                    for i in 0..ctx.array_length(old_now) {
                        let this_now = ctx.read_native_pin(this_pin, this);
                        if let Some(old_now) = lhm_state(ctx, this_now).0 {
                            let _ = untreeify_bin_at(ctx, this_now, old_now, i);
                        }
                    }
                }
            }
            let this = ctx.read_native_pin(this_pin, this);
            let new_buckets = ctx.read_native_pin(nb_pin, new_buckets);
            (this, new_buckets, done)
        }
        _ => (this, new_buckets, false),
    };
    ctx.unpin_native_roots(this_pin);

    // No allocation and no Java dispatch happens below, so one refresh of
    // `this` covers the whole rehash.
    //
    // `CRATONVM_COMPAT_LHM_BUCKET_ORDER`: split each OLD chain in order, as
    // `HashMap.resize()` does, so a bucket keeps its nodes in the order `put`
    // linked them. `lhm_state` re-reads the old table from the refreshed `this`
    // (the `existing` above predates the allocation).
    let split_done = split_done
        || (!has_trees
            && bucket_order
            && match lhm_state(ctx, this).0 {
                Some(old) => lhm_rehash_by_split(ctx, old, new_buckets, size),
                None => false,
            });
    if !split_done {
        // Walk the insertion-order list and rehash: the legacy rebuild, and
        // the fallback when the old chains do not account for exactly `size`
        // nodes. Under the switch each node is appended to its new bucket's
        // tail, otherwise pushed on its head (which reversed every chain).
        let walk_cap = chain_walk_limit(size);
        let mut cur = lhm_get(ctx, this, "head", LHM_FIELD_HEAD);
        while let Value::Object(Some(node)) = cur {
            let hash = match ctx.get_field(node, LHM_NODE_HASH) {
                Value::Int(h) => h,
                _ => 0,
            };
            let idx = map_bucket_index(hash, new_cap as i32);

            // Detach from old bucket chain
            ctx.set_field(node, LHM_NODE_NEXT, Value::Object(None));
            lhm_link_into_bucket(ctx, new_buckets, idx, node, bucket_order, walk_cap);

            cur = ctx.get_field(node, LHM_NODE_AFTER);
        }
    }

    lhm_set(
        ctx,
        this,
        "table",
        LHM_FIELD_BUCKETS,
        Value::Object(Some(new_buckets)),
    );
    lhm_set(ctx, this, "size", LHM_FIELD_SIZE, Value::Int(size));
    lhm_set(
        ctx,
        this,
        "__capacity",
        LHM_FIELD_CAPACITY,
        Value::Int(new_cap as i32),
    );
    // `CRATONVM_COMPAT_LHM_LOAD_FACTOR`: a custom factor's `threshold` is
    // maintained as `resize()` maintains it (read by `lhm_grow_threshold`).
    // Before the first table the field held the pending size, so the first
    // table recomputes. Field reads and one int store: nothing moves.
    if lhm_load_factor_enabled() {
        if let Some(lf) = map_custom_load_factor(&*ctx, this) {
            let (old_cap, old_thr) = if existing.is_some() {
                (cap, map_threshold_field(&*ctx, this).unwrap_or(0))
            } else {
                (0, 0)
            };
            let thr = jdk_hashmap_next_threshold(old_cap, old_thr, new_cap as i32, lf);
            try_set_jdk_map_field(ctx, this, "threshold", Value::Int(thr));
        }
    }
}

/// `CRATONVM_COMPAT_LHM_BUCKET_ORDER` — default ON, `=0` restores the old
/// chains. `LinkedHashMap` inherits `HashMap.putVal`, which APPENDS a new node
/// to its bucket, and `HashMap.resize()`, which splits each chain keeping its
/// order. The native `put` pushed on the bucket head and `lhm_resize` rebuilt
/// every bucket by head-pushing the insertion-order list, so each chain ran
/// newest-first: a lookup called `equals` on a different set of stored keys,
/// in a different order, than HotSpot (observable through a side-effecting or
/// non-transitive `equals`). Round 13 wave 8, lane hashcompat3
/// (`R13FramestateHashMapCounts`: `bad` on the two `LinkedHashMap` arms).
pub(crate) fn lhm_bucket_order_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_LHM_BUCKET_ORDER")
}

/// Link `node` (its `next` already null) into bucket `idx`: at the chain's
/// TAIL when `append` (`HashMap.putVal`), else at its head (the legacy
/// order). A chain longer than `walk_cap` (a cycle) takes the head, as before.
/// Reference stores only: no Java and no allocation, so nothing moves.
pub(crate) fn lhm_link_into_bucket(
    ctx: &mut dyn NativeContext,
    buckets: ObjectRef,
    idx: usize,
    node: ObjectRef,
    append: bool,
    walk_cap: usize,
) {
    let head = ctx.get_array_element(buckets, idx);
    if append {
        let mut tail: Option<ObjectRef> = None;
        let mut cur = head;
        let mut steps: usize = 0;
        let mut cyclic = false;
        while let Value::Object(Some(n)) = cur {
            steps += 1;
            if steps > walk_cap {
                cyclic = true;
                break;
            }
            tail = Some(n);
            cur = ctx.get_field(n, LHM_NODE_NEXT);
        }
        if !cyclic {
            match tail {
                Some(t) => ctx.set_field(t, LHM_NODE_NEXT, Value::Object(Some(node))),
                None => ctx.set_array_element(buckets, idx, Value::Object(Some(node))),
            }
            return;
        }
    }
    if let Value::Object(Some(h)) = head {
        ctx.set_field(node, LHM_NODE_NEXT, Value::Object(Some(h)));
    }
    ctx.set_array_element(buckets, idx, Value::Object(Some(node)));
}

/// A `LinkedHashMap$Entry`'s stored `hash`.
pub(crate) fn lhm_node_hash(ctx: &dyn NativeContext, node: ObjectRef) -> i32 {
    match ctx.get_field(node, LHM_NODE_HASH) {
        Value::Int(h) => h,
        _ => 0,
    }
}

/// `HashMap.resize()`'s split for a `LinkedHashMap` table that doubles:
/// each old chain `i`, walked in order, goes to the tails of new buckets `i`
/// and `i + oldCap` (under `map_bucket_index`'s `(h & 0x7fffffff) % len`, a
/// doubled length can send a node nowhere else, power of two or not).
///
/// A first pass proves the old table holds exactly `size` nodes, every chain
/// ends, and every node lands in one of its two buckets; if not, it answers
/// `false` having written nothing, and the caller rebuilds from the
/// insertion-order list instead. Reference stores only: no Java, no
/// allocation.
pub(crate) fn lhm_rehash_by_split(
    ctx: &mut dyn NativeContext,
    old: ObjectRef,
    new_buckets: ObjectRef,
    size: i32,
) -> bool {
    let old_cap = ctx.array_length(old);
    let new_cap = ctx.array_length(new_buckets);
    if old_cap == 0 || new_cap != old_cap.saturating_mul(2) || size < 0 {
        return false;
    }
    let new_len = new_cap as i32;
    let mut count: usize = 0;
    for i in 0..old_cap {
        let mut cur = ctx.get_array_element(old, i);
        while let Value::Object(Some(n)) = cur {
            count += 1;
            if count > size as usize {
                return false;
            }
            let to = map_bucket_index(lhm_node_hash(&*ctx, n), new_len);
            if to != i && to != i + old_cap {
                return false;
            }
            cur = ctx.get_field(n, LHM_NODE_NEXT);
        }
    }
    if count != size as usize {
        return false;
    }
    for i in 0..old_cap {
        lhm_split_chain(ctx, old, new_buckets, i, old_cap, new_len);
    }
    true
}

/// One old chain `i` of [`lhm_rehash_by_split`], in order, to the tails of new
/// buckets `i` and `i + old_cap`. Reference stores only.
pub(crate) fn lhm_split_chain(
    ctx: &mut dyn NativeContext,
    old: ObjectRef,
    new_buckets: ObjectRef,
    i: usize,
    old_cap: usize,
    new_len: i32,
) {
    let mut lo: (Option<ObjectRef>, Option<ObjectRef>) = (None, None);
    let mut hi: (Option<ObjectRef>, Option<ObjectRef>) = (None, None);
    let mut cur = ctx.get_array_element(old, i);
    while let Value::Object(Some(n)) = cur {
        let next = ctx.get_field(n, LHM_NODE_NEXT);
        ctx.set_field(n, LHM_NODE_NEXT, Value::Object(None));
        let half = if map_bucket_index(lhm_node_hash(&*ctx, n), new_len) == i {
            &mut lo
        } else {
            &mut hi
        };
        match half.1 {
            Some(t) => ctx.set_field(t, LHM_NODE_NEXT, Value::Object(Some(n))),
            None => half.0 = Some(n),
        }
        half.1 = Some(n);
        cur = next;
    }
    if let Some(h) = lo.0 {
        ctx.set_array_element(new_buckets, i, Value::Object(Some(h)));
    }
    if let Some(h) = hi.0 {
        ctx.set_array_element(new_buckets, i + old_cap, Value::Object(Some(h)));
    }
}

/// [`lhm_rehash_by_split`] for a table holding tree bins
/// (`CRATONVM_COMPAT_LHM_TREE_BINS`): the same first pass, then each old bin
/// split as `resize()` splits it -- a tree bin of two nodes or more by the
/// JDK's `TreeNode.split(this, newTab, j, oldCap)` (which re-treeifies or
/// untreeifies each half, `LinkedHashMap.replacementNode` keeping the list),
/// every other bin by [`lhm_split_chain`]. The split runs bytecode, so `this`
/// (rooted at `this_pin`), the new table (at `nb_pin`) and the old one are
/// re-read around it. `false`, having written nothing, when the first pass
/// refuses or the table is not a power of two.
#[allow(clippy::too_many_arguments)]
pub(crate) fn lhm_split_tree_table(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
    old: ObjectRef,
    nb_pin: usize,
    new_buckets: ObjectRef,
    size: i32,
) -> bool {
    let old_cap = ctx.array_length(old);
    let new_cap = ctx.array_length(new_buckets);
    if old_cap == 0
        || !old_cap.is_power_of_two()
        || new_cap != old_cap.saturating_mul(2)
        || size < 0
    {
        return false;
    }
    let new_len = new_cap as i32;
    let mut count: usize = 0;
    for i in 0..old_cap {
        let mut cur = ctx.get_array_element(old, i);
        while let Value::Object(Some(n)) = cur {
            count += 1;
            if count > size as usize {
                return false;
            }
            let to = map_bucket_index(lhm_node_hash(&*ctx, n), new_len);
            if to != i && to != i + old_cap {
                return false;
            }
            cur = ctx.get_field(n, LHM_NODE_NEXT);
        }
    }
    if count != size as usize {
        return false;
    }
    let old_pin = ctx.pin_native_root(old);
    for i in 0..old_cap {
        let old_now = ctx.read_native_pin(old_pin, old);
        let head = ctx.get_array_element(old_now, i);
        if let Some(first) = hm_tree_head(&*ctx, head) {
            if !matches!(ctx.get_field(first, LHM_NODE_NEXT), Value::Object(None)) {
                let this_now = ctx.read_native_pin(this_pin, this);
                let nb = ctx.read_native_pin(nb_pin, new_buckets);
                let _ = ctx.invoke_special_bytecode_only(
                    HM_TREE_NODE_CLASS,
                    "split",
                    HM_SPLIT_DESC,
                    &[
                        Value::Object(Some(first)),
                        Value::Object(Some(this_now)),
                        Value::Object(Some(nb)),
                        Value::Int(i as i32),
                        Value::Int(old_cap as i32),
                    ],
                );
                continue;
            }
        }
        let nb = ctx.read_native_pin(nb_pin, new_buckets);
        lhm_split_chain(ctx, old_now, nb, i, old_cap, new_len);
    }
    ctx.unpin_native_roots(old_pin);
    true
}

pub(crate) fn lhm_find_node(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key: &Value,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    lhm_find_node_hashed(ctx, this, key, None)
}

/// [`lhm_find_node`] with the key's [`map_hash_key`] already in hand, so the
/// probe does not call `hashCode()` a second time. The JDK's `putVal` hashes a
/// key once per `put`; a caller that hashed to size or place the entry passes
/// that hash here (round 13 wave 7, `CRATONVM_COMPAT_LHM_SINGLE_HASH`).
pub(crate) fn lhm_find_node_hashed(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key: &Value,
    known_hash: Option<i32>,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    lhm_find_node_tree(ctx, this, key, known_hash, TreeLookup::AfterFirst)
}

/// [`lhm_find_node_hashed`] with the tree-bin search spelled out
/// (`CRATONVM_COMPAT_LHM_TREE_BINS`, see [`map_chain_find_from`]'s `tree`):
/// [`TreeLookup::AfterFirst`] is `getNode`'s, [`TreeLookup::Direct`] the
/// compute family's. A receiver outside [`lhm_tree_family`] walks `next`.
pub(crate) fn lhm_find_node_tree(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key: &Value,
    known_hash: Option<i32>,
    tree: TreeLookup,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let tree = if lhm_tree_family(&*ctx, this) {
        tree
    } else {
        TreeLookup::Off
    };
    // GC-safety: same "Family 1" stale-ObjectRef pattern fixed in
    // `native_map_contains_key` (see its comment for the live
    // CRATONVM_DBG_STALE_OBJREF capture this mirrors) -- `map_hash_key`/
    // `map_keys_equal` dispatch arbitrary Java code (hashCode()/equals()),
    // which can trigger a moving GC. Pin `this` and the search key up front,
    // and pin/re-read the bucket-chain `node` around each `equals()` call.
    let this_pin = ctx.pin_native_root(this);
    let key_val = *key;
    let key_pin = pin_value(ctx, key_val);
    let (hash, is_null, key_ref) = match key_val {
        Value::Object(Some(k)) => {
            let hash = match known_hash {
                Some(hash) => hash,
                None => map_hash_key(ctx, k)?,
            };
            (hash, false, Some(k))
        }
        Value::Object(None) => (0, true, None),
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Ok(None);
        }
    };
    let this = ctx.read_native_pin(this_pin, this);
    let key_val = read_pinned_elem(ctx, key_pin, key_val);

    let (buckets, _, cap) = lhm_state(ctx, this);
    let buckets = match buckets {
        Some(b) => b,
        None => {
            ctx.unpin_native_roots(this_pin);
            return Ok(None);
        }
    };
    let idx = map_bucket_index(hash, cap);
    let mut node_val = ctx.get_array_element(buckets, idx);
    // A tree bin: `getTreeNode` straight away (`Direct`), or after the first
    // node did not match (`AfterFirst`), as in `map_chain_find_from`.
    let first_tree = if tree == TreeLookup::Off {
        None
    } else {
        hm_tree_head(&*ctx, node_val)
    };
    if let (Some(first), TreeLookup::Direct) = (first_tree, tree) {
        let found = hm_tree_get_node(ctx, first, hash, key_val);
        ctx.unpin_native_roots(this_pin);
        return found;
    }
    let mut walk_count: usize = 0;
    while let Value::Object(Some(mut node)) = node_val {
        walk_count += 1;
        if first_tree.is_some() && walk_count == 2 && is_hm_tree_node(&*ctx, node) {
            let key_now = read_pinned_elem(ctx, key_pin, key_val);
            let found = hm_tree_get_node(ctx, node, hash, key_now);
            ctx.unpin_native_roots(this_pin);
            return found;
        }
        let node_hash = match ctx.get_field(node, LHM_NODE_HASH) {
            Value::Int(node_hash) => node_hash,
            _ => 0,
        };
        let node_key = ctx.get_field(node, LHM_NODE_KEY);
        if is_null {
            if matches!(node_key, Value::Object(None)) {
                ctx.unpin_native_roots(this_pin);
                return Ok(Some(node));
            }
        } else if node_hash == hash {
            let Value::Object(Some(nk)) = node_key else {
                node_val = ctx.get_field(node, LHM_NODE_NEXT);
                continue;
            };
            let node_pin = ctx.pin_native_root(node);
            let node_key_pin = ctx.pin_native_root(nk);
            let nk = ctx.read_native_pin(node_key_pin, nk);
            let k_cur = match read_pinned_elem(ctx, key_pin, key_val) {
                Value::Object(Some(kk)) => kk,
                _ => key_ref.unwrap(),
            };
            let eq = map_keys_equal(ctx, k_cur, nk)?;
            node = ctx.read_native_pin(node_pin, node);
            ctx.unpin_native_roots(node_key_pin);
            ctx.unpin_native_roots(node_pin);
            if eq {
                ctx.unpin_native_roots(this_pin);
                return Ok(Some(node));
            }
        }
        node_val = ctx.get_field(node, LHM_NODE_NEXT);
    }
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

/// Allocate + initialise a fresh synthetic `java/util/LinkedHashMap`.
/// Mirrors `alloc_backing_map` but produces an insertion-ordered LinkedHashMap
/// (so `collect_entries_any` / iteration honour put order).
pub(crate) fn alloc_linked_hash_map(ctx: &mut dyn NativeContext) -> ObjectRef {
    let cid = match ctx.ensure_class_initialized("java/util/LinkedHashMap") {
        Ok(id) => id,
        Err(_) => ctx
            .class_id_by_name("java/util/LinkedHashMap")
            .unwrap_or(ClassId::new(0)),
    };
    let total = ctx.class_num_total_fields(cid);
    let n = std::cmp::max(total, MAP_NUM_FIELDS);
    let m = ctx.alloc_object(cid, n);
    // JVMS §2.3 defaults — see [`init_native_map_reference_defaults`]. The
    // initializer below is EAGER, so `table` is overwritten with a real array
    // one line later; `keySet`/`values`/`entrySet` are the slots this rescues.
    init_native_map_reference_defaults(ctx, m);
    lhm_init_with_cap(ctx, m, MAP_DEFAULT_CAPACITY);
    m
}

/// Build a NEW `LinkedHashMap` holding `source`'s entries in REVERSE encounter
/// order — the backing for `SequencedMap.reversed()` on synthetic LinkedHashMap
/// and unmodifiable-map wrappers (Java 21 `SequencedMap`).
///
/// This is a snapshot (later mutations of `source` do not propagate), which is
/// sufficient for the read/iterate use that dominates `reversed()` callers
/// (Log4j2 / Elasticsearch config walking). A fully live reverse-view would need
/// a dedicated synthetic view class; this keeps the common case correct without
/// `NoSuchMethodError` / an empty result.
/// The overlay key a reversed map keeps its SOURCE under, and the one it keeps
/// the source generation it was last rebuilt from under.
///
/// The overlay rather than a heap field or a new side table: `lhm_set` writes
/// name-keyed entries there and nowhere else, and the overlay is already one of
/// the tables wired into all four collector hooks -- so a source held here is
/// rooted, remapped and pruned for free, which a fresh `HashMap<usize,
/// ObjectRef>` would not be.
pub(crate) const LHM_REVERSED_SOURCE: &str = "__reversed_source";
pub(crate) const LHM_REVERSED_GENERATION: &str = "__reversed_generation";
/// The `(key, value)` identities a reversed view was last rebuilt from; see
/// [`lhm_reversed_sees_overwrites_enabled`].
pub(crate) const LHM_REVERSED_FINGERPRINT: &str = "__reversed_fingerprint";

pub(crate) fn build_reversed_map_snapshot(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    let m = alloc_linked_hash_map(ctx);
    rebuild_reversed_from(ctx, m, source)
}

/// Fill `view` with `source`'s entries in reverse encounter order and record
/// what it was built from.
pub(crate) fn rebuild_reversed_from(
    ctx: &mut dyn NativeContext,
    view: ObjectRef,
    source: ObjectRef,
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    // GC: `collect_entries_any`, `native_lhm_clear` and `native_lhm_put` each
    // dispatch Java and allocate, so the view, the source and every collected
    // key/value can move under them. The view is the outermost pin and
    // releasing it releases the rest.
    let view_pin = ctx.pin_native_root(view);
    let src_pin = ctx.pin_native_root(source);
    let generation = lhm_source_generation(&*ctx, source);
    let mut entries = collect_entries_any(ctx, source)?;
    entries.reverse();
    // `CRATONVM_COMPAT_LHM_REVERSED_BY_NODE`: a `LinkedHashMap` source's
    // entries are distinct keys with stored hashes, so the view is filled
    // from those (`lhm_append_distinct`) and no key's `hashCode()` or
    // `equals()` runs -- the JDK's reversed map is a view and never calls
    // either. `lhm_collect_hashes` walks the same list `collect_entries_any`
    // just read, with no Java in between.
    let mut hashes = {
        let source_now = ctx.read_native_pin(src_pin, source);
        if lhm_reversed_by_node_enabled()
            && is_lhm_receiver(&*ctx, source_now)
            && entries.iter().all(|(_, v)| matches!(v, Value::Object(_)))
        {
            lhm_collect_hashes(&*ctx, source_now)
        } else {
            Vec::new()
        }
    };
    hashes.reverse();
    let by_hash = !hashes.is_empty() && hashes.len() == entries.len();
    let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (_, handles) = pin_value_slice(ctx, &flat);
    let view_now = ctx.read_native_pin(view_pin, view);
    native_lhm_clear(ctx, &[Value::Object(Some(view_now))])?;
    for i in 0..entries.len() {
        let k = read_pinned_elem(ctx, handles[2 * i], flat[2 * i]);
        let v = read_pinned_elem(ctx, handles[2 * i + 1], flat[2 * i + 1]);
        let view_now = ctx.read_native_pin(view_pin, view);
        if by_hash {
            lhm_append_distinct(ctx, view_now, k, v, hashes[i])?;
        } else {
            native_lhm_put(ctx, &[Value::Object(Some(view_now)), k, v])?;
        }
    }
    let view_now = ctx.read_native_pin(view_pin, view);
    let source_now = ctx.read_native_pin(src_pin, source);
    // AFTER the clear, which drops the view's overlay entry along with its
    // contents -- writing the marker before it would lose it.
    lhm_set(
        ctx,
        view_now,
        LHM_REVERSED_SOURCE,
        0,
        Value::Object(Some(source_now)),
    );
    lhm_set(
        ctx,
        view_now,
        LHM_REVERSED_GENERATION,
        0,
        Value::Int(generation),
    );
    // `CRATONVM_COMPAT_LHM_REVERSED_SEES_OVERWRITES`: what the view was built
    // from, `[k, v, k, v, ..]` in the VIEW's (reversed) order, so a later read
    // can tell a value overwrite in the source (which moves neither its size
    // nor its `modCount`) from no change at all. See
    // `reversed_fingerprint_matches`.
    let source_now = ctx.read_native_pin(src_pin, source);
    if lhm_reversed_sees_overwrites_enabled() && is_lhm_receiver(&*ctx, source_now) {
        let arr = alloc_ref_array(ctx, flat.len());
        let arr_pin = ctx.pin_native_root(arr);
        for (i, v) in flat.iter().enumerate() {
            let v = read_pinned_elem(ctx, handles[i], *v);
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_array_element(arr, i, v);
        }
        let arr = ctx.read_native_pin(arr_pin, arr);
        let view_now = ctx.read_native_pin(view_pin, view);
        lhm_set(
            ctx,
            view_now,
            LHM_REVERSED_FINGERPRINT,
            0,
            Value::Object(Some(arr)),
        );
    }
    let view_now = ctx.read_native_pin(view_pin, view);
    ctx.unpin_native_roots(view_pin);
    Ok(view_now)
}

/// `CRATONVM_COMPAT_LHM_REVERSED_SEES_OVERWRITES` — default ON, `=0`
/// restores the size-only watch. A `LinkedHashMap.reversed()` view is rebuilt
/// when its source's SIZE moves ([`lhm_source_generation`]), so a value
/// OVERWRITE in the source (`put` of a present key, `replace`,
/// `Entry.setValue`, a compute-family update) and a balanced `remove` + `put`
/// were never seen by a view taken earlier; the JDK's view reads the live
/// nodes. The view now also keeps the `(key, value)` identities it was built
/// from and is rebuilt when the source's differ -- an O(n) walk of pure field
/// reads per read of the view (only a reversed view pays it). A write made
/// through the view itself (which does not reach the source here, see the
/// residual page) is left alone while the source is unchanged, as before.
/// Round 13 wave 10, lane compat5 (residual page item 3).
pub(crate) fn lhm_reversed_sees_overwrites_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_LHM_REVERSED_SEES_OVERWRITES")
}

/// Whether a reversed `view` still shows its `LinkedHashMap` `source`: the
/// fingerprint [`rebuild_reversed_from`] recorded holds, in reverse order,
/// the very key and value objects the source holds now. `true` (trust the
/// size watch, the old behaviour) when there is no fingerprint or the source
/// is not a `LinkedHashMap`. Pure reads.
pub(crate) fn reversed_fingerprint_matches(
    ctx: &dyn NativeContext,
    view: ObjectRef,
    source: ObjectRef,
) -> bool {
    if !lhm_reversed_sees_overwrites_enabled() || !is_lhm_receiver(ctx, source) {
        return true;
    }
    let Value::Object(Some(arr)) = lhm_get(ctx, view, LHM_REVERSED_FINGERPRINT, 0) else {
        return true;
    };
    let keys = lhm_collect_keys(ctx, source);
    let vals = lhm_collect_values(ctx, source);
    let n = keys.len();
    if vals.len() != n || ctx.array_length(arr) != 2 * n {
        return false;
    }
    (0..n).all(|i| {
        let j = n - 1 - i;
        value_same_ref(ctx.get_array_element(arr, 2 * i), keys[j])
            && value_same_ref(ctx.get_array_element(arr, 2 * i + 1), vals[j])
    })
}

/// What a reversed view watches its source for: the source's own size.
///
/// Size and not `modCount`, for the reason `family_snapshot_generation` records
/// -- `LinkedHashMap` kept its whole state in the overlay and did not advance
/// the JDK `modCount` until recently, and size is available for every map this
/// can be taken over. It can only MISS (a balanced put+remove between two
/// reads), never rebuild spuriously, which is the safe direction: the cost of a
/// miss is the staleness this whole mechanism is removing, and the cost of a
/// false positive would be an O(n) rebuild on every read.
pub(crate) fn lhm_source_generation(ctx: &dyn NativeContext, source: ObjectRef) -> i32 {
    if is_lhm_receiver(ctx, source) {
        return lhm_state(ctx, source).1;
    }
    if is_tree_map_receiver(ctx, source) {
        return tm_state(ctx, source).1;
    }
    map_state(ctx, source).1
}

/// `SequencedMap.reversed()`'s LIVE half: rebuild the view from its source when
/// the source has moved since the last rebuild.
///
/// Returns the receiver unchanged for any map that is not a reversed view, so
/// every call site is one line and costs one overlay lookup on an ordinary
/// `LinkedHashMap`.
///
/// MEASURED (`apps/probes/LinkedSequencedShadowSweep` row 92): a `put` into the
/// source after `reversed()` was taken was invisible --
/// `{c=3, a=1, b=2, d=4}` where HotSpot answers `{e=5, c=3, a=1, b=2, d=4}`,
/// because the JDK's is a `ReverseOrderLinkedHashMapView` over the live map and
/// this VM built a snapshot. The snapshot stays -- this VM has no real
/// `head`/`tail`/`before`/`after` chain for the JDK's view bytecode to walk --
/// but it is now rebuilt on read, which is observationally the same thing for
/// everything a caller can ask.
pub(crate) fn resync_reversed_map(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    let source = match lhm_get(&*ctx, this, LHM_REVERSED_SOURCE, 0) {
        Value::Object(Some(s)) => s,
        _ => return Ok(this),
    };
    let seen = match lhm_get(&*ctx, this, LHM_REVERSED_GENERATION, 0) {
        Value::Int(g) => g,
        _ => i32::MIN,
    };
    let generation = lhm_source_generation(&*ctx, source);
    if generation == seen && reversed_fingerprint_matches(&*ctx, this, source) {
        return Ok(this);
    }
    // STAMP BEFORE REBUILDING. The rebuild calls `native_lhm_clear` and
    // `native_lhm_put`, and a read native reached from either would land back
    // here; with the old generation still recorded it would decide to rebuild
    // again, and so on. The clear also drops the source marker, which stops the
    // recursion a second way -- but relying on the ORDER of two side effects
    // for termination is the kind of thing that holds until someone reorders
    // them, so the stamp is explicit.
    lhm_set(
        ctx,
        this,
        LHM_REVERSED_GENERATION,
        0,
        Value::Int(generation),
    );
    rebuild_reversed_from(ctx, this, source)
}

/// [`resync_reversed_map`] for a read native that holds argument values across
/// it: pins `vals` across the resync and rewrites each one in place.
///
/// gen r4w4/rooting2: a stale rebuild re-puts every entry (the keys'
/// `hashCode()`/`equals()`, a node allocation per entry), so it is a GC point,
/// and the natives that called `resync_reversed_map` then read their key /
/// default / target out of `args` -- which the native funnel never rewrites.
/// A receiver that is not a reversed view (the common case) is answered by the
/// same overlay read `resync_reversed_map` opens with, and pins nothing.
pub(crate) fn resync_reversed_map_carrying(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    vals: &mut [Value],
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    if !matches!(
        lhm_get(&*ctx, this, LHM_REVERSED_SOURCE, 0),
        Value::Object(Some(_))
    ) {
        return Ok(this);
    }
    let base = ctx.pin_native_root(this);
    let (_, handles) = pin_value_slice(ctx, vals);
    let out = resync_reversed_map(ctx, this);
    for (v, h) in vals.iter_mut().zip(&handles) {
        *v = read_pinned_elem(ctx, *h, *v);
    }
    ctx.unpin_native_roots(base);
    out
}

/// Is `LinkedHashMap.reversed()` left to its real bytecode (the JDK's live
/// `ReversedLinkedHashMapView`) instead of the rebuilt snapshot map
/// ([`build_reversed_map_snapshot`] + [`resync_reversed_map`])?
///
/// Round 14 wave 4 (lane compat4), proposal C3-2 of
/// `jit-r14-compat3-proposals-RETIRED-20260930.md`, item 3 of
/// `r13w8-hashcompat3-compatible-collection-residuals-CLOSED-20260930.md`: a write
/// THROUGH the snapshot (`put`, `remove`, `putFirst`, ...) never reached the
/// source, and its class was `LinkedHashMap`. The view's members, JDK 25 source,
/// member by member:
///
/// * `size`, `isEmpty`, `containsKey`, `containsValue`, `get`, `put`,
///   `remove`, `putAll`, `clear`, `getOrDefault`, `putIfAbsent`,
///   `remove(k, v)`, both `replace`s, the compute family, `putFirst` /
///   `putLast` (swapped), `pollFirstEntry` / `pollLastEntry` (swapped),
///   `equals`, `hashCode`: `base.<same>` -- the natives registered on the
///   source, live and write-through;
/// * `keySet()` / `entrySet()`: `base.sequenced{Key,Entry}Set().reversed()` --
///   the native carrier and [`native_view_reversed`], which since C3-1 answers
///   a live reversed `SequencedSet` for both (the entry-set row answered an
///   `ArrayList`, and `AbstractMap.toString`'s `entrySet().iterator()` would
///   have died with `IncompatibleClassChangeError`: this switch therefore
///   also requires `CRATONVM_COMPAT_LHM_REVERSED_LIVE_VIEWS`);
/// * `values()`: `base.sequencedValues().reversed()` -- since round 14 wave 5
///   (C4-1) a live reversed `LinkedValues` carrier from `native_view_reversed`
///   (`CRATONVM_COMPAT_LHM_REVERSED_LIVE_VALUES`; with it off, the reversed
///   `ArrayList` snapshot, right when read, not write-through);
/// * `forEach` / `replaceAll`: real bytecode over `base.tail`, `e.before`,
///   `e.key`, `e.value` and `base.modCount` -- all real heap state
///   (`lhm_set` mirrors `head` / `tail` / `size`, bumps `modCount` on a size
///   write, and every list edit writes both node links; wave 3's audit);
/// * `firstEntry` / `lastEntry`: `base.lastEntry()` / `firstEntry()`, the
///   `SequencedMap` defaults over `entrySet()` / `reversed().entrySet()`;
/// * `toString`: `AbstractMap`'s, over `entrySet()`; `reversed()`: `base`.
///
/// A correctness fix, so on in `--compatible` too (owner decision
/// 2026-09-27); `--jdk-only` is unchanged (the row is a retired shadow there,
/// `RETIRED_SHADOW_LHM_SEQUENCED_VIEW_TRIPLES`, and the same view already
/// runs), and the synthetic JDK, which has no bytecode for it, keeps the
/// snapshot. Kill switch `CRATONVM_COMPAT_LHM_REVERSED_REAL_VIEW=0`, or
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW` naming the triple. Read once per
/// registry population (boot), so no cache.
pub(crate) fn lhm_reversed_left_to_bytecode(r: &NativeMethodRegistry) -> bool {
    r.real_jdk()
        && !r.compatibility_mode().is_jdk_only()
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_COMPAT_LHM_REVERSED_REAL_VIEW")
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_COMPAT_LHM_REVERSED_LIVE_VIEWS")
        && !cratonvm_native_api::unretire::is_excluded(
            "java/util/LinkedHashMap",
            "reversed",
            "()Ljava/util/SequencedMap;",
        )
}

pub(crate) fn register_linked_hashmap_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/util/LinkedHashMap";

    registry.register(c, "<init>", "()V", native_lhm_init);
    registry.register(c, "<init>", "(I)V", native_lhm_init_capacity);
    registry.register(c, "<init>", "(IF)V", native_lhm_init_capacity_lf);
    // Round-9 HIGH: the access-ordered ctor — when the `boolean accessOrder`
    // argument is true, `get(k)` must move the accessed entry to the tail
    // of the insertion-order list. This is what makes LinkedHashMap usable
    // as the underlying store for LRU caches (e.g. `removeEldestEntry`
    // overrides in Guava / Caffeine fallbacks). Without this ctor the
    // accessOrder flag silently defaults to false and `get` is read-only,
    // corrupting LRU eviction policies.
    registry.register(c, "<init>", "(IFZ)V", native_lhm_init_capacity_lf_access);
    registry.register(c, "<init>", "(Ljava/util/Map;)V", native_lhm_init_from_map);
    registry.register(c, "size", "()I", native_lhm_size);
    registry.register(c, "isEmpty", "()Z", native_lhm_is_empty);
    registry.register(
        c,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_lhm_put_entry,
    );
    registry.register(
        c,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_lhm_get,
    );
    registry.register(
        c,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_lhm_remove,
    );
    registry.register(
        c,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_lhm_contains_key,
    );
    registry.register(
        c,
        "containsValue",
        "(Ljava/lang/Object;)Z",
        native_lhm_contains_value,
    );
    registry.register(c, "clear", "()V", native_lhm_clear);
    registry.register(c, "keySet", "()Ljava/util/Set;", native_lhm_key_set);
    registry.register(c, "values", "()Ljava/util/Collection;", native_lhm_values);
    registry.register(c, "entrySet", "()Ljava/util/Set;", native_lhm_entry_set);
    // SequencedMap.reversed() (Java 21). The synthetic LinkedHashMap has no real
    // head/tail entry chain for the JDK reverse-view bytecode to walk, so an
    // unintercepted reversed() yielded an empty map. Build a reverse-ordered
    // snapshot from the live insertion order instead.
    //
    // On a real JDK in `--compatible` it is no longer registered
    // (`lhm_reversed_left_to_bytecode`, proposal C3-2): the JDK's own
    // `ReversedLinkedHashMapView` runs over the natives registered here.
    if !lhm_reversed_left_to_bytecode(registry) {
        registry.register(c, "reversed", "()Ljava/util/SequencedMap;", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            Ok(Some(Value::Object(Some(build_reversed_map_snapshot(
                ctx, this,
            )?))))
        });
    }
    registry.register(c, "toString", "()Ljava/lang/String;", native_lhm_to_string);
    registry.register(
        c,
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_lhm_get_or_default,
    );
    registry.register(
        c,
        "putIfAbsent",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_lhm_put_if_absent,
    );
    registry.register(
        c,
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        native_lhm_for_each,
    );
    registry.register(c, "putAll", "(Ljava/util/Map;)V", native_lhm_put_all);
    // THE JDK 21 SEQUENCED SURFACE. Only `reversed` was registered, so every
    // other member ran the real body over the real `head`/`tail`/`before`/
    // `after` chain -- which this class keeps in an overlay, not on the heap.
    // See `lhm_put_at_end` and `lhm_poll_end`.
    registry.register(
        c,
        "putFirst",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_lhm_put_first,
    );
    registry.register(
        c,
        "putLast",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_lhm_put_last,
    );
    registry.register(
        c,
        "pollFirstEntry",
        "()Ljava/util/Map$Entry;",
        native_lhm_poll_first_entry,
    );
    registry.register(
        c,
        "pollLastEntry",
        "()Ljava/util/Map$Entry;",
        native_lhm_poll_last_entry,
    );
    // `SequencedMap` narrows the return TYPE of the three view accessors and
    // changes nothing else, so these are `keySet`/`values`/`entrySet` and
    // inherit their liveness rather than snapshotting again. Under
    // `--jdk-only` all three are retired shadows
    // (`RETIRED_SHADOW_LHM_SEQUENCED_VIEW_TRIPLES`): on 21 and 25 the retired
    // `keySet` & co. delegate here, and the real bodies are O(1).
    registry.register(
        c,
        "sequencedKeySet",
        "()Ljava/util/SequencedSet;",
        native_lhm_key_set,
    );
    registry.register(
        c,
        "sequencedValues",
        "()Ljava/util/SequencedCollection;",
        native_lhm_values,
    );
    registry.register(
        c,
        "sequencedEntrySet",
        "()Ljava/util/SequencedSet;",
        native_lhm_entry_set,
    );
    // `pollFirstEntry`/`pollLastEntry` are `SequencedMap` DEFAULT methods, which
    // `LinkedHashMap` does not declare -- the registry dump says so in one line
    // (`has_code: false`, `invocations: 0` while the sibling `putFirst` reads
    // `has_code: true`, `invocations: 2`). Dispatch resolves them to the
    // declaring INTERFACE, so the class-name registration above can never fire.
    // Registered on the interface as well, exactly as this file already does for
    // `java/util/Map`, `java/util/Deque` and `java/util/SortedSet`.
    registry.register(
        "java/util/SequencedMap",
        "pollFirstEntry",
        "()Ljava/util/Map$Entry;",
        native_lhm_poll_first_entry,
    );
    registry.register(
        "java/util/SequencedMap",
        "pollLastEntry",
        "()Ljava/util/Map$Entry;",
        native_lhm_poll_last_entry,
    );
    // ... and the three view carriers' own `reversed()`, which is real bytecode
    // over the same overlay-held chain: `sequencedKeySet().reversed()` answered
    // `[]` while `sequencedKeySet()` itself was already right
    // (apps/probes/LinkedSequencedShadowSweep 80-81). One class further out than the
    // round-2 fix reached.
    registry.register(
        "java/util/LinkedHashMap$LinkedKeySet",
        "reversed",
        "()Ljava/util/SequencedSet;",
        native_view_reversed,
    );
    registry.register(
        "java/util/LinkedHashMap$LinkedValues",
        "reversed",
        "()Ljava/util/SequencedCollection;",
        native_view_reversed,
    );
    registry.register(
        "java/util/LinkedHashMap$LinkedEntrySet",
        "reversed",
        "()Ljava/util/SequencedSet;",
        native_view_reversed,
    );

    // computeIfAbsent on LinkedHashMap must route through the LHM put/get
    // natives (which use the overlay-backed slots), NOT through the HashMap
    // versions inherited via dispatch — otherwise the put no-ops because
    // HashMap's slot indices alias different real-JDK fields on a LHM object.
    registry.register(
        c,
        "computeIfAbsent",
        "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;",
        native_lhm_compute_if_absent,
    );
    // `LinkedHashMap` inherits `HashMap`'s bucket-walking overrides of the
    // three conditional mutators, and `HashMap.removeNode` calls
    // `LinkedHashMap.afterNodeRemoval`, which casts the node to
    // `LinkedHashMap$Entry` — see `register_map_conditional_mutators`.
    register_map_conditional_mutators(registry, c);
    registry.set_category(__prev_cat);
}

/// `SequencedMap.putFirst(k, v)` / `putLast(k, v)`.
///
/// Both are an ordinary `put` FOLLOWED by a move: an absent key is inserted and
/// then placed at the requested end, and a PRESENT key keeps its value updated
/// and moves. The move is the half real bytecode could not do here, and it is
/// the whole difference from `put`.
pub(crate) fn lhm_put_at_end(ctx: &mut dyn NativeContext, args: &[Value], at_head: bool) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    if lhm_single_hash_enabled() {
        let value = args.get(2).copied().unwrap_or(Value::Object(None));
        let value_pin = pin_value(ctx, value);
        let result = lhm_put_at_end_single_hash(
            ctx,
            (this_pin, this),
            (key_pin, key),
            (value_pin, value),
            at_head,
        );
        ctx.unpin_native_roots(this_pin);
        return result;
    }
    let previous = match native_lhm_put(ctx, args) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let this_cur = ctx.read_native_pin(this_pin, this);
    let key_cur = read_pinned_elem(ctx, key_pin, key);
    let node = match lhm_find_node(ctx, this_cur, &key_cur) {
        Ok(n) => n,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let this_cur = ctx.read_native_pin(this_pin, this);
    if let Some(node) = node {
        if at_head {
            lhm_move_to_head(ctx, this_cur, node);
        } else {
            lhm_move_to_tail(ctx, this_cur, node);
        }
    }
    ctx.unpin_native_roots(this_pin);
    Ok(previous)
}

/// [`lhm_put_at_end`] under `CRATONVM_COMPAT_LHM_SINGLE_HASH`: one `hashCode()`
/// for the whole call, as the JDK's `putFirst`/`putLast` (`putVal(hash(key),
/// ..)`), and the node taken from the `put` rather than looked up again. Each
/// `(pin, value)` pair is the caller's pin and the value it was taken from; the
/// caller releases the pins.
pub(crate) fn lhm_put_at_end_single_hash(
    ctx: &mut dyn NativeContext,
    this: (usize, ObjectRef),
    key: (usize, Value),
    value: (usize, Value),
    at_head: bool,
) -> MethodCallResult {
    let hash = match key.1 {
        Value::Object(Some(k)) => Some(map_hash_key(ctx, k)?),
        _ => None,
    };
    // `hashCode()` is Java and can move all three.
    let this_cur = ctx.read_native_pin(this.0, this.1);
    let key_cur = read_pinned_elem(ctx, key.0, key.1);
    let value_cur = read_pinned_elem(ctx, value.0, value.1);
    let mut placed: Option<ObjectRef> = None;
    let previous = native_lhm_put_evict_hashed(
        ctx,
        &[Value::Object(Some(this_cur)), key_cur, value_cur],
        true,
        GrowAlloc::SingleAttempt,
        hash,
        Some(&mut placed),
        false,
    )?;
    let node = match placed {
        Some(node) => Some(node),
        None => {
            let this_cur = ctx.read_native_pin(this.0, this.1);
            let key_cur = read_pinned_elem(ctx, key.0, key.1);
            lhm_find_node_hashed(ctx, this_cur, &key_cur, hash)?
        }
    };
    let this_cur = ctx.read_native_pin(this.0, this.1);
    if let Some(node) = node {
        if at_head {
            lhm_move_to_head(ctx, this_cur, node);
        } else {
            lhm_move_to_tail(ctx, this_cur, node);
        }
    }
    Ok(previous)
}

pub(crate) fn native_lhm_put_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lhm_put_at_end(ctx, args, true)
}

pub(crate) fn native_lhm_put_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lhm_put_at_end(ctx, args, false)
}

/// `SequencedMap.pollFirstEntry()` / `pollLastEntry()` — return the entry AND
/// remove it.
///
/// The real bodies answered the right entry and left it in place, so the idiom
/// the method exists for (`while ((e = pollLastEntry()) != null)`) never
/// terminated.
pub(crate) fn lhm_poll_end(ctx: &mut dyn NativeContext, args: &[Value], at_head: bool) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let node = if at_head {
        lhm_get(ctx, this, "head", LHM_FIELD_HEAD)
    } else {
        lhm_get(ctx, this, "tail", LHM_FIELD_TAIL)
    };
    let node = match node {
        Value::Object(Some(nd)) => nd,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = ctx.get_field(node, LHM_NODE_KEY);
    let value = ctx.get_field(node, LHM_NODE_VALUE);
    // The returned entry is a DETACHED snapshot in the JDK
    // (`AbstractMap.SimpleImmutableEntry`), so it is built before the removal
    // and does not track it. Mint the real class rather than the interface
    // `Map$Entry` (HotSpot cannot produce an instance of an interface --
    // `H16-2` N2) at its OWN declared width (2 fields, no source slot): this
    // is the exact sibling of `tm_make_entry`'s `firstEntry`/`lastEntry`
    // snapshots, and it gets the same benefit for free -- real
    // `AbstractMap.SimpleImmutableEntry.setValue` bytecode throws
    // `UnsupportedOperationException`, matching the JDK contract, where the
    // old 3-field carrier with a null source slot let `setValue` silently
    // succeed.
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let value_pin = pin_value(ctx, value);
    let entry = try_alloc_synthetic(ctx, "java/util/AbstractMap$SimpleImmutableEntry", 2);
    let entry = match entry {
        Ok(e) => e,
        Err(err) => {
            ctx.unpin_native_roots(this_pin);
            return Err(err);
        }
    };
    let entry_pin = ctx.pin_native_root(entry);
    let entry_cur = ctx.read_native_pin(entry_pin, entry);
    let key_cur = read_pinned_elem(ctx, key_pin, key);
    ctx.set_field(entry_cur, 0, key_cur);
    let entry_cur = ctx.read_native_pin(entry_pin, entry);
    let value_cur = read_pinned_elem(ctx, value_pin, value);
    ctx.set_field(entry_cur, 1, value_cur);
    let this_cur = ctx.read_native_pin(this_pin, this);
    let key_cur = read_pinned_elem(ctx, key_pin, key);
    let removed = native_lhm_remove(ctx, &[Value::Object(Some(this_cur)), key_cur]);
    let answer = Value::Object(Some(ctx.read_native_pin(entry_pin, entry)));
    ctx.unpin_native_roots(this_pin);
    removed?;
    Ok(Some(answer))
}

pub(crate) fn native_lhm_poll_first_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lhm_poll_end(ctx, args, true)
}

pub(crate) fn native_lhm_poll_last_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lhm_poll_end(ctx, args, false)
}

/// `SequencedCollection.reversed()` for a view carrier — a reverse-ordered
/// `ArrayList` over the view's current contents.
///
/// A SNAPSHOT, like `LinkedHashMap.reversed`'s. The JDK's is a live
/// `ReverseOrderLinkedHashMapView` and the difference shows only for a write to
/// the SOURCE after the view was taken; the alternative here was real bytecode
/// over a `head`/`tail` chain this VM keeps in an overlay, which answered an
/// empty collection.
///
/// Round 14 wave 3 (lane compat3): the `LinkedKeySet` row is declared
/// `()Ljava/util/SequencedSet;`, and an `ArrayList` is not a `SequencedSet`.
/// A caller that uses the answer through that type -- the JDK's own
/// `ReverseLinkedHashSetView.iterator()`,
/// `map().sequencedKeySet().reversed().iterator()`, which runs since CP2-2 --
/// died with `IncompatibleClassChangeError: Class java.util.ArrayList does
/// not implement the requested interface java.util.SequencedSet`. A key-set
/// carrier over a `LinkedHashMap` now answers a reverse-ordered
/// `LinkedHashSet` built from the SOURCE map's list and stored hashes
/// ([`lhs_reversed_snapshot_of`]: no `hashCode()`), under the same switch as
/// CP2-2 (`CRATONVM_COMPAT_LHS_REVERSED_REAL_VIEW`). The entry-set row has the
/// same type hole and is left (building a set of entries would call the
/// entries' `hashCode()`, i.e. the user's key and value `hashCode()`); the
/// values row's `SequencedCollection` is satisfied by the `ArrayList`.
///
/// Round 14 wave 4 (lane compat4), proposal C3-1 of
/// `jit-r14-compat3-proposals-RETIRED-20260930.md`: a key-set or entry-set carrier over a
/// `LinkedHashMap` now answers what the JDK answers, `new LinkedKeySet(true)`
/// / `new LinkedEntrySet(true)` -- a LIVE view of the same source with its
/// declared `reversed` set ([`linked_view_reversal_plan`]) -- and a reversed
/// one answers the source's own `sequencedKeySet()` / `sequencedEntrySet()`.
/// So a removal through the reversed view (its iterator, `remove(o)`,
/// `removeIf`, `retainAll`, `clear()`) reaches the source, and the entry-set
/// row is a `SequencedSet` at last. `CRATONVM_COMPAT_LHM_REVERSED_LIVE_VIEWS=0`
/// restores the wave-3 answers below.
///
/// Round 14 wave 5 (lane compat5), proposal C4-1: the values row likewise
/// answers `new LinkedValues(true)` for a native `LinkedValues` carrier over a
/// `LinkedHashMap` (and a reversed one the source's `sequencedValues()`); see
/// [`lhm_reversed_live_values_enabled`]. Its class is HotSpot's and its
/// removals reach the source.
pub(crate) fn native_view_reversed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    if let Some((source, kind, reversed)) = linked_view_reversal_plan(&*ctx, this) {
        if reversed {
            let a = [Value::Object(Some(source))];
            return if kind == VIEW_KIND_KEYSET {
                native_lhm_key_set(ctx, &a)
            } else {
                native_lhm_entry_set(ctx, &a)
            };
        }
        // `this` is read again below if the mint cannot be marked, so it is
        // rooted across the mint's allocations.
        let this_pin = ctx.pin_native_root(this);
        let minted = mint_reversed_linked_view(ctx, source, kind);
        this = ctx.read_native_pin(this_pin, this);
        ctx.unpin_native_roots(this_pin);
        if let Some(set) = minted? {
            return Ok(Some(Value::Object(Some(set))));
        }
    }
    // C4-1: the values row, `LinkedValues.reversed()` -- `reversed ?
    // LinkedHashMap.this.sequencedValues() : new LinkedValues(true)`.
    if let Some((source, reversed)) = linked_values_reversal_plan(&*ctx, this) {
        if reversed {
            return native_lhm_values(ctx, &[Value::Object(Some(source))]);
        }
        let this_pin = ctx.pin_native_root(this);
        let minted = mint_reversed_linked_values(ctx, source);
        this = ctx.read_native_pin(this_pin, this);
        ctx.unpin_native_roots(this_pin);
        if let Some(list) = minted? {
            return Ok(Some(Value::Object(Some(list))));
        }
    }
    if let Some(source) = lhm_key_set_view_source(&*ctx, this) {
        return lhs_reversed_snapshot_of(ctx, this, source);
    }
    let mut elems = al_or_collection_elements(ctx, this)?;
    elems.reverse();
    let (pin_base, handles) = pin_value_slice(ctx, &elems);
    let arr = alloc_ref_array(ctx, elems.len());
    let arr_pin = ctx.pin_native_root(arr);
    for (i, e) in elems.iter().enumerate() {
        let arr = ctx.read_native_pin(arr_pin, arr);
        let v = read_pinned_elem(ctx, handles[i], *e);
        ctx.set_array_element(arr, i, v);
    }
    let arr = ctx.read_native_pin(arr_pin, arr);
    let out = alloc_arraylist_with(ctx, arr, elems.len() as i32)?;
    if pin_base != usize::MAX {
        ctx.unpin_native_roots(pin_base);
    } else {
        ctx.unpin_native_roots(arr_pin);
    }
    Ok(Some(Value::Object(Some(out))))
}

/// The source `LinkedHashMap` of a native `LinkedHashMap$LinkedKeySet`
/// carrier (its view backing records the source), when
/// [`native_view_reversed`] must answer a `SequencedSet` for it; `None` for
/// every other receiver or with `CRATONVM_COMPAT_LHS_REVERSED_REAL_VIEW=0`.
/// Pure reads.
pub(crate) fn lhm_key_set_view_source(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !compat_switch_on(&ON, "CRATONVM_COMPAT_LHS_REVERSED_REAL_VIEW") {
        return None;
    }
    let class = ctx.class_name_arc_of_id(ctx.class_id_of_object(this))?;
    if &*class != "java/util/LinkedHashMap$LinkedKeySet" {
        return None;
    }
    let source = view_backing_source(ctx, hs_backing_map(ctx, this)?)?;
    is_lhm_receiver(ctx, source).then_some(source)
}

/// `CRATONVM_COMPAT_LHM_REVERSED_LIVE_VIEWS` -- default ON, `=0` restores the
/// wave-3 snapshots. `LinkedHashMap$LinkedKeySet.reversed()` /
/// `$LinkedEntrySet.reversed()` answer a live carrier with `reversed` set
/// (see [`native_view_reversed`]), and every reader of a set-shaped view
/// honours that field ([`linked_set_view_reversed`]). Round 14 wave 4 (lane
/// compat4), proposal C3-1; item 3 of
/// `r13w8-hashcompat3-compatible-collection-residuals-CLOSED-20260930.md`.
pub(crate) fn lhm_reversed_live_views_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_LHM_REVERSED_LIVE_VIEWS")
}

/// Whether `this` is a `LinkedHashMap$LinkedKeySet` / `$LinkedEntrySet`
/// whose declared `final boolean reversed` is set, i.e. a view that must walk
/// its source tail first (the JDK's `LinkedKeyIterator(reversed)`,
/// `keysToArray(.., reversed)`, `forEach`'s `tail` / `before` loop). Pure
/// reads, so a caller may ask it before its own allocations and keep the
/// answer across them. `false` with the switch off.
pub(crate) fn linked_set_view_reversed(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    if !lhm_reversed_live_views_enabled() {
        return false;
    }
    let cid = ctx.class_id_of_object(this);
    if !matches!(
        ctx.class_name_arc_of_id(cid).as_deref(),
        Some("java/util/LinkedHashMap$LinkedKeySet" | "java/util/LinkedHashMap$LinkedEntrySet")
    ) {
        return false;
    }
    let Some(slot) = ctx.resolve_field_index_by_class_id(cid, "reversed") else {
        return false;
    };
    slot < ctx.object_num_fields(this)
        && matches!(ctx.get_field(this, slot), Value::Int(v) if v != 0)
}

/// `elems` as the receiver hands them out: reversed when
/// [`linked_set_view_reversed`] said so (asked before the collection).
pub(crate) fn in_view_order(mut elems: Vec<Value>, reversed: bool) -> Vec<Value> {
    if reversed {
        elems.reverse();
    }
    elems
}

/// What [`native_view_reversed`] answers for a native key-set / entry-set
/// carrier over a `LinkedHashMap`: `(source, kind, this_is_reversed)`, or
/// `None` for every other receiver (a values carrier, a carrier the image's
/// own bytecode minted, a non-`LinkedHashMap` source, a class without the
/// `reversed` field) and with the switch off. Pure reads.
pub(crate) fn linked_view_reversal_plan(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Option<(ObjectRef, i32, bool)> {
    if !lhm_reversed_live_views_enabled() {
        return None;
    }
    let cid = ctx.class_id_of_object(this);
    let kind = match ctx.class_name_arc_of_id(cid).as_deref() {
        Some("java/util/LinkedHashMap$LinkedKeySet") => VIEW_KIND_KEYSET,
        Some("java/util/LinkedHashMap$LinkedEntrySet") => VIEW_KIND_ENTRYSET,
        _ => return None,
    };
    // The minted twin is marked through this field; no field, no plan (a JDK
    // before 21 declares neither it nor `reversed()`).
    let slot = ctx.resolve_field_index_by_class_id(cid, "reversed")?;
    if slot >= ctx.object_num_fields(this) {
        return None;
    }
    let backing = hs_backing_map(ctx, this)?;
    let source = view_backing_source(ctx, backing)?;
    if view_backing_kind(ctx, backing) != kind || !is_lhm_receiver(ctx, source) {
        return None;
    }
    let reversed = matches!(ctx.get_field(this, slot), Value::Int(v) if v != 0);
    Some((source, kind, reversed))
}

/// `new LinkedKeySet(true)` / `new LinkedEntrySet(true)` over `source`: a
/// live view carrier minted exactly as `keySet()` / `entrySet()` mint theirs
/// (a view backing recording the source, so every write goes through to it)
/// but NOT cached in the source's `keySet` / `entrySet` field, with its
/// declared `reversed` set. A key set's backing is filled from the source's
/// keys in list order (`make_view_set_of` takes their stored hashes: no
/// `hashCode()`); an entry set's is rebuilt from the source on every read
/// ([`view_source_generation`] gives that kind no stamp), so it starts empty
/// and no entry's `hashCode()` runs. `Ok(None)` when the mint did not land on
/// a class whose `reversed` could be set (the caller falls back).
pub(crate) fn mint_reversed_linked_view(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
    kind: i32,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let mut source = source;
    let keys = if kind == VIEW_KIND_KEYSET {
        lhm_collect_keys(&*ctx, source)
    } else {
        Vec::new()
    };
    let set = make_view_set_of(ctx, &mut source, kind, &keys)?;
    // Pure reads and one store from here: `set` stays current.
    let cid = ctx.class_id_of_object(set);
    let linked = matches!(
        ctx.class_name_arc_of_id(cid).as_deref(),
        Some("java/util/LinkedHashMap$LinkedKeySet" | "java/util/LinkedHashMap$LinkedEntrySet")
    );
    let Some(slot) = ctx.resolve_field_index_by_class_id(cid, "reversed") else {
        return Ok(None);
    };
    // Never over the undeclared backing slot `hs_map_slot` placed past the
    // declared fields.
    let backing_slot = hs_map_slot(&*ctx, set).unwrap_or(HS_FIELD_MAP);
    if !linked || slot >= backing_slot {
        return Ok(None);
    }
    ctx.set_field(set, slot, Value::Int(1));
    Ok(Some(set))
}

/// `CRATONVM_COMPAT_LHM_REVERSED_LIVE_VALUES` -- default ON, `=0` restores the
/// reversed `ArrayList` snapshot. `LinkedHashMap$LinkedValues.reversed()` on a
/// values carrier this crate minted answers what JDK 25 answers,
/// `new LinkedValues(true)` (a live values carrier over the same source with
/// its declared `reversed` set, [`mint_reversed_linked_values`]), and a
/// reversed one answers the source's own `sequencedValues()`. Every reader of
/// the carrier honours the field ([`values_view_reversed`]):
/// [`resync_values_view`] (iterator, `toArray`, `forEach`, `stream`,
/// `toString`, ...), the iterator's node lookup ([`values_view_node_at`]),
/// `remove(o)` ([`values_view_remove_jdk`]: the LAST equal value goes, as
/// `AbstractCollection.remove` over the reversed iterator removes) and the
/// node-walking `removeIf` ([`native_al_remove_if`]). So a removal through
/// `lhm.sequencedValues().reversed()` -- and through `lhm.reversed().values()`,
/// which is that call since C3-2 -- reaches the source. Round 14 wave 5 (lane
/// compat5), proposal C4-1 of `jit-r14-compat4-proposals-RETIRED-20260930.md`; item 3 of
/// `r13w8-hashcompat3-compatible-collection-residuals-CLOSED-20260930.md`. Also
/// requires `CRATONVM_COMPAT_LHM_REVERSED_LIVE_VIEWS` (the family switch).
pub(crate) fn lhm_reversed_live_values_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    lhm_reversed_live_views_enabled()
        && compat_switch_on(&ON, "CRATONVM_COMPAT_LHM_REVERSED_LIVE_VALUES")
}

/// The declared `reversed` slot of a `LinkedHashMap$LinkedValues` view carrier
/// this crate's layout can hold (strictly below the carrier's list slots,
/// [`view_carrier_slots`]); `None` for every other receiver. Pure reads.
pub(crate) fn linked_values_reversed_slot(ctx: &dyn NativeContext, list: ObjectRef) -> Option<usize> {
    let cid = ctx.class_id_of_object(list);
    if ctx.class_name_arc_of_id(cid).as_deref() != Some("java/util/LinkedHashMap$LinkedValues") {
        return None;
    }
    let slot = ctx.resolve_field_index_by_class_id(cid, "reversed")?;
    let ((data_slot, _, _), layout) = al_slots_and_layout_for(ctx, list);
    (layout == AlLayout::ViewCarrier && slot < data_slot && slot < ctx.object_num_fields(list))
        .then_some(slot)
}

/// Whether `list` is a `LinkedHashMap$LinkedValues` carrier whose declared
/// `reversed` is set, i.e. one that hands its source's values out tail first
/// (the JDK's `LinkedValueIterator(reversed)`). Pure reads, so a caller may ask
/// it before its own allocations and keep the answer across them. `false` with
/// the switch off. See [`lhm_reversed_live_values_enabled`].
pub(crate) fn values_view_reversed(ctx: &dyn NativeContext, list: ObjectRef) -> bool {
    if !lhm_reversed_live_values_enabled() {
        return false;
    }
    linked_values_reversed_slot(ctx, list)
        .is_some_and(|slot| matches!(ctx.get_field(list, slot), Value::Int(v) if v != 0))
}

/// Set a values carrier's declared `reversed`; `false` (nothing written) when
/// `list` is not a `LinkedValues` carrier of this crate's layout.
pub(crate) fn mark_values_view_reversed(ctx: &mut dyn NativeContext, list: ObjectRef) -> bool {
    let Some(slot) = linked_values_reversed_slot(&*ctx, list) else {
        return false;
    };
    ctx.set_field(list, slot, Value::Int(1));
    true
}

/// What [`native_view_reversed`] answers for a native `LinkedValues` carrier
/// over a `LinkedHashMap`: `(source, this_is_reversed)`, or `None` for every
/// other receiver (a carrier the image's own bytecode minted, a
/// non-`LinkedHashMap` source, a class without the `reversed` field) and with
/// the switch off. Pure reads.
pub(crate) fn linked_values_reversal_plan(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Option<(ObjectRef, bool)> {
    if !lhm_reversed_live_values_enabled() {
        return None;
    }
    let slot = linked_values_reversed_slot(ctx, this)?;
    let source = values_view_source(ctx, this)?;
    if source == this || !is_lhm_receiver(ctx, source) {
        return None;
    }
    let reversed = matches!(ctx.get_field(this, slot), Value::Int(v) if v != 0);
    Some((source, reversed))
}

/// `new LinkedValues(true)` over `source`: a live values carrier minted
/// exactly as `values()` mints its own ([`make_view_list_of`], the source in
/// the trailing marker slot so every removal goes through to it) but NOT
/// cached in the source's `values` field, with its declared `reversed` set and
/// its snapshot in reverse list order (every later read rebuilds it reversed,
/// [`resync_values_view`]). `Ok(None)` when the mint did not land on a class
/// whose `reversed` could be set (the caller falls back).
pub(crate) fn mint_reversed_linked_values(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let mut vals = lhm_collect_values(&*ctx, source);
    vals.reverse();
    let list = make_view_list_of(ctx, source, &vals, "java/util/LinkedHashMap$LinkedValues")?;
    // Pure reads and one store: `list` stays current.
    Ok(mark_values_view_reversed(ctx, list).then_some(list))
}

pub(crate) fn native_lhm_compute_if_absent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // RULE F, ahead of the key. See `reject_null_functional`.
    reject_null_functional(args.get(2))?;
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let function = match args.get(2) {
        Some(Value::Object(Some(f))) => *f,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `CRATONVM_COMPAT_MAP_COMPUTE_JDK`: the inherited `HashMap` body -- one
    // `hashCode()`, `afterNodeAccess` only on a hit, and a `modCount` check
    // around the mapper (see `map_compute_jdk_enabled`).
    if let Some(route) = map_remap_route(&*ctx, this, key) {
        let null = Value::Object(None);
        return map_remap_jdk(ctx, route, this, key, null, function, RemapOp::IfAbsent);
    }

    // GC-safety: `native_lhm_get`, the mapper's `apply` and `native_lhm_put`
    // below each dispatch arbitrary Java and can complete a moving young GC.
    // `this`, `key` and `function` are bare Rust locals — the caller's
    // `safe_native_call` pins keep the OBJECTS alive but do not rewrite these
    // copies, so every later use addressed a pre-move location.
    let cia_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let fn_pin = ctx.pin_native_root(function);
    let out =
        native_lhm_compute_if_absent_pinned(ctx, this, cia_pin, key, key_pin, function, fn_pin);
    ctx.unpin_native_roots(cia_pin);
    out
}

pub(crate) fn native_lhm_compute_if_absent_pinned(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    cia_pin: usize,
    key: Value,
    key_pin: usize,
    function: ObjectRef,
    fn_pin: usize,
) -> MethodCallResult {
    // If the key is already present, return the existing value unchanged.
    let key = read_pinned_elem(ctx, key_pin, key);
    let existing = native_lhm_get(ctx, &[Value::Object(Some(this)), key])?;
    if let Some(Value::Object(Some(_))) = existing {
        return Ok(existing);
    }

    // The mapper result is not one of this native call's original arguments,
    // so it is the only ObjectRef that needs an explicit root across the
    // direct (and potentially allocating) `native_lhm_put` below. `this` and
    // `key` remain in the surrounding safe-native argument root set. A short
    // global root avoids the per-frame snapshot work of three transient pins
    // while preserving the result through a moving collection.
    let function = ctx.read_native_pin(fn_pin, function);
    let key = read_pinned_elem(ctx, key_pin, key);
    let result = ctx.invoke_virtual(
        function,
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[key],
    )?;
    let new_val = result.unwrap_or(Value::Object(None));
    if let Value::Object(None) = new_val {
        return Ok(Some(Value::Object(None)));
    }
    let result_root = match new_val {
        Value::Object(Some(obj)) => Some(ctx.add_global_root(obj)),
        _ => None,
    };
    let this = ctx.read_native_pin(cia_pin, this);
    let key = read_pinned_elem(ctx, key_pin, key);
    let put_result = native_lhm_put(ctx, &[Value::Object(Some(this)), key, new_val]);
    let new_val = match result_root.and_then(|root| ctx.resolve_global_root(root)) {
        Some(obj) => Value::Object(Some(obj)),
        None => new_val,
    };
    if let Some(root) = result_root {
        ctx.remove_global_root(root);
    }
    put_result?;
    Ok(Some(new_val))
}

/// `LinkedHashMap`'s constructor body, EAGER -- for this crate's own maps.
/// See [`native_map_init`] for why the two variants exist.
pub(crate) fn lhm_init_with_cap(ctx: &mut dyn NativeContext, this: ObjectRef, cap: usize) {
    lhm_init_inner(ctx, this, cap, true)
}

/// `lhm_init_with_cap` as the JDK's constructor behaves: allocate nothing and
/// let the first insert size the table. `LinkedHashMap` extends `HashMap` and
/// inherits its lazy `table`, so an empty one has `table == null` on HotSpot
/// exactly as a `HashMap` does -- MEASURED.
pub(crate) fn lhm_init_with_cap_lazy(ctx: &mut dyn NativeContext, this: ObjectRef, cap: usize) {
    lhm_init_inner(ctx, this, cap, false)
}

pub(crate) fn lhm_init_inner(ctx: &mut dyn NativeContext, this: ObjectRef, cap: usize, eager: bool) {
    // Seed the identity-hash header *before* the first overlay write,
    // so that the GC-stable key is established at init time — see
    // `identity_hash::seed`.
    ih_seed(ctx, this);
    // GC-safety: `alloc_bucket_table` below allocates the bucket array and
    // can trigger a moving GC; `this` is reused across every `lhm_set` call
    // that follows, unpinned otherwise. Confirmed live via
    // CRATONVM_DBG_STALE_OBJREF during WildFly parallel-extension-add (same
    // "Family 1" pattern as native_hashmap_get_exact's fix -- see
    // wildfly-parallel-boot-stale-objectref-residual.md).
    let this_pin = ctx.pin_native_root(this);
    if !eager {
        set_map_size(ctx, this, 0);
        try_set_jdk_map_field(ctx, this, "modCount", Value::Int(0));
        // `threshold` carries the pending table size while `table` is null --
        // HotSpot's own encoding, and what `map_state` reads back. 0 for the
        // no-arg constructor, which is what HotSpot leaves there too.
        let pending = if cap == MAP_DEFAULT_CAPACITY {
            0
        } else {
            cap as i32
        };
        try_set_jdk_map_field(ctx, this, "threshold", Value::Int(pending));
        try_set_jdk_map_field(ctx, this, "loadFactor", Value::Float(0.75_f32));
        // The linkage head/tail and the model's own size/capacity still have to
        // be established -- only the BUCKET ARRAY is deferred. `lhm_set` writes
        // both the model slot and the JDK-named field, so an empty
        // `LinkedHashMap` is fully formed apart from `table`, exactly as
        // HotSpot leaves it.
        lhm_set(ctx, this, "size", LHM_FIELD_SIZE, Value::Int(0));
        lhm_set(ctx, this, "head", LHM_FIELD_HEAD, Value::Object(None));
        lhm_set(ctx, this, "tail", LHM_FIELD_TAIL, Value::Object(None));
        try_set_jdk_map_field(ctx, this, "modCount", Value::Int(0));
        ctx.unpin_native_roots(this_pin);
        return;
    }
    // Cap the eager bucket-table allocation to what the heap can hold (see
    // `alloc_bucket_table`); `cap` is rebound to the actual table length so
    // `__capacity`/`threshold` stay consistent and the map grows on demand.
    // `LinkedHashMap` inherits `HashMap`'s `table` field, so its component type
    // is `java/util/HashMap$Node` and NOT `LinkedHashMap$Entry` — MEASURED on
    // HotSpot, where `LinkedHashMap.table` is `[Ljava.util.HashMap$Node;` while
    // its nodes are `LinkedHashMap$Entry`. The nodes are a SUBCLASS, so the
    // store is covariant-legal. `bucket_table_component` returns the one type
    // that serves both callers; see its table.
    let component = bucket_table_component(ctx, this);
    let this = ctx.read_native_pin(this_pin, this);
    let (buckets, cap) = alloc_bucket_table(ctx, component, cap);
    let this = ctx.read_native_pin(this_pin, this);
    lhm_set(
        ctx,
        this,
        "table",
        LHM_FIELD_BUCKETS,
        Value::Object(Some(buckets)),
    );
    lhm_set(ctx, this, "size", LHM_FIELD_SIZE, Value::Int(0));
    lhm_set(
        ctx,
        this,
        "__capacity",
        LHM_FIELD_CAPACITY,
        Value::Int(cap as i32),
    );
    lhm_set(ctx, this, "head", LHM_FIELD_HEAD, Value::Object(None));
    lhm_set(ctx, this, "tail", LHM_FIELD_TAIL, Value::Object(None));

    // KC26 clone path: LinkedHashMap.<init>()V is intercepted natively (we
    // never run the bytecode chain LHM → HashMap.<init> → putfield loadFactor).
    // That leaves the JDK-resolved `loadFactor`, `threshold`, and `table` heap
    // slots zero-initialised. Once any bytecode path reads them — notably
    // `HashMap.clone()` → `reinitialize()` → `putMapEntries()` → `resize()` —
    // a loadFactor of 0.0f makes the resize compute `newCap = 1073741824`,
    // which then OOMs on the `anewarray Node[1073741824]`. Mirror the JDK
    // defaults into the real heap fields so cloned LHM instances see sane
    // values when the HashMap.clone bytecode walks them.
    try_set_jdk_map_field(ctx, this, "loadFactor", Value::Float(0.75_f32));
    try_set_jdk_map_field(ctx, this, "threshold", Value::Int((cap as i32 * 3) / 4));
    try_set_jdk_map_field(ctx, this, "modCount", Value::Int(0));
    ctx.unpin_native_roots(this_pin);
}

pub(crate) fn native_lhm_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    lhm_init_with_cap_lazy(ctx, this, MAP_DEFAULT_CAPACITY);
    Ok(None)
}

pub(crate) fn native_lhm_init_capacity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Serves the `(I)V`, `(IF)V` and `(IFZ)V` constructors, so validating
    // `args[1]`/`args[2]` here covers all three.
    map_ctor_capacity_load_check(args, MapCtorRefusalSpelling::HashMapFamily)?;
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let cap = match args.get(1) {
        // `CRATONVM_COMPAT_LHM_CTOR_CAPACITY`: `HashMap(n, lf)`'s
        // `tableSizeFor(n)`, recorded as the pending `threshold` by the lazy
        // constructor below and read back by `lhm_pending_table_size`.
        Some(Value::Int(c)) if lhm_ctor_capacity_enabled() => lhm_jdk_table_size_for(*c) as usize,
        Some(Value::Int(c)) => {
            // Bug 4: same load-factor adjustment as `native_map_init_capacity`.
            // The JDK contract: a caller asking for capacity N expects to hold
            // N mappings without triggering a resize. With loadFactor=0.75
            // that needs ceil(N * 4 / 3) buckets, rounded up to a power of two.
            let requested = std::cmp::max(*c, 1) as u64;
            let needed = requested.saturating_mul(4).div_ceil(3);
            let capped = std::cmp::min(needed, MAP_MAX_CAPACITY as u64).max(1) as usize;
            capped
                .checked_next_power_of_two()
                .unwrap_or(MAP_MAX_CAPACITY as usize)
                .max(MAP_DEFAULT_CAPACITY)
        }
        _ => MAP_DEFAULT_CAPACITY,
    };
    lhm_init_with_cap_lazy(ctx, this, cap);
    // `CRATONVM_COMPAT_LHM_LOAD_FACTOR`: `this.loadFactor = loadFactor` (the
    // check above refused `<= 0` and NaN). The lazy constructor allocates
    // nothing, so `this` is still current.
    if let Some(Value::Float(lf)) = args.get(2) {
        if lhm_load_factor_enabled() {
            try_set_jdk_map_field(ctx, this, "loadFactor", Value::Float(*lf));
        }
    }
    Ok(None)
}

/// `LinkedHashMap(int initialCapacity, float loadFactor)` — the factor is kept
/// in `loadFactor` and grows the table under `CRATONVM_COMPAT_LHM_LOAD_FACTOR`
/// (see [`lhm_grow_threshold`]); with the switch off it is validated and
/// dropped (0.75).
pub(crate) fn native_lhm_init_capacity_lf(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_lhm_init_capacity(ctx, args)
}

/// `LinkedHashMap(int initialCapacity, float loadFactor, boolean accessOrder)`
/// — when accessOrder is true, subsequent `get(k)` calls reorder the entry
/// to the tail of the insertion-order list (LRU-style). The flag is stashed
/// in the LHM overlay under the name `accessOrder` so `native_lhm_get` can
/// consult it without a Java field read.
pub(crate) fn native_lhm_init_capacity_lf_access(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    // Reuse the (I)V capacity adjustment path.
    native_lhm_init_capacity(ctx, args)?;
    let access_order = match args.get(3) {
        Some(Value::Int(v)) => *v != 0,
        _ => false,
    };
    if access_order {
        // Use LHM_FIELD_TAIL+1 as a dummy fallback index — `lhm_set` writes
        // the value primarily under the string-name slot in the overlay,
        // which is what `lhm_is_access_order` reads back.
        lhm_set(ctx, this, "accessOrder", LHM_FIELD_TAIL + 1, Value::Int(1));
    }
    // Also write through to the real-JDK field name in case the receiver
    // is a real-JDK LinkedHashMap with a slot for `accessOrder`.
    ctx.set_field_by_name(
        this,
        "accessOrder",
        Value::Int(if access_order { 1 } else { 0 }),
    );
    Ok(None)
}

/// Read the access-order flag for `this`. Defaults to false (insertion-order)
/// when no IFZ ctor ran. Consults both the overlay (where our ctor writes it)
/// and the real-JDK `accessOrder` field by name so a real-JDK LHM instance
/// initialised via Java bytecode still reports correctly.
pub(crate) fn lhm_is_access_order(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    if let Value::Int(v) = lhm_get(ctx, this, "accessOrder", LHM_FIELD_TAIL + 1) {
        if v != 0 {
            return true;
        }
    }
    matches!(ctx.get_field_by_name(this, "accessOrder"), Value::Int(v) if v != 0)
}

// S111r14: LinkedHashMap copy-constructor — see `native_map_init_from_map`
// for the rationale. DateTimeFormatterBuilder.appendText reaches us via
// `new LinkedHashMap<>(map)`.
pub(crate) fn native_lhm_init_from_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // RULE C: `LinkedHashMap(Map)` is `putMapEntries(m, false)`, which reads
    // `m.size()`.
    // NOT `reject_null_collection`: that is the
    // `Objects.requireNonNull(c)` shape, whose NPE carries no message. This
    // constructor DEREFERENCES its argument (`m.size()`), so the JDK raises a
    // helpful NPE naming the method and its own parameter name.
    if matches!(args.get(1), Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException {
            message: Some(
                "Cannot invoke \"java.util.Map.size()\" because \"m\" is null".to_string(),
            ),
        }
        .into());
    }
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave) —
    // lhm_init_with_cap allocates, so pass `native_lhm_put_all` refreshed
    // refs, not the pre-alloc `this`/`src`.
    let this_pin = ctx.pin_native_root(this);
    let src0 = args.get(1).copied().unwrap_or(Value::Object(None));
    let src_handle = pin_value(ctx, src0);
    lhm_init_with_cap_lazy(ctx, this, MAP_DEFAULT_CAPACITY);
    let this = ctx.read_native_pin(this_pin, this);
    let src = read_pinned_elem(ctx, src_handle, src0);
    // S111r24-fix: Route through `native_lhm_put_all` so insertion-order
    // linkage (head/tail) is established for each copied entry. Calling
    // `native_map_put_all` would invoke `native_map_put` which writes
    // directly into HashMap-style bucket nodes without `lhm_link_tail`,
    // leaving the LHM's insertion-order list empty and the entries
    // invisible to `entrySet()`/`putAll(...)` consumers like Spring's
    // `AnnotationAttributes(Map)` copy constructor.
    //
    // `putMapEntries(m, false)`: no `removeEldestEntry` during the copy
    // (`CRATONVM_COMPAT_LHM_COPY_NO_EVICT`, round 15 wave 4, lane compat4).
    let evict = !lhm_copy_no_evict_enabled();
    let r = lhm_put_all_impl(ctx, &[Value::Object(Some(this)), src], evict);
    ctx.unpin_native_roots(this_pin);
    r?;
    Ok(None)
}

pub(crate) fn native_lhm_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    // `reversed()`'s live half: rebuild from the source if it has moved. A receiver
    // that is not a reversed view is returned unchanged, at the cost of one overlay
    // lookup. See `resync_reversed_map`.
    let this = resync_reversed_map(ctx, this)?;
    let (_, size, _) = lhm_state(ctx, this);
    Ok(Some(Value::Int(size)))
}

pub(crate) fn native_lhm_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(1))),
    };
    // `reversed()`'s live half: rebuild from the source if it has moved. A receiver
    // that is not a reversed view is returned unchanged, at the cost of one overlay
    // lookup. See `resync_reversed_map`.
    let this = resync_reversed_map(ctx, this)?;
    let (_, size, _) = lhm_state(ctx, this);
    Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))
}

/// `LinkedHashMap.put` for the internal Rust callers (`putAll`, `merge`,
/// `compute`, the view and copy paths): the table grow gets one no-GC attempt
/// ([`GrowAlloc::SingleAttempt`]), because such a caller may hold bare
/// references across the put. The registered door is [`native_lhm_put_entry`].
pub(crate) fn native_lhm_put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_lhm_put_evict(ctx, args, true, GrowAlloc::SingleAttempt)
}

/// The registered `LinkedHashMap.put`: reached only from Java dispatch or
/// `invoke_virtual`, so the table grow may collect and retry
/// ([`GrowAlloc::Reclaim`]; gc-common w10-g).
pub(crate) fn native_lhm_put_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_lhm_put_evict(ctx, args, true, GrowAlloc::Reclaim)
}

pub(crate) fn lhm_remove_eldest_hook_decision(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> (ClassId, Option<std::rc::Rc<str>>, bool, bool) {
    let this_cid = ctx.class_id_of_object(this);
    // gc-common w21-d: the per-thread, per-VM `class_name_rc` memo, not
    // `class_name_of_id` -- this runs on every `LinkedHashMap.put` that inserts
    // a new key (and every `LinkedHashSet.add`), and the latter was a
    // class-manager read lock plus a fresh `String` per call for a name that
    // never changes.
    let class_name = class_name_rc(ctx, this_cid);
    let is_plain_lhm = class_name.as_deref() == Some("java/util/LinkedHashMap");
    let is_untrusted_slot0_object =
        this_cid.as_u32() == 0 && matches!(class_name.as_deref(), Some("java/lang/Object") | None);

    // A slot-0/Object read from a receiver that already reached the
    // LinkedHashMap native is not a real LHM subclass. Dispatching
    // Object.removeEldestEntry only turns the stale class-word read into a
    // NoSuchMethodError. Keep real subclass hooks, but fail closed here.
    (
        this_cid,
        class_name,
        is_plain_lhm,
        !is_plain_lhm && !is_untrusted_slot0_object,
    )
}

/// Like `native_lhm_put`, but honoring an explicit eviction-hook toggle --
/// see `native_map_put_evict` for why `native_hashmap_read_object` needs
/// `evict=false`. `grow` is the table's allocation policy, passed through to
/// [`lhm_resize`].
pub(crate) fn native_lhm_put_evict(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    evict: bool,
    grow: GrowAlloc,
) -> MethodCallResult {
    native_lhm_put_evict_hashed(ctx, args, evict, grow, None, None, false)
}

/// `CRATONVM_COMPAT_LHM_SINGLE_HASH` — default ON, `=0` restores the old
/// count. The JDK's `LinkedHashMap.put` / `putFirst` / `putLast` call the
/// key's `hashCode()` ONCE (`putVal(hash(key), ..)`). The native `put` hashed
/// it to place the node and then again inside `lhm_find_node`, and
/// `putFirst`/`putLast` ran that `put` and then a third lookup to find the node
/// to move: three `hashCode()` calls (and a second `equals()`) where HotSpot
/// makes one. `LinkedHashSet.addFirst`/`addLast` reach them through real
/// bytecode under `--jdk-only`. Round 13 wave 7, lane shadow
/// (`R13ShadowHashSetFamily`, SequencedSet scenario).
pub(crate) fn lhm_single_hash_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_LHM_SINGLE_HASH")
}

/// [`native_lhm_put_evict`] with the key's [`map_hash_key`] optionally
/// precomputed by the caller, and optionally reporting the node the key ended
/// up in (`placed`) so the caller need not look it up again. `placed` is left
/// `None` when a `removeEldestEntry` override ran after the insert: that is
/// Java, it can move or unlink the node, and the caller then re-finds it.
pub(crate) fn native_lhm_put_evict_hashed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    evict: bool,
    grow: GrowAlloc,
    known_hash: Option<i32>,
    placed: Option<&mut Option<ObjectRef>>,
    known_absent: bool,
) -> MethodCallResult {
    native_lhm_put_evict_inner(
        ctx,
        args,
        evict,
        grow,
        known_hash,
        placed,
        known_absent,
        true,
    )
}

/// [`native_lhm_put_evict_hashed`]; `may_treeify` is `false` for a caller that
/// relinks the new node itself and applies its own treeify rule (the compute
/// family, [`map_remap_jdk`]). A TREE bin is always inserted into through
/// `putTreeVal` (`CRATONVM_COMPAT_LHM_TREE_BINS`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn native_lhm_put_evict_inner(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    evict: bool,
    grow: GrowAlloc,
    known_hash: Option<i32>,
    mut placed: Option<&mut Option<ObjectRef>>,
    known_absent: bool,
    may_treeify: bool,
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let value = args.get(2).copied().unwrap_or(Value::Object(None));

    // A LinkedHashMap value is a REFERENCE: the node's value slot is
    // `V value` -> `Ljava/lang/Object;` on the real `java/util/HashMap$Node`
    // that `LinkedHashMap$Entry` extends. Now that `lhm_alloc_node` binds to
    // that real class, the descriptor-aware `set_field` path coerces a
    // primitive `Value` in that slot to NULL.
    //
    // Our own set layer writes a raw `Value::Int(1)` PRESENT sentinel there
    // (`native_hs_add`), and `native_hs_add` / `native_hs_remove` decide "was
    // it already in the set" purely from whether the previous value was null.
    // Coerced to null, `LinkedHashSet.remove(x)` DELETED the element and still
    // reported `false` — which broke Jersey's
    // `Resource.Builder.onBuildMethod`, whose
    // `checkState(methodBuilders.remove(builder))` then threw
    // `IllegalStateException` and failed every test that starts the Jersey
    // filter. `CopyOnWriteArraySet` shares the same backing and broke with it.
    //
    // Box it on the way in. A real Java caller can never arrive here with a
    // primitive — `Map.put`'s descriptor is `(Object,Object)Object`, so the
    // interpreter has already boxed — so this only ever fires for our own
    // internal sentinels, and `Integer.valueOf(1)` hands back the JDK's cached
    // instance rather than allocating. Boxing here, before the pins below, is
    // deliberate: `box_primitive_result` dispatches `valueOf`, which can GC.
    //
    // gen r4w4/rooting2: "before the pins" was only half of it -- the pins
    // below then rooted a receiver and key read BEFORE that `valueOf`. They
    // are carried across it here; a reference value (every Java caller) skips
    // the whole arm.
    let (this, key_val, value) = match value {
        Value::Object(_) => (this, key_val, value),
        primitive => {
            let box_base = ctx.pin_native_root(this);
            let box_key_pin = pin_value(ctx, key_val);
            let boxed = box_primitive_result(ctx, primitive);
            let this = ctx.read_native_pin(box_base, this);
            let key_val = read_pinned_elem(ctx, box_key_pin, key_val);
            ctx.unpin_native_roots(box_base);
            (this, key_val, boxed?)
        }
    };

    // GC-SAFETY: `this` is a bare Rust local read from `args`, not itself a
    // GC root -- only a `pin_native_root`/`read_native_pin` handle survives a
    // moving GC. `map_hash_key` (key.hashCode()), `lhm_resize` (allocates a
    // new bucket array), `lhm_find_node` (key.equals() dispatch per
    // bucket-chain probe), `new_object_initialized`, and `invoke_virtual`
    // (removeEldestEntry) below can all trigger one; this function used
    // `this` again after each without ever refreshing it. Confirmed live via
    // CRATONVM_DBG_STALE_OBJREF: native_lhm_put_evict -> lhm_state ->
    // lhm_get -> lhm_overlay_key -> widened_obj_key -> identity_hash_code
    // dereferencing a stale `this`. Pin once up front, re-read after every
    // hazard, and unpin once at each exit.
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key_val);
    let value_pin = pin_value(ctx, value);
    let key_val = read_pinned_elem(ctx, key_pin, key_val);
    let value = read_pinned_elem(ctx, value_pin, value);

    let hash = match key_val {
        Value::Object(Some(k)) => match known_hash {
            Some(hash) => hash,
            None => map_hash_key(ctx, k)?,
        },
        Value::Object(None) => 0,
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(None)));
        }
    };
    let this = ctx.read_native_pin(this_pin, this);
    let key_val = read_pinned_elem(ctx, key_pin, key_val);
    let value = read_pinned_elem(ctx, value_pin, value);

    // Check for resize. Also initialize table when buckets is None
    // (e.g. `org/springframework/core/annotation/AnnotationAttributes`
    // constructed via Spring's `new AnnotationAttributes(annotationType, false)`
    // — LinkedHashMap.<init>() runs as JDK bytecode and leaves our overlay
    // empty until the first put. Without this init the put would silently
    // no-op, dropping every key Spring writes via `TypeMappedAnnotation.asMap`
    // and surfacing as `IllegalArgumentException: Attribute 'type' not found
    // in attributes for annotation [...]ComponentScan$Filter` deep in
    // `ComponentScanAnnotationParser.parse` for `@SpringBootApplication`).
    //
    // `CRATONVM_COMPAT_LHM_GROW_ON_INSERT`: only a missing table is made here.
    // Growth waits until the key is known absent (below), as `putVal`'s
    // `if (++size > threshold) resize()` does; growing first also grew on an
    // OVERWRITE at the threshold, and resized before a tree bin's insert.
    // `CRATONVM_COMPAT_LHM_REMAP_GROW_ON_ENTRY`: a compute-family insert
    // (`may_treeify = false`, [`map_remap_jdk`]) grew on entry and does not
    // grow here at all.
    let no_grow = !may_treeify && lhm_remap_grow_on_entry_enabled();
    let grow_on_insert = lhm_grow_on_insert_enabled() && !no_grow;
    let (initial_buckets, size, cap) = lhm_state(ctx, this);
    if initial_buckets.is_none()
        || (!no_grow && !grow_on_insert && size + 1 > lhm_grow_threshold(&*ctx, this, cap))
    {
        lhm_resize(ctx, this, grow);
    }
    let this = ctx.read_native_pin(this_pin, this);
    let key_val = read_pinned_elem(ctx, key_pin, key_val);
    let value = read_pinned_elem(ctx, value_pin, value);

    // `CRATONVM_COMPAT_LHM_TREE_BINS`: a TREE bin takes `putVal`'s tree arm
    // itself -- the first-node test, then `putTreeVal` -- instead of the walk
    // below, whose `getTreeNode` would compare every key `putTreeVal` then
    // compares again.
    if !known_absent && lhm_tree_family(&*ctx, this) {
        if let Some(result) = lhm_tree_bin_put(
            ctx,
            this_pin,
            this,
            (key_pin, key_val),
            (value_pin, value),
            hash,
            (true, false),
            (evict, grow, grow_on_insert),
            &mut placed,
        ) {
            return result;
        }
    }
    let this = ctx.read_native_pin(this_pin, this);
    let key_val = read_pinned_elem(ctx, key_pin, key_val);
    let value = read_pinned_elem(ctx, value_pin, value);

    // Check for existing key. With the single-hash switch on, the hash taken
    // above is the probe's too (`lhm_single_hash_enabled`). A caller that has
    // just walked the chain itself and found no match (`known_absent`, the
    // single-walk `putIfAbsent`) skips the second walk and its `equals` calls;
    // no Java has run since that walk.
    let probe_hash = lhm_single_hash_enabled().then_some(hash);
    let existing = if known_absent {
        None
    } else {
        lhm_find_node_hashed(ctx, this, &key_val, probe_hash)?
    };
    if let Some(node) = existing {
        let this = ctx.read_native_pin(this_pin, this);
        let old = ctx.get_field(node, LHM_NODE_VALUE);
        let value = read_pinned_elem(ctx, value_pin, value);
        ctx.set_field(node, LHM_NODE_VALUE, value);
        // Access-order semantics: re-inserting a value for an existing key
        // counts as a structural access, so the entry must move to the tail
        // of the insertion-order list (mirrors `native_lhm_get`). With
        // insertion-order (the default) this is left untouched.
        if lhm_is_access_order(ctx, this) {
            lhm_move_to_tail(ctx, this, node);
        }
        if let Some(out) = placed.as_deref_mut() {
            *out = Some(node);
        }
        ctx.unpin_native_roots(this_pin);
        return Ok(Some(old));
    }
    let this = ctx.read_native_pin(this_pin, this);

    // `CRATONVM_COMPAT_LHM_TREE_BINS`: a tree bin (the caller walked it
    // itself, `known_absent`) takes `putTreeVal`, whose
    // `LinkedHashMap.newTreeNode` also links the new node at the end of the
    // insertion-order list; a chain that already holds `TREEIFY_THRESHOLD`
    // plain entries is treeified after this append.
    let mut treeify_after_insert = false;
    if lhm_tree_family(&*ctx, this) {
        if let Some(result) = lhm_tree_bin_put(
            ctx,
            this_pin,
            this,
            (key_pin, key_val),
            (value_pin, value),
            hash,
            (false, false),
            (evict, grow, grow_on_insert),
            &mut placed,
        ) {
            return result;
        }
        let this = ctx.read_native_pin(this_pin, this);
        let (buckets, size, cap) = lhm_state(ctx, this);
        if let Some(tab) = buckets {
            let head = ctx.get_array_element(tab, map_bucket_index(hash, cap));
            if may_treeify {
                let (len, all_plain) =
                    chain_census_of(&*ctx, head, chain_walk_limit(size), CF_LHM_ENTRY);
                // `hm_tree_classes_real` can load a class; only pinned
                // references are used after it.
                treeify_after_insert = len >= HM_TREEIFY_THRESHOLD
                    && all_plain
                    && lhm_tree_may_treeify(&*ctx, this)
                    && hm_tree_classes_real(ctx);
            }
        }
    }
    let this = ctx.read_native_pin(this_pin, this);
    // `CRATONVM_COMPAT_LHM_GROW_ON_INSERT`: the key is absent, so NOW the table
    // may grow -- before the link, which for a chain is where `putVal` plus
    // `resize()` leaves the new node too (see `native_map_put_evict_pinned`);
    // a bin about to be treeified keeps `putVal`'s own order (below).
    if grow_on_insert && !treeify_after_insert {
        let (_, size, cap) = lhm_state(ctx, this);
        if size + 1 > lhm_grow_threshold(&*ctx, this, cap) {
            lhm_resize(ctx, this, grow);
        }
    }
    let this = ctx.read_native_pin(this_pin, this);

    // Insert new node
    let (buckets, size, cap) = lhm_state(ctx, this);
    let buckets = match buckets {
        Some(b) => b,
        None => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(None)));
        }
    };
    let idx = map_bucket_index(hash, cap);

    // gcstress residual face-1 fix — `lhm_alloc_node` allocates and can
    // trigger a moving young GC that relocates `this` and `buckets` (bare
    // Rust locals captured above). The stale addresses would make the
    // head-insert `set_array_element(buckets, ..)` and the `this` writes below
    // land in freed-and-reused storage (same corrupt-cell family as
    // native_map_put). Pin `buckets` across the alloc and re-read both after
    // (`this` reuses the outer `this_pin` established at function entry).
    let buckets_pin = ctx.pin_native_root(buckets);
    let key_val = read_pinned_elem(ctx, key_pin, key_val);
    let value = read_pinned_elem(ctx, value_pin, value);
    let new_node: ObjectRef = lhm_alloc_node(ctx, key_val, value, hash)?;
    let this = ctx.read_native_pin(this_pin, this);
    let buckets = ctx.read_native_pin(buckets_pin, buckets);

    // Link into the bucket chain: at its tail, as `HashMap.putVal` does, under
    // `CRATONVM_COMPAT_LHM_BUCKET_ORDER`; at its head otherwise.
    lhm_link_into_bucket(
        ctx,
        buckets,
        idx,
        new_node,
        lhm_bucket_order_enabled(),
        chain_walk_limit(size),
    );

    // Link at tail of insertion-order list
    lhm_link_tail(ctx, this, new_node);

    lhm_set(ctx, this, "size", LHM_FIELD_SIZE, Value::Int(size + 1));

    if !treeify_after_insert {
        return lhm_after_node_insertion(ctx, this_pin, this, evict, Some(new_node), placed);
    }
    // `putVal`: `if (binCount >= TREEIFY_THRESHOLD - 1) treeifyBin(tab, hash);`
    // after the append, then `if (++size > threshold) resize();`. The bin's
    // entries are REPLACED by tree nodes, so the caller is not handed
    // `new_node` (it re-finds the key).
    let this = ctx.read_native_pin(this_pin, this);
    if let Err(e) = lhm_treeify_bin(ctx, this, hash, grow) {
        ctx.unpin_native_roots(this_pin);
        return Err(e);
    }
    if grow_on_insert {
        let this = ctx.read_native_pin(this_pin, this);
        let (_, size_now, cap_now) = lhm_state(ctx, this);
        if size_now > lhm_grow_threshold(&*ctx, this, cap_now) {
            lhm_resize(ctx, this, grow);
        }
    }
    lhm_after_node_insertion(ctx, this_pin, this, evict, None, placed)
}

/// `LinkedHashMap.afterNodeInsertion(evict)` for a native insert into the map
/// rooted at `this_pin`, then the release of `this_pin` (every pin above it
/// goes with it). `new_node` is the inserted node when it is still current
/// (no Java ran since it was made), reported through `placed` unless an
/// eviction hook runs. Answers the insert's `null`.
pub(crate) fn lhm_after_node_insertion(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
    evict: bool,
    new_node: Option<ObjectRef>,
    mut placed: Option<&mut Option<ObjectRef>>,
) -> MethodCallResult {
    let this = ctx.read_native_pin(this_pin, this);
    // Replicate `LinkedHashMap.afterNodeInsertion(true)`: after a NEW node is
    // inserted, consult the (overridable) `removeEldestEntry` hook on the
    // eldest insertion-order entry and evict it when the hook returns true.
    // The base `LinkedHashMap.removeEldestEntry` always returns false, and the
    // `LinkedHashSet` backing map's runtime class is exactly
    // `java/util/LinkedHashMap`, so we skip the virtual call in those cases.
    // Only genuine subclasses (LRU caches, Hibernate's
    // `BoundedConcurrentHashMap.LRU`) override it. Without this, bounded LHM
    // subclasses never evict and grow without bound — e.g.
    // `ExplicitQueryStatsMaxSizeTest` (query-plan stats trimmed at 100 entries).
    let (this_cid, this_class_name, is_plain_lhm, invoke_remove_eldest) =
        lhm_remove_eldest_hook_decision(ctx, this);
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_LHM_EVICT").is_some() {
        eprintln!(
            "[dbg-lhm-evict] this_cid={:?} class_name={:?} is_plain_lhm={} invoke_remove_eldest={} evict={}",
            this_cid,
            this_class_name,
            is_plain_lhm,
            invoke_remove_eldest,
            evict
        );
    }
    if evict && invoke_remove_eldest {
        if let Value::Object(Some(head)) = lhm_get(ctx, this, "head", LHM_FIELD_HEAD) {
            // Hand the override the REAL head node, exactly as HotSpot's
            // `LinkedHashMap.afterNodeInsertion` does
            // (`removeEldestEntry(first)` where `first = head`).
            //
            // This used to wrap the head's key/value in a fresh
            // `AbstractMap$SimpleImmutableEntry`, because the node was a fake
            // `java/util/LinkedHashMap$Node` whose class declares no methods —
            // so an override that inspects the eldest (Hibernate's
            // `LRU.removeEldestEntry` calls `eldest.getKey()`) hit a
            // `NoSuchMethodError`. `lhm_alloc_node` now mints the real
            // `java/util/LinkedHashMap$Entry`, which inherits the real
            // `HashMap$Node.getKey/getValue/setValue/equals/hashCode/toString`
            // bodies and reads them off the very slots we write, so the copy is
            // no longer needed — and the copy was itself a deviation: it was
            // immutable (`setValue` threw `UnsupportedOperationException` where
            // HotSpot mutates the map) and never `==` the live entry.
            let head_pin = ctx.pin_native_root(head);
            // Family-1 fix (cce0079): the key is re-used for the reentrant
            // remove below, across the hook's virtual dispatch (GC-capable) —
            // pin it so the remove receives the current address, not a pre-GC
            // one. Read it BEFORE the dispatch: an override that removes the
            // eldest reentrantly may leave the node unlinked by the time we
            // get back.
            let key = ctx.get_field(head, LHM_NODE_KEY);
            let eldest_key_pin = pin_value(ctx, key);
            let this = ctx.read_native_pin(this_pin, this);
            let head = ctx.read_native_pin(head_pin, head);
            // An exception from the override (or from the eviction below)
            // releases `this_pin`, and every pin above it, before it
            // propagates: the helper's contract is a release on every answer.
            let verdict = match ctx.invoke_virtual(
                this,
                "removeEldestEntry",
                "(Ljava/util/Map$Entry;)Z",
                &[Value::Object(Some(head))],
            ) {
                Ok(verdict) => verdict,
                Err(e) => {
                    ctx.unpin_native_roots(this_pin);
                    return Err(e);
                }
            };
            if matches!(verdict, Some(Value::Int(n)) if n != 0) {
                // `invoke_virtual` above runs arbitrary Java and can trigger
                // a moving GC; refresh `this` AND the eldest key before the
                // reentrant remove (Family-1 fix, cce0079).
                let this = ctx.read_native_pin(this_pin, this);
                let key = read_pinned_elem(ctx, eldest_key_pin, key);
                // Evict the eldest by key. If the override already removed it
                // reentrantly (Hibernate's LRU calls `segment.remove` ->
                // `eviction.onEntryRemove` -> `this.remove`), this is an
                // idempotent no-op; for a plain LRU cache it performs the
                // removal here.
                if let Err(e) = native_lhm_remove(ctx, &[Value::Object(Some(this)), key]) {
                    ctx.unpin_native_roots(this_pin);
                    return Err(e);
                }
            }
        }
    } else if let Some(out) = placed.as_deref_mut() {
        // No Java ran since the node was made (the caller's contract for
        // `new_node`), so it is current.
        *out = new_node;
    }

    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(None)))
}

/// `putVal`'s tree arm for a `LinkedHashMap` whose bin for `hash` is a tree
/// bin (`CRATONVM_COMPAT_LHM_TREE_BINS`), or `None`, having done nothing, when
/// it is not one. With `first_check`, the first-node test runs first (`putVal`);
/// without, the caller has walked the bin itself and found no match. Then
/// `putTreeVal`: an existing node is overwritten as `put` overwrites one
/// (`afterNodeAccess` included); a new `TreeNode` is linked into the tree and,
/// by `LinkedHashMap.newTreeNode`, at the end of the insertion-order list, and
/// is accounted as the native insert accounts one. `this_pin` roots `this`
/// and is released on every answer (see [`lhm_after_node_insertion`]).
/// `only_if_absent` is `putVal`'s: an existing node keeps a non-null value
/// (`putIfAbsent`, `CRATONVM_COMPAT_LHM_TREE_PUT_IF_ABSENT`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn lhm_tree_bin_put(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
    key: (usize, Value),
    value: (usize, Value),
    hash: i32,
    (first_check, only_if_absent): (bool, bool),
    (evict, grow, grow_on_insert): (bool, GrowAlloc, bool),
    placed: &mut Option<&mut Option<ObjectRef>>,
) -> Option<MethodCallResult> {
    let (key_pin, key_val) = key;
    let (value_pin, value_val) = value;
    let this = ctx.read_native_pin(this_pin, this);
    let (buckets, size, cap) = lhm_state(ctx, this);
    let tab = buckets?;
    let head = ctx.get_array_element(tab, map_bucket_index(hash, cap));
    let first = hm_tree_head(&*ctx, head)?;
    // `if (p.hash == hash && ((k = p.key) == key || key.equals(k))) e = p;`
    let first_pin = ctx.pin_native_root(first);
    let matched = if first_check {
        hm_node_matches_key(ctx, first_pin, first, hash, key_pin, key_val)
    } else {
        Ok(false)
    };
    let first = ctx.read_native_pin(first_pin, first);
    ctx.unpin_native_roots(first_pin);
    let existing = match matched {
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Some(Err(e));
        }
        Ok(true) => Some(first),
        Ok(false) => {
            // `else if (p instanceof TreeNode) e = ((TreeNode) p).putTreeVal(..)`
            let this = ctx.read_native_pin(this_pin, this);
            let Some(tab) = lhm_state(ctx, this).0 else {
                ctx.unpin_native_roots(this_pin);
                return Some(Ok(Some(Value::Object(None))));
            };
            let key_now = read_pinned_elem(ctx, key_pin, key_val);
            let value_now = read_pinned_elem(ctx, value_pin, value_val);
            match hm_tree_put_tree_val(ctx, first, this, tab, hash, key_now, value_now) {
                Ok(e) => e,
                Err(e) => {
                    ctx.unpin_native_roots(this_pin);
                    return Some(Err(e));
                }
            }
        }
    };
    let this = ctx.read_native_pin(this_pin, this);
    if let Some(node) = existing {
        // `if (!onlyIfAbsent || oldValue == null) e.value = value;
        // afterNodeAccess(e); return oldValue;`
        let old = ctx.get_field(node, LHM_NODE_VALUE);
        if !only_if_absent || matches!(old, Value::Object(None)) {
            let value_now = read_pinned_elem(ctx, value_pin, value_val);
            ctx.set_field(node, LHM_NODE_VALUE, value_now);
        }
        if lhm_is_access_order(ctx, this) {
            lhm_move_to_tail(ctx, this, node);
        }
        if let Some(out) = placed.as_deref_mut() {
            *out = Some(node);
        }
        ctx.unpin_native_roots(this_pin);
        return Some(Ok(Some(old)));
    }
    // `++modCount; if (++size > threshold) resize(); afterNodeInsertion(evict);`
    lhm_set(ctx, this, "size", LHM_FIELD_SIZE, Value::Int(size + 1));
    if grow_on_insert && size + 1 > lhm_grow_threshold(&*ctx, this, cap) {
        lhm_resize(ctx, this, grow);
    }
    let this = ctx.read_native_pin(this_pin, this);
    let new_node = match lhm_get(ctx, this, "tail", LHM_FIELD_TAIL) {
        Value::Object(Some(t)) => Some(t),
        _ => None,
    };
    Some(lhm_after_node_insertion(
        ctx,
        this_pin,
        this,
        evict,
        new_node,
        placed.as_deref_mut(),
    ))
}

/// `CRATONVM_COMPAT_LHM_GROW_ON_INSERT` — default ON, `=0` restores the
/// grow-first `put`. See [`native_lhm_put_evict_inner`]. Round 13 wave 11,
/// lane compat6.
pub(crate) fn lhm_grow_on_insert_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_LHM_GROW_ON_INSERT")
}

/// `CRATONVM_COMPAT_LHM_CTOR_CAPACITY` — default ON, `=0` restores the old
/// table size. `new LinkedHashMap<>(n[, lf[, accessOrder]])` is `HashMap(n,
/// lf)`: `threshold = tableSizeFor(n)`, the size the FIRST `resize()`
/// allocates. The native constructor inflated `n` by 4/3 and floored it at 16
/// (`new LinkedHashMap<>(64)` asked for 128), and the lazy table then ignored
/// even that: [`lhm_state`] read no pending size, so every `LinkedHashMap(n)`
/// started at 16 buckets. Iteration is by list, so only the table-dependent
/// counts show it: a bin of 8+ colliding keys in a map built with 64 slots
/// is treeified on HotSpot at its 9th key and was resized twice here first
/// (`R13Compat6TreeBins` lines `tree-lhm` and `tree-lhm-access`, 13 extra
/// `equals`). Round 13 wave 13, lane compat7 (proposal C6-4).
pub(crate) fn lhm_ctor_capacity_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_LHM_CTOR_CAPACITY")
}

/// `CRATONVM_COMPAT_LHM_LOAD_FACTOR` — default ON (and off whenever
/// `CRATONVM_COMPAT_MAP_LOAD_FACTOR` is), `=0` restores the dropped factor.
/// `LinkedHashMap(n, lf[, accessOrder])` and `LinkedHashSet(n, lf)` are
/// `HashMap(n, lf)`: the factor is kept in `loadFactor` and `putVal` grows at
/// `++size > threshold`, the threshold maintained by `resize()`. The natives
/// wrote 0.75 and grew every `LinkedHashMap` at `(cap * 3) / 4`, so `new
/// LinkedHashMap<>(16, 0.5f)` kept 16 buckets up to 12 entries where HotSpot
/// resizes at the 9th (table-dependent `equals` counts and treeify points; a
/// reflective `loadFactor` read). A factor of exactly 0.75 keeps the old
/// arithmetic and writes no `threshold`. Round 14 wave 1, lane compat
/// (`r13w13-compat7-collection-review-findings` item 1).
pub(crate) fn lhm_load_factor_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    map_load_factor_enabled() && compat_switch_on(&ON, "CRATONVM_COMPAT_LHM_LOAD_FACTOR")
}

/// `CRATONVM_COMPAT_LHM_CLONE_FRESH_CAPACITY` — default ON, `=0` restores the
/// copied `__capacity`. See [`clone_lhm_overlay_ctx`]. Round 14 wave 1, lane
/// compat (`r13w13-compat7-collection-review-findings` item 4).
pub(crate) fn lhm_clone_fresh_capacity_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_LHM_CLONE_FRESH_CAPACITY")
}

/// The `LinkedHashMap` counterpart of [`map_grow_threshold`]: a native insert
/// grows when `size + 1 >` this. `(cap * 3) / 4` unless the receiver carries
/// its own factor ([`lhm_load_factor_enabled`]); then the `threshold` field
/// [`lhm_resize`] maintains once a table exists (while there is none that
/// field is the pending table size, not a threshold), else the JDK's
/// recomputed value. Pure reads.
pub(crate) fn lhm_grow_threshold(ctx: &dyn NativeContext, this: ObjectRef, cap: i32) -> i32 {
    let legacy = (cap * 3) / 4;
    if !lhm_load_factor_enabled() {
        return legacy;
    }
    let Some(lf) = map_custom_load_factor(ctx, this) else {
        return legacy;
    };
    match map_threshold_field(ctx, this) {
        Some(t) if t >= 0 && lhm_state(ctx, this).0.is_some() => t,
        _ => jdk_hashmap_threshold(cap, lf),
    }
}

/// `HashMap.resize()`'s new `threshold` for a table growing from `old_cap`
/// (0: the first table) with threshold `old_thr` to `new_cap`: `oldThr << 1`
/// when the old table had at least 16 slots and the new one is below
/// `MAXIMUM_CAPACITY`, otherwise (or when that shift gives 0) the recomputed
/// `(int)(newCap * loadFactor)`. The doubling is why a custom factor's
/// threshold depends on the table's history, not only its size: 0.6 at 16
/// slots is 9, so 18 at 32 (a recomputation would say 19).
pub(crate) fn jdk_hashmap_next_threshold(old_cap: i32, old_thr: i32, new_cap: i32, lf: f32) -> i32 {
    if old_cap >= JDK_HASHMAP_MAXIMUM_CAPACITY {
        return i32::MAX;
    }
    if old_cap >= MAP_DEFAULT_CAPACITY as i32 && new_cap < JDK_HASHMAP_MAXIMUM_CAPACITY {
        let doubled = old_thr.wrapping_shl(1);
        if doubled != 0 {
            return doubled;
        }
    }
    jdk_hashmap_threshold(new_cap, lf)
}

/// `HashMap.tableSizeFor(cap)`: the smallest power of two `>= cap`, `1` for
/// `cap <= 1` (the JDK's `-1 >>> numberOfLeadingZeros(cap - 1)` is negative
/// there), and `MAXIMUM_CAPACITY` (`1 << 30`) at most.
pub(crate) fn lhm_jdk_table_size_for(cap: i32) -> i32 {
    let requested = std::cmp::max(cap, 1) as u32;
    requested
        .checked_next_power_of_two()
        .map_or(MAP_MAX_CAPACITY, |n| {
            std::cmp::min(n, MAP_MAX_CAPACITY as u32) as i32
        })
}

/// The table a `LinkedHashMap` without one allocates on its first insert
/// (`CRATONVM_COMPAT_LHM_CTOR_CAPACITY`): HotSpot's own encoding, a positive
/// `threshold` while `table == null` is `tableSizeFor(initialCapacity)` (see
/// [`map_state`], which reads it for `HashMap`); `0` (the no-argument
/// constructor, or `HashMap.reinitialize()` in a `clone()`) is the default
/// 16. Pure reads.
pub(crate) fn lhm_pending_table_size(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    if lhm_ctor_capacity_enabled() {
        let nf = ctx.object_num_fields(this);
        let pending = ctx
            .resolve_field_index_by_class_id(ctx.class_id_of_object(this), "threshold")
            .filter(|s| *s < nf)
            .map(|s| ctx.get_field(this, s));
        if let Some(Value::Int(t)) = pending {
            if t > 0 {
                return lhm_jdk_table_size_for(t);
            }
        }
    }
    MAP_DEFAULT_CAPACITY as i32
}

/// `CRATONVM_COMPAT_LHM_TREE_PUT_IF_ABSENT` — default ON, `=0` restores the
/// find-then-insert. `LinkedHashMap.putIfAbsent` is `HashMap.putVal(hash,
/// key, value, onlyIfAbsent = true, ..)`: on a TREE bin, the first-node test
/// and then ONE `putTreeVal` walk, which both finds a present key and links
/// an absent one. The native found the key with `getTreeNode` first and then
/// inserted through `putTreeVal`, walking the tree twice for an absent key
/// (and, for a present one, asking `compareTo` where `find` and `putTreeVal`
/// differ). Round 13 wave 13, lane compat7 (proposal C6-4).
pub(crate) fn lhm_tree_put_if_absent_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_LHM_TREE_PUT_IF_ABSENT")
}

/// `CRATONVM_COMPAT_LHM_REMAP_EVICT_LAST` — default ON, `=0` restores the old
/// order. A `LinkedHashMap` insert by `compute` / `computeIfAbsent` / `merge`
/// runs, on the JDK, `newNode`, the treeify of a bin that reached
/// `TREEIFY_THRESHOLD`, `++size` and only then `afterNodeInsertion(true)`
/// (`removeEldestEntry`). The native consulted the hook inside its `put`,
/// BEFORE the node moved to its bucket head and the bin was treeified, and
/// when the hook ran it skipped both (the node was no longer "placed"). Round
/// 13 wave 13, lane compat7 (proposal C6-4; the "count residue" of the
/// tree-bin page).
pub(crate) fn lhm_remap_evict_last_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_LHM_REMAP_EVICT_LAST")
}

/// `CRATONVM_COMPAT_LHM_REMAP_GROW_ON_ENTRY` — default ON, `=0` restores the
/// grow-before-link insert. `compute` / `computeIfAbsent` / `merge` resize on
/// ENTRY (`size > threshold || table == null`) and never after their insert,
/// for `LinkedHashMap` too (the bodies are `HashMap`'s); the `HashMap` route of
/// [`map_remap_jdk`] has done so since wave 9, the `LinkedHashMap` insert grew
/// through its `put`, before the link. Only the table-dependent counts (and
/// with them when a bin treeifies) see it. Round 13 wave 13, lane compat7.
pub(crate) fn lhm_remap_grow_on_entry_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_LHM_REMAP_GROW_ON_ENTRY")
}

/// `CRATONVM_COMPAT_LHM_CLEAR_IN_PLACE` — default ON, `=0` restores the
/// fresh-table `clear()`. See [`native_lhm_clear`]. Round 13 wave 13, lane
/// compat7.
pub(crate) fn lhm_clear_in_place_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_LHM_CLEAR_IN_PLACE")
}

pub(crate) fn native_lhm_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `reversed()`'s live half: rebuild from the source if it has moved. A receiver
    // that is not a reversed view is returned unchanged, at the cost of one overlay
    // lookup. See `resync_reversed_map`.
    let mut carried = [args.get(1).copied().unwrap_or(Value::Object(None))];
    let this = resync_reversed_map_carrying(ctx, this, &mut carried)?;
    let key = carried[0];
    // GC-SAFETY: `lhm_find_node` dispatches key.hashCode()/equals() (Java
    // bytecode), which can trigger a moving GC; `this` must be re-read
    // through a pin before any post-lookup use, on both the hit and miss
    // paths. Same "Family 1" pattern as `native_lhm_put_evict`.
    let this_pin = ctx.pin_native_root(this);
    let node = lhm_find_node(ctx, this, &key)?;
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    if let Some(node) = node {
        let value = ctx.get_field(node, LHM_NODE_VALUE);
        // Round-9 HIGH: access-order semantics. When the LHM was
        // constructed with `(IFZ)V` accessOrder=true, `get` must move
        // the accessed entry to the tail of the insertion-order list
        // so that iteration order reflects LRU. With insertion-order
        // (the default) this is a read-only operation.
        if lhm_is_access_order(ctx, this) {
            lhm_move_to_tail(ctx, this, node);
        }
        Ok(Some(value))
    } else {
        // Diagnostic: an enum-keyed LinkedHashMap miss is the prime suspect
        // for the Keycloak `Profile.isFeatureEnabled` NPE.
        if dbg_kcbool() {
            if let Value::Object(Some(k)) = key {
                if enum_key_identity(ctx, k).is_some() {
                    let node_keys: Vec<ObjectRef> = lhm_collect_keys(ctx, this)
                        .into_iter()
                        .filter_map(|v| match v {
                            Value::Object(Some(r)) => Some(r),
                            _ => None,
                        })
                        .collect();
                    dbg_kcbool_report_miss(ctx, "LinkedHashMap.get", k, &node_keys);
                }
            }
        }
        Ok(Some(Value::Object(None)))
    }
}

/// Move `node` to the tail of the insertion-order linked list. Used by
/// access-order LinkedHashMaps to implement LRU semantics on `get`. Pure
/// pointer surgery: unlink + relink at tail; the bucket-chain pointers
/// (`LHM_NODE_NEXT`) are untouched so lookup remains correct.
pub(crate) fn lhm_move_to_tail(ctx: &mut dyn NativeContext, this: ObjectRef, node: ObjectRef) {
    // If already at tail, no-op.
    let current_tail = lhm_get(ctx, this, "tail", LHM_FIELD_TAIL);
    if let Value::Object(Some(t)) = current_tail {
        if t.as_ptr() == node.as_ptr() {
            return;
        }
    }
    // An access-order reorder CHANGES THE ITERATION ORDER of every view over
    // this map, and HotSpot counts it as a structural modification:
    // `LinkedHashMap.afterNodeAccess` opens with `++modCount`, which is why
    // `get()` on an access-ordered map throws `ConcurrentModificationException`
    // from a live iterator there and used to be invisible here.
    //
    // Load-bearing twice over. Without it the fail-fast door is wrong for the
    // one LinkedHashMap shape that reorders, AND
    // [`view_source_generation`]'s guard would not move — so a keySet view
    // would keep serving the pre-access order for as long as no key was added
    // or removed. The no-op early-return above is what keeps this from
    // inflating the count on a `get` of the entry that is already newest;
    // HotSpot's own `afterNodeAccess` has the same `if (last != e)` guard.
    bump_map_mod_count(&*ctx, this);
    // Unlink from current position.
    lhm_unlink(ctx, this, node);
    // The unlink above clears the head/tail entries when relevant, but it
    // leaves `node.before` / `node.after` populated with stale pointers.
    // Clear them before relinking at tail so `lhm_link_tail` sees a fresh
    // node.
    ctx.set_field(node, LHM_NODE_BEFORE, Value::Object(None));
    ctx.set_field(node, LHM_NODE_AFTER, Value::Object(None));
    lhm_link_tail(ctx, this, node);
}

pub(crate) fn native_lhm_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key_val = args.get(1).copied().unwrap_or(Value::Object(None));

    // GC-SAFETY: same "Family 1" stale-ObjectRef pattern fixed in
    // `lhm_find_node`/`native_lhm_put_evict` -- `map_hash_key`/
    // `map_keys_equal` dispatch arbitrary Java code (hashCode()/equals()),
    // which can trigger a moving GC. Pin `this` and the search key up
    // front; pin/re-read `buckets`, the walked `node`, and the
    // carried-over `prev` around every such call too.
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key_val);

    let (hash, is_null, key_ref) = match key_val {
        Value::Object(Some(k)) => (map_hash_key(ctx, k)?, false, Some(k)),
        Value::Object(None) => (0, true, None),
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(None)));
        }
    };
    let mut this = ctx.read_native_pin(this_pin, this);
    let key_val = read_pinned_elem(ctx, key_pin, key_val);

    let (buckets, size, cap) = lhm_state(ctx, this);
    let mut buckets = match buckets {
        Some(b) => b,
        None => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(None)));
        }
    };
    let buckets_pin = ctx.pin_native_root(buckets);
    let idx = map_bucket_index(hash, cap);

    // `CRATONVM_COMPAT_LHM_TREE_BINS`: a tree bin is removed from by the JDK's
    // own `removeNode(hash, key, null, false, true)`, whose
    // `afterNodeRemoval` unlinks the insertion-order list.
    if lhm_tree_family(&*ctx, this)
        && hm_tree_head(&*ctx, ctx.get_array_element(buckets, idx)).is_some()
    {
        let key_now = read_pinned_elem(ctx, key_pin, key_val);
        let removed =
            hm_remove_node_bytecode(ctx, this, hash, key_now, Value::Object(None), false, true);
        let this = ctx.read_native_pin(this_pin, this);
        let old_value = match removed {
            Ok(Some(n)) => {
                let v = ctx.get_field(n, LHM_NODE_VALUE);
                lhm_sync_size_after_bytecode(ctx, this);
                v
            }
            Ok(None) => Value::Object(None),
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };
        ctx.unpin_native_roots(this_pin);
        return Ok(Some(old_value));
    }

    // Walk bucket chain, tracking prev
    let mut prev: Option<ObjectRef> = None;
    let mut prev_pin: Option<usize> = None;
    let mut node_val = ctx.get_array_element(buckets, idx);
    while let Value::Object(Some(mut node)) = node_val {
        let node_hash = match ctx.get_field(node, LHM_NODE_HASH) {
            Value::Int(node_hash) => node_hash,
            _ => 0,
        };
        let node_key = ctx.get_field(node, LHM_NODE_KEY);
        let found = if is_null {
            matches!(node_key, Value::Object(None))
        } else if node_hash == hash {
            let Value::Object(Some(nk)) = node_key else {
                node_val = ctx.get_field(node, LHM_NODE_NEXT);
                continue;
            };
            let node_pin = ctx.pin_native_root(node);
            let node_key_pin = ctx.pin_native_root(nk);
            let nk = ctx.read_native_pin(node_key_pin, nk);
            let key_cur = match read_pinned_elem(ctx, key_pin, key_val) {
                Value::Object(Some(kk)) => kk,
                _ => key_ref.unwrap(),
            };
            let eq = map_keys_equal(ctx, key_cur, nk)?;
            node = ctx.read_native_pin(node_pin, node);
            this = ctx.read_native_pin(this_pin, this);
            buckets = ctx.read_native_pin(buckets_pin, buckets);
            if let (Some(p), Some(pp)) = (prev, prev_pin) {
                prev = Some(ctx.read_native_pin(pp, p));
            }
            ctx.unpin_native_roots(node_key_pin);
            ctx.unpin_native_roots(node_pin);
            eq
        } else {
            false
        };

        if found {
            let old_value = ctx.get_field(node, LHM_NODE_VALUE);
            let next = ctx.get_field(node, LHM_NODE_NEXT);

            // Unlink from bucket chain
            if let Some(p) = prev {
                ctx.set_field(p, LHM_NODE_NEXT, next);
            } else {
                ctx.set_array_element(buckets, idx, next);
            }

            // Unlink from insertion-order list
            lhm_unlink(ctx, this, node);

            lhm_set(ctx, this, "size", LHM_FIELD_SIZE, Value::Int(size - 1));
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(old_value));
        }

        prev = Some(node);
        prev_pin = Some(ctx.pin_native_root(node));
        node_val = ctx.get_field(node, LHM_NODE_NEXT);
    }
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(None)))
}

pub(crate) fn native_lhm_contains_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    // `reversed()`'s live half: rebuild from the source if it has moved. A receiver
    // that is not a reversed view is returned unchanged, at the cost of one overlay
    // lookup. See `resync_reversed_map`.
    let mut carried = [args.get(1).copied().unwrap_or(Value::Object(None))];
    let this = resync_reversed_map_carrying(ctx, this, &mut carried)?;
    let key = carried[0];
    Ok(Some(Value::Int(
        if lhm_find_node(ctx, this, &key)?.is_some() {
            1
        } else {
            0
        },
    )))
}

pub(crate) fn native_lhm_contains_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    // `reversed()`'s live half: rebuild from the source if it has moved. A receiver
    // that is not a reversed view is returned unchanged, at the cost of one overlay
    // lookup. See `resync_reversed_map`.
    let mut carried = [args.get(1).copied().unwrap_or(Value::Object(None))];
    let this = resync_reversed_map_carrying(ctx, this, &mut carried)?;
    let target = carried[0];
    // Family-1 fix (cce0079): pinned chain walk — the per-node `equals()`
    // dispatch can move the node/target mid-scan.
    let th = pin_value(ctx, target);
    let mut target = target;
    let mut cur = lhm_get(ctx, this, "head", LHM_FIELD_HEAD);
    let mut found = false;
    while let Value::Object(Some(node)) = cur {
        let node_pin = ctx.pin_native_root(node);
        let val = ctx.get_field(node, LHM_NODE_VALUE);
        let matched = list_element_matches(ctx, &val, &target)?;
        let node = ctx.read_native_pin(node_pin, node);
        target = read_pinned_elem(ctx, th, target);
        if matched {
            ctx.unpin_native_roots(node_pin);
            found = true;
            break;
        }
        cur = ctx.get_field(node, LHM_NODE_AFTER);
        ctx.unpin_native_roots(node_pin);
    }
    if th != usize::MAX {
        ctx.unpin_native_roots(th);
    }
    Ok(Some(Value::Int(if found { 1 } else { 0 })))
}

pub(crate) fn native_lhm_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    // `CRATONVM_COMPAT_LHM_CLEAR_IN_PLACE`: `HashMap.clear()` nulls the slots
    // of the table it has (`if ((tab = table) != null && size > 0)`), keeps
    // its length and its `HashMap$Node[]` class, and allocates nothing;
    // `LinkedHashMap.clear()` then drops `head`/`tail`. The native allocated
    // a fresh untyped `Object[]` of the old length on every call (a table
    // `null` before stayed allocated), as `HashMap`'s native never did.
    if lhm_clear_in_place_enabled() {
        let (buckets, _, _) = lhm_state(ctx, this);
        if let Some(tab) = buckets {
            for i in 0..ctx.array_length(tab) {
                ctx.set_array_element(tab, i, Value::Object(None));
            }
        }
        // A size write moves `modCount` only when the size changes, which
        // is the JDK's `size > 0` test.
        lhm_set(ctx, this, "size", LHM_FIELD_SIZE, Value::Int(0));
        lhm_set(ctx, this, "head", LHM_FIELD_HEAD, Value::Object(None));
        lhm_set(ctx, this, "tail", LHM_FIELD_TAIL, Value::Object(None));
        return Ok(None);
    }
    let (_, _, cap) = lhm_state(ctx, this);
    // HIB-MAPRESIZE-STALE.1: same missed pin as `lhm_resize` — `alloc_ref_array`
    // can complete a moving young GC, and every `lhm_set` below (including the
    // overlay key, which is `this`'s identity hash) would then address a
    // pre-move copy of the map, leaving the real map uncleared and scribbling
    // on whatever now occupies the old address.
    let (this, new_buckets) = rooted_across1(ctx, this, |ctx| alloc_ref_array(ctx, cap as usize));
    lhm_set(
        ctx,
        this,
        "table",
        LHM_FIELD_BUCKETS,
        Value::Object(Some(new_buckets)),
    );
    lhm_set(ctx, this, "size", LHM_FIELD_SIZE, Value::Int(0));
    lhm_set(ctx, this, "head", LHM_FIELD_HEAD, Value::Object(None));
    lhm_set(ctx, this, "tail", LHM_FIELD_TAIL, Value::Object(None));
    Ok(None)
}

pub(crate) fn lhm_collect_keys(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<Value> {
    let mut keys = Vec::new();
    let mut cur = lhm_get(ctx, this, "head", LHM_FIELD_HEAD);
    while let Value::Object(Some(node)) = cur {
        keys.push(ctx.get_field(node, LHM_NODE_KEY));
        cur = ctx.get_field(node, LHM_NODE_AFTER);
    }
    keys
}

/// The stored `hash` of every node, in insertion order: [`lhm_collect_keys`]'
/// twin, so `result[i]` is the hash of that function's `result[i]`.
pub(crate) fn lhm_collect_hashes(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<i32> {
    let mut hashes = Vec::new();
    let mut cur = lhm_get(ctx, this, "head", LHM_FIELD_HEAD);
    while let Value::Object(Some(node)) = cur {
        hashes.push(match ctx.get_field(node, LHM_NODE_HASH) {
            Value::Int(h) => h,
            _ => 0,
        });
        cur = ctx.get_field(node, LHM_NODE_AFTER);
    }
    hashes
}

/// `CRATONVM_COMPAT_LHS_NODE_OPS` — default ON, `=0` restores the old
/// counts. `LinkedHashSet.removeFirst()` / `removeLast()` remove the end NODE
/// and `reversed()` is a view: none of them calls an element's `hashCode()`
/// or `equals()` on the JDK. The natives removed by element (a `hashCode()`
/// and a chain walk of `equals`) and filled the reversed snapshot with `add`s
/// (a `hashCode()` per element, and `equals` against every same-hash element
/// already added). Round 13 wave 8, lane hashcompat3 (`R13ShadowHashSetFamily`
/// scenario 5: 26 `hashCode()` calls on the first run where HotSpot makes 8,
/// and a different count on every later run, the `drift 20000`).
pub(crate) fn lhs_node_ops_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_LHS_NODE_OPS")
}

/// `CRATONVM_COMPAT_LHM_REVERSED_BY_NODE` — default ON, `=0` restores the
/// old rebuild. `LinkedHashMap.reversed()` is a view on the JDK and calls no
/// key's `hashCode()` or `equals()`; the native snapshot was (re)built with a
/// `put` per entry, one `hashCode()` each (and `equals` against every
/// same-hash key already copied), on every rebuild. It is now filled from the
/// source nodes' stored hashes ([`lhm_append_distinct`]), as
/// `LinkedHashSet.reversed()` has been since wave 8. Round 13 wave 9, lane
/// hashcompat4 (residual page item 7).
pub(crate) fn lhm_reversed_by_node_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_LHM_REVERSED_BY_NODE")
}

/// Append `key -> value` to a `LinkedHashMap` known NOT to hold `key`, filed
/// under `hash` (the stored hash of the node it is copied from): the insert
/// half of [`native_lhm_put_evict_hashed`] without its `hashCode()` and its
/// duplicate search, for rebuilding a snapshot from another map's nodes. The
/// receiver must be an exact `LinkedHashMap` (no `removeEldestEntry` hook runs).
pub(crate) fn lhm_append_distinct(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key: Value,
    value: Value,
    hash: i32,
) -> Result<(), MethodCallFailed> {
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let value_pin = pin_value(ctx, value);
    // Every pin above `this_pin` is released with it, on every exit.
    let result = (|| -> Result<(), MethodCallFailed> {
        let this = ctx.read_native_pin(this_pin, this);
        let (initial_buckets, size, cap) = lhm_state(ctx, this);
        if initial_buckets.is_none() || size + 1 > lhm_grow_threshold(&*ctx, this, cap) {
            lhm_resize(ctx, this, GrowAlloc::SingleAttempt);
        }
        let this = ctx.read_native_pin(this_pin, this);
        let (buckets, size, cap) = lhm_state(ctx, this);
        let Some(buckets) = buckets else {
            return Ok(());
        };
        let idx = map_bucket_index(hash, cap);
        // Never link a plain node into a tree bin (`CRATONVM_COMPAT_LHM_TREE_BINS`):
        // a snapshot this rebuilds is not treeified, so this is a safety net.
        let head = ctx.get_array_element(buckets, idx);
        if lhm_tree_family(&*ctx, this) && hm_tree_head(&*ctx, head).is_some() {
            untreeify_bin_at(ctx, this, buckets, idx)?;
        }
        let this = ctx.read_native_pin(this_pin, this);
        let Some(buckets) = lhm_state(ctx, this).0 else {
            return Ok(());
        };
        let buckets_pin = ctx.pin_native_root(buckets);
        let key = read_pinned_elem(ctx, key_pin, key);
        let value = read_pinned_elem(ctx, value_pin, value);
        let node = lhm_alloc_node(ctx, key, value, hash)?;
        let this = ctx.read_native_pin(this_pin, this);
        let buckets = ctx.read_native_pin(buckets_pin, buckets);
        lhm_link_into_bucket(
            ctx,
            buckets,
            idx,
            node,
            lhm_bucket_order_enabled(),
            chain_walk_limit(size),
        );
        lhm_link_tail(ctx, this, node);
        lhm_set(ctx, this, "size", LHM_FIELD_SIZE, Value::Int(size + 1));
        Ok(())
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

/// [`lhm_append_distinct`]'s `HashMap` twin, for a SET backing: link `key`
/// (its own PRESENT marker, see [`present_marker`]) at the tail of the chain
/// of `hash` -- the stored hash of the node it is copied from -- without a
/// `hashCode()` and without the duplicate search, then account for it as
/// `putVal` does (`size + 1`, one `modCount` bump). The receiver must be a
/// view backing this crate allocated (no overlay row, the plain node layout)
/// known NOT to hold `key`. Grows first when the insert would pass the
/// threshold, which for a distinct key is where `putVal` grows too.
pub(crate) fn map_append_distinct(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key: ObjectRef,
    hash: i32,
) -> Result<(), MethodCallFailed> {
    let this_pin = ctx.pin_native_root(this);
    let key_pin = ctx.pin_native_root(key);
    // Every pin above `this_pin` is released with it, on every exit.
    let result = (|| -> Result<(), MethodCallFailed> {
        let mut this = ctx.read_native_pin(this_pin, this);
        let (initial, size, cap) = map_state(&*ctx, this);
        if initial.is_none() || size + 1 > map_grow_threshold(&*ctx, this, cap) {
            map_resize(ctx, &mut this, GrowAlloc::SingleAttempt);
        }
        let this = ctx.read_native_pin(this_pin, this);
        let (buckets, _, cap) = map_state(&*ctx, this);
        let Some(buckets) = buckets else {
            return Ok(());
        };
        let idx = map_bucket_index(hash, cap);
        // Never link a plain node into a tree bin
        // (`CRATONVM_COMPAT_HASHMAP_TREE_BINS`): a view backing does not
        // treeify ([`hm_tree_may_treeify`]), so this is a safety net.
        let head = ctx.get_array_element(buckets, idx);
        if hm_tree_family(&*ctx, this) && hm_tree_head(&*ctx, head).is_some() {
            hm_untreeify_bin(ctx, this, idx)?;
        }
        let this = ctx.read_native_pin(this_pin, this);
        let (buckets, size, cap) = map_state(&*ctx, this);
        let Some(buckets) = buckets else {
            return Ok(());
        };
        // No Java and no allocation in this walk.
        let mut tail: Option<ObjectRef> = None;
        let mut cur = ctx.get_array_element(buckets, idx);
        let mut steps: usize = 0;
        while let Value::Object(Some(n)) = cur {
            steps += 1;
            if steps > chain_walk_limit(size) {
                return Ok(());
            }
            tail = Some(n);
            cur = ctx.get_field(n, NODE_FIELD_NEXT);
        }
        let buckets_pin = ctx.pin_native_root(buckets);
        let tail_pin = tail.map(|t| ctx.pin_native_root(t));
        let key = ctx.read_native_pin(key_pin, key);
        let node = map_alloc_node(ctx, key, Value::Object(Some(key)), hash, None)?;
        match (tail, tail_pin) {
            (Some(t), Some(tp)) => {
                let t = ctx.read_native_pin(tp, t);
                ctx.set_field(t, NODE_FIELD_NEXT, Value::Object(Some(node)));
            }
            _ => {
                let buckets = ctx.read_native_pin(buckets_pin, buckets);
                ctx.set_array_element(buckets, idx, Value::Object(Some(node)));
            }
        }
        // The reference store above can run a write-barrier allocation.
        let this = ctx.read_native_pin(this_pin, this);
        set_map_size(ctx, this, size + 1);
        let this = ctx.read_native_pin(this_pin, this);
        bump_map_mod_count(ctx, this);
        Ok(())
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

/// `CRATONVM_COMPAT_VIEW_BUILD_BY_NODE` — default ON, `=0` restores the
/// `put` per key. `HashMap.keySet()` / `LinkedHashMap.keySet()` allocate a
/// view and nothing else on the JDK: no key's `hashCode()` or `equals()` is
/// called, neither when the view is taken nor when it is read after the map
/// changed. The native view is a `HashSet` snapshot of the source's keys,
/// built (and rebuilt after every structural change) with a `put` per key: a
/// `hashCode()` each, and `equals` against every same-hash key already
/// copied. For a source whose nodes this crate can walk
/// ([`plain_node_map`]), the snapshot is now filled from the nodes' STORED
/// hashes ([`map_append_distinct`]). Round 13 wave 10, lane compat5
/// (`r13w9-hashcompat4-map-views-call-hashcode-FIXED-20260928.md` item 1).
pub(crate) fn view_build_by_node_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_VIEW_BUILD_BY_NODE")
}

/// Whether `map` keeps every mapping in `HashMap` / `LinkedHashMap` nodes this
/// crate can walk: a `LinkedHashMap` (or subclass), or a `HashMap` (or
/// subclass) on the plain bucket layout with no integer-overlay row -- not a
/// view backing, CHM, `TreeMap`, `IdentityHashMap`, unmodifiable wrapper or
/// `Hashtable`/`Properties`. The shape [`hs_backing_nodes`] walks. Pure reads.
pub(crate) fn plain_node_map(ctx: &dyn NativeContext, map: ObjectRef) -> bool {
    if view_backing_source(ctx, map).is_some() {
        return false;
    }
    if is_lhm_receiver(ctx, map) {
        return true;
    }
    let facts = receiver_facts(ctx, map);
    facts.has(CF_BUCKET_MAP_NAME)
        && !facts.has(CF_CHM)
        && !facts.has(CF_TREE_MAP)
        && !facts.has(CF_IDENTITY_MAP)
        && !facts.has(CF_UNMOD_WRAPPER)
        && !facts.has(CF_HASHTABLE_ANCESTRY)
        && hm_int_fast_len(ctx, map).is_none()
}

/// The `size` of a map [`plain_node_map`] accepts, read through the layout
/// its family keeps it in. Pure read.
pub(crate) fn node_map_size(ctx: &dyn NativeContext, map: ObjectRef) -> i32 {
    if is_lhm_receiver(ctx, map) {
        lhm_state(ctx, map).1
    } else {
        map_state(ctx, map).1
    }
}

/// `CRATONVM_COMPAT_VIEW_REMOVE_BY_NODE` — default ON, `=0` restores the
/// by-key write-through. On the JDK a removal through a `HashMap` /
/// `LinkedHashMap` `keySet()` or `entrySet()` view is `removeNode` on the
/// map: `keySet().remove(k)` and `entrySet().remove(e)` hash the key ONCE,
/// and a view iterator's `remove()` hashes nothing (`removeNode(p.hash, ..)`).
/// The natives removed from the view's snapshot and then from the source by
/// key (two `hashCode()` calls), ran `containsKey` + `get` + `remove` for an
/// entry (three), and removed by key again from a view iterator. For a source
/// [`plain_node_map`] accepts they now take the JDK's shape. Round 13 wave 10,
/// lane compat5 (`r13w9-hashcompat4-map-views-call-hashcode-FIXED-20260928.md`
/// item 2).
pub(crate) fn view_remove_by_node_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_VIEW_REMOVE_BY_NODE")
}

/// `CRATONVM_COMPAT_ENTRY_SET_NODES` — default ON, `=0` restores the
/// per-read copies. On the JDK a `HashMap` / `LinkedHashMap` `entrySet()`
/// element IS the map's node: `getValue()` sees a later `put` of its key,
/// `setValue` writes the node with no lookup (no `hashCode()`), and two walks
/// hand out the same objects. The natives minted a fresh live copy per pair
/// on every read (`alloc_live_entry`), so a held entry answered its OLD value
/// after `m.put(e.getKey(), v)` and `setValue` found its node by the key's
/// hash. The table nodes are real `HashMap$Node` / `LinkedHashMap$Entry`
/// objects at their declared width, which `Node.setValue`'s forced native
/// ([`native_entry_set_value`]) writes in place and whose `getKey` /
/// `getValue` / `equals` / `hashCode` / `toString` are the JDK's own
/// bytecode, and a resize relinks them rather than copying, so they are now
/// handed out directly ([`entry_view_nodes`]). Round 13 wave 10, lane compat5
/// (`jit-r13-hashcompat4-proposals-RETIRED-20260929.md` HC4-2;
/// `r13w9-hashcompat4-map-views-call-hashcode-FIXED-20260928.md` item 3).
pub(crate) fn entry_set_nodes_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_ENTRY_SET_NODES")
}

/// The nodes of `source` in its iteration order, to be handed out as its
/// `entrySet()` elements, or `None` (switch off, a source
/// [`plain_node_map`] refuses, or any node that is not the family's real
/// entry class at its declared width -- a primitive-valued carrier, a live
/// copy -- which keeps the copies). Pure reads.
pub(crate) fn entry_view_nodes(ctx: &dyn NativeContext, source: ObjectRef) -> Option<Vec<ObjectRef>> {
    if !entry_set_nodes_enabled() || !plain_node_map(ctx, source) {
        return None;
    }
    let (nodes, lhm) = hs_backing_nodes(ctx, source)?;
    let want = if lhm {
        "java/util/LinkedHashMap$Entry"
    } else {
        HM_NODE_CLASS
    };
    let want_cid = ctx.class_id_by_name(want)?;
    let width = ctx.class_num_total_fields(want_cid);
    if width == 0 {
        return None;
    }
    // A `HashMap` tree bin's nodes are `HashMap$TreeNode`s, the entries the
    // JDK hands out for them (`CRATONVM_COMPAT_HASHMAP_TREE_BINS`).
    // (`entry_slots` knows a `TreeNode`'s slots only under the switch.) A
    // `LinkedHashMap` tree bin's nodes are `TreeNode`s too.
    let tree_cid = if hm_tree_bins_enabled() && (!lhm || lhm_tree_bins_enabled()) {
        ctx.class_id_by_name(HM_TREE_NODE_CLASS)
    } else {
        None
    };
    let tree_width = tree_cid.map_or(0, |cid| ctx.class_num_total_fields(cid));
    for n in &nodes {
        let cid = ctx.class_id_of_object(*n);
        let ok = (cid == want_cid && ctx.object_num_fields(*n) == width)
            || (Some(cid) == tree_cid
                && tree_width != 0
                && ctx.object_num_fields(*n) == tree_width);
        if !ok {
            return None;
        }
    }
    Some(nodes)
}

/// The keys of `source` in its iteration order beside each one's STORED node
/// hash, or `None` (under `CRATONVM_COMPAT_VIEW_BUILD_BY_NODE` off, or for a
/// source [`plain_node_map`] refuses). Pure reads.
pub(crate) fn view_source_keys_hashed(
    ctx: &dyn NativeContext,
    source: ObjectRef,
) -> Option<(Vec<Value>, Vec<i32>)> {
    if !view_build_by_node_enabled() || !plain_node_map(ctx, source) {
        return None;
    }
    let (nodes, lhm) = hs_backing_nodes(ctx, source)?;
    let keys: Vec<Value> = nodes
        .iter()
        .map(|n| either_node_key(ctx, *n, lhm))
        .collect();
    let hashes: Vec<i32> = nodes
        .iter()
        .map(|n| either_node_hash(ctx, *n, lhm))
        .collect();
    Some((keys, hashes))
}

/// Fill a keySet view `backing` with `keys` (a source's distinct keys, each
/// beside its stored hash): [`map_append_distinct`] per non-null key, the
/// ordinary `put` for the null key (whose hash is `0` and calls nothing).
/// `key_pins[i]` roots `keys[i]`; the caller releases the pins.
pub(crate) fn fill_view_backing_hashed(
    ctx: &mut dyn NativeContext,
    backing_pin: usize,
    backing: ObjectRef,
    keys: &[Value],
    key_pins: &[usize],
    hashes: &[i32],
) -> Result<(), MethodCallFailed> {
    for (i, k) in keys.iter().enumerate() {
        let backing = ctx.read_native_pin(backing_pin, backing);
        match read_pinned_elem(ctx, key_pins[i], *k) {
            Value::Object(Some(key)) => map_append_distinct(ctx, backing, key, hashes[i])?,
            k => {
                native_map_put(ctx, &[Value::Object(Some(backing)), k, present_marker(k)])?;
            }
        }
    }
    Ok(())
}

pub(crate) fn lhm_collect_values(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<Value> {
    let mut vals = Vec::new();
    let mut cur = lhm_get(ctx, this, "head", LHM_FIELD_HEAD);
    while let Value::Object(Some(node)) = cur {
        vals.push(ctx.get_field(node, LHM_NODE_VALUE));
        cur = ctx.get_field(node, LHM_NODE_AFTER);
    }
    vals
}

pub(crate) fn native_lhm_key_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `reversed()`'s live half: rebuild from the source if it has moved. A receiver
    // that is not a reversed view is returned unchanged, at the cost of one overlay
    // lookup. See `resync_reversed_map`.
    let mut this = resync_reversed_map(ctx, this)?;
    // See `native_map_key_set`: a live view can be handed out again rather than
    // rebuilt, and this is the receiver the workload that motivated it uses.
    if let Some(cached) = cached_live_view(ctx, this, VIEW_KIND_KEYSET) {
        return Ok(Some(Value::Object(Some(cached))));
    }
    MAP_VIEW_BUILT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Live view: the returned set carries `this` as the source map so
    // `keySet().remove` / `iterator().remove` write through (the propagation
    // routes through LinkedHashMap.remove via virtual dispatch).
    let this_pin = ctx.pin_native_root(this);
    let keys = lhm_collect_keys(ctx, this);
    let set = make_view_set_of(ctx, &mut this, VIEW_KIND_KEYSET, &keys)?;
    let this = ctx.read_native_pin(this_pin, this);
    store_live_view(ctx, this, VIEW_KIND_KEYSET, set);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(set))))
}

pub(crate) fn native_lhm_values(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `reversed()`'s live half: rebuild from the source if it has moved. A receiver
    // that is not a reversed view is returned unchanged, at the cost of one overlay
    // lookup. See `resync_reversed_map`.
    let this = resync_reversed_map(ctx, this)?;
    if let Some(cached) = cached_live_values_view(ctx, this) {
        return Ok(Some(Value::Object(Some(cached))));
    }
    let vals = lhm_collect_values(ctx, this);
    let carrier = values_carrier_for(&*ctx, this);
    let list = make_view_list_of(ctx, this, &vals, carrier)?;
    if let Some(src) = values_view_source(&*ctx, list) {
        store_live_values_view(ctx, src, list);
    }
    Ok(Some(Value::Object(Some(list))))
}

pub(crate) fn native_lhm_entry_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `reversed()`'s live half: rebuild from the source if it has moved. A receiver
    // that is not a reversed view is returned unchanged, at the cost of one overlay
    // lookup. See `resync_reversed_map`.
    let this = resync_reversed_map(ctx, this)?;
    // `LinkedHashMap` has its OWN entrySet native, so the cache the HashMap
    // twin got does not reach it -- the same fourth-mint-site asymmetry the
    // `store_set_view_backref` note below records, and the reason it is worth
    // repeating: `probes/MapViewCacheProbe` read `lhm.ident.entrySet=false`
    // against HotSpot's `true` while every other family answered `true`. A
    // LinkedHashMap is the source type in the workload this cache exists for,
    // and each miss allocates one `Map$Entry` per entry.
    if let Some(cached) = cached_live_view(ctx, this, VIEW_KIND_ENTRYSET) {
        return Ok(Some(Value::Object(Some(cached))));
    }
    // Collect (key, value) pairs in insertion order.
    let mut pairs = Vec::new();
    let mut cur = lhm_get(ctx, this, "head", LHM_FIELD_HEAD);
    while let Value::Object(Some(node)) = cur {
        let key = ctx.get_field(node, LHM_NODE_KEY);
        let val = ctx.get_field(node, LHM_NODE_VALUE);
        pairs.push((key, val));
        cur = ctx.get_field(node, LHM_NODE_AFTER);
    }
    // Build the entrySet view backing keyed by the entry objects' IDENTITY hash,
    // exactly like `native_map_entry_set` (HashMap). The previous
    // `make_view_set_of` path hashed each entry via its Java `hashCode` (=
    // key.hashCode ^ value.hashCode), calling the VALUE's `hashCode` at
    // construction. For a `LinkedHashMap<…, PersistentCollection>` that ran
    // `PersistentSet.hashCode` -> `read` -> lazy init during
    // `BatchFetchQueue.collectBatchLoadableCollectionKeys`, recursing into
    // another batch load -> `StackOverflowError` (HotSpot never hashes entries
    // while building/iterating `entrySet()`). Live removal/`setValue` still work
    // via the entrySet view-backing (`VIEW_KIND_ENTRYSET` + 3-field entries).
    // GC-safety: this builds the view with N+2 allocations in a loop while
    // holding `this`, `set`, `backing_map` and every collected key/value in
    // bare Rust locals — none of which is a GC root, so each allocation could
    // leave the next store writing through a pre-move address (and could
    // reclaim the freshly built `set`/`backing_map` outright, since nothing on
    // the heap references them yet). Root them all for the whole build.
    let mut this = this;
    let flat: Vec<Value> = pairs.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (flat_base, flat_pins) = pin_value_slice(ctx, &flat);
    let carrier = set_view_carrier_for(&*ctx, this, VIEW_KIND_ENTRYSET);
    let mut set = rooted_across(ctx, &mut [&mut this], |ctx| {
        alloc_set_view_carrier(ctx, carrier)
    })?;
    let cap = std::cmp::max(pairs.len().next_power_of_two(), MAP_DEFAULT_CAPACITY);
    let mut this_at_call = this;
    let mut backing_map = rooted_across(ctx, &mut [&mut this, &mut set], |ctx| {
        alloc_view_backing(ctx, &mut this_at_call, VIEW_KIND_ENTRYSET, cap)
    })?;
    hs_set_backing_map(ctx, set, backing_map);
    // `LinkedHashMap` has its OWN entrySet native — it does not reach
    // `native_map_entry_set` or `make_view_set_of` — so the declared
    // enclosing-instance field needs storing here as well. MEASURED: with the
    // other three mint sites fixed, `LinkedHashMap.entrySet()` was the ONE
    // carrier still reporting `this$0 = NULL` against HotSpot's
    // `java.util.LinkedHashMap`, while its own `keySet()` and `values()` were
    // already right. A fourth mint site is exactly the shape `H23`'s
    // three-of-four table typing had; the lesson is the same one.
    store_set_view_backref(ctx, set, this);
    for i in 0..pairs.len() {
        // `AbstractMap$SimpleEntry`, not the interface `Map$Entry` — HotSpot
        // cannot produce an instance of an interface (`H16-2` N2).
        let mut entry_obj: ObjectRef =
            rooted_across(ctx, &mut [&mut this, &mut set, &mut backing_map], |ctx| {
                try_alloc_synthetic(ctx, "java/util/AbstractMap$SimpleEntry", 3)
            })?;
        ctx.set_field(
            entry_obj,
            0,
            read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]),
        );
        ctx.set_field(
            entry_obj,
            1,
            read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]),
        );
        ctx.set_field(entry_obj, 2, Value::Object(Some(this)));
        let hash = ctx.identity_hash_code(entry_obj);
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
                // gc-common w20-a: the resize allocates, and the receiver, the
                // view and the entry were carried across it raw (only the
                // backing map is rewritten by `map_resize` itself).
                rooted_across(ctx, &mut [&mut this, &mut set, &mut entry_obj], |ctx| {
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
        // `map_alloc_node` allocates: the bucket array and the map itself must
        // survive it, and the entry has to come back at its post-move address.
        let mut bucket_arr: ObjectRef = b;
        let entry_at_call = entry_obj;
        let head_at_call = head;
        let node = rooted_across(
            ctx,
            &mut [
                &mut this,
                &mut set,
                &mut backing_map,
                &mut bucket_arr,
                &mut entry_obj,
            ],
            |ctx| map_alloc_node(ctx, entry_at_call, sentinel, hash, head_at_call),
        )?;
        ctx.set_array_element(bucket_arr, idx, Value::Object(Some(node)));
        set_map_size(ctx, backing_map, size + 1);
    }
    if flat_base != usize::MAX {
        ctx.unpin_native_roots(flat_base);
    }
    // `this` was carried through `rooted_across` above, so it is current here.
    store_live_view(ctx, this, VIEW_KIND_ENTRYSET, set);
    Ok(Some(Value::Object(Some(set))))
}

pub(crate) fn native_lhm_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(Some(ctx.create_string("{}"))))),
    };
    // `reversed()`'s live half: rebuild from the source if it has moved. A receiver
    // that is not a reversed view is returned unchanged, at the cost of one overlay
    // lookup. See `resync_reversed_map`.
    let this = resync_reversed_map(ctx, this)?;
    // gc-common w20-a: collect the rows first (the walk does not allocate),
    // then render them through pins. Rendering inside the walk ran `toString()`
    // while `node` was held raw, and then read the node's value and successor
    // through its pre-move address.
    let mut rows = Vec::new();
    let mut cur = lhm_get(ctx, this, "head", LHM_FIELD_HEAD);
    while let Value::Object(Some(node)) = cur {
        let key = ctx.get_field(node, LHM_NODE_KEY);
        let val = ctx.get_field(node, LHM_NODE_VALUE);
        rows.push((key, val));
        cur = ctx.get_field(node, LHM_NODE_AFTER);
    }
    let parts: Vec<Units> = display_entry_units_pinned(ctx, &rows)?
        .iter()
        .map(|(ks, vs)| entry_units(ks, vs))
        .collect();
    let s = wrap_join("{", &parts, ", ", "}");
    Ok(Some(Value::Object(Some(ctx.create_string_from_units(&s)))))
}

pub(crate) fn native_lhm_get_or_default(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `reversed()`'s live half: rebuild from the source if it has moved. A receiver
    // that is not a reversed view is returned unchanged, at the cost of one overlay
    // lookup. See `resync_reversed_map`.
    let mut carried = [
        args.get(1).copied().unwrap_or(Value::Object(None)),
        args.get(2).copied().unwrap_or(Value::Object(None)),
    ];
    let this = resync_reversed_map_carrying(ctx, this, &mut carried)?;
    let [key, default] = carried;
    // GC-SAFETY: `lhm_find_node` dispatches key.hashCode()/equals() (Java
    // bytecode), which can trigger a moving GC; `this` must be re-read
    // through a pin before any post-lookup use. Same "Family 1" pattern as
    // `native_lhm_get`/`native_lhm_put_evict`.
    // cceres5: the DEFAULT value likewise sat raw across the lookup — the
    // absent-key branch returned a pre-move address (see
    // native_map_get_or_default's live capture).
    let this_pin = ctx.pin_native_root(this);
    let default_pin = pin_value(ctx, default);
    let node = lhm_find_node(ctx, this, &key)?;
    let this = ctx.read_native_pin(this_pin, this);
    let default = read_pinned_elem(ctx, default_pin, default);
    ctx.unpin_native_roots(this_pin);
    if let Some(node) = node {
        let value = ctx.get_field(node, LHM_NODE_VALUE);
        // Round-9 HIGH: `getOrDefault` is also an access for access-order
        // semantics — same reorder as `get`. The JDK's `LinkedHashMap`
        // explicitly documents this in `Map.getOrDefault`'s contract.
        if lhm_is_access_order(ctx, this) {
            lhm_move_to_tail(ctx, this, node);
        }
        Ok(Some(value))
    } else {
        Ok(Some(default))
    }
}

pub(crate) fn native_lhm_put_if_absent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    // GC-SAFETY: `lhm_find_node` dispatches the key's `hashCode`/`equals`, so
    // `this` is re-read through a pin before any post-lookup use.
    //
    // gen r4w4/rooting2: so are the key and value, because both insert arms
    // below used to forward the ORIGINAL `args` to `native_lhm_put` -- the
    // pre-lookup receiver, key and value, which the funnel never rewrites.
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let value_pin = pin_value(ctx, value);
    // `CRATONVM_COMPAT_MAP_SINGLE_WALK`: `putIfAbsent` is `putVal(hash(key),
    // .., onlyIfAbsent = true)` -- ONE `hashCode()` and one chain walk. The
    // composite below looked the key up and then `put` it, hashing and walking
    // again. With the hash taken here, the lookup reuses it and an absent key
    // is linked without a second walk (`known_absent`).
    let single_hash = match key {
        Value::Object(Some(k)) if map_single_walk_enabled() => match map_hash_key(ctx, k) {
            Ok(h) => Some(h),
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        },
        _ => None,
    };
    // `CRATONVM_COMPAT_LHM_TREE_PUT_IF_ABSENT`: a TREE bin takes `putVal`'s
    // tree arm with `onlyIfAbsent` -- the first-node test, then one
    // `putTreeVal` that both finds a present key and links an absent one --
    // instead of the find below followed by a second walk in `putTreeVal`.
    // `lhm_tree_bin_put` answers `None`, having done nothing, for a bin that
    // is not a tree, and releases `this_pin` (and the pins above it) on every
    // other answer. Only a reference value takes it (a Java caller always
    // passes one; an internal primitive sentinel is boxed by the put below).
    if let Some(hash) = single_hash {
        let this = ctx.read_native_pin(this_pin, this);
        let key_now = read_pinned_elem(ctx, key_pin, key);
        let value_now = read_pinned_elem(ctx, value_pin, value);
        if lhm_tree_put_if_absent_enabled()
            && matches!(value_now, Value::Object(_))
            && lhm_tree_family(&*ctx, this)
        {
            let mut no_placed: Option<&mut Option<ObjectRef>> = None;
            if let Some(result) = lhm_tree_bin_put(
                ctx,
                this_pin,
                this,
                (key_pin, key_now),
                (value_pin, value_now),
                hash,
                (true, true),
                (true, GrowAlloc::SingleAttempt, lhm_grow_on_insert_enabled()),
                &mut no_placed,
            ) {
                return result;
            }
        }
    }
    let this = ctx.read_native_pin(this_pin, this);
    let key_now = read_pinned_elem(ctx, key_pin, key);
    let found = match lhm_find_node_hashed(ctx, this, &key_now, single_hash) {
        Ok(found) => found,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let this = ctx.read_native_pin(this_pin, this);
    let fresh_args = [
        Value::Object(Some(this)),
        read_pinned_elem(ctx, key_pin, key),
        read_pinned_elem(ctx, value_pin, value),
    ];
    ctx.unpin_native_roots(this_pin);
    if let Some(node) = found {
        let existing = ctx.get_field(node, LHM_NODE_VALUE);
        // Map.putIfAbsent contract: a key "present with null value" counts as
        // absent — associate the new value and return the old (null). Returning
        // the null existing without storing left the mapping null
        // (LinkedCaseInsensitiveMapTests.computeIfAbsentWithExistingValue).
        if matches!(existing, Value::Object(None)) {
            if single_hash.is_some() && matches!(fresh_args[2], Value::Object(_)) {
                // `putVal`: `e.value = value; afterNodeAccess(e)` on the node
                // the walk found, no second lookup. No Java has run since.
                ctx.set_field(node, LHM_NODE_VALUE, fresh_args[2]);
                if lhm_is_access_order(ctx, this) {
                    lhm_move_to_tail(ctx, this, node);
                }
                return Ok(Some(Value::Object(None)));
            }
            return native_lhm_put(ctx, &fresh_args);
        }
        // ... and a present key IS AN ACCESS. The JDK routes `putIfAbsent`
        // through `putVal(.., onlyIfAbsent = true)`, which calls
        // `afterNodeAccess(e)` on the existing node before returning it -- so in
        // ACCESS order the key moves to the tail even though nothing was
        // stored. MEASURED `{c=3, a=1, b=22}` against HotSpot's
        // `{a=1, b=22, c=3}` (apps/probes/LinkedSequencedShadowSweep 42); every later
        // row of an LRU sequence inherits the wrong order from that one.
        if lhm_is_access_order(ctx, this) {
            lhm_move_to_tail(ctx, this, node);
        }
        Ok(Some(existing))
    } else if single_hash.is_some() {
        native_lhm_put_evict_hashed(
            ctx,
            &fresh_args,
            true,
            GrowAlloc::SingleAttempt,
            single_hash,
            None,
            true,
        )
    } else {
        native_lhm_put(ctx, &fresh_args)
    }
}

pub(crate) fn native_lhm_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    // `reversed()`'s live half: rebuild from the source if it has moved. A receiver
    // that is not a reversed view is returned unchanged, at the cost of one overlay
    // lookup. See `resync_reversed_map`.
    let this = resync_reversed_map(ctx, this)?;
    // RULE F, empty receiver included. See `reject_null_functional`.
    reject_null_functional(args.get(1))?;
    let consumer = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    // `LinkedHashMap.forEach`'s `int mc = modCount` and its after-the-loop
    // check (`CRATONVM_COMPAT_MAP_REMAP_CME`, see `map_for_each_mod_count`).
    // `this_pin` is below `consumer_pin`, so releasing it releases both.
    let mod_before = map_for_each_mod_count(&*ctx, this);
    let this_pin = ctx.pin_native_root(this);
    let consumer_pin = ctx.pin_native_root(consumer);
    let mut cur = lhm_get(ctx, this, "head", LHM_FIELD_HEAD);
    while let Value::Object(Some(node)) = cur {
        let node_pin = ctx.pin_native_root(node);
        let key = ctx.get_field(node, LHM_NODE_KEY);
        let val = ctx.get_field(node, LHM_NODE_VALUE);
        let key_pin = match key {
            Value::Object(Some(o)) => ctx.pin_native_root(o),
            _ => usize::MAX,
        };
        let val_pin = match val {
            Value::Object(Some(o)) => ctx.pin_native_root(o),
            _ => usize::MAX,
        };
        let consumer = ctx.read_native_pin(consumer_pin, consumer);
        let key = read_pinned_elem(ctx, key_pin, key);
        let val = read_pinned_elem(ctx, val_pin, val);
        if let Err(e) = ctx.invoke_virtual(
            consumer,
            "accept",
            "(Ljava/lang/Object;Ljava/lang/Object;)V",
            &[key, val],
        ) {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
        let node = ctx.read_native_pin(node_pin, node);
        cur = ctx.get_field(node, LHM_NODE_AFTER);
        ctx.unpin_native_roots(node_pin);
    }
    let moved = map_remap_moved(&*ctx, ctx.read_native_pin(this_pin, this), mod_before);
    ctx.unpin_native_roots(this_pin);
    if moved {
        return Err(RuntimeError::ConcurrentModificationException.into());
    }
    Ok(None)
}

pub(crate) fn native_lhm_put_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lhm_put_all_impl(ctx, args, true)
}

/// `putMapEntries(m, evict)` into a `LinkedHashMap`: `evict` is `putAll`'s
/// `true` or the copy constructor's `false` ([`lhm_copy_no_evict_enabled`]),
/// which decides whether `removeEldestEntry` is asked after each insert.
pub(crate) fn lhm_put_all_impl(ctx: &mut dyn NativeContext, args: &[Value], evict: bool) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    // G1-1 RECONCILED (retirement pass): `LinkedHashMap` does not override
    // `putAll` in the JDK, so a null source is the inherited
    // `HashMap.putAll`'s `int s = m.size();` NPE — MEASURED identical to the
    // `HashMap`/`Properties` row (`native_map_put_all`'s sibling literal).
    // This body predates that fix and, being a separate per-class copy
    // (`register_linkedhashmap_natives`, not the shared generic native),
    // never got it: a null source fell through the `Some(Object(Some(_)))`
    // match to the malformed-call `Ok(None)` arm and silently no-op'd.
    if matches!(args.get(1), Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException {
            message: Some(
                "Cannot invoke \"java.util.Map.size()\" because \"m\" is null".to_string(),
            ),
        }
        .into());
    }
    let source = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };

    // Collect entries from the source, dispatching on its concrete backend
    // (LinkedHashMap overlay, TreeMap tree, HashMap/CHM buckets, or — via the
    // `entrySet().iterator()` fallback — any other `Map`). Earlier this only
    // handled LHM + HashMap-bucket sources and fell back to a TreeMap-blind
    // `map_collect_entries`, so `new LinkedHashMap<>(treeMap)` / `lhm.putAll(
    // treeMap)` (and real-JDK unmodifiable-map sources) silently produced an
    // empty map. `collect_entries_any` preserves LHM insertion order.
    // cceres3: pin across GC-capable call (stream stale-at-store wave) — each
    // native_lhm_put re-enters Java, moving `this` and every pending pair.
    let this_pin = ctx.pin_native_root(this);
    // `evict == false` only from the copy constructor with its switch on, so
    // `=0` of `CRATONVM_COMPAT_LHM_COPY_NO_EVICT` is the old call exactly.
    let put_one = |ctx: &mut dyn NativeContext, args: &[Value]| -> MethodCallResult {
        if evict {
            native_lhm_put(ctx, args)
        } else {
            native_lhm_put_evict(ctx, args, false, GrowAlloc::SingleAttempt)
        }
    };
    // C3W3-2 (`CRATONVM_COMPAT_MAP_COPY_JDK_QUESTIONS`, round 15 wave 4, lane
    // compat4): a program's map is asked `putMapEntries`' questions (`size()`
    // gates the walk), each put interleaved.
    if let Some(prog) = foreign_map_source(&*ctx, source) {
        let prog_pin = ctx.pin_native_root(prog);
        let walked = map_for_each_entry(
            ctx,
            (prog_pin, prog),
            MapSizeAsk::Gate,
            |ctx, key, value| {
                let this_now = ctx.read_native_pin(this_pin, this);
                put_one(ctx, &[Value::Object(Some(this_now)), key, value])?;
                Ok(())
            },
        );
        ctx.unpin_native_roots(this_pin);
        return walked.map(|()| None);
    }
    let collected = collect_entries_any(ctx, source);
    let entries = unpin_on_err(ctx, this_pin, collected)?;
    let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (_, pair_handles) = pin_value_slice(ctx, &flat);
    for (i, (key, val)) in entries.iter().enumerate() {
        let this = ctx.read_native_pin(this_pin, this);
        let key = read_pinned_elem(ctx, pair_handles[2 * i], *key);
        let val = read_pinned_elem(ctx, pair_handles[2 * i + 1], *val);
        if let Err(e) = put_one(ctx, &[Value::Object(Some(this)), key, val]) {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    }
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

// ===========================================================================
