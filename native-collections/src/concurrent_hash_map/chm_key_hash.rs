#[allow(unused_imports)]
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
    true
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
// Under `--real-jdk`, and under `--compatible` over a real JDK with the
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
    true
}

/// Is every `ConcurrentHashMap` in this VM the JDK's own (`table` of `Node`s)?
/// `--real-jdk`, or `--compatible` over a real JDK with the
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
        } else {
            return hash == CHM_JDK_RESERVED;
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
/// [`chm_jdk_model`] (which adds `--real-jdk`).
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
            ctx.unpin_native_roots(key_pin);        }
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
// Under `--real-jdk` none of this changes anything (its registrations are
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
    pub(in crate::concurrent_hash_map) fn resolve(ctx: &dyn NativeContext) -> Option<ChmJdkNodeSlots> {
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
            let c = "java/util/concurrent/ConcurrentHashMap";

    // The `JdkConcurrentHashMap` row (interpreter round i3 wave 76, lane L1;
    // default off): over a real JDK under `--compatible`, none of this
    // cluster's rows is registered -- the class, its views and iterators, and
    // `register_chm_key_set_view_natives` -- so the JDK's bodies run over the
    // JDK's `table`, as under `--real-jdk`. The five `ConcurrentMap`
    // INTERFACE rows stay (item 1 of the cluster note above), registered as
    // doors that hand a CHM receiver to its own body; stated `Bridge` (the
    // `set_category` above).
    if chm_row_registers_nothing(r) {
        let cm = "java/util/concurrent/ConcurrentMap";
        r.register(cm, "size", "()I", native_chm_door_size, cratonvm_native_api::NativeKind::Bridge);
        r.register(
            cm,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            native_chm_door_get, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            cm,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            native_chm_door_put, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            cm,
            "remove",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            native_chm_door_remove, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            cm,
            "containsKey",
            "(Ljava/lang/Object;)Z",
            native_chm_door_contains_key, cratonvm_native_api::NativeKind::Bridge,
        );
                return;
    }

    // Constructors
    r.register(c, "<init>", "()V", native_chm_init_default, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "<init>", "(I)V", native_chm_init_capacity, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "<init>", "(IFI)V", native_chm_init_full, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "<init>", "(Ljava/util/Map;)V", native_chm_init_from_map, cratonvm_native_api::NativeKind::Bridge);

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
        native_chm_write_object, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "readObject",
        "(Ljava/io/ObjectInputStream;)V",
        native_chm_read_object, cratonvm_native_api::NativeKind::Bridge,
    );

    // Core operations — segmented
    r.register(c, "size", "()I", native_chm_size, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "isEmpty", "()Z", native_chm_is_empty, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_put, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_get, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_remove, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_chm_contains_key, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "containsValue",
        "(Ljava/lang/Object;)Z",
        native_chm_contains_value, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "clear", "()V", native_chm_clear, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "keySet", "()Ljava/util/Set;", native_chm_key_set, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "values", "()Ljava/util/Collection;", native_chm_values, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "entrySet", "()Ljava/util/Set;", native_chm_entry_set, cratonvm_native_api::NativeKind::Bridge);
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
        native_chm_key_set, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "toString", "()Ljava/lang/String;", native_chm_to_string, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_get_or_default, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "putIfAbsent",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_put_if_absent, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "putAll", "(Ljava/util/Map;)V", native_chm_put_all, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "hashCode", "()I", native_chm_hash_code, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "equals", "(Ljava/lang/Object;)Z", native_chm_equals, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        native_chm_for_each, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "computeIfAbsent",
        "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;",
        native_chm_compute_if_absent, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "compute",
        "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        native_chm_compute, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "computeIfPresent",
        "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        native_chm_compute_if_present, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "merge",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        native_chm_merge, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "replaceAll",
        "(Ljava/util/function/BiFunction;)V",
        native_chm_replace_all, cratonvm_native_api::NativeKind::Bridge,
    );

    // ConcurrentHashMap-specific methods
    r.register(
        c,
        "remove",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
        native_chm_remove_kv, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_replace, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z",
        native_chm_replace_kv, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "forEach",
        "(JLjava/util/function/BiConsumer;)V",
        native_chm_for_each_parallel, cratonvm_native_api::NativeKind::Bridge,
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
        native_chm_reduce_values, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "searchKeys",
        "(JLjava/util/function/Function;)Ljava/lang/Object;",
        native_chm_search_keys, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "mappingCount", "()J", native_chm_mapping_count, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "newKeySet",
        "()Ljava/util/concurrent/ConcurrentHashMap$KeySetView;",
        native_chm_new_key_set, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "newKeySet",
        "(I)Ljava/util/concurrent/ConcurrentHashMap$KeySetView;",
        native_chm_new_key_set_cap, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "keySet",
        "(Ljava/lang/Object;)Ljava/util/concurrent/ConcurrentHashMap$KeySetView;",
        native_chm_key_set_view, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "contains",
        "(Ljava/lang/Object;)Z",
        native_chm_contains_value, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "elements",
        "()Ljava/util/Enumeration;",
        native_chm_elements, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "keys", "()Ljava/util/Enumeration;", native_chm_keys, cratonvm_native_api::NativeKind::Bridge);

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
        native_al_itr_has_next, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CHM_VALUE_ITER_CLASS,
        "nextElement",
        "()Ljava/lang/Object;",
        native_al_itr_next, cratonvm_native_api::NativeKind::Bridge,
    );
    // And the KEY iterator's, which `keys()` hands back. Its snapshot lives at
    // `key_itr_base` rather than `al_itr_alt_base`, so it takes the
    // `MAP_KEY_ITR_CARRIERS` bodies — the two families are not
    // interchangeable, and swapping them reads every field one slot wrong.
    r.register(
        CHM_KEY_ITER_CLASS,
        "hasMoreElements",
        "()Z",
        native_map_key_itr_has_next, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CHM_KEY_ITER_CLASS,
        "nextElement",
        "()Ljava/lang/Object;",
        native_map_key_itr_next, cratonvm_native_api::NativeKind::Bridge,
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
                // `--real-jdk`, UNARMED, against HotSpot 25.0.3+9 (H16-3):
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
        }, cratonvm_native_api::NativeKind::Bridge,
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
        }, cratonvm_native_api::NativeKind::Bridge,
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
        }, cratonvm_native_api::NativeKind::Bridge,
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
        }, cratonvm_native_api::NativeKind::Bridge,
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
                }, cratonvm_native_api::NativeKind::Bridge,
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
                }, cratonvm_native_api::NativeKind::Bridge,
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
        }, cratonvm_native_api::NativeKind::Bridge,
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
        }, cratonvm_native_api::NativeKind::Bridge,
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
            }, cratonvm_native_api::NativeKind::Bridge);
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
                }, cratonvm_native_api::NativeKind::Bridge,
            );        };    }
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
                }, cratonvm_native_api::NativeKind::Bridge,
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
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // ConcurrentMap interface — delegates to CHM segmented ops
    let cm = "java/util/concurrent/ConcurrentMap";
    r.register(cm, "size", "()I", native_chm_size, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        cm,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_get, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cm,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_put, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cm,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_remove, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cm,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_chm_contains_key, cratonvm_native_api::NativeKind::Bridge,
    );
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
                chm_seg_put_hashed(ctx, seg, key, value, hash, false)?;            }        }
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
    Some((prim, h ^ ((h as u32) >> 16) as i32))}

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
                    (h, k, 1)                }
                _ => (0, ctx.get_field(node, 1), 2),            }        };
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
    true
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
    true
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
    true
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
            put?;        } else {
            chm_seg_put_hashed(ctx, seg_now, key_now, new_now, hash, false)?;        }
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
    true
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

