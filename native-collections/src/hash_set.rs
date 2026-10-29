// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

// ===========================================================================
// HashSet — field 0 = Object (backing HashMap)
// ===========================================================================

pub(crate) const HS_FIELD_MAP: usize = 0;
pub(crate) const HS_NUM_FIELDS: usize = 1;

// ---------------------------------------------------------------------------
// Live map-view write-through (keySet / entrySet / values)
//
// `Map.keySet()`, `Map.entrySet()` and `Map.values()` must return *live*
// views: removing through them (`view.remove(o)`, `view.iterator().remove()`,
// `view.removeIf(...)`) has to delete the corresponding entry from the
// *backing* map. CratonVM models maps natively and these accessors used to
// hand back a detached snapshot (a fresh `HashSet`/`ArrayList`), so removals
// silently vanished — e.g. Jackson's `POJOPropertiesCollector._renameProperties`
// does `props.entrySet().iterator().remove()` then re-`get`s the same key; with
// a dead snapshot the entry survived and the property was merged with itself,
// producing the "Conflicting getter definitions" failure.
//
// Fix: the snapshot's backing HashMap (for keySet/entrySet) carries a
// reference to the *source* map in a high, never-name-resolved slot, and the
// values snapshot (an ArrayList) stashes the source in the last capacity slot
// of its element array. The shared removal natives (`native_hs_remove`,
// `native_al_itr_remove`, `native_al_remove_obj`) detect that marker and
// propagate the removal to the source map. Both carriers are GC-scanned
// (a synthetic class's declared field block / a live `Object[]`), so the
// stored `ObjectRef` survives a moving GC.
//
// The marker slots are chosen well above any real `java/util/HashMap` field
// index so `map_state` / `set_map_size`'s name-resolved writes never clobber
// them; the view backing is a dedicated synthetic class allocated with exactly
// `VIEW_BACKING_FIELDS` slots so the GC scans the marker slots.
pub(crate) const VIEW_BACKING_FIELDS: usize = 17;
pub(crate) const VIEW_BACKING_SRC_SLOT: usize = 15;
pub(crate) const VIEW_BACKING_KIND_SLOT: usize = 14;
/// The source map's modification generation as of this backing's last resync,
/// or absent when this backing has never been stamped.
///
/// Placed ABOVE both marker slots rather than below them, at the cost of one
/// extra slot per view backing. Below would have been free — slot 13 is
/// undeclared on every `java/util/HashMap` this VM has ever seen — but "every
/// HashMap I have seen" is exactly the assumption that makes a silent
/// clobber possible on the next JDK, and this slot decides whether a rebuild
/// is SKIPPED. `view_backing_source` still discriminates on
/// `> VIEW_BACKING_SRC_SLOT`, so an ordinary (non-view) HashSet backing is
/// classified exactly as before.
pub(crate) const VIEW_BACKING_SRC_GEN_SLOT: usize = 16;
/// `VIEW_KIND_KEYSET`: the set's elements are the source map's keys.
pub(crate) const VIEW_KIND_KEYSET: i32 = 0;
/// `VIEW_KIND_ENTRYSET`: the set's elements are `Map.Entry` objects whose key
/// (slot 0) identifies the source-map entry to delete on removal.
pub(crate) const VIEW_KIND_ENTRYSET: i32 = 1;
/// `VIEW_KIND_ENTRYSET_STATIC`: like `VIEW_KIND_ENTRYSET` — the elements are
/// `Map.Entry` objects and a `remove` writes through to the source map — but the
/// view does **not** resync its contents from the source on every
/// `iterator()`/`size()`. Used for sources whose entries cannot be read back
/// without re-entering this method: `java/util/Properties` keeps its data in a
/// Rust side-table, so resyncing would walk `Properties.entrySet()` and recurse.
/// The set's contents are materialised once at construction and then read from
/// the backing directly; only structural `remove` (and `Entry.setValue`)
/// propagate to the live source.
pub(crate) const VIEW_KIND_ENTRYSET_STATIC: i32 = 2;
/// `VIEW_KIND_KEYSET_STATIC`: the keySet twin of [`VIEW_KIND_ENTRYSET_STATIC`]
/// — elements are bare keys and a `remove` writes through, but the source's
/// keys cannot be collected by walking its fields, so a resync cannot go
/// through `collect_keys_any`. Used for `java/util/Properties`, whose keys are
/// half in a Rust side-table and half in a `ConcurrentHashMap` field and are
/// only assembled correctly by `Properties.keySet()` itself.
///
/// Both STATIC kinds resync by [`adopt_fresh_view_backing`]: ask the source for
/// a NEW view and take its backing. That costs one extra view construction per
/// read and, unlike a field walk, cannot get the contents wrong.
pub(crate) const VIEW_KIND_KEYSET_STATIC: i32 = 3;

/// True for either entrySet view kind (elements are `Map.Entry` objects), as
/// opposed to a keySet view (elements are bare keys).
pub(crate) fn is_entryset_kind(kind: i32) -> bool {
    kind == VIEW_KIND_ENTRYSET || kind == VIEW_KIND_ENTRYSET_STATIC
}

/// The accessor a view of `kind` is rebuilt through — see
/// [`adopt_fresh_view_backing`]. `None` for the resyncing kinds, which rebuild
/// their contents from the source's fields instead.
pub(crate) fn static_view_accessor(kind: i32) -> Option<(&'static str, &'static str)> {
    match kind {
        VIEW_KIND_ENTRYSET_STATIC => Some(("entrySet", "()Ljava/util/Set;")),
        VIEW_KIND_KEYSET_STATIC => Some(("keySet", "()Ljava/util/Set;")),
        _ => None,
    }
}

/// Allocate the backing HashMap for a keySet/entrySet view: a synthetic
/// `cratonvm/util/MapViewBacking` with the usual `(buckets, size, capacity)`
/// state in slots 0..3 plus the source-map reference and view-kind marker in
/// the high slots.
pub(crate) fn alloc_view_backing(
    ctx: &mut dyn NativeContext,
    source: &mut ObjectRef,
    kind: i32,
    cap: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    // GC-safety: `source` outlives the bucket allocation, the
    // `ensure_class_initialized` below (which can run a `<clinit>`) and the
    // backing allocation, and `buckets` outlives the last two — all as bare
    // Rust locals. A freshly allocated bucket array that nothing references yet
    // is reclaimed outright by a collection here. See `rooted_across`.
    let mut buckets = rooted_across(ctx, &mut [source], |ctx| alloc_ref_array(ctx, cap));

    // Build the view backing with the REAL `java/util/HashMap` field layout
    // (buckets at the resolved `table` slot), NOT the legacy synthetic
    // `(buckets@0, size@1, capacity@2)` placement. JDK bytecode for
    // `keySet()/entrySet().spliterator()` constructs a
    // `HashMap.KeySpliterator(map, ...)` and reads `getfield map.table` /
    // `map.modCount` / `map.size` directly; on the synthetic layout `table`
    // (real slot 2) resolved to the `Int(capacity=16)` slot, so `arraylength`
    // panicked with `expected object reference, got int(16)`. This mirrors the
    // already-fixed plain-HashSet path (`make_hashset_with_elements`, S111r13).
    //
    // The source-map + view-kind markers stay in the high slots (14/15), which
    // sit above every real `java/util/HashMap` instance field (loadFactor is the
    // last, ≤ slot 7), so write-through removal keeps working and
    // `view_backing_source` still distinguishes a view backing (≥ 16 slots) from
    // an ordinary HashSet backing (real layout, fewer slots). `map_state` /
    // `set_map_size` are already layout-aware (table-slot fallback + name-
    // resolved `size`), so the view's own population/iterator/remove natives are
    // unaffected. Falls back to the legacy synthetic `MapViewBacking` layout if
    // the real HashMap class is not yet resolvable (early bootstrap).
    rooted_across(ctx, &mut [source, &mut buckets], |ctx| {
        let _ = ctx.ensure_class_initialized("java/util/HashMap");
    });
    let f_table = ctx.resolve_field_index("java/util/HashMap", "table");
    let f_size = ctx.resolve_field_index("java/util/HashMap", "size");
    if let (Some(f_table), Some(f_size)) = (f_table, f_size) {
        let hashmap_cid = ctx
            .class_id_by_name("java/util/HashMap")
            .unwrap_or_else(|| cratonvm_types::ClassId::new(0));
        let f_modcount = ctx.resolve_field_index("java/util/HashMap", "modCount");
        let f_threshold = ctx.resolve_field_index("java/util/HashMap", "threshold");
        let f_loadfactor = ctx.resolve_field_index("java/util/HashMap", "loadFactor");
        let f_entryset = ctx.resolve_field_index("java/util/HashMap", "entrySet");
        // Enough slots for every real field AND the high marker slots.
        let real_max = [
            Some(f_table),
            Some(f_size),
            f_modcount,
            f_threshold,
            f_loadfactor,
            f_entryset,
        ]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(0);
        let n_fields = std::cmp::max(real_max + 1, VIEW_BACKING_FIELDS);
        let backing: ObjectRef = rooted_across(ctx, &mut [source, &mut buckets], |ctx| {
            ctx.alloc_object(hashmap_cid, n_fields)
        });
        ctx.set_field(backing, f_table, Value::Object(Some(buckets)));
        // No slot-0 mirror. This backing is allocated as a real
        // `java/util/HashMap`, so slot 0 is `AbstractMap.keySet` and the array
        // does not belong there; `map_state` and the `collect_view_snapshot`
        // guard both ask `map_buckets_slot`, which answers `f_table` for this
        // object. (The mirror existed so those readers could go straight to
        // slot 0 without resolving anything.)
        ctx.set_field(backing, f_size, Value::Int(0));
        if let Some(f) = f_modcount {
            ctx.set_field(backing, f, Value::Int(0));
        }
        if let Some(f) = f_threshold {
            ctx.set_field(backing, f, Value::Int((cap as i32 * 3) / 4));
        }
        if let Some(f) = f_loadfactor {
            ctx.set_field(backing, f, Value::Float(0.75));
        }
        if let Some(f) = f_entryset {
            ctx.set_field(backing, f, Value::Object(None));
        }
        ctx.set_field(backing, VIEW_BACKING_KIND_SLOT, Value::Int(kind));
        ctx.set_field(backing, VIEW_BACKING_SRC_SLOT, Value::Object(Some(*source)));
        return Ok(backing);
    }

    // Fallback: legacy synthetic `(buckets, size, capacity)` MapViewBacking
    // layout, used before the real `java/util/HashMap` class is resolvable.
    let backing: ObjectRef = rooted_across(ctx, &mut [source, &mut buckets], |ctx| {
        try_alloc_synthetic(ctx, "cratonvm/util/MapViewBacking", VIEW_BACKING_FIELDS)
    })?;
    publish_map_table(ctx, backing, buckets, cap as i32);
    set_map_size(ctx, backing, 0);
    ctx.set_field(backing, VIEW_BACKING_KIND_SLOT, Value::Int(kind));
    ctx.set_field(backing, VIEW_BACKING_SRC_SLOT, Value::Object(Some(*source)));
    Ok(backing)
}

/// If `backing` is a keySet/entrySet view backing carrying a source-map
/// reference, return it. Returns `None` for ordinary HashSet backings (real
/// `java/util/HashMap`, fewer than `VIEW_BACKING_FIELDS` slots).
pub(crate) fn view_backing_source(ctx: &dyn NativeContext, backing: ObjectRef) -> Option<ObjectRef> {
    if ctx.object_num_fields(backing) > VIEW_BACKING_SRC_SLOT {
        if let Value::Object(Some(src)) = ctx.get_field(backing, VIEW_BACKING_SRC_SLOT) {
            return Some(src);
        }
    }
    None
}

/// The view kind (keySet vs entrySet) stored on a view backing.
pub(crate) fn view_backing_kind(ctx: &dyn NativeContext, backing: ObjectRef) -> i32 {
    if ctx.object_num_fields(backing) > VIEW_BACKING_KIND_SLOT {
        if let Value::Int(k) = ctx.get_field(backing, VIEW_BACKING_KIND_SLOT) {
            return k;
        }
    }
    VIEW_KIND_KEYSET
}

// ---------------------------------------------------------------------------
// Live-view rebuild elision (`perf/lazy-map-views-*`)
// ---------------------------------------------------------------------------

/// Kill switch for the keySet-view rebuild elision below.
/// `CRATONVM_MAP_VIEW_CACHE=0` restores the unconditional per-read rebuild.
/// Default ON.
pub(crate) fn map_view_cache_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_MAP_VIEW_CACHE")
}

/// `CRATONVM_VERIFY_MAP_VIEW_CACHE=1` — take the elision decision, then rebuild
/// anyway and compare. A divergence panics with both key multisets.
///
/// This exists because the soundness of the elision is an argument about which
/// mutators move a generation counter, and an argument is not a measurement.
/// Running a suite under this turns "the guard cannot under-invalidate" into
/// something the tree checks on every read.
pub(crate) fn verify_map_view_cache() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        matches!(
            cratonvm_types::flags::runtime_var_os("CRATONVM_VERIFY_MAP_VIEW_CACHE")
                .as_deref()
                .and_then(|s| s.to_str()),
            Some("1")
        )
    })
}

/// The number of rebuilds this process SKIPPED, and the number it ran. Printed
/// by `CRATONVM_DBG=map-view-cache` at exit.
///
/// Both counters, not just the hit: `skipped=0` is the answer "the elision
/// never engaged", and it is worth nothing unless it can be told apart from
/// the switch being off — the same reason `report_lambda_census_at_exit`
/// prints its zeros. See `reference_put_the_engagement_counter_next_to_the_number`.
pub(crate) static MAP_VIEW_RESYNC_SKIPPED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static MAP_VIEW_RESYNC_RAN: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The JDK field a live view of `kind` is cached in on its source map, or
/// `None` for a kind that must not be cached.
///
/// The names are the JDK's own: `java.util.AbstractMap` declares `keySet` and
/// `values`, `java.util.HashMap` declares `entrySet`, and each is exactly the
/// "return the one instance" slot HotSpot uses (`Set<K> ks = keySet; return ks
/// != null ? ks : (keySet = new KeySet());`). So caching here does not merely
/// save work, it restores the identity guarantee `map.keySet() ==
/// map.keySet()` that this VM did not have.
///
/// Both STATIC kinds are refused, and not for the reason the elision refuses
/// them. A STATIC view resyncs by [`adopt_fresh_view_backing`], which asks the
/// source for a FRESH view and adopts its backing — if the accessor returned
/// the cached instance, the view would adopt its own backing and never refresh
/// again. That is a liveness break, not a slow path.
pub(crate) fn view_cache_field(kind: i32) -> Option<&'static str> {
    match kind {
        VIEW_KIND_KEYSET => Some("keySet"),
        VIEW_KIND_ENTRYSET => Some("entrySet"),
        _ => None,
    }
}

/// The live view of `kind` already cached on `source`, if there is one and it
/// still describes this source.
///
/// The validation is the point. The field is a REAL JDK slot that other code
/// may write, and a wrong object here would be handed out as this map's key
/// set — so the candidate has to prove itself: it must carry a view backing,
/// that backing's source must be `source` by identity, and its kind must
/// match. Anything else is ignored and a fresh view is built, which is what
/// happened before this cache existed.
///
/// `unwrap_synchronized` first, because `Hashtable`/`Properties` accessors hand
/// back a `Collections$Synchronized*` wrapper (see `wrap_synchronized_view`) and
/// the backing lookup is one level in — the same step
/// `adopt_fresh_view_backing` needs for the same reason.
pub(crate) fn cached_live_view(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
    kind: i32,
) -> Option<ObjectRef> {
    if !map_view_cache_enabled() {
        return None;
    }
    // `Properties` is refused: it keeps half its keys in a Rust side-table that
    // only its own `keySet()` assembles correctly, so a cached instance would
    // pin whatever the field-walking path produced instead of rebuilding it.
    // HotSpot does not cache a `Properties` view either, so the refusal is the
    // right answer and not only the safe one.
    //
    // `Hashtable` is NOT refused, and used to be. The wrapper its accessors
    // hand back is handled by `unwrap_synchronized` below; see
    // [`view_cache_refused`] for the measurement that separated the two.
    if view_cache_refused(&*ctx, source) {
        return None;
    }
    let field = view_cache_field(kind)?;
    let class_id = ctx.class_id_of_object(source);
    let slot = ctx.resolve_field_index_by_class_id(class_id, field)?;
    if slot >= ctx.object_num_fields(source) {
        return None;
    }
    let Value::Object(Some(view)) = ctx.get_field(source, slot) else {
        return None;
    };
    let inner = unwrap_synchronized(ctx, view);
    let backing = hs_backing_map(&*ctx, inner)?;
    let cached_source = view_backing_source(&*ctx, backing)?;
    if !std::ptr::eq(cached_source.as_ptr(), source.as_ptr()) {
        return None;
    }
    if view_backing_kind(&*ctx, backing) != kind {
        return None;
    }
    MAP_VIEW_REUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Some(view)
}

/// The [`MAP_VIEW_CARRIERS`] twin of [`cached_live_view`], for `values()`.
///
/// A values view is not a set view: it keeps its state in an `ArrayList`-shaped
/// carrier and its back-reference in the trailing capacity slot of its element
/// array, not in a view backing carrying a `VIEW_KIND_*`. So it needs its own
/// reader and its own validation, and it gets only the CONSTRUCTION half of the
/// fix — `resync_values_view` still rebuilds the element array on every read,
/// and must, because a values view is exactly the shape a value-replacing `put`
/// changes while moving no structural counter.
///
/// Same refusal of the `Hashtable`/`Properties` family as the set twin, for the
/// same reasons.
///
/// The entry carrier is excluded by name: `TreeMap$EntrySet` is a
/// `MAP_VIEW_CARRIERS` member whose elements are `Map.Entry`, not values, and
/// it is not what `values()` returns.
pub(crate) fn cached_live_values_view(ctx: &mut dyn NativeContext, source: ObjectRef) -> Option<ObjectRef> {
    if !map_view_cache_enabled() || view_cache_refused(&*ctx, source) {
        return None;
    }
    let class_id = ctx.class_id_of_object(source);
    let slot = ctx.resolve_field_index_by_class_id(class_id, "values")?;
    if slot >= ctx.object_num_fields(source) {
        return None;
    }
    let Value::Object(Some(view)) = ctx.get_field(source, slot) else {
        return None;
    };
    // UNWRAP FIRST. A `Hashtable`'s accessors hand back a
    // `Collections$SynchronizedCollection` around the real carrier, so both the
    // carrier test and the source test have to look through it -- which
    // `cached_live_view` already does for keySet and entrySet, and this did
    // not. It is why a `Hashtable` values view stayed uncached even after the
    // family stopped being refused outright: the stored object's class was the
    // WRAPPER's, which is not a map-view carrier, so every lookup declined.
    let inner = unwrap_synchronized(ctx, view);
    match ctx
        .class_name_arc_of_id(ctx.class_id_of_object(inner))
        .as_deref()
    {
        Some(n) if is_map_view_carrier(n) && n != TM_ENTRY_SET_CARRIER => {}
        _ => return None,
    }
    let cached_source = values_view_source(&*ctx, inner)?;
    if !std::ptr::eq(cached_source.as_ptr(), source.as_ptr()) {
        return None;
    }
    MAP_VIEW_REUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Some(view)
}

/// Record `view` as this source's live `values()` view.
pub(crate) fn store_live_values_view(ctx: &mut dyn NativeContext, source: ObjectRef, view: ObjectRef) {
    // `view_cache_refused`, matching the reader. These two guards differ only
    // in their return type, which is why the first pass at this change moved
    // the reader and left the writer -- and a values view that is read from the
    // cache but never written to it caches nothing at all.
    if !map_view_cache_enabled() || view_cache_refused(&*ctx, source) {
        return;
    }
    try_set_jdk_map_field(ctx, source, "values", Value::Object(Some(view)));
}

/// Record `view` as this source's live view of `kind`.
///
/// A no-op when the receiver's class does not declare the field — a
/// synthetic-mode map has no `keySet` slot, and `try_set_jdk_map_field`'s own
/// note explains why resolving the name on the RECEIVER's class rather than on
/// a hard-coded `java/util/HashMap` is the difference between writing the field
/// and destroying an unrelated one.
pub(crate) fn store_live_view(ctx: &mut dyn NativeContext, source: ObjectRef, kind: i32, view: ObjectRef) {
    if !map_view_cache_enabled() {
        return;
    }
    // Same refusal as the read side, and it has to be here too: a store the
    // read can never accept is a leak of a live view into a JDK slot for no
    // benefit at all.
    if view_cache_refused(&*ctx, source) {
        return;
    }
    let Some(field) = view_cache_field(kind) else {
        return;
    };
    try_set_jdk_map_field(ctx, source, field, Value::Object(Some(view)));
}

/// Views handed back out of the source's own JDK slot rather than rebuilt.
pub(crate) static MAP_VIEW_REUSED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Views constructed from scratch.
pub(crate) static MAP_VIEW_BUILT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Print the rebuild-elision census on `CRATONVM_DBG=map-view-cache`.
///
/// Prints even when every counter is zero — a zero `skipped` is the answer
/// "the elision never engaged", and it is worth nothing unless it can be told
/// apart from the switch having been off, which the absence of a line cannot.
pub fn report_map_view_cache_at_exit() {
    // The DECLARED key, not a hand-rolled split of `CRATONVM_DBG`: the grouped
    // form expands `CRATONVM_DBG=map-view-cache` into this name, and reading the
    // group string directly would miss every other way of setting it (and would
    // be invisible to `flags::with_thread_overrides` in a test).
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_MAP_VIEW_CACHE").is_none() {
        return;
    }
    let skipped = MAP_VIEW_RESYNC_SKIPPED.load(std::sync::atomic::Ordering::Relaxed);
    let ran = MAP_VIEW_RESYNC_RAN.load(std::sync::atomic::Ordering::Relaxed);
    let total = skipped + ran;
    let pct = if total == 0 {
        0.0
    } else {
        (skipped as f64) * 100.0 / (total as f64)
    };
    eprintln!(
        "[MAP-VIEW-CACHE] EXIT resync_skipped={skipped} resync_ran={ran} elided={pct:.1}% view_reused={} view_built={} switch={} verify={} modcount_bumped={} modcount_refused={}",
        MAP_VIEW_REUSED.load(std::sync::atomic::Ordering::Relaxed),
        MAP_VIEW_BUILT.load(std::sync::atomic::Ordering::Relaxed),
        if map_view_cache_enabled() { "ON" } else { "OFF" },
        if verify_map_view_cache() { "ON" } else { "OFF" },
        MAP_MODCOUNT_BUMPED.load(std::sync::atomic::Ordering::Relaxed),
        MAP_MODCOUNT_REFUSED.load(std::sync::atomic::Ordering::Relaxed),
    );
}

/// The source-map generation a keySet view's backing may be stamped with, or
/// `None` when this `(kind, source)` pair has no usable invalidation signal and
/// must therefore rebuild on every read.
///
/// Every refusal here is a family whose mutators are NOT known to move the
/// generation, and a missing bump is a silently stale collection — the worst
/// failure mode available. The admitted set is exactly:
///
/// * **keySet views only** (`VIEW_KIND_KEYSET`). An entrySet view's contents
///   include VALUES, and a value-replacing `put` deliberately does not bump
///   `modCount` (HotSpot does not either — `put=5 remove=6 replace=6` in
///   `probes/MapModCountProbe2`), so a generation guard would UNDER-invalidate
///   it. The two STATIC kinds are refused because their sources
///   (`java/util/Properties`) keep half their keys in a Rust side-table that no
///   `modCount` tracks.
/// * **not a `ConcurrentHashMap` source.** CHM mutations bump the SEGMENT's
///   `modCount` (`native_chm_put` delegates to `native_map_put` on the segment),
///   never the CHM's own, so its generation is frozen for the life of the map.
/// * **the source actually has a readable `modCount`.** Absent one there is no
///   signal at all.
///
/// What that leaves — `HashMap`, `LinkedHashMap`, `Hashtable`/its subclasses
/// and `TreeMap` — is the set whose every structural mutator was audited to
/// call `bump_map_mod_count`: `native_map_put_evict_pinned`,
/// `native_map_remove_pinned` (x2), `native_map_clear` for the HashMap family
/// (which `Hashtable` shares), `lhm_set("size")` for `LinkedHashMap`, and
/// `tm_set_slot` for `TreeMap`. Over-invalidation is always safe here; only
/// under-invalidation is not.
pub(crate) fn view_source_generation(ctx: &dyn NativeContext, source: ObjectRef, kind: i32) -> Option<i32> {
    if !map_view_cache_enabled() {
        return None;
    }
    if kind != VIEW_KIND_KEYSET {
        return None;
    }
    if is_chm_receiver(ctx, source) {
        return None;
    }
    map_itr_mod_count(ctx, source)
}

/// The generation stamped on `backing` at its last resync, if any.
pub(crate) fn view_backing_stamp(ctx: &dyn NativeContext, backing: ObjectRef) -> Option<i32> {
    if ctx.object_num_fields(backing) > VIEW_BACKING_SRC_GEN_SLOT {
        if let Value::Int(g) = ctx.get_field(backing, VIEW_BACKING_SRC_GEN_SLOT) {
            return Some(g);
        }
    }
    None
}

/// Record `gen` as the generation `backing`'s contents were built from.
pub(crate) fn set_view_backing_stamp(ctx: &mut dyn NativeContext, backing: ObjectRef, gen: i32) {
    if ctx.object_num_fields(backing) > VIEW_BACKING_SRC_GEN_SLOT {
        ctx.set_field(backing, VIEW_BACKING_SRC_GEN_SLOT, Value::Int(gen));
    }
}

/// Drop any stamp on `backing`, forcing the next read to rebuild.
pub(crate) fn clear_view_backing_stamp(ctx: &mut dyn NativeContext, backing: ObjectRef) {
    if ctx.object_num_fields(backing) > VIEW_BACKING_SRC_GEN_SLOT {
        ctx.set_field(backing, VIEW_BACKING_SRC_GEN_SLOT, Value::Object(None));
    }
}

/// Remove `key` from `source` by dispatching through the map's own virtual
/// `remove(Object)` so the correct backend fires (HashMap buckets,
/// LinkedHashMap overlay, TreeMap tree, ConcurrentHashMap segments).
pub(crate) fn source_map_remove(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
    key: Value,
) -> Result<(), MethodCallFailed> {
    ctx.invoke_virtual(
        source,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[key],
    )?;
    Ok(())
}

/// Collect `(key, value)` pairs from any of the natively-modelled maps,
/// dispatching on the concrete backend so a LinkedHashMap's overlay / a
/// TreeMap's tree are read correctly rather than as empty bucket tables.
///
/// Fallible for the reason on [`collection_elements_generic`], and this is the
/// map-side twin of that defect: every caller below is a copy constructor, a
/// `putAll`, a `forEach`, a `hashCode` or a serialisation writer, and each one
/// reads an empty answer as "the source map had no entries". A refused
/// `isEmpty()` / `entrySet()` / `iterator()` therefore used to produce an empty
/// destination map and no exception at all.
pub(crate) fn collect_entries_any(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
) -> Result<Vec<(Value, Value)>, MethodCallFailed> {
    // See through CratonVM's synthetic unmodifiable wrapper first so a
    // `Map.of(...)` / `Collections.unmodifiableMap(...)` source is read as its
    // backing map (and the TreeMap/LHM dispatch below sees the real class).
    let source = unwrap_unmod(ctx, source);
    // Ancestry-aware: LinkedHashMap SUBCLASSES (Spring AnnotationAttributes,
    // LinkedMultiValueMap internals, ...) store entries in the LHM overlay
    // too — an exact-name check here read their (empty) HashMap buckets.
    if is_lhm_receiver(ctx, source) {
        let ks = lhm_collect_keys(ctx, source);
        let vs = lhm_collect_values(ctx, source);
        return Ok(ks.into_iter().zip(vs).collect());
    }
    if is_tree_map_receiver(ctx, source) {
        return Ok(tm_collect_pairs(ctx, source));
    }
    // Only interpret the source's fields as a HashMap bucket table when it is
    // actually one of the map kinds CratonVM models that way (real/synthetic
    // `java/util/HashMap` or subclass, `ConcurrentHashMap`, or a
    // `Properties`/`Hashtable` backed by a side CHM). An arbitrary `Map` whose
    // slot 0 merely happens to be an object array — Spring's
    // `ConcurrentReferenceHashMap`, whose slot 0 is `Segment[] segments` — would
    // otherwise be walked as `HashMap$Node` chains, surfacing internal segment
    // locks and reference queues as bogus (ReferenceQueue, map) entries. For
    // those, skip straight to the polymorphic `entrySet().iterator()` walk.
    if is_native_bucket_map(ctx, source) {
        let entries = map_collect_entries(ctx, source);
        if !entries.is_empty() {
            return Ok(entries);
        }
    }
    // Either a genuinely empty native map, or a `Map` whose internal layout
    // CratonVM does not model natively — e.g. the real-JDK
    // `Collections$Unmodifiable{Navigable,Sorted}Map` wrappers that
    // `Settings.settings` uses (`new HashMap<>(settings.settings)` in
    // `Settings.Builder.put(Settings)`), Spring's `ConcurrentReferenceHashMap`,
    // or any third-party `Map`. Consult the map's own `isEmpty()`; only if it
    // actually has entries do we pay for the polymorphic `entrySet().iterator()`
    // walk — the same contract the JDK's `HashMap.putMapEntries` relies on — so
    // the copy works for ANY `Map`.
    //
    // cceres5 (live-captured on the FIXED fix3 binary, boot-145: the
    // `Object.entrySet()` NSME at getOrCreateSubregistry recurred): the
    // `isEmpty()` invoke below dispatches real bytecode (GC-capable) and
    // `source` was carried RAW across it into the iterator walk -- the walk's
    // own entry pin then faithfully pinned an ALREADY-STALE address. Pin +
    // refresh across the probe.
    let source_pin = ctx.pin_native_root(source);
    // The `matches!` this replaced folded `Err` into "empty", which is the
    // arm that mattered: a refused `isEmpty()` skipped the walk entirely and
    // the caller copied nothing. Unwind the pin before propagating.
    let is_empty = ctx.invoke(
        "java/util/Map",
        "isEmpty",
        "()Z",
        &[Value::Object(Some(source))],
    );
    let source = ctx.read_native_pin(source_pin, source);
    ctx.unpin_native_roots(source_pin);
    let nonempty = match is_empty {
        Ok(Some(Value::Int(0))) => true,
        Err(e) => return Err(e),
        _ => false,
    };
    if nonempty {
        return collect_entries_via_iterator(ctx, source);
    }
    Ok(Vec::new())
}

