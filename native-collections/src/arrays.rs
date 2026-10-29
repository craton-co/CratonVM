// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

// ===========================================================================
// Arrays utility class
// ===========================================================================

pub(crate) fn register_arrays_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(
        "java/util/Arrays",
        "copyOf",
        "([Ljava/lang/Object;I)[Ljava/lang/Object;",
        native_arrays_copy_of,
    );
    r.register(
        "java/util/Arrays",
        "toString",
        "([Ljava/lang/Object;)Ljava/lang/String;",
        native_arrays_to_string,
    );
    r.register(
        "java/util/Arrays",
        "asList",
        "([Ljava/lang/Object;)Ljava/util/List;",
        native_arrays_as_list,
    );
    let aal = "java/util/Arrays$ArrayList";
    r.register(aal, "size", "()I", native_arrays_array_list_size);
    r.register(aal, "isEmpty", "()Z", native_arrays_array_list_is_empty);
    r.register(
        aal,
        "get",
        "(I)Ljava/lang/Object;",
        native_arrays_array_list_get,
    );
    r.register(
        aal,
        "iterator",
        "()Ljava/util/Iterator;",
        native_arrays_array_list_iterator,
    );
    r.register("java/util/Arrays", "sort", "([I)V", native_arrays_sort_int);
    r.register(
        "java/util/Arrays",
        "sort",
        "([Ljava/lang/Object;)V",
        native_arrays_sort_objects,
    );
    r.register("java/util/Arrays", "fill", "([II)V", native_arrays_fill_int);
    r.register(
        "java/util/Arrays",
        "fill",
        "([Ljava/lang/Object;Ljava/lang/Object;)V",
        native_arrays_fill_object,
    );
    r.register(
        "java/util/Arrays",
        "binarySearch",
        "([II)I",
        native_arrays_binary_search_int,
    );
    r.register(
        "java/util/Arrays",
        "equals",
        "([I[I)Z",
        native_arrays_equals_int,
    );
    r.set_category(__prev_cat);
}

// FMT-STREAMS-CCE: the real JDK's 2-arg `Arrays.copyOf(T[] original, int
// newLength)` allocates the copy via `Array.newInstance(original.getClass()
// .getComponentType(), newLength)`, so the result preserves `original`'s
// exact runtime component type — including nested array types. E.g. copying
// an `Object[][]` (as `java.util.stream.SpinedBuffer.ensureCapacity` does for
// its `E[][] spine` field, reached via a `LongStream`/boxing pipeline's
// `toArray()`) must yield another `Object[][]`, not a flat `Object[]`. This
// previously always allocated a flat `Object[]` via `alloc_ref_array`
// (component ClassId hardcoded to 0) regardless of `src`'s real type, so
// callers whose static type is a multi-dimensional array hit an implicit
// erasure checkcast back to the 2D descriptor against the flat 1D result and
// threw `ClassCastException: java.lang.Object cannot be cast to
// [[Ljava.lang.Object;` (format.datetime.standard.InstantFormatterTests).
// Fix: read `src`'s own component ClassId from its heap header (same
// template `real_jdk_to_array_typed` / the native-builtins `phases_early`
// `copyOf` overloads already use) and propagate it via `new_ref_array`.
pub(crate) fn native_arrays_copy_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let src = match args.first() {
        Some(Value::Object(Some(arr))) => *arr,
        // `Arrays.copyOf` reads `original.length` as its first act, so a null
        // source is a `NullPointerException` AT THE CALL. Returning `null`
        // instead moved the NPE to whatever later dereferenced the result, with
        // a stack trace pointing away from the actual mistake.
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some(
                    "Cannot read the array length because \"original\" is null".to_string(),
                ),
            }
            .into())
        }
    };
    let new_len = match args.get(1) {
        // `std::cmp::max(*l, 0)` — the clamp turned a loud bug into a quiet
        // one. `Arrays.copyOf(names, size - 1)` with a bookkeeping bug that
        // makes `size == 0` throws `NegativeArraySizeException: -1` on HotSpot,
        // at the faulty line; clamped to 0 it hands back an empty array and the
        // caller proceeds on silently truncated data. The allocation this feeds
        // (`new_ref_array`) is the same one `anewarray` performs, and the
        // opcode raises — so this is also the reflective side of a
        // native-vs-bytecode pair disagreeing about one JVMS rule.
        Some(Value::Int(l)) if *l < 0 => {
            return Err(
                cratonvm_types::error::RuntimeError::NegativeArraySizeException { size: *l }.into(),
            )
        }
        Some(Value::Int(l)) => *l as usize,
        _ => return Ok(Some(Value::Object(None))),
    };
    let old_len = ctx.array_length(src);
    let comp_cid = ctx.class_id_of_object(src);
    // cceres3: pin across GC-capable call (stream stale-at-store wave) — the
    // result-array allocation can move `src` before the copy loop.
    //
    // gc-common w10-g: registered-only (`Arrays.copyOf(Object[], int)`), so the
    // allocation may collect and retry, as the `native-builtins` twin has since
    // w8-b; whichever registration wins, the call now reclaims. `live` audit:
    // `src` is pinned above and re-read below; `comp_cid` is a `ClassId`.
    let src_pin = ctx.pin_native_root(src);
    let result = new_ref_array_grow(ctx, comp_cid, new_len, GrowAlloc::Reclaim);
    let src = ctx.read_native_pin(src_pin, src);
    let copy_len = std::cmp::min(old_len, new_len);
    for i in 0..copy_len {
        let val = ctx.get_array_element(src, i);
        ctx.set_array_element(result, i, val);
    }
    ctx.unpin_native_roots(src_pin);
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_arrays_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        Some(Value::Object(None)) => {
            let s = ctx.create_string("null");
            return Ok(Some(Value::Object(Some(s))));
        }
        _ => {
            let s = ctx.create_string("null");
            return Ok(Some(Value::Object(Some(s))));
        }
    };
    let len = ctx.array_length(arr);
    // cceres3: pin across GC-capable call (stream stale-at-store wave) — each
    // obj_to_display_units can dispatch toString and move `arr`.
    let arr_pin = ctx.pin_native_root(arr);
    let mut parts = Vec::with_capacity(len);
    for i in 0..len {
        let arr = ctx.read_native_pin(arr_pin, arr);
        let val = ctx.get_array_element(arr, i);
        parts.push(obj_to_display_units(ctx, &val)?);
    }
    ctx.unpin_native_roots(arr_pin);
    let text = wrap_join("[", &parts, ", ", "]");
    let s = ctx.create_string_from_units(&text);
    Ok(Some(Value::Object(Some(s))))
}

