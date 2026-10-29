// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

// ===========================================================================
// ArrayList — synthetic-jdk layout: field 0 = Object[] elementData, field 1 = Int size
// Real-JDK layout: AbstractList.modCount(I) at slot 0, ArrayList.elementData at slot 1,
// ArrayList.size at slot 2. We resolve the real layout via field-index lookup and fall
// back to the synthetic layout when the real class is unavailable.
// ===========================================================================

pub(crate) const AL_FIELD_DATA: usize = 0;
pub(crate) const AL_FIELD_SIZE: usize = 1;
pub(crate) const AL_NUM_FIELDS: usize = 2;
pub(crate) const AL_DEFAULT_CAPACITY: usize = 10;

/// Resolve ArrayList field slots: returns (data_slot, size_slot, n_fields).
/// In real-JDK mode this follows the actual `elementData` / `size` field
/// indices (with `modCount` from `AbstractList` taking slot 0); in
/// synthetic-jdk mode it falls back to (0, 1, 2). The result is cheap to
/// compute (the underlying resolver caches by class), so we recompute it on
/// each access rather than caching globally.
#[inline]
pub(crate) fn al_slots(ctx: &dyn NativeContext) -> (usize, usize, usize) {
    al_slots_resolved(ctx).unwrap_or((AL_FIELD_DATA, AL_FIELD_SIZE, AL_NUM_FIELDS))
}

/// `al_slots`, but reporting whether the layout was actually RESOLVED rather
/// than defaulted. `al_slots_for`'s memo caches only `Some`: a
/// `resolve_field_index` that fails today because the class is not loaded yet
/// can succeed later, and caching the constant fallback would pin the wrong
/// layout for the rest of the run. Same argument `class_name_rc` makes for not
/// caching its `None`.
pub(crate) fn al_slots_resolved(ctx: &dyn NativeContext) -> Option<(usize, usize, usize)> {
    let data = ctx.resolve_field_index("java/util/ArrayList", "elementData")?;
    let size = ctx.resolve_field_index("java/util/ArrayList", "size")?;
    Some((data, size, std::cmp::max(data, size) + 1))
}

/// Receiver-aware slot resolution for the shared ArrayList-backed list natives.
///
/// `java.util.Vector` (and its subclass `java.util.Stack`) are registered against
/// the same `native_al_*` implementations, but Vector is **not** an ArrayList and
/// has a different field layout: `elementData` + `elementCount` (NOT `size`) +
/// `capacityIncrement`. Using ArrayList's `(elementData, size)` slots on a Vector
/// reads/writes the wrong fields (and the layout guard rejects it), so every
/// Vector/Stack mutation silently no-ops (DF08: `Vector.add`/`Stack.push` lost
/// the element → Xerces external-DTD entity stack empty → "Premature end of
/// file"). Resolve against the receiver's actual class instead.
pub(crate) fn al_slots_for(ctx: &dyn NativeContext, this: ObjectRef) -> (usize, usize, usize) {
    al_slots_and_layout_for(ctx, this).0
}

/// [`al_slots_for`] with the receiver's [`AlLayout`] verdict, from the same
/// memo entry. Callers that need both — `al_state` does, on every operation —
/// must use this rather than asking twice.
pub(crate) fn al_slots_and_layout_for(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> ((usize, usize, usize), AlLayout) {
    al_slots_and_layout_for_class(ctx, ctx.class_id_of_object(this))
}

/// [`al_slots_and_layout_for`] for a caller that already holds the receiver's
/// `ClassId` — [`al_fast_state`] needs it for the class-facts test as well, and
/// `class_id_of_object` is a checked heap read, not a free field load.
pub(crate) fn al_slots_and_layout_for_class(
    ctx: &dyn NativeContext,
    cid: ClassId,
) -> ((usize, usize, usize), AlLayout) {
    let raw = cid.as_u32();
    let vm = ctx.vm_identity();
    let index = facts_slot_index(raw) & (AL_SLOTS_WAYS - 1);

    let hit = AL_SLOTS.with(|cell| {
        let mut cache = cell.borrow_mut();
        if cache.0 != Some(vm) {
            *cache = (Some(vm), [None; AL_SLOTS_WAYS]);
        }
        match cache.1[index] {
            Some((cached_id, slots)) if cached_id == raw => Some(slots),
            _ => None,
        }
    });
    if let Some(slots) = hit {
        return slots;
    }

    // Cold. The borrow above is released first: the resolution below re-enters
    // the VM, which must never find this `RefCell` already borrowed.
    let resolved = al_slots_for_uncached(ctx, cid);
    if let Some(slots) = resolved {
        AL_SLOTS.with(|cell| {
            let mut cache = cell.borrow_mut();
            if cache.0 == Some(vm) {
                cache.1[index] = Some((raw, slots));
            }
        });
        return slots;
    }
    (
        (AL_FIELD_DATA, AL_FIELD_SIZE, AL_NUM_FIELDS),
        AlLayout::Lenient,
    )
}

/// Class facts that send a receiver down one of the wrapper/view routes a
/// `native_al_*` entry point checks before it reads `elementData` itself:
/// `unwrap_unmod`/`unmod_receiver_backing` (`CF_UNMOD_WRAPPER`), `ksv_route`
/// (`CF_KEY_SET_VIEW`), and `vc_route`/`vc_route_source_size`
/// (`CF_VALUES_VIEW`, `CF_MAP_ENTRY_VIEW`). None of them can be an
/// [`AlLayout::ArrayList`] today; [`al_fast_state`] tests them anyway, from the
/// same thread-local memo, so the fast path can never answer for a receiver the
/// general path would have rerouted.
pub(crate) const AL_FAST_PATH_EXCLUDED_FACTS: u32 =
    CF_UNMOD_WRAPPER | CF_KEY_SET_VIEW | CF_VALUES_VIEW | CF_MAP_ENTRY_VIEW;

/// The hot-path `(elementData, size, elementData.length)` of an ordinary
/// `java.util.ArrayList` (or subclass), or `None` when the receiver needs ANY
/// of the general path's special handling — in which case the caller must run
/// that path unchanged.
///
/// PERF (round 9 wave 3, `perf-collection-natives-cost-a-generic-native-
/// dispatch-per-call`). `native_al_get` on a plain `ArrayList` reached its
/// element through `vc_route` -> `unmod_receiver_backing` ->
/// `singleton_wrapper_size` -> `al_state_for_read` -> `al_state` ->
/// `view_source_in`: five `class_id_of_object` reads, a `class_name_rc`
/// (`RefCell` borrow + `Rc` clone + six string compares), two `get_field`s
/// that each re-resolved the slot's descriptor, a `heap_kind_of`, an
/// `array_length`, and only then the element — every one of those a checked
/// heap access with its own membership walk (`is_object_address` was 17 % of
/// `SpliceCastProbe`). This answers the same question with ONE class-id read,
/// two thread-local memo hits, one header read, ONE batched typed field read
/// (one forwarding barrier, no descriptor lookup) and ONE `array_shape` (kind
/// and length under one membership walk).
///
/// `Some` is returned only when every route the general path checks is ruled
/// out and it would have produced exactly this pair:
///
/// * the class memo says [`AlLayout::ArrayList`] — never `Vector`, a view
///   carrier, a lenient/bare placeholder or a foreign class;
/// * none of [`AL_FAST_PATH_EXCLUDED_FACTS`] is set (unmodifiable wrapper,
///   `KeySetView`, values/entry view) — and `Collections$Singleton*`/`Empty*`
///   (`singleton_wrapper_size`) are not `ArrayList` subclasses;
/// * the receiver has both slots, `elementData` is a live REFERENCE array
///   (`null`, a primitive array, a non-array -> `None`) and `size >= 0`;
/// * the backing array carries no `values()`-view source marker in its
///   trailing capacity slot (`view_source_in` — a view the general path must
///   resync first).
///
/// The descriptors are `ArrayList`'s own declared ones (`elementData:
/// [Ljava/lang/Object;`, `size: I`), which is what `get_field` would have
/// looked up for these slots; that is the `get_field_typed` contract.
#[inline]
pub(crate) fn al_fast_state(ctx: &dyn NativeContext, this: ObjectRef) -> Option<(ObjectRef, i32, usize)> {
    let cid = ctx.class_id_of_object(this);
    let ((data_slot, size_slot, _), layout) = al_slots_and_layout_for_class(ctx, cid);
    if layout != AlLayout::ArrayList {
        return None;
    }
    if class_facts(ctx, cid).has(AL_FAST_PATH_EXCLUDED_FACTS) {
        return None;
    }
    let n_fields = ctx.object_num_fields(this);
    if n_fields <= data_slot || n_fields <= size_slot {
        return None;
    }
    let mut out = [Value::Object(None), Value::Int(0)];
    ctx.get_fields_typed(this, &[(data_slot, b'['), (size_slot, b'I')], &mut out);
    let data = match out[0] {
        Value::Object(Some(arr)) => arr,
        _ => return None,
    };
    let size = match out[1] {
        Value::Int(s) if s >= 0 => s,
        _ => return None,
    };
    let (elem_ty, len) = ctx.array_shape(data)?;
    if elem_ty != ArrayElementType::Reference {
        return None;
    }
    // `view_source_in`'s marker test, verbatim: a trailing non-null slot past
    // the logical size names a source map this list is a stale view of.
    if len > size as usize {
        if let Value::Object(Some(_)) = ctx.get_array_element(data, len - 1) {
            return None;
        }
    }
    Some((data, size, len))
}

/// Ways in the direct-mapped receiver-class -> list-slot-layout cache.
///
/// `al_slots_for` sits on EVERY `native_al_*` operation and re-derived a
/// constant each time: a string-keyed `class_id_by_name`, an `is_subclass`
/// walk, and two string-keyed `resolve_field_index` calls, all behind the
/// class-manager lock. `native_al_size` reaches it twice per call. Measured
/// before this memo, on an idle box: `ArrayList.size()` 1014 ns/op against
/// 28 ns/op for the identical method body written in plain Java on the same VM;
/// netty's `AhoCorasicSearchProcessorFactory.buildTrie` — 6.05 M ArrayList
/// native calls — took 217 s where HotSpot takes 0.214 s.
///
/// Sound for the same reason as `RECEIVER_FACTS` and `RECEIVER_NAMES`: a
/// `ClassId` is never reissued, and a class's field layout is immutable after
/// linking.
pub(crate) const AL_SLOTS_WAYS: usize = 64;

thread_local! {
    /// `(vm_identity, [(ClassId, ((data, size, n), layout)); WAYS])`.
    static AL_SLOTS: std::cell::RefCell<(
        Option<usize>,
        [Option<(u32, ((usize, usize, usize), AlLayout))>; AL_SLOTS_WAYS],
    )> = const { std::cell::RefCell::new((None, [None; AL_SLOTS_WAYS])) };
}

/// What the receiver's CLASS says about its list layout — the class-only half
/// of [`al_is_list_layout`], resolved once per `ClassId` and cached beside the
/// slot layout in [`AL_SLOTS`].
///
/// PERF (2026-08-13, the `arraylist-native-overhead` residual). The predicate
/// this replaces ran `class_id_by_name("java/util/ArrayList")` followed by
/// `is_subclass` on EVERY `al_state`, and a second such pair for
/// `java/util/Vector` whenever the first said no — each of them a
/// `class_manager` read lock plus a name-hash lookup over a 19-byte string.
/// `native_al_size` reached `al_state` twice, so one `ArrayList.size()` paid
/// two of them. A flat `perf record -F 499` of a `size()`-only loop put
/// `class_id_by_name` + `find_unique_class_by_name` + `classify_exact_name` at
/// **15.9% of the whole run** — the largest family in that profile, ahead of
/// every dispatch symbol.
///
/// Sound for the same reason as [`AL_SLOTS`] itself and `RECEIVER_FACTS`: a
/// `ClassId` is never reissued and a class's ancestry is immutable after
/// linking. Like `AL_SLOTS` it is only ever FILLED with a resolved verdict —
/// while `java/util/ArrayList` is not loaded the old predicate answered its
/// lenient `true` for every receiver, and freezing that into the memo would
/// keep answering it after ArrayList arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AlLayout {
    /// `java/util/ArrayList` or a subclass — the ordinary case.
    ArrayList,
    /// `java/util/Vector` or a subclass (`java/util/Stack`), which carries its
    /// own `elementData`/`elementCount` slots.
    Vector,
    /// One of the [`MAP_VIEW_CARRIERS`]. Reads and writes exactly like
    /// [`AlLayout::ArrayList`] — the difference is the slot triple it is paired
    /// with, which [`view_carrier_slots`] places above the carrier's own
    /// declared fields instead of at ArrayList's absolute indices.
    ViewCarrier,
    /// Exactly `java/lang/Object`. CratonVM's own class-id-0 placeholders land
    /// here; a zero-field bare `Object` is not a list. See
    /// [`is_bare_java_lang_object`], whose test this reproduces.
    BareObject,
    /// Nameless or unnamed class — the lenient arm the original carried so
    /// bare synthetic allocations keep working.
    Lenient,
    /// A known, named, non-list class (`String`, `LinkedList`, a JDK bean).
    /// Reading ArrayList's slots out of one of these is the wild-read the
    /// guard exists to prevent — see [`al_state`].
    Foreign,
}

impl AlLayout {
    /// `true` when the receiver's class alone rules out every wrapper/view
    /// route a `native_al_*` entry point checks before doing its own work.
    ///
    /// `ksv_route` asks whether the receiver is a
    /// `ConcurrentHashMap$KeySetView`; `unmod_receiver_backing` whether it is
    /// one of the `cratonvm/internal/Unmodifiable*` views. Neither can be an
    /// `ArrayList`, a `Vector` or a map-view carrier — they are named classes
    /// with their own layouts, so they classify as [`AlLayout::Foreign`]. Every
    /// `native_al_*` entry point ran both anyway, on every ordinary list, at a
    /// combined 3.5% of a flat `ArrayList.size()` profile (`ksv_route` 1.3%,
    /// `unmod_receiver_backing` 1.0%, and the `class_name_rc` it drives 1.3%).
    /// This answers both from the memo the operation is about to consult for
    /// its slot layout anyway.
    #[inline]
    pub(crate) fn rules_out_wrapper_routes(self) -> bool {
        matches!(
            self,
            AlLayout::ArrayList | AlLayout::Vector | AlLayout::ViewCarrier
        )
    }

    /// The verdict [`al_is_list_layout`] returns for an object of this class.
    /// `n_fields` is the receiver's own field count, which every caller has
    /// already read for the slot bounds check.
    #[inline]
    pub(crate) fn accepts(self, n_fields: usize) -> bool {
        match self {
            AlLayout::ArrayList | AlLayout::Vector | AlLayout::ViewCarrier | AlLayout::Lenient => {
                true
            }
            // `!is_bare_java_lang_object`: exact `java/lang/Object` with zero
            // fields is rejected; one that carries fields is a CratonVM
            // placeholder list and stays accepted.
            AlLayout::BareObject => n_fields != 0,
            AlLayout::Foreign => false,
        }
    }
}

/// The uncached body of [`al_slots_for`]. `None` when the layout could not be
/// resolved, so the caller can apply the constant fallback WITHOUT caching it.
pub(crate) fn al_slots_for_uncached(
    ctx: &dyn NativeContext,
    cid: ClassId,
) -> Option<((usize, usize, usize), AlLayout)> {
    if let Some(vec_id) = ctx.class_id_by_name("java/util/Vector") {
        if cid == vec_id || ctx.is_subclass(cid, vec_id) {
            if let (Some(d), Some(s)) = (
                ctx.resolve_field_index("java/util/Vector", "elementData"),
                ctx.resolve_field_index("java/util/Vector", "elementCount"),
            ) {
                return Some(((d, s, std::cmp::max(d, s) + 1), AlLayout::Vector));
            }
            // A Vector receiver whose own layout is unresolvable must NOT fall
            // through to ArrayList's `size` slot — that is the DF08 mixup this
            // function exists to prevent. Report unresolved instead.
            return None;
        }
    }
    // Same argument as Vector, one family over: a `Map.values()` view carrier
    // is not an ArrayList either, and using ArrayList's ABSOLUTE slots on one
    // overwrites a field the carrier really declares. See
    // [`view_carrier_slots`] for the collision this prevents.
    if let Some(name) = ctx.class_name_arc_of_id(cid) {
        if is_map_view_carrier(&name) {
            return Some((view_carrier_slots(ctx, cid)?, AlLayout::ViewCarrier));
        }
    }
    let slots = al_slots_resolved(ctx)?;
    // Refuse to CACHE a verdict until `java/util/ArrayList` itself resolves:
    // the arm below this one is the old predicate's lenient "ArrayList not
    // loaded (synthetic-jdk) — preserve old behavior" answer, and it stops
    // being true the moment ArrayList loads. The uncached fallback in
    // `al_slots_for` reproduces it meanwhile.
    let al_id = ctx.class_id_by_name("java/util/ArrayList")?;
    let layout = if cid == al_id || ctx.is_subclass(cid, al_id) {
        AlLayout::ArrayList
    } else {
        // Not an ArrayList: reject only when the class is known and named.
        match ctx.class_name_arc_of_id(cid).as_deref() {
            None => AlLayout::Lenient,
            Some("") => AlLayout::Lenient,
            Some("java/lang/Object") => AlLayout::BareObject,
            // A map view carrier never reaches here: the early return above
            // answers `AlLayout::ViewCarrier` for every `MAP_VIEW_CARRIERS`
            // name, with its own slot triple. (Until 2026-08-17 this arm
            // answered `AlLayout::ArrayList` and the carrier shared
            // ArrayList's ABSOLUTE slots — the `LinkedValues` collision.)
            Some(_) => AlLayout::Foreign,
        }
    };
    Some((slots, layout))
}

/// `true` iff `obj` has a list layout the `native_al_*` natives can read at the
/// receiver-aware slots — ArrayList (or subclass) **or** Vector (or subclass,
/// e.g. Stack). See [`AlLayout`] for the foreign-receiver rationale and for why
/// the answer is memoized per `ClassId` rather than re-derived per call.
pub(crate) fn al_is_list_layout(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    al_slots_and_layout_for(ctx, obj)
        .1
        .accepts(ctx.object_num_fields(obj))
}

/// Allocate an ArrayList instance, sized to fit whichever field layout the
/// runtime is using. Initializes elementData and size to (buf, init_size).
pub(crate) fn alloc_arraylist_with(
    ctx: &mut dyn NativeContext,
    buf: ObjectRef,
    init_size: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let (data_slot, size_slot, n_fields) = al_slots(ctx);
    // Allocation can move the caller-owned backing array. Root both the
    // backing array and the new list until every field store is complete.
    let roots_base = ctx.pin_native_root(buf);
    let list = try_alloc_synthetic(ctx, "java/util/ArrayList", n_fields)?;
    let list_pin = ctx.pin_native_root(list);
    let list = ctx.read_native_pin(list_pin, list);
    let buf = ctx.read_native_pin(roots_base, buf);
    ctx.set_field(list, data_slot, Value::Object(Some(buf)));
    let list = ctx.read_native_pin(list_pin, list);
    ctx.set_field(list, size_slot, Value::Int(init_size));
    let list = ctx.read_native_pin(list_pin, list);
    ctx.unpin_native_roots(roots_base);
    Ok(list)
}

#[inline]
pub(crate) fn is_bare_java_lang_object(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    // PERF (collections-classification-cost): field count first — it is a
    // header read, and it is false for every real map, so the memo lookup is
    // usually skipped outright. `native_map_put_evict` calls this on entry.
    ctx.object_num_fields(obj) == 0 && receiver_facts(ctx, obj).has(CF_EXACT_OBJECT)
}

/// `true` iff `obj` actually has the `java.util.ArrayList` field layout, so
/// reading `elementData`/`size` at the resolved slots is sound. Real
/// `ArrayList`s (and subclasses, which inherit the layout) qualify; a known
/// non-ArrayList class (`String`, `LinkedList`, a JDK bean, …) does not.
/// Synthetic / unknown objects with no resolvable class name stay lenient —
/// CratonVM's internal placeholder lists allocate against
/// `java/util/ArrayList` (so they resolve by name and qualify), but bare
/// class-id-0 allocations must keep working as before.
///
/// The classification itself now lives in [`AlLayout`], memoized per
/// `ClassId`; this is the exact-ArrayList question on top of it, which is not
/// the same as [`al_is_list_layout`] (that one also admits `Vector`).
pub(crate) fn al_is_arraylist_layout(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    match al_slots_and_layout_for(ctx, obj).1 {
        AlLayout::ArrayList | AlLayout::ViewCarrier | AlLayout::Lenient => true,
        AlLayout::BareObject => !is_bare_java_lang_object(ctx, obj),
        AlLayout::Vector | AlLayout::Foreign => false,
    }
}

/// Extract ArrayList state: (elementData, size).
pub(crate) fn al_state(ctx: &dyn NativeContext, this: ObjectRef) -> (Option<ObjectRef>, i32) {
    // See through CratonVM's unmodifiable wrapper views. A generic
    // `java/util/List` interface native (e.g. `native_al_equals`) can be
    // dispatched with a `cratonvm/internal/UnmodifiableList` receiver — and,
    // critically, with such a wrapper as the *other* argument: e.g.
    // `List.of(...).equals(List.of(...))` delegates `equals` to the backing
    // ArrayList, which then reads the wrapper argument's slots. Slot 0 of a
    // wrapper is the backing collection ObjectRef, NOT the element array;
    // without this unwrap `al_state` sees a non-array there, reports
    // `data = None` / `size = 0`, and `equals` wrongly returns false. Reading
    // from the backing ArrayList is always correct here — all wrapper mutators
    // throw, so callers reaching `al_state` are read-only. Mirrors the same
    // unwrap that `map_state` already performs for unmodifiable maps.
    let this = unwrap_unmod(ctx, this);
    let ((data_slot, size_slot, _), layout) = al_slots_and_layout_for(ctx, this);
    // Receiver-layout guard. `al_state` is reached through `Collection`-
    // and `List`-interface natives (`size`, `forEach`, `stream`, …) whose
    // bytecode dispatcher can target *any* object — including non-list
    // receivers funnelled through reflection, lambda metafactory, or a
    // misresolved vtable.  In real-JDK mode `al_slots` returns
    // `(elementData=4, size=6)` (matching the inherited AbstractList /
    // AbstractCollection field layout); on a 3-slot receiver such as
    // `java/nio/charset/Charset` the unguarded `get_field` issues an
    // out-of-bounds slot read which the GC guard catches and the JUnit
    // bootstrap eventually segfaults on downstream.  Returning the
    // "empty list" sentinel matches the documented contract of
    // `collection_elements_generic` and lets the caller fall back to a
    // virtual-dispatch path.
    let n_fields = ctx.object_num_fields(this);
    if n_fields <= data_slot || n_fields <= size_slot {
        return (None, 0);
    }
    // Layout-class guard (defense-in-depth over the field-count check above).
    // A foreign object with >= size_slot fields (e.g. a `String`, a JDK bean)
    // passes the count check, but slots data_slot/size_slot are NOT its
    // `elementData`/`size`. Decoding such a slot as a `Value` can yield a
    // garbage discriminant, and `coerce_field_value_by_descriptor`'s match then
    // jumps through an out-of-range jump-table entry — a wild read /
    // EXCEPTION_ACCESS_VIOLATION (observed on `ArrayList.equals(nonList)` during
    // Hibernate JSON dirty-checking: HIB JSON-function SIGSEGV). Only read the
    // ArrayList slots when the receiver actually has ArrayList layout; otherwise
    // return the empty sentinel so the caller falls back to the receiver's own
    // `iterator()` (equals) / virtual dispatch.
    if !layout.accepts(n_fields) {
        return (None, 0);
    }
    let data = match ctx.get_field(this, data_slot) {
        Value::Object(Some(arr)) if ctx.heap_kind_of(arr) == ObjectKind::Array => Some(arr),
        _ => None,
    };
    let size = match ctx.get_field(this, size_slot) {
        Value::Int(s) => s,
        _ => 0,
    };
    (data, size)
}

/// Snapshot the elements of a collection whose receiver is **not** one of
/// our ArrayList-layout objects (so `al_state` reported `data = None`).
///
/// The ArrayList-shaped natives (`stream`, `forEach`, …) are registered on
/// the `Collection` / `List` / `Iterable` interfaces, so they also catch
/// foreign collections — Guava's `ImmutableList`, `Maps$Values`, etc. For
/// those, `al_state`'s hardcoded `elementData`/`size` slots read past the
/// (shorter) object and yield an empty result, silently dropping every
/// element. Instead, walk the receiver's *real* `iterator()` — which
/// dispatches to the collection's own bytecode — so the elements survive.
///
/// The 64M cap is a runaway guard; a well-behaved iterator terminates long
/// before it.
///
/// # Why this returns a `Result` and not a bare `Vec`
///
/// It used to be `-> Vec<Value>`, and both `match` arms mapped *every* failure
/// of `iterator()` / `hasNext()` / `next()` to "no more elements". Measured
/// 2026-08-11 on the pre-built binary against Temurin 25.0.3.9:
///
/// ```text
/// new ArrayList<>(List.of("a","b","c")).equals(linkedList)
/// ```
///
/// answered `true` on HotSpot and in `Compatible`, and **`false`** under
/// `--jdk-only` — silently, no exception — because the `NoClassDefFoundError:
/// java/util/LinkedList$Itr` raised by the `invoke_virtual` below arrived here
/// as `Vec::new()` and `native_al_equals` then compared 3 elements against 0.
///
/// A refusal that reaches a caller with nowhere to put it stops being a
/// refusal, which is the one failure mode contract §5 cannot tolerate: the
/// census still records the violation, so the run looks *measured* while the
/// program is handed a wrong answer. And an empty collection is the failure
/// mode that reads as a pass everywhere a caller only iterates — the same
/// shape that let four `TreeMap` navigable views answer `{}` for months
/// (docs/feature-designs/collections-interception.md).
/// Reasoned in
/// docs/feature-designs/collections-interception.md
/// and docs/feature-designs/jdk-only-mode.md
///
/// The discrimination line, which is the whole of the fix and applies to every
/// helper in this family:
///
/// * `Err(..)` — the call was *refused* or threw. Propagate. This is a policy
///   `NoClassDefFoundError` under `--jdk-only`, and in `Compatible` it is an
///   exception the receiver's own `iterator()`/`next()` raised, which HotSpot
///   also propagates out of `AbstractList.equals` rather than truncating.
/// * `Ok(..)` with nothing usable — a null iterator, a void return, a
///   primitive. The call did not fail; it gave us nothing to walk. That is the
///   documented best effort for a foreign collection whose layout we cannot
///   model, and it is unchanged.
pub(crate) fn collection_elements_generic(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<Vec<Value>, MethodCallFailed> {
    let pin_base = ctx.pin_native_root(this);
    let this_cur = ctx.read_native_pin(pin_base, this);
    let iter = match ctx.invoke_virtual(this_cur, "iterator", "()Ljava/util/Iterator;", &[]) {
        Ok(Some(Value::Object(Some(it)))) => it,
        Err(e) => {
            ctx.unpin_native_roots(pin_base);
            return Err(e);
        }
        Ok(_) => {
            ctx.unpin_native_roots(pin_base);
            return Ok(Vec::new());
        }
    };
    let iter_pin = ctx.pin_native_root(iter);
    let mut out: Vec<(Value, Option<(usize, ObjectRef)>)> = Vec::new();
    // Set when the walk was cut short by a *failure* rather than by the
    // iterator reporting done. Held in a local instead of returning early so
    // the pin unwind below stays one strictly-LIFO path — an early `return`
    // past the per-element handles is the PIN-DANGLING shape this file's
    // `chm_extra_entries` history already paid for once.
    let mut failed: Option<MethodCallFailed> = None;
    const MAX_ELEMENTS: usize = 64 * 1024 * 1024;
    while out.len() < MAX_ELEMENTS {
        let iter_cur = ctx.read_native_pin(iter_pin, iter);
        match ctx.invoke_virtual(iter_cur, "hasNext", "()Z", &[]) {
            Ok(Some(Value::Int(n))) if n != 0 => {}
            Err(e) => {
                failed = Some(e);
                break;
            }
            _ => break,
        }
        let iter_cur = ctx.read_native_pin(iter_pin, iter);
        match ctx.invoke_virtual(iter_cur, "next", "()Ljava/lang/Object;", &[]) {
            Ok(Some(v)) => {
                let pin = match v {
                    Value::Object(Some(r)) => Some((ctx.pin_native_root(r), r)),
                    _ => None,
                };
                out.push((v, pin));
            }
            Err(e) => {
                failed = Some(e);
                break;
            }
            _ => break,
        }
    }
    let refreshed = out
        .iter()
        .map(|(value, pin)| match pin {
            Some((handle, fallback)) => {
                Value::Object(Some(ctx.read_native_pin(*handle, *fallback)))
            }
            None => *value,
        })
        .collect();
    for (_, pin) in &out {
        if let Some((handle, _)) = pin {
            ctx.unpin_native_roots(*handle);
        }
    }
    ctx.unpin_native_roots(iter_pin);
    ctx.unpin_native_roots(pin_base);
    match failed {
        // The partial `refreshed` is dropped on purpose: a caller that got a
        // prefix would be right back to comparing 3 elements against 0.
        Some(e) => Err(e),
        None => Ok(refreshed),
    }
}

/// Copy `len` elements `src[src_off..]` -> `dst[dst_off..]` between two
/// `ArrayList` backing arrays, memmove-style (`src` and `dst` may be the same
/// array).
///
/// The four `ArrayList` natives that move a backing array's contents (grow,
/// `addAll`, `trimToSize`, the `remove(Object)` shift) each tried
/// `bulk_array_copy` and fell back to a per-element loop. A backing array is
/// an `Object[]`, which `bulk_array_copy` refuses by contract (primitive
/// arrays only), so the "fallback" WAS the path, every time: two receiver
/// validations and forwardings per element in the VM (round 16 lane
/// `arraylist`; `docs/feature-designs/collections-interception.md`).
///
/// Under `CRATONVM_JIT_BULK_REF_ARRAYCOPY=1` the context's reference bulk copy
/// takes it instead. Like the loop, it applies no store check -- these are
/// list backing arrays, which accept any reference -- and it writes nothing
/// when it declines, so the loop below is still the exact old behaviour.
pub(crate) fn al_copy_elements(
    ctx: &mut dyn NativeContext,
    src: ObjectRef,
    src_off: usize,
    dst: ObjectRef,
    dst_off: usize,
    len: usize,
) {
    if ctx.bulk_array_copy(src, src_off, dst, dst_off, len) {
        return;
    }
    if al_copy_refs_bulk(ctx, src, src_off, dst, dst_off, len) {
        return;
    }
    if src.as_ptr() == dst.as_ptr() && dst_off > src_off {
        for i in (0..len).rev() {
            let val = ctx.get_array_element(src, src_off + i);
            ctx.set_array_element(dst, dst_off + i, val);
        }
    } else {
        for i in 0..len {
            let val = ctx.get_array_element(src, src_off + i);
            ctx.set_array_element(dst, dst_off + i, val);
        }
    }
}

/// The opt-in reference bulk copy alone (`CRATONVM_JIT_BULK_REF_ARRAYCOPY=1`),
/// for the two shifts (`add(int, E)`, `remove(int)`) that never tried
/// `bulk_array_copy` and keep their own loops as the fallback. `false` means
/// nothing was written.
#[inline]
pub(crate) fn al_copy_refs_bulk(
    ctx: &mut dyn NativeContext,
    src: ObjectRef,
    src_off: usize,
    dst: ObjectRef,
    dst_off: usize,
    len: usize,
) -> bool {
    // nio21 wave 17: read once per process -- this runs per arraycopy
    // (2.4M declared-flag reads on the w16 `hdr` row).
    static BULK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let bulk = *BULK.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_BULK_REF_ARRAYCOPY")
    });
    bulk && ctx.copy_reference_array_elements(src, src_off, dst, dst_off, len)
}

