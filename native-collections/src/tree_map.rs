// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

// TreeMap — sorted map.
//
// Round-9 HIGH (MED-8 carryover): the original implementation was a sorted
// `Object[]` with binary-search lookup and `tm_insert_at`/`tm_remove_at`
// shifting, giving O(log N) `get` but O(N) `put`/`remove`. The JDK's
// `TreeMap` is a red-black tree at O(log N) for all operations.
//
// We adopt a hybrid: for the common case (no custom Comparator, keys are
// String / Integer / Long / wrapped primitive) we maintain a Rust-side
// `BTreeMap<TreeKey, Value>` as the authoritative store (O(log N) on all
// ops). For custom Comparator we fall through to the array path, because
// keying a Rust BTreeMap on Java's `Comparator.compare` requires invoking
// Java code from comparison — that's a re-entrancy and lifetime hazard
// (the BTreeMap holds a mutable borrow during compare while the Comparator
// might trigger arbitrary VM code, including class loading).
//
// The fast-mode flag is stored in the LHM-style overlay (`tm_overlay`).
// When fast-mode is active the array slot stays empty; size is mirrored
// to the synthetic slot so legacy size readers still work. Iteration-style
// natives (`keySet`, `values`, `entrySet`, `forEach`) populate their output
// from the BTreeMap when active.
// ===========================================================================

pub(crate) const TM_FIELD_DATA: usize = 0; // Object[] interleaved [k0, v0, k1, v1, ...]
pub(crate) const TM_FIELD_SIZE: usize = 1; // Int: number of entries
pub(crate) const TM_FIELD_COMPARATOR: usize = 2; // Comparator object or null
pub(crate) const TM_NUM_FIELDS: usize = 3;
pub(crate) const TM_DEFAULT_CAPACITY: usize = 16; // initial entry slots (array len = 32)

/// Natural-order key supported by the fast-mode TreeMap. Variants are
/// ordered so the derived `Ord` matches Java's natural ordering for
/// homogeneous-typed maps (String, Integer, Long, Character, Byte, Short,
/// Boolean). Mixed-type maps disable fast mode (the array path handles them
/// via `natural_compare`).
///
/// Each numeric wrapper has its OWN variant — they must NOT collapse to a
/// single integer representation. The variant records the original Java
/// wrapper type so `tree_key_to_value` reboxes a `Character` key back to a
/// `Character` (not an `Integer`); collapsing `Character`/`Byte`/`Short` to
/// `I32` made `firstKey()`/`entrySet()` hand back `Integer`s, failing the
/// `checkcast Character` in e.g. `sun.util.locale.LocaleExtensions.toID`
/// (ClassCastException Integer→Character on `Locale.forLanguageTag`). The
/// per-variant in-memory representation matches each wrapper's natural
/// `compareTo`: `Char` is unsigned (u16), `Byte`/`Short` are signed.
///
/// `Str` holds UTF-16 code UNITS, not a Rust `String`, because
/// `String.compareTo` compares code units and `String`'s `Ord` compares code
/// POINTS. Those disagree above the BMP: a supplementary character starts with
/// a surrogate unit in `0xD800..=0xDBFF`, which is BELOW `U+E000..=U+FFFF`,
/// where its code point is above them. MEASURED on both VMs 2026-08-17
/// (`scratchpad/g55/G55Ord.java`): a `TreeSet` of
/// `{U+10000, U+E000, U+FFFF}` enumerates `[U+10000, U+E000, U+FFFF]` on
/// HotSpot and enumerated `[U+E000, U+FFFF, U+10000]` here — `firstKey()` and
/// `lastKey()` both wrong, for ordinary emoji/CJK-ext keys with no surrogate
/// exotica involved. `Vec<u16>`'s lexicographic `Ord` IS `String.compareTo`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TreeKey {
    Str(Vec<u16>),
    I32(i32),
    I64(i64),
    Char(u16),
    Byte(i8),
    Short(i16),
    Bool(bool),
}

/// Try to extract a fast-mode key from a Java value. Returns None for
/// types that need a Java-side `compareTo` callback (custom Comparable
/// objects, non-primitive wrappers, etc.); the caller falls back to the
/// array path.
pub(crate) fn tree_key_from_value(ctx: &dyn NativeContext, v: &Value) -> Option<TreeKey> {
    match v {
        Value::Int(i) => Some(TreeKey::I32(*i)),
        Value::Long(l) => Some(TreeKey::I64(*l)),
        Value::Object(Some(o)) => {
            // String fast path.
            //
            // `read_string` is the only String reader this crate has, and it
            // substitutes `U+FFFD` for an unpaired surrogate — so a key that
            // carries one cannot be represented here faithfully, and two keys
            // that differ only in WHICH one they carry would collapse into a
            // single tree entry (MEASURED 2026-08-17: `TreeMap.size()` 1 where
            // HotSpot says 2, with the survivor's value answered for both
            // keys). A decode containing no `U+FFFD` is provably exact; one
            // that does is REFUSED, and the caller falls back to the array
            // path, which keeps the real `ObjectRef` and runs the receiver's
            // own units-exact `String.compareTo`. That is slower and correct,
            // which is the right trade for content this representation cannot
            // hold.
            if let Some(s) = ctx.read_string(*o) {
                return tree_key_from_decoded_string(&s);
            }
            // Boxed primitive wrappers: field 0 holds the value. The wrapper
            // CLASS selects the variant so the key reboxes to its original
            // type on read-back — Character/Byte/Short/Boolean must NOT
            // collapse to Integer (see TreeKey doc). An unrecognized
            // single-field class (a custom Comparable) returns None so the
            // caller falls back to the array path, which keeps the real
            // ObjectRef and honors its own compareTo/identity.
            let name = ctx
                .class_name_of_id(ctx.class_id_of_object(*o))
                .unwrap_or_default();
            match ctx.get_field(*o, 0) {
                Value::Int(i) => match name.as_str() {
                    "java/lang/Integer" => Some(TreeKey::I32(i)),
                    "java/lang/Character" => Some(TreeKey::Char(i as u16)),
                    "java/lang/Byte" => Some(TreeKey::Byte(i as i8)),
                    "java/lang/Short" => Some(TreeKey::Short(i as i16)),
                    "java/lang/Boolean" => Some(TreeKey::Bool(i != 0)),
                    _ => None,
                },
                Value::Long(l) if name == "java/lang/Long" => Some(TreeKey::I64(l)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Per-TreeMap fast-mode side-table. The outer key is the synthetic
/// receiver's address; the inner BTreeMap is the authoritative store
/// for fast-mode maps (mirroring is avoided — when fast mode is active,
/// the array slot is left empty and we read from the BTreeMap).
pub(crate) fn tm_fast_table() -> &'static Mutex<OverlayRows<std::collections::BTreeMap<TreeKey, Value>>> {
    static T: std::sync::OnceLock<Mutex<OverlayRows<std::collections::BTreeMap<TreeKey, Value>>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| Mutex::new(OverlayRows::new()))
}

// TreeMap / TreeSet side-table key: rekeyed onto
// `ctx.identity_hash_code(this)` so the array-mode state survives a
// moving-GC relocation. Originally `this.as_ptr() as usize`, which
// silently dropped the `data` array and size when the GC moved the
// TreeMap (worst case: a subsequent `put` re-creates state under a
// new key while old entries become unreachable garbage in the table).
pub(crate) fn tm_obj_key(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    widened_obj_key(ctx, this)
}

/// Address-keyed TreeMap array-mode state side-table — `(data array, size,
/// comparator)` keyed by the receiver's address.
///
/// Why a side-table instead of object fields: in real-JDK mode `TreeMap`
/// instances have the *real* JDK field layout (`comparator`, `root`,
/// `size`, `modCount`, ...), and subclasses such as Felix's `StringMap
/// extends TreeMap` add their own fields on top. Writing to the synthetic
/// slots 0/1/2 lands in unrelated real fields — silently losing the data
/// array and size, and (worse) writing an `Int` into a reference slot
/// corrupts the heap for the GC. Keying state by the object's address is
/// layout-independent and works for arbitrary subclasses.
#[derive(Clone)]
pub(crate) struct TmArrayState {
    pub(crate) data: Option<ObjectRef>,
    pub(crate) size: i32,
    pub(crate) comparator: Value,
    /// The `modCount` this map's real `root` mirror was last built at, or `-1`
    /// when it has never been built. See [`tm_publish_real_root`].
    ///
    /// This is what makes the mirror affordable. `WORKER-2-NOTE-1` section 5b
    /// declined `root` because the only available triggers are the bulk-read
    /// natives and `native_tm_entry_set` ALREADY allocates N entries, so a
    /// mirror rebuilt on every read would roughly double the allocation cost of
    /// every `TreeMap` iteration in the VM. Gating on `modCount` -- which this
    /// lane fixed one commit earlier, and which is pinned at 0 without that fix
    /// -- turns "per iteration" into "per mutation": an unchanged map is a
    /// single `i32` compare.
    pub(crate) root_published_at: i32,
}
impl Default for TmArrayState {
    fn default() -> Self {
        TmArrayState {
            data: None,
            size: 0,
            comparator: Value::Object(None),
            root_published_at: -1,
        }
    }
}
pub(crate) fn tm_array_table() -> &'static Mutex<OverlayRows<TmArrayState>> {
    static T: std::sync::OnceLock<Mutex<OverlayRows<TmArrayState>>> = std::sync::OnceLock::new();
    T.get_or_init(|| Mutex::new(OverlayRows::new()))
}

/// Materialize a *deserialized* TreeMap's entries into the native side-table.
///
/// `TreeMap.readObject` (real bytecode) rebuilds the red-black tree via
/// `buildFromSorted`, populating the real `root`/`size` fields and a graph of
/// `TreeMap$Entry{key,value,left,right,...}` nodes — but it never touches our
/// `tm_fast_table`/`tm_array_table` side-tables, so every native read op
/// (`get`/`size`/`entrySet`/…) sees an empty map. This walks the real tree
/// in-order and replays its entries into the fast-mode BTreeMap, so a
/// round-tripped TreeMap reads back correct content.
///
/// Runs entirely on `&dyn NativeContext` (field/array/string reads are all
/// `&self`; no Java allocation), so it can hook the shared read funnels
/// (`tm_get_slot` / `tm_is_fast_mode`) and cover every read path at once.
/// Idempotent: a no-op once side-table state exists or when the real `root`
/// is null (a fresh or empty map). Covers the natural-order case
/// (String/Integer/Long keys, null comparator) — exactly what serialization
/// round-trips. A custom-comparator or non-extractable-key tree would need the
/// array path (Java array allocation, unavailable here) and is left to the
/// owning `&mut` native; such maps simply stay empty after deserialization,
/// the pre-existing behavior.
pub(crate) fn tm_materialize_deser_if_needed(ctx: &dyn NativeContext, this: ObjectRef) {
    let key = tm_obj_key(ctx, this);
    // Already have native state (built via put/init, or previously
    // materialized) — nothing to do.
    if tm_fast_table().lock().unwrap().contains_key(&key) {
        return;
    }
    if let Some(st) = tm_array_table().lock().unwrap().get(&key) {
        if st.size > 0 || st.data.is_some() {
            return;
        }
    }
    let nf = ctx.object_num_fields(this);
    // Real red-black-tree root: null for native-managed/fresh maps (they never
    // write the real field), non-null only after `buildFromSorted`.
    let root = match ctx.resolve_field_index("java/util/TreeMap", "root") {
        Some(i) if i < nf => match ctx.get_field(this, i) {
            Value::Object(Some(r)) => r,
            _ => return,
        },
        _ => return,
    };
    // Custom comparator → array path (needs Java alloc) — skip here.
    let has_comparator = ctx
        .resolve_field_index("java/util/TreeMap", "comparator")
        .filter(|&i| i < nf)
        .map(|i| !matches!(ctx.get_field(this, i), Value::Object(None)))
        .unwrap_or(false);
    if has_comparator {
        return;
    }
    let ev = "java/util/TreeMap$Entry";
    let (ki, vi, li, ri) = match (
        ctx.resolve_field_index(ev, "key"),
        ctx.resolve_field_index(ev, "value"),
        ctx.resolve_field_index(ev, "left"),
        ctx.resolve_field_index(ev, "right"),
    ) {
        (Some(a), Some(b), Some(c), Some(d)) => (a, b, c, d),
        _ => return,
    };
    let child = |n: ObjectRef, idx: usize| -> Option<ObjectRef> {
        match ctx.get_field(n, idx) {
            Value::Object(o) => o,
            _ => None,
        }
    };
    // Iterative in-order traversal (left, node, right).
    let mut entries: Vec<(TreeKey, Value)> = Vec::new();
    let mut stack: Vec<ObjectRef> = Vec::new();
    let mut cur = Some(root);
    while cur.is_some() || !stack.is_empty() {
        while let Some(n) = cur {
            stack.push(n);
            cur = child(n, li);
        }
        let n = match stack.pop() {
            Some(n) => n,
            None => break,
        };
        let k = ctx.get_field(n, ki);
        let v = ctx.get_field(n, vi);
        match tree_key_from_value(ctx, &k) {
            Some(tk) => entries.push((tk, v)),
            // Non-extractable key (custom Comparable) → needs the array path.
            None => return,
        }
        cur = child(n, ri);
    }
    let len = entries.len() as i32;
    {
        let mut ft = tm_fast_table().lock().unwrap();
        let bt = ft.entry(key).or_default();
        for (tk, v) in entries {
            bt.insert(tk, v);
        }
    }
    // native_tm_size reads the array-state `size` slot even in fast mode.
    tm_array_table()
        .lock()
        .unwrap()
        .entry(key)
        .or_default()
        .size = len;
}

/// Read a `TreeMap$Entry` child link (left/right) as an `Option<ObjectRef>`.
pub(crate) fn tm_child(ctx: &dyn NativeContext, n: ObjectRef, idx: usize) -> Option<ObjectRef> {
    match ctx.get_field(n, idx) {
        Value::Object(o) => o,
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Navigable views — `headMap` / `tailMap` / `subMap` / `descendingMap`
// ---------------------------------------------------------------------------
//
// W7-1 family 1 (2026-08-11). `SortedMap.headMap` and its neighbours are
// specified as VIEWS: a write through one is a write to the backing map, and a
// write to the backing map is visible through the view. CratonVM answered a
// detached SNAPSHOT — `native_tm_head_map` allocated a second TreeMap and
// replayed the matching entries into it with `native_tm_put` — so
// `tm.headMap("c").remove("a")` deleted from a map nobody else could see and
// `tm` still read `{a=1, b=2, c=3, d=4}`. The record's `pollFirstEntry` row is
// that same miss, one line later, not a second defect.
//
// An empty or detached view is the failure mode that reads as a PASS everywhere
// a caller only iterates, which is why it survived so long and why
// `probes/JdkOnlyCollectionViewProbe` was written to report CONTENT.
//
// THE SHAPE OF THE FIX. The view is a real `java.util.TreeMap` object whose own
// storage is a CACHE, not the truth: a [`TmViewSpec`] in the side table below
// names the backing map and the range, and [`tm_sync_native_state`] — the funnel
// every content native already calls on entry — rebuilds the cache from the
// backing map before that native reads it. So every read native
// (`get`/`size`/`firstKey`/`ceilingEntry`/`entrySet`/`toString`/…) becomes
// view-correct without being touched, and only the mutators need a branch that
// redirects the write to the backing map.
//
// The view's own comparator slot holds the source comparator, REVERSED for a
// descending view (`Collections.reverseOrder`). That one field is what makes
// `firstKey`, `ceilingKey`, `pollFirstEntry` and `tm_binary_search` all agree
// with the view's iteration order without a second code path for descending.
//
// COST. The cache is rebuilt on every operation rather than being invalidated by
// a version stamp. That is deliberate: a stamp has to be bumped at every site
// that mutates a TreeMap's contents, and a site missed there is a silently stale
// view — the exact defect class this record is about. It is also not a
// regression on the common shape: `tm.headMap(k)` used to cost one allocation
// plus N comparator-driven `native_tm_put`s (O(N log N)) at CREATION, where a
// rebuild is one allocation plus N bound comparisons. Only a view that is
// RETAINED and read repeatedly pays more than before. See the record's
// "still needs measuring" section.

/// The backing map and range of one navigable view.
///
/// Bounds are expressed in the IMMEDIATE source's ordering, and a view of a view
/// records its parent rather than the root map. That is what makes composition
/// work without any interval arithmetic: `descendingMap().headMap(k)` filters
/// "up to `k`" using the descending view's own (reversed) comparator, which is
/// exactly what the JDK's `DescendingSubMap.headMap` means by it.
#[derive(Clone, Copy)]
pub(crate) struct TmViewSpec {
    pub(crate) source: ObjectRef,
    /// Lower bound in the source's ordering. `lo_bounded == false` means
    /// unbounded; the `Value` is then unused. The flag is separate from a null
    /// `Value` because a custom Comparator may legitimately order a null key.
    pub(crate) lo: Value,
    pub(crate) lo_bounded: bool,
    pub(crate) lo_inclusive: bool,
    pub(crate) hi: Value,
    pub(crate) hi_bounded: bool,
    pub(crate) hi_inclusive: bool,
    /// Iteration order is the reverse of the source's.
    pub(crate) descending: bool,
}

/// Has any navigable view been created in this process?
///
/// [`tm_view_spec`] runs at the top of every TreeMap content native, on every
/// receiver, so a run that never calls `headMap`/`descendingMap` must not pay a
/// mutex acquisition for it. Monotone by design — the GC prune and the
/// recycled-identity sweep remove ENTRIES without decrementing this — so it is a
/// "have views ever existed" hint, never a count. Once one view exists the flag
/// stays set and the lock is taken; that is the same bargain
/// `tm_force_array_set` makes.
pub(crate) static TM_VIEWS_EVER_CREATED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// View `ObjectRef` (as a GC-stable [`widened_obj_key`]) -> backing map + range.
///
/// GC CONTRACT — holds three heap references (`source`, and the two bound keys)
/// that are reachable no other way, so it is wired into all four overlay hooks
/// exactly like [`snapshot_itr_backing_table`]: [`for_each_overlay_ref`] (root +
/// post-move remap), [`push_overlay_roots_for_keys`] (the per-owner rule, keyed
/// on the VIEW, so the backing map is retained precisely while the view is),
/// [`gc_prune_dead_collection_overlays`] (through `drop_overlay_entries_for_dead_keys`).
pub(crate) fn tm_view_table() -> &'static Mutex<OverlayRows<TmViewSpec>> {
    static T: std::sync::OnceLock<Mutex<OverlayRows<TmViewSpec>>> = std::sync::OnceLock::new();
    T.get_or_init(|| Mutex::new(OverlayRows::new()))
}

/// The view spec for `this`, or `None` for an ordinary TreeMap.
pub(crate) fn tm_view_spec(ctx: &dyn NativeContext, this: ObjectRef) -> Option<TmViewSpec> {
    if !TM_VIEWS_EVER_CREATED.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    let key = tm_obj_key(ctx, this);
    tm_view_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .copied()
}

/// Record `view` as a live view of `spec.source`. `view` and every `ObjectRef`
/// inside `spec` must be POST-allocation addresses.
pub(crate) fn tm_register_view(ctx: &dyn NativeContext, view: ObjectRef, spec: TmViewSpec) {
    let key = tm_obj_key(ctx, view);
    tm_view_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, spec);
    TM_VIEWS_EVER_CREATED.store(true, std::sync::atomic::Ordering::Relaxed);
}

thread_local! {
    /// Views currently being rebuilt on this thread.
    ///
    /// [`tm_resync_view`] dispatches the key's real `compareTo` /
    /// `Comparator.compare`, which is arbitrary Java and may read the very view
    /// being rebuilt. Re-entering would recurse without bound; the guard makes
    /// the inner call a no-op, so the nested read sees the view's PREVIOUS
    /// contents (stale by at most the mutation in flight) instead of a stack
    /// overflow. Same shape as `SNAPITR_FALLBACK`'s shadow-recursion guard.
    pub(crate) static TM_VIEW_RESYNC: std::cell::RefCell<std::collections::HashSet<usize>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

/// `Collections.reverseOrder(cmp)` — the view comparator of a descending view.
///
/// Degrades to `cmp` in an image with no usable `java.util.Collections`, which
/// leaves `descendingMap()` iterating ASCENDING rather than failing the call.
/// That is the same trade `native_ts_descending_set` already makes, and it is
/// the lesser evil: the alternative is that a descending view cannot be built at
/// all, which is the pre-fix behaviour (an empty map) with an exception on top.
pub(crate) fn tm_reverse_comparator(
    ctx: &mut dyn NativeContext,
    cmp: Value,
) -> Result<Value, MethodCallFailed> {
    let reversed = match cmp {
        Value::Object(Some(c)) => ctx.invoke(
            "java/util/Collections",
            "reverseOrder",
            "(Ljava/util/Comparator;)Ljava/util/Comparator;",
            &[Value::Object(Some(c))],
        )?,
        _ => ctx.invoke(
            "java/util/Collections",
            "reverseOrder",
            "()Ljava/util/Comparator;",
            &[],
        )?,
    };
    Ok(match reversed {
        Some(v @ Value::Object(Some(_))) => v,
        _ => cmp,
    })
}

/// Is `key` inside `spec`'s range, judged by the SOURCE's ordering?
///
/// The JDK's `NavigableSubMap.inRange`. Used by the mutators, which have to
/// decide before delegating: `put` outside the range is
/// `IllegalArgumentException`, `remove` outside it is a plain `null`.
pub(crate) fn tm_view_in_range(
    ctx: &mut dyn NativeContext,
    spec: &TmViewSpec,
    key: Value,
) -> Result<bool, MethodCallFailed> {
    // GC-SAFETY: each `tree_compare` dispatches interpreted Java and can move
    // the key, the bounds and the comparator, all of which are bare Rust locals
    // here. Pin them once and read every one back before each use.
    let mut cmp = tm_get_slot(ctx, spec.source, TM_FIELD_COMPARATOR);
    let cmp_pin = pin_value(ctx, cmp);
    let mut key = key;
    let key_pin = pin_value(ctx, key);
    let mut lo = spec.lo;
    let lo_pin = pin_value(ctx, lo);
    let mut hi = spec.hi;
    let hi_pin = pin_value(ctx, hi);
    let unpin_base = [cmp_pin, key_pin, lo_pin, hi_pin]
        .into_iter()
        .filter(|h| *h != usize::MAX)
        .min()
        .unwrap_or(usize::MAX);

    let mut verdict = Ok(true);
    if spec.lo_bounded {
        match tree_compare(ctx, &cmp, key, lo) {
            Ok(c) => {
                if c < 0 || (c == 0 && !spec.lo_inclusive) {
                    verdict = Ok(false);
                }
            }
            Err(e) => verdict = Err(e),
        }
        cmp = read_pinned_elem(ctx, cmp_pin, cmp);
        key = read_pinned_elem(ctx, key_pin, key);
        lo = read_pinned_elem(ctx, lo_pin, lo);
        hi = read_pinned_elem(ctx, hi_pin, hi);
    }
    let _ = lo;
    if matches!(verdict, Ok(true)) && spec.hi_bounded {
        match tree_compare(ctx, &cmp, key, hi) {
            Ok(c) => {
                if c > 0 || (c == 0 && !spec.hi_inclusive) {
                    verdict = Ok(false);
                }
            }
            Err(e) => verdict = Err(e),
        }
    }
    if unpin_base != usize::MAX {
        ctx.unpin_native_roots(unpin_base);
    }
    verdict
}

/// Rebuild `view`'s cached storage from its backing map.
///
/// Called from [`tm_sync_native_state`], i.e. on entry to every TreeMap content
/// native, so a view is never read while stale. A no-op for an ordinary map.
pub(crate) fn tm_resync_view(ctx: &mut dyn NativeContext, view: ObjectRef) -> Result<(), MethodCallFailed> {
    let spec = match tm_view_spec(ctx, view) {
        Some(s) => s,
        None => return Ok(()),
    };
    let key = tm_obj_key(ctx, view);
    if !TM_VIEW_RESYNC.with(|s| s.borrow_mut().insert(key)) {
        // Already rebuilding this view further up the stack — see the guard's
        // doc comment.
        //
        // CRATONVM_DBG_TMVIEW is the instrument that FOUND the stale-receiver
        // defect, after six theories and the lane's own best lead had all been
        // eliminated. It is kept because the next bug in this area will present
        // the same way: a view answering a DEFAULT rather than a wrong value.
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_TMVIEW").is_some() {
            eprintln!(
                "[TMVIEW] resync SKIPPED (re-entrant) view={:?}",
                view.as_ptr()
            );
        }
        return Ok(());
    }
    let result = tm_resync_view_inner(ctx, view, &spec);
    TM_VIEW_RESYNC.with(|s| {
        s.borrow_mut().remove(&key);
    });
    result
}

pub(crate) fn tm_resync_view_inner(
    ctx: &mut dyn NativeContext,
    view: ObjectRef,
    spec: &TmViewSpec,
) -> Result<(), MethodCallFailed> {
    // GC-SAFETY, and the pin ORDER matters. Everything below allocates —
    // `tm_collect_pairs` re-boxes a fast-mode map's keys, `tree_compare`
    // dispatches interpreted Java, `alloc_ref_array` collects — and `view`, the
    // source and the two bounds are all bare Rust locals copied out of the side
    // table, which the collector remaps IN THE TABLE and not in our copy. So
    // every one of them is pinned BEFORE the first GC point, and `view_pin` goes
    // first so a single `unpin_native_roots(view_pin)` unwinds the lot.
    let view_pin = ctx.pin_native_root(view);
    let src_pin = ctx.pin_native_root(spec.source);
    let mut lo = spec.lo;
    let lo_pin = pin_value(ctx, lo);
    let mut hi = spec.hi;
    let hi_pin = pin_value(ctx, hi);

    // A view of a view: bring the parent up to date first. Terminates because a
    // view's source always predates it, so the chain is finite and acyclic.
    if let Err(e) = tm_sync_native_state(ctx, spec.source) {
        ctx.unpin_native_roots(view_pin);
        return Err(e);
    }
    let source = ctx.read_native_pin(src_pin, spec.source);
    let pairs = tm_collect_pairs(ctx, source);
    let source = ctx.read_native_pin(src_pin, source);

    // Select by INDEX rather than accumulating `Value`s, so the kept entries are
    // re-read through their pins at copy time instead of being carried across
    // the comparisons.
    let pinned = PinnedPairs::new(ctx, &pairs);
    let mut cmp = tm_get_slot(ctx, source, TM_FIELD_COMPARATOR);
    let cmp_pin = pin_value(ctx, cmp);
    lo = read_pinned_elem(ctx, lo_pin, lo);
    hi = read_pinned_elem(ctx, hi_pin, hi);

    let mut kept: Vec<usize> = Vec::with_capacity(pinned.len());
    // CRATONVM_DBG_VIEWRESYNC verdict histogram - a view that comes back empty
    // has either seen no pairs at all or had every comparison land outside the
    // range, and those are different defects.
    let (mut dbg_lt, mut dbg_eq, mut dbg_gt) = (0usize, 0usize, 0usize);
    for i in 0..pinned.len() {
        // Only the key is compared; `_v` exists because `refresh` re-reads the
        // pair as a unit.
        let (mut k, mut _v) = pinned.get(&*ctx, i);
        let mut keep = true;
        if spec.lo_bounded {
            pinned.refresh(&*ctx, i, &mut k, &mut _v);
            let c = match tree_compare(ctx, &cmp, k, lo) {
                Ok(c) => c,
                Err(e) => {
                    ctx.unpin_native_roots(view_pin);
                    return Err(e);
                }
            };
            cmp = read_pinned_elem(ctx, cmp_pin, cmp);
            lo = read_pinned_elem(ctx, lo_pin, lo);
            hi = read_pinned_elem(ctx, hi_pin, hi);
            match c.cmp(&0) {
                std::cmp::Ordering::Less => dbg_lt += 1,
                std::cmp::Ordering::Equal => dbg_eq += 1,
                std::cmp::Ordering::Greater => dbg_gt += 1,
            }
            if c < 0 || (c == 0 && !spec.lo_inclusive) {
                keep = false;
            }
        }
        if keep && spec.hi_bounded {
            pinned.refresh(&*ctx, i, &mut k, &mut _v);
            let c = match tree_compare(ctx, &cmp, k, hi) {
                Ok(c) => c,
                Err(e) => {
                    ctx.unpin_native_roots(view_pin);
                    return Err(e);
                }
            };
            cmp = read_pinned_elem(ctx, cmp_pin, cmp);
            lo = read_pinned_elem(ctx, lo_pin, lo);
            hi = read_pinned_elem(ctx, hi_pin, hi);
            match c.cmp(&0) {
                std::cmp::Ordering::Less => dbg_lt += 1,
                std::cmp::Ordering::Equal => dbg_eq += 1,
                std::cmp::Ordering::Greater => dbg_gt += 1,
            }
            if c > 0 || (c == 0 && !spec.hi_inclusive) {
                keep = false;
            }
        }
        if keep {
            kept.push(i);
        }
    }
    let _ = (lo, hi, cmp);

    if cratonvm_types::flags::runtime_var("CRATONVM_DBG_VIEWRESYNC").is_ok() {
        let hi_now = read_pinned_elem(ctx, hi_pin, hi);
        eprintln!(
            "[VIEWRESYNC] view={:#x} source={:#x} pairs={} kept={} lo_bounded={} lo_inc={} \
hi_bounded={} hi_inc={} desc={} hi={:?} verdicts(lt/eq/gt)={}/{}/{}",
            ctx.read_native_pin(view_pin, view).as_ptr() as usize,
            source.as_ptr() as usize,
            pinned.len(),
            kept.len(),
            spec.lo_bounded,
            spec.lo_inclusive,
            spec.hi_bounded,
            spec.hi_inclusive,
            spec.descending,
            hi_now,
            dbg_lt,
            dbg_eq,
            dbg_gt,
        );
    }

    // Publish. The source's order is ascending, so a descending view is the same
    // selection read back to front — and the view's comparator slot (set once at
    // creation, never touched here) is already the reversed one, so the array
    // stays consistent with what `tm_binary_search` will assume about it.
    let n = kept.len();
    // GC-SAFETY, and the ORDER of the next dozen lines is the whole of it. This
    // block used to allocate `buf`, then release EVERY pin, and only then write
    // `buf` into `view` -- so between the release and the writes both objects
    // were reachable from nothing but a Rust local. A relocating collection in
    // that window left `tm_set_slot` writing the array into a from-space `view`,
    // and the view kept the `TM_FIELD_SIZE = 0` its constructor had just set:
    // `headMap(k, false)` came back EMPTY, which is the failure mode
    // `W7-1-treemap-views-and-iterator-remove-contract` exists to refuse and the
    // one a caller that only iterates cannot see. MEASURED: `RTreeRangeGc`
    // failed 12/12 with "0 entries, expected 300" and passed 12/12 under
    // `CRATONVM_ZGC_RELOCATE=0` on the same binary.
    //
    // So: pin `buf` too, re-read both through their pins immediately before each
    // use, and unpin only after the last write has landed.
    let buf = alloc_ref_array(ctx, (n * 2).max(TM_DEFAULT_CAPACITY * 2));
    let buf_pin = ctx.pin_native_root(buf);
    for (out, src_idx) in kept.iter().enumerate() {
        let slot = if spec.descending { n - 1 - out } else { out };
        let (k, v) = pinned.get(&*ctx, *src_idx);
        let buf = ctx.read_native_pin(buf_pin, buf);
        ctx.set_array_element(buf, slot * 2, k);
        ctx.set_array_element(buf, slot * 2 + 1, v);
    }
    let view = ctx.read_native_pin(view_pin, view);
    // A view never uses the fast-mode BTreeMap: its ordering can be a reversed
    // comparator, which `TreeKey`'s derived `Ord` cannot express.
    // (The key before the table lock: gc-common w26-a, see `ll_get`.)
    let view_key = tm_obj_key(ctx, view);
    tm_fast_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&view_key);
    let view = ctx.read_native_pin(view_pin, view);
    tm_set_force_array(ctx, view);
    let view = ctx.read_native_pin(view_pin, view);
    let buf = ctx.read_native_pin(buf_pin, buf);
    tm_set_slot(ctx, view, TM_FIELD_DATA, Value::Object(Some(buf)));
    tm_set_slot(ctx, view, TM_FIELD_SIZE, Value::Int(n as i32));
    // `view_pin` is the earliest handle this function pushed, so releasing it
    // unwinds `buf_pin` and the pair pins with it.
    ctx.unpin_native_roots(view_pin);
    Ok(())
}

/// Allocate a live navigable view of `source` and return it.
///
/// The single constructor behind `headMap` / `tailMap` / `subMap` (both arities)
/// and `descendingMap`; each of those natives is now just this call with the
/// right bounds, which is what keeps their six bound-comparison edge cases from
/// drifting apart again.
pub(crate) fn tm_new_range_view(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
    lo: Option<(Value, bool)>,
    hi: Option<(Value, bool)>,
    descending: bool,
) -> MethodCallResult {
    // W7-36 residual — the OTHER arm of `NavigableSubMap`'s constructor, the one
    // `tm_refuse_reversed_bounds` documents as deliberately not routed to it:
    //
    // ```text
    // } else {
    //     if (!fromStart) m.compare(lo, lo);   // type (and possibly null) check
    //     if (!toEnd)     m.compare(hi, hi);   // type check
    // }
    // ```
    //
    // So a SINGLE-bound `headMap`/`tailMap` refuses a null bound with
    // `NullPointerException` and a non-`Comparable` bound with
    // `ClassCastException`. Both compares are unconditional — they do not
    // consult `root` — which is why `container_is_empty` is passed `true` rather
    // than asked of the map: on an empty map the JDK still refuses, and asking
    // would under-throw exactly there.
    //
    // Fires only when exactly one bound is present. Two bounds take the `if`
    // arm, already covered by `tm_refuse_reversed_bounds` at the two `subMap`
    // entry points; `descendingMap` supplies neither and is untouched. Reads the
    // RECEIVER's comparator slot, as `tm_refuse_reversed_bounds` does, so a view
    // of a view is judged by its own ordering. Runs before any pin because it
    // allocates nothing: `implements_comparable` and `object_class_name` are
    // both pure `ClassId` lookups.
    if lo.is_some() != hi.is_some() {
        let bound = lo.or(hi).map(|(k, _)| k).unwrap_or(Value::Object(None));
        tree_natural_order_key_check(
            ctx,
            &tm_get_slot(ctx, source, TM_FIELD_COMPARATOR),
            bound,
            true,
        )?;
    }
    // A VIEW OF A VIEW NARROWS AND CANNOT WIDEN.
    // `NavigableSubMap.subMap` opens
    // `if (!inRange(fromKey, fromInclusive)) throw new
    //  IllegalArgumentException("fromKey out of range");`
    // for each supplied bound. Without it a caller can narrow a view and then
    // quietly get back everything it excluded, which is the one thing a range
    // view exists to prevent. MEASURED no-throw (apps/probes/TreeShadowSweep 131).
    //
    // GC-SAFETY: `tm_view_in_range` dispatches the comparator, so `source` and
    // both bounds move under it; they are pinned here and read back, and the
    // spec is re-read from the table rather than carried in a stack copy (the
    // collector remaps the TABLE, not our copy) -- the same discipline
    // `native_tm_put`'s view branch documents.
    let mut source = source;
    let mut lo = lo;
    let mut hi = hi;
    if tm_view_spec(ctx, source).is_some() {
        let src_pin0 = ctx.pin_native_root(source);
        let lo_key0 = lo.map(|(k, _)| k).unwrap_or(Value::Object(None));
        let lo_h = pin_value(ctx, lo_key0);
        let hi_key0 = hi.map(|(k, _)| k).unwrap_or(Value::Object(None));
        let hi_h = pin_value(ctx, hi_key0);
        let mut refusal: Option<MethodCallFailed> = None;
        for (present, key0, h) in [(lo.is_some(), lo_key0, lo_h), (hi.is_some(), hi_key0, hi_h)] {
            if !present || refusal.is_some() {
                continue;
            }
            let key = read_pinned_elem(ctx, h, key0);
            let src_cur = ctx.read_native_pin(src_pin0, source);
            let spec = match tm_view_spec(ctx, src_cur) {
                Some(s) => s,
                None => break,
            };
            match tm_view_in_range(ctx, &spec, key) {
                Ok(true) => {}
                Ok(false) => {
                    refusal = Some(
                        cratonvm_types::error::RuntimeError::IllegalArgumentException {
                            message: "fromKey out of range".to_string(),
                        }
                        .into(),
                    )
                }
                Err(e) => refusal = Some(e),
            }
        }
        // Refresh the three locals THROUGH the pins before releasing them: the
        // comparator dispatches above are collection points, and everything
        // below this block pins and stores `source` and both bounds.
        source = ctx.read_native_pin(src_pin0, source);
        lo = lo.map(|(k, inc)| (read_pinned_elem(ctx, lo_h, k), inc));
        hi = hi.map(|(k, inc)| (read_pinned_elem(ctx, hi_h, k), inc));
        ctx.unpin_native_roots(src_pin0);
        if let Some(e) = refusal {
            return Err(e);
        }
    }
    // GC-SAFETY: the allocation, `Collections.reverseOrder` and the resync all
    // collect, and `source` and the two bound keys are bare Rust locals that end
    // up STORED in the side table — a from-space address recorded there would
    // outlive the collection that produced it.
    let src_pin = ctx.pin_native_root(source);
    let lo_key = lo.map(|(k, _)| k).unwrap_or(Value::Object(None));
    let lo_pin = pin_value(ctx, lo_key);
    let hi_key = hi.map(|(k, _)| k).unwrap_or(Value::Object(None));
    let hi_pin = pin_value(ctx, hi_key);

    let view = match try_alloc_synthetic(ctx, "java/util/TreeMap", TM_NUM_FIELDS) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(src_pin);
            return Err(e);
        }
    };
    let view_pin = ctx.pin_native_root(view);
    // The view never runs a constructor, so nothing writes its declared
    // reference defaults -- see `init_tm_reference_defaults`. This is the site
    // the census row came from: `descendingKeySet()` mints a view here and
    // hands it to `native_tm_key_set`, whose `cached_tm_view` reads `keySet`
    // on it before anything has stored one.
    init_tm_reference_defaults(ctx, view);
    let source = ctx.read_native_pin(src_pin, source);
    let src_cmp = tm_get_slot(ctx, source, TM_FIELD_COMPARATOR);
    let view_cmp = if descending {
        match tm_reverse_comparator(ctx, src_cmp) {
            Ok(c) => c,
            Err(e) => {
                ctx.unpin_native_roots(src_pin);
                return Err(e);
            }
        }
    } else {
        src_cmp
    };
    let view = ctx.read_native_pin(view_pin, view);
    let source = ctx.read_native_pin(src_pin, source);
    // Order first, contents second: `tm_resync_view` writes the array assuming
    // this comparator is already in place.
    tm_set_slot(ctx, view, TM_FIELD_COMPARATOR, view_cmp);
    tm_set_slot(ctx, view, TM_FIELD_SIZE, Value::Int(0));
    tm_set_force_array(ctx, view);
    // Read the bounds back through their pins BEFORE the call, not inside the
    // struct literal: the allocation and `Collections.reverseOrder` above may
    // have moved either key, and a from-space address recorded in the table
    // would be what every later rebuild compared against.
    let lo_cur = read_pinned_elem(ctx, lo_pin, lo_key);
    let hi_cur = read_pinned_elem(ctx, hi_pin, hi_key);
    tm_register_view(
        ctx,
        view,
        TmViewSpec {
            source,
            lo: lo_cur,
            lo_bounded: lo.is_some(),
            lo_inclusive: lo.map(|(_, inc)| inc).unwrap_or(false),
            hi: hi_cur,
            hi_bounded: hi.is_some(),
            hi_inclusive: hi.map(|(_, inc)| inc).unwrap_or(false),
            descending,
        },
    );
    let view = ctx.read_native_pin(view_pin, view);
    // Populate eagerly so the view is correct even for a reader that reaches it
    // without going through `tm_sync_native_state` (nothing does today; the
    // guarantee is cheap and it makes the object self-consistent on return).
    if let Err(e) = tm_sync_native_state(ctx, view) {
        ctx.unpin_native_roots(src_pin);
        return Err(e);
    }
    let view = ctx.read_native_pin(view_pin, view);
    ctx.unpin_native_roots(src_pin);
    Ok(Some(Value::Object(Some(view))))
}

/// Bring `this`'s native state up to date, and the single thing every TreeMap
/// content native calls on entry.
///
/// Two jobs, deliberately behind ONE call:
///
///   1. [`tm_resync_view`] — if `this` is a `headMap`/`tailMap`/`subMap`/
///      `descendingMap` view, rebuild its cached storage from the backing map.
///      Putting it here is what makes every read native view-correct without
///      being edited: `get`, `size`, `firstKey`, `ceilingEntry`, `keySet`,
///      `entrySet`, `toString` and the rest all already called this funnel.
///   2. [`tm_materialize_deser_array`] — if `this` is a *deserialized* TreeMap
///      whose real red-black tree has never been mirrored into the side table,
///      mirror it.
///
/// Ordering matters: a resynced view leaves a non-empty side-table entry, which
/// is exactly the condition step 2 treats as "already materialized", so a view
/// is never also walked as a deserialized tree.
///
/// Fallible since 2026-08-11: step 1 dispatches the key's real `compareTo` /
/// `Comparator.compare` to test the range bounds, and a comparator that throws
/// must surface at the call, not be swallowed into a silently short view.
///
/// # Returns the receiver, and every caller must use it
///
/// Both steps ALLOCATE - step 1 dispatches the key's real `compareTo` over
/// every entry of the backing map, step 2 allocates the mirror array - so a
/// relocating collection can move `this` inside this call. Callers had no way
/// to notice: the funnel returned `()` and they went on using the `ObjectRef`
/// they had passed in.
///
/// MEASURED, `RTreeRangeGc` on the default collector: `native_tm_entry_set`
/// called this, the view relocated inside it, and the following
/// `tm_collect_pairs(ctx, this)` looked the map up in the address-keyed side
/// tables under its FROM-SPACE address - a miss, so a fresh empty state.
/// `headMap(k, false).entrySet()` came back EMPTY, 12/12, and passed 12/12
/// under `CRATONVM_ZGC_RELOCATE=0` on the same binary. An empty view is the
/// failure mode `W7-1-treemap-views-and-iterator-remove-contract` exists to
/// refuse, and a caller that only iterates cannot see it.
///
/// Returning the refreshed receiver is what makes that unmissable rather than
/// a rule to remember at 34 call sites: `#[must_use]` turns every one that
/// ignores it into a compile error.
#[must_use = "tm_sync_native_state can relocate the receiver - use the ObjectRef it returns, \
              not the one you passed in"]
pub(crate) fn tm_sync_native_state(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let pin = ctx.pin_native_root(this);
    let resync = tm_resync_view(ctx, this);
    let this = ctx.read_native_pin(pin, this);
    if let Err(e) = resync {
        ctx.unpin_native_roots(pin);
        return Err(e);
    }
    tm_materialize_deser_array(ctx, this);
    let this = ctx.read_native_pin(pin, this);
    ctx.unpin_native_roots(pin);
    Ok(this)
}

/// [`tm_sync_native_state`], with one raw argument carried across it.
///
/// The funnel ALLOCATES, so a `Value` read out of the interpreter's `args`
/// slice AFTER it holds a pre-move address: the slice is a plain Rust borrow
/// and nothing remaps it. Fourteen TreeMap natives read their key (or value, or
/// action) out of `args` on the line right after the sync call, which is the
/// same defect the funnel's own return value closes, one argument over.
///
/// The pin is FREE for a non-reference argument - `pin_value` answers
/// `usize::MAX` for `Int`/`Long`/`null` and pushes nothing - so `get(int)` and
/// friends pay one branch.
pub(crate) fn tm_sync_with_arg(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    arg: Value,
) -> Result<(ObjectRef, Value), MethodCallFailed> {
    let arg_pin = pin_value(ctx, arg);
    let synced = tm_sync_native_state(ctx, this);
    let arg = read_pinned_elem(ctx, arg_pin, arg);
    if arg_pin != usize::MAX {
        ctx.unpin_native_roots(arg_pin);
    }
    Ok((synced?, arg))
}

/// The two-argument twin of [`tm_sync_with_arg`] - `put` and `putIfAbsent`.
pub(crate) fn tm_sync_with_args2(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    a: Value,
    b: Value,
) -> Result<(ObjectRef, Value, Value), MethodCallFailed> {
    let a_pin = pin_value(ctx, a);
    let b_pin = pin_value(ctx, b);
    // `a_pin` is pushed first, so releasing the lower of the two handles
    // unwinds both.
    let base = [a_pin, b_pin]
        .into_iter()
        .filter(|h| *h != usize::MAX)
        .min();
    let synced = tm_sync_native_state(ctx, this);
    let a = read_pinned_elem(ctx, a_pin, a);
    let b = read_pinned_elem(ctx, b_pin, b);
    if let Some(base) = base {
        ctx.unpin_native_roots(base);
    }
    Ok((synced?, a, b))
}

/// `&mut` companion to `tm_materialize_deser_if_needed` for the cases it can't
/// handle: a *deserialized* TreeMap whose ordering is a **custom Comparator**
/// (or whose keys aren't fast-mode-extractable). Those need the array path —
/// a real Java `Object[]` `data` array keyed for `tm_binary_search` — which
/// requires allocation and so can't run from the `&dyn` read funnels. Called
/// at the top of the TreeMap content natives (which all hold `&mut`).
///
/// GC-safe: the allocation may move the tree, so we walk ONCE to count (storing
/// no object refs across the alloc), allocate, then re-read the (possibly
/// moved) `root`/`comparator` and walk AGAIN, storing each key/value straight
/// into the array — no allocation between read and store. Idempotent; a no-op
/// unless there is a real red-black tree and the side-table is still empty.
pub(crate) fn tm_materialize_deser_array(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let key = tm_obj_key(ctx, this);
    if tm_fast_table().lock().unwrap().contains_key(&key) {
        return;
    }
    if let Some(st) = tm_array_table().lock().unwrap().get(&key) {
        if st.size > 0 || st.data.is_some() {
            return;
        }
    }
    let nf = ctx.object_num_fields(this);
    let root_idx = match ctx.resolve_field_index("java/util/TreeMap", "root") {
        Some(i) if i < nf => i,
        _ => return,
    };
    let root = match ctx.get_field(this, root_idx) {
        Value::Object(Some(r)) => r,
        _ => return,
    };
    let cmp_idx = ctx
        .resolve_field_index("java/util/TreeMap", "comparator")
        .filter(|&i| i < nf);
    let has_comparator = cmp_idx
        .map(|i| !matches!(ctx.get_field(this, i), Value::Object(None)))
        .unwrap_or(false);
    let ev = "java/util/TreeMap$Entry";
    let (ki, vi, li, ri) = match (
        ctx.resolve_field_index(ev, "key"),
        ctx.resolve_field_index(ev, "value"),
        ctx.resolve_field_index(ev, "left"),
        ctx.resolve_field_index(ev, "right"),
    ) {
        (Some(a), Some(b), Some(c), Some(d)) => (a, b, c, d),
        _ => return,
    };

    // PASS 1: count + detect whether any key is non-fast-extractable. No
    // ObjectRef is retained past this point (the upcoming alloc may move them).
    let mut count: usize = 0;
    let mut all_extractable = true;
    {
        let mut stack: Vec<ObjectRef> = Vec::new();
        let mut cur = Some(root);
        while cur.is_some() || !stack.is_empty() {
            while let Some(n) = cur {
                stack.push(n);
                cur = tm_child(ctx, n, li);
            }
            let n = match stack.pop() {
                Some(n) => n,
                None => break,
            };
            let k = ctx.get_field(n, ki);
            if tree_key_from_value(ctx, &k).is_none() {
                all_extractable = false;
            }
            count += 1;
            cur = tm_child(ctx, n, ri);
        }
    }
    if count == 0 {
        return;
    }
    // Natural-order + all keys extractable is the `&dyn` fast path's job; leave
    // it (avoids racing two stores into the same map).
    if !has_comparator && all_extractable {
        return;
    }

    // ARRAY PATH. Allocate first (may GC), then re-read fresh and fill.
    // gc-common w20-a: "re-read fresh" needs the receiver's CURRENT address:
    // it is rooted across the allocation (the tree was re-read through its
    // pre-allocation address, and marked force-array there).
    let this_pin = ctx.pin_native_root(this);
    let buf: ObjectRef = alloc_ref_array(ctx, (count * 2).max(TM_DEFAULT_CAPACITY * 2));
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    let root2 = match ctx.get_field(this, root_idx) {
        Value::Object(Some(r)) => r,
        _ => return,
    };
    let comparator = cmp_idx
        .map(|i| ctx.get_field(this, i))
        .unwrap_or(Value::Object(None));
    let mut idx: usize = 0;
    let mut stack: Vec<ObjectRef> = Vec::new();
    let mut cur = Some(root2);
    while cur.is_some() || !stack.is_empty() {
        while let Some(n) = cur {
            stack.push(n);
            cur = tm_child(ctx, n, li);
        }
        let n = match stack.pop() {
            Some(n) => n,
            None => break,
        };
        let k = ctx.get_field(n, ki);
        let v = ctx.get_field(n, vi);
        if idx * 2 + 1 < ctx.array_length(buf) {
            ctx.set_array_element(buf, idx * 2, k);
            ctx.set_array_element(buf, idx * 2 + 1, v);
            idx += 1;
        }
        cur = tm_child(ctx, n, ri);
    }
    {
        let mut tbl = tm_array_table().lock().unwrap();
        let st = tbl.entry(key).or_default();
        st.data = Some(buf);
        st.size = idx as i32;
        st.comparator = comparator;
    }
    // Keep this map on the array path permanently (its keys/comparator can't
    // go through the fast BTreeMap).
    tm_set_force_array(ctx, this);
}

/// Read a TreeMap "slot" (`TM_FIELD_DATA`/`SIZE`/`COMPARATOR`) from the
/// address-keyed side-table. Returns layout-independent defaults when no
/// entry exists yet. The object's own fields are never consulted.
pub(crate) fn tm_get_slot(ctx: &dyn NativeContext, this: ObjectRef, slot: usize) -> Value {
    tm_materialize_deser_if_needed(ctx, this);
    let key = tm_obj_key(ctx, this);
    let tbl = tm_array_table().lock().unwrap();
    if let Some(st) = tbl.get(&key) {
        return match slot {
            TM_FIELD_DATA => Value::Object(st.data),
            TM_FIELD_SIZE => Value::Int(st.size),
            TM_FIELD_COMPARATOR => st.comparator,
            _ => Value::Object(None),
        };
    }
    match slot {
        TM_FIELD_SIZE => Value::Int(0),
        _ => Value::Object(None),
    }
}

/// Write a TreeMap "slot" into the address-keyed side-table (creating the
/// entry on first write). The object's own fields are never touched — the
/// side-table is the sole authoritative store (see `TmArrayState`).
pub(crate) fn tm_set_slot(ctx: &mut dyn NativeContext, this: ObjectRef, slot: usize, v: Value) {
    let key = tm_obj_key(ctx, this);
    // Captured under the same lock that is about to overwrite it, so the
    // structural-change test below cannot race its own write.
    let mut size_changed = false;
    {
        let mut tbl = tm_array_table().lock().unwrap();
        let st = tbl.entry(key).or_default();
        if slot == TM_FIELD_SIZE {
            size_changed = !matches!(v, Value::Int(n) if n == st.size);
        }
        match slot {
            TM_FIELD_DATA => {
                st.data = match v {
                    Value::Object(o) => o,
                    _ => None,
                }
            }
            TM_FIELD_SIZE => {
                st.size = match v {
                    Value::Int(n) => n,
                    _ => 0,
                }
            }
            TM_FIELD_COMPARATOR => st.comparator = v,
            _ => {}
        }
    }
    // Mirror the entry count to the real JDK `size` field (resolved by name — the
    // synthetic slot would corrupt an unrelated real field, see tm_obj_key doc).
    // Real-bytecode TreeMap.writeObject does `s.writeInt(size)` reading this
    // field directly; without the mirror it stayed 0, so a populated TreeMap
    // serialized as size=0 + the entries, which a peer read back as empty.
    if slot == TM_FIELD_SIZE {
        if let Value::Int(n) = v {
            if let Some(real) = ctx.resolve_field_index("java/util/TreeMap", "size") {
                if real < ctx.object_num_fields(this) {
                    ctx.set_field(this, real, Value::Int(n));
                }
            }
        }
        // `modCount` is the OTHER field real `TreeMap` bytecode reads without
        // going through a native, and it was pinned at 0 for the life of every
        // map this VM built. MEASURED, 20 puts + 1 remove: HotSpot 25.0.3+9
        // reports `modCount=21`, CratonVM reported `modCount=0`.
        //
        // A size change is exactly a STRUCTURAL modification, which is exactly
        // what `TreeMap` bumps `modCount` for — and the correspondence holds in
        // both directions, which is why this is gated on the size actually
        // changing rather than on the setter being called: `put` of an EXISTING
        // key does not change the size and does not bump `modCount` on HotSpot
        // either, while `put` of a new key, `remove` of a present key and
        // `clear` of a non-empty map each do both. A redundant same-value write
        // through this setter must not inflate the count, so the test is on the
        // value, not on the call.
        if size_changed {
            bump_map_mod_count(ctx, this);
        }
    }
    // Mirror the comparator to the real JDK `comparator` field. Real-bytecode
    // `TreeMap.writeObject`→`defaultWriteObject` serializes this field directly;
    // without the mirror a custom-comparator TreeMap serialized `comparator=null`
    // (the synthetic comparator lived only in the side-table), so a peer read it
    // back as a natural-order map with a mis-ordered tree. We mirror the *real
    // serializable* form (a synthetic `Comparator$Native` substituted by its JDK
    // singleton — `defaultWriteObject`'s field serialization does NOT run the
    // `writeReplace` hook, so the substitution must happen here). The side-table
    // keeps the original (synthetic) comparator so `comparator_compare` retains
    // its fast tag dispatch; only the real field — read solely by serialization
    // — holds the singleton.
    if slot == TM_FIELD_COMPARATOR {
        if let Some(real) = ctx.resolve_field_index("java/util/TreeMap", "comparator") {
            if real < ctx.object_num_fields(this) {
                let serializable = replace_synthetic_comparator_for_ser(ctx, v);
                ctx.set_field(this, real, serializable);
            }
        }
    }
}

/// Returns true if this TreeMap is currently using the BTreeMap fast path.
/// Determined by:
///   1. comparator field is null/absent (custom Comparator → array path), AND
///   2. a fast-mode entry exists in `tm_fast_table` (set on first put when
///      the first key is extractable into a TreeKey).
///
/// Empty TreeMaps with null comparator are tentatively "fast-eligible" —
/// the first non-extractable key flips them to array mode.
pub(crate) fn tm_is_fast_mode(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    tm_materialize_deser_if_needed(ctx, this);
    let key = tm_obj_key(ctx, this);
    tm_fast_table().lock().unwrap().contains_key(&key)
}

/// True if the comparator slot is null. Custom Comparator forces array mode
/// because we can't replicate user-defined ordering in a Rust BTreeMap.
pub(crate) fn tm_has_no_comparator(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    matches!(
        tm_get_slot(ctx, this, TM_FIELD_COMPARATOR),
        Value::Object(None)
    )
}

/// The JDK's `compare(key, key)` — "type (and possibly null) check" in its own
/// comment — for the sorted containers, hoisted to the natives that can reach an
/// EMPTY one.
///
/// Three measured divergences, one cause:
///
/// ```text
/// TreeMap.nullKeyPut              HotSpot NullPointerException  CratonVM no-throw
/// TreeMap.nullKeyGet              HotSpot NullPointerException  CratonVM no-throw
/// TreeSet.nullAddNaturalOrdering  HotSpot NullPointerException  CratonVM no-throw
/// TreeSet.incomparableFirstAdd    HotSpot ClassCastException    CratonVM no-throw
/// ```
///
/// None of them is an extra rule. Under natural ordering every insert and every
/// lookup runs `((Comparable) key).compareTo(other)`, which raises both: the
/// `checkcast` in front of the call raises `ClassCastException` for a
/// non-`Comparable` receiver, and the invoke itself raises
/// `NullPointerException` for a null one. `TreeMap.getEntry` opens with an
/// explicit `if (key == null) throw new NullPointerException()` for exactly
/// that reason, and `addEntryToEmptyMap` writes the self-compare out by hand
/// because on an empty container no comparison would otherwise happen at all.
///
/// So the gap here is *two* gaps that look like one. [`compare_via_compare_to`]
/// already raises the ClassCastException half — but only once there is
/// something to compare against, which is why only the FIRST add diverged. It
/// never raises the null half at all, and deliberately: it orders `null` first,
/// because its other caller is `Arrays.sort(Object[])`, whose contract that is.
/// A sorted map has the opposite contract, so the check has to live here rather
/// than be added there.
///
/// Scoped to natural ordering — `comparator == null` — because that is where
/// the JDK scopes it. `TreeMap.put`: "@throws NullPointerException if the
/// specified key is null and this map uses natural ordering, **or its
/// comparator does not permit null keys**"; with a comparator supplied the JDK
/// asks the comparator and nothing else. `new TreeSet<>(Comparator.nullsFirst(..))`
/// legitimately holds a null, and a comparator over a non-`Comparable` element
/// type is the ordinary reason to supply one — this file already documents a
/// `Set<Class<?>>` ordered by a name-comparator above `tree_key_from_value`, and
/// `compare_via_compare_to` documents Spring catching the CCE from the other
/// shape. Refusing under a comparator would be the over-throw.
///
/// The CCE message is `compare_via_compare_to`'s, verbatim, so the two sites
/// that model the same `checkcast` cannot drift into two different reports. The
/// NPE carries no message, like [`ad_refuse_null`]: HotSpot's has none here and
/// `thrownDetail` prints a message when there is one.
///
/// `container_is_empty` gates the `Comparable` half only, and gates it exactly
/// where the JDK gates it: the self-compare is written out by hand in
/// `addEntryToEmptyMap` and nowhere else, because a non-empty container reaches
/// the same `checkcast` through the real comparison a moment later — here, via
/// `compare_via_compare_to` inside `tm_binary_search`/`ts_binary_search`.
/// Running the hierarchy walk on every populated `get`/`put`/`add` would buy
/// nothing and would put a class-hierarchy walk on the fast-mode `TreeMap`
/// path, which today performs no comparison at all. The null half is
/// unconditional because it is a `matches!` and because
/// `compare_via_compare_to` will never supply it.
/// The NAVIGATION family's key contract, which is not [`tree_natural_order_key_check`]'s.
///
/// `TreeMap.getEntry` opens `if (key == null) throw new NullPointerException();`
/// and refuses a null on an EMPTY map. `getCeilingEntry` / `getFloorEntry` /
/// `getHigherEntry` / `getLowerEntry` do not: they start `Entry<K,V> p = root;`
/// and the refusal is a side effect of the first comparison, so an empty map
/// answers `null` for the same call. MEASURED both ways
/// (`apps/probes/TreeShadowSweep` 71 and 77-78).
///
/// With a comparator installed there is no cast and no implicit null check at
/// all — a null-tolerant comparator is exactly why `TreeMap` HAS this split —
/// which [`tree_natural_order_key_check`] already encodes.
pub(crate) fn tm_nav_key_check(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    key: Value,
) -> Result<(), MethodCallFailed> {
    let size = match tm_get_slot(ctx, this, TM_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if size == 0 {
        return Ok(());
    }
    let comparator = tm_get_slot(ctx, this, TM_FIELD_COMPARATOR);
    // `container_is_empty = true` selects the arm that also raises the
    // `ClassCastException`: on a non-empty map the JDK DOES compare, so both
    // refusals are due, and the search below would otherwise reach a bare
    // `compareTo` on a non-`Comparable` receiver.
    tree_natural_order_key_check(ctx, &comparator, key, true)
}

/// The same contract for the `TreeSet` navigation methods, which delegate to
/// the map family in the JDK (`TreeSet.ceiling` is `m.ceilingKey(e)`).
pub(crate) fn ts_nav_elem_check(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    elem: Value,
) -> Result<(), MethodCallFailed> {
    let (_, size, comparator) = ts_state(ctx, this);
    if size == 0 {
        return Ok(());
    }
    tree_natural_order_key_check(ctx, &comparator, elem, true)
}

pub(crate) fn tree_natural_order_key_check(
    ctx: &dyn NativeContext,
    comparator: &Value,
    key: Value,
    container_is_empty: bool,
) -> Result<(), MethodCallFailed> {
    if !matches!(comparator, Value::Object(None)) {
        return Ok(());
    }
    match key {
        Value::Object(None) => Err(RuntimeError::NullPointerException { message: None }.into()),
        Value::Object(Some(k)) if container_is_empty && !implements_comparable(ctx, k) => {
            let cname = object_class_name_dotted(ctx, k);
            Err(cratonvm_types::error::RuntimeError::ClassCastException {
                message: format!("class {cname} cannot be cast to class java.lang.Comparable"),
            }
            .into())
        }
        // A primitive `Value` never reaches a `(Ljava/lang/Object;)` key
        // parameter; treat anything else as already checked rather than
        // inventing a refusal for a shape the registrations cannot produce.
        _ => Ok(()),
    }
}

/// `true` when a natively-managed TreeMap holds no entries, in either storage
/// mode — `TM_FIELD_SIZE` is maintained by the fast-mode BTreeMap path as well
/// as the array path (`native_tm_put`'s own fast-mode eligibility test reads
/// it). Only ever used to decide whether the JDK's empty-map self-compare
/// applies, so a wrong answer under-throws rather than over-throws.
pub(crate) fn tm_is_empty_now(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    !matches!(tm_get_slot(ctx, this, TM_FIELD_SIZE), Value::Int(n) if n > 0)
}

/// Borrow the fast-mode BTreeMap mutably and call `f`. Creates the entry
/// if missing. Caller must ensure they only invoke this when fast mode
/// is applicable (no comparator, etc.).
pub(crate) fn tm_fast_with<R>(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    f: impl FnOnce(&mut std::collections::BTreeMap<TreeKey, Value>) -> R,
) -> R {
    let key = tm_obj_key(ctx, this);
    let mut map = tm_fast_table().lock().unwrap();
    let bt = map.entry(key).or_default();
    f(bt)
}

/// "Sticky" flag side-table — once a TreeMap is forced to array mode
/// (e.g. by a non-extractable key) it stays there for its lifetime so
/// we never split state across both stores.
///
/// Keyed by `widened_obj_key`, exactly like `tm_array_table`/`tm_fast_table`,
/// and therefore subject to the same recycle hazard those two document: the
/// packed key embeds a 32-bit identity hash, so a leftover entry under a
/// reclaimed map's key can be inherited by a later map that mints the same
/// hash. It holds no `ObjectRef`, so a stale entry is not a use-after-free —
/// but the newcomer silently starts life pinned to array mode, and the table
/// grows without bound for the life of the process. It was the one
/// `widened_obj_key`-keyed table `gc_prune_dead_collection_overlays` did not
/// sweep; it does now. Poison-recovered on read/write for the same reason the
/// GC funnel is: an `unwrap()` that panics here would poison the lock and make
/// every later `tm_force_array_mode` call abort the VM.
pub(crate) fn tm_force_array_set() -> &'static Mutex<StdHashMap<usize, ()>> {
    static T: std::sync::OnceLock<Mutex<StdHashMap<usize, ()>>> = std::sync::OnceLock::new();
    T.get_or_init(|| Mutex::new(StdHashMap::new()))
}
pub(crate) fn tm_force_array_mode(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let key = tm_obj_key(ctx, this);
    tm_force_array_set()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&key)
}
pub(crate) fn tm_set_force_array(ctx: &dyn NativeContext, this: ObjectRef) {
    let key = tm_obj_key(ctx, this);
    tm_force_array_set()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, ());
}

/// Test hook: report whether `this`'s sticky array-mode flag is present.
/// Exists so the prune regression test can observe the table directly
/// rather than inferring it from downstream mode selection.
#[doc(hidden)]
pub fn __test_tm_force_array_mode(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    tm_force_array_mode(ctx, this)
}

/// Test hook: set `this`'s sticky array-mode flag.
#[doc(hidden)]
pub fn __test_tm_set_force_array(ctx: &dyn NativeContext, this: ObjectRef) {
    tm_set_force_array(ctx, this)
}

/// Test hook: number of entries in the sticky array-mode table.
///
/// The prune regression test asserts on this rather than on
/// `tm_force_array_mode(dead)`: pruning also drops the dead object's slot from
/// `obj_key_registry`, so a post-prune `widened_obj_key` on the same address
/// mints a FRESH generation and therefore a different packed key. A
/// presence-check would then read `false` whether or not the table was swept.
/// Counting entries distinguishes "swept" from "re-keyed".
#[doc(hidden)]
pub fn __test_tm_force_array_len() -> usize {
    tm_force_array_set()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len()
}

/// Migrate any fast-mode entries to the array store, then remove the
/// fast-mode side-table entry. Called when a non-extractable key arrives
/// at a map that previously had fast-mode entries — keeps state coherent
/// across the mode flip without losing data.
/// # `this` is `&mut` — N2 of `WORKER-5-NOTE-11`
///
/// This function ALREADY knew its receiver moves: it declares `mut this` and
/// reassigns it from `tm_install_backing_array`'s refreshed value below. Taking
/// the receiver BY VALUE threw that refresh away at the return, so the caller
/// kept the pre-move address.
///
/// One of its two call sites had been protected by hand, with the reason
/// spelled out — *"The migration allocates (and can move this map); pin and
/// refresh the receiver across it"* — and the other had not, going straight on
/// to `tm_state(ctx, this)`. That is "the correct helper exists and one call
/// site uses it", and it is why the receiver is `&mut` rather than a second
/// hand-written pin at the missed site: the caller cannot now forget.
pub(crate) fn tm_migrate_fast_to_array(ctx: &mut dyn NativeContext, this_out: &mut ObjectRef) {
    let mut this = *this_out;
    let key = tm_obj_key(ctx, this);
    // Snapshot fast-mode entries first, then drop the side-table entry.
    let entries: Vec<(TreeKey, Value)> = tm_fast_with(ctx, this, |bt| {
        bt.iter().map(|(k, v)| (k.clone(), *v)).collect()
    });
    tm_fast_table().lock().unwrap().remove(&key);
    if entries.is_empty() {
        *this_out = this;
        return;
    }
    // Box keys back to Java wrappers and insert via the array path.
    let boxed: Vec<(Value, Value)> = entries
        .into_iter()
        .map(|(k, v)| (tree_key_to_value(ctx, &k), v))
        .collect();
    // Reset size counter — the array put loop will re-establish it.
    tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(0));
    // Ensure data array exists.
    if matches!(tm_get_slot(ctx, this, TM_FIELD_DATA), Value::Object(None)) {
        let (refreshed, _) = tm_install_backing_array(ctx, this, TM_DEFAULT_CAPACITY * 2);
        this = refreshed;
    }
    // Now insert each entry through the array path. The fast-mode check
    // in `native_tm_put` will skip because `tm_force_array_set` is set
    // before we get here (caller's responsibility).
    let this_pin = ctx.pin_native_root(this);
    for (k, v) in boxed {
        let this = ctx.read_native_pin(this_pin, this);
        let _ = native_tm_put(ctx, &[Value::Object(Some(this)), k, v]);
    }
    *this_out = this;
}

/// Convert a TreeKey back to a Java `Value` for return values that need
/// the original key (firstKey, ceilingKey, etc.). Boxes primitives to
/// the corresponding wrapper class; reuses the standard `Integer.valueOf`
/// / `Long.valueOf` / `String` paths via the NativeContext.
pub(crate) fn tree_key_to_value(ctx: &mut dyn NativeContext, k: &TreeKey) -> Value {
    // Box a primitive back to its wrapper via `<Wrapper>.valueOf`, preserving
    // the original Java type recorded in the TreeKey variant.
    let box_via = |ctx: &mut dyn NativeContext, cls: &str, desc: &str, arg: Value| -> Value {
        ctx.invoke(cls, "valueOf", desc, &[arg])
            .ok()
            .flatten()
            .unwrap_or(Value::Object(None))
    };
    match k {
        // `tree_key_from_value` refuses any String whose decode was lossy, so
        // every `Str` here re-encodes to exactly the text it came from and
        // `from_utf16_lossy` is lossless in practice for these units.
        TreeKey::Str(units) => {
            let text = String::from_utf16_lossy(units);
            Value::Object(Some(ctx.create_string(&text)))
        }
        TreeKey::I32(i) => box_via(
            ctx,
            "java/lang/Integer",
            "(I)Ljava/lang/Integer;",
            Value::Int(*i),
        ),
        TreeKey::I64(l) => box_via(
            ctx,
            "java/lang/Long",
            "(J)Ljava/lang/Long;",
            Value::Long(*l),
        ),
        TreeKey::Char(c) => box_via(
            ctx,
            "java/lang/Character",
            "(C)Ljava/lang/Character;",
            Value::Int(*c as i32),
        ),
        TreeKey::Byte(b) => box_via(
            ctx,
            "java/lang/Byte",
            "(B)Ljava/lang/Byte;",
            Value::Int(*b as i32),
        ),
        TreeKey::Short(s) => box_via(
            ctx,
            "java/lang/Short",
            "(S)Ljava/lang/Short;",
            Value::Int(*s as i32),
        ),
        TreeKey::Bool(b) => box_via(
            ctx,
            "java/lang/Boolean",
            "(Z)Ljava/lang/Boolean;",
            Value::Int(if *b { 1 } else { 0 }),
        ),
    }
}

// TreeSet — sorted set backed by sorted array.
//
// These are NOT object slot indices. `ts_get_slot`/`ts_set_slot` key an
// address-keyed side table on them and never touch the object's own fields, so
// nothing here describes a layout and nothing here may be handed to an
// allocator: real `java.util.TreeSet` declares exactly ONE instance field
// (`javap -p`, JDK 25.0.3.9: `private transient NavigableMap<E,Object> m`), and
// every set-view native used to ask for three. Allocate through
// `try_alloc_declared_width`, which asks for what the loaded class declares.
// W7-66-live-over-allocations.md.
pub(crate) const TS_FIELD_DATA: usize = 0; // Object[] sorted elements
pub(crate) const TS_FIELD_SIZE: usize = 1; // Int: number of elements
pub(crate) const TS_FIELD_COMPARATOR: usize = 2; // Comparator or null
pub(crate) const TS_NUM_FIELDS: usize = 3;
pub(crate) const TS_DEFAULT_CAPACITY: usize = 16;

// `java/util/TreeSet$Itr` — the fabricated snapshot iterator `native_ts_iterator`
// and `native_ts_descending_iterator` hand back (the real iterator behind
// `TreeSet.iterator()` is `TreeMap$KeyIterator`, so under `--jdk-only` this shape
// is refused and `real_snapshot_iterator` stands in instead).
//
// `LAST_RET` is the fourth slot, added 2026-08-11 for W7-1 family 2. The
// snapshot does not shift when the backing set loses an element, so `cursor`
// alone cannot distinguish "next() has been called" from "remove() already
// consumed that element" — see `native_ts_itr_remove`.
pub(crate) const TS_ITR_FIELD_DATA: usize = 0;
pub(crate) const TS_ITR_FIELD_CURSOR: usize = 1;
pub(crate) const TS_ITR_FIELD_OWNER: usize = 2;
pub(crate) const TS_ITR_FIELD_LAST_RET: usize = 3;
/// The fail-fast generation this cursor was minted at — the `TreeSet`-carried
/// twin of the `expectedModCount` the `HashMap$KeyItr` family resolves by name.
/// A slot rather than a name because `java/util/TreeSet$Itr` is fabricated (no
/// JDK declares it), so there is no declared field to resolve.
pub(crate) const TS_ITR_FIELD_EXPECTED_MOD: usize = 4;
pub(crate) const TS_ITR_NUM_FIELDS: usize = 5;

/// The map a `TreeSet$Itr` is fail-fast against.
///
/// `TreeMap.keySet()` mints a `TreeMap$KeySet` whose elements live in the
/// `ts_array_table` side-table and which stashes its source map in the LAST
/// capacity slot of the element array — [`ts_view_source`] is the reader, and
/// it is what already gives this view its write-through. A plain `TreeSet` has
/// no such marker and answers `None`, which is the no-check case: a set that is
/// nobody's view has no source generation to watch.
///
/// `TreeSet.descendingSet()` puts another `TreeSet` behind the same marker;
/// that answers `None` from [`map_itr_mod_count`] below (a set keeps no
/// `modCount`), so it also lands on the no-check path rather than on a wrong
/// one — the same map-vs-set distinction [`ts_source_remove`] makes.
pub(crate) fn ts_itr_comod_source(ctx: &dyn NativeContext, itr: ObjectRef) -> Option<ObjectRef> {
    if ctx.object_num_fields(itr) <= TS_ITR_FIELD_OWNER {
        return None;
    }
    let owner = match ctx.get_field(itr, TS_ITR_FIELD_OWNER) {
        Value::Object(Some(o)) => o,
        _ => return None,
    };
    ts_view_source(ctx, owner)
}

/// `expectedModCount = modCount` for the `TreeSet`-carried cursor.
pub(crate) fn ts_itr_seed_expected(ctx: &mut dyn NativeContext, itr: ObjectRef) {
    if !map_itr_failfast_enabled() || ctx.object_num_fields(itr) <= TS_ITR_FIELD_EXPECTED_MOD {
        return;
    }
    let Some(src) = ts_itr_comod_source(&*ctx, itr) else {
        return;
    };
    if let Some(seen) = map_itr_mod_count(&*ctx, src) {
        ctx.set_field(itr, TS_ITR_FIELD_EXPECTED_MOD, Value::Int(seen));
    }
}

/// The comodification test for the `TreeSet`-carried cursor, i.e. the one
/// `TreeMap.keySet().iterator()` hands out. Fails open on every arm, exactly as
/// [`map_itr_check_comod`] does: an iterator minted before this slot existed, a
/// set that is nobody's view, or a source with no generation, all keep the old
/// never-throw behaviour rather than guessing.
pub(crate) fn ts_itr_check_comod(ctx: &dyn NativeContext, itr: ObjectRef) -> Result<(), MethodCallFailed> {
    if !map_itr_failfast_enabled() || ctx.object_num_fields(itr) <= TS_ITR_FIELD_EXPECTED_MOD {
        return Ok(());
    }
    let expected = match ctx.get_field(itr, TS_ITR_FIELD_EXPECTED_MOD) {
        Value::Int(v) => v,
        _ => return Ok(()),
    };
    let Some(src) = ts_itr_comod_source(ctx, itr) else {
        return Ok(());
    };
    match map_itr_mod_count(ctx, src) {
        Some(actual) if actual != expected => {
            Err(cratonvm_types::error::RuntimeError::ConcurrentModificationException.into())
        }
        _ => Ok(()),
    }
}

/// Address-keyed TreeSet state side-table. Mirrors `TmArrayState`: in
/// real-JDK mode `java.util.TreeSet` (and any subclass) has the real JDK
/// field layout, so the synthetic slots 0/1/2 do not exist. All state —
/// backing data array, size, comparator — lives here, keyed by the
/// receiver's address, so it is layout-independent.
#[derive(Clone)]
pub(crate) struct TsArrayState {
    pub(crate) data: Option<ObjectRef>,
    pub(crate) size: i32,
    pub(crate) comparator: Value,
    /// While the set holds exactly two elements: whether HotSpot's red-black
    /// tree would be rooted at the LARGER one ([`ts_note_pair_insert`],
    /// [`ts_note_pair_remove`]). Not an `ObjectRef`, so the GC hooks have
    /// nothing to visit here.
    pub(crate) pair_root_max: bool,
}
impl Default for TsArrayState {
    fn default() -> Self {
        TsArrayState {
            data: None,
            size: 0,
            comparator: Value::Object(None),
            pair_root_max: false,
        }
    }
}

/// `CRATONVM_SPLITERATOR_TREE_PAIR_ROOT` -- default ON, `=0` restores the
/// fixed "a two-element tree never splits" answer. HotSpot's
/// `TreeMap.KeySpliterator.trySplit()` first hands off the elements below the
/// tree's ROOT, so it declines exactly when the root is the first element it
/// would walk; `DescendingKeySpliterator` declines when the root is the LAST
/// one. In a red-black tree of three or more nodes the root always has two
/// children, so only a two-element set depends on history: its root is the
/// element that was there first (a second insert never rotates), or what a
/// three-node tree (always rooted at its middle element) leaves behind --
/// the middle one, or the largest after the middle one is deleted
/// (`deleteEntry` copies the successor into the root). `buildFromSorted`
/// roots two elements at the first. `R16SpliterW9` `TreeSet.two` (inserted
/// 1, 2: root 1, no split) vs `TreeSet.two.rev` (inserted 2, 1: root 2,
/// split). Round 16 wave 13, lane compatcoll.
pub(crate) fn spliterator_tree_pair_root_enabled() -> bool {
    // Read live, not latched: one read per bulk copy / `spliterator()` /
    // `unordered()`, and no process-wide cache to keep per VM.
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_SPLITERATOR_TREE_PAIR_ROOT")
}

/// The two-element root rule of [`TsArrayState::pair_root_max`] as a pure
/// function, for an INSERT at `pos` into a set of `old_size` elements:
/// `Some(root is the larger)` when the insert makes two elements.
pub(crate) fn pair_root_after_insert(old_size: i32, pos: usize) -> Option<bool> {
    // 1 -> 2: the old element stays the (black) root; it is the larger one
    // exactly when the new element went in front of it.
    (old_size == 1).then_some(pos == 0)
}

/// [`pair_root_after_insert`] for a REMOVAL at `idx` from `old_size`
/// elements: a three-node red-black tree is rooted at its middle element, so
/// removing the smallest leaves the root as the smaller of two; removing the
/// largest leaves it as the larger; removing the middle (the root, with two
/// children) copies its successor -- the largest -- into the root.
pub(crate) fn pair_root_after_remove(old_size: i32, idx: usize) -> Option<bool> {
    (old_size == 3).then_some(idx != 0)
}

/// Record [`pair_root_after_insert`] on `this`'s side-table row. No
/// allocation, no Java.
pub(crate) fn ts_note_pair_insert(ctx: &dyn NativeContext, this: ObjectRef, old_size: i32, pos: usize) {
    if let Some(root_max) = pair_root_after_insert(old_size, pos) {
        ts_set_pair_root_max(ctx, this, root_max);
    }
}

/// Record [`pair_root_after_remove`] on `this`'s side-table row. No
/// allocation, no Java.
pub(crate) fn ts_note_pair_remove(ctx: &dyn NativeContext, this: ObjectRef, old_size: i32, idx: usize) {
    if let Some(root_max) = pair_root_after_remove(old_size, idx) {
        ts_set_pair_root_max(ctx, this, root_max);
    }
}

pub(crate) fn ts_set_pair_root_max(ctx: &dyn NativeContext, this: ObjectRef, root_max: bool) {
    let key = tm_obj_key(ctx, this);
    if let Ok(mut tbl) = ts_array_table().lock() {
        if let Some(st) = tbl.get_mut(&key) {
            st.pair_root_max = root_max;
        }
    }
}

/// Whether the native `TreeSet` at the root of `this`'s view chain (`this`
/// itself for a plain set) holds exactly two elements rooted at the larger.
/// `false` for a `TreeMap` root, whose tree shape this side-table does not
/// record: a map filled in ascending order is rooted at its smaller key,
/// which is what `false` answers. Pure reads.
pub(crate) fn ts_pair_root_is_max(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let mut cur = this;
    for _ in 0..NAV_SHAPE_MAX_DEPTH {
        let next = match tm_view_spec(ctx, cur) {
            Some(spec) => Some(spec.source),
            None => ts_view_source(ctx, cur),
        };
        match next {
            Some(n) => cur = n,
            None => break,
        }
    }
    if is_tree_map_receiver(ctx, cur) {
        return false;
    }
    let key = tm_obj_key(ctx, cur);
    match ts_array_table().lock() {
        Ok(tbl) => tbl
            .get(&key)
            .is_some_and(|st| st.size == 2 && st.pair_root_max),
        Err(_) => false,
    }
}
pub(crate) fn ts_array_table() -> &'static Mutex<OverlayRows<TsArrayState>> {
    static T: std::sync::OnceLock<Mutex<OverlayRows<TsArrayState>>> = std::sync::OnceLock::new();
    T.get_or_init(|| Mutex::new(OverlayRows::new()))
}

// ---------------------------------------------------------------------------
// GC integration for overlay-backed collections
// ---------------------------------------------------------------------------
//
// LinkedList, LinkedHashMap, TreeMap and TreeSet model their backing storage
// (bucket/element arrays, list/tree nodes, comparator) in process-global Rust
// side-tables ("overlays") keyed by a GC-stable object key, NOT in the Java
// object's own fields. The VM's field-tracing root scanner therefore never
// sees those backing objects. When the ONLY reference to a backing array is
// the overlay, a moving young-gen GC reclaims or relocates it and the overlay
// is left holding a dangling pointer — which later surfaces as a read of a
// zero-header object (`class_id=0`, `num_slots=0`), e.g. `LinkedHashMap.get`
// crashing on DaCapo's `Config` map (`lhm_state` → `array_length` on a freed
// bucket array).
//
// These two functions plug the overlays into the GC the same way the
// Integer/Boolean valueOf cache and the LambdaMetafactory CallSite cache are
// (see `roots.rs` step 15/16 + `gc.rs`): `..._roots` reports every top-level
// ObjectRef the overlays hold so the collector keeps the whole backing graph
// live and relocates it; `..._refs` repoints the overlays to the moved
// addresses afterwards. Only the top-level entry points need handling — once
// the bucket/element array (and head/tail/data) are live roots, the collector
// traces and updates the rest of the node graph (array elements + node fields)
// through the normal heap walk.

/// The `skip` argument of [`for_each_overlay_ref`] that skips nothing: the
/// VM-less hooks, which walk every VM's rows.
pub(crate) fn no_skip(_key: usize) -> bool {
    false
}

/// Visit — in place, mutably — every top-level `ObjectRef` stored in any
/// collection overlay side-table, calling `f` on each.
///
/// This is the SINGLE funnel both GC hooks share. `gc_scan_collection_overlay_roots`
/// passes a closure that copies each ref into the roots vector; `gc_update_collection_overlay_refs`
/// passes one that rewrites each ref through the pointer map. Routing both
/// through one enumeration is deliberate: the B1/V1 use-after-free arose
/// precisely because `tm_fast_table` got wired into neither hook, and the
/// older shape (two hand-maintained copies of the table list) made it easy
/// to add a table to one and forget the other. With this funnel, a newly
/// added overlay only has to be listed HERE once — both scanning and
/// remapping pick it up automatically, and the
/// `all_overlay_object_values_*` tests (gc_relocation_harness) assert each
/// known table is reached.
///
/// Both `Value::Object(Some(_))` slots and bare `Option<ObjectRef>` slots
/// (TreeMap/TreeSet `data`) are unified to a `&mut ObjectRef` — the only
/// shape the GC actually cares about. `tm_fast_table` keys are owned
/// `TreeKey`s (String/i32/i64), never ObjectRefs, so iterating values only
/// is correct and never perturbs the BTreeMap ordering.
/// Walk every top-level `ObjectRef` held by the overlay-backed collections.
///
/// `for_rooting` selects the caller's intent:
///   - `true`  (GC root scan): skip the `LinkedHashMap` entries whose structural
///     refs are already heap-rooted (see [`lhm_heap_backed`]). Rooting them too
///     would pin dead LHMs/LHSs forever and leak the young gen.
///   - `false` (post-GC remap): visit ALL entries — the cached refs of *live*
///     LHMs that relocated must still be repointed; dead entries are not in the
///     `pointer_map` so their stale refs are left untouched (never read again).
///
/// `skip(key)` drops every entry keyed by that packed key (gc-common w12-c:
/// the VM-scoped hooks skip the keys [`foreign_overlay_keys`] proved belong
/// to another VM). [`no_skip`] visits everything, as before.
pub(crate) fn for_each_overlay_ref(
    for_rooting: bool,
    skip: impl Fn(usize) -> bool,
    mut f: impl FnMut(&'static str, usize, &mut ObjectRef),
) {
    // PERF (overlay-empty-skip): this runs on EVERY GC. Pruning keeps each
    // overlay table bounded to live collections, but an app that uses none of a
    // given collection type leaves that table empty — yet we still locked it and
    // built an iterator every GC. Each block now early-skips when its table is
    // empty (a single lock + `is_empty()` check, no iterator setup). For the
    // LinkedHashMap block this additionally avoids acquiring the second
    // `lhm_heap_backed` lock when there is nothing to walk. Behaviour is
    // identical: iterating an empty map invokes `f` zero times.
    //
    // GC-CORRECTNESS (poison-recover): every overlay table lock below recovers
    // its guard on a poisoned `Mutex` (`unwrap_or_else(|e| e.into_inner())`)
    // rather than skipping the table on an `if let Ok(_)`. A `std::sync::Mutex`
    // poisons when any thread panics while holding it; an `Ok`-only guard would
    // then SILENTLY SKIP that overlay for the rest of the process. During a
    // moving GC that means the table's backing-array `ObjectRef`s are neither
    // rooted (root scan, `for_rooting=true`) NOR remapped (post-GC remap,
    // `for_rooting=false`) — so the backing array is reclaimed or relocated out
    // from under the still-live overlay → use-after-free on the next collection
    // access. A GC must NEVER skip a root: a poisoned table's contents may be
    // momentarily inconsistent, but a present (even stale) ref the collector can
    // trace and relocate is categorically safer than a dropped root. `into_inner`
    // always yields the protected map, so the scan/remap sees every table
    // unconditionally.
    //
    // Fresh exact HashMap<Integer, ?> fast stores. Both the canonical key
    // object and value are authoritative roots until lazy materialization.
    // PERF (hm-int-fast-shard): the overlay is sharded, so this walks EVERY
    // shard. Iterating all `HM_INT_FAST_SHARDS` is exactly equivalent to
    // iterating the former single map — every entry lives in exactly one
    // shard, chosen deterministically from its key — and skipping even one
    // would drop a root: during a moving GC the shard's `ObjectRef`s would be
    // neither traced (`for_rooting=true`) nor remapped (`false`), i.e. a
    // use-after-free. Each shard's guard is poison-recovered individually for
    // the same reason.
    for shard in hm_int_fast_shards().iter() {
        let mut maps = shard.lock().unwrap_or_else(|e| e.into_inner());
        if maps.is_empty() {
            continue;
        }
        for (&overlay_key, state) in maps.gc_iter_mut() {
            if skip(overlay_key) {
                continue;
            }
            for (key, value) in state.entries.values_mut() {
                f("hm-int-fast/key", overlay_key, key);
                if let Value::Object(Some(object)) = value {
                    f("hm-int-fast/value", overlay_key, object);
                }
            }
        }
    }
    // Inner name -> Value overlays (LinkedList head/tail/size, LinkedHashMap
    // table/head/tail/…).
    {
        let mut ll = ll_overlay().lock().unwrap_or_else(|e| e.into_inner());
        for (&key, inner) in ll.gc_iter_mut() {
            if skip(key) {
                continue;
            }
            for v in inner.values_mut() {
                if let Value::Object(Some(r)) = v {
                    f("ll-overlay", key, r);
                }
            }
        }
    }
    {
        let mut lhm = lhm_overlay().lock().unwrap_or_else(|e| e.into_inner());
        if lhm.is_empty() {
            // Nothing to root/remap — skip without taking `lhm_heap_backed`.
        } else {
            // Hold the heap-backed set across the loop (lock order: heap_backed is
            // never taken while lhm_overlay is held elsewhere — `lhm_set` locks them
            // sequentially, not nested — so this order can't deadlock). Recover the
            // guard on poison too, for the same reason as the outer tables.
            // A/B LEVER (`CRATONVM_LHM_ROOT_ALL=1`): root EVERY LinkedHashMap
            // overlay ref, i.e. ignore `lhm_heap_backed`.
            //
            // `lhm-overlay` is the only table the post-GC audit reports
            // dangling refs from (600 `ZEROED(reclaimed)` reports over 68
            // distinct addresses in one `DefaultCatalogAndSchemaTest` run), and
            // it is also the only table whose refs the ROOT scan conditionally
            // SKIPS. The skip's premise is that a heap-backed LHM's
            // `head`/`tail`/`table` are mirrored into real heap fields and so
            // stay alive by ordinary field tracing. This flag tests that
            // premise directly instead of arguing about it: with the skip off,
            // the audit count must go to zero. It is a measurement lever, not a
            // fix — removing the skip outright reintroduces the unbounded
            // pinning of dead LHMs/LHSs it was added to stop.
            let hb = if for_rooting && !lhm_root_all() {
                Some(lhm_heap_backed().lock().unwrap_or_else(|e| e.into_inner()))
            } else {
                None
            };
            for (key, inner) in lhm.gc_iter_mut() {
                if skip(*key) {
                    continue;
                }
                if let Some(hb) = &hb {
                    if hb.contains(key) {
                        continue;
                    }
                }
                for (name, v) in inner.iter_mut() {
                    if let Value::Object(Some(r)) = v {
                        // Name the slot, not just the table: which of
                        // `head`/`tail`/`table` dangles says whether the mirror
                        // is missing or merely stale.
                        f(
                            match name.as_str() {
                                "head" => "lhm-overlay/head",
                                "tail" => "lhm-overlay/tail",
                                "table" => "lhm-overlay/table",
                                _ => "lhm-overlay/other",
                            },
                            *key,
                            r,
                        );
                    }
                }
            }
        } // end else (non-empty lhm)
    }
    // TreeMap array mode: backing `data` array + comparator.
    {
        let mut tm = tm_array_table().lock().unwrap_or_else(|e| e.into_inner());
        for (&key, st) in tm.gc_iter_mut() {
            if skip(key) {
                continue;
            }
            if let Some(r) = &mut st.data {
                f("tm-array/data", key, r);
            }
            if let Value::Object(Some(r)) = &mut st.comparator {
                f("tm-array/comparator", key, r);
            }
        }
    }
    // TreeMap fast mode: the BTreeMap *is* the authoritative store (the array
    // slot is left empty), so its `Value::Object` values are reachable only
    // through this side-table.
    {
        let mut tmf = tm_fast_table().lock().unwrap_or_else(|e| e.into_inner());
        for (&key, bt) in tmf.gc_iter_mut() {
            if skip(key) {
                continue;
            }
            for v in bt.values_mut() {
                if let Value::Object(Some(r)) = v {
                    f("tm-fast/value", key, r);
                }
            }
        }
    }
    // TreeSet array mode: backing `data` array + comparator.
    {
        let mut ts = ts_array_table().lock().unwrap_or_else(|e| e.into_inner());
        for (&key, st) in ts.gc_iter_mut() {
            if skip(key) {
                continue;
            }
            if let Some(r) = &mut st.data {
                f("ts-array/data", key, r);
            }
            if let Value::Object(Some(r)) = &mut st.comparator {
                f("ts-array/comparator", key, r);
            }
        }
    }
    // ConcurrentSkipListMap custom comparators (fix item 5). The comparator is
    // held only in this side-table (the synthetic CSLM has no object slot for
    // it), so it is reachable only here — root + remap it like the TreeMap/
    // TreeSet comparators above so a moving GC keeps it live and repointed.
    {
        let mut cmps = cslm_comparator_table()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (&key, r) in cmps.gc_iter_mut() {
            if skip(key) {
                continue;
            }
            f("cslm/comparator", key, r);
        }
    }
    // The `--jdk-only` snapshot iterator's backing collection. A real
    // `Arrays$ArrayItr` has only `cursor` and `a`, so the collection its
    // `remove()` writes through to is reachable ONLY from this table — root it
    // and repoint it, exactly like the CSLM comparator above. Walked on both
    // `for_rooting` settings: a live iterator whose backing was reclaimed would
    // remove into freed memory.
    {
        let mut backings = snapshot_itr_backing_table()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (&key, st) in backings.gc_iter_mut() {
            if skip(key) {
                continue;
            }
            f("snapshot-itr/backing", key, &mut st.backing);
        }
    }
    // Navigable-view specs (W7-1 family 1). Three refs each and all three are
    // reachable only here: the backing map (a view keeps its source alive for
    // exactly as long as the view is itself reachable) and the two bound keys,
    // which are compared against on every rebuild and so must survive and be
    // repointed like any other side-table key.
    {
        let mut views = tm_view_table().lock().unwrap_or_else(|e| e.into_inner());
        for (&key, spec) in views.gc_iter_mut() {
            if skip(key) {
                continue;
            }
            f("tm-view/source", key, &mut spec.source);
            if let Value::Object(Some(r)) = &mut spec.lo {
                f("tm-view/lo", key, r);
            }
            if let Value::Object(Some(r)) = &mut spec.hi {
                f("tm-view/hi", key, r);
            }
        }
    }
}

/// Push every top-level ObjectRef held by the overlay-backed collections onto
/// `roots` so a moving GC keeps the backing storage live and relocates it.
pub fn gc_scan_collection_overlay_roots(roots: &mut Vec<ObjectRef>) {
    for_each_overlay_ref(true, no_skip, |_table, _key, r| roots.push(*r));
}

/// [`gc_scan_collection_overlay_roots`] for the collecting VM `vm` (gc-common
/// w12-c): every row except those [`foreign_overlay_keys`] proves belong to
/// another VM. The collector is no longer handed another heap's objects as
/// roots.
pub fn gc_scan_collection_overlay_roots_for_vm(vm: usize, roots: &mut Vec<ObjectRef>) {
    let foreign = foreign_overlay_keys(vm);
    if foreign.is_empty() {
        return gc_scan_collection_overlay_roots(roots);
    }
    let skip = |key: usize| foreign.contains(&key);
    for_each_overlay_ref(true, skip, |_table, _key, r| roots.push(*r));
}

/// Post-GC audit: hand every overlay-held `ObjectRef` to `classify` and report
/// the ones it calls dangling.
///
/// The heap-side verifiers (`verify_heap_object_fields`, `verify_no_stale_refs`)
/// walk Java objects and thread frames, so an overlay that lost its backing is
/// invisible to both — the side table is neither on the heap nor in a frame.
/// That blind spot is where the `LinkedHashMap$Node` reclaimed-receiver family
/// lives, and inferring it from downstream symptoms has repeatedly gone wrong.
///
/// `classify` returns `Some(reason)` for a dangling target (off-heap, zeroed
/// header, un-forwarded) and `None` for a healthy one; only the collector can
/// answer that, so it supplies the predicate. Call with POST-remap addresses,
/// so a report means the ref is genuinely bad rather than merely not-yet-fixed.
/// Diagnostic-only: opt-in and capped at the call site.
///
/// Walks with `for_rooting = false`, so it audits the `lhm_heap_backed` entries
/// the ROOT scan deliberately skips as well. Those are the interesting ones:
/// their survival rests on the mirrored `head`/`tail`/`table` heap fields still
/// being current, and nothing else checks that.
pub fn gc_audit_overlay_refs(
    classify: &dyn Fn(usize) -> Option<&'static str>,
    report: &mut dyn FnMut(&'static str, &'static str, usize),
) {
    for_each_overlay_ref(false, no_skip, |table, _key, r| {
        let addr = r.as_ptr() as usize;
        if addr != 0 {
            if let Some(reason) = classify(addr) {
                report(reason, table, addr);
            }
        }
    });
}

/// Publish the collection-overlay root contract to the collector abstraction.
///
/// Registration is deliberately performed from native initialization rather
/// than from the GC crate: this keeps the collector independent of collection
/// implementations and guarantees the provider exists before an overlay can be
/// created through a registered native.
pub(crate) fn register_gc_root_provider() {
    cratonvm_gc::external_roots::register_external_root_provider(
        cratonvm_gc::external_roots::ExternalRootProvider {
            name: "native-collection-overlays",
            scan: gc_scan_collection_overlay_roots,
            owner_addrs: gc_overlay_owner_addrs,
            roots_for_owner: gc_overlay_roots_for_collection,
            roots_for_matching_owners: gc_overlay_roots_for_matching_owners,
            remap: gc_update_collection_overlay_refs,
            prune: gc_prune_dead_collection_overlays,
            gate_stats: Some(|| {
                let (h, m) = overlay_class_gate_stats();
                (h, m, overlay_class_gate_disabled())
            }),
            owner_rows: Some(cratonvm_gc::external_roots::OwnerRowCallbacks {
                all_rows: gc_overlay_all_rows,
                rows_for_keys: gc_overlay_rows_for_keys,
                take_dirty_keys: gc_overlay_take_dirty_keys,
                enable_dirty_tracking: gc_overlay_enable_dirty_tracking,
            }),
        },
    );
    // gc-common w12-c: the VM-scoped halves. A VM's own root scan, remap and
    // post-collection prune reach these (`cratonvm_gc::external_roots::*_for_vm`),
    // so one VM's collection neither roots, remaps nor condemns another VM's
    // rows; teardown drops the VM's rows.
    cratonvm_gc::external_roots::register_vm_scoped_root_callbacks(
        "native-collection-overlays",
        cratonvm_gc::external_roots::VmScopedRootCallbacks {
            scan: gc_scan_collection_overlay_roots_for_vm,
            remap: gc_update_collection_overlay_refs_for_vm,
            prune: gc_prune_dead_collection_overlays_for_vm,
            forget_vm: forget_vm_collection_overlays,
        },
    );
}

/// Owner addresses that have at least one overlay key registered.
///
/// `gc_overlay_roots_for_collection` locks the owner index on every call and
/// returns an empty `Vec` for an owner that has no overlay -- which is every
/// object in a run that never touches an overlay-backed collection. A
/// stop-the-world marker can take this set once and consult it lock-free per
/// object, which is what makes a multi-threaded young marker viable (the
/// per-object `Mutex` would otherwise serialise it). `None` means the index
/// is empty, so the marker can skip the check entirely.
pub fn gc_overlay_owner_addrs() -> Option<std::collections::HashSet<usize>> {
    let index = overlay_owner_keys()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // `Some` of an EMPTY set, not `None`, when nothing is registered.
    //
    // The two say opposite things to a marker. `None` means "I cannot
    // enumerate my owners, so do not exclude anything on my behalf"
    // (`ExternalRootProvider::owner_addrs`), which forces the per-object
    // overlay lookup -- the provider lock and this mutex -- for every marked
    // object in the heap. `Some(empty)` is the fact: this provider owns
    // nothing, so no address needs asking about. A program that touches no
    // native-backed collection is exactly the case that was paying most for
    // the ambiguity.
    Some(index.keys().copied().collect())
}

/// Return the Rust-side references owned by one already-marked collection.
///
/// On the Generational non-moving collector this is the conditional replacement
/// for treating every overlay value as a process-global root: the marker calls
/// it only after the collection object itself has been found reachable through
/// ordinary Java roots/fields. The returned copy deliberately releases every
/// side-table lock before the collector recursively marks the values.
/// `owner_class_id` is the class id of the object CURRENTLY at `owner_addr`.
/// The owner index is keyed by address, and an address is recycled the moment
/// its previous tenant is reclaimed — so without this check the marker is
/// handed the DEAD collection's references on behalf of whatever unrelated
/// object now sits there. `widened_obj_key` already applies exactly this
/// discriminator on the mutator side (its case 2b re-keys and clears a
/// different-class recycle), but that only runs when the recycled address is
/// used as a COLLECTION again; an ordinary object landing there is never
/// observed, so the stale entry survives indefinitely. `None` skips the check.
pub fn gc_overlay_roots_for_collection(
    owner_addr: usize,
    owner_class_id: Option<u32>,
) -> Vec<ObjectRef> {
    // THE CLASS GATE, before the mutex -- see `OVERLAY_OWNER_CLASS_BITS`. This
    // runs once per MARKED OBJECT, and on any ordinary class the answer is a
    // definitive no from two relaxed loads. `None` cannot be gated (there is no
    // class to test) and falls through, which is correct: that caller is the
    // per-OWNER predicate seed, not the per-object path.
    if let Some(cid) = owner_class_id {
        if !overlay_owner_class_possible(cid) {
            OVERLAY_CLASS_GATE_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Vec::new();
        }
        OVERLAY_CLASS_GATE_MISSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let keys = {
        let index = overlay_owner_keys()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        index.get(&owner_addr).cloned().unwrap_or_default()
    };
    gc_overlay_roots_for_collection_with_keys(owner_addr, owner_class_id, keys)
}

/// [`gc_overlay_roots_for_collection`] for a caller that has ALREADY read this
/// owner's key list out of `overlay_owner_keys`.
///
/// Exists so a whole-index sweep can take that mutex once instead of once per
/// owner. `gc_overlay_roots_for_matching_owners` locked it to list the owners
/// and then re-locked it in here for every owner it had just listed — `1 + N`
/// acquisitions and `N` key-set clones per minor GC, with `N` in the thousands
/// for a Spring context. Visible as `overlay_forward` under
/// `CRATONVM_DBG=gcpause`: 36–66 ms per young collection, and it does NOT
/// shrink when the young generation does, so it grows as a share of the pause
/// once the copying phases come down.
pub fn gc_overlay_roots_for_collection_with_keys(
    owner_addr: usize,
    owner_class_id: Option<u32>,
    keys: Vec<usize>,
) -> Vec<ObjectRef> {
    if keys.is_empty() {
        return Vec::new();
    }
    // Drop any key whose registry slot records a DIFFERENT class than the
    // object now at this address. A class id is GC-invariant (it travels with
    // the header), so a mismatch means the registering collection is gone and
    // this entry belongs to a previous tenant.
    let keys: Vec<usize> = match owner_class_id {
        Some(actual) if owner_class_filter_enabled() => keys
            .into_iter()
            .filter(|key| owner_key_class_matches(*key, actual, owner_addr))
            .collect(),
        _ => keys,
    };
    if keys.is_empty() {
        return Vec::new();
    }

    let mut roots = Vec::new();
    push_overlay_roots_for_keys(&keys, &mut roots);
    roots
}

/// Append every overlay reference registered under `keys` to `roots`.
///
/// PERF (overlay-root-batch). Each overlay table is locked ONCE for the whole
/// key list rather than once per key. The per-key form cost `7 * K` mutex
/// acquisitions, and `gc_overlay_roots_for_matching_owners` drives `K` to the
/// key count of the entire index on every minor GC — thousands, for a Spring
/// context. Batched, the acquisition count is bounded by the number of TABLES
/// (six global, plus at most `HM_INT_FAST_SHARDS` shards) and no longer grows
/// with `K`. This is the same batching shape `gc_prune_dead_collection_overlays`
/// already uses for its removals.
///
/// DEADLOCK. Only one table's guard is ever held at a time, exactly as in the
/// per-key form — the hoist widens each guard's lifetime but never nests two,
/// so it introduces no lock ORDER to violate.
///
/// ORDER. Roots now arrive grouped by table rather than interleaved per key.
/// That is safe: the collector forwards each root independently, forwarding is
/// idempotent, and duplicates were already possible whenever two owners shared
/// a key.
pub(crate) fn push_overlay_roots_for_keys(keys: &[usize], roots: &mut Vec<ObjectRef>) {
    visit_overlay_refs_for_keys(keys, &mut |_, r| roots.push(r));
}

/// The owner address a packed object key's slot currently records, or `None`
/// once the slot is gone (pruned, or torn down with its VM).
pub(crate) fn owner_of_obj_key(key: usize) -> Option<usize> {
    let hash = (key >> 32) as u32;
    let generation = key as u32;
    let shard = obj_key_shard_for(hash)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    shard
        .get(&hash)?
        .iter()
        .find(|s| s.generation == generation)
        .map(|s| s.last_ptr)
        .filter(|&p| p != 0)
}

/// Group `keys`' references by key into [`OwnerRow`]s, given each key's owner.
pub(crate) fn owner_rows_of(keyed: Vec<(usize, usize)>) -> Vec<cratonvm_gc::external_roots::OwnerRow> {
    if keyed.is_empty() {
        return Vec::new();
    }
    let keys: Vec<usize> = keyed.iter().map(|(k, _)| *k).collect();
    // Keyed by the packed object key ROTATED so its identity-hash half is the
    // low half: Fx multiplies, and on the raw key (generation in the low half,
    // nearly always 0 or 1) every entry collides (see `OverlayRows::dirty`).
    let mut refs: FxHashMap<usize, Vec<ObjectRef>> =
        FxHashMap::with_capacity_and_hasher(keys.len(), Default::default());
    visit_overlay_refs_for_keys(&keys, &mut |key, r| {
        refs.entry(key.rotate_left(32)).or_default().push(r)
    });
    keyed
        .into_iter()
        .map(|(key, owner)| cratonvm_gc::external_roots::OwnerRow {
            owner,
            key,
            refs: refs.remove(&key.rotate_left(32)).unwrap_or_default(),
        })
        .collect()
}

/// [`cratonvm_gc::external_roots::OwnerRowCallbacks::all_rows`]: every key of
/// every owner the predicate accepts, from the owner index.
pub fn gc_overlay_all_rows(
    owner_matches: &dyn Fn(usize) -> bool,
) -> Vec<cratonvm_gc::external_roots::OwnerRow> {
    // Key (rotated, see `owner_rows_of`) -> (key, owner, refs), from the owner
    // index; then ONE walk of every table (`for_each_overlay_ref`, the same
    // funnel the root scan and the remap use) fills the references. Reading
    // the whole index this way is a table iteration, not a hashed lookup per
    // key per table.
    let mut rows: FxHashMap<usize, cratonvm_gc::external_roots::OwnerRow> = {
        let index = overlay_owner_keys()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        index
            .iter()
            .filter(|(owner, _)| owner_matches(**owner))
            .flat_map(|(owner, keys)| {
                keys.iter().map(move |k| {
                    (
                        k.rotate_left(32),
                        cratonvm_gc::external_roots::OwnerRow {
                            owner: *owner,
                            key: *k,
                            refs: Vec::new(),
                        },
                    )
                })
            })
            .collect()
    };
    if rows.is_empty() {
        return Vec::new();
    }
    for_each_overlay_ref(false, no_skip, |_table, key, r| {
        if let Some(row) = rows.get_mut(&key.rotate_left(32)) {
            row.refs.push(*r);
        }
    });
    rows.into_values().collect()
}

/// [`cratonvm_gc::external_roots::OwnerRowCallbacks::rows_for_keys`].
pub fn gc_overlay_rows_for_keys(keys: &[usize]) -> Vec<cratonvm_gc::external_roots::OwnerRow> {
    let keyed: Vec<(usize, usize)> = keys
        .iter()
        .filter_map(|&k| owner_of_obj_key(k).map(|owner| (k, owner)))
        .collect();
    // A few keys: look each up (`owner_rows_of`). Many (a young pause right
    // after a burst of collections): one walk of the tables is cheaper than
    // a hashed lookup per key per table.
    if keyed.len() <= ROWS_BY_LOOKUP_MAX {
        return owner_rows_of(keyed);
    }
    let mut rows: FxHashMap<usize, cratonvm_gc::external_roots::OwnerRow> = keyed
        .into_iter()
        .map(|(key, owner)| {
            (
                key.rotate_left(32),
                cratonvm_gc::external_roots::OwnerRow {
                    owner,
                    key,
                    refs: Vec::new(),
                },
            )
        })
        .collect();
    for_each_overlay_ref(false, no_skip, |_table, key, r| {
        if let Some(row) = rows.get_mut(&key.rotate_left(32)) {
            row.refs.push(*r);
        }
    });
    rows.into_values().collect()
}

/// Above this many keys [`gc_overlay_rows_for_keys`] walks the tables once
/// instead of looking each key up in each of them.
pub(crate) const ROWS_BY_LOOKUP_MAX: usize = 512;

/// [`cratonvm_gc::external_roots::OwnerRowCallbacks::enable_dirty_tracking`]:
/// start recording which rows mutators write (`overlay_rows.rs`).
pub fn gc_overlay_enable_dirty_tracking() {
    overlay_rows::enable_dirty_tracking();
}

/// The written-key record of one overlay table, type-erased so the nine tables
/// can be walked by one loop.
pub(crate) trait RecordAccess {
    fn take(&mut self) -> std::collections::HashSet<usize>;
    fn restore(&mut self, keys: &[usize]);
}
impl<V, S: std::hash::BuildHasher> RecordAccess for OverlayRows<V, S> {
    fn take(&mut self) -> std::collections::HashSet<usize> {
        self.take_dirty()
    }
    fn restore(&mut self, keys: &[usize]) {
        self.restore_dirty(keys.iter().copied());
    }
}

/// Apply `f` to each overlay table's record under that table's lock, one
/// table at a time — the same nine tables [`for_each_overlay_ref`] walks, and
/// a new table must be listed in both.
pub(crate) fn with_each_overlay_record(f: &mut dyn FnMut(&mut dyn RecordAccess)) {
    for shard in hm_int_fast_shards().iter() {
        f(&mut *shard.lock().unwrap_or_else(|e| e.into_inner()));
    }
    f(&mut *ll_overlay().lock().unwrap_or_else(|e| e.into_inner()));
    f(&mut *lhm_overlay().lock().unwrap_or_else(|e| e.into_inner()));
    f(&mut *tm_array_table().lock().unwrap_or_else(|e| e.into_inner()));
    f(&mut *tm_fast_table().lock().unwrap_or_else(|e| e.into_inner()));
    f(&mut *ts_array_table().lock().unwrap_or_else(|e| e.into_inner()));
    f(&mut *cslm_comparator_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner()));
    f(&mut *snapshot_itr_backing_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner()));
    f(&mut *tm_view_table().lock().unwrap_or_else(|e| e.into_inner()));
}

/// [`cratonvm_gc::external_roots::OwnerRowCallbacks::take_dirty_keys`]: the
/// keys written since the last take whose owner the predicate accepts. The
/// rest are put back for the collector of the VM that owns them; a key whose
/// slot is gone is dropped (its rows went with it).
pub fn gc_overlay_take_dirty_keys(owner_matches: &dyn Fn(usize) -> bool) -> Vec<usize> {
    // One taker at a time across the process: between this take and the
    // restore below, another VM's keys live only in this call's locals, and a
    // second VM's collector taking concurrently would find its own rows
    // unrecorded and miss a young reference stored into one of them.
    static TAKE: Mutex<()> = Mutex::new(());
    let _one_taker = TAKE.lock().unwrap_or_else(|e| e.into_inner());
    let mut taken: std::collections::HashSet<usize> = std::collections::HashSet::new();
    with_each_overlay_record(&mut |table| taken.extend(table.take()));
    if taken.is_empty() {
        return Vec::new();
    }
    let mut mine = Vec::new();
    let mut theirs = Vec::new();
    for key in taken {
        match owner_of_obj_key(key) {
            Some(owner) if owner_matches(owner) => mine.push(key),
            Some(_) => theirs.push(key),
            None => {}
        }
    }
    if !theirs.is_empty() {
        with_each_overlay_record(&mut |table| table.restore(&theirs));
    }
    mine
}

pub(crate) fn visit_overlay_refs_for_keys(keys: &[usize], visit: &mut dyn FnMut(usize, ObjectRef)) {
    if keys.is_empty() {
        return;
    }

    fn push_hm_int_fast_roots(
        key: usize,
        state: &HmIntFastState,
        visit: &mut dyn FnMut(usize, ObjectRef),
    ) {
        for (entry_key, value) in state.entries.values() {
            visit(key, entry_key);
            if let Value::Object(Some(object)) = value {
                visit(key, object);
            }
        }
    }

    // `hm_int_fast` is sharded BY KEY, so it cannot collapse to one
    // acquisition — group the keys by shard instead and lock each shard that
    // has any (a shard with no keys is never locked), the same grouping
    // `gc_prune_dead_collection_overlays` does for its dead keys. Short key
    // lists skip the grouping: `gc_overlay_roots_for_collection` runs per owner
    // inside the major marker's BFS, typically with a single key, where
    // building 64 buckets would cost more than the acquisitions it saves.
    if keys.len() <= 4 {
        for key in keys {
            if let Some(state) = hm_int_fast_shard_for(*key)
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(key)
            {
                push_hm_int_fast_roots(*key, state, visit);
            }
        }
    } else {
        let mut by_shard: [Vec<usize>; HM_INT_FAST_SHARDS] = std::array::from_fn(|_| Vec::new());
        for key in keys {
            by_shard[hm_int_fast_shard_index(*key)].push(*key);
        }
        for (shard, shard_keys) in hm_int_fast_shards().iter().zip(by_shard.iter()) {
            if shard_keys.is_empty() {
                continue;
            }
            let table = shard.lock().unwrap_or_else(|e| e.into_inner());
            for key in shard_keys {
                if let Some(state) = table.get(key) {
                    push_hm_int_fast_roots(*key, state, visit);
                }
            }
        }
    }

    {
        let table = ll_overlay().lock().unwrap_or_else(|e| e.into_inner());
        for key in keys {
            if let Some(inner) = table.get(key) {
                for value in inner.values() {
                    if let Value::Object(Some(object)) = value {
                        visit(*key, *object);
                    }
                }
            }
        }
    }
    {
        let table = lhm_overlay().lock().unwrap_or_else(|e| e.into_inner());
        for key in keys {
            if let Some(inner) = table.get(key) {
                for value in inner.values() {
                    if let Value::Object(Some(object)) = value {
                        visit(*key, *object);
                    }
                }
            }
        }
    }
    {
        let table = tm_array_table().lock().unwrap_or_else(|e| e.into_inner());
        for key in keys {
            if let Some(state) = table.get(key) {
                if let Some(data) = state.data {
                    visit(*key, data);
                }
                if let Value::Object(Some(comparator)) = state.comparator {
                    visit(*key, comparator);
                }
            }
        }
    }
    {
        let table = tm_fast_table().lock().unwrap_or_else(|e| e.into_inner());
        for key in keys {
            if let Some(entries) = table.get(key) {
                for value in entries.values() {
                    if let Value::Object(Some(object)) = value {
                        visit(*key, *object);
                    }
                }
            }
        }
    }
    {
        let table = ts_array_table().lock().unwrap_or_else(|e| e.into_inner());
        for key in keys {
            if let Some(state) = table.get(key) {
                if let Some(data) = state.data {
                    visit(*key, data);
                }
                if let Value::Object(Some(comparator)) = state.comparator {
                    visit(*key, comparator);
                }
            }
        }
    }
    {
        let table = cslm_comparator_table()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for key in keys {
            if let Some(comparator) = table.get(key) {
                visit(*key, *comparator);
            }
        }
    }
    {
        // The backing collection of a `--jdk-only` snapshot iterator. The owner
        // here is the ITERATOR, so this edge is retained exactly while the
        // iterator is itself reachable — which is the correct lifetime: a dead
        // iterator can no longer call `remove()`.
        //
        // Hoisted like its neighbours: dev took each overlay lock once per minor
        // GC rather than once per key, and this table is the highest-turnover of
        // the set (one entry per strict-mode `HashSet.iterator()`), so per-key
        // locking here would have been the worst offender.
        let table = snapshot_itr_backing_table()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for key in keys {
            if let Some(state) = table.get(key) {
                visit(*key, state.backing);
            }
        }
    }
    {
        // A navigable view's backing map and range bounds. The owner is the
        // VIEW, so `headMap`'s result keeps the map it is a view OF alive for
        // precisely as long as the view can still be read — which is the JDK's
        // reachability too, since there the view holds a hard reference to `m`.
        let table = tm_view_table().lock().unwrap_or_else(|e| e.into_inner());
        for key in keys {
            if let Some(spec) = table.get(key) {
                visit(*key, spec.source);
                if let Value::Object(Some(r)) = spec.lo {
                    visit(*key, r);
                }
                if let Value::Object(Some(r)) = spec.hi {
                    visit(*key, r);
                }
            }
        }
    }
}

/// Return overlay references whose owners satisfy `owner_matches`.
///
/// A young-only collection cannot tell whether an old-generation object is
/// otherwise reachable: old objects are not swept in that cycle. Its overlay
/// edges therefore need the same treatment as ordinary old→young card edges —
/// retain them for the minor collection, then let the major marker apply the
/// precise per-owner rule before old-space compaction.
pub fn gc_overlay_roots_for_matching_owners(
    owner_matches: &dyn Fn(usize) -> bool,
) -> Vec<ObjectRef> {
    // ONE acquisition of `overlay_owner_keys`, collecting every matching
    // owner's keys in the same pass that selects them. This previously locked
    // here to list the owners and then re-locked once per owner inside
    // `gc_overlay_roots_for_collection` — `1 + N` acquisitions of one global
    // mutex, plus `N` clones of the same key lists, on every minor GC.
    //
    // The keys are FLATTENED rather than kept per owner because no per-owner
    // work survives the collection: this path passes no class id (see below),
    // so nothing downstream distinguishes one owner's keys from another's. One
    // flat list is what lets `push_overlay_roots_for_keys` sweep each overlay
    // table once for the whole GC instead of once per owner — with per-owner
    // batching alone the table lock count would still be `6 * N`.
    //
    // No class id: this seed is address-based by construction — the predicate
    // selects owners by generation/range, and no class id is available for
    // them. It is already a deliberate over-approximation ("retain the edges of
    // every current owner"), so skipping the recycled-owner check here only
    // over-retains, which is this path's existing contract. The precise
    // per-owner rule runs in the BFS, which does pass a class id.
    let keys: Vec<usize> = {
        let index = overlay_owner_keys()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        index
            .iter()
            .filter(|(owner, _)| owner_matches(**owner))
            .flat_map(|(_, keys)| keys.iter().copied())
            .collect()
    };
    let mut roots = Vec::new();
    push_overlay_roots_for_keys(&keys, &mut roots);
    roots
}

/// Repoint every top-level ObjectRef held by the overlay-backed collections to
/// its relocated address after a moving GC. Mirror of
/// `gc_scan_collection_overlay_roots`.
pub fn gc_update_collection_overlay_refs(pointer_map: &cratonvm_types::PointerMap) {
    update_collection_overlay_refs(None, pointer_map);
}

/// [`gc_update_collection_overlay_refs`] for the collecting VM `vm` (gc-common
/// w12-c): `vm`'s pointer map is applied to `vm`'s rows, its slots'
/// `last_ptr`s and its owner-index entries only. A row
/// [`foreign_overlay_keys`] cannot prove foreign is treated as `vm`'s, which
/// is harmless: a foreign reference is never a key of this VM's pointer map.
pub fn gc_update_collection_overlay_refs_for_vm(
    vm: usize,
    pointer_map: &cratonvm_types::PointerMap,
) {
    update_collection_overlay_refs(Some(vm), pointer_map);
}

pub(crate) fn update_collection_overlay_refs(vm: Option<usize>, pointer_map: &cratonvm_types::PointerMap) {
    if pointer_map.is_empty() {
        return;
    }
    let foreign = vm.map(foreign_overlay_keys).unwrap_or_default();
    let skip = |key: usize| !foreign.is_empty() && foreign.contains(&key);
    for_each_overlay_ref(false, &skip, |_table, _key, r| {
        if let Some(&new_addr) = pointer_map.get(&(r.as_ptr() as usize)) {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            *r = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
    });

    // Fix item 1/2: keep the `obj_key_registry` `last_ptr` markers in step with
    // the move so (a) the next `widened_obj_key` resolves a relocated object by
    // exact-pointer match -- its ONLY match since gc-common w26-a removed the
    // "lone occupant" fallback, so a slot this pass misses loses its
    // collection's contents at the next lookup -- and
    // (b) the liveness prune below tests each slot against its *current*
    // address. A relocated collection object's identity-hash key (the overlay
    // bucket key) is invariant across the move, so only the stored pointer
    // changes — the overlay/cache entries stay correctly keyed.
    // PERF (registry-shard): iterate every shard — equivalent to walking the
    // former single map. Runs during GC (single-threaded), so per-shard locking
    // adds no contention.
    //
    // Poison-recovered (gc-common w12-c): this used `if let Ok(..)`, which
    // SKIPPED a poisoned shard for the rest of the process. A skipped shard
    // keeps its slots' PRE-move `last_ptr`s, and the prune that follows asks
    // liveness about those stale addresses -- a relocated live collection
    // reads as dead and its overlay (its contents) is deleted. The prune's own
    // `if let Ok` only skips pruning (a leak), the safe direction; this one
    // was not. Every overlay table lock in this file recovers the same way.
    for shard in obj_key_shards().iter() {
        let mut reg = shard.lock().unwrap_or_else(|e| e.into_inner());
        for slots in reg.values_mut() {
            for slot in slots.iter_mut() {
                // Another VM's slot is not this pointer map's to move.
                if vm.is_some_and(|v| slot.vm != v) {
                    continue;
                }
                if let Some(&new_addr) = pointer_map.get(&slot.last_ptr) {
                    slot.last_ptr = new_addr;
                }
            }
        }
    }

    // Keep the reverse owner index in the same post-move address space as the
    // object-key registry. The marker uses this index on the next non-moving
    // cycle, so leaving even one pre-copy address here would silently drop a
    // live collection's overlay edge.
    //
    // Rebuild into a FRESH map rather than relocating entries in place. Every
    // other remap in this file is a single-step lookup — each ref is rewritten
    // exactly once, which is what `pointer_map` means (the collector has
    // already composed young->promoted->compacted chains into one hop before
    // handing it over). Relocating in place is not single-step: a `pointer_map`
    // may legitimately contain both `A -> B` and `B -> C`, because old-gen
    // sliding compaction hands one live object the address another live object
    // just vacated. Applying `A -> B` first parks A's keys at B, and the later
    // `B -> C` then sweeps them on to C along with B's own — so A's collection,
    // which really is at B, ends up with NO entry at its own address.
    //
    // Nothing catches that afterwards. The side tables are keyed by the
    // relocation-invariant identity hash, so the collection keeps reading its
    // own state correctly; only the GC's "which refs does this collection own?"
    // question breaks, and only on the paths that ask it per owner — the
    // non-moving young marker and `old_gen_gc`'s mark BFS. Those then free a
    // live backing array, which resurfaces as `checkcast: not an object
    // reference` in whatever reads the collection next. The moving young path
    // seeds from `external_roots_for_matching_owners(&|_| true)`, a union over
    // every indexed address, so it survives a misplaced entry and cannot expose
    // the bug — which is why this stayed invisible while the moving-young work
    // was the thing under test.
    //
    // Iteration order of `moves` decided whether it fired, so it was silent,
    // intermittent, and needed two overlay-backed collections in old gen plus a
    // compacting cycle that slid one onto the other's address.
    let mut owners = overlay_owner_keys()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut rebuilt: StdHashMap<usize, Vec<usize>> = StdHashMap::with_capacity(owners.len());
    for (addr, keys) in owners.drain() {
        // Absent from the map means "did not move" — keep the address as is.
        let target_addr = pointer_map.get(&addr).copied().unwrap_or(addr);
        for key in keys {
            // A key proven to be another VM's stays where it is: this VM's
            // pointer map says nothing about that VM's addresses.
            let at = if target_addr != addr && skip(key) {
                addr
            } else {
                target_addr
            };
            let target = rebuilt.entry(at).or_default();
            if !target.contains(&key) {
                target.push(key);
            }
        }
    }
    *owners = rebuilt;
}

/// Prune overlay/side-table entries whose backing collection object is no
/// longer live, bounding the otherwise-unbounded growth of `obj_key_registry`,
/// `lhm_overlay`, `lhm_heap_backed`, and `ll_overlay`
/// (fix item 2). Before this, every `LinkedHashMap`/`LinkedHashSet`/
/// `LinkedList` ever touched left a permanent side-table entry — a steady leak,
/// and an O(n)-per-GC walk in `for_each_overlay_ref` over dead entries.
///
/// `is_live(addr)` must report whether the heap object at `addr` survived the
/// just-completed collection. It is supplied by the GC, which alone has the
/// authoritative post-trace liveness information (the per-generation
/// `pointer_map` handed to `gc_update_collection_overlay_refs` only covers the
/// *collected* generation's survivors, so it cannot by itself classify an
/// old-gen overlay object as dead — hence the explicit predicate).
///
/// MUST be called AFTER `gc_update_collection_overlay_refs` so the registry's
/// `last_ptr` markers already point at post-GC addresses.
///
/// NOTE (cross-file wiring): the GC collection epilogue in
/// `vm/src/memory/gc.rs` (the site that already calls
/// `gc_update_collection_overlay_refs`) must call this with its `is_live`
/// closure for the prune to take effect. That one-line wiring lives outside
/// this crate's scope and is flagged in the change report rather than edited
/// here.
pub fn gc_prune_dead_collection_overlays(is_live: &dyn Fn(usize) -> bool) {
    prune_dead_collection_overlays(None, is_live);
}

/// [`gc_prune_dead_collection_overlays`] for the collecting VM `vm` (gc-common
/// w12-c): only `vm`'s object-key slots are handed to `is_live`; every other
/// VM's slot is kept whatever the predicate would say about its address.
///
/// This is the per-VM screen the prune lacked on Generational
/// (`common-w10g-generational-overlay-prune-has-no-ownership-screen`): a
/// collecting VM's liveness answers "dead" for any address outside its heap,
/// so the VM-less prune deleted every other VM's live overlay-backed
/// collections on each of its collections. A slot records its VM when it is
/// created (`widened_obj_key`, `ObjKeyEntry::vm`), so this screen needs no
/// address span and no collector-side ownership test.
pub fn gc_prune_dead_collection_overlays_for_vm(vm: usize, is_live: &dyn Fn(usize) -> bool) {
    prune_dead_collection_overlays(Some(vm), is_live);
}

/// Drop every overlay row VM `vm` owns: its object-key slots and every table
/// entry keyed by them. Called once, from the VM's teardown
/// (`cratonvm_gc::external_roots::forget_vm_external_roots`, reached from
/// `vm_init.rs::release_vm_native_state`).
///
/// Without it a torn-down VM's rows are never pruned, because no collection
/// of that VM runs again. Before w12-c every other VM's scan also rooted
/// them. Their owner-index rows stay keyed by addresses in a heap that no
/// longer exists. Once a new heap is mapped over the old reservation,
/// [`gc_overlay_roots_for_collection`] hands those dead references to any
/// marker that finds a new object at one of those addresses.
pub fn forget_vm_collection_overlays(vm: usize) {
    let mut dead_keys: Vec<(usize, usize)> = Vec::new();
    for shard in obj_key_shards().iter() {
        let mut reg = shard.lock().unwrap_or_else(|e| e.into_inner());
        reg.retain(|&hash, slots| {
            slots.retain(|slot| {
                let mine = slot.vm == vm;
                if mine {
                    dead_keys.push((pack_obj_key(hash, slot.generation), slot.last_ptr));
                }
                !mine
            });
            !slots.is_empty()
        });
    }
    if dead_keys.is_empty() {
        return;
    }
    let dead: std::collections::HashSet<usize> = dead_keys.iter().map(|(k, _)| *k).collect();
    drop_overlay_entries_for_dead_keys(dead_keys);
    // The owner index is keyed by address, and an entry can sit at an address
    // other than the slot's last `last_ptr` (a missed re-key). Teardown runs
    // once per VM, so sweep the whole index rather than trust the addresses.
    overlay_owner_keys()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|_, keys| {
            keys.retain(|k| !dead.contains(k));
            !keys.is_empty()
        });
}

/// Did any object-key slot ever get created by a VM other than the first one?
///
/// A perf latch for [`foreign_overlay_keys`], nothing more: in a one-VM
/// process (every production run) it stays `false`, and the VM-scoped scan and
/// remap skip the registry walk that would only prove there is nothing
/// foreign. It is sticky and process-wide by design -- it can only make the
/// walk happen, never change what the walk finds, so a stale `true` costs one
/// walk per collection and nothing else.
pub(crate) static OBJ_KEY_SLOTS_FROM_SEVERAL_VMS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The VM that created the first object-key slot, plus one (`0` = none yet).
pub(crate) static OBJ_KEY_FIRST_VM_PLUS_ONE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Record that VM `vm` is creating an object-key slot. See
/// [`OBJ_KEY_SLOTS_FROM_SEVERAL_VMS`].
#[inline]
pub(crate) fn note_obj_key_slot_vm(vm: usize) {
    use std::sync::atomic::Ordering;
    let tag = vm.wrapping_add(1);
    let first = match OBJ_KEY_FIRST_VM_PLUS_ONE.compare_exchange(
        0,
        tag,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => return,
        Err(first) => first,
    };
    if first != tag && !OBJ_KEY_SLOTS_FROM_SEVERAL_VMS.load(Ordering::Relaxed) {
        OBJ_KEY_SLOTS_FROM_SEVERAL_VMS.store(true, Ordering::Release);
    }
}

/// Packed overlay keys whose object-key slot belongs to a VM other than `vm`.
///
/// The VM-scoped scan and remap SKIP exactly these, and treat every other
/// key as `vm`'s -- including a key with no slot at all. That is the safe
/// direction on both counts: a skipped key is a root `vm`'s collector is not
/// given, so only a key PROVEN foreign may be skipped; and a foreign slot
/// created after this walk (another VM's mutators are not stopped by this
/// VM's pause) is merely scanned and remapped for one cycle, as every foreign
/// row was before the screen existed. The remap of such a row is a no-op: its
/// references are in the other VM's heap, never keys of this VM's pointer map.
///
/// Empty without a walk while only one VM has ever created a slot and it is
/// `vm` (see [`OBJ_KEY_SLOTS_FROM_SEVERAL_VMS`]).
pub(crate) fn foreign_overlay_keys(vm: usize) -> std::collections::HashSet<usize> {
    use std::sync::atomic::Ordering;
    let mut foreign = std::collections::HashSet::new();
    let first = OBJ_KEY_FIRST_VM_PLUS_ONE.load(Ordering::Acquire);
    if !OBJ_KEY_SLOTS_FROM_SEVERAL_VMS.load(Ordering::Acquire)
        && (first == 0 || first == vm.wrapping_add(1))
    {
        return foreign;
    }
    for shard in obj_key_shards().iter() {
        let reg = shard.lock().unwrap_or_else(|e| e.into_inner());
        for (&hash, slots) in reg.iter() {
            for slot in slots {
                if slot.vm != vm {
                    foreign.insert(pack_obj_key(hash, slot.generation));
                }
            }
        }
    }
    foreign
}

/// The prune both entry points share. `vm == None` hands every slot of the
/// process to `is_live` (the VM-less form); `Some(vm)` only that VM's.
pub(crate) fn prune_dead_collection_overlays(vm: Option<usize>, is_live: &dyn Fn(usize) -> bool) {
    let dbg = cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_MIRRORPIN");
    if dbg {
        let total_slots: usize = obj_key_shards()
            .iter()
            .map(|s| {
                s.lock()
                    .map(|reg| reg.values().map(|v| v.len()).sum::<usize>())
                    .unwrap_or(0)
            })
            .sum();
        eprintln!(
            "[DBG_MIRRORPIN] gc_prune_dead_collection_overlays CALLED, total_slots={}",
            total_slots
        );
    }
    // 1. Collect the packed keys of dead collection objects from the registry,
    //    and rebuild the registry without their slots. Done as an explicit
    //    two-pass walk (collect dead keys, then drop slots) to keep the
    //    `dead_keys` mutation cleanly outside the `retain` predicate's borrow.
    // PERF (registry-shard): walk every shard, collecting dead keys and dropping
    // their slots per-shard. Equivalent to the former single-map walk (the union
    // of all shards is the whole registry); runs under GC so per-shard locking is
    // uncontended. Preserves the original early-return: if NO shard yielded a
    // dead key, skip the overlay-cleanup pass entirely.
    //
    // PERF (gc-common w10-g): ONE predicate call per slot. The walk used to ask
    // `is_live` twice for every slot of the process -- once to collect the dead
    // keys, again inside `retain` -- and the collecting VM's predicate is a
    // pointer-map probe plus a heap liveness probe, per slot, per collection of
    // ANY VM. The predicate is a pure function of the pause it runs in, so the
    // single `retain` that records what it drops keeps and drops exactly the
    // same slots.
    let mut dead_keys: Vec<(usize, usize)> = Vec::new();
    for shard in obj_key_shards().iter() {
        if let Ok(mut reg) = shard.lock() {
            // Drop the dead slots, recording each; remove buckets that become
            // empty.
            reg.retain(|&hash, slots| {
                slots.retain(|slot| {
                    // Another VM's slot is never this collection's to condemn
                    // (gc-common w12-c): its address is outside the collecting
                    // heap, where `is_live` answers "dead".
                    if vm.is_some_and(|v| slot.vm != v) {
                        return true;
                    }
                    // A zero `last_ptr` is the unset/never-resolved sentinel;
                    // treat it as live (defensive — never prune what we can't
                    // classify).
                    let live = slot.last_ptr == 0 || is_live(slot.last_ptr);
                    if !live {
                        dead_keys.push((pack_obj_key(hash, slot.generation), slot.last_ptr));
                    }
                    live
                });
                !slots.is_empty()
            });
        }
    }
    if dbg {
        eprintln!(
            "[DBG_MIRRORPIN] gc_prune_dead_collection_overlays dead_keys={}",
            dead_keys.len()
        );
    }
    if dead_keys.is_empty() {
        return;
    }
    drop_overlay_entries_for_dead_keys(dead_keys);
}

/// Remove every table entry keyed by one of `dead_keys` (`(packed key, owner
/// address)`), and the keys' owner-index rows. The slots themselves are
/// already gone from the object-key registry.
pub(crate) fn drop_overlay_entries_for_dead_keys(dead_keys: Vec<(usize, usize)>) {
    // PERF (hm-int-fast-shard): the overlay is sharded, so a dead key must be
    // removed from ITS shard. Grouping the keys by shard first keeps the
    // batching property the single-map version had — lock each shard at most
    // once per GC, so the lock count stays bounded by the shard count rather
    // than growing with the number of dead keys. A shard with no dead keys is
    // never locked at all.
    {
        let mut by_shard: [Vec<usize>; HM_INT_FAST_SHARDS] = std::array::from_fn(|_| Vec::new());
        for (k, _) in &dead_keys {
            by_shard[hm_int_fast_shard_index(*k)].push(*k);
        }
        for (shard, keys) in hm_int_fast_shards().iter().zip(by_shard.iter()) {
            if keys.is_empty() {
                continue;
            }
            let mut hm = shard.lock().unwrap_or_else(|e| e.into_inner());
            for k in keys {
                hm.remove(k);
            }
        }
    }

    // 2. Drop the dead objects' overlay/cache entries everywhere they are keyed
    //    by the packed `widened_obj_key`. This must cover EVERY table keyed by
    //    `widened_obj_key`: a leftover entry under a reclaimed object's key is
    //    both a leak and — should a later slot ever be issued the dead one's
    //    generation (`next_generation` reissues a pruned maximum) — stale state
    //    a newcomer would alias onto. The TreeMap (array +
    //    fast) and TreeSet tables were previously omitted here and are now
    //    included. Locks are poison-recovered (`into_inner`) so a panic elsewhere
    //    can't leave a table un-pruned and its stale state re-aliasable. Batched
    //    (lock each table once, loop the keys inside) to keep the per-GC lock
    //    count independent of the number of dead keys.
    {
        let mut lhm = lhm_overlay().lock().unwrap_or_else(|e| e.into_inner());
        for (k, _) in &dead_keys {
            lhm.remove(k);
        }
    }
    {
        let mut hb = lhm_heap_backed().lock().unwrap_or_else(|e| e.into_inner());
        for (k, _) in &dead_keys {
            hb.remove(k);
        }
    }
    {
        let mut ll = ll_overlay().lock().unwrap_or_else(|e| e.into_inner());
        for (k, _) in &dead_keys {
            ll.remove(k);
        }
    }
    {
        let mut tm = tm_array_table().lock().unwrap_or_else(|e| e.into_inner());
        for (k, _) in &dead_keys {
            tm.remove(k);
        }
    }
    {
        let mut tmf = tm_fast_table().lock().unwrap_or_else(|e| e.into_inner());
        for (k, _) in &dead_keys {
            tmf.remove(k);
        }
    }
    // The sticky "this TreeMap is pinned to array mode" flag is keyed by the
    // same packed `widened_obj_key` as the two tables above and was the one
    // such table this sweep missed: it grew for every TreeMap that ever hit a
    // non-extractable key and, once a dead map's 32-bit identity hash was
    // recycled, silently forced the NEW map into array mode from its first put.
    {
        let mut forced = tm_force_array_set()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (k, _) in &dead_keys {
            forced.remove(k);
        }
    }
    {
        let mut ts = ts_array_table().lock().unwrap_or_else(|e| e.into_inner());
        for (k, _) in &dead_keys {
            ts.remove(k);
        }
    }
    // CSLM custom-comparator side-table (fix item 5) is keyed by the same
    // packed `widened_obj_key`; drop the dead maps' comparators too.
    {
        let mut cmps = cslm_comparator_table()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (k, _) in &dead_keys {
            cmps.remove(k);
        }
    }
    // The `--jdk-only` snapshot iterator's backing collection, keyed by the same
    // packed `widened_obj_key` — of the ITERATOR, not of a collection. That
    // makes this the highest-turnover table of the set (one entry per
    // `HashSet.iterator()` in strict mode) and the one that would leak fastest
    // if it were skipped: each entry also ROOTS its backing set, so a missed
    // sweep pins dead collections, not merely a few words.
    {
        let mut backings = snapshot_itr_backing_table()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (k, _) in &dead_keys {
            backings.remove(k);
        }
    }
    // Navigable-view specs, keyed by the VIEW's packed `widened_obj_key`. Each
    // entry roots a whole backing map, so a missed sweep pins collections rather
    // than a few words — the same reason the iterator table above cannot be
    // skipped.
    {
        let mut views = tm_view_table().lock().unwrap_or_else(|e| e.into_inner());
        for (k, _) in &dead_keys {
            views.remove(k);
        }
    }
    for (key, owner_addr) in dead_keys {
        remove_overlay_owner_key(owner_addr, key);
    }
}

/// Allocate a fresh backing array for `owner` and publish it into the overlay
/// `DATA` slot with BOTH the owner and the new array pinned across every step.
/// Returns the (possibly relocated) owner and array — callers MUST use the
/// returned values, not their pre-call copies.
///
/// `alloc_ref_array` is a Java-heap allocation and can trigger a moving young
/// collection that relocates `owner`. Storing through a pre-allocation `owner`
/// registers the overlay under a STALE owner address in `overlay_owner_keys` —
/// the reverse index the collector consults to answer "which side-table refs
/// does this collection own?". The collection still reads its own state fine
/// (the side-table key is the relocation-invariant identity hash), but the GC
/// never associates the live object with its overlay, so the backing array is
/// never rooted and a later collection reclaims it. What comes back is a zeroed
/// header — `ClassId(0)`, `array_length == 0` — which surfaces far away as an
/// `Int(0)` where an element should be (`TreeSet.contains` → `ts_binary_search`
/// → `Comparator.compare` → `checkcast: not an object reference`).
///
/// `native_tm_put` already had this fix ("gcstress face-1"); every other
/// TreeMap/TreeSet allocation site did not. Route them all through here.
pub(crate) fn ts_install_backing_array(
    ctx: &mut dyn NativeContext,
    owner: ObjectRef,
    cap: usize,
) -> (ObjectRef, ObjectRef) {
    let owner_pin = ctx.pin_native_root(owner);
    let buf = alloc_ref_array(ctx, cap);
    let buf_pin = ctx.pin_native_root(buf);
    let owner = ctx.read_native_pin(owner_pin, owner);
    let buf = ctx.read_native_pin(buf_pin, buf);
    ts_set_slot(ctx, owner, TS_FIELD_DATA, Value::Object(Some(buf)));
    let owner = ctx.read_native_pin(owner_pin, owner);
    let buf = ctx.read_native_pin(buf_pin, buf);
    ctx.unpin_native_roots(owner_pin);
    (owner, buf)
}

/// Owners already reported by [`report_short_overlay_backing`], so one
/// corrupted collection logs once instead of once per read.
pub(crate) fn reported_short_backings() -> &'static Mutex<std::collections::HashSet<(usize, usize)>> {
    static SEEN: std::sync::OnceLock<Mutex<std::collections::HashSet<(usize, usize)>>> =
        std::sync::OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Name a TreeMap/TreeSet whose overlay `size` no longer fits its backing
/// array, at the read that first observes it.
///
/// This is the invariant every indexed read of the array-mode backing store
/// depends on and none of them check: `size` says N entries are live, so the
/// array must be at least `N * slots_per_entry` long. When the collector
/// reclaims the array out from under a still-live collection, what comes back
/// is a zeroed header — `ClassId(0)`, `array_length == 0` — and the reads keep
/// going, returning `Int(0)` where an element belongs. That `Int(0)` then
/// travels: `TreeSet.contains` -> `ts_binary_search` -> `tree_compare` ->
/// `comparator_compare` -> the real `String$CaseInsensitiveComparator.compare`,
/// which dies on `checkcast: not an object reference` in a stack frame that
/// names neither the collection nor the GC. Three separate investigations
/// (2026-07-28, 07-31, 07-31 again) each had to re-derive that chain from the
/// checkcast backwards, twice via a hand-added throwaway probe.
///
/// So this stays ON, unconditionally: it cannot fire unless the VM is already
/// broken, and one line here replaces that whole derivation. It reports and
/// returns — the caller's behaviour is unchanged, and the downstream failure
/// still happens; the point is that the log now says what actually went wrong.
/// De-duplicated per (owner address, side-table key) so a corrupted collection
/// that is read in a loop does not drown the log.
///
/// Raw numbers only, never a `{:?}` of a decoded value: the whole premise is
/// that these bytes are garbage, and formatting a `Value`/enum read out of a
/// freed header is how a diagnostic turns a recoverable report into a SIGSEGV.
pub(crate) fn report_short_overlay_backing(
    ctx: &dyn NativeContext,
    kind: &'static str,
    owner: ObjectRef,
    data: Option<ObjectRef>,
    size: i32,
    slots_per_entry: usize,
) {
    // Only an installed-but-too-short array is unambiguous. `data == None`
    // with `size > 0` is a LEGITIMATE state: a fast-mode TreeMap keeps its
    // entries in `tm_fast_table` and leaves the array slot empty, but still
    // records the count in the array-state `size` slot so `native_tm_size` can
    // read it (see `tm_materialize_deser_if_needed`). Reporting that shape
    // would fire on every deserialized natural-order TreeMap in the process.
    let data = match data {
        Some(d) if size > 0 => d,
        _ => return,
    };
    let needed = (size as usize).saturating_mul(slots_per_entry);
    let arr_len = ctx.array_length(data);
    if arr_len >= needed {
        return;
    }
    let data_addr = data.as_ptr() as usize;
    let owner_addr = owner.as_ptr() as usize;
    let key = tm_obj_key(ctx, owner);
    // A map that has BOTH an array and a fast-mode table is mid-conversion,
    // not corrupt: the authoritative store is the table and the array slot is
    // whatever the array path last left there.
    if tm_fast_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&key)
    {
        return;
    }
    {
        let mut seen = reported_short_backings()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !seen.insert((owner_addr, key)) {
            return;
        }
    }
    // `class_id_of_object` on a zeroed header reports 0, which is also the real
    // id of `java/lang/Object` — print the number and let the reader decide,
    // rather than resolving it to a name that would read as a plausible class.
    let data_cid = ctx.class_id_of_object(data).as_u32();
    eprintln!(
        "[overlay-backing] {kind} backing array is shorter than its size — the \
         collector reclaimed or failed to remap it while the collection was \
         still live. owner=0x{owner_addr:x} key=0x{key:x} size={size} \
         needed={needed} array_length={arr_len} data=0x{data_addr:x} \
         data_class_id={data_cid} (array_length=0 together with \
         data_class_id=0 is a freed, zeroed object). Reads past the end \
         return Int(0), which surfaces far away as \
         'checkcast: not an object reference'."
    );
}

/// TreeMap twin of [`ts_install_backing_array`] — same hazard, same contract.
pub(crate) fn tm_install_backing_array(
    ctx: &mut dyn NativeContext,
    owner: ObjectRef,
    cap: usize,
) -> (ObjectRef, ObjectRef) {
    let owner_pin = ctx.pin_native_root(owner);
    let buf = alloc_ref_array(ctx, cap);
    let buf_pin = ctx.pin_native_root(buf);
    let owner = ctx.read_native_pin(owner_pin, owner);
    let buf = ctx.read_native_pin(buf_pin, buf);
    tm_set_slot(ctx, owner, TM_FIELD_DATA, Value::Object(Some(buf)));
    let owner = ctx.read_native_pin(owner_pin, owner);
    let buf = ctx.read_native_pin(buf_pin, buf);
    ctx.unpin_native_roots(owner_pin);
    (owner, buf)
}

/// Read a TreeSet "slot" (`TS_FIELD_DATA`/`SIZE`/`COMPARATOR`) from the
/// address-keyed side-table. Returns layout-independent defaults when no
/// entry exists yet.
pub(crate) fn ts_get_slot(ctx: &dyn NativeContext, this: ObjectRef, slot: usize) -> Value {
    let key = tm_obj_key(ctx, this);
    let tbl = ts_array_table().lock().unwrap();
    if let Some(st) = tbl.get(&key) {
        return match slot {
            TS_FIELD_DATA => Value::Object(st.data),
            TS_FIELD_SIZE => Value::Int(st.size),
            TS_FIELD_COMPARATOR => st.comparator,
            _ => Value::Object(None),
        };
    }
    match slot {
        TS_FIELD_SIZE => Value::Int(0),
        _ => Value::Object(None),
    }
}

/// Write a TreeSet "slot" into the address-keyed side-table. The object's
/// own fields are never touched (see `tm_set_slot`).
pub(crate) fn ts_set_slot(ctx: &mut dyn NativeContext, this: ObjectRef, slot: usize, v: Value) {
    let key = tm_obj_key(ctx, this);
    let mut tbl = ts_array_table().lock().unwrap();
    let st = tbl.entry(key).or_default();
    match slot {
        TS_FIELD_DATA => {
            st.data = match v {
                Value::Object(o) => o,
                _ => None,
            }
        }
        TS_FIELD_SIZE => {
            st.size = match v {
                Value::Int(n) => n,
                _ => 0,
            }
        }
        TS_FIELD_COMPARATOR => st.comparator = v,
        _ => {}
    }
}

// ----- comparison helper -----

pub(crate) fn tree_compare(
    ctx: &mut dyn NativeContext,
    comparator: &Value,
    a: Value,
    b: Value,
) -> Result<i32, cratonvm_types::error::MethodCallFailed> {
    let result = match comparator {
        Value::Object(Some(cmp)) => comparator_compare(ctx, *cmp, a, b)?,
        _ => natural_compare(ctx, &a, &b)?,
    };
    match result {
        Some(Value::Int(v)) => Ok(v),
        _ => Ok(0),
    }
}

// ----- TreeMap binary search -----
// Returns Ok(index) if key found at that entry index, Err(insert_pos) if not found.
/// Family-1 stale-ObjectRef fix (2026-07-13): `tree_compare` below can
/// dispatch to an arbitrary user-supplied `Comparator` (`thenComparing`
/// chains, key-extractor lambdas, a real-bytecode `compare()` override) —
/// a full interpreted call that can allocate and trigger a moving GC. Both
/// `owner` (the TreeMap/TreeSet object, needed by callers for side-table
/// lookups keyed off its address) and `data` (the backing `Object[]` slots
/// array, read again on every loop iteration) are raw `ObjectRef`s that
/// must be re-read after every comparator invocation, not just once at
/// entry — hence pinning both here and returning their current values
/// alongside the search result, so callers never reuse a pre-search copy.
/// See wildfly-parallel-boot-stale-objectref-residual.md.
pub(crate) fn tm_binary_search(
    ctx: &mut dyn NativeContext,
    owner: ObjectRef,
    data: ObjectRef,
    size: i32,
    comparator: &Value,
    key: &Value,
) -> Result<
    (Result<usize, usize>, ObjectRef, ObjectRef, Value),
    cratonvm_types::error::MethodCallFailed,
> {
    let owner_pin = ctx.pin_native_root(owner);
    let data_pin = ctx.pin_native_root(data);
    let mut owner = owner;
    let mut data = data;
    // Family-1 stale-ObjectRef fix (2026-07-13 follow-up): `comparator`
    // itself — not just `owner`/`data` — is dereferenced by every
    // `tree_compare` call below (`comparator_compare` reads its fields
    // immediately). Across 2+ loop iterations, an earlier comparison's
    // GC-triggering dispatch can relocate the comparator object, so it
    // must be pinned and re-read exactly like `owner`/`data`.
    let mut comparator = *comparator;
    let comparator_pin = match comparator {
        Value::Object(Some(r)) => Some(ctx.pin_native_root(r)),
        _ => None,
    };
    // Family-1 stale-ObjectRef fix (second follow-up): `key` — the searched
    // key, compared on EVERY loop iteration — is exactly as much at risk as
    // `comparator`/`owner`/`data`: a comparator dispatch on one iteration
    // can relocate it before the next iteration's `tree_compare` call
    // dereferences it again. Refreshed and returned to the caller too,
    // since some callers (`native_tm_put`/`native_tm_put_if_absent`) reuse
    // the searched key after this function returns (to store it into the
    // array on a miss). This — plus the sibling `a`/`b` reuse inside
    // `comparator_compare`'s own dispatch arms — was the actual root cause
    // of the residual panics that survived the owner/data/comparator-only
    // fix. See wildfly-parallel-boot-stale-objectref-residual.md.
    let mut key = *key;
    let key_pin = pin_value(ctx, key);
    let mut low: usize = 0;
    let mut high = size as usize;
    while low < high {
        let mid = low + (high - low) / 2;
        let mid_key = ctx.get_array_element(data, mid * 2);
        // Match TreeMap.put/get semantics: compare the searched key with the
        // current node key. Non-antisymmetric comparators rely on this
        // orientation, including Liquibase's pipeline-order tie comparator.
        let cmp = match tree_compare(ctx, &comparator, key, mid_key) {
            Ok(c) => c,
            Err(e) => {
                ctx.unpin_native_roots(owner_pin);
                return Err(e);
            }
        };
        owner = ctx.read_native_pin(owner_pin, owner);
        data = ctx.read_native_pin(data_pin, data);
        if let (Value::Object(Some(r)), Some(pin)) = (comparator, comparator_pin) {
            comparator = Value::Object(Some(ctx.read_native_pin(pin, r)));
        }
        key = read_pinned_elem(ctx, key_pin, key);
        if cmp > 0 {
            low = mid + 1;
        } else if cmp < 0 {
            high = mid;
        } else {
            ctx.unpin_native_roots(owner_pin);
            return Ok((Ok(mid), owner, data, key));
        }
    }
    ctx.unpin_native_roots(owner_pin);
    Ok((Err(low), owner, data, key))
}

// ----- TreeSet binary search -----
/// See `tm_binary_search`'s doc comment — identical Family-1 fix, applied to
/// TreeSet's backing array.
pub(crate) fn ts_binary_search(
    ctx: &mut dyn NativeContext,
    owner: ObjectRef,
    data: ObjectRef,
    size: i32,
    comparator: &Value,
    key: &Value,
) -> Result<
    (Result<usize, usize>, ObjectRef, ObjectRef, Value),
    cratonvm_types::error::MethodCallFailed,
> {
    let owner_pin = ctx.pin_native_root(owner);
    let data_pin = ctx.pin_native_root(data);
    let mut owner = owner;
    let mut data = data;
    // Family-1 stale-ObjectRef fix: same as `tm_binary_search` above —
    // `comparator` AND `key` (the searched element) must be pinned/
    // refreshed across loop iterations too, not just `owner`/`data`.
    let mut comparator = *comparator;
    let comparator_pin = match comparator {
        Value::Object(Some(r)) => Some(ctx.pin_native_root(r)),
        _ => None,
    };
    let mut key = *key;
    let key_pin = pin_value(ctx, key);
    let mut low: usize = 0;
    let mut high = size as usize;
    while low < high {
        let mid = low + (high - low) / 2;
        let mid_elem = ctx.get_array_element(data, mid);
        // Keep TreeSet aligned with TreeMap's backing-key behavior:
        // compare(new_element, existing_element), not the reverse.
        let cmp = match tree_compare(ctx, &comparator, key, mid_elem) {
            Ok(c) => c,
            Err(e) => {
                ctx.unpin_native_roots(owner_pin);
                return Err(e);
            }
        };
        owner = ctx.read_native_pin(owner_pin, owner);
        data = ctx.read_native_pin(data_pin, data);
        if let (Value::Object(Some(r)), Some(pin)) = (comparator, comparator_pin) {
            comparator = Value::Object(Some(ctx.read_native_pin(pin, r)));
        }
        key = read_pinned_elem(ctx, key_pin, key);
        if cmp > 0 {
            low = mid + 1;
        } else if cmp < 0 {
            high = mid;
        } else {
            ctx.unpin_native_roots(owner_pin);
            return Ok((Ok(mid), owner, data, key));
        }
    }
    ctx.unpin_native_roots(owner_pin);
    Ok((Err(low), owner, data, key))
}

// Read TreeMap state
pub(crate) fn tm_state(ctx: &dyn NativeContext, this: ObjectRef) -> (Option<ObjectRef>, i32, Value) {
    let data = match tm_get_slot(ctx, this, TM_FIELD_DATA) {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };
    let size = match tm_get_slot(ctx, this, TM_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    // TreeMap's array mode stores key/value pairs, so `size` entries occupy
    // `2 * size` slots.
    report_short_overlay_backing(ctx, "TreeMap", this, data, size, 2);
    let comparator = tm_get_slot(ctx, this, TM_FIELD_COMPARATOR);
    (data, size, comparator)
}

// Ensure TreeMap data array has room for at least one more entry.
//
// gc-common w11-c (`common-w8b-native-factories-still-single-attempt`, the
// `tm_ensure_capacity` residue): the grow runs under `grow`'s policy.
// `GrowAlloc::Reclaim` comes only from the registered doors
// (`native_tm_put_entry`, `native_tm_put_if_absent`), whose every caller is a
// GC point already; the ~25 internal `native_tm_put` callers keep
// `SingleAttempt`. `live` audit: `this` and `data` are pinned below and
// re-read after the allocation; the caller pins its key and value.
pub(crate) fn tm_ensure_capacity(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    size: i32,
    data: ObjectRef,
    grow: GrowAlloc,
) -> ObjectRef {
    let arr_len = ctx.array_length(data);
    let needed = ((size + 1) as usize) * 2;
    if needed <= arr_len {
        return data;
    }
    let new_cap = (arr_len * 2).max(needed);
    // Family-1 stale-ObjectRef fix: `alloc_ref_array` allocates, which can
    // trigger a moving GC that relocates `this`/`data` while they sit in bare
    // Rust locals across the call — pin both and refresh before using them
    // again (copy loop reads `data`; `tm_set_slot` writes through `this`).
    let this_pin = ctx.pin_native_root(this);
    let data_pin = ctx.pin_native_root(data);
    let new_arr = new_ref_array_grow(ctx, ClassId::new(0), new_cap, grow);
    let this = ctx.read_native_pin(this_pin, this);
    let data = ctx.read_native_pin(data_pin, data);
    for i in 0..(size as usize * 2) {
        let v = ctx.get_array_element(data, i);
        ctx.set_array_element(new_arr, i, v);
    }
    tm_set_slot(ctx, this, TM_FIELD_DATA, Value::Object(Some(new_arr)));
    ctx.unpin_native_roots(this_pin);
    new_arr
}

// Insert key/value at position, shifting elements right
pub(crate) fn tm_insert_at(
    ctx: &mut dyn NativeContext,
    data: ObjectRef,
    size: i32,
    pos: usize,
    key: Value,
    value: Value,
) {
    // cce0079 tree-key-tail fix: a reference-typed store can itself
    // cooperate with a GC (canary-proven in the sibling
    // `native_map_put_evict_pinned` node-population sequence: value CURRENT
    // at refresh, STALE one ref-store later). Every ref crossing a store
    // here — `data`, the shifted `k`/`v` snapshots, and the final
    // `key`/`value` — must be refreshed through a pin immediately before
    // its own store.
    let data_pin = ctx.pin_native_root(data);
    let kh = pin_value(ctx, key);
    let vh = pin_value(ctx, value);
    let mut data = data;
    let s = size as usize;
    for i in (pos..s).rev() {
        let k = ctx.get_array_element(data, i * 2);
        let v = ctx.get_array_element(data, i * 2 + 1);
        let ke = pin_value(ctx, k);
        let ve = pin_value(ctx, v);
        data = ctx.read_native_pin(data_pin, data);
        let k = read_pinned_elem(ctx, ke, k);
        ctx.set_array_element(data, (i + 1) * 2, k);
        data = ctx.read_native_pin(data_pin, data);
        let v = read_pinned_elem(ctx, ve, v);
        ctx.set_array_element(data, (i + 1) * 2 + 1, v);
        data = ctx.read_native_pin(data_pin, data);
        // Truncate this iteration's two element pins (LIFO: ke is the
        // lowest handle pushed this iteration).
        if ke != usize::MAX {
            ctx.unpin_native_roots(ke);
        } else if ve != usize::MAX {
            ctx.unpin_native_roots(ve);
        }
    }
    let key = read_pinned_elem(ctx, kh, key);
    ctx.set_array_element(data, pos * 2, key);
    data = ctx.read_native_pin(data_pin, data);
    let value = read_pinned_elem(ctx, vh, value);
    ctx.set_array_element(data, pos * 2 + 1, value);
    ctx.unpin_native_roots(data_pin);
}

// Remove entry at position, shifting elements left
pub(crate) fn tm_remove_at(ctx: &mut dyn NativeContext, data: ObjectRef, size: i32, pos: usize) {
    // cce0079 tree-key-tail fix: same per-store refresh discipline as
    // `tm_insert_at`.
    let data_pin = ctx.pin_native_root(data);
    let mut data = data;
    let s = size as usize;
    for i in pos..(s - 1) {
        let k = ctx.get_array_element(data, (i + 1) * 2);
        let v = ctx.get_array_element(data, (i + 1) * 2 + 1);
        let ke = pin_value(ctx, k);
        let ve = pin_value(ctx, v);
        data = ctx.read_native_pin(data_pin, data);
        let k = read_pinned_elem(ctx, ke, k);
        ctx.set_array_element(data, i * 2, k);
        data = ctx.read_native_pin(data_pin, data);
        let v = read_pinned_elem(ctx, ve, v);
        ctx.set_array_element(data, i * 2 + 1, v);
        data = ctx.read_native_pin(data_pin, data);
        if ke != usize::MAX {
            ctx.unpin_native_roots(ke);
        } else if ve != usize::MAX {
            ctx.unpin_native_roots(ve);
        }
    }
    ctx.set_array_element(data, (s - 1) * 2, Value::Object(None));
    data = ctx.read_native_pin(data_pin, data);
    ctx.set_array_element(data, (s - 1) * 2 + 1, Value::Object(None));
    ctx.unpin_native_roots(data_pin);
}

// ----- TreeSet helpers -----

pub(crate) fn ts_state(ctx: &dyn NativeContext, this: ObjectRef) -> (Option<ObjectRef>, i32, Value) {
    let data = match ts_get_slot(ctx, this, TS_FIELD_DATA) {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };
    let size = match ts_get_slot(ctx, this, TS_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    report_short_overlay_backing(ctx, "TreeSet", this, data, size, 1);
    let comparator = ts_get_slot(ctx, this, TS_FIELD_COMPARATOR);
    (data, size, comparator)
}

pub(crate) fn ts_ensure_capacity(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    size: i32,
    data: ObjectRef,
) -> ObjectRef {
    let arr_len = ctx.array_length(data);
    // A live-view marker lives in the LAST capacity slot (see `ts_view_source`),
    // so a view must keep one slot free beyond its logical size or the next
    // `ts_insert_at` writes over the marker and the view silently stops writing
    // through — the "silently stale view" failure this file's other view code
    // is careful about. Reserve the slot, and carry the marker across a grow.
    //
    // Detected exactly as `ts_view_source` detects it, so the two cannot
    // disagree: `length > size` AND a non-null trailing slot. A plain TreeSet's
    // spare capacity is all null, so this is `false` for every non-view.
    let has_marker = (arr_len as i32) > size
        && matches!(
            ctx.get_array_element(data, arr_len - 1),
            Value::Object(Some(_))
        );
    let needed = (size + 1) as usize + usize::from(has_marker);
    if needed <= arr_len {
        return data;
    }
    let new_cap = (arr_len * 2).max(needed);
    // Family-1 stale-ObjectRef fix: same hazard as `tm_ensure_capacity` —
    // `alloc_ref_array` can trigger a moving GC that relocates `this`/`data`
    // while they sit in bare Rust locals across the call.
    let this_pin = ctx.pin_native_root(this);
    let data_pin = ctx.pin_native_root(data);
    let new_arr = alloc_ref_array(ctx, new_cap);
    // The freshly allocated array is a bare Rust local until it is published,
    // and `ts_set_slot` can relocate it (see `ts_install_backing_array`). Pin
    // it alongside the owner and re-read after the store.
    let new_pin = ctx.pin_native_root(new_arr);
    let this = ctx.read_native_pin(this_pin, this);
    let data = ctx.read_native_pin(data_pin, data);
    let new_arr = ctx.read_native_pin(new_pin, new_arr);
    for i in 0..(size as usize) {
        let v = ctx.get_array_element(data, i);
        ctx.set_array_element(new_arr, i, v);
    }
    if has_marker {
        // Read the marker through the pinned `data` rather than carrying it in a
        // Rust local across `alloc_ref_array` above: a source recorded here as a
        // from-space address is a use-after-free on the first write-through.
        let marker = ctx.get_array_element(data, arr_len - 1);
        ctx.set_array_element(new_arr, new_cap - 1, marker);
    }
    ts_set_slot(ctx, this, TS_FIELD_DATA, Value::Object(Some(new_arr)));
    let new_arr = ctx.read_native_pin(new_pin, new_arr);
    ctx.unpin_native_roots(this_pin);
    new_arr
}

pub(crate) fn ts_insert_at(ctx: &mut dyn NativeContext, data: ObjectRef, size: i32, pos: usize, elem: Value) {
    let s = size as usize;
    for i in (pos..s).rev() {
        let v = ctx.get_array_element(data, i);
        ctx.set_array_element(data, i + 1, v);
    }
    ctx.set_array_element(data, pos, elem);
}

pub(crate) fn ts_remove_at(ctx: &mut dyn NativeContext, data: ObjectRef, size: i32, pos: usize) {
    let s = size as usize;
    for i in pos..(s - 1) {
        let v = ctx.get_array_element(data, i + 1);
        ctx.set_array_element(data, i, v);
    }
    ctx.set_array_element(data, s - 1, Value::Object(None));
}

// ===========================================================================
// TreeMap native methods
// ===========================================================================

/// JVMS §2.3 defaults for the REFERENCE fields a `TreeMap` declares and
/// inherits, which nothing else on the construction path writes.
///
/// `tm_set_slot` keeps this family's state in `tm_array_table`, a Rust
/// side-table keyed on the object, and touches a real field only through its
/// two explicit mirrors (`size` and `comparator`). So the declared reference
/// fields are never assigned by anything: `native_tm_init` is registered for
/// `TreeMap.<init>()V`, so the real constructor -- which would leave them null
/// the way HotSpot does -- does not run either.
///
/// `Value::Object` carries a `NonNull` niche, so the all-zero cell
/// `alloc_zeroed` leaves decodes as `Value::Int(0)` and NOT as
/// `Value::Object(None)`. A read of one of these slots therefore hands
/// `coerce_field_value_for_slot` a primitive in a slot declared `L`, and it
/// DESTROYS the value. Measured on `probes/CollectionSlotFloor`, before this:
///
/// ```text
///   descriptor-coercion census: total=1 primitive-into-reference[read=1]
///     class_id=124 index=0 descriptor=L hits=1
/// ```
///
/// `class_id=124` is `java/util/TreeMap` and index 0 is `keySet` -- INHERITED
/// from `java.util.AbstractMap`, whose `keySet` and `values` precede
/// `TreeMap`'s own seven, which is why the layout reports `fields=9` and why
/// slot 0 is not `comparator`. `cached_tm_view` is the reader: it resolves the
/// view field by name and expects `Object(None)` for "no view cached yet".
///
/// It coerces to null, which is the right answer for a freshly built map --
/// and that is exactly why it has to be written rather than tolerated. The
/// census exists to find the reads where the coerced answer is NOT right, and
/// a benign row sitting at a fixed locator is how a real one stays hidden.
///
/// Resolved against the RECEIVER's own class, so a fabricated stub that
/// declares none of these names is a no-op rather than a write to whatever
/// those slots mean there, and bounds-checked for the same reason. Not a GC
/// point (`set_field` never allocates from the Java heap), so callers may pass
/// a reference they have already re-read through a pin.
pub(crate) fn init_tm_reference_defaults(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let class_id = ctx.class_id_of_object(this);
    let num_fields = ctx.object_num_fields(this);
    // `keySet`/`values` are `AbstractMap`'s; the rest are `TreeMap`'s own.
    // `comparator` is absent deliberately -- `tm_set_slot`'s mirror writes it
    // on every path into this constructor, including the null one.
    for name in [
        "keySet",
        "values",
        "root",
        "entrySet",
        "navigableKeySet",
        "descendingMap",
    ] {
        if let Some(slot) = ctx.resolve_field_index_by_class_id(class_id, name) {
            if slot < num_fields {
                ctx.set_field(this, slot, Value::Object(None));
            }
        }
    }
}

pub(crate) fn native_tm_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // Seed identity-hash so the side-table key survives GC moves.
    // The backing array is NOT allocated here. `native_tm_put`'s `data_opt ==
    // None` arm installs it on the FIRST insert, and `tm_get_slot` documents
    // `Value::Object(None)` as the layout-independent default for a map with no
    // side-table entry -- so the eager allocation this constructor used to do
    // bought nothing but a `TM_DEFAULT_CAPACITY * 2` array on every `new
    // TreeMap()`, retained for as long as the map was.
    //
    // MEASURED (probes/CollectionShapeCause.java, retained heap per empty
    // instance, Temurin 25.0.3+9): `TreeMap` 352 B against HotSpot's 48.1, of
    // which 272 B is exactly this `Object[32]`. HotSpot allocates nothing until
    // the first `put` -- `root` stays null -- and an empty map that is never
    // written is the common shape in configuration and model code.
    ih_seed(ctx, this);
    init_tm_reference_defaults(ctx, this);
    tm_set_slot(ctx, this, TM_FIELD_DATA, Value::Object(None));
    tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(0));
    tm_set_slot(ctx, this, TM_FIELD_COMPARATOR, Value::Object(None));
    Ok(None)
}

pub(crate) fn native_tm_init_comparator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // See `native_tm_init`: the backing array is installed on first insert.
    // No allocation here also means `cmp` needs no pin across one.
    ih_seed(ctx, this);
    let cmp = args.get(1).copied().unwrap_or(Value::Object(None));
    init_tm_reference_defaults(ctx, this);
    tm_set_slot(ctx, this, TM_FIELD_DATA, Value::Object(None));
    tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(0));
    tm_set_slot(ctx, this, TM_FIELD_COMPARATOR, cmp);
    Ok(None)
}

/// `TreeMap.put` for a direct Rust caller (about 25 inside this crate, some of
/// which hold bare references across the put): the backing-array grow gets one
/// no-GC attempt ([`GrowAlloc::SingleAttempt`]). The registered door is
/// [`native_tm_put_entry`].
pub(crate) fn native_tm_put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    tm_put_impl(ctx, args, GrowAlloc::SingleAttempt)
}

/// The registered `TreeMap.put`, and `Map.put`'s TreeMap route from a
/// reclaiming `put` door: reached only from Java dispatch or `invoke_virtual`,
/// so a refused grow may collect and retry ([`GrowAlloc::Reclaim`]; gc-common
/// w11-c).
pub(crate) fn native_tm_put_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    tm_put_impl(ctx, args, GrowAlloc::Reclaim)
}

pub(crate) fn tm_put_impl(ctx: &mut dyn NativeContext, args: &[Value], grow: GrowAlloc) -> MethodCallResult {
    // gcstress residual face-1 fix — `this` is re-read after the two
    // allocating calls in the array-mode path (alloc_ref_array on first
    // insert, tm_ensure_capacity on grow); a moving young GC there would
    // otherwise leave the `tm_set_slot(this, ..)` writes hitting stale
    // storage (the native_map_put corrupt-cell family). Hence `mut`.
    let mut this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key0 = args.get(1).copied().unwrap_or(Value::Object(None));
    let value0 = args.get(2).copied().unwrap_or(Value::Object(None));
    let (this_synced, key, value) = tm_sync_with_args2(ctx, this, key0, value0)?;
    this = this_synced;

    // Before the view branch, not after: a DESCENDING view carries a
    // `Collections.reverseOrder` comparator, so this is a no-op on it and the
    // redirect below lets the backing map raise the refusal itself. An
    // ASCENDING range view carries the source's own (null) comparator, and the
    // JDK refuses there too — `NavigableSubMap.put` calls `inRange(key)` first,
    // which is a `compare(key, bound)` and raises the NPE before the
    // `IllegalArgumentException` this function raises for a real out-of-range
    // key can be reached.
    tree_natural_order_key_check(
        ctx,
        &tm_get_slot(ctx, this, TM_FIELD_COMPARATOR),
        key,
        tm_is_empty_now(ctx, this),
    )?;

    // W7-1 family 1: a write through a navigable view is a write to the BACKING
    // MAP. Redirect it, and refuse an out-of-range key the way
    // `NavigableSubMap.put` does — `IllegalArgumentException`, not a silent
    // insert into a view that could never contain it.
    //
    // GC-SAFETY: `tm_view_in_range` dispatches the key's comparator, so
    // everything held across it moves — including the `spec` copy, whose
    // `source` the collector remaps IN THE TABLE and not in our stack copy.
    // Hence: pin `this`/`key`/`value`, and re-read the spec afterwards rather
    // than reusing the pre-comparison one.
    if tm_view_spec(ctx, this).is_some() {
        let this_pin = ctx.pin_native_root(this);
        let key_pin = pin_value(ctx, key);
        let value_pin = pin_value(ctx, value);
        let verdict = match tm_view_spec(ctx, this) {
            Some(spec) => tm_view_in_range(ctx, &spec, key),
            None => Ok(true),
        };
        let this = ctx.read_native_pin(this_pin, this);
        let key = read_pinned_elem(ctx, key_pin, key);
        let value = read_pinned_elem(ctx, value_pin, value);
        let result = match (verdict, tm_view_spec(ctx, this)) {
            (Err(e), _) => Err(e),
            (Ok(false), _) => Err(
                cratonvm_types::error::RuntimeError::IllegalArgumentException {
                    message: "key out of range".to_string(),
                }
                .into(),
            ),
            // The redirect keeps this call's policy: nothing but the unpin
            // below follows it.
            (Ok(true), Some(spec)) => {
                tm_put_impl(ctx, &[Value::Object(Some(spec.source)), key, value], grow)
            }
            // The view entry vanished under us (only reachable if the GC pruned
            // it, which means the view is dead) — nothing sane to write to.
            (Ok(true), None) => Ok(Some(Value::Object(None))),
        };
        ctx.unpin_native_roots(this_pin);
        return result;
    }

    // Family-1 stale-AT-STORE fix (cce0079 follow-up): `key` and `value`
    // must be pinned BEFORE any GC-capable step in this function — the
    // fast-to-array migration and first-insert array alloc below, and the
    // comparator dispatches inside `tm_binary_search`, can each move either
    // object, and the eventual `tm_insert_at`/replace store would otherwise
    // write the pre-GC address into the backing array. The previous
    // Err-branch pins started too late: they captured locals that could
    // already have gone stale during the search, so the pin faithfully
    // tracked a dead address (a later reader then sees a recycled object —
    // the WildFly `parallel-extension-add` "Object cannot be cast to X"
    // shape). The replace (`Ok`) branch had no `value` protection at all.
    let kh0 = pin_value(ctx, key);
    let vh0 = pin_value(ctx, value);
    let pin_base = if kh0 != usize::MAX { kh0 } else { vh0 };

    // Round-9 HIGH (MED-8 carryover): fast-mode BTreeMap path. Eligible
    // when no custom Comparator was supplied AND the key extracts into
    // a TreeKey (String / Integer / Long). For an empty map this also
    // *enables* fast mode by creating the side-table entry.
    if tm_has_no_comparator(ctx, this) && !tm_force_array_mode(ctx, this) {
        if let Some(tk) = tree_key_from_value(ctx, &key) {
            // If the map already has entries in array mode (e.g. from an
            // earlier non-extractable key), fall through to the array
            // path to avoid splitting state across two stores.
            let already_in_array = matches!(tm_get_slot(ctx, this, TM_FIELD_SIZE), Value::Int(n) if n > 0)
                && !tm_is_fast_mode(ctx, this);
            if !already_in_array {
                let (old, new_size) = tm_fast_with(ctx, this, |bt| {
                    let old = bt.insert(tk, value).unwrap_or(Value::Object(None));
                    (old, bt.len() as i32)
                });
                tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(new_size));
                if pin_base != usize::MAX {
                    ctx.unpin_native_roots(pin_base);
                }
                return Ok(Some(old));
            }
        } else {
            // Non-extractable key → force array mode permanently for this map.
            // If fast-mode had accumulated entries, migrate them first so
            // we don't split state across two stores.
            tm_set_force_array(ctx, this);
            if tm_is_fast_mode(ctx, this) {
                // The migration allocates and can move this map. The pin that
                // used to be written out here is now inside the funnel, which
                // writes the refreshed receiver back through `&mut` -- so the
                // OTHER call site cannot go on being the one that forgot.
                tm_migrate_fast_to_array(ctx, &mut this);
            }
        }
    }
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let data: ObjectRef = match data_opt {
        Some(d) => d,
        None => {
            // gcstress face-1 fix: re-read `this` across the array alloc.
            let this_pin = ctx.pin_native_root(this);
            let buf: ObjectRef = alloc_ref_array(ctx, TM_DEFAULT_CAPACITY * 2);
            this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            tm_set_slot(ctx, this, TM_FIELD_DATA, Value::Object(Some(buf)));
            buf
        }
    };
    // Refresh `key` before the search (the first-insert alloc above may
    // have moved it; `tm_binary_search` pins its own argument, so it must
    // receive the current address).
    let key = read_pinned_elem(ctx, kh0, key);
    // Family-1 stale-ObjectRef fix: `tm_binary_search` invokes the user
    // Comparator, which can trigger a moving GC — use its refreshed
    // `this`/`data`/`key` (shadowed here) instead of the pre-search copies
    // above; `key`'s own staleness (not just `this`/`data`) was the actual
    // root cause of the residual panics that survived the earlier fix.
    let (search, mut this, data, key) =
        match tm_binary_search(ctx, this, data, size, &comparator, &key) {
            Ok(t) => t,
            Err(e) => {
                if pin_base != usize::MAX {
                    ctx.unpin_native_roots(pin_base);
                }
                return Err(e);
            }
        };
    let value = read_pinned_elem(ctx, vh0, value);
    let result = match search {
        Ok(idx) => {
            // Key exists — replace value, return old
            let old = ctx.get_array_element(data, idx * 2 + 1);
            ctx.set_array_element(data, idx * 2 + 1, value);
            Ok(Some(old))
        }
        Err(pos) => {
            // Key not found — insert at pos. gcstress face-1 fix: pin `this`
            // and `key`/`value` across tm_ensure_capacity (which reallocs the
            // data array on grow → can move a moving-GC survivor). `data` is
            // returned fresh; `this` is re-read; key/value refs are re-read so
            // tm_insert_at stores their current addresses.
            let this_pin = ctx.pin_native_root(this);
            let kh = pin_value(ctx, key);
            let vh = pin_value(ctx, value);
            let data = tm_ensure_capacity(ctx, this, size, data, grow);
            this = ctx.read_native_pin(this_pin, this);
            let key = read_pinned_elem(ctx, kh, key);
            let value = read_pinned_elem(ctx, vh, value);
            ctx.unpin_native_roots(this_pin);
            tm_insert_at(ctx, data, size, pos, key, value);
            tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(size + 1));
            Ok(Some(Value::Object(None)))
        }
    };
    if pin_base != usize::MAX {
        ctx.unpin_native_roots(pin_base);
    }
    result
}

pub(crate) fn native_tm_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let (this, key) = tm_sync_with_arg(ctx, this, key)?;
    // `TreeMap.getEntry` refuses a null key on a natural-ordering map BEFORE it
    // looks at `root`, so an EMPTY map throws too — which is the shape the probe
    // measures (`new TreeMap<>().get(null)`). The fast-mode branch below would
    // otherwise answer `null` for it, i.e. "absent", the quiet wrong answer.
    tree_natural_order_key_check(
        ctx,
        &tm_get_slot(ctx, this, TM_FIELD_COMPARATOR),
        key,
        tm_is_empty_now(ctx, this),
    )?;
    if tm_is_fast_mode(ctx, this) {
        if let Some(tk) = tree_key_from_value(ctx, &key) {
            let v = tm_fast_with(ctx, this, |bt| {
                bt.get(&tk).copied().unwrap_or(Value::Object(None))
            });
            return Ok(Some(v));
        }
        // Fast-mode map asked for a non-extractable key → not present.
        return Ok(Some(Value::Object(None)));
    }
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    // Family-1 stale-ObjectRef fix: use tm_binary_search's refreshed `data`
    // (the Comparator invocation inside it can trigger a moving GC).
    let (tm_search_result, _this, data, _key) =
        tm_binary_search(ctx, this, data, size, &comparator, &key)?;
    match tm_search_result {
        Ok(idx) => Ok(Some(ctx.get_array_element(data, idx * 2 + 1))),
        Err(_) => Ok(Some(Value::Object(None))),
    }
}

pub(crate) fn native_tm_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let (this, key) = tm_sync_with_arg(ctx, this, key)?;
    // W7-36 residual, and BEFORE the view branch for the reason `native_tm_put`
    // states: a DESCENDING view carries a `Collections.reverseOrder` comparator,
    // so this is a no-op on it and the redirect lets the backing map raise;
    // an ASCENDING view carries the source's own (null) comparator and
    // `NavigableSubMap.remove`'s `inRange(key)` is a `compare(key, bound)` that
    // raises there too. `TreeMap.remove` itself is `getEntry(key)`, which
    // null-checks and casts to `Comparable` ahead of `root` — an empty map
    // refuses as well.
    tree_natural_order_key_check(
        ctx,
        &tm_get_slot(ctx, this, TM_FIELD_COMPARATOR),
        key,
        tm_is_empty_now(ctx, this),
    )?;
    // W7-1 family 1. This is the row the record measured directly:
    // `tm.headMap("c").remove("a")` used to delete from a detached snapshot and
    // leave `tm` reading `{a=1, b=2, c=3, d=4}`. The JDK's
    // `NavigableSubMap.remove` is `!inRange(key) ? null : m.remove(key)` — an
    // out-of-range key is a plain miss, NOT the `IllegalArgumentException` that
    // `put` raises. Same pin-and-re-read contract as `native_tm_put` above.
    if tm_view_spec(ctx, this).is_some() {
        let this_pin = ctx.pin_native_root(this);
        let key_pin = pin_value(ctx, key);
        let verdict = match tm_view_spec(ctx, this) {
            Some(spec) => tm_view_in_range(ctx, &spec, key),
            None => Ok(true),
        };
        let this = ctx.read_native_pin(this_pin, this);
        let key = read_pinned_elem(ctx, key_pin, key);
        let result = match (verdict, tm_view_spec(ctx, this)) {
            (Err(e), _) => Err(e),
            (Ok(true), Some(spec)) => {
                native_tm_remove(ctx, &[Value::Object(Some(spec.source)), key])
            }
            _ => Ok(Some(Value::Object(None))),
        };
        ctx.unpin_native_roots(this_pin);
        return result;
    }
    if tm_is_fast_mode(ctx, this) {
        if let Some(tk) = tree_key_from_value(ctx, &key) {
            let (old, new_size) = tm_fast_with(ctx, this, |bt| {
                let old = bt.remove(&tk).unwrap_or(Value::Object(None));
                (old, bt.len() as i32)
            });
            tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(new_size));
            return Ok(Some(old));
        }
        return Ok(Some(Value::Object(None)));
    }
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    // Family-1 stale-ObjectRef fix: `this` is used again below
    // (tm_set_slot) after the Comparator-invoking search, so use the
    // refreshed copy tm_binary_search returns (`key` isn't reused here).
    let (search, this, data, _key) = tm_binary_search(ctx, this, data, size, &comparator, &key)?;
    match search {
        Ok(idx) => {
            let old_val = ctx.get_array_element(data, idx * 2 + 1);
            tm_remove_at(ctx, data, size, idx);
            tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(size - 1));
            Ok(Some(old_val))
        }
        Err(_) => Ok(Some(Value::Object(None))),
    }
}

pub(crate) fn native_tm_contains_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let (this, key) = tm_sync_with_arg(ctx, this, key)?;
    // W7-36 residual: `TreeMap.containsKey` is `getEntry(key) != null`, and
    // `getEntry` performs the `if (key == null) throw new NullPointerException();`
    // + `(Comparable) key` checkcast BEFORE it looks at `root` — so an EMPTY map
    // refuses too. Both branches below would otherwise answer "absent", which is
    // the quiet wrong answer `native_tm_get` was given this same call for.
    tree_natural_order_key_check(
        ctx,
        &tm_get_slot(ctx, this, TM_FIELD_COMPARATOR),
        key,
        tm_is_empty_now(ctx, this),
    )?;
    if tm_is_fast_mode(ctx, this) {
        if let Some(tk) = tree_key_from_value(ctx, &key) {
            let found = tm_fast_with(ctx, this, |bt| bt.contains_key(&tk));
            return Ok(Some(Value::Int(i32::from(found))));
        }
        return Ok(Some(Value::Int(0)));
    }
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Int(0))),
    };
    let found = tm_binary_search(ctx, this, data, size, &comparator, &key)?
        .0
        .is_ok();
    Ok(Some(Value::Int(i32::from(found))))
}

pub(crate) fn native_tm_contains_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    let (this, target) = tm_sync_with_arg(ctx, this, target)?;
    if tm_is_fast_mode(ctx, this) {
        let values: Vec<Value> = tm_fast_with(ctx, this, |bt| bt.values().copied().collect());
        // Family-1 fix (cce0079): pin the snapshot — each `equals()`
        // dispatch can move the remaining values and the target.
        let (pin_base, handles) = pin_value_slice(ctx, &values);
        let th = pin_value(ctx, target);
        let mut target = target;
        let mut found = false;
        for (i, v_orig) in values.iter().enumerate() {
            let v = read_pinned_elem(ctx, handles[i], *v_orig);
            let matched = list_element_matches(ctx, &v, &target)?;
            target = read_pinned_elem(ctx, th, target);
            if matched {
                found = true;
                break;
            }
        }
        if pin_base != usize::MAX {
            ctx.unpin_native_roots(pin_base);
        } else if th != usize::MAX {
            ctx.unpin_native_roots(th);
        }
        return Ok(Some(Value::Int(if found { 1 } else { 0 })));
    }
    let (data_opt, size, _) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Int(0))),
    };
    // Family-1 fix (cce0079): pinned value-slot scan of the k/v array.
    let data_pin = ctx.pin_native_root(data);
    let th = pin_value(ctx, target);
    let mut data = data;
    let mut target = target;
    let mut found = false;
    for i in 0..(size as usize) {
        let v = ctx.get_array_element(data, i * 2 + 1);
        let matched = list_element_matches(ctx, &v, &target)?;
        data = ctx.read_native_pin(data_pin, data);
        target = read_pinned_elem(ctx, th, target);
        if matched {
            found = true;
            break;
        }
    }
    ctx.unpin_native_roots(data_pin);
    Ok(Some(Value::Int(if found { 1 } else { 0 })))
}

pub(crate) fn native_tm_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let this = tm_sync_native_state(ctx, this)?;
    let size = match tm_get_slot(ctx, this, TM_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(size)))
}

pub(crate) fn native_tm_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(1))),
    };
    let this = tm_sync_native_state(ctx, this)?;
    let size = match tm_get_slot(ctx, this, TM_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(i32::from(size == 0))))
}

pub(crate) fn native_tm_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // W7-1 family 1: `NavigableSubMap.clear()` is `iterator().remove()` over the
    // range, i.e. it deletes the view's entries FROM THE BACKING MAP and leaves
    // everything outside the range alone. Sync first so the key list is the
    // current one, then delete each key through the ordinary view `remove`
    // (which redirects and re-checks the range for us).
    if tm_view_spec(ctx, this).is_some() {
        let this = tm_sync_native_state(ctx, this)?;
        let pairs = tm_collect_pairs(ctx, this);
        let this_pin = ctx.pin_native_root(this);
        let pinned = PinnedPairs::new(ctx, &pairs);
        let mut this = this;
        for i in 0..pinned.len() {
            let (k, _) = pinned.get(&*ctx, i);
            if let Err(e) = native_tm_remove(ctx, &[Value::Object(Some(this)), k]) {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
            this = ctx.read_native_pin(this_pin, this);
        }
        ctx.unpin_native_roots(this_pin);
        return Ok(None);
    }
    // Fast-mode side-table needs clearing too — without this an iter
    // helper would return stale entries from before the clear. Remove the
    // entry directly instead of going through `tm_fast_with`: comparator-backed
    // TreeMaps live in array mode, and creating an empty fast entry here would
    // make subsequent reads ignore the array store after clear()+put().
    let key = tm_obj_key(ctx, this);
    tm_fast_table().lock().unwrap().remove(&key);
    let (this, _buf) = tm_install_backing_array(ctx, this, TM_DEFAULT_CAPACITY * 2);
    tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(0));
    Ok(None)
}

pub(crate) fn native_tm_first_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "TreeMap is empty".to_string(),
                }
                .into(),
            )
        }
    };
    let this = tm_sync_native_state(ctx, this)?;
    if tm_is_fast_mode(ctx, this) {
        let first = tm_fast_with(ctx, this, |bt| bt.keys().next().cloned());
        match first {
            Some(tk) => return Ok(Some(tree_key_to_value(ctx, &tk))),
            None => {
                return Err(
                    cratonvm_types::error::RuntimeError::NoSuchElementException {
                        message: "TreeMap is empty".to_string(),
                    }
                    .into(),
                )
            }
        }
    }
    let (data_opt, size, _) = tm_state(ctx, this);
    // Treat a None backing array as empty: a divergence between the size
    // counter and the data array must not panic via `unwrap`.
    let data = match data_opt {
        Some(d) if size != 0 => d,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "TreeMap is empty".to_string(),
                }
                .into(),
            );
        }
    };
    Ok(Some(ctx.get_array_element(data, 0)))
}

pub(crate) fn native_tm_last_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "TreeMap is empty".to_string(),
                }
                .into(),
            )
        }
    };
    let this = tm_sync_native_state(ctx, this)?;
    if tm_is_fast_mode(ctx, this) {
        let last = tm_fast_with(ctx, this, |bt| bt.keys().next_back().cloned());
        match last {
            Some(tk) => return Ok(Some(tree_key_to_value(ctx, &tk))),
            None => {
                return Err(
                    cratonvm_types::error::RuntimeError::NoSuchElementException {
                        message: "TreeMap is empty".to_string(),
                    }
                    .into(),
                )
            }
        }
    }
    let (data_opt, size, _) = tm_state(ctx, this);
    // Treat a None backing array as empty rather than panicking on `unwrap`.
    let data = match data_opt {
        Some(d) if size != 0 => d,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "TreeMap is empty".to_string(),
                }
                .into(),
            );
        }
    };
    Ok(Some(ctx.get_array_element(data, (size as usize - 1) * 2)))
}

// ceilingKey: smallest key >= given key
pub(crate) fn native_tm_ceiling_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let (this, key) = tm_sync_with_arg(ctx, this, key)?;
    // See `tm_nav_key_check`: refuses only on a NON-empty map, and only under
    // natural ordering. Ahead of the fast-mode arm, whose
    // `tree_key_from_value` answers `None` for a null key and returns null.
    tm_nav_key_check(&*ctx, this, key)?;
    if tm_is_fast_mode(ctx, this) {
        if let Some(tk) = tree_key_from_value(ctx, &key) {
            let res = tm_fast_with(ctx, this, |bt| {
                bt.range(tk..).next().map(|(k, _)| k.clone())
            });
            return Ok(Some(
                res.map(|k| tree_key_to_value(ctx, &k))
                    .unwrap_or(Value::Object(None)),
            ));
        }
        return Ok(Some(Value::Object(None)));
    }
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    // Family-1 stale-ObjectRef fix: use tm_binary_search's refreshed `data`
    // (the Comparator invocation inside it can trigger a moving GC).
    let (tm_search_result, _this, data, _key) =
        tm_binary_search(ctx, this, data, size, &comparator, &key)?;
    match tm_search_result {
        Ok(idx) => Ok(Some(ctx.get_array_element(data, idx * 2))),
        Err(pos) => {
            if pos < size as usize {
                Ok(Some(ctx.get_array_element(data, pos * 2)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
    }
}

// floorKey: largest key <= given key
pub(crate) fn native_tm_floor_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let (this, key) = tm_sync_with_arg(ctx, this, key)?;
    // See `tm_nav_key_check`: refuses only on a NON-empty map, and only under
    // natural ordering. Ahead of the fast-mode arm, whose
    // `tree_key_from_value` answers `None` for a null key and returns null.
    tm_nav_key_check(&*ctx, this, key)?;
    if tm_is_fast_mode(ctx, this) {
        if let Some(tk) = tree_key_from_value(ctx, &key) {
            let res = tm_fast_with(ctx, this, |bt| {
                bt.range(..=tk).next_back().map(|(k, _)| k.clone())
            });
            return Ok(Some(
                res.map(|k| tree_key_to_value(ctx, &k))
                    .unwrap_or(Value::Object(None)),
            ));
        }
        return Ok(Some(Value::Object(None)));
    }
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    // Family-1 stale-ObjectRef fix: use tm_binary_search's refreshed `data`
    // (the Comparator invocation inside it can trigger a moving GC).
    let (tm_search_result, _this, data, _key) =
        tm_binary_search(ctx, this, data, size, &comparator, &key)?;
    match tm_search_result {
        Ok(idx) => Ok(Some(ctx.get_array_element(data, idx * 2))),
        Err(pos) => {
            if pos > 0 {
                Ok(Some(ctx.get_array_element(data, (pos - 1) * 2)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
    }
}

// higherKey: smallest key strictly > given key
pub(crate) fn native_tm_higher_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let (this, key) = tm_sync_with_arg(ctx, this, key)?;
    // See `tm_nav_key_check`: refuses only on a NON-empty map, and only under
    // natural ordering. Ahead of the fast-mode arm, whose
    // `tree_key_from_value` answers `None` for a null key and returns null.
    tm_nav_key_check(&*ctx, this, key)?;
    if tm_is_fast_mode(ctx, this) {
        if let Some(tk) = tree_key_from_value(ctx, &key) {
            use std::ops::Bound;
            let res = tm_fast_with(ctx, this, |bt| {
                bt.range((Bound::Excluded(tk), Bound::Unbounded))
                    .next()
                    .map(|(k, _)| k.clone())
            });
            return Ok(Some(
                res.map(|k| tree_key_to_value(ctx, &k))
                    .unwrap_or(Value::Object(None)),
            ));
        }
        return Ok(Some(Value::Object(None)));
    }
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    // Family-1 stale-ObjectRef fix: use tm_binary_search's refreshed `data`
    // (the Comparator invocation inside it can trigger a moving GC).
    let (tm_search_result, _this, data, _key) =
        tm_binary_search(ctx, this, data, size, &comparator, &key)?;
    match tm_search_result {
        Ok(idx) => {
            let next = idx + 1;
            if next < size as usize {
                Ok(Some(ctx.get_array_element(data, next * 2)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
        Err(pos) => {
            if pos < size as usize {
                Ok(Some(ctx.get_array_element(data, pos * 2)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
    }
}

// lowerKey: largest key strictly < given key
pub(crate) fn native_tm_lower_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let (this, key) = tm_sync_with_arg(ctx, this, key)?;
    // See `tm_nav_key_check`: refuses only on a NON-empty map, and only under
    // natural ordering. Ahead of the fast-mode arm, whose
    // `tree_key_from_value` answers `None` for a null key and returns null.
    tm_nav_key_check(&*ctx, this, key)?;
    if tm_is_fast_mode(ctx, this) {
        if let Some(tk) = tree_key_from_value(ctx, &key) {
            use std::ops::Bound;
            let res = tm_fast_with(ctx, this, |bt| {
                bt.range((Bound::Unbounded, Bound::Excluded(tk)))
                    .next_back()
                    .map(|(k, _)| k.clone())
            });
            return Ok(Some(
                res.map(|k| tree_key_to_value(ctx, &k))
                    .unwrap_or(Value::Object(None)),
            ));
        }
        return Ok(Some(Value::Object(None)));
    }
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    // Family-1 stale-ObjectRef fix: use tm_binary_search's refreshed `data`
    // (the Comparator invocation inside it can trigger a moving GC).
    let (tm_search_result, _this, data, _key) =
        tm_binary_search(ctx, this, data, size, &comparator, &key)?;
    match tm_search_result {
        Ok(idx) => {
            if idx > 0 {
                Ok(Some(ctx.get_array_element(data, (idx - 1) * 2)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
        Err(pos) => {
            if pos > 0 {
                Ok(Some(ctx.get_array_element(data, (pos - 1) * 2)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
    }
}

/// Build the `Map.Entry` that `TreeMap`'s *relative-navigation* accessors hand
/// back — `firstEntry`, `lastEntry`, `pollFirstEntry`, `pollLastEntry`,
/// `floorEntry`, `ceilingEntry`, `higherEntry`, `lowerEntry`, and nothing else.
///
/// Every one of those is `exportEntry(e)` in the JDK, which is
/// `new AbstractMap.SimpleImmutableEntry<>(e)` — a SNAPSHOT, whose `setValue`
/// throws. `TreeMap.firstEntryIsImmutable` measured
/// `tm.firstEntry().setValue(99)` returning normally here against HotSpot's
/// `java.lang.UnsupportedOperationException`, because this allocated the
/// MUTABLE twin. `AbstractMap.SimpleImmutableEntry.setValue`: "@throws
/// UnsupportedOperationException always".
///
/// The two classes have the same two instance fields in the same order (`key`,
/// `value`; `javap -p` on both), so nothing about the direct field stores below
/// changes — only which `setValue` the receiver resolves to.
///
/// This is deliberately NOT the entry-set path. `TreeMap.entrySet()`'s iterator
/// yields the LIVE `TreeMap.Entry`, whose `setValue` writes through, and
/// `TreeMap.entrySetEntryIsLive` already matches HotSpot; that path allocates
/// its own `AbstractMap$SimpleEntry` through `alloc_live_entry` and is
/// untouched. Getting these two backwards in either direction is a divergence,
/// which is why the split is stated here rather than left to the call sites.
pub(crate) fn tm_make_entry(
    ctx: &mut dyn NativeContext,
    key: Value,
    value: Value,
) -> Result<ObjectRef, MethodCallFailed> {
    // Both `Simple*Entry` classes are real `Map.Entry` implementations, so
    // reflection (Class.getMethods) and reflective property access see
    // getKey/getValue. The old fabricated "HashMap$Entry" does not implement
    // Map.Entry — see native_tm_entry_set for the SpEL EL1008E that motivated
    // that change.
    // GC-safety: same hazard as `native_map_entry` — pin `key`/`value` across
    // the allocation so the entry is populated with post-move addresses.
    let key_pin = pin_value(ctx, key);
    let value_pin = pin_value(ctx, value);
    let entry = try_alloc_synthetic(ctx, "java/util/AbstractMap$SimpleImmutableEntry", 2)?;
    let key = read_pinned_elem(ctx, key_pin, key);
    let value = read_pinned_elem(ctx, value_pin, value);
    if key_pin != usize::MAX {
        ctx.unpin_native_roots(key_pin);
    } else if value_pin != usize::MAX {
        ctx.unpin_native_roots(value_pin);
    }
    ctx.set_field(entry, 0, key);
    ctx.set_field(entry, 1, value);
    Ok(entry)
}

/// Build a `Map.Entry` for the array-backed slot pair at logical index `i`
/// (key at `i*2`, value at `i*2+1`). Shared by the relative-`*Entry` natives.
pub(crate) fn tm_array_entry(
    ctx: &mut dyn NativeContext,
    data: ObjectRef,
    i: usize,
) -> Result<Value, MethodCallFailed> {
    let k = ctx.get_array_element(data, i * 2);
    let v = ctx.get_array_element(data, i * 2 + 1);
    Ok(Value::Object(Some(tm_make_entry(ctx, k, v)?)))
}

// The relative-navigation `*Entry` natives. TreeMap exposes `higherKey`/
// `lowerKey`/`floorKey`/`ceilingKey` (key-only, above) plus their `*Entry`
// counterparts that return a `Map.Entry`. These mirror the key versions
// exactly, projecting the resolved slot to an entry instead of a bare key.
// `NavigableMap`/`UnmodifiableMap` route here too (registration below).

// ceilingEntry: entry for the smallest key >= given key
pub(crate) fn native_tm_ceiling_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let (this, key) = tm_sync_with_arg(ctx, this, key)?;
    // See `tm_nav_key_check`: refuses only on a NON-empty map, and only under
    // natural ordering. Ahead of the fast-mode arm, whose
    // `tree_key_from_value` answers `None` for a null key and returns null.
    tm_nav_key_check(&*ctx, this, key)?;
    if tm_is_fast_mode(ctx, this) {
        if let Some(tk) = tree_key_from_value(ctx, &key) {
            let res = tm_fast_with(ctx, this, |bt| {
                bt.range(tk..).next().map(|(k, v)| (k.clone(), *v))
            });
            return Ok(Some(match res {
                Some((tk, v)) => {
                    let k = tree_key_to_value(ctx, &tk);
                    Value::Object(Some(tm_make_entry(ctx, k, v)?))
                }
                None => Value::Object(None),
            }));
        }
        return Ok(Some(Value::Object(None)));
    }
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    // Family-1 stale-ObjectRef fix: use tm_binary_search's refreshed `data`
    // (the Comparator invocation inside it can trigger a moving GC).
    let (tm_search_result, _this, data, _key) =
        tm_binary_search(ctx, this, data, size, &comparator, &key)?;
    match tm_search_result {
        Ok(idx) => Ok(Some(tm_array_entry(ctx, data, idx)?)),
        Err(pos) => {
            if pos < size as usize {
                Ok(Some(tm_array_entry(ctx, data, pos)?))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
    }
}

// floorEntry: entry for the largest key <= given key
pub(crate) fn native_tm_floor_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let (this, key) = tm_sync_with_arg(ctx, this, key)?;
    // See `tm_nav_key_check`: refuses only on a NON-empty map, and only under
    // natural ordering. Ahead of the fast-mode arm, whose
    // `tree_key_from_value` answers `None` for a null key and returns null.
    tm_nav_key_check(&*ctx, this, key)?;
    if tm_is_fast_mode(ctx, this) {
        if let Some(tk) = tree_key_from_value(ctx, &key) {
            let res = tm_fast_with(ctx, this, |bt| {
                bt.range(..=tk).next_back().map(|(k, v)| (k.clone(), *v))
            });
            return Ok(Some(match res {
                Some((tk, v)) => {
                    let k = tree_key_to_value(ctx, &tk);
                    Value::Object(Some(tm_make_entry(ctx, k, v)?))
                }
                None => Value::Object(None),
            }));
        }
        return Ok(Some(Value::Object(None)));
    }
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    // Family-1 stale-ObjectRef fix: use tm_binary_search's refreshed `data`
    // (the Comparator invocation inside it can trigger a moving GC).
    let (tm_search_result, _this, data, _key) =
        tm_binary_search(ctx, this, data, size, &comparator, &key)?;
    match tm_search_result {
        Ok(idx) => Ok(Some(tm_array_entry(ctx, data, idx)?)),
        Err(pos) => {
            if pos > 0 {
                Ok(Some(tm_array_entry(ctx, data, pos - 1)?))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
    }
}

// higherEntry: entry for the smallest key strictly > given key
pub(crate) fn native_tm_higher_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let (this, key) = tm_sync_with_arg(ctx, this, key)?;
    // See `tm_nav_key_check`: refuses only on a NON-empty map, and only under
    // natural ordering. Ahead of the fast-mode arm, whose
    // `tree_key_from_value` answers `None` for a null key and returns null.
    tm_nav_key_check(&*ctx, this, key)?;
    if tm_is_fast_mode(ctx, this) {
        if let Some(tk) = tree_key_from_value(ctx, &key) {
            use std::ops::Bound;
            let res = tm_fast_with(ctx, this, |bt| {
                bt.range((Bound::Excluded(tk), Bound::Unbounded))
                    .next()
                    .map(|(k, v)| (k.clone(), *v))
            });
            return Ok(Some(match res {
                Some((tk, v)) => {
                    let k = tree_key_to_value(ctx, &tk);
                    Value::Object(Some(tm_make_entry(ctx, k, v)?))
                }
                None => Value::Object(None),
            }));
        }
        return Ok(Some(Value::Object(None)));
    }
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    // Family-1 stale-ObjectRef fix: use tm_binary_search's refreshed `data`
    // (the Comparator invocation inside it can trigger a moving GC).
    let (tm_search_result, _this, data, _key) =
        tm_binary_search(ctx, this, data, size, &comparator, &key)?;
    match tm_search_result {
        Ok(idx) => {
            let next = idx + 1;
            if next < size as usize {
                Ok(Some(tm_array_entry(ctx, data, next)?))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
        Err(pos) => {
            if pos < size as usize {
                Ok(Some(tm_array_entry(ctx, data, pos)?))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
    }
}

// lowerEntry: entry for the largest key strictly < given key
pub(crate) fn native_tm_lower_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let (this, key) = tm_sync_with_arg(ctx, this, key)?;
    // See `tm_nav_key_check`: refuses only on a NON-empty map, and only under
    // natural ordering. Ahead of the fast-mode arm, whose
    // `tree_key_from_value` answers `None` for a null key and returns null.
    tm_nav_key_check(&*ctx, this, key)?;
    if tm_is_fast_mode(ctx, this) {
        if let Some(tk) = tree_key_from_value(ctx, &key) {
            use std::ops::Bound;
            let res = tm_fast_with(ctx, this, |bt| {
                bt.range((Bound::Unbounded, Bound::Excluded(tk)))
                    .next_back()
                    .map(|(k, v)| (k.clone(), *v))
            });
            return Ok(Some(match res {
                Some((tk, v)) => {
                    let k = tree_key_to_value(ctx, &tk);
                    Value::Object(Some(tm_make_entry(ctx, k, v)?))
                }
                None => Value::Object(None),
            }));
        }
        return Ok(Some(Value::Object(None)));
    }
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    // Family-1 stale-ObjectRef fix: use tm_binary_search's refreshed `data`
    // (the Comparator invocation inside it can trigger a moving GC).
    let (tm_search_result, _this, data, _key) =
        tm_binary_search(ctx, this, data, size, &comparator, &key)?;
    match tm_search_result {
        Ok(idx) => {
            if idx > 0 {
                Ok(Some(tm_array_entry(ctx, data, idx - 1)?))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
        Err(pos) => {
            if pos > 0 {
                Ok(Some(tm_array_entry(ctx, data, pos - 1)?))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
    }
}

pub(crate) fn native_tm_first_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let this = tm_sync_native_state(ctx, this)?;
    if tm_is_fast_mode(ctx, this) {
        let first = tm_fast_with(ctx, this, |bt| {
            bt.iter().next().map(|(k, v)| (k.clone(), *v))
        });
        match first {
            Some((tk, v)) => {
                let k = tree_key_to_value(ctx, &tk);
                let entry = tm_make_entry(ctx, k, v)?;
                return Ok(Some(Value::Object(Some(entry))));
            }
            None => return Ok(Some(Value::Object(None))),
        }
    }
    let (data_opt, size, _) = tm_state(ctx, this);
    // Treat a None backing array as empty rather than panicking on `unwrap`.
    let data = match data_opt {
        Some(d) if size != 0 => d,
        _ => return Ok(Some(Value::Object(None))),
    };
    let k = ctx.get_array_element(data, 0);
    let v = ctx.get_array_element(data, 1);
    let entry = tm_make_entry(ctx, k, v)?;
    Ok(Some(Value::Object(Some(entry))))
}

pub(crate) fn native_tm_last_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let this = tm_sync_native_state(ctx, this)?;
    if tm_is_fast_mode(ctx, this) {
        let last = tm_fast_with(ctx, this, |bt| {
            bt.iter().next_back().map(|(k, v)| (k.clone(), *v))
        });
        match last {
            Some((tk, v)) => {
                let k = tree_key_to_value(ctx, &tk);
                let entry = tm_make_entry(ctx, k, v)?;
                return Ok(Some(Value::Object(Some(entry))));
            }
            None => return Ok(Some(Value::Object(None))),
        }
    }
    let (data_opt, size, _) = tm_state(ctx, this);
    // Treat a None backing array as empty rather than panicking on `unwrap`.
    let data = match data_opt {
        Some(d) if size != 0 => d,
        _ => return Ok(Some(Value::Object(None))),
    };
    let last = (size as usize - 1) * 2;
    let k = ctx.get_array_element(data, last);
    let v = ctx.get_array_element(data, last + 1);
    let entry = tm_make_entry(ctx, k, v)?;
    Ok(Some(Value::Object(Some(entry))))
}

pub(crate) fn native_tm_poll_first_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let this = tm_sync_native_state(ctx, this)?;
    if tm_view_spec(ctx, this).is_some() {
        return tm_view_poll_entry(ctx, this, true);
    }
    if tm_is_fast_mode(ctx, this) {
        let removed = tm_fast_with(ctx, this, |bt| {
            let first = bt.iter().next().map(|(k, _)| k.clone())?;
            let v = bt.remove(&first)?;
            Some((first, v, bt.len() as i32))
        });
        match removed {
            Some((tk, v, new_size)) => {
                tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(new_size));
                let k = tree_key_to_value(ctx, &tk);
                let entry = tm_make_entry(ctx, k, v)?;
                return Ok(Some(Value::Object(Some(entry))));
            }
            None => return Ok(Some(Value::Object(None))),
        }
    }
    let (data_opt, size, _) = tm_state(ctx, this);
    // Treat a None backing array as empty rather than panicking on `unwrap`.
    let data = match data_opt {
        Some(d) if size != 0 => d,
        _ => return Ok(Some(Value::Object(None))),
    };
    let k = ctx.get_array_element(data, 0);
    let v = ctx.get_array_element(data, 1);
    tm_remove_at(ctx, data, size, 0);
    tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(size - 1));
    let entry = tm_make_entry(ctx, k, v)?;
    Ok(Some(Value::Object(Some(entry))))
}

/// `pollFirstEntry` / `pollLastEntry` on a navigable view: take the end entry in
/// the VIEW's order and delete its key from the backing map.
///
/// The view's cached array is already sorted by the view's own (possibly
/// reversed) comparator, so "first" is index 0 for an ascending and a descending
/// view alike — there is no second code path for descending, which is the whole
/// point of storing the reversed comparator on the view.
pub(crate) fn tm_view_poll_entry(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    first: bool,
) -> MethodCallResult {
    let (data_opt, size, _) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) if size > 0 => d,
        _ => return Ok(Some(Value::Object(None))),
    };
    let idx = if first { 0 } else { (size as usize) - 1 };
    let k = ctx.get_array_element(data, idx * 2);
    let v = ctx.get_array_element(data, idx * 2 + 1);
    // GC-SAFETY: the removal dispatches the comparator and allocates, so `k` and
    // `v` — needed afterwards to build the returned entry — are pinned across
    // it. `this` is pinned because the redirect re-reads its spec.
    let this_pin = ctx.pin_native_root(this);
    let k_pin = pin_value(ctx, k);
    let v_pin = pin_value(ctx, v);
    let removed = native_tm_remove(ctx, &[Value::Object(Some(this)), k]);
    let k = read_pinned_elem(ctx, k_pin, k);
    let v = read_pinned_elem(ctx, v_pin, v);
    ctx.unpin_native_roots(this_pin);
    let _ = removed?;
    let entry = tm_make_entry(ctx, k, v)?;
    Ok(Some(Value::Object(Some(entry))))
}

pub(crate) fn native_tm_poll_last_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let this = tm_sync_native_state(ctx, this)?;
    if tm_view_spec(ctx, this).is_some() {
        return tm_view_poll_entry(ctx, this, false);
    }
    if tm_is_fast_mode(ctx, this) {
        let removed = tm_fast_with(ctx, this, |bt| {
            let last = bt.iter().next_back().map(|(k, _)| k.clone())?;
            let v = bt.remove(&last)?;
            Some((last, v, bt.len() as i32))
        });
        match removed {
            Some((tk, v, new_size)) => {
                tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(new_size));
                let k = tree_key_to_value(ctx, &tk);
                let entry = tm_make_entry(ctx, k, v)?;
                return Ok(Some(Value::Object(Some(entry))));
            }
            None => return Ok(Some(Value::Object(None))),
        }
    }
    let (data_opt, size, _) = tm_state(ctx, this);
    // Treat a None backing array as empty rather than panicking on `unwrap`.
    let data = match data_opt {
        Some(d) if size != 0 => d,
        _ => return Ok(Some(Value::Object(None))),
    };
    let last = (size as usize - 1) * 2;
    let k = ctx.get_array_element(data, last);
    let v = ctx.get_array_element(data, last + 1);
    ctx.set_array_element(data, last, Value::Object(None));
    ctx.set_array_element(data, last + 1, Value::Object(None));
    tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(size - 1));
    let entry = tm_make_entry(ctx, k, v)?;
    Ok(Some(Value::Object(Some(entry))))
}

/// `java.util.TreeMap$Entry`, the real node class. `javap -p` (JDK 25):
/// `K key; V value; Entry left; Entry right; Entry parent; boolean color`.
pub(crate) const TM_ENTRY_CLASS: &str = "java/util/TreeMap$Entry";
/// `TreeMap.RED` / `.BLACK`. The JDK spells them `false` / `true`.
pub(crate) const TM_RED: i32 = 0;
pub(crate) const TM_BLACK: i32 = 1;

/// The shape `TreeMap.buildFromSorted` would give `n` sorted entries, as plain
/// indices: `(left, right, parent, color, root)` with `-1` for absent.
///
/// Computed as integers BEFORE anything is allocated, which is what keeps the
/// mirror GC-safe: the only heap work afterwards is one field-writing pass with
/// every node reachable from a pinned array.
pub(crate) fn tm_build_tree_shape(n: usize) -> (Vec<i32>, Vec<i32>, Vec<i32>, Vec<i32>, i32) {
    let mut left = vec![-1i32; n];
    let mut right = vec![-1i32; n];
    let mut parent = vec![-1i32; n];
    let mut color = vec![TM_BLACK; n];
    if n == 0 {
        return (left, right, parent, color, -1);
    }
    // `TreeMap.computeRedLevel(int size)`, JDK 21+: the level whose nodes are
    // RED is the deepest complete one, so the tree is a valid red-black tree
    // (every root-to-leaf path has the same BLACK count).
    let red_level = 31 - ((n as u32) + 1).leading_zeros() as i32;
    // Explicit stack rather than recursion: `n` is unbounded from Java, and a
    // native is not the place to inherit the JVM's stack depth. Each frame
    // carries which SIDE of its parent it is, rather than trying to infer that
    // from the indices afterwards -- the inference is exactly the kind of
    // almost-right arithmetic that produces a tree which walks correctly in
    // order and is silently unbalanced.
    // (lo, hi, level, parent, is_left)
    let mut stack: Vec<(i64, i64, i32, i32, bool)> = vec![(0, n as i64 - 1, 0, -1, false)];
    let mut root = -1i32;
    while let Some((lo, hi, level, par, is_left)) = stack.pop() {
        if hi < lo {
            continue;
        }
        // `>>> 1` on the SUM, matching `buildFromSorted`'s own midpoint, so the
        // shape is the one java.base would have produced for these entries.
        let mid = (((lo + hi) as u64) >> 1) as usize;
        parent[mid] = par;
        if level == red_level {
            color[mid] = TM_RED;
        }
        if par < 0 {
            root = mid as i32;
        } else if is_left {
            left[par as usize] = mid as i32;
        } else {
            right[par as usize] = mid as i32;
        }
        stack.push((lo, mid as i64 - 1, level + 1, mid as i32, true));
        stack.push((mid as i64 + 1, hi, level + 1, mid as i32, false));
    }
    (left, right, parent, color, root)
}

/// Populate the receiver's real `java.util.TreeMap.root` with a real
/// `TreeMap$Entry` red-black tree over its current contents.
///
/// # What this closes
///
/// `WORKER-2` section 5 row 3 asks for `TreeMap`'s own diagnosis, and the
/// answer (`WORKER-2-NOTE-1` section 5) is that `TreeMap` has no `this$0`
/// problem at all -- its whole structure lives in the address-keyed
/// `tm_array_table`, so the real JDK fields are populated only where something
/// took the trouble. `size` and `comparator` were mirrored, `modCount` was
/// fixed one commit back, and `root` was the last one left:
///
/// ```text
///   TreeMap, 20 puts + 1 remove   HotSpot 25.0.3+9        before
///     root                        a java.util.TreeMap$Entry tree   null
/// ```
///
/// # Why it is safe to build
///
/// `buildFromSorted` is a pure function of a sorted sequence and
/// `computeRedLevel` gives a genuinely valid colouring, so the result is a
/// real red-black tree rather than a shape that merely looks like one. This VM
/// already proves the class is buildable here: `TreeMap.clone()` returns a map
/// whose `root` IS populated on CratonVM too, because clone runs java.base's
/// own `buildFromSorted` bytecode.
///
/// The authoritative store does not move. `tm_array_table` stays the single
/// source of truth and this mirror is rebuilt from it; nothing reads back
/// through `root`. That direction matters: a mirror that were also an input
/// would be the two-producers-one-slot trap this file has already paid for
/// twice (`WORKER-2-NOTE-1` sections 4a and 9a.1).
///
/// # Why it is affordable
///
/// Gated on `modCount`, so an unchanged map costs one `i32` compare. See
/// [`TmArrayState::root_published_at`].
pub(crate) fn tm_publish_real_root(ctx: &mut dyn NativeContext, this: ObjectRef) {
    // A stand-in would be exactly the class-fabrication `--jdk-only` exists to
    // stop, and its fields would not be the ones real bytecode reads.
    if ctx.would_fabricate_synthetic_stub(TM_ENTRY_CLASS)
        || ctx.is_class_synthetic_stub(TM_ENTRY_CLASS)
    {
        return;
    }
    let root_slot = match ctx.resolve_field_index("java/util/TreeMap", "root") {
        Some(s) if s < ctx.object_num_fields(this) => s,
        // No `root` field to mirror into: the synthetic-JDK shape, where the
        // caller keeps exactly the behaviour it had.
        _ => return,
    };
    let mod_now = match ctx.resolve_field_index("java/util/TreeMap", "modCount") {
        Some(s) if s < ctx.object_num_fields(this) => match ctx.get_field(this, s) {
            Value::Int(m) => m,
            _ => return,
        },
        _ => return,
    };
    {
        // The key before the table lock (gc-common w26-a, see `ll_get`).
        let key = tm_obj_key(ctx, this);
        let tbl = tm_array_table().lock().unwrap();
        if let Some(st) = tbl.get(&key) {
            if st.root_published_at == mod_now {
                return;
            }
        }
    }
    // The side table DIRECTLY, never [`tm_collect_pairs`]: that funnel falls
    // back to `root` when the side table is empty, and a mirror whose source
    // can be its own output is the two-producers-one-slot trap this function's
    // doc warns about. Empty here means empty, and the mirror is cleared.
    let pairs = tm_collect_pairs_side_table(ctx, this);
    let n = pairs.len();
    if n == 0 {
        ctx.set_field(this, root_slot, Value::Object(None));
        let key = tm_obj_key(ctx, this);
        tm_array_table()
            .lock()
            .unwrap()
            .entry(key)
            .or_default()
            .root_published_at = mod_now;
        return;
    }
    let (left, right, parent, color, root_idx) = tm_build_tree_shape(n);
    let (kf, vf, lf, rf, pf, cf) = match (
        ctx.resolve_field_index(TM_ENTRY_CLASS, "key"),
        ctx.resolve_field_index(TM_ENTRY_CLASS, "value"),
        ctx.resolve_field_index(TM_ENTRY_CLASS, "left"),
        ctx.resolve_field_index(TM_ENTRY_CLASS, "right"),
        ctx.resolve_field_index(TM_ENTRY_CLASS, "parent"),
        ctx.resolve_field_index(TM_ENTRY_CLASS, "color"),
    ) {
        (Some(a), Some(b), Some(c), Some(d), Some(e), Some(f)) => (a, b, c, d, e, f),
        _ => return,
    };
    let cid = match ctx.ensure_class_initialized(TM_ENTRY_CLASS) {
        Ok(c) => c,
        Err(_) => return,
    };
    if ctx.class_name_arc_of_id(cid).as_deref() != Some(TM_ENTRY_CLASS) {
        return;
    }
    let width = ctx.class_num_total_fields(cid);
    // GC-SAFETY: every allocation below can move the heap, so the nodes live in
    // a pinned `Object[]` from the moment they exist rather than in a Rust Vec
    // the collector cannot rewrite. `this` and the key/value snapshot are
    // pinned for the same reason.
    let this_pin = ctx.pin_native_root(this);
    let keys: Vec<Value> = pairs.iter().map(|(k, _)| *k).collect();
    let vals: Vec<Value> = pairs.iter().map(|(_, v)| *v).collect();
    let (_, key_pins) = pin_value_slice(ctx, &keys);
    let (_, val_pins) = pin_value_slice(ctx, &vals);
    let nodes = alloc_ref_array(ctx, n);
    let nodes_pin = ctx.pin_native_root(nodes);
    for i in 0..n {
        let e = ctx.alloc_object(cid, width);
        let nodes = ctx.read_native_pin(nodes_pin, nodes);
        ctx.set_array_element(nodes, i, Value::Object(Some(e)));
    }
    for i in 0..n {
        let nodes = ctx.read_native_pin(nodes_pin, nodes);
        let e = match ctx.get_array_element(nodes, i) {
            Value::Object(Some(e)) => e,
            _ => continue,
        };
        let k = read_pinned_elem(ctx, key_pins[i], keys[i]);
        let v = read_pinned_elem(ctx, val_pins[i], vals[i]);
        ctx.set_field(e, kf, k);
        ctx.set_field(e, vf, v);
        ctx.set_field(e, cf, Value::Int(color[i]));
    }
    let link = |ctx: &mut dyn NativeContext, i: usize, slot: usize, target: i32| {
        let nodes = ctx.read_native_pin(nodes_pin, nodes);
        let e = match ctx.get_array_element(nodes, i) {
            Value::Object(Some(e)) => e,
            _ => return,
        };
        let t = if target < 0 {
            Value::Object(None)
        } else {
            ctx.get_array_element(nodes, target as usize)
        };
        ctx.set_field(e, slot, t);
    };
    for i in 0..n {
        link(ctx, i, lf, left[i]);
        link(ctx, i, rf, right[i]);
        link(ctx, i, pf, parent[i]);
    }
    let nodes = ctx.read_native_pin(nodes_pin, nodes);
    let root_val = if root_idx < 0 {
        Value::Object(None)
    } else {
        ctx.get_array_element(nodes, root_idx as usize)
    };
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, root_slot, root_val);
    ctx.unpin_native_roots(this_pin);
    let key = tm_obj_key(ctx, this);
    tm_array_table()
        .lock()
        .unwrap()
        .entry(key)
        .or_default()
        .root_published_at = mod_now;
}

/// Populate a `java.util.TreeSet`'s real `m` field with a real
/// `java.util.TreeMap` holding the same elements, itself carrying a real
/// `root` tree.
///
/// `WORKER-2-NOTE-1` section 5 measured `TreeSet.m` as null where HotSpot has
/// the backing map, and declined it on the same has-no-consumer/costs-per-read
/// grounds as `root`. Both halves of that objection are gone: the cost is now
/// gated on `modCount` exactly as [`tm_publish_real_root`] is, and the map this
/// mints is the same real object graph that function already builds — so `m`
/// costs one extra `TreeMap` per mutation rather than per read.
///
/// The minted map is a MIRROR, never an authority. `ts_state`'s side-table
/// entry for the SET stays the single source of truth; nothing in this file
/// reads the set's contents back through `m`. Making it readable both ways
/// would be the two-producers-one-slot trap this lane has already paid for
/// twice (`WORKER-2-NOTE-1` sections 4a and 9a.1).
pub(crate) fn ts_publish_real_backing_map(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let m_slot = match ctx.resolve_field_index("java/util/TreeSet", "m") {
        Some(s) if s < ctx.object_num_fields(this) => s,
        // The synthetic-JDK shape has no `m` to mirror into; the caller keeps
        // exactly the behaviour it had.
        _ => return,
    };
    let (data_opt, size, comparator) = ts_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return,
    };
    // Reuse the map already published for this set when the element count has
    // not moved. `TreeSet` has no `modCount` of its own — it is `m`'s — so the
    // size is the cheap structural proxy here, and the map's own
    // `tm_publish_real_root` re-gates on the real `modCount` underneath.
    if let Value::Object(Some(existing)) = ctx.get_field(this, m_slot) {
        if matches!(tm_state(ctx, existing), (_, n, _) if n == size) {
            tm_publish_real_root(ctx, existing);
            return;
        }
    }
    let this_pin = ctx.pin_native_root(this);
    let data_pin = ctx.pin_native_root(data);
    let cmp_pin = pin_value(ctx, comparator);
    let map = match try_alloc_synthetic(ctx, "java/util/TreeMap", TM_NUM_FIELDS) {
        Ok(m) => m,
        Err(_) => {
            ctx.unpin_native_roots(this_pin);
            return;
        }
    };
    let map_pin = ctx.pin_native_root(map);
    // Same as the range view above: allocated, never constructed.
    init_tm_reference_defaults(ctx, map);
    // `PRESENT` is the JDK's own sentinel value for every `TreeSet` mapping.
    // A missing one is not a reason to refuse the mirror — the VALUES of a set's
    // backing map are not observable through any `Set` operation — so fall back
    // to null rather than dropping `m` on the floor.
    let present = ctx
        .ensure_class_initialized("java/util/TreeSet")
        .ok()
        .and_then(|cid| {
            ctx.static_field_index_by_name(cid, "PRESENT")
                .map(|idx| ctx.get_static_field(cid, idx))
        })
        .unwrap_or(Value::Object(None));
    let present_pin = pin_value(ctx, present);
    let n = size.max(0) as usize;
    let buf = alloc_ref_array(ctx, (n * 2).max(TM_DEFAULT_CAPACITY * 2));
    let buf_pin = ctx.pin_native_root(buf);
    for i in 0..n {
        let data = ctx.read_native_pin(data_pin, data);
        let k = ctx.get_array_element(data, i);
        let buf = ctx.read_native_pin(buf_pin, buf);
        ctx.set_array_element(buf, i * 2, k);
        let p = read_pinned_elem(ctx, present_pin, present);
        ctx.set_array_element(buf, i * 2 + 1, p);
    }
    let map = ctx.read_native_pin(map_pin, map);
    let buf = ctx.read_native_pin(buf_pin, buf);
    tm_set_force_array(ctx, map);
    tm_set_slot(ctx, map, TM_FIELD_DATA, Value::Object(Some(buf)));
    // Hoisted: `tm_set_slot` takes `ctx` mutably, so the pinned re-read cannot
    // be an argument expression to it.
    let cmp_now = read_pinned_elem(ctx, cmp_pin, comparator);
    tm_set_slot(ctx, map, TM_FIELD_COMPARATOR, cmp_now);
    // Written LAST: `tm_set_slot` bumps the map's real `modCount` off a size
    // change, and `tm_publish_real_root` gates on that value — so the size has
    // to be in place before the root is asked for.
    tm_set_slot(ctx, map, TM_FIELD_SIZE, Value::Int(size));
    let map = ctx.read_native_pin(map_pin, map);
    tm_publish_real_root(ctx, map);
    let map = ctx.read_native_pin(map_pin, map);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, m_slot, Value::Object(Some(map)));
    ctx.unpin_native_roots(this_pin);
}

/// Refresh the real-JDK mirrors a `TreeSet` bulk read makes observable.
pub(crate) fn ts_refresh_real_mirrors(ctx: &mut dyn NativeContext, args: &[Value]) {
    if let Some(Value::Object(Some(this))) = args.first().copied() {
        ts_publish_real_backing_map(ctx, this);
    }
}

/// Refresh the real-JDK mirrors a bulk read makes observable. Called from the
/// `TreeMap` bulk-read natives, which are the only points where this VM knows
/// something is about to look at the map as an object rather than through an
/// accessor.
pub(crate) fn tm_refresh_real_mirrors(ctx: &mut dyn NativeContext, args: &[Value]) {
    if let Some(Value::Object(Some(this))) = args.first().copied() {
        tm_publish_real_root(ctx, this);
    }
}

/// The `TreeMap` view cached in `field`, if it is still THIS map's view.
///
/// A separate pair from [`cached_live_view`] because the validation differs:
/// that one reaches the source through `hs_backing_map`, and a `TreeMap` view
/// keeps its source in a trailing array slot instead — `ts_view_source` for the
/// TreeSet-shaped keySet, `values_view_source` for the two list-shaped ones.
/// Both are tried, and a view whose stashed source is not this map is refused
/// rather than adopted.
///
/// MEASURED before enabling, `apps/probes/ViewIdentityProbe`: HotSpot answers
/// `true` to `tm.keySet() == tm.keySet()` and this VM answered `false`, because
/// every call rebuilt the whole tree into a fresh view. The consequence is not
/// only identity — `AbstractCollection` does not override `equals`, so
/// `tm.values().equals(tm.values())` compared two different objects and
/// answered FALSE where HotSpot says true.
///
/// LIVENESS is the property a cache must not cost, and it is measured in the
/// same probe rather than argued: a view held across a `put`, a `remove` and an
/// in-place value replacement answers for the map's CURRENT contents on every
/// family, because these views resync from the stashed source on each read.
/// Those rows passed before this change and must keep passing after it.
///
/// The carriers are the real JDK classes (`TreeMap$KeySet`, `TreeMap$Values`,
/// `TreeMap$EntrySet`), so each is assignable to the field it is stored in —
/// which matters because `try_set_jdk_map_field` resolves a field by NAME and
/// does not check its descriptor.
pub(crate) fn cached_tm_view(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
    field: &str,
    carrier: &str,
) -> Option<ObjectRef> {
    if !map_view_cache_enabled() {
        return None;
    }
    let class_id = ctx.class_id_of_object(source);
    let slot = ctx.resolve_field_index_by_class_id(class_id, field)?;
    if slot >= ctx.object_num_fields(source) {
        return None;
    }
    let Value::Object(Some(view)) = ctx.get_field(source, slot) else {
        return None;
    };
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(view))
        .as_deref()
        != Some(carrier)
    {
        return None;
    }
    let cached_source = ts_view_source(&*ctx, view).or_else(|| values_view_source(&*ctx, view))?;
    if !std::ptr::eq(cached_source.as_ptr(), source.as_ptr()) {
        return None;
    }
    MAP_VIEW_REUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Some(view)
}

/// Record `view` as this `TreeMap`'s live view for `field`.
pub(crate) fn store_tm_view(ctx: &mut dyn NativeContext, source: ObjectRef, field: &str, view: ObjectRef) {
    if !map_view_cache_enabled() {
        return;
    }
    try_set_jdk_map_field(ctx, source, field, Value::Object(Some(view)));
}

pub(crate) fn native_tm_key_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    tm_refresh_real_mirrors(ctx, args);
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let this = tm_sync_native_state(ctx, this)?;
    // AFTER the sync, because the sync can hand back a different `this`.
    if let Some(cached) = cached_tm_view(ctx, this, "keySet", "java/util/TreeMap$KeySet") {
        return Ok(Some(Value::Object(Some(cached))));
    }
    let pairs = tm_collect_pairs(ctx, this);
    let size = pairs.len() as i32;
    // Family-1 stale-ObjectRef fix (2026-07-31): the two allocations below can
    // collect, moving both `this` and every key still held only in `pairs`.
    let this_pin = ctx.pin_native_root(this);
    let pinned_pairs = PinnedPairs::new(ctx, &pairs);
    // The carrier is `java/util/TreeMap$KeySet` — what HotSpot's
    // `treeMap.keySet().getClass()` answers — falling back to `java/util/TreeSet`
    // when that class cannot be had. Nothing about the view's STATE depends on
    // the choice: a native TreeSet keeps `(data, size, comparator)` in the
    // `ts_array_table` side-table keyed on the object, never in its fields, so
    // the carrier's own declared `m` slot is neither read nor written. What the
    // choice does need is the `native_ts_*` family registered on the carrier
    // (`register_tree_set_natives`) and the name in both force-native gates.
    let ts = match try_alloc_declared_width(ctx, "java/util/TreeMap$KeySet", TS_NUM_FIELDS) {
        Ok(o) => o,
        // Not `?` (gc-common w20-a): a refusal releases this frame's pins.
        Err(_) => match try_alloc_declared_width(ctx, "java/util/TreeSet", TS_NUM_FIELDS) {
            Ok(o) => o,
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        },
    };
    // Reserve one extra trailing slot and stash the source TreeMap there so the
    // keySet view writes through (`keySet().remove` / `iterator().remove`).
    // The slot lives beyond the logical size, so sorted iteration / binary
    // search (which use `size`) never see it, yet it is GC-scanned.
    let cap = std::cmp::max(pairs.len(), TS_DEFAULT_CAPACITY) + 1;
    let ts_pin = ctx.pin_native_root(ts);
    let buf: ObjectRef = alloc_ref_array(ctx, cap);
    for i in 0..pinned_pairs.len() {
        let (k, _) = pinned_pairs.get(&*ctx, i);
        ctx.set_array_element(buf, i, k);
    }
    let this = ctx.read_native_pin(this_pin, this);
    let ts = ctx.read_native_pin(ts_pin, ts);
    ctx.set_array_element(buf, cap - 1, Value::Object(Some(this)));
    ts_set_slot(ctx, ts, TS_FIELD_DATA, Value::Object(Some(buf)));
    ts_set_slot(ctx, ts, TS_FIELD_SIZE, Value::Int(size));
    let comparator = tm_get_slot(ctx, this, TM_FIELD_COMPARATOR);
    ts_set_slot(ctx, ts, TS_FIELD_COMPARATOR, comparator);
    ctx.unpin_native_roots(this_pin);
    // The store half, on every return path. `this` and `ts` are both
    // pin-refreshed above.
    store_tm_view(ctx, this, "keySet", ts);
    Ok(Some(Value::Object(Some(ts))))
}

/// Upper bound on the nodes [`tm_pairs_from_real_root`] will visit.
///
/// A malformed or cyclic node graph must not hang a native. The cap is far
/// above any map this VM can hold in memory and far below a livelock; hitting
/// it returns EMPTY rather than a truncated map, because a prefix of a broken
/// tree is a wrong answer that looks like a right one.
pub(crate) const TM_REAL_ROOT_WALK_CAP: usize = 1 << 24;

/// The receiver's REAL `java.util.TreeMap.root` node graph, read in key order.
///
/// # Why a one-way mirror had to grow a reader
///
/// [`tm_publish_real_root`] states the direction as a design decision —
/// *"`tm_array_table` stays the single source of truth and this mirror is
/// rebuilt from it; nothing reads back through `root`"* — and that held for as
/// long as the TreeMap natives were the only writers. They are not, in the two
/// situations this lane exists to measure: with the enforcement dial armed on
/// `java/util/TreeMap`, and with the family retired, REAL BYTECODE maintains
/// `root` and `tm_array_table` is empty. Every side-table reader then answers
/// zero for a map that has entries.
///
/// MEASURED on the wave-8 control binary, `--jdk-only`, dial armed on
/// `java/util/TreeMap` alone, `apps/probes/L1MapViewToArrayProbe` (168 rows):
///
/// ```text
///   D.treemap.keySet.toArray.len     HotSpot 3    CratonVM 0
///   D.treemap.entrySet.toArray.len   HotSpot 3    CratonVM 0
///   E.treemap.order.toArray          [a, b, c]    []
/// ```
///
/// 38 rows, and `size()`, `iterator()`, `toString()`, `contains` and removal
/// through the same views were all already right — they reach real bytecode
/// through the dispatch door, which is the door `toArray`'s internally-asked
/// `size()` does not go through.
///
/// # Why this does not re-open the two-producers trap
///
/// It is consulted from ONE place ([`tm_collect_pairs`]) and only when the side
/// table holds no pairs for the receiver, so `tm_array_table` remains the
/// authority wherever it has anything to say. The mirror's own writer reads the
/// side table directly ([`tm_collect_pairs_side_table`]) and so can never
/// republish what this function just read out of `root`.
///
/// # GC
///
/// Pure reads: no allocation, no re-entry into Java, therefore no GC point
/// inside the walk and no pin required. The `(key, value)` pairs it returns are
/// bare `ObjectRef`s and are the caller's problem the instant it allocates —
/// every caller reaches this through [`tm_collect_pairs`], whose callers
/// already pin what they keep.
pub(crate) fn tm_pairs_from_real_root(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<(Value, Value)> {
    let empty: Vec<(Value, Value)> = Vec::new();
    let Some(root_slot) = ctx.resolve_field_index("java/util/TreeMap", "root") else {
        if tmview_dbg() {
            eprintln!("[TMVIEW] real_root: no `root` field on java/util/TreeMap");
        }
        return empty;
    };
    if root_slot >= ctx.object_num_fields(this) {
        if tmview_dbg() {
            eprintln!(
                "[TMVIEW] real_root: root_slot {root_slot} >= nf {}",
                ctx.object_num_fields(this)
            );
        }
        return empty;
    }
    let got = ctx.get_field(this, root_slot);
    if tmview_dbg() {
        eprintln!(
            "[TMVIEW] real_root: recv={} root={got:?}",
            ctx.class_name_of_id(ctx.class_id_of_object(this))
                .unwrap_or_else(|| "<unknown>".into())
        );
    }
    let Value::Object(Some(root)) = got else {
        return empty;
    };
    // The slots below are `TreeMap$Entry`'s. A `root` of any other class means
    // something else owns this field and the walk would be reading whatever
    // sits at those indices — the mistyped-slot read this file guards against
    // everywhere else.
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(root))
        .as_deref()
        != Some(TM_ENTRY_CLASS)
    {
        return empty;
    }
    let (kf, vf, lf, rf) = match (
        ctx.resolve_field_index(TM_ENTRY_CLASS, "key"),
        ctx.resolve_field_index(TM_ENTRY_CLASS, "value"),
        ctx.resolve_field_index(TM_ENTRY_CLASS, "left"),
        ctx.resolve_field_index(TM_ENTRY_CLASS, "right"),
    ) {
        (Some(a), Some(b), Some(c), Some(d)) => (a, b, c, d),
        _ => return empty,
    };
    // Iterative in-order walk with an explicit stack, for the reason
    // `tm_build_tree_shape` gives for its own: `n` is unbounded from Java and a
    // native is not the place to inherit the JVM's stack depth. The flag is
    // "this node's left subtree has already been pushed".
    let mut out: Vec<(Value, Value)> = Vec::new();
    let mut stack: Vec<(ObjectRef, bool)> = vec![(root, false)];
    while let Some((node, expanded)) = stack.pop() {
        if out.len() >= TM_REAL_ROOT_WALK_CAP || stack.len() >= TM_REAL_ROOT_WALK_CAP {
            return Vec::new();
        }
        if expanded {
            out.push((ctx.get_field(node, kf), ctx.get_field(node, vf)));
            continue;
        }
        if let Value::Object(Some(r)) = ctx.get_field(node, rf) {
            stack.push((r, false));
        }
        stack.push((node, true));
        if let Value::Object(Some(l)) = ctx.get_field(node, lf) {
            stack.push((l, false));
        }
    }
    if tmview_dbg() {
        eprintln!("[TMVIEW] real_root: walked {} pair(s)", out.len());
    }
    out
}

/// The receiver's REAL `java.util.TreeMap.size` field, if it has one.
///
/// The tie-break between the two stores, and a fair one in both directions:
/// [`tm_set_slot`] MIRRORS every side-table size change into this field by
/// name — it says why, and the reason (`TreeMap.writeObject` does
/// `s.writeInt(size)` straight off the field) is exactly that real bytecode
/// reads it without asking a native — and real bytecode MAINTAINS it when the
/// natives are gone. Whichever store was written last wrote here too.
pub(crate) fn tm_real_size_field(ctx: &dyn NativeContext, this: ObjectRef) -> Option<i32> {
    let slot = ctx.resolve_field_index("java/util/TreeMap", "size")?;
    if slot >= ctx.object_num_fields(this) {
        return None;
    }
    match ctx.get_field(this, slot) {
        Value::Int(n) => Some(n),
        _ => None,
    }
}

/// Collect (key, value) pairs in sorted order, from whichever of the two
/// stores the receiver's own `size` field agrees with.
///
/// # Why this is not "side table first"
///
/// It was, for one build, and `apps/probes/StaleViewAddAllProbe` caught it.
/// With the family RETIRED, something on the read path seeds `tm_array_table`
/// with the map's contents once and nothing ever updates it again — the
/// writers that would have are the retired natives — so the side table becomes
/// a SNAPSHOT that outlives its map:
///
/// ```text
///   cratonvm-l1w8-t5, --jdk-only, apps/probes/StaleViewAddAllProbe
///     TreeMap after growth size()          5   OK      <- the real field
///     TreeMap after growth for-each        5   OK      <- real bytecode
///     TreeMap after growth toArray         3   FAIL    <- the side table
///     TreeMap held view addAll after 6     3   FAIL
/// ```
///
/// `size()` and the for-each walk were right because they never ask a native;
/// every row that went through one read the stale copy. Seven rows, and the
/// only probe in a 163-probe tree that moved.
///
/// So neither store gets to be the authority by position. The receiver's own
/// `size` field names the winner ([`tm_real_size_field`]), and it is written by
/// both of them. Where they agree — every map this crate's natives manage —
/// the side table answers exactly as it always did.
///
/// An empty map costs one field read and one null `root`.
pub(crate) fn tm_collect_pairs(ctx: &mut dyn NativeContext, this: ObjectRef) -> Vec<(Value, Value)> {
    let side = tm_collect_pairs_side_table(ctx, this);
    let Some(n) = tm_real_size_field(&*ctx, this) else {
        // No real `size` to arbitrate with (a synthetic-JDK image): the
        // historical order, plus the real-root fallback for an empty table.
        if !side.is_empty() {
            return side;
        }
        return tm_pairs_from_real_root(&*ctx, this);
    };
    let want = n.max(0) as usize;
    if want == side.len() && !side.is_empty() {
        return side;
    }
    let real = tm_pairs_from_real_root(&*ctx, this);
    if tmview_dbg() && real.len() != side.len() {
        eprintln!(
            "[TMVIEW] tm_collect_pairs side={} real_root={} size_field={n}",
            side.len(),
            real.len()
        );
    }
    if real.len() == want {
        return real;
    }
    // Neither store matches the field. Prefer whichever has anything to say,
    // side table first, rather than inventing a third answer — and say nothing
    // only when both are empty.
    if side.is_empty() {
        return real;
    }
    side
}

/// Collect (key, value) pairs in sorted order for both fast and array modes,
/// from `tm_array_table` and nothing else.
/// Boxes fast-mode primitive keys back to Java wrapper objects after
/// releasing the side-table lock (boxing may re-enter the VM).
pub(crate) fn tm_collect_pairs_side_table(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Vec<(Value, Value)> {
    if tm_is_fast_mode(ctx, this) {
        let raw: Vec<(TreeKey, Value)> = tm_fast_with(ctx, this, |bt| {
            bt.iter().map(|(k, v)| (k.clone(), *v)).collect()
        });
        // Family-1 stale-ObjectRef fix (2026-07-31): `tree_key_to_value`
        // ALLOCATES (`create_string` / `Wrapper.valueOf`), so boxing key `i`
        // can trigger a young collection that relocates (a) every value after
        // it, which lives only as a raw `ObjectRef` in `raw`, and (b) every key
        // already boxed on a previous iteration. Neither Vec is rewritten by
        // the collector. Pin both sides across the whole conversion and read
        // them back through their pins at the end.
        let anchor = ctx.pin_native_root(this);
        let vals: Vec<Value> = raw.iter().map(|(_, v)| *v).collect();
        let (_, val_pins) = pin_value_slice(ctx, &vals);
        let mut keys: Vec<Value> = Vec::with_capacity(raw.len());
        let mut key_pins: Vec<usize> = Vec::with_capacity(raw.len());
        for (k, _) in raw.iter() {
            let boxed = tree_key_to_value(ctx, k);
            key_pins.push(pin_value(ctx, boxed));
            keys.push(boxed);
        }
        let out: Vec<(Value, Value)> = (0..raw.len())
            .map(|i| {
                (
                    read_pinned_elem(ctx, key_pins[i], keys[i]),
                    read_pinned_elem(ctx, val_pins[i], vals[i]),
                )
            })
            .collect();
        ctx.unpin_native_roots(anchor);
        out
    } else {
        let (data_opt, size, _) = tm_state(ctx, this);
        let data = match data_opt {
            Some(d) => d,
            None => return Vec::new(),
        };
        let mut out = Vec::with_capacity(size as usize);
        for i in 0..(size as usize) {
            let k = ctx.get_array_element(data, i * 2);
            let v = ctx.get_array_element(data, i * 2 + 1);
            out.push((k, v));
        }
        out
    }
}

/// GC-safe cursor over a [`tm_collect_pairs`] snapshot.
///
/// Family-1 stale-ObjectRef fix (2026-07-31): `tm_collect_pairs` hands back a
/// bare Rust `Vec` of raw `ObjectRef`s, and every consumer then runs code that
/// allocates while walking it — `tree_compare` dispatching a custom
/// `Comparable`/`Comparator`, `native_tm_put`, `alloc_ref_array`,
/// `alloc_live_entry`, a `toString()`, a user lambda. A young collection during
/// any of those relocates every entry the walk has not consumed yet, and
/// nothing rewrites the `Vec`, so the rest of the walk dereferences moved
/// addresses. Reproduced on `headMap` as
/// `ClassCastException: class java.lang.Object cannot be cast to class
/// java.lang.Comparable` (the stale key decoded as whatever now occupies the
/// address); see the sibling
/// `bug-h2-priorityblockingqueue-stale-objectref-classcastexception-FIXED.md`
/// for the same defect in `PriorityBlockingQueue`.
///
/// Pin the whole snapshot once, then read each element back through its pin
/// immediately before every use — the collector remaps `native_pin_roots`, so
/// that is the only view of the snapshot that stays correct. Same shape
/// `native_tm_for_each` already used inline.
///
/// The pins are pushed on the caller's pin stack. Release them with
/// `ctx.unpin_native_roots(pinned.base())`, or — when the caller took an
/// earlier pin of its own and unpins from that handle — construct this
/// afterwards and let the caller's existing `unpin_native_roots` cover it.
///
/// Cost is one pin per entry, on a walk that already performs one interpreted
/// call (or one allocation) per entry.
pub(crate) struct PinnedPairs {
    pub(crate) orig: Vec<Value>,
    pub(crate) handles: Vec<usize>,
    pub(crate) base: usize,
}

impl PinnedPairs {
    fn new(ctx: &mut dyn NativeContext, pairs: &[(Value, Value)]) -> Self {
        let orig: Vec<Value> = pairs.iter().flat_map(|(k, v)| [*k, *v]).collect();
        let (base, handles) = pin_value_slice(ctx, &orig);
        Self {
            orig,
            handles,
            base,
        }
    }

    fn len(&self) -> usize {
        self.orig.len() / 2
    }

    /// Handle to pass to `unpin_native_roots`; `usize::MAX` when nothing was
    /// pinned (an all-primitive snapshot), which `unpin_native_roots` ignores.
    fn base(&self) -> usize {
        self.base
    }

    /// The current (post-GC) key/value at `i`.
    fn get(&self, ctx: &dyn NativeContext, i: usize) -> (Value, Value) {
        (
            read_pinned_elem(ctx, self.handles[i * 2], self.orig[i * 2]),
            read_pinned_elem(ctx, self.handles[i * 2 + 1], self.orig[i * 2 + 1]),
        )
    }

    /// Refresh `k`/`v` in place. Used immediately before every dereference so
    /// the caller keeps ordinary `Value` locals instead of re-binding.
    fn refresh(&self, ctx: &dyn NativeContext, i: usize, k: &mut Value, v: &mut Value) {
        let (nk, nv) = self.get(ctx, i);
        *k = nk;
        *v = nv;
    }
}

pub(crate) fn native_tm_values(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    tm_refresh_real_mirrors(ctx, args);
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let this = tm_sync_native_state(ctx, this)?;
    if let Some(cached) = cached_tm_view(ctx, this, "values", "java/util/TreeMap$Values") {
        return Ok(Some(Value::Object(Some(cached))));
    }
    // Live view: the ArrayList stashes the source TreeMap so
    // `values().iterator().remove()` deletes the matching entry from the tree.
    let pairs = tm_collect_pairs(ctx, this);
    let vals: Vec<Value> = pairs.into_iter().map(|(_, v)| v).collect();
    let carrier = values_carrier_for(&*ctx, this);
    let list = make_view_list_of(ctx, this, &vals, carrier)?;
    // `this` is a pre-allocation address by now, so the source is re-derived
    // from the view's own back-reference -- the one pointer to it guaranteed
    // live and current. Same shape as `native_map_values`.
    if let Some(src) = values_view_source(&*ctx, list) {
        store_tm_view(ctx, src, "values", list);
    }
    Ok(Some(Value::Object(Some(list))))
}

pub(crate) fn native_tm_entry_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    tm_refresh_real_mirrors(ctx, args);
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let this = tm_sync_native_state(ctx, this)?;
    if let Some(cached) = cached_tm_view(ctx, this, "entrySet", TM_ENTRY_SET_CARRIER) {
        return Ok(Some(Value::Object(Some(cached))));
    }
    let pairs = tm_collect_pairs(ctx, this);
    // Live view: build Map.Entry objects and stash the source TreeMap so
    // removing an entry through the list (or its iterator) deletes the key.
    //
    // Family-1 stale-ObjectRef fix (2026-07-31): `alloc_live_entry` allocates
    // once per pair, so entry `i`'s allocation can relocate every pair after it
    // AND every entry already produced. `alloc_live_entry` pins its own three
    // arguments, but nothing was keeping these two Vecs current.
    let this_pin = ctx.pin_native_root(this);
    let pinned_pairs = PinnedPairs::new(ctx, &pairs);
    let mut entries: Vec<Value> = Vec::with_capacity(pinned_pairs.len());
    let mut entry_pins: Vec<usize> = Vec::with_capacity(pinned_pairs.len());
    for i in 0..pinned_pairs.len() {
        let (k, v) = pinned_pairs.get(&*ctx, i);
        let this = ctx.read_native_pin(this_pin, this);
        let entry = Value::Object(Some(alloc_live_entry(
            ctx,
            // The FALLBACK only: `alloc_live_entry` upgrades this to
            // `java.util.TreeMap$Entry` when the image has it. Kept as a real
            // `Map.Entry` so reflection still sees getKey/getValue (SpEL
            // `map.?[key...]` selection over a TreeMap) on a stripped image;
            // the fabricated "HashMap$Entry" does not implement Map.Entry →
            // EL1008E.
            "java/util/AbstractMap$SimpleEntry",
            k,
            v,
            this,
        )?));
        entry_pins.push(pin_value(ctx, entry));
        entries.push(entry);
    }
    let entries: Vec<Value> = (0..entries.len())
        .map(|i| read_pinned_elem(ctx, entry_pins[i], entries[i]))
        .collect();
    let this = ctx.read_native_pin(this_pin, this);
    let list = make_view_list_of(ctx, this, &entries, TM_ENTRY_SET_CARRIER)?;
    if let Some(src) = values_view_source(&*ctx, list) {
        store_tm_view(ctx, src, "entrySet", list);
    }
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(list))))
}

pub(crate) fn native_tm_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // RULE F, and deliberately AHEAD of `tm_sync_native_state`: the JDK's
    // `Objects.requireNonNull(action)` is `forEach`'s first statement, so a
    // null action must win over anything the sync could raise (it re-enters a
    // user `Comparator`). See `reject_null_functional`.
    reject_null_functional(args.get(1))?;
    let action0 = args.get(1).copied().unwrap_or(Value::Object(None));
    let (this, action0) = tm_sync_with_arg(ctx, this, action0)?;
    let action = match action0 {
        Value::Object(Some(r)) => r,
        _ => return Ok(None),
    };
    let pairs = tm_collect_pairs(ctx, this);
    let action_pin = ctx.pin_native_root(action);
    let flat: Vec<Value> = pairs.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (_, flat_pins) = pin_value_slice(ctx, &flat);
    for i in 0..pairs.len() {
        let action = ctx.read_native_pin(action_pin, action);
        let k = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
        let v = read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]);
        if let Err(e) = ctx.invoke_virtual(
            action,
            "accept",
            "(Ljava/lang/Object;Ljava/lang/Object;)V",
            &[k, v],
        ) {
            ctx.unpin_native_roots(action_pin);
            return Err(e);
        }
    }
    ctx.unpin_native_roots(action_pin);
    Ok(None)
}

pub(crate) fn native_tm_get_or_default(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // cceres5: `tm_materialize_deser_array` can allocate and
    // `tm_binary_search` dispatches user Comparator bytecode — the raw `args`
    // key/default go stale across both; the absent-key branch then returned a
    // pre-move default (see native_map_get_or_default's live capture).
    let key0 = args.get(1).copied().unwrap_or(Value::Object(None));
    let key_pin = pin_value(ctx, key0);
    let default0 = args.get(2).copied().unwrap_or(Value::Object(None));
    let default_pin = pin_value(ctx, default0);
    let this_pin = ctx.pin_native_root(this);
    let this = tm_sync_native_state(ctx, this)?;
    let this = ctx.read_native_pin(this_pin, this);
    let key = read_pinned_elem(ctx, key_pin, key0);
    let first_pin = if key_pin == usize::MAX {
        if default_pin == usize::MAX {
            this_pin
        } else {
            default_pin
        }
    } else {
        key_pin
    };
    // W7-36 named this native as a suspected `getOrDefault`-over-a-null-VALUE
    // defect. It is not one — see the record's correction: both storage paths
    // read the mapping itself (`bt.get(&tk).copied()` / the array slot), so a
    // key present with a null value already answers `null` rather than the
    // default, which is what `Map.getOrDefault` specifies. What IS missing is
    // the refusal: `TreeMap.getOrDefault` is `getEntry(key)`, the same
    // null-check + `Comparable` checkcast `native_tm_get` runs, and without it
    // `new TreeMap<>().getOrDefault(null, d)` answered `d` where HotSpot throws.
    //
    // Computed into a local BEFORE the `if let`: an `if let` scrutinee's
    // temporaries — including the `&*ctx` reborrow the call takes — live to the
    // end of the `if let` in this edition, and the arm needs `ctx` mutably to
    // release the pins.
    let key_check = {
        let comparator = tm_get_slot(ctx, this, TM_FIELD_COMPARATOR);
        let empty_now = tm_is_empty_now(ctx, this);
        tree_natural_order_key_check(ctx, &comparator, key, empty_now)
    };
    if let Err(e) = key_check {
        ctx.unpin_native_roots(first_pin);
        return Err(e);
    }
    if tm_is_fast_mode(ctx, this) {
        if let Some(tk) = tree_key_from_value(ctx, &key) {
            let v = tm_fast_with(ctx, this, |bt| bt.get(&tk).copied());
            let default = read_pinned_elem(ctx, default_pin, default0);
            ctx.unpin_native_roots(first_pin);
            return Ok(Some(v.unwrap_or(default)));
        }
        let default = read_pinned_elem(ctx, default_pin, default0);
        ctx.unpin_native_roots(first_pin);
        return Ok(Some(default));
    }
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => {
            let default = read_pinned_elem(ctx, default_pin, default0);
            ctx.unpin_native_roots(first_pin);
            return Ok(Some(default));
        }
    };
    // Family-1 stale-ObjectRef fix: use tm_binary_search's refreshed `data`
    // (the Comparator invocation inside it can trigger a moving GC).
    let search = tm_binary_search(ctx, this, data, size, &comparator, &key);
    let default = read_pinned_elem(ctx, default_pin, default0);
    ctx.unpin_native_roots(first_pin);
    let (tm_search_result, _this, data, _key) = search?;
    match tm_search_result {
        Ok(idx) => Ok(Some(ctx.get_array_element(data, idx * 2 + 1))),
        Err(_) => Ok(Some(default)),
    }
}

pub(crate) fn native_tm_put_if_absent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key0 = args.get(1).copied().unwrap_or(Value::Object(None));
    let value0 = args.get(2).copied().unwrap_or(Value::Object(None));
    let (this_synced, key, value) = tm_sync_with_args2(ctx, this, key0, value0)?;
    this = this_synced;
    // W7-1 family 1. Unlike `computeIfAbsent`/`merge` — which already route
    // through `native_tm_get` + `native_tm_put` and so inherit the view
    // redirect for free — this native writes the backing array itself, so a
    // `put` through a view would land in the view's CACHE and be discarded by
    // the next rebuild. `get` + `put` is the JDK's own default body for
    // `putIfAbsent`, and both halves are already view-aware.
    if tm_view_spec(ctx, this).is_some() {
        let this_pin = ctx.pin_native_root(this);
        let key_pin = pin_value(ctx, key);
        let value_pin = pin_value(ctx, value);
        let existing = match native_tm_get(ctx, &[Value::Object(Some(this)), key]) {
            Ok(v) => v.unwrap_or(Value::Object(None)),
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };
        if !matches!(existing, Value::Object(None)) {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(existing));
        }
        let this = ctx.read_native_pin(this_pin, this);
        let key = read_pinned_elem(ctx, key_pin, key);
        let value = read_pinned_elem(ctx, value_pin, value);
        // Registered-only, and nothing but the unpin follows: the reclaiming
        // door (gc-common w11-c).
        let result = native_tm_put_entry(ctx, &[Value::Object(Some(this)), key, value]);
        ctx.unpin_native_roots(this_pin);
        return result;
    }
    // Fast-mode eligibility check mirrors `native_tm_put`.
    if tm_has_no_comparator(ctx, this) && !tm_force_array_mode(ctx, this) {
        if let Some(tk) = tree_key_from_value(ctx, &key) {
            let already_in_array = matches!(tm_get_slot(ctx, this, TM_FIELD_SIZE), Value::Int(n) if n > 0)
                && !tm_is_fast_mode(ctx, this);
            if !already_in_array {
                let (existing, new_size) = tm_fast_with(ctx, this, |bt| {
                    use std::collections::btree_map::Entry;
                    match bt.entry(tk) {
                        Entry::Occupied(o) => (Some(*o.get()), bt.len() as i32),
                        Entry::Vacant(v) => {
                            v.insert(value);
                            (None, bt.len() as i32)
                        }
                    }
                });
                tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(new_size));
                return Ok(Some(existing.unwrap_or(Value::Object(None))));
            }
        } else {
            tm_set_force_array(ctx, this);
            if tm_is_fast_mode(ctx, this) {
                tm_migrate_fast_to_array(ctx, &mut this);
            }
        }
    }
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => {
            let (refreshed, buf) = tm_install_backing_array(ctx, this, TM_DEFAULT_CAPACITY * 2);
            this = refreshed;
            buf
        }
    };
    // Family-1 stale-ObjectRef fix: use tm_binary_search's refreshed
    // `this`/`data`/`key` (shadowed) rather than the pre-search copies —
    // `key` is reused below (tm_insert_at) on a miss, and its own
    // staleness (not just `this`/`data`) was the actual root cause of the
    // residual panics that survived the earlier fix. `value` is pinned
    // separately across `tm_ensure_capacity` since it isn't threaded
    // through the search.
    let (search, this, data, key) = tm_binary_search(ctx, this, data, size, &comparator, &key)?;
    match search {
        Ok(idx) => {
            let existing = ctx.get_array_element(data, idx * 2 + 1);
            Ok(Some(existing))
        }
        Err(pos) => {
            // gc-common w11-c: `this` rides the frame too. It was a bare copy
            // across the grow and then took the `size` store below, while
            // `tm_ensure_capacity` only refreshed its OWN copy. Registered-only
            // (`putIfAbsent`), so the grow may collect and retry
            // (`GrowAlloc::Reclaim`): `this`, `key` and `value` are everything
            // used afterwards, and `data` comes back fresh.
            let this_pin = ctx.pin_native_root(this);
            let key_pin = pin_value(ctx, key);
            let value_pin = pin_value(ctx, value);
            let data = tm_ensure_capacity(ctx, this, size, data, GrowAlloc::Reclaim);
            let this = ctx.read_native_pin(this_pin, this);
            let key = read_pinned_elem(ctx, key_pin, key);
            let value = read_pinned_elem(ctx, value_pin, value);
            ctx.unpin_native_roots(this_pin);
            tm_insert_at(ctx, data, size, pos, key, value);
            tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(size + 1));
            Ok(Some(Value::Object(None)))
        }
    }
}

pub(crate) fn native_tm_put_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // cceres5: `tm_materialize_deser_array` can allocate (materializing a
    // deserialized backing array), which would leave the raw `args` source —
    // and `this` itself — stale for everything below. Pin both across it.
    let source0 = args.get(1).copied().unwrap_or(Value::Object(None));
    let source_base = pin_value(ctx, source0);
    let this_pin_early = ctx.pin_native_root(this);
    // A failed sync releases this frame's pins (round 15 wave 4, lane compat4).
    let synced = tm_sync_native_state(ctx, this);
    let sync_base = if source_base == usize::MAX {
        this_pin_early
    } else {
        source_base
    };
    let this = unpin_on_err(ctx, sync_base, synced)?;
    let this = ctx.read_native_pin(this_pin_early, this);
    let source = match read_pinned_elem(ctx, source_base, source0) {
        Value::Object(Some(r)) => r,
        _ => {
            ctx.unpin_native_roots(if source_base == usize::MAX {
                this_pin_early
            } else {
                source_base
            });
            return Ok(None);
        }
    };
    // Collect entries from the source map, dispatching on its concrete backend
    // (TreeMap tree, HashMap/CHM buckets, LinkedHashMap overlay, or — via the
    // `entrySet().iterator()` fallback — any other `Map`). The previous version
    // hand-decoded the source's raw slots assuming a specific HashMap/TreeMap
    // field layout (field 1 = size, field 2 = capacity, node field 3 = next);
    // those indices did not match CratonVM's actual HashMap/Node layout, so
    // `TreeMap.putAll(hashMap)` and `new TreeMap<>(map)` (which the JDK ctor
    // implements via `putAll`) silently copied nothing — the second half of the
    // Elasticsearch `Settings.Builder.put(Settings)` failure (its builder map is
    // a TreeMap, so `output.map.putAll(settingsMap)` lands here).
    //
    // Every exit below releases from the lowest pin this frame took.
    let frame_base = sync_base;
    // `AbstractMap.putAll` asks `put` virtually: a program's subclass
    // receiver gets `invoke_virtual` per entry (`CRATONVM_COMPAT_PUT_ALL_VIRTUAL_PUT`).
    // A `SortedMap` source may take `buildFromSorted` instead, which asks no
    // `put`; it keeps the native put.
    let virtual_put = put_all_virtual_put_enabled()
        && class_is_programs(&*ctx, this)
        && !object_is_sorted_map(&*ctx, source);
    let put_one = |ctx: &mut dyn NativeContext,
                   this: ObjectRef,
                   key: Value,
                   value: Value|
     -> MethodCallResult {
        if virtual_put {
            map_put_virtual(ctx, this, key, value)
        } else {
            native_tm_put(ctx, &[Value::Object(Some(this)), key, value])
        }
    };
    // C3W3-2 (`CRATONVM_COMPAT_MAP_COPY_JDK_QUESTIONS`, round 15 wave 4, lane
    // compat4): a program's map that is not a `SortedMap` is asked
    // `TreeMap.putAll`'s `map.size()`, then `AbstractMap.putAll`'s walk, each
    // `put` (its comparisons) interleaved. A `SortedMap` source keeps the old
    // path: `buildFromSorted` asks a different sequence.
    if let Some(prog) = foreign_map_source(&*ctx, source) {
        if !object_is_sorted_map(&*ctx, source) && !object_is_sorted_map(&*ctx, prog) {
            let this_pin = ctx.pin_native_root(this);
            let prog_pin = ctx.pin_native_root(prog);
            let walked =
                map_for_each_entry(ctx, (prog_pin, prog), MapSizeAsk::Ask, |ctx, key, value| {
                    let this_now = ctx.read_native_pin(this_pin, this);
                    put_one(ctx, this_now, key, value)?;
                    Ok(())
                });
            ctx.unpin_native_roots(frame_base);
            return walked.map(|()| None);
        }
        // A program's `SortedMap` (round 15 wave 6, lane sweepC): the
        // `buildFromSorted` questions, `tm_put_all_from_sorted_program_map`.
        if tm_sorted_source_questions_enabled() && object_is_sorted_map(&*ctx, source) {
            let this_pin = ctx.pin_native_root(this);
            let prog_pin = ctx.pin_native_root(prog);
            let walked =
                tm_put_all_from_sorted_program_map(ctx, (this_pin, this), (prog_pin, prog));
            ctx.unpin_native_roots(frame_base);
            return walked.map(|()| None);
        }
    }
    let collected = collect_entries_any(ctx, source);
    let pairs = unpin_on_err(ctx, frame_base, collected)?;
    // Family-1 stale-ObjectRef fix: each `native_tm_put` call below can run a
    // user Comparator/lambda and trigger a moving GC. `this` was a bare local
    // reused across every iteration, and `pairs` (keys/values collected
    // up front) sat entirely unpinned in a Rust `Vec` across the whole loop —
    // any GC triggered by an early iteration would leave `this` and every
    // not-yet-consumed key/value stale for later iterations. Pin all of them
    // before the loop starts and refresh on every iteration.
    let keys: Vec<Value> = pairs.iter().map(|(k, _)| *k).collect();
    let vals: Vec<Value> = pairs.iter().map(|(_, v)| *v).collect();
    // `this_pin` is pushed first, so it is the earliest (lowest) handle in
    // this function's pin scope; unpinning from it unwinds everything pushed
    // afterward (the key/value slices) too — no need to track their bases
    // separately.
    let this_pin = ctx.pin_native_root(this);
    let (_, key_pins) = pin_value_slice(ctx, &keys);
    let (_, val_pins) = pin_value_slice(ctx, &vals);
    for i in 0..pairs.len() {
        let this_cur = ctx.read_native_pin(this_pin, this);
        let key = read_pinned_elem(ctx, key_pins[i], keys[i]);
        let val = read_pinned_elem(ctx, val_pins[i], vals[i]);
        if let Err(e) = put_one(ctx, this_cur, key, val) {
            ctx.unpin_native_roots(if source_base == usize::MAX {
                this_pin_early
            } else {
                source_base
            });
            return Err(e);
        }
    }
    ctx.unpin_native_roots(if source_base == usize::MAX {
        this_pin_early
    } else {
        source_base
    });
    Ok(None)
}

pub(crate) fn native_tm_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    tm_refresh_real_mirrors(ctx, args);
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let this = tm_sync_native_state(ctx, this)?;
    let pairs = tm_collect_pairs(ctx, this);
    // Family-1 stale-ObjectRef fix (2026-07-31): `obj_to_display_units`
    // dispatches each element's real `toString()`, which allocates — every
    // not-yet-rendered entry can move under it.
    let pinned_pairs = PinnedPairs::new(ctx, &pairs);
    let mut buf = units_of("{");
    for i in 0..pinned_pairs.len() {
        if i > 0 {
            push_lit(&mut buf, ", ");
        }
        let (k, _) = pinned_pairs.get(&*ctx, i);
        let ku = obj_to_display_units(ctx, &k)?;
        buf.extend_from_slice(&ku);
        buf.push(u16::from(b'='));
        let (_, v) = pinned_pairs.get(&*ctx, i);
        let vu = obj_to_display_units(ctx, &v)?;
        buf.extend_from_slice(&vu);
    }
    ctx.unpin_native_roots(pinned_pairs.base());
    buf.push(u16::from(b'}'));
    let s = ctx.create_string_from_units(&buf);
    Ok(Some(Value::Object(Some(s))))
}

pub(crate) fn native_tm_comparator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let this = tm_sync_native_state(ctx, this)?;
    Ok(Some(tm_get_slot(ctx, this, TM_FIELD_COMPARATOR)))
}

// The six range-view natives below, plus `descendingMap`/`descendingKeySet`,
// are one call each into `tm_new_range_view` — see the "Navigable views" block
// above `TmViewSpec` for what changed and why.
//
// What they used to be: each allocated a second TreeMap and replayed the
// matching entries into it with `native_tm_put`, producing a detached SNAPSHOT.
// The comment that stood here recorded the *previous* defect in the same
// functions — `tm_state()` reading a fast-mode map's never-written array and so
// answering `size` phantom nulls, which made `headMap` compare `null` on the
// first entry and `break` (spuriously empty) and made `tailMap` `put(null, ..)`
// repeatedly. `tm_collect_pairs` fixed that, and it is still what the resync
// reads with, so the fast/array-mode hazard stays closed.

// headMap: entries with keys strictly < toKey
pub(crate) fn native_tm_head_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let to_key = args.get(1).copied().unwrap_or(Value::Object(None));
    tm_new_range_view(ctx, this, None, Some((to_key, false)), false)
}

// tailMap: entries with keys >= fromKey
pub(crate) fn native_tm_tail_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let from_key = args.get(1).copied().unwrap_or(Value::Object(None));
    tm_new_range_view(ctx, this, Some((from_key, true)), None, false)
}

// subMap: entries with keys >= fromKey and < toKey
/// `NavigableSubMap`'s constructor, first three lines:
///
/// ```text
/// if (!fromStart && !toEnd) {
///     if (m.compare(lo, hi) > 0)
///         throw new IllegalArgumentException("fromKey > toKey");
/// }
/// ```
///
/// `TreeMap.subMapReversedBounds` measured `tm.subMap("d", "a")` returning an
/// empty view here against HotSpot's `IllegalArgumentException`. An empty view
/// is the failure mode that reads as a pass everywhere a caller only iterates —
/// W7-1-treemap-views-and-iterator-remove-contract is a whole record about that
/// shape — and a caller that transposed two bounds gets no signal at all.
///
/// Only the two-bound entry points reach this. `headMap`/`tailMap` take the
/// `else` arm of that `if`, which is a type check on the single bound and not
/// an ordering check, so they are deliberately not routed here.
///
/// The ordering asked is the RECEIVER's, not the source map's: for
/// `descendingMap().subMap(a, b)` the receiver is the descending view, whose
/// comparator slot holds the `Collections.reverseOrder` wrapper, and the JDK's
/// `m.compare` on a `DescendingSubMap` is that same reversed ordering. Reading
/// the slot rather than the backing map's is what makes composition agree.
///
/// GC-SAFETY: `tree_compare` dispatches a user `Comparator` (or a
/// `Comparable.compareTo` override), a full interpreted call that can move
/// `this` and both bound keys — all three are used afterwards, and the two keys
/// end up STORED in the view's side-table entry, where a from-space address
/// would outlive the collection that produced it. Pin, compare, re-read.
pub(crate) fn tm_refuse_reversed_bounds(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    from_key: Value,
    to_key: Value,
) -> Result<(ObjectRef, Value, Value), MethodCallFailed> {
    let comparator = tm_get_slot(ctx, this, TM_FIELD_COMPARATOR);
    let this_pin = ctx.pin_native_root(this);
    let from_pin = pin_value(ctx, from_key);
    let to_pin = pin_value(ctx, to_key);
    let verdict = tree_compare(ctx, &comparator, from_key, to_key);
    let this = ctx.read_native_pin(this_pin, this);
    let from_key = read_pinned_elem(ctx, from_pin, from_key);
    let to_key = read_pinned_elem(ctx, to_pin, to_key);
    ctx.unpin_native_roots(this_pin);
    if verdict? > 0 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "fromKey > toKey".to_string(),
            }
            .into(),
        );
    }
    Ok((this, from_key, to_key))
}

pub(crate) fn native_tm_sub_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let from_key = args.get(1).copied().unwrap_or(Value::Object(None));
    let to_key = args.get(2).copied().unwrap_or(Value::Object(None));
    let (this, from_key, to_key) = tm_refuse_reversed_bounds(ctx, this, from_key, to_key)?;
    tm_new_range_view(
        ctx,
        this,
        Some((from_key, true)),
        Some((to_key, false)),
        false,
    )
}

// --- NavigableMap range views (`headMap`/`tailMap`/`subMap` with inclusive
// flags). Real JDK `TreeMap.headMap(K,boolean)`/`tailMap(K,boolean)`/
// `subMap(K,boolean,K,boolean)` bytecode builds an `AscendingSubMap` that
// navigates the *real* `root`/`comparator` fields -- but a natively-managed
// TreeMap keeps its entries in the `tm_fast_table`/`tm_array_table`
// side-tables (see the fast/array-mode doc block above `TM_FIELD_DATA`) and
// never populates the real red-black tree, so that bytecode sees an
// always-null `root` and the resulting view is unconditionally empty.
// `descendingMap` / `descendingKeySet` had no native at all and so were exactly
// that: the real bytecode built a `DescendingSubMap` over a null `root` and
// answered `{}` / `[]`, which is the record's first two rows.
pub(crate) fn native_tm_head_map_inclusive(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let to_key = args.get(1).copied().unwrap_or(Value::Object(None));
    let inclusive = arg_bool(args, 2);
    tm_new_range_view(ctx, this, None, Some((to_key, inclusive)), false)
}

pub(crate) fn native_tm_tail_map_inclusive(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let from_key = args.get(1).copied().unwrap_or(Value::Object(None));
    let inclusive = arg_bool(args, 2);
    tm_new_range_view(ctx, this, Some((from_key, inclusive)), None, false)
}

pub(crate) fn native_tm_sub_map_inclusive(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let from_key = args.get(1).copied().unwrap_or(Value::Object(None));
    let from_inclusive = arg_bool(args, 2);
    let to_key = args.get(3).copied().unwrap_or(Value::Object(None));
    let to_inclusive = arg_bool(args, 4);
    // The JDK's check is on the bounds alone and ignores the inclusive flags —
    // `subMap(k, false, k, false)` is a legal empty range, `subMap(hi, .., lo, ..)`
    // is not. See `tm_refuse_reversed_bounds`.
    let (this, from_key, to_key) = tm_refuse_reversed_bounds(ctx, this, from_key, to_key)?;
    tm_new_range_view(
        ctx,
        this,
        Some((from_key, from_inclusive)),
        Some((to_key, to_inclusive)),
        false,
    )
}

/// `TreeMap.descendingMap()` — a live NavigableMap over the same entries in the
/// reverse of this map's ordering.
///
/// W7-1 family 1: there was no native for this at all, so the call reached the
/// real `TreeMap.descendingMap()` bytecode, which builds a `DescendingSubMap`
/// over the `root` field — and a natively-managed TreeMap never populates
/// `root`, so the answer was `{}` where HotSpot has `{d=4, c=3, b=2, a=1}`. An
/// unbounded descending view is the whole implementation.
pub(crate) fn native_tm_descending_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    tm_new_range_view(ctx, this, None, None, true)
}

/// `TreeMap.descendingKeySet()` — the JDK's own definition is
/// `descendingMap().navigableKeySet()`, and that is what this is: the descending
/// view, then its `keySet`, which `native_tm_key_set` already builds as a live
/// TreeSet carrying the source map in its trailing array slot. So removing
/// through the returned set reaches the descending view, and the view redirects
/// it to the backing map.
pub(crate) fn native_tm_descending_key_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let view = match native_tm_descending_map(ctx, args)? {
        Some(v @ Value::Object(Some(_))) => v,
        other => return Ok(other),
    };
    native_tm_key_set(ctx, &[view])
}

/// `TreeMap(Map)` — a fresh natural-ordered map, then `putAll`.
pub(crate) fn native_tm_init_from_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // RULE C: the JDK's body is `putAll(m)`, which reads `m.size()`.
    // NOT `reject_null_collection`: that is the
    // `Objects.requireNonNull(c)` shape, whose NPE carries no message. This
    // constructor DEREFERENCES its argument (`map.size()`), so the JDK raises a
    // helpful NPE naming the method and its own parameter name.
    if matches!(args.get(1), Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException {
            message: Some(
                "Cannot invoke \"java.util.Map.size()\" because \"map\" is null".to_string(),
            ),
        }
        .into());
    }
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let src = args.get(1).copied().unwrap_or(Value::Object(None));
    let this_pin = ctx.pin_native_root(this);
    let src_pin = pin_value(ctx, src);
    let this_cur = ctx.read_native_pin(this_pin, this);
    let r = native_tm_init(ctx, &[Value::Object(Some(this_cur))]);
    if let Err(e) = r {
        ctx.unpin_native_roots(this_pin);
        return Err(e);
    }
    let this_cur = ctx.read_native_pin(this_pin, this);
    let src_cur = read_pinned_elem(ctx, src_pin, src);
    // `comparator = null; putAll(m);` -- VIRTUAL, so a program subclass's
    // `putAll` override runs during `super(m)` ([`copy_ctor_bulk_override`]).
    let r = if copy_ctor_bulk_override(&*ctx, this_cur, "putAll", "(Ljava/util/Map;)V") {
        ctx.invoke_virtual(this_cur, "putAll", "(Ljava/util/Map;)V", &[src_cur])
    } else {
        native_tm_put_all(ctx, &[Value::Object(Some(this_cur)), src_cur])
    };
    ctx.unpin_native_roots(this_pin);
    r?;
    Ok(None)
}

/// `TreeMap(SortedMap)` — the same, and it INHERITS the source's comparator.
///
/// That is the whole difference between the two constructors, and it is
/// decided by the argument's STATIC type: `new TreeMap<>((Map) sortedMap)`
/// deliberately does not inherit it. Both spellings are in
/// `apps/probes/TreeShadowSweep`.
pub(crate) fn native_tm_init_from_sorted_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `comparator = m.comparator();` (MEASURED, `R15Compat5NullArgMore` row 9).
    reject_null_collection_asked(
        &*ctx,
        args.get(1),
        NULL_ARG_TM_INIT_SORTED,
        NULL_M_COMPARATOR,
    )?;
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let src = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let this_pin = ctx.pin_native_root(this);
    let src_pin = ctx.pin_native_root(src);
    let src_cur = ctx.read_native_pin(src_pin, src);
    let asked = ctx.invoke_virtual(src_cur, "comparator", "()Ljava/util/Comparator;", &[]);
    // `comparator = m.comparator();` propagates what it throws
    // (`CRATONVM_COMPAT_MAP_COPY_JDK_QUESTIONS`, round 15 wave 4, lane
    // compat4): the `unwrap_or(None)` read a throwing `comparator()` of a
    // program's `SortedMap` as natural ordering and copied on.
    let asked = if map_copy_jdk_questions_enabled() {
        unpin_on_err(ctx, this_pin, asked)?
    } else {
        asked.unwrap_or(None)
    };
    let cmp = asked.unwrap_or(Value::Object(None));
    let cmp_pin = pin_value(ctx, cmp);
    let this_cur = ctx.read_native_pin(this_pin, this);
    let cmp_cur = read_pinned_elem(ctx, cmp_pin, cmp);
    let r = native_tm_init_comparator(ctx, &[Value::Object(Some(this_cur)), cmp_cur]);
    if let Err(e) = r {
        ctx.unpin_native_roots(this_pin);
        return Err(e);
    }
    let this_cur = ctx.read_native_pin(this_pin, this);
    let src_cur = ctx.read_native_pin(src_pin, src);
    // A program's `SortedMap` (round 15 wave 6, lane sweepC): the rest of the
    // constructor is `buildFromSorted(m.size(), m.entrySet().iterator(), ..)`
    // -- one `size()`, then the walk; `putAll` would ask `size()` and
    // `comparator()` again.
    if tm_sorted_source_questions_enabled() {
        if let Some(prog) = foreign_map_source(&*ctx, src_cur) {
            let prog_pin = ctx.pin_native_root(prog);
            let walked = (|| -> Result<(), MethodCallFailed> {
                let prog_now = ctx.read_native_pin(prog_pin, prog);
                let map_size = match ctx.invoke_virtual(prog_now, "size", "()I", &[])? {
                    Some(Value::Int(n)) => n,
                    _ => 0,
                };
                tm_build_from_sorted_walk(
                    ctx,
                    (this_pin, this),
                    (prog_pin, prog),
                    map_size,
                    "java.util.SortedMap",
                )
            })();
            ctx.unpin_native_roots(this_pin);
            walked?;
            return Ok(None);
        }
    }
    let r = native_tm_put_all(
        ctx,
        &[Value::Object(Some(this_cur)), Value::Object(Some(src_cur))],
    );
    ctx.unpin_native_roots(this_pin);
    r?;
    Ok(None)
}

pub(crate) fn native_tm_compute_if_absent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // RULE F, ahead of the key. MEASURED: `TreeMap.computeIfAbsent(null, null)`
    // and `(null, F)` are BOTH message-less here — unlike `Hashtable`, whose
    // key arm carries text — so the ordering is unobservable for this receiver
    // and the check is placed first for consistency with the family.
    reject_null_functional(args.get(2))?;
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let mapper = match args.get(2) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let mapper_pin = ctx.pin_native_root(mapper);
    // Round-9 HIGH: route through `native_tm_get` / `native_tm_put` so the
    // fast-mode BTreeMap path applies for natural-ordering maps.
    let this_cur = ctx.read_native_pin(this_pin, this);
    let key_cur = read_pinned_elem(ctx, key_pin, key);
    let existing = match native_tm_get(ctx, &[Value::Object(Some(this_cur)), key_cur]) {
        Ok(v) => v.unwrap_or(Value::Object(None)),
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    if !matches!(existing, Value::Object(None)) {
        ctx.unpin_native_roots(this_pin);
        return Ok(Some(existing));
    }
    // THE CALLBACK BOUNDARY. A mapping function that structurally modifies the
    // same map is a `ConcurrentModificationException` -- the native decides the
    // key is absent BEFORE calling the mapper and writes its result AFTER, so
    // if the mapper resized or rewrote the map in between, that write lands
    // against a decision taken on a map that no longer exists. Same defect
    // `HashMap` had (fixed 2026-08-28), at the sibling that shares the
    // contract. MEASURED no-throw (apps/probes/TreeShadowSweep 214).
    //
    // `size` is the signal here, not `modCount`: this family's state lives in
    // an array-backed overlay whose real `modCount` slot no native writes, so
    // `read_map_mod_count` would answer for a field nothing maintains. A size
    // change is unambiguously structural; an add paired with a remove is the
    // case it cannot see, and answering nothing there is the same trade
    // `al_mod_count_slot` already documents.
    let this_cur = ctx.read_native_pin(this_pin, this);
    let size_before = match tm_get_slot(&*ctx, this_cur, TM_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    let mapper_cur = ctx.read_native_pin(mapper_pin, mapper);
    let key_cur = read_pinned_elem(ctx, key_pin, key);
    let result = match ctx.invoke_virtual(
        mapper_cur,
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[key_cur],
    ) {
        Ok(r) => r,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    // The comodification half of the callback boundary, taken BEFORE the write
    // and before the null-result short circuit -- the JDK checks
    // `if (modCount != mc) throw new ConcurrentModificationException();`
    // regardless of what the mapper answered.
    let this_cur = ctx.read_native_pin(this_pin, this);
    let size_after = match tm_get_slot(&*ctx, this_cur, TM_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if size_after != size_before {
        ctx.unpin_native_roots(this_pin);
        return Err(cratonvm_types::error::RuntimeError::ConcurrentModificationException.into());
    }
    let val = result.unwrap_or(Value::Object(None));
    if matches!(val, Value::Object(None)) {
        ctx.unpin_native_roots(this_pin);
        return Ok(Some(Value::Object(None)));
    }
    let val_pin = pin_value(ctx, val);
    let this_cur = ctx.read_native_pin(this_pin, this);
    let key_cur = read_pinned_elem(ctx, key_pin, key);
    let val_cur = read_pinned_elem(ctx, val_pin, val);
    if let Err(e) = native_tm_put(ctx, &[Value::Object(Some(this_cur)), key_cur, val_cur]) {
        ctx.unpin_native_roots(this_pin);
        return Err(e);
    }
    let ret = read_pinned_elem(ctx, val_pin, val);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(ret))
}

pub(crate) fn native_tm_merge(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // RULE F. MEASURED `TreeMap.merge(k, v, null)` -> bare NPE, present or
    // absent key. The null-VALUE arm (`merge(k, null, f)`, also a bare NPE on
    // `TreeMap`) is NOT added here: `native_map_merge` refuses it for the
    // `HashMap` family, but `Hashtable.merge(k, null, f)` SUCCEEDS on the
    // oracle (G1-1 §5), so the value rule is per-receiver and this lane does
    // not widen it from one row. See `reject_null_functional`.
    reject_null_functional(args.get(3))?;
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    let remap_fn = match args.get(3) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let value_pin = pin_value(ctx, value);
    let remap_pin = ctx.pin_native_root(remap_fn);
    // Round-9 HIGH: route through `native_tm_get` / `native_tm_put` so the
    // fast-mode BTreeMap path applies for natural-ordering maps.
    let this_cur = ctx.read_native_pin(this_pin, this);
    let key_cur = read_pinned_elem(ctx, key_pin, key);
    let existing = match native_tm_get(ctx, &[Value::Object(Some(this_cur)), key_cur]) {
        Ok(v) => v.unwrap_or(Value::Object(None)),
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let existing_pin = pin_value(ctx, existing);
    let new_val = if matches!(existing, Value::Object(None)) {
        read_pinned_elem(ctx, value_pin, value)
    } else {
        let remap_cur = ctx.read_native_pin(remap_pin, remap_fn);
        let existing_cur = read_pinned_elem(ctx, existing_pin, existing);
        let value_cur = read_pinned_elem(ctx, value_pin, value);
        let merged = match ctx.invoke_virtual(
            remap_cur,
            "apply",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[existing_cur, value_cur],
        ) {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };
        merged.unwrap_or(Value::Object(None))
    };
    if matches!(new_val, Value::Object(None)) {
        // JDK Map.merge contract: null result removes the mapping.
        let this_cur = ctx.read_native_pin(this_pin, this);
        let key_cur = read_pinned_elem(ctx, key_pin, key);
        if let Err(e) = native_tm_remove(ctx, &[Value::Object(Some(this_cur)), key_cur]) {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
        ctx.unpin_native_roots(this_pin);
        return Ok(Some(Value::Object(None)));
    }
    let new_val_pin = pin_value(ctx, new_val);
    let this_cur = ctx.read_native_pin(this_pin, this);
    let key_cur = read_pinned_elem(ctx, key_pin, key);
    let new_val_cur = read_pinned_elem(ctx, new_val_pin, new_val);
    if let Err(e) = native_tm_put(ctx, &[Value::Object(Some(this_cur)), key_cur, new_val_cur]) {
        ctx.unpin_native_roots(this_pin);
        return Err(e);
    }
    let ret = read_pinned_elem(ctx, new_val_pin, new_val);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(ret))
}

// TreeMap key iterator: snapshot-based, returns keys in sorted order
pub(crate) fn native_tm_key_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Both fast and array modes go through the shared snapshot helper so
    // the iterator sees sorted-order keys regardless of backing store.
    // W7-1 family 1: this was the one content native that did NOT open with the
    // state funnel, so a navigable view would have been iterated from whatever
    // its cache last held.
    let this = tm_sync_native_state(ctx, this)?;
    let pairs = tm_collect_pairs(ctx, this);
    // Family-1 stale-ObjectRef fix (2026-07-31): both allocations below can
    // collect and move the snapshotted keys (and the snapshot array itself).
    let pinned_pairs = PinnedPairs::new(ctx, &pairs);
    let snap = alloc_ref_array(ctx, pairs.len().max(1));
    let snap_pin = ctx.pin_native_root(snap);
    for i in 0..pinned_pairs.len() {
        let (k, _) = pinned_pairs.get(&*ctx, i);
        ctx.set_array_element(snap, i, k);
    }
    let itr = try_alloc_synthetic(ctx, "java/util/TreeMap$KeyItr", 2)?;
    let snap = ctx.read_native_pin(snap_pin, snap);
    ctx.set_field(itr, 0, Value::Object(Some(snap)));
    ctx.set_field(itr, 1, Value::Int(0));
    Ok(Some(Value::Object(Some(itr))))
}

// ===========================================================================
// TreeSet native methods
// ===========================================================================

pub(crate) fn native_ts_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // The backing array is NOT allocated here. `native_ts_add`'s `data_opt ==
    // None` arm installs it on the FIRST insert, and `ts_get_slot` documents
    // `Value::Object(None)` as the layout-independent default for a set with no
    // side-table entry -- so the eager allocation this constructor used to do
    // bought nothing but a `TS_DEFAULT_CAPACITY` array on every `new
    // TreeSet()`, retained for as long as the set was.
    //
    // MEASURED (probes/CollectionShapeCause.java, retained heap per empty
    // instance, Temurin 25.0.3+9): `TreeSet` 208 B against HotSpot's 64.1, of
    // which 144 B is exactly this `Object[16]`. HotSpot allocates nothing until
    // the first `add` -- its backing `TreeMap.root` stays null -- and an empty set that is never
    // written is the common shape in configuration and model code.
    ih_seed(ctx, this);
    ts_set_slot(ctx, this, TS_FIELD_DATA, Value::Object(None));
    ts_set_slot(ctx, this, TS_FIELD_SIZE, Value::Int(0));
    ts_set_slot(ctx, this, TS_FIELD_COMPARATOR, Value::Object(None));
    Ok(None)
}

pub(crate) fn native_ts_init_comparator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // See `native_ts_init`: the backing array is installed on first `add`.
    // Nothing here allocates any more, so `cmp` needs no pin across one.
    ih_seed(ctx, this);
    let cmp = args.get(1).copied().unwrap_or(Value::Object(None));
    ts_set_slot(ctx, this, TS_FIELD_DATA, Value::Object(None));
    ts_set_slot(ctx, this, TS_FIELD_SIZE, Value::Int(0));
    ts_set_slot(ctx, this, TS_FIELD_COMPARATOR, cmp);
    Ok(None)
}

pub(crate) fn native_ts_init_collection(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // RULE C: `TreeSet(Collection)` is `addAll(c)` on an empty set, which
    // reads `c.size()` (MEASURED, `R15Compat5NullArgMore` row 1).
    reject_null_collection_deref(args.get(1), NULL_C_SIZE)?;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    ih_seed(ctx, this);
    let source = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let (this, _buf) = ts_install_backing_array(ctx, this, TS_DEFAULT_CAPACITY);
            ts_set_slot(ctx, this, TS_FIELD_SIZE, Value::Int(0));
            ts_set_slot(ctx, this, TS_FIELD_COMPARATOR, Value::Object(None));
            return Ok(None);
        }
    };
    let source_pin = ctx.pin_native_root(source);
    let (this, _buf) = ts_install_backing_array(ctx, this, TS_DEFAULT_CAPACITY);
    let source = ctx.read_native_pin(source_pin, source);
    ctx.unpin_native_roots(source_pin);
    ts_set_slot(ctx, this, TS_FIELD_SIZE, Value::Int(0));
    ts_set_slot(ctx, this, TS_FIELD_COMPARATOR, Value::Object(None));
    // `this(); addAll(c);`: a program subclass's `addAll` runs during
    // `super(c)` ([`copy_ctor_bulk_override`]). Nothing above collects.
    if copy_ctor_bulk_override(&*ctx, this, "addAll", "(Ljava/util/Collection;)Z") {
        return ctx
            .invoke_virtual(
                this,
                "addAll",
                "(Ljava/util/Collection;)Z",
                &[Value::Object(Some(source))],
            )
            .map(|_| None);
    }
    // Add elements from source. Use the `_or_real` collector so a foreign /
    // real-bytecode source whose layout the heuristics can't read (e.g. a
    // `Collections.unmodifiableMap(LinkedCaseInsensitiveMap).keySet()` view)
    // is still drained via its real `toArray()` — otherwise `new TreeSet<>(
    // someView)` comes out empty (mirrors `native_hs_add_all`).
    //
    // Round 15 wave 4, lane compat5: pinned BEFORE the walk (a foreign
    // source's `toArray()` is a GC point; pinning afterwards pinned a moved
    // address), and every throw releases the frame -- see `native_ts_add_all`.
    let this_pin = ctx.pin_native_root(this);
    // JIT round 16 (lane compatdrain), r15w4-compat5 item 7: `addAll(c)` on
    // the empty set takes the `buildFromSorted` arm for a `SortedSet` `c` in
    // natural order.
    let source_pin = ctx.pin_native_root(source);
    let built = ts_add_all_build_from_sorted(ctx, (this_pin, this), (source_pin, source));
    if unpin_on_err(ctx, this_pin, built)? {
        ctx.unpin_native_roots(this_pin);
        return Ok(None);
    }
    let source = ctx.read_native_pin(source_pin, source);
    let collected = collect_collection_elements_or_real(ctx, source);
    let elems = unpin_on_err(ctx, this_pin, collected)?;
    // Family-1 stale-ObjectRef fix: same hazard as `native_ts_add_all` —
    // each `native_ts_add` call can trigger a moving GC, so `this` and the
    // up-front-collected `elems` must be pinned across the whole loop.
    let (_, elem_pins) = pin_value_slice(ctx, &elems);
    for i in 0..elems.len() {
        let this_cur = ctx.read_native_pin(this_pin, this);
        let e = read_pinned_elem(ctx, elem_pins[i], elems[i]);
        let added = native_ts_add(ctx, &[Value::Object(Some(this_cur)), e]);
        unpin_on_err(ctx, this_pin, added)?;
    }
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

/// `Set.add` / `Set.addAll` on a MAP KEY-SET VIEW: always
/// `UnsupportedOperationException`.
///
/// A keySet view has no value to associate with a new key, so the JDK's
/// `AbstractCollection.add` default stands and every map's keySet refuses.
/// CratonVM mirrors the `TreeSet` native surface onto
/// `java/util/TreeMap$KeySet` because the view is a TreeSet-SHAPED object whose
/// state lives in the same side-table -- correct for every method except this
/// one pair, where the two contracts are opposites.
pub(crate) fn native_view_add_unsupported(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // EMPTY, which `RuntimeError::to_java` turns into a null `getMessage()` by
    // calling the no-arg ctor. HotSpot's is message-less -- the throw comes from
    // `AbstractCollection.add`, which is `throw new
    // UnsupportedOperationException()` -- and every other map's keySet in this
    // crate already answers `msg=null`. Only `TreeMap$KeySet` reached this
    // registration and carried an invented message with it.
    // MEASURED, `apps/probes/ViewIdentityProbe` rows 149-150.
    Err(RuntimeError::UnsupportedOperationException {
        message: String::new(),
    }
    .into())
}

/// `TreeMap$KeySet.addAll(c)`. JDK 25 inherits `AbstractCollection.addAll`
/// (`for (E e : c) if (add(e)) modified = true;`), so a null `c` is the
/// helpful NPE of `c.iterator()`, an EMPTY `c` answers `false` without ever
/// reaching `add`, and only a first element reaches `add`'s message-less
/// `UnsupportedOperationException` (after `c`'s `iterator()`, `hasNext()`,
/// `next()`). [`native_view_add_unsupported`] threw the UOE for all three.
/// Under `CRATONVM_COMPAT_BULK_ADD_JDK_QUESTIONS` (`=0` restores the
/// unconditional UOE). Round 15 wave 5, lane compat6.
pub(crate) fn native_tree_key_set_add_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if !bulk_add_jdk_questions_enabled() {
        return native_view_add_unsupported(ctx, args);
    }
    let source = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        Some(Value::Object(None)) => {
            reject_null_collection_deref(args.get(1), NULL_C_ITERATOR)?;
            return Err(bare_npe());
        }
        _ => return native_view_add_unsupported(ctx, args),
    };
    let source_pin = ctx.pin_native_root(source);
    let walked = for_each_by_iterator(ctx, (source_pin, source), |_ctx, _e| {
        Err(RuntimeError::UnsupportedOperationException {
            message: String::new(),
        }
        .into())
    });
    ctx.unpin_native_roots(source_pin);
    walked?;
    Ok(Some(Value::Int(0)))
}

pub(crate) fn native_ts_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let mut elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data_opt, size, comparator) = ts_state(ctx, this);
    // Before the search, because on an EMPTY set the search performs no
    // comparison and so raises nothing — the `TreeSet.incomparableFirstAdd`
    // half. On a non-empty set this fires first and `compare_via_compare_to`
    // (which would raise the same CCE) is never reached, so the two agree by
    // construction rather than by coincidence.
    tree_natural_order_key_check(ctx, &comparator, elem, size == 0)?;
    // A RANGE VIEW REFUSES A KEY OUTSIDE ITS RANGE.
    // `NavigableSubSet.add` is `if (!inRange(e)) throw new
    // IllegalArgumentException("key out of range"); return m.put(e, ..)`. Before
    // this the add landed in the snapshot and nowhere else, so the view reported
    // an element the source had never heard of. MEASURED
    // (apps/probes/TreeShadowSweep 140).
    //
    // The write-through for an IN-range add needs no branch here: the wrapper
    // that installed this range also installed the `ts_view_source` marker, and
    // the propagation below already follows it.
    if tm_view_spec(ctx, this).is_some() {
        let this_pin0 = ctx.pin_native_root(this);
        let elem_h0 = pin_value(ctx, elem);
        let verdict = match tm_view_spec(ctx, this) {
            Some(spec) => tm_view_in_range(ctx, &spec, elem),
            None => Ok(true),
        };
        this = ctx.read_native_pin(this_pin0, this);
        elem = read_pinned_elem(ctx, elem_h0, elem);
        ctx.unpin_native_roots(this_pin0);
        match verdict {
            Ok(true) => {}
            Ok(false) => {
                return Err(
                    cratonvm_types::error::RuntimeError::IllegalArgumentException {
                        message: "key out of range".to_string(),
                    }
                    .into(),
                )
            }
            Err(e) => return Err(e),
        }
    }
    let (data_opt, size, comparator) = ts_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => {
            let elem_pin = pin_value(ctx, elem);
            let (refreshed, buf) = ts_install_backing_array(ctx, this, TS_DEFAULT_CAPACITY);
            elem = read_pinned_elem(ctx, elem_pin, elem);
            ctx.unpin_native_roots(elem_pin);
            this = refreshed;
            buf
        }
    };
    // Family-1 stale-ObjectRef fix: use ts_binary_search's refreshed
    // `this`/`data`/`elem` (the Comparator invocation inside it can trigger
    // a moving GC) rather than the pre-search copies above — `elem` is
    // reused below (ts_insert_at) on a miss.
    let (search, this, data, elem) = ts_binary_search(ctx, this, data, size, &comparator, &elem)?;
    match search {
        Ok(_) => Ok(Some(Value::Int(0))), // already present
        Err(pos) => {
            // `this` is pinned as well as `elem` now: `ts_ensure_capacity`
            // allocates, and both are used after it — `this` by the size store
            // and, below, by the write-through.
            let this_pin = ctx.pin_native_root(this);
            let elem_pin = pin_value(ctx, elem);
            let data = ts_ensure_capacity(ctx, this, size, data);
            let this = ctx.read_native_pin(this_pin, this);
            let elem = read_pinned_elem(ctx, elem_pin, elem);
            ts_insert_at(ctx, data, size, pos, elem);
            ts_set_slot(ctx, this, TS_FIELD_SIZE, Value::Int(size + 1));
            ts_note_pair_insert(&*ctx, this, size, pos);
            // Live `descendingSet()` view: an add through the view is an add to
            // the backing set. `TreeSet.descendingWriteThrough` measured
            // `ts.descendingSet().add(9)` leaving `ts` at `[1, 2, 3]` where
            // HotSpot has `[1, 2, 9]` — the view's own contents were already
            // right, so this is the one direction that was missing.
            //
            // A MAP source is a `TreeMap.keySet()` view, and `keySet().add` is
            // `UnsupportedOperationException` in the JDK, not a write-through.
            // That refusal is a separate, unmeasured row and is deliberately not
            // added here; what matters for this one is that a map source must
            // NOT be handed an add, which is what the predicate says.
            let propagated = match ts_view_source(ctx, this) {
                Some(source) if !is_tree_map_receiver(ctx, source) => ctx
                    .invoke_virtual(source, "add", "(Ljava/lang/Object;)Z", &[elem])
                    .map(|_| ()),
                _ => Ok(()),
            };
            ctx.unpin_native_roots(this_pin);
            propagated?;
            Ok(Some(Value::Int(1)))
        }
    }
}

/// `TreeSet.clone()` — a shallow copy under the RECEIVER's own class.
///
/// The JDK's body is `super.clone()` (so a subclass stays its own class)
/// followed by `clone.m = new TreeMap<>(m)`, i.e. a fresh backing with the same
/// comparator and the same element references. Two properties are load-bearing
/// and neither is decoration:
///
/// * the clone is INDEPENDENT — `clone.add(x)` must not reach the original —
///   so the element array is copied rather than shared;
/// * the clone is NOT a view. A `TreeMap.keySet()` view or a `descendingSet()`
///   carries a source marker in the last capacity slot of its element array
///   (see [`ts_view_source`]); the fresh buffer here is sized so that slot
///   stays null, and a clone therefore writes through to nothing.
pub(crate) fn native_ts_clone(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data_opt, size, comparator) = ts_state(ctx, this);
    // GC-SAFETY: `alloc_object` and `alloc_ref_array` are both collection
    // points, and the receiver, its backing array and the comparator are all
    // bare Rust locals read afterwards. `this_pin` is taken first so one
    // `unpin_native_roots` unwinds the whole group.
    let this_pin = ctx.pin_native_root(this);
    let data_pin = data_opt.map(|d| ctx.pin_native_root(d));
    let comparator_pin = pin_value(ctx, comparator);
    let cid = ctx.class_id_of_object(this);
    let n = ctx.object_num_fields(this);
    let clone = ctx.alloc_object(cid, n);
    let clone_pin = ctx.pin_native_root(clone);
    let cap = std::cmp::max(size as usize, TS_DEFAULT_CAPACITY);
    let buf = alloc_ref_array(ctx, cap);
    let buf_pin = ctx.pin_native_root(buf);
    if let (Some(dp), Some(d)) = (data_pin, data_opt) {
        let d = ctx.read_native_pin(dp, d);
        let buf = ctx.read_native_pin(buf_pin, buf);
        for i in 0..(size as usize) {
            let v = ctx.get_array_element(d, i);
            ctx.set_array_element(buf, i, v);
        }
    }
    let clone = ctx.read_native_pin(clone_pin, clone);
    ih_seed(ctx, clone);
    let clone = ctx.read_native_pin(clone_pin, clone);
    let buf = ctx.read_native_pin(buf_pin, buf);
    ts_set_slot(ctx, clone, TS_FIELD_DATA, Value::Object(Some(buf)));
    let clone = ctx.read_native_pin(clone_pin, clone);
    ts_set_slot(ctx, clone, TS_FIELD_SIZE, Value::Int(size));
    let clone = ctx.read_native_pin(clone_pin, clone);
    let comparator = read_pinned_elem(ctx, comparator_pin, comparator);
    ts_set_slot(ctx, clone, TS_FIELD_COMPARATOR, comparator);
    let clone = ctx.read_native_pin(clone_pin, clone);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(clone))))
}

pub(crate) fn native_ts_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data_opt, size, comparator) = ts_state(ctx, this);
    // `add` and `contains` already ask this; `remove` was the third door and
    // never did, so `naturalOrderedSet.remove(null)` answered `false` -- which
    // tells the caller its element was absent rather than that its argument was
    // a bug. `TreeMap.remove` is `getEntry(key)`, whose null check fires on an
    // EMPTY map too, hence `size == 0` here rather than `size > 0`.
    tree_natural_order_key_check(ctx, &comparator, elem, size == 0)?;
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Int(0))),
    };
    // Family-1 stale-ObjectRef fix: `this` AND `elem` are used again below
    // (ts_set_slot, ts_view_source, source_map_remove) after the
    // Comparator-invoking search.
    let (search, this, data, elem) = ts_binary_search(ctx, this, data, size, &comparator, &elem)?;
    match search {
        Ok(idx) => {
            ts_remove_at(ctx, data, size, idx);
            ts_set_slot(ctx, this, TS_FIELD_SIZE, Value::Int(size - 1));
            ts_note_pair_remove(&*ctx, this, size, idx);
            // Live TreeMap keySet view: delete the key from the source TreeMap.
            // Live view: propagate the removal to whatever the marker names —
            // a `TreeMap` for a keySet view, a `TreeSet` for a descendingSet.
            if let Some(source) = ts_view_source(ctx, this) {
                ts_source_remove(ctx, source, elem)?;
            }
            Ok(Some(Value::Int(1)))
        }
        Err(_) => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_ts_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let this = resync_ts_view(ctx, this)?;
    let (data_opt, size, comparator) = ts_state(ctx, this);
    // W7-36 residual: `TreeSet.contains` is `m.containsKey(o)`, i.e. the same
    // `getEntry` null-check + `Comparable` checkcast `native_ts_add` already
    // runs. It has to be ahead of the `data_opt` early return as well as the
    // search: an empty set has no backing array here, and answering `false` for
    // `contains(null)` is precisely the "returns where the JDK refuses" row.
    tree_natural_order_key_check(ctx, &comparator, elem, size == 0)?;
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Int(0))),
    };
    let found = ts_binary_search(ctx, this, data, size, &comparator, &elem)?
        .0
        .is_ok();
    Ok(Some(Value::Int(i32::from(found))))
}

pub(crate) fn native_ts_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let this = resync_ts_view(ctx, this)?;
    let size = match ts_get_slot(ctx, this, TS_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(size)))
}

/// `TreeSet.writeObject(ObjectOutputStream)` — serialize from `ts_state`.
///
/// Real `TreeSet.writeObject` writes `comparator`, `size`, then each element
/// by iterating the backing `m` TreeMap's `keySet()`. CratonVM's native
/// TreeSet keeps its elements in the `ts_array_table` side-table and never
/// populates `m`, so the inherited real method serialized an empty set. This
/// native reproduces the exact stream format (`defaultWriteObject`, comparator,
/// size, elements in sorted order) directly from the side-table. Contained to
/// the serialization path — the hot add/contains/size path is untouched.
pub(crate) fn native_ts_write_object(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let oos = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let (data_opt, size, comparator) = ts_state(ctx, this);
    // Substitute a synthetic factory comparator with its real serializable JDK
    // singleton: writing it directly via OOS.writeObject below bypasses the
    // `writeReplace` reflection hook that real `defaultWriteObject` would apply
    // (see native_comparator_write_replace), which would otherwise throw
    // NotSerializableException on `Comparator$Native`.
    let comparator = replace_synthetic_comparator_for_ser(ctx, comparator);
    let oos_cls = "java/io/ObjectOutputStream";
    // s.defaultWriteObject() — TreeSet has no non-transient fields, so this
    // writes nothing but keeps the stream's per-object context consistent.
    ctx.invoke(
        oos_cls,
        "defaultWriteObject",
        "()V",
        &[Value::Object(Some(oos))],
    )?;
    // s.writeObject(comparator)
    ctx.invoke(
        oos_cls,
        "writeObject",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(oos)), comparator],
    )?;
    // s.writeInt(size)
    ctx.invoke(
        oos_cls,
        "writeInt",
        "(I)V",
        &[Value::Object(Some(oos)), Value::Int(size)],
    )?;
    // Elements in sorted order (the data array is kept sorted by native_ts_add).
    if let Some(data) = data_opt {
        // Family-1 stale-ObjectRef fix (2026-07-31): each `writeObject` runs
        // interpreted serialization bytecode, which allocates freely — the
        // backing array and the stream both move under this loop.
        let data_pin = ctx.pin_native_root(data);
        let oos_pin = ctx.pin_native_root(oos);
        let mut data = data;
        let mut oos = oos;
        for i in 0..size as usize {
            let e = ctx.get_array_element(data, i);
            if let Err(err) = ctx.invoke(
                oos_cls,
                "writeObject",
                "(Ljava/lang/Object;)V",
                &[Value::Object(Some(oos)), e],
            ) {
                ctx.unpin_native_roots(data_pin);
                return Err(err);
            }
            data = ctx.read_native_pin(data_pin, data);
            oos = ctx.read_native_pin(oos_pin, oos);
        }
        ctx.unpin_native_roots(data_pin);
    }
    Ok(None)
}

/// `TreeSet.readObject(ObjectInputStream)` — deserialize into `ts_state`.
///
/// Mirrors `native_ts_write_object`. Real `TreeSet.readObject` rebuilds the
/// backing `m` TreeMap's red-black tree, which CratonVM's native TreeSet never
/// reads — so a round-tripped set read back empty. This reads the same format
/// (`defaultReadObject`, comparator, size, elements) straight into the
/// side-table via `native_ts_add` (which honors the comparator and keeps the
/// array sorted; elements arrive already sorted so each insert appends).
pub(crate) fn native_ts_read_object(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let ois = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let ois_cls = "java/io/ObjectInputStream";
    ctx.invoke(
        ois_cls,
        "defaultReadObject",
        "()V",
        &[Value::Object(Some(ois))],
    )?;
    let comparator = ctx
        .invoke(
            ois_cls,
            "readObject",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(ois))],
        )?
        .unwrap_or(Value::Object(None));
    let size = match ctx.invoke(ois_cls, "readInt", "()I", &[Value::Object(Some(ois))])? {
        Some(Value::Int(n)) => n,
        _ => 0,
    };
    // Initialize a fresh backing array + comparator, then add each element.
    let (this, _buf) =
        ts_install_backing_array(ctx, this, (size as usize).max(TS_DEFAULT_CAPACITY));
    ts_set_slot(ctx, this, TS_FIELD_SIZE, Value::Int(0));
    ts_set_slot(ctx, this, TS_FIELD_COMPARATOR, comparator);
    let ois_pin = ctx.pin_native_root(ois);
    let this_pin = ctx.pin_native_root(this);
    for _ in 0..size {
        let ois = ctx.read_native_pin(ois_pin, ois);
        let this = ctx.read_native_pin(this_pin, this);
        let e = ctx
            .invoke(
                ois_cls,
                "readObject",
                "()Ljava/lang/Object;",
                &[Value::Object(Some(ois))],
            )?
            .unwrap_or(Value::Object(None));
        native_ts_add(ctx, &[Value::Object(Some(this)), e])?;
    }
    Ok(None)
}

pub(crate) fn native_ts_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(1))),
    };
    let this = resync_ts_view(ctx, this)?;
    let size = match ts_get_slot(ctx, this, TS_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(i32::from(size == 0))))
}

pub(crate) fn native_ts_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // Live TreeMap keySet view: clearing the view clears the source TreeMap.
    // GC-safety: the view's `clear()` is arbitrary Java and the allocation
    // below collects too; `this` is used after both. See `rooted_across`.
    let this_pin = ctx.pin_native_root(this);
    let cleared = match ts_view_source(ctx, this) {
        Some(source) => ctx.invoke_virtual(source, "clear", "()V", &[]).map(|_| ()),
        None => Ok(()),
    };
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    cleared?;
    let (this, buf) = rooted_across1(ctx, this, |ctx| alloc_ref_array(ctx, TS_DEFAULT_CAPACITY));
    ts_set_slot(ctx, this, TS_FIELD_DATA, Value::Object(Some(buf)));
    ts_set_slot(ctx, this, TS_FIELD_SIZE, Value::Int(0));
    Ok(None)
}

pub(crate) fn native_ts_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "TreeSet is empty".to_string(),
                }
                .into(),
            )
        }
    };
    let this = resync_ts_view(ctx, this)?;
    let (data_opt, size, _) = ts_state(ctx, this);
    // Treat a None backing array as empty rather than panicking on `unwrap`.
    let data = match data_opt {
        Some(d) if size != 0 => d,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "TreeSet is empty".to_string(),
                }
                .into(),
            );
        }
    };
    Ok(Some(ctx.get_array_element(data, 0)))
}

pub(crate) fn native_ts_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "TreeSet is empty".to_string(),
                }
                .into(),
            )
        }
    };
    let this = resync_ts_view(ctx, this)?;
    let (data_opt, size, _) = ts_state(ctx, this);
    // Treat a None backing array as empty rather than panicking on `unwrap`.
    let data = match data_opt {
        Some(d) if size != 0 => d,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "TreeSet is empty".to_string(),
                }
                .into(),
            );
        }
    };
    Ok(Some(ctx.get_array_element(data, (size - 1) as usize)))
}

pub(crate) fn native_ts_ceiling(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    ts_nav_elem_check(&*ctx, this, elem)?;
    let (data_opt, size, comparator) = ts_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    // Family-1 stale-ObjectRef fix: use ts_binary_search's refreshed `data`
    // (the Comparator invocation inside it can trigger a moving GC).
    let (ts_search_result, _this, data, _elem) =
        ts_binary_search(ctx, this, data, size, &comparator, &elem)?;
    match ts_search_result {
        Ok(idx) => Ok(Some(ctx.get_array_element(data, idx))),
        Err(pos) => {
            if pos < size as usize {
                Ok(Some(ctx.get_array_element(data, pos)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
    }
}

pub(crate) fn native_ts_floor(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    ts_nav_elem_check(&*ctx, this, elem)?;
    let (data_opt, size, comparator) = ts_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    // Family-1 stale-ObjectRef fix: use ts_binary_search's refreshed `data`
    // (the Comparator invocation inside it can trigger a moving GC).
    let (ts_search_result, _this, data, _elem) =
        ts_binary_search(ctx, this, data, size, &comparator, &elem)?;
    match ts_search_result {
        Ok(idx) => Ok(Some(ctx.get_array_element(data, idx))),
        Err(pos) => {
            if pos > 0 {
                Ok(Some(ctx.get_array_element(data, pos - 1)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
    }
}

pub(crate) fn native_ts_higher(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    ts_nav_elem_check(&*ctx, this, elem)?;
    let (data_opt, size, comparator) = ts_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    // Family-1 stale-ObjectRef fix: use ts_binary_search's refreshed `data`
    // (the Comparator invocation inside it can trigger a moving GC).
    let (ts_search_result, _this, data, _elem) =
        ts_binary_search(ctx, this, data, size, &comparator, &elem)?;
    match ts_search_result {
        Ok(idx) => {
            let next = idx + 1;
            if next < size as usize {
                Ok(Some(ctx.get_array_element(data, next)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
        Err(pos) => {
            if pos < size as usize {
                Ok(Some(ctx.get_array_element(data, pos)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
    }
}

pub(crate) fn native_ts_lower(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    ts_nav_elem_check(&*ctx, this, elem)?;
    let (data_opt, size, comparator) = ts_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    // Family-1 stale-ObjectRef fix: use ts_binary_search's refreshed `data`
    // (the Comparator invocation inside it can trigger a moving GC).
    let (ts_search_result, _this, data, _elem) =
        ts_binary_search(ctx, this, data, size, &comparator, &elem)?;
    match ts_search_result {
        Ok(idx) => {
            if idx > 0 {
                Ok(Some(ctx.get_array_element(data, idx - 1)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
        Err(pos) => {
            if pos > 0 {
                Ok(Some(ctx.get_array_element(data, pos - 1)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
    }
}

/// NOTE (2026-08-22): `TreeSet.iterator()` and `TreeMap.keySet().iterator()`
/// answer `java.util.Arrays$ArrayItr` where HotSpot answers
/// `java.util.TreeMap$KeyIterator`, and that is the REFUSAL landing below --
/// `java/util/TreeSet$Itr` is fabricated, so `--jdk-only` refuses it.
///
/// Minting the real class was BUILT and REVERTED. It works for a TreeSet this
/// crate backs, and it reddened `RJdkJmx`: java.base builds
/// `TreeMap$KeyIterator` ITSELF for maps the natives never intercept, and a
/// force-native gate keys on the CLASS, so this file's snapshot bodies ran over
/// a real JDK cursor's fields and MBean introspection came back null
/// (`NotCompliantMBeanException: java.lang.management.MemoryMXBean: null`).
///
/// That is the same two-producers-one-class shape that reddened
/// `RJdkEnumerations` for `Hashtable$Enumerator` and for
/// `ConcurrentHashMap$ValueIterator` -- three families, one rule:
/// **a snapshot carrier may only take a real class name that NOTHING ELSE
/// mints.** `HashMap$KeyIterator`, `HashMap$EntryIterator` and the
/// `LinkedHashMap` pair pass that test because the natives own every path that
/// produces them; `TreeMap$KeyIterator` does not.
///
/// Closing it needs the TreeMap natives to own every producer, or a cursor over
/// the real red-black tree `tm_publish_real_root` now builds -- which is the
/// same authority-not-mirror change `W7-96` describes for CHM.
pub(crate) fn native_ts_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    ts_refresh_real_mirrors(ctx, args);
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let this = resync_ts_view(ctx, this)?;
    // `java/util/TreeMap$KeyIterator` -- the class HotSpot 25.0.4+7 hands out
    // for BOTH `TreeSet.iterator()` and `TreeMap.keySet().iterator()`, the
    // second of which is TreeSet-carried on this VM and so comes through here
    // too. ONE producer of the class, which is what keeps this out of
    // `two-producers-of-one-carrier-class-is-a-failure-family`.
    //
    // The comment this replaces already knew the class: "No JDK declares
    // `java.util.TreeSet$Itr`; the real iterator is `TreeMap$KeyIterator`
    // behind `TreeSet.iterator()`." It refused the fabrication, correctly, and
    // then landed on `Arrays$ArrayItr` -- and the fallback is what cost both
    // the class identity and the fail-fast check, because `Arrays$ArrayItr`'s
    // `next()` is real bytecode with nowhere for a check to run.
    //
    // `descendingIterator` is untouched and still mints the `TreeSet$Itr`
    // shape: its HotSpot class is `TreeMap$DescendingKeyIterator`, a different
    // carrier and a separate row.
    let (data_opt, size, _) = ts_state(ctx, this);
    let n = size.max(0) as usize;
    let mut elems = Vec::with_capacity(n);
    if let Some(data) = data_opt {
        for i in 0..n {
            elems.push(ctx.get_array_element(data, i));
        }
    }
    let itr = alloc_family_snapshot_iterator(ctx, this, &elems, "java/util/TreeMap$KeyIterator")?;
    Ok(Some(Value::Object(Some(itr))))
}

/// `TreeSet$Itr.remove()` — delete the last element returned by `next()` from
/// the owning `TreeSet`. The snapshot iterator stores the owning set in
/// [`TS_ITR_FIELD_OWNER`]; the last-returned element is the snapshot entry at
/// [`TS_ITR_FIELD_LAST_RET`].
///
/// W7-1 family 2 (2026-08-11): the guard used to be `cursor <= 0`, which catches
/// `remove()` before any `next()` but NOT `remove()` twice in a row — the cursor
/// is not rewound (the snapshot does not shift under a backing-set removal), so
/// a second call found it unchanged, re-deleted the element it had already
/// deleted, and reported success where the JDK raises `IllegalStateException`.
/// A `lastRet` slot expresses both halves of the contract with one value, which
/// is why the JDK has one and why the `--jdk-only` stand-in already carried the
/// equivalent (`SnapshotItrBacking::last_removed_cursor`).
pub(crate) fn native_ts_itr_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let arr = match ctx.get_field(this, TS_ITR_FIELD_DATA) {
        Value::Object(Some(r)) if ctx.heap_kind_of(r) == ObjectKind::Array => r,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                message: "remove".to_string(),
            }
            .into())
        }
    };
    // Iterators minted before the `lastRet` slot existed (or by a foreign path
    // that built a 3-field shape) fall back to the old cursor heuristic rather
    // than reading past the object.
    let last_ret = if ctx.object_num_fields(this) > TS_ITR_FIELD_LAST_RET {
        match ctx.get_field(this, TS_ITR_FIELD_LAST_RET) {
            Value::Int(v) => v,
            _ => -1,
        }
    } else {
        match ctx.get_field(this, TS_ITR_FIELD_CURSOR) {
            Value::Int(v) => v - 1,
            _ => -1,
        }
    };
    if last_ret < 0 {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "remove".to_string(),
        }
        .into());
    }
    let owner = match ctx.get_field(this, TS_ITR_FIELD_OWNER) {
        Value::Object(Some(o)) => o,
        _ => {
            // No owning set recorded — nothing to remove from. Stay silent
            // rather than throwing UOE so the iteration can still complete.
            return Ok(None);
        }
    };
    let last = ctx.get_array_element(arr, last_ret as usize);
    // GC-SAFETY: `ts_remove_element` dispatches the element's `compareTo` /
    // `Comparator.compare` and can allocate, so read the iterator back through
    // its pin before clearing `lastRet`.
    let this_pin = ctx.pin_native_root(this);
    let removed = ts_remove_element(ctx, owner, last);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    let _ = removed?;
    if ctx.object_num_fields(this) > TS_ITR_FIELD_LAST_RET {
        ctx.set_field(this, TS_ITR_FIELD_LAST_RET, Value::Int(-1));
    }
    // The removal above wrote through to the source map and bumped its
    // generation; adopt it, or the next `next()` trips the check this cursor
    // installed on itself.
    ts_itr_seed_expected(ctx, this);
    Ok(None)
}

/// Delete `last` from the live TreeSet `owner` — the write-through half of
/// `TreeSet$Itr.remove()`.
///
/// Extracted so the `--jdk-only` stand-in (a real `Arrays$ArrayItr` with its
/// backing set in [`snapshot_itr_backing_table`]) removes through exactly the
/// same path as the fabricated iterator, rather than a second copy of it.
pub(crate) fn ts_remove_element(
    ctx: &mut dyn NativeContext,
    owner: ObjectRef,
    last: Value,
) -> MethodCallResult {
    // Delete the element from the live set via the existing TreeSet remove
    // path (binary search + side-table size update).
    let (data_opt, size, comparator) = ts_state(ctx, owner);
    if let Some(data) = data_opt {
        // Family-1 stale-ObjectRef fix: `owner`/`data` are used again below
        // (ts_remove_at, ts_set_slot) after the Comparator-invoking search
        // (`last` isn't reused here so its refreshed copy is discarded).
        let (search, owner, data, _last) =
            ts_binary_search(ctx, owner, data, size, &comparator, &last)?;
        if let Ok(idx) = search {
            ts_remove_at(ctx, data, size, idx);
            ts_set_slot(ctx, owner, TS_FIELD_SIZE, Value::Int(size - 1));
            ts_note_pair_remove(&*ctx, owner, size, idx);
        }
    }
    // Live TreeMap keySet view: the element is a key, so delete it from the
    // source TreeMap as well.
    if let Some(source) = ts_view_source(ctx, owner) {
        ts_source_remove(ctx, source, last)?;
    }
    Ok(None)
}

pub(crate) fn native_ts_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // RULE F, ahead of `resync_ts_view` for the same reason `native_tm_for_each`
    // puts it ahead of `tm_sync_native_state`. See `reject_null_functional`.
    reject_null_functional(args.get(1))?;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let action = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let this = resync_ts_view(ctx, this)?;
    let (data_opt, size, _) = ts_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(None),
    };
    let action_pin = ctx.pin_native_root(action);
    let data_pin = ctx.pin_native_root(data);
    for i in 0..(size as usize) {
        let data = ctx.read_native_pin(data_pin, data);
        let v = ctx.get_array_element(data, i);
        let v_pin = match v {
            Value::Object(Some(o)) => ctx.pin_native_root(o),
            _ => usize::MAX,
        };
        let action = ctx.read_native_pin(action_pin, action);
        let v = read_pinned_elem(ctx, v_pin, v);
        if let Err(e) = ctx.invoke_virtual(action, "accept", "(Ljava/lang/Object;)V", &[v]) {
            ctx.unpin_native_roots(action_pin);
            return Err(e);
        }
        if v_pin != usize::MAX {
            ctx.unpin_native_roots(v_pin);
        }
    }
    ctx.unpin_native_roots(action_pin);
    Ok(None)
}

pub(crate) fn native_ts_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let this = resync_ts_view(ctx, this)?;
    let (data_opt, size, _) = ts_state(ctx, this);
    // cceres3: `data` spans the result-array allocation — pin + re-read.
    let data_pin = data_opt.map(|d| ctx.pin_native_root(d));
    let arr = alloc_ref_array(ctx, size as usize);
    if let (Some(data), Some(pin)) = (data_opt, data_pin) {
        let data = ctx.read_native_pin(pin, data);
        for i in 0..(size as usize) {
            let v = ctx.get_array_element(data, i);
            ctx.set_array_element(arr, i, v);
        }
        ctx.unpin_native_roots(pin);
    }
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn native_ts_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let this = resync_ts_view(ctx, this)?;
    let (data_opt, size, _) = ts_state(ctx, this);
    let mut buf = units_of("[");
    if let Some(data) = data_opt {
        // gc-common w20-a: the element array is read back through a pin before
        // each element (each `toString()` runs Java and can move it).
        push_array_display_units(ctx, &mut buf, data, size.max(0) as usize)?;
    }
    buf.push(u16::from(b']'));
    let s = ctx.create_string_from_units(&buf);
    Ok(Some(Value::Object(Some(s))))
}

pub(crate) fn native_ts_comparator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ts_get_slot(ctx, this, TS_FIELD_COMPARATOR)))
}

/// Give a freshly built `TreeSet` range view its SOURCE and its RANGE.
///
/// See the module note on `TmViewSpec`: the source goes in the element array's
/// trailing capacity slot (the marker [`ts_view_source`] reads and
/// [`ts_ensure_capacity`] preserves), and the bounds go in [`tm_view_table`].
///
/// Called AFTER the builder has returned, deliberately: installing the marker
/// before the fill would make every element the builder copies write straight
/// back into the source.
pub(crate) fn ts_install_range_view(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
    view: ObjectRef,
    lo: Option<(Value, bool)>,
    hi: Option<(Value, bool)>,
) -> Result<(), MethodCallFailed> {
    let (data_opt, size, _) = ts_state(ctx, view);
    // GC-SAFETY: `alloc_ref_array` collects, and `source`, `view` and both
    // bounds are bare Rust locals that END UP STORED -- in the array and in the
    // side table. `src_pin` goes up first so one unpin unwinds the group.
    let src_pin = ctx.pin_native_root(source);
    let view_pin = ctx.pin_native_root(view);
    let data_pin = data_opt.map(|d| ctx.pin_native_root(d));
    let lo_key = lo.map(|(k, _)| k).unwrap_or(Value::Object(None));
    let lo_h = pin_value(ctx, lo_key);
    let hi_key = hi.map(|(k, _)| k).unwrap_or(Value::Object(None));
    let hi_h = pin_value(ctx, hi_key);

    // One spare slot past the logical size, which is what `ts_view_source`
    // tests for. A rebuild rather than a resize keeps the rule in one place.
    let cap = std::cmp::max(size as usize, TS_DEFAULT_CAPACITY) + 1;
    let buf = alloc_ref_array(ctx, cap);
    let buf_pin = ctx.pin_native_root(buf);
    if let (Some(dp), Some(d)) = (data_pin, data_opt) {
        let d = ctx.read_native_pin(dp, d);
        let buf = ctx.read_native_pin(buf_pin, buf);
        for i in 0..(size as usize) {
            let v = ctx.get_array_element(d, i);
            ctx.set_array_element(buf, i, v);
        }
    }
    let buf = ctx.read_native_pin(buf_pin, buf);
    let source_cur = ctx.read_native_pin(src_pin, source);
    ctx.set_array_element(buf, cap - 1, Value::Object(Some(source_cur)));
    let view_cur = ctx.read_native_pin(view_pin, view);
    let buf = ctx.read_native_pin(buf_pin, buf);
    ts_set_slot(ctx, view_cur, TS_FIELD_DATA, Value::Object(Some(buf)));
    let view_cur = ctx.read_native_pin(view_pin, view);
    ts_set_slot(ctx, view_cur, TS_FIELD_SIZE, Value::Int(size));

    let view_cur = ctx.read_native_pin(view_pin, view);
    let source_cur = ctx.read_native_pin(src_pin, source);
    let spec = TmViewSpec {
        source: source_cur,
        lo: read_pinned_elem(ctx, lo_h, lo_key),
        lo_bounded: lo.is_some(),
        lo_inclusive: lo.map(|(_, i)| i).unwrap_or(false),
        hi: read_pinned_elem(ctx, hi_h, hi_key),
        hi_bounded: hi.is_some(),
        hi_inclusive: hi.map(|(_, i)| i).unwrap_or(false),
        descending: false,
    };
    tm_register_view(&*ctx, view_cur, spec);
    ctx.unpin_native_roots(src_pin);
    Ok(())
}

/// Shared tail of the six `TreeSet` range wrappers.
pub(crate) fn ts_range_view(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    built: MethodCallResult,
    lo: Option<(Value, bool)>,
    hi: Option<(Value, bool)>,
) -> MethodCallResult {
    let view = match built? {
        Some(Value::Object(Some(v))) => v,
        other => return Ok(other),
    };
    let source = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(Some(view)))),
    };
    let view_pin = ctx.pin_native_root(view);
    let r = ts_install_range_view(ctx, source, view, lo, hi);
    let view = ctx.read_native_pin(view_pin, view);
    ctx.unpin_native_roots(view_pin);
    r?;
    Ok(Some(Value::Object(Some(view))))
}

/// `args[i]`, or a null reference for a malformed call. Prefixed because this
/// module already has an `arg_bool(args, idx)` of its own further down.
pub(crate) fn ts_range_arg(args: &[Value], i: usize) -> Value {
    args.get(i).copied().unwrap_or(Value::Object(None))
}
pub(crate) fn ts_range_arg_bool(args: &[Value], i: usize) -> bool {
    matches!(args.get(i), Some(Value::Int(v)) if *v != 0)
}

pub(crate) fn native_ts_head_set_view(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let hi = Some((ts_range_arg(args, 1), false));
    let built = native_ts_head_set(ctx, args);
    ts_range_view(ctx, args, built, None, hi)
}
pub(crate) fn native_ts_head_set_inclusive_view(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let hi = Some((ts_range_arg(args, 1), ts_range_arg_bool(args, 2)));
    let built = native_ts_head_set_inclusive(ctx, args);
    ts_range_view(ctx, args, built, None, hi)
}
pub(crate) fn native_ts_tail_set_view(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let lo = Some((ts_range_arg(args, 1), true));
    let built = native_ts_tail_set(ctx, args);
    ts_range_view(ctx, args, built, lo, None)
}
pub(crate) fn native_ts_tail_set_inclusive_view(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let lo = Some((ts_range_arg(args, 1), ts_range_arg_bool(args, 2)));
    let built = native_ts_tail_set_inclusive(ctx, args);
    ts_range_view(ctx, args, built, lo, None)
}
pub(crate) fn native_ts_sub_set_view(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let lo = Some((ts_range_arg(args, 1), true));
    let hi = Some((ts_range_arg(args, 2), false));
    let built = native_ts_sub_set(ctx, args);
    ts_range_view(ctx, args, built, lo, hi)
}
pub(crate) fn native_ts_sub_set_inclusive_view(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let lo = Some((ts_range_arg(args, 1), ts_range_arg_bool(args, 2)));
    let hi = Some((ts_range_arg(args, 3), ts_range_arg_bool(args, 4)));
    let built = native_ts_sub_set_inclusive(ctx, args);
    ts_range_view(ctx, args, built, lo, hi)
}

pub(crate) fn native_ts_head_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let to_elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data_opt, size, comparator, mut result, [to_elem, _]) =
        ts_begin_range_result(ctx, this, [to_elem, Value::Object(None)])?;
    if let Some(data) = data_opt {
        // Family-1 stale-ObjectRef fix: `tree_compare`/`native_ts_add` can
        // run a user Comparator/lambda and trigger a moving GC. See
        // wildfly-parallel-boot-stale-objectref-residual.md.
        let data_pin = ctx.pin_native_root(data);
        let result_pin = ctx.pin_native_root(result);
        let mut data = data;
        let mut comparator = comparator;
        let comparator_pin = pin_value(ctx, comparator);
        let mut to_elem = to_elem;
        let to_elem_pin = pin_value(ctx, to_elem);
        for i in 0..(size as usize) {
            let e = ctx.get_array_element(data, i);
            let cmp = tree_compare(ctx, &comparator, e, to_elem)?;
            data = ctx.read_native_pin(data_pin, data);
            result = ctx.read_native_pin(result_pin, result);
            comparator = read_pinned_elem(ctx, comparator_pin, comparator);
            to_elem = read_pinned_elem(ctx, to_elem_pin, to_elem);
            if cmp >= 0 {
                break;
            }
            // Family-1 stale-ObjectRef fix (2026-07-31): `e` was read from
            // `data` BEFORE the comparison above, which dispatches
            // interpreted bytecode and can move it. `data` is kept
            // current through its pin, so re-read the element.
            let e = ctx.get_array_element(data, i);
            native_ts_add(ctx, &[Value::Object(Some(result)), e])?;
            data = ctx.read_native_pin(data_pin, data);
            result = ctx.read_native_pin(result_pin, result);
            comparator = read_pinned_elem(ctx, comparator_pin, comparator);
            to_elem = read_pinned_elem(ctx, to_elem_pin, to_elem);
        }
        ctx.unpin_native_roots(data_pin);
    }
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_ts_tail_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let from_elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data_opt, size, comparator, mut result, [from_elem, _]) =
        ts_begin_range_result(ctx, this, [from_elem, Value::Object(None)])?;
    if let Some(data) = data_opt {
        // Family-1 stale-ObjectRef fix: same hazard as `native_ts_head_set`.
        let data_pin = ctx.pin_native_root(data);
        let result_pin = ctx.pin_native_root(result);
        let mut data = data;
        let mut comparator = comparator;
        let comparator_pin = pin_value(ctx, comparator);
        let mut from_elem = from_elem;
        let from_elem_pin = pin_value(ctx, from_elem);
        for i in 0..(size as usize) {
            let e = ctx.get_array_element(data, i);
            let cmp = tree_compare(ctx, &comparator, e, from_elem)?;
            data = ctx.read_native_pin(data_pin, data);
            result = ctx.read_native_pin(result_pin, result);
            comparator = read_pinned_elem(ctx, comparator_pin, comparator);
            from_elem = read_pinned_elem(ctx, from_elem_pin, from_elem);
            if cmp >= 0 {
                // Family-1 stale-ObjectRef fix (2026-07-31): `e` was read from
                // `data` BEFORE the comparison above, which dispatches
                // interpreted bytecode and can move it. `data` is kept
                // current through its pin, so re-read the element.
                let e = ctx.get_array_element(data, i);
                native_ts_add(ctx, &[Value::Object(Some(result)), e])?;
                data = ctx.read_native_pin(data_pin, data);
                result = ctx.read_native_pin(result_pin, result);
                comparator = read_pinned_elem(ctx, comparator_pin, comparator);
                from_elem = read_pinned_elem(ctx, from_elem_pin, from_elem);
            }
        }
        ctx.unpin_native_roots(data_pin);
    }
    Ok(Some(Value::Object(Some(result))))
}

/// The prologue all six `TreeSet` range-view natives share: read the source's
/// side-table state, allocate the result set and its element array, publish
/// them - and hand every one of those back at its POST-allocation address.
///
/// # Why this is a function and not six copies of six lines
///
/// The six copies read `ts_state` - the source's backing array and its
/// comparator - and the caller's bound elements BEFORE two allocations
/// (`try_alloc_declared_width`, then `alloc_ref_array`), and read `result` and
/// `buf` back after them, all as bare Rust locals. Each native then opened its
/// scan loop by pinning those same locals, which pins nothing if a relocation
/// already happened: a from-space address, pinned, stays a from-space address.
///
/// MEASURED, `RTreeRangeGc` under `--jdk-only`, 2 runs in 20:
///
/// ```text
/// ClassCastException: class java.lang.Object cannot be cast to
///                     class java.lang.Comparable   at RTreeRangeGc.java:210
/// ```
///
/// and `CRATONVM_DBG_CCE_BT` named the site exactly - `native_ts_sub_set` ->
/// `tree_compare` -> `compare_via_compare_to`, with `a=java/lang/Object(cid=0)`
/// and `b=RTreeRangeGc$K`. The BOUND was fine; `a` is an ELEMENT read out of a
/// backing array that had moved, and `java.lang.Object` with `class_id=0` is
/// the free-list face of a block that was reclaimed while still referenced.
///
/// This is `WORKER-1-NOTE-1`'s undiagnosed half: it measured `RTreeRangeGc` at
/// 9 pass / 3 fail under `--jdk-only` and deterministic in compatible mode, and
/// recorded that those had to be two different defects. They were.
///
/// `bounds` is a fixed pair so a single-bound caller pays no allocation for it;
/// pass `Value::Object(None)` for the absent one and ignore it on the way out.
pub(crate) fn ts_begin_range_result(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    bounds: [Value; 2],
) -> Result<(Option<ObjectRef>, i32, Value, ObjectRef, [Value; 2]), MethodCallFailed> {
    // `this_pin` goes up first, so one `unpin_native_roots(this_pin)` unwinds
    // every handle this function pushes - including on the failure path.
    let this_pin = ctx.pin_native_root(this);
    let lo_pin = pin_value(ctx, bounds[0]);
    let hi_pin = pin_value(ctx, bounds[1]);
    let (data_opt, size, comparator) = ts_state(ctx, this);
    let data_pin = data_opt.map(|d| ctx.pin_native_root(d));
    let cmp_pin = pin_value(ctx, comparator);

    let result = match try_alloc_declared_width(ctx, "java/util/TreeSet", TS_NUM_FIELDS) {
        Ok(r) => r,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let result_pin = ctx.pin_native_root(result);
    let buf = alloc_ref_array(ctx, TS_DEFAULT_CAPACITY);
    let buf_pin = ctx.pin_native_root(buf);

    let result = ctx.read_native_pin(result_pin, result);
    let buf = ctx.read_native_pin(buf_pin, buf);
    let comparator = read_pinned_elem(ctx, cmp_pin, comparator);
    ts_set_slot(ctx, result, TS_FIELD_DATA, Value::Object(Some(buf)));
    ts_set_slot(ctx, result, TS_FIELD_SIZE, Value::Int(0));
    ts_set_slot(ctx, result, TS_FIELD_COMPARATOR, comparator);

    let result = ctx.read_native_pin(result_pin, result);
    let data_opt = match (data_opt, data_pin) {
        (Some(d), Some(p)) => Some(ctx.read_native_pin(p, d)),
        _ => None,
    };
    let comparator = read_pinned_elem(ctx, cmp_pin, comparator);
    let bounds = [
        read_pinned_elem(ctx, lo_pin, bounds[0]),
        read_pinned_elem(ctx, hi_pin, bounds[1]),
    ];
    ctx.unpin_native_roots(this_pin);
    Ok((data_opt, size, comparator, result, bounds))
}

/// The `TreeSet` twin of [`tm_refuse_reversed_bounds`].
///
/// W7-36 left this one explicitly: *"`TreeSet.subSet(hi, lo)`. The `TreeMap`
/// twin of it is fixed; the `TreeSet` one is not measured and was not widened
/// into."* `TreeSet.subSet` is `new TreeSet<>(m.subMap(...))`, so it reaches the
/// **same** `NavigableSubMap` constructor and the same
/// `throw new IllegalArgumentException("fromKey > toKey")` — the message is that
/// constructor's verbatim, in both places, for the reason
/// `tree_natural_order_key_check` gives for sharing the CCE text: two sites
/// modelling one refusal must not drift into two different reports.
///
/// Returns the possibly-moved receiver and bounds, exactly as the `TreeMap`
/// twin does: `tree_compare` dispatches a user `Comparator` (or a
/// `Comparable.compareTo` override), a full interpreted call that can move all
/// three, and all three are used afterwards.
pub(crate) fn ts_refuse_reversed_bounds(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    from_elem: Value,
    to_elem: Value,
) -> Result<(ObjectRef, Value, Value), MethodCallFailed> {
    let comparator = ts_get_slot(ctx, this, TS_FIELD_COMPARATOR);
    let this_pin = ctx.pin_native_root(this);
    let from_pin = pin_value(ctx, from_elem);
    let to_pin = pin_value(ctx, to_elem);
    let verdict = tree_compare(ctx, &comparator, from_elem, to_elem);
    let this = ctx.read_native_pin(this_pin, this);
    let from_elem = read_pinned_elem(ctx, from_pin, from_elem);
    let to_elem = read_pinned_elem(ctx, to_pin, to_elem);
    ctx.unpin_native_roots(this_pin);
    if verdict? > 0 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "fromKey > toKey".to_string(),
            }
            .into(),
        );
    }
    Ok((this, from_elem, to_elem))
}

pub(crate) fn native_ts_sub_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let from_elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let to_elem = args.get(2).copied().unwrap_or(Value::Object(None));
    let (this, from_elem, to_elem) = ts_refuse_reversed_bounds(ctx, this, from_elem, to_elem)?;
    let (data_opt, size, comparator, mut result, [from_elem, to_elem]) =
        ts_begin_range_result(ctx, this, [from_elem, to_elem])?;
    if let Some(data) = data_opt {
        // Family-1 stale-ObjectRef fix: same hazard as `native_ts_head_set`.
        let data_pin = ctx.pin_native_root(data);
        let result_pin = ctx.pin_native_root(result);
        let mut data = data;
        let mut comparator = comparator;
        let comparator_pin = pin_value(ctx, comparator);
        let mut from_elem = from_elem;
        let from_elem_pin = pin_value(ctx, from_elem);
        let mut to_elem = to_elem;
        let to_elem_pin = pin_value(ctx, to_elem);
        for i in 0..(size as usize) {
            let e = ctx.get_array_element(data, i);
            let cmp_lo = tree_compare(ctx, &comparator, e, from_elem)?;
            data = ctx.read_native_pin(data_pin, data);
            result = ctx.read_native_pin(result_pin, result);
            comparator = read_pinned_elem(ctx, comparator_pin, comparator);
            from_elem = read_pinned_elem(ctx, from_elem_pin, from_elem);
            to_elem = read_pinned_elem(ctx, to_elem_pin, to_elem);
            if cmp_lo < 0 {
                continue;
            }
            // `e` was read from `data` BEFORE the `from_elem` comparison above,
            // and that comparison dispatches the element's real `compareTo` -- a
            // full interpreted call that can move `e`. The re-read three
            // statements down (before `native_ts_add`) has been here since
            // 2026-07-31 and says exactly this; the SECOND COMPARISON never got
            // it, so it fed a from-space address to `tree_compare` and the
            // element decoded as whatever now occupied its old block.
            //
            // MEASURED: `RTreeRangeGc` under `--jdk-only` failed ~3 runs in 100
            // with `ClassCastException: class java.lang.Object cannot be cast to
            // class java.lang.Comparable` from THIS line, while the source set
            // audited 400 / 400 intact immediately afterwards and an immediate
            // retry of the same `subSet` answered 200. Nothing was wrong with
            // the set; one Rust local was one collection out of date.
            //
            // This is also why `subSet` is ~6x more sensitive than `headSet` /
            // `tailSet` rather than the 2x its extra bound key would explain
            // (`WORKER-1-NOTE-2` N2): the single-bound natives compare once per
            // element and have nothing to go stale between.
            let e = ctx.get_array_element(data, i);
            let cmp_hi = tree_compare(ctx, &comparator, e, to_elem)?;
            data = ctx.read_native_pin(data_pin, data);
            result = ctx.read_native_pin(result_pin, result);
            comparator = read_pinned_elem(ctx, comparator_pin, comparator);
            from_elem = read_pinned_elem(ctx, from_elem_pin, from_elem);
            to_elem = read_pinned_elem(ctx, to_elem_pin, to_elem);
            if cmp_hi >= 0 {
                break;
            }
            // Family-1 stale-ObjectRef fix (2026-07-31): `e` was read from
            // `data` BEFORE the comparison above, which dispatches
            // interpreted bytecode and can move it. `data` is kept
            // current through its pin, so re-read the element.
            let e = ctx.get_array_element(data, i);
            native_ts_add(ctx, &[Value::Object(Some(result)), e])?;
            data = ctx.read_native_pin(data_pin, data);
            result = ctx.read_native_pin(result_pin, result);
            comparator = read_pinned_elem(ctx, comparator_pin, comparator);
            from_elem = read_pinned_elem(ctx, from_elem_pin, from_elem);
            to_elem = read_pinned_elem(ctx, to_elem_pin, to_elem);
        }
        ctx.unpin_native_roots(data_pin);
    }
    Ok(Some(Value::Object(Some(result))))
}

// --- NavigableSet range views (`tailSet`/`headSet`/`subSet` with inclusive
// flags). The synthetic TreeSet keeps its elements in the `ts_state` side-table
// and never populates the inherited backing `m` TreeMap, so the real-JDK
// bytecode for these methods NPEs on a null `m` (`this.m.tailMap(...)`). Spring's
// PathMatchingResourcePatternResolver scans cached jar entries via
// `NavigableSet.tailSet(entry, false)`, which tripped exactly this. Drive them
// from the side-table like the SortedSet-returning 1-/2-arg variants above. The
// side-table data is kept in ascending order, so a single forward scan suffices.

pub(crate) fn arg_bool(args: &[Value], idx: usize) -> bool {
    matches!(args.get(idx), Some(Value::Int(v)) if *v != 0)
}

pub(crate) fn native_ts_tail_set_inclusive(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let from_elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let inclusive = arg_bool(args, 2);
    let (data_opt, size, comparator, mut result, [from_elem, _]) =
        ts_begin_range_result(ctx, this, [from_elem, Value::Object(None)])?;
    if let Some(data) = data_opt {
        // Family-1 stale-ObjectRef fix: same hazard as `native_ts_head_set`.
        let data_pin = ctx.pin_native_root(data);
        let result_pin = ctx.pin_native_root(result);
        let mut data = data;
        let mut comparator = comparator;
        let comparator_pin = pin_value(ctx, comparator);
        let mut from_elem = from_elem;
        let from_elem_pin = pin_value(ctx, from_elem);
        for i in 0..(size as usize) {
            let e = ctx.get_array_element(data, i);
            let cmp = tree_compare(ctx, &comparator, e, from_elem)?;
            data = ctx.read_native_pin(data_pin, data);
            result = ctx.read_native_pin(result_pin, result);
            comparator = read_pinned_elem(ctx, comparator_pin, comparator);
            from_elem = read_pinned_elem(ctx, from_elem_pin, from_elem);
            let keep = if inclusive { cmp >= 0 } else { cmp > 0 };
            if keep {
                // Family-1 stale-ObjectRef fix (2026-07-31): `e` was read from
                // `data` BEFORE the comparison above, which dispatches
                // interpreted bytecode and can move it. `data` is kept
                // current through its pin, so re-read the element.
                let e = ctx.get_array_element(data, i);
                native_ts_add(ctx, &[Value::Object(Some(result)), e])?;
                data = ctx.read_native_pin(data_pin, data);
                result = ctx.read_native_pin(result_pin, result);
                comparator = read_pinned_elem(ctx, comparator_pin, comparator);
                from_elem = read_pinned_elem(ctx, from_elem_pin, from_elem);
            }
        }
        ctx.unpin_native_roots(data_pin);
    }
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_ts_head_set_inclusive(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let to_elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let inclusive = arg_bool(args, 2);
    let (data_opt, size, comparator, mut result, [to_elem, _]) =
        ts_begin_range_result(ctx, this, [to_elem, Value::Object(None)])?;
    if let Some(data) = data_opt {
        // Family-1 stale-ObjectRef fix: same hazard as `native_ts_head_set`.
        let data_pin = ctx.pin_native_root(data);
        let result_pin = ctx.pin_native_root(result);
        let mut data = data;
        let mut comparator = comparator;
        let comparator_pin = pin_value(ctx, comparator);
        let mut to_elem = to_elem;
        let to_elem_pin = pin_value(ctx, to_elem);
        for i in 0..(size as usize) {
            let e = ctx.get_array_element(data, i);
            let cmp = tree_compare(ctx, &comparator, e, to_elem)?;
            data = ctx.read_native_pin(data_pin, data);
            result = ctx.read_native_pin(result_pin, result);
            comparator = read_pinned_elem(ctx, comparator_pin, comparator);
            to_elem = read_pinned_elem(ctx, to_elem_pin, to_elem);
            // Ascending order: once we pass the upper bound, stop.
            let stop = if inclusive { cmp > 0 } else { cmp >= 0 };
            if stop {
                break;
            }
            // Family-1 stale-ObjectRef fix (2026-07-31): `e` was read from
            // `data` BEFORE the comparison above, which dispatches
            // interpreted bytecode and can move it. `data` is kept
            // current through its pin, so re-read the element.
            let e = ctx.get_array_element(data, i);
            native_ts_add(ctx, &[Value::Object(Some(result)), e])?;
            data = ctx.read_native_pin(data_pin, data);
            result = ctx.read_native_pin(result_pin, result);
            comparator = read_pinned_elem(ctx, comparator_pin, comparator);
            to_elem = read_pinned_elem(ctx, to_elem_pin, to_elem);
        }
        ctx.unpin_native_roots(data_pin);
    }
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_ts_sub_set_inclusive(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let from_elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let from_inclusive = arg_bool(args, 2);
    let to_elem = args.get(3).copied().unwrap_or(Value::Object(None));
    let to_inclusive = arg_bool(args, 4);
    // The JDK's check is on the bounds alone and ignores the inclusive flags —
    // `subSet(e, false, e, false)` is a legal empty range, `subSet(hi, .., lo, ..)`
    // is not. Same split as `native_tm_sub_map_inclusive`.
    let (this, from_elem, to_elem) = ts_refuse_reversed_bounds(ctx, this, from_elem, to_elem)?;
    let (data_opt, size, comparator, mut result, [from_elem, to_elem]) =
        ts_begin_range_result(ctx, this, [from_elem, to_elem])?;
    if let Some(data) = data_opt {
        // Family-1 stale-ObjectRef fix: same hazard as `native_ts_head_set`.
        let data_pin = ctx.pin_native_root(data);
        let result_pin = ctx.pin_native_root(result);
        let mut data = data;
        let mut comparator = comparator;
        let comparator_pin = pin_value(ctx, comparator);
        let mut from_elem = from_elem;
        let from_elem_pin = pin_value(ctx, from_elem);
        let mut to_elem = to_elem;
        let to_elem_pin = pin_value(ctx, to_elem);
        for i in 0..(size as usize) {
            let e = ctx.get_array_element(data, i);
            let cmp_lo = tree_compare(ctx, &comparator, e, from_elem)?;
            data = ctx.read_native_pin(data_pin, data);
            result = ctx.read_native_pin(result_pin, result);
            comparator = read_pinned_elem(ctx, comparator_pin, comparator);
            from_elem = read_pinned_elem(ctx, from_elem_pin, from_elem);
            to_elem = read_pinned_elem(ctx, to_elem_pin, to_elem);
            let below = if from_inclusive {
                cmp_lo < 0
            } else {
                cmp_lo <= 0
            };
            if below {
                continue;
            }
            // `e` was read from `data` BEFORE the `from_elem` comparison above,
            // and that comparison dispatches the element's real `compareTo` -- a
            // full interpreted call that can move `e`. The re-read three
            // statements down (before `native_ts_add`) has been here since
            // 2026-07-31 and says exactly this; the SECOND COMPARISON never got
            // it, so it fed a from-space address to `tree_compare` and the
            // element decoded as whatever now occupied its old block.
            //
            // MEASURED: `RTreeRangeGc` under `--jdk-only` failed ~3 runs in 100
            // with `ClassCastException: class java.lang.Object cannot be cast to
            // class java.lang.Comparable` from THIS line, while the source set
            // audited 400 / 400 intact immediately afterwards and an immediate
            // retry of the same `subSet` answered 200. Nothing was wrong with
            // the set; one Rust local was one collection out of date.
            //
            // This is also why `subSet` is ~6x more sensitive than `headSet` /
            // `tailSet` rather than the 2x its extra bound key would explain
            // (`WORKER-1-NOTE-2` N2): the single-bound natives compare once per
            // element and have nothing to go stale between.
            let e = ctx.get_array_element(data, i);
            let cmp_hi = tree_compare(ctx, &comparator, e, to_elem)?;
            data = ctx.read_native_pin(data_pin, data);
            result = ctx.read_native_pin(result_pin, result);
            comparator = read_pinned_elem(ctx, comparator_pin, comparator);
            from_elem = read_pinned_elem(ctx, from_elem_pin, from_elem);
            to_elem = read_pinned_elem(ctx, to_elem_pin, to_elem);
            let above = if to_inclusive {
                cmp_hi > 0
            } else {
                cmp_hi >= 0
            };
            if above {
                break;
            }
            // Family-1 stale-ObjectRef fix (2026-07-31): `e` was read from
            // `data` BEFORE the comparison above, which dispatches
            // interpreted bytecode and can move it. `data` is kept
            // current through its pin, so re-read the element.
            let e = ctx.get_array_element(data, i);
            native_ts_add(ctx, &[Value::Object(Some(result)), e])?;
            data = ctx.read_native_pin(data_pin, data);
            result = ctx.read_native_pin(result_pin, result);
            comparator = read_pinned_elem(ctx, comparator_pin, comparator);
            from_elem = read_pinned_elem(ctx, from_elem_pin, from_elem);
            to_elem = read_pinned_elem(ctx, to_elem_pin, to_elem);
        }
        ctx.unpin_native_roots(data_pin);
    }
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_ts_add_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // RULE C, with the dereference JDK 25 `TreeSet.addAll` makes first:
    // `m.size()==0 && c.size() > 0` on an empty receiver, else
    // `AbstractCollection.addAll`'s `for (E e : c)` (MEASURED,
    // `R15Compat5NullArgMore` rows 2-3; round 15 wave 5 lane compat6).
    if matches!(args.get(1), Some(Value::Object(None))) {
        // Round 15 wave 6 (lane sweepC; `r15w5-compat6-review-findings` item
        // 9): the JDK asks `m.size()` LIVE, and a range view's side-table size
        // is a snapshot of its source map; `native_ts_size` resyncs the view
        // first (it can run Java; nothing else is held here).
        let receiver_empty = match args.first() {
            Some(Value::Object(Some(obj))) if null_collection_message_enabled() => {
                matches!(
                    native_ts_size(ctx, &[Value::Object(Some(*obj))])?,
                    Some(Value::Int(0))
                )
            }
            Some(Value::Object(Some(obj))) => ts_state(ctx, *obj).1 == 0,
            _ => true,
        };
        let message = if receiver_empty {
            NULL_C_SIZE
        } else {
            NULL_C_ITERATOR
        };
        reject_null_collection_deref(args.get(1), message)?;
    }
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let source = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    // `_or_real`: see `native_ts_init_collection` — a foreign source view
    // (e.g. an unmodifiable map's keySet over a real-bytecode backing map)
    // returns no elements from the layout heuristics, so fall back to its
    // real `toArray()`. Without this, `TreeSet.addAll(map.keySet())` was a
    // silent no-op (MimeType.compareTo's case-sensitive parameter check).
    //
    // Round 15 wave 4, lane compat5: `this` is pinned BEFORE the walk, which
    // runs a foreign source's own `toArray()` / iterator (a GC point) -- it was
    // pinned after it, and pinning a moved address keeps it stale -- and a
    // throwing walk or `add` (a comparator's `ClassCastException` for an
    // incomparable element) releases the frame instead of leaving it pinned.
    let this_pin = ctx.pin_native_root(this);
    // JIT round 16 (lane compatdrain), r15w4-compat5 item 7: the
    // `buildFromSorted` arm. `source` is rooted in this frame's group.
    let source_pin = ctx.pin_native_root(source);
    let built = ts_add_all_build_from_sorted(ctx, (this_pin, this), (source_pin, source));
    if unpin_on_err(ctx, this_pin, built)? {
        ctx.unpin_native_roots(this_pin);
        return Ok(Some(Value::Int(1)));
    }
    let source = ctx.read_native_pin(source_pin, source);
    let collected = collect_collection_elements_or_real(ctx, source);
    let elems = unpin_on_err(ctx, this_pin, collected)?;
    // Family-1 stale-ObjectRef fix: this is the exact hazard behind the
    // WildFly `XMLSequence$DefaultXMLSequence.collectNames` boot panics
    // (class_id 128) — `TreeSet.addAll` on a set built with a real
    // (non-synthetic) user Comparator. Each `native_ts_add` call below can
    // run that Comparator/lambda and trigger a moving GC. `this` was a bare
    // local reused across every iteration, and `elems` (collected up front)
    // sat entirely unpinned in a Rust `Vec` across the whole loop — pin both
    // before the loop starts and refresh on every iteration. See
    // wildfly-parallel-boot-stale-objectref-residual.md.
    let (_, elem_pins) = pin_value_slice(ctx, &elems);
    let mut changed = false;
    for i in 0..elems.len() {
        let this_cur = ctx.read_native_pin(this_pin, this);
        let e = read_pinned_elem(ctx, elem_pins[i], elems[i]);
        let added = native_ts_add(ctx, &[Value::Object(Some(this_cur)), e]);
        let result = unpin_on_err(ctx, this_pin, added)?;
        if result == Some(Value::Int(1)) {
            changed = true;
        }
    }
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Int(i32::from(changed))))
}

/// `CRATONVM_COMPAT_TREESET_BUILD_FROM_SORTED` -- default ON, `=0` restores
/// the per-element `add`. See [`ts_add_all_build_from_sorted`].
pub(crate) fn treeset_build_from_sorted_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_TREESET_BUILD_FROM_SORTED")
}

/// JDK 25 `TreeSet.addAll(c)`'s first arm: `if (m.size()==0 && c.size() > 0
/// && c instanceof SortedSet && m instanceof TreeMap) { if
/// (Objects.equals(set.comparator(), map.comparator())) { ++modCount;
/// map.addAllForTreeSet(set, PRESENT); return true; } }` --
/// `addAllForTreeSet` is `buildFromSorted(set.size(), set.iterator(), ..)`:
/// `next()` exactly `size()` times, no `hasNext()`, and NO comparator call.
/// `TreeSet(SortedSet)` (`this(s.comparator()); addAll(s)`) and
/// `TreeSet(Collection)` over a sorted source reach it too. The natives added
/// per element (`n log n` comparisons, which a counting or side-effecting
/// comparator observes: 54 calls for 20 elements against HotSpot's 0). JIT
/// round 16 (lane compatdrain), `r15w4-compat5-review-findings-FIXED-20261001.md`
/// item 7 (proposal C5W4-6's `TreeSet` half).
///
/// The receiver (rooted at `this_pin`) must be a plain native `TreeSet`: a
/// range or descending view's `m` is not a `TreeMap`, so the JDK skips the
/// arm for it, and so does this. The elements are appended in the source's
/// order with no ordering check, as `buildFromSorted` links them. Answers
/// `Ok(false)` when the arm does not apply and the caller adds per element
/// (having asked a `SortedSet` source `size()` and `comparator()`, as the
/// JDK does before its fallback `super.addAll(c)`).
pub(crate) fn ts_add_all_build_from_sorted(
    ctx: &mut dyn NativeContext,
    (this_pin, this): (usize, ObjectRef),
    (source_pin, source): (usize, ObjectRef),
) -> Result<bool, MethodCallFailed> {
    if !treeset_build_from_sorted_enabled() {
        return Ok(false);
    }
    let this_now = ctx.read_native_pin(this_pin, this);
    if tm_view_spec(&*ctx, this_now).is_some() || ts_view_source(&*ctx, this_now).is_some() {
        return Ok(false);
    }
    if ts_state(&*ctx, this_now).1 != 0 {
        return Ok(false);
    }
    let source_now = ctx.read_native_pin(source_pin, source);
    if !obj_is_instance_of(&*ctx, source_now, "java/util/SortedSet") {
        return Ok(false);
    }
    // `c.size() > 0`. The JDK asks it of every `c` before its `instanceof`;
    // asked here of a `SortedSet` only, so a source this arm can never take
    // keeps the fallback walk's questions unchanged.
    match ctx.invoke_virtual(source_now, "size", "()I", &[])? {
        Some(Value::Int(n)) if n > 0 => {}
        _ => return Ok(false),
    }
    let source_now = ctx.read_native_pin(source_pin, source);
    let cc = match ctx.invoke_virtual(source_now, "comparator", "()Ljava/util/Comparator;", &[])? {
        Some(Value::Object(c)) => c,
        _ => None,
    };
    let this_now = ctx.read_native_pin(this_pin, this);
    let mc = match ts_state(&*ctx, this_now).2 {
        Value::Object(c) => c,
        _ => None,
    };
    // `Objects.equals(cc, mc)`: `cc == mc || (cc != null && cc.equals(mc))`.
    let same = match (cc, mc) {
        (None, None) => true,
        (Some(a), Some(b)) if a == b => true,
        (Some(a), mc) => matches!(
            ctx.invoke_virtual(a, "equals", "(Ljava/lang/Object;)Z", &[Value::Object(mc)])?,
            Some(Value::Int(1))
        ),
        (None, Some(_)) => false,
    };
    if !same {
        return Ok(false);
    }
    // `buildFromSorted(set.size(), set.iterator(), null, PRESENT)`.
    let source_now = ctx.read_native_pin(source_pin, source);
    let n = match ctx.invoke_virtual(source_now, "size", "()I", &[])? {
        Some(Value::Int(n)) => n.max(0),
        _ => 0,
    };
    let source_now = ctx.read_native_pin(source_pin, source);
    let it = match ctx.invoke_virtual(source_now, "iterator", "()Ljava/util/Iterator;", &[])? {
        Some(Value::Object(Some(it))) => it,
        _ => return Err(bare_npe()),
    };
    let it_pin = ctx.pin_native_root(it);
    let filled = (|| -> Result<(), MethodCallFailed> {
        for _ in 0..n {
            let it_now = ctx.read_native_pin(it_pin, it);
            let e = ctx
                .invoke_virtual(it_now, "next", "()Ljava/lang/Object;", &[])?
                .unwrap_or(Value::Object(None));
            let e_pin = pin_value(ctx, e);
            let mut this_now = ctx.read_native_pin(this_pin, this);
            let (data_opt, size, _) = ts_state(&*ctx, this_now);
            let data = match data_opt {
                Some(d) => d,
                None => {
                    let (refreshed, buf) =
                        ts_install_backing_array(ctx, this_now, TS_DEFAULT_CAPACITY);
                    this_now = refreshed;
                    buf
                }
            };
            let data = ts_ensure_capacity(ctx, this_now, size, data);
            let this_now = ctx.read_native_pin(this_pin, this);
            let e = read_pinned_elem(&*ctx, e_pin, e);
            // Appended: `size` is the end, so nothing shifts.
            ts_insert_at(ctx, data, size, size as usize, e);
            ts_set_slot(ctx, this_now, TS_FIELD_SIZE, Value::Int(size + 1));
            // `buildFromSorted` roots a two-element tree at its FIRST element.
            ts_note_pair_insert(&*ctx, this_now, size, size as usize);
            if e_pin != usize::MAX {
                ctx.unpin_native_roots(e_pin);
            }
        }
        Ok(())
    })();
    ctx.unpin_native_roots(it_pin);
    filled.map(|()| true)
}

/// `TreeSet.descendingIterator()` — iterate elements in descending order.
///
/// Real `TreeSet.descendingIterator()` is `return m.descendingKeySet().iterator()`,
/// reading the backing `m` TreeMap. CratonVM's native TreeSet keeps its
/// elements in the `ts_array_table` side-table and never populates `m`, so the
/// inherited real method dereferenced a null `m` → NPE (snakeyaml's
/// `SafeConstructor.processDuplicateKeys` hit this). This native builds a
/// reversed snapshot and returns the same `TreeSet$Itr` shape used by
/// `iterator()` (field 0 = snapshot, 1 = cursor, 2 = owning set for
/// `Iterator.remove`).
pub(crate) fn native_ts_descending_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let this = resync_ts_view(ctx, this)?;
    let (data_opt, size, _) = ts_state(ctx, this);
    let n = size as usize;
    // GC-safety: see `native_ts_iterator`.
    let mut this = this;
    let mut data_ref = data_opt.unwrap_or(this);
    let mut snap = rooted_across(ctx, &mut [&mut this, &mut data_ref], |ctx| {
        alloc_ref_array(ctx, n)
    });
    let data_opt = data_opt.map(|_| data_ref);
    if let Some(data) = data_opt {
        for i in 0..n {
            // Reverse: snap[i] = data[size-1-i] (the data array is kept sorted
            // ascending by native_ts_add).
            let v = ctx.get_array_element(data, n - 1 - i);
            ctx.set_array_element(snap, i, v);
        }
    }
    // Fallible since 2026-08-05 (JDK-only wave 2, lane L7). No JDK declares
    // `java.util.TreeSet$Itr`; the real iterator is `TreeMap$KeyIterator`
    // behind `TreeSet.iterator()`. Refuse under `--jdk-only`, naming the class.
    let refused = rooted_across(ctx, &mut [&mut this, &mut snap], |ctx| {
        try_alloc_synthetic(ctx, "java/util/TreeSet$Itr", TS_ITR_NUM_FIELDS)
    });
    let itr: ObjectRef = match refused {
        Ok(itr) => itr,
        // The snapshot is already reversed, so the real iterator walks it in
        // descending order without further work. Removal is by ELEMENT, not by
        // index, so the reversed order does not change what `remove()` deletes.
        Err(_) => {
            return real_snapshot_iterator(ctx, snap, n, Some((this, SnapshotItrRoute::TreeSet)))
        }
    };
    ctx.set_field(itr, TS_ITR_FIELD_DATA, Value::Object(Some(snap)));
    ctx.set_field(itr, TS_ITR_FIELD_CURSOR, Value::Int(0));
    ctx.set_field(itr, TS_ITR_FIELD_OWNER, Value::Object(Some(this)));
    ctx.set_field(itr, TS_ITR_FIELD_LAST_RET, Value::Int(-1));
    // After `TS_ITR_FIELD_OWNER`: that is how `ts_itr_comod_source` reaches the
    // source map whose generation this cursor is fail-fast against. Both mint
    // sites (`iterator` and `descendingIterator`) hand out this same shape.
    ts_itr_seed_expected(ctx, itr);
    Ok(Some(Value::Object(Some(itr))))
}

/// `TreeSet.descendingSet()` — a NavigableSet whose iteration order is the
/// reverse of this set. Real JDK returns `new TreeSet<>(m.descendingMap())`,
/// reading the null backing `m` → NPE. This native returns a fresh native
/// TreeSet holding the same elements ordered by the reversed comparator
/// (`Collections.reverseOrder`), so `iterator`/`first`/`last`/`contains` on the
/// result are all consistent with descending order.
///
/// It is also a WRITE-THROUGH view. `TreeSet.descendingWriteThrough` measured
///
/// ```text
/// HotSpot   3:[9, 2, 1]:[1, 2, 9]
/// CratonVM  3:[9, 2, 1]:[1, 2, 3]
/// ```
///
/// — `ds.pollFirst()` and `ds.add(9)` both answered correctly *on the view* and
/// left the backing set untouched. The mechanism is the source marker in the
/// trailing capacity slot that `ts_view_source` already defines for `TreeMap`
/// keySet views: `pollFirst`, `pollLast`, `remove`, `clear` and the iterator's
/// `remove` all consult it, so installing it here is most of the fix; only the
/// add direction and the map-vs-set removal descriptor
/// ([`ts_source_remove`]) had to be added.
///
/// The marker goes on AFTER the population loop, deliberately: installed first,
/// every `native_ts_add` below would write each element straight back into the
/// set it was copied from.
///
/// **This makes the view live in one direction only.** A write through `ds`
/// reaches `ts`; a later write to `ts` is NOT visible in `ds`, which remains
/// the snapshot it always was. The JDK's is live both ways. Closing the other
/// direction needs a rebuild-before-read funnel like `tm_sync_native_state`,
/// and `TreeSet` has no such funnel — every one of its ~40 natives reads
/// `ts_state` directly. Recorded in W7-36-differential-view-families rather
/// than half-built here.
pub(crate) fn native_ts_descending_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // GC-SAFETY: `this` is now needed at the very END of this function (it is
    // the source stored in the view marker), across a `Collections.reverseOrder`
    // dispatch, two allocations and a whole population loop. It was previously
    // dead after the first read and so was never pinned.
    let this_pin = ctx.pin_native_root(this);
    // Single-exit through a closure so no `?` can unwind past the pin above and
    // strand it (and everything pinned on top of it) — the shape
    // `native_map_put_if_absent` uses for the same reason.
    let outcome = (|| -> MethodCallResult {
        let this = ctx.read_native_pin(this_pin, this);
        let (data_opt, size, comparator) = ts_state(ctx, this);
        let rev = match comparator {
            Value::Object(Some(c)) => ctx.invoke(
                "java/util/Collections",
                "reverseOrder",
                "(Ljava/util/Comparator;)Ljava/util/Comparator;",
                &[Value::Object(Some(c))],
            )?,
            _ => ctx.invoke(
                "java/util/Collections",
                "reverseOrder",
                "()Ljava/util/Comparator;",
                &[],
            )?,
        }
        .unwrap_or(Value::Object(None));
        // `rev` is a freshly allocated comparator held in a bare Rust local
        // across the two allocations below and then STORED — the same
        // stale-at-store shape the Family-1 notes throughout this file describe.
        let rev_pin = pin_value(ctx, rev);
        let result = try_alloc_declared_width(ctx, "java/util/TreeSet", TS_NUM_FIELDS)?;
        let result_pin = ctx.pin_native_root(result);
        // One slot beyond the elements, for the source marker installed after
        // the population loop. Sized so `ts_ensure_capacity` never has to grow
        // during that loop — when the marker is not there yet to be carried.
        let cap = std::cmp::max(size as usize, TS_DEFAULT_CAPACITY) + 1;
        let buf = alloc_ref_array(ctx, cap);
        let mut result = ctx.read_native_pin(result_pin, result);
        let rev = read_pinned_elem(ctx, rev_pin, rev);
        ts_set_slot(ctx, result, TS_FIELD_DATA, Value::Object(Some(buf)));
        ts_set_slot(ctx, result, TS_FIELD_SIZE, Value::Int(0));
        ts_set_slot(ctx, result, TS_FIELD_COMPARATOR, rev);
        if let Some(data) = data_opt {
            // Family-1 stale-ObjectRef fix (2026-07-31): `native_ts_add` allocates
            // (it grows the destination array and can dispatch a Comparator), so
            // both the source array and the result move under this loop.
            let data_pin = ctx.pin_native_root(data);
            let mut data = data;
            for i in 0..(size as usize) {
                let e = ctx.get_array_element(data, i);
                native_ts_add(ctx, &[Value::Object(Some(result)), e])?;
                data = ctx.read_native_pin(data_pin, data);
                result = ctx.read_native_pin(result_pin, result);
            }
            // Left pinned: the caller's `unpin_native_roots(this_pin)` releases
            // this frame's whole pin range, and unpinning `data_pin` here would
            // drop `result_pin` with it (it sits above).
        }
        // Install the live-view marker last. Nothing allocates from here to the
        // return, so both refs read through their pins stay valid.
        let this = ctx.read_native_pin(this_pin, this);
        let result = ctx.read_native_pin(result_pin, result);
        let (rdata, rsize, _) = ts_state(ctx, result);
        if let Some(rd) = rdata {
            let dlen = ctx.array_length(rd);
            if (dlen as i32) > rsize {
                ctx.set_array_element(rd, dlen - 1, Value::Object(Some(this)));
            }
        }
        Ok(Some(Value::Object(Some(result))))
    })();
    ctx.unpin_native_roots(this_pin);
    outcome
}

/// `TreeSet.pollFirst()` — remove and return the lowest element (null if empty).
/// Real JDK reads the null backing `m` (`m.pollFirstEntry()`) → NPE.
pub(crate) fn native_ts_poll_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data_opt, size, _) = ts_state(ctx, this);
    let data = match data_opt {
        Some(d) if size > 0 => d,
        _ => return Ok(Some(Value::Object(None))),
    };
    let first = ctx.get_array_element(data, 0);
    ts_remove_at(ctx, data, size, 0);
    ts_set_slot(ctx, this, TS_FIELD_SIZE, Value::Int(size - 1));
    ts_note_pair_remove(&*ctx, this, size, 0);
    // Live TreeMap keySet view: delete the key from the source TreeMap too.
    if let Some(source) = ts_view_source(ctx, this) {
        ts_source_remove(ctx, source, first)?;
    }
    Ok(Some(first))
}

/// `TreeSet.pollLast()` — remove and return the highest element (null if empty).
/// Real JDK reads the null backing `m` (`m.pollLastEntry()`) → NPE.
pub(crate) fn native_ts_poll_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data_opt, size, _) = ts_state(ctx, this);
    let data = match data_opt {
        Some(d) if size > 0 => d,
        _ => return Ok(Some(Value::Object(None))),
    };
    let idx = (size - 1) as usize;
    let last = ctx.get_array_element(data, idx);
    ts_remove_at(ctx, data, size, idx);
    ts_set_slot(ctx, this, TS_FIELD_SIZE, Value::Int(size - 1));
    ts_note_pair_remove(&*ctx, this, size, idx);
    if let Some(source) = ts_view_source(ctx, this) {
        ts_source_remove(ctx, source, last)?;
    }
    Ok(Some(last))
}

// ===========================================================================
// Registration
// ===========================================================================

pub(crate) fn register_tree_map_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/util/TreeMap";
    registry.register(c, "<init>", "()V", native_tm_init);
    registry.register(
        c,
        "<init>",
        "(Ljava/util/Comparator;)V",
        native_tm_init_comparator,
    );
    registry.register(
        c,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_put_entry,
    );
    registry.register(
        c,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_get,
    );
    registry.register(
        c,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_remove,
    );
    registry.register(
        c,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_tm_contains_key,
    );
    registry.register(
        c,
        "containsValue",
        "(Ljava/lang/Object;)Z",
        native_tm_contains_value,
    );
    registry.register(c, "size", "()I", native_tm_size);
    registry.register(c, "isEmpty", "()Z", native_tm_is_empty);
    registry.register(c, "clear", "()V", native_tm_clear);
    registry.register(c, "firstKey", "()Ljava/lang/Object;", native_tm_first_key);
    registry.register(c, "lastKey", "()Ljava/lang/Object;", native_tm_last_key);
    registry.register(
        c,
        "ceilingKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_ceiling_key,
    );
    registry.register(
        c,
        "floorKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_floor_key,
    );
    registry.register(
        c,
        "higherKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_higher_key,
    );
    registry.register(
        c,
        "lowerKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_lower_key,
    );
    registry.register(
        c,
        "ceilingEntry",
        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
        native_tm_ceiling_entry,
    );
    registry.register(
        c,
        "floorEntry",
        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
        native_tm_floor_entry,
    );
    registry.register(
        c,
        "higherEntry",
        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
        native_tm_higher_entry,
    );
    registry.register(
        c,
        "lowerEntry",
        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
        native_tm_lower_entry,
    );
    registry.register(
        c,
        "firstEntry",
        "()Ljava/util/Map$Entry;",
        native_tm_first_entry,
    );
    registry.register(
        c,
        "lastEntry",
        "()Ljava/util/Map$Entry;",
        native_tm_last_entry,
    );
    registry.register(
        c,
        "pollFirstEntry",
        "()Ljava/util/Map$Entry;",
        native_tm_poll_first_entry,
    );
    registry.register(
        c,
        "pollLastEntry",
        "()Ljava/util/Map$Entry;",
        native_tm_poll_last_entry,
    );
    registry.register(c, "keySet", "()Ljava/util/Set;", native_tm_key_set);
    registry.register(c, "values", "()Ljava/util/Collection;", native_tm_values);
    registry.register(c, "entrySet", "()Ljava/util/Set;", native_tm_entry_set);
    registry.register(
        c,
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        native_tm_for_each,
    );
    registry.register(
        c,
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_get_or_default,
    );
    registry.register(
        c,
        "putIfAbsent",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_put_if_absent,
    );
    registry.register(c, "putAll", "(Ljava/util/Map;)V", native_tm_put_all);
    registry.register(c, "toString", "()Ljava/lang/String;", native_tm_to_string);
    // UNREGISTERED until 2026-08-28, so real bytecode walked the red-black tree
    // this VM never builds -- `replace`/`replaceAll`/`remove(k,v)` all reported
    // success and did nothing. The generic bodies serve a `TreeMap` receiver
    // now that `native_map_put` routes (see its comment); registering them here
    // is what stops the JDK's own overrides from running instead.
    registry.register(
        c,
        "replaceAll",
        "(Ljava/util/function/BiFunction;)V",
        native_map_replace_all,
    );
    registry.register(
        c,
        "compute",
        "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        native_map_compute,
    );
    registry.register(
        c,
        "computeIfPresent",
        "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        native_map_compute_if_present,
    );
    register_map_conditional_mutators(registry, c);
    // `TreeMap(Map)` is `putAll(m)`, which reaches the `Map` interface native
    // and worked; `TreeMap(SortedMap)` is `buildFromSorted`, which writes
    // `root` directly and produced an EMPTY map
    // (apps/probes/TreeShadowSweep 220). A shim is only ever as good as the JDK path
    // that happens to route through it, so both are registered.
    registry.register(c, "<init>", "(Ljava/util/Map;)V", native_tm_init_from_map);
    registry.register(
        c,
        "<init>",
        "(Ljava/util/SortedMap;)V",
        native_tm_init_from_sorted_map,
    );
    registry.register(
        c,
        "comparator",
        "()Ljava/util/Comparator;",
        native_tm_comparator,
    );
    registry.register(
        c,
        "headMap",
        "(Ljava/lang/Object;)Ljava/util/SortedMap;",
        native_tm_head_map,
    );
    registry.register(
        c,
        "tailMap",
        "(Ljava/lang/Object;)Ljava/util/SortedMap;",
        native_tm_tail_map,
    );
    registry.register(
        c,
        "subMap",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/SortedMap;",
        native_tm_sub_map,
    );
    registry.register(
        c,
        "headMap",
        "(Ljava/lang/Object;Z)Ljava/util/NavigableMap;",
        native_tm_head_map_inclusive,
    );
    registry.register(
        c,
        "tailMap",
        "(Ljava/lang/Object;Z)Ljava/util/NavigableMap;",
        native_tm_tail_map_inclusive,
    );
    registry.register(
        c,
        "subMap",
        "(Ljava/lang/Object;ZLjava/lang/Object;Z)Ljava/util/NavigableMap;",
        native_tm_sub_map_inclusive,
    );
    // W7-1 family 1: these three had NO registration, so they reached the real
    // `TreeMap` bytecode, which navigates the `root` field a natively-managed
    // TreeMap never populates — `descendingMap()` answered `{}` and
    // `descendingKeySet()` answered `[]`.
    registry.register(
        c,
        "descendingMap",
        "()Ljava/util/NavigableMap;",
        native_tm_descending_map,
    );
    registry.register(
        c,
        "descendingKeySet",
        "()Ljava/util/NavigableSet;",
        native_tm_descending_key_set,
    );
    registry.register(
        c,
        "navigableKeySet",
        "()Ljava/util/NavigableSet;",
        native_tm_key_set,
    );
    registry.register(
        c,
        "computeIfAbsent",
        "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;",
        native_tm_compute_if_absent,
    );
    registry.register(
        c,
        "merge",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        native_tm_merge,
    );
    registry.register(
        c,
        "iterator",
        "()Ljava/util/Iterator;",
        native_tm_key_iterator,
    );

    // Iterator for TreeMap keys
    let ki = "java/util/TreeMap$KeyItr";
    registry.register(ki, "hasNext", "()Z", native_snapshot_itr_has_next);
    registry.register(ki, "next", "()Ljava/lang/Object;", native_snapshot_itr_next);

    // SortedMap/NavigableMap interface dispatch
    let sm = "java/util/SortedMap";
    registry.register(sm, "firstKey", "()Ljava/lang/Object;", native_tm_first_key);
    registry.register(sm, "lastKey", "()Ljava/lang/Object;", native_tm_last_key);
    registry.register(
        sm,
        "comparator",
        "()Ljava/util/Comparator;",
        native_tm_comparator,
    );
    registry.register(
        sm,
        "headMap",
        "(Ljava/lang/Object;)Ljava/util/SortedMap;",
        native_tm_head_map,
    );
    registry.register(
        sm,
        "tailMap",
        "(Ljava/lang/Object;)Ljava/util/SortedMap;",
        native_tm_tail_map,
    );
    registry.register(
        sm,
        "subMap",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/SortedMap;",
        native_tm_sub_map,
    );
    let nm = "java/util/NavigableMap";
    registry.register(
        nm,
        "ceilingKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_ceiling_key,
    );
    registry.register(
        nm,
        "floorKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_floor_key,
    );
    registry.register(
        nm,
        "higherKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_higher_key,
    );
    registry.register(
        nm,
        "lowerKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_lower_key,
    );
    registry.register(
        nm,
        "ceilingEntry",
        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
        native_tm_ceiling_entry,
    );
    registry.register(
        nm,
        "floorEntry",
        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
        native_tm_floor_entry,
    );
    registry.register(
        nm,
        "higherEntry",
        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
        native_tm_higher_entry,
    );
    registry.register(
        nm,
        "lowerEntry",
        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
        native_tm_lower_entry,
    );
    registry.register(
        nm,
        "firstEntry",
        "()Ljava/util/Map$Entry;",
        native_tm_first_entry,
    );
    registry.register(
        nm,
        "lastEntry",
        "()Ljava/util/Map$Entry;",
        native_tm_last_entry,
    );
    registry.register(
        nm,
        "pollFirstEntry",
        "()Ljava/util/Map$Entry;",
        native_tm_poll_first_entry,
    );
    registry.register(
        nm,
        "pollLastEntry",
        "()Ljava/util/Map$Entry;",
        native_tm_poll_last_entry,
    );
    registry.register(
        nm,
        "headMap",
        "(Ljava/lang/Object;Z)Ljava/util/NavigableMap;",
        native_tm_head_map_inclusive,
    );
    registry.register(
        nm,
        "tailMap",
        "(Ljava/lang/Object;Z)Ljava/util/NavigableMap;",
        native_tm_tail_map_inclusive,
    );
    registry.register(
        nm,
        "subMap",
        "(Ljava/lang/Object;ZLjava/lang/Object;Z)Ljava/util/NavigableMap;",
        native_tm_sub_map_inclusive,
    );
    registry.register(
        nm,
        "descendingMap",
        "()Ljava/util/NavigableMap;",
        native_tm_descending_map,
    );
    registry.register(
        nm,
        "descendingKeySet",
        "()Ljava/util/NavigableSet;",
        native_tm_descending_key_set,
    );
    registry.register(
        nm,
        "navigableKeySet",
        "()Ljava/util/NavigableSet;",
        native_tm_key_set,
    );
    registry.set_category(__prev_cat);
}

pub(crate) fn register_tree_set_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    // Every registration below is mirrored onto `java/util/TreeMap$KeySet`, the
    // class `native_tm_key_set` mints a TreeMap keySet view under since
    // 2026-08-13 (HotSpot: `treeMap.keySet().getClass()` is
    // `java.util.TreeMap$KeySet`, not `java.util.TreeSet`). The view is a
    // TreeSet-SHAPED object under its own class — its state is in the
    // `ts_array_table` side-table, not in fields — so it needs the identical
    // native surface, and `force_native_over_real_jdk_bytecode` carries the name
    // so the JDK's own `KeySet` bodies (which read `m`, null here) never run.
    for c in ["java/util/TreeSet", "java/util/TreeMap$KeySet"] {
        registry.register(c, "<init>", "()V", native_ts_init);
        registry.register(
            c,
            "<init>",
            "(Ljava/util/Comparator;)V",
            native_ts_init_comparator,
        );
        registry.register(
            c,
            "<init>",
            "(Ljava/util/Collection;)V",
            native_ts_init_collection,
        );
        // `add`/`addAll` are the ONE place the two carriers' contracts diverge,
        // so they are the one place the surface must not be identical. A
        // `TreeSet` may be added to; a `TreeMap$KeySet` is a VIEW and
        // `Set.add` on it throws `UnsupportedOperationException` -- there is no
        // mapping to invent a value for. Sharing `native_ts_add` let
        // `treeMap.keySet().add("z")` SUCCEED, inserting a key into the view's
        // side-table that the backing map never learned about (MEASURED against
        // HotSpot 25.0.3+9, `probes/IoSystemSweep.java`, both modes).
        //
        // `remove` deliberately stays shared: removing THROUGH a keySet view is
        // legal and writes through, which is exactly what `native_ts_remove`
        // does over the shared side-table.
        //
        // This is the `CopyOnWriteArraySet`/`HashSet` shape recorded in
        // `a-vm-collection-can-share-a-native-surface-with-the-opposite-contract`:
        // a view class sharing a native surface with a class whose contract is
        // the opposite on one method.
        if c == "java/util/TreeMap$KeySet" {
            registry.register(
                c,
                "add",
                "(Ljava/lang/Object;)Z",
                native_view_add_unsupported,
            );
        } else {
            registry.register(c, "add", "(Ljava/lang/Object;)Z", native_ts_add);
        }
        registry.register(c, "remove", "(Ljava/lang/Object;)Z", native_ts_remove);
        // Serialization: drive the stream from `ts_state` so a native TreeSet
        // round-trips byte-correct (the inherited real methods go through the
        // never-populated backing `m` TreeMap → an empty set).
        registry.register(
            c,
            "writeObject",
            "(Ljava/io/ObjectOutputStream;)V",
            native_ts_write_object,
        );
        registry.register(
            c,
            "readObject",
            "(Ljava/io/ObjectInputStream;)V",
            native_ts_read_object,
        );
        registry.register(c, "contains", "(Ljava/lang/Object;)Z", native_ts_contains);
        registry.register(c, "size", "()I", native_ts_size);
        registry.register(c, "isEmpty", "()Z", native_ts_is_empty);
        registry.register(c, "clear", "()V", native_ts_clear);
        registry.register(c, "first", "()Ljava/lang/Object;", native_ts_first);
        registry.register(c, "last", "()Ljava/lang/Object;", native_ts_last);
        registry.register(
            c,
            "ceiling",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            native_ts_ceiling,
        );
        registry.register(
            c,
            "floor",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            native_ts_floor,
        );
        registry.register(
            c,
            "higher",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            native_ts_higher,
        );
        registry.register(
            c,
            "lower",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            native_ts_lower,
        );
        registry.register(c, "iterator", "()Ljava/util/Iterator;", native_ts_iterator);
        registry.register(
            c,
            "forEach",
            "(Ljava/util/function/Consumer;)V",
            native_ts_for_each,
        );
        registry.register(c, "toArray", "()[Ljava/lang/Object;", native_ts_to_array);
        // UNREGISTERED until 2026-08-28, so real `TreeSet.clone()` bytecode ran
        // `clone.m = new TreeMap<>(m)` against the ONE field a real `TreeSet`
        // declares -- which this array-backed family never populates. The result
        // was not a wrong answer but a hard stop:
        // `NullPointerException: Cannot invoke "java.util.SortedMap.comparator()"
        // because "m" is null`, from `TreeMap.<init>`.
        registry.register(c, "clone", "()Ljava/lang/Object;", native_ts_clone);
        registry.register(c, "toString", "()Ljava/lang/String;", native_ts_to_string);
        registry.register(
            c,
            "comparator",
            "()Ljava/util/Comparator;",
            native_ts_comparator,
        );
        registry.register(
            c,
            "headSet",
            "(Ljava/lang/Object;)Ljava/util/SortedSet;",
            native_ts_head_set_view,
        );
        registry.register(
            c,
            "tailSet",
            "(Ljava/lang/Object;)Ljava/util/SortedSet;",
            native_ts_tail_set_view,
        );
        registry.register(
            c,
            "subSet",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/SortedSet;",
            native_ts_sub_set_view,
        );
        // NavigableSet range views (inclusive flags). The inherited bytecode reads
        // the never-populated backing `m` TreeMap and NPEs; drive them from the
        // side-table. (PMRPR's jar-entry scan calls `tailSet(entry, false)`.)
        registry.register(
            c,
            "tailSet",
            "(Ljava/lang/Object;Z)Ljava/util/NavigableSet;",
            native_ts_tail_set_inclusive_view,
        );
        registry.register(
            c,
            "headSet",
            "(Ljava/lang/Object;Z)Ljava/util/NavigableSet;",
            native_ts_head_set_inclusive_view,
        );
        registry.register(
            c,
            "subSet",
            "(Ljava/lang/Object;ZLjava/lang/Object;Z)Ljava/util/NavigableSet;",
            native_ts_sub_set_inclusive_view,
        );
        if c == "java/util/TreeMap$KeySet" {
            // See the `add` note above: `addAll` on a keySet view is the same
            // refusal, and the JDK reaches it through `AbstractCollection
            // .addAll` calling `add` -- per element, so a null / empty argument
            // answers before any `add` (`native_tree_key_set_add_all`).
            registry.register(
                c,
                "addAll",
                "(Ljava/util/Collection;)Z",
                native_tree_key_set_add_all,
            );
        } else {
            registry.register(c, "addAll", "(Ljava/util/Collection;)Z", native_ts_add_all);
        }
        registry.register(c, "stream", "()Ljava/util/stream/Stream;", native_ts_stream);
        // `spliterator()` for the same reason `stream()` is here: both classes
        // declare it, and the JDK body goes through `TreeMap.keySpliteratorFor`
        // over fields our layout keeps in the side-table. Without this,
        // `treeSet.spliterator()` and `treeMap.keySet().spliterator()` threw
        // `NullPointerException: ... "java.util.TreeMap$NavigableSubMap
        // .keySpliterator()" because "sm" is null` — and `spliterator` was
        // already in `force_native_over_real_jdk_bytecode` for
        // `java/util/TreeMap$KeySet`, which does nothing without a native to
        // prefer.
        registry.register(
            c,
            "spliterator",
            "()Ljava/util/Spliterator;",
            native_ts_spliterator,
        );
        // Descending/poll views — read the backing `m` TreeMap in real JDK, which
        // CratonVM's native TreeSet never populates (state lives in the side-table),
        // so the inherited bytecode NPEs on a null `m`. Drive them from `ts_state`.
        registry.register(
            c,
            "descendingIterator",
            "()Ljava/util/Iterator;",
            native_ts_descending_iterator,
        );
        registry.register(
            c,
            "descendingSet",
            "()Ljava/util/NavigableSet;",
            native_ts_descending_set,
        );
        registry.register(c, "pollFirst", "()Ljava/lang/Object;", native_ts_poll_first);
        registry.register(c, "pollLast", "()Ljava/lang/Object;", native_ts_poll_last);
    }

    // TreeSet iterator
    let ti = "java/util/TreeSet$Itr";
    registry.register(ti, "hasNext", "()Z", native_snapshot_itr_has_next);
    registry.register(ti, "next", "()Ljava/lang/Object;", native_snapshot_itr_next);
    // `Iterator.remove()` is a supported operation for TreeSet's iterator —
    // delete the last-returned element from the owning set (field 2).
    registry.register(ti, "remove", "()V", native_ts_itr_remove);

    // SortedSet/NavigableSet interface dispatch
    let ss = "java/util/SortedSet";
    registry.register(ss, "first", "()Ljava/lang/Object;", native_ts_first);
    registry.register(ss, "last", "()Ljava/lang/Object;", native_ts_last);
    registry.register(
        ss,
        "comparator",
        "()Ljava/util/Comparator;",
        native_ts_comparator,
    );
    registry.register(
        ss,
        "headSet",
        "(Ljava/lang/Object;)Ljava/util/SortedSet;",
        native_ts_head_set,
    );
    registry.register(
        ss,
        "tailSet",
        "(Ljava/lang/Object;)Ljava/util/SortedSet;",
        native_ts_tail_set,
    );
    registry.register(
        ss,
        "subSet",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/SortedSet;",
        native_ts_sub_set,
    );
    let ns = "java/util/NavigableSet";
    registry.register(
        ns,
        "ceiling",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_ts_ceiling,
    );
    registry.register(
        ns,
        "floor",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_ts_floor,
    );
    registry.register(
        ns,
        "higher",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_ts_higher,
    );
    registry.register(
        ns,
        "lower",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_ts_lower,
    );
    registry.register(
        ns,
        "descendingIterator",
        "()Ljava/util/Iterator;",
        native_ts_descending_iterator,
    );
    registry.register(
        ns,
        "descendingSet",
        "()Ljava/util/NavigableSet;",
        native_ts_descending_set,
    );
    registry.register(
        ns,
        "pollFirst",
        "()Ljava/lang/Object;",
        native_ts_poll_first,
    );
    registry.register(ns, "pollLast", "()Ljava/lang/Object;", native_ts_poll_last);
    registry.set_category(__prev_cat);
}

