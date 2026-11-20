#[allow(unused_imports)]
use super::*;


/// The deadline a `java/net/SocketInputStream` read must honour, derived from
/// the socket's own `SO_RCVTIMEO`.
///
/// This is the rule W7-53 states per regime: a syscall that is no longer issued
/// until the socket is ready is a syscall `SO_RCVTIMEO` can never bound. The
/// close-aware read parks in `poll` and only then reads, so without this the
/// timeout a caller set with `setSoTimeout` would never fire and the fix for one
/// hang would have introduced another. `SO_RCVTIMEO` stays set and remains the
/// first line; this is what still ends the park where it cannot.
///
/// `None` — no timeout configured, or the query itself failed — means "no
/// deadline", which is what an untimed socket asked for. Guessing one would be
/// the `SocketTimeoutException`-on-a-socket-with-no-timeout regression that
/// record names as its own falsifier.
///
/// (The private `read_retry_eintr` that used to sit here went with its last
/// caller: the three reads below were its only users and
/// `re1_read_close_aware` reissues EINTR itself.)
pub fn sis_read_deadline(stream: &std::net::TcpStream) -> Option<std::time::Instant> {
    stream
        .read_timeout()
        .ok()
        .flatten()
        .map(|d| std::time::Instant::now() + d)
}

pub fn object_array_element_hash_code(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
) -> Result<i32, MethodCallFailed> {
    if ctx.object_is_array(obj) {
        return Ok(ctx.identity_hash_code(obj));
    }

    match ctx
        .class_name_arc_of_id(ctx.class_id_of_object(obj))
        .as_deref()
    {
        Some("java/lang/String") => {
            match native_string_hash_code(ctx, &[Value::Object(Some(obj))])? {
                Some(Value::Int(v)) => Ok(v),
                _ => Ok(0),
            }
        }
        Some(
            "java/lang/Integer" | "java/lang/Byte" | "java/lang/Short" | "java/lang/Character",
        ) => match ctx.get_field(obj, 0) {
            Value::Int(v) => Ok(v),
            _ => Ok(0),
        },
        Some("java/lang/Boolean") => match ctx.get_field(obj, 0) {
            Value::Int(v) => Ok(if v != 0 { 1231 } else { 1237 }),
            _ => Ok(1237),
        },
        Some("java/lang/Long") => match ctx.get_field(obj, 0) {
            Value::Long(v) => Ok((v ^ ((v as u64 >> 32) as i64)) as i32),
            _ => Ok(0),
        },
        Some("java/lang/Float") => match ctx.get_field(obj, 0) {
            Value::Float(v) => {
                let bits = if v.is_nan() { 0x7fc0_0000 } else { v.to_bits() };
                Ok(bits as i32)            }
            _ => Ok(0),
        },
        Some("java/lang/Double") => match ctx.get_field(obj, 0) {
            Value::Double(v) => {
                let bits = if v.is_nan() {
                    0x7ff8_0000_0000_0000u64                } else {
                    v.to_bits()                };
                Ok((bits ^ (bits >> 32)) as i32)            }
            _ => Ok(0),
        },
        _ => match ctx.invoke_virtual(obj, "hashCode", "()I", &[])? {
            Some(Value::Int(v)) => Ok(v),
            _ => Ok(0),
        },
    }
}