pub(crate) fn native_arrays_as_list(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // HotSpot's `Arrays.asList(T...)` returns a FIXED-SIZE
    // `java.util.Arrays$ArrayList` backed directly by the supplied array — not a
    // resizable `java.util.ArrayList`. The previous synthetic copy reported
    // `getClass() == java.util.ArrayList`, was `instanceof ArrayList`, and let
    // `add()` succeed, all of which diverge from real-JDK (SpEL
    // IndexingTests.indexIntoPropertyContainingListOfList asserts the exact
    // runtime type `java.util.Arrays$ArrayList<...>`). Delegate to the real
    // nested-class constructor (`Arrays$ArrayList(E[])`, which stores the array
    // and derives `size()` from `a.length`); all operations then run real
    // bytecode, so `getClass()`/`instanceof`/`set` (OK) and `add`/`remove`
    // (UnsupportedOperationException) match HotSpot. This is exactly what the
    // real `Arrays.asList` bytecode does (`new Arrays$ArrayList; invokespecial`).
    let arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        // `Arrays.asList((Object[]) null)` is `new ArrayList<>(a)`, whose ctor
        // is `Objects.requireNonNull(array)`. An empty fixed-size list here is
        // the fabricated-success shape: the caller gets a usable list and never
        // learns its argument was null. A MISSING argument is a malformed
        // native call and keeps the defensive empty list.
        Some(Value::Object(None)) => return Err(bare_npe()),
        _ => alloc_ref_array(ctx, 0),
    };
    let arr_pin = ctx.pin_native_root(arr);
    let list = ctx.new_object_initialized(
        "java/util/Arrays$ArrayList",
        "([Ljava/lang/Object;)V",
        &[Value::Object(Some(arr))],
    )?;
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(arr_pin);
    // "…and `Arrays$ArrayList(E[])` stores the array" is true of the REAL
    // nested class only. In a synthetic-library build there is no
    // `java/util/Arrays$ArrayList` class file: the name is fabricated by
    // `ensure_synthetic_class`, its declared constructor set is empty, and the
    // `invokespecial` above therefore stores nothing at all. Every accessor
    // then read a backing array that was never written —
    // `arrays_array_list_backing` returned `None`, `size()` answered 0, and
    // `Arrays.asList("x","y","z").size() != 3` was pinned in
    // `KNOWN_SYNTHETIC_JDK_GAPS` as a missing `Arrays.asList`. The method was
    // not missing; the constructor delegation this native was rewritten to use
    // only carries the array on the real-JDK path.
    //
    // Storing it here covers both paths without a native `<init>` on a real
    // JDK class (which would shadow the real constructor for every instance):
    // on the real-JDK path the constructor has already run, the read below
    // finds the array, and this is a no-op.
    if let Some(Value::Object(Some(l))) = list {
        if arrays_array_list_backing(ctx, l).is_none() {
            ctx.set_field_by_name(l, "a", Value::Object(Some(arr)));
        }
    }
    Ok(list)
}

pub(crate) fn arrays_array_list_backing(ctx: &mut dyn NativeContext, list: ObjectRef) -> Option<ObjectRef> {
    if let Value::Object(Some(arr)) = ctx.get_field_by_name(list, "a") {
        if ctx.heap_kind_of(arr) == ObjectKind::Array {
            return Some(arr);
        }
    }
    let slot = ctx
        .resolve_field_index("java/util/Arrays$ArrayList", "a")
        .unwrap_or(0);
    if slot < ctx.object_num_fields(list) {
        if let Value::Object(Some(arr)) = ctx.get_field(list, slot) {
            if ctx.heap_kind_of(arr) == ObjectKind::Array {
                return Some(arr);
            }
        }
    }
    None
}

pub(crate) fn native_arrays_array_list_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let size = arrays_array_list_backing(ctx, this)
        .map(|arr| ctx.array_length(arr) as i32)
        .unwrap_or(0);
    Ok(Some(Value::Int(size)))
}

pub(crate) fn native_arrays_array_list_is_empty(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match native_arrays_array_list_size(ctx, args)? {
        Some(Value::Int(n)) => Ok(Some(Value::Int(i32::from(n == 0)))),
        _ => Ok(Some(Value::Int(1))),
    }
}

pub(crate) fn native_arrays_array_list_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let Some(arr) = arrays_array_list_backing(ctx, this) else {
        return Ok(Some(Value::Object(None)));
    };
    if index < 0 || index as usize >= ctx.array_length(arr) {
        // `Arrays$ArrayList` has no bounds check of its own: it indexes `a[i]`
        // directly, so what a caller sees is the ARRAY access failing. Hence
        // the subclass, and hence the array-access wording rather than the
        // index-only one.
        return Err(RuntimeError::aioobe(index, ctx.array_length(arr) as i32).into());
    }
    Ok(Some(ctx.get_array_element(arr, index as usize)))
}

pub(crate) fn native_arrays_array_list_iterator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let empty = alloc_ref_array(ctx, 0);
            return make_iterator_from_array(ctx, empty, 0);
        }
    };
    let arr: ObjectRef =
        arrays_array_list_backing(ctx, this).unwrap_or_else(|| alloc_ref_array(ctx, 0));
    let len = ctx.array_length(arr);
    make_iterator_from_array(ctx, arr, len)
}

// ===========================================================================
// Tests
// ===========================================================================
// java.util.Optional
// ===========================================================================

pub(crate) const OPT_FIELD_VALUE: usize = 0;
pub(crate) const OPT_NUM_FIELDS: usize = 1;

pub(crate) fn opt_value_is_empty(value: Value) -> bool {
    matches!(
        value,
        Value::Object(None) | Value::Int(0) | Value::Long(0) | Value::Uninitialized
    )
}

// Primitive Optionals (`OptionalInt`/`OptionalLong`/`OptionalDouble`) do NOT
// share the generic `Optional` layout. The real JDK classes declare
// `boolean isPresent` first (field 0) and the primitive `value` second
// (field 1); emptiness is the `isPresent` flag, not a null value. Our natives
// previously stored the value at field 0 (the generic-Optional convention),
// which is invisible-but-wrong while a *native* getter reads it back, but
// breaks the moment the real JDK `getAsInt()/getAsLong()/getAsDouble()`
// bytecode runs (it reads `value` at field 1) — e.g. `IntStream.average()
// .getAsDouble()` returned 0.0. Use the real 2-field layout everywhere so both
// native and real-bytecode consumers agree.
pub(crate) const OPT_PRIM_FIELD_PRESENT: usize = 0;
pub(crate) const OPT_PRIM_FIELD_VALUE: usize = 1;
pub(crate) const OPT_PRIM_NUM_FIELDS: usize = 2;

/// Build an `OptionalInt`/`OptionalLong`/`OptionalDouble` with the real JDK
/// field layout. `value = Some(v)` → present; `None` → empty.
pub(crate) fn make_opt_prim(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    value: Option<Value>,
) -> Result<ObjectRef, MethodCallFailed> {
    let opt = try_alloc_synthetic(ctx, class_name, OPT_PRIM_NUM_FIELDS)?;
    match value {
        Some(v) => {
            ctx.set_field(opt, OPT_PRIM_FIELD_PRESENT, Value::Int(1));
            ctx.set_field(opt, OPT_PRIM_FIELD_VALUE, v);
        }
        None => {
            ctx.set_field(opt, OPT_PRIM_FIELD_PRESENT, Value::Int(0));
        }
    }
    Ok(opt)
}

/// Mark a primitive Optional present and store its value (real layout).
pub(crate) fn set_opt_prim_value(ctx: &mut dyn NativeContext, opt: ObjectRef, value: Value) {
    ctx.set_field(opt, OPT_PRIM_FIELD_PRESENT, Value::Int(1));
    ctx.set_field(opt, OPT_PRIM_FIELD_VALUE, value);
}

/// Read the value of a primitive Optional, honouring `isPresent` (field 0).
pub(crate) fn opt_prim_value(ctx: &dyn NativeContext, this: ObjectRef) -> Option<Value> {
    if matches!(ctx.get_field(this, OPT_PRIM_FIELD_PRESENT), Value::Int(1)) {
        Some(ctx.get_field(this, OPT_PRIM_FIELD_VALUE))
    } else {
        None
    }
}

