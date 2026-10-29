// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

// ConcurrentHashMap (Phase 22 / M18, Phase 86.2 — segmented concurrency)
// ===========================================================================
//
// ConcurrentHashMap uses a striped/segmented layout for real concurrency:
//   field 0 = Object[] segments (array of segment objects)
//   field 1 = Int segment_mask (num_segments - 1)
// Each segment is a 3-field HashMap-like object (buckets, size, capacity).
// Reads (get, containsKey) are lock-free. Writes (put, remove) lock only
// the target segment via monitor_enter/exit, allowing concurrent writes to
// different segments without contention.

pub(crate) const CHM_FIELD_SEGMENTS: usize = 0;
// Slot 1 was historically used to cache `num_segments - 1`, but a real-JDK
// ConcurrentHashMap's slot 1 is reference-typed and silently coerces `Int`
// writes to `Object(None)`. The mask/count are now derived from the segments
// array length instead (see `chm_segment_for`), so this slot is unused.
pub(crate) const _CHM_FIELD_SEGMENT_MASK: usize = 1;
pub(crate) const _CHM_NUM_FIELDS: usize = 2;
pub(crate) const CHM_DEFAULT_SEGMENTS: usize = 16;
pub(crate) const CHM_DEFAULT_SEGMENT_CAP: usize = 4;
/// Segment count for the no-arg `ConcurrentHashMap()` constructor ONLY
/// (`native_chm_init_default`) — chosen so that
/// `CHM_DEFAULT_INIT_SEGMENTS * CHM_DEFAULT_SEGMENT_CAP == 16`, matching real
/// JDK's `ConcurrentHashMap.DEFAULT_CAPACITY` (the lazily-allocated
/// single-table size on first `put`, before any resize).
///
/// `chm_total_capacity` sums live segment bucket-array lengths as a proxy
/// for "what a real flat table would currently be sized to" (see
/// `chm_reorder_by_virtual_bucket`), so the STARTING total must match
/// HotSpot's real default (16), not the previous placeholder
/// (`CHM_DEFAULT_SEGMENTS * CHM_DEFAULT_SEGMENT_CAP` = 64) — the mismatch
/// silently reordered `entrySet()`/`keySet()`/`values()` relative to
/// HotSpot for any freshly-constructed default map with few entries (e.g.
/// Hibernate's `EntityPersisterConcurrentMap`, which caused
/// `SoftDeleteFetchModeTests` to skip a cross-entity validation check that
/// depends on processing order).
///
/// Using 16 segments at capacity 1 each (also totalling 16) was tried and
/// rejected: `native_map_put`'s resize check is `size + 1 > (cap * 3) / 4`,
/// which for `cap == 1` integer-truncates the threshold to 0 — ANY first
/// insert into a segment immediately triggers a resize, so the segment
/// capacities (and therefore the sum `chm_total_capacity` reads) no longer
/// stay at their intended starting total even for a handful of entries.
/// 4 segments of capacity 4 keeps the same total (16) but tolerates up to 3
/// entries per segment before resizing, matching real JDK's actual
/// behaviour for lightly-loaded maps. The cost is reduced write-striping
/// concurrency (4-way instead of 16-way) for maps that are never given an
/// explicit initial capacity — deemed an acceptable trade-off for
/// correctness of iteration order, which explicit-capacity constructors
/// (`native_chm_init_capacity`/`native_chm_init_full`, unaffected by this
/// constant) are unaffected by.
pub(crate) const CHM_DEFAULT_INIT_SEGMENTS: usize = 4;

/// Compute hash for a key value (reuses map_hash_key for object keys).
///
/// Propagates any exception thrown by a user-supplied `hashCode()` via the
/// underlying `map_hash_key` (MED fix).
pub(crate) fn chm_key_hash(ctx: &mut dyn NativeContext, key: &Value) -> Result<i32, MethodCallFailed> {
    match key {
        Value::Object(Some(k)) => map_hash_key(ctx, *k),
        _ => Ok(0),
    }
}

/// Get the segment object for a given hash.
pub(crate) fn chm_segment_for(ctx: &dyn NativeContext, this: ObjectRef, hash: i32) -> Option<ObjectRef> {
    // r9w12: read with the descriptor supplied rather than resolved.
    //
    // `ctx.get_field` opens by resolving this slot's declared descriptor from
    // the receiver's class metadata, and that lookup needs the receiver's class
    // id, which is a checked heap read of its own. The answer is not in doubt:
    // slot 0 is the segments array on a CratonVM-shaped `ConcurrentHashMap` and
    // `table` (`Node<K,V>[]`, also a reference) on a real-JDK-shaped one, and
    // the `Value::Object(Some(..))` match below already refuses everything
    // else. `b'L'` and `b'['` both decode as a reference, so the coercion is
    // the same one the resolved descriptor would have produced.
    //
    // This is on the per-`get` path via `chm_get_wrapper_key_fast`; see
    // `NativeSystemAccess::class_id_and_unboxed_primitive` for the measurement
    // that made the per-access cost of this native the thing to cut.
    let segments = match ctx.get_field_typed(this, CHM_FIELD_SEGMENTS, b'L') {
        Value::Object(Some(arr)) => arr,
        _ => return None,
    };
    // Derive the segment mask from the segments array length — NOT from the
    // `CHM_FIELD_SEGMENT_MASK` slot. Slot 1 of a real-JDK ConcurrentHashMap is a
    // reference-typed field, so writing `Int(num_segments - 1)` there is silently
    // descriptor-coerced to `Object(None)` (see `coerce_field_value_by_descriptor`
    // `b'L'` arm). The stale fallback (`CHM_DEFAULT_SEGMENTS - 1` = 15) then only
    // matched when there happened to be exactly 16 segments; any other count
    // (e.g. concurrencyLevel=1 → 1 segment, as Hibernate's
    // `EntityManagerFactoryBuilderImpl` uses for `configurationValues`) routed
    // most hashes out of bounds, so `put` silently dropped them. The array length
    // is always a power of two, so `len - 1` is the authoritative mask.
    let seglen = ctx.array_length(segments) as usize;
    if seglen == 0 {
        return None;
    }
    let mask = seglen - 1;
    let idx = (hash as u32 as usize) & mask;
    match ctx.get_array_element(segments, idx) {
        Value::Object(Some(seg)) => Some(seg),
        _ => None,
    }
}

/// [`chm_segment_for`] inside a validate-once read region, handing back the
/// segment as a cursor so the chain walk continues in the SAME region.
///
/// Identical decisions to `chm_segment_for`, read for read — the descriptor is
/// supplied rather than resolved, the mask comes from the segments array's
/// LENGTH and not from `CHM_FIELD_SEGMENT_MASK` (see that function for the
/// silently-dropped-`put` defect that rule exists for), and a non-reference or
/// absent slot declines. What differs is only that `this` is validated once
/// and the two reads reached from it are not re-validated at all.
pub(crate) fn chm_segment_cursor_for<'c>(
    ctx: &'c dyn NativeContext,
    this: ObjectRef,
    hash: i32,
) -> Option<cratonvm_native_api::ReadCursor<'c>> {
    let this_cur = ctx.read_cursor(this)?;
    let segments_cur = ctx.cursor_field_reference(&this_cur, CHM_FIELD_SEGMENTS)?;
    let (_, seglen) = ctx.cursor_array_shape(&segments_cur)?;
    if seglen == 0 {
        return None;
    }
    let idx = (hash as u32 as usize) & (seglen - 1);
    ctx.cursor_array_element_reference(&segments_cur, idx)
}

/// Get all segment objects.
pub(crate) fn chm_all_segments(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<ObjectRef> {
    let segments = match ctx.get_field(this, CHM_FIELD_SEGMENTS) {
        Value::Object(Some(arr)) => arr,
        _ => return Vec::new(),
    };
    // Segment count = segments array length (authoritative). See `chm_segment_for`
    // for why the `CHM_FIELD_SEGMENT_MASK` slot cannot be trusted.
    let count = ctx.array_length(segments) as usize;
    let mut result = Vec::with_capacity(count);
    for i in 0..count {
        if let Value::Object(Some(seg)) = ctx.get_array_element(segments, i) {
            result.push(seg);
        }
    }
    result
}

/// Compute the total live bucket capacity across all segments — the sum of
/// each segment's own bucket-array length. Used as a proxy for "what a real
/// flat-table JDK `ConcurrentHashMap` would currently be sized to", so that
/// `chm_virtual_bucket` can reorder entries to match HotSpot's single-table
/// iteration order (see `chm_reorder_by_virtual_bucket`).
/// The bucket-table size a real JDK `ConcurrentHashMap` holds for `n` entries.
///
/// **Measured, not derived.** `probes/ChmTableSizeProbe.java` reads
/// `ConcurrentHashMap.table.length` by reflection for every initial capacity in
/// `1..=80` and for a default map at every size up to 200 (JDK 25, Temurin).
/// The function that fits all of it is: the smallest power of two `P` with
/// `n < P - (P >>> 2)` — the first table whose 0.75 load threshold is strictly
/// above `n`. Spot values, all confirmed: `CHM(2)` -> 4, `CHM(3)` -> 8,
/// `CHM(11)` -> 16, `CHM(12)` -> 32, `CHM(47)` -> 64, `CHM(48)` -> 128; a
/// default map grows 16 -> 32 -> 64 -> 128 at sizes 12, 24, 48.
///
/// Note this is NOT `tableSizeFor(c + (c >>> 1) + 1)`, which the constructor's
/// source reads like and which predicts 32 for `CHM(11)`. The measurement says
/// 16. Where the two disagree, the measurement wins.
pub(crate) fn chm_jdk_table_size_for(n: usize) -> usize {
    let mut cap = 1usize;
    // 1 << 30 is `MAXIMUM_CAPACITY`; stop there rather than overflow.
    while cap < (1usize << 30) && n >= cap - (cap >> 2) {
        cap <<= 1;
    }
    cap
}

/// Record the table size a real JDK map would have allocated for this
/// constructor, so iteration can reproduce its bucket order later.
///
/// `sizeCtl` is the JDK's OWN field for exactly this — `ConcurrentHashMap(int)`
/// ends with `this.sizeCtl = cap`, the table size its first `put` will
/// allocate. Writing it here is descriptor-correct (it is an `int` field on the
/// real class, so no `Ljava/lang/Object;` coercion), it is a value real-JDK
/// bytecode reading the field would find plausible, and nothing in this
/// implementation reads it for any other purpose.
///
/// Why it has to be recorded at all: `chm_total_capacity` sums the LIVE segment
/// bucket arrays, and those resize on PER-SEGMENT load while a real JDK table
/// resizes on TOTAL load. The two agree at construction and drift apart from
/// the first segment resize onward — which is precisely the bug this records
/// against. See `chm_reorder_by_virtual_bucket`.
pub(crate) fn chm_record_initial_table(ctx: &mut dyn NativeContext, this: ObjectRef, jdk_cap: usize) {
    ctx.set_field_by_name(this, "sizeCtl", Value::Int(jdk_cap.min(1 << 30) as i32));
}

/// The recorded construction-time table size, or the default-constructor table
/// size (16) when nothing was recorded.
pub(crate) fn chm_initial_table(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    match ctx.get_field_by_name(this, "sizeCtl") {
        Value::Int(v) if v > 0 => (v as usize).next_power_of_two(),
        _ => 16,
    }
}

/// `CRATONVM_COMPAT_CHM_TABLE_HIGH_WATER` — default ON, `=0` restores the old
/// order. A real `ConcurrentHashMap` table only ever GROWS (`clear` and
/// `remove` keep it), so after a map that reached 12 entries loses three, its
/// nine entries still iterate in the order of a 32-bucket table. The
/// iteration reorder ([`chm_reorder_by_virtual_bucket`]) sized its virtual
/// table from the CURRENT count, i.e. 16. Round 13 wave 8, lane hashcompat3
/// (`R13HashcompatEqualsOrder`, `ConcurrentHashMap spread` `sig=`).
pub(crate) fn chm_table_high_water_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_CHM_TABLE_HIGH_WATER")
}

/// Entries across every segment (the sum `native_chm_size` answers).
pub(crate) fn chm_entry_count(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    let mut count = 0usize;
    for seg in chm_all_segments(ctx, this) {
        if let Value::Int(s) = ctx.get_field(seg, MAP_FIELD_SIZE) {
            count += s.max(0) as usize;
        }
    }
    count
}

/// `CRATONVM_COMPAT_CHM_JDK25_SIZING` -- default ON, `=0` restores the old
/// table model. JDK 25.0.3 `ConcurrentHashMap` (read from the JDK's own
/// `src.zip`, round 15 wave 5 lane compat6):
///
/// * `ConcurrentHashMap(Map m)` is `this(m.size()); putAll(m);` -- `m.size()`
///   is asked once (MEASURED, `R15Compat4MapQuestions` row 19: `sz=1`), a
///   negative answer is `this(int)`'s `IllegalArgumentException`, and the
///   table is `ConcurrentHashMap(int)`'s ([`chm_jdk_table_size_for`] of the
///   answer), not the default 16 -- so a copy of a small map iterates in an
///   8- or 4-bucket order;
/// * `putAll(m)` presizes with `tryPresize(size() + m.size())`, not
///   `tryPresize(m.size())` ([`chm_note_presize`]).
pub(crate) fn chm_jdk25_sizing_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_CHM_JDK25_SIZING")
}

/// `ConcurrentHashMap.putAll(m)` runs `tryPresize(size() + m.size())` when its
/// table already exists (approximated, as `native_chm_put_all`'s NPE message
/// already does, by a non-zero entry count): the table doubles until
/// `tableSizeFor(size + size/2 + 1)` fits under its 0.75 threshold, which can
/// be a size the inserts alone never reach (a 1-entry map `putAll` of 10
/// entries: `tryPresize(11)` wants 32, which a 16-table's threshold of 12
/// does not hold and a 32-table's of 24 does not either, so 64 buckets, where
/// 11 entries alone stay in 16). `size` is `m.size()`; the receiver's own
/// count is added under [`chm_jdk25_sizing_enabled`] (round 15 wave 5, lane
/// compat6 -- it was left out). Recorded like [`chm_note_table_high_water`],
/// under the same switch.
pub(crate) fn chm_note_presize(ctx: &mut dyn NativeContext, this: ObjectRef, size: usize) {
    if !chm_table_high_water_enabled() {
        return;
    }
    let count = chm_entry_count(&*ctx, this);
    if count == 0 {
        return;
    }
    let size = if chm_jdk25_sizing_enabled() {
        size.saturating_add(count)
    } else {
        size
    };
    const MAXIMUM_CAPACITY: usize = 1 << 30;
    let wanted = if size >= MAXIMUM_CAPACITY >> 1 {
        MAXIMUM_CAPACITY
    } else {
        (size + (size >> 1) + 1)
            .checked_next_power_of_two()
            .unwrap_or(MAXIMUM_CAPACITY)
            .min(MAXIMUM_CAPACITY)
    };
    let recorded = chm_initial_table(&*ctx, this);
    let mut table = recorded.max(chm_jdk_table_size_for(count));
    while wanted > table - (table >> 2) && table < MAXIMUM_CAPACITY {
        table <<= 1;
    }
    if table > recorded {
        chm_record_initial_table(ctx, this, table);
    }
}

/// Raise the recorded table size ([`chm_record_initial_table`], read back by
/// [`chm_initial_table`]) to the table a real map holding this many entries
/// has grown to. Called after the natives that can insert. Each insert adds
/// one entry, so only a count at a JDK resize point (12, 24, 48, ... for a
/// default map) can raise it, and only there is the by-name `sizeCtl` field
/// read; `bulk` (after `putAll`) checks unconditionally. No Java, no
/// allocation on the Java heap.
pub(crate) fn chm_note_table_high_water(ctx: &mut dyn NativeContext, this: ObjectRef, bulk: bool) {
    if !chm_table_high_water_enabled() {
        return;
    }
    let count = chm_entry_count(&*ctx, this);
    let table = chm_jdk_table_size_for(count);
    if count == 0 || (!bulk && table == chm_jdk_table_size_for(count - 1)) {
        return;
    }
    if table > chm_initial_table(&*ctx, this) {
        chm_record_initial_table(ctx, this, table);
    }
}

pub(crate) fn chm_total_capacity(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    let mut total = 0usize;
    for seg in chm_all_segments(ctx, this) {
        let (_buckets, _size, cap) = map_state(ctx, seg);
        total += cap.max(0) as usize;
    }
    total.max(1)
}

/// Reorder entries collected from CratonVM's segmented storage (iterated
/// segment-major, bucket-minor) into the order a real flat-table JDK
/// `ConcurrentHashMap` would yield them in (bucket-major over a single
/// table sized to the map's current total capacity).
///
/// CratonVM's `ConcurrentHashMap` is backed by `CHM_DEFAULT_SEGMENTS` (16)
/// independently-hashed+resized bucket tables rather than the one flat table
/// real JDK uses, so a segment-major walk visits entries in a fundamentally
/// different order than `aliasMap.forEach` on HotSpot. Callers that only
/// care about *membership* (the overwhelming majority) are unaffected, but
/// order-sensitive callers — notably `SimpleAliasRegistry.retrieveAliases`
/// (`aliasMap.forEach`), which feeds `getAliases()`'s returned array order —
/// broke `XmlBeanDefinitionReaderTests`/`XmlBeanFactoryTests`'
/// `containsExactly("myalias", "youralias")` assertions.
///
/// Since every bucket node already stores its full (spread) hash
/// (`NODE_FIELD_HASH`), and `native_chm_init_capacity` now sizes the total
/// bucket count to match real JDK's `tableSizeFor` formula, we can recover
/// HotSpot's order by a *stable* sort keyed on `hash & (total_capacity - 1)`
/// — the bucket index a flat table of that size would use. Ties (distinct
/// CratonVM segments/buckets that alias to the same virtual bucket) keep
/// their relative collection order, which is the best available
/// approximation of JDK's intra-bucket chain order without also replicating
/// its incremental resize-and-split history exactly.
pub(crate) fn chm_reorder_by_virtual_bucket<T>(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    mut items: Vec<(i32, T)>,
) -> Vec<T> {
    // The virtual table size is the one a REAL JDK map would hold for this many
    // entries, never `chm_total_capacity`. Summing the live segment bucket
    // arrays looks equivalent and is not: our segments resize on their OWN
    // load, a real flat table resizes on TOTAL load, so the sum overshoots from
    // the first segment resize onward and every key's bucket index shifts with
    // it. Measured on the 8 constraint names of H2's `testScript.sql` SCRIPT
    // case: the sum was 32 where the real table was 16, and cap 32 reproduces
    // exactly the wrong order that test saw.
    //
    // A stable sort is load-bearing for the tie-break. Two keys in one virtual
    // bucket share every low bit of the mask, so they also share OUR segment
    // and (our per-segment table being no larger than the virtual one) our
    // bucket — one chain, whose order is insertion order. `probes/ChainOrderProbe`
    // confirms that against HotSpot using equal-hashCode keys, which no table
    // size can separate.
    let cap = chm_initial_table(ctx, this).max(chm_jdk_table_size_for(items.len()));
    let mask = (cap - 1) as u32;
    items.sort_by_key(|(hash, _)| (*hash as u32) & mask);
    items.into_iter().map(|(_, v)| v).collect()
}

// ---------------------------------------------------------------------------
// The JDK model (nio21 wave 1, L4; the `jdk-concurrent-hash-map` row)
// ---------------------------------------------------------------------------
//
// Under `--jdk-only`, and under `--compatible` over a real JDK with the
// `jdk-concurrent-hash-map` row (`retired_shadow::COMPAT_FAMILIES`), no
// `ConcurrentHashMap` native is registered (`RETIRED_SHADOW_PHASE3_TRIPLES`
// plus the family's stub and bridge lists). Every producer of the segmented
// layout above is one of those natives, so in such a VM EVERY
// `ConcurrentHashMap` was built and is maintained by the JDK's bytecode: slot 0
// is `table` (`ConcurrentHashMap$Node[]`), and reading it as a segments array
// treats each bin's `Node` as a segment. The Rust readers that are NOT
// registered CHM natives -- the `map_collect_*` collectors (`collect_entries_any`
// copies, `Properties.map` through `properties_backing_chm`) and the
// `java/util/Map` interface natives' CHM reroutes -- must therefore read the
// JDK's table, or run the receiver's own method. The predicate is VM-wide
// (`NativeContext::aligned`, one per VM, no process state) because the model
// is: under the row nothing can mint a segmented map.

/// `CRATONVM_CHM_JDK_TABLE_READS` -- default ON; `=0` restores the segmented
/// reads for a JDK-model map (the pre-nio21 answers, which were wrong for one:
/// each bin's `Node` was read as a segment). nio21 wave 1, L4.
pub(crate) fn chm_jdk_table_reads_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_CHM_JDK_TABLE_READS")
}

/// Is every `ConcurrentHashMap` in this VM the JDK's own (`table` of `Node`s)?
/// `--jdk-only`, or `--compatible` over a real JDK with the
/// `jdk-concurrent-hash-map` row in force. Cold relative to the reads it
/// guards: asked only once a receiver is already known to be a CHM.
#[inline]
pub(crate) fn chm_jdk_model(ctx: &dyn NativeContext) -> bool {
    chm_jdk_table_reads_enabled()
        && ctx.aligned(cratonvm_types::HotSpotAlignment::JdkConcurrentHashMap)
}

/// `ConcurrentHashMap.MOVED`: a `ForwardingNode` (the bin moved to `nextTable`).
pub(crate) const CHM_JDK_MOVED: i32 = -1;
/// `ConcurrentHashMap.TREEBIN`: a `TreeBin`, whose entries are its `first` list.
pub(crate) const CHM_JDK_TREEBIN: i32 = -2;
/// `ConcurrentHashMap.RESERVED`: a `ReservationNode` (an in-flight
/// `computeIfAbsent`), which holds no mapping.
pub(crate) const CHM_JDK_RESERVED: i32 = -3;
/// Nodes walked in one bin before the walk gives up. Only a corrupt cycle
/// reaches it; a `TreeBin`'s list is bounded by the map's size.
pub(crate) const CHM_JDK_BIN_WALK_LIMIT: usize = 1 << 24;
/// Nested `ForwardingNode` tables followed (one per resize in flight).
pub(crate) const CHM_JDK_FORWARD_DEPTH: usize = 8;

/// The `hash` / `key` / `val` / `next` slots of a JDK `ConcurrentHashMap$Node`
/// class (every bin class extends `Node`, so a subclass inherits the four),
/// memoized per class for one walk.
pub(crate) fn chm_jdk_node_slots(
    ctx: &dyn NativeContext,
    cache: &mut Vec<(ClassId, [usize; 4])>,
    cid: ClassId,
) -> Option<[usize; 4]> {
    if let Some((_, slots)) = cache.iter().find(|(c, _)| *c == cid) {
        return Some(*slots);
    }
    let slot = |name: &str| ctx.resolve_field_index_by_class_id(cid, name);
    let slots = [slot("hash")?, slot("key")?, slot("val")?, slot("next")?];
    cache.push((cid, slots));
    Some(slots)
}

/// Visit every mapping of a JDK-model `ConcurrentHashMap` in the JDK's own
/// iteration order, which is `ConcurrentHashMap$Traverser`'s: table index
/// order, bin order within an index; a `TreeBin` by its `first` list; a
/// `ForwardingNode` at index `i` of an `n`-bin table by bins `i` then `i + n`
/// of its `nextTable`; a `ReservationNode` holds nothing. Weakly consistent,
/// like the JDK's iterators. No Java call and no Java allocation (`&dyn`).
///
/// `false` when some bin could not be read (a shape the walk does not know,
/// a layout with no such field, or a cap): what was visited is then a lower
/// bound. A map whose `table` was never allocated is empty (`true`).
pub(crate) fn chm_jdk_table_walk(
    ctx: &dyn NativeContext,
    map: ObjectRef,
    visit: &mut dyn FnMut(Value, Value),
) -> bool {
    let map_class = ctx.class_id_of_object(map);
    let Some(table_slot) = ctx.resolve_field_index_by_class_id(map_class, "table") else {
        return false;
    };
    let table = match ctx.get_field(map, table_slot) {
        Value::Object(Some(table)) => table,
        _ => return true,
    };
    let mut cache = Vec::new();
    let mut whole = true;
    for i in 0..ctx.array_length(table) {
        whole &= chm_jdk_bin_walk(ctx, table, i, 0, &mut cache, visit);
    }
    whole
}

/// One bin of [`chm_jdk_table_walk`].
pub(crate) fn chm_jdk_bin_walk(
    ctx: &dyn NativeContext,
    table: ObjectRef,
    index: usize,
    depth: usize,
    cache: &mut Vec<(ClassId, [usize; 4])>,
    visit: &mut dyn FnMut(Value, Value),
) -> bool {
    let n = ctx.array_length(table);
    if index >= n {
        return true;
    }
    let mut node = match ctx.get_array_element(table, index) {
        Value::Object(Some(node)) => node,
        _ => return true,
    };
    for _ in 0..CHM_JDK_BIN_WALK_LIMIT {
        let cid = ctx.class_id_of_object(node);
        let Some([hash_slot, key_slot, val_slot, next_slot]) = chm_jdk_node_slots(ctx, cache, cid)
        else {
            return false;
        };
        let hash = match ctx.get_field(node, hash_slot) {
            Value::Int(hash) => hash,
            _ => return false,
        };
        if hash >= 0 {
            let key = ctx.get_field(node, key_slot);
            if matches!(key, Value::Object(Some(_))) {
                visit(key, ctx.get_field(node, val_slot));
            }
        } else if hash == CHM_JDK_TREEBIN {
            // The `TreeNode`s are also a `next`-linked list from `first`, in
            // the order the JDK's `Traverser` reads them.
            let Some(first_slot) = ctx.resolve_field_index_by_class_id(cid, "first") else {
                return false;
            };
            match ctx.get_field(node, first_slot) {
                Value::Object(Some(first)) => {
                    node = first;
                    continue;
                }
                _ => return true,
            }
        } else if hash == CHM_JDK_MOVED {
            if depth >= CHM_JDK_FORWARD_DEPTH {
                return false;
            }
            let Some(next_table_slot) = ctx.resolve_field_index_by_class_id(cid, "nextTable")
            else {
                return false;
            };
            let Value::Object(Some(next_table)) = ctx.get_field(node, next_table_slot) else {
                return false;
            };
            // The upper half is visited whatever the lower one answered.
            let lower = chm_jdk_bin_walk(ctx, next_table, index, depth + 1, cache, visit);
            let upper = chm_jdk_bin_walk(ctx, next_table, index + n, depth + 1, cache, visit);
            return lower && upper;
        } else if hash == CHM_JDK_RESERVED {
            return true;
        } else {
            return false;
        }
        match ctx.get_field(node, next_slot) {
            Value::Object(Some(next)) => node = next,
            _ => return true,
        }
    }
    false
}

/// The mappings of `this` read off the JDK's table, when the VM's CHM model is
/// the JDK's ([`chm_jdk_model`]); `None` otherwise (the segmented collectors
/// answer). A walk that could not read every bin answers what it read.
pub(crate) fn chm_jdk_collect(ctx: &dyn NativeContext, this: ObjectRef) -> Option<Vec<(Value, Value)>> {
    if !chm_jdk_model(ctx) {
        return None;
    }
    let mut out = Vec::new();
    let _whole = chm_jdk_table_walk(ctx, this, &mut |key, value| out.push((key, value)));
    Some(out)
}

/// A generic `java/util/Map` native reached with a `ConcurrentHashMap`
/// receiver in a JDK-model VM ([`chm_jdk_model`]): run the receiver's own
/// `name` / `descriptor` (the JDK's bytecode, since no CHM native is
/// registered in such a VM) with the `arity` arguments after the receiver
/// (`args[1..=arity]`; a missing one is `null`, an extra one -- a shared body
/// such as `compute*` calling the `get` native with its own arguments -- is not
/// passed). `None` for every other receiver or VM, and the caller keeps its
/// route (the segmented CHM natives).
///
/// Since the nio21 dev merge this is also the `java/util/Map` interface door
/// of interpreter round i3 wave 76 (lane L1, `chm_door_runs_jdk_body`, which
/// the `ConcurrentMap` doors still use): it delegates whenever that one would
/// ([`chm_jdk_bodies`]: `--compatible` with the row, where no segmented store
/// exists whatever `CRATONVM_CHM_JDK_TABLE_READS` says) as well as under
/// [`chm_jdk_model`] (which adds `--jdk-only`).
pub(crate) fn chm_jdk_delegate(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    name: &str,
    descriptor: &str,
    arity: usize,
) -> Option<MethodCallResult> {
    let Some(Value::Object(Some(this))) = args.first() else {
        return None;
    };
    let this = *this;
    if !receiver_facts(&*ctx, this).has(CF_CHM) || !(chm_jdk_model(&*ctx) || chm_jdk_bodies(&*ctx))
    {
        return None;
    }
    let call: Vec<Value> = (1..=arity)
        .map(|i| args.get(i).copied().unwrap_or(Value::Object(None)))
        .collect();
    Some(ctx.invoke_virtual(this, name, descriptor, &call))
}

/// Collect all entries from all segments, reordered to match real JDK
/// `ConcurrentHashMap` iteration order (see `chm_reorder_by_virtual_bucket`).
pub(crate) fn chm_collect_all_entries(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<(Value, Value)> {
    if let Some(entries) = chm_jdk_collect(ctx, this) {
        return entries;
    }
    // Walk each segment's bucket table DIRECTLY (via `map_state` +
    // `get_node_key`/`get_node_value`), symmetrically with
    // `chm_collect_all_keys` / `chm_collect_all_values`. The previous
    // implementation delegated to `map_collect_entries(seg)`, which re-runs the
    // receiver-type predicates (`properties_backing_chm` / `is_chm_receiver` /
    // `is_lhm_receiver`) on the *segment*. When a segment is itself classified
    // as a bucket-backed map with no nested segments, that delegation returned
    // an empty vec — so `entrySet()` over a CHM-backed `Hashtable`/`Properties`
    // came back size 0 even though `keySet()`/`values()` (which walk segments
    // directly) were correct. That broke `javax.management.ObjectName`'s
    // Hashtable-constructor path (`new ObjectName(domain, table)` →
    // `entrySet()` iteration yielded nothing → null `Property` → NPE), which
    // Spring's JMX naming strategies rely on. Walking directly here keeps the
    // three collectors consistent.
    let mut entries = Vec::new();
    for seg in chm_all_segments(ctx, this) {
        let (buckets, _size, cap) = map_state(ctx, seg);
        if let Some(b) = buckets {
            for i in 0..(cap as usize) {
                let mut node_val = ctx.get_array_element(b, i);
                while let Value::Object(Some(node)) = node_val {
                    let hash = match ctx.get_field(node, NODE_FIELD_HASH) {
                        Value::Int(h) => h,
                        _ => 0,
                    };
                    let key = get_node_key(ctx, node);
                    let value = get_node_value(ctx, node);
                    // CHM-mapper-deadlock fix (2026-07-21) follow-up: a value
                    // that is the segment object itself is an in-flight
                    // `computeIfAbsent` RESERVATION, not a real mapping (see
                    // `chm_seg_get`). Exposing it here handed iterating user
                    // bytecode a bare cid=0 Object as the entry value —
                    // `(String) entry.getValue()` CCE under WildFly's
                    // parallel-extension-add.
                    let reserved = matches!(value, Value::Object(Some(v))
                        if std::ptr::eq(v.as_ptr(), seg.as_ptr()));
                    if !reserved {
                        entries.push((hash, (key, value)));
                    }
                    node_val = ctx.get_field(node, NODE_FIELD_NEXT);
                }
            }
        }
    }
    chm_reorder_by_virtual_bucket(ctx, this, entries)
}

/// Collect all keys from all segments, reordered to match real JDK
/// `ConcurrentHashMap` iteration order (see `chm_reorder_by_virtual_bucket`).
///
/// Uses the layout-aware `get_node_key` helper rather than a hardcoded
/// `NODE_FIELD_KEY` slot: CHM segment bucket nodes can use either the legacy
/// (key@0,val@1) or the JDK (hash@0,key@1,val@2) layout. Reading slot 0
/// blindly returned the `hash` int for JDK-layout nodes, so `keySet()` /
/// `values()` collected garbage that was then silently dropped — leaving the
/// view iterators empty even though `size()` was correct. `entrySet()` already
/// went through `get_node_key`/`get_node_value` and worked, which is why only
/// `keySet()`/`values()` were broken (observed via Felix's
/// `Felix.getServiceReferences`, whose `new ArrayList<>(set)` over a
/// `Collections.newSetFromMap(new ConcurrentHashMap())` came back empty).
pub(crate) fn chm_collect_all_keys(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<Value> {
    if let Some(entries) = chm_jdk_collect(ctx, this) {
        return entries.into_iter().map(|(key, _)| key).collect();
    }
    let mut keys = Vec::new();
    for seg in chm_all_segments(ctx, this) {
        let (buckets, _size, cap) = map_state(ctx, seg);
        if let Some(b) = buckets {
            for i in 0..(cap as usize) {
                let mut node_val = ctx.get_array_element(b, i);
                while let Value::Object(Some(node)) = node_val {
                    let hash = match ctx.get_field(node, NODE_FIELD_HASH) {
                        Value::Int(h) => h,
                        _ => 0,
                    };
                    // Skip in-flight `computeIfAbsent` reservations (value ==
                    // the segment object) — the JDK's ReservationNode is
                    // likewise invisible to key iteration. See
                    // `chm_collect_all_entries`.
                    let reserved = matches!(get_node_value(ctx, node), Value::Object(Some(v))
                        if std::ptr::eq(v.as_ptr(), seg.as_ptr()));
                    if !reserved {
                        keys.push((hash, get_node_key(ctx, node)));
                    }
                    node_val = ctx.get_field(node, NODE_FIELD_NEXT);
                }
            }
        }
    }
    chm_reorder_by_virtual_bucket(ctx, this, keys)
}

/// Collect all values from all segments, reordered to match real JDK
/// `ConcurrentHashMap` iteration order (see `chm_reorder_by_virtual_bucket`).
///
/// See `chm_collect_all_keys` — uses the layout-aware `get_node_value` helper
/// so JDK-layout bucket nodes (hash@0,key@1,val@2) are read correctly.
pub(crate) fn chm_collect_all_values(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<Value> {
    if let Some(entries) = chm_jdk_collect(ctx, this) {
        return entries.into_iter().map(|(_, value)| value).collect();
    }
    let mut vals = Vec::new();
    for seg in chm_all_segments(ctx, this) {
        let (buckets, _size, cap) = map_state(ctx, seg);
        if let Some(b) = buckets {
            for i in 0..(cap as usize) {
                let mut node_val = ctx.get_array_element(b, i);
                while let Value::Object(Some(node)) = node_val {
                    let hash = match ctx.get_field(node, NODE_FIELD_HASH) {
                        Value::Int(h) => h,
                        _ => 0,
                    };
                    let value = get_node_value(ctx, node);
                    // Skip in-flight `computeIfAbsent` reservations (value ==
                    // the segment object). See `chm_collect_all_entries`.
                    let reserved = matches!(value, Value::Object(Some(v))
                        if std::ptr::eq(v.as_ptr(), seg.as_ptr()));
                    if !reserved {
                        vals.push((hash, value));
                    }
                    node_val = ctx.get_field(node, NODE_FIELD_NEXT);
                }
            }
        }
    }
    chm_reorder_by_virtual_bucket(ctx, this, vals)
}

/// Initialize a CHM with segments.
/// [`chm_segment_for`] for a MUTATOR: builds the segment array on demand
/// rather than answering `None`.
///
/// # W7-96 §7 NOMINATION 2 — a dropped store reported as a successful insert
///
/// Every mutator used to end in
///
/// ```text
///     None => Ok(Some(Value::Object(None))),
/// ```
///
/// and on `put` that value means *"there was no previous mapping"* — i.e. a
/// FRESH INSERT. So a `ConcurrentHashMap` whose segments are absent answered
/// "stored, and it is new" to every `put` while storing nothing, and `size()`
/// stayed 0. W7-96 §3.1 measured six puts landing one entry, 3 of 3 runs
/// identical, and spent an hour reading it as a real-bytecode CAS bug.
///
/// A receiver reaches that state whenever `<init>` did not run this crate's
/// constructor: `readObject` (which already worked around it by calling
/// `chm_init_segments` itself), `Unsafe.allocateInstance`, and — the case that
/// made it matter — the retirement dial retiring `<init>` while the force gate
/// kept `put`.
///
/// Materialising is better than throwing. A segment-less CHM is not a
/// corrupt object, it is an UNINITIALISED one, and the JDK's own
/// `ConcurrentHashMap` allocates its table lazily on first `put` for exactly
/// the same reason. This makes the VM correct rather than merely loud, and it
/// removes the workaround `native_chm_read_object` had to carry.
///
/// READERS are deliberately NOT routed here: on a map with no segments there is
/// genuinely nothing to find, so `get`/`containsKey` answering absent is right,
/// and materialising on a read would allocate on a path that must not.
pub(crate) fn chm_segment_for_mut(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    hash: i32,
) -> Option<ObjectRef> {
    if let Some(seg) = chm_segment_for(&*ctx, this, hash) {
        return Some(seg);
    }
    if matches!(
        ctx.get_field(this, CHM_FIELD_SEGMENTS),
        Value::Object(Some(_))
    ) {
        // The array exists NOW -- but the `chm_segment_for` above may have read
        // the field before another thread's first insert installed it. That is
        // not "this bucket is empty": a published array never holds a null
        // segment. This used to `return None`, which every caller reads as "no
        // segment": `put` silently did nothing and `computeIfAbsent` returned
        // null. It was the residual of the lazy-install race below -- 6/2000
        // bad rounds after the install itself was serialised, 0/3000 once the
        // segments were installed before the threads started. Look again.
        return chm_segment_for(&*ctx, this, hash);
    }
    // `CHM_DEFAULT_INIT_SEGMENTS`, not `CHM_DEFAULT_SEGMENTS`: this is the
    // shape `native_chm_init_default` used to build eagerly, and since that
    // constructor now defers to this path it is the shape every ordinary
    // `new ConcurrentHashMap<>()` gets. The two disagreed -- 16 segments here
    // against the constructor's 4 -- which mattered for nothing while this
    // arm served only receivers no constructor had run on, and would have
    // quadrupled every default map's first table the moment it served them
    // all.
    //
    // It also makes two numbers agree that never did: `chm_total_capacity`
    // sums the live segment arrays (4 * 4 = 16) and `chm_initial_table`
    // reports 16 for an unrecorded receiver, and `chm_reorder_by_virtual_bucket`
    // reads both.
    //
    // PUBLISHED BY CAS, not by a store. Unlike the constructors and
    // `readObject`, this runs on a map other threads can already see, and two
    // first inserts can both find the field null. With a plain store each
    // built its own array and the later store won: the earlier thread had
    // already reserved its key in a segment of the LOSING array, so its commit
    // re-read the winner, found no reservation, took "a racing mutator
    // replaced our marker", and `computeIfAbsent` returned null -- the value
    // was never stored anywhere. MEASURED 2026-09-12: Tomcat's
    // `TimeBucketCounterBase.increment` (`map.computeIfAbsent(key, v -> new
    // AtomicInteger()).incrementAndGet()`, four client threads on a fresh map)
    // threw NullPointerException on a client's first request in 3 of 12
    // `RateLimitStallProbe` runs, which is `TestRateLimitFilter`'s
    // `expected:<200> but was:<0>`. A `put` racing the same way lost its entry.
    //
    // The install is a check-and-store under `CHM_SEGMENTS_INSTALL`. The
    // critical section is two field accesses and never allocates, so no thread
    // can reach a safepoint while holding it, and each map takes it once in its
    // life. Serialising the install took the failure rate from 1070/2000 rounds
    // to 6/2000; the rest was the early `return None` above, which answered "no
    // segment" when the install landed between this function's two reads.
    let this_pin = ctx.pin_native_root(this);
    let built = chm_build_segments(ctx, CHM_DEFAULT_INIT_SEGMENTS, CHM_DEFAULT_SEGMENT_CAP);
    let built_pin = ctx.pin_native_root(built);
    {
        let _install = CHM_SEGMENTS_INSTALL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let this = ctx.read_native_pin(this_pin, this);
        if !matches!(
            ctx.get_field(this, CHM_FIELD_SEGMENTS),
            Value::Object(Some(_))
        ) {
            let built = ctx.read_native_pin(built_pin, built);
            ctx.set_field(this, CHM_FIELD_SEGMENTS, Value::Object(Some(built)));
        }
        // Otherwise another thread installed first: its array is the map, and
        // ours -- which holds nothing -- is garbage.
    }
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    chm_segment_for(&*ctx, this, hash)
}

/// Serialises the lazy segments install of [`chm_segment_for_mut`]. One lock
/// for every map: it is held for a field read and a field store, and a map
/// takes it once.
pub(crate) static CHM_SEGMENTS_INSTALL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Build and install a segments array on a receiver no other thread can see
/// yet (a constructor, `readObject`, a native-built map). A map that is
/// already shared must go through [`chm_segment_for_mut`], which publishes
/// with a CAS.
pub(crate) fn chm_init_segments(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    num_segments: usize,
    cap_per_segment: usize,
) {
    let this_pin = ctx.pin_native_root(this);
    let segments = chm_build_segments(ctx, num_segments, cap_per_segment);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, CHM_FIELD_SEGMENTS, Value::Object(Some(segments)));
    ctx.unpin_native_roots(this_pin);
    // NOTE: the segment count/mask is intentionally NOT persisted to a field.
    // Slot 1 of a real-JDK ConcurrentHashMap is reference-typed, so an `Int`
    // write there is descriptor-coerced to `Object(None)` and lost. Readers
    // (`chm_segment_for`, `chm_all_segments`) derive the mask/count from the
    // segments array length, which is always a power of two.
}

/// The segment count for a map whose modelled JDK table has `table` buckets,
/// under [`chm_jdk25_sizing_enabled`]: no more than `table` (floored at the
/// default map's [`CHM_DEFAULT_INIT_SEGMENTS`]), else `segments` unchanged.
///
/// Why: [`chm_reorder_by_virtual_bucket`] keeps two keys of one virtual bucket
/// in collection order, which is their INSERTION order only while they share
/// one segment chain -- true when the segment index bits (`hash & (n - 1)`)
/// are a subset of the virtual bucket bits. With 16 segments under an 8-bucket
/// table (`ConcurrentHashMap(Map)` of 5 entries) the keys 8 and 16 share
/// bucket 0 but sit in segments 8 and 0, and came out `16,8` where HotSpot
/// appends `8,16` (round 15 wave 5 lane compat6, `R15Compat6ChmSizing` row 5).
/// The floor costs nothing: a table under 4 buckets holds at most one entry
/// (two entries already need 4), so it has no ties; and the modelled table
/// only grows, so the subset relation holds for the map's life. (The
/// per-segment table stays no larger than the virtual one: a segment past
/// `0.75 * virtual` entries would hold more than the whole map.)
pub(crate) fn chm_segments_for_table(segments: usize, table: usize) -> usize {
    if chm_jdk25_sizing_enabled() {
        segments.min(table.max(CHM_DEFAULT_INIT_SEGMENTS))
    } else {
        segments
    }
}

/// [`chm_segments_for_table`] for a constructor that installed
/// [`CHM_DEFAULT_SEGMENTS`] before the table was known: rebuild the STILL
/// EMPTY segment array with the fitted count. Allocates (pins `this` itself);
/// the caller re-reads every reference it holds.
pub(crate) fn chm_fit_segments_to_table(ctx: &mut dyn NativeContext, this: ObjectRef, table: usize) {
    let n = chm_segments_for_table(CHM_DEFAULT_SEGMENTS, table);
    if n < CHM_DEFAULT_SEGMENTS {
        chm_init_segments(ctx, this, n, CHM_DEFAULT_SEGMENT_CAP);
    }
}

/// A fresh, empty segments array: `num_segments` segments of
/// `cap_per_segment` buckets each. Not installed anywhere.
pub(crate) fn chm_build_segments(
    ctx: &mut dyn NativeContext,
    num_segments: usize,
    cap_per_segment: usize,
) -> ObjectRef {
    // cceres5: the 2N+1 allocations below can each trigger a moving GC; the
    // segments array and each fresh `seg` were carried raw across them
    // (stale-array element stores). Pin + re-read.
    let segments = alloc_ref_array(ctx, num_segments);
    let segments_pin = ctx.pin_native_root(segments);
    for i in 0..num_segments {
        let seg = ctx.alloc_object(ClassId::new(0), MAP_NUM_FIELDS);
        let seg_pin = ctx.pin_native_root(seg);
        let buckets = alloc_ref_array(ctx, cap_per_segment);
        let seg = ctx.read_native_pin(seg_pin, seg);
        // Segments are bare `ClassId(0)` three-slot objects — the fabricated
        // layout — so this keeps writing `Int(capacity)` at slot 2.
        publish_map_table(ctx, seg, buckets, cap_per_segment as i32);
        set_map_size(ctx, seg, 0);
        let segments = ctx.read_native_pin(segments_pin, segments);
        let _ = ctx.set_array_element(segments, i, Value::Object(Some(seg)));
        ctx.unpin_native_roots(seg_pin);
    }
    let segments = ctx.read_native_pin(segments_pin, segments);
    ctx.unpin_native_roots(segments_pin);
    segments
}

/// `ConcurrentHashMap.writeObject(ObjectOutputStream)`.
///
/// CratonVM keeps a `ConcurrentHashMap`'s entries in the segmented native
/// layout rooted at `CHM_FIELD_SEGMENTS`, NOT in the real JDK's `table` field
/// (which stays null for the object's whole life). The JDK's own
/// `writeObject` walks `table` with a `Traverser`, so it found nothing and
/// serialised EVERY `ConcurrentHashMap` as EMPTY -- `deser(ser(chm)).size()`
/// was 0 for a two-entry map, while `HashMap` (whose natives do maintain the
/// real bucket layout) round-tripped fine.
///
/// Real-world fallout: Spring's `PersistenceAnnotationBeanPostProcessor`
/// tracks extended `EntityManager`s to close in a
/// `ConcurrentHashMap<Object, EntityManager>`. Serialising a `SimpleMapScope`
/// carries that post-processor along; after deserialisation the map came back
/// empty, so `postProcessBeforeDestruction` found no EM to close and
/// `PersistenceInjectionTests.publicExtendedPersistenceContextSetterWith\
/// Serialization` failed on `assertThat(DummyInvocationHandler.closed).isTrue()`.
///
/// This emits the JDK's serial form byte-for-byte compatibly: the three
/// `serialPersistentFields` (`segments`, `segmentShift`, `segmentMask`),
/// then alternating key/value objects, then two nulls as the terminator.
/// `segments` is written as null -- the JDK writes a freshly built,
/// entry-free `Segment[16]` there purely for pre-Java-8 stream compatibility
/// and its own `readObject` discards the value without dereferencing it, so a
/// null is accepted by both HotSpot's reader and ours.
pub(crate) fn native_chm_write_object(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let stream = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let this_pin = ctx.pin_native_root(this);
    let stream_pin = ctx.pin_native_root(stream);

    let result = (|| -> MethodCallResult {
        // --- serialPersistentFields, in the JDK's declaration order.
        let stream = ctx.read_native_pin(stream_pin, stream);
        let put_field = match ctx.invoke_virtual(
            stream,
            "putFields",
            "()Ljava/io/ObjectOutputStream$PutField;",
            &[],
        )? {
            Some(Value::Object(Some(pf))) => pf,
            _ => return Ok(None),
        };
        let pf_pin = ctx.pin_native_root(put_field);
        for (name, value) in [
            ("segments", Value::Object(None)),
            // DEFAULT_CONCURRENCY_LEVEL is 16 => ssize 16, sshift 4.
            ("segmentShift", Value::Int(32 - 4)),
            ("segmentMask", Value::Int(16 - 1)),
        ] {
            let name_obj = ctx.create_string(name);
            let put_field = ctx.read_native_pin(pf_pin, put_field);
            let desc = if matches!(value, Value::Int(_)) {
                "(Ljava/lang/String;I)V"
            } else {
                "(Ljava/lang/String;Ljava/lang/Object;)V"
            };
            ctx.invoke_virtual(
                put_field,
                "put",
                desc,
                &[Value::Object(Some(name_obj)), value],
            )?;
        }
        ctx.unpin_native_roots(pf_pin);
        let stream = ctx.read_native_pin(stream_pin, stream);
        ctx.invoke_virtual(stream, "writeFields", "()V", &[])?;

        // --- key/value pairs, then the two-null terminator.
        let this = ctx.read_native_pin(this_pin, this);
        let entries = collect_entries_any(ctx, this)?;
        let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
        let (_, flat_pins) = pin_value_slice(ctx, &flat);
        for i in 0..flat.len() {
            let v = read_pinned_elem(ctx, flat_pins[i], flat[i]);
            let stream = ctx.read_native_pin(stream_pin, stream);
            ctx.invoke_virtual(stream, "writeObject", "(Ljava/lang/Object;)V", &[v])?;
        }
        for _ in 0..2 {
            let stream = ctx.read_native_pin(stream_pin, stream);
            ctx.invoke_virtual(
                stream,
                "writeObject",
                "(Ljava/lang/Object;)V",
                &[Value::Object(None)],
            )?;
        }
        Ok(None)
    })();

    ctx.unpin_native_roots(this_pin);
    result
}

/// `ConcurrentHashMap.readObject(ObjectInputStream)` -- the read side of
/// [`native_chm_write_object`].
///
/// The JDK body rebuilds the real `table`, which our `get`/`size`/`entrySet`
/// natives never look at, so even a correctly written stream would have
/// deserialised to a map that reads as empty. Read the same wire form and
/// funnel every pair through the segmented native layout instead.
///
/// The stream allocates the instance without running any `<init>`, so the
/// segments array is absent on entry and has to be built before the first
/// `put` (`chm_segment_for` would otherwise answer `None` and silently drop
/// every entry).
pub(crate) fn native_chm_read_object(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let stream = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let this_pin = ctx.pin_native_root(this);
    let stream_pin = ctx.pin_native_root(stream);

    let result = (|| -> MethodCallResult {
        let stream = ctx.read_native_pin(stream_pin, stream);
        ctx.invoke_virtual(stream, "defaultReadObject", "()V", &[])?;

        let this = ctx.read_native_pin(this_pin, this);
        if !matches!(
            ctx.get_field(this, CHM_FIELD_SEGMENTS),
            Value::Object(Some(_))
        ) {
            chm_init_segments(
                ctx,
                this,
                CHM_DEFAULT_INIT_SEGMENTS,
                CHM_DEFAULT_SEGMENT_CAP,
            );
        }

        loop {
            let stream = ctx.read_native_pin(stream_pin, stream);
            let key = match ctx.invoke_virtual(stream, "readObject", "()Ljava/lang/Object;", &[])? {
                Some(v @ Value::Object(Some(_))) => v,
                _ => break,
            };
            let key_pin = pin_value(ctx, key);
            let stream = ctx.read_native_pin(stream_pin, stream);
            let val = match ctx.invoke_virtual(stream, "readObject", "()Ljava/lang/Object;", &[])? {
                Some(v @ Value::Object(Some(_))) => v,
                _ => {
                    ctx.unpin_native_roots(key_pin);
                    break;
                }
            };
            let key = read_pinned_elem(ctx, key_pin, key);
            let this = ctx.read_native_pin(this_pin, this);
            native_chm_put(ctx, &[Value::Object(Some(this)), key, val])?;
            ctx.unpin_native_roots(key_pin);
        }
        Ok(None)
    })();

    ctx.unpin_native_roots(this_pin);
    result
}

// ---------------------------------------------------------------------------
// The `JdkConcurrentHashMap` row (interpreter round i3 wave 76, lane L1;
// `docs/feature-designs/collections-interception.md`).
//
// With the row in force (`--compatible` over a real JDK,
// `CRATONVM_COMPAT_JDK_CONCURRENT_HASH_MAP=1`, default OFF) every
// `ConcurrentHashMap` is the JDK's: its entries live in the JDK's `table`, and
// the segmented store below (hung off `AbstractMap.keySet`, slot 0) is never
// built. So:
//
// * the cluster's registrations are not made (`chm_row_registers_nothing`:
//   this file's CHM registrar and carrier loops, `native-builtins`'
//   `register_m18_concurrent_fixes`; the `COMPAT_FAMILIES` entry takes the
//   retired rows of any other registrar);
// * a `java.util.Map` / `ConcurrentMap` interface door that meets a CHM
//   receiver runs the receiver's own body (`chm_door_runs_jdk_body`) instead of
//   a segmented native that would answer "empty";
// * the bulk collectors read the JDK's `table` (`chm_jdk_table_entries`).
//
// Under `--jdk-only` none of this changes anything (its registrations are
// decided by the retired-shadow tables, and these helpers answer `false`).
// ---------------------------------------------------------------------------

/// Is the `JdkConcurrentHashMap` row in force for this registry: `--compatible`
/// over a real JDK with the row? Registration-time (cold).
pub(crate) fn chm_row_registers_nothing(r: &NativeMethodRegistry) -> bool {
    r.compatibility_mode() == cratonvm_types::compat::CompatibilityMode::Compatible
        && r.aligned(cratonvm_types::HotSpotAlignment::JdkConcurrentHashMap)
}

/// [`chm_row_registers_nothing`] at run time: `--compatible` with the row in
/// force (one per-VM registry read). Asked only on a CHM receiver's path.
#[inline]
pub(crate) fn chm_jdk_bodies(ctx: &dyn NativeContext) -> bool {
    !ctx.is_jdk_only() && ctx.aligned(cratonvm_types::HotSpotAlignment::JdkConcurrentHashMap)
}

/// A `java.util.Map` / `java.util.concurrent.ConcurrentMap` interface door
/// reached with a `ConcurrentHashMap` receiver while the row is in force: run
/// the receiver's own `method` bytecode (the receiver's class's when it
/// declares one, else `ConcurrentHashMap`'s), bytecode-only so the call cannot
/// come back to this door. `args[0]` is the receiver. `None` (run the door's
/// own body) when the row is not in force or the receiver is not a CHM.
pub(crate) fn chm_door_runs_jdk_body(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
) -> Option<MethodCallResult> {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    if !is_chm_receiver(ctx, this) || !chm_jdk_bodies(ctx) {
        return None;
    }
    let cid = ctx.class_id_of_object(this);
    let owner = if ctx.class_declares_method(cid, method, descriptor) {
        ctx.class_name_of_id(cid)
            .unwrap_or_else(|| CHM_CLASS.to_string())
    } else {
        CHM_CLASS.to_string()
    };
    Some(ctx.invoke_special_bytecode_only(&owner, method, descriptor, args))
}

/// `ConcurrentHashMap.TREEBIN`: the hash of a `TreeBin` bin head.
pub(crate) const CHM_JDK_TREEBIN_HASH: i32 = -2;
/// `ConcurrentHashMap.MOVED`: the hash of a `ForwardingNode` (a resize in
/// progress moved this bin to `nextTable`).
pub(crate) const CHM_JDK_MOVED_HASH: i32 = -1;

/// The slot indices [`chm_jdk_table_entries`] reads, resolved by name on the
/// JDK's classes (`None` when the image has no such class loaded yet: then
/// no node of it exists either).
pub(crate) struct ChmJdkNodeSlots {
    pub(crate) hash: usize,
    pub(crate) key: usize,
    pub(crate) val: usize,
    pub(crate) next: usize,
    pub(crate) tree_first: Option<usize>,
    pub(crate) next_table: Option<usize>,
}

impl ChmJdkNodeSlots {
    fn resolve(ctx: &dyn NativeContext) -> Option<ChmJdkNodeSlots> {
        Some(ChmJdkNodeSlots {
            hash: ctx.resolve_field_index(CHM_NODE_CLASS, "hash")?,
            key: ctx.resolve_field_index(CHM_NODE_CLASS, "key")?,
            val: ctx.resolve_field_index(CHM_NODE_CLASS, "val")?,
            next: ctx.resolve_field_index(CHM_NODE_CLASS, "next")?,
            tree_first: ctx
                .resolve_field_index("java/util/concurrent/ConcurrentHashMap$TreeBin", "first"),
            next_table: ctx.resolve_field_index(
                "java/util/concurrent/ConcurrentHashMap$ForwardingNode",
                "nextTable",
            ),
        })
    }
}

/// Every `(key, value)` of a JDK-built `ConcurrentHashMap`, read from its
/// `table` in the order the JDK's `Traverser` visits a quiescent map: bins in
/// index order, a bin's chain in `next` order, a `TreeBin` by its `first` list,
/// a forwarded bin (a resize in progress) from bins `i` and `i + n` of
/// `nextTable`; a `ReservationNode` holds nothing. Reads only (no allocation,
/// no Java), so the references it returns are live when it returns. Used by the
/// bulk collectors under the row, where no CHM has a segmented store.
pub(crate) fn chm_jdk_table_entries(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<(Value, Value)> {
    let mut out = Vec::new();
    let Some(table_slot) = ctx.resolve_field_index(CHM_CLASS, "table") else {
        return out;
    };
    let Some(slots) = ChmJdkNodeSlots::resolve(ctx) else {
        return out;
    };
    let tab = match ctx.get_field(this, table_slot) {
        Value::Object(Some(tab)) => tab,
        _ => return out,
    };
    let n = ctx.array_length(tab);
    for i in 0..n {
        chm_jdk_walk_bin(ctx, &slots, tab, n, i, 0, &mut out);
    }
    out
}

/// One bin of [`chm_jdk_table_entries`]. `depth` bounds the forwarding chain.
pub(crate) fn chm_jdk_walk_bin(
    ctx: &dyn NativeContext,
    slots: &ChmJdkNodeSlots,
    tab: ObjectRef,
    n: usize,
    i: usize,
    depth: u32,
    out: &mut Vec<(Value, Value)>,
) {
    // A chain longer than this is a corrupt (cyclic) read, not a map: stop.
    const CHAIN_LIMIT: usize = 1 << 26;
    let mut steps = 0usize;
    let mut e = ctx.get_array_element(tab, i);
    while let Value::Object(Some(node)) = e {
        steps += 1;
        if steps > CHAIN_LIMIT {
            return;
        }
        let hash = match ctx.get_field(node, slots.hash) {
            Value::Int(h) => h,
            _ => return,
        };
        if hash >= 0 {
            out.push((
                ctx.get_field(node, slots.key),
                ctx.get_field(node, slots.val),
            ));
            e = ctx.get_field(node, slots.next);
            continue;
        }
        if hash == CHM_JDK_TREEBIN_HASH {
            if let Some(first) = slots.tree_first {
                let mut t = ctx.get_field(node, first);
                let mut tree_steps = 0usize;
                while let Value::Object(Some(tn)) = t {
                    tree_steps += 1;
                    if tree_steps > CHAIN_LIMIT {
                        return;
                    }
                    out.push((ctx.get_field(tn, slots.key), ctx.get_field(tn, slots.val)));
                    t = ctx.get_field(tn, slots.next);
                }
            }
            return;
        }
        if hash == CHM_JDK_MOVED_HASH && depth < 8 {
            if let Some(nt_slot) = slots.next_table {
                if let Value::Object(Some(next_tab)) = ctx.get_field(node, nt_slot) {
                    let m = ctx.array_length(next_tab);
                    if m == 2 * n {
                        chm_jdk_walk_bin(ctx, slots, next_tab, m, i, depth + 1, out);
                        chm_jdk_walk_bin(ctx, slots, next_tab, m, i + n, depth + 1, out);
                    }
                }
            }
        }
        // A `ReservationNode` (or a forwarding chain past the bound).
        return;
    }
}

/// [`map_collect_entries`]' answer for a CHM under the row (see
/// [`chm_jdk_table_entries`]); `None` when the row is not in force.
#[inline]
pub(crate) fn chm_jdk_entries_if_row(ctx: &dyn NativeContext, chm: ObjectRef) -> Option<Vec<(Value, Value)>> {
    if chm_jdk_bodies(ctx) {
        Some(chm_jdk_table_entries(ctx, chm))
    } else {
        None
    }
}

/// The five `java/util/concurrent/ConcurrentMap` interface rows under the row:
/// a CHM receiver runs its own body, any other receiver (a class that declares
/// none of these, i.e. a VM-fabricated one) keeps the segmented native.
pub(crate) fn native_chm_door_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = chm_door_runs_jdk_body(ctx, args, "size", "()I") {
        return r;
    }
    native_chm_size(ctx, args)
}

pub(crate) fn native_chm_door_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) =
        chm_door_runs_jdk_body(ctx, args, "get", "(Ljava/lang/Object;)Ljava/lang/Object;")
    {
        return r;
    }
    native_chm_get(ctx, args)
}

pub(crate) fn native_chm_door_put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = chm_door_runs_jdk_body(
        ctx,
        args,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ) {
        return r;
    }
    native_chm_put(ctx, args)
}

pub(crate) fn native_chm_door_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = chm_door_runs_jdk_body(
        ctx,
        args,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ) {
        return r;
    }
    native_chm_remove(ctx, args)
}

pub(crate) fn native_chm_door_contains_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = chm_door_runs_jdk_body(ctx, args, "containsKey", "(Ljava/lang/Object;)Z") {
        return r;
    }
    native_chm_contains_key(ctx, args)
}

/// CLUSTER NOTE (H4-1, 2026-08-20) — **this is the map/set family closest to
/// being movable, and the exact list of what still blocks it.**
///
/// Its carriers are minted by nothing else: `ConcurrentHashMap$KeySetView`
/// only by `alloc_key_set_view_object` here, `$ValuesView` only by
/// [`native_chm_values`], `$EntrySetView` only by [`native_chm_entry_set`], and
/// its iterators go through those views rather than through
/// [`MAP_KEY_ITR_CARRIERS`]. It also has the one `java.util` container whose
/// real JDK constructor body is EMPTY, so a `try_alloc_concurrent_synthetic`
/// object under the real class id is a legitimate freshly-constructed
/// `ConcurrentHashMap` to real bytecode (contrast `HashMap()`, which must set
/// `loadFactor`). So the cluster is `ConcurrentHashMap` + the three views +
/// `register_chm_key_set_view_natives`, and it is closed under minting.
///
/// Three things must land in the SAME commit as any retag of it:
///
/// 1. **Keep the `java/util/concurrent/ConcurrentMap` rows `Bridge`.** They are
///    at the tail of this function (`size`/`get`/`put`/`remove`/`containsKey`)
///    and they are INTERFACE registrations: they answer the no-`Code` door for
///    receivers whose own class declares nothing, which is not a
///    ConcurrentHashMap question. Refusing them turns that door into an
///    `AbstractMethodError`. Wrap them in an inner
///    `r.with_category(NativeKind::Bridge, …)` window.
/// 2. **`vm/src/jit/helpers.rs`.** `jit_concurrent_hashmap_get_direct` calls
///    [`native_chm_get`] as a plain Rust function from compiled code, past
///    dispatch and past the kind check. After the retag the interpreter would
///    read the real `table` and the JIT the (now unwritten) side segments, so
///    a hot loop would start answering null for keys the map holds — a
///    divergence no arm asks about, because `run.sh` does not diff tiers.
///    The bind site must be gated on the policy, or the helper deleted.
/// 3. **A `--dump-native-registry` diff** proving all four class rows moved
///    from `bridge` to `synthetic-stub` and that `ConcurrentMap`'s five did
///    not (`G85-1`: an inner `set_category` silently overrides a call-site
///    wrapper, and six retags written that way were inert).
///
/// Pre-flight it for free on the CURRENT binary before building anything:
/// `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/concurrent/ConcurrentHashMap`
/// over the 36-vector screen (`HANDOFF-20260819.md` §2). The dial yields to
/// bytecode where bytecode exists, which is the same verdict a refusal gives
/// for every concrete row here; it differs only on the abstract-interface
/// rows, i.e. exactly the ones (1) says to keep.
pub(crate) fn register_concurrent_hashmap_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/util/concurrent/ConcurrentHashMap";

    // The `JdkConcurrentHashMap` row (interpreter round i3 wave 76, lane L1;
    // default off): over a real JDK under `--compatible`, none of this
    // cluster's rows is registered -- the class, its views and iterators, and
    // `register_chm_key_set_view_natives` -- so the JDK's bodies run over the
    // JDK's `table`, as under `--jdk-only`. The five `ConcurrentMap`
    // INTERFACE rows stay (item 1 of the cluster note above), registered as
    // doors that hand a CHM receiver to its own body; stated `Bridge` (the
    // `set_category` above).
    if chm_row_registers_nothing(r) {
        let cm = "java/util/concurrent/ConcurrentMap";
        r.register(cm, "size", "()I", native_chm_door_size);
        r.register(
            cm,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            native_chm_door_get,
        );
        r.register(
            cm,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            native_chm_door_put,
        );
        r.register(
            cm,
            "remove",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            native_chm_door_remove,
        );
        r.register(
            cm,
            "containsKey",
            "(Ljava/lang/Object;)Z",
            native_chm_door_contains_key,
        );
        r.set_category(__prev_cat);
        return;
    }

    // Constructors
    r.register(c, "<init>", "()V", native_chm_init_default);
    r.register(c, "<init>", "(I)V", native_chm_init_capacity);
    r.register(c, "<init>", "(IFI)V", native_chm_init_full);
    r.register(c, "<init>", "(Ljava/util/Map;)V", native_chm_init_from_map);

    // Java serialization -- the real JDK bodies walk/rebuild the `table`
    // field that CratonVM's segmented layout never populates, so a CHM
    // round-tripped through ObjectOutputStream came back EMPTY. See
    // `native_chm_write_object`. Both are private and reached through
    // `ObjectStreamClass.invokeWriteObject`/`invokeReadObject`'s reflective
    // `Method.invoke`, so they also need the
    // `force_native_over_real_jdk_bytecode` entry (interpreter.rs) to beat
    // the real bytecode.
    r.register(
        c,
        "writeObject",
        "(Ljava/io/ObjectOutputStream;)V",
        native_chm_write_object,
    );
    r.register(
        c,
        "readObject",
        "(Ljava/io/ObjectInputStream;)V",
        native_chm_read_object,
    );

    // Core operations — segmented
    r.register(c, "size", "()I", native_chm_size);
    r.register(c, "isEmpty", "()Z", native_chm_is_empty);
    r.register(
        c,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_put,
    );
    r.register(
        c,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_get,
    );
    r.register(
        c,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_remove,
    );
    r.register(
        c,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_chm_contains_key,
    );
    r.register(
        c,
        "containsValue",
        "(Ljava/lang/Object;)Z",
        native_chm_contains_value,
    );
    r.register(c, "clear", "()V", native_chm_clear);
    r.register(c, "keySet", "()Ljava/util/Set;", native_chm_key_set);
    r.register(c, "values", "()Ljava/util/Collection;", native_chm_values);
    r.register(c, "entrySet", "()Ljava/util/Set;", native_chm_entry_set);
    // `ConcurrentHashMap.keySet()` has a covariant return type: the real
    // method's descriptor is `()L...$KeySetView;`, and `()Ljava/util/Set;`
    // is only the synthetic bridge. `javac` emits an invokevirtual against
    // the *covariant* descriptor when the static receiver type is
    // `ConcurrentHashMap`, so without this registration `keySet()` fell
    // through to the real-JDK bytecode (`new KeySetView(this, null)`), whose
    // iterator walks the unpopulated `table` field and yields nothing. This
    // broke `new ArrayList<>(Collections.newSetFromMap(new ConcurrentHashMap()))`
    // — observed as Felix `getServiceReference` returning null because
    // `Felix.getServiceReferences` copies the CHM-backed result set.
    r.register(
        c,
        "keySet",
        "()Ljava/util/concurrent/ConcurrentHashMap$KeySetView;",
        native_chm_key_set,
    );
    r.register(c, "toString", "()Ljava/lang/String;", native_chm_to_string);
    r.register(
        c,
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_get_or_default,
    );
    r.register(
        c,
        "putIfAbsent",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_put_if_absent,
    );
    r.register(c, "putAll", "(Ljava/util/Map;)V", native_chm_put_all);
    r.register(c, "hashCode", "()I", native_chm_hash_code);
    r.register(c, "equals", "(Ljava/lang/Object;)Z", native_chm_equals);
    r.register(
        c,
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        native_chm_for_each,
    );
    r.register(
        c,
        "computeIfAbsent",
        "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;",
        native_chm_compute_if_absent,
    );
    r.register(
        c,
        "compute",
        "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        native_chm_compute,
    );
    r.register(
        c,
        "computeIfPresent",
        "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        native_chm_compute_if_present,
    );
    r.register(
        c,
        "merge",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        native_chm_merge,
    );
    r.register(
        c,
        "replaceAll",
        "(Ljava/util/function/BiFunction;)V",
        native_chm_replace_all,
    );

    // ConcurrentHashMap-specific methods
    r.register(
        c,
        "remove",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
        native_chm_remove_kv,
    );
    r.register(
        c,
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_replace,
    );
    r.register(
        c,
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z",
        native_chm_replace_kv,
    );
    r.register(
        c,
        "forEach",
        "(JLjava/util/function/BiConsumer;)V",
        native_chm_for_each_parallel,
    );
    // The two bulk operations the round-2 differential measured. Every method
    // in this family walks `table` through a `Traverser`, and a natively-backed
    // CHM keeps its entries in a SEGMENTED layout that never populates `table`
    // — so unregistered, they run real bytecode over an empty tree and answer
    // `null`. See `native_chm_reduce_values` for why that answer is the worst
    // possible one. The rest of the family is still unregistered; the list is in
    // W7-36-differential-view-families.
    r.register(
        c,
        "reduceValues",
        "(JLjava/util/function/BiFunction;)Ljava/lang/Object;",
        native_chm_reduce_values,
    );
    r.register(
        c,
        "searchKeys",
        "(JLjava/util/function/Function;)Ljava/lang/Object;",
        native_chm_search_keys,
    );
    r.register(c, "mappingCount", "()J", native_chm_mapping_count);
    r.register(
        c,
        "newKeySet",
        "()Ljava/util/concurrent/ConcurrentHashMap$KeySetView;",
        native_chm_new_key_set,
    );
    r.register(
        c,
        "newKeySet",
        "(I)Ljava/util/concurrent/ConcurrentHashMap$KeySetView;",
        native_chm_new_key_set_cap,
    );
    r.register(
        c,
        "keySet",
        "(Ljava/lang/Object;)Ljava/util/concurrent/ConcurrentHashMap$KeySetView;",
        native_chm_key_set_view,
    );
    r.register(
        c,
        "contains",
        "(Ljava/lang/Object;)Z",
        native_chm_contains_value,
    );
    r.register(
        c,
        "elements",
        "()Ljava/util/Enumeration;",
        native_chm_elements,
    );
    r.register(c, "keys", "()Ljava/util/Enumeration;", native_chm_keys);

    // `ConcurrentHashMap$ValueIterator`'s ENUMERATION half.
    //
    // `elements()` hands back a snapshot carrier of this class (see
    // `native_chm_elements`), whose `hasNext`/`next` this file owns through
    // `VALUES_ITR_CARRIERS`. That is not enough, and the reason is worth
    // stating because it is the second time this family has been caught by it:
    // `BaseIterator.hasMoreElements()` is NOT `return hasNext();` — it is
    //
    // ```text
    //   public final boolean hasMoreElements() { return next != null; }
    // ```
    //
    // a DIRECT read of the `next` field, so it never reaches the native at all
    // and answered false over a perfectly good snapshot. MEASURED: `elements()`
    // drained EMPTY on a three-entry map while `values().iterator()` — the same
    // carrier, entered through `hasNext` — drained all three. An empty
    // enumeration is a quieter wrong answer than the non-terminating one it
    // replaced, which is exactly why it needed measuring rather than reading.
    //
    // `nextElement()` IS `return next();` and would have worked through the
    // existing registration; it is registered anyway so the pair cannot drift.
    r.register(
        CHM_VALUE_ITER_CLASS,
        "hasMoreElements",
        "()Z",
        native_al_itr_has_next,
    );
    r.register(
        CHM_VALUE_ITER_CLASS,
        "nextElement",
        "()Ljava/lang/Object;",
        native_al_itr_next,
    );
    // And the KEY iterator's, which `keys()` hands back. Its snapshot lives at
    // `key_itr_base` rather than `al_itr_alt_base`, so it takes the
    // `MAP_KEY_ITR_CARRIERS` bodies — the two families are not
    // interchangeable, and swapping them reads every field one slot wrong.
    r.register(
        CHM_KEY_ITER_CLASS,
        "hasMoreElements",
        "()Z",
        native_map_key_itr_has_next,
    );
    r.register(
        CHM_KEY_ITER_CLASS,
        "nextElement",
        "()Ljava/lang/Object;",
        native_map_key_itr_next,
    );

    // The `newKeySet()` / `keySet(V)` product's own surface — see the module
    // comment above `KSV_CLASS`.
    register_chm_key_set_view_natives(r);

    // Bulk operations — snapshot under per-segment locks
    r.register(
        c,
        "forEachEntry",
        "(JLjava/util/function/Consumer;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let action = match args.get(2) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(None),
            };
            let entries = chm_collect_all_entries(ctx, this);
            // `this` is the entries' source map and is handed to every carrier
            // below, so it has to survive the per-iteration allocation. It was
            // previously read once and never used after the first allocation,
            // which was only safe because nothing consumed it.
            let this_pin = ctx.pin_native_root(this);
            let action_pin = ctx.pin_native_root(action);
            let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
            let (_, flat_pins) = pin_value_slice(ctx, &flat);
            for i in 0..entries.len() {
                let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
                let value = read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]);
                let source = ctx.read_native_pin(this_pin, this);
                // The carrier handed to the `Consumer` MUST be a real
                // `Map.Entry`. This line used to be the untyped-allocation
                // sentinel at width `NODE_NUM_FIELDS` — spelled out here would
                // be a FALSE POSITIVE for `scripts/untyped-alloc-ratchet.sh`,
                // which greps source text and cannot tell a comment from a call
                // (`H16-1` §5) — with the key and value written at the NODE
                // slots (1 and 2), which produced
                // a `cratonvm/synthetic/AnonymousObject$4` — an object that
                // implements nothing. MEASURED on `cratonvm-r5.exe`,
                // `--jdk-only`, UNARMED, against HotSpot 25.0.3+9 (H16-3):
                //
                //   HotSpot   entry cls=java.util.concurrent.ConcurrentHashMap$Node
                //             key=k1 val=v1 … visited=2
                //   CratonVM  ClassCastException: class
                //             cratonvm.synthetic.AnonymousObject$4 cannot be cast
                //             to class java.util.Map$Entry … after 0
                //
                // so `forEachEntry` was unusable for any consumer that does
                // anything with its argument — which the parameter type makes
                // every consumer. It also left slot 0 (`hash`) and slot 3
                // (`next`) unwritten, so it was not even a well-formed node.
                //
                // `AbstractMap$SimpleEntry` with `source = this` is what every
                // other entry-yielding path in this file already mints
                // (`collect_entries_any`, `tm_make_entry`'s live twin), it is a
                // real `Map.Entry` so reflection and `checkcast` both work, and
                // the live-entry slot 2 gives `setValue` the write-through that
                // HotSpot's `$Node` argument also has. It is NOT a
                // `ConcurrentHashMap$Node`: a consumer that downcasts to the
                // concrete node class still fails, and that is a smaller and
                // louder gap than the one it replaces.
                let entry = match alloc_live_entry(
                    ctx,
                    "java/util/AbstractMap$SimpleEntry",
                    key,
                    value,
                    source,
                ) {
                    Ok(e) => e,
                    Err(err) => {
                        ctx.unpin_native_roots(this_pin);
                        return Err(err);
                    }
                };
                let action = ctx.read_native_pin(action_pin, action);
                if let Err(e) = ctx.invoke_virtual(
                    action,
                    "accept",
                    "(Ljava/lang/Object;)V",
                    &[Value::Object(Some(entry))],
                ) {
                    // `this_pin` was taken BELOW `action_pin`, so unwinding to
                    // it releases `action_pin` and the `flat` handles too.
                    ctx.unpin_native_roots(this_pin);
                    return Err(e);
                }
            }
            ctx.unpin_native_roots(this_pin);
            Ok(None)
        },
    );
    r.register(
        c,
        "forEachKey",
        "(JLjava/util/function/Consumer;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let action = match args.get(2) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(None),
            };
            let keys = chm_collect_all_keys(ctx, this);
            let action_pin = ctx.pin_native_root(action);
            let (_, key_pins) = pin_value_slice(ctx, &keys);
            for i in 0..keys.len() {
                let action = ctx.read_native_pin(action_pin, action);
                let key = read_pinned_elem(ctx, key_pins[i], keys[i]);
                if let Err(e) =
                    ctx.invoke_virtual(action, "accept", "(Ljava/lang/Object;)V", &[key])
                {
                    ctx.unpin_native_roots(action_pin);
                    return Err(e);
                }
            }
            ctx.unpin_native_roots(action_pin);
            Ok(None)
        },
    );
    r.register(
        c,
        "forEachValue",
        "(JLjava/util/function/Consumer;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let action = match args.get(2) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(None),
            };
            let vals = chm_collect_all_values(ctx, this);
            let action_pin = ctx.pin_native_root(action);
            let (_, val_pins) = pin_value_slice(ctx, &vals);
            for i in 0..vals.len() {
                let action = ctx.read_native_pin(action_pin, action);
                let val = read_pinned_elem(ctx, val_pins[i], vals[i]);
                if let Err(e) =
                    ctx.invoke_virtual(action, "accept", "(Ljava/lang/Object;)V", &[val])
                {
                    ctx.unpin_native_roots(action_pin);
                    return Err(e);
                }
            }
            ctx.unpin_native_roots(action_pin);
            Ok(None)
        },
    );
    r.register(
        c,
        "search",
        "(JLjava/util/function/BiFunction;)Ljava/lang/Object;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let func = match args.get(2) {
                Some(Value::Object(Some(f))) => *f,
                _ => return Ok(Some(Value::Object(None))),
            };
            let entries = chm_collect_all_entries(ctx, this);
            let func_pin = ctx.pin_native_root(func);
            let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
            let (_, flat_pins) = pin_value_slice(ctx, &flat);
            for i in 0..entries.len() {
                let func = ctx.read_native_pin(func_pin, func);
                let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
                let val = read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]);
                let result = match ctx.invoke_virtual(
                    func,
                    "apply",
                    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                    &[key, val],
                ) {
                    Ok(r) => r,
                    Err(e) => {
                        ctx.unpin_native_roots(func_pin);
                        return Err(e);
                    }
                };
                if let Some(Value::Object(Some(_))) = result {
                    ctx.unpin_native_roots(func_pin);
                    return Ok(result);
                }
            }
            ctx.unpin_native_roots(func_pin);
            Ok(Some(Value::Object(None)))
        },
    );

    // --- the rest of the bulk-operation family (see `chm_bulk_walk`) ---
    //
    // Argument positions: `args[0]` is the receiver, `args[1]` the
    // parallelism-threshold `long` (ONE slot in this vec, not two), and the
    // functional arguments follow. `reduce*To{Int,Long,Double}` puts the BASIS
    // between the transformer and the reducer.
    //
    // A missing or null functional argument keeps the pre-existing "answer the
    // identity" behaviour rather than raising: `reject_null_functional`'s
    // distinction applies, and the JDK's own NPE on these comes from
    // dereferencing the argument inside a parallel task, which is not a shape
    // this sequential walk reproduces faithfully enough to imitate.
    macro_rules! chm_reduce_obj {
        ($name:literal, $over:expr) => {
            r.register(
                c,
                $name,
                "(JLjava/util/function/BiFunction;)Ljava/lang/Object;",
                |ctx, args| {
                    chm_reject_null_bulk_args(args, &[2])?;
                    let this = match args.first() {
                        Some(Value::Object(Some(o))) => *o,
                        _ => return Ok(Some(Value::Object(None))),
                    };
                    match chm_fn_arg(args, 2) {
                        Some(reducer) => chm_bulk_reduce(ctx, this, $over, None, reducer),
                        None => Ok(Some(Value::Object(None))),
                    }
                },
            );
            r.register(
                c,
                $name,
                "(JLjava/util/function/Function;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
                |ctx, args| {
                    chm_reject_null_bulk_args(args, &[2, 3])?;
                    let this = match args.first() {
                        Some(Value::Object(Some(o))) => *o,
                        _ => return Ok(Some(Value::Object(None))),
                    };
                    match (chm_fn_arg(args, 2), chm_fn_arg(args, 3)) {
                        (Some(t), Some(reducer)) => {
                            chm_bulk_reduce(ctx, this, $over, Some(t), reducer)
                        }
                        _ => Ok(Some(Value::Object(None))),
                    }
                },
            );
        };
    }
    chm_reduce_obj!("reduceKeys", ChmBulkOver::Keys);
    chm_reduce_obj!("reduceEntries", ChmBulkOver::Entries);
    // `reduceValues(J, BiFunction)` is already registered above
    // (`native_chm_reduce_values`); only its transformer form is new.
    r.register(
        c,
        "reduceValues",
        "(JLjava/util/function/Function;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        |ctx, args| {
            chm_reject_null_bulk_args(args, &[2, 3])?;
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            match (chm_fn_arg(args, 2), chm_fn_arg(args, 3)) {
                (Some(t), Some(reducer)) => {
                    chm_bulk_reduce(ctx, this, ChmBulkOver::Values, Some(t), reducer)
                }
                _ => Ok(Some(Value::Object(None))),
            }
        },
    );
    r.register(
        c,
        "reduce",
        "(JLjava/util/function/BiFunction;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            chm_reject_null_bulk_args(args, &[2, 3])?;
            match (chm_fn_arg(args, 2), chm_fn_arg(args, 3)) {
                (Some(t), Some(reducer)) => chm_bulk_reduce_pairs(ctx, this, t, reducer),
                _ => Ok(Some(Value::Object(None))),
            }
        },
    );

    macro_rules! chm_reduce_prim {
        ($name:literal, $desc:literal, $over:expr, $pairs:expr, $width:expr) => {
            r.register(c, $name, $desc, |ctx, args| {
                chm_reject_null_bulk_args(args, &[2, 4])?;
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(Some($width.zero())),
                };
                let basis = args.get(3).copied().unwrap_or_else(|| $width.zero());
                match (chm_fn_arg(args, 2), chm_fn_arg(args, 4)) {
                    (Some(t), Some(reducer)) => {
                        chm_bulk_reduce_prim(ctx, this, $over, $pairs, t, basis, reducer, $width)
                    }
                    _ => Ok(Some(basis)),
                }
            });
        };
    }
    chm_reduce_prim!(
        "reduceKeysToInt",
        "(JLjava/util/function/ToIntFunction;ILjava/util/function/IntBinaryOperator;)I",
        ChmBulkOver::Keys,
        false,
        ChmPrimWidth::Int
    );
    chm_reduce_prim!(
        "reduceKeysToLong",
        "(JLjava/util/function/ToLongFunction;JLjava/util/function/LongBinaryOperator;)J",
        ChmBulkOver::Keys,
        false,
        ChmPrimWidth::Long
    );
    chm_reduce_prim!(
        "reduceKeysToDouble",
        "(JLjava/util/function/ToDoubleFunction;DLjava/util/function/DoubleBinaryOperator;)D",
        ChmBulkOver::Keys,
        false,
        ChmPrimWidth::Double
    );
    chm_reduce_prim!(
        "reduceValuesToInt",
        "(JLjava/util/function/ToIntFunction;ILjava/util/function/IntBinaryOperator;)I",
        ChmBulkOver::Values,
        false,
        ChmPrimWidth::Int
    );
    chm_reduce_prim!(
        "reduceValuesToLong",
        "(JLjava/util/function/ToLongFunction;JLjava/util/function/LongBinaryOperator;)J",
        ChmBulkOver::Values,
        false,
        ChmPrimWidth::Long
    );
    chm_reduce_prim!(
        "reduceValuesToDouble",
        "(JLjava/util/function/ToDoubleFunction;DLjava/util/function/DoubleBinaryOperator;)D",
        ChmBulkOver::Values,
        false,
        ChmPrimWidth::Double
    );
    chm_reduce_prim!(
        "reduceEntriesToInt",
        "(JLjava/util/function/ToIntFunction;ILjava/util/function/IntBinaryOperator;)I",
        ChmBulkOver::Entries,
        false,
        ChmPrimWidth::Int
    );
    chm_reduce_prim!(
        "reduceEntriesToLong",
        "(JLjava/util/function/ToLongFunction;JLjava/util/function/LongBinaryOperator;)J",
        ChmBulkOver::Entries,
        false,
        ChmPrimWidth::Long
    );
    chm_reduce_prim!(
        "reduceEntriesToDouble",
        "(JLjava/util/function/ToDoubleFunction;DLjava/util/function/DoubleBinaryOperator;)D",
        ChmBulkOver::Entries,
        false,
        ChmPrimWidth::Double
    );
    chm_reduce_prim!(
        "reduceToInt",
        "(JLjava/util/function/ToIntBiFunction;ILjava/util/function/IntBinaryOperator;)I",
        ChmBulkOver::Entries,
        true,
        ChmPrimWidth::Int
    );
    chm_reduce_prim!(
        "reduceToLong",
        "(JLjava/util/function/ToLongBiFunction;JLjava/util/function/LongBinaryOperator;)J",
        ChmBulkOver::Entries,
        true,
        ChmPrimWidth::Long
    );
    chm_reduce_prim!(
        "reduceToDouble",
        "(JLjava/util/function/ToDoubleBiFunction;DLjava/util/function/DoubleBinaryOperator;)D",
        ChmBulkOver::Entries,
        true,
        ChmPrimWidth::Double
    );

    macro_rules! chm_search {
        ($name:literal, $over:expr) => {
            r.register(
                c,
                $name,
                "(JLjava/util/function/Function;)Ljava/lang/Object;",
                |ctx, args| {
                    chm_reject_null_bulk_args(args, &[2])?;
                    let this = match args.first() {
                        Some(Value::Object(Some(o))) => *o,
                        _ => return Ok(Some(Value::Object(None))),
                    };
                    match chm_fn_arg(args, 2) {
                        Some(f) => chm_bulk_search(ctx, this, $over, f),
                        None => Ok(Some(Value::Object(None))),
                    }
                },
            );
        };
    }
    chm_search!("searchValues", ChmBulkOver::Values);
    chm_search!("searchEntries", ChmBulkOver::Entries);

    macro_rules! chm_foreach_transformed {
        ($name:literal, $over:expr) => {
            r.register(
                c,
                $name,
                "(JLjava/util/function/Function;Ljava/util/function/Consumer;)V",
                |ctx, args| {
                    chm_reject_null_bulk_args(args, &[2, 3])?;
                    let this = match args.first() {
                        Some(Value::Object(Some(o))) => *o,
                        _ => return Ok(None),
                    };
                    match (chm_fn_arg(args, 2), chm_fn_arg(args, 3)) {
                        (Some(t), Some(a)) => {
                            chm_bulk_foreach_transformed(ctx, this, $over, false, t, a)
                        }
                        _ => Ok(None),
                    }
                },
            );
        };
    }
    chm_foreach_transformed!("forEachKey", ChmBulkOver::Keys);
    chm_foreach_transformed!("forEachValue", ChmBulkOver::Values);
    chm_foreach_transformed!("forEachEntry", ChmBulkOver::Entries);
    r.register(
        c,
        "forEach",
        "(JLjava/util/function/BiFunction;Ljava/util/function/Consumer;)V",
        |ctx, args| {
            chm_reject_null_bulk_args(args, &[2, 3])?;
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            match (chm_fn_arg(args, 2), chm_fn_arg(args, 3)) {
                (Some(t), Some(a)) => {
                    chm_bulk_foreach_transformed(ctx, this, ChmBulkOver::Entries, true, t, a)
                }
                _ => Ok(None),
            }
        },
    );

    // ConcurrentMap interface — delegates to CHM segmented ops
    let cm = "java/util/concurrent/ConcurrentMap";
    r.register(cm, "size", "()I", native_chm_size);
    r.register(
        cm,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_get,
    );
    r.register(
        cm,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_put,
    );
    r.register(
        cm,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_remove,
    );
    r.register(
        cm,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_chm_contains_key,
    );
    r.set_category(__prev_cat);
}

// --- ConcurrentHashMap segmented native functions (Phase 86.2) ---

/// `ConcurrentHashMap`'s null-key refusal on the LOOKUP path — `get`,
/// `getOrDefault`, `containsKey`, `remove(Object)`.
///
/// MEASURED on the oracle 2026-08-16. None of these has an explicit null test:
/// each opens with `int h = spread(key.hashCode());`, so HotSpot's helpful-NPE
/// names the parameter and the text is TRANSCRIBED, not derived:
///
/// ```text
/// j.u.c.ConcurrentHashMap | get(null)          | java.lang.NullPointerException
///     msg=<<Cannot invoke "Object.hashCode()" because "key" is null>>
/// j.u.c.ConcurrentHashMap | getOrDefault(null,d) | ... same
/// j.u.c.ConcurrentHashMap | containsKey(null)  | ... same
/// j.u.c.ConcurrentHashMap | remove(null)       | ... same
/// ```
///
/// The string that used to be here — `"ConcurrentHashMap does not permit null
/// keys"` — appears nowhere in the JDK. It was a FABRICATION of the same kind
/// F41-1 §3 found twice in the `Properties` axis: right exception KIND,
/// invented text, invisible to any assertion that checks only the type.
///
/// This is NOT the message for the WRITE path. `put` / `putIfAbsent` /
/// `replace` / `merge` / `compute*` and `remove(Object,Object)` all reach an
/// explicit `throw new NullPointerException()` (`putVal`'s
/// `if (key == null || value == null)`, `remove(k,v)`'s
/// `if (key == null)`), which carries NO message — see `chm_bare_npe_on_null`.
pub(crate) fn chm_reject_null_key(key: &Value) -> Result<(), MethodCallFailed> {
    if matches!(key, Value::Object(None)) {
        return Err(RuntimeError::NullPointerException {
            // Same text `java.util.Hashtable` produces for the same reason: the
            // dereferenced local is named `key` in both.
            message: Some(HASHTABLE_NULL_KEY_MSG.to_string()),
        }
        .into());
    }
    Ok(())
}

/// `ConcurrentHashMap`'s WRITE-path null refusal. `putVal` opens with
///
/// ```text
///  0: aload_1        // key
///  1: ifnull    8
///  4: aload_2        // value
///  5: ifnonnull 16
///  8: new       #162  // class java/lang/NullPointerException
/// 12: invokespecial  // NullPointerException."<init>":()V
/// 15: athrow
/// ```
///
/// (`javap -p -c java.util.concurrent.ConcurrentHashMap`, JDK 25.0.3+9). The
/// no-arg constructor means **no message at all** — MEASURED:
/// `put(null,v)`, `put(k,null)`, `putIfAbsent` both ways and
/// `remove(null,null)` every one reads `msg=<NO-MESSAGE>`.
#[inline]
pub(crate) fn chm_bare_npe_on_null(key: &Value, value: &Value) -> Result<(), MethodCallFailed> {
    if matches!(key, Value::Object(None)) || matches!(value, Value::Object(None)) {
        return Err(bare_npe());
    }
    Ok(())
}

/// `ConcurrentHashMap`'s constructor guard, shared by `(I)V` and `(IFI)V`
/// because the one-argument form is literally `this(initialCapacity,
/// LOAD_FACTOR, 1)`.
///
/// ```text
/// public ConcurrentHashMap(int initialCapacity, float loadFactor, int concurrencyLevel) {
///     if (!(loadFactor > 0.0f) || initialCapacity < 0 || concurrencyLevel <= 0)
///         throw new IllegalArgumentException();
/// ```
///
/// Three things about that line are easy to get wrong and all three were:
///
///  * it is `new IllegalArgumentException()` with **no message at all**, unlike
///    `HashMap`'s, which names the offending value. Inventing a message here
///    would be the eleventh fabricated string in this family (`G1-1`);
///  * `!(loadFactor > 0.0f)` catches **NaN** for free, because every comparison
///    against NaN is false. Written as `loadFactor <= 0.0f` — the shape a
///    from-memory implementation reaches for — NaN passes, the threshold
///    becomes NaN and the table never resizes;
///  * `concurrencyLevel <= 0` is a refusal, not a clamp. The old body did
///    `(*v).max(1)`, and **clamping an argument is not validating it**.
pub(crate) fn chm_reject_bad_ctor_args(
    capacity: Option<&Value>,
    load_factor: Option<&Value>,
    concurrency: Option<&Value>,
) -> Result<(), MethodCallFailed> {
    let bad_cap = matches!(capacity, Some(Value::Int(c)) if *c < 0);
    // `clippy::neg_cmp_op_on_partial_ord` wants `partial_cmp` here, and the
    // negated comparison is the whole point -- see the third bullet of this
    // function's doc. `!(f > 0.0)` is false for NaN, so NaN is REJECTED; the
    // shapes the lint would steer this to (`f <= 0.0`, or a `partial_cmp`
    // match that treats `None` as "not less") let NaN through, the threshold
    // becomes NaN and the table never resizes. It is also the JDK's own source
    // line, quoted verbatim above, which is what makes this checkable against
    // `javap` rather than against memory.
    #[allow(clippy::neg_cmp_op_on_partial_ord)]
    let bad_lf = matches!(load_factor, Some(Value::Float(f)) if !(*f > 0.0));
    let bad_cl = matches!(concurrency, Some(Value::Int(c)) if *c <= 0);
    if bad_cap || bad_lf || bad_cl {
        return Err(RuntimeError::IllegalArgumentException {
            message: String::new(),
        }
        .into());
    }
    Ok(())
}

pub(crate) fn native_chm_init_default(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // The segments are NOT allocated here. `chm_segment_for_mut` installs them
    // on the first insert, and every inserting entry point goes through it
    // (`put`, `putIfAbsent`, `merge`, `compute`, `computeIfAbsent`, `replace`);
    // the read-only paths treat a segment-less receiver as an empty map, which
    // it is. HotSpot does exactly this -- `table` is null until the first
    // `put`, and the constructor only sets `sizeCtl`.
    //
    // MEASURED (probes/CollectionShapeCause.java, retained heap per empty
    // instance, Temurin 25.0.3+9): 600 B against HotSpot's 64.1, the worst
    // ratio of any collection in that table by a factor of four. The 536 B was
    // an `Object[4]` of segments, four 3-slot segment objects and four
    // `Object[4]` bucket arrays -- all of it for a map that may never be
    // written.
    //
    // `sizeCtl` is still recorded: a real default map's first table is
    // DEFAULT_CAPACITY = 16, and `chm_reorder_by_virtual_bucket` needs that
    // number whether or not a table exists yet.
    chm_record_initial_table(ctx, this, 16);
    Ok(None)
}

pub(crate) fn native_chm_init_capacity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // FOUND FROM `java.util.Properties`, recorded for L6 whose family this is.
    // `new Properties(-1)` answered no-throw where HotSpot raises
    // `IllegalArgumentException` (apps/probes/PropertiesShadowSweep 79) -- and
    // `Properties` is not where the guard belongs: real `Properties(int)`
    // bytecode runs `new ConcurrentHashMap<>(initialCapacity)`, and THIS
    // constructor validated nothing. One line, the same guard every other
    // hash-ordered constructor in the file now shares.
    map_ctor_capacity_load_check(args, MapCtorRefusalSpelling::Messageless)?;
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // ARGUMENT VALIDATION, which this constructor had none of. MEASURED
    // against HotSpot 25.0.4+7 in BOTH modes (`probes/ChmShadowSweep.java`):
    //
    //   new ConcurrentHashMap<>(-1)   HotSpot IllegalArgumentException  CratonVM no-throw
    //
    // `ConcurrentHashMap(int)` is `this(initialCapacity, LOAD_FACTOR, 1)`, so
    // the guard is the 3-arg constructor's -- see `chm_reject_bad_ctor_args`,
    // which also carries why the message is EMPTY here and named on `HashMap`.
    chm_reject_bad_ctor_args(args.get(1), None, None)?;
    let total_cap = match args.get(1) {
        Some(Value::Int(v)) => (*v).max(1) as usize,
        _ => CHM_DEFAULT_SEGMENTS * CHM_DEFAULT_SEGMENT_CAP,
    };
    // Match real JDK `ConcurrentHashMap(int initialCapacity)` exactly:
    //   cap = tableSizeFor(initialCapacity + (initialCapacity >>> 1) + 1)
    // i.e. inflate by 1.5x + 1 (NOT the naive 4/3 used previously), then
    // round up to a power of two. The previous `ceil(cap*4/3)` formula
    // undershot real JDK's table size (e.g. requested 16 -> adjusted 22 ->
    // /16 segments -> cap_per_seg 1 -> total buckets 16, whereas real JDK
    // sizes `new ConcurrentHashMap<>(16)`'s table to 32 buckets: 16 + 8 + 1
    // = 25 -> tableSizeFor(25) = 32). The mismatched total bucket count
    // shifted every key's virtual JDK-table bucket index, which broke
    // `SimpleAliasRegistry.getAliases()` (backed by a `ConcurrentHashMap<>(16)`)
    // — its `aliasMap.forEach` iteration order no longer matched HotSpot's,
    // so `XmlBeanDefinitionReaderTests`/`XmlBeanFactoryTests`'
    // `containsExactly("myalias", "youralias")` assertions failed.
    let total_cap_u64 = total_cap as u64;
    let jdk_adjusted = total_cap_u64
        .saturating_add(total_cap_u64 >> 1)
        .saturating_add(1);
    let adjusted_total = hashmap_table_size_for_u64(jdk_adjusted) as usize;
    let cap_per_seg = (adjusted_total / CHM_DEFAULT_SEGMENTS)
        .max(1)
        .next_power_of_two();
    // The bucket order a caller observes has to follow the REAL table size for
    // this initial capacity, which is `chm_jdk_table_size_for` (measured), not
    // the segment total below — those two agree here but diverge as soon as any
    // segment resizes.
    //
    // gen r4w4/rooting2: recorded BEFORE the segments are built. The record
    // is a plain field store that does not care about order, while the build
    // allocates 2N+1 objects and used to leave `this` a pre-move address for
    // this write.
    let table = chm_jdk_table_size_for(total_cap);
    chm_record_initial_table(ctx, this, table);
    // No more segments than the modelled table has buckets (round 15 wave 5,
    // lane compat6): `ConcurrentHashMap(2)` is a 4-bucket table.
    chm_init_segments(
        ctx,
        this,
        chm_segments_for_table(CHM_DEFAULT_SEGMENTS, table),
        cap_per_seg,
    );
    Ok(None)
}

pub(crate) fn native_chm_init_full(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // The three guards the JDK runs before anything else. See
    // `chm_reject_bad_ctor_args`; every one of them was missing, and a NaN
    // load factor reached the `adjusted_total` division below.
    chm_reject_bad_ctor_args(args.get(1), args.get(2), args.get(3))?;
    let total_cap = match args.get(1) {
        Some(Value::Int(v)) => (*v).max(1) as usize,
        _ => CHM_DEFAULT_SEGMENTS * CHM_DEFAULT_SEGMENT_CAP,
    };
    // Bug 4: the 3-arg form takes (initialCapacity, loadFactor, concurrencyLevel).
    // Honour the supplied load factor (arg index 2 is a float). Default to 0.75
    // when missing/invalid.
    let load_factor = match args.get(2) {
        Some(Value::Float(f)) if *f > 0.0 && f.is_finite() => *f,
        _ => 0.75_f32,
    };
    let concurrency = match args.get(3) {
        Some(Value::Int(v)) => (*v).max(1) as usize,
        _ => CHM_DEFAULT_SEGMENTS,
    };
    let num_segments = concurrency.next_power_of_two().min(256);
    let adjusted_total = ((total_cap as f64) / (load_factor as f64)).ceil() as usize;
    let adjusted_total = adjusted_total.max(total_cap); // guard against fp underflow
    let cap_per_seg = (adjusted_total / num_segments).max(1).next_power_of_two();
    // gen r4w4/rooting2: record first -- see `native_chm_init_capacity`.
    let table = chm_jdk_table_size_for(total_cap);
    chm_record_initial_table(ctx, this, table);
    chm_init_segments(
        ctx,
        this,
        chm_segments_for_table(num_segments, table),
        cap_per_seg,
    );
    Ok(None)
}

pub(crate) fn native_chm_init_from_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // `ConcurrentHashMap(Map m)` is `this.sizeCtl = DEFAULT_CAPACITY; putAll(m);`
    // and `putAll` opens by calling `m.size()`, so a null source is an NPE
    // BEFORE the map is usable. This body answered an empty map instead —
    // the shape `phase-2-worklist` records as the worst a refusal can take,
    // because the caller does not learn it passed null until much later.
    if matches!(args.get(1), None | Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException {
            message: Some(
                "Cannot invoke \"java.util.Map.size()\" because \"m\" is null".to_string(),
            ),
        }
        .into());
    }
    // cceres5: `chm_init_segments` allocates (segments + buckets); `source`
    // sat raw in `args` across it, so `collect_entries_any` below could walk a
    // stale receiver (see native_map_init_from_map's live capture). Pin both
    // before the first allocation.
    let source0 = args.get(1).copied().unwrap_or(Value::Object(None));
    let source_base = pin_value(ctx, source0);
    let this_pin0 = ctx.pin_native_root(this);
    let first_pin = if source_base == usize::MAX {
        this_pin0
    } else {
        source_base
    };
    // gen r4w4/rooting2: record first -- see `native_chm_init_capacity`.
    chm_record_initial_table(ctx, this, 16);
    chm_init_segments(ctx, this, CHM_DEFAULT_SEGMENTS, CHM_DEFAULT_SEGMENT_CAP);
    let this = ctx.read_native_pin(this_pin0, this);
    let source_refreshed = read_pinned_elem(ctx, source_base, source0);
    // Copy entries from the source map. Use `collect_entries_any` (NOT the
    // HashMap-bucket-only `map_collect_entries`) so a `TreeMap` / `Collections
    // .singletonMap` / real-JDK unmodifiable / any third-party `Map` source is
    // copied in full via `entrySet().iterator()` instead of being read as an
    // empty bucket table. This matches `native_chm_put_all`. Without it,
    // `new ConcurrentHashMap<>(Collections.singletonMap(k,v))` produced an EMPTY
    // map — which silently broke Mockito's inline mock maker: `MockUtil`'s static
    // `mockMakers = new ConcurrentHashMap<>(singletonMap(makerClass, maker))` came
    // up empty, so `getMockHandlerOrNull` iterated nothing and `isMock()` was
    // always false → `when()/given()` on any mock threw `NotAMockException`.
    let source = match source_refreshed {
        Value::Object(Some(o)) => o,
        _ => {
            ctx.unpin_native_roots(first_pin);
            return Ok(None);
        }
    };
    // `putAll(m)` is VIRTUAL in `this(m.size()); putAll(m);`: a program
    // subclass overriding it runs its override during `super(m)`
    // ([`copy_ctor_bulk_override`]).
    if copy_ctor_bulk_override(&*ctx, this, "putAll", "(Ljava/util/Map;)V") {
        let routed = (|| -> MethodCallResult {
            if chm_jdk25_sizing_enabled() {
                let n = match read_pinned_elem(ctx, source_base, source0) {
                    Value::Object(Some(s)) => match ctx.invoke_virtual(s, "size", "()I", &[])? {
                        Some(Value::Int(n)) => n,
                        _ => 0,
                    },
                    _ => 0,
                };
                chm_reject_bad_ctor_args(Some(&Value::Int(n)), None, None)?;
                let this_now = ctx.read_native_pin(this_pin0, this);
                let table = chm_jdk_table_size_for(n.max(1) as usize);
                chm_record_initial_table(ctx, this_now, table);
                chm_fit_segments_to_table(ctx, this_now, table);
            }
            let this_now = ctx.read_native_pin(this_pin0, this);
            let src_now = read_pinned_elem(ctx, source_base, source0);
            ctx.invoke_virtual(this_now, "putAll", "(Ljava/util/Map;)V", &[src_now])
        })();
        ctx.unpin_native_roots(first_pin);
        return routed.map(|_| None);
    }
    // C3W3-2 (`CRATONVM_COMPAT_MAP_COPY_JDK_QUESTIONS`, round 15 wave 4, lane
    // compat4): `putAll(m)` with no table yet -- no `m.size()` -- then the
    // walk, one `putVal` per entry as it arrives. JDK 25 asks `m.size()` once
    // BEFORE it, in `this(m.size())` ([`chm_jdk25_sizing_enabled`]; round 15
    // wave 5, lane compat6).
    if let Some(prog) = foreign_map_source(&*ctx, source) {
        let _resize_flag = ChmResizeLockGuard::enter();
        let prog_pin = ctx.pin_native_root(prog);
        if chm_jdk25_sizing_enabled() {
            let sized = (|| -> Result<(), MethodCallFailed> {
                let prog_now = ctx.read_native_pin(prog_pin, prog);
                let answer = ctx.invoke_virtual(prog_now, "size", "()I", &[])?;
                let n = match answer {
                    Some(Value::Int(n)) => n,
                    _ => 0,
                };
                // `this(int)`: `initialCapacity < 0` is the message-less IAE.
                chm_reject_bad_ctor_args(Some(&Value::Int(n)), None, None)?;
                let this_now = ctx.read_native_pin(this_pin0, this);
                // `initialCapacity < concurrencyLevel (1)` is raised to 1.
                let table = chm_jdk_table_size_for(n.max(1) as usize);
                chm_record_initial_table(ctx, this_now, table);
                // Allocates; the walk below re-reads `this` / `prog` from pins.
                chm_fit_segments_to_table(ctx, this_now, table);
                Ok(())
            })();
            if let Err(e) = sized {
                ctx.unpin_native_roots(first_pin);
                return Err(e);
            }
        }
        let walked = map_for_each_entry(
            ctx,
            (prog_pin, prog),
            MapSizeAsk::Skip,
            |ctx, key, value| chm_put_all_entry(ctx, (this_pin0, this), key, value),
        );
        ctx.unpin_native_roots(first_pin);
        return walked.map(|()| None);
    }
    // GC-safety: same pinned-slice iteration as `native_chm_put_all` — the
    // collected entries are reused across GC-triggering hashing/puts and
    // GC-pausable segment-lock waits; `this` is pinned BEFORE the
    // GC-triggering collection. A throwing collection releases the frame's
    // pins (round 15 wave 4, lane compat4: the `?` left them up).
    let collected = collect_entries_any(ctx, source);
    let src_entries = unpin_on_err(ctx, first_pin, collected)?;
    // `this(m.size())`: the table `ConcurrentHashMap(int)` sizes for the
    // source's count (raised to its `concurrencyLevel` of 1), not the default
    // 16 recorded above.
    if chm_jdk25_sizing_enabled() {
        let this_now = ctx.read_native_pin(this_pin0, this);
        chm_record_initial_table(
            ctx,
            this_now,
            chm_jdk_table_size_for(src_entries.len().max(1)),
        );
    }
    let _resize_flag = ChmResizeLockGuard::enter();
    let flat: Vec<Value> = src_entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (_, flat_pins) = pin_value_slice(ctx, &flat);
    // After the pins: the refit allocates, and the loop re-reads every entry
    // and `this` from them.
    if chm_jdk25_sizing_enabled() {
        let this_now = ctx.read_native_pin(this_pin0, this);
        chm_fit_segments_to_table(
            ctx,
            this_now,
            chm_jdk_table_size_for(src_entries.len().max(1)),
        );
    }
    let result = (|| -> MethodCallResult {
        for i in 0..src_entries.len() {
            let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
            let value = read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]);
            // `ConcurrentHashMap(Map m)` IS `putAll(m)`, so it inherits the
            // per-entry refusal: copying a `HashMap` that holds one null value
            // is an NPE, not a map that quietly dropped an entry. MEASURED
            // against HotSpot 25.0.4+7, both modes.
            chm_bare_npe_on_null(&key, &value)?;
            let hash = chm_key_hash(ctx, &key)?;
            let this = ctx.read_native_pin(this_pin0, this);
            if let Some(seg) = chm_segment_for_mut(ctx, this, hash) {
                let (_guard, seg) = ChmMonitorGuard::acquire_gc_safe(ctx, seg);
                let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
                let value = read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]);
                // `putVal` hashes once: the segment put reuses `hash`, as
                // `native_chm_put_all` does (round 15 wave 4, lane compat4:
                // `native_map_put` hashed the key a second time, so a program
                // key's `hashCode()` ran twice per entry here and once on
                // HotSpot). `chm_seg_put_hashed` falls back to `native_map_put`
                // under `CRATONVM_COMPAT_CHM_SINGLE_HASH=0`.
                chm_seg_put_hashed(ctx, seg, key, value, hash, false)?;
            }
        }
        Ok(None)
    })();
    ctx.unpin_native_roots(first_pin);
    result
}

// --- Core read operations (lock-free) ---

/// Round-5 CRIT fix (publication race): CHM-specific segment lookup.
///
/// Reads the segment's buckets slot via `get_field_volatile` and walks
/// the chain using `get_field_volatile` on `NODE_FIELD_NEXT`. Pairs with
/// `map_resize`'s `set_field_volatile` publication of the new buckets
/// array: a reader is guaranteed to observe either the OLD or NEW
/// fully-linked array, never a half-spliced chain.
///
/// Returns `Some(value)` on hit (including null-valued mappings),
/// `None` on miss. The hash and key matching logic mirrors
/// `native_map_get`.
///
/// `hash` is the caller's `chm_key_hash(key_val)` — every caller has already
/// computed it to pick `seg`. r9w11 chm11: this used to hash the key a SECOND
/// time, which for a user-defined key ran its Java `hashCode()` twice per
/// lookup (the JDK runs it once) and for a wrapper key paid a second unbox. A
/// primitive-wrapper key is first offered to the in-place, no-pin, no-`Vec`
/// walk [`chm_seg_get_wrapper_key_hashed`]; only a decline reaches the general
/// snapshot path below.
pub(crate) fn chm_seg_get(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    key_val: Value,
    hash: i32,
) -> Result<Option<Value>, MethodCallFailed> {
    chm_seg_get_impl(ctx, seg, key_val, hash, false)
}

/// [`chm_seg_get`] that answers a `computeIfAbsent` RESERVATION (the segment
/// object stored as the value) as the value it is instead of as absent: what
/// the reservation protocol itself must see, with the caller's hash, so it
/// makes no `hashCode()` call of its own (`CRATONVM_COMPAT_CHM_REMAP_SINGLE_HASH`).
pub(crate) fn chm_seg_get_raw(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    key_val: Value,
    hash: i32,
) -> Result<Option<Value>, MethodCallFailed> {
    chm_seg_get_impl(ctx, seg, key_val, hash, true)
}

pub(crate) fn chm_seg_get_impl(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    key_val: Value,
    hash: i32,
    raw: bool,
) -> Result<Option<Value>, MethodCallFailed> {
    match key_val {
        // The raw read skips the wrapper-key fast walk and takes the general
        // walk below, whose reservation test it can switch off (a wrapper's
        // `equals` is not observable either way).
        Value::Object(Some(k)) if !raw => {
            if let Some(hit) = chm_seg_get_wrapper_key_hashed(&*ctx, seg, k, hash) {
                return Ok(hit);
            }
        }
        Value::Object(_) => {}
        _ => return Ok(None),
    }
    // Equality dispatch is an arbitrary Java call. Keep the segment and lookup
    // key rooted across it so a moving collection cannot invalidate the chain
    // snapshot.
    let roots_base = ctx.pin_native_root(seg);
    let key_pin = pin_value(ctx, key_val);
    let key_val = read_pinned_elem(ctx, key_pin, key_val);
    let is_null_key = matches!(key_val, Value::Object(None));
    let seg = ctx.read_native_pin(roots_base, seg);
    // Bug 1+2 (CRIT) round-10 fix: take a read-lock against any
    // in-progress concurrent `map_resize_concurrent` on this segment.
    // The resize path mutates old-chain NEXT pointers in place, which
    // can cause lock-free readers to skip past keys living in the hi
    // partition. The read-lock serializes readers against the writer's
    // mutation; parallel reads remain concurrent.
    //
    // Stripe is selected by `ctx.identity_hash_code(seg)` (stable
    // across GC moves) and indexes into a 256-entry static RwLock
    // array — no global Mutex, no raw-pointer keying.
    let seg_id = ctx.identity_hash_code(seg);
    // A resize holds the write side across allocations and write barriers,
    // both of which can request a collection. Enter the native blocking
    // protocol while waiting so that collection can complete; refresh `seg`
    // from its root after the potentially moving pause.
    let read_guard = chm_seg_read_gc_safe(ctx, seg_id);
    let seg = ctx.read_native_pin(roots_base, seg);
    // Acquire-load the buckets array reference. If the writer has
    // begun publishing a new array, we see either the old one (with a
    // fully-linked chain) or the new one (also fully-linked) — never a
    // torn intermediate.
    let buckets_val = ctx.get_field_volatile(seg, MAP_FIELD_BUCKETS);
    let buckets = match buckets_val {
        Value::Object(Some(arr)) if ctx.heap_kind_of(arr) == ObjectKind::Array => arr,
        _ => {
            drop(read_guard);
            ctx.unpin_native_roots(roots_base);
            return Ok(None);
        }
    };
    // Derive cap from the bucket array's length — that's authoritative and
    // immune to the slot-aliasing problem below. Previously this read
    // `get_field(seg, MAP_FIELD_CAPACITY)` (= slot 2), which was correct
    // before the first segment-resize but stale afterwards: `map_resize`
    // resolves `java/util/HashMap.table` via the real-JDK class metadata —
    // which puts `table` at absolute slot 2 (after AbstractMap.keySet@0 and
    // AbstractMap.values@1) — and `set_field_volatile`s the new bucket
    // array into that slot. Slot 2 then holds `Value::Object(Some(arr))`,
    // not `Value::Int(cap)`, and this match's `_` arm returned `Ok(None)`
    // for every subsequent `get` on that segment — silently making every
    // CHM lookup miss after the first growth (10000-key probe: 0/10000
    // hits; reproduces BC's `ObjectIdentifier.intern()` "Should be taken
    // from cache" failure where the pool grows past the resize threshold
    // and subsequent intern lookups can't find their predecessor).
    let cap = ctx.array_length(buckets) as i32;
    if cap <= 0 {
        drop(read_guard);
        ctx.unpin_native_roots(roots_base);
        return Ok(None);
    }
    let idx = map_bucket_index(hash, cap);
    // PERF (chm-narrow-lock): SNAPSHOT the bucket chain (each node's key+value)
    // while holding the read-lock, then RELEASE the lock before running the
    // key-equality comparisons. The lock exists only to serialize this chain
    // walk against `map_resize_concurrent`'s in-place NEXT mutation; capturing a
    // consistent, fully-linked view under the lock satisfies that contract
    // exactly (same linearization point a lock-free JDK `get` uses). Holding the
    // `parking_lot` read-lock across `map_keys_equal` was both a needlessly long
    // critical section AND a latent self-deadlock: `map_keys_equal` can invoke a
    // user-defined `equals(Object)` (arbitrary Java) which could re-enter a CHM
    // resize and take the WRITE lock on the same stripe — parking_lot read→write
    // on one thread is not reentrant. Resize only re-links NEXT pointers; it
    // never rewrites a node's key/value, so a snapshot taken under the lock stays
    // valid for comparison after the lock drops. Behaviour is identical: the same
    // (key,value) pairs are examined in the same order.
    let mut chain: Vec<(i32, Value, Value)> = Vec::new();
    let mut node_val = ctx.get_array_element(buckets, idx);
    while let Value::Object(Some(node)) = node_val {
        chain.push((
            get_node_hash(ctx, node),
            get_node_key(ctx, node),
            get_node_value(ctx, node),
        ));
        // Acquire-load the NEXT pointer. Pairs with the writer's
        // `set_field` of NEXT during chain construction — the writer
        // publishes the buckets array with `set_field_volatile` AFTER
        // all NEXT links are set, so a reader observing the new
        // buckets array necessarily observes the corresponding NEXT
        // writes (happens-before via Release/Acquire).
        node_val = ctx.get_field_volatile(node, NODE_FIELD_NEXT);
    }
    drop(read_guard); // critical section ends — comparisons run lock-free

    let chain_pins: Vec<(usize, usize)> = chain
        .iter()
        .map(|(_, key, value)| (pin_value(ctx, *key), pin_value(ctx, *value)))
        .collect();
    for ((node_hash, node_key_field, node_value), (node_key_pin, node_value_pin)) in
        chain.into_iter().zip(chain_pins)
    {
        let node_key_field = read_pinned_elem(ctx, node_key_pin, node_key_field);
        let node_value = read_pinned_elem(ctx, node_value_pin, node_value);
        if is_null_key {
            if matches!(node_key_field, Value::Object(None)) {
                // CHM-mapper-deadlock fix (2026-07-21): a value that is the
                // segment object itself is an in-flight `computeIfAbsent`
                // RESERVATION (see `chm_compute_if_absent_absent_path`), not
                // a real mapping — lock-free readers must treat it as absent,
                // exactly like the JDK's `ReservationNode`.
                let seg_now = ctx.read_native_pin(roots_base, seg);
                if let Value::Object(Some(v)) = node_value {
                    if !raw && std::ptr::eq(v.as_ptr(), seg_now.as_ptr()) {
                        ctx.unpin_native_roots(roots_base);
                        return Ok(None);
                    }
                }
                ctx.unpin_native_roots(roots_base);
                return Ok(Some(node_value));
            }
        } else if node_hash == hash {
            let Value::Object(Some(node_key)) = node_key_field else {
                continue;
            };
            let key = match read_pinned_elem(ctx, key_pin, key_val) {
                Value::Object(Some(key)) => key,
                _ => {
                    ctx.unpin_native_roots(roots_base);
                    return Ok(None);
                }
            };
            if map_keys_equal(ctx, key, node_key)? {
                let node_value = read_pinned_elem(ctx, node_value_pin, node_value);
                // CHM-mapper-deadlock fix (2026-07-21): reservation markers
                // read as absent — see the null-key arm above.
                let seg_now = ctx.read_native_pin(roots_base, seg);
                if let Value::Object(Some(v)) = node_value {
                    if !raw && std::ptr::eq(v.as_ptr(), seg_now.as_ptr()) {
                        ctx.unpin_native_roots(roots_base);
                        return Ok(None);
                    }
                }
                ctx.unpin_native_roots(roots_base);
                return Ok(Some(node_value));
            }
        }
    }
    ctx.unpin_native_roots(roots_base);
    Ok(None)
}

/// Allocation-free String-key fast path for `ConcurrentHashMap.get`, the direct
/// analogue of [`native_hashmap_get_string_fast`] on the HashMap side.
///
/// String hashing and equality are *final, pure* operations: they run no Java
/// code and therefore cannot trigger a collection. That removes the reason for
/// nearly everything the general [`chm_seg_get`] path pays per lookup — the
/// receiver/key pinning, a second `map_hash_key`, and above all the
/// snapshot-the-entire-chain-into-two-heap-allocated-`Vec`s dance, which exists
/// only so a *user-defined* `equals` cannot observe a half-relinked chain or
/// re-enter a resize and self-deadlock on the stripe lock. With no user code in
/// play the chain can simply be walked in place under the read lock, exactly
/// like a JDK lock-free `get`.
///
/// `native_chm_get` measured 6920 cycles/call without this path — 6x the
/// equivalent HashMap lookup — which is what made `CharsetCache.getCharset` (a
/// `ConcurrentMap<String,Charset>` probe) lose to the *uncached*
/// `Charset.forName` baseline in `TestCharsetCachePerformance`.
///
/// Returns `None` — meaning "fall through to the general path, unchanged" —
/// whenever anything is not a plain `java.lang.String`: a non-String lookup
/// key, or a chain containing any non-String key. Mixed-key maps and every
/// exotic key type (enums, Thread mirrors, user-defined `equals`) therefore run
/// exactly the code they ran before.
pub(crate) fn native_chm_get_string_chain(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    key: ObjectRef,
    hash: i32,
) -> Option<(Value, Option<ObjectRef>)> {
    let buckets = match ctx.get_field_volatile(seg, MAP_FIELD_BUCKETS) {
        Value::Object(Some(arr)) if ctx.heap_kind_of(arr) == ObjectKind::Array => arr,
        _ => return Some((Value::Object(None), None)),
    };
    let cap = ctx.array_length(buckets) as i32;
    if cap <= 0 {
        return Some((Value::Object(None), None));
    }
    let mut node_value = ctx.get_array_element(buckets, map_bucket_index(hash, cap));
    const CHAIN_WALK_LIMIT: usize = 4096;
    for _ in 0..CHAIN_WALK_LIMIT {
        let Value::Object(Some(node)) = node_value else {
            return Some((Value::Object(None), None));
        };
        let Value::Object(Some(node_key)) = get_node_key(ctx, node) else {
            // CHM rejects null keys, so this is an unexpected node shape —
            // defer to the general path rather than guessing.
            return None;
        };
        match ctx.java_strings_equal(node_key, key) {
            Some(true) => {
                let value = get_node_value(ctx, node);
                // An in-flight `computeIfAbsent` parks the segment object
                // itself in the value slot as a reservation marker; lock-free
                // readers must read that as absent, exactly as `chm_seg_get`
                // does (CHM-mapper-deadlock fix, 2026-07-21).
                //
                // Handing back no memoizable node with it is what keeps that
                // marker out of the per-thread node memo — a memo hit answers
                // later lookups WITHOUT re-running this filter, and the
                // `computeIfAbsent` phases that install and then replace the
                // marker deliberately run outside `ChmMonitorGuard`, so they
                // never bump the mutation epoch that would evict the entry.
                if let Value::Object(Some(v)) = value {
                    if std::ptr::eq(v.as_ptr(), seg.as_ptr()) {
                        return Some((Value::Object(None), None));
                    }
                }
                return Some((value, Some(node)));
            }
            Some(false) => {}
            // A non-String key on the chain — hand the whole lookup back so
            // the general path's full equality ladder decides.
            None => return None,
        }
        node_value = ctx.get_field_volatile(node, NODE_FIELD_NEXT);
    }
    None
}

pub(crate) fn native_chm_get_string_fast(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key: ObjectRef,
) -> Option<Value> {
    // The memo is keyed by the CHM receiver and the exact key String object,
    // not by its segment. A validated hit therefore skips the String hash, the
    // segment-array access, and the segment identity-hash minting that a
    // regular CHM get requires.
    if let Some((cached_seg_id, cached_generation, value)) =
        ctx.chm_string_node_cache_get_object(this, key)
    {
        let (epoch, active) = chm_seg_mutation_snapshot(cached_seg_id);
        if active == 0 && epoch == cached_generation {
            return Some(value);
        }
    }
    let raw_hash = ctx.java_string_hash_code(key)?;
    let hash = raw_hash ^ ((raw_hash as u32) >> 16) as i32;
    let seg = chm_segment_for(ctx, this, hash)?;
    let seg_id = ctx.identity_hash_code(seg);
    let before_mutation = chm_seg_mutation_snapshot(seg_id);

    // Optimistic walk. The resize epoch is odd for exactly as long as a
    // resizer holds this stripe's write lock, so an unchanged even epoch
    // across the walk means no relinking overlapped it.
    let resize_epoch = chm_seg_resize_epoch(seg_id);
    let resize_before = resize_epoch.load(std::sync::atomic::Ordering::Acquire);
    let walked = if resize_before & 1 == 0 {
        let result = native_chm_get_string_chain(ctx, seg, key, hash);
        if resize_epoch.load(std::sync::atomic::Ordering::Acquire) == resize_before {
            Some(result)
        } else {
            None
        }
    } else {
        None
    };
    // The resize writer relinks existing node chains in place. If it overlapped
    // the optimistic walk, redo the walk under the established stripe lock.
    let (result, node) = match walked {
        Some(result) => result?,
        None => {
            // r9w11 chm11: `try_read`, not `read` -- this body is also the
            // String half of the JIT leaf probe `jit_chm_get`, which must
            // never block (see `chm_seg_get_wrapper_key`). A busy stripe
            // declines to the general path, which waits for it in the funnel.
            let _read_guard = chm_seg_lock_for(seg_id).try_read()?;
            native_chm_get_string_chain(ctx, seg, key, hash)?
        }
    };

    // Memoize only a node that no mutator touched across the walk. Re-sampling
    // the stripe here, rather than trusting the pre-walk read, is what makes
    // the recorded generation a promise about the walk itself.
    if let Some(node) = node {
        let after_mutation = chm_seg_mutation_snapshot(seg_id);
        if before_mutation == after_mutation && before_mutation.1 == 0 {
            if let Some(key_text) = ctx.read_string(key) {
                ctx.chm_string_node_cache_put(
                    this,
                    seg_id,
                    key,
                    &key_text,
                    node,
                    before_mutation.0,
                );
            }
        }
    }
    Some(result)
}

// ---------------------------------------------------------------------------
// r9w11 chm11: primitive-wrapper-key fast path for `ConcurrentHashMap` reads.
// ---------------------------------------------------------------------------
//
// `perf-concurrenthashmap-get-native-override-is-360x-slower-than-hotspot`:
// `VolSplice chm` (30 M `get(Integer)` on a 1024-entry map) measured 35.4 s on
// the w10 binary against 123 ms on HotSpot, ~1.1 us per lookup. The sampler put
// ~75 % of it inside this native, not in the dispatch: an `Integer` key has no
// fast path (the String one above declines it after scanning the String-node
// memo), so every lookup paid `chm_key_hash` (unbox), four native pins,
// `chm_seg_get`'s SECOND `map_hash_key`, the stripe read lock, two heap
// `Vec`s for the chain snapshot, SEVEN descriptor-resolving checked field
// reads per chain node (`get_node_hash`/`_key`/`_value` each re-read slot 0 to
// sniff the layout), per-node pins, and `map_keys_equal`'s two unboxes.
//
// A JDK primitive wrapper key (`Integer`, `Long`, `Short`, `Byte`,
// `Character`, `Boolean`, `Float`, `Double` -- all `final`) hashes and compares
// by value with no Java code, so -- exactly like the String path -- nothing
// can allocate or run a collection, and the chain can be walked IN PLACE:
// no pins, no snapshot, one layout sniff per node, and the value read only on
// a hit. Concurrency is the String path's: an optimistic walk validated by the
// stripe's resize epoch (odd while a resizer relinks NEXT pointers in place),
// retried under the stripe read lock when a resize overlapped it (or declined
// to the general path if that lock is busy -- never blocking). Mutators
// (`put`/`remove`/...) never relink a live chain in place except under that
// resize guard, and publish new nodes/arrays with volatile stores, so a
// validated walk sees a consistent chain -- the same linearisation the
// general path's snapshot gives.
//
// Answers are the general path's answers, by construction:
//   * the hash is `map_hash_key`'s wrapper arm, spread the same way;
//   * equality is `map_keys_equal`'s: identity, else SAME wrapper class and
//     `wrapper_prims_equal` on the unboxed values. A node key of a DIFFERENT
//     class is unequal there too -- for a non-wrapper node key the general
//     path ends in `key.equals(nodeKey)`, i.e. the wrapper's own `final`
//     `equals`, whose `instanceof` refuses it;
//   * a node whose value is the segment itself is an in-flight
//     `computeIfAbsent` reservation and reads as absent;
//   * anything unexpected (a same-class key that will not unbox, an
//     over-long chain, a typed node read that is not an `int` hash) DECLINES
//     (`None`) and the general path runs unchanged.

/// `(unboxed value, spread hash)` of a primitive-wrapper key, computed exactly
/// as `map_hash_key`'s wrapper arm computes it; `None` for any other key.
#[inline]
pub(crate) fn chm_wrapper_key_hash(ctx: &dyn NativeContext, key: ObjectRef) -> Option<(Value, i32)> {
    let prim = unbox_wrapper(ctx, key)?;
    chm_wrapper_key_hash_for(ctx, ctx.class_id_of_object(key), prim)
}

/// [`chm_wrapper_key_hash_of`] for a key of class `key_class`: the one arm
/// the primitive alone cannot decide is `Boolean`, whose `Int(0|1)` hashes
/// as 1231/1237 under `CRATONVM_COMPAT_BOOLEAN_KEY_HASH` -- exactly as
/// [`map_key_raw_hash`] hashes it, so this no-Java walk and the general path
/// (`chm_key_hash`) keep looking in the same bucket.
#[inline]
pub(crate) fn chm_wrapper_key_hash_for(
    ctx: &dyn NativeContext,
    key_class: ClassId,
    prim: Value,
) -> Option<(Value, i32)> {
    if let Value::Int(bit) = prim {
        if let Some(h) = boolean_key_bucket_hash(ctx, key_class, bit) {
            return Some((prim, h ^ ((h as u32) >> 16) as i32));
        }
    }
    chm_wrapper_key_hash_of(prim)
}

/// [`chm_wrapper_key_hash`] for a caller that already holds the unboxed
/// primitive — the hash derivation alone, with no heap access at all.
///
/// r9w12: split out because `chm_get_wrapper_key_fast` needs the key's class
/// id as well as its hash, and reading the two through separate doors costs an
/// extra `is_object_address` walk per lookup. See
/// `NativeSystemAccess::class_id_and_unboxed_primitive`.
#[inline]
pub(crate) fn chm_wrapper_key_hash_of(prim: Value) -> Option<(Value, i32)> {
    let h = match prim {
        Value::Int(v) => v,
        Value::Long(v) => (v ^ (v >> 32)) as i32,
        Value::Float(v) => cratonvm_types::jfp::float_hash_code(v),
        Value::Double(v) => cratonvm_types::jfp::double_hash_code(v),
        _ => return None,
    };
    Some((prim, h ^ ((h as u32) >> 16) as i32))
}

/// Chain-walk bound for [`chm_wrapper_chain_walk`]; a longer chain declines to
/// the general path (which has no bound) rather than answering "absent".
pub(crate) const CHM_WRAPPER_WALK_LIMIT: usize = 4096;

/// This thread's memoized `java/util/HashMap$Node` class id for THIS VM, if
/// [`hm_node_class_id`] has already resolved it. Never resolves (that can
/// load a class), so it is safe on the no-allocation paths.
#[inline]
pub(crate) fn hm_node_class_memoized(ctx: &dyn NativeContext) -> Option<ClassId> {
    let vm = ctx.vm_identity();
    match HM_NODE_CLASS_MEMO.with(|c| c.get()) {
        Some((cached_vm, answer)) if cached_vm == vm => answer,
        _ => None,
    }
}

/// One in-place walk of `seg`'s bucket for a primitive-wrapper `key`.
///
/// `Some(Some(v))` hit (a null-valued mapping included), `Some(None)` miss,
/// `None` = cannot decide here, run the general path. No allocation, no Java,
/// no pins: the caller must not be able to reach a safepoint across it.
pub(crate) fn chm_wrapper_chain_walk<'c>(
    ctx: &'c dyn NativeContext,
    seg_cur: &cratonvm_native_api::ReadCursor<'c>,
    key: ObjectRef,
    key_class: ClassId,
    key_prim: Value,
    hash: i32,
) -> Option<Option<Value>> {
    let seg = seg_cur.object();
    // r9w15 (heapapi): this walk runs inside the caller's validate-once read
    // region, and `seg_cur` is the proof of entry. Every accessor below used to
    // pay an `is_object_address` membership probe — a random word of a bitmap
    // sized one bit per eight arena bytes, so a cache miss — and this function
    // makes about eight of them. MEASURED at 15.3 % of `ChmProbe get` across
    // the two probe layers on the 2026-09-20 `dev` build.
    //
    // The region's premise is this function's own documented contract, four
    // lines up: "No allocation, no Java, no pins: the caller must not be able
    // to reach a safepoint across it." Inside a region that is a BORROW, not a
    // comment — a cursor holds `&*ctx`, and every method that could reach a
    // safepoint takes `&mut self`. See `NativeHeapAccess::read_cursor`.
    //
    // A context with no cheaper answer (every mock, any other heap) hands back
    // an untrusted cursor and every call below is exactly the checked accessor
    // it replaced, so the ANSWER cannot differ by implementor.
    //
    // Each reference is read and promoted to a cursor in the SAME call: the
    // un-fused `reached(&c, read(&c))` spelling is two dynamic calls where the
    // checked accessor it replaces is one, and MEASURED, that alone gave back
    // most of what the region saved.
    // gen r4w4/rooting2: a fused `cursor_*_reference` answers `None` for TWO
    // different things -- a null slot (a real miss) and a non-null reference
    // the implementor declined to promote. This walk used to read both as
    // "absent", which is a definitive wrong answer rather than a decline. The
    // raw re-read below separates them; see `chm_walk_link_end`.
    let Some(buckets_cur) = ctx.cursor_field_volatile_reference(seg_cur, MAP_FIELD_BUCKETS) else {
        return chm_walk_link_end(ctx.cursor_field_volatile(seg_cur, MAP_FIELD_BUCKETS));
    };
    // r9w12: ONE membership walk for "is it an array" and "how long is it".
    //
    // This was `heap_kind_of(arr) == Array` followed by `array_length(arr)`,
    // and each of those is its own `is_object_address` probe plus its own
    // forwarding barrier — the `array_shape` doc says exactly this about the
    // `object_is_array` + `array_length` pair, and `al_fast_state` already
    // takes the fused form for the same reason. On the 2026-09-20 dev build
    // `ChmProbe get` spent 11.77 % + 3.54 % of its samples in the two
    // `is_object_address` layers, reached from ~15 checked heap accesses per
    // lookup; this removes one of them.
    //
    // `array_shape` answers `None` for a non-array -- but ALSO for any
    // implementor that does not override `object_is_array`, whose trait default
    // is `false` (`NativeHeapAccess::array_shape` is gated on it). That is the
    // whole of `chm_contains_key_agrees_with_get_under_relocation`'s red since
    // `4dee8ff19` (2026-09-20), which swapped `heap_kind_of == Array` for this
    // call: the test `MockCtx` implements `heap_kind_of` and not
    // `object_is_array`, so every bucket array read as "not an array" and every
    // wrapper-key `get`/`containsKey` answered ABSENT. The VM overrides both,
    // and its `array_shape` also answers `None` for an address its membership
    // walk rejects. Neither is a verdict about the key, so DECLINE (gen
    // r4w4/rooting2): the general path's `heap_kind_of` screen then decides,
    // and for a genuine non-array it answers the same miss this used to.
    //
    // The element type is not consulted here for the same reason it was not
    // before: a bucket array's elements are read through `get_array_element`,
    // which is type-faithful on its own.
    let (_, cap) = ctx.cursor_array_shape(&buckets_cur)?;
    let cap = i32::try_from(cap).ok()?;
    if cap <= 0 {
        return Some(None);
    }
    // Nodes of the real `HashMap$Node` class (hash:I key:L value:L next:L, the
    // JDK layout `get_node_*` sniffs as "slot 0 is an Int") are read with their
    // declared descriptors in one batched read; anything else goes through the
    // layout-sniffing `get_field` route, identical to `get_node_*`.
    let real_node_class = hm_node_class_memoized(ctx);
    // The element came out of the region's bucket array, so each successive
    // node — and each `next` link read off one — is reached, not re-probed.
    let bucket_idx = map_bucket_index(hash, cap);
    let mut node_cursor = ctx.cursor_array_element_reference(&buckets_cur, bucket_idx);
    if node_cursor.is_none() {
        return chm_walk_link_end(ctx.cursor_array_element(&buckets_cur, bucket_idx));
    }
    for _ in 0..CHM_WRAPPER_WALK_LIMIT {
        // Every `None` is resolved before the loop and at its bottom, so this
        // arm only guards the invariant.
        let Some(node_cur) = node_cursor else {
            return None;
        };
        let node = node_cur.object();
        let typed = real_node_class.is_some_and(|c| ctx.cursor_class_id(&node_cur) == c);
        let (node_hash, node_key, value_slot) = if typed {
            let mut out = [Value::Int(0), Value::Object(None)];
            ctx.cursor_fields_typed(
                &node_cur,
                &[(NODE_FIELD_HASH, b'I'), (NODE_FIELD_KEY, b'L')],
                &mut out,
            );
            let Value::Int(h) = out[0] else {
                return None;
            };
            (h, out[1], NODE_FIELD_VALUE)
        } else {
            // `get_node_hash` / `get_node_key` / `get_node_value`, fused so
            // slot 0 is read once instead of three times. Untyped: this is the
            // layout-SNIFFING arm, so it keeps `get_field`, whose descriptor
            // resolution is the thing being sniffed.
            match ctx.get_field(node, 0) {
                Value::Int(h) => (h, ctx.get_field(node, 1), 2),
                k @ Value::Object(_) => {
                    let h = match ctx.get_field(node, 2) {
                        Value::Int(h) => h,
                        _ => 0,
                    };
                    (h, k, 1)
                }
                _ => (0, ctx.get_field(node, 1), 2),
            }
        };
        if node_hash == hash {
            if let Value::Object(Some(nk)) = node_key {
                // r9w12: ONE membership walk for the node key's class AND its
                // primitive, instead of `class_id_of_object` followed by
                // `unbox_wrapper`, which reads the class id a second time
                // inside `fast_unbox_primitive_wrapper`. See
                // `NativeSystemAccess::class_id_and_unboxed_primitive`.
                //
                // The screen and the answer are unchanged: a class mismatch
                // still short-circuits to `false` without looking at the
                // primitive, and a key whose class matches but which the VM
                // door cannot unbox still declines to the general path. The
                // `None` (no fast door) arm keeps `unbox_wrapper`'s own
                // name-ladder route, which is what a mock context gives.
                //
                // r9w15: the CLASS screen — which is the arm every miss takes
                // — is now a region read, so the common case makes no probe at
                // all. Only a key whose class actually matches reaches the
                // fused door, and that is the arm that goes on to answer the
                // lookup.
                let equal = if std::ptr::eq(nk.as_ptr(), key.as_ptr()) {
                    true
                } else {
                    let nk_class = match ctx.reached(&node_cur, node_key) {
                        Some(nk_cur) => ctx.cursor_class_id(&nk_cur),
                        None => ctx.class_id_of_object(nk),
                    };
                    if nk_class != key_class {
                        false
                    } else {
                        let prim = match ctx.class_id_and_unboxed_primitive(nk).1 {
                            Some(prim) => prim,
                            None => unbox_wrapper(ctx, nk),
                        };
                        match prim {
                            Some(p) => wrapper_prims_equal(key_prim, p),
                            None => return None,
                        }
                    }
                };
                if equal {
                    let value = if typed {
                        ctx.cursor_field_typed(&node_cur, value_slot, b'L')
                    } else {
                        ctx.get_field(node, value_slot)
                    };
                    // In-flight `computeIfAbsent` reservation -- absent, as in
                    // `chm_seg_get`.
                    if let Value::Object(Some(v)) = value {
                        if std::ptr::eq(v.as_ptr(), seg.as_ptr()) {
                            return Some(None);
                        }
                    }
                    return Some(Some(value));
                }
            }
        }
        node_cursor = ctx.cursor_field_volatile_reference(&node_cur, NODE_FIELD_NEXT);
        if node_cursor.is_none() {
            return chm_walk_link_end(ctx.cursor_field_volatile(&node_cur, NODE_FIELD_NEXT));
        }
    }
    None
}

/// Resolve a `None` from a fused `cursor_*_reference` step of
/// [`chm_wrapper_chain_walk`] by the raw slot `raw` it was read from.
///
/// A null (or non-reference) slot is the end of the bucket or chain: a miss,
/// `Some(None)`. A NON-null reference means the implementor declined to promote
/// it -- the VM's `promote_reached` / `read_cursor` do that for an address their
/// membership walk rejects, and a context without a cursor override can do it
/// too -- and a declined link is not evidence that the key is absent, so the
/// walk declines (`None`) and the general path answers. The re-read costs one
/// checked access, and only on a path that was about to answer "absent".
///
/// The re-read can race a writer publishing a node into that slot. Either
/// answer is then a valid linearisation: null-then-node declines to the
/// general path, and node-then-null ends at a null that WAS the slot's value.
#[inline]
pub(crate) fn chm_walk_link_end(raw: Value) -> Option<Option<Value>> {
    match raw {
        Value::Object(Some(_)) => None,
        _ => Some(None),
    }
}

/// [`chm_wrapper_chain_walk`] made safe against a concurrent in-place resize:
/// optimistic under an even, unchanged stripe resize epoch, else redone under
/// the stripe read lock when it is free, else declined (`None`). Same
/// protocol as `native_chm_get_string_fast`.
pub(crate) fn chm_seg_get_wrapper_key(
    ctx: &dyn NativeContext,
    seg: ObjectRef,
    key: ObjectRef,
    key_class: ClassId,
    key_prim: Value,
    hash: i32,
) -> Option<Option<Value>> {
    // The ObjectRef-taking door: opens a region of its own. The per-`get` fast
    // path reaches `..._in_region` directly with a segment it already walked
    // to, so it does not pay this entry validation twice.
    let seg_cur = ctx.read_cursor(seg)?;
    chm_seg_get_wrapper_key_in_region(ctx, &seg_cur, key, key_class, key_prim, hash)
}

/// [`chm_seg_get_wrapper_key`] for a caller that is already inside a read
/// region and holds the segment as a cursor.
pub(crate) fn chm_seg_get_wrapper_key_in_region<'c>(
    ctx: &'c dyn NativeContext,
    seg_cur: &cratonvm_native_api::ReadCursor<'c>,
    key: ObjectRef,
    key_class: ClassId,
    key_prim: Value,
    hash: i32,
) -> Option<Option<Value>> {
    let seg_id = ctx.identity_hash_code(seg_cur.object());
    let resize_epoch = chm_seg_resize_epoch(seg_id);
    let before = resize_epoch.load(std::sync::atomic::Ordering::Acquire);
    if before & 1 == 0 {
        let walked = chm_wrapper_chain_walk(ctx, seg_cur, key, key_class, key_prim, hash);
        // Seqlock reader: the walk's loads must not drift past the re-check.
        std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
        if resize_epoch.load(std::sync::atomic::Ordering::Relaxed) == before {
            return walked;
        }
    }
    // A resize overlapped (or was in progress): walk again with the resizer
    // excluded -- but only if the stripe is free RIGHT NOW. This path must
    // never block: it is reached from compiled code outside the native funnel
    // (`jit_chm_get`), and a resizer holds the write lock across an
    // allocation that can request a collection, which would then wait for
    // this thread to reach a safepoint it is parked short of. Declining hands
    // the lookup to the general path, which takes the stripe lock inside the
    // funnel exactly as it always has.
    let _read_guard = chm_seg_lock_for(seg_id).try_read()?;
    chm_wrapper_chain_walk(ctx, seg_cur, key, key_class, key_prim, hash)
}

/// [`chm_seg_get_wrapper_key`] for a caller that already hashed `key` with
/// `chm_key_hash`: declines (`None`) unless `key` is a primitive wrapper whose
/// wrapper hash IS that hash.
#[inline]
pub(crate) fn chm_seg_get_wrapper_key_hashed(
    ctx: &dyn NativeContext,
    seg: ObjectRef,
    key: ObjectRef,
    hash: i32,
) -> Option<Option<Value>> {
    let (prim, wrapper_hash) = chm_wrapper_key_hash(ctx, key)?;
    if wrapper_hash != hash {
        return None;
    }
    let key_class = ctx.class_id_of_object(key);
    chm_seg_get_wrapper_key(ctx, seg, key, key_class, prim, hash)
}

/// The whole of `ConcurrentHashMap.get(key)` for a primitive-wrapper key, or
/// `None` to run the general path. The answer is `native_chm_get`'s.
pub(crate) fn chm_get_wrapper_key_fast(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    key: ObjectRef,
) -> Option<Value> {
    // r9w12: the key's class id and its primitive under ONE membership walk.
    // A `None` from the fused door (a context with no VM fast path, e.g. the
    // test mock) falls back to the name-ladder route, which is what this line
    // used to do unconditionally.
    let (key_class, unboxed) = ctx.class_id_and_unboxed_primitive(key);
    let prim = match unboxed {
        Some(prim) => prim,
        None => unbox_wrapper(ctx, key),
    }?;
    let (prim, hash) = chm_wrapper_key_hash_for(ctx, key_class, prim)?;
    // r9w15 (heapapi): the region opens HERE, not at the chain walk, so the
    // three accesses `chm_segment_for` makes on the way to the segment
    // (`this.segments`, its length, the element) are inside it too. With the
    // region opening at the walk instead, `ChmProbe get`'s profile still
    // showed `is_object_address` at 9.59 % — and those three plus the key's
    // own door are what was left. See `chm_segment_cursor_for`.
    let seg_cur = chm_segment_cursor_for(ctx, this, hash)?;
    chm_seg_get_wrapper_key_in_region(ctx, &seg_cur, key, key_class, prim, hash)
        .map(|hit| hit.unwrap_or(Value::Object(None)))
}

/// JIT leaf probe for `ConcurrentHashMap.get(Object)` (and the
/// `ConcurrentMap.get` registration of the same native): the answer
/// [`native_chm_get`] gives, for the key shapes it can give without Java --
/// primitive-wrapper keys (above) and `String` keys
/// (`native_chm_get_string_fast`).
///
/// Same contract as [`jit_overlay_hashmap_get`] / `jit_arraylist_get`: never
/// allocates on the Java heap, never dispatches Java code, never throws. A
/// `None` (null key -- the native's NPE --, any other key type, a map with no
/// segments yet, anything a fast path declines) means "run the site's
/// ordinary callback", which is `native_chm_get` itself.
pub fn jit_chm_get(ctx: &mut dyn NativeContext, this: ObjectRef, key: Value) -> Option<Value> {
    let Value::Object(Some(key_object)) = key else {
        return None;
    };
    if let Some(value) = chm_get_wrapper_key_fast(&*ctx, this, key_object) {
        return Some(value);
    }
    native_chm_get_string_fast(ctx, this, key_object)
}

pub fn native_chm_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    chm_reject_null_key(&key)?;
    if let Value::Object(Some(key_object)) = key {
        // r9w11 chm11: wrapper keys first -- the String path's opening memo
        // scan and String probe can only decline them.
        if let Some(value) = chm_get_wrapper_key_fast(&*ctx, this, key_object) {
            return Ok(Some(value));
        }
        if let Some(value) = native_chm_get_string_fast(ctx, this, key_object) {
            return Ok(Some(value));
        }
    }
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let key = read_pinned_elem(ctx, key_pin, key);
    let hash = chm_key_hash(ctx, &key)?;
    let this = ctx.read_native_pin(this_pin, this);
    let key = read_pinned_elem(ctx, key_pin, key);
    let result = match chm_segment_for(ctx, this, hash) {
        Some(seg) => Ok(Some(
            chm_seg_get(ctx, seg, key, hash)?.unwrap_or(Value::Object(None)),
        )),
        None => Ok(Some(Value::Object(None))),
    };
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_chm_contains_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    chm_reject_null_key(&key)?;
    // GC-safety: `chm_key_hash` dispatches the key's real `hashCode()`
    // (arbitrary Java), so it can complete a moving young GC — every sibling
    // CHM entry point pins across it and this one did not. The stale `this`
    // then made `chm_segment_for` read the segments array out of a
    // freed-and-reused address, so `containsKey` reported ABSENT for a key the
    // map demonstrably held. Caught deterministically by
    // `regression-suite/src/RMapGcStress.java` under `CRATONVM_DBG_GC_STRESS`
    // ("ConcurrentHashMap/filled: containsKey false for 0", with the
    // `CRATONVM_DBG_STALE_OBJREF` canary firing in the same native).
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let key = read_pinned_elem(ctx, key_pin, key);
    let result = (|| -> MethodCallResult {
        let hash = chm_key_hash(ctx, &key)?;
        let this = ctx.read_native_pin(this_pin, this);
        let key = read_pinned_elem(ctx, key_pin, key);
        match chm_segment_for(ctx, this, hash) {
            Some(seg) => {
                // Volatile-read path: a present mapping is detected by a
                // non-null Value::Object payload OR by walking the chain
                // and finding a key match (which `chm_seg_get` does). A
                // miss returns None here.
                match chm_seg_get(ctx, seg, key, hash)? {
                    Some(_) => Ok(Some(Value::Int(1))),
                    None => Ok(Some(Value::Int(0))),
                }
            }
            None => Ok(Some(Value::Int(0))),
        }
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_chm_get_or_default(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let default0 = args.get(2).copied().unwrap_or(Value::Object(None));
    chm_reject_null_key(&key)?;
    // cceres5: `chm_key_hash`/`chm_seg_get` dispatch hashCode/equals — the
    // raw default goes stale across them; the absent-key branch then
    // returned a pre-move address (see native_map_get_or_default's live
    // capture).
    let default_pin = pin_value(ctx, default0);
    let key_pin = pin_value(ctx, key);
    let first_pin = if default_pin == usize::MAX {
        key_pin
    } else {
        default_pin
    };
    let this_pin = ctx.pin_native_root(this);
    let hash = match chm_key_hash(ctx, &key) {
        Ok(h) => h,
        Err(e) => {
            if first_pin != usize::MAX {
                ctx.unpin_native_roots(first_pin);
            } else {
                ctx.unpin_native_roots(this_pin);
            }
            return Err(e);
        }
    };
    let this = ctx.read_native_pin(this_pin, this);
    let key = read_pinned_elem(ctx, key_pin, key);
    let result = match chm_segment_for(ctx, this, hash) {
        Some(seg) => chm_seg_get(ctx, seg, key, hash),
        None => Ok(None),
    };
    let default = read_pinned_elem(ctx, default_pin, default0);
    if first_pin != usize::MAX {
        ctx.unpin_native_roots(first_pin);
    } else {
        ctx.unpin_native_roots(this_pin);
    }
    match result? {
        Some(v) => Ok(Some(v)),
        None => Ok(Some(default)),
    }
}

/// `ConcurrentHashMap$ValuesView.contains(Object)` — `map.containsValue(o)`,
/// null refusal included. See the registration in `MAP_VIEW_CARRIERS`.
pub(crate) fn native_chm_values_view_contains(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if matches!(args.get(1), Some(Value::Object(None))) {
        return Err(bare_npe());
    }
    native_al_contains(ctx, args)
}

pub(crate) fn native_chm_contains_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // THE READ-SIDE half of this family's null axis, and the one a shim that
    // shares code with `HashMap` inherits wrong for free: `HashMap` sits in
    // this same file and `containsValue(null)` there is a legitimate question
    // with a legitimate `false`. `ConcurrentHashMap.containsValue` opens
    // `if (value == null) throw new NullPointerException();`, message-less
    // like the rest of the family. `contains(Object)` — the `Hashtable`-era
    // alias — is registered to this same body and inherits the refusal, which
    // is also what the JDK does (`contains` is `containsValue`).
    if matches!(args.get(1), Some(Value::Object(None))) {
        return Err(bare_npe());
    }
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    // Must scan all segments.
    //
    // gen r4w4/rooting2: each per-segment scan dispatches the target's
    // `equals()`, so a moving collection can run between two segments. The
    // segment list and the target used to be bare locals across it: every
    // segment after the first was scanned at a vacated address (answering
    // "absent") and the target was compared from its pre-move copy. Same
    // pin-and-re-read shape as `native_chm_clear`; `this_pin` is the unpin
    // base for every exit.
    let this_pin = ctx.pin_native_root(this);
    let target_pin = pin_value(ctx, target);
    let segs: Vec<Value> = chm_all_segments(ctx, this)
        .into_iter()
        .map(|s| Value::Object(Some(s)))
        .collect();
    let (_, seg_pins) = pin_value_slice(ctx, &segs);
    let result = (|| -> MethodCallResult {
        for i in 0..segs.len() {
            let seg = read_pinned_elem(ctx, seg_pins[i], segs[i]);
            let target = read_pinned_elem(ctx, target_pin, target);
            let found = native_map_contains_value(ctx, &[seg, target])?;
            if found == Some(Value::Int(1)) {
                return Ok(Some(Value::Int(1)));
            }
        }
        Ok(Some(Value::Int(0)))
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

// --- Core write operations (per-segment locking) ---

pub(crate) fn native_chm_put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    // C24 (HIGH): ConcurrentHashMap.put rejects null keys and null values per
    // JDK spec — `putVal`'s opening `if (key == null || value == null) throw
    // new NullPointerException();`. Message-less: see `chm_bare_npe_on_null`.
    chm_bare_npe_on_null(&key, &value)?;
    // GC-safety: `chm_key_hash` invokes arbitrary `hashCode()` bytecode and
    // `acquire_gc_safe`'s contended wait can span a completed moving GC, so
    // every raw local captured above must be pinned and re-read before use.
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let value_pin = pin_value(ctx, value);
    let result = (|| -> MethodCallResult {
        let key = read_pinned_elem(ctx, key_pin, key);
        let hash = chm_key_hash(ctx, &key)?;
        let this = ctx.read_native_pin(this_pin, this);
        match chm_segment_for_mut(ctx, this, hash) {
            Some(seg) => {
                let _resize_flag = ChmResizeLockGuard::enter();
                let (_guard, seg) = ChmMonitorGuard::acquire_gc_safe(ctx, seg);
                let key = read_pinned_elem(ctx, key_pin, key);
                let value = read_pinned_elem(ctx, value_pin, value);
                chm_seg_put_hashed(ctx, seg, key, value, hash, false)
            }
            None => Ok(Some(Value::Object(None))),
        }
    })();
    // A CHM value is never null, so a null answer is an insert.
    if matches!(result, Ok(Some(Value::Object(None)))) {
        let this_now = ctx.read_native_pin(this_pin, this);
        chm_note_table_high_water(ctx, this_now, false);
    }
    ctx.unpin_native_roots(this_pin);
    result
}

/// `CRATONVM_COMPAT_CHM_SINGLE_HASH` — default ON, `=0` restores the old
/// count. The JDK's `ConcurrentHashMap.put` / `remove` call the key's
/// `hashCode()` ONCE (`spread(key.hashCode())`). The natives hashed it to pick
/// the segment and then again inside the segment's `HashMap` put / remove, so
/// every `put`, overwrite and `remove` made two calls. Round 13 wave 8, lane
/// hashcompat3 (`R13HashcompatEqualsOrder`, `ConcurrentHashMap` lines).
pub(crate) fn chm_single_hash_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_CHM_SINGLE_HASH")
}

/// Put into one CHM segment with the [`chm_key_hash`] the caller already took
/// (the segment's own bucket hash is the same `map_hash_key`). A segment is a
/// fabricated `ClassId(0)` bucket map that none of `native_map_put`'s receiver
/// routes (TreeMap, CHM, LHM, `Hashtable`, the integer overlay, which only
/// serves an exact `HashMap`) ever claims, so going straight to the pinned
/// put changes nothing but the second `hashCode()`.
///
/// `only_if_absent` serves `putIfAbsent` (`putVal(key, value, true)`), whose
/// composite (`get` + `put`) hashed the key twice more.
pub(crate) fn chm_seg_put_hashed(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    key: Value,
    value: Value,
    hash: i32,
    only_if_absent: bool,
) -> MethodCallResult {
    if !chm_single_hash_enabled() {
        let args = [Value::Object(Some(seg)), key, value];
        return if only_if_absent {
            native_map_put_if_absent(ctx, &args)
        } else {
            native_map_put(ctx, &args)
        };
    }
    let mut seg = seg;
    let put_pin_base = ctx.pin_native_root(seg);
    let key_pin = pin_value(ctx, key);
    let value_pin = pin_value(ctx, value);
    let result = native_map_put_evict_pinned_hashed(
        ctx,
        &mut seg,
        key,
        value,
        true,
        put_pin_base,
        key_pin,
        value_pin,
        GrowAlloc::SingleAttempt,
        Some(hash),
        only_if_absent,
    );
    ctx.unpin_native_roots(put_pin_base);
    result
}

/// The `remove` twin of [`chm_seg_put_hashed`].
pub(crate) fn chm_seg_remove_hashed(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    key: Value,
    hash: i32,
) -> MethodCallResult {
    if !chm_single_hash_enabled() {
        return native_map_remove(ctx, &[Value::Object(Some(seg)), key]);
    }
    let seg_pin = ctx.pin_native_root(seg);
    let key_pin = pin_value(ctx, key);
    let seg = ctx.read_native_pin(seg_pin, seg);
    let key = read_pinned_elem(ctx, key_pin, key);
    let result = native_map_remove_pinned_hashed(ctx, seg, key, seg_pin, key_pin, Some(hash));
    ctx.unpin_native_roots(seg_pin);
    result
}

/// `CRATONVM_COMPAT_CHM_REMAP_SINGLE_HASH` — default ON, `=0` restores the
/// composites. `ConcurrentHashMap.computeIfAbsent` / `compute` /
/// `computeIfPresent` / `merge` call the key's `hashCode()` ONCE
/// (`spread(key.hashCode())`) and walk its bin under the bin lock. The natives
/// hashed it to pick the segment and then ran `HashMap` composites over the
/// segment that hashed it again for every step: `computeIfAbsent` of an
/// absent key made five calls (the segment pick, then the reservation
/// protocol's `get`, `put` marker, `get`, `put` value), `compute` / `merge` /
/// `computeIfPresent` three. Every segment step now takes the caller's hash
/// ([`chm_seg_get_raw`], [`chm_seg_put_hashed`], [`chm_seg_remove_hashed`]),
/// and `compute` / `computeIfPresent` / `merge` run [`chm_seg_remap_hashed`]
/// instead of the `HashMap` composite, which also stops it throwing
/// `ConcurrentModificationException` when the function writes another key of
/// the same segment (the JDK has no such check; its recursion check is
/// [`chm_reject_recursive_update`]). Round 13 wave 10, lane compat5 (residual
/// page item 5, the `ConcurrentHashMap` half).
pub(crate) fn chm_remap_single_hash_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_CHM_REMAP_SINGLE_HASH")
}

/// The reservation protocol's segment read: [`chm_seg_get_raw`] with the
/// caller's hash, or the old `native_map_get` (a `hashCode()` of its own)
/// under `CRATONVM_COMPAT_CHM_REMAP_SINGLE_HASH=0`.
pub(crate) fn chm_remap_seg_get(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    key: Value,
    hash: i32,
) -> MethodCallResult {
    if chm_remap_single_hash_enabled() {
        chm_seg_get_raw(ctx, seg, key, hash)
    } else {
        native_map_get(ctx, &[Value::Object(Some(seg)), key])
    }
}

/// The reservation protocol's segment write (see [`chm_remap_seg_get`]).
pub(crate) fn chm_remap_seg_put(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    key: Value,
    value: Value,
    hash: i32,
) -> MethodCallResult {
    if chm_remap_single_hash_enabled() {
        chm_seg_put_hashed(ctx, seg, key, value, hash, false)
    } else {
        native_map_put(ctx, &[Value::Object(Some(seg)), key, value])
    }
}

/// The reservation protocol's segment removal (see [`chm_remap_seg_get`]).
pub(crate) fn chm_remap_seg_remove(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    key: Value,
    hash: i32,
) -> MethodCallResult {
    if chm_remap_single_hash_enabled() {
        chm_seg_remove_hashed(ctx, seg, key, hash)
    } else {
        native_map_remove(ctx, &[Value::Object(Some(seg)), key])
    }
}

/// `compute` (`arg` unused), `computeIfPresent` (`arg` unused) or `merge`
/// (`arg` is the value) on one CHM segment the caller holds the monitor of,
/// with the caller's [`chm_key_hash`]: the JDK bodies' outcomes -- an absent
/// key runs `compute`'s function with `null`, is `merge`d to `arg` without
/// calling the function, and is left alone by `computeIfPresent`; a null
/// result removes a present mapping -- with no `hashCode()` call. A
/// reservation of another thread's `computeIfAbsent` reads as absent, as it
/// does for every lock-free reader.
pub(crate) fn chm_seg_remap_hashed(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    key: Value,
    arg: Value,
    func: Value,
    hash: i32,
    op: RemapOp,
) -> MethodCallResult {
    if chm_remap_single_walk_enabled() {
        return chm_seg_remap_single_walk(ctx, seg, key, arg, func, hash, op);
    }
    let null = Value::Object(None);
    let seg_pin = ctx.pin_native_root(seg);
    let key_pin = pin_value(ctx, key);
    let arg_pin = pin_value(ctx, arg);
    let func_pin = pin_value(ctx, func);
    // Every pin above `seg_pin` is released with it, on every exit.
    let result = (|| -> MethodCallResult {
        let seg_now = ctx.read_native_pin(seg_pin, seg);
        let key_now = read_pinned_elem(ctx, key_pin, key);
        let old = chm_seg_get(ctx, seg_now, key_now, hash)?.unwrap_or(null);
        let present = !matches!(old, Value::Object(None));
        let new_val = match op {
            RemapOp::Merge if !present => read_pinned_elem(ctx, arg_pin, arg),
            RemapOp::IfPresent if !present => return Ok(Some(null)),
            _ => {
                let old_pin = pin_value(ctx, old);
                let Value::Object(Some(f)) = read_pinned_elem(ctx, func_pin, func) else {
                    return Ok(Some(null));
                };
                let old_now = read_pinned_elem(ctx, old_pin, old);
                let fn_args = if op == RemapOp::Merge {
                    [old_now, read_pinned_elem(ctx, arg_pin, arg)]
                } else {
                    [read_pinned_elem(ctx, key_pin, key), old_now]
                };
                ctx.invoke_virtual(
                    f,
                    "apply",
                    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                    &fn_args,
                )?
                .unwrap_or(null)
            }
        };
        let seg_now = ctx.read_native_pin(seg_pin, seg);
        let key_now = read_pinned_elem(ctx, key_pin, key);
        if matches!(new_val, Value::Object(None)) {
            if present {
                chm_seg_remove_hashed(ctx, seg_now, key_now, hash)?;
            }
            return Ok(Some(null));
        }
        let new_pin = pin_value(ctx, new_val);
        let new_now = read_pinned_elem(ctx, new_pin, new_val);
        chm_seg_put_hashed(ctx, seg_now, key_now, new_now, hash, false)?;
        Ok(Some(read_pinned_elem(ctx, new_pin, new_val)))
    })();
    ctx.unpin_native_roots(seg_pin);
    result
}

/// `CRATONVM_COMPAT_CHM_REMAP_SINGLE_WALK` — default ON, `=0` restores the
/// `get` + `put`/`remove` body. `ConcurrentHashMap.compute` /
/// `computeIfPresent` / `merge` walk the key's bin ONCE under the bin lock:
/// the node found is written in place (`e.val = val`) or unlinked, and an
/// absent key is appended after the node the walk ended on (`pred.next = new
/// Node(h, key, val)`). [`chm_seg_remap_hashed`] looked the key up, ran the
/// function, then walked the chain again to `put` or `remove` it, so a
/// colliding key saw every same-hash `equals` twice. And `computeIfPresent`
/// probed the segment lock-free first with a third, full lookup, where the
/// JDK's only lock-free question is whether the bin is empty: that probe now
/// asks only whether the bin holds the key's HASH ([`chm_seg_bin_holds_hash`],
/// no `equals`), which keeps the lock-free miss for every key that cannot be
/// there. Round 13 wave 11, lane compat6
/// (`r13w10-compat5-map-view-residuals-CLOSED-20260929.md` item 5).
pub(crate) fn chm_remap_single_walk_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_CHM_REMAP_SINGLE_WALK")
}

/// Whether bin `hash` of the CHM segment `seg` holds a node whose stored hash
/// is `hash` -- a lookup without its `equals`. Taken under the stripe read
/// lock, as [`chm_seg_get`] snapshots its chain. No Java; the lock wait is
/// GC-safe.
pub(crate) fn chm_seg_bin_holds_hash(ctx: &mut dyn NativeContext, seg: ObjectRef, hash: i32) -> bool {
    // The walk bound, read before the stripe lock (it takes no other lock
    // under it, as `chm_seg_get` does not).
    let walk_cap = chain_walk_limit(map_state(&*ctx, seg).1);
    let seg_pin = ctx.pin_native_root(seg);
    let seg_id = ctx.identity_hash_code(seg);
    let read_guard = chm_seg_read_gc_safe(ctx, seg_id);
    let seg = ctx.read_native_pin(seg_pin, seg);
    let mut found = false;
    if let Value::Object(Some(buckets)) = ctx.get_field_volatile(seg, MAP_FIELD_BUCKETS) {
        if ctx.heap_kind_of(buckets) == ObjectKind::Array {
            let cap = ctx.array_length(buckets) as i32;
            if cap > 0 {
                let mut cur = ctx.get_array_element(buckets, map_bucket_index(hash, cap));
                for _ in 0..walk_cap {
                    let Value::Object(Some(n)) = cur else {
                        break;
                    };
                    if get_node_hash(&*ctx, n) == hash {
                        found = true;
                        break;
                    }
                    cur = ctx.get_field_volatile(n, NODE_FIELD_NEXT);
                }
            }
        }
    }
    drop(read_guard);
    ctx.unpin_native_roots(seg_pin);
    found
}

/// [`chm_seg_remap_hashed`] as the JDK bodies walk: see
/// [`chm_remap_single_walk_enabled`]. The caller holds the segment monitor.
/// A reservation (another thread's `computeIfAbsent` in flight, the segment
/// object stored as the value) reads as absent, as before, and an insert over
/// it writes that node, which is what the old `put` did. An absent key is
/// appended without a second walk only while the function left the segment's
/// size, table and bin as the walk saw them; otherwise the old `put` decides.
pub(crate) fn chm_seg_remap_single_walk(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    key: Value,
    arg: Value,
    func: Value,
    hash: i32,
    op: RemapOp,
) -> MethodCallResult {
    let null = Value::Object(None);
    let seg_pin = ctx.pin_native_root(seg);
    let key_pin = pin_value(ctx, key);
    let arg_pin = pin_value(ctx, arg);
    let func_pin = pin_value(ctx, func);
    // Every pin above `seg_pin` is released with it, on every exit.
    let result = (|| -> MethodCallResult {
        let seg_now = ctx.read_native_pin(seg_pin, seg);
        let (buckets, size, cap) = map_state(&*ctx, seg_now);
        let start = match buckets {
            Some(b) => ctx.get_array_element(b, map_bucket_index(hash, cap)),
            None => null,
        };
        // What the function must leave alone for "absent" to still hold.
        let before = (
            size,
            cap,
            buckets.is_some(),
            chain_len(&*ctx, start, chain_walk_limit(size)),
        );
        let key_now = read_pinned_elem(ctx, key_pin, key);
        let node = map_chain_find_from(
            ctx,
            start,
            hash,
            key_pin,
            key_now,
            false,
            chain_walk_limit(size),
            TreeLookup::Off,
        )?;
        let node_pin = node.map(|n| ctx.pin_native_root(n));
        let seg_now = ctx.read_native_pin(seg_pin, seg);
        let old = match (node, node_pin) {
            (Some(n), Some(p)) => {
                let n = ctx.read_native_pin(p, n);
                match get_node_value(&*ctx, n) {
                    Value::Object(Some(v)) if std::ptr::eq(v.as_ptr(), seg_now.as_ptr()) => null,
                    v => v,
                }
            }
            _ => null,
        };
        let present = !matches!(old, Value::Object(None));
        let new_val = match op {
            RemapOp::Merge if !present => read_pinned_elem(ctx, arg_pin, arg),
            RemapOp::IfPresent if !present => return Ok(Some(null)),
            _ => {
                let old_pin = pin_value(ctx, old);
                let Value::Object(Some(f)) = read_pinned_elem(ctx, func_pin, func) else {
                    return Ok(Some(null));
                };
                let old_now = read_pinned_elem(ctx, old_pin, old);
                let fn_args = if op == RemapOp::Merge {
                    [old_now, read_pinned_elem(ctx, arg_pin, arg)]
                } else {
                    [read_pinned_elem(ctx, key_pin, key), old_now]
                };
                ctx.invoke_virtual(
                    f,
                    "apply",
                    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                    &fn_args,
                )?
                .unwrap_or(null)
            }
        };
        let seg_now = ctx.read_native_pin(seg_pin, seg);
        let node_now = match (node, node_pin) {
            (Some(n), Some(p)) => Some(ctx.read_native_pin(p, n)),
            _ => None,
        };
        if matches!(new_val, Value::Object(None)) {
            // `pred.next = e.next` / `setTabAt(tab, i, e.next)`.
            if present {
                if let Some(n) = node_now {
                    map_unlink_node(ctx, seg_now, n);
                }
            }
            return Ok(Some(null));
        }
        let new_pin = pin_value(ctx, new_val);
        let new_now = read_pinned_elem(ctx, new_pin, new_val);
        if let Some(n) = node_now {
            // `e.val = val` (a reservation node too: the old `put` wrote it).
            set_node_value(ctx, n, new_now);
            return Ok(Some(read_pinned_elem(ctx, new_pin, new_val)));
        }
        let (buckets, size, cap) = map_state(&*ctx, seg_now);
        let start = match buckets {
            Some(b) => ctx.get_array_element(b, map_bucket_index(hash, cap)),
            None => null,
        };
        let after = (
            size,
            cap,
            buckets.is_some(),
            chain_len(&*ctx, start, chain_walk_limit(size)),
        );
        let key_now = read_pinned_elem(ctx, key_pin, key);
        if after == before && buckets.is_some() {
            // `pred.next = new Node<K,V>(h, key, val)`: no second walk.
            let mut seg_mut = seg_now;
            let put_base = ctx.pin_native_root(seg_mut);
            let k_pin = pin_value(ctx, key_now);
            let v_pin = pin_value(ctx, new_now);
            let put = native_map_put_evict_pinned_inner(
                ctx,
                &mut seg_mut,
                key_now,
                new_now,
                true,
                put_base,
                k_pin,
                v_pin,
                GrowAlloc::SingleAttempt,
                Some(hash),
                false,
                PutMode {
                    no_grow: false,
                    assume_absent: true,
                },
            );
            ctx.unpin_native_roots(put_base);
            put?;
        } else {
            chm_seg_put_hashed(ctx, seg_now, key_now, new_now, hash, false)?;
        }
        Ok(Some(read_pinned_elem(ctx, new_pin, new_val)))
    })();
    ctx.unpin_native_roots(seg_pin);
    result
}

pub(crate) fn native_chm_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    // ConcurrentHashMap.remove(null) throws NPE per JDK spec (null keys are not
    // permitted), mirroring the guard in native_chm_put. Without this, Spring's
    // SimpleAliasRegistry.removeAlias(null) returned silently instead of NPE
    // (SimpleAliasRegistryTests.removeNullAlias).
    chm_reject_null_key(&key)?;
    // GC-safety: pin across hashCode() dispatch and the (GC-pausable)
    // segment-lock wait — see `native_chm_put`.
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let result = (|| -> MethodCallResult {
        let key = read_pinned_elem(ctx, key_pin, key);
        let hash = chm_key_hash(ctx, &key)?;
        let this = ctx.read_native_pin(this_pin, this);
        match chm_segment_for(ctx, this, hash) {
            Some(seg) => {
                let _resize_flag = ChmResizeLockGuard::enter();
                let (_guard, seg) = ChmMonitorGuard::acquire_gc_safe(ctx, seg);
                let key = read_pinned_elem(ctx, key_pin, key);
                chm_seg_remove_hashed(ctx, seg, key, hash)
            }
            None => Ok(Some(Value::Object(None))),
        }
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_chm_put_if_absent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    // C24 (HIGH): JDK rejects null key/value. `putIfAbsent` is
    // `putVal(key, value, true)`, i.e. the same message-less refusal as `put`.
    chm_bare_npe_on_null(&key, &value)?;
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let value_pin = pin_value(ctx, value);
    let result = (|| -> MethodCallResult {
        let key = read_pinned_elem(ctx, key_pin, key);
        let hash = chm_key_hash(ctx, &key)?;
        let this = ctx.read_native_pin(this_pin, this);
        match chm_segment_for_mut(ctx, this, hash) {
            Some(seg) => {
                let _resize_flag = ChmResizeLockGuard::enter();
                // GC-pausable contended wait — re-read the pinned locals
                // AFTER acquiring (see `acquire_gc_safe`'s doc).
                let (_guard, seg) = ChmMonitorGuard::acquire_gc_safe(ctx, seg);
                let key = read_pinned_elem(ctx, key_pin, key);
                let value = read_pinned_elem(ctx, value_pin, value);
                chm_seg_put_hashed(ctx, seg, key, value, hash, true)
            }
            None => Ok(Some(Value::Object(None))),
        }
    })();
    if matches!(result, Ok(Some(Value::Object(None)))) {
        let this_now = ctx.read_native_pin(this_pin, this);
        chm_note_table_high_water(ctx, this_now, false);
    }
    ctx.unpin_native_roots(this_pin);
    return result;

    #[allow(unreachable_code)]
    let hash = chm_key_hash(ctx, &key)?;
    match chm_segment_for_mut(ctx, this, hash) {
        Some(seg) => {
            let _resize_flag = ChmResizeLockGuard::enter();
            let _guard = ChmMonitorGuard::acquire(ctx, seg);
            native_map_put_if_absent(ctx, &[Value::Object(Some(seg)), key, value])
        }
        None => Ok(Some(Value::Object(None))),
    }
}

/// CHM-mapper-deadlock fix (2026-07-21): `computeIfAbsent`'s mapping
/// function must NOT run while holding the per-segment Java monitor.
///
/// The JDK runs the mapper under a PER-BIN lock (a `ReservationNode` placed
/// in the empty bin), so operations on other keys never order against it.
/// Our 16-way segment monitor is far coarser: running the mapper under it
/// lets a mapper that takes an unrelated lock deadlock against that lock's
/// holder doing a CHM op on a DIFFERENT key of the same segment. Live
/// capture (WildFly `parallel-extension-add`, trace7-a2): tid=31's
/// `NodeSubregistry.registerChild` mapper held segment monitor B and blocked
/// on the `CapabilityRegistry` write lock, while tid=40 held that write lock
/// and blocked on B from `registerPossibleCapability`'s `computeIfAbsent`
/// \-- an AB-BA impossible on HotSpot, and the dominant cause of the
/// "Could not start container" suite failure.
///
/// Reservation protocol (JDK `ReservationNode` equivalent):
///   Phase 1 \-- under the segment monitor: if the key maps to another
///     thread's reservation, `Object.wait` on the segment and re-check
///     (same-key semantics preserved: at most one mapper per install);
///     if a real value appeared, adopt it; otherwise install the SEGMENT
///     OBJECT ITSELF as the reservation marker (user code can never store
///     a segment as a value, and both copies are GC-remapped, so identity
///     comparison stays sound across moving collections).
///   Phase 2 \-- with NO monitor held: invoke the mapping function.
///   Phase 3 \-- under the monitor again: if our reservation is still in
///     place, replace it with the computed value (or remove it when the
///     mapper returned null / threw), then `notifyAll` the segment so
///     same-key waiters re-check. If a racing plain `put`/`remove` replaced
///     the marker, the raced-in state wins (a small deviation from JDK
///     ordering, which would have blocked that `put` on the bin lock \--
///     accepted: the marker is never exposed and no mapper result is
///     silently double-applied).
///
/// Lock-free readers observe the marker as ABSENT via the filter in
/// `chm_seg_get` \-- the same visibility the JDK gives `ReservationNode`.
pub(crate) fn chm_compute_if_absent_absent_path(
    ctx: &mut dyn NativeContext,
    roots_base: usize,
    this: ObjectRef,
    hash: i32,
    key_pin: usize,
    key: Value,
    func_pin: usize,
    func: Value,
) -> MethodCallResult {
    // ---- Phase 1: reserve (or adopt) under the segment monitor. ----
    let this_now = ctx.read_native_pin(roots_base, this);
    let Some(seg) = chm_segment_for(ctx, this_now, hash) else {
        return Ok(Some(Value::Object(None)));
    };
    let seg_pin = ctx.pin_native_root(seg);
    let entered = ctx.monitor_enter_gc_safe(seg);
    let mut segc = ctx.read_native_pin(seg_pin, entered);
    let mut adopted: Option<Value> = None;
    let mut reserve_err: Option<MethodCallFailed> = None;
    loop {
        let key_now = read_pinned_elem(ctx, key_pin, key);
        let cur = {
            let _resize_flag = ChmResizeLockGuard::enter();
            match chm_remap_seg_get(ctx, segc, key_now, hash) {
                Ok(v) => v,
                Err(e) => {
                    reserve_err = Some(e);
                    break;
                }
            }
        };
        segc = ctx.read_native_pin(seg_pin, segc);
        match cur {
            Some(Value::Object(Some(v))) if std::ptr::eq(v.as_ptr(), segc.as_ptr()) => {
                // Another thread's mapper is in flight for this key \--
                // wait exactly like the JDK waits on the reserved bin.
                let _ = ctx.monitor_wait(segc, Some(50));
                segc = ctx.read_native_pin(seg_pin, segc);
            }
            Some(v @ Value::Object(Some(_))) => {
                adopted = Some(v);
                break;
            }
            _ => {
                let key_now = read_pinned_elem(ctx, key_pin, key);
                let _resize_flag = ChmResizeLockGuard::enter();
                let marker = Value::Object(Some(segc));
                if let Err(e) = chm_remap_seg_put(ctx, segc, key_now, marker, hash) {
                    reserve_err = Some(e);
                }
                segc = ctx.read_native_pin(seg_pin, segc);
                break;
            }
        }
    }
    ctx.monitor_exit(segc);
    ctx.unpin_native_roots(seg_pin);
    if let Some(e) = reserve_err {
        return Err(e);
    }
    if let Some(v) = adopted {
        return Ok(Some(v));
    }

    // ---- Phase 2: run the mapping function with NO monitor held. ----
    let func_now = read_pinned_elem(ctx, func_pin, func);
    let key_now = read_pinned_elem(ctx, key_pin, key);
    let mapped: Result<Option<Value>, MethodCallFailed> = match func_now {
        Value::Object(Some(f)) => ctx.invoke_virtual(
            f,
            "apply",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[key_now],
        ),
        _ => Ok(Some(Value::Object(None))),
    };
    let mapped_val = match &mapped {
        Ok(Some(v)) => *v,
        _ => Value::Object(None),
    };
    let mapped_pin = pin_value(ctx, mapped_val);

    // ---- Phase 3: commit under the monitor; wake same-key waiters. ----
    let this_now = ctx.read_native_pin(roots_base, this);
    let commit: Result<Value, MethodCallFailed> = match chm_segment_for(ctx, this_now, hash) {
        None => Ok(Value::Object(None)),
        Some(seg3) => {
            let seg_pin = ctx.pin_native_root(seg3);
            let entered = ctx.monitor_enter_gc_safe(seg3);
            let mut segc = ctx.read_native_pin(seg_pin, entered);
            let mut result: Result<Value, MethodCallFailed> = Ok(Value::Object(None));
            let key_now = read_pinned_elem(ctx, key_pin, key);
            let cur = {
                let _resize_flag = ChmResizeLockGuard::enter();
                chm_remap_seg_get(ctx, segc, key_now, hash)
            };
            segc = ctx.read_native_pin(seg_pin, segc);
            match cur {
                Err(e) => result = Err(e),
                Ok(cur) => {
                    let ours = matches!(
                        cur,
                        Some(Value::Object(Some(v))) if std::ptr::eq(v.as_ptr(), segc.as_ptr())
                    );
                    if ours {
                        let mv = read_pinned_elem(ctx, mapped_pin, mapped_val);
                        let key_now = read_pinned_elem(ctx, key_pin, key);
                        let _resize_flag = ChmResizeLockGuard::enter();
                        if mapped.is_ok() && !matches!(mv, Value::Object(None)) {
                            match chm_remap_seg_put(ctx, segc, key_now, mv, hash) {
                                // gen r4w4/rooting2: the put runs the key's
                                // `hashCode()`/`equals()` and allocates the
                                // node, so `mv` is re-read from its pin before
                                // it is handed back to Java.
                                Ok(_) => result = Ok(read_pinned_elem(ctx, mapped_pin, mv)),
                                Err(e) => result = Err(e),
                            }
                        } else {
                            match chm_remap_seg_remove(ctx, segc, key_now, hash) {
                                Ok(_) => result = Ok(Value::Object(None)),
                                Err(e) => result = Err(e),
                            }
                        }
                        segc = ctx.read_native_pin(seg_pin, segc);
                    } else {
                        // A racing mutator replaced our marker \-- its state
                        // wins; surface whatever the map now holds.
                        result = Ok(match cur {
                            Some(v) => v,
                            None => Value::Object(None),
                        });
                    }
                }
            }
            let _ = ctx.monitor_notify_all(segc);
            ctx.monitor_exit(segc);
            ctx.unpin_native_roots(seg_pin);
            result
        }
    };
    match mapped {
        Err(e) => Err(e),
        Ok(_) => commit.map(Some),
    }
}

/// `computeIfAbsent`'s body before round 13 wave 13: a full lock-free lookup
/// (a present key answers without the segment monitor), then the reservation
/// protocol of [`chm_compute_if_absent_absent_path`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn chm_compute_if_absent_probe_then_reserve(
    ctx: &mut dyn NativeContext,
    roots_base: usize,
    this: ObjectRef,
    seg: ObjectRef,
    hash: i32,
    key_pin: usize,
    key: Value,
    func_pin: usize,
    func: Value,
) -> MethodCallResult {
    let key_now = read_pinned_elem(ctx, key_pin, key);
    match chm_seg_get(ctx, seg, key_now, hash)? {
        Some(existing) if !matches!(existing, Value::Object(None)) => Ok(Some(existing)),
        _ => chm_compute_if_absent_absent_path(
            ctx, roots_base, this, hash, key_pin, key, func_pin, func,
        ),
    }
}

/// `CRATONVM_COMPAT_CHM_CIA_SINGLE_WALK` — default ON, `=0` restores the
/// probe-then-reserve body. `ConcurrentHashMap.computeIfAbsent` tests the
/// bin's FIRST node lock-free and otherwise walks the bin once (under its
/// lock), running the mapper for an absent key at the end of that walk and
/// linking the new node after the last one (`pred.next = new Node(..)`). The
/// native reservation protocol walked a colliding chain five times for an
/// absent key -- the lock-free probe, the reservation's lookup and its `put`,
/// the commit's lookup and its `put` -- so `key.equals` ran five times per
/// same-hash node where HotSpot runs it once (plus the first-node test). Now
/// [`chm_compute_if_absent_single_walk`]: the first-node test, one walk of a
/// snapshot of the bin, a reservation node linked without comparing any key
/// the walk already compared, and a commit that finds that NODE by identity.
/// The mapper still never runs under the segment monitor (the WildFly deadlock
/// recorded at [`chm_compute_if_absent_absent_path`]), and a present key
/// still answers without taking it. Round 13 wave 13, lane compat7 (proposal
/// C6-2; `r13w10-compat5-map-view-residuals-CLOSED-20260929.md` item 5).
pub(crate) fn chm_cia_single_walk_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_CHM_CIA_SINGLE_WALK")
}

/// Longest bin [`chm_compute_if_absent_single_walk`] snapshots (each node is
/// pinned across the key comparisons); a longer one takes the old body.
pub(crate) const CHM_CIA_SNAPSHOT_CAP: usize = 64;

/// The nodes of bin `hash` of the CHM segment `seg`, in `next` order, read
/// under the stripe read lock as [`chm_seg_get`] snapshots its chain, or
/// `None` when the segment has no table or the bin holds more than `cap`
/// nodes. No Java. The answers are bare references with nothing allocated
/// since they were read: the caller pins them before it runs anything.
pub(crate) fn chm_seg_bin_snapshot(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    hash: i32,
    cap: usize,
) -> Option<Vec<ObjectRef>> {
    let seg_pin = ctx.pin_native_root(seg);
    let seg_id = ctx.identity_hash_code(seg);
    let read_guard = chm_seg_read_gc_safe(ctx, seg_id);
    let seg = ctx.read_native_pin(seg_pin, seg);
    let mut nodes = Vec::new();
    let mut complete = false;
    if let Value::Object(Some(buckets)) = ctx.get_field_volatile(seg, MAP_FIELD_BUCKETS) {
        if ctx.heap_kind_of(buckets) == ObjectKind::Array {
            let len = ctx.array_length(buckets) as i32;
            if len > 0 {
                complete = true;
                let mut cur = ctx.get_array_element(buckets, map_bucket_index(hash, len));
                while let Value::Object(Some(n)) = cur {
                    if nodes.len() >= cap {
                        complete = false;
                        break;
                    }
                    nodes.push(n);
                    cur = ctx.get_field_volatile(n, NODE_FIELD_NEXT);
                }
            }
        }
    }
    drop(read_guard);
    ctx.unpin_native_roots(seg_pin);
    complete.then_some(nodes)
}

/// Whether the node rooted at `pin` still holds this segment's reservation
/// marker (the segment object itself, see
/// [`chm_compute_if_absent_absent_path`]).
pub(crate) fn chm_node_holds_marker(ctx: &dyn NativeContext, node: ObjectRef, seg: ObjectRef) -> bool {
    matches!(get_node_value(ctx, node), Value::Object(Some(v)) if std::ptr::eq(v.as_ptr(), seg.as_ptr()))
}

/// Whether `node` is linked in bin `hash` of `seg`'s current table (an
/// identity walk: no Java). The caller holds the segment monitor.
pub(crate) fn chm_seg_bin_links(ctx: &dyn NativeContext, seg: ObjectRef, hash: i32, node: ObjectRef) -> bool {
    let (buckets, size, cap) = map_state(ctx, seg);
    let Some(buckets) = buckets else {
        return false;
    };
    let mut cur = ctx.get_array_element(buckets, map_bucket_index(hash, cap));
    for _ in 0..chain_walk_limit(size) {
        match cur {
            Value::Object(Some(n)) if std::ptr::eq(n.as_ptr(), node.as_ptr()) => return true,
            Value::Object(Some(n)) => cur = ctx.get_field(n, NODE_FIELD_NEXT),
            _ => return false,
        }
    }
    false
}

/// `ConcurrentHashMap.computeIfAbsent` in the JDK's walk
/// (`CRATONVM_COMPAT_CHM_CIA_SINGLE_WALK`), with this VM's reservation so the
/// mapper runs with no monitor held:
///
/// 1. lock-free, over a snapshot of the bin: `computeIfAbsent`'s first-node
///    test (`fh == h && key.equals(fk) && fv != null`: return `fv`), then the
///    walk from that first node (`e.hash == h && key.equals(ek)`). A present
///    key answers here, without the segment monitor, as before;
/// 2. under the monitor: the bin is walked by IDENTITY, and only a node the
///    snapshot did not hold (one linked since) is compared by `equals`; the
///    reservation node is then linked at the chain's end with no comparison
///    (keys are final, so a snapshot node that did not match still does not);
/// 3. the mapper, with no monitor held;
/// 4. under the monitor: the reservation NODE, still linked and still holding
///    the marker, takes the value (or is unlinked for a null / thrown
///    result), and same-key waiters are woken.
///
/// Anything the snapshot cannot settle -- a bin longer than
/// [`CHM_CIA_SNAPSHOT_CAP`], another thread's reservation for the key, a
/// segment that changed identity -- takes the old body
/// ([`chm_compute_if_absent_absent_path`]). A reservation node a racing `put`
/// wrote over, or a racing `remove` unlinked, leaves the raced-in state, as
/// the old commit did.
#[allow(clippy::too_many_arguments)]
pub(crate) fn chm_compute_if_absent_single_walk(
    ctx: &mut dyn NativeContext,
    roots_base: usize,
    this: ObjectRef,
    seg: ObjectRef,
    hash: i32,
    key_pin: usize,
    key: Value,
    func_pin: usize,
    func: Value,
) -> MethodCallResult {
    let null = Value::Object(None);
    // The snapshot waits for the stripe lock GC-safely, so the segment is
    // rooted across it.
    let seg_pin = ctx.pin_native_root(seg);
    let snapshot = chm_seg_bin_snapshot(ctx, seg, hash, CHM_CIA_SNAPSHOT_CAP);
    let seg = ctx.read_native_pin(seg_pin, seg);
    let Some(snapshot) = snapshot else {
        let old = chm_compute_if_absent_probe_then_reserve(
            ctx, roots_base, this, seg, hash, key_pin, key, func_pin, func,
        );
        ctx.unpin_native_roots(seg_pin);
        return old;
    };
    // Nothing has run since the snapshot's nodes were read: pin them.
    let seen: Vec<(usize, ObjectRef)> = snapshot
        .iter()
        .map(|n| (ctx.pin_native_root(*n), *n))
        .collect();
    // Every pin above `seg_pin` is released with it, on every exit.
    let result = (|| -> MethodCallResult {
        // ---- 1. lock-free: the first-node test, then the walk. ----
        let mut found: Option<(usize, ObjectRef)> = None;
        if let Some(&(first_pin, first)) = seen.first() {
            let first = ctx.read_native_pin(first_pin, first);
            if get_node_hash(&*ctx, first) == hash {
                if let (Value::Object(Some(fk)), Value::Object(Some(k))) = (
                    get_node_key(&*ctx, first),
                    read_pinned_elem(ctx, key_pin, key),
                ) {
                    if map_keys_equal(ctx, k, fk)? {
                        let first = ctx.read_native_pin(first_pin, first);
                        let seg_now = ctx.read_native_pin(seg_pin, seg);
                        let fv = get_node_value(&*ctx, first);
                        if !matches!(fv, Value::Object(None))
                            && !chm_node_holds_marker(&*ctx, first, seg_now)
                        {
                            return Ok(Some(fv));
                        }
                    }
                }
            }
        }
        for &(pin, node) in &seen {
            let node = ctx.read_native_pin(pin, node);
            if get_node_hash(&*ctx, node) != hash {
                continue;
            }
            let (Value::Object(Some(nk)), Value::Object(Some(k))) = (
                get_node_key(&*ctx, node),
                read_pinned_elem(ctx, key_pin, key),
            ) else {
                continue;
            };
            if map_keys_equal(ctx, k, nk)? {
                found = Some((pin, node));
                break;
            }
        }
        if let Some((pin, node)) = found {
            let node = ctx.read_native_pin(pin, node);
            let seg_now = ctx.read_native_pin(seg_pin, seg);
            let v = get_node_value(&*ctx, node);
            if !matches!(v, Value::Object(None)) && !chm_node_holds_marker(&*ctx, node, seg_now) {
                return Ok(Some(v));
            }
            // Another thread's mapper is in flight for this key: the old body
            // waits for it.
            return chm_compute_if_absent_absent_path(
                ctx, roots_base, this, hash, key_pin, key, func_pin, func,
            );
        }

        // ---- 2. reserve under the monitor, comparing only new nodes. ----
        let this_now = ctx.read_native_pin(roots_base, this);
        let seg_now = ctx.read_native_pin(seg_pin, seg);
        match chm_segment_for(&*ctx, this_now, hash) {
            Some(s) if std::ptr::eq(s.as_ptr(), seg_now.as_ptr()) => {}
            _ => {
                return chm_compute_if_absent_absent_path(
                    ctx, roots_base, this, hash, key_pin, key, func_pin, func,
                )
            }
        }
        let reserved: Option<Option<(usize, ObjectRef)>> = {
            let _resize_flag = ChmResizeLockGuard::enter();
            let (guard, segc) = ChmMonitorGuard::acquire_gc_safe(ctx, seg_now);
            let (buckets, size, cap) = map_state(&*ctx, segc);
            let mut cur = match buckets {
                Some(b) => ctx.get_array_element(b, map_bucket_index(hash, cap)),
                None => null,
            };
            let mut raced = false;
            let mut steps = 0usize;
            while let Value::Object(Some(n)) = cur {
                steps += 1;
                if steps > chain_walk_limit(size) {
                    raced = true;
                    break;
                }
                let is_seen = seen
                    .iter()
                    .any(|&(p, s)| std::ptr::eq(ctx.read_native_pin(p, s).as_ptr(), n.as_ptr()));
                if !is_seen && get_node_hash(&*ctx, n) == hash {
                    // Linked since the snapshot: `equals` decides, as the
                    // JDK's walk would. A match of any kind (a value, or a
                    // reservation) is settled by the old body.
                    if let (Value::Object(Some(nk)), Value::Object(Some(k))) =
                        (get_node_key(&*ctx, n), read_pinned_elem(ctx, key_pin, key))
                    {
                        let n_pin = ctx.pin_native_root(n);
                        let eq = map_keys_equal(ctx, k, nk);
                        let n_now = ctx.read_native_pin(n_pin, n);
                        ctx.unpin_native_roots(n_pin);
                        match eq {
                            Ok(true) => {
                                raced = true;
                                break;
                            }
                            Ok(false) => {}
                            Err(e) => {
                                drop(guard);
                                return Err(e);
                            }
                        }
                        cur = ctx.get_field(n_now, NODE_FIELD_NEXT);
                        continue;
                    }
                }
                cur = ctx.get_field(n, NODE_FIELD_NEXT);
            }
            if raced || buckets.is_none() {
                drop(guard);
                None
            } else {
                // `pred.next = new Node(..)`, holding the marker: no key
                // comparison (`PutMode::assume_absent`), no second hash.
                let segc = ctx.read_native_pin(seg_pin, segc);
                let key_now = read_pinned_elem(ctx, key_pin, key);
                let marker = Value::Object(Some(segc));
                let mut seg_mut = segc;
                let put_base = ctx.pin_native_root(seg_mut);
                let k_pin = pin_value(ctx, key_now);
                let m_pin = pin_value(ctx, marker);
                let put = native_map_put_evict_pinned_inner(
                    ctx,
                    &mut seg_mut,
                    key_now,
                    marker,
                    true,
                    put_base,
                    k_pin,
                    m_pin,
                    GrowAlloc::SingleAttempt,
                    Some(hash),
                    false,
                    PutMode {
                        no_grow: false,
                        assume_absent: true,
                    },
                );
                ctx.unpin_native_roots(put_base);
                if let Err(e) = put {
                    drop(guard);
                    return Err(e);
                }
                // The node just linked: the one holding this key OBJECT and
                // the marker (identity reads; a resize may have moved it).
                let segc = ctx.read_native_pin(seg_pin, segc);
                let key_now = read_pinned_elem(ctx, key_pin, key);
                let (buckets, size, cap) = map_state(&*ctx, segc);
                let mut mine: Option<ObjectRef> = None;
                if let (Some(b), Value::Object(Some(k))) = (buckets, key_now) {
                    let mut cur = ctx.get_array_element(b, map_bucket_index(hash, cap));
                    for _ in 0..chain_walk_limit(size) {
                        let Value::Object(Some(n)) = cur else {
                            break;
                        };
                        if matches!(get_node_key(&*ctx, n), Value::Object(Some(nk)) if std::ptr::eq(nk.as_ptr(), k.as_ptr()))
                            && chm_node_holds_marker(&*ctx, n, segc)
                        {
                            mine = Some(n);
                            break;
                        }
                        cur = ctx.get_field(n, NODE_FIELD_NEXT);
                    }
                }
                // Rooted before the monitor is released (no allocation since
                // the walk that found it), by a GLOBAL root: the guard's drop
                // releases every native pin taken above its own. Removed
                // after the commit. `Some(None)`: reserved, but the node was
                // not found again; the commit then goes by key, as the old
                // one did.
                let mine = mine.map(|n| (ctx.add_global_root(n), n));
                drop(guard);
                Some(mine)
            }
        };
        let Some(node_ref) = reserved else {
            return chm_compute_if_absent_absent_path(
                ctx, roots_base, this, hash, key_pin, key, func_pin, func,
            );
        };

        // ---- 3. the mapper, with no monitor held. ----
        let func_now = read_pinned_elem(ctx, func_pin, func);
        let key_now = read_pinned_elem(ctx, key_pin, key);
        let mapped: Result<Option<Value>, MethodCallFailed> = match func_now {
            Value::Object(Some(f)) => ctx.invoke_virtual(
                f,
                "apply",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[key_now],
            ),
            _ => Ok(Some(null)),
        };
        let mapped_val = match &mapped {
            Ok(Some(v)) => *v,
            _ => null,
        };
        let mapped_pin = pin_value(ctx, mapped_val);

        // ---- 4. commit the reservation node under the monitor. ----
        let this_now = ctx.read_native_pin(roots_base, this);
        let commit: Result<Value, MethodCallFailed> = match chm_segment_for(&*ctx, this_now, hash) {
            None => Ok(null),
            Some(seg3) => {
                let _resize_flag = ChmResizeLockGuard::enter();
                let (guard, segc) = ChmMonitorGuard::acquire_gc_safe(ctx, seg3);
                // The segment the monitor is held on (not necessarily the
                // one reserved in, `seg_pin`), rooted for the key lookups
                // below, which run Java.
                let seg3_pin = ctx.pin_native_root(segc);
                let node = node_ref.map(|(root, n)| ctx.resolve_global_root(root).unwrap_or(n));
                let mv = read_pinned_elem(ctx, mapped_pin, mapped_val);
                let result = match node {
                    Some(n) if chm_seg_bin_links(&*ctx, segc, hash, n) => {
                        if chm_node_holds_marker(&*ctx, n, segc) {
                            if mapped.is_ok() && !matches!(mv, Value::Object(None)) {
                                // `pred.next = new Node<K,V>(h, key, val)`.
                                set_node_value(ctx, n, mv);
                                Ok(read_pinned_elem(ctx, mapped_pin, mv))
                            } else {
                                map_unlink_node(ctx, segc, n);
                                Ok(null)
                            }
                        } else {
                            // A racing `put` wrote over the reservation: its
                            // value stands.
                            Ok(get_node_value(&*ctx, n))
                        }
                    }
                    _ => {
                        // Unlinked by a racing mutator (or not found again):
                        // surface what the map holds for the key now, as the
                        // old commit did; a reservation of ours still in
                        // place is committed by key.
                        let key_now = read_pinned_elem(ctx, key_pin, key);
                        let cur = chm_seg_get_raw(ctx, segc, key_now, hash);
                        let segc = ctx.read_native_pin(seg3_pin, segc);
                        match cur {
                            Err(e) => Err(e),
                            Ok(Some(Value::Object(Some(v))))
                                if std::ptr::eq(v.as_ptr(), segc.as_ptr()) =>
                            {
                                let key_now = read_pinned_elem(ctx, key_pin, key);
                                let mv = read_pinned_elem(ctx, mapped_pin, mapped_val);
                                if mapped.is_ok() && !matches!(mv, Value::Object(None)) {
                                    match chm_seg_put_hashed(ctx, segc, key_now, mv, hash, false) {
                                        Ok(_) => Ok(read_pinned_elem(ctx, mapped_pin, mv)),
                                        Err(e) => Err(e),
                                    }
                                } else {
                                    match chm_seg_remove_hashed(ctx, segc, key_now, hash) {
                                        Ok(_) => Ok(null),
                                        Err(e) => Err(e),
                                    }
                                }
                            }
                            Ok(Some(v)) => Ok(v),
                            Ok(None) => Ok(null),
                        }
                    }
                };
                let segc = ctx.read_native_pin(seg3_pin, segc);
                let _ = ctx.monitor_notify_all(segc);
                ctx.unpin_native_roots(seg3_pin);
                drop(guard);
                result
            }
        };
        if let Some((root, _)) = node_ref {
            ctx.remove_global_root(root);
        }
        match mapped {
            Err(e) => Err(e),
            Ok(_) => commit.map(Some),
        }
    })();
    ctx.unpin_native_roots(seg_pin);
    result
}

thread_local! {
    /// The `(pin handle of the map, key hash)` of every `ConcurrentHashMap`
    /// mutating callback THIS THREAD is currently inside.
    ///
    /// The handle, not the `ObjectRef`: a moving collection can run inside the
    /// callback, so a raw pointer stored here would be stale by the time the
    /// re-entrant call reads it. A pin handle is stable for as long as the
    /// frame that took it is alive, and the outer frame is by construction
    /// alive whenever an inner one is looking — so
    /// `read_native_pin(handle, ..)` always answers the map's CURRENT location.
    pub(crate) static CHM_CALLBACK_FRAMES: std::cell::RefCell<Vec<(usize, i32)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Pushes one entry onto [`CHM_CALLBACK_FRAMES`] and pops it on every exit
/// path, `?` and panic included.
pub(crate) struct ChmCallbackFrame;

impl ChmCallbackFrame {
    fn enter(map_pin: usize, hash: i32) -> Self {
        CHM_CALLBACK_FRAMES.with(|f| f.borrow_mut().push((map_pin, hash)));
        ChmCallbackFrame
    }
}

impl Drop for ChmCallbackFrame {
    fn drop(&mut self) {
        CHM_CALLBACK_FRAMES.with(|f| {
            f.borrow_mut().pop();
        });
    }
}

/// The JDK's `IllegalStateException("Recursive update")`, or `Ok(())`.
///
/// # What the JDK does, and what this VM did instead
///
/// `computeIfAbsent`/`compute`/`merge` lock ONE BIN and run the user function
/// under it. A function that calls back into the same map on the same bin
/// re-enters that lock — reentrant, so it does not deadlock — finds the
/// reservation it placed itself, and throws:
///
/// ```text
///   throw new IllegalStateException("Recursive update");
/// ```
///
/// CratonVM's `computeIfAbsent` runs its mapper with NO monitor held and
/// marks the key with a reservation instead (`chm_compute_if_absent_absent_path`).
/// Phase 1 of the re-entrant call finds that reservation, cannot tell that the
/// thread waiting for it to clear is ITSELF, and waits for it in a 50 ms loop
/// that nothing can ever end. MEASURED, both modes, no flags:
///
/// ```text
///   chm.computeIfAbsent("q", k -> chm.computeIfAbsent("q", k2 -> 1))
///     HotSpot   IllegalStateException: Recursive update
///     CratonVM  never returns
/// ```
///
/// `compute` is the other polarity of one bug: it holds a REENTRANT segment
/// monitor, so the inner call simply succeeded and the map came back holding a
/// mapping the JDK refuses to make.
///
/// # Why the hash and not the key
///
/// The JDK's rule is per-BIN, i.e. `hash & (n - 1)`, which is COARSER than
/// per-key: two unequal keys that collide in one bin also throw. Keying the
/// guard on the full hash is therefore strictly narrower than the JDK — it
/// never throws where the JDK would not — and it needs no `equals` call, which
/// on this path would be more user code re-entering the same map.
pub(crate) fn chm_reject_recursive_update(
    ctx: &dyn NativeContext,
    map: ObjectRef,
    hash: i32,
) -> Result<(), MethodCallFailed> {
    let frames: Vec<(usize, i32)> = CHM_CALLBACK_FRAMES.with(|f| f.borrow().clone());
    let recursive = frames.iter().any(|(pin, h)| {
        *h == hash && std::ptr::eq(ctx.read_native_pin(*pin, map).as_ptr(), map.as_ptr())
    });
    if recursive {
        return Err(RuntimeError::IllegalStateException {
            message: "Recursive update".to_string(),
        }
        .into());
    }
    Ok(())
}

pub(crate) fn native_chm_compute_if_absent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let func = args.get(2).copied().unwrap_or(Value::Object(None));
    // C24 (HIGH): JDK rejects null key and null mappingFunction. MEASURED
    // message-less on the oracle — see `chm_bare_npe_on_null`.
    if matches!(key, Value::Object(None)) || matches!(func, Value::Object(None)) {
        return Err(bare_npe());
    }
    let roots_base = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let func_pin = pin_value(ctx, func);
    let key = read_pinned_elem(ctx, key_pin, key);
    let hash = chm_key_hash(ctx, &key)?;
    let this = ctx.read_native_pin(roots_base, this);
    // See `chm_reject_recursive_update`: without this the re-entrant call waits
    // forever for a reservation only it can clear.
    if let Err(e) = chm_reject_recursive_update(&*ctx, this, hash) {
        ctx.unpin_native_roots(roots_base);
        return Err(e);
    }
    let _frame = ChmCallbackFrame::enter(roots_base, hash);
    let result = match chm_segment_for_mut(ctx, this, hash) {
        Some(seg) => {
            // JDK-exact lock-free fast path: `computeIfAbsent` on a PRESENT
            // key returns the existing value WITHOUT locking. Real CHM locks
            // a single bin, so a mapping callback that takes unrelated locks
            // (WildFly's registry callbacks take the management-registry
            // write lock) never orders against other keys' operations; our
            // segment monitor is far coarser, and locking it just to discover
            // the key already exists created a segment-monitor ↔ registry-
            // RWLock cycle that wedged `parallel-extension-add` boots (the
            // mode-2 wedge: gdb showed writers starving at
            // stamped_lock::rw_write_lock while a read-lock holder waited in
            // acquire_gc_safe on the same segment).
            if chm_cia_single_walk_enabled() {
                chm_compute_if_absent_single_walk(
                    ctx, roots_base, this, seg, hash, key_pin, key, func_pin, func,
                )
            } else {
                chm_compute_if_absent_probe_then_reserve(
                    ctx, roots_base, this, seg, hash, key_pin, key, func_pin, func,
                )
            }
        }
        None => Ok(Some(Value::Object(None))),
    };
    if result.is_ok() {
        let this_now = ctx.read_native_pin(roots_base, this);
        chm_note_table_high_water(ctx, this_now, false);
    }
    ctx.unpin_native_roots(roots_base);
    result
}

pub(crate) fn native_chm_compute(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let func = args.get(2).copied().unwrap_or(Value::Object(None));
    // C24 (HIGH): JDK rejects null key and null remappingFunction. MEASURED
    // message-less on the oracle — see `chm_bare_npe_on_null`.
    if matches!(key, Value::Object(None)) || matches!(func, Value::Object(None)) {
        return Err(bare_npe());
    }
    let roots_base = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let func_pin = pin_value(ctx, func);
    let key = read_pinned_elem(ctx, key_pin, key);
    let hash = chm_key_hash(ctx, &key)?;
    let this = ctx.read_native_pin(roots_base, this);
    // `compute`'s segment monitor is REENTRANT, so a re-entrant call did not
    // hang here — it succeeded, and left the map holding a mapping the JDK
    // refuses to make. See `chm_reject_recursive_update`.
    if let Err(e) = chm_reject_recursive_update(&*ctx, this, hash) {
        ctx.unpin_native_roots(roots_base);
        return Err(e);
    }
    let _frame = ChmCallbackFrame::enter(roots_base, hash);
    let result = match chm_segment_for_mut(ctx, this, hash) {
        Some(seg) => {
            let _resize_flag = ChmResizeLockGuard::enter();
            // GC-pausable contended wait — re-read the pinned locals AFTER
            // acquiring (see `acquire_gc_safe`'s doc).
            let (_guard, seg) = ChmMonitorGuard::acquire_gc_safe(ctx, seg);
            let key = read_pinned_elem(ctx, key_pin, key);
            let func = read_pinned_elem(ctx, func_pin, func);
            // `CRATONVM_COMPAT_CHM_REMAP_SINGLE_HASH`: no second hash.
            if chm_remap_single_hash_enabled() {
                let null = Value::Object(None);
                chm_seg_remap_hashed(ctx, seg, key, null, func, hash, RemapOp::Compute)
            } else {
                native_map_compute(ctx, &[Value::Object(Some(seg)), key, func])
            }
        }
        None => Ok(Some(Value::Object(None))),
    };
    if result.is_ok() {
        let this_now = ctx.read_native_pin(roots_base, this);
        chm_note_table_high_water(ctx, this_now, false);
    }
    ctx.unpin_native_roots(roots_base);
    result
}

pub(crate) fn native_chm_compute_if_present(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let func = args.get(2).copied().unwrap_or(Value::Object(None));
    // JDK ConcurrentHashMap rejects null key and null remappingFunction.
    // MEASURED message-less on the oracle — see `chm_bare_npe_on_null`.
    if matches!(key, Value::Object(None)) || matches!(func, Value::Object(None)) {
        return Err(bare_npe());
    }
    let roots_base = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let func_pin = pin_value(ctx, func);
    let key = read_pinned_elem(ctx, key_pin, key);
    let hash = chm_key_hash(ctx, &key)?;
    let this = ctx.read_native_pin(roots_base, this);
    let result = match chm_segment_for(ctx, this, hash) {
        Some(seg) => {
            // JDK-exact lock-free fast path: `computeIfPresent` on an ABSENT
            // key returns null WITHOUT locking (real CHM's lock-free tabAt
            // probe). See `native_chm_compute_if_absent` for why avoiding the
            // segment monitor here matters (callback-under-coarse-lock
            // ordering cycles).
            let key_now = read_pinned_elem(ctx, key_pin, key);
            // `CRATONVM_COMPAT_CHM_REMAP_SINGLE_WALK`: the lock-free question
            // runs no `equals` (the JDK's is "is the bin empty"); the one
            // walk that does is `chm_seg_remap_single_walk`'s, locked.
            let maybe_present =
                if chm_remap_single_walk_enabled() && chm_remap_single_hash_enabled() {
                    chm_seg_bin_holds_hash(ctx, seg, hash)
                } else {
                    chm_seg_get(ctx, seg, key_now, hash)?.is_some()
                };
            match maybe_present {
                false => Ok(Some(Value::Object(None))),
                true => {
                    let this = ctx.read_native_pin(roots_base, this);
                    match chm_segment_for(ctx, this, hash) {
                        Some(seg) => {
                            let _resize_flag = ChmResizeLockGuard::enter();
                            // GC-pausable contended wait — re-read the pinned
                            // locals AFTER acquiring (see `acquire_gc_safe`).
                            let (_guard, seg) = ChmMonitorGuard::acquire_gc_safe(ctx, seg);
                            let key = read_pinned_elem(ctx, key_pin, key);
                            let func = read_pinned_elem(ctx, func_pin, func);
                            // `CRATONVM_COMPAT_CHM_REMAP_SINGLE_HASH`.
                            if chm_remap_single_hash_enabled() {
                                let null = Value::Object(None);
                                chm_seg_remap_hashed(
                                    ctx,
                                    seg,
                                    key,
                                    null,
                                    func,
                                    hash,
                                    RemapOp::IfPresent,
                                )
                            } else {
                                native_map_compute_if_present(
                                    ctx,
                                    &[Value::Object(Some(seg)), key, func],
                                )
                            }
                        }
                        None => Ok(Some(Value::Object(None))),
                    }
                }
            }
        }
        None => Ok(Some(Value::Object(None))),
    };
    ctx.unpin_native_roots(roots_base);
    result
}

pub(crate) fn native_chm_merge(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    let func = args.get(3).copied().unwrap_or(Value::Object(None));
    // C24 (HIGH): JDK rejects null key, null value, and null remappingFunction.
    // MEASURED message-less on the oracle — see `chm_bare_npe_on_null`.
    if matches!(key, Value::Object(None))
        || matches!(value, Value::Object(None))
        || matches!(func, Value::Object(None))
    {
        return Err(bare_npe());
    }
    let roots_base = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let value_pin = pin_value(ctx, value);
    let func_pin = pin_value(ctx, func);
    let key = read_pinned_elem(ctx, key_pin, key);
    let hash = chm_key_hash(ctx, &key)?;
    let this = ctx.read_native_pin(roots_base, this);
    let result = match chm_segment_for_mut(ctx, this, hash) {
        Some(seg) => {
            let _resize_flag = ChmResizeLockGuard::enter();
            // GC-pausable contended wait — re-read the pinned locals AFTER
            // acquiring (see `acquire_gc_safe`'s doc).
            let (_guard, seg) = ChmMonitorGuard::acquire_gc_safe(ctx, seg);
            let key = read_pinned_elem(ctx, key_pin, key);
            let value = read_pinned_elem(ctx, value_pin, value);
            let func = read_pinned_elem(ctx, func_pin, func);
            // `CRATONVM_COMPAT_CHM_REMAP_SINGLE_HASH`.
            if chm_remap_single_hash_enabled() {
                chm_seg_remap_hashed(ctx, seg, key, value, func, hash, RemapOp::Merge)
            } else {
                native_map_merge(ctx, &[Value::Object(Some(seg)), key, value, func])
            }
        }
        None => Ok(Some(Value::Object(None))),
    };
    if result.is_ok() {
        let this_now = ctx.read_native_pin(roots_base, this);
        chm_note_table_high_water(ctx, this_now, false);
    }
    ctx.unpin_native_roots(roots_base);
    result
}

// --- Bulk operations (iterate all segments) ---

pub(crate) fn native_chm_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let mut total = 0i32;
    for seg in chm_all_segments(ctx, this) {
        if let Value::Int(s) = ctx.get_field(seg, MAP_FIELD_SIZE) {
            total += s;
        }
    }
    Ok(Some(Value::Int(total)))
}

pub(crate) fn native_chm_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let size = native_chm_size(ctx, args)?.unwrap_or(Value::Int(0));
    match size {
        Value::Int(0) => Ok(Some(Value::Int(1))),
        _ => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_chm_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let _resize_flag = ChmResizeLockGuard::enter();
    // GC-safety: each per-segment lock wait is GC-pausable, so the segments
    // collected up front must survive (and be re-read) across iterations.
    // `this_pin` doubles as the unconditional unpin base.
    let this_pin = ctx.pin_native_root(this);
    let segs: Vec<Value> = chm_all_segments(ctx, this)
        .into_iter()
        .map(|s| Value::Object(Some(s)))
        .collect();
    let (_, seg_pins) = pin_value_slice(ctx, &segs);
    let result = (|| -> MethodCallResult {
        for i in 0..segs.len() {
            let seg = match read_pinned_elem(ctx, seg_pins[i], segs[i]) {
                Value::Object(Some(s)) => s,
                _ => continue,
            };
            let (_guard, seg) = ChmMonitorGuard::acquire_gc_safe(ctx, seg);
            native_map_clear(ctx, &[Value::Object(Some(seg))])?;
        }
        Ok(None)
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_chm_put_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // `putAll(null)` dereferences its argument, so it is an NPE and not a
    // silent no-op. A missing argument is a dispatch defect rather than a
    // Java-visible null and keeps the old return — the distinction
    // `reject_null_functional` draws.
    //
    // G1-1 RECONCILED (retirement pass): the message is NOT a constant.
    // `ConcurrentHashMap.putAll`'s bytecode is `if (table != null) { ...
    // m.size() ... } m.entrySet()...` — MEASURED on HotSpot 25 both ways:
    // `new ConcurrentHashMap<>().putAll(null)` (never populated, `table`
    // still null) throws `entrySet()`/`m`; `m.put("k","v"); m.putAll(null)`
    // (table already allocated by the prior put) throws `size()`/`m`. G1-1's
    // own probe used the populated shape and recorded `size()/m`; an earlier
    // pass here measured only the empty shape and hardcoded `entrySet()/m`
    // for every receiver, which is right for a never-touched map and wrong
    // for the far more common already-populated one. `native_chm_size` > 0
    // approximates "table allocated" — it is not identical (a map emptied by
    // `remove` keeps its table but reads 0 here), but it is the only signal
    // this backend tracks and it matches both MEASURED states exactly.
    if matches!(args.get(1), Some(Value::Object(None))) {
        let table_allocated =
            matches!(native_chm_size(ctx, args), Ok(Some(Value::Int(n))) if n > 0);
        let message = if table_allocated {
            "Cannot invoke \"java.util.Map.size()\" because \"m\" is null"
        } else {
            "Cannot invoke \"java.util.Map.entrySet()\" because \"m\" is null"
        };
        return Err(RuntimeError::NullPointerException {
            message: Some(message.to_string()),
        }
        .into());
    }
    let source = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let _resize_flag = ChmResizeLockGuard::enter();
    // `collect_entries_any` (not the HashMap-only `map_collect_entries`) so a
    // TreeMap / LinkedHashMap / real-JDK unmodifiable-map source is copied in
    // full rather than read as an empty bucket table.
    //
    // GC-safety: the collected entries are raw `Value`s reused across
    // GC-triggering hashing/puts and GC-pausable lock waits — pin the whole
    // flattened slice (plus `this`, pinned BEFORE the GC-triggering
    // collection) and re-read each element right before use.
    let this_pin = ctx.pin_native_root(this);
    // C3W3-2 (`CRATONVM_COMPAT_MAP_COPY_JDK_QUESTIONS`, round 15 wave 4, lane
    // compat4): a program's map is asked `m.size()` only once the table exists
    // (`if (table != null) tryPresize(size() + m.size())`, approximated by a
    // non-zero count as the NPE message above is), then walked, one `putVal`
    // per entry as it arrives.
    if let Some(prog) = foreign_map_source(&*ctx, source) {
        let prog_pin = ctx.pin_native_root(prog);
        let walked = (|| -> Result<(), MethodCallFailed> {
            let this_now = ctx.read_native_pin(this_pin, this);
            if chm_entry_count(&*ctx, this_now) > 0 {
                let prog_now = ctx.read_native_pin(prog_pin, prog);
                let answer = ctx.invoke_virtual(prog_now, "size", "()I", &[])?;
                let this_now = ctx.read_native_pin(this_pin, this);
                if let Some(Value::Int(n)) = answer {
                    chm_note_presize(ctx, this_now, n.max(0) as usize);
                }
            }
            map_for_each_entry(
                ctx,
                (prog_pin, prog),
                MapSizeAsk::Skip,
                |ctx, key, value| chm_put_all_entry(ctx, (this_pin, this), key, value),
            )
        })();
        let this_now = ctx.read_native_pin(this_pin, this);
        chm_note_table_high_water(ctx, this_now, true);
        ctx.unpin_native_roots(this_pin);
        return walked.map(|()| None);
    }
    let collected = collect_entries_any(ctx, source);
    let entries = unpin_on_err(ctx, this_pin, collected)?;
    // `tryPresize(m.size())` before the first `putVal`; no Java runs in it.
    let this_now = ctx.read_native_pin(this_pin, this);
    chm_note_presize(ctx, this_now, entries.len());
    let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (_, flat_pins) = pin_value_slice(ctx, &flat);
    let result = (|| -> MethodCallResult {
        for i in 0..entries.len() {
            let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
            let value = read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]);
            // `putAll` reaches `putVal` once per entry, so a null key or value
            // ANYWHERE in the source is the same message-less NPE a direct
            // `put` would raise — the source map's own null tolerance is not
            // inherited. `native_map_put` below is the HashMap body and accepts
            // both, which is why the check cannot be left to the callee.
            chm_bare_npe_on_null(&key, &value)?;
            let hash = chm_key_hash(ctx, &key)?;
            let this = ctx.read_native_pin(this_pin, this);
            if let Some(seg) = chm_segment_for_mut(ctx, this, hash) {
                let (_guard, seg) = ChmMonitorGuard::acquire_gc_safe(ctx, seg);
                let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
                let value = read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]);
                // One `hashCode()` per entry, as `putVal` (`CRATONVM_COMPAT_CHM_SINGLE_HASH`).
                chm_seg_put_hashed(ctx, seg, key, value, hash, false)?;
            }
        }
        Ok(None)
    })();
    let this_now = ctx.read_native_pin(this_pin, this);
    chm_note_table_high_water(ctx, this_now, true);
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_chm_replace_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // RULE F. Without this the null function reached `native_map_replace_all`
    // once per segment, which returned its own silent no-op — so an empty CHM
    // and a populated one both answered "done". MEASURED bare NPE for both.
    // See `reject_null_functional`.
    reject_null_functional(args.get(1))?;
    let func = args.get(1).copied().unwrap_or(Value::Object(None));
    // NOT `native_map_replace_all` per segment any more. That body is the
    // `HashMap` one, where a function returning null is a legal way to map a
    // key to null; on a `ConcurrentHashMap` it is a
    // `NullPointerException` — the JDK's `replaceAll` re-checks its own
    // function's OUTPUT (`if (newValue == null) throw new
    // NullPointerException();`) precisely because the map cannot hold it.
    // MEASURED: `chm.replaceAll((k, v) -> null)` answered no-throw and left
    // the map with its old values, so the caller's mistake was invisible.
    //
    // The entry-collect-then-apply shape is `native_chm_put_all`'s, for its
    // GC discipline: the mapper is arbitrary Java and can move everything.
    // Applying through `native_chm_put` also gets the write-side null refusal
    // for free, but the explicit check above it is what names the CONTRACT
    // rather than relying on a callee to notice.
    let this_pin = ctx.pin_native_root(this);
    let func_pin = pin_value(ctx, func);
    let entries = chm_collect_all_entries(ctx, this);
    let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (_, flat_pins) = pin_value_slice(ctx, &flat);
    let result = (|| -> MethodCallResult {
        for i in 0..entries.len() {
            let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
            let value = read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]);
            let f = match read_pinned_elem(ctx, func_pin, func) {
                Value::Object(Some(f)) => f,
                _ => return Ok(None),
            };
            let mapped = ctx.invoke_virtual(
                f,
                "apply",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[key, value],
            )?;
            let mapped = match mapped {
                Some(v) => v,
                None => Value::Object(None),
            };
            if matches!(mapped, Value::Object(None)) {
                return Err(bare_npe());
            }
            let this = ctx.read_native_pin(this_pin, this);
            let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
            native_chm_put(ctx, &[Value::Object(Some(this)), key, mapped])?;
        }
        Ok(None)
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_chm_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // RULE F. G1-1 gave the CHM write path `chm_bare_npe_on_null` but did not
    // reach `forEach`/`replaceAll`, which take no key or value to test.
    // MEASURED bare NPE, empty receiver included. See `reject_null_functional`.
    reject_null_functional(args.get(1))?;
    let action = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(None),
    };
    let entries = chm_collect_all_entries(ctx, this);
    let action_pin = ctx.pin_native_root(action);
    let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (_, flat_pins) = pin_value_slice(ctx, &flat);
    for i in 0..entries.len() {
        let action = ctx.read_native_pin(action_pin, action);
        let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
        let value = read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]);
        if let Err(e) = ctx.invoke_virtual(
            action,
            "accept",
            "(Ljava/lang/Object;Ljava/lang/Object;)V",
            &[key, value],
        ) {
            ctx.unpin_native_roots(action_pin);
            return Err(e);
        }
    }
    ctx.unpin_native_roots(action_pin);
    Ok(None)
}

/// `ConcurrentHashMap.keySet()` — the JDK's `new KeySetView<>(this, null)`.
///
/// This used to build a `java/util/HashSet` carrying a view-backing that
/// remembered the source map, so reads resynced and `remove` wrote through.
/// That got the common paths right and three things wrong, all of which the
/// real `KeySetView` gets right for free now that it has its own natives:
///
/// * `(ConcurrentHashMap.KeySetView<K,?>) map.keySet()` threw
///   `ClassCastException` — the object was a `HashSet`.
/// * `add`/`addAll` SUCCEEDED, mutating the view's private snapshot and
///   silently losing the element at the next resync. A `keySet()` view has no
///   mapped value, so the JDK throws `UnsupportedOperationException`.
/// * `retainAll` did not write through at all (the map kept every element the
///   caller asked to drop), and `removeAll` returned `false` after removing.
///
/// The iteration order is unchanged: the view-backing read
/// `collect_keys_any` → `map_collect_keys` → `chm_collect_all_keys`, which is
/// exactly what the `KeySetView` natives read, so the HotSpot-faithful bucket
/// order `chm_reorder_by_virtual_bucket` reconstructs (and which Spring's
/// `SimpleAliasRegistry.getAliases` depends on) is preserved.
pub(crate) fn native_chm_key_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Refresh the real `table` mirror: this native already walks every entry,
    // so the rebuild is a constant factor and not a change of order. See
    // `chm_refresh_real_table`.
    chm_refresh_real_table(ctx, &mut this);
    // A null mapped value is what makes this view read-only, per the JDK.
    let view = make_key_set_view(ctx, this, Value::Object(None))?;
    Ok(Some(Value::Object(Some(view))))
}

pub(crate) fn native_chm_values(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Refresh the real `table` mirror: this native already walks every entry,
    // so the rebuild is a constant factor and not a change of order. See
    // `chm_refresh_real_table`.
    chm_refresh_real_table(ctx, &mut this);
    let vals = chm_collect_all_values(ctx, this);
    // Use the layout-aware ArrayList helpers: in real-JDK mode `elementData`
    // and `size` are NOT at slots 0/1 (`AbstractList.modCount` occupies an
    // earlier slot), so the previous hardcoded `set_field(list, 0/1, ...)`
    // wrote the backing array into the wrong slots and `values()` iterated
    // empty even though `size()` was correct.
    //
    // Live view (2026-07-22): reserve one extra trailing slot and stash the
    // source map there, exactly like the generic `native_map_values`
    // (HashMap) does — `values_view_source`/`resync_values_view` (see
    // `native_al_size`/`native_al_get`/`native_al_contains`/
    // `native_al_iterator`/etc.) detect that marker and re-collect the
    // current values from the live map before every read. Without it, this
    // ArrayList was a dead one-time snapshot: `map.values()` captured before
    // any entries existed (or before later puts) never grew, e.g. H2's
    // `Schema.getAllSequences()` (a raw `ConcurrentHashMap.values()` captured
    // once) permanently reported 0 regardless of how many sequences were
    // created afterward (`TestAlter.testAlterTableDropIdentityColumn`).
    //
    // cceres5: the list/array allocations can move the just-collected values
    // (and `this`, the source map stashed below) — pin + re-read before the
    // stores (see make_static_entry_set / native_chm_entry_set).
    let (elem_base, val_handles) = pin_value_slice(ctx, &vals);
    let this_pin = ctx.pin_native_root(this);
    let first_pin = if elem_base == usize::MAX {
        this_pin
    } else {
        elem_base
    };
    // Not `?` (gc-common w20-a): a refused carrier releases this frame's pins.
    let list = match alloc_view_carrier(ctx, "java/util/concurrent/ConcurrentHashMap$ValuesView") {
        Ok(l) => l,
        Err(e) => {
            ctx.unpin_native_roots(first_pin);
            return Err(e);
        }
    };
    let list_pin = ctx.pin_native_root(list);
    let cap = std::cmp::max(vals.len(), AL_DEFAULT_CAPACITY) + 1;
    let arr: ObjectRef = alloc_ref_array(ctx, cap);
    for (i, v) in vals.iter().enumerate() {
        let v = read_pinned_elem(ctx, val_handles[i], *v);
        let _ = ctx.set_array_element(arr, i, v);
    }
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_array_element(arr, cap - 1, Value::Object(Some(this)));
    let list = ctx.read_native_pin(list_pin, list);
    store_view_carrier_backref(ctx, list, this);
    al_set_data(ctx, list, arr);
    al_set_size(ctx, list, vals.len() as i32);
    ctx.unpin_native_roots(first_pin);
    Ok(Some(Value::Object(Some(list))))
}

pub(crate) fn native_chm_entry_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Refresh the real `table` mirror: this native already walks every entry,
    // so the rebuild is a constant factor and not a change of order. See
    // `chm_refresh_real_table`.
    chm_refresh_real_table(ctx, &mut this);
    let entries = chm_collect_all_entries(ctx, this);
    // cceres5: pin the snapshot + receiver before the first allocation below.
    let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (elem_base, flat_pins) = pin_value_slice(ctx, &flat);
    let this_pin = ctx.pin_native_root(this);
    let first_pin = if elem_base == usize::MAX {
        this_pin
    } else {
        elem_base
    };
    // Build an entrySet carrier holding Map.Entry objects. Each entry must be a
    // real `java/util/Map$Entry` (not raw Object cid=0) so user-bytecode
    // `checkcast Map$Entry` succeeds after iterating entrySet() — see
    // Spring DefaultSingletonBeanRegistry.destroyBean iterating the
    // dependentBeanMap (a ConcurrentHashMap) entrySet.
    let carrier = set_view_carrier_for(&*ctx, this, VIEW_KIND_ENTRYSET);
    let set = alloc_set_view_carrier(ctx, carrier)?;
    let set_pin = ctx.pin_native_root(set);
    let cap = std::cmp::max(entries.len().next_power_of_two(), MAP_DEFAULT_CAPACITY);
    // Live entrySet view: back the HashSet with a view-backing that remembers the
    // source ConcurrentHashMap (+ VIEW_KIND_ENTRYSET), exactly like
    // `native_chm_key_set` (keySet) and the generic `native_map_entry_set`
    // (HashMap). This is what makes removing THROUGH the set write through to the
    // map — `entrySet().remove(e)` / `entrySet().iterator().remove()` /
    // `entrySet().removeIf(...)` (see `native_hs_remove`, which pulls the source
    // off the backing and deletes `e.getKey()` from it). The previous plain
    // snapshot backing (`alloc_backing_map`, no source marker) made
    // `view_backing_source` return `None`, so those removals silently vanished:
    // Narayana's `TransactionReaper` drains its `pendingInsertions` CHM via
    // `entrySet().remove(entry)` in `ReaperElementManager.flushPending`, so with a
    // dead snapshot the timed-out transaction was never moved into the reaper's
    // sorted queue and never rolled back (Hibernate `TransactionTimeoutTest`).
    // Reads resync from the live map via `collect_entries_any`, which routes a
    // segmented CHM through `chm_collect_all_entries`.
    let mut this_now = ctx.read_native_pin(this_pin, this);
    let backing = alloc_view_backing(ctx, &mut this_now, VIEW_KIND_ENTRYSET, cap)?;
    let backing_pin = ctx.pin_native_root(backing);
    let set_now = ctx.read_native_pin(set_pin, set);
    hs_set_backing_map(ctx, set_now, backing);
    // cceres5: `alloc_live_entry`/`map_alloc_node` below can move the raw
    // snapshot refs still pending in `entries` (and `set`/`backing`/`this`
    // themselves); pin + re-read everything (see make_static_entry_set).
    let sentinel = Value::Int(1);
    for i in 0..entries.len() {
        let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
        let value = read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]);
        let this_now = ctx.read_native_pin(this_pin, this);
        let entry_obj = alloc_live_entry(ctx, "java/util/Map$Entry", key, value, this_now)?;
        let entry_pin = ctx.pin_native_root(entry_obj);

        let hash = ctx.identity_hash_code(entry_obj);
        let backing_now = ctx.read_native_pin(backing_pin, backing);
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
        let backing_now = ctx.read_native_pin(backing_pin, backing);
        set_map_size(ctx, backing_now, size + 1);
        ctx.unpin_native_roots(entry_pin);
    }
    let set_now = ctx.read_native_pin(set_pin, set);
    ctx.unpin_native_roots(first_pin);
    Ok(Some(Value::Object(Some(set_now))))
}

pub(crate) fn native_chm_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // gen r4w4/rooting2: `obj_to_display_units` dispatches each key's and
    // value's `toString()`, so the collected entries are pinned for the whole
    // walk and each one is re-read at its use -- the `native_ksv_to_string`
    // shape. The first pin is the unpin base; with no object entries there is
    // nothing pinned and nothing to unpin.
    let entries = chm_collect_all_entries(ctx, this);
    let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (pin_base, pins) = pin_value_slice(ctx, &flat);
    let result = (|| -> MethodCallResult {
        let mut parts = Vec::with_capacity(entries.len());
        for i in 0..entries.len() {
            let key = read_pinned_elem(ctx, pins[2 * i], flat[2 * i]);
            let k = crate::obj_to_display_units(ctx, &key)?;
            let value = read_pinned_elem(ctx, pins[2 * i + 1], flat[2 * i + 1]);
            let v = crate::obj_to_display_units(ctx, &value)?;
            parts.push(crate::entry_units(&k, &v));
        }
        let s = crate::wrap_join("{", &parts, ", ", "}");
        let sref = ctx.create_string_from_units(&s);
        Ok(Some(Value::Object(Some(sref))))
    })();
    if pin_base != usize::MAX {
        ctx.unpin_native_roots(pin_base);
    }
    result
}

pub(crate) fn native_chm_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // gen r4w4/rooting2: each segment's hash runs its keys' and values'
    // `hashCode()`, so every segment after the first used to be hashed at a
    // vacated address (contributing 0). Pin the segment list across the walk,
    // as `native_chm_clear` does.
    let segs: Vec<Value> = chm_all_segments(ctx, this)
        .into_iter()
        .map(|s| Value::Object(Some(s)))
        .collect();
    let (pin_base, seg_pins) = pin_value_slice(ctx, &segs);
    let result = (|| -> MethodCallResult {
        let mut total = 0i32;
        for i in 0..segs.len() {
            let seg = read_pinned_elem(ctx, seg_pins[i], segs[i]);
            if let Some(Value::Int(h)) = native_map_hash_code(ctx, &[seg])? {
                total = total.wrapping_add(h);
            }
        }
        Ok(Some(Value::Int(total)))
    })();
    if pin_base != usize::MAX {
        ctx.unpin_native_roots(pin_base);
    }
    result
}

pub(crate) fn native_chm_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    if std::ptr::eq(this.as_ptr(), other.as_ptr()) {
        return Ok(Some(Value::Int(1)));
    }
    // `ConcurrentHashMap.equals(o)` is `false` for a non-Map argument
    // (`if (!(o instanceof Map)) return false`).
    if !object_is_map(ctx, other) {
        return Ok(Some(Value::Int(0)));
    }
    // GC-safety: every `invoke_virtual` below runs arbitrary Java (`size`,
    // `get`, and the values' `equals`) and can complete a moving young GC.
    // `other` and every key/value in `our_entries` are bare Rust locals — the
    // Vec is not a GC root at all, so its contents would be pre-move addresses
    // (and an entry reachable only from it can be reclaimed outright). Root
    // them for the whole comparison and re-read each element at its use.
    let this_pin = ctx.pin_native_root(this);
    let other_h = ctx.pin_native_root(other);
    let our_entries = chm_collect_all_entries(ctx, this);
    let flat: Vec<Value> = our_entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (_flat_base, flat_pins) = pin_value_slice(ctx, &flat);
    let result = native_chm_equals_pinned(ctx, other, other_h, &flat, &flat_pins);
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_chm_equals_pinned(
    ctx: &mut dyn NativeContext,
    other: ObjectRef,
    other_h: usize,
    flat: &[Value],
    flat_pins: &[usize],
) -> MethodCallResult {
    let other = ctx.read_native_pin(other_h, other);
    // `other` may be ANY `Map`, not just a (segmented) ConcurrentHashMap —
    // HotSpot's `CHM.equals` does `m.get(p.key)` / iterates `m.entrySet()` via
    // the `Map` interface. Reading `other` through the CHM-specific
    // `native_chm_size` / `native_chm_get` reported size 0 / no entries for a
    // plain HashMap argument, so `chm.equals(hashMap)` wrongly returned false.
    // Route `other` access through virtual dispatch (`other.size()` /
    // `other.get(k)`) so it resolves to whatever natives/bytecode `other`'s real
    // class provides — same cross-implementation fix as `native_map_equals`.
    let other_size = match ctx.invoke_virtual(other, "size", "()I", &[])? {
        Some(Value::Int(s)) => s,
        _ => {
            // Defensive: a Map whose size() didn't yield an int — fall back to
            // the CHM-native read rather than mis-comparing. `size()` above ran
            // Java, so `other` is re-read from its pin (gen r4w4/rooting2).
            let other = ctx.read_native_pin(other_h, other);
            match native_chm_size(ctx, &[Value::Object(Some(other))])? {
                Some(Value::Int(s)) => s,
                _ => 0,
            }
        }
    };
    if other_size != (flat.len() / 2) as i32 {
        return Ok(Some(Value::Int(0)));
    }
    // Compare each value with the `Map.equals` contract: `v.equals(otherVal)`.
    // `values_equal` alone only knew identity / String contents / enum identity,
    // so it wrongly reported unequal for equal-but-not-identical object values
    // (e.g. two maps holding equal `List`/bean values). Route object/object
    // pairs through `map_keys_equal` (which dispatches the value's real Java
    // `equals`); keep `values_equal` for primitive/null/mixed cases.
    //
    // NOTE: unlike `AbstractMap.equals` (the HashMap family, see
    // `native_map_equals`), `ConcurrentHashMap.equals` does NOT wrap this in
    // `catch (ClassCastException | NullPointerException) { return false; }` — it
    // dispatches `v.equals(val)` directly and lets such an exception propagate.
    // So we propagate here too (the `?`), matching HotSpot.
    for i in 0..(flat.len() / 2) {
        let other = ctx.read_native_pin(other_h, other);
        let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
        let other_val = ctx
            .invoke_virtual(
                other,
                "get",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[key],
            )?
            .unwrap_or(Value::Object(None));
        // `get` above ran arbitrary Java; refresh our value before comparing,
        // and root `other_val` across `map_keys_equal` (which dispatches the
        // value's own `equals`).
        let value = read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]);
        let other_val_pin = pin_value(ctx, other_val);
        let eq = match (value, other_val) {
            (Value::Object(Some(va)), Value::Object(Some(vb))) => map_keys_equal(ctx, va, vb)?,
            _ => values_equal(ctx, &value, &other_val),
        };
        if other_val_pin != usize::MAX {
            ctx.unpin_native_roots(other_val_pin);
        }
        if !eq {
            return Ok(Some(Value::Int(0)));
        }
    }
    Ok(Some(Value::Int(1)))
}

// --- ConcurrentHashMap-specific compound operations ---

pub(crate) fn native_chm_remove_kv(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let expected_val = args.get(2).copied().unwrap_or(Value::Object(None));
    // WRITE path, so the explicit refusal, not the helpful-NPE one:
    // `ConcurrentHashMap.remove(Object,Object)` opens with
    // `if (key == null) throw new NullPointerException();` and then answers
    // `false` for a null value rather than throwing. MEASURED:
    // `remove(null,v)` and `remove(null,null)` are `<NO-MESSAGE>`,
    // `remove(k,null)` is `OK -> false`.
    if matches!(key, Value::Object(None)) {
        return Err(bare_npe());
    }
    // G1-1 RECONCILED (retirement pass): the real body is
    // `return value != null && replaceNode(key, null, value) != null;` — the
    // null-value short circuit comes BEFORE any lookup, so an ABSENT key with
    // a null expected value must answer `false` without ever comparing
    // "current is absent" (`null`) to "expected is null" and calling that a
    // match. MEASURED: `chm.remove("zz", null)` on an empty-of-"zz" map is
    // `false`, not `true`. The old code below reached `values_equal(current,
    // expected_val)` with both sides `Value::Object(None)` in exactly that
    // case and treated it as equal.
    if matches!(expected_val, Value::Object(None)) {
        return Ok(Some(Value::Int(0)));
    }
    // GC-safety: pin across hashCode()/equals() dispatch and the
    // GC-pausable segment-lock wait — see `native_chm_put`.
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let expected_pin = pin_value(ctx, expected_val);
    let result = (|| -> MethodCallResult {
        let key = read_pinned_elem(ctx, key_pin, key);
        let hash = chm_key_hash(ctx, &key)?;
        let this = ctx.read_native_pin(this_pin, this);
        match chm_segment_for(ctx, this, hash) {
            Some(seg) => {
                let _resize_flag = ChmResizeLockGuard::enter();
                let (_guard, seg) = ChmMonitorGuard::acquire_gc_safe(ctx, seg);
                let seg_pin = ctx.pin_native_root(seg);
                let key = read_pinned_elem(ctx, key_pin, key);
                let current = native_map_get(ctx, &[Value::Object(Some(seg)), key])?
                    .unwrap_or(Value::Object(None));
                let current_pin = pin_value(ctx, current);
                let expected_val = read_pinned_elem(ctx, expected_pin, expected_val);
                let current = read_pinned_elem(ctx, current_pin, current);
                // `replaceNode`: `cv == ev || (ev != null && cv.equals(ev))`,
                // the ARGUMENT's `equals` (`CRATONVM_COMPAT_CHM_VALUE_EQUALS`).
                // It may run Java; `seg`/`key` are re-read from pins below.
                let eq = if chm_value_equals_enabled() {
                    values_equal_deep(ctx, &expected_val, &current)?
                } else {
                    values_equal(ctx, &current, &expected_val)
                };
                let r = if eq {
                    let seg = ctx.read_native_pin(seg_pin, seg);
                    let key = read_pinned_elem(ctx, key_pin, key);
                    native_map_remove(ctx, &[Value::Object(Some(seg)), key])?;
                    Ok(Some(Value::Int(1)))
                } else {
                    Ok(Some(Value::Int(0)))
                };
                ctx.unpin_native_roots(seg_pin);
                r
            }
            None => Ok(Some(Value::Int(0))),
        }
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_chm_replace(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let new_val = args.get(2).copied().unwrap_or(Value::Object(None));
    // C24 (HIGH): JDK rejects null key/value. MEASURED message-less on the
    // oracle — see `chm_bare_npe_on_null`.
    chm_bare_npe_on_null(&key, &new_val)?;
    // GC-safety: pin across hashCode() dispatch, the GC-pausable segment-lock
    // wait, and the GC-capable put (the returned `current` must be the
    // relocated address, not the pre-put one) — see `native_chm_put`.
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let new_val_pin = pin_value(ctx, new_val);
    let result = (|| -> MethodCallResult {
        let key = read_pinned_elem(ctx, key_pin, key);
        let hash = chm_key_hash(ctx, &key)?;
        let this = ctx.read_native_pin(this_pin, this);
        match chm_segment_for(ctx, this, hash) {
            Some(seg) => {
                let _resize_flag = ChmResizeLockGuard::enter();
                let (_guard, seg) = ChmMonitorGuard::acquire_gc_safe(ctx, seg);
                let seg_pin = ctx.pin_native_root(seg);
                let key = read_pinned_elem(ctx, key_pin, key);
                let current = native_map_get(ctx, &[Value::Object(Some(seg)), key])?
                    .unwrap_or(Value::Object(None));
                let current_pin = pin_value(ctx, current);
                let r = match current {
                    Value::Object(None) => Ok(Some(Value::Object(None))),
                    _ => {
                        let seg = ctx.read_native_pin(seg_pin, seg);
                        let key = read_pinned_elem(ctx, key_pin, key);
                        let new_val = read_pinned_elem(ctx, new_val_pin, new_val);
                        native_map_put(ctx, &[Value::Object(Some(seg)), key, new_val])?;
                        Ok(Some(read_pinned_elem(ctx, current_pin, current)))
                    }
                };
                ctx.unpin_native_roots(seg_pin);
                r
            }
            None => Ok(Some(Value::Object(None))),
        }
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_chm_replace_kv(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let old_val = args.get(2).copied().unwrap_or(Value::Object(None));
    let new_val = args.get(3).copied().unwrap_or(Value::Object(None));
    // C24 (HIGH): JDK rejects null key/old/new. MEASURED message-less on the
    // oracle — see `chm_bare_npe_on_null`.
    if matches!(key, Value::Object(None))
        || matches!(old_val, Value::Object(None))
        || matches!(new_val, Value::Object(None))
    {
        return Err(bare_npe());
    }
    // GC-safety: pin across hashCode()/equals() dispatch and the
    // GC-pausable segment-lock wait — see `native_chm_put`.
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let old_pin = pin_value(ctx, old_val);
    let new_pin = pin_value(ctx, new_val);
    let result = (|| -> MethodCallResult {
        let key = read_pinned_elem(ctx, key_pin, key);
        let hash = chm_key_hash(ctx, &key)?;
        let this = ctx.read_native_pin(this_pin, this);
        match chm_segment_for(ctx, this, hash) {
            Some(seg) => {
                let _resize_flag = ChmResizeLockGuard::enter();
                let (_guard, seg) = ChmMonitorGuard::acquire_gc_safe(ctx, seg);
                let seg_pin = ctx.pin_native_root(seg);
                let key = read_pinned_elem(ctx, key_pin, key);
                let current = native_map_get(ctx, &[Value::Object(Some(seg)), key])?
                    .unwrap_or(Value::Object(None));
                let current_pin = pin_value(ctx, current);
                let old_val = read_pinned_elem(ctx, old_pin, old_val);
                let current = read_pinned_elem(ctx, current_pin, current);
                // `replaceNode`: `cv == ev || (ev != null && cv.equals(ev))`,
                // the ARGUMENT's `equals` (`CRATONVM_COMPAT_CHM_VALUE_EQUALS`).
                // It may run Java; `seg`/`key`/`new_val` are re-read below.
                let eq = if chm_value_equals_enabled() {
                    values_equal_deep(ctx, &old_val, &current)?
                } else {
                    values_equal(ctx, &current, &old_val)
                };
                let r = if eq {
                    let seg = ctx.read_native_pin(seg_pin, seg);
                    let key = read_pinned_elem(ctx, key_pin, key);
                    let new_val = read_pinned_elem(ctx, new_pin, new_val);
                    native_map_put(ctx, &[Value::Object(Some(seg)), key, new_val])?;
                    Ok(Some(Value::Int(1)))
                } else {
                    Ok(Some(Value::Int(0)))
                };
                ctx.unpin_native_roots(seg_pin);
                r
            }
            None => Ok(Some(Value::Int(0))),
        }
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_chm_for_each_parallel(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // RULE F on argument **2**, not 1: this is
    // `forEach(long parallelismThreshold, BiConsumer)`, whose first parameter
    // is a `long`. Confirming which argument is the function before copying the
    // guard is the point — the sibling `native_chm_for_each` checks arg 1.
    reject_null_functional(args.get(2))?;
    let action = match args.get(2) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(None),
    };
    let entries = chm_collect_all_entries(ctx, this);
    let action_pin = ctx.pin_native_root(action);
    let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (_, flat_pins) = pin_value_slice(ctx, &flat);
    for i in 0..entries.len() {
        let action = ctx.read_native_pin(action_pin, action);
        let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
        let value = read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]);
        if let Err(e) = ctx.invoke_virtual(
            action,
            "accept",
            "(Ljava/lang/Object;Ljava/lang/Object;)V",
            &[key, value],
        ) {
            ctx.unpin_native_roots(action_pin);
            return Err(e);
        }
    }
    ctx.unpin_native_roots(action_pin);
    Ok(None)
}

/// `ConcurrentHashMap.reduceValues(long parallelismThreshold, BiFunction reducer)`
/// — "the result of accumulating all values using the given reducer to combine
/// values, or null if none".
///
/// Measured `CHM.reduceValues=null` against HotSpot's `6`, on a map whose
/// ordinary `entrySet` iteration answered correctly in the SAME run
/// (`CHM.sortedContent={a=6}` matches, and that is `new TreeMap<>(chm)` walking
/// `entrySet`). The split is the `Traverser`: every method in the bulk-operation
/// family walks the real `table` field, and a natively-backed CHM keeps its
/// entries in the segmented side layout that never populates `table` — so the
/// real bytecode traverses an empty tree and reports "no values".
///
/// That is the worst answer available. `null` from `reduceValues` is exactly
/// what an empty map returns, so a caller cannot tell a lost traversal from a
/// legitimately empty reduction — the quiet shape the round-2 widening was
/// written to catch. `forEach(long, BiConsumer)` above was registered for the
/// same reason.
///
/// The fold matches `ReduceValuesTask`: seed with the first value, then
/// `reducer.apply(acc, next)`. The `parallelismThreshold` argument is read and
/// ignored, as it may be: it is a *hint*, and a sequential evaluation is a legal
/// answer for any value of it.
pub(crate) fn native_chm_reduce_values(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `if (reducer == null) throw new NullPointerException();` — the first
    // statement of the real method.
    let reducer = match args.get(2) {
        Some(Value::Object(Some(f))) => *f,
        Some(Value::Object(None)) => {
            return Err(RuntimeError::NullPointerException { message: None }.into())
        }
        _ => return Ok(Some(Value::Object(None))),
    };
    let values = chm_collect_all_values(ctx, this);
    // GC-SAFETY: `apply` is arbitrary interpreted bytecode. It can move the
    // reducer, every value still held only in this Rust Vec, and the accumulator
    // — which is the one that matters most, because it is fed back in as an
    // argument on the very next iteration.
    let reducer_pin = ctx.pin_native_root(reducer);
    let (_, value_pins) = pin_value_slice(ctx, &values);
    let mut acc: Option<Value> = None;
    let mut acc_pin = usize::MAX;
    let mut result: Result<Option<Value>, MethodCallFailed> = Ok(None);
    for i in 0..values.len() {
        let next = read_pinned_elem(ctx, value_pins[i], values[i]);
        let Some(prev) = acc else {
            acc = Some(next);
            acc_pin = pin_value(ctx, next);
            continue;
        };
        let reducer = ctx.read_native_pin(reducer_pin, reducer);
        let prev = read_pinned_elem(ctx, acc_pin, prev);
        match ctx.invoke_virtual(
            reducer,
            "apply",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[prev, next],
        ) {
            Ok(r) => {
                let combined = r.unwrap_or(Value::Object(None));
                acc = Some(combined);
                acc_pin = pin_value(ctx, combined);
            }
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }
    if let Ok(ref mut out) = result {
        *out = Some(match acc {
            Some(v) => read_pinned_elem(ctx, acc_pin, v),
            None => Value::Object(None),
        });
    }
    ctx.unpin_native_roots(reducer_pin);
    result
}

/// `ConcurrentHashMap.searchKeys(long parallelismThreshold, Function searchFunction)`
/// — "a non-null result from applying the given search function on each key, or
/// null if none".
///
/// Same cause and same silence as [`native_chm_reduce_values`]: measured
/// `CHM.searchKeys=null` against HotSpot's `found`, where `null` is also the
/// legitimate "nothing matched" answer. Short-circuits on the first non-null
/// result, as `SearchKeysTask` does.
pub(crate) fn native_chm_search_keys(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let search_fn = match args.get(2) {
        Some(Value::Object(Some(f))) => *f,
        Some(Value::Object(None)) => {
            return Err(RuntimeError::NullPointerException { message: None }.into())
        }
        _ => return Ok(Some(Value::Object(None))),
    };
    let keys = chm_collect_all_keys(ctx, this);
    let fn_pin = ctx.pin_native_root(search_fn);
    let (_, key_pins) = pin_value_slice(ctx, &keys);
    let mut result: MethodCallResult = Ok(Some(Value::Object(None)));
    for i in 0..keys.len() {
        let search_fn = ctx.read_native_pin(fn_pin, search_fn);
        let key = read_pinned_elem(ctx, key_pins[i], keys[i]);
        match ctx.invoke_virtual(
            search_fn,
            "apply",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[key],
        ) {
            Ok(Some(v @ Value::Object(Some(_)))) => {
                result = Ok(Some(v));
                break;
            }
            Ok(_) => {}
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }
    ctx.unpin_native_roots(fn_pin);
    result
}

pub(crate) fn native_chm_mapping_count(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let size = native_chm_size(ctx, args)?.unwrap_or(Value::Int(0));
    match size {
        Value::Int(v) => Ok(Some(Value::Long(v as i64))),
        _ => Ok(Some(Value::Long(0))),
    }
}

/// `ConcurrentHashMap.newKeySet()` — a real `KeySetView` over a real
/// native-backed `ConcurrentHashMap`, mapped value `Boolean.TRUE`.
///
/// This used to return a `java/util/HashSet` because a real `KeySetView`'s
/// Java-side `add()` runs `CHM.putVal`, which walks the `table` field
/// CratonVM's segmented layout never populates. The answer to that is not to
/// hand back a different class — a `HashSet` takes no lock, so the JDK's
/// canonical concurrent set corrupted under concurrent mutation and its
/// `size()` could go negative — it is to give `KeySetView` its own natives, so
/// no method of the view is left running `table`-walking bytecode. See the
/// module comment above `KSV_CLASS`.
pub(crate) fn native_chm_new_key_set(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let map = alloc_backing_chm(ctx, None)?;
    // `Boolean.valueOf` is arbitrary Java and can complete a moving GC, so the
    // fresh map — which nothing points at yet — has to stay rooted across it.
    let (map, mapped) = rooted_across1(ctx, map, ksv_boxed_true);
    let view = make_key_set_view(ctx, map, mapped)?;
    Ok(Some(Value::Object(Some(view))))
}

/// `ConcurrentHashMap.newKeySet(int initialCapacity)`. Static, so the capacity
/// is the first argument — but the dispatch path can shift argument positions
/// (see `newSetFromMap`), hence the search rather than a fixed index.
pub(crate) fn native_chm_new_key_set_cap(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `newKeySet(int)` is `new KeySetView<>(new ConcurrentHashMap<>(cap), TRUE)`,
    // so it inherits the constructor's refusal — and the `find_map` below,
    // which skips any non-positive argument, was exactly the clamp that hid it.
    if let Some(Value::Int(c)) = args.iter().find(|v| matches!(v, Value::Int(_))) {
        chm_reject_bad_ctor_args(Some(&Value::Int(*c)), None, None)?;
    }
    let cap = args.iter().find_map(|v| match v {
        Value::Int(n) if *n > 0 => Some(*n),
        _ => None,
    });
    let map = alloc_backing_chm(ctx, cap)?;
    let (map, mapped) = rooted_across1(ctx, map, ksv_boxed_true);
    let view = make_key_set_view(ctx, map, mapped)?;
    Ok(Some(Value::Object(Some(view))))
}

/// `ConcurrentHashMap.keySet(V mappedValue)` — a LIVE, add-able view of this
/// map's keys, exactly as the JDK's `new KeySetView<>(this, mappedValue)`.
///
/// Previously this delegated to `keySet()`, whose product has no `add` (a
/// `keySet()` view must not be add-able) and is not a `KeySetView`, so the one
/// thing this overload exists for did not work.
pub(crate) fn native_chm_key_set_view(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let mapped = args.get(1).copied().unwrap_or(Value::Object(None));
    // MEASURED 2026-08-16: `keySet(null)` is `msg=<NO-MESSAGE>`. The JDK body
    // opens `0: aload_1 / 1: ifnonnull 12 / 4: new NullPointerException /
    // 8: invokespecial <init>()V / 11: athrow` — the no-arg constructor again.
    // `"ConcurrentHashMap.keySet(null)"` was the tenth fabricated string in
    // this family; see G1-1.
    if matches!(mapped, Value::Object(None)) {
        return Err(bare_npe());
    }
    let view = make_key_set_view(ctx, this, mapped)?;
    Ok(Some(Value::Object(Some(view))))
}

// ===========================================================================
// ConcurrentHashMap$KeySetView — the object `newKeySet()` / `keySet(V)` returns
// ===========================================================================
//
// `newKeySet()` used to hand back a plain `java/util/HashSet`. Two things were
// wrong with that. It is not the class the JDK promises, so
// `(ConcurrentHashMap.KeySetView<K,?>) set` threw `ClassCastException` and
// `getMappedValue()` was a `NoSuchMethodError`. And, far worse, a `HashSet`'s
// `add`/`remove`/`size` run the `HashMap` natives, which take no lock — so
// `ConcurrentHashMap.newKeySet()`, the JDK's canonical "give me a concurrent
// set", was answered with a structure that corrupts under concurrent mutation.
// Balanced churn (every `add` followed by its own `remove`, correct final size
// 0 on any interleaving) left the size at 18, at -3, at 131; a real
// `ConcurrentHashMap.size()` cannot even go negative. `isEmpty()` is
// `size() == 0`, and `ThreadPerTaskExecutor.tryTerminate()` only advances
// SHUTDOWN → TERMINATED when its `newKeySet()` of live threads reports empty —
// so `Executors.newVirtualThreadPerTaskExecutor()` in a try-with-resources hung
// forever on the closing brace. See the retired
// `concurrenthashmap-newkeyset-returns-a-plain-hashset` write-up.
//
// The view is now a real `ConcurrentHashMap$KeySetView` over a real
// native-backed `ConcurrentHashMap`, so every mutation inherits the per-segment
// monitor that already makes `native_chm_put` / `native_chm_remove` safe, and
// `size()` is the sum of per-segment counts that a writer only ever updates
// under that segment's lock.
//
// The surface below is deliberately COMPLETE rather than "the methods the
// reproducer needs". In real-JDK mode the methods `CollectionView` declares
// (`size` / `isEmpty` / `clear` / `toArray` / `toString` / `containsAll` /
// `removeAll` / `retainAll`) are written in terms of `map` — already native —
// or of `iterator()`, native below, so their real bytecode is correct and
// these registrations are simply never consulted for them. In synthetic-jdk
// mode there is no `CollectionView` bytecode at all and the same registrations
// are the only implementation. Leaving any of them out would reproduce the
// partial-surface trap the known-issue doc warns about: a method left to real
// bytecode walks the `table` field CratonVM's segmented layout never
// populates, and quietly answers empty.

pub(crate) const KSV_CLASS: &str = "java/util/concurrent/ConcurrentHashMap$KeySetView";
pub(crate) const CHM_CLASS: &str = "java/util/concurrent/ConcurrentHashMap";
/// `CollectionView.map`, slot 0 in the JDK's layout. Also the fallback when the
/// field cannot be resolved by name — synthetic-jdk mode fabricates the class,
/// so there is no declared field list to resolve against.
pub(crate) const KSV_FIELD_MAP: usize = 0;
/// `KeySetView.value` — the value `add(e)` maps `e` to.
pub(crate) const KSV_FIELD_VALUE: usize = 1;
pub(crate) const KSV_NUM_FIELDS: usize = 2;

/// Slot index of `name` on `this`, preferring the class's own declared layout
/// and falling back to the synthetic slot. Resolved by `ClassId` rather than by
/// class name so a redefined/duplicate-loader `ConcurrentHashMap` still
/// resolves (see `resolve_field_index_by_class_id`).
pub(crate) fn ksv_slot(ctx: &dyn NativeContext, this: ObjectRef, name: &str, fallback: usize) -> usize {
    let n = ctx.object_num_fields(this);
    let cid = ctx.class_id_of_object(this);
    match ctx.resolve_field_index_by_class_id(cid, name) {
        Some(i) if i < n => i,
        _ => fallback,
    }
}

/// The backing `ConcurrentHashMap` of a `KeySetView`.
pub(crate) fn ksv_map(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    let slot = ksv_slot(ctx, this, "map", KSV_FIELD_MAP);
    if slot >= ctx.object_num_fields(this) {
        return None;
    }
    match ctx.get_field(this, slot) {
        Value::Object(Some(m)) => Some(m),
        _ => None,
    }
}

/// `this`'s mapped value — what `add(e)` stores against `e`. `Object(None)` for
/// a view that has none, which is exactly the JDK's `add` → `UnsupportedOperation`
/// condition. Deliberately allocation-free: callers read it next to a
/// `ksv_map` read and must not have a GC in between.
pub(crate) fn ksv_mapped_value(ctx: &dyn NativeContext, this: ObjectRef) -> Value {
    let slot = ksv_slot(ctx, this, "value", KSV_FIELD_VALUE);
    if slot >= ctx.object_num_fields(this) {
        return Value::Object(None);
    }
    ctx.get_field(this, slot)
}

/// True when `obj`'s runtime class is `ConcurrentHashMap$KeySetView`.
#[inline]
pub(crate) fn is_key_set_view(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    receiver_facts(ctx, obj).has(CF_KEY_SET_VIEW)
}

/// Reroute a `KeySetView` receiver that arrived through a generic
/// `HashSet`/`ArrayList`-shaped native to the view's own implementation.
///
/// Needed because the *interface* registrations — `java/util/Set.size`,
/// `java/util/Collection.iterator`, `java/util/AbstractSet.hashCode`, … — are
/// what a call site resolves to when the receiver's class declares no such
/// method, which in synthetic-jdk mode is every method of the fabricated
/// `KeySetView` class. Those natives win over the exact-class registrations,
/// and a `KeySetView` matches neither layout they read, so `size()` answered 0
/// and `iterator()` answered null (a `for (x : set)` NPE) while `add` /
/// `contains` / `remove` — which have no interface-level registration and so
/// fall through to the exact-class natives — worked. Guarding at the generic
/// entry points is what keeps that split from existing at all.
#[inline]
pub(crate) fn ksv_route(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    imp: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult,
) -> Option<MethodCallResult> {
    match args.first() {
        Some(Value::Object(Some(o))) if is_key_set_view(ctx, *o) => Some(imp(ctx, args)),
        _ => None,
    }
}

/// `Boolean.TRUE` — `newKeySet()`'s mapped value in the JDK. Falls back to the
/// unboxed truth so a fabricated view still has a non-null mapped value (the
/// CHM natives reject a null value per JDK spec, so `add` would otherwise NPE).
pub(crate) fn ksv_boxed_true(ctx: &mut dyn NativeContext) -> Value {
    match ctx.invoke(
        "java/lang/Boolean",
        "valueOf",
        "(Z)Ljava/lang/Boolean;",
        &[Value::Int(1)],
    ) {
        Ok(Some(v @ Value::Object(Some(_)))) => v,
        _ => Value::Int(1),
    }
}

/// Allocate a `KeySetView` instance with no constructor run.
///
/// In real-JDK mode allocate with the class's OWN field count rather than the
/// hard-coded two: `set_field` past the end of an under-sized object is dropped
/// silently, so a JDK that ever adds a field would leave `map` unset and every
/// method below answering empty.
pub(crate) fn alloc_key_set_view_object(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    // `ensure_class_initialized` can report success having FABRICATED a
    // stand-in, so check the resolved name rather than trusting `Ok`.
    let mut real_cid = None;
    if let Ok(cid) = ctx.ensure_class_initialized(KSV_CLASS) {
        if ctx.class_name_arc_of_id(cid).as_deref() == Some(KSV_CLASS) {
            real_cid = Some(cid);
        }
    }
    match real_cid {
        Some(cid) => {
            let n = ctx.class_num_total_fields(cid).max(KSV_NUM_FIELDS);
            Ok(ctx.alloc_object(cid, n))
        }
        None => try_alloc_synthetic(ctx, KSV_CLASS, KSV_NUM_FIELDS),
    }
}

/// Allocate a fresh, fully segmented native `ConcurrentHashMap` with no
/// constructor run — the backing store for a `newKeySet()` view.
///
/// `initial_capacity` mirrors `newKeySet(int)`; `None` reproduces the no-arg
/// `ConcurrentHashMap()` shape so iteration order matches a default map.
pub(crate) fn alloc_backing_chm(
    ctx: &mut dyn NativeContext,
    initial_capacity: Option<i32>,
) -> Result<ObjectRef, MethodCallFailed> {
    let mut real_cid = None;
    if let Ok(cid) = ctx.ensure_class_initialized(CHM_CLASS) {
        if ctx.class_name_arc_of_id(cid).as_deref() == Some(CHM_CLASS) {
            real_cid = Some(cid);
        }
    }
    let chm = match real_cid {
        Some(cid) => {
            let n = ctx.class_num_total_fields(cid).max(_CHM_NUM_FIELDS);
            ctx.alloc_object(cid, n)
        }
        None => try_alloc_synthetic(ctx, CHM_CLASS, _CHM_NUM_FIELDS)?,
    };
    // Segment construction is 2N+1 allocations deep; keep the fresh map rooted
    // across it, since nothing points at it yet.
    let chm_pin = ctx.pin_native_root(chm);
    match initial_capacity {
        Some(cap) => {
            let _ = native_chm_init_capacity(ctx, &[Value::Object(Some(chm)), Value::Int(cap)]);
        }
        None => chm_init_segments(ctx, chm, CHM_DEFAULT_INIT_SEGMENTS, CHM_DEFAULT_SEGMENT_CAP),
    }
    let chm = ctx.read_native_pin(chm_pin, chm);
    ctx.unpin_native_roots(chm_pin);
    Ok(chm)
}

/// Build a `KeySetView` over `map` with mapped value `mapped`.
pub(crate) fn make_key_set_view(
    ctx: &mut dyn NativeContext,
    map: ObjectRef,
    mapped: Value,
) -> Result<ObjectRef, MethodCallFailed> {
    // The view allocation can move both operands; `map_pin` is the unpin base.
    let map_pin = ctx.pin_native_root(map);
    let mapped_pin = pin_value(ctx, mapped);
    let view = alloc_key_set_view_object(ctx)?;
    let view_pin = ctx.pin_native_root(view);
    let map = ctx.read_native_pin(map_pin, map);
    let mapped = read_pinned_elem(ctx, mapped_pin, mapped);
    let view = ctx.read_native_pin(view_pin, view);
    let map_slot = ksv_slot(ctx, view, "map", KSV_FIELD_MAP);
    let value_slot = ksv_slot(ctx, view, "value", KSV_FIELD_VALUE);
    ctx.set_field(view, map_slot, Value::Object(Some(map)));
    ctx.set_field(view, value_slot, mapped);
    ctx.unpin_native_roots(map_pin);
    Ok(view)
}

/// The view's current elements. Weakly consistent, exactly like the JDK's own
/// `KeySetView` iterator: a concurrent writer's effect may or may not be seen.
pub(crate) fn ksv_collect(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<Value> {
    match ksv_map(ctx, this) {
        Some(m) => chm_collect_all_keys(ctx, m),
        None => Vec::new(),
    }
}

/// `ksv_collect` into a fresh `Object[]`, GC-safely.
///
/// The array allocation can move every ref the first collection returned, so
/// count first, allocate, then RE-collect from the live map and store with
/// nothing allocating in between — the `native_hs_iterator` / `native_hs_to_array`
/// idiom. Returns the array and its live element count; a concurrent removal
/// between the two passes trims the array rather than leaving null holes, which
/// is what `CollectionView.toArray`'s own `Arrays.copyOf(r, i)` does.
pub(crate) fn ksv_snapshot_array(ctx: &mut dyn NativeContext, this: ObjectRef) -> (ObjectRef, usize) {
    let this_pin = ctx.pin_native_root(this);
    let len = ksv_collect(ctx, this).len();
    let this = ctx.read_native_pin(this_pin, this);
    let arr = alloc_ref_array(ctx, len);
    let arr_pin = ctx.pin_native_root(arr);
    let this = ctx.read_native_pin(this_pin, this);
    let elems = ksv_collect(ctx, this);
    let arr = ctx.read_native_pin(arr_pin, arr);
    let n = len.min(elems.len());
    for (i, e) in elems.iter().enumerate().take(n) {
        ctx.set_array_element(arr, i, *e);
    }
    ctx.unpin_native_roots(this_pin);
    if n == len {
        return (arr, n);
    }
    let arr_pin = ctx.pin_native_root(arr);
    let trimmed = alloc_ref_array(ctx, n);
    let trimmed_pin = ctx.pin_native_root(trimmed);
    let arr = ctx.read_native_pin(arr_pin, arr);
    let trimmed = ctx.read_native_pin(trimmed_pin, trimmed);
    for i in 0..n {
        let v = ctx.get_array_element(arr, i);
        ctx.set_array_element(trimmed, i, v);
    }
    ctx.unpin_native_roots(arr_pin);
    (trimmed, n)
}

pub(crate) fn native_ksv_get_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(match ksv_map(ctx, this) {
        Some(m) => Value::Object(Some(m)),
        None => Value::Object(None),
    }))
}

pub(crate) fn native_ksv_get_mapped_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ksv_mapped_value(ctx, this)))
}

pub(crate) fn native_ksv_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    match ksv_map(ctx, this) {
        Some(m) => native_chm_size(ctx, &[Value::Object(Some(m))]),
        None => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_ksv_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let size = match native_ksv_size(ctx, args)? {
        Some(Value::Int(n)) => n,
        _ => 0,
    };
    Ok(Some(Value::Int(i32::from(size == 0))))
}

pub(crate) fn native_ksv_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    match ksv_map(ctx, this) {
        Some(m) => native_chm_clear(ctx, &[Value::Object(Some(m))]),
        None => Ok(None),
    }
}

pub(crate) fn native_ksv_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    match ksv_map(ctx, this) {
        // `containsKey` carries the JDK's null-key NPE, which is also
        // `KeySetView.contains(null)`'s contract.
        Some(m) => native_chm_contains_key(ctx, &[Value::Object(Some(m)), elem]),
        None => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_ksv_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    // JDK: `if ((v = value) == null) throw new UnsupportedOperationException();`
    // — a view from `keySet()` (no mapped value) cannot be added to.
    let mapped = match ksv_mapped_value(ctx, this) {
        Value::Object(None) => return Err(unsupported_op()),
        v => v,
    };
    let map = match ksv_map(ctx, this) {
        Some(m) => m,
        None => return Ok(Some(Value::Int(0))),
    };
    // `putIfAbsent` takes the target segment's monitor, so concurrent adds are
    // safe and the per-segment count stays exact. Its null-key guard is
    // `add(null)`'s NPE.
    let prior = native_chm_put_if_absent(ctx, &[Value::Object(Some(map)), elem, mapped])?;
    Ok(Some(Value::Int(i32::from(matches!(
        prior,
        None | Some(Value::Object(None))
    )))))
}

pub(crate) fn native_ksv_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let map = match ksv_map(ctx, this) {
        Some(m) => m,
        None => return Ok(Some(Value::Int(0))),
    };
    // JDK: `return map.remove(o) != null;` — a CHM never holds a null value, so
    // the returned value doubles as the membership answer.
    let prior = native_chm_remove(ctx, &[Value::Object(Some(map)), elem])?;
    Ok(Some(Value::Int(i32::from(matches!(
        prior,
        Some(Value::Object(Some(_)))
    )))))
}

pub(crate) fn native_ksv_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let this_pin = ctx.pin_native_root(this);
    let (arr, total) = ksv_snapshot_array(ctx, this);
    let arr_pin = ctx.pin_native_root(arr);
    // Fallible since 2026-08-10; the refusal ARM below was added 2026-08-11,
    // and the reason the arm was missing is worth keeping because it was a
    // correct argument that its own file had already outgrown.
    //
    // The 2026-08-10 note said this site must refuse rather than land on a real
    // `Arrays$ArrayItr`, because HotSpot's `KeySetView.iterator()` returns a
    // `KeyIterator` whose `remove()` writes through to the map, `RChmKeySetView`
    // exercises exactly that, and a fixed-size list's iterator answers
    // `UnsupportedOperationException: remove`. Every clause of that was true
    // when it was written and the last one no longer is: `real_snapshot_iterator`
    // takes a `backing` argument, [`snapshot_itr_backing_table`] carries the
    // pointer the real two-field class has no slot for, and
    // `native_snapshot_itr_remove`'s `SetLike` arm ALREADY discriminates a key-set
    // view (`if is_key_set_view(ctx, backing) { native_ksv_remove(..) }`) — the
    // write-through this site was held back for was built for this receiver and
    // then not wired to it. Measured on the pre-fix binary: `--jdk-only`
    // `RChmKeySetView` died at `surface():140` with
    // `NoClassDefFoundError: java/util/HashMap$KeyItr` on the plain `for (String
    // x : s)`, i.e. the refusal cost the whole iteration, not just `remove()`.
    //
    // What is NOT available here, and why the refusal cannot simply be dropped:
    // real `KeySetView.iterator()` bytecode reads `ConcurrentHashMap.table`, and
    // on a CratonVM-native CHM that field is null — measured reflectively under
    // `--add-opens java.base/java.util.concurrent=ALL-UNNAMED`: `table = null`,
    // `baseCount = 0` for a map with one entry, against `Node[16]`/`1` on
    // HotSpot. Letting the real iterator run would iterate EMPTY, which is the
    // failure mode that reads as a pass. The snapshot keeps the natives the one
    // owner of the state; only the class name becomes honest.
    //
    // `Compatible` is untouched: `try_alloc_synthetic` succeeds there, so this
    // arm never runs and the fabricated `HashMap$KeyItr` is still what
    // `--real-jdk` hands back.
    //
    // `alloc_key_itr`, not the fabricated `HashMap$KeyItr` shape: HotSpot
    // answers `ConcurrentHashMap$KeyIterator` here and this VM answered a
    // fabrication in compatible mode and `Arrays$ArrayItr` under `--jdk-only`.
    // `alloc_key_itr` still falls back to the fabricated shape when the image
    // has no real class, so a stripped image keeps the refusal arm below.
    // gen r4w4/rooting2: `ksv_snapshot_array` allocated, so the receiver whose
    // class picks the carrier is re-read from its pin first.
    let this_now = ctx.read_native_pin(this_pin, this);
    let carrier = key_itr_carrier_for(&*ctx, this_now, false);
    let itr = match alloc_key_itr(ctx, carrier) {
        Ok(itr) => itr,
        Err(_refused) => {
            let arr = ctx.read_native_pin(arr_pin, arr);
            let this = ctx.read_native_pin(this_pin, this);
            let real =
                real_snapshot_iterator(ctx, arr, total, Some((this, SnapshotItrRoute::SetLike)));
            ctx.unpin_native_roots(this_pin);
            return real;
        }
    };
    // `base + FIELD`, not `FIELD`. These five writes were ABSOLUTE, which was
    // right for exactly one carrier: the fabricated `java/util/HashMap$KeyItr`
    // declares no fields of its own, so its `key_itr_base` is 0 and the
    // distinction was invisible. Minting the real
    // `ConcurrentHashMap$KeyIterator` — which declares ten — made it visible at
    // once: every READER in this file (`native_map_key_itr_has_next`, `_next`,
    // `_remove`, `map_itr_comod_source`) offsets by `key_itr_base`, so the
    // snapshot landed ten slots below where they look. MEASURED: the first
    // `next()` on a three-element key set threw `NoSuchElementException`, and
    // `remove()` before `next()` answered `UnsupportedOperationException` where
    // HotSpot says `IllegalStateException` — a cursor reading a total of zero
    // and a backing of null.
    let base = key_itr_base(&*ctx, itr);
    let arr = ctx.read_native_pin(arr_pin, arr);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(itr, base + MAP_KEY_ITR_FIELD_KEYS, Value::Object(Some(arr)));
    ctx.set_field(itr, base + MAP_KEY_ITR_FIELD_CURSOR, Value::Int(0));
    ctx.set_field(
        itr,
        base + MAP_KEY_ITR_FIELD_TOTAL,
        Value::Int(total as i32),
    );
    // Wire the view itself as the removal target: `CollectionView.retainAll`
    // and `removeAll` are written in terms of `it.remove()`, so an iterator
    // without a backing silently makes both no-ops.
    ctx.set_field(
        itr,
        base + MAP_KEY_ITR_FIELD_BACKING,
        Value::Object(Some(this)),
    );
    ctx.set_field(itr, base + MAP_KEY_ITR_FIELD_LAST_RET, Value::Int(-1));
    // Seed the fail-fast generation LAST, exactly as `native_hs_iterator` does:
    // `MAP_KEY_ITR_FIELD_BACKING` above is how `map_itr_comod_source` finds the
    // collection to watch. A `ConcurrentHashMap` iterator is weakly consistent
    // and must never throw `ConcurrentModificationException`, and it does not:
    // the comodification check reads a generation the CHM natives do not bump.
    map_itr_seed_expected(ctx, itr);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(itr))))
}

pub(crate) fn native_ksv_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (arr, _) = ksv_snapshot_array(ctx, this);
    Ok(Some(Value::Object(Some(arr))))
}

/// `toArray(T[])` — fill the caller's array when it is large enough (writing
/// the spec's null terminator), otherwise allocate a fresh array of the
/// template's runtime component type so `Set<String>.toArray(new String[0])`
/// stays a `String[]`.
pub(crate) fn native_ksv_to_array_typed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reject_null_to_array_template(args.get(1))?;
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let target = match args.get(1).copied() {
        Some(Value::Object(Some(t))) => Some(t),
        _ => None,
    };
    let target_pin = match target {
        Some(t) => ctx.pin_native_root(t),
        None => usize::MAX,
    };
    let this_pin = ctx.pin_native_root(this);
    let (src, n) = ksv_snapshot_array(ctx, this);
    let src_pin = ctx.pin_native_root(src);
    let target = target.map(|t| ctx.read_native_pin(target_pin, t));
    if let Some(t) = target {
        if ctx.array_length(t) >= n {
            let len = ctx.array_length(t);
            let src = ctx.read_native_pin(src_pin, src);
            for i in 0..n {
                let v = ctx.get_array_element(src, i);
                ctx.set_array_element(t, i, v);
            }
            if len > n {
                ctx.set_array_element(t, n, Value::Object(None));
            }
            ctx.unpin_native_roots(if target_pin == usize::MAX {
                this_pin
            } else {
                target_pin
            });
            return Ok(Some(Value::Object(Some(t))));
        }
    }
    let out = match target {
        Some(t) => {
            let comp = ctx.class_id_of_object(t);
            ctx.new_ref_array(comp, n)
        }
        None => alloc_ref_array(ctx, n),
    };
    let out_pin = ctx.pin_native_root(out);
    let src = ctx.read_native_pin(src_pin, src);
    let out = ctx.read_native_pin(out_pin, out);
    for i in 0..n {
        let v = ctx.get_array_element(src, i);
        ctx.set_array_element(out, i, v);
    }
    ctx.unpin_native_roots(if target_pin == usize::MAX {
        this_pin
    } else {
        target_pin
    });
    Ok(Some(Value::Object(Some(out))))
}

pub(crate) fn native_ksv_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let elems = ksv_collect(ctx, this);
    // `obj_to_display_units` dispatches `toString()`, which allocates — pin
    // every element and re-read it at the point of use.
    let (elems_pin, handles) = pin_value_slice(ctx, &elems);
    let mut parts = Vec::with_capacity(elems.len());
    for i in 0..elems.len() {
        let e = read_pinned_elem(ctx, handles[i], elems[i]);
        parts.push(obj_to_display_units(ctx, &e)?);
    }
    if elems_pin != usize::MAX {
        ctx.unpin_native_roots(elems_pin);
    }
    let text = wrap_join("[", &parts, ", ", "]");
    let s = ctx.create_string_from_units(&text);
    Ok(Some(Value::Object(Some(s))))
}

pub(crate) fn native_ksv_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elems = ksv_collect(ctx, this);
    let (elems_pin, handles) = pin_value_slice(ctx, &elems);
    let mut h: i32 = 0;
    for i in 0..elems.len() {
        let e = read_pinned_elem(ctx, handles[i], elems[i]);
        // `Set.hashCode` contract: the sum of the elements' hash codes.
        match element_hash_code(ctx, &e) {
            Ok(eh) => h = h.wrapping_add(eh),
            Err(err) => {
                if elems_pin != usize::MAX {
                    ctx.unpin_native_roots(elems_pin);
                }
                return Err(err);
            }
        }
    }
    if elems_pin != usize::MAX {
        ctx.unpin_native_roots(elems_pin);
    }
    Ok(Some(Value::Int(h)))
}

pub(crate) fn native_ksv_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(Some(Value::Int(0))),
    };
    if std::ptr::eq(this.as_ptr(), other.as_ptr()) {
        return Ok(Some(Value::Int(1)));
    }
    // `AbstractSet.equals`: a Set equals only another Set. The guard must
    // precede the `size()` dispatch, or `set.equals("text")` raises
    // NoSuchMethodError instead of answering false.
    if !obj_is_instance_of(ctx, other, "java/util/Set") {
        return Ok(Some(Value::Int(0)));
    }
    let this_pin = ctx.pin_native_root(this);
    let other_pin = ctx.pin_native_root(other);
    let result = (|| -> MethodCallResult {
        let other = ctx.read_native_pin(other_pin, other);
        let other_size = match ctx.invoke_virtual(other, "size", "()I", &[])? {
            Some(Value::Int(n)) => n,
            _ => return Ok(Some(Value::Int(0))),
        };
        let this = ctx.read_native_pin(this_pin, this);
        let elems = ksv_collect(ctx, this);
        if elems.len() as i32 != other_size {
            return Ok(Some(Value::Int(0)));
        }
        let (_, handles) = pin_value_slice(ctx, &elems);
        for i in 0..elems.len() {
            let other = ctx.read_native_pin(other_pin, other);
            let e = read_pinned_elem(ctx, handles[i], elems[i]);
            let contains = ctx.invoke_virtual(other, "contains", "(Ljava/lang/Object;)Z", &[e])?;
            if !matches!(contains, Some(Value::Int(1))) {
                return Ok(Some(Value::Int(0)));
            }
        }
        Ok(Some(Value::Int(1)))
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_ksv_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // RULE F. MEASURED on `ConcurrentHashMap.keySet()`, empty or not. Also
    // reached via `ksv_route` from `native_al_for_each` / `native_hs_for_each`,
    // which already check — the duplicate is idempotent and keeps this body
    // correct when it is entered directly by its own registration.
    reject_null_functional(args.get(1))?;
    let action = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(None),
    };
    let elems = ksv_collect(ctx, this);
    // `accept` is arbitrary Java: it allocates, so `action` and every element
    // must be pinned and re-read per dispatch.
    let action_pin = ctx.pin_native_root(action);
    let (_, handles) = pin_value_slice(ctx, &elems);
    let mut result = Ok(None);
    for i in 0..elems.len() {
        let a = ctx.read_native_pin(action_pin, action);
        let e = read_pinned_elem(ctx, handles[i], elems[i]);
        if let Err(err) = ctx.invoke_virtual(a, "accept", "(Ljava/lang/Object;)V", &[e]) {
            result = Err(err);
            break;
        }
    }
    ctx.unpin_native_roots(action_pin);
    result
}

pub(crate) fn native_ksv_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return make_stream(ctx, &[]),
    };
    let (arr, _) = ksv_snapshot_array(ctx, this);
    let arr_pin = ctx.pin_native_root(arr);
    let stream = try_alloc_synthetic(ctx, "java/util/stream/Stream", STREAM_NUM_FIELDS)?;
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field(stream, STREAM_FIELD_ELEMENTS, Value::Object(Some(arr)));
    ctx.set_field(stream, STREAM_FIELD_CLOSE_HANDLERS, Value::Object(None));
    ctx.unpin_native_roots(arr_pin);
    Ok(Some(Value::Object(Some(stream))))
}

/// `ConcurrentHashMap.keySet().spliterator()`.
///
/// FOUR fields, not three: the fourth carries this spliterator's own
/// CHARACTERISTICS, which [`native_spliterator_characteristics`] reads when it
/// is present. Every synthetic spliterator in the VM shared one constant
/// `SIZED | SUBSIZED | ORDERED`, and for this receiver all three of those bits
/// are wrong while the two that matter are missing. MEASURED against HotSpot
/// 25.0.4+7, both modes:
///
/// ```text
///   chm.keySet().spliterator().characteristics() & Spliterator.CONCURRENT
///     HotSpot  non-zero      CratonVM  0
/// ```
///
/// `CONCURRENT` is the load-bearing one and it is not cosmetic: it is how a
/// stream learns that its source can change under it, so a stream pipeline told
/// `SIZED | SUBSIZED` instead is entitled to pre-size its result array to a
/// count that may already be stale. `DISTINCT` and `NONNULL` come with it
/// because `ConcurrentHashMap$KeySpliterator` reports exactly those three, and
/// `ORDERED` is dropped because a hash container has no encounter order to
/// promise.
pub(crate) fn native_ksv_spliterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (arr, n) = ksv_snapshot_array(ctx, this);
    let arr_pin = ctx.pin_native_root(arr);
    let spl = try_alloc_synthetic(ctx, "java/util/Spliterator", 4)?;
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field(spl, 0, Value::Object(Some(arr)));
    ctx.set_field(spl, 1, Value::Int(0));
    ctx.set_field(spl, 2, Value::Int(n as i32));
    ctx.set_field(spl, 3, Value::Int(SPLITERATOR_CHM_KEYSET_CHARACTERISTICS));
    ctx.unpin_native_roots(arr_pin);
    Ok(Some(Value::Object(Some(spl))))
}

/// `DISTINCT | NONNULL | CONCURRENT` — what
/// `ConcurrentHashMap$KeySpliterator.characteristics()` returns.
pub(crate) const SPLITERATOR_CHM_KEYSET_CHARACTERISTICS: i32 = 0x1 | 0x100 | 0x1000;

pub(crate) fn native_ksv_contains_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // C4W4-6: `CollectionView.containsAll`'s `for (Object e : c)` on null.
    reject_null_contains_all_arg(args.get(1))?;
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => return Ok(Some(Value::Int(0))),
    };
    let this_pin = ctx.pin_native_root(this);
    // JIT round 16 (lane compatdrain), r15w4-compat5 item 6: see
    // [`contains_all_by_iterator`].
    if bulk_add_jdk_questions_enabled() && copy_source_is_foreign(&*ctx, coll) {
        let walked = contains_all_by_iterator(ctx, (this_pin, this), coll, native_ksv_contains);
        ctx.unpin_native_roots(this_pin);
        return walked.map(|all| Some(Value::Int(i32::from(all))));
    }
    // A throwing walk releases `this_pin` (round 15 wave 4, lane compat5).
    let collected = collect_collection_elements_or_real(ctx, coll);
    let elems = unpin_on_err(ctx, this_pin, collected)?;
    let (_, handles) = pin_value_slice(ctx, &elems);
    let mut answer = 1;
    let mut failure = None;
    for i in 0..elems.len() {
        let this = ctx.read_native_pin(this_pin, this);
        let e = read_pinned_elem(ctx, handles[i], elems[i]);
        match native_ksv_contains(ctx, &[Value::Object(Some(this)), e]) {
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
    match failure {
        Some(err) => Err(err),
        None => Ok(Some(Value::Int(answer))),
    }
}

pub(crate) fn native_ksv_add_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => return Ok(Some(Value::Int(0))),
    };
    // JDK: `addAll` throws before looking at the collection, so an EMPTY
    // collection throws too. Deferring to the per-element `add` below would
    // quietly return `false` for that case.
    if matches!(ksv_mapped_value(ctx, this), Value::Object(None)) {
        return Err(unsupported_op());
    }
    let this_pin = ctx.pin_native_root(this);
    let elems = collect_collection_elements_or_real(ctx, coll)?;
    let (_, handles) = pin_value_slice(ctx, &elems);
    let mut modified = false;
    let mut failure = None;
    for i in 0..elems.len() {
        let this = ctx.read_native_pin(this_pin, this);
        let e = read_pinned_elem(ctx, handles[i], elems[i]);
        match native_ksv_add(ctx, &[Value::Object(Some(this)), e]) {
            Ok(Some(Value::Int(1))) => modified = true,
            Ok(_) => {}
            Err(err) => {
                failure = Some(err);
                break;
            }
        }
    }
    ctx.unpin_native_roots(this_pin);
    match failure {
        Some(err) => Err(err),
        None => Ok(Some(Value::Int(i32::from(modified)))),
    }
}

pub(crate) fn native_ksv_remove_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => return Ok(Some(Value::Int(0))),
    };
    let this_pin = ctx.pin_native_root(this);
    let elems = collect_collection_elements_or_real(ctx, coll)?;
    let (_, handles) = pin_value_slice(ctx, &elems);
    let mut modified = false;
    let mut failure = None;
    for i in 0..elems.len() {
        let this = ctx.read_native_pin(this_pin, this);
        let e = read_pinned_elem(ctx, handles[i], elems[i]);
        match native_ksv_remove(ctx, &[Value::Object(Some(this)), e]) {
            Ok(Some(Value::Int(1))) => modified = true,
            Ok(_) => {}
            Err(err) => {
                failure = Some(err);
                break;
            }
        }
    }
    ctx.unpin_native_roots(this_pin);
    match failure {
        Some(err) => Err(err),
        None => Ok(Some(Value::Int(i32::from(modified)))),
    }
}

pub(crate) fn native_ksv_retain_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => return Ok(Some(Value::Int(0))),
    };
    let this_pin = ctx.pin_native_root(this);
    let keep = collect_collection_elements_or_real(ctx, coll)?;
    let this_now = ctx.read_native_pin(this_pin, this);
    let current = ksv_collect(ctx, this_now);
    let (_, keep_handles) = pin_value_slice(ctx, &keep);
    let (_, cur_handles) = pin_value_slice(ctx, &current);
    let mut modified = false;
    let mut failure = None;
    for i in 0..current.len() {
        let mut e = read_pinned_elem(ctx, cur_handles[i], current[i]);
        let mut should_keep = false;
        for j in 0..keep.len() {
            let k = read_pinned_elem(ctx, keep_handles[j], keep[j]);
            let matched = list_element_matches(ctx, &k, &e)?;
            e = read_pinned_elem(ctx, cur_handles[i], e);
            if matched {
                should_keep = true;
                break;
            }
        }
        if should_keep {
            continue;
        }
        let this = ctx.read_native_pin(this_pin, this);
        match native_ksv_remove(ctx, &[Value::Object(Some(this)), e]) {
            Ok(Some(Value::Int(1))) => modified = true,
            Ok(_) => {}
            Err(err) => {
                failure = Some(err);
                break;
            }
        }
    }
    ctx.unpin_native_roots(this_pin);
    match failure {
        Some(err) => Err(err),
        None => Ok(Some(Value::Int(i32::from(modified)))),
    }
}

pub(crate) fn native_ksv_remove_if(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // RULE F, empty receiver included. See `reject_null_functional`.
    reject_null_functional(args.get(1))?;
    let pred = match args.get(1) {
        Some(Value::Object(Some(p))) => *p,
        _ => return Ok(Some(Value::Int(0))),
    };
    let this_pin = ctx.pin_native_root(this);
    let pred_pin = ctx.pin_native_root(pred);
    let this_now = ctx.read_native_pin(this_pin, this);
    let elems = ksv_collect(ctx, this_now);
    let (_, handles) = pin_value_slice(ctx, &elems);
    let mut modified = false;
    let mut failure = None;
    for i in 0..elems.len() {
        let p = ctx.read_native_pin(pred_pin, pred);
        let e = read_pinned_elem(ctx, handles[i], elems[i]);
        let verdict = match ctx.invoke_virtual(p, "test", "(Ljava/lang/Object;)Z", &[e]) {
            Ok(v) => matches!(v, Some(Value::Int(1))),
            Err(err) => {
                failure = Some(err);
                break;
            }
        };
        if !verdict {
            continue;
        }
        let this = ctx.read_native_pin(this_pin, this);
        let e = read_pinned_elem(ctx, handles[i], elems[i]);
        match native_ksv_remove(ctx, &[Value::Object(Some(this)), e]) {
            Ok(Some(Value::Int(1))) => modified = true,
            Ok(_) => {}
            Err(err) => {
                failure = Some(err);
                break;
            }
        }
    }
    ctx.unpin_native_roots(this_pin);
    match failure {
        Some(err) => Err(err),
        None => Ok(Some(Value::Int(i32::from(modified)))),
    }
}

/// Public builder for a live, add-able, concurrency-safe `Set` view over an
/// existing native-backed `ConcurrentHashMap`, mapped value `Boolean.TRUE`.
///
/// `Collections.newSetFromMap(new ConcurrentHashMap<>())` — the other canonical
/// spelling of "give me a concurrent set" — used to answer with a plain
/// `HashSet` for exactly the same reason `newKeySet()` did, and with exactly
/// the same consequence: `add`/`remove`/`size` on the result ran the unlocked
/// `HashMap` natives. A `newSetFromMap` over an empty map is definitionally the
/// same object as `map.keySet(Boolean.TRUE)`, so it is built here rather than
/// duplicated. See `native-builtins`' `newSetFromMap` registration.
pub fn make_concurrent_key_set_view(
    ctx: &mut dyn NativeContext,
    map: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let (map, mapped) = rooted_across1(ctx, map, ksv_boxed_true);
    Ok(make_key_set_view(ctx, map, mapped)?)
}

pub(crate) fn register_chm_key_set_view_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = KSV_CLASS;
    r.register(c, "size", "()I", native_ksv_size);
    r.register(c, "isEmpty", "()Z", native_ksv_is_empty);
    r.register(c, "clear", "()V", native_ksv_clear);
    r.register(c, "contains", "(Ljava/lang/Object;)Z", native_ksv_contains);
    r.register(c, "add", "(Ljava/lang/Object;)Z", native_ksv_add);
    r.register(c, "remove", "(Ljava/lang/Object;)Z", native_ksv_remove);
    r.register(c, "iterator", "()Ljava/util/Iterator;", native_ksv_iterator);
    r.register(c, "toArray", "()[Ljava/lang/Object;", native_ksv_to_array);
    r.register(
        c,
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
        native_ksv_to_array_typed,
    );
    r.register(
        c,
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
        native_collection_to_array_generator,
    );
    r.register(c, "toString", "()Ljava/lang/String;", native_ksv_to_string);
    r.register(c, "hashCode", "()I", native_ksv_hash_code);
    r.register(c, "equals", "(Ljava/lang/Object;)Z", native_ksv_equals);
    r.register(
        c,
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        native_ksv_for_each,
    );
    r.register(
        c,
        "stream",
        "()Ljava/util/stream/Stream;",
        native_ksv_stream,
    );
    r.register(
        c,
        "spliterator",
        "()Ljava/util/Spliterator;",
        native_ksv_spliterator,
    );
    r.register(
        c,
        "containsAll",
        "(Ljava/util/Collection;)Z",
        native_ksv_contains_all,
    );
    r.register(c, "addAll", "(Ljava/util/Collection;)Z", native_ksv_add_all);
    r.register(
        c,
        "removeAll",
        "(Ljava/util/Collection;)Z",
        native_ksv_remove_all,
    );
    r.register(
        c,
        "retainAll",
        "(Ljava/util/Collection;)Z",
        native_ksv_retain_all,
    );
    r.register(
        c,
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
        native_ksv_remove_if,
    );
    r.register(
        c,
        "getMappedValue",
        "()Ljava/lang/Object;",
        native_ksv_get_mapped_value,
    );
    r.register(
        c,
        "getMap",
        "()Ljava/util/concurrent/ConcurrentHashMap;",
        native_ksv_get_map,
    );
    r.set_category(__prev_cat);
}

/// A real `java.util.Enumeration` over `array`, or `None` when this image
/// cannot build one.
///
/// The `Enumeration` form of `real_snapshot_iterator` (spelled without an
/// intra-doc link because that helper is private and this item is not), and the
/// same rule in its strongest form: prefer a class the JDK BUILDS ITSELF over one whose
/// fields we fill. The snapshot is already an `Object[]`,
/// `java.util.Arrays$ArrayList` is the JDK's own fixed-size list over exactly
/// that shape, and `Collections.enumeration(Collection)` turns one into a real
/// `Enumeration` — real bytecode the whole way, so nothing here has to know
/// what the resulting anonymous class is CALLED (it is `java.util.Collections$3`
/// on JDK 25, and an anonymous class's number must not be written down).
///
/// Deliberately a duplicate of `native-builtins`' `classloader::
/// real_snapshot_enumeration`, not a call to it: `native-builtins` depends on
/// this crate, so the dependency cannot run the other way. This is the copy the
/// crate that owns `cratonvm/internal/SnapshotEnumeration` can reach; it is
/// `pub` so the `native-builtins` copies can converge on it later.
///
/// `hasMoreElements`/`nextElement` on the result are real JDK bytecode: neither
/// name is in `vm_exec`'s `check_override` chain, and the anonymous class
/// declares both concretely, so the interface-level
/// `java/util/Enumeration.hasMoreElements` native registered by
/// `register_collections_natives` never intercepts it (that native is reachable
/// only through the C25 *abstract*-method arm).
pub fn real_snapshot_enumeration(
    ctx: &mut dyn NativeContext,
    array: ObjectRef,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let list = match ctx.new_object_initialized(
        "java/util/Arrays$ArrayList",
        "([Ljava/lang/Object;)V",
        &[Value::Object(Some(array))],
    )? {
        Some(Value::Object(Some(list))) => list,
        _ => return Ok(None),
    };
    match ctx.invoke(
        "java/util/Collections",
        "enumeration",
        "(Ljava/util/Collection;)Ljava/util/Enumeration;",
        &[Value::Object(Some(list))],
    )? {
        Some(Value::Object(Some(enm))) => Ok(Some(enm)),
        _ => Ok(None),
    }
}

/// The snapshot enumeration over `elems`: the fabricated
/// `cratonvm/internal/SnapshotEnumeration` in `Compatible` mode, and a real
/// `java.util.Enumeration` when `--jdk-only` refuses that fabrication.
///
/// WHY A FALLBACK AND NOT A DROPPED SHADOW. The two callers that reach strict
/// mode are `ConcurrentHashMap.keys()`/`elements()`, and a natively-backed CHM
/// keeps its entries in a SEGMENTED layout that never populates `table` — the
/// same reason `native_chm_reduce_values` and `native_chm_search_keys` are
/// registered right beside them. Real `ConcurrentHashMap.keys()` bytecode
/// builds a `KeyIterator` over `table`, so deleting these natives would answer
/// an EMPTY enumeration over a populated map, which is strictly worse than the
/// refusal it replaced. Only the CARRIER was fabricated, so only the carrier is
/// replaced.
///
/// `Compatible` mode is byte-for-byte what it was: the fallback is reached only
/// from the refusal arm, which only strict mode takes.
pub(crate) fn make_snapshot_enumeration(
    ctx: &mut dyn NativeContext,
    elems: &[Value],
) -> Result<ObjectRef, MethodCallFailed> {
    let (elem_base, elem_handles) = pin_value_slice(ctx, elems);
    let arr = alloc_ref_array(ctx, elems.len());
    let arr_pin = ctx.pin_native_root(arr);
    for (i, val) in elems.iter().enumerate() {
        let arr = ctx.read_native_pin(arr_pin, arr);
        let val = read_pinned_elem(ctx, elem_handles[i], *val);
        ctx.set_array_element(arr, i, val);
    }
    // Unwind to this base on EVERY path. The old `?` on the allocation leaked
    // both pin sets on the refusal it is now the whole point of handling.
    let unpin_base = if elem_base == usize::MAX {
        arr_pin
    } else {
        elem_base
    };
    let out = match try_alloc_synthetic(ctx, "cratonvm/internal/SnapshotEnumeration", 2) {
        Ok(en) => {
            let en_pin = ctx.pin_native_root(en);
            let arr = ctx.read_native_pin(arr_pin, arr);
            let en = ctx.read_native_pin(en_pin, en);
            ctx.set_field(en, 0, Value::Object(Some(arr)));
            ctx.set_field(en, 1, Value::Int(0));
            Ok(en)
        }
        Err(refusal) => {
            let arr = ctx.read_native_pin(arr_pin, arr);
            match real_snapshot_enumeration(ctx, arr) {
                Ok(Some(en)) => Ok(en),
                // Nothing real to stand in — an image with no
                // `Arrays$ArrayList` — so the refusal stands rather than
                // silently becoming a fabrication again.
                Ok(None) => Err(refusal),
                Err(err) => Err(err),
            }
        }
    };
    ctx.unpin_native_roots(unpin_base);
    out
}

/// The real JDK bin-node class a `ConcurrentHashMap.table` is an array of.
pub(crate) const CHM_NODE_CLASS: &str = "java/util/concurrent/ConcurrentHashMap$Node";
/// The JDK's own dual `Enumeration`/`Iterator` carriers.
pub(crate) const CHM_KEY_ITER_CLASS: &str = "java/util/concurrent/ConcurrentHashMap$KeyIterator";
pub(crate) const CHM_VALUE_ITER_CLASS: &str = "java/util/concurrent/ConcurrentHashMap$ValueIterator";

/// Resolve a REAL image class, never a fabrication.
///
/// `ensure_class_initialized` fabricates a stand-in rather than failing, so an
/// `Ok` from it is not evidence the image has the class (the recurring
/// `ensure-class-initialized-fabricates-instead-of-failing` shape). Both the
/// name the id resolves BACK to and the synthetic-stub predicate are checked,
/// and either mismatch is reported as "not available" so the caller falls back
/// rather than building a carrier out of a stub.
pub(crate) fn chm_real_class(ctx: &mut dyn NativeContext, name: &str) -> Option<ClassId> {
    if ctx.is_class_synthetic_stub(name) {
        return None;
    }
    if let Some(cid) = ctx.class_id_by_name(name) {
        return Some(cid);
    }
    let cid = ctx.ensure_class_initialized(name).ok()?;
    if ctx.class_name_of_id(cid).as_deref() == Some(name) {
        Some(cid)
    } else {
        None
    }
}

/// Materialise the segmented store into a REAL `ConcurrentHashMap$Node[]` and
/// publish it in the receiver's real `table` field, returning the array.
///
/// # Why this exists
///
/// `Hashtable`'s precedent is the whole argument: its bucket array lives in the
/// receiver's own `table` field, and java.base's own `Hashtable$Enumerator`
/// walks it correctly even though the NODES are ours. CHM was the opposite —
/// measured on this binary, `--jdk-only`, JDK 25.0.3+9, for an empty map, a
/// 6-entry map and a 200-entry map alike:
///
/// ```text
///                        HotSpot 25                    CratonVM --jdk-only
///   table                len=16 used=6 $Node           null
///   baseCount / sizeCtl  6 / 12                        0 / 0
///   slot 0 (AbstractMap.keySet)  null                  Object[4] of segments
///   keys()               $KeyIterator (dual)           Collections$3 (NOT an Iterator)
/// ```
///
/// so `keys()`/`elements()` could not hand back the JDK's dual
/// `Enumeration`+`Iterator` carrier: there was nothing for a `KeyIterator` to
/// walk. That is `RJdkEnumerations`' `carriersAreTheJdksOwnDualInterface\
/// Enumerators` vector.
///
/// The feasibility question was settled by measurement BEFORE this was written
/// (`probes/ChmDrive.java`): a `Node[]` built by hand at the Java level, handed
/// to java.base's own `KeyIterator`/`ValueIterator`, iterates CORRECTLY on
/// CratonVM `--jdk-only` — both faces, one shared cursor, the right multiset.
/// `Traverser.advance()`, `tabAt()`'s `U.getReferenceAcquire` and its checkcast
/// to `$Node` all run. So the shape is walkable; only the field was missing.
///
/// # What this is and is NOT
///
/// It is a **refresh-on-read mirror**, not a move of the authority. The
/// segmented store rooted at slot 0 remains the one writable copy; every
/// mutator (`put`/`remove`/`compute*`/…) is unchanged and never runs this. The
/// mirror is rebuilt from scratch on each `keys()`/`elements()`, so `table` is
/// exactly as fresh as the most recent iteration and can never drift silently
/// behind the store the way a build-once cache would.
///
/// **Concurrency.** This adds no new sharing and takes no segment lock. It
/// reads the segments exactly as `chm_collect_all_keys` already does, so it
/// inherits that walk's existing weak consistency, and it publishes a private,
/// freshly-allocated array that no other thread can reach until the store to
/// `table` makes it visible. A concurrent mutation during or after the walk is
/// not reflected in an iterator already handed out — which is precisely what
/// `ConcurrentHashMap`'s spec permits of its weakly-consistent iterators
/// ("may or may not reflect" post-construction modifications). Two threads
/// calling `keys()` at once each build their own array; the last store to
/// `table` wins, and both iterators still hold their own. What this does NOT
/// give is atomicity of the snapshot against a concurrent writer — no more and
/// no less than the snapshot enumeration it replaces.
///
/// **It does not unblock retiring the CHM shadows.** Retirement needs `table`
/// to be the AUTHORITY that real `putVal`/`get` bytecode CASes into, which a
/// mirror is not. See `W7-96-chm-table-never-populated.md`.
///
/// # Why the fallback is total
///
/// Any surprise — no real `$Node` in the image, an over-large table, a
/// primitive in a slot a real `$Node` declares as a reference (which
/// descriptor coercion would silently turn into a DIFFERENT value on store) —
/// returns `None` and the caller keeps today's snapshot carrier verbatim. A
/// partially-materialised `table` would be a populated-looking lie, which is
/// the one outcome worse than the null it replaces.
//
// ---------------------------------------------------------------------------

/// Refresh [`chm_publish_real_table`]'s mirror for its side effect, discarding
/// the array.
///
/// # Why the O(N) read natives, and not `put`
///
/// `H23-2` §7.3 nominated *"`ConcurrentHashMap.table` is null for an ordinary
/// map"*, and MEASURED on this host it was: a two-entry `new
/// ConcurrentHashMap<>()` reported `table = null` where HotSpot 25.0.3+9
/// reports `[Ljava.util.concurrent.ConcurrentHashMap$Node; len=16`, and a
/// 40-entry one reported null against HotSpot's `len=64`. The mirror was
/// correct wherever it ran; it ran from exactly ONE caller,
/// `chm_real_dual_iterator`, i.e. only from `keys()`/`elements()`.
///
/// The mirror is a rebuild-from-scratch of the whole segmented store, so it is
/// O(N). Hanging it on `put` would make every insert O(N) and hanging it on
/// `size()` would make an O(1) query O(N) — both are worse than the defect.
/// Every caller added here is a bulk operation that ALREADY walks every entry
/// (`chm_collect_all_keys` / `_values` / `_entries` do the same walk the mirror
/// does), so the cost is a constant factor on an already-linear operation and
/// the asymptotics of no operation change.
///
/// What that buys, and what it does not: after any bulk read of a CHM its
/// `table` is a fresh, correctly-typed `$Node[]` that java.base's own
/// `Traverser` walks. A map that has been WRITTEN and never bulk-read still
/// presents `table = null` — the residual, stated rather than hidden, because
/// closing it needs `table` to be the authority a real `putVal` CASes into
/// rather than a mirror (`W7-96-chm-table-never-populated.md`), which is a
/// different change from this one.
///
/// The total-fallback contract is inherited unchanged: any surprise leaves
/// `table` exactly as it was, so a failed refresh can never be worse than not
/// refreshing.
/// # `this` is `&mut` — `WORKER-5-NOTE-12` N3 (the depth-2 pass)
///
/// `chm_publish_real_table` pins `this` for its whole body, but the pin is
/// released before it returns and the CALLER's copy is never updated — and the
/// body allocates (class resolution can LOAD a class, as its own comment says).
/// All three call sites then dereference the receiver immediately
/// (`make_key_set_view`, `chm_collect_all_values`, `chm_collect_all_entries`).
///
/// Found only at `--depth 2`: the allocation is two helper hops down, so the
/// depth-1 sweep that produced NOTE-12's baseline could not see it. That is
/// exactly the limit NOTE-12 §5 flagged, and it is why the baseline moved to
/// depth 2.
pub(crate) fn chm_refresh_real_table(ctx: &mut dyn NativeContext, this: &mut ObjectRef) {
    let pin = ctx.pin_native_root(*this);
    let _ = chm_publish_real_table(ctx, *this);
    *this = ctx.read_native_pin(pin, *this);
    ctx.unpin_native_roots(pin);
}

pub(crate) fn chm_publish_real_table(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    // `this` is pinned for the WHOLE body and every early exit unwinds through
    // the single `unpin_native_roots` below, so the many `return None` arms
    // inside cannot leak a pin. The array and key/value pins taken later are
    // all above this handle and are released by the same call.
    let this_pin = ctx.pin_native_root(this);
    let out = chm_publish_real_table_pinned(ctx, this, this_pin);
    ctx.unpin_native_roots(this_pin);
    out
}

/// [`chm_publish_real_table`]'s body, with `this` already pinned as `this_pin`
/// and the unpin left to the caller.
pub(crate) fn chm_publish_real_table_pinned(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    this_pin: usize,
) -> Option<ObjectRef> {
    // Class resolution can LOAD a class, which allocates, so this runs before
    // anything raw is read off `this` and `this` is re-read straight after.
    let node_cid = chm_real_class(ctx, CHM_NODE_CLASS)?;
    let this = ctx.read_native_pin(this_pin, this);
    let node_width = ctx.class_num_total_fields(node_cid).max(NODE_NUM_FIELDS);
    let table_slot = receiver_table_slot(ctx, this)?;

    // Snapshot the segmented store. `get_field`/`get_array_element`/
    // `array_length` never allocate, so there is no GC point in this walk and
    // the raw refs it collects are still live when it ends. Allocation starts
    // below, behind pins.
    let mut entries: Vec<(i32, Value, Value)> = Vec::new();
    for seg in chm_all_segments(ctx, this) {
        let (buckets, _size, cap) = map_state(ctx, seg);
        if let Some(b) = buckets {
            for i in 0..(cap as usize) {
                let mut node_val = ctx.get_array_element(b, i);
                while let Value::Object(Some(node)) = node_val {
                    // The JDK's `spread()` masks with `HASH_BITS`, and
                    // `Traverser.advance()` reads `e.hash < 0` as "this bin
                    // head is a Forwarding/Tree/Reservation node, not a
                    // mapping". A negative hash here would make java.base
                    // treat our plain node as a control node and walk off it.
                    let hash = match ctx.get_field(node, NODE_FIELD_HASH) {
                        Value::Int(h) => h & 0x7fff_ffff,
                        _ => return None,
                    };
                    let key = get_node_key(ctx, node);
                    let value = get_node_value(ctx, node);
                    // A value that IS the segment object is an in-flight
                    // `computeIfAbsent` reservation, not a mapping — invisible
                    // to iteration, exactly as the JDK's ReservationNode is.
                    // See `chm_collect_all_entries`.
                    let reserved = matches!(value, Value::Object(Some(v))
                        if std::ptr::eq(v.as_ptr(), seg.as_ptr()));
                    if !reserved {
                        // A real `$Node` declares `key` and `val` as
                        // references. A primitive stored into one would be
                        // descriptor-coerced on the way in and read back as a
                        // different value, so abandon rather than publish it.
                        if !matches!(key, Value::Object(Some(_)))
                            || !matches!(value, Value::Object(Some(_)))
                        {
                            return None;
                        }
                        entries.push((hash, key, value));
                    }
                    node_val = ctx.get_field(node, NODE_FIELD_NEXT);
                }
            }
        }
    }

    // HotSpot's own sizing, so the published table matches it bin-for-bin:
    // start at DEFAULT_CAPACITY and double while the count exceeds the 0.75
    // load factor. Measured against HotSpot: 6 entries -> 16, 200 -> 512.
    //
    // ...but DEFAULT_CAPACITY is only the floor for a map built by the no-arg
    // constructor. `new ConcurrentHashMap<>(16)` ends with
    // `sizeCtl = tableSizeFor(16 + (16 >>> 1) + 1) = 32`, and HotSpot's first
    // `put` allocates 32 buckets and keeps them — a 3-entry map built that way
    // has `table.length == 32`, not 16. [`chm_initial_table`] is where this
    // implementation already records that, so take it as the floor.
    //
    // TWO PRODUCERS, ONE SLOT, and this is where they reconcile. `sizeCtl` is
    // written by [`chm_record_initial_table`] as the requested TABLE SIZE and
    // by the tail of this function as the 0.75 THRESHOLD — both faithful to
    // HotSpot, which reuses the field for both meanings at different points in
    // a map's life, and indistinguishable to [`chm_initial_table`], which
    // applies `next_power_of_two` to whatever it finds. That round trip is
    // exact once the floor below is in place, and not by luck: the threshold
    // this function writes is `cap - (cap >> 2)`, i.e. `0.75 * cap`, which is
    // strictly greater than `cap / 2`, so `next_power_of_two` of it is `cap`
    // for every `cap >= 4`. The mirror therefore reports the SAME virtual table
    // size to `chm_reorder_by_virtual_bucket` after a refresh as before one,
    // and repeated refreshes are idempotent.
    //
    // MEASURED, and this is why the floor is not cosmetic: without it, a
    // refresh of the `new ConcurrentHashMap<>(16)` in `RChmKeySetView` wrote
    // `sizeCtl = 12`, `chm_initial_table` then answered 16 where the
    // constructor had recorded 32, and `keySet()` came back
    // `[youralias, thirdalias, myalias]` against HotSpot's
    // `[thirdalias, myalias, youralias]` — a real regression, caught by that
    // vector, from a mirror that only ran on `keys()` before.
    let n = entries.len();
    // Round 15 wave 5, lane compat6 ([`chm_jdk25_sizing_enabled`]): the SAME
    // table `chm_reorder_by_virtual_bucket` models, so the published array and
    // the native iteration agree. The 16 floor was wrong for a table HotSpot
    // sizes smaller (`ConcurrentHashMap(2)` -> 4, a copy of a 3-entry map ->
    // 8), and `n > threshold` kept 16 buckets at exactly 12 entries, where
    // HotSpot has already resized to 32 (`addCount`: `s >= sizeCtl`).
    let mut cap = if chm_jdk25_sizing_enabled() {
        chm_initial_table(ctx, this).max(chm_jdk_table_size_for(n))
    } else {
        chm_initial_table(ctx, this).max(16)
    };
    while n > cap - (cap >> 2) {
        if cap > (1usize << 29) {
            return None;
        }
        cap <<= 1;
    }

    // GC discipline: every collected key/value must be pinned before the first
    // allocation and re-read after each one. `this` is already pinned by the
    // caller, whose handle is the base that unwinds this whole batch.
    let flat: Vec<Value> = entries.iter().flat_map(|(_, k, v)| [*k, *v]).collect();
    let (_flat_base, flat_pins) = pin_value_slice(ctx, &flat);
    let table = ctx.try_new_ref_array(node_cid, cap)?;
    let table_pin = ctx.pin_native_root(table);

    for (i, (hash, _, _)) in entries.iter().enumerate() {
        // The only GC point in the loop. Everything raw is stale after it.
        let node = ctx.alloc_object(node_cid, node_width);
        let table = ctx.read_native_pin(table_pin, table);
        let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
        let value = read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]);
        // Read the current bin head straight out of the array rather than
        // carrying it across the allocation above.
        let idx = (*hash as usize) & (cap - 1);
        let head = ctx.get_array_element(table, idx);
        ctx.set_field(node, NODE_FIELD_HASH, Value::Int(*hash));
        ctx.set_field(node, NODE_FIELD_KEY, key);
        ctx.set_field(node, NODE_FIELD_VALUE, value);
        ctx.set_field(node, NODE_FIELD_NEXT, head);
        ctx.set_array_element(table, idx, Value::Object(Some(node)));
    }

    let this = ctx.read_native_pin(this_pin, this);
    let table = ctx.read_native_pin(table_pin, table);
    // The bookkeeping HotSpot carries alongside the array. `baseCount` is a
    // `long` and `sizeCtl` the 0.75 threshold; both are resolved by NAME, so a
    // receiver that does not declare them (a subclass with a shadowing layout)
    // simply gets the array and no bookkeeping rather than a stray store.
    ctx.set_field(this, table_slot, Value::Object(Some(table)));
    let cid = ctx.class_id_of_object(this);
    let width = ctx.object_num_fields(this);
    if let Some(slot) = ctx.resolve_field_index_by_class_id(cid, "baseCount") {
        if slot < width {
            ctx.set_field(this, slot, Value::Long(n as i64));
        }
    }
    if let Some(slot) = ctx.resolve_field_index_by_class_id(cid, "sizeCtl") {
        if slot < width {
            ctx.set_field(this, slot, Value::Int((cap - (cap >> 2)) as i32));
        }
    }
    Some(table)
}

// ===========================================================================
// ConcurrentHashMap's bulk-operation family.
//
// Every method here walks `table` through a `Traverser` in the real JDK, and a
// natively-backed CHM keeps its entries in a SEGMENTED layout that never
// populates `table` (`W7-96-chm-table-never-populated`). Unregistered, they
// therefore run real bytecode over an EMPTY tree and answer the identity
// element — which is the worst shape a wrong answer can take here, because
// every one of these methods has a perfectly ordinary-looking result for an
// empty map. MEASURED against HotSpot 25.0.4+7, both modes, on a three-entry
// map (`probes/ChmShadowSweep.java`):
//
// ```text
//   reduceKeys(1, (a,b) -> a+b)                      HotSpot "abc"  CratonVM null
//   reduceValuesToInt(1, Integer::intValue, 0, sum)  HotSpot 6      CratonVM 0
//   reduceValuesToLong(1, Integer::longValue, 0, +)  HotSpot 6      CratonVM 0
//   reduceEntriesToInt(1, e -> 1, 0, sum)            HotSpot 3      CratonVM 0
//   searchValues(1, v -> v == 2 ? "found" : null)    HotSpot found  CratonVM null
//   reduceKeys(1, String::toUpperCase, min)          HotSpot "A"    CratonVM null
// ```
//
// `reduceValues`, `searchKeys` and `search` were registered — the three the
// round-2 differential happened to ask — and answered correctly, which is why
// the family read as working. The rest is registered here as one block so the
// half-applied state cannot come back: a partly-registered family is worse
// than either endpoint, because the registered half is evidence for the
// unregistered half.
//
// PARALLELISM THRESHOLD. Every signature's leading `long` is ignored, exactly
// as `forEachEntry`/`search` already ignore it. It is documented as a hint —
// "the (estimated) number of elements needed for this operation to be executed
// in parallel" — and a sequential walk satisfies every one of these
// specifications; `Long.MAX_VALUE` (never parallel) is the value that makes
// the JDK behave identically.

/// Which of the three views a bulk operation walks.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChmBulkOver {
    Keys,
    Values,
    /// A `Map.Entry` per mapping. `AbstractMap$SimpleEntry` with the source map
    /// as its live backing, exactly what `forEachEntry` already mints and for
    /// the reason recorded there: a real `Map.Entry` so `checkcast` and
    /// reflection both work, rather than the class-less carrier that made
    /// `forEachEntry` unusable for any consumer that touched its argument.
    Entries,
}

/// Walk every mapping under the snapshot discipline the rest of this family
/// uses, calling `step` with the CURRENT receiver and the pinned key/value.
///
/// `step` answers `false` to stop early — which is what `search*` needs and
/// what makes it one walker rather than two.
pub(crate) fn chm_bulk_walk(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    step: &mut dyn FnMut(
        &mut dyn NativeContext,
        ObjectRef,
        Value,
        Value,
    ) -> Result<bool, MethodCallFailed>,
) -> Result<(), MethodCallFailed> {
    let this_pin = ctx.pin_native_root(this);
    let entries = chm_collect_all_entries(ctx, this);
    let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (_, flat_pins) = pin_value_slice(ctx, &flat);
    let out = (|| -> Result<(), MethodCallFailed> {
        for i in 0..entries.len() {
            let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
            let value = read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]);
            let source = ctx.read_native_pin(this_pin, this);
            if !step(ctx, source, key, value)? {
                break;
            }
        }
        Ok(())
    })();
    ctx.unpin_native_roots(this_pin);
    out
}

/// The element a `Keys`/`Values`/`Entries` operation sees for one mapping.
pub(crate) fn chm_bulk_element(
    ctx: &mut dyn NativeContext,
    over: ChmBulkOver,
    source: ObjectRef,
    key: Value,
    value: Value,
) -> Result<Value, MethodCallFailed> {
    Ok(match over {
        ChmBulkOver::Keys => key,
        ChmBulkOver::Values => value,
        ChmBulkOver::Entries => Value::Object(Some(alloc_live_entry(
            ctx,
            "java/util/AbstractMap$SimpleEntry",
            key,
            value,
            source,
        )?)),
    })
}

/// `func` as an object, or `None` when the slot holds a null / is absent.
pub(crate) fn chm_fn_arg(args: &[Value], index: usize) -> Option<ObjectRef> {
    match args.get(index) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

/// The bulk family's null contract.
///
/// The first cut of these registrations answered the identity element for a
/// null functional argument, on the theory that the JDK's own NPE comes from
/// dereferencing the argument inside a parallel task and a sequential walk
/// cannot reproduce that faithfully. MEASURED, and the theory was wrong:
///
/// ```text
///   chm.searchValues(1, null)   HotSpot NullPointerException   CratonVM no-throw
/// ```
///
/// The JDK checks explicitly — every one of these methods opens with
/// `if (searchFunction == null) throw new NullPointerException();` or the
/// equivalent `Objects.requireNonNull`, BEFORE it decides whether to go
/// parallel. Message-less, like the rest of this family. `reject_null_functional`
/// keeps the "a MISSING argument is a dispatch defect, not a Java-visible null"
/// distinction the rest of the file draws.
pub(crate) fn chm_reject_null_bulk_args(args: &[Value], indices: &[usize]) -> Result<(), MethodCallFailed> {
    for i in indices {
        reject_null_functional(args.get(*i))?;
    }
    Ok(())
}

/// `reduceKeys(par, reducer)` / `reduceValues(..)` / `reduceEntries(..)`, and
/// their three-argument transformer forms.
///
/// The JDK's shape, which the fold below reproduces exactly: the transformer
/// (when present) may answer null, and a null transform is SKIPPED rather than
/// folded — so a transformer that filters is the idiom, and folding its null
/// would poison the accumulator.
pub(crate) fn chm_bulk_reduce(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    over: ChmBulkOver,
    transformer: Option<ObjectRef>,
    reducer: ObjectRef,
) -> MethodCallResult {
    let reducer_pin = ctx.pin_native_root(reducer);
    let transformer_pin = transformer.map(|t| ctx.pin_native_root(t));
    // THE ACCUMULATOR LIVES IN A ONE-SLOT REFERENCE ARRAY, not in a pin taken
    // inside the walk. `chm_bulk_walk` releases its own pin base on the way
    // out, and every pin the loop body took sits ABOVE that base -- so an
    // accumulator pinned per iteration would be dangling by the time this
    // function read it back. An array slot is remapped by the collector like
    // any other heap reference, and it is pinned once, HERE, below the walk.
    // gen r4w4/rooting2: `this` is pinned across the cell allocation below and
    // re-read for the walk, which would otherwise pin a pre-move receiver.
    let this_pin = ctx.pin_native_root(this);
    let cell = alloc_ref_array(ctx, 1);
    let cell_pin = ctx.pin_native_root(cell);
    let this = ctx.read_native_pin(this_pin, this);
    let mut seen = false;
    let walk = chm_bulk_walk(ctx, this, &mut |ctx, source, key, value| {
        let elem = chm_bulk_element(ctx, over, source, key, value)?;
        let elem = match (transformer, transformer_pin) {
            (Some(t), Some(pin)) => {
                let t = ctx.read_native_pin(pin, t);
                match ctx.invoke_virtual(
                    t,
                    "apply",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                    &[elem],
                )? {
                    Some(v @ Value::Object(Some(_))) => v,
                    // A null transform is skipped, not folded.
                    _ => return Ok(true),
                }
            }
            _ => elem,
        };
        if !seen {
            let cell = ctx.read_native_pin(cell_pin, cell);
            ctx.set_array_element(cell, 0, elem);
            seen = true;
            return Ok(true);
        }
        let cell_now = ctx.read_native_pin(cell_pin, cell);
        let left = ctx.get_array_element(cell_now, 0);
        let f = ctx.read_native_pin(reducer_pin, reducer);
        let folded = ctx.invoke_virtual(
            f,
            "apply",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[left, elem],
        )?;
        let cell_now = ctx.read_native_pin(cell_pin, cell);
        ctx.set_array_element(cell_now, 0, folded.unwrap_or(Value::Object(None)));
        Ok(true)
    });
    let cell_now = ctx.read_native_pin(cell_pin, cell);
    let answer = if seen {
        ctx.get_array_element(cell_now, 0)
    } else {
        Value::Object(None)
    };
    ctx.unpin_native_roots(reducer_pin);
    walk?;
    Ok(Some(answer))
}

/// The pair-shaped `reduce(par, BiFunction transformer, BiFunction reducer)`,
/// whose transformer takes the KEY and the VALUE rather than one element.
pub(crate) fn chm_bulk_reduce_pairs(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    transformer: ObjectRef,
    reducer: ObjectRef,
) -> MethodCallResult {
    let transformer_pin = ctx.pin_native_root(transformer);
    let reducer_pin = ctx.pin_native_root(reducer);
    // See `chm_bulk_reduce` for why the accumulator is an array slot, and why
    // `this` is pinned across its allocation (gen r4w4/rooting2).
    let this_pin = ctx.pin_native_root(this);
    let cell = alloc_ref_array(ctx, 1);
    let cell_pin = ctx.pin_native_root(cell);
    let this = ctx.read_native_pin(this_pin, this);
    let mut seen = false;
    let walk = chm_bulk_walk(ctx, this, &mut |ctx, _source, key, value| {
        let t = ctx.read_native_pin(transformer_pin, transformer);
        let elem = match ctx.invoke_virtual(
            t,
            "apply",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[key, value],
        )? {
            Some(v @ Value::Object(Some(_))) => v,
            _ => return Ok(true),
        };
        if !seen {
            let cell = ctx.read_native_pin(cell_pin, cell);
            ctx.set_array_element(cell, 0, elem);
            seen = true;
            return Ok(true);
        }
        let cell_now = ctx.read_native_pin(cell_pin, cell);
        let left = ctx.get_array_element(cell_now, 0);
        let f = ctx.read_native_pin(reducer_pin, reducer);
        let folded = ctx.invoke_virtual(
            f,
            "apply",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[left, elem],
        )?;
        let cell_now = ctx.read_native_pin(cell_pin, cell);
        ctx.set_array_element(cell_now, 0, folded.unwrap_or(Value::Object(None)));
        Ok(true)
    });
    let cell_now = ctx.read_native_pin(cell_pin, cell);
    let answer = if seen {
        ctx.get_array_element(cell_now, 0)
    } else {
        Value::Object(None)
    };
    ctx.unpin_native_roots(transformer_pin);
    walk?;
    Ok(Some(answer))
}

/// The primitive width a `reduce*To{Int,Long,Double}` family member folds in.
///
/// Each carries its own method NAME and DESCRIPTOR pair because the functional
/// interfaces differ per width — `ToIntFunction.applyAsInt` against
/// `IntBinaryOperator.applyAsInt`, and so on. Getting one of those descriptors
/// wrong is a `NoSuchMethodError` at the first element, not a wrong number, so
/// they are spelled out rather than derived.
#[derive(Clone, Copy)]
pub(crate) enum ChmPrimWidth {
    Int,
    Long,
    Double,
}

impl ChmPrimWidth {
    fn transform_method(self) -> (&'static str, &'static str, &'static str) {
        match self {
            // (name, one-arg descriptor, two-arg descriptor)
            ChmPrimWidth::Int => (
                "applyAsInt",
                "(Ljava/lang/Object;)I",
                "(Ljava/lang/Object;Ljava/lang/Object;)I",
            ),
            ChmPrimWidth::Long => (
                "applyAsLong",
                "(Ljava/lang/Object;)J",
                "(Ljava/lang/Object;Ljava/lang/Object;)J",
            ),
            ChmPrimWidth::Double => (
                "applyAsDouble",
                "(Ljava/lang/Object;)D",
                "(Ljava/lang/Object;Ljava/lang/Object;)D",
            ),
        }
    }
    fn reduce_method(self) -> (&'static str, &'static str) {
        match self {
            ChmPrimWidth::Int => ("applyAsInt", "(II)I"),
            ChmPrimWidth::Long => ("applyAsLong", "(JJ)J"),
            ChmPrimWidth::Double => ("applyAsDouble", "(DD)D"),
        }
    }
    fn zero(self) -> Value {
        match self {
            ChmPrimWidth::Int => Value::Int(0),
            ChmPrimWidth::Long => Value::Long(0),
            ChmPrimWidth::Double => Value::Double(0.0),
        }
    }
}

/// `reduce{Keys,Values,Entries}To{Int,Long,Double}(par, transformer, basis,
/// reducer)`, and — with `pairs` — the un-suffixed `reduceTo*` whose
/// transformer takes the key and the value.
///
/// Unlike the object-shaped reduces there is no "skip" case and no empty
/// answer: the BASIS is the identity and an empty map answers it, which is why
/// an unregistered `reduceValuesToInt` answering `0` looked exactly like a
/// correct answer for a map that happened to sum to zero.
pub(crate) fn chm_bulk_reduce_prim(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    over: ChmBulkOver,
    pairs: bool,
    transformer: ObjectRef,
    basis: Value,
    reducer: ObjectRef,
    width: ChmPrimWidth,
) -> MethodCallResult {
    let transformer_pin = ctx.pin_native_root(transformer);
    let reducer_pin = ctx.pin_native_root(reducer);
    let mut acc = basis;
    let (tname, tdesc1, tdesc2) = width.transform_method();
    let (rname, rdesc) = width.reduce_method();
    let walk = chm_bulk_walk(ctx, this, &mut |ctx, source, key, value| {
        let t = ctx.read_native_pin(transformer_pin, transformer);
        let mapped = if pairs {
            ctx.invoke_virtual(t, tname, tdesc2, &[key, value])?
        } else {
            let elem = chm_bulk_element(ctx, over, source, key, value)?;
            let t = ctx.read_native_pin(transformer_pin, transformer);
            ctx.invoke_virtual(t, tname, tdesc1, &[elem])?
        };
        let mapped = mapped.unwrap_or_else(|| width.zero());
        let f = ctx.read_native_pin(reducer_pin, reducer);
        let folded = ctx.invoke_virtual(f, rname, rdesc, &[acc, mapped])?;
        acc = folded.unwrap_or_else(|| width.zero());
        Ok(true)
    });
    ctx.unpin_native_roots(transformer_pin);
    walk?;
    Ok(Some(acc))
}

/// `search{Keys,Values,Entries}(par, fn)` — the first NON-NULL answer wins and
/// the walk stops there, which is the whole contract: a search function is
/// allowed to have side effects and must not see the rest of the map.
pub(crate) fn chm_bulk_search(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    over: ChmBulkOver,
    func: ObjectRef,
) -> MethodCallResult {
    let func_pin = ctx.pin_native_root(func);
    // See `chm_bulk_reduce` for why the answer is parked in an array slot, and
    // why `this` is pinned across its allocation (gen r4w4/rooting2).
    let this_pin = ctx.pin_native_root(this);
    let cell = alloc_ref_array(ctx, 1);
    let cell_pin = ctx.pin_native_root(cell);
    let this = ctx.read_native_pin(this_pin, this);
    let walk = chm_bulk_walk(ctx, this, &mut |ctx, source, key, value| {
        let elem = chm_bulk_element(ctx, over, source, key, value)?;
        let f = ctx.read_native_pin(func_pin, func);
        let hit = ctx.invoke_virtual(
            f,
            "apply",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[elem],
        )?;
        if let Some(v @ Value::Object(Some(_))) = hit {
            let cell = ctx.read_native_pin(cell_pin, cell);
            ctx.set_array_element(cell, 0, v);
            return Ok(false);
        }
        Ok(true)
    });
    let cell_now = ctx.read_native_pin(cell_pin, cell);
    let answer = ctx.get_array_element(cell_now, 0);
    ctx.unpin_native_roots(func_pin);
    walk?;
    Ok(Some(answer))
}

/// `forEach{Key,Value,Entry}(par, transformer, action)` — the transformer form.
/// A null transform is SKIPPED, exactly as in the reduces.
pub(crate) fn chm_bulk_foreach_transformed(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    over: ChmBulkOver,
    pairs: bool,
    transformer: ObjectRef,
    action: ObjectRef,
) -> MethodCallResult {
    let transformer_pin = ctx.pin_native_root(transformer);
    let action_pin = ctx.pin_native_root(action);
    let walk = chm_bulk_walk(ctx, this, &mut |ctx, source, key, value| {
        let t = ctx.read_native_pin(transformer_pin, transformer);
        let mapped = if pairs {
            ctx.invoke_virtual(
                t,
                "apply",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[key, value],
            )?
        } else {
            let elem = chm_bulk_element(ctx, over, source, key, value)?;
            let t = ctx.read_native_pin(transformer_pin, transformer);
            ctx.invoke_virtual(
                t,
                "apply",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[elem],
            )?
        };
        let mapped = match mapped {
            Some(v @ Value::Object(Some(_))) => v,
            _ => return Ok(true),
        };
        let a = ctx.read_native_pin(action_pin, action);
        ctx.invoke_virtual(a, "accept", "(Ljava/lang/Object;)V", &[mapped])?;
        Ok(true)
    });
    ctx.unpin_native_roots(transformer_pin);
    walk?;
    Ok(None)
}

pub(crate) fn native_chm_elements(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Ok(Some(Value::Object(Some(make_snapshot_enumeration(
                ctx,
                &[],
            )?))))
        }
    };
    let this_pin = ctx.pin_native_root(this);
    let out = (|| -> MethodCallResult {
        let this_now = ctx.read_native_pin(this_pin, this);
        let view = match native_chm_values(ctx, &[Value::Object(Some(this_now))])? {
            Some(Value::Object(Some(v))) => v,
            _ => return Ok(None),
        };
        let view_pin = ctx.pin_native_root(view);
        let view = ctx.read_native_pin(view_pin, view);
        let built = native_al_iterator(ctx, &[Value::Object(Some(view))])?;
        Ok(match built {
            Some(Value::Object(Some(it)))
                if ctx
                    .class_name_arc_of_id(ctx.class_id_of_object(it))
                    .is_some_and(|nm| &*nm == CHM_VALUE_ITER_CLASS) =>
            {
                Some(Value::Object(Some(it)))
            }
            _ => None,
        })
    })();
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    if let Some(v) = out? {
        return Ok(Some(v));
    }
    let vals = chm_collect_all_values(ctx, this);
    Ok(Some(Value::Object(Some(make_snapshot_enumeration(
        ctx, &vals,
    )?))))
}

/// `ConcurrentHashMap.keys()` — the key-side twin of [`native_chm_elements`],
/// and it moves for the same reason: there may only be ONE producer of
/// `ConcurrentHashMap$KeyIterator`.
///
/// This body used to build a REAL cursor over the published mirror table while
/// `keySet().iterator()` built a snapshot under a different (fabricated) class.
/// That was survivable only while the two classes were different. Now that
/// `keySet().iterator()` mints the real `ConcurrentHashMap$KeyIterator` — which
/// is what HotSpot answers and what removes a Phase-1 fabrication from the
/// collection surface — a real cursor of the same class would meet the snapshot
/// natives registered on it, which is precisely how `elements()` came to hand
/// back `null` forever.
///
/// So `keys()` is `keySet().iterator()`, exactly as the JDK's is: both are a
/// `KeyIterator`, and that class implements `Enumeration` as well as
/// `Iterator`.
pub(crate) fn native_chm_keys(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Ok(Some(Value::Object(Some(make_snapshot_enumeration(
                ctx,
                &[],
            )?))))
        }
    };
    let this_pin = ctx.pin_native_root(this);
    let out = (|| -> MethodCallResult {
        let this_now = ctx.read_native_pin(this_pin, this);
        let view = match native_chm_key_set(ctx, &[Value::Object(Some(this_now))])? {
            Some(Value::Object(Some(v))) => v,
            _ => return Ok(None),
        };
        let view_pin = ctx.pin_native_root(view);
        let view = ctx.read_native_pin(view_pin, view);
        let built = native_ksv_iterator(ctx, &[Value::Object(Some(view))])?;
        Ok(match built {
            Some(Value::Object(Some(it)))
                if ctx
                    .class_name_arc_of_id(ctx.class_id_of_object(it))
                    .is_some_and(|nm| &*nm == CHM_KEY_ITER_CLASS) =>
            {
                Some(Value::Object(Some(it)))
            }
            // A stripped image with no real `KeyIterator` lands on
            // `Arrays$ArrayItr`, which is NOT an `Enumeration`; the snapshot
            // enumeration below is the honest answer there.
            _ => None,
        })
    })();
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    if let Some(v) = out? {
        return Ok(Some(v));
    }
    let keys = chm_collect_all_keys(ctx, this);
    Ok(Some(Value::Object(Some(make_snapshot_enumeration(
        ctx, &keys,
    )?))))
}

// ===========================================================================
// Collections$SetFromMap — the view returned by `Collections.newSetFromMap`.
//
// `SetFromMap` captures `s = m.keySet()` *once* in its constructor and routes
// `iterator()` / `toArray()` through that captured `s`. CratonVM's `keySet()`
// natives return an eager snapshot, so when `newSetFromMap` runs over a fresh
// (empty) map, `s` is permanently empty — every later `set.add(...)` mutates
// the live map `m` but not the stale `s`. `iterator()`/`toArray()` then come
// back empty even though `size()` (which delegates to `m.size()`) is correct.
//
// Felix's `CapabilitySet.match` returns `Collections.newSetFromMap(new
// ConcurrentHashMap())`, fills it via `addAll`, and `Felix.getServiceReferences`
// then does `new ArrayList<>(set)` — which calls `set.toArray()` and came back
// empty, so `getServiceReference("...StartLevel")` returned null and Felix's
// `AutoProcessor.processAutoProperties` NPE'd.
//
// Fix: intercept the affected `SetFromMap` view methods and route them through
// the *live* backing map `m` instead of the captured `s`.
// ===========================================================================

pub(crate) const SET_FROM_MAP_CLASS: &str = "java/util/Collections$SetFromMap";

/// Resolve the live backing `Map` field (`m`) of a `Collections$SetFromMap`.
pub(crate) fn set_from_map_backing(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    let slot = ctx.resolve_field_index(SET_FROM_MAP_CLASS, "m")?;
    match ctx.get_field(this, slot) {
        Value::Object(Some(m)) => Some(m),
        _ => None,
    }
}

pub(crate) fn native_set_from_map_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Delegate to the real backing map's `size()` via virtual dispatch, same
    // as `native_set_from_map_contains`/`_to_array`/`_iterator` below — NOT
    // through `native_map_size`, which has its own synthetic-layout
    // heuristic (`map_state`'s "is slot 0 an array" probe) that can misfire
    // on a REAL map whose own `table`/backing array field happens to land at
    // the same slot index CratonVM's synthetic HashMap uses for `buckets`.
    // For a real `LinkedCaseInsensitiveMap` (extends `LinkedHashMap`) this
    // silently read the wrong field as "size" and returned 0, breaking
    // `HeadersAdaptersTests.sizeWithMultipleValuesForHeaderShouldCountHeaders`
    // once `newSetFromMap` started returning a real `SetFromMap` for it.
    match set_from_map_backing(ctx, this) {
        Some(m) => ctx.invoke_virtual(m, "size", "()I", &[]),
        None => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_set_from_map_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let size = match native_set_from_map_size(ctx, args)? {
        Some(Value::Int(n)) => n,
        _ => 0,
    };
    Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))
}

pub(crate) fn native_set_from_map_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    // Delegate to the real backing map's `containsKey` via virtual dispatch so
    // an `IdentityHashMap` backing uses reference identity (and the correct
    // layout) rather than the synthetic-layout `native_map_contains_key`.
    match set_from_map_backing(ctx, this) {
        Some(m) => ctx.invoke_virtual(m, "containsKey", "(Ljava/lang/Object;)Z", &[key]),
        None => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_set_from_map_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Delegate to the real backing map's `keySet().toArray()` via virtual
    // dispatch (see `native_set_from_map_iterator` — `map_collect_keys` assumes
    // a synthetic map layout and mis-reads a real `IdentityHashMap`).
    let backing = match set_from_map_backing(ctx, this) {
        Some(m) => m,
        None => return Ok(Some(Value::Object(None))),
    };
    let set = match ctx.invoke_virtual(backing, "keySet", "()Ljava/util/Set;", &[])? {
        Some(Value::Object(Some(s))) => s,
        _ => return Ok(Some(Value::Object(None))),
    };
    ctx.invoke_virtual(set, "toArray", "()[Ljava/lang/Object;", &[])
}

pub(crate) fn native_set_from_map_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Route through the live backing map's *real* `keySet().iterator()` via
    // virtual dispatch so the correct per-map-type view is used. The
    // `native_map_key_set` helper assumes CratonVM's synthetic HashMap field
    // layout (buckets/size/capacity) and reads garbage from a real
    // `IdentityHashMap` (whose backing is a flat alternating key/value table).
    // Since `newSetFromMap` only returns a real `Collections$SetFromMap` for an
    // `IdentityHashMap` backing (other backings get the synthetic HashSet), the
    // virtual call dispatches to the real map's keySet, which iterates the live
    // keys correctly. The snapshot is taken now, after every add().
    let backing = match set_from_map_backing(ctx, this) {
        Some(m) => m,
        None => return Ok(Some(Value::Object(None))),
    };
    let set = match ctx.invoke_virtual(backing, "keySet", "()Ljava/util/Set;", &[])? {
        Some(Value::Object(Some(s))) => s,
        _ => return Ok(Some(Value::Object(None))),
    };
    ctx.invoke_virtual(set, "iterator", "()Ljava/util/Iterator;", &[])
}

pub(crate) fn register_set_from_map_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = SET_FROM_MAP_CLASS;
    r.register(c, "size", "()I", native_set_from_map_size);
    r.register(c, "isEmpty", "()Z", native_set_from_map_is_empty);
    r.register(
        c,
        "contains",
        "(Ljava/lang/Object;)Z",
        native_set_from_map_contains,
    );
    r.register(
        c,
        "toArray",
        "()[Ljava/lang/Object;",
        native_set_from_map_to_array,
    );
    r.register(
        c,
        "iterator",
        "()Ljava/util/Iterator;",
        native_set_from_map_iterator,
    );
    r.set_category(__prev_cat);
}

// ===========================================================================
// Properties — delegates to HashMap backing store (same 3-field layout)
// Field 0,1,2 = HashMap fields (buckets, size, capacity)
// Field 3 = defaults Properties reference (or null)
// ===========================================================================

pub(crate) const PROPS_FIELD_DEFAULTS: usize = 3;

/// Where this `Properties` receiver's `defaults` chain link actually lives.
///
/// JDK-ONLY-LAYOUT (L2, step 3): `PROPS_FIELD_DEFAULTS` (3) is a slot of the
/// fabricated four-field `Properties` model. On a real
/// `java.util.Properties` the inherited `Hashtable` fields occupy 0..=7 and
/// absolute slot 3 is `float loadFactor`, so `native_props_init`'s
/// `Object(None)` landed on a primitive — and the overlay hunter did NOT
/// report it, because its predicate only flagged `Object(Some(_))` over a
/// primitive descriptor. It was invisible in the census for that reason, not
/// absent from the defect. **Closed 2026-08-05 as L4 gap 3**:
/// `overlay_access_is_cross_type` now flags `Object(_)`, so a write of this
/// exact shape appears in the census, and the shadow-layout diff reports the
/// slot as a `TYPE` disagreement whether or not anything writes it.
///
/// Reading the same slot back was the other half: `getProperty`'s fallback
/// walk read slot 3, got a `Float`, and stopped — so on a real layout a
/// `Properties(defaults)` chain never resolved a single inherited key, even
/// though `native_props_init_defaults` had already learned to mirror the
/// reference into the real `defaults` field by name.
///
/// The real field is resolved on the RECEIVER's class (a `Properties`
/// subclass — `java.security.Provider`, Ant's `PropertySet` — inherits it),
/// and only a receiver that does not declare `defaults` at all, i.e. the
/// fabricated stub, falls back to the raw model slot.
pub(crate) fn props_defaults_slot(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    let class_id = ctx.class_id_of_object(this);
    if let Some(idx) = ctx.resolve_field_index_by_class_id(class_id, "defaults") {
        if idx < ctx.object_num_fields(this) {
            return idx;
        }
    }
    PROPS_FIELD_DEFAULTS
}

/// Read the `defaults` link of a `Properties`-shaped receiver.
pub(crate) fn props_get_defaults(ctx: &dyn NativeContext, this: ObjectRef) -> Value {
    ctx.get_field(this, props_defaults_slot(ctx, this))
}

/// Write the `defaults` link of a `Properties`-shaped receiver.
pub(crate) fn props_set_defaults(ctx: &mut dyn NativeContext, this: ObjectRef, value: Value) {
    let slot = props_defaults_slot(ctx, this);
    ctx.set_field(this, slot, value);
}

// JDK-ONLY-CLASSIFY: unknown — needs census. HIGHEST-RISK GROUP IN THIS CRATE.
// All 43 registrations target `java.util.Properties` methods that have concrete
// bytecode in JDK 25 and none that is ACC_NATIVE, which reads as "stub" on the
// static evidence alone. Do not act on that reading. `Properties` is on the
// real-JDK bootstrap path (`java.home` and friends are read out of it before
// most of the class library is usable), and this repo has already shipped a
// bootstrap failure — `InternalError: null property: java.home` — caused by a
// whole function's worth of `Properties` registrations picking up the wrong
// ambient category at a single call site. Anything that changes what survives
// here must be proven with a full `--jdk-only` boot, not with `javap`.
// Evidence needed before any retag: schema-v2 `invocations` for every triple in
// this group across a real-JDK boot, plus `overwrote` to see which of these are
// already being replaced by `native-builtins`' `register_properties_sidetable`.
pub(crate) fn register_properties_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    let p = "java/util/Properties";

    // <init>()V — empty properties
    registry.register(p, "<init>", "()V", native_props_init);

    // <init>(Properties)V — with defaults
    registry.register(
        p,
        "<init>",
        "(Ljava/util/Properties;)V",
        native_props_init_defaults,
    );

    // setProperty(String, String) -> String (old value)
    registry.register(
        p,
        "setProperty",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Object;",
        native_map_put_entry,
    );

    // getProperty(String) -> String
    registry.register(
        p,
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_props_get_property,
    );

    // getProperty(String, String) -> String (with default value)
    registry.register(
        p,
        "getProperty",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        native_props_get_property_default,
    );

    // size() -> int
    registry.register(p, "size", "()I", native_map_size);

    // isEmpty() -> boolean
    registry.register(p, "isEmpty", "()Z", native_map_is_empty);

    // containsKey(Object) -> boolean
    registry.register(
        p,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_map_contains_key,
    );

    // containsValue(Object) -> boolean
    registry.register(
        p,
        "containsValue",
        "(Ljava/lang/Object;)Z",
        native_map_contains_value,
    );

    // put(Object, Object) -> Object
    registry.register(
        p,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_put_entry,
    );

    // get(Object) -> Object
    registry.register(
        p,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_get,
    );

    // remove(Object) -> Object
    registry.register(
        p,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_remove,
    );

    // clear()
    registry.register(p, "clear", "()V", native_map_clear);

    // keySet() -> Set
    registry.register(p, "keySet", "()Ljava/util/Set;", native_map_key_set);

    // values() -> Collection
    registry.register(p, "values", "()Ljava/util/Collection;", native_map_values);

    // entrySet() -> Set
    registry.register(p, "entrySet", "()Ljava/util/Set;", native_map_entry_set);

    // toString()
    registry.register(p, "toString", "()Ljava/lang/String;", native_map_to_string);

    // putAll(Map)
    registry.register(p, "putAll", "(Ljava/util/Map;)V", native_map_put_all);

    // load(InputStream) — parse key=value lines
    registry.register(p, "load", "(Ljava/io/InputStream;)V", native_props_load);

    // load(Reader) — parse key=value lines
    registry.register(p, "load", "(Ljava/io/Reader;)V", native_props_load);

    // store(OutputStream, String) — write key=value lines
    registry.register(
        p,
        "store",
        "(Ljava/io/OutputStream;Ljava/lang/String;)V",
        native_props_store,
    );

    // propertyNames() -> Enumeration (returns keys as ArrayList iterator)
    registry.register(
        p,
        "propertyNames",
        "()Ljava/util/Enumeration;",
        native_props_property_names,
    );

    // stringPropertyNames() -> Set<String>
    registry.register(
        p,
        "stringPropertyNames",
        "()Ljava/util/Set;",
        native_props_string_property_names,
    );

    // getOrDefault(Object, Object) -> Object
    registry.register(
        p,
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_get_or_default,
    );

    // forEach(BiConsumer)
    registry.register(
        p,
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        native_map_for_each,
    );

    // hashCode
    registry.register(p, "hashCode", "()I", native_map_hash_code);

    // equals
    registry.register(p, "equals", "(Ljava/lang/Object;)Z", native_map_equals);

    // Also register under Hashtable (Properties extends Hashtable)
    let ht = "java/util/Hashtable";
    registry.register(ht, "<init>", "()V", native_map_init);
    registry.register(ht, "<init>", "(I)V", native_hashtable_init_capacity);
    registry.register(ht, "<init>", "(IF)V", native_hashtable_init_capacity_load);
    registry.register(
        ht,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_put_entry,
    );
    registry.register(
        ht,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_get,
    );
    registry.register(
        ht,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_remove,
    );
    registry.register(ht, "size", "()I", native_map_size);
    registry.register(ht, "isEmpty", "()Z", native_map_is_empty);
    registry.register(
        ht,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_map_contains_key,
    );
    registry.register(
        ht,
        "containsValue",
        "(Ljava/lang/Object;)Z",
        native_map_contains_value,
    );
    registry.register(ht, "clear", "()V", native_map_clear);
    registry.register(ht, "keySet", "()Ljava/util/Set;", native_map_key_set);
    registry.register(ht, "values", "()Ljava/util/Collection;", native_map_values);
    registry.register(ht, "entrySet", "()Ljava/util/Set;", native_map_entry_set);
    registry.register(ht, "toString", "()Ljava/lang/String;", native_map_to_string);
    registry.register(ht, "putAll", "(Ljava/util/Map;)V", native_map_put_all);
    // `Hashtable` (and `Properties`, which overrides them again) declare their
    // own bucket-walking `remove(k,v)` / `replace(k,v)` / `replace(k,old,new)`.
    // Same gap, same fix — see `register_map_conditional_mutators`. The null
    // rejections those overrides open with are honoured by
    // `map_remove_kv_null_contract` / `map_replace_kv_null_contract` /
    // `map_replace3_null_contract`, which route `Hashtable` and `Properties`
    // differently because the JDK does.
    register_map_conditional_mutators(registry, ht);
    register_map_conditional_mutators(registry, p);
    // Note: `keys()` / `elements()` are registered by
    // `cratonvm-native-builtins::deprecated_io_util::register_*_natives`,
    // which runs AFTER us in `vm_init`'s real-JDK arm. The force-native
    // override in `interpreter.rs::force_native_over_real_jdk_bytecode`
    // routes dispatch to that registration instead of the real-JDK
    // Hashtable.keys() body (which walks Hashtable's own internal
    // `table[]` — empty in our impl because `put` writes to a side-store).
    // Without the force-native entry, BC's
    // `AbstractX500NameStyle.copyHashTable(BCStyle.DefaultLookUp)` produced
    // an empty per-instance `defaultLookUp` and every
    // `BCStyle.attrNameToOID("cn"/...)` threw "Unknown object id".
    registry.set_category(__prev_cat);
}

/// The `Hashtable`-inherited half of both `Properties` constructors: allocate
/// NOTHING, which is what HotSpot's own `Properties()` does.
///
/// # What used to happen, and what it cost
///
/// Both constructors called `map_init_eager`. `Properties` is deliberately
/// EXCLUDED from `CF_HASHTABLE_LAYOUT` (JDK 25 backs it with a side
/// `ConcurrentHashMap`, not with `Hashtable`'s buckets), so that call took the
/// GENERIC arm of [`map_init_inner`] and allocated an `Object[16]` into the
/// receiver's `table`. On a real-layout `Properties` that is 144 bytes of the
/// 224 an empty one retained, against HotSpot's 120.3.
///
/// # Why null is safe here, which is a MEASUREMENT and not an argument
///
/// The comment this replaces named one risk — "leaving it null would make
/// `map_state` report no buckets on a receiver whose native put path does not
/// go through `map_resize`" — and the risk is real in the abstract. It does not
/// apply to this class, on three independent counts:
///
///   * **Nothing writes that array.** `register_properties_sidetable`
///     (`native-builtins`) runs AFTER `register_collections_natives` in BOTH
///     `vm_init` arms and OVERWRITES every Map method on `java/util/Properties`
///     — `put`, `get`, `remove`, `clear`, `size`, `isEmpty`, `containsKey`,
///     `keySet`, `values`, `entrySet`, `keys`, `elements`, `contains`. Not one
///     of those bodies reads or writes a bucket table: a String->String pair
///     goes to the Rust side-table, anything else to the real `map` CHM, which
///     `put_non_string_into_chm` CREATES ON DEMAND. `map_carrier_class_for_receiver`
///     already records the consequence, measured: a `Properties` bucket table
///     reports `occupied=0` with `size=2`.
///   * **The one path that COULD reach the buckets already handles null.**
///     `native_map_put_evict_pinned` opens with
///     `if initial_buckets.is_none() || size + 1 > (cap * 3) / 4 { map_resize(..) }`
///     — the branch a JDK-bytecode-constructed `LinkedHashMap` has always taken
///     — so even a receiver that somehow reached the generic put would
///     materialise its table on first insert rather than drop the write.
///     `map_state`'s capacity fallback reads `threshold` when there is no
///     table, and the lazy arm sets it to 0, so that path answers
///     `MAP_DEFAULT_CAPACITY` — the same 16 the eager array had.
///   * **The busiest `Properties` in the VM has ALWAYS had a null table.**
///     `system_properties_object` allocates the `System.getProperties()`
///     singleton and writes NONE of its slots; `java.home` and every other
///     bootstrap property has been read out of a bucket-less `Properties`
///     since that factory was written. This change makes every other
///     `Properties` the same shape as the one that was already proving it.
///
/// HotSpot agrees, and that is the oracle rather than the reasoning above:
/// `probes/CollectionShapeCause.java`'s field dump on a real JDK 25 shows
/// `new Properties()` leaving `table` null, `loadFactor` 0.0 and `threshold` 0,
/// with the entries in a `ConcurrentHashMap map`.
pub(crate) fn props_init_map_half(ctx: &mut dyn NativeContext, this: ObjectRef) -> MethodCallResult {
    native_map_init(ctx, &[Value::Object(Some(this))])
}

pub(crate) fn native_props_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // LAZY, like the JDK. See [`props_init_map_half`] for why the receiver's
    // bucket table stays null.
    props_init_map_half(ctx, this)?;
    props_set_defaults(ctx, this, Value::Object(None));
    Ok(None)
}

pub(crate) fn native_props_init_defaults(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // LAZY, like the JDK. See [`props_init_map_half`] for why the receiver's
    // bucket table stays null.
    props_init_map_half(ctx, this)?;
    // Store the defaults reference in the REAL `defaults` field, resolved by
    // name on the receiver's class. On a real-layout `java.util.Properties` the
    // inherited Hashtable fields push `defaults` well past this native model's
    // `PROPS_FIELD_DEFAULTS` (3) — which there lands on `loadFactor`, a float —
    // so the raw slot both misplaces the reference and destroys a live field.
    // The side-table `getProperty` reads the by-name `defaults` slot to walk
    // the fallback chain (TC0622 follow-up).
    //
    // JDK-ONLY-LAYOUT (L2, step 3): this used to write BOTH slots, so a real
    // receiver still took the destructive write; `props_set_defaults` writes
    // the one slot that is right for this receiver, and every reader now
    // resolves the same way.
    props_set_defaults(ctx, this, args[1]);
    Ok(None)
}

/// `(oval instanceof String) ? (String)oval : null` — JDK 25
/// `Properties.getProperty(String)`'s own type test. A `Properties`/`Hashtable`
/// slot is `Object`-typed, so `p.put("k", someInteger)` is legal and MEASURED
/// (W8-E29-1) to answer that `Integer` back out of a `()Ljava/lang/String;`
/// method unless this filter runs before every value is handed to the caller.
pub(crate) fn props_value_is_string(ctx: &dyn NativeContext, o: ObjectRef) -> bool {
    ctx.class_name_of_id(ctx.class_id_of_object(o)).as_deref() == Some("java/lang/String")
}

pub(crate) fn native_props_get_property(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // First check this map
    let result = native_map_get(ctx, args)?;
    if let Some(Value::Object(Some(o))) = result {
        if props_value_is_string(ctx, o) {
            return Ok(result);
        }
    }
    // Fall through to defaults chain
    let mut defaults_val = props_get_defaults(ctx, this);
    while let Value::Object(Some(defs)) = defaults_val {
        let def_result = native_map_get(ctx, &[Value::Object(Some(defs)), args[1]])?;
        if let Some(Value::Object(Some(o))) = def_result {
            if props_value_is_string(ctx, o) {
                return Ok(def_result);
            }
        }
        defaults_val = props_get_defaults(ctx, defs);
    }
    Ok(Some(Value::Object(None)))
}

pub(crate) fn native_props_get_property_default(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(args[2])),
    };
    // First check this map
    let result = native_map_get(ctx, &[args[0], args[1]])?;
    if let Some(Value::Object(Some(o))) = result {
        if props_value_is_string(ctx, o) {
            return Ok(result);
        }
    }
    // Fall through to defaults chain
    let mut defaults_val = props_get_defaults(ctx, this);
    while let Value::Object(Some(defs)) = defaults_val {
        let def_result = native_map_get(ctx, &[Value::Object(Some(defs)), args[1]])?;
        if let Some(Value::Object(Some(o))) = def_result {
            if props_value_is_string(ctx, o) {
                return Ok(def_result);
            }
        }
        defaults_val = props_get_defaults(ctx, defs);
    }
    // Return the default value arg
    Ok(Some(args[2]))
}

/// Load properties from a stream-like input: parse "key=value" or "key:value" lines.
/// Simplified: reads all bytes from the stream arg (field 0 = data for BAIS),
/// then parses lines.
pub(crate) fn native_props_load(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let stream = match args[1] {
        Value::Object(Some(s)) => s,
        _ => return Ok(None),
    };

    // Try to read the stream contents: if it's a BAIS, read data directly
    // Otherwise try to read it as a string field
    let text = props_read_input(ctx, stream);

    // MED fix: implement the JDK `Properties.load` spec (close to it,
    // anyway):
    //   - lines starting with `#` or `!` (after leading whitespace) are
    //     comments and skipped entirely;
    //   - blank lines (whitespace only) are skipped;
    //   - leading whitespace before a key is trimmed;
    //   - a line that ends with an *unescaped* trailing `\` continues
    //     onto the next line (the `\` is dropped and leading whitespace
    //     on the continuation line is trimmed);
    //   - the key terminates at the first unescaped whitespace, `=`, or
    //     `:`; subsequent whitespace and one optional `=`/`:` are then
    //     consumed; the rest of the logical line is the value;
    //   - inside both key and value the JDK escapes are honoured:
    //       \t \n \r \f \\ \" \'  → the corresponding ASCII char,
    //       \= \: \space          → the literal char (allows them in keys),
    //       \uXXXX               → the BMP codepoint XXXX,
    //       \<anything else>      → the trailing char itself (per JDK).
    //
    // This is intentionally implemented in pure Rust against the in-memory
    // text rather than dispatching back to JDK Properties for parity —
    // the previous implementation silently truncated/corrupted any
    // real-world `.properties` file with escapes or continuations.
    let this_pin = ctx.pin_native_root(this);
    for (key, value) in props_parse_logical_lines(&text) {
        let this = ctx.read_native_pin(this_pin, this);
        if key.is_empty() {
            continue;
        }
        let key_obj = ctx.create_string(&key);
        let val_obj = ctx.create_string(&value);
        native_map_put(
            ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(key_obj)),
                Value::Object(Some(val_obj)),
            ],
        )?;
    }
    Ok(None)
}

/// Parse a `.properties` text into `(key, value)` pairs per the JDK
/// `Properties.load` spec — handles comments, blank lines, leading-
/// whitespace trim, trailing-backslash continuations, the
/// `\t\n\r\f\\\"\'\=\:` and `\uXXXX` escape sequences in both keys and
/// values, and the natural key/value separator (the first unescaped
/// whitespace, `=`, or `:`).
pub(crate) fn props_parse_logical_lines(text: &str) -> Vec<(String, String)> {
    let raw_lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while i < raw_lines.len() {
        // Strip a trailing `\r` (CRLF inputs) without losing escapes.
        let mut line = raw_lines[i].trim_end_matches('\r').to_string();
        // Trim leading whitespace per spec.
        let trimmed_start = line.trim_start();
        let leading_skipped = line.len() - trimmed_start.len();
        // Comment / blank — skipped entirely (continuations do not apply
        // to comment lines).
        if trimmed_start.is_empty()
            || trimmed_start.starts_with('#')
            || trimmed_start.starts_with('!')
        {
            i += 1;
            continue;
        }
        line.drain(..leading_skipped);

        // Apply continuation: if the line ends with an *odd* number of
        // trailing backslashes, the last `\` is a continuation marker
        // and the following physical line is appended (after trimming
        // its leading whitespace).
        while ends_with_unescaped_backslash(&line) {
            line.pop(); // remove the trailing backslash
            i += 1;
            if i >= raw_lines.len() {
                break;
            }
            let next = raw_lines[i].trim_end_matches('\r');
            line.push_str(next.trim_start());
        }
        i += 1;

        // Walk the logical line to find the key/value boundary. The key
        // ends at the first unescaped whitespace, `=`, or `:`.
        let bytes = line.as_bytes();
        let mut idx = 0;
        let mut key_buf = String::new();
        while idx < bytes.len() {
            let b = bytes[idx];
            if b == b'\\' {
                // Pull one escape into the key.
                let (decoded, consumed) = decode_escape(&bytes[idx..]);
                key_buf.push_str(&decoded);
                idx += consumed;
                continue;
            }
            if b == b' ' || b == b'\t' || b == b'\x0c' || b == b'=' || b == b':' {
                break;
            }
            // Multi-byte UTF-8: copy through.
            let ch_end = utf8_char_end(bytes, idx);
            key_buf.push_str(std::str::from_utf8(&bytes[idx..ch_end]).unwrap_or(""));
            idx = ch_end;
        }

        // Skip whitespace, then at most one `=` or `:`, then more whitespace.
        while idx < bytes.len() && matches!(bytes[idx], b' ' | b'\t' | b'\x0c') {
            idx += 1;
        }
        if idx < bytes.len() && (bytes[idx] == b'=' || bytes[idx] == b':') {
            idx += 1;
            while idx < bytes.len() && matches!(bytes[idx], b' ' | b'\t' | b'\x0c') {
                idx += 1;
            }
        }

        // Remainder is the value (with escapes decoded).
        let mut val_buf = String::new();
        while idx < bytes.len() {
            let b = bytes[idx];
            if b == b'\\' {
                let (decoded, consumed) = decode_escape(&bytes[idx..]);
                val_buf.push_str(&decoded);
                idx += consumed;
                continue;
            }
            let ch_end = utf8_char_end(bytes, idx);
            val_buf.push_str(std::str::from_utf8(&bytes[idx..ch_end]).unwrap_or(""));
            idx = ch_end;
        }

        out.push((key_buf, val_buf));
    }
    out
}

/// True when the line ends with a backslash that is *not* itself
/// escaped (i.e. odd run of trailing backslashes).
pub(crate) fn ends_with_unescaped_backslash(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut n = 0usize;
    for &b in bytes.iter().rev() {
        if b == b'\\' {
            n += 1;
        } else {
            break;
        }
    }
    n % 2 == 1
}

/// Decode a single backslash-escape starting at `bytes[0] == b'\\'`.
/// Returns `(decoded_string, bytes_consumed_including_backslash)`.
/// Falls back to the literal trailing char for unknown escapes (matches
/// JDK behaviour: `\q` parses as `q`).
pub(crate) fn decode_escape(bytes: &[u8]) -> (String, usize) {
    debug_assert_eq!(bytes[0], b'\\');
    if bytes.len() < 2 {
        return (String::new(), 1);
    }
    match bytes[1] {
        b't' => ("\t".to_string(), 2),
        b'n' => ("\n".to_string(), 2),
        b'r' => ("\r".to_string(), 2),
        b'f' => ("\x0c".to_string(), 2),
        b'\\' => ("\\".to_string(), 2),
        b'"' => ("\"".to_string(), 2),
        b'\'' => ("'".to_string(), 2),
        b'u' => {
            // \uXXXX (exactly 4 hex digits per JDK spec).
            if bytes.len() < 6 {
                return ((bytes[1] as char).to_string(), 2);
            }
            let hex = std::str::from_utf8(&bytes[2..6]).unwrap_or("");
            match u32::from_str_radix(hex, 16) {
                Ok(cp) => match char::from_u32(cp) {
                    Some(ch) => (ch.to_string(), 6),
                    // Surrogate halves and other invalid code points: fall
                    // through to literal 'u' so we never lose data.
                    None => ("u".to_string(), 2),
                },
                Err(_) => ("u".to_string(), 2),
            }
        }
        // Per spec: any other char after `\` (including space, `=`, `:`,
        // and unknown letters) becomes itself, allowing them inside keys.
        other => {
            // Multi-byte UTF-8 starting at byte index 1.
            let end = utf8_char_end(bytes, 1);
            let s = std::str::from_utf8(&bytes[1..end])
                .map(|s| s.to_string())
                .unwrap_or_else(|_| (other as char).to_string());
            (s, end)
        }
    }
}

/// Return the byte-index just past the UTF-8 char that begins at `start`,
/// or `start + 1` for invalid sequences (safe fallback).
pub(crate) fn utf8_char_end(bytes: &[u8], start: usize) -> usize {
    if start >= bytes.len() {
        return start;
    }
    let b = bytes[start];
    let len = if b < 0x80 {
        1
    } else if b < 0xC0 {
        1 // continuation byte — should not start a char; treat as 1
    } else if b < 0xE0 {
        2
    } else if b < 0xF0 {
        3
    } else {
        4
    };
    (start + len).min(bytes.len())
}

/// Helper: read all text from a stream or reader object.
pub(crate) fn props_read_input(ctx: &mut dyn NativeContext, stream: ObjectRef) -> String {
    // Try BAIS protocol: field 0 = byte[], field 1 = pos, field 2 = mark, field 3 = count
    if let Value::Object(Some(data_arr)) = ctx.get_field(stream, 0) {
        let pos = match ctx.get_field(stream, 1) {
            Value::Int(p) => p as usize,
            _ => 0,
        };
        let count = match ctx.get_field(stream, 3) {
            Value::Int(c) => c as usize,
            _ => 0,
        };
        let mut bytes = Vec::with_capacity(count.saturating_sub(pos));
        for i in pos..count {
            if let Value::Int(b) = ctx.get_array_element(data_arr, i) {
                bytes.push(b as u8);
            }
        }
        return String::from_utf8_lossy(&bytes).into_owned();
    }
    // Fallback: try to read field 0 as a string directly (e.g. BufferedReader)
    if let Some(s) = ctx.read_string(stream) {
        return s;
    }
    String::new()
}

/// Store properties to an output stream as "key=value" lines.
pub(crate) fn native_props_store(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let mut output = String::new();

    // Optional comment header — mirror JDK `Properties.writeComments`: the
    // comment is prefixed with a bare `#` (no space), and an embedded newline
    // starts a fresh `#` comment line. The previous `# {}` form inserted a
    // spurious space; more importantly the entry loop below was broken (see
    // next comment), so a save/load round-trip such as H2's `FileLock`
    // watchdog (`AUTO_SERVER=TRUE`) read back an empty file and aborted with
    // "Concurrent update".
    if let Value::Object(Some(comment)) = args[2] {
        if let Some(c) = ctx.read_string(comment) {
            output.push('#');
            for ch in c.chars() {
                match ch {
                    '\n' | '\r' => {
                        output.push('\n');
                        output.push('#');
                    }
                    _ => output.push(ch),
                }
            }
            output.push('\n');
        }
    }

    // Entries. Use the canonical, layout-aware collector that already backs
    // `entrySet()`/`toString()` (`map_collect_entries`) rather than the old
    // slot-0-only `props_collect_keys` + per-key `get`. `props_collect_keys`
    // read field 0 of each bucket node as the key — correct for legacy nodes
    // (key=0) but NOT for real-JDK-created nodes (hash=0, key=1), where field
    // 0 is the primitive `int` hash. So every JDK-layout entry was silently
    // dropped and `store` wrote only the comment. `map_collect_entries`
    // resolves the real `this.map` ConcurrentHashMap backing / CHM/LHM
    // receivers / slot-0 buckets and uses the layout-aware node accessors.
    // Keys and values are escaped per JDK `Properties.saveConvert` so the
    // output round-trips through our (faithful) `Properties.load`.
    for (k, v) in map_collect_entries(ctx, this) {
        let key_str = match k {
            Value::Object(Some(o)) => match ctx.read_string(o) {
                Some(s) => s,
                None => continue,
            },
            _ => continue,
        };
        let val_str = match v {
            Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
            _ => String::new(),
        };
        output.push_str(&props_save_convert(&key_str, true));
        output.push('=');
        output.push_str(&props_save_convert(&val_str, false));
        output.push('\n');
    }

    // Write to output stream.
    //
    // MED fix: the previous implementation issued one `write(I)V` virtual
    // dispatch per byte — for a Properties with ~1000 entries this meant
    // ~50 000 virtual calls. Build a single `byte[]` once and call
    // `write([B)V` exactly once per line, which most OutputStream
    // implementations override to do a single bulk copy. We dispatch
    // line-by-line rather than the entire buffer in one go so a
    // pathological output stream (e.g. one that requires a flush between
    // lines) still gets reasonable behaviour, and so each line's bytes
    // fit easily into a single allocation.
    if let Value::Object(Some(ostream)) = args[1] {
        // gc-common w20-a: every line allocates its byte[] and runs `write`
        // (Java); the stream was a bare copy of the argument, dispatched on at
        // its entry address for every line. Pinned for the loop.
        let ostream_pin = ctx.pin_native_root(ostream);
        for line in output.split_inclusive('\n') {
            let bytes = line.as_bytes();
            let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
            for (i, &b) in bytes.iter().enumerate() {
                // Byte array slots hold sign-extended Int per the VM's
                // primitive-array value model.
                ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
            }
            // Prefer the bulk `write([B)V` overload. If a stream does
            // not implement it, the JDK default (AbstractOutputStream)
            // falls back to per-byte `write(I)V`, so we keep behaviour
            // even on minimal stream impls.
            // Read back AFTER the byte[]'s allocation, just before the call.
            let ostream = ctx.read_native_pin(ostream_pin, ostream);
            let _ = ctx.invoke_virtual(ostream, "write", "([B)V", &[Value::Object(Some(arr))]);
        }
        ctx.unpin_native_roots(ostream_pin);
    }
    Ok(None)
}

/// Collect all key ObjectRefs from a Properties' backing store.
///
/// Delegates to the canonical `map_collect_keys` (which resolves the real
/// `this.map` ConcurrentHashMap backing, CHM/LHM receivers, and slot-0
/// buckets, all with the layout-aware node accessors) and keeps only the
/// object-reference keys. The previous body read field 0 of each bucket node
/// directly, which is the key only for legacy nodes (key=0); for real-JDK
/// nodes (hash=0, key=1) field 0 is the primitive hash, so keys were dropped
/// — the same defect that left `Properties.store` writing zero entries.
pub(crate) fn props_collect_keys(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<ObjectRef> {
    map_collect_keys(ctx, this)
        .into_iter()
        .filter_map(|v| match v {
            Value::Object(Some(o)) => Some(o),
            _ => None,
        })
        .collect()
}

/// Escape a key or value for `Properties.store` output, mirroring JDK
/// `Properties.saveConvert` for the `store(OutputStream, …)` path (i.e. with
/// Unicode escaping enabled). Keys pass `escape_space = true` (every space is
/// escaped); values pass `false` (only a leading space is escaped). The
/// metacharacters `\ \t \n \r \f = : # !` are always escaped, and any
/// control / non-ASCII character is emitted as `\\uXXXX` UTF-16 unit(s), so
/// the result parses back to the original via `props_parse_logical_lines`.
pub(crate) fn props_save_convert(s: &str, escape_space: bool) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for (i, ch) in s.chars().enumerate() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{000C}' => out.push_str("\\f"),
            ' ' => {
                if i == 0 || escape_space {
                    out.push('\\');
                }
                out.push(' ');
            }
            '=' | ':' | '#' | '!' => {
                out.push('\\');
                out.push(ch);
            }
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04X}", unit));
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// propertyNames() -> returns a real Enumeration<Object> over the property keys.
///
/// Previously this delegated to `keySet`, which returns a `HashSet`. HashSet
/// doesn't implement `Enumeration`, so callers like Kafka's
/// `Utils.propsToMap` that invoke `hasMoreElements()` / `nextElement()` on
/// the result fell through to the snapshot-iterator native's generic-Iterator
/// fallback, which couldn't find a `hasNext` method on HashSet and silently
/// returned 0 — yielding an empty enumeration and a Properties-derived map
/// with zero entries. Kafka then reported `Missing required configuration
/// "process.roles"` even though server.properties listed it.
///
/// Now we allocate the same 2-field snapshot object the Enumeration interface
/// natives expect (field 0 = `Object[]` snapshot, field 1 = `int` cursor).
pub(crate) fn native_props_property_names(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // JDK 25 `Properties` stores entries in a private `ConcurrentHashMap map`
    // field, NOT in the inherited `Hashtable.table` array. Real-JDK
    // `Properties.size()` / `propertyNames()` / `entrySet()` bytecode all
    // route through `this.map`. Walk that map when present; fall back to the
    // legacy slot-0 backing for synthetic Properties allocations.
    let backing = match ctx.resolve_field_index("java/util/Properties", "map") {
        Some(slot) if slot < ctx.object_num_fields(this) => match ctx.get_field(this, slot) {
            Value::Object(Some(m)) => Some(m),
            _ => None,
        },
        _ => None,
    };
    let keys = match backing {
        Some(m) => {
            // `Properties.map` is declared as `ConcurrentHashMap`. Use the
            // CHM-segmented key collector — `map_collect_keys` walks slot 0
            // as a plain Node[] and only finds the head segment objects on
            // a real-JDK CHM, yielding the wrong key type.
            let cid = ctx.class_id_of_object(m);
            let cn = ctx.class_name_of_id(cid).unwrap_or_default();
            if cn == "java/util/concurrent/ConcurrentHashMap"
                || cn.starts_with("java/util/concurrent/ConcurrentHashMap$")
            {
                chm_collect_all_keys(ctx, m)
            } else {
                map_collect_keys(ctx, m)
            }
        }
        None => map_collect_keys(ctx, this),
    };
    // Use our internal SnapshotEnumeration synthetic class so the
    // snapshot-iterator natives (`hasMoreElements`/`nextElement` registered on
    // this class name) win virtual dispatch. Field 0 = Object[] snapshot,
    // field 1 = cursor. Built through the pin-hardened helper — the
    // array/enumeration allocations can move the just-collected keys before
    // the raw stores (cceres5, see make_static_entry_set).
    let en = make_snapshot_enumeration(ctx, &keys)?;
    Ok(Some(Value::Object(Some(en))))
}

/// stringPropertyNames() -> Set<String>
///
/// # A FRESH set, not the receiver's own `keySet()` view
///
/// This used to take `native_map_key_set(this)` and then `add` each of the
/// defaults' keys to it. `native_map_key_set` returns a CACHED LIVE VIEW --
/// `cached_live_view(.., VIEW_KIND_KEYSET)`, the same instance `keySet()` hands
/// out, which is why `map.keySet() == map.keySet()` holds here as it does on
/// HotSpot. Adding to it therefore did two wrong things at once:
///
/// * it merged the DEFAULTS' keys into the view the receiver returns from
///   `keySet()`, which must not walk the chain -- `size()` and `keySet()`
///   deliberately see only this map's own entries, and only
///   `stringPropertyNames()` walks;
/// * it mutated a set the caller may already be holding from an earlier call.
///
/// In synthetic-JDK mode it did not even get that far: `native_hs_add` on a
/// view carrier raises `UnsupportedOperationException`, so
/// `stringPropertyNames()` threw outright on any `Properties` with a defaults
/// chain. That is what `probes/CollectionSlotFloor.java`'s
/// `Properties chain names` section reports.
///
/// Building one fresh set from the union fixes all three. `make_hashset_with_elements`
/// does the de-duplication (by `hashCode`/`equals`, so an overridden key in the
/// child shadows the parent's exactly once) and gives the set the real
/// `HashSet` -> `HashMap` layout that JDK bytecode reads through.
pub(crate) fn native_props_string_property_names(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Collect first, allocate second: everything in this loop is a field or
    // table READ, so no key gathered here can move before `make_hashset_with_elements`
    // pins them all.
    let mut keys: Vec<Value> = props_collect_keys(ctx, this)
        .into_iter()
        .map(|k| Value::Object(Some(k)))
        .collect();
    let mut defaults_val = props_get_defaults(ctx, this);
    // Bounded: a `Properties` whose `defaults` chain loops back on itself is
    // malformed, but it is reachable from application code
    // (`p.defaults = p` via a subclass), and a hang inside a bootstrap-path
    // native is the worst way to report it. The JDK's own walk is unbounded;
    // 64 is far past any real chain.
    for _ in 0..64 {
        let Value::Object(Some(defs)) = defaults_val else {
            break;
        };
        keys.extend(
            props_collect_keys(ctx, defs)
                .into_iter()
                .map(|k| Value::Object(Some(k))),
        );
        defaults_val = props_get_defaults(ctx, defs);
    }
    Ok(Some(Value::Object(Some(make_hashset_with_elements(
        ctx, &keys,
    )?))))
}

// ===========================================================================
// Unmodifiable collection views
// ===========================================================================
//
// `Collections.unmodifiableList/Set/Map/Collection(...)` return wrapper views
// that delegate every read to the backing collection and throw
// `UnsupportedOperationException` from every mutator. `List.of` / `Set.of` /
// `Map.of` produce fully immutable snapshots (the backing collection is
// private, so it can never be mutated).
//
// Each wrapper is a dedicated synthetic class with a single field (slot 0)
// holding the backing collection's ObjectRef. Read methods forward to the
// backing object via `invoke_virtual`, so dispatch lands on whatever native
// implements the concrete backing type (ArrayList / HashMap / HashSet / ...).
// Mutators are registered natives that unconditionally throw.

/// Synthetic class names for the unmodifiable wrappers.
pub(crate) const UNMOD_LIST_CLASS: &str = "cratonvm/internal/UnmodifiableList";
pub(crate) const UNMOD_SET_CLASS: &str = "cratonvm/internal/UnmodifiableSet";
pub(crate) const UNMOD_SORTED_SET_CLASS: &str = "cratonvm/internal/UnmodifiableSortedSet";
pub(crate) const UNMOD_NAVIGABLE_SET_CLASS: &str = "cratonvm/internal/UnmodifiableNavigableSet";
pub(crate) const UNMOD_MAP_CLASS: &str = "cratonvm/internal/UnmodifiableMap";
pub(crate) const UNMOD_COLLECTION_CLASS: &str = "cratonvm/internal/UnmodifiableCollection";
pub(crate) const UNMOD_ITR_CLASS: &str = "cratonvm/internal/UnmodifiableItr";
pub(crate) const UNMOD_LIST_ITR_CLASS: &str = "cratonvm/internal/UnmodifiableListItr";
/// `entrySet()` of an unmodifiable/immutable map. Distinct from
/// [`UNMOD_SET_CLASS`] because its elements are `Map.Entry` — its
/// `iterator()`/`forEach()` wrap each entry in [`UNMOD_MAP_ENTRY_CLASS`] so
/// `setValue()` throws instead of mutating the backing map, matching the
/// JDK's `Collections$UnmodifiableMap$UnmodifiableEntrySet`.
pub(crate) const UNMOD_ENTRY_SET_CLASS: &str = "cratonvm/internal/UnmodifiableEntrySet";
/// Iterator over an [`UNMOD_ENTRY_SET_CLASS`] whose `next()` wraps the
/// backing iterator's `Map.Entry` in [`UNMOD_MAP_ENTRY_CLASS`].
pub(crate) const UNMOD_ENTRY_ITR_CLASS: &str = "cratonvm/internal/UnmodifiableEntryItr";
/// Read-only `Map.Entry` view: `getKey`/`getValue`/`toString`/`hashCode`/
/// `equals` delegate to the backing (real, mutable) entry; `setValue` throws.
pub(crate) const UNMOD_MAP_ENTRY_CLASS: &str = "cratonvm/internal/UnmodifiableMapEntry";

/// Slot 0 of every wrapper holds the backing collection / iterator.
pub(crate) const UNMOD_FIELD_BACKING: usize = 0;

/// Slot 1 distinguishes the *immutable* `List.of`/`Set.of`/`Map.of`/`copyOf`
/// factories (`Int(1)`) from the *unmodifiable* `Collections.unmodifiable*`
/// wrappers (default). Both share the same `cratonvm/internal/Unmodifiable*`
/// stamp but report different concrete classes from `getClass()`
/// (`ImmutableCollections$*` vs `Collections$Unmodifiable*`). Set only by
/// `freeze_result`. CONTRACT: this slot index is mirrored by
/// `getclass_immutable_marker` in native-builtins (the `getClass()` display
/// map); keep the two in sync.
pub(crate) const UNMOD_FIELD_IMMUTABLE: usize = 1;

/// Slot 2 of a map-VIEW wrapper: set when the map it is a view OF came from an
/// immutable factory.
///
/// Deliberately NOT `UNMOD_FIELD_IMMUTABLE`. That slot is a cross-crate
/// contract -- `getclass_immutable_marker` in native-builtins reads it to
/// decide whether `getClass()` says `ImmutableCollections$*` or
/// `Collections$Unmodifiable*` -- and a `Map.of` keySet is NEITHER on HotSpot:
/// it is `java.util.AbstractMap$1`. Widening that marker would make
/// `getClass()` answer a third wrong name, and `unmod_is_immutable` also gates
/// RULE I's null-query NPE, which this measurement says nothing about.
///
/// So this is a narrow flag with a single reader.
pub(crate) const UNMOD_FIELD_IMMUTABLE_VIEW: usize = 2;

/// Whether `this` is a view of a map that came from `Map.of`/`Map.copyOf`.
pub(crate) fn unmod_is_immutable_map_view(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    ctx.object_num_fields(this) > UNMOD_FIELD_IMMUTABLE_VIEW
        && matches!(
            ctx.get_field(this, UNMOD_FIELD_IMMUTABLE_VIEW),
            Value::Int(1)
        )
}

pub(crate) fn is_unmod_set_class(name: &str) -> bool {
    matches!(
        name,
        UNMOD_SET_CLASS
            | UNMOD_SORTED_SET_CLASS
            | UNMOD_NAVIGABLE_SET_CLASS
            | UNMOD_ENTRY_SET_CLASS
    )
}

/// Build an `UnsupportedOperationException` error for a blocked mutator.
pub(crate) fn unsupported_op() -> MethodCallFailed {
    RuntimeError::UnsupportedOperationException {
        message: String::new(),
    }
    .into()
}

/// Allocate an unmodifiable wrapper of `class_name` around `backing`.
///
/// Two slots: slot 0 = backing collection (read by every wrapper native), slot
/// 1 = the [`UNMOD_FIELD_IMMUTABLE`] marker, left at its default here and set to
/// `Int(1)` only by `freeze_result` for the immutable `*.of`/`copyOf` factories.
///
/// Fallible since 2026-08-05 (JDK-only wave 2, lane L7). The seven
/// `cratonvm/internal/Unmodifiable*` views stand in for
/// `java.util.Collections$Unmodifiable*` and the `ImmutableCollections$*`
/// family, whose real bytecode is not running — a compatibility substitution
/// contract §5 forbids under `--jdk-only`, whatever the stand-in is named. It
/// is refused there, as a catchable `NoClassDefFoundError` naming the class,
/// instead of being recorded as a violation and then performed anyway.
pub(crate) fn alloc_unmod_wrapper(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    backing: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let backing_pin = ctx.pin_native_root(backing);
    // Not `?`: the pin above is this frame's base and must be released before
    // unwinding, or it and everything pinned above it are stranded.
    let wrapper = match try_alloc_synthetic(ctx, class_name, 2) {
        Ok(wrapper) => wrapper,
        Err(err) => {
            ctx.unpin_native_roots(backing_pin);
            return Err(err);
        }
    };
    let backing = ctx.read_native_pin(backing_pin, backing);
    ctx.set_field(wrapper, UNMOD_FIELD_BACKING, Value::Object(Some(backing)));
    ctx.unpin_native_roots(backing_pin);
    Ok(wrapper)
}

/// Allocate an *immutable*-collection wrapper (the `List.of`/`Set.of`/`Map.of`/
/// `copyOf` factories) — an unmodifiable wrapper with the [`UNMOD_FIELD_IMMUTABLE`]
/// marker set, so `getClass()` reports the `java.util.ImmutableCollections$*`
/// family rather than `Collections$Unmodifiable*`.
///
/// # `H0-2` §4's twelve `instanceof` cells cannot be fixed by a retag
///
/// H4-1, 2026-08-20, source-verified. `H0-2` §5 proposes moving the map/set
/// cluster so that "`Map.of(...)` returns a real `ImmutableCollections$Map1`
/// and `is_subclass_of` walks a real chain". A `NativeKind` change cannot do
/// that, in either direction:
///
/// * **In `--jdk-only` the producers are ALREADY refused.** Every allocator of
///   a `cratonvm/internal/Unmodifiable*` — `register_factory_natives`
///   (`set_category(SyntheticStub)` at its own head), the six `unmodifiable*`
///   rows in `register_collections_extras_natives` (explicit inner
///   `SyntheticStub` window) and the three `copyOf` rows (explicit
///   `register_with_kind(..., SyntheticStub)`) — is dropped by
///   `NativeMethodRegistry::register_inner`'s `JdkOnly` refusal arm, so strict
///   mode never reaches this function and already answers all twelve cells
///   from real bytecode. `HANDOFF-20260819.md` §6 records the corroborating
///   arm result: `RImmutableFactoryTypes` is one of the SUITE=all five and the
///   `--jdk-only` arm over the same 102 vectors is 102/102.
/// * **In `Compatible` the kind is discarded.** `NativeKind::allowed_in`
///   returns an unconditional `true` for `Compatible`, and the only
///   Compatible-mode kind test is `invoke_or_native`'s `real_protected_stub`
///   arm, gated on `real_protected_stub_class` — a twelve-entry allow-list
///   that contains no `java/util` collection and no `cratonvm/internal/*`
///   class. So no tag on any registrar changes what this function does under
///   `--real-jdk`.
///
/// Restated as the rule: `HANDOFF-20260819.md` §1's trap runs both ways.
/// Retiring stubs moves compatible mode by zero; **retagging moves compatible
/// mode by zero too.** `H0-2` §4 is a compatible-mode measurement, so its
/// remedy has to be a compatible-mode change — retiring this carrier at its
/// producers, or `P4A` N1b option (c). Not a tag.
pub(crate) fn alloc_immutable_wrapper(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    backing: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let wrapper = alloc_unmod_wrapper(ctx, class_name, backing)?;
    ctx.set_field(wrapper, UNMOD_FIELD_IMMUTABLE, Value::Int(1));
    Ok(wrapper)
}

/// Allocate a wrapper for one of a map's three VIEWS, carrying
/// [`UNMOD_FIELD_IMMUTABLE_VIEW`] when the map itself was immutable.
pub(crate) fn alloc_unmod_view_wrapper(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    backing: ObjectRef,
    from_immutable: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    if !from_immutable {
        return alloc_unmod_wrapper(ctx, class_name, backing);
    }
    let backing_pin = ctx.pin_native_root(backing);
    let wrapper = match try_alloc_synthetic(ctx, class_name, 3) {
        Ok(w) => w,
        Err(err) => {
            ctx.unpin_native_roots(backing_pin);
            return Err(err);
        }
    };
    let backing = ctx.read_native_pin(backing_pin, backing);
    ctx.set_field(wrapper, UNMOD_FIELD_BACKING, Value::Object(Some(backing)));
    ctx.set_field(wrapper, UNMOD_FIELD_IMMUTABLE_VIEW, Value::Int(1));
    ctx.unpin_native_roots(backing_pin);
    Ok(wrapper)
}

/// Read the backing collection out of a wrapper. Recurses if the backing is
/// itself a wrapper (e.g. `unmodifiableList(unmodifiableList(x))`).
pub(crate) fn unmod_backing(ctx: &mut dyn NativeContext, wrapper: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field(wrapper, UNMOD_FIELD_BACKING) {
        Value::Object(Some(b)) => Some(b),
        _ => None,
    }
}

/// Forward a read-only call to the backing collection. If the wrapper's
/// backing is missing, returns a benign default.
pub(crate) fn unmod_delegate(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let backing = match unmod_backing(ctx, this) {
        Some(b) => b,
        None => return Ok(Some(Value::Object(None))),
    };
    ctx.invoke_virtual(backing, method, descriptor, &args[1..])
}

/// Forward a read-only NavigableMap/SortedMap *view* call to the backing
/// (e.g. `headMap`, `subMap`, `descendingMap`) and re-wrap the returned map as
/// unmodifiable, mirroring the JDK's UnmodifiableNavigableMap which returns
/// unmodifiable submaps. A non-map / null result is passed through unchanged.
pub(crate) fn unmod_delegate_rewrap_map(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
) -> MethodCallResult {
    let res = unmod_delegate(ctx, args, method, descriptor)?;
    match res {
        Some(Value::Object(Some(m))) => Ok(Some(Value::Object(Some(alloc_unmod_wrapper(
            ctx,
            UNMOD_MAP_CLASS,
            m,
        )?)))),
        other => Ok(other),
    }
}

/// Forward a read-only NavigableSet/SortedSet *view* call to the backing
/// (e.g. `tailSet`, `headSet`, `subSet`, `descendingSet`) and re-wrap the
/// returned set as unmodifiable, mirroring the JDK's `UnmodifiableNavigableSet`
/// which returns unmodifiable sub-views. A non-set / null result is passed
/// through unchanged.
pub(crate) fn unmod_delegate_rewrap_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
    wrapper_class: &str,
) -> MethodCallResult {
    let res = unmod_delegate(ctx, args, method, descriptor)?;
    match res {
        // Sub-views (tailSet/headSet/subSet/descendingSet) are only reachable
        // starting from an already-sorted/navigable set, and the JDK's own
        // UnmodifiableNavigableSet returns further NavigableSet-typed
        // sub-views — so re-wrap with UNMOD_SORTED_SET_CLASS (NOT the plain
        // UNMOD_SET_CLASS), keeping the SortedSet/NavigableSet surface (e.g.
        // comparator()) available on the result the same way the JDK does.
        Some(Value::Object(Some(s))) => Ok(Some(Value::Object(Some(alloc_unmod_wrapper(
            ctx,
            wrapper_class,
            s,
        )?)))),
        other => Ok(other),
    }
}

pub(crate) fn unmod_delegate_rewrap_sorted_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
) -> MethodCallResult {
    unmod_delegate_rewrap_set(ctx, args, method, descriptor, UNMOD_SORTED_SET_CLASS)
}

pub(crate) fn unmod_delegate_rewrap_navigable_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
) -> MethodCallResult {
    unmod_delegate_rewrap_set(ctx, args, method, descriptor, UNMOD_NAVIGABLE_SET_CLASS)
}

pub(crate) fn register_unmodifiable_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    // `SyntheticStub`, not `Bridge` (JDK-only wave 2, lane L7 residual R1,
    // 2026-08-05). Every factory here returns a `cratonvm/internal/*` stand-in
    // for a `java.util.Collections$Unmodifiable*` / `ImmutableCollections$*` /
    // comparator object, and every method it registers on those stand-ins only
    // exists to serve one. `Bridge` asserts "no working real-bytecode fallback
    // exists"; for this family there plainly is one, in `java.base`, and it
    // works even though the backing collection is native-backed — a real
    // unmodifiable wrapper *delegates* every call to the map it was handed, and
    // that map's natives still answer. (Contrast `HashSet.iterator()`, which
    // cannot be retagged: the real iterator reads the real `table[]`, which
    // CratonVM's `HashMap.put` native never fills, so it would return a
    // silently EMPTY iteration rather than a loud error. That one stays a
    // refusal until the collections reclassification wave.)
    //
    // Under `--jdk-only` these are now dropped at registration — recorded as
    // `SyntheticNativeRegistered` violations naming this site — and the real
    // bytecode runs. `Compatible` / `--real-jdk` keep SyntheticStub
    // registrations, so both are unchanged.
    //
    // This is the "retag per subsystem, one PR each, with evidence" the
    // `register_collections_natives` header asks for, not the bulk flip it
    // forbids: four named registrars, each with a real-bytecode fallback that
    // was measured, not assumed.
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    // ---- UnmodifiableCollection (also the shared base for List/Set) -------
    for c in [
        UNMOD_COLLECTION_CLASS,
        UNMOD_LIST_CLASS,
        UNMOD_SET_CLASS,
        UNMOD_SORTED_SET_CLASS,
        UNMOD_NAVIGABLE_SET_CLASS,
        UNMOD_ENTRY_SET_CLASS,
    ] {
        r.register(c, "size", "()I", native_unmod_size);
        r.register(c, "isEmpty", "()Z", native_unmod_is_empty);
        r.register(
            c,
            "contains",
            "(Ljava/lang/Object;)Z",
            native_unmod_contains,
        );
        r.register(
            c,
            "containsAll",
            "(Ljava/util/Collection;)Z",
            native_unmod_contains_all,
        );
        r.register(
            c,
            "iterator",
            "()Ljava/util/Iterator;",
            native_unmod_iterator,
        );
        r.register(c, "toArray", "()[Ljava/lang/Object;", native_unmod_to_array);
        r.register(
            c,
            "toArray",
            "([Ljava/lang/Object;)[Ljava/lang/Object;",
            native_unmod_to_array_typed,
        );
        r.register(
            c,
            "toString",
            "()Ljava/lang/String;",
            native_unmod_to_string,
        );
        r.register(c, "hashCode", "()I", native_unmod_hash_code);
        r.register(c, "equals", "(Ljava/lang/Object;)Z", native_unmod_equals);
        r.register(
            c,
            "forEach",
            "(Ljava/util/function/Consumer;)V",
            native_unmod_for_each,
        );
        r.register(
            c,
            "stream",
            "()Ljava/util/stream/Stream;",
            native_unmod_stream,
        );
        r.register(
            c,
            "spliterator",
            "()Ljava/util/Spliterator;",
            native_unmod_spliterator,
        );
        // Mutators — all throw UnsupportedOperationException.
        r.register(c, "add", "(Ljava/lang/Object;)Z", native_unmod_throw);
        r.register(c, "remove", "(Ljava/lang/Object;)Z", native_unmod_throw);
        r.register(c, "clear", "()V", native_unmod_throw);
        r.register(c, "addAll", "(Ljava/util/Collection;)Z", native_unmod_throw);
        r.register(
            c,
            "removeAll",
            "(Ljava/util/Collection;)Z",
            native_unmod_throw,
        );
        r.register(
            c,
            "retainAll",
            "(Ljava/util/Collection;)Z",
            native_unmod_throw,
        );
        r.register(
            c,
            "removeIf",
            "(Ljava/util/function/Predicate;)Z",
            native_unmod_throw,
        );
        // NavigableMap / SortedMap read-only views. `cratonvm/internal/
        // UnmodifiableMap` is synthetic (no real bytecode), so any nav method
        // not registered here raises NoSuchMethodError when a wrapped TreeMap /
        // NavigableMap is navigated — e.g. `unmodifiableNavigableMap(tm)
        // .higherEntry(k)`. Each simply forwards to the backing map (which has
        // the full nav surface registered above), preserving read-only-ness;
        // the mutating poll* variants stay unregistered/throwing.
        r.register(c, "firstKey", "()Ljava/lang/Object;", |ctx, args| {
            unmod_delegate(ctx, args, "firstKey", "()Ljava/lang/Object;")
        });
        r.register(c, "lastKey", "()Ljava/lang/Object;", |ctx, args| {
            unmod_delegate(ctx, args, "lastKey", "()Ljava/lang/Object;")
        });
        for m in ["ceilingKey", "floorKey", "higherKey", "lowerKey"] {
            let desc = "(Ljava/lang/Object;)Ljava/lang/Object;";
            let cb: cratonvm_native_api::NativeCallback = match m {
                "ceilingKey" => |ctx, args| {
                    unmod_delegate(
                        ctx,
                        args,
                        "ceilingKey",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                    )
                },
                "floorKey" => |ctx, args| {
                    unmod_delegate(
                        ctx,
                        args,
                        "floorKey",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                    )
                },
                "higherKey" => |ctx, args| {
                    unmod_delegate(
                        ctx,
                        args,
                        "higherKey",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                    )
                },
                _ => |ctx, args| {
                    unmod_delegate(
                        ctx,
                        args,
                        "lowerKey",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                    )
                },
            };
            r.register(c, m, desc, cb);
        }
        r.register(c, "firstEntry", "()Ljava/util/Map$Entry;", |ctx, args| {
            unmod_delegate(ctx, args, "firstEntry", "()Ljava/util/Map$Entry;")
        });
        r.register(c, "lastEntry", "()Ljava/util/Map$Entry;", |ctx, args| {
            unmod_delegate(ctx, args, "lastEntry", "()Ljava/util/Map$Entry;")
        });
        for m in ["ceilingEntry", "floorEntry", "higherEntry", "lowerEntry"] {
            let desc = "(Ljava/lang/Object;)Ljava/util/Map$Entry;";
            let cb: cratonvm_native_api::NativeCallback = match m {
                "ceilingEntry" => |ctx, args| {
                    unmod_delegate(
                        ctx,
                        args,
                        "ceilingEntry",
                        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
                    )
                },
                "floorEntry" => |ctx, args| {
                    unmod_delegate(
                        ctx,
                        args,
                        "floorEntry",
                        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
                    )
                },
                "higherEntry" => |ctx, args| {
                    unmod_delegate(
                        ctx,
                        args,
                        "higherEntry",
                        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
                    )
                },
                _ => |ctx, args| {
                    unmod_delegate(
                        ctx,
                        args,
                        "lowerEntry",
                        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
                    )
                },
            };
            r.register(c, m, desc, cb);
        }
    }

    // ---- UnmodifiableSortedSet / NavigableSet read-only surfaces ----------
    //
    // Plain `Collections.unmodifiableSet` must not satisfy `SortedSet`; AssertJ
    // checks that guard before calling `comparator()`. Sorted/navigable APIs use
    // distinct stamps so those casts keep working without broadening every
    // unmodifiable set view.
    {
        for c in [UNMOD_SORTED_SET_CLASS, UNMOD_NAVIGABLE_SET_CLASS] {
            r.register(c, "first", "()Ljava/lang/Object;", |ctx, args| {
                unmod_delegate(ctx, args, "first", "()Ljava/lang/Object;")
            });
            r.register(c, "last", "()Ljava/lang/Object;", |ctx, args| {
                unmod_delegate(ctx, args, "last", "()Ljava/lang/Object;")
            });
            r.register(c, "comparator", "()Ljava/util/Comparator;", |ctx, args| {
                unmod_delegate(ctx, args, "comparator", "()Ljava/util/Comparator;")
            });
            r.register(
                c,
                "headSet",
                "(Ljava/lang/Object;)Ljava/util/SortedSet;",
                |ctx, args| {
                    unmod_delegate_rewrap_sorted_set(
                        ctx,
                        args,
                        "headSet",
                        "(Ljava/lang/Object;)Ljava/util/SortedSet;",
                    )
                },
            );
            r.register(
                c,
                "tailSet",
                "(Ljava/lang/Object;)Ljava/util/SortedSet;",
                |ctx, args| {
                    unmod_delegate_rewrap_sorted_set(
                        ctx,
                        args,
                        "tailSet",
                        "(Ljava/lang/Object;)Ljava/util/SortedSet;",
                    )
                },
            );
            r.register(
                c,
                "subSet",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/SortedSet;",
                |ctx, args| {
                    unmod_delegate_rewrap_sorted_set(
                        ctx,
                        args,
                        "subSet",
                        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/SortedSet;",
                    )
                },
            );
        }

        let c = UNMOD_NAVIGABLE_SET_CLASS;
        for m in ["ceiling", "floor", "higher", "lower"] {
            let desc = "(Ljava/lang/Object;)Ljava/lang/Object;";
            let cb: cratonvm_native_api::NativeCallback = match m {
                "ceiling" => |ctx, args| {
                    unmod_delegate(
                        ctx,
                        args,
                        "ceiling",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                    )
                },
                "floor" => |ctx, args| {
                    unmod_delegate(ctx, args, "floor", "(Ljava/lang/Object;)Ljava/lang/Object;")
                },
                "higher" => |ctx, args| {
                    unmod_delegate(
                        ctx,
                        args,
                        "higher",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                    )
                },
                _ => |ctx, args| {
                    unmod_delegate(ctx, args, "lower", "(Ljava/lang/Object;)Ljava/lang/Object;")
                },
            };
            r.register(c, m, desc, cb);
        }
        r.register(
            c,
            "descendingIterator",
            "()Ljava/util/Iterator;",
            |ctx, args| unmod_delegate(ctx, args, "descendingIterator", "()Ljava/util/Iterator;"),
        );
        r.register(c, "pollFirst", "()Ljava/lang/Object;", native_unmod_throw);
        r.register(c, "pollLast", "()Ljava/lang/Object;", native_unmod_throw);
        r.register(
            c,
            "descendingSet",
            "()Ljava/util/NavigableSet;",
            |ctx, args| {
                unmod_delegate_rewrap_navigable_set(
                    ctx,
                    args,
                    "descendingSet",
                    "()Ljava/util/NavigableSet;",
                )
            },
        );
        r.register(
            c,
            "headSet",
            "(Ljava/lang/Object;Z)Ljava/util/NavigableSet;",
            |ctx, args| {
                unmod_delegate_rewrap_navigable_set(
                    ctx,
                    args,
                    "headSet",
                    "(Ljava/lang/Object;Z)Ljava/util/NavigableSet;",
                )
            },
        );
        r.register(
            c,
            "tailSet",
            "(Ljava/lang/Object;Z)Ljava/util/NavigableSet;",
            |ctx, args| {
                unmod_delegate_rewrap_navigable_set(
                    ctx,
                    args,
                    "tailSet",
                    "(Ljava/lang/Object;Z)Ljava/util/NavigableSet;",
                )
            },
        );
        r.register(
            c,
            "subSet",
            "(Ljava/lang/Object;ZLjava/lang/Object;Z)Ljava/util/NavigableSet;",
            |ctx, args| {
                unmod_delegate_rewrap_navigable_set(
                    ctx,
                    args,
                    "subSet",
                    "(Ljava/lang/Object;ZLjava/lang/Object;Z)Ljava/util/NavigableSet;",
                )
            },
        );
    }

    // ---- UnmodifiableList — adds positional reads + list mutators ---------
    {
        let c = UNMOD_LIST_CLASS;
        r.register(c, "get", "(I)Ljava/lang/Object;", native_unmod_get);
        r.register(c, "indexOf", "(Ljava/lang/Object;)I", native_unmod_index_of);
        r.register(
            c,
            "lastIndexOf",
            "(Ljava/lang/Object;)I",
            native_unmod_last_index_of,
        );
        r.register(c, "subList", "(II)Ljava/util/List;", native_unmod_sub_list);
        r.register(
            c,
            "listIterator",
            "()Ljava/util/ListIterator;",
            native_unmod_list_iterator,
        );
        r.register(
            c,
            "listIterator",
            "(I)Ljava/util/ListIterator;",
            native_unmod_list_iterator_idx,
        );
        // List mutators.
        r.register(
            c,
            "set",
            "(ILjava/lang/Object;)Ljava/lang/Object;",
            native_unmod_throw,
        );
        r.register(c, "add", "(ILjava/lang/Object;)V", native_unmod_throw);
        r.register(c, "remove", "(I)Ljava/lang/Object;", native_unmod_throw);
        r.register(
            c,
            "addAll",
            "(ILjava/util/Collection;)Z",
            native_unmod_throw,
        );
        r.register(c, "sort", "(Ljava/util/Comparator;)V", native_unmod_throw);
        r.register(
            c,
            "replaceAll",
            "(Ljava/util/function/UnaryOperator;)V",
            native_unmod_throw,
        );
    }

    // ---- UnmodifiableMap --------------------------------------------------
    {
        let c = UNMOD_MAP_CLASS;
        r.register(c, "size", "()I", native_unmod_size);
        r.register(c, "isEmpty", "()Z", native_unmod_is_empty);
        r.register(
            c,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            native_unmod_map_get,
        );
        r.register(
            c,
            "getOrDefault",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            native_unmod_map_get_or_default,
        );
        r.register(
            c,
            "containsKey",
            "(Ljava/lang/Object;)Z",
            native_unmod_map_contains_key,
        );
        r.register(
            c,
            "containsValue",
            "(Ljava/lang/Object;)Z",
            native_unmod_map_contains_value_checked,
        );
        r.register(c, "keySet", "()Ljava/util/Set;", native_unmod_map_key_set);
        r.register(
            c,
            "values",
            "()Ljava/util/Collection;",
            native_unmod_map_values,
        );
        r.register(
            c,
            "entrySet",
            "()Ljava/util/Set;",
            native_unmod_map_entry_set,
        );
        r.register(
            c,
            "toString",
            "()Ljava/lang/String;",
            native_unmod_to_string,
        );
        r.register(c, "hashCode", "()I", native_unmod_hash_code);
        r.register(c, "equals", "(Ljava/lang/Object;)Z", native_unmod_equals);
        // SequencedMap.reversed() (Java 21) — an unmodifiable reverse-ordered
        // view over the backing's entries. The synthetic wrapper does not
        // implement SequencedMap, so an unintercepted call raised
        // NoSuchMethodError (blocked ES `Version`/Log4j config walking). Build a
        // reverse snapshot of the backing and re-wrap it read-only.
        r.register(c, "reversed", "()Ljava/util/SequencedMap;", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let backing = match unmod_backing(ctx, this) {
                Some(b) => b,
                None => return Ok(Some(Value::Object(None))),
            };
            let rev = build_reversed_map_snapshot(ctx, backing)?;
            Ok(Some(Value::Object(Some(alloc_unmod_wrapper(
                ctx,
                UNMOD_MAP_CLASS,
                rev,
            )?))))
        });
        r.register(
            c,
            "forEach",
            "(Ljava/util/function/BiConsumer;)V",
            native_unmod_map_for_each,
        );
        // Map mutators.
        r.register(
            c,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            native_unmod_throw,
        );
        r.register(
            c,
            "remove",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            native_unmod_throw,
        );
        r.register(c, "clear", "()V", native_unmod_throw);
        r.register(c, "putAll", "(Ljava/util/Map;)V", native_unmod_throw);
        r.register(
            c,
            "putIfAbsent",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            native_unmod_throw,
        );
        r.register(
            c,
            "replace",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            native_unmod_throw,
        );
        r.register(
            c,
            "computeIfAbsent",
            "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;",
            native_unmod_throw,
        );
        r.register(
            c,
            "compute",
            "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
            native_unmod_throw,
        );
        r.register(
            c,
            "computeIfPresent",
            "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
            native_unmod_throw,
        );
        r.register(
            c,
            "merge",
            "(Ljava/lang/Object;Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
            native_unmod_throw,
        );
        r.register(
            c,
            "replaceAll",
            "(Ljava/util/function/BiFunction;)V",
            native_unmod_throw,
        );

        // ---- NavigableMap / SortedMap read surface ------------------------
        // `Collections.unmodifiableNavigableMap` / `unmodifiableSortedMap`
        // reuse this same wrapper class, but only the plain Map methods were
        // registered. Any NavigableMap/SortedMap call (e.g. `floorEntry`,
        // dispatched from a `NavigableMap`-typed reference and retargeted to
        // the concrete wrapper) hit NoSuchMethodError — a dominant Elasticsearch
        // suite-setup blocker. Delegate every read to the backing (a real
        // NavigableMap such as TreeMap); re-wrap map-view returns as read-only;
        // the `poll*` views are mutators → throw. (`NativeCallback` is a bare
        // fn pointer, so each closure must inline its own name/descriptor — no
        // captures — matching the ASL `toArray` delegation pattern above.)
        r.register(c, "comparator", "()Ljava/util/Comparator;", |ctx, args| {
            unmod_delegate(ctx, args, "comparator", "()Ljava/util/Comparator;")
        });
        r.register(c, "firstKey", "()Ljava/lang/Object;", |ctx, args| {
            unmod_delegate(ctx, args, "firstKey", "()Ljava/lang/Object;")
        });
        r.register(c, "lastKey", "()Ljava/lang/Object;", |ctx, args| {
            unmod_delegate(ctx, args, "lastKey", "()Ljava/lang/Object;")
        });
        r.register(c, "firstEntry", "()Ljava/util/Map$Entry;", |ctx, args| {
            unmod_delegate(ctx, args, "firstEntry", "()Ljava/util/Map$Entry;")
        });
        r.register(c, "lastEntry", "()Ljava/util/Map$Entry;", |ctx, args| {
            unmod_delegate(ctx, args, "lastEntry", "()Ljava/util/Map$Entry;")
        });
        r.register(
            c,
            "floorEntry",
            "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
            |ctx, args| {
                unmod_delegate(
                    ctx,
                    args,
                    "floorEntry",
                    "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
                )
            },
        );
        r.register(
            c,
            "ceilingEntry",
            "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
            |ctx, args| {
                unmod_delegate(
                    ctx,
                    args,
                    "ceilingEntry",
                    "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
                )
            },
        );
        r.register(
            c,
            "higherEntry",
            "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
            |ctx, args| {
                unmod_delegate(
                    ctx,
                    args,
                    "higherEntry",
                    "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
                )
            },
        );
        r.register(
            c,
            "lowerEntry",
            "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
            |ctx, args| {
                unmod_delegate(
                    ctx,
                    args,
                    "lowerEntry",
                    "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
                )
            },
        );
        r.register(
            c,
            "floorKey",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            |ctx, args| {
                unmod_delegate(
                    ctx,
                    args,
                    "floorKey",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                )
            },
        );
        r.register(
            c,
            "ceilingKey",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            |ctx, args| {
                unmod_delegate(
                    ctx,
                    args,
                    "ceilingKey",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                )
            },
        );
        r.register(
            c,
            "higherKey",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            |ctx, args| {
                unmod_delegate(
                    ctx,
                    args,
                    "higherKey",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                )
            },
        );
        r.register(
            c,
            "lowerKey",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            |ctx, args| {
                unmod_delegate(
                    ctx,
                    args,
                    "lowerKey",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                )
            },
        );
        r.register(
            c,
            "navigableKeySet",
            "()Ljava/util/NavigableSet;",
            |ctx, args| unmod_delegate(ctx, args, "navigableKeySet", "()Ljava/util/NavigableSet;"),
        );
        r.register(
            c,
            "descendingKeySet",
            "()Ljava/util/NavigableSet;",
            |ctx, args| unmod_delegate(ctx, args, "descendingKeySet", "()Ljava/util/NavigableSet;"),
        );
        // Map-returning views — re-wrap read-only.
        r.register(
            c,
            "descendingMap",
            "()Ljava/util/NavigableMap;",
            |ctx, args| {
                unmod_delegate_rewrap_map(ctx, args, "descendingMap", "()Ljava/util/NavigableMap;")
            },
        );
        r.register(
            c,
            "headMap",
            "(Ljava/lang/Object;Z)Ljava/util/NavigableMap;",
            |ctx, args| {
                unmod_delegate_rewrap_map(
                    ctx,
                    args,
                    "headMap",
                    "(Ljava/lang/Object;Z)Ljava/util/NavigableMap;",
                )
            },
        );
        r.register(
            c,
            "tailMap",
            "(Ljava/lang/Object;Z)Ljava/util/NavigableMap;",
            |ctx, args| {
                unmod_delegate_rewrap_map(
                    ctx,
                    args,
                    "tailMap",
                    "(Ljava/lang/Object;Z)Ljava/util/NavigableMap;",
                )
            },
        );
        r.register(
            c,
            "subMap",
            "(Ljava/lang/Object;ZLjava/lang/Object;Z)Ljava/util/NavigableMap;",
            |ctx, args| {
                unmod_delegate_rewrap_map(
                    ctx,
                    args,
                    "subMap",
                    "(Ljava/lang/Object;ZLjava/lang/Object;Z)Ljava/util/NavigableMap;",
                )
            },
        );
        r.register(
            c,
            "headMap",
            "(Ljava/lang/Object;)Ljava/util/SortedMap;",
            |ctx, args| {
                unmod_delegate_rewrap_map(
                    ctx,
                    args,
                    "headMap",
                    "(Ljava/lang/Object;)Ljava/util/SortedMap;",
                )
            },
        );
        r.register(
            c,
            "tailMap",
            "(Ljava/lang/Object;)Ljava/util/SortedMap;",
            |ctx, args| {
                unmod_delegate_rewrap_map(
                    ctx,
                    args,
                    "tailMap",
                    "(Ljava/lang/Object;)Ljava/util/SortedMap;",
                )
            },
        );
        r.register(
            c,
            "subMap",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/SortedMap;",
            |ctx, args| {
                unmod_delegate_rewrap_map(
                    ctx,
                    args,
                    "subMap",
                    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/SortedMap;",
                )
            },
        );
        // Mutating navigable views.
        r.register(
            c,
            "pollFirstEntry",
            "()Ljava/util/Map$Entry;",
            native_unmod_throw,
        );
        r.register(
            c,
            "pollLastEntry",
            "()Ljava/util/Map$Entry;",
            native_unmod_throw,
        );
    }

    // ---- UnmodifiableItr — read-only iterator -----------------------------
    {
        let c = UNMOD_ITR_CLASS;
        r.register(c, "hasNext", "()Z", native_unmod_itr_has_next);
        r.register(c, "next", "()Ljava/lang/Object;", native_unmod_itr_next);
        r.register(c, "remove", "()V", native_unmod_throw);
    }

    // ---- UnmodifiableEntrySet — entrySet() of an unmodifiable/immutable
    // map. Overrides the plain-Set `iterator`/`forEach` registered by the
    // shared loop above so each yielded `Map.Entry` is wrapped in
    // `UnmodifiableMapEntry` (setValue() must throw, not silently mutate the
    // backing map through the "locked" view — see
    // parametermap-immutability-not-locked-FIXED.md).
    {
        let c = UNMOD_ENTRY_SET_CLASS;
        r.register(
            c,
            "iterator",
            "()Ljava/util/Iterator;",
            native_unmod_entry_set_iterator,
        );
        r.register(
            c,
            "forEach",
            "(Ljava/util/function/Consumer;)V",
            native_unmod_entry_set_for_each,
        );
    }

    // ---- UnmodifiableEntryItr — iterator over an UnmodifiableEntrySet -----
    {
        let c = UNMOD_ENTRY_ITR_CLASS;
        r.register(c, "hasNext", "()Z", native_unmod_itr_has_next);
        r.register(
            c,
            "next",
            "()Ljava/lang/Object;",
            native_unmod_entry_itr_next,
        );
        r.register(c, "remove", "()V", native_unmod_throw);
    }

    // ---- UnmodifiableMapEntry — read-only Map.Entry view -------------------
    // Wraps a real (mutable) Map.Entry so setValue() throws
    // UnsupportedOperationException instead of writing through to the
    // backing map, matching `Collections$UnmodifiableMap$UnmodifiableEntrySet
    // $UnmodifiableEntry`. Only entries obtained via UnmodifiableEntrySet's
    // iterator/forEach are wrapped — entries from other paths (e.g. a direct
    // `Map.get`) are the real mutable objects.
    {
        let c = UNMOD_MAP_ENTRY_CLASS;
        r.register(c, "getKey", "()Ljava/lang/Object;", |ctx, args| {
            unmod_delegate(ctx, args, "getKey", "()Ljava/lang/Object;")
        });
        r.register(c, "getValue", "()Ljava/lang/Object;", |ctx, args| {
            unmod_delegate(ctx, args, "getValue", "()Ljava/lang/Object;")
        });
        r.register(
            c,
            "setValue",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            native_unmod_throw,
        );
        r.register(
            c,
            "toString",
            "()Ljava/lang/String;",
            native_unmod_to_string,
        );
        r.register(c, "hashCode", "()I", native_unmod_hash_code);
        r.register(c, "equals", "(Ljava/lang/Object;)Z", native_unmod_equals);
    }

    // ---- UnmodifiableListItr — read-only ListIterator ---------------------
    // A self-contained `ListIterator` over a snapshot of the backing list:
    //   field 0 = Object[] snapshot of the list elements
    //   field 1 = Int cursor (the `nextIndex`)
    // Read operations walk the snapshot; the mutators (`set`/`add`/`remove`)
    // throw `UnsupportedOperationException`, consistent with an unmodifiable
    // list and the JDK's `Collections$UnmodifiableList$1` list-iterator view.
    {
        let c = UNMOD_LIST_ITR_CLASS;
        r.register(c, "hasNext", "()Z", native_unmod_listitr_has_next);
        r.register(c, "next", "()Ljava/lang/Object;", native_unmod_listitr_next);
        r.register(c, "hasPrevious", "()Z", native_unmod_listitr_has_previous);
        r.register(
            c,
            "previous",
            "()Ljava/lang/Object;",
            native_unmod_listitr_previous,
        );
        r.register(c, "nextIndex", "()I", native_unmod_listitr_next_index);
        r.register(
            c,
            "previousIndex",
            "()I",
            native_unmod_listitr_previous_index,
        );
        r.register(
            c,
            "forEachRemaining",
            "(Ljava/util/function/Consumer;)V",
            native_unmod_listitr_for_each_remaining,
        );
        // Mutators — all throw UnsupportedOperationException.
        r.register(c, "set", "(Ljava/lang/Object;)V", native_unmod_throw);
        r.register(c, "add", "(Ljava/lang/Object;)V", native_unmod_throw);
        r.register(c, "remove", "()V", native_unmod_throw);
    }
    r.set_category(__prev_cat);
}

/// Universal mutator: throws `UnsupportedOperationException`.
pub(crate) fn native_unmod_throw(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Err(unsupported_op())
}

pub(crate) fn native_unmod_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "size", "()I")
}

pub(crate) fn native_unmod_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "isEmpty", "()Z")
}

pub(crate) fn native_unmod_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reject_null_immutable_query(&*ctx, args, 1)?;
    unmod_delegate(ctx, args, "contains", "(Ljava/lang/Object;)Z")
}

pub(crate) fn native_unmod_contains_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "containsAll", "(Ljava/util/Collection;)Z")
}

pub(crate) fn native_unmod_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Bounds-check BEFORE delegating, and keep the
    // `ArrayIndexOutOfBoundsException` this path has always thrown.
    //
    // The delegate is a real `java/util/ArrayList`, so it now answers with the
    // plain `IndexOutOfBoundsException` that HotSpot's `ArrayList` throws --
    // right for that receiver, wrong for this one. Measured against HotSpot 25
    // with `probes/ListOutOfBoundsProbe`: `List.of()` and `List.of(a,b,c,d)`
    // throw `ArrayIndexOutOfBoundsException` (`ImmutableCollections.ListN`
    // indexes its array directly), while the 1-2 element `List12` throws the
    // plain class with a different wording again ("Index: 3 Size: 1").
    //
    // The discriminator is slot 1 (`UNMOD_FIELD_IMMUTABLE`), not the class:
    // `unmod_list_oob_error` separates a 1-2 element `List.of` from
    // `Collections.unmodifiableList` over a 1-2 element `ArrayList` with the
    // same bit `getClass()` already splits on. See that function.
    if let (Some(Value::Object(Some(this))), Some(Value::Int(index))) = (args.first(), args.get(1))
    {
        if let Some(backing) = unmod_receiver_backing(ctx, *this) {
            // The SIZE must not come from `al_state` alone -- it reads an
            // `elementData`-shaped backing and reports 0 for every other List
            // shape, so this pre-check rejected PERFECTLY VALID indices on a
            // view over a LinkedList or an Arrays$ArrayList, while the same
            // list iterated fine (iteration does not come through here):
            //
            //   Collections.unmodifiableList(new LinkedList<>(List.of("x","y")))
            //       .get(0)  -> AIOOBE    (HotSpot: "x")
            //
            // and it is `unmod_delegate` below, not this check, that produces
            // the right answer once the index is allowed through. See
            // `unmod_view_size`. This is the registration that actually serves
            // `Collections.unmodifiableList(..).get(i)` --
            // `--dump-native-registry` names it
            // `cratonvm/internal/UnmodifiableList.get`.
            let (data, al_size) = al_state(ctx, backing);
            if let Some(n) = unmod_view_size(data, al_size) {
                if *index < 0 || *index >= n {
                    if let Some(err) = unmod_list_oob_error(&*ctx, *this, *index, n) {
                        return Err(err);
                    }
                    // `None` = a `Collections.unmodifiable*` view; `unmod_delegate`
                    // below lets the backing throw its own, which is what HotSpot
                    // does for that receiver.
                }
            }
            // `None` = `al_state` could not read this backing. STAND ASIDE and
            // let `unmod_delegate` below bounds-check: the backing's own `get`
            // does it correctly and throws the JDK's own exception for its
            // shape. Asking the backing for its `size()` here instead would
            // cost a full VM dispatch on EVERY get -- two per element for the
            // `for (i..n) list.get(i)` loop that is the common shape -- for a
            // check the delegate is about to do anyway.
        }
    }
    unmod_delegate(ctx, args, "get", "(I)Ljava/lang/Object;")
}

pub(crate) fn native_unmod_index_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reject_null_immutable_query(&*ctx, args, 1)?;
    unmod_delegate(ctx, args, "indexOf", "(Ljava/lang/Object;)I")
}

pub(crate) fn native_unmod_last_index_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reject_null_immutable_query(&*ctx, args, 1)?;
    unmod_delegate(ctx, args, "lastIndexOf", "(Ljava/lang/Object;)I")
}

pub(crate) fn native_unmod_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "toArray", "()[Ljava/lang/Object;")
}

pub(crate) fn native_unmod_to_array_typed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(
        ctx,
        args,
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
    )
}

pub(crate) fn native_unmod_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "toString", "()Ljava/lang/String;")
}

pub(crate) fn native_unmod_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // HIB-CV-30: for a List-shaped immutable wrapper (e.g. `List.of(..)` →
    // `cratonvm/internal/UnmodifiableList` over a synthetic ArrayList backing),
    // compute the `List.hashCode()` contract directly over the backing's
    // element array instead of delegating to `invoke_virtual(backing,
    // "hashCode")`. That delegation re-enters real `AbstractList.hashCode`
    // bytecode, which — when it hashes a GC-relocated element whose class does
    // not override `hashCode` — reads an unstable identity hash for that
    // element (0, then 1, ... per GC cycle) rather than the header value. The
    // canonical victim is JUnit 6's `Namespace.hashCode()` over
    // `List.of(extensionClassName, testInstance)`: the drift silently breaks
    // `NamespacedHierarchicalStore` lookups, which (via `@TestInstance(PER_CLASS)`)
    // makes Hibernate rebuild the EntityManagerFactory between test methods —
    // surfacing as "a cascaded association is null". `native_al_hash_code`
    // reads each element fresh from the backing array and hashes it immediately,
    // so it does not hold a stale element ref across an allocation/GC.
    if let Some(Value::Object(Some(this))) = args.first() {
        if let Some(backing) = unmod_backing(ctx, *this) {
            // Only redirect for ordered list backings; Set/Map wrappers use the
            // unordered contract and must keep delegating.
            let bcid = ctx.class_id_of_object(backing);
            let is_list = ctx
                .class_name_of_id(bcid)
                .map(|n| n == "java/util/ArrayList" || n == "java/util/LinkedList")
                .unwrap_or(false);
            if is_list {
                return native_al_hash_code(ctx, &[Value::Object(Some(backing))]);
            }
        }
    }
    unmod_delegate(ctx, args, "hashCode", "()I")
}

pub(crate) fn native_unmod_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `this.equals(other)` forwards to `backing.equals(other)`. If `other` is
    // itself an unmodifiable wrapper, it must be unwrapped to its backing
    // collection first — otherwise the backing list's `AbstractList.equals`
    // tries to iterate the *wrapper* (e.g. via `listIterator()`, which the
    // wrapper does not expose) and the comparison spuriously fails. This is
    // the JDK behaviour: `List.of(a,b).equals(List.of(a,b))` is `true`, and
    // crucially `ResourceBundle$Control.FORMAT_DEFAULT.equals(itself)` must
    // be `true` or `getNoFallbackControl` throws IllegalArgumentException.
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let backing = match unmod_backing(ctx, this) {
        Some(b) => b,
        None => return Ok(Some(Value::Object(None))),
    };
    // Unwrap the argument: walk down any chain of unmodifiable wrappers so
    // the backing's `equals` sees a concrete ArrayList / HashSet / HashMap.
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => {
            let mut cur = *o;
            while is_unmod_wrapper(ctx, cur) {
                match unmod_backing(ctx, cur) {
                    Some(b) => cur = b,
                    None => break,
                }
            }
            Value::Object(Some(cur))
        }
        v => v.cloned().unwrap_or(Value::Object(None)),
    };
    ctx.invoke_virtual(backing, "equals", "(Ljava/lang/Object;)Z", &[other])
}

pub(crate) fn native_unmod_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "forEach", "(Ljava/util/function/Consumer;)V")
}

pub(crate) fn native_unmod_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "stream", "()Ljava/util/stream/Stream;")
}

/// `Spliterator.characteristics()` for an IMMUTABLE collection, measured across
/// every shape the factories produce.
///
/// The backing's own answer is wrong for these because the JDK does not ask the
/// backing: `ImmutableCollections` builds its spliterator with its own flags,
/// and the size-1 shapes take `Collections.singletonSpliterator` instead.
/// MEASURED against HotSpot 25.0.4+7 (`apps/probes/UtilCoverageSweep`):
///
/// ```text
///   Set.of("a")            17745      List.of("a")           17745
///   Set.of()               16449      List.of()              16464
///   Set.of("a","b")        16449      List.of x3             16464
///   Set.of x3 (SetN)       16449      Map.of().entrySet()    17745  (size 1)
///   Map.of().keySet()      16449      Map.of().values()      16448
/// ```
///
/// **The discriminator is SIZE, not class.** Every 17745 is a size-1 immutable
/// -- `Set.of("a")`, `List.of("a")`, a one-entry `entrySet`, and
/// `Collections.singleton`, which this VM already answered correctly and is the
/// control that names the rule. The JDK routes those to
/// `Collections.singletonSpliterator` (the `Collections$2` a strict-mode run
/// reports); everything else goes through
/// `Spliterators.spliterator(collection, flags)`, which contributes
/// `SIZED | SUBSIZED` on top of the family's own bits.
///
/// COMPATIBLE MODE ONLY. Strict refuses `cratonvm/internal/Unmodifiable*` and
/// runs java.base's own bodies, which is why it was already 0-diff on all of
/// these -- the tenth row in this campaign where `--jdk-only` is the more
/// correct mode.
pub(crate) const SPL_IMMUTABLE_SINGLETON: i32 =
    SPL_SIZED | SPL_DISTINCT | SPL_ORDERED | SPL_NONNULL | 0x0400 | SPL_SUBSIZED;

/// The characteristics HotSpot's immutable factories answer, by shape.
///
/// MEASURED across the whole matrix (`apps/probes/ImmutableSplProbe`, HotSpot
/// 25.0.4+7) rather than derived from the cells that failed, because the
/// obvious rule -- "a size-1 immutable is a singleton" -- is contradicted by
/// the very same map:
///
/// ```text
///   Set.of("a")              17745  ImmutableCollections$Set12 / Collections$2
///   List.of("a")             17745  ImmutableCollections$List12 / Collections$2
///   Map.of("a",1).entrySet() 17745  ImmutableCollections$Set12 / Collections$2
///   Map.of("a",1).keySet()   16449  AbstractMap$1 / Spliterators$IteratorSpliterator
///   Map.of("a",1).values()   16448  AbstractMap$2 / Spliterators$IteratorSpliterator
/// ```
///
/// The CLASS column is the rule. Every 17745 is `Collections$2`, which is
/// `Collections.singletonSpliterator`, and `List12`/`Set12` route to it when
/// their second slot is the empty sentinel -- so "size 1" is right, but only
/// for a collection the factory built as a `List12`/`Set12`. A `Map.of`'s
/// keySet and values are not: they are the anonymous `AbstractMap$1`/`$2`
/// views, which carry no immutable bits at all, at any size. `entrySet` is the
/// odd one because `Map1.entrySet()` is literally `Set.of(entry)`, and a
/// two-entry map's `MapN$1` entrySet drops back to 16449.
///
/// `Collections.unmodifiableSet(hashSet)` is the control that must NOT move:
/// the JDK's WRAPPER really does hand back the backing's spliterator, so that
/// arm never reaches here.
pub(crate) fn immutable_spliterator_characteristics(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    size: i32,
    is_map_view: bool,
) -> i32 {
    let cls = ctx.class_name_arc_of_id(ctx.class_id_of_object(this));
    if is_map_view {
        // A view of an immutable map is a plain `AbstractMap` view, with one
        // exception the class name already tells apart.
        return match cls.as_deref() {
            Some(UNMOD_ENTRY_SET_CLASS) if size == 1 => SPL_IMMUTABLE_SINGLETON,
            Some(UNMOD_COLLECTION_CLASS) => SPL_SIZED | SPL_SUBSIZED,
            _ => SPL_SIZED | SPL_DISTINCT | SPL_SUBSIZED,
        };
    }
    // Size outranks the carrier here, and only here: it is the whole reason
    // `Set.of("a")` and `List.of("a")` agree at 17745 while their two- and
    // three-element siblings do not.
    if size == 1 {
        return SPL_IMMUTABLE_SINGLETON;
    }
    match cls.as_deref() {
        // A list keeps its encounter order and is not distinct.
        Some(UNMOD_LIST_CLASS) => SPL_SIZED | SPL_ORDERED | SPL_SUBSIZED,
        // A values view is neither distinct nor ordered.
        Some(UNMOD_COLLECTION_CLASS) => SPL_SIZED | SPL_SUBSIZED,
        // Sets, keySets and entrySets: distinct, no encounter order.
        _ => SPL_SIZED | SPL_DISTINCT | SPL_SUBSIZED,
    }
}

pub(crate) fn native_unmod_spliterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // An IMMUTABLE receiver -- or a view of an immutable map -- answers its own
    // family's bits; an unmodifiable WRAPPER delegates, because the JDK's
    // wrapper really does hand back the backing's spliterator
    // (`unmodifiableSet(hashSet)` is 65 on both VMs, and that row is the
    // control for leaving this arm alone).
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return unmod_delegate(ctx, args, "spliterator", "()Ljava/util/Spliterator;");
    };
    let is_map_view = unmod_is_immutable_map_view(&*ctx, this);
    if !unmod_is_immutable(&*ctx, this) && !is_map_view {
        return unmod_delegate(ctx, args, "spliterator", "()Ljava/util/Spliterator;");
    }

    let spl = unmod_delegate(ctx, args, "spliterator", "()Ljava/util/Spliterator;")?;

    // ONLY a spliterator THIS CRATE MINTED may be written to. The backing's
    // `spliterator()` is not guaranteed to reach `native_al_spliterator`: for a
    // `List.of` the delegate lands on java.base's own `ArrayList.spliterator()`
    // bytecode and hands back a REAL `ArrayList$ArrayListSpliterator`, whose
    // slot 3 is its `this$0`. Writing a characteristics mask there clobbered
    // it, and the next `estimateSize()` died in `getFence` with "Cannot read
    // field modCount because this.this$0 is null" -- which is how this was
    // found, one probe row after the change that caused it.
    //
    // `two-producers-of-one-carrier-class` again, in its most direct form: the
    // synthetic `java/util/Spliterator` and the JDK's own classes both arrive
    // here, and only one of them has a slot 3 that means what this code thinks
    // it means.
    let spl_is_ours = matches!(spl, Some(Value::Object(Some(o)))
        if ctx.class_name_arc_of_id(ctx.class_id_of_object(o)).as_deref()
            == Some("java/util/Spliterator"));

    if let (true, Some(Value::Object(Some(spl_obj)))) = (spl_is_ours, spl) {
        let size = match ctx.get_field(spl_obj, 2) {
            Value::Int(n) => n,
            _ => -1,
        };
        let chars = immutable_spliterator_characteristics(&*ctx, this, size, is_map_view);
        let hint = immutable_spliterator_display_hint(&*ctx, this, size, is_map_view);
        if ctx.object_num_fields(spl_obj) > SPL_FIELD_DISPLAY_HINT {
            ctx.set_field(spl_obj, SPL_FIELD_CHARACTERISTICS, Value::Int(chars));
            ctx.set_field(spl_obj, SPL_FIELD_DISPLAY_HINT, Value::Int(hint));
            return Ok(spl);
        }
        // The backing handed back a narrower shape (three fields, no
        // characteristics slot; or four, no display-hint slot). Widen it:
        // copy the array, cursor and length across and add both slots.
        let arr = ctx.get_field(spl_obj, 0);
        let cursor = ctx.get_field(spl_obj, 1);
        let wide = try_alloc_synthetic(ctx, "java/util/Spliterator", 5)?;
        ctx.set_field(wide, 0, arr);
        ctx.set_field(wide, 1, cursor);
        ctx.set_field(wide, 2, Value::Int(size));
        ctx.set_field(wide, SPL_FIELD_CHARACTERISTICS, Value::Int(chars));
        ctx.set_field(wide, SPL_FIELD_DISPLAY_HINT, Value::Int(hint));
        return Ok(Some(Value::Object(Some(wide))));
    }

    // The delegate handed back one of the JDK's OWN spliterators, which we may
    // not write to and whose mask is the BACKING's rather than the immutable
    // family's. `List.of("a")` is that case: java.base answers 16464 for the
    // `ArrayList` behind it where HotSpot answers 17745 for a `List12`. Mint
    // ours over the same elements instead. The rule was already right and
    // simply had nowhere to be written, which is what left this one cell open
    // after the set shapes were fixed.
    let arr = unmod_delegate(ctx, args, "toArray", "()[Ljava/lang/Object;")?;
    if let Some(Value::Object(Some(a))) = arr {
        let len = ctx.array_length(a) as i32;
        let chars = immutable_spliterator_characteristics(&*ctx, this, len, is_map_view);
        let hint = immutable_spliterator_display_hint(&*ctx, this, len, is_map_view);
        let pin = ctx.pin_native_root(a);
        // Not `?`: the pin is this frame's base and must be released before
        // unwinding, or it and everything pinned above it are stranded.
        let minted = match try_alloc_synthetic(ctx, "java/util/Spliterator", 5) {
            Ok(m) => m,
            Err(err) => {
                ctx.unpin_native_roots(pin);
                return Err(err);
            }
        };
        let a = ctx.read_native_pin(pin, a);
        ctx.set_field(minted, 0, Value::Object(Some(a)));
        ctx.set_field(minted, 1, Value::Int(0));
        ctx.set_field(minted, 2, Value::Int(len));
        ctx.set_field(minted, SPL_FIELD_CHARACTERISTICS, Value::Int(chars));
        ctx.set_field(minted, SPL_FIELD_DISPLAY_HINT, Value::Int(hint));
        ctx.unpin_native_roots(pin);
        return Ok(Some(Value::Object(Some(minted))));
    }
    Ok(spl)
}

/// `subList` returns another unmodifiable view over the backing sub-list.
pub(crate) fn native_unmod_sub_list(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let sub = unmod_delegate(ctx, args, "subList", "(II)Ljava/util/List;")?;
    if let Some(Value::Object(Some(inner))) = sub {
        let w = alloc_unmod_wrapper(ctx, UNMOD_LIST_CLASS, inner)?;
        return Ok(Some(Value::Object(Some(w))));
    }
    Ok(sub)
}

/// `CRATONVM_COMPAT_UNMOD_ITERATOR_JDK` -- default ON, `=0` restores the
/// `toArray()` snapshot. JIT round 16 (lane compatdrain),
/// `r15w3-compat3-collection-review-findings-CLOSED-20261001.md` item 6:
/// `Collections.unmodifiableList(l).iterator()` delegates to `l.iterator()`
/// live, as JDK 25's inherited `UnmodifiableCollection.iterator()` does. The
/// same switch makes `native-builtins`' `getClass()` name the
/// `UnmodifiableItr` carrier `Collections$UnmodifiableCollection$1`.
pub(crate) fn unmod_iterator_jdk_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_UNMOD_ITERATOR_JDK")
}

/// `iterator()` returns a read-only iterator.
pub(crate) fn native_unmod_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let is_list_wrapper = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .map(|n| n == UNMOD_LIST_CLASS)
        .unwrap_or(false);
    // JIT round 16 (lane compatdrain), r15w3-compat3 item 6: JDK 25's
    // `UnmodifiableList` inherits `UnmodifiableCollection.iterator()`, a LIVE
    // wrapper over the backing's own `c.iterator()` (`Collections$
    // UnmodifiableCollection$1`; HotSpot sees a change made during the walk,
    // or throws the backing's CME). Only the immutable factories' lists keep
    // the snapshot: their backing never changes.
    let live_list_iterator =
        is_list_wrapper && unmod_iterator_jdk_enabled() && !unmod_is_immutable(&*ctx, this);
    if is_list_wrapper && !live_list_iterator {
        let snapshot = match unmod_list_snapshot(ctx, args) {
            Some(a) => a,
            None => return Ok(Some(Value::Object(None))),
        };
        let immutable = matches!(args.first(), Some(Value::Object(Some(w)))
            if unmod_is_immutable(&*ctx, *w));
        return Ok(Some(Value::Object(Some(alloc_unmod_list_itr(
            ctx, snapshot, 0, immutable, false,
        )?))));
    }

    let inner = unmod_delegate(ctx, args, "iterator", "()Ljava/util/Iterator;")?;
    if let Some(Value::Object(Some(itr))) = inner {
        let w = alloc_unmod_wrapper(ctx, UNMOD_ITR_CLASS, itr)?;
        return Ok(Some(Value::Object(Some(w))));
    }
    Ok(inner)
}

pub(crate) fn native_unmod_itr_has_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "hasNext", "()Z")
}

pub(crate) fn native_unmod_itr_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "next", "()Ljava/lang/Object;")
}

/// `UnmodifiableEntrySet.iterator()` — like `native_unmod_iterator`, but
/// wraps the backing iterator in `UnmodifiableEntryItr` so each yielded
/// `Map.Entry` is itself wrapped (see `native_unmod_entry_itr_next`).
pub(crate) fn native_unmod_entry_set_iterator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let inner = unmod_delegate(ctx, args, "iterator", "()Ljava/util/Iterator;")?;
    if let Some(Value::Object(Some(itr))) = inner {
        let w = alloc_unmod_wrapper(ctx, UNMOD_ENTRY_ITR_CLASS, itr)?;
        return Ok(Some(Value::Object(Some(w))));
    }
    Ok(inner)
}

/// `UnmodifiableEntryItr.next()` — wraps the backing iterator's `Map.Entry`
/// in `UnmodifiableMapEntry` so a caller's `setValue()` throws instead of
/// writing through to the backing map (the `Map.replaceAll` default method
/// iterates `entrySet()` and calls `entry.setValue(...)` directly, so an
/// unwrapped entry here silently defeats `ParameterMap`'s lock).
pub(crate) fn native_unmod_entry_itr_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let next = unmod_delegate(ctx, args, "next", "()Ljava/lang/Object;")?;
    if let Some(Value::Object(Some(entry))) = next {
        let w = alloc_unmod_wrapper(ctx, UNMOD_MAP_ENTRY_CLASS, entry)?;
        return Ok(Some(Value::Object(Some(w))));
    }
    Ok(next)
}

/// `UnmodifiableEntrySet.forEach(action)` — wraps each entry the same way
/// the iterator does before invoking the consumer, so `action.accept(entry)`
/// calling `entry.setValue(...)` still throws instead of mutating the
/// backing map.
pub(crate) fn native_unmod_entry_set_for_each(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let (this, consumer) = match (args.first(), args.get(1)) {
        (Some(Value::Object(Some(t))), Some(Value::Object(Some(c)))) => (*t, *c),
        _ => return Ok(None),
    };
    let backing = match unmod_backing(ctx, this) {
        Some(b) => b,
        None => return Ok(None),
    };
    // gc-common w20-a: `consumer` is a bare copy of the argument and every
    // turn below runs Java and allocates; it used to be handed to `accept`
    // through its entry address. Pinned first, so it is the frame's base.
    let consumer_pin = ctx.pin_native_root(consumer);
    let itr = match ctx.invoke_virtual(backing, "iterator", "()Ljava/util/Iterator;", &[]) {
        Ok(Some(Value::Object(Some(i)))) => i,
        Ok(_) => {
            ctx.unpin_native_roots(consumer_pin);
            return Ok(None);
        }
        Err(e) => {
            ctx.unpin_native_roots(consumer_pin);
            return Err(e);
        }
    };
    let itr_pin = ctx.pin_native_root(itr);
    let outcome = (|| -> Result<(), MethodCallFailed> {
        loop {
            let itr_now = ctx.read_native_pin(itr_pin, itr);
            let has_next = matches!(
                ctx.invoke_virtual(itr_now, "hasNext", "()Z", &[])?,
                Some(Value::Int(n)) if n != 0
            );
            if !has_next {
                return Ok(());
            }
            // `hasNext` ran Java: re-read the iterator before `next`.
            let itr_now = ctx.read_native_pin(itr_pin, itr);
            let entry = match ctx.invoke_virtual(itr_now, "next", "()Ljava/lang/Object;", &[])? {
                Some(Value::Object(Some(e))) => e,
                _ => return Ok(()),
            };
            let wrapped = alloc_unmod_wrapper(ctx, UNMOD_MAP_ENTRY_CLASS, entry)?;
            let consumer_now = ctx.read_native_pin(consumer_pin, consumer);
            ctx.invoke_virtual(
                consumer_now,
                "accept",
                "(Ljava/lang/Object;)V",
                &[Value::Object(Some(wrapped))],
            )?;
        }
    })();
    ctx.unpin_native_roots(consumer_pin);
    outcome.map(|()| None)
}

/// Slot layout of a `UnmodifiableListItr`.
pub(crate) const UNMOD_LIST_ITR_SNAPSHOT: usize = 0;
pub(crate) const UNMOD_LIST_ITR_CURSOR: usize = 1;
/// Whether the wrapper this iterator walks came from an immutable factory
/// (`List.of`/`copyOf`) rather than `Collections.unmodifiableList` — copied
/// from the wrapper's own [`UNMOD_FIELD_IMMUTABLE`] at mint time, because the
/// iterator carries only a snapshot `Object[]` and loses the wrapper once
/// built. Read by `getclass_display_class_id`'s `CollListItr` arm
/// (native-builtins) to pick `ImmutableCollections$ListItr` vs one of the
/// `Collections$Unmodifiable*` names — see [`UNMOD_LIST_ITR_VIA_LIST_ITERATOR`]
/// for the other half of that choice. CONTRACT: keep the slot index in sync
/// with that reader.
pub(crate) const UNMOD_LIST_ITR_IMMUTABLE: usize = 2;
/// Whether this iterator was minted by `listIterator()`/`listIterator(int)`
/// rather than `iterator()`. Both entry points share this one carrier class —
/// `native_unmod_iterator`'s list-wrapper branch and
/// `native_unmod_list_iterator[_idx]` all bottom out in
/// [`alloc_unmod_list_itr`] — but HotSpot hands out two different classes for
/// an `unmodifiableList` wrapper (`Collections$UnmodifiableCollection$1` for
/// `iterator()`, `Collections$UnmodifiableList$1` for `listIterator()`; a
/// `List.of` wrapper answers `ImmutableCollections$ListItr` either way, so
/// this bit is only consulted when [`UNMOD_LIST_ITR_IMMUTABLE`] is unset).
/// Measured: `apps/probes/ItrNames` against HotSpot 25. CONTRACT: keep the
/// slot index in sync with `getclass_display_class_id`'s `CollListItr` arm.
pub(crate) const UNMOD_LIST_ITR_VIA_LIST_ITERATOR: usize = 3;

/// Snapshot the elements of an unmodifiable-list wrapper into an `Object[]`,
/// by delegating to the backing collection's `toArray()`.
pub(crate) fn unmod_list_snapshot(ctx: &mut dyn NativeContext, args: &[Value]) -> Option<ObjectRef> {
    match unmod_delegate(ctx, args, "toArray", "()[Ljava/lang/Object;") {
        Ok(Some(Value::Object(Some(arr)))) => Some(arr),
        _ => None,
    }
}

/// Allocate a read-only `ListIterator` over `snapshot`, positioned at `cursor`.
/// Fallible since 2026-08-05 (JDK-only wave 2, lane L7) — see
/// [`alloc_unmod_wrapper`] for why these views are refused under `--jdk-only`.
///
/// `immutable` and `via_list_iterator` are display-only (see the two constants
/// above) and change nothing about how this iterator walks or fails.
pub(crate) fn alloc_unmod_list_itr(
    ctx: &mut dyn NativeContext,
    snapshot: ObjectRef,
    cursor: i32,
    immutable: bool,
    via_list_iterator: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    // GC-SAFETY: same contract as `make_iterator_from_array` — the shell
    // allocation can relocate the snapshot. Covered by
    // `gc_native_pins::unmodifiable_list_iterator_roots_snapshot_graph_across_allocation`.
    let snapshot_pin = ctx.pin_native_root(snapshot);
    // Not `?`: `snapshot_pin` is this frame's pin base and must be released
    // before unwinding.
    let it = match try_alloc_synthetic(ctx, UNMOD_LIST_ITR_CLASS, 4) {
        Ok(it) => it,
        Err(err) => {
            ctx.unpin_native_roots(snapshot_pin);
            return Err(err);
        }
    };
    let it_pin = ctx.pin_native_root(it);
    let it = ctx.read_native_pin(it_pin, it);
    let snapshot = ctx.read_native_pin(snapshot_pin, snapshot);
    ctx.set_field(it, UNMOD_LIST_ITR_SNAPSHOT, Value::Object(Some(snapshot)));
    let it = ctx.read_native_pin(it_pin, it);
    ctx.set_field(it, UNMOD_LIST_ITR_CURSOR, Value::Int(cursor));
    let it = ctx.read_native_pin(it_pin, it);
    ctx.set_field(it, UNMOD_LIST_ITR_IMMUTABLE, Value::Int(immutable as i32));
    let it = ctx.read_native_pin(it_pin, it);
    ctx.set_field(
        it,
        UNMOD_LIST_ITR_VIA_LIST_ITERATOR,
        Value::Int(via_list_iterator as i32),
    );
    let it = ctx.read_native_pin(it_pin, it);
    ctx.unpin_native_roots(snapshot_pin);
    Ok(it)
}

/// `listIterator()` returns a read-only `ListIterator` over a `toArray()`
/// SNAPSHOT of the backing list, not the live list HotSpot's
/// `Collections$UnmodifiableList$1` walks (CD16-4). Unobservable for an
/// immutable-factory wrapper (its backing never changes); for a
/// `Collections.unmodifiableList` wrapper it is reached only where the
/// stand-in itself is: `--compatible` with `-jdk-unmodifiable-views`, or an
/// image without the JDK's bytecode. Over a real JDK both modes run the JDK's
/// factory and views (`j27_unmodifiable_view_factory_registration_tests`;
/// `docs/internal/jit/j27-unmodifiable-views-snapshot-iterators-FIXED-20261029.md`).
pub(crate) fn native_unmod_list_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let snapshot = match unmod_list_snapshot(ctx, args) {
        Some(a) => a,
        None => return Ok(Some(Value::Object(None))),
    };
    let immutable = matches!(args.first(), Some(Value::Object(Some(w)))
        if unmod_is_immutable(&*ctx, *w));
    Ok(Some(Value::Object(Some(alloc_unmod_list_itr(
        ctx, snapshot, 0, immutable, true,
    )?))))
}

/// `listIterator(int)` returns a read-only `ListIterator` positioned at `index`.
pub(crate) fn native_unmod_list_iterator_idx(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let snapshot = match unmod_list_snapshot(ctx, args) {
        Some(a) => a,
        None => return Ok(Some(Value::Object(None))),
    };
    let len = ctx.array_length(snapshot) as i32;
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let immutable = matches!(args.first(), Some(Value::Object(Some(this)))
        if unmod_is_immutable(&*ctx, *this));
    if index < 0 || index > len {
        // The JDK throws the PLAIN `IndexOutOfBoundsException`. Throwing the
        // subclass because "a `catch (IndexOutOfBoundsException)` still catches
        // it" was the old rationale here, and it is the wrong way round: the
        // subclass passes every catch the superclass would, and fails every
        // test of the class itself.
        //
        // The wording follows the same split as `unmod_list_oob_error` — a
        // SPACE for the immutable factories ("Index: 5 Size: 2"),
        // `ArrayList`/`LinkedList`'s comma for an unmodifiable view.
        let message = if immutable {
            format!("Index: {index} Size: {len}")
        } else {
            format!("Index: {index}, Size: {len}")
        };
        return Err(RuntimeError::ioobe(message).into());
    }
    Ok(Some(Value::Object(Some(alloc_unmod_list_itr(
        ctx, snapshot, index, immutable, true,
    )?))))
}

/// Read the (snapshot, cursor) state out of a list-iterator wrapper.
pub(crate) fn unmod_list_itr_state(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Option<(ObjectRef, i32, i32)> {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    let snapshot = match ctx.get_field(this, UNMOD_LIST_ITR_SNAPSHOT) {
        Value::Object(Some(a)) => a,
        _ => return None,
    };
    let cursor = match ctx.get_field(this, UNMOD_LIST_ITR_CURSOR) {
        Value::Int(c) => c,
        _ => 0,
    };
    let len = ctx.array_length(snapshot) as i32;
    Some((snapshot, cursor, len))
}

pub(crate) fn native_unmod_listitr_has_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match unmod_list_itr_state(ctx, args) {
        Some((_, cursor, len)) => Ok(Some(Value::Int(if cursor < len { 1 } else { 0 }))),
        None => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_unmod_listitr_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (this, snapshot, cursor, len) = match (args.first(), unmod_list_itr_state(ctx, args)) {
        (Some(Value::Object(Some(o))), Some((s, c, l))) => (*o, s, c, l),
        _ => {
            return Err(RuntimeError::NoSuchElementException {
                message: String::new(),
            }
            .into());
        }
    };
    if cursor >= len {
        return Err(RuntimeError::NoSuchElementException {
            message: String::new(),
        }
        .into());
    }
    let elem = ctx.get_array_element(snapshot, cursor as usize);
    ctx.set_field(this, UNMOD_LIST_ITR_CURSOR, Value::Int(cursor + 1));
    Ok(Some(elem))
}

pub(crate) fn native_unmod_listitr_has_previous(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match unmod_list_itr_state(ctx, args) {
        Some((_, cursor, _)) => Ok(Some(Value::Int(if cursor > 0 { 1 } else { 0 }))),
        None => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_unmod_listitr_previous(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (this, snapshot, cursor, _) = match (args.first(), unmod_list_itr_state(ctx, args)) {
        (Some(Value::Object(Some(o))), Some((s, c, l))) => (*o, s, c, l),
        _ => {
            return Err(RuntimeError::NoSuchElementException {
                message: String::new(),
            }
            .into());
        }
    };
    if cursor <= 0 {
        return Err(RuntimeError::NoSuchElementException {
            message: String::new(),
        }
        .into());
    }
    let idx = cursor - 1;
    let elem = ctx.get_array_element(snapshot, idx as usize);
    ctx.set_field(this, UNMOD_LIST_ITR_CURSOR, Value::Int(idx));
    Ok(Some(elem))
}

pub(crate) fn native_unmod_listitr_next_index(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match unmod_list_itr_state(ctx, args) {
        Some((_, cursor, _)) => Ok(Some(Value::Int(cursor))),
        None => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_unmod_listitr_previous_index(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match unmod_list_itr_state(ctx, args) {
        Some((_, cursor, _)) => Ok(Some(Value::Int(cursor - 1))),
        None => Ok(Some(Value::Int(-1))),
    }
}

pub(crate) fn native_unmod_listitr_for_each_remaining(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let (this, snapshot, mut cursor, len) = match (args.first(), unmod_list_itr_state(ctx, args)) {
        (Some(Value::Object(Some(o))), Some((s, c, l))) => (*o, s, c, l),
        _ => return Ok(None),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => return Ok(None),
    };
    let this_pin = ctx.pin_native_root(this);
    let snapshot_pin = ctx.pin_native_root(snapshot);
    let consumer_pin = ctx.pin_native_root(consumer);
    while cursor < len {
        let this = ctx.read_native_pin(this_pin, this);
        let snapshot = ctx.read_native_pin(snapshot_pin, snapshot);
        let elem = ctx.get_array_element(snapshot, cursor as usize);
        let elem_pin = match elem {
            Value::Object(Some(o)) => ctx.pin_native_root(o),
            _ => usize::MAX,
        };
        cursor += 1;
        ctx.set_field(this, UNMOD_LIST_ITR_CURSOR, Value::Int(cursor));
        let consumer = ctx.read_native_pin(consumer_pin, consumer);
        let elem = read_pinned_elem(ctx, elem_pin, elem);
        if let Err(e) = ctx.invoke_virtual(consumer, "accept", "(Ljava/lang/Object;)V", &[elem]) {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
        if elem_pin != usize::MAX {
            ctx.unpin_native_roots(elem_pin);
        }
    }
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

// ---- Map delegations ------------------------------------------------------

pub(crate) fn native_unmod_map_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reject_null_immutable_query(&*ctx, args, 1)?;
    unmod_delegate(ctx, args, "get", "(Ljava/lang/Object;)Ljava/lang/Object;")
}

pub(crate) fn native_unmod_map_get_or_default(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    unmod_delegate(
        ctx,
        args,
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    )
}

pub(crate) fn native_unmod_map_contains_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reject_null_immutable_query(&*ctx, args, 1)?;
    unmod_delegate(ctx, args, "containsKey", "(Ljava/lang/Object;)Z")
}

pub(crate) fn native_unmod_map_contains_value_checked(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    reject_null_immutable_query(&*ctx, args, 1)?;
    native_unmod_map_contains_value(ctx, args)
}

pub(crate) fn native_unmod_map_contains_value(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    unmod_delegate(ctx, args, "containsValue", "(Ljava/lang/Object;)Z")
}

pub(crate) fn native_unmod_map_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "forEach", "(Ljava/util/function/BiConsumer;)V")
}

/// `keySet()` returns an unmodifiable Set view of the backing map's key set.
pub(crate) fn native_unmod_map_key_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let from_immutable = matches!(args.first(), Some(Value::Object(Some(m)))
        if unmod_is_immutable(&*ctx, *m));
    let ks = unmod_delegate(ctx, args, "keySet", "()Ljava/util/Set;")?;
    if let Some(Value::Object(Some(inner))) = ks {
        let w = alloc_unmod_view_wrapper(ctx, UNMOD_SET_CLASS, inner, from_immutable)?;
        return Ok(Some(Value::Object(Some(w))));
    }
    Ok(ks)
}

/// `values()` returns an unmodifiable Collection view of the backing values.
pub(crate) fn native_unmod_map_values(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let from_immutable = matches!(args.first(), Some(Value::Object(Some(m)))
        if unmod_is_immutable(&*ctx, *m));
    let vs = unmod_delegate(ctx, args, "values", "()Ljava/util/Collection;")?;
    if let Some(Value::Object(Some(inner))) = vs {
        let w = alloc_unmod_view_wrapper(ctx, UNMOD_COLLECTION_CLASS, inner, from_immutable)?;
        return Ok(Some(Value::Object(Some(w))));
    }
    Ok(vs)
}

/// `entrySet()` returns an unmodifiable Set view of the backing entry set.
/// Uses `UNMOD_ENTRY_SET_CLASS` (not the plain `UNMOD_SET_CLASS`) so its
/// iterator/forEach wrap each yielded `Map.Entry` — a raw entry from the
/// backing map would let `entry.setValue(...)` mutate through the
/// "unmodifiable" view instead of throwing.
pub(crate) fn native_unmod_map_entry_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let from_immutable = matches!(args.first(), Some(Value::Object(Some(m)))
        if unmod_is_immutable(&*ctx, *m));
    let es = unmod_delegate(ctx, args, "entrySet", "()Ljava/util/Set;")?;
    if let Some(Value::Object(Some(inner))) = es {
        let w = alloc_unmod_view_wrapper(ctx, UNMOD_ENTRY_SET_CLASS, inner, from_immutable)?;
        return Ok(Some(Value::Object(Some(w))));
    }
    Ok(es)
}

// ===========================================================================
// Phase 39: Collections extras — unmodifiableMap/Set, emptyMap/Set/Iterator,
//           frequency, disjoint, singleton, Enumeration stubs
// ===========================================================================

pub(crate) fn register_collections_extras_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/util/Collections";
    r.register(
        c,
        "emptyMap",
        "()Ljava/util/Map;",
        native_collections_empty_map,
    );
    r.register(
        c,
        "emptySet",
        "()Ljava/util/Set;",
        native_collections_empty_set,
    );
    r.register(
        c,
        "emptyIterator",
        "()Ljava/util/Iterator;",
        native_collections_empty_iterator,
    );
    r.register(
        c,
        "singleton",
        "(Ljava/lang/Object;)Ljava/util/Set;",
        native_collections_singleton,
    );
    r.register(
        c,
        "singletonMap",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;",
        native_collections_singleton_map,
    );
    // The six `unmodifiable*` factories only, in their own window — this
    // registrar also holds `emptyMap` / `singleton*` / `synchronized*`, which
    // are a different question. See `register_unmodifiable_natives` for why
    // this family has a real-bytecode fallback and the iterator family does
    // not (JDK-only wave 2, lane L7 residual R1, 2026-08-05).
    let __unmod_prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        c,
        "unmodifiableMap",
        "(Ljava/util/Map;)Ljava/util/Map;",
        native_collections_unmodifiable_map,
    );
    r.register(
        c,
        "unmodifiableSet",
        "(Ljava/util/Set;)Ljava/util/Set;",
        native_collections_unmodifiable_set,
    );
    // Keep the real-JDK bytecode authoritative for the sorted/navigable map
    // factories. The plain synthetic UnmodifiableMap stamp implements Map,
    // not SortedMap/NavigableMap; routing these factories to the plain-map
    // callback therefore violates their declared return type and makes
    // Charset.availableCharsets() fail its checkcast. The JDK implementations
    // create the correctly typed, read-only wrapper classes.
    r.register(
        c,
        "unmodifiableSortedSet",
        "(Ljava/util/SortedSet;)Ljava/util/SortedSet;",
        native_collections_unmodifiable_sorted_set,
    );
    r.register(
        c,
        "unmodifiableNavigableSet",
        "(Ljava/util/NavigableSet;)Ljava/util/NavigableSet;",
        native_collections_unmodifiable_navigable_set,
    );
    r.register(
        c,
        "unmodifiableCollection",
        "(Ljava/util/Collection;)Ljava/util/Collection;",
        native_collections_unmodifiable_collection,
    );
    r.set_category(__unmod_prev_cat);
    r.register(
        c,
        "synchronizedList",
        "(Ljava/util/List;)Ljava/util/List;",
        native_collections_synchronized_list,
    );
    r.register(
        c,
        "synchronizedMap",
        "(Ljava/util/Map;)Ljava/util/Map;",
        native_collections_synchronized_map,
    );
    r.register(
        c,
        "synchronizedSet",
        "(Ljava/util/Set;)Ljava/util/Set;",
        native_collections_synchronized_set,
    );
    r.register(
        c,
        "synchronizedCollection",
        "(Ljava/util/Collection;)Ljava/util/Collection;",
        native_collections_synchronized_collection,
    );
    r.register(
        c,
        "frequency",
        "(Ljava/util/Collection;Ljava/lang/Object;)I",
        native_collections_frequency,
    );
    // `Collections.disjoint` deliberately NOT registered: the previous native
    // (`native_collections_disjoint`) only read ArrayList-backed collections via
    // `al_state` and bailed to "disjoint = true" for any other Collection — so
    // `Collections.disjoint(hashSetA, hashSetB)` always claimed no overlap even
    // when the sets shared elements (keycloak DisclosureRedList red-list guard
    // never fired). The real pure-Java `Collections.disjoint` iterates one
    // collection and calls `contains` on the other; both work for HashSet on
    // CratonVM, so we let the real bytecode run. (Two more stale stubs lived in
    // native-builtins phases_early/phases_late — also removed.)
    r.register(
        c,
        "max",
        "(Ljava/util/Collection;)Ljava/lang/Object;",
        native_collections_max,
    );
    r.register(
        c,
        "min",
        "(Ljava/util/Collection;)Ljava/lang/Object;",
        native_collections_min,
    );
    r.register(c, "swap", "(Ljava/util/List;II)V", native_collections_swap);
    r.register(
        c,
        "fill",
        "(Ljava/util/List;Ljava/lang/Object;)V",
        native_collections_fill,
    );
    r.register(
        c,
        "nCopies",
        "(ILjava/lang/Object;)Ljava/util/List;",
        native_collections_n_copies,
    );
    r.register(
        c,
        "shuffle",
        "(Ljava/util/List;)V",
        native_collections_shuffle,
    );
    r.register(
        c,
        "addAll",
        "(Ljava/util/Collection;[Ljava/lang/Object;)Z",
        native_collections_add_all,
    );
    // List.copyOf / Set.copyOf / Map.copyOf — `SyntheticStub` for the same
    // reason as `unmodifiableList` above: each returns a `cratonvm/internal/*`
    // stand-in that `--jdk-only` refuses to fabricate, and each is a duplicate
    // of a registration `register_factory_natives` already retagged.
    //
    // `CRATONVM_COMPAT_JDK_IMMUTABLES` (read once here, per VM): not registered
    // at all, so `--compatible` runs the JDK's `ImmutableCollections.listCopy`
    // / `setCopy` / `Map.copyOf` bodies and hands out `List12` / `ListN` /
    // `Set12` / `SetN` / `Map1` / `MapN`, as HotSpot and `--jdk-only` do.
    // Measured first, as for `List.of` in wave 48: a real immutable collection
    // already answers every row of `L6W49CopyOfIsTheJdks` under `--compatible`
    // (invokeinterface reaches its own bytecode). Interpreter round i1 wave 49,
    // lane L6: stage 4 of
    // `docs/feature-designs/jdk-only-mode.md`.
    // The HotSpot-alignment row `jdk-immutables` since interpreter round i6
    // wave 118, lane L5.
    if !r.aligned(cratonvm_types::HotSpotAlignment::CompatJdkImmutables) {
        r.register_with_kind(
            "java/util/List",
            "copyOf",
            "(Ljava/util/Collection;)Ljava/util/List;",
            native_list_copy_of,
            cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register_with_kind(
            "java/util/Set",
            "copyOf",
            "(Ljava/util/Collection;)Ljava/util/Set;",
            native_set_copy_of,
            cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register_with_kind(
            "java/util/Map",
            "copyOf",
            "(Ljava/util/Map;)Ljava/util/Map;",
            native_map_copy_of,
            cratonvm_native_api::NativeKind::SyntheticStub,
        );
    }

    // Enumeration interface
    let en = "java/util/Enumeration";
    r.register(en, "hasMoreElements", "()Z", native_snapshot_itr_has_next);
    r.register(
        en,
        "nextElement",
        "()Ljava/lang/Object;",
        native_snapshot_itr_next,
    );

    // Snapshot Enumeration used by `Properties.propertyNames()` and similar
    // call sites that need a real Enumeration over a fixed key list. We can't
    // reuse `Collections$EmptyEnumeration` because its real-JDK bytecode for
    // `hasMoreElements` is hardcoded to `iconst_0; ireturn` and wins concrete
    // dispatch over the interface-level Enumeration native, so a non-empty
    // snapshot built on EmptyEnumeration appears empty. Register concrete
    // overrides on a dedicated synthetic class.
    let sne = "cratonvm/internal/SnapshotEnumeration";
    r.register(sne, "hasMoreElements", "()Z", native_snapshot_itr_has_next);
    r.register(
        sne,
        "nextElement",
        "()Ljava/lang/Object;",
        native_snapshot_itr_next,
    );
    r.set_category(__prev_cat);
}

/// Identity — just returns the first argument (used for synchronized wrappers)
pub(crate) fn native_collections_identity(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    Ok(Some(args.first().cloned().unwrap_or(Value::Object(None))))
}

/// `Collections.synchronizedMap(m)` — wrap in a REAL `Collections$SynchronizedMap`
/// so every method (notably `computeIfAbsent`) is `synchronized (mutex)`. The old
/// identity stub returned the bare map, making `synchronizedMap(...).computeIfAbsent`
/// non-atomic under concurrency (duplicate values for the same key) — which broke
/// Spring's `ConcurrencyLimitBeanPostProcessor` per-proxy throttle cache. Null
/// passes through.
pub(crate) fn native_collections_synchronized_map(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(v @ Value::Object(Some(_))) => ctx.new_object_initialized(
            "java/util/Collections$SynchronizedMap",
            "(Ljava/util/Map;)V",
            &[v.clone()],
        ),
        other => Ok(Some(other.cloned().unwrap_or(Value::Object(None)))),
    }
}

/// `Collections.synchronizedList(l)` — real `SynchronizedList`, or
/// `SynchronizedRandomAccessList` when the backing implements `RandomAccess`.
/// Same rationale as [`native_collections_synchronized_map`].
pub(crate) fn native_collections_synchronized_list(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    reject_null_collections_arg(args.first())?;
    match args.first() {
        Some(v @ Value::Object(Some(list))) => {
            let is_random_access = match ctx.class_id_by_name("java/util/RandomAccess") {
                Some(ra) => ctx.is_subclass(ctx.class_id_of_object(*list), ra),
                None => false,
            };
            let cls = if is_random_access {
                "java/util/Collections$SynchronizedRandomAccessList"
            } else {
                "java/util/Collections$SynchronizedList"
            };
            ctx.new_object_initialized(cls, "(Ljava/util/List;)V", &[v.clone()])
        }
        other => Ok(Some(other.cloned().unwrap_or(Value::Object(None)))),
    }
}

/// `Collections.synchronizedSet(s)` — real `SynchronizedSet`.
pub(crate) fn native_collections_synchronized_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(v @ Value::Object(Some(_))) => ctx.new_object_initialized(
            "java/util/Collections$SynchronizedSet",
            "(Ljava/util/Set;)V",
            &[v.clone()],
        ),
        other => Ok(Some(other.cloned().unwrap_or(Value::Object(None)))),
    }
}

/// `Collections.synchronizedCollection(c)` — real `SynchronizedCollection`.
pub(crate) fn native_collections_synchronized_collection(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(v @ Value::Object(Some(_))) => ctx.new_object_initialized(
            "java/util/Collections$SynchronizedCollection",
            "(Ljava/util/Collection;)V",
            &[v.clone()],
        ),
        other => Ok(Some(other.cloned().unwrap_or(Value::Object(None)))),
    }
}

// `List.copyOf` / `Set.copyOf` / `Map.copyOf` — return an INDEPENDENT immutable
// snapshot of the source (JDK contract), NOT the source itself. The previous
// identity impl aliased the source: a later mutation of the source leaked into
// the "copy". Surfaced as Kafka AdminApiDriver, whose `RequestSpec.keys` is
// built via `Set.copyOf(scopeKeys)` — when the driver later mapped another key
// to the same broker, the already-returned spec's key set gained that key
// (`{bar}` -> `{bar, foo}`). Fix: build a fresh backing collection from the
// source's elements, then wrap it unmodifiable so reads see the snapshot and
// mutators throw.
pub(crate) fn native_list_copy_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `List.copyOf(null)` is `ImmutableCollections.listCopy(coll)`, whose
    // `coll.isEmpty()` dereferences it. A usable empty list for a null argument
    // is the fabricated-success shape.
    reject_null_copy_of_arg(
        args.first(),
        "Cannot invoke \"java.util.Collection.isEmpty()\" because \"coll\" is null",
    )?;
    let src = args.first().copied().unwrap_or(Value::Object(None));
    // `List.copyOf(x) == x` when `x` is ALREADY immutable — not an optimisation
    // the JDK left implicit: `ImmutableCollections.listCopy` returns its
    // argument, and application code compares the two with `==`. The
    // independent-snapshot rule above still holds for everything else, which is
    // what `already_immutable_of` is careful about.
    if let Some(same) = already_immutable_of(ctx, src, UNMOD_LIST_CLASS) {
        return Ok(Some(Value::Object(Some(same))));
    }
    // C4-3: `listCopy`'s `coll.isEmpty()` -> `List.of()`.
    let (empty, mut src) = copy_of_empty_pinned(ctx, src, UNMOD_LIST_CLASS)?;
    if let Some(empty) = empty {
        return Ok(Some(Value::Object(Some(empty))));
    }
    // CW2-5: `List.of(coll.toArray())`.
    if let Some(r) = list_copy_of_one_pass(ctx, &mut src) {
        return r;
    }
    // GC-safety: the allocation and the copy-in below both collect, and `src`
    // and `backing` are bare locals read after each. Same idiom, and the same
    // reason, as `native_map_copy_of` — which had it and these two did not.
    //
    // Measured, not inferred: on BindableTests under `CRATONVM_DBG_GC_STRESS`
    // `native_al_init_from_collection` relocated `backing`, and the pre-move
    // address went into the wrapper's slot 0 and out to the program. See
    // `docs/feature-designs/jdk-only-mode.md`'s BindableTests residual page.
    //
    // The pin goes up BEFORE the null scan: `source_collection_has_null` runs
    // the source's `toArray()` (Java), and pinning after it pinned an address
    // that call may already have moved (round 15 wave 2, lane compat).
    let src_pin = pin_value(ctx, src);
    if let Value::Object(Some(s)) = src {
        if source_collection_has_null(ctx, s) {
            if src_pin != usize::MAX {
                ctx.unpin_native_roots(src_pin);
            }
            return Err(MethodCallFailed::from(RuntimeError::NullPointerException {
                message: None,
            }));
        }
    }
    let mut backing = try_alloc_synthetic(ctx, "java/util/ArrayList", AL_NUM_FIELDS)?;
    let src = read_pinned_elem(ctx, src_pin, src);
    let backing_at_call = backing;
    let copied = rooted_across(ctx, &mut [&mut backing], |ctx| {
        native_al_init_from_collection(ctx, &[Value::Object(Some(backing_at_call)), src])
    });
    if src_pin != usize::MAX {
        ctx.unpin_native_roots(src_pin);
    }
    copied?;
    Ok(Some(Value::Object(Some(alloc_immutable_wrapper(
        ctx,
        UNMOD_LIST_CLASS,
        backing,
    )?))))
}

pub(crate) fn native_set_copy_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reject_null_copy_of_arg(
        args.first(),
        "Cannot invoke \"java.util.Collection.isEmpty()\" because \"coll\" is null",
    )?;
    let src = args.first().copied().unwrap_or(Value::Object(None));
    // Identity for an already-immutable source, as `List.copyOf`. NOTE the
    // asymmetry with `Set.of`: `copyOf` DEDUPLICATES and must not throw on a
    // repeat (`Set.copyOf(List.of("a","a")).size() == 1`); only nulls are
    // rejected.
    if let Some(same) = already_immutable_of(ctx, src, UNMOD_SET_CLASS) {
        return Ok(Some(Value::Object(Some(same))));
    }
    // C4-3: `Set.copyOf`'s `coll.isEmpty()` -> `Set.of()`.
    let (empty, src) = copy_of_empty_pinned(ctx, src, UNMOD_SET_CLASS)?;
    if let Some(empty) = empty {
        return Ok(Some(Value::Object(Some(empty))));
    }
    // CW2-5: `Set.of(new HashSet<>(coll).toArray())`.
    if copyof_one_pass_enabled() {
        return set_copy_of_one_pass(ctx, src);
    }
    // GC-safety: see `native_list_copy_of` — identical shape, identical fix,
    // the pin ahead of the null scan's `toArray()` included.
    let src_pin = pin_value(ctx, src);
    if let Value::Object(Some(s)) = src {
        if source_collection_has_null(ctx, s) {
            if src_pin != usize::MAX {
                ctx.unpin_native_roots(src_pin);
            }
            return Err(MethodCallFailed::from(RuntimeError::NullPointerException {
                message: None,
            }));
        }
    }
    let mut backing = try_alloc_synthetic(ctx, "java/util/HashSet", HS_NUM_FIELDS)?;
    let src = read_pinned_elem(ctx, src_pin, src);
    let backing_at_call = backing;
    let copied = rooted_across(ctx, &mut [&mut backing], |ctx| {
        native_hs_init_from_collection(ctx, &[Value::Object(Some(backing_at_call)), src])
    });
    if src_pin != usize::MAX {
        ctx.unpin_native_roots(src_pin);
    }
    copied?;
    Ok(Some(Value::Object(Some(alloc_immutable_wrapper(
        ctx,
        UNMOD_SET_CLASS,
        backing,
    )?))))
}

pub(crate) fn native_map_copy_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reject_null_copy_of_arg(
        args.first(),
        "Cannot invoke \"java.util.Map.isEmpty()\" because \"map\" is null",
    )?;
    let src = args.first().copied().unwrap_or(Value::Object(None));
    if let Some(same) = already_immutable_of(ctx, src, UNMOD_MAP_CLASS) {
        return Ok(Some(Value::Object(Some(same))));
    }
    // C4-3: `Map.copyOf`'s `map.isEmpty()` -> `Map.of()`.
    let (empty, src) = copy_of_empty_pinned(ctx, src, UNMOD_MAP_CLASS)?;
    if let Some(empty) = empty {
        return Ok(Some(Value::Object(Some(empty))));
    }
    // CW2-5: `Map.ofEntries(map.entrySet().toArray(new Entry[0]))` -- the
    // source's entries in its own order, through `of_map` (`Map1` / `MapN`'s
    // checks: a key an `IdentityHashMap` holds twice is the JDK's `duplicate
    // key`, where the `HashMap` copy below folded it; a null is found per pair
    // in order).
    if copyof_one_pass_enabled() {
        if let Value::Object(Some(s)) = src {
            // C3W3-2 (`CRATONVM_COMPAT_MAP_COPY_JDK_QUESTIONS`, round 15 wave
            // 4, lane compat4): a program's map is asked `entrySet().toArray(new
            // Entry[0])`, then each entry `getKey()` / `getValue()` -- not
            // `isEmpty()` a second time and an iterator walk.
            let pairs = match foreign_map_source(&*ctx, s) {
                Some(prog) => map_entries_by_to_array(ctx, prog)?,
                // Handed straight to `of_map`, which pins them first.
                None => collect_entries_any(ctx, s)?,
            };
            return of_map(ctx, &pairs);
        }
    }
    // GC-safety: the allocation and the copy-in below both collect; `src` and
    // `backing` are bare locals used after each. See `rooted_across`.
    let src_pin = pin_value(ctx, src);
    let mut backing = try_alloc_synthetic(ctx, "java/util/HashMap", MAP_NUM_FIELDS)?;
    let src = read_pinned_elem(ctx, src_pin, src);
    let backing_at_call = backing;
    let copied = rooted_across(ctx, &mut [&mut backing], |ctx| {
        native_map_init_from_map(ctx, &[Value::Object(Some(backing_at_call)), src])
    });
    if src_pin != usize::MAX {
        ctx.unpin_native_roots(src_pin);
    }
    copied?;
    // `Map.copyOf` rejects a null key or value, like the `of` factories. Read
    // off the BUILT backing (ours, so the walk is a field read) rather than
    // re-entering the source's `entrySet`.
    if map_collect_entries(ctx, backing)
        .iter()
        .any(|(k, v)| matches!(k, Value::Object(None)) || matches!(v, Value::Object(None)))
    {
        return Err(MethodCallFailed::from(RuntimeError::NullPointerException {
            message: None,
        }));
    }
    let wrapper = alloc_immutable_wrapper(ctx, UNMOD_MAP_CLASS, backing)?;
    Ok(Some(Value::Object(Some(wrapper))))
}

/// `Collections.unmodifiableMap` — wrap the source map in a live read-only view.
pub(crate) fn native_collections_unmodifiable_map(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(Value::Object(Some(src))) => {
            let w = alloc_unmod_wrapper(ctx, UNMOD_MAP_CLASS, *src)?;
            Ok(Some(Value::Object(Some(w))))
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

/// `Collections.unmodifiableSet` — wrap the source set in a live read-only view.
///
/// Uses the plain `UNMOD_SET_CLASS` stamp (Set/Collection/Serializable only —
/// NOT SortedSet/NavigableSet). `unmodifiableSortedSet`/`unmodifiableNavigableSet`
/// go through `native_collections_unmodifiable_sorted_set` below instead, so a
/// plain wrapped `LinkedHashSet`/`HashSet` never claims a `comparator()` (etc.)
/// surface it can't actually delegate (real `LinkedHashSet` has no such method).
pub(crate) fn native_collections_unmodifiable_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(Value::Object(Some(src))) => {
            let w = alloc_unmod_wrapper(ctx, UNMOD_SET_CLASS, *src)?;
            Ok(Some(Value::Object(Some(w))))
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

/// `Collections.unmodifiableSortedSet` ? wrap as a read-only SortedSet view.
pub(crate) fn native_collections_unmodifiable_sorted_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(Value::Object(Some(src))) => {
            let w = alloc_unmod_wrapper(ctx, UNMOD_SORTED_SET_CLASS, *src)?;
            Ok(Some(Value::Object(Some(w))))
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

/// `Collections.unmodifiableNavigableSet` — wrap as a read-only NavigableSet view.
pub(crate) fn native_collections_unmodifiable_navigable_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(Value::Object(Some(src))) => {
            let w = alloc_unmod_wrapper(ctx, UNMOD_NAVIGABLE_SET_CLASS, *src)?;
            Ok(Some(Value::Object(Some(w))))
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

/// `Collections.unmodifiableCollection` — wrap the source collection in a
/// live read-only view.
pub(crate) fn native_collections_unmodifiable_collection(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(Value::Object(Some(src))) => {
            let w = alloc_unmod_wrapper(ctx, UNMOD_COLLECTION_CLASS, *src)?;
            Ok(Some(Value::Object(Some(w))))
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

pub(crate) fn native_collections_empty_map(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    if let Some(v) = collections_empty_singleton(ctx, "EMPTY_MAP") {
        return Ok(Some(v));
    }
    // `alloc_real_jdk` FIRST, exactly as `native_collections_singleton_list`
    // does one screen up, and for the same reason: it resolves the class the
    // JDK would have returned in BOTH modes — real-JDK from the image, and
    // `--synthetic-jdk` from `class_manager`'s fabrication tables, which carry
    // `Collections$Empty*` field shapes and interface rows already. Only the
    // static-field cache above is real-JDK-only.
    //
    // Without it this fallback minted an ordinary mutable synthetic, so in
    // `--synthetic-jdk` (measured 2026-09-02, `apps/probes/EmptySingletonImmutable`):
    //
    //     Collections.emptyList()  class=java.util.ArrayList
    //                              instanceof ArrayList = true
    //                              add("x") = SUCCEEDED
    //
    // The `add` is the defect. `ensure_collections_empty_singletons` above
    // records what a mutable empty singleton cost the last time one shipped —
    // kotlin-reflect's shaded protobuf tests `instanceof ArrayList` to decide
    // whether to replace its `emptyList()` placeholder, skipped the
    // replacement, and mutated the shared object. Here each call happens to
    // mint a FRESH list, so the write is not shared — it is silently DISCARDED
    // instead, which is the same class of wrong answer with a quieter failure.
    if let Some(o) = alloc_real_jdk(ctx, "java/util/Collections$EmptyMap") {
        return Ok(Some(Value::Object(Some(o))));
    }
    // Last resort: fresh synthetic empty map (gc-common w20-a: see
    // `alloc_initialised_backing_map`).
    let map = alloc_initialised_backing_map(ctx)?;
    Ok(Some(Value::Object(Some(map))))
}

pub(crate) fn native_collections_empty_set(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    if let Some(v) = collections_empty_singleton(ctx, "EMPTY_SET") {
        return Ok(Some(v));
    }
    // See `native_collections_empty_map` for why this precedes the synthetic.
    if let Some(o) = alloc_real_jdk(ctx, "java/util/Collections$EmptySet") {
        return Ok(Some(Value::Object(Some(o))));
    }
    // Last resort: fresh synthetic empty set (gc-common w20-a: see
    // `alloc_synthetic_hash_set`).
    let (set, _) = alloc_synthetic_hash_set(ctx)?;
    Ok(Some(Value::Object(Some(set))))
}

pub(crate) fn native_collections_empty_iterator(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let itr = alloc_empty_array_holder(ctx, "java/util/Collections$EmptyItr", 2)?;
    Ok(Some(Value::Object(Some(itr))))
}

pub(crate) fn native_collections_singleton(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let elem = args.first().cloned().unwrap_or(Value::Object(None));
    // gc-common w20-a: `elem` is a bare copy of the argument and every arm
    // below allocates before storing it; the synthetic arm also stored the map
    // into, and returned, the set's pre-allocation address. The element is
    // pinned for the whole call and read back at each use; the set is pinned
    // across the `put` (which allocates a node and runs `hashCode`).
    let elem_pin = pin_value(ctx, elem);
    let out = (|| -> MethodCallResult {
        if let Some(set) = alloc_real_jdk(ctx, "java/util/Collections$SingletonSet") {
            let elem = read_pinned_elem(ctx, elem_pin, elem);
            ctx.set_field_by_name(set, "element", elem);
            return Ok(Some(Value::Object(Some(set))));
        }
        let (set, inner_map) = alloc_synthetic_hash_set(ctx)?;
        // Add elem. The value was `Value::Object(None)` — a NULL marker, which
        // `native_hs_add`/`native_hs_remove` read as "the element is absent" and
        // which real `HashSet.remove`'s `== PRESENT` identity test can never
        // satisfy. See [`hs_present_marker_at`].
        let elem = read_pinned_elem(ctx, elem_pin, elem);
        let present = hs_present_marker(&*ctx, elem);
        let (set, put) = rooted_across1(ctx, set, |ctx| {
            native_map_put(ctx, &[Value::Object(Some(inner_map)), elem, present])
        });
        put?;
        Ok(Some(Value::Object(Some(set))))
    })();
    if elem_pin != usize::MAX {
        ctx.unpin_native_roots(elem_pin);
    }
    out
}

pub(crate) fn native_collections_singleton_map(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let key = args.first().cloned().unwrap_or(Value::Object(None));
    let val = args.get(1).cloned().unwrap_or(Value::Object(None));
    // gc-common w20-a: `key` and `val` are bare copies of the arguments and
    // both arms allocate before storing them; the synthetic arm also `put`
    // into, and returned, the map's pre-initialisation address. Both are
    // pinned for the whole call (`base` is the first pin taken).
    let (base, pins) = pin_value_slice(ctx, &[key, val]);
    let out = (|| -> MethodCallResult {
        if let Some(map) = alloc_real_jdk(ctx, "java/util/Collections$SingletonMap") {
            let key = read_pinned_elem(ctx, pins[0], key);
            let val = read_pinned_elem(ctx, pins[1], val);
            ctx.set_field_by_name(map, "k", key);
            ctx.set_field_by_name(map, "v", val);
            return Ok(Some(Value::Object(Some(map))));
        }
        let map = alloc_initialised_backing_map(ctx)?;
        let key = read_pinned_elem(ctx, pins[0], key);
        let val = read_pinned_elem(ctx, pins[1], val);
        let (map, put) = rooted_across1(ctx, map, |ctx| {
            native_map_put(ctx, &[Value::Object(Some(map)), key, val])
        });
        put?;
        Ok(Some(Value::Object(Some(map))))
    })();
    if base != usize::MAX {
        ctx.unpin_native_roots(base);
    }
    out
}

pub(crate) fn native_collections_frequency(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reject_null_collections_arg(args.first())?;
    let coll = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let mut target = args.get(1).cloned().unwrap_or(Value::Object(None));
    let (data, size) = al_state(ctx, coll);
    // Stub-removal wave 2 (2026-07-28): `al_state` only understands the
    // ArrayList shape (field 0 = backing array, field 1 = size). EVERY other
    // Collection — `Arrays.asList` (an `Arrays$ArrayList`), `HashSet`,
    // `LinkedList`, any user Collection — fell into the `None` arm and
    // reported frequency 0, i.e. "this element does not occur", for a
    // collection full of matches. Measured against HotSpot 25:
    // Arrays.asList 3 -> 0, HashSet 1 -> 0, LinkedList 2 -> 0.
    // Drive the receiver's own `toArray()` instead; every Collection has one.
    let (data, size) = match data {
        Some(d) => (d, size),
        None => {
            let coll_pin = ctx.pin_native_root(coll);
            let target_pin = pin_value(ctx, target);
            let arr = ctx.invoke_virtual(coll, "toArray", "()[Ljava/lang/Object;", &[])?;
            target = read_pinned_elem(ctx, target_pin, target);
            ctx.unpin_native_roots(coll_pin);
            match arr {
                Some(Value::Object(Some(a))) => {
                    let n = ctx.array_length(a) as i32;
                    (a, n)
                }
                _ => return Ok(Some(Value::Int(0))),
            }
        }
    };
    // Family-1 fix (cce0079): pinned scan (counts every match, so no
    // early-exit helper).
    let data_pin = ctx.pin_native_root(data);
    let th = pin_value(ctx, target);
    let mut data = data;

    let mut count = 0i32;
    for i in 0..size as usize {
        let elem = ctx.get_array_element(data, i);
        let matched = list_element_matches(ctx, &elem, &target)?;
        data = ctx.read_native_pin(data_pin, data);
        target = read_pinned_elem(ctx, th, target);
        if matched {
            count += 1;
        }
    }
    ctx.unpin_native_roots(data_pin);
    Ok(Some(Value::Int(count)))
}

// Retained for reference only — no longer registered (see the note at the
// `Collections` registration site); the real JDK bytecode handles `disjoint`.
#[allow(dead_code)]
pub(crate) fn native_collections_disjoint(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let c1 = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(1))),
    };
    let c2 = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(1))),
    };
    let (data1, size1) = al_state(ctx, c1);
    let (data2, size2) = al_state(ctx, c2);
    let (d1, d2) = match (data1, data2) {
        (Some(a), Some(b)) => (a, b),
        _ => return Ok(Some(Value::Int(1))),
    };
    for i in 0..size1 as usize {
        let elem = ctx.get_array_element(d1, i);
        for j in 0..size2 as usize {
            let other = ctx.get_array_element(d2, j);
            if values_equal(ctx, &elem, &other) {
                return Ok(Some(Value::Int(0)));
            }
        }
    }
    Ok(Some(Value::Int(1)))
}

/// `Collections.max(Collection)` / `Collections.min(Collection)` share this
/// natural-ordering scan: pick element 0, then walk the rest comparing via
/// `Comparable.compareTo` (the real JDK contract — `max`/`min` are literally
/// specified in terms of `compareTo`), replacing the running candidate when
/// `cmp() > 0` (max) or `cmp() < 0` (min). Ties keep the earliest element,
/// matching `Collections.max`/`min`'s documented "first of equal maximums"
/// behavior.
///
/// The previous implementation compared `val_to_string(elem)` — i.e. each
/// element's `read_string()` decode — which only means something for actual
/// `java.lang.String` elements. For any other `Comparable` (e.g. a
/// `DeadNode`/custom class), `read_string` returns `None` for a non-String
/// object, so every element degraded to the same empty string and the `>`/`<`
/// comparison was *always false* — silently returning element 0 regardless of
/// true ordering. That produced a deterministic-but-wrong "minimum" for
/// `RestClient.selectNodes`'s `Collections.min(selectedDeadNodes)` dead-host
/// revival pick, always resurrecting the first-registered host instead of the
/// one with the lowest `deadUntilNanos` (RestClientMultipleHostsTests.
/// testRoundRobinRetryErrors: "host [...] not found, most likely used
/// multiple times").
pub(crate) fn native_collections_extreme(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    want_max: bool,
) -> MethodCallResult {
    reject_null_collections_arg(args.first())?;
    let coll = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, size) = al_state(ctx, coll);
    let elems = if let Some(data) = data {
        let len = size.max(0) as usize;
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            out.push(ctx.get_array_element(data, i));
        }
        out
    } else {
        collect_collection_elements_or_real(ctx, coll)?
    };
    if elems.is_empty() {
        return Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: String::new(),
            }
            .into(),
        );
    }
    // cceres3: pin across GC-capable call (stream stale-at-store wave) — the
    // compareTo dispatch re-enters Java, moving `best` and every later elem;
    // track `best` by handle like native_stream_min does.
    let (elem_base, elem_handles) = pin_value_slice(ctx, &elems);
    let mut best = elems[0];
    let mut best_handle = elem_handles[0];
    for i in 1..elems.len() {
        let elem = read_pinned_elem(ctx, elem_handles[i], elems[i]);
        let best_arg = read_pinned_elem(ctx, best_handle, best);
        let cmp = match compare_via_compare_to(ctx, &elem, &best_arg) {
            Ok(c) => c,
            Err(e) => {
                if elem_base != usize::MAX {
                    ctx.unpin_native_roots(elem_base);
                }
                return Err(e);
            }
        };
        if (want_max && cmp > 0) || (!want_max && cmp < 0) {
            best = elems[i];
            best_handle = elem_handles[i];
        }
    }
    let best = read_pinned_elem(ctx, best_handle, best);
    if elem_base != usize::MAX {
        ctx.unpin_native_roots(elem_base);
    }
    Ok(Some(best))
}

pub(crate) fn native_collections_max(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_collections_extreme(ctx, args, true)
}

pub(crate) fn native_collections_min(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_collections_extreme(ctx, args, false)
}

pub(crate) fn native_collections_swap(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reject_null_collections_arg(args.first())?;
    let list = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // MEASURED 2026-08-13 (scratchpad/orch/Exp.java): out-of-range is
    // IndexOutOfBoundsException with the JDK's ArrayList text. Two defects
    // here: there was NO bounds check at all, and `*v as usize` turned a
    // NEGATIVE index into a huge one, so `swap(l, -1, 0)` was silently
    // accepted rather than refused. Bound against the list SIZE, not the
    // backing array capacity -- an ArrayList over-allocates, so the array
    // length would admit indices past the end.
    let i = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => return Ok(None),
    };
    let j = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => return Ok(None),
    };
    let (data, size) = al_state(ctx, list);
    for idx in [i, j] {
        if idx < 0 || idx >= size {
            return Err(RuntimeError::IndexOutOfBoundsException {
                message: Some(format!("Index {idx} out of bounds for length {size}")),
            }
            .into());
        }
    }
    if let Some(d) = data {
        let (i, j) = (i as usize, j as usize);
        let a = ctx.get_array_element(d, i);
        let b = ctx.get_array_element(d, j);
        ctx.set_array_element(d, i, b);
        ctx.set_array_element(d, j, a);
        return Ok(None);
    }
    // Not an ArrayList-layout receiver. `al_state` answers `(None, 0)` for a
    // sublist view, a `LinkedList`, or any application `List`, and the loop
    // above then swapped nothing and reported success — a SILENT no-op, which
    // is what `probes/SubListBehaviourProbe`'s `write.swap` row measures. It
    // failed on both collectors AND under `--jdk-only`, because this native is
    // reached whatever runs `Collections.swap`. Go through the receiver's own
    // `get`/`set`, which is what the JDK's own body does.
    list_swap_via_accessors(ctx, list, i as i32, j as i32)
}

/// `Collections.swap` for a receiver whose backing array this file cannot
/// read: two `get`s and two `set`s through the list's own methods.
///
/// Each invoke can allocate and therefore move the other element, so both are
/// pinned across the pair.
pub(crate) fn list_swap_via_accessors(
    ctx: &mut dyn NativeContext,
    list: ObjectRef,
    i: i32,
    j: i32,
) -> MethodCallResult {
    let list_pin = ctx.pin_native_root(list);
    let a = ctx
        .invoke_virtual(list, "get", "(I)Ljava/lang/Object;", &[Value::Int(i)])?
        .unwrap_or(Value::Object(None));
    let a_pin = pin_value(ctx, a);
    let list = ctx.read_native_pin(list_pin, list);
    let b = ctx
        .invoke_virtual(list, "get", "(I)Ljava/lang/Object;", &[Value::Int(j)])?
        .unwrap_or(Value::Object(None));
    let b_pin = pin_value(ctx, b);
    let list = ctx.read_native_pin(list_pin, list);
    let a = read_pinned_elem(ctx, a_pin, a);
    let b = read_pinned_elem(ctx, b_pin, b);
    ctx.invoke_virtual(
        list,
        "set",
        "(ILjava/lang/Object;)Ljava/lang/Object;",
        &[Value::Int(i), b],
    )?;
    let list = ctx.read_native_pin(list_pin, list);
    let a = read_pinned_elem(ctx, a_pin, a);
    ctx.invoke_virtual(
        list,
        "set",
        "(ILjava/lang/Object;)Ljava/lang/Object;",
        &[Value::Int(j), a],
    )?;
    ctx.unpin_native_roots(list_pin);
    Ok(None)
}

pub(crate) fn native_collections_fill(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reject_null_collections_arg(args.first())?;
    let list = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let val = args.get(1).cloned().unwrap_or(Value::Object(None));
    let (data, size) = al_state(ctx, list);
    if let Some(d) = data {
        for i in 0..size as usize {
            ctx.set_array_element(d, i, val);
        }
        return Ok(None);
    }
    // Same silent no-op as `native_collections_swap`, same fix: a receiver
    // whose backing array this file cannot read gets its own `size`/`set`.
    let list_pin = ctx.pin_native_root(list);
    let val_pin = pin_value(ctx, val);
    let n = match ctx.invoke_virtual(list, "size", "()I", &[])? {
        Some(Value::Int(n)) => n,
        _ => 0,
    };
    for i in 0..n {
        let list_cur = ctx.read_native_pin(list_pin, list);
        let val_cur = read_pinned_elem(ctx, val_pin, val);
        ctx.invoke_virtual(
            list_cur,
            "set",
            "(ILjava/lang/Object;)Ljava/lang/Object;",
            &[Value::Int(i), val_cur],
        )?;
    }
    ctx.unpin_native_roots(list_pin);
    Ok(None)
}

pub(crate) fn native_collections_n_copies(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let n = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let val = args.get(1).cloned().unwrap_or(Value::Object(None));
    // MEASURED 2026-08-13 (scratchpad/orch/Two.java): a negative length is
    // IllegalArgumentException with the count in the text, NOT a clamp. The
    // `n.max(0)` below turned nCopies(-1, x) into an empty list -- a plausible
    // answer, so nothing downstream could notice. The check precedes both
    // allocations and the pins: an early return between pin and unpin leaks.
    if n < 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("List length = {n}").into(),
        }
        .into());
    }
    let __al_n_fields = al_slots(ctx).2;
    // cceres3: pin across GC-capable call (stream stale-at-store wave) — the
    // two allocations can move `val` and the freshly allocated `list`.
    let val_handle = pin_value(ctx, val);
    // Not `?` (gc-common w20-a): a refusal releases the value's pin.
    let list = match try_alloc_synthetic(ctx, "java/util/ArrayList", __al_n_fields) {
        Ok(l) => l,
        Err(e) => {
            if val_handle != usize::MAX {
                ctx.unpin_native_roots(val_handle);
            }
            return Err(e);
        }
    };
    let list_pin = ctx.pin_native_root(list);
    let arr: ObjectRef = alloc_ref_array(ctx, n.max(0) as usize);
    let val = read_pinned_elem(ctx, val_handle, val);
    for i in 0..n.max(0) as usize {
        ctx.set_array_element(arr, i, val);
    }
    let list = ctx.read_native_pin(list_pin, list);
    al_set_data(ctx, list, arr);
    al_set_size(ctx, list, n.max(0));
    // IMMUTABLE. The JDK answers a `Collections$CopiesList`, which refuses
    // `add`/`set`/`remove`; this returned a plain `ArrayList`, so
    // `nCopies(2, "x").set(0, "y")` succeeded (apps/probes/CollectionsShadowSweep
    // 40-41). `nCopies` is used precisely BECAUSE its result is shared and
    // cannot change -- a mutable one is handed to several holders at once.
    // Wrapped while still pinned: the wrapper allocates.
    let list = ctx.read_native_pin(list_pin, list);
    // The `JdkUnmodifiableViews` row (interpreter round i1 wave 51, lane L5):
    // the JDK's `Collections.unmodifiableList` first, which a real image runs
    // as bytecode there, so `nCopies` hands out no stand-in either; a
    // synthetic image (no such body) falls to the native below.
    if ctx.aligned(cratonvm_types::HotSpotAlignment::JdkUnmodifiableViews) {
        if let Ok(v @ Some(Value::Object(Some(_)))) = ctx.invoke(
            "java/util/Collections",
            "unmodifiableList",
            "(Ljava/util/List;)Ljava/util/List;",
            &[Value::Object(Some(list))],
        ) {
            ctx.unpin_native_roots(if val_handle == usize::MAX {
                list_pin
            } else {
                val_handle
            });
            return Ok(v);
        }
    }
    let list = ctx.read_native_pin(list_pin, list);
    let wrapped = match native_collections_unmodifiable_list(ctx, &[Value::Object(Some(list))]) {
        Ok(v @ Some(Value::Object(Some(_)))) => Ok(v),
        // `cratonvm/internal/UnmodifiableList` is a FABRICATION, and
        // `--jdk-only` refuses it. `Collections.unmodifiableList` is registered
        // as a `SyntheticStub`, so strict mode drops the registration and real
        // bytecode serves the Java call -- but calling the native's BODY here
        // bypasses that drop, which is how a fix for the immutability row
        // introduced a `NoClassDefFoundError` in the strict arm. Ask the real
        // static instead; every real image has it, and it answers a genuine
        // `Collections$UnmodifiableRandomAccessList`.
        _ => {
            let list = ctx.read_native_pin(list_pin, list);
            match ctx.invoke(
                "java/util/Collections",
                "unmodifiableList",
                "(Ljava/util/List;)Ljava/util/List;",
                &[Value::Object(Some(list))],
            ) {
                Ok(v @ Some(Value::Object(Some(_)))) => Ok(v),
                // A synthetic-JDK image has neither. The list stays mutable,
                // which is what it was before this became immutable at all.
                _ => Ok(Some(Value::Object(Some(
                    ctx.read_native_pin(list_pin, list),
                )))),
            }
        }
    };
    ctx.unpin_native_roots(if val_handle == usize::MAX {
        list_pin
    } else {
        val_handle
    });
    wrapped
}

pub(crate) fn native_collections_shuffle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reject_null_collections_arg(args.first())?;
    let list = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let (data, size) = al_state(ctx, list);
    if let Some(d) = data {
        let n = size as usize;
        // Simple Fisher-Yates using system time as seed
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(42u64, |t| t.as_nanos() as u64);
        let mut rng = seed;
        for i in (1..n).rev() {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let j = (rng >> 33) as usize % (i + 1);
            let a = ctx.get_array_element(d, i);
            let b = ctx.get_array_element(d, j);
            ctx.set_array_element(d, i, b);
            ctx.set_array_element(d, j, a);
        }
    }
    Ok(None)
}

pub(crate) fn native_collections_add_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // BOTH arguments: the varargs array is dereferenced for its length, so
    // `Collections.addAll(c, (Object[]) null)` is an NPE too.
    reject_null_collections_arg(args.first())?;
    reject_null_collections_arg(args.get(1))?;
    let coll = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elements = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Every `Collection.add` dispatch can execute arbitrary Java code and
    // collect. Root both the destination and the source array before the first
    // callback, then refresh them for every iteration. Keeping the array as a
    // raw entry-argument copy made the second element read dereference
    // from-space during WildFly's parallel subsystem boot.
    let pin_base = ctx.pin_native_root(coll);
    let elements_pin = ctx.pin_native_root(elements);
    let elements = ctx.read_native_pin(elements_pin, elements);
    let len = ctx.array_length(elements);
    let mut modified = false;
    for i in 0..len {
        let elements = ctx.read_native_pin(elements_pin, elements);
        let elem = ctx.get_array_element(elements, i);
        // Dispatch to the target collection's real `add` instead of
        // hard-coding `native_al_add` (the ArrayList `elementData`/`size`
        // layout). `Collections.addAll` accepts any `Collection` — a
        // HashSet, LinkedList, TreeSet, etc. store elements nothing like
        // ArrayList, so `native_al_add` would write the wrong fields and
        // silently drop every element. JVMS/JDK spec is `result |=
        // c.add(element)`; honour that via virtual dispatch.
        let coll = ctx.read_native_pin(pin_base, coll);
        match ctx.invoke_virtual(coll, "add", "(Ljava/lang/Object;)Z", &[elem]) {
            Ok(Some(Value::Int(1))) => modified = true,
            Ok(_) => {}
            Err(error) => {
                ctx.unpin_native_roots(pin_base);
                return Err(error);
            }
        }
    }
    ctx.unpin_native_roots(pin_base);
    Ok(Some(Value::Int(if modified { 1 } else { 0 })))
}