/// Walk an arbitrary `java.util.Map` via its polymorphic
/// `entrySet().iterator()`, reading each entry's key/value through the real
/// `Map.Entry` accessors. Layout-agnostic fallback used by
/// [`collect_entries_any`] for `Map` implementations CratonVM does not model
/// natively (real-JDK unmodifiable wrappers, third-party maps).
thread_local! {
    /// Source maps currently being walked by `collect_entries_via_iterator`.
    /// Guards against infinite recursion when a view-set's `iterator()` resyncs
    /// from its source map (`resync_view_set` → `collect_entries_any` →
    /// `collect_entries_via_iterator` → `source.entrySet().iterator()` →
    /// `native_hs_iterator` → `resync_view_set` …). Each `entrySet()` allocates
    /// a fresh view object, so a per-view guard wouldn't catch it — key on the
    /// stable source-map pointer instead. Observed as the JSSE-connector
    /// stack-overflow during TLS init.
    static ITER_COLLECT_GUARD: std::cell::RefCell<std::collections::HashSet<u64>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

/// RAII: removes our source key from `ITER_COLLECT_GUARD` on drop, including on
/// panic/unwind — so a caught native panic can never leak a stale key (which
/// would make a later, unrelated collection of a pointer-reused source wrongly
/// return empty).
pub(crate) struct IterCollectGuard {
    key: u64,
    inserted: bool,
}
impl Drop for IterCollectGuard {
    fn drop(&mut self) {
        if self.inserted {
            ITER_COLLECT_GUARD.with(|g| {
                g.borrow_mut().remove(&self.key);
            });
        }
    }
}

pub(crate) fn collect_entries_via_iterator(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
) -> Result<Vec<(Value, Value)>, MethodCallFailed> {
    let guard_key = source.as_ptr() as u64;
    let inserted = ITER_COLLECT_GUARD.with(|g| g.borrow_mut().insert(guard_key));
    // The RAII guard also covers the `?`-shaped early returns the inner walk
    // now has: an `Err` unwinds through `Drop` exactly as a panic did, so a
    // refusal cannot leave a stale key behind and make a later collection of a
    // pointer-reused source wrongly answer empty.
    let _guard = IterCollectGuard {
        key: guard_key,
        inserted,
    };
    if !inserted {
        // Re-entrant collection of the SAME source via its own entrySet view —
        // walking it again would recurse forever. Break the cycle.
        return Ok(Vec::new());
    }
    collect_entries_via_iterator_inner(ctx, source)
}

pub(crate) fn collect_entries_via_iterator_inner(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
) -> Result<Vec<(Value, Value)>, MethodCallFailed> {
    let mut out = Vec::new();
    // Dispatch through `invoke_virtual` on each concrete receiver — NOT
    // `ctx.invoke("java/util/Map", ...)`. The latter keys the native lookup on
    // the *interface* class name and so fires the natively-modelled
    // `native_map_entry_set` (bucket-scan) regardless of the receiver's real
    // type. For a `Map` whose layout CratonVM does NOT model — e.g. Spring's
    // `MessageHeaders` (backed by a private `HashMap` field, slot 0 is not a
    // bucket array) or the real-JDK `Collections$Unmodifiable*Map` wrappers —
    // that bucket scan reads nothing and the walk returns EMPTY, silently
    // dropping every entry (the `new HashMap<>(messageHeaders)` copy losing the
    // `contentType` header). Virtual dispatch instead resolves each call on the
    // receiver's actual class, running its real `entrySet()`/`iterator()`/
    // `Map.Entry` bytecode — exactly what a `for (e : map.entrySet())` loop does.
    // cceres5 (live-captured 2026-07-22, three baseline events at ONE call
    // site — `new HashMap<>(children)` in ConcreteResourceRegistration.
    // getOrCreateSubregistry): every `invoke_virtual` below is GC-capable
    // real-bytecode dispatch, yet `it` was reused raw across every iteration
    // (`NoSuchMethodError java/lang/Object.hasNext()`), `entry` was reused raw
    // across `getKey` (`NoSuchMethodError java/lang/Object.entrySet()` one
    // level up), and the accumulated `out` pairs sat raw across the whole
    // remaining walk — the caller then pinned ALREADY-STALE addresses and
    // copied garbage into the destination map (the fatal
    // `checkcast OperationStepHandler` on a bare cid=0 Object). Pin the chain
    // and every produced pair; read everything back through the pins at the
    // end (mirrors `chm_extra_entries`).
    let source_pin = ctx.pin_native_root(source);
    let source_c = ctx.read_native_pin(source_pin, source);
    let set = match ctx.invoke_virtual(source_c, "entrySet", "()Ljava/util/Set;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => s,
        Err(e) => {
            ctx.unpin_native_roots(source_pin);
            return Err(e);
        }
        _ => {
            ctx.unpin_native_roots(source_pin);
            return Ok(out);
        }
    };
    let set_pin = ctx.pin_native_root(set);
    let set_c = ctx.read_native_pin(set_pin, set);
    let it = match ctx.invoke_virtual(set_c, "iterator", "()Ljava/util/Iterator;", &[]) {
        Ok(Some(Value::Object(Some(i)))) => i,
        Err(e) => {
            ctx.unpin_native_roots(source_pin);
            return Err(e);
        }
        _ => {
            ctx.unpin_native_roots(source_pin);
            return Ok(out);
        }
    };
    let it_pin = ctx.pin_native_root(it);
    // (key_pin, key_fallback, value_pin, value_fallback) per entry. Pins are
    // strictly LIFO — nothing is unpinned until the single truncate below, so
    // handles stay valid (see the PIN-DANGLING history in chm_extra_entries).
    let mut pinned: Vec<(usize, Value, usize, Value)> = Vec::new();
    // Same reason as `collection_elements_generic`: a failure inside the walk
    // is recorded and the loop left by the normal `break`, so the one truncate
    // at `source_pin` still covers `set_pin`, `it_pin` and every per-entry pin.
    let mut failed: Option<MethodCallFailed> = None;
    loop {
        let it_c = ctx.read_native_pin(it_pin, it);
        match ctx.invoke_virtual(it_c, "hasNext", "()Z", &[]) {
            Ok(Some(Value::Int(n))) if n != 0 => {}
            Err(e) => {
                failed = Some(e);
                break;
            }
            _ => break,
        }
        let it_c = ctx.read_native_pin(it_pin, it);
        let entry = match ctx.invoke_virtual(it_c, "next", "()Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(e)))) => e,
            Err(e) => {
                failed = Some(e);
                break;
            }
            _ => break,
        };
        let entry_pin = ctx.pin_native_root(entry);
        let entry_c = ctx.read_native_pin(entry_pin, entry);
        // `.ok().flatten().unwrap_or(null)` mapped a throwing or refused
        // `getKey()` to a null KEY, which then went into the destination map as
        // a real entry — a fabricated datum, not a missing one.
        let key = match ctx.invoke_virtual(entry_c, "getKey", "()Ljava/lang/Object;", &[]) {
            Ok(v) => v.unwrap_or(Value::Object(None)),
            Err(e) => {
                failed = Some(e);
                break;
            }
        };
        let key_pin = pin_value(ctx, key);
        let entry_c = ctx.read_native_pin(entry_pin, entry);
        let value = match ctx.invoke_virtual(entry_c, "getValue", "()Ljava/lang/Object;", &[]) {
            Ok(v) => v.unwrap_or(Value::Object(None)),
            Err(e) => {
                failed = Some(e);
                break;
            }
        };
        let value_pin = pin_value(ctx, value);
        pinned.push((key_pin, key, value_pin, value));
    }
    for (key_pin, key0, value_pin, value0) in &pinned {
        let key = read_pinned_elem(ctx, *key_pin, *key0);
        let value = read_pinned_elem(ctx, *value_pin, *value0);
        out.push((key, value));
    }
    ctx.unpin_native_roots(source_pin);
    match failed {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

/// Build a keySet/entrySet view: a `HashSet` snapshot whose backing remembers
/// the source map (+ kind) so removals write through.
pub(crate) fn make_view_set_of(
    ctx: &mut dyn NativeContext,
    source: &mut ObjectRef,
    kind: i32,
    elems: &[Value],
) -> Result<ObjectRef, MethodCallFailed> {
    // cceres3: pin across GC-capable call (stream stale-at-store wave) — the
    // set/backing allocations move `source` and the snapshot elems, and every
    // native_map_put below re-enters Java, moving `backing` + pending elems.
    // Read BEFORE the first allocation below. `elems` was collected by the
    // caller with nothing running in between, so this generation describes
    // exactly the contents about to be inserted. The `native_map_put` loop
    // further down dispatches `hashCode()`, which is arbitrary Java and may
    // mutate the source — in which case the generation moves past this stamp
    // and the next read rebuilds. That is the safe direction.
    let source_gen = view_source_generation(&*ctx, *source, kind);
    // `CRATONVM_COMPAT_VIEW_BUILD_BY_NODE`: the stored hash behind each key,
    // when `elems` are exactly the source's node keys in order. Pure reads,
    // before anything allocates.
    let stored_hashes: Option<Vec<i32>> = if kind == VIEW_KIND_KEYSET {
        view_source_keys_hashed(&*ctx, *source)
            .filter(|(keys, _)| {
                keys.len() == elems.len()
                    && keys.iter().zip(elems).all(|(a, b)| value_same_ref(*a, *b))
            })
            .map(|(_, hashes)| hashes)
    } else {
        None
    };
    let (elem_base, elem_handles) = pin_value_slice(ctx, elems);
    let source_pin = ctx.pin_native_root(*source);
    let first_pin = if elem_base == usize::MAX {
        source_pin
    } else {
        elem_base
    };
    let carrier = set_view_carrier_for(&*ctx, *source, kind);
    let set = alloc_set_view_carrier(ctx, carrier)?;
    let set_pin = ctx.pin_native_root(set);
    let cap = std::cmp::max(elems.len().next_power_of_two(), MAP_DEFAULT_CAPACITY);
    *source = ctx.read_native_pin(source_pin, *source);
    let backing = alloc_view_backing(ctx, source, kind, cap)?;
    let backing_pin = ctx.pin_native_root(backing);
    // A fresh allocation can land on an address a dead overlay tenant was
    // last seen at, and the overlay outlives the object it was keyed to --
    // the same hazard `HashMap.<init>` purges for. Do it once here so the
    // put loop below starts from an empty map by every measure of empty.
    hm_int_fast_purge(&*ctx, backing);
    let set = ctx.read_native_pin(set_pin, set);
    hs_set_backing_map(ctx, set, backing);
    // The declared enclosing-instance field, next to the undeclared backing
    // slot above. `source` was re-read from its pin two lines up and nothing
    // has allocated since.
    *source = ctx.read_native_pin(source_pin, *source);
    let set = ctx.read_native_pin(set_pin, set);
    store_set_view_backref(ctx, set, *source);
    let sentinel = Value::Int(1);
    let hashed_fill = stored_hashes.is_some();
    if let Some(hashes) = stored_hashes {
        let backing = ctx.read_native_pin(backing_pin, backing);
        if let Err(e) =
            fill_view_backing_hashed(ctx, backing_pin, backing, elems, &elem_handles, &hashes)
        {
            ctx.unpin_native_roots(first_pin);
            return Err(e);
        }
    }
    for (i, elem) in elems.iter().enumerate() {
        if hashed_fill {
            break;
        }
        let backing = ctx.read_native_pin(backing_pin, backing);
        let elem = read_pinned_elem(ctx, elem_handles[i], *elem);
        if let Err(e) = native_map_put(
            ctx,
            &[Value::Object(Some(backing)), elem, present_marker(elem)],
        ) {
            ctx.unpin_native_roots(first_pin);
            return Err(e);
        }
    }
    if let Some(g) = source_gen {
        let backing = ctx.read_native_pin(backing_pin, backing);
        set_view_backing_stamp(ctx, backing, g);
    }
    let set = ctx.read_native_pin(set_pin, set);
    ctx.unpin_native_roots(first_pin);
    Ok(set)
}

/// The carrier classes a map view is minted under.
///
/// Every one of these was `java/util/ArrayList` until 2026-08-13, and that had
/// two costs. The visible one: `hashMap.values().getClass()` answered
/// `java.util.ArrayList`, `values() instanceof List` was `true` where HotSpot
/// says `false`, and `values().equals(anArrayList)` was `true` where HotSpot's
/// `AbstractCollection` gives identity equality. The expensive one: because a
/// receiver whose class is exactly `java/util/ArrayList` MIGHT have been a
/// view, no `native_al_*` operation could take an exact-class fast path — every
/// `size()` on every ordinary list ran the whole view-discrimination chain
/// first.
///
/// These are the REAL JDK class names, not `cratonvm/internal/*` ones, which is
/// affordable here in a way it was not for `cratonvm/internal/ArrayListSubList`
/// (see the "ArrayList subList backed view" section): a view carries its state
/// in ArrayList's own resolved `elementData`/`size` slots, which sit at
/// absolute indices 1 and 2 in the real-JDK layout, past the single `this$0`
/// these classes declare. Writing there is the same undeclared-slot pattern
/// `alloc_key_set_view_object` already uses on the real
/// `ConcurrentHashMap$KeySetView`: an undeclared slot resolves to no field
/// descriptor and is left untyped rather than mistyped. The JDK's own bodies
/// for these classes must not run over that layout, which is why every name
/// here also appears in `force_native_over_real_jdk_bytecode` and in
/// `vm_exec`'s companion arm, exactly as `java/util/ArrayList` did.
pub(crate) const MAP_VIEW_CARRIERS: &[&str] = &[
    "java/util/HashMap$Values",
    "java/util/LinkedHashMap$LinkedValues",
    "java/util/TreeMap$Values",
    TM_ENTRY_SET_CARRIER,
    "java/util/Hashtable$ValueCollection",
    "java/util/concurrent/ConcurrentHashMap$ValuesView",
];

/// The ONE [`MAP_VIEW_CARRIERS`] member that holds `Map.Entry` elements rather
/// than plain values.
///
/// Named rather than spelled out at each of its uses because that asymmetry is
/// load-bearing: every predicate over `MAP_VIEW_CARRIERS` that means "values"
/// has to exclude this class, and every one that classifies a carrier by its
/// contents has to prefer it over the guess. See
/// [`view_carrier_holds_entries_by_class`].
pub(crate) const TM_ENTRY_SET_CARRIER: &str = "java/util/TreeMap$EntrySet";

/// `java/util/TreeMap$KeySet`, the one map-view carrier in NEITHER
/// [`MAP_VIEW_CARRIERS`] nor [`SET_VIEW_CARRIERS`].
///
/// It is absent from both on purpose: a `TreeMap.keySet()` view is
/// TreeSet-SHAPED — its elements live in `ts_array_table` and
/// `register_tree_set_natives` mirrors every `native_ts_*` registration onto
/// it — so neither the ArrayList-layout list nor the HashSet-layout one
/// describes it. What it DOES share with both is the failure mode: a carrier
/// the image minted itself has none of this crate's state and has to be
/// decoded through its `this$0`. See [`ts_view_source`].
pub(crate) const TM_KEY_SET_CARRIER: &str = "java/util/TreeMap$KeySet";

/// `true` iff `name` is one of the [`MAP_VIEW_CARRIERS`]. Cold path only: the
/// per-call answer comes from the `AlLayout` memo, which consults this once per
/// `ClassId`.
#[inline]
pub(crate) fn is_map_view_carrier(name: &str) -> bool {
    MAP_VIEW_CARRIERS.contains(&name)
}

/// The `(elementData, size, n_fields)` triple a [`MAP_VIEW_CARRIERS`] object
/// keeps its list state in — placed ABOVE every field the carrier class itself
/// declares, rather than at `java/util/ArrayList`'s absolute indices.
///
/// The comment on `MAP_VIEW_CARRIERS` used to argue that ArrayList's absolute
/// `elementData`/`size` (slots 1 and 2 in the real-JDK layout, with
/// `AbstractList.modCount` at 0) sit "past the single `this$0` these classes
/// declare". Five of the six carriers do declare exactly one field. **The sixth
/// does not**, and `javap -p` on JDK 25.0.3+9 says so:
///
/// ```text
/// final class java.util.LinkedHashMap$LinkedValues … {
///   final boolean reversed;              -> slot 0
///   final java.util.LinkedHashMap this$0; -> slot 1
/// ```
///
/// so `al_set_data` wrote the element buffer straight over `this$0`, and
/// [`values_view_class_source`] — which resolves `this$0` BY NAME, precisely
/// because `LinkedValues` puts it at slot 1 — read that buffer back and handed
/// an `Object[]` to `collect_entries_any` as if it were the source map. MEASURED
/// consequence before this function existed: `LinkedHashMap$LinkedValues.this$0`
/// reflected as `[Ljava.lang.Object;[len=11]` where HotSpot reports
/// `java.util.LinkedHashMap`, `values().size()` answered **0** for a
/// three-entry map in BOTH policy modes, and every element-touching operation
/// died with `AbstractMethodError: java/util/Map.isEmpty()Z` under `--jdk-only`
/// (7 of 7 rows; see `docs/known-issues/jdk-only/G22-1-*` and its G13-1
/// predecessor).
///
/// This is the same move [`al_slots_for_uncached`] already makes for
/// `java/util/Vector`: a receiver that is not an `ArrayList` does not get
/// `ArrayList`'s slots. For the five one-field carriers the answer is `(1, 2, 3)`
/// — bit-identical to what they used before, which is why the measured
/// `HashMap` / `TreeMap` / `Hashtable` / `ConcurrentHashMap` rows (all clean)
/// stay exactly as they were. Only `LinkedValues` moves, to `(2, 3, 4)`.
///
/// `class_num_total_fields` counts INHERITED fields too, which is required:
/// `ConcurrentHashMap$ValuesView` declares nothing itself and inherits `map`
/// from `ConcurrentHashMap$CollectionView`.
///
/// `None` (or a zero count) means the carrier class is not linked — a
/// synthetic-JDK build, or a call that runs before the class resolves. There is
/// then no declared field to collide with, so the historical ArrayList layout is
/// the right answer, and it is also the one [`alloc_view_carrier`] allocates
/// for. `None` propagates all the way out of `al_slots_for_uncached` so the
/// verdict is NOT cached, for the reason [`al_slots_resolved`] gives.
pub(crate) fn view_carrier_slots(ctx: &dyn NativeContext, cid: ClassId) -> Option<(usize, usize, usize)> {
    let declared = ctx.class_num_total_fields(cid);
    if declared == 0 {
        return al_slots_resolved(ctx);
    }
    Some((declared, declared + 1, declared + 2))
}

/// The carrier classes a map's SET-shaped view (`keySet()`, `entrySet()`) is
/// minted under — the [`MAP_VIEW_CARRIERS`] argument, one family over.
///
/// Every one of these was `java/util/HashSet` until 2026-08-13, so
/// `hashMap.keySet().getClass()` answered `java.util.HashSet` where HotSpot
/// says `java.util.HashMap$KeySet` (`probes/ViewClassProbe`, six rows).
///
/// Same undeclared-slot argument as the values carriers, one slot lower: a view
/// set keeps its state in HashSet's own `map` slot, which is absolute index 0
/// because nothing in `HashSet`'s superclass chain declares a field — and these
/// carriers declare exactly one field there themselves, the compiler-generated
/// `this$0`, a REFERENCE of the enclosing map's type. Storing the view backing
/// (a real `java/util/HashMap`, or `cratonvm/util/MapViewBacking` before
/// `java/util/HashMap` resolves) in it is a reference into a reference slot, so
/// no collector and no GC guard sees anything unusual; it is only the *declared
/// type* that can be a supertype mismatch, and that would matter solely to JDK
/// bytecode — which is exactly what the two force-native gates keep off these
/// classes. `Hashtable$KeySet`/`$EntrySet` are the inner views the JDK wraps in
/// a `Collections$Synchronized*`; see [`wrap_synchronized_view`].
pub(crate) const SET_VIEW_CARRIERS: &[&str] = &[
    "java/util/HashMap$KeySet",
    "java/util/HashMap$EntrySet",
    "java/util/LinkedHashMap$LinkedKeySet",
    "java/util/LinkedHashMap$LinkedEntrySet",
    "java/util/Hashtable$KeySet",
    "java/util/Hashtable$EntrySet",
    "java/util/concurrent/ConcurrentHashMap$EntrySetView",
];

/// `true` iff `name` is one of the [`SET_VIEW_CARRIERS`]. Cold path only —
/// `is_hashset_native_backed` reaches it just for a receiver that is not a
/// `HashSet` subclass, which every ordinary set is.
#[inline]
pub(crate) fn is_set_view_carrier(name: &str) -> bool {
    SET_VIEW_CARRIERS.contains(&name)
}

/// `true` iff this [`SET_VIEW_CARRIERS`] class holds `Map.Entry` elements
/// rather than keys.
///
/// By NAME, and that is the point: the twin reading
/// [`al_view_holds_entries_by_head`] classifies a carrier by its FIRST
/// ELEMENT, which a view the image built has none of -- it answers "values"
/// for an empty view, and "values" for an entry set is the wrong kind twice
/// over (wrong elements, and an `ArrayStoreException` when the caller passes a
/// `Map.Entry[]` template).
/// The elements a set-shaped receiver over `backing` should answer with: the
/// map's ENTRIES when the receiver is an entry-set carrier the IMAGE built,
/// and its keys in every other case.
///
/// [`collect_view_snapshot_ordered`] already makes that distinction, and it
/// makes it from a marker THIS CRATE writes on the backing
/// ([`view_backing_kind`]). A carrier real `entrySet()` bytecode minted carries
/// no marker and its "backing" -- resolved by name in [`hs_backing_map`] -- is
/// the source map itself, so that reading answers KEYS for an entry set.
///
/// MEASURED 2026-09-12 on `cratonvm-l1w7-t1-20260912` with `java/util/Hashtable`
/// armed: `entrySet().toArray(new Map.Entry[0])` threw
/// `ArrayStoreException: java.lang.String`. The route is worth naming because
/// it does not pass through this crate's own typed-array native at all --
/// `Hashtable.entrySet()` hands back a `Collections$SynchronizedSet`, whose
/// real `toArray(T[])` bytecode calls `c.toArray(a)` on the inner carrier,
/// the dial declines the carrier's own registration, and
/// `real_jdk_to_array_typed` (`vm/src/vm/vm_init.rs`) then drives the
/// receiver's `iterator()` -- which is `native_hs_iterator`.
///
/// So the kind has to be right in the ITERATOR, not only in the array natives.
pub(crate) fn hs_view_elements(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    backing: ObjectRef,
) -> Result<Vec<Value>, MethodCallFailed> {
    // A reversed `LinkedKeySet` / `LinkedEntrySet` (C3-1, see
    // `native_view_reversed`) hands its elements out tail first. Asked before
    // the collection, which can allocate and move `this`.
    let reversed = linked_set_view_reversed(&*ctx, this);
    hs_view_elements_forward(ctx, this, backing).map(|v| in_view_order(v, reversed))
}

/// [`hs_view_elements`] in the source's own order.
pub(crate) fn hs_view_elements_forward(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    backing: ObjectRef,
) -> Result<Vec<Value>, MethodCallFailed> {
    let foreign_entry_view = ctx
        .class_name_arc_of_id(ctx.class_id_of_object(this))
        .map(|n| set_view_carrier_holds_entries(&n))
        .unwrap_or(false)
        && view_backing_source(ctx, backing).is_none();
    if !foreign_entry_view {
        return collect_view_snapshot_ordered(ctx, backing);
    }
    // `backing` IS the source map here. Every `alloc_live_entry` inside
    // `live_entries_for_pairs` is a GC point, so the map is pinned across both
    // halves and re-read between them -- the same discipline
    // `collect_view_snapshot_ordered`'s own entry-set arm follows.
    let pin = ctx.pin_native_root(backing);
    let pairs = match collect_entries_any(ctx, backing) {
        Ok(p) => p,
        Err(e) => {
            ctx.unpin_native_roots(pin);
            return Err(e);
        }
    };
    let backing = ctx.read_native_pin(pin, backing);
    let out = live_entries_for_pairs(ctx, pin, backing, &pairs);
    ctx.unpin_native_roots(pin);
    out
}

#[inline]
pub(crate) fn set_view_carrier_holds_entries(name: &str) -> bool {
    matches!(
        name,
        "java/util/HashMap$EntrySet"
            | "java/util/LinkedHashMap$LinkedEntrySet"
            | "java/util/Hashtable$EntrySet"
            | "java/util/concurrent/ConcurrentHashMap$EntrySetView"
    )
}

/// The class a `keySet()`/`entrySet()` view over `source` should be minted
/// under, matching what the JDK's own accessor returns for that map family.
///
/// `Hashtable`/`Properties` name the INNER view here; the JDK's outer answer is
/// a `Collections$Synchronized{Set,Collection}` around it, applied separately by
/// [`wrap_synchronized_view`] so the wrapper's own bytecode does the delegating.
/// `TreeMap` is absent because its keySet is a `TreeSet`-shaped view with its
/// own carrier (`native_tm_key_set`) and its entrySet an ArrayList-shaped one
/// (`MAP_VIEW_CARRIERS`); `ConcurrentHashMap`'s keySet is
/// `alloc_key_set_view_object`'s real `KeySetView`.
pub(crate) fn set_view_carrier_for(ctx: &dyn NativeContext, source: ObjectRef, kind: i32) -> &'static str {
    let entryset = is_entryset_kind(kind);
    // Order mirrors `collect_entries_any`: LinkedHashMap is a HashMap subclass,
    // so it has to be asked first.
    if is_lhm_receiver(ctx, source) {
        if entryset {
            "java/util/LinkedHashMap$LinkedEntrySet"
        } else {
            "java/util/LinkedHashMap$LinkedKeySet"
        }
    } else if is_chm_receiver(ctx, source) {
        if entryset {
            "java/util/concurrent/ConcurrentHashMap$EntrySetView"
        } else {
            // Reached only if a CHM keySet ever arrives here; its own accessor
            // returns the real `KeySetView` via `alloc_key_set_view_object`.
            "java/util/HashMap$KeySet"
        }
    } else if receiver_facts(ctx, source).has(CF_HASHTABLE_ANCESTRY) {
        if entryset {
            "java/util/Hashtable$EntrySet"
        } else {
            "java/util/Hashtable$KeySet"
        }
    } else if entryset {
        "java/util/HashMap$EntrySet"
    } else {
        "java/util/HashMap$KeySet"
    }
}

/// Allocate a set-view carrier wide enough for both its own declared fields and
/// [`HS_FIELD_MAP`], falling back to `java/util/HashSet` if the carrier class
/// cannot be had.
///
/// The fallback is the same not-decoration argument [`alloc_view_carrier`]
/// makes: these are package-private JDK classes, and a stripped image or a
/// synthetic-JDK build that has not bootstrapped them must not turn
/// `map.keySet()` — which cannot fail — into a `NoClassDefFoundError`.
/// Store the source map in a [`SET_VIEW_CARRIERS`] object's OWN declared
/// enclosing-instance field, the way the JDK's own `keySet()`/`entrySet()`
/// accessors do.
///
/// The `values()` twin is [`store_view_carrier_backref`], and this is the half
/// that was missing. MEASURED before this change, `--jdk-only`, one case per
/// process, against HotSpot 25.0.3+9:
///
/// ```text
///   HM.keySet    this$0 = NULL              HotSpot: java.util.HashMap
///   HM.entrySet  this$0 = NULL              HotSpot: java.util.HashMap
///   HM.values    this$0 = java.util.HashMap HotSpot: java.util.HashMap   (already right)
/// ```
///
/// and the same two-of-three split for `LinkedHashMap`. So the defect is not
/// "view carriers have a null `this$0`" — it is that the SET carriers do, and
/// the `values` carrier does not, because only the `values` mint path calls
/// [`store_view_carrier_backref`].
///
/// # Why this is the containment `H4-1` §2 names
///
/// A view carrier is minted with its real JDK class and its list/backing state
/// at *undeclared* slots; the declared `this$0` is what real JDK bytecode for
/// `HashMap$KeySet.size()` dereferences. Today those bodies never run — every
/// carrier class is in `force_native_over_real_jdk_bytecode` — so a null
/// `this$0` is contained by the registrar rows, and `H4-1` §2's point is that
/// **arming removes that containment**: refuse the carrier's registrations
/// while the producer is still a bridge and the real body NPEs on the null.
///
/// Populating the field removes the coupling in the other direction: the
/// carrier becomes correct whether or not its rows are refused, so the per
/// carrier-family split `H4-1` §2 requires is no longer a *correctness*
/// precondition for arming this half of the family.
///
/// Both names are tried for the reason [`values_view_class_source`] gives:
/// `ConcurrentHashMap`'s views inherit `map` from `CollectionView` and declare
/// no `this$0`. The write is skipped when the resolved slot is not below the
/// undeclared slot [`hs_map_slot`] hands the backing map, so it can never
/// collide with the backing-map store.
pub(crate) fn store_set_view_backref(ctx: &mut dyn NativeContext, set: ObjectRef, source: ObjectRef) {
    let cid = ctx.class_id_of_object(set);
    let backing_slot = hs_map_slot(&*ctx, set).unwrap_or(HS_FIELD_MAP);
    for name in ["this$0", "map"] {
        if let Some(slot) = ctx.resolve_field_index_by_class_id(cid, name) {
            if slot < backing_slot {
                ctx.set_field(set, slot, Value::Object(Some(source)));
                return;
            }
        }
    }
}

pub(crate) fn alloc_set_view_carrier(
    ctx: &mut dyn NativeContext,
    carrier: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Ok(cid) = ctx.ensure_class_initialized(carrier) {
        if ctx.class_name_arc_of_id(cid).as_deref() == Some(carrier) {
            // Declared fields, then one undeclared slot for the backing map —
            // the slot `hs_map_slot` reads. See its doc for why not slot 0.
            let n = ctx.class_num_total_fields(cid) + HS_NUM_FIELDS;
            return Ok(ctx.alloc_object(cid, n));
        }
    }
    try_alloc_synthetic(ctx, "java/util/HashSet", HS_NUM_FIELDS)
}

/// Wrap a `Hashtable`-family view in the `Collections$Synchronized*` the JDK's
/// own `Hashtable.keySet()`/`entrySet()`/`values()` return, so `getClass()`
/// answers what HotSpot answers.
///
/// The wrapper is a REAL `Collections$SynchronizedCollection`/`$SynchronizedSet`
/// running real JDK bytecode — `Collections.synchronizedSet` already builds one
/// (`native_collections_synchronized_set`) — so every method is
/// `synchronized (mutex) { c.method(); }` onto the inner view, which is the
/// carrier this crate's natives are registered on. Nothing about the view's
/// liveness or write-through changes; one delegation hop is added.
///
/// Returns `view` unchanged if the wrapper class cannot be built, which costs
/// the `getClass()` fidelity for that run and nothing else.
pub(crate) fn wrap_synchronized_view(
    ctx: &mut dyn NativeContext,
    view: ObjectRef,
    as_set: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    let (cls, desc) = if as_set {
        (
            "java/util/Collections$SynchronizedSet",
            "(Ljava/util/Set;)V",
        )
    } else {
        (
            "java/util/Collections$SynchronizedCollection",
            "(Ljava/util/Collection;)V",
        )
    };
    // GC-SAFETY: `new_object_initialized` allocates and runs a constructor;
    // `view` is a bare Rust local passed INTO it and used again on the failure
    // path, so it is pinned and re-read.
    let view_pin = ctx.pin_native_root(view);
    let built = ctx.new_object_initialized(cls, desc, &[Value::Object(Some(view))]);
    let view = ctx.read_native_pin(view_pin, view);
    ctx.unpin_native_roots(view_pin);
    match built {
        Ok(Some(Value::Object(Some(w)))) => Ok(w),
        _ => Ok(view),
    }
}

/// `true` iff `source`'s views are the JDK's `synchronized` wrappers — i.e. it
/// is a `Hashtable` or a subclass such as `Properties`.
pub(crate) fn wants_synchronized_views(ctx: &dyn NativeContext, source: ObjectRef) -> bool {
    receiver_facts(ctx, source).has(CF_HASHTABLE_ANCESTRY)
}

/// Whether this receiver must NOT cache its view objects.
///
/// `Properties` ONLY, and the difference from `wants_synchronized_views` is
/// measured rather than reasoned. HotSpot caches a `Hashtable`'s three views
/// and does NOT cache a `Properties`'s:
///
/// ```text
/// apps/probes/ViewIdentityProbe          HotSpot   was
///   ht    keySet/values/entrySet same twice   true   false
///   props keySet same twice                  false   false
/// ```
///
/// `Hashtable.keySet()` is `if (keySet == null) keySet = synchronizedSet(...)`,
/// while `Properties` overrides it to wrap its side `ConcurrentHashMap` afresh
/// on every call. One predicate was standing for two classes that differ here,
/// and it cost four rows: `ht values().equals(ht.values())` answered FALSE,
/// because `AbstractCollection` does not override `equals` and each call handed
/// back a different object.
///
/// The old refusal gave two reasons and only the second is `Properties`-shaped:
/// the `Collections$Synchronized*` wrapper is already handled by
/// `cached_live_view`'s `unwrap_synchronized`, and it is the side-table keys
/// that only `Properties`' own `keySet()` assembles correctly. `Properties` is
/// exactly `CF_HASHTABLE_ANCESTRY` without `CF_HASHTABLE_LAYOUT`, which is a
/// distinction this file already draws for `native_map_put_evict`.
pub(crate) fn view_cache_refused(ctx: &dyn NativeContext, source: ObjectRef) -> bool {
    let facts = receiver_facts(ctx, source);
    facts.has(CF_HASHTABLE_ANCESTRY) && !facts.has(CF_HASHTABLE_LAYOUT)
}

/// The collection inside a `Collections$Synchronized{Collection,Set,List,…}`,
/// or `obj` itself when it is not one.
///
/// The inverse of [`wrap_synchronized_view`], for the native paths that need
/// the CratonVM-backed view rather than the JDK object in front of it.
pub(crate) fn unwrap_synchronized(ctx: &dyn NativeContext, obj: ObjectRef) -> ObjectRef {
    let is_sync = match ctx
        .class_name_arc_of_id(ctx.class_id_of_object(obj))
        .as_deref()
    {
        Some(n) => n.starts_with("java/util/Collections$Synchronized"),
        None => false,
    };
    if !is_sync {
        return obj;
    }
    // `c` is the field every `SynchronizedCollection` subclass inherits; by
    // name, not by index, because the synthetic-JDK stub and the real class
    // need not agree on the slot.
    match ctx.get_field_by_name(obj, "c") {
        Value::Object(Some(inner)) => inner,
        _ => obj,
    }
}

/// The class a `values()` view over `source` should be minted under, chosen to
/// match what the JDK's own `values()` returns for that map family.
///
/// `Hashtable`/`Properties` are the one deliberate approximation: the JDK wraps
/// `Hashtable$ValueCollection` in a `Collections$SynchronizedCollection`, and
/// only the inner class is modelled here. That is a strictly smaller divergence
/// than `java.util.ArrayList` and does not affect the exact-class fast path,
/// which only needs the answer to be "not `java/util/ArrayList`".
pub(crate) fn values_carrier_for(ctx: &dyn NativeContext, source: ObjectRef) -> &'static str {
    // Order matters and mirrors `collect_entries_any`: LinkedHashMap is a
    // HashMap subclass, so it has to be asked first.
    if is_lhm_receiver(ctx, source) {
        "java/util/LinkedHashMap$LinkedValues"
    } else if is_tree_map_receiver(ctx, source) {
        "java/util/TreeMap$Values"
    } else if is_chm_receiver(ctx, source) {
        "java/util/concurrent/ConcurrentHashMap$ValuesView"
    } else if receiver_facts(ctx, source).has(CF_HASHTABLE_ANCESTRY) {
        "java/util/Hashtable$ValueCollection"
    } else {
        "java/util/HashMap$Values"
    }
}

/// Allocate a view carrier, falling back to `java/util/ArrayList` if the
/// carrier class cannot be had.
///
/// The fallback is not decoration. These are package-private JDK classes; a
/// stripped image, a `--jdk-only` policy refusal, or a synthetic-JDK build that
/// has not bootstrapped them would otherwise turn `map.values()` — which cannot
/// fail — into a `NoClassDefFoundError`. Degrading to the old carrier costs the
/// `getClass()` fidelity for that run and nothing else, because every
/// `native_al_*` path still accepts an ArrayList-classed view.
///
/// The field count is derived HERE rather than passed in, and that is the
/// invariant the `LinkedValues` fix rests on: the number of slots allocated and
/// the slot indices [`al_slots_for`] later reads must both come from
/// [`view_carrier_slots`] applied to the SAME `ClassId`. Callers used to hand
/// in `al_slots(ctx).2` — ArrayList's own count — which is one slot short for a
/// two-field carrier, so `al_set_size` would silently skip its
/// out-of-bounds write and the view would report size 0.
pub(crate) fn alloc_view_carrier(
    ctx: &mut dyn NativeContext,
    carrier: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let fallback = al_slots(ctx).2;
    let is_carrier = is_map_view_carrier(carrier);
    if let Ok(cid) = ctx.ensure_class_initialized(carrier) {
        if ctx.class_name_arc_of_id(cid).as_deref() == Some(carrier) {
            let n = if is_carrier {
                view_carrier_slots(&*ctx, cid).map_or(fallback, |s| s.2)
            } else {
                fallback
            };
            return Ok(ctx.alloc_object(cid, n));
        }
    }
    if let Some(cid) = ctx.class_id_by_name(carrier) {
        let n = if is_carrier {
            view_carrier_slots(&*ctx, cid).map_or(fallback, |s| s.2)
        } else {
            fallback
        };
        return Ok(ctx.alloc_object(cid, n));
    }
    // THE LAST-RESORT ARM, and the only surviving producer of a view whose
    // class is exactly `java/util/ArrayList`. Announce it: `java/util/ArrayList`
    // is on `real_protected_stub_class_common` precisely BECAUSE no view wears
    // that class any more, and this arm is the one exception. Sticky — once a
    // run has minted one, `ArrayList`'s natives win for the rest of it.
    cratonvm_types::arraylist_view::note_arraylist_classed_view_minted();
    try_alloc_synthetic(ctx, "java/util/ArrayList", fallback)
}

/// Build a `values()` view: an ArrayList-LAYOUT snapshot, under its own carrier
/// class, whose element array stashes the source map in its last capacity slot
/// so removals write through.
pub(crate) fn make_view_list_of(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
    vals: &[Value],
    carrier: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    // cceres5 (metrics-registry Properties CCE family): the list/buffer
    // allocations below can move `source` and every snapshotted value held
    // only in this Rust slice; storing the raw refs would bake pre-move
    // addresses into the view. Pin + re-read across the allocations.
    let (elem_base, val_handles) = pin_value_slice(ctx, vals);
    let source_pin = ctx.pin_native_root(source);
    let first_pin = if elem_base == usize::MAX {
        source_pin
    } else {
        elem_base
    };
    let list = alloc_view_carrier(ctx, carrier)?;
    let list_pin = ctx.pin_native_root(list);
    let cap = std::cmp::max(vals.len(), AL_DEFAULT_CAPACITY) + 1;
    // `buf` is pinned for the same reason `tm_resync_view_inner` pins its own:
    // it is the only reference to a freshly allocated array until `al_set_data`
    // lands, and `make_list_of` next door has always pinned its equivalent. The
    // window here is short and today contains no allocation, which is exactly
    // how the twin in `tm_resync_view_inner` read before a `tm_obj_key` call
    // grew into it.
    let buf = alloc_ref_array(ctx, cap);
    let buf_pin = ctx.pin_native_root(buf);
    for (i, val) in vals.iter().enumerate() {
        let val = read_pinned_elem(ctx, val_handles[i], *val);
        let buf = ctx.read_native_pin(buf_pin, buf);
        ctx.set_array_element(buf, i, val);
    }
    let source = ctx.read_native_pin(source_pin, source);
    let buf = ctx.read_native_pin(buf_pin, buf);
    ctx.set_array_element(buf, cap - 1, Value::Object(Some(source)));
    let list = ctx.read_native_pin(list_pin, list);
    store_view_carrier_backref(ctx, list, source);
    let list = ctx.read_native_pin(list_pin, list);
    let buf = ctx.read_native_pin(buf_pin, buf);
    al_set_data(ctx, list, buf);
    al_set_size(ctx, list, vals.len() as i32);
    ctx.unpin_native_roots(first_pin);
    Ok(list)
}

thread_local! {
    /// Sources currently being re-read by [`adopt_fresh_view_backing`], keyed
    /// on the source's own pointer.
    ///
    /// The accessors this calls (`Properties.keySet()` / `.entrySet()`)
    /// materialise a view directly from their storage and do not resync
    /// anything, so today the re-entrant case cannot arise. It is guarded
    /// anyway, for the reason `ITER_COLLECT_GUARD` was added: a source whose
    /// accessor DOES read one of its own views turns "refresh on read" into
    /// unbounded recursion, and that shape was found live once already
    /// (the JSSE-connector stack overflow during TLS init).
    static ADOPT_VIEW_GUARD: std::cell::RefCell<std::collections::HashSet<u64>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

/// RAII removal from [`ADOPT_VIEW_GUARD`], including on unwind — see
/// [`IterCollectGuard`], which this mirrors.
pub(crate) struct AdoptViewGuard {
    key: u64,
    inserted: bool,
}
impl Drop for AdoptViewGuard {
    fn drop(&mut self) {
        if self.inserted {
            ADOPT_VIEW_GUARD.with(|g| {
                g.borrow_mut().remove(&self.key);
            });
        }
    }
}

/// Refresh a STATIC view (see [`VIEW_KIND_KEYSET_STATIC`]) by asking `source`
/// for a fresh view of the same kind and pointing `set` at the fresh view's
/// backing.
///
/// The fresh view is built by the source's own accessor native, which is the
/// only code that knows how to assemble that source's contents; adopting its
/// backing wholesale means this function never has to know. The fresh backing
/// carries the same `(source, kind)` markers — `alloc_view_backing` wrote them
/// — so `set` stays a live view with write-through afterwards.
///
/// Refusals from the accessor are propagated. A non-view answer (no backing to
/// adopt) leaves `set` exactly as it was: a stale view is a worse answer than a
/// fresh one, but an emptied view is worse than both.
pub(crate) fn adopt_fresh_view_backing(
    ctx: &mut dyn NativeContext,
    set: ObjectRef,
    source: ObjectRef,
    method: &str,
    descriptor: &str,
) -> Result<(), MethodCallFailed> {
    let guard_key = source.as_ptr() as u64;
    let inserted = ADOPT_VIEW_GUARD.with(|g| g.borrow_mut().insert(guard_key));
    let _guard = AdoptViewGuard {
        key: guard_key,
        inserted,
    };
    if !inserted {
        return Ok(());
    }
    // GC-SAFETY: the accessor is a full native dispatch that allocates a view,
    // its backing and its bucket array — `set` is a bare Rust local used for
    // the store afterwards, so it is pinned and re-read (the same shape
    // `native_hs_iterator` uses around its own `resync_view_set` call).
    let set_pin = ctx.pin_native_root(set);
    let fresh = ctx.invoke_virtual(source, method, descriptor, &[]);
    let fresh = match fresh {
        Ok(Some(Value::Object(Some(f)))) => f,
        Err(e) => {
            ctx.unpin_native_roots(set_pin);
            return Err(e);
        }
        _ => {
            ctx.unpin_native_roots(set_pin);
            return Ok(());
        }
    };
    // `Hashtable`/`Properties` accessors hand back the JDK's
    // `Collections$Synchronized*` wrapper (see `wrap_synchronized_view`), so the
    // fresh view is one level in. Without this the backing lookup answers `None`
    // and the resync becomes a silent no-op — which is the very bug this
    // function exists to close.
    let fresh = unwrap_synchronized(ctx, fresh);
    let backing = match hs_backing_map(ctx, fresh) {
        Some(b) => b,
        None => {
            ctx.unpin_native_roots(set_pin);
            return Ok(());
        }
    };
    let set = ctx.read_native_pin(set_pin, set);
    hs_set_backing_map(ctx, set, backing);
    ctx.unpin_native_roots(set_pin);
    Ok(())
}

/// Refresh a keySet/entrySet view's backing HashSet from its live source map,
/// so reads (`contains`/`size`/`isEmpty`/`iterator`) reflect mutations made
/// DIRECTLY to the source map after the view was obtained — JDK keySet/entrySet
/// views are live. Without this, `map.keySet()` then `map.remove(k)` left the
/// snapshot reporting `contains(k)==true` (Tomcat `ParameterMap` setUp asserts
/// the opposite). No-op for an ordinary HashSet (backing carries no source).
/// # `set` is `&mut` for the reason `tm_sync_native_state` is — N4 of `WORKER-5-NOTE-10`
///
/// This funnel ALLOCATES (`alloc_ref_array` for the bucket table,
/// `try_alloc_synthetic` per entry), so a moving collector can relocate the
/// RECEIVER inside it. It used to take `set` by value and return `()`, which
/// gave its ten callers no way to learn the new address: each went straight on
/// to `hs_backing_map(ctx, this)`, and that keys the side table on
/// `widened_obj_key(this)` — off a from-space address it mints a fresh slot,
/// finds nothing, and the caller takes its "no backing" branch. That is exactly
/// how `TreeMap.size()` came to answer 0 on an unmutated view.
///
/// **LATENT, not measured.** Unlike the TreeMap case this has no reproduction:
/// a `HashMap` keySet/entrySet walked under the same `--Xmx 64m --nojit`
/// pressure answers correctly every time, because the early returns above
/// usually fire before the allocating path. It is fixed because it is the same
/// SHAPE, the fix is mechanical, and `&mut` makes the eight unprotected call
/// sites compile errors rather than a grep somebody has to repeat. Two of the
/// ten (`native_hs_iterator`, `native_hs_remove_if`) already re-read through a
/// pin and were never exposed.
pub(crate) fn resync_view_set(
    ctx: &mut dyn NativeContext,
    set: &mut ObjectRef,
) -> Result<(), MethodCallFailed> {
    let pin = ctx.pin_native_root(*set);
    let r = resync_view_set_inner(ctx, *set);
    *set = ctx.read_native_pin(pin, *set);
    ctx.unpin_native_roots(pin);
    r
}

pub(crate) fn resync_view_set_inner(
    ctx: &mut dyn NativeContext,
    set: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let backing = match hs_backing_map(ctx, set) {
        Some(b) => b,
        None => return Ok(()),
    };
    let source = match view_backing_source(ctx, backing) {
        Some(s) => s,
        None => return Ok(()),
    };
    let kind = view_backing_kind(ctx, backing);
    // A STATIC view's source cannot be read back by walking its fields, so it
    // is refreshed by asking the source for a fresh view and adopting that
    // view's backing. Until 2026-08-13 this arm returned without doing
    // anything, which is what `probes/MapViewBehaviourProbe` measured as
    //
    // ```text
    // props.keys.afterPut.size      HotSpot 3   CratonVM 2
    // props.entries.afterPut.size   HotSpot 3   CratonVM 2
    // ```
    //
    // — a `Properties` view was live in the WRITE direction only.
    if let Some((method, desc)) = static_view_accessor(kind) {
        return adopt_fresh_view_backing(ctx, set, source, method, desc);
    }
    // ── Rebuild elision ────────────────────────────────────────────────────
    //
    // Everything below rebuilds the whole backing — a fresh bucket array plus
    // one `native_map_put` per key — and it runs from `size()`, `iterator()`
    // and nine other readers. `StringUtils.toStringArray(map.keySet())` in
    // Spring's `getPropertyNames()` therefore rebuilt a 1000-entry hash map
    // twice per call, 101 910 times per run of
    // `ConfigurationPropertySourcesTests`.
    //
    // The view is meant to be LIVE, and resyncing is how it stays live. What
    // makes the rebuild skippable is that "live" only requires the contents to
    // agree with the source AT THE MOMENT OF THE READ, and a monotonic
    // generation on the source answers that in one field load. See
    // [`view_source_generation`] for which `(kind, source)` pairs have such a
    // generation at all — every other pair falls through and rebuilds exactly
    // as before.
    //
    // The guard may OVER-invalidate freely; it must never UNDER-invalidate.
    // `remove(k1); put(k2)` leaves the size unchanged, which is why the signal
    // is a counter and not the size.
    let source_gen = view_source_generation(&*ctx, source, kind);
    let mut verify_expected: Option<Vec<i64>> = None;
    if let Some(gen) = source_gen {
        if view_backing_stamp(&*ctx, backing) == Some(gen) {
            if !verify_map_view_cache() {
                MAP_VIEW_RESYNC_SKIPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Ok(());
            }
            // Verify mode: take the fast path's decision, then rebuild anyway
            // and compare. The elision's soundness is otherwise an argument
            // about which mutators move the counter, and an argument is not a
            // measurement.
            verify_expected = Some(view_element_fingerprints(ctx, backing));
        }
    }
    MAP_VIEW_RESYNC_RAN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Rebuild the backing map's contents from the live source. Derive the
    // capacity from the current bucket-array length via `map_state` (NOT a raw
    // `get_field(MAP_FIELD_CAPACITY)`): a real-layout view backing keeps its
    // bucket array at the JDK `table` slot, which may be the same absolute slot
    // (2) as the synthetic `capacity`, so reading slot 2 as an Int would yield
    // `None`.
    let cap = map_state(ctx, backing).2.max(MAP_DEFAULT_CAPACITY as i32);
    // Rebuilding a live view allocates before the nested Map.put calls below.
    // These references originate in the view's old backing graph, so preserve
    // them through that allocation and refresh them before every nested native
    // dispatch.
    let roots_base = ctx.pin_native_root(backing);
    let source_pin = ctx.pin_native_root(source);
    let buckets = alloc_ref_array(ctx, cap as usize);
    let buckets_pin = ctx.pin_native_root(buckets);
    let backing = ctx.read_native_pin(roots_base, backing);
    let source = ctx.read_native_pin(source_pin, source);
    let buckets = ctx.read_native_pin(buckets_pin, buckets);
    // Synthetic slot-0 bucket store (what map_state / iterator / remove read),
    // plus a mirror into the backing's own `table` slot so JDK
    // `keySet()/entrySet().spliterator()` bytecode (which reads `getfield
    // map.table` directly) sees the live array instead of a stale value. The
    // synthetic `Int(capacity)` is written only on our fabricated layout,
    // otherwise it clobbers the bucket array and `arraylength` panics with
    // `expected object reference, got int(16)`. This mirrors the dual-storage
    // `map_resize` already maintains.
    //
    // JDK-ONLY-LAYOUT (L2): a view backing is EITHER a real `java/util/HashMap`
    // (the branch above, ≥16 slots) or the fabricated
    // `cratonvm/util/MapViewBacking`, and `publish_map_table` tells them apart
    // by asking each receiver for its own `table` field — the fixed
    // `java/util/HashMap` lookup answered `2` for both.
    publish_map_table(ctx, backing, buckets, cap);
    // Reset modCount so the JDK spliterator's CME check stays consistent.
    try_set_jdk_map_field(ctx, backing, "modCount", Value::Int(0));
    set_map_size(ctx, backing, 0);
    // The three lines above reset the backing through its FIELDS, which the
    // integer overlay does not live in -- see [`hm_int_fast_purge`]. Without
    // this the re-put loop below adds the current keys to an overlay that
    // still holds every key this view has ever shown, and `size()` becomes a
    // high-water mark: `probes/MapGenProbe` reported `keySet().size() == 6`
    // for a map that a `remove` had taken to 5 and a `clear` to 0.
    hm_int_fast_purge(&*ctx, backing);
    let sentinel = Value::Int(1);
    if kind == VIEW_KIND_ENTRYSET {
        let entries = collect_entries_any(ctx, source)?;
        let entry_pins: Vec<(usize, usize)> = entries
            .iter()
            .map(|(k, v)| (pin_value(ctx, *k), pin_value(ctx, *v)))
            .collect();
        for ((k, v), (key_pin, value_pin)) in entries.into_iter().zip(entry_pins) {
            // 3-field entry (key@0, value@1, sourceMap@2) so `Entry.setValue`
            // writes through to the live source map. This `resync` path runs on
            // every entrySet `iterator()`/`size()` read, so without the source
            // field here the iterator hands back detached 2-field entries and
            // `setValue` is silently a no-op (cf. native_map_entry_set).
            // `AbstractMap$SimpleEntry`, not the interface `Map$Entry` — HotSpot
            // cannot produce an instance of an interface (`H16-2` N2).
            let entry = try_alloc_synthetic(ctx, "java/util/AbstractMap$SimpleEntry", 3)?;
            let backing = ctx.read_native_pin(roots_base, backing);
            let source = ctx.read_native_pin(source_pin, source);
            let k = read_pinned_elem(ctx, key_pin, k);
            let v = read_pinned_elem(ctx, value_pin, v);
            ctx.set_field(entry, 0, k);
            ctx.set_field(entry, 1, v);
            ctx.set_field(entry, 2, Value::Object(Some(source)));
            // An entry's Java hash is key.hashCode() ^ value.hashCode().
            // Calling native_map_put here therefore invokes PersistentSet's
            // hashCode while Hibernate is initializing its batch queue, which
            // recursively reloads the same collection. Entry-set views must
            // use identity buckets, as native_map_entry_set does.
            let backing = ctx.read_native_pin(roots_base, backing);
            let (buckets, size, capacity) = map_state(ctx, backing);
            if let Some(buckets) = buckets {
                let buckets_pin = ctx.pin_native_root(buckets);
                let entry_pin = ctx.pin_native_root(entry);
                let hash = ctx.identity_hash_code(entry);
                let index = map_bucket_index(hash, capacity);
                let head = match ctx.get_array_element(buckets, index) {
                    Value::Object(head) => head,
                    _ => None,
                };
                let node = map_alloc_node(ctx, entry, sentinel, hash, head)?;
                let backing = ctx.read_native_pin(roots_base, backing);
                let buckets = ctx.read_native_pin(buckets_pin, buckets);
                ctx.set_array_element(buckets, index, Value::Object(Some(node)));
                set_map_size(ctx, backing, size + 1);
                ctx.unpin_native_roots(entry_pin);
                ctx.unpin_native_roots(buckets_pin);
            }
            // cceres3 (PIN-DANGLING root cause, live-captured): DO NOT unpin
            // key_pin/value_pin per iteration. `unpin_native_roots` TRUNCATES
            // the pin stack — the pairs were all pinned up front, so
            // unpinning iteration 0's pins wiped iteration 1..N's handles and
            // every later `read_pinned_elem` silently degraded to the raw,
            // possibly-stale snapshot value (the stored-stale set elements
            // behind the hs_hash_code/hs_iterator canary hits). The single
            // `unpin_native_roots(roots_base)` at function end releases the
            // whole range correctly.
            let _ = (key_pin, value_pin);
        }
    } else if let Some((keys, hashes)) = view_source_keys_hashed(&*ctx, source) {
        // `CRATONVM_COMPAT_VIEW_BUILD_BY_NODE`: from the stored hashes, no
        // `hashCode()` and no `equals()` (see `view_build_by_node_enabled`).
        // The keys are pinned before the first allocation.
        let (_, key_pins) = pin_value_slice(ctx, &keys);
        let backing = ctx.read_native_pin(roots_base, backing);
        let _ = fill_view_backing_hashed(ctx, roots_base, backing, &keys, &key_pins, &hashes);
    } else {
        let keys = collect_keys_any(ctx, source);
        let (_, key_pins) = pin_value_slice(ctx, &keys);
        for (k, key_pin) in keys.into_iter().zip(key_pins) {
            let backing = ctx.read_native_pin(roots_base, backing);
            let k = read_pinned_elem(ctx, key_pin, k);
            let _ = native_map_put(ctx, &[Value::Object(Some(backing)), k, present_marker(k)]);
        }
    }
    // Stamp with the generation read BEFORE the collection above, not after:
    // `collect_keys_any` dispatches `hashCode()`/`equals()` bytecode, which may
    // itself mutate the source. Stamping the older value can only make the next
    // read rebuild unnecessarily, which is the safe direction.
    let backing = ctx.read_native_pin(roots_base, backing);
    match source_gen {
        Some(g) => set_view_backing_stamp(ctx, backing, g),
        // No usable signal for this pair — make sure no stale stamp survives,
        // so a later change of kind/source can never resurrect one.
        None => clear_view_backing_stamp(ctx, backing),
    }
    if let Some(expected) = verify_expected {
        let actual = view_element_fingerprints(ctx, backing);
        if expected != actual {
            // The divergence alone says the guard under-invalidated; it does
            // not say which half was wrong. Read the generation back from the
            // source now, after the rebuild, beside the value the elision
            // compared: a `gen now` equal to `gen compared` means the mutator
            // moved nothing at all (the int-overlay `put` did exactly that
            // until 2026-08-27), while a larger one means the bump landed
            // where this view's stamp cannot see it.
            let source = ctx.read_native_pin(source_pin, source);
            let gen_now = map_itr_mod_count(&*ctx, source);
            let stamp = view_backing_stamp(&*ctx, backing);
            let src_cid = ctx.class_id_of_object(source);
            let src_name = ctx
                .class_name_of_id(src_cid)
                .unwrap_or_else(|| format!("<cid {src_cid:?}>"));
            let mut msg = String::new();
            use std::fmt::Write as _;
            let _ = writeln!(
                msg,
                "CRATONVM_VERIFY_MAP_VIEW_CACHE: the rebuild elision would have served STALE"
            );
            let _ = writeln!(
                msg,
                "  contents for a kind={kind} view - the generation guard did not move, but"
            );
            let _ = writeln!(msg, "  the contents did.");
            let _ = writeln!(msg, "  source: {src_name}");
            let _ = writeln!(
                msg,
                "  gen compared={source_gen:?} gen now={gen_now:?} backing stamp={stamp:?}"
            );
            let _ = writeln!(msg, "  elided  (identity hashes): {expected:?}");
            let _ = writeln!(msg, "  rebuilt (identity hashes): {actual:?}");
            panic!("{msg}");
        }
    }
    ctx.unpin_native_roots(roots_base);
    Ok(())
}

/// Order-independent fingerprints of a view backing's elements, for
/// [`verify_map_view_cache`].
///
/// Identity hashes, not addresses: the rebuild this compares against allocates,
/// so a moving collector relocates every element between the two snapshots and
/// a pointer comparison would report a divergence on every single call. Sorted
/// because bucket order is not part of what the elision promises to preserve.
pub(crate) fn view_element_fingerprints(ctx: &mut dyn NativeContext, backing: ObjectRef) -> Vec<i64> {
    let mut out: Vec<i64> = map_collect_keys(ctx, backing)
        .into_iter()
        .map(|v| match v {
            Value::Object(Some(o)) => ctx.identity_hash_code(o) as i64,
            Value::Object(None) => i64::MIN,
            Value::Int(i) => i as i64,
            Value::Long(l) => l,
            other => format!("{other:?}").len() as i64,
        })
        .collect();
    out.sort_unstable();
    out
}

/// Collect keys from any natively-modelled map (HashMap / LinkedHashMap /
/// TreeMap / CHM), dispatching on the concrete backend.
pub(crate) fn collect_keys_any(ctx: &mut dyn NativeContext, source: ObjectRef) -> Vec<Value> {
    // Ancestry-aware LHM check — see collect_entries_any.
    if is_lhm_receiver(ctx, source) {
        return lhm_collect_keys(ctx, source);
    }
    if is_tree_map_receiver(ctx, source) {
        return tm_collect_pairs(ctx, source)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
    }
    map_collect_keys(ctx, source)
}

/// Snapshot a HashSet's elements in *iteration order* for the order-sensitive
/// read paths (iterator / toArray / forEach / stream / toString / addAll-source).
///
/// An ordinary (non-view) HashSet has no source map, so we read its elements
/// straight from `backing` in bucket order. But a keySet/entrySet/values *view*
/// must iterate in its **source map's** encounter order: insertion order for a
/// `LinkedHashMap`, sorted order for a `TreeMap`, and HotSpot-faithful bucket
/// order for a plain `HashMap`. We therefore collect directly from the source
/// whenever one is present.
///
/// Walking the view's own HashSet backing instead is wrong for an `entrySet`:
/// that backing buckets each freshly-built `Map.Entry` by the entry object's
/// *identity hash* (a keySet backing buckets by the key's hash, which happens
/// to reproduce the source's layout), so an entrySet walk re-orders entries by
/// entry-hash and diverges from both `keySet()` and HotSpot. That surfaced as
/// the SD-JWT claim-ordering bug and, more broadly, any code that compares
/// `entrySet()`/`values()` encounter order against HotSpot. Collecting from the
/// source keeps `entrySet()` order identical to `keySet()` for every map type.
///
/// keySet → the source keys; entrySet → freshly built `Map.Entry` objects over
/// the source's ordered `(key,value)` pairs (slot 0 = key, slot 1 = value, the
/// layout `native_lhm_entry_set` and `native_hs_remove`'s key-extraction use).
pub(crate) fn collect_view_snapshot_ordered(
    ctx: &mut dyn NativeContext,
    backing: ObjectRef,
) -> Result<Vec<Value>, MethodCallFailed> {
    if let Some(source) = view_backing_source(ctx, backing) {
        // A STATIC view (Properties): its contents were put in the backing by
        // the source's own accessor — either at construction or by the
        // `adopt_fresh_view_backing` this read's `resync_view_set` just ran —
        // so read them straight from the backing, the same path a plain set
        // takes. Walking the source instead would recurse into
        // `Properties.entrySet()`. Both kinds' elements are already the right
        // objects (entries for one, keys for the other).
        if static_view_accessor(view_backing_kind(ctx, backing)).is_some() {
            return Ok(map_collect_keys(ctx, backing));
        }
        if view_backing_kind(ctx, backing) == VIEW_KIND_ENTRYSET {
            // `CRATONVM_COMPAT_ENTRY_SET_NODES`: the elements ARE the source's
            // nodes, as on the JDK (see `entry_set_nodes_enabled`). Pure reads.
            if let Some(nodes) = entry_view_nodes(&*ctx, source) {
                return Ok(nodes.into_iter().map(|n| Value::Object(Some(n))).collect());
            }
            // Family-1 fix (cce0079): every `alloc_live_entry` allocation can
            // move `source`, the remaining snapshot keys/values, AND the
            // entries already allocated by earlier iterations — pin all of
            // them and hand back only refreshed addresses (canary-caught
            // live via `native_hs_iterator` during WildFly domain boot).
            let src_pin = ctx.pin_native_root(source);
            let collected = collect_entries_any(ctx, source);
            let entries = unpin_on_err(ctx, src_pin, collected)?;
            let source = ctx.read_native_pin(src_pin, source);
            let (keys, vals): (Vec<Value>, Vec<Value>) = entries.into_iter().unzip();
            let (_, kh) = pin_value_slice(ctx, &keys);
            let (_, vh) = pin_value_slice(ctx, &vals);
            let mut source = source;
            let mut raw_entries: Vec<ObjectRef> = Vec::with_capacity(keys.len());
            let mut entry_handles: Vec<usize> = Vec::with_capacity(keys.len());
            for i in 0..keys.len() {
                let k = read_pinned_elem(ctx, kh[i], keys[i]);
                let v = read_pinned_elem(ctx, vh[i], vals[i]);
                let e = alloc_live_entry(ctx, "java/util/AbstractMap$SimpleEntry", k, v, source)?;
                source = ctx.read_native_pin(src_pin, source);
                entry_handles.push(ctx.pin_native_root(e));
                raw_entries.push(e);
            }
            let out: Vec<Value> = raw_entries
                .iter()
                .zip(&entry_handles)
                .map(|(e, &h)| Value::Object(Some(ctx.read_native_pin(h, *e))))
                .collect();
            ctx.unpin_native_roots(src_pin);
            return Ok(out);
        }
        return Ok(collect_keys_any(ctx, source));
    }
    Ok(map_collect_keys(ctx, backing))
}

/// Refresh an ArrayList-backed map view (`values()` OR TreeMap `entrySet()`)
/// from its live source map, preserving the trailing source-marker slot.
/// Mirrors `resync_view_set`. The element kind is inferred from the list's
/// current head element: a `Map.Entry`/`HashMap$Entry` element means this is an
/// entrySet view (rebuild with Entry objects), otherwise a values view (raw
/// values). Inferring avoids a separate kind marker and keeps TreeMap
/// entrySet's Entry elements from being clobbered with bare values (which
/// caused `ClassCastException: Integer cannot be cast to Map$Entry`).
/// True only for the JDK-internal synthetic `Map.Entry` classes we materialise
/// for `entrySet()` views (`java/util/HashMap$Entry` via `tm_make_entry`,
/// `java/util/Map$Entry`, `java/util/LinkedHashMap$Entry`, ...).
///
/// Used to tell an `entrySet()` view (elements are entries) apart from a
/// `values()` view (elements are values) when both are ArrayList-backed views
/// sharing the trailing-source-slot marker. The previous test —
/// `class_name.contains("Entry")` — misfired on application VALUE types whose
/// simple name merely ends in "Entry": a `Map<String, PathEntry>.values()`
/// view (WildFly `org/jboss/as/controller/services/path/PathEntry`) was
/// rebuilt as `HashMap$Entry` objects, so `values().iterator().next()` yielded
/// an entry and `PathManagerService.addPathManagerResources`'s
/// `checkcast PathEntry` threw `ClassCastException`. Applications cannot define
/// classes in the sealed `java/util` package, so the prefix+suffix test is
/// exact and cannot match an app type.
pub(crate) fn is_synthetic_map_entry_class(name: &str) -> bool {
    // `$Entry` covers the legacy fabricated names (HashMap$Entry, TreeMap$Entry,
    // LinkedHashMap$Entry, Map$Entry); `$SimpleEntry`/`$SimpleImmutableEntry`
    // cover the AbstractMap entries now materialised for TreeMap/HashMap/
    // LinkedHashMap entrySet views (a real Map.Entry impl, so reflection sees
    // getKey/getValue). All live under the sealed `java/util/` package, so the
    // prefix+suffix test cannot match an application value type named `*Entry`.
    name.starts_with("java/util/")
        && (name.ends_with("$Entry")
            || name.ends_with("$SimpleEntry")
            || name.ends_with("$SimpleImmutableEntry"))
}

/// Mint one live `java.util.AbstractMap$SimpleEntry` per collected pair.
///
/// `source_pin` must ALREADY root `source`; the pairs are pinned here. Shared by
/// [`resync_values_view`] and [`vc_route`] so the two entry-view rebuilds cannot
/// drift — before this existed only the first had the branch at all, and
/// `vc_route` re-expressed an `entrySet()` receiver as its map's VALUES.
///
/// GC-SAFETY: `alloc_live_entry` allocates once per pair, so an earlier
/// iteration's allocation can relocate a later iteration's key/value AND the
/// source map that every minted entry embeds. Both are re-read through their
/// pins immediately before each use.
pub(crate) fn live_entries_for_pairs(
    ctx: &mut dyn NativeContext,
    source_pin: usize,
    source: ObjectRef,
    pairs: &[(Value, Value)],
) -> Result<Vec<Value>, MethodCallFailed> {
    let mut flat: Vec<Value> = Vec::with_capacity(pairs.len() * 2);
    for (k, v) in pairs {
        flat.push(*k);
        flat.push(*v);
    }
    let (_flat_pin, flat_handles) = pin_value_slice(ctx, &flat);
    let mut out = Vec::with_capacity(pairs.len());
    for i in 0..pairs.len() {
        let k = read_pinned_elem(ctx, flat_handles[2 * i], flat[2 * i]);
        let v = read_pinned_elem(ctx, flat_handles[2 * i + 1], flat[2 * i + 1]);
        let source_cur = ctx.read_native_pin(source_pin, source);
        // Real Map.Entry impl (see `native_tm_entry_set`): reflective property
        // access over rebuilt TreeMap entrySet elements.
        let entry = alloc_live_entry(ctx, "java/util/AbstractMap$SimpleEntry", k, v, source_cur)?;
        out.push(Value::Object(Some(entry)));
    }
    Ok(out)
}

pub(crate) fn resync_values_view(
    ctx: &mut dyn NativeContext,
    list: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let source = match values_view_source(ctx, list) {
        Some(s) => s,
        None => return Ok(list),
    };
    // A COLLECTION SOURCE IS NOT REBUILT HERE. The three snapshot-iterator
    // families put a `TreeSet`, an `ArrayDeque` or a `PriorityQueue` behind
    // this marker so their iterators' `remove()` can write through; the read
    // direction must not follow, because `collect_entries_any` below knows the
    // six MAP families and none of these is one -- it would answer nothing and
    // rebuild the snapshot as EMPTY, on the `next()` path.
    //
    // The snapshot is also the only correct content: it is taken once, in the
    // family's own iteration order, exactly as the real iterator walks it.
    // Liveness is not the contract here -- fail-fast is, and that is
    // `al_view_itr_check_comod`'s job through `family_snapshot_generation`.
    //
    // Same shape, and the same reason, as `resync_ts_view`'s
    // `is_tree_map_receiver` guard: only a MAP source is rebuilt.
    if family_snapshot_generation(&*ctx, source).is_some() {
        return Ok(list);
    }
    let list_pin = ctx.pin_native_root(list);
    let source_pin = ctx.pin_native_root(source);
    let list = ctx.read_native_pin(list_pin, list);
    let source = ctx.read_native_pin(source_pin, source);
    // Determine whether elements are Map.Entry (entrySet) by inspecting the
    // current head element's class — restricted to our synthetic entry classes
    // so an app value type named `*Entry` is not misread as an entry. Factored
    // into `al_view_holds_entries` so this reading and `al_is_values_view`'s
    // cannot drift; the body is unchanged (both are pure field reads, so
    // hoisting it behind a call introduces no GC point).
    let is_entry_view = al_view_holds_entries(ctx, list);
    // A reversed `LinkedValues` (C4-1, see `mint_reversed_linked_values`)
    // holds its source's values tail first. Asked before the collection, which
    // can run Java and move `list`.
    let reversed = values_view_reversed(&*ctx, list);
    let collected = collect_entries_any(ctx, source);
    let entries = unpin_on_err(ctx, list_pin, collected)?;

    // GC-SAFETY: the entry-view branch below allocates a new `SimpleEntry`
    // per element via `alloc_live_entry`, and `alloc_ref_array` further down
    // allocates the new backing array -- both can trigger a moving GC.
    // `list` (needed by the trailing `al_set_data`/`al_set_size`) and
    // `source` (re-embedded into every minted entry, so at risk again on the
    // very next loop iteration) are pinned across these calls and re-read
    // before each subsequent use; the entry-view branch additionally pins
    // every key/value already collected into `entries` (a plain Rust `Vec`,
    // not itself a GC root) since an earlier iteration's allocation can
    // relocate a later iteration's key/value before it's read. Confirmed
    // live via CRATONVM_DBG_STALE_OBJREF: native_al_stream ->
    // resync_values_view -> al_set_data -> al_slots_for dereferencing a
    // stale `list`.

    let mut vals: Vec<Value> = if is_entry_view {
        live_entries_for_pairs(ctx, source_pin, source, &entries)?
    } else {
        entries.into_iter().map(|(_, v)| v).collect()
    };
    if reversed {
        vals.reverse();
    }

    let (_vals_pin, vals_handles) = pin_value_slice(ctx, &vals);
    let cap = vals.len().max(AL_DEFAULT_CAPACITY) + 1;
    // Pinned for the reason spelled out on `tm_resync_view_inner`'s publish
    // block: a freshly allocated array whose only reference is a Rust local is
    // one allocation away from being written into a from-space object.
    let buf = alloc_ref_array(ctx, cap);
    let buf_pin = ctx.pin_native_root(buf);
    let list = ctx.read_native_pin(list_pin, list);
    let source = ctx.read_native_pin(source_pin, source);
    let vals = read_value_slice(ctx, &vals_handles, &vals);
    let buf = ctx.read_native_pin(buf_pin, buf);
    for (i, v) in vals.iter().enumerate() {
        ctx.set_array_element(buf, i, *v);
    }
    ctx.set_array_element(buf, cap - 1, Value::Object(Some(source)));
    al_set_data(ctx, list, buf);
    al_set_size(ctx, list, vals.len() as i32);
    let list = ctx.read_native_pin(list_pin, list);
    ctx.unpin_native_roots(list_pin);
    Ok(list)
}

/// If `list`'s element array carries a `values()`-view source-map marker in
/// its last capacity slot, return it. The marker is only present when the
/// array is longer than the logical size (so a normal full ArrayList, whose
/// `length == size`, never matches) and the trailing slot is non-null (spare
/// capacity slots of a normal list are always null).
pub(crate) fn values_view_source(ctx: &dyn NativeContext, list: ObjectRef) -> Option<ObjectRef> {
    let (data, size) = al_state(ctx, list);
    view_source_in(ctx, data?, size)
}

/// The marker test of [`values_view_source`], for a caller that already holds
/// the receiver's `(elementData, size)` pair.
///
/// Split out so the hot natives can answer "is this a view?" from the
/// `al_state` they had to run anyway. Reading it through `values_view_source`
/// instead cost a second full `al_state` — two more `get_field`s and a second
/// layout classification — on every `size()`/`isEmpty()`/`get()` of every
/// ordinary list, which is the overwhelming majority of the calls.
#[inline]
pub(crate) fn view_source_in(ctx: &dyn NativeContext, data: ObjectRef, size: i32) -> Option<ObjectRef> {
    let dlen = ctx.array_length(data) as i32;
    if dlen > size {
        if let Value::Object(Some(src)) = ctx.get_array_element(data, (dlen - 1) as usize) {
            return Some(src);
        }
    }
    None
}

/// `true` iff a marker-carrying view list holds `Map.Entry` elements (an
/// `entrySet()` view) rather than plain values (a `values()` view).
///
/// Restricted to our synthetic entry classes so an app value type named
/// `*Entry` is not misread as an entry — the same test [`resync_values_view`]
/// applies, factored out so the two callers cannot drift. An EMPTY view answers
/// `false`: with no head element there is nothing to classify, and every caller
/// of this predicate wants `values()` to be the default reading.
///
/// The head-element reading is now the FALLBACK, not the answer: see
/// [`view_carrier_holds_entries_by_class`] for the two ways it reached "values"
/// on a genuine `entrySet()` and what asking the class first closes.
pub(crate) fn al_view_holds_entries(ctx: &dyn NativeContext, list: ObjectRef) -> bool {
    if let Some(by_class) = view_carrier_holds_entries_by_class(ctx, list) {
        if cratonvm_types::flags::runtime_var("CRATONVM_DBG_VIEWKIND").is_ok() {
            let by_head = al_view_holds_entries_by_head(ctx, list);
            if by_head != by_class {
                let (data, size) = al_state(ctx, list);
                eprintln!(
                    "[VIEWKIND] carrier={} class_says_entries={by_class} head_says_entries={by_head} \
                     data={} size={size} dlen={} head_class={}",
                    ctx.class_name_of_id(ctx.class_id_of_object(list))
                        .unwrap_or_else(|| "<unknown>".into()),
                    data.is_some(),
                    data.map(|d| ctx.array_length(d) as i64).unwrap_or(-1),
                    match data.filter(|_| size > 0).map(|d| ctx.get_array_element(d, 0)) {
                        Some(Value::Object(Some(e))) => ctx
                            .class_name_of_id(ctx.class_id_of_object(e))
                            .unwrap_or_else(|| "<unnamed>".into()),
                        Some(other) => format!("<non-ref {other:?}>"),
                        None => "<no head>".to_string(),
                    },
                );
            }
        }
        return by_class;
    }
    al_view_holds_entries_by_head(ctx, list)
}

/// What a view carrier's own CLASS says it holds: `Some(true)` for the
/// entry-shaped carrier, `Some(false)` for a values-shaped one, and `None` for
/// a carrier whose class does not name its kind — the `java/util/ArrayList`
/// fallback [`alloc_view_carrier`] takes when the real class cannot be had.
///
/// [`MAP_VIEW_CARRIERS`] has exactly one entry-shaped member
/// ([`TM_ENTRY_SET_CARRIER`], minted only by `native_tm_entry_set`) and five
/// values-shaped ones, so the class is a COMPLETE and exact answer for every
/// carrier this crate mints under a real JDK — and, unlike the head element, it
/// is readable whatever state the element array is in.
///
/// That last property is the whole point. The head-element reading in
/// [`al_view_holds_entries_by_head`] answers "values" for anything it cannot
/// classify, and there are two ordinary ways to reach that verdict on a genuine
/// `entrySet()`:
///
/// * the view was EMPTY when the carrier was minted, so there is no head
///   element to read. MEASURED, no GC involved: `new TreeMap<>().entrySet()`
///   held across two `put`s then iterated answered `[v1, v2]` where HotSpot
///   25.0.3+9 answers `[k1=v1, k2=v2]`;
/// * the head element could not be classified during a moving collection, which
///   is how the `RTreeRangeGc` regression vector reddened on the default
///   collector — `headMap(k, false).entrySet()` handed back 300 `V` objects and
///   the vector's `checkcast` to `Map.Entry` failed.
///
/// Both faces are one defect — a kind INFERRED from data, allowed to default to
/// the wrong answer — and both close by asking the class first.
pub(crate) fn view_carrier_holds_entries_by_class(ctx: &dyn NativeContext, view: ObjectRef) -> Option<bool> {
    let name = ctx.class_name_arc_of_id(ctx.class_id_of_object(view))?;
    if &*name == TM_ENTRY_SET_CARRIER {
        return Some(true);
    }
    if is_map_view_carrier(&name) {
        return Some(false);
    }
    None
}

/// The head-element reading of [`al_view_holds_entries`], for a carrier whose
/// class does not name its kind.
///
/// Restricted to our synthetic entry classes so an app value type named
/// `*Entry` is not misread as an entry. An EMPTY view answers `false`: with no
/// head element there is nothing to classify. That default is exactly why this
/// is no longer consulted first.
pub(crate) fn al_view_holds_entries_by_head(ctx: &dyn NativeContext, list: ObjectRef) -> bool {
    let (data, size) = al_state(ctx, list);
    match data {
        Some(d) if size > 0 => match ctx.get_array_element(d, 0) {
            Value::Object(Some(e)) => ctx
                .class_name_of_id(ctx.class_id_of_object(e))
                .map(|n| is_synthetic_map_entry_class(&n))
                .unwrap_or(false),
            _ => false,
        },
        _ => false,
    }
}

/// `true` iff `list` is one of this crate's live `Map.values()` views — an
/// `ArrayList`-shaped carrier with a source-map marker in its trailing capacity
/// slot ([`values_view_source`]) whose elements are values, not entries.
///
/// **Why any native needs to ask.** On a real JVM `Map.values()` returns a
/// `HashMap$Values` / `LinkedHashMap$LinkedValues` / `TreeMap$Values` /
/// `Hashtable$ValueCollection`, and every one of those extends
/// `java.util.AbstractCollection`, which overrides **neither `equals` nor
/// `hashCode`**. Both therefore come from `java.lang.Object` and are IDENTITY
/// operations. Measured on HotSpot 25.0.3+9 (probe `ValuesSem`, two `HashMap`s
/// with identical contents):
///
/// ```text
///   hm.values.class                       java.util.HashMap$Values
///   equalMaps.values.equals               false      <- content-equal, still not equal
///   values.hashCode.isIdentity            true
///   values.equals.arrayListOfSameContent  false
///   arrayListOfSameContent.equals.values  false
///   empty.values.equals.emptyList         false      <- not even when both are empty
///   equalMaps.keySet.equals               true       <- contrast: AbstractSet DOES override
/// ```
///
/// This VM hands back a `java/util/ArrayList` instead (`native_map_values`,
/// `native_chm_values`, `make_live_values_list`), so without this predicate
/// `native_al_equals`/`native_al_hash_code` answer `AbstractList`'s
/// CONTENT-based contract on an object whose real class has no such contract.
/// The divergence is silent and it runs in the wrong direction: two unrelated
/// maps' value collections compare EQUAL, and a `values()` view compares equal
/// to an ordinary `ArrayList` of the same elements — so a `Set<Collection<..>>`
/// or a `Map` keyed on a view collapses distinct entries into one.
///
/// The `keySet()`/`entrySet()` views are deliberately NOT covered: their real
/// classes extend `AbstractSet`, which DOES override both, so the content-based
/// answer is the correct one there. That is why the entry discriminator is part
/// of this predicate and not an afterthought.
/// The second disjunct covers a receiver whose class is ALREADY one of the five
/// real view classes ([`is_values_view_class`]). For such an object the answer
/// is the same and the reason is stronger — its real superclass genuinely is
/// `AbstractCollection`, so identity is not an emulation but the actual
/// contract. Keeping both readings behind one predicate is what stops the
/// `ArrayList`-shaped carrier and the real class from drifting apart while the
/// two shapes coexist.
pub(crate) fn al_is_values_view(ctx: &dyn NativeContext, list: ObjectRef) -> bool {
    // [`TM_ENTRY_SET_CARRIER`] is in `MAP_VIEW_CARRIERS` — so
    // `is_values_view_class` admits it — and it is the one member that is NOT a
    // values view. Its real class extends `AbstractSet`, which DOES override
    // equals/hashCode, so the content-based answer is the correct one for it
    // and the identity answer below would be a divergence in the opposite
    // direction from the one this predicate exists to stop.
    if view_carrier_holds_entries_by_class(ctx, list) == Some(true) {
        return false;
    }
    if is_values_view_class(ctx, list) {
        return true;
    }
    values_view_source(ctx, list).is_some() && !al_view_holds_entries(ctx, list)
}

/// `true` iff `obj`'s runtime class is one of the five real JDK classes that
/// `Map.values()` returns. See [`CF_VALUES_VIEW`].
///
/// **Why these receivers need rerouting rather than their own bytecode.**
/// Measured on HotSpot 25.0.3+9 (`javap -p -c java.util.HashMap$Values`), the
/// real body of `size()` is three instructions:
///
/// ```text
///   0: aload_0
///   1: getfield  #7   // Field this$0:Ljava/util/HashMap;
///   4: getfield  #19  // Field java/util/HashMap.size:I
///   7: ireturn
/// ```
///
/// — a RAW read of the source map's `size` field. In this VM an integer-keyed
/// `HashMap`'s entries live in the `hm_int_fast_shards` overlay:
/// `try_hm_int_fast_put` inserts into the Rust shard and returns without ever
/// calling `set_map_size`, and it only engages when `map_state` already reports
/// size 0. `native_map_size` reads `hm_int_fast_len` BEFORE `map_state`, so the
/// two owners never reconcile and the heap `size` slot stays 0 for the object's
/// whole life. Real `HashMap$Values.size()` would therefore answer **0** on a
/// non-empty map. `Hashtable$ValueCollection.size()` is the same shape
/// (`getfield this$0.count`).
///
/// Not every method is like that, and the difference decides which ones need a
/// native at all — `TreeMap$Values.size()` is `invokevirtual TreeMap.size()`
/// and `HashMap$Values.clear()` is `invokevirtual HashMap.clear()`, both of
/// which reach registered natives and are correct as bytecode. The per-class
/// split is tabulated in `docs/known-issues/jdk-only/C13-1-*`.
#[inline]
pub(crate) fn is_values_view_class(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    receiver_facts(ctx, obj).has(CF_VALUES_VIEW)
}

/// `true` iff `obj`'s runtime class is one of the [`MAP_VIEW_CARRIERS`] — the
/// five [`is_values_view_class`] names AND [`TM_ENTRY_SET_CARRIER`].
///
/// This, not the values predicate, is what [`vc_route`] and
/// [`vc_route_source_size`] are asking: *is this receiver a real JDK map-view
/// class whose state this crate keeps somewhere the generic natives cannot
/// read?* The answer for `TreeMap$EntrySet` is yes, and `vc_route` already
/// knows what to do with it — see [`CF_MAP_ENTRY_VIEW`].
///
/// Two memoized bits rather than a name compare, because both routes sit at the
/// head of every `native_al_*` body and so run for every ordinary `ArrayList`.
#[inline]
pub(crate) fn is_map_view_carrier_receiver(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    let facts = receiver_facts(ctx, obj);
    facts.has(CF_VALUES_VIEW) || facts.has(CF_MAP_ENTRY_VIEW)
}

/// `true` iff `view` is a [`MAP_VIEW_CARRIERS`] object THIS crate minted —
/// i.e. one that already carries its elements in its own
/// [`view_carrier_slots`] `elementData` slot and its source map in the element
/// buffer's trailing capacity slot ([`values_view_source`]).
///
/// Such a receiver must NOT be re-expressed by [`vc_route`], for two reasons.
/// The decisive one is termination: `vc_route` calls back into the SAME native
/// entry point that reached it, over a carrier of the same class built by
/// `make_view_list_of`, and that carrier would satisfy `vc_route`'s own test
/// again — an unbounded recursion. It stayed latent only because nothing ever
/// populated `this$0`, so [`values_view_class_source`] answered `None` and the
/// route was inert; [`store_view_carrier_backref`] populates it, so the guard
/// has to be explicit rather than accidental. The second reason is that the
/// reroute would be pure waste: the direct decode is already live (every
/// element-reading native opens with `resync_values_view`) and already writes
/// removals through.
///
/// `vc_route` therefore keeps exactly the receivers it was written for: a view
/// object that arrived from somewhere other than this crate's own minting.
#[inline]
pub(crate) fn is_own_view_carrier(ctx: &dyn NativeContext, view: ObjectRef) -> bool {
    values_view_source(ctx, view).is_some()
}

/// Store the backing map in a [`MAP_VIEW_CARRIERS`] object's OWN declared
/// enclosing-instance field — `this$0` on the five inner classes, `map` on
/// `ConcurrentHashMap$ValuesView` (inherited from `CollectionView`) — the way
/// the JDK's own constructors do.
///
/// MEASURED before this existed (`scratchpad/g22/G22Slot.java`, run with
/// `--add-opens java.base/java.util=ALL-UNNAMED`, on both VMs): HotSpot reports
/// `HashMap$Values.this$0 -> java.util.HashMap`, `TreeMap$Values.this$0 ->
/// java.util.TreeMap` and `ConcurrentHashMap$ValuesView.map ->
/// java.util.concurrent.ConcurrentHashMap`; CratonVM reported `null` for all
/// three. Nothing had ever written those fields — the source map lived only in
/// the element buffer's trailing capacity slot ([`values_view_source`]), which
/// no reflective reader can see. G22-1 N2 / G13-1 N2.
///
/// Two guards, both structural rather than defensive:
///
/// * only a [`AlLayout::ViewCarrier`] receiver is written at all, so this can
///   never touch an ordinary `ArrayList`;
/// * only a slot strictly BELOW the receiver's `elementData` slot is written,
///   which is exactly the range [`view_carrier_slots`] reserves for the
///   carrier's own declared fields. This function therefore cannot re-create
///   the collision it was added alongside, even if a future carrier's layout
///   changes underneath it.
pub(crate) fn store_view_carrier_backref(ctx: &mut dyn NativeContext, list: ObjectRef, source: ObjectRef) {
    let ((data_slot, _, _), layout) = al_slots_and_layout_for(ctx, list);
    if layout != AlLayout::ViewCarrier {
        return;
    }
    let cid = ctx.class_id_of_object(list);
    for name in ["this$0", "map"] {
        if let Some(slot) = ctx.resolve_field_index_by_class_id(cid, name) {
            if slot < data_slot {
                ctx.set_field(list, slot, Value::Object(Some(source)));
                return;
            }
        }
    }
}

/// The source map a real view-class object was built over.
///
/// Resolved BY NAME, never by slot, and the reason is
/// `LinkedHashMap$LinkedValues` — it is the one member of the family with TWO
/// fields, and `javap -p` declares them in this order:
///
/// ```text
/// final class java.util.LinkedHashMap$LinkedValues extends
///         java.util.AbstractCollection<V> implements java.util.SequencedCollection<V> {
///   final boolean reversed;
///   final java.util.LinkedHashMap this$0;
/// ```
///
/// so its `this$0` is slot **1**, not slot 0. A hard-coded slot 0 would read
/// the `reversed` boolean as an object reference — a wrong-typed read of
/// exactly the kind `al_state`'s layout guard exists to prevent. The CHM member
/// needs the second name: `ConcurrentHashMap$ValuesView` declares no map field
/// itself and inherits `map` from `ConcurrentHashMap$CollectionView`, which
/// `resolve_field_index_by_class_id` finds by walking the chain (the same walk
/// that finds `Properties`' inherited `count` in [`map_size_slot`]).
pub(crate) fn values_view_class_source(ctx: &dyn NativeContext, view: ObjectRef) -> Option<ObjectRef> {
    let cid = ctx.class_id_of_object(view);
    let nf = ctx.object_num_fields(view);
    for name in ["this$0", "map"] {
        if let Some(slot) = ctx.resolve_field_index_by_class_id(cid, name) {
            if slot < nf {
                if let Value::Object(Some(m)) = ctx.get_field(view, slot) {
                    return Some(m);
                }
            }
        }
    }
    None
}

/// Reroute a real `Map.values()` view-class receiver that arrived at one of the
/// generic `ArrayList`-shaped natives, by re-expressing the call over a live
/// `ArrayList`-shaped view of the same source map.
///
/// The twin of [`ksv_route`], and it exists for the same reason: a receiver
/// that matches neither layout the generic natives read must not be decoded as
/// though it did. It differs in one deliberate way — `ksv_route` hands off to a
/// hand-written `native_ksv_*` implementation per method, whereas this one
/// rebuilds the marker-carrying carrier that `native_map_values` already
/// produces and then calls the SAME `imp`. That reuse is the point: liveness,
/// write-through removal and the entry/value discriminator are all already
/// implemented and measured on that path (`values_view_source` /
/// `resync_values_view` / `propagate_list_removal`), so a view-class receiver
/// gets the behaviour this VM already ships rather than a second
/// implementation of it that can drift.
///
/// Two consequences worth stating rather than discovering:
///
/// * the iterator handed back is an iterator over the rebuilt carrier, so its
///   `remove()` writes through to the source exactly as `values().iterator()`
///   does today;
/// * a `reversed` `LinkedValues` is rebuilt as a reversed carrier (C4-1,
///   [`values_view_reversed`]), so it iterates tail first; with
///   `CRATONVM_COMPAT_LHM_REVERSED_LIVE_VALUES=0` it iterates FORWARD, as
///   before.
///
/// Errors are returned, never flattened to "empty". `collect_entries_any` can
/// refuse (a `hashCode()` on an element can throw), and this crate has already
/// paid once for a helper that mapped a refusal onto `Vec::new()` — see
/// `collection_elements_generic`'s own doc.
pub(crate) fn vc_route(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    imp: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult,
) -> Option<MethodCallResult> {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    if !is_map_view_carrier_receiver(ctx, this) {
        return None;
    }
    if is_own_view_carrier(ctx, this) {
        return None;
    }
    // `MAP_VIEW_CARRIERS` is not all values — [`TM_ENTRY_SET_CARRIER`] is an
    // ENTRY set — and re-expressing one as `entries.map(|(_, v)| v)` is how
    // `RTreeRangeGc`'s `headMap(k, false).entrySet()` handed back 300 `V`
    // objects. Ask the CLASS what this receiver holds and mint that shape. The
    // marker test above asks "did WE mint it", which is a different question;
    // it was answering this one by omission, so a receiver it declined for any
    // reason silently changed kind.
    let holds_entries = view_carrier_holds_entries_by_class(ctx, this) == Some(true);
    if cratonvm_types::flags::runtime_var("CRATONVM_DBG_VIEWKIND").is_ok() {
        eprintln!(
            "[VIEWKIND] vc_route rebuilding carrier={} holds_entries={holds_entries} \
             (the own-carrier marker test declined it)",
            ctx.class_name_of_id(ctx.class_id_of_object(this))
                .unwrap_or_else(|| "<unknown>".into()),
        );
    }
    let source = values_view_class_source(ctx, this)?;
    // An image-minted `new LinkedValues(true)` is rebuilt as a reversed carrier
    // (C4-1), so it iterates tail first and its removals map to the right
    // nodes. Pure reads, asked before anything runs.
    let reversed = !holds_entries && values_view_reversed(&*ctx, this);
    // GC-safety, and the pins go up FIRST — before `collect_entries_any`, not
    // between it and `make_view_list_of`. Two distinct hazards:
    //
    //  * `collect_entries_any` re-enters Java (a TreeMap comparator, an
    //    element's `hashCode`, a `Properties` entrySet rebuild), so it can move
    //    `source` and every remaining argument before it returns. Pinning after
    //    it would pin an address that is already stale — the "Family 1"
    //    stale-ObjectRef shape this crate has paid for repeatedly (see
    //    `remove_source_entry_by_value` immediately below).
    //  * `make_view_list_of` then allocates the carrier AND its backing array.
    //
    // `vals` needs no pin of its own: `make_view_list_of` pins the slice it is
    // handed as its very first act, before it allocates, and nothing between
    // this `collect` and that call touches the Java heap.
    let source_pin = ctx.pin_native_root(source);
    let tail: Vec<Value> = args[1..].to_vec();
    let (_, tail_handles) = pin_value_slice(ctx, &tail);
    let entries = match collect_entries_any(ctx, source) {
        Ok(e) => e,
        Err(e) => {
            ctx.unpin_native_roots(source_pin);
            return Some(Err(e));
        }
    };
    let vals: Vec<Value> = if holds_entries {
        match live_entries_for_pairs(ctx, source_pin, source, &entries) {
            Ok(v) => v,
            Err(e) => {
                ctx.unpin_native_roots(source_pin);
                return Some(Err(e));
            }
        }
    } else {
        in_view_order(entries.into_iter().map(|(_, v)| v).collect(), reversed)
    };
    let source = ctx.read_native_pin(source_pin, source);
    // The carrier comes from the SOURCE map, not from `this`: this function
    // rebuilds the carrier `native_map_values` would have produced for that
    // map, and `this` is the view we are re-routing. Every other
    // `make_view_list_of` call site derives it the same way. (The parameter and
    // this call site arrived on opposite sides of the 2026-08-16 `dev` merge,
    // which is why git produced a clean 3-argument call to a 4-argument fn.)
    //
    // The entry-shaped receiver keeps its OWN class: `values_carrier_for` only
    // knows the five values carriers, so deriving it from the source here would
    // hand an `entrySet()` back under `TreeMap$Values` — the class half of the
    // same mix-up the `vals` branch above closes.
    let carrier = if holds_entries {
        TM_ENTRY_SET_CARRIER
    } else {
        values_carrier_for(&*ctx, source)
    };
    let list = match make_view_list_of(ctx, source, &vals, carrier) {
        Ok(l) => l,
        Err(e) => {
            ctx.unpin_native_roots(source_pin);
            return Some(Err(e));
        }
    };
    if reversed {
        // `false` only for a carrier that fell back off `LinkedValues`; its
        // snapshot is already in reversed order either way.
        let _ = mark_values_view_reversed(ctx, list);
    }
    let mut call_args: Vec<Value> = Vec::with_capacity(args.len());
    call_args.push(Value::Object(Some(list)));
    for (i, v) in tail.iter().enumerate() {
        call_args.push(read_pinned_elem(ctx, tail_handles[i], *v));
    }
    let r = imp(ctx, &call_args);
    ctx.unpin_native_roots(source_pin);
    Some(r)
}

/// `vc_route`'s cheap arm for the two size questions.
///
/// `size()` and `isEmpty()` do not need the elements, so rebuilding the carrier
/// for them would be an allocation (and a GC point) purely to count. Ask the
/// source map instead — `native_map_size` already dispatches the TreeMap / CHM
/// / integer-overlay families correctly, which is exactly the set of answers a
/// view over that map must agree with.
pub(crate) fn vc_route_source_size(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<MethodCallResult> {
    if !is_map_view_carrier_receiver(ctx, this) {
        return None;
    }
    if is_own_view_carrier(ctx, this) {
        return None;
    }
    let source = values_view_class_source(ctx, this)?;
    Some(native_map_size(ctx, &[Value::Object(Some(source))]))
}

/// Delete the first entry of `source` whose value equals `value` (identity
/// first, then `.equals`). Used to propagate a `values()`-view removal back to
/// the backing map. `reversed`: the view hands the source's values out tail
/// first (a reversed `LinkedValues`, [`view_by_value_reversed`]), so the walk
/// is tail first too and the LAST equal value in list order goes (CP5-3).
pub(crate) fn remove_source_entry_by_value(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
    value: Value,
    reversed: bool,
) -> Result<(), MethodCallFailed> {
    let entries = collect_entries_any(ctx, source)?;
    // Family-1 fix (cce0079): the per-entry `equals()` dispatch can move
    // `source`/`value`/every snapshot key+value — pin them all, refresh per
    // use, and hand `source_map_remove` current addresses.
    let src_pin = ctx.pin_native_root(source);
    let vh = pin_value(ctx, value);
    let (keys, vals): (Vec<Value>, Vec<Value>) = entries.into_iter().unzip();
    let (_, k_handles) = pin_value_slice(ctx, &keys);
    let (_, v_handles) = pin_value_slice(ctx, &vals);
    let mut source = source;
    let mut value = value;
    let mut result = Ok(());
    let order: Vec<usize> = if reversed {
        (0..keys.len()).rev().collect()
    } else {
        (0..keys.len()).collect()
    };
    for i in order {
        let v = read_pinned_elem(ctx, v_handles[i], vals[i]);
        // A throwing `equals` propagates, with the pins released below (the
        // bare `?` this replaced returned past the unpin).
        let matched = match list_element_matches(ctx, &v, &value) {
            Ok(m) => m,
            Err(e) => {
                result = Err(e);
                break;
            }
        };
        source = ctx.read_native_pin(src_pin, source);
        value = read_pinned_elem(ctx, vh, value);
        if matched {
            let k = read_pinned_elem(ctx, k_handles[i], keys[i]);
            result = source_map_remove(ctx, source, k).map(|_| ());
            break;
        }
    }
    ctx.unpin_native_roots(src_pin);
    result
}

/// The node a `values()` view iterator's `remove()` stands on: the
/// `index`-th node of `source` in iteration order, when that node's value is
/// the very `removed` element (`None` otherwise, or under
/// `CRATONVM_COMPAT_VIEW_REMOVE_BY_NODE=0`, or for a source
/// [`plain_node_map`] refuses). The view's snapshot is built in the source's
/// iteration order and the iterator's comodification check has just proved
/// the source unchanged since, so position `index` is the node the JDK's
/// `ValueIterator` would remove. [`remove_source_entry_by_value`] removed the
/// FIRST entry with an equal value instead: with `{a=1, b=1}`, removing the
/// second value dropped `a`. `reversed`: the view is a reversed `LinkedValues`
/// ([`values_view_reversed`]), whose position `index` is node `n - 1 - index`
/// (C4-1). Pure reads.
pub(crate) fn values_view_node_at(
    ctx: &dyn NativeContext,
    source: ObjectRef,
    index: usize,
    removed: Value,
    reversed: bool,
) -> Option<(ObjectRef, bool)> {
    if !view_remove_by_node_enabled() || !plain_node_map(ctx, source) {
        return None;
    }
    let (nodes, lhm) = hs_backing_nodes(ctx, source)?;
    let index = if reversed {
        nodes.len().checked_sub(index.checked_add(1)?)?
    } else {
        index
    };
    let node = *nodes.get(index)?;
    let value = if lhm {
        ctx.get_field(node, LHM_NODE_VALUE)
    } else {
        get_node_value(ctx, node)
    };
    value_same_ref(value, removed).then_some((node, lhm))
}

/// `CRATONVM_COMPAT_VALUES_REMOVE_JDK` — default ON, `=0` restores the
/// snapshot search. `HashMap.Values` / `LinkedHashMap.LinkedValues` inherit
/// `AbstractCollection.remove(o)`: walk the LIVE map's values in iteration
/// order, ask `o.equals(v)` (the argument is the receiver; `v == null` for a
/// null `o`), and remove the first match with the iterator's `remove()` --
/// `removeNode(p.hash, p.key, null, false, false)`, no `hashCode()`. The
/// native searched the view's element snapshot, which `remove(Object)` did not
/// resync first (so a value `put` after the view was taken was not found),
/// and then removed from the source BY KEY after a second search
/// (`remove_source_entry_by_value`: another `equals` per entry ahead of the
/// match, and a `hashCode()`). Round 13 wave 11, lane compat6
/// (`r13w10-compat5-map-view-residuals-CLOSED-20260929.md` item 1).
pub(crate) fn values_remove_jdk_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_VALUES_REMOVE_JDK")
}

/// `AbstractCollection.remove(o)` over a `values()` view `list` of `source`
/// (a map [`plain_node_map`] accepts), as described at
/// [`values_remove_jdk_enabled`]: `Some(true/false)` as the JDK answers, with
/// the view's snapshot rebuilt after a removal. The walk is `ValueIterator`'s,
/// so an `equals` that structurally modifies the map makes the next step
/// throw `ConcurrentModificationException` (`CRATONVM_COMPAT_MAP_ITR_FAILFAST`).
/// `None`, before any Java has run, when the source's nodes cannot be walked.
pub(crate) fn values_view_remove_jdk(
    ctx: &mut dyn NativeContext,
    list: ObjectRef,
    source: ObjectRef,
    target: Value,
) -> Option<MethodCallResult> {
    let (mut nodes, lhm) = hs_backing_nodes(&*ctx, source)?;
    // A reversed `LinkedValues` (C4-1): its iterator walks tail first, so the
    // LAST equal value in list order is the one removed.
    if values_view_reversed(&*ctx, list) {
        nodes.reverse();
    }
    let node_vals: Vec<Value> = nodes.iter().map(|n| Value::Object(Some(*n))).collect();
    let fail_fast = map_itr_failfast_enabled();
    let expected = read_map_mod_count(&*ctx, source);
    let list_pin = ctx.pin_native_root(list);
    let source_pin = ctx.pin_native_root(source);
    let target_pin = pin_value(ctx, target);
    let (_, node_pins) = pin_value_slice(ctx, &node_vals);
    // Every pin above `list_pin` is released with it, on every exit.
    let result = (|| -> MethodCallResult {
        for (i, nv) in node_vals.iter().enumerate() {
            // `nextNode()`'s comodification check.
            let source_now = ctx.read_native_pin(source_pin, source);
            if fail_fast && hs_map_mod_moved(&*ctx, source_now, expected) {
                return Err(RuntimeError::ConcurrentModificationException.into());
            }
            let Value::Object(Some(node)) = read_pinned_elem(ctx, node_pins[i], *nv) else {
                continue;
            };
            let value = if lhm {
                ctx.get_field(node, LHM_NODE_VALUE)
            } else {
                get_node_value(&*ctx, node)
            };
            let target_now = read_pinned_elem(ctx, target_pin, target);
            // `o.equals(it.next())`, or `it.next() == null` for a null `o`.
            if !list_element_matches(ctx, &value, &target_now)? {
                continue;
            }
            // `it.remove()`: its own comodification check, then the node.
            let source_now = ctx.read_native_pin(source_pin, source);
            if fail_fast && hs_map_mod_moved(&*ctx, source_now, expected) {
                return Err(RuntimeError::ConcurrentModificationException.into());
            }
            let node = ctx.read_native_pin(node_pins[i], node);
            map_remove_node_by_walk(ctx, source_now, node, lhm)?;
            let list_now = ctx.read_native_pin(list_pin, list);
            resync_values_view(ctx, list_now)?;
            return Ok(Some(Value::Int(1)));
        }
        Ok(Some(Value::Int(0)))
    })();
    ctx.unpin_native_roots(list_pin);
    Some(result)
}

/// Propagate the removal of `removed` (an element of an ArrayList-backed map
/// view) to the source map. For an `entrySet()` view the element is a
/// `Map.Entry`, so delete its key (slot 0); for a `values()` view the element
/// is a value, so delete the first entry whose value matches.
pub(crate) fn propagate_list_removal(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
    removed: Value,
) -> Result<(), MethodCallFailed> {
    propagate_list_removal_in(ctx, source, removed, false)
}

/// `CRATONVM_COMPAT_VALUES_BY_VALUE_REVERSED` -- default ON, `=0` restores the
/// forward walk. The by-VALUE fallback of a `values()` view removal
/// ([`remove_source_entry_by_value`], reached for a `LinkedHashMap` source only
/// with `CRATONVM_COMPAT_VALUES_REMOVE_JDK=0` or
/// `CRATONVM_COMPAT_VIEW_REMOVE_BY_NODE=0`) removed the FIRST equal value in
/// list order even for a reversed `LinkedValues` (C4-1), whose `remove(o)` /
/// iterator walk tail first on the JDK. Round 15 wave 2 (lane compat),
/// proposal CP5-3 of `jit-r14-compat5-proposals-RETIRED-20260930.md`.
pub(crate) fn values_by_value_reversed_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_VALUES_BY_VALUE_REVERSED")
}

/// Whether a by-value removal through the values view `list` walks its source
/// tail first: `list` is a reversed `LinkedValues` ([`values_view_reversed`])
/// and CP5-3's switch is on. Pure reads; ask it before anything GC-capable.
pub(crate) fn view_by_value_reversed(ctx: &dyn NativeContext, list: ObjectRef) -> bool {
    values_by_value_reversed_enabled() && values_view_reversed(ctx, list)
}

/// [`propagate_list_removal`] for a view whose by-value walk may run tail
/// first (`reversed`, from [`view_by_value_reversed`]).
pub(crate) fn propagate_list_removal_in(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
    removed: Value,
    reversed: bool,
) -> Result<(), MethodCallFailed> {
    // A COLLECTION SOURCE removes by VALUE through its own native. Each of the
    // three owns an invariant a generic removal would break: the queue's
    // sift-down, the deque's ring-buffer compaction, the set's tree ordering.
    if is_tree_set_shaped(&*ctx, source) {
        let _ = ts_remove_element(ctx, source, removed)?;
        return Ok(());
    }
    if let Some(name) = ctx.class_name_arc_of_id(ctx.class_id_of_object(source)) {
        match &*name {
            "java/util/PriorityQueue" => {
                let _ = native_pq_remove(ctx, &[Value::Object(Some(source)), removed])?;
                return Ok(());
            }
            "java/util/ArrayDeque" => {
                let _ = native_ad_remove_first_occurrence(
                    ctx,
                    &[Value::Object(Some(source)), removed],
                )?;
                return Ok(());
            }
            _ => {}
        }
    }
    if let Value::Object(Some(e)) = removed {
        let cls = ctx
            .class_name_of_id(ctx.class_id_of_object(e))
            .unwrap_or_default();
        // Only treat the element as a Map.Entry (delete by its key) when it is
        // one of our synthetic entry classes — not merely any class whose name
        // ends in "Entry" (an app value type like PathEntry must delete by
        // value, not by `field(0)`).
        if is_synthetic_map_entry_class(&cls) {
            // See the note in `native_hs_remove`: entry slots are per class.
            let (k_slot, _, _) = entry_slots(&*ctx, e);
            let key = ctx.get_field(e, k_slot);
            return source_map_remove(ctx, source, key);
        }
    }
    remove_source_entry_by_value(ctx, source, removed, reversed)
}

/// If `ts` is a TreeMap `keySet()` view (an array-backed `TreeSet` whose
/// element array stashes the source map in its last capacity slot), return the
/// source map. Mirrors `values_view_source`: the marker only exists when the
/// array is longer than the logical size and the trailing slot is non-null.
/// `CRATONVM_DBG_TMVIEW` — the trace that separates the four places a
/// TreeMap view decode can answer EMPTY. See `ts_view_source`.
pub(crate) fn tmview_dbg() -> bool {
    cratonvm_types::flags::runtime_var("CRATONVM_DBG_TMVIEW").is_ok()
}

pub(crate) fn ts_view_source(ctx: &dyn NativeContext, ts: ObjectRef) -> Option<ObjectRef> {
    let (data, size, _) = ts_state(ctx, ts);
    if let Some(data) = data {
        let dlen = ctx.array_length(data) as i32;
        if dlen > size {
            if let Value::Object(Some(src)) = ctx.get_array_element(data, (dlen - 1) as usize) {
                return Some(src);
            }
        }
    }
    // THE CARRIER THE IMAGE BUILT ITSELF has no marker, because this crate did
    // not mint it — and it is the shape `treeMap.keySet()` takes the moment the
    // TreeMap natives stop answering (the dial armed on `java/util/TreeMap`, or
    // the family retired). It still has the enclosing map, in the field the
    // JDK's own constructor wrote: `this$0`.
    //
    // The set-shaped twin of [`hs_backing_map`]'s by-name arm, and for the same
    // measured reason. `java/util/TreeMap$KeySet` is in NEITHER carrier list —
    // it shares the `native_ts_*` surface over `ts_array_table` — so nothing
    // else in this file can see the source, and `resync_ts_view` returned the
    // receiver untouched. MEASURED on the wave-8 control, `--jdk-only`, dial
    // armed on `java/util/TreeMap`: `keySet().toArray()` answered `[0]` for a
    // three-key map while `keySet().size()`, `.iterator()` and `.toString()`
    // over the very same view were right — those reach real bytecode through
    // the dispatch door, and `toArray`'s own `size()` question is asked BY a
    // native, which does not.
    //
    // Exact class, not ancestry: a `java/util/TreeSet` reaching here with no
    // marker is a plain set, not a view, and must keep answering `None`. It is
    // also what makes reading `m` below safe -- `java/util/TreeSet` declares a
    // field of that name too, and it is the set's OWN backing map rather than a
    // view source.
    let cid = ctx.class_id_of_object(ts);
    if ctx.class_name_arc_of_id(cid).as_deref() != Some(TM_KEY_SET_CARRIER) {
        return None;
    }
    // NOT [`values_view_class_source`], and the difference cost a build.
    // `TreeMap.KeySet` is a STATIC nested class -- `javap -p` on
    // jdk-17.0.20.1+1, jdk-21.0.12+8 and jdk-25.0.4+7 all give it exactly one
    // field:
    //
    // ```text
    //   final class java.util.TreeMap$KeySet<E> extends java.util.AbstractSet<E>
    //           implements java.util.NavigableSet<E> {
    //     private final java.util.NavigableMap<E, ?> m;
    // ```
    //
    // -- so it has no `this$0` and no `map`, the two names that helper knows,
    // and the first cut of this arm measured EXACTLY as well as no arm at all
    // (28 of the 38 rows still differing, all of them the keySet half). The
    // sibling carriers really are inner classes and really do have `this$0`,
    // which is what made the wrong guess look reasonable.
    let slot = ctx.resolve_field_index_by_class_id(cid, "m");
    if tmview_dbg() {
        eprintln!(
            "[TMVIEW] ts_view_source KeySet m_slot={slot:?} nf={}",
            ctx.object_num_fields(ts)
        );
    }
    let slot = slot?;
    if slot >= ctx.object_num_fields(ts) {
        return None;
    }
    let got = ctx.get_field(ts, slot);
    if tmview_dbg() {
        eprintln!("[TMVIEW] ts_view_source m = {got:?}");
    }
    match got {
        Value::Object(Some(m)) => Some(m),
        _ => None,
    }
}

/// Refresh a `TreeMap.keySet()` view from its live source map, so a read sees
/// keys put into the map AFTER the view was taken. JDK keySet views are live;
/// this is the `TreeSet`-carried family's counterpart to [`resync_view_set`]
/// (HashSet-carried keySet/entrySet) and [`resync_values_view`]
/// (ArrayList-carried values/TreeMap-entrySet).
///
/// Measured before this existed, `probes/MapViewBehaviourProbe`:
///
/// ```text
/// tm.keys.afterPut.size   HotSpot 3   CratonVM 2
/// ```
///
/// — the view had write-through (`remove`/`clear`/`pollFirst` all consult the
/// marker) but no read-through, so it was a snapshot in the read direction
/// only. `values()` and `entrySet()` of the same TreeMap already resynced,
/// which is why exactly this one row diverged.
///
/// Only a **map** source is rebuilt. `TreeSet.descendingSet()` puts another
/// `TreeSet` behind the same marker and its elements are the source's in
/// REVERSE; rebuilding it in sorted order here would silently un-reverse the
/// view. Its liveness is a separate, unmeasured row — see
/// [`ts_source_remove`], which makes the same map-vs-set distinction.
pub(crate) fn resync_ts_view(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let source = match ts_view_source(ctx, this) {
        Some(s) => s,
        None => {
            if tmview_dbg() {
                eprintln!("[TMVIEW] resync_ts_view: no source marker");
            }
            return Ok(this);
        }
    };
    if !is_tree_map_receiver(ctx, source) {
        if tmview_dbg() {
            eprintln!(
                "[TMVIEW] resync_ts_view: source {} is not a TreeMap receiver",
                ctx.class_name_of_id(ctx.class_id_of_object(source))
                    .unwrap_or_else(|| "<unknown>".into())
            );
        }
        // A `subSet` / `headSet` / `tailSet` view of another `TreeSet` (it has
        // a range in `tm_view_table`; a `descendingSet` has none and keeps the
        // early return, see the doc above).
        if ts_range_view_live_enabled() {
            return resync_ts_set_range_view(ctx, this, source);
        }
        return Ok(this);
    }
    // GC-SAFETY: `tm_collect_pairs` boxes fast-mode keys (allocating), and
    // `alloc_ref_array` below collects too. `this`, `source` and every
    // collected key are bare Rust locals used after those calls, so all three
    // are pinned and re-read — the same shape `resync_values_view` uses.
    let this_pin = ctx.pin_native_root(this);
    let source_pin = ctx.pin_native_root(source);
    let source = ctx.read_native_pin(source_pin, source);
    let keys: Vec<Value> = tm_collect_pairs(ctx, source)
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    if tmview_dbg() {
        eprintln!("[TMVIEW] resync_ts_view rebuilt {} key(s)", keys.len());
    }
    let (_, key_handles) = pin_value_slice(ctx, &keys);
    let cap = std::cmp::max(keys.len(), TS_DEFAULT_CAPACITY) + 1;
    let buf = alloc_ref_array(ctx, cap);
    let buf_pin = ctx.pin_native_root(buf);
    for (i, k) in keys.iter().enumerate() {
        let k = read_pinned_elem(ctx, key_handles[i], *k);
        let buf = ctx.read_native_pin(buf_pin, buf);
        ctx.set_array_element(buf, i, k);
    }
    // Re-install the trailing source marker: the array is longer than the
    // logical size and its last slot is non-null, which is exactly what
    // `ts_view_source` tests for. Without it the rebuilt view stops writing
    // through.
    let source = ctx.read_native_pin(source_pin, source);
    let buf = ctx.read_native_pin(buf_pin, buf);
    ctx.set_array_element(buf, cap - 1, Value::Object(Some(source)));
    let this = ctx.read_native_pin(this_pin, this);
    ts_set_slot(ctx, this, TS_FIELD_DATA, Value::Object(Some(buf)));
    ts_set_slot(ctx, this, TS_FIELD_SIZE, Value::Int(keys.len() as i32));
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    Ok(this)
}

/// `CRATONVM_COMPAT_TS_RANGE_VIEW_LIVE` -- default ON, `=0` keeps the
/// creation-time snapshot. Round 15 wave 7 (lane sweepC hand-back): a
/// `TreeSet` range view of another `TreeSet` is rebuilt from the source's LIVE
/// elements inside its bounds on every read ([`resync_ts_set_range_view`]).
pub(crate) fn ts_range_view_live_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_TS_RANGE_VIEW_LIVE")
}