/// The `Optional` members that call back into user code (a `Function`,
/// `Predicate`, `Consumer`, `Runnable` or `Supplier`). Every one is a row of
/// `retired_shadow.rs`'s `java/util/Optional` table, so `--jdk-only` already
/// refuses them.
pub(crate) const OPTIONAL_FUNCTIONAL_ROWS: &[(&str, &str)] = &[
    ("ifPresent", "(Ljava/util/function/Consumer;)V"),
    ("map", "(Ljava/util/function/Function;)Ljava/util/Optional;"),
    (
        "flatMap",
        "(Ljava/util/function/Function;)Ljava/util/Optional;",
    ),
    (
        "filter",
        "(Ljava/util/function/Predicate;)Ljava/util/Optional;",
    ),
    (
        "orElseGet",
        "(Ljava/util/function/Supplier;)Ljava/lang/Object;",
    ),
    (
        "ifPresentOrElse",
        "(Ljava/util/function/Consumer;Ljava/lang/Runnable;)V",
    ),
    ("or", "(Ljava/util/function/Supplier;)Ljava/util/Optional;"),
    (
        "orElseThrow",
        "(Ljava/util/function/Supplier;)Ljava/lang/Object;",
    ),
];

/// Is this `Optional` row left to the real class bytes instead of registered?
///
/// Round 14 wave 2 (lane compat2), the `Optional.map` addendum on
/// `r13w13-trace3-vm-served-jdk-methods-residuals-FIXED-20260929.md`: a native that
/// calls the user's lambda through `invoke_virtual` leaves NO
/// `java.util.Optional.map(Optional.java:..)` frame between the caller and
/// the lambda, so a trace / `StackWalker` / `getStackTrace()` taken inside
/// the lambda is one frame short of HotSpot's, in `--compatible` and even
/// under `--nojit`. The bodies are a null test and one call, so nothing is
/// gained by serving them natively (AGENTS.md: real class bytes are
/// authoritative). A correctness fix, so on in `--compatible` too (owner
/// decision 2026-09-27); `--jdk-only` is unchanged (the retirement table
/// already refuses these rows there), and the synthetic JDK, which has no
/// bytecode for them, keeps them.
///
/// Kill switch `CRATONVM_COMPAT_OPTIONAL_REAL_BYTECODE=0` registers all eight
/// again; `CRATONVM_UNRETIRE_NATIVE_SHADOW` (`java/util/Optional`, or one
/// row) does the same per row. Read once per registry population (boot), so
/// no cache.
pub(crate) fn optional_functional_left_to_bytecode(
    r: &NativeMethodRegistry,
    method: &str,
    descriptor: &str,
) -> bool {
    r.real_jdk()
        && !r.compatibility_mode().is_jdk_only()
        && OPTIONAL_FUNCTIONAL_ROWS
            .iter()
            .any(|(m, d)| *m == method && *d == descriptor)
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_COMPAT_OPTIONAL_REAL_BYTECODE")
        && !cratonvm_native_api::unretire::is_excluded("java/util/Optional", method, descriptor)
}

pub(crate) fn register_optional_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let o = "java/util/Optional";
    r.register(o, "empty", "()Ljava/util/Optional;", native_opt_empty);
    r.register(
        o,
        "of",
        "(Ljava/lang/Object;)Ljava/util/Optional;",
        native_opt_of,
    );
    r.register(
        o,
        "ofNullable",
        "(Ljava/lang/Object;)Ljava/util/Optional;",
        native_opt_of_nullable,
    );
    r.register(o, "get", "()Ljava/lang/Object;", native_opt_get);
    r.register(o, "isPresent", "()Z", native_opt_is_present);
    r.register(o, "isEmpty", "()Z", native_opt_is_empty);
    r.register(
        o,
        "orElse",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_opt_or_else,
    );
    r.register(
        o,
        "orElseThrow",
        "()Ljava/lang/Object;",
        native_opt_or_else_throw,
    );
    r.register(o, "equals", "(Ljava/lang/Object;)Z", native_opt_equals);
    r.register(o, "hashCode", "()I", native_opt_hash_code);
    r.register(o, "toString", "()Ljava/lang/String;", native_opt_to_string);
    if !optional_functional_left_to_bytecode(r, "ifPresent", "(Ljava/util/function/Consumer;)V") {
        r.register(
            o,
            "ifPresent",
            "(Ljava/util/function/Consumer;)V",
            native_opt_if_present,
        );
    }
    if !optional_functional_left_to_bytecode(
        r,
        "map",
        "(Ljava/util/function/Function;)Ljava/util/Optional;",
    ) {
        r.register(
            o,
            "map",
            "(Ljava/util/function/Function;)Ljava/util/Optional;",
            native_opt_map,
        );
    }
    if !optional_functional_left_to_bytecode(
        r,
        "flatMap",
        "(Ljava/util/function/Function;)Ljava/util/Optional;",
    ) {
        r.register(
            o,
            "flatMap",
            "(Ljava/util/function/Function;)Ljava/util/Optional;",
            native_opt_flat_map,
        );
    }
    if !optional_functional_left_to_bytecode(
        r,
        "filter",
        "(Ljava/util/function/Predicate;)Ljava/util/Optional;",
    ) {
        r.register(
            o,
            "filter",
            "(Ljava/util/function/Predicate;)Ljava/util/Optional;",
            native_opt_filter,
        );
    }
    if !optional_functional_left_to_bytecode(
        r,
        "orElseGet",
        "(Ljava/util/function/Supplier;)Ljava/lang/Object;",
    ) {
        r.register(
            o,
            "orElseGet",
            "(Ljava/util/function/Supplier;)Ljava/lang/Object;",
            native_opt_or_else_get,
        );
    }
    if !optional_functional_left_to_bytecode(
        r,
        "ifPresentOrElse",
        "(Ljava/util/function/Consumer;Ljava/lang/Runnable;)V",
    ) {
        r.register(
            o,
            "ifPresentOrElse",
            "(Ljava/util/function/Consumer;Ljava/lang/Runnable;)V",
            native_opt_if_present_or_else,
        );
    }
    if !optional_functional_left_to_bytecode(
        r,
        "or",
        "(Ljava/util/function/Supplier;)Ljava/util/Optional;",
    ) {
        r.register(
            o,
            "or",
            "(Ljava/util/function/Supplier;)Ljava/util/Optional;",
            native_opt_or,
        );
    }
    r.register(
        o,
        "stream",
        "()Ljava/util/stream/Stream;",
        native_opt_stream,
    );
    if !optional_functional_left_to_bytecode(
        r,
        "orElseThrow",
        "(Ljava/util/function/Supplier;)Ljava/lang/Object;",
    ) {
        r.register(
            o,
            "orElseThrow",
            "(Ljava/util/function/Supplier;)Ljava/lang/Object;",
            native_opt_or_else_throw_supplier,
        );
    }
    r.set_category(__prev_cat);
}

#[cfg(test)]
mod optional_functional_rows_tests {
    use super::{register_optional_natives, OPTIONAL_FUNCTIONAL_ROWS};
    use cratonvm_native_api::NativeMethodRegistry;
    use cratonvm_types::compat::CompatibilityMode;

    const OPT: &str = "java/util/Optional";

    /// `row`: the `jdk-optional` HotSpot-alignment row (nio20 wave 1, default
    /// on), which drops every retired `Optional` row on a real JDK.
    fn registry_with_row(real_jdk: bool, row: bool) -> NativeMethodRegistry {
        let mut r = NativeMethodRegistry::new();
        r.set_compatibility_mode(CompatibilityMode::Compatible);
        r.set_hotspot_alignments(
            cratonvm_types::HotSpotAlignments::defaults()
                .with(cratonvm_types::HotSpotAlignment::JdkOptional, row),
        );
        r.set_real_jdk(real_jdk);
        register_optional_natives(&mut r);
        r
    }

