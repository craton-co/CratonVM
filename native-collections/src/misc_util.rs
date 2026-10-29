// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

pub use streams::*;

// ===========================================================================
// Interface method registrations
// ===========================================================================
// When bytecode invokes methods via interface types (e.g. Collection.iterator()),
// the interpreter resolves based on the invoked class name from the constant pool.
// For synthetic objects (ClassId(0)), there's no class hierarchy to search, so
// we register native methods for common interfaces that delegate to the concrete
// implementations (ArrayList / HashSet).

// JDK-ONLY-CLASSIFY: unknown — needs census. 23 of these 34 registrations land
// on ABSTRACT methods of `java.util` interfaces. An abstract method has no
// bytecode, so jdk-only-native-review.md rule 3 says the strict disposition is
// `MissingImplementation` rather than a stub — but that is only true if no
// implementor is reached first. In practice a native on an abstract interface
// method intercepts every implementing class, including user-defined ones, so
// the tag here silently decides dispatch far outside `java.util`. Neither
// `bridge` nor `stub` is defensible from source. Evidence needed: which of
// these 23 are ever dispatched, and against which receiver classes.
pub(crate) fn register_interface_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    // --- java/util/Collection ---
    registry.register(
        "java/util/Collection",
        "iterator",
        "()Ljava/util/Iterator;",
        native_al_iterator,
    );
    registry.register("java/util/Collection", "size", "()I", native_al_size);
    // `stream()` and `toArray(IntFunction)` deleted 2026-09-20 -- two of the
    // 23 rows this function's own header called "unknown, needs census".
    // Census taken: `l3-followups-...-20260830.md` §2 plus this session's own
    // DeadDoorProbe/DeadDoorProbe2 re-run (both modes, an anonymous
    // AbstractSet receiver reached through a `Collection`-typed local, a
    // method reference, and a synthetic-registry dump) all agree at
    // invocations=0, invocations_complete=true, and the registry census's own
    // `real_declaring_method.has_code` is true for both -- real `Collection`
    // default-method bytecode already answers every call that could have
    // reached these. `java/util/Collection.toArray()` (0-arg) is NOT one of
    // the two: it is a live, separate concern (see `AbstractCollection`'s own
    // 0-arg registration a few hundred lines up, kept for the reason
    // documented there).
    registry.register("java/util/Collection", "isEmpty", "()Z", native_al_is_empty);
    registry.register(
        "java/util/Collection",
        "toArray",
        "()[Ljava/lang/Object;",
        native_al_to_array_entry,
    );

    // --- java/util/List ---
    registry.register(
        "java/util/List",
        "get",
        "(I)Ljava/lang/Object;",
        native_al_get,
    );
    registry.register("java/util/List", "size", "()I", native_al_size);
    // The reclaiming door (gc-common w10-g): a registration is reached only
    // from Java dispatch or `invoke_virtual`, see `GrowAlloc`.
    registry.register(
        "java/util/List",
        "add",
        "(Ljava/lang/Object;)Z",
        native_al_add_entry,
    );
    registry.register(
        "java/util/List",
        "iterator",
        "()Ljava/util/Iterator;",
        native_al_iterator,
    );
    registry.register("java/util/List", "isEmpty", "()Z", native_al_is_empty);
    // Stated, not ambient: see `list_stream_iface_kind` (round 17 wave 3,
    // lane streams3).
    registry.register_with_kind(
        "java/util/List",
        "stream",
        "()Ljava/util/stream/Stream;",
        native_al_stream,
        list_stream_iface_kind(),
    );
    registry.register(
        "java/util/List",
        "toArray",
        "()[Ljava/lang/Object;",
        native_al_to_array_entry,
    );
    registry.register(
        "java/util/List",
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
        native_collection_to_array_generator,
    );

    // Do not register ArrayList-shaped bridges directly on `Iterable`.
    // A method-reference lambda such as `list::iterator` implements Iterable
    // too, but its receiver is the lambda proxy rather than an ArrayList. A
    // direct interface native would therefore manufacture an iterator over the
    // proxy's fields and silently report no elements. Leaving Iterable to the
    // interpreter lets its lambda-SAM rescue dispatch the real implementation;
    // synthetic collection receivers still reach the ArrayList fallback from
    // the no-Code interface-dispatch path.

    // --- forEach on Collection / List / Set ---
    registry.register(
        "java/util/Collection",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        native_al_for_each,
    );
    registry.register(
        "java/util/List",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        native_al_for_each,
    );
    registry.register(
        "java/util/Set",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        native_hs_for_each,
    );

    // --- java/util/Set ---
    registry.register(
        "java/util/Set",
        "iterator",
        "()Ljava/util/Iterator;",
        native_hs_iterator,
    );
    registry.register("java/util/Set", "size", "()I", native_hs_size);
    registry.register("java/util/Set", "isEmpty", "()Z", native_hs_is_empty);

    // --- java/util/Map ---
    registry.register("java/util/Map", "size", "()I", native_map_size);
    registry.register(
        "java/util/Map",
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        native_map_for_each,
    );
    registry.register(
        "java/util/Map",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_get,
    );
    registry.register(
        "java/util/Map",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_put_entry,
    );
    registry.register(
        "java/util/Map",
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_map_contains_key,
    );
    registry.register(
        "java/util/Map",
        "keySet",
        "()Ljava/util/Set;",
        native_map_key_set,
    );
    registry.register(
        "java/util/Map",
        "values",
        "()Ljava/util/Collection;",
        native_map_values,
    );
    registry.register(
        "java/util/Map",
        "entrySet",
        "()Ljava/util/Set;",
        native_map_entry_set,
    );

    // --- java/util/Map$Entry interface ---
    registry.register(
        "java/util/Map$Entry",
        "getKey",
        "()Ljava/lang/Object;",
        native_entry_get_key,
    );
    registry.register(
        "java/util/Map$Entry",
        "getValue",
        "()Ljava/lang/Object;",
        native_entry_get_value,
    );
    registry.register(
        "java/util/Map$Entry",
        "setValue",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_entry_set_value,
    );
    registry.register(
        "java/util/Map$Entry",
        "hashCode",
        "()I",
        native_entry_hash_code,
    );
    registry.register(
        "java/util/Map$Entry",
        "equals",
        "(Ljava/lang/Object;)Z",
        native_entry_equals,
    );
    // Also register under AbstractMap$SimpleEntry for completeness
    registry.register(
        "java/util/AbstractMap$SimpleEntry",
        "getKey",
        "()Ljava/lang/Object;",
        native_entry_get_key,
    );
    registry.register(
        "java/util/AbstractMap$SimpleEntry",
        "getValue",
        "()Ljava/lang/Object;",
        native_entry_get_value,
    );
    registry.register(
        "java/util/AbstractMap$SimpleEntry",
        "setValue",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_entry_set_value,
    );
    registry.register(
        "java/util/AbstractMap$SimpleEntry",
        "hashCode",
        "()I",
        native_entry_hash_code,
    );
    registry.register(
        "java/util/AbstractMap$SimpleEntry",
        "equals",
        "(Ljava/lang/Object;)Z",
        native_entry_equals,
    );
    // The synthetic entries this crate mints are instances of the INTERFACE
    // `java.util.Map$Entry`, which declares no `toString`, so every one of them
    // printed as `java.util.Map$Entry@<identity hash>` -- non-deterministic as
    // well as wrong. MEASURED on `Properties.entrySet()`
    // (apps/probes/PropertiesShadowSweep 151) against HotSpot's `[a=over, b=2, ...]`.
    // Every JDK entry implementation renders `key + "=" + value`.
    registry.register(
        "java/util/Map$Entry",
        "toString",
        "()Ljava/lang/String;",
        native_entry_to_string,
    );
    registry.register(
        "java/util/AbstractMap$SimpleEntry",
        "toString",
        "()Ljava/lang/String;",
        native_entry_to_string,
    );
    registry.set_category(__prev_cat);
}

// ===========================================================================
// Collection copy constructors
// ===========================================================================

pub(crate) fn register_copy_constructor_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    // ArrayList(Collection)
    registry.register(
        "java/util/ArrayList",
        "<init>",
        "(Ljava/util/Collection;)V",
        native_al_init_from_collection,
    );
    // HashSet(Collection)
    registry.register(
        "java/util/HashSet",
        "<init>",
        "(Ljava/util/Collection;)V",
        native_hs_init_from_collection,
    );
    // HashMap(Map)
    registry.register(
        "java/util/HashMap",
        "<init>",
        "(Ljava/util/Map;)V",
        native_map_init_from_map,
    );
    registry.set_category(__prev_cat);
}

/// `CRATONVM_COMPAT_COPY_CTOR_JDK_QUESTIONS` -- default ON, `=0` restores the
/// layout probes plus `size()` + `toArray()` for every source. Proposal C2W3-2
/// of `jit-r15w3-compat2-proposals-RETIRED-20261003.md` (round 15 wave 3, lane compat3), page
/// `r15w3-compat2-collection-copy-call-sequences-FIXED-20260930.md`. JDK 25 asks a
/// copy constructor's source:
///
/// * `ArrayList(Collection)` / `Vector(Collection)`: `c.toArray()`, once,
///   and nothing else ([`al_init_from_to_array`]);
/// * `HashSet(Collection)` / `LinkedHashSet(Collection)`: `c.size()` (it
///   sizes the table), then `addAll(c)` -- `for (E e : c) add(e)`: `iterator()`,
///   `hasNext()` / `next()`, each element hashed as it arrives
///   ([`hs_add_all_by_iterator`]).
///
/// The natives read a source's fields where a layout probe matched (no
/// question at all -- a `(Object[], int)` pair in a program's own class was
/// read as its contents) and asked `size()` + `toArray()` otherwise, so a
/// collection whose `toArray()` and `iterator()` disagree, or that counts
/// what it is asked, copied or counted differently than on HotSpot. Only a
/// FOREIGN source takes the JDK's questions ([`copy_source_is_foreign`]): for
/// the JDK's own classes the probes answer what their bodies would, and none
/// of their answers is a program's code.
pub(crate) fn copy_ctor_jdk_questions_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_COPY_CTOR_JDK_QUESTIONS")
}

/// Class-name prefixes of the JDK's and this VM's own classes: a copy
/// constructor's source of one of these is not [`copy_source_is_foreign`].
pub(crate) const COPY_CTOR_OWN_PREFIXES: [&str; 6] =
    ["java/", "javax/", "jdk/", "sun/", "com/sun/", "cratonvm/"];

/// Whether `source`'s `size()` / `toArray()` / `iterator()` are a program's
/// own code: its class is not the JDK's or this VM's, or it is a wrapper --
/// one of the JDK's `Collections$*` (unmodifiable, synchronized, checked; the
/// wrapped collection in `c`) or this VM's `cratonvm/internal/Unmodifiable*`
/// collection views (at [`UNMOD_FIELD_BACKING`]) -- directly around such a
/// collection: the wrapper's `toArray()` / `size()` / `iterator()` ask the
/// wrapped one, so the questions reach the program either way. Pure reads.
pub(crate) fn copy_source_is_foreign(ctx: &dyn NativeContext, source: ObjectRef) -> bool {
    let own = |ctx: &dyn NativeContext, o: ObjectRef| -> Option<bool> {
        let name = ctx.class_name_arc_of_id(ctx.class_id_of_object(o))?;
        Some(COPY_CTOR_OWN_PREFIXES.iter().any(|p| name.starts_with(p)))
    };
    let Some(name) = ctx.class_name_arc_of_id(ctx.class_id_of_object(source)) else {
        // An unnamed class (a synthetic allocation) is this VM's.
        return false;
    };
    if !COPY_CTOR_OWN_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return true;
    }
    let inner = if name.starts_with("java/util/Collections$") {
        ctx.get_field_by_name(source, "c")
    } else if &*name == UNMOD_COLLECTION_CLASS
        || &*name == UNMOD_LIST_CLASS
        || is_unmod_set_class(&name)
    {
        if ctx.object_num_fields(source) > UNMOD_FIELD_BACKING {
            ctx.get_field(source, UNMOD_FIELD_BACKING)
        } else {
            Value::Object(None)
        }
    } else {
        return false;
    };
    match inner {
        Value::Object(Some(inner)) => own(ctx, inner) == Some(false),
        _ => false,
    }
}

/// `CRATONVM_COMPAT_LAYOUT_PROBE_OWN_CLASSES` -- default ON, `=0` restores
/// the unguarded probes. [`collect_collection_elements`]' two generic probes
/// -- ArrayList's resolved `(elementData, size)` slots, then `(slot 0, slot 1)`
/// -- read ANY receiver whose fields there are an array and an `int`, with no
/// class test (unlike [`al_state`], whose [`AlLayout`] refuses a foreign
/// class). A program's `AbstractList` with `int[] data; int size` (ArrayList's
/// very slots, 1 and 2) was read as a list of raw `Value::Int`s, which the
/// copy / `toArray()` / `addAll` natives then stored into `Object[]`s -- a
/// primitive in a reference array; a ring buffer's `Object[] buf; int head` was
/// read as `buf[0..head)`. `toArray()`, `stream()`, `forEach()` (the
/// `AbstractCollection` natives) and every bulk operation answered that where
/// HotSpot asks the collection. With the switch on the probes answer only with
/// a REFERENCE array, and never for a program's own collection class
/// ([`program_collection_class`]), whose elements every caller then gets by
/// asking it (`size()` + its iterator, or its `toArray()`). Round 15 wave 3
/// (lane compat3), `r15w3-compat3-unguarded-layout-probes-read-program-fields-FIXED-20260930.md`.
pub(crate) fn layout_probe_own_classes_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_LAYOUT_PROBE_OWN_CLASSES")
}

/// A collection class the program (or a library) wrote, whose fields the
/// generic layout probes must not interpret: named outside
/// [`COPY_CTOR_OWN_PREFIXES`], and not an `ArrayList` / `Vector` subclass
/// (those inherit the real `elementData` the natives maintain). Pure reads.
pub(crate) fn program_collection_class(ctx: &dyn NativeContext, coll: ObjectRef) -> bool {
    let Some(name) = ctx.class_name_arc_of_id(ctx.class_id_of_object(coll)) else {
        return false;
    };
    if COPY_CTOR_OWN_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return false;
    }
    !matches!(
        al_slots_and_layout_for(ctx, coll).1,
        AlLayout::ArrayList | AlLayout::Vector
    )
}

/// C2W3-2's `Object[] a = c.toArray()` for a foreign `source`, as
/// `ArrayList.addAll` / `Vector.addAll` ask it: the answer's elements (read
/// straight out of the fresh array; the caller pins them), its exception
/// propagated, a null answer the JDK's NPE on `a.length`.
pub(crate) fn foreign_to_array_elements(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
) -> Result<Vec<Value>, MethodCallFailed> {
    match ctx.invoke_virtual(source, "toArray", "()[Ljava/lang/Object;", &[])? {
        Some(Value::Object(Some(a))) if ctx.heap_kind_of(a) == ObjectKind::Array => {
            let len = ctx.array_length(a);
            Ok((0..len).map(|i| ctx.get_array_element(a, i)).collect())
        }
        Some(Value::Object(None)) | None => Err(RuntimeError::NullPointerException {
            message: Some("Cannot read the array length because \"a\" is null".to_string()),
        }
        .into()),
        // Not an array (a `toArray()` the verifier would have refused).
        Some(_) => Ok(Vec::new()),
    }
}

