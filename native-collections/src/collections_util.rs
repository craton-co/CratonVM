// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

// ===========================================================================
// Collections utilities
// ===========================================================================

pub(crate) fn register_collections_utility_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/util/Collections";
    r.register(c, "<clinit>", "()V", native_collections_clinit);
    r.register(c, "sort", "(Ljava/util/List;)V", native_collections_sort);
    r.register(
        c,
        "emptyList",
        "()Ljava/util/List;",
        native_collections_empty_list,
    );
    r.register(
        c,
        "singletonList",
        "(Ljava/lang/Object;)Ljava/util/List;",
        native_collections_singleton_list,
    );
    r.register(
        c,
        "reverse",
        "(Ljava/util/List;)V",
        native_collections_reverse,
    );
    // `SyntheticStub`, matching `register_unmodifiable_natives` (L7 R1). This
    // is the DUPLICATE of that registration, in a different registrar, and it
    // wins by last-write — so the retag there had no effect on the triple that
    // actually dispatches until this one moved too.
    r.register_with_kind(
        c,
        "unmodifiableList",
        "(Ljava/util/List;)Ljava/util/List;",
        native_collections_unmodifiable_list,
        cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        c,
        "sort",
        "(Ljava/util/List;Ljava/util/Comparator;)V",
        native_collections_sort_comparator,
    );
    r.set_category(__prev_cat);
}

/// `Collections.sort(List)`: JDK 25 is `list.sort(null)`. A program class
/// that overrides `sort(Comparator)` gets that call
/// ([`collections_sort_override`]); every other receiver takes the native
/// natural-order sort, [`list_sort_natural`].
pub(crate) fn native_collections_sort(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = collections_sort_override(ctx, args.first(), Value::Object(None)) {
        return r;
    }
    list_sort_natural(ctx, args)
}

/// `CRATONVM_COMPAT_COLLECTIONS_SORT_VIRTUAL` -- default ON, `=0` restores
/// the native sort for every receiver. JDK 25 `Collections.sort(list[, c])`
/// is `list.sort(c)`, so a program's `ArrayList` subclass that overrides
/// `sort(Comparator)` runs its override on HotSpot, which may log, refuse,
/// sort differently or call `super.sort(c)`. The `--compatible` natives
/// sorted the backing array directly and never called it (cmecompat backlog
/// b5). Round 16 wave 14, lane compat2. Read live, per call.
pub(crate) fn collections_sort_virtual_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_COMPAT_COLLECTIONS_SORT_VIRTUAL")
}

/// `Collections.sort`'s `list.sort(comparator)` as a virtual call, when
/// `list` is non-null and a program class overrides `sort(Comparator)`
/// ([`program_class_overrides`]); `None` -- run the native sort -- otherwise.
///
/// Only the two `Collections.sort` entry points ask. `ArrayList.sort` /
/// `Vector.sort` themselves ([`native_al_sort_comparator`]) do not, and the
/// natural-order body they share with `Collections.sort(List)`
/// ([`list_sort_natural`]) does not either, so an override's
/// `super.sort(c)` reaches the native sort and cannot come back here.
pub(crate) fn collections_sort_override(
    ctx: &mut dyn NativeContext,
    list: Option<&Value>,
    comparator: Value,
) -> Option<MethodCallResult> {
    let Some(Value::Object(Some(list))) = list else {
        return None;
    };
    if !collections_sort_virtual_enabled()
        || !program_class_overrides(&*ctx, *list, "sort", "(Ljava/util/Comparator;)V")
    {
        return None;
    }
    Some(ctx.invoke_virtual(*list, "sort", "(Ljava/util/Comparator;)V", &[comparator]))
}

/// The natural-order list sort behind `Collections.sort(List)` and
/// `ArrayList.sort(null)`.
pub(crate) fn list_sort_natural(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A NULL list is a NullPointerException, not a silent no-op. MEASURED in
    // BOTH modes (`probes/BaosCollectionsShadowSweep.java`): HotSpot NPE, this
    // VM returned normally, so `Collections.sort(maybeNull)` looked like it had
    // sorted something.
    let Some(Value::Object(Some(list))) = args.first() else {
        return Err(cratonvm_types::error::RuntimeError::NullPointerException {
            message: Some("Collections.sort: list is null".to_string()),
        }
        .into());
    };
    let list = *list;
    // An IMMUTABLE list must refuse. MEASURED in COMPATIBLE mode only --
    // `--jdk-only` already threw UnsupportedOperationException, because it runs
    // the real bytecode:
    //
    //   Collections.sort(List.of("b", "a"))
    //     HotSpot     UnsupportedOperationException
    //     --jdk-only  same
    //     compatible  no-throw
    //
    // and "no-throw" here does not mean it declined: `al_state` reads the
    // backing array of an `ImmutableCollections.ListN` just as happily as an
    // ArrayList's, so the native SORTED AN IMMUTABLE LIST IN PLACE. Every
    // holder of that list -- and `List.of` is shared, interned and handed
    // around precisely because it cannot change -- observed its contents
    // reorder underneath it.
    //
    // The receiver's class name is `cratonvm/internal/Unmodifiable*`, NOT
    // `java/util/ImmutableCollections$*`: this VM funnels every unmodifiable
    // view through seven synthetic classes and FAKES `getClass()` to report the
    // JDK name (`getclass_immutable_marker` in native-builtins). So a guard
    // written against the name the probe prints -- which is what I tried first
    // -- can never fire. Screening on the real receiver class also catches
    // `Collections.unmodifiableList(..)`, which must refuse for the same reason.
    //
    // Delegating to the receiver's own `sort` runs the real refusal body; that
    // is the same route the non-ArrayList branch below already takes.
    if object_class_name(ctx, list).starts_with("cratonvm/internal/Unmodifiable") {
        return ctx.invoke_virtual(
            list,
            "sort",
            "(Ljava/util/Comparator;)V",
            &[Value::Object(None)],
        );
    }
    let (data, size) = al_state(ctx, list);
    let data = match data {
        Some(d) => d,
        // Non-ArrayList list (LinkedList, CopyOnWriteArrayList, …): al_state
        // can't read its backing array. Collections.sort(list) is defined as
        // list.sort(null) in the JDK — delegate to the receiver's own sort
        // bytecode (sort is native only on ArrayList, so this runs the real
        // AbstractList.sort for everything else). Was a silent no-op (kcfull
        // #17b sibling).
        None => {
            return ctx.invoke_virtual(
                list,
                "sort",
                "(Ljava/util/Comparator;)V",
                &[Value::Object(None)],
            )
        }
    };
    let len = size as usize;
    // `ArrayList.sort` / `Vector.sort`: `expectedModCount = modCount` before
    // the sort, `modCount++` after it (see [`al_sort_mod_count_before`]).
    let mod_count_before = al_sort_mod_count_before(&*ctx, list);
    if len <= 1 {
        return al_sort_mod_count_after(ctx, list, mod_count_before);
    }
    // Sort by the elements' natural ordering (Comparable.compareTo), matching
    // `Arrays.sort(Object[])`. The previous implementation sorted by each
    // element's `read_string()` representation, so `List<Integer>` /
    // `List<Date>` / any non-String Comparable produced an all-empty-string
    // key and the list was returned UNSORTED (B3). We mirror
    // `native_arrays_sort_objects`: verify Comparable, then merge-sort
    // dispatching through `compare_via_compare_to` (stable, O(n log n), and
    // correct with a fallible comparator — a thrown compareTo propagates
    // cleanly and short-circuits the sort).
    let mut items: Vec<Value> = Vec::with_capacity(len);
    for i in 0..len {
        items.push(ctx.get_array_element(data, i));
    }
    // Verify Comparable on every non-null element (JDK throws CCE otherwise),
    // and REFUSE a null one.
    //
    // MEASURED in both modes: `Collections.sort(list with a null)` returned
    // normally here where HotSpot throws NullPointerException. The JDK's NPE
    // comes out of the comparison itself -- `compareTo` is invoked ON the null
    // -- so it can only happen when the element is actually compared, which for
    // `len >= 2` it always is. `len <= 1` has already returned above, so this
    // loop is exactly the region where a null must throw.
    for v in &items {
        if matches!(v, Value::Object(None)) {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Collections.sort: null element".to_string()),
            }
            .into());
        }
        if let Value::Object(Some(obj)) = v {
            if !implements_comparable(ctx, *obj) {
                let cname = object_class_name(ctx, *obj);
                dbg_cce_backtrace("sort_preflight", ctx, *obj, None);
                return Err(cratonvm_types::error::RuntimeError::ClassCastException {
                    message: format!(
                        "element of class {} does not implement java.lang.Comparable",
                        cname
                    ),
                }
                .into());
            }
        }
    }
    // Stable merge sort (O(n log n)) with fallible comparator.
    //
    // GC-SAFETY (Family-1 stale-ObjectRef fix, mirroring sort_with_comparator's
    // established idiom): `compare_via_compare_to` dispatches the elements'
    // `compareTo` via `invoke_virtual` up to O(n log n) times, and each
    // dispatch can run arbitrary user bytecode and trigger a moving GC. `data`
    // was held raw across the whole sort and reused directly afterward by the
    // `set_array_element` write-back loop below -- and every entry in `items`
    // is a raw `ObjectRef` snapshot taken once before the sort and never
    // refreshed, so a GC triggered by comparing any OTHER pair can relocate an
    // element already sitting in `items`, leaving both the eventual write-back
    // and the *next* `compare_via_compare_to` dispatch on that element
    // operating on a stale/dangling reference. Pin `data` and every element
    // (via the same index-array trick as `sort_with_comparator`, which keeps
    // `items`/`elem_handles` index-stable while only the `idx` permutation is
    // shuffled by the merge), refresh each right before use.
    // `list` is read again after the sort (the `modCount` protocol), and every
    // `compareTo` can move it: pinned first, so its handle unwinds the rest.
    let list_pin = ctx.pin_native_root(list);
    let data_pin = ctx.pin_native_root(data);
    let (_, elem_handles) = pin_value_slice(ctx, &items);
    let mut idx: Vec<Value> = (0..items.len() as i32).map(Value::Int).collect();
    let sort_result = merge_sort_fallible(ctx, &mut idx, |c, a, b| {
        let ia = if let Value::Int(v) = a {
            *v as usize
        } else {
            0
        };
        let ib = if let Value::Int(v) = b {
            *v as usize
        } else {
            0
        };
        let ea = read_pinned_elem(c, elem_handles[ia], items[ia]);
        let eb = read_pinned_elem(c, elem_handles[ib], items[ib]);
        // See Arrays.sort(Object[]) above: retain the JDK's right-vs-left
        // natural-order probe for default-method Comparable implementations.
        compare_via_compare_to(c, &eb, &ea).map(i32::saturating_neg)
    });
    if let Err(e) = sort_result {
        ctx.unpin_native_roots(list_pin);
        return Err(e);
    }
    let data = ctx.read_native_pin(data_pin, data);
    for (out, slot) in idx.iter().enumerate() {
        let i = if let Value::Int(v) = slot {
            *v as usize
        } else {
            0
        };
        let val = read_pinned_elem(ctx, elem_handles[i], items[i]);
        ctx.set_array_element(data, out, val);
    }
    let list = ctx.read_native_pin(list_pin, list);
    ctx.unpin_native_roots(list_pin);
    al_sort_mod_count_after(ctx, list, mod_count_before)
}

/// Return the process-wide `Collections.EMPTY_LIST` / `EMPTY_MAP` / `EMPTY_SET`
/// singleton — the real `Collections$Empty*` instance that `Collections.<clinit>`
/// already populated into the named static field.
///
/// In the JDK, `Collections.emptyList()` is literally `(List<T>) EMPTY_LIST`, so
/// `emptyList() == EMPTY_LIST` and `emptyList() == emptyList()` hold by identity.
/// Code relies on that: WildFly's `ModelTestBootOperationsBuilder` initialises
/// its `bootOperations` field with `emptyList()` and later guards with
/// `bootOperations != Collections.EMPTY_LIST` ("Boot operations are already
/// set"). Allocating a fresh list per `emptyList()` call broke that identity and
/// made the guard throw on every `setXml`/`setXmlResource`. Returning the field
/// value keeps us identity-consistent with the (correctly populated) singleton.
pub(crate) fn collections_empty_singleton(ctx: &mut dyn NativeContext, field: &str) -> Option<Value> {
    let cid = ctx.class_id_by_name("java/util/Collections")?;
    let _ = ctx.ensure_class_initialized("java/util/Collections");
    let _ = ensure_collections_empty_singletons(ctx);
    let idx = ctx.static_field_index_by_name(cid, field)?;
    match ctx.get_static_field(cid, idx) {
        v @ Value::Object(Some(_)) => Some(v),
        _ => None,
    }
}

pub(crate) fn native_collections_clinit(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    ensure_collections_empty_singletons(ctx)?;
    Ok(None)
}

pub(crate) fn ensure_collections_empty_singletons(
    ctx: &mut dyn NativeContext,
) -> Result<(), MethodCallFailed> {
    let Some(cid) = ctx.class_id_by_name("java/util/Collections") else {
        return Ok(());
    };

    // The singletons MUST be genuinely immutable, non-ArrayList/HashMap/HashSet
    // typed instances. They were previously seeded as ORDINARY MUTABLE synthetic
    // `java/util/ArrayList` / `java/util/HashMap` / `java/util/HashSet` objects,
    // which broke process-global correctness two ways:
    //   1. `emptyList() instanceof ArrayList` answered true. kotlin-reflect's
    //      shaded protobuf (`SmallSortedMap.ensureEntryArrayMutable`) uses
    //      exactly that check to decide whether it must replace its
    //      `Collections.emptyList()` placeholder with a fresh ArrayList before
    //      inserting — so it skipped the replacement and mutated the singleton.
    //   2. The mutation SUCCEEDED (our ArrayList natives happily grow whatever
    //      has the arraylist layout), so from that point on EVERY
    //      `Collections.emptyList()` in the process contained a phantom
    //      `SmallSortedMap$Entry`: JUnit's `ReflectionUtils.findFields` blew up
    //      with "SmallSortedMap$Entry cannot be cast to Field" on ALL classes,
    //      taking down the whole Kotlin slice of the spring-webflux reactive
    //      suite (LOADERRs + jupiter discovery failures + per-test CCEs).
    // Seed the real JDK `Collections$Empty*` instances instead (their
    // mutators run real `AbstractList`/`AbstractMap` bytecode and throw
    // UnsupportedOperationException; `try_delegate_real_collection` already
    // handles them on every interface-registered native). Fall back to the
    // legacy mutable synthetics only when the real classes are unavailable
    // (synthetic-jdk mode), preserving the old behaviour there.
    if let Some(idx) = ctx.static_field_index_by_name(cid, "EMPTY_LIST") {
        if !matches!(ctx.get_static_field(cid, idx), Value::Object(Some(_))) {
            let list = match alloc_real_jdk(ctx, "java/util/Collections$EmptyList") {
                Some(list) => list,
                None => {
                    let __al_n_fields = al_slots(ctx).2;
                    let list = try_alloc_synthetic(ctx, "java/util/ArrayList", __al_n_fields)?;
                    // gc-common w20-a: rooted across the array's allocation.
                    let (list, arr) = rooted_across1(ctx, list, |ctx| alloc_ref_array(ctx, 0));
                    al_set_data(ctx, list, arr);
                    al_set_size(ctx, list, 0);
                    list
                }
            };
            ctx.set_static_field(cid, idx, Value::Object(Some(list)));
        }
    }

    if let Some(idx) = ctx.static_field_index_by_name(cid, "EMPTY_MAP") {
        if !matches!(ctx.get_static_field(cid, idx), Value::Object(Some(_))) {
            let map = match alloc_real_jdk(ctx, "java/util/Collections$EmptyMap") {
                Some(m) => m,
                // gc-common w20-a: see `alloc_initialised_backing_map`.
                None => alloc_initialised_backing_map(ctx)?,
            };
            ctx.set_static_field(cid, idx, Value::Object(Some(map)));
        }
    }

    if let Some(idx) = ctx.static_field_index_by_name(cid, "EMPTY_SET") {
        if !matches!(ctx.get_static_field(cid, idx), Value::Object(Some(_))) {
            let set = match alloc_real_jdk(ctx, "java/util/Collections$EmptySet") {
                Some(s) => s,
                // gc-common w20-a: see `alloc_synthetic_hash_set`.
                None => alloc_synthetic_hash_set(ctx)?.0,
            };
            ctx.set_static_field(cid, idx, Value::Object(Some(set)));
        }
    }

    Ok(())
}

pub(crate) fn native_collections_empty_list(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    if let Some(v) = collections_empty_singleton(ctx, "EMPTY_LIST") {
        return Ok(Some(v));
    }
    // See `native_collections_empty_map` for why this precedes the synthetic:
    // the sibling `native_collections_singleton_list` just below already does
    // it, which is why `singletonList` reported the right class in
    // `--synthetic-jdk` while `emptyList` reported `java.util.ArrayList`.
    if let Some(o) = alloc_real_jdk(ctx, "java/util/Collections$EmptyList") {
        return Ok(Some(Value::Object(Some(o))));
    }
    // Last resort: fresh synthetic empty list.
    let __al_n_fields = al_slots(ctx).2;
    let list = try_alloc_synthetic(ctx, "java/util/ArrayList", __al_n_fields)?;
    // gc-common w20-a: rooted across the array's allocation.
    let (list, arr) = rooted_across1(ctx, list, |ctx| alloc_ref_array(ctx, 0));
    al_set_data(ctx, list, arr);
    al_set_size(ctx, list, 0);
    Ok(Some(Value::Object(Some(list))))
}

pub(crate) fn native_collections_singleton_list(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = args.first().copied().unwrap_or(Value::Object(None));
    // gc-common w20-a: `val` is a bare copy of the argument, and both arms
    // below allocate (the real arm may also run `<clinit>`) before storing it;
    // the synthetic arm also stored its backing array into the list's
    // pre-allocation address. Pinned for the whole function, read back at
    // each store.
    let val_pin = pin_value(ctx, val);
    let out = singleton_list_pinned(ctx, val, val_pin);
    if val_pin != usize::MAX {
        ctx.unpin_native_roots(val_pin);
    }
    out
}

/// [`native_collections_singleton_list`]'s body, with the element pinned at
/// `val_pin` (`usize::MAX` for a non-reference).
pub(crate) fn singleton_list_pinned(
    ctx: &mut dyn NativeContext,
    val: Value,
    val_pin: usize,
) -> MethodCallResult {
    // Real-JDK: return a real `Collections$SingletonList` (its size/get/iterator
    // run real bytecode; size/isEmpty reach the interface natives which delegate
    // back to it). Falls back to a synthetic 1-element ArrayList if unavailable.
    if let Some(o) = alloc_real_jdk(ctx, "java/util/Collections$SingletonList") {
        let val = read_pinned_elem(ctx, val_pin, val);
        ctx.set_field_by_name(o, "element", val);
        return Ok(Some(Value::Object(Some(o))));
    }
    let __al_n_fields = al_slots(ctx).2;
    let list = try_alloc_synthetic(ctx, "java/util/ArrayList", __al_n_fields)?;
    let list_pin = ctx.pin_native_root(list);
    let arr: ObjectRef = alloc_ref_array(ctx, 1);
    let list = ctx.read_native_pin(list_pin, list);
    ctx.unpin_native_roots(list_pin);
    let val = read_pinned_elem(ctx, val_pin, val);
    ctx.set_array_element(arr, 0, val);
    al_set_data(ctx, list, arr);
    al_set_size(ctx, list, 1);
    Ok(Some(Value::Object(Some(list))))
}

pub(crate) fn native_collections_reverse(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reject_null_collections_arg(args.first())?;
    let list = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let (data, size) = al_state(ctx, list);
    let data = match data {
        Some(d) => d,
        // Not an ArrayList-style backing (e.g. a LinkedList overlay, or any
        // other List): fall back to a generic reverse driven by the list's own
        // size()/get(int)/set(int,E). Without this, Collections.reverse was a
        // silent no-op on every non-ArrayList List — e.g. Gradle
        // GFileUtils.mkdirs reverses a LinkedList of parent dirs, so cache dirs
        // were created child-before-parent ("… is not a directory").
        None => return collections_reverse_generic(ctx, list),
    };
    let len = size as usize;
    // Read all elements
    let mut elems: Vec<Value> = Vec::with_capacity(len);
    for i in 0..len {
        elems.push(ctx.get_array_element(data, i));
    }
    // Write back in reverse
    for (i, val) in elems.iter().rev().enumerate() {
        ctx.set_array_element(data, i, *val);
    }
    Ok(None)
}

/// Generic `Collections.reverse` for any `List` that is not an ArrayList-style
/// backing: read every element via `get(int)`, then write them back reversed via
/// `set(int, E)`. Uses the list's own (native or bytecode) methods, so it works
/// for LinkedList, sublists, etc.
pub(crate) fn collections_reverse_generic(ctx: &mut dyn NativeContext, list: ObjectRef) -> MethodCallResult {
    // cceres3: pin across GC-capable call (stream stale-at-store wave) — every
    // size/get/set dispatch re-enters Java; re-read `list` and the gathered
    // elements through pins per iteration.
    let list_pin = ctx.pin_native_root(list);
    // invoke_virtual dispatches on the receiver's actual class (LinkedList, …).
    let size = match ctx.invoke_virtual(list, "size", "()I", &[]) {
        Ok(Some(Value::Int(n))) => n,
        _ => {
            ctx.unpin_native_roots(list_pin);
            return Ok(None);
        }
    };
    if size <= 1 {
        ctx.unpin_native_roots(list_pin);
        return Ok(None);
    }
    let len = size as usize;
    let mut elems: Vec<Value> = Vec::with_capacity(len);
    let mut elem_handles: Vec<usize> = Vec::with_capacity(len);
    for i in 0..len {
        let list = ctx.read_native_pin(list_pin, list);
        let v = match ctx.invoke_virtual(
            list,
            "get",
            "(I)Ljava/lang/Object;",
            &[Value::Int(i as i32)],
        ) {
            Ok(r) => r.unwrap_or(Value::Object(None)),
            Err(e) => {
                ctx.unpin_native_roots(list_pin);
                return Err(e);
            }
        };
        elem_handles.push(pin_value(ctx, v));
        elems.push(v);
    }
    for (i, (val, handle)) in elems
        .iter()
        .rev()
        .zip(elem_handles.iter().rev())
        .enumerate()
    {
        let list = ctx.read_native_pin(list_pin, list);
        let val = read_pinned_elem(ctx, *handle, *val);
        if let Err(e) = ctx.invoke_virtual(
            list,
            "set",
            "(ILjava/lang/Object;)Ljava/lang/Object;",
            &[Value::Int(i as i32), val],
        ) {
            ctx.unpin_native_roots(list_pin);
            return Err(e);
        }
    }
    ctx.unpin_native_roots(list_pin);
    Ok(None)
}

pub(crate) fn native_collections_unmodifiable_list(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    reject_null_collections_arg(args.first())?;
    // Wrap the source list in a live `UnmodifiableList` view: reads delegate
    // to the backing list (so later mutations of the backing list are
    // visible), and every mutator throws `UnsupportedOperationException`.
    let src = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `Collections.unmodifiableList` is idempotent by identity:
    //
    // ```java
    // if (list.getClass() == UnmodifiableList.class ||
    //     list.getClass() == UnmodifiableRandomAccessList.class)
    //     return (List<T>) list;
    // ```
    //
    // `unmodifiableList.rewrapIsNewObject` measured
    // `Collections.unmodifiableList(un) == un` as `false` here against HotSpot's
    // `true`. A second wrapper is not merely wasteful — it is a different
    // object, so a caller that re-wraps defensively and then compares by
    // identity (or uses the result as a map key) sees two distinct lists where
    // the JDK has one, and every extra layer costs another delegation hop on
    // every read.
    //
    // The test is exact class identity, as the JDK's is, and NOT
    // [`unmod_is_immutable`]: a `List.of(...)` shares this wrapper class here
    // but reports `ImmutableCollections$ListN` from `getClass()`, and the JDK
    // does wrap that one — its two `==` comparands are the `Collections$Unmodifiable*`
    // classes only. So the immutable marker has to exclude, not include.
    if ctx.object_num_fields(src) > UNMOD_FIELD_IMMUTABLE
        && ctx.class_name_of_id(ctx.class_id_of_object(src)).as_deref() == Some(UNMOD_LIST_CLASS)
        && !matches!(ctx.get_field(src, UNMOD_FIELD_IMMUTABLE), Value::Int(1))
    {
        return Ok(Some(Value::Object(Some(src))));
    }
    let w = alloc_unmod_wrapper(ctx, UNMOD_LIST_CLASS, src)?;
    Ok(Some(Value::Object(Some(w))))
}

// ===========================================================================
// forEach implementations
// ===========================================================================

pub(crate) fn native_al_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // RULE F, and deliberately AHEAD of both routes. MEASURED: a null action
    // throws on a `values()` / `keySet()` / `entrySet()` view exactly as it
    // does on a plain list, and on an EMPTY one, so nothing is gained by
    // rebuilding a carrier first — and `vc_route` re-enters this same body,
    // where the check would fire anyway. See `reject_null_functional`.
    reject_null_functional(args.get(1))?;
    if let Some(r) = ksv_route(ctx, args, native_ksv_for_each) {
        return r;
    }
    if let Some(r) = vc_route(ctx, args, native_al_for_each) {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let action = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    // GC-safety, and it has to happen HERE rather than at the pin group below.
    //
    // `resync_values_view` and `al_or_collection_elements` both allocate - the
    // second materialises a whole element vector through the receiver's real
    // iterator, which runs Java - so a moving young collection can relocate
    // `this` and `action` before either is pinned. `action` is the one that
    // bites, because nothing between here and `invoke_virtual` reads it: the
    // pin-time canary caught `ctx.pin_native_root(action)` receiving an address
    // that named no live object on BindableTests under
    // `CRATONVM_DBG_GC_STRESS`, and the dead receiver reached `accept()`.
    //
    // `action_pin` is the group base: every pin taken below it is released by
    // the single `unpin_native_roots(action_pin)` on the way out.
    let action_pin = ctx.pin_native_root(action);
    let this_pin_pre = ctx.pin_native_root(this);
    let this = ctx.read_native_pin(this_pin_pre, this);
    let this = match resync_values_view(ctx, this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(action_pin);
            return Err(e);
        }
    };
    // Pin what `resync_values_view` RETURNED, not what it was given: it can
    // rebuild the carrier, and a freshly allocated one is covered by no pin
    // taken before the call.
    let this_pin_mid = ctx.pin_native_root(this);
    // Collect elements first to avoid borrowing issues during invoke_virtual.
    // Use the generic helper so non-ArrayList collections (EnumSet/TreeSet/…)
    // routed here through the AbstractCollection/Iterable interface natives are
    // materialised via their real iterator instead of seeing an empty backing.
    // (Before 2026-08-01 the helper had no such fallback — this comment
    // described `native_al_to_array`'s inlined copy of it, not what `forEach`
    // actually did, so an unmodelled receiver's `forEach` visited nothing.)
    let elems = match al_or_collection_elements(ctx, this) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(action_pin);
            return Err(e);
        }
    };
    let this = ctx.read_native_pin(this_pin_mid, this);
    let action = ctx.read_native_pin(action_pin, action);
    // GC-safety: each `accept()` body runs arbitrary Java bytecode via
    // invoke_virtual and can allocate → moving young GC relocates `action` and
    // any object-typed element. The raw ObjectRefs captured here are NOT GC
    // roots, so without pinning they go stale and the *next* invoke_virtual
    // faults in `class_id_of` (kafka GarbageCollectedMemoryPoolTest SIGSEGV).
    // Pin `action` + the object elements and re-read the forwarded refs from
    // the pin slots on every iteration.
    // `ArrayList.forEach` is FAIL-FAST, and the snapshot this native takes is
    // exactly what hides that: the JDK's loop is
    // `for (int i = 0; modCount == expectedModCount && i < size; i++)` with a
    // final re-check, so `l.forEach(x -> l.clear())` throws rather than
    // completing over a list that no longer exists. MEASURED: no-throw
    // (apps/probes/ArrayListShadowSweep 50).
    //
    // `al_mod_count` answers `None` for a receiver with no usable slot -- a view
    // carrier, a synthetic layout -- and the check is then SKIPPED rather than
    // guessed, which is the direction `al_mod_count_slot`'s own doc calls safe.
    // `this` is pinned FIRST so the whole group still unwinds with one call.
    let this_pin = ctx.pin_native_root(this);
    let pin_base = ctx.pin_native_root(action);
    let elem_pins: Vec<(Value, Option<(usize, ObjectRef)>)> = elems
        .iter()
        .map(|v| match v {
            Value::Object(Some(r)) => (*v, Some((ctx.pin_native_root(*r), *r))),
            _ => (*v, None),
        })
        .collect();
    // AFTER the pins, not before: it reads a field through the receiver, and
    // every reference this native still held bare at that point would have
    // been unprotected across it.
    let expected_mod = al_mod_count(ctx, this);
    let mut result: MethodCallResult = Ok(None);
    for (orig, handle) in &elem_pins {
        let this_cur = ctx.read_native_pin(this_pin, this);
        if al_comodified(ctx, this_cur, expected_mod) {
            result =
                Err(cratonvm_types::error::RuntimeError::ConcurrentModificationException.into());
            break;
        }
        let action_cur = ctx.read_native_pin(pin_base, action);
        let elem_val = match handle {
            Some((h, fallback)) => Value::Object(Some(ctx.read_native_pin(*h, *fallback))),
            None => *orig,
        };
        if let Err(e) =
            ctx.invoke_virtual(action_cur, "accept", "(Ljava/lang/Object;)V", &[elem_val])
        {
            result = Err(e);
            break;
        }
    }
    if result.is_ok() {
        // The JDK's trailing `if (modCount != expectedModCount) throw` — the
        // half that catches a modification made by the LAST element's action.
        let this_cur = ctx.read_native_pin(this_pin, this);
        if al_comodified(ctx, this_cur, expected_mod) {
            result =
                Err(cratonvm_types::error::RuntimeError::ConcurrentModificationException.into());
        }
    }
    // `action_pin` is the group base - see its comment above.
    ctx.unpin_native_roots(action_pin);
    result
}