/// Rebuild the `TreeSet` range view `this` (registered by
/// `ts_install_range_view`, bounds in [`tm_view_table`]) from its `TreeSet`
/// `source`: the source's current elements, in the source's order, that lie
/// inside the bounds by the source's comparator -- JDK 25's
/// `NavigableSubMap` reads the backing map live, so `ts.subSet(1, 3)` after
/// `ts.remove(1); ts.remove(2)` has `size() == 0` (`R15SweepCNullArgViews`
/// row 4: `addAll(null)` then asks `c.size()`, not `c.iterator()`). It was a
/// snapshot taken at `subSet` time. The source is brought up to date first
/// (a view of a view, or a map's `keySet`). Not a range view (no spec, or a
/// descending one): `this` unchanged. Re-entry from a comparator reading the
/// same view is the same no-op guard [`tm_resync_view`] uses.
pub(crate) fn resync_ts_set_range_view(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    source: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let spec = match tm_view_spec(ctx, this) {
        Some(s) if !s.descending => s,
        _ => return Ok(this),
    };
    let key = tm_obj_key(ctx, this);
    if !TM_VIEW_RESYNC.with(|s| s.borrow_mut().insert(key)) {
        return Ok(this);
    }
    let result = resync_ts_set_range_view_inner(ctx, this, source, &spec);
    TM_VIEW_RESYNC.with(|s| {
        s.borrow_mut().remove(&key);
    });
    result
}