/// C2W3-2's `ArrayList(Collection)` for a foreign `source`: `Object[] a =
/// c.toArray()` -- its exception propagated, a null answer the JDK's NPE on
/// `a.length` -- then the list is `a`'s elements. `this` is rooted across the
/// call and the buffer's allocation.
pub(crate) fn al_init_from_to_array(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    source: ObjectRef,
) -> MethodCallResult {
    let this_pin = ctx.pin_native_root(this);
    let answer = ctx.invoke_virtual(source, "toArray", "()[Ljava/lang/Object;", &[]);
    let arr = match answer {
        Ok(Some(Value::Object(Some(a)))) if ctx.heap_kind_of(a) == ObjectKind::Array => Some(a),
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
        // `if ((size = a.length) != 0)` on a null `a`.
        Ok(Some(Value::Object(None))) | Ok(None) => {
            ctx.unpin_native_roots(this_pin);
            return Err(RuntimeError::NullPointerException {
                message: Some("Cannot read the array length because \"a\" is null".to_string()),
            }
            .into());
        }
        // Not an array (a `toArray()` the verifier would have refused): empty.
        Ok(_) => None,
    };
    let len = arr.map_or(0, |a| ctx.array_length(a));
    let arr_pin = arr.map(|a| ctx.pin_native_root(a));
    // gc-common w10-g: may collect and retry; `this` and the answer are
    // pinned and re-read below.
    let cap = std::cmp::max(len, AL_DEFAULT_CAPACITY);
    let buf = new_ref_array_grow(ctx, ClassId::new(0), cap, GrowAlloc::Reclaim);
    if let (Some(a), Some(pin)) = (arr, arr_pin) {
        let a = ctx.read_native_pin(pin, a);
        for i in 0..len {
            let v = ctx.get_array_element(a, i);
            ctx.set_array_element(buf, i, v);
        }
    }
    let this = ctx.read_native_pin(this_pin, this);
    al_set_data(ctx, this, buf);
    al_set_size(ctx, this, len as i32);
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

/// C2W3-2's `HashSet(Collection)` for a foreign `source` (rooted at
/// `source_pin`), into the set's fresh backing map (rooted at `backing_pin`):
/// `c.size()` -- asked, and its answer sizes the table
/// ([`hs_backing_presize`]; a count with repeats, or a lying one, sizes it
/// larger than the puts would grow it) -- then `addAll(c)`: `c.iterator()`,
/// and per element `hasNext()`, `next()` and the `add` (its `hashCode()` /
/// `equals()`), in that order, as `for (E e : c) add(e)` interleaves them.
/// The caller releases its pins.
pub(crate) fn hs_add_all_by_iterator(
    ctx: &mut dyn NativeContext,
    (source_pin, source): (usize, ObjectRef),
    (backing_pin, backing): (usize, ObjectRef),
) -> Result<(), MethodCallFailed> {
    let source = ctx.read_native_pin(source_pin, source);
    if let Some(Value::Int(s)) = ctx.invoke_virtual(source, "size", "()I", &[])? {
        let backing_now = ctx.read_native_pin(backing_pin, backing);
        hs_backing_presize(ctx, backing_now, s);
    }
    let source = ctx.read_native_pin(source_pin, source);
    let it = match ctx.invoke_virtual(source, "iterator", "()Ljava/util/Iterator;", &[])? {
        Some(Value::Object(Some(it))) => it,
        // `for (E e : c)` over a null iterator: `hasNext()` on null.
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
    let present_slot = hs_present_slot(&*ctx);
    let it_pin = ctx.pin_native_root(it);
    let walked = (|| -> Result<(), MethodCallFailed> {
        loop {
            let it = ctx.read_native_pin(it_pin, it);
            match ctx.invoke_virtual(it, "hasNext", "()Z", &[])? {
                Some(Value::Int(n)) if n != 0 => {}
                _ => return Ok(()),
            }
            let it = ctx.read_native_pin(it_pin, it);
            let e = ctx
                .invoke_virtual(it, "next", "()Ljava/lang/Object;", &[])?
                .unwrap_or(Value::Object(None));
            // Nothing collects between `next()` and the put, which pins what
            // it holds across its own Java.
            let backing = ctx.read_native_pin(backing_pin, backing);
            let present = hs_present_marker_at(&*ctx, present_slot, e);
            native_map_put(ctx, &[Value::Object(Some(backing)), e, present])?;
        }
    })();
    ctx.unpin_native_roots(it_pin);
    walked
}

/// C4W4-1 (`CRATONVM_COMPAT_HASHMAP_COPY_PRESIZE`, round 15 wave 4, lane
/// compat5): `HashSet(Collection)`'s `map = HashMap.newHashMap(Math.max(
/// c.size(), 12))` for the set's FRESH backing (lazy, `threshold` 16, no put
/// yet, unreachable from the program): `tableSizeFor(ceil(max(s, 12) /
/// 0.75))`. It differs from per-put growth when `s` counts repeats -- `new
/// HashSet<>(listOf20WithRepeats)` iterates over 32 buckets on HotSpot. A
/// `LinkedHashSet`'s backing is a `LinkedHashMap` and is left alone. No Java.
pub(crate) fn hs_backing_presize(ctx: &mut dyn NativeContext, backing: ObjectRef, s: i32) {
    if hashmap_copy_presize_applies(&*ctx, backing) && map_state(&*ctx, backing).0.is_none() {
        hashmap_presize_null_table(ctx, backing, s.max(12));
    }
}

/// ArrayList.<init>(Collection) — copy elements from source collection into this list.
pub(crate) fn native_al_init_from_collection(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // RULE C. An explicit Java null is an NPE, not an empty list:
    // `ArrayList(Collection)` and `Vector(Collection)` both open on
    // `c.toArray()`, and `List.copyOf` -- the one internal caller -- opens on
    // `Objects.requireNonNull`. `reject_null_collection` separates that from a
    // MISSING argument, which the `None` arm below still handles leniently.
    //
    // MEASURED after `Vector(Collection)` was bound to this function on
    // 2026-09-02: `apps/probes/HashtableVectorShadowSweep` row 108,
    // `new Vector<String>((Collection<String>) null)` -- HotSpot NPE, here
    // no-throw. COMPATIBLE MODE ONLY, and that is the interesting half: the
    // registration's own comment says "real-JDK mode runs the class's own
    // bytecode and never reaches here", which is true under `--jdk-only`, where
    // the Vector family is `SyntheticStub` and dropped, and false in the
    // DEFAULT mode, where `NativeKind::allowed_in` keeps it. Strict was 0-diff
    // on that row throughout; only compat moved. The message is the
    // `c.toArray()` dereference's (C4W4-6, `R15Compat4NullArgMessages` row 8).
    reject_null_collection_asked(&*ctx, args.get(1), NULL_ARG_AL_INIT, NULL_C_TO_ARRAY)?;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let source = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            // null or missing — just init empty
            // cceres3: pin across GC-capable call (stream stale-at-store wave)
            let this_pin = ctx.pin_native_root(this);
            let buf = alloc_ref_array(ctx, AL_DEFAULT_CAPACITY);
            let this = ctx.read_native_pin(this_pin, this);
            al_set_data(ctx, this, buf);
            al_set_size(ctx, this, 0);
            ctx.unpin_native_roots(this_pin);
            return Ok(None);
        }
    };

    // C2W3-2 (`CRATONVM_COMPAT_COPY_CTOR_JDK_QUESTIONS`): a foreign source is
    // asked `toArray()` once -- `ArrayList(Collection)` / `Vector(Collection)`
    // ask nothing else -- ahead of every layout probe below, which could
    // read a program's own fields without asking it anything.
    if copy_ctor_jdk_questions_enabled() && copy_source_is_foreign(&*ctx, source) {
        return al_init_from_to_array(ctx, this, source);
    }

    // Fast path: a genuine ArrayList-shaped source (field-layout match) with
    // elements — copy the backing array directly.
    //
    // BUT skip it for org.apache.kafka.common.utils.ImplicitLinkedHashCollection
    // (and subclasses): its `elements` is an open-addressing hash array with
    // NULL holes, so the ArrayList-layout read would copy `elements[0..size]`
    // and carry the holes (e.g. `new ArrayList<>(apiVersionCollection)` got 5
    // null elements). Fall through to `collect_collection_elements`, which has
    // a dedicated null-skipping branch for these.
    let source_is_ilhc = ctx
        .class_id_by_name("org/apache/kafka/common/utils/ImplicitLinkedHashCollection")
        .is_some_and(|ilhc| ctx.is_subclass(ctx.class_id_of_object(source), ilhc));
    let (src_data, src_size) = if source_is_ilhc {
        (None, 0)
    } else {
        al_state(ctx, source)
    };
    if let (Some(_arr0), size) = (src_data, src_size) {
        if size > 0 {
            let cap = std::cmp::max(size as usize, AL_DEFAULT_CAPACITY);
            // GC-safe (HIB-CV-18): `alloc_ref_array` may move the SOURCE's
            // backing array, so the `_arr0` captured before the allocation is a
            // stale pointer afterwards. Re-read the source's array AFTER the
            // alloc, then copy element-by-element (no allocation in the loop).
            // cceres3: pin across GC-capable call (stream stale-at-store wave) —
            // `this` and `source` themselves also move across that alloc.
            let this_pin = ctx.pin_native_root(this);
            let src_pin = ctx.pin_native_root(source);
            // gc-common w10-g: may collect and retry (see the audit on the
            // generic path below). `this` and `source` are pinned above and
            // re-read below; the source's array is re-read from `source`.
            let buf = new_ref_array_grow(ctx, ClassId::new(0), cap, GrowAlloc::Reclaim);
            let source = ctx.read_native_pin(src_pin, source);
            let arr: ObjectRef = match al_state(ctx, source).0 {
                Some(a) => a,
                None => {
                    ctx.unpin_native_roots(this_pin);
                    return Ok(None);
                }
            };
            for i in 0..size as usize {
                let val = ctx.get_array_element(arr, i);
                ctx.set_array_element(buf, i, val);
            }
            let this = ctx.read_native_pin(this_pin, this);
            al_set_data(ctx, this, buf);
            al_set_size(ctx, this, size);
            ctx.unpin_native_roots(this_pin);
            return Ok(None);
        }
        // `al_state` reported a 0-size ArrayList layout. That is either a
        // genuinely empty ArrayList OR a non-ArrayList whose slots merely
        // aliased an `elementData`/`size` pair (e.g. ConcurrentLinkedQueue's
        // real `head`/`tail` Node fields read as (Object, 0)). Fall through to
        // the generic collector, which double-checks the real `size()` and,
        // when non-zero, materialises via the collection's own `toArray()`.
    }

    // Source is not an ArrayList-shaped object (an unmodifiable view, HashSet,
    // LinkedList, ConcurrentLinkedQueue, a third-party Collection, …). Walk it
    // generically so `new ArrayList<>(...)` copies the real elements instead of
    // producing an empty list — the JUnit-Vintage `new ArrayList<>(
    // Description.fChildren)` bug, where `fChildren` is a real-bytecode
    // ConcurrentLinkedQueue, hid every `@org.junit.Test` method from discovery.
    // GC-safe (HIB-CV-18): the gathered element pointers are raw, non-rooted
    // refs; `alloc_ref_array` may trigger a young GC that moves the elements,
    // leaving every pre-alloc pointer stale (observed as VM-singleton heap
    // garbage from `new ArrayList<>(persistentSet)` on the first read after a
    // collection load). Collect once only for the length, allocate, then
    // RE-collect from the live source and store immediately — no allocation
    // between the read and the store.
    // cceres3: pin across GC-capable call (stream stale-at-store wave) — the
    // second element collection re-enters Java (size/toArray) and can move
    // `buf`/`this`/`source` and the just-gathered elements.
    //
    // gc-common w10-g: the buffer may collect and retry (`GrowAlloc::Reclaim`).
    // This function already collects -- both element walks re-enter Java --
    // so no caller may hold a bare reference across it: its callers are its
    // two registrations (`ArrayList` / `Vector(Collection)`) and `List.copyOf`
    // (`native_list_copy_of`), which roots its `src` and `backing` across the
    // call. `live` audit: `this` and `source` are pinned here and re-read
    // after every GC point; the collected elements are pinned before the
    // buffer's allocation and read back through their pins. The walk's error
    // path releases this frame instead of leaving it to the caller's pin floor.
    //
    // gc-common w20-a: ONE walk, not two. This used to walk the source once
    // for the length, allocate, and walk it AGAIN for the elements -- the
    // shape `native_hs_iterator` retired: pinning the first walk's answer
    // survives the allocation just as well, and the second walk re-entered
    // Java (`size`/`toArray`/the iterator) for every `new ArrayList<>(c)` over
    // a non-ArrayList. It was also inconsistent: a second walk that answered
    // fewer elements than the first left `size` covering trailing nulls.
    let this_pin = ctx.pin_native_root(this);
    let elems = match collect_collection_elements_or_real(ctx, source) {
        Ok(elems) => elems,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let (_, elem_handles) = pin_value_slice(ctx, &elems);
    let len = elems.len();
    let cap = std::cmp::max(len, AL_DEFAULT_CAPACITY);
    let buf: ObjectRef = new_ref_array_grow(ctx, ClassId::new(0), cap, GrowAlloc::Reclaim);
    for (i, val) in elems.iter().enumerate() {
        let val = read_pinned_elem(ctx, elem_handles[i], *val);
        ctx.set_array_element(buf, i, val);
    }
    let this = ctx.read_native_pin(this_pin, this);
    al_set_data(ctx, this, buf);
    al_set_size(ctx, this, len as i32);
    ctx.unpin_native_roots(this_pin);

    Ok(None)
}

/// HashSet.<init>(Collection) — copy elements from source into this set.
pub(crate) fn native_hs_init_from_collection(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A real `CopyOnWriteArraySet` is served by its own bytecode over the
    // `CopyOnWriteArrayList` in `al`, not by this file's map surface.
    // See [`cow_set_route`].
    if let Some(r) = cow_set_route(ctx, args, "<init>", "(Ljava/util/Collection;)V") {
        return r;
    }
    // `HashSet(Collection)` / `LinkedHashSet(Collection)` size their table
    // from `c.size()`: HotSpot's helpful message (C4W4-6, row 9).
    reject_null_collection_asked(&*ctx, args.get(1), NULL_ARG_HS_INIT, NULL_C_SIZE)?;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let source = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            // Init empty set (LinkedHashMap backing for insertion-ordered sets).
            // cceres3: pin across GC-capable call (stream stale-at-store wave)
            let this_pin = ctx.pin_native_root(this);
            let backing = alloc_hs_backing(ctx, this, MAP_DEFAULT_CAPACITY);
            let this = ctx.read_native_pin(this_pin, this);
            hs_set_backing_map(ctx, this, backing);
            ctx.unpin_native_roots(this_pin);
            return Ok(None);
        }
    };

    // Create the backing map for this set (LinkedHashMap for insertion-ordered
    // sets so iteration preserves order) and populate it.
    // cceres3: pin across GC-capable call (stream stale-at-store wave) — the
    // backing alloc moves `this`, and every native_map_put below re-enters Java
    // (hashCode/equals, possible resize) moving `backing` + pending elems.
    //
    // `source` must be rooted BEFORE `alloc_hs_backing` too. That allocation is
    // GC-capable, and `source` is a raw `args` value that is next dereferenced
    // AFTER it, by `collect_collection_elements_or_real` below — a moving young
    // GC in between leaves it pointing into from-space, so `new HashSet<>(coll)`
    // reads its elements out of a dead (possibly already reused) object. This is
    // the same hazard `native_map_init_from_map` guards with its `source_pin`
    // (added after the live `NoSuchMethodError java/lang/Object.entrySet()`
    // capture from `new HashMap<>(children)` during WildFly
    // `parallel-extension-add`) and `native_al_init_from_collection` guards with
    // its `src_pin`; the HashSet constructor was the one member of the family
    // that never got it, and `new HashSet<>(Arrays.asList(...))` is on exactly
    // that WildFly boot path.
    let source_pin = ctx.pin_native_root(source);
    let this_pin = ctx.pin_native_root(this);
    let backing = alloc_hs_backing(ctx, this, MAP_DEFAULT_CAPACITY);
    let backing_pin = ctx.pin_native_root(backing);
    let this = ctx.read_native_pin(this_pin, this);
    hs_set_backing_map(ctx, this, backing);

    // Round 49 fix: route through `collect_collection_elements` so we
    // honour every wrapper layout (ArrayList, Arrays$ArrayList,
    // LinkedList, HashSet/LinkedHashSet, Collections$UnmodifiableList,
    // …) — not just the synthetic ArrayList layout.  Previously
    // `new LinkedHashSet(unmodifiableList)` (the dominant Spring Boot
    // pattern in `AutoConfigurationImportSelector.removeDuplicates`,
    // since `ImportCandidates.getCandidates()` returns
    // `Collections.unmodifiableList(arrayList)`) silently produced an
    // empty set — wiping every auto-configuration before filtering and
    // surfacing as `MissingWebServerFactoryBeanException` at boot.
    let source = ctx.read_native_pin(source_pin, source);
    // `map = HashMap.newHashMap(Math.max(c.size(), 12)); addAll(c);` (the
    // `LinkedHashSet` twin alike): a program subclass's `addAll` runs during
    // `super(c)` ([`copy_ctor_bulk_override`]).
    if copy_ctor_bulk_override(&*ctx, this, "addAll", "(Ljava/util/Collection;)Z") {
        let routed = (|| -> MethodCallResult {
            let source_now = ctx.read_native_pin(source_pin, source);
            if let Some(Value::Int(s)) = ctx.invoke_virtual(source_now, "size", "()I", &[])? {
                let backing_now = ctx.read_native_pin(backing_pin, backing);
                hs_backing_presize(ctx, backing_now, s);
            }
            let this_now = ctx.read_native_pin(this_pin, this);
            let source_now = ctx.read_native_pin(source_pin, source);
            ctx.invoke_virtual(
                this_now,
                "addAll",
                "(Ljava/util/Collection;)Z",
                &[Value::Object(Some(source_now))],
            )
        })();
        ctx.unpin_native_roots(source_pin);
        return routed.map(|_| None);
    }
    // C2W3-2 (`CRATONVM_COMPAT_COPY_CTOR_JDK_QUESTIONS`): a foreign source is
    // asked `size()`, then walked by its own `iterator()`, one `add` per
    // element as it arrives.
    if copy_ctor_jdk_questions_enabled() && copy_source_is_foreign(&*ctx, source) {
        let added = hs_add_all_by_iterator(ctx, (source_pin, source), (backing_pin, backing));
        ctx.unpin_native_roots(source_pin);
        return added.map(|()| None);
    }
    // A throwing walk releases this frame's pins (round 15 wave 3, lane
    // compat3: the `?` left `source` / `this` / `backing` pinned).
    let collected = collect_collection_elements_or_real(ctx, source);
    let elems = unpin_on_err(ctx, source_pin, collected)?;
    let (_, elem_handles) = pin_value_slice(ctx, &elems);
    // C4W4-1: the JDK's own source answers `c.size()` with its element count,
    // repeats included.
    let backing_now = ctx.read_native_pin(backing_pin, backing);
    hs_backing_presize(
        ctx,
        backing_now,
        i32::try_from(elems.len()).unwrap_or(i32::MAX),
    );
    // Dead `let sentinel = Value::Int(1)` removed — see [`hs_present_marker_at`].
    // The receiver is a `HashSet`/`LinkedHashSet`, both of which inherit
    // `HashSet.remove`'s `== PRESENT` identity test, so both need the real one.
    let present_slot = hs_present_slot(&*ctx);
    for (i, val) in elems.iter().enumerate() {
        let backing = ctx.read_native_pin(backing_pin, backing);
        let val = read_pinned_elem(ctx, elem_handles[i], *val);
        let present = hs_present_marker_at(&*ctx, present_slot, val);
        if let Err(e) = native_map_put(ctx, &[Value::Object(Some(backing)), val, present]) {
            ctx.unpin_native_roots(source_pin);
            return Err(e);
        }
    }

    ctx.unpin_native_roots(source_pin);
    Ok(None)
}