/// Has `this`'s `modCount` moved since `expected` was taken?
///
/// `false` when either side is unknown: a receiver with no usable `modCount`
/// slot loses comodification detection, which is the same trade
/// [`al_mod_count_slot`] already documents, and is strictly better than
/// inventing a `ConcurrentModificationException` from a slot that is really
/// somebody's `this$0`.
#[inline]
pub(crate) fn al_comodified(ctx: &dyn NativeContext, this: ObjectRef, expected: Option<i32>) -> bool {
    match (expected, al_mod_count(ctx, this)) {
        (Some(exp), Some(cur)) => exp != cur,
        _ => false,
    }
}

pub(crate) fn native_hibernate_persistent_map_for_each(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let action = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };

    let this_pin = ctx.pin_native_root(this);
    let action_pin = ctx.pin_native_root(action);
    if !matches!(ctx.get_field_by_name(this, "map"), Value::Object(Some(_))) {
        let this_cur = ctx.read_native_pin(this_pin, this);
        let _ = ctx.invoke_virtual(this_cur, "size", "()I", &[]);
    }
    let this_cur = ctx.read_native_pin(this_pin, this);
    let backing = match ctx.get_field_by_name(this_cur, "map") {
        Value::Object(Some(m)) => m,
        _ => {
            ctx.unpin_native_roots(action_pin);
            ctx.unpin_native_roots(this_pin);
            return Ok(None);
        }
    };
    let entries = collect_entries_any(ctx, backing)?;
    let keys: Vec<Value> = entries.iter().map(|(k, _)| *k).collect();
    let vals: Vec<Value> = entries.iter().map(|(_, v)| *v).collect();
    let (key_pin_base, key_pins) = pin_value_slice(ctx, &keys);
    let (val_pin_base, val_pins) = pin_value_slice(ctx, &vals);
    let mut result = Ok(None);
    for i in 0..entries.len() {
        let action_cur = ctx.read_native_pin(action_pin, action);
        let key = read_pinned_elem(ctx, key_pins[i], keys[i]);
        let val = read_pinned_elem(ctx, val_pins[i], vals[i]);
        if let Err(e) = ctx.invoke_virtual(
            action_cur,
            "accept",
            "(Ljava/lang/Object;Ljava/lang/Object;)V",
            &[key, val],
        ) {
            result = Err(e);
            break;
        }
    }
    ctx.unpin_native_roots(val_pin_base);
    ctx.unpin_native_roots(key_pin_base);
    ctx.unpin_native_roots(action_pin);
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_map_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    // RULE F. MEASURED bare NPE on every map receiver INCLUDING an empty one,
    // so the check precedes the entry walk. See `reject_null_functional`.
    reject_null_functional(args.get(1))?;
    // A JDK-model `ConcurrentHashMap` (nio21 w1 L4): its own `forEach`, which
    // walks the live table as the JDK's does.
    if let Some(r) = chm_jdk_delegate(
        ctx,
        args,
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        1,
    ) {
        return r;
    }
    let action = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    // `Map.forEach` is a default interface method. Running the real JDK body
    // on a synthetic immutable map falls back through `entrySet()` and a
    // HashSet view, which hashes every entry while constructing its iterator.
    // That is observably wrong for a map whose value refers back to the map:
    // iteration must hand the value to the consumer, not evaluate its
    // `hashCode()`. Use the concrete-backend collector so this bridge is also
    // safe for LinkedHashMap, TreeMap, immutable wrappers, and foreign maps.
    // `HashMap.forEach`'s `int mc = modCount`, taken before any Java runs
    // (`CRATONVM_COMPAT_MAP_REMAP_CME`, see `map_for_each_mod_count`).
    let mod_before = map_for_each_mod_count(&*ctx, this);
    // `CRATONVM_COMPAT_MAP_FOREACH_LIVE`: a plain `HashMap` walks its own
    // table, as `HashMap.forEach` does (see `map_for_each_live`); an
    // integer-overlay map reads each value when it is handed over.
    if let Some(result) = map_for_each_live(ctx, this, action, mod_before) {
        return result;
    }
    let refresh_overlay = map_for_each_live_enabled() && hm_int_fast_len(&*ctx, this).is_some();
    let this_pin = ctx.pin_native_root(this);
    let entries = match collect_entries_any(ctx, this) {
        Ok(entries) => entries,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    // GC-SAFETY: the BiConsumer `accept` allocates → moving young GC relocates
    // `action` and every key/value; pin all and re-read from the handles before
    // each dispatch.
    let action_pin = ctx.pin_native_root(action);
    let keys: Vec<Value> = entries.iter().map(|(k, _)| *k).collect();
    let vals: Vec<Value> = entries.iter().map(|(_, v)| *v).collect();
    let (_, khandles) = pin_value_slice(ctx, &keys);
    let (_, vhandles) = pin_value_slice(ctx, &vals);
    let mut result = Ok(None);
    for i in 0..entries.len() {
        let a = ctx.read_native_pin(action_pin, action);
        let k = read_pinned_elem(ctx, khandles[i], keys[i]);
        let mut v = read_pinned_elem(ctx, vhandles[i], vals[i]);
        if refresh_overlay {
            // The JDK hands over `e.value` as it is NOW: a value an earlier
            // `accept` replaced arrives new, and a key it removed is not
            // visited. The overlay answers both without Java.
            let this_now = ctx.read_native_pin(this_pin, this);
            match hm_int_fast_lookup(&*ctx, this_now, k) {
                Some(Some(live)) => v = live,
                Some(None) => continue,
                None => {}
            }
        }
        if let Err(e) = ctx.invoke_virtual(
            a,
            "accept",
            "(Ljava/lang/Object;Ljava/lang/Object;)V",
            &[k, v],
        ) {
            result = Err(e);
            break;
        }
    }
    if result.is_ok() && map_remap_moved(&*ctx, ctx.read_native_pin(this_pin, this), mod_before) {
        result = Err(RuntimeError::ConcurrentModificationException.into());
    }
    // Releases `action_pin` and both slices too: they were pinned after it.
    ctx.unpin_native_roots(this_pin);
    result
}

/// `CRATONVM_COMPAT_MAP_FOREACH_LIVE` — default ON, `=0` restores the
/// snapshot. `HashMap.forEach` walks its live table:
///
/// ```java
/// if (size > 0 && (tab = table) != null) {
///     int mc = modCount;
///     for (Node<K,V> e : tab) for (; e != null; e = e.next) action.accept(e.key, e.value);
///     if (modCount != mc) throw new ConcurrentModificationException();
/// }
/// ```
///
/// The native collected every `(key, value)` pair first and handed the
/// action those, so a value the action replaced for a key it had not reached
/// yet arrived OLD (HotSpot: new), and a key it removed was still visited.
/// Round 13 wave 9, lane hashcompat4 (residual page item 1).
pub(crate) fn map_for_each_live_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_MAP_FOREACH_LIVE")
}

/// `HashMap.forEach` over the receiver's own table (see
/// [`map_for_each_live_enabled`]), for a plain `HashMap` (or user subclass)
/// with no integer-overlay row. `None`, before any Java has run, for every
/// other receiver. `mod_before` is the caller's [`map_for_each_mod_count`].
pub(crate) fn map_for_each_live(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    action: ObjectRef,
    mod_before: Option<i32>,
) -> Option<MethodCallResult> {
    if !map_for_each_live_enabled() {
        return None;
    }
    let facts = receiver_facts(&*ctx, this);
    if !facts.has(CF_BUCKET_MAP_NAME)
        || facts.has(CF_LHM)
        || facts.has(CF_CHM)
        || facts.has(CF_TREE_MAP)
        || facts.has(CF_IDENTITY_MAP)
        || facts.has(CF_UNMOD_WRAPPER)
        || facts.has(CF_HASHTABLE_ANCESTRY)
        || hm_int_fast_len(&*ctx, this).is_some()
    {
        return None;
    }
    let (buckets, size, _) = map_state(&*ctx, this);
    let tab = match buckets {
        Some(tab) if size > 0 => tab,
        _ => return Some(Ok(None)),
    };
    let len = ctx.array_length(tab);
    // A cycle in a corrupt chain must not spin forever. An action that adds
    // keys can make the walk visit more nodes than `size` (it also ends in a
    // `ConcurrentModificationException`), so the bound is generous.
    let limit = (size.max(0) as usize)
        .saturating_mul(2)
        .saturating_add(chain_walk_limit(size));
    let this_pin = ctx.pin_native_root(this);
    let tab_pin = ctx.pin_native_root(tab);
    let action_pin = ctx.pin_native_root(action);
    // Every pin above `this_pin` is released with it, on every exit.
    let result = (|| -> MethodCallResult {
        let mut visited: usize = 0;
        for i in 0..len {
            let tab_now = ctx.read_native_pin(tab_pin, tab);
            let mut cur = ctx.get_array_element(tab_now, i);
            while let Value::Object(Some(node)) = cur {
                visited += 1;
                if visited > limit {
                    let this_now = ctx.read_native_pin(this_pin, this);
                    return Err(if map_remap_moved(&*ctx, this_now, mod_before) {
                        RuntimeError::ConcurrentModificationException.into()
                    } else {
                        RuntimeError::IllegalStateException {
                            message: "hashmap chain exceeded safety cap; possible corruption"
                                .to_string(),
                        }
                        .into()
                    });
                }
                let node_pin = ctx.pin_native_root(node);
                let key = get_node_key(&*ctx, node);
                let value = get_node_value(&*ctx, node);
                let a = ctx.read_native_pin(action_pin, action);
                let accepted = ctx.invoke_virtual(
                    a,
                    "accept",
                    "(Ljava/lang/Object;Ljava/lang/Object;)V",
                    &[key, value],
                );
                let node = ctx.read_native_pin(node_pin, node);
                ctx.unpin_native_roots(node_pin);
                accepted?;
                cur = ctx.get_field(node, NODE_FIELD_NEXT);
            }
        }
        let this_now = ctx.read_native_pin(this_pin, this);
        if map_remap_moved(&*ctx, this_now, mod_before) {
            return Err(RuntimeError::ConcurrentModificationException.into());
        }
        Ok(None)
    })();
    ctx.unpin_native_roots(this_pin);
    Some(result)
}

pub(crate) fn native_hs_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A real `CopyOnWriteArraySet` is served by its own bytecode over the
    // `CopyOnWriteArrayList` in `al`, not by this file's map surface.
    // See [`cow_set_route`].
    if let Some(r) = cow_set_route(ctx, args, "forEach", "(Ljava/util/function/Consumer;)V") {
        return r;
    }
    // RULE F, ahead of the route — see the note in `native_al_for_each`.
    reject_null_functional(args.get(1))?;
    if let Some(r) = ksv_route(ctx, args, native_ksv_for_each) {
        return r;
    }
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let action = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    resync_view_set(ctx, &mut this)?;
    let backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => return Ok(None),
    };
    let keys = hs_view_elements(ctx, this, backing)?;
    // GC-SAFETY: `accept` allocates → moving young GC relocates `action` and the
    // keys; pin both and re-read each from its handle before dispatch.
    let action_pin = ctx.pin_native_root(action);
    let (_, khandles) = pin_value_slice(ctx, &keys);
    let mut result = Ok(None);
    for (i, &key) in keys.iter().enumerate() {
        let a = ctx.read_native_pin(action_pin, action);
        let k = read_pinned_elem(ctx, khandles[i], key);
        if let Err(e) = ctx.invoke_virtual(a, "accept", "(Ljava/lang/Object;)V", &[k]) {
            result = Err(e);
            break;
        }
    }
    ctx.unpin_native_roots(action_pin);
    result
}

// ===========================================================================
// Optional functional methods
// ===========================================================================

pub(crate) fn native_opt_if_present(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    // NOT RULE F: `ifPresent` has no `requireNonNull` — its body is
    // `if (value != null) action.accept(value);`, so `empty.ifPresent(null)`
    // does NOT throw. MEASURED (apps/probes/PqOptionalShadowSweep 97).
    let val_pre = ctx.get_field(this, OPT_FIELD_VALUE);
    if !opt_value_is_empty(val_pre) {
        reject_null_functional(args.get(1))?;
    }
    let action = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    if !opt_value_is_empty(val) {
        ctx.invoke_virtual(action, "accept", "(Ljava/lang/Object;)V", &[val])?;
    }
    Ok(None)
}

pub(crate) fn native_opt_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // RULE F, ahead of everything: `Optional.map` opens
    // `Objects.requireNonNull(mapper)`, so an explicit null throws on an EMPTY
    // optional too. MEASURED both ways (apps/probes/PqOptionalShadowSweep 80-81).
    reject_null_functional(args.get(1))?;
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return native_opt_empty(ctx, &[]),
    };
    let mapper = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            // A MISSING argument (not an explicit null -- RULE F took that
            // above) is a malformed native call, e.g. an invokedynamic lambda
            // that never materialised during fake-JDK bootstrap. Preserve a
            // present value rather than turning `map` into empty.
            let val = ctx.get_field(this, OPT_FIELD_VALUE);
            if opt_value_is_empty(val) {
                return native_opt_empty(ctx, &[]);
            }
            return Ok(Some(Value::Object(Some(this))));
        }
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    if opt_value_is_empty(val) {
        return native_opt_empty(ctx, &[]);
    }
    let result = ctx.invoke_virtual(
        mapper,
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[val],
    )?;
    let mapped = result.unwrap_or(Value::Object(None));
    native_opt_of_nullable(ctx, &[mapped])
}

pub(crate) fn native_opt_flat_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // RULE F: `requireNonNull(mapper)` precedes the presence test.
    reject_null_functional(args.get(1))?;
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return native_opt_empty(ctx, &[]),
    };
    let mapper = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return native_opt_empty(ctx, &[]),
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    if opt_value_is_empty(val) {
        return native_opt_empty(ctx, &[]);
    }
    let result = ctx.invoke_virtual(
        mapper,
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[val],
    )?;
    // `Optional.flatMap` ends `return Objects.requireNonNull(r);` — a mapper
    // that answers null is a caller bug, and answering an empty Optional for it
    // moves the failure away from the line that caused it.
    match result {
        Some(Value::Object(Some(_))) => Ok(result),
        _ => Err(bare_npe()),
    }
}

pub(crate) fn native_opt_filter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // RULE F: `requireNonNull(predicate)` precedes the presence test.
    reject_null_functional(args.get(1))?;
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return native_opt_empty(ctx, &[]),
    };
    let predicate = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        // Missing invokedynamic predicate: keep the Optional unfiltered.
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    if opt_value_is_empty(val) {
        return native_opt_empty(ctx, &[]);
    }
    let result = ctx.invoke_virtual(predicate, "test", "(Ljava/lang/Object;)Z", &[val])?;
    let passed = matches!(result, Some(Value::Int(1)));
    if passed {
        // Return `this` (the same Optional)
        Ok(Some(Value::Object(Some(this))))
    } else {
        native_opt_empty(ctx, &[])
    }
}

pub(crate) fn native_opt_or_else_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            // null optional → call supplier
            let supplier = match args.get(1) {
                Some(Value::Object(Some(r))) => *r,
                _ => return Ok(Some(Value::Object(None))),
            };
            return ctx.invoke_virtual(supplier, "get", "()Ljava/lang/Object;", &[]);
        }
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    if opt_value_is_empty(val) {
        // `orElseGet` does NOT pre-validate: its body is
        // `return value != null ? value : supplier.get();`, so a null supplier
        // is an NPE on THIS branch only. MEASURED: `present.orElseGet(null)`
        // does not throw (apps/probes/PqOptionalShadowSweep 72-73).
        reject_null_functional(args.get(1))?;
        let supplier = match args.get(1) {
            Some(Value::Object(Some(r))) => *r,
            _ => return Ok(Some(Value::Object(None))),
        };
        ctx.invoke_virtual(supplier, "get", "()Ljava/lang/Object;", &[])
    } else {
        Ok(Some(val))
    }
}

pub(crate) fn native_opt_if_present_or_else(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    // Like `orElseGet`, `ifPresentOrElse` pre-validates neither argument: the
    // NPE comes from invoking whichever of the two the branch reaches, so
    // `present.ifPresentOrElse(action, null)` does not throw and
    // `empty.ifPresentOrElse(null, null)` does.
    if !opt_value_is_empty(val) {
        reject_null_functional(args.get(1))?;
        let consumer = match args.get(1) {
            Some(Value::Object(Some(r))) => *r,
            _ => return Ok(None),
        };
        ctx.invoke_virtual(consumer, "accept", "(Ljava/lang/Object;)V", &[val])?;
    } else {
        reject_null_functional(args.get(2))?;
        let runnable = match args.get(2) {
            Some(Value::Object(Some(r))) => *r,
            _ => return Ok(None),
        };
        ctx.invoke_virtual(runnable, "run", "()V", &[])?;
    }
    Ok(None)
}

pub(crate) fn native_opt_or(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // RULE F: `Optional.or` opens `Objects.requireNonNull(supplier)`, so it
    // throws even when the value IS present and the supplier is never needed.
    reject_null_functional(args.get(1))?;
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            // null optional → call supplier
            let supplier = match args.get(1) {
                Some(Value::Object(Some(r))) => *r,
                _ => return native_opt_empty(ctx, &[]),
            };
            return ctx.invoke_virtual(supplier, "get", "()Ljava/lang/Object;", &[]);
        }
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    if opt_value_is_empty(val) {
        let supplier = match args.get(1) {
            Some(Value::Object(Some(r))) => *r,
            _ => return native_opt_empty(ctx, &[]),
        };
        let produced = ctx.invoke_virtual(supplier, "get", "()Ljava/lang/Object;", &[])?;
        // `Optional.or` ends `return Objects.requireNonNull(r);`.
        match produced {
            Some(Value::Object(Some(_))) => Ok(produced),
            _ => Err(bare_npe()),
        }
    } else {
        Ok(Some(Value::Object(Some(this))))
    }
}

pub(crate) fn native_opt_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    if opt_value_is_empty(val) {
        make_stream(ctx, &[])
    } else {
        make_stream(ctx, &[val])
    }
}

pub(crate) fn native_opt_or_else_throw_supplier(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "No value present".to_string(),
                }
                .into(),
            );
        }
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    if opt_value_is_empty(val) {
        // Empty Optional → `throw exceptionSupplier.get()` (NOT the default
        // NoSuchElementException). The previous code ignored the supplier and
        // always threw "No value present", so callers relying on a custom
        // exception type/message saw the wrong throwable — e.g. Elasticsearch
        // `ProviderLocator` does `ServiceLoader.findFirst().orElseThrow(supplier)`
        // and surfaced a bare NoSuchElementException instead of the intended
        // provider-not-found error.
        match args.get(1) {
            Some(Value::Object(Some(supplier))) => {
                let produced = ctx.invoke_virtual(*supplier, "get", "()Ljava/lang/Object;", &[])?;
                match produced {
                    Some(Value::Object(Some(exc))) => Err(
                        cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc),
                    ),
                    // supplier returned null → `throw null` is an NPE.
                    _ => Err(cratonvm_types::error::RuntimeError::NullPointerException {
                        message: None,
                    }
                    .into()),
                }
            }
            // null supplier → invoking get() on it is an NPE.
            _ => Err(
                cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
            ),
        }
    } else {
        Ok(Some(val))
    }
}

// ===========================================================================
// Comparator-based sort
// ===========================================================================

/// `CRATONVM_COMPAT_AL_LISTITR_COMOD` (shared with the `ArrayList$ListItr`
/// natives in native-builtins) -- default ON, `=0` restores the old sorts,
/// which never touched `modCount`. JDK 25 `ArrayList.sort(c)` and
/// `Vector.sort(c)` are
///
/// ```text
///   final int expectedModCount = modCount;
///   Arrays.sort((E[]) elementData, 0, size, c);
///   if (modCount != expectedModCount) throw new ConcurrentModificationException();
///   modCount++;
/// ```
///
/// so even an already-sorted (or 0/1-element) list invalidates every live
/// iterator, and a comparator that structurally modifies the list fails the
/// sort. `Collections.sort(list[, c])` is `list.sort(c)`. Under `--compatible`
/// `Collections.sort` runs [`native_collections_sort`] /
/// [`sort_with_comparator`] directly, so `it.next(); Collections.sort(l);
/// it.next()` printed `ok` where HotSpot throws (`R16CmecompatSweep` c41,
/// round 16 wave 13, lane cmecompat).
pub(crate) fn al_sort_mod_count_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_AL_LISTITR_COMOD")
}

/// The `expectedModCount` of a native sort: `Some` only when the switch is on
/// and `list` is an `ArrayList` or `Vector` (whose `sort` is the body quoted
/// on [`al_sort_mod_count_enabled`]) with a usable `modCount` slot. Other
/// receivers that reach the array sort -- `Arrays.asList`, whose `sort` is a
/// bare `Arrays.sort`, the unmodifiable wrappers -- keep their old answer.
pub(crate) fn al_sort_mod_count_before(ctx: &dyn NativeContext, list: ObjectRef) -> Option<i32> {
    if !al_sort_mod_count_enabled() {
        return None;
    }
    if !obj_is_instance_of(ctx, list, "java/util/ArrayList")
        && !obj_is_instance_of(ctx, list, "java/util/Vector")
    {
        return None;
    }
    al_mod_count(ctx, list)
}

/// What the end of `ArrayList.sort` does with the count read before it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SortModCount {
    /// Not a receiver the protocol applies to.
    Skip,
    /// `modCount++`.
    Bump,
    /// The comparator changed the list: `ConcurrentModificationException`.
    Comodified,
}

pub(crate) fn sort_mod_count_outcome(before: Option<i32>, after: Option<i32>) -> SortModCount {
    match before {
        None => SortModCount::Skip,
        Some(b) if after == Some(b) => SortModCount::Bump,
        Some(_) => SortModCount::Comodified,
    }
}

/// The tail of `ArrayList.sort` after a successful native sort; `list` must
/// be current (re-read through its pin when the sort ran user code).
pub(crate) fn al_sort_mod_count_after(
    ctx: &mut dyn NativeContext,
    list: ObjectRef,
    before: Option<i32>,
) -> MethodCallResult {
    let after = before.and(al_mod_count(&*ctx, list));
    match sort_mod_count_outcome(before, after) {
        SortModCount::Skip => Ok(None),
        SortModCount::Bump => {
            al_bump_mod_count(ctx, list);
            Ok(None)
        }
        SortModCount::Comodified => {
            Err(cratonvm_types::error::RuntimeError::ConcurrentModificationException.into())
        }
    }
}

#[cfg(test)]
mod al_sort_mod_count_tests {
    use super::{sort_mod_count_outcome, SortModCount};

    #[test]
    fn an_unchanged_count_is_bumped_once() {
        // `ArrayList.sort` ends in `modCount++` even when nothing moved.
        assert_eq!(sort_mod_count_outcome(Some(7), Some(7)), SortModCount::Bump);
        assert_eq!(
            sort_mod_count_outcome(Some(i32::MAX), Some(i32::MAX)),
            SortModCount::Bump
        );
    }

    #[test]
    fn a_comparator_that_modified_the_list_is_a_cme() {
        assert_eq!(
            sort_mod_count_outcome(Some(7), Some(8)),
            SortModCount::Comodified
        );
        assert_eq!(
            sort_mod_count_outcome(Some(7), None),
            SortModCount::Comodified
        );
    }

    #[test]
    fn a_receiver_outside_the_protocol_is_left_alone() {
        assert_eq!(sort_mod_count_outcome(None, Some(3)), SortModCount::Skip);
        assert_eq!(sort_mod_count_outcome(None, None), SortModCount::Skip);
    }
}

pub(crate) fn native_al_sort_comparator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let comparator = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        Some(Value::Object(None)) | None => {
            // null comparator → natural ordering (delegate to existing sort)
            return list_sort_natural(ctx, &[Value::Object(Some(this))]);
        }
        _ => return Ok(None),
    };
    sort_with_comparator(ctx, this, comparator)
}

/// RULE S — the null argument of a `java.util.Collections` static.
///
/// Every one of these opens by dereferencing its argument (`c.size()`,
/// `list.get(0)`, `new ArrayList<>(list)`), so a null is an NPE before the
/// algorithm starts, and on an EMPTY argument as much as a populated one.
/// MEASURED, twelve rows, both modes (`apps/probes/CollectionsShadowSweep` 5, 8, 12,
/// 18, 22, 27, 34, 35, 47, 114, 147).
///
/// Same missing-vs-explicitly-null distinction as [`reject_null_functional`]:
/// a MISSING argument is a malformed native call and keeps the body's existing
/// defensive answer, while an explicitly passed null is a program-level error
/// the JDK reports.
#[inline]
pub(crate) fn reject_null_collections_arg(arg: Option<&Value>) -> Result<(), MethodCallFailed> {
    reject_null_functional(arg)
}

pub(crate) fn native_collections_sort_comparator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    reject_null_collections_arg(args.first())?;
    let list = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    // `list.sort(c)`: a program's override runs (wave 14, lane compat2).
    let comparator_arg = match args.get(1) {
        Some(v @ Value::Object(_)) => *v,
        _ => Value::Object(None),
    };
    if let Some(r) = collections_sort_override(ctx, args.first(), comparator_arg) {
        return r;
    }
    let comparator = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        Some(Value::Object(None)) | None => {
            // null comparator → natural ordering
            return list_sort_natural(ctx, &[Value::Object(Some(list))]);
        }
        _ => return Ok(None),
    };
    sort_with_comparator(ctx, list, comparator)
}

/// Stable merge sort (O(n log n)) using a Comparator lambda.
pub(crate) fn sort_with_comparator(
    ctx: &mut dyn NativeContext,
    list: ObjectRef,
    comparator: ObjectRef,
) -> MethodCallResult {
    let (data, size) = al_state(ctx, list);
    let data = match data {
        Some(d) => d,
        // Non-ArrayList list (LinkedList, CopyOnWriteArrayList, …): al_state
        // can't read its backing array. Collections.sort(list, c) is defined as
        // list.sort(c) in the JDK — delegate to the receiver's own sort
        // bytecode (sort(Comparator) is native only on ArrayList, so this runs
        // the real AbstractList.sort for everything else). Was a silent no-op
        // (kcfull #17b: keycloak LDAPMappersComparatorTest sorts a LinkedList).
        None => {
            return ctx.invoke_virtual(
                list,
                "sort",
                "(Ljava/util/Comparator;)V",
                &[Value::Object(Some(comparator))],
            )
        }
    };
    let len = size as usize;
    // `ArrayList.sort(c)` / `Vector.sort(c)`: see [`al_sort_mod_count_before`].
    let mod_count_before = al_sort_mod_count_before(&*ctx, list);
    if len <= 1 {
        return al_sort_mod_count_after(ctx, list, mod_count_before);
    }

    // Read all elements into a Vec.
    let mut elems: Vec<Value> = Vec::with_capacity(len);
    for i in 0..len {
        elems.push(ctx.get_array_element(data, i));
    }

    // Stable merge sort — O(n log n) — dispatching through the Comparator.
    // The comparison is fallible: a comparator that throws propagates the
    // error out and short-circuits the sort. A non-Int return is treated as
    // 0 ("equal"), preserving the previous insertion-sort semantics.
    //
    // GC-SAFETY (Family-1 stale-ObjectRef fix, follow-up to the TreeMap/TreeSet
    // comparator_compare fix): `merge_sort_fallible` dispatches the caller's
    // (possibly `thenComparing`-chained) Comparator via `comparator_compare`
    // up to O(n log n) times, and each dispatch can invoke a user lambda and
    // trigger a moving GC. `data` and `comparator` were already pinned/
    // refreshed here, but the `list` parameter itself was held raw across the
    // WHOLE sort and dereferenced only at the very end by
    // `values_view_source(ctx, list)` (which reads `list`'s class via
    // `al_state`/`unwrap_unmod`/`class_id_of_object`) -- pin it too. Confirmed
    // via a targeted repro (concurrent `ArrayList.sort(Comparator
    // .comparingInt(f).thenComparing(g))` under GC pressure) as the actual
    // root cause of the `Comparator.lambda$thenComparing$...` (class_id 6)
    // panics surviving the TreeMap/TreeSet fix.
    let list_pin = ctx.pin_native_root(list);
    let data_pin = ctx.pin_native_root(data);
    let cmp_pin = ctx.pin_native_root(comparator);
    let (_, elem_handles) = pin_value_slice(ctx, &elems);
    let mut idx: Vec<Value> = (0..elems.len() as i32).map(Value::Int).collect();
    let sort_result = merge_sort_fallible(ctx, &mut idx, |c, a, b| {
        let ia = if let Value::Int(v) = a {
            *v as usize
        } else {
            0
        };
        let ib = if let Value::Int(v) = b {
            *v as usize
        } else {
            0
        };
        let cmp = c.read_native_pin(cmp_pin, comparator);
        let ea = read_pinned_elem(c, elem_handles[ia], elems[ia]);
        let eb = read_pinned_elem(c, elem_handles[ib], elems[ib]);
        match comparator_compare(c, cmp, ea, eb)? {
            Some(Value::Int(v)) => Ok(v),
            _ => Ok(0),
        }
    });
    if let Err(e) = sort_result {
        ctx.unpin_native_roots(list_pin);
        return Err(e);
    }

    let data = ctx.read_native_pin(data_pin, data);
    for (out, slot) in idx.iter().enumerate() {
        let i = if let Value::Int(v) = slot {
            *v as usize
        } else {
            0
        };
        let val = read_pinned_elem(ctx, elem_handles[i], elems[i]);
        ctx.set_array_element(data, out, val);
    }
    let list = ctx.read_native_pin(list_pin, list);
    ctx.unpin_native_roots(list_pin);
    // Detach from `Map.values()` live-view semantics. Real `Map.values()`'s
    // return type doesn't implement `List` (no `.sort()` possible on it at
    // all on HotSpot) — reaching this native means whatever called
    // sort()/Collections.sort() already treated this object as a plain,
    // independent `List`, e.g. `DefaultListableBeanFactory
    // .resolveMultipleBeanCollection`'s `@Order`-based `List<T>` autowiring,
    // which builds the list from `matchingBeans.values()`. Left unfixed, the
    // very next `iterator()`/`toString()`-adjacent call sees the live-view
    // marker (`values_view_source`) still present and silently discards the
    // sort by resyncing from the ORIGINAL (unsorted) backing map — exactly
    // what broke `Spr11310Tests`/`Spr12636Tests`. Mirrors the same
    // null-the-marker fix already applied to `native_al_retain_all`.
    if values_view_source(ctx, list).is_some() {
        let dlen = ctx.array_length(data) as usize;
        if dlen > 0 {
            ctx.set_array_element(data, dlen - 1, Value::Object(None));
        }
    }
    al_sort_mod_count_after(ctx, list, mod_count_before)
}