#[inline]
pub(crate) fn al_set_data(ctx: &mut dyn NativeContext, this: ObjectRef, buf: ObjectRef) {
    let ((data_slot, _, _), layout) = al_slots_and_layout_for(ctx, this);
    // Receiver-layout guard — see `al_state` for rationale. Skip the write
    // entirely on a wrong-class receiver to avoid the out-of-bounds GC guard
    // warning that pairs with the read-side fix above.
    let n_fields = ctx.object_num_fields(this);
    if data_slot >= n_fields {
        return;
    }
    // ... and the layout-class half of `al_state`'s guard (interpreter round
    // i3 wave 79, lane L1): a foreign receiver with enough fields
    // (`CopyOnWriteArrayList`, whose field 1 is `array`) passed the count test
    // and had ArrayList's `elementData` stored over its own field.
    if !layout.accepts(n_fields) {
        return;
    }
    ctx.set_field(this, data_slot, Value::Object(Some(buf)));
}

/// The `AbstractList.modCount` slot of a list receiver, or `None` when there is
/// no slot this crate may write.
///
/// `modCount` only exists in the real-JDK layout, where `AbstractList`
/// contributes it ahead of `ArrayList.elementData`/`size`. In synthetic-jdk mode
/// `al_slots_for` falls back to `(data = 0, size = 1)`, so a resolver that
/// answered `Some(0)` there would have us store an `Int` over the backing-array
/// REFERENCE — the same heap corruption `tm_obj_key`'s doc block describes for
/// the TreeMap slots, and the GC would then decode a garbage discriminant out of
/// a reference slot. Hence the two exclusions: the slot has to be in range for
/// this receiver AND distinct from the two slots the list natives already own.
///
/// `None` degrades to the pre-2026-08-11 behaviour (no comodification detection
/// at all), which is the safe direction: an iterator that never throws
/// `ConcurrentModificationException` is what every CratonVM list iterator did
/// before this, so a build with no resolvable `modCount` loses nothing.
#[inline]
pub(crate) fn al_mod_count_slot(ctx: &dyn NativeContext, this: ObjectRef) -> Option<usize> {
    let slot = abstract_list_mod_count_slot(ctx)?;
    let ((data_slot, size_slot, _), layout) = al_slots_and_layout_for(ctx, this);
    if slot == data_slot || slot == size_slot || slot >= ctx.object_num_fields(this) {
        return None;
    }
    // A `MAP_VIEW_CARRIERS` receiver has NO `modCount` slot of its own. It
    // extends `AbstractCollection`, not `AbstractList`, so `AbstractList`'s
    // resolved index (0 in the real-JDK layout) does not name a field of this
    // object at all — it names the carrier's OWN first declared field:
    // `HashMap$Values.this$0` / `TreeMap$Values.this$0` /
    // `ConcurrentHashMap$CollectionView.map`, all REFERENCES, and
    // `LinkedHashMap$LinkedValues.reversed`, a boolean. Bumping a counter there
    // is a type-punning store into a declared reference slot — the exact hazard
    // this function's doc block describes for the synthetic layout, and the
    // reason CratonVM reported `HashMap$Values.this$0 == null` where HotSpot
    // reports the backing map (G22-1 N2, MEASURED). Views lose comodification
    // detection, which is the direction this function already documents as
    // safe.
    if layout == AlLayout::ViewCarrier {
        return None;
    }
    Some(slot)
}

thread_local! {
    /// `(vm_identity, AbstractList.modCount slot)`, filled only with a RESOLVED
    /// answer — same never-cache-the-miss rule as `al_slots_resolved`.
    static AL_ABSTRACT_LIST_MOD_COUNT: std::cell::Cell<Option<(usize, usize)>> =
        const { std::cell::Cell::new(None) };
}

/// `resolve_field_index("java/util/AbstractList", "modCount")`, memoized per
/// thread and VM.
///
/// PERF (round 9 wave 3). `al_mod_count_slot` sits under `al_set_size` — every
/// `ArrayList.add`/`remove` — and under the list iterator's comodification
/// check, once per element. The raw resolver hashes the class name and walks
/// the field table under the class-manager read lock to produce a constant: a
/// class's field layout is immutable after linking, and the bootstrap
/// `java/util/AbstractList` is never replaced within one VM.
#[inline]
pub(crate) fn abstract_list_mod_count_slot(ctx: &dyn NativeContext) -> Option<usize> {
    let vm = ctx.vm_identity();
    if let Some((cached_vm, slot)) = AL_ABSTRACT_LIST_MOD_COUNT.with(|c| c.get()) {
        if cached_vm == vm {
            return Some(slot);
        }
    }
    let slot = ctx.resolve_field_index("java/util/AbstractList", "modCount")?;
    AL_ABSTRACT_LIST_MOD_COUNT.with(|c| c.set(Some((vm, slot))));
    Some(slot)
}

/// Current `modCount` of `this`, or `None` when the receiver has no usable slot.
#[inline]
pub(crate) fn al_mod_count(ctx: &dyn NativeContext, this: ObjectRef) -> Option<i32> {
    match ctx.get_field(this, al_mod_count_slot(ctx, this)?) {
        Value::Int(v) => Some(v),
        _ => None,
    }
}

#[inline]
pub(crate) fn al_set_size(ctx: &mut dyn NativeContext, this: ObjectRef, size: i32) {
    // Same memo entry `al_slots_for` reads; the layout verdict rides along.
    let ((_, size_slot, _), layout) = al_slots_and_layout_for(ctx, this);
    let n_fields = ctx.object_num_fields(this);
    if size_slot >= n_fields {
        return;
    }
    // The layout-class guard `al_state` and `al_set_data` apply (interpreter
    // round i3 wave 79, lane L1): never store an `int` "size" into a foreign
    // receiver's field.
    if !layout.accepts(n_fields) {
        return;
    }
    // W7-1 family 2: `for (String s : l) l.add(s)` raised no
    // `ConcurrentModificationException` on CratonVM because nothing in this
    // crate had ever written `modCount` — the field the JDK's
    // `checkForComodification` compares against. Every structural modification
    // of an ArrayList-layout receiver changes its logical size and every such
    // change lands here, so this is the one funnel that sees all of them; a
    // same-value write is not a structural change and must not bump (callers
    // re-assert the current size on paths like `resync_values_view`).
    //
    // Deliberately UNDER-reports relative to the JDK: `sort` and `replaceAll`
    // bump `modCount` there and do not change the size here, so they stay
    // undetected. Under-reporting is the safe direction — it is exactly the old
    // behaviour — whereas a spurious bump would fail a loop that the real JDK
    // runs to completion.
    if !matches!(ctx.get_field(this, size_slot), Value::Int(n) if n == size) {
        if let Some(mod_slot) = al_mod_count_slot(ctx, this) {
            let cur = match ctx.get_field(this, mod_slot) {
                Value::Int(v) => v,
                _ => 0,
            };
            ctx.set_field(this, mod_slot, Value::Int(cur.wrapping_add(1)));
        }
    }
    ctx.set_field(this, size_slot, Value::Int(size));
}

/// `CRATONVM_COMPAT_AL_ADD_ALL_EMPTY_MODCOUNT` -- default ON, `=0` restores
/// the silent `false`. JDK 25 `ArrayList.addAll(c)` / `Vector.addAll(c)` bump
/// `modCount` right after `c.toArray()`, BEFORE the `numNew == 0` early
/// return, so `for (x : list) list.addAll(List.of())` throws
/// `ConcurrentModificationException` on the next `next()` on HotSpot; with
/// [`al_set_size`] as the only funnel the empty add bumped nothing. Round 15
/// wave 4 (lane compat4), `r15w3-compat3-collection-review-findings-CLOSED-20261001.md`
/// item 2.
pub(crate) fn al_add_all_empty_mod_count_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_AL_ADD_ALL_EMPTY_MODCOUNT")
}

/// One `modCount++` on an ArrayList-layout `this` that changes no size (the
/// structural bumps ride [`al_set_size`]). Nothing for a receiver with no
/// usable slot ([`al_mod_count_slot`]: views included).
pub(crate) fn al_bump_mod_count(ctx: &mut dyn NativeContext, this: ObjectRef) {
    if let Some(mod_slot) = al_mod_count_slot(&*ctx, this) {
        let cur = match ctx.get_field(this, mod_slot) {
            Value::Int(v) => v,
            _ => 0,
        };
        ctx.set_field(this, mod_slot, Value::Int(cur.wrapping_add(1)));
    }
}

/// Maximum backing-array length. Mirrors HotSpot's `ArrayList.MAX_ARRAY_SIZE`
/// guard (a value safely below `Integer.MAX_VALUE`): a required capacity beyond
/// this is unsatisfiable and yields `OutOfMemoryError`.
pub(crate) const AL_MAX_CAPACITY: usize = 1 << 30; // ~1 billion elements

/// Ensure the backing array has room for at least `min_cap` elements.
///
/// `grow` says whether the new backing array may be served after a collection
/// ([`GrowAlloc`]). Under `Reclaim` the collection can move `this` and the old
/// buffer (both pinned below, and re-read) and every reference the CALLER
/// pinned; the caller must re-read its own pins afterwards, as all five
/// callers already do.
pub(crate) fn al_ensure_capacity(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    min_cap: usize,
    grow: GrowAlloc,
) -> Result<ObjectRef, MethodCallFailed> {
    let (data, _size) = al_state(ctx, this);
    let old_cap = data.map_or(0, |d| ctx.array_length(d));

    if min_cap <= old_cap {
        // `old_cap == 0` implies the backing array is None (e.g. a freshly
        // constructed list asked to ensure capacity 0). Unwrapping would
        // panic, so allocate an empty array as the None-safe fallback.
        return Ok(data.unwrap_or_else(|| alloc_ref_array(ctx, 0)));
    }

    // A required capacity past the max array length is unsatisfiable. HotSpot's
    // `ArrayList.grow`/`newCapacity` throws `OutOfMemoryError` in this case; the
    // previous code instead returned the OLD (undersized) buffer, after which the
    // caller wrote at index `size >= old_cap` (out of bounds of the returned
    // array — a dropped element) and still bumped `size`, leaving `size` larger
    // than the backing array so a later `get` read uninitialized/stale storage.
    // Throw the catchable OOM instead (same idiom as `native_al_init_capacity`).
    if min_cap > AL_MAX_CAPACITY {
        return Err(RuntimeError::OutOfMemoryError {
            message: "Required array length too large".to_string(),
        }
        .into());
    }

    // Grow: max(old_cap * 1.5, min_cap) — matches Java's ArrayList strategy.
    // Use >> 1 for integer 1.5x, with minimum growth of 1 (handles old_cap == 0).
    let growth = std::cmp::max(old_cap >> 1, 1);
    // The 1.5x preferred growth may overshoot the max even though `min_cap`
    // itself fits (large `old_cap`). Mirror HotSpot's `hugeLength`: clamp the
    // preferred capacity down to the satisfiable `min_cap` rather than throwing
    // — only an out-of-range *required* capacity (handled above) is an error.
    let new_cap = std::cmp::min(
        std::cmp::max(old_cap.saturating_add(growth), min_cap),
        AL_MAX_CAPACITY,
    );

    // GC-SAFETY: `alloc_ref_array` below is a real allocation and can trigger
    // a moving GC, which may relocate `this` (the ArrayList receiver) and the
    // OLD backing array. Neither was pinned here before, so `al_set_data`'s
    // `ctx.set_field(this, ...)` could write through a STALE handle -- either
    // corrupting whatever now occupies that address (observed as GC header
    // corruption / segfaults) or silently landing on a reused-but-harmless
    // slot, in which case the real (moved) ArrayList's `data`/`size` fields
    // never get updated and the list looks permanently empty to callers like
    // `stream()`. Every caller of this function hits the exact same hazard on
    // `this` after the call returns (`al_set_size` etc.), so this is `add`/
    // `add(int,Object)`/`addAll`'s shared, hottest allocation site. See
    // stream-arraylist-gc-pressure-heap-corruption-FIXED.md.
    let this_pin = ctx.pin_native_root(this);
    let old_buf_pin = data.map(|d| ctx.pin_native_root(d)).unwrap_or(usize::MAX);
    // gc-common w9-d: under `GrowAlloc::Reclaim` a refused grow collects and
    // retries before it fails, as `ArrayList.grow`'s `Arrays.copyOf` does on
    // HotSpot. `live` is empty because both references this function uses
    // afterwards are already pinned just above and re-read below; the
    // caller's own references are in the caller's pins. A refused retry falls
    // through to the historical single-attempt allocator, so exhaustion still
    // surfaces exactly as it did (the native funnel's `OutOfMemoryError`).
    let new_buf: ObjectRef = match grow {
        GrowAlloc::SingleAttempt => alloc_ref_array(ctx, new_cap),
        GrowAlloc::Reclaim => {
            match try_ref_array_reclaiming(ctx, ClassId::new(0), new_cap, &mut []) {
                Some(buf) => buf,
                None => alloc_ref_array(ctx, new_cap),
            }
        }
    };
    let this = ctx.read_native_pin(this_pin, this);

    // Copy old content. Prefer the bulk intrinsic so the VM can use
    // `copy_nonoverlapping` on the underlying storage; fall back to the
    // per-element loop only when the override declines (default trait impl
    // also returns `true`, so the loop is unreachable when bulk succeeds).
    if let Some(old_buf) = data {
        let old_buf = if old_buf_pin != usize::MAX {
            ctx.read_native_pin(old_buf_pin, old_buf)
        } else {
            old_buf
        };
        let copy_len = std::cmp::min(old_cap, new_cap);
        al_copy_elements(ctx, old_buf, 0, new_buf, 0, copy_len);
    }

    al_set_data(ctx, this, new_buf);
    if altrace_enabled() {
        eprintln!(
            "[altrace GROW] this={:p} old_cap={old_cap} new_cap={new_cap} new_buf={:p}",
            this.as_ptr(),
            new_buf.as_ptr()
        );
    }
    ctx.unpin_native_roots(this_pin);
    Ok(new_buf)
}

pub(crate) fn register_arraylist_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/util/ArrayList";

    r.register(c, "<init>", "()V", native_al_init);
    r.register(c, "<init>", "(I)V", native_al_init_capacity);
    r.register(c, "size", "()I", native_al_size);
    r.register(c, "isEmpty", "()Z", native_al_is_empty);
    r.register(c, "get", "(I)Ljava/lang/Object;", native_al_get);
    r.register(
        c,
        "set",
        "(ILjava/lang/Object;)Ljava/lang/Object;",
        native_al_set,
    );
    // The registered doors are the `GrowAlloc::Reclaim` twins of the `pub`
    // functions other crates call directly -- see `GrowAlloc`.
    r.register(c, "add", "(Ljava/lang/Object;)Z", native_al_add_entry);
    r.register(c, "add", "(ILjava/lang/Object;)V", native_al_add_at_entry);
    r.register(c, "remove", "(I)Ljava/lang/Object;", native_al_remove_at);
    r.register(c, "remove", "(Ljava/lang/Object;)Z", native_al_remove_obj);
    r.register(c, "clear", "()V", native_al_clear);
    r.register(c, "contains", "(Ljava/lang/Object;)Z", native_al_contains);
    r.register(c, "indexOf", "(Ljava/lang/Object;)I", native_al_index_of);
    r.register(
        c,
        "lastIndexOf",
        "(Ljava/lang/Object;)I",
        native_al_last_index_of,
    );
    r.register(
        c,
        "toArray",
        "()[Ljava/lang/Object;",
        native_al_to_array_entry,
    );
    r.register(
        c,
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
        native_collection_to_array_generator,
    );
    // ArrayList.toArray(T[]) — Spring Boot fat-jar launcher's
    // `Launcher.createClassLoader(Collection)` calls `c.toArray(new URL[0])`
    // to obtain a typed `URL[]`. Real-JDK bytecode reads `elementData` and
    // calls `Arrays.copyOf(elementData, size, a.getClass())`, but that path
    // NPEs on our synthetic ArrayList because `a.getClass()` returns a
    // mirror without the array-component-type metadata `Arrays.copyOf`
    // walks. Provide an explicit override that copies into the supplied
    // array (or allocates a fresh one) without consulting the runtime
    // class of the template. Returning a simple Object[] is fine because
    // checkcast at the call site only verifies the array's component
    // class; our `alloc_ref_array` produces a raw reference array that
    // checkcasts to any `Object[]` subtype.
    r.register(
        c,
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
        native_al_to_array_typed,
    );
    // Also register on AbstractCollection (where the inherited bytecode
    // resolves) so the dispatch path that walks the superclass chain
    // finds the native before reaching the broken bytecode.
    r.register(
        "java/util/AbstractCollection",
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
        native_al_to_array_typed,
    );
    // Also the 0-arg form: `EnumSet.allOf(...).toArray()` resolves to
    // AbstractCollection.toArray(), whose real bytecode loops `iterator()` —
    // and our `Iterable.iterator` native only models ArrayList layout, so it
    // would iterate zero elements. Route it through `native_al_to_array`, which
    // uses the generic `collect_collection_elements` (handles EnumSet etc.).
    r.register(
        "java/util/AbstractCollection",
        "toArray",
        "()[Ljava/lang/Object;",
        native_al_to_array_entry,
    );
    r.register(
        "java/util/AbstractCollection",
        "contains",
        "(Ljava/lang/Object;)Z",
        native_al_contains,
    );
    r.register(c, "iterator", "()Ljava/util/Iterator;", native_al_iterator);
    r.register(c, "ensureCapacity", "(I)V", native_al_ensure_capacity);
    r.register(c, "trimToSize", "()V", native_al_trim_to_size);
    r.register(c, "toString", "()Ljava/lang/String;", native_al_to_string);
    r.register(c, "addAll", "(Ljava/util/Collection;)Z", native_al_add_all);
    // `subList` is `SyntheticStub`, not the ambient `Bridge` (JDK-only wave 2,
    // L7 R1 follow-up, 2026-08-05). It mints `cratonvm/internal/ArrayListSubList`,
    // which `--jdk-only` refuses to fabricate — so this registration, which
    // OUTLIVED L7's retag because it is a duplicate in a different registrar and
    // last write wins, turned `list.subList(..)` into a
    // `NoClassDefFoundError` naming a class no caller has heard of. It took
    // `Pattern.split` and `String.split` down with it, neither of which mentions
    // a collection.
    //
    // The real-bytecode fallback is `java.util.ArrayList$SubList`, which reads
    // `elementData`/`size` through the same accessors `native_al_*` maintain —
    // measured by `probes/JdkOnlyCollectionViewProbe`, which reports CONTENT so
    // that a view coming back silently EMPTY (L7's reason for leaving
    // `HashSet.iterator()` alone) cannot read as a pass.
    r.register_with_kind(
        c,
        "subList",
        "(II)Ljava/util/List;",
        native_al_sub_list,
        cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(c, "hashCode", "()I", native_al_hash_code);
    r.register(c, "equals", "(Ljava/lang/Object;)Z", native_al_equals);
    r.register(
        c,
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        native_al_for_each,
    );
    r.register(
        c,
        "sort",
        "(Ljava/util/Comparator;)V",
        native_al_sort_comparator,
    );
    r.register(
        c,
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
        native_al_remove_if,
    );
    r.register(
        c,
        "replaceAll",
        "(Ljava/util/function/UnaryOperator;)V",
        native_al_replace_all,
    );
    r.register(c, "stream", "()Ljava/util/stream/Stream;", native_al_stream);
    r.set_category(__prev_cat);
    // Fix (item 6): the backed-view class returned by `subList`.
    register_al_sublist_natives(r);
    // The classes a map's `values()`/`entrySet()` view is minted under.
    register_map_view_carrier_natives(r);
}

/// The `Collection` surface of a map view, registered on each of the
/// [`MAP_VIEW_CARRIERS`].
///
/// A view is ArrayList-LAYOUT under its own class, so it needs the same
/// `native_al_*` bodies the ArrayList registration installs. Interface-level
/// registrations (`java/util/Collection`, `java/util/AbstractCollection`) catch
/// some of these already, but only for methods the carrier's own JDK class does
/// not declare — and `HashMap$Values` declares `size`, `iterator`, `clear`,
/// `forEach` and `spliterator` itself, `TreeMap$Values` adds `contains` and
/// `remove`, and `ConcurrentHashMap$ValuesView` declares nearly the whole
/// surface. Registering here is what keeps the receiver-has-own-bytecode rule
/// from running JDK bodies that would read `this$0` (null on these carriers) —
/// paired with the entries in `force_native_over_real_jdk_bytecode`.
///
/// Deliberately NOT registered: `equals` and `hashCode`. The JDK's views
/// inherit `AbstractCollection`'s, i.e. `Object` identity, and installing the
/// LIST-contract `native_al_equals`/`native_al_hash_code` here would reinstate
/// exactly the `values().equals(anArrayList) == true` divergence that giving
/// these views their own class removes. `get`/`indexOf`/`lastIndexOf`/`set`/
/// `add(int, …)` are omitted for the same reason: a `Collection` has no
/// positional access, and answering one would make the view act like the `List`
/// it is no longer classed as.
/// CLUSTER NOTE (H4-1, 2026-08-20). This registrar spans FOUR ownership
/// families, and a retag of it as a unit is a partial retag of three of them.
/// [`MAP_VIEW_CARRIERS`] holds `HashMap$Values` and
/// `LinkedHashMap$LinkedValues` (the HashMap family), `TreeMap$Values` /
/// `TreeMap$EntrySet` (minted by `register_tree_map_natives`, which nothing
/// proposes to move), `Hashtable$ValueCollection` (minted by
/// `register_properties_natives` and by `properties_sidetable.rs`) and
/// `ConcurrentHashMap$ValuesView`.
///
/// A carrier is minted by a LIVE native with its list state at undeclared
/// slots and `this$0` left null. Refuse this registrar's rows for a carrier
/// whose producer is still `Bridge` and the real JDK body runs instead —
/// `TreeMap$Values.size()` dereferences that null `this$0`. So the split has
/// to be per carrier family, keyed on whether the producing registrar moved,
/// not per registrar. See the block above `register_collections_natives`.
pub(crate) fn register_map_view_carrier_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    for c in MAP_VIEW_CARRIERS {
        let c = *c;
        // The `JdkConcurrentHashMap` row (wave 76, L1): no CHM carrier is
        // minted under it, and a real `$ValuesView` runs its JDK body.
        if c.starts_with(CHM_CLASS) && chm_row_registers_nothing(r) {
            continue;
        }
        // PER CARRIER, not per registrar — `H4-1` §2. This list spans four
        // ownership clusters; one `set_category` outside the loop made moving
        // any of them a partial retag of the other three. See
        // [`CarrierFamily`].
        r.set_category(carrier_family_kind(carrier_family_of(c)));
        r.register(c, "size", "()I", native_al_size);
        r.register(c, "isEmpty", "()Z", native_al_is_empty);
        // `ConcurrentHashMap$ValuesView.contains` is `map.containsValue(o)`,
        // and THAT refuses null. Every other carrier in this list is backed by
        // a map that accepts null values, so the shared body is right for them
        // and wrong for exactly one — the shared-surface trap this campaign has
        // now hit four times. Registered INSIDE the loop rather than after it
        // on purpose: a second `r.register` for the same triple elsewhere in
        // the tree only wins if it runs later, and nothing in a registrar's
        // source position says when it runs (`owns_slot`, §5 of the handoff).
        if c == "java/util/concurrent/ConcurrentHashMap$ValuesView" {
            r.register(
                c,
                "contains",
                "(Ljava/lang/Object;)Z",
                native_chm_values_view_contains,
            );
        } else {
            r.register(c, "contains", "(Ljava/lang/Object;)Z", native_al_contains);
        }
        r.register(c, "iterator", "()Ljava/util/Iterator;", native_al_iterator);
        r.register(
            c,
            "toArray",
            "()[Ljava/lang/Object;",
            native_al_to_array_entry,
        );
        r.register(
            c,
            "toArray",
            "([Ljava/lang/Object;)[Ljava/lang/Object;",
            native_al_to_array_typed,
        );
        r.register(
            c,
            "toArray",
            "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
            native_collection_to_array_generator,
        );
        r.register(c, "toString", "()Ljava/lang/String;", native_al_to_string);
        r.register(c, "remove", "(Ljava/lang/Object;)Z", native_al_remove_obj);
        r.register(c, "clear", "()V", native_al_clear);
        r.register(
            c,
            "forEach",
            "(Ljava/util/function/Consumer;)V",
            native_al_for_each,
        );
        r.register(c, "stream", "()Ljava/util/stream/Stream;", native_al_stream);
        r.register(
            c,
            "spliterator",
            "()Ljava/util/Spliterator;",
            native_al_spliterator,
        );
        r.register(
            c,
            "removeIf",
            "(Ljava/util/function/Predicate;)Z",
            native_al_remove_if,
        );
    }
    r.set_category(__prev_cat);
}

pub fn native_al_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // GC-safety: `alloc_ref_array` can complete a moving young GC and `this` is
    // a bare Rust local — see `rooted_across`.
    let (this, buf) = rooted_across1(ctx, this, |ctx| alloc_ref_array(ctx, AL_DEFAULT_CAPACITY));
    al_set_data(ctx, this, buf);
    al_set_size(ctx, this, 0);
    Ok(None)
}

pub(crate) fn native_al_init_capacity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // JDK `ArrayList(int initialCapacity)`: a negative capacity throws
    // IllegalArgumentException; otherwise the backing `Object[initialCapacity]`
    // is allocated eagerly.
    let cap_i = match args.get(1) {
        Some(Value::Int(c)) => *c,
        _ => AL_DEFAULT_CAPACITY as i32,
    };
    if cap_i < 0 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: format!("Illegal Capacity: {cap_i}"),
            }
            .into(),
        );
    }
    let cap = cap_i as usize;
    // Match HotSpot: when `Object[cap]` is too large for the heap, throw a
    // *catchable* OutOfMemoryError ("Requested array size exceeds VM limit")
    // rather than aborting the whole VM. The previous code clamped `cap` to
    // 1<<30 and then hard-aborted in the panicking allocator on the resulting
    // ~8 GiB request (e.g. `new ArrayList(Integer.MAX_VALUE)` in SpEL's
    // ArrayConstructorTests — see the spring crash report).
    //
    // gc-common w9-d: a refusal collects and retries first (`new Object[cap]`
    // in HotSpot's constructor collects before it throws). Legal here: this
    // native is reached only through its registrations (`ArrayList(int)`,
    // `Vector(int)`), and `this` -- the only reference it holds -- is in
    // `live` and read back refreshed.
    let mut live = [this];
    let buf = match try_ref_array_reclaiming(ctx, ClassId::new(0), cap, &mut live) {
        Some(b) => b,
        None => {
            return Err(cratonvm_types::error::RuntimeError::OutOfMemoryError {
                message: "Requested array size exceeds VM limit".to_string(),
            }
            .into());
        }
    };
    let [this] = live;
    al_set_data(ctx, this, buf);
    al_set_size(ctx, this, 0);
    Ok(None)
}

/// `true` iff `obj`'s runtime class is (a subclass / implementor of)
/// `class_name`. Interface implementation counts (see `Class::is_subclass_of`).
pub(crate) fn obj_is_instance_of(ctx: &dyn NativeContext, obj: ObjectRef, class_name: &str) -> bool {
    match ctx.class_id_by_name(class_name) {
        Some(target) => ctx.is_subclass(ctx.class_id_of_object(obj), target),
        None => false,
    }
}

/// `true` iff `obj` is one of CratonVM's synthetic-backed collections
/// (ArrayList/Vector-layout list, HashSet, or HashMap-bucket map). These are
/// served by the `native_*` fast paths and MUST NOT be delegated to real
/// bytecode (their data lives in CratonVM-managed slots).
pub(crate) fn is_synthetic_backed_collection(ctx: &mut dyn NativeContext, obj: ObjectRef) -> bool {
    al_is_list_layout(ctx, obj)
        || hs_backing_map(ctx, obj).is_some()
        || map_state(ctx, obj).0.is_some()
}