/// HashMap.<init>(Map) — copy entries from source map into this map.
pub(crate) fn native_map_init_from_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // A null source map answered an EMPTY MAP here, where the JDK
    // dereferences it: `HashMap(Map m)` -> `putMapEntries(m, false)` ->
    // `int s = m.size()`. Silently accepting null is worse than a wrong
    // message -- the caller does not learn it passed null at all. Only an
    // explicitly-passed null throws; a missing argument stays the
    // malformed-call no-op, the distinction `reject_null_functional` draws.
    if matches!(args.get(1), Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException {
            message: Some(
                "Cannot invoke \"java.util.Map.size()\" because \"m\" is null".to_string(),
            ),
        }
        .into());
    }
    let source = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            // Init empty HashMap
            // cceres3: pin across GC-capable call (stream stale-at-store wave)
            let this_pin = ctx.pin_native_root(this);
            let buckets = alloc_ref_array(ctx, MAP_DEFAULT_CAPACITY);
            let this = ctx.read_native_pin(this_pin, this);
            publish_map_table(ctx, this, buckets, MAP_DEFAULT_CAPACITY as i32);
            set_map_size(ctx, this, 0);
            ctx.unpin_native_roots(this_pin);
            return Ok(None);
        }
    };
    // See through `Collections.unmodifiableMap` / `Map.of` wrappers so
    // `new HashMap<>(Map.of(...))` copies the real entries.
    let source = unwrap_unmod(ctx, source);

    // Init this map
    // cceres3: pin across GC-capable call (stream stale-at-store wave) — the
    // buckets alloc moves `this`, and every native_map_put below re-enters
    // Java, moving `this` and every not-yet-stored (key, value) pair.
    //
    // cceres5 (live-captured 2026-07-22, `NoSuchMethodError
    // java/lang/Object.entrySet()` from ConcreteResourceRegistration.
    // getOrCreateSubregistry's `new HashMap<>(children)`): `source` was
    // carried RAW across the buckets allocation below, so a moving GC there
    // handed `collect_entries_any` a stale receiver whose reused block
    // identified as bare `java.lang.Object` — the polymorphic `entrySet()`
    // fallback then NSME'd and rolled back the whole parallel-extension-add.
    // Pin `source` across the allocation too.
    let source_pin = ctx.pin_native_root(source);
    let this_pin = ctx.pin_native_root(this);
    // C4W4-1 (`CRATONVM_COMPAT_HASHMAP_COPY_PRESIZE`, round 15 wave 4, lane
    // compat5): `this.loadFactor = DEFAULT_LOAD_FACTOR; putMapEntries(m,
    // false)` leaves `table` null and parks `tableSizeFor(ceil(s / 0.75))` in
    // `threshold` once `s` is known, so the first put allocates 2 / 4 / 8
    // buckets for a small source where this allocated 16 (and never wrote
    // `loadFactor`). `s == 0` leaves `threshold` 0: 16 on a later first put.
    let presize = hashmap_copy_presize_applies(&*ctx, this);
    let this = if presize {
        set_map_size(ctx, this, 0);
        try_set_jdk_map_field(ctx, this, "threshold", Value::Int(0));
        try_set_jdk_map_field(ctx, this, "loadFactor", Value::Float(0.75_f32));
        this
    } else {
        let cap = MAP_DEFAULT_CAPACITY;
        let buckets = alloc_ref_array(ctx, cap);
        let this = ctx.read_native_pin(this_pin, this);
        // JDK-ONLY-LAYOUT (L2): `new HashMap<>(map)` gets the same treatment as the
        // no-arg constructor. Writing `Int(cap)` at absolute slot 2 landed on the
        // real `table` field of every real-layout receiver — the `HashMap` slot-2
        // `Int` over `[` census row — leaving JDK bytecode an int where it expects
        // the bucket array, while `native_map_init` right next door already stored
        // the array there.
        publish_map_table(ctx, this, buckets, cap as i32);
        set_map_size(ctx, this, 0);
        this
    };

    // Copy entries from source, dispatching on its concrete backend
    // (LinkedHashMap overlay, TreeMap tree, HashMap/CHM buckets, or — via the
    // `entrySet().iterator()` fallback — any other `Map` such as the real-JDK
    // `Collections$Unmodifiable*Map` wrappers). Previously this only handled
    // LinkedHashMap and fell back to HashMap-bucket scanning, so
    // `new HashMap<>(treeMap)` / `new HashMap<>(unmodifiableNavigableMap)`
    // silently produced an empty map.
    let source = ctx.read_native_pin(source_pin, source);
    // C3W3-2 (`CRATONVM_COMPAT_MAP_COPY_JDK_QUESTIONS`, round 15 wave 4, lane
    // compat4): a program's map is asked `putMapEntries(m, false)`'s
    // questions, each put interleaved. Pure reads decide it.
    if let Some(prog) = foreign_map_source(&*ctx, source) {
        let prog_pin = ctx.pin_native_root(prog);
        let walked = map_for_each_entry_sized(
            ctx,
            (prog_pin, prog),
            MapSizeAsk::Gate,
            |ctx, s| {
                if presize {
                    let this_now = ctx.read_native_pin(this_pin, this);
                    hashmap_presize_null_table(ctx, this_now, s);
                }
            },
            |ctx, key, value| {
                let this_now = ctx.read_native_pin(this_pin, this);
                native_map_put(ctx, &[Value::Object(Some(this_now)), key, value])?;
                Ok(())
            },
        );
        ctx.unpin_native_roots(source_pin);
        return walked.map(|()| None);
    }
    let collected = collect_entries_any(ctx, source);
    let entries = unpin_on_err(ctx, source_pin, collected)?;
    let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (_, pair_handles) = pin_value_slice(ctx, &flat);
    if presize {
        // The JDK's own source answers `m.size()` with its entry count.
        let this_now = ctx.read_native_pin(this_pin, this);
        hashmap_presize_null_table(
            ctx,
            this_now,
            i32::try_from(entries.len()).unwrap_or(i32::MAX),
        );
    }
    for (i, (key, value)) in entries.iter().enumerate() {
        let this = ctx.read_native_pin(this_pin, this);
        let key = read_pinned_elem(ctx, pair_handles[2 * i], *key);
        let value = read_pinned_elem(ctx, pair_handles[2 * i + 1], *value);
        if let Err(e) = native_map_put(ctx, &[Value::Object(Some(this)), key, value]) {
            ctx.unpin_native_roots(source_pin);
            return Err(e);
        }
    }

    ctx.unpin_native_roots(source_pin);
    Ok(None)
}

/// JDK `HashMap.tableSizeFor` — smallest power of two >= `cap`.
pub(crate) fn hashmap_table_size_for(cap: i32) -> i32 {
    // `tableSizeFor(0)` is 1 (`-1 >>> numberOfLeadingZeros(-1)` is negative):
    // `(0 - 1) as u32` would answer MAXIMUM_CAPACITY.
    if cap <= 1 {
        return 1;
    }
    let mut n = (cap - 1) as u32;
    n |= n >> 1;
    n |= n >> 2;
    n |= n >> 4;
    n |= n >> 8;
    n |= n >> 16;
    if (n as i64) + 1 >= (1i64 << 30) {
        1 << 30
    } else {
        (n + 1) as i32
    }
}

/// `u64`-domain sibling of [`hashmap_table_size_for`] for callers (like the
/// `ConcurrentHashMap(int)` constructor) that pre-inflate the requested
/// capacity by more than 2x and could otherwise overflow `i32` before the
/// `tableSizeFor` cap kicks in. Same JDK `MAXIMUM_CAPACITY` (1<<30) ceiling.
pub(crate) fn hashmap_table_size_for_u64(cap: u64) -> i32 {
    if cap <= 1 {
        return 1;
    }
    let mut n = cap - 1;
    n |= n >> 1;
    n |= n >> 2;
    n |= n >> 4;
    n |= n >> 8;
    n |= n >> 16;
    n |= n >> 32;
    if n + 1 >= (1u64 << 30) {
        1 << 30
    } else {
        (n + 1) as i32
    }
}

/// Capacity to serialize as the HashMap stream's `buckets` int. Prefer the real
/// `table` array length (a real-backed map serializes byte-identically to the
/// real `writeObject`); otherwise pre-size from the entry count the way
/// `HashMap.putMapEntries` does. The value is advisory — `readObject` recomputes
/// threshold from `loadFactor` — so an approximation round-trips correctly.
pub(crate) fn hashmap_serialized_capacity(ctx: &dyn NativeContext, this: ObjectRef, size: i32) -> i32 {
    // JDK-ONLY-LAYOUT (L2): the receiver's own `table`, not `HashMap`'s index
    // read off an unrelated layout.
    if let Some(slot) = receiver_table_slot(ctx, this) {
        if let Value::Object(Some(tab)) = ctx.get_field(this, slot) {
            let len = ctx.array_length(tab);
            if len > 0 {
                return len as i32;
            }
        }
    }
    if size <= 0 {
        return MAP_DEFAULT_CAPACITY as i32;
    }
    let ft = (size as f32 / 0.75) + 1.0;
    hashmap_table_size_for(ft as i32).max(1)
}

/// `HashMap.writeObject(ObjectOutputStream)` — serialize from whatever backing
/// the map actually uses.
///
/// CratonVM's `HashMap.<init>(Map)` copy-constructor (`native_map_init_from_map`)
/// populates the synthetic bucket side-table but leaves the real-JDK `table` /
/// `loadFactor` / `threshold` fields unset. The inherited real `writeObject`
/// then serialized `loadFactor=0.0` with zero entries, so a peer's `readObject`
/// threw `InvalidObjectException: Illegal load factor: 0.0` — breaking any
/// object graph holding a `new HashMap<>(src)` (e.g. Hibernate's
/// `@AttributeConverter` serializing an `Exif`/`Caption` value). Driving the
/// stream from `collect_entries_any` (which reads native-backed *and*
/// real-backed maps via the bucket reader + `entrySet().iterator()` fallback)
/// and ensuring a valid `loadFactor` reproduces the real stream format for both.
pub(crate) fn native_hashmap_write_object(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let oos = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // Ensure the real `loadFactor`/`threshold` fields hold valid values before
    // `defaultWriteObject` reads them. Only rewrites when `loadFactor` is still
    // 0.0 (the native-backed case) — a real-backed map keeps its own 0.75.
    if let Some(cname) = ctx.class_name_of_id(ctx.class_id_of_object(this)) {
        ensure_hashtable_load_factor(ctx, this, &cname);
    }
    // JIT round 16 (lane compatdrain): `this`, `oos` and every collected
    // entry are rooted across the stream's Java (`collect_entries_any` and
    // each `writeObject` allocate and can move them); the entries and `this`
    // were bare addresses read after it, `oos` before the loop. Every exit
    // below releases this one group.
    let this_pin = ctx.pin_native_root(this);
    let oos_pin = ctx.pin_native_root(oos);
    let collected = collect_entries_any(ctx, this);
    let entries = unpin_on_err(ctx, this_pin, collected)?;
    let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (_, handles) = pin_value_slice(ctx, &flat);
    let this = ctx.read_native_pin(this_pin, this);
    let size = entries.len() as i32;
    // JIT round 16 (lane compatdrain), r15w4-compat5 item 8: an
    // integer-overlay map has no JDK table yet, and its stream carried the
    // count-derived `buckets` and the parked (or made-up) `threshold`; HotSpot
    // writes the table the map holds (`capacity()`) and that table's
    // `threshold`. Both are the overlay's modelled table's now; the field is
    // put back after `defaultWriteObject` (the overlay keeps the map's state).
    let overlay_cap = if hashmap_serialized_threshold_enabled() {
        hm_int_fast_java_cap(&*ctx, this).and_then(|c| i32::try_from(c).ok())
    } else {
        None
    };
    let cap = overlay_cap.unwrap_or_else(|| hashmap_serialized_capacity(&*ctx, this, size));
    let parked_threshold = overlay_cap.and_then(|c| {
        let old = map_threshold_field(&*ctx, this)?;
        let lf = map_custom_load_factor(&*ctx, this).unwrap_or(0.75);
        try_set_jdk_map_field(
            ctx,
            this,
            "threshold",
            Value::Int(jdk_hashmap_threshold(c, lf)),
        );
        Some(old)
    });
    let written = (|| -> Result<(), MethodCallFailed> {
        let oos_cls = "java/io/ObjectOutputStream";
        // s.defaultWriteObject() — writes the non-transient loadFactor + threshold.
        let o = ctx.read_native_pin(oos_pin, oos);
        ctx.invoke(
            oos_cls,
            "defaultWriteObject",
            "()V",
            &[Value::Object(Some(o))],
        )?;
        if let Some(old) = parked_threshold {
            let this_now = ctx.read_native_pin(this_pin, this);
            try_set_jdk_map_field(ctx, this_now, "threshold", Value::Int(old));
        }
        // s.writeInt(buckets); s.writeInt(size)
        let o = ctx.read_native_pin(oos_pin, oos);
        ctx.invoke(
            oos_cls,
            "writeInt",
            "(I)V",
            &[Value::Object(Some(o)), Value::Int(cap)],
        )?;
        let o = ctx.read_native_pin(oos_pin, oos);
        ctx.invoke(
            oos_cls,
            "writeInt",
            "(I)V",
            &[Value::Object(Some(o)), Value::Int(size)],
        )?;
        // internalWriteEntries: key then value for each mapping.
        for (i, (key, value)) in entries.iter().enumerate() {
            let key = read_pinned_elem(ctx, handles[2 * i], *key);
            let o = ctx.read_native_pin(oos_pin, oos);
            ctx.invoke(
                oos_cls,
                "writeObject",
                "(Ljava/lang/Object;)V",
                &[Value::Object(Some(o)), key],
            )?;
            let value = read_pinned_elem(ctx, handles[2 * i + 1], *value);
            let o = ctx.read_native_pin(oos_pin, oos);
            ctx.invoke(
                oos_cls,
                "writeObject",
                "(Ljava/lang/Object;)V",
                &[Value::Object(Some(o)), value],
            )?;
        }
        Ok(())
    })();
    if written.is_err() {
        if let Some(old) = parked_threshold {
            // `defaultWriteObject` threw before the field was put back.
            let this_now = ctx.read_native_pin(this_pin, this);
            if map_threshold_field(&*ctx, this_now) != Some(old) {
                try_set_jdk_map_field(ctx, this_now, "threshold", Value::Int(old));
            }
        }
    }
    ctx.unpin_native_roots(this_pin);
    written.map(|()| None)
}

/// `CRATONVM_COMPAT_HASHMAP_SERIALIZED_THRESHOLD` -- default ON, `=0` keeps
/// the count-derived `buckets` and the field as it stands. See the item-8
/// note in [`native_hashmap_write_object`].
pub(crate) fn hashmap_serialized_threshold_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_HASHMAP_SERIALIZED_THRESHOLD")
}