    /// `--compatible` on a real JDK with `-jdk-optional`: the eight callback
    /// rows run the class's bytecode (so `Optional.map` keeps its frame); the
    /// leaf rows stay.
    #[test]
    fn compatible_on_a_real_jdk_leaves_the_callback_rows_to_bytecode() {
        if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_COMPAT_OPTIONAL_REAL_BYTECODE")
        {
            return; // switched off in this environment
        }
        let r = registry_with_row(true, false);
        for (m, d) in OPTIONAL_FUNCTIONAL_ROWS {
            if cratonvm_native_api::unretire::is_excluded(OPT, m, d) {
                continue;
            }
            assert!(r.find(OPT, m, d).is_none(), "{m}{d} is still registered");
        }
        assert!(r.find(OPT, "isPresent", "()Z").is_some());
        assert!(r
            .find(
                OPT,
                "ofNullable",
                "(Ljava/lang/Object;)Ljava/util/Optional;"
            )
            .is_some());
    }

    /// The default `jdk-optional` row (nio20 wave 1): the whole family is the
    /// JDK's, leaf rows included (both are retired rows).
    #[test]
    fn the_jdk_optional_row_leaves_the_leaf_rows_to_bytecode_too() {
        for (m, d) in [
            ("isPresent", "()Z"),
            ("ofNullable", "(Ljava/lang/Object;)Ljava/util/Optional;"),
        ] {
            if cratonvm_native_api::unretire::is_excluded(OPT, m, d) {
                continue;
            }
            assert!(
                registry_with_row(true, true).find(OPT, m, d).is_none(),
                "{m}{d}"
            );
            // Without a real JDK the row cannot apply.
            assert!(
                registry_with_row(false, true).find(OPT, m, d).is_some(),
                "{m}{d}"
            );
        }
    }

    /// The synthetic JDK has no bytecode for them and keeps all eight, row or
    /// no row.
    #[test]
    fn the_synthetic_jdk_keeps_the_callback_rows() {
        for row in [false, true] {
            let r = registry_with_row(false, row);
            for (m, d) in OPTIONAL_FUNCTIONAL_ROWS {
                assert!(r.find(OPT, m, d).is_some(), "{m}{d} is missing");
            }
        }
    }
}

/// Round 14 wave 3 (lane compat3), CP2-2: `LinkedHashSet.reversed()` is left
/// to the JDK's live view on a real JDK in `--compatible`
/// (`CRATONVM_COMPAT_LHS_REVERSED_REAL_VIEW`).
#[cfg(test)]
mod lhs_reversed_registration_tests {
    use super::register_hashset_natives;
    use cratonvm_native_api::NativeMethodRegistry;
    use cratonvm_types::compat::CompatibilityMode;

    const LHS: &str = "java/util/LinkedHashSet";
    const REVERSED: &str = "()Ljava/util/SequencedSet;";

    /// The `jdk-hash-collections` row off: these tests are about the one row
    /// `--compatible` left to bytecode before that row adopted the family.
    fn registry(real_jdk: bool) -> NativeMethodRegistry {
        registry_with_row(real_jdk, false)
    }

    fn registry_with_row(real_jdk: bool, row: bool) -> NativeMethodRegistry {
        let mut r = NativeMethodRegistry::new();
        r.set_compatibility_mode(CompatibilityMode::Compatible);
        r.set_hotspot_alignments(
            cratonvm_types::HotSpotAlignments::defaults()
                .with(cratonvm_types::HotSpotAlignment::JdkHashCollections, row),
        );
        r.set_real_jdk(real_jdk);
        register_hashset_natives(&mut r);
        r
    }

    /// hcb: with the `jdk-hash-collections` row the source members go too
    /// (the whole family runs the JDK's bodies, as under `--jdk-only`).
    #[test]
    fn the_hash_collection_row_leaves_the_members_to_the_jdk_too() {
        if cratonvm_native_api::unretire::is_excluded(LHS, "addFirst", "(Ljava/lang/Object;)V") {
            return; // switched off in this environment
        }
        let r = registry_with_row(true, true);
        assert!(r.find(LHS, "reversed", REVERSED).is_none());
        assert!(r.find(LHS, "addFirst", "(Ljava/lang/Object;)V").is_none());
        // Without a real JDK the row cannot apply.
        let r = registry_with_row(false, true);
        assert!(r.find(LHS, "reversed", REVERSED).is_some());
    }

    /// The snapshot is not registered, so the real `ReverseLinkedHashSetView`
    /// runs; the source natives it delegates to stay registered.
    #[test]
    fn compatible_on_a_real_jdk_leaves_reversed_to_the_jdk_view() {
        if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_COMPAT_LHS_REVERSED_REAL_VIEW")
            || cratonvm_native_api::unretire::is_excluded(LHS, "reversed", REVERSED)
        {
            return; // switched off in this environment
        }
        let r = registry(true);
        assert!(r.find(LHS, "reversed", REVERSED).is_none());
        for (m, d) in [
            ("addFirst", "(Ljava/lang/Object;)V"),
            ("getLast", "()Ljava/lang/Object;"),
            ("removeFirst", "()Ljava/lang/Object;"),
            ("size", "()I"),
        ] {
            assert!(r.find(LHS, m, d).is_some(), "{m}{d} is missing");
        }
    }

    /// The synthetic JDK has no bytecode for it and keeps the snapshot.
    #[test]
    fn the_synthetic_jdk_keeps_the_reversed_snapshot() {
        let r = registry(false);
        assert!(r.find(LHS, "reversed", REVERSED).is_some());
    }
}

/// Round 14 wave 4 (lane compat4), C3-2: `LinkedHashMap.reversed()` is left
/// to the JDK's `ReversedLinkedHashMapView` on a real JDK in `--compatible`
/// (`CRATONVM_COMPAT_LHM_REVERSED_REAL_VIEW`, which needs
/// `CRATONVM_COMPAT_LHM_REVERSED_LIVE_VIEWS` for the view's `entrySet()`).
#[cfg(test)]
mod lhm_reversed_registration_tests {
    use super::register_linked_hashmap_natives;
    use cratonvm_native_api::NativeMethodRegistry;
    use cratonvm_types::compat::CompatibilityMode;

    const LHM: &str = "java/util/LinkedHashMap";
    const REVERSED: &str = "()Ljava/util/SequencedMap;";

    /// The `jdk-hash-collections` row off, as in `lhs_reversed_registration_tests`.
    fn registry(real_jdk: bool, mode: CompatibilityMode) -> NativeMethodRegistry {
        let mut r = NativeMethodRegistry::new();
        r.set_compatibility_mode(mode);
        r.set_hotspot_alignments(
            cratonvm_types::HotSpotAlignments::defaults()
                .with(cratonvm_types::HotSpotAlignment::JdkHashCollections, false),
        );
        r.set_real_jdk(real_jdk);
        register_linked_hashmap_natives(&mut r);
        r
    }

    fn switched_on() -> bool {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_COMPAT_LHM_REVERSED_REAL_VIEW")
            && cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_COMPAT_LHM_REVERSED_LIVE_VIEWS",
            )
            && !cratonvm_native_api::unretire::is_excluded(LHM, "reversed", REVERSED)
    }

    /// The snapshot is not registered, so the real view runs; the members it
    /// delegates to, and the view carriers' own `reversed()`, stay native.
    #[test]
    fn compatible_on_a_real_jdk_leaves_reversed_to_the_jdk_view() {
        if !switched_on() {
            return; // switched off in this environment
        }
        let r = registry(true, CompatibilityMode::Compatible);
        assert!(r.find(LHM, "reversed", REVERSED).is_none());
        for (c, m, d) in [
            (LHM, "sequencedKeySet", "()Ljava/util/SequencedSet;"),
            (LHM, "sequencedEntrySet", "()Ljava/util/SequencedSet;"),
            (
                LHM,
                "putFirst",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            ),
            (LHM, "pollLastEntry", "()Ljava/util/Map$Entry;"),
            (
                "java/util/LinkedHashMap$LinkedKeySet",
                "reversed",
                "()Ljava/util/SequencedSet;",
            ),
            (
                "java/util/LinkedHashMap$LinkedEntrySet",
                "reversed",
                "()Ljava/util/SequencedSet;",
            ),
        ] {
            assert!(r.find(c, m, d).is_some(), "{c}.{m}{d} is missing");
        }
    }

    /// The synthetic JDK has no bytecode for the view and keeps the snapshot.
    #[test]
    fn the_synthetic_jdk_keeps_the_reversed_map_snapshot() {
        let r = registry(false, CompatibilityMode::Compatible);
        assert!(r.find(LHM, "reversed", REVERSED).is_some());
    }
}