pub(crate) fn resync_ts_set_range_view_inner(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    source: ObjectRef,
    spec: &TmViewSpec,
) -> Result<ObjectRef, MethodCallFailed> {
    // GC-SAFETY: the parent resync, every `tree_compare` (Java) and
    // `alloc_ref_array` can move `this`, `source`, the bounds and every
    // element; all are pinned before the first GC point, `this_pin` first so
    // one unpin unwinds the group.
    let this_pin = ctx.pin_native_root(this);
    let src_pin = ctx.pin_native_root(source);
    let mut lo = spec.lo;
    let lo_pin = pin_value(ctx, lo);
    let mut hi = spec.hi;
    let hi_pin = pin_value(ctx, hi);
    let walked = (|| -> Result<(), MethodCallFailed> {
        let source = ctx.read_native_pin(src_pin, source);
        resync_ts_view(ctx, source)?;
        let source = ctx.read_native_pin(src_pin, source);
        let (data_opt, size, cmp) = ts_state(&*ctx, source);
        let elems: Vec<Value> = match data_opt {
            Some(data) => {
                let n = (size.max(0) as usize).min(ctx.array_length(data));
                (0..n).map(|i| ctx.get_array_element(data, i)).collect()
            }
            None => Vec::new(),
        };
        let (_, e_handles) = pin_value_slice(ctx, &elems);
        let mut cmp = cmp;
        let cmp_pin = pin_value(ctx, cmp);
        let mut kept: Vec<usize> = Vec::with_capacity(elems.len());
        for (i, e0) in elems.iter().enumerate() {
            let mut keep = true;
            if spec.lo_bounded {
                let e = read_pinned_elem(&*ctx, e_handles[i], *e0);
                lo = read_pinned_elem(&*ctx, lo_pin, lo);
                let c = tree_compare(ctx, &cmp, e, lo)?;
                cmp = read_pinned_elem(&*ctx, cmp_pin, cmp);
                if c < 0 || (c == 0 && !spec.lo_inclusive) {
                    keep = false;
                }
            }
            if keep && spec.hi_bounded {
                let e = read_pinned_elem(&*ctx, e_handles[i], *e0);
                hi = read_pinned_elem(&*ctx, hi_pin, hi);
                let c = tree_compare(ctx, &cmp, e, hi)?;
                cmp = read_pinned_elem(&*ctx, cmp_pin, cmp);
                if c > 0 || (c == 0 && !spec.hi_inclusive) {
                    keep = false;
                }
            }
            if keep {
                kept.push(i);
            }
        }
        let _ = (lo, hi, cmp);
        // One spare slot past the logical size holds the source marker, as
        // `ts_install_range_view` built it.
        let cap = std::cmp::max(kept.len(), TS_DEFAULT_CAPACITY) + 1;
        let buf = alloc_ref_array(ctx, cap);
        for (j, &i) in kept.iter().enumerate() {
            let e = read_pinned_elem(&*ctx, e_handles[i], elems[i]);
            ctx.set_array_element(buf, j, e);
        }
        let source = ctx.read_native_pin(src_pin, source);
        ctx.set_array_element(buf, cap - 1, Value::Object(Some(source)));
        let this_now = ctx.read_native_pin(this_pin, this);
        ts_set_slot(ctx, this_now, TS_FIELD_DATA, Value::Object(Some(buf)));
        let this_now = ctx.read_native_pin(this_pin, this);
        ts_set_slot(ctx, this_now, TS_FIELD_SIZE, Value::Int(kept.len() as i32));
        Ok(())
    })();
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    walked.map(|()| this)
}

/// Write a removal back to whatever a [`ts_view_source`] marker names.
///
/// Two kinds of source reach that marker now. A `TreeMap` keySet view names a
/// MAP, whose removal is `remove(Object)Object`; a `descendingSet` view names
/// another `TreeSet`, whose removal is `remove(Object)Z`. Dispatching by
/// descriptor is not optional — `source_map_remove`'s
/// `(Ljava/lang/Object;)Ljava/lang/Object;` does not exist on `TreeSet` at all,
/// so calling it on a set source raises `NoSuchMethodError` rather than
/// removing anything.
pub(crate) fn ts_source_remove(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
    elem: Value,
) -> Result<(), MethodCallFailed> {
    if is_tree_map_receiver(ctx, source) {
        return source_map_remove(ctx, source, elem);
    }
    ctx.invoke_virtual(source, "remove", "(Ljava/lang/Object;)Z", &[elem])?;
    Ok(())
}

/// `java/util/concurrent/CopyOnWriteArraySet`, which is served by its OWN
/// bytecode whenever the image declares it.
pub(crate) const COW_SET_CLASS: &str = "java/util/concurrent/CopyOnWriteArraySet";

/// Is `this` a `CopyOnWriteArraySet` that the IMAGE declares, rather than one
/// this VM fabricated?
///
/// Asked by NAME, not by mode flag: the real class declares exactly one
/// instance field, `al` (`Ljava/util/concurrent/CopyOnWriteArrayList;`), and a
/// fabricated stub declares `_f0`/`_f1`. `resolve_field_index_by_class_id`
/// therefore separates the two on the fact that actually matters — whether
/// there is real bytecode under this receiver — and does it per RECEIVER, so a
/// subclass is classified by what it really inherits. It is the same question
/// [`hs_map_slot`] and `props_defaults_slot` ask about their own receivers.
pub(crate) fn is_real_cow_array_set(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let cid = ctx.class_id_of_object(this);
    // Memoised per `(vm, class)` the way `hs_map_slot` is, and for the same
    // reason: this sits in front of every method of the class, and the body
    // below takes the class-manager lock twice.
    thread_local! {
        pub(crate) static MEMO: ClassMemo<usize> = const { std::cell::RefCell::new(Vec::new()) };
    }
    let vm = ctx.vm_identity();
    if let Some(v) = MEMO.with(|t| class_memo_get(t, vm, cid.as_u32())) {
        return v == 1;
    }
    let Some(base) = ctx.class_id_by_name(COW_SET_CLASS) else {
        // Not loaded at all, so nothing can be an instance of it. Not cached:
        // `class_memo_get` keeps positives only, and the answer changes the
        // moment the class arrives.
        return false;
    };
    if cid != base && !ctx.is_subclass(cid, base) {
        return false;
    }
    let real = ctx.resolve_field_index_by_class_id(cid, "al").is_some();
    MEMO.with(|t| class_memo_put(t, vm, cid.as_u32(), usize::from(real)));
    real
}