/// `HashMap.readObject(ObjectInputStream)` — deserialize into the native
/// backing. Mirrors `native_hashmap_write_object` and the real stream format:
/// `defaultReadObject` (loadFactor + threshold), `readInt` buckets (ignored),
/// `readInt` size, then size×(key, value) re-inserted via `native_map_put`.
pub(crate) fn native_hashmap_read_object(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let ois = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let ois_cls = "java/io/ObjectInputStream";
    // Pin BEFORE the first GC-capable call: `defaultReadObject` already
    // replays this map's own serialized fields, so it allocates and can
    // collect just like the per-entry `readObject`s below.
    let this_pin = ctx.pin_native_root(this);
    let ois_pin = ctx.pin_native_root(ois);
    let mut this = this;
    let mut ois = ois;
    ctx.invoke(
        ois_cls,
        "defaultReadObject",
        "()V",
        &[Value::Object(Some(ois))],
    )?;
    ois = ctx.read_native_pin(ois_pin, ois);
    // buckets (ignored — capacity is recomputed from size)
    let _ = ctx.invoke(ois_cls, "readInt", "()I", &[Value::Object(Some(ois))])?;
    ois = ctx.read_native_pin(ois_pin, ois);
    let n = match ctx.invoke(ois_cls, "readInt", "()I", &[Value::Object(Some(ois))])? {
        Some(Value::Int(v)) => v,
        _ => 0,
    };
    ois = ctx.read_native_pin(ois_pin, ois);
    this = ctx.read_native_pin(this_pin, this);
    // GC-safety: each `readObject` below replays an arbitrary nested object
    // graph, so it allocates and can collect. `this` and `ois` were captured
    // into bare Rust locals at entry and are invisible to the collector's root
    // scan; a collection that relocates them — the "non-moving" young sweep
    // still SELECTIVELY PROMOTES survivors into old gen — leaves both locals
    // addressing reclaimed young memory.
    //
    // Both stale locals were observed crashing
    // `TestGroupChannelSenderConnections`: a stale `ois` is passed as the
    // receiver of the NEXT `readObject`, and the interpreter's getfield
    // receiver barrier then faults reading a reclaimed header
    // (`VmHeap::load_and_forward`); a stale `this` faults in
    // `native_map_put_evict`'s `object_num_fields`. Pin both across the whole
    // replay and re-read after every GC-capable call. `key` has to survive the
    // second `readObject` too, so it is pinned across it.
    for _ in 0..n {
        let key = ctx
            .invoke(
                ois_cls,
                "readObject",
                "()Ljava/lang/Object;",
                &[Value::Object(Some(ois))],
            )?
            .unwrap_or(Value::Object(None));
        ois = ctx.read_native_pin(ois_pin, ois);
        let key_pin = pin_value(ctx, key);
        let value = ctx
            .invoke(
                ois_cls,
                "readObject",
                "()Ljava/lang/Object;",
                &[Value::Object(Some(ois))],
            )?
            .unwrap_or(Value::Object(None));
        ois = ctx.read_native_pin(ois_pin, ois);
        let key = read_pinned_elem(ctx, key_pin, key);
        this = ctx.read_native_pin(this_pin, this);
        // evict=false mirrors HotSpot's `HashMap.readObject`, which calls
        // `putVal(hash, key, value, false, false)`: the trailing `evict=false`
        // suppresses `afterNodeInsertion`'s `removeEldestEntry` callback while
        // entries are being replayed. A `LinkedHashMap` subclass overriding
        // `removeEldestEntry` (e.g. Spring's `LinkedCaseInsensitiveMap`'s
        // anonymous inner class) may reference its own not-yet-restored
        // fields (a synthetic `this$0` outer reference) at this point in the
        // deserialization sequence -- calling the hook here throws
        // `NullPointerException: Cannot invoke
        // "...removeEldestEntry(...)" because "this.this$0" is null`.
        native_map_put_evict(ctx, &[Value::Object(Some(this)), key, value], false)?;
        // `native_map_put_evict` itself runs bytecode (hashCode/equals on the
        // key, plus table growth), so refresh the pinned receiver once more
        // before the next iteration reuses it.
        this = ctx.read_native_pin(this_pin, this);
        ois = ctx.read_native_pin(ois_pin, ois);
        if key_pin != usize::MAX {
            ctx.unpin_native_roots(key_pin);
        }
    }
    ctx.unpin_native_roots(ois_pin);
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

// ===========================================================================
// Comparator factory methods
// ===========================================================================

// Comparator synthetic objects: 3 fields (tag, arg1, arg2)
pub(crate) const CMP_FIELD_TAG: usize = 0;
pub(crate) const CMP_FIELD_ARG1: usize = 1;
pub(crate) const CMP_FIELD_ARG2: usize = 2;
pub(crate) const CMP_NUM_FIELDS: usize = 3;

pub(crate) const CMP_TAG_NATURAL_ORDER: i32 = 1;
pub(crate) const CMP_TAG_REVERSE_ORDER: i32 = 2;
pub(crate) const CMP_TAG_COMPARING: i32 = 3;
pub(crate) const CMP_TAG_REVERSED: i32 = 4;
pub(crate) const CMP_TAG_THEN_COMPARING: i32 = 5;
// Primitive key-extractor comparators. `Comparator.comparingInt(ToIntFunction)`
// (and Long/Double) must invoke the extractor's primitive SAM
// (`applyAsInt`/`applyAsLong`/`applyAsDouble`), NOT `Function.apply` — the
// CMP_TAG_COMPARING arm calls `apply(Object)Object`, which does not exist on a
// `ToIntFunction` lambda. These distinct tags carry the SAM choice without
// widening the comparator's field layout. (Groovy/ANTLR4's
// `ParserATNSimulator.STATE_ALT_SORT_COMPARATOR = comparingInt(..).thenComparingInt(..)`
// hits exactly this path.)
pub(crate) const CMP_TAG_COMPARING_INT: i32 = 6;
pub(crate) const CMP_TAG_COMPARING_LONG: i32 = 7;
pub(crate) const CMP_TAG_COMPARING_DOUBLE: i32 = 8;

pub(crate) fn make_comparator(ctx: &mut dyn NativeContext, tag: i32) -> Result<ObjectRef, MethodCallFailed> {
    // Fallible since 2026-08-05 (JDK-only wave 2, lane L7). No JDK declares
    // `java.util.Comparator$Native`; it stands in for the comparator objects
    // the real `Comparator` factory methods return.
    let cmp = try_alloc_synthetic(ctx, "java/util/Comparator$Native", CMP_NUM_FIELDS)?;
    ctx.set_field(cmp, CMP_FIELD_TAG, Value::Int(tag));
    Ok(cmp)
}

/// Compare two values using a Comparator. If the comparator is a tagged factory
/// object, dispatch based on tag. Otherwise, fall through to invoke_virtual (for lambdas).
pub fn comparator_compare(
    ctx: &mut dyn NativeContext,
    comparator: ObjectRef,
    a: Value,
    b: Value,
) -> MethodCallResult {
    // Guard: only read the tag if the object has enough fields (factory comparators have 3).
    // Lambda proxies may have 0 fields, so reading field 0 would panic.
    let tag = if ctx.object_num_fields(comparator) >= CMP_NUM_FIELDS {
        match ctx.get_field(comparator, CMP_FIELD_TAG) {
            Value::Int(t) if (CMP_TAG_NATURAL_ORDER..=CMP_TAG_COMPARING_DOUBLE).contains(&t) => {
                Some(t)
            }
            _ => None,
        }
    } else {
        None
    };
    let tag = match tag {
        Some(t) => t,
        None => {
            // A descriptor-blind `thenComparing` dispatch can store a key
            // extractor lambda directly in the secondary-comparator slot. If
            // the receiver is one of those lambda proxies, skip the doomed
            // `compare(Object,Object)` probe and run the corresponding
            // `Comparator.comparing*` semantics up front.
            let lambda_iface = ctx.lambda_functional_interface(ctx.class_id_of_object(comparator));
            match lambda_iface.as_deref() {
                Some("java/util/function/Function") => {
                    return compare_with_key_function(ctx, comparator, a, b);
                }
                Some("java/util/function/ToIntFunction") => {
                    // Family-1 stale-ObjectRef fix (2026-07-13, follow-up):
                    // the first `applyAsInt` call can run arbitrary
                    // interpreted bytecode and trigger a moving GC. Not just
                    // `comparator` (the key-extractor lambda) but also `b`
                    // (the second element, computed AFTER that hazard) must
                    // be re-read — `a`/`b` are raw `ObjectRef`-carrying
                    // `Value`s just like `comparator`. See
                    // wildfly-parallel-boot-stale-objectref-residual.md.
                    let comparator_pin = ctx.pin_native_root(comparator);
                    let b_pin = pin_value(ctx, b);
                    let ia = comparing_key_as_i64(
                        ctx,
                        comparator,
                        "applyAsInt",
                        "(Ljava/lang/Object;)I",
                        a,
                    )?;
                    let comparator = ctx.read_native_pin(comparator_pin, comparator);
                    let b = read_pinned_elem(ctx, b_pin, b);
                    let ib = comparing_key_as_i64(
                        ctx,
                        comparator,
                        "applyAsInt",
                        "(Ljava/lang/Object;)I",
                        b,
                    )?;
                    ctx.unpin_native_roots(comparator_pin);
                    return Ok(Some(Value::Int(ia.cmp(&ib) as i32)));
                }
                Some("java/util/function/ToLongFunction") => {
                    // Family-1 stale-ObjectRef fix: same as ToIntFunction above.
                    let comparator_pin = ctx.pin_native_root(comparator);
                    let b_pin = pin_value(ctx, b);
                    let ia = comparing_key_as_i64(
                        ctx,
                        comparator,
                        "applyAsLong",
                        "(Ljava/lang/Object;)J",
                        a,
                    )?;
                    let comparator = ctx.read_native_pin(comparator_pin, comparator);
                    let b = read_pinned_elem(ctx, b_pin, b);
                    let ib = comparing_key_as_i64(
                        ctx,
                        comparator,
                        "applyAsLong",
                        "(Ljava/lang/Object;)J",
                        b,
                    )?;
                    ctx.unpin_native_roots(comparator_pin);
                    return Ok(Some(Value::Int(ia.cmp(&ib) as i32)));
                }
                Some("java/util/function/ToDoubleFunction") => {
                    // Family-1 stale-ObjectRef fix: same as ToIntFunction above.
                    let comparator_pin = ctx.pin_native_root(comparator);
                    let b_pin = pin_value(ctx, b);
                    let da = comparing_key_as_f64(ctx, comparator, a)?;
                    let comparator = ctx.read_native_pin(comparator_pin, comparator);
                    let b = read_pinned_elem(ctx, b_pin, b);
                    let db = comparing_key_as_f64(ctx, comparator, b)?;
                    ctx.unpin_native_roots(comparator_pin);
                    return Ok(Some(Value::Int(double_compare(da, db))));
                }
                _ => {}
            }
            // Not a factory comparator — invoke its `compare` (lambda path).
            //
            // Fallback: if `compare` does not resolve, the object is actually a
            // key-extractor `Function` that was stored RAW as a nested
            // comparator — a `someComparator.thenComparing(EntityTableMapping::
            // relativePosition)` whose dispatch landed on the `(Comparator)`
            // overload (`native_comparator_then_comparing`, inner_tag=None)
            // instead of `(Function)`. Treat it like `Comparator.comparing(keyFn)`
            // (apply + natural compare), matching the JDK default method. A real
            // Comparator lambda implements `compare` (not `apply`), so a genuine
            // exception from its `compare` still propagates (apply would fail and
            // we re-raise the original error). Without this, Hibernate's
            // `ConstraintModelBuilder` (Stream.sorted over the entity-table
            // comparator) threw `NoSuchMethodError: Function.compare`, which
            // unwound through the JUnit MethodHandle interceptor chain and tripped
            // "InvocationInterceptors called invocation multiple times".
            // Family-1 stale-ObjectRef fix: `comparator` is reused in the
            // `compare_with_key_function` fallback below if the first call
            // fails; root it across the attempt in case the failing call
            // still executed enough bytecode to trigger a moving GC.
            let comparator_pin = ctx.pin_native_root(comparator);
            return match ctx.invoke_virtual(
                comparator,
                "compare",
                "(Ljava/lang/Object;Ljava/lang/Object;)I",
                &[a, b],
            ) {
                Ok(v) => {
                    ctx.unpin_native_roots(comparator_pin);
                    Ok(v)
                }
                Err(compare_err) => {
                    let comparator = ctx.read_native_pin(comparator_pin, comparator);
                    ctx.unpin_native_roots(comparator_pin);
                    match compare_with_key_function(ctx, comparator, a, b) {
                        Ok(v) => Ok(v),
                        Err(_) => Err(compare_err),
                    }
                }
            };
        }
    };

    match tag {
        CMP_TAG_NATURAL_ORDER => natural_compare(ctx, &a, &b),
        CMP_TAG_REVERSE_ORDER => {
            let result = natural_compare(ctx, &a, &b)?;
            Ok(result.map(|v| match v {
                Value::Int(i) => Value::Int(-i),
                other => other,
            }))
        }
        CMP_TAG_COMPARING => {
            let key_fn = match ctx.get_field(comparator, CMP_FIELD_ARG1) {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Int(0))),
            };
            // Family-1 stale-ObjectRef fix (2026-07-13, follow-up): the
            // first `apply` call can run arbitrary interpreted bytecode
            // (the key extractor) and trigger a moving GC; `key_fn` AND
            // `b` (the second element, used AFTER that hazard) are both
            // raw `ObjectRef`-carrying `Value`s that must be re-read. See
            // wildfly-parallel-boot-stale-objectref-residual.md.
            let key_fn_pin = ctx.pin_native_root(key_fn);
            let b_pin = pin_value(ctx, b);
            let ka = ctx
                .invoke_virtual(
                    key_fn,
                    "apply",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                    &[a],
                )?
                .unwrap_or(Value::Object(None));
            let key_fn = ctx.read_native_pin(key_fn_pin, key_fn);
            let b = read_pinned_elem(ctx, b_pin, b);
            // Root-cause-2 fix (WildFly parallel-extension-add CCE family,
            // wildfly-remoting-classcastexception-parallel-extension-add-FIXED.md): `ka` -- the FIRST extracted key --
            // was read raw here and reused below at `natural_compare(ctx,
            // &ka, &kb)`, but the very next line's `invoke_virtual` (computing
            // `kb`) is exactly as GC-capable as the first one that produced
            // `ka` -- and unlike `key_fn`/`b` just above, `ka` was never
            // pinned across it. Live-captured via `EnhancedQueueExecutor
            // $ThreadBody` worker threads racing this window during
            // `parallel-extension-add`'s `ResourceDescriptor.addCapabilities`
            // (`Comparator.comparing(Capability::getName)`-shaped natural-
            // ordering comparator on a `TreeMap`): the stale `ka` read back
            // as a blank `java.lang.Object` (class id 0 -- which is both the
            // real bootstrap id of `java.lang.Object` *and* what a zeroed/
            // never-written header reads as, masking the staleness as a
            // superficially valid instance) against a freshly-extracted `kb`
            // `String`, producing `class java.lang.Object cannot be cast to
            // class java.lang.Comparable`. Pin `ka` across the second
            // `apply` call and refresh it immediately before use.
            let ka_pin = pin_value(ctx, ka);
            let kb = ctx
                .invoke_virtual(
                    key_fn,
                    "apply",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                    &[b],
                )?
                .unwrap_or(Value::Object(None));
            let ka = read_pinned_elem(ctx, ka_pin, ka);
            ctx.unpin_native_roots(key_fn_pin);
            natural_compare(ctx, &ka, &kb)
        }
        CMP_TAG_REVERSED => {
            let inner = match ctx.get_field(comparator, CMP_FIELD_ARG1) {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Int(0))),
            };
            let result = comparator_compare(ctx, inner, a, b)?;
            Ok(result.map(|v| match v {
                Value::Int(i) => Value::Int(-i),
                other => other,
            }))
        }
        CMP_TAG_THEN_COMPARING => {
            let primary = match ctx.get_field(comparator, CMP_FIELD_ARG1) {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Int(0))),
            };
            // Family-1 stale-ObjectRef fix (2026-07-13, follow-up): the
            // recursive `comparator_compare` call for `primary` can invoke
            // a real user Comparator/lambda, triggering a moving GC. Not
            // just `comparator` (used again below for CMP_FIELD_ARG2) but
            // also `a`/`b` — reused for the SECOND recursive call below —
            // must be re-read. This was the actual root cause of the
            // residual panics that survived the owner/data/comparator-only
            // fix (class_id 1295/1299 traces via `Comparator.lambda$
            // thenComparing$...` and `ResourceAttributesXMLContentReader
            // .<init>`). See
            // wildfly-parallel-boot-stale-objectref-residual.md.
            let comparator_pin = ctx.pin_native_root(comparator);
            let a_pin = pin_value(ctx, a);
            let b_pin = pin_value(ctx, b);
            let result = comparator_compare(ctx, primary, a, b)?;
            let comparator = ctx.read_native_pin(comparator_pin, comparator);
            let a = read_pinned_elem(ctx, a_pin, a);
            let b = read_pinned_elem(ctx, b_pin, b);
            ctx.unpin_native_roots(comparator_pin);
            match result {
                Some(Value::Int(0)) => {
                    let secondary = match ctx.get_field(comparator, CMP_FIELD_ARG2) {
                        Value::Object(Some(r)) => r,
                        _ => return Ok(Some(Value::Int(0))),
                    };
                    comparator_compare(ctx, secondary, a, b)
                }
                other => Ok(other),
            }
        }
        CMP_TAG_COMPARING_INT => {
            let key_fn = match ctx.get_field(comparator, CMP_FIELD_ARG1) {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Int(0))),
            };
            // Family-1 stale-ObjectRef fix: `key_fn` AND `b` (used after the
            // first GC-triggering `applyAsInt` call) both need re-reading.
            let key_fn_pin = ctx.pin_native_root(key_fn);
            let b_pin = pin_value(ctx, b);
            let ia = comparing_key_as_i64(ctx, key_fn, "applyAsInt", "(Ljava/lang/Object;)I", a)?;
            let key_fn = ctx.read_native_pin(key_fn_pin, key_fn);
            let b = read_pinned_elem(ctx, b_pin, b);
            let ib = comparing_key_as_i64(ctx, key_fn, "applyAsInt", "(Ljava/lang/Object;)I", b)?;
            ctx.unpin_native_roots(key_fn_pin);
            Ok(Some(Value::Int(ia.cmp(&ib) as i32)))
        }
        CMP_TAG_COMPARING_LONG => {
            let key_fn = match ctx.get_field(comparator, CMP_FIELD_ARG1) {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Int(0))),
            };
            // Family-1 stale-ObjectRef fix: `key_fn` AND `b` reused across
            // two GC-triggering `applyAsLong` calls.
            let key_fn_pin = ctx.pin_native_root(key_fn);
            let b_pin = pin_value(ctx, b);
            let ia = comparing_key_as_i64(ctx, key_fn, "applyAsLong", "(Ljava/lang/Object;)J", a)?;
            let key_fn = ctx.read_native_pin(key_fn_pin, key_fn);
            let b = read_pinned_elem(ctx, b_pin, b);
            let ib = comparing_key_as_i64(ctx, key_fn, "applyAsLong", "(Ljava/lang/Object;)J", b)?;
            ctx.unpin_native_roots(key_fn_pin);
            Ok(Some(Value::Int(ia.cmp(&ib) as i32)))
        }
        CMP_TAG_COMPARING_DOUBLE => {
            let key_fn = match ctx.get_field(comparator, CMP_FIELD_ARG1) {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Int(0))),
            };
            // Family-1 stale-ObjectRef fix: `key_fn` AND `b` reused across
            // two GC-triggering `applyAsDouble` calls.
            let key_fn_pin = ctx.pin_native_root(key_fn);
            let b_pin = pin_value(ctx, b);
            let da = comparing_key_as_f64(ctx, key_fn, a)?;
            let key_fn = ctx.read_native_pin(key_fn_pin, key_fn);
            let b = read_pinned_elem(ctx, b_pin, b);
            let db = comparing_key_as_f64(ctx, key_fn, b)?;
            ctx.unpin_native_roots(key_fn_pin);
            // Match java.lang.Double.compare ordering (NaN greatest, -0.0 < 0.0).
            Ok(Some(Value::Int(double_compare(da, db))))
        }
        _ => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn compare_with_key_function(
    ctx: &mut dyn NativeContext,
    key_fn: ObjectRef,
    a: Value,
    b: Value,
) -> MethodCallResult {
    // Family-1 stale-ObjectRef fix (2026-07-13, follow-up): the first
    // `apply` call can run arbitrary interpreted bytecode and trigger a
    // moving GC; `key_fn` AND `b` (used after that hazard) both need
    // re-reading. See
    // wildfly-parallel-boot-stale-objectref-residual.md.
    let key_fn_pin = ctx.pin_native_root(key_fn);
    let b_pin = pin_value(ctx, b);
    let ka = ctx
        .invoke_virtual(
            key_fn,
            "apply",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[a],
        )?
        .unwrap_or(Value::Object(None));
    let key_fn = ctx.read_native_pin(key_fn_pin, key_fn);
    let b = read_pinned_elem(ctx, b_pin, b);
    let kb = ctx
        .invoke_virtual(
            key_fn,
            "apply",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[b],
        )?
        .unwrap_or(Value::Object(None));
    ctx.unpin_native_roots(key_fn_pin);
    natural_compare(ctx, &ka, &kb)
}

/// Invoke a primitive key-extractor SAM (`applyAsInt`/`applyAsLong`) on `arg`
/// and coerce the result to `i64`. Used by the `comparingInt`/`comparingLong`
/// comparator arms.
pub(crate) fn comparing_key_as_i64(
    ctx: &mut dyn NativeContext,
    key_fn: ObjectRef,
    sam: &str,
    desc: &str,
    arg: Value,
) -> Result<i64, MethodCallFailed> {
    match ctx.invoke_virtual(key_fn, sam, desc, &[arg])? {
        Some(Value::Int(v)) => Ok(v as i64),
        Some(Value::Long(v)) => Ok(v),
        _ => Ok(0),
    }
}