/// `Optional.empty()` must return the *same* instance on every call: the real
/// JDK is `(Optional<T>) EMPTY`, so `Optional.empty() == Optional.empty()` holds
/// by identity. Spring's `GenericConversionService`/`ObjectToOptionalConverter`
/// produce `Optional.empty()` for null/empty conversions and callers assert
/// `isSameAs(Optional.empty())` (an identity check). Allocating a fresh synthetic
/// per call broke that identity. We cache the singleton in Optional's own
/// `EMPTY` static field (a GC root) so it survives collection and stays stable.
pub(crate) fn opt_empty_singleton(ctx: &mut dyn NativeContext) -> Result<Value, MethodCallFailed> {
    if let Some(cid) = ctx.class_id_by_name("java/util/Optional") {
        let _ = ctx.ensure_class_initialized("java/util/Optional");
        if let Some(idx) = ctx.static_field_index_by_name(cid, "EMPTY") {
            if let v @ Value::Object(Some(_)) = ctx.get_static_field(cid, idx) {
                // Already populated (by Optional.<clinit> `new Optional<>(null)`
                // or a prior empty() call) — return the existing singleton.
                return Ok(v);
            }
            // EMPTY not yet set: allocate once and store it back so every
            // subsequent empty() (and our clinit fallback) shares this object.
            let opt = try_alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS)?;
            ctx.set_static_field(cid, idx, Value::Object(Some(opt)));
            return Ok(Value::Object(Some(opt)));
        }
    }
    // Fallback (no EMPTY field, e.g. a fabricated synthetic Optional): fresh
    // instance — identity is not guaranteed, but value semantics hold.
    let opt = try_alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS)?;
    Ok(Value::Object(Some(opt)))
}

pub(crate) fn native_opt_empty(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(opt_empty_singleton(ctx)?))
}

pub(crate) fn native_opt_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match args.first() {
        Some(val) if opt_value_is_empty(*val) => {
            Err(cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into())
        }
        None => {
            Err(cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into())
        }
        Some(val) => {
            let opt = try_alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS)?;
            ctx.set_field(opt, OPT_FIELD_VALUE, *val);
            Ok(Some(Value::Object(Some(opt))))
        }
    }
}

pub(crate) fn native_opt_of_nullable(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = args.first().copied().unwrap_or(Value::Object(None));
    // Real JDK: `ofNullable(null)` is `empty()` — return the shared singleton.
    if opt_value_is_empty(val) {
        return Ok(Some(opt_empty_singleton(ctx)?));
    }
    let opt = try_alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS)?;
    ctx.set_field(opt, OPT_FIELD_VALUE, val);
    Ok(Some(Value::Object(Some(opt))))
}

pub(crate) fn native_opt_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    match val {
        _ if opt_value_is_empty(val) => Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "No value present".to_string(),
            }
            .into(),
        ),
        _ => Ok(Some(val)),
    }
}

pub(crate) fn native_opt_is_present(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    Ok(Some(Value::Int(if opt_value_is_empty(val) {
        0
    } else {
        1
    })))
}

pub(crate) fn native_opt_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(1))),
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    Ok(Some(Value::Int(if opt_value_is_empty(val) {
        1
    } else {
        0
    })))
}

pub(crate) fn native_opt_or_else(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let default = args.get(1).copied().unwrap_or(Value::Object(None));
            return Ok(Some(default));
        }
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    if opt_value_is_empty(val) {
        let default = args.get(1).copied().unwrap_or(Value::Object(None));
        Ok(Some(default))
    } else {
        Ok(Some(val))
    }
}

pub(crate) fn native_opt_or_else_throw(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    if opt_value_is_empty(val) {
        Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "No value present".to_string(),
            }
            .into(),
        )
    } else {
        Ok(Some(val))
    }
}

pub(crate) fn native_opt_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Real `Optional.equals`: `this == obj` short-circuit.
    if this.as_ptr() == other.as_ptr() {
        return Ok(Some(Value::Int(1)));
    }
    // `obj instanceof Optional` — `Optional` is final, so identical class id
    // is the exact instanceof test. A non-Optional `other` is not equal (and
    // reading its slot 0 would be meaningless).
    if ctx.class_id_of_object(this) != ctx.class_id_of_object(other) {
        return Ok(Some(Value::Int(0)));
    }
    let this_val = ctx.get_field(this, OPT_FIELD_VALUE);
    let other_val = ctx.get_field(other, OPT_FIELD_VALUE);
    // `Objects.equals(value, other.value)` — must dispatch to the contained
    // value's *real* `equals`, not the structural `values_equal` (which only
    // knows identity/String/enum/wrapper and so reports value-equal lists,
    // records, and user value-classes as unequal). Mirrors the real-JDK
    // `Optional.equals` bytecode, which routes through `Objects.equals`.
    let eq = opt_value_equals(ctx, this_val, other_val)?;
    Ok(Some(Value::Int(if eq { 1 } else { 0 })))
}

/// Null-safe `Objects.equals(a, b)`: both-null → true; one-null → false;
/// otherwise dispatch to `a.equals(b)` so the contained value's real
/// equality contract is honoured.
pub(crate) fn opt_value_equals(
    ctx: &mut dyn NativeContext,
    a: Value,
    b: Value,
) -> Result<bool, MethodCallFailed> {
    match (a, b) {
        (x, y) if opt_value_is_empty(x) && opt_value_is_empty(y) => Ok(true),
        (x, y) if opt_value_is_empty(x) || opt_value_is_empty(y) => Ok(false),
        (Value::Object(Some(ra)), Value::Object(Some(rb))) => {
            if ra.as_ptr() == rb.as_ptr() {
                return Ok(true);
            }
            match ctx.invoke_virtual(
                ra,
                "equals",
                "(Ljava/lang/Object;)Z",
                &[Value::Object(Some(rb))],
            )? {
                Some(Value::Int(v)) => Ok(v != 0),
                _ => Ok(false),
            }
        }
        // Primitive-valued Optionals never occur (Optional<T> boxes), but a
        // structural compare is exact for any non-object slot.
        (x, y) => Ok(x == y),
    }
}

pub(crate) fn native_opt_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    // Real `Optional.hashCode` is `Objects.hashCode(value)` =
    // `value == null ? 0 : value.hashCode()`. Dispatch to the value's real
    // `hashCode` rather than returning its identity hash (which made
    // `Optional.hashCode` non-value-based and broke hash-keyed lookups).
    match val {
        _ if opt_value_is_empty(val) => Ok(Some(Value::Int(0))),
        Value::Object(Some(obj)) => match ctx.invoke_virtual(obj, "hashCode", "()I", &[])? {
            Some(Value::Int(h)) => Ok(Some(Value::Int(h))),
            _ => Ok(Some(Value::Int(0))),
        },
        _ => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_opt_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let s = ctx.create_string("Optional.empty");
            return Ok(Some(Value::Object(Some(s))));
        }
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    let s = match val {
        _ if opt_value_is_empty(val) => units_of("Optional.empty"),
        _ => {
            let display = obj_to_display_units(ctx, &val)?;
            let mut out = units_of("Optional[");
            out.extend_from_slice(&display);
            out.push(u16::from(b']'));
            out
        }
    };
    Ok(Some(Value::Object(Some(ctx.create_string_from_units(&s)))))
}