/// Real-JDK completeness: the `size`/`isEmpty` natives are registered on the
/// `Collection`/`List`/`Set`/`Map` *interfaces* as a fallback. When one is
/// dispatched on a *real* JDK collection that CratonVM has no synthetic backing
/// for — e.g. `Collections$SingletonList`, `Collections$EmptyList`, or any other
/// real `List`/`Set`/`Map` implementation — run the receiver's OWN bytecode for
/// the method instead of returning the empty sentinel.
///
/// Returns `None` (caller keeps its sentinel) for CratonVM's synthetic
/// collections and for non-collection receivers (reflection /
/// lambda-metafactory funnels a `Charset`, `String`, … here).
///
/// THE PREMISE THIS HELPER WAS WRITTEN ON IS FALSE FOR PART OF ITS INPUT, and
/// the correction is the `class_declares_method` branch in the body. It used
/// to read:
///
/// > `invoke_special` does an *exact* per-class native lookup (which finds
/// > nothing for these real classes) and then runs their real `Code`, so there
/// > is no recursion back into this native.
///
/// It finds nothing for `Collections$SingletonList` and friends, which is the
/// input the sentence was written about. It finds a native for every
/// [`SET_VIEW_CARRIERS`] entry, because this VM registers those natives under
/// the REAL JDK class name — so on a real `java/util/HashMap$EntrySet` the
/// lookup re-finds the very native that called us, the re-entrancy guard
/// below trips, and the caller returns the sentinel this helper exists to
/// avoid: `size()` answers 0 for a three-entry map, four frames below a
/// `size()` that answers 3. Measured 2026-09-11, wave 3 of lane 1 — see
/// `docs/feature-designs/jdk-only-mode.md` §3.
///
/// A comment is not a compile-time link to the premise it depends on, which is
/// why the branch asks the registry instead of restating the claim.
thread_local! {
    /// Re-entrancy guard for [`try_delegate_real_collection`]. Holds a
    /// [`ReentryEntry`] per real-collection delegation currently in flight on
    /// this thread.
    pub(crate) static DELEGATE_GUARD: std::cell::RefCell<Vec<ReentryEntry>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// One entry of this file's thread-local re-entrancy guards (`DELEGATE_GUARD`,
/// `COW_SET_GUARD`, `SNAPITR_FALLBACK`): the receiver's native PIN HANDLE, the
/// receiver as it was when the entry was pushed (the `read_native_pin`
/// fallback), and the method.
///
/// gc-common w26-a: the entries used to record [`reentry_guard_key`], the
/// receiver's identity hash. That follows the receiver across a move (the
/// w13-c fix below), but two LIVE objects can share an identity hash -- every
/// backend mints it from one per-heap counter that wraps after about 2^32
/// requests -- so a collection whose hash collided with an outer frame's
/// receiver read as a re-entry and answered its native fallback (`size() ==
/// 0`, an exhausted iterator). A pin handle names exactly one object and
/// reads back its CURRENT address, the pattern `CHM_CALLBACK_FRAMES` already
/// uses: the outer frame that pushed the entry is alive (and so is its pin)
/// whenever an inner frame is looking. See
/// `docs/feature-designs/jdk-only-mode.md`.
type ReentryEntry = (usize, ObjectRef, &'static str);

/// Push a guard entry for `(this, method)`, or answer `None` when an entry
/// already names this receiver for this method (a self-re-entry). On `Some`
/// the caller owns the returned pin handle and MUST call
/// [`reentry_guard_leave`] with it on every path after the guarded call.
pub(crate) fn reentry_guard_enter(
    ctx: &mut dyn NativeContext,
    guard: &'static std::thread::LocalKey<std::cell::RefCell<Vec<ReentryEntry>>>,
    this: ObjectRef,
    method: &'static str,
) -> Option<usize> {
    let pin = ctx.pin_native_root(this);
    let reentered = guard.with(|g| {
        let mut g = g.borrow_mut();
        let ctx: &dyn NativeContext = &*ctx;
        let held = g
            .iter()
            .any(|&(h, pushed, m)| m == method && ctx.read_native_pin(h, pushed) == this);
        if !held {
            g.push((pin, this, method));
        }
        held
    });
    if reentered {
        ctx.unpin_native_roots(pin);
        return None;
    }
    Some(pin)
}

/// Pop the entry [`reentry_guard_enter`] pushed under `pin` and release the
/// pin. Entries nest (each guarded call pops its own before it returns), so
/// this is the last entry carrying `pin`; `rposition` rather than `pop` keeps
/// it exact even if a mock hands every pin the same handle.
pub(crate) fn reentry_guard_leave(
    ctx: &mut dyn NativeContext,
    guard: &'static std::thread::LocalKey<std::cell::RefCell<Vec<ReentryEntry>>>,
    pin: usize,
) {
    guard.with(|g| {
        let mut g = g.borrow_mut();
        if let Some(i) = g.iter().rposition(|&(h, _, _)| h == pin) {
            g.remove(i);
        }
    });
    ctx.unpin_native_roots(pin);
}

/// The identity-hash key this file's re-entrancy guards recorded for a
/// receiver until gc-common w26-a: its identity hash, tagged with the top bit
/// so it can never equal a raw address. The guards now record a pin handle
/// ([`ReentryEntry`]); this stays for the `__test_reentry_guard_key` hook that
/// `native-collections/tests/lhm_clone_overlay.rs` still calls.
///
/// gc-common w13-c: the guards used to record the RAW ADDRESS. Each one is
/// held across a call into Java bytecode (`invoke_special*` / `invoke`), and
/// Java can allocate and trigger a moving collection. The receiver then moves
/// while its entry still names the address it left, and a young collection
/// hands that address straight back to the allocator. A DIFFERENT object
/// allocated there that reaches the same guard inside the same call, for
/// example an iterator a composite iterator's `hasNext` creates, read as a
/// re-entry. The guard then declined: a real collection answered its native
/// fallback (`size() == 0`), or an iterator reported itself exhausted, with no
/// exception anywhere.
///
/// The identity hash moves with the header and is minted from a counter at
/// allocation, so the recycled-address newcomer presents a different key,
/// and the receiver keeps its own key across the move. A hash of `0` (an
/// address the heap does not recognise) falls back to the address, which is
/// the old behaviour.
#[inline]
pub(crate) fn reentry_guard_key(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    match ctx.identity_hash_code(this) {
        0 => this.as_ptr() as usize,
        h => (1usize << (usize::BITS - 1)) | (h as u32 as usize),
    }
}

/// Test hook for [`reentry_guard_key`].
#[doc(hidden)]
pub fn __test_reentry_guard_key(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    reentry_guard_key(ctx, this)
}

/// Delegate a `Collection`/`Map` method on a *real* (non-synthetic-backed)
/// receiver to its actual class implementation via `invokespecial`.
///
/// Re-entrancy guard (HIB native stack-overflow fix): `invoke_special` below
/// resolves `cls.method`, and for a class whose `method` is itself registered to
/// the SAME native shim that called us (e.g. a real `java.util.Map` instance with
/// no CratonVM native bucket state, whose `isEmpty`/`size` is force-native), the
/// dispatch lands straight back in that shim → it re-delegates → unbounded native
/// recursion. No Java frame is pushed on this path, so the interpreter's
/// stack-depth guard never fires and the process dies with a native
/// `EXCEPTION_STACK_OVERFLOW` (observed building a Hibernate `SessionFactory`).
/// We detect the self-re-entry on the same `(object, method)` and decline to
/// delegate, letting the caller fall back to its native best-effort result
/// instead of looping. The guard matches on a pin handle of the receiver
/// ([`ReentryEntry`]), neither the raw pointer nor the identity hash: the
/// looping path runs no bytecode between levels, but the delegated body that
/// runs while the entry is held does, and can move the receiver and hand its
/// old address to another collection; and another live collection can share
/// the receiver's identity hash.
pub(crate) fn try_delegate_real_collection(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    iface: &str,
    method: &'static str,
    descriptor: &str,
) -> Option<MethodCallResult> {
    try_delegate_real_collection_args(ctx, this, iface, method, descriptor, &[])
}

/// [`try_delegate_real_collection`] for a method that takes arguments: `rest`
/// are the arguments after the receiver. Interpreter round i3 wave 79, lane L1
/// (`i74-L1-list-add-interface-native-writes-arraylist-slots-into-a-foreign-receiver`):
/// the `List.add` interface natives send a foreign-layout receiver to its own
/// `add` through this instead of writing ArrayList's slots into it.
pub(crate) fn try_delegate_real_collection_args(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    iface: &str,
    method: &'static str,
    descriptor: &str,
    rest: &[Value],
) -> Option<MethodCallResult> {
    if !obj_is_instance_of(ctx, this, iface) || is_synthetic_backed_collection(ctx, this) {
        return None;
    }
    let Some(guard_pin) = reentry_guard_enter(ctx, &DELEGATE_GUARD, this, method) else {
        // Self-delegation loop: the resolved `cls.method` is the same native
        // shim. Decline so the caller returns its native fallback rather than
        // recursing into a stack overflow.
        return None;
    };
    let cls = match ctx.class_name_of_id(ctx.class_id_of_object(this)) {
        Some(c) => c,
        None => {
            reentry_guard_leave(ctx, &DELEGATE_GUARD, guard_pin);
            return None;
        }
    };
    // ASK, DO NOT ASSUME, whether this class's own `method` is native-backed.
    //
    // When the receiver's OWN class declares the method, its `Code` is the
    // thing we want and `invoke_special_bytecode_only` is the API for exactly
    // this case — its contract is "for a native that IS ITSELF the native
    // registered for (class, method, descriptor)". It binds statically on
    // `cls`, never virtually on a subclass, so it cannot re-dispatch into an
    // override that calls us back.
    //
    // When the class does NOT declare it, the resolution walks up to a
    // supertype that may be abstract or may have no body at all (a synthetic
    // stub), and `invoke_special`'s native-first lookup is the safer answer —
    // that is the input the original premise was written about, and its
    // behaviour is unchanged here.
    //
    // `class_declares_method` EXCLUDES compiler-generated bridges, which is
    // the behaviour this wants rather than an accident to work around: a
    // bridge is javac's forwarding stub, not the receiver's own body, so
    // running it "bytecode-only" would forward to a supertype method the
    // native-first lookup should have had a say about. Declared-and-not-a-
    // bridge is exactly "this class provides its own implementation".
    let declares_own = ctx.class_declares_method(ctx.class_id_of_object(this), method, descriptor);
    // The receiver-only case (every caller but the `add` ones) keeps its
    // allocation-free argument array.
    let one = [Value::Object(Some(this))];
    let owned: Vec<Value>;
    let call_args: &[Value] = if rest.is_empty() {
        &one
    } else {
        owned = one.iter().chain(rest.iter()).copied().collect();
        &owned
    };
    let r = if declares_own {
        ctx.invoke_special_bytecode_only(&cls, method, descriptor, call_args)
    } else {
        ctx.invoke_special(&cls, method, descriptor, call_args)
    };
    reentry_guard_leave(ctx, &DELEGATE_GUARD, guard_pin);
    Some(r)
}

/// If `this` is one of the `cratonvm/internal/Unmodifiable*` wrappers that
/// `List.of` / `Set.of` / `Map.of` and `Collections.unmodifiable*` hand back,
/// the collection it wraps.
///
/// The wrapper's own class carries delegating natives, so a call that arrives
/// through it is fine. A call that arrives through the CONCRETE class instead —
/// `HashSet.size()` on a `Set.of(a, b)` result, which is what real code does
/// after `Set.of` is assigned to a `HashSet`-typed local, and what these tests
/// do — found no backing map and answered 0. Unwrapping first makes the
/// concrete-class entry points agree with the wrapper's.
/// The size to bounds-check an unmodifiable view's `get(index)` against.
///
/// `al_state` reads an `elementData`-shaped backing and reports **0** for every
/// other List shape. Taking that at face value made
/// `Collections.unmodifiableList(x).get(i)` throw on a perfectly valid index
/// whenever `x` was a `LinkedList`, an `Arrays$ArrayList`, or anything else —
/// on a list that iterates fine, because iteration does not come through here.
///
/// So a reported 0 means "could not read it", not "empty", and the caller has
/// to ask the backing itself. A genuinely empty backing answers 0 either way,
/// which is why the fallback is safe to take on 0 rather than needing a
/// separate "unreadable" signal.
pub(crate) fn unmod_view_size(al_state_data: Option<ObjectRef>, al_state_size: i32) -> Option<i32> {
    // The DATA slot, not the size, is the readability signal. A size of 0 is
    // ambiguous — it is what `al_state` reports both for a genuinely empty
    // array-backed list and for a backing it could not read at all — and
    // conflating those is the whole defect.
    al_state_data.map(|_| al_state_size.max(0))
}

/// Whether a `cratonvm/internal/Unmodifiable*` receiver came from an
/// *immutable* factory (`List.of` / `copyOf`) rather than from
/// `Collections.unmodifiable*`.
///
/// Slot 1 is [`UNMOD_FIELD_IMMUTABLE`], stamped by `freeze_result`. CONTRACT:
/// mirrors `getclass_immutable_marker` in native-builtins, which reads the same
/// slot to decide whether `getClass()` reports `ImmutableCollections$*` or
/// `Collections$Unmodifiable*`.
pub(crate) fn unmod_is_immutable(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    matches!(ctx.get_field(this, UNMOD_FIELD_IMMUTABLE), Value::Int(1))
}

/// RULE I — a null QUERY argument on an IMMUTABLE collection is an NPE.
///
/// `ImmutableCollections` opens `contains`, `indexOf`, `lastIndexOf`, `get`,
/// `getOrDefault`, `containsKey` and `containsValue` with
/// `Objects.requireNonNull(o)`: a null cannot be IN one of these, so asking
/// about one is a caller bug rather than a question whose answer is "no".
///
/// `Collections.unmodifiableList(l).contains(null)` is `false` for the opposite
/// reason — the wrapper delegates to a list that permits nulls — and this VM
/// funnels both through one synthetic class. The discriminator is therefore
/// [`unmod_is_immutable`] (slot 1) and NOT the class, exactly as
/// [`unmod_list_oob_error`] splits the three out-of-range messages.
///
/// MEASURED in COMPATIBLE MODE ONLY (apps/probes/UtilTailShadowSweep 65, 66, 85,
/// 101, 102): strict already answers correctly, because these factories are
/// `SyntheticStub` registrations it drops in favour of real bytecode.
pub(crate) fn reject_null_immutable_query(
    ctx: &dyn NativeContext,
    args: &[Value],
    at: usize,
) -> Result<(), MethodCallFailed> {
    if !matches!(args.get(at), Some(Value::Object(None))) {
        return Ok(());
    }
    match args.first() {
        Some(Value::Object(Some(this))) if unmod_is_immutable(ctx, *this) => Err(bare_npe()),
        _ => Ok(()),
    }
}

/// The out-of-range error for an unmodifiable/immutable list wrapper, or `None`
/// to stand aside and let the backing's own `get` throw.
///
/// HotSpot has three answers here and this VM funnels all three through one
/// synthetic class — but not through one *object shape*. Measured on Temurin 25
/// with `probes/ListOutOfBoundsProbe`:
///
/// ```text
/// List.of("a").get(3)                       IndexOutOfBoundsException  "Index: 3 Size: 1"
/// List.of("a","b").get(2)                   IndexOutOfBoundsException  "Index: 2 Size: 2"
/// List.of().get(0)                          ArrayIndexOutOfBounds...   "Index 0 out of bounds for length 0"
/// List.of("a","b","c").get(3)               ArrayIndexOutOfBounds...   "Index 3 out of bounds for length 3"
/// unmodifiableList(new ArrayList(1)).get(3) IndexOutOfBoundsException  "Index 3 out of bounds for length 1"
/// unmodifiableList(Arrays.asList(2)).get(5) ArrayIndexOutOfBounds...   "Index 5 out of bounds for length 2"
/// unmodifiableList(new LinkedList(2)).get(5) IndexOutOfBoundsException "Index: 5, Size: 2"
/// ```
///
/// The JDK's split is `ImmutableCollections.List12` (1-2 elements, bounds-checks
/// first, plain class) versus `ListN` (0 or 3+, indexes its array directly, so
/// the array access throws the subclass); and `Collections.unmodifiable*` is a
/// *view* that simply delegates, so its answer is whatever its backing throws —
/// which is why the last three rows differ from each other despite the same
/// wrapper.
///
/// The doc for this bug recorded that the `List12` case could not be told from
/// `Collections.unmodifiableList` here. It can: [`unmod_is_immutable`] reads the
/// marker slot, which is the same bit `getClass()` already splits on. Reproducing
/// the JDK's shape is therefore not a guess.
pub(crate) fn unmod_list_oob_error(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    index: i32,
    size: i32,
) -> Option<MethodCallFailed> {
    if !unmod_is_immutable(ctx, this) {
        // `Collections.unmodifiableList` is a view. HotSpot delegates, so the
        // backing decides both class and wording — an ArrayList backing answers
        // with the plain class, an `Arrays$ArrayList` with the subclass, a
        // LinkedList with a third wording again. Pre-empting it here is what
        // made all three look alike.
        return None;
    }
    if (1..=2).contains(&size) {
        // `ImmutableCollections.List12` — bounds-checks before indexing, so the
        // plain class, and its own wording: a SPACE, not the comma that
        // `ArrayList.add`'s `rangeCheckForAdd` uses.
        return Some(
            cratonvm_types::error::RuntimeError::ioobe(format!("Index: {index} Size: {size}"))
                .into(),
        );
    }
    // `ImmutableCollections.ListN` (0 or 3+ elements) indexes its array
    // directly, so the array access itself throws the subclass.
    Some(cratonvm_types::error::RuntimeError::aioobe(index, size).into())
}

pub(crate) fn unmod_receiver_backing(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    // The receiver's CLASS answers this outright for every ordinary list, from
    // a memo the operation is about to consult anyway — see
    // `AlLayout::rules_out_wrapper_routes`. Without it this function ran a
    // `class_id_of_object`, a `class_name_rc` and a `starts_with` on every
    // `size()`/`get()`/`isEmpty()` of every `ArrayList` in the process, for an
    // answer the class had already settled: 1.0% of a flat `ArrayList.size()`
    // profile here plus the 1.3% `class_name_rc` it drives.
    if al_slots_and_layout_for(&*ctx, this)
        .1
        .rules_out_wrapper_routes()
    {
        return None;
    }
    // `class_name_rc` is the memoized reader; `class_name_of_id` takes the
    // class-manager read lock and allocates a fresh `String` on EVERY call.
    // This runs on the hot path of every `native_al_*` entry point.
    let cid = ctx.class_id_of_object(this);
    let name = class_name_rc(&*ctx, cid)?;
    if !name.starts_with("cratonvm/internal/Unmodifiable") {
        return None;
    }
    unmod_backing(ctx, this)
}

/// Size of a JDK singleton/empty collection wrapper.
///
/// `Collections.singletonList` / `singleton` / `singletonMap` return real
/// `Collections$Singleton*` objects, and `emptyList` and friends return
/// `Collections$Empty*`. None of them has an `elementData`/`table` field, so
/// `al_state` and `map_state` read nothing and the accessors below fall back to
/// delegating into the class's own bytecode. That is right when the bytecode is
/// loaded and answers nothing when it is not — which left `singletonList(x).size()`
/// reporting **0**. The sizes are constants of the wrapper type, so answer them
/// directly and keep the delegation as the path for everything else.
pub(crate) fn singleton_wrapper_size(ctx: &dyn NativeContext, this: ObjectRef) -> Option<i32> {
    let name = class_name_rc(ctx, ctx.class_id_of_object(this))?;
    match &*name {
        "java/util/Collections$SingletonList"
        | "java/util/Collections$SingletonSet"
        | "java/util/Collections$SingletonMap" => Some(1),
        "java/util/Collections$EmptyList"
        | "java/util/Collections$EmptySet"
        | "java/util/Collections$EmptyMap" => Some(0),
        _ => None,
    }
}

/// Outcome of [`al_state_for_read`].
pub(crate) enum AlRead {
    /// The receiver is a live map view and the caller wanted only its element
    /// count, which the source answered without a rebuild.
    ViewSize(i32),
    /// Ordinary state: the (possibly relocated) receiver, its backing array
    /// and its size.
    State(ObjectRef, Option<ObjectRef>, i32),
}

/// `al_state` for a receiver that may be a live map view, computing the ONE
/// `al_state` that both the marker test and the answer need.
///
/// `resync_values_view` opens with `values_view_source`, which is itself an
/// `al_state`; every native that called it and then `al_state` therefore read
/// the receiver's two fields twice and classified its layout twice. On the
/// non-view path — every ordinary `ArrayList`, which is the bulk of the traffic
/// — the second read is pure duplication: `values_view_source` allocates
/// nothing and runs no Java, so nothing between the two can have moved the
/// object. On the view path the rebuild DOES move things, so the re-read stays.
///
/// `size_only` additionally skips the rebuild altogether. A view's logical size
/// is by construction its source's size — `resync_values_view` refills it from
/// `collect_entries_any(source)` and stores exactly that many elements — so
/// `size()` and `isEmpty()` never need the elements at all, and rebuilding for
/// them was O(n) with an allocation PER CALL: measured at **15.9 us** for
/// `values().size()` over a 64-entry `HashMap`, against 610 ns for
/// `ArrayList.size()` on the same binary and 3 ns on HotSpot. One virtual
/// `size()` on the source replaces it. A source that cannot answer falls back
/// to the rebuild rather than inventing a number.
pub(crate) fn al_state_for_read(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    size_only: bool,
) -> Result<AlRead, MethodCallFailed> {
    let (data, size) = al_state(ctx, this);
    let Some(d) = data else {
        return Ok(AlRead::State(this, None, size));
    };
    let Some(source) = view_source_in(ctx, d, size) else {
        return Ok(AlRead::State(this, data, size));
    };
    // A view naming ITSELF as its source would recurse forever. The marker is
    // never written that way, but these natives are reachable from application
    // bytecode that can hand over any object.
    if size_only && source != this {
        if let Ok(Some(Value::Int(n))) = ctx.invoke_virtual(source, "size", "()I", &[]) {
            if n >= 0 {
                return Ok(AlRead::ViewSize(n));
            }
        }
    }
    let this = resync_values_view(ctx, this)?;
    let (data, size) = al_state(ctx, this);
    Ok(AlRead::State(this, data, size))
}

pub fn native_al_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Hot path: an ordinary `ArrayList` answers from its `size` field — what the
    // general path below reaches for it after ruling out every route that
    // `al_fast_state` rules out (`KeySetView`, unmodifiable wrapper, values
    // view class, view-carrier marker, null `elementData`).
    if let Some(Value::Object(Some(this))) = args.first() {
        if let Some((_, size, _)) = al_fast_state(&*ctx, *this) {
            return Ok(Some(Value::Int(size)));
        }
    }
    if let Some(r) = ksv_route(ctx, args, native_ksv_size) {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    if let Some(b) = unmod_receiver_backing(ctx, this) {
        return native_al_size(ctx, &[Value::Object(Some(b))]);
    }
    // Two DIFFERENT view shapes reach here, and each needs its own test.
    //
    // First: a real `Map.values()` view-class receiver (`java.util.HashMap$Values`
    // and family, `CF_VALUES_VIEW`). It is not ArrayList-shaped at ALL — it has
    // no `elementData` — so `al_state` cannot decode it and the class test has
    // to run first. It counts through its source map. See `vc_route_source_size`.
    if let Some(r) = vc_route_source_size(ctx, this) {
        return r;
    }
    // Second: an ArrayList-SHAPED carrier that names a source map in the spare
    // trailing slot of its backing array. `al_state_for_read` reads the one
    // `al_state` both the marker test and the answer need, and `size_only`
    // answers from the source instead of rebuilding the carrier to count it.
    let (this, data, size) = match al_state_for_read(ctx, this, true)? {
        AlRead::ViewSize(n) => return Ok(Some(Value::Int(n))),
        AlRead::State(t, d, s) => (t, d, s),
    };
    if data.is_none() {
        if let Some(r) =
            try_delegate_real_collection(ctx, this, "java/util/Collection", "size", "()I")
        {
            return r;
        }
        if let Some(n) = singleton_wrapper_size(ctx, this) {
            return Ok(Some(Value::Int(n)));
        }
    }
    Ok(Some(Value::Int(size)))
}

pub fn native_al_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Hot path; same argument as `native_al_size`'s.
    if let Some(Value::Object(Some(this))) = args.first() {
        if let Some((_, size, _)) = al_fast_state(&*ctx, *this) {
            return Ok(Some(Value::Int(i32::from(size == 0))));
        }
    }
    if let Some(r) = ksv_route(ctx, args, native_ksv_is_empty) {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(1))),
    };
    // Both view shapes again, in the same order as `native_al_size`, each
    // reduced to the boolean. Going through the COUNT rather than through
    // `vc_route` keeps `isEmpty()` free of the carrier allocation on either
    // path.
    if let Some(r) = vc_route_source_size(ctx, this) {
        return Ok(Some(Value::Int(match r? {
            Some(Value::Int(n)) => i32::from(n == 0),
            _ => 1,
        })));
    }
    let (this, data, size) = match al_state_for_read(ctx, this, true)? {
        AlRead::ViewSize(n) => return Ok(Some(Value::Int(i32::from(n == 0)))),
        AlRead::State(t, d, s) => (t, d, s),
    };
    if data.is_none() {
        if let Some(r) =
            try_delegate_real_collection(ctx, this, "java/util/Collection", "isEmpty", "()Z")
        {
            return r;
        }
        if let Some(n) = singleton_wrapper_size(ctx, this) {
            return Ok(Some(Value::Int(i32::from(n == 0))));
        }
    }
    Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))
}

// ALTRACE (checkcast-corruption investigation, 20260721): env-gated
// unconditional trace of every ArrayList add/get -- prints (this ptr, buf
// ptr, index/size) so two concurrently-growing lists (e.g. `received` vs
// `garbage` in JulGcStressRepro) can be told apart in the log and any
// cross-contamination between their backing arrays caught at the exact
// call that produced it.
#[inline]
pub(crate) fn altrace_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_ALTRACE"))
}

// ALTRACE helper: resolve a Value to "ptr cls=... [str=...]" so a trace line
// records what the element ACTUALLY IS at that moment (catches stale pointers
// whose referent has been replaced by a foreign object after a moving GC).
pub(crate) fn altrace_describe(ctx: &mut dyn NativeContext, v: Value) -> String {
    match v {
        Value::Object(Some(o)) => {
            let cls = ctx
                .class_name_of_id(ctx.class_id_of_object(o))
                .unwrap_or_else(|| "<unknown-class>".to_string());
            if cls == "java/lang/String" {
                format!(
                    "{:p} cls={cls} str={:?}",
                    o.as_ptr(),
                    ctx.read_string(o).unwrap_or_default()
                )
            } else {
                format!("{:p} cls={cls}", o.as_ptr())
            }
        }
        other => format!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Vector's LEGACY index checks.
//
// MEASURED 2026-08-13 (scratchpad/orch/V2.java, HotSpot 25.0.3+9-LTS). Two
// things the shared ArrayList natives get wrong for a Vector receiver:
//
//  * the CLASS is ArrayIndexOutOfBoundsException, a SUBCLASS of the
//    IndexOutOfBoundsException `native_al_get` throws -- so a `catch
//    (ArrayIndexOutOfBoundsException)` in legacy code never fired;
//  * the TEXT follows three conventions that cannot be derived from one
//    another, and the two negative arms actively disagree:
//
//      elementAt(i >= size)        "3 >= 3"
//      elementAt(i < 0)            "Index -1 out of bounds for length 10"
//      get(i >= size)              "Array index out of range: 3"
//      get(i < 0)                  "Index -1 out of bounds for length 10"
//      removeElementAt(i >= size)  "3 >= 3"
//      removeElementAt(i < 0)      "Array index out of range: -1"
//
// The "length" in the negative arms is the BACKING ARRAY's capacity, not the
// element count: `new Vector<>(100)` with one element reports "length 100".
// That is `elementData(index)` faulting on the raw array, so it is reproduced
// from the real array length rather than from `size`.
pub(crate) fn vec_aioobe(index: i32, message: String) -> MethodCallFailed {
    RuntimeError::ArrayIndexOutOfBoundsException {
        index,
        message: Some(message),
    }
    .into()
}

/// The negative-index arm shared by `elementAt` and `get` (but NOT by
/// `removeElementAt`, which words it differently).
pub(crate) fn vec_negative_index(ctx: &dyn NativeContext, this: ObjectRef, index: i32) -> MethodCallFailed {
    let (data, _) = al_state(ctx, this);
    let cap = data.map_or(0, |d| ctx.array_length(d));
    vec_aioobe(
        index,
        format!("Index {index} out of bounds for length {cap}"),
    )
}

pub(crate) fn vec_index_args(args: &[Value]) -> Option<(ObjectRef, i32)> {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => return None,
    };
    Some((this, index))
}

pub(crate) fn native_vec_element_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some((this, index)) = vec_index_args(args) else {
        return Ok(Some(Value::Object(None)));
    };
    let (_, size) = al_state(ctx, this);
    if index < 0 {
        return Err(vec_negative_index(ctx, this, index));
    }
    if index >= size {
        return Err(vec_aioobe(index, format!("{index} >= {size}")));
    }
    native_al_get(ctx, args)
}

pub(crate) fn native_vec_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some((this, index)) = vec_index_args(args) else {
        return Ok(Some(Value::Object(None)));
    };
    let (_, size) = al_state(ctx, this);
    if index < 0 {
        return Err(vec_negative_index(ctx, this, index));
    }
    if index >= size {
        return Err(vec_aioobe(
            index,
            format!("Array index out of range: {index}"),
        ));
    }
    native_al_get(ctx, args)
}

pub(crate) fn native_vec_remove_element_at_checked(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some((this, index)) = vec_index_args(args) else {
        return Ok(None);
    };
    let (_, size) = al_state(ctx, this);
    if index < 0 {
        return Err(vec_aioobe(
            index,
            format!("Array index out of range: {index}"),
        ));
    }
    if index >= size {
        return Err(vec_aioobe(index, format!("{index} >= {size}")));
    }
    native_vec_remove_element_at(ctx, args)
}

// The rest of Vector's index-taking surface. MEASURED 2026-08-13
// (scratchpad/orch/V3.java). SIX conventions across five methods, and they do
// not reduce to a rule:
//
//   set(i >= size)              "Array index out of range: 3"
//   set(i < 0)                  "Index -1 out of bounds for length 10"
//   setElementAt(x, i >= size)  "3 >= 3"
//   setElementAt(x, i < 0)      "Index -1 out of bounds for length 10"
//   insertElementAt(x, i > size) "4 > 3"      <- `>`, not `>=`: i == size is LEGAL
//   insertElementAt(x, i < 0)   "arraycopy: source index -1 out of bounds for object array[10]"
//   add(i > size) / add(i < 0)  same two as insertElementAt
//   remove(i >= size)           "Array index out of range: 3"
//   remove(i < 0)               "Index -1 out of bounds for length 10"
//
// NOT converted: `subList`, which keeps the PLAIN IndexOutOfBoundsException
// ("toIndex = 5") -- measured. A blanket "Vector throws the array subclass"
// rule would be wrong for exactly that one method.
pub(crate) fn vec_arraycopy_negative(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    index: i32,
) -> MethodCallFailed {
    let (data, _) = al_state(ctx, this);
    let cap = data.map_or(0, |d| ctx.array_length(d));
    vec_aioobe(
        index,
        format!("arraycopy: source index {index} out of bounds for object array[{cap}]"),
    )
}

pub(crate) fn native_vec_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some((this, index)) = vec_index_args(args) else {
        return Ok(Some(Value::Object(None)));
    };
    let (_, size) = al_state(ctx, this);
    if index < 0 {
        return Err(vec_negative_index(ctx, this, index));
    }
    if index >= size {
        return Err(vec_aioobe(
            index,
            format!("Array index out of range: {index}"),
        ));
    }
    native_al_set(ctx, args)
}

pub(crate) fn native_vec_remove_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some((this, index)) = vec_index_args(args) else {
        return Ok(Some(Value::Object(None)));
    };
    let (_, size) = al_state(ctx, this);
    if index < 0 {
        return Err(vec_negative_index(ctx, this, index));
    }
    if index >= size {
        return Err(vec_aioobe(
            index,
            format!("Array index out of range: {index}"),
        ));
    }
    native_al_remove_at(ctx, args)
}

pub(crate) fn native_vec_add_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some((this, index)) = vec_index_args(args) else {
        return Ok(None);
    };
    let (_, size) = al_state(ctx, this);
    if index < 0 {
        return Err(vec_arraycopy_negative(ctx, this, index));
    }
    if index > size {
        return Err(vec_aioobe(index, format!("{index} > {size}")));
    }
    native_al_add_at(ctx, args)
}

/// JIT direct-call probe for `ArrayList.get(int)` on an ordinary
/// `java.util.ArrayList` with an in-range index (round 9 wave 3,
/// `perf-collection-natives-cost-a-generic-native-dispatch-per-call`).
///
/// `None` = not servable here (any receiver [`al_fast_state`] declines, an
/// out-of-range index whose exception the general path must build, the
/// `CRATONVM_DBG_ALTRACE` trace) — the caller must fall back to the full
/// dispatch path, which runs [`native_al_get`]. `Some(v)` is exactly what
/// `native_al_get` would have returned. Never allocates on the Java heap,
/// never dispatches Java code and never throws, so a VM-side thin helper may
/// call it without the `safe_native_call` wrapper — the same contract as
/// [`jit_overlay_hashmap_get`].
pub fn jit_arraylist_get(ctx: &dyn NativeContext, this: ObjectRef, index: i32) -> Option<Value> {
    if altrace_enabled() {
        return None;
    }
    let (data, size, _) = al_fast_state(ctx, this)?;
    if index < 0 || index >= size {
        return None;
    }
    Some(ctx.get_array_element(data, index as usize))
}

/// `size()` sibling of [`jit_arraylist_get`]: `Some(n)` is exactly what
/// [`native_al_size`] would have returned for this receiver, `None` means
/// "use the full dispatch path". Same no-allocation / no-Java / no-throw
/// contract.
pub fn jit_arraylist_size(ctx: &dyn NativeContext, this: ObjectRef) -> Option<i32> {
    al_fast_state(ctx, this).map(|(_, size, _)| size)
}