/// Invoke `ToDoubleFunction.applyAsDouble` on `arg` and coerce to `f64`.
pub(crate) fn comparing_key_as_f64(
    ctx: &mut dyn NativeContext,
    key_fn: ObjectRef,
    arg: Value,
) -> Result<f64, MethodCallFailed> {
    match ctx.invoke_virtual(key_fn, "applyAsDouble", "(Ljava/lang/Object;)D", &[arg])? {
        Some(Value::Double(v)) => Ok(v),
        Some(Value::Float(v)) => Ok(v as f64),
        Some(Value::Int(v)) => Ok(v as f64),
        Some(Value::Long(v)) => Ok(v as f64),
        _ => Ok(0.0),
    }
}

/// `java.lang.Double.compare` semantics: total ordering with NaN greatest and
/// `-0.0 < 0.0` (so it is a valid `Comparator` even with NaN/zero keys).
pub(crate) fn double_compare(a: f64, b: f64) -> i32 {
    // Was a local transcription that fell back to RAW bits, so two NaNs with
    // different payloads compared unequal. See `cratonvm_types::jfp`.
    cratonvm_types::jfp::double_compare(a, b)
}

/// Natural ordering: compare by string content or by wrapper field 0 value.
pub(crate) fn natural_compare(ctx: &mut dyn NativeContext, a: &Value, b: &Value) -> MethodCallResult {
    match (a, b) {
        (Value::Object(Some(ra)), Value::Object(Some(rb))) => {
            // Try string comparison first.
            //
            // By UTF-16 code UNIT, which is what `String.compareTo` compares —
            // NOT `String::cmp`, which compares code points. The two disagree
            // above the BMP: a supplementary character begins with a surrogate
            // unit below `U+E000`, so Java sorts it BEFORE `U+E000..=U+FFFF`
            // and Rust sorts it after (MEASURED both VMs, `G55Ord.java`; see
            // `TreeKey`).
            //
            // And a decode that produced `U+FFFD` may have destroyed an
            // unpaired surrogate, in which case this comparison is not ours to
            // make: fall through to the real `Comparable.compareTo` dispatch at
            // the bottom, which runs the receiver's own units-exact
            // `String.compareTo`. Two keys mangled to the same `U+FFFD` text
            // would otherwise compare EQUAL and a natural-order
            // `TreeSet`/`TreeMap` would silently drop one of them.
            let sa = ctx.read_string(*ra);
            let sb = ctx.read_string(*rb);
            if let (Some(sa), Some(sb)) = (&sa, &sb) {
                if let Some(ord) = compare_decoded_strings(sa, sb) {
                    return Ok(Some(Value::Int(ord as i32)));
                }
            }
            // Try as primitive wrapper: unbox BOTH receivers and compare the
            // boxed primitives. CRITICAL — gate on `unbox_wrapper` (a SINGLE
            // primitive field), NOT a raw field-0 probe. A raw probe of slot 0
            // mis-fires on any entity / POJO whose first *declared* field is a
            // primitive: e.g. a JPA `@Id @GeneratedValue long id`. Two freshly
            // `new`'d, not-yet-persisted entities both have `id == 0`, so the
            // probe returned 0 ("equal") and a natural-order TreeSet/TreeMap
            // collapsed two distinct elements into one — dropping a child on
            // persist (HHH-8827 `SortNaturalTest`: `owner.cats` lost a Cat;
            // the real ordering lives in the entity's own `compareTo(name)`).
            // `unbox_wrapper` only matches genuine 1-field wrappers (Integer,
            // Long, Float, Double, …), so a multi-field Comparable falls through
            // to its real `Comparable.compareTo` below — the JDK's natural-order
            // semantics. (It also keeps the original behaviour of never probing
            // a zero-field object, avoiding the gen_heap out-of-bounds warning.)
            if let (Some(fa), Some(fb)) = (unbox_wrapper(&*ctx, *ra), unbox_wrapper(&*ctx, *rb)) {
                match (fa, fb) {
                    (Value::Int(a), Value::Int(b)) => {
                        return Ok(Some(Value::Int(a.cmp(&b) as i32)))
                    }
                    (Value::Long(a), Value::Long(b)) => {
                        return Ok(Some(Value::Int(a.cmp(&b) as i32)))
                    }
                    // `Float.compare`/`Double.compare`, NOT `total_cmp`: Java
                    // canonicalizes the NaN payload first, so all NaNs are
                    // equal to each other and greater than every number, where
                    // IEEE totalOrder sorts a negatively-signed NaN BELOW
                    // -infinity and orders NaNs among themselves by payload.
                    // With `total_cmp` a natural-order `TreeSet<Double>` built
                    // from 23 values held 13 elements where HotSpot holds 9,
                    // and `Comparator.naturalOrder().compare` disagreed with
                    // `Double.compare` on 206 of 529 sampled pairs.
                    (Value::Float(a), Value::Float(b)) => {
                        return Ok(Some(Value::Int(cratonvm_types::jfp::float_compare(a, b))))
                    }
                    (Value::Double(a), Value::Double(b)) => {
                        return Ok(Some(Value::Int(cratonvm_types::jfp::double_compare(a, b))))
                    }
                    _ => {}
                }
            }
            // Not a String and not a homogeneous primitive wrapper — dispatch
            // the element's real `Comparable.compareTo`. Without this,
            // comparator-less `TreeMap`/`TreeSet` of a custom `Comparable` key
            // (the array-mode path → `tree_compare`) silently treated every
            // element as equal (returned 0), collapsing the ordering and
            // dropping/dedup-ing entries (B2).
            Ok(Some(Value::Int(compare_via_compare_to(ctx, a, b)?)))
        }
        // Compare bare ints/longs/etc. (for comparingInt results)
        (Value::Int(a), Value::Int(b)) => Ok(Some(Value::Int(a.cmp(b) as i32))),
        (Value::Long(a), Value::Long(b)) => Ok(Some(Value::Int(a.cmp(b) as i32))),
        (Value::Float(a), Value::Float(b)) => {
            Ok(Some(Value::Int(cratonvm_types::jfp::float_compare(*a, *b))))
        }
        (Value::Double(a), Value::Double(b)) => Ok(Some(Value::Int(
            cratonvm_types::jfp::double_compare(*a, *b),
        ))),
        _ => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_comparator_compare_method(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [this, a, b]
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let a = args.get(1).cloned().unwrap_or(Value::Object(None));
    let b = args.get(2).cloned().unwrap_or(Value::Object(None));
    comparator_compare(ctx, this, a, b)
}

pub(crate) fn register_comparator_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
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
    registry.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        "java/util/Comparator$Native",
        "compare",
        "(Ljava/lang/Object;Ljava/lang/Object;)I",
        native_comparator_compare_method,
    );
    // Serialization substitution: replace the synthetic factory comparator with
    // the real serializable JDK singleton (so reverse/natural-ordered
    // TreeMap/TreeSet serialize byte-identically to HotSpot).
    registry.register(
        "java/util/Comparator$Native",
        "writeReplace",
        "()Ljava/lang/Object;",
        native_comparator_write_replace,
    );
    // H0 (2026-08-21) — the last standing `SUITE=all` failure.
    //
    // MEASURED on r10, Compatible mode: `Comparator.comparingInt(String::length)`
    // returns `java.util.Comparator$Native` and `naturalOrder()` returns the same,
    // where HotSpot 25.0.3+9 returns `Comparator$$Lambda` and
    // `Comparators$NaturalOrderComparator`. Every VALUE is right — compare/sort
    // results match the oracle exactly — and only the CLASS is fabricated, which
    // is why no value-diffing vector ever saw it and `RJdkFunctionCombinators`
    // catches it with a `getClass().getName()` screen (`notFabricated`, :392).
    //
    // Under `--jdk-only` this registrar is already dropped (it is `SyntheticStub`
    // above) and the real bytecode runs, which is why the vector PASSES strict and
    // fails only Compatible — `H15-1`'s pattern, now for the fifth time. This
    // guard finishes the job for a real image.
    //
    // WHY THE WHOLE `java/util/Comparator` BLOCK MOVES TOGETHER, and not just the
    // factories: these registrations are one cluster, not eight rows. The
    // factories MINT `Comparator$Native`, and `reversed`/`thenComparing*` are
    // registered *because* that synthetic receiver has no real interface
    // hierarchy or default-method bytecode (see the comment at
    // `thenComparingInt` below — unregistering them once produced
    // `AbstractMethodError: ... has no Code attribute` in Groovy/ANTLR4's
    // `ParserATNSimulator.STATE_ALT_SORT_COMPARATOR`). Guard the factories alone
    // and the defaults survive to intercept calls on REAL lambdas, with bodies
    // that expect a tagged receiver. `H4-1`'s rule: the unit of work is the
    // ownership cluster, not the registrar row.
    //
    // `Comparator$Native.compare`/`writeReplace` above stay registered
    // unconditionally: that class does not exist on a real image, so the rows are
    // unreachable there, and under `synthetic-jdk` they are the implementation.
    //
    // PRICE, from `H24-3` §5 and NOT yet measured at the time of writing: this is
    // a PATH SWITCH, not a deletion. Natural-ordered `TreeMap`/`sort` move from
    // Rust `natural_compare` to real `compareTo` bytecode. The arms are the test.
    if !registry.drops_real_layout_synthetic() {
        registry.register(
            "java/util/Comparator",
            "naturalOrder",
            "()Ljava/util/Comparator;",
            native_comparator_natural_order,
        );
        registry.register(
            "java/util/Comparator",
            "reverseOrder",
            "()Ljava/util/Comparator;",
            native_comparator_reverse_order,
        );
        registry.register(
            "java/util/Comparator",
            "comparing",
            "(Ljava/util/function/Function;)Ljava/util/Comparator;",
            native_comparator_comparing,
        );
        registry.register(
            "java/util/Comparator",
            "comparingInt",
            "(Ljava/util/function/ToIntFunction;)Ljava/util/Comparator;",
            native_comparator_comparing_int,
        );
        registry.register(
            "java/util/Comparator",
            "comparingLong",
            "(Ljava/util/function/ToLongFunction;)Ljava/util/Comparator;",
            native_comparator_comparing_long,
        );
        registry.register(
            "java/util/Comparator",
            "comparingDouble",
            "(Ljava/util/function/ToDoubleFunction;)Ljava/util/Comparator;",
            native_comparator_comparing_double,
        );
        registry.register(
            "java/util/Comparator",
            "reversed",
            "()Ljava/util/Comparator;",
            native_comparator_reversed,
        );
        registry.register(
            "java/util/Comparator",
            "thenComparing",
            "(Ljava/util/Comparator;)Ljava/util/Comparator;",
            native_comparator_then_comparing,
        );
        // `thenComparing(Function)` default = `thenComparing(comparing(keyExtractor))`.
        registry.register(
            "java/util/Comparator",
            "thenComparing",
            "(Ljava/util/function/Function;)Ljava/util/Comparator;",
            native_comparator_then_comparing_key,
        );
        // Primitive `thenComparing*` defaults = `thenComparing(comparing{Int,Long,Double}(keyExtractor))`.
        // These were previously UNregistered: invoking them on a synthetic
        // `Comparator$Native` (which has no real interface hierarchy or default-method
        // bytecode) produced a tagless comparator → `comparator_compare` fell to the
        // lambda branch → `invoke_virtual("compare")` resolved to the abstract
        // `Comparator.compare` → `AbstractMethodError: ... has no Code attribute`
        // (Groovy/ANTLR4 `ParserATNSimulator.STATE_ALT_SORT_COMPARATOR`).
        registry.register(
            "java/util/Comparator",
            "thenComparingInt",
            "(Ljava/util/function/ToIntFunction;)Ljava/util/Comparator;",
            native_comparator_then_comparing_int,
        );
        registry.register(
            "java/util/Comparator",
            "thenComparingLong",
            "(Ljava/util/function/ToLongFunction;)Ljava/util/Comparator;",
            native_comparator_then_comparing_long,
        );
        registry.register(
            "java/util/Comparator",
            "thenComparingDouble",
            "(Ljava/util/function/ToDoubleFunction;)Ljava/util/Comparator;",
            native_comparator_then_comparing_double,
        );
    } // end `if !drops_real_layout_synthetic()` — see the block comment above.
    registry.set_category(__prev_cat);
}

pub(crate) fn native_comparator_natural_order(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let cmp = make_comparator(ctx, CMP_TAG_NATURAL_ORDER)?;
    Ok(Some(Value::Object(Some(cmp))))
}

pub(crate) fn native_comparator_reverse_order(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let cmp = make_comparator(ctx, CMP_TAG_REVERSE_ORDER)?;
    Ok(Some(Value::Object(Some(cmp))))
}

/// `Comparator$Native.writeReplace()` — serialization substitution.
///
/// CratonVM models `Comparator.naturalOrder()` / `reverseOrder()` as a
/// synthetic tagged `Comparator$Native` object (so `comparator_compare` can
/// dispatch on the tag without an `invoke_virtual`). That synthetic class is
/// NOT Serializable and doesn't exist on a real JDK, so serializing it (e.g.
/// a reverse-ordered TreeMap/TreeSet's comparator) threw `NotSerializableException`
/// and aborted the stream. `ObjectOutputStream` honors `writeReplace()`: return
/// the REAL serializable JDK singleton so the stream is byte-identical to
/// HotSpot. `Collections.reverseOrder()` (not intercepted) yields the real
/// `Collections$ReverseComparator`; `Comparators$NaturalOrderComparator.INSTANCE`
/// is the natural-order singleton. Other tags (comparing/reversed/thenComparing)
/// have no JDK singleton — left unreplaced (rarely serialized).
/// If `cmp` is a synthetic `Comparator$Native` factory comparator, return its
/// real serializable JDK singleton (via `native_comparator_write_replace`);
/// otherwise return it unchanged. Used by collection `writeObject` natives that
/// serialize a comparator through `OOS.writeObject` (which doesn't run the
/// `writeReplace` reflection hook the way real `defaultWriteObject` does).
pub(crate) fn replace_synthetic_comparator_for_ser(ctx: &mut dyn NativeContext, cmp: Value) -> Value {
    if let Value::Object(Some(o)) = cmp {
        let cn = ctx
            .class_name_of_id(ctx.class_id_of_object(o))
            .unwrap_or_default();
        if cn == "java/util/Comparator$Native" {
            if let Ok(Some(rep)) = native_comparator_write_replace(ctx, &[cmp]) {
                return rep;
            }
        }
    }
    cmp
}

pub(crate) fn native_comparator_write_replace(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let tag = if ctx.object_num_fields(this) >= CMP_NUM_FIELDS {
        match ctx.get_field(this, CMP_FIELD_TAG) {
            Value::Int(t) => t,
            _ => 0,
        }
    } else {
        0
    };
    match tag {
        CMP_TAG_REVERSE_ORDER => ctx.invoke(
            "java/util/Collections",
            "reverseOrder",
            "()Ljava/util/Comparator;",
            &[],
        ),
        CMP_TAG_NATURAL_ORDER => {
            if let Ok(cid) =
                ctx.ensure_class_initialized("java/util/Comparators$NaturalOrderComparator")
            {
                if let Some(idx) = ctx.static_field_index_by_name(cid, "INSTANCE") {
                    if let v @ Value::Object(Some(_)) = ctx.get_static_field(cid, idx) {
                        return Ok(Some(v));
                    }
                }
            }
            Ok(Some(Value::Object(Some(this))))
        }
        _ => Ok(Some(Value::Object(Some(this)))),
    }
}

/// Build a key-extractor comparator (`comparing`/`comparingInt`/`comparingLong`/
/// `comparingDouble`) tagged so `comparator_compare` invokes the right SAM.
pub(crate) fn make_comparing(ctx: &mut dyn NativeContext, args: &[Value], tag: i32) -> MethodCallResult {
    let key_fn = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    // GC-SAFETY (Family-1 stale-ObjectRef fix, follow-up to the TreeMap/TreeSet
    // comparator_compare fix): `make_comparator` allocates the tagged synthetic
    // Comparator$Native object (via `alloc_synthetic` -> `ensure_class_initialized`
    // + the allocation itself), either of which can run a `<clinit>` and trigger a
    // moving GC. `key_fn` is a raw ObjectRef held across that call and reused
    // afterward in `set_field` -- pin it first and refresh before the write.
    let key_fn_pin = ctx.pin_native_root(key_fn);
    let cmp = make_comparator(ctx, tag)?;
    let key_fn = ctx.read_native_pin(key_fn_pin, key_fn);
    ctx.set_field(cmp, CMP_FIELD_ARG1, Value::Object(Some(key_fn)));
    ctx.unpin_native_roots(key_fn_pin);
    Ok(Some(Value::Object(Some(cmp))))
}

pub(crate) fn native_comparator_comparing(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    make_comparing(ctx, args, CMP_TAG_COMPARING)
}

pub(crate) fn native_comparator_comparing_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    make_comparing(ctx, args, CMP_TAG_COMPARING_INT)
}

pub(crate) fn native_comparator_comparing_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    make_comparing(ctx, args, CMP_TAG_COMPARING_LONG)
}

pub(crate) fn native_comparator_comparing_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    make_comparing(ctx, args, CMP_TAG_COMPARING_DOUBLE)
}

pub(crate) fn native_comparator_reversed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    // GC-SAFETY: same `make_comparator` allocation hazard as `make_comparing`
    // above -- pin `this` across it and refresh before the write-back.
    let this_pin = ctx.pin_native_root(this);
    let cmp = make_comparator(ctx, CMP_TAG_REVERSED)?;
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(cmp, CMP_FIELD_ARG1, Value::Object(Some(this)));
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(cmp))))
}