// ===========================================================================
// Arrays.sort / fill / binarySearch / equals (added to register_arrays_natives)
// ===========================================================================

pub(crate) fn native_arrays_sort_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `Arrays.sort(null)` reads `a.length` on its first line, so it is an NPE
    // and not the silent no-op this body used to answer.
    // MEASURED: `apps/probes/NullArgMsgProbe.java` row 56.
    if matches!(args.first(), Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException {
            message: Some("Cannot read the array length because \"a\" is null".to_string()),
        }
        .into());
    }
    let arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(None),
    };
    let len = ctx.array_length(arr);
    let mut vals: Vec<i32> = Vec::with_capacity(len);
    for i in 0..len {
        match ctx.get_array_element(arr, i) {
            Value::Int(v) => vals.push(v),
            _ => vals.push(0),
        }
    }
    vals.sort();
    for (i, &v) in vals.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(v));
    }
    Ok(None)
}

pub(crate) fn native_arrays_sort_objects(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(None),
    };
    if ctx.heap_kind_of(arr) != ObjectKind::Array {
        // Some early bootstrap paths (ClassWorlds / Maven launcher) can
        // route a List receiver into Arrays.sort native overrides. Coerce an
        // ArrayList receiver to its backing elementData to preserve behavior.
        let (data, _size) = al_state(ctx, arr);
        if let Some(backing) = data {
            arr = backing;
        } else {
            return Ok(None);
        }
    }
    let len = ctx.array_length(arr);

    // MED fix: real JDK `Arrays.sort(Object[])` orders elements by
    // natural comparison (`Comparable.compareTo`), and throws
    // `ClassCastException` for elements that don't implement Comparable.
    // The previous implementation sorted by `read_string()` representation,
    // which silently produced wrong order for any type whose toString does
    // not match its natural ordering (Integer, Date, custom types) and
    // happily sorted non-Comparable objects without complaint.
    //
    // We now:
    //   1. Snapshot the array into a Vec<Value>.
    //   2. Verify every non-null element is Comparable; throw
    //      ClassCastException on the first non-Comparable element.
    //   3. Sort via a stable merge sort (O(n log n)), dispatching through
    //      `Comparable.compareTo(Object)` for every pair-wise comparison.
    //      The merge is fallible: a thrown compareTo propagates out cleanly
    //      and short-circuits the sort. We can't use Rust's stable `sort_by`
    //      because its comparator is infallible, so we run our own stable
    //      merge sort (`merge_sort_fallible`) that threads the error through.
    //
    // Null elements are permitted (JDK sorts them as if smaller than any
    // non-null element when the comparator is null, but throws NPE when
    // comparing null via compareTo). To match JDK behaviour we propagate
    // the NPE that compareTo would naturally throw if a null sneaks in.

    let mut items: Vec<Value> = Vec::with_capacity(len);
    for i in 0..len {
        items.push(ctx.get_array_element(arr, i));
    }

    // Verify Comparable on every non-null element.
    for v in &items {
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
    // GC-SAFETY (Family-1 stale-ObjectRef fix, mirroring native_collections_sort's
    // established idiom): `compare_via_compare_to` dispatches the elements'
    // `compareTo` via `invoke_virtual` up to O(n log n) times, and each dispatch
    // can run arbitrary user bytecode and trigger a moving GC. `arr` was held
    // raw across the whole sort and reused directly afterward by the
    // `set_array_element` write-back loop below -- and every entry in `items`
    // is a raw `ObjectRef` snapshot taken once before the sort and never
    // refreshed, so a GC triggered by comparing any OTHER pair can relocate an
    // element already sitting in `items`, leaving both the eventual write-back
    // and the *next* `compare_via_compare_to` dispatch on that element
    // operating on a stale/dangling reference. Pin `arr` and every element (via
    // the same index-array trick as `native_collections_sort`, which keeps
    // `items`/`elem_handles` index-stable while only the `idx` permutation is
    // shuffled by the merge), refresh each right before use.
    let arr_pin = ctx.pin_native_root(arr);
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
        // JDK natural-order sorting probes the right run against the left
        // while identifying/merging ordered runs. That is equivalent for a
        // normal antisymmetric Comparable, but matters when an inherited
        // interface default returns 0 and a concrete peer supplies ordering.
        compare_via_compare_to(c, &eb, &ea).map(i32::saturating_neg)
    });
    if let Err(e) = sort_result {
        ctx.unpin_native_roots(arr_pin);
        return Err(e);
    }
    let arr = ctx.read_native_pin(arr_pin, arr);
    for (out, slot) in idx.iter().enumerate() {
        let i = if let Value::Int(v) = slot {
            *v as usize
        } else {
            0
        };
        let val = read_pinned_elem(ctx, elem_handles[i], items[i]);
        ctx.set_array_element(arr, out, val);
    }
    ctx.unpin_native_roots(arr_pin);
    Ok(None)
}

/// Walk `obj`'s class hierarchy (including superclasses) and return true if
/// any class implements `java/lang/Comparable` directly or transitively
/// via a super-interface. Used by `native_arrays_sort_objects` to throw
/// `ClassCastException` before invoking `compareTo` on a non-Comparable.
pub(crate) fn implements_comparable(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    let mut cid = ctx.class_id_of_object(obj);
    for _ in 0..64 {
        // Lambda proxy classes live outside `class_manager`, so their direct
        // interfaces must be resolved through the lambda registry first.
        if let Some(iface_name) = ctx.lambda_functional_interface(cid) {
            if let Some(iface) = ctx.class_id_by_name(&iface_name) {
                if iface_extends_comparable(ctx, iface) {
                    return true;
                }
            }
        }
        for iface in ctx.class_interfaces(cid) {
            if iface_extends_comparable(ctx, iface) {
                return true;
            }
        }
        match ctx.superclass_of(cid) {
            Some(p) => cid = p,
            None => break,
        }
    }
    false
}

/// `CRATONVM_DBG_CCE_BT` — when set, print a native backtrace plus operand
/// identity at the moment a natural-order `ClassCastException` is
/// constructed. Attribution tool for the WildFly `parallel-extension-add`
/// stale-object CCE family (`class java.lang.Object cannot be cast to class
/// java.lang.Comparable` with no useful Java frames): identifies the exact
/// native caller (sort / tree / min-max path) and the victim object's
/// address so it can be correlated against GC cycle logs and the
/// `CRATONVM_DBG_STALE_OBJREF` quarantine. Default-off, one cached-bool
/// branch when unset.
pub(crate) fn dbg_cce_backtrace(site: &str, ctx: &dyn NativeContext, ao: ObjectRef, bo: Option<ObjectRef>) {
    static E: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*E.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_CCE_BT").is_some()) {
        return;
    }
    let a_name = object_class_name(ctx, ao);
    let a_cid = ctx.class_id_of_object(ao);
    let b_desc = bo
        .map(|b| {
            format!(
                "{}(cid={}) @0x{:x}",
                object_class_name(ctx, b),
                ctx.class_id_of_object(b).as_u32(),
                b.as_ptr() as usize
            )
        })
        .unwrap_or_else(|| "<none>".to_string());
    eprintln!(
        "CRATONVM_DBG_CCE_BT: site={site} a={a_name}(cid={}) @0x{:x} b={b_desc}\n{}",
        a_cid.as_u32(),
        ao.as_ptr() as usize,
        std::backtrace::Backtrace::force_capture()
    );
}