pub fn native_al_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Hot path: an ordinary `ArrayList` and an in-range index. Everything else
    // — every wrapper/view route below, an out-of-range index (whose exception
    // shape the general path owns), the `CRATONVM_DBG_ALTRACE` trace — falls
    // through unchanged. See `al_fast_state` for why this is equivalent.
    if let (Some(Value::Object(Some(this))), Some(Value::Int(index))) = (args.first(), args.get(1))
    {
        if !altrace_enabled() {
            if let Some((data, size, _)) = al_fast_state(&*ctx, *this) {
                if *index >= 0 && *index < size {
                    return Ok(Some(ctx.get_array_element(data, *index as usize)));
                }
            }
        }
    }
    // `List.get(int)` on a values view is not reachable from correct Java — the
    // real classes are `AbstractCollection`s and the cast to `List` throws
    // (`view.castToList=java.lang.ClassCastException`, measured). The guard is
    // here because this native is registered on `java/util/List` and can be
    // reached by a synthetic/reflective receiver regardless of what Java could
    // express, and answering element 0 of an object that has no elements is the
    // failure this whole family is about.
    if let Some(r) = vc_route(ctx, args, native_al_get) {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => return Ok(Some(Value::Object(None))),
    };
    if let Some(b) = unmod_receiver_backing(ctx, this) {
        // Bounds-check HERE, before delegating, and keep the
        // `ArrayIndexOutOfBoundsException` this path has always thrown.
        //
        // `native_al_get` below now throws the plain `IndexOutOfBoundsException`
        // that HotSpot's `ArrayList` throws -- correct for a real `ArrayList`
        // receiver and WRONG for most of the receivers that arrive here.
        // Measured against a HotSpot 25 control with
        // `probes/ListOutOfBoundsProbe`:
        //
        //   List.of()            -> ArrayIndexOutOfBoundsException
        //   List.of(a)/(a,b)     -> IndexOutOfBoundsException "Index: 3 Size: 1"
        //   List.of(a,b,c,d)     -> ArrayIndexOutOfBoundsException
        //
        // i.e. the JDK splits on `ImmutableCollections.List12` vs `ListN`, which
        // index their storage differently. This VM funnels every
        // unmodifiable/immutable list through ONE synthetic class -- but not
        // through one object shape: slot 1 carries `UNMOD_FIELD_IMMUTABLE`, and
        // `unmod_list_oob_error` makes the split from it. See that function for
        // the measured table and for why the `Collections.unmodifiable*` case
        // must delegate rather than answer here.
        //
        // The SIZE, though, must not come from `al_state`. That reads an
        // `elementData`-shaped backing and reports 0 for anything else, so an
        // unmodifiable view over a `LinkedList`, an `Arrays$ArrayList`, or any
        // other List threw on a PERFECTLY VALID index -- on a list that
        // iterates fine, because iteration takes a different path:
        //
        //   Collections.unmodifiableList(new LinkedList<>(List.of("x","y")))
        //       .get(0)   -> AIOOBE       (HotSpot: "x")
        //   Collections.unmodifiableList(Arrays.asList("p","q"))
        //       .get(0)   -> AIOOBE       (HotSpot: "p")
        //
        // That is what fails Spring's `CompileWithForkedClassLoaderExtension
        // .runTest:140` -- `summary.getFailures().get(0)`, where
        // `SummaryGeneratingListener.getFailures()` hands back exactly such a
        // view -- and with it both `AotIntegrationTests` end-to-end tests. The
        // nested run's REAL failure is destroyed on the way out and replaced by
        // this AIOOBE. Repro: docs/known-issues/repros/junit-summary-failures/
        // (SUM.java, 3 seconds) and .../list-get-oob/UM.java.
        //
        // Ask the backing for its own size, and read through its own `get`, so
        // any List shape works. `unmod_receiver_backing` is `None` for a plain
        // ArrayList, so the virtual call cannot come back here forever: the
        // innermost backing takes the normal path below.
        let (data, al_size) = al_state(ctx, b);
        if let Some(n) = unmod_view_size(data, al_size) {
            if index < 0 || index >= n {
                if let Some(err) = unmod_list_oob_error(&*ctx, this, index, n) {
                    return Err(err);
                }
                // `None` = a `Collections.unmodifiable*` view. Fall through to
                // the delegation below and let the backing throw its own.
            }
        }
        // Unreadable backing: stand aside, exactly as `native_unmod_get` does.
        return ctx.invoke_virtual(b, "get", "(I)Ljava/lang/Object;", &[Value::Int(index)]);
    }
    // `Collections$SingletonList` keeps its one element in a field, not in an
    // `elementData` array, so `al_state` reports size 0 and every index looked
    // out of range. Same reason `size()` needed `singleton_wrapper_size`.
    match singleton_wrapper_size(ctx, this) {
        Some(1) => {
            if index != 0 {
                // `Collections$SingletonList.get` -> `IndexOutOfBoundsException
                // ("Index: "+index+", Size: 1")`. Plain class, comma form.
                return Err(cratonvm_types::error::RuntimeError::ioobe(format!(
                    "Index: {index}, Size: 1"
                ))
                .into());
            }
            return Ok(Some(ctx.get_field_by_name(this, "element")));
        }
        Some(0) => {
            // `Collections$EmptyList.get` names no size at all — its message is
            // "Index: 0", full stop. Measured; not an omission here.
            return Err(
                cratonvm_types::error::RuntimeError::ioobe(format!("Index: {index}")).into(),
            );
        }
        _ => {}
    }
    let (this, data, size) = match al_state_for_read(ctx, this, false)? {
        AlRead::ViewSize(n) => return Ok(Some(Value::Int(n))),
        AlRead::State(t, d, s) => (t, d, s),
    };
    if index < 0 || index >= size {
        // HotSpot: IndexOutOfBoundsException, "Index N out of bounds for
        // length M". NOT the ArrayIndexOutOfBoundsException subclass -- that is
        // the direction that breaks a `catch`, and it is what this threw until
        // 2026-08-05 even though the comment already named the right class.
        // There was no plain variant in `RuntimeError` before then.
        return Err(cratonvm_types::error::RuntimeError::ioobe(format!(
            "Index {index} out of bounds for length {size}"
        ))
        .into());
    }
    let data = match data {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    let val = ctx.get_array_element(data, index as usize);
    if altrace_enabled() {
        let val_desc = altrace_describe(ctx, val);
        eprintln!(
            "[altrace GET] this={:p} buf={:p} index={index} size={size} val={val_desc}",
            this.as_ptr(),
            data.as_ptr(),
        );
    }
    Ok(Some(val))
}

pub fn native_al_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => return Ok(Some(Value::Object(None))),
    };
    let new_val = args.get(2).copied().unwrap_or(Value::Object(None));
    // Hot path: an ordinary `ArrayList`, in range. `al_state` below reads the
    // same pair for such a receiver (it declines strictly more), and nothing
    // between the read and the store can allocate.
    if let Some((data, size, _)) = al_fast_state(&*ctx, this) {
        if index >= 0 && index < size {
            let old = ctx.get_array_element(data, index as usize);
            ctx.set_array_element(data, index as usize, new_val);
            return Ok(Some(old));
        }
    }
    let (data, size) = al_state(ctx, this);
    if index < 0 || index >= size {
        // HotSpot: IndexOutOfBoundsException with the same wording as
        // `get`; see the note there for why this is not the Array... subclass.
        return Err(cratonvm_types::error::RuntimeError::ioobe(format!(
            "Index {index} out of bounds for length {size}"
        ))
        .into());
    }
    let data = match data {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    let old = ctx.get_array_element(data, index as usize);
    ctx.set_array_element(data, index as usize, new_val);
    Ok(Some(old))
}

/// Is `this` one of the JDK's IMMUTABLE stand-in classes, for which a
/// structural mutator must raise `UnsupportedOperationException`?
///
/// In real-JDK mode these receivers carry real bytecode — `Collections$EmptyList`
/// inherits `AbstractList.add`, which throws — so nothing here is consulted. In
/// `--synthetic-jdk` there is no bytecode, the interface-registered natives
/// serve the call instead, and they mutated happily. Measured 2026-09-02 with
/// `apps/probes/EmptySingletonImmutable`:
///
/// ```text
///     Collections.emptyList().add("x")        SUCCEEDED   (HotSpot: UOE)
///     Collections.emptyMap().put("k","v")     SUCCEEDED   (HotSpot: UOE)
///     Collections.singletonList("a").add("x") SUCCEEDED   (HotSpot: UOE)
///     Arrays.asList("a","b").add("x")         SUCCEEDED   (HotSpot: UOE)
/// ```
///
/// `List.of` / `Set.of` / `Map.of` / `unmodifiable*` were already correct in
/// every mode: they carry the `cratonvm/internal/Unmodifiable*` stamp, whose
/// own natives refuse. These seven are the ones minted under a JDK class name
/// with no such stamp.
///
/// **Structural mutators only.** `Arrays$ArrayList` is fixed-SIZE, not
/// immutable: `add`/`remove` throw on HotSpot and `set` is legal and writes
/// through to the backing array. That is why this is consulted from `add` and
/// `put` rather than from a blanket "any write" check — a guard that also
/// refused `set` would break `Arrays.asList(a).set(0, x)`, which is the
/// idiomatic reason to call `asList` at all.
pub(crate) fn is_immutable_jdk_stand_in(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    // `class_name_rc`, the per-thread memo, not `class_name_of_id`: this runs
    // on EVERY `ArrayList.add`, and `class_name_of_id` takes the class-manager
    // read lock and allocates a fresh `String` per call.
    let cid = ctx.class_id_of_object(this);
    let Some(name) = class_name_rc(&*ctx, cid) else {
        return false;
    };
    matches!(
        &*name,
        "java/util/Collections$EmptyList"
            | "java/util/Collections$EmptySet"
            | "java/util/Collections$EmptyMap"
            | "java/util/Collections$SingletonList"
            | "java/util/Collections$SingletonSet"
            | "java/util/Collections$SingletonMap"
            | "java/util/Arrays$ArrayList"
    )
}

/// `ArrayList.add(E)` for a direct Rust caller: a grow gets one no-GC attempt
/// ([`GrowAlloc::SingleAttempt`]), because the caller may hold bare references
/// across this call. The registered door is [`native_al_add_entry`]; a Rust
/// caller that pins what it holds uses [`native_al_add_entry_pub`].
pub fn native_al_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    al_add_impl(ctx, args, GrowAlloc::SingleAttempt)
}

/// The registered `ArrayList.add(E)`: reached only from Java dispatch or
/// `invoke_virtual`, so a grow may collect and retry ([`GrowAlloc::Reclaim`]).
pub(crate) fn native_al_add_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    al_add_impl(ctx, args, GrowAlloc::Reclaim)
}

/// `add` on a receiver whose class is not ArrayList-layout ([`AlLayout`]
/// refuses it), delegated to the receiver's own `add`.
///
/// Interpreter round i3 wave 79, lane L1
/// (`i74-L1-list-add-interface-native-writes-arraylist-slots-into-a-foreign-receiver`).
/// `native_al_add_entry` / `native_al_add_at_entry` are also registered on the
/// abstract `java/util/List.add` triples, so a door that resolves the
/// interface declaration (reflection on `List.class`, a method handle found
/// on `List`, `invoke_virtual` from a native) hands them a
/// `CopyOnWriteArrayList` or a `LinkedList`. `al_state` already reads such a
/// receiver as empty, but the write half then stored a fresh one-element
/// `Object[]` into the receiver's field `elementData`'s index names -- a
/// `CopyOnWriteArrayList`'s `array` -- and lost every earlier element.
/// `al_set_data` / `al_set_size` now refuse the store too; this runs the
/// receiver's real `add` (HotSpot's answer) first. `None` (a synthetic-backed
/// receiver, or the delegated call re-entering this native) leaves the caller
/// on its own path, whose writes are now no-ops for a foreign receiver.
///
/// Only the registered doors ([`GrowAlloc::Reclaim`]) take it: the delegated
/// body runs Java and may collect, which a direct Rust caller
/// ([`GrowAlloc::SingleAttempt`], holding bare references) does not allow.
#[cold]
#[inline(never)]
pub(crate) fn al_foreign_add_route(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    iface: &str,
    method: &'static str,
    descriptor: &str,
    rest: &[Value],
) -> Option<MethodCallResult> {
    try_delegate_real_collection_args(ctx, this, iface, method, descriptor, rest)
}

#[inline]
pub(crate) fn al_add_impl(ctx: &mut dyn NativeContext, args: &[Value], grow: GrowAlloc) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    // Hot path: an ordinary `ArrayList` with spare capacity. `al_fast_state`
    // rules out the view marker the `values_view_source` test below refuses,
    // and no immutable stand-in (`Collections$Empty*`/`Singleton*`,
    // `Arrays$ArrayList`) is an `ArrayList` subclass. With room in the backing
    // array `al_ensure_capacity` would hand back this same array without
    // allocating, so there is no GC point and nothing to pin; `al_set_size`
    // keeps the `modCount` bump the general path makes.
    if !altrace_enabled() {
        if let Some((data, size, len)) = al_fast_state(&*ctx, this) {
            if (size as usize) < len {
                ctx.set_array_element(data, size as usize, elem);
                al_set_size(ctx, this, size + 1);
                return Ok(Some(Value::Int(1)));
            }
        }
    }
    // An immutable JDK stand-in refuses structurally — see
    // `is_immutable_jdk_stand_in` for the measurement and for why `set` is not
    // guarded alongside `add`.
    if is_immutable_jdk_stand_in(ctx, this) {
        return Err(
            cratonvm_types::error::RuntimeError::UnsupportedOperationException {
                message: String::new(),
            }
            .into(),
        );
    }
    // A `values()` / TreeMap-`entrySet()` view is an `ArrayList` here, but it is
    // not addable. `Map.values`: "The collection supports element removal ... It
    // does not support the `add` or `addAll` operations"; the JDK's
    // `HashMap$Values extends AbstractCollection`, whose `add` is a bare
    // `throw new UnsupportedOperationException()`.
    //
    // `values.addUnsupported` measured `m.values().add(99)` returning `true`
    // here, leaving a value in the snapshot that no key in `m` maps to.
    //
    // The marker is the same one `native_al_remove_obj` / `native_al_clear` /
    // the iterator's `remove()` already consult to write removals through, so
    // this adds no new notion of "is a view" — it answers the question those
    // three already ask. A plain `ArrayList` cannot match it: the trailing
    // capacity slot of a normal list is null, and this test requires both a
    // non-null trailing slot AND `length > size`.
    if values_view_source(ctx, this).is_some() {
        return Err(
            cratonvm_types::error::RuntimeError::UnsupportedOperationException {
                message: String::new(),
            }
            .into(),
        );
    }
    // A foreign-layout receiver (reached through the abstract `List.add`
    // interface native) runs its own `add`; see `al_foreign_add_route`.
    if grow == GrowAlloc::Reclaim && !al_is_list_layout(ctx, this) {
        if let Some(r) = al_foreign_add_route(
            ctx,
            this,
            "java/util/Collection",
            "add",
            "(Ljava/lang/Object;)Z",
            &[elem],
        ) {
            return r;
        }
    }
    let (_, size) = al_state(ctx, this);
    let size = size as usize;
    // GC-SAFETY: `al_ensure_capacity` allocates (and can GC) internally on a
    // grow. Pin `this` (used again below in `al_set_size`) and `elem` across
    // it -- see the GC-SAFETY note on `al_ensure_capacity` itself.
    //
    // gc-common w9-d: under `GrowAlloc::Reclaim` that GC is a real collection
    // on a refused grow, and this pin frame is what makes it legal: `this` and
    // `elem` are the only references used afterwards, and both are re-read
    // below. The frame is released on the error path too (it used to leak up
    // to the caller's pin floor on the `Required array length too large` OOME).
    let this_pin = ctx.pin_native_root(this);
    let elem_pin = pin_value(ctx, elem);
    let this = ctx.read_native_pin(this_pin, this);
    let buf = match al_ensure_capacity(ctx, this, size + 1, grow) {
        Ok(buf) => buf,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let this = ctx.read_native_pin(this_pin, this);
    let elem = read_pinned_elem(ctx, elem_pin, elem);
    if altrace_enabled() {
        let elem_desc = altrace_describe(ctx, elem);
        eprintln!(
            "[altrace ADD] this={:p} buf={:p} at_index={size} elem={elem_desc}",
            this.as_ptr(),
            buf.as_ptr(),
        );
    }
    ctx.set_array_element(buf, size, elem);
    al_set_size(ctx, this, (size + 1) as i32);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Int(1))) // returns true
}

/// `ArrayList.add(int, E)` for a direct Rust caller ([`GrowAlloc::SingleAttempt`]);
/// the registered door is [`native_al_add_at_entry`]. See [`native_al_add`].
pub fn native_al_add_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    al_add_at_impl(ctx, args, GrowAlloc::SingleAttempt)
}

/// The registered `ArrayList.add(int, E)` ([`GrowAlloc::Reclaim`]).
pub(crate) fn native_al_add_at_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    al_add_at_impl(ctx, args, GrowAlloc::Reclaim)
}

pub(crate) fn al_add_at_impl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    grow: GrowAlloc,
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // Parse as i32 and reject negatives BEFORE casting to usize — a negative
    // Java int would otherwise wrap to a huge usize and pass the `> size` check.
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => return Ok(None),
    };
    let elem = args.get(2).copied().unwrap_or(Value::Object(None));
    // Before the bounds check: `al_state` reads a foreign receiver as size 0.
    if grow == GrowAlloc::Reclaim && !al_is_list_layout(ctx, this) {
        if let Some(r) = al_foreign_add_route(
            ctx,
            this,
            "java/util/List",
            "add",
            "(ILjava/lang/Object;)V",
            &[Value::Int(index), elem],
        ) {
            return r;
        }
    }
    let (_, size) = al_state(ctx, this);
    let size = size as usize;
    if index < 0 || index as usize > size {
        // HotSpot: IndexOutOfBoundsException, and note the wording is NOT the
        // one `get`/`set` use -- `ArrayList.add(int,E)` goes through
        // `rangeCheckForAdd`, which builds "Index: 9, Size: 2" (comma, and
        // "Size" rather than "length"). Measured, not assumed:
        // probes/ListOutOfBoundsProbe against a HotSpot 25 control shows four
        // different OOB wordings across the List API.
        return Err(cratonvm_types::error::RuntimeError::ioobe(format!(
            "Index: {index}, Size: {size}"
        ))
        .into());
    }
    let index = index as usize;
    // GC-SAFETY: same hazard as `native_al_add` -- `al_ensure_capacity` can
    // allocate/GC internally, and both `this` (used again in `al_set_size`)
    // and `elem` are live across it.
    let this_pin = ctx.pin_native_root(this);
    let elem_pin = pin_value(ctx, elem);
    let this = ctx.read_native_pin(this_pin, this);
    let buf = match al_ensure_capacity(ctx, this, size + 1, grow) {
        Ok(buf) => buf,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let this = ctx.read_native_pin(this_pin, this);
    let elem = read_pinned_elem(ctx, elem_pin, elem);
    // Shift elements right
    if !al_copy_refs_bulk(ctx, buf, index, buf, index + 1, size - index) {
        for i in (index..size).rev() {
            let val = ctx.get_array_element(buf, i);
            ctx.set_array_element(buf, i + 1, val);
        }
    }
    ctx.set_array_element(buf, index, elem);
    al_set_size(ctx, this, (size + 1) as i32);
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub fn native_al_remove_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, size) = al_state(ctx, this);
    if index < 0 || index >= size {
        // HotSpot: IndexOutOfBoundsException with the same wording as
        // `get`; see the note there for why this is not the Array... subclass.
        return Err(cratonvm_types::error::RuntimeError::ioobe(format!(
            "Index {index} out of bounds for length {size}"
        ))
        .into());
    }
    let data = match data {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    let old = ctx.get_array_element(data, index as usize);
    // Shift elements left
    let size = size as usize;
    let from = index as usize + 1;
    if !al_copy_refs_bulk(ctx, data, from, data, from - 1, size - from) {
        for i in from..size {
            let val = ctx.get_array_element(data, i);
            ctx.set_array_element(data, i - 1, val);
        }
    }
    // Null out the last element
    ctx.set_array_element(data, size - 1, Value::Object(None));
    al_set_size(ctx, this, (size - 1) as i32);
    Ok(Some(old))
}

pub fn native_al_remove_obj(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    // Live `values()` view: a successful removal must also drop the matching
    // entry from the source map.
    let view_src = values_view_source(ctx, this);
    // CP5-3: asked now, while `this` is current (pure reads).
    let by_value_reversed = view_src.is_some() && view_by_value_reversed(&*ctx, this);
    if let Some(source) = view_src {
        if values_remove_jdk_enabled()
            && plain_node_map(&*ctx, source)
            && !al_view_holds_entries(&*ctx, this)
        {
            if let Some(r) = values_view_remove_jdk(ctx, this, source, target) {
                return r;
            }
        }
    }
    let (data, size) = al_state(ctx, this);
    let data = match data {
        Some(d) => d,
        None => return Ok(Some(Value::Int(0))),
    };
    let size = size as usize;
    // Family-1 fix (cce0079): search via the pinned helper — the per-element
    // `equals()` dispatch can move `this`/`data`/`target`/the view source —
    // then shift through the REFRESHED references.
    let this_pin = ctx.pin_native_root(this);
    let vh = view_src.map(|v| ctx.pin_native_root(v));
    let (found, data, _target) = pinned_array_search(ctx, data, size, target)?;
    let this = ctx.read_native_pin(this_pin, this);
    let view_src = match (view_src, vh) {
        (Some(v), Some(h)) => Some(ctx.read_native_pin(h, v)),
        _ => None,
    };
    ctx.unpin_native_roots(this_pin);
    if let Some(i) = found {
        // Re-read the matched element from the refreshed array BEFORE the
        // shift overwrites its slot (it is handed to the view-source
        // propagation below).
        let elem = ctx.get_array_element(data, i);
        // Close the gap: shift the tail [i+1..size) left by one into
        // [i..size-1). Use the bulk intrinsic (memmove-style overlap is
        // handled by the VM) with a per-element fallback, mirroring
        // `native_al_add_all`.
        let tail_len = size - i - 1;
        al_copy_elements(ctx, data, i + 1, data, i, tail_len);
        ctx.set_array_element(data, size - 1, Value::Object(None));
        al_set_size(ctx, this, (size - 1) as i32);
        if let Some(source) = view_src {
            propagate_list_removal_in(ctx, source, elem, by_value_reversed)?;
        }
        return Ok(Some(Value::Int(1)));
    }
    Ok(Some(Value::Int(0)))
}

pub fn native_al_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // Live `values()` view: clearing the view clears the source map.
    // GC-safety: that `clear()` is arbitrary Java and can complete a moving
    // young GC, leaving `this` a pre-move address for everything below.
    let this_pin = ctx.pin_native_root(this);
    let cleared = match values_view_source(ctx, this) {
        Some(source) => ctx.invoke_virtual(source, "clear", "()V", &[]).map(|_| ()),
        None => Ok(()),
    };
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    cleared?;
    let (data, size) = al_state(ctx, this);
    if let Some(d) = data {
        for i in 0..(size as usize) {
            ctx.set_array_element(d, i, Value::Object(None));
        }
    }
    al_set_size(ctx, this, 0);
    Ok(None)
}

pub fn native_al_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = ksv_route(ctx, args, native_ksv_contains) {
        return r;
    }
    if let Some(r) = vc_route(ctx, args, native_al_contains) {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let this = resync_values_view(ctx, this)?;
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    if let Some(cls_name) = ctx.class_name_of_id(ctx.class_id_of_object(this)) {
        if cls_name == "java/util/EnumSet" {
            if let Value::Object(Some(backing)) = ctx.get_field(this, 0) {
                return native_al_contains(ctx, &[Value::Object(Some(backing)), target]);
            }
        }
        if cls_name == "java/util/RegularEnumSet" || cls_name == "java/util/JumboEnumSet" {
            let contained = match target {
                Value::Object(Some(elem)) => {
                    enum_set_contains_member(&*ctx, this, elem).unwrap_or(false)
                }
                _ => false,
            };
            return Ok(Some(Value::Int(if contained { 1 } else { 0 })));
        }
    }
    let (data, size) = al_state(ctx, this);
    if let Some(data) = data {
        // Family-1 fix (cce0079): pinned scan — the `equals()` dispatch can
        // move `data`/`target` mid-walk.
        let (found, _, _) = pinned_array_search(ctx, data, size as usize, target)?;
        if found.is_some() {
            return Ok(Some(Value::Int(1)));
        }
    } else {
        // Family-1 fix (cce0079): pin the snapshot vector's elements — a GC
        // triggered by any iteration's `equals()` leaves the LATER raw
        // `Vec` slots stale.
        //
        // 2026-08-01 (collections-interception audit): use the wrapper, not the
        // bare `collect_collection_elements`. This native is registered on
        // `java/util/AbstractCollection.contains`, and `AbstractCollection` is
        // the base whose *documented* minimal subclass contract is "implement
        // `iterator()` and `size()`" — i.e. exactly the receivers whose layout
        // no heuristic here models. The bare collector returns an empty `Vec`
        // for those, so `contains` answered a flat `false` for every element of
        // such a collection: not a crash, a plausible wrong answer. The wrapper
        // asks the receiver's own `size()` and falls back to its real
        // `iterator()`. No recursion risk: the `iterator()` native snapshots
        // through `collect_collection_elements`, which never drives
        // `iterator()`, and never calls `contains`.
        let elems = al_or_collection_elements(ctx, this)?;
        let (pin_base, handles) = pin_value_slice(ctx, &elems);
        let th = pin_value(ctx, target);
        let mut target = target;
        let mut found = false;
        for (i, orig) in elems.iter().enumerate() {
            let elem = read_pinned_elem(ctx, handles[i], *orig);
            let matched = list_element_matches(ctx, &elem, &target)?;
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
        if found {
            return Ok(Some(Value::Int(1)));
        }
    }
    Ok(Some(Value::Int(0)))
}

pub fn native_al_index_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data, size) = al_state(ctx, this);
    let data = match data {
        Some(d) => d,
        None => return Ok(Some(Value::Int(-1))),
    };
    // Family-1 fix (cce0079): pinned scan.
    let (found, _, _) = pinned_array_search(ctx, data, size as usize, target)?;
    Ok(Some(Value::Int(found.map_or(-1, |i| i as i32))))
}

pub(crate) fn native_al_last_index_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data, size) = al_state(ctx, this);
    let data = match data {
        Some(d) => d,
        None => return Ok(Some(Value::Int(-1))),
    };
    let size = size as usize;
    // Family-1 fix (cce0079): pinned reverse scan — the `equals()` dispatch
    // can move `data`/`target` mid-walk.
    let data_pin = ctx.pin_native_root(data);
    let th = pin_value(ctx, target);
    let mut data = data;
    let mut target = target;
    let mut found = None;
    for i in (0..size).rev() {
        let elem = ctx.get_array_element(data, i);
        let matched = list_element_matches(ctx, &elem, &target)?;
        data = ctx.read_native_pin(data_pin, data);
        target = read_pinned_elem(ctx, th, target);
        if matched {
            found = Some(i);
            break;
        }
    }
    ctx.unpin_native_roots(data_pin);
    Ok(Some(Value::Int(found.map_or(-1, |i| i as i32))))
}

/// `toArray()` for a direct Rust caller: the result array gets one no-GC
/// attempt ([`GrowAlloc::SingleAttempt`]), because such a caller may hold bare
/// references across the call. The registered door is
/// [`native_al_to_array_entry`]; a Rust caller that pins what it holds uses
/// [`native_al_to_array_entry_pub`] (gc-common w11-c moved the four
/// `native-builtins` callers there).
pub fn native_al_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    al_to_array_impl(ctx, args, GrowAlloc::SingleAttempt)
}

/// The registered `toArray()` (`ArrayList`, `AbstractCollection` and the
/// other collection registrars): reached only from Java dispatch or
/// `invoke_virtual`, so a refused result array may collect and retry
/// ([`GrowAlloc::Reclaim`]; gc-common w10-g).
pub(crate) fn native_al_to_array_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    al_to_array_impl(ctx, args, GrowAlloc::Reclaim)
}

pub(crate) fn al_to_array_impl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    grow: GrowAlloc,
) -> MethodCallResult {
    if let Some(r) = ksv_route(ctx, args, native_ksv_to_array) {
        return r;
    }
    // A rebuilt view carrier re-enters through the door this call came in by,
    // so it keeps the caller's policy.
    let this_door: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult = match grow {
        GrowAlloc::SingleAttempt => native_al_to_array,
        GrowAlloc::Reclaim => native_al_to_array_entry,
    };
    if let Some(r) = vc_route(ctx, args, this_door) {
        return r;
    }
    if cratonvm_types::flags::runtime_var("CRATONVM_DBG_TOARRAY").is_ok() {
        eprintln!(
            "[DBG_TOARRAY] native_al_to_array (0-arg) HIT nargs={}",
            args.len()
        );
    }
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `collect_collection_elements` only knows fixed collection layouts and
    // returns empty for any other Collection — but this native is also
    // registered on `AbstractCollection.toArray()`, so it intercepts *every*
    // Collection, including custom user/library subclasses (e.g. ByteBuddy's
    // `TypeList$Generic$Explicit`, which backs the interface list of every
    // generated class). For those, `toArray()` must honour the real
    // `Collection` contract — drive the real `iterator()`. That fallback, and
    // the null-hole guard that used to be inlined here, now live in
    // `al_or_collection_elements` so the `toArray(T[])`, `toArray(IntFunction)`,
    // `forEach` and `stream` natives — which are intercepting the very same
    // receivers — get them too.
    let elems = al_or_collection_elements(ctx, this)?;
    // GC-SAFETY: `elems` contains bare object refs collected before the
    // result array allocation. `alloc_ref_array` can trigger a moving GC;
    // pin and refresh every object element before storing it into the new
    // array, otherwise callers can later iterate stale refs that report as
    // `java/lang/Object` and fail erased writer/checkcast bridges.
    //
    // gc-common w10-g, `live` audit for `GrowAlloc::Reclaim`: every element is
    // pinned just above and re-read just below; `this` is not used after the
    // allocation; `result` is the fresh array. Nothing else is live.
    let (pin_base, elem_handles) = pin_value_slice(ctx, &elems);
    let result = new_ref_array_grow(ctx, ClassId::new(0), elems.len(), grow);
    let elems = read_value_slice(ctx, &elem_handles, &elems);
    for (i, val) in elems.iter().enumerate() {
        ctx.set_array_element(result, i, *val);
    }
    if pin_base != usize::MAX {
        ctx.unpin_native_roots(pin_base);
    }
    Ok(Some(Value::Object(Some(result))))
}