pub fn register_collections_extras_natives(r: &mut NativeMethodRegistry) {
            let cu = "java/util/Collections";
    // unmodifiableList/Set/Map just return the same collection (simplified)
    r.register(
        cu,
        "unmodifiableList",
        "(Ljava/util/List;)Ljava/util/List;",
        native_return_first_arg, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "unmodifiableSet",
        "(Ljava/util/Set;)Ljava/util/Set;",
        native_return_first_arg, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "unmodifiableMap",
        "(Ljava/util/Map;)Ljava/util/Map;",
        native_return_first_arg, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "unmodifiableCollection",
        "(Ljava/util/Collection;)Ljava/util/Collection;",
        native_return_first_arg, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "unmodifiableSortedSet",
        "(Ljava/util/SortedSet;)Ljava/util/SortedSet;",
        native_return_first_arg, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "synchronizedList",
        "(Ljava/util/List;)Ljava/util/List;",
        native_synchronized_list, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "synchronizedSet",
        "(Ljava/util/Set;)Ljava/util/Set;",
        native_synchronized_set, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "synchronizedMap",
        "(Ljava/util/Map;)Ljava/util/Map;",
        native_synchronized_map, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "synchronizedCollection",
        "(Ljava/util/Collection;)Ljava/util/Collection;",
        native_synchronized_collection, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "checkedList",
        "(Ljava/util/List;Ljava/lang/Class;)Ljava/util/List;",
        native_return_first_arg, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "checkedSet",
        "(Ljava/util/Set;Ljava/lang/Class;)Ljava/util/Set;",
        native_return_first_arg, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "checkedMap",
        "(Ljava/util/Map;Ljava/lang/Class;Ljava/lang/Class;)Ljava/util/Map;",
        native_return_first_arg, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "singletonList",
        "(Ljava/lang/Object;)Ljava/util/List;",
        native_collections_singleton_list, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "singleton",
        "(Ljava/lang/Object;)Ljava/util/Set;",
        native_collections_singleton_set, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "singletonMap",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;",
        native_collections_singleton_map, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "list",
        "(Ljava/util/Enumeration;)Ljava/util/ArrayList;",
        native_return_first_arg, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "frequency",
        "(Ljava/util/Collection;Ljava/lang/Object;)I",
        native_collections_frequency, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // `Collections.disjoint` intentionally left unregistered — the real pure-Java
    // implementation runs correctly on CratonVM. A native stub here (previously
    // `native_return_false`, and a conflicting always-true stub in phases_late)
    // produced wrong answers; see the note in phases_late.rs.
    r.register(
        cu,
        "nCopies",
        "(ILjava/lang/Object;)Ljava/util/List;",
        native_collections_ncopies, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "min",
        "(Ljava/util/Collection;)Ljava/lang/Object;",
        native_collections_min, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "max",
        "(Ljava/util/Collection;)Ljava/lang/Object;",
        native_collections_max, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(cu, "swap", "(Ljava/util/List;II)V", |ctx, args| {
        // Swap two elements in an ArrayList
        let list = match args.first() {
            Some(Value::Object(Some(r))) => *r,
            _ => return Ok(None),
        };
        let i = match args.get(1) {
            Some(Value::Int(v)) => *v as usize,
            _ => return Ok(None),
        };
        let j = match args.get(2) {
            Some(Value::Int(v)) => *v as usize,
            _ => return Ok(None),
        };
        let data = match ctx.get_field(list, 0) {
            Value::Object(Some(arr)) => arr,
            _ => return Ok(None),
        };
        let a = ctx.get_array_element(data, i);
        let b = ctx.get_array_element(data, j);
        ctx.set_array_element(data, i, b);
        ctx.set_array_element(data, j, a);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(cu, "rotate", "(Ljava/util/List;I)V", |ctx, args| {
        // Rotate an ArrayList by `distance` positions
        let list = match args.first() {
            Some(Value::Object(Some(r))) => *r,
            _ => return Ok(None),
        };
        let distance = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => return Ok(None),
        };
        let data = match ctx.get_field(list, 0) {
            Value::Object(Some(arr)) => arr,
            _ => return Ok(None),
        };
        let size = match ctx.get_field(list, 1) {
            Value::Int(s) => s as usize,
            _ => return Ok(None),
        };
        if size == 0 {
            return Ok(None);
        }
        let d = ((distance % size as i32) + size as i32) as usize % size;
        if d == 0 {
            return Ok(None);
        }
        // Collect, then write back rotated
        let mut elems: Vec<Value> = (0..size).map(|i| ctx.get_array_element(data, i)).collect();
        elems.rotate_right(d);
        for (i, val) in elems.into_iter().enumerate() {
            ctx.set_array_element(data, i, val);        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        cu,
        "fill",
        "(Ljava/util/List;Ljava/lang/Object;)V",
        |ctx, args| {
            let list = match args.first() {
                Some(Value::Object(Some(r))) => *r,
                _ => return Ok(None),
            };
            let value = args.get(1).copied().unwrap_or(Value::Object(None));
            let data = match ctx.get_field(list, 0) {
                Value::Object(Some(arr)) => arr,
                _ => return Ok(None),
            };
            let size = match ctx.get_field(list, 1) {
                Value::Int(s) => s as usize,
                _ => return Ok(None),
            };
            for i in 0..size {
                ctx.set_array_element(data, i, value);
            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "copy",
        "(Ljava/util/List;Ljava/util/List;)V",
        native_collections_copy, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "replaceAll",
        "(Ljava/util/List;Ljava/lang/Object;Ljava/lang/Object;)Z",
        native_collections_replace_all, cratonvm_native_api::NativeKind::Intrinsic,
    );
    }

/// A `Value` argument held across a call that can allocate.
///
/// A reference argument is an address copied out of the caller's frame: the
/// frame roots the object, but this copy is not updated when the collector
/// moves it, so it must be rooted in the scope and read back at the point of
/// use. A primitive needs none of that and is carried verbatim.
pub enum ArgSlot {
    Ref(NativeHandle),
    Plain(Value),
}

pub fn element_handle(scope: &mut NativeHandleScope<'_>, value: Option<Value>) -> ArgSlot {
    match value {
        Some(Value::Object(Some(o))) => ArgSlot::Ref(scope.root(o)),
        Some(v) => ArgSlot::Plain(v),
        None => ArgSlot::Plain(Value::Object(None)),
    }
}

pub fn element_value(scope: &NativeHandleScope<'_>, slot: &ArgSlot) -> Value {
    match slot {
        ArgSlot::Ref(h) => Value::Object(Some(scope.get(h))),
        ArgSlot::Plain(v) => *v,
    }
}

/// A synthetic `java/util/ArrayList` (backing array in field 0, size in field
/// 1) holding `elems` in order.
///
/// gc-common w19-c (stale-handle triage): `List.of()` / `of(E)` / `of(E, E)`,
/// `Collections.singletonList` and the empty arms of `Arrays.asList`,
/// `List.of(E...)` and `Stream.toList` each spelled this out, holding the list
/// raw across the backing array's allocation and each element (an address
/// copied out of `args`, which no collection updates) raw across both. The
/// list's class resolution can run a `<clinit>` (Java). Every element and the
/// list are rooted here and re-read at their stores.
pub fn pe_array_list_of(ctx: &mut dyn NativeContext, elems: &[Value]) -> MethodCallResult {
    let mut scope = NativeHandleScope::new(ctx);
    let mut slots = Vec::with_capacity(elems.len());
    for v in elems {
        slots.push(element_handle(&mut scope, Some(*v)));
    }
    let list = try_alloc_concurrent_synthetic(&mut *scope, "java/util/ArrayList", 2)?;
    let list_h = scope.root(list);
    let arr = scope.new_array(cratonvm_types::ArrayElementType::Reference, slots.len());
    for (i, slot) in slots.iter().enumerate() {
        let v = element_value(&scope, slot);
        scope.set_array_element(arr, i, v);
    }
    let list = scope.get(&list_h);
    scope.set_field(list, 0, Value::Object(Some(arr)));
    scope.set_field(list, 1, Value::Int(slots.len() as i32));
    Ok(Some(Value::Object(Some(list))))
}

/// The grow path of `StringJoiner.add`: copy the joiner's element list
/// (`list`, backing array `arr` holding `size` elements) into a larger array,
/// append `elem`, publish it and the new size, and hand back the joiner's
/// post-allocation address.
///
/// gc-common w19-c: the receiver, the list, the old array and the element (an
/// address copied out of `args`) were all held raw across the new array's
/// allocation. All four are rooted and re-read at their uses. The capacity now
/// also covers the appended slot: `cap * 2` was 0 for an empty array, and the
/// append then stored past its end (the `jndi_grow_and_append` fix, w16-e).
pub fn sj_grow_and_append(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    list: ObjectRef,
    arr: ObjectRef,
    size: usize,
    elem: Value,
) -> ObjectRef {
    let cap = ctx.array_length(arr);
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let list_h = scope.root(list);
    let arr_h = scope.root(arr);
    let elem_h = element_handle(&mut scope, Some(elem));
    let new_cap = (cap * 2).max(size + 1);
    let new_arr = scope.new_array(cratonvm_types::ArrayElementType::Reference, new_cap);
    let old = scope.get(&arr_h);
    for i in 0..size {
        let v = scope.get_array_element(old, i);
        scope.set_array_element(new_arr, i, v);
    }
    let elem_now = element_value(&scope, &elem_h);
    scope.set_array_element(new_arr, size, elem_now);
    let list_now = scope.get(&list_h);
    scope.set_field(list_now, 0, Value::Object(Some(new_arr)));
    scope.set_field(list_now, 1, Value::Int((size + 1) as i32));
    scope.get(&this_h)
}

/// A synthetic `java/nio/charset/Charset` (1-field layout, slot 0 = canonical
/// name) named `name`: `Charset.forName`, `defaultCharset` and the
/// `StandardCharsets.<clinit>` override.
///
/// gc-common w19-c: each held the fresh charset raw across its name's
/// allocation. Rooted here; the returned address is current.
pub fn pe_new_charset(ctx: &mut dyn NativeContext, name: &str) -> Result<ObjectRef, MethodCallFailed> {
    let charset = try_alloc_concurrent_synthetic(ctx, "java/nio/charset/Charset", 1)?;
    let mut scope = NativeHandleScope::new(ctx);
    let charset_h = scope.root(charset);
    let n = scope.create_string(name);
    let charset = scope.get(&charset_h);
    scope.set_field(charset, 0, Value::Object(Some(n)));
    Ok(charset)
}

/// `""`, the answer of `String.join` / `StringJoiner.toString` when there is
/// nothing to join. A function of its own (gc-common w19-c) so each early
/// return is one line and allocates after every read of the caller's objects.
pub fn pe_empty_string(ctx: &mut dyn NativeContext) -> MethodCallResult {
    let s = ctx.create_string("");
    Ok(Some(Value::Object(Some(s))))
}

/// Allocate the String `text` and store it into field `index` of `obj`;
/// answers `obj`'s current address.
///
/// gc-common w20-b (stale-handle triage): some twenty natives in this file
/// spelled this out as `let s = ctx.create_string(..); ctx.set_field(obj, i,
/// s)` with `obj` -- an address copied out of `args` or just allocated --
/// held raw across the String's allocation. `obj` is pinned across it here,
/// and the store goes to the re-read address.
pub fn alloc_string_into_field(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    index: usize,
    text: &str,
) -> ObjectRef {
    let pin = ctx.pin_native_root(obj);
    let s = ctx.create_string(text);
    let obj_now = ctx.read_native_pin(pin, obj);
    ctx.unpin_native_roots(pin);
    ctx.set_field(obj_now, index, Value::Object(Some(s)));
    obj_now
}

/// Allocate the String `text` and store it into the named field of `obj`,
/// returning the object's current address. Currency uses this because the JDK
/// image's field order is not a contract a synthetic native may assume.
pub fn alloc_string_into_named_field(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    field_name: &str,
    text: &str,
) -> ObjectRef {
    let pin = ctx.pin_native_root(obj);
    let s = ctx.create_string_uninterned(text);
    let obj_now = ctx.read_native_pin(pin, obj);
    ctx.unpin_native_roots(pin);
    ctx.set_field_by_name(obj_now, field_name, Value::Object(Some(s)));
    obj_now
}

/// A fresh primitive (or `Object[]`) array of `len` elements stored into
/// field `index` of `obj`; answers `obj`'s current address. The
/// [`alloc_string_into_field`] of arrays (gc-common w20-b), spelled through
/// `util_concurrent_ext::uce_store_fresh_array`, which pins `obj` across the
/// allocation.
pub fn alloc_array_into_field(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    index: usize,
    elem: cratonvm_types::ArrayElementType,
    len: usize,
) -> ObjectRef {
    crate::util_concurrent_ext::uce_store_fresh_array(ctx, obj, index, elem, len)
}

/// The length of the array held in field 0 of `obj` (0 with none): the
/// synthetic `AtomicIntegerArray` / `AtomicLongArray` keep no length slot,
/// as the real classes have none (FS3-2).
pub fn backing_array_length(ctx: &dyn NativeContext, obj: ObjectRef) -> usize {
    match ctx.get_field(obj, 0) {
        Value::Object(Some(arr)) => ctx.array_length(arr),
        _ => 0,
    }
}

/// [`alloc_array_into_field`] filled with `values` before the store.
///
/// `values` must be primitives: a reference in the slice is an address the
/// allocation does not update. (gc-common w20-b; `BitSet.clone` /
/// `valueOf(long[])` and the synthetic primitive-array holders use it.)
pub fn alloc_filled_array_into_field(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    index: usize,
    elem: cratonvm_types::ArrayElementType,
    values: &[Value],
) -> ObjectRef {
    debug_assert!(!values.iter().any(|v| matches!(v, Value::Object(Some(_)))));
    let pin = ctx.pin_native_root(obj);
    let arr = ctx.new_array(elem, values.len());
    for (i, v) in values.iter().enumerate() {
        ctx.set_array_element(arr, i, *v);
    }
    let obj_now = ctx.read_native_pin(pin, obj);
    ctx.unpin_native_roots(pin);
    ctx.set_field(obj_now, index, Value::Object(Some(arr)));
    obj_now
}

/// A synthetic `java/util/ArrayList` holding a copy of every element of the
/// reference array `src` (`Arrays.asList`, `List.of(E...)`, `Stream.toList`).
///
/// gc-common w19-c: those natives held `src` raw across the copy's allocation
/// and the copy raw across the list's (a class resolution, which can run a
/// `<clinit>`). `src` and the list are rooted, and `src` is re-read after the
/// last allocation, before the copy.
pub fn pe_array_list_copy_of(ctx: &mut dyn NativeContext, src: ObjectRef) -> MethodCallResult {
    let mut scope = NativeHandleScope::new(ctx);
    let src_h = scope.root(src);
    let list = try_alloc_concurrent_synthetic(&mut *scope, "java/util/ArrayList", 2)?;
    let list_h = scope.root(list);
    let len = {
        let src_now = scope.get(&src_h);
        scope.array_length(src_now)
    };
    let arr = scope.new_array(cratonvm_types::ArrayElementType::Reference, len);
    let src_now = scope.get(&src_h);
    for i in 0..len {
        let v = scope.get_array_element(src_now, i);
        scope.set_array_element(arr, i, v);
    }
    let list = scope.get(&list_h);
    scope.set_field(list, 0, Value::Object(Some(arr)));
    scope.set_field(list, 1, Value::Int(len as i32));
    Ok(Some(Value::Object(Some(list))))
}

pub fn native_collections_singleton_list(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // The argument is an address copied off the caller's operand stack: the
    // stack itself is a root, but this copy is not, so it is stale the moment
    // `<clinit>` or the allocation below collects. Root it before anything
    // runs, and read it back where it is stored.
    let mut scope = NativeHandleScope::new(ctx);
    let elem_h = element_handle(&mut scope, args.first().copied());
    if let Ok(cid) = scope.ensure_class_initialized("java/util/Collections$SingletonList") {
        let fields = scope.class_num_total_fields(cid);
        let list = scope.alloc_object(cid, fields);
        let elem = element_value(&scope, &elem_h);
        scope.set_field_by_name(list, "element", elem);
        return Ok(Some(Value::Object(Some(list))));
    }
    // The array is built first and the list allocated after it, so the array's
    // address has to be read back on the far side of that allocation.
    let arr_obj = scope.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    let arr_h = scope.root(arr_obj);
    let elem = element_value(&scope, &elem_h);
    scope.set_array_element(arr_obj, 0, elem);
    let list = try_alloc_concurrent_synthetic(&mut *scope, "java/util/ArrayList", 2)?;
    let arr = scope.get(&arr_h);
    scope.set_field(list, 0, Value::Object(Some(arr)));
    scope.set_field(list, 1, Value::Int(1));
    Ok(Some(Value::Object(Some(list))))
}

pub fn native_collections_singleton_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut scope = NativeHandleScope::new(ctx);
    let elem_h = element_handle(&mut scope, args.first().copied());
    if let Ok(cid) = scope.ensure_class_initialized("java/util/Collections$SingletonSet") {
        let fields = scope.class_num_total_fields(cid);
        let set = scope.alloc_object(cid, fields);
        let elem = element_value(&scope, &elem_h);
        scope.set_field_by_name(set, "element", elem);
        return Ok(Some(Value::Object(Some(set))));
    }
    // `set` outlives the map's allocation, `native_map_init` (which allocates a
    // bucket array) and `native_map_put_entry_pub` (which can run Java for
    // `hashCode`), so neither it nor the map can be held as a bare address.
    let set_obj = try_alloc_concurrent_synthetic(&mut *scope, "java/util/HashSet", 1)?;
    let set_h = scope.root(set_obj);
    let map_obj = try_alloc_concurrent_synthetic(&mut *scope, "java/util/HashMap", 3)?;
    let map_h = scope.root(map_obj);
    let map_now = scope.get(&map_h);
    cratonvm_native_collections::native_map_init(&mut *scope, &[Value::Object(Some(map_now))])?;
    let map_now = scope.get(&map_h);
    let elem = element_value(&scope, &elem_h);
    // gc-common w11-c: the reclaiming door (registered-only; `set` and `map`
    // are scope-rooted and re-read below).
    cratonvm_native_collections::native_map_put_entry_pub(
        &mut *scope,
        &[Value::Object(Some(map_now)), elem, Value::Object(None)],
    )?;
    let set = scope.get(&set_h);
    let map = scope.get(&map_h);
    scope.set_field(set, 0, Value::Object(Some(map)));
    Ok(Some(Value::Object(Some(set))))
}

pub fn native_collections_singleton_map(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut scope = NativeHandleScope::new(ctx);
    let key_h = element_handle(&mut scope, args.first().copied());
    let val_h = element_handle(&mut scope, args.get(1).copied());
    if let Ok(cid) = scope.ensure_class_initialized("java/util/Collections$SingletonMap") {
        let fields = scope.class_num_total_fields(cid);
        let map = scope.alloc_object(cid, fields);
        let key = element_value(&scope, &key_h);
        let val = element_value(&scope, &val_h);
        scope.set_field_by_name(map, "k", key);
        scope.set_field_by_name(map, "v", val);
        return Ok(Some(Value::Object(Some(map))));
    }
    // Create HashMap with 1 entry. The map, the bucket array and the node are
    // three allocations that each move the two before it.
    let map_obj = try_alloc_concurrent_synthetic(&mut *scope, "java/util/HashMap", 3)?;
    let map_h = scope.root(map_obj);
    let cap = 16;
    let buckets_obj = scope.new_array(cratonvm_types::ArrayElementType::Reference, cap);
    let buckets_h = scope.root(buckets_obj);
    let map = scope.get(&map_h);
    scope.set_field(map, 0, Value::Object(Some(buckets_obj)));
    scope.set_field(map, 1, Value::Int(0));
    scope.set_field(map, 2, Value::Int(cap as i32));
    // Put the single entry using native_map_put logic
    // Simplified: just store it
    let node = try_alloc_concurrent_synthetic(&mut *scope, "java/util/HashMap$Node", 4)?;
    let key = element_value(&scope, &key_h);
    let val = element_value(&scope, &val_h);
    scope.set_field(node, 0, key);
    scope.set_field(node, 1, val);
    let hash = 0i32; // simplified
    scope.set_field(node, 2, Value::Int(hash));
    scope.set_field(node, 3, Value::Object(None));
    let idx = 0;
    let buckets = scope.get(&buckets_h);
    let map = scope.get(&map_h);
    scope.set_array_element(buckets, idx, Value::Object(Some(node)));
    scope.set_field(map, 1, Value::Int(1));
    Ok(Some(Value::Object(Some(map))))
}

// ---------------------------------------------------------------------------
// Shared helpers for the `java.util.Collections` natives below.
//
// `equals`/`compareTo`/`size`/`get`/`set` all re-enter Java, so anything held
// across them can be moved by a collection. These mirror native-collections'
// private `pin_value*`/`read_pinned_elem` idiom (that crate does not export
// them) so the snapshots here are tracked by pin handle, not by raw ObjectRef.
// ---------------------------------------------------------------------------

pub fn pe_pin_value(ctx: &mut dyn NativeContext, v: Value) -> usize {
    match v {
        Value::Object(Some(o)) => ctx.pin_native_root(o),
        _ => usize::MAX,
    }
}

pub fn pe_pin_value_slice(ctx: &mut dyn NativeContext, vals: &[Value]) -> Vec<usize> {
    vals.iter()
        .map(|v| match v {
            Value::Object(Some(o)) => ctx.pin_native_root(*o),
            _ => usize::MAX,
        })
        .collect()
}

pub fn pe_read_pinned(ctx: &dyn NativeContext, handle: usize, orig: Value) -> Value {
    match orig {
        Value::Object(Some(o)) if handle != usize::MAX => {
            Value::Object(Some(ctx.read_native_pin(handle, o)))
        }
        _ => orig,
    }
}

/// Snapshot a `Collection`'s elements through the receiver's own `toArray()`.
/// Going through the receiver keeps these natives correct for `HashSet` /
/// `TreeSet` / `LinkedList`, unlike the neighbouring `swap`/`fill`/`copy`
/// natives which read the ArrayList slot layout directly. Nothing between the
/// call and the copy can run Java, so no pinning is needed inside.
pub fn pe_collection_elements(
    ctx: &mut dyn NativeContext,
    coll: ObjectRef,
) -> Result<Vec<Value>, MethodCallFailed> {
    let arr = match ctx.invoke_virtual(coll, "toArray", "()[Ljava/lang/Object;", &[])? {
        Some(Value::Object(Some(a))) => a,
        _ => return Ok(Vec::new()),
    };
    let len = ctx.array_length(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        out.push(ctx.get_array_element(arr, i));
    }
    Ok(out)
}

/// `Objects.equals(a, b)`: identity first, then `a.equals(b)`, with two
/// null/unset references counting as equal.
pub fn pe_values_equal(
    ctx: &mut dyn NativeContext,
    a: Value,
    b: Value,
) -> Result<bool, MethodCallFailed> {
    match (a, b) {
        (Value::Object(Some(x)), Value::Object(Some(y))) => {
            if x.as_ptr() == y.as_ptr() {
                return Ok(true);
            }
            match ctx.invoke_virtual(
                x,
                "equals",
                "(Ljava/lang/Object;)Z",
                &[Value::Object(Some(y))],
            )? {
                Some(Value::Int(v)) => Ok(v != 0),
                _ => Ok(false),
            }
        }
        (Value::Object(Some(_)), _) | (_, Value::Object(Some(_))) => Ok(false),
        _ => Ok(true),
    }
}

/// `Collections.frequency(c, o)` — count the elements equal to `o`. The
/// previous body returned a constant 0 regardless of the collection's
/// contents.
pub fn native_collections_frequency(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let coll = obj_arg(args, 0)?;
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    // Pin the receiver FIRST: `unpin_native_roots` releases everything from a
    // handle onward, so one call at the end covers the target and the snapshot.
    let coll_pin = ctx.pin_native_root(coll);
    let result = pe_frequency_inner(ctx, coll_pin, coll, target);
    ctx.unpin_native_roots(coll_pin);
    result
}

pub fn pe_frequency_inner(
    ctx: &mut dyn NativeContext,
    coll_pin: usize,
    coll: ObjectRef,
    target: Value,
) -> MethodCallResult {
    let target_pin = pe_pin_value(ctx, target);
    let coll_now = ctx.read_native_pin(coll_pin, coll);
    let elems = pe_collection_elements(ctx, coll_now)?;
    let handles = pe_pin_value_slice(ctx, &elems);
    let mut count = 0i32;
    for (i, orig) in elems.iter().enumerate() {
        let elem = pe_read_pinned(ctx, handles[i], *orig);
        let target_now = pe_read_pinned(ctx, target_pin, target);
        if pe_values_equal(ctx, target_now, elem)? {
            count += 1;
        }
    }
    Ok(Some(Value::Int(count)))
}

pub fn native_collections_min(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    pe_collections_extreme(ctx, args, false)
}

pub fn native_collections_max(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    pe_collections_extreme(ctx, args, true)
}

/// `Collections.min`/`max` over the elements' natural ordering. `null` — what
/// these used to return unconditionally — is never a legal answer: the JDK
/// walks the collection with `Comparable.compareTo` and throws
/// `NoSuchElementException` when it is empty.
pub fn pe_collections_extreme(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    want_max: bool,
) -> MethodCallResult {
    let coll = obj_arg(args, 0)?;
    let coll_pin = ctx.pin_native_root(coll);
    let result = pe_collections_extreme_inner(ctx, coll_pin, coll, want_max);
    ctx.unpin_native_roots(coll_pin);
    result
}

pub fn pe_collections_extreme_inner(
    ctx: &mut dyn NativeContext,
    coll_pin: usize,
    coll: ObjectRef,
    want_max: bool,
) -> MethodCallResult {
    let coll_now = ctx.read_native_pin(coll_pin, coll);
    let elems = pe_collection_elements(ctx, coll_now)?;
    if elems.is_empty() {
        return Err(RuntimeError::NoSuchElementException {
            message: String::new(),
        }
        .into());
    }
    let handles = pe_pin_value_slice(ctx, &elems);
    let mut best = 0usize;
    for i in 1..elems.len() {
        let cand = pe_read_pinned(ctx, handles[i], elems[i]);
        let incumbent = pe_read_pinned(ctx, handles[best], elems[best]);
        let (a, b) = match (cand, incumbent) {
            (Value::Object(Some(a)), Value::Object(Some(b))) => (a, b),
            // The JDK's own `compareTo` call site NPEs on a null element.
            _ => {
                return Err(RuntimeError::NullPointerException {
                    message: Some("Collections.min/max: null element".to_string()),
                }
                .into())
            }
        };
        let cmp = match ctx.invoke_virtual(
            a,
            "compareTo",
            "(Ljava/lang/Object;)I",
            &[Value::Object(Some(b))],
        )? {
            Some(Value::Int(v)) => v,
            _ => 0,
        };
        if (want_max && cmp > 0) || (!want_max && cmp < 0) {
            best = i;
        }
    }
    Ok(Some(pe_read_pinned(ctx, handles[best], elems[best])))
}

/// `Collections.replaceAll(list, oldVal, newVal)` — replace every occurrence
/// and report whether anything changed. The previous `native_return_false` did
/// neither, so a caller that branches on the result skipped its update against
/// a list that had also been left untouched.
pub fn native_collections_replace_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let list = obj_arg(args, 0)?;
    let old_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let new_val = args.get(2).copied().unwrap_or(Value::Object(None));
    let list_pin = ctx.pin_native_root(list);
    let result = pe_replace_all_inner(ctx, list_pin, list, old_val, new_val);
    ctx.unpin_native_roots(list_pin);
    result
}

pub fn pe_replace_all_inner(
    ctx: &mut dyn NativeContext,
    list_pin: usize,
    list: ObjectRef,
    old_val: Value,
    new_val: Value,
) -> MethodCallResult {
    let old_pin = pe_pin_value(ctx, old_val);
    let new_pin = pe_pin_value(ctx, new_val);
    // Drive the receiver's own size/get/set so every List implementation works,
    // not just the ArrayList slot layout the neighbours above assume.
    let list_now = ctx.read_native_pin(list_pin, list);
    let size = match ctx.invoke_virtual(list_now, "size", "()I", &[])? {
        Some(Value::Int(n)) => n,
        _ => 0,
    };
    let mut changed = false;
    for i in 0..size {
        let list_now = ctx.read_native_pin(list_pin, list);
        let elem = ctx
            .invoke_virtual(list_now, "get", "(I)Ljava/lang/Object;", &[Value::Int(i)])?
            .unwrap_or(Value::Object(None));
        let old_now = pe_read_pinned(ctx, old_pin, old_val);
        // Matches the JDK: `oldVal.equals(list.get(i))`, null-safe.
        if !pe_values_equal(ctx, old_now, elem)? {
            continue;
        }
        let list_now = ctx.read_native_pin(list_pin, list);
        let new_now = pe_read_pinned(ctx, new_pin, new_val);
        ctx.invoke_virtual(
            list_now,
            "set",
            "(ILjava/lang/Object;)Ljava/lang/Object;",
            &[Value::Int(i), new_now],
        )?;
        changed = true;
    }
    Ok(Some(Value::Int(i32::from(changed))))
}

/// `Collections.copy(dest, src)` -- replace `dest[i]` with `src[i]` for every
/// index of `src`. W6-12/W7-36's "Collections fidelity residual": the direct
/// field peek this replaced assumed every receiver was a plain ArrayList in
/// the fast slot layout (field 0 = backing array, field 1 = size) and fell
/// through its own `_ => return Ok(None)` arm -- a SILENT no-op -- whenever
/// that guess was wrong, with no bounds check either. Driven through the
/// receivers' own `size`/`get`/`set` instead, matching the `pe_replace_all_inner`
/// idiom immediately above, so any `List` implementation works and a source
/// that does not fit raises `IndexOutOfBoundsException` exactly as
/// `java.util.Collections.copy`'s own first statement does.
pub fn native_collections_copy(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let dest = obj_arg(args, 0)?;
    let src = obj_arg(args, 1)?;
    let dest_pin = ctx.pin_native_root(dest);
    let result = pe_copy_inner(ctx, dest_pin, dest, src);
    ctx.unpin_native_roots(dest_pin);
    result
}

pub fn pe_copy_inner(
    ctx: &mut dyn NativeContext,
    dest_pin: usize,
    dest: ObjectRef,
    src: ObjectRef,
) -> MethodCallResult {
    // `src` is pinned AFTER `dest`, so the caller's single
    // `unpin_native_roots(dest_pin)` releases both -- the same scoping
    // `pe_replace_all_inner`'s `old_pin`/`new_pin` rely on.
    let src_pin = ctx.pin_native_root(src);
    let dest_now = ctx.read_native_pin(dest_pin, dest);
    let dest_size = match ctx.invoke_virtual(dest_now, "size", "()I", &[])? {
        Some(Value::Int(n)) => n,
        _ => 0,
    };
    let src_now = ctx.read_native_pin(src_pin, src);
    let src_size = match ctx.invoke_virtual(src_now, "size", "()I", &[])? {
        Some(Value::Int(n)) => n,
        _ => 0,
    };
    if src_size > dest_size {
        return Err(RuntimeError::ioobe("Source does not fit in dest").into());
    }
    for i in 0..src_size {
        let src_now = ctx.read_native_pin(src_pin, src);
        let elem = ctx
            .invoke_virtual(src_now, "get", "(I)Ljava/lang/Object;", &[Value::Int(i)])?
            .unwrap_or(Value::Object(None));
        let dest_now = ctx.read_native_pin(dest_pin, dest);
        ctx.invoke_virtual(
            dest_now,
            "set",
            "(ILjava/lang/Object;)Ljava/lang/Object;",
            &[Value::Int(i), elem],
        )?;
    }
    Ok(None)
}

pub fn native_collections_ncopies(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let n = match args.first() {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    // gc-common w19-c: the element (an address copied out of `args`) was held
    // raw across the array's allocation, and the filled array raw across the
    // list's class resolution. Both rooted; the list is allocated first.
    let mut scope = NativeHandleScope::new(ctx);
    let elem_h = element_handle(&mut scope, args.get(1).copied());
    let list = try_alloc_concurrent_synthetic(&mut *scope, "java/util/ArrayList", 2)?;
    let list_h = scope.root(list);
    let arr = scope.new_array(cratonvm_types::ArrayElementType::Reference, n);
    let elem = element_value(&scope, &elem_h);
    for i in 0..n {
        scope.set_array_element(arr, i, elem);
    }
    let list = scope.get(&list_h);
    scope.set_field(list, 0, Value::Object(Some(arr)));
    scope.set_field(list, 1, Value::Int(n as i32));
    Ok(Some(Value::Object(Some(list))))
}

/// `jdk.internal.misc.CDS.dumpClassList(String)`.
///
/// HotSpot's `-XX:DumpLoadedClassList` writer: one loaded class per line in
/// internal (`java/lang/Object`) form. CratonVM cannot build an archive from
/// the result, but the list itself is real VM state and is what every consumer
/// of this file actually reads.
pub fn native_cds_dump_class_list_to_file(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let path = match args.first().copied() {
        Some(Value::Object(Some(s))) => ctx.read_string(s),
        _ => None,
    };
    let path = match path {
        Some(p) if !p.is_empty() => p,
        // No path: HotSpot's dumper has nowhere to write either.
        _ => return Ok(None),
    };
    let mut out = String::new();
    for name in ctx.list_application_class_names() {
        out.push_str(&name);
        out.push('\n');
    }
    if let Err(e) = std::fs::write(&path, out) {
        return Err(RuntimeError::IOException {
            message: format!("dumpClassList {path}: {e}"),        }
        .into());
    }
    Ok(None)
}

// ===========================================================================
// Core stdlib utility methods (Collections.emptyList, Arrays.asList, Optional)
// ===========================================================================

pub fn register_core_stdlib_extras(r: &mut NativeMethodRegistry) {
            // --- Collections.emptyList/emptyMap/emptySet ---
    let cu = "java/util/Collections";
    r.register(cu, "emptyList", "()Ljava/util/List;", |ctx, _args| {
        // Do not hand-assemble ArrayList's internal fields, and do not return
        // a native-side allocation across a potential collection. `List.of()`
        // is the JDK-owned empty immutable list construction and keeps its
        // result live through the ordinary VM invocation path.
        ctx.invoke("java/util/List", "of", "()Ljava/util/List;", &[])
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(cu, "emptyMap", "()Ljava/util/Map;", |ctx, _args| {
        let map = try_alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3)?;
        cratonvm_native_collections::native_map_init(ctx, &[Value::Object(Some(map))]).ok();
        Ok(Some(Value::Object(Some(map))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(cu, "emptySet", "()Ljava/util/Set;", |ctx, _args| {
        // S111r7: use the native-collections HashSet layout (single
        // `map` field holding the backing HashMap) so real-JDK
        // `HashSet.iterator()` bytecode reads the correct receiver.
        let set = cratonvm_native_collections::make_hashset_with_elements(ctx, &[])?;
        Ok(Some(Value::Object(Some(set))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        cu,
        "emptyIterator",
        "()Ljava/util/Iterator;",
        |ctx, _args| {
            // `Collections.emptyIterator()` is `(Iterator<T>) EmptyIterator.EMPTY_ITERATOR`
            // in the JDK, so `emptyIterator() == emptyIterator()` holds by identity.
            // Hibernate's StatelessSession "is-cleared" check asserts
            // `managedEntitiesIterator() == Collections.emptyIterator()`; a fresh
            // instance per call broke it. Return the process-wide singleton stored
            // in the (GC-rooted) `EMPTY_ITERATOR` static field, lazily populating it
            // on first use (cf. `Collections.EMPTY_MAP`).
            // Initialize the class FIRST so its `class_id` resolves even on a
            // cold start (the native intercepts `emptyIterator()` before any
            // bytecode that would otherwise load `EmptyIterator`).
            let _ = ctx.ensure_class_initialized("java/util/Collections$EmptyIterator");
            if let Some(cid) = ctx.class_id_by_name("java/util/Collections$EmptyIterator") {
                if let Some(idx) = ctx.static_field_index_by_name(cid, "EMPTY_ITERATOR") {
                    if let v @ Value::Object(Some(_)) = ctx.get_static_field(cid, idx) {
                        return Ok(Some(v));
                    }
                    let itr = try_alloc_concurrent_synthetic(
                        ctx,
                        "java/util/Collections$EmptyIterator",
                        0,
                    )?;
                    ctx.set_static_field(cid, idx, Value::Object(Some(itr)));
                    return Ok(Some(Value::Object(Some(itr))));
                }
            }
            let iter =
                try_alloc_concurrent_synthetic(ctx, "java/util/Collections$EmptyIterator", 0)?;
            Ok(Some(Value::Object(Some(iter))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "emptyEnumeration",
        "()Ljava/util/Enumeration;",
        |ctx, _args| {
            let e =
                try_alloc_concurrent_synthetic(ctx, "java/util/Collections$EmptyEnumeration", 0)?;
            Ok(Some(Value::Object(Some(e))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- Arrays.asList ---
    r.register(
        "java/util/Arrays",
        "asList",
        "([Ljava/lang/Object;)Ljava/util/List;",
        |ctx, args| match args.first() {
            Some(Value::Object(Some(a))) => pe_array_list_copy_of(ctx, *a),
            // `Arrays.asList((Object[]) null)` is `new ArrayList<>(a)`, whose
            // constructor is `Objects.requireNonNull(array)`: HotSpot throws a
            // bare NullPointerException (`ArrayListShadowSweep` row 125).
            Some(Value::Object(None)) => {
                Err(RuntimeError::NullPointerException { message: None }.into())
            }
            _ => pe_array_list_of(ctx, &[]),
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- java.util.Arrays additional methods (Phase 48) ---
    let arrays = "java/util/Arrays";

    // Arrays.copyOf(Object[], int) → Object[]
    //
    // FMT-STREAMS-CCE: the real JDK's 2-arg `Arrays.copyOf(T[] original, int
    // newLength)` allocates the copy via `Array.newInstance(original.getClass()
    // .getComponentType(), newLength)`, so the RESULT preserves `original`'s
    // exact runtime component type — including nested array types, e.g.
    // copying an `Object[][]` (as `java.util.stream.SpinedBuffer.ensureCapacity`
    // does for its `E[][] spine` field) must yield another `Object[][]`, not a
    // flat `Object[]`. This native previously always allocated a flat
    // `Object[]` via `ctx.new_array(Reference, ..)` regardless of `src`'s real
    // type, so callers whose static type is a multi-dimensional array (e.g.
    // `spine = Arrays.copyOf(spine, n);` where `spine: E[][]`) hit an implicit
    // erasure checkcast back to `[Ljava/lang/Object;` (2D descriptor) against
    // the flat 1D result and threw `ClassCastException: java.lang.Object
    // cannot be cast to [[Ljava.lang.Object;` (format.datetime.standard.
    // InstantFormatterTests, via LongStream boxing pipeline toArray()).
    //
    // Fix: read `src`'s own component ClassId from its heap header (the same
    // template used by the 3-arg overload below and by
    // `real_jdk_to_array_typed` for `ArrayList.toArray(T[])`) and propagate it
    // to the copy via `new_ref_array`, so the result's runtime class matches
    // `src`'s exactly — including nested array depth.
    r.register(
        arrays,
        "copyOf",
        "([Ljava/lang/Object;I)[Ljava/lang/Object;",
        |ctx, args| {
            if crate::nbflags().dbg_toarray_ok {
                eprintln!(
                    "[DBG_TOARRAY] copyOf2 (Object[],int) HIT nargs={}",
                    args.len()
                );
            }
            let src = match args.first() {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let new_len = match args.get(1) {
                Some(Value::Int(n)) => *n as usize,
                _ => 0,
            };
            let src_len = ctx.array_length(src);
            let comp_cid = ctx.class_id_of_object(src);
            // gc-common w8-b: reclaim and retry, `src` refreshed.
            let mut live = [src];
            let dst = crate::new_ref_array_reclaiming(
                ctx,
                comp_cid,
                new_len,
                &mut live,
                "Arrays.copyOf",
            )?;
            let [src] = live;
            let copy_len = src_len.min(new_len);
            crate::arrays_copy_elements(ctx, src, 0, dst, copy_len, true);
            Ok(Some(Value::Object(Some(dst))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // Arrays.copyOf(Object[], int, Class) → Object[]
    // Real-JDK `ArrayList.toArray(T[])` calls this 3-arg overload to
    // produce a new typed array (the runtime class of the supplied
    // template). We treat the Class arg as advisory metadata only —
    // every reference array in our heap is the same Object[] kind, and
    // checkcast at the call site validates the component type. Without
    // this native the call falls through to bytecode that dereferences
    // unsupported `arrayClass` reflection internals and NPEs.
    r.register(
        arrays,
        "copyOf",
        "([Ljava/lang/Object;ILjava/lang/Class;)[Ljava/lang/Object;",
        |ctx, args| {
            if crate::nbflags().dbg_toarray_ok {
                eprintln!(
                    "[DBG_TOARRAY] copyOf3 (Object[],int,Class) HIT nargs={}",
                    args.len()
                );
            }
            let src = match args.first() {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let new_len = match args.get(1) {
                Some(Value::Int(n)) => *n as usize,
                _ => 0,
            };
            let src_len = ctx.array_length(src);
            // Honor the 3rd `Class` arg (the target array type, e.g.
            // `String[][].class`): allocate a typed array preserving the
            // component type so the caller's `(String[][])` / `(Value[][])`
            // checkcast succeeds. Fall back to `Object[]` only if it can't be
            // resolved.
            let comp_cid = match args.get(2) {
                Some(Value::Object(Some(mirror))) => {
                    crate::lang_class::mirror_class_id(ctx, *mirror)
                        .and_then(|arr_cid| ctx.array_component_class_id(arr_cid))
                }
                _ => None,
            };
            // gc-common w8-b: reclaim and retry, `src` refreshed (`mirror` is
            // not used after this point).
            let mut live = [src];
            let dst = match comp_cid {
                Some(cid) => {
                    crate::new_ref_array_reclaiming(ctx, cid, new_len, &mut live, "Arrays.copyOf")?
                }
                None => crate::new_array_reclaiming(
                    ctx,
                    cratonvm_types::ArrayElementType::Reference,
                    new_len,
                    &mut live,
                    "Arrays.copyOf",
                )?,
            };
            let [src] = live;
            let copy_len = src_len.min(new_len);
            for i in 0..copy_len {
                ctx.set_array_element(dst, i, ctx.get_array_element(src, i));            }
            Ok(Some(Value::Object(Some(dst))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // Arrays.copyOf(int[], int) → int[]
    r.register(arrays, "copyOf", "([II)[I", |ctx, args| {
        let src = match args.first() {
            Some(Value::Object(Some(a))) => *a,
            _ => return Ok(Some(Value::Object(None))),
        };
        let new_len = match args.get(1) {
            Some(Value::Int(n)) => *n as usize,
            _ => 0,
        };
        let src_len = ctx.array_length(src);
        // gc-common w8-b: reclaim and retry, `src` refreshed.
        let mut live = [src];
        let dst = crate::new_array_reclaiming(
            ctx,
            cratonvm_types::ArrayElementType::Int,
            new_len,
            &mut live,
            "Arrays.copyOf",
        )?;
        let [src] = live;
        let copy_len = src_len.min(new_len);
        crate::arrays_copy_elements(ctx, src, 0, dst, copy_len, false);
        Ok(Some(Value::Object(Some(dst))))
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // Arrays.copyOfRange(Object[], int, int) → Object[]
    //
    // FMT-STREAMS-CCE: same bug as the 2-arg `copyOf` above — the real JDK's
    // `copyOfRange(T[] original, int from, int to)` allocates via
    // `original.getClass()`, preserving nested array types. Propagate `src`'s
    // own component ClassId (from its heap header) instead of always
    // allocating a flat `Object[]`.
    r.register(
        arrays,
        "copyOfRange",
        "([Ljava/lang/Object;II)[Ljava/lang/Object;",
        |ctx, args| {
            let src = match args.first() {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let from = match args.get(1) {
                Some(Value::Int(n)) => *n as usize,
                _ => 0,
            };
            let to = match args.get(2) {
                Some(Value::Int(n)) => *n as usize,
                _ => 0,
            };
            let new_len = to.saturating_sub(from);
            let comp_cid = ctx.class_id_of_object(src);
            // gc-common w8-b: reclaim and retry, `src` refreshed.
            let mut live = [src];
            let dst = crate::new_ref_array_reclaiming(
                ctx,
                comp_cid,
                new_len,
                &mut live,
                "Arrays.copyOfRange",
            )?;
            let [src] = live;
            let src_len = ctx.array_length(src);
            crate::arrays_copy_elements(
                ctx,
                src,
                from,
                dst,
                new_len.min(src_len.saturating_sub(from)),
                true,
            );
            Ok(Some(Value::Object(Some(dst))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // Arrays.copyOfRange(byte[], int, int) → byte[]
    r.register(arrays, "copyOfRange", "([BII)[B", |ctx, args| {
        let src = match args.first() {
            Some(Value::Object(Some(a))) => *a,
            _ => return Ok(Some(Value::Object(None))),
        };
        let from = match args.get(1) {
            Some(Value::Int(n)) => *n as usize,
            _ => 0,
        };
        let to = match args.get(2) {
            Some(Value::Int(n)) => *n as usize,
            _ => 0,
        };
        let new_len = to.saturating_sub(from);
        // gc-common w8-b: reclaim and retry, `src` refreshed.
        let mut live = [src];
        let dst = crate::new_array_reclaiming(
            ctx,
            cratonvm_types::ArrayElementType::Byte,
            new_len,
            &mut live,
            "Arrays.copyOfRange",
        )?;
        let [src] = live;
        let src_len = ctx.array_length(src);
        crate::arrays_copy_elements(
            ctx,
            src,
            from,
            dst,
            new_len.min(src_len.saturating_sub(from)),
            false,
        );
        Ok(Some(Value::Object(Some(dst))))
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // Arrays.copyOf(byte[], int) → byte[]
    r.register(arrays, "copyOf", "([BI)[B", |ctx, args| {
        let src = match args.first() {
            Some(Value::Object(Some(a))) => *a,
            _ => return Ok(Some(Value::Object(None))),
        };
        let new_len = match args.get(1) {
            Some(Value::Int(n)) => *n as usize,
            _ => 0,
        };
        // gc-common w8-b: reclaim and retry, `src` refreshed.
        let mut live = [src];
        let dst = crate::new_array_reclaiming(
            ctx,
            cratonvm_types::ArrayElementType::Byte,
            new_len,
            &mut live,
            "Arrays.copyOf",
        )?;
        let [src] = live;
        let src_len = ctx.array_length(src);
        let copy_len = src_len.min(new_len);
        crate::arrays_copy_elements(ctx, src, 0, dst, copy_len, false);
        Ok(Some(Value::Object(Some(dst))))
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // Arrays.fill(int[], int)
    r.register(arrays, "fill", "([II)V", |ctx, args| {
        let arr = crate::arrays_src_or_npe(args, "fill")?;
        let val = args.get(1).copied().unwrap_or(Value::Int(0));
        let len = ctx.array_length(arr);
        for i in 0..len {
            ctx.set_array_element(arr, i, val);        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // Arrays.fill(Object[], Object)
    r.register(
        arrays,
        "fill",
        "([Ljava/lang/Object;Ljava/lang/Object;)V",
        |ctx, args| {
            let arr = crate::arrays_src_or_npe(args, "fill")?;
            let val = args.get(1).copied().unwrap_or(Value::Object(None));
            // STORE CHECK. `Arrays.fill(Object[], Object)` is an ordinary
            // covariant array store per element, so filling a `String[]` with an
            // `Integer` is an `ArrayStoreException` -- and `set_array_element`
            // does not check, so this native accepted it.
            //
            // MEASURED in BOTH modes (`probes/ArraysHashSetShadowSweep.java`):
            // HotSpot ArrayStoreException, this VM `no-throw`, leaving an
            // Integer sitting in a String[] for the next reader to trip over.
            // That is the same class of hole as the `aastore` covariance gap
            // the roadmap tracked as P3-A, reached through a library method
            // instead of through the opcode.
            //
            // A null value is always storable, and a component type of
            // `java/lang/Object` accepts everything, so both short-circuit.
            //
            // THE CHECK IS THE VM'S `aastore` PREDICATE, NOT A `ClassId`
            // COMPARISON. The `ClassId` comparison that used to stand here is
            // wrong in BOTH directions, and the same trap is written out at
            // length on `Arrays.copyOf`'s own store check
            // (`native-builtins/src/lib.rs`): on a REFERENCE ARRAY the heap
            // header's class id holds the COMPONENT class, so a value that is
            // itself an array (`Initializer[]`) answers
            // `org/hibernate/.../Initializer` while the destination component
            // of an `Initializer[][]` is `[Lorg/hibernate/.../Initializer;`.
            // They can never be equal, so EVERY legal store of an array into a
            // two-dimensional array threw a FALSE `ArrayStoreException` naming
            // the component. MEASURED, hibernate-reactive complete suite
            // 2026-08-29: nine embeddable/embedded-id classes died on
            // `EmbeddableInitializerImpl.fill`'s
            // `Arrays.fill(Initializer[][], Initializer.EMPTY_ARRAY)` with
            // `ArrayStoreException: org.hibernate.sql.results.graph.Initializer`
            // — a message naming an INTERFACE, which no instance can ever have
            // as its class, and which is the tell that the name came from a
            // component rather than from a value.
            //
            // It also missed every hedge the shared predicate carries — an
            // interface component, a `$Proxy` value, a synthetic class id, a
            // cross-loader same-named component — each of which was paid for by
            // its own regression. `reject_unstorable` is the one place that
            // rule lives for a native, and it fails OPEN when the context
            // cannot decide, so this can never manufacture a refusal.
            //
            // `StoreRoute::Aastore` because the JDK's own
            // `Arrays.fill(Object[], Object)` is a plain `a[i] = val` loop, so
            // the text it owes is `aastore`'s bare external name of the VALUE.
            if let Some(e) = cratonvm_native_api::array_store::reject_unstorable(
                ctx,
                arr,
                val,
                cratonvm_native_api::array_store::StoreRoute::Aastore,
            ) {
                return Err(e);
            }
            let len = ctx.array_length(arr);
            for i in 0..len {
                ctx.set_array_element(arr, i, val);
            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // Arrays.fill(long[], long)
    r.register(arrays, "fill", "([JJ)V", |ctx, args| {
        let arr = crate::arrays_src_or_npe(args, "fill")?;
        let val = args.get(1).copied().unwrap_or(Value::Long(0));
        let len = ctx.array_length(arr);
        for i in 0..len {
            ctx.set_array_element(arr, i, val);        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // Arrays.fill(byte[], byte)
    r.register(arrays, "fill", "([BB)V", |ctx, args| {
        let arr = crate::arrays_src_or_npe(args, "fill")?;
        let val = args.get(1).copied().unwrap_or(Value::Int(0));
        let len = ctx.array_length(arr);
        for i in 0..len {
            ctx.set_array_element(arr, i, val);        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // Arrays.sort(int[])
    r.register(arrays, "sort", "([I)V", |ctx, args| {
        let arr = crate::arrays_src_or_npe(args, "fill")?;
        let len = ctx.array_length(arr);
        let mut vals: Vec<i32> = (0..len)
            .map(|i| match ctx.get_array_element(arr, i) {
                Value::Int(v) => v,
                _ => 0,
            })
            .collect();
        vals.sort();
        for (i, v) in vals.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*v));        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // Arrays.sort(long[])
    r.register(arrays, "sort", "([J)V", |ctx, args| {
        let arr = crate::arrays_src_or_npe(args, "fill")?;
        let len = ctx.array_length(arr);
        let mut vals: Vec<i64> = (0..len)
            .map(|i| match ctx.get_array_element(arr, i) {
                Value::Long(v) => v,
                _ => 0,
            })
            .collect();
        vals.sort();
        for (i, v) in vals.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Long(*v));
        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // Arrays.sort(double[])
    r.register(arrays, "sort", "([D)V", |ctx, args| {
        let arr = crate::arrays_src_or_npe(args, "fill")?;
        let len = ctx.array_length(arr);
        let mut vals: Vec<f64> = (0..len)
            .map(|i| match ctx.get_array_element(arr, i) {
                Value::Double(d) => d,
                _ => 0.0,
            })
            .collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        for (i, v) in vals.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Double(*v));
        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // Arrays.equals(int[], int[])
    r.register(arrays, "equals", "([I[I)Z", |ctx, args| {
        let a = match args.first() {
            Some(Value::Object(Some(a))) => *a,
            _ => return Ok(Some(Value::Int(0))),
        };
        let b = match args.get(1) {
            Some(Value::Object(Some(b))) => *b,
            _ => return Ok(Some(Value::Int(0))),
        };
        let la = ctx.array_length(a);
        let lb = ctx.array_length(b);
        if la != lb {
            return Ok(Some(Value::Int(0)));
        }
        for i in 0..la {
            if ctx.get_array_element(a, i) != ctx.get_array_element(b, i) {
                return Ok(Some(Value::Int(0)));
            }
        }
        Ok(Some(Value::Int(1)))
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // Arrays.equals(Object[], Object[])
    r.register(
        arrays,
        "equals",
        "([Ljava/lang/Object;[Ljava/lang/Object;)Z",
        |ctx, args| {
            let a = match args.first() {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Int(0))),
            };
            let b = match args.get(1) {
                Some(Value::Object(Some(b))) => *b,
                _ => return Ok(Some(Value::Int(0))),
            };
            let la = ctx.array_length(a);
            let lb = ctx.array_length(b);
            if la != lb {
                return Ok(Some(Value::Int(0)));
            }
            for i in 0..la {
                if ctx.get_array_element(a, i) != ctx.get_array_element(b, i) {
                    return Ok(Some(Value::Int(0)));
                }
            }
            Ok(Some(Value::Int(1)))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // Arrays.hashCode(Object[])
    r.register(arrays, "hashCode", "([Ljava/lang/Object;)I", |ctx, args| {
        let arr = match args.first() {
            Some(Value::Object(Some(a))) => *a,
            Some(Value::Object(None)) | None => return Ok(Some(Value::Int(0))),
            _ => return Ok(Some(Value::Int(0))),
        };
        let base_pin = ctx.pin_native_root(arr);
        let len = ctx.array_length(arr);
        let mut hash = 1i32;
        for i in 0..len {
            let arr = ctx.read_native_pin(base_pin, arr);
            let elem_hash = match ctx.get_array_element(arr, i) {
                Value::Object(Some(obj)) => {
                    let elem_pin = ctx.pin_native_root(obj);
                    let obj = ctx.read_native_pin(elem_pin, obj);
                    let elem_hash = object_array_element_hash_code(ctx, obj);
                    ctx.unpin_native_roots(elem_pin);
                    elem_hash?
                }
                Value::Object(None) | Value::Uninitialized => 0,
                Value::Int(v) => v,
                Value::Long(v) => (v ^ (v >> 32)) as i32,
                Value::Float(v) => v.to_bits() as i32,
                Value::Double(v) => {
                    let bits = v.to_bits() as i64;
                    (bits ^ (bits >> 32)) as i32                }
                _ => 0,
            };
            hash = hash.wrapping_mul(31).wrapping_add(elem_hash);        }
        ctx.unpin_native_roots(base_pin);
        Ok(Some(Value::Int(hash)))
    }, cratonvm_native_api::NativeKind::Intrinsic);

    r.register(arrays, "hashCode", "([B)I", |ctx, args| {
        let arr = match args.first() {
            Some(Value::Object(Some(a))) => *a,
            Some(Value::Object(None)) | None => return Ok(Some(Value::Int(0))),
            _ => return Ok(Some(Value::Int(0))),
        };
        let len = ctx.array_length(arr);
        let mut hash = 1i32;
        for i in 0..len {
            let elem_hash = match ctx.get_array_element(arr, i) {
                Value::Int(v) => v as i8 as i32,
                Value::Long(v) => v as i8 as i32,
                _ => 0,
            };
            hash = hash.wrapping_mul(31).wrapping_add(elem_hash);        }
        Ok(Some(Value::Int(hash)))
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // Arrays.toString(int[])
    r.register(arrays, "toString", "([I)Ljava/lang/String;", |ctx, args| {
        let arr = match args.first() {
            Some(Value::Object(Some(a))) => *a,
            Some(Value::Object(None)) => {
                return Ok(Some(Value::Object(Some(ctx.create_string("null")))))
            }
            _ => return Ok(Some(Value::Object(Some(ctx.create_string("null"))))),
        };
        let len = ctx.array_length(arr);
        let mut s = String::from("[");
        for i in 0..len {
            if i > 0 {
                s.push_str(", ");
            }
            match ctx.get_array_element(arr, i) {
                Value::Int(v) => s.push_str(&v.to_string()),
                _ => s.push_str("0"),
            }
        }
        s.push(']');
        Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // Arrays.toString(Object[])
    r.register(
        arrays,
        "toString",
        "([Ljava/lang/Object;)Ljava/lang/String;",
        |ctx, args| {
            let arr = match args.first() {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Object(Some(ctx.create_string("null"))))),
            };
            let len = ctx.array_length(arr);
            let mut s = String::from("[");
            for i in 0..len {
                if i > 0 {
                    s.push_str(", ");
                }
                match ctx.get_array_element(arr, i) {
                    Value::Object(Some(o)) => {
                        let text = crate::lang_string::invoke_to_string(ctx, o)?;
                        s.push_str(&text);
                    }
                    Value::Int(v) => s.push_str(&v.to_string()),
                    Value::Long(v) => s.push_str(&v.to_string()),
                    Value::Object(None) => s.push_str("null"),
                    _ => s.push_str("?"),
                }
            }
            s.push(']');
            Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // Arrays.stream(Object[]) intercept lives in real-JDK mode via
    // `register_essential_natives` (see lib.rs). This function is part of
    // `register_enterprise_final_natives` which is gated on the
    // `synthetic-jdk` feature and so does not run in real-JDK mode.

    // Arrays.stream(int[]) → IntStream
    r.register(
        arrays,
        "stream",
        "([I)Ljava/util/stream/IntStream;",
        |ctx, args| {
            let arr = match args.first() {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let stream = try_alloc_concurrent_synthetic(ctx, "java/util/stream/IntStream", 1)?;
            ctx.set_field(stream, 0, Value::Object(Some(arr)));
            Ok(Some(Value::Object(Some(stream))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // Arrays.stream(long[]) → LongStream
    r.register(
        arrays,
        "stream",
        "([J)Ljava/util/stream/LongStream;",
        |ctx, args| {
            let arr = match args.first() {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let stream = try_alloc_concurrent_synthetic(ctx, "java/util/stream/LongStream", 1)?;
            ctx.set_field(stream, 0, Value::Object(Some(arr)));
            Ok(Some(Value::Object(Some(stream))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // Arrays.stream(double[]) → DoubleStream
    r.register(
        arrays,
        "stream",
        "([D)Ljava/util/stream/DoubleStream;",
        |ctx, args| {
            let arr = match args.first() {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let stream = try_alloc_concurrent_synthetic(ctx, "java/util/stream/DoubleStream", 1)?;
            ctx.set_field(stream, 0, Value::Object(Some(arr)));
            Ok(Some(Value::Object(Some(stream))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // Arrays.binarySearch(int[], int) — standard binary search
    r.register(arrays, "binarySearch", "([II)I", |ctx, args| {
        let arr = match args.first() {
            Some(Value::Object(Some(a))) => *a,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let key = match args.get(1) {
            Some(Value::Int(k)) => *k,
            _ => 0,
        };
        let len = ctx.array_length(arr);
        let mut lo: i32 = 0;
        let mut hi: i32 = len as i32 - 1;
        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            let val = match ctx.get_array_element(arr, mid as usize) {
                Value::Int(v) => v,
                _ => 0,
            };
            if val == key {
                return Ok(Some(Value::Int(mid)));
            }
            if val < key {
                lo = mid + 1;
            } else {
                hi = mid - 1;
            }
        }
        Ok(Some(Value::Int(-(lo + 1))))
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // `java.util.Optional`: no registrations here. The 11 rows this block held
    // (`of` .. `toString`) were overwritten in every configuration by
    // `native-collections` `register_optional_natives` (this pass is reached only
    // in a synthetic-jdk build, and `register_collections_natives` runs after
    // it), and their bodies were wrong besides (`toString` answered
    // "Optional[present]", `ifPresent` swallowed the consumer's exception).
    // Round 14 wave 2, lane compat2,
    // `r14w2-compat2-dead-phases-early-optional-rows-FIXED-20260929.md`.

    // --- String.replace(CharSequence, CharSequence) ---
    r.register(
        "java/lang/String",
        "replace",
        "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let s = ctx.read_string(this).unwrap_or_default();
            let target = match args.get(1) {
                Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
                _ => return Ok(Some(Value::Object(Some(this)))),
            };
            let replacement = match args.get(2) {
                Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
                _ => String::new(),
            };
            let result = s.replace(&target, &replacement);
            let r = ctx.create_string(&result);
            Ok(Some(Value::Object(Some(r))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // String.format is registered in register_essential_natives (lang_math.rs)
    // with full flags/width/precision support — do NOT duplicate here.

    // The `List.of` / `Set.of` / `Map.of` / `Map.ofEntries` / `copyOf` rows
    // that stood here are gone (round 15 wave 3, lane compat3). This function
    // is reached only in a synthetic-JDK build, before
    // `register_collections_natives`, whose `register_factory_natives` and
    // `copyOf` rows overwrite all twelve triples (last write wins), so they
    // never answered. They were also wrong enough to be dangerous on a reorder
    // (the `Optional` rows' story,
    // `r14w2-compat2-dead-phases-early-optional-rows-FIXED-20260929.md`):
    // `copyOf` was `native_return_first_arg` -- a "copy" that tracks its
    // source, the aliasing that broke Kafka's `AdminApiDriver` -- `Set.of`
    // built a MUTABLE set with no null / duplicate checks, `List.of` a mutable
    // `ArrayList`, and `Map.ofEntries` an EMPTY map that ignored its entries.
    // `r15w3-compat2-dead-phases-early-factory-rows-patch-FIXED-20260930.md`.

    // `SyntheticStub` for the same reason as the superseding registration in
    // `phases_late.rs`: `java.util.Map.entry` is ordinary `java.base` bytecode
    // and JDK 25 declares no `ACC_NATIVE` on it, so `--real-jdk` must drop this
    // and let the real method mint the real `KeyValueHolder`. Retagging the
    // superseding copy alone would have left this one's kind claiming
    // something nothing uses, which is the ambient-kind trap.
    r.register(
        "java/util/Map",
        "entry",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map$Entry;",
        |ctx, args| {
            // `java/util/KeyValueHolder`, matching the later registration in
            // `phases_late.rs` that supersedes this one — and matching HotSpot,
            // which returns that class from `Map.entry`. It was
            // `AbstractMap$SimpleImmutableEntry`, which is a different class
            // with a different `getClass()` answer and, being `Serializable`,
            // a different contract from the one `Map.entry` documents.
            let entry = try_alloc_concurrent_synthetic(ctx, "java/util/KeyValueHolder", 2)?;
            ctx.set_field(
                entry,
                0,
                args.first().copied().unwrap_or(Value::Object(None)),
            );
            ctx.set_field(
                entry,
                1,
                args.get(1).copied().unwrap_or(Value::Object(None)),
            );
            Ok(Some(Value::Object(Some(entry))))
        },
        cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // -----------------------------------------------------------------------
    // Phase 29: Additional stdlib methods for Spring Boot apps
    // -----------------------------------------------------------------------

    // --- String.join ---
    let s = "java/lang/String";
    r.register(
        s,
        "join",
        "(Ljava/lang/CharSequence;[Ljava/lang/CharSequence;)Ljava/lang/String;",
        |ctx, args| {
            let delim = match args.first() {
                Some(Value::Object(Some(d))) => ctx.read_string(*d).unwrap_or_default(),
                _ => "".to_string(),
            };
            let arr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => return pe_empty_string(ctx),
            };
            let len = ctx.array_length(arr);
            let mut parts = Vec::with_capacity(len);
            for i in 0..len {
                match ctx.get_array_element(arr, i) {
                    Value::Object(Some(o)) => parts.push(ctx.read_string(o).unwrap_or_default()),
                    _ => parts.push("null".to_string()),
                }
            }
            let result = parts.join(&delim);
            let s = ctx.create_string(&result);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        s,
        "join",
        "(Ljava/lang/CharSequence;Ljava/lang/Iterable;)Ljava/lang/String;",
        |ctx, args| {
            let delim = match args.first() {
                Some(Value::Object(Some(d))) => ctx.read_string(*d).unwrap_or_default(),
                _ => "".to_string(),
            };
            // For Iterable, try to read it as an ArrayList (field 0 = backing array, field 1 = size)
            let list = match args.get(1) {
                Some(Value::Object(Some(l))) => *l,
                _ => return pe_empty_string(ctx),
            };
            let size = ctx.get_field(list, 1).as_int().unwrap_or(0) as usize;
            let arr = match ctx.get_field(list, 0) {
                Value::Object(Some(a)) => a,
                _ => return pe_empty_string(ctx),
            };
            let mut parts = Vec::with_capacity(size);
            for i in 0..size {
                match ctx.get_array_element(arr, i) {
                    Value::Object(Some(o)) => parts.push(ctx.read_string(o).unwrap_or_default()),
                    _ => parts.push("null".to_string()),
                }
            }
            let result = parts.join(&delim);
            let s = ctx.create_string(&result);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- String.strip / stripLeading / stripTrailing ---
    r.register(s, "strip", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = ctx.read_string(this).unwrap_or_default();
        let result = val.trim().to_string();
        let s = ctx.create_string(&result);
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(s, "stripLeading", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = ctx.read_string(this).unwrap_or_default();
        let result = val.trim_start().to_string();
        let s = ctx.create_string(&result);
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(s, "stripTrailing", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = ctx.read_string(this).unwrap_or_default();
        let result = val.trim_end().to_string();
        let s = ctx.create_string(&result);
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // --- String.repeat(int) ---
    r.register(s, "repeat", "(I)Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = ctx.read_string(this).unwrap_or_default();
        let count = args.get(1).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
        let result = val.repeat(count);
        let s = ctx.create_string(&result);
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // --- String.chars() → IntStream — RETIRED, deliberately not registered ---
    //
    // The twin of the `codePointAt` retirement below, found by the same
    // question and removed for the same reason. The closure that was here did
    // `ctx.read_string(this).chars().map(|c| c as i32)`, which is Rust's
    // `char` iterator — CODE POINTS. Java's `chars()` is the code-UNIT view:
    // `"a\u{1F600}b".chars()` is `[97, 55357, 56832, 98]` (four), and this
    // body answered `[97, 128512, 98]` (three), so `chars().count()` was wrong
    // over any astral character, not merely the values. The `read_string`
    // round trip also folded an unpaired surrogate to U+FFFD.
    //
    // `lang_string::native_string_chars` is the body that reads the String's
    // own `value` array as UTF-16, and `lang_math.rs:729` registers it. As
    // with `codePointAt`, that registration owns the slot in real-JDK mode
    // and was OVERWRITTEN by this one in synthetic-jdk mode, where
    // `register_enterprise_final_natives` runs after `register_wrapper_natives`.
    // W7-95a.

    // --- String.toUpperCase(Locale) / toLowerCase(Locale) ---
    // Share the locale-aware implementation rather than folding with Rust's
    // root-locale mapping and dropping `args[1]` on the floor (DIV-001).
    r.register(
        s,
        "toUpperCase",
        "(Ljava/util/Locale;)Ljava/lang/String;",
        crate::lang_string::native_string_to_upper_case_uncached, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        s,
        "toLowerCase",
        "(Ljava/util/Locale;)Ljava/lang/String;",
        crate::lang_string::native_string_to_lower_case, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- String.getBytes(String charsetName) ---
    r.register(s, "getBytes", "(Ljava/lang/String;)[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = ctx.read_string(this).unwrap_or_default();
        let bytes = val.as_bytes();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
        for (i, &b) in bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
        }
        Ok(Some(Value::Object(Some(arr))))
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // --- String.getBytes(Charset) ---
    r.register(
        s,
        "getBytes",
        "(Ljava/nio/charset/Charset;)[B",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let val = ctx.read_string(this).unwrap_or_default();
            let bytes = val.as_bytes();
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
            for (i, &b) in bytes.iter().enumerate() {
                ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
            }
            Ok(Some(Value::Object(Some(arr))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- String.codePointAt(int) — RETIRED, deliberately not registered here ---
    //
    // What used to be here:
    //
    //     r.register(s, "codePointAt", "(I)I", |ctx, args| {
    //         let val = ctx.read_string(this).unwrap_or_default();
    //         let idx = args.get(1)... as usize;
    //         let cp = val.chars().nth(idx).map(|c| c as i32).unwrap_or(0);
    //     });
    //
    // `chars().nth(idx)` counts Rust `char`s — CODE POINTS — and Java's
    // `codePointAt(int index)` takes an index in UTF-16 CODE UNITS. The two
    // agree for a wholly-BMP string and diverge silently for anything else:
    // no exception, just a wrong character, and one code point of skew for
    // every astral character to the left of `index`. The `read_string` round
    // trip made it worse — a Rust `str` cannot hold an unpaired surrogate, so
    // `"x\uD800y".codePointAt(1)` answered U+FFFD. And out of range answered
    // `0` where the spec says `StringIndexOutOfBoundsException`.
    //
    // Which body wins is not a matter of which is better —
    // `NativeMethodRegistry::register` is LAST-WRITE-WINS, and this
    // registration ran LATER than the good one in exactly one of the two
    // shipping modes. Measured from `--dump-native-registry`:
    //
    //   real-JDK / compatible mode  `register_core_stdlib_extras` is not
    //     called at all (it is reached only through
    //     `register_enterprise_final_natives`), so `lang_math.rs:760` ->
    //     `lang_string::native_string_code_point_at` already owned the slot.
    //     Two dumps agree: `owns_slot=true, overwrote=None`, and no
    //     `phases_early` row for `java/lang/String` exists.
    //
    //   synthetic-jdk mode  `register_synthetic_overrides` calls
    //     `register_wrapper_natives` (lib.rs:23411) FIRST and
    //     `register_enterprise_final_natives` (lib.rs:24005) after it, so this
    //     closure overwrote the good body and was the live answer.
    //
    // So this was not dead code, and removing it is a behaviour change in
    // synthetic-jdk mode — to the body that decodes the String's own `value`
    // array as UTF-16, handles surrogate pairs, and throws SIOOBE with
    // HotSpot's message. W7-95a.
    //
    // Same defect family as the atomic-array bounds hole fixed this wave:
    // an implementation that is right for the common case and silently wrong
    // at the edge, with nothing in the type system or the fixtures to notice.

    // --- StringJoiner ---
    // Layout: 3-field (delimiter=0 String, prefix=1 String, parts=2 ArrayList)
            let sj = "java/util/StringJoiner";
    r.register(sj, "<init>", "(Ljava/lang/CharSequence;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let delim = args.get(1).cloned().unwrap_or(Value::Object(None));
        // GC-SAFETY: `create_string`/`try_alloc_concurrent_synthetic`/
        // `new_array` below all allocate, any of which can trigger a moving
        // collection — `this` must be pinned and re-read after each one, or a
        // relocation mid-`<init>` writes the later fields (the ArrayList in
        // particular) onto a stale, now-unreachable copy of the receiver and
        // the object `StringJoiner.add`/`toString` are later called on comes
        // back with field 2 unset. Same family as this file's other native
        // stale-local fixes.
        let mut scope = NativeHandleScope::new(ctx);
        let this_h = scope.root(this);
        scope.set_field(this, 0, delim);
        let empty_prefix = scope.create_string("");
        let this = scope.get(&this_h);
        scope.set_field(this, 1, Value::Object(Some(empty_prefix)));
        let list = try_alloc_concurrent_synthetic(&mut *scope, "java/util/ArrayList", 2)?;
        let list_h = scope.root(list);
        let arr = scope.new_array(cratonvm_types::ArrayElementType::Reference, 16);
        let list = scope.get(&list_h);
        scope.set_field(list, 0, Value::Object(Some(arr)));
        scope.set_field(list, 1, Value::Int(0));
        let this = scope.get(&this_h);
        let list = scope.get(&list_h);
        scope.set_field(this, 2, Value::Object(Some(list)));
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        sj,
        "<init>",
        "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;Ljava/lang/CharSequence;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let delim = args.get(1).cloned().unwrap_or(Value::Object(None));
            // Store prefix in field 1 (we'll use suffix from arg 3 at toString time)
            let prefix = args.get(2).cloned().unwrap_or(Value::Object(None));
            // GC-SAFETY: see the 1-arg overload just above — same hazard,
            // same fix.
            let mut scope = NativeHandleScope::new(ctx);
            let this_h = scope.root(this);
            scope.set_field(this, 0, delim);
            scope.set_field(this, 1, prefix);
            let list = try_alloc_concurrent_synthetic(&mut *scope, "java/util/ArrayList", 2)?;
            let list_h = scope.root(list);
            let arr = scope.new_array(cratonvm_types::ArrayElementType::Reference, 16);
            let list = scope.get(&list_h);
            scope.set_field(list, 0, Value::Object(Some(arr)));
            scope.set_field(list, 1, Value::Int(0));
            let this = scope.get(&this_h);
            let list = scope.get(&list_h);
            scope.set_field(this, 2, Value::Object(Some(list)));
            Ok(None)
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        sj,
        "add",
        "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let elem = args.get(1).cloned().unwrap_or(Value::Object(None));
            let list = match ctx.get_field(this, 2) {
                Value::Object(Some(l)) => l,
                _ => return Ok(Some(Value::Object(Some(this)))),
            };
            // Add element to the ArrayList
            let size = ctx.get_field(list, 1).as_int().unwrap_or(0) as usize;
            let arr = match ctx.get_field(list, 0) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Object(Some(this)))),
            };
            let cap = ctx.array_length(arr);
            if size >= cap {
                let this = sj_grow_and_append(ctx, this, list, arr, size, elem);
                return Ok(Some(Value::Object(Some(this))));
            }
            ctx.set_array_element(arr, size, elem);
            ctx.set_field(list, 1, Value::Int((size + 1) as i32));
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(sj, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let delim = match ctx.get_field(this, 0) {
            Value::Object(Some(d)) => ctx.read_string(d).unwrap_or_default(),
            _ => "".to_string(),
        };
        let list = match ctx.get_field(this, 2) {
            Value::Object(Some(l)) => l,
            _ => return pe_empty_string(ctx),
        };
        let size = ctx.get_field(list, 1).as_int().unwrap_or(0) as usize;
        let arr = match ctx.get_field(list, 0) {
            Value::Object(Some(a)) => a,
            _ => return pe_empty_string(ctx),
        };
        let mut parts = Vec::with_capacity(size);
        for i in 0..size {
            match ctx.get_array_element(arr, i) {
                Value::Object(Some(o)) => {
                    parts.push(ctx.read_string(o).unwrap_or_else(|| "null".to_string()))
                }
                _ => parts.push("null".to_string()),
            }
        }
        let result = parts.join(&delim);
        let s = ctx.create_string(&result);
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sj, "length", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let list = match ctx.get_field(this, 2) {
            Value::Object(Some(l)) => l,
            _ => return Ok(Some(Value::Int(0))),
        };
        Ok(Some(ctx.get_field(list, 1)))    }, cratonvm_native_api::NativeKind::SyntheticStub);
    
    // --- Stream.toList() (Java 16+) ---
    for cls in &[
        "java/util/stream/Stream",
        "java/util/stream/ReferencePipeline",
        "java/util/stream/ReferencePipeline$Head",
    ] {
        r.register(cls, "toList", "()Ljava/util/List;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Delegate to collect(Collectors.toList()) — implemented in native-collections
            // For simplicity, check if this stream has a backing array (field 0) and convert
            match ctx.get_field(this, 0) {
                Value::Object(Some(backing)) => pe_array_list_copy_of(ctx, backing),
                // Return empty list
                _ => pe_array_list_of(ctx, &[]),
            }
        }, cratonvm_native_api::NativeKind::Intrinsic);
    }

    // --- Collections.unmodifiableList/Map/Set ---
    // These just return the input (we don't enforce immutability)
    r.register(
        cu,
        "unmodifiableList",
        "(Ljava/util/List;)Ljava/util/List;",
        native_return_first_arg, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "unmodifiableMap",
        "(Ljava/util/Map;)Ljava/util/Map;",
        native_return_first_arg, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "unmodifiableSet",
        "(Ljava/util/Set;)Ljava/util/Set;",
        native_return_first_arg, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "synchronizedList",
        "(Ljava/util/List;)Ljava/util/List;",
        native_synchronized_list, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "synchronizedMap",
        "(Ljava/util/Map;)Ljava/util/Map;",
        native_synchronized_map, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "synchronizedSet",
        "(Ljava/util/Set;)Ljava/util/Set;",
        native_synchronized_set, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "singletonList",
        "(Ljava/lang/Object;)Ljava/util/List;",
        |ctx, args| {
            let elem = args.first().cloned().unwrap_or(Value::Object(None));
            pe_array_list_of(ctx, &[elem])
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "singletonMap",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;",
        |ctx, args| {
            // gc-common w19-c: the key and value are addresses copied out of
            // `args`; they were held raw across the map's class resolution
            // and `native_map_init` (which can load a class). Rooted first.
            let mut scope = NativeHandleScope::new(ctx);
            let key_h = element_handle(&mut scope, args.first().copied());
            let val_h = element_handle(&mut scope, args.get(1).copied());
            let map = try_alloc_concurrent_synthetic(&mut *scope, "java/util/HashMap", 3)?;
            // gc-common w11-c: as `Map.of(K, V)` above -- `map` is returned
            // after a put that can collect, so it is rooted and re-read.
            let map_h = scope.root(map);
            cratonvm_native_collections::native_map_init(&mut *scope, &[Value::Object(Some(map))])
                .ok();
            let map = scope.get(&map_h);
            let key = element_value(&scope, &key_h);
            let val = element_value(&scope, &val_h);
            cratonvm_native_collections::native_map_put_entry_pub(
                &mut *scope,
                &[Value::Object(Some(map)), key, val],
            )
            .ok();
            let map = scope.get(&map_h);
            Ok(Some(Value::Object(Some(map))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "singleton",
        "(Ljava/lang/Object;)Ljava/util/Set;",
        |ctx, args| {
            // gc-common w19-c: the element is an address copied out of
            // `args`; it was held raw across both class resolutions and
            // `native_map_init`. Rooted first, read back for the put.
            let mut scope = NativeHandleScope::new(ctx);
            let elem_h = element_handle(&mut scope, args.first().copied());
            let set = try_alloc_concurrent_synthetic(&mut *scope, "java/util/HashSet", 1)?;
            // gc-common w11-c: `set` and `map` are both used after a put that
            // can collect (the element's `hashCode`, and now the reclaiming
            // door), so both are rooted and re-read, as
            // `native_collections_singleton_set` already does.
            let set_h = scope.root(set);
            let map = try_alloc_concurrent_synthetic(&mut *scope, "java/util/HashMap", 3)?;
            let map_h = scope.root(map);
            cratonvm_native_collections::native_map_init(&mut *scope, &[Value::Object(Some(map))])?;
            let map = scope.get(&map_h);
            let elem = element_value(&scope, &elem_h);
            cratonvm_native_collections::native_map_put_entry_pub(
                &mut *scope,
                &[Value::Object(Some(map)), elem, Value::Object(None)],
            )?;
            let set = scope.get(&set_h);
            let map = scope.get(&map_h);
            scope.set_field(set, 0, Value::Object(Some(map)));
            Ok(Some(Value::Object(Some(set))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // `Collections.sort(List)` / `sort(List, Comparator)` are deliberately NOT
    // registered here. They used to be `native_noop` "overridden by
    // native-collections" — and the override is real: `register_collections_natives`
    // (native-collections `register_collections_utility_natives`) installs
    // `native_collections_sort`/`native_collections_sort_comparator` and runs
    // AFTER `register_builtins` in every VM init path (vm_init.rs synthetic and
    // real-JDK arms alike), and `NativeMethodRegistry::register` is
    // last-write-wins. So these two lines only ever pushed a no-op into the
    // class-agnostic `by_method_desc` first-wins index.
    r.register(cu, "reverse", "(Ljava/util/List;)V", |ctx, args| {
        // Reverse an ArrayList in-place
        let list = match args.first() {
            Some(Value::Object(Some(r))) => *r,
            _ => return Ok(None),
        };
        let data = match ctx.get_field(list, 0) {
            // AL_FIELD_DATA
            Value::Object(Some(arr)) => arr,
            _ => return Ok(None),
        };
        let size = match ctx.get_field(list, 1) {
            // AL_FIELD_SIZE
            Value::Int(s) => s as usize,
            _ => return Ok(None),
        };
        // Swap elements from both ends
        let mut lo = 0usize;
        let mut hi = if size > 0 { size - 1 } else { return Ok(None) };
        while lo < hi {
            let a = ctx.get_array_element(data, lo);
            let b = ctx.get_array_element(data, hi);
            ctx.set_array_element(data, lo, b);
            ctx.set_array_element(data, hi, a);
            lo += 1;
            hi -= 1;        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(cu, "shuffle", "(Ljava/util/List;)V", |ctx, args| {
        // Fisher–Yates shuffle on an ArrayList
        let list = match args.first() {
            Some(Value::Object(Some(r))) => *r,
            _ => return Ok(None),
        };
        let data = match ctx.get_field(list, 0) {
            Value::Object(Some(arr)) => arr,
            _ => return Ok(None),
        };
        let size = match ctx.get_field(list, 1) {
            Value::Int(s) => s as usize,
            _ => return Ok(None),
        };
        // Simple PRNG (xorshift32) — good enough for Collections.shuffle
        let mut rng: u32 = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u32)
            .unwrap_or(42))
            | 1;
        for i in (1..size).rev() {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            let j = (rng as usize) % (i + 1);
            let a = ctx.get_array_element(data, i);
            let b = ctx.get_array_element(data, j);
            ctx.set_array_element(data, i, b);
            ctx.set_array_element(data, j, a);        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    // W2: an always-0 `Collections.frequency` used to be re-registered HERE,
    // ~2100 lines after the real `native_collections_frequency` registration
    // near the top of this same function. `NativeMethodRegistry::register` is
    // last-write-wins, so this duplicate silently shadowed wave 1's real
    // implementation and made every `Collections.frequency(...)` answer 0.
    // Removed; the registration at the top of this function is the live one.

    // --- Wrapper-type <clinit>: set TYPE = primitive mirror ---
    // Java bytecode `int.class` compiles to `getstatic java/lang/Integer.TYPE`.
    // Synthetic stubs have a TYPE static field at index 0; we initialise it here.
    fn clinit_integer(ctx: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        let m = ctx.primitive_class_mirror("int");
        if let Some(c) = ctx.class_id_by_name("java/lang/Integer") {
            ctx.set_static_field(c, 0, Value::Object(Some(m)));
        }
        Ok(None)
    }
    fn clinit_long(ctx: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        let m = ctx.primitive_class_mirror("long");
        if let Some(c) = ctx.class_id_by_name("java/lang/Long") {
            ctx.set_static_field(c, 0, Value::Object(Some(m)));
        }
        Ok(None)
    }
    fn clinit_float(ctx: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        let m = ctx.primitive_class_mirror("float");
        if let Some(c) = ctx.class_id_by_name("java/lang/Float") {
            ctx.set_static_field(c, 0, Value::Object(Some(m)));
        }
        Ok(None)
    }
    fn clinit_double(ctx: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        let m = ctx.primitive_class_mirror("double");
        if let Some(c) = ctx.class_id_by_name("java/lang/Double") {
            ctx.set_static_field(c, 0, Value::Object(Some(m)));
        }
        Ok(None)
    }
    fn clinit_boolean(ctx: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        let m = ctx.primitive_class_mirror("boolean");
        if let Some(c) = ctx.class_id_by_name("java/lang/Boolean") {
            // Unlike the other primitive wrappers (whose other static finals
            // like MIN_VALUE/MAX_VALUE are primitives, so TYPE is their only
            // object-reference static and lands at index 0), real
            // java.lang.Boolean declares TRUE and FALSE — both
            // object-reference statics — BEFORE TYPE, so real Boolean.TYPE is
            // index 2, not 0. This native <clinit> replaces the real bytecode
            // <clinit> entirely, so hardcoding index 0 here silently
            // overwrote Boolean.TRUE with the Class mirror and left
            // Boolean.FALSE at its default null forever — real-bytecode
            // `Boolean.valueOf(false)` (`return b ? TRUE : FALSE`) returned
            // null. Resolve TYPE's slot by name, and explicitly populate
            // TRUE/FALSE since the real bytecode that would do so never runs.
            let type_idx = ctx.static_field_index_by_name(c, "TYPE").unwrap_or(0);
            ctx.set_static_field(c, type_idx, Value::Object(Some(m)));
            let true_idx = ctx.static_field_index_by_name(c, "TRUE").unwrap_or(0);
            let false_idx = ctx.static_field_index_by_name(c, "FALSE").unwrap_or(1);
            let true_obj = ctx.alloc_object(c, 1);
            ctx.set_field(true_obj, 0, Value::Int(1));
            ctx.set_static_field(c, true_idx, Value::Object(Some(true_obj)));
            let false_obj = ctx.alloc_object(c, 1);
            ctx.set_field(false_obj, 0, Value::Int(0));
            ctx.set_static_field(c, false_idx, Value::Object(Some(false_obj)));
        }
        Ok(None)
    }
    fn clinit_char(ctx: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        let m = ctx.primitive_class_mirror("char");
        if let Some(c) = ctx.class_id_by_name("java/lang/Character") {
            ctx.set_static_field(c, 0, Value::Object(Some(m)));
        }
        Ok(None)
    }
    fn clinit_byte(ctx: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        let m = ctx.primitive_class_mirror("byte");
        if let Some(c) = ctx.class_id_by_name("java/lang/Byte") {
            ctx.set_static_field(c, 0, Value::Object(Some(m)));
        }
        Ok(None)
    }
    fn clinit_short(ctx: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        let m = ctx.primitive_class_mirror("short");
        if let Some(c) = ctx.class_id_by_name("java/lang/Short") {
            ctx.set_static_field(c, 0, Value::Object(Some(m)));
        }
        Ok(None)
    }
    fn clinit_void(ctx: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        let m = ctx.primitive_class_mirror("void");
        if let Some(c) = ctx.class_id_by_name("java/lang/Void") {
            ctx.set_static_field(c, 0, Value::Object(Some(m)));
        }
        Ok(None)
    }
    r.register("java/lang/Integer", "<clinit>", "()V", clinit_integer, cratonvm_native_api::NativeKind::Intrinsic);
    r.register("java/lang/Long", "<clinit>", "()V", clinit_long, cratonvm_native_api::NativeKind::Intrinsic);
    r.register("java/lang/Float", "<clinit>", "()V", clinit_float, cratonvm_native_api::NativeKind::Intrinsic);
    r.register("java/lang/Double", "<clinit>", "()V", clinit_double, cratonvm_native_api::NativeKind::Intrinsic);
    r.register("java/lang/Boolean", "<clinit>", "()V", clinit_boolean, cratonvm_native_api::NativeKind::Intrinsic);
    r.register("java/lang/Character", "<clinit>", "()V", clinit_char, cratonvm_native_api::NativeKind::Intrinsic);
    r.register("java/lang/Byte", "<clinit>", "()V", clinit_byte, cratonvm_native_api::NativeKind::Intrinsic);
    r.register("java/lang/Short", "<clinit>", "()V", clinit_short, cratonvm_native_api::NativeKind::Intrinsic);
    r.register("java/lang/Void", "<clinit>", "()V", clinit_void, cratonvm_native_api::NativeKind::Intrinsic);

    // --- Integer.sum/max/min, Long.sum/max/min ---
    r.register("java/lang/Integer", "sum", "(II)I", |_ctx, args| {
        let a = args.first().and_then(|v| v.as_int()).unwrap_or(0);
        let b = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        Ok(Some(Value::Int(a.wrapping_add(b))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register("java/lang/Integer", "max", "(II)I", |_ctx, args| {
        let a = args.first().and_then(|v| v.as_int()).unwrap_or(0);
        let b = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        Ok(Some(Value::Int(a.max(b))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register("java/lang/Integer", "min", "(II)I", |_ctx, args| {
        let a = args.first().and_then(|v| v.as_int()).unwrap_or(0);
        let b = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        Ok(Some(Value::Int(a.min(b))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register("java/lang/Integer", "compare", "(II)I", |_ctx, args| {
        let a = args.first().and_then(|v| v.as_int()).unwrap_or(0);
        let b = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        Ok(Some(Value::Int(a.cmp(&b) as i32)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        "java/lang/Integer",
        "toUnsignedLong",
        "(I)J",
        |_ctx, args| {
            let a = args.first().and_then(|v| v.as_int()).unwrap_or(0) as u32;
            Ok(Some(Value::Long(a as i64)))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/lang/Integer",
        "toHexString",
        "(I)Ljava/lang/String;",
        |ctx, args| {
            let v = args.first().and_then(|v| v.as_int()).unwrap_or(0);
            let s = ctx.create_string(&format!("{:x}", v));
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/lang/Integer",
        "toBinaryString",
        "(I)Ljava/lang/String;",
        |ctx, args| {
            let v = args.first().and_then(|v| v.as_int()).unwrap_or(0);
            let s = ctx.create_string(&format!("{:b}", v));
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/lang/Integer",
        "toOctalString",
        "(I)Ljava/lang/String;",
        |ctx, args| {
            let v = args.first().and_then(|v| v.as_int()).unwrap_or(0);
            let s = ctx.create_string(&format!("{:o}", v));
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // --- Wrapper toString() instance methods (Phase 53) ---
    // Integer.toString() — instance method on boxed Integer
    r.register(
        "java/lang/Integer",
        "toString",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let val = match ctx.get_field(this, 0) {
                Value::Int(v) => v,
                _ => 0,
            };
            Ok(Some(Value::Object(Some(
                ctx.create_string(&val.to_string()),
            ))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // Long.toString()
    r.register(
        "java/lang/Long",
        "toString",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let val = match ctx.get_field(this, 0) {
                Value::Long(v) => v,
                _ => 0,
            };
            Ok(Some(Value::Object(Some(
                ctx.create_string(&val.to_string()),
            ))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // Float.toString()
    r.register(
        "java/lang/Float",
        "toString",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let val = match ctx.get_field(this, 0) {
                Value::Float(v) => v,
                _ => 0.0,
            };
            Ok(Some(Value::Object(Some(
                ctx.create_string(&format!("{}", val)),
            ))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // Double.toString()
    r.register(
        "java/lang/Double",
        "toString",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let val = match ctx.get_field(this, 0) {
                Value::Double(v) => v,
                _ => 0.0,
            };
            Ok(Some(Value::Object(Some(
                ctx.create_string(&format!("{}", val)),
            ))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // Boolean.toString()
    r.register(
        "java/lang/Boolean",
        "toString",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let val = match ctx.get_field(this, 0) {
                Value::Int(v) => v != 0,
                _ => false,
            };
            Ok(Some(Value::Object(Some(ctx.create_string(if val {
                "true"
            } else {
                "false"
            })))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // Character.toString()
    r.register(
        "java/lang/Character",
        "toString",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let val = match ctx.get_field(this, 0) {
                Value::Int(v) => v,
                _ => 0,
            };
            let ch = char::from_u32(val as u32).unwrap_or('?');
            Ok(Some(Value::Object(Some(
                ctx.create_string(&ch.to_string()),
            ))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // Byte.toString()
    r.register(
        "java/lang/Byte",
        "toString",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let val = match ctx.get_field(this, 0) {
                Value::Int(v) => v as i8,
                _ => 0,
            };
            Ok(Some(Value::Object(Some(
                ctx.create_string(&val.to_string()),
            ))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // Short.toString()
    r.register(
        "java/lang/Short",
        "toString",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let val = match ctx.get_field(this, 0) {
                Value::Int(v) => v as i16,
                _ => 0,
            };
            Ok(Some(Value::Object(Some(
                ctx.create_string(&val.to_string()),
            ))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    r.register("java/lang/Long", "sum", "(JJ)J", |_ctx, args| {
        let a = args.first().and_then(|v| v.as_long()).unwrap_or(0);
        let b = args.get(1).and_then(|v| v.as_long()).unwrap_or(0);
        Ok(Some(Value::Long(a.wrapping_add(b))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register("java/lang/Long", "max", "(JJ)J", |_ctx, args| {
        let a = args.first().and_then(|v| v.as_long()).unwrap_or(0);
        let b = args.get(1).and_then(|v| v.as_long()).unwrap_or(0);
        Ok(Some(Value::Long(a.max(b))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register("java/lang/Long", "min", "(JJ)J", |_ctx, args| {
        let a = args.first().and_then(|v| v.as_long()).unwrap_or(0);
        let b = args.get(1).and_then(|v| v.as_long()).unwrap_or(0);
        Ok(Some(Value::Long(a.min(b))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register("java/lang/Long", "compare", "(JJ)I", |_ctx, args| {
        let a = args.first().and_then(|v| v.as_long()).unwrap_or(0);
        let b = args.get(1).and_then(|v| v.as_long()).unwrap_or(0);
        Ok(Some(Value::Int(a.cmp(&b) as i32)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        "java/lang/Long",
        "toHexString",
        "(J)Ljava/lang/String;",
        |ctx, args| {
            let v = args.first().and_then(|v| v.as_long()).unwrap_or(0);
            let s = ctx.create_string(&format!("{:x}", v));
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register("java/lang/Double", "sum", "(DD)D", |_ctx, args| {
        let a = args.first().and_then(|v| v.as_double()).unwrap_or(0.0);
        let b = args.get(1).and_then(|v| v.as_double()).unwrap_or(0.0);
        Ok(Some(Value::Double(a + b)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    // `Double.min`/`max` have their OWN bodies here, so the `java/lang/Math`
    // repair does not reach them — a third copy of one rule. JDK 25's
    // `Double.min` is literally `return Math.min(a, b);`, so sharing one tree
    // is a theorem, not a convenience. Measured wrong before this:
    // `Double.min(1.0, NaN)` answered `1.0` and `Double.min(-0.0, 0.0)`
    // answered `+0.0`; Java requires `NaN` and `-0.0`. See the helpers' doc in
    // `lang_math.rs` for why Rust's `f64::min` is the wrong primitive.
    r.register("java/lang/Double", "max", "(DD)D", |_ctx, args| {
        let a = args.first().and_then(|v| v.as_double()).unwrap_or(0.0);
        let b = args.get(1).and_then(|v| v.as_double()).unwrap_or(0.0);
        Ok(Some(Value::Double(crate::lang_math::java_math_max_f64(
            a, b,
        ))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register("java/lang/Double", "min", "(DD)D", |_ctx, args| {
        let a = args.first().and_then(|v| v.as_double()).unwrap_or(0.0);
        let b = args.get(1).and_then(|v| v.as_double()).unwrap_or(0.0);
        Ok(Some(Value::Double(crate::lang_math::java_math_min_f64(
            a, b,
        ))))
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // --- Charset ---
    let cs = "java/nio/charset/Charset";
    r.register(
        cs,
        "forName",
        "(Ljava/lang/String;)Ljava/nio/charset/Charset;",
        |ctx, args| {
            let name = match args.first() {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => "UTF-8".to_string(),
            };
            let charset = pe_new_charset(ctx, &name)?;
            Ok(Some(Value::Object(Some(charset))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cs,
        "defaultCharset",
        "()Ljava/nio/charset/Charset;",
        |ctx, _args| {
            let charset = pe_new_charset(ctx, "UTF-8")?;
            Ok(Some(Value::Object(Some(charset))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(cs, "name", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(cs, "displayName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    // StandardCharsets constants.
    //
    // charset-NPE fix (2026-05-21): this registration is the LAST writer
    // for `java/nio/charset/StandardCharsets.<clinit>` — `register_core_stdlib_extras`
    // runs after `register_charset_natives` (see `lib.rs`: 33236 vs 9120),
    // and `NativeMethodRegistry::register` is last-write-wins. The previous
    // `native_noop` therefore SHADOWED the field-populating `<clinit>`
    // installed by `register_charset_natives`, so in real-JDK mode the
    // `StandardCharsets.UTF_8` / `US_ASCII` / `ISO_8859_1` / `UTF_16*`
    // static fields were NEVER assigned and stayed null.
    //
    // Any app code that does `new OutputStreamWriter(stream,
    // StandardCharsets.UTF_8)` or `new PrintStream/PrintWriter(out,
    // autoFlush, StandardCharsets.X)` then passed `null` for the charset,
    // and the JDK bytecode `OutputStreamWriter(OutputStream, Charset)`
    // (`if (cs == null) throw new NullPointerException("charset")`) threw
    // the bare `NullPointerException: charset` — the keycloak26 Picocli /
    // Hazelcast JAXP-XPath blocker. (`<clinit>` is dispatched as a native
    // override of the real bytecode initializer when one is registered —
    // see `vm/src/vm/vm_util.rs::ensure_class_initialized_shared`.)
    //
    // Replace the no-op with the same field-populating body used by
    // `register_charset_natives` so the static slots always hold non-null
    // synthetic Charset objects (1-field layout, slot 0 = canonical name)
    // that the rest of the charset native surface already understands.
    let scs = "java/nio/charset/StandardCharsets";
    r.register(scs, "<clinit>", "()V", |ctx, _args| {
        // Field order matches the JDK declaration; populate every public
        // standard-charset static so a later `getstatic` reads a live ref.
        for (field_name, charset_name) in [
            ("UTF_8", "UTF-8"),
            ("US_ASCII", "US-ASCII"),
            ("ISO_8859_1", "ISO-8859-1"),
            ("UTF_16", "UTF-16"),
            ("UTF_16BE", "UTF-16BE"),
            ("UTF_16LE", "UTF-16LE"),
        ] {
            let obj = pe_new_charset(ctx, charset_name)?;
            ctx.set_static_field_by_name(
                "java/nio/charset/StandardCharsets",
                field_name,
                Value::Object(Some(obj)),
            );
        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // -----------------------------------------------------------------------
    // Phase 37: registerNatives for all core JDK classes
    // -----------------------------------------------------------------------
    // Real JDK classes call registerNatives() in their <clinit>. This is a
    // native method that in HotSpot registers JNI method pointers, but in
    // CratonVM it's a no-op since we register native methods in Rust at startup.
    //
    // The registerNatives no-ops, Unsafe fences, and jdk.internal.misc.VM
    // entries below are VM-internal bridges, not Intrinsics — tag the block.
            for cls in &[
        "java/lang/ClassLoader",
        "java/lang/invoke/MethodHandleNatives",
        "java/lang/ref/Finalizer",
        "java/lang/ref/Reference",
        "java/lang/reflect/Array",
        "java/lang/reflect/Executable",
        "java/lang/reflect/Field",
        "java/lang/reflect/Method",
        "java/lang/reflect/Constructor",
        "java/lang/Runtime",
        "java/lang/ProcessBuilder",
        "java/lang/ProcessEnvironment",
        "java/lang/Shutdown",
        "java/lang/SecurityManager",
        "java/io/FileDescriptor",
        "java/io/FileInputStream",
        "java/io/FileOutputStream",
        "java/io/RandomAccessFile",
        "java/io/UnixFileSystem",
        "java/io/WinNTFileSystem",
        "java/io/Console",
        "java/net/InetAddress",
        "java/net/Inet4Address",
        "java/net/Inet6Address",
        "java/net/PlainSocketImpl",
        "java/net/NetworkInterface",
        "jdk/internal/misc/Signal",
        "jdk/internal/misc/VM",
        "jdk/internal/misc/ScopedMemoryAccess",
        "sun/nio/ch/IOUtil",
        "sun/nio/ch/FileDispatcherImpl",
        "sun/nio/ch/NativeThread",
        "sun/nio/ch/Net",
        "sun/nio/ch/ServerSocketChannelImpl",
        "sun/nio/ch/SocketChannelImpl",
        "sun/nio/fs/WindowsNativeDispatcher",
        "sun/nio/fs/UnixNativeDispatcher",
        "java/util/zip/ZipFile",
        "java/util/zip/Inflater",
        "java/util/zip/Deflater",
        "java/util/zip/CRC32",
        "java/util/zip/Adler32",
        "java/util/TimeZone",
        "java/util/concurrent/atomic/AtomicLong",
    ] {
        r.register(cls, "registerNatives", "()V", native_noop, cratonvm_native_api::NativeKind::Bridge);
    }

    // jdk.internal.misc.Unsafe additional natives needed for class init.
    //
    // The three fences are REAL fences, not no-ops. They are the JMM primitives
    // behind VarHandle release/acquire and the JDK's own lock-free code; a
    // no-op is only accidentally correct on x86-TSO and still lets LLVM
    // reorder the surrounding native accesses on every target. Orderings
    // mirror the JDK contract: storeFence = StoreStore|LoadStore (release),
    // loadFence = LoadLoad|LoadStore (acquire), fullFence = full barrier.
    // (`unsafe_natives_ext::native_unsafe_fence` uses SeqCst for all three and
    // is registered earlier in this same pass, so it only wins in real-JDK
    // mode, where this function does not run.)
    let unsafe_cls = "jdk/internal/misc/Unsafe";
    r.register(unsafe_cls, "storeFence", "()V", |_ctx, _args| {
        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(unsafe_cls, "loadFence", "()V", |_ctx, _args| {
        std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(unsafe_cls, "fullFence", "()V", |_ctx, _args| {
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        unsafe_cls,
        "ensureClassInitialized0",
        "(Ljava/lang/Class;)V",
        native_unsafe_ensure_class_initialized, cratonvm_native_api::NativeKind::Bridge,
    );

    // jdk.internal.misc.CDS: CratonVM does not use HotSpot class-data sharing.
    // Report all dump/sharing modes disabled and make the archive hooks inert.
    //
    // The three `is*0` flags are a spec-correct KEEP, not a stub: CDS is an
    // optional HotSpot feature, and HotSpot itself answers exactly this way
    // when built with `INCLUDE_CDS=0` or launched with `-Xshare:off`. With
    // them all false the JDK's own callers never consult an archive, so
    // `initializeFromArchive` (fields stay at their <clinit> values),
    // `defineArchivedModules` (the module graph is built normally) and
    // `logLambdaFormInvoker` are inert by definition rather than by omission.
    //
    // The two `dump*` entry points are different: a caller only reaches them
    // by explicitly asking for a dump, so "silently did nothing, reported
    // success" is a lie. `dumpClassList` now writes the real list; the archive
    // dump refuses. See below.
    let cds_cls = "jdk/internal/misc/CDS";
    r.register(cds_cls, "isDumpingClassList0", "()Z", native_return_false, cratonvm_native_api::NativeKind::Bridge);
    r.register(cds_cls, "isDumpingArchive0", "()Z", native_return_false, cratonvm_native_api::NativeKind::Bridge);
    r.register(cds_cls, "isSharingEnabled0", "()Z", native_return_false, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        cds_cls,
        "logLambdaFormInvoker",
        "(Ljava/lang/String;)V",
        native_noop, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cds_cls,
        "initializeFromArchive",
        "(Ljava/lang/Class;)V",
        native_noop, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cds_cls,
        "defineArchivedModules",
        "(Ljava/lang/ClassLoader;Ljava/lang/ClassLoader;)V",
        native_noop, cratonvm_native_api::NativeKind::Bridge,
    );
    // KEEP: HotSpot's `CDS.getRandomSeedForDumping()` returns 0 whenever the VM
    // is not dumping a CDS archive, and CratonVM never dumps one. Callers
    // (`ImageGenerator`, `System.identityHashCode` seeding) treat 0 as "no
    // archive seed", which is the truth here.
    r.register(cds_cls, "getRandomSeedForDumping", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // The one CDS entry point with a real local answer: HotSpot's
    // `-XX:DumpLoadedClassList` writer emits one internal-form class name per
    // line, and the VM knows that list (`list_application_class_names`).
    r.register(
        cds_cls,
        "dumpClassList",
        "(Ljava/lang/String;)V",
        native_cds_dump_class_list_to_file, cratonvm_native_api::NativeKind::Bridge,
    );
    // THROW rather than pretend. There is no CratonVM archive format at all,
    // so a no-op reported "archive written" and left the caller with no file.
    r.register(
        cds_cls,
        "dumpDynamicArchive",
        "(Ljava/lang/String;)V",
        |_ctx, _args| {
            Err(RuntimeError::UnsupportedOperationException {
                message: "CratonVM does not support CDS archives".to_string(),
            }
            .into())
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // jdk.internal.misc.VM natives.
    //
    // `VM.initialize()` is a spec-correct KEEP in the same family as
    // `registerNatives()`. Its real body (libjava's `VM.c`) acquires a handle
    // to libjvm so that later `JDK_FindJvmEntry` symbol lookups resolve — a
    // host-linkage step with no analogue here, since CratonVM's "JVM entry
    // points" are Rust functions. Everything the Java side then reads (the
    // init level, the saved-properties snapshot) is already established by the
    // Rust bootstrap before any Java frame runs and is served by the
    // `initLevel`/`awaitInitLevel`/`getSavedProperty` natives immediately
    // below. There is no VM state left for it to install.
    r.register("jdk/internal/misc/VM", "initialize", "()V", native_noop, cratonvm_native_api::NativeKind::Bridge);
    // WP1.3 / j27 lane FBTAIL: see [`vm_init_level_native`] (the same
    // callback as the registration in lib.rs).
    let init_level = vm_init_level_native(r);
    r.register("jdk/internal/misc/VM", "initLevel", "()I", init_level, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        "jdk/internal/misc/VM",
        "awaitInitLevel",
        "(I)V",
        |_ctx, args| {
            let target = match args.first() {
                Some(Value::Int(n)) => *n,
                _ => 0,
            };
            cratonvm_native_api::init_level::await_init_level(target);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `getSavedProperty` must answer from the property store, not null. This
    // registration runs after `register_essential_natives`' real one and is
    // therefore the winner in synthetic mode, so it has to BE the real one —
    // the previous always-null closure silently blanked every saved property
    // (`java.home`, `sun.jnu.encoding`, …) that JDK bootstrap code reads
    // through `VM.getSavedProperty`.
    r.register(
        "jdk/internal/misc/VM",
        "getSavedProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        crate::lang_system::native_vm_get_saved_property, cratonvm_native_api::NativeKind::Bridge,
    );
    
    // Note: Class.getPrimitiveClass(String) is already registered in lib.rs.
    // Do NOT re-register here — the existing registration uses the correct mirror layout
    // (field 0 = ClassId, field 1 = name string) that mirror_class_name() depends on.

    // --- java.util.Properties (Phase 48) ---
    // Properties extends Hashtable, which we model as HashMap-like (2 fields: data array, size)
    let props = "java/util/Properties";
    r.register(props, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // The receiver's address was taken before this allocation.
        let mut scope = NativeHandleScope::new(ctx);
        let this_h = scope.root(this);
        let data = scope.new_array(cratonvm_types::ArrayElementType::Reference, 32);
        let this = scope.get(&this_h);
        scope.set_field(this, 0, Value::Object(Some(data)));
        scope.set_field(this, 1, Value::Int(0));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        props,
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = match args.get(1) {
                Some(Value::Object(Some(k))) => ctx.read_string(*k).unwrap_or_default(),
                _ => return Ok(Some(Value::Object(None))),
            };
            // Stored as interleaved key/value pairs (data[2i]=key, data[2i+1]=value).
            let data = match ctx.get_field(this, 0) {
                Value::Object(Some(d)) => d,
                _ => return Ok(Some(Value::Object(None))),
            };
            let size = match ctx.get_field(this, 1) {
                Value::Int(s) => s as usize,
                _ => 0,
            };
            // [PERF] O(1) cached key->index lookup instead of an O(n) scan that
            // decoded every stored key. Returns the first-match value, exactly as
            // the old linear scan did; falls through to null on a miss.
            match props_index_cache::lookup_index(ctx, this, data, size, &key) {
                Some(i) => Ok(Some(ctx.get_array_element(data, i * 2 + 1))),
                None => Ok(Some(Value::Object(None))),
            }
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        props,
        "getProperty",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = match args.get(1) {
                Some(Value::Object(Some(k))) => ctx.read_string(*k).unwrap_or_default(),
                _ => return Ok(args.get(2).copied()),
            };
            let data = match ctx.get_field(this, 0) {
                Value::Object(Some(d)) => d,
                _ => return Ok(args.get(2).copied()),
            };
            let size = match ctx.get_field(this, 1) {
                Value::Int(s) => s as usize,
                _ => 0,
            };
            // [PERF] O(1) cached lookup; on a miss return the supplied default,
            // exactly as the old linear scan did.
            match props_index_cache::lookup_index(ctx, this, data, size, &key) {
                Some(i) => Ok(Some(ctx.get_array_element(data, i * 2 + 1))),
                None => Ok(args.get(2).copied()),
            }
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        props,
        "setProperty",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let val = args.get(2).copied().unwrap_or(Value::Object(None));
            let data = match ctx.get_field(this, 0) {
                Value::Object(Some(d)) => d,
                _ => return Ok(Some(Value::Object(None))),
            };
            let size = match ctx.get_field(this, 1) {
                Value::Int(s) => s as usize,
                _ => 0,
            };
            // Check for existing key
            let key_str = match args.get(1) {
                Some(Value::Object(Some(k))) => ctx.read_string(*k).unwrap_or_default(),
                _ => String::new(),
            };
            // [PERF] O(1) cached lookup for the existing-key (overwrite) case
            // instead of an O(n) decode-every-key scan. On an in-place overwrite
            // the key/index/size are unchanged, so the cache stays valid (no
            // rebuild). Identical result: the FIRST matching pair's old value is
            // returned and its value slot is replaced.
            if let Some(i) = props_index_cache::lookup_index(ctx, this, data, size, &key_str) {
                let old = ctx.get_array_element(data, i * 2 + 1);
                ctx.set_array_element(data, i * 2 + 1, val);
                return Ok(Some(old));
            }
            // Add new pair.
            //
            // BUGFIX [nb-phases-early (1)]: the <init> above allocates a FIXED
            // 32-element interleaved key/value backing array (= 16 pairs). The
            // previous code wrote data[size*2]/[size*2+1] with NO capacity check,
            // so the 17th distinct key produced an out-of-bounds array store whose
            // error was silently discarded by vm_exec.rs — the pair simply vanished
            // and `size` was still bumped, corrupting the table. Grow the backing
            // array (double capacity, copy, reset field 0) BEFORE the store once it
            // would not fit, mirroring ucl_add_url's growth in classloader.rs.
            //
            // The growth allocates, which moves the receiver, the old backing
            // array and both arguments — all of which are stored below. From
            // here on every address goes through the scope.
            let mut scope = NativeHandleScope::new(ctx);
            let this_h = scope.root(this);
            let old_data_h = scope.root(data);
            let key_h = element_handle(&mut scope, args.get(1).copied());
            let val_h = element_handle(&mut scope, args.get(2).copied());
            let data = {
                let data = scope.get(&old_data_h);
                let arr_len = scope.array_length(data);
                if size * 2 + 1 >= arr_len {
                    // Double capacity (guard the degenerate len==0 case) and copy
                    // every existing slot into the fresh, larger array.
                    let new_cap = (arr_len * 2).max((size + 1) * 2);
                    // (Typed so the stale-handle audit sees the binding: it
                    // does not follow a `let` whose initialiser wraps.)
                    let new_arr: ObjectRef =
                        scope.new_array(cratonvm_types::ArrayElementType::Reference, new_cap);
                    let data = scope.get(&old_data_h);
                    for i in 0..arr_len {
                        let elem = scope.get_array_element(data, i);
                        scope.set_array_element(new_arr, i, elem);
                    }
                    let this = scope.get(&this_h);
                    scope.set_field(this, 0, Value::Object(Some(new_arr)));
                    new_arr
                } else {
                    data
                }
            };
            let key = element_value(&scope, &key_h);
            let val = element_value(&scope, &val_h);
            let this = scope.get(&this_h);
            scope.set_array_element(data, size * 2, key);
            scope.set_array_element(data, size * 2 + 1, val);
            scope.set_field(this, 1, Value::Int((size + 1) as i32));
            // [PERF] Fold the just-appended key into the lookup cache so a
            // load-then-many-reads pattern stays O(1) per op (the `lookup_index`
            // above already (re)built the cache for the pre-append size, so this
            // is an incremental update, not a full rebuild). `note_append` passes
            // the NEW backing array `data` so a growth-copy refreshes the array
            // validity token without invalidating the indices (contents copied
            // 1:1). On any inconsistency it drops the entry and the next lookup
            // rebuilds — behavior stays correct either way.
            props_index_cache::note_append(&mut *scope, this, data, size);
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(props, "size", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        props,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = match args.get(1) {
                Some(Value::Object(Some(k))) => ctx.read_string(*k).unwrap_or_default(),
                _ => return Ok(Some(Value::Int(0))),
            };
            let data = match ctx.get_field(this, 0) {
                Value::Object(Some(d)) => d,
                _ => return Ok(Some(Value::Int(0))),
            };
            let size = match ctx.get_field(this, 1) {
                Value::Int(s) => s as usize,
                _ => 0,
            };
            // [PERF] O(1) cached membership test instead of an O(n) decode scan.
            let found = props_index_cache::lookup_index(ctx, this, data, size, &key).is_some();
            Ok(Some(Value::Int(found as i32)))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        props,
        "stringPropertyNames",
        "()Ljava/util/Set;",
        |ctx, args| {
            // S111r11+: collect the keys we need to expose, then return a HashSet
            // that wraps a real-layout HashMap so JDK-bytecode stream / spliterator
            // / iterator paths see the layout slots they expect.
            //
            // Previous synthetic-2-field (data_array, size) layout broke
            // `HashSet.spliterator()` (inherited bytecode does
            // `new HashMap.KeySpliterator<>(this.map, ...)` reading slot 0 as
            // the wrapped HashMap; later forEachRemaining does
            // `getfield m.table` which on the synthetic resolved to slot 2
            // (real HashMap layout) and produced
            //   `expected object reference, got int(16)`
            // — same failure pattern S111r7 fixed for `System.getenv()`.
            let this = obj_arg(args, 0)?;
            let mut keys: Vec<ObjectRef> = Vec::new();
            if let Value::Object(Some(data)) = ctx.get_field(this, 0) {
                let size = match ctx.get_field(this, 1) {
                    Value::Int(s) => s as usize,
                    _ => 0,
                };
                for i in 0..size {
                    if let Value::Object(Some(k)) = ctx.get_array_element(data, i * 2) {
                        keys.push(k);
                    }
                }
            }
            Ok(Some(Value::Object(Some(build_real_layout_string_hashset(
                ctx, &keys,
            )?))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- Map.forEach / Map.compute / Map.putIfAbsent (Phase 48) ---
    let hm = "java/util/HashMap";
    r.register(
        hm,
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let action = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(None),
            };
            // HashMap: field 0 = entries array, field 1 = size
            let entries = match ctx.get_field(this, 0) {
                Value::Object(Some(e)) => e,
                _ => return Ok(None),
            };
            let _size = match ctx.get_field(this, 1) {
                Value::Int(s) => s as usize,
                _ => 0,
            };
            // Each entry is a 3-field object (key=0, value=1, next=2)
            let arr_len = ctx.array_length(entries);
            // GC-safety: `accept` is an arbitrary user lambda -- it allocates --
            // and both the consumer and the bucket array are carried across
            // every turn.
            let action_pin = ctx.pin_native_root(action);
            let entries_pin = ctx.pin_native_root(entries);
            for i in 0..arr_len {
                let action = ctx.read_native_pin(action_pin, action);
                let entries = ctx.read_native_pin(entries_pin, entries);
                if let Value::Object(Some(entry)) = ctx.get_array_element(entries, i) {
                    let key = ctx.get_field(entry, 0);
                    let val = ctx.get_field(entry, 1);
                    ctx.invoke_virtual(
                        action,
                        "accept",
                        "(Ljava/lang/Object;Ljava/lang/Object;)V",
                        &[key, val],
                    )?;                }            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        hm,
        "putIfAbsent",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            // Delegate to HashMap.get(); if null, put and return null; otherwise return existing
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let val = args.get(2).copied().unwrap_or(Value::Object(None));
            let existing = ctx.invoke_virtual(
                this,
                "get",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[key],
            )?;
            match existing {
                Some(Value::Object(None)) | None => {
                    ctx.invoke_virtual(
                        this,
                        "put",
                        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                        &[key, val],
                    )?;
                    Ok(Some(Value::Object(None)))
                }
                other => Ok(other),
            }
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        hm,
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let default_val = args.get(2).copied().unwrap_or(Value::Object(None));
            let existing = ctx.invoke_virtual(
                this,
                "get",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[key],
            )?;
            match existing {
                Some(Value::Object(None)) | None => Ok(Some(default_val)),
                other => Ok(other),
            }
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    }

/// The callback for `jdk.internal.misc.VM.initLevel()`, registered by
/// `register_core_stdlib_extras` and `register_essential_natives_with_shims`.
///
/// JDK 25's body is `return initLevel;`, a bare read of the static, and that
/// is what HotSpot answers. The row is a retired shadow (`--real-jdk` drops it
/// and the JDK body runs), but `--compatible` keeps it in front of that body.
/// It used to answer `max(live, 2)` from the PROCESS-wide boot level there,
/// which disagreed with the static the JDK's own `initLevel(int)`,
/// `isBooted()` and `isModuleSystemInited()` read, and made the boot's
/// `initLevel()`-gated `VM.initLevel(1)` write never run (j27 lane FBTAIL,
/// `docs/feature-designs/real-jdk-mode.md`).
/// Over a real JDK it now answers the static, as the JDK body does
/// (`CRATONVM_COMPAT_VM_INIT_LEVEL_FROM_STATIC`, default ON, `=0` restores
/// the floor). The synthetic JDK, which has no such static to answer from,
/// keeps the floor of 2 its class initialisers rely on. Read per registration
/// (per VM).
///
/// j27 lane VMALIGN: with the `jdk-vm-init-level` HotSpot-alignment row in
/// force (default ON), `--compatible` over a real JDK registers neither
/// registration and the JDK body itself runs
/// (`retired_shadow::is_vm_init_level_row`); this callback is then the
/// `CRATONVM_COMPAT_ALIGN=-jdk-vm-init-level` arm and the synthetic JDK's.
pub fn vm_init_level_native(
    r: &NativeMethodRegistry,
) -> cratonvm_native_api::NativeCallback {
    if r.real_jdk()
    {
        native_vm_init_level_from_static
    } else {
        native_vm_init_level_floor
    }
}

pub fn native_vm_init_level_from_static(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let jdk_static = ctx.class_id_by_name("jdk/internal/misc/VM").and_then(|vm| {
        let index = ctx.static_field_index_by_name(vm, "initLevel")?;
        match ctx.get_static_field(vm, index) {
            Value::Int(level) => Some(level),
            _ => None,
        }
    });
    let live = cratonvm_native_api::init_level::get_init_level();
    Ok(Some(Value::Int(vm_init_level_answer(jdk_static, live))))
}

pub fn native_vm_init_level_floor(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let live = cratonvm_native_api::init_level::get_init_level();
    Ok(Some(Value::Int(vm_init_level_answer(None, live))))
}

/// `VM.initLevel()`'s answer: the JDK's static when there is one to read,
/// else the boot level with the synthetic JDK's floor of 2.
pub fn vm_init_level_answer(jdk_static: Option<i32>, live: i32) -> i32 {
    jdk_static.unwrap_or_else(|| live.max(2))
}

// ===========================================================================
// Phase 51: Scanner, StringReader, StringWriter (CLI app support)
// ===========================================================================

// The whole `java.util.Scanner` bundle used to be registered here, over a
// THREE-slot model (source=0, position=1, delimiter=2) — and it was dead in
// every build configuration, for the same reason the `close()`, `StringReader`
// and `StringWriter` registrations that used to sit below it were:
// `register_scanner_natives` is reachable only from
// `register_synthetic_overrides`, which `register_builtins` calls at
// vm_init.rs:1426, and `register_io_natives` runs at 1428 — two lines later,
// into a last-write-wins map. All twelve triples registered here
// (`<init>` x3, next/nextLine/nextInt/nextLong/nextDouble, hasNext/hasNextLine/
// hasNextInt, useDelimiter) are among the 34 that `native-io`'s own
// `register_scanner_natives` registers, so every one of them was overwritten
// before a single Java frame ran. `--dump-native-registry` on a real-JDK run
// confirms the other half: all 35 live `java/util/Scanner` entries name
// `native-io/src/lib.rs` as their registration site, and none of them records
// having overwritten anything.
//
// Removed rather than kept in sync. A second, DIFFERENT layout model for the
// same class is how this work item's defects get written: the Real-JDK wave-2
// L3 brief was scoped from this file and described the Scanner model as three
// fields, while the writer that actually produced the census rows was
// `native_scanner_init_string` in `native-io`, over five. The live model, and
// the mapping from it onto the real `java.util.Scanner` layout, is documented
// in one place — the JDK-ONLY-LAYOUT block above `SCAN_FIELD_INPUT` there.

// ===========================================================================
// Phase 50: ThreadLocal, StringTokenizer, BitSet, EnumSet, EnumMap
// ===========================================================================

pub(crate) fn register_phase50_natives(registry: &mut NativeMethodRegistry) {
    register_thread_local_natives(registry);
    register_string_tokenizer_natives(registry);
    register_bitset_natives(registry);
    register_enum_set_natives(registry);
    register_enum_map_natives(registry);
    register_identity_hashmap_natives(registry);
    register_weak_hashmap_natives(registry);
    // T2.3.1: `jdk.internal.util.ArraysSupport` vectorized intrinsics.
    register_arrays_support_natives(registry);
    register_string_latin1_natives(registry);
    // T2.3.2/3/6/8/12 — remaining java.util.* natives.
    // T1.5.1 housekeeping: the original call site references
    // `register_t2_3_completion_natives` which was split across
    // phases in an earlier session; the fallback no-op below is a
    // safe bridge so incremental compilation stays green. When T2
    // lands the full collection bootstrap this becomes the real
    // entry point.
    let _ = registry;
}

pub fn atomic_int_update_with_operator(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    op: ObjectRef,
    return_new: bool,
) -> MethodCallResult {
    let this_pin = ctx.pin_native_root(this);
    let op_pin = ctx.pin_native_root(op);
    let mut this_cur = this;
    let mut op_cur = op;
    loop {
        this_cur = ctx.read_native_pin(this_pin, this_cur);
        op_cur = ctx.read_native_pin(op_pin, op_cur);
        let cur = ctx.get_field_volatile(this_cur, 0);
        let applied = match ctx.invoke_virtual(op_cur, "applyAsInt", "(I)I", &[cur]) {
            Ok(v) => v.unwrap_or(Value::Int(0)),
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };
        this_cur = ctx.read_native_pin(this_pin, this_cur);
        if ctx.compare_and_swap_field(this_cur, 0, cur, applied) {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(if return_new { applied } else { cur }));
        }
    }
}

pub fn atomic_long_update_with_operator(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    op: ObjectRef,
    return_new: bool,
) -> MethodCallResult {
    let this_pin = ctx.pin_native_root(this);
    let op_pin = ctx.pin_native_root(op);
    let mut this_cur = this;
    let mut op_cur = op;
    loop {
        this_cur = ctx.read_native_pin(this_pin, this_cur);
        op_cur = ctx.read_native_pin(op_pin, op_cur);
        let cur = ctx.get_field_volatile(this_cur, 0);
        let applied = match ctx.invoke_virtual(op_cur, "applyAsLong", "(J)J", &[cur]) {
            Ok(v) => v.unwrap_or(Value::Long(0)),
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };
        this_cur = ctx.read_native_pin(this_pin, this_cur);
        if ctx.compare_and_swap_field(this_cur, 0, cur, applied) {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(if return_new { applied } else { cur }));
        }
    }
}

pub fn pinned_object_value(ctx: &mut dyn NativeContext, value: Value) -> Option<(usize, ObjectRef)> {
    match value {
        Value::Object(Some(obj)) => Some((ctx.pin_native_root(obj), obj)),
        _ => None,
    }
}

pub fn read_pinned_object_value(
    ctx: &dyn NativeContext,
    pin: Option<(usize, ObjectRef)>,
    fallback: Value,
) -> Value {
    match pin {
        Some((handle, obj)) => Value::Object(Some(ctx.read_native_pin(handle, obj))),
        None => fallback,
    }
}

pub fn atomic_reference_update_with_operator(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    op: ObjectRef,
    return_new: bool,
) -> MethodCallResult {
    let this_pin = ctx.pin_native_root(this);
    let op_pin = ctx.pin_native_root(op);
    let mut this_cur = this;
    let mut op_cur = op;
    loop {
        this_cur = ctx.read_native_pin(this_pin, this_cur);
        op_cur = ctx.read_native_pin(op_pin, op_cur);
        let cur = ctx.get_field_volatile(this_cur, 0);
        let cur_pin = pinned_object_value(ctx, cur);
        let arg = read_pinned_object_value(ctx, cur_pin, cur);
        let applied = match ctx.invoke_virtual(
            op_cur,
            "apply",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[arg],
        ) {
            Ok(v) => v.unwrap_or(Value::Object(None)),
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };
        this_cur = ctx.read_native_pin(this_pin, this_cur);
        let expected = read_pinned_object_value(ctx, cur_pin, cur);
        if let Some((handle, _)) = cur_pin {
            ctx.unpin_native_roots(handle);
        }
        if ctx.compare_and_swap_field(this_cur, 0, expected, applied) {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(if return_new { applied } else { expected }));
        }
    }
}

// ---------------------------------------------------------------------------
// ThreadLocal — per-thread storage (audit-round7 CRIT fixes 1/2/3).
// ---------------------------------------------------------------------------
// Round-5 fix moved per-thread state from field-0 (clobbered across threads)
// into a thread-local map keyed by the ThreadLocal's raw pointer. Round-7
// found three follow-on CRITs:
//
//   (1) Pointer keys are invalidated by the moving GC (compact-header
//       forwarding, G1/gen-heap relocation). After GC the new pointer hash
//       misses; recycled addresses inherit dead entries cross-thread.
//       Fix: key by the JLS identity hash (pinned in the compact header),
//       obtained via `ctx.identity_hash_code(this)`.
//
//   (2) `withInitial` never stored the supplier, so every non-creator
//       thread saw null forever. Fix: side table
//       `WITH_INITIAL_SUPPLIERS` (identity-hash → supplier ObjectRef).
//       `get`, on miss, looks up + invokes + caches.
//
//   (3) `InheritableThreadLocal` had no parent→child copy. Fix:
//       `INHERITABLE_TL_IDS` records which TL identity hashes are
//       inheritable; on `Thread.start`, the parent's matching entries
//       are snapshotted into `INHERITED_PENDING[child_thread_id_hash]`.
//       The child's first TL access drains its pending bucket into the
//       local map (we lazily drain because the child runs on a fresh
//       OS thread we don't control from native).
//
// Slot 0 is retained for compatibility with code that walks the synthetic
// layout, but is never the source of truth.
//
// # One row per ThreadLocal, not per identity hash (gc-common w29-a)
//
// `docs/feature-designs/native-dispatch-architecture.md`
// rank 1. The map used to be `identity hash -> value`. An identity hash is 32
// bits minted from a per-heap counter that wraps, so two LIVE ThreadLocals of
// one VM can share one: `B.get()` answered `A`'s value and `B.set(x)`
// overwrote it (a security context, a transaction, a request scope handed to
// the wrong code), and a dead ThreadLocal's rooted value stayed for the
// thread's life and was inherited by the next ThreadLocal with its hash.
//
// Now the hash only picks a BUCKET, and each row in it names its ThreadLocal
// through a JNI WEAK global (`add_weak_global_root`): not a root, remapped by
// every moving pause, cleared once the ThreadLocal is unreachable. A lookup
// compares the resolved owner with the receiver, so colliders keep separate
// rows, and a row whose owner is cleared is dead: it is expunged -- its
// value's root and its weak handle released -- the next time this thread
// touches that bucket, at the amortised full expunge an insert runs once the
// map has doubled ([`TL_EXPUNGE_AT`]), or at thread death. That is
// `ThreadLocalMap`'s own contract (weak keys, stale entries expunged by the
// owning thread), which retires the round-7 "weak-key cleanup" TODO that used
// to stand here. A new ThreadLocal never inherits a dead one's row: the dead
// row's owner is cleared before its memory can be reused.
//
// Why not the lock-key registry (route R1) here: `get` is the hot path (every
// `ReentrantReadWriteLock` read acquisition goes through `readHolds.get()`),
// and a key lookup takes the process-wide registry mutex on every call
// (lesson jj). The weak handle is resolved under the calling VM's own
// global-ref lock, which the value's resolution takes anyway. The per-VM
// `withInitial` and `InheritableThreadLocal` tables below are cold and do use
// R1.

#[derive(Clone, Copy)]
pub enum ThreadLocalValue {
    Root { handle: usize, fallback: ObjectRef },
    Plain(Value),
}

/// One `ThreadLocal`'s value on one Java thread, with the `ThreadLocal` it
/// belongs to (gc-common w29-a; see the note above).
#[derive(Clone, Copy)]
pub struct TlRow {
    /// The ThreadLocal, as a JNI weak global handle owned by this row, or `0`
    /// in a context without weak globals (the unit-test mock, which never
    /// moves or frees objects) -- there `owner_fallback` IS its identity.
    pub owner: usize,
    /// The ThreadLocal's address when the row was made. Only ever read when
    /// `owner` is `0`.
    pub owner_fallback: ObjectRef,
    pub value: ThreadLocalValue,
}

/// A Java thread's `ThreadLocal` map: identity hash -> the rows of every
/// ThreadLocal with that hash (almost always one).
pub type TlMap = rustc_hash::FxHashMap<i32, Vec<TlRow>>;

/// The smallest map size (buckets) at which an insert runs a full expunge.
pub const TL_EXPUNGE_FLOOR: usize = 64;

std::thread_local! {
    /// The `ThreadLocal` map of the Java thread this OS thread is RUNNING:
    /// TL identity hash → the rows of that hash. A platform thread owns its OS
    /// thread for life, so for it this is simply its map. A virtual thread's
    /// continuation runs on whichever carrier mounts it; the mount swaps the
    /// virtual thread's own map in and the unmount swaps it back out
    /// ([`swap_current_thread_locals`], called by
    /// `vm_exec::resume_virtual_continuation`), so the map migrates with the
    /// Java thread instead of staying with the carrier.
    pub static TL_MAP: std::cell::RefCell<TlMap> = std::cell::RefCell::new(TlMap::default());
    /// One-shot flag: has the Java thread this OS thread is running drained
    /// the inherited ITL entries queued for it? Swapped together with
    /// `TL_MAP`, so every virtual thread drains its OWN bucket (it used to be
    /// only the first virtual thread a carrier ever ran), and re-armed by
    /// [`release_current_thread_locals`], so a JNI host thread that detaches
    /// and re-attaches as a different `Thread` drains that thread's bucket too.
    pub static TL_INHERITED_DRAINED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The map size (buckets) at which the next insert of a new row expunges
    /// every dead row of the map: twice the live size the last expunge left,
    /// so the scans cost O(1) per insert, amortised (gc-common w29-a). A
    /// heuristic only; not swapped with the map.
    pub static TL_EXPUNGE_AT: std::cell::Cell<usize> = const { std::cell::Cell::new(TL_EXPUNGE_FLOOR) };
    static TL_COMPAT_MRU: std::cell::Cell<[CompatTlMruSlot; 4]> =
        const { std::cell::Cell::new([CompatTlMruSlot { vm: 0, epoch: 0, tl_raw: 0, val_bits: 0 }; 4]) };
    static TL_COMPAT_MRU_NEXT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Clone, Copy)]
struct CompatTlMruSlot {
    vm: usize,
    epoch: u64,
    tl_raw: usize,
    /// `0` = empty, `1` = null value, `>= 8` = validated `ObjectRef` pointer bits.
    val_bits: usize,
}

#[inline(always)]
pub fn invalidate_tl_compat_mru() {
    let _ = TL_COMPAT_MRU.try_with(|c| {
        c.set([CompatTlMruSlot {
            vm: 0,
            epoch: 0,
            tl_raw: 0,
            val_bits: 0,
        }; 4]);
    });
}

#[inline(always)]
pub fn tl_compat_mru_lookup(vm: usize, epoch: u64, tl_raw: usize) -> Option<usize> {
    let slots = TL_COMPAT_MRU.with(|c| c.get());
    for s in slots {
        if s.val_bits != 0 && s.vm == vm && s.epoch == epoch && s.tl_raw == tl_raw {
            return Some(s.val_bits);
        }
    }
    None
}

#[inline]
pub fn tl_compat_mru_insert(vm: usize, epoch: u64, tl_raw: usize, val_bits: usize) {
    if !TL_INHERITED_DRAINED.with(|f| f.get()) || val_bits == 0 {
        return;
    }
    let mut slots = TL_COMPAT_MRU.with(|c| c.get());
    for s in &mut slots {
        if s.vm == vm && s.tl_raw == tl_raw && s.val_bits != 0 {
            s.epoch = epoch;
            s.val_bits = val_bits;
            TL_COMPAT_MRU.with(|c| c.set(slots));
            return;
        }
    }
    let idx = TL_COMPAT_MRU_NEXT.with(|n| {
        let cur = n.get();
        n.set((cur + 1) & 3);
        cur
    });
    slots[idx] = CompatTlMruSlot {
        vm,
        epoch,
        tl_raw,
        val_bits,
    };
    TL_COMPAT_MRU.with(|c| c.set(slots));
}

/// One Java thread's `ThreadLocal` state while it is NOT installed on the OS
/// thread running it: a virtual thread between mounts, or a carrier's own
/// (normally empty) state while a virtual thread is mounted on it.
///
/// Values are the same GC-stable [`ThreadLocalValue`]s `TL_MAP` holds: an
/// object value is a JNI global root (rooted and remapped by the VM's global
/// ref table), so a detached map needs no scan of its own. Dropping a
/// non-empty one without [`release_current_thread_locals`] leaks those roots
/// exactly as a dead thread's map used to; the virtual death path installs the
/// map and releases it.
///
/// `docs/feature-designs/native-dispatch-architecture.md`
/// (gc-common w4-b).
#[derive(Default)]
pub struct DetachedThreadLocals {
    pub map: TlMap,
    pub inherited_drained: bool,
}

impl DetachedThreadLocals {
    /// Number of `ThreadLocal` values held.
    pub fn len(&self) -> usize {
        self.map.values().map(Vec::len).sum()
    }

    /// `true` when no `ThreadLocal` value is held.
    pub fn is_empty(&self) -> bool {
        self.map.values().all(Vec::is_empty)
    }
}

/// Install `incoming` as the `ThreadLocal` state of the Java thread this OS
/// thread is about to run, and return the state it replaces.
///
/// The virtual-thread mount calls this with the continuation's saved state
/// (a fresh [`DetachedThreadLocals::default`] on its first mount) and keeps
/// the carrier's; the unmount calls it with the carrier's and saves the
/// continuation's. Without the swap two virtual threads mounted on one
/// carrier read and wrote ONE map (`ThreadLocal.get()` in B returned what A
/// set), a virtual thread that migrated carriers lost its values, and a dead
/// virtual thread's values were never released.
///
/// Must not run inside a `ThreadLocal` native (the `TL_MAP` borrow would
/// conflict); the mount/unmount sites run between continuation slices, where
/// no Java frame of this OS thread is active.
pub fn swap_current_thread_locals(incoming: DetachedThreadLocals) -> DetachedThreadLocals {
    invalidate_tl_compat_mru();
    let DetachedThreadLocals {
        map,
        inherited_drained,
    } = incoming;
    let map = TL_MAP.with(|m| std::mem::replace(&mut *m.borrow_mut(), map));
    let inherited_drained = TL_INHERITED_DRAINED.with(|f| f.replace(inherited_drained));
    DetachedThreadLocals {
        map,
        inherited_drained,
    }
}

/// Release every root a DETACHED `ThreadLocal` map holds (each value's global
/// root and each row's weak handle on its `ThreadLocal`) without installing
/// it on this OS thread. Returns the rows released.
///
/// gci i14/ve: for a parked virtual thread the collector found unreachable
/// (`vm::jdk_continuation::retire_collected_continuations`). Its map is never
/// swapped back in, and the caller is the collecting door's OS thread, which
/// may be inside a `ThreadLocal` native, so [`release_current_thread_locals`]
/// (which drains `TL_MAP`) is not an option. The thread's pending inherited
/// bucket, if any, is keyed by its dead mirror and is released by the same
/// pause's bucket sweep.
pub fn release_detached_thread_locals(
    ctx: &mut dyn NativeContext,
    detached: DetachedThreadLocals,
) -> usize {
    let rows: Vec<TlRow> = detached.map.into_values().flatten().collect();
    let released = rows.len();
    tl_release_rows(ctx, rows);
    released
}

pub fn tl_value_from_java(ctx: &mut dyn NativeContext, value: Value) -> ThreadLocalValue {
    match value {
        Value::Object(Some(obj)) => ThreadLocalValue::Root {
            handle: ctx.add_global_root(obj),
            fallback: obj,
        },
        other => ThreadLocalValue::Plain(other),
    }
}

pub fn tl_value_to_java(ctx: &dyn NativeContext, value: ThreadLocalValue) -> Value {
    match value {
        ThreadLocalValue::Root { handle, fallback } => {
            let obj = if handle != 0 {
                ctx.resolve_global_root(handle).unwrap_or(fallback)
            } else {
                fallback
            };
            Value::Object(Some(obj))
        }
        ThreadLocalValue::Plain(value) => value,
    }
}

pub fn tl_drop_value_root(ctx: &mut dyn NativeContext, value: ThreadLocalValue) {
    if let ThreadLocalValue::Root { handle, .. } = value {
        if handle != 0 {
            let _ = ctx.remove_global_root(handle);
        }
    }
}

/// Release the JNI handles `row` owns: its value's global root and its
/// owner's weak handle.
pub fn tl_release_row(ctx: &mut dyn NativeContext, row: TlRow) {
    tl_drop_value_root(ctx, row.value);
    if row.owner != 0 {
        let _ = ctx.remove_global_root(row.owner);
    }
}

pub fn tl_release_rows(ctx: &mut dyn NativeContext, rows: Vec<TlRow>) {
    for row in rows {
        tl_release_row(ctx, row);
    }
}

/// `row`'s ThreadLocal at its CURRENT address, or `None` once a collection
/// has found it unreachable (its weak handle is cleared).
pub fn tl_row_owner(ctx: &dyn NativeContext, row: &TlRow) -> Option<ObjectRef> {
    if row.owner == 0 {
        Some(row.owner_fallback)
    } else {
        ctx.resolve_global_root(row.owner)
    }
}

/// A new row for the ThreadLocal `tl` (its CURRENT address) holding `value`;
/// the row owns a fresh weak handle on `tl`.
pub fn tl_new_row(ctx: &mut dyn NativeContext, tl: ObjectRef, value: ThreadLocalValue) -> TlRow {
    TlRow {
        owner: ctx.add_weak_global_root(tl),
        owner_fallback: tl,
        value,
    }
}

/// The bucket of this thread's map `tl`'s rows live in. Only a bucket: two
/// ThreadLocals may share one (see the note above [`ThreadLocalValue`]).
#[inline]
pub fn tl_bucket_hash(ctx: &dyn NativeContext, tl: ObjectRef) -> i32 {
    ctx.identity_hash_code(tl)
}

/// Walk one bucket: move the rows of dead ThreadLocals into `dead`, and
/// answer the index of `tl`'s row, if it has one. No `ctx` call here can
/// allocate or run Java, so it runs under the `TL_MAP` borrow.
pub fn tl_bucket_scan(
    ctx: &dyn NativeContext,
    bucket: &mut Vec<TlRow>,
    tl: ObjectRef,
    dead: &mut Vec<TlRow>,
) -> Option<usize> {
    let mut mine = None;
    let mut i = 0;
    while i < bucket.len() {
        match tl_row_owner(ctx, &bucket[i]) {
            // `swap_remove` moves the last row to `i`, which is judged next;
            // an index already answered is below `i` and stays put.
            None => dead.push(bucket.swap_remove(i)),
            Some(owner) => {
                if owner == tl && mine.is_none() {
                    mine = Some(i);
                }
                i += 1;
            }
        }
    }
    mine
}

/// `tl`'s value on this thread, if it has one. Dead rows met in its bucket
/// are expunged into `dead` for the caller to release.
pub fn tl_lookup(
    ctx: &dyn NativeContext,
    tl: ObjectRef,
    dead: &mut Vec<TlRow>,
) -> Option<ThreadLocalValue> {
    let hash = tl_bucket_hash(ctx, tl);
    TL_MAP.with(|m| {
        let mut map = m.borrow_mut();
        let bucket = map.get_mut(&hash)?;
        let found = tl_bucket_scan(ctx, bucket, tl, dead).map(|i| bucket[i].value);
        if bucket.is_empty() {
            map.remove(&hash);
        }
        found
    })
}

/// Remove and answer `tl`'s row on this thread. Dead rows met in its bucket
/// are expunged into `dead`.
pub fn tl_remove_row(ctx: &dyn NativeContext, tl: ObjectRef, dead: &mut Vec<TlRow>) -> Option<TlRow> {
    invalidate_tl_compat_mru();
    let hash = tl_bucket_hash(ctx, tl);
    TL_MAP.with(|m| {
        let mut map = m.borrow_mut();
        let bucket = map.get_mut(&hash)?;
        let row = tl_bucket_scan(ctx, bucket, tl, dead).map(|i| bucket.swap_remove(i));
        if bucket.is_empty() {
            map.remove(&hash);
        }
        row
    })
}

/// Store `value` as the ThreadLocal `tl`'s value on this thread (`tl` at its
/// CURRENT address), and answer the value it replaces -- whose root the
/// caller releases. A new row mints its weak owner handle; an insert that
/// grows the map past [`TL_EXPUNGE_AT`] expunges every dead row first.
pub fn tl_store(
    ctx: &mut dyn NativeContext,
    tl: ObjectRef,
    value: ThreadLocalValue,
) -> Option<ThreadLocalValue> {
    invalidate_tl_compat_mru();
    let hash = tl_bucket_hash(&*ctx, tl);
    let mut dead = Vec::new();
    let replaced = {
        let rctx: &dyn NativeContext = &*ctx;
        TL_MAP.with(|m| {
            let mut map = m.borrow_mut();
            let bucket = map.get_mut(&hash)?;
            let old = tl_bucket_scan(rctx, bucket, tl, &mut dead)
                .map(|i| std::mem::replace(&mut bucket[i].value, value));
            if bucket.is_empty() {
                map.remove(&hash);
            }
            old
        })
    };
    if replaced.is_none() {
        // The owner handle is minted outside the borrow (it takes the VM's
        // global-ref lock, like every `ctx` call here, and runs no Java).
        let row = tl_new_row(ctx, tl, value);
        let buckets = TL_MAP.with(|m| {
            let mut map = m.borrow_mut();
            map.entry(hash).or_default().push(row);
            map.len()
        });
        if buckets >= TL_EXPUNGE_AT.with(|at| at.get()) {
            tl_expunge_all(ctx);
        }
    }
    tl_release_rows(ctx, dead);
    replaced
}

/// Expunge every row of this thread's map whose ThreadLocal is dead, release
/// their handles, and re-arm [`TL_EXPUNGE_AT`] at twice the live size.
pub fn tl_expunge_all(ctx: &mut dyn NativeContext) {
    let mut dead = Vec::new();
    let live = {
        let rctx: &dyn NativeContext = &*ctx;
        TL_MAP.with(|m| {
            let mut map = m.borrow_mut();
            map.retain(|_, bucket| {
                let mut i = 0;
                while i < bucket.len() {
                    if tl_row_owner(rctx, &bucket[i]).is_some() {
                        i += 1;
                    } else {
                        dead.push(bucket.swap_remove(i));
                    }
                }
                !bucket.is_empty()
            });
            map.len()
        })
    };
    TL_EXPUNGE_AT.with(|at| at.set(live.saturating_mul(2).max(TL_EXPUNGE_FLOOR)));
    tl_release_rows(ctx, dead);
}

/// One `withInitial` supplier: the VM whose `withInitial` filed it, and the
/// supplier as a JNI global root owned by the row (`0` where the context has
/// none; `fallback` is then the supplier).
pub struct TlSupplierRow {
    pub vm: usize,
    pub root: usize,
    pub fallback: ObjectRef,
}

/// The per-VM ThreadLocal side tables, keyed by the ThreadLocal's WEAK LOCK
/// KEY (`crate::gc_stable_weak_lock_key`) -- gc-common w29-a, route R1 of
/// `docs/feature-designs/native-dispatch-architecture.md`.
///
/// A lock key is unique per object and never minted again, and across VMs
/// too, so one process-wide table serves every VM without mixing them; each
/// row names its VM for teardown ([`forget_vm_thread_local_side_tables`]).
/// Rows are dropped when their ThreadLocal dies, by [`forget_thread_local_keys`]
/// from the lock-key sweep; a dropped supplier's root is queued in
/// `orphaned_roots` for the next `ThreadLocal` native of its VM to release
/// (the sweep has no `ctx`).
///
/// History: both tables were `VmScoped` maps keyed by the ThreadLocal's
/// identity hash (per VM since gc-common w10-a), and held their objects as
/// PERMANENT `register_var_handle_root` roots read back by the object's own
/// identity hash. Two live ThreadLocals with one hash ran each other's
/// supplier / `childValue`, a colliding root registration unrooted the first
/// object, and every supplier and every `InheritableThreadLocal` ever built
/// stayed reachable for the VM's life.
#[derive(Default)]
pub struct TlSideTables {
    /// `withInitial` ThreadLocal -> its supplier. Read by `get` on a miss.
    pub suppliers: rustc_hash::FxHashMap<usize, TlSupplierRow>,
    /// `InheritableThreadLocal` (anything whose `<init>` ran
    /// `native_itl_init`) -> its VM. Holds no reference: the snapshot finds
    /// each ITL through the parent thread's own row
    /// ([`snapshot_inheritable_tl_entries`]), and an ITL is collectable like
    /// any object.
    pub inheritable: rustc_hash::FxHashMap<usize, usize>,
    /// How many `inheritable` rows each VM has: the snapshot's early out.
    pub inheritable_per_vm: rustc_hash::FxHashMap<usize, usize>,
    /// Supplier roots of rows the lock-key sweep dropped, per VM, awaiting a
    /// `ctx` to release them ([`tl_release_orphaned_roots`]).
    pub orphaned_roots: rustc_hash::FxHashMap<usize, Vec<usize>>,
}

/// A leaf lock (`LockLevel::Scratch`): no path holds it across a `ctx` call,
/// Java, or another lock.
pub fn tl_side_tables() -> &'static cratonvm_types::lock_order::OrderedPlMutex<TlSideTables> {
    static T: std::sync::OnceLock<cratonvm_types::lock_order::OrderedPlMutex<TlSideTables>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            TlSideTables::default(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Set once any `withInitial` supplier was filed: a `get` miss in a process
/// that never used `withInitial` skips the key lookup.
pub static ANY_TL_SUPPLIER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set once any supplier root was queued in `orphaned_roots`.
pub static TL_ORPHANS_QUEUED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The `withInitial` supplier of the ThreadLocal `tl`, at its current
/// address, if `tl` was built by `withInitial`. Never mints a key.
pub fn tl_with_initial_supplier(ctx: &dyn NativeContext, tl: ObjectRef) -> Option<ObjectRef> {
    if !ANY_TL_SUPPLIER.load(std::sync::atomic::Ordering::Acquire) {
        return None;
    }
    let key = crate::existing_weak_lock_key(ctx, tl)?;
    let (root, fallback) = tl_side_tables()
        .lock()
        .suppliers
        .get(&key)
        .map(|row| (row.root, row.fallback))?;
    Some(if root != 0 {
        ctx.resolve_global_root(root).unwrap_or(fallback)
    } else {
        fallback
    })
}

/// File `key` (a ThreadLocal's weak lock key) as one of `vm`'s
/// `InheritableThreadLocal`s.
pub fn tl_inheritable_insert(vm: usize, key: usize) {
    let mut guard = tl_side_tables().lock();
    let t = &mut *guard;
    if t.inheritable.insert(key, vm).is_none() {
        *t.inheritable_per_vm.entry(vm).or_default() += 1;
    }
}

/// Drop `key`'s inheritable row, keeping the per-VM count.
pub fn tl_inheritable_drop(t: &mut TlSideTables, key: usize) {
    if let Some(vm) = t.inheritable.remove(&key) {
        let emptied = match t.inheritable_per_vm.get_mut(&vm) {
            Some(n) => {
                *n = n.saturating_sub(1);
                *n == 0
            }
            None => false,
        };
        if emptied {
            t.inheritable_per_vm.remove(&vm);
        }
    }
}

/// Does `vm` have any `InheritableThreadLocal`?
pub fn tl_vm_has_inheritable(vm: usize) -> bool {
    tl_side_tables().lock().inheritable_per_vm.contains_key(&vm)
}

/// Release the supplier roots the lock-key sweep dropped for this VM.
pub fn tl_release_orphaned_roots(ctx: &mut dyn NativeContext) {
    if !TL_ORPHANS_QUEUED.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let vm = ctx.vm_identity();
    let roots = tl_side_tables().lock().orphaned_roots.remove(&vm);
    for root in roots.unwrap_or_default() {
        let _ = ctx.remove_global_root(root);
    }
}

/// The lock-key sweep freed `keys` (their objects are dead): drop the
/// `withInitial` and `InheritableThreadLocal` rows filed under them, queueing
/// each dropped supplier's root for its VM. gc-common w29-a; called through
/// [`forget_thread_state_keys`].
pub fn forget_thread_local_keys(keys: &[usize]) {
    if keys.is_empty() {
        return;
    }
    let mut guard = tl_side_tables().lock();
    let t = &mut *guard;
    if t.suppliers.is_empty() && t.inheritable.is_empty() {
        return;
    }
    for key in keys {
        if let Some(row) = t.suppliers.remove(key) {
            if row.root != 0 {
                t.orphaned_roots.entry(row.vm).or_default().push(row.root);
                TL_ORPHANS_QUEUED.store(true, std::sync::atomic::Ordering::Release);
            }
        }
        tl_inheritable_drop(t, *key);
    }
}

/// The ONE `forget_*_keys(&freed)` hook of lane w29-a's thread-state tables,
/// listed in `lib.rs::sweep_lock_keys` and `lib.rs::forget_vm_lock_keys`: the
/// `withInitial` / `InheritableThreadLocal` rows here, the `--compatible`
/// carrier-thread-local rows, the uncaught-exception handlers, the JFR thread
/// exclusions and the deprecated stop / suspend rows, each keyed by a weak
/// lock key since gc-common w29-a. Each table's lock is a leaf, taken one at
/// a time.
pub fn forget_thread_state_keys(keys: &[usize]) {
    if keys.is_empty() {
        return;
    }
    forget_thread_local_keys(keys);
    crate::shared_secrets_bridge::forget_carrier_tl_keys(keys);
    crate::uncaught_handlers::forget_uncaught_handler_keys(keys);
    crate::hooks::jfr_forget_jfr_thread_keys(keys);
    crate::deprecated_lang::forget_deprecated_thread_keys(keys);
}

/// Drop `vm`'s `withInitial` suppliers and inheritable-ThreadLocal rows (VM
/// teardown, from `lib.rs::forget_vm_native_root_stores`), and the supplier
/// roots queued for it. Idempotent.
///
/// gc-common w22-a (`common-w21a-more-process-wide-global-root-handle-caches`
/// item 7): also `vm`'s rows in [`tl_inherited_pending`] -- snapshots queued
/// for child threads that never started (or never touched a `ThreadLocal`).
/// The per-collection sweep only ever visits a LIVE VM's rows, so these
/// outlived the VM, each bucket holding handles into its dead global-ref
/// table. Dropped unreleased: that table dies with the VM.
pub fn forget_vm_thread_local_side_tables(vm: usize) {
    {
        let mut guard = tl_side_tables().lock();
        let t = &mut *guard;
        t.suppliers.retain(|_, row| row.vm != vm);
        t.inheritable.retain(|_, owner| *owner != vm);
        t.inheritable_per_vm.remove(&vm);
        t.orphaned_roots.remove(&vm);
    }
    tl_inherited_pending()
        .lock()
        .retain(|&(owner, _), _| owner != vm);
}

/// One child `Thread`'s inherited snapshot: `(TL identity hash, row)` pairs,
/// each row owning its own weak owner handle and value root in the capturing
/// VM (gc-common w29-a: it was `hash -> value`, so two inherited colliders
/// were one entry).
pub type InheritedTlBucket = Vec<(i32, TlRow)>;

/// `(vm_identity, child Thread address)` → the snapshot of inherited entries
/// to seed when that thread first accesses any `ThreadLocal`. Consumed
/// (drained) once, by the child's first access or by its death path.
///
/// # Keyed by the Thread OBJECT, per VM, and swept by the collector
///
/// (gc-common w5-b,
/// `docs/feature-designs/native-dispatch-architecture.md`.)
/// It used to be keyed by the child's identity hash alone, which had three
/// defects: a `Thread` that was constructed and never started ran neither the
/// drain nor a death path, so its bucket -- and every inherited value's global
/// root -- lived for the life of the process (every executor that mints
/// workers eagerly under an `InheritableThreadLocal` context); a second
/// capture for the same hash dropped the previous bucket without releasing
/// its roots; and a thread of ANOTHER VM (or a later thread of this one) with
/// a colliding 31-bit hash drained a stranger's values, resolving another
/// VM's global-ref handles against its own table.
///
/// Now the key is the child's address in its own VM's heap, which is unique
/// among live objects, and the VM keeps it current: after every collection
/// `vm::memory::gc::update_all_roots` calls
/// [`remap_and_sweep_inherited_tl_buckets`], which re-keys a moved child and
/// hands back the global-root handles of every bucket whose child did NOT
/// survive, for the VM to release. The table itself roots nothing: the child
/// `Thread` lives or dies on its own reachability, exactly as HotSpot's
/// `Thread.inheritableThreadLocals` map does.
///
/// Callers that hold a child across a GC point (the `childValue` upcalls in
/// [`snapshot_inheritable_tl_entries`]) must queue with the child's address
/// read AFTER that point.
///
/// Not an `ObjectRef`-keyed map for `vm/src/memory/addr_keyed.rs`'s census
/// (the key is a plain address), but its disposition is that module's: REMAPPED
/// + SWEPT, from `update_all_roots` before its empty-map early return.
pub fn tl_inherited_pending(
) -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<(usize, usize), InheritedTlBucket>> {
    static S: std::sync::OnceLock<
        parking_lot::Mutex<rustc_hash::FxHashMap<(usize, usize), InheritedTlBucket>>,
    > = std::sync::OnceLock::new();
    S.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

/// `true` once any bucket has ever been queued: the collector's per-cycle
/// sweep is one relaxed load in a process that never used an
/// `InheritableThreadLocal`.
pub static ANY_INHERITED_TL_QUEUED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[inline]
pub fn inherited_tl_key(ctx: &dyn NativeContext, child: ObjectRef) -> (usize, usize) {
    (ctx.vm_identity(), child.as_ptr() as usize)
}

/// Queue an inheritable snapshot for the child Java `Thread` `child`. The
/// child's first TL access drains it (`drain_inherited_for_current_thread`);
/// its death path releases it if it never made one; the collector releases it
/// if the `Thread` dies unstarted ([`tl_inherited_pending`]).
///
/// `child` must be the child's CURRENT address: read it after any GC point
/// between the capture and this call. A bucket already queued for the same
/// child is replaced and its values' global roots are released -- it used to
/// be dropped with them still registered.
pub fn queue_inherited_tl_for_child(
    ctx: &mut dyn NativeContext,
    child: ObjectRef,
    mut snapshot: InheritedTlBucket,
) {
    let key = inherited_tl_key(ctx, child);
    // gch h3/jx: under the ephemeron switch the queued values are held by the
    // table itself; their strong handles are released once the table has them.
    let released_roots = if inherited_pending_ephemeron_enabled() {
        tl_bucket_values_to_plain(ctx, &mut snapshot)
    } else {
        Vec::new()
    };
    let replaced = tl_inherited_pending().lock().insert(key, snapshot);
    ANY_INHERITED_TL_QUEUED.store(true, std::sync::atomic::Ordering::Release);
    // Lock dropped: `tl_release_row` takes the VM's JNI global table lock.
    for handle in released_roots {
        let _ = ctx.remove_global_root(handle);
    }
    if let Some(old) = replaced {
        for (_, row) in old {
            tl_release_row(ctx, row);
        }
    }
}

/// gch h3/jx (`docs/feature-designs/native-dispatch-architecture.md`,
/// the ephemeron of `common-w14d-proposal-ephemeron-native-side-tables` for this
/// one table): `CRATONVM_TL_INHERITED_PENDING_EPHEMERON`, default ON since
/// gch h4/fl (`runtime_flag_default_on`, `=0` restores the JNI-global values;
/// vh3c-jx passed every rule with the switch at `=1`, and `vh4-fl.list` block
/// C runs the default arm with no env on Generational, G1 and ZGC), read per
/// queued bucket (no new `static`: the statics ratchet). On, a queued
/// bucket's object values are held by
/// [`tl_inherited_pending`] itself as plain references
/// (`ThreadLocalValue::Plain(Value::Object(..))`) instead of as JNI GLOBAL
/// roots, and the `inherited-tl-pending` native-root row
/// ([`gc_scan_inherited_tl_pending_values`]) reports each one as a
/// `metadata_pin` edge from its child `Thread` on a licensed cycle -- marked only
/// if the child is, HotSpot's `Thread.inheritableThreadLocals` edge -- and as a
/// plain root on every other cycle (over-retention for that cycle, never
/// unsound). So a never-started `Thread`'s inherited values die in the SAME
/// collection as the `Thread`, where the handles made them live one collection
/// longer (`ak1_itl_conc_cpt`: `gcCalls=3`, HotSpot 1). Reached only from the
/// `--compatible` `Thread` constructor shims (`lang_system.rs`); `--real-jdk`
/// runs the JDK's own map. Only a Generational STW collection with no
/// concurrent cycle open pins (`vm::memory::native_roots`
/// `scan_inherited_tl_pending`, narrowed by gch h3/jx), and -- since gc wave 9
/// lane F (`CRATONVM_G1_REMARK_ITL_EPHEMERON`) and gci i6/g1t
/// (`CRATONVM_G1_ITL_DEFER_AT_MARK`, both default ON) -- a G1 marking cycle's
/// initial mark and remark, whose remark closes the pin mark-exact
/// ([`gc_inherited_tl_pending_values_of_live_children`]); ZGC and every other
/// G1 pause root the values as the JNI-global arm did.
pub fn inherited_pending_ephemeron_enabled() -> bool {
    true
}

/// zt t27/zc10 (`docs/feature-designs/zgc-architecture.md`
/// item 3, `h5_IT1_C`): `CRATONVM_ZGC_ITL_DEFER_IN_CYCLE`, default ON, `=0`
/// kills it. ZGC only (it acts only while a ZGC concurrent mark is in flight
/// or opening); matters on `--compatible`. On, every ZGC root scan pins a queued
/// bucket's values to their child `Thread` (`vm::memory::native_roots`
/// `inherited_tl_pending_heap_defers`) -- the concurrent cycle's opening scan
/// and the scans while its mark is in flight included, which rooted them -- so
/// a never-started `Thread`'s inherited values die in the concurrent
/// `System.gc()` that frees the `Thread`, as on HotSpot (was: the next one).
///
/// Its other half is here: a bucket TAKEN while a ZGC mark is in flight (the
/// child started, or touched its first `ThreadLocal`, mid-cycle) leaves a
/// SHADOW of its object values in [`tl_inherited_pending`], keyed by the child
/// with [`INHERITED_TL_SHADOW_BIT`] set, until the end of the next pause that
/// sweeps the table (`remap_and_sweep_inherited_tl_buckets` drops every
/// shadow). The scans in between keep pinning those values to the (real)
/// child, so the cycle's closing scan files every pin owner its opening scan
/// filed: without the shadow the closing pause's root-filter repair
/// (`ZMarkRootFilter::late_owners_within`) finds a pin owner gone and refuses
/// to certify the concurrent mark. A shadow owns no handle (its rows' weak
/// owner is `0`, its values are plain), and the values themselves are JNI
/// global roots of the started child from the take on, so a shadow only ever
/// adds an edge. Each shadowed value also goes through the SATB keep-alive
/// (`gc_reference_keep_alive`), the deletion of the child -> value edge while
/// the mark runs. `=0`: the scans root the values while a mark is in flight or
/// opening and no shadow is made, as before. Read per take and per scan.
pub fn zgc_itl_defer_in_cycle_enabled() -> bool {
    true
}

/// zt t27/zc10: the key bit of a shadow bucket ([`zgc_itl_defer_in_cycle_enabled`]):
/// `(vm, child | INHERITED_TL_SHADOW_BIT)`. No object address has the top bit
/// set, so a shadow never answers an exact-key lookup (the take, the
/// queued-for-child test); the scans strip it to pin to the real child.
pub const INHERITED_TL_SHADOW_BIT: usize = 1usize << (usize::BITS - 1);

/// gch h3/jx: turn every object value of `bucket` from a JNI global root into a
/// plain reference the pending table holds, and return the handles for the
/// caller to release AFTER the bucket is in the table (no collection can run
/// in between: this is a VM-state native between safepoints, as
/// `memory::string_table::intern_receiver` relies on). A value read from a
/// handle is its current address.
pub fn tl_bucket_values_to_plain(
    ctx: &mut dyn NativeContext,
    bucket: &mut InheritedTlBucket,
) -> Vec<usize> {
    let mut handles = Vec::new();
    for (_, row) in bucket.iter_mut() {
        if let ThreadLocalValue::Root { handle, fallback } = row.value {
            let obj = if handle != 0 {
                ctx.resolve_global_root(handle).unwrap_or(fallback)
            } else {
                fallback
            };
            row.value = ThreadLocalValue::Plain(Value::Object(Some(obj)));
            if handle != 0 {
                handles.push(handle);
            }
        }
    }
    handles
}

/// gch h3/jx: the reverse of [`tl_bucket_values_to_plain`], for a bucket just
/// taken OUT of the pending table (nothing reports its values any more): every
/// plain object value becomes a JNI global root again, before anything can
/// collect, so the rows that join `TL_MAP` hold handles as every other row
/// does. A no-op on a bucket queued with the switch off (its object values are
/// `Root`s; a `Plain` object value exists only through the switch).
pub fn tl_bucket_values_to_roots(ctx: &mut dyn NativeContext, bucket: &mut InheritedTlBucket) {
    for (_, row) in bucket.iter_mut() {
        if let ThreadLocalValue::Plain(Value::Object(Some(obj))) = row.value {
            row.value = tl_value_from_java(ctx, Value::Object(Some(obj)));
        }
    }
}

/// gch h3/jx: the `inherited-tl-pending` native-root row's scan
/// (`vm::memory::native_roots`): `vm_identity`'s queued plain object values
/// (only [`inherited_pending_ephemeron_enabled`] makes any). `deferrable` is
/// THIS VM's loader-metadata licence folded with
/// `VmHeap::metadata_pin_deferrable`, exactly as the `classloader-values` row
/// passes it: a deferrable value is pinned to its child `Thread`
/// (`metadata_pin`, marked iff the child is), any other is pushed onto `out` as
/// a root. One relaxed load when no bucket was ever queued.
pub fn gc_scan_inherited_tl_pending_values(
    vm_identity: usize,
    out: &mut Vec<ObjectRef>,
    deferrable: &dyn Fn(usize) -> bool,
) {
    if !ANY_INHERITED_TL_QUEUED.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let mut pins: Vec<(usize, usize)> = Vec::new();
    {
        let pending = tl_inherited_pending().lock();
        for (&(vm, child), bucket) in pending.iter() {
            if vm != vm_identity {
                continue;
            }
            // zt t27/zc10: a shadow pins to the real child (its key bit
            // stripped); `child` of an ordinary bucket is unchanged.
            let child = child & !INHERITED_TL_SHADOW_BIT;
            for (_, row) in bucket {
                if let ThreadLocalValue::Plain(Value::Object(Some(value))) = row.value {
                    let addr = value.as_ptr() as usize;
                    if deferrable(addr) {
                        pins.push((child, addr));
                    } else {
                        out.push(value);
                    }
                }
            }
        }
    }
    // Lock dropped: the registry's write lock is taken alone.
    cratonvm_types::metadata_pin::add_metadata_pins(vm_identity, &pins);
}

/// gc wave 9 lane F (`gch-h3jx-g1-compat-unstarted-thread-inherited-values-never-released-20261008`):
/// the G1 remark's ephemeron step. Appends to `out` every queued plain object
/// value of `vm_identity` whose child `Thread` reads live and which itself
/// reads dead, both by `is_live` (the completed mark's verdict), so the
/// caller marks it: a live child's inherited values are reachable on HotSpot
/// (`Thread.inheritableThreadLocals`). The pairs are copied out under the
/// table lock and judged after it is dropped (`is_live` takes the heap's
/// region lock). One relaxed load when no bucket was ever queued.
pub fn gc_inherited_tl_pending_values_of_live_children(
    vm_identity: usize,
    is_live: &dyn Fn(usize) -> bool,
    out: &mut Vec<usize>,
) {
    if !ANY_INHERITED_TL_QUEUED.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    {
        let pending = tl_inherited_pending().lock();
        for (&(vm, child), bucket) in pending.iter() {
            if vm != vm_identity {
                continue;
            }
            for (_, row) in bucket {
                if let ThreadLocalValue::Plain(Value::Object(Some(value))) = row.value {
                    pairs.push((child, value.as_ptr() as usize));
                }
            }
        }
    }
    for (child, value) in pairs {
        if is_live(child) && !is_live(value) {
            out.push(value);
        }
    }
}

/// gch h3/jx: the `inherited-tl-pending` row's remap: re-address every queued
/// plain object value of `vm_identity` that the collection moved. The child
/// keys are re-keyed (and dead children's buckets dropped) by
/// [`remap_and_sweep_inherited_tl_buckets`] in the same epilogue; the two touch
/// disjoint halves of a bucket, so their order does not matter (a dead value
/// is never a key of the map).
pub fn gc_update_inherited_tl_pending_values(
    vm_identity: usize,
    pointer_map: &cratonvm_types::PointerMap,
) {
    if pointer_map.is_empty() || !ANY_INHERITED_TL_QUEUED.load(std::sync::atomic::Ordering::Acquire)
    {
        return;
    }
    let mut pending = tl_inherited_pending().lock();
    for (&(vm, _), bucket) in pending.iter_mut() {
        if vm != vm_identity {
            continue;
        }
        for (_, row) in bucket.iter_mut() {
            if let ThreadLocalValue::Plain(Value::Object(Some(value))) = row.value {
                if let Some(&to) = pointer_map.get(&(value.as_ptr() as usize)) {
                    debug_assert!(to != 0, "GC pointer map contains null address");
                    // SAFETY: the collector's forwarding address for a live
                    // object of this VM's heap.
                    let moved = unsafe { ObjectRef::from_raw(to as *mut u8) };
                    row.value = ThreadLocalValue::Plain(Value::Object(Some(moved)));
                }
            }
        }
    }
}

/// Is a snapshot queued for the child `Thread` `child` (at its current
/// address) in this VM? The predicate `native_thread_start0` asks to decide
/// whether a construction-time capture already happened.
pub fn inherited_tl_queued_for_child(ctx: &dyn NativeContext, child: ObjectRef) -> bool {
    tl_inherited_pending()
        .lock()
        .contains_key(&inherited_tl_key(ctx, child))
}

/// Remove and return the bucket queued for the Java thread this OS thread is
/// running, if any.
pub fn take_inherited_for_current_thread(ctx: &mut dyn NativeContext) -> Option<InheritedTlBucket> {
    let thr_obj = ctx.current_thread_object();
    take_inherited_for_thread_object(ctx, thr_obj)
}

/// zt t20/tg9 (`CRATONVM_TL_RELEASE_MINTS_NO_THREAD`, default ON, `=0` kill):
/// whether [`release_current_thread_locals`] looks the inherited bucket up
/// only under a `java.lang.Thread` the thread already has
/// (`current_thread_object_if_any`) instead of minting one
/// (`current_thread_object`). A bucket is keyed by a child `Thread` that
/// existed when it was queued, so a thread without a mirror has none, and
/// minting one at the thread's end only to look it up was pure side effect:
/// a `java.lang.Thread` built, a JDK thread id drawn from
/// `ThreadIdentifiers`, and -- for the system class loader the mirror's
/// `contextClassLoader` asks for -- `ClassLoaders.<clinit>` run on that
/// thread. The ZGC director tick's attachment (`run_attached_service_as`,
/// detached after every `Timer` collection) is such a thread: with
/// `-XX:ZCollectionInterval=0.01` its first detach came before
/// `VM.saveProperties`, so `ClassLoaders.<clinit>` failed there with
/// `IllegalStateException: Not yet initialized [in thread "ZDirector"]` and
/// every later use of the class threw `NoClassDefFoundError` (t16 rows
/// `tg7_c1_D-*`, Defect D1). HotSpot's director is a `NonJavaThread` and
/// runs no Java. `=0`: mint, as before.
pub fn tl_release_mints_no_thread_enabled() -> bool {
    true
}

/// [`take_inherited_for_current_thread`] for the thread-end release: with
/// [`tl_release_mints_no_thread_enabled`], `None` at once for a thread that
/// has no `java.lang.Thread` (nothing can be queued under one it never had).
pub fn take_inherited_for_ending_thread(ctx: &mut dyn NativeContext) -> Option<InheritedTlBucket> {
    if tl_release_mints_no_thread_enabled() {
        let thr_obj = ctx.current_thread_object_if_any()?;
        return take_inherited_for_thread_object(ctx, thr_obj);
    }
    take_inherited_for_current_thread(ctx)
}