/// Build a `thenComparing*` comparator. `this` (arg 0) is the primary; the
/// secondary depends on `inner_tag`:
///   - `None`            → arg 1 IS already a `Comparator` (`thenComparing(Comparator)`).
///   - `Some(key_tag)`   → arg 1 is a key extractor; wrap it as a fresh
///     `comparing*`-tagged comparator (`thenComparing(Function)` /
///     `thenComparingInt/Long/Double` — matching the JDK defaults, e.g.
///     `thenComparingInt(f) == thenComparing(comparingInt(f))`).
pub(crate) fn make_then_comparing(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    inner_tag: Option<i32>,
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let arg1 = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    // GC-SAFETY (Family-1 stale-ObjectRef fix, follow-up to the TreeMap/TreeSet
    // comparator_compare fix -- this residual survived that fix because it's a
    // DIFFERENT native entry point: comparator *construction*, not comparator
    // *dispatch*). Building a `thenComparing*` comparator can allocate TWICE
    // (the inner key-extractor comparator when `inner_tag` is `Some`, then the
    // outer THEN_COMPARING comparator always) via `make_comparator` ->
    // `alloc_synthetic` -> `ensure_class_initialized` + the allocation itself,
    // either of which can run a `<clinit>` and trigger a moving GC. `this` and
    // `arg1` are raw ObjectRefs read from `args` before either allocation and
    // then written into fields afterward -- pin both up front, refresh right
    // before each write. Observed as `Comparator.lambda$thenComparing$...`
    // (class_id 6) panics surviving the TreeMap/TreeSet fix.
    let this_pin = ctx.pin_native_root(this);
    let arg1_pin = ctx.pin_native_root(arg1);
    let secondary = match inner_tag {
        None => arg1,
        Some(key_tag) => {
            let inner = make_comparator(ctx, key_tag)?;
            let arg1 = ctx.read_native_pin(arg1_pin, arg1);
            ctx.set_field(inner, CMP_FIELD_ARG1, Value::Object(Some(arg1)));
            inner
        }
    };
    let secondary_pin = ctx.pin_native_root(secondary);
    let cmp = make_comparator(ctx, CMP_TAG_THEN_COMPARING)?;
    let this = ctx.read_native_pin(this_pin, this);
    let secondary = ctx.read_native_pin(secondary_pin, secondary);
    ctx.set_field(cmp, CMP_FIELD_ARG1, Value::Object(Some(this)));
    ctx.set_field(cmp, CMP_FIELD_ARG2, Value::Object(Some(secondary)));
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(cmp))))
}

pub(crate) fn native_comparator_then_comparing(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    make_then_comparing(ctx, args, None)
}

pub(crate) fn native_comparator_then_comparing_key(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    make_then_comparing(ctx, args, Some(CMP_TAG_COMPARING))
}

pub(crate) fn native_comparator_then_comparing_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    make_then_comparing(ctx, args, Some(CMP_TAG_COMPARING_INT))
}

pub(crate) fn native_comparator_then_comparing_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    make_then_comparing(ctx, args, Some(CMP_TAG_COMPARING_LONG))
}

pub(crate) fn native_comparator_then_comparing_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    make_then_comparing(ctx, args, Some(CMP_TAG_COMPARING_DOUBLE))
}

// ===========================================================================
// Phase 13 Step 5: StringJoiner
// ===========================================================================

// Synthetic-fallback StringJoiner is a 5-field object, used only when no real
// JDK `java/util/StringJoiner` class is loaded (see
// `classloading::class_manager::synthetic_stub_fields`, which declares 5
// UNNAMED fields `_f0.._f4` for exactly this shape):
//   field 0 = delimiter (String)
//   field 1 = prefix (String or null)
//   field 2 = suffix (String or null)
//   field 3 = elements ArrayList
//   field 4 = emptyValue (String or null)
//
// In real-JDK mode `new`+`<init>` instead allocate an object shaped like the
// REAL class (7 NAMED fields: prefix, delimiter, suffix, elts:String[],
// size, len, emptyValue) — writing through the legacy indices above then
// silently lands on the wrong real fields (wrong type too: `elts` is a
// String[], not an ArrayList). See
// stringjoiner-synthetic-native-real-jdk-field-mismatch-FIXED.md.
// `sj_real_layout` resolves the real class's actual field indices by name
// when present; every entry point below branches on it. This is
// intentionally still a full from-scratch Rust reimplementation of
// StringJoiner's behavior (NOT a yield to real bytecode) even when it
// targets the real object's fields: running the real `add()` bytecode
// against a real StringJoiner exposes a separate, unrelated
// heap-reference-integrity defect (see interpreter.rs's
// `synthetic_stub_should_yield_to_real_bytecode` StringJoiner exclusion), so
// this native must keep owning execution either way.
//
// W7-77-guarded-slot-maps.md renamed these from `SJ_FIELD_*`. The old name read
// as "StringJoiner's fields" and they are not: on JDK 25.0.3.9 `javap -p
// java.util.StringJoiner` gives the transitive instance layout
//
//     0 prefix   1 delimiter   2 suffix   3 elts (String[])
//     4 size     5 len         6 emptyValue
//
// so slots 0 and 1 are `prefix`/`delimiter` SWAPPED against the map below, and
// slot 4 is `size`, an `int`, where the map says `emptyValue`, a String. Slot 2
// (`suffix`) and slot 3 (`elts`, the map's `ELEMENTS`) happen to line up, which
// is what makes this shape dangerous: two of five agree, both mismatched pairs
// are String-to-String, and every read resolves.
//
// THE MAP IS NOT WRONG — IT IS FOR A DIFFERENT CLASS. It describes the
// fabricated 5-slot stub (`_f0.._f4`), which really does have this layout. Both
// classes answer to the binary name `java/util/StringJoiner`, so no `SlotMap`
// can tell them apart by name and `SJ_STUB_SLOT_MAP` below is published knowing
// it will read as a disagreement against a loaded real class. That is correct:
// the row is meant to stay in the census, because a guarded row is not a clean
// row. The `_SYNTHETIC_SLOT_` infix is the part a reader sees before they reach
// for these constants on a real receiver.
//
// THE GUARD IS `sj_real_layout`, a CLASS-SIDE witness — it resolves all seven
// real field names or answers `None`, and is therefore all-or-nothing rather
// than per-receiver. Every entry point that touches a constant below branches on
// it first: `native_sj_init_delim`, `native_sj_init_full`, `native_sj_add`,
// `native_sj_merge`, `native_sj_set_empty_value` and `sj_build_string` (which
// `native_sj_to_string` and `native_sj_length` delegate to). `sj_read_elements`
// is the one helper that reads a constant without asking, and it is private and
// called only from the two legacy branches of `sj_build_string`/`native_sj_merge`.
// `sj_init_legacy` (gc-common w20-a) is its write-side twin: the shared body of
// the two constructors' legacy branches, private, and reached only from there.
// `guarded_slot_maps.rs::string_joiner_entry_points_still_branch_on_the_real_layout`
// mechanises exactly that list, so deleting the branch fails a test instead of
// silently writing a prefix into a real `delimiter`.
pub(crate) const SJ_SYNTHETIC_SLOT_DELIM: usize = 0;
pub(crate) const SJ_SYNTHETIC_SLOT_PREFIX: usize = 1;
pub(crate) const SJ_SYNTHETIC_SLOT_SUFFIX: usize = 2;
pub(crate) const SJ_SYNTHETIC_SLOT_ELEMENTS: usize = 3;
pub(crate) const SJ_SYNTHETIC_SLOT_EMPTY_VALUE: usize = 4;

/// What the legacy numeric path believes about `java/util/StringJoiner`,
/// published for `read_alias::verify_declared_slot_maps`.
///
/// Deliberately states the BELIEF, not JDK 25's layout: a map that publishes
/// the right answer sweeps clean and measures nothing. Three of these five will
/// report as `WrongField` whenever the real class is the loaded one — that is
/// the census row this record is about, and it leaves the census only when the
/// numeric path does.
pub static SJ_STUB_SLOT_MAP: cratonvm_native_api::read_alias::SlotMap =
    cratonvm_native_api::read_alias::SlotMap {
        class: "java/util/StringJoiner",
        slots: &[
            (SJ_SYNTHETIC_SLOT_DELIM, "delimiter"),
            (SJ_SYNTHETIC_SLOT_PREFIX, "prefix"),
            (SJ_SYNTHETIC_SLOT_SUFFIX, "suffix"),
            (SJ_SYNTHETIC_SLOT_ELEMENTS, "elts"),
            (SJ_SYNTHETIC_SLOT_EMPTY_VALUE, "emptyValue"),
        ],
        origin: "native-collections/src/lib.rs SJ_SYNTHETIC_SLOT_*",
    };

// JDK-ONLY-CLASSIFY: stub — `java.util.StringJoiner` declares zero ACC_NATIVE
// methods in JDK 25; all 7 registrations here shadow concrete bytecode using a
// fabricated 5-slot layout (`SJ_SYNTHETIC_SLOT_*` above) that does not match the real
// class. This is the repo's clearest worked example of why the category must be
// a per-registration decision rather than ambient state: the SAME seven
// registrations are emitted under `Bridge` by `register_string_joiner_natives`
// and under `SyntheticStub` by `register_string_joiner_stub_natives`, and the
// only thing that distinguishes them is the `kind` argument this helper was
// handed. Both are the same code registering the same triples; only the tag
// differs, and the tag decides whether they survive `--jdk-only`.
pub(crate) fn register_string_joiner_natives_with_category(
    registry: &mut NativeMethodRegistry,
    kind: cratonvm_native_api::NativeKind,
) {
    let __prev_cat = registry.current_category();
    registry.set_category(kind);
    // Unconditional and idempotent-by-pointer, so publishing from BOTH the
    // `Bridge` and the `SyntheticStub` registrar is one entry, not two. Not
    // gated on `CRATONVM_DBG_LAYOUT_ALIAS`: a run that enables the flag later
    // must still have something to sweep.
    cratonvm_native_api::read_alias::declare_slot_map(&SJ_STUB_SLOT_MAP);
    registry.register(
        "java/util/StringJoiner",
        "<init>",
        "(Ljava/lang/CharSequence;)V",
        native_sj_init_delim,
    );
    registry.register(
        "java/util/StringJoiner",
        "<init>",
        "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;Ljava/lang/CharSequence;)V",
        native_sj_init_full,
    );
    registry.register(
        "java/util/StringJoiner",
        "add",
        "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;",
        native_sj_add,
    );
    registry.register(
        "java/util/StringJoiner",
        "toString",
        "()Ljava/lang/String;",
        native_sj_to_string,
    );
    registry.register("java/util/StringJoiner", "length", "()I", native_sj_length);
    registry.register(
        "java/util/StringJoiner",
        "merge",
        "(Ljava/util/StringJoiner;)Ljava/util/StringJoiner;",
        native_sj_merge,
    );
    registry.register(
        "java/util/StringJoiner",
        "setEmptyValue",
        "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;",
        native_sj_set_empty_value,
    );
    registry.set_category(__prev_cat);
}

pub(crate) fn register_string_joiner_natives(registry: &mut NativeMethodRegistry) {
    register_string_joiner_natives_with_category(registry, cratonvm_native_api::NativeKind::Bridge);
}

/// Register the synthetic StringJoiner fallback surface without stealing real
/// JDK bytecode. Real-JDK mode drops the Bridge registrar above because its
/// field layout corrupts a real StringJoiner, but synthetic fallback classes
/// still need these methods when no real bytecode exists.
pub fn register_string_joiner_stub_natives(registry: &mut NativeMethodRegistry) {
    register_string_joiner_natives_with_category(
        registry,
        cratonvm_native_api::NativeKind::SyntheticStub,
    );
}

/// Real JDK `java/util/StringJoiner`'s actual field indices, resolved by
/// name. `None` when only the synthetic 5-field fallback class is loaded (no
/// real bytecode present) — callers fall back to the legacy
/// `SJ_SYNTHETIC_SLOT_*` constants/ArrayList-of-elements representation in that
/// case.
///
/// This is the guard for the whole `SJ_SYNTHETIC_SLOT_*` census row (W7-77).
/// It is a CLASS-side witness, not a receiver-side one, and it is
/// all-or-nothing: one unresolvable name out of seven sends every entry point
/// down the legacy path. On a loaded real class that would write a prefix into
/// `delimiter` and an `emptyValue` String into the `int` `size`.
pub(crate) struct SjRealLayout {
    prefix: usize,
    delimiter: usize,
    suffix: usize,
    elts: usize,
    size: usize,
    len: usize,
    empty_value: usize,
}

pub(crate) fn sj_real_layout(ctx: &dyn NativeContext) -> Option<SjRealLayout> {
    const CN: &str = "java/util/StringJoiner";
    Some(SjRealLayout {
        prefix: ctx.resolve_field_index(CN, "prefix")?,
        delimiter: ctx.resolve_field_index(CN, "delimiter")?,
        suffix: ctx.resolve_field_index(CN, "suffix")?,
        elts: ctx.resolve_field_index(CN, "elts")?,
        size: ctx.resolve_field_index(CN, "size")?,
        len: ctx.resolve_field_index(CN, "len")?,
        empty_value: ctx.resolve_field_index(CN, "emptyValue")?,
    })
}

/// `String.valueOf`-style coercion of a (possibly null) `CharSequence` arg to
/// a guaranteed non-null real `java.lang.String` object.
pub(crate) fn sj_coerce_string(ctx: &mut dyn NativeContext, v: Value) -> Value {
    let s = match v {
        Value::Object(Some(r)) => ctx.read_string(r).unwrap_or_default(),
        _ => String::new(),
    };
    Value::Object(Some(ctx.create_string(&s)))
}

pub(crate) fn native_sj_init_delim(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let delim = match args.get(1) {
        Some(Value::Object(Some(r))) => Value::Object(Some(*r)),
        _ => Value::Object(None),
    };
    // gc-common w20-a: `this` and `delim` are bare copies of the arguments,
    // and every coerced String below is an allocation. The receiver's
    // argument slot is rooted, but this copy of it is not remapped: each
    // store after the first went through its pre-move address (and `delim`
    // was read through a stale one). Pinned, and read back after each
    // allocation; `this_pin` is the frame's base.
    let this_pin = ctx.pin_native_root(this);
    let delim_pin = pin_value(ctx, delim);
    let out = (|| -> MethodCallResult {
        if let Some(layout) = sj_real_layout(ctx) {
            // Store each coerced String into `this` immediately after creating
            // it (rather than creating all three up front) so no raw String is
            // held across a *later* allocation: once stored it is reachable
            // from the (pinned) receiver.
            let prefix_s = sj_coerce_string(ctx, Value::Object(None));
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field(this, layout.prefix, prefix_s);
            let delim = read_pinned_elem(ctx, delim_pin, delim);
            let delim_s = sj_coerce_string(ctx, delim);
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field(this, layout.delimiter, delim_s);
            let suffix_s = sj_coerce_string(ctx, Value::Object(None));
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field(this, layout.suffix, suffix_s);
            ctx.set_field(this, layout.elts, Value::Object(None));
            ctx.set_field(this, layout.size, Value::Int(0));
            ctx.set_field(this, layout.len, Value::Int(0));
            ctx.set_field(this, layout.empty_value, Value::Object(None));
            return Ok(None);
        }

        // Legacy 5-field synthetic fallback (no real class loaded).
        let rows = [
            (SJ_SYNTHETIC_SLOT_DELIM, delim_pin, delim),
            (SJ_SYNTHETIC_SLOT_PREFIX, usize::MAX, Value::Object(None)),
            (SJ_SYNTHETIC_SLOT_SUFFIX, usize::MAX, Value::Object(None)),
        ];
        sj_init_legacy(ctx, this_pin, this, rows)
    })();
    ctx.unpin_native_roots(this_pin);
    out
}

/// The legacy 5-field synthetic `StringJoiner` constructor body shared by
/// [`native_sj_init_delim`] and [`native_sj_init_full`]: a fresh internal
/// `ArrayList` of capacity 10 in `SJ_SYNTHETIC_SLOT_ELEMENTS`, no empty
/// value, and each `(slot, pin, value)` row stored with `value` read back
/// through `pin` (`usize::MAX` for a non-reference).
///
/// gc-common w20-a: the list is rooted across its backing array's
/// allocation, and the receiver and the row values are read back through
/// their pins after both allocations (they were stored through their
/// pre-allocation addresses).
pub(crate) fn sj_init_legacy(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
    rows: [(usize, usize, Value); 3],
) -> MethodCallResult {
    let elements = try_alloc_synthetic(ctx, "java/util/ArrayList", 2)?;
    let elements_pin = ctx.pin_native_root(elements);
    let backing: ObjectRef = alloc_ref_array(ctx, 10);
    let elements = ctx.read_native_pin(elements_pin, elements);
    ctx.unpin_native_roots(elements_pin);
    ctx.set_field(elements, 0, Value::Object(Some(backing)));
    ctx.set_field(elements, 1, Value::Int(0));

    let this = ctx.read_native_pin(this_pin, this);
    for (slot, pin, value) in rows {
        let value = read_pinned_elem(ctx, pin, value);
        ctx.set_field(this, slot, value);
    }
    ctx.set_field(
        this,
        SJ_SYNTHETIC_SLOT_ELEMENTS,
        Value::Object(Some(elements)),
    );
    ctx.set_field(this, SJ_SYNTHETIC_SLOT_EMPTY_VALUE, Value::Object(None));
    Ok(None)
}

pub(crate) fn native_sj_init_full(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let delim = match args.get(1) {
        Some(Value::Object(Some(r))) => Value::Object(Some(*r)),
        _ => Value::Object(None),
    };
    let prefix = match args.get(2) {
        Some(Value::Object(Some(r))) => Value::Object(Some(*r)),
        _ => Value::Object(None),
    };
    let suffix = match args.get(3) {
        Some(Value::Object(Some(r))) => Value::Object(Some(*r)),
        _ => Value::Object(None),
    };

    // gc-common w20-a: the same stale-receiver shape as
    // `native_sj_init_delim`, with three string arguments: all four are
    // pinned and read back after each allocation; `this_pin` is the base.
    let this_pin = ctx.pin_native_root(this);
    let delim_pin = pin_value(ctx, delim);
    let prefix_pin = pin_value(ctx, prefix);
    let suffix_pin = pin_value(ctx, suffix);
    let out = (|| -> MethodCallResult {
        if let Some(layout) = sj_real_layout(ctx) {
            let prefix = read_pinned_elem(ctx, prefix_pin, prefix);
            let prefix_s = sj_coerce_string(ctx, prefix);
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field(this, layout.prefix, prefix_s);
            let delim = read_pinned_elem(ctx, delim_pin, delim);
            let delim_s = sj_coerce_string(ctx, delim);
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field(this, layout.delimiter, delim_s);
            let suffix = read_pinned_elem(ctx, suffix_pin, suffix);
            let suffix_s = sj_coerce_string(ctx, suffix);
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field(this, layout.suffix, suffix_s);
            ctx.set_field(this, layout.elts, Value::Object(None));
            ctx.set_field(this, layout.size, Value::Int(0));
            ctx.set_field(this, layout.len, Value::Int(0));
            ctx.set_field(this, layout.empty_value, Value::Object(None));
            return Ok(None);
        }

        // Legacy 5-field synthetic fallback (no real class loaded).
        let rows = [
            (SJ_SYNTHETIC_SLOT_DELIM, delim_pin, delim),
            (SJ_SYNTHETIC_SLOT_PREFIX, prefix_pin, prefix),
            (SJ_SYNTHETIC_SLOT_SUFFIX, suffix_pin, suffix),
        ];
        sj_init_legacy(ctx, this_pin, this, rows)
    })();
    ctx.unpin_native_roots(this_pin);
    out
}