/// Last-resort element collection for a Collection whose layout
/// `collect_collection_elements` doesn't model: drive the real `iterator()`
/// via virtual dispatch. ONLY safe to call from paths the `iterator()` native
/// does not itself route through (e.g. `toArray()`), since the iterator native
/// snapshots non-list collections through `collect_collection_elements`.
///
/// Fallible for the reason spelled out on [`collection_elements_generic`]: an
/// `Err` from any of the three calls below is a refusal or a thrown exception,
/// and returning `Vec::new()` for it hands the caller a wrong answer that
/// every caller-that-only-iterates reads as a pass. `Ok` with nothing usable
/// still means "nothing to walk" and still yields an empty `Vec`.
pub(crate) fn collect_via_real_iterator(
    ctx: &mut dyn NativeContext,
    coll: ObjectRef,
) -> Result<Vec<Value>, MethodCallFailed> {
    let it = match ctx.invoke_virtual(coll, "iterator", "()Ljava/util/Iterator;", &[]) {
        Ok(Some(Value::Object(Some(it)))) => it,
        Err(e) => return Err(e),
        Ok(_) => return Ok(Vec::new()),
    };
    if tmview_dbg() {
        eprintln!(
            "[TMVIEW] collect_via_real_iterator recv={} itr={}",
            ctx.class_name_of_id(ctx.class_id_of_object(coll))
                .unwrap_or_else(|| "<unknown>".into()),
            ctx.class_name_of_id(ctx.class_id_of_object(it))
                .unwrap_or_else(|| "<unknown>".into())
        );
    }
    let mut out = Vec::new();
    let it_pin = ctx.pin_native_root(it);
    // GC-safety for the ELEMENTS, not just the iterator.
    //
    // Every `hasNext`/`next` here runs arbitrary Java and can complete a moving
    // young collection, and `out` is a plain Rust `Vec` — no GC root, no remap.
    // So each turn of this loop can relocate every element collected so far,
    // and the caller receives a vector whose earlier entries name pre-move
    // addresses. It is the generic form of the defect: this one function backs
    // `toArray`, `addAll`, `forEach` and `stream` for every foreign collection.
    //
    // MEASURED on BindableTests under `CRATONVM_DBG_GC_STRESS=262144`:
    // `[deadref-pin]` fired inside `native_al_to_array`'s `pin_value_slice`,
    // pinning an element this loop had already let go stale.
    //
    // `it_pin` is the group base, so one `unpin_native_roots(it_pin)` releases
    // the iterator and every element pin together.
    let mut handles: Vec<usize> = Vec::new();
    let result = (|| -> Result<(), MethodCallFailed> {
        loop {
            let it = ctx.read_native_pin(it_pin, it);
            match ctx.invoke_virtual(it, "hasNext", "()Z", &[]) {
                Ok(Some(Value::Int(n))) if n != 0 => {}
                Err(e) => return Err(e),
                _ => break,
            }
            // gc-common w20-a: `hasNext` ran Java and may have moved the
            // iterator; `next` used to be dispatched on the address read
            // before it.
            let it = ctx.read_native_pin(it_pin, it);
            match ctx.invoke_virtual(it, "next", "()Ljava/lang/Object;", &[]) {
                Ok(Some(v)) => {
                    handles.push(pin_value(ctx, v));
                    out.push(v);
                }
                Err(e) => return Err(e),
                _ => break,
            }
            // Safety bound against a misbehaving iterator that never reports done.
            if out.len() > 16_777_216 {
                break;
            }
        }
        Ok(())
    })();
    // Read every element back through its pin before the group is released —
    // the point of taking them.
    for (i, h) in handles.iter().enumerate() {
        out[i] = read_pinned_elem(ctx, *h, out[i]);
    }
    ctx.unpin_native_roots(it_pin);
    result?;
    Ok(out)
}

/// Recursion-safe wrapper around [`collect_via_real_iterator`].
///
/// The real `iterator()` of a foreign collection can land back in a
/// collection native that itself wants a fallback snapshot. One level is all
/// any caller needs, so a re-entrant request yields an empty Vec (and the
/// caller keeps whatever it already had) instead of recursing.
pub(crate) fn collect_via_real_iterator_once(
    ctx: &mut dyn NativeContext,
    coll: ObjectRef,
) -> Result<Vec<Value>, MethodCallFailed> {
    use std::cell::Cell;
    thread_local! {
        static IN_REAL_ITER: Cell<bool> = const { Cell::new(false) };
    }
    if IN_REAL_ITER.with(Cell::get) {
        return Ok(Vec::new());
    }
    IN_REAL_ITER.with(|g| g.set(true));
    let out = collect_via_real_iterator(ctx, coll);
    // Cleared on the failing path too. `out` is now a `Result`, and an early
    // `?` here would leave the guard latched for the rest of the thread's
    // life — every later fallback walk on this thread would answer empty,
    // which is the exact laundering this change exists to remove.
    IN_REAL_ITER.with(|g| g.set(false));
    out
}

/// True iff a heuristic element snapshot of `coll` must not be trusted.
///
/// The layout probes in [`collect_collection_elements`] assume the backing
/// array is a dense `arr[0..size)` prefix. That is a *list* invariant. A
/// foreign open-addressed collection keeps NULL holes for empty buckets in
/// exactly those slots, so the probe returns the right element *count* made up
/// of mostly nulls and looks entirely plausible — the failure only shows up
/// when the caller dereferences one. Known members of that shape: MSC's
/// `org.jboss.msc.service.IdentityHashSet`, Kafka's
/// `ImplicitLinkedHashCollection`, Jetty's `BlockingArrayQueue`; the latter two
/// carry hand-written special cases in `collect_collection_elements` precisely
/// because of this, which is what this general guard replaces the need for.
///
/// Concretely closed here: `new HashSet<>().addAll(mscIdentityHashSet)` turned
/// a 3-element set into `{null}`, so WildFly's `ContainerStateMonitor`
/// iterated a null `ServiceController` and every boot that reported a failed
/// service died on `NullPointerException: ... "controller" is null` instead of
/// logging the report.
///
/// A `List` may legitimately hold nulls at any index, so nulls are only
/// suspicious in a non-`List` receiver. Note this deliberately does NOT consult
/// `is_synthetic_backed_collection`: its `al_is_list_layout` arm matches on the
/// very `(Object[], int)` shape an open-addressed foreign set has, so it
/// reported MSC's `IdentityHashSet` as synthetic-backed and suppressed the
/// guard. Callers only *adopt* the real-iterator answer when it is non-empty,
/// and CratonVM's own synthetic sets/maps resolve `iterator()` to their own
/// (correct) natives, so re-deriving is at worst equivalent for them.
pub(crate) fn heuristic_snapshot_is_suspect(
    ctx: &mut dyn NativeContext,
    coll: ObjectRef,
    elems: &[Value],
) -> bool {
    if !elems.iter().any(|v| matches!(v, Value::Object(None))) {
        return false;
    }
    !obj_is_instance_of(ctx, coll, "java/util/List")
}

/// Read elements for the `toArray` / `forEach` / `stream` natives. These are
/// registered on `java/util/AbstractCollection` (and on the `Collection` /
/// `List` interfaces), so they intercept **every** Collection that does not
/// declare its own override — JDK, third-party and user subclasses alike.
/// `al_state` cannot be trusted to return `None` for those: for a
/// `RegularEnumSet` it reports an empty ArrayList (size 0), which made
/// `EnumSet.allOf(...).toArray()`/`forEach`/`new ArrayList<>(enumSet)` all see
/// zero elements.
///
/// [`collect_collection_elements`] alone is NOT enough, and its own closing
/// comment says so: it must never drive `iterator()`, because the
/// `Iterable`/`Collection` `iterator()` native snapshots *through* it and would
/// recurse — so a layout it does not hand-model materialises as an EMPTY `Vec`
/// there, and "empty" is indistinguishable from "genuinely empty". Supplying
/// the fallback is this wrapper's job. Every caller of this wrapper
/// (`toArray()`, `toArray(T[])`, `toArray(IntFunction)`, `forEach`, `stream`)
/// is off the iterator native's path, so driving the receiver's real
/// `iterator()` from here is safe.
///
/// **2026-08-01 (collections-interception audit).** Until this change the
/// fallback lived inside `native_al_to_array` only. The zero-arg `toArray()`
/// therefore answered correctly for an unmodelled receiver while
/// `toArray(T[])`, `toArray(IntFunction)`, `forEach` and `stream` silently
/// answered EMPTY for the *same* receiver — e.g. `coll.toArray(new T[0])` over
/// a user `class X extends AbstractCollection` (which only has to implement
/// `iterator()` and `size()`) returned a zero-length array rather than the
/// elements, and `x.forEach(...)` visited nothing. The doc comments on
/// `native_al_to_array_typed`, `native_al_for_each` and `native_al_stream`
/// already claimed this fallback existed; now it does.
///
/// Ordering mirrors what `native_al_to_array` did, so the zero-arg path keeps
/// its exact behaviour:
/// 1. heuristic snapshot; if it looks suspect (null holes in a non-`List`),
///    prefer a re-entrancy-guarded real-iterator walk;
/// 2. if the heuristics found nothing, ask the receiver's own `size()` — it is
///    not shadowed on `AbstractCollection` (`size()` is abstract there, so the
///    dispatch walk stops at the subclass's own bytecode) — and only pay for a
///    real-iterator walk when it reports a non-empty collection.
pub(crate) fn al_or_collection_elements(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<Vec<Value>, MethodCallFailed> {
    // GC-safety: every step below can run arbitrary Java (`invoke_virtual`),
    // which may complete a moving young collection and leave `this` stale.
    //
    // Split so the pin is released on the failing path as well. The body has
    // always had a `?` in it (`collect_collection_elements`) and now has three
    // more, and an early return past `unpin_native_roots` leaks a root for the
    // rest of the run — the object is then immortal, and a later
    // `unpin_native_roots` taken at a *lower* base silently truncates it away.
    let this_pin = ctx.pin_native_root(this);
    let out = al_or_collection_elements_pinned(ctx, this, this_pin);
    ctx.unpin_native_roots(this_pin);
    out
}

pub(crate) fn al_or_collection_elements_pinned(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    this_pin: usize,
) -> Result<Vec<Value>, MethodCallFailed> {
    let mut elems = collect_collection_elements(ctx, this)?;
    let this = ctx.read_native_pin(this_pin, this);
    let suspect = !elems.is_empty() && heuristic_snapshot_is_suspect(ctx, this, &elems);
    // `CRATONVM_DBG_TOARRAY`: the layout-probe answer as it stands BEFORE
    // either re-derivation below can overwrite it. Reading `nulls` beside
    // `suspect` is what separates "the probe found nothing" (`heuristic_len=0`,
    // falls through to the `size()`+real-iterator path) from "the probe found
    // holes and the guard caught them" — the two ways a `toArray`-family answer
    // goes wrong, which are indistinguishable from the caller.
    if cratonvm_types::flags::runtime_var("CRATONVM_DBG_TOARRAY").is_ok() {
        let cid = ctx.class_id_of_object(this);
        let nm = ctx
            .class_name_arc_of_id(cid)
            .map_or_else(|| "<unknown>".to_string(), |a| a.to_string());
        let nulls = elems
            .iter()
            .filter(|v| matches!(v, Value::Object(None)))
            .count();
        eprintln!(
            "[DBG_TOARRAY] al_or_collection_elements recv={nm} heuristic_len={} nulls={nulls} suspect={suspect}",
            elems.len()
        );
    }
    if !elems.is_empty() {
        if suspect {
            let this = ctx.read_native_pin(this_pin, this);
            // The `?` matters here even though we already hold `elems`:
            // `heuristic_snapshot_is_suspect` has just said the snapshot in
            // hand must NOT be trusted (null holes in a non-`List`), so
            // swallowing a refusal from the real walk would return the
            // untrusted answer as if it were the checked one.
            //
            // gc-common w20-a: an empty real walk keeps `elems`, which the walk
            // (Java) can have moved — pinned across it and read back.
            let (base, handles) = pin_value_slice(ctx, &elems);
            let real = collect_via_real_iterator_once(ctx, this);
            elems = read_value_slice(ctx, &handles, &elems);
            if base != usize::MAX {
                ctx.unpin_native_roots(base);
            }
            let real = real?;
            if !real.is_empty() {
                elems = real;
            }
        }
        return Ok(elems);
    }
    // A refused `size()` is not "size 0". Answering 0 here skips the walk and
    // returns the empty heuristic snapshot, which is the wrong answer this
    // whole family launders.
    let real_size = match ctx.invoke_virtual(this, "size", "()I", &[]) {
        Ok(Some(Value::Int(n))) => n,
        Err(e) => return Err(e),
        _ => 0,
    };
    let this = ctx.read_native_pin(this_pin, this);
    if real_size > 0 {
        elems = collect_via_real_iterator(ctx, this)?;
    }
    if tmview_dbg() {
        eprintln!(
            "[TMVIEW] al_or_collection_elements fallback real_size={real_size} walked={}",
            elems.len()
        );
    }
    Ok(elems)
}

/// `ArrayList.toArray(T[])` / `AbstractCollection.toArray(T[])` —
/// produce a typed array (or grow the supplied template) without going
/// through the bytecode's `Arrays.copyOf(elementData, size, a.getClass())`
/// path which NPEs on synthetic ArrayLists.
pub fn native_al_to_array_typed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `CRATONVM_DBG_TOARRAY`, at the very top so it reports the calls the
    // routes below divert as well as the ones that reach the body. The
    // ABSENCE of this line is itself a finding: this native is registered on
    // `java/util/AbstractCollection` by `register_collections_natives`, but
    // real-JDK mode registers `real_jdk_to_array_typed` (`vm/src/vm/
    // vm_init.rs`) over the top of it, so a silent trace on a real-JDK run
    // means the OTHER implementation is the one under investigation.
    if cratonvm_types::flags::runtime_var("CRATONVM_DBG_TOARRAY").is_ok() {
        let cls = match args.first() {
            Some(Value::Object(Some(o))) => {
                let cid = ctx.class_id_of_object(*o);
                ctx.class_name_arc_of_id(cid)
                    .map_or_else(|| "<unknown>".to_string(), |a| a.to_string())
            }
            _ => "<null>".to_string(),
        };
        eprintln!("[DBG_TOARRAY] native_al_to_array_typed ENTER recv={cls}");
    }
    reject_null_to_array_template(args.get(1))?;
    if let Some(r) = ksv_route(ctx, args, native_ksv_to_array_typed) {
        return r;
    }
    if let Some(r) = vc_route(ctx, args, native_al_to_array_typed) {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let template = args.get(1).copied().unwrap_or(Value::Object(None));
    // gc-common w10-g: `al_or_collection_elements` can run Java (a foreign
    // collection's `iterator()`) and so collect, and the template is read
    // after it -- stored INTO when it is big enough. It was a bare copy of the
    // argument; root it across the walk. It is only read (never allocated
    // around) after this point, so the pin ends here.
    let template_pin = pin_value(ctx, template);
    let elems = al_or_collection_elements(ctx, this);
    let template = read_pinned_elem(ctx, template_pin, template);
    if template_pin != usize::MAX {
        ctx.unpin_native_roots(template_pin);
    }
    let elems = elems?;
    let size = elems.len();
    if cratonvm_types::flags::runtime_var("CRATONVM_DBG_TOARRAY").is_ok() {
        eprintln!(
            "[DBG_TOARRAY] native_al_to_array_typed HIT nargs={} size={} template_some={}",
            args.len(),
            size,
            matches!(template, Value::Object(Some(_)))
        );
    }
    // GC-SAFETY: the target allocation below can move object refs already
    // captured in `elems`; refresh them from native pins before storing into
    // the returned array. This mirrors the zero-arg `toArray()` path above.
    //
    // gc-common w10-g: the allocation may collect and retry
    // (`GrowAlloc::Reclaim`). This native is registered-only (`ArrayList` and
    // `AbstractCollection.toArray(T[])`, and its own view route); no Rust code
    // calls it. `live` audit: the elements are pinned just above and re-read
    // just below; the template is read for its class id BEFORE the allocation
    // and not used after it; `this` is not used after it.
    let (pin_base, elem_handles) = pin_value_slice(ctx, &elems);
    let target = match template {
        Value::Object(Some(arr)) if ctx.array_length(arr) >= size => arr,
        // Template too small: allocate a NEW array of the template's runtime
        // component type (JDK contract `Arrays.copyOf(elementData, size,
        // a.getClass())`), NOT a bare `Object[]`. For an array object the heap
        // header stores its component class id, so `class_id_of_object(arr)` IS
        // the component class id `new_ref_array` wants ? preserving multi-dim
        // types (`Value[][]` for H2 SortOrder.sort, not `Object[]`).
        Value::Object(Some(arr)) => {
            let comp = ctx.class_id_of_object(arr);
            new_ref_array_grow(ctx, comp, size, GrowAlloc::Reclaim)
        }
        _ => new_ref_array_grow(ctx, ClassId::new(0), size, GrowAlloc::Reclaim),
    };
    let elems = read_value_slice(ctx, &elem_handles, &elems);
    // `ArrayList.toArray(T[])` copies through `Arrays.copyOf` /
    // `System.arraycopy`, so a narrowing element is an `ArrayStoreException` in
    // arraycopy's wording. The source is named `java.lang.Object[]` because
    // that is what `ArrayList.elementData` is, whatever the list's element type
    // -- and it is what HotSpot prints (MEASURED, `probes/DodArrayStoreSweep`).
    for (i, val) in elems.iter().enumerate() {
        if let Some(e) = cratonvm_native_api::array_store::reject_unstorable(
            ctx,
            target,
            *val,
            cratonvm_native_api::array_store::StoreRoute::Arraycopy {
                source_component: "java/lang/Object",
            },
        ) {
            if pin_base != usize::MAX {
                ctx.unpin_native_roots(pin_base);
            }
            return Err(e);
        }
        ctx.set_array_element(target, i, *val);
    }
    let target_len = ctx.array_length(target);
    if target_len > size {
        ctx.set_array_element(target, size, Value::Object(None));
    }
    if pin_base != usize::MAX {
        ctx.unpin_native_roots(pin_base);
    }
    Ok(Some(Value::Object(Some(target))))
}

/// Collection.toArray(IntFunction) — invoke the generator to preserve the
/// caller-requested runtime array type, then copy this collection's elements.
pub(crate) fn native_collection_to_array_generator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `al_or_collection_elements`' fallback walks the receiver's own
    // `iterator()`, which for a real view class is the JDK's table-walking
    // iterator over a table this VM never populates — i.e. silently empty.
    // Route before that fallback can be taken.
    if let Some(r) = vc_route(ctx, args, native_collection_to_array_generator) {
        return r;
    }
    let generator = args.get(1).copied().unwrap_or(Value::Object(None));
    let elems = al_or_collection_elements(ctx, this)?;
    let size = elems.len();

    // The generator call and the target allocation can move objects already
    // captured in `elems`; pin all element refs and refresh before storing.
    let inferred_component = elems.iter().find_map(|v| match v {
        Value::Object(Some(o)) => Some(ctx.class_id_of_object(*o)),
        _ => None,
    });
    let (pin_base, elem_handles) = pin_value_slice(ctx, &elems);
    let gen_pin = match generator {
        Value::Object(Some(g)) => Some(ctx.pin_native_root(g)),
        _ => None,
    };

    // gc-common w10-g: every fallback allocation below may collect and retry
    // (`GrowAlloc::Reclaim`); this native is registered-only. `live` audit:
    // the elements and the generator are pinned just above and re-read after;
    // `this` is not used after the walk; an undersized `target` is read for
    // its class id BEFORE its replacement is allocated and dropped after;
    // `inferred_component` is a `ClassId`. `ClassId::new(0)` is
    // `alloc_ref_array`'s `Object[]`, the historical no-component fallback.
    let fallback_component = inferred_component.unwrap_or(ClassId::new(0));
    let mut target = match generator {
        Value::Object(Some(g)) => {
            let g = gen_pin.map_or(g, |pin| ctx.read_native_pin(pin, g));
            match ctx.invoke_virtual(
                g,
                "apply",
                "(I)Ljava/lang/Object;",
                &[Value::Int(size as i32)],
            ) {
                Ok(Some(Value::Object(Some(arr)))) if ctx.object_is_array(arr) => arr,
                _ => new_ref_array_grow(ctx, fallback_component, size, GrowAlloc::Reclaim),
            }
        }
        _ => new_ref_array_grow(ctx, fallback_component, size, GrowAlloc::Reclaim),
    };

    if ctx.array_length(target) < size {
        let comp = if ctx.object_is_array(target) {
            ctx.class_id_of_object(target)
        } else {
            fallback_component
        };
        target = new_ref_array_grow(ctx, comp, size, GrowAlloc::Reclaim);
    }

    let target_pin = ctx.pin_native_root(target);
    let elems = read_value_slice(ctx, &elem_handles, &elems);
    for (i, val) in elems.iter().enumerate() {
        let target = ctx.read_native_pin(target_pin, target);
        ctx.set_array_element(target, i, *val);
    }
    let target = ctx.read_native_pin(target_pin, target);
    if ctx.array_length(target) > size {
        ctx.set_array_element(target, size, Value::Object(None));
    }

    let mut unpin_base = target_pin;
    if let Some(pin) = gen_pin {
        unpin_base = unpin_base.min(pin);
    }
    if pin_base != usize::MAX {
        unpin_base = unpin_base.min(pin_base);
    }
    ctx.unpin_native_roots(unpin_base);
    Ok(Some(Value::Object(Some(target))))
}

pub fn native_al_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = ksv_route(ctx, args, native_ksv_iterator) {
        return r;
    }
    // A real view class has no element array to iterate. Rebuilding the live
    // carrier here also preserves write-through `remove()`, because the carrier
    // it builds is the marker-carrying one `propagate_list_removal` understands.
    if let Some(r) = vc_route(ctx, args, native_al_iterator) {
        return r;
    }
    // The `Hashtable` family's THIRD door. `keySet()` and `entrySet()` are
    // set-shaped and route through `native_hs_iterator`; `values()` is
    // ArrayList-shaped and arrives here, so without this the same map answered
    // `Hashtable$Enumerator` for two of its views and `ArrayList$Itr` for the
    // third. See [`real_ht_view_enumerator`] — java.base's own cursor, no
    // natives registered on it and no force-native gate entry.
    if let Some(Value::Object(Some(recv))) = args.first().copied() {
        if let Some(src_map) = values_view_source(&*ctx, recv) {
            if let Some(e) = real_ht_view_enumerator(ctx, recv, src_map)? {
                return Ok(Some(Value::Object(Some(e))));
            }
        }
    }
    let input = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `resync_values_view` and every iterator allocation below may collect.
    // Keep the receiver rooted from entry, and refresh it when the resync
    // returns the original object.
    let roots_base = ctx.pin_native_root(input);
    let resynced = resync_values_view(ctx, input)?;
    let this = if std::ptr::eq(resynced.as_ptr(), input.as_ptr()) {
        ctx.read_native_pin(roots_base, input)
    } else {
        resynced
    };
    let this_pin = ctx.pin_native_root(this);
    // `java/util/List.iterator()` / `Collection.iterator()` / `Iterable.iterator()`
    // are all wired to this native at the interface level (see
    // `register_interface_natives`).  When the receiver is *not* a
    // `java/util/ArrayList`-compatible object (i.e. its instance layout
    // doesn't have `elementData` at the ArrayList slot), the ArrayList$Itr
    // we build below reads from the wrong field slots and yields an empty
    // iteration — silently breaking real-JDK collections that the
    // interpreter cannot dispatch to via bytecode.
    //
    // The canonical offender is `java/util/concurrent/CopyOnWriteArrayList`
    // (used by Spring's `BeanPostProcessorCacheAwareList`): COWAL stores
    // its backing `Object[]` in the `array` field at instance-slot 1
    // (slot 0 is the `lock` monitor), with no separate `size` field.
    // Sending such a receiver through the ArrayList path makes
    // `al_state` read `lock` as `elementData` (returns `None`), so
    // `ArrayList$Itr.hasNext()` reports `false` on the first call and
    // every loop over the list runs zero iterations.  In Spring 6+ this
    // manifests as `internalConfigurationAnnotationProcessor` setter
    // injection failing with
    //   `Property 'metadataReaderFactory' threw exception: ...
    //    MetadataReaderFactory must not be null`
    // because `ApplicationContextAwareProcessor.setResourceLoader` is
    // never invoked on `SharedMetadataReaderFactoryBean` — the
    // `applyBeanPostProcessorsBeforeInitialization` loop over
    // `beanPostProcessors` produces zero items.
    //
    // Route COWAL receivers (including subclasses) to a snapshot
    // iterator backed by the live `array` field.
    let receiver_class_id = ctx.class_id_of_object(this);
    let cowal_cid = ctx.class_id_by_name("java/util/concurrent/CopyOnWriteArrayList");
    if let Some(cowal_id) = cowal_cid {
        if ctx.is_subclass(receiver_class_id, cowal_id) {
            // Snapshot COWAL's `array` field by resolving the slot by name
            // — works whether the runtime saw the real JDK class file or a
            // synthetic stub.  `size()` on COWAL is `array.length`, so we
            // wrap the snapshot in an ArrayList-shaped object (slot 0 =
            // backing array, slot 1 = size) and build a normal
            // ArrayList$Itr on top.  We do *not* mutate the original COWAL.
            let this = ctx.read_native_pin(this_pin, this);
            let arr_slot =
                ctx.resolve_field_index("java/util/concurrent/CopyOnWriteArrayList", "array");
            let snapshot = arr_slot.and_then(|s| match ctx.get_field(this, s) {
                Value::Object(Some(a)) => Some(a),
                _ => None,
            });
            // Answer what the JDK's `iterator()` answers: a real
            // `COWIterator` over the same snapshot (`new COWIterator<E>(
            // getArray(), 0)`), so the class, `remove()`
            // (UnsupportedOperationException) and the `ListIterator` face are
            // HotSpot's. `i72-L1-interface-iterator-native-hands-a-cowal-an-arraylist-itr-20261105`.
            // Wave 73 gated this on the `JdkCopyOnWrite` row; wave 76 (lane
            // L1, the row graduated) answers it whatever the row says: the
            // ArrayList$Itr wrapper below is reached only on a class library
            // with no real `COWIterator` (`real_cowal_iterator` answers `None`
            // there: the synthetic library).
            let mut snapshot = snapshot;
            if let Some(snap) = snapshot {
                // GC-SAFETY: minting runs Java (`<clinit>`, the constructor);
                // keep the snapshot rooted for the fallback.
                let snap_pin = ctx.pin_native_root(snap);
                let built = real_cowal_iterator(ctx, snap_pin, snap);
                let snap = ctx.read_native_pin(snap_pin, snap);
                ctx.unpin_native_roots(snap_pin);
                if let Some(itr) = built? {
                    ctx.note_alignment_divergence(cratonvm_types::HotSpotAlignment::JdkCopyOnWrite);
                    ctx.unpin_native_roots(roots_base);
                    return Ok(Some(Value::Object(Some(itr))));
                }
                snapshot = Some(snap);
            }
            let snap_len = snapshot.map(|a| ctx.array_length(a) as i32).unwrap_or(0);
            let backing = snapshot.unwrap_or_else(|| alloc_ref_array(ctx, 0));
            let wrapper = alloc_arraylist_with(ctx, backing, snap_len)?;
            let itr = alloc_arraylist_iterator(ctx, wrapper)?;
            ctx.unpin_native_roots(roots_base);
            return Ok(Some(Value::Object(Some(itr))));
        }
    }
    // EnumSet reached via the `Iterable.iterator()` interface native: the
    // indexed ArrayList$Itr below would call `this.get(cursor)`, but
    // RegularEnumSet has no `get(int)`, and `al_state` can't read its elements
    // either (it would iterate zero / NoSuchMethodError). Snapshot the real
    // elements into an ArrayList-shaped wrapper and iterate THAT. Scoped to
    // EnumSet specifically (by class name) so ordinary lists keep their live,
    // remove()-capable iterator — `is_subclass`-based detection is unreliable
    // (it reports false for ArrayList under the unit-test mock context, which
    // would wrongly snapshot a real ArrayList and break `iterator().remove()`).
    {
        let cls = ctx
            .class_name_of_id(ctx.class_id_of_object(this))
            .unwrap_or_default();
        // `java/nio/file/Path` reaches this Iterable.iterator() bridge when a
        // caller iterates a Path via the `Iterable` super-interface (the JDK's
        // `Iterable.forEach` default does exactly this: `for (T t : this)`).
        // The synthetic Path is not ArrayList-layout, so snapshot its real name
        // elements (via `collect_collection_elements`, which special-cases Path)
        // and iterate the snapshot — mirroring the EnumSet path below. The
        // Path-class `iterator()` native (phases_late) still serves direct
        // `path.iterator()` calls.
        if cls == "java/util/EnumSet"
            || cls == "java/util/RegularEnumSet"
            || cls == "java/util/JumboEnumSet"
            || cls == "java/nio/file/Path"
        {
            let elems = collect_collection_elements(ctx, this)?;
            let (elem_pin_base, elem_handles) = pin_value_slice(ctx, &elems);
            let backing = alloc_ref_array(ctx, elems.len());
            let backing_pin = ctx.pin_native_root(backing);
            for (i, v) in elems.iter().enumerate() {
                let backing = ctx.read_native_pin(backing_pin, backing);
                let v = read_pinned_elem(ctx, elem_handles[i], *v);
                ctx.set_array_element(backing, i, v);
            }
            let backing = ctx.read_native_pin(backing_pin, backing);
            let wrapper = alloc_arraylist_with(ctx, backing, elems.len() as i32)?;
            let itr = alloc_arraylist_iterator(ctx, wrapper)?;
            if elem_pin_base != usize::MAX {
                ctx.unpin_native_roots(elem_pin_base);
            }
            ctx.unpin_native_roots(roots_base);
            return Ok(Some(Value::Object(Some(itr))));
        }
    }
    // Create the iterator. A values-shaped VIEW gets its family's real class
    // (`VALUES_ITR_CARRIERS`); an ordinary list gets `ArrayList$Itr` at its
    // name-resolved slots, exactly as before.
    let this = ctx.read_native_pin(this_pin, this);
    let carrier = values_itr_carrier_for(&*ctx, this);
    let itr = alloc_arraylist_iterator_as(ctx, this, carrier)?;
    ctx.unpin_native_roots(roots_base);
    Ok(Some(Value::Object(Some(itr))))
}

/// The snapshot iterator the three collection families hand out: their REAL
/// iterator class, over a wrapper that carries the source.
///
/// One helper for `TreeSet`, `ArrayDeque` and `PriorityQueue` because the three
/// wanted the same four things and had three hand-written approximations of
/// them, each landing on a different wrong class. It gives all four at once:
///
/// * the class HotSpot hands out (`apps/probes/FailFastShapeProbe`), through
///   `alloc_arraylist_iterator_as`, which REFUSES rather than fabricates and
///   falls back to the plain shape when the carrier is not real;
/// * `native_al_itr_*` bound to that class, from the `VALUES_ITR_CARRIERS`
///   registration loop;
/// * write-through `remove()`, from the source in the wrapper's trailing
///   capacity slot, which `propagate_list_removal` routes to the family's own
///   removal native;
/// * fail-fast, from `al_view_itr_seed` reading `family_snapshot_generation`
///   into the carrier's declared `expectedModCount`.
///
/// The wrapper never escapes: it is minted here, handed to the iterator, and
/// never returned, so the marker's blast radius is the natives this iterator
/// reaches and not every consumer of the marker.
///
/// GC: every allocation below can move the source, the elements and the array,
/// so the source is pinned first and read back through its pin, the elements
/// through theirs. `unpin_native_roots(src_pin)` releases all of them at once
/// -- pins are a stack and this is the outermost.
pub(crate) fn alloc_family_snapshot_iterator(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
    elems: &[Value],
    carrier: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let n = elems.len();
    let src_pin = ctx.pin_native_root(source);
    let (_, handles) = pin_value_slice(ctx, elems);
    let arr = alloc_ref_array(ctx, n + 1);
    let arr_pin = ctx.pin_native_root(arr);
    for (i, v) in elems.iter().enumerate() {
        let v = read_pinned_elem(ctx, handles[i], *v);
        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.set_array_element(arr, i, v);
    }
    let arr = ctx.read_native_pin(arr_pin, arr);
    let source_now = ctx.read_native_pin(src_pin, source);
    ctx.set_array_element(arr, n, Value::Object(Some(source_now)));
    let wrapper = match alloc_arraylist_with(ctx, arr, n as i32) {
        Ok(w) => w,
        Err(e) => {
            ctx.unpin_native_roots(src_pin);
            return Err(e);
        }
    };
    let wrap_pin = ctx.pin_native_root(wrapper);
    let wrapper = ctx.read_native_pin(wrap_pin, wrapper);
    let itr = alloc_arraylist_iterator_as(ctx, wrapper, Some(carrier));
    ctx.unpin_native_roots(src_pin);
    itr
}