thread_local! {
    /// Re-entrancy guard for [`cow_set_route`], holding a [`ReentryEntry`]
    /// per delegation in flight on this thread.
    ///
    /// `CopyOnWriteArraySet`'s own bodies delegate to `al`, a DIFFERENT
    /// receiver, so a self-loop should not be reachable. "Should not" is the
    /// reason this exists: the same reasoning held for
    /// [`try_delegate_real_collection`] until a native registered under a real
    /// JDK class name re-found itself, and the failure there is a stack
    /// overflow rather than a wrong answer.
    static COW_SET_GUARD: std::cell::RefCell<Vec<ReentryEntry>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Run the receiver's OWN `CopyOnWriteArraySet` bytecode, when the receiver is
/// a real one.
///
/// # Why this class leaves the HashSet surface
///
/// `register_hashset_natives` mirrors the whole `java.util.HashSet` surface
/// onto `CopyOnWriteArraySet`, and `native_hs_init` gave it a `LinkedHashMap`
/// backing at absolute slot 0. On the real class slot 0 is
/// `al` (`Ljava/util/concurrent/CopyOnWriteArrayList;`) — its ONE instance
/// field — so that is a map stored in a slot declared to hold a list, and it
/// is not a theoretical mismatch:
///
/// ```text
///   cowSet.removeIf(p)
///     -> NoSuchMethodError: java.util.LinkedHashMap.removeIf(java.util.function.Predicate)
/// ```
///
/// measured on `probes/CowSetBacking.java` before this change. `removeIf` is
/// the one method of the nineteen the class declares that the mirror does NOT
/// register, so its real body ran, read `al`, and found a map. Every other
/// method looked correct only because a native stood in front of it.
///
/// It also cost the class its shape: a `LinkedHashMap` is 88 bytes empty on
/// this VM where a `CopyOnWriteArrayList` is 48, so an empty
/// `CopyOnWriteArraySet` retained 112 against HotSpot's 56.1 — the one
/// collection left at 2.0x after the synthetic slot floor came off, and the
/// only one whose excess was a different OBJECT rather than reference width.
///
/// # Why delegation rather than a second implementation
///
/// `CopyOnWriteArrayList` is already real-layout on this VM: over a real JDK
/// its own bodies run (no stand-in is registered there since interpreter
/// round i3 wave 76), its `<init>` override was deliberately removed so the
/// JDK constructor runs, and
/// it measures 48.0 against HotSpot's 40.0 — the best ratio in the collection
/// table. So the JDK's own `CopyOnWriteArraySet` bodies, which are thin
/// forwards to `al`, already have a working object underneath them. Writing a
/// second implementation of nineteen forwards would be a second thing to keep
/// in step with it.
///
/// Returns `None` — the caller keeps its own body — for a FABRICATED
/// `CopyOnWriteArraySet`, where there is no bytecode to delegate to and the
/// map-backed surface IS the implementation.
/// The elements of a real `CopyOnWriteArraySet`, in iteration order, or `None`
/// when `this` is not one.
///
/// For the handful of methods [`cow_set_route`] cannot take to bytecode. The
/// route works by resolving the method on `CopyOnWriteArraySet`'s hierarchy, which is
/// right for the nineteen `CopyOnWriteArraySet` declares and wrong for a
/// `Collection` DEFAULT method: `stream()` resolves to `Collection.stream`,
/// whose body builds a real `java.util.stream` pipeline over a real
/// `Spliterator`, and this VM's streams are a synthetic carrier. So that one
/// takes the elements and builds the carrier, which is what every other
/// `*_stream` native in this file does. (Round 17 wave 3: only under
/// `CRATONVM_COW_SET_STREAM_JDK_BODY=0` now; by default `stream()` IS routed,
/// see [`cow_set_stream_jdk_body_enabled`].)
///
/// Reads them through `toArray()`, which IS delegated — so the elements come
/// from `al` by the same path everything else uses, rather than from a second
/// reading of the receiver's slots.
pub(crate) fn cow_set_elements(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<Vec<Value>> {
    if !is_real_cow_array_set(ctx, this) {
        return None;
    }
    // `invoke_virtual` runs Java and can complete a moving young GC; `this` is
    // a bare Rust local. The elements are collected AFTER it returns and
    // handed straight to a caller that pins them.
    let this_pin = ctx.pin_native_root(this);
    let this = ctx.read_native_pin(this_pin, this);
    let elements = match ctx.invoke_virtual(this, "toArray", "()[Ljava/lang/Object;", &[]) {
        Ok(Some(Value::Object(Some(arr)))) => {
            let n = ctx.array_length(arr);
            (0..n).map(|i| ctx.get_array_element(arr, i)).collect()
        }
        _ => Vec::new(),
    };
    ctx.unpin_native_roots(this_pin);
    Some(elements)
}

pub(crate) fn cow_set_route(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &'static str,
    descriptor: &str,
) -> Option<MethodCallResult> {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    if !is_real_cow_array_set(ctx, this) {
        return None;
    }
    let guard_pin = reentry_guard_enter(ctx, &COW_SET_GUARD, this, method)?;
    // Bytecode-only (`invoke_special`'s native-first lookup would re-find the
    // very native that called us), and bound STATICALLY on
    // `CopyOnWriteArraySet`, the class these natives are registered on -- not
    // on the receiver's class. Interpreter round i3 wave 79, lane L1
    // (`i75-L1-jdk-only-copyonwritearrayset-subclass-super-add-loses-the-element`):
    // the native is reached for a subclass receiver through that subclass's
    // `super.m()` (an `invokespecial CopyOnWriteArraySet.m`); binding on the
    // receiver's class re-found the subclass's override (or javac's bridge,
    // which `invokevirtual`s the override), so the override ran twice, the
    // second `super.m()` hit the held re-entry guard, and the native's
    // map-shaped fallback dropped the element (`SubSet [] adds=4` against
    // HotSpot's `[p] adds=2`). A virtual call on a subclass that overrides
    // `m` never reaches this native (the virtual door keys the native lookup
    // on the receiver's own class), so the static binding is the body the
    // JDK's own `invokespecial` would run in every case that gets here. The
    // `CompletableFuture` / `ThreadPoolExecutor` routers bind the same way.
    let out = ctx.invoke_special_bytecode_only(COW_SET_CLASS, method, descriptor, args);
    reentry_guard_leave(ctx, &COW_SET_GUARD, guard_pin);
    Some(out)
}

/// Get the backing HashMap from a HashSet.
pub(crate) fn is_hashset_native_backed(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    hs_map_slot(ctx, this).is_some()
}

/// The slot a HashSet-shaped receiver keeps its backing map in, or `None` when
/// the receiver is not HashSet-shaped at all.
///
/// For a real `HashSet` (or `CopyOnWriteArraySet`, or a subclass) it is
/// [`HS_FIELD_MAP`] — absolute 0, because nothing in that chain declares a
/// field before `HashSet.map`.
///
/// For one of the [`SET_VIEW_CARRIERS`] it is **past** the class's own declared
/// fields, and that is not decoration:
///
/// ```text
/// java.util.HashMap$KeySet             final java.util.HashMap this$0;
/// java.util.LinkedHashMap$LinkedKeySet final boolean reversed;
///                                      final java.util.LinkedHashMap this$0;
/// ```
///
/// The `Linked*` views declare `reversed` FIRST, so slot 0 there is a BOOLEAN.
/// Storing the backing map at slot 0 (which is what the first cut of this
/// change did) wrote a reference into an int-declared slot, and every
/// `linkedHashMap.keySet()` read back empty —
/// `probes/MapViewBehaviourProbe`'s `lhm.keys.size` 0 against HotSpot's 2, and
/// `AbstractMap.toString` recursing until the stack ran out. Anchoring on
/// `class_num_total_fields` puts it in an UNDECLARED slot, which resolves to no
/// field descriptor and is left untyped — the same rule the values carriers
/// follow, and the reason those were already safe (they write ArrayList's
/// absolute slots 1 and 2, past both declared fields).
pub(crate) fn hs_map_slot(ctx: &dyn NativeContext, this: ObjectRef) -> Option<usize> {
    let cid = ctx.class_id_of_object(this);
    // Memoized per `(vm, class)`. Reached once per `next()` through
    // `map_itr_comod_source`, and the two `is_subclass` calls below are what
    // put `Class::is_subclass_of_inner` and its `hashbrown` visited set at the
    // top of the `KeySetBench hoisted` profile. Positive answers only — see the
    // note on `class_memo_get` for why a `None` here must not be kept.
    thread_local! {
        pub(crate) static MEMO: ClassMemo<usize> = const { std::cell::RefCell::new(Vec::new()) };
    }
    let vm = ctx.vm_identity();
    if let Some(slot) = MEMO.with(|t| class_memo_get(t, vm, cid.as_u32())) {
        return Some(slot);
    }
    let answer = hs_map_slot_uncached(ctx, this, cid);
    if let Some(slot) = answer {
        MEMO.with(|t| class_memo_put(t, vm, cid.as_u32(), slot));
    }
    answer
}

/// The uncached body of [`hs_map_slot`].
pub(crate) fn hs_map_slot_uncached(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    cid: cratonvm_types::ClassId,
) -> Option<usize> {
    // A REAL `CopyOnWriteArraySet` keeps a `CopyOnWriteArrayList` in slot 0,
    // not a map: slot 0 IS its `al` field. Answering `HS_FIELD_MAP` for one
    // would hand every reader in this file a list to call `native_map_*` on,
    // and would make `is_synthetic_backed_collection` claim a receiver this VM
    // no longer backs. See [`cow_set_route`], which takes those receivers to
    // their own bytecode; the FABRICATED stub keeps the map shape, because
    // there the map surface is the implementation.
    if is_real_cow_array_set(ctx, this) {
        return None;
    }
    for class_name in [
        "java/util/HashSet",
        "java/util/concurrent/CopyOnWriteArraySet",
    ] {
        if let Some(base) = ctx.class_id_by_name(class_name) {
            if cid == base || ctx.is_subclass(cid, base) {
                return Some(HS_FIELD_MAP);
            }
        }
    }
    // Reached only after both subclass checks miss, so no ordinary set pays for
    // the name lookup.
    match ctx.class_name_arc_of_id(cid).as_deref() {
        Some(n) if is_set_view_carrier(n) => Some(ctx.class_num_total_fields(cid)),
        _ => None,
    }
}

/// Point a HashSet-shaped receiver at its backing map, at whatever slot
/// [`hs_map_slot`] says. Every view mint site goes through this rather than
/// writing [`HS_FIELD_MAP`] directly, so the reader and the writer cannot
/// disagree about where the backing lives.
pub(crate) fn hs_set_backing_map(ctx: &mut dyn NativeContext, set: ObjectRef, backing: ObjectRef) {
    let slot = hs_map_slot(&*ctx, set).unwrap_or(HS_FIELD_MAP);
    ctx.set_field(set, slot, Value::Object(Some(backing)));
}

pub(crate) fn hs_backing_map(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    let slot = hs_map_slot(ctx, this)?;
    // BOUND THE SLOT AGAINST THE OBJECT, not against the class.
    //
    // For a [`SET_VIEW_CARRIERS`] entry [`hs_map_slot`] answers
    // `class_num_total_fields` — a slot PAST the class's own declared fields,
    // which only exists because [`alloc_set_view_carrier`] allocates the
    // carrier wider than the class declares. A view the IMAGE'S OWN BYTECODE
    // built is exactly as wide as it declares, so that slot is off the end of
    // it, and `java/util/HashMap$EntrySet` declares one field:
    //
    // ```text
    //   WARN zgc real: field index OOB index=1 num_slots=1 op="get"   (x10)
    // ```
    //
    // Ten of those per `hashMap.entrySet().toArray()` on the 2026-09-11
    // dial-armed trace, on a run that otherwise looks clean. The heap hands
    // back a default for an out-of-range slot, so the read already ANSWERED
    // `None` and this bound changes no caller's answer — it stops the VM
    // reading a cell that is not the object's and saying so ten times. The
    // shape is `has_byte_array_stream_layout`'s, one family over: ask the slot
    // count before probing the layout.
    if slot < ctx.object_num_fields(this) {
        if let Value::Object(Some(m)) = ctx.get_field(this, slot) {
            return Some(m);
        }
    }
    // THE CARRIER THE IMAGE BUILT ITSELF still has an enclosing map, and it is
    // in the field the JDK's own constructor wrote: `this$0`.
    //
    // The bound above stops the out-of-range read; it does not answer the
    // question. Every `native_hs_*` on one of these classes ends in "and if
    // there is no backing map, the set is empty", so a view that real
    // `HashMap.entrySet()` bytecode minted answered EMPTY through this one
    // `None` -- `size()` 0, and `AbstractCollection.toArray` sizing its result
    // from that 0 (`Arrays.copyOf(r, 0)`) even though the iterator behind it
    // walks correctly.
    //
    // MEASURED 2026-09-12 on `cratonvm-l1w7-base-20260912`, `--jdk-only`, the
    // dial armed one family per process so the view accessors yield to real
    // bytecode and hand these natives a REAL carrier
    // (`apps/probes/L1MapViewToArrayProbe`):
    //
    // ```text
    //                       entrySet().toArray()   HotSpot   armed, before
    //   HashMap                              [3]       [3]           [0]
    //   LinkedHashMap                        [3]       [3]           [0]
    //   Hashtable                            [3]       [3]           [0]
    //   TreeMap                              [3]       [3]           [0]
    // ```
    //
    // Everything else in those arms already matched HotSpot -- `size()`,
    // `iterator()`, the for-each walk, `stream().count()`, `toString()` and the
    // real heap fields (`table`, `count`, `head`/`tail`, `root`). This one
    // answer is what stood between the four map families and a retirement, and
    // it is not a state-model gap: the state was there and unreadable through a
    // slot only OUR carrier has.
    //
    // Resolved BY NAME for the reason [`values_view_class_source`] gives on the
    // values half: `LinkedHashMap$LinkedKeySet` declares `reversed` FIRST, so a
    // hard-coded slot 0 reads a boolean as a reference. Restricted to the
    // carrier classes so an ordinary `HashSet` -- whose `map` IS slot 0 and
    // whose null there is a real answer -- keeps the `None` it has today.
    let name = ctx.class_name_arc_of_id(ctx.class_id_of_object(this))?;
    if !is_set_view_carrier(&name) {
        return None;
    }
    values_view_class_source(ctx, this)
}

/// Public helper: allocate a properly-initialised HashSet containing `elems`.
///
/// Field layout matches `make_set_of` / `native_hs_init`: a single
/// instance field at offset 0 holding the backing `java/util/HashMap`. This
/// is what real-JDK HashSet bytecode expects (`getfield map` reads field 0
/// → must be a HashMap; `HashMap.keySet()` then dispatches normally).
///
/// Used by `native-builtins/src/lib.rs::build_hashset_from_args` so that
/// the higher-arity `Set.of(...)` natives (4..=10 args) produce HashSets
/// whose layout is compatible with real-JDK `HashSet.iterator()` /
/// `AbstractSet.equals()` etc.
pub fn make_hashset_with_elements(
    ctx: &mut dyn NativeContext,
    elems: &[Value],
) -> Result<ObjectRef, MethodCallFailed> {
    // S111r13: Build the backing HashMap using the real-JDK field layout
    // (`table`, `size`, `threshold`, `loadFactor`, `entrySet`) instead of
    // the synthetic 3-field `(buckets, size, capacity)` layout.
    //
    // Why: JDK `HashSet.spliterator()` does
    //   `new HashMap.KeySpliterator<>(this.map, ...)` and reads the map's
    // `table` field directly via `getfield`. With the synthetic layout,
    // slot 2 held `Int(capacity=16)` rather than the bucket array, so
    // downstream `arraylength` panicked with
    //   `internal error: expected object reference, got int(16)`.
    //
    // Strategy: resolve the real HashMap / HashMap$Node field slot indices
    // via `resolve_field_index`. If every required field is resolvable,
    // allocate enough field slots to cover the real layout, populate at
    // the real indices, and build the bucket array + Node chain so that
    // bytecode `getfield`/`arraylength` see the correct types. Falls back
    // to the legacy synthetic 3-field layout if any slot resolution fails
    // (e.g. running before bootstrap completes or against a synthetic stub).
    //
    // Hashing follows the JDK formula: `h = key.hashCode() ^ (h >>> 16)`,
    // and bucket index is `(n - 1) & hash` for power-of-two `n`.
    //
    // `CRATONVM_COMPAT_HASHSET_BUILD_JDK` (see `hashset_build_jdk_enabled`):
    // size, link order and exceptions of `new HashSet<>()` + `add` each.
    // `elems.len()` bounds the distinct count, so a duplicate-carrying input
    // can at worst get one doubling more than the adds would.
    let jdk_build = hashset_build_jdk_enabled();
    let cap = if jdk_build {
        hashset_build_capacity(elems.len())
    } else {
        std::cmp::max(elems.len().next_power_of_two(), MAP_DEFAULT_CAPACITY)
    };

    // `elems` is the CALLER's raw slice, and every allocation below is a GC
    // point -- `alloc_ref_array`, `alloc_object` and `try_alloc_synthetic` in
    // the real-layout branch, plus one `native_map_put` per element in the
    // fallback. So the pin is taken HERE, ahead of the first of them, rather
    // than beside the loop that reads the elements, which is where it used to
    // sit: a caller that gathered its `ObjectRef`s out of a live collection
    // holds bare addresses, and three allocations are room enough for a moving
    // collector to leave every one of them stale before anything pinned it.
    // `native_props_string_property_names`, walking a `defaults` chain into a
    // plain `Vec`, is exactly that caller.
    //
    // This base is also the truncation point for both branches' final
    // `unpin_native_roots`, being the earliest pin either one takes -- except
    // for an EMPTY `elems`, where `pin_value_slice` pins nothing and answers
    // `usize::MAX`; then each branch falls back to its own first pin.
    let (elem_pin_base, elem_pins) = pin_value_slice(ctx, elems);

    // Best-effort: ensure the real classes are loaded so the field-index
    // resolver can see them.
    let hashmap_class_id = ctx
        .ensure_class_initialized("java/util/HashMap")
        .ok()
        .or_else(|| ctx.class_id_by_name("java/util/HashMap"))
        .unwrap_or(ClassId::new(0));
    let _ = ctx.ensure_class_initialized("java/util/HashMap$Node");
    let node_class_id = ctx
        .class_id_by_name("java/util/HashMap$Node")
        .unwrap_or(ClassId::new(0));

    let f_table = ctx.resolve_field_index("java/util/HashMap", "table");
    let f_size = ctx.resolve_field_index("java/util/HashMap", "size");
    let f_threshold = ctx.resolve_field_index("java/util/HashMap", "threshold");
    let f_loadfactor = ctx.resolve_field_index("java/util/HashMap", "loadFactor");
    let f_entryset = ctx.resolve_field_index("java/util/HashMap", "entrySet");
    let n_hash = ctx.resolve_field_index("java/util/HashMap$Node", "hash");
    let n_key = ctx.resolve_field_index("java/util/HashMap$Node", "key");
    let n_value = ctx.resolve_field_index("java/util/HashMap$Node", "value");
    let n_next = ctx.resolve_field_index("java/util/HashMap$Node", "next");

    if let (
        Some(f_table),
        Some(f_size),
        Some(f_threshold),
        Some(f_loadfactor),
        Some(f_entryset),
        Some(n_hash),
        Some(n_key),
        Some(n_value),
        Some(n_next),
    ) = (
        f_table,
        f_size,
        f_threshold,
        f_loadfactor,
        f_entryset,
        n_hash,
        n_key,
        n_value,
        n_next,
    ) {
        let map_n_fields = [f_table, f_size, f_threshold, f_loadfactor, f_entryset]
            .iter()
            .copied()
            .max()
            .unwrap_or(0)
            + 1;
        let node_n_fields = [n_hash, n_key, n_value, n_next]
            .iter()
            .copied()
            .max()
            .unwrap_or(0)
            + 1;

        // GC-SAFETY: `buckets`/`backing_map`/`set` are bare locals held
        // across every later `alloc_object` call below (backing_map, set,
        // and one per element for `node`) -- a stress-GC during any of them
        // can leave these stale, and `set` is the function's return value,
        // so a stale `set` hands the CALLER a dangling receiver. Pin each
        // right after its own allocation and re-read before every later
        // use, mirroring `map_alloc_node`/`native_map_put_evict_pinned`.
        let buckets = alloc_ref_array(ctx, cap);
        let pin_base = ctx.pin_native_root(buckets);
        let backing_map = ctx.alloc_object(hashmap_class_id, map_n_fields);
        // JVMS §2.3 defaults — see [`init_native_map_reference_defaults`]. This
        // branch already writes `entrySet` explicitly and overwrites `table`
        // two lines down; `keySet`/`values` are the slots that were left holding
        // `Int(0)`. Not a GC point, so the pin re-read below is unaffected.
        init_native_map_reference_defaults(ctx, backing_map);
        let buckets = ctx.read_native_pin(pin_base, buckets);
        ctx.set_field(backing_map, f_table, Value::Object(Some(buckets)));
        ctx.set_field(backing_map, f_size, Value::Int(0));
        ctx.set_field(backing_map, f_threshold, Value::Int((cap as i32 * 3) / 4));
        ctx.set_field(backing_map, f_loadfactor, Value::Float(0.75));
        ctx.set_field(backing_map, f_entryset, Value::Object(None));

        // Resolve HashSet's `map` slot too, with a defensive fallback to
        // the synthetic `HS_FIELD_MAP = 0` if unresolved.
        let hs_map_slot = ctx
            .resolve_field_index("java/util/HashSet", "map")
            .unwrap_or(HS_FIELD_MAP);
        let hs_n_fields = std::cmp::max(hs_map_slot + 1, HS_NUM_FIELDS);
        let backing_map_pin = ctx.pin_native_root(backing_map);
        let set = try_alloc_synthetic(ctx, "java/util/HashSet", hs_n_fields)?;
        let backing_map = ctx.read_native_pin(backing_map_pin, backing_map);
        ctx.set_field(set, hs_map_slot, Value::Object(Some(backing_map)));
        let set_pin = ctx.pin_native_root(set);

        // The set's PRESENT marker. It was `Value::Object(None)` here, with the
        // comment "null is fine for 'is in set'". It is not fine in either
        // direction: `native_hs_add`/`native_hs_remove` read membership out of
        // "was the previous value null", so a null marker makes every re-add
        // report NEW and every remove report `false` while deleting; and real
        // `java.util.HashSet.remove` is `map.remove(o) == PRESENT`, an identity
        // test a null can never pass. See [`hs_present_marker_at`]. Resolved
        // once, outside the per-element loop.
        let present_slot = hs_present_slot(&*ctx);
        let mut size = 0i32;
        // `CRATONVM_COMPAT_HASHSET_BUILD_TREEIFY`: every element linked so far,
        // in order, with its hash, for the replay through `put` when a bin
        // fills (see `hashset_build_finish_by_put`).
        let replay_on_full_bin = jdk_build && hashset_build_treeify_enabled();
        // `CRATONVM_COMPAT_HASHSET_BUILD_NULLS`: a null element hands the
        // build to the same replay (pure reads here), so `placed` must be
        // recorded whenever one is present.
        let keep_nulls =
            hashset_build_nulls_enabled() && elems.iter().any(|v| matches!(v, Value::Object(None)));
        let record_placed = replay_on_full_bin || keep_nulls;
        let mut placed: Vec<(usize, i32)> = Vec::new();
        // `CRATONVM_COMPAT_HASHSET_BUILD_TREEIFY_LEVELS`: per-bin counts of the
        // placed elements at every table the adds pass through below the
        // presized one (16, 32, .., `cap / 2`; one saturating byte per bin).
        // A bin reaching 9 there is a `treeifyBin` (a tree at 64+ slots, a
        // resize below) that the presized table cannot see, e.g. a tree formed
        // at 64 slots whose halves stay trees after a later split.
        let mut levels: Vec<Vec<u8>> = Vec::new();
        if replay_on_full_bin && hashset_build_treeify_levels_enabled() {
            let mut s = MAP_DEFAULT_CAPACITY;
            while s < cap {
                levels.push(vec![0u8; s]);
                s *= 2;
            }
        }
        for (i, elem) in elems.iter().enumerate() {
            let key_obj = match read_pinned_elem(ctx, elem_pins[i], *elem) {
                Value::Object(Some(obj)) => obj,
                // The first null (`CRATONVM_COMPAT_HASHSET_BUILD_NULLS`):
                // `add(null)` and every later `add` through `put`, after the
                // elements placed so far. It is the first, so it is absent.
                Value::Object(None) if keep_nulls => {
                    let finished = hashset_build_finish_by_put(
                        ctx,
                        (backing_map_pin, backing_map),
                        (f_table, f_size, f_threshold),
                        (elems, elem_pins.as_slice()),
                        &placed,
                        (i, 0),
                        present_slot,
                        true,
                    );
                    let set = ctx.read_native_pin(set_pin, set);
                    ctx.unpin_native_roots(pin_base.min(elem_pin_base));
                    return finished.map(|()| set);
                }
                _ => continue, // skip nulls / primitives we can't hash
            };
            // gc-common w31-b: `map_hash_key` can run the element's Java
            // `hashCode()`, so the identity fallback reads the RE-READ element;
            // it used to hash `key_obj`'s pre-call address.
            //
            // A throwing `hashCode()` propagates under `jdk_build`, as it does
            // out of `HashSet.add` (every caller of this builder already
            // returns through `?`). With the switch off it falls back to the
            // identity hash, the legacy behaviour.
            let hashed = map_hash_key(ctx, key_obj);
            let key_obj = ctx.read_native_pin(elem_pins[i], key_obj);
            let raw_hash = match hashed {
                Ok(h) => h,
                Err(e) if jdk_build => {
                    ctx.unpin_native_roots(pin_base.min(elem_pin_base));
                    return Err(e);
                }
                Err(_) => ctx.identity_hash_code(key_obj),
            };
            let buckets = ctx.read_native_pin(pin_base, buckets);
            // map_hash_key already applies the (h ^ h>>>16) spread; the
            // bucket index uses raw_hash as-is for power-of-two cap.
            let idx = ((cap as u32 - 1) & raw_hash as u32) as usize;

            // Skip duplicates (real-JDK Set.of rejects them; we just no-op).
            // `tail` is the chain's last node, where `jdk_build` links.
            let mut existing_head = ctx.get_array_element(buckets, idx);
            let mut dup = false;
            let mut probe = existing_head;
            let mut tail: Option<ObjectRef> = None;
            let mut chain_len = 0usize;
            let key_obj_pin = ctx.pin_native_root(key_obj);
            while let Value::Object(Some(probe_obj)) = probe {
                chain_len += 1;
                let key_obj = ctx.read_native_pin(key_obj_pin, key_obj);
                let probe_hash = match ctx.get_field(probe_obj, n_hash) {
                    Value::Int(hash) => hash,
                    _ => 0,
                };
                let probe_key = ctx.get_field(probe_obj, n_key);
                let mut probe_obj = probe_obj;
                if probe_hash == raw_hash {
                    let Value::Object(Some(pk)) = probe_key else {
                        tail = Some(probe_obj);
                        probe = ctx.get_field(probe_obj, n_next);
                        continue;
                    };
                    // `equals` is Java and can collect: the chain node is
                    // re-read from a pin before its `next` is followed (it
                    // used to be a bare local across the call).
                    let probe_pin = ctx.pin_native_root(probe_obj);
                    let equal = map_keys_equal(ctx, key_obj, pk);
                    probe_obj = ctx.read_native_pin(probe_pin, probe_obj);
                    ctx.unpin_native_roots(probe_pin);
                    let equal = match equal {
                        Ok(equal) => equal,
                        Err(e) if jdk_build => {
                            ctx.unpin_native_roots(pin_base.min(elem_pin_base));
                            return Err(e);
                        }
                        // Legacy: a thrown `equals` read as "not equal".
                        Err(_) => false,
                    };
                    if equal {
                        dup = true;
                        break;
                    }
                }
                tail = Some(probe_obj);
                probe = ctx.get_field(probe_obj, n_next);
            }
            if dup {
                continue;
            }
            // `CRATONVM_COMPAT_HASHSET_BUILD_TREEIFY`: this would be the 9th
            // node of one bin, where `add` treeifies (or resizes first). The
            // walk above was `putVal`'s, so the element goes in by its hash
            // without a second one; everything after it is an ordinary `add`.
            // The table the adds would have at this point is
            // `hashset_build_capacity(size)` (at least; a forced resize only
            // makes it larger, whose bins are sub-bins of these), so checking
            // it catches every earlier 9th node, conservatively.
            let intermediate_full = !levels.is_empty() && {
                let t = hashset_build_capacity(size.max(0) as usize);
                let li = (t.trailing_zeros() - MAP_DEFAULT_CAPACITY.trailing_zeros()) as usize;
                t < cap
                    && levels.get(li).is_some_and(|level| {
                        level[(raw_hash as u32 as usize) & (t - 1)] as usize >= HM_TREEIFY_THRESHOLD
                    })
            };
            if replay_on_full_bin && (chain_len >= HM_TREEIFY_THRESHOLD || intermediate_full) {
                let finished = hashset_build_finish_by_put(
                    ctx,
                    (backing_map_pin, backing_map),
                    (f_table, f_size, f_threshold),
                    (elems, elem_pins.as_slice()),
                    &placed,
                    (i, raw_hash),
                    present_slot,
                    keep_nulls,
                );
                let set = ctx.read_native_pin(set_pin, set);
                ctx.unpin_native_roots(pin_base.min(elem_pin_base));
                return finished.map(|()| set);
            }
            // Re-read head in case ctx mutated between probes (defensive).
            let buckets = ctx.read_native_pin(pin_base, buckets);
            existing_head = ctx.get_array_element(buckets, idx);

            // `alloc_object` below can move `key_obj`/`existing_head`/`tail`;
            // pin + re-read across it (mirrors `map_alloc_node`).
            let existing_head_pin = pin_value(ctx, existing_head);
            let tail_pin = match tail {
                Some(t) if jdk_build => Some(ctx.pin_native_root(t)),
                _ => None,
            };
            let node = ctx.alloc_object(node_class_id, node_n_fields);
            let key_obj = ctx.read_native_pin(elem_pins[i], key_obj);
            let existing_head = read_pinned_elem(ctx, existing_head_pin, existing_head);
            // Read AFTER the `alloc_object` above: `PRESENT` is a static field,
            // so the read always yields its current address and no pin is
            // needed — but it must not be hoisted above an allocation into a
            // bare local, which is why only the (class, slot) pair is hoisted.
            let sentinel = hs_present_marker_at(&*ctx, present_slot, Value::Object(Some(key_obj)));
            ctx.set_field(node, n_hash, Value::Int(raw_hash));
            ctx.set_field(node, n_key, Value::Object(Some(key_obj)));
            ctx.set_field(node, n_value, sentinel);
            match (tail, tail_pin) {
                // `HashMap.putVal`: `p.next = newNode(..)` -- the tail.
                (Some(t), Some(t_pin)) => {
                    ctx.set_field(node, n_next, Value::Object(None));
                    let t = ctx.read_native_pin(t_pin, t);
                    ctx.set_field(t, n_next, Value::Object(Some(node)));
                }
                // An empty bucket (either mode), or the legacy head insert.
                _ => {
                    ctx.set_field(node, n_next, existing_head);
                    let buckets = ctx.read_native_pin(pin_base, buckets);
                    ctx.set_array_element(buckets, idx, Value::Object(Some(node)));
                }
            }
            size += 1;
            if record_placed {
                placed.push((i, raw_hash));
            }
            for level in levels.iter_mut() {
                let bin = (raw_hash as u32 as usize) & (level.len() - 1);
                level[bin] = level[bin].saturating_add(1);
            }
        }
        let backing_map = ctx.read_native_pin(backing_map_pin, backing_map);
        ctx.set_field(backing_map, f_size, Value::Int(size));
        let set = ctx.read_native_pin(set_pin, set);
        ctx.unpin_native_roots(pin_base.min(elem_pin_base));
        return Ok(set);
    }

    // Legacy fallback: synthetic 3-field (buckets, size, capacity) layout.
    // Used when real HashMap/Node classes aren't resolvable yet.
    //
    // GC-SAFETY: same hazard as the real-layout branch above -- `set` and
    // `backing_map` are bare locals re-used across further allocating
    // calls (each other's alloc, plus one `native_map_put` per element,
    // which itself allocates map nodes/resizes the table). Pin `set`
    // first so its handle is the base for the final `unpin_native_roots`.
    let set = try_alloc_synthetic(ctx, "java/util/HashSet", HS_NUM_FIELDS)?;
    let set_pin = ctx.pin_native_root(set);
    let backing_map = alloc_backing_map(ctx);
    let backing_map_pin = ctx.pin_native_root(backing_map);
    let buckets = alloc_ref_array(ctx, cap);
    let backing_map = ctx.read_native_pin(backing_map_pin, backing_map);
    publish_map_table(ctx, backing_map, buckets, cap as i32);
    set_map_size(ctx, backing_map, 0);
    let set = ctx.read_native_pin(set_pin, set);
    let backing_map = ctx.read_native_pin(backing_map_pin, backing_map);
    hs_set_backing_map(ctx, set, backing_map);

    // The `let sentinel = Value::Int(1)` that stood here was dead — the marker
    // has come from `present_marker(elem)` since that helper was introduced —
    // and it read as the live marker to anyone scanning this loop. See
    // [`hs_present_marker_at`] for why a HashSet-family receiver needs the real
    // `PRESENT` and not the element.
    let present_slot = hs_present_slot(&*ctx);
    for (i, elem) in elems.iter().enumerate() {
        let elem = read_pinned_elem(ctx, elem_pins[i], *elem);
        let backing_map = ctx.read_native_pin(backing_map_pin, backing_map);
        let present = hs_present_marker_at(&*ctx, present_slot, elem);
        // Best-effort populate; ignore errors so callers see a non-empty
        // set even if a single put failed (e.g. unhashable wrapper) -- unless
        // `CRATONVM_COMPAT_HASHSET_BUILD_JDK`, where a throwing
        // `hashCode()`/`equals()` propagates as out of `HashSet.add`.
        let put = native_map_put(ctx, &[Value::Object(Some(backing_map)), elem, present]);
        if let Err(e) = put {
            if jdk_build {
                ctx.unpin_native_roots(set_pin.min(elem_pin_base));
                return Err(e);
            }
        }
    }
    let set = ctx.read_native_pin(set_pin, set);
    ctx.unpin_native_roots(set_pin.min(elem_pin_base));
    Ok(set)
}

/// The rest of [`make_hashset_with_elements`] once a bin is about to take its
/// 9th node (`CRATONVM_COMPAT_HASHSET_BUILD_TREEIFY`): the backing map goes
/// back to `new HashMap<>()`'s state (no table, `threshold` 0), the elements
/// already `placed` are put again in their order with the hash each was
/// placed with and no key comparison (the builder's walk already made the
/// `equals` calls `add` makes; they are known distinct), then the `current`
/// element the same way, then every later element as an ordinary `add` (its
/// `hashCode()`, the walk, a tree bin's `putTreeVal`). The native `put` grows
/// and treeifies as `putVal` does, so the result is the adds' table. The
/// nodes the builder linked are dropped with its table. `backing` is rooted
/// at its pin; `elem_pins` are `elems`' pins; `fields` are the backing map's
/// `table`, `size` and `threshold` slots. The caller releases every pin.
/// `keep_nulls` ([`hashset_build_nulls_enabled`]): a null element is `put`
/// (as `add(null)`) rather than skipped; `current` may then be the first null
/// (its hash is ignored: `put` hashes a null key as 0).
#[allow(clippy::too_many_arguments)]
pub(crate) fn hashset_build_finish_by_put(
    ctx: &mut dyn NativeContext,
    (backing_pin, backing): (usize, ObjectRef),
    (f_table, f_size, f_threshold): (usize, usize, usize),
    (elems, elem_pins): (&[Value], &[usize]),
    placed: &[(usize, i32)],
    current: (usize, i32),
    present_slot: Option<(ClassId, usize)>,
    keep_nulls: bool,
) -> Result<(), MethodCallFailed> {
    let map = ctx.read_native_pin(backing_pin, backing);
    ctx.set_field(map, f_table, Value::Object(None));
    ctx.set_field(map, f_size, Value::Int(0));
    ctx.set_field(map, f_threshold, Value::Int(0));
    let known = placed.iter().copied().chain(std::iter::once(current));
    let rest = (current.0 + 1..elems.len()).map(|i| (i, None));
    for (i, known_hash) in known.map(|(i, h)| (i, Some(h))).chain(rest) {
        let key = read_pinned_elem(&*ctx, elem_pins[i], elems[i]);
        let keep = match key {
            Value::Object(Some(_)) => true,
            Value::Object(None) => keep_nulls,
            _ => false,
        };
        if !keep {
            continue;
        }
        let present = hs_present_marker_at(&*ctx, present_slot, key);
        let mut map = ctx.read_native_pin(backing_pin, backing);
        let base = ctx.pin_native_root(map);
        let key_pin = pin_value(ctx, key);
        let present_pin = pin_value(ctx, present);
        let mode = PutMode {
            no_grow: false,
            assume_absent: known_hash.is_some(),
        };
        let put = native_map_put_evict_pinned_inner(
            ctx,
            &mut map,
            key,
            present,
            true,
            base,
            key_pin,
            present_pin,
            GrowAlloc::SingleAttempt,
            known_hash,
            false,
            mode,
        );
        ctx.unpin_native_roots(base);
        put?;
    }
    Ok(())
}

/// The node at one end of a `LinkedHashSet`'s encounter order, or `None` when
/// the set is empty or is not insertion-ordered at all.
pub(crate) fn lhs_end_node(ctx: &dyn NativeContext, this: ObjectRef, at_head: bool) -> Option<ObjectRef> {
    let backing = hs_backing_map(ctx, this)?;
    let end = if at_head {
        lhm_get(ctx, backing, "head", LHM_FIELD_HEAD)
    } else {
        lhm_get(ctx, backing, "tail", LHM_FIELD_TAIL)
    };
    match end {
        Value::Object(Some(nd)) => Some(nd),
        _ => None,
    }
}

/// `SequencedCollection.getFirst()` / `getLast()` — `NoSuchElementException` on
/// an empty set, which is the half a `peek`-shaped body gets wrong.
pub(crate) fn lhs_get_end(ctx: &mut dyn NativeContext, args: &[Value], at_head: bool) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    match lhs_end_node(&*ctx, this, at_head) {
        Some(nd) => Ok(Some(ctx.get_field(nd, LHM_NODE_KEY))),
        None => Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: String::new(),
            }
            .into(),
        ),
    }
}

pub(crate) fn native_lhs_get_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lhs_get_end(ctx, args, true)
}

pub(crate) fn native_lhs_get_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lhs_get_end(ctx, args, false)
}

/// `SequencedSet.addFirst(e)` / `addLast(e)`.
///
/// Both are `add` FOLLOWED by a move, and the move applies to an element that
/// was ALREADY present too -- which is the half that distinguishes them from
/// `add` and the half real bytecode could not do here.
pub(crate) fn lhs_add_at_end(ctx: &mut dyn NativeContext, args: &[Value], at_head: bool) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    // `LinkedHashSet.addFirst(e)` IS `map.putFirst(e, PRESENT)` on the JDK, so
    // under `CRATONVM_COMPAT_LHM_SINGLE_HASH` it is exactly that over the
    // backing `LinkedHashMap`: one `hashCode()`, where `add` + a lookup made
    // three. Nothing has run since `args` was read, so `backing` is current.
    if lhm_single_hash_enabled() {
        if let Some(backing) = hs_backing_map(&*ctx, this)
            .filter(|b| obj_is_instance_of(&*ctx, *b, "java/util/LinkedHashMap"))
        {
            let present = hs_present_marker(&*ctx, elem);
            return lhm_put_at_end(ctx, &[Value::Object(Some(backing)), elem, present], at_head)
                .map(|_| None);
        }
    }
    let this_pin = ctx.pin_native_root(this);
    let elem_pin = pin_value(ctx, elem);
    let added = native_hs_add(ctx, args);
    if let Err(e) = added {
        ctx.unpin_native_roots(this_pin);
        return Err(e);
    }
    let this_cur = ctx.read_native_pin(this_pin, this);
    let backing = hs_backing_map(&*ctx, this_cur);
    let elem_cur = read_pinned_elem(ctx, elem_pin, elem);
    if let Some(backing) = backing {
        let node = match lhm_find_node(ctx, backing, &elem_cur) {
            Ok(n) => n,
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };
        let this_cur = ctx.read_native_pin(this_pin, this);
        if let (Some(node), Some(backing)) = (node, hs_backing_map(&*ctx, this_cur)) {
            if at_head {
                lhm_move_to_head(ctx, backing, node);
            } else {
                lhm_move_to_tail(ctx, backing, node);
            }
        }
    }
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub(crate) fn native_lhs_add_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lhs_add_at_end(ctx, args, true)
}