/// Return a useful identity for diagnostics, including hidden lambda proxies,
/// in internal form (`a/b/C`).
pub(crate) fn object_class_name(ctx: &dyn NativeContext, obj: ObjectRef) -> String {
    class_display_name(ctx, obj, false)
}

/// [`object_class_name`] in binary form (`a.b.C`), as HotSpot's
/// `ClassCastException` messages spell it. Use this instead of
/// `object_class_name(..).replace('/', ".")`, which also turned a lambda
/// name's `$$Lambda/0x..` separator into `$$Lambda.0x..`.
pub(crate) fn object_class_name_dotted(ctx: &dyn NativeContext, obj: ObjectRef) -> String {
    class_display_name(ctx, obj, true)
}

pub(crate) fn class_display_name(ctx: &dyn NativeContext, obj: ObjectRef, dotted: bool) -> String {
    let cid = ctx.class_id_of_object(obj);
    if let Some(name) = ctx.class_name_of_id(cid) {
        return if dotted { name.replace('/', ".") } else { name };
    }
    if let Some(host) = ctx.lambda_proxy_host(cid) {
        // HotSpot's hidden-class tail: `0x` plus 16 zero-padded hex digits,
        // the shape `lang_class::lambda_proxy_class_name` gives
        // `Class.getName()`. Only the host part is dotted; the `/` before the
        // tail belongs to the name.
        let host = if dotted { host.replace('/', ".") } else { host };
        return format!("{host}$$Lambda/0x{:016x}", cid.as_u32());
    }
    ctx.lambda_functional_interface(cid)
        .map(|iface| {
            let iface = if dotted {
                iface.replace('/', ".")
            } else {
                iface
            };
            format!("lambda implementing {iface}")
        })
        .unwrap_or_else(|| "<unknown>".to_string())
}

/// True iff `iface` IS `java/lang/Comparable` or transitively extends it.
pub(crate) fn iface_extends_comparable(ctx: &dyn NativeContext, iface: ClassId) -> bool {
    // BFS with a depth cap (interface graphs in real-world JDKs are shallow).
    let mut stack: Vec<ClassId> = vec![iface];
    let mut budget = 64usize;
    while let Some(c) = stack.pop() {
        if budget == 0 {
            break;
        }
        budget -= 1;
        if let Some(name) = ctx.class_name_of_id(c) {
            if name == "java/lang/Comparable" {
                return true;
            }
        }
        for super_iface in ctx.class_interfaces(c) {
            stack.push(super_iface);
        }
    }
    false
}

/// Dispatch through `Comparable.compareTo(Object)`. Treats `null` as
/// less than any non-null element (matching how the JDK's natural-order
/// comparator handles the corner case for `Arrays.sort(Object[])`).
pub(crate) fn compare_via_compare_to(
    ctx: &mut dyn NativeContext,
    a: &Value,
    b: &Value,
) -> Result<i32, MethodCallFailed> {
    match (a, b) {
        (Value::Object(None), Value::Object(None)) => Ok(0),
        (Value::Object(None), _) => Ok(-1),
        (_, Value::Object(None)) => Ok(1),
        (Value::Object(Some(ao)), Value::Object(Some(bo))) => {
            // The JDK's natural-ordering compare is `((Comparable) k1).compareTo(k2)`.
            // When the receiver `k1` does not implement `Comparable`, the bytecode
            // `checkcast java/lang/Comparable` throws `ClassCastException` *before*
            // `compareTo` is ever dispatched. Replicate that here: without this
            // guard the bare `invoke_virtual("compareTo")` below would raise a
            // `NoSuchMethodError` instead (the method simply isn't there), which is
            // the wrong exception type — callers that legitimately catch
            // `ClassCastException` (e.g. Spring's `SetDelegate.orderForCodeConsistency`,
            // which does `new TreeSet<>(set)` on a `Set<Class<?>>` and falls back to
            // the original set when the elements aren't comparable) would not catch a
            // `NoSuchMethodError` and the real failure would escape (spring-bug-04).
            if !implements_comparable(ctx, *ao) {
                let cname = object_class_name_dotted(ctx, *ao);
                dbg_cce_backtrace("compare_via_compare_to", ctx, *ao, Some(*bo));
                return Err(cratonvm_types::error::RuntimeError::ClassCastException {
                    message: format!("class {cname} cannot be cast to class java.lang.Comparable"),
                }
                .into());
            }
            let r = ctx.invoke_virtual(
                *ao,
                "compareTo",
                "(Ljava/lang/Object;)I",
                &[Value::Object(Some(*bo))],
            )?;
            match r {
                Some(Value::Int(v)) => Ok(v),
                _ => Err(cratonvm_types::error::RuntimeError::ClassCastException {
                    message: "compareTo did not return an int".to_string(),
                }
                .into()),
            }
        }
        // Non-object slots are unreachable in an Object[]; defensive fallback.
        _ => Ok(0),
    }
}

/// Stable, fallible merge sort — O(n log n).
///
/// Sorts `items` in place using `cmp`, a comparison closure that returns
/// `Ok(<0 | 0 | >0)` (the usual `compareTo`/`Comparator.compare` contract) or
/// an `Err` when the underlying Java comparison throws (e.g.
/// `ClassCastException` or a comparator-thrown exception). The first error
/// short-circuits the whole sort and is propagated to the caller, leaving
/// `items` in a partially-merged (but memory-safe) state — matching the
/// behaviour of the previous insertion sort, which also stopped on the first
/// `Err` mid-pass.
///
/// `ctx` is threaded through to the closure on each call so the comparison can
/// dispatch back into the VM (`invoke_virtual`) without the closure having to
/// borrow `ctx` itself. The merge is **stable**: when two elements compare
/// equal, the one originally earlier in `items` is kept earlier, which is the
/// contract `Collections.sort`/`Arrays.sort` guarantee and that the old
/// insertion sort provided (`cmp <= 0 => break`).
///
/// This is a classic bottom-up (iterative) merge sort to avoid recursion and
/// keep the borrow of `ctx`/`cmp` flat. The merge step copies the left run
/// into a scratch buffer and merges it back with the (untouched) right run,
/// preferring the left element on ties to preserve stability.
pub(crate) fn merge_sort_fallible<F>(
    ctx: &mut dyn NativeContext,
    items: &mut [Value],
    mut cmp: F,
) -> Result<(), MethodCallFailed>
where
    F: FnMut(&mut dyn NativeContext, &Value, &Value) -> Result<i32, MethodCallFailed>,
{
    let n = items.len();
    if n <= 1 {
        return Ok(());
    }
    // Scratch buffer holds a copy of the current left run during each merge.
    let mut scratch: Vec<Value> = Vec::with_capacity(n / 2 + 1);
    let mut width = 1usize;
    while width < n {
        let mut lo = 0usize;
        while lo < n {
            let mid = (lo + width).min(n);
            let hi = (lo + 2 * width).min(n);
            // Already sorted relative to each other if there is no right run.
            if mid < hi {
                merge_runs_fallible(ctx, items, lo, mid, hi, &mut scratch, &mut cmp)?;
            }
            lo += 2 * width;
        }
        width *= 2;
    }
    Ok(())
}