/// Build an ArrayList iterator without retaining either the list or the new
/// iterator as a raw Rust local across allocation/reference stores.
///
/// W7-1 family 2 (2026-08-11): this used to write only `this$0` and `cursor`
/// and leave `lastRet` at whatever `alloc_object` zero-initialises an `int` to,
/// i.e. **0** — which `native_al_itr_remove` reads as "`next()` returned index
/// 0". So `l.iterator().remove()` with no `next()` in front of it did not raise
/// `IllegalStateException`: it silently deleted `l.get(0)`, and every later
/// `next(); remove()` pair was then one element out of step. That single missing
/// store is what produced four of the record's measured rows —
/// `Iterator.removeBeforeNext`, `Iterator.afterRemove` (`[c, d]` for `[b, c, d]`)
/// and, purely as a cascade of the list already being one element short,
/// `ListIterator.afterSetAdd` (`[B, B2, d]` for `[B, B2, c, d]`). `set`/`add`
/// were never wrong themselves.
///
/// `expectedModCount` is seeded from the list's live `modCount` for the same
/// reason the real constructor does it — see `al_itr_check_comod`.
pub(crate) fn alloc_arraylist_iterator(
    ctx: &mut dyn NativeContext,
    list: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    alloc_arraylist_iterator_as(ctx, list, None)
}

/// [`alloc_arraylist_iterator`] under a named real carrier class, whose three
/// snapshot fields go past its own declared ones. `None` keeps the plain
/// `java.util.ArrayList$Itr` shape at its name-resolved slots.
pub(crate) fn alloc_arraylist_iterator_as(
    ctx: &mut dyn NativeContext,
    list: ObjectRef,
    carrier: Option<&str>,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(cls) = carrier {
        // A stand-in is refused rather than fabricated: minting a fake
        // `HashMap$ValueIterator` is exactly what `--jdk-only` exists to stop,
        // and its methods would be unbound. Fall back to the plain shape, which
        // is what every values view had before.
        if !ctx.would_fabricate_synthetic_stub(cls) && !ctx.is_class_synthetic_stub(cls) {
            if let Ok(cid) = ctx.ensure_class_initialized(cls) {
                if ctx.class_name_arc_of_id(cid).as_deref() == Some(cls) {
                    let n = ctx.class_num_total_fields(cid) + AL_ITR_NUM_FIELDS;
                    let roots_base = ctx.pin_native_root(list);
                    let itr = ctx.alloc_object(cid, n);
                    let itr_pin = ctx.pin_native_root(itr);
                    let list = ctx.read_native_pin(roots_base, list);
                    let itr = ctx.read_native_pin(itr_pin, itr);
                    let b = n - AL_ITR_NUM_FIELDS;
                    ctx.set_field(itr, b + AL_ITR_FIELD_LIST, Value::Object(Some(list)));
                    ctx.set_field(itr, b + AL_ITR_FIELD_CURSOR, Value::Int(0));
                    ctx.set_field(itr, b + AL_ITR_FIELD_LAST_RET, Value::Int(-1));
                    let itr = ctx.read_native_pin(itr_pin, itr);
                    // The values-shaped views' `expectedModCount = modCount`.
                    // It goes in the carrier's OWN declared slot, not in the
                    // three snapshot fields above — see `al_view_itr_seed`.
                    let list = ctx.read_native_pin(roots_base, list);
                    al_view_itr_seed(ctx, itr, list);
                    let itr = ctx.read_native_pin(itr_pin, itr);
                    ctx.unpin_native_roots(roots_base);
                    return Ok(itr);
                }
            }
        }
    }
    // The split that lets `java/util/ArrayList$Itr` mean "a real `modCount`".
    // See [`itr_backing_has_real_mod_count`]; the layout and slot triple are
    // identical either way, so every `al_itr_*` native reads both the same.
    let itr_class = if itr_backing_has_real_mod_count(&*ctx, list) {
        "java/util/ArrayList$Itr"
    } else {
        AL_VIEW_ITR_CLASS
    };
    let roots_base = ctx.pin_native_root(list);
    // FS16-3 (r17 w2, lane smallfix): resolved AFTER the pin, because
    // `al_itr_slots_loaded` may load `ArrayList$Itr` (a load can collect).
    let (cursor_slot, list_slot, n_fields) = al_itr_slots_loaded(ctx);
    // THE REFUSAL LANDING. `AL_VIEW_ITR_CLASS` is a fabricated class, so under
    // `--jdk-only` this allocation is refused — and with a bare `?` that
    // refusal propagated as `NoClassDefFoundError` and killed the caller
    // BEFORE `next()`, which is the Phase-1 shape the roadmap is about. The two
    // sibling mint sites (`native_ksv_iterator`, `native_hs_iterator`) have
    // landed their refusals on `real_snapshot_iterator` since 2026-08-11; this
    // one never got the arm.
    //
    // `Compatible` is untouched: `try_alloc_synthetic` succeeds there, so this
    // arm never runs and the fabricated carrier is still what `--real-jdk`
    // hands back.
    //
    // The snapshot is an EXACT-LENGTH COPY rather than the view's own backing
    // array. `real_snapshot_iterator` would otherwise use the array directly,
    // and `SnapshotItrRoute::ViewCollection`'s `remove()` shifts that same
    // array underneath the iterator — the cursor would then skip the element
    // after every removal. The fabricated carrier never had that problem
    // because its `next()` re-reads the live list each time.
    let itr = match try_alloc_synthetic(ctx, itr_class, n_fields) {
        Ok(itr) => itr,
        Err(_refused) => {
            let list = ctx.read_native_pin(roots_base, list);
            let (data, size) = al_state(&*ctx, list);
            let count = size.max(0) as usize;
            let snapshot = alloc_ref_array(ctx, count);
            let snap_pin = ctx.pin_native_root(snapshot);
            if let Some(src) = data {
                let src_pin = ctx.pin_native_root(src);
                for i in 0..count {
                    let src = ctx.read_native_pin(src_pin, src);
                    if i >= ctx.array_length(src) {
                        break;
                    }
                    let v = ctx.get_array_element(src, i);
                    let snapshot = ctx.read_native_pin(snap_pin, snapshot);
                    ctx.set_array_element(snapshot, i, v);
                }
            }
            let snapshot = ctx.read_native_pin(snap_pin, snapshot);
            let list = ctx.read_native_pin(roots_base, list);
            let real = real_snapshot_iterator(
                ctx,
                snapshot,
                count,
                Some((list, SnapshotItrRoute::ViewCollection)),
            );
            ctx.unpin_native_roots(roots_base);
            return match real {
                Ok(Some(Value::Object(Some(o)))) => Ok(o),
                Ok(_) => Err(_refused),
                Err(e) => Err(e),
            };
        }
    };
    let itr_pin = ctx.pin_native_root(itr);
    let itr = ctx.read_native_pin(itr_pin, itr);
    let list = ctx.read_native_pin(roots_base, list);
    ctx.set_field(itr, list_slot, Value::Object(Some(list)));
    let itr = ctx.read_native_pin(itr_pin, itr);
    ctx.set_field(itr, cursor_slot, Value::Int(0));
    let itr = ctx.read_native_pin(itr_pin, itr);
    // `-1` is the JDK's "no element has been returned yet" sentinel, and the
    // value `ArrayList$Itr.remove()` restores after every successful removal.
    let last_ret_slot = al_itr_last_ret_slot(ctx);
    if last_ret_slot < ctx.object_num_fields(itr) {
        ctx.set_field(itr, last_ret_slot, Value::Int(-1));
    }
    let itr = ctx.read_native_pin(itr_pin, itr);
    if let Some(expected_slot) = al_itr_expected_mod_count_slot(ctx, itr) {
        let seen = al_mod_count(ctx, list).unwrap_or(0);
        ctx.set_field(itr, expected_slot, Value::Int(seen));
    }
    let itr = ctx.read_native_pin(itr_pin, itr);
    ctx.unpin_native_roots(roots_base);
    Ok(itr)
}

pub(crate) fn native_al_ensure_capacity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let min_cap = match args.get(1) {
        Some(Value::Int(c)) => std::cmp::max(*c, 0) as usize,
        _ => return Ok(None),
    };
    // Registered-only (`ArrayList.ensureCapacity`) and nothing is used after
    // the call, so the grow may collect (gc-common w9-d, `GrowAlloc`).
    al_ensure_capacity(ctx, this, min_cap, GrowAlloc::Reclaim)?;
    Ok(None)
}

pub(crate) fn native_al_trim_to_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let (data, size) = al_state(ctx, this);
    let size = size as usize;
    let old_cap = data.map_or(0, |d| ctx.array_length(d));
    if size < old_cap {
        // GC-safety: the allocation can move both `this` and the old buffer;
        // both are used afterwards. See `rooted_across`.
        //
        // gc-common w10-g: registered-only (`ArrayList.trimToSize`), so the
        // allocation may collect and retry. `live` audit: `this` and the old
        // buffer are the two roots `rooted_across` holds and rewrites; nothing
        // else is used afterwards.
        let mut this = this;
        let mut old = data.unwrap_or(this);
        let new_buf = rooted_across(ctx, &mut [&mut this, &mut old], |ctx| {
            new_ref_array_grow(ctx, ClassId::new(0), size, GrowAlloc::Reclaim)
        });
        let data = data.map(|_| old);
        if let Some(old_buf) = data {
            al_copy_elements(ctx, old_buf, 0, new_buf, 0, size);
        }
        al_set_data(ctx, this, new_buf);
    }
    Ok(None)
}

pub fn native_al_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::fmt::Write as _;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // A `values()` view is LIVE, and this was the one element-reading native
    // that never said so: `native_al_get`, `native_al_contains`,
    // `native_al_iterator`, `native_al_for_each` and `native_al_stream` all
    // open with a resync, and `toString` read the capture-time backing array
    // straight out of `al_state`. MEASURED on all five map families
    // (`scratchpad/g22/G22Fam.java`, the `.values.live` row): after
    // `m.put("d","4"); m.remove("a")` on a three-entry map, `v.size()` answered
    // the live 3 but `v.toString()` answered the captured `[1, 2, 3]` where HotSpot
    // answers `[2, 3, 4]`. `resync_values_view` returns `list` unchanged for a
    // receiver with no view marker, so an ordinary `ArrayList.toString()` pays
    // one `al_state` it was going to pay anyway.
    let this = resync_values_view(ctx, this)?;
    let (data, size) = al_state(ctx, this);
    let size = size as usize;
    // Pre-size: "[" + N elements averaging ~16 chars each + (N-1) ", " + "]".
    // Single allocation avoids the intermediate Vec<String> + join() walk.
    let mut text = Units::with_capacity(2 + size.saturating_mul(18));
    text.push(u16::from(b'['));
    if let Some(d) = data {
        // gc-common w20-a: pinned across the elements' `toString()` calls.
        push_array_display_units(ctx, &mut text, d, size)?;
    }
    text.push(u16::from(b']'));
    let s = ctx.create_string_from_units(&text);
    Ok(Some(Value::Object(Some(s))))
}

pub(crate) fn native_al_add_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Ahead of the receiver: `ArrayList.addAll` / `Vector.addAll` open
    // `Object[] a = c.toArray()`, so a null argument throws whatever the
    // receiver holds -- HotSpot's helpful message (C4W4-6).
    reject_null_collection_asked(&*ctx, args.get(1), NULL_ARG_AL_ADD_ALL, NULL_C_TO_ARRAY)?;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    // A `values()`/`entrySet()` view is LIVE, and the fast path below reads a
    // view's CAPTURED `(elementData, size)` straight out of `al_state`. That
    // makes this the one remaining reader of a view's raw slots that never went
    // through the resync funnel: `collect_collection_elements_pinned` opens with
    // one ("the read has to go through the funnel like every other reader"),
    // and `native_al_get` / `contains` / `iterator` / `for_each` / `stream` /
    // `toString` each open with their own. `addAll` reads the ARGUMENT rather
    // than the receiver, which is why the receiver-side sweep missed it.
    //
    // MEASURED (`apps/probes/StaleViewAddAllProbe.java`, --nojit, all three map
    // families): put a,b,c; `addAll(m.values())` -> 3; put d,e;
    // `addAll(m.values())` -> **3**, where `size()`, `toArray()`, the for-each
    // and `new ArrayList<>(m.values())` on the very same view all answer 5.
    //
    // The consumer that found it is Hibernate's
    // `InFlightMetadataCollectorImpl.collectTableMappings()` --
    // `new ArrayList<>()` then `addAll(namespace.getTables())`, where
    // `getTables()` is `tables.values()` on a map that GROWS when Envers
    // contributes its audit tables. The stale copy left `Country_AUD` out of the
    // table set the foreign-key second pass walks, so
    // `ForeignKey.referencedTable` was never assigned and schema export threw
    // `NullPointerException: Cannot invoke "org.hibernate.mapping.Table
    // .isPhysicalTable()" because "this.referencedTable" is null` --
    // `HibernateJpaAutoConfigurationTests` (4 methods) and
    // `DataJpaRepositoriesWithEnversRevisionAutoConfigurationTests` on the
    // twelve-unclustered Spring residuals page.
    //
    // `resync_values_view` returns its argument unchanged for a receiver with no
    // view marker, so an ordinary `ArrayList.addAll(ArrayList)` pays one field
    // read it was going to pay anyway. It CAN allocate (it rebuilds the view's
    // backing array), so `this` is pinned across it.
    let this_pin = ctx.pin_native_root(this);
    let other = resync_values_view(ctx, other)?;
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    // C2W3-2 (`CRATONVM_COMPAT_COPY_CTOR_JDK_QUESTIONS`): `addAll` opens
    // `Object[] a = c.toArray()`, so a foreign source is asked exactly that --
    // no layout read (the fast path below also reads a program's own
    // `ArrayList` subclass without calling its `toArray()` override).
    let foreign = copy_ctor_jdk_questions_enabled() && copy_source_is_foreign(&*ctx, other);
    // Fast path: the source is a plain ArrayList whose synthetic
    // (data, size) slots are directly readable — copy its backing
    // array in one shot.
    let (other_data, other_size) = if foreign {
        (None, 0)
    } else {
        al_state(ctx, other)
    };
    let other_size = other_size as usize;
    if let (Some(other_data), true) = (other_data, other_size > 0) {
        let (_, my_size) = al_state(ctx, this);
        let my_size = my_size as usize;
        // GC-SAFETY: same `al_ensure_capacity` allocation hazard as
        // `native_al_add` -- `this` (used again in `al_set_size`) and
        // `other_data` (the source array, read again below) are both live
        // across it.
        //
        // gc-common w9-d: `native_al_add_all` is reached only through its
        // registrations (`ArrayList.addAll`, `Vector.addAll`), and this frame
        // pins everything used after the grow (`other` is not), so the grow
        // may collect (`GrowAlloc::Reclaim`).
        let this_pin = ctx.pin_native_root(this);
        let other_data_pin = ctx.pin_native_root(other_data);
        let this = ctx.read_native_pin(this_pin, this);
        let buf = match al_ensure_capacity(ctx, this, my_size + other_size, GrowAlloc::Reclaim) {
            Ok(buf) => buf,
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };
        let this = ctx.read_native_pin(this_pin, this);
        let other_data = ctx.read_native_pin(other_data_pin, other_data);
        al_copy_elements(ctx, other_data, 0, buf, my_size, other_size);
        al_set_size(ctx, this, (my_size + other_size) as i32);
        ctx.unpin_native_roots(this_pin);
        return Ok(Some(Value::Int(1)));
    }
    // General path: the source is NOT a plain ArrayList — it may be a
    // `Collections.unmodifiableList(...)` view, an `Arrays.asList(...)`
    // (`java/util/Arrays$ArrayList`), a `List.of(...)` snapshot, a
    // HashSet, etc. `al_state` only understands the synthetic ArrayList
    // layout, so it reports size 0 for all of these and the elements
    // are silently dropped. Fall back to `collect_collection_elements`,
    // which sees through every wrapper and reads arbitrary collections
    // via their real layout / iterator. This is the path WildFly's
    // `PathAddress.append(...)` depends on (`ArrayList.addAll` of a
    // `Collections.unmodifiableList` parent address) — without it the
    // appended PathAddress comes out empty and resource-tree
    // registration NPEs in `ConcreteResourceRegistration.registerSubModel`.
    // `_or_real` adds the real-`toArray()` fallback so a real-bytecode source
    // (ConcurrentLinkedQueue, LinkedList, …) the layout heuristics can't read
    // still contributes its elements.
    // GC-SAFETY, and the pin has to be taken BEFORE this call rather than after
    // it: `collect_collection_elements_or_real` sees through every wrapper and,
    // for a real-bytecode source, calls the collection's own `toArray()` — so it
    // allocates and can run arbitrary Java. `this` is live across it and was
    // being read afterwards unprotected.
    //
    // MEASURED on BindableTests under `CRATONVM_DBG_GC_STRESS=262144`:
    // `[deadref-pin]` caught the `pin_native_root(this)` that used to sit below
    // this call being handed an address naming no live object — the pin
    // faithfully preserving a dead value instead of protecting a live one.
    let this_pin = ctx.pin_native_root(this);
    let collected = if foreign {
        foreign_to_array_elements(ctx, other)
    } else {
        collect_collection_elements_or_real(ctx, other)
    };
    let elems = match collected {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let this = ctx.read_native_pin(this_pin, this);
    if elems.is_empty() {
        ctx.unpin_native_roots(this_pin);
        // `Object[] a = c.toArray(); modCount++; ... if (numNew == 0) return
        // false;` (`ArrayList` and `Vector` alike): the bump comes first.
        if al_add_all_empty_mod_count_enabled() {
            al_bump_mod_count(ctx, this);
        }
        return Ok(Some(Value::Int(0)));
    }
    let (_, my_size) = al_state(ctx, this);
    let my_size = my_size as usize;
    // Same `al_ensure_capacity` allocation hazard as `native_al_add`: every
    // object-typed element of `elems` is written into `buf` after it.
    // (gc-common w9-d: every element and `this` are pinned, so the grow may
    // collect -- see the fast path above.)
    let (_, elems_handles) = pin_value_slice(ctx, &elems);
    let this = ctx.read_native_pin(this_pin, this);
    let buf = match al_ensure_capacity(ctx, this, my_size + elems.len(), GrowAlloc::Reclaim) {
        Ok(buf) => buf,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let this = ctx.read_native_pin(this_pin, this);
    let elems = read_value_slice(ctx, &elems_handles, &elems);
    for (i, val) in elems.iter().enumerate() {
        ctx.set_array_element(buf, my_size + i, *val);
    }
    al_set_size(ctx, this, (my_size + elems.len()) as i32);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Int(1)))
}

pub(crate) fn native_al_sub_list(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let from_i32 = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let to_i32 = match args.get(2) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let (_, size) = al_state(ctx, this);
    // `AbstractList.subListRangeCheck`: the plain class, and a wording that
    // names WHICH endpoint failed rather than printing an index. The
    // `fromIndex > toIndex` case below is an IllegalArgumentException, not a
    // bounds exception at all, and already was.
    if from_i32 < 0 {
        return Err(
            cratonvm_types::error::RuntimeError::ioobe(format!("fromIndex = {from_i32}")).into(),
        );
    }
    if to_i32 > size {
        return Err(
            cratonvm_types::error::RuntimeError::ioobe(format!("toIndex = {to_i32}")).into(),
        );
    }
    if from_i32 > size || to_i32 < 0 {
        return Err(cratonvm_types::error::RuntimeError::ioobe(format!(
            "fromIndex = {from_i32}, toIndex = {to_i32}"
        ))
        .into());
    }
    if from_i32 > to_i32 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: format!("fromIndex({from_i32}) > toIndex({to_i32})"),
            }
            .into(),
        );
    }
    let from = from_i32 as usize;
    let to = to_i32 as usize;
    let sub_size = to.saturating_sub(from);
    // Fix (item 6): return a real BACKED VIEW, not a detached copy. The view
    // shares the parent's backing array, so `get`/`set` read and WRITE THROUGH
    // to the parent (the old copy silently diverged: mutations on either side
    // were invisible to the other). Structural changes to the parent are
    // detected via the captured parent size and surface as
    // `ConcurrentModificationException`; structural mutation OF the sublist
    // throws `UnsupportedOperationException` (fail loud) rather than diverging.
    // See `register_al_sublist_natives` and the `ASL_*` handlers below.
    let parent_size = size;
    // Fallible since 2026-08-05 (JDK-only wave 2, lane L7): the real
    // `ArrayList.subList` returns a `java.util.ArrayList$SubList` running real
    // bytecode, so handing back this stand-in under `--jdk-only` is exactly the
    // substitution §5 forbids. Refuse there instead, naming the class.
    // GC-SAFETY: `alloc_asl_view` allocates (and may run a `<clinit>`), and
    // `this` is stored into the new view immediately afterwards — a from-space
    // `this` here is the root list every later read of the view indexes into.
    let this_pin = ctx.pin_native_root(this);
    let view = match alloc_asl_view(ctx) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let view_pin = ctx.pin_native_root(view);
    let this = ctx.read_native_pin(this_pin, this);
    let view = ctx.read_native_pin(view_pin, view);
    let base = asl_base(ctx, view);
    ctx.set_field(view, base + ASL_FIELD_PARENT, Value::Object(Some(this)));
    ctx.set_field(view, base + ASL_FIELD_OFFSET, Value::Int(from as i32));
    ctx.set_field(view, base + ASL_FIELD_SIZE, Value::Int(sub_size as i32));
    ctx.set_field(view, base + ASL_FIELD_EXPECTED, Value::Int(parent_size));
    // Explicit, not left to whatever `alloc_object` zero-initialises a slot of
    // an undeclared shape to: W7-1's `lastRet` was exactly this hazard, an
    // int-zero read as a meaningful value. `this` is a real `ArrayList` here,
    // so there is no enclosing view to propagate to.
    ctx.set_field(view, base + ASL_FIELD_VIEW_PARENT, Value::Object(None));
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(view))))
}

/// The enclosing view of a nested `subList`, or `None` for a view carved
/// directly out of an `ArrayList`. Guards on the field count so a view built by
/// some other path with the older four-field shape simply reports "no enclosing
/// view" (it degrades to the pre-existing behaviour) rather than reading past
/// the object.
pub(crate) fn asl_view_parent(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    let base = asl_base(ctx, this);
    if ctx.object_num_fields(this) <= base + ASL_FIELD_VIEW_PARENT {
        return None;
    }
    match ctx.get_field(this, base + ASL_FIELD_VIEW_PARENT) {
        Value::Object(Some(p)) => Some(p),
        _ => None,
    }
}

/// `subList` ON a sublist — a real nested view, not a view of a snapshot.
///
/// This was registered as `asl_delegate_snapshot`, which materialised the
/// slice into a fresh `ArrayList` and returned a view of THAT. Reads were
/// right; every write went into the throwaway. Measured:
///
/// ```text
/// subList.nestedClearWritesThroughToBase  HotSpot [a, B, e]  CratonVM [a, B, c, d, e]
/// subList.afterNestedClear                HotSpot [B]        CratonVM [B, c, d]
/// ```
///
/// The composition is on the ROOT: the nested view points at the same backing
/// `ArrayList` with `offset = enclosing.offset + fromIndex`, so it needs no
/// interval arithmetic at read time and every existing `asl_*` native works on
/// it unchanged. The enclosing view is recorded separately, in
/// [`ASL_FIELD_VIEW_PARENT`], only so a structural mutation can walk back up.
///
/// Range check is against the ENCLOSING VIEW's size, not the root's, and uses
/// `AbstractList.subListRangeCheck`'s wording — the same three arms as
/// `native_al_sub_list`, and note the `fromIndex > toIndex` case is an
/// `IllegalArgumentException` rather than a bounds exception at all.
pub(crate) fn native_asl_sub_list(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = asl_delegate_foreign(ctx, args, "subList", "(II)Ljava/util/List;") {
        return r;
    }

    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let from_i32 = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let to_i32 = match args.get(2) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let (parent, _, size, expected) = match asl_state(ctx, this) {
        Some(s) => s,
        None => return Ok(Some(Value::Object(None))),
    };
    asl_check_comod(ctx, parent, expected)?;
    if from_i32 < 0 {
        return Err(
            cratonvm_types::error::RuntimeError::ioobe(format!("fromIndex = {from_i32}")).into(),
        );
    }
    if to_i32 > size {
        return Err(
            cratonvm_types::error::RuntimeError::ioobe(format!("toIndex = {to_i32}")).into(),
        );
    }
    if from_i32 > size || to_i32 < 0 {
        return Err(cratonvm_types::error::RuntimeError::ioobe(format!(
            "fromIndex = {from_i32}, toIndex = {to_i32}"
        ))
        .into());
    }
    if from_i32 > to_i32 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: format!("fromIndex({from_i32}) > toIndex({to_i32})"),
            }
            .into(),
        );
    }
    // GC-SAFETY: the allocation below can move `this` and the root list, and
    // both are STORED in the new view's fields. Re-read the whole enclosing
    // state through the pin afterwards rather than reusing the pre-allocation
    // copies — a from-space `parent` recorded here would be what every later
    // read of the nested view indexes into.
    let this_pin = ctx.pin_native_root(this);
    let view = match alloc_asl_view(ctx) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let view_pin = ctx.pin_native_root(view);
    let this = ctx.read_native_pin(this_pin, this);
    let (parent, offset, _, expected) = match asl_state(ctx, this) {
        Some(s) => s,
        None => {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(None)));
        }
    };
    let view = ctx.read_native_pin(view_pin, view);
    let base = asl_base(ctx, view);
    ctx.set_field(view, base + ASL_FIELD_PARENT, Value::Object(Some(parent)));
    ctx.set_field(view, base + ASL_FIELD_OFFSET, Value::Int(offset + from_i32));
    ctx.set_field(view, base + ASL_FIELD_SIZE, Value::Int(to_i32 - from_i32));
    ctx.set_field(view, base + ASL_FIELD_EXPECTED, Value::Int(expected));
    ctx.set_field(
        view,
        base + ASL_FIELD_VIEW_PARENT,
        Value::Object(Some(this)),
    );
    let view = ctx.read_native_pin(view_pin, view);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(view))))
}

// ---------------------------------------------------------------------------
// ArrayList.subList backed view (fix item 6).
//
// `java.util.ArrayList.subList(from, to)` returns a *view* whose reads and
// element writes go straight through to the parent list, and which fails fast
// (ConcurrentModificationException) if the parent is structurally modified
// through another path. The previous native returned an independent COPY, so
// `subList().set(...)` never reached the parent and parent edits were invisible
// to the sublist — a silent-divergence bug. This implements a genuine backed
// view sharing the parent's `elementData`.
//
// Scope note: structural mutation of the *sublist itself* (add/remove/clear)
// is modeled as a loud `UnsupportedOperationException` rather than the full
// JDK behavior of mutating the parent and rippling offsets to sibling views.
// That keeps the implementation bounded while honoring the project rule
// (no silent divergence): callers either read/`set` through correctly or get
// an explicit failure.
// ---------------------------------------------------------------------------

pub(crate) const ASL_CLASS: &str = "cratonvm/internal/ArrayListSubList";
pub(crate) const ASL_FIELD_PARENT: usize = 0;
pub(crate) const ASL_FIELD_OFFSET: usize = 1;
pub(crate) const ASL_FIELD_SIZE: usize = 2;
pub(crate) const ASL_FIELD_EXPECTED: usize = 3;
/// The view this view was carved out of — `null` when `subList` was called on a
/// real `ArrayList`, and the enclosing `ArrayListSubList` when it was called on
/// another view.
///
/// [`ASL_FIELD_PARENT`] deliberately stays the ROOT list with an ABSOLUTE
/// offset, so every existing `asl_*` native keeps working on a nested view
/// without being edited. This field exists for the one thing that cannot be
/// derived from the root: a structural mutation through a nested view has to
/// adjust the ENCLOSING views' `size`/`expected` too, or they immediately read
/// as comodified. The JDK does the same walk, up its own `SubList.parent`
/// chain, in `updateSizeAndModCount`.
///
/// It is an object field and therefore GC-scanned, unlike a side table — the
/// same reason `TmViewSpec`'s three references had to be wired into all four
/// overlay GC hooks and this one does not.
pub(crate) const ASL_FIELD_VIEW_PARENT: usize = 4;
pub(crate) const ASL_NUM_FIELDS: usize = 5;

/// The class HotSpot's `arrayList.subList(0, 2).getClass()` answers, and the
/// one row of `probes/ViewClassProbe` this VM still does not match.
///
/// **Not used as a carrier, and the reason is measured** — see
/// `collection-view-carrier-residuals-FIXED-20260813.md`,
/// "the one row that stayed open". The undeclared-slot rule does apply here:
/// the JDK class declares `root`/`parent`/`offset`/`size` on top of
/// `AbstractList.modCount`, this VM's five fields go PAST them ([`asl_base`]),
/// and `--real-jdk` measures byte-clean that way. What is NOT solved is
/// receiver OWNERSHIP. Registering the `native_asl_*` family on the real class
/// means it also fires for a `SubList` that **java.base's own bytecode** built —
/// which is precisely what `--jdk-only` produces, because there
/// `ArrayList.subList` runs its own body. Such an object has the class's
/// declared width and none of this VM's fields, so `asl_state` reads `modCount`
/// as `parent` and answers "empty": `sublist.size` 0, `sublist.of-sublist`
/// null, `Pattern.split` -> `null|null|null` — seven lines of
/// `probes/JdkOnlyCollectionViewProbe`, on an arm where the `--real-jdk` arm of
/// the same run was clean.
///
/// A retry needs a receiver test, not another carrier: `asl_base` must answer
/// `None` when `object_num_fields < class_num_total_fields + ASL_NUM_FIELDS`,
/// and every `native_asl_*` must then delegate through
/// `invoke_virtual_bytecode_only` instead of answering zero. Mirroring this
/// VM's state into the JDK's five fields was tried first and is not sufficient
/// alone: it makes the JDK's bodies correct on OUR objects and does nothing
/// about ours running on THEIRS.
pub(crate) const ASL_REAL_CLASS: &str = "java/util/ArrayList$SubList";

/// Where a sublist view's five native fields start: past the carrier's own
/// declared fields (5 on the real JDK class, 0 on [`ASL_CLASS`]).
///
/// Derived from the object's width for the same reason [`key_itr_base`] is —
/// both mint sites allocate exactly `declared + ASL_NUM_FIELDS`, and a view
/// built by some other path with the older FOUR-field shape stays at 0, which
/// is what keeps `asl_view_parent`'s width guard meaning what it meant.
#[inline]
pub(crate) fn asl_base(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    ctx.object_num_fields(this).saturating_sub(ASL_NUM_FIELDS)
}

/// [`asl_base`], answering `None` for a receiver this VM did not mint.
///
/// This is the receiver-ownership test the carrier needed. Wearing
/// [`ASL_REAL_CLASS`] means the `native_asl_*` family is also handed every
/// `SubList` **java.base's own bytecode** built — `ArrayList.subList` runs its
/// own body under `--jdk-only`, and can under `--real-jdk` too. Such an object
/// is exactly `class_num_total_fields` wide and carries none of this VM's five
/// fields, so [`asl_base`] would land inside the JDK's own
/// `root`/`parent`/`offset`/`size`, read `modCount` as `parent`, and every
/// native would answer empty. That is a SILENT wrong answer on the most
/// universally reached list path in the VM, which is why the previous attempt
/// at this carrier was withdrawn rather than shipped
/// (`collection-view-carrier-residuals-FIXED-20260813.md`,
/// "the one row that stayed open").
///
/// Every mint site allocates `class_num_total_fields + ASL_NUM_FIELDS` — on
/// BOTH carriers, which is what makes one comparison serve both and needs no
/// assumption about how wide `class_manager` fabricates [`ASL_CLASS`]. A
/// receiver narrower than that is not ours; [`asl_delegate_foreign`] then runs
/// the receiver's own bytecode.
#[inline]
pub(crate) fn asl_base_checked(ctx: &dyn NativeContext, this: ObjectRef) -> Option<usize> {
    let width = ctx.object_num_fields(this);
    let declared = ctx.class_num_total_fields(ctx.class_id_of_object(this));
    if width < declared + ASL_NUM_FIELDS {
        return None;
    }
    Some(width - ASL_NUM_FIELDS)
}