pub(crate) fn native_lhs_add_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lhs_add_at_end(ctx, args, false)
}

/// `SequencedCollection.removeFirst()` / `removeLast()` — answer the element AND
/// remove it.
pub(crate) fn lhs_remove_end(ctx: &mut dyn NativeContext, args: &[Value], at_head: bool) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let node = match lhs_end_node(&*ctx, this, at_head) {
        Some(nd) => nd,
        None => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: String::new(),
                }
                .into(),
            )
        }
    };
    let elem = ctx.get_field(node, LHM_NODE_KEY);
    // `CRATONVM_COMPAT_LHS_NODE_OPS`: the JDK removes the end NODE (its stored
    // hash, no `hashCode()` and no `equals`); the element-keyed remove below
    // hashed and walked for it. Nothing has run since `this` was read.
    if lhs_node_ops_enabled() {
        if let Some(backing) = hs_backing_map(&*ctx, this)
            .filter(|b| is_lhm_receiver(&*ctx, *b) && view_backing_source(&*ctx, *b).is_none())
        {
            // A tree node's unlink runs bytecode and can collect: carry the
            // receiver and the element across it.
            let carry_pin = ctx.pin_native_root(this);
            let elem_pin = pin_value(ctx, elem);
            let unlinked = lhm_unlink_node(ctx, backing, node);
            let this_now = ctx.read_native_pin(carry_pin, this);
            let elem_now = read_pinned_elem(ctx, elem_pin, elem);
            ctx.unpin_native_roots(carry_pin);
            if unlinked {
                return Ok(Some(elem_now));
            }
            return lhs_remove_end_by_element(ctx, this_now, elem_now);
        }
    }
    lhs_remove_end_by_element(ctx, this, elem)
}

/// [`lhs_remove_end`]'s element-keyed removal (`native_hs_remove`), answering
/// the element.
pub(crate) fn lhs_remove_end_by_element(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    elem: Value,
) -> MethodCallResult {
    let this_pin = ctx.pin_native_root(this);
    let elem_pin = pin_value(ctx, elem);
    let this_cur = ctx.read_native_pin(this_pin, this);
    let elem_cur = read_pinned_elem(ctx, elem_pin, elem);
    let removed = native_hs_remove(ctx, &[Value::Object(Some(this_cur)), elem_cur]);
    let answer = read_pinned_elem(ctx, elem_pin, elem);
    ctx.unpin_native_roots(this_pin);
    removed?;
    Ok(Some(answer))
}

pub(crate) fn native_lhs_remove_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lhs_remove_end(ctx, args, true)
}

pub(crate) fn native_lhs_remove_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    lhs_remove_end(ctx, args, false)
}

/// Is `LinkedHashSet.reversed()` left to its real bytecode (the JDK's live
/// `ReverseLinkedHashSetView`) instead of [`native_lhs_reversed`]'s snapshot?
///
/// Round 14 wave 3 (lane compat3), proposal CP2-2 of
/// `jit-r14-compat2-proposals-RETIRED-20260930.md`, item 3 of
/// `r13w8-hashcompat3-compatible-collection-residuals-CLOSED-20260930.md`: a write to
/// the source after `reversed()` was never seen by the snapshot. The view's
/// members delegate to the natives already registered on the source
/// (`size`, `add`, `addFirst`/`Last`, `getFirst`/`Last`, `removeFirst`/`Last`,
/// so live and write-through), `iterator()` is
/// `map().sequencedKeySet().reversed().iterator()` (the native keySet carrier
/// and [`native_view_reversed`]: a fresh snapshot per iterator, so live per
/// call), and `toArray()` is the real `LinkedHashMap.keysToArray(.., true)`
/// walking the heap `tail` / `before` chain. That chain is current: every
/// native list edit goes through `lhm_link_tail` / `lhm_link_head` /
/// `lhm_unlink` / `lhm_move_to_*`, which write BOTH links on the heap nodes,
/// and `lhm_set` mirrors `head` / `tail` / `size` to the real fields of every
/// real-layout backing (`alloc_hs_backing` allocates a real `LinkedHashMap`).
/// A correctness fix, so on in `--compatible` too (owner decision 2026-09-27);
/// `--jdk-only` is unchanged, and the synthetic JDK, which has no bytecode for
/// it, keeps the native.
///
/// Kill switch `CRATONVM_COMPAT_LHS_REVERSED_REAL_VIEW=0` registers the
/// snapshot again; `CRATONVM_UNRETIRE_NATIVE_SHADOW` (the triple) does the same.
/// Read once per registry population (boot), so no cache.
pub(crate) fn lhs_reversed_left_to_bytecode(r: &NativeMethodRegistry) -> bool {
    r.real_jdk()
        && !r.compatibility_mode().is_jdk_only()
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_COMPAT_LHS_REVERSED_REAL_VIEW")
        && !cratonvm_native_api::unretire::is_excluded(
            "java/util/LinkedHashSet",
            "reversed",
            "()Ljava/util/SequencedSet;",
        )
}

/// `SequencedSet.reversed()` — a reverse-ordered `LinkedHashSet`.
///
/// A SNAPSHOT, like `LinkedHashMap.reversed`'s: the JDK's is a live
/// `ReverseOrderLinkedHashSetView`, and the difference shows only for a write
/// to the source AFTER the view was taken. Recorded rather than hidden; the
/// alternative here was a `NullPointerException` out of the real body, which no
/// caller can do anything with. On a real JDK in `--compatible` it is no longer
/// registered (see [`lhs_reversed_left_to_bytecode`]); it still serves the
/// synthetic JDK and the kill switch.
pub(crate) fn native_lhs_reversed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let backing = match hs_backing_map(&*ctx, this) {
        Some(b) => b,
        None => return Ok(Some(Value::Object(None))),
    };
    lhs_reversed_snapshot_of(ctx, this, backing)
}

/// A new `LinkedHashSet` holding `backing`'s (a `LinkedHashMap`'s) keys in
/// reverse order, filled from the stored hashes under
/// `CRATONVM_COMPAT_LHS_NODE_OPS` (no `hashCode()` / `equals()`). `this` is
/// the caller's receiver, kept rooted across the allocations; nothing is read
/// from it. [`native_lhs_reversed`]'s body, shared with
/// [`native_view_reversed`]'s key-set arm.
pub(crate) fn lhs_reversed_snapshot_of(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    backing: ObjectRef,
) -> MethodCallResult {
    let mut elems = lhm_collect_keys(&*ctx, backing);
    elems.reverse();
    // The stored hash of each element, in the same (reversed) order: the
    // JDK's reversed set is a VIEW and never calls `hashCode()` or `equals()`,
    // so under `CRATONVM_COMPAT_LHS_NODE_OPS` the snapshot is filled from the
    // stored hashes with no duplicate search (the source is a set).
    let node_ops = lhs_node_ops_enabled();
    let mut hashes = if node_ops {
        lhm_collect_hashes(&*ctx, backing)
    } else {
        Vec::new()
    };
    hashes.reverse();
    let node_ops = node_ops && hashes.len() == elems.len();
    let this_pin = ctx.pin_native_root(this);
    let (_, elem_pins) = pin_value_slice(ctx, &elems);
    let out = match ctx.new_object_initialized("java/util/LinkedHashSet", "()V", &[]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(None)));
        }
    };
    let out_pin = ctx.pin_native_root(out);
    for (i, e) in elems.iter().enumerate() {
        let out_cur = ctx.read_native_pin(out_pin, out);
        let e_cur = read_pinned_elem(ctx, elem_pins[i], *e);
        let out_backing = if node_ops {
            hs_backing_map(&*ctx, out_cur).filter(|b| is_lhm_receiver(&*ctx, *b))
        } else {
            None
        };
        // A primitive marker (no `HashSet.PRESENT` on this VM) keeps the `add`
        // path, which boxes it; `lhm_append_distinct` stores values as given.
        let present = hs_present_marker(&*ctx, e_cur);
        let added = match out_backing {
            Some(out_backing) if matches!(present, Value::Object(Some(_))) => {
                lhm_append_distinct(ctx, out_backing, e_cur, present, hashes[i])
            }
            _ => native_hs_add(ctx, &[Value::Object(Some(out_cur)), e_cur]).map(|_| ()),
        };
        if let Err(err) = added {
            ctx.unpin_native_roots(this_pin);
            return Err(err);
        }
    }
    let out = ctx.read_native_pin(out_pin, out);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(out))))
}

pub(crate) fn register_hashset_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // LETSGO_S1: Mirror HashSet's native surface onto the class names of
    // its real-JDK subclasses that share the same `field 0 = backing
    // map` layout. Without this mirroring, `LinkedHashSet.add(Object)Z`
    // dispatch — which `force_native` looks up by the receiver's *exact*
    // class name — falls off the synthetic-jdk dispatcher and surfaces a
    // `NoSuchMethodError`, breaking real-app boots that allocate
    // `LinkedHashSet`s during Spring/Log4J/SLF4J initialisation. Real
    // JDK semantics for `LinkedHashSet` differ only in iteration order
    // (insertion-order vs hash order), which our `make_set_of` /
    // `native_hs_iterator` already preserve via the synthetic backing
    // map's bucket walk for the purposes of caller code.
    //
    // `CopyOnWriteArraySet` IS STILL MIRRORED HERE AND IS NO LONGER SERVED BY
    // IT on a real receiver, which is a distinction worth stating where the
    // list is rather than only where the exception is.
    //
    // The premise above — "subclasses that share the same `field 0 = backing
    // map` layout" — was never true of that class. It is not a `HashSet`
    // subclass at all, and the one instance field the real one declares is
    // `al` (`Ljava/util/concurrent/CopyOnWriteArrayList;`) sitting at exactly
    // that slot 0. So `native_hs_init` put a `LinkedHashMap` where a list
    // belongs, and the sentence that ends this comment in its original form —
    // "applications that rely on their JDK bytecode keep working because the
    // natives only override matching signatures" — is the half that gave it
    // away: `removeIf` is one of the nineteen methods the class declares and
    // one of the few this registrar does NOT register, so its JDK body ran,
    // read `al`, and raised
    // `NoSuchMethodError: java.util.LinkedHashMap.removeIf`.
    //
    // [`cow_set_route`] now takes a real `CopyOnWriteArraySet` to its own
    // bytecode, method by method. The registrations stay because the
    // FABRICATED stub still needs them: there the map surface is the
    // implementation, and `is_real_cow_array_set` — a by-name test for `al` on
    // the receiver's class — is what tells the two apart.
    //
    // Same original rationale still holds for `EnumSet` and
    // `ConcurrentSkipListSet`: applications that allocate them via the
    // synthetic-stub path get a working contract, and applications that rely
    // on their JDK bytecode keep working because the natives only override
    // matching signatures.
    const SET_CLASSES: &[&str] = &[
        "java/util/HashSet",
        "java/util/LinkedHashSet",
        "java/util/concurrent/CopyOnWriteArraySet",
    ];

    // THE JDK 21 `SequencedSet` SURFACE, on `LinkedHashSet` only: the other two
    // SET_CLASSES are not sequenced, and registering these for them would give
    // a `HashSet` an encounter order it does not have.
    r.register(
        "java/util/LinkedHashSet",
        "getFirst",
        "()Ljava/lang/Object;",
        native_lhs_get_first,
    );
    r.register(
        "java/util/LinkedHashSet",
        "getLast",
        "()Ljava/lang/Object;",
        native_lhs_get_last,
    );
    r.register(
        "java/util/LinkedHashSet",
        "addFirst",
        "(Ljava/lang/Object;)V",
        native_lhs_add_first,
    );
    r.register(
        "java/util/LinkedHashSet",
        "addLast",
        "(Ljava/lang/Object;)V",
        native_lhs_add_last,
    );
    r.register(
        "java/util/LinkedHashSet",
        "removeFirst",
        "()Ljava/lang/Object;",
        native_lhs_remove_first,
    );
    r.register(
        "java/util/LinkedHashSet",
        "removeLast",
        "()Ljava/lang/Object;",
        native_lhs_remove_last,
    );
    if !lhs_reversed_left_to_bytecode(r) {
        r.register(
            "java/util/LinkedHashSet",
            "reversed",
            "()Ljava/util/SequencedSet;",
            native_lhs_reversed,
        );
    }

    for c in SET_CLASSES {
        r.register(c, "<init>", "()V", native_hs_init);
        r.register(c, "<init>", "(I)V", native_hs_init_capacity);
        r.register(c, "<init>", "(IF)V", native_hs_init_capacity_load);
        r.register(
            c,
            "<init>",
            "(Ljava/util/Collection;)V",
            native_hs_init_from_collection,
        );
        r.register(c, "size", "()I", native_hs_size);
        r.register(c, "isEmpty", "()Z", native_hs_is_empty);
        r.register(c, "add", "(Ljava/lang/Object;)Z", native_hs_add);
        r.register(c, "remove", "(Ljava/lang/Object;)Z", native_hs_remove);
        r.register(c, "contains", "(Ljava/lang/Object;)Z", native_hs_contains);
        r.register(c, "clear", "()V", native_hs_clear);
        r.register(c, "iterator", "()Ljava/util/Iterator;", native_hs_iterator);
        r.register(c, "toArray", "()[Ljava/lang/Object;", native_hs_to_array);
        r.register(
            c,
            "toArray",
            "([Ljava/lang/Object;)[Ljava/lang/Object;",
            native_hs_to_array_typed,
        );
        r.register(
            c,
            "toArray",
            "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
            native_collection_to_array_generator,
        );
        r.register(c, "toString", "()Ljava/lang/String;", native_hs_to_string);
        r.register(c, "hashCode", "()I", native_hs_hash_code);
        r.register(c, "equals", "(Ljava/lang/Object;)Z", native_hs_equals);
        r.register(
            c,
            "forEach",
            "(Ljava/util/function/Consumer;)V",
            native_hs_for_each,
        );
        r.register(c, "stream", "()Ljava/util/stream/Stream;", native_hs_stream);
        r.register(
            c,
            "spliterator",
            "()Ljava/util/Spliterator;",
            native_hs_spliterator,
        );
        r.register(c, "addAll", "(Ljava/util/Collection;)Z", native_hs_add_all);
        r.register(
            c,
            "removeAll",
            "(Ljava/util/Collection;)Z",
            native_hs_remove_all,
        );
        r.register(
            c,
            "retainAll",
            "(Ljava/util/Collection;)Z",
            native_hs_retain_all,
        );
        r.register(
            c,
            "containsAll",
            "(Ljava/util/Collection;)Z",
            native_hs_contains_all,
        );
    }
    // `java/util/AbstractSet.hashCode()` deleted 2026-09-20: AbstractSet is
    // abstract and this VM mints no object whose class name is literally
    // "java/util/AbstractSet", so no receiver could ever resolve to it.
    // Confirmed dead (not just unreached) by
    // `l3-followups-the-carrier-identity-and-the-dead-registrations-20260830.md`
    // §2 and this session's own re-measurement, both modes, several receiver
    // shapes including a method reference (the declaring-class door).
    r.set_category(__prev_cat);
    // The classes a map's `keySet()`/`entrySet()` view is minted under.
    register_set_view_carrier_natives(r);
}

/// The `Set` surface of a keySet/entrySet view, registered on each of the
/// [`SET_VIEW_CARRIERS`] — the set-shaped twin of
/// [`register_map_view_carrier_natives`].
///
/// A view is HashSet-LAYOUT under its own class, so it needs the same
/// `native_hs_*` bodies the `HashSet` registration installs. It is not optional:
/// `HashMap$KeySet` declares `size`, `contains`, `remove`, `iterator`, `clear`,
/// `forEach` and `spliterator` itself, and the receiver-has-own-bytecode rule
/// would run those JDK bodies over `this$0` — null on a CratonVM view, whose
/// state is the backing map in that same slot. Paired with the entries in
/// `force_native_over_real_jdk_bytecode` and its `vm_exec` twin.
///
/// `equals`/`hashCode` ARE registered here, unlike on the values carriers: the
/// JDK's set views extend `AbstractSet`, whose `equals`/`hashCode` are the SET
/// contract, which is exactly what `native_hs_equals`/`native_hs_hash_code`
/// implement. (On a `Collection` view they would have installed LIST semantics,
/// which is why that family omits them.)
///
/// `<init>` is deliberately absent: nothing constructs these, and a constructor
/// native would fire for a JDK-built view too.
/// CLUSTER NOTE (H4-1, 2026-08-20). Same four-family span as
/// [`register_map_view_carrier_natives`], plus one extra edge that is easy to
/// miss: the `iterator` row below is [`native_hs_iterator`], and it mints its
/// result through [`key_itr_carrier_for`], which answers
/// `java/util/HashMap$KeyIterator` for **every receiver that is not
/// LinkedHashMap-shaped** — including `java/util/Hashtable$KeySet` and
/// `Hashtable$EntrySet`, whose producers are not part of the HashMap cluster.
///
/// So the four [`MAP_KEY_ITR_CARRIERS`] cannot be refused while any
/// `SET_VIEW_CARRIERS` entry outside the moving family is still `Bridge`: the
/// live Hashtable carrier would mint a `HashMap$KeyIterator` whose natives are
/// gone, and real `HashMap$HashIterator.hasNext()` would read a null `next`
/// field and report an EMPTY iteration rather than fail. Moving them needs
/// either the Properties/Hashtable cluster in the same commit, or a
/// Hashtable-family iterator carrier of its own
/// (`java/util/Hashtable$Enumerator` is what HotSpot actually answers here,
/// and `deprecated_util.rs` already names it).
pub(crate) fn register_set_view_carrier_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    for c in SET_VIEW_CARRIERS {
        let c = *c;
        // The `JdkConcurrentHashMap` row (wave 76, L1): see
        // `register_map_view_carrier_natives`.
        if c.starts_with(CHM_CLASS) && chm_row_registers_nothing(r) {
            continue;
        }
        // PER CARRIER — see [`CarrierFamily`] and the note above.
        r.set_category(carrier_family_kind(carrier_family_of(c)));
        r.register(c, "size", "()I", native_hs_size);
        r.register(c, "isEmpty", "()Z", native_hs_is_empty);
        r.register(c, "add", "(Ljava/lang/Object;)Z", native_hs_add);
        r.register(c, "remove", "(Ljava/lang/Object;)Z", native_hs_remove);
        r.register(c, "contains", "(Ljava/lang/Object;)Z", native_hs_contains);
        r.register(c, "clear", "()V", native_hs_clear);
        r.register(c, "iterator", "()Ljava/util/Iterator;", native_hs_iterator);
        r.register(c, "toArray", "()[Ljava/lang/Object;", native_hs_to_array);
        r.register(
            c,
            "toArray",
            "([Ljava/lang/Object;)[Ljava/lang/Object;",
            native_hs_to_array_typed,
        );
        r.register(
            c,
            "toArray",
            "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
            native_collection_to_array_generator,
        );
        r.register(c, "toString", "()Ljava/lang/String;", native_hs_to_string);
        r.register(c, "hashCode", "()I", native_hs_hash_code);
        r.register(c, "equals", "(Ljava/lang/Object;)Z", native_hs_equals);
        r.register(
            c,
            "forEach",
            "(Ljava/util/function/Consumer;)V",
            native_hs_for_each,
        );
        r.register(c, "stream", "()Ljava/util/stream/Stream;", native_hs_stream);
        r.register(
            c,
            "spliterator",
            "()Ljava/util/Spliterator;",
            native_hs_spliterator,
        );
        r.register(c, "addAll", "(Ljava/util/Collection;)Z", native_hs_add_all);
        r.register(
            c,
            "removeAll",
            "(Ljava/util/Collection;)Z",
            native_hs_remove_all,
        );
        r.register(
            c,
            "retainAll",
            "(Ljava/util/Collection;)Z",
            native_hs_retain_all,
        );
        r.register(
            c,
            "containsAll",
            "(Ljava/util/Collection;)Z",
            native_hs_contains_all,
        );
        // `ConcurrentHashMap$EntrySetView` DECLARES `removeIf`, so without this
        // the receiver-has-own-bytecode rule ran `map.removeEntryIf(filter)`
        // over a view whose `map` field is null. See `native_hs_remove_if`.
        r.register(
            c,
            "removeIf",
            "(Ljava/util/function/Predicate;)Z",
            native_hs_remove_if,
        );
    }
    r.set_category(__prev_cat);
}

/// LETSGO_S1: HashSet(int initialCapacity, float loadFactor) — mirror of
/// `native_hs_init_capacity` that ignores the load factor (matches our
/// HashMap layout, where loadFactor is fixed at 0.75).
pub(crate) fn native_hs_init_capacity_load(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // BEFORE the trim, or the load factor is validated by nobody: the guard
    // reads `args[2]`, and the (I)V path this delegates to never sees it.
    map_ctor_capacity_load_check(args, MapCtorRefusalSpelling::HashMapFamily)?;
    // `CRATONVM_COMPAT_HASHSET_CTOR_JDK`: the (I)V path hands the factor to
    // the backing map (see `hashset_ctor_jdk_enabled`); otherwise it is
    // dropped, as before.
    if hashset_ctor_jdk_enabled() {
        return native_hs_init_capacity(ctx, args);
    }
    // Drop the loadFactor (last) arg and reuse the (I)V path.
    let trimmed: Vec<Value> = args.iter().take(2).copied().collect();
    native_hs_init_capacity(ctx, &trimmed)
}

/// LETSGO_S1: HashSet.toArray(T[]) — typed-array variant. Our existing
/// `native_hs_to_array` returns a fresh `Object[]`, but the typed
/// variant must populate the caller's array (or allocate a new one of
/// the same component type if the input is too small). For now we
/// allocate a fresh `Object[]` of the right size — sufficient for the
/// callers we observe (Spring's iteration of HashSet keysets). When the
/// input array is large enough we copy into it and write null at index
/// `size` per spec; otherwise we fall back to a fresh allocation.
pub(crate) fn native_hs_to_array_typed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A real `CopyOnWriteArraySet` is served by its own bytecode over the
    // `CopyOnWriteArrayList` in `al`, not by this file's map surface.
    // See [`cow_set_route`].
    if let Some(r) = cow_set_route(
        ctx,
        args,
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
    ) {
        return r;
    }
    reject_null_to_array_template(args.get(1))?;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let target = match args.get(1).copied() {
        Some(Value::Object(Some(t))) => Some(t),
        _ => None,
    };
    let backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => return Ok(Some(Value::Object(None))),
    };
    // gc-common w10-g: the receiver, the backing map and the template are
    // rooted for the whole call. `hs_view_elements` is a GC point (a foreign
    // entry view allocates its live entries), and all three were bare copies
    // read after it: the template stored into, `this` read for its class by
    // the second `hs_view_elements`, which also ran after the allocation with
    // only `backing` rooted. Released together by the one unpin below.
    let this_pin = ctx.pin_native_root(this);
    let backing_pin = ctx.pin_native_root(backing);
    let target = target.map(|t| (ctx.pin_native_root(t), t));
    let out = hs_to_array_typed_rooted(ctx, (this_pin, this), (backing_pin, backing), target);
    ctx.unpin_native_roots(this_pin);
    out
}

/// The body of [`native_hs_to_array_typed`], with the receiver, the backing
/// map and the template pinned by the caller as `(handle, reference)`. Pins
/// taken here sit above the caller's, which releases them too.
pub(crate) fn hs_to_array_typed_rooted(
    ctx: &mut dyn NativeContext,
    this: (usize, ObjectRef),
    backing: (usize, ObjectRef),
    target: Option<(usize, ObjectRef)>,
) -> MethodCallResult {
    let (this_pin, this) = this;
    let (backing_pin, backing) = backing;
    let keys = hs_view_elements(ctx, this, backing)?;
    let this = ctx.read_native_pin(this_pin, this);
    let backing = ctx.read_native_pin(backing_pin, backing);
    let target = target.map(|(pin, t)| ctx.read_native_pin(pin, t));
    if let Some(arr) = target {
        let len = ctx.array_length(arr);
        if len >= keys.len() {
            // Storing into the caller-supplied array: no allocation between the
            // collect and the stores, so the gathered refs stay valid.
            for (i, k) in keys.iter().enumerate() {
                ctx.set_array_element(arr, i, *k);
            }
            if len > keys.len() {
                ctx.set_array_element(arr, keys.len(), Value::Object(None));
            }
            return Ok(Some(Value::Object(Some(arr))));
        }
    }
    // Template too small (or absent): allocate a NEW array of the template's
    // runtime component type (JDK contract), not a bare `Object[]`. The array
    // header stores its component class id, so `class_id_of_object(t)` is the
    // component id `new_ref_array` wants — keeps `Set<String>.toArray(new
    // String[0])` typed `String[]` instead of `Object[]`.
    //
    // GC-safe (HIB-CV-18): the allocation may move the element objects, so the
    // `keys` gathered above are stale once `new_ref_array`/`alloc_ref_array`
    // returns. Use only their count for sizing, then RE-collect from the live
    // backing after the alloc and store immediately (no alloc in between).
    let len = keys.len();
    let comp = match target {
        Some(t) => ctx.class_id_of_object(t),
        None => ClassId::new(0),
    };
    // Registered-only (`toArray(T[])` of `SET_CLASSES` and of the set-view
    // carriers), so the allocation may collect and retry (gc-common w10-g).
    // `live` audit: `this` and `backing` are pinned by the caller and re-read
    // below; the template is read for its class id above and not used after;
    // the elements are RE-collected below.
    let arr = new_ref_array_grow(ctx, comp, len, GrowAlloc::Reclaim);
    let this = ctx.read_native_pin(this_pin, this);
    let backing = ctx.read_native_pin(backing_pin, backing);
    // The re-collect is a GC point too, and `arr` is stored into after it.
    let arr_pin = ctx.pin_native_root(arr);
    let keys = hs_view_elements(ctx, this, backing);
    let arr = ctx.read_native_pin(arr_pin, arr);
    let keys = keys?;
    // `AbstractSet`/`AbstractCollection.toArray(T[])` stores through
    // `aastore` in its own loop, so a narrowing element is an
    // `ArrayStoreException` naming the VALUE -- `java.lang.Integer`, not
    // arraycopy's sentence, which is `ArrayList`'s (MEASURED on 25.0.4+7,
    // `probes/DodArrayStoreSweep`). Unchecked before, in BOTH modes: these
    // registrations are `Bridge`, so `--jdk-only` does not refuse them and
    // strict was wrong here too.
    //
    // The ZERO-arg `toArray()` next door needs no check and does not get one:
    // it returns an `Object[]`, which accepts every reference.
    for (i, k) in keys.iter().enumerate().take(len) {
        if let Some(e) = cratonvm_native_api::array_store::reject_unstorable(
            ctx,
            arr,
            *k,
            cratonvm_native_api::array_store::StoreRoute::Aastore,
        ) {
            return Err(e);
        }
        ctx.set_array_element(arr, i, *k);
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// LETSGO_S1: HashSet.hashCode — sum of `hashCode()` over elements
/// (matches `AbstractSet.hashCode` semantics).
///
/// This native WAS registered on `java/util/AbstractSet` too (where HashSet's
/// inherited `hashCode()` resolves) until `e8d96c293` (2026-09-20) deleted that
/// row as a dead door; it now serves the HashSet family and the set-view
/// carriers only. The history below is why it still handles *any* `AbstractSet`
/// subclass — including user/library sets whose elements do not live in a
/// HashSet-shaped backing map, e.g. Weld's `ImmutableTinySet$Doubleton`
/// (members held in named `element1`/`element2` fields). For those
/// `hs_backing_map` is `None`, and the bare `collect_collection_elements`
/// extractor — which only knows fixed collection layouts — returned an empty
/// Vec, so `hashCode()` was silently 0 instead of the `AbstractSet` contract's
/// element-hash sum. A 0 hash still compares `equals` correctly but lands in
/// the wrong HashMap bucket, so a `Map<Set<..>, ..>` keyed by such a set is
/// unfindable with an equal `HashSet` probe (the `getSharedSet` shape from
/// HIB-CV-25; caught by regression-suite `RCollections` "set-key true hit").
/// Use the `_or_real` variant — the same fix already applied to
/// `containsAll`/`retainAll`/`removeAll` — which falls back to the
/// collection's real `toArray()`.
pub(crate) fn native_hs_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A real `CopyOnWriteArraySet` is served by its own bytecode over the
    // `CopyOnWriteArrayList` in `al`, not by this file's map surface.
    // See [`cow_set_route`].
    if let Some(r) = cow_set_route(ctx, args, "hashCode", "()I") {
        return r;
    }
    if let Some(r) = ksv_route(ctx, args, native_ksv_hash_code) {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let keys = match hs_backing_map(ctx, this) {
        Some(m) => map_collect_keys(ctx, m),
        None => collect_collection_elements_or_real(ctx, this)?,
    };
    let (pin_base, handles) = pin_value_slice(ctx, &keys);
    let mut h: i32 = 0;
    for i in 0..keys.len() {
        let k = read_pinned_elem(ctx, handles[i], keys[i]);
        // Set.hashCode contract: sum of element hashCode()s (0 for null).
        match element_hash_code(ctx, &k) {
            Ok(eh) => h = h.wrapping_add(eh),
            Err(e) => {
                ctx.unpin_native_roots(pin_base);
                return Err(e);
            }
        }
    }
    ctx.unpin_native_roots(pin_base);
    Ok(Some(Value::Int(h)))
}

/// LETSGO_S1: HashSet.equals — same size + every element of `this` is in
/// `other` (`AbstractSet.equals` semantics).
pub(crate) fn native_hs_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A real `CopyOnWriteArraySet` is served by its own bytecode over the
    // `CopyOnWriteArrayList` in `al`, not by this file's map surface.
    // See [`cow_set_route`].
    if let Some(r) = cow_set_route(ctx, args, "equals", "(Ljava/lang/Object;)Z") {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(Some(Value::Int(0))),
    };
    if std::ptr::eq(this.as_ptr(), other.as_ptr()) {
        return Ok(Some(Value::Int(1)));
    }
    // `AbstractSet.equals` contract (JDK): a Set equals only another Set. The
    // `instanceof Set` guard MUST precede the `other.size()` call below —
    // without it, `set.equals(aString)` invokes `String.size()` (no such
    // method) and raises NoSuchMethodError instead of returning false. Spring's
    // `AbstractBeanFactory.isPrototypeCurrentlyInCreation` hits exactly this:
    // when ≥2 prototypes are in creation the `prototypesCurrentlyInCreation`
    // ThreadLocal holds a `HashSet`, and `curVal.equals(beanName)` compares that
    // set against a `String` bean name.
    if !obj_is_instance_of(ctx, other, "java/util/Set") {
        return Ok(Some(Value::Int(0)));
    }
    let this_pin = ctx.pin_native_root(this);
    let other_pin = ctx.pin_native_root(other);
    let result = (|| {
        let other = ctx.read_native_pin(other_pin, other);
        let other_size = match ctx.invoke_virtual(other, "size", "()I", &[])? {
            Some(Value::Int(n)) => n,
            _ => return Ok(Some(Value::Int(0))),
        };
        let this = ctx.read_native_pin(this_pin, this);
        let backing = match hs_backing_map(ctx, this) {
            Some(m) => m,
            None => return Ok(Some(Value::Int(0))),
        };
        let keys = map_collect_keys(ctx, backing);
        let (keys_pin, handles) = pin_value_slice(ctx, &keys);
        let contains_result: Result<bool, MethodCallFailed> = (|| {
            if keys.len() as i32 != other_size {
                return Ok(false);
            }
            for i in 0..keys.len() {
                let other = ctx.read_native_pin(other_pin, other);
                let key = read_pinned_elem(ctx, handles[i], keys[i]);
                let contains =
                    ctx.invoke_virtual(other, "contains", "(Ljava/lang/Object;)Z", &[key])?;
                if !matches!(contains, Some(Value::Int(1))) {
                    return Ok(false);
                }
            }
            Ok(true)
        })();
        ctx.unpin_native_roots(keys_pin);
        Ok(Some(Value::Int(if contains_result? { 1 } else { 0 })))
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

/// LETSGO_S1: HashSet.containsAll(Collection<?>) — true iff every element
/// of the source collection is present in this set.  Wraps the existing
/// `native_hs_contains` and `collect_collection_elements` helpers.
///
/// Uses `collect_collection_elements_or_real` (NOT the bare
/// `collect_collection_elements`): when the argument is a foreign collection
/// whose layout none of the heuristics model — e.g. Weld's
/// `ImmutableTinySet$Doubleton`, which stores its members in named `element1`/
/// `element2` fields — the bare extractor returns an empty Vec, and an empty
/// argument makes `containsAll` vacuously `true`. That false positive poisoned
/// Weld's CDI bootstrap: `ImmutableSet.equalsSet` is `size==size &&
/// other.containsAll(this)`, so a producer's qualifier set `{Parameters,Any}`
/// compared equal to an interned `{Produces,Parameters}`, and
/// `SharedObjectCache.getSharedSet` handed back the wrong canonical set —
/// surfacing as `WELD-001301: @Produces is not a qualifier` (HIB-CV-25). The
/// `_or_real` variant falls back to the collection's real `toArray()`.
pub(crate) fn native_hs_contains_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A real `CopyOnWriteArraySet` is served by its own bytecode over the
    // `CopyOnWriteArrayList` in `al`, not by this file's map surface.
    // See [`cow_set_route`].
    if let Some(r) = cow_set_route(ctx, args, "containsAll", "(Ljava/util/Collection;)Z") {
        return r;
    }
    // C4W4-6: `containsAll(null)` is the helpful NPE, not `false`.
    reject_null_contains_all_arg(args.get(1))?;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    // GC-safety: `collect_collection_elements_or_real` dispatches Java and the
    // per-element `native_hs_contains` below dispatches more (the source map's
    // `containsKey`/`get`, the elements' `hashCode`/`equals`). `this` and every
    // element sit in bare Rust locals across those calls — the `Vec` is not a
    // GC root, so its contents would be pre-move addresses.
    let this_pin = ctx.pin_native_root(this);
    // JIT round 16 (lane compatdrain), r15w4-compat5 item 6: a program
    // collection is walked by its own iterator, stopping at the first miss.
    if bulk_add_jdk_questions_enabled() && copy_source_is_foreign(&*ctx, coll) {
        let walked = contains_all_by_iterator(ctx, (this_pin, this), coll, native_hs_contains);
        ctx.unpin_native_roots(this_pin);
        return walked.map(|all| Some(Value::Int(i32::from(all))));
    }
    // A throwing walk releases `this_pin` (round 15 wave 4, lane compat5: the
    // `?` left it up).
    let collected = collect_collection_elements_or_real(ctx, coll);
    let elems = unpin_on_err(ctx, this_pin, collected)?;
    let (_elem_base, elem_pins) = pin_value_slice(ctx, &elems);
    let mut answer = 1;
    let mut failure = None;
    for (i, e) in elems.iter().enumerate() {
        let this = ctx.read_native_pin(this_pin, this);
        let e = read_pinned_elem(ctx, elem_pins[i], *e);
        match native_hs_contains(ctx, &[Value::Object(Some(this)), e]) {
            Ok(r) => {
                if !matches!(r, Some(Value::Int(1))) {
                    answer = 0;
                    break;
                }
            }
            Err(err) => {
                failure = Some(err);
                break;
            }
        }
    }
    ctx.unpin_native_roots(this_pin);
    if let Some(err) = failure {
        return Err(err);
    }
    Ok(Some(Value::Int(answer)))
}

/// `AbstractCollection.containsAll(c)` / `ConcurrentHashMap$CollectionView
/// .containsAll(c)` for a program collection `coll`: `for (Object e : c) if
/// (!contains(e)) return false;` -- `c.iterator()`, then per element
/// `hasNext()`, `next()` and the receiver's `contains` (`contains_fn` on the
/// receiver rooted at `this_pin`), stopping at the first miss, where the
/// natives collected the whole argument first (`toArray()` / its iterator to
/// the end). JIT round 16 (lane compatdrain), r15w4-compat5 item 6 (proposal
/// C5W4-5), under `CRATONVM_COMPAT_BULK_ADD_JDK_QUESTIONS`. The caller
/// releases `this_pin`.
pub(crate) fn contains_all_by_iterator(
    ctx: &mut dyn NativeContext,
    (this_pin, this): (usize, ObjectRef),
    coll: ObjectRef,
    contains_fn: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult,
) -> Result<bool, MethodCallFailed> {
    let it = match ctx.invoke_virtual(coll, "iterator", "()Ljava/util/Iterator;", &[])? {
        Some(Value::Object(Some(it))) => it,
        // `for (Object e : c)` over a null iterator: `hasNext()` on null.
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
    let it_pin = ctx.pin_native_root(it);
    let walked = (|| -> Result<bool, MethodCallFailed> {
        loop {
            let it = ctx.read_native_pin(it_pin, it);
            match ctx.invoke_virtual(it, "hasNext", "()Z", &[])? {
                Some(Value::Int(n)) if n != 0 => {}
                _ => return Ok(true),
            }
            let it = ctx.read_native_pin(it_pin, it);
            let e = ctx
                .invoke_virtual(it, "next", "()Ljava/lang/Object;", &[])?
                .unwrap_or(Value::Object(None));
            // Nothing collects between `next()` and the query, which roots
            // its own arguments across its Java.
            let this = ctx.read_native_pin(this_pin, this);
            if !matches!(
                contains_fn(ctx, &[Value::Object(Some(this)), e])?,
                Some(Value::Int(1))
            ) {
                return Ok(false);
            }
        }
    })();
    ctx.unpin_native_roots(it_pin);
    walked
}

/// A fresh synthetic backing map ([`alloc_backing_map`]) after
/// `map_init_eager`, at its CURRENT address.
///
/// gc-common w20-a: the eager initialisation allocates the bucket table, and
/// the six inline `alloc_backing_map` + `map_init_eager` pairs this replaces
/// went on to use (publish, `put` into, or wire into a set) the map's
/// pre-initialisation address.
pub(crate) fn alloc_initialised_backing_map(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    let map = alloc_backing_map(ctx);
    let (map, init) = rooted_across1(ctx, map, |ctx| {
        map_init_eager(ctx, &[Value::Object(Some(map))])
    });
    init?;
    Ok(map)
}

/// A fresh synthetic `java/util/HashSet` wired to a fresh initialised backing
/// map: `(set, backing_map)`, both at their current addresses.
///
/// gc-common w20-a: the set is rooted across the backing map's allocation and
/// initialisation; the inline copies stored the map into the set's
/// pre-allocation address.
pub(crate) fn alloc_synthetic_hash_set(
    ctx: &mut dyn NativeContext,
) -> Result<(ObjectRef, ObjectRef), MethodCallFailed> {
    let set = try_alloc_synthetic(ctx, "java/util/HashSet", HS_NUM_FIELDS)?;
    let (set, inner) = rooted_across1(ctx, set, |ctx| alloc_initialised_backing_map(ctx));
    let inner = inner?;
    hs_set_backing_map(ctx, set, inner);
    Ok((set, inner))
}

/// S111r28: Allocate the backing HashMap with enough field slots to cover
/// every inherited / declared instance field of the real JDK
/// `java.util.HashMap` class. The previous `try_alloc_synthetic("HashMap", 3)?`
/// produced a 3-slot object whose class metadata still claims 8 fields, so
/// the descriptor-aware `set_field` coercion path mangled writes to the
/// inherited `AbstractMap.values: Collection` field at slot 1 (where our
/// `MAP_FIELD_SIZE` index lives) — `Int(0)` got rewritten as
/// `Object(None)`, which broke the size accounting downstream.
pub(crate) fn alloc_backing_map(ctx: &mut dyn NativeContext) -> ObjectRef {
    let cid = match ctx.ensure_class_initialized("java/util/HashMap") {
        Ok(class_id) => class_id,
        Err(_) => ctx
            .class_id_by_name("java/util/HashMap")
            .unwrap_or(ClassId::new(0)),
    };
    let total = ctx.class_num_total_fields(cid);
    let n = std::cmp::max(total, MAP_NUM_FIELDS);
    let m = ctx.alloc_object(cid, n);
    // JVMS §2.3 defaults for the reference slots. `NativeContext::alloc_object`
    // zero-fills and stops there, and a zeroed cell decodes as `Int(0)`, not as
    // `null` — so without this every reference field of this map reads back as a
    // primitive. `table` is the one a caller then reads on EVERY map operation
    // until the first insert publishes an array, and it was the whole of the
    // 2,740-hit descriptor-coercion census row. See
    // [`init_native_map_reference_defaults`].
    init_native_map_reference_defaults(ctx, m);
    m
}

/// `true` for Set classes whose iteration must preserve *insertion* order
/// (`LinkedHashSet`, `CopyOnWriteArraySet`). These share the HashSet native
/// surface but must be backed by a `LinkedHashMap` so the shared iterator
/// (which walks the backing map) yields insertion order, not bucket order.
///
/// Uses a *subclass* check rather than an exact class-name match: real apps
/// subclass `LinkedHashSet` (e.g. Spring's `ManagedSet`, which the bean-factory
/// collection-merge path iterates by insertion order), and those instances
/// dispatch through this same HashSet native surface. An exact-name match would
/// hand a `ManagedSet` a plain `HashMap` backing, so `merge()`/`addAll()` would
/// iterate in bucket order and break Spring's order-sensitive merge contract.
pub(crate) fn hs_is_insertion_ordered(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    obj_is_instance_of(ctx, this, "java/util/LinkedHashSet")
        || obj_is_instance_of(ctx, this, "java/util/concurrent/CopyOnWriteArraySet")
}

/// Allocate + initialize a fresh backing map for a Set: a `LinkedHashMap`
/// (insertion-ordered) for `LinkedHashSet`/`CopyOnWriteArraySet`, else a
/// `HashMap`.
pub(crate) fn alloc_hs_backing(ctx: &mut dyn NativeContext, this: ObjectRef, cap: usize) -> ObjectRef {
    if hs_is_insertion_ordered(ctx, this) {
        let cid = ctx
            .ensure_class_initialized("java/util/LinkedHashMap")
            .unwrap_or_else(|_| {
                ctx.class_id_by_name("java/util/LinkedHashMap")
                    .unwrap_or(ClassId::new(0))
            });
        let total = ctx.class_num_total_fields(cid);
        let n = std::cmp::max(total, MAP_NUM_FIELDS);
        let m = ctx.alloc_object(cid, n);
        // JVMS §2.3 defaults, before anything can read them — see
        // [`init_native_map_reference_defaults`]. This is the insertion-ordered
        // twin of the `alloc_backing_map` arm below and has the same defect:
        // `lhm_init_with_cap_lazy` deliberately leaves `table` unallocated, so
        // without an explicit `null` the slot reads back `Int(0)` for the whole
        // life of an empty `LinkedHashSet`. Not a GC point, so it sits outside
        // the pin below.
        init_native_map_reference_defaults(ctx, m);
        // The freshly allocated backing map is referenced ONLY by this local
        // until the caller stores it into the set's `map` field. Pin it across
        // the initializer (which allocates the bucket table): a moving cycle
        // would relocate it, and a non-moving sweep cannot even see it as live,
        // so the returned reference could name reclaimed-and-reused memory.
        let m_pin = ctx.pin_native_root(m);
        // LAZY: `new HashSet<>()` on HotSpot is `new HashMap<>()`, whose table
        // is null until the first insert -- MEASURED, `EMPTYHS`. Every caller of
        // this function is a Java `HashSet`/`LinkedHashSet` constructor.
        lhm_init_with_cap_lazy(ctx, m, cap.max(MAP_DEFAULT_CAPACITY));
        let m = ctx.read_native_pin(m_pin, m);
        ctx.unpin_native_roots(m_pin);
        m
    } else {
        let m = alloc_backing_map(ctx);
        let m_pin = ctx.pin_native_root(m);
        // EAGER on purpose: this map is this crate's own backing store, and
        // `native_map_entry_set`'s builder does `map_state(..).0.unwrap()` on
        // it. See `native_map_init` for why the two variants exist.
        let _ = native_map_init_capacity(ctx, &[Value::Object(Some(m)), Value::Int(cap as i32)]);
        let m = ctx.read_native_pin(m_pin, m);
        ctx.unpin_native_roots(m_pin);
        m
    }
}

pub(crate) fn native_hs_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A real `CopyOnWriteArraySet` is served by its own bytecode over the
    // `CopyOnWriteArrayList` in `al`, not by this file's map surface.
    // See [`cow_set_route`].
    if let Some(r) = cow_set_route(ctx, args, "<init>", "()V") {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // `alloc_hs_backing` allocates and can therefore move `this`; the field
    // store below would otherwise land on a from-space address.
    let this_pin = ctx.pin_native_root(this);
    let backing = alloc_hs_backing(ctx, this, MAP_DEFAULT_CAPACITY);
    let this = ctx.read_native_pin(this_pin, this);
    hs_set_backing_map(ctx, this, backing);
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub(crate) fn native_hs_init_capacity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    map_ctor_capacity_load_check(args, MapCtorRefusalSpelling::HashMapFamily)?;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let cap = match args.get(1) {
        Some(Value::Int(c)) if *c > 0 => *c as usize,
        _ => MAP_DEFAULT_CAPACITY,
    };
    // Same allocation-moves-`this` hazard as `native_hs_init`.
    let this_pin = ctx.pin_native_root(this);
    let backing = alloc_hs_backing(ctx, this, cap);
    let this = ctx.read_native_pin(this_pin, this);
    hs_set_backing_map(ctx, this, backing);
    ctx.unpin_native_roots(this_pin);
    // `HashSet(int[, float])` is `map = new HashMap<>(initialCapacity[,
    // loadFactor])` (`CRATONVM_COMPAT_HASHSET_CTOR_JDK`): the backing takes the
    // constructor's own arguments, so `new HashSet<>(0)` starts from
    // `tableSizeFor(0)` = 1 bucket (the `cap` above floors a 0 at 16) and a
    // custom load factor is kept, as `HashMap(int, float)` keeps it. The
    // insertion-ordered sets (`LinkedHashSet`, iteration by list) keep theirs.
    // No allocation above since `backing` was read, and the lazy constructor
    // below allocates nothing.
    if hashset_ctor_jdk_enabled() && !hs_is_insertion_ordered(&*ctx, this) {
        if let Some(requested @ Value::Int(_)) = args.get(1) {
            let mut init = vec![Value::Object(Some(backing)), *requested];
            if let Some(load @ Value::Float(_)) = args.get(2) {
                init.push(*load);
            }
            native_map_init_capacity(ctx, &init)?;
        }
    }
    // `LinkedHashSet(int[, float])` is `new LinkedHashMap<>(initialCapacity,
    // loadFactor)`: its first table is `tableSizeFor(initialCapacity)`, which
    // the lazy backing reads back from `threshold`
    // (`CRATONVM_COMPAT_LHM_CTOR_CAPACITY`, `lhm_pending_table_size`). The
    // `cap` above floored a small request at 16 and kept a large one
    // unrounded. Still no allocation since `backing` was read.
    if lhm_ctor_capacity_enabled() && hs_is_insertion_ordered(&*ctx, this) {
        if let Some(Value::Int(requested)) = args.get(1) {
            let first_table = lhm_jdk_table_size_for(*requested);
            try_set_jdk_map_field(ctx, backing, "threshold", Value::Int(first_table));
        }
    }
    // `CRATONVM_COMPAT_LHM_LOAD_FACTOR`: and `new LinkedHashMap<>(n, lf)`
    // keeps `lf`, as the map constructor now does.
    if lhm_load_factor_enabled() && hs_is_insertion_ordered(&*ctx, this) {
        if let Some(Value::Float(lf)) = args.get(2) {
            try_set_jdk_map_field(ctx, backing, "loadFactor", Value::Float(*lf));
        }
    }
    Ok(None)
}

/// `CRATONVM_COMPAT_HASHSET_CTOR_JDK` — default ON, `=0` restores the old
/// backing. `HashSet(int)` / `HashSet(int, float)` build their backing map as
/// `new HashMap<>(initialCapacity[, loadFactor])`: the native floored a
/// capacity of 0 at 16 buckets (HotSpot: 1, grown 2, 4, ...; `new
/// HashSet<>(0)` of `{1, 4}` iterates `[4, 1]` there) and dropped the load
/// factor (`new HashSet<>(16, 0.5f)` kept 16 buckets past 8 elements). Round
/// 13 wave 9, lane hashcompat4 (comparison-order page, wave-6 item 2).
pub(crate) fn hashset_ctor_jdk_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_HASHSET_CTOR_JDK")
}

pub(crate) fn native_hs_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A real `CopyOnWriteArraySet` is served by its own bytecode over the
    // `CopyOnWriteArrayList` in `al`, not by this file's map surface.
    // See [`cow_set_route`].
    if let Some(r) = cow_set_route(ctx, args, "size", "()I") {
        return r;
    }
    if let Some(r) = ksv_route(ctx, args, native_ksv_size) {
        return r;
    }
    let mut this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    if let Some(b) = unmod_receiver_backing(ctx, this) {
        return native_hs_size(ctx, &[Value::Object(Some(b))]);
    }
    resync_view_set(ctx, &mut this)?;
    let backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => {
            if let Some(r) = try_delegate_real_collection(ctx, this, "java/util/Set", "size", "()I")
            {
                return r;
            }
            return Ok(Some(Value::Int(0)));
        }
    };
    let map_args = [Value::Object(Some(backing))];
    native_map_size(ctx, &map_args)
}

pub(crate) fn native_hs_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A real `CopyOnWriteArraySet` is served by its own bytecode over the
    // `CopyOnWriteArrayList` in `al`, not by this file's map surface.
    // See [`cow_set_route`].
    if let Some(r) = cow_set_route(ctx, args, "isEmpty", "()Z") {
        return r;
    }
    if let Some(r) = ksv_route(ctx, args, native_ksv_is_empty) {
        return r;
    }
    let mut this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(1))),
    };
    resync_view_set(ctx, &mut this)?;
    let backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => {
            if let Some(r) =
                try_delegate_real_collection(ctx, this, "java/util/Set", "isEmpty", "()Z")
            {
                return r;
            }
            return Ok(Some(Value::Int(1)));
        }
    };
    let map_args = [Value::Object(Some(backing))];
    native_map_is_empty(ctx, &map_args)
}