// ===========================================================================
// Map.Entry accessors
// ===========================================================================

pub(crate) fn register_map_entry_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/util/HashMap$Entry";
    r.register(c, "getKey", "()Ljava/lang/Object;", native_entry_get_key);
    r.register(
        c,
        "getValue",
        "()Ljava/lang/Object;",
        native_entry_get_value,
    );
    r.register(
        c,
        "setValue",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_entry_set_value,
    );
    r.register(c, "hashCode", "()I", native_entry_hash_code);
    r.register(c, "equals", "(Ljava/lang/Object;)Z", native_entry_equals);
    // The real per-family entry classes, and ONLY `setValue`.
    //
    // That restraint is the point. A live entry holds `key` and `value` in the
    // slots the class DECLARES, so java.base's own `getKey`, `getValue`,
    // `equals`, `hashCode` and `toString` bytecode reads exactly the right
    // fields -- better than any native this file could register, and MEASURED
    // identical to HotSpot on all four. `setValue` is the single method whose
    // JDK body is insufficient: it writes the field and returns, where a live
    // entry must also write THROUGH to the source map, or
    // `entrySet().iterator()...setValue(v)` is silently a no-op.
    //
    // Paired with an entry in BOTH force-native gates. A registration with no
    // gate entry loses to the receiver's own bytecode and is as silent as a
    // gate entry with no registration -- this lane has now been bitten by each
    // direction once.
    for c in LIVE_ENTRY_CLASSES {
        // The `JdkConcurrentHashMap` row (wave 76, L1): a real `$MapEntry`
        // (the JDK's `EntryIterator` mints it) writes through its own body.
        if c.starts_with(CHM_CLASS) && chm_row_registers_nothing(r) {
            continue;
        }
        r.register(
            c,
            "setValue",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            native_entry_set_value,
        );
    }
    r.set_category(__prev_cat);
}

/// Slot index of the optional back-reference to the source map carried by a
/// *live* `entrySet()` entry. When present (field 2 is the source map),
/// `setValue` writes through to that map — the JDK contract for
/// `map.entrySet().iterator().next().setValue(v)`. Detached entries (a
/// standalone `AbstractMap.SimpleEntry`, `firstEntry()`/`lastEntry()`
/// snapshots, unmodifiable views) are allocated with only 2 fields, so a
/// fail-safe out-of-range read of slot 2 returns `Object(None)` and no
/// write-through happens — matching their non-live semantics.
pub(crate) const ENTRY_FIELD_SOURCE: usize = 2;

/// The real `Map.Entry` implementation HotSpot hands out for each map family's
/// `entrySet()` elements.
///
/// MEASURED against HotSpot 25.0.3+9 -- every one of these was
/// `java.util.AbstractMap$SimpleEntry` here, a real `Map.Entry` but the wrong
/// one for every family that has its own:
///
/// ```text
///   HashMap             java.util.HashMap$Node
///   LinkedHashMap       java.util.LinkedHashMap$Entry
///   TreeMap             java.util.TreeMap$Entry
///   ConcurrentHashMap   java.util.concurrent.ConcurrentHashMap$MapEntry
///   Hashtable           java.util.Hashtable$Entry
/// ```
pub(crate) const LIVE_ENTRY_CLASSES: &[&str] = &[
    "java/util/HashMap$Node",
    "java/util/LinkedHashMap$Entry",
    "java/util/TreeMap$Entry",
    "java/util/concurrent/ConcurrentHashMap$MapEntry",
    "java/util/Hashtable$Entry",
];

/// Which of [`LIVE_ENTRY_CLASSES`] a `entrySet()` element of `source` should be,
/// or `None` for a receiver with no family entry class of its own (the caller
/// keeps whatever carrier it was going to use).
pub(crate) fn live_entry_class_for(ctx: &dyn NativeContext, source: ObjectRef) -> Option<&'static str> {
    // Order mirrors `collect_entries_any`: `LinkedHashMap` is a `HashMap`
    // subclass, so it has to be asked first.
    if is_lhm_receiver(ctx, source) {
        Some("java/util/LinkedHashMap$Entry")
    } else if is_chm_receiver(ctx, source) {
        Some("java/util/concurrent/ConcurrentHashMap$MapEntry")
    } else if is_hashtable_receiver(ctx, source) {
        Some("java/util/Hashtable$Entry")
    } else if is_tree_map_receiver(ctx, source) {
        Some(TM_ENTRY_CLASS)
    } else {
        // The HashMap family is the fallthrough: the four asked above are the
        // only java.util maps with an entry class of their own, and anything
        // else reaching here is not bucket-backed at all, so it keeps the
        // caller's generic carrier.
        match ctx
            .class_name_arc_of_id(ctx.class_id_of_object(source))
            .as_deref()
        {
            Some("java/util/HashMap") => Some("java/util/HashMap$Node"),
            _ => None,
        }
    }
}

/// `(key_slot, value_slot, source_slot)` for a live entry of ANY shape.
///
/// # Why this cannot be `(0, 1, 2)`
///
/// The synthetic 3-field carrier is `key@0 value@1 source@2`, and the file
/// assumed that everywhere. NOT ONE of the real classes agrees:
///
/// ```text
///   HashMap$Node          hash@0  key@1  value@2  next@3
///   LinkedHashMap$Entry   hash@0  key@1  value@2  next@3  before@4  after@5
///   Hashtable$Entry       hash@0  key@1  value@2  next@3
///   TreeMap$Entry         key@0   value@1  left@2  right@3  parent@4  color@5
///   CHM$MapEntry          key@0   val@1    map@2
/// ```
///
/// Two traps in that table. `TreeMap$Entry` slot 2 is `left`, so the old
/// `ENTRY_FIELD_SOURCE` would have written a MAP where real bytecode expects an
/// `Entry` -- which is why `WORKER-2-NOTE-1` §5b declined this class. And the
/// value field is spelled `val`, not `value`, on `CHM$MapEntry`.
///
/// So the slots are resolved BY NAME, and `source` goes in a trailing
/// undeclared slot the object is allocated one wider to hold.
///
/// `source_slot` is `None` for a real entry class at its DECLARED width -- a
/// node of the red-black tree [`tm_publish_real_root`] builds, or a table node,
/// neither of which has a source and neither of which may have one invented at
/// a declared field's expense.
pub(crate) fn entry_slots(ctx: &dyn NativeContext, entry: ObjectRef) -> (usize, usize, Option<usize>) {
    let cid = ctx.class_id_of_object(entry);
    let w = ctx.object_num_fields(entry);
    if let Some(name) = ctx.class_name_arc_of_id(cid) {
        // A `HashMap$TreeNode` is a `HashMap$Node` (it inherits the
        // registered `setValue`) with `hash` at slot 0 too: the default
        // `(0, 1, 2)` below would write its KEY slot and read its value as a
        // source map (`CRATONVM_COMPAT_HASHMAP_TREE_BINS`).
        if LIVE_ENTRY_CLASSES.contains(&&*name)
            || (hm_tree_bins_enabled() && &*name == HM_TREE_NODE_CLASS)
        {
            let k = ctx.resolve_field_index_by_class_id(cid, "key").unwrap_or(0);
            let v = ctx
                .resolve_field_index_by_class_id(cid, "value")
                .or_else(|| ctx.resolve_field_index_by_class_id(cid, "val"))
                .unwrap_or(1);
            let declared = ctx.class_num_total_fields(cid);
            let s = if w > declared { Some(w - 1) } else { None };
            return (k, v, s);
        }
    }
    (
        0,
        1,
        if w > ENTRY_FIELD_SOURCE {
            Some(ENTRY_FIELD_SOURCE)
        } else {
            None
        },
    )
}

/// Allocate a live `entrySet()` entry: 3 fields = (key, value, source-map).
/// `setValue` on it writes through to `source` (see [`ENTRY_FIELD_SOURCE`]).
pub(crate) fn alloc_live_entry(
    ctx: &mut dyn NativeContext,
    class: &str,
    key: Value,
    value: Value,
    source: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    // Entry allocation can trigger a moving collection. The caller often
    // obtained these values from a source-map snapshot held only in Rust, so
    // keeping them as bare ObjectRefs here can write pre-move addresses into
    // the freshly allocated entry. That manifests much later as a dropped
    // entry while Java walks entrySet() (for example, a sporadically missing
    // HTTP response header). Root all three inputs and reload after the
    // allocation before publishing the entry fields.
    let key_pin = pin_value(ctx, key);
    let value_pin = pin_value(ctx, value);
    let source_pin = ctx.pin_native_root(source);
    // Upgrade the caller's carrier to the source map family's OWN entry class
    // when this image has it. Every caller passes a generic fallback
    // (`AbstractMap$SimpleEntry` / `Map$Entry`), which stays the landing for a
    // family with no class of its own and for a stripped image -- a stand-in is
    // refused rather than fabricated, since minting a fake `HashMap$Node` is
    // exactly what `--jdk-only` exists to stop.
    //
    // Allocated ONE SLOT WIDER than the class declares: `source` lives in that
    // trailing undeclared slot, because on four of the five real classes the
    // declared slot 2 is something else entirely (`next`, `left`, `map`). See
    // [`entry_slots`].
    let real = live_entry_class_for(&*ctx, source)
        .filter(|c| !ctx.would_fabricate_synthetic_stub(c) && !ctx.is_class_synthetic_stub(c));
    let entry: ObjectRef =
        match real.and_then(|c| ctx.ensure_class_initialized(c).ok().map(|id| (c, id))) {
            Some((c, cid)) if ctx.class_name_arc_of_id(cid).as_deref() == Some(c) => {
                let n = ctx.class_num_total_fields(cid) + 1;
                ctx.alloc_object(cid, n)
            }
            // Not `?`: a refusal must release the three pins (the lowest of them
            // is this frame's base) rather than strand them (gc-common w20-a).
            _ => match try_alloc_synthetic(ctx, class, 3) {
                Ok(entry) => entry,
                Err(e) => {
                    ctx.unpin_native_roots(key_pin.min(value_pin).min(source_pin));
                    return Err(e);
                }
            },
        };
    let key = read_pinned_elem(ctx, key_pin, key);
    let value = read_pinned_elem(ctx, value_pin, value);
    let source = ctx.read_native_pin(source_pin, source);
    let (k_slot, v_slot, s_slot) = entry_slots(&*ctx, entry);
    ctx.set_field(entry, k_slot, key);
    ctx.set_field(entry, v_slot, value);
    if let Some(s) = s_slot {
        ctx.set_field(entry, s, Value::Object(Some(source)));
    }
    ctx.unpin_native_roots(source_pin);
    ctx.unpin_native_roots(value_pin);
    ctx.unpin_native_roots(key_pin);
    Ok(entry)
}

pub(crate) fn native_entry_get_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field(this, 0)))
}

pub(crate) fn native_entry_get_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field(this, 1)))
}

/// `CRATONVM_COMPAT_ENTRY_SET_VALUE_IN_PLACE` — default ON, `=0` restores
/// the write-through `put`. On the JDK a `HashMap` / `LinkedHashMap`
/// `entrySet()` element IS the map's node, and `Node.setValue` is `V old =
/// value; value = newValue; return old;`: no `equals`, no
/// `afterNodeAccess` (an access-ordered map does NOT reorder), and a node
/// whose key was removed from the map in the meantime changes nothing. The
/// live-entry COPY this crate hands out wrote through with `put(key, v)`:
/// an `equals` walk, a move to the tail of an access-ordered map, and a
/// removed key put BACK. Round 13 wave 9, lane hashcompat4.
pub(crate) fn entry_set_value_in_place_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_ENTRY_SET_VALUE_IN_PLACE")
}

/// The write-through of a live entry's `setValue` as `Node.setValue` does it
/// (see [`entry_set_value_in_place_enabled`]): find the source map's node that
/// holds this very key object -- its chain found by the key's hash, compared
/// by IDENTITY, so no `equals` -- and write the value into it. No node (the
/// key was removed) writes nothing. `None`, before any Java has run, for a
/// source that is not a plain `HashMap` / `LinkedHashMap` on the node layout
/// (the integer overlay, `Hashtable`, CHM, `TreeMap`, ...), which keeps the
/// `put`. Each `(pin, value)` pair is the caller's pin and the value read from
/// it; the caller releases them.
///
/// One `hashCode()` remains where the JDK makes none: the copy does not
/// carry its node's stored hash. A key whose hash changed since it was
/// inserted is not found, and its node is not written.
pub(crate) fn entry_set_value_in_place(
    ctx: &mut dyn NativeContext,
    src: (usize, ObjectRef),
    key: (usize, Value),
    value: (usize, Value),
) -> Option<Result<(), MethodCallFailed>> {
    if !entry_set_value_in_place_enabled() {
        return None;
    }
    let facts = receiver_facts(&*ctx, src.1);
    let lhm = facts.has(CF_LHM);
    if !lhm
        && (!facts.has(CF_BUCKET_MAP_NAME)
            || facts.has(CF_CHM)
            || facts.has(CF_TREE_MAP)
            || facts.has(CF_IDENTITY_MAP)
            || facts.has(CF_UNMOD_WRAPPER)
            || facts.has(CF_HASHTABLE_ANCESTRY)
            || hm_int_fast_len(&*ctx, src.1).is_some())
    {
        return None;
    }
    let hash = match key.1 {
        Value::Object(Some(k)) => match map_hash_key(ctx, k) {
            Ok(h) => h,
            Err(e) => return Some(Err(e)),
        },
        Value::Object(None) => 0,
        _ => return None,
    };
    // `hashCode()` is Java: re-read all three.
    let map = ctx.read_native_pin(src.0, src.1);
    let key_now = read_pinned_elem(ctx, key.0, key.1);
    let value_now = read_pinned_elem(ctx, value.0, value.1);
    let (buckets, size, cap) = if lhm {
        lhm_state(&*ctx, map)
    } else {
        map_state(&*ctx, map)
    };
    let Some(buckets) = buckets else {
        return Some(Ok(()));
    };
    // `LHM_NODE_NEXT` and `NODE_FIELD_NEXT` are the same slot (`next@3`), but
    // each layout is read through its own names.
    let mut cur = ctx.get_array_element(buckets, map_bucket_index(hash, cap));
    for _ in 0..chain_walk_limit(size) {
        let Value::Object(Some(node)) = cur else {
            break;
        };
        let stored = if lhm {
            ctx.get_field(node, LHM_NODE_KEY)
        } else {
            get_node_key(&*ctx, node)
        };
        if value_same_ref(stored, key_now) {
            if lhm {
                ctx.set_field(node, LHM_NODE_VALUE, value_now);
            } else {
                set_node_value(ctx, node, value_now);
            }
            break;
        }
        cur = ctx.get_field(node, if lhm { LHM_NODE_NEXT } else { NODE_FIELD_NEXT });
    }
    Some(Ok(()))
}

pub(crate) fn native_entry_set_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let new_val = args.get(1).copied().unwrap_or(Value::Object(None));
    // Resolved, not hard-coded: `CHM$MapEntry` spells it `val`, and the
    // `HashMap`/`Hashtable` node families put it at slot 2 behind `hash`.
    let (_, v_slot, _) = entry_slots(&*ctx, this);
    let old_val = ctx.get_field(this, v_slot);
    ctx.set_field(this, v_slot, new_val);
    // Live-entry write-through: a `Map$Entry` minted by `native_map_entry_set`
    // carries its source map at slot 2 (3-field layout). `Map.Entry.setValue`
    // must mutate the backing map, not just the detached entry copy — otherwise
    // `entrySet().iterator()...setValue(v)` is silently a no-op (keycloak
    // StripSecretsUtils masking, and any read-modify-write over a map). 2-field
    // entries from other paths (TreeMap, SimpleEntry, ...) have no slot 2 and
    // keep the detached-update behaviour.
    let (k_slot, _, s_slot) = entry_slots(&*ctx, this);
    if let Some(src_slot) = s_slot {
        if let Value::Object(Some(src_map)) = ctx.get_field(this, src_slot) {
            let key = ctx.get_field(this, k_slot);
            // GC-safety: the write-through below dispatches arbitrary Java
            // (`put`, the key's `hashCode`/`equals`, a possible resize). Root the
            // source map and both values so the call receives current addresses.
            let src_pin = ctx.pin_native_root(src_map);
            let key_pin = pin_value(ctx, key);
            let val_pin = pin_value(ctx, new_val);
            let src_map = ctx.read_native_pin(src_pin, src_map);
            let key = read_pinned_elem(ctx, key_pin, key);
            let new_val = read_pinned_elem(ctx, val_pin, new_val);
            let in_place = entry_set_value_in_place(
                ctx,
                (src_pin, src_map),
                (key_pin, key),
                (val_pin, new_val),
            );
            let wrote = if let Some(done) = in_place {
                done
            } else if is_hashtable_receiver(ctx, src_map) {
                // Hashtable/Properties store their entries outside the HashMap
                // bucket array `native_map_put` writes to (Properties keeps them
                // in a Rust side-table). Dispatch through the receiver's virtual
                // `put` so the write reaches the real backing store — otherwise
                // `props.entrySet().iterator()...setValue(v)` (Hibernate's
                // `ConfigurationHelper.resolvePlaceHolders`) would silently no-op.
                ctx.invoke_virtual(
                    src_map,
                    "put",
                    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                    &[key, new_val],
                )
                .map(|_| ())
            } else {
                native_map_put(ctx, &[Value::Object(Some(src_map)), key, new_val]).map(|_| ())
            };
            ctx.unpin_native_roots(src_pin);
            wrote?;
        }
    }
    Ok(Some(old_val))
}

/// `Map.Entry.toString()` — `key + "=" + value`, which is what
/// `AbstractMap.SimpleEntry`, `HashMap$Node` and `TreeMap$Entry` all render.
pub(crate) fn native_entry_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // GC-SAFETY: both accessors and both `String.valueOf` dispatches re-enter
    // Java, so the receiver and the extracted key are pinned across them.
    let this_pin = ctx.pin_native_root(this);
    let rendered = (|| -> Result<Vec<u16>, MethodCallFailed> {
        let this = ctx.read_native_pin(this_pin, this);
        let key = ctx
            .invoke_virtual(this, "getKey", "()Ljava/lang/Object;", &[])?
            .unwrap_or(Value::Object(None));
        let key_pin = pin_value(ctx, key);
        let this = ctx.read_native_pin(this_pin, this);
        let value = ctx
            .invoke_virtual(this, "getValue", "()Ljava/lang/Object;", &[])?
            .unwrap_or(Value::Object(None));
        let value_pin = pin_value(ctx, value);
        let key = read_pinned_elem(ctx, key_pin, key);
        let mut out = obj_to_display_units(ctx, &key)?;
        out.push(b'=' as u16);
        let value = read_pinned_elem(ctx, value_pin, value);
        out.extend_from_slice(&obj_to_display_units(ctx, &value)?);
        Ok(out)
    })();
    ctx.unpin_native_roots(this_pin);
    let units = rendered?;
    Ok(Some(Value::Object(Some(
        ctx.create_string_from_units(&units),
    ))))
}

pub(crate) fn native_entry_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    // GC-safety: every dispatch here runs arbitrary Java. `this` and the two
    // extracted values are bare Rust locals reused after the next call.
    let this_pin = ctx.pin_native_root(this);
    let hashes = (|| -> Result<i32, MethodCallFailed> {
        let key = ctx
            .invoke_virtual(this, "getKey", "()Ljava/lang/Object;", &[])?
            .unwrap_or(Value::Object(None));
        let key_pin = pin_value(ctx, key);
        let this = ctx.read_native_pin(this_pin, this);
        let value = ctx
            .invoke_virtual(this, "getValue", "()Ljava/lang/Object;", &[])?
            .unwrap_or(Value::Object(None));
        let value_pin = pin_value(ctx, value);
        let key = read_pinned_elem(ctx, key_pin, key);
        let kh = element_hash_code(ctx, &key)?;
        let value = read_pinned_elem(ctx, value_pin, value);
        Ok(kh ^ element_hash_code(ctx, &value)?)
    })();
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Int(hashes?)))
}

pub(crate) fn native_entry_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    if !obj_is_instance_of(ctx, other, "java/util/Map$Entry") {
        return Ok(Some(Value::Int(0)));
    }

    // GC-safety: four Java dispatches plus two `equals` comparisons, with the
    // receivers and every extracted value living in bare Rust locals across
    // them. Root the two receivers and each value as it is produced.
    let eq_base = ctx.pin_native_root(this);
    let other_pin = ctx.pin_native_root(other);
    let verdict = (|| -> Result<bool, MethodCallFailed> {
        let this_key = ctx
            .invoke_virtual(this, "getKey", "()Ljava/lang/Object;", &[])?
            .unwrap_or(Value::Object(None));
        let this_key_pin = pin_value(ctx, this_key);
        let this = ctx.read_native_pin(eq_base, this);
        let this_value = ctx
            .invoke_virtual(this, "getValue", "()Ljava/lang/Object;", &[])?
            .unwrap_or(Value::Object(None));
        let this_value_pin = pin_value(ctx, this_value);
        let other = ctx.read_native_pin(other_pin, other);
        let other_key = ctx
            .invoke_virtual(other, "getKey", "()Ljava/lang/Object;", &[])?
            .unwrap_or(Value::Object(None));
        let other_key_pin = pin_value(ctx, other_key);
        let this_key = read_pinned_elem(ctx, this_key_pin, this_key);
        let other_key = read_pinned_elem(ctx, other_key_pin, other_key);
        if !values_equal_deep(ctx, &this_key, &other_key)? {
            return Ok(false);
        }
        let other = ctx.read_native_pin(other_pin, other);
        let other_value = ctx
            .invoke_virtual(other, "getValue", "()Ljava/lang/Object;", &[])?
            .unwrap_or(Value::Object(None));
        let other_value_pin = pin_value(ctx, other_value);
        let this_value = read_pinned_elem(ctx, this_value_pin, this_value);
        let other_value = read_pinned_elem(ctx, other_value_pin, other_value);
        values_equal_deep(ctx, &this_value, &other_value)
    })();
    ctx.unpin_native_roots(eq_base);
    Ok(Some(Value::Int(if verdict? { 1 } else { 0 })))
}

/// True when `this`'s runtime class is `java/util/Hashtable` or a subclass
/// (notably `java/util/Properties`). Such maps hold their entries outside the
/// HashMap bucket array CratonVM models natively — `Properties` uses a Rust
/// side-table — so live-entry `setValue` write-through and other generic
/// mutations must dispatch through the receiver's own virtual method rather
/// than the direct bucket-level `native_map_*`.
pub(crate) fn is_hashtable_receiver(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let mut cur = ctx.class_id_of_object(this);
    for _ in 0..32 {
        match ctx.class_name_of_id(cur) {
            Some(n) if n == "java/util/Hashtable" || n == "java/util/Properties" => return true,
            Some(n) if n == "java/util/HashMap" || n == "java/lang/Object" => return false,
            _ => {}
        }
        match ctx.superclass_of(cur) {
            Some(p) if p != cur => cur = p,
            _ => return false,
        }
    }
    false
}

/// True for `Hashtable` and its ordinary subclasses. `Properties` is excluded:
/// JDK 25 backs it with a separate `ConcurrentHashMap` and is handled by the
/// properties side-table path rather than the native Hashtable bucket layout.
///
/// PERF (collections-classification-cost): memoized per `ClassId` — see
/// `CF_HASHTABLE_LAYOUT`, which keeps the `Properties` exclusion.
#[inline]
pub(crate) fn uses_native_hashtable_layout(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    receiver_facts(ctx, this).has(CF_HASHTABLE_LAYOUT)
}

// ===========================================================================
// ArrayList.removeIf / replaceAll
// ===========================================================================

pub(crate) fn native_al_remove_if(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    // RULE F. MEASURED on `ArrayList`, `Vector`, `Stack`, `LinkedList`,
    // `HashSet`, `TreeSet`, `ArrayDeque` and the map views, empty or not.
    // NOTE this body serves MUTABLE receivers only — an immutable one answers
    // `UnsupportedOperationException` instead, and is not registered here.
    // See `reject_null_functional`.
    reject_null_functional(args.get(1))?;
    let predicate = match args.get(1) {
        Some(Value::Object(Some(p))) => *p,
        _ => return Ok(Some(Value::Int(0))),
    };

    let (al_data_slot, al_size_slot, _) = al_slots_for(ctx, this);
    let data = match ctx.get_field(this, al_data_slot) {
        Value::Object(Some(arr)) => arr,
        _ => return Ok(Some(Value::Int(0))),
    };
    let size = match ctx.get_field(this, al_size_slot) {
        Value::Int(s) => s as usize,
        _ => return Ok(Some(Value::Int(0))),
    };

    // Live map view: removed elements must also be deleted from the source map.
    let view_src = values_view_source(ctx, this);

    // `CRATONVM_COMPAT_VIEW_REMOVE_BY_NODE`: `HashMap.Values` inherits
    // `Collection.removeIf`, which walks a `ValueIterator` over the LIVE
    // nodes and removes each condemned one as `removeNode(p.hash, ..)`: no
    // `hashCode()`, and exactly that node -- where the element body tested
    // this view's snapshot and removed "the first entry whose value equals"
    // by key (a `hashCode()` per removal, and possibly another key's entry).
    // No Java has run in this native yet.
    if let Some(source) = view_src {
        if view_remove_by_node_enabled()
            && plain_node_map(&*ctx, source)
            && !al_view_holds_entries(&*ctx, this)
        {
            if let Some((mut nodes, lhm)) = hs_backing_nodes(&*ctx, source) {
                // A reversed `LinkedValues` (C4-1): the predicate runs tail first.
                if values_view_reversed(&*ctx, this) {
                    nodes.reverse();
                }
                return remove_if_over_nodes(
                    ctx,
                    this,
                    source,
                    nodes,
                    lhm,
                    predicate,
                    NodeElem::Value,
                );
            }
        }
    }

    let this_pin = ctx.pin_native_root(this);
    let data_pin = ctx.pin_native_root(data);
    let pred_pin = ctx.pin_native_root(predicate);
    let source_pin = view_src.map(|source| ctx.pin_native_root(source));

    // Snapshot elements, then test each with the predicate.
    let elements: Vec<Value> = (0..size).map(|i| ctx.get_array_element(data, i)).collect();
    let (_, elem_handles) = pin_value_slice(ctx, &elements);
    let mut keep_idx = Vec::with_capacity(size);
    let mut dropped_idx = Vec::new();
    for i in 0..size {
        let predicate = ctx.read_native_pin(pred_pin, predicate);
        let elem = read_pinned_elem(ctx, elem_handles[i], elements[i]);
        let result = match ctx.invoke_virtual(predicate, "test", "(Ljava/lang/Object;)Z", &[elem]) {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };
        let is_true = matches!(result, Some(Value::Int(v)) if v != 0);
        if is_true {
            dropped_idx.push(i);
        } else {
            keep_idx.push(i);
        }
    }

    let removed = keep_idx.len() < size;

    // Write back kept elements.
    let data = ctx.read_native_pin(data_pin, data);
    for (i, &elem_idx) in keep_idx.iter().enumerate() {
        let val = read_pinned_elem(ctx, elem_handles[elem_idx], elements[elem_idx]);
        ctx.set_array_element(data, i, val);
    }
    // Clear trailing slots (the `values()`-view source marker, if any, lives
    // beyond `size` and is therefore untouched).
    for i in keep_idx.len()..size {
        ctx.set_array_element(data, i, Value::Object(None));
    }
    let this = ctx.read_native_pin(this_pin, this);
    al_set_size(ctx, this, keep_idx.len() as i32);

    if let Some(source) = view_src {
        for &elem_idx in &dropped_idx {
            let source = match source_pin {
                Some(pin) => ctx.read_native_pin(pin, source),
                None => source,
            };
            let elem = read_pinned_elem(ctx, elem_handles[elem_idx], elements[elem_idx]);
            if let Err(e) = propagate_list_removal(ctx, source, elem) {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        }
    }

    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Int(if removed { 1 } else { 0 })))
}

pub(crate) fn native_al_replace_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // RULE F, empty receiver included. `replaceAll` takes a `UnaryOperator`;
    // `sort` takes a `Comparator` and a null one is NOT an error there — see
    // `reject_null_functional`'s third bullet and `native_al_sort_comparator`.
    reject_null_functional(args.get(1))?;
    let operator = match args.get(1) {
        Some(Value::Object(Some(op))) => *op,
        _ => return Ok(None),
    };

    let (al_data_slot, al_size_slot, _) = al_slots_for(ctx, this);
    let data = match ctx.get_field(this, al_data_slot) {
        Value::Object(Some(arr)) => arr,
        _ => return Ok(None),
    };
    let size = match ctx.get_field(this, al_size_slot) {
        Value::Int(s) => s as usize,
        _ => return Ok(None),
    };

    let data_pin = ctx.pin_native_root(data);
    let op_pin = ctx.pin_native_root(operator);
    for i in 0..size {
        let data = ctx.read_native_pin(data_pin, data);
        let elem = ctx.get_array_element(data, i);
        let operator = ctx.read_native_pin(op_pin, operator);
        let result = match ctx.invoke_virtual(
            operator,
            "apply",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[elem],
        ) {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(data_pin);
                return Err(e);
            }
        };
        // Store the boxed result as-is. Collections hold Object references;
        // unboxing to a raw primitive here corrupts array storage (the value
        // reads back as null). Matches the JDK and the TreeMap natives.
        let new_val = result.unwrap_or(Value::Object(None));
        let data = ctx.read_native_pin(data_pin, data);
        ctx.set_array_element(data, i, new_val);
    }

    ctx.unpin_native_roots(data_pin);
    Ok(None)
}