/// Run the receiver's OWN bytecode when it is a `SubList` this VM did not mint.
///
/// Returns `None` for a receiver that is ours, so the caller proceeds with the
/// native body. `invoke_virtual_bytecode_only` is what keeps this from
/// recursing straight back into the same registration.
pub(crate) fn asl_delegate_foreign(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
) -> Option<MethodCallResult> {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    if asl_base_checked(&*ctx, this).is_some() {
        return None;
    }
    Some(ctx.invoke_virtual_bytecode_only(this, method, descriptor, &args[1..]))
}

/// Allocate a sublist view under [`ASL_REAL_CLASS`], wide enough for its own
/// declared fields plus this VM's five, falling back to [`ASL_CLASS`].
///
/// **The §5 gate runs FIRST, and it is asked about the INTERNAL name.** That is
/// not bookkeeping. A sublist view is a compatibility stand-in whatever class it
/// wears — its state is at [`asl_base`], not in `root`/`parent`/`offset`/`size`
/// — so `--jdk-only` must go on refusing it, and refusing is what makes strict
/// mode fall back to `java.base`'s own `ArrayList.subList`, which builds a REAL
/// SubList that works. Skipping the gate because the object now has a real class
/// name was measured, on `probes/JdkOnlyCollectionViewProbe`:
///
/// ```text
/// sublist.mid    HotSpot [b|c]/2   --jdk-only []/0
/// Pattern.split  HotSpot 1|2|3     --jdk-only null|null|null
/// ```
///
/// — the JDK's own `SubList` bytecode reading fields this VM never fills. Seven
/// probe lines, from one skipped refusal.
///
/// `try_ensure_synthetic_class` rather than a throwaway `try_alloc_synthetic`:
/// same policy question, same answer, no object allocated to ask it.
pub(crate) fn alloc_asl_view(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    // Policy FIRST, and asked about the INTERNAL name — see this function's
    // doc comment. Wearing a real class must not smuggle a compatibility
    // stand-in past a mode that refuses them.
    let fallback = match ctx.try_ensure_synthetic_class(ASL_CLASS, ASL_NUM_FIELDS) {
        Ok(id) => id,
        Err(err) => return Err(cratonvm_native_api::refusal_to_java_failure(ctx, err)),
    };
    // `ensure_class_initialized` can report success having FABRICATED a
    // stand-in, so check the resolved name rather than trusting `Ok` — the same
    // check `alloc_key_set_view_object` makes for the same reason.
    if let Ok(cid) = ctx.ensure_class_initialized(ASL_REAL_CLASS) {
        if ctx.class_name_arc_of_id(cid).as_deref() == Some(ASL_REAL_CLASS) {
            let n = ctx.class_num_total_fields(cid) + ASL_NUM_FIELDS;
            return Ok(ctx.alloc_object(cid, n));
        }
    }
    // Same width rule on the fallback carrier, so `asl_base_checked` has one
    // comparison to make and not two.
    let n = ctx.class_num_total_fields(fallback) + ASL_NUM_FIELDS;
    Ok(ctx.alloc_object(fallback, n))
}

/// Read `(parent, offset, size, expected_parent_size)` from a sublist view.
pub(crate) fn asl_state(ctx: &dyn NativeContext, this: ObjectRef) -> Option<(ObjectRef, i32, i32, i32)> {
    let base = asl_base(ctx, this);
    let parent = match ctx.get_field(this, base + ASL_FIELD_PARENT) {
        Value::Object(Some(p)) => p,
        _ => return None,
    };
    let offset = match ctx.get_field(this, base + ASL_FIELD_OFFSET) {
        Value::Int(o) => o,
        _ => 0,
    };
    let size = match ctx.get_field(this, base + ASL_FIELD_SIZE) {
        Value::Int(s) => s,
        _ => 0,
    };
    let expected = match ctx.get_field(this, base + ASL_FIELD_EXPECTED) {
        Value::Int(e) => e,
        _ => 0,
    };
    Some((parent, offset, size, expected))
}

/// Fail fast if the parent list was structurally modified since the view was
/// created — mirrors the JDK's `checkForComodification`. Detected by comparing
/// the parent's current size against the size captured at `subList()` time.
pub(crate) fn asl_check_comod(
    ctx: &dyn NativeContext,
    parent: ObjectRef,
    expected: i32,
) -> Result<(), MethodCallFailed> {
    let (_, cur) = al_state(ctx, parent);
    if cur != expected {
        return Err(cratonvm_types::error::RuntimeError::ConcurrentModificationException.into());
    }
    Ok(())
}

pub(crate) fn register_al_sublist_natives(r: &mut NativeMethodRegistry) {
    // Both carriers. `ASL_REAL_CLASS` is safe to register now that
    // `asl_base_checked` can tell a view this VM minted from one java.base's
    // own bytecode built; without that test this registration is a silent
    // empty list on every foreign `SubList`, which is why it was withdrawn
    // once already.
    register_al_sublist_natives_on(r, ASL_CLASS);
    register_al_sublist_natives_on(r, ASL_REAL_CLASS);
    register_al_sublist_iterator_natives(r);
}

/// The whole `subList` surface, on one carrier class.
///
/// Split out from `register_al_sublist_natives` for a second carrier that was
/// then withdrawn: registering this family on the real
/// `java/util/ArrayList$SubList` also hands it every `SubList` java.base's own
/// bytecode builds, which have none of this VM's fields. [`ASL_REAL_CLASS`]
/// carries the measurement. The shape is left as a function because the retry
/// described there needs exactly it, plus a receiver test.
pub(crate) fn register_al_sublist_natives_on(r: &mut NativeMethodRegistry, c: &str) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "size", "()I", native_asl_size);
    r.register(c, "isEmpty", "()Z", native_asl_is_empty);
    r.register(c, "get", "(I)Ljava/lang/Object;", native_asl_get);
    r.register(
        c,
        "set",
        "(ILjava/lang/Object;)Ljava/lang/Object;",
        native_asl_set,
    );
    r.register(c, "iterator", "()Ljava/util/Iterator;", native_asl_iterator);
    r.register(c, "toArray", "()[Ljava/lang/Object;", native_asl_to_array);
    // `toArray(T[])` and `toArray(IntFunction)` are registered ONCE, further
    // down this same function under "REGRESSION FIX (hibernate-smoke)".
    //
    // A byte-identical second pair used to sit here: two independent regression
    // fixes — the JUnit Platform launcher and ~every ESTestCase via ES-FAIL-04
    // here, Hibernate's metadata collector there — each added the same two
    // registrations without noticing the other. Registration is
    // last-write-wins, so this pair could only ever LOSE the slot, and
    // `duplicate_registration_gate` counted both rows as shadowed. The callers
    // named by both fixes are served by the surviving pair, which has the same
    // body (`asl_delegate_snapshot` with the same descriptors), so deleting
    // this one changes no behaviour. Do not re-add a pair here.
    r.register(c, "toString", "()Ljava/lang/String;", native_asl_to_string);
    // Remaining read methods delegate to a fresh snapshot ArrayList. Without
    // these, an interface-level `Collection`/`List` native would be reached
    // with an ASL receiver and read it through `al_state` (wrong layout →
    // empty), silently reporting the sublist as empty. The snapshot is rebuilt
    // each call so it always reflects the live parent slice (after the
    // comod check).
    r.register(c, "contains", "(Ljava/lang/Object;)Z", |ctx, args| {
        asl_delegate_snapshot(ctx, args, "contains", "(Ljava/lang/Object;)Z")
    });
    r.register(c, "indexOf", "(Ljava/lang/Object;)I", |ctx, args| {
        asl_delegate_snapshot(ctx, args, "indexOf", "(Ljava/lang/Object;)I")
    });
    r.register(c, "lastIndexOf", "(Ljava/lang/Object;)I", |ctx, args| {
        asl_delegate_snapshot(ctx, args, "lastIndexOf", "(Ljava/lang/Object;)I")
    });
    r.register(c, "stream", "()Ljava/util/stream/Stream;", |ctx, args| {
        asl_delegate_snapshot(ctx, args, "stream", "()Ljava/util/stream/Stream;")
    });
    r.register(
        c,
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        |ctx, args| asl_delegate_snapshot(ctx, args, "forEach", "(Ljava/util/function/Consumer;)V"),
    );
    r.register(c, "hashCode", "()I", |ctx, args| {
        asl_delegate_snapshot(ctx, args, "hashCode", "()I")
    });
    r.register(c, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        asl_delegate_snapshot(ctx, args, "equals", "(Ljava/lang/Object;)Z")
    });
    // REGRESSION FIX (hibernate-smoke): the synthetic view only had the
    // no-arg `toArray()`; real code (e.g. Hibernate's metadata collector) calls
    // the `toArray(T[])` overload and other List read methods, which 404'd as a
    // NoSuchMethodError on this synthetic class. Delegate the remaining common
    // read methods to a fresh snapshot ArrayList (which carries the real JDK
    // implementations), matching the snapshot pattern above.
    r.register(
        c,
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
        |ctx, args| {
            asl_delegate_snapshot(
                ctx,
                args,
                "toArray",
                "([Ljava/lang/Object;)[Ljava/lang/Object;",
            )
        },
    );
    // `toArray(IntFunction)` sibling of the `toArray(T[])` overload above: the
    // JUnit Platform launcher calls `subList(..).toArray(X[]::new)`, which 404'd
    // as a NoSuchMethodError on the synthetic ASL class. A fresh snapshot
    // ArrayList carries both overloads via `native_collection_to_array_generator`.
    r.register(
        c,
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
        |ctx, args| {
            asl_delegate_snapshot(
                ctx,
                args,
                "toArray",
                "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
            )
        },
    );
    r.register(
        c,
        "containsAll",
        "(Ljava/util/Collection;)Z",
        |ctx, args| asl_delegate_snapshot(ctx, args, "containsAll", "(Ljava/util/Collection;)Z"),
    );
    // NOT `asl_delegate_snapshot`: it returned a real `ArrayList$ListItr` over
    // a snapshot `ArrayList`, whose `set`/`remove`/`add` mutated the COPY and
    // reported success — three silently-wrong rows of
    // `probes/SubListBehaviourProbe`. See the `SLI_CLASS` block.
    r.register(
        c,
        "listIterator",
        "()Ljava/util/ListIterator;",
        native_asl_list_iterator,
    );
    r.register(
        c,
        "listIterator",
        "(I)Ljava/util/ListIterator;",
        native_asl_list_iterator_idx,
    );
    // NOT `asl_delegate_snapshot`: a view of a snapshot reads correctly and
    // writes nowhere. See `native_asl_sub_list`.
    r.register(c, "subList", "(II)Ljava/util/List;", native_asl_sub_list);
    r.register(
        c,
        "spliterator",
        "()Ljava/util/Spliterator;",
        |ctx, args| asl_delegate_snapshot(ctx, args, "spliterator", "()Ljava/util/Spliterator;"),
    );
    // Structural mutators: delegate through a snapshot ArrayList and write the
    // result back into the parent slice (live-view contract — see
    // `asl_delegate_mutating`). `subList(a,b).clear()` and friends now mutate the
    // parent (Kafka `Acknowledgements.getAcknowledgementBatches` range-delete),
    // instead of the former fail-loud `UnsupportedOperationException`.
    r.register(c, "add", "(Ljava/lang/Object;)Z", |ctx, args| {
        asl_delegate_mutating(ctx, args, "add", "(Ljava/lang/Object;)Z")
    });
    r.register(c, "add", "(ILjava/lang/Object;)V", |ctx, args| {
        asl_delegate_mutating(ctx, args, "add", "(ILjava/lang/Object;)V")
    });
    r.register(c, "remove", "(I)Ljava/lang/Object;", |ctx, args| {
        asl_delegate_mutating(ctx, args, "remove", "(I)Ljava/lang/Object;")
    });
    r.register(c, "remove", "(Ljava/lang/Object;)Z", |ctx, args| {
        asl_delegate_mutating(ctx, args, "remove", "(Ljava/lang/Object;)Z")
    });
    r.register(c, "clear", "()V", |ctx, args| {
        asl_delegate_mutating(ctx, args, "clear", "()V")
    });
    r.register(c, "addAll", "(Ljava/util/Collection;)Z", |ctx, args| {
        asl_delegate_mutating(ctx, args, "addAll", "(Ljava/util/Collection;)Z")
    });
    r.register(
        c,
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
        |ctx, args| {
            asl_delegate_mutating(ctx, args, "removeIf", "(Ljava/util/function/Predicate;)Z")
        },
    );
    // `List.sort` is a MUTATOR of the view's range, and `subList.sortWritesThrough`
    // measured `base.subList(1,4).sort(naturalOrder())` leaving `base` at
    // `[9, 5, 1, 7, 3]` where HotSpot has `[9, 1, 5, 7, 3]`. It had no
    // registration here at all, so the call reached an interface-level native
    // that read the ASL receiver through the `ArrayList` layout — the sorted
    // result landed nowhere. `asl_delegate_mutating` is exactly right for it:
    // the snapshot's real `sort` reorders in place, the element count does not
    // change, and the slice is written back over `[offset, offset+size)`.
    r.register(c, "sort", "(Ljava/util/Comparator;)V", |ctx, args| {
        asl_delegate_mutating(ctx, args, "sort", "(Ljava/util/Comparator;)V")
    });
    // The rest of the `List` surface, on the REAL carrier only.
    //
    // WHY IT IS NEEDED. On the INTERNAL carrier an unregistered method fell to
    // an interface-level native and raised `AbstractMethodError:
    // java/util/List.removeAll has no Code attribute` — already a defect, and
    // `probes/SubListBehaviourProbe` is what found it. On [`ASL_REAL_CLASS`]
    // the same gap is worse: the receiver now HAS its own bytecode, so
    // `removeAll`/`retainAll`/`replaceAll` reach the JDK's real `SubList`
    // bodies and NPE on the null `root` this VM never fills. A carrier is only
    // as complete as the surface registered on it — the same lesson
    // `register_set_view_carrier_natives` records for the Set views.
    //
    // WHY ONLY THERE. The ambient `Bridge` set above is re-tagged
    // `SyntheticStub` for a `cratonvm/internal/*` receiver, so registering
    // these on both carriers adds twelve synthetic stubs and trips
    // `registry_census` — for rows nothing can reach. [`alloc_asl_view`] mints
    // under [`ASL_REAL_CLASS`] whenever a class of that name resolves, and
    // `class_manager` carries `java/util/ArrayList$SubList` in its preload list
    // with a declared width, so synthetic-JDK mode resolves it too. The
    // internal carrier is a fallback for "no class of that name anywhere" and
    // keeps the surface it has always had.
    if c == ASL_REAL_CLASS {
        r.register(c, "removeAll", "(Ljava/util/Collection;)Z", |ctx, args| {
            asl_delegate_mutating(ctx, args, "removeAll", "(Ljava/util/Collection;)Z")
        });
        r.register(c, "retainAll", "(Ljava/util/Collection;)Z", |ctx, args| {
            asl_delegate_mutating(ctx, args, "retainAll", "(Ljava/util/Collection;)Z")
        });
        r.register(
            c,
            "replaceAll",
            "(Ljava/util/function/UnaryOperator;)V",
            |ctx, args| {
                asl_delegate_mutating(
                    ctx,
                    args,
                    "replaceAll",
                    "(Ljava/util/function/UnaryOperator;)V",
                )
            },
        );
        r.register(c, "addAll", "(ILjava/util/Collection;)Z", |ctx, args| {
            asl_delegate_mutating(ctx, args, "addAll", "(ILjava/util/Collection;)Z")
        });
        r.register(
            c,
            "parallelStream",
            "()Ljava/util/stream/Stream;",
            |ctx, args| {
                asl_delegate_snapshot(ctx, args, "parallelStream", "()Ljava/util/stream/Stream;")
            },
        );
        // `SequencedCollection` (JDK 21). `List` declares all seven, so a receiver
        // with real bytecode answers them from `root` — null here.
        r.register(c, "getFirst", "()Ljava/lang/Object;", |ctx, args| {
            asl_delegate_snapshot(ctx, args, "getFirst", "()Ljava/lang/Object;")
        });
        r.register(c, "getLast", "()Ljava/lang/Object;", |ctx, args| {
            asl_delegate_snapshot(ctx, args, "getLast", "()Ljava/lang/Object;")
        });
        r.register(c, "addFirst", "(Ljava/lang/Object;)V", |ctx, args| {
            asl_delegate_mutating(ctx, args, "addFirst", "(Ljava/lang/Object;)V")
        });
        r.register(c, "addLast", "(Ljava/lang/Object;)V", |ctx, args| {
            asl_delegate_mutating(ctx, args, "addLast", "(Ljava/lang/Object;)V")
        });
        r.register(c, "removeFirst", "()Ljava/lang/Object;", |ctx, args| {
            asl_delegate_mutating(ctx, args, "removeFirst", "()Ljava/lang/Object;")
        });
        r.register(c, "removeLast", "()Ljava/lang/Object;", |ctx, args| {
            asl_delegate_mutating(ctx, args, "removeLast", "()Ljava/lang/Object;")
        });
        // `reversed()` is a live VIEW in the JDK, and a view of a snapshot writes
        // nowhere — the same reason `subList` is NOT snapshot-delegated (see
        // `native_asl_sub_list`). Registered anyway because the alternative is the
        // JDK body on a null `root`: reads are correct, writes through the reversed
        // view are lost, and that is strictly better than an NPE. Covered by
        // `probes/SubListBehaviourProbe`'s `read.reversed` row.
        r.register(c, "reversed", "()Ljava/util/List;", |ctx, args| {
            asl_delegate_snapshot(ctx, args, "reversed", "()Ljava/util/List;")
        });
    }
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// The sublist view's LIVE ListIterator
// ---------------------------------------------------------------------------
//
// WHAT WAS WRONG. `iterator()` snapshotted the slice into an `Object[]` and
// handed back a real `java.util.Arrays$ArrayItr` over it; `listIterator()` went
// through `asl_delegate_snapshot`, so it returned a real
// `java.util.ArrayList$ListItr` over a snapshot `ArrayList`. Both were
// iterators over a COPY, and `List.subList` is specified as a LIVE VIEW, so
// seven rows of `probes/SubListBehaviourProbe` were wrong at once:
//
//   iter.remove / removeSecond / removeAllViaIterator
//       `Arrays$ArrayItr` declares no `remove()` at all, so the call reached
//       `Iterator.remove()`'s throwing default: UnsupportedOperationException
//       where HotSpot mutates.
//   iter.removeBeforeNext
//       the same UOE where HotSpot raises IllegalStateException.
//   iter.listIterator.set / .remove / .add
//       `ArrayList$ListItr` HAS working mutators — and they mutated the copy.
//       Those three reported SUCCESS and changed nothing, which is the
//       dangerous half: a silent wrong answer on a live-view contract, where
//       the `Iterator.remove` rows at least threw.
//
// WHY A CARRIER AND NOT A ROUTE. A write-through `remove()` can be bolted onto
// the snapshot iterator — `snapshot_itr_backing_table` already does exactly
// that for `HashSet`/`TreeSet`/`LinkedList`/`ArrayDeque` — but `set` and `add`
// cannot: both need the POSITION the iterator is at, in the VIEW's
// coordinates, and a snapshot iterator's cursor drifts from the view's indices
// the moment anything is removed. So this is a real iterator over
// `(view, cursor, lastRet)`, which is what the JDK's own
// `ArrayList$SubList$1` is, and every method below is that class's body
// rewritten against this VM's view state.
//
// WHY THE JDK'S OWN CLASS IS THE CARRIER. `SubList.iterator()` on HotSpot IS
// `listIterator()`, so one live iterator serves both entry points and
// `subList(1,4).iterator().getClass()` stops answering
// `java.util.Arrays$ArrayItr`. The cost is the receiver-ownership trap the
// `ASL_REAL_CLASS` carrier hit one level up: wearing a real class means these
// natives are also handed every `ArrayList$SubList$1` **java.base's own
// bytecode** built, which is what `--jdk-only` produces (there
// `ArrayList.subList` is refused, java.base runs its own body, and its
// iterator is a real one with real state). `sli_base_checked` is the same
// width test `asl_base_checked` is, for the same reason, and
// `sli_delegate_foreign` runs the receiver's own bytecode when the answer is
// "not ours". `probes/SubListBehaviourProbe` on BOTH arms is what proves it.
//
// A build with no `java.util.ArrayList$SubList$1` to resolve keeps the
// snapshot iterators it has always had — `alloc_sli_view` answers `None` and
// both entry points fall back. That is deliberate and not a second carrier:
// registering this surface on a `cratonvm/internal/*` name would add ten
// synthetic stubs and trip `registry_census`, for rows only a synthetic-JDK build
// could reach, which is the same trade `register_al_sublist_natives` records
// for the twelve `List` methods it puts on the real carrier only.

/// The JDK's own anonymous `ListIterator` inside `ArrayList$SubList`, and the
/// class `subList(a, b).iterator().getClass()` answers on HotSpot.
pub(crate) const SLI_CLASS: &str = "java/util/ArrayList$SubList$1";

/// The sublist view this iterator walks.
pub(crate) const SLI_FIELD_VIEW: usize = 0;
/// `nextIndex()`, in the VIEW's coordinates.
pub(crate) const SLI_FIELD_CURSOR: usize = 1;
/// The index `next()`/`previous()` last returned, or `-1` — the JDK's `lastRet`,
/// and the flag `remove()`/`set()` raise `IllegalStateException` on.
pub(crate) const SLI_FIELD_LASTRET: usize = 2;
pub(crate) const SLI_NUM_FIELDS: usize = 3;

/// Where this iterator's three native fields start: past the carrier's own
/// declared fields. Exactly [`asl_base`]'s rule, one level down.
#[inline]
pub(crate) fn sli_base(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    ctx.object_num_fields(this).saturating_sub(SLI_NUM_FIELDS)
}

/// [`sli_base`], answering `None` for an iterator this VM did not mint.
///
/// The receiver-ownership test. A real `ArrayList$SubList$1` that java.base
/// built is exactly `class_num_total_fields` wide (`cursor`, `lastRet`,
/// `expectedModCount`, `val$index`, `this$0`) and carries none of this VM's
/// three; without this, every native below would read `expectedModCount` as
/// the view and answer for an object it knows nothing about — the silent
/// wrong answer that withdrew the first attempt at the `ArrayList$SubList`
/// carrier.
#[inline]
pub(crate) fn sli_base_checked(ctx: &dyn NativeContext, this: ObjectRef) -> Option<usize> {
    let width = ctx.object_num_fields(this);
    let declared = ctx.class_num_total_fields(ctx.class_id_of_object(this));
    if width < declared + SLI_NUM_FIELDS {
        return None;
    }
    Some(width - SLI_NUM_FIELDS)
}

/// Run the receiver's OWN bytecode when it is a `SubList` iterator this VM did
/// not mint. `None` means the receiver is ours and the caller proceeds.
pub(crate) fn sli_delegate_foreign(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
) -> Option<MethodCallResult> {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    if sli_base_checked(&*ctx, this).is_some() {
        return None;
    }
    Some(ctx.invoke_virtual_bytecode_only(this, method, descriptor, &args[1..]))
}

/// Whether `this` is a live sublist iterator this VM minted.
///
/// Used by the interface-level `hasNext`/`next` natives to route deliberately
/// rather than by accident: their generic fallback would reach these bodies
/// anyway, through a by-name re-dispatch guarded against shadow recursion, and
/// an explicit arm is cheaper to read and impossible to lose to a change in
/// that fallback. Costs nothing on the fast path — it is only consulted after
/// the "slot 0 holds an `Object[]`" probe has already missed.
pub(crate) fn is_live_sublist_itr(ctx: &dyn NativeContext, this: ObjectRef, class_name: &str) -> bool {
    class_name == SLI_CLASS && sli_base_checked(ctx, this).is_some()
}

/// Allocate a live sublist iterator over `view`, positioned at `index`.
///
/// `None` when this image has no real `java.util.ArrayList$SubList$1` — the
/// caller then keeps the snapshot iterator it has always returned. The resolved
/// NAME is checked, not just `Ok`, for the same reason `alloc_asl_view` checks
/// it: `ensure_class_initialized` can report success having fabricated a
/// stand-in, and a fabricated carrier is exactly what must not be minted here.
pub(crate) fn alloc_sli_view(ctx: &mut dyn NativeContext, view: ObjectRef, index: i32) -> Option<ObjectRef> {
    let cid = ctx.ensure_class_initialized(SLI_CLASS).ok()?;
    if ctx.class_name_arc_of_id(cid).as_deref() != Some(SLI_CLASS) {
        return None;
    }
    let n = ctx.class_num_total_fields(cid) + SLI_NUM_FIELDS;
    // GC-SAFETY: `alloc_object` collects, and `view` is a bare Rust local the
    // collector cannot see. Root it across the allocation and read both halves
    // back through their pins before the stores — the contract
    // `alloc_real_snapshot_iterator_of` documents, and what `gc_native_pins`
    // covers.
    let view_pin = ctx.pin_native_root(view);
    let itr = ctx.alloc_object(cid, n);
    let itr_pin = ctx.pin_native_root(itr);
    let itr = ctx.read_native_pin(itr_pin, itr);
    let view = ctx.read_native_pin(view_pin, view);
    let base = n - SLI_NUM_FIELDS;
    ctx.set_field(itr, base + SLI_FIELD_VIEW, Value::Object(Some(view)));
    let itr = ctx.read_native_pin(itr_pin, itr);
    ctx.set_field(itr, base + SLI_FIELD_CURSOR, Value::Int(index));
    let itr = ctx.read_native_pin(itr_pin, itr);
    // Explicit, not left to whatever `alloc_object` zero-initialises an
    // undeclared slot to: `lastRet` MUST start at -1, and W7-1's `lastRet` was
    // exactly this hazard — an int-zero read as a meaningful value, which here
    // would make `remove()` before any `next()` delete element 0 instead of
    // raising `IllegalStateException`.
    ctx.set_field(itr, base + SLI_FIELD_LASTRET, Value::Int(-1));
    let itr = ctx.read_native_pin(itr_pin, itr);
    ctx.unpin_native_roots(view_pin);
    Some(itr)
}

/// Read `(view, cursor, lastRet)`.
pub(crate) fn sli_state(ctx: &dyn NativeContext, this: ObjectRef) -> Option<(ObjectRef, i32, i32)> {
    let base = sli_base_checked(ctx, this)?;
    let view = match ctx.get_field(this, base + SLI_FIELD_VIEW) {
        Value::Object(Some(v)) => v,
        _ => return None,
    };
    let cursor = match ctx.get_field(this, base + SLI_FIELD_CURSOR) {
        Value::Int(c) => c,
        _ => 0,
    };
    let last_ret = match ctx.get_field(this, base + SLI_FIELD_LASTRET) {
        Value::Int(l) => l,
        _ => -1,
    };
    Some((view, cursor, last_ret))
}

pub(crate) fn sli_set_cursor(ctx: &mut dyn NativeContext, this: ObjectRef, cursor: i32, last_ret: i32) {
    let base = sli_base(ctx, this);
    ctx.set_field(this, base + SLI_FIELD_CURSOR, Value::Int(cursor));
    ctx.set_field(this, base + SLI_FIELD_LASTRET, Value::Int(last_ret));
}

/// The view's current `size()`, which also runs the comodification check —
/// so a parent mutated behind the view's back fails here exactly as it does
/// through any other accessor.
pub(crate) fn sli_view_size(ctx: &mut dyn NativeContext, view: ObjectRef) -> Result<i32, MethodCallFailed> {
    match native_asl_size(ctx, &[Value::Object(Some(view))])? {
        Some(Value::Int(n)) => Ok(n),
        _ => Ok(0),
    }
}

/// `new IllegalStateException()` — with NO message, which is what
/// `ArrayList$SubList$1.remove()`/`set()` throw.
///
/// The message is load-bearing here in the negative: `RuntimeError::IllegalStateException`
/// always carries one (`Some(message.as_str())`), so routing through it prints
/// `java.lang.IllegalStateException: remove` where HotSpot prints
/// `java.lang.IllegalStateException` — `Throwable.toString()` omits the suffix
/// only for a NULL message, and `iter.removeBeforeNext` measures exactly that
/// line. `Iterator.remove()`'s throwing DEFAULT does carry `"remove"`, which is
/// why the snapshot iterator's version keeps it; this is the other exception,
/// from the other class, and it is bare.
/// `new NoSuchElementException()` — with NO message, which is what
/// `LinkedList$ListItr.next()`/`previous()` throw.
///
/// `RuntimeError::NoSuchElementException` always supplies a message and
/// `Throwable.toString()` prints the suffix whenever there is one, so a
/// "helpful" `"LinkedList$ListItr.next"` is a visible difference from HotSpot
/// rather than a bonus. Exactly the trap [`sli_illegal_state`] exists for, on
/// the other exception — this is the third time it has been paid in this file,
/// which is why it is now a named helper instead of an inline literal.
///
/// NOT for the interface-level snapshot iterators: `ArrayList$Itr.next()`'s
/// JDK body also throws bare, but several natives here deliberately carry a
/// message that a caller matches on. Change those only with a probe row.
pub(crate) fn bare_no_such_element(ctx: &mut dyn NativeContext) -> MethodCallFailed {
    if let Ok(Some(Value::Object(Some(exc)))) =
        ctx.new_object_initialized("java/util/NoSuchElementException", "()V", &[])
    {
        return MethodCallFailed::ExceptionThrown(exc);
    }
    cratonvm_types::error::RuntimeError::NoSuchElementException {
        message: String::new(),
    }
    .into()
}

pub(crate) fn sli_illegal_state(ctx: &mut dyn NativeContext) -> MethodCallFailed {
    if let Ok(Some(Value::Object(Some(exc)))) =
        ctx.new_object_initialized("java/lang/IllegalStateException", "()V", &[])
    {
        return MethodCallFailed::ExceptionThrown(exc);
    }
    // An image with no `java.lang.IllegalStateException` to build cannot
    // happen, but a wrong message beats no exception.
    cratonvm_types::error::RuntimeError::IllegalStateException {
        message: "remove".to_string(),
    }
    .into()
}

pub(crate) fn native_sli_has_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = sli_delegate_foreign(ctx, args, "hasNext", "()Z") {
        return r;
    }
    let Some((view, cursor, _)) = args
        .first()
        .and_then(|v| match v {
            Value::Object(Some(o)) => Some(*o),
            _ => None,
        })
        .and_then(|this| sli_state(&*ctx, this))
    else {
        return Ok(Some(Value::Int(0)));
    };
    let size = sli_view_size(ctx, view)?;
    Ok(Some(Value::Int(i32::from(cursor != size))))
}

pub(crate) fn native_sli_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = sli_delegate_foreign(ctx, args, "next", "()Ljava/lang/Object;") {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let Some((view, cursor, _)) = sli_state(&*ctx, this) else {
        return Ok(Some(Value::Object(None)));
    };
    let size = sli_view_size(ctx, view)?;
    if cursor >= size {
        return Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: String::new(),
            }
            .into(),
        );
    }
    let v = native_asl_get(ctx, &[Value::Object(Some(view)), Value::Int(cursor)])?
        .unwrap_or(Value::Object(None));
    sli_set_cursor(ctx, this, cursor + 1, cursor);
    Ok(Some(v))
}