pub(crate) fn native_sj_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let element = match args.get(1) {
        Some(v) => *v,
        _ => Value::Object(None),
    };
    // gc-common w20-a: `this` and `element` are bare copies of the arguments.
    // The comments below used to say `this` "stays rooted for the whole native
    // call, so re-reading its field is always safe" -- the argument slot is
    // rooted, but this function's copy of it is never remapped, so every
    // "fresh" re-read after `alloc_ref_array` / `create_string` went through
    // the receiver's pre-move address (and the legacy branch stored `element`
    // and the grown array through stale addresses too). Both are pinned for
    // the whole call now and read back after every allocation; `this_pin` is
    // the frame's base, released on every path below.
    let this_pin = ctx.pin_native_root(this);
    let element_pin = pin_value(ctx, element);
    let out = (|| -> MethodCallResult {
        if let Some(layout) = sj_real_layout(ctx) {
            // `String.valueOf(newElement)`: null -> "null", else `.toString()`.
            // `read_string` only succeeds for an actual `java.lang.String`; a
            // `StringBuilder`/`StringBuffer`/other `CharSequence` element (a
            // common `StringJoiner.add` argument -- e.g. Spring's
            // `AbstractSqlParameterSource.toString()` builds each entry as a
            // `StringBuilder`) needs its real `toString()` invoked, exactly like
            // real `StringJoiner.add`'s `String.valueOf(newElement)`. Without
            // this fallback, every non-String element silently became the
            // literal text "null" instead of its real content.
            // Converted to a plain Rust `String` immediately (no live
            // `ObjectRef` held across the allocations below).
            let elt_str = match element {
                Value::Object(Some(r)) => match ctx.read_string(r) {
                    Some(s) => s,
                    None => {
                        let result = ctx.invoke_virtual(r, "toString", "()Ljava/lang/String;", &[]);
                        match result {
                            Ok(Some(Value::Object(Some(sref)))) => {
                                ctx.read_string(sref).unwrap_or_else(|| "null".to_string())
                            }
                            _ => "null".to_string(),
                        }
                    }
                },
                _ => "null".to_string(),
            };

            let this = ctx.read_native_pin(this_pin, this);
            let size = match ctx.get_field(this, layout.size) {
                Value::Int(n) => n,
                _ => 0,
            };
            let has_elts = matches!(ctx.get_field(this, layout.elts), Value::Object(Some(_)));

            let base_len = if !has_elts {
                // First-ever add(): allocate `elts = new String[8]`, matching
                // real StringJoiner. No delimiter is prepended to `len` yet.
                let arr: ObjectRef = alloc_ref_array(ctx, 8);
                let this = ctx.read_native_pin(this_pin, this);
                ctx.set_field(this, layout.elts, Value::Object(Some(arr)));
                0i32
            } else {
                // `elts` is re-fetched from the (re-read) receiver after every
                // allocation rather than held raw across one.
                let arr: ObjectRef = match ctx.get_field(this, layout.elts) {
                    Value::Object(Some(r)) => r,
                    _ => return Ok(Some(Value::Object(Some(this)))),
                };
                let capacity = ctx.array_length(arr);
                let delim_len = match ctx.get_field(this, layout.delimiter) {
                    Value::Object(Some(r)) => {
                        ctx.read_string(r).map(|s| s.len() as i32).unwrap_or(0)
                    }
                    _ => 0,
                };
                let existing_len = match ctx.get_field(this, layout.len) {
                    Value::Int(n) => n,
                    _ => 0,
                };
                let base_len = existing_len + delim_len;
                if size as usize >= capacity {
                    // Overflow-safe doubling capped at 1<<30, mirroring the
                    // legacy fallback's own growth policy below.
                    const SJ_MAX_CAPACITY: usize = 1 << 30;
                    let new_cap = std::cmp::min(
                        std::cmp::max(capacity.saturating_mul(2), capacity.saturating_add(1)),
                        SJ_MAX_CAPACITY,
                    );
                    if new_cap <= capacity {
                        return Err(cratonvm_types::error::RuntimeError::OutOfMemoryError {
                            message: format!(
                                "StringJoiner backing array exceeds the {SJ_MAX_CAPACITY}-element limit"
                            ),
                        }
                        .into());
                    }
                    let new_arr: ObjectRef = alloc_ref_array(ctx, new_cap);
                    // Re-read the receiver, then the OLD array from it
                    // (post-allocation), before reading out of it.
                    let this = ctx.read_native_pin(this_pin, this);
                    let arr: ObjectRef = match ctx.get_field(this, layout.elts) {
                        Value::Object(Some(r)) => r,
                        _ => return Ok(Some(Value::Object(Some(this)))),
                    };
                    for i in 0..capacity {
                        let v = ctx.get_array_element(arr, i);
                        ctx.set_array_element(new_arr, i, v);
                    }
                    ctx.set_field(this, layout.elts, Value::Object(Some(new_arr)));
                }
                base_len
            };

            let elt_obj = ctx.create_string(&elt_str);
            // Re-read the receiver and re-fetch `elts` once more: `create_string`
            // above is itself an allocation.
            let this = ctx.read_native_pin(this_pin, this);
            let elts_arr: ObjectRef = match ctx.get_field(this, layout.elts) {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Object(Some(this)))),
            };
            ctx.set_array_element(elts_arr, size as usize, Value::Object(Some(elt_obj)));
            ctx.set_field(this, layout.size, Value::Int(size + 1));
            ctx.set_field(
                this,
                layout.len,
                Value::Int(base_len + elt_str.len() as i32),
            );
            return Ok(Some(Value::Object(Some(this))));
        }

        // Legacy 5-field synthetic fallback: elements live in an internal
        // ArrayList (field SJ_SYNTHETIC_SLOT_ELEMENTS), added to below.
        let this = ctx.read_native_pin(this_pin, this);
        let elements: ObjectRef = match ctx.get_field(this, SJ_SYNTHETIC_SLOT_ELEMENTS) {
            Value::Object(Some(r)) => r,
            _ => return Ok(Some(Value::Object(Some(this)))),
        };
        // Read current size
        let size = match ctx.get_field(elements, 1) {
            Value::Int(n) => n,
            _ => 0,
        };
        let backing: ObjectRef = match ctx.get_field(elements, 0) {
            Value::Object(Some(r)) => r,
            _ => return Ok(Some(Value::Object(Some(this)))),
        };
        let capacity = ctx.array_length(backing);
        if size as usize >= capacity {
            // Overflow-safe doubling capped at 1<<30, mirroring `al_ensure_capacity`
            // (lib.rs ~690). `saturating_mul` prevents `usize` overflow on absurd
            // capacities; the cap then bounds the allocation. We always need room
            // for at least one more element, so floor the growth at `capacity + 1`.
            const SJ_MAX_CAPACITY: usize = 1 << 30;
            let new_cap = std::cmp::min(
                std::cmp::max(capacity.saturating_mul(2), capacity.saturating_add(1)),
                SJ_MAX_CAPACITY,
            );
            if new_cap <= capacity {
                // Already at the cap and full — refuse to grow further rather than
                // allocating an array that cannot hold the new element.
                return Err(cratonvm_types::error::RuntimeError::OutOfMemoryError {
                    message: format!(
                        "StringJoiner backing array exceeds the {SJ_MAX_CAPACITY}-element limit"
                    ),
                }
                .into());
            }
            let new_backing: ObjectRef = alloc_ref_array(ctx, new_cap);
            // The receiver, its element list and the old backing array are
            // re-derived from the pin after the allocation.
            let this = ctx.read_native_pin(this_pin, this);
            let elements: ObjectRef = match ctx.get_field(this, SJ_SYNTHETIC_SLOT_ELEMENTS) {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Object(Some(this)))),
            };
            let backing: ObjectRef = match ctx.get_field(elements, 0) {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Object(Some(this)))),
            };
            for i in 0..capacity {
                let v = ctx.get_array_element(backing, i);
                ctx.set_array_element(new_backing, i, v);
            }
            ctx.set_field(elements, 0, Value::Object(Some(new_backing)));
            let element = read_pinned_elem(ctx, element_pin, element);
            ctx.set_array_element(new_backing, size as usize, element);
            ctx.set_field(elements, 1, Value::Int(size + 1));
            return Ok(Some(Value::Object(Some(this))));
        }
        ctx.set_array_element(backing, size as usize, element);
        ctx.set_field(elements, 1, Value::Int(size + 1));
        Ok(Some(Value::Object(Some(this))))
    })();
    ctx.unpin_native_roots(this_pin);
    out
}

/// Helper: read all elements from a legacy-layout StringJoiner's internal
/// ArrayList as strings. Only used on the synthetic-fallback path.
pub(crate) fn sj_read_elements(ctx: &mut dyn NativeContext, sj: ObjectRef) -> Vec<String> {
    let elements = match ctx.get_field(sj, SJ_SYNTHETIC_SLOT_ELEMENTS) {
        Value::Object(Some(r)) => r,
        _ => return Vec::new(),
    };
    let size = match ctx.get_field(elements, 1) {
        Value::Int(n) => n as usize,
        _ => 0,
    };
    let backing = match ctx.get_field(elements, 0) {
        Value::Object(Some(r)) => r,
        _ => return Vec::new(),
    };
    let mut result = Vec::with_capacity(size);
    for i in 0..size {
        let elem = ctx.get_array_element(backing, i);
        let s = match elem {
            Value::Object(Some(r)) => ctx.read_string(r).unwrap_or_else(|| "null".to_string()),
            _ => "null".to_string(),
        };
        result.push(s);
    }
    result
}

/// Read all elements from a real-layout StringJoiner's `elts: String[]`
/// (first `size` slots) as strings.
pub(crate) fn sj_read_elements_real(
    ctx: &mut dyn NativeContext,
    layout: &SjRealLayout,
    sj: ObjectRef,
) -> Vec<String> {
    let size = match ctx.get_field(sj, layout.size) {
        Value::Int(n) => n as usize,
        _ => 0,
    };
    let elts = match ctx.get_field(sj, layout.elts) {
        Value::Object(Some(r)) => r,
        _ => return Vec::new(),
    };
    let mut result = Vec::with_capacity(size);
    for i in 0..size {
        let s = match ctx.get_array_element(elts, i) {
            Value::Object(Some(r)) => ctx.read_string(r).unwrap_or_else(|| "null".to_string()),
            _ => "null".to_string(),
        };
        result.push(s);
    }
    result
}

pub(crate) fn sj_build_string(ctx: &mut dyn NativeContext, sj: ObjectRef) -> String {
    if let Some(layout) = sj_real_layout(ctx) {
        let prefix = match ctx.get_field(sj, layout.prefix) {
            Value::Object(Some(r)) => ctx.read_string(r).unwrap_or_default(),
            _ => String::new(),
        };
        let suffix = match ctx.get_field(sj, layout.suffix) {
            Value::Object(Some(r)) => ctx.read_string(r).unwrap_or_default(),
            _ => String::new(),
        };
        let size = match ctx.get_field(sj, layout.size) {
            Value::Int(n) => n,
            _ => 0,
        };
        if size == 0 {
            if let Value::Object(Some(ev)) = ctx.get_field(sj, layout.empty_value) {
                return ctx.read_string(ev).unwrap_or_default();
            }
            return format!("{prefix}{suffix}");
        }
        let delim = match ctx.get_field(sj, layout.delimiter) {
            Value::Object(Some(r)) => ctx.read_string(r).unwrap_or_default(),
            _ => String::new(),
        };
        let elements = sj_read_elements_real(ctx, &layout, sj);
        let joined = elements.join(&delim);
        return format!("{prefix}{joined}{suffix}");
    }

    // Legacy 5-field synthetic fallback.
    let elements = sj_read_elements(ctx, sj);
    let delim = match ctx.get_field(sj, SJ_SYNTHETIC_SLOT_DELIM) {
        Value::Object(Some(r)) => ctx.read_string(r).unwrap_or_default(),
        _ => String::new(),
    };
    let prefix = match ctx.get_field(sj, SJ_SYNTHETIC_SLOT_PREFIX) {
        Value::Object(Some(r)) => ctx.read_string(r).unwrap_or_default(),
        _ => String::new(),
    };
    let suffix = match ctx.get_field(sj, SJ_SYNTHETIC_SLOT_SUFFIX) {
        Value::Object(Some(r)) => ctx.read_string(r).unwrap_or_default(),
        _ => String::new(),
    };

    if elements.is_empty() {
        // Check emptyValue
        if let Value::Object(Some(ev)) = ctx.get_field(sj, SJ_SYNTHETIC_SLOT_EMPTY_VALUE) {
            return ctx.read_string(ev).unwrap_or_default();
        }
        return format!("{prefix}{suffix}");
    }

    let joined = elements.join(&delim);
    format!("{prefix}{joined}{suffix}")
}

pub(crate) fn native_sj_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let result = sj_build_string(ctx, this);
    let s = ctx.create_string(&result);
    Ok(Some(Value::Object(Some(s))))
}

pub(crate) fn native_sj_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let result = sj_build_string(ctx, this);
    Ok(Some(Value::Int(result.len() as i32)))
}

pub(crate) fn native_sj_merge(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(Some(this)))),
    };

    if let Some(layout) = sj_real_layout(ctx) {
        let other_elements = sj_read_elements_real(ctx, &layout, other);
        if other_elements.is_empty() {
            return Ok(Some(Value::Object(Some(this))));
        }
        let other_delim = match ctx.get_field(other, layout.delimiter) {
            Value::Object(Some(r)) => ctx.read_string(r).unwrap_or_default(),
            _ => String::new(),
        };
        return sj_add_joined(ctx, this, &other_elements.join(&other_delim));
    }

    // Merge: add all elements from other into this (without other's prefix/suffix)
    let other_elements = sj_read_elements(ctx, other);
    if other_elements.is_empty() {
        return Ok(Some(Value::Object(Some(this))));
    }
    // Join other's elements with other's delimiter and add as a single element
    let other_delim = match ctx.get_field(other, SJ_SYNTHETIC_SLOT_DELIM) {
        Value::Object(Some(r)) => ctx.read_string(r).unwrap_or_default(),
        _ => String::new(),
    };
    // Add as single element to this
    sj_add_joined(ctx, this, &other_elements.join(&other_delim))
}

/// `this.add(merged)`: the tail both [`native_sj_merge`] branches share.
///
/// gc-common w20-a: the receiver is rooted across the String's allocation;
/// both branches used to hand the add the receiver's pre-allocation address.
pub(crate) fn sj_add_joined(ctx: &mut dyn NativeContext, this: ObjectRef, merged: &str) -> MethodCallResult {
    let (this, merged_str) = rooted_across1(ctx, this, |ctx| ctx.create_string(merged));
    native_sj_add(
        ctx,
        &[Value::Object(Some(this)), Value::Object(Some(merged_str))],
    )
}

pub(crate) fn native_sj_set_empty_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let empty_val = match args.get(1) {
        Some(v) => *v,
        _ => Value::Object(None),
    };

    if let Some(layout) = sj_real_layout(ctx) {
        // gc-common w20-a: rooted across the coerced String's allocation (it
        // was stored through the receiver's pre-allocation address).
        let (this, coerced) = rooted_across1(ctx, this, |ctx| sj_coerce_string(ctx, empty_val));
        ctx.set_field(this, layout.empty_value, coerced);
        return Ok(Some(Value::Object(Some(this))));
    }

    ctx.set_field(this, SJ_SYNTHETIC_SLOT_EMPTY_VALUE, empty_val);
    Ok(Some(Value::Object(Some(this))))
}

// ===========================================================================
// Phase 14 Step 1: java.util.Random
// ===========================================================================

// Random = 2-field synthetic (field 0 = Long seed, field 1 = cached second
// gaussian variate). Field 1 is what classloading's
// `"java/util/Random" => instance_fields(2)` calls "has-next-gaussian".
pub(crate) const RND_FIELD_SEED: usize = 0;
/// `java.util.Random.nextNextGaussian` + `haveNextNextGaussian`, folded into
/// one slot: `Value::Double(x)` means "cached", anything else (an unwritten
/// slot reads back `Object(None)` by index) means "empty".
pub(crate) const RND_FIELD_NEXT_GAUSSIAN: usize = 1;

/// The same real-JDK Random policy as native-builtins' registrar. The
/// synthetic layout below stores a primitive seed where the real class stores
/// an AtomicLong reference, so both registration sites must yield together.
/// `CRATONVM_JDK_RANDOM=0` restores the legacy shadow; synthetic-JDK always
/// keeps it. Read at registration without retaining process-global policy.
pub(crate) fn jdk_random_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JDK_RANDOM")
}

#[cfg(test)]
mod random_registration_policy_tests {
    use super::*;
    use cratonvm_native_api::NativeKind;
    use cratonvm_types::compat::CompatibilityMode;