// ===========================================================================
// HashMap functional methods (computeIfAbsent, compute, merge, replaceAll)
// ===========================================================================

pub(crate) fn native_map_compute_if_absent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // RULE F, ahead of the key: MEASURED `computeIfAbsent(null, null)` is
    // message-less on all six map receivers. See `reject_null_functional`.
    reject_null_functional(args.get(2))?;
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let function = match args.get(2) {
        Some(Value::Object(Some(f))) => *f,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `CRATONVM_COMPAT_MAP_COMPUTE_JDK`: see `map_compute_jdk_enabled`.
    if let Some(route) = map_remap_route(&*ctx, this, key) {
        let null = Value::Object(None);
        return map_remap_jdk(ctx, route, this, key, null, function, RemapOp::IfAbsent);
    }

    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let function_pin = ctx.pin_native_root(function);

    // Check if key already present.
    let this_cur = ctx.read_native_pin(this_pin, this);
    let key_cur = read_pinned_elem(ctx, key_pin, key);
    let existing = match native_map_get(ctx, &[Value::Object(Some(this_cur)), key_cur]) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    if let Some(Value::Object(Some(_))) = existing {
        ctx.unpin_native_roots(this_pin);
        return Ok(existing);
    }

    // Key absent — call function.apply(key).
    //
    // MOD-COUNT SNAPSHOT across the callback. `HashMap.computeIfAbsent` is
    // specified to throw `ConcurrentModificationException` if the mapping
    // function modifies THIS map, and the JDK enforces it by comparing
    // `modCount` either side of the call. MEASURED in both modes
    // (`probes/HashMapShadowSweep.java`):
    //
    //   cme.computeIfAbsent("y", x -> { cme.put("z", 9); return 2; })
    //     HotSpot   ConcurrentModificationException
    //     CratonVM  no-throw
    //
    // Why it is not merely a missing exception: the native decided the key was
    // absent BEFORE calling the mapper, and then writes its result afterwards.
    // If the mapper resized or rewrote the table in between, that write lands
    // against a decision taken on a table that no longer exists. The CME is
    // what stops a caller from silently corrupting its own map, and it is the
    // reason the JDK's own body re-checks rather than trusting the earlier
    // lookup.
    //
    // The real `modCount` is used rather than a size comparison: an add paired
    // with a remove leaves the size unchanged and is still a structural
    // modification. `read_map_mod_count` answers `None` for a receiver with no
    // real-JDK layout slot, and the check is then skipped rather than guessed.
    let mod_before = read_map_mod_count(&*ctx, ctx.read_native_pin(this_pin, this));
    let function_cur = ctx.read_native_pin(function_pin, function);
    let key_cur = read_pinned_elem(ctx, key_pin, key);
    let result = match ctx.invoke_virtual(
        function_cur,
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
    if let Some(before) = mod_before {
        let after = read_map_mod_count(&*ctx, ctx.read_native_pin(this_pin, this));
        if after.is_some_and(|a| a != before) {
            ctx.unpin_native_roots(this_pin);
            return Err(RuntimeError::ConcurrentModificationException.into());
        }
    }
    // Keep the result boxed — maps store Object references; unboxing here makes
    // the value read back as null (TreeMap's native_tm_compute_if_absent stores
    // it boxed and is correct). The boxed wrapper is what the JDK stores/returns.
    let new_val = result.unwrap_or(Value::Object(None));

    if let Value::Object(None) = new_val {
        ctx.unpin_native_roots(this_pin);
        return Ok(Some(Value::Object(None)));
    }

    let new_val_pin = pin_value(ctx, new_val);
    let this_cur = ctx.read_native_pin(this_pin, this);
    let key_cur = read_pinned_elem(ctx, key_pin, key);
    let new_val_cur = read_pinned_elem(ctx, new_val_pin, new_val);
    if let Err(e) = map_remap_composite_put(ctx, this_cur, key_cur, new_val_cur) {
        ctx.unpin_native_roots(this_pin);
        return Err(e);
    }
    let ret = read_pinned_elem(ctx, new_val_pin, new_val);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(ret))
}

/// `CRATONVM_COMPAT_MAP_REMAP_CME` — default ON, `=0` restores the old
/// behaviour. `HashMap.compute`, `computeIfPresent` and `merge` (the last only
/// when it calls its function) snapshot `modCount` around the remapping
/// function and throw `ConcurrentModificationException` when the function
/// structurally modified the map (`int mc = modCount; v = f.apply(..); if (mc
/// != modCount) throw ..`), exactly as `native_map_compute_if_absent` already
/// does for `computeIfAbsent`. The natives called the function and then wrote
/// its result over a decision taken on the old table. Round 13 wave 8, lane
/// hashcompat3.
pub(crate) fn map_remap_cme_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_MAP_REMAP_CME")
}

/// The `modCount` a remapping function must leave unchanged (see
/// [`map_remap_cme_enabled`]). `None`, and so no check, with the switch off,
/// for a `TreeMap` receiver (a native side table whose `modCount` this layer
/// does not keep JDK-exact), and for a receiver with no `modCount` slot (a
/// `ConcurrentHashMap` segment: CHM has no such check).
pub(crate) fn map_remap_mod_count(ctx: &dyn NativeContext, this: ObjectRef) -> Option<i32> {
    if !map_remap_cme_enabled() || is_tree_map_receiver(ctx, this) {
        return None;
    }
    read_map_mod_count(ctx, this)
}

/// The `modCount` `HashMap.forEach` / `LinkedHashMap.forEach` compare after
/// their loop (`if (modCount != mc) throw new ConcurrentModificationException()`),
/// under the same switch as [`map_remap_mod_count`]. Only those two families:
/// `Hashtable` and `IdentityHashMap` check inside their loops, `Properties`
/// and `ConcurrentHashMap` never throw, and a foreign map reaching the
/// `Map.forEach` interface native keeps its old behaviour.
pub(crate) fn map_for_each_mod_count(ctx: &dyn NativeContext, this: ObjectRef) -> Option<i32> {
    let facts = receiver_facts(ctx, this);
    let hash_map_family = facts.has(CF_LHM)
        || (facts.has(CF_BUCKET_MAP_NAME)
            && !facts.has(CF_HASHTABLE_ANCESTRY)
            && !facts.has(CF_IDENTITY_MAP)
            && !facts.has(CF_CHM)
            && !facts.has(CF_UNMOD_WRAPPER));
    if !hash_map_family {
        return None;
    }
    map_remap_mod_count(ctx, this)
}

/// Whether the remapping function moved the `modCount` captured by
/// [`map_remap_mod_count`].
pub(crate) fn map_remap_moved(ctx: &dyn NativeContext, this: ObjectRef, before: Option<i32>) -> bool {
    match before {
        Some(before) => read_map_mod_count(ctx, this).is_some_and(|after| after != before),
        None => false,
    }
}

/// `CRATONVM_COMPAT_MAP_COMPUTE_JDK` — default ON, `=0` restores the
/// `get` + callback + `put`/`remove` composites. `HashMap` and
/// `LinkedHashMap` `computeIfAbsent`, `compute`, `computeIfPresent` and
/// `merge` run the JDK bodies' own shape ([`map_remap_jdk`]):
///
/// * ONE `hashCode()` and one chain walk before the function runs (the
///   composites made two of each), and the JDK's second walk (and, for
///   `computeIfPresent`, second `hash(key)`) only on the removal path, where
///   `removeNode` makes it too;
/// * a NEW key is linked at its bucket's HEAD (`tab[i] = newNode(hash, key,
///   v, first)`), not its tail as `put` does, and the table grows BEFORE the
///   walk when `size > threshold`, never after the insert. Both are iteration
///   order: `m.put("Aa", 1); m.computeIfAbsent("BB", ..)` iterates `{BB, Aa}`
///   on HotSpot (the two collide), and a compute that takes a 16-bucket map
///   to 13 entries leaves it at 16 buckets until the next insert;
/// * an access-ordered `LinkedHashMap` moves the entry AFTER a non-null
///   result (`afterNodeAccess`), not before the function runs: the composites'
///   `get` moved it on entry, so a function that threw, returned null or looked
///   at the order saw it already moved;
/// * a key present with a NULL value is present: `compute` returning null
///   removes that mapping (the composite saw "absent" and left it), and
///   `computeIfAbsent`'s result is written into the existing node.
///
/// Round 13 wave 9, lane hashcompat4 (residual page items 2 and 5).
pub(crate) fn map_compute_jdk_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_MAP_COMPUTE_JDK")
}

/// Which remapping method [`map_remap_jdk`] runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RemapOp {
    IfAbsent,
    Compute,
    IfPresent,
    Merge,
}

/// Which node layout [`map_remap_jdk`] works on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RemapRoute {
    /// `java.util.HashMap` (or a user subclass) on the plain bucket layout.
    HashMap,
    /// `java.util.LinkedHashMap` (or a subclass): `LinkedHashMap$Entry` nodes.
    Linked,
}

/// The receivers [`map_remap_jdk`] serves, or `None` to keep the composite:
/// `LinkedHashMap`, and a `HashMap` with no integer-overlay row and a key
/// that is not a primitive wrapper (the composite's `put` is what starts the
/// overlay on a fresh exact `HashMap`, and a wrapper's `hashCode()`/`equals()`
/// are not observable). `Hashtable` (its own bytecode serves these), CHM
/// segments (no class name), `TreeMap`, `IdentityHashMap` and the
/// unmodifiable wrappers keep the composite. Pure reads: no Java.
pub(crate) fn map_remap_route(ctx: &dyn NativeContext, this: ObjectRef, key: Value) -> Option<RemapRoute> {
    if !map_compute_jdk_enabled() {
        return None;
    }
    let Value::Object(key_ref) = key else {
        return None;
    };
    let facts = receiver_facts(ctx, this);
    if facts.has(CF_LHM) {
        return Some(RemapRoute::Linked);
    }
    if !facts.has(CF_BUCKET_MAP_NAME)
        || facts.has(CF_CHM)
        || facts.has(CF_TREE_MAP)
        || facts.has(CF_IDENTITY_MAP)
        || facts.has(CF_UNMOD_WRAPPER)
        || facts.has(CF_HASHTABLE_ANCESTRY)
    {
        return None;
    }
    if hm_int_fast_len(ctx, this).is_some() {
        return None;
    }
    // A wrapper key keeps the composite only where the composite's `put` can
    // START the integer overlay: an exact `HashMap` with nothing in it. A map
    // with real nodes can never take the overlay again, so its wrapper keys
    // get the JDK shape too (`CRATONVM_COMPAT_OVERLAY_REMAP_ORDER`; `=0`
    // keeps every wrapper key on the composite, as before).
    if key_ref.is_some_and(|k| unbox_wrapper(ctx, k).is_some())
        && (!overlay_remap_order_enabled()
            || (facts.has(CF_EXACT_HASHMAP) && map_state(ctx, this).1 == 0))
    {
        return None;
    }
    Some(RemapRoute::HashMap)
}

/// `CRATONVM_COMPAT_OVERLAY_REMAP_ORDER` — default ON, `=0` restores the
/// tail-append, grow-after model. `HashMap.computeIfAbsent` / `compute` /
/// `merge` link a NEW key at its bucket's HEAD and grow the table only on
/// ENTRY (`size > threshold`), never after the insert; wave 9 gave the node
/// table that shape ([`map_remap_jdk`]) but an `Integer`-keyed map (the
/// integer overlay, `DenseIntEntries`) kept `put`'s: appended, and grown at
/// `3/4` right after. `m.put(1, a); m.computeIfAbsent(17, ..)` iterated
/// `{1, 17}` where HotSpot iterates `{17, 1}`. The overlay now models both
/// ([`DenseIntEntries::insert_remap`]), and a wrapper key on a map with real
/// nodes (which can never take the overlay again) takes `map_remap_jdk`.
/// Round 13 wave 10, lane compat5
/// (`r13w9-hashcompat4-overlay-remap-order-FIXED-20260928.md`;
/// `jit-r13-hashcompat4-proposals-RETIRED-20260929.md` HC4-4).
pub(crate) fn overlay_remap_order_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_OVERLAY_REMAP_ORDER")
}

/// `CRATONVM_COMPAT_OVERLAY_MATERIALIZE_CAP` — default ON, `=0` restores the
/// re-put into the start table. See [`materialize_hm_int_fast`]. Round 13
/// wave 11, lane compat6 (`r13w10-compat5-map-view-residuals-CLOSED-20260929.md`
/// item 2).
pub(crate) fn overlay_materialize_cap_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_OVERLAY_MATERIALIZE_CAP")
}

/// `CRATONVM_COMPAT_OVERLAY_MATERIALIZE_ROLLBACK` -- default ON, `=0` leaves a
/// failed materialisation half-done. [`materialize_hm_int_fast`] re-puts the
/// integer overlay's entries into the node table one by one and drops the
/// overlay only after the last; an `Err` mid-loop (a refused node allocation)
/// returned with the overlay still present AND some entries linked, `size`
/// counting only those: lookups (overlay first) stayed right, `size()` from
/// the field and an iterator over the table did not. Now the partial nodes are
/// unlinked, `size` and the generation restored. Round 15 wave 4 (lane
/// compat4), `r15w3-compat3-collection-review-findings-CLOSED-20261001.md` item 1.
pub(crate) fn overlay_materialize_rollback_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_OVERLAY_MATERIALIZE_ROLLBACK")
}

/// The write of a compute-family composite (`computeIfAbsent`, `compute`,
/// `computeIfPresent`, `merge`): an exact `HashMap` offers a wrapper key to
/// the integer overlay as a REMAP insert (bucket head, no growth after, see
/// [`overlay_remap_order_enabled`]); everything else is the ordinary `put`.
pub(crate) fn map_remap_composite_put(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key: Value,
    value: Value,
) -> MethodCallResult {
    if overlay_remap_order_enabled() && receiver_facts(&*ctx, this).has(CF_EXACT_HASHMAP) {
        if let Some(result) = try_hm_int_fast_put_mode(ctx, this, key, value, true) {
            return result;
        }
    }
    native_map_put(ctx, &[Value::Object(Some(this)), key, value])
}

/// What a remapping function must leave unchanged for the node the walk found
/// (or its absence) to still be true: the `modCount` when the receiver keeps
/// one, and its size and table length either way.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct RemapGuard {
    mod_count: Option<i32>,
    size: i32,
    cap: i32,
    table: bool,
}

pub(crate) fn remap_guard(ctx: &dyn NativeContext, this: ObjectRef, route: RemapRoute) -> RemapGuard {
    let (buckets, size, cap) = match route {
        RemapRoute::HashMap => map_state(ctx, this),
        RemapRoute::Linked => lhm_state(ctx, this),
    };
    RemapGuard {
        mod_count: read_map_mod_count(ctx, this),
        size,
        cap,
        table: buckets.is_some(),
    }
}

/// `getNode(hash, key)` over either layout: the first node of `key`'s chain
/// that matches it (`hash`, then `==`, then `key.equals(k)`). `hash` is `None`
/// for a null key. The node is current after the last `equals`; the caller
/// roots it before running Java again. `tree` is how a `HashMap` tree bin is
/// searched: [`TreeLookup::Direct`] for the bodies that open with
/// `if (first instanceof TreeNode) old = t.getTreeNode(hash, key)`
/// (`compute`, `computeIfAbsent`, `merge`), [`TreeLookup::AfterFirst`] for a
/// `getNode` / `removeNode` (`computeIfPresent`, the removal walk).
pub(crate) fn remap_find(
    ctx: &mut dyn NativeContext,
    route: RemapRoute,
    this: ObjectRef,
    key_pin: usize,
    key: Value,
    hash: Option<i32>,
    tree: TreeLookup,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let key_now = read_pinned_elem(ctx, key_pin, key);
    match route {
        RemapRoute::Linked => lhm_find_node_tree(ctx, this, &key_now, hash, tree),
        RemapRoute::HashMap => {
            let (buckets, size, cap) = map_state(ctx, this);
            let Some(buckets) = buckets else {
                return Ok(None);
            };
            let bucket_hash = hash.unwrap_or(0);
            let start = ctx.get_array_element(buckets, map_bucket_index(bucket_hash, cap));
            let tree = TreeLookup::for_receiver(&*ctx, this, tree);
            map_chain_find_from(
                ctx,
                start,
                bucket_hash,
                key_pin,
                key_now,
                false,
                chain_walk_limit(size),
                tree,
            )
        }
    }
}

pub(crate) fn remap_node_value(ctx: &dyn NativeContext, route: RemapRoute, node: ObjectRef) -> Value {
    match route {
        RemapRoute::HashMap => get_node_value(ctx, node),
        RemapRoute::Linked => ctx.get_field(node, LHM_NODE_VALUE),
    }
}

/// `afterNodeAccess(e)`: an access-ordered `LinkedHashMap` moves the node to
/// the tail; a no-op for `HashMap`.
pub(crate) fn remap_after_access(
    ctx: &mut dyn NativeContext,
    route: RemapRoute,
    this: ObjectRef,
    node: ObjectRef,
) {
    if route == RemapRoute::Linked && lhm_is_access_order(&*ctx, this) {
        lhm_move_to_tail(ctx, this, node);
    }
}

/// Link a NEW `HashMap` node for `key -> value` at the HEAD of its bucket, as
/// `compute` / `computeIfAbsent` / `merge` do (`tab[i] = newNode(hash, key,
/// value, first)`), then `++size` and `++modCount` with NO resize (those
/// bodies grow only on entry, `size > threshold`). `false`, having changed
/// nothing, when the map has no table. No Java runs; the node allocation can
/// collect, so every reference is re-read from its pin.
#[allow(clippy::too_many_arguments)]
pub(crate) fn map_link_new_node_at_head(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
    key_pin: usize,
    key: Value,
    value_pin: usize,
    value: Value,
    hash: i32,
) -> bool {
    let this_now = ctx.read_native_pin(this_pin, this);
    if map_state(ctx, this_now).0.is_none() {
        return false;
    }
    let key_now = read_pinned_elem(ctx, key_pin, key);
    let value_now = read_pinned_elem(ctx, value_pin, value);
    // Can load the node class (a collection point) on the first call.
    let node_cid = map_node_class_for(ctx, this_now, key_now, value_now);
    let node = ctx.alloc_object(node_cid, NODE_NUM_FIELDS);
    let node_pin = ctx.pin_native_root(node);
    ctx.set_field(node, NODE_FIELD_HASH, Value::Int(hash));
    let node = ctx.read_native_pin(node_pin, node);
    let key_now = read_pinned_elem(ctx, key_pin, key);
    ctx.set_field(node, NODE_FIELD_KEY, key_now);
    let node = ctx.read_native_pin(node_pin, node);
    let value_now = read_pinned_elem(ctx, value_pin, value);
    ctx.set_field(node, NODE_FIELD_VALUE, value_now);
    let this_now = ctx.read_native_pin(this_pin, this);
    let (buckets, size, cap) = map_state(ctx, this_now);
    let Some(buckets) = buckets else {
        ctx.unpin_native_roots(node_pin);
        return false;
    };
    let idx = map_bucket_index(hash, cap);
    let head = ctx.get_array_element(buckets, idx);
    let node = ctx.read_native_pin(node_pin, node);
    ctx.set_field(node, NODE_FIELD_NEXT, head);
    let node = ctx.read_native_pin(node_pin, node);
    let this_now = ctx.read_native_pin(this_pin, this);
    if let Some(buckets) = map_state(ctx, this_now).0 {
        ctx.set_array_element(buckets, idx, Value::Object(Some(node)));
    }
    let this_now = ctx.read_native_pin(this_pin, this);
    set_map_size(ctx, this_now, size + 1);
    bump_map_mod_count(ctx, this_now);
    ctx.unpin_native_roots(node_pin);
    true
}

/// The insert of `HashMap.compute` / `computeIfAbsent` / `merge` for an absent
/// key, tree bins included (`CRATONVM_COMPAT_HASHMAP_TREE_BINS`):
///
/// ```java
/// else if (t != null) t.putTreeVal(this, tab, hash, key, v);
/// else {
///     tab[i] = newNode(hash, key, v, first);
///     if (binCount >= TREEIFY_THRESHOLD - 1) treeifyBin(tab, hash);
/// }
/// modCount = mc + 1; ++size;
/// ```
///
/// `binCount` is the length of the chain the find walked (the key was
/// absent), which the caller's modification guard proved unchanged since.
/// `putTreeVal`'s answer is ignored, as the JDK ignores it. `false`, having
/// changed nothing, when the map has no table (see
/// [`map_link_new_node_at_head`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn hm_remap_insert(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
    key_pin: usize,
    key: Value,
    v_pin: usize,
    v: Value,
    hash: i32,
) -> Result<bool, MethodCallFailed> {
    let this_now = ctx.read_native_pin(this_pin, this);
    let key_now = read_pinned_elem(ctx, key_pin, key);
    let v_now = read_pinned_elem(ctx, v_pin, v);
    let objects = matches!(key_now, Value::Object(_)) && matches!(v_now, Value::Object(_));
    let mut treeify = false;
    if objects && hm_tree_family(&*ctx, this_now) {
        let (buckets, size, cap) = map_state(&*ctx, this_now);
        if let Some(tab) = buckets {
            let head = ctx.get_array_element(tab, map_bucket_index(hash, cap));
            if let Some(first) = hm_tree_head(&*ctx, head) {
                hm_tree_put_tree_val(ctx, first, this_now, tab, hash, key_now, v_now)?;
                let this_now = ctx.read_native_pin(this_pin, this);
                let size = map_state(&*ctx, this_now).1;
                set_map_size(ctx, this_now, size + 1);
                bump_map_mod_count(ctx, this_now);
                return Ok(true);
            }
            let (len, all_plain) = hm_chain_census(&*ctx, head, chain_walk_limit(size));
            // `hm_tree_classes_real` can load a class; nothing below reuses a
            // bare reference.
            treeify = len >= HM_TREEIFY_THRESHOLD - 1
                && all_plain
                && hm_tree_may_treeify(&*ctx, this_now)
                && hm_tree_classes_real(ctx);
        }
    }
    if !map_link_new_node_at_head(ctx, this_pin, this, key_pin, key, v_pin, v, hash) {
        return Ok(false);
    }
    if treeify {
        let mut this_now = ctx.read_native_pin(this_pin, this);
        hm_treeify_bin(ctx, &mut this_now, hash, GrowAlloc::SingleAttempt)?;
    }
    Ok(true)
}

/// Move a `LinkedHashMap` node that `put` just linked at its bucket's tail to
/// the bucket's HEAD, where `compute` / `computeIfAbsent` / `merge` link a new
/// node. Pointer surgery on one chain: no Java, no allocation, and nothing
/// else (the insertion-order list and the size) changes. A node not found in
/// its bucket within the walk bound is left where it is.
pub(crate) fn lhm_relink_bucket_head(ctx: &mut dyn NativeContext, this: ObjectRef, node: ObjectRef) {
    let (buckets, size, cap) = lhm_state(ctx, this);
    let Some(buckets) = buckets else {
        return;
    };
    let hash = match ctx.get_field(node, LHM_NODE_HASH) {
        Value::Int(h) => h,
        _ => 0,
    };
    let idx = map_bucket_index(hash, cap);
    let head = ctx.get_array_element(buckets, idx);
    // A tree bin's order is `putTreeVal`'s (`CRATONVM_COMPAT_LHM_TREE_BINS`).
    if lhm_tree_bins_enabled()
        && (is_hm_tree_node(&*ctx, node) || hm_tree_head(&*ctx, head).is_some())
    {
        return;
    }
    let mut prev = match head {
        Value::Object(Some(h)) if std::ptr::eq(h.as_ptr(), node.as_ptr()) => return,
        Value::Object(Some(h)) => h,
        _ => return,
    };
    for _ in 0..chain_walk_limit(size) {
        let next = ctx.get_field(prev, LHM_NODE_NEXT);
        match next {
            Value::Object(Some(n)) if std::ptr::eq(n.as_ptr(), node.as_ptr()) => {
                let after = ctx.get_field(node, LHM_NODE_NEXT);
                ctx.set_field(prev, LHM_NODE_NEXT, after);
                ctx.set_field(node, LHM_NODE_NEXT, head);
                ctx.set_array_element(buckets, idx, Value::Object(Some(node)));
                return;
            }
            Value::Object(Some(n)) => prev = n,
            _ => return,
        }
    }
}

/// The composites' write, for a function that structurally modified the map
/// on a receiver that does not throw for it (no `modCount`, or
/// `CRATONVM_COMPAT_MAP_REMAP_CME=0`): the walk's answer is stale, so the
/// result is written by key, as before. `v` is pinned at `v_pin`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn remap_write_by_key(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
    key_pin: usize,
    key: Value,
    v_pin: usize,
    v: Value,
    remove_on_null: bool,
) -> MethodCallResult {
    let this_now = ctx.read_native_pin(this_pin, this);
    let key_now = read_pinned_elem(ctx, key_pin, key);
    if matches!(v, Value::Object(None)) {
        if remove_on_null {
            native_map_remove(ctx, &[Value::Object(Some(this_now)), key_now])?;
        }
        return Ok(Some(Value::Object(None)));
    }
    let v_now = read_pinned_elem(ctx, v_pin, v);
    native_map_put(ctx, &[Value::Object(Some(this_now)), key_now, v_now])?;
    Ok(Some(read_pinned_elem(ctx, v_pin, v)))
}