pub(crate) fn native_sli_has_previous(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = sli_delegate_foreign(ctx, args, "hasPrevious", "()Z") {
        return r;
    }
    let Some((_, cursor, _)) = args
        .first()
        .and_then(|v| match v {
            Value::Object(Some(o)) => Some(*o),
            _ => None,
        })
        .and_then(|this| sli_state(&*ctx, this))
    else {
        return Ok(Some(Value::Int(0)));
    };
    Ok(Some(Value::Int(i32::from(cursor != 0))))
}

pub(crate) fn native_sli_previous(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = sli_delegate_foreign(ctx, args, "previous", "()Ljava/lang/Object;") {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let Some((view, cursor, _)) = sli_state(&*ctx, this) else {
        return Ok(Some(Value::Object(None)));
    };
    if cursor <= 0 {
        return Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: String::new(),
            }
            .into(),
        );
    }
    let i = cursor - 1;
    let v = native_asl_get(ctx, &[Value::Object(Some(view)), Value::Int(i)])?
        .unwrap_or(Value::Object(None));
    sli_set_cursor(ctx, this, i, i);
    Ok(Some(v))
}

pub(crate) fn native_sli_next_index(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = sli_delegate_foreign(ctx, args, "nextIndex", "()I") {
        return r;
    }
    let cursor = args
        .first()
        .and_then(|v| match v {
            Value::Object(Some(o)) => Some(*o),
            _ => None,
        })
        .and_then(|this| sli_state(&*ctx, this))
        .map(|(_, c, _)| c)
        .unwrap_or(0);
    Ok(Some(Value::Int(cursor)))
}

pub(crate) fn native_sli_previous_index(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = sli_delegate_foreign(ctx, args, "previousIndex", "()I") {
        return r;
    }
    let cursor = args
        .first()
        .and_then(|v| match v {
            Value::Object(Some(o)) => Some(*o),
            _ => None,
        })
        .and_then(|this| sli_state(&*ctx, this))
        .map(|(_, c, _)| c)
        .unwrap_or(0);
    Ok(Some(Value::Int(cursor - 1)))
}

/// `remove()` — delete the element `next()`/`previous()` last returned, THROUGH
/// the view, so the parent list sees it.
///
/// Removal is BY INDEX, not by element: a list is positional and may hold
/// duplicates, and `lastRet` already says which slot. `asl_delegate_mutating`
/// is the view's own `remove(int)`, so the parent's backing array is rebuilt
/// and every enclosing view's `size`/`expected` is resynced — which is what
/// keeps the very next read of the view from raising
/// `ConcurrentModificationException`.
pub(crate) fn native_sli_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = sli_delegate_foreign(ctx, args, "remove", "()V") {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let Some((view, _cursor, last_ret)) = sli_state(&*ctx, this) else {
        return Ok(None);
    };
    if last_ret < 0 {
        return Err(sli_illegal_state(ctx));
    }
    // GC-SAFETY: the delegate allocates (a snapshot list, a replacement backing
    // array) and can dispatch Java, so the iterator can move under us. Root it
    // and read it back before the bookkeeping write.
    let this_pin = ctx.pin_native_root(this);
    let r = asl_delegate_mutating(
        ctx,
        &[Value::Object(Some(view)), Value::Int(last_ret)],
        "remove",
        "(I)Ljava/lang/Object;",
    );
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    r?;
    // The JDK's `cursor = lastRet; lastRet = -1`: the elements after the hole
    // shift down by one, so the next `next()` must re-read the slot just
    // vacated. `iter.removeAllViaIterator` is the row that catches getting
    // this wrong — it removes index 0 three times in a row.
    sli_set_cursor(ctx, this, last_ret, -1);
    Ok(None)
}

/// `set(E)` — overwrite the element last returned, through the view.
pub(crate) fn native_sli_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = sli_delegate_foreign(ctx, args, "set", "(Ljava/lang/Object;)V") {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let Some((view, _cursor, last_ret)) = sli_state(&*ctx, this) else {
        return Ok(None);
    };
    if last_ret < 0 {
        return Err(sli_illegal_state(ctx));
    }
    let value = args.get(1).copied().unwrap_or(Value::Object(None));
    native_asl_set(
        ctx,
        &[Value::Object(Some(view)), Value::Int(last_ret), value],
    )?;
    // `set` does not disturb the cursor and does not clear `lastRet` — the JDK
    // allows `set` twice in a row, and only `remove`/`add` reset it.
    Ok(None)
}

/// `add(E)` — insert at the cursor, through the view.
pub(crate) fn native_sli_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = sli_delegate_foreign(ctx, args, "add", "(Ljava/lang/Object;)V") {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let Some((view, cursor, _)) = sli_state(&*ctx, this) else {
        return Ok(None);
    };
    let value = args.get(1).copied().unwrap_or(Value::Object(None));
    let this_pin = ctx.pin_native_root(this);
    let r = asl_delegate_mutating(
        ctx,
        &[Value::Object(Some(view)), Value::Int(cursor), value],
        "add",
        "(ILjava/lang/Object;)V",
    );
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    r?;
    // The JDK's `cursor++; lastRet = -1`: the new element goes BEFORE the one
    // `next()` would return, so the cursor moves past it, and `add` invalidates
    // `remove`/`set` exactly as it does on `ArrayList$ListItr`.
    sli_set_cursor(ctx, this, cursor + 1, -1);
    Ok(None)
}

/// `forEachRemaining(Consumer)`.
///
/// Registered because the carrier DECLARES it: leaving it to the real body
/// would run JDK bytecode over `this$0`/`expectedModCount` this VM never fills
/// — an NPE on a receiver every other method serves correctly, which is the
/// "a carrier is only as complete as the surface registered on it" rule
/// `register_al_sublist_natives` records.
pub(crate) fn native_sli_for_each_remaining(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = sli_delegate_foreign(
        ctx,
        args,
        "forEachRemaining",
        "(Ljava/util/function/Consumer;)V",
    ) {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => return Ok(None),
    };
    // Bounded by the size read at entry, re-read each round: the consumer can
    // legally mutate nothing here, but a shrinking view must end the loop
    // rather than index past it.
    let consumer_pin = ctx.pin_native_root(consumer);
    loop {
        let consumer = ctx.read_native_pin(consumer_pin, consumer);
        let Some((view, cursor, _)) = sli_state(&*ctx, this) else {
            return Ok(None);
        };
        let size = sli_view_size(ctx, view)?;
        if cursor >= size {
            return Ok(None);
        }
        let v = native_asl_get(ctx, &[Value::Object(Some(view)), Value::Int(cursor)])?
            .unwrap_or(Value::Object(None));
        sli_set_cursor(ctx, this, cursor + 1, cursor);
        let this_pin = ctx.pin_native_root(this);
        let r = ctx.invoke_virtual(consumer, "accept", "(Ljava/lang/Object;)V", &[v]);
        let this = ctx.read_native_pin(this_pin, this);
        ctx.unpin_native_roots(this_pin);
        r?;
        let _ = this;
    }
}

/// The ten `ListIterator` methods `ArrayList$SubList$1` declares.
///
/// On the REAL carrier only, and gated by `sli_delegate_foreign` on every one:
/// the class also names every iterator java.base's own `SubList` bytecode
/// builds, and those must keep running their own bodies.
pub(crate) fn register_al_sublist_iterator_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = SLI_CLASS;
    r.register(c, "hasNext", "()Z", native_sli_has_next);
    r.register(c, "next", "()Ljava/lang/Object;", native_sli_next);
    r.register(c, "hasPrevious", "()Z", native_sli_has_previous);
    r.register(c, "previous", "()Ljava/lang/Object;", native_sli_previous);
    r.register(c, "nextIndex", "()I", native_sli_next_index);
    r.register(c, "previousIndex", "()I", native_sli_previous_index);
    r.register(c, "remove", "()V", native_sli_remove);
    r.register(c, "set", "(Ljava/lang/Object;)V", native_sli_set);
    r.register(c, "add", "(Ljava/lang/Object;)V", native_sli_add);
    r.register(
        c,
        "forEachRemaining",
        "(Ljava/util/function/Consumer;)V",
        native_sli_for_each_remaining,
    );
    r.set_category(__prev_cat);
}

/// `listIterator()` / `listIterator(int)` / `iterator()` for a sublist view.
///
/// One body for all three, because on HotSpot they ARE one body:
/// `ArrayList$SubList.iterator()` is `listIterator()`.
pub(crate) fn asl_live_list_iterator(
    ctx: &mut dyn NativeContext,
    view: ObjectRef,
    index: i32,
) -> Option<MethodCallResult> {
    // Fail the same way every other view accessor does if the parent moved
    // under us, BEFORE minting an iterator over a stale slice.
    let size = match asl_state(&*ctx, view) {
        Some((parent, _, size, expected)) => match asl_check_comod(&*ctx, parent, expected) {
            Ok(()) => size,
            Err(e) => return Some(Err(e)),
        },
        None => return None,
    };
    if index < 0 || index > size {
        return Some(Err(cratonvm_types::error::RuntimeError::ioobe(format!(
            "Index: {index}"
        ))
        .into()));
    }
    alloc_sli_view(ctx, view, index).map(|itr| Ok(Some(Value::Object(Some(itr)))))
}

pub(crate) fn native_asl_list_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = asl_delegate_foreign(ctx, args, "listIterator", "()Ljava/util/ListIterator;") {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    if let Some(r) = asl_live_list_iterator(ctx, this, 0) {
        return r;
    }
    // No real `ArrayList$SubList$1` in this image — keep the snapshot
    // ListIterator this method has always returned.
    asl_delegate_snapshot(ctx, args, "listIterator", "()Ljava/util/ListIterator;")
}

pub(crate) fn native_asl_list_iterator_idx(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = asl_delegate_foreign(ctx, args, "listIterator", "(I)Ljava/util/ListIterator;")
    {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    if let Some(r) = asl_live_list_iterator(ctx, this, index) {
        return r;
    }
    asl_delegate_snapshot(ctx, args, "listIterator", "(I)Ljava/util/ListIterator;")
}

pub(crate) fn native_asl_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = asl_delegate_foreign(ctx, args, "size", "()I") {
        return r;
    }

    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    match asl_state(ctx, this) {
        Some((parent, _, size, expected)) => {
            asl_check_comod(ctx, parent, expected)?;
            Ok(Some(Value::Int(size)))
        }
        None => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_asl_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = asl_delegate_foreign(ctx, args, "isEmpty", "()Z") {
        return r;
    }

    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(1))),
    };
    match asl_state(ctx, this) {
        Some((parent, _, size, expected)) => {
            asl_check_comod(ctx, parent, expected)?;
            Ok(Some(Value::Int(i32::from(size == 0))))
        }
        None => Ok(Some(Value::Int(1))),
    }
}

pub(crate) fn native_asl_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = asl_delegate_foreign(ctx, args, "get", "(I)Ljava/lang/Object;") {
        return r;
    }

    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (parent, offset, size, expected) = match asl_state(ctx, this) {
        Some(s) => s,
        None => return Ok(Some(Value::Object(None))),
    };
    asl_check_comod(ctx, parent, expected)?;
    if index < 0 || index >= size {
        // HotSpot: IndexOutOfBoundsException, "Index 1 out of bounds for
        // length 1" -- a subList reports the SUBLIST's length, not the
        // parent's. Not the Array... subclass; see native_al_get.
        return Err(cratonvm_types::error::RuntimeError::ioobe(format!(
            "Index {index} out of bounds for length {size}"
        ))
        .into());
    }
    // Write-through READ: index straight into the parent's live backing array.
    let (data, _) = al_state(ctx, parent);
    match data {
        Some(d) => Ok(Some(ctx.get_array_element(d, (offset + index) as usize))),
        None => Ok(Some(Value::Object(None))),
    }
}

pub(crate) fn native_asl_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) =
        asl_delegate_foreign(ctx, args, "set", "(ILjava/lang/Object;)Ljava/lang/Object;")
    {
        return r;
    }

    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => return Ok(Some(Value::Object(None))),
    };
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    let (parent, offset, size, expected) = match asl_state(ctx, this) {
        Some(s) => s,
        None => return Ok(Some(Value::Object(None))),
    };
    asl_check_comod(ctx, parent, expected)?;
    if index < 0 || index >= size {
        // HotSpot: IndexOutOfBoundsException, "Index 1 out of bounds for
        // length 1" -- a subList reports the SUBLIST's length, not the
        // parent's. Not the Array... subclass; see native_al_get.
        return Err(cratonvm_types::error::RuntimeError::ioobe(format!(
            "Index {index} out of bounds for length {size}"
        ))
        .into());
    }
    // Write-through SET: the element write lands in the PARENT's backing array.
    let (data, _) = al_state(ctx, parent);
    match data {
        Some(d) => {
            let old = ctx.get_array_element(d, (offset + index) as usize);
            ctx.set_array_element(d, (offset + index) as usize, value);
            Ok(Some(old))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

/// Snapshot the current slice (offset..offset+size) of the parent into a fresh
/// `Object[]`, used by both `toArray()` and `iterator()`.
pub(crate) fn asl_snapshot(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let (parent, offset, size, expected) = match asl_state(ctx, this) {
        Some(s) => s,
        None => return Ok(alloc_ref_array(ctx, 0)),
    };
    asl_check_comod(ctx, parent, expected)?;
    // gc-common w20-a: the allocation can move the view and its parent, and
    // `parent` was read before it; the copy read the parent's backing array
    // through that stale address. Root the view and re-read the parent.
    let this_pin = ctx.pin_native_root(this);
    let buf: ObjectRef = alloc_ref_array(ctx, size as usize);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    let parent = asl_state(ctx, this).map_or(parent, |(p, _, _, _)| p);
    let (data, _) = al_state(ctx, parent);
    if let Some(d) = data {
        for i in 0..(size as usize) {
            let v = ctx.get_array_element(d, offset as usize + i);
            ctx.set_array_element(buf, i, v);
        }
    }
    Ok(buf)
}

pub(crate) fn native_asl_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = asl_delegate_foreign(ctx, args, "toArray", "()[Ljava/lang/Object;") {
        return r;
    }

    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let buf = asl_snapshot(ctx, this)?;
    Ok(Some(Value::Object(Some(buf))))
}

pub(crate) fn native_asl_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = asl_delegate_foreign(ctx, args, "toString", "()Ljava/lang/String;") {
        return r;
    }

    use std::fmt::Write as _;
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Build from the live slice snapshot (the ASL layout differs from
    // ArrayList's, so the ArrayList toString cannot be reused directly).
    let buf = asl_snapshot(ctx, this)?;
    let len = ctx.array_length(buf);
    let mut text = Units::with_capacity(2 + len.saturating_mul(18));
    text.push(u16::from(b'['));
    // gc-common w20-a: each element's `toString()` runs Java and can move the
    // snapshot; it was indexed through its first address for every element.
    push_array_display_units(ctx, &mut text, buf, len)?;
    text.push(u16::from(b']'));
    let s = ctx.create_string_from_units(&text);
    Ok(Some(Value::Object(Some(s))))
}

pub(crate) fn native_asl_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = asl_delegate_foreign(ctx, args, "iterator", "()Ljava/util/Iterator;") {
        return r;
    }

    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `SubList.iterator()` IS `listIterator()` on HotSpot, and it has to be
    // here too: a snapshot iterator cannot write through, which is what made
    // `iter.remove` answer `UnsupportedOperationException` where HotSpot
    // mutates the parent. See the `SLI_CLASS` block above.
    if let Some(r) = asl_live_list_iterator(ctx, this, 0) {
        return r;
    }
    // No real `ArrayList$SubList$1` in this image — keep the snapshot iterator
    // this method has always returned.
    let buf = asl_snapshot(ctx, this)?;
    let len = ctx.array_length(buf);
    // `make_iterator_from_array` already returns a `MethodCallResult`
    // (`Ok(Some(Value::Object(Some(iterator))))`); forward it directly.
    make_iterator_from_array(ctx, buf, len)
}

/// Delegate a read method to a freshly materialized snapshot `ArrayList` of
/// the view's current slice. Keeps the less-common read methods correct
/// without re-implementing each one against the offset/parent layout, while
/// still failing fast on parent comodification (via `asl_snapshot`).
pub(crate) fn asl_delegate_snapshot(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
) -> MethodCallResult {
    if let Some(r) = asl_delegate_foreign(ctx, args, method, descriptor) {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let buf = asl_snapshot(ctx, this)?;
    let size = ctx.array_length(buf) as i32;
    let snap = alloc_arraylist_with(ctx, buf, size)?;
    ctx.invoke_virtual(snap, method, descriptor, &args[1..])
}

/// Delegate a STRUCTURAL mutator (`add`/`remove`/`clear`/`addAll`/`removeIf`)
/// through a snapshot `ArrayList` of the view's slice, then write the mutated
/// slice back into the parent — honouring the `List.subList` live-view contract
/// (mutations through the sublist write through to the parent). Reuses the real
/// ArrayList mutator semantics + return value on the snapshot, then replaces the
/// parent's `[offset, offset+size)` slice with the snapshot's new contents and
/// resyncs the view's `size`/`expected`. Mirrors `asl_delegate_snapshot` (the
/// read-side pattern); supersedes the former fail-loud `native_asl_unsupported`.
///
/// GC-safety: the view (`this`) and the snapshot are pinned across the
/// (possibly allocating) mutator invoke, and the view is read back through its
/// pin — and the parent re-read through it — after each allocation (the
/// argument slot is rooted, but this function's copy of it is not remapped);
/// the new backing array is allocated once and filled with no intervening
/// allocation.
pub(crate) fn asl_delegate_mutating(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
) -> MethodCallResult {
    if let Some(r) = asl_delegate_foreign(ctx, args, method, descriptor) {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (parent, _offset, _size, expected) = match asl_state(ctx, this) {
        Some(s) => s,
        None => return Ok(Some(Value::Object(None))),
    };
    asl_check_comod(ctx, parent, expected)?;

    // Materialize the current slice as a real ArrayList and run the mutator on
    // it (reuses the real-JDK semantics + return value). Pin the snapshot across
    // the invoke, which may allocate/GC.
    // gc-common w20-a: `this` is a bare copy of the argument, and the
    // snapshot's allocations, the mutator (Java) and the replacement array's
    // allocation all sit between here and its last use. It used to be read
    // unpinned throughout — the "re-read" of the parent below went through the
    // view's pre-move address, and the view's size/expected were stored there.
    // Pinned first, so it is the base every exit releases.
    let this_pin = ctx.pin_native_root(this);
    let buf = match asl_snapshot(ctx, this) {
        Ok(b) => b,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let size0 = ctx.array_length(buf) as i32;
    let snap = match alloc_arraylist_with(ctx, buf, size0) {
        Ok(s) => s,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let pin = ctx.pin_native_root(snap);
    let ret = match ctx.invoke_virtual(snap, method, descriptor, &args[1..]) {
        Ok(r) => r,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };

    // Allocate the replacement parent backing once, then re-read every object
    // ref (the allocations above may have moved them) before the alloc-free copy.
    let this = ctx.read_native_pin(this_pin, this);
    let (parent, offset, size, _) = match asl_state(ctx, this) {
        Some(s) => s,
        None => {
            ctx.unpin_native_roots(this_pin);
            return Ok(ret);
        }
    };
    let snap_r = ctx.read_native_pin(pin, snap);
    let (_, new_size) = al_state(ctx, snap_r);
    let (_, psize) = al_state(ctx, parent);
    let new_psize = (psize - size + new_size).max(0);
    let new_buf: ObjectRef =
        alloc_ref_array(ctx, std::cmp::max(new_psize as usize, AL_DEFAULT_CAPACITY));

    let this = ctx.read_native_pin(this_pin, this);
    let snap_r = ctx.read_native_pin(pin, snap);
    let (parent, offset, size, _) = asl_state(ctx, this).unwrap_or((parent, offset, size, 0));
    let (sdata, new_size) = al_state(ctx, snap_r);
    let (pdata, psize) = al_state(ctx, parent);

    let mut w = 0usize;
    if let Some(pd) = pdata {
        for i in 0..(offset as usize) {
            let v = ctx.get_array_element(pd, i);
            ctx.set_array_element(new_buf, w, v);
            w += 1;
        }
    }
    if let Some(sd) = sdata {
        for k in 0..(new_size as usize) {
            let v = ctx.get_array_element(sd, k);
            ctx.set_array_element(new_buf, w, v);
            w += 1;
        }
    }
    if let Some(pd) = pdata {
        for i in ((offset + size) as usize)..(psize as usize) {
            let v = ctx.get_array_element(pd, i);
            ctx.set_array_element(new_buf, w, v);
            w += 1;
        }
    }
    al_set_data(ctx, parent, new_buf);
    al_set_size(ctx, parent, w as i32);
    let this_base = asl_base(ctx, this);
    ctx.set_field(this, this_base + ASL_FIELD_SIZE, Value::Int(new_size));
    ctx.set_field(this, this_base + ASL_FIELD_EXPECTED, Value::Int(w as i32));
    // Ripple the element-count change up the enclosing views, the way the JDK's
    // `SubList.updateSizeAndModCount` walks its own `parent` chain. Without
    // this, `sub.subList(1,3).clear()` leaves `sub` holding the pre-mutation
    // `expected`, so the very next read of `sub` raises
    // `ConcurrentModificationException` — where HotSpot answers `[B]`
    // (`subList.afterNestedClear`). Only ANCESTORS are updated: a SIBLING view
    // over the same parent is genuinely comodified and must keep failing, which
    // is what its now-stale `expected` does.
    //
    // No allocation in this loop, so the refs stay valid without pinning. The
    // hop cap is the same defensive bound `implements_comparable` uses: a cycle
    // is impossible by construction (a view's enclosing view is always older
    // than it), and a cap costs nothing to be sure.
    let delta = new_size - size;
    let mut cursor = asl_view_parent(ctx, this);
    for _ in 0..64 {
        let Some(v) = cursor else { break };
        let vb = asl_base(ctx, v);
        let vs = match ctx.get_field(v, vb + ASL_FIELD_SIZE) {
            Value::Int(s) => s,
            _ => break,
        };
        ctx.set_field(v, vb + ASL_FIELD_SIZE, Value::Int((vs + delta).max(0)));
        ctx.set_field(v, vb + ASL_FIELD_EXPECTED, Value::Int(w as i32));
        cursor = asl_view_parent(ctx, v);
    }
    ctx.unpin_native_roots(this_pin);
    Ok(ret)
}

#[allow(dead_code)]
pub(crate) fn native_asl_unsupported(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Retained for reference: the former fail-loud stopgap. Structural mutation
    // is now delegated through `asl_delegate_mutating` (live-view contract).
    Err(
        cratonvm_types::error::RuntimeError::UnsupportedOperationException {
            message: "structural modification of ArrayList.subList view is not supported"
                .to_string(),
        }
        .into(),
    )
}

pub(crate) fn native_al_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    // A `Map.values()` view is an `AbstractCollection`, which does not override
    // `hashCode` — so on a real JVM it hashes by IDENTITY, not by content. This
    // VM's view is an `ArrayList`-shaped carrier, so without this arm the
    // `AbstractList` 31*acc+e contract below runs on an object whose real class
    // never had it. See [`al_is_values_view`] for the HotSpot transcript.
    //
    // No `resync_values_view` first: an identity hash does not depend on the
    // contents, and re-collecting them would be a GC point for nothing.
    if al_is_values_view(ctx, this) {
        return Ok(Some(Value::Int(ctx.identity_hash_code(this))));
    }
    let (data, size) = al_state(ctx, this);
    let size = size as usize;
    let mut hash: i32 = 1;
    if let Some(d) = data {
        // GC-safety: `element_hash_code` can invoke an element's real
        // `hashCode()` (Java bytecode), which can trigger a moving GC. The
        // backing array `d` is read again on every loop iteration; pin it
        // once up front and re-read from the pin before each use. Confirmed
        // live via CRATONVM_DBG_STALE_OBJREF during WildFly
        // parallel-extension-add (same "Family 1" pattern as
        // native_hashmap_get_exact's `map_keys_equal` fix -- see
        // wildfly-parallel-boot-stale-objectref-residual.md).
        let d_pin = ctx.pin_native_root(d);
        for i in 0..size {
            let d_cur = ctx.read_native_pin(d_pin, d);
            let val = ctx.get_array_element(d_cur, i);
            // List.hashCode contract: 31*acc + e.hashCode() (0 for null).
            let elem_hash = match element_hash_code(ctx, &val) {
                Ok(h) => h,
                Err(e) => {
                    ctx.unpin_native_roots(d_pin);
                    return Err(e);
                }
            };
            hash = hash.wrapping_mul(31).wrapping_add(elem_hash);
        }
        ctx.unpin_native_roots(d_pin);
    }
    Ok(Some(Value::Int(hash)))
}

pub(crate) fn native_al_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this_raw = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other_raw = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Identity check (on the original references, before unwrapping).
    if std::ptr::eq(this_raw.as_ptr(), other_raw.as_ptr()) {
        return Ok(Some(Value::Int(1)));
    }
    // See through unmodifiable wrappers on either side so an ArrayList can be
    // compared element-by-element against a `List.of(...)` /
    // `Collections.unmodifiableList(...)` view. `al_state` reads the backing
    // array directly and would otherwise see the wrapper's slot-0 backing
    // pointer (a non-array), reporting size 0 and a spurious inequality.
    let this = unwrap_unmod(ctx, this_raw);
    let other = unwrap_unmod(ctx, other_raw);
    if std::ptr::eq(this.as_ptr(), other.as_ptr()) {
        return Ok(Some(Value::Int(1)));
    }
    // A `Map.values()` view is an `AbstractCollection`, which does not override
    // `equals` — so on a real JVM two distinct views are NEVER equal, however
    // equal their contents, and a view is never equal to a `List` of the same
    // elements in either direction. Both identity checks above have already
    // run, so reaching here with a view on either side means "not the same
    // object", which is the whole of the answer. See [`al_is_values_view`].
    //
    // Order matters: this must sit AFTER the identity checks (so `v.equals(v)`
    // still answers true, as HotSpot's `selfView.equals=true` records) and
    // BEFORE `al_state`, so no content is ever compared.
    if al_is_values_view(ctx, this) || al_is_values_view(ctx, other) {
        return Ok(Some(Value::Int(0)));
    }
    let (data_a, size_a) = al_state(ctx, this);
    let (data_b, size_b) = al_state(ctx, other);
    // Cross-layout comparison: either side may be a List that is NOT
    // ArrayList-shaped — e.g. `ArrayList.equals(LinkedList)` (and the
    // reverse, since this native is registered on the List interface).
    // `al_state` reads ArrayList slots on such a receiver and reports
    // data=None, and the old strict match answered `false` even for equal
    // contents. The AbstractList.equals contract is defined by element
    // ITERATION, not by class — walk both sides' own iterator() instead.
    // (kafka ConfigDef testGroupInference: assertEquals(ArrayList, LinkedList).)
    if data_a.is_none() || data_b.is_none() {
        // Spec guard: a non-List operand is never equal — but synthetic
        // allocs (class id 0 / bare Object) carry no usable hierarchy, so
        // only reject when the class is known and provably not a List.
        if !al_eq_operand_is_list(ctx, other) || !al_eq_operand_is_list(ctx, this) {
            return Ok(Some(Value::Int(0)));
        }
        // Nothing of ours is pinned yet, so a refusal on the first operand can
        // propagate with a bare `?`.
        let ea = collection_elements_generic(ctx, this)?;
        let (ea_pin, ea_handles) = pin_value_slice(ctx, &ea);
        // The second operand is where `arrayList.equals(linkedList)` refuses
        // under `--jdk-only`, and by now `ea`'s elements are pinned — so this
        // one has to unwind before it propagates. `?` here would leak
        // `ea_pin`'s roots for the rest of the run.
        let eb = match collection_elements_generic(ctx, other) {
            Ok(eb) => eb,
            Err(e) => {
                if ea_pin != usize::MAX {
                    ctx.unpin_native_roots(ea_pin);
                }
                return Err(e);
            }
        };
        let (eb_pin, eb_handles) = pin_value_slice(ctx, &eb);
        let pin_base = if ea_pin == usize::MAX { eb_pin } else { ea_pin };
        let equal: Result<bool, MethodCallFailed> = (|| {
            if ea.len() != eb.len() {
                return Ok(false);
            }
            for i in 0..ea.len() {
                let va = read_pinned_elem(ctx, ea_handles[i], ea[i]);
                let vb = read_pinned_elem(ctx, eb_handles[i], eb[i]);
                if !values_equal_deep(ctx, &va, &vb)? {
                    return Ok(false);
                }
            }
            Ok(true)
        })();
        ctx.unpin_native_roots(pin_base);
        return Ok(Some(Value::Int(if equal? { 1 } else { 0 })));
    }
    if size_a != size_b {
        return Ok(Some(Value::Int(0)));
    }
    let size = size_a as usize;
    match (data_a, data_b) {
        (Some(da), Some(db)) => {
            let da_pin = ctx.pin_native_root(da);
            let db_pin = ctx.pin_native_root(db);
            let equal: Result<bool, MethodCallFailed> = (|| {
                for i in 0..size {
                    let da = ctx.read_native_pin(da_pin, da);
                    let va = ctx.get_array_element(da, i);
                    let db = ctx.read_native_pin(db_pin, db);
                    let vb = ctx.get_array_element(db, i);
                    if !values_equal_deep(ctx, &va, &vb)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            })();
            ctx.unpin_native_roots(da_pin);
            Ok(Some(Value::Int(if equal? { 1 } else { 0 })))
        }
        (None, None) => Ok(Some(Value::Int(1))),
        _ => Ok(Some(Value::Int(0))),
    }
}

/// Deep value equality for collection elements: `values_equal` plus a
/// virtual `equals(Object)` fallback for arbitrary object types — the same
/// gap `map_keys_equal` closed for HashMap keys. Without the fallback, two
/// equal-but-distinct objects of any type that is not a String / enum /
/// boxed primitive (e.g. `java.util.UUID`) compare as NOT equal:
/// `List.of(uuid).equals(List.of(sameUuidParsedBack))` returned false
/// (kafka listSerde round-trip assertEquals on ArrayList<UUID>).
pub(crate) fn values_equal_deep(
    ctx: &mut dyn NativeContext,
    a: &Value,
    b: &Value,
) -> Result<bool, MethodCallFailed> {
    if values_equal(ctx, a, b) {
        return Ok(true);
    }
    if let (Value::Object(Some(oa)), Value::Object(Some(ob))) = (a, b) {
        return map_keys_equal(ctx, *oa, *ob);
    }
    Ok(false)
}

/// Is `obj` usable as a List operand for `equals`? Known non-List classes
/// (a HashSet, a Map view, …) must compare unequal per the List.equals
/// contract; synthetic allocations whose class id is 0 / bare Object carry
/// no hierarchy information, so they are accepted leniently (matches the
/// old behavior for our internal placeholder lists).
pub(crate) fn al_eq_operand_is_list(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    let cid = ctx.class_id_of_object(obj);
    let name = match ctx.class_name_of_id(cid) {
        Some(n) => n,
        None => return true, // synthetic / unknown — lenient
    };
    if name.is_empty() || name == "java/lang/Object" {
        return true;
    }
    match ctx.class_id_by_name("java/util/List") {
        Some(list_id) => ctx.is_subclass(cid, list_id),
        None => true,
    }
}