/// Merge the two adjacent sorted runs `[lo, mid)` and `[mid, hi)` of `items`
/// into a single sorted run `[lo, hi)`. Stable: ties keep the left element
/// first. `scratch` is reused across calls to avoid per-merge allocation.
pub(crate) fn merge_runs_fallible<F>(
    ctx: &mut dyn NativeContext,
    items: &mut [Value],
    lo: usize,
    mid: usize,
    hi: usize,
    scratch: &mut Vec<Value>,
    cmp: &mut F,
) -> Result<(), MethodCallFailed>
where
    F: FnMut(&mut dyn NativeContext, &Value, &Value) -> Result<i32, MethodCallFailed>,
{
    // Copy the left run aside; the right run stays in place in `items`.
    scratch.clear();
    scratch.extend_from_slice(&items[lo..mid]);

    let mut l = 0usize; // index into scratch (left run)
    let mut r = mid; // index into items (right run)
    let mut out = lo; // write position in items
    let left_len = scratch.len();

    while l < left_len && r < hi {
        // Stable: take the left element when left <= right.
        let c = cmp(ctx, &scratch[l], &items[r])?;
        if c <= 0 {
            items[out] = scratch[l];
            l += 1;
        } else {
            items[out] = items[r];
            r += 1;
        }
        out += 1;
    }
    // Drain whatever remains of the left run (right run is already in place).
    while l < left_len {
        items[out] = scratch[l];
        l += 1;
        out += 1;
    }
    Ok(())
}

/// `Arrays.fill(int[], int)`.
///
/// **This is the registration that OWNS the slot.** `native-builtins`'
/// `phases_early.rs` registers the same triple and loses; the registry dump
/// says so (`owns_slot`), and a fix applied only there is inert. Checked with
/// `--dump-native-registry` after exactly that mistake:
///
/// ```text
/// fill ([II)V   kind=bridge inv=1 owns=True by=native-collections/src/lib.rs:20914
/// ```
pub(crate) fn native_arrays_fill_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A null array is a NullPointerException, not a silent no-op. MEASURED
    // against HotSpot 25.0.3+9 in BOTH modes
    // (`probes/ArraysHashSetShadowSweep.java`): HotSpot NPE, this VM `no-throw`.
    // The JDK reaches it by dereferencing `a.length` on the first loop guard.
    let Some(Value::Object(Some(arr))) = args.first() else {
        return Err(RuntimeError::NullPointerException {
            message: Some("Arrays.fill: array is null".to_string()),
        }
        .into());
    };
    let arr = *arr;
    let val = args.get(1).copied().unwrap_or(Value::Int(0));
    let len = ctx.array_length(arr);
    for i in 0..len {
        ctx.set_array_element(arr, i, val);
    }
    Ok(None)
}

/// `Arrays.fill(Object[], Object)` — null-checked AND store-checked.
///
/// Same slot ownership note as [`native_arrays_fill_int`].
///
/// The store check is the one that matters. `Arrays.fill(Object[], Object)` is
/// an ordinary covariant array store per element, so filling a `String[]` with
/// an `Integer` is an `ArrayStoreException` — and `set_array_element` does not
/// check. MEASURED in BOTH modes, and the result is not merely a missing
/// exception:
///
/// ```text
/// String[] t = new String[2];
/// Arrays.fill((Object[]) t, Integer.valueOf(1));
///   HotSpot   ArrayStoreException
///   CratonVM  no-throw, and Arrays.toString(t) is [1, 1]
/// ```
///
/// A `String[]` whose contents are `Integer`s is an object that contradicts its
/// own type. Every later reader — a cast, a `checkcast` the JIT elides on the
/// strength of the array's type, a serializer — is entitled to assume it cannot
/// exist. This is the same hole the roadmap tracked as P3-A for the `aastore`
/// opcode, reached through a library method instead.
pub(crate) fn native_arrays_fill_object(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(arr))) = args.first() else {
        return Err(RuntimeError::NullPointerException {
            message: Some("Arrays.fill: array is null".to_string()),
        }
        .into());
    };
    let arr = *arr;
    let val = args.get(1).copied().unwrap_or(Value::Object(None));
    // A null value stores into any reference array, and a component type of
    // `java/lang/Object` accepts everything; `reject_unstorable` short-circuits
    // both.
    //
    // THE `ClassId` COMPARISON THIS REPLACES WAS WRONG IN BOTH DIRECTIONS, and
    // the fix arrived in two halves. The first half kept an exact
    // `class_id_of_object(arr) == class_id_of_object(v)` test and only
    // *consulted* the shared predicate when it failed, because on a REFERENCE
    // ARRAY the header's class id holds the COMPONENT class -- so a `char[][]`'s
    // component is the primitive array class `char[]`, which the two sides
    // cannot report the same way. MEASURED: `Arrays.fill((Object[]) new
    // char[3][], new char[]{'a'})` threw `ArrayStoreException:
    // java.lang.Object` here while HotSpot filled it fine
    // (`sun.nio.cs.HKSCS$Encoder.initc2b`'s `Arrays.fill(c2b, C2B_UNMAPPABLE)`
    // hit exactly this, breaking every real `Big5-HKSCS`/`MS950_HKSCS`/etc.
    // charset's static init).
    //
    // That half left two defects standing, both of them the SAME root cause
    // wearing different clothes:
    //
    //  * `unwrap_or(false)` made the fallback fail CLOSED. `None` is "this
    //    context models no hierarchy", which is the one answer that must never
    //    become a refusal -- the shared predicate's whole contract is that it
    //    is ADDITIVE and never manufactures a false `ArrayStoreException`.
    //  * the REFUSAL MESSAGE named `class_id_of_object(v)`, i.e. the component
    //    again, so an array-valued element was reported one dimension short.
    //    That is how the hibernate-reactive failure was identifiable at all:
    //    `ArrayStoreException: org.hibernate.sql.results.graph.Initializer`
    //    names an INTERFACE, and no instance can ever have an interface as its
    //    class.
    //
    // `reject_unstorable` is the one place a native's store rule lives: the
    // shared predicate, failing open on `None`, with `aastore`'s own external
    // name of the VALUE. `StoreRoute::Aastore` because the JDK's
    // `Arrays.fill(Object[], Object)` is a plain `a[i] = val` loop.
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
}

pub(crate) fn native_arrays_binary_search_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let key = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let len = ctx.array_length(arr);
    let mut low: usize = 0;
    let mut high = len;
    while low < high {
        let mid = low + (high - low) / 2;
        let mid_val = match ctx.get_array_element(arr, mid) {
            Value::Int(v) => v,
            _ => 0,
        };
        match mid_val.cmp(&key) {
            std::cmp::Ordering::Less => low = mid + 1,
            std::cmp::Ordering::Equal => return Ok(Some(Value::Int(mid as i32))),
            std::cmp::Ordering::Greater => high = mid,
        }
    }
    // Not found: return -(insertion point) - 1
    Ok(Some(Value::Int(-(low as i32) - 1)))
}

pub(crate) fn native_arrays_equals_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Object(Some(arr))) => *arr,
        Some(Value::Object(None)) => {
            return match args.get(1) {
                Some(Value::Object(None)) => Ok(Some(Value::Int(1))),
                _ => Ok(Some(Value::Int(0))),
            };
        }
        _ => return Ok(Some(Value::Int(0))),
    };
    let b = match args.get(1) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => return Ok(Some(Value::Int(0))),
    };
    let len_a = ctx.array_length(a);
    let len_b = ctx.array_length(b);
    if len_a != len_b {
        return Ok(Some(Value::Int(0)));
    }
    for i in 0..len_a {
        if ctx.get_array_element(a, i) != ctx.get_array_element(b, i) {
            return Ok(Some(Value::Int(0)));
        }
    }
    Ok(Some(Value::Int(1)))
}