/// `HashMap.computeIfAbsent` / `compute` / `computeIfPresent` / `merge` (and
/// `LinkedHashMap`'s, which are inherited) as the JDK bodies run them. See
/// [`map_compute_jdk_enabled`] for what differs from the composites. `arg` is
/// `merge`'s value (the caller has run each method's null contract); the
/// other three ignore it.
pub(crate) fn map_remap_jdk(
    ctx: &mut dyn NativeContext,
    route: RemapRoute,
    this: ObjectRef,
    key: Value,
    arg: Value,
    function: ObjectRef,
    op: RemapOp,
) -> MethodCallResult {
    let null = Value::Object(None);
    // A `reversed()` view rebuilds from its source first, as every other
    // `LinkedHashMap` read does; the arguments are carried across it.
    let (this, key, arg, function) = if route == RemapRoute::Linked {
        let mut carried = [key, arg, Value::Object(Some(function))];
        let this = resync_reversed_map_carrying(ctx, this, &mut carried)?;
        let [key, arg, carried_fn] = carried;
        let Value::Object(Some(function)) = carried_fn else {
            return Ok(Some(null));
        };
        (this, key, arg, function)
    } else {
        (this, key, arg, function)
    };
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let arg_pin = pin_value(ctx, arg);
    let fn_pin = ctx.pin_native_root(function);
    // Every pin above `this_pin` is released with it, on every exit.
    let result = (|| -> MethodCallResult {
        let hash = match read_pinned_elem(ctx, key_pin, key) {
            Value::Object(Some(k)) => Some(map_hash_key(ctx, k)?),
            _ => None,
        };
        // `if (size > threshold || (tab = table) == null ..) n = (tab =
        // resize()).length;` -- `compute`, `computeIfAbsent` and `merge` grow
        // on ENTRY; `computeIfPresent` is a `getNode`. The `LinkedHashMap`
        // insert below grows through its own `put` (its bucket chains are
        // not its iteration order).
        if route == RemapRoute::HashMap && op != RemapOp::IfPresent {
            let mut this_now = ctx.read_native_pin(this_pin, this);
            let (buckets, size, cap) = map_state(ctx, this_now);
            if buckets.is_none() || size > map_grow_threshold(ctx, this_now, cap) {
                map_resize(ctx, &mut this_now, GrowAlloc::SingleAttempt);
            }
        }
        // `CRATONVM_COMPAT_LHM_REMAP_GROW_ON_ENTRY`: the same entry growth for
        // a `LinkedHashMap` (the bodies are inherited), whose insert below
        // then does not grow (`native_lhm_put_evict_inner`, `may_treeify =
        // false`). The native `put` grew before linking instead, so the 13th
        // key a `computeIfAbsent` added doubled a 16-slot table HotSpot keeps.
        if route == RemapRoute::Linked
            && op != RemapOp::IfPresent
            && lhm_remap_grow_on_entry_enabled()
        {
            let this_now = ctx.read_native_pin(this_pin, this);
            let (buckets, size, cap) = lhm_state(ctx, this_now);
            if buckets.is_none() || size > lhm_grow_threshold(&*ctx, this_now, cap) {
                lhm_resize(ctx, this_now, GrowAlloc::SingleAttempt);
            }
        }
        let this_now = ctx.read_native_pin(this_pin, this);
        // `computeIfPresent` is a `getNode`; the other three search a tree
        // bin with `getTreeNode` straight away.
        let find_tree = if op == RemapOp::IfPresent {
            TreeLookup::AfterFirst
        } else {
            TreeLookup::Direct
        };
        let old = remap_find(ctx, route, this_now, key_pin, key, hash, find_tree)?;
        let old_pin = old.map(|n| ctx.pin_native_root(n));
        let old_value = match old {
            Some(n) => remap_node_value(&*ctx, route, n),
            None => null,
        };
        let old_is_null = matches!(old_value, Value::Object(None));
        match op {
            // `if (old != null && (oldValue = old.value) != null) {
            // afterNodeAccess(old); return oldValue; }`
            RemapOp::IfAbsent if !old_is_null => {
                if let (Some(n), Some(p)) = (old, old_pin) {
                    let n = ctx.read_native_pin(p, n);
                    let this_now = ctx.read_native_pin(this_pin, this);
                    remap_after_access(ctx, route, this_now, n);
                }
                return Ok(Some(old_value));
            }
            // `if ((e = getNode(key)) != null && (oldValue = e.value) != null)`
            RemapOp::IfPresent if old_is_null => return Ok(Some(null)),
            _ => {}
        }
        let old_value_pin = pin_value(ctx, old_value);
        // `merge` calls its function only for a present non-null value; a
        // present null value, or an absent key, takes `value` as it is.
        let calls_function = op != RemapOp::Merge || (old.is_some() && !old_is_null);
        let v = if calls_function {
            let this_now = ctx.read_native_pin(this_pin, this);
            let guard = remap_guard(&*ctx, this_now, route);
            let f = ctx.read_native_pin(fn_pin, function);
            let key_now = read_pinned_elem(ctx, key_pin, key);
            let old_now = read_pinned_elem(ctx, old_value_pin, old_value);
            let returned = match op {
                RemapOp::IfAbsent => ctx.invoke_virtual(
                    f,
                    "apply",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                    &[key_now],
                )?,
                RemapOp::Merge => {
                    let arg_now = read_pinned_elem(ctx, arg_pin, arg);
                    ctx.invoke_virtual(
                        f,
                        "apply",
                        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                        &[old_now, arg_now],
                    )?
                }
                RemapOp::Compute | RemapOp::IfPresent => ctx.invoke_virtual(
                    f,
                    "apply",
                    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                    &[key_now, old_now],
                )?,
            };
            let v = returned.unwrap_or(Value::Object(None));
            let this_now = ctx.read_native_pin(this_pin, this);
            if remap_guard(&*ctx, this_now, route) != guard {
                // `if (mc != modCount) throw new ConcurrentModificationException();`
                if guard.mod_count.is_some() && (op == RemapOp::IfAbsent || map_remap_cme_enabled())
                {
                    return Err(RuntimeError::ConcurrentModificationException.into());
                }
                let v_pin = pin_value(ctx, v);
                let remove_on_null = old.is_some() && op != RemapOp::IfAbsent;
                return remap_write_by_key(
                    ctx,
                    this_pin,
                    this,
                    key_pin,
                    key,
                    v_pin,
                    v,
                    remove_on_null,
                );
            }
            v
        } else {
            read_pinned_elem(ctx, arg_pin, arg)
        };
        let v_pin = pin_value(ctx, v);
        let v_is_null = matches!(v, Value::Object(None));
        let old_node = match (old, old_pin) {
            (Some(n), Some(p)) => Some(ctx.read_native_pin(p, n)),
            _ => None,
        };
        match old_node {
            // `old.value = v; afterNodeAccess(old);`
            Some(node) if !v_is_null => {
                let v_now = read_pinned_elem(ctx, v_pin, v);
                match route {
                    RemapRoute::HashMap => set_node_value(ctx, node, v_now),
                    RemapRoute::Linked => ctx.set_field(node, LHM_NODE_VALUE, v_now),
                }
                let this_now = ctx.read_native_pin(this_pin, this);
                remap_after_access(ctx, route, this_now, node);
                Ok(Some(read_pinned_elem(ctx, v_pin, v)))
            }
            // `computeIfAbsent` over a present null value: nothing changes.
            Some(_) if op == RemapOp::IfAbsent => Ok(Some(null)),
            // `removeNode(hash, key, null, false, true)`: a second walk of the
            // chain, and for `computeIfPresent` a second `hash(key)` first.
            Some(_) => {
                let hash2 = if op == RemapOp::IfPresent {
                    match read_pinned_elem(ctx, key_pin, key) {
                        Value::Object(Some(k)) => Some(map_hash_key(ctx, k)?),
                        _ => None,
                    }
                } else {
                    hash
                };
                let this_now = ctx.read_native_pin(this_pin, this);
                if let Some(victim) = remap_find(
                    ctx,
                    route,
                    this_now,
                    key_pin,
                    key,
                    hash2,
                    TreeLookup::AfterFirst,
                )? {
                    let this_now = ctx.read_native_pin(this_pin, this);
                    match route {
                        RemapRoute::HashMap => {
                            map_unlink_node(ctx, this_now, victim);
                        }
                        RemapRoute::Linked => {
                            lhm_unlink_node(ctx, this_now, victim);
                        }
                    }
                }
                Ok(Some(null))
            }
            None if v_is_null => Ok(Some(null)),
            None => {
                match route {
                    RemapRoute::HashMap => {
                        let linked = hm_remap_insert(
                            ctx,
                            this_pin,
                            this,
                            key_pin,
                            key,
                            v_pin,
                            v,
                            hash.unwrap_or(0),
                        )?;
                        if !linked {
                            return remap_write_by_key(
                                ctx, this_pin, this, key_pin, key, v_pin, v, false,
                            );
                        }
                    }
                    RemapRoute::Linked => {
                        // `newNode` links at the list tail and
                        // `afterNodeInsertion(true)` consults
                        // `removeEldestEntry`, which the `put` below does;
                        // the node then moves to its bucket's head.
                        let this_now = ctx.read_native_pin(this_pin, this);
                        let key_now = read_pinned_elem(ctx, key_pin, key);
                        let v_now = read_pinned_elem(ctx, v_pin, v);
                        let mut placed: Option<ObjectRef> = None;
                        // `CRATONVM_COMPAT_LHM_REMAP_EVICT_LAST`: the put
                        // does not consult `removeEldestEntry`; it runs after
                        // the relink and the treeify below, as the JDK's
                        // `afterNodeInsertion(true)` does.
                        let evict_last = lhm_remap_evict_last_enabled();
                        // `may_treeify = false`: the compute bodies treeify
                        // after their own head insert, with their own count
                        // (below). A tree bin still takes `putTreeVal`.
                        native_lhm_put_evict_inner(
                            ctx,
                            &[Value::Object(Some(this_now)), key_now, v_now],
                            !evict_last,
                            GrowAlloc::SingleAttempt,
                            hash,
                            Some(&mut placed),
                            true,
                            false,
                        )?;
                        if let Some(node) = placed {
                            let this_now = ctx.read_native_pin(this_pin, this);
                            lhm_relink_bucket_head(ctx, this_now, node);
                            // `if (binCount >= TREEIFY_THRESHOLD - 1)
                            // treeifyBin(tab, hash);`: the bin held `binCount`
                            // entries before this one (`CRATONVM_COMPAT_LHM_TREE_BINS`).
                            let this_now = ctx.read_native_pin(this_pin, this);
                            if !is_hm_tree_node(&*ctx, node)
                                && lhm_tree_may_treeify(&*ctx, this_now)
                            {
                                let (buckets, size, cap) = lhm_state(ctx, this_now);
                                if let Some(tab) = buckets {
                                    let bin = ctx.get_array_element(
                                        tab,
                                        map_bucket_index(hash.unwrap_or(0), cap),
                                    );
                                    let (len, all_plain) = chain_census_of(
                                        &*ctx,
                                        bin,
                                        chain_walk_limit(size),
                                        CF_LHM_ENTRY,
                                    );
                                    if len >= HM_TREEIFY_THRESHOLD
                                        && all_plain
                                        && hm_tree_classes_real(ctx)
                                    {
                                        let this_now = ctx.read_native_pin(this_pin, this);
                                        lhm_treeify_bin(
                                            ctx,
                                            this_now,
                                            hash.unwrap_or(0),
                                            GrowAlloc::SingleAttempt,
                                        )?;
                                    }
                                }
                            }
                        }
                        if evict_last {
                            // `afterNodeInsertion(true)`. The helper releases
                            // `evict_pin` on every answer; `this_pin` (below
                            // it) is released by the caller.
                            let this_now = ctx.read_native_pin(this_pin, this);
                            let evict_pin = ctx.pin_native_root(this_now);
                            lhm_after_node_insertion(ctx, evict_pin, this_now, true, None, None)?;
                        }
                    }
                }
                Ok(Some(read_pinned_elem(ctx, v_pin, v)))
            }
        }
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_map_compute(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // RULE F, ahead of the key. See `reject_null_functional`.
    reject_null_functional(args.get(2))?;
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let bi_function = match args.get(2) {
        Some(Value::Object(Some(f))) => *f,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `CRATONVM_COMPAT_MAP_COMPUTE_JDK`: see `map_compute_jdk_enabled`.
    if let Some(route) = map_remap_route(&*ctx, this, key) {
        let null = Value::Object(None);
        return map_remap_jdk(ctx, route, this, key, null, bi_function, RemapOp::Compute);
    }

    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let bi_function_pin = ctx.pin_native_root(bi_function);

    let this_cur = ctx.read_native_pin(this_pin, this);
    let key_cur = read_pinned_elem(ctx, key_pin, key);
    let existing = match native_map_get(ctx, &[Value::Object(Some(this_cur)), key_cur]) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let old_val = existing.unwrap_or(Value::Object(None));
    let old_val_pin = pin_value(ctx, old_val);

    let mod_before = map_remap_mod_count(&*ctx, ctx.read_native_pin(this_pin, this));
    let bi_function_cur = ctx.read_native_pin(bi_function_pin, bi_function);
    let key_cur = read_pinned_elem(ctx, key_pin, key);
    let old_val_cur = read_pinned_elem(ctx, old_val_pin, old_val);
    let result = match ctx.invoke_virtual(
        bi_function_cur,
        "apply",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[key_cur, old_val_cur],
    ) {
        Ok(r) => r,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    if map_remap_moved(&*ctx, ctx.read_native_pin(this_pin, this), mod_before) {
        ctx.unpin_native_roots(this_pin);
        return Err(RuntimeError::ConcurrentModificationException.into());
    }
    // Keep boxed — maps store Object references (unboxing reads back as null).
    let new_val = result.unwrap_or(Value::Object(None));

    if let Value::Object(None) = new_val {
        // Remove mapping if new value is null.
        if let Value::Object(Some(_)) = old_val {
            let this_cur = ctx.read_native_pin(this_pin, this);
            let key_cur = read_pinned_elem(ctx, key_pin, key);
            if let Err(e) = native_map_remove(ctx, &[Value::Object(Some(this_cur)), key_cur]) {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        }
        ctx.unpin_native_roots(this_pin);
        return Ok(Some(Value::Object(None)));
    }

    let new_val_pin = pin_value(ctx, new_val);
    let this_cur = ctx.read_native_pin(this_pin, this);
    let key_cur = read_pinned_elem(ctx, key_pin, key);
    let new_val_cur = read_pinned_elem(ctx, new_val_pin, new_val);
    if let Err(e) = map_remap_composite_put(ctx, this_cur, key_cur, new_val_cur) {
        ctx.unpin_native_roots(this_pin);
        return Err(e);
    }
    let ret = read_pinned_elem(ctx, new_val_pin, new_val);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(ret))
}

pub(crate) fn native_map_compute_if_present(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // RULE F, ahead of the key. MEASURED: this fires even when the key is
    // ABSENT (`computeIfPresent("zz", null)` throws), so it cannot be folded
    // into the present-branch. See `reject_null_functional`.
    reject_null_functional(args.get(2))?;
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let bi_function = match args.get(2) {
        Some(Value::Object(Some(f))) => *f,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `CRATONVM_COMPAT_MAP_COMPUTE_JDK`: see `map_compute_jdk_enabled`.
    if let Some(route) = map_remap_route(&*ctx, this, key) {
        let null = Value::Object(None);
        return map_remap_jdk(ctx, route, this, key, null, bi_function, RemapOp::IfPresent);
    }

    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let bi_function_pin = ctx.pin_native_root(bi_function);

    let this_cur = ctx.read_native_pin(this_pin, this);
    let key_cur = read_pinned_elem(ctx, key_pin, key);
    let existing = match native_map_get(ctx, &[Value::Object(Some(this_cur)), key_cur]) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let old_val = existing.unwrap_or(Value::Object(None));

    // Only apply if key is present (old_val is non-null)
    if let Value::Object(None) = old_val {
        ctx.unpin_native_roots(this_pin);
        return Ok(Some(Value::Object(None)));
    }
    let old_val_pin = pin_value(ctx, old_val);

    let mod_before = map_remap_mod_count(&*ctx, ctx.read_native_pin(this_pin, this));
    let bi_function_cur = ctx.read_native_pin(bi_function_pin, bi_function);
    let key_cur = read_pinned_elem(ctx, key_pin, key);
    let old_val_cur = read_pinned_elem(ctx, old_val_pin, old_val);
    let result = match ctx.invoke_virtual(
        bi_function_cur,
        "apply",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[key_cur, old_val_cur],
    ) {
        Ok(r) => r,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    if map_remap_moved(&*ctx, ctx.read_native_pin(this_pin, this), mod_before) {
        ctx.unpin_native_roots(this_pin);
        return Err(RuntimeError::ConcurrentModificationException.into());
    }
    // Keep boxed — maps store Object references (unboxing reads back as null).
    let new_val = result.unwrap_or(Value::Object(None));

    if let Value::Object(None) = new_val {
        let this_cur = ctx.read_native_pin(this_pin, this);
        let key_cur = read_pinned_elem(ctx, key_pin, key);
        if let Err(e) = native_map_remove(ctx, &[Value::Object(Some(this_cur)), key_cur]) {
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
    if let Err(e) = map_remap_composite_put(ctx, this_cur, key_cur, new_val_cur) {
        ctx.unpin_native_roots(this_pin);
        return Err(e);
    }
    let ret = read_pinned_elem(ctx, new_val_pin, new_val);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(ret))
}

pub(crate) fn native_map_merge(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    // `HashMap.merge` opens with
    // `if (value == null || remappingFunction == null) throw new NullPointerException();`
    // — `Map.merge`: "@throws NullPointerException if ... the value or
    // remappingFunction is null". `Map.mergeNullValueThrows` measured
    // `m.merge("g", null, Integer::sum)` returning normally here.
    //
    // The refusal is not decoration. A null value means "remove the mapping" to
    // the rest of this function (see the `new_val == null` branch below, which
    // is the correct handling of a null RESULT), so a caller that reached
    // `merge` with a null argument silently got a delete where it asked for an
    // insert — and on an absent key, a silent no-op.
    //
    // The refusal is on an explicitly-passed null, not on a missing argument:
    // `args.get(3)` returning `None` is a malformed call, and turning that into
    // an NPE would report a dispatch defect as a program error.
    if matches!(value, Value::Object(None)) || matches!(args.get(3), Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    }
    let bi_function = match args.get(3) {
        Some(Value::Object(Some(f))) => *f,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `CRATONVM_COMPAT_MAP_COMPUTE_JDK`: see `map_compute_jdk_enabled`.
    if let Some(route) = map_remap_route(&*ctx, this, key) {
        return map_remap_jdk(ctx, route, this, key, value, bi_function, RemapOp::Merge);
    }

    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let value_pin = pin_value(ctx, value);
    let bi_function_pin = ctx.pin_native_root(bi_function);

    let this_cur = ctx.read_native_pin(this_pin, this);
    let key_cur = read_pinned_elem(ctx, key_pin, key);
    let existing = match native_map_get(ctx, &[Value::Object(Some(this_cur)), key_cur]) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let old_val = existing.unwrap_or(Value::Object(None));
    let old_val_pin = pin_value(ctx, old_val);

    let new_val = if let Value::Object(Some(_)) = old_val {
        // Key present — merge with BiFunction.
        let mod_before = map_remap_mod_count(&*ctx, ctx.read_native_pin(this_pin, this));
        let bi_function_cur = ctx.read_native_pin(bi_function_pin, bi_function);
        let old_val_cur = read_pinned_elem(ctx, old_val_pin, old_val);
        let value_cur = read_pinned_elem(ctx, value_pin, value);
        let result = match ctx.invoke_virtual(
            bi_function_cur,
            "apply",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[old_val_cur, value_cur],
        ) {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };
        if map_remap_moved(&*ctx, ctx.read_native_pin(this_pin, this), mod_before) {
            ctx.unpin_native_roots(this_pin);
            return Err(RuntimeError::ConcurrentModificationException.into());
        }
        // Keep boxed — maps store Object references (unboxing reads back as null).
        result.unwrap_or(Value::Object(None))
    } else {
        // Key absent — use value directly.
        read_pinned_elem(ctx, value_pin, value)
    };

    if let Value::Object(None) = new_val {
        let this_cur = ctx.read_native_pin(this_pin, this);
        let key_cur = read_pinned_elem(ctx, key_pin, key);
        if let Err(e) = native_map_remove(ctx, &[Value::Object(Some(this_cur)), key_cur]) {
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
    if let Err(e) = map_remap_composite_put(ctx, this_cur, key_cur, new_val_cur) {
        ctx.unpin_native_roots(this_pin);
        return Err(e);
    }
    let ret = read_pinned_elem(ctx, new_val_pin, new_val);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(ret))
}

/// `HashMap.replaceAll` as the JDK runs it, under
/// `CRATONVM_COMPAT_MAP_REMAP_CME`: walk the map's own nodes, write each
/// function result into its node (`e.value = function.apply(e.key,
/// e.value)`: no `hashCode()`, no `equals`), and throw
/// `ConcurrentModificationException` after the walk when the function
/// structurally modified the map. The element path below re-`put` every
/// entry: a hash and a chain walk per entry, no exception, and it put back a
/// key the function had removed. Plain `HashMap` receivers only (the shapes
/// [`map_single_walk_eligible`] and [`hs_backing_nodes`] accept); `None`
/// before any Java has run. Round 13 wave 8, lane hashcompat3.
pub(crate) fn map_replace_all_by_nodes(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    function: ObjectRef,
) -> Option<MethodCallResult> {
    if !map_remap_cme_enabled()
        || !map_single_walk_eligible(&*ctx, this)
        || receiver_facts(&*ctx, this).has(CF_HASHTABLE_ANCESTRY)
    {
        return None;
    }
    let (nodes, lhm) = hs_backing_nodes(&*ctx, this)?;
    if lhm {
        return None;
    }
    let mod_before = map_remap_mod_count(&*ctx, this);
    let node_vals: Vec<Value> = nodes.iter().map(|n| Value::Object(Some(*n))).collect();
    let this_pin = ctx.pin_native_root(this);
    let function_pin = ctx.pin_native_root(function);
    let (_, node_pins) = pin_value_slice(ctx, &node_vals);
    // Every pin above `this_pin` is released with it, on every exit.
    let result = (|| -> MethodCallResult {
        for (i, nv) in node_vals.iter().enumerate() {
            let Value::Object(Some(node)) = read_pinned_elem(ctx, node_pins[i], *nv) else {
                continue;
            };
            let key = get_node_key(&*ctx, node);
            let value = get_node_value(&*ctx, node);
            let f = ctx.read_native_pin(function_pin, function);
            let replaced = ctx
                .invoke_virtual(
                    f,
                    "apply",
                    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                    &[key, value],
                )?
                .unwrap_or(Value::Object(None));
            let node = ctx.read_native_pin(node_pins[i], node);
            set_node_value(ctx, node, replaced);
        }
        if map_remap_moved(&*ctx, ctx.read_native_pin(this_pin, this), mod_before) {
            return Err(RuntimeError::ConcurrentModificationException.into());
        }
        Ok(None)
    })();
    ctx.unpin_native_roots(this_pin);
    Some(result)
}

pub(crate) fn native_map_replace_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // RULE F, empty receiver included. See `reject_null_functional`.
    reject_null_functional(args.get(1))?;
    let bi_function = match args.get(1) {
        Some(Value::Object(Some(f))) => *f,
        _ => return Ok(None),
    };
    if let Some(result) = map_replace_all_by_nodes(ctx, this, bi_function) {
        return result;
    }

    let this_pin = ctx.pin_native_root(this);
    let bi_function_pin = ctx.pin_native_root(bi_function);
    // `collect_entries_any`, not `map_collect_entries`: the latter reads HashMap
    // buckets, so registering this for `java/util/TreeMap` in round 2 produced a
    // native that RAN (`invocations: 1`) over zero entries and reported success.
    // The `_any` collector is the one that knows the six map families, and it is
    // what `build_reversed_map_snapshot` already uses.
    let entries = collect_entries_any(ctx, this)?;
    let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (_, flat_pins) = pin_value_slice(ctx, &flat);
    for i in 0..entries.len() {
        let bi_function_cur = ctx.read_native_pin(bi_function_pin, bi_function);
        let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
        let value = read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]);
        let result = match ctx.invoke_virtual(
            bi_function_cur,
            "apply",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[key, value],
        ) {
            Ok(r) => r,
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };
        // Keep boxed — maps store Object references (unboxing reads back as null).
        let new_val = result.unwrap_or(Value::Object(None));
        let new_val_pin = pin_value(ctx, new_val);
        let this_cur = ctx.read_native_pin(this_pin, this);
        let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
        let new_val_cur = read_pinned_elem(ctx, new_val_pin, new_val);
        if let Err(e) = native_map_put(ctx, &[Value::Object(Some(this_cur)), key, new_val_cur]) {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
        ctx.unpin_native_roots(new_val_pin);
    }

    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

// ===========================================================================
// Factory methods: List.of, Set.of, Map.of, Map.entry
// ===========================================================================

pub(crate) fn register_factory_natives(r: &mut NativeMethodRegistry) {
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
    // With a REAL JDK class library these rows are not registered, in either
    // mode, since interpreter round i1 wave 48 (lane L6): `List.of`, `Set.of`,
    // `Map.of` and `Map.ofEntries` run the JDK's `ImmutableCollections`
    // bytecode, as under `--jdk-only` and on HotSpot. Under `--compatible` the
    // rows handed out `cratonvm/internal/Unmodifiable*` stand-ins whose class a
    // debugger reported (`L1W23JdiConformance`, `this.tags`) and whose fields
    // `Unsafe` read as the backing collection (probes
    // `tools/probes/interp/L6/L6W48ListOfIsTheJdksList.java`,
    // `L6W48SetOfIsTheJdksSet.java`, `L6W48MapOfIsTheJdksMap.java`). Every
    // `List` / `Set` / `Map` / `Collection` native already answers a real
    // `ImmutableCollections` receiver by its own bytecode (measured under
    // `--compatible`, with and without the JIT).
    //
    // WITHOUT a real JDK (`!r.real_jdk()`: an embedded VM with no class
    // library, or the synthetic JDK) there is no `java.util.List.of` bytecode
    // at all, and these natives are the only implementation: removing them
    // outright made `Set.of(..)` a `NoSuchMethodError` there
    // (`vm/tests/collection_dedup_is_hash_bucketed.rs`, wave-48 host run).
    // (`docs/feature-designs/jdk-only-mode.md`.)
    if !r.real_jdk() {
        // List.of
        r.register(
            "java/util/List",
            "of",
            "()Ljava/util/List;",
            native_list_of_0,
        );
        r.register(
            "java/util/List",
            "of",
            "(Ljava/lang/Object;)Ljava/util/List;",
            native_list_of_1,
        );
        r.register(
            "java/util/List",
            "of",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/List;",
            native_list_of_2,
        );
        r.register(
            "java/util/List",
            "of",
            "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/List;",
            native_list_of_3,
        );
        r.register(
            "java/util/List",
            "of",
            "([Ljava/lang/Object;)Ljava/util/List;",
            native_list_of_array,
        );

        // Set.of
        r.register("java/util/Set", "of", "()Ljava/util/Set;", native_set_of_0);
        r.register(
            "java/util/Set",
            "of",
            "(Ljava/lang/Object;)Ljava/util/Set;",
            native_set_of_1,
        );
        r.register(
            "java/util/Set",
            "of",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Set;",
            native_set_of_2,
        );
        r.register(
            "java/util/Set",
            "of",
            "([Ljava/lang/Object;)Ljava/util/Set;",
            native_set_of_array,
        );

        // Map.of
        r.register("java/util/Map", "of", "()Ljava/util/Map;", native_map_of_0);
        r.register(
            "java/util/Map",
            "of",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;",
            native_map_of_1,
        );
        r.register(
            "java/util/Map",
            "of",
            "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;",
            native_map_of_2,
        );
        // DELETED 2026-08-05 — `Map.entry` is not registered, deliberately.
        //
        // It used to return a 2-field synthetic `java/util/Map$Entry`. A
        // differential run against HotSpot 25 (`probes/ShadowDifferentialProbe.java`)
        // caught two observable differences, both of them the fake being a worse
        // version of code that already exists:
        //
        //   * `Map.entry("k", 7).toString()` was `java.util.Map$Entry@6c`, not
        //     `k=7` — the synthetic has no `toString`, so `Object`'s ran.
        //   * `setValue` SUCCEEDED. `Map.entry` is specified to return an
        //     immutable entry and the JDK's `KeyValueHolder.setValue` throws
        //     `UnsupportedOperationException`. Silently accepting the write is the
        //     dangerous half: a caller that defensively mutates a copy got no
        //     signal that it had mutated nothing anybody would read.
        //
        // `java.util.Map.entry` is a static interface method with ordinary
        // bytecode, so deleting the registration is all it takes — contract §1.4's
        // "the real bytecode wins" applied by removing the thing that was winning
        // instead. Everything else this probe exercises (`List/Set/Map.of`,
        // `copyOf`, `unmodifiable*`, `subList`, `LinkedHashMap` entry views incl.
        // write-through `setValue`) already matched HotSpot byte for byte.

        // Higher-arity `List.of` / `Set.of` / `Map.of` overloads (real JDK
        // declares fixed-arity variants up to 10). Without these, e.g.
        // `Set.of("a","b","c")` falls through to real-JDK bytecode and the
        // result is not one of our immutable wrappers — its mutators would not
        // throw. Register a generic handler for every fixed arity so all of
        // them produce a frozen wrapper.
        for n in 4..=10 {
            let elem_obj = "Ljava/lang/Object;";
            let list_desc = format!("({})Ljava/util/List;", elem_obj.repeat(n));
            r.register("java/util/List", "of", &list_desc, native_list_of_varargs);
            let set_desc = format!("({})Ljava/util/Set;", elem_obj.repeat(n));
            r.register("java/util/Set", "of", &set_desc, native_set_of_varargs);
        }
        // `Set.of` 3-arg (List.of 1..3 already covered above; Set.of only had
        // 0..2 fixed variants registered).
        r.register(
            "java/util/Set",
            "of",
            "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Set;",
            native_set_of_varargs,
        );
        // `Map.of` 3..10 entries (each entry is two Object args).
        for n in 3..=10 {
            let map_desc = format!("({})Ljava/util/Map;", "Ljava/lang/Object;".repeat(n * 2));
            r.register("java/util/Map", "of", &map_desc, native_map_of_varargs);
        }
        // `Map.ofEntries(Map$Entry...)`.
        r.register(
            "java/util/Map",
            "ofEntries",
            "([Ljava/util/Map$Entry;)Ljava/util/Map;",
            native_map_of_entries,
        );
    }
    r.set_category(__prev_cat);
    // The write and read halves of what the factories above produce.
    register_immutable_serialization_natives(r);
}

/// Generic `List.of` for fixed-arity overloads: every positional arg is an
/// element. Produces a frozen (unmodifiable) list.
// ---------------------------------------------------------------------------
// `List.of` / `Set.of` / `Map.of` argument validation
// ---------------------------------------------------------------------------
//
// These factories are not "an ArrayList with a frozen wrapper". Their contract
// is `java.util.ImmutableCollections`', which REJECTS a null element with a
// `NullPointerException` and a repeated `Set` element / `Map` key with an
// `IllegalArgumentException`. Ours accepted both: `Set.of("a","a")` silently
// deduplicated to `["a"]` and `Map.of(null,"1")` produced `{null=1}`. Validating
// is the reason an application writes `Set.of(...)` instead of
// `new HashSet<>(Arrays.asList(...))`, so answering with a collection where the
// JDK throws converts a caught, located bug into a wrong value carried onward.
//
// The checks live at the `of` ENTRY POINTS and NOT inside `make_set_of` /
// `make_map_of`, because those two also build `Collectors.toSet()`, `Set.copyOf`
// and every other synthetic set/map in this crate — and `copyOf` DEDUPLICATES
// by contract (`Set.copyOf(List.of("a","a")).size() == 1`, a row
// `ImmutableCollectionsDifferentialProbe` asserts). One helper serving both
// callers would have to pick, and either pick is wrong for the other.

/// `CRATONVM_COMPAT_IMMUTABLE_OF_NULL_CHECKS` -- default ON, `=0` restores the
/// old answers. JDK 25's `List.of(E...)` / `Set.of(E...)` open with
/// `switch (elements.length)` and `Map.ofEntries` with `entries.length`, so a
/// null ARRAY is a `NullPointerException`; `Map.ofEntries` then dereferences
/// every slot (`getKey()`) and lets a throwing `getKey()` / `getValue()`
/// propagate. The natives answered an empty collection for a null array,
/// SKIPPED a null slot (`Map.ofEntries(e, null)` was `{e}`), and swallowed a
/// throwing accessor into a null (and then a bare NPE). Round 15 wave 2 (lane
/// compat). The helpful messages are the JDK's for those bytecodes (java.base
/// carries its parameter names, as the measured `"m"` of `putAll` shows).
pub(crate) fn immutable_of_null_checks_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_IMMUTABLE_OF_NULL_CHECKS")
}

/// A `NullPointerException` carrying `message` (a JDK helpful-NPE text).
pub(crate) fn of_npe(message: &str) -> MethodCallFailed {
    RuntimeError::NullPointerException {
        message: Some(message.to_string()),
    }
    .into()
}

/// `Objects.requireNonNull` over a factory's arguments.
///
/// The NPE carries **no message**: the JDK's comes from a bare
/// `Objects.requireNonNull(e)`, and `Throwable.toString()` omits the `": …"`
/// suffix only for a null message — so a "helpful" message here is a visible
/// difference from HotSpot rather than a bonus. Same trap as the bare
/// `IllegalStateException` in `ArrayList$SubList$1.remove`.
pub(crate) fn of_reject_null_elements(elems: &[Value]) -> Result<(), MethodCallFailed> {
    if elems.iter().any(|e| matches!(e, Value::Object(None))) {
        return Err(MethodCallFailed::from(RuntimeError::NullPointerException {
            message: None,
        }));
    }
    Ok(())
}

/// Render one element for a `duplicate element: …` / `duplicate key: …`
/// message through the receiver's own `toString()` — the text HotSpot prints,
/// and the text the probe diffs.
pub(crate) fn of_element_text(ctx: &mut dyn NativeContext, v: Value) -> String {
    match v {
        Value::Object(Some(o)) => ctx
            .invoke_virtual(o, "toString", "()Ljava/lang/String;", &[])
            .ok()
            .flatten()
            .and_then(|r| match r {
                Value::Object(Some(s)) => ctx.read_string(s),
                _ => None,
            })
            .unwrap_or_else(|| "?".to_string()),
        Value::Int(i) => i.to_string(),
        Value::Long(l) => l.to_string(),
        other => format!("{other:?}"),
    }
}

/// `IllegalArgumentException: duplicate <noun>: <e>` for the first repeat.
///
/// Equality is `values_equal_deep`, which ends in the RECEIVER's `equals` — the
/// same predicate `make_set_of`'s `native_map_put` uses, so the check and the
/// build can never disagree about what counts as a duplicate. It is asked only
/// of earlier elements with the same Java `hashCode()`, which is also the only
/// place `ImmutableCollections.SetN`/`MapN` look for one.
///
/// This used to compare every pair, on the reasoning that every `of` overload
/// takes at most ten arguments. The array form does not, and it is on a hot
/// path: `CLDRLocaleProviderAdapter.createLanguageTagSet` hands `Set.of` all
/// 1152 CLDR language tags, so the first `Calendar.getInstance` per locale paid
/// ~660 000 comparisons (MEASURED 2026-09-12: 199 ms for `Set.of` of 1101
/// strings, HotSpot 0 ms).
pub(crate) fn of_reject_duplicates(
    ctx: &mut dyn NativeContext,
    elems: &[Value],
    noun: &str,
) -> Result<(), MethodCallFailed> {
    if elems.len() < 2 {
        return Ok(());
    }
    // `values_equal_deep` ends in the receiver's `equals`, which allocates and
    // can therefore MOVE every element in this slice. The `Value`s here are
    // bare copies of the caller's argument registers; re-reading them through
    // pins is the same discipline `make_set_of` and `make_map_of` already keep
    // for exactly this reason, and skipping it is how a comparison ends up
    // asking an unrelated `java.lang.Object` whether it equals itself.
    let (base, handles) = pin_value_slice(ctx, elems);
    let mut verdict: Result<(), MethodCallFailed> = Ok(());
    // Earlier indices by `hashCode()`.
    let mut buckets: std::collections::HashMap<i32, Vec<usize>> = std::collections::HashMap::new();
    'outer: for i in 0..elems.len() {
        let a = read_pinned_elem(ctx, handles[i], elems[i]);
        let hash = match element_hash_code(ctx, &a) {
            Ok(h) => h,
            Err(e) => {
                verdict = Err(e);
                break 'outer;
            }
        };
        let earlier = buckets.entry(hash).or_default();
        let candidates = earlier.clone();
        earlier.push(i);
        for j in candidates {
            let a = read_pinned_elem(ctx, handles[i], elems[i]);
            let b = read_pinned_elem(ctx, handles[j], elems[j]);
            match values_equal_deep(ctx, &a, &b) {
                Ok(false) => {}
                Ok(true) => {
                    let dup = read_pinned_elem(ctx, handles[i], elems[i]);
                    let text = of_element_text(ctx, dup);
                    verdict = Err(MethodCallFailed::from(
                        RuntimeError::IllegalArgumentException {
                            message: format!("duplicate {noun}: {text}"),
                        },
                    ));
                    break 'outer;
                }
                Err(e) => {
                    verdict = Err(e);
                    break 'outer;
                }
            }
        }
    }
    if base != usize::MAX {
        ctx.unpin_native_roots(base);
    }
    verdict
}

/// `CRATONVM_COMPAT_IMMUTABLE_OF_JDK_WALK` -- default ON, `=0` restores the
/// two passes ([`of_reject_null_elements`] over everything, then
/// [`of_reject_duplicates`] by hash bucket). JDK 25 validates `Set.of` /
/// `Map.of` / `Map.ofEntries` (and `Set.copyOf` / `Map.copyOf`, which end in
/// them) INSIDE the constructor of the class it returns, one element (pair)
/// at a time in argument order, so a null and a duplicate race by position
/// and the messages and call counts are those constructors':
///
/// * `Set12(e0)`: `requireNonNull(e0)` -- a bare NPE; no `hashCode`/`equals`;
/// * `Set12(e0, e1)`: `e0.equals(requireNonNull(e1))` -- `e1 == null` is a
///   bare NPE, `e0 == null` the helpful `"e0"` one; ONE `equals`, no
///   `hashCode`;
/// * `SetN(E...)` (3+): per element `probe(e)`: `e.hashCode()` (a null is the
///   helpful `"pe"` NPE), then `e.equals(occupant)` along the linear probe of
///   a `2n`-slot table from `floorMod(hash, 2n)` -- no identity short cut and
///   no SALT (the SALT only picks where ITERATION starts);
/// * `Map1(k, v)`: `requireNonNull(k)`, `requireNonNull(v)` (bare);
/// * `MapN(Object...)` (2+ pairs): per pair `requireNonNull(k)`,
///   `requireNonNull(v)` (bare), then the same probe over `2n` key slots.
///
/// `Set.of("a", "a", null)` is `IllegalArgumentException: duplicate element:
/// a` there (it was an NPE here), `Set.of(null, "a")` names `"e0"` and
/// `Set.of(null, "a", "b")` names `"pe"` (both were bare). The build that
/// follows reuses the walk's hash of every element whose `hashCode()` is Java
/// code ([`of_hash_is_java`]) and links without comparing again, so a `SetN` /
/// `MapN` runs each user `hashCode()` once, as on HotSpot. `Set12` / `Map1`
/// still hash for their `HashSet` / `HashMap` backing, which the JDK does not
/// (CW2-1, [`jdk_mint_immutable`], is the answer for those). Round 15 wave 3
/// (lane compat2), `r15w2-compat-immutable-factory-residuals-CLOSED-20260930.md`.
pub(crate) fn immutable_of_jdk_walk_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_IMMUTABLE_OF_JDK_WALK")
}

/// The bare `NullPointerException` of an `Objects.requireNonNull`.
pub(crate) fn of_bare_npe() -> MethodCallFailed {
    RuntimeError::NullPointerException { message: None }.into()
}

/// `recv.equals(arg)` as the JDK constructors ask it: with NO identity short
/// cut (`SetN.probe`, `MapN.probe` and `Set12` call `equals` on the very same
/// object too, and an irreflexive `equals` is then not a duplicate). A
/// `String`, box or enum answers natively -- final, reflexive, free of side
/// effects, so nothing observable is skipped ([`hashtable_keys_equal`]'s
/// arms, whose `stored` is the receiver); anything else runs its `equals`.
pub(crate) fn of_jdk_equals(
    ctx: &mut dyn NativeContext,
    recv: Value,
    arg: Value,
) -> Result<bool, MethodCallFailed> {
    match (recv, arg) {
        (Value::Object(Some(r)), Value::Object(Some(a))) => {
            if let Some(equal) = ctx.java_strings_equal(r, a) {
                return Ok(equal);
            }
            hashtable_keys_equal(ctx, a, r)
        }
        (r, a) => values_equal_deep(ctx, &r, &a),
    }
}

/// Whether the native maps hash `o` by calling its Java `hashCode()` --
/// [`map_key_raw_hash`]'s last arm: `o` is not a box, `String` or enum, whose
/// hashes they compute without a call. Only such an element's walk hash is
/// handed to the build ([`of_backing_put_absent`]): it is the same number the
/// map's own `hashCode()` call would have answered, so a build hash never
/// disagrees with a later lookup's. Pure reads.
pub(crate) fn of_hash_is_java(ctx: &dyn NativeContext, o: ObjectRef) -> bool {
    // Strings first: the common element, and the cheapest refusal.
    ctx.java_string_hash_code(o).is_none()
        && unbox_wrapper(ctx, o).is_none()
        && enum_key_identity(ctx, o).is_none()
}

/// `IllegalArgumentException: duplicate <noun>: <v>` as the JDK's
/// `"duplicate element: " + e` builds it: `String.valueOf(e)`, i.e.
/// `e.toString()` once (a `null` answer prints `null`), and a throwing
/// `toString()` propagates IN PLACE of the IAE. [`of_element_text`] printed
/// `?` for both.
pub(crate) fn of_duplicate(ctx: &mut dyn NativeContext, noun: &str, v: Value) -> MethodCallFailed {
    let text = match v {
        // `String.toString()` is `this`.
        Value::Object(Some(o)) => match ctx.read_string(o) {
            Some(s) => s,
            None => match ctx.invoke_virtual(o, "toString", "()Ljava/lang/String;", &[]) {
                Ok(Some(Value::Object(Some(s)))) => {
                    ctx.read_string(s).unwrap_or_else(|| "null".to_string())
                }
                Ok(_) => "null".to_string(),
                Err(e) => return e,
            },
        },
        other => of_element_text(ctx, other),
    };
    RuntimeError::IllegalArgumentException {
        message: format!("duplicate {noun}: {text}"),
    }
    .into()
}

/// The `SetN(E...)` (`stride` 1, `noun` "element") / `MapN(Object...)`
/// (`stride` 2 over `k0, v0, k1, v1, ..`, `noun` "key") constructor walk of
/// [`immutable_of_jdk_walk_enabled`], over `flat` whose object slots are
/// pinned at `handles` (the caller's). Answers, per element (key), the hash
/// the build may reuse -- `Some` only where [`of_hash_is_java`] -- or the
/// constructor's exception.
pub(crate) fn of_jdk_table_walk(
    ctx: &mut dyn NativeContext,
    flat: &[Value],
    handles: &[usize],
    stride: usize,
    noun: &str,
) -> Result<Vec<Option<i32>>, MethodCallFailed> {
    let n = flat.len() / stride;
    if n == 0 {
        return Ok(Vec::new());
    }
    // `EXPAND_FACTOR * n` slots (a `MapN` table holds `4n` cells, `2n` keys).
    let slots = 2 * n;
    let mut table: Vec<Option<usize>> = vec![None; slots];
    let mut known = Vec::with_capacity(n);
    let at = |ctx: &dyn NativeContext, i: usize| read_pinned_elem(ctx, handles[i], flat[i]);
    for i in 0..n {
        let k = i * stride;
        if stride == 2 {
            // `MapN`: `requireNonNull(k)`, then `requireNonNull(v)`.
            if matches!(flat[k], Value::Object(None)) || matches!(flat[k + 1], Value::Object(None))
            {
                return Err(of_bare_npe());
            }
        } else if matches!(flat[k], Value::Object(None)) {
            // `SetN.probe(pe)`'s `pe.hashCode()`.
            return Err(of_npe(
                "Cannot invoke \"Object.hashCode()\" because \"pe\" is null",
            ));
        }
        let key = at(&*ctx, k);
        let hash = element_hash_code(ctx, &key)?;
        let java_hash = match at(&*ctx, k) {
            Value::Object(Some(o)) => of_hash_is_java(&*ctx, o),
            _ => false,
        };
        let mut idx = (hash as i64).rem_euclid(slots as i64) as usize;
        // Terminates: `slots` is twice the most elements the table can hold.
        while let Some(j) = table[idx] {
            let pe = at(&*ctx, k);
            let ee = at(&*ctx, j * stride);
            if of_jdk_equals(ctx, pe, ee)? {
                let dup = at(&*ctx, k);
                return Err(of_duplicate(ctx, noun, dup));
            }
            idx += 1;
            if idx == slots {
                idx = 0;
            }
        }
        table[idx] = Some(i);
        known.push(java_hash.then_some(hash));
    }
    Ok(known)
}

/// `Set.of(elems)`'s JDK validation by arity ([`immutable_of_jdk_walk_enabled`]:
/// `Set12(e0)`, `Set12(e0, e1)`, `SetN`), over `elems` pinned at `handles`.
/// Answers the hashes [`of_jdk_table_walk`] answers (none for `Set12`).
pub(crate) fn of_set_jdk_check(
    ctx: &mut dyn NativeContext,
    elems: &[Value],
    handles: &[usize],
) -> Result<Vec<Option<i32>>, MethodCallFailed> {
    match elems.len() {
        0 => Ok(Vec::new()),
        1 => {
            if matches!(elems[0], Value::Object(None)) {
                return Err(of_bare_npe());
            }
            Ok(vec![None])
        }
        2 => {
            // `Set12(e0, e1)`: `e0.equals(Objects.requireNonNull(e1))`.
            if matches!(elems[1], Value::Object(None)) {
                return Err(of_bare_npe());
            }
            if matches!(elems[0], Value::Object(None)) {
                return Err(of_npe(
                    "Cannot invoke \"Object.equals(Object)\" because \"e0\" is null",
                ));
            }
            let e0 = read_pinned_elem(&*ctx, handles[0], elems[0]);
            let e1 = read_pinned_elem(&*ctx, handles[1], elems[1]);
            if of_jdk_equals(ctx, e0, e1)? {
                let e0 = read_pinned_elem(&*ctx, handles[0], elems[0]);
                return Err(of_duplicate(ctx, "element", e0));
            }
            Ok(vec![None, None])
        }
        _ => of_jdk_table_walk(ctx, elems, handles, 1, "element"),
    }
}

/// `of_set` under [`immutable_of_jdk_walk_enabled`].
pub(crate) fn of_set_jdk(ctx: &mut dyn NativeContext, elems: &[Value]) -> MethodCallResult {
    if elems.is_empty() {
        // C4-3: `Set.of()` is the JDK's one `EMPTY_SET`.
        if let Some(empty) = canonical_empty_immutable(ctx, UNMOD_SET_CLASS) {
            return Ok(Some(Value::Object(Some(empty))));
        }
        let r = make_set_of(ctx, elems);
        return freeze_result(ctx, UNMOD_SET_CLASS, r);
    }
    // The walk runs `hashCode`/`equals`/`toString` (Java): pinned across it,
    // and the build reads the elements back.
    let (base, handles) = pin_value_slice(ctx, elems);
    let checked = of_set_jdk_check(ctx, elems, &handles);
    let elems = read_value_slice(ctx, &handles, elems);
    if base != usize::MAX {
        ctx.unpin_native_roots(base);
    }
    let known = checked?;
    let r = if known.iter().any(Option::is_some) {
        make_set_of_known(ctx, &elems, Some(known.as_slice()))
    } else {
        make_set_of(ctx, &elems)
    };
    freeze_result(ctx, UNMOD_SET_CLASS, r)
}

/// `of_map` under [`immutable_of_jdk_walk_enabled`].
pub(crate) fn of_map_jdk(ctx: &mut dyn NativeContext, pairs: &[(Value, Value)]) -> MethodCallResult {
    if pairs.is_empty() {
        // C4-3: `Map.of()` / `Map.ofEntries()` is the JDK's one `EMPTY_MAP`.
        if let Some(empty) = canonical_empty_immutable(ctx, UNMOD_MAP_CLASS) {
            return Ok(Some(Value::Object(Some(empty))));
        }
        let r = make_map_of(ctx, pairs);
        return freeze_result(ctx, UNMOD_MAP_CLASS, r);
    }
    let flat: Vec<Value> = pairs.iter().flat_map(|(k, v)| [*k, *v]).collect();
    if pairs.len() == 1 {
        // `Map1(k0, v0)`: `requireNonNull(k0)`, then `requireNonNull(v0)`,
        // both bare; nothing else is asked.
        of_reject_null_elements(&flat)?;
        let r = make_map_of(ctx, pairs);
        return freeze_result(ctx, UNMOD_MAP_CLASS, r);
    }
    let (base, handles) = pin_value_slice(ctx, &flat);
    let checked = of_jdk_table_walk(ctx, &flat, &handles, 2, "key");
    let flat = read_value_slice(ctx, &handles, &flat);
    if base != usize::MAX {
        ctx.unpin_native_roots(base);
    }
    let known = checked?;
    let pairs: Vec<(Value, Value)> = flat.chunks_exact(2).map(|kv| (kv[0], kv[1])).collect();
    let r = if known.iter().any(Option::is_some) {
        make_map_of_known(ctx, &pairs, Some(known.as_slice()))
    } else {
        make_map_of(ctx, &pairs)
    };
    freeze_result(ctx, UNMOD_MAP_CLASS, r)
}

/// One `put` into the fresh exact-`HashMap` backing `map` of an immutable
/// factory, for a key the factory's JDK walk has already hashed and proved
/// distinct from every earlier one: `raw_hash` (the key's Java `hashCode()`,
/// `Some` only where [`of_hash_is_java`]) stands in for a second `hashCode()`
/// call, and the chain is linked at its tail without comparing keys
/// ([`PutMode::assume_absent`]), so the build runs no user code. `None` hashes
/// natively, as `put` would. Every put of such a build comes through here, so
/// the integer overlay `put` may start is never started for it and nothing
/// needs materialising.
pub(crate) fn of_backing_put_absent(
    ctx: &mut dyn NativeContext,
    map: ObjectRef,
    key: Value,
    value: Value,
    raw_hash: Option<i32>,
) -> MethodCallResult {
    let mut this = map;
    let put_pin_base = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let value_pin = pin_value(ctx, value);
    let result = native_map_put_evict_pinned_inner(
        ctx,
        &mut this,
        key,
        value,
        true,
        put_pin_base,
        key_pin,
        value_pin,
        GrowAlloc::SingleAttempt,
        // `HashMap.hash`: the spread [`map_hash_key`] applies.
        raw_hash.map(|h| h ^ ((h as u32) >> 16) as i32),
        false,
        PutMode {
            no_grow: false,
            assume_absent: true,
        },
    );
    ctx.unpin_native_roots(put_pin_base);
    result
}

/// `CRATONVM_COMPAT_IMMUTABLE_OF_JDK_MINT` -- default OFF, `=1` turns it on.
/// Proposal CW2-1 of `jit-r15w2-compat-proposals-RETIRED-20261003.md`; see
/// [`jdk_mint_immutable`]. Off by default because it changes the CLASS of
/// every non-empty `List.of` / `Set.of` / `Map.of` / `copyOf` product under
/// `--compatible` (to HotSpot's), and with it `Set.of` / `Map.of` iteration
/// order (to HotSpot's per-run SALT order): what decides the flip is a
/// `--compatible` Spring / Tomcat run with it on.
pub(crate) fn immutable_of_jdk_mint_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_on("CRATONVM_COMPAT_IMMUTABLE_OF_JDK_MINT")
    })
}

/// A fresh `Object[]` holding `elems`, read back through their pins
/// (`handles`) after the allocation.
pub(crate) fn of_mint_array(
    ctx: &mut dyn NativeContext,
    elems: &[Value],
    handles: &[usize],
) -> Result<ObjectRef, MethodCallFailed> {
    let arr = alloc_ref_array_or_oom(ctx, elems.len())?;
    for (i, v) in elems.iter().enumerate() {
        let v = read_pinned_elem(&*ctx, handles[i], *v);
        ctx.set_array_element(arr, i, v);
    }
    Ok(arr)
}

/// CW2-1: the JDK's OWN immutable collection for a non-empty factory call of
/// `family` (`UNMOD_LIST_CLASS` / `UNMOD_SET_CLASS` / `UNMOD_MAP_CLASS`; for a
/// map `elems` is `k0, v0, k1, v1, ..`), built by the code JDK 25 runs for it:
/// `ImmutableCollections.listFromTrustedArray(Object[])` for a list,
/// `new Set12(e0[, e1])` / `new SetN(Object[])` for a set and
/// `new Map1(k, v)` / `new MapN(Object[])` for a map. Those constructors do
/// the null and duplicate checks with their own messages and call counts, and
/// the result is HotSpot's class with HotSpot's iteration order -- every item
/// of the residual page at once, `Set12` / `Map1`'s hash-free construction
/// included, which the native walk cannot give.
///
/// `None` -- no collection built, the caller builds its wrapper -- with the
/// switch off ([`immutable_of_jdk_mint_enabled`]), for no elements (the
/// canonical empties answer those), when a class or constructor is missing
/// (the synthetic JDK), or while `ImmutableCollections` is not usable yet: its
/// `EMPTY_MAP` static is the proof, being assigned after `EMPTY`, the
/// unused-slot sentinel `List12` / `Set12` store (a call from inside its
/// `<clinit>` would store null there). Otherwise the constructor's answer,
/// its exception included.
///
/// Class initialisation can collect, so a `None` that got that far fills
/// `refreshed` with `elems` read back through their pins: the caller builds
/// from `refreshed` when it is non-empty (its own `elems` may be stale), and
/// from `elems` when it is empty (nothing ran).
pub(crate) fn jdk_mint_immutable(
    ctx: &mut dyn NativeContext,
    family: &str,
    elems: &[Value],
    refreshed: &mut Vec<Value>,
) -> Option<MethodCallResult> {
    const IC: &str = "java/util/ImmutableCollections";
    const ONE: &str = "(Ljava/lang/Object;)V";
    const TWO: &str = "(Ljava/lang/Object;Ljava/lang/Object;)V";
    const ARR: &str = "([Ljava/lang/Object;)V";
    const LIST_DESC: &str = "([Ljava/lang/Object;)Ljava/util/List;";
    if !immutable_of_jdk_mint_enabled() || elems.is_empty() {
        return None;
    }
    // (class, constructor descriptor, array argument?) -- `None` class: the
    // list's static factory.
    let (class, desc, by_array): (Option<&str>, &str, bool) = if family == UNMOD_LIST_CLASS {
        (None, LIST_DESC, true)
    } else if family == UNMOD_SET_CLASS {
        match elems.len() {
            1 => (Some("java/util/ImmutableCollections$Set12"), ONE, false),
            2 => (Some("java/util/ImmutableCollections$Set12"), TWO, false),
            _ => (Some("java/util/ImmutableCollections$SetN"), ARR, true),
        }
    } else if family == UNMOD_MAP_CLASS {
        match elems.len() {
            2 => (Some("java/util/ImmutableCollections$Map1"), TWO, false),
            n if n % 2 == 0 => (Some("java/util/ImmutableCollections$MapN"), ARR, true),
            _ => return None,
        }
    } else {
        return None;
    };
    // Class initialisation, the array and the constructor all collect.
    let (base, handles) = pin_value_slice(ctx, elems);
    let ready = match ctx.ensure_class_initialized(IC) {
        Ok(cid) => ctx
            .static_field_index_by_name(cid, "EMPTY_MAP")
            .is_some_and(|idx| matches!(ctx.get_static_field(cid, idx), Value::Object(Some(_)))),
        Err(_) => false,
    } && match class {
        None => ctx.method_exists(IC, "listFromTrustedArray", LIST_DESC),
        Some(c) => ctx.ensure_class_initialized(c).is_ok() && ctx.method_exists(c, "<init>", desc),
    };
    let answer: Option<MethodCallResult> = if !ready {
        None
    } else if by_array {
        Some(match of_mint_array(ctx, elems, &handles) {
            Ok(arr) => {
                let args = [Value::Object(Some(arr))];
                match class {
                    // The array is the list's own storage from here on
                    // ("trusted"): nothing else holds it.
                    None => ctx.invoke(IC, "listFromTrustedArray", LIST_DESC, &args),
                    Some(c) => ctx.new_object_initialized(c, desc, &args),
                }
            }
            Err(e) => Err(e),
        })
    } else {
        let args = read_value_slice(&*ctx, &handles, elems);
        class.map(|c| ctx.new_object_initialized(c, desc, &args))
    };
    let built = match answer {
        Some(Ok(Some(Value::Object(Some(o))))) => Some(Ok(Some(Value::Object(Some(o))))),
        Some(Err(e)) => Some(Err(e)),
        // Not built (or no object came back): the caller's native build,
        // from the elements as they are now.
        _ => {
            *refreshed = read_value_slice(&*ctx, &handles, elems);
            None
        }
    };
    if base != usize::MAX {
        ctx.unpin_native_roots(base);
    }
    built
}

/// The one `List.of` body: every arity registration (registered only without a
/// real JDK since wave 48) and `CollSer.readResolve` reach it.
pub(crate) fn of_list(ctx: &mut dyn NativeContext, elems: &[Value]) -> MethodCallResult {
    // CW2-1 (default off): `listFromTrustedArray`.
    let mut refreshed = Vec::new();
    if let Some(r) = jdk_mint_immutable(ctx, UNMOD_LIST_CLASS, elems, &mut refreshed) {
        return r;
    }
    let elems: &[Value] = if refreshed.is_empty() {
        elems
    } else {
        &refreshed
    };
    of_reject_null_elements(elems)?;
    // C4-3: `List.of()` is the JDK's one `EMPTY_LIST`.
    if elems.is_empty() {
        if let Some(empty) = canonical_empty_immutable(ctx, UNMOD_LIST_CLASS) {
            return Ok(Some(Value::Object(Some(empty))));
        }
    }
    let r = make_list_of(ctx, elems);
    freeze_result(ctx, UNMOD_LIST_CLASS, r)
}

/// The one `Set.of` body. With [`immutable_of_jdk_walk_enabled`] the checks
/// are the JDK constructors' single ordered walk ([`of_set_jdk`]); with it
/// off, nulls first, then duplicates (`Set.of("a", null)` is an NPE, but so is
/// `Set.of("a", "a", null)`, which is an IAE on the JDK).
pub(crate) fn of_set(ctx: &mut dyn NativeContext, elems: &[Value]) -> MethodCallResult {
    // CW2-1 (default off): `new Set12(..)` / `new SetN(..)`.
    let mut refreshed = Vec::new();
    if let Some(r) = jdk_mint_immutable(ctx, UNMOD_SET_CLASS, elems, &mut refreshed) {
        return r;
    }
    let elems: &[Value] = if refreshed.is_empty() {
        elems
    } else {
        &refreshed
    };
    if immutable_of_jdk_walk_enabled() {
        return of_set_jdk(ctx, elems);
    }
    of_reject_null_elements(elems)?;
    // C4-3: `Set.of()` is the JDK's one `EMPTY_SET`.
    if elems.is_empty() {
        if let Some(empty) = canonical_empty_immutable(ctx, UNMOD_SET_CLASS) {
            return Ok(Some(Value::Object(Some(empty))));
        }
    }
    // gc-common w20-a: the duplicate check runs `hashCode`/`equals` (Java) and
    // re-reads ITS pins, but the caller's `elems` were then handed to
    // `make_set_of` at their pre-check addresses. Pin across the check and
    // build from the read-back values.
    let (base, handles) = pin_value_slice(ctx, elems);
    let verdict = of_reject_duplicates(ctx, elems, "element");
    let elems = read_value_slice(ctx, &handles, elems);
    if base != usize::MAX {
        ctx.unpin_native_roots(base);
    }
    verdict?;
    let r = make_set_of(ctx, &elems);
    freeze_result(ctx, UNMOD_SET_CLASS, r)
}

/// The one `Map.of` / `Map.ofEntries` body. Keys AND values must be non-null;
/// only keys are checked for duplicates. [`of_map_jdk`] under
/// [`immutable_of_jdk_walk_enabled`] (per pair, in order); with it off, every
/// null first.
pub(crate) fn of_map(ctx: &mut dyn NativeContext, pairs: &[(Value, Value)]) -> MethodCallResult {
    let flat: Vec<Value> = pairs.iter().flat_map(|(k, v)| [*k, *v]).collect();
    // CW2-1 (default off): `new Map1(k, v)` / `new MapN(..)`.
    let mut refreshed = Vec::new();
    if let Some(r) = jdk_mint_immutable(ctx, UNMOD_MAP_CLASS, &flat, &mut refreshed) {
        return r;
    }
    let (flat, pairs_now): (Vec<Value>, Vec<(Value, Value)>) = if refreshed.is_empty() {
        (flat, pairs.to_vec())
    } else {
        let pairs_now = refreshed.chunks_exact(2).map(|kv| (kv[0], kv[1])).collect();
        (refreshed, pairs_now)
    };
    let pairs: &[(Value, Value)] = &pairs_now;
    if immutable_of_jdk_walk_enabled() {
        return of_map_jdk(ctx, pairs);
    }
    of_reject_null_elements(&flat)?;
    // C4-3: `Map.of()` / `Map.ofEntries()` is the JDK's one `EMPTY_MAP`.
    if pairs.is_empty() {
        if let Some(empty) = canonical_empty_immutable(ctx, UNMOD_MAP_CLASS) {
            return Ok(Some(Value::Object(Some(empty))));
        }
    }
    let keys: Vec<Value> = pairs.iter().map(|(k, _)| *k).collect();
    // gc-common w20-a: the key check runs `hashCode`/`equals` (Java); the keys
    // AND values were handed to `make_map_of` at their pre-check addresses.
    // Pin every key and value across it and build from the read-back rows.
    let (base, handles) = pin_value_slice(ctx, &flat);
    let verdict = of_reject_duplicates(ctx, &keys, "key");
    let flat = read_value_slice(ctx, &handles, &flat);
    if base != usize::MAX {
        ctx.unpin_native_roots(base);
    }
    verdict?;
    let pairs: Vec<(Value, Value)> = flat.chunks_exact(2).map(|kv| (kv[0], kv[1])).collect();
    let r = make_map_of(ctx, &pairs);
    freeze_result(ctx, UNMOD_MAP_CLASS, r)
}

/// Does `src` (any `Collection`) hold a null element? Asked through `toArray()`
/// so it also answers for a collection this crate does not own.
pub(crate) fn source_collection_has_null(ctx: &mut dyn NativeContext, src: ObjectRef) -> bool {
    let arr = match ctx.invoke_virtual(src, "toArray", "()[Ljava/lang/Object;", &[]) {
        Ok(Some(Value::Object(Some(a)))) => a,
        _ => return false,
    };
    let len = ctx.array_length(arr);
    (0..len).any(|i| matches!(ctx.get_array_element(arr, i), Value::Object(None)))
}

/// `CRATONVM_COMPAT_COPYOF_REAL_IMMUTABLE_IDENTITY` -- default ON, `=0`
/// restores the copy. See [`real_immutable_copy_identity`].
pub(crate) fn copyof_real_immutable_identity_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_COPYOF_REAL_IMMUTABLE_IDENTITY")
}

/// `v` itself when it is a REAL JDK immutable collection that the JDK's
/// `copyOf` hands back unchanged (JDK 25):
///
/// * `List.copyOf` (`ImmutableCollections.listCopy`): `coll instanceof List12
///   || (coll instanceof ListN c && !c.allowNulls)` -- a `Stream.toList()`
///   `ListN` allows nulls and is copied;
/// * `Set.copyOf`: `coll instanceof ImmutableCollections.AbstractImmutableSet`;
/// * `Map.copyOf`: `map instanceof ImmutableCollections.AbstractImmutableMap`.
///
/// The natives short-circuited only this crate's own wrapper
/// ([`already_immutable_of`]), so a real `ListN` built by JDK bytecode or by a
/// native (`MemoryLayout.structLayout(..).memberLayouts()`) was copied into a
/// `cratonvm/internal/UnmodifiableList`: `List.copyOf(x) == x` answered
/// `false` where HotSpot answers `true`, and a real JDK constructor that
/// stores `List.copyOf(arg)` held the wrapper. `class_name` is the wrapper
/// class the caller asked about, which names the family. Pure reads; `None`
/// with the switch off. Round 14 wave 4 (lane compat4).
pub(crate) fn real_immutable_copy_identity(
    ctx: &dyn NativeContext,
    v: Value,
    class_name: &str,
) -> Option<ObjectRef> {
    let Value::Object(Some(obj)) = v else {
        return None;
    };
    if !copyof_real_immutable_identity_enabled() {
        return None;
    }
    let cid = ctx.class_id_of_object(obj);
    let is_a = |base: &str| {
        ctx.class_id_by_name(base)
            .is_some_and(|b| cid == b || ctx.is_subclass(cid, b))
    };
    let same = if class_name == UNMOD_LIST_CLASS {
        match ctx.class_name_arc_of_id(cid).as_deref() {
            Some("java/util/ImmutableCollections$List12") => true,
            Some("java/util/ImmutableCollections$ListN") => {
                // `allowNulls` must resolve and read false; anything else
                // keeps the copy.
                ctx.resolve_field_index_by_class_id(cid, "allowNulls")
                    .filter(|&slot| slot < ctx.object_num_fields(obj))
                    .is_some_and(|slot| matches!(ctx.get_field(obj, slot), Value::Int(0)))
            }
            _ => false,
        }
    } else if class_name == UNMOD_SET_CLASS {
        is_a("java/util/ImmutableCollections$AbstractImmutableSet")
    } else if class_name == UNMOD_MAP_CLASS {
        is_a("java/util/ImmutableCollections$AbstractImmutableMap")
    } else {
        false
    };
    same.then_some(obj)
}

/// Is `v` already an immutable `*.of` / `copyOf` product of `class_name`?
///
/// `UNMOD_FIELD_IMMUTABLE` is the discriminator the wrapper already carried:
/// `alloc_immutable_wrapper` sets it and `alloc_unmod_wrapper` — which is what
/// `Collections.unmodifiableList` uses — does not. That is exactly the JDK's
/// `coll instanceof ImmutableCollections.AbstractImmutableList` test, and it is
/// why `copyOf` may return its argument unchanged here while still snapshotting
/// a `Collections.unmodifiableList` VIEW of a mutable list: the view's contents
/// can still change underneath it, which is the aliasing the Kafka
/// `AdminApiDriver` fix above was about.
pub(crate) fn already_immutable_of(
    ctx: &mut dyn NativeContext,
    v: Value,
    class_name: &str,
) -> Option<ObjectRef> {
    // A REAL JDK immutable collection too (round 14 wave 4, lane compat4;
    // `r14w4-ffm4-compat-list-copyof-of-a-real-immutable-list-FIXED-20260929.md`),
    // ahead of this crate's own wrapper test below.
    if let Some(same) = real_immutable_copy_identity(&*ctx, v, class_name) {
        return Some(same);
    }
    let obj = match v {
        Value::Object(Some(o)) => o,
        _ => return None,
    };
    let cid = ctx.class_id_of_object(obj);
    if ctx.class_name_of_id(cid).as_deref() != Some(class_name) {
        return None;
    }
    match ctx.get_field(obj, UNMOD_FIELD_IMMUTABLE) {
        Value::Int(1) => Some(obj),
        _ => None,
    }
}

/// `CRATONVM_COMPAT_IMMUTABLE_EMPTY_CANONICAL` -- default ON, `=0` restores a
/// fresh wrapper per empty factory call. See [`canonical_empty_immutable`].
pub(crate) fn immutable_empty_canonical_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_IMMUTABLE_EMPTY_CANONICAL")
}

/// The JDK's one shared empty immutable collection of `class_name`'s family
/// (`UNMOD_LIST_CLASS` / `UNMOD_SET_CLASS` / `UNMOD_MAP_CLASS`):
/// `ImmutableCollections.EMPTY_LIST` / `EMPTY_SET` / `EMPTY_MAP`, a real
/// `ListN` / `SetN` / `MapN`, read from the class's own static (per VM).
///
/// JDK 25 answers exactly that object from `List.of()`, `List.of(E...)` of a
/// zero-length array, `Set.of()`, `Set.of(E...)`, `Map.of()`,
/// `Map.ofEntries()` and -- through `listCopy` / `Set.copyOf` / `Map.copyOf`'s
/// `isEmpty()` shortcut -- from every `copyOf` of an empty non-immutable
/// argument, so `List.copyOf(new ArrayList<>()) == List.of()` and
/// `Set.of() == Set.of()` are `true` on HotSpot. The natives minted a fresh
/// `cratonvm/internal/Unmodifiable*` wrapper each time (`false`). The real
/// object is the right answer here too: an empty real `ListN` / `SetN` /
/// `MapN` holds no state a native would have to read (a map native that
/// cannot read its `table` sees it as what it is, empty, and the collection
/// natives delegate to a real receiver's own bytecode), and its class is the
/// one `getClass()` already reported for an empty wrapper.
///
/// `None` -- the caller keeps the fresh wrapper -- with the switch off, when
/// the class does not exist (the synthetic JDK), fails to initialise, is being
/// initialised by this thread (its static is still null), or holds anything
/// but the expected real class. Initialising `ImmutableCollections` here is
/// what HotSpot does at the same call (`List.of()` reads its static).
/// Round 15 wave 2 (lane compat), proposal C4-3 of `jit-r14-compat4-proposals-RETIRED-20260930.md`.
pub(crate) fn canonical_empty_immutable(ctx: &mut dyn NativeContext, class_name: &str) -> Option<ObjectRef> {
    if !immutable_empty_canonical_enabled() {
        return None;
    }
    let (field, expect) = if class_name == UNMOD_LIST_CLASS {
        ("EMPTY_LIST", "java/util/ImmutableCollections$ListN")
    } else if class_name == UNMOD_SET_CLASS {
        ("EMPTY_SET", "java/util/ImmutableCollections$SetN")
    } else if class_name == UNMOD_MAP_CLASS {
        ("EMPTY_MAP", "java/util/ImmutableCollections$MapN")
    } else {
        return None;
    };
    let cid = ctx
        .ensure_class_initialized("java/util/ImmutableCollections")
        .ok()?;
    let idx = ctx.static_field_index_by_name(cid, field)?;
    let Value::Object(Some(obj)) = ctx.get_static_field(cid, idx) else {
        return None;
    };
    (ctx.class_name_arc_of_id(ctx.class_id_of_object(obj))
        .as_deref()
        == Some(expect))
    .then_some(obj)
}

/// `copyOf(null)`'s refusal. JDK 25's `listCopy` / `Set.copyOf` /
/// `Map.copyOf` test `instanceof` (null-safe) and then dereference the
/// argument with `isEmpty()`, so HotSpot's NPE carries the helpful message of
/// that `invokeinterface` (`message`); the native threw it bare. Under C4-3's
/// switch (the same `isEmpty()` shape); with it off, the bare NPE as before.
/// A MISSING argument stays the malformed-call no-op, as in
/// [`reject_null_collection`].
pub(crate) fn reject_null_copy_of_arg(arg: Option<&Value>, message: &str) -> Result<(), MethodCallFailed> {
    if immutable_empty_canonical_enabled() && matches!(arg, Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException {
            message: Some(message.to_string()),
        }
        .into());
    }
    reject_null_collection(arg)
}