pub(crate) fn native_hs_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A real `CopyOnWriteArraySet` is served by its own bytecode over the
    // `CopyOnWriteArrayList` in `al`, not by this file's map surface.
    // See [`cow_set_route`].
    if let Some(r) = cow_set_route(ctx, args, "add", "(Ljava/lang/Object;)Z") {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let mut backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => return Ok(Some(Value::Int(0))),
    };
    // A keySet()/entrySet() view is not addable. `Map.keySet`: "The set supports
    // element removal ... It does not support the `add` or `addAll`
    // operations"; the JDK's `HashMap$KeySet extends AbstractSet`, and
    // `AbstractCollection.add` is a bare `throw new
    // UnsupportedOperationException()`.
    //
    // `keySet.addUnsupported` measured `m.keySet().add("nope")` returning
    // normally here. That is worse than it looks: the key landed in the VIEW's
    // backing and not in `m`, so the caller was left holding a "view" that
    // disagrees with the map it is a view of — the same silent-divergence shape
    // the write-through rows are about, arrived at from the other side.
    //
    // The predicate is the view marker itself, not a class-name test: an
    // ordinary `HashSet`'s backing is a real `java/util/HashMap` with fewer
    // than `VIEW_BACKING_FIELDS` slots, so `view_backing_source` answers `None`
    // for it by construction. Construction of the view is unaffected —
    // `make_view_set_of` populates the backing through `native_map_put` and
    // never comes through here.
    if let Some(view_source) = view_backing_source(ctx, backing) {
        // ONE VIEW IS ADDABLE, and it is the exception the JDK documents.
        // `ConcurrentHashMap$EntrySetView.add(Entry)` is supported -- it does
        // `map.putVal(e.getKey(), e.getValue(), false)` -- unlike every other
        // map's entrySet, which inherits `AbstractCollection.add`'s bare throw.
        // Refusing it here was the OPPOSITE-POLARITY twin of the
        // `TreeMap$KeySet.add` defect fixed the day before: there a view
        // accepted what the JDK refuses, here a view refused what the JDK
        // accepts. MEASURED against HotSpot 25.0.3+9,
        // `probes/ViewFamilySweep.java`, both modes -- the other four entrySet
        // carriers matched, and only this one differed.
        let recv_class = ctx
            .class_name_of_id(ctx.class_id_of_object(this))
            .unwrap_or_default();
        if recv_class == "java/util/concurrent/ConcurrentHashMap$EntrySetView" {
            let entry = match elem {
                Value::Object(Some(e)) => e,
                // `add(null)` on CHM's entrySet dereferences the entry, so it
                // is an NPE rather than a silent false.
                _ => {
                    return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                        message: None,
                    }
                    .into())
                }
            };
            // Pin across each invoke: `getKey`/`getValue` are Java calls and a
            // moving young generation would relocate the entry and the backing
            // behind us. Same discipline as `map_entries_of`.
            let backing_pin = ctx.pin_native_root(view_source);
            let entry_pin = ctx.pin_native_root(entry);
            let entry_c = ctx.read_native_pin(entry_pin, entry);
            let key = ctx
                .invoke_virtual(entry_c, "getKey", "()Ljava/lang/Object;", &[])?
                .unwrap_or(Value::Object(None));
            let key_pin = pin_value(ctx, key);
            let entry_c = ctx.read_native_pin(entry_pin, entry);
            let value = ctx
                .invoke_virtual(entry_c, "getValue", "()Ljava/lang/Object;", &[])?
                .unwrap_or(Value::Object(None));
            let value_pin = pin_value(ctx, value);
            let key = read_pinned_elem(ctx, key_pin, key);
            let value = read_pinned_elem(ctx, value_pin, value);
            // Put into the SOURCE map, not the view's own backing. Putting
            // into `backing` is exactly what the refusal comment above warns
            // about -- "the key landed in the VIEW's backing and not in `m`, so
            // the caller was left holding a view that disagrees with the map it
            // is a view of". MEASURED: the first version of this fix did that,
            // so `add` reported success and the entry never reached the map
            // (`entrySet is LIVE after put` stayed 4 against HotSpot's 5).
            let src = ctx.read_native_pin(backing_pin, view_source);
            let prev = native_map_put(ctx, &[Value::Object(Some(src)), key, value])?;
            ctx.unpin_native_roots(backing_pin);
            // `Set.add` reports whether the set CHANGED. CHM's own body returns
            // `putVal(..) == null`, i.e. true when there was no previous
            // mapping.
            let changed = matches!(prev, None | Some(Value::Object(None)));
            return Ok(Some(Value::Int(i32::from(changed))));
        }
        return Err(
            cratonvm_types::error::RuntimeError::UnsupportedOperationException {
                // HotSpot's is message-less; `thrownDetail` prints a message
                // when there is one.
                message: String::new(),
            }
            .into(),
        );
    }
    // put(element, PRESENT) — the previous value being null is how this
    // reports "the element was not already in the set". See `present_marker`
    // for why the marker must be a reference, and [`hs_present_marker_at`] for
    // why on a HashSet-family receiver it must be the REAL
    // `java.util.HashSet.PRESENT`: an element added here and removed by real
    // `HashSet.remove` bytecode is decided by `map.remove(o) == PRESENT`, an
    // identity test that the element-as-its-own-marker fallback fails.
    //
    // A NULL element has no reference to mark itself with, so its membership
    // cannot be read out of the value slot at all. Settle that one case with
    // `containsKey`, which distinguishes "absent" from "present" without
    // consulting the value.
    let elem_is_null = matches!(elem, Value::Object(None));
    let had_null = if elem_is_null {
        // `native_map_contains_key` can allocate (`materialize_hm_int_fast`,
        // the bucket-chain walk) even for a null search key, and any
        // allocation can complete a moving GC. Pin `backing` across the call
        // and re-read it before using it again -- the same discipline
        // `native_map_contains_key` applies to its own `this` internally
        // (see its "GC-safety" comment), which this caller must also honour
        // for the copy of the reference it holds.
        let backing_pin = ctx.pin_native_root(backing);
        let found = matches!(
            native_map_contains_key(ctx, &[Value::Object(Some(backing)), elem])?,
            Some(Value::Int(1))
        );
        backing = ctx.read_native_pin(backing_pin, backing);
        ctx.unpin_native_roots(backing_pin);
        found
    } else {
        false
    };
    let present = hs_present_marker(&*ctx, elem);
    let put_args = [Value::Object(Some(backing)), elem, present];
    let old = native_map_put(ctx, &put_args)?;
    let was_new = if elem_is_null {
        !had_null
    } else {
        matches!(old, Some(Value::Object(None)))
    };
    Ok(Some(Value::Int(if was_new { 1 } else { 0 })))
}

/// `HashSet.remove(Object)` against this VM's native backing map, for callers
/// that registered their own override on a `HashSet` subclass and need the
/// ordinary behaviour as a fallback.
///
/// Returns `None` when `this` has no native backing map, so the caller can
/// still fall through to real bytecode. Falling through UNCONDITIONALLY is not
/// safe: real `HashSet.remove` is `return map.remove(o) == PRESENT;`, an
/// IDENTITY comparison against the one `java.util.HashSet.PRESENT` instance.
/// [`hs_present_marker_at`]/`11798a8a2` (`H9-1`, 2026-08-20) made every
/// HashSet-family producer in this file write the real `PRESENT` rather than a
/// synthetic `Int(1)` sentinel or the element itself, so this caution is no
/// longer about every caller — it is about whichever ones still are not
/// covered by that fix (see `hs_present_marker_at`'s own doc for the two
/// deliberately-excluded view-carrier sites).
pub fn try_native_hashset_remove(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Option<MethodCallResult> {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    hs_backing_map(ctx, this)?;
    Some(native_hs_remove(ctx, args))
}

pub(crate) fn native_hs_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A real `CopyOnWriteArraySet` is served by its own bytecode over the
    // `CopyOnWriteArrayList` in `al`, not by this file's map surface.
    // See [`cow_set_route`].
    if let Some(r) = cow_set_route(ctx, args, "remove", "(Ljava/lang/Object;)Z") {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => return Ok(Some(Value::Int(0))),
    };
    // Live entrySet view: `remove(o)` follows the JDK `EntrySetView.remove`
    // contract — `map.remove(e.getKey(), e.getValue())`. The boolean result must
    // reflect whether the SOURCE map held that exact `(k, v)` mapping, not
    // whether the detached view snapshot did (the snapshot buckets entries by
    // identity hash and may have been rebuilt by a resync, so a snapshot-based
    // answer was unreliable — it returned `false` even when the write-through
    // removed the mapping, breaking callers that gate on the return such as
    // Narayana `ReaperElementManager.flushPending`). Reads of this same view
    // resync from the (now-updated) source, so we don't also mutate the snapshot.
    if let Some(source) = view_backing_source(ctx, backing) {
        if is_entryset_kind(view_backing_kind(ctx, backing)) {
            let (key, want_val) = match elem {
                Value::Object(Some(e)) => {
                    // Through `entry_slots`, not slots 0/1: a live entry is now the source
                    // family's OWN class, and only `TreeMap$Entry` and `CHM$MapEntry`
                    // put key/value there. `HashMap$Node`, `LinkedHashMap$Entry` and
                    // `Hashtable$Entry` all lead with `hash`, so a hard-coded 0 reads
                    // the HASH as the key and the removal silently matches nothing.
                    // MEASURED: this is what made `entrySet().iterator().remove()`
                    // report success while leaving the map at size 2.
                    let (k, v, _) = entry_slots(&*ctx, e);
                    (ctx.get_field(e, k), ctx.get_field(e, v))
                }
                // A non-Map.Entry argument is never contained in an entrySet.
                _ => return Ok(Some(Value::Int(0))),
            };
            // `CRATONVM_COMPAT_VIEW_REMOVE_BY_NODE`: `HashMap.EntrySet.remove`
            // is `removeNode(hash(key), key, value, true, true) != null` --
            // ONE `hashCode()`, one walk, and `value.equals(v)` with the
            // argument's value as the receiver: the single-walk `remove(k, v)`
            // body. No Java has run in this native yet.
            if view_remove_by_node_enabled() && plain_node_map(&*ctx, source) {
                return native_map_remove_kv(ctx, &[Value::Object(Some(source)), key, want_val]);
            }
            // JDK `EntrySetView.remove(o)` == `map.remove(e.getKey(),
            // e.getValue())`: remove (and report true) iff the SOURCE currently
            // maps `key` to a value equal to `want_val`. We evaluate that with
            // `containsKey` + `get` + value-equality rather than dispatching the
            // 2-arg `Map.remove(k,v)`, because for a natively-backed
            // LinkedHashMap/TreeMap the 2-arg form resolves to the JDK
            // `Map.remove` DEFAULT method whose real bytecode (`removeNode` ->
            // `afterNodeRemoval`) walks linkage the native backing never
            // populated and throws. The write-through itself is the native 1-arg
            // `remove(Object)` (`source_map_remove`), exactly as the keySet path
            // and the pre-fix entrySet path used. This mirrors `native_hs_contains`.
            // Family-1 fix (cce0079): `containsKey`/`get`/`map_keys_equal`
            // below all dispatch Java (GC-capable) — pin `source`/`key`/
            // `want_val`, refresh after every dispatch, and hand
            // `source_map_remove` current addresses.
            let src_pin = ctx.pin_native_root(source);
            let kh = pin_value(ctx, key);
            let wh = pin_value(ctx, want_val);
            let mut source = source;
            let mut key = key;
            let mut want_val = want_val;
            let has_key =
                match ctx.invoke_virtual(source, "containsKey", "(Ljava/lang/Object;)Z", &[key]) {
                    Ok(v) => matches!(v, Some(Value::Int(1))),
                    Err(e) => {
                        ctx.unpin_native_roots(src_pin);
                        return Err(e);
                    }
                };
            source = ctx.read_native_pin(src_pin, source);
            key = read_pinned_elem(ctx, kh, key);
            want_val = read_pinned_elem(ctx, wh, want_val);
            if !has_key {
                ctx.unpin_native_roots(src_pin);
                return Ok(Some(Value::Int(0)));
            }
            let got = match ctx.invoke_virtual(
                source,
                "get",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[key],
            ) {
                Ok(v) => v.unwrap_or(Value::Object(None)),
                Err(e) => {
                    ctx.unpin_native_roots(src_pin);
                    return Err(e);
                }
            };
            source = ctx.read_native_pin(src_pin, source);
            key = read_pinned_elem(ctx, kh, key);
            want_val = read_pinned_elem(ctx, wh, want_val);
            let eq = values_equal(ctx, &got, &want_val)
                || match (got, want_val) {
                    (Value::Object(Some(a)), Value::Object(Some(b))) => {
                        match map_keys_equal(ctx, a, b) {
                            Ok(r) => r,
                            Err(e) => {
                                ctx.unpin_native_roots(src_pin);
                                return Err(e);
                            }
                        }
                    }
                    _ => false,
                };
            source = ctx.read_native_pin(src_pin, source);
            key = read_pinned_elem(ctx, kh, key);
            if eq {
                let r = source_map_remove(ctx, source, key);
                ctx.unpin_native_roots(src_pin);
                r?;
                return Ok(Some(Value::Int(1)));
            }
            ctx.unpin_native_roots(src_pin);
            return Ok(Some(Value::Int(0)));
        }
        // `CRATONVM_COMPAT_VIEW_REMOVE_BY_NODE`: `HashMap.KeySet.remove` is
        // `removeNode(hash(key), key, null, false, true) != null` -- ONE
        // `hashCode()` on the SOURCE. The snapshot is left alone: the removal
        // moves the source's `modCount`, and the next read of the view
        // resyncs from it. Whether a node went is read off the source's size,
        // which a key mapped to null does not confuse. No Java has run yet.
        if view_remove_by_node_enabled() && plain_node_map(&*ctx, source) {
            let src_pin = ctx.pin_native_root(source);
            let before = node_map_size(&*ctx, source);
            let r = native_map_remove(ctx, &[Value::Object(Some(source)), elem]);
            let source = ctx.read_native_pin(src_pin, source);
            let after = node_map_size(&*ctx, source);
            ctx.unpin_native_roots(src_pin);
            r?;
            return Ok(Some(Value::Int(i32::from(after < before))));
        }
        // keySet view: remove the key from both the snapshot (for the result)
        // and the source map (write-through).
        //
        // Family-1 fix (cce0079): `native_map_remove` dispatches
        // hashCode()/equals() — refresh `source`/`elem` through pins before
        // the write-through.
        let src_pin = ctx.pin_native_root(source);
        let eh = pin_value(ctx, elem);
        let remove_args = [Value::Object(Some(backing)), elem];
        let old = match native_map_remove(ctx, &remove_args) {
            Ok(o) => o,
            Err(e) => {
                ctx.unpin_native_roots(src_pin);
                return Err(e);
            }
        };
        let source = ctx.read_native_pin(src_pin, source);
        let elem = read_pinned_elem(ctx, eh, elem);
        ctx.unpin_native_roots(src_pin);
        let was_present = !matches!(old, Some(Value::Object(None)));
        source_map_remove(ctx, source, elem)?;
        return Ok(Some(Value::Int(if was_present { 1 } else { 0 })));
    }
    // Ordinary (non-view) HashSet: remove from the backing only.
    //
    // `ThreadPoolExecutor.processWorkerExit` removes its Worker from this kind
    // of HashSet while a collection can occur in the nested HashMap native.
    // The map implementation roots its own entry arguments, but the outer
    // HashSet frame still supplied those raw addresses at the nesting boundary.
    // Retain both values here as well and refresh them immediately before the
    // nested call, so neither layer can hand a from-space reference to the
    // other.
    let backing_pin = ctx.pin_native_root(backing);
    let elem_pin = pin_value(ctx, elem);
    let backing = ctx.read_native_pin(backing_pin, backing);
    let elem = read_pinned_elem(ctx, elem_pin, elem);
    // A null element carries no PRESENT marker of its own (see
    // `present_marker`), so the removed value cannot say whether it was there.
    // Ask `containsKey` first — it does not consult the value. `containsKey`
    // dispatches Java and can move things, so re-read through the pins after.
    let had_null = if matches!(elem, Value::Object(None)) {
        match native_map_contains_key(ctx, &[Value::Object(Some(backing)), elem]) {
            Ok(v) => Some(matches!(v, Some(Value::Int(1)))),
            Err(e) => {
                ctx.unpin_native_roots(backing_pin);
                return Err(e);
            }
        }
    } else {
        None
    };
    let backing = ctx.read_native_pin(backing_pin, backing);
    let elem = read_pinned_elem(ctx, elem_pin, elem);
    let remove_args = [Value::Object(Some(backing)), elem];
    let old = native_map_remove(ctx, &remove_args);
    ctx.unpin_native_roots(backing_pin);
    let old = old?;
    Ok(Some(Value::Int(
        if had_null.unwrap_or(!matches!(old, Some(Value::Object(None)))) {
            1
        } else {
            0
        },
    )))
}

pub(crate) fn native_hs_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A real `CopyOnWriteArraySet` is served by its own bytecode over the
    // `CopyOnWriteArrayList` in `al`, not by this file's map surface.
    // See [`cow_set_route`].
    if let Some(r) = cow_set_route(ctx, args, "contains", "(Ljava/lang/Object;)Z") {
        return r;
    }
    let mut this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    // gen r4w4/rooting2: `resync_view_set` can re-put the view's contents
    // (Java `hashCode()`/`equals()`, node allocations), so the element is read
    // and pinned BEFORE it; reading it from `args` afterwards and pinning that
    // preserved a pre-move address.
    let elem0 = args.get(1).copied().unwrap_or(Value::Object(None));
    let resync_pin = pin_value(ctx, elem0);
    let resynced = resync_view_set(ctx, &mut this);
    let elem = read_pinned_elem(ctx, resync_pin, elem0);
    if resync_pin != usize::MAX {
        ctx.unpin_native_roots(resync_pin);
    }
    resynced?;
    let backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => return Ok(Some(Value::Int(0))),
    };
    // GC-safety: the view branches below dispatch the SOURCE map's real
    // `containsKey`/`get` (arbitrary Java) and `map_keys_equal`, each of which
    // can complete a moving young GC. `backing`, `elem` and the entry's
    // key/value are bare Rust locals used after those calls. Root them for the
    // whole native and re-read at each use.
    let hs_pin = ctx.pin_native_root(backing);
    let elem_pin = pin_value(ctx, elem);
    let elem = read_pinned_elem(ctx, elem_pin, elem);
    let result = native_hs_contains_pinned(ctx, backing, hs_pin, elem, elem_pin);
    ctx.unpin_native_roots(hs_pin);
    result
}

pub(crate) fn native_hs_contains_pinned(
    ctx: &mut dyn NativeContext,
    backing: ObjectRef,
    hs_pin: usize,
    elem: Value,
    elem_pin: usize,
) -> MethodCallResult {
    // entrySet() view: `contains(e)` must follow `AbstractMap`'s contract —
    // `getNode(e.getKey()) != null && node.value.equals(e.getValue())` — i.e.
    // compare by Map.Entry equality against the SOURCE map. Looking the entry
    // object itself up in the view's HashSet backing (as the keySet path below
    // does) buckets it by the entry's identity hash, so an entry produced by a
    // different `entrySet()` call (or any foreign Map.Entry) never matches and
    // `contains` wrongly returns false.
    if let Some(source) = view_backing_source(ctx, backing) {
        if is_entryset_kind(view_backing_kind(ctx, backing)) {
            // Non-object / null arg is never a Map.Entry → not contained.
            let entry = match elem {
                Value::Object(Some(e)) => e,
                _ => return Ok(Some(Value::Int(0))),
            };
            // Synthetic Map.Entry layout: slot 0 = key, slot 1 = value (matches
            // the entrySet builders and `native_hs_remove`'s key extraction).
            // See the note in `native_hs_remove`: entry slots are per class.
            let (k_slot, v_slot, _) = entry_slots(&*ctx, entry);
            let key = ctx.get_field(entry, k_slot);
            let want_val = ctx.get_field(entry, v_slot);
            // Resolve via the source map's own `containsKey`/`get` so this works
            // for every backing map type (HashMap / LinkedHashMap / TreeMap).
            // `containsKey` distinguishes "absent" from "present with null value".
            // Both dispatches are arbitrary Java and can move every local here.
            let source_pin = ctx.pin_native_root(source);
            let key_pin = pin_value(ctx, key);
            let want_pin = pin_value(ctx, want_val);
            let has_key =
                ctx.invoke_virtual(source, "containsKey", "(Ljava/lang/Object;)Z", &[key])?;
            if !matches!(has_key, Some(Value::Int(1))) {
                return Ok(Some(Value::Int(0)));
            }
            let source = ctx.read_native_pin(source_pin, source);
            let key = read_pinned_elem(ctx, key_pin, key);
            let got = ctx
                .invoke_virtual(
                    source,
                    "get",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                    &[key],
                )?
                .unwrap_or(Value::Object(None));
            let got_pin = pin_value(ctx, got);
            let want_val = read_pinned_elem(ctx, want_pin, want_val);
            let eq = values_equal(ctx, &got, &want_val)
                || match (read_pinned_elem(ctx, got_pin, got), want_val) {
                    (Value::Object(Some(a)), Value::Object(Some(b))) => map_keys_equal(ctx, a, b)?,
                    _ => false,
                };
            return Ok(Some(Value::Int(if eq { 1 } else { 0 })));
        }
        // keySet() view: `contains(k)` must follow the SOURCE map's (possibly
        // overridden) `containsKey`, exactly like the entrySet branch above —
        // not a direct lookup in the native backing. Spring's
        // LinkedCaseInsensitiveMap backs its keySet on an inner LinkedHashMap
        // whose `containsKey` is overridden to be case-insensitive; a direct
        // native lookup bypassed that override so `keySet().contains("KEY")`
        // returned false (LinkedCaseInsensitiveMapTests putAndGet /
        // putWithOverlappingKeys).
        if !is_entryset_kind(view_backing_kind(ctx, backing)) {
            let elem = read_pinned_elem(ctx, elem_pin, elem);
            let has =
                ctx.invoke_virtual(source, "containsKey", "(Ljava/lang/Object;)Z", &[elem])?;
            return Ok(Some(Value::Int(if matches!(has, Some(Value::Int(1))) {
                1
            } else {
                0
            })));
        }
    }
    let backing = ctx.read_native_pin(hs_pin, backing);
    let elem = read_pinned_elem(ctx, elem_pin, elem);
    let ck_args = [Value::Object(Some(backing)), elem];
    native_map_contains_key(ctx, &ck_args)
}

pub(crate) fn native_hs_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A real `CopyOnWriteArraySet` is served by its own bytecode over the
    // `CopyOnWriteArrayList` in `al`, not by this file's map surface.
    // See [`cow_set_route`].
    if let Some(r) = cow_set_route(ctx, args, "clear", "()V") {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => return Ok(None),
    };
    // Live keySet/entrySet view: clearing the view clears the source map.
    // GC-safety: that `clear()` is arbitrary Java and can complete a moving
    // young GC, after which both `this` and `backing` are pre-move addresses.
    let this_pin = ctx.pin_native_root(this);
    let backing_h = ctx.pin_native_root(backing);
    let cleared = match view_backing_source(ctx, backing) {
        Some(source) => ctx.invoke_virtual(source, "clear", "()V", &[]).map(|_| ()),
        None => Ok(()),
    };
    let this = ctx.read_native_pin(this_pin, this);
    let backing = ctx.read_native_pin(backing_h, backing);
    ctx.unpin_native_roots(this_pin);
    cleared?;
    let clear_args = [Value::Object(Some(backing))];
    // LinkedHashSet / CopyOnWriteArraySet back onto a LinkedHashMap, whose state
    // (size/head/tail + bucket table) lives in the LHM overlay, NOT the plain
    // HashMap-layout slots that `native_map_clear` resets. Calling
    // `native_map_clear` on an LHM backing zeroed the bucket table + size field
    // but left the LinkedHashMap `head`/`tail` insertion chain intact, so the
    // set kept iterating the pre-clear entries and re-adding a previously
    // present element DUPLICATED it. JUnit Jupiter reorders a class descriptor's
    // method children via clear()+re-add, so this produced two identical method
    // descriptors → discovery aborted with PreconditionViolationException
    // "returned a cyclic graph", blocking discovery of EVERY Hibernate test
    // class. Route to `native_lhm_clear` for insertion-ordered sets (the set's
    // own type is authoritative — `alloc_hs_backing` always pairs them with a
    // LinkedHashMap; the backing class-name probe alone can misfire) or when the
    // backing resolves as a LinkedHashMap.
    let backing_is_lhm = hs_is_insertion_ordered(ctx, this)
        || matches!(
            ctx.class_name_arc_of_id(ctx.class_id_of_object(backing))
                .as_deref(),
            Some("java/util/LinkedHashMap")
        );
    if backing_is_lhm {
        native_lhm_clear(ctx, &clear_args)
    } else {
        native_map_clear(ctx, &clear_args)
    }
}

/// `java.util.Hashtable$Enumerator`, java.base's OWN cursor, which is both an
/// `Enumeration` and an `Iterator` over one position.
pub(crate) const HT_ENUMERATOR: &str = "java/util/Hashtable$Enumerator";
/// `Hashtable$Enumerator(Hashtable, int type, boolean iterator)`.
pub(crate) const HT_ENUMERATOR_CTOR: &str = "(Ljava/util/Hashtable;IZ)V";
/// `Hashtable.KEYS` / `.VALUES` / `.ENTRIES`, the `type` argument above.
pub(crate) const HT_ENUM_KEYS: i32 = 0;
pub(crate) const HT_ENUM_VALUES: i32 = 1;
pub(crate) const HT_ENUM_ENTRIES: i32 = 2;