    #[test]
    fn real_jdk_yields_in_both_modes_and_the_shadow_is_an_explicit_opt_out() {
        for mode in [CompatibilityMode::JdkOnly, CompatibilityMode::Compatible] {
            for (flag, registered) in [(None, false), (Some("1"), false), (Some("0"), true)] {
                cratonvm_types::flags::with_thread_overrides(
                    &[("CRATONVM_JDK_RANDOM", flag)],
                    || {
                        let mut r = NativeMethodRegistry::new();
                        r.set_compatibility_mode(mode);
                        r.set_real_jdk(true);
                        r.set_category(NativeKind::Bridge);
                        register_random_natives(&mut r);
                        for (name, desc) in [("<init>", "(J)V"), ("nextInt", "()I")] {
                            assert_eq!(
                                r.find("java/util/Random", name, desc).is_some(),
                                registered
                            );
                        }
                        assert_eq!(r.current_category(), Some(NativeKind::Bridge));
                    },
                );
            }
        }
    }

    #[test]
    fn synthetic_library_keeps_random_even_when_the_real_jdk_shadow_is_retired() {
        cratonvm_types::flags::with_thread_overrides(&[("CRATONVM_JDK_RANDOM", Some("1"))], || {
            let mut r = NativeMethodRegistry::new();
            r.set_real_jdk(false);
            r.set_category(NativeKind::Bridge);
            register_random_natives(&mut r);
            assert!(r.find("java/util/Random", "<init>", "(J)V").is_some());
            assert!(r.find("java/util/Random", "nextInt", "()I").is_some());
            assert_eq!(r.current_category(), Some(NativeKind::Bridge));
        });
    }
}

pub(crate) fn register_random_natives(registry: &mut NativeMethodRegistry) {
    // THIS SHAPE IS SYNTHETIC-JDK ONLY, and on a real JDK it is actively wrong.
    //
    // It keeps the seed in FIELD 0 of the receiver (`RND_FIELD_SEED`), which is
    // what the synthetic `java/util/Random` layout declares. The REAL
    // `java.util.Random` has `private final AtomicLong seed` in that slot — an
    // object reference, not a long — so these bodies read and write the wrong
    // thing and every draw comes back 0.
    //
    // That never showed because `securerandom.rs` registers the same ten
    // triples afterwards and registration is LAST-WRITE-WINS, so this bridge
    // was dead in compatible mode. Retiring THAT shadow uncovered this one:
    // `--dump-native-registry` on the first attempt showed `nextInt()I` served
    // by `kind: "bridge"`, `registered_by: native-collections/src/lib.rs`,
    // `invocations: 7` — and `RandomSpec` printing eight rows of zeros while
    // `AtomicLongSpec` proved the CAS loop underneath was byte-identical to
    // HotSpot.
    //
    // So both sites take the same gate. On a real JDK with the flag on, neither
    // registers and `java.util.Random`'s own bytecode runs.
    //
    // The old throughput objection is superseded by the 2026-10-03 measured
    // callee/boxing paths; see the retired JpaLargeBlobTest document. The JDK
    // bytecode default is checked against HotSpot in both runtime modes.
    if registry.real_jdk() && jdk_random_enabled() {
        return;
    }
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/util/Random";
    registry.register(c, "<init>", "()V", native_random_init);
    registry.register(c, "<init>", "(J)V", native_random_init_seed);
    registry.register(c, "nextInt", "()I", native_random_next_int);
    registry.register(c, "nextInt", "(I)I", native_random_next_int_bound);
    registry.register(c, "nextLong", "()J", native_random_next_long);
    registry.register(c, "nextDouble", "()D", native_random_next_double);
    registry.register(c, "nextFloat", "()F", native_random_next_float);
    registry.register(c, "nextBoolean", "()Z", native_random_next_boolean);
    registry.register(c, "setSeed", "(J)V", native_random_set_seed);
    registry.register(c, "nextGaussian", "()D", native_random_next_gaussian);
    registry.set_category(__prev_cat);
}

/// Java LCG constants
pub(crate) const LCG_MULTIPLIER: i64 = 0x5DEECE66D;
pub(crate) const LCG_INCREMENT: i64 = 0xB;
pub(crate) const LCG_MASK: i64 = (1i64 << 48) - 1;

pub(crate) fn rnd_scramble_seed(seed: i64) -> i64 {
    (seed ^ LCG_MULTIPLIER) & LCG_MASK
}

pub(crate) fn rnd_next(ctx: &mut dyn NativeContext, this: ObjectRef, bits: u32) -> i32 {
    let old_seed = match ctx.get_field(this, RND_FIELD_SEED) {
        Value::Long(s) => s,
        _ => 0,
    };
    let new_seed = (old_seed
        .wrapping_mul(LCG_MULTIPLIER)
        .wrapping_add(LCG_INCREMENT))
        & LCG_MASK;
    ctx.set_field(this, RND_FIELD_SEED, Value::Long(new_seed));
    (new_seed >> (48 - bits)) as i32
}

pub(crate) fn native_random_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(42);
    ctx.set_field(this, RND_FIELD_SEED, Value::Long(rnd_scramble_seed(seed)));
    Ok(None)
}

pub(crate) fn native_random_init_seed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let seed = match args.get(1) {
        Some(Value::Long(s)) => *s,
        _ => 0,
    };
    ctx.set_field(this, RND_FIELD_SEED, Value::Long(rnd_scramble_seed(seed)));
    Ok(None)
}

pub(crate) fn native_random_next_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(rnd_next(ctx, this, 32))))
}

pub(crate) fn native_random_next_int_bound(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let bound = match args.get(1) {
        Some(Value::Int(b)) => *b,
        _ => 1,
    };
    if bound <= 0 {
        // CRATONVM_DBG_NEXTINT=1 — dump the Java caller chain. See the twin
        // diagnostic in native-builtins securerandom.rs for rationale (locating
        // the empty-collection divergence in ES/Lucene test-framework setup).
        if cratonvm_types::flags::runtime_var("CRATONVM_DBG_NEXTINT").is_ok() {
            eprintln!("NEXTINT-BAD(coll) bound={bound} caller-chain (inner→outer):");
            let frames = ctx.capture_stack_trace();
            for f in frames.iter().rev().take(20) {
                eprintln!(
                    "  NEXTINT-STK {}.{} line={} bci={}",
                    f.class_name, f.method_name, f.line_number, f.byte_code_index
                );
            }
        }
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "bound must be positive".to_string(),
            }
            .into(),
        );
    }
    // Rejection sampling for uniform distribution
    let mut r = rnd_next(ctx, this, 31);
    let m = bound - 1;
    if (bound & m) == 0 {
        // power of two
        r = ((bound as i64 * r as i64) >> 31) as i32;
    } else {
        let mut u = r;
        loop {
            r = u % bound;
            if u.wrapping_sub(r).wrapping_add(m) >= 0 {
                break;
            }
            u = rnd_next(ctx, this, 31);
        }
    }
    Ok(Some(Value::Int(r)))
}

pub(crate) fn native_random_next_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Long(0))),
    };
    let hi = rnd_next(ctx, this, 32) as i64;
    let lo = rnd_next(ctx, this, 32) as i64;
    Ok(Some(Value::Long((hi << 32) + lo)))
}

pub(crate) fn native_random_next_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    let hi = (rnd_next(ctx, this, 26) as i64) << 27;
    let lo = rnd_next(ctx, this, 27) as i64;
    let v = (hi + lo) as f64 / ((1i64 << 53) as f64);
    Ok(Some(Value::Double(v)))
}

pub(crate) fn native_random_next_float(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Float(0.0))),
    };
    let v = rnd_next(ctx, this, 24) as f32 / ((1i32 << 24) as f32);
    Ok(Some(Value::Float(v)))
}

pub(crate) fn native_random_next_boolean(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let v = rnd_next(ctx, this, 1);
    Ok(Some(Value::Int(v)))
}

pub(crate) fn native_random_set_seed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let seed = match args.get(1) {
        Some(Value::Long(s)) => *s,
        _ => 0,
    };
    ctx.set_field(this, RND_FIELD_SEED, Value::Long(rnd_scramble_seed(seed)));
    // `java.util.Random.setSeed` also does `haveNextNextGaussian = false`, so a
    // re-seeded generator must not hand out a variate drawn from the old seed.
    ctx.set_field(this, RND_FIELD_NEXT_GAUSSIAN, Value::Object(None));
    Ok(None)
}

/// One `nextDouble()` draw: `(next(26) << 27 + next(27)) * 2^-53`.
pub(crate) fn rnd_uniform_double(next: &mut impl FnMut(u32) -> i32) -> f64 {
    let hi = (next(26) as i64) << 27;
    let lo = next(27) as i64;
    (hi + lo) as f64 / ((1i64 << 53) as f64)
}

/// The polar (Marsaglia) method exactly as `java.util.Random.nextGaussian()`
/// specifies it, returning BOTH variates of the accepted pair.
///
/// Factored out of the native so the specified sequence is testable without a
/// VM — `next` is `Random.next(bits)`.
///
/// The multiplier is `StrictMath.sqrt(-2 * StrictMath.log(s) / s)` in the JDK,
/// and the `StrictMath` there is load-bearing: `sqrt`, `*` and `/` are
/// exactly-rounded IEEE 754 and agree everywhere, but `log` is a bit-for-bit
/// fdlibm contract that platform libm does not meet. Using `s.ln()` here put
/// every SEEDED gaussian stream one ULP off HotSpot's —
/// `new Random(42).nextGaussian()` printed `1.141905315473055` against
/// HotSpot's `1.1419053154730547`, which is the same double as
/// `Math.log`-derived, not a `Double.toString` difference. See
/// W7-44-numberformat-enum-and-double-tostring.md.
pub(crate) fn rnd_gaussian_pair(mut next: impl FnMut(u32) -> i32) -> (f64, f64) {
    loop {
        let v1 = 2.0 * rnd_uniform_double(&mut next) - 1.0;
        let v2 = 2.0 * rnd_uniform_double(&mut next) - 1.0;
        let s = v1 * v1 + v2 * v2;
        if s < 1.0 && s != 0.0 {
            let multiplier = (-2.0 * cratonvm_types::fdlibm::log(s) / s).sqrt();
            return (v1 * multiplier, v2 * multiplier);
        }
    }
}

/// `java.util.Random.nextGaussian()`.
///
/// The polar method yields TWO standard normals per accepted pair; the JDK
/// returns the first and caches the second for the following call. This used to
/// discard the second and draw a fresh pair every time, which was wrong twice
/// over: the LCG then advanced at twice the specified rate, so every SEEDED
/// sequence diverged from a conforming VM, and consecutive results came out
/// i.i.d. instead of sharing a `multiplier` the way the spec's pairs do.
///
/// That correlation is observable, not academic: H2's `MemoryEstimator` sizes a
/// skip counter from how close consecutive values are, so `TestMemoryEstimator`
/// measured a sampling percentage of 8 against its own `<= 7` bound. It was
/// recorded in `vm/src/jit/skip_list.rs` as the sole blocker on lifting the
/// `org/h2/` JIT ban and read as a JIT miscompile; it is neither JIT-related
/// nor H2-specific.
pub(crate) fn native_random_next_gaussian(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    // A cached second variate is consumed before any new draw, so the stream
    // advances once per PAIR of calls.
    if let Value::Double(cached) = ctx.get_field(this, RND_FIELD_NEXT_GAUSSIAN) {
        ctx.set_field(this, RND_FIELD_NEXT_GAUSSIAN, Value::Object(None));
        return Ok(Some(Value::Double(cached)));
    }
    let (first, second) = rnd_gaussian_pair(|bits| rnd_next(ctx, this, bits));
    ctx.set_field(this, RND_FIELD_NEXT_GAUSSIAN, Value::Double(second));
    Ok(Some(Value::Double(first)))
}

// ===========================================================================
// Phase 14 Step 4: OptionalInt / OptionalLong / OptionalDouble
// ===========================================================================

pub(crate) fn register_optional_int_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/util/OptionalInt";
    registry.register(c, "of", "(I)Ljava/util/OptionalInt;", native_opt_int_of);
    registry.register(
        c,
        "empty",
        "()Ljava/util/OptionalInt;",
        native_opt_int_empty,
    );
    registry.register(c, "getAsInt", "()I", native_opt_int_get);
    registry.register(c, "isPresent", "()Z", native_opt_int_is_present);
    registry.register(c, "orElse", "(I)I", native_opt_int_or_else);
    registry.register(
        c,
        "ifPresent",
        "(Ljava/util/function/IntConsumer;)V",
        native_opt_int_if_present,
    );
    registry.set_category(__prev_cat);
}

pub(crate) fn register_optional_long_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/util/OptionalLong";
    registry.register(c, "of", "(J)Ljava/util/OptionalLong;", native_opt_long_of);
    registry.register(
        c,
        "empty",
        "()Ljava/util/OptionalLong;",
        native_opt_long_empty,
    );
    registry.register(c, "getAsLong", "()J", native_opt_long_get);
    registry.register(c, "isPresent", "()Z", native_opt_long_is_present);
    registry.register(c, "orElse", "(J)J", native_opt_long_or_else);
    registry.register(
        c,
        "ifPresent",
        "(Ljava/util/function/LongConsumer;)V",
        native_opt_long_if_present,
    );
    registry.set_category(__prev_cat);
}

pub(crate) fn register_optional_double_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/util/OptionalDouble";
    registry.register(
        c,
        "of",
        "(D)Ljava/util/OptionalDouble;",
        native_opt_double_of,
    );
    registry.register(
        c,
        "empty",
        "()Ljava/util/OptionalDouble;",
        native_opt_double_empty,
    );
    registry.register(c, "getAsDouble", "()D", native_opt_double_get);
    registry.register(c, "isPresent", "()Z", native_opt_double_is_present);
    registry.register(c, "orElse", "(D)D", native_opt_double_or_else);
    registry.register(
        c,
        "ifPresent",
        "(Ljava/util/function/DoubleConsumer;)V",
        native_opt_double_if_present,
    );
    registry.set_category(__prev_cat);
}

// --- OptionalInt ---

pub(crate) fn native_opt_int_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let opt = make_opt_prim(ctx, "java/util/OptionalInt", Some(Value::Int(val)))?;
    Ok(Some(Value::Object(Some(opt))))
}

pub(crate) fn native_opt_int_empty(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let opt = make_opt_prim(ctx, "java/util/OptionalInt", None)?;
    Ok(Some(Value::Object(Some(opt))))
}

pub(crate) fn native_opt_int_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "No value present".to_string(),
                }
                .into(),
            )
        }
    };
    match opt_prim_value(ctx, this) {
        Some(Value::Int(v)) => Ok(Some(Value::Int(v))),
        Some(other) => Ok(Some(other)),
        None => Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "No value present".to_string(),
            }
            .into(),
        ),
    }
}

pub(crate) fn native_opt_int_is_present(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(if opt_prim_value(ctx, this).is_some() {
        1
    } else {
        0
    })))
}

pub(crate) fn native_opt_int_or_else(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let default_val = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(default_val))),
    };
    match opt_prim_value(ctx, this) {
        Some(Value::Int(v)) => Ok(Some(Value::Int(v))),
        _ => Ok(Some(Value::Int(default_val))),
    }
}

pub(crate) fn native_opt_int_if_present(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    if let Some(Value::Int(v)) = opt_prim_value(ctx, this) {
        ctx.invoke_virtual(consumer, "accept", "(I)V", &[Value::Int(v)])?;
    }
    Ok(None)
}

// --- OptionalLong ---

pub(crate) fn native_opt_long_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let opt = make_opt_prim(ctx, "java/util/OptionalLong", Some(Value::Long(val)))?;
    Ok(Some(Value::Object(Some(opt))))
}

pub(crate) fn native_opt_long_empty(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let opt = make_opt_prim(ctx, "java/util/OptionalLong", None)?;
    Ok(Some(Value::Object(Some(opt))))
}

pub(crate) fn native_opt_long_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "No value present".to_string(),
                }
                .into(),
            )
        }
    };
    match opt_prim_value(ctx, this) {
        Some(Value::Long(v)) => Ok(Some(Value::Long(v))),
        Some(other) => Ok(Some(other)),
        None => Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "No value present".to_string(),
            }
            .into(),
        ),
    }
}

pub(crate) fn native_opt_long_is_present(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(if opt_prim_value(ctx, this).is_some() {
        1
    } else {
        0
    })))
}

pub(crate) fn native_opt_long_or_else(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let default_val = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Long(default_val))),
    };
    match opt_prim_value(ctx, this) {
        Some(Value::Long(v)) => Ok(Some(Value::Long(v))),
        _ => Ok(Some(Value::Long(default_val))),
    }
}

pub(crate) fn native_opt_long_if_present(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    if let Some(Value::Long(v)) = opt_prim_value(ctx, this) {
        ctx.invoke_virtual(consumer, "accept", "(J)V", &[Value::Long(v)])?;
    }
    Ok(None)
}

// --- OptionalDouble ---

pub(crate) fn native_opt_double_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let opt = make_opt_prim(ctx, "java/util/OptionalDouble", Some(Value::Double(val)))?;
    Ok(Some(Value::Object(Some(opt))))
}

pub(crate) fn native_opt_double_empty(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let opt = make_opt_prim(ctx, "java/util/OptionalDouble", None)?;
    Ok(Some(Value::Object(Some(opt))))
}

pub(crate) fn native_opt_double_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "No value present".to_string(),
                }
                .into(),
            )
        }
    };
    match opt_prim_value(ctx, this) {
        Some(Value::Double(v)) => Ok(Some(Value::Double(v))),
        Some(other) => Ok(Some(other)),
        None => Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "No value present".to_string(),
            }
            .into(),
        ),
    }
}

pub(crate) fn native_opt_double_is_present(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(if opt_prim_value(ctx, this).is_some() {
        1
    } else {
        0
    })))
}

pub(crate) fn native_opt_double_or_else(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let default_val = match args.get(1) {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Double(default_val))),
    };
    match opt_prim_value(ctx, this) {
        Some(Value::Double(v)) => Ok(Some(Value::Double(v))),
        _ => Ok(Some(Value::Double(default_val))),
    }
}

pub(crate) fn native_opt_double_if_present(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    if let Some(Value::Double(v)) = opt_prim_value(ctx, this) {
        ctx.invoke_virtual(consumer, "accept", "(D)V", &[Value::Double(v)])?;
    }
    Ok(None)
}