/// C4-3's `copyOf` shortcut, asked after the already-immutable identity test:
/// `coll.isEmpty()` (the JDK's implicit null check and the only question it
/// asks of an empty argument), and when that answers `true`, the canonical
/// empty collection ([`canonical_empty_immutable`]). Answers `(Some(empty),
/// _)` to return, or `(None, src)` to take the copying path with `src`
/// re-read (`isEmpty()` and the class initialisation are Java and can move
/// it); `Err` when `isEmpty()` threw, which the JDK propagates. With the switch
/// off it asks nothing and hands `src` back untouched, so the old path runs
/// byte-identical. An empty argument no longer has `toArray()` called on it,
/// which is also the JDK's call sequence.
pub(crate) fn copy_of_empty_pinned(
    ctx: &mut dyn NativeContext,
    src: Value,
    class_name: &str,
) -> Result<(Option<ObjectRef>, Value), MethodCallFailed> {
    if !immutable_empty_canonical_enabled() {
        return Ok((None, src));
    }
    let Value::Object(Some(s)) = src else {
        return Ok((None, src));
    };
    let src_pin = ctx.pin_native_root(s);
    let asked = ctx.invoke_virtual(s, "isEmpty", "()Z", &[]);
    let empty = match asked {
        Ok(Some(Value::Int(v))) if v != 0 => canonical_empty_immutable(ctx, class_name),
        Ok(_) => None,
        Err(e) => {
            ctx.unpin_native_roots(src_pin);
            return Err(e);
        }
    };
    let s = ctx.read_native_pin(src_pin, s);
    ctx.unpin_native_roots(src_pin);
    Ok((empty, Value::Object(Some(s))))
}