/// A real `Hashtable$Enumerator` over the Hashtable behind a `Hashtable$KeySet`
/// / `Hashtable$EntrySet` view, or `None` when this receiver is not one of
/// those (or the image cannot build the class).
///
/// # Why this exists
///
/// `H4-1` section 3 predicts that refusing the four `MAP_KEY_ITR_CARRIERS`
/// while `Hashtable$KeySet` is still `Bridge` makes the Hashtable carrier mint
/// a `HashMap$KeyIterator` whose natives are gone — a silently EMPTY iteration.
/// That reasoning is right and it UNDERSTATES the problem. MEASURED, unarmed,
/// `--jdk-only`, two-entry map:
///
/// ```text
///                             HotSpot 25.0.3+9       CratonVM before
///   ht.keySet().iterator()    Hashtable$Enumerator   HashMap$KeyIterator
///   ht.entrySet().iterator()  Hashtable$Enumerator   HashMap$EntryIterator
///   ht.keys()                 Hashtable$Enumerator   Hashtable$Enumerator  <- already right
/// ```
///
/// The wrong class is handed out TODAY, and this map's own `keys()` was already
/// correct — so one `Hashtable` answered two different iterator classes
/// depending on which door you came through, the same two-answers-one-object
/// shape as the enumeration ORDER bug three commits back.
///
/// # Why a REAL cursor and not a snapshot under the real name
///
/// The first attempt registered the snapshot natives onto
/// `Hashtable$Enumerator` and named it in both force-native gates. That
/// **reddened `RJdkEnumerations`**, and the mechanism is this file's own
/// two-producers-one-slot lesson (`WORKER-2-NOTE-1` section 4a) in a new place:
/// `Hashtable.keys()` ALREADY produces a real JDK-built `Hashtable$Enumerator`
/// (`deprecated_util::real_hashtable_enumerator`), so a gate entry keyed on the
/// CLASS made the snapshot natives intercept that producer's objects too —
/// objects whose `key_itr_base` slots nothing ever wrote. Two producers, one
/// class, and a gate that cannot tell them apart.
///
/// Using java.base's own cursor for BOTH doors removes the second producer
/// instead of trying to disambiguate it: one class, one shape, no natives
/// registered on it, and **no force-native gate entries at all** — so this
/// stays inside `native-collections`.
///
/// It works for the same measured reason `real_hashtable_enumerator` works, and
/// this lane strengthened that reason: the receiver's `table` is now a real
/// `[Ljava.util.Hashtable$Entry;` holding real `Hashtable$Entry` nodes, so the
/// JDK's own `Enumerator` walks the authoritative store with its own bytecode
/// and its `Entry.key`/`.value`/`.next` accesses land exactly where it expects.
/// `remove()` and `count` are that same real state, so write-through is the
/// JDK's own code rather than anything this file has to mirror.
pub(crate) fn real_ht_view_enumerator(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    source: ObjectRef,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let enum_type = match ctx
        .class_name_arc_of_id(ctx.class_id_of_object(this))
        .as_deref()
    {
        Some("java/util/Hashtable$KeySet") => HT_ENUM_KEYS,
        Some("java/util/Hashtable$EntrySet") => HT_ENUM_ENTRIES,
        // HotSpot answers ONE class for all three of a Hashtable's views.
        // MEASURED: `ht.values().iterator()` is a `Hashtable$Enumerator` there
        // and was a `java.util.ArrayList$Itr` here, because the values carrier
        // is ArrayList-SHAPED and reaches `native_al_iterator` rather than
        // `native_hs_iterator`. Same cursor, different door.
        Some("java/util/Hashtable$ValueCollection") => HT_ENUM_VALUES,
        _ => return Ok(None),
    };
    // Never accept a stand-in: a fabricated `Hashtable$Enumerator` is exactly
    // the class-fabrication `--jdk-only` exists to stop, and its methods would
    // be unbound. Ask before a stub can be created and again after init.
    if ctx.would_fabricate_synthetic_stub(HT_ENUMERATOR)
        || ctx.is_class_synthetic_stub(HT_ENUMERATOR)
        || ctx.ensure_class_initialized(HT_ENUMERATOR).is_err()
        || ctx.is_class_synthetic_stub(HT_ENUMERATOR)
        || !ctx.method_exists(HT_ENUMERATOR, "<init>", HT_ENUMERATOR_CTOR)
    {
        return Ok(None);
    }
    // `source` must be the real `Hashtable` the view was minted over, NOT the
    // view: `VIEW_BACKING_SRC_SLOT` lives on the view's BACKING map, which is
    // why the caller resolves it and hands it in. Reading it off the view
    // itself is how the first cut of this route silently never fired.
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(source))
        .as_deref()
        != Some("java/util/Hashtable")
    {
        // `Properties` also reaches here (it extends `Hashtable`), and on
        // JDK 25 HotSpot answers a `ConcurrentHashMap$KeyIterator` for it
        // because `Properties` is CHM-backed. Handing it a
        // `Hashtable$Enumerator` would trade one wrong class for another, so
        // the exact class is required and `Properties` keeps what it had.
        return Ok(None);
    }
    // GC-SAFETY: `new_object_initialized` runs Java (`<clinit>` and the
    // constructor) and can move the heap, so `source` — a bare Rust local the
    // collector cannot see — is rooted across it and read back through the pin.
    let pin = ctx.pin_native_root(source);
    let built = ctx.new_object_initialized(
        HT_ENUMERATOR,
        HT_ENUMERATOR_CTOR,
        &[
            Value::Object(Some(ctx.read_native_pin(pin, source))),
            Value::Int(enum_type),
            // `iterator = true`: the Iterator face, whose `next()` performs the
            // modCount check `keySet().iterator()` is required to do. The
            // Enumeration face stays available on the same object — that is the
            // whole point of this class — which is why `keys()` and
            // `keySet().iterator()` can now be the same shape.
            Value::Int(1),
        ],
    );
    ctx.unpin_native_roots(pin);
    match built? {
        Some(Value::Object(Some(e))) => Ok(Some(e)),
        _ => Ok(None),
    }
}

/// `java.util.concurrent.CopyOnWriteArrayList$COWIterator`, the class the JDK's
/// `CopyOnWriteArrayList.iterator()` / `listIterator()` answer.
pub(crate) const COW_ITERATOR: &str = "java/util/concurrent/CopyOnWriteArrayList$COWIterator";
/// `COWIterator(Object[] es, int initialCursor)`.
pub(crate) const COW_ITERATOR_CTOR: &str = "([Ljava/lang/Object;I)V";

/// The JDK's own iterator for a `CopyOnWriteArrayList` whose `array` snapshot
/// is pinned at `snap_pin` (`snap` is its last-known address): what
/// `iterator()` returns, `new COWIterator<E>(getArray(), 0)`. `None` when the
/// class library has no real `COWIterator` (a synthetic stub would be the
/// class fabrication `--jdk-only` exists to stop, with unbound methods), so the
/// caller keeps its `ArrayList$Itr` wrapper. Same guards as
/// [`real_ht_view_enumerator`].
pub(crate) fn real_cowal_iterator(
    ctx: &mut dyn NativeContext,
    snap_pin: usize,
    snap: ObjectRef,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    if ctx.would_fabricate_synthetic_stub(COW_ITERATOR)
        || ctx.is_class_synthetic_stub(COW_ITERATOR)
        || ctx.ensure_class_initialized(COW_ITERATOR).is_err()
        || ctx.is_class_synthetic_stub(COW_ITERATOR)
        || !ctx.method_exists(COW_ITERATOR, "<init>", COW_ITERATOR_CTOR)
    {
        return Ok(None);
    }
    let snap = ctx.read_native_pin(snap_pin, snap);
    let built = ctx.new_object_initialized(
        COW_ITERATOR,
        COW_ITERATOR_CTOR,
        &[Value::Object(Some(snap)), Value::Int(0)],
    );
    match built? {
        Some(Value::Object(Some(itr))) => Ok(Some(itr)),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod i73_l1_cow_iterator_tests {
    use super::*;
    use cratonvm_native_api::test_mock::MockNativeContext;

    /// A class library with no real `COWIterator` constructor (the mock's
    /// `method_exists` answers false) mints nothing: the caller keeps its
    /// `ArrayList$Itr` wrapper rather than fabricating a stub iterator.
    #[test]
    fn no_real_cow_iterator_mints_nothing() {
        let mut mock = MockNativeContext::new();
        let snap = mock.fresh_object_ref();
        let ctx: &mut dyn NativeContext = &mut mock;
        let pin = ctx.pin_native_root(snap);
        let got = real_cowal_iterator(ctx, pin, snap).unwrap();
        assert!(got.is_none());
    }
}

/// Interpreter round i3 wave 74, lane L1 (`JdkCopyOnWriteArraySet`,
/// `CRATONVM_COMPAT_JDK_COPY_ON_WRITE_SET`): over a real JDK,
/// `register_hashset_natives` with the row registers none of its 24
/// `CopyOnWriteArraySet` rows (three retired, 21 reviewed `bridges` of the
/// family), and with the row off all 24 stay; the synthetic library keeps them
/// either way (the fabricated set's map surface is its implementation), and
/// `HashSet` / `LinkedHashSet` do not move with it. The row is set explicitly.
#[cfg(test)]
mod i74_l1_cow_set_row_tests {
    use super::*;
    use cratonvm_types::compat::CompatibilityMode;
    use cratonvm_types::{HotSpotAlignment as Row, HotSpotAlignments};

    const COWAS: &str = "java/util/concurrent/CopyOnWriteArraySet";

    fn rows_on(class: &str, real_jdk: bool, on: bool) -> usize {
        let mut r = NativeMethodRegistry::new();
        r.set_compatibility_mode(CompatibilityMode::Compatible);
        // From NONE, not the defaults: `JdkStreams` (default on) drops the
        // `stream()` row of every `SET_CLASSES` name by itself.
        r.set_hotspot_alignments(HotSpotAlignments::NONE.with(Row::JdkCopyOnWriteArraySet, on));
        r.set_real_jdk(real_jdk);
        register_hashset_natives(&mut r);
        r.dump_registrations()
            .into_iter()
            .filter(|(c, _, _, _)| *c == class)
            .count()
    }

    #[test]
    fn the_set_surface_follows_the_copy_on_write_set_row() {
        if cratonvm_native_api::unretire::is_excluded(COWAS, "isEmpty", "()Z") {
            return; // a retired row was un-retired in this environment
        }
        assert_eq!(rows_on(COWAS, true, false), 24);
        assert_eq!(rows_on(COWAS, true, true), 0);
        // The synthetic library: the row needs a real JDK.
        assert_eq!(rows_on(COWAS, false, true), 24);
        for c in ["java/util/HashSet", "java/util/LinkedHashSet"] {
            assert_eq!(rows_on(c, true, true), rows_on(c, true, false), "{c}");
        }
    }
}

/// Interpreter round i3 wave 75, lane L1 (`JdkPriorityBlockingQueue`,
/// `CRATONVM_COMPAT_JDK_PRIORITY_BLOCKING_QUEUE`): over a real JDK,
/// `register_priority_blocking_queue_natives` with the row registers none of
/// its nine `PriorityBlockingQueue` stand-ins (all retired rows), and with the
/// row off all nine stay; the synthetic library keeps them either way (the
/// row needs a real JDK). The row is set explicitly, from `NONE`.
#[cfg(test)]
mod i75_l1_pbq_row_tests {
    use super::*;
    use cratonvm_types::compat::CompatibilityMode;
    use cratonvm_types::{HotSpotAlignment as Row, HotSpotAlignments};

    const PBQ: &str = "java/util/concurrent/PriorityBlockingQueue";

    fn rows(real_jdk: bool, on: bool) -> usize {
        let mut r = NativeMethodRegistry::new();
        r.set_compatibility_mode(CompatibilityMode::Compatible);
        r.set_hotspot_alignments(HotSpotAlignments::NONE.with(Row::JdkPriorityBlockingQueue, on));
        r.set_real_jdk(real_jdk);
        register_priority_blocking_queue_natives(&mut r);
        r.dump_registrations()
            .into_iter()
            .filter(|(c, _, _, _)| *c == PBQ)
            .count()
    }

    #[test]
    fn the_queue_stand_ins_follow_the_priority_blocking_queue_row() {
        for (m, d) in [
            ("<init>", "()V"),
            ("offer", "(Ljava/lang/Object;)Z"),
            ("take", "()Ljava/lang/Object;"),
            ("size", "()I"),
        ] {
            if cratonvm_native_api::unretire::is_excluded(PBQ, m, d) {
                return; // a retired row was un-retired in this environment
            }
        }
        assert_eq!(rows(true, false), 9);
        assert_eq!(rows(true, true), 0);
        // The synthetic library: the row needs a real JDK.
        assert_eq!(rows(false, true), 9);
        assert_eq!(rows(false, false), 9);
    }
}

/// Interpreter round i3 wave 76, lane L1 (`JdkConcurrentHashMap`,
/// `CRATONVM_COMPAT_JDK_CONCURRENT_HASH_MAP`): over a real JDK under
/// `--compatible`, the CHM registrar and the two view-carrier registrars make
/// no registration on any `ConcurrentHashMap` class with the row, and keep the
/// five `ConcurrentMap` interface rows (as door wrappers); without the row, or
/// on the synthetic library, or under `--jdk-only`, the row changes nothing.
/// The row is set explicitly, from `NONE`.
#[cfg(test)]
mod i76_l1_chm_row_tests {
    use super::*;
    use cratonvm_types::compat::CompatibilityMode;
    use cratonvm_types::{HotSpotAlignment as Row, HotSpotAlignments};

    const CM: &str = "java/util/concurrent/ConcurrentMap";

    /// `(registrations on a CHM class, registrations on ConcurrentMap)`.
    fn counts(mode: CompatibilityMode, real_jdk: bool, on: bool) -> (usize, usize) {
        let mut r = NativeMethodRegistry::new();
        r.set_compatibility_mode(mode);
        r.set_hotspot_alignments(HotSpotAlignments::NONE.with(Row::JdkConcurrentHashMap, on));
        r.set_real_jdk(real_jdk);
        register_concurrent_hashmap_natives(&mut r);
        register_map_view_carrier_natives(&mut r);
        register_set_view_carrier_natives(&mut r);
        let regs = r.dump_registrations();
        let chm = regs
            .iter()
            .filter(|(c, _, _, _)| c.starts_with(CHM_CLASS))
            .count();
        let cm = regs.iter().filter(|(c, _, _, _)| *c == CM).count();
        (chm, cm)
    }

    #[test]
    fn the_concurrent_hash_map_cluster_follows_the_row() {
        let compat = CompatibilityMode::Compatible;
        let (off_chm, off_cm) = counts(compat, true, false);
        assert!(off_chm > 0, "no CHM registration without the row");
        assert_eq!(off_cm, 5);
        let (on_chm, on_cm) = counts(compat, true, true);
        assert_eq!(on_chm, 0, "a CHM registration survives the row");
        assert_eq!(on_cm, 5, "the ConcurrentMap interface rows stay");
        // The synthetic library: the row needs a real JDK.
        assert_eq!(counts(compat, false, true), counts(compat, false, false));
        // `--jdk-only`: its registrations are the retired-shadow tables'.
        assert_eq!(
            counts(CompatibilityMode::JdkOnly, true, true),
            counts(CompatibilityMode::JdkOnly, true, false)
        );
    }

    /// A `ConcurrentMap` door with the row in force but a receiver that is not a
    /// CHM (the mock classifies nothing as one) keeps the segmented native; the
    /// mock context answers `aligned` with `is_jdk_only` (false), so the door
    /// helper declines whatever the receiver.
    #[test]
    fn the_door_helper_declines_without_the_row() {
        let mut mock = cratonvm_native_api::test_mock::MockNativeContext::new();
        let this = mock.fresh_object_ref();
        let ctx: &mut dyn NativeContext = &mut mock;
        assert!(!chm_jdk_bodies(ctx));
        assert!(chm_door_runs_jdk_body(ctx, &[Value::Object(Some(this))], "size", "()I").is_none());
        assert!(chm_jdk_table_entries(ctx, this).is_empty());
    }
}

/// A snapshot iterator for a `CopyOnWriteArraySet` receiver, or `None` if this
/// receiver is not one.
///
/// The copy-on-write contract is that an iterator reflects the collection as it
/// stood when the iterator was created, never throws
/// `ConcurrentModificationException`, and refuses `remove()`. The set family
/// here is backed by a live `LinkedHashMap`, so the shared cursor gives the
/// opposite of all three.
///
/// Rather than re-derive the contract, this mints a real
/// `java/util/concurrent/CopyOnWriteArrayList` over the snapshot and returns
/// ITS iterator — the same structure the JDK uses (`CopyOnWriteArraySet` holds a
/// `CopyOnWriteArrayList` and returns `al.iterator()`), and the same code path
/// `probes/CowSnapshotProbe.java` rows L01-L04 already show to be correct on
/// this VM. If the delegate ever regresses, the set regresses with it, which is
/// the intended coupling.
///
/// Returns `None` — falling through to the live cursor — for any receiver that
/// is not a `CopyOnWriteArraySet`, and for the two shapes that would otherwise
/// be guesses: no resolvable COWAL `array` field, or an allocation that fails.
pub(crate) fn cow_set_snapshot_iterator(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    backing: ObjectRef,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let cows_cid = match ctx.class_id_by_name("java/util/concurrent/CopyOnWriteArraySet") {
        Some(id) => id,
        None => return Ok(None),
    };
    if !ctx.is_subclass(ctx.class_id_of_object(this), cows_cid) {
        return Ok(None);
    }
    // Read the elements BEFORE allocating anything: every allocation below can
    // collect. `collect_view_snapshot_ordered` on the resolved backing map is
    // what the rest of this Set surface uses (`toArray`); the generic
    // `collect_collection_elements` does NOT read a map-backed Set and returns
    // an empty vector, which reads downstream as "the set is empty" rather than
    // as a failure.
    let elems = hs_view_elements(ctx, this, backing)?;
    let (elem_pin_base, elem_handles) = pin_value_slice(ctx, &elems);
    let backing = alloc_ref_array(ctx, elems.len());
    let backing_pin = ctx.pin_native_root(backing);
    for (i, v) in elems.iter().enumerate() {
        let backing = ctx.read_native_pin(backing_pin, backing);
        let v = read_pinned_elem(ctx, elem_handles[i], *v);
        ctx.set_array_element(backing, i, v);
    }
    let backing = ctx.read_native_pin(backing_pin, backing);

    let delegate = match ctx.new_object("java/util/concurrent/CopyOnWriteArrayList")? {
        Some(Value::Object(Some(o))) => o,
        _ => {
            if elem_pin_base != usize::MAX {
                ctx.unpin_native_roots(elem_pin_base);
            } else {
                ctx.unpin_native_roots(backing_pin);
            }
            return Ok(None);
        }
    };
    let delegate_pin = ctx.pin_native_root(delegate);
    let backing = ctx.read_native_pin(backing_pin, backing);
    let arr_slot = ctx.resolve_field_index("java/util/concurrent/CopyOnWriteArrayList", "array");
    let itr = match arr_slot {
        Some(slot) => {
            let delegate = ctx.read_native_pin(delegate_pin, delegate);
            ctx.set_field(delegate, slot, Value::Object(Some(backing)));
            let delegate = ctx.read_native_pin(delegate_pin, delegate);
            match ctx.invoke_virtual(delegate, "iterator", "()Ljava/util/Iterator;", &[])? {
                Some(Value::Object(Some(it))) => Some(it),
                _ => None,
            }
        }
        // No `array` field to write means this VM is not modelling COWAL the
        // way the delegate path expects. Decline rather than hand back an
        // iterator over an empty list, which would read as "the set is empty".
        None => None,
    };
    let unpin = if elem_pin_base != usize::MAX {
        elem_pin_base.min(backing_pin).min(delegate_pin)
    } else {
        backing_pin.min(delegate_pin)
    };
    ctx.unpin_native_roots(unpin);
    Ok(itr)
}

pub(crate) fn native_hs_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A real `CopyOnWriteArraySet` is served by its own bytecode over the
    // `CopyOnWriteArrayList` in `al`, not by this file's map surface.
    // See [`cow_set_route`].
    if let Some(r) = cow_set_route(ctx, args, "iterator", "()Ljava/util/Iterator;") {
        return r;
    }
    if let Some(r) = ksv_route(ctx, args, native_ksv_iterator) {
        return r;
    }
    let mut this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    if let Some(cls) = ctx.class_name_of_id(ctx.class_id_of_object(this)) {
        if cls == "java/util/EnumSet"
            || cls == "java/util/RegularEnumSet"
            || cls == "java/util/JumboEnumSet"
        {
            return native_al_iterator(ctx, args);
        }
    }
    // Refreshing a live view allocates a replacement bucket array. Pin the
    // HashSet before that allocation, otherwise the subsequent backing lookup
    // can read the pre-move receiver and return a null iterator.
    //
    // gc-common w20-a: `this_pin` is this frame's pin base, and every exit
    // below releases it -- the four `?` exits and the "no backing map" return
    // used to strand it (and whatever a callee had pinned above it).
    let this_pin = ctx.pin_native_root(this);
    if let Err(e) = resync_view_set(ctx, &mut this) {
        ctx.unpin_native_roots(this_pin);
        return Err(e);
    }
    let this = ctx.read_native_pin(this_pin, this);
    let mut backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => {
            // (Quieted: previously eprintln. Enable via CRATONVM_HS_ITR_DBG.)
            if dbg_hs_itr() {
                eprintln!(
                    "[HS-ITR-DBG] native_hs_iterator: backing map is None for {:?}",
                    this
                );
            }
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(None)));
        }
    };
    // Copy-on-write is decided HERE, once `backing` is resolved and before any
    // live cursor is built: this family shares the HashSet surface but has the
    // opposite iterator contract. (It allocates nothing when it declines.)
    match cow_set_snapshot_iterator(ctx, this, backing) {
        Ok(Some(itr)) => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(Some(itr))));
        }
        Ok(None) => {}
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    }
    // `H4-1` section 3, closed. See [`real_ht_view_enumerator`]: the Hashtable
    // family gets java.base's OWN cursor rather than a snapshot under a
    // borrowed class name. Placed here because it needs `backing` — the view
    // carries no source slot of its own.
    //
    // `this_pin` is this frame's pin base, so it has to be released before any
    // return past it, exactly like the refusal path below.
    if let Some(src_map) = view_backing_source(&*ctx, backing) {
        // Pinned across the enumerator's construction (Java), which a declined
        // `None` can follow: the collect below needs the backing's current
        // address.
        let backing_pin = ctx.pin_native_root(backing);
        let e = real_ht_view_enumerator(ctx, this, src_map);
        let backing_now = ctx.read_native_pin(backing_pin, backing);
        ctx.unpin_native_roots(backing_pin);
        match e {
            Ok(Some(e)) => {
                ctx.unpin_native_roots(this_pin);
                return Ok(Some(Value::Object(Some(e))));
            }
            Ok(None) => {}
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        }
        backing = backing_now;
    }
    // Collect once for the count, allocate, then re-collect from the live
    // backing before storing. `alloc_ref_array` can trigger a moving GC; object
    // refs held only in the Rust Vec from the first collection would otherwise
    // go stale and surface as null/stale iterator elements during WildFly MSC
    // state reporting.
    //
    // Family-1 fix (cce0079): the FIRST collect below is itself GC-capable
    // for entrySet views (it allocates a live entry per pair) — `backing`
    // must be pinned BEFORE it, not after (pinning afterwards captured an
    // already-stale address; canary-caught live at `view_backing_source`).
    //
    // ONE collect, not two. The shape above was "collect for the length,
    // allocate, then collect AGAIN because `alloc_ref_array` may have moved
    // everything the first collect produced" — and the second collect's results
    // are then immediately handed to `pin_value_slice`, which is the OTHER way
    // to survive an allocation and the one this file uses everywhere else. So
    // pin the FIRST collect instead and the second walk is pure waste: for a
    // `LinkedHashMap` keySet view it is 1000 nodes x 2 `ctx.get_field` calls per
    // `iterator()`, on a path the Spring workload this was traced from runs
    // 100 000 times per test.
    //
    // The pins are unchanged in kind and in order relative to `this_pin`, which
    // is still this frame's base and still releases all of them.
    let backing_pin = ctx.pin_native_root(backing);
    let this = ctx.read_native_pin(this_pin, this);
    let keys = match hs_view_elements(ctx, this, backing) {
        Ok(k) => k,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let (_, key_handles) = pin_value_slice(ctx, &keys);
    let len = keys.len();
    // `CRATONVM_COMPAT_ITR_REMOVE_BY_NODE`: the NODE behind each element rides
    // in the upper half of the snapshot array, so `Iterator.remove()` can
    // remove it as `HashIterator.remove()` does (see `itr_snapshot_nodes`).
    // The half is reserved by shape here and filled after the last allocation
    // below, so no node needs a pin; a slot left null falls back to removal
    // by element.
    let carry_nodes = len > 0 && {
        let backing_now = ctx.read_native_pin(backing_pin, backing);
        itr_node_map(&*ctx, backing_now).is_some()
    };
    let keys_arr: ObjectRef = alloc_ref_array(ctx, if carry_nodes { len * 2 } else { len });
    let keys_arr_pin = ctx.pin_native_root(keys_arr);
    let backing = ctx.read_native_pin(backing_pin, backing);
    let keys_arr = ctx.read_native_pin(keys_arr_pin, keys_arr);
    let total = len;
    if dbg_hs_itr() {
        eprintln!(
            "[HS-ITR-DBG] native_hs_iterator: collected {} keys from backing map {:?}",
            total, backing
        );
    }
    for (i, k) in keys.iter().enumerate().take(total) {
        let k = read_pinned_elem(ctx, key_handles[i], *k);
        ctx.set_array_element(keys_arr, i, k);
    }
    if carry_nodes {
        // Pure reads and reference stores since the allocation: the backing
        // and the elements just stored are current, and so is every node.
        // A `keySet()` / `entrySet()` view carries its SOURCE's nodes
        // (`CRATONVM_COMPAT_VIEW_REMOVE_BY_NODE`), matched against each
        // element's key.
        if let Some((map, entries)) = itr_node_map(&*ctx, backing) {
            let stored: Vec<Value> = (0..total)
                .map(|i| itr_element_key(&*ctx, ctx.get_array_element(keys_arr, i), entries))
                .collect();
            for (i, node) in itr_snapshot_nodes(&*ctx, map, &stored)
                .into_iter()
                .enumerate()
            {
                ctx.set_array_element(keys_arr, total + i, Value::Object(Some(node)));
            }
        }
    }
    // Fallible since 2026-08-05 (JDK-only wave 2, lane L7). No JDK declares
    // `java.util.HashMap$KeyItr` — the real one is `HashMap$KeyIterator` — so a
    // strict run that gets this shape is running a snapshot iterator in place
    // of the real bytecode. Refuse there, naming the class.
    // Not `?`: `this_pin` is this frame's pin base, and unwinding past the
    // `unpin_native_roots` below would strand it and everything pinned above it.
    // A refusal is not the end of the road: `real_snapshot_iterator` hands back
    // the same elements through a real `Arrays$ArrayItr`, carrying the same
    // backing set so `it.remove()` still writes through (see that function).
    let this_now = ctx.read_native_pin(this_pin, this);
    let backing_now = ctx.read_native_pin(backing_pin, backing);
    let entryset = is_entryset_kind(view_backing_kind(ctx, backing_now));
    let carrier = key_itr_carrier_for(&*ctx, this_now, entryset);
    let itr: ObjectRef = match alloc_key_itr(ctx, carrier) {
        Ok(itr) => itr,
        Err(_refused) => {
            let keys_arr = ctx.read_native_pin(keys_arr_pin, keys_arr);
            let this = ctx.read_native_pin(this_pin, this);
            let real = real_snapshot_iterator(
                ctx,
                keys_arr,
                total,
                Some((this, SnapshotItrRoute::SetLike)),
            );
            ctx.unpin_native_roots(this_pin);
            return real;
        }
    };
    let base = key_itr_base(ctx, itr);
    let keys_arr = ctx.read_native_pin(keys_arr_pin, keys_arr);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(
        itr,
        base + MAP_KEY_ITR_FIELD_KEYS,
        Value::Object(Some(keys_arr)),
    );
    ctx.set_field(itr, base + MAP_KEY_ITR_FIELD_CURSOR, Value::Int(0));
    ctx.set_field(
        itr,
        base + MAP_KEY_ITR_FIELD_TOTAL,
        Value::Int(total as i32),
    );
    // Wire backing HashSet for Iterator.remove() — without this, JDK code
    // like MXBeanSupport.findMXBeanInterface (which iterates a HashSet and
    // calls it.remove() inside the loop) throws UnsupportedOperationException
    // because dispatch falls through to the default Iterator.remove().
    ctx.set_field(
        itr,
        base + MAP_KEY_ITR_FIELD_BACKING,
        Value::Object(Some(this)),
    );
    ctx.set_field(itr, base + MAP_KEY_ITR_FIELD_LAST_RET, Value::Int(-1));
    // Seed the fail-fast generation LAST: `MAP_KEY_ITR_FIELD_BACKING` above is
    // how `map_itr_comod_source` finds the collection to watch.
    map_itr_seed_expected(ctx, itr);
    // And the carrier's OWN fields, so the class name this VM put on the object
    // is not the only true thing about it. Pure reads and writes on an object
    // allocated two statements ago -- no allocation, so nothing can move
    // between `alloc_key_itr` and here and `itr` needs no pin of its own.
    //
    // `key_itr_view_source` is the same source `key_itr_carrier_for` consulted
    // to CHOOSE the carrier, which is what keeps the seeded `this$0` and the
    // class name agreeing about which map this iterator is over.
    let this_now = ctx.read_native_pin(this_pin, this);
    let source = key_itr_view_source(&*ctx, this_now);
    if let Some(source) = source {
        seed_real_map_itr_fields(ctx, itr, carrier, source);
        // A reversed `LinkedKeySet` / `LinkedEntrySet` (C3-1): the real
        // `LinkedHashIterator(true)` starts at `tail` and follows `before`.
        // Only over a carrier the seeding above populated (`this$0` landed).
        if linked_set_view_reversed(&*ctx, this_now)
            && matches!(ctx.get_field_by_name(itr, "this$0"), Value::Object(Some(_)))
            && set_real_itr_field(ctx, itr, base, "reversed", Value::Int(1))
        {
            let tail = real_map_field(&*ctx, "java/util/LinkedHashMap", source, "tail")
                .unwrap_or(Value::Object(None));
            set_real_itr_field(ctx, itr, base, "next", tail);
        }
    }
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(itr))))
}

pub(crate) fn native_hs_spliterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A real `CopyOnWriteArraySet` is served by its own bytecode over the
    // `CopyOnWriteArrayList` in `al`, not by this file's map surface.
    // See [`cow_set_route`].
    if let Some(r) = cow_set_route(ctx, args, "spliterator", "()Ljava/util/Spliterator;") {
        return r;
    }
    let mut this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            let spl = alloc_empty_array_holder(ctx, "java/util/Spliterator", 4)?;
            ctx.set_field(spl, 2, Value::Int(0));
            // No receiver to ask on this arm, and an empty spliterator's
            // encounter order is unobservable either way.
            ctx.set_field(spl, SPL_FIELD_CHARACTERISTICS, Value::Int(SPL_HASH_SET));
            return Ok(Some(Value::Object(Some(spl))));
        }
    };

    // Live map views (`Map.keySet()` / `entrySet()`) are represented as a
    // HashSet snapshot plus a source-map marker. Iterator/contains/size already
    // resync that snapshot, but real JDK `HashSet.spliterator()` bypasses those
    // natives and reads the backing map directly. Resync here before taking the
    // stream snapshot so a cached view reflects source-map mutations made after
    // the view was obtained (Spring `LinkedCaseInsensitiveMap$KeySet`).
    resync_view_set(ctx, &mut this)?;

    let backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => return Ok(Some(Value::Object(None))),
    };
    let len = hs_view_elements(ctx, this, backing)?.len();
    let backing_pin = ctx.pin_native_root(backing);
    // Wave 9 (lane spliter): `this` is read again after both allocations
    // below (`hs_is_insertion_ordered`); pinned with `backing` so one unpin
    // releases both.
    let this_pin = ctx.pin_native_root(this);
    let arr = alloc_ref_array(ctx, len);
    let arr_pin = ctx.pin_native_root(arr);
    let backing = ctx.read_native_pin(backing_pin, backing);
    let this = ctx.read_native_pin(this_pin, this);
    let elems = hs_view_elements(ctx, this, backing)?;
    let arr = ctx.read_native_pin(arr_pin, arr);
    for (i, v) in elems.iter().enumerate().take(len) {
        ctx.set_array_element(arr, i, *v);
    }
    // `--jdk-only` (wave 9, backlog collections w8b-1): the only receiver that
    // reaches this body there is a real `CopyOnWriteArraySet` whose
    // `cow_set_route` declined (a re-entry). Answer with what the JDK's own
    // body builds, `Spliterators.spliterator(array, IMMUTABLE | DISTINCT)`, a
    // real `Spliterators$ArraySpliterator`, rather than an instance of the
    // `Spliterator` INTERFACE (see `native_stream_spliterator`).
    // `CRATONVM_SPLITERATOR_JDK_FALLBACK=0` restores the interface instance.
    if ctx.is_jdk_only() && spliterator_jdk_fallback_enabled() {
        let chars = if is_real_cow_array_set(ctx, this) {
            SPL_IMMUTABLE | SPL_DISTINCT
        } else {
            SPL_DISTINCT
        };
        let r = ctx.invoke(
            "java/util/Spliterators",
            "spliterator",
            "([Ljava/lang/Object;I)Ljava/util/Spliterator;",
            &[Value::Object(Some(arr)), Value::Int(chars)],
        );
        ctx.unpin_native_roots(backing_pin);
        return r;
    }
    let spl = match try_alloc_synthetic(ctx, "java/util/Spliterator", 4) {
        Ok(s) => s,
        Err(e) => {
            ctx.unpin_native_roots(backing_pin);
            return Err(e);
        }
    };
    let arr = ctx.read_native_pin(arr_pin, arr);
    let backing = ctx.read_native_pin(backing_pin, backing);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(spl, 0, Value::Object(Some(arr)));
    ctx.set_field(spl, 1, Value::Int(0));
    ctx.set_field(spl, 2, Value::Int(len as i32));
    let spl_chars = hs_spliterator_characteristics(&*ctx, this, backing);
    ctx.set_field(spl, SPL_FIELD_CHARACTERISTICS, Value::Int(spl_chars));
    ctx.unpin_native_roots(backing_pin);
    ctx.unpin_native_roots(arr_pin);
    Ok(Some(Value::Object(Some(spl))))
}

/// The characteristics of [`native_hs_spliterator`]'s spliterator over
/// `this` (whose backing map is `backing`), which [`native_hs_stream`] also
/// records as its stream's source facts. Pure reads.
///
/// A VIEW answers its SOURCE family's cell; a standalone set answers its
/// own. The two used to collapse into one binary choice on
/// `hs_is_insertion_ordered`, which gave `LinkedHashMap.keySet()` and
/// `.entrySet()` the plain `HashSet` cell -- they are 16465, not 65 -- and
/// gave a standalone `LinkedHashSet` 81 where the JDK's construction adds
/// SUBSIZED for 16465.
pub(crate) fn hs_spliterator_characteristics(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    backing: ObjectRef,
) -> i32 {
    match view_backing_source(ctx, backing) {
        Some(src) => {
            // `view_backing_kind`, the marker the backing already carries,
            // rather than a fresh element-class guess: it is what every other
            // consumer of these views reads.
            let kind = match view_backing_kind(ctx, backing) {
                VIEW_KIND_ENTRYSET | VIEW_KIND_ENTRYSET_STATIC => SplViewKind::Entries,
                _ => SplViewKind::Keys,
            };
            view_spliterator_characteristics(ctx, src, kind)
        }
        None if hs_is_insertion_ordered(ctx, this) => SPL_LINKED_SET,
        None => SPL_HASH_SET,
    }
}

pub(crate) fn native_hs_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A real `CopyOnWriteArraySet` is served by its own bytecode over the
    // `CopyOnWriteArrayList` in `al`, not by this file's map surface.
    // See [`cow_set_route`].
    if let Some(r) = cow_set_route(ctx, args, "toArray", "()[Ljava/lang/Object;") {
        return r;
    }
    let mut this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    resync_view_set(ctx, &mut this)?;
    let backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => return Ok(Some(Value::Object(None))),
    };
    // GC-safe (HIB-CV-18): `alloc_ref_array` may trigger a young GC that MOVES
    // the element objects. The collected `Value`s are raw object pointers held
    // only in a Rust `Vec` — not a GC root — so any pointer gathered BEFORE the
    // allocation is stale afterwards, and storing it yields whatever now lives
    // at the old address (observed as VM-singleton heap garbage on the first
    // `toArray()` after a collection load, while the second call and the
    // GC-aware iterator returned the right values). Mirror the documented
    // count → allocate → re-read pattern (cf. `tm_materialize_deser_array`):
    // collect once only to learn the length, allocate, then RE-collect from the
    // live (and now post-GC) backing and store each ref immediately, with no
    // allocation between the read and the store.
    //
    // ...and root the receiver and `backing` for the whole call (gc-common
    // w10-g; only `backing`, and only across the allocation, until then): both
    // `hs_view_elements` calls are GC points, and each reads `this` for its
    // class and walks `backing`.
    let this_pin = ctx.pin_native_root(this);
    let backing_pin = ctx.pin_native_root(backing);
    let out = hs_to_array_rooted(ctx, (this_pin, this), (backing_pin, backing));
    ctx.unpin_native_roots(this_pin);
    out
}

/// The body of [`native_hs_to_array`], with the receiver and the backing map
/// pinned by the caller as `(handle, reference)`. Pins taken here sit above
/// the caller's, which releases them too.
pub(crate) fn hs_to_array_rooted(
    ctx: &mut dyn NativeContext,
    this: (usize, ObjectRef),
    backing: (usize, ObjectRef),
) -> MethodCallResult {
    let (this_pin, this) = this;
    let (backing_pin, backing) = backing;
    let len = hs_view_elements(ctx, this, backing)?.len();
    // Registered-only (`toArray()` of `SET_CLASSES` and of the set-view
    // carriers), so the allocation may collect and retry (gc-common w10-g).
    // `live` audit: `this` and `backing` are pinned by the caller and re-read
    // below; the elements are RE-collected below; `arr` is pinned across that
    // re-collect, the one GC point between its allocation and its stores.
    let arr = new_ref_array_grow(ctx, ClassId::new(0), len, GrowAlloc::Reclaim);
    let this = ctx.read_native_pin(this_pin, this);
    let backing = ctx.read_native_pin(backing_pin, backing);
    let arr_pin = ctx.pin_native_root(arr);
    let keys = hs_view_elements(ctx, this, backing);
    let arr = ctx.read_native_pin(arr_pin, arr);
    let keys = keys?;
    for (i, k) in keys.iter().enumerate().take(len) {
        ctx.set_array_element(arr, i, *k);
    }
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn native_hs_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A real `CopyOnWriteArraySet` is served by its own bytecode over the
    // `CopyOnWriteArrayList` in `al`, not by this file's map surface.
    // See [`cow_set_route`].
    if let Some(r) = cow_set_route(ctx, args, "toString", "()Ljava/lang/String;") {
        return r;
    }
    let mut this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    resync_view_set(ctx, &mut this)?;
    let backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => {
            let s = ctx.create_string("[]");
            return Ok(Some(Value::Object(Some(s))));
        }
    };
    let keys = hs_view_elements(ctx, this, backing)?;
    // gc-common w20-a: rendered through pins (each `toString()` can move the
    // keys not yet rendered).
    let parts = display_units_pinned(ctx, &keys)?;
    let text = wrap_join("[", &parts, ", ", "]");
    let s = ctx.create_string_from_units(&text);
    Ok(Some(Value::Object(Some(s))))
}