/// `CRATONVM_COMPAT_COPYOF_ONE_PASS` -- default ON, `=0` restores the null
/// scan plus the copy. `List.copyOf` / `Set.copyOf` of a non-empty argument
/// asked `toArray()` once for a null scan ([`source_collection_has_null`],
/// which SWALLOWED an exception) and then collected the elements again for
/// the copy, so a counting or throwing `toArray()` ran twice. JDK 25 is one
/// pass: `List.of(coll.toArray())` ([`list_copy_of_one_pass`]),
/// `Set.of(new HashSet<>(coll).toArray())` ([`set_copy_of_one_pass`]) and
/// `Map.ofEntries(map.entrySet().toArray(..))` (`native_map_copy_of`), each
/// validated by the `of` it ends in. Round 15 wave 3 (lane compat2), proposal
/// CW2-5 of `jit-r15w2-compat-proposals-RETIRED-20261003.md`.
pub(crate) fn copyof_one_pass_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_COPYOF_ONE_PASS")
}

/// CW2-5's `List.copyOf` after the identity and empty shortcuts:
/// `List.of(coll.toArray())` -- ONE `toArray()`, its exception propagated, the
/// list built from its answer by [`of_list`] (a null element is `List12` /
/// `listFromArray`'s bare NPE). `None` with the switch off or when `toArray()`
/// answered no array at all (nothing to call); `src` is re-read either way,
/// since the call is Java.
pub(crate) fn list_copy_of_one_pass(ctx: &mut dyn NativeContext, src: &mut Value) -> Option<MethodCallResult> {
    if !copyof_one_pass_enabled() {
        return None;
    }
    let Value::Object(Some(s)) = *src else {
        return None;
    };
    let pin = ctx.pin_native_root(s);
    let answer = ctx.invoke_virtual(s, "toArray", "()[Ljava/lang/Object;", &[]);
    *src = Value::Object(Some(ctx.read_native_pin(pin, s)));
    ctx.unpin_native_roots(pin);
    match answer {
        Err(e) => Some(Err(e)),
        Ok(Some(Value::Object(Some(arr)))) if ctx.heap_kind_of(arr) == ObjectKind::Array => {
            // Pure reads up to `of_list`, which pins the elements first.
            let len = ctx.array_length(arr);
            let elems: Vec<Value> = (0..len).map(|i| ctx.get_array_element(arr, i)).collect();
            Some(of_list(ctx, &elems))
        }
        // `List.of((Object[]) null)`: `switch (elements.length)` throws.
        Ok(Some(Value::Object(None))) => Some(Err(of_npe(
            "Cannot read the array length because \"elements\" is null",
        ))),
        Ok(_) => None,
    }
}

/// CW2-5's `Set.copyOf` after the identity and empty shortcuts, JDK 25's
/// `Set.of(new HashSet<>(coll).toArray())`: the `HashSet` copy (each element
/// hashed once, repeats folded by `putVal`'s own test, a null kept), then
/// `Set.of` over its table-order keys -- `Set12` / `SetN`'s checks
/// ([`of_set_jdk_check`]), so a null is an NPE whose message follows the
/// DEDUPLICATED arity and a `SetN` hashes every element again, as on HotSpot.
/// The set just built is the backing. No null scan by `toArray()`.
///
/// Under CW2-1 ([`jdk_mint_immutable`]) the second half is the JDK's own
/// `Set12` / `SetN` over the same keys.
pub(crate) fn set_copy_of_one_pass(ctx: &mut dyn NativeContext, src: Value) -> MethodCallResult {
    // GC-safety: the allocation, the copy-in and the walk all collect; `src`
    // and `backing` are rooted across each (as in the copying path).
    let src_pin = pin_value(ctx, src);
    let backing = match try_alloc_synthetic(ctx, "java/util/HashSet", HS_NUM_FIELDS) {
        Ok(b) => b,
        Err(e) => {
            if src_pin != usize::MAX {
                ctx.unpin_native_roots(src_pin);
            }
            return Err(e);
        }
    };
    let src = read_pinned_elem(ctx, src_pin, src);
    let mut backing = backing;
    let backing_at_call = backing;
    let copied = rooted_across(ctx, &mut [&mut backing], |ctx| {
        native_hs_init_from_collection(ctx, &[Value::Object(Some(backing_at_call)), src])
    });
    if src_pin != usize::MAX {
        ctx.unpin_native_roots(src_pin);
    }
    copied?;
    let backing_pin = ctx.pin_native_root(backing);
    // `HashSet.toArray()` is `map.keysToArray(..)`: the keys in table order.
    // Pure reads of the set this function built.
    let elems: Vec<Value> = match hs_backing_map(&*ctx, backing) {
        Some(map) => map_collect_entries(&*ctx, map)
            .into_iter()
            .map(|(k, _)| k)
            .collect(),
        None => Vec::new(),
    };
    let (_, handles) = pin_value_slice(ctx, &elems);
    let mut refreshed = Vec::new();
    let minted = jdk_mint_immutable(ctx, UNMOD_SET_CLASS, &elems, &mut refreshed);
    let checked = if minted.is_some() {
        Ok(Vec::new())
    } else {
        // `handles` are this function's pins of `elems`, so the walk reads
        // current addresses whichever slice it is handed.
        let elems: &[Value] = if refreshed.is_empty() {
            &elems
        } else {
            &refreshed
        };
        if immutable_of_jdk_walk_enabled() {
            of_set_jdk_check(ctx, elems, &handles)
        } else {
            // The walk switched off: the old bare NPE for any null.
            of_reject_null_elements(elems).map(|()| Vec::new())
        }
    };
    // `Set.of(new Object[0])` -- a source that answered `isEmpty()` false
    // and then held nothing -- is the JDK's `EMPTY_SET`. Asked while
    // `backing` is still pinned (it can initialise a class).
    let empty = if minted.is_none() && checked.is_ok() && elems.is_empty() {
        canonical_empty_immutable(ctx, UNMOD_SET_CLASS)
    } else {
        None
    };
    let backing = ctx.read_native_pin(backing_pin, backing);
    // Releases the element pins too (the group's base).
    ctx.unpin_native_roots(backing_pin);
    if let Some(r) = minted {
        return r;
    }
    checked?;
    if let Some(empty) = empty {
        return Ok(Some(Value::Object(Some(empty))));
    }
    Ok(Some(Value::Object(Some(alloc_immutable_wrapper(
        ctx,
        UNMOD_SET_CLASS,
        backing,
    )?))))
}

/// A marked `toSet()` collector's accumulator `v` frozen as JDK 25's
/// `toUnmodifiableSet` finisher freezes it: `Set.of(set.toArray())` -- the
/// table-order keys through [`of_set`] -- rather than `Set.copyOf`, which
/// first hashes every element into another `HashSet`. `None` when `v` is not
/// a set with a native backing map (the caller copies). Under
/// [`immutable_of_jdk_walk_enabled`].
pub(crate) fn set_of_collected(ctx: &mut dyn NativeContext, v: Value) -> Option<MethodCallResult> {
    if !immutable_of_jdk_walk_enabled() {
        return None;
    }
    let Value::Object(Some(set)) = v else {
        return None;
    };
    let map = hs_backing_map(&*ctx, set)?;
    // Pure reads; `of_set` pins the keys before anything collects.
    let elems: Vec<Value> = map_collect_entries(&*ctx, map)
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    Some(of_set(ctx, &elems))
}

pub(crate) fn native_list_of_varargs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let elems: Vec<Value> = args.to_vec();
    of_list(ctx, &elems)
}

/// Generic `Set.of` for fixed-arity overloads. Produces a frozen set.
pub(crate) fn native_set_of_varargs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let elems: Vec<Value> = args.to_vec();
    of_set(ctx, &elems)
}

/// Generic `Map.of` for fixed-arity overloads: args are k0,v0,k1,v1,...
pub(crate) fn native_map_of_varargs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut pairs: Vec<(Value, Value)> = Vec::with_capacity(args.len() / 2);
    let mut i = 0;
    while i + 1 < args.len() {
        pairs.push((args[i], args[i + 1]));
        i += 2;
    }
    of_map(ctx, &pairs)
}

/// `Map.ofEntries(Map$Entry...)`.
///
/// Each element is asked for its key and value through `getKey()`/`getValue()`
/// rather than read at slots 0/1. Since `Map.entry` stopped being registered
/// (see the note at its old registration site) the arguments are real
/// `java.util.KeyValueHolder`s, whose layout this crate does not own and must
/// not assume — and a caller may pass any `Map.Entry` implementation at all,
/// including one the application wrote, where a positional read is not merely
/// fragile but wrong.
pub(crate) fn native_map_of_entries(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let jdk_nulls = immutable_of_null_checks_enabled();
    let arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        // `entries.length` is `ofEntries`' first act.
        Some(Value::Object(None)) if jdk_nulls => {
            return Err(of_npe(
                "Cannot read the array length because \"entries\" is null",
            ));
        }
        _ => {
            return of_map(ctx, &[]);
        }
    };
    let len = ctx.array_length(arr);
    // gc-common w20-a: `getKey`/`getValue` are Java calls. The array, the
    // entry between its two calls, and every key and value collected so far
    // were held raw across them and handed to `of_map` at pre-move addresses.
    // All of them are pinned now (`arr_pin` is the frame's base) and read back
    // before use.
    let arr_pin = ctx.pin_native_root(arr);
    let mut pairs: Vec<(Value, Value)> = Vec::with_capacity(len);
    let mut pair_pins: Vec<(usize, usize)> = Vec::with_capacity(len);
    for i in 0..len {
        let arr_now = ctx.read_native_pin(arr_pin, arr);
        if let Value::Object(Some(entry)) = ctx.get_array_element(arr_now, i) {
            let entry_pin = ctx.pin_native_root(entry);
            let k = match ctx.invoke_virtual(entry, "getKey", "()Ljava/lang/Object;", &[]) {
                Ok(k) => k.unwrap_or(Value::Object(None)),
                // A throwing `getKey()` propagates on the JDK; it used to be
                // swallowed into a null key (and then a bare NPE).
                Err(e) if jdk_nulls => {
                    ctx.unpin_native_roots(arr_pin);
                    return Err(e);
                }
                Err(_) => Value::Object(None),
            };
            let k_pin = pin_value(ctx, k);
            let entry = ctx.read_native_pin(entry_pin, entry);
            let v = match ctx.invoke_virtual(entry, "getValue", "()Ljava/lang/Object;", &[]) {
                Ok(v) => v.unwrap_or(Value::Object(None)),
                Err(e) if jdk_nulls => {
                    ctx.unpin_native_roots(arr_pin);
                    return Err(e);
                }
                Err(_) => Value::Object(None),
            };
            let v_pin = pin_value(ctx, v);
            pairs.push((k, v));
            pair_pins.push((k_pin, v_pin));
        } else if jdk_nulls {
            // A null slot: the JDK dereferences it (`entries[0].getKey()` for
            // one entry, the for-each local `entry.getKey()` for more); the
            // native skipped it, so `Map.ofEntries(e, null)` answered `{e}`.
            ctx.unpin_native_roots(arr_pin);
            return Err(of_npe(if len == 1 {
                "Cannot invoke \"java.util.Map$Entry.getKey()\" because \"entries[0]\" is null"
            } else {
                "Cannot invoke \"java.util.Map$Entry.getKey()\" because \"entry\" is null"
            }));
        }
    }
    let pairs: Vec<(Value, Value)> = pairs
        .iter()
        .zip(pair_pins.iter())
        .map(|(&(k, v), &(kp, vp))| (read_pinned_elem(ctx, kp, k), read_pinned_elem(ctx, vp, v)))
        .collect();
    let out = of_map(ctx, &pairs);
    ctx.unpin_native_roots(arr_pin);
    out
}

/// Helper: create an ArrayList from a slice of values.
pub(crate) fn make_list_of(ctx: &mut dyn NativeContext, elems: &[Value]) -> MethodCallResult {
    let __al_n_fields = al_slots(ctx).2;
    let (elem_base, elem_handles) = pin_value_slice(ctx, elems);
    let list = try_alloc_synthetic(ctx, "java/util/ArrayList", __al_n_fields)?;
    let list_pin = ctx.pin_native_root(list);
    let cap = std::cmp::max(elems.len(), AL_DEFAULT_CAPACITY);
    let buf = alloc_ref_array(ctx, cap);
    let buf_pin = ctx.pin_native_root(buf);
    for (i, val) in elems.iter().enumerate() {
        let buf = ctx.read_native_pin(buf_pin, buf);
        let val = read_pinned_elem(ctx, elem_handles[i], *val);
        ctx.set_array_element(buf, i, val);
    }
    let list = ctx.read_native_pin(list_pin, list);
    let buf = ctx.read_native_pin(buf_pin, buf);
    al_set_data(ctx, list, buf);
    al_set_size(ctx, list, elems.len() as i32);
    ctx.unpin_native_roots(if elem_base == usize::MAX {
        list_pin
    } else {
        elem_base
    });
    Ok(Some(Value::Object(Some(list))))
}

/// Helper: create an ArrayList (returns ObjectRef, not MethodCallResult).
pub(crate) fn make_list_of_raw(
    ctx: &mut dyn NativeContext,
    elems: &[Value],
) -> Result<ObjectRef, MethodCallFailed> {
    let __al_n_fields = al_slots(ctx).2;
    let (elem_base, elem_handles) = pin_value_slice(ctx, elems);
    let list = try_alloc_synthetic(ctx, "java/util/ArrayList", __al_n_fields)?;
    let list_pin = ctx.pin_native_root(list);
    let cap = std::cmp::max(elems.len(), AL_DEFAULT_CAPACITY);
    let buf = alloc_ref_array(ctx, cap);
    let buf_pin = ctx.pin_native_root(buf);
    for (i, val) in elems.iter().enumerate() {
        let buf = ctx.read_native_pin(buf_pin, buf);
        let val = read_pinned_elem(ctx, elem_handles[i], *val);
        ctx.set_array_element(buf, i, val);
    }
    let list = ctx.read_native_pin(list_pin, list);
    let buf = ctx.read_native_pin(buf_pin, buf);
    al_set_data(ctx, list, buf);
    al_set_size(ctx, list, elems.len() as i32);
    ctx.unpin_native_roots(if elem_base == usize::MAX {
        list_pin
    } else {
        elem_base
    });
    Ok(list)
}

/// Helper: create a HashSet from a slice of values.
pub(crate) fn make_set_of(ctx: &mut dyn NativeContext, elems: &[Value]) -> MethodCallResult {
    make_set_of_known(ctx, elems, None)
}

/// [`make_set_of`]; with `known`, every element is linked through
/// [`of_backing_put_absent`] with its walk hash (`known[i]`), for a factory
/// whose JDK walk ([`of_set_jdk`]) already hashed the elements and proved
/// them distinct.
pub(crate) fn make_set_of_known(
    ctx: &mut dyn NativeContext,
    elems: &[Value],
    known: Option<&[Option<i32>]>,
) -> MethodCallResult {
    // Building the synthetic set allocates both its backing map and bucket
    // array, and each native_map_put may run Java equality/hash code and move
    // the heap. Keep every input element and partially-built object rooted,
    // refreshing their addresses immediately before use. A raw stream element
    // here previously became an unrelated java.lang.Object while collecting
    // TypeElement values with Collectors.toSet().
    let (elem_base, elem_handles) = pin_value_slice(ctx, elems);
    let set = try_alloc_synthetic(ctx, "java/util/HashSet", HS_NUM_FIELDS)?;
    let set_pin = ctx.pin_native_root(set);
    let backing_map = alloc_backing_map(ctx);
    let backing_map_pin = ctx.pin_native_root(backing_map);
    // `CRATONVM_COMPAT_SET_OF_START_TABLE`: `new HashSet<>()`'s 16 slots, grown
    // by the puts below as the adds grow it.
    let cap = if set_of_start_table_enabled() {
        MAP_DEFAULT_CAPACITY
    } else {
        std::cmp::max(elems.len().next_power_of_two(), MAP_DEFAULT_CAPACITY)
    };
    let buckets = alloc_ref_array(ctx, cap);
    let buckets_pin = ctx.pin_native_root(buckets);
    let backing_map = ctx.read_native_pin(backing_map_pin, backing_map);
    let buckets = ctx.read_native_pin(buckets_pin, buckets);
    publish_map_table(ctx, backing_map, buckets, cap as i32);
    set_map_size(ctx, backing_map, 0);
    let set = ctx.read_native_pin(set_pin, set);
    let backing_map = ctx.read_native_pin(backing_map_pin, backing_map);
    hs_set_backing_map(ctx, set, backing_map);

    // Dead `let sentinel = Value::Int(1)` removed: the marker has come from
    // `present_marker(elem)` since that helper landed, and leaving the old
    // binding in the loop's header made it read as the live one. The receiver
    // built here is a `java/util/HashSet`, so it takes the real `PRESENT` —
    // see [`hs_present_marker_at`].
    let present_slot = hs_present_slot(&*ctx);
    for (index, elem) in elems.iter().enumerate() {
        let backing_map = ctx.read_native_pin(backing_map_pin, backing_map);
        let elem = read_pinned_elem(ctx, elem_handles[index], *elem);
        let present = hs_present_marker_at(&*ctx, present_slot, elem);
        let put = match known {
            Some(known) => {
                let hash = known.get(index).copied().flatten();
                of_backing_put_absent(ctx, backing_map, elem, present, hash)
            }
            None => native_map_put(ctx, &[Value::Object(Some(backing_map)), elem, present]),
        };
        if let Err(err) = put {
            ctx.unpin_native_roots(if elem_base == usize::MAX {
                set_pin
            } else {
                elem_base
            });
            return Err(err);
        }
    }

    let set = ctx.read_native_pin(set_pin, set);
    ctx.unpin_native_roots(if elem_base == usize::MAX {
        set_pin
    } else {
        elem_base
    });
    Ok(Some(Value::Object(Some(set))))
}

/// Helper: create a HashMap from key-value pairs.
pub(crate) fn make_map_of(ctx: &mut dyn NativeContext, pairs: &[(Value, Value)]) -> MethodCallResult {
    make_map_of_known(ctx, pairs, None)
}

/// [`make_map_of`]; with `known`, as [`make_set_of_known`] (per pair: the
/// key's walk hash, from [`of_map_jdk`]).
pub(crate) fn make_map_of_known(
    ctx: &mut dyn NativeContext,
    pairs: &[(Value, Value)],
    known: Option<&[Option<i32>]>,
) -> MethodCallResult {
    // cceres3: pin across GC-capable call (stream stale-at-store wave) — each
    // `native_map_put` re-enters Java (hashCode/equals) and allocates, so the
    // map and every not-yet-stored pair must be re-read through pins before
    // each put (mirrors `make_set_of`, which already had this discipline). The
    // map itself was also stored-to via `set_field` after the buckets
    // allocation without a pin.
    let flat: Vec<Value> = pairs.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (pair_base, pair_handles) = pin_value_slice(ctx, &flat);
    let map = alloc_backing_map(ctx);
    let map_pin = ctx.pin_native_root(map);
    let cap = std::cmp::max(pairs.len().next_power_of_two(), MAP_DEFAULT_CAPACITY);
    let buckets = alloc_ref_array(ctx, cap);
    let map = ctx.read_native_pin(map_pin, map);
    publish_map_table(ctx, map, buckets, cap as i32);
    set_map_size(ctx, map, 0);

    for (i, (key, value)) in pairs.iter().enumerate() {
        let map = ctx.read_native_pin(map_pin, map);
        let key = read_pinned_elem(ctx, pair_handles[2 * i], *key);
        let value = read_pinned_elem(ctx, pair_handles[2 * i + 1], *value);
        let put = match known {
            Some(known) => {
                let hash = known.get(i).copied().flatten();
                of_backing_put_absent(ctx, map, key, value, hash)
            }
            None => native_map_put(ctx, &[Value::Object(Some(map)), key, value]),
        };
        if let Err(e) = put {
            ctx.unpin_native_roots(if pair_base == usize::MAX {
                map_pin
            } else {
                pair_base
            });
            return Err(e);
        }
    }

    let map = ctx.read_native_pin(map_pin, map);
    ctx.unpin_native_roots(if pair_base == usize::MAX {
        map_pin
    } else {
        pair_base
    });
    Ok(Some(Value::Object(Some(map))))
}

/// Wrap a `MethodCallResult` holding a collection ObjectRef in an
/// unmodifiable view of `wrapper_class`. Used by the `List.of` / `Set.of` /
/// `Map.of` immutable factories — the backing collection is private so the
/// snapshot can never be mutated, and every mutator throws.
pub(crate) fn freeze_result(
    ctx: &mut dyn NativeContext,
    wrapper_class: &str,
    result: MethodCallResult,
) -> MethodCallResult {
    match result? {
        Some(Value::Object(Some(backing))) => {
            // Immutable `*.of` product → `java.util.ImmutableCollections$*`.
            let w = alloc_immutable_wrapper(ctx, wrapper_class, backing)?;
            Ok(Some(Value::Object(Some(w))))
        }
        other => Ok(other),
    }
}

pub(crate) fn native_list_of_0(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    of_list(ctx, &[])
}

pub(crate) fn native_list_of_1(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let e1 = args.first().copied().unwrap_or(Value::Object(None));
    of_list(ctx, &[e1])
}

pub(crate) fn native_list_of_2(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let e1 = args.first().copied().unwrap_or(Value::Object(None));
    let e2 = args.get(1).copied().unwrap_or(Value::Object(None));
    of_list(ctx, &[e1, e2])
}

pub(crate) fn native_list_of_3(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let e1 = args.first().copied().unwrap_or(Value::Object(None));
    let e2 = args.get(1).copied().unwrap_or(Value::Object(None));
    let e3 = args.get(2).copied().unwrap_or(Value::Object(None));
    of_list(ctx, &[e1, e2, e3])
}

pub(crate) fn native_list_of_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        // `List.of((Object[]) null)`: `switch (elements.length)` throws.
        Some(Value::Object(None)) if immutable_of_null_checks_enabled() => {
            return Err(of_npe(
                "Cannot read the array length because \"elements\" is null",
            ));
        }
        _ => {
            return of_list(ctx, &[]);
        }
    };
    let len = ctx.array_length(arr);
    let mut elems = Vec::with_capacity(len);
    for i in 0..len {
        elems.push(ctx.get_array_element(arr, i));
    }
    of_list(ctx, &elems)
}

pub(crate) fn native_set_of_0(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    of_set(ctx, &[])
}

pub(crate) fn native_set_of_1(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let e1 = args.first().copied().unwrap_or(Value::Object(None));
    of_set(ctx, &[e1])
}

pub(crate) fn native_set_of_2(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let e1 = args.first().copied().unwrap_or(Value::Object(None));
    let e2 = args.get(1).copied().unwrap_or(Value::Object(None));
    of_set(ctx, &[e1, e2])
}

pub(crate) fn native_set_of_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        // `Set.of((Object[]) null)`: `switch (elements.length)` throws.
        Some(Value::Object(None)) if immutable_of_null_checks_enabled() => {
            return Err(of_npe(
                "Cannot read the array length because \"elements\" is null",
            ));
        }
        _ => {
            return of_set(ctx, &[]);
        }
    };
    let len = ctx.array_length(arr);
    let mut elems = Vec::with_capacity(len);
    for i in 0..len {
        elems.push(ctx.get_array_element(arr, i));
    }
    of_set(ctx, &elems)
}

pub(crate) fn native_map_of_0(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    of_map(ctx, &[])
}

pub(crate) fn native_map_of_1(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let k = args.first().copied().unwrap_or(Value::Object(None));
    let v = args.get(1).copied().unwrap_or(Value::Object(None));
    of_map(ctx, &[(k, v)])
}

pub(crate) fn native_map_of_2(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let k1 = args.first().copied().unwrap_or(Value::Object(None));
    let v1 = args.get(1).copied().unwrap_or(Value::Object(None));
    let k2 = args.get(2).copied().unwrap_or(Value::Object(None));
    let v2 = args.get(3).copied().unwrap_or(Value::Object(None));
    of_map(ctx, &[(k1, v1), (k2, v2)])
}

// ===========================================================================
// Serialization of the immutable collection carriers
// ===========================================================================
//
// `List.of`/`Set.of`/`Map.of`/`copyOf` hand back a CratonVM-minted carrier
// stamped with a `cratonvm/internal/Unmodifiable*` class that `getClass()`
// aliases to the size-discriminated real JDK class (`ImmutableCollections$
// List12`/`ListN`/`Set12`/`SetN`/`Map1`/`MapN`) - see `collection_display_kind`
// in native-builtins. Its state is this crate's `(backing, immutable-marker)`
// slot pair, NOT the `e0`/`e1`/`elements`/`table` fields those JDK classes
// declare.
//
// Serialization is the one place that difference escaped. `ObjectOutputStream`
// looks the object's class up by its ALIASED name, finds the real
// `writeReplace()` those six classes declare, and runs it - over a receiver
// whose declared fields were never populated. The `java.util.CollSer` it built
// therefore carried this crate's slots instead of the elements:
//
// ```text
//                       HotSpot        CratonVM (before)
//   List.of("a")        [a]            [[a], 1]   <- (backing, marker)
//   Set.of("a","b")     [a, b]         InvalidObjectException: invalid object
//   Map.of("a","1")     {a=1}          InvalidObjectException: invalid object
// ```
//
// 20 of the 52 rows of `probes/CollectionSerProbe.java` failed this way, and it
// is why Spring's `AnnotationTransactionAttributeSourceTests.serializable()`
// died in `CollSer.readResolve` with `InvalidObjectException: invalid object`
// (its cause an NPE out of `Set.of(array)`, which rejects the null elements a
// null `elements` field produced).
//
// The fix is the pair below: `writeReplace` builds the `CollSer` from the
// receiver's OWN public surface (`toArray()`, `entrySet()`) so it is correct
// for a CratonVM carrier and for a genuine JDK instance alike, and
// `CollSer.readResolve` rebuilds through `of_list`/`of_set`/`of_map` so the
// result is a CratonVM carrier rather than a real `MapN` whose `table` these
// natives cannot read. Both need an entry in BOTH force-native gates
// (`force_native_over_real_jdk_bytecode` and its `vm_exec` twin), because each
// class declares the method itself and the reflective `Method.invoke` route
// `ObjectStreamClass` uses has no bytecode PC to key an invoke-cache entry on.

/// `java.util.CollSer`'s tag values (java.base, `ImmutableCollections.java`).
/// The low 8 bits pick the shape; the high 24 are reserved and written zero.
pub(crate) const COLLSER_CLASS: &str = "java/util/CollSer";
pub(crate) const COLLSER_IMM_LIST: i32 = 1;
pub(crate) const COLLSER_IMM_SET: i32 = 2;
pub(crate) const COLLSER_IMM_MAP: i32 = 3;
pub(crate) const COLLSER_IMM_LIST_NULLS: i32 = 4;

/// Snapshot a collection receiver's elements through its own `toArray()`.
///
/// Deliberately a virtual dispatch and not a slot read: this native is reached
/// for a CratonVM-minted carrier AND for a genuine java.base-built instance of
/// the same class, and only the receiver knows which it is.
pub(crate) fn collser_elements(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<Vec<Value>, MethodCallFailed> {
    let arr = match ctx.invoke_virtual(this, "toArray", "()[Ljava/lang/Object;", &[])? {
        Some(Value::Object(Some(a))) => a,
        _ => return Ok(Vec::new()),
    };
    let len = ctx.array_length(arr);
    Ok((0..len).map(|i| ctx.get_array_element(arr, i)).collect())
}

/// Build the `java.util.CollSer` replacement object for `tag` over `elems`.
///
/// GC-SAFETY: `alloc_ref_array` and `new_object` both allocate, so the elements
/// are pinned before the first of them and the array across the second.
pub(crate) fn make_collser(ctx: &mut dyn NativeContext, tag: i32, elems: &[Value]) -> MethodCallResult {
    let (base, handles) = pin_value_slice(ctx, elems);
    let arr = alloc_ref_array(ctx, elems.len());
    let arr_pin = ctx.pin_native_root(arr);
    let base = if base == usize::MAX { arr_pin } else { base };
    for (i, e) in elems.iter().enumerate() {
        let arr = ctx.read_native_pin(arr_pin, arr);
        let v = read_pinned_elem(ctx, handles[i], *e);
        ctx.set_array_element(arr, i, v);
    }
    let ser = match ctx.new_object(COLLSER_CLASS) {
        Ok(Some(Value::Object(Some(o)))) => o,
        Ok(_) => {
            ctx.unpin_native_roots(base);
            return Ok(Some(Value::Object(None)));
        }
        Err(e) => {
            ctx.unpin_native_roots(base);
            return Err(e);
        }
    };
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(base);
    // By name, not by slot: `tag`/`array` are java.base's field names and this
    // is a real JDK object, so a positional guess would be the same class of
    // mistake this whole fix is about.
    ctx.set_field_by_name(ser, "tag", Value::Int(tag));
    ctx.set_field_by_name(ser, "array", Value::Object(Some(arr)));
    Ok(Some(Value::Object(Some(ser))))
}

/// `ImmutableCollections$List12.writeReplace()` / `$ListN.writeReplace()`.
///
/// `IMM_LIST_NULLS` when any element is null - `List.of` cannot produce one,
/// but the nulls-allowed lists java.base builds through
/// `listFromTrustedArrayNullsAllowed` share these carriers, and `List.of(array)`
/// on the read side rejects nulls with an NPE that surfaces as exactly the
/// opaque `InvalidObjectException` this fixes.
pub(crate) fn native_imm_list_write_replace(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let elems = collser_elements(ctx, this)?;
    let tag = if elems.iter().any(|e| matches!(e, Value::Object(None))) {
        COLLSER_IMM_LIST_NULLS
    } else {
        COLLSER_IMM_LIST
    };
    make_collser(ctx, tag, &elems)
}

/// `ImmutableCollections$Set12.writeReplace()` / `$SetN.writeReplace()`.
pub(crate) fn native_imm_set_write_replace(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let elems = collser_elements(ctx, this)?;
    make_collser(ctx, COLLSER_IMM_SET, &elems)
}

/// `ImmutableCollections$Map1.writeReplace()` / `$MapN.writeReplace()`.
///
/// The array is `k0, v0, k1, v1, ...`, so its length is twice the size - the
/// layout `CollSer.readResolve`'s `IMM_MAP` arm reads back.
pub(crate) fn native_imm_map_write_replace(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `entrySet()` then `getKey`/`getValue` per entry, for the same reason
    // `native_map_of_entries` does it: the entry objects may be any
    // `Map.Entry` implementation, and a positional read would assume one.
    let entries = match ctx.invoke_virtual(this, "entrySet", "()Ljava/util/Set;", &[])? {
        Some(Value::Object(Some(es))) => collser_elements(ctx, es)?,
        _ => Vec::new(),
    };
    let (base, handles) = pin_value_slice(ctx, &entries);
    let mut flat: Vec<Value> = Vec::with_capacity(entries.len() * 2);
    let mut flat_pins: Vec<usize> = Vec::with_capacity(entries.len() * 2);
    let mut failure = None;
    for i in 0..entries.len() {
        // Re-read the entry from its pin before EACH dispatch: `getKey` can
        // itself allocate and relocate the entry the `getValue` call needs.
        let e: ObjectRef = match read_pinned_elem(ctx, handles[i], entries[i]) {
            Value::Object(Some(o)) => o,
            _ => continue,
        };
        let k = match ctx.invoke_virtual(e, "getKey", "()Ljava/lang/Object;", &[]) {
            Ok(v) => v.unwrap_or(Value::Object(None)),
            Err(err) => {
                failure = Some(err);
                break;
            }
        };
        let k_pin = pin_value(ctx, k);
        let e: ObjectRef = match read_pinned_elem(ctx, handles[i], entries[i]) {
            Value::Object(Some(o)) => o,
            _ => continue,
        };
        let v = match ctx.invoke_virtual(e, "getValue", "()Ljava/lang/Object;", &[]) {
            Ok(v) => v.unwrap_or(Value::Object(None)),
            Err(err) => {
                failure = Some(err);
                break;
            }
        };
        // gc-common w20-a: every later entry's `getKey`/`getValue` can move the
        // keys and values already collected, so each keeps its pin and all are
        // read back once the walk is over (they were read back at push time
        // and then held raw across the rest of the walk).
        let v_pin = pin_value(ctx, v);
        flat.push(k);
        flat_pins.push(k_pin);
        flat.push(v);
        flat_pins.push(v_pin);
    }
    let flat: Vec<Value> = flat
        .iter()
        .zip(flat_pins.iter())
        .map(|(&v, &h)| read_pinned_elem(ctx, h, v))
        .collect();
    if base != usize::MAX {
        ctx.unpin_native_roots(base);
    }
    if let Some(err) = failure {
        return Err(err);
    }
    make_collser(ctx, COLLSER_IMM_MAP, &flat)
}

/// `Collections$UnmodifiableRandomAccessList.writeReplace()`.
///
/// The JDK body is `new UnmodifiableList<>(list)` - it downgrades the
/// RandomAccess wrapper to the plain one so the stream never names a class that
/// exists only to carry the marker interface, and `UnmodifiableList.readResolve`
/// puts it back. Over a CratonVM carrier `this.list` is unset, so the stream got
/// a wrapper around null and reading it back threw NPE.
///
/// Answering `this` is the whole fix: the plain `Collections$Unmodifiable{List,
/// Set,Map,Collection,SortedSet,NavigableSet}` carriers declare no
/// `writeReplace` and already round-trip correctly - `probes/
/// CollectionSerProbe.java`'s `unmodifiableList(LinkedList)` row, the
/// non-RandomAccess twin of the failing one, passes - so declining the
/// replacement puts the RandomAccess list on that same working path.
pub(crate) fn native_unmod_ral_write_replace(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
}

/// `java.util.CollSer.readResolve()`.
///
/// The real body rebuilds through `List.of`/`Set.of` (natives here, fine) but
/// through `new ImmutableCollections.Map1<>(k, v)` / `new MapN<>(array)` for
/// maps - real constructors, producing a real `table`-backed object that every
/// map native in this crate then reads as empty (`NullPointerException: Cannot
/// read the array length because "this.table" is null`). Rebuilding through
/// `of_map` keeps the deserialized map a CratonVM carrier, which is what the
/// rest of the VM - and `getClass()` - already agrees on.
///
/// With [`collser_resolve_exceptions_enabled`] off, a null array and a tag
/// this does not recognise hand back `this` untouched, and a factory's NPE /
/// IAE propagates bare. With it on (the default), the corrupt-stream answers
/// are the JDK's (see that switch).
pub(crate) fn native_collser_read_resolve(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let jdk_exceptions = collser_resolve_exceptions_enabled();
    // The FULL tag: the unknown-tag message prints it, high bits included.
    let full_tag = match ctx.get_field_by_name(this, "tag") {
        Value::Int(t) => t,
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    let tag = full_tag & 0xff;
    let arr = match ctx.get_field_by_name(this, "array") {
        Value::Object(Some(a)) => a,
        // `if (array == null) throw new InvalidObjectException("null array")`,
        // ahead of the tag switch. `this` is rooted across the attempt: the
        // fallback (no class to build) still answers it.
        _ if jdk_exceptions => {
            let this_pin = ctx.pin_native_root(this);
            let thrown = collser_invalid_object(ctx, "null array", None);
            let this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            return match thrown {
                Some(e) => Err(e),
                None => Ok(Some(Value::Object(Some(this)))),
            };
        }
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    let len = ctx.array_length(arr);
    let elems: Vec<Value> = (0..len).map(|i| ctx.get_array_element(arr, i)).collect();
    let built = match tag {
        COLLSER_IMM_LIST | COLLSER_IMM_LIST_NULLS => of_list_allowing_nulls(ctx, &elems, tag),
        COLLSER_IMM_SET => of_set(ctx, &elems),
        // `MapN(Object...)`'s first check (a `length` of 0 is `EMPTY_MAP` and
        // 2 is `Map1`, so every odd length reaches it); `InternalError` is
        // not one of the two `readResolve` catches.
        COLLSER_IMM_MAP if jdk_exceptions && elems.len() % 2 != 0 => {
            return Err(RuntimeError::InternalError {
                message: "length is odd".to_string(),
            }
            .into());
        }
        COLLSER_IMM_MAP => {
            let mut pairs: Vec<(Value, Value)> = Vec::with_capacity(elems.len() / 2);
            let mut i = 0;
            while i + 1 < elems.len() {
                pairs.push((elems[i], elems[i + 1]));
                i += 2;
            }
            of_map(ctx, &pairs)
        }
        // `String.format("invalid flags 0x%x", tag)`: `%x` of a negative
        // `int` is its 32-bit two's complement.
        _ if jdk_exceptions => {
            let message = format!("invalid flags 0x{:x}", full_tag as u32);
            let this_pin = ctx.pin_native_root(this);
            let thrown = collser_invalid_object(ctx, &message, None);
            let this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            return match thrown {
                Some(e) => Err(e),
                None => Ok(Some(Value::Object(Some(this)))),
            };
        }
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    match built {
        Err(e) if jdk_exceptions => Err(collser_wrap_factory_failure(ctx, e)),
        other => other,
    }
}

/// `CRATONVM_COMPAT_COLLSER_RESOLVE_EXCEPTIONS` -- default ON, `=0` restores
/// the old answers. JDK 25's `CollSer.readResolve` throws
/// `InvalidObjectException("null array")` for a null array and
/// `InvalidObjectException("invalid flags 0x<tag>")` for an unknown tag,
/// wraps a factory's `NullPointerException` / `IllegalArgumentException` (a
/// null or duplicate element in the stream) in
/// `InvalidObjectException("invalid object")` whose cause it is, and lets
/// `MapN`'s `InternalError("length is odd")` through. The native handed back
/// the `CollSer` itself for the first two, the bare NPE / IAE for the third,
/// and dropped an odd map array's last element. Round 15 wave 3 (lane
/// compat3), `r15w3-compat2-collser-read-resolve-exceptions-FIXED-20260930.md`.
pub(crate) fn collser_resolve_exceptions_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_COLLSER_RESOLVE_EXCEPTIONS")
}

/// `new InvalidObjectException(message)` built by its constructor, with
/// `cause` handed to its `initCause` when given -- the thrown failure. `None`
/// when the class cannot be built (a synthetic JDK); the caller keeps its old
/// answer then. An `initCause` that throws answers its own exception, as it
/// would out of the JDK body. `cause` is rooted throughout and rewritten to
/// its current address on every path.
pub(crate) fn collser_invalid_object(
    ctx: &mut dyn NativeContext,
    message: &str,
    cause: Option<&mut ObjectRef>,
) -> Option<MethodCallFailed> {
    // The message string and the exception both allocate: `cause` is rooted
    // across them (the VM's `new_object_initialized` roots its arguments).
    let Some(cause) = cause else {
        let text = ctx.create_string_uninterned(message);
        return match ctx.new_object_initialized(
            "java/io/InvalidObjectException",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(text))],
        ) {
            Ok(Some(Value::Object(Some(ioe)))) => Some(MethodCallFailed::ExceptionThrown(ioe)),
            _ => None,
        };
    };
    let cause_pin = ctx.pin_native_root(*cause);
    let text = ctx.create_string_uninterned(message);
    let built = ctx.new_object_initialized(
        "java/io/InvalidObjectException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(text))],
    );
    let ioe = match built {
        Ok(Some(Value::Object(Some(ioe)))) => ioe,
        _ => {
            *cause = ctx.read_native_pin(cause_pin, *cause);
            ctx.unpin_native_roots(cause_pin);
            return None;
        }
    };
    let ioe_pin = ctx.pin_native_root(ioe);
    *cause = ctx.read_native_pin(cause_pin, *cause);
    let linked = ctx.invoke_virtual(
        ioe,
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
        &[Value::Object(Some(*cause))],
    );
    let ioe = ctx.read_native_pin(ioe_pin, ioe);
    *cause = ctx.read_native_pin(cause_pin, *cause);
    // `cause_pin` is the lower of the two: releases both.
    ctx.unpin_native_roots(cause_pin);
    Some(match linked {
        Err(e) => e,
        Ok(_) => MethodCallFailed::ExceptionThrown(ioe),
    })
}

/// `readResolve`'s `catch (NullPointerException | IllegalArgumentException
/// ex)`: such an `err` becomes `InvalidObjectException("invalid object")`
/// caused by it (a native NPE / IAE is first built as the Java throwable it
/// stands for); anything else, or a failure to build the wrapper, is `err`
/// unchanged.
pub(crate) fn collser_wrap_factory_failure(
    ctx: &mut dyn NativeContext,
    err: MethodCallFailed,
) -> MethodCallFailed {
    // (a thrown Java NPE / IAE) | (a native one: its class and message).
    let (thrown, native): (Option<ObjectRef>, Option<(&'static str, Option<String>)>) = match &err {
        MethodCallFailed::ExceptionThrown(obj) => (Some(*obj), None),
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            RuntimeError::NullPointerException { message },
        )) => (
            None,
            Some((
                "java/lang/NullPointerException",
                message
                    .as_deref()
                    .filter(|m| !m.is_empty())
                    .map(str::to_string),
            )),
        ),
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            RuntimeError::IllegalArgumentException { message },
        )) => (
            None,
            Some(("java/lang/IllegalArgumentException", Some(message.clone()))),
        ),
        _ => (None, None),
    };
    let mut cause = if let Some(obj) = thrown {
        if !(obj_is_instance_of(&*ctx, obj, "java/lang/NullPointerException")
            || obj_is_instance_of(&*ctx, obj, "java/lang/IllegalArgumentException"))
        {
            return err;
        }
        obj
    } else if let Some((class, message)) = native {
        let built = match message {
            Some(m) => {
                let text = ctx.create_string_uninterned(&m);
                ctx.new_object_initialized(
                    class,
                    "(Ljava/lang/String;)V",
                    &[Value::Object(Some(text))],
                )
            }
            None => ctx.new_object_initialized(class, "()V", &[]),
        };
        match built {
            Ok(Some(Value::Object(Some(obj)))) => obj,
            _ => return err,
        }
    } else {
        return err;
    };
    match collser_invalid_object(ctx, "invalid object", Some(&mut cause)) {
        Some(wrapped) => wrapped,
        // A thrown cause may have moved while the wrapper was attempted.
        None if thrown.is_some() => MethodCallFailed::ExceptionThrown(cause),
        None => err,
    }
}

/// `of_list`, but honouring `IMM_LIST_NULLS`: that tag exists precisely because
/// the list is allowed to hold nulls, so `of_list`'s null rejection - correct
/// for `List.of` - must not run for it.
pub(crate) fn of_list_allowing_nulls(
    ctx: &mut dyn NativeContext,
    elems: &[Value],
    tag: i32,
) -> MethodCallResult {
    if tag == COLLSER_IMM_LIST_NULLS {
        // JDK 25: `listFromTrustedArrayNullsAllowed(Arrays.copyOf(array, ..))`
        // -- a `ListN` that allows nulls, which `List.copyOf` copies. The
        // frozen wrapper below is marked immutable, so `copyOf` handed it back.
        if let Some(r) = jdk_list_nulls_allowed(ctx, elems) {
            return r;
        }
        let r = make_list_of(ctx, elems);
        return freeze_result(ctx, UNMOD_LIST_CLASS, r);
    }
    of_list(ctx, elems)
}

/// Register the serialization hooks above, from the registrar that installs
/// `List.of`/`Set.of`/`Map.of` (without a real JDK only, since wave 48): the
/// write and read halves of what those factories produce. With a real JDK the
/// factories run the JDK's bytecode; these still serve the stand-ins `copyOf`,
/// `Collectors` and `CollSer.readResolve` build under `--compatible`.
pub(crate) fn register_immutable_serialization_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    // `SyntheticStub`, not `Bridge`, so `--jdk-only` DROPS the whole family and
    // real bytecode runs.
    //
    // `Bridge` asserts "no working real-bytecode fallback exists". For this
    // family that is a compatible-mode claim wearing a mode-independent tag,
    // and under `--jdk-only` it was not merely unnecessary but FATAL:
    //
    // ```text
    // apps/probes/UtilCoverage4Sweep, --jdk-only
    //   48 ser List.of(1)  THREW java.lang.NoClassDefFoundError
    //   ...
    //   java.lang.NoClassDefFoundError: cratonvm/internal/UnmodifiableList
    // ```
    //
    // -- every `List.of`/`Set.of`/`Map.of` failed to DESERIALIZE. Writing
    // worked and produced the same 59 bytes HotSpot writes; the read side then
    // reached `native_collser_read_resolve`, which rebuilds through `of_list`
    // and `freeze_result` into a `cratonvm/internal/Unmodifiable*` that strict
    // mode refuses to fabricate. The producers this carrier has are supposed to
    // be dropped in strict -- `alloc_immutable_wrapper`'s doc says so and lists
    // them -- and this one was missed because it is registered from a DIFFERENT
    // registrar than the factories it mirrors, under this `Bridge` window.
    //
    // The reason the native exists at all is in `native_collser_read_resolve`:
    // the real body rebuilds maps through real `ImmutableCollections` ctors,
    // producing a `table`-backed object that this crate's map natives read as
    // empty. That is true in COMPATIBLE mode, where those natives run. It is
    // exactly false under `--jdk-only`, where they are dropped and a real
    // `Map1` is the right answer and the only one -- which is why the strict
    // rows now agree with HotSpot down to the class name.
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    for c in [
        "java/util/ImmutableCollections$List12",
        "java/util/ImmutableCollections$ListN",
    ] {
        r.register(
            c,
            "writeReplace",
            "()Ljava/lang/Object;",
            native_imm_list_write_replace,
        );
    }
    for c in [
        "java/util/ImmutableCollections$Set12",
        "java/util/ImmutableCollections$SetN",
    ] {
        r.register(
            c,
            "writeReplace",
            "()Ljava/lang/Object;",
            native_imm_set_write_replace,
        );
    }
    for c in [
        "java/util/ImmutableCollections$Map1",
        "java/util/ImmutableCollections$MapN",
    ] {
        r.register(
            c,
            "writeReplace",
            "()Ljava/lang/Object;",
            native_imm_map_write_replace,
        );
    }
    r.register(
        "java/util/Collections$UnmodifiableRandomAccessList",
        "writeReplace",
        "()Ljava/lang/Object;",
        native_unmod_ral_write_replace,
    );
    r.register(
        COLLSER_CLASS,
        "readResolve",
        "()Ljava/lang/Object;",
        native_collser_read_resolve,
    );
    r.set_category(__prev_cat);
}

/// JIT round j27 wave 4 (lane UNMODLIST), CD16-4
/// (`docs/internal/jit/j27-unmodifiable-views-snapshot-iterators-FIXED-20261029.md`):
/// the `cratonvm/internal/UnmodifiableList` stand-in's `listIterator([i])`
/// walks a `toArray()` snapshot where HotSpot's `Collections$UnmodifiableList`
/// wraps the backing's live list iterator. The stand-in is reached only
/// through the `Collections.unmodifiable*` factory natives, so the JDK body is
/// authoritative exactly when no factory is registered: both modes over a real
/// JDK. These pin that, with the two arms that must keep the stand-ins (the
/// row's revert, and an image with no `java.util.Collections` bytecode) as the
/// controls that show the check can fail. Probe:
/// `tools/probes/jit/j27/J27UnmodViewRows.java`.
#[cfg(test)]
mod j27_unmodifiable_view_factory_registration_tests {
    use super::register_collections_utility_natives;
    use crate::register_collections_extras_natives;
    use cratonvm_native_api::NativeMethodRegistry;
    use cratonvm_types::compat::CompatibilityMode;
    use cratonvm_types::{HotSpotAlignment, HotSpotAlignments};

    const COLLECTIONS: &str = "java/util/Collections";

    /// Every `Collections.unmodifiable*` factory the two native-collections
    /// registrars install (the `unmodifiableList` one is `collections_util`'s,
    /// the other five `register_collections_extras_natives`').
    const FACTORIES: [(&str, &str); 6] = [
        ("unmodifiableList", "(Ljava/util/List;)Ljava/util/List;"),
        ("unmodifiableCollection", "(Ljava/util/Collection;)Ljava/util/Collection;"),
        ("unmodifiableSet", "(Ljava/util/Set;)Ljava/util/Set;"),
        ("unmodifiableSortedSet", "(Ljava/util/SortedSet;)Ljava/util/SortedSet;"),
        (
            "unmodifiableNavigableSet",
            "(Ljava/util/NavigableSet;)Ljava/util/NavigableSet;",
        ),
        ("unmodifiableMap", "(Ljava/util/Map;)Ljava/util/Map;"),
    ];

    fn registry(mode: CompatibilityMode, real_jdk: bool, row: bool) -> NativeMethodRegistry {
        let mut r = NativeMethodRegistry::new();
        r.set_compatibility_mode(mode);
        r.set_hotspot_alignments(
            HotSpotAlignments::defaults().with(HotSpotAlignment::JdkUnmodifiableViews, row),
        );
        r.set_real_jdk(real_jdk);
        register_collections_utility_natives(&mut r);
        register_collections_extras_natives(&mut r);
        r
    }

    /// The factories that would hand out a `cratonvm/internal/Unmodifiable*`.
    fn stand_in_factories(r: &NativeMethodRegistry) -> Vec<&'static str> {
        FACTORIES
            .iter()
            .filter(|(m, d)| r.find(COLLECTIONS, m, d).is_some())
            .map(|(m, _)| *m)
            .collect()
    }

    fn all_factories() -> Vec<&'static str> {
        FACTORIES.iter().map(|(m, _)| *m).collect()
    }

    #[test]
    fn compatible_over_a_real_jdk_runs_every_factory_as_jdk_bytecode() {
        let r = registry(CompatibilityMode::Compatible, true, true);
        assert_eq!(stand_in_factories(&r), Vec::<&str>::new());
    }

    #[test]
    fn jdk_only_runs_every_factory_as_jdk_bytecode() {
        let r = registry(CompatibilityMode::JdkOnly, true, true);
        assert_eq!(stand_in_factories(&r), Vec::<&str>::new());
    }

    /// The row's revert (`CRATONVM_COMPAT_ALIGN=-jdk-unmodifiable-views`) puts
    /// every stand-in back: the snapshot list iterator is reachable again.
    #[test]
    fn the_row_off_puts_the_stand_in_factories_back() {
        let r = registry(CompatibilityMode::Compatible, true, false);
        assert_eq!(stand_in_factories(&r), all_factories());
    }

    /// No real JDK: there is no bytecode to defer to, so the stand-ins stay.
    #[test]
    fn an_image_without_the_jdk_keeps_the_stand_in_factories() {
        let r = registry(CompatibilityMode::Compatible, false, true);
        assert_eq!(stand_in_factories(&r), all_factories());
    }
}

